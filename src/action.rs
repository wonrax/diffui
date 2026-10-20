//! The one vocabulary of user intents.
//!
//! Every producer — the keyboard (through [`crate::keymap`]), the command
//! palette, the menus, toolbar buttons, drag & drop — ends up emitting an
//! [`Action`], and `Message::Action` is the single arm that performs it. That
//! is what keeps a fix in one place: before this existed the same intent was
//! spelled three ways (a `MenuAction`, a palette `CommandId`, a key match) and
//! a bug fixed in one spelling stayed alive in the other two.
//!
//! An `Action` is *what the user asked for*, not how a widget said it. Widget
//! plumbing (scroll offsets, hover, debounce ticks) stays out — see
//! [`crate::message::UiEvent`].

use diffui_core::{FetchTarget, RevisionSelection};

use crate::palette::ResultRef;
use crate::theme::ThemePreference;
use crate::{DetailField, MainView, TabId, activity, mutations};

#[derive(Debug, Clone)]
pub(crate) enum Action {
    // ── Revisions ───────────────────────────────────────────────────────
    /// Show this revision in the diff view.
    SelectRevision(RevisionSelection),
    /// Jump to whatever a palette result row names (revision, bookmark, `@`).
    JumpToResult(ResultRef),
    /// Drop the sidebar's multi-selection marks.
    ClearMultiSelection,
    /// Run a jj mutation. Every mutation entry point funnels here.
    Mutate(mutations::MutationOp),
    /// Ask before running a destructive mutation (a keyboard or palette
    /// abandon), then run it like [`Action::Mutate`].
    ConfirmMutation(mutations::MutationOp),
    /// `jj rebase -b <this> -o trunk()`, asking first only when the
    /// forecast names new conflicts (see [`crate::trunk`]).
    RebaseOntoTrunk(RevisionSelection),
    /// Enter target mode: pick a destination for a rebase/squash/merge.
    StartDraft {
        kind: mutations::DraftKind,
        source: RevisionSelection,
    },
    /// Select `target` and open its inline description editor. `None` means
    /// the revision already on screen — the diff header's own edit button
    /// publishes a fixed message and has no revision to name.
    EditDescription {
        target: Option<RevisionSelection>,
    },
    SaveDescription,
    CancelDescription,
    /// Open the source browser at `revision`, optionally jumped to `path`.
    BrowseSource {
        revision: RevisionSelection,
        path: Option<String>,
    },

    // ── Files ───────────────────────────────────────────────────────────
    SelectNextFile,
    SelectPreviousFile,
    /// Select a file by index into the current document.
    SelectFile(usize),
    /// Select the file at `path` in the current document, if it is there.
    JumpToFile(String),

    // ── Target mode ─────────────────────────────────────────────────────
    /// Move the destination candidate by this delta, skipping draft sources.
    DraftCandidate(i32),
    /// Set the pending placement (the op bar's segmented control).
    DraftPlacement(mutations::PlacementKind),
    /// `o`/`a`/`b`: apply on the armed candidate with that placement, or just
    /// set the placement when nothing is armed yet.
    DraftApplyPlacement(mutations::PlacementKind),
    DraftConfirm,
    DraftCancel,
    /// Stack/un-stack the candidate as a draft source.
    DraftToggleSource,

    // ── Overlays ────────────────────────────────────────────────────────
    OpenPalette,
    ClosePalette,
    TogglePalette,
    /// Esc / Backspace at empty input: pop the rightmost palette column.
    PalettePop,
    /// Move the palette's highlighted row by `±1`.
    PaletteMove(i32),
    /// Tab: push an actions column for the highlighted result.
    PaletteActions,
    OpenFind,
    CloseFind,
    FindNext,
    FindPrevious,
    OpenRepoDialog,
    CloseRepoDialog,
    /// Pick a local working-copy directory with the native folder chooser.
    ChooseRepoFolder,
    /// Resolve the dialog's path and open it as a tab — or, with a folder
    /// suggestion highlighted, act on that row as a click would.
    SubmitRepoDialog,
    /// Move the highlighted folder suggestion by `±1`.
    OpenRepoMove(i32),
    /// Tab: complete the path into the highlighted (else first) suggestion.
    OpenRepoComplete,
    /// A folder suggestion, by row: a repository opens, anything else is
    /// browsed into.
    OpenRepoPick(usize),
    /// Open a repository (or PR) by path/reference.
    OpenRepo(String),
    /// Remove one path from the persisted recent-repository list.
    RemoveRecentRepo(String),
    ClearRecentRepos,
    ToggleActivityPopover,
    CloseActivityPopover,
    ClearActivities,
    DismissMenu,
    /// Open or close the keyboard-shortcut cheat sheet.
    ToggleShortcuts,
    CloseShortcuts,
    ConfirmAccept,
    ConfirmCancel,

    // ── Repository ──────────────────────────────────────────────────────
    Refresh,
    Fetch(FetchTarget),
    Undo,
    Redo,
    /// Revert exactly this jj operation (an activity row's "Revert").
    UndoOperation(activity::ActivityId, String),
    /// Replace the revset and re-evaluate.
    SetRevset(String),
    /// Re-evaluate the log against whatever is in the revset box.
    SubmitRevset,

    // ── View ────────────────────────────────────────────────────────────
    ToggleWrap,
    ToggleSplit,
    /// Show SVG files as their text diff, or rendered.
    SetSvgSource(bool),
    /// Fold the diff header down to its title and one metadata line, or
    /// unfold it.
    ToggleCommitDetails,
    /// Collapse or restore the revision history pane.
    ToggleHistoryPanel,
    /// Collapse or restore the changed-files pane.
    ToggleFilesPanel,
    SetTheme(ThemePreference),
    SetMainView(MainView),

    // ── Tabs ────────────────────────────────────────────────────────────
    SelectTab(TabId),
    /// Activate the tab at this position (⌘1–9). Out of range is a no-op.
    SelectTabIndex(usize),
    CloseTab(TabId),
    CloseActiveTab,
    /// Reorder: put this tab at position `to`. Which tab is active is
    /// untouched. Dragging a tab along the strip emits one per slot crossed.
    MoveTab {
        id: TabId,
        to: usize,
    },

    // ── Clipboard ───────────────────────────────────────────────────────
    /// Copy a value already in hand.
    Copy(String),
    /// Copy author / committer / full description — read from the repository
    /// on demand, since the loaded graph keeps neither dates nor the full
    /// message. `fallback` is copied if that read fails.
    CopyDetail {
        revision: RevisionSelection,
        field: DetailField,
        fallback: String,
    },
    /// Copy the selected file's diff as text.
    CopyFileDiff,

    /// Open a URL (from an activity's captured remote output) in the browser.
    OpenUrl(String),

    /// Bookmark workflow and push — see [`crate::bookmarks`].
    Bookmark(crate::bookmarks::BookmarkAction),

    /// Answer the credential prompt — see [`crate::credentials`].
    Credential(crate::credentials::CredentialAction),
    /// The palette's command line and the CLI output panel.
    CommandLine(crate::cmdline::CmdAction),

    /// Commit, placed new / duplicate, batch drafts — see [`crate::revops`].
    Revops(crate::revops::RevopsAction),

    // ── Partial changes / conflicts ─────────────────────────────────────
    /// A change-selection or conflict-resolution intent (see
    /// [`crate::changes`]).
    Changes(crate::changes::ChangesMsg),

    // ── History ─────────────────────────────────────────────────────────
    /// Op log, evolog, file history, annotate — see [`crate::history_panel`].
    History(crate::history_panel::HistoryAction),

    // ── Diagnostics ─────────────────────────────────────────────────────
    /// Show diffui's log file in the file manager.
    RevealLogFile,
    /// Open `config.toml` in a text editor, creating it if missing.
    OpenConfigFile,

    // ── Comparing ───────────────────────────────────────────────────────
    /// Diff two revisions against each other (see [`crate::compare`]).
    Compare(crate::compare::CompareAction),
    /// Range diffs: diff since trunk, and the range banner's edit.
    Range(crate::range::RangeAction),
    /// Load the full diff of a file (by index) the backend collapsed for size.
    ExpandFile(usize),
}
