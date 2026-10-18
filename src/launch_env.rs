//! Repair `PATH` for a launch from Finder, the Dock or Spotlight.
//!
//! launchd starts an app bundle with `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so
//! everything diffui shells out to from Homebrew or nix — `gh` for PR tabs,
//! `gpg` and ssh signing programs, `git-lfs` behind git — is missing, while
//! the same binary started from a terminal works. When the environment looks
//! like that, ask the user's login shell for its `PATH` (the way a terminal
//! would have set it up) and fall back to appending the usual install dirs.
//!
//! A terminal launch is left alone, minimal `PATH` or not: there the user (or
//! their shell) chose it.

#[cfg(unix)]
use std::io::IsTerminal;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant};

/// What launchd hands a GUI app. A `PATH` made only of these is the default,
/// not a choice.
const SYSTEM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Brackets the shell's answer, so whatever its rc files print around it
/// (greetings, prompts, update nags) can be told apart from the value.
const MARKER: &str = "__DIFFUI_PATH__";

/// How long a login shell gets to start. A slow `.zshrc` beyond this costs the
/// launch too much; the fallback dirs cover the common installs.
#[cfg(unix)]
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Repair `PATH` if this looks like a GUI launch with launchd's default, and
/// describe what was done for the log (which isn't up yet when this runs).
///
/// Must run before any other thread exists: it calls `set_var`.
pub(crate) fn repair_path() -> Option<String> {
    #[cfg(unix)]
    {
        let current = std::env::var("PATH").unwrap_or_default();
        let terminal = std::io::stdin().is_terminal()
            || std::io::stdout().is_terminal()
            || std::io::stderr().is_terminal();
        if terminal || !looks_minimal(&current) {
            return None;
        }
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/zsh".to_owned());
        let from_shell = login_shell_path(&shell);
        let repaired = merge_paths(from_shell.as_ref().ok().map(String::as_str), &current, {
            let mut dirs = fallback_dirs();
            dirs.retain(|dir| dir.is_dir());
            dirs
        });
        // SAFETY: called first thing in `main`, before the app or any library
        // has spawned a thread, so nothing can be reading the environment
        // concurrently.
        unsafe { std::env::set_var("PATH", &repaired) };
        Some(match from_shell {
            Ok(_) => format!("GUI launch: PATH taken from login shell {shell}: {repaired}"),
            Err(error) => {
                format!("GUI launch: login shell {shell} gave no PATH ({error}); using {repaired}")
            }
        })
    }
    #[cfg(not(unix))]
    None
}

fn looks_minimal(path: &str) -> bool {
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .all(|entry| SYSTEM_DIRS.contains(&entry))
}

/// Where Homebrew (Apple silicon and Intel), nix (single-user, nix-darwin,
/// home-manager, the daemon's default profile) and per-user tool installs put
/// their binaries.
fn fallback_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/opt/homebrew/bin", "/usr/local/bin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = PathBuf::from(home);
        dirs.push(home.join(".nix-profile/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        dirs.push(PathBuf::from(format!("/etc/profiles/per-user/{user}/bin")));
    }
    dirs.push(PathBuf::from("/run/current-system/sw/bin"));
    dirs.push(PathBuf::from("/nix/var/nix/profiles/default/bin"));
    dirs
}

/// The login shell's `PATH`, or why there isn't one. Interactive (`-i`) as
/// well as login, because plenty of setups (nix's zsh hook, `brew shellenv`
/// in `.zshrc`) only run there.
#[cfg(unix)]
fn login_shell_path(shell: &str) -> Result<String, String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};

    let script = format!("printf '{MARKER}%s{MARKER}' \"$PATH\"");
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    // Polled rather than read on a helper thread: no thread may exist yet.
    // The answer is a few hundred bytes, so the pipe can't fill before exit
    // unless the rc files spew, which the timeout then covers.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < SHELL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {SHELL_TIMEOUT:?}"));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    parse_marked(&output).ok_or_else(|| "no PATH in its output".to_owned())
}

fn parse_marked(output: &str) -> Option<String> {
    let (_, rest) = output.split_once(MARKER)?;
    let (path, _) = rest.split_once(MARKER)?;
    let path = path.trim();
    path.contains('/').then(|| path.to_owned())
}

/// `primary` (the shell's answer) first, then what we started with, then the
/// fallback dirs; each entry once, in first-seen order.
fn merge_paths(primary: Option<&str>, current: &str, fallback: Vec<PathBuf>) -> String {
    let mut seen: Vec<String> = Vec::new();
    let entries = primary
        .into_iter()
        .chain([current])
        .flat_map(|path| path.split(':'))
        .map(str::to_owned)
        .chain(
            fallback
                .into_iter()
                .map(|dir| dir.to_string_lossy().into_owned()),
        );
    for entry in entries {
        if !entry.is_empty() && !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    seen.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_launchd_default_counts_as_minimal() {
        assert!(looks_minimal("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(looks_minimal(""));
        assert!(!looks_minimal("/opt/homebrew/bin:/usr/bin:/bin"));
    }

    #[test]
    fn the_marked_path_survives_rc_file_noise() {
        let output = format!("Welcome!\n{MARKER}/a/bin:/usr/bin{MARKER}\nbye\n");
        assert_eq!(parse_marked(&output).as_deref(), Some("/a/bin:/usr/bin"));
        assert_eq!(parse_marked("no markers here"), None);
        assert_eq!(parse_marked(&format!("{MARKER}{MARKER}")), None);
    }

    #[test]
    fn merging_keeps_the_shell_order_and_drops_repeats() {
        let merged = merge_paths(
            Some("/opt/homebrew/bin:/usr/bin"),
            "/usr/bin:/bin",
            vec![
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/nix/bin"),
            ],
        );
        assert_eq!(merged, "/opt/homebrew/bin:/usr/bin:/bin:/nix/bin");
        assert_eq!(
            merge_paths(None, "/usr/bin", vec![PathBuf::from("/x")]),
            "/usr/bin:/x"
        );
    }
}
