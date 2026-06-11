//! Persisted window geometry + sidebar width, restored on the next launch.
//!
//! Window position/size and the sidebar split are *state* rather than
//! configuration — per-machine UI layout that should survive a restart but
//! isn't precious user data — so each OS gets its conventional state location
//! rather than the XDG-everywhere path `config.rs` / `palette.rs` use:
//!   * Linux:   `$XDG_STATE_HOME/diffui/window.toml` (else `~/.local/state/...`)
//!   * macOS:   `~/Library/Application Support/diffui/window.toml`
//!   * Windows: `%LOCALAPPDATA%\diffui\window.toml`   (else `%APPDATA%\...`)
//!
//! Persistence is best-effort, mirroring `Recents`: any I/O or parse failure
//! silently degrades to defaults. Geometry is convenience state, never
//! load-bearing, so we never surface an error or block the UI on it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The smallest restored window we'll trust. A file with degenerate or
/// corrupt dimensions falls back to the platform default rather than opening
/// an unusable sliver of a window.
const MIN_RESTORE_DIMENSION: f32 = 100.0;

/// Window geometry, sidebar split, and the open-repo session persisted between
/// runs. Every field is optional / defaulted so a partial or older file still
/// loads — a missing field just falls back to the in-app default. Geometry is
/// logical pixels; the type is intentionally iced-free (plain `f32` / `String`)
/// so persistence stays decoupled from the UI layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowState {
    #[serde(default)]
    pub width: Option<f32>,
    #[serde(default)]
    pub height: Option<f32>,
    /// Outer top-left, relative to the desktop. `None` on first run and on
    /// platforms that don't report a position (e.g. Wayland), where the
    /// platform places the window instead.
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Whether the diff pane wraps long lines. `None` (older state files)
    /// falls back to wrapping on.
    #[serde(default)]
    pub diff_wrap: Option<bool>,
    /// Whether the diff pane uses the side-by-side layout. `None` falls back
    /// to the unified view.
    #[serde(default)]
    pub diff_split: Option<bool>,
    /// Repository roots that were open as tabs, in tab order. Restored on the
    /// next launch when no repositories are given on the command line.
    #[serde(default)]
    pub open_repos: Vec<String>,
    /// Root of the tab that was active, so it's re-focused on restore. `None`
    /// (or an unresolvable path) falls back to the first tab.
    #[serde(default)]
    pub active_repo: Option<String>,
    /// Per-repository revset (jj) / revision-range (git), keyed by repo root.
    /// Restored so each repo reopens with the filter the user last set.
    #[serde(default)]
    pub revsets: BTreeMap<String, String>,
    /// Most-recently-opened repository roots, newest first. Offered as
    /// quick-pick rows in the "Open repository" dialog so a closed repo is one
    /// click to reopen. Distinct from `open_repos` (which is only what's open
    /// *right now*); this remembers history across closes.
    #[serde(default)]
    pub recent_repos: Vec<String>,
}

impl WindowState {
    /// Load persisted state. A missing file or unparseable contents both yield
    /// `WindowState::default()` (everything `None`).
    pub fn load() -> Self {
        state_path()
            .and_then(|path| std::fs::read_to_string(&path).ok())
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Write the current state to disk, creating the directory if needed.
    /// Best-effort: errors are dropped.
    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = toml::to_string(self) {
            let _ = std::fs::write(&path, raw);
        }
    }

    /// Validated logical size, or `None` if absent or implausible (a corrupt
    /// file, NaN, or a degenerate dimension). Used to seed the initial window
    /// size; callers fall back to the platform default.
    pub fn size(&self) -> Option<(f32, f32)> {
        match (self.width, self.height) {
            (Some(w), Some(h))
                if w.is_finite()
                    && h.is_finite()
                    && w >= MIN_RESTORE_DIMENSION
                    && h >= MIN_RESTORE_DIMENSION =>
            {
                Some((w, h))
            }
            _ => None,
        }
    }

    /// Validated outer top-left, or `None` if absent or non-finite. Off-screen
    /// coordinates are left to the windowing system to constrain (macOS and
    /// Windows both pull stray windows back onto a visible monitor).
    pub fn position(&self) -> Option<(f32, f32)> {
        match (self.x, self.y) {
            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
            _ => None,
        }
    }
}

/// Resolve the per-platform state file path. Uses runtime `cfg!` branches
/// (not `#[cfg]`) so every platform's path logic is type-checked in every
/// build; the untaken branches are dead but harmless.
fn state_path() -> Option<PathBuf> {
    use std::env;

    let dir = if cfg!(windows) {
        // Per-machine, non-roaming state: window coordinates shouldn't roam to
        // a machine with a different monitor layout. Fall back to roaming.
        env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .map(PathBuf::from)?
    } else if cfg!(target_os = "macos") {
        PathBuf::from(env::var_os("HOME")?).join("Library/Application Support")
    } else {
        // Linux / other unixes: the XDG state dir, else its spec'd default.
        env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?
    };

    Some(dir.join("diffui").join("window.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_validates_dimensions() {
        // Both present and plausible.
        assert_eq!(
            WindowState {
                width: Some(1200.0),
                height: Some(800.0),
                ..Default::default()
            }
            .size(),
            Some((1200.0, 800.0))
        );
        // A missing dimension, a degenerate one, and NaN all fall back.
        assert_eq!(
            WindowState {
                width: Some(1200.0),
                ..Default::default()
            }
            .size(),
            None
        );
        assert_eq!(
            WindowState {
                width: Some(10.0),
                height: Some(800.0),
                ..Default::default()
            }
            .size(),
            None
        );
        assert_eq!(
            WindowState {
                width: Some(f32::NAN),
                height: Some(800.0),
                ..Default::default()
            }
            .size(),
            None
        );
    }

    #[test]
    fn position_allows_off_screen_but_rejects_non_finite() {
        // Negative coordinates are valid (multi-monitor / a monitor left of
        // the primary); the windowing system constrains stray windows.
        assert_eq!(
            WindowState {
                x: Some(-50.0),
                y: Some(10.0),
                ..Default::default()
            }
            .position(),
            Some((-50.0, 10.0))
        );
        assert_eq!(
            WindowState {
                x: Some(f32::INFINITY),
                y: Some(10.0),
                ..Default::default()
            }
            .position(),
            None
        );
    }

    #[test]
    fn toml_round_trip_preserves_values() {
        let state = WindowState {
            width: Some(1200.0),
            height: Some(800.0),
            x: Some(100.0),
            y: Some(50.0),
            sidebar_width: Some(280.0),
            diff_wrap: Some(false),
            diff_split: Some(true),
            open_repos: vec!["/a/repo".to_owned(), "/b/repo".to_owned()],
            active_repo: Some("/b/repo".to_owned()),
            revsets: BTreeMap::from([
                ("/a/repo".to_owned(), "all()".to_owned()),
                ("/b/repo".to_owned(), "mine()".to_owned()),
            ]),
            recent_repos: vec!["/b/repo".to_owned(), "/a/repo".to_owned()],
        };
        let raw = toml::to_string(&state).expect("serialize");
        let parsed: WindowState = toml::from_str(&raw).expect("deserialize");
        assert_eq!(parsed.size(), Some((1200.0, 800.0)));
        assert_eq!(parsed.position(), Some((100.0, 50.0)));
        assert_eq!(parsed.sidebar_width, Some(280.0));
        assert_eq!(parsed.diff_wrap, Some(false));
        assert_eq!(parsed.diff_split, Some(true));
        assert_eq!(
            parsed.open_repos,
            vec!["/a/repo".to_owned(), "/b/repo".to_owned()]
        );
        assert_eq!(parsed.active_repo.as_deref(), Some("/b/repo"));
        assert_eq!(
            parsed.revsets.get("/b/repo").map(String::as_str),
            Some("mine()")
        );
        assert_eq!(
            parsed.recent_repos,
            vec!["/b/repo".to_owned(), "/a/repo".to_owned()]
        );
    }

    #[test]
    fn partial_and_empty_toml_default_missing_fields() {
        // An empty file (everything defaulted) must load, not error — older or
        // truncated files shouldn't wipe the launch.
        let empty: WindowState = toml::from_str("").expect("empty deserializes");
        assert_eq!(empty.size(), None);
        assert_eq!(empty.position(), None);
        assert_eq!(empty.sidebar_width, None);

        // A file with only the sidebar width keeps it and leaves geometry unset.
        let partial: WindowState =
            toml::from_str("sidebar_width = 300.0").expect("partial deserializes");
        assert_eq!(partial.sidebar_width, Some(300.0));
        assert_eq!(partial.size(), None);
    }
}
