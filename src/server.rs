//! rmate server for Zed.
//! TCP server.

use tracing::{debug, error, info};

use std::{
    error::Error,
    path::{Path, PathBuf},
};

use tokio::{
    fs::File,
    io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::{sleep, Duration},
};

use notify::{
    event::{DataChange, EventKind, ModifyKind},
    Event, RecursiveMode, Watcher,
};

use async_process::{Child, Command, Stdio};

use tempfile::{tempdir, TempDir};

use crate::protocol::*;

#[derive(Debug)]
struct TmpFile {
    remote_file: RmateFile,
    file_path: PathBuf,
    #[allow(unused)]
    tmp_dir: TempDir,
}

/// A tmp file created from an rmate request.
impl TmpFile {
    /// Copies an rmate file body to a tmp file.
    async fn with_remotefile<R>(
        remote_file: RmateFile,
        body_reader: &mut R,
    ) -> Result<TmpFile, std::io::Error>
    where
        R: AsyncBufRead + Unpin,
    {
        // Create a directory inside of `std::env::temp_dir()`
        let tmp_dir = tempdir()?;

        // Prepare tmp file
        let safe_file_name = remote_file.safe_display_name();
        let file_path = tmp_dir.path().canonicalize()?.join(safe_file_name);
        let mut tmp_file = File::create(&file_path).await?;

        // Copy data to tmp file
        debug!("Writing to {file_path:#?}");
        let n = tokio::io::copy(body_reader, &mut tmp_file).await?;
        info!("Received file {:?} of {n} bytes", remote_file.display_name);

        // Flush file to closed immediately when it goes out of scope
        tmp_file.flush().await?;

        Ok(TmpFile {
            remote_file,
            file_path,
            tmp_dir,
        })
    }

    /// Watch files and send change notify events to a channel.
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
                debug!("file event: {:?}", event);
                futures::executor::block_on(async {
                    // TODO: Cancel safety might be an issue with a tokio::select! statement
                    if tx.send(event).await.is_err() {
                        debug!("receiver dropped");
                    }
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
        let p = &self.file_path;
        let p = p.to_string_lossy().to_string();
        if self.remote_file.selection.is_empty() {
            p
        } else {
            // Use `path:line:row` syntax to open a file at a specific location
            format!("{p}:{}", self.remote_file.selection)
        }
    }

    /// Spawn Zed by calling the CLI with some options and a list of file paths.
    fn spawn_zed(zed_bin: &Path, files: &[TmpFile]) -> Result<Child, std::io::Error> {
        let arg_new = files.iter().any(|r| r.remote_file.new);

        let paths = files.iter().map(|r| r.tmp_file_path_with_selection());

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

    /// Send a local tmp file to the rmate stream.
    async fn send_file<S>(&self, conn: &mut RmateConnection<S>) -> Result<(), std::io::Error>
    where
        S: AsyncBufRead + AsyncWrite + Unpin,
    {
        // Open tmp file
        debug!("Opening tmp file to send");
        let mut tmp_file = File::open(&self.file_path).await?;
        let file_size = tmp_file.metadata().await?.len();

        // Send tmp file
        info!("Sending file of {file_size} bytes to remote");
        conn.send(&self.remote_file, &mut tmp_file, file_size).await
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
async fn handle_connection(stream: TcpStream, zed_bin: PathBuf) -> Result<(), std::io::Error> {
    // Send server identification
    let mut conn = RmateConnection::new(BufReader::new(stream)).await?;

    let mut files = vec![];
    // Read open requests
    while let Some((req, mut body_reader)) = conn.recv().await? {
        debug!("Request: {req:#?}");
        // Copy remote file body to a tmp file
        let tmp = TmpFile::with_remotefile(req, &mut body_reader).await?;
        files.push(tmp);
    }

    // Sleep a short time for the notify to settle
    sleep(Duration::from_millis(200)).await;

    // Watch the tmp files for changes and write each to the connection
    let file_paths = files.iter().map(|r| r.file_path.as_path());
    let (_watcher, mut rx) = TmpFile::watch_files(file_paths)
        .map_err(|_| std::io::Error::other("File watcher error"))?;

    // Open the tmp files in Zed
    info!("Opening Zed");
    let mut zed = TmpFile::spawn_zed(&zed_bin, &files)?;

    loop {
        tokio::select! {
            event = rx.recv() => {
                // We are only interested in content change and file remove events
                match event {
                    Some(Event {kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)), paths, .. }) => {
                        for p in paths {
                            if let Some(file) = files.iter().find(|f| { f.file_path == *p }) {
                                info!("File {:?} changed", file.remote_file.display_name);
                                file.send_file(&mut conn).await?;
                            }
                        }
                    }
                    Some(Event {kind: EventKind::Remove(_), paths, .. }) => {
                        for p in paths {
                            if let Some(file) = files.iter().find(|f| { f.file_path == *p }) {
                                info!("File {:?} removed", file.remote_file.display_name);
                                // TODO: drop the file
                            }
                        }
                    }
                    _ => {
                        // ignore
                    }
                }
            }
            // TODO: We might need re-check files here if close is signaled before a save is detected
            status = zed.status() => {
                debug!("Zed closed ({status:#?})");
                break;
            }
        };
    }

    // On Zed close or remote connection close remove the tmp file
    //let _ = zed.output().await?;
    info!("Zed closed, closing connection");
    conn.close().await
}
