//! Repositories on another machine.
//!
//! jj-lib wants a local disk, and a filesystem mounted over a network would
//! turn every graph walk and snapshot into a stream of round trips. So the
//! repository's actor runs where the repository is, inside `diffui-serve`,
//! and the app drives it over a byte stream with the protocol it uses in
//! process ([`crate::repo::Command`] / [`crate::repo::Payload`]), framed by
//! [`wire`].
//!
//! The stream is the stdio of a program the app starts: `ssh`, `wsl.exe`,
//! `docker exec`, anything that ends up running the server on the other
//! side. What to start is the frontend's business (its hosts and transports),
//! which it hands over as a [`Connector`]. diffui never installs anything on
//! the other machine: the server is whatever that command finds there.
//!
//! One stream serves a host: [`Hosts`] keeps one [`HostLink`] per host that
//! every tab on it, and the open prompt browsing it, share.

mod dirs;
pub(crate) mod link;
pub mod serve;
pub mod wire;

use std::fmt;

use futures::future::BoxFuture;

pub use link::{HostLink, Hosts, LinkState, Transcript};
pub use wire::{DirEntry, FoundRepository, Hello, HostInfo, Opened, PROTOCOL, Probed};

/// The commit this build was made from, when the build said (`DIFFUI_REV`).
/// A plain `cargo build` doesn't know.
pub fn build_rev() -> Option<&'static str> {
    option_env!("DIFFUI_REV").filter(|rev| !rev.is_empty())
}

/// A machine repositories are opened on, by the name the frontend knows it
/// under.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct HostId(pub String);

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A place on a host: what a remote tab is opened from, and how it is
/// written wherever a path would be (`diffui box:~/src/repo`, the session
/// file, the recent list).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteTarget {
    pub host: HostId,
    /// A path on the host, absolute or under its home (`~/…`).
    pub path: String,
}

impl RemoteTarget {
    /// Read `host:/path` or `host:~/path`.
    ///
    /// A path on the host starts with `/` or `~`, which is what tells a
    /// target from a file name with a colon in it. A host name is at least
    /// two characters, so a Windows drive (`C:/src`) is never one, and holds
    /// no path separator.
    pub fn parse(text: &str) -> Option<Self> {
        let colon = text.find(":/").or_else(|| text.find(":~"))?;
        let (host, path) = (&text[..colon], &text[colon + 1..]);
        let named = host.chars().count() >= 2 && !host.contains(['/', '\\']) && host.trim() == host;
        named.then(|| Self {
            host: HostId(host.to_owned()),
            path: path.to_owned(),
        })
    }
}

impl fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.path)
    }
}

/// The stream to a host's server, or how to get it.
pub enum Launch {
    /// A program to start, whose stdin and stdout are the stream: `ssh`,
    /// `wsl.exe`, `docker exec`. What a transport plugin answers.
    Program {
        /// The whole command line, the server's included.
        argv: Vec<String>,
        /// Environment for the program started here (not for the server).
        env: Vec<(String, String)>,
    },
    /// A stream the frontend opened itself: a socket, or the two ends of a
    /// server running in this process.
    Stream {
        reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        writer: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
        /// What it is, for messages.
        name: String,
    },
}

impl Launch {
    /// The command line as one would type it, for messages.
    pub fn display(&self) -> String {
        match self {
            Self::Stream { name, .. } => name.clone(),
            Self::Program { argv, .. } => argv
                .iter()
                .map(|word| {
                    if word.is_empty() || word.contains([' ', '\t', '"', '\'']) {
                        format!("{word:?}")
                    } else {
                        word.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// How a launched program ended without a server having answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// `None` when a signal ended it, or it was still running when the app
    /// gave up.
    pub code: Option<i32>,
    /// The end of what it wrote to stderr.
    pub stderr: String,
}

/// The frontend's knowledge of hosts: how to reach one, and what a failure
/// to means.
pub trait Connector: Send + Sync + 'static {
    /// The command that reaches `host`'s server, or why there is none (no
    /// such host, its transport is off).
    fn launch(&self, host: &HostId) -> BoxFuture<'static, Result<Launch, String>>;

    /// `exit` in the transport's words ("box has no diffui-serve on its
    /// PATH"), when it has any.
    fn explain(&self, host: &HostId, exit: &Exit) -> BoxFuture<'static, Option<String>>;
}

/// Why a host, or a repository on it, can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The frontend has no command for the host.
    Launch(String),
    /// The command couldn't be started on this machine.
    Spawn { command: String, message: String },
    /// The command ran and ended, or went quiet, before a server said hello.
    NoServer {
        command: String,
        exit: Exit,
        /// [`Connector::explain`]'s reading of `exit`.
        explained: Option<String>,
        timed_out: bool,
    },
    /// A server answered that speaks another protocol version.
    Mismatch {
        ours: Box<Hello>,
        theirs: Box<Hello>,
    },
    /// The stream was up and ended.
    Lost { message: String },
    /// The host is fine; it has no repository at the path.
    Refused(String),
}

impl ConnectError {
    /// Whether trying again could go differently without the user changing
    /// anything on either machine.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Lost { .. })
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch(message) | Self::Refused(message) => f.write_str(message),
            Self::Spawn { command, message } => write!(f, "Couldn't run `{command}`: {message}"),
            Self::NoServer {
                explained: Some(explained),
                ..
            } => f.write_str(explained),
            Self::NoServer {
                command,
                exit,
                timed_out,
                ..
            } => {
                if *timed_out {
                    write!(f, "`{command}` started no diffui-serve in time")?;
                } else {
                    match exit.code {
                        Some(code) => write!(
                            f,
                            "`{command}` exited with {code} before diffui-serve answered"
                        )?,
                        None => write!(f, "`{command}` ended before diffui-serve answered")?,
                    }
                }
                match exit
                    .stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                {
                    Some(last) => write!(f, ": {}", last.trim()),
                    None => Ok(()),
                }
            }
            Self::Mismatch { ours, theirs } => write!(
                f,
                "diffui {} can't talk to the diffui-serve {} over there",
                ours.describe(),
                theirs.describe()
            ),
            Self::Lost { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Expand `~` and `~/…` against `home`. Anything else is the path as given.
pub(crate) fn expand_home(path: &str, home: Option<&str>) -> String {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            format!("{}{rest}", home.trim_end_matches('/'))
        }
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tilde_is_the_home_folder() {
        assert_eq!(expand_home("~", Some("/home/me")), "/home/me");
        assert_eq!(expand_home("~/src/x", Some("/home/me/")), "/home/me/src/x");
        assert_eq!(expand_home("~other/x", Some("/home/me")), "~other/x");
        assert_eq!(expand_home("/abs", Some("/home/me")), "/abs");
        assert_eq!(expand_home("~/x", None), "~/x");
    }

    #[test]
    fn a_target_is_a_host_and_a_path_there() {
        let target = RemoteTarget::parse("box:~/src/repo").unwrap();
        assert_eq!(
            (target.host.0.as_str(), target.path.as_str()),
            ("box", "~/src/repo")
        );
        assert_eq!(target.to_string(), "box:~/src/repo");
        let named = RemoteTarget::parse("Ubuntu-24.04:/home/me/a:b").unwrap();
        assert_eq!(named.host.0, "Ubuntu-24.04");
        assert_eq!(named.path, "/home/me/a:b");
        // A drive, a local path, a name with a colon, a pull request.
        for local in [
            "C:/src/repo",
            "/home/me/a:/b",
            "notes:today",
            "owner/repo#12",
            "a/b:/c",
        ] {
            assert_eq!(RemoteTarget::parse(local), None, "{local}");
        }
    }

    #[test]
    fn a_failed_start_reads_as_what_happened() {
        let error = ConnectError::NoServer {
            command: "ssh box diffui-serve".to_owned(),
            exit: Exit {
                code: Some(127),
                stderr: "motd\nsh: diffui-serve: not found\n\n".to_owned(),
            },
            explained: None,
            timed_out: false,
        };
        assert_eq!(
            error.to_string(),
            "`ssh box diffui-serve` exited with 127 before diffui-serve answered: \
             sh: diffui-serve: not found"
        );
    }
}
