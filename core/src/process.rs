//! Running subprocesses (`git`, `gh`, a user's declared command), bounded.
//!
//! A child is spawned with `kill_on_drop`, so whatever stops waiting on it
//! (a timeout, a cancel, the job's future being dropped) also ends it. Before
//! this every call site spawned its own and waited without a limit: a stalled
//! connection froze the tab until restart, and a cancelled PR download kept
//! reading to the end.

use std::io;
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

use crate::cli::passthrough::{CliLine, CliStream};
use crate::repo::CancelFlag;

/// How long one network request (`gh api`, `gh pr view`) may take, and how
/// long a streaming one may go without sending anything.
pub(crate) const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a wait looks at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Run `command` to completion and collect its output, giving up after
/// `timeout` or once `cancel` is raised. Giving up kills the child. stdin is
/// `/dev/null`.
pub(crate) async fn run(
    command: &mut Command,
    timeout: Option<Duration>,
    cancel: Option<&CancelFlag>,
) -> io::Result<Output> {
    let child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    bounded(child.wait_with_output(), timeout, cancel).await?
}

/// Run `command` with `stdin` written to it, handing each line it prints to
/// `on_line` as it comes (stdout and stderr interleaved as they arrive, the
/// newline dropped), until it exits. Like [`run`], it gives up after
/// `timeout` or once `cancel` is raised, and giving up kills the child.
///
/// A program that isn't there fails with [`io::ErrorKind::NotFound`] before
/// anything runs.
pub async fn stream(
    command: &mut Command,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
    cancel: Option<&CancelFlag>,
    mut on_line: impl FnMut(CliLine) + Send,
) -> io::Result<ExitStatus> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let input = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async move {
        // The child may exit without reading all of its input (or at all):
        // that is its business, not a failure of the run.
        let feed = async move {
            if let (Some(mut input), Some(bytes)) = (input, stdin) {
                let _ = input.write_all(&bytes).await;
                let _ = input.shutdown().await;
            }
        };
        let mut out = stdout.map(|pipe| BufReader::new(pipe).lines());
        let mut err = stderr.map(|pipe| BufReader::new(pipe).lines());
        let read = async {
            loop {
                let line = tokio::select! {
                    Some(line) = next_line(&mut out) => line.map(|text| (CliStream::Stdout, text)),
                    Some(line) = next_line(&mut err) => line.map(|text| (CliStream::Stderr, text)),
                    else => break,
                };
                match line {
                    Ok((stream, text)) => on_line(CliLine { stream, text }),
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        };
        let ((), read) = tokio::join!(feed, read);
        read?;
        child.wait().await
    };
    bounded(work, timeout, cancel).await?
}

/// The next line of `lines`, or `None` once it's done (and from then on).
async fn next_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut Option<tokio::io::Lines<R>>,
) -> Option<io::Result<String>> {
    let reader = lines.as_mut()?;
    match reader.next_line().await {
        Ok(Some(line)) => Some(Ok(line)),
        Ok(None) => {
            *lines = None;
            None
        }
        Err(error) => {
            *lines = None;
            Some(Err(error))
        }
    }
}

/// Await `work`, giving up after `timeout` or once `cancel` is raised, with an
/// error of kind [`io::ErrorKind::TimedOut`] or [`io::ErrorKind::Interrupted`].
/// Giving up drops `work`, and with it any child it owns.
pub(crate) async fn bounded<F: Future>(
    work: F,
    timeout: Option<Duration>,
    cancel: Option<&CancelFlag>,
) -> io::Result<F::Output> {
    let deadline = async {
        match timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending().await,
        }
    };
    let cancelled = async {
        let Some(cancel) = cancel else {
            return std::future::pending().await;
        };
        while !cancel.is_cancelled() {
            tokio::time::sleep(CANCEL_POLL).await;
        }
    };
    tokio::select! {
        output = work => Ok(output),
        () = deadline => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "no answer within {} seconds",
                timeout.unwrap_or_default().as_secs()
            ),
        )),
        () = cancelled => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_timeout_kills_the_child() {
        let started = std::time::Instant::now();
        let error = run(
            Command::new("sleep").arg("30"),
            Some(Duration::from_millis(100)),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_cancel_stops_the_wait() {
        let cancel = CancelFlag::default();
        cancel.cancel();
        let error = run(Command::new("sleep").arg("30"), None, Some(&cancel))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn a_stream_feeds_stdin_and_hands_back_each_line() {
        let mut lines = Vec::new();
        let status = stream(
            Command::new("sh").args(["-c", "tr a-z A-Z; echo done >&2; exit 3"]),
            Some(b"one\ntwo\n".to_vec()),
            Some(NETWORK_TIMEOUT),
            None,
            |line| lines.push((line.stream, line.text)),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(3));
        let out: Vec<&str> = lines
            .iter()
            .filter(|(stream, _)| *stream == CliStream::Stdout)
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(out, ["ONE", "TWO"]);
        assert!(lines.contains(&(CliStream::Stderr, "done".to_owned())));
    }

    #[tokio::test]
    async fn a_missing_program_says_so() {
        let error = stream(
            &mut Command::new("diffui-no-such-program"),
            None,
            None,
            None,
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn a_cancelled_stream_kills_the_child() {
        let cancel = CancelFlag::default();
        cancel.cancel();
        let error = stream(
            Command::new("sleep").arg("30"),
            None,
            None,
            Some(&cancel),
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn output_comes_back_when_nothing_intervenes() {
        let output = run(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Some(NETWORK_TIMEOUT),
            Some(&CancelFlag::default()),
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }
}
