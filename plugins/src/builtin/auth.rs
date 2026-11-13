//! `auth.token`: a credential for a host the plugin may talk to.
//!
//! diffui keeps no token store. The user's `[plugins.<id>] token_cmd` argv
//! answers when set; otherwise git's credential helpers do, with every way
//! of prompting turned off: this runs on a worker thread nobody is watching.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Map, Value};

use super::exec::{self, Run};
use crate::host::HostError;
use crate::manifest::{HostPattern, TOKEN_CMD};

/// A credential helper is local; one that hangs is asking for something.
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 64 * 1024;

/// `host` (with its port, if the remote has one) must be one the manifest
/// lets the plugin reach: a token is only useful where it can be sent.
pub(crate) fn check(host: &str, allowed: &[HostPattern]) -> Result<(), HostError> {
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => (name, port.parse().ok()),
        _ => (host, None),
    };
    // Without a port the https default stands in, which only matters to a
    // pattern that names one.
    if allowed
        .iter()
        .any(|pattern| pattern.matches(name, port.unwrap_or(443)))
    {
        return Ok(());
    }
    Err(HostError::denied(format!(
        "auth.token(\"{host}\") is not allowed: plugin.toml's [permissions] http must list the host"
    )))
}

/// The user's `token_cmd`, when the plugin's config has one.
pub(crate) fn token_cmd(config: &Map<String, Value>) -> Result<Option<Vec<String>>, HostError> {
    let Some(value) = config.get(TOKEN_CMD) else {
        return Ok(None);
    };
    let argv: Option<Vec<String>> =
        value
            .as_array()
            .filter(|words| !words.is_empty())
            .and_then(|words| {
                words
                    .iter()
                    .map(|word| word.as_str().map(str::to_string))
                    .collect()
            });
    argv.map(Some).ok_or_else(|| {
        HostError::new(
            "Error",
            format!("{TOKEN_CMD} must be an argv like [\"pass\", \"show\", \"{{host}}\"]"),
        )
    })
}

/// Run a helper to its end, in `cwd`.
fn run(
    argv: &[String],
    stdin: Option<String>,
    env: &[(&str, &str)],
    cwd: &Path,
    cancel: &AtomicBool,
) -> Result<exec::Output, HostError> {
    let run = Run {
        argv,
        stdin,
        cwd,
        env,
        timeout: TIMEOUT,
        max_output: MAX_OUTPUT,
    };
    exec::run(run, cancel, |_| {})
}

/// Ask for `host`'s token. `{host}` in a `token_cmd` word is replaced.
pub(crate) fn token(
    host: &str,
    token_cmd: Option<&[String]>,
    cwd: &Path,
    cancel: &AtomicBool,
) -> Result<String, HostError> {
    match token_cmd {
        Some(argv) => {
            let argv: Vec<String> = argv
                .iter()
                .map(|word| word.replace("{host}", host))
                .collect();
            let output = run(&argv, None, &[], cwd, cancel)?;
            let token = output.stdout.lines().next().unwrap_or("").trim();
            if output.code != Some(0) || token.is_empty() {
                let said = output.stderr.lines().last().unwrap_or("it printed nothing");
                return Err(HostError::new(
                    "Error",
                    format!(
                        "No token for {host}: {TOKEN_CMD} ({}) failed: {said}",
                        argv[0]
                    ),
                ));
            }
            Ok(token.to_string())
        }
        None => credential_fill("git", host, cwd, cancel),
    }
}

/// `git credential fill` for `host`, through whatever helpers git has.
fn credential_fill(
    git: &str,
    host: &str,
    cwd: &Path,
    cancel: &AtomicBool,
) -> Result<String, HostError> {
    let argv = [git, "credential", "fill"].map(String::from);
    // An empty GIT_ASKPASS wins over core.askPass and SSH_ASKPASS and then
    // counts as none (git's prompt.c), and the terminal prompt is off, so a
    // missing credential is an error, never a dialog.
    let env = [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", ""),
        ("GCM_INTERACTIVE", "never"),
    ];
    let asked = format!("protocol=https\nhost={host}\n\n");
    let output = run(&argv, Some(asked), &env, cwd, cancel)?;
    output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("password="))
        .filter(|token| output.code == Some(0) && !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            HostError::new(
                "Error",
                format!(
                    "No token for {host}: git has no stored credential for it. Store one with `git credential approve`, or set {TOKEN_CMD} for this plugin in config.toml."
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_only_for_a_listed_host() {
        let allowed = vec![
            HostPattern::parse("codeberg.org").unwrap(),
            HostPattern::parse("127.0.0.1:3000").unwrap(),
        ];
        assert!(check("codeberg.org", &allowed).is_ok());
        assert!(check("127.0.0.1:3000", &allowed).is_ok());
        for host in [
            "github.com",
            "127.0.0.1:3001",
            "127.0.0.1",
            "evil.codeberg.org",
        ] {
            let error = check(host, &allowed).unwrap_err();
            assert_eq!(error.name, "PermissionError", "{host}");
        }
    }

    #[test]
    fn token_cmd_is_an_argv_with_the_host_filled_in() {
        let config: Map<String, Value> =
            serde_json::from_str(r#"{ "token_cmd": ["sh", "-c", "echo tok-for-$0", "{host}"] }"#)
                .unwrap();
        let argv = token_cmd(&config).unwrap().unwrap();
        let token = token(
            "codeberg.org",
            Some(&argv),
            Path::new("/"),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(token, "tok-for-codeberg.org");

        let failing = ["sh", "-c", "echo locked >&2; exit 1"].map(String::from);
        let error = super::token(
            "codeberg.org",
            Some(&failing),
            Path::new("/"),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "No token for codeberg.org: token_cmd (sh) failed: locked"
        );

        let config: Map<String, Value> =
            serde_json::from_str(r#"{ "token_cmd": "pass show x" }"#).unwrap();
        assert!(
            token_cmd(&config)
                .unwrap_err()
                .message
                .contains("must be an argv")
        );
        assert_eq!(token_cmd(&Map::new()).unwrap(), None);
    }

    /// A stand-in for git that answers `credential fill` from its stdin and
    /// environment, so no real helper (or keychain) is asked.
    #[test]
    fn git_is_asked_without_any_way_to_prompt() {
        let dir = std::env::temp_dir().join(format!("diffui-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = dir.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\n[ \"$1 $2\" = \"credential fill\" ] || exit 9\nasked=$(cat | tr '\\n' ' ')\n[ \"$asked\" = \"protocol=https host=known.test  \" ] || exit 1\necho \"username=me\"\necho \"password=tok-$GIT_TERMINAL_PROMPT-${GIT_ASKPASS:-empty}-$GCM_INTERACTIVE\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = AtomicBool::new(false);
        let git = git.to_str().unwrap();
        assert_eq!(
            credential_fill(git, "known.test", &dir, &cancel).unwrap(),
            "tok-0-empty-never"
        );
        let error = credential_fill(git, "unknown.test", &dir, &cancel).unwrap_err();
        assert!(
            error
                .message
                .starts_with("No token for unknown.test: git has no stored credential for it."),
            "{}",
            error.message
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
