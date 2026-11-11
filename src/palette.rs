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

use crate::root::PaneSet;
use crate::theme::{Color, Font, Weight, text_size};
use crate::{Action, Cx, icons, input, ui};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, Div, ElementId, MouseButton, MouseMoveEvent, ScrollHandle, Window, div, px,
};
use nucleo_matcher::{Config, Matcher, Utf32Str, Utf32String};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::Diffui;
use crate::commands::{self, CommandArg, CommandId, PaletteScope};
use crate::theme::{self, ThemeSpec};
use crate::typography::{self, Aligned};
use diffui_core::RevisionSelection;

/// `Id` for the active palette text input. Refocused whenever the
/// rightmost column changes (open, push, pop) so keystrokes always land in
/// the column the user is steering.
pub const PALETTE_INPUT_ID: &str = "palette-input";

/// The scrollable id for column at `depth`. Each column gets its own, so each
/// keeps its own scroll handle.
pub fn results_scrollable_id(depth: usize) -> String {
    format!("palette-results-{depth}")
}

/// Where a root column's results list starts below the card's top edge.
#[cfg(test)]
pub(crate) fn list_top() -> f32 {
    INPUT_BAND + 1.0 + SCOPES_HEIGHT + 1.0
}

/// Fixed width per column. The stack lays columns out in a horizontal row;
/// the rightmost is centered in the viewport and earlier columns shift left,
/// clipping off the leading edge once the stack is deep enough.
pub(crate) const COLUMN_WIDTH: f32 = 620.0;
const COLUMN_HEIGHT: f32 = 500.0;
const COLUMN_HORIZONTAL_GAP: f32 = 16.0;
/// Height of the input band at the card's top.
pub(crate) const INPUT_BAND: f32 = 52.0;
pub(crate) const ROW_HEIGHT: f32 = theme::list::height::RESULT;
/// Inset of the results list inside the card. A result row pads its content
/// by [`ROW_PAD_X`], so row glyphs, the input's glyph and the group headings
/// all start on the one [`CONTENT_X`] edge.
pub(crate) const RESULTS_INSET: f32 = theme::list::INSET;
const ROW_PAD_X: f32 = CONTENT_X - RESULTS_INSET;
pub(crate) const CONTENT_X: f32 = theme::space::XL;
/// Fixed box for a row's leading glyph, so icon and `@` rows start their
/// labels at the same x.
const GLYPH_BOX: f32 = 16.0;
pub(crate) const GLYPH_GAP: f32 = 10.0;
const GLYPH_ICON_SIZE: f32 = 14.0;
/// The most a row's hint takes, so a long one leaves the label room.
const TAIL_MAX_WIDTH: f32 = COLUMN_WIDTH * 0.45;
/// The band of scope tabs under the input.
const SCOPES_HEIGHT: f32 = 40.0;

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
/// happened, now show me where I am". `EaseInOut` is
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
    /// rightmost slot. The floats pane asks for animation frames while
    /// `is_animating()` is true.
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
    /// Bumped on every query edit and every recompute: the results list
    /// reveals its highlighted row afresh when it moves, so a new query starts
    /// at the top.
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
    /// How many results each scope tab would show for the current needle.
    pub counts: ScopeCounts,
}

/// The count beside each scope tab. Commit search runs on Enter only, so its
/// count is known once it has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeCounts {
    pub commands: usize,
    pub files: usize,
    pub bookmarks: usize,
    pub revisions: Option<usize>,
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
    /// nucleo's match positions in the row's label, by char, for the row to
    /// pick out what was typed. Empty when the label isn't what was matched.
    pub indices: Vec<u32>,
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
        let persisted = PersistedRecents::from(self);
        if let Ok(raw) = toml::to_string(&persisted) {
            let _ = crate::app_files::write_atomic(&path, raw.as_bytes());
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

fn recents_path() -> Option<std::path::PathBuf> {
    crate::app_files::app_file(crate::app_files::AppDir::Data, "recents.toml")
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

impl PaletteState {
    pub fn open(ui: &Diffui) -> Self {
        let mut column = PaletteColumn {
            query: String::new(),
            source: ColumnSource::Root,
            matches: Vec::new(),
            selected: 0,
            query_version: 0,
            dirty: false,
            searched: false,
            command: None,
            counts: ScopeCounts::default(),
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
            query_version: 0,
            dirty: false,
            searched: false,
            command: None,
            counts: ScopeCounts::default(),
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

    /// Whether the column-push/pop animation is still mid-transition: the
    /// floats pane asks for frames while it is.
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

    let root = matches!(column.source, ColumnSource::Root);
    let search_commits = root && mode == Mode::Revisions && search_revisions;
    let mut candidates: Vec<Candidate> = Vec::new();
    match &column.source {
        ColumnSource::Root => {
            // Every cheap scope's candidates, whatever the scope in force, so
            // each scope tab can say how many it holds. Commits are NOT
            // searched here unless asked: that's `:` mode on Enter, kept off
            // the keystroke path.
            push_command_candidates(&mut candidates, ui, PaletteScope::Root, &CommandArg::None);
            push_bookmark_candidates(&mut candidates, ui);
            push_file_candidates(&mut candidates, ui);
            if search_commits {
                candidates.push(Candidate {
                    item: ResultRef::WorkingCopy,
                    haystack: "working copy @".to_owned(),
                    disabled: None,
                });
            }
        }
        ColumnSource::Actions(target) => {
            push_action_candidates(&mut candidates, ui, target);
        }
    }

    let mut counts = ScopeCounts::default();
    let mut matches: Vec<(PaletteMatch, String)> = Vec::with_capacity(candidates.len());
    // One buffer for every haystack's code points, rather than one per row.
    let mut chars = Vec::new();
    let mut score_of = |haystack: &str| {
        if needle.is_empty() {
            Some(0u16)
        } else {
            matcher.fuzzy_match(Utf32Str::new(haystack, &mut chars), needle_utf32.slice(..))
        }
    };
    for candidate in candidates {
        let score = score_of(&candidate.haystack);
        let Some(raw_score) = score else { continue };
        match &candidate.item {
            ResultRef::Command(_) => counts.commands += 1,
            ResultRef::Bookmark(_) => counts.bookmarks += 1,
            ResultRef::File(_) => counts.files += 1,
            ResultRef::WorkingCopy | ResultRef::Commit { .. } => {}
        }
        if root && !mode.lists(&candidate.item) {
            continue;
        }

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
        matches.push((
            PaletteMatch {
                item: candidate.item,
                score: total,
                disabled: candidate.disabled,
                indices: Vec::new(),
            },
            candidate.haystack,
        ));
    }
    if search_commits {
        // Every commit is scored through one reused haystack, and only the
        // ones that can still make the list are made rows.
        let commits = &ui.active().session.commits;
        let mut hits: Vec<(u32, usize)> = Vec::new();
        let mut haystack = String::new();
        for (index, commit) in commits.iter().enumerate() {
            commit_haystack(&commit, &mut haystack);
            if let Some(score) = score_of(&haystack) {
                // The commit category's tie-break is 3, as in the loop above.
                let total = score as u32 * 100 + ui.recents.revision_bonus(commit.change_id()) + 3;
                hits.push((total, index));
            }
        }
        hits.sort_by_key(|&(total, index)| (std::cmp::Reverse(total), index));
        hits.truncate(MAX_RESULT_ROWS);
        hits.sort_by_key(|&(_, index)| index);
        for (score, index) in hits {
            let commit = commits.row(index);
            let item = ResultRef::Commit {
                change: ChangeId(commit.change_id().to_owned()),
                commit: commit.commit_id().to_owned(),
            };
            let entry = PaletteMatch {
                item,
                score,
                disabled: None,
                indices: Vec::new(),
            };
            // A commit row's label isn't its haystack, so no positions to find.
            matches.push((entry, String::new()));
        }
    }

    // What can run first, then what can't, each by score.
    matches.sort_by_key(|(entry, _)| (entry.disabled.is_some(), std::cmp::Reverse(entry.score)));
    matches.truncate(MAX_RESULT_ROWS);
    // Where the needle landed, for the rows on show only: finding the
    // positions costs more than the score.
    if !needle.is_empty() {
        for (entry, haystack) in &mut matches {
            if !label_leads_haystack(&entry.item) {
                continue;
            }
            let _ = matcher.fuzzy_indices(
                Utf32Str::new(haystack, &mut chars),
                needle_utf32.slice(..),
                &mut entry.indices,
            );
            entry.indices.sort_unstable();
            entry.indices.dedup();
        }
    }
    let mut matches: Vec<PaletteMatch> = matches.into_iter().map(|(entry, _)| entry).collect();
    group_rows(&mut matches);

    if root {
        counts.revisions = (mode == Mode::Revisions && search_revisions).then_some(matches.len());
        column.counts = counts;
    }
    column.matches = matches;
    column.selected = column.selected.min(column.matches.len().saturating_sub(1));
}

impl Mode {
    /// Whether a root result belongs in this scope's list.
    fn lists(self, item: &ResultRef) -> bool {
        match self {
            Mode::Mixed => !matches!(item, ResultRef::WorkingCopy | ResultRef::Commit { .. }),
            Mode::Commands => matches!(item, ResultRef::Command(_)),
            Mode::Files => matches!(item, ResultRef::File(_)),
            Mode::Revisions => matches!(item, ResultRef::WorkingCopy | ResultRef::Commit { .. }),
        }
    }
}

/// Whether a row's label is the start of the haystack it was matched in, so
/// the match positions fall on the label.
fn label_leads_haystack(item: &ResultRef) -> bool {
    matches!(
        item,
        ResultRef::Command(_) | ResultRef::Bookmark(_) | ResultRef::File(_)
    )
}

/// The heading a result lists under.
pub(crate) fn group_of(item: &ResultRef) -> &'static str {
    match item {
        ResultRef::WorkingCopy | ResultRef::Commit { .. } => "Revisions",
        ResultRef::Bookmark(_) => "Bookmarks",
        ResultRef::File(_) => "Files",
        ResultRef::Command(id) => command_group(id),
    }
}

/// A command's group, from its id's namespace.
fn command_group(id: CommandId) -> &'static str {
    match id.split('.').next().unwrap_or(id) {
        "repo" | "tab" | "activity" | "palette" => "Repository",
        "revision" | "selection" | "range" | "diff" => "Revisions",
        "bookmark" => "Bookmarks",
        "changes" | "file" | "lines" => "Changes",
        "conflict" => "Conflicts",
        "compare" => "Compare",
        "view" | "find" | "source" | "history" => "View",
        "theme" => "Appearance",
        _ => "App",
    }
}

/// Gathers each group's rows together, the groups in the order their best
/// row ranked and the rows keeping their rank inside a group, so the best
/// match still leads the list.
fn group_rows(matches: &mut [PaletteMatch]) {
    let mut order: Vec<&'static str> = Vec::new();
    for entry in matches.iter() {
        let group = group_of(&entry.item);
        if !order.contains(&group) {
            order.push(group);
        }
    }
    matches.sort_by_key(|entry| {
        let group = group_of(&entry.item);
        order.iter().position(|seen| *seen == group)
    });
}

/// Where row `index` of a grouped list sits: its top (taking in the heading
/// above it when it opens a group) and its bottom, below the list's padding.
/// `groups` is each row's group, in order.
pub(crate) fn row_span<'a>(groups: impl IntoIterator<Item = &'a str>, index: usize) -> (f32, f32) {
    let mut y = 0.0;
    let mut last: Option<&str> = None;
    for (at, group) in groups.into_iter().enumerate() {
        let opens = last != Some(group);
        if opens {
            y += crate::overlay_kit::GROUP_HEIGHT;
            last = Some(group);
        }
        if at == index {
            let top = if opens {
                y - crate::overlay_kit::GROUP_HEIGHT
            } else {
                y
            };
            return (top, y + ROW_HEIGHT);
        }
        y += ROW_HEIGHT;
    }
    (y, y)
}

struct Candidate {
    item: ResultRef,
    /// String fed to nucleo. Usually a concatenation of all the user-visible
    /// identifiers for the row, so a query like "abc def" matches across
    /// change-id + description + author.
    haystack: String,
    disabled: Option<commands::Reason>,
}

/// What a commit is searched by: its ids, description, author and
/// bookmarks, written into `out`.
fn commit_haystack(commit: &diffui_core::RowView<'_>, out: &mut String) {
    out.clear();
    out.push_str(commit.change_id());
    out.push(' ');
    out.push_str(commit.commit_id());
    out.push(' ');
    if commit.has_description() {
        out.push_str(commit.description());
        out.push(' ');
    }
    out.push_str(commit.author());
    for bookmark in commit.bookmarks() {
        out.push(' ');
        out.push_str(bookmark);
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

/// Build the palette overlay. Empty when closed so the caller can stack it
/// unconditionally into the top-level view.
///
/// Every column is pinned at an absolute x inside a clipping layer, so a column
/// off-screen left keeps its true width and is cut by the layer. The animation
/// offset is added to each column uniformly: on push it starts at `+(COL+GAP)`,
/// so every column sits right of its final spot and slides left into place; on
/// pop it starts at `-(COL+GAP)` and the remaining columns slide right back.
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(state) = ui.palette() else {
        return div().into_any_element();
    };
    let close = Diffui::on_action(Action::ClosePalette, cx);
    let viewport = f32::from(window.viewport_size().width);
    let offset = state.shift_anim.offset(Instant::now());
    let count = state.stack.len();
    let mut layer = div()
        .id("palette")
        .absolute()
        .inset_0()
        .overflow_hidden()
        .occlude()
        .child(
            div()
                .absolute()
                .inset_0()
                .bg(theme::SCRIM)
                .on_mouse_down(MouseButton::Left, close),
        );
    for (depth, column) in state.stack.iter().enumerate() {
        let focused = depth + 1 == count;
        let x = (viewport - COLUMN_WIDTH) / 2.0
            - (count - 1 - depth) as f32 * (COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP)
            + offset;
        // Anchored near the top (rather than centered), like VS Code / Raycast,
        // so the input keeps its y as columns push and pop.
        layer = layer.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(crate::toolbar::modal_top()))
                .child(build_column(
                    ui,
                    theme,
                    column,
                    depth,
                    focused,
                    focused && count > 1,
                    window,
                    cx,
                )),
        );
    }
    layer.into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn build_column(
    ui: &Diffui,
    theme: ThemeSpec,
    column: &PaletteColumn,
    depth: usize,
    focused: bool,
    // Only worth marking when there are columns to tell apart.
    outline_focus: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let edge = if outline_focus {
        theme.accent
    } else {
        theme.border
    };
    let card = theme::surface_with(
        div(),
        theme::Elevation::Modal,
        theme::Elevation::Modal.radius(),
        edge,
        theme,
    )
    .id(("palette-column", depth))
    .w(px(COLUMN_WIDTH))
    .h(px(COLUMN_HEIGHT))
    .flex()
    .flex_col()
    .overflow_hidden()
    .occlude();
    let body = |slot: AnyElement| div().flex_1().min_h_0().w_full().child(slot);
    let rule = || crate::root::horizontal_divider(theme);
    if let Some(command) = column.command.as_deref() {
        let input =
            crate::cmdline_view::input(ui, theme, &column.query, command, focused, window, cx);
        let strip = crate::cmdline_view::args_strip(ui, theme, &column.query, command, window, cx);
        let results = crate::cmdline_view::results(ui, theme, &column.query, command, window, cx);
        let footer = crate::cmdline_view::footer(ui, theme, command, window, cx);
        card.child(input)
            .children(strip)
            .child(rule())
            .child(scope_tabs(ui, theme, column, cx))
            .child(rule())
            .child(body(results))
            .child(footer)
            .into_any_element()
    } else {
        let root = matches!(column.source, ColumnSource::Root);
        let input = build_input(ui, theme, column, focused, window, cx);
        let results = build_results(ui, theme, column, depth, window, cx);
        let footer = column_footer(ui, theme, column);
        card.child(input)
            .child(rule())
            .when(root, |card| {
                card.child(scope_tabs(ui, theme, column, cx)).child(rule())
            })
            .child(body(results))
            .child(footer)
            .into_any_element()
    }
}

/// The row of scope tabs under the input: All / Commands / Files / Revisions,
/// each with how many results it holds, then, past a rule, the tab that turns
/// the input into a jj command line. The scopes are the query prefixes
/// (`>`, `@`, `:`), so typing one presses its tab too.
fn scope_tabs(ui: &Diffui, theme: ThemeSpec, column: &PaletteColumn, cx: &mut Cx) -> AnyElement {
    let font = ui.config.ui_font;
    let command = column.command.is_some();
    let (mode, needle) = parse_query(&column.query);
    let needle = if command { "" } else { needle };
    let counts = column.counts;
    let scopes = [
        (
            "All",
            "",
            Mode::Mixed,
            Some(counts.commands + counts.bookmarks + counts.files),
        ),
        ("Commands", ">", Mode::Commands, Some(counts.commands)),
        ("Files", "@", Mode::Files, Some(counts.files)),
        ("Revisions", ":", Mode::Revisions, counts.revisions),
    ];
    let mut well = ui::segmented(theme);
    for (index, (label, prefix, scope, count)) in scopes.into_iter().enumerate() {
        let query = format!("{prefix}{needle}");
        // The counts are the search's: a command line has none to give. They
        // keep their room while hidden, so the tabs don't move.
        let count = count.map(|count| (count, !command));
        well = well.child(
            segment(
                ("palette-scope", index),
                None,
                label,
                count,
                !command && mode == scope,
                font,
                theme,
                cx,
            )
            .on_click(Diffui::listen(cx, move |this, _, _, cx| {
                this.palette_set_query(query.clone(), cx);
            })),
        );
    }
    let jj = ui::segmented(theme).child(
        segment(
            "palette-scope-jj",
            Some(icons::TERMINAL),
            "jj command",
            None,
            command,
            font,
            theme,
            cx,
        )
        .on_click(Diffui::listen(cx, |this, _, _, cx| {
            if this.command_line().is_none() {
                this.palette_on_query_changed(crate::cmdline::PREFIX.to_owned(), cx);
            }
        })),
    );
    div()
        .w_full()
        .h(px(SCOPES_HEIGHT))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XS))
        .px(px(theme::space::LG))
        .child(well)
        .child(
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(18.0))
                .mx(px(theme::space::XS))
                .bg(crate::theme::firm_ring(theme)),
        )
        .child(jj)
        .into_any_element()
}

/// One scope tab: [`ui::segment`] at the card's small step.
#[allow(clippy::too_many_arguments)]
fn segment(
    id: impl Into<ElementId>,
    glyph: Option<&'static str>,
    label: &'static str,
    count: Option<(usize, bool)>,
    pressed: bool,
    font: Font,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> ui::Button {
    ui::segment(
        id,
        ui::Segment {
            icon: glyph,
            label: label.into(),
            count,
            active: pressed,
            enabled: true,
        },
        ui::Segmented::Small,
        font,
        theme,
        cx,
    )
}

/// The strip under the results: the keys that act on the highlight, and on
/// the root column how to reach the command line; on an actions column, what
/// the actions are for.
fn column_footer(ui: &Diffui, theme: ThemeSpec, column: &PaletteColumn) -> AnyElement {
    let font = ui.config.ui_font;
    let hint =
        |keys: &[&str], label: &'static str| crate::overlay_kit::key_hint(keys, label, font, theme);
    let bound = |commands: &[crate::commands::CommandId], label: &'static str| {
        crate::overlay_kit::bound_hint(
            ui,
            crate::commands::Context::Palette,
            commands,
            label,
            theme,
        )
    };
    let strip = crate::overlay_kit::footer(
        crate::overlay_kit::Density::Strip,
        crate::overlay_kit::MODAL_FOOT,
        theme,
    );
    match &column.source {
        ColumnSource::Root => {
            let searching = revision_mode_needle(&column.query).is_some() && !column.searched;
            let enter = match column.matches.get(column.selected).map(|row| &row.item) {
                _ if searching => "Search",
                Some(ResultRef::Command(_)) | None => "Run",
                Some(_) => "Open",
            };
            strip
                .children(bound(&["palette.previous", "palette.next"], "Select"))
                .child(hint(&["\u{21b5}"], enter))
                .children(bound(&["palette.actions"], "Actions"))
                .child(
                    div()
                        .ml_auto()
                        .flex()
                        .flex_row()
                        .flex_none()
                        .items_center()
                        .gap(px(theme::space::SM))
                        .child(crate::overlay_kit::footer_note("Type", font, theme))
                        .child(ui::keycap("jj", font, theme).view())
                        .child(crate::overlay_kit::footer_note(
                            "for a jj command",
                            font,
                            theme,
                        )),
                )
                .into_any_element()
        }
        ColumnSource::Actions(target) => {
            let font = theme::emphasis_font(font, Weight::MEDIUM);
            strip
                .children(bound(&["palette.back"], "Back"))
                .child(hint(&["\u{21b5}"], "Run"))
                .child(
                    div()
                        .ml_auto()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .justify_end()
                        .child(
                            typography::centered(
                                filling(
                                    target_label(target, ui),
                                    font,
                                    text_size::CAPTION,
                                    theme.subtle_text,
                                ),
                                font,
                                text_size::CAPTION,
                            )
                            .min_w_0(),
                        ),
                )
                .into_any_element()
        }
    }
}

fn build_input(
    ui: &Diffui,
    theme: ThemeSpec,
    column: &PaletteColumn,
    focused: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let placeholder = match &column.source {
        ColumnSource::Root => "Run a command, or find a file, bookmark or revision",
        ColumnSource::Actions(_) => "Filter actions\u{2026}",
    };
    let font = ui.config.ui_font;
    let band = div()
        .w_full()
        .flex_none()
        .h(px(INPUT_BAND))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(GLYPH_GAP))
        .px(px(CONTENT_X))
        .font(font.gpui())
        .child(glyph_box(
            icons::icon(icons::SEARCH, 16.0, theme.subtle_text).into_any_element(),
        ));
    let esc = ui::keycap("esc", font, theme).view();
    if !focused {
        // Older (left-of-center) columns are read-only previews. Only the rightmost
        // holds the input: a field's state is keyed by its id.
        let (text, color) = if column.query.is_empty() {
            (placeholder.to_owned(), theme.subtle_text)
        } else {
            (column.query.clone(), theme.text)
        };
        return band
            .child(
                filling(text, font, text_size::TITLE, color)
                    .flex_1()
                    .min_w_0(),
            )
            .into_any_element();
    }
    let on_input = Diffui::listen(cx, |this, query: &String, _, cx| {
        let same = this
            .palette()
            .and_then(PaletteState::top)
            .is_some_and(|column| column.query == *query);
        if !same {
            this.palette_on_query_changed(query.clone(), cx);
        }
    });
    let on_submit = Diffui::listen(cx, |this, _: &(), _, cx| this.palette_submit(cx));
    let spec = input::FieldSpec::new(PALETTE_INPUT_ID, &column.query)
        .placeholder(placeholder)
        .on_input(move |query, window, cx| on_input(&query, window, cx))
        .on_submit(move |window, cx| on_submit(&(), window, cx));
    band.child(
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .child(
                input::single_line(spec, font, window, cx)
                    .appearance(false)
                    .text_size(px(text_size::TITLE))
                    .h_full()
                    .px(px(0.0))
                    .py(px(0.0)),
            ),
    )
    .child(esc)
    .into_any_element()
}

impl Diffui {
    /// A scope tab was pressed: the query takes its prefix, keeping what was
    /// typed after it, and the input keeps the keyboard.
    fn palette_set_query(&mut self, query: String, cx: &mut Cx) {
        self.palette_on_query_changed(query.clone(), cx);
        crate::input::focus(PALETTE_INPUT_ID, cx);
        crate::input::set_caret(PALETTE_INPUT_ID, query.len(), cx);
    }
}

fn build_results(
    ui: &Diffui,
    theme: ThemeSpec,
    column: &PaletteColumn,
    depth: usize,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    // `:` commit-search defers the all-commits scan to ⏎: show a prompt until it
    // runs, instead of an empty "No matches".
    let revision_prompt = revision_mode_needle(&column.query)
        .filter(|_| !column.searched)
        .map(|needle| {
            let needle = needle.trim();
            if needle.is_empty() {
                "Type a query, then press ⏎ to search revisions".to_owned()
            } else {
                format!("Press ⏎ to search revisions for “{needle}”")
            }
        });
    if revision_prompt.is_some() || column.matches.is_empty() {
        let label = revision_prompt.unwrap_or_else(|| {
            if column.dirty {
                "Searching…".to_owned()
            } else {
                "No matches".to_owned()
            }
        });
        return empty_results(ui, theme, label).into_any_element();
    }

    let font = ui.config.ui_font;
    let root = matches!(column.source, ColumnSource::Root);
    // The bar floats over the rows' right inset like the graph's does; the rows'
    // text padding keeps every label and hint clear of it.
    let id = results_scrollable_id(depth);
    let handle = ui::scroll_handle(ElementId::Name(format!("{id}-scroll").into()), window, cx)
        .read(cx)
        .clone();
    // A new query reveals its highlight afresh, which takes the list back to
    // its top; between queries, only a highlight that moves scrolls it.
    ui::reveal_on_change(
        ElementId::Name(format!("{id}-revealed-{}", column.query_version).into()),
        &handle,
        Some(column.selected),
        |row| {
            let (top, bottom) = row_span(
                column.matches.iter().map(|entry| group_of(&entry.item)),
                row,
            );
            (top, bottom + 2.0 * RESULTS_INSET)
        },
        window,
        cx,
    );
    let viewport = ui::scroll_viewport(&handle, window);
    let items = visible_results(
        column.matches.iter().map(|row| Some(group_of(&row.item))),
        &handle,
        viewport,
        ROW_HEIGHT,
        |heading| group_heading(heading, font, theme).into_any_element(),
        |index| {
            let row = &column.matches[index];
            let selected = index == column.selected;
            let body = result_row_body(ui, theme, row, root, selected, window, cx);
            div()
                .id(("palette-row", index))
                .child(row_shell(body, selected, theme, window, cx))
                .on_mouse_move(
                    cx.listener(move |this: &mut Diffui, _: &MouseMoveEvent, _, cx| {
                        let selected = this
                            .palette()
                            .and_then(|state| state.stack.get(depth))
                            .is_some_and(|column| column.selected == index);
                        if !selected {
                            this.act_on(cx, PaneSet::FLOATS, |this, _| {
                                this.palette_on_select_index(depth, index)
                            });
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    Diffui::listen(cx, move |this, _, _, cx| {
                        this.palette_on_accept_index(depth, index, cx);
                    }),
                )
                .into_any_element()
        },
    );
    let list = div().w_full().flex().flex_col().children(items);
    results_list(ElementId::Name(id.into()), &handle, list)
}

/// A results list scrolling in what's left of its column, the rows inset
/// from it inside the scroll view so the bar keeps to the column's edge.
pub(crate) fn results_list(id: ElementId, handle: &ScrollHandle, rows: Div) -> AnyElement {
    ui::scroll_list(
        id,
        handle,
        ui::ScrollList {
            inset: [RESULTS_INSET, RESULTS_INSET].into(),
            ..Default::default()
        },
        rows,
    )
    .into_any_element()
}

/// A results list's group headings and rows, in order, built only near the
/// `viewport` tall window `handle` is scrolled to: a list holds up to a couple
/// of hundred rows and shows about ten, and it is rebuilt on every keystroke
/// and hover. Headings and rows (`row_height`) are fixed heights, so spacers
/// hold the rest's place and the list keeps its length. `groups` is each row's heading, if it
/// has one; a heading opens where it changes.
pub(crate) fn visible_results<G: PartialEq + Copy>(
    groups: impl IntoIterator<Item = Option<G>>,
    handle: &ScrollHandle,
    viewport: f32,
    row_height: f32,
    mut heading: impl FnMut(G) -> AnyElement,
    mut row: impl FnMut(usize) -> AnyElement,
) -> Vec<AnyElement> {
    enum Item<G> {
        Heading(G),
        Row(usize),
    }
    let mut items = Vec::new();
    let mut open = None;
    for (index, group) in groups.into_iter().enumerate() {
        if let Some(group) = group
            && open != Some(group)
        {
            open = Some(group);
            items.push(Item::Heading(group));
        }
        items.push(Item::Row(index));
    }
    // Half a viewport either side: the wheel and the bar move the list before
    // the render that follows them.
    let top = -f32::from(handle.offset().y);
    let (from, to) = (top - viewport / 2.0, top + 1.5 * viewport);
    let spacer = |height: f32| div().w_full().flex_none().h(px(height)).into_any_element();
    let mut built = Vec::new();
    let (mut y, mut skipped) = (0.0, 0.0);
    for item in items {
        let height = match item {
            Item::Heading(_) => crate::overlay_kit::GROUP_HEIGHT,
            Item::Row(_) => row_height,
        };
        if y + height < from || y > to {
            skipped += height;
        } else {
            if skipped > 0.0 {
                built.push(spacer(skipped));
                skipped = 0.0;
            }
            built.push(match item {
                Item::Heading(group) => heading(group),
                Item::Row(index) => row(index),
            });
        }
        y += height;
    }
    if skipped > 0.0 {
        built.push(spacer(skipped));
    }
    built
}

/// A group's heading in a results list, its label on the rows' text edge.
pub(crate) fn group_heading(label: &'static str, font: Font, theme: ThemeSpec) -> Div {
    crate::overlay_kit::group_heading(label, font, theme).px(px(ROW_PAD_X))
}

/// What a results list says when it has no rows.
pub(crate) fn empty_results(ui: &Diffui, theme: ThemeSpec, label: String) -> Div {
    ui::empty_state(
        label,
        ui::Placement::Top,
        ui.config.ui_font,
        theme.subtle_text,
    )
    .size_full()
    .bg(theme.panel_background_elevated)
}

/// A result row's cell: fixed height, the label inset on the content edge,
/// washed when highlighted. The pointer moves the highlight, so no hover.
pub(crate) fn row_shell(
    body: impl gpui_kit::IntoElement,
    selected: bool,
    theme: ThemeSpec,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    ui::list_row(
        ui::ListRow::new(ROW_HEIGHT, ROW_PAD_X).no_hover(),
        selected,
        theme,
    )
    .child(body)
    .into_any_element()
}

/// The ink of a row's leading glyph: quiet, the accent on the highlighted row.
pub(crate) fn glyph_ink(selected: bool, theme: ThemeSpec) -> Color {
    if selected {
        theme.accent
    } else {
        theme.subtle_text
    }
}

fn result_row_body(
    ui: &Diffui,
    theme: ThemeSpec,
    row: &PaletteMatch,
    root: bool,
    selected: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let primary = theme.text;
    let muted = theme.subtle_text;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let ink = glyph_ink(selected, theme);
    let indices = row.indices.as_slice();

    // The hint keeps its width, up to a limit, after the label; the label takes
    // what it needs and ellipsises, so a long commit message truncates instead
    // of shoving the hint off the row.
    let hint = |value: String| -> Option<Aligned> {
        (!value.is_empty()).then(|| {
            Aligned::text(
                typography::line(value, font, text_size::BODY, muted)
                    .max_w(px(TAIL_MAX_WIDTH))
                    .overflow_hidden()
                    .text_ellipsis(),
                font,
                text_size::BODY,
            )
            .shrink()
        })
    };
    let (lead, label, hint, keys): (Aligned, Aligned, Option<Aligned>, Vec<String>) =
        match &row.item {
            ResultRef::WorkingCopy => (
                Aligned::text(
                    typography::line("@", mono, text_size::BODY, theme.accent),
                    mono,
                    text_size::BODY,
                )
                .map(glyph_box),
                highlighted(ui, "Working copy", &[], font, primary, theme),
                None,
                Vec::new(),
            ),
            ResultRef::Commit { change, commit } => {
                let commit = ui.active().session.commits.find_by_commit_id(commit);
                let prefix = revision_prefix(commit.as_ref(), change);
                let unique = commit
                    .as_ref()
                    .and_then(|c| c.shortest_change_id_len())
                    .unwrap_or(prefix.len())
                    .min(prefix.len());
                let description = commit
                    .as_ref()
                    .map(|c| {
                        if c.has_description() {
                            c.description().lines().next().unwrap_or("").to_owned()
                        } else {
                            "(no description)".to_owned()
                        }
                    })
                    .unwrap_or_default();
                let author = commit.map(|c| c.author().to_owned()).unwrap_or_default();
                (
                    change_id_label(&prefix, unique, mono, theme),
                    highlighted(ui, &description, &[], font, primary, theme),
                    hint(author),
                    Vec::new(),
                )
            }
            ResultRef::Bookmark(name) => {
                // Find the owning commit so we can show the destination in the
                // hint. Bookmarks without a matching commit (stale data races
                // between snapshots) still render; the hint just goes empty.
                let commit = ui.active().session.commits.find_by_bookmark(name);
                let destination = commit
                    .map(|c| {
                        if c.has_description() {
                            c.description().lines().next().unwrap_or("").to_owned()
                        } else {
                            c.change_id().chars().take(8).collect::<String>()
                        }
                    })
                    .unwrap_or_default();
                (
                    glyph_icon(crate::overlay_kit::glyph::BOOKMARK, ink, window, cx),
                    highlighted(ui, name, indices, mono, primary, theme),
                    hint(destination),
                    Vec::new(),
                )
            }
            ResultRef::File(path) => (
                glyph_icon(icons::FILE, ink, window, cx),
                highlighted(ui, path, indices, mono, primary, theme),
                // The glyph already says "file".
                None,
                Vec::new(),
            ),
            // A command that can't run: greyed, with the reason as its hint.
            ResultRef::Command(id) if let Some(reason) = row.disabled => (
                glyph_icon(command_icon(id), muted, window, cx),
                highlighted(ui, &command_label(id), indices, font, muted, theme),
                hint(reason.to_owned()),
                Vec::new(),
            ),
            ResultRef::Command(id) => (
                glyph_icon(command_icon(id), ink, window, cx),
                if root && is_revision_verb(id) {
                    verb_label(ui, theme, id, indices)
                } else {
                    highlighted(ui, &command_label(id), indices, font, primary, theme)
                },
                None,
                // The shortcut, where there is one: what the palette teaches.
                ui.keymap
                    .chord_for(commands::Context::Base, id)
                    .map(|chord| crate::overlay_kit::chord_keys(&chord.display()))
                    .unwrap_or_default(),
            ),
        };
    // The lead, the label and the hint are different fonts and sizes: they share
    // a baseline (see `crate::typography`). The keys centre on the row.
    let mut line = vec![lead, Aligned::gap(GLYPH_GAP), label];
    if let Some(hint) = hint {
        line.push(Aligned::gap(theme::space::MD));
        line.push(hint);
    }
    let text = typography::baseline_row(line, 0.0).flex_1().min_w_0();
    let mut row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::LG))
        .child(text);
    if !keys.is_empty() {
        row = row.child(ui::chord(keys, font, theme));
    }
    row.into_any_element()
}

/// A change id as jj prints it: the shortest unique prefix in the id colour,
/// the rest quiet.
fn change_id_label(prefix: &str, unique: usize, mono: Font, theme: ThemeSpec) -> Aligned {
    let split = prefix
        .char_indices()
        .nth(unique)
        .map_or(prefix.len(), |(at, _)| at);
    let (head, tail) = prefix.split_at(split);
    let spans = [
        crate::ellipsis::Span::new(head).color(theme.change_id),
        crate::ellipsis::Span::new(tail).color(theme.subtle_text),
    ];
    Aligned::text(
        crate::overlay_kit::rich_line(mono, text_size::BODY, theme.subtle_text)
            .child(crate::ellipsis::rich_text(&spans, mono, theme.subtle_text)),
        mono,
        text_size::BODY,
    )
}

/// One line of `label` whose chars at `indices` (what the query matched) are
/// picked out in the accent, semibold. Takes the room it needs and gives way
/// with an ellipsis.
fn highlighted(
    ui: &Diffui,
    label: &str,
    indices: &[u32],
    font: Font,
    color: Color,
    theme: ThemeSpec,
) -> Aligned {
    let size = if font == ui.config.mono_font {
        // Mono faces run wide and tall: at the same size a path reads louder
        // than the sans labels around it.
        text_size::BODY
    } else {
        text_size::BODY
    };
    let strong = theme::emphasis_font(font, Weight::SEMIBOLD);
    let mut spans: Vec<crate::ellipsis::Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (at, ch) in label.chars().enumerate() {
        let matched = indices.binary_search(&(at as u32)).is_ok();
        if matched != run_matched && !run.is_empty() {
            spans.push(span(std::mem::take(&mut run), run_matched, strong, theme));
        }
        run_matched = matched;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(span(run, run_matched, strong, theme));
    }
    Aligned::text(
        crate::overlay_kit::rich_line(font, size, color)
            .overflow_hidden()
            .text_ellipsis()
            .child(crate::ellipsis::rich_text(&spans, font, color)),
        font,
        size,
    )
    .shrink()
}

fn span(
    text: String,
    matched: bool,
    strong: Font,
    theme: ThemeSpec,
) -> crate::ellipsis::Span<'static> {
    let span = crate::ellipsis::Span::new(text);
    if matched {
        span.font(strong).color(theme.accent)
    } else {
        span
    }
}

/// A command's glyph, from what it does.
fn command_icon(id: CommandId) -> &'static str {
    use crate::overlay_kit::glyph;
    let verb = id.rsplit('.').find(|part| *part != "start").unwrap_or(id);
    match id {
        "repo.refresh" => icons::REFRESH,
        "repo.undo" | "history.revert-operation" => icons::UNDO,
        "repo.redo" => icons::REDO,
        "repo.fetch" => icons::FETCH,
        "repo.push" | "revision.push-change" | "bookmark.push-to" => icons::UPLOAD,
        "repo.open-dialog" => icons::FOLDER_OPEN,
        "palette.command-line" => icons::TERMINAL,
        "activity.toggle" => icons::LIST_CHECKS,
        "theme.light" => glyph::SUN,
        "theme.dark" => glyph::MOON,
        "theme.system" => glyph::MONITOR,
        "theme.contrast" => glyph::EYE,
        "app.open-config" => glyph::SETTINGS,
        "app.reveal-log" => glyph::FILE_TEXT,
        "help.shortcuts" => glyph::KEYBOARD,
        "find.open" => icons::SEARCH,
        "view.diff" | "diff.since-trunk" => icons::FILE_DIFF,
        "view.source" | "revision.browse-source" | "file.open" => icons::FILE_CODE,
        "view.toggle-split" => icons::SPLIT,
        "view.toggle-wrap" => icons::WRAP,
        "view.toggle-svg-markup" => icons::CODE_XML,
        "view.toggle-commit-details" => icons::SCROLL_TEXT,
        "source.toggle-annotate" => icons::USER,
        "history.op-log" | "history.evolog" => icons::HISTORY,
        "history.restore-operation" => icons::ROTATE_CCW,
        "file.next" => icons::ARROW_DOWN,
        "file.previous" => icons::ARROW_UP,
        "file.expand-diff" => icons::UNFOLD_VERTICAL,
        "lines.toggle" | "changes.toggle-file" | "selection.toggle" => icons::LIST_CHECKS,
        "range.edit" => icons::PENCIL,
        "tab.close" | "changes.clear" => icons::CLOSE,
        _ if id.starts_with("conflict.") => icons::ALERT_TRIANGLE,
        _ if id.starts_with("compare.") => icons::GIT_COMPARE,
        _ if id.starts_with("file.history") => icons::FILE_CLOCK,
        _ if id.starts_with("bookmark.") => glyph::BOOKMARK,
        _ => match verb {
            "rebase" | "branch" | "descendants" | "onto-trunk" => icons::GIT_BRANCH,
            "squash" | "squash-into-parent" | "absorb" | "merge" | "new-merge" => icons::GIT_MERGE,
            "new" | "new-child" | "new-after" | "new-before" => icons::SQUARE_PLUS,
            "duplicate"
            | "copy-change-id"
            | "copy-commit-message"
            | "copy-author"
            | "copy-path"
            | "copy-diff" => icons::COPY,
            "edit" | "edit-description" => icons::PENCIL,
            "commit" => glyph::GIT_COMMIT,
            "split" => icons::SPLIT,
            "parallelize" => icons::COLUMNS_3,
            "restore" | "discard" => icons::ROTATE_CCW,
            "move-into" => icons::ARROW_RIGHT,
            "reset-author" | "update-author-timestamp" => icons::USER,
            "abandon" => icons::CLOSE,
            "keep" => icons::GIT_FORK,
            "select" => icons::CIRCLE,
            _ => icons::CHEVRON_RIGHT,
        },
    }
}

/// A top-level revision verb's label: the verb, then the revision it will act
/// on (the selected one) so a pick never lands somewhere unexpected.
fn verb_label(ui: &Diffui, theme: ThemeSpec, id: CommandId, indices: &[u32]) -> Aligned {
    let selection = &ui.active().session.selected_revision;
    let commits = &ui.active().session.commits;
    let (target, summary) = match commits.find(selection) {
        Some(row) => {
            let prefix = revision_prefix(Some(&row), &ChangeId(row.change_id().to_owned()));
            let summary = if row.has_description() {
                row.description().lines().next().unwrap_or("").to_owned()
            } else {
                "(no description)".to_owned()
            };
            (prefix, summary)
        }
        None => (ui.active().revision_short_label(selection), String::new()),
    };
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    typography::group(
        [
            highlighted(ui, &command_label(id), indices, font, theme.text, theme),
            Aligned::text(
                typography::line(target, mono, text_size::BODY, theme.accent),
                mono,
                text_size::BODY,
            ),
            Aligned::text(
                filling(summary, font, text_size::BODY, theme.subtle_text),
                font,
                text_size::BODY,
            )
            .fill(),
        ],
        theme::space::SM,
    )
    .fill()
}

/// A row's leading icon, centred in the [`GLYPH_BOX`] column and on the
/// label's caps.
pub(crate) fn glyph_icon(
    glyph: &'static str,
    color: Color,
    _window: &mut Window,
    _cx: &mut Cx,
) -> Aligned {
    Aligned::glyph(
        glyph_box(icons::icon(glyph, GLYPH_ICON_SIZE, color).into_any_element()),
        GLYPH_ICON_SIZE,
    )
}

/// Center a row's leading glyph in the shared [`GLYPH_BOX`] column.
fn glyph_box(glyph: AnyElement) -> AnyElement {
    div()
        .w(px(GLYPH_BOX))
        .flex()
        .flex_none()
        .justify_center()
        .child(glyph)
        .into_any_element()
}

/// One line that takes the width it's given and cuts itself with an ellipsis.
pub(crate) fn filling(
    content: impl Into<gpui_kit::SharedString>,
    font: Font,
    size: f32,
    color: Color,
) -> Div {
    typography::line(content, font, size, color)
        .w_full()
        .overflow_hidden()
        .text_ellipsis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::point;

    /// `:` search scores every commit and lists the ones that match, best
    /// first.
    #[test]
    fn commit_search_lists_the_matching_commits() {
        crate::tabs::tests::with_cx(|_| {
            let mut ui = crate::tabs::tests::app();
            crate::tabs::tests::push_tab(&mut ui, "/tmp/repo");
            let mut builder = diffui_core::CommitStoreBuilder::with_capacity(3);
            for (id, description) in [
                ("k", "fix the parser"),
                ("l", "docs"),
                ("m", "parser tests"),
            ] {
                builder.push(diffui_core::CommitSummary {
                    change_id: id.repeat(8),
                    commit_id: id.repeat(12),
                    shortest_change_id_len: None,
                    description: description.to_owned(),
                    author: String::new(),
                    author_timestamp: None,
                    has_description: true,
                    is_empty: None,
                    has_conflict: false,
                    is_divergent: false,
                    is_hidden: false,
                    change_offset: None,
                    is_working_copy: false,
                    is_immutable: false,
                    on_trunk: false,
                    bookmarks: Vec::new(),
                    parent_ids: Vec::new(),
                });
            }
            ui.active_mut().session.commits = builder.finish();
            let mut column = PaletteState::open(&ui).stack.remove(0);
            column.query = ":parser".to_owned();

            recompute_matches(&mut column, &ui, true);

            let changes: Vec<&str> = column
                .matches
                .iter()
                .filter_map(|entry| match &entry.item {
                    ResultRef::Commit { change, .. } => Some(change.0.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(changes.len(), 2, "{changes:?}");
            assert!(!changes.contains(&"llllllll"));
        });
    }

    /// A long results list builds the rows near its view, wherever it's
    /// scrolled, and the headings over them.
    #[test]
    fn a_results_list_builds_only_the_rows_near_its_view() {
        let groups: Vec<Option<&str>> = (0..200)
            .map(|index| Some(if index < 100 { "A" } else { "B" }))
            .collect();
        let handle = ScrollHandle::default();
        let built = |handle: &ScrollHandle| {
            let rows = std::cell::RefCell::new(Vec::new());
            let headings = std::cell::RefCell::new(Vec::new());
            visible_results(
                groups.iter().copied(),
                handle,
                400.0,
                ROW_HEIGHT,
                |group| {
                    headings.borrow_mut().push(group);
                    div().into_any_element()
                },
                |index| {
                    rows.borrow_mut().push(index);
                    div().into_any_element()
                },
            );
            (rows.into_inner(), headings.into_inner())
        };
        let (rows, headings) = built(&handle);
        assert_eq!(headings, ["A"]);
        assert_eq!(rows.first(), Some(&0));
        assert!(rows.len() < 30, "{rows:?}");

        // Scrolled to the second group's heading.
        let heading = crate::overlay_kit::GROUP_HEIGHT;
        handle.set_offset(point(px(0.0), px(-(100.0 * ROW_HEIGHT + heading))));
        let (rows, headings) = built(&handle);
        assert_eq!(headings, ["B"]);
        assert!(rows.contains(&100) && rows.contains(&105), "{rows:?}");
        assert!(rows.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert!(rows.len() < 30, "{rows:?}");
    }

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
    fn rows_group_by_their_best_match_and_keep_their_rank() {
        let row = |item: ResultRef, score: u32| PaletteMatch {
            item,
            score,
            disabled: None,
            indices: Vec::new(),
        };
        let mut rows = vec![
            row(ResultRef::Command("theme.dark"), 9),
            row(ResultRef::File("a.rs".to_owned()), 8),
            row(ResultRef::Command("theme.light"), 7),
            row(ResultRef::File("b.rs".to_owned()), 6),
        ];
        group_rows(&mut rows);
        let order: Vec<u32> = rows.iter().map(|row| row.score).collect();
        assert_eq!(order, [9, 7, 8, 6]);
        // A heading opens each group: the first row of the second group starts
        // under two headings and the two rows above it, its heading included.
        let groups: Vec<&str> = rows.iter().map(|row| group_of(&row.item)).collect();
        let heading = crate::overlay_kit::GROUP_HEIGHT;
        assert_eq!(
            row_span(groups.iter().copied(), 2),
            (heading + 2.0 * ROW_HEIGHT, 2.0 * heading + 3.0 * ROW_HEIGHT)
        );
        assert_eq!(
            row_span(groups.iter().copied(), 0),
            (0.0, heading + ROW_HEIGHT)
        );
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
