//! rmate server for Zed.
//! rmate protocol handling.
//
//! S.a. https://github.com/textmate/textmate/blob/master/Applications/TextMate/src/RMateServer.mm

use tracing::{debug, warn};

use std::{io::Error, path::PathBuf, str::FromStr};

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Take,
};

static SERVER_ID: &str = "zed-rmate-server 0\n";

pub struct RmateConnection<S> {
    stream: S,
}

impl<S> RmateConnection<S>
where
    S: AsyncBufRead + AsyncWrite + Unpin,
{
    /// Open an rmate connection by sending our server identification string to the output stream.
    ///
    /// # Arguments
    ///
    /// * `stream` - Input reader and output writer, likely a TcpStream
    pub async fn new(mut stream: S) -> Result<Self, Error> {
        debug!("Sending server identification");
        stream.write_all(SERVER_ID.as_bytes()).await?;

        Ok(Self { stream })
    }

    /// Read an rmate command from the input stream.
    ///
    /// Accepted commands are "open" and ".".
    /// Empty lines will be skipped silently.
    ///
    /// After receiving a RemoteFile you need to fully read the body before continuing to recv() again.
    ///
    /// After receiving a None result the read part of the protocol is finished
    /// and you can't expect to successfully recv() anymore.
    pub async fn recv(&mut self) -> Result<Option<(RmateFile, Take<&mut S>)>, Error> {
        debug!("Reading client command");

        let mut buf = String::new();
        loop {
            // Read an rmate command
            let _n = self.stream.read_line(&mut buf).await?;
            debug!("Parsing {buf:#?} line");

            // Match command or skip empty lines and retry
            match buf.trim_end() {
                "" => {
                    // skip
                }
                "open" => {
                    debug!("Got \"open\" command");
                    return Ok(Some(RmateFile::recv_file(&mut self.stream).await?));
                }
                "." => {
                    debug!("Got end of commands");
                    return Ok(None);
                }
                _ => return Err(Error::other("Protocol error")),
            }
            buf.clear();
        }
    }

    pub async fn send<R>(&mut self, req: &RmateFile, reader: &mut R, size: u64) -> Result<(), Error>
    where
        R: AsyncRead + Unpin,
    {
        req.send_file(reader, size, &mut self.stream).await
    }

    /// Send the server close command to the output stream.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.stream.write_all("close\n".as_bytes()).await
    }
}

#[derive(Default, Debug)]
pub struct RmateFile {
    // path: String,
    // uuid: String,
    pub display_name: String,
    pub real_path: String,
    pub data_on_save: bool,
    pub re_activate: bool,
    pub token: String,
    pub new: bool,
    pub selection: String, // <line>[:<column>][-<line>[:<column>]]
    pub file_type: String,
    // project_uuid: String,
    // add_to_recents: bool,
    // authorization: String,
    // wait: bool,
    // data_on_close: bool,
    pub data: u64,
}

/// A remote file as described and received from rmate.
impl RmateFile {
    /// Reads rmate "open" command headers from an input stream.
    ///
    /// After reading the headers the remaining data of `data` bytes length from the
    /// input stream is returned as wrapped reader to read the rmate file body contents.
    ///
    /// # Arguments
    ///
    /// * `reader` - Input reader, likely a TcpStream
    async fn recv_file<R>(reader: &mut R) -> Result<(Self, Take<&mut R>), Error>
    where
        R: AsyncBufRead + Unpin,
    {
        // Read rmate open command headers
        let mut buf = String::new();
        let mut req = Self::default();
        debug!("Reading client open headers");
        loop {
            // Read key-value lines
            let _ = reader.read_line(&mut buf).await?;
            debug!("Parsing {buf:#?} line");
            let (key, value) = buf
                .split_once(": ")
                .ok_or(Error::other("Protocol header error"))?;
            let value = value.trim_end();
            debug!("Got {key:#?}: {value:#?}");
            // TODO: we really should check and sanitze these inputs!
            match key {
                "display-name" => req.display_name = value.to_string(),
                "real-path" => req.real_path = value.to_string(),
                "data-on-save" => {
                    req.data_on_save =
                        parse_bool(value).map_err(|_| Error::other("Protocol value error"))?
                }
                "re-activate" => {
                    req.re_activate =
                        parse_bool(value).map_err(|_| Error::other("Protocol value error"))?
                }
                "token" => req.token = value.to_string(),
                "new" => {
                    req.new = parse_bool(value).map_err(|_| Error::other("Protocol value error"))?
                }
                "selection" => req.selection = value.to_string(),
                "file-type" => req.file_type = value.to_string(),
                "data" => {
                    req.data = value
                        .parse()
                        .map_err(|_| Error::other("Protocol value error"))?;
                    // "data:" is the last key
                    // Remaining bytes in the reader of `data` length are file content
                    debug!("Receiving file of {} bytes", req.data);
                    let body_reader = reader.take(req.data);

                    return Ok((req, body_reader));
                }
                _ => {
                    warn!("Ignoring unknown {key:#?}: {value:#?}");
                }
            }
            buf.clear();
        }
    }

    /// Return a sanitized `display_name`.
    ///
    /// Replaces colons with a tilde as workaround for Zed.
    /// Removes any paths contained in the display name.
    pub fn safe_display_name(&self) -> String {
        const DEFAULT_NAME: &str = "rmate-file.txt";

        let mut s = self.display_name.clone();
        if s.is_empty() {
            s.clone_from(&self.real_path);
        }
        if s.is_empty() {
            s = DEFAULT_NAME.to_string();
        }

        // Currently Zed has problems with colons in filenames, replace with a tilde
        let s = s.replace(':', "~");

        // Remove any paths contained in the display name
        let s_unsafe = PathBuf::from(s);
        if let Some(s) = s_unsafe.file_name() {
            // Since we started with a String to_string_lossy() is safe.
            s.to_string_lossy().to_string()
        } else {
            DEFAULT_NAME.to_string()
        }
    }

    /// Send local file content to the rmate stream using the rmate "save" command.
    ///
    /// # Arguments
    ///
    /// * `reader` - Input reader, likely a File
    /// * `size` - A length indication, must match the reader
    /// * `stream` - Output writer, likely a TcpStream
    async fn send_file<R, W>(&self, reader: &mut R, size: u64, stream: &mut W) -> Result<(), Error>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        debug!("Sending file to remote");

        // Write rmate "save" header
        debug!("Sending headers");
        let token = &self.token;
        let header = format!("save\ntoken: {token}\ndata: {size}\n");
        stream.write_all(header.as_bytes()).await?;

        debug!("Sending {size} bytes");
        tokio::io::copy(reader, stream).await?;

        debug!("Sending done");
        // Send a closing newline
        stream.write_all("\n".as_bytes()).await?;

        // Flush the stream to ensure data is sent
        stream.flush().await
    }
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
