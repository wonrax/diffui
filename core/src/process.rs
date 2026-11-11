//! Running the `git` and `gh` subprocesses, bounded.
//!
//! A child is spawned with `kill_on_drop`, so whatever stops waiting on it
//! (a timeout, a cancel, the job's future being dropped) also ends it. Before
//! this every call site spawned its own and waited without a limit: a stalled
//! connection froze the tab until restart, and a cancelled PR download kept
//! reading to the end.

use std::io;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;

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
