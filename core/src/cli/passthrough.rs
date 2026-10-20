//! Running the real jj CLI for the commands diffui doesn't run itself.
//!
//! The CLI is diffui's own binary: started with `argv[0]` = `jj` it runs
//! jj-cli 0.45.1's `CliRunner` (see [`jj_main`]), which links the same jj-lib
//! diffui embeds. So the CLI always matches the repo format diffui writes,
//! whatever `jj` is on the user's `PATH`, and it reads the user's jj config
//! the way the real CLI does. Core can't know the app's executable, so the
//! caller names it in [`CliInvocation::program`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};

use crate::repo::cancel::CancelFlag;
use crate::repo::{JobId, Payload};

/// The jj-cli version linked in, which `jj version` reports.
pub const JJ_VERSION: &str = "0.45.1";

/// Set in a passthrough child's environment: the CLI then writes plain text
/// (no colour, no pager) whatever the user's `ui.color` says.
const PASSTHROUGH_ENV: &str = "DIFFUI_JJ_PASSTHROUGH";

/// How long output may sit before it's sent, and how often a cancel is
/// noticed while the child is quiet.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Lines per [`Payload::CliOutput`] at most.
const MAX_BATCH: usize = 256;

/// One jj CLI run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInvocation {
    /// An executable that runs jj-cli when started as `jj`: the app's own
    /// binary (`std::env::current_exe()`).
    pub program: PathBuf,
    /// The arguments after `jj`.
    pub args: Vec<String>,
    /// Extra environment for the child, applied after diffui's own so it can
    /// override them. This is where git credential helpers (askpass) go for
    /// a passthrough `jj git push` / `jj git fetch`.
    pub env: Vec<(String, String)>,
}

impl CliInvocation {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
        }
    }
}

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliStream {
    Stdout,
    Stderr,
}

/// One line of CLI output, without its newline and with any ANSI styling
/// removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLine {
    pub stream: CliStream,
    pub text: String,
}

/// The editor a passthrough child gets: jj would open `$EDITOR` for a
/// description, and there is no terminal to show it in, so it fails at once
/// with the fix in its message.
#[cfg(unix)]
fn failing_editor() -> String {
    "sh -c 'echo \"diffui: no editor for jj here, pass -m\" >&2; exit 1'".to_owned()
}

#[cfg(not(unix))]
fn failing_editor() -> String {
    "false".to_owned()
}

/// The child process, configured but not started.
fn command(invocation: &CliInvocation, cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&invocation.program);
    #[cfg(unix)]
    command.arg0("jj");
    command
        .args(&invocation.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env(PASSTHROUGH_ENV, "1")
        .env("JJ_EDITOR", failing_editor())
        // No terminal to answer git's credential prompt on: fail instead of
        // waiting forever (askpass in `invocation.env` takes over).
        .env("GIT_TERMINAL_PROMPT", "0");
    for (name, value) in &invocation.env {
        command.env(name, value);
    }
    command
}

/// Run `invocation` in `cwd`, streaming its output as
/// [`Payload::CliOutput`] and ending with [`Payload::CliExited`], or
/// [`Payload::Cancelled`] once `cancel` is raised (the child is killed).
pub(crate) async fn run(
    job: JobId,
    invocation: &CliInvocation,
    cwd: &Path,
    cancel: &CancelFlag,
    mut emit: impl FnMut(Payload),
) -> Result<()> {
    let mut child = command(invocation, cwd)
        .spawn()
        .with_context(|| format!("failed to start {}", invocation.program.display()))?;
    let mut stdout = Reader::new(child.stdout.take(), CliStream::Stdout);
    let mut stderr = Reader::new(child.stderr.take(), CliStream::Stderr);
    let mut pending: Vec<CliLine> = Vec::new();
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    fn flush(job: JobId, pending: &mut Vec<CliLine>, emit: &mut impl FnMut(Payload)) {
        if !pending.is_empty() {
            emit(Payload::CliOutput {
                job,
                lines: std::mem::take(pending),
            });
        }
    }
    let status = loop {
        tokio::select! {
            line = stdout.next(), if !stdout.done => pending.extend(line),
            line = stderr.next(), if !stderr.done => pending.extend(line),
            status = child.wait(), if stdout.done && stderr.done => {
                break status.context("failed to wait for jj")?;
            }
            _ = tick.tick() => {
                if cancel.is_cancelled() {
                    let _ = child.kill().await;
                    flush(job, &mut pending, &mut emit);
                    emit(Payload::Cancelled { job });
                    return Ok(());
                }
                flush(job, &mut pending, &mut emit);
            }
        }
        if pending.len() >= MAX_BATCH {
            flush(job, &mut pending, &mut emit);
        }
    };
    flush(job, &mut pending, &mut emit);
    emit(Payload::CliExited {
        job,
        code: status.code(),
    });
    Ok(())
}

/// Line reader over one of the child's pipes.
struct Reader<R> {
    reader: Option<BufReader<R>>,
    stream: CliStream,
    buf: Vec<u8>,
    done: bool,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    fn new(pipe: Option<R>, stream: CliStream) -> Self {
        Self {
            done: pipe.is_none(),
            reader: pipe.map(BufReader::new),
            stream,
            buf: Vec::new(),
        }
    }

    /// The next line, or `None` at end of stream. Cancel-safe: a partial
    /// line stays in `buf` for the next call.
    async fn next(&mut self) -> Option<CliLine> {
        let reader = self.reader.as_mut()?;
        let read = reader.read_until(b'\n', &mut self.buf).await;
        if matches!(read, Ok(0) | Err(_)) {
            self.done = true;
            if self.buf.is_empty() {
                return None;
            }
        }
        let bytes = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        Some(CliLine {
            stream: self.stream,
            text: strip_ansi(text),
        })
    }
}

/// Drop ANSI escape sequences (CSI `ESC [ … final`, and OSC `ESC ] … BEL`),
/// in case the user's config forces colour where the child's own override
/// didn't reach (an error printed before the command ran).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The app's first call in `main`, before anything else is set up.
///
/// - Started as `jj` (`argv[0]`'s file name): run jj-cli and return its exit
///   code. This is how passthrough children run.
/// - Started as `diffui jj <args…>`: re-execute as `jj <args…>`, so jj's
///   usage and error text say `jj`. Returns only if that fails.
/// - Otherwise `None`: carry on as the GUI.
pub fn jj_main() -> Option<u8> {
    let mut args = std::env::args_os();
    let argv0 = args.next()?;
    if Path::new(&argv0).file_name() == Some(OsStr::new("jj")) {
        return Some(run_cli());
    }
    if args.next().as_deref() != Some(OsStr::new("jj")) {
        return None;
    }
    Some(reexec_as_jj(args))
}

#[cfg(unix)]
fn reexec_as_jj(args: std::env::ArgsOs) -> u8 {
    use std::os::unix::process::CommandExt as _;
    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).arg0("jj").args(args).exec(),
        Err(error) => error,
    };
    eprintln!("diffui: failed to start jj: {error}");
    1
}

#[cfg(not(unix))]
fn reexec_as_jj(_args: std::env::ArgsOs) -> u8 {
    eprintln!("diffui: `diffui jj` needs a unix platform");
    1
}

fn run_cli() -> u8 {
    let runner = jj_cli::cli_util::CliRunner::init().version(JJ_VERSION);
    let runner = if std::env::var_os(PASSTHROUGH_ENV).is_some() {
        runner.add_dispatch_hook(async |ui, helper, dispatch| {
            // Plain output for the GUI, without putting `--color=never
            // --no-pager` into the args jj records in the op log.
            let mut config = helper.settings().config().clone();
            let mut layer =
                jj_lib::config::ConfigLayer::empty(jj_lib::config::ConfigSource::CommandArg);
            layer
                .set_value("ui.color", "never")
                .expect("a plain key takes a string");
            layer
                .set_value("ui.paginate", "never")
                .expect("a plain key takes a string");
            config.add_layer(layer);
            ui.reset(&config)?;
            dispatch.call(ui, helper).await
        })
    } else {
        runner
    };
    runner.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_styling() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[38;5;2mok\u{1b}[0m done"),
            "ok done"
        );
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"),
            "link"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_lines_and_reports_the_exit_code() {
        // `sh` stands in for the app binary: started as `jj` it runs the
        // script it's given, which is enough to check the plumbing.
        let invocation = CliInvocation {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "echo out1; echo err1 >&2; printf 'no newline'; test \"$X\" = y && exit 3".into(),
            ],
            env: vec![("X".into(), "y".into())],
        };
        let mut payloads = Vec::new();
        run(
            JobId(7),
            &invocation,
            Path::new("/"),
            &CancelFlag::default(),
            |p| payloads.push(p),
        )
        .await
        .unwrap();
        let mut lines = Vec::new();
        for payload in &payloads[..payloads.len() - 1] {
            match payload {
                Payload::CliOutput { job, lines: batch } => {
                    assert_eq!(*job, JobId(7));
                    lines.extend(batch.iter().cloned());
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let text = |stream| {
            lines
                .iter()
                .filter(|l| l.stream == stream)
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(text(CliStream::Stdout), ["out1", "no newline"]);
        assert_eq!(text(CliStream::Stderr), ["err1"]);
        assert!(matches!(
            payloads.last(),
            Some(Payload::CliExited { code: Some(3), .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_child() {
        let invocation = CliInvocation::new("/bin/sh", vec!["-c".into(), "sleep 30".into()]);
        let cancel = CancelFlag::default();
        cancel.cancel();
        let mut payloads = Vec::new();
        let started = std::time::Instant::now();
        run(JobId(1), &invocation, Path::new("/"), &cancel, |p| {
            payloads.push(p)
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(payloads.as_slice(), [Payload::Cancelled { .. }]));
    }
}
