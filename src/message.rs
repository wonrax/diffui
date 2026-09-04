//! The application `Message` enum — every event the iced runtime delivers to
//! `Diffui::update`.
//!
//! There are five kinds of thing in here and they are kept apart on purpose:
//! an [`Action`] is what the *user* asked for (and is the only kind a keymap,
//! a menu or the palette can produce), [`Message::Repo`] is a repository
//! actor reporting, [`UiEvent`] is widget plumbing that no one would ever bind
//! a key to, [`WindowEvent`] is the platform talking, and the overlay
//! sub-enums are each overlay's own internal chatter.

use iced::{Point, Size, theme as iced_theme, widget::text_editor};

use diffui_core::SyntaxSpan;

use crate::action::Action;
use crate::{HoverTarget, ToolbarMenu, activity, revision_list};

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// A user intent, from whichever producer resolved it. The one arm that
    /// performs work the user asked for — see [`crate::action`].
    Action(Action),
    /// One event from a repository actor. Everything the projection owns —
    /// graph batches, diffs, snapshots, mutations — arrives here and is folded
    /// in by [`diffui_core::Session::apply`]; the rest is routed by job to the
    /// tab that asked. The event carries its own `RepoId`.
    Repo(Box<diffui_core::Event>),
    /// Widget plumbing: scroll offsets, hover, debounce ticks.
    Ui(UiEvent),
    /// The platform: window geometry, focus, appearance, the title bar.
    Window(WindowEvent),
    /// Command-palette messages — see [`crate::palette::PaletteMessage`].
    Palette(crate::palette::PaletteMessage),
    /// In-diff find bar messages — see [`crate::find::FindMessage`].
    Find(crate::find::FindMessage),
    /// Popup-menu messages — see [`crate::menu::MenuMessage`].
    Menu(crate::menu::MenuMessage),
}

/// Everything a widget reports that is *not* an intent: positions, hovers,
/// debounce ticks, and the clicks whose meaning depends on state only the
/// handler can resolve.
#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    /// A click on a revision row. Whether that selects, marks, or arms a draft
    /// candidate depends on the modifiers and the mode, so it stays an event.
    SelectRowKey(revision_list::RowSelectionKey),
    /// A click on a file-tree row in the sidebar, by *display* row index
    /// (the flattened tree, not the document file index): a file row
    /// selects that file, a directory row toggles its collapse.
    SidebarFileRow(usize),
    /// The sidebar / diff view reported a new scroll offset. Mirrored into the
    /// active tab's state so its position is restored on the next switch back.
    SidebarScrolled(f64),
    DiffScrolled(f32),
    /// Source view / source tree scroll offsets, mirrored per tab for the
    /// switch-back restore like [`UiEvent::DiffScrolled`].
    SourceScrolled(f32),
    SourceTreeScrolled(f64),
    /// Cursor entered/left a caret control — drives its hover highlight.
    SetHover(Option<HoverTarget>),
    /// Click on an error toast — dismiss it.
    ToastDismiss(u64),
    /// Periodic prune of expired toasts; subscribed only while any are up.
    ToastTick,
    /// Periodic tick while a load is in flight. No-op handler — it exists only
    /// to keep `view()` re-running so the loading indicator can appear after
    /// its grace period and animate.
    LoadingTick,
    /// Right-click on a revision row — opens the context menu. Carries the row's
    /// on-screen rect (window-content points) so the glow can be anchored over
    /// it, plus the cursor point the menu opens at.
    RevisionContextMenu(revision_list::RowSelectionKey, iced::Rectangle, iced::Point),
    /// Right-click on a file row in either sidebar tree (diff or source
    /// mode), by display index; opens the file context menu at the cursor.
    SidebarFileContextMenu(usize, iced::Rectangle, iced::Point),
    /// Open a toolbar dropdown (fetch branches / revset presets), anchored
    /// edge-to-edge below the carried trigger rect.
    OpenToolbarMenu(ToolbarMenu, iced::Rectangle),
    DescriptionAction(text_editor::Action),
    /// Expand/collapse one activity row's captured output.
    ActivityExpand(activity::ActivityId),
    /// A caret/selection/scroll action on an expanded row's read-only output
    /// editor (edit actions are dropped before reaching the buffer).
    ActivityDetailAction(activity::ActivityId, text_editor::Action),
    /// Swallow clicks on the activity card / dropdown so they don't dismiss it.
    ActivityNoOp,
    /// Swallow clicks on the confirmation card so they don't hit the scrim.
    ConfirmNoOp,
    /// Swallow clicks on the open-repo card so they don't dismiss it.
    OpenRepoNoOp,
    OpenRepoPathChanged(String),
    /// Revset input edited.
    RevsetChanged(String),
    SidebarWidthChanged(f32),
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
    /// The diff view's per-file-header browse button, by document file index —
    /// the widget callback can't capture the revision, so the handler resolves
    /// it before performing [`Action::BrowseSource`].
    BrowseFileFromDiff(usize),
    /// A drag crossed the activation threshold on this commit row: start a
    /// rebase draft with it as the source.
    RevisionDragStart(usize),
    /// The drop spot under the cursor changed during an active drag
    /// (`None` = not over a valid spot). Drives the op bar hint + preview.
    RevisionDragHover(Option<revision_list::DropSpot>),
    /// The drag released: execute on the spot, or `None` to keep the draft
    /// in click-to-pick mode.
    RevisionDragDrop(Option<revision_list::DropSpot>),
    /// Target mode: plain hover crossed onto a commit row (`Some`) or left
    /// the rows (`None`). Arms the row as the draft candidate — the mouse
    /// equivalent of `j`/`k`, honoring the armed placement.
    DraftHoverCandidate(Option<usize>),
    /// The preview debounce timer fired for this version — run the parked
    /// simulation if the version is still current.
    DraftPreviewKick(u64),
    /// Background syntax highlighting finished for one file: sparse
    /// `(hunk, line, spans)` for the document identified by the leading
    /// `document_id` (routed to whichever tab still shows it, on screen or
    /// not, and dropped once that document is gone).
    FileHighlighted(u64, usize, Vec<(usize, usize, Vec<SyntaxSpan>)>),
}

/// The platform half: window geometry and lifecycle, live modifier state, and
/// the OS appearance.
#[derive(Debug, Clone)]
pub(crate) enum WindowEvent {
    /// A raw key press, before the keymap has looked at it. `consumed` is
    /// iced's report that a focused widget already took the key — the keymap
    /// uses it to leave a text input's typing alone. Resolution happens in
    /// `update`, where the mode stack and the keymap are both in reach.
    KeyPressed {
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        consumed: bool,
    },
    /// The window finished opening: carries its initial outer position (absent
    /// on Wayland) and inner size. Seeds geometry tracking without marking it
    /// dirty — the restored state is already on disk.
    Opened(Option<Point>, Size),
    Resized(Size),
    Moved(Point),
    FocusChanged(bool),
    /// The user asked to close the window (⌘Q, the close button). winit sends
    /// no `Unfocused` for either, so this is the only chance to flush what the
    /// debounce is still holding; the handler writes, then exits.
    CloseRequested,
    /// Debounce tick: persist the window geometry + sidebar width once the
    /// changes have settled. Subscribed only while a change is pending.
    PersistState,
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
    /// Live modifier state, tracked for drop-time decisions (⌥ = move with
    /// descendants).
    ModifiersChanged(iced::keyboard::Modifiers),
    SystemThemeChanged(iced_theme::Mode),
    /// Poll tick (macOS, System theme only): re-read the live OS appearance and
    /// re-resolve if it changed. Covers winit going silent once iced pins the
    /// window appearance — see [`crate::chrome::system_appearance`].
    PollSystemTheme,
}

impl From<Action> for Message {
    fn from(action: Action) -> Self {
        Message::Action(action)
    }
}

impl From<UiEvent> for Message {
    fn from(event: UiEvent) -> Self {
        Message::Ui(event)
    }
}

impl From<WindowEvent> for Message {
    fn from(event: WindowEvent) -> Self {
        Message::Window(event)
    }
}
