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
    /// Resolve the dialog's path and open it as a tab.
    SubmitRepoDialog,
    /// Open a repository (or PR) by path/reference.
    OpenRepo(String),
    /// Remove one path from the persisted recent-repository list.
    RemoveRecentRepo(String),
    ClearRecentRepos,
    ToggleActivityPopover,
    CloseActivityPopover,
    ClearActivities,
    DismissMenu,
    ConfirmAccept,
    ConfirmCancel,

    // ── Repository ──────────────────────────────────────────────────────
    Refresh,
    Fetch(FetchTarget),
    Undo,
    /// Revert exactly this jj operation (an activity row's "Undo").
    UndoOperation(activity::ActivityId, String),
    /// Replace the revset and re-evaluate.
    SetRevset(String),
    /// Re-evaluate the log against whatever is in the revset box.
    SubmitRevset,

    // ── View ────────────────────────────────────────────────────────────
    ToggleWrap,
    ToggleSplit,
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
}
