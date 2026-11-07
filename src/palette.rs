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

use crate::Cx;
use crate::modes::ModeKind;
use crate::theme::Color;
use gpui_kit::{AnyElement, Window};
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::Diffui;
use crate::commands::{self, CommandArg, CommandId, PaletteScope};
use crate::theme::{self, ThemeSpec};
use crate::typography::Aligned;
use diffui_core::RevisionSelection;

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
/// the user. Computed as: column height − input band − rule − results-pane
/// padding (top + bottom) − rule − footer.
const RESULTS_VIEWPORT_HEIGHT: f32 = COLUMN_HEIGHT
    - (INPUT_HEIGHT + 2.0 * RESULTS_INSET)
    - 1.0
    - 2.0 * RESULTS_INSET
    - 1.0
    - FOOTER_HEIGHT;

/// Fixed width per column. The stack lays columns out in a horizontal row;
/// the rightmost is centered in the viewport and earlier columns shift left,
/// clipping off the leading edge once the stack is deep enough.
pub(crate) const COLUMN_WIDTH: f32 = 520.0;
const COLUMN_HEIGHT: f32 = 460.0;
const COLUMN_HORIZONTAL_GAP: f32 = 16.0;
pub(crate) const INPUT_HEIGHT: f32 = 48.0;
pub(crate) const ROW_HEIGHT: f32 = 34.0;
pub(crate) const FOOTER_HEIGHT: f32 = 36.0;
/// Inset of the results list inside the card. A result row pads its content
/// by `CONTENT_X - RESULTS_INSET`, so row glyphs, the input text and the
/// header label all start on the one [`CONTENT_X`] edge.
pub(crate) const RESULTS_INSET: f32 = theme::space::XS;
pub(crate) const CONTENT_X: f32 = theme::space::XL;
/// Fixed box for a row's leading glyph, so icon and `@` rows start their
/// labels at the same x.
const GLYPH_BOX: f32 = 16.0;
pub(crate) const GLYPH_GAP: f32 = 10.0;
const GLYPH_ICON_SIZE: f32 = 14.0;
/// The footer's scope pills (All / Commands / Files / Commits).
const SCOPE_HEIGHT: f32 = 24.0;
const SCOPE_PAD_X: f32 = 8.0;

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
    pub shift_anim: SlideAnimation,
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
fn build_shift_anim(initial: f32) -> SlideAnimation {
    SlideAnimation {
        value: initial,
        started: None,
    }
}

/// The column slide: from `value` to zero, over [`COLUMN_SLIDE_DURATION`].
#[derive(Debug, Clone, Copy)]
pub struct SlideAnimation {
    value: f32,
    started: Option<Instant>,
}

impl SlideAnimation {
    fn start(&mut self, at: Instant) {
        self.started = Some(at);
    }

    pub fn is_animating(&self, at: Instant) -> bool {
        self.started
            .is_some_and(|started| at.duration_since(started) < COLUMN_SLIDE_DURATION)
    }

    /// The offset at `at`, eased out: fast off the mark, settling gently.
    pub fn offset(&self, at: Instant) -> f32 {
        let Some(started) = self.started else {
            return self.value;
        };
        let t = (at.duration_since(started).as_secs_f32() / COLUMN_SLIDE_DURATION.as_secs_f32())
            .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.value * (1.0 - eased)
    }
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
    /// The command line, while the query starts with `jj `.
    pub command: Option<Box<crate::cmdline::CommandLine>>,
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
    /// `@` / working copy. Modelled separately from `Commit` because it
    /// doesn't have a stable change-id.
    WorkingCopy,
    /// A revision. Carries both ids on purpose: the change-id is what recents
    /// key on (it survives rewrites), while the commit-id is what *addresses*
    /// the row. A divergent change has several commits under one change-id, so
    /// a change-id alone would collapse every copy onto whichever one the index
    /// answers with — the row would say `abc/2` and jump to `abc/1`.
    Commit {
        change: ChangeId,
        commit: String,
    },
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
    /// Why a command row can't run: it lists greyed, below the rows that
    /// can, with this beside it.
    pub disabled: Option<commands::Reason>,
}

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
    /// Command lines run from the palette, newest first.
    pub cli: VecDeque<String>,
    /// Revsets typed (the revset field, command-line arguments), newest first.
    pub revsets: VecDeque<String>,
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

    pub fn push_cli(&mut self, line: String) {
        push_text(&mut self.cli, line);
    }

    pub fn push_revset(&mut self, revset: String) {
        if !revset.trim().is_empty() {
            push_text(&mut self.revsets, revset);
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
        // A recents file written by another build can name a command this one
        // no longer has; drop those rather than carrying dead ids around.
        let mut commands: VecDeque<CommandId> = p
            .commands
            .into_iter()
            .filter_map(|name| {
                let name = renamed_command(&name).unwrap_or(&name);
                commands::command(name).map(|command| command.id)
            })
            .collect();
        if revisions.len() > RECENTS_CAPACITY {
            revisions.truncate(RECENTS_CAPACITY);
        }
        if commands.len() > RECENTS_CAPACITY {
            commands.truncate(RECENTS_CAPACITY);
        }
        let mut cli: VecDeque<String> = p.cli.into_iter().collect();
        cli.truncate(TEXT_RECENTS_CAPACITY);
        let mut revsets: VecDeque<String> = p.revsets.into_iter().collect();
        revsets.truncate(TEXT_RECENTS_CAPACITY);
        Self {
            revisions,
            commands,
            cli,
            revsets,
        }
    }
}

/// How many typed lines (commands, revsets) the recents keep.
const TEXT_RECENTS_CAPACITY: usize = 50;

fn push_text(list: &mut VecDeque<String>, entry: String) {
    list.retain(|seen| *seen != entry);
    list.push_front(entry);
    list.truncate(TEXT_RECENTS_CAPACITY);
}

/// The id a pre-registry recents file called this command, when it called it
/// something else.
///
/// Command ids used to be a `CommandId` enum with its own kebab-case persist
/// names; the registry replaced them with dotted ids. Left alone, every user's
/// most-recent commands are silently dropped the first time the new build
/// reads the file — the palette forgets what they use, once, for no reason
/// they can see.
fn renamed_command(persisted: &str) -> Option<&'static str> {
    Some(match persisted {
        "refresh-repository" => "repo.refresh",
        "select-next-file" => "file.next",
        "select-previous-file" => "file.previous",
        "theme-system" => "theme.system",
        "theme-light" => "theme.light",
        "theme-dark" => "theme.dark",
        "theme-high-contrast" => "theme.contrast",
        "copy-file-diff" => "file.copy-diff",
        "open-find" => "find.open",
        "jump-to-revision" => "revision.select",
        "copy-change-id" => "revision.copy-change-id",
        "copy-commit-message" => "revision.copy-commit-message",
        "copy-author" => "revision.copy-author",
        "open-file" => "file.open",
        "copy-file-path" => "file.copy-path",
        _ => return None,
    })
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedRecents {
    #[serde(default)]
    revisions: Vec<String>,
    #[serde(default)]
    commands: Vec<String>,
    #[serde(default)]
    cli: Vec<String>,
    #[serde(default)]
    revsets: Vec<String>,
}

impl From<&Recents> for PersistedRecents {
    fn from(r: &Recents) -> Self {
        Self {
            revisions: r.revisions.iter().cloned().collect(),
            commands: r.commands.iter().map(|id| (*id).to_owned()).collect(),
            cli: r.cli.iter().cloned().collect(),
            revsets: r.revsets.iter().cloned().collect(),
        }
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
            command: None,
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
            command: None,
        };
        recompute_matches(&mut column, ui, false);
        self.stack.push(column);
        // Slide-in from the right: kick the offset out by one column
        // worth (so the new rightmost is initially off-center to the
        // right) and animate back to 0.
        self.shift_anim = build_shift_anim(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP);
        self.shift_anim.start(Instant::now());
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
            self.shift_anim.start(Instant::now());
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
    // A command line lists completions, not matches.
    if matches!(column.source, ColumnSource::Root) && crate::cmdline::is_command(&column.query) {
        column.matches.clear();
        column.selected = 0;
        return;
    }
    let (mode, needle) = parse_query(&column.query);
    let mut matcher = Matcher::new(Config::DEFAULT);
    // nucleo matches case-insensitively by folding the haystack; the needle
    // has to arrive folded already, or an uppercase letter trips its
    // prefilter assertion.
    let needle = needle.to_lowercase();
    let needle = needle.as_str();
    let needle_utf32 = Utf32String::from(needle);

    let mut candidates: Vec<Candidate> = Vec::new();
    match &column.source {
        ColumnSource::Root => {
            if mode == Mode::Mixed || mode == Mode::Commands {
                push_command_candidates(&mut candidates, ui, PaletteScope::Root, &CommandArg::None);
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
            push_action_candidates(&mut candidates, ui, target);
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
            ResultRef::Commit { change, .. } => ui.recents.revision_bonus(change.as_str()),
            ResultRef::Command(id) => ui.recents.command_bonus(id),
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
            ResultRef::WorkingCopy | ResultRef::Commit { .. } => 3,
            ResultRef::File(_) => 2,
            ResultRef::Command(_) => 1,
        };
        let total = (raw_score as u32 * 100) + bonus + category_tiebreak;
        matches.push(PaletteMatch {
            item: candidate.item,
            score: total,
            disabled: candidate.disabled,
        });
    }

    // What can run first, then what can't, each by score.
    matches.sort_by_key(|entry| (entry.disabled.is_some(), std::cmp::Reverse(entry.score)));
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
    disabled: Option<commands::Reason>,
}

fn push_revision_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    out.push(Candidate {
        item: ResultRef::WorkingCopy,
        haystack: "working copy @".to_owned(),
        disabled: None,
    });
    for commit in ui.active().session.commits.iter() {
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
            item: ResultRef::Commit {
                change: ChangeId(commit.change_id().to_owned()),
                commit: commit.commit_id().to_owned(),
            },
            haystack,
            disabled: None,
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
    let mut rows: Vec<(usize, &[String])> = ui.active().session.commits.bookmarked_rows().collect();
    rows.sort_by_key(|(index, _)| *index);
    // A conflicted bookmark's `name??` chip sits on every side; one candidate
    // is enough (the first displayed row — the same side `find_by_bookmark`
    // resolves a pick to).
    let mut seen_labels: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (index, bookmarks) in rows {
        let commit = ui.active().session.commits.row(index);
        for bookmark in bookmarks {
            if !seen_labels.insert(bookmark.as_str()) {
                continue;
            }
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
                disabled: None,
            });
        }
    }
}

fn push_file_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    for file in &ui.active().session.document.files {
        out.push(Candidate {
            item: ResultRef::File(file.path.clone()),
            haystack: file.path.clone(),
            disabled: None,
        });
    }
}

/// The registry's commands for `scope`, each with its `enabled` verdict
/// against the argument it would run with. The ones that can't run stay
/// listed, greyed with the reason, so a missing row never has to be guessed
/// at.
fn push_command_candidates(
    out: &mut Vec<Candidate>,
    ui: &Diffui,
    scope: PaletteScope,
    arg: &CommandArg,
) {
    for (command, availability) in commands::palette_commands(ui, scope, arg) {
        out.push(Candidate {
            item: ResultRef::Command(command.id),
            haystack: format!("{} {}", command.palette_label(), command.hint),
            disabled: availability.err(),
        });
    }
}

fn push_action_candidates(out: &mut Vec<Candidate>, ui: &Diffui, target: &ResultRef) {
    // Bookmarks share the revision action list; the bookmark just points at a
    // commit, so "Copy change-id" / "Copy author" resolve through the owning
    // revision. A command row is its own terminal action and has none.
    let scope = match target {
        ResultRef::WorkingCopy | ResultRef::Commit { .. } | ResultRef::Bookmark(_) => {
            PaletteScope::Revision
        }
        ResultRef::File(_) => PaletteScope::File,
        ResultRef::Command(_) => return,
    };
    push_command_candidates(out, ui, scope, &CommandArg::Result(target.clone()));
}

/// The command line's results viewport: the plain one less the argument
/// strip under the input.
pub(crate) fn command_results_viewport() -> f32 {
    RESULTS_VIEWPORT_HEIGHT - crate::cmdline_view::ARGS_HEIGHT
}

fn is_revision_verb(id: CommandId) -> bool {
    commands::command(id).is_some_and(|command| command.palette == PaletteScope::RevisionVerb)
}

fn target_label(target: &ResultRef, ui: &Diffui) -> String {
    match target {
        ResultRef::WorkingCopy => "Working copy".to_owned(),
        ResultRef::Commit { change, commit } => revision_prefix(
            ui.active()
                .session
                .commits
                .find_by_commit_id(commit)
                .as_ref(),
            change,
        ),
        ResultRef::Bookmark(name) => name.clone(),
        ResultRef::File(path) => path.clone(),
        ResultRef::Command(id) => command_label(id),
    }
}

/// A registry command's label for a palette row. A row can only exist for a
/// command that is in the registry, so the fallback never renders.
fn command_label(id: CommandId) -> String {
    commands::command(id)
        .map(|command| command.palette_label().to_owned())
        .unwrap_or_default()
}

/// The short change-id a revision row shows: the unique prefix (min 8 chars),
/// `/N`-suffixed for a divergent or hidden copy so two rows of one change read
/// apart. Falls back to a bare 8-char prefix for a row that has left the graph.
fn revision_prefix(row: Option<&diffui_core::RowView<'_>>, change: &ChangeId) -> String {
    let Some(row) = row else {
        return change.as_str().chars().take(8).collect();
    };
    let len = row.shortest_change_id_len().unwrap_or(8).max(8);
    let mut prefix: String = row.change_id().chars().take(len).collect();
    if let Some(offset) = row.change_offset() {
        prefix.push('/');
        prefix.push_str(&offset.to_string());
    }
    prefix
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
        // Resolved through the store rather than trusted: a commit-id from a
        // persisted recent, or from a graph the reload has since replaced,
        // must not flow into a mutation.
        ResultRef::Commit { commit, .. } => ui
            .active()
            .session
            .commits
            .find_by_commit_id(commit)
            .map(|c| RevisionSelection::Commit(c.commit_id().to_owned())),
        ResultRef::Bookmark(name) => ui
            .active()
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
        ResultRef::Commit { change, .. } => Some(change.0.clone()),
        ResultRef::Bookmark(name) => ui
            .active()
            .session
            .commits
            .find_by_bookmark(name)
            .map(|c| c.change_id().to_owned()),
        ResultRef::WorkingCopy => ui
            .active()
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

    /// The pre-registry names in a recents file written by an older build all
    /// resolve, so an upgrade doesn't quietly empty the palette's MRU.
    #[test]
    fn recents_written_before_the_registry_still_resolve() {
        let persisted = PersistedRecents {
            revisions: Vec::new(),
            cli: Vec::new(),
            revsets: Vec::new(),
            commands: vec![
                "refresh-repository".to_owned(),
                "copy-change-id".to_owned(),
                "theme-high-contrast".to_owned(),
                "a-command-no-build-ever-had".to_owned(),
            ],
        };
        let recents = Recents::from_persisted(persisted);
        assert_eq!(
            recents.commands.iter().copied().collect::<Vec<_>>(),
            vec!["repo.refresh", "revision.copy-change-id", "theme.contrast"],
            "known names map, the unknown one is dropped"
        );
    }

    /// Every name the old build could have written maps to a command this one
    /// still has — a typo in the table reads as "that command was removed" and
    /// drops the entry.
    #[test]
    fn every_renamed_command_still_exists() {
        for persisted in [
            "refresh-repository",
            "select-next-file",
            "select-previous-file",
            "theme-system",
            "theme-light",
            "theme-dark",
            "theme-high-contrast",
            "copy-file-diff",
            "open-find",
            "jump-to-revision",
            "copy-change-id",
            "copy-commit-message",
            "copy-author",
            "open-file",
            "copy-file-path",
        ] {
            let id = renamed_command(persisted)
                .unwrap_or_else(|| panic!("{persisted} should map to a current id"));
            assert!(
                commands::command(id).is_some(),
                "{persisted} maps to {id}, which no longer exists"
            );
        }
    }

    /// An uppercase letter in the query used to trip nucleo's prefilter
    /// assertion and take the app down.
    #[test]
    fn an_uppercase_query_matches_like_a_lowercase_one() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/palette");
        ui.active = 0;
        let mut column = PaletteState::open(&ui).stack.remove(0);
        column.query = "Theme".to_owned();
        recompute_matches(&mut column, &ui, false);
        let upper: Vec<_> = column.matches.iter().map(|m| m.item.clone()).collect();
        column.query = "theme".to_owned();
        recompute_matches(&mut column, &ui, false);
        let lower: Vec<_> = column.matches.iter().map(|m| m.item.clone()).collect();
        assert!(!upper.is_empty());
        assert_eq!(upper, lower);
    }

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

/// Stub: the palette view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "palette::build_overlay",
        ui.mode(ModeKind::Palette).is_some(),
    )
}

/// Stub: the palette view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn row_shell(
    body: impl gpui_kit::IntoElement,
    selected: bool,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("palette::row_shell")
}

/// Stub: the palette view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn glyph_icon(
    glyph: &'static str,
    color: Color,
    window: &mut Window,
    cx: &mut Cx,
) -> Aligned {
    crate::stub::aligned("palette::glyph_icon")
}
