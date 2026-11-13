//! Persisted window geometry + navigation-pane widths, restored on launch.
//!
//! Window position/size and the sidebar split are *state* rather than
//! configuration — per-machine UI layout that should survive a restart but
//! isn't precious user data — so each OS gets its conventional state location
//! rather than the XDG-everywhere path `config.rs` / `palette.rs` use:
//!   * Linux:   `$XDG_STATE_HOME/diffui/window.toml` (else `~/.local/state/...`)
//!   * macOS:   `~/Library/Application Support/diffui/window.toml`
//!   * Windows: `%LOCALAPPDATA%\diffui\window.toml`   (else `%APPDATA%\...`)
//!
//! Persistence is best-effort, mirroring `Recents`: any I/O failure degrades
//! to defaults, and a parse failure says so on stderr before it does. The file
//! now carries the session (open repos, the active one, per-repo revsets and
//! recents), so losing it silently would look like the app forgot which
//! repositories were open — but it still never blocks the UI.

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
/// logical pixels; the type is intentionally UI-free (plain `f32` / `String`)
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
    /// The display `x` and `y` are relative to, by UUID, on platforms that report
    /// a window's position per display. `None` means the primary one, or global
    /// coordinates where that's what the platform reports.
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Last expanded width of the changed-files tree.
    #[serde(default)]
    pub file_nav_width: Option<f32>,
    /// Per-repository panel visibility. Maps keep the layout each tab had
    /// instead of forcing every restored repository into the active tab's
    /// arrangement.
    #[serde(default)]
    pub history_panel_collapsed: BTreeMap<String, bool>,
    #[serde(default)]
    pub files_panel_collapsed: BTreeMap<String, bool>,
    /// Whether the diff pane wraps long lines. `None` (older state files)
    /// falls back to wrapping on.
    #[serde(default)]
    pub diff_wrap: Option<bool>,
    /// Whether the diff pane uses the side-by-side layout. `None` falls back
    /// to the unified view.
    #[serde(default)]
    pub diff_split: Option<bool>,
    /// Whether the diff header is folded to its title and metadata line.
    /// `None` falls back to unfolded.
    #[serde(default)]
    pub commit_details_collapsed: Option<bool>,
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

/// Each repository's filter and panel layout, kept for the ones that aren't
/// open too, so one reopens the way it was closed.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepoLayouts {
    pub(crate) revsets: BTreeMap<String, String>,
    pub(crate) history_panel_collapsed: BTreeMap<String, bool>,
    pub(crate) files_panel_collapsed: BTreeMap<String, bool>,
}

impl RepoLayouts {
    pub(crate) fn from_saved(saved: &WindowState) -> Self {
        Self {
            revsets: saved.revsets.clone(),
            history_panel_collapsed: saved.history_panel_collapsed.clone(),
            files_panel_collapsed: saved.files_panel_collapsed.clone(),
        }
    }

    /// A never-loaded tab for `repository`, laid out as it was last left.
    pub(crate) fn tab_state(&self, repository: crate::Repository) -> crate::TabState {
        let key = repository.root.to_string_lossy().into_owned();
        let revset = self
            .revsets
            .get(&key)
            .filter(|value| !value.is_empty())
            .cloned();
        let mut state = crate::TabState::unloaded(Some(repository), revset);
        state.history_panel_collapsed = self.history_panel_collapsed.get(&key) == Some(&true);
        state.files_panel_collapsed = self.files_panel_collapsed.get(&key) == Some(&true);
        state
    }

    /// Record how `tab` is laid out now. PR tabs have no root to key it by.
    pub(crate) fn remember(&mut self, tab: &crate::Tab) {
        let Some(root) = tab.root() else {
            return;
        };
        let key = root.to_string_lossy().into_owned();
        let state = &tab.state;
        if state.session.revset.is_empty() {
            self.revsets.remove(&key);
        } else {
            self.revsets
                .insert(key.clone(), state.session.revset.clone());
        }
        self.history_panel_collapsed
            .insert(key.clone(), state.history_panel_collapsed);
        self.files_panel_collapsed
            .insert(key, state.files_panel_collapsed);
    }

    /// Forget the repositories `keep` doesn't name.
    pub(crate) fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.revsets.retain(|root, _| keep(root));
        self.history_panel_collapsed.retain(|root, _| keep(root));
        self.files_panel_collapsed.retain(|root, _| keep(root));
    }
}

impl WindowState {
    /// Load persisted state. A missing file or unparseable contents both yield
    /// `WindowState::default()` (everything `None`); the unparseable case is
    /// reported first, because it drops the whole session with it and the user
    /// would otherwise only see the tabs silently gone.
    pub fn load() -> Self {
        let Some(path) = state_path() else {
            return Self::default();
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&raw) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    "ignoring unreadable window state: {error}"
                );
                Self::default()
            }
        }
    }

    /// Write the current state to disk, creating the directory if needed.
    /// Best-effort: errors are dropped.
    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        let Ok(raw) = toml::to_string(self) else {
            return;
        };
        // Atomic: the session lives here, so a torn write would cost the user
        // their open tabs, not just the geometry.
        let _ = crate::app_files::write_atomic(&path, raw.as_bytes());
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

/// Share of the screen's usable area a first-launch window takes.
const DEFAULT_SCREEN_FRACTION: f32 = 0.85;
/// Cap on a first-launch window, so a big display doesn't open a wall of
/// whitespace.
const DEFAULT_MAX_SIZE: (f32, f32) = (1800.0, 1150.0);

/// The window size for a launch with no saved geometry: most of the screen's
/// usable area (`visible`, logical points, menu bar and Dock excluded), up to
/// a cap. A fixed 1024-wide default would leave a three-pane split view too
/// narrow to read. `None` without a screen to measure.
pub fn default_window_size(visible: Option<(f32, f32)>) -> Option<(f32, f32)> {
    let (width, height) = visible.filter(|(w, h)| {
        w.is_finite() && h.is_finite() && *w >= MIN_RESTORE_DIMENSION && *h >= MIN_RESTORE_DIMENSION
    })?;
    Some((
        (width * DEFAULT_SCREEN_FRACTION)
            .min(DEFAULT_MAX_SIZE.0)
            .round(),
        (height * DEFAULT_SCREEN_FRACTION)
            .min(DEFAULT_MAX_SIZE.1)
            .round(),
    ))
}

fn state_path() -> Option<PathBuf> {
    crate::app_files::app_file(crate::app_files::AppDir::State, "window.toml")
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
            display: Some("37D8832A-2D66-02CA-B9F7-8F30A301B230".to_owned()),
            sidebar_width: Some(280.0),
            file_nav_width: Some(220.0),
            history_panel_collapsed: BTreeMap::from([
                ("/a/repo".to_owned(), true),
                ("/b/repo".to_owned(), false),
            ]),
            files_panel_collapsed: BTreeMap::from([("/b/repo".to_owned(), true)]),
            diff_wrap: Some(false),
            diff_split: Some(true),
            commit_details_collapsed: Some(true),
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
        assert_eq!(parsed.display, state.display);
        assert_eq!(parsed.sidebar_width, Some(280.0));
        assert_eq!(parsed.file_nav_width, Some(220.0));
        assert_eq!(parsed.history_panel_collapsed.get("/a/repo"), Some(&true));
        assert_eq!(parsed.files_panel_collapsed.get("/b/repo"), Some(&true));
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
        assert_eq!(empty.file_nav_width, None);
        assert!(empty.history_panel_collapsed.is_empty());
        assert!(empty.files_panel_collapsed.is_empty());

        // A file with only the sidebar width keeps it and leaves geometry unset.
        let partial: WindowState =
            toml::from_str("sidebar_width = 300.0").expect("partial deserializes");
        assert_eq!(partial.sidebar_width, Some(300.0));
        assert_eq!(partial.size(), None);
    }

    #[test]
    fn a_first_launch_window_takes_most_of_the_screen_up_to_a_cap() {
        assert_eq!(
            default_window_size(Some((1440.0, 875.0))),
            Some((1224.0, 744.0))
        );
        assert_eq!(
            default_window_size(Some((3008.0, 1667.0))),
            Some((1800.0, 1150.0))
        );
        assert_eq!(default_window_size(None), None);
        assert_eq!(default_window_size(Some((f32::NAN, 800.0))), None);
    }
}
