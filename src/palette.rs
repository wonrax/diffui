//! Unified command palette / fuzzy navigator. Single overlay reached by
//! ⌘K / Ctrl+K. Mode prefixes:
//!
//!   *(empty)* — mixed search across revisions, files, and commands
//!   `>`       — commands only
//!   `@`       — files in the current revision only
//!
//! `Tab` on a highlighted result pushes a contextual "actions" column to the
//! right; the previous column slides left so the newly focused one stays
//! centered. `Esc` or `Backspace` (at empty input) pops the rightmost column;
//! `Esc` on the root column closes the palette.
//!
//! Ranking is `nucleo` fuzzy score plus a tapered recency bonus for items
//! the user has recently jumped to / commands they recently ran. Recents
//! are kept in-memory in `Recents` and persisted to the XDG data dir.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use iced::advanced::text::{Ellipsis, Wrapping};
use iced::{
    Animation, Background, Border, Color, Element, Length, Padding, Shadow, Vector, alignment,
    animation::Easing,
    border,
    font::Weight,
    widget::{
        Space, column, container, mouse_area, opaque, pin, responsive, row, scrollable,
        scrollable::{Direction, Scrollbar},
        stack, text, text_input,
    },
};
use nucleo_matcher::{Config, Matcher, Utf32String};

use crate::icons;
use crate::theme::{self, ThemeSpec};
use crate::{Diffui, Message};
use diffui_core::RevisionSelection;

/// Messages from the command palette, nested under [`Message::Palette`].
#[derive(Debug, Clone)]
pub enum PaletteMessage {
    Open,
    Close,
    QueryChanged(String),
    /// `(column depth, query version)` — drops a recompute the user typed past.
    Recompute(usize, u64),
    /// Move the highlighted result by `±1`.
    MoveSelection(i32),
    /// Set the highlighted index (used by hover).
    SelectIndex(usize),
    /// Enter / on_submit: act on the highlighted row.
    Accept,
    /// Click a specific row — explicit index against re-render races.
    AcceptIndex(usize),
    /// Tab: push an actions column for the highlighted result.
    PushActions,
    /// Esc / Backspace at empty: pop the rightmost column.
    PopColumn,
    /// Captured-but-inert (e.g. scroll ticks on the palette scrim).
    NoOp,
    /// Per-frame tick driving the column push/pop slide animation.
    Tick,
}

/// `Id` for the active palette text input. Refocused whenever the
/// rightmost column changes (open, push, pop) so keystrokes always land in
/// the column the user is steering.
pub const PALETTE_INPUT_ID: &str = "palette-input";

/// Build the scrollable id for column at `depth`. Each column gets its
/// own id so `scroll_to(...)` only operates on the targeted one — iced
/// routes widget operations by id, so reusing the same id across multiple
/// scrollables would non-deterministically scroll any of them.
pub fn results_scrollable_id(depth: usize) -> String {
    format!("palette-results-{depth}")
}

/// Approximate viewport height of a column's result list. Used to decide
/// when keyboard navigation has pushed the highlighted row out of view.
/// Doesn't need to be exact — the worst case of a too-small estimate is
/// an extra scroll-to that doesn't move the offset, which is invisible to
/// the user. Computed as: column height − header − input − results-pane
/// padding (top + bottom).
const RESULTS_VIEWPORT_HEIGHT: f32 = COLUMN_HEIGHT - 36.0 - INPUT_HEIGHT - 8.0 - 8.0;

/// Fixed width per column. The stack lays columns out in a horizontal row;
/// the rightmost is centered in the viewport and earlier columns shift left,
/// clipping off the leading edge once the stack is deep enough.
const COLUMN_WIDTH: f32 = 520.0;
const COLUMN_HEIGHT: f32 = 460.0;
const COLUMN_HORIZONTAL_GAP: f32 = 16.0;
/// Top offset of the palette stack from the viewport top. Anchoring near
/// the top (rather than centered) mirrors VS Code / Raycast and keeps the
/// input at a predictable Y as columns push/pop.
const TOP_OFFSET: f32 = 96.0;
const INPUT_HEIGHT: f32 = 48.0;
const ROW_HEIGHT: f32 = 34.0;

/// Cap on rows rendered per column. Beyond this the column would still
/// scroll, but for big repos (10k+ commits) capping keeps the layout pass
/// cheap and the user can refine the query to surface what they want.
const MAX_RESULT_ROWS: usize = 200;

const RECENTS_CAPACITY: usize = 32;
const RECENCY_TOP_N: usize = 5;
const RECENCY_PEAK_BONUS: u32 = 150;

/// Duration of the column push/pop slide. Short enough that the user
/// isn't waiting on the animation; long enough that it reads as motion
/// rather than a pop-in.
const COLUMN_SLIDE_DURATION: Duration = Duration::from_millis(140);
/// Easing curve for the slide. `EaseOutExpo` does most of its travel in
/// the first ~25% of the duration, so the column appears to *whip* into
/// place and gently settles — the snappy feel you want for "navigation
/// happened, now show me where I am". `EaseInOut` (iced's default) is
/// the opposite: slow start, slow end, which reads as sluggish even at
/// short durations.
const COLUMN_SLIDE_EASING: Easing = Easing::EaseOutExpo;

#[derive(Debug, Clone)]
pub struct PaletteState {
    /// Stack of open columns. Always non-empty when the palette is "open" —
    /// the wrapping `Option<PaletteState>` on `Diffui` carries the
    /// open/closed bit.
    pub stack: Vec<PaletteColumn>,
    /// Animation knob for column push/pop transitions. Carries an
    /// "extra" horizontal offset (px) that's added to the right-padding
    /// of the column row at render time and animates to 0.
    ///
    /// On push, we initialize it to `+(COL + GAP)` so the new column
    /// starts visually shifted left of center (still off-screen on the
    /// right) and slides into place. On pop, we initialize to
    /// `-(COL + GAP)` so the remaining columns start shifted right of
    /// their final position and slide left to fill the now-vacant
    /// rightmost slot. Animation is driven by a `PaletteTick`
    /// subscription that only runs while `is_animating()` is true.
    pub shift_anim: Animation<f32>,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            shift_anim: build_shift_anim(0.0),
        }
    }
}

/// Construct a fresh `shift_anim` with our shared duration + easing.
/// Centralized so push/pop/open use the same curve and only the initial
/// offset varies.
fn build_shift_anim(initial: f32) -> Animation<f32> {
    Animation::new(initial)
        .duration(COLUMN_SLIDE_DURATION)
        .easing(COLUMN_SLIDE_EASING)
}

#[derive(Debug, Clone)]
pub struct PaletteColumn {
    pub query: String,
    pub source: ColumnSource,
    pub matches: Vec<PaletteMatch>,
    pub selected: usize,
    /// Cached Y offset of this column's result scrollable. Tracked
    /// app-side because iced's `scrollable::scroll_to` operation needs an
    /// absolute target — we can't ask the widget "where are you now?"
    /// from outside, so we maintain our own approximation and update it
    /// whenever a keyboard move pushes the selected row out of the
    /// visible window.
    pub scroll_y: f32,
    /// Bumped on every query edit; the debounced recompute carries the value
    /// it was scheduled for and is dropped if the user has typed past it.
    pub query_version: u64,
    /// True while a query edit is awaiting its debounced recompute — drives
    /// the palette's "searching…" hint.
    pub dirty: bool,
    /// `:` commit-search only: whether the (deferred) commit scan has run for
    /// the current query. The first ⏎ runs the scan and sets this; editing the
    /// query resets it so the "press ⏎ to search" prompt comes back. Ignored in
    /// other modes (they search live).
    pub searched: bool,
}

#[derive(Debug, Clone)]
pub enum ColumnSource {
    /// Top-level search; mode is derived from the query prefix.
    Root,
    /// Action menu for a specific result. Renders a fixed catalog instead
    /// of fuzzy-matching against repo data.
    Actions(ResultRef),
}

/// A jj change id (the stable k–z identifier) — **not** a commit id. Wrapped
/// so it can't silently flow into `RevisionSelection::Commit`, which carries
/// commit-id hex: a change id passed there targets the wrong (usually no)
/// revision. Resolve through the commit store first — see
/// [`revision_selection`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeId(pub String);

impl ChangeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResultRef {
    /// `@` / working copy. Modelled separately from `Commit(_)` because it
    /// doesn't have a stable change-id.
    WorkingCopy,
    /// A revision, identified by its change-id.
    Commit(ChangeId),
    /// A bookmark, identified by its name. Resolves to the revision that
    /// owns it at accept time so the rest of the action pipeline can
    /// treat it as a regular revision jump.
    Bookmark(String),
    /// A file, identified by its path within the current revision's diff.
    File(String),
    Command(CommandId),
}

#[derive(Debug, Clone)]
pub struct PaletteMatch {
    pub item: ResultRef,
    pub score: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    RefreshRepository,
    SelectNextFile,
    SelectPreviousFile,
    ThemeSystem,
    ThemeLight,
    ThemeDark,
    ThemeHighContrast,
    CopyFileDiff,
    OpenFind,
    // Tab-action targets (filled per result type by `push_action_candidates`):
    JumpToRevision,
    CopyChangeId,
    CopyCommitMessage,
    CopyAuthor,
    OpenFile,
    CopyFilePath,
}

impl CommandId {
    pub fn label(self) -> &'static str {
        match self {
            Self::RefreshRepository => "Refresh repository",
            Self::SelectNextFile => "Select next file",
            Self::SelectPreviousFile => "Select previous file",
            Self::ThemeSystem => "Theme: System",
            Self::ThemeLight => "Theme: Light",
            Self::ThemeDark => "Theme: Dark",
            Self::ThemeHighContrast => "Theme: High contrast",
            Self::CopyFileDiff => "Copy current file diff",
            Self::OpenFind => "Find in current diff",
            Self::JumpToRevision => "Jump to revision",
            Self::CopyChangeId => "Copy change-id",
            Self::CopyCommitMessage => "Copy commit message",
            Self::CopyAuthor => "Copy author",
            Self::OpenFile => "Open file",
            Self::CopyFilePath => "Copy file path",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::RefreshRepository => "Re-read the repository state",
            Self::SelectNextFile | Self::SelectPreviousFile => "Move between files in current diff",
            Self::ThemeSystem => "Follow OS appearance",
            Self::ThemeLight | Self::ThemeDark | Self::ThemeHighContrast => "Set palette theme",
            Self::CopyFileDiff => "Copy the selected file's diff text",
            Self::OpenFind => "⌘F-style in-diff search across all files",
            Self::JumpToRevision => "Show this revision in the diff view",
            Self::CopyChangeId => "Copy the revision's change-id",
            Self::CopyCommitMessage => "Copy the commit message",
            Self::CopyAuthor => "Copy author name and email",
            Self::OpenFile => "Scroll the diff to this file",
            Self::CopyFilePath => "Copy the file's path",
        }
    }
}

/// Top-level commands shown when the user enters the palette with no
/// context. Tab-action commands are appended only inside `Actions(_)`
/// columns by `push_action_candidates`.
const ROOT_COMMANDS: &[CommandId] = &[
    CommandId::RefreshRepository,
    CommandId::OpenFind,
    CommandId::SelectNextFile,
    CommandId::SelectPreviousFile,
    CommandId::ThemeSystem,
    CommandId::ThemeDark,
    CommandId::ThemeLight,
    CommandId::ThemeHighContrast,
    CommandId::CopyFileDiff,
];

/// In-memory MRU bookkeeping. Two tracks: recently-jumped revisions and
/// recently-run commands. Surfaces a tapered score bump in mixed-mode
/// ranking so muscle-memory navigation stays predictable.
///
/// Persisted to `$XDG_DATA_HOME/diffui/recents.toml` (or
/// `$HOME/.local/share/diffui/recents.toml`). Persistence is best-effort —
/// any I/O failure is silently dropped; recents are convenience state, not
/// data we should ever block the UI on.
#[derive(Debug, Clone, Default)]
pub struct Recents {
    pub revisions: VecDeque<String>,
    pub commands: VecDeque<CommandId>,
}

impl Recents {
    pub fn push_revision(&mut self, change_id: String) {
        self.revisions.retain(|c| c != &change_id);
        self.revisions.push_front(change_id);
        if self.revisions.len() > RECENTS_CAPACITY {
            self.revisions.truncate(RECENTS_CAPACITY);
        }
    }

    pub fn push_command(&mut self, command: CommandId) {
        self.commands.retain(|c| *c != command);
        self.commands.push_front(command);
        if self.commands.len() > RECENTS_CAPACITY {
            self.commands.truncate(RECENTS_CAPACITY);
        }
    }

    pub fn revision_bonus(&self, change_id: &str) -> u32 {
        self.revisions
            .iter()
            .take(RECENCY_TOP_N)
            .position(|c| c == change_id)
            .map(rank_bonus)
            .unwrap_or(0)
    }

    pub fn command_bonus(&self, command: CommandId) -> u32 {
        self.commands
            .iter()
            .take(RECENCY_TOP_N)
            .position(|c| *c == command)
            .map(rank_bonus)
            .unwrap_or(0)
    }

    /// Load persisted recents from the XDG data dir. Missing files /
    /// unparseable contents both yield `Recents::default()` — recents are
    /// best-effort convenience state, never load-bearing.
    pub fn load() -> Self {
        recents_path()
            .and_then(|path| std::fs::read_to_string(&path).ok())
            .and_then(|raw| toml::from_str::<PersistedRecents>(&raw).ok())
            .map(Self::from_persisted)
            .unwrap_or_default()
    }

    /// Write the current recents to the XDG data dir, creating the
    /// directory if needed. Best-effort: errors are dropped.
    pub fn save(&self) {
        let Some(path) = recents_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let persisted = PersistedRecents::from(self);
        if let Ok(raw) = toml::to_string(&persisted) {
            let _ = std::fs::write(&path, raw);
        }
    }

    fn from_persisted(p: PersistedRecents) -> Self {
        let mut revisions: VecDeque<String> = p.revisions.into_iter().collect();
        let mut commands: VecDeque<CommandId> = p
            .commands
            .into_iter()
            .filter_map(|name| CommandId::from_persist_name(&name))
            .collect();
        if revisions.len() > RECENTS_CAPACITY {
            revisions.truncate(RECENTS_CAPACITY);
        }
        if commands.len() > RECENTS_CAPACITY {
            commands.truncate(RECENTS_CAPACITY);
        }
        Self {
            revisions,
            commands,
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedRecents {
    #[serde(default)]
    revisions: Vec<String>,
    #[serde(default)]
    commands: Vec<String>,
}

impl From<&Recents> for PersistedRecents {
    fn from(r: &Recents) -> Self {
        Self {
            revisions: r.revisions.iter().cloned().collect(),
            commands: r
                .commands
                .iter()
                .map(|c| c.persist_name().to_owned())
                .collect(),
        }
    }
}

impl CommandId {
    /// Stable string used in the on-disk recents file. Keep this separate
    /// from the user-visible `label()` so renaming a label doesn't
    /// invalidate persisted MRU entries.
    fn persist_name(self) -> &'static str {
        match self {
            Self::RefreshRepository => "refresh-repository",
            Self::SelectNextFile => "select-next-file",
            Self::SelectPreviousFile => "select-previous-file",
            Self::ThemeSystem => "theme-system",
            Self::ThemeLight => "theme-light",
            Self::ThemeDark => "theme-dark",
            Self::ThemeHighContrast => "theme-high-contrast",
            Self::CopyFileDiff => "copy-file-diff",
            Self::OpenFind => "open-find",
            Self::JumpToRevision => "jump-to-revision",
            Self::CopyChangeId => "copy-change-id",
            Self::CopyCommitMessage => "copy-commit-message",
            Self::CopyAuthor => "copy-author",
            Self::OpenFile => "open-file",
            Self::CopyFilePath => "copy-file-path",
        }
    }

    fn from_persist_name(name: &str) -> Option<Self> {
        Some(match name {
            "refresh-repository" => Self::RefreshRepository,
            "select-next-file" => Self::SelectNextFile,
            "select-previous-file" => Self::SelectPreviousFile,
            "theme-system" => Self::ThemeSystem,
            "theme-light" => Self::ThemeLight,
            "theme-dark" => Self::ThemeDark,
            "theme-high-contrast" => Self::ThemeHighContrast,
            "copy-file-diff" => Self::CopyFileDiff,
            "open-find" => Self::OpenFind,
            "jump-to-revision" => Self::JumpToRevision,
            "copy-change-id" => Self::CopyChangeId,
            "copy-commit-message" => Self::CopyCommitMessage,
            "copy-author" => Self::CopyAuthor,
            "open-file" => Self::OpenFile,
            "copy-file-path" => Self::CopyFilePath,
            _ => return None,
        })
    }
}

/// Path to the persisted recents file. Mirrors `config.rs`'s XDG handling
/// but resolves the *data* dir (`$XDG_DATA_HOME` → `$HOME/.local/share`).
fn recents_path() -> Option<std::path::PathBuf> {
    use std::env;
    use std::path::PathBuf;

    let base = if let Ok(xdg) = env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(env::var("HOME").ok()?)
            .join(".local")
            .join("share")
    };
    Some(base.join("diffui").join("recents.toml"))
}

fn rank_bonus(rank: usize) -> u32 {
    // Linear taper: rank 0 = peak, rank N-1 ≈ 0. Cheap and deterministic;
    // gives the most-recent item a clear lead without making rank-5+ feel
    // arbitrary.
    let denom = RECENCY_TOP_N.max(1) as u32;
    let step = RECENCY_PEAK_BONUS / denom;
    RECENCY_PEAK_BONUS.saturating_sub(rank as u32 * step)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Mixed,
    Commands,
    Files,
    /// Commit search (`:` prefix). Scanning all commits is `O(#commits)` and
    /// too slow to run per keystroke on a 1M-commit repo, so this mode defers
    /// the scan to ⏎ (see `recompute_matches` / `Diffui::palette_submit`).
    Revisions,
}

fn parse_query(raw: &str) -> (Mode, &str) {
    if let Some(rest) = raw.strip_prefix('>') {
        (Mode::Commands, rest.trim_start())
    } else if let Some(rest) = raw.strip_prefix('@') {
        (Mode::Files, rest.trim_start())
    } else if let Some(rest) = raw.strip_prefix(':') {
        (Mode::Revisions, rest.trim_start())
    } else {
        (Mode::Mixed, raw)
    }
}

/// The needle of a `:`-prefixed commit-search query, or `None` when the query
/// isn't in commit-search mode. `Some("")` means the user typed `:` with no
/// term yet. Lets `main.rs` and the results view branch on commit-search mode
/// without exposing the private `Mode`.
pub fn revision_mode_needle(query: &str) -> Option<&str> {
    match parse_query(query) {
        (Mode::Revisions, needle) => Some(needle),
        _ => None,
    }
}

impl PaletteColumn {
    /// Adjust `scroll_y` so the highlighted row is inside the visible
    /// window. Returns `true` when the offset actually changed — the
    /// caller uses that signal to decide whether to dispatch a
    /// `scroll_to` task. Mirrors the "keep cursor visible" math every
    /// editor uses: only scroll when the selection has left the window,
    /// snap just enough to bring it back.
    pub fn ensure_selected_visible(&mut self) -> bool {
        if self.matches.is_empty() {
            return false;
        }
        let row_top = self.selected as f32 * ROW_HEIGHT;
        let row_bottom = row_top + ROW_HEIGHT;
        let viewport = RESULTS_VIEWPORT_HEIGHT;
        let new_y = if row_top < self.scroll_y {
            row_top
        } else if row_bottom > self.scroll_y + viewport {
            row_bottom - viewport
        } else {
            self.scroll_y
        };
        let new_y = new_y.max(0.0);
        if (new_y - self.scroll_y).abs() > f32::EPSILON {
            self.scroll_y = new_y;
            true
        } else {
            false
        }
    }
}

impl PaletteState {
    pub fn open(ui: &Diffui) -> Self {
        let mut column = PaletteColumn {
            query: String::new(),
            source: ColumnSource::Root,
            matches: Vec::new(),
            selected: 0,
            scroll_y: 0.0,
            query_version: 0,
            dirty: false,
            searched: false,
        };
        recompute_matches(&mut column, ui, false);
        // The first column doesn't animate — it just appears. Push/pop
        // *between* columns is what justifies a transition; the
        // open-from-nothing case has no "from" state to slide out of, so
        // animating it just adds latency to the open keystroke.
        Self {
            stack: vec![column],
            shift_anim: build_shift_anim(0.0),
        }
    }

    pub fn top(&self) -> Option<&PaletteColumn> {
        self.stack.last()
    }

    pub fn top_mut(&mut self) -> Option<&mut PaletteColumn> {
        self.stack.last_mut()
    }

    /// Push an Actions column for the currently highlighted target. Returns
    /// `false` (and no-ops) when there's nothing to push — either no matches
    /// in the top column or the highlighted item is a Command (which is its
    /// own terminal action).
    pub fn push_actions(&mut self, ui: &Diffui) -> bool {
        let Some(top) = self.stack.last() else {
            return false;
        };
        let Some(target) = top.matches.get(top.selected).map(|m| m.item.clone()) else {
            return false;
        };
        if matches!(target, ResultRef::Command(_)) {
            return false;
        }
        let mut column = PaletteColumn {
            query: String::new(),
            source: ColumnSource::Actions(target),
            matches: Vec::new(),
            selected: 0,
            scroll_y: 0.0,
            query_version: 0,
            dirty: false,
            searched: false,
        };
        recompute_matches(&mut column, ui, false);
        self.stack.push(column);
        // Slide-in from the right: kick the offset out by one column
        // worth (so the new rightmost is initially off-center to the
        // right) and animate back to 0.
        self.shift_anim = build_shift_anim(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP);
        self.shift_anim.go_mut(0.0, Instant::now());
        true
    }

    /// Pop the rightmost column. Returns `true` if the palette should
    /// remain open (columns still on the stack), `false` when the user
    /// just closed the root column.
    pub fn pop(&mut self) -> bool {
        self.stack.pop();
        if !self.stack.is_empty() {
            // Slide-back: remaining columns start visually shifted right
            // (one slot past where they'll settle) and slide left into
            // place. The popped column is gone from state, so we can't
            // animate it sliding out — this is the cheapest illusion of
            // "the layout moved" available without a parallel ghost
            // column.
            self.shift_anim = build_shift_anim(-(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP));
            self.shift_anim.go_mut(0.0, Instant::now());
            true
        } else {
            false
        }
    }

    /// Whether the column-push/pop animation is still mid-transition.
    /// Drives the per-frame redraw subscription from `main.rs`.
    pub fn is_animating(&self, at: Instant) -> bool {
        self.shift_anim.is_animating(at)
    }
}

/// Re-run the fuzzy match against the column's current query.
///
/// `search_revisions` gates the expensive all-commits scan: the live
/// (debounced) keystroke path passes `false` so commit search never runs per
/// keystroke; the ⏎-triggered path (`Diffui::palette_submit`) passes `true`.
/// In `:` (commit) mode with `false`, candidates stay empty and the results
/// view shows the "press ⏎ to search" prompt. Other modes ignore the flag.
pub fn recompute_matches(column: &mut PaletteColumn, ui: &Diffui, search_revisions: bool) {
    let (mode, needle) = parse_query(&column.query);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle_utf32 = Utf32String::from(needle);

    let mut candidates: Vec<Candidate> = Vec::new();
    match &column.source {
        ColumnSource::Root => {
            if mode == Mode::Mixed || mode == Mode::Commands {
                push_command_candidates(&mut candidates);
            }
            if mode == Mode::Mixed {
                // Commits are NOT searched here — that's `:` mode (below), kept
                // off the keystroke path. Bookmarks/files stay live and cheap.
                push_bookmark_candidates(&mut candidates, ui);
                push_file_candidates(&mut candidates, ui);
            } else if mode == Mode::Files {
                push_file_candidates(&mut candidates, ui);
            } else if mode == Mode::Revisions && search_revisions {
                push_revision_candidates(&mut candidates, ui);
            }
        }
        ColumnSource::Actions(target) => {
            push_action_candidates(&mut candidates, target);
        }
    }

    let mut matches: Vec<PaletteMatch> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let score = if needle.is_empty() {
            Some(0u16)
        } else {
            let hay_utf32 = Utf32String::from(candidate.haystack.as_str());
            matcher.fuzzy_match(hay_utf32.slice(..), needle_utf32.slice(..))
        };
        let Some(raw_score) = score else { continue };

        let bonus = match &candidate.item {
            ResultRef::Commit(id) => ui.recents.revision_bonus(id.as_str()),
            ResultRef::Command(id) => ui.recents.command_bonus(*id),
            ResultRef::WorkingCopy | ResultRef::Bookmark(_) | ResultRef::File(_) => 0,
        };
        // Category gets a stable tie-break, but the fuzzy score is the
        // dominant term — multiply by a wide constant so a barely-better
        // match always wins regardless of category. Bookmarks rank
        // *above* regular revisions: when the user types a branch name
        // they want the bookmark row, not a commit message that happens
        // to mention the same word.
        let category_tiebreak = match &candidate.item {
            ResultRef::Bookmark(_) => 4,
            ResultRef::WorkingCopy | ResultRef::Commit(_) => 3,
            ResultRef::File(_) => 2,
            ResultRef::Command(_) => 1,
        };
        let total = (raw_score as u32 * 100) + bonus + category_tiebreak;
        matches.push(PaletteMatch {
            item: candidate.item,
            score: total,
        });
    }

    matches.sort_by(|a, b| b.score.cmp(&a.score));
    matches.truncate(MAX_RESULT_ROWS);

    column.matches = matches;
    column.selected = column.selected.min(column.matches.len().saturating_sub(1));
}

struct Candidate {
    item: ResultRef,
    /// String fed to nucleo. Usually a concatenation of all the user-visible
    /// identifiers for the row, so a query like "abc def" matches across
    /// change-id + description + author.
    haystack: String,
}

fn push_revision_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    out.push(Candidate {
        item: ResultRef::WorkingCopy,
        haystack: "working copy @".to_owned(),
    });
    for commit in ui.session.commits.iter() {
        let mut haystack = String::with_capacity(
            commit.change_id().len() + commit.description().len() + commit.author().len() + 32,
        );
        haystack.push_str(commit.change_id());
        haystack.push(' ');
        haystack.push_str(commit.commit_id());
        haystack.push(' ');
        if commit.has_description() {
            haystack.push_str(commit.description());
            haystack.push(' ');
        }
        haystack.push_str(commit.author());
        for bookmark in commit.bookmarks() {
            haystack.push(' ');
            haystack.push_str(bookmark);
        }
        out.push(Candidate {
            item: ResultRef::Commit(ChangeId(commit.change_id().to_owned())),
            haystack,
        });
    }
}

fn push_bookmark_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    // Emit one entry per bookmark. Bookmarks are sparse, so iterate the
    // bookmark index directly (`O(#bookmarks)`) rather than scanning all ~1M
    // commits — the latter is what made keystroke search 5fps. We deliberately
    // include even bookmarks that duplicate a revision row's haystack: they get
    // a category boost over the revision so an exact bookmark-name match floats
    // to the top, which is what the user wants when typing "main" or a feature
    // branch. Sorted by row so the empty-query order is stable (the index is a
    // HashMap).
    let mut rows: Vec<(usize, &[String])> = ui.session.commits.bookmarked_rows().collect();
    rows.sort_by_key(|(index, _)| *index);
    for (index, bookmarks) in rows {
        let commit = ui.session.commits.row(index);
        for bookmark in bookmarks {
            let mut haystack =
                String::with_capacity(bookmark.len() + commit.description().len() + 8);
            haystack.push_str(bookmark);
            haystack.push(' ');
            if commit.has_description() {
                haystack.push_str(commit.description());
            }
            out.push(Candidate {
                item: ResultRef::Bookmark(bookmark.clone()),
                haystack,
            });
        }
    }
}

fn push_file_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    for file in &ui.session.document.files {
        out.push(Candidate {
            item: ResultRef::File(file.path.clone()),
            haystack: file.path.clone(),
        });
    }
}

fn push_command_candidates(out: &mut Vec<Candidate>) {
    for cmd in ROOT_COMMANDS {
        out.push(Candidate {
            item: ResultRef::Command(*cmd),
            haystack: format!("{} {}", cmd.label(), cmd.hint()),
        });
    }
}

fn push_action_candidates(out: &mut Vec<Candidate>, target: &ResultRef) {
    let actions: &[CommandId] = match target {
        // Bookmarks share the revision action menu; the bookmark just
        // points at a commit, so "Copy change-id" / "Copy author" etc.
        // resolve through the owning revision.
        ResultRef::WorkingCopy | ResultRef::Commit(_) | ResultRef::Bookmark(_) => &[
            CommandId::JumpToRevision,
            CommandId::CopyChangeId,
            CommandId::CopyCommitMessage,
            CommandId::CopyAuthor,
        ],
        ResultRef::File(_) => &[CommandId::OpenFile, CommandId::CopyFilePath],
        ResultRef::Command(_) => &[],
    };
    for cmd in actions {
        out.push(Candidate {
            item: ResultRef::Command(*cmd),
            haystack: format!("{} {}", cmd.label(), cmd.hint()),
        });
    }
}

/// Build the palette overlay. Returns an empty placeholder when closed so
/// the caller can stack it unconditionally into the top-level view.
pub fn build_overlay<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let Some(state) = &ui.palette else {
        return Space::new().into();
    };

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.45,
                    ..Color::BLACK
                })),
                ..container::Style::default()
            }),
    )
    .on_press(Message::Palette(PaletteMessage::Close));

    // Layout strategy: every column is pinned at an absolute X inside a
    // clipping `Stack`. We can't use a row-of-columns inside a container —
    // iced's flex layout caps each `Length::Fixed` child by the parent's
    // available width, so when the viewport is narrower than the row's
    // intrinsic width the columns get compressed instead of clipping.
    // `pin` short-circuits that constraint by giving its child
    // `available = limits.max() - position`; with a negative position
    // (column off-screen left), `available` actually *exceeds* the
    // viewport's max, so the column lays out at its true 520-px width and
    // the overflow is clipped by the parent `Stack::clip(true)`.
    //
    // The animation offset (`shift_anim`) is added to each column's
    // pinned X uniformly — on push it starts at `+(COL+GAP)` so every
    // column is shifted right of its final spot (the new column is just
    // off-screen right of center) and slides left into place as the
    // offset animates to 0; on pop it starts at `-(COL+GAP)` so the
    // remaining columns sit one slot left of their final position and
    // slide right back to center.
    let anim_offset = state.shift_anim.interpolate_with(|v| v, Instant::now());
    let column_count = state.stack.len();

    let palette_block = responsive(move |size| {
        let viewport_w = size.width;
        let mut layers: Vec<Element<'_, Message>> = Vec::with_capacity(column_count);
        let last_index = column_count.saturating_sub(1);
        for (index, column_state) in state.stack.iter().enumerate() {
            let is_focused = index == last_index;
            let column_el = build_column(ui, theme, column_state, index, is_focused);
            let baseline_x = (viewport_w - COLUMN_WIDTH) / 2.0
                - (column_count - 1 - index) as f32 * (COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP);
            let target_x = baseline_x + anim_offset;
            layers.push(pin(column_el).x(target_x).y(TOP_OFFSET).into());
        }
        stack(layers)
            .clip(true)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    });

    // Wrap the entire stacked overlay in a mouse_area that captures any
    // unhandled scroll. The scrollable inside the modal sees wheel events
    // first and consumes them while it can scroll; once it hits its
    // limit, unhandled delta bubbles up and this outer mouse_area
    // captures it so the diff view behind doesn't move.
    //
    // `opaque` closes the remaining hole: `mouse_interaction` is queried
    // per-layer regardless of event capture, so without it the diff text's
    // I-beam cursor still showed through the scrim.
    opaque(
        mouse_area(stack![scrim, palette_block])
            .on_scroll(|_| Message::Palette(PaletteMessage::NoOp)),
    )
}

fn build_column<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
    depth: usize,
    is_focused: bool,
) -> Element<'a, Message> {
    let header = column_header(ui, theme, column_state);
    let input = build_input(ui, theme, column_state, is_focused);
    let results = build_results(ui, theme, column_state, depth);

    let body = column![header, input, results].spacing(0);

    let panel_color = theme.panel_background_elevated;
    let border_color = if is_focused {
        theme.accent
    } else {
        theme.border
    };
    container(body)
        .width(Length::Fixed(COLUMN_WIDTH))
        .height(Length::Fixed(COLUMN_HEIGHT))
        .style(move |_| container::Style {
            background: Some(Background::Color(panel_color)),
            border: Border {
                width: 1.0,
                color: border_color,
                radius: 10.0.into(),
            },
            // Subtle drop shadow: the scrim already darkens what's behind
            // the modal, so a heavy shadow would just look like double
            // dimming. Just enough to suggest the card is lifted.
            shadow: Shadow {
                color: Color {
                    a: 0.18,
                    ..Color::BLACK
                },
                offset: Vector::new(0.0, 4.0),
                blur_radius: 12.0,
            },
            ..container::Style::default()
        })
        .into()
}

fn column_header<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
) -> Element<'a, Message> {
    let label = match &column_state.source {
        ColumnSource::Root => "Search".to_owned(),
        ColumnSource::Actions(target) => format!("Actions · {}", target_label(target, ui)),
    };
    container(
        text(label)
            .size(11)
            // `emphasis_font` is a no-op on the default UI font (generic
            // sans-serif) — see its docs for why. On macOS the generic
            // family won't resolve to a Medium-weight face and we'd
            // render tofu boxes instead of letters.
            .font(theme::emphasis_font(ui.config.ui_font, Weight::Medium))
            .color(theme.subtle_text),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(theme.panel_background)),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: border::Radius::default().top(10.0),
        },
        ..container::Style::default()
    })
    .into()
}

fn build_input<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
    is_focused: bool,
) -> Element<'a, Message> {
    let placeholder = match &column_state.source {
        ColumnSource::Root => "Search bookmarks, files, commands  (>cmd  @file  :commit)",
        ColumnSource::Actions(_) => "Filter actions",
    };

    let mut input = text_input(placeholder, &column_state.query)
        .id(PALETTE_INPUT_ID)
        .padding(Padding::from([10, 14]))
        .size(15)
        .font(ui.config.ui_font)
        .style(move |_, _status| text_input::Style {
            background: Background::Color(theme.panel_background_elevated),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 0.0.into(),
            },
            icon: theme.muted_text,
            placeholder: theme.subtle_text,
            value: theme.text,
            selection: Color {
                a: 0.25,
                ..theme.accent
            },
        });

    if is_focused {
        // Older (left-of-center) columns are read-only previews. The
        // rightmost column receives input + submit.
        input = input
            .on_input(|q| Message::Palette(PaletteMessage::QueryChanged(q)))
            .on_submit(Message::Palette(PaletteMessage::Accept));
    }

    container(input)
        .padding([4, 4])
        .height(Length::Fixed(INPUT_HEIGHT + 8.0))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn build_results<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
    depth: usize,
) -> Element<'a, Message> {
    // `:` commit-search defers the all-commits scan to ⏎ — show a prompt until
    // it runs, instead of an empty "No matches".
    let revision_prompt = revision_mode_needle(&column_state.query)
        .filter(|_| !column_state.searched)
        .map(|needle| {
            let needle = needle.trim();
            if needle.is_empty() {
                "Type a query, then press ⏎ to search commits".to_owned()
            } else {
                format!("Press ⏎ to search commits for “{needle}”")
            }
        });

    if revision_prompt.is_some() || column_state.matches.is_empty() {
        let label = revision_prompt.unwrap_or_else(|| {
            if column_state.dirty {
                "Searching…".to_owned()
            } else {
                "No matches".to_owned()
            }
        });
        let empty = container(
            text(label)
                .size(13)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding([24, 16]);
        return container(empty)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(theme.panel_background_elevated)),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: border::Radius::default().bottom(10.0),
                },
                ..container::Style::default()
            })
            .into();
    }

    let mut list = column![].spacing(0);
    for (index, m) in column_state.matches.iter().enumerate() {
        list = list.push(build_result_row(ui, theme, column_state, index, m));
    }

    let scrollable_list = scrollable(list)
        .id(results_scrollable_id(depth))
        .direction(Direction::Vertical(
            Scrollbar::default()
                .width(theme::SCROLLBAR_WIDTH)
                .scroller_width(theme::SCROLLBAR_WIDTH)
                .margin(theme::SCROLLBAR_MARGIN)
                // `spacing` switches the scrollbar from overlay mode to
                // embedded — the rail reserves its own width in the
                // layout instead of floating over the content. Without
                // this, long author/hint text on the right of result
                // rows runs underneath the rail.
                .spacing(6.0),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_, status| theme::iced_scrollable_style(theme, status));

    container(scrollable_list)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::from([4, 4]))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: border::Radius::default().bottom(10.0),
            },
            ..container::Style::default()
        })
        .into()
}

fn build_result_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
    index: usize,
    m: &'a PaletteMatch,
) -> Element<'a, Message> {
    let selected = index == column_state.selected;
    let row_bg = if selected {
        theme.selected_file
    } else {
        Color::TRANSPARENT
    };

    let body = result_row_body(ui, theme, &m.item);

    let row_el = container(body)
        .padding([6, 12])
        .width(Length::Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        // Center the body row inside the fixed-height container. The
        // body's own `Row::align_y(Center)` only centers items relative
        // to the row's intrinsic height — without this the row sits at
        // the top of the 34-px cell and the icon/label/hint appear
        // glued to the upper edge.
        .align_y(alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: Some(Background::Color(row_bg)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 6.0.into(),
            },
            ..container::Style::default()
        });

    mouse_area(row_el)
        .on_press(Message::Palette(PaletteMessage::AcceptIndex(index)))
        .on_enter(Message::Palette(PaletteMessage::SelectIndex(index)))
        .into()
}

fn result_row_body<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    item: &'a ResultRef,
) -> Element<'a, Message> {
    let primary = theme.text;
    let muted = theme.subtle_text;

    // Tail elements (author / "file" / hint) keep their intrinsic width
    // and align right; the middle label takes `Length::Fill` with
    // wrapping disabled + ellipsis so long commit messages truncate
    // cleanly instead of wrapping into the next row or shoving the tail
    // off-screen.
    match item {
        ResultRef::WorkingCopy => row![
            text("@")
                .size(13)
                .font(ui.config.mono_font)
                .color(theme.accent),
            Space::new().width(Length::Fixed(10.0)),
            primary_label(ui, primary, "Working copy"),
            Space::new().width(Length::Fixed(8.0)),
            text("revision")
                .size(11)
                .font(ui.config.ui_font)
                .color(muted),
        ]
        .spacing(0)
        .align_y(alignment::Vertical::Center)
        .into(),
        ResultRef::Commit(change_id) => {
            let commit = ui.session.commits.find_by_change_id(change_id.as_str());
            let prefix = commit
                .map(|c| {
                    let len = c.shortest_change_id_len().unwrap_or(8).max(8);
                    c.change_id().chars().take(len).collect::<String>()
                })
                .unwrap_or_else(|| change_id.as_str().chars().take(8).collect::<String>());
            let description = commit
                .map(|c| {
                    if c.has_description() {
                        c.description().lines().next().unwrap_or("").to_owned()
                    } else {
                        "(no description)".to_owned()
                    }
                })
                .unwrap_or_default();
            let author = commit.map(|c| c.author().to_owned()).unwrap_or_default();
            row![
                text(prefix)
                    .size(13)
                    .font(ui.config.mono_font)
                    .color(theme.accent),
                Space::new().width(Length::Fixed(10.0)),
                primary_label(ui, primary, description),
                Space::new().width(Length::Fixed(8.0)),
                text(author).size(11).font(ui.config.ui_font).color(muted),
            ]
            .spacing(0)
            .align_y(alignment::Vertical::Center)
            .into()
        }
        ResultRef::Bookmark(name) => {
            // Find the owning commit so we can show the destination in
            // the tail. Bookmarks without a matching commit (stale data
            // races between snapshots) still render — the tail just goes
            // empty.
            let commit = ui.session.commits.find_by_bookmark(name);
            let tail = commit
                .map(|c| {
                    if c.has_description() {
                        c.description().lines().next().unwrap_or("").to_owned()
                    } else {
                        c.change_id().chars().take(8).collect::<String>()
                    }
                })
                .unwrap_or_default();
            row![
                icons::icon(icons::HASH, 13.0, theme.modified_token),
                Space::new().width(Length::Fixed(10.0)),
                mono_primary_label(ui, primary, name.clone()),
                Space::new().width(Length::Fixed(8.0)),
                text(tail).size(11).font(ui.config.ui_font).color(muted),
            ]
            .spacing(0)
            .align_y(alignment::Vertical::Center)
            .into()
        }
        ResultRef::File(path) => row![
            text("@")
                .size(13)
                .font(ui.config.mono_font)
                .color(theme.info),
            Space::new().width(Length::Fixed(10.0)),
            mono_primary_label(ui, primary, path.clone()),
            Space::new().width(Length::Fixed(8.0)),
            text("file").size(11).font(ui.config.ui_font).color(muted),
        ]
        .spacing(0)
        .align_y(alignment::Vertical::Center)
        .into(),
        ResultRef::Command(cmd) => row![
            text(">")
                .size(13)
                .font(ui.config.mono_font)
                .color(theme.modified_token),
            Space::new().width(Length::Fixed(10.0)),
            primary_label(ui, primary, cmd.label().to_owned()),
            Space::new().width(Length::Fixed(8.0)),
            text(cmd.hint())
                .size(11)
                .font(ui.config.ui_font)
                .color(muted),
        ]
        .spacing(0)
        .align_y(alignment::Vertical::Center)
        .into(),
    }
}

/// Single-line truncating label that takes the row's flexible space.
/// `Length::Fill` width + `Wrapping::None` + `Ellipsis::End` is the iced
/// recipe for "as wide as available, no wrap, truncate with …".
fn primary_label<'a>(
    ui: &'a Diffui,
    color: Color,
    label: impl Into<String>,
) -> Element<'a, Message> {
    text(label.into())
        .size(14)
        .font(ui.config.ui_font)
        .color(color)
        .width(Length::Fill)
        .wrapping(Wrapping::None)
        .ellipsis(Ellipsis::End)
        .into()
}

fn mono_primary_label<'a>(
    ui: &'a Diffui,
    color: Color,
    label: impl Into<String>,
) -> Element<'a, Message> {
    text(label.into())
        .size(14)
        .font(ui.config.mono_font)
        .color(color)
        .width(Length::Fill)
        .wrapping(Wrapping::None)
        .ellipsis(Ellipsis::End)
        .into()
}

fn target_label(target: &ResultRef, ui: &Diffui) -> String {
    match target {
        ResultRef::WorkingCopy => "Working copy".to_owned(),
        ResultRef::Commit(change_id) => ui
            .session
            .commits
            .find_by_change_id(change_id.as_str())
            .map(|c| {
                let len = c.shortest_change_id_len().unwrap_or(8).max(8);
                c.change_id().chars().take(len).collect()
            })
            .unwrap_or_else(|| change_id.as_str().chars().take(8).collect()),
        ResultRef::Bookmark(name) => name.clone(),
        ResultRef::File(path) => path.clone(),
        ResultRef::Command(cmd) => cmd.label().to_owned(),
    }
}

/// Translate a result row to the revision selection it represents.
///
/// For `Commit(change_id)`, we look up the matching `CommitSummary` to
/// resolve the change-id (alphabetic, stable across rewrites — good for
/// recents and identity) into a `revision_id` (hex commit-id — what the jj
/// backend's `CommitId::try_from_hex` actually consumes). `Bookmark(name)`
/// finds the revision that owns the bookmark and uses that one's
/// revision-id. Returns `None` when the underlying reference no longer
/// resolves (e.g. an abandoned change still living in persisted recents,
/// or a bookmark deleted between snapshots).
pub fn revision_selection(item: &ResultRef, ui: &Diffui) -> Option<RevisionSelection> {
    match item {
        ResultRef::WorkingCopy => Some(RevisionSelection::WorkingCopy),
        ResultRef::Commit(change_id) => ui
            .session
            .commits
            .find_by_change_id(change_id.as_str())
            .map(|c| RevisionSelection::Commit(c.commit_id().to_owned())),
        ResultRef::Bookmark(name) => ui
            .session
            .commits
            .find_by_bookmark(name)
            .map(|c| RevisionSelection::Commit(c.commit_id().to_owned())),
        _ => None,
    }
}

/// Resolve a `ResultRef` to its underlying change-id, if any. Used by the
/// accept handler to populate recents (which key by change-id so they
/// survive rewrites). `WorkingCopy` resolves to whichever commit is
/// flagged as the WC right now.
pub fn change_id_for_recents(item: &ResultRef, ui: &Diffui) -> Option<String> {
    match item {
        ResultRef::Commit(change_id) => Some(change_id.0.clone()),
        ResultRef::Bookmark(name) => ui
            .session
            .commits
            .find_by_bookmark(name)
            .map(|c| c.change_id().to_owned()),
        ResultRef::WorkingCopy => ui
            .session
            .commits
            .working_copy()
            .map(|c| c.change_id().to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_prefixes_select_modes() {
        assert_eq!(parse_query("fix bug"), (Mode::Mixed, "fix bug"));
        assert_eq!(parse_query(">refresh"), (Mode::Commands, "refresh"));
        assert_eq!(parse_query("@main.rs"), (Mode::Files, "main.rs"));
        assert_eq!(parse_query(":abc"), (Mode::Revisions, "abc"));
        // The term after a prefix is left-trimmed.
        assert_eq!(parse_query(":  spaced"), (Mode::Revisions, "spaced"));
    }

    #[test]
    fn revision_mode_needle_only_fires_for_colon_prefix() {
        assert_eq!(revision_mode_needle(":abc"), Some("abc"));
        // Bare `:` is commit-search mode with an empty term (shows the prompt,
        // doesn't trigger a scan).
        assert_eq!(revision_mode_needle(":"), Some(""));
        assert_eq!(revision_mode_needle("abc"), None);
        assert_eq!(revision_mode_needle(">cmd"), None);
        assert_eq!(revision_mode_needle("@file"), None);
    }
}
