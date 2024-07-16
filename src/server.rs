//! rmate server for Zed.
//! TCP server.

use tracing::{debug, error, info};

use std::{
    error::Error,
    path::{Path, PathBuf},
    str::FromStr,
};

use tokio::{
    fs::File,
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::{sleep, Duration},
};

use notify::{Event, RecursiveMode, Watcher};

use async_process::{Child, Command, Stdio};

use tempfile::{tempdir, TempDir};

static SERVER_ID: &str = "zed-rmate-server 0\n";

#[derive(Default, Debug)]
struct RemoteFile {
    // client values
    display_name: String,
    real_path: String,
    data_on_save: bool,
    re_activate: bool,
    token: String,
    new: bool,
    selection: String,
    file_type: String,
    data: usize,
    // server generated fields
    tmp_file_path: PathBuf,
    tmp_dir: Option<TempDir>,
}

impl RemoteFile {
    /// Send server identification string.
    async fn send_server_id(stream: &mut TcpStream) -> Result<(), std::io::Error> {
        stream.write_all(SERVER_ID.as_bytes()).await
    }

    /// Read an rmate command from a TCP stream.
    ///
    /// Accepted commands are "open" and ".".
    /// Empty lines will be skipped silently.
    ///
    /// After reading an "open" command we next need to read headers then body data.
    async fn read_cmd<R>(reader: &mut R) -> Result<bool, std::io::Error>
    where
        R: AsyncBufRead + Unpin,
    {
        // Read an rmate command
        let mut buf = String::new();
        debug!("Reading client command");

        loop {
            buf.clear();
            let _n = reader.read_line(&mut buf).await?;
            debug!("Parsing {buf:#?} line");

            match buf.trim_end() {
                "" => {
                    // skip
                }
                "open" => {
                    debug!("Got \"open\" command");
                    return Ok(true);
                }
                "." => {
                    debug!("Got end of commands");
                    return Ok(false);
                }
                _ => return Err(std::io::Error::other("Protocol error")),
            }
        }
    }

    /// Reads rmate open command headers from a TCP stream.
    ///
    /// After reading the headers all remaining data in the reader is file content.
    async fn with_reader<R>(reader: &mut R) -> Result<Self, std::io::Error>
    where
        R: AsyncBufRead + Unpin,
    {
        // Read rmate open command headers
        let mut buf = String::new();
        let mut req = Self::default();
        loop {
            // Read key-value lines
            let _ = reader.read_line(&mut buf).await?;
            debug!("Parsing {buf:#?} line");
            let kv = buf.split_once(": ");
            if kv.is_none() {
                return Err(std::io::Error::other("Protocol error"));
            }
            let (key, value) = kv.unwrap();
            let value = value.trim_end();
            debug!("Got {key:#?}: {value:#?}");
            // TODO: we really should check and sanitze these inputs!
            match key {
                "display-name" => req.display_name = value.to_string(),
                "real-path" => req.real_path = value.to_string(),
                "data-on-save" => req.data_on_save = parse_bool(value).unwrap(),
                "re-activate" => req.re_activate = parse_bool(value).unwrap(),
                "token" => req.token = value.to_string(),
                "new" => req.new = parse_bool(value).unwrap(),
                "selection" => req.selection = value.to_string(),
                "file-type" => req.file_type = value.to_string(),
                "data" => {
                    req.data = value.parse().unwrap(); // int
                    return Ok(req); // "data:" is the last key
                }
                _ => return Err(std::io::Error::other("Protocol error")),
            }
            buf.clear();
        }
        // All remaining data in the reader is file content
    }

    /// Copies an rmate file body to a tmp file.
    async fn copy_body<R>(&mut self, reader: &mut R) -> Result<(), std::io::Error>
    where
        R: AsyncBufRead + Unpin,
    {
        // Create a directory inside of `std::env::temp_dir()`
        let tmp_dir = tempdir()?;

        // Prepare tmp file
        let safe_file_name = self.safe_display_name();
        let file_path = tmp_dir.path().canonicalize()?.join(safe_file_name);
        let mut tmp_file = File::create(&file_path).await?;

        // Copy data to tmp file
        let bytes_total = self.data;
        debug!("Copying file of {bytes_total} bytes to {file_path:#?}");
        copy_stream_to_file(reader, bytes_total, &mut tmp_file).await?;

        // Flush file to closed immediately when it goes out of scope
        tmp_file.flush().await?;

        self.tmp_file_path = file_path;
        self.tmp_dir = Some(tmp_dir);

        Ok(())
    }

    /// Return a sanitized `display_name``.
    ///
    /// Replaces colons with a tilde as workaround for Zed.
    /// Removes any paths contained in the display name.
    fn safe_display_name(&self) -> String {
        let mut s = self.display_name.clone();
        if s.is_empty() {
            s.clone_from(&self.real_path);
        }
        if s.is_empty() {
            s = "rmate-file.txt".to_string();
        }

        // Currently Zed has problems with colons in filenames, replace with a tilde
        let s = s.replace(':', "~");

        // Remove any paths contained in the display name
        let s_unsafe = PathBuf::from(s);
        s_unsafe.file_name().unwrap().to_string_lossy().to_string()
    }

    /// Send a local tmp file to the rmate stream using the rmate "save" command.
    async fn send_file(&self, stream: &mut TcpStream) -> Result<(), std::io::Error> {
        debug!("Sending file to remote");
        // Open tmp file
        let mut tmp_file = File::open(&self.tmp_file_path).await?;
        let file_size = tmp_file.metadata().await?.len();

        // Write rmate "save" header
        debug!("Sending headers");
        let token = &self.token;
        let header = format!("save\ntoken: {token}\ndata: {file_size}\n");
        stream.write_all(header.as_bytes()).await?;

        debug!("Sending {file_size} bytes");
        tokio::io::copy(&mut tmp_file, stream).await?;

        debug!("Sending done");
        // Send a closing newline
        stream.write_all("\n".as_bytes()).await?;

        Ok(())
    }

    /// Send server close command.
    async fn send_close(stream: &mut TcpStream) -> Result<(), std::io::Error> {
        stream.write_all("close\n".as_bytes()).await
    }

    /// Watch a file and send change notify events to a channel.
    fn watch_files<'a, T>(
        file_paths: T,
    ) -> Result<(impl Watcher, mpsc::Receiver<Event>), notify::Error>
    where
        T: Iterator<Item = &'a Path>,
    {
        let (tx, rx) = mpsc::channel(32);

        // Automatically select the best implementation for a platform.
        let mut watcher = notify::recommended_watcher(move |res| match res {
            Ok(event) => {
                info!("file event: {:?}", event);
                futures::executor::block_on(async {
                    tx.send(event).await.unwrap();
                })
            }
            Err(e) => {
                error!("file watch error: {:?}", e);
            }
        })?;

        // Add the file paths to be watched
        for p in file_paths {
            watcher.watch(p, RecursiveMode::NonRecursive)?
        }

        Ok((watcher, rx))
    }

    /// Return the tmp file path with optional selection string added.
    ///
    /// Uses `path:line:row` syntax to open a file at a specific location.
    fn tmp_file_path_with_selection(&self) -> String {
        let p = &self.tmp_file_path;
        let p = p.to_string_lossy().to_string();
        if self.selection.is_empty() {
            p
        } else {
            // Use `path:line:row` syntax to open a file at a specific location
            format!("{p}:{}", self.selection)
        }
    }

    /// Spawn Zed by calling the CLI with some options and a list of file paths.
    fn spawn_zed(zed_bin: &Path, reqs: &[RemoteFile]) -> Result<Child, std::io::Error> {
        let arg_new = reqs.iter().any(|r| r.new);

        let paths = reqs.iter().map(|r| r.tmp_file_path_with_selection());

        let mut args = vec!["--wait"];
        if arg_new {
            args.push("--new");
        }

        Command::new(zed_bin)
            .args(args)
            .args(paths)
            .stdout(Stdio::piped())
            .spawn()
    }
}

/// Binds a TCP listener and handles each incoming connection with `handle_connection()`.
pub(crate) async fn serve(
    bind: String,
    zed_bin: PathBuf,
    once: bool,
) -> Result<(), Box<dyn Error>> {
    // Bind a TCP listener
    let listener = TcpListener::bind(&bind).await?;
    info!("zed-rmate-server listening on {}", bind);

    // Listening to new TCP Connections
    loop {
        // Asynchronously wait for an inbound socket.
        let (stream, addr) = listener.accept().await?;
        info!("Got rmate connection from {addr:#?}");

        if once {
            handle_connection(stream, zed_bin.clone()).await?;
            break Ok(()); // accept a single connection then terminate
        } else {
            tokio::spawn(handle_connection(stream, zed_bin.clone()));
        }
    }
}

/// Handles a new rmate TCP connection.
///
/// - Output a server identification
/// - Read key-value pairs
/// - Copy data to a tmp file
/// - Open the tmp file in Zed
/// - Watch the tmp file for changes and send the file to the connection
/// - On Zed close or remote connection close remove the tmp file
async fn handle_connection(mut stream: TcpStream, zed_bin: PathBuf) -> Result<(), std::io::Error> {
    // Send server identification
    debug!("Sending server identification");
    RemoteFile::send_server_id(&mut stream).await?;

    let mut reader = BufReader::new(&mut stream);

    let mut reqs = vec![];
    while RemoteFile::read_cmd(&mut reader).await? {
        debug!("Reading client request header");

        // Read request headers
        let mut req = RemoteFile::with_reader(&mut reader).await?;
        // All remaining data in the reader is file content
        req.copy_body(&mut reader).await?;

        debug!("Request: {req:#?}");
        reqs.push(req);
    }

    // Sleep a short time for the notify to settle
    sleep(Duration::from_millis(200)).await;

    // Watch the tmp files for changes and write each to the connection
    let file_paths = reqs.iter().map(|r| r.tmp_file_path.as_path());
    let (_watcher, mut rx) = RemoteFile::watch_files(file_paths).unwrap();

    // Open the tmp files in Zed
    info!("Opening Zed");
    let mut zed = RemoteFile::spawn_zed(&zed_bin, &reqs).unwrap();

    loop {
        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    debug!("File event ({event:#?})");
                    if event.kind.is_modify() {
                        let p = event.paths.first().unwrap();
                        let req = reqs.iter().find(|r| { r.tmp_file_path == *p }).unwrap();
                        req.send_file(&mut stream).await?;
                    }
                }
            }
            status = zed.status() => {
                debug!("Zed closed ({status:#?})");
                break;
            }
        };
    }

    // On Zed close or remote connection close remove the tmp file
    //let _ = zed.output().await?;
    info!("Zed closed, closing connection");
    RemoteFile::send_close(&mut stream).await
}

// helper

/// Parse a string to bool, recognizes `true`, `t`, `yes`, `y`, `1` and `false`, `f`, `no`, `n`, `0`.
fn parse_bool(s: &str) -> Result<bool, std::str::ParseBoolError> {
    if s.eq_ignore_ascii_case("true")
        || s.eq_ignore_ascii_case("t")
        || s.eq_ignore_ascii_case("yes")
        || s.eq_ignore_ascii_case("y")
        || s == "1"
    {
        Ok(true)
    } else if s.eq_ignore_ascii_case("false")
        || s.eq_ignore_ascii_case("f")
        || s.eq_ignore_ascii_case("no")
        || s.eq_ignore_ascii_case("n")
        || s == "0"
    {
        Ok(false)
    } else {
        // The only accepted values are "true" and "false". Any other input will return an error.
        bool::from_str(s)
    }
}

/// Copy given number of TCP stream data bytes to tmp file.
async fn copy_stream_to_file<R, W>(
    reader: &mut R,
    len: usize,
    file: &mut W,
) -> Result<usize, std::io::Error>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 0x10_000];
    let mut bytes_copied = 0;
    let mut bytes_left = (len - bytes_copied).min(buf.len());
    while let Ok(n) = reader.read(&mut buf[..bytes_left]).await {
        debug!("Writing {:?}", &buf[..n]);
        file.write_all(&buf[..n]).await?;
        bytes_copied += n;
        if bytes_copied >= len {
            break;
        }
        bytes_left = (len - bytes_copied).min(buf.len());
    }
    Ok(bytes_copied)
}
