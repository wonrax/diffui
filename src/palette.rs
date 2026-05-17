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
        Space, column, container, mouse_area, pin, responsive, row, scrollable,
        scrollable::{Direction, Scrollbar},
        stack, text, text_editor, text_input,
    },
};
use nucleo_matcher::{Config, Matcher, Utf32String};

use crate::backend::RevisionSelection;
use crate::theme::{self, ThemeSpec};
use crate::{Diffui, Message};

/// `Id` for the active palette text input. Refocused whenever the
/// rightmost column changes (open, push, pop) so keystrokes always land in
/// the column the user is steering.
pub const PALETTE_INPUT_ID: &str = "palette-input";

/// `Id` for the op-pad's multi-line message editor. Focused when the op
/// pad is pushed so the user can start typing immediately without
/// clicking the editor first.
pub const OP_PAD_MESSAGE_ID: &str = "op-pad-message";

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
    /// Op pad — a structured form for a destructive jj operation. Doesn't
    /// fuzzy-match anything; the column's `query` / `matches` / `selected`
    /// stay at their defaults and the form renders from `op_draft`.
    OpPad(OpDraft),
    /// Revision picker dedicated to filling the op pad below. Behaves
    /// like a `Root` column but accepts only revisions (commits +
    /// bookmarks + working copy) and routes its accept through the op
    /// pad's `placement_target` slot instead of jumping the diff view.
    OpPadTargetPicker,
}

/// A user-edited draft of a mutating jj operation. Populated from the
/// current selection + the command's `MutationShape` defaults; mutated by
/// the op pad ui slot interactions; consumed by the mutations module when
/// the user hits Apply.
///
/// `message` is a `text_editor::Content` rather than a plain `String` so
/// the multi-line message slot keeps cursor/selection state across view
/// rebuilds. Read the text with `message.text()` at apply time.
#[derive(Debug, Clone)]
pub struct OpDraft {
    pub command: CommandId,
    /// Revisions in user-visible order (primary first, then additional).
    pub source: Vec<RevisionSelection>,
    pub source_mode: SourceMode,
    pub placement_kind: Option<PlacementKind>,
    /// Target rev for placements that need one. `None` means the slot is
    /// armed (waiting for a click in the revision list, or a typed
    /// revset).
    pub placement_target: Option<RevisionSelection>,
    /// Multi-line commit message editor state (used by `describe` /
    /// `new`). Empty when the command's shape doesn't include a message
    /// slot.
    pub message: text_editor::Content,
}

impl OpDraft {
    /// Build a fresh draft for `command`, prefilling source from the
    /// current selection and choosing the default source-mode/placement
    /// from the command's shape. The message field is loaded from the
    /// primary commit's existing description when the op edits an
    /// existing message (currently: `describe`).
    pub fn from_selection(command: CommandId, ui: &Diffui) -> Self {
        let shape = command
            .mutation_shape()
            .expect("OpDraft::from_selection called with non-mutation command");

        let mut source = Vec::with_capacity(1 + ui.selection.additional.len());
        if !matches!(shape.source_arity, Arity::Zero) {
            source.push(ui.selection.primary.clone());
            source.extend(ui.selection.additional.iter().cloned());
        }

        let source_mode = shape
            .allowed_source_modes
            .first()
            .copied()
            .unwrap_or(SourceMode::Just);
        let placement_kind = shape.allowed_placements.first().copied();

        let message = if shape.needs_message && command == CommandId::Describe {
            // Preload the primary commit's existing message so the user
            // is editing in place rather than starting from blank.
            text_editor::Content::with_text(&describe_initial_message(ui).unwrap_or_default())
        } else {
            text_editor::Content::new()
        };

        Self {
            command,
            source,
            source_mode,
            placement_kind,
            placement_target: None,
            message,
        }
    }
}

fn describe_initial_message(ui: &Diffui) -> Option<String> {
    let primary_commit_id = match &ui.selection.primary {
        RevisionSelection::Commit(id) => id.clone(),
        RevisionSelection::WorkingCopy => ui
            .commits
            .iter()
            .find(|c| c.is_working_copy())?
            .commit_id()
            .to_owned(),
    };
    ui.commits
        .iter()
        .find(|c| c.commit_id() == primary_commit_id.as_str())
        .filter(|c| c.has_description())
        .map(|c| c.description().to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResultRef {
    /// `@` / working copy. Modelled separately from `Commit(_)` because it
    /// doesn't have a stable change-id.
    WorkingCopy,
    /// A revision, identified by its change-id.
    Commit(String),
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
    // Built-in commands (top-level, non-mutating)
    RefreshRepository,
    SelectNextFile,
    SelectPreviousFile,
    ThemeSystem,
    ThemeLight,
    ThemeDark,
    ThemeHighContrast,
    CopyFileDiff,
    OpenFind,
    // Tab-action commands (filled per result type by `push_action_candidates`)
    JumpToRevision,
    CopyChangeId,
    CopyCommitMessage,
    CopyAuthor,
    OpenFile,
    CopyFilePath,
    // Mutating jj operations — accept opens an op pad column.
    New,
    Edit,
    Abandon,
    Describe,
    Squash,
    Rebase,
    OpUndo,
    /// Create or move a local bookmark to point at the selected commit.
    BookmarkSet,
    /// Delete a local bookmark (name supplied in the message slot).
    BookmarkDelete,
}

/// Required source-revset arity for a mutation. Drives palette filtering
/// against the current `Selection`. Non-mutation commands ignore this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// No source needed (e.g. `op undo`).
    Zero,
    /// Exactly one source revision (e.g. `edit`).
    One,
    /// One or more source revisions (e.g. `rebase`, `abandon`).
    OneOrMany,
}

/// How a source revset is interpreted relative to its descendants — the
/// rebase `-r` / `-s` / `-b` axis. For every op other than rebase this is
/// always `Just`, but we model it uniformly so the op-pad ui can render a
/// mode selector whenever `allowed_source_modes` has more than one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    /// `-r`: only the named revisions; their descendants are re-parented
    /// in place.
    Just,
    /// `-s`: revisions plus all their descendants come along.
    WithDescendants,
    /// `-b`: the whole "branch" containing the revisions (everything from
    /// their nearest trunk-side ancestor up to them and their descendants).
    Branch,
}

/// Where the source lands relative to the destination. `Onto` (rebase /
/// duplicate / new) makes the destination the new parent; `Into` (squash)
/// folds the changes into the destination; `InsertAfter` / `InsertBefore`
/// splice into a chain on either side of the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementKind {
    Onto,
    Into,
    InsertAfter,
    InsertBefore,
}

/// Risk level for the warning ui. `Safe` = no rewrite (copies, navigation,
/// theme changes); `Rewrite` = standard destructive but recoverable via
/// `jj op undo`; `Irreversible` = a rewrite that can't be undone (rare, not
/// in p0 — placeholder for future ops like `git push --force`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Danger {
    Safe,
    Rewrite,
    Irreversible,
}

/// Shape of the op pad rendered when a mutation command is accepted.
/// Drives slot visibility, validation, and dispatch.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct MutationShape {
    pub source_arity: Arity,
    pub allowed_source_modes: &'static [SourceMode],
    pub allowed_placements: &'static [PlacementKind],
    /// True when the op pad shows a message editor slot — i.e. `describe`
    /// and `new`. The op uses the message both for input (display the
    /// existing message for describe) and for output (write it on apply).
    pub needs_message: bool,
    pub danger: Danger,
}

/// What a command actually does when accepted. `Builtin` and `Action`
/// match the existing read-only command paths; `Mutation(_)` opens an op
/// pad column.
#[derive(Debug, Clone, Copy)]
pub enum CommandKind {
    Builtin,
    Action,
    Mutation(MutationShape),
}

/// Single source of truth for a command's metadata. Replaces the
/// scattered `.label()` / `.hint()` / `.persist_name()` impls so adding a
/// new command is one entry in the `COMMAND_SPECS` table.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub id: CommandId,
    pub label: &'static str,
    pub hint: &'static str,
    /// Stable string used in the on-disk recents file. Renaming `label`
    /// doesn't invalidate persisted MRU entries — only this does.
    pub persist_name: &'static str,
    pub kind: CommandKind,
}

const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        id: CommandId::RefreshRepository,
        label: "Refresh repository",
        hint: "Re-read the repository state",
        persist_name: "refresh-repository",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::SelectNextFile,
        label: "Select next file",
        hint: "Move between files in current diff",
        persist_name: "select-next-file",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::SelectPreviousFile,
        label: "Select previous file",
        hint: "Move between files in current diff",
        persist_name: "select-previous-file",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::ThemeSystem,
        label: "Theme: System",
        hint: "Follow OS appearance",
        persist_name: "theme-system",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::ThemeLight,
        label: "Theme: Light",
        hint: "Set palette theme",
        persist_name: "theme-light",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::ThemeDark,
        label: "Theme: Dark",
        hint: "Set palette theme",
        persist_name: "theme-dark",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::ThemeHighContrast,
        label: "Theme: High contrast",
        hint: "Set palette theme",
        persist_name: "theme-high-contrast",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::CopyFileDiff,
        label: "Copy current file diff",
        hint: "Copy the selected file's diff text",
        persist_name: "copy-file-diff",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::OpenFind,
        label: "Find in current diff",
        hint: "⌘F-style in-diff search across all files",
        persist_name: "open-find",
        kind: CommandKind::Builtin,
    },
    CommandSpec {
        id: CommandId::JumpToRevision,
        label: "Jump to revision",
        hint: "Show this revision in the diff view",
        persist_name: "jump-to-revision",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::CopyChangeId,
        label: "Copy change-id",
        hint: "Copy the revision's change-id",
        persist_name: "copy-change-id",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::CopyCommitMessage,
        label: "Copy commit message",
        hint: "Copy the commit message",
        persist_name: "copy-commit-message",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::CopyAuthor,
        label: "Copy author",
        hint: "Copy author name and email",
        persist_name: "copy-author",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::OpenFile,
        label: "Open file",
        hint: "Scroll the diff to this file",
        persist_name: "open-file",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::CopyFilePath,
        label: "Copy file path",
        hint: "Copy the file's path",
        persist_name: "copy-file-path",
        kind: CommandKind::Action,
    },
    CommandSpec {
        id: CommandId::New,
        label: "New",
        hint: "Create a new commit (with the selected revisions as parents)",
        persist_name: "new",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::OneOrMany,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[
                PlacementKind::Onto,
                PlacementKind::InsertAfter,
                PlacementKind::InsertBefore,
            ],
            needs_message: true,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::Edit,
        label: "Edit",
        hint: "Move the working copy to the selected revision",
        persist_name: "edit",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::One,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[],
            needs_message: false,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::Abandon,
        label: "Abandon",
        hint: "Discard the selected revision(s); descendants get re-parented",
        persist_name: "abandon",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::OneOrMany,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[],
            needs_message: false,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::Describe,
        label: "Describe",
        hint: "Edit the commit message of the selected revision",
        persist_name: "describe",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::One,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[],
            needs_message: true,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::Squash,
        label: "Squash",
        hint: "Fold the selected revision(s) into a destination",
        persist_name: "squash",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::OneOrMany,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[PlacementKind::Into],
            needs_message: false,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::Rebase,
        label: "Rebase",
        hint: "Move the selected revision(s) onto / after / before another commit",
        persist_name: "rebase",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::OneOrMany,
            allowed_source_modes: &[
                SourceMode::Just,
                SourceMode::WithDescendants,
                SourceMode::Branch,
            ],
            allowed_placements: &[
                PlacementKind::Onto,
                PlacementKind::InsertAfter,
                PlacementKind::InsertBefore,
            ],
            needs_message: false,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::OpUndo,
        label: "Undo last op",
        hint: "Revert the most recent destructive operation",
        persist_name: "op-undo",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::Zero,
            allowed_source_modes: &[],
            allowed_placements: &[],
            needs_message: false,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::BookmarkSet,
        label: "Bookmark set",
        hint: "Create or move a local bookmark to the selected commit",
        persist_name: "bookmark-set",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::One,
            allowed_source_modes: &[SourceMode::Just],
            allowed_placements: &[],
            // The message field stands in as the bookmark name slot —
            // single-line content typed by the user.
            needs_message: true,
            danger: Danger::Rewrite,
        }),
    },
    CommandSpec {
        id: CommandId::BookmarkDelete,
        label: "Bookmark delete",
        hint: "Delete a local bookmark by name",
        persist_name: "bookmark-delete",
        kind: CommandKind::Mutation(MutationShape {
            source_arity: Arity::Zero,
            allowed_source_modes: &[],
            allowed_placements: &[],
            needs_message: true,
            danger: Danger::Rewrite,
        }),
    },
];

impl CommandId {
    /// Lookup the spec for this variant. Panics if the table is missing
    /// an entry — that's a programming error, not a runtime one.
    pub fn spec(self) -> &'static CommandSpec {
        COMMAND_SPECS
            .iter()
            .find(|s| s.id == self)
            .expect("CommandSpec missing for variant")
    }

    pub fn label(self) -> &'static str {
        self.spec().label
    }

    pub fn hint(self) -> &'static str {
        self.spec().hint
    }

    /// True when this command opens an op pad on accept (vs running
    /// inline).
    #[allow(dead_code)]
    pub fn is_mutation(self) -> bool {
        matches!(self.spec().kind, CommandKind::Mutation(_))
    }

    /// `MutationShape` for this command, if it's a mutation.
    pub fn mutation_shape(self) -> Option<&'static MutationShape> {
        match &self.spec().kind {
            CommandKind::Mutation(shape) => Some(shape),
            _ => None,
        }
    }
}

/// Top-level commands shown when the user enters the palette with no
/// context. Tab-action commands are appended only inside `Actions(_)`
/// columns by `push_action_candidates`. Mutation commands are also
/// surfaced here but filtered against the current selection's arity by
/// `push_command_candidates`.
const ROOT_COMMANDS: &[CommandId] = &[
    // Built-ins
    CommandId::RefreshRepository,
    CommandId::OpenFind,
    CommandId::SelectNextFile,
    CommandId::SelectPreviousFile,
    CommandId::ThemeSystem,
    CommandId::ThemeDark,
    CommandId::ThemeLight,
    CommandId::ThemeHighContrast,
    CommandId::CopyFileDiff,
    // Mutations
    CommandId::New,
    CommandId::Edit,
    CommandId::Describe,
    CommandId::Abandon,
    CommandId::Squash,
    CommandId::Rebase,
    CommandId::OpUndo,
    CommandId::BookmarkSet,
    CommandId::BookmarkDelete,
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
    fn persist_name(self) -> &'static str {
        self.spec().persist_name
    }

    fn from_persist_name(name: &str) -> Option<Self> {
        COMMAND_SPECS
            .iter()
            .find(|s| s.persist_name == name)
            .map(|s| s.id)
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

    /// Push an op-pad column for `command`, pre-filled from the current
    /// selection. The form's mutations live entirely inside the
    /// `ColumnSource::OpPad(draft)` payload so the existing column-stack
    /// animation / focus management works unchanged.
    pub fn push_op_pad(&mut self, command: CommandId, ui: &Diffui) {
        let draft = OpDraft::from_selection(command, ui);
        let column = PaletteColumn {
            query: String::new(),
            source: ColumnSource::OpPad(draft),
            matches: Vec::new(),
            selected: 0,
            scroll_y: 0.0,
            query_version: 0,
            dirty: false,
            searched: false,
        };
        self.stack.push(column);
        self.shift_anim = build_shift_anim(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP);
        self.shift_anim.go_mut(0.0, Instant::now());
    }

    /// Mutable access to the current op-pad draft, if the topmost column
    /// is one. Used by message-changed / radio-changed / target-set
    /// handlers in `main.rs` to update the draft in place.
    pub fn top_op_draft_mut(&mut self) -> Option<&mut OpDraft> {
        match &mut self.stack.last_mut()?.source {
            ColumnSource::OpPad(draft) => Some(draft),
            _ => None,
        }
    }

    /// Push a target-picker column on top of an op-pad column. No-op
    /// unless the current top is an op pad — callers must check that
    /// the placement makes sense (a picker on top of Root or Actions
    /// would have nowhere to write its result back to).
    pub fn push_target_picker(&mut self, ui: &Diffui) -> bool {
        let Some(top) = self.stack.last() else {
            return false;
        };
        if !matches!(top.source, ColumnSource::OpPad(_)) {
            return false;
        }
        let mut column = PaletteColumn {
            query: String::new(),
            source: ColumnSource::OpPadTargetPicker,
            matches: Vec::new(),
            selected: 0,
            scroll_y: 0.0,
            query_version: 0,
            dirty: false,
            searched: false,
        };
        recompute_matches(&mut column, ui, false);
        self.stack.push(column);
        self.shift_anim = build_shift_anim(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP);
        self.shift_anim.go_mut(0.0, Instant::now());
        true
    }

    /// Pop the target-picker column and write `selection` into the
    /// op-pad column underneath. No-op when the stack isn't in that
    /// configuration. Returns the popped column's animation kicked
    /// off — drives the slide-back transition.
    pub fn fill_target_and_pop(&mut self, selection: RevisionSelection) -> bool {
        // Validate stack shape: [..., OpPad, OpPadTargetPicker].
        let len = self.stack.len();
        if len < 2 {
            return false;
        }
        if !matches!(self.stack[len - 1].source, ColumnSource::OpPadTargetPicker) {
            return false;
        }
        if !matches!(self.stack[len - 2].source, ColumnSource::OpPad(_)) {
            return false;
        }

        self.stack.pop();
        if let Some(top) = self.stack.last_mut()
            && let ColumnSource::OpPad(draft) = &mut top.source
        {
            draft.placement_target = Some(selection);
        }
        self.shift_anim = build_shift_anim(-(COLUMN_WIDTH + COLUMN_HORIZONTAL_GAP));
        self.shift_anim.go_mut(0.0, Instant::now());
        true
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
                push_command_candidates(&mut candidates, ui);
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
        ColumnSource::OpPad(_) => {
            // Op pad columns don't fuzzy-match anything — the form renders
            // straight from the draft.
        }
        ColumnSource::OpPadTargetPicker => {
            // Same surface as the Root column's revision search but
            // without commands or files — the user is choosing a
            // destination rev for the op pad behind this column.
            push_revision_candidates(&mut candidates, ui);
            push_bookmark_candidates(&mut candidates, ui);
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
            ResultRef::Commit(id) => ui.recents.revision_bonus(id),
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
    for commit in ui.commits.iter() {
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
            item: ResultRef::Commit(commit.change_id().to_owned()),
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
    let mut rows: Vec<(usize, &[String])> = ui.commits.bookmarked_rows().collect();
    rows.sort_by_key(|(index, _)| *index);
    for (index, bookmarks) in rows {
        let commit = ui.commits.row(index);
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
    for file in &ui.document.files {
        out.push(Candidate {
            item: ResultRef::File(file.path.clone()),
            haystack: file.path.clone(),
        });
    }
}

fn push_command_candidates(out: &mut Vec<Candidate>, ui: &Diffui) {
    let selection_count = ui.selection.count();
    for cmd in ROOT_COMMANDS {
        // Mutation commands gate on the current selection's size; the
        // op-pad ui then operates on that selection as its source. Non-
        // mutation commands always show — they don't consume selection.
        if let Some(shape) = cmd.mutation_shape()
            && !arity_accepts(shape.source_arity, selection_count)
        {
            continue;
        }
        out.push(Candidate {
            item: ResultRef::Command(*cmd),
            haystack: format!("{} {}", cmd.label(), cmd.hint()),
        });
    }
}

/// Decide whether a command with `arity` should be visible given the
/// current selection size. Selections always have at least 1 (the
/// primary), so `Zero` and `OneOrMany` always show; `One` is only
/// reachable when the user hasn't multi-selected.
fn arity_accepts(arity: Arity, selection_count: usize) -> bool {
    match arity {
        Arity::Zero => true,
        Arity::One => selection_count == 1,
        Arity::OneOrMany => selection_count >= 1,
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

    // Modal scrim: clicking closes the palette, but every other mouse
    // event in the screen area outside the palette card must be
    // *absorbed* — without `on_move` / `on_release`, hovers fall through
    // to the sidebar and trigger row tooltips behind the modal.
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
    .on_press(Message::PaletteClose)
    .on_release(Message::PaletteNoOp)
    .on_move(|_| Message::PaletteNoOp);

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
    mouse_area(stack![scrim, palette_block])
        .on_scroll(|_| Message::PaletteNoOp)
        .into()
}

fn build_column<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    column_state: &'a PaletteColumn,
    depth: usize,
    is_focused: bool,
) -> Element<'a, Message> {
    let header = column_header(ui, theme, column_state);

    let body = match &column_state.source {
        ColumnSource::OpPad(draft) => {
            let form = build_op_pad_body(ui, theme, draft, is_focused);
            column![header, form].spacing(0)
        }
        ColumnSource::Root | ColumnSource::Actions(_) | ColumnSource::OpPadTargetPicker => {
            let input = build_input(ui, theme, column_state, is_focused);
            let results = build_results(ui, theme, column_state, depth);
            column![header, input, results].spacing(0)
        }
    };

    let panel_color = theme.panel_background_elevated;
    let border_color = if is_focused {
        theme.accent
    } else {
        theme.border
    };
    let card = container(body)
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
        });

    // Mouse-event absorber around the card: clicks on empty parts of the
    // column (between rows, on the header) would otherwise fall through
    // to the scrim and close the palette. The interactive widgets inside
    // (text_input, result rows, text_editor) capture their own events
    // first, so this wrapper only fires when the click lands on dead
    // space.
    mouse_area(card)
        .on_press(Message::PaletteNoOp)
        .on_release(Message::PaletteNoOp)
        .on_move(|_| Message::PaletteNoOp)
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
        ColumnSource::OpPad(draft) => format!("Op · {}", draft.command.label()),
        ColumnSource::OpPadTargetPicker => "Pick destination".to_owned(),
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
        // Op-pad columns short-circuit before this function runs; the
        // placeholder here is unreachable but kept for match exhaustivity.
        ColumnSource::OpPad(_) => "",
        ColumnSource::OpPadTargetPicker => "Search for a destination revision",
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
            .on_input(Message::PaletteQueryChanged)
            .on_submit(Message::PaletteAccept);
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
    // Commit search defers the all-commits scan to ⏎ — show a prompt until it
    // runs, instead of an empty "No matches".
    let deferred_needle = revision_mode_needle(&column_state.query);
    let revision_prompt = deferred_needle
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

fn build_op_pad_body<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    draft: &'a OpDraft,
    is_focused: bool,
) -> Element<'a, Message> {
    let shape = draft
        .command
        .mutation_shape()
        .expect("op pad rendered for non-mutation command");

    let mut sections: Vec<Element<'a, Message>> = Vec::new();

    // Source section — hidden when the op takes no source (e.g. OpUndo).
    if !matches!(shape.source_arity, Arity::Zero) {
        sections.push(op_pad_section_label(ui, theme, "Source"));
        sections.push(source_chip_row(ui, theme, &draft.source));
    }

    // Mode section — shown only when the command allows more than one
    // mode (currently rebase). Rendered read-only for chunk B; the radio
    // wiring lands in a later chunk.
    if shape.allowed_source_modes.len() > 1 {
        sections.push(op_pad_section_label(ui, theme, "Mode"));
        sections.push(read_only_value(
            ui,
            theme,
            source_mode_label(draft.source_mode),
        ));
    }

    // Placement + target — shown only when the command has placements.
    if !shape.allowed_placements.is_empty() {
        sections.push(op_pad_section_label(ui, theme, "Placement"));
        let placement_text = draft
            .placement_kind
            .map(placement_kind_label)
            .unwrap_or("—");
        sections.push(read_only_value(ui, theme, placement_text));

        sections.push(op_pad_section_label(ui, theme, "Target"));
        let target_text = draft
            .placement_target
            .as_ref()
            .map(|t| revision_chip_label(ui, t))
            .unwrap_or_else(|| "(click a row to set destination)".to_owned());
        sections.push(read_only_value(ui, theme, &target_text));
    }

    // Message editor — interactive multi-line text editor. Fills the
    // remaining vertical space in the column.
    if shape.needs_message {
        sections.push(op_pad_section_label(ui, theme, "Message"));
        let editor_padding = Padding::from([8, 12]);
        let mut editor = text_editor(&draft.message)
            .id(OP_PAD_MESSAGE_ID)
            .padding(editor_padding)
            .size(14)
            .font(ui.config.ui_font)
            .height(Length::Fill)
            // Match the rest of the palette aesthetic: panel-elevated
            // background, subtle 1px border, accent border on focus,
            // panel-text foreground, accent selection.
            .style(move |_, status| text_editor::Style {
                background: Background::Color(theme.panel_background),
                border: Border {
                    width: 1.0,
                    color: if matches!(status, text_editor::Status::Focused { .. }) {
                        theme.accent
                    } else {
                        theme.border
                    },
                    radius: 6.0.into(),
                },
                placeholder: theme.subtle_text,
                value: theme.text,
                selection: Color {
                    a: 0.25,
                    ..theme.accent
                },
            })
            // Custom key-binding: drop `⌘⏎` / `Ctrl+⏎` so it doesn't
            // insert a newline — that combo is reserved for "apply",
            // routed by the global subscription. Every other key falls
            // back to iced's default text_editor bindings.
            .key_binding(|kp| {
                if matches!(
                    kp.key.as_ref(),
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
                ) && (kp.modifiers.command() || kp.modifiers.control())
                {
                    return None;
                }
                text_editor::Binding::from_key_press(kp)
            });
        if is_focused {
            editor = editor.on_action(Message::OpPadMessageAction);
        }
        let editor_box = container(editor)
            .padding(Padding::from([0, 16]))
            .width(Length::Fill)
            .height(Length::Fill);
        sections.push(editor_box.into());
    }

    sections.push(op_pad_preview(ui, theme, draft));
    sections.push(op_pad_footer(ui, theme, draft));

    let body = column(sections).spacing(8);
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding::from([12, 0]))
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

fn op_pad_section_label<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    label: &'static str,
) -> Element<'a, Message> {
    container(
        text(label)
            .size(10)
            .font(theme::emphasis_font(ui.config.ui_font, Weight::Medium))
            .color(theme.subtle_text),
    )
    .padding(Padding::from([0, 16]))
    .into()
}

fn read_only_value<'a>(ui: &'a Diffui, theme: ThemeSpec, value: &str) -> Element<'a, Message> {
    container(
        text(value.to_owned())
            .size(13)
            .font(ui.config.ui_font)
            .color(theme.text),
    )
    .padding(Padding::from([2, 16]))
    .into()
}

fn source_chip_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    source: &[RevisionSelection],
) -> Element<'a, Message> {
    let mut chip_row = row![].spacing(6);
    for rev in source {
        chip_row = chip_row.push(op_pad_chip(
            theme,
            ui.config.mono_font,
            revision_chip_label(ui, rev),
        ));
    }
    if source.is_empty() {
        chip_row = chip_row.push(
            text("(none)")
                .size(12)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        );
    }
    container(chip_row).padding(Padding::from([0, 16])).into()
}

fn op_pad_chip<'a>(theme: ThemeSpec, font: iced::Font, label: String) -> Element<'a, Message> {
    container(text(label).size(11).font(font).color(theme.text))
        .padding(Padding::from([2, 8]))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background)),
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: 6.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn revision_chip_label(ui: &Diffui, rev: &RevisionSelection) -> String {
    match rev {
        RevisionSelection::WorkingCopy => ui
            .commits
            .iter()
            .find(|c| c.is_working_copy())
            .map(|c| {
                let short: String = c.change_id().chars().take(8).collect();
                format!("@ {short}")
            })
            .unwrap_or_else(|| "@".to_owned()),
        RevisionSelection::Commit(id) => ui
            .commits
            .iter()
            .find(|c| c.commit_id() == id.as_str())
            .map(|c| c.change_id().chars().take(8).collect::<String>())
            .unwrap_or_else(|| id.chars().take(8).collect::<String>()),
    }
}

fn source_mode_label(mode: SourceMode) -> &'static str {
    match mode {
        SourceMode::Just => "Just these revisions",
        SourceMode::WithDescendants => "+ descendants",
        SourceMode::Branch => "Whole branch",
    }
}

fn placement_kind_label(kind: PlacementKind) -> &'static str {
    match kind {
        PlacementKind::Onto => "Onto",
        PlacementKind::Into => "Into",
        PlacementKind::InsertAfter => "Insert after",
        PlacementKind::InsertBefore => "Insert before",
    }
}

fn op_pad_footer<'a>(ui: &'a Diffui, theme: ThemeSpec, _draft: &OpDraft) -> Element<'a, Message> {
    // `⌘⏎` applies for every op pad — consistent across the surface so
    // there's a single muscle-memory keybinding regardless of whether
    // the op has a message editor.
    container(
        text("⌘⏎ Apply  ·  Esc Cancel")
            .size(11)
            .color(theme.subtle_text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([8, 16]))
    .into()
}

/// One-line preview text describing what the op will do, given the
/// current draft. A text-only stand-in for the ghost graph — beefier
/// than a raw "rewrites N commits" because it names the affected
/// change-ids, but cheaper than rendering a projected graph overlay.
/// The real graph overlay slips to p2 polish.
fn op_pad_preview<'a>(ui: &'a Diffui, theme: ThemeSpec, draft: &OpDraft) -> Element<'a, Message> {
    let text_value = preview_text(ui, draft);
    container(
        text(text_value)
            .size(12)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([0, 16]))
    .into()
}

fn preview_text(ui: &Diffui, draft: &OpDraft) -> String {
    let sources = draft
        .source
        .iter()
        .map(|r| revision_chip_label(ui, r))
        .collect::<Vec<_>>();
    let target = draft
        .placement_target
        .as_ref()
        .map(|r| revision_chip_label(ui, r));
    let source_phrase = if sources.is_empty() {
        "<none>".to_owned()
    } else if sources.len() <= 3 {
        sources.join(", ")
    } else {
        format!("{} (+ {} more)", sources[..2].join(", "), sources.len() - 2)
    };

    match draft.command {
        CommandId::OpUndo => "Reverts the most recent operation".to_owned(),
        CommandId::Edit => format!("Moves working copy to {source_phrase}"),
        CommandId::Describe => format!("Rewrites message of {source_phrase}"),
        CommandId::Abandon => {
            let n = draft.source.len();
            format!(
                "Discards {n} commit{plural}: {source_phrase}",
                plural = if n == 1 { "" } else { "s" }
            )
        }
        CommandId::New => format!("New commit with parent(s): {source_phrase}"),
        CommandId::Squash => match &target {
            Some(t) => format!("Folds {source_phrase} into {t}"),
            None => format!("Folds {source_phrase} into … (destination required)"),
        },
        CommandId::Rebase => match &target {
            Some(t) => format!("Moves {source_phrase} onto {t}"),
            None => format!("Moves {source_phrase} onto … (destination required)"),
        },
        CommandId::BookmarkSet => {
            let raw = draft.message.text();
            let name = raw.trim();
            if name.is_empty() {
                format!("Sets bookmark <name> at {source_phrase}")
            } else {
                format!("Sets bookmark `{name}` at {source_phrase}")
            }
        }
        CommandId::BookmarkDelete => {
            let raw = draft.message.text();
            let name = raw.trim();
            if name.is_empty() {
                "Deletes bookmark <name>".to_owned()
            } else {
                format!("Deletes bookmark `{name}`")
            }
        }
        _ => String::new(),
    }
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
        .on_press(Message::PaletteAcceptIndex(index))
        .on_enter(Message::PaletteSelectIndex(index))
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
            let commit = ui.commits.find_by_change_id(change_id);
            let prefix = commit
                .map(|c| {
                    let len = c.shortest_change_id_len().unwrap_or(8).max(8);
                    c.change_id().chars().take(len).collect::<String>()
                })
                .unwrap_or_else(|| change_id.chars().take(8).collect::<String>());
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
            let commit = ui
                .commits
                .iter()
                .find(|c| c.bookmarks().iter().any(|b| b == name));
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
                text("⌗")
                    .size(13)
                    .font(ui.config.mono_font)
                    .color(theme.modified_token),
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
            .commits
            .find_by_change_id(change_id)
            .map(|c| {
                let len = c.shortest_change_id_len().unwrap_or(8).max(8);
                c.change_id().chars().take(len).collect()
            })
            .unwrap_or_else(|| change_id.chars().take(8).collect()),
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
            .commits
            .find_by_change_id(change_id)
            .map(|c| RevisionSelection::Commit(c.commit_id().to_owned())),
        ResultRef::Bookmark(name) => ui
            .commits
            .iter()
            .find(|c| c.bookmarks().iter().any(|b| b == name))
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
        ResultRef::Commit(change_id) => Some(change_id.clone()),
        ResultRef::Bookmark(name) => ui
            .commits
            .iter()
            .find(|c| c.bookmarks().iter().any(|b| b == name))
            .map(|c| c.change_id().to_owned()),
        ResultRef::WorkingCopy => ui.commits.working_copy().map(|c| c.change_id().to_owned()),
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
