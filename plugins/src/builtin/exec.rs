//! `exec`: run a program the manifest allows, bounded.
//!
//! Plain threads rather than `diffui_core::process`: that runner is tokio's,
//! and a plugin worker is a blocking thread with no runtime. The bounds are
//! the same: a timeout, a cancel flag, and the child is killed when either
//! ends the wait.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::host::HostError;

/// How often a wait looks at its cancel flag and deadline.
const POLL: Duration = Duration::from_millis(20);

/// What a script passes to `exec`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecRequest {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    /// The script passed `onStdout`: send lines as they come.
    #[serde(default)]
    pub stream: bool,
}

pub(crate) struct Output {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Refuse an argv no prefix in the manifest covers.
pub(crate) fn check(argv: &[String], allowed: &[Vec<String>]) -> Result<(), HostError> {
    if allowed.iter().any(|prefix| argv.starts_with(prefix)) {
        return Ok(());
    }
    let listed = match allowed {
        [] => "it lists none".to_string(),
        prefixes => format!(
            "it allows {}",
            prefixes
                .iter()
                .map(|prefix| format!("{prefix:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Err(HostError::denied(format!(
        "exec {argv:?} is not allowed: plugin.toml's [permissions] exec must list an argv prefix for it, and {listed}"
    )))
}

/// One program to run.
pub(crate) struct Run<'a> {
    pub argv: &'a [String],
    pub stdin: Option<String>,
    pub cwd: &'a Path,
    pub env: &'a [(&'a str, &'a str)],
    pub timeout: Duration,
    /// The most either output stream may hold, in bytes.
    pub max_output: usize,
}

/// Run it to its end and collect what it wrote. `on_line` hears each stdout
/// line as it arrives. Gives up, killing the child, once `cancel` is raised,
/// after the timeout, or when either stream passes `max_output`.
pub(crate) fn run(
    run: Run<'_>,
    cancel: &AtomicBool,
    mut on_line: impl FnMut(&str),
) -> Result<Output, HostError> {
    let Run {
        argv,
        stdin,
        cwd,
        env,
        timeout,
        max_output,
    } = run;
    let program = argv
        .first()
        .ok_or_else(|| HostError::new("TypeError", "exec needs a program"))?;
    let mut child = Command::new(program)
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| HostError::new("Error", format!("can't run {program}: {error}")))?;

    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // On its own thread: a child that fills its stdout before reading
        // would otherwise deadlock against us.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let (lines, incoming) = mpsc::channel();
    let out = child.stdout.take().expect("stdout is piped");
    std::thread::spawn(move || {
        let mut out = BufReader::new(out);
        let mut line = Vec::new();
        while out.read_until(b'\n', &mut line).is_ok_and(|read| read > 0) {
            if lines
                .send(String::from_utf8_lossy(&line).into_owned())
                .is_err()
            {
                break;
            }
            line.clear();
        }
    });
    let mut err = child.stderr.take().expect("stderr is piped");
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        // One byte over, so the caller can tell "at the cap" from "past it".
        let _ = (&mut err)
            .take(max_output as u64 + 1)
            .read_to_end(&mut bytes);
        // Keep draining so the child never blocks on a full pipe.
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        bytes
    });

    let deadline = Instant::now() + timeout;
    let mut stdout = String::new();
    let mut open = true;
    let gave_up = |child: &mut std::process::Child, error: HostError| {
        let _ = child.kill();
        let _ = child.wait();
        Err(error)
    };
    let mut take = |line: String, stdout: &mut String| {
        let text = line.strip_suffix('\n').unwrap_or(&line);
        on_line(text.strip_suffix('\r').unwrap_or(text));
        stdout.push_str(&line);
    };
    let status = loop {
        match incoming.recv_timeout(POLL) {
            Ok(line) => take(line, &mut stdout),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => open = false,
        }
        if stdout.len() > max_output {
            return gave_up(
                &mut child,
                HostError::new(
                    "Error",
                    format!(
                        "{program} wrote more than {} MB",
                        max_output / (1024 * 1024)
                    ),
                ),
            );
        }
        if cancel.load(Ordering::Relaxed) {
            return gave_up(
                &mut child,
                HostError::new("AbortError", "The operation was aborted"),
            );
        }
        if Instant::now() >= deadline {
            return gave_up(
                &mut child,
                HostError::new(
                    "TimeoutError",
                    format!("{program} did not finish within {} s", timeout.as_secs()),
                ),
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // What it wrote before exiting is still on its way.
                while let Ok(line) =
                    incoming.recv_timeout(if open { POLL * 5 } else { Duration::ZERO })
                {
                    take(line, &mut stdout);
                }
                break status;
            }
            Ok(None) if !open => std::thread::sleep(POLL),
            Ok(None) => {}
            Err(error) => {
                return gave_up(
                    &mut child,
                    HostError::new("Error", format!("{program}: {error}")),
                );
            }
        }
    };
    let stderr = errors.join().unwrap_or_default();
    if stderr.len() > max_output {
        return Err(HostError::new(
            "Error",
            format!(
                "{program} wrote more than {} MB",
                max_output / (1024 * 1024)
            ),
        ));
    }
    Ok(Output {
        code: status.code(),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn run_sh(
        script: &str,
        stdin: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Output, HostError> {
        run(
            Run {
                argv: &sh(script),
                stdin: stdin.map(str::to_string),
                cwd: Path::new("/"),
                env: &[("GREETING", "hi")],
                timeout,
                max_output: 1024 * 1024,
            },
            cancel,
            |_| {},
        )
    }

    #[test]
    fn a_prefix_covers_longer_argvs_only() {
        let allowed = vec![vec!["git".to_string(), "credential".to_string()]];
        let argv = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        assert!(check(&argv(&["git", "credential", "fill"]), &allowed).is_ok());
        assert!(check(&argv(&["git", "credential"]), &allowed).is_ok());
        for refused in [
            &["git"][..],
            &["git", "push"],
            &["/usr/bin/git", "credential"],
            &["rm", "-rf", "/"],
        ] {
            let error = check(&argv(refused), &allowed).unwrap_err();
            assert_eq!(error.name, "PermissionError");
            assert!(
                error
                    .message
                    .contains("it allows [\"git\", \"credential\"]"),
                "{}",
                error.message
            );
        }
        assert!(
            check(&argv(&["ls"]), &[])
                .unwrap_err()
                .message
                .contains("it lists none")
        );
    }

    #[test]
    fn output_stdin_env_and_exit_code_come_back() {
        let output = run_sh(
            "cat; echo $GREETING; echo oops >&2; exit 3",
            Some("in\n"),
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout, "in\nhi\n");
        assert_eq!(output.stderr, "oops\n");
    }

    #[test]
    fn lines_stream_as_they_come() {
        let mut heard = Vec::new();
        run(
            Run {
                argv: &sh("echo one; echo two; printf three"),
                stdin: None,
                cwd: Path::new("/"),
                env: &[],
                timeout: Duration::from_secs(10),
                max_output: 1024,
            },
            &AtomicBool::new(false),
            |line| heard.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(heard, ["one", "two", "three"]);
    }

    #[test]
    fn a_timeout_a_cancel_and_a_flood_all_end_the_child() {
        let started = Instant::now();
        let error = run_sh(
            "sleep 30",
            None,
            Duration::from_millis(100),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert_eq!(error.name, "TimeoutError");
        let error = run_sh(
            "sleep 30",
            None,
            Duration::from_secs(30),
            &AtomicBool::new(true),
        )
        .err()
        .unwrap();
        assert_eq!(error.name, "AbortError");
        let error = run_sh(
            "yes",
            None,
            Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(
            error.message.contains("wrote more than 1 MB"),
            "{}",
            error.message
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_says_so() {
        let missing = Run {
            argv: &["no-such-program-here".into()],
            stdin: None,
            cwd: Path::new("/"),
            env: &[],
            timeout: Duration::from_secs(1),
            max_output: 1024,
        };
        let error = run(missing, &AtomicBool::new(false), |_| {}).err().unwrap();
        assert!(
            error.message.starts_with("can't run no-such-program-here:"),
            "{}",
            error.message
        );
    }
}
