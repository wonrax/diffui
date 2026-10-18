//! Warn when the `jj` CLI on PATH is older than the jj-lib diffui embeds.
//!
//! Both write to the same `.jj` directory. jj migrates repository storage
//! forward (0.45 changed how colocated repos record git HEAD), and an older CLI
//! reading what a newer library wrote is untested territory, so the first
//! jj repository opened per launch checks `jj --version` once, off the UI
//! thread, and says so if the CLI is behind.

use std::sync::atomic::{AtomicBool, Ordering};

use iced::Task;

use crate::Message;

/// The jj-lib version Cargo.lock resolved (see `build.rs`).
const EMBEDDED: &str = env!("DIFFUI_JJ_LIB_VERSION");

static CHECKED: AtomicBool = AtomicBool::new(false);

/// Start the check if it hasn't run this launch.
pub(crate) fn check_once() -> Task<Message> {
    if CHECKED.swap(true, Ordering::Relaxed) {
        return Task::none();
    }
    Task::perform(cli_version(), Message::JjCliVersion)
}

/// `jj --version`'s version, or `None` when there is no `jj` on PATH (or it
/// didn't answer) — nothing to be out of step with then.
async fn cli_version() -> Option<String> {
    let output = tokio::process::Command::new("jj")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    // `jj 0.41.0`, or `jj 0.42.0-<commit>` for a dev build.
    Some(text.split_whitespace().nth(1)?.to_owned())
}

/// The warning to show for a CLI at `cli`, if it is older than the library.
pub(crate) fn skew_warning(cli: &str) -> Option<String> {
    is_older(cli, EMBEDDED).then(|| {
        format!(
            "Your jj CLI is {cli} but diffui uses jj {EMBEDDED}; upgrade jj before \
             using both on the same repository."
        )
    })
}

fn is_older(cli: &str, embedded: &str) -> bool {
    match (parse(cli), parse(embedded)) {
        (Some(cli), Some(embedded)) => cli < embedded,
        _ => false,
    }
}

/// `major.minor.patch`, ignoring any `-suffix`.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

impl crate::Diffui {
    pub(crate) fn on_jj_cli_version(&mut self, version: Option<String>) -> Task<Message> {
        let Some(version) = version else {
            tracing::info!("no jj CLI on PATH; jj-lib {EMBEDDED} embedded");
            return Task::none();
        };
        match skew_warning(&version) {
            Some(warning) => {
                tracing::warn!("{warning}");
                self.push_sticky_toast("jj version mismatch".to_owned(), &warning);
            }
            None => tracing::info!("jj CLI {version}, jj-lib {EMBEDDED} embedded"),
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_version_was_found() {
        assert!(parse(EMBEDDED).is_some(), "build.rs embedded {EMBEDDED:?}");
    }

    #[test]
    fn only_an_older_cli_warns() {
        assert!(is_older("0.41.0", "0.45.1"));
        assert!(is_older("0.45.0", "0.45.1"));
        assert!(!is_older("0.45.1", "0.45.1"));
        assert!(!is_older("0.46.0-abcdef", "0.45.1"));
        assert!(is_older("0.44.2-abcdef", "0.45.1"));
        // Unparseable on either side: say nothing rather than guess.
        assert!(!is_older("nightly", "0.45.1"));
        assert!(!is_older("0.41.0", ""));
    }
}
