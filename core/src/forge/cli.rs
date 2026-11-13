//! Running a forge's CLI (`gh`, `glab`) and turning how it failed into a
//! [`ForgeError`].

use std::io;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{ForgeError, ForgeKind, ForgeResult};
use crate::process::NETWORK_TIMEOUT;
use crate::repo::CancelFlag;

/// One forge CLI: which program, for which forge.
#[derive(Debug, Clone)]
pub(super) struct Cli {
    pub kind: ForgeKind,
    pub program: PathBuf,
}

impl Cli {
    /// The program, pointed at `host` through its host variable.
    pub fn command(&self, host: &str) -> Command {
        let mut command = Command::new(&self.program);
        command.env(self.kind.host_env(), host);
        command
    }

    /// Run with `args` against `host` and return stdout. One request, so it
    /// gets [`NETWORK_TIMEOUT`] to answer.
    pub async fn run(
        &self,
        host: &str,
        args: &[&str],
        cancel: Option<&CancelFlag>,
    ) -> ForgeResult<Vec<u8>> {
        let output =
            crate::process::run(self.command(host).args(args), Some(NETWORK_TIMEOUT), cancel)
                .await
                .map_err(|error| self.wait_error(host, error))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // gh api prints the API's error body on stdout.
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(self.failure(host, &stderr, &stdout));
        }
        Ok(output.stdout)
    }

    /// [`Self::run`], with stdout parsed as JSON.
    pub async fn json<T: serde::de::DeserializeOwned>(
        &self,
        host: &str,
        args: &[&str],
        cancel: Option<&CancelFlag>,
    ) -> ForgeResult<T> {
        let stdout = self.run(host, args, cancel).await?;
        serde_json::from_slice(&stdout).map_err(|error| {
            ForgeError::Failed(format!(
                "{} returned something diffui couldn't read: {error}",
                self.display()
            ))
        })
    }

    fn display(&self) -> String {
        Path::new(&self.program).file_name().map_or_else(
            || self.kind.cli().to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// Couldn't start it, or it stopped answering.
    pub fn wait_error(&self, host: &str, error: io::Error) -> ForgeError {
        match error.kind() {
            io::ErrorKind::NotFound => ForgeError::MissingCli { kind: self.kind },
            io::ErrorKind::TimedOut => ForgeError::TimedOut {
                host: host.to_owned(),
                seconds: NETWORK_TIMEOUT.as_secs(),
            },
            io::ErrorKind::Interrupted => ForgeError::Cancelled,
            _ => ForgeError::Failed(format!("couldn't run {}: {error}", self.display())),
        }
    }

    /// A run that exited non-zero. The CLIs relay the API's own messages, so
    /// this keys on the phrases they use for the cases that have a fix.
    pub fn failure(&self, host: &str, stderr: &str, stdout: &str) -> ForgeError {
        let text = format!("{stderr}\n{stdout}");
        let lower = text.to_ascii_lowercase();
        let host = host.to_owned();
        let kind = self.kind;
        if lower.contains("auth login")
            || lower.contains("not logged in")
            || lower.contains("not logged into")
            || lower.contains("authentication required")
            || lower.contains("bad credentials")
            || lower.contains("401 unauthorized")
            || lower.contains("http 401")
            || lower.contains("has not been authenticated")
            || lower.contains("no token found")
        {
            return ForgeError::NotAuthenticated { kind, host };
        }
        if lower.contains("rate limit") {
            return ForgeError::RateLimited { kind, host };
        }
        if lower.contains("could not resolve to a")
            || lower.contains("http 404")
            || lower.contains("404 not found")
            || lower.contains("404 project not found")
        {
            return ForgeError::NotFound {
                kind,
                host,
                what: "The project".to_owned(),
            };
        }
        if lower.contains("error connecting to")
            || lower.contains("no such host")
            || lower.contains("could not resolve host")
            || lower.contains("dial tcp")
        {
            return ForgeError::Unreachable { host };
        }
        let detail = text.trim();
        let detail: String = detail
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        ForgeError::Failed(if detail.is_empty() {
            format!("{} failed without saying why", self.display())
        } else {
            format!("{} failed: {detail}", self.display())
        })
    }
}

/// The hosts a CLI has been signed in to, from its config file's `hosts`:
/// what lets a provider claim a company host whose name doesn't say which
/// forge it is. Host names only; nothing else is read.
pub(super) fn configured_hosts(kind: ForgeKind) -> Vec<String> {
    let config_dir = |var: &str, dir: &str| -> Option<PathBuf> {
        std::env::var_os(var)
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME").map(|base| PathBuf::from(base).join(dir))
            })
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join(dir))
            })
    };
    let (file, top_level) = match kind {
        ForgeKind::GitHub => (
            config_dir("GH_CONFIG_DIR", "gh").map(|dir| dir.join("hosts.yml")),
            true,
        ),
        ForgeKind::GitLab => (
            config_dir("GLAB_CONFIG_DIR", "glab-cli").map(|dir| dir.join("config.yml")),
            false,
        ),
    };
    file.and_then(|file| std::fs::read_to_string(file).ok())
        .map(|text| yaml_hosts(&text, top_level))
        .unwrap_or_default()
}

/// The host keys of a CLI config: `gh`'s `hosts.yml` keys them at the top
/// level, `glab`'s `config.yml` one level under `hosts:`.
fn yaml_hosts(text: &str, top_level: bool) -> Vec<String> {
    let key = |line: &str| -> Option<String> {
        let key = line.trim().strip_suffix(':')?;
        let key = key.trim_matches(['"', '\'']);
        (super::remote::looks_like_host(key)).then(|| key.to_ascii_lowercase())
    };
    if top_level {
        return text
            .lines()
            .filter(|line| !line.starts_with([' ', '\t', '#']))
            .filter_map(key)
            .collect();
    }
    let mut hosts = Vec::new();
    let mut in_hosts = false;
    let mut indent: Option<usize> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        if depth == 0 {
            in_hosts = line.trim_end() == "hosts:";
            indent = None;
            continue;
        }
        if !in_hosts {
            continue;
        }
        let level = *indent.get_or_insert(depth);
        if depth == level
            && let Some(host) = key(line)
        {
            hosts.push(host);
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(kind: ForgeKind) -> Cli {
        Cli {
            kind,
            program: kind.cli().into(),
        }
    }

    #[test]
    fn failures_get_typed() {
        let gh = cli(ForgeKind::GitHub);
        let host = "github.acme.com";
        assert_eq!(
            gh.failure(
                host,
                "To get started with GitHub CLI, please run:  gh auth login",
                ""
            ),
            ForgeError::NotAuthenticated {
                kind: ForgeKind::GitHub,
                host: host.to_owned()
            }
        );
        assert!(matches!(
            gh.failure(host, "HTTP 403: API rate limit exceeded for user ID 1.", ""),
            ForgeError::RateLimited { .. }
        ));
        assert!(matches!(
            gh.failure(host, "gh: Not Found (HTTP 404)", ""),
            ForgeError::NotFound { .. }
        ));
        assert!(matches!(
            gh.failure(host, "error connecting to github.acme.com", ""),
            ForgeError::Unreachable { .. }
        ));
        let glab = cli(ForgeKind::GitLab);
        assert!(matches!(
            glab.failure("gitlab.com", "ERROR: 401 Unauthorized", ""),
            ForgeError::NotAuthenticated { .. }
        ));
        match gh.failure(host, "", "{\"message\":\"Validation Failed\"}") {
            ForgeError::Failed(message) => assert!(message.contains("Validation Failed")),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            gh.wait_error(host, io::Error::from(io::ErrorKind::NotFound)),
            ForgeError::MissingCli {
                kind: ForgeKind::GitHub
            }
        );
    }

    #[test]
    fn config_hosts_are_read_by_key() {
        let gh = "github.com:\n    user: me\n    oauth_token: x\ngithub.acme.com:\n    user: me\n";
        assert_eq!(yaml_hosts(gh, true), ["github.com", "github.acme.com"]);
        let glab = "git_protocol: ssh\nhosts:\n  gitlab.com:\n    token: x\n    api_host: gitlab.com\n  git.acme.org:\n    token: y\nother: 1\n";
        assert_eq!(yaml_hosts(glab, false), ["gitlab.com", "git.acme.org"]);
    }
}
