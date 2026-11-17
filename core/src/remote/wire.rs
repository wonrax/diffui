//! What goes down the stream between the app and `diffui-serve`.
//!
//! The server writes a magic line first, so whatever a login prints before it
//! (a banner, an `echo` in a shell profile) is skipped instead of being read as
//! a frame. After it both ends send frames: a 4-byte big-endian length and
//! that many bytes of JSON. Each end's first frame is its [`Hello`]; then the
//! app sends [`Up`] and the server answers with [`Down`].
//!
//! One stream serves a whole host. Requests about the host itself (listing a
//! folder) carry a request id; everything about one open repository carries
//! the [`Channel`] the app opened it on, so several tabs share the stream.

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite};

use crate::repo::{Capabilities, Command, Payload};
use crate::repository::Vcs;

/// The version of this module's messages. Both ends must speak the same one.
///
/// Raise it when a build with the change can't talk to a build without it: a
/// variant or field removed or renamed, a meaning changed. A field added with
/// `#[serde(default)]` or a variant only a new app sends to a server that
/// answers `Unsupported` doesn't need it. `the_wire_is_the_golden_file` fails
/// when the shapes move, which is the moment to decide.
pub const PROTOCOL: u32 = 1;

/// The line the server writes before its first frame.
pub const MAGIC: &str = "diffui-serve";

/// How much a login may print before the magic line.
const MAX_PREAMBLE: usize = 64 * 1024;

/// A frame longer than this is a corrupt stream, not a message.
const MAX_FRAME: u32 = 1 << 30;

/// One open repository on the stream. The app picks the number.
pub type Channel = u32;

/// One question about the host. The app picks the number.
pub type Request = u64;

/// Each end's first frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The build's version, for the message when the protocols differ.
    pub version: String,
    /// The commit the build was made from, when it knows.
    #[serde(default)]
    pub rev: Option<String>,
    /// The server's machine. The app sends none.
    #[serde(default)]
    pub host: Option<HostInfo>,
}

impl Hello {
    /// This build's.
    pub fn ours(host: Option<HostInfo>) -> Self {
        Self {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            rev: super::build_rev().map(str::to_owned),
            host,
        }
    }

    /// `0.1.0 (protocol 1, 1a2b3c4d)`.
    pub fn describe(&self) -> String {
        match &self.rev {
            Some(rev) => format!(
                "{} (protocol {}, {})",
                self.version,
                self.protocol,
                &rev[..rev.len().min(12)]
            ),
            None => format!("{} (protocol {})", self.version, self.protocol),
        }
    }
}

/// What the server says about the machine it runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// `std::env::consts::OS` there: "linux", "macos", "windows".
    pub os: String,
    /// The home folder there, which a path starting `~/` is under.
    #[serde(default)]
    pub home: Option<String>,
}

/// App to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Up {
    /// The folders in `path`. Answered by [`Down::DirListed`].
    ListDir { request: Request, path: String },
    /// What opening `path` would open. Answered by [`Down::Probed`].
    Probe { request: Request, path: String },
    /// Open the repository at or above `path` on `channel`. Answered by
    /// [`Down::Opened`] or [`Down::Refused`].
    Open { channel: Channel, path: String },
    /// A command for the repository on `channel`, narrowed to `scope` (a path
    /// under its root, as [`Opened::scope`]).
    Run {
        channel: Channel,
        scope: String,
        command: Box<Command>,
    },
    /// The app is done with `channel`: its repository's actor stops.
    Close { channel: Channel },
}

/// Server to app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Down {
    /// The folders, or why the folder couldn't be read.
    DirListed {
        request: Request,
        entries: Vec<DirEntry>,
        #[serde(default)]
        error: Option<String>,
    },
    Probed {
        request: Request,
        probe: Probed,
    },
    Opened {
        channel: Channel,
        repository: Opened,
    },
    /// Nothing to open at the path: why, in the words opening it here would
    /// have used.
    Refused {
        channel: Channel,
        message: String,
    },
    Event {
        channel: Channel,
        payload: Box<Payload>,
    },
}

/// One folder in a folder on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// The folder is a repository's root.
    pub vcs: Option<Vcs>,
}

/// What a path on the host is, as far as opening it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probed {
    /// The folder the path is in (the path itself when it ends in `/`)
    /// exists.
    pub dir_exists: bool,
    pub repository: Option<FoundRepository>,
}

/// The repository at or above a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundRepository {
    pub root: String,
    pub vcs: Vcs,
    /// The path names the root itself, not something inside it.
    pub at_root: bool,
    /// What the path names under the root; empty for the root.
    #[serde(default)]
    pub scope: String,
}

/// A repository the server opened, in place of the in-process
/// [`Payload::Ready`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    /// The workspace root, as a path on the host.
    pub root: String,
    pub vcs: Vcs,
    /// What the opened path named under the root; empty for the root.
    pub scope: String,
    pub capabilities: Capabilities,
    /// jj's `revsets.log` there, which the app can't read from here.
    #[serde(default)]
    pub default_revset: Option<String>,
    /// The `(name, url)` git remotes, for the forges.
    #[serde(default)]
    pub remotes: Vec<(String, String)>,
}

/// Write one frame and leave it in `writer`'s buffer.
pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| io::Error::other("message too large for a frame"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&body).await
}

/// Read one frame. `None` when the stream ended between frames.
pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len}-byte frame: the stream isn't diffui-serve's"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write the magic line.
pub async fn write_magic<W: AsyncWrite + Unpin>(writer: &mut W) -> io::Result<()> {
    writer.write_all(format!("{MAGIC}\n").as_bytes()).await
}

/// Read up to the magic line, dropping what comes before it. `false` when
/// the stream ended, or ran past [`MAX_PREAMBLE`], without one.
pub async fn read_magic<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<bool> {
    let mut seen = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Ok(false);
        }
        // A profile that prints without a newline leaves its text in front.
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.ends_with(MAGIC.as_bytes()) {
            return Ok(true);
        }
        seen += read;
        if seen > MAX_PREAMBLE {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_frame_reads_back_and_the_end_is_none() {
        let mut stream = Vec::new();
        let hello = Hello::ours(None);
        write_frame(&mut stream, &hello).await.unwrap();
        let mut reader = stream.as_slice();
        assert_eq!(
            read_frame::<_, Hello>(&mut reader).await.unwrap(),
            Some(hello)
        );
        assert_eq!(read_frame::<_, Hello>(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn what_a_login_prints_before_the_magic_is_dropped() {
        let stream = b"Welcome to box\r\nlast login: never\nprompt> diffui-serve\nrest";
        let mut reader = stream.as_slice();
        assert!(read_magic(&mut reader).await.unwrap());
        assert_eq!(reader, b"rest");

        let mut reader = b"sh: diffui-serve: command not found\n".as_slice();
        assert!(!read_magic(&mut reader).await.unwrap());
    }

    #[tokio::test]
    async fn a_length_no_frame_has_is_a_corrupt_stream() {
        let mut reader = [0xff, 0xff, 0xff, 0xff, 0].as_slice();
        let error = read_frame::<_, Hello>(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
