//! The application `Message` enum — every event the iced runtime delivers to
//! `Diffui::update`. Pulled into its own module to tame `main.rs`; variants are
//! still flat (the self-contained overlay groups get nested in a later pass).

use iced::{Point, Size, theme as iced_theme, widget::text_editor};

use diffui_core::{
    BackendOutput, CommitsTail, DiffDocument, DiffFile, FetchTarget, RepositorySnapshot,
    RevisionDetails, RevisionSelection, StreamRow, SyntaxSpan, github,
};

use crate::theme::ThemePreference;
use crate::{
    HoverTarget, MainView, RefreshOrigin, TabId, ToolbarMenu, activity, mutations, revision_list,
};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// Atomic graph reload finished. Tab-addressed (like every per-tab load
    /// completion): a result landing after a tab switch is dropped instead of
    /// applying to whichever tab is now active — most dangerously, two tabs
    /// both pending on `@` would otherwise pass the revision guard and show
    /// one repo's diff in the other's view.
    BackendLoaded(TabId, RevisionSelection, Box<Result<BackendOutput, String>>),
    /// One batch of commits from a streaming cold load, tagged with the
    /// `commits_version` the stream was started under. Appended into the live
    /// `commits` / `graph` so the sidebar grows as the walk progresses.
    CommitsBatch(u64, Vec<StreamRow>),
    /// End of a streaming cold load: the snapshot fingerprint + single-parent
    /// emptiness updates, or an error. Finalizes the stream (clears the load
    /// cursor, kicks off background empty-status resolution for merges).
    CommitsFinished(u64, Box<Result<CommitsTail, String>>),
    /// Working-copy diff for a streaming cold load, tagged with the stream's
    /// `commits_version`. Sets the diff pane *without* flipping `status` to
    /// `Loaded` — that's the first `CommitsBatch`'s job, so the sidebar never
    /// flashes empty while the graph walk loads the index.
    InitialDiff(
        u64,
        Box<Result<(DiffDocument, Option<RevisionDetails>), String>>,
    ),
    /// Diff-only load for a revision switch — carries just the document and
    /// header details, leaving the commit graph and snapshot untouched.
    /// Tab-addressed; see [`Message::BackendLoaded`].
    DiffLoaded(
        TabId,
        RevisionSelection,
        Box<Result<(DiffDocument, Option<RevisionDetails>), String>>,
    ),
    /// Tab-addressed; see [`Message::BackendLoaded`] — an unguarded snapshot
    /// completing after a tab switch would write the old repo's op fingerprint
    /// into the new tab's session.
    RepositorySnapshotLoaded(TabId, RefreshOrigin, Result<RepositorySnapshot, String>),
    /// Background-resolved empty status for the merge/root commits the loader
    /// left unknown, tagged with the `commits_version` it was computed against
    /// so results from a superseded load are dropped. Tab-addressed because
    /// `commits_version` alone is not unique across tabs.
    EmptyStatusComputed(TabId, u64, Vec<(usize, bool)>),
    SelectFile(usize),
    SelectRowKey(revision_list::RowSelectionKey),
    /// The sidebar / diff view reported a new scroll offset. Mirrored into the
    /// active tab's state so it can be stashed and restored on tab switch.
    SidebarScrolled(f64),
    DiffScrolled(f32),
    /// Right-click on a revision row — opens the context menu. Carries the row's
    /// on-screen rect (window-content points) so the glow can be anchored over
    /// it, plus the cursor point the menu opens at.
    RevisionContextMenu(revision_list::RowSelectionKey, iced::Rectangle, iced::Point),
    /// Turn the selected revision's description strip into its inline editor.
    DescriptionEdit,
    DescriptionAction(text_editor::Action),
    DescriptionSave,
    DescriptionCancel,
    /// A context-menu mutation (new/edit/abandon/bookmark/push) finished,
    /// tab-addressed with its activity id so push remote output lands in the
    /// right log.
    MutationCompleted(
        TabId,
        activity::ActivityId,
        Box<Result<mutations::MutationOutcome, String>>,
    ),
    SelectTheme(ThemePreference),
    /// Toggle diff-pane line wrapping (toolbar button / ⌥Z). Global and
    /// persisted with the window state.
    ToggleDiffWrap,
    /// Toggle the side-by-side diff layout (toolbar button). Global and
    /// persisted with the window state.
    ToggleDiffSplit,
    /// A click on a file-tree row in the sidebar, by *display* row index
    /// (the flattened tree, not the document file index): a file row
    /// selects that file, a directory row toggles its collapse.
    SidebarFileRow(usize),
    SystemThemeChanged(iced_theme::Mode),
    /// Poll tick (macOS, System theme only): re-read the live OS appearance and
    /// re-resolve if it changed. Covers winit going silent once iced pins the
    /// window appearance — see [`crate::chrome::system_appearance`].
    PollSystemTheme,
    WindowFocusChanged(bool),
    RefreshRepository,
    /// The fs watcher saw a write under `.jj/repo/op_heads` — an operation
    /// landed (ours: a wc snapshot / mutation, or external: a CLI `jj` command).
    /// Triggers a cheap op-head read to decide whether it's worth reloading.
    OpLogChanged,
    /// Result of the op-head read kicked by [`Message::OpLogChanged`]. `None` for
    /// git (no op log). Reloads only if the head differs from the one the graph
    /// already reflects (so our own writes don't cause a redundant walk).
    /// Tab-addressed; see [`Message::BackendLoaded`].
    OpHeadChecked(TabId, Box<Result<Option<String>, String>>),
    /// Periodic tick while a load is in flight. No-op handler — it exists only
    /// to keep `view()` re-running so the loading indicator can appear after
    /// its grace period and animate.
    LoadingTick,
    SelectNextFile,
    SelectPreviousFile,
    CopyToClipboard(String),
    SidebarWidthChanged(f32),
    /// The window finished opening: carries its initial outer position (absent
    /// on Wayland) and inner size. Seeds geometry tracking without marking it
    /// dirty — the restored state is already on disk.
    WindowOpened(Option<Point>, Size),
    /// The window was resized by the user. Updates tracking and schedules a
    /// debounced save.
    WindowResized(Size),
    /// The window was moved by the user. Updates tracking and schedules a
    /// debounced save.
    WindowMoved(Point),
    /// Debounce tick: persist the window geometry + sidebar width once the
    /// changes have settled. Subscribed only while a change is pending.
    PersistWindowState,
    // ── Multi-repo ──────────────────────────────────────────────────────
    /// Activate the tab with this id (clicking a tab).
    SelectTab(TabId),
    /// Activate the tab at this position (⌘1–9). Out-of-range is a no-op.
    SelectTabIndex(usize),
    /// Close the tab with this id (clicking its ×).
    CloseTab(TabId),
    /// Close the active tab (⌘W).
    CloseActiveTab,
    /// Open the "open repository" path dialog (+ button / ⌘O).
    OpenRepoDialogOpen,
    OpenRepoDialogClose,
    OpenRepoPathChanged(String),
    /// Resolve the dialog's path and open it as a tab (Enter / "Open").
    OpenRepoSubmit,
    /// Open a repo picked from the dialog's recent-repositories list.
    OpenRecentRepo(String),
    /// Swallow clicks on the dialog card so they don't dismiss it.
    OpenRepoNoOp,
    /// Begin an interactive window drag — fired when the user presses an empty
    /// area of the tab strip on platforms where it stands in for the title bar.
    TitleBarDrag,
    /// Perform the system-configured title-bar double-click action (zoom /
    /// minimize) — fired when the user double-clicks an empty area of the strip.
    /// A native title bar does this for free; our custom strip replicates it.
    TitleBarDoubleClick,
    /// The double-click action resolved on the main thread (macOS): the window's
    /// current frame, its screen's visible frame (AppKit coords `[x,y,w,h]`), the
    /// configured action (0 = zoom, 1 = minimize, 2 = none), and AppKit's native
    /// `animationResizeTime:` for a zoom to the visible frame. Zoom starts the
    /// custom [`crate::ZoomAnim`]; the others dispatch directly.
    TitleBarDoubleClickPlan {
        current: [f64; 4],
        visible: [f64; 4],
        action: u8,
        duration: f64,
    },
    /// One frame of the custom zoom animation — steps the window frame.
    ZoomAnimTick,
    /// Command-palette messages — see [`crate::palette::PaletteMessage`].
    Palette(crate::palette::PaletteMessage),
    /// In-diff find bar messages (⌘F) — see [`crate::find::FindMessage`].
    Find(crate::find::FindMessage),

    // ── Toolbar / activity / revset ─────────────────────────────────────
    /// Toolbar "Refresh": force a working-copy snapshot + full graph reload.
    ToolbarRefresh,
    /// Toolbar "Fetch" (main button or a caret-menu item).
    Fetch(FetchTarget),
    /// A fetch finished: captured output lines, or an error. Tab-addressed so a
    /// fetch that completes after a tab switch resolves against the right log.
    /// Carries the target so the activity's result summary can name it.
    FetchCompleted(
        TabId,
        activity::ActivityId,
        FetchTarget,
        Box<Result<Vec<String>, String>>,
    ),
    /// Toolbar "Undo": revert the latest jj operation.
    Undo,
    /// An undo finished.
    UndoCompleted(
        TabId,
        activity::ActivityId,
        Box<Result<Vec<String>, String>>,
    ),
    /// Revset input edited.
    RevsetChanged(String),
    /// Revset submitted (Enter) — re-evaluate the log.
    RevsetSubmit,
    /// Open a toolbar dropdown (fetch branches / revset presets), anchored
    /// edge-to-edge below the carried trigger rect.
    OpenToolbarMenu(ToolbarMenu, iced::Rectangle),
    /// Popup-menu messages — see [`crate::menu::MenuMessage`].
    Menu(crate::menu::MenuMessage),
    /// Ancestry check for a bookmark move resolved: `true` = backwards or
    /// sideways (the jj CLI would refuse without `--allow-backwards`) → raise
    /// the confirmation dialog; `false` (or check failure) → run it. Carries
    /// the fully-wired mutation either way.
    BookmarkMoveChecked(Box<crate::PendingMutation>, Box<Result<bool, String>>),
    /// Confirmation dialog: run the held mutation.
    ConfirmAccept,
    /// Confirmation dialog: dismiss, resolving the held activity as canceled.
    ConfirmCancel,
    /// Swallow clicks on the confirmation card so they don't hit the scrim.
    ConfirmNoOp,
    /// Open/close the activity popover.
    ActivityToggle,
    /// Expand/collapse one activity row's captured output.
    ActivityExpand(activity::ActivityId),
    /// A caret/selection/scroll action on an expanded row's read-only output
    /// editor (edit actions are dropped before reaching the buffer).
    ActivityDetailAction(activity::ActivityId, text_editor::Action),
    /// Clear finished activities from the active tab's log.
    ActivityClear,
    /// Swallow clicks on the activity card / dropdown so they don't dismiss it.
    ActivityNoOp,
    /// Open a URL (from an activity's remote output) in the default browser.
    OpenUrl(String),
    /// Cursor entered/left a caret control — drives its hover highlight.
    SetHover(Option<HoverTarget>),

    // ── GitHub PR tabs ──────────────────────────────────────────────────
    /// PR header metadata (`gh pr view`) for the streaming PR load tagged with
    /// its version (the `session.load` cursor guard, like `CommitsBatch`).
    PrMetaLoaded(u64, Box<Result<github::PrInfo, String>>),
    /// One batch of completed files off the PR diff stream.
    PrFilesBatch(u64, Vec<DiffFile>),
    /// The PR's commit list (`gh pr view --json commits`), for the sidebar.
    PrCommitsLoaded(u64, Box<Result<Vec<github::PrCommit>, String>>),
    /// The PR diff stream ended — every file was emitted, or it failed.
    PrFinished(u64, Box<Result<(), String>>),

    /// Background syntax highlighting finished for one file: sparse
    /// `(hunk, line, spans)` for the document identified by the leading
    /// `document_id` (routed to whichever session still shows it — active or
    /// stashed — and dropped once that document is gone).
    FileHighlighted(u64, usize, Vec<(usize, usize, Vec<SyntaxSpan>)>),

    // ── Source browser ──────────────────────────────────────────────────
    /// Toolbar view switcher: show the diff or the source browser. Switching
    /// to Source with no browsed revision yet browses the selected revision,
    /// jumped to the diff's selected file.
    SetMainView(MainView),
    /// The diff view's per-file-header browse button: open the source browser
    /// at the shown revision, jumped to this file (by document file index —
    /// the widget callback can't capture the revision; the context-menu
    /// entry points dispatch through `MenuAction::BrowseSource` instead).
    BrowseFileFromDiff(usize),
    /// The browsed revision's file listing finished. Tab-addressed and
    /// version-guarded (`SourceState::version`) like every per-tab load.
    SourceTreeLoaded(
        TabId,
        u64,
        Box<Result<Vec<diffui_core::SourceEntry>, String>>,
    ),
    /// One file's contents finished loading for the source browser. Carries
    /// the path so a result for a superseded selection is dropped.
    SourceFileLoaded(
        TabId,
        u64,
        String,
        Box<Result<diffui_core::SourceFileLoad, String>>,
    ),
    /// A lazy one-level listing of an ignored directory finished (the user
    /// expanded its unenumerated row). Carries the dir path it lists.
    SourceDirLoaded(
        TabId,
        u64,
        String,
        Box<Result<Vec<diffui_core::SourceEntry>, String>>,
    ),
    /// Click on a row of the source sidebar's file tree, by display index:
    /// files load into the viewer, directories toggle their collapse.
    SourceSidebarRow(usize),
    /// Click on the source sidebar's revision header row — a no-op (the row
    /// is informational), but the widget requires a message.
    SourceHeaderClicked,
    /// The sidebar's fuzzy file-search query was edited.
    SourceFilterChanged(String),
    /// Enter in the file-search box: open the best match.
    SourceFilterSubmit,
    /// Source view / source tree scroll offsets, mirrored for stash/restore
    /// like [`Message::DiffScrolled`] / [`Message::SidebarScrolled`].
    SourceScrolled(f32),
    SourceTreeScrolled(f64),
    /// Right-click on a file row in either sidebar tree (diff or source
    /// mode), by display index; opens the file context menu at the cursor.
    SidebarFileContextMenu(usize, iced::Rectangle, iced::Point),
}
