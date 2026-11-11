//! Warn when the `jj` CLI on PATH is a different minor version from the
//! jj-lib diffui embeds.
//!
//! Both write to the same `.jj` directory, and jj migrates repository storage
//! forward (0.45 changed how colocated repos record git HEAD). Neither side
//! promises to read what the other minor version wrote: an older CLI meets
//! storage diffui upgraded, a newer one upgrades storage diffui then reads. So
//! the first jj repository opened per launch checks `jj --version` once, off
//! the UI thread, and says so if the minor versions differ.

use crate::Cx;
use std::cmp::Ordering;
use std::sync::atomic::{self, AtomicBool};

use crate::Diffui;

/// The jj-lib version Cargo.lock resolved (see `build.rs`).
const EMBEDDED: &str = env!("DIFFUI_JJ_LIB_VERSION");

static CHECKED: AtomicBool = AtomicBool::new(false);

/// Start the check if it hasn't run this launch.
pub(crate) fn check_once(cx: &mut Cx) {
    if CHECKED.swap(true, atomic::Ordering::Relaxed) {
        return;
    }
    Diffui::run_async(cli_version(), cx, |this, version, _| {
        this.on_jj_cli_version(version);
    });
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

/// The warning to show for a CLI at `cli`, if its minor version isn't the
/// library's.
pub(crate) fn skew_warning(cli: &str) -> Option<String> {
    match skew(cli, EMBEDDED)? {
        Ordering::Less => Some(format!(
            "Your jj CLI is {cli} but diffui uses jj {EMBEDDED}; upgrade jj before \
             using both on the same repository."
        )),
        Ordering::Greater => Some(format!(
            "Your jj CLI is {cli} but diffui uses jj {EMBEDDED}; the newer jj can \
             upgrade repository storage diffui can't read, so don't use both on the same \
             repository."
        )),
        Ordering::Equal => None,
    }
}

/// How the CLI's `major.minor` compares with the library's; `None` when
/// either doesn't parse. Patch releases don't change storage.
fn skew(cli: &str, embedded: &str) -> Option<Ordering> {
    let (cli, embedded) = (parse(cli)?, parse(embedded)?);
    Some((cli.0, cli.1).cmp(&(embedded.0, embedded.1)))
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
    pub(crate) fn on_jj_cli_version(&mut self, version: Option<String>) {
        let Some(version) = version else {
            tracing::info!("no jj CLI on PATH; jj-lib {EMBEDDED} embedded");
            return;
        };
        match skew_warning(&version) {
            Some(warning) => {
                tracing::warn!("{warning}");
                self.push_sticky_toast("jj version mismatch".to_owned(), &warning);
            }
            None => tracing::info!("jj CLI {version}, jj-lib {EMBEDDED} embedded"),
        }
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
    fn any_minor_skew_warns() {
        assert_eq!(skew("0.41.0", "0.45.1"), Some(Ordering::Less));
        assert_eq!(skew("0.44.2-abcdef", "0.45.1"), Some(Ordering::Less));
        assert_eq!(skew("0.46.0-abcdef", "0.45.1"), Some(Ordering::Greater));
        assert_eq!(skew("1.45.1", "0.45.1"), Some(Ordering::Greater));
        // A patch release apart is the same storage.
        assert_eq!(skew("0.45.0", "0.45.1"), Some(Ordering::Equal));
        assert_eq!(skew("0.45.1", "0.45.1"), Some(Ordering::Equal));
        // Unparseable on either side: say nothing rather than guess.
        assert_eq!(skew("nightly", "0.45.1"), None);
        assert_eq!(skew("0.41.0", ""), None);
    }
}
