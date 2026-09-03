//! The application `Message` enum — every event the iced runtime delivers to
//! `Diffui::update`. Pulled into its own module to tame `main.rs`; variants are
//! still flat (the self-contained overlay groups get nested in a later pass).

use iced::{Point, Size, theme as iced_theme, widget::text_editor};

use diffui_core::{FetchTarget, RevisionSelection, SyntaxSpan};

use crate::theme::ThemePreference;
use crate::{HoverTarget, MainView, TabId, ToolbarMenu, activity, mutations, revision_list};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// One event from a repository actor. Everything the projection owns —
    /// graph batches, diffs, snapshots, mutations — arrives here and is folded
    /// in by [`diffui_core::Session::apply`]; the rest is routed by job to the
    /// tab that asked.
    Repo(Box<diffui_core::Event>),
    SelectFile(usize),
    SelectRowKey(revision_list::RowSelectionKey),
    /// Drop the sidebar's multi-selection marks (Esc with no overlay open,
    /// or the batch menu's "Clear selection").
    MultiSelectClear,
    /// The sidebar / diff view reported a new scroll offset. Mirrored into the
    /// active tab's state so its position is restored on the next switch back.
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
    // ── Target-mode drafts (rebase / squash destination picking) ────────
    /// Start a draft for the given source revision and enter target mode.
    DraftStart(mutations::DraftKind, RevisionSelection),
    /// Keyboard entry points (`r` / `R` / `s`): start a draft on the
    /// *selected* revision.
    DraftStartKey(mutations::DraftKind),
    /// Op-bar segmented control: change the pending placement.
    DraftPlacement(mutations::PlacementKind),
    /// `o`/`a`/`b` during a draft: apply on the keyboard candidate with that
    /// placement, or just set the placement when no candidate is armed yet.
    DraftPlacementKey(mutations::PlacementKind),
    /// Keyboard: move the destination candidate row by this delta, skipping
    /// draft sources.
    DraftCandidate(i32),
    /// Enter: execute a complete source-only merge, or apply the draft to the
    /// candidate with the current placement.
    DraftConfirm,
    /// Esc / the op bar's ✕: leave target mode without running anything.
    DraftCancel,
    /// Target mode: plain hover crossed onto a commit row (`Some`) or left
    /// the rows (`None`). Arms the row as the draft candidate — the mouse
    /// equivalent of `j`/`k`, honoring the armed placement.
    DraftHoverCandidate(Option<usize>),
    /// The preview debounce timer fired for this version — run the parked
    /// simulation if the version is still current.
    DraftPreviewKick(u64),

    // ── Revision drag & drop (sidebar) ──────────────────────────────────
    /// A drag crossed the activation threshold on this commit row: start a
    /// rebase draft with it as the source.
    RevisionDragStart(usize),
    /// The drop spot under the cursor changed during an active drag
    /// (`None` = not over a valid spot). Drives the op bar hint + preview.
    RevisionDragHover(Option<revision_list::DropSpot>),
    /// The drag released: execute on the spot, or `None` to keep the draft
    /// in click-to-pick mode.
    RevisionDragDrop(Option<revision_list::DropSpot>),

    /// Toggle the keyboard candidate as a draft source (space): stack it as
    /// another merge parent / revision to move / squash source, or un-stack
    /// it if it already is one. ⌘-click routes through `SelectRowKey`.
    DraftToggleSource,
    /// Click on an error toast — dismiss it.
    ToastDismiss(u64),
    /// Periodic prune of expired toasts; subscribed only while any are up.
    ToastTick,
    /// An activity row's "Undo" button: revert exactly that jj operation.
    UndoActivityOp(activity::ActivityId, String),
    /// Live modifier state, tracked for drop-time decisions (⌥ = move with
    /// descendants).
    ModifiersChanged(iced::keyboard::Modifiers),
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
    /// The user asked to close the window (⌘Q, the close button). winit sends
    /// no `Unfocused` for either, so this is the only chance to flush what the
    /// debounce is still holding; the handler writes, then exits.
    WindowCloseRequested,
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
    /// Toolbar "Undo": revert the latest jj operation.
    Undo,
    /// Revset input edited.
    RevsetChanged(String),
    /// Revset submitted (Enter) — re-evaluate the log.
    RevsetSubmit,
    /// Open a toolbar dropdown (fetch branches / revset presets), anchored
    /// edge-to-edge below the carried trigger rect.
    OpenToolbarMenu(ToolbarMenu, iced::Rectangle),
    /// Popup-menu messages — see [`crate::menu::MenuMessage`].
    Menu(crate::menu::MenuMessage),
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
    /// Background syntax highlighting finished for one file: sparse
    /// `(hunk, line, spans)` for the document identified by the leading
    /// `document_id` (routed to whichever tab still shows it, on screen or
    /// not, and dropped once that document is gone).
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
    /// Source view / source tree scroll offsets, mirrored per tab for the
    /// switch-back restore like [`Message::DiffScrolled`] /
    /// [`Message::SidebarScrolled`].
    SourceScrolled(f32),
    SourceTreeScrolled(f64),
    /// Right-click on a file row in either sidebar tree (diff or source
    /// mode), by display index; opens the file context menu at the cursor.
    SidebarFileContextMenu(usize, iced::Rectangle, iced::Point),
}
