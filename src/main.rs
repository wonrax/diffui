use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant},
};

mod activity;
mod backend;
mod chrome;
mod config;
mod diff_panel;
mod diff_view;
mod find;
mod git;
mod graph;
mod graph_layout;
mod graph_view;
mod jj;
#[cfg(target_os = "macos")]
mod macos_native;
mod mutations;
mod palette;
mod repository;
mod resize_handle;
mod revision_list;
mod scrollbar;
mod sidebar;
mod tab_bar;
mod theme;
mod toolbar;
mod window_state;

/// Profiling-only global allocator (enabled by the `track-alloc` feature). It
/// forwards every request to the system allocator while tracking the live byte
/// count and its high-water mark, so a measurement harness can compare the
/// retained working set against the transient peak during a load. The atomics
/// add real per-alloc cost — this is never compiled into a normal build.
#[cfg(feature = "track-alloc")]
pub(crate) mod track_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    pub static CURRENT: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);

    fn add(size: usize) {
        let now = CURRENT.fetch_add(size, Relaxed) + size;
        PEAK.fetch_max(now, Relaxed);
    }

    pub struct Tracking;

    // SAFETY: every method delegates to the system allocator with the same
    // layout/pointer it was handed; the only extra work is updating counters.
    unsafe impl GlobalAlloc for Tracking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                add(layout.size());
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                add(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            CURRENT.fetch_sub(layout.size(), Relaxed);
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                if new_size >= layout.size() {
                    add(new_size - layout.size());
                } else {
                    CURRENT.fetch_sub(layout.size() - new_size, Relaxed);
                }
            }
            new_ptr
        }
    }
}

#[cfg(feature = "track-alloc")]
#[global_allocator]
static GLOBAL: track_alloc::Tracking = track_alloc::Tracking;

// mimalloc returns freed memory to the OS far more eagerly than macOS's system
// allocator, which otherwise parks a load's transient high-water mark and keeps
// RSS pinned near the peak long after the working set shrinks. The profiler
// (`track-alloc`) overrides this with its own counting allocator.
#[cfg(not(feature = "track-alloc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use backend::{
    BackendOutput, BookmarksInfo, BranchStatus, CommitStore, CommitsTail, DiffDocument,
    LoadProgress, RevisionDetails, RevisionSelection, RowView, SignatureInfo, StreamRow,
    compute_empty_status, load_backend, load_diff, load_repository_snapshot,
};
use clap::Parser;
use config::AppConfig;
use find::FindState;
use futures::{SinkExt, Stream, StreamExt};
use iced::theme as iced_theme;
use iced::{
    Element, Length, Point, Size, Subscription, Task, Theme, alignment,
    event::{self, Event},
    keyboard, system, time,
    widget::{self, column, container, row, stack, text},
    window,
};
use notify::Watcher;
use palette::{
    ColumnSource, CommandId as PaletteCommand, PaletteState, Recents, ResultRef,
    change_id_for_recents, revision_selection,
};
use repository::{Repository, RepositorySnapshot, Vcs, prepare_repository};
use resize_handle::ResizeHandle;
use theme::{
    ResolvedTheme, ThemePreference, ThemeSpec, app_shell_style, horizontal_divider,
    vertical_divider,
};
use window_state::WindowState;

/// Quiet period after the last filesystem event before we refresh. A single
/// editor save typically emits a burst of events; coalescing them avoids
/// snapshotting several times for one logical change.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// Debounce before re-running the palette matcher. It scans every commit, so
/// coalescing fast typing keeps the input responsive on large repos.
const PALETTE_QUERY_DEBOUNCE: Duration = Duration::from_millis(120);

/// Quiet period after the last window move/resize (or sidebar drag) before the
/// geometry is written to disk. A single drag emits a burst of events;
/// coalescing them into one write avoids hammering the disk every frame.
const WINDOW_STATE_DEBOUNCE: Duration = Duration::from_millis(400);

/// How many recently-opened repo roots to remember for the open dialog's
/// quick-pick list. Bounded so the persisted state file stays small.
const RECENT_REPOS_MAX: usize = 12;

fn main() -> iced::Result {
    let cli = Cli::parse();

    // Restore the last window geometry before the window is created. The
    // sidebar split lives in app state, so it's restored later in
    // `Diffui::new`; `saved` is handed to the boot closure for that.
    let saved = WindowState::load();
    let mut window_settings = window::Settings::default();
    if let Some((width, height)) = saved.size() {
        window_settings.size = Size::new(width, height);
    }
    if let Some((x, y)) = saved.position() {
        window_settings.position = window::Position::Specific(Point::new(x, y));
    }
    // Platform window chrome (e.g. macOS transparent title bar so the tab strip
    // sits inline with the traffic lights). See `chrome`.
    chrome::apply_window_settings(&mut window_settings);

    iced::application(
        move || Diffui::new(cli.clone(), saved.clone()),
        Diffui::update,
        Diffui::view,
    )
    .title("diffui")
    .window(window_settings)
    .subscription(Diffui::subscription)
    .theme(Diffui::theme)
    .run()
}

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Native GUI diff viewer for jj and git")]
struct Cli {
    /// One or more repository paths to open as tabs. Defaults to the current
    /// directory when none are given.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct Diffui {
    pub(crate) repository: Option<Repository>,
    pub(crate) status: LoadStatus,
    pub(crate) document: DiffDocument,
    pub(crate) commits: CommitStore,
    pub(crate) selected_revision: RevisionSelection,
    /// Sticky inline-file-list preference. Always reflects whether the
    /// *selected* revision's file list is shown; the user toggles it by
    /// re-clicking the selected row, and the value persists across
    /// revision switches so collapsing once stays collapsed for whatever
    /// revision the user moves to next.
    pub(crate) file_list_expanded: bool,
    pub(crate) pending_revision: Option<RevisionSelection>,
    pub(crate) repository_snapshot: Option<RepositorySnapshot>,
    pub(crate) snapshot_pending: bool,
    pub(crate) app_focused: bool,
    pub(crate) selected_theme: ThemePreference,
    pub(crate) system_theme: iced_theme::Mode,
    pub(crate) selected_file: usize,
    pub(crate) sidebar_width: f32,
    /// Cached result of `sidebar::min_width(config)`. The min width is
    /// purely a function of `config.ui_font` + `config.mono_font` glyph
    /// advances at `CAPTION_TEXT_SIZE`, which are stable for the life of
    /// the app — caching avoids re-shaping six strings on every `view()`
    /// rebuild and on every drag tick of the resize handle.
    pub(crate) sidebar_min_width: f32,
    /// Last window geometry seen from the compositor: inner (client) size and
    /// outer top-left position. Updated on `Opened`/`Resized`/`Moved` and
    /// written back — debounced — so the next launch restores it. Position is
    /// `None` until the window reports one (and stays `None` on Wayland, which
    /// doesn't report window positions).
    pub(crate) window_size: Size,
    pub(crate) window_position: Option<Point>,
    /// Set when the window geometry or sidebar width changes; cleared once the
    /// new value is persisted. While it's `Some`, the subscription runs a
    /// debounce timer whose tick flushes the geometry to disk after the
    /// changes settle. `None` when nothing is pending.
    pub(crate) geometry_dirty_since: Option<Instant>,
    pub(crate) config: AppConfig,
    pub(crate) revision_details: Option<RevisionDetails>,
    /// Working-copy branch summary (nearest local bookmark + ahead/behind vs
    /// its tracked upstream) for the sidebar footer. `None` until a load
    /// resolves it, or when `@` has no local bookmark in its ancestry.
    pub(crate) branch_status: Option<BranchStatus>,
    /// Repo-wide bookmark table (local targets + per-remote tracking state),
    /// loaded alongside the graph. Drives the revision context menu's
    /// move/track/delete/push actions. Empty for git repos.
    pub(crate) bookmarks: BookmarksInfo,
    /// `None` when closed; a non-empty column stack when open.
    pub(crate) palette: Option<PaletteState>,
    /// In-session recents (revisions + commands) used to score palette
    /// matches. Persisted to the XDG data dir between sessions.
    pub(crate) recents: Recents,
    /// `None` when the in-diff find bar is closed.
    pub(crate) find: Option<FindState>,
    /// Bumped whenever the user picks a revision through a path that
    /// doesn't go through the revision list (currently: the palette).
    /// The sidebar's `RevisionList` watches this and scrolls the matching
    /// row into view on disagreement.
    pub(crate) revision_reveal_token: u64,
    /// True while a palette-initiated revision load is in flight. We
    /// can't bump `revision_reveal_token` at the moment the user accepts
    /// the result — at that point `selected_revision` is still the old
    /// value, so the sidebar would scroll the wrong row into view. The
    /// `BackendLoaded` handler reads this flag once the new revision has
    /// actually been written into `selected_revision`, *then* bumps the
    /// token so the next render reveals the correct row.
    pub(crate) pending_revision_reveal: bool,
    /// Bumped when `commits` is replaced; tags background empty-status results
    /// so a result from a superseded load is dropped.
    pub(crate) commits_version: u64,
    /// Compact run-length lane store + shortest-unique-prefix lengths. The
    /// sidebar renders rows on demand from these, so it never materializes the
    /// whole (up to ~1M-row) list. A streaming cold load appends to both in
    /// place per `CommitsBatch`; a refresh swaps them wholesale.
    pub(crate) graph: graph_layout::GraphLayout,
    pub(crate) sidebar_prefix_lens: Vec<usize>,
    /// Index of the selected commit in `commits` (drives the reveal-on-jump
    /// scroll and the expanded file-list span), recomputed on selection change
    /// so `view()` stays O(visible rows).
    pub(crate) selected_commit_index: Option<usize>,
    /// Live progress of the in-flight commit-graph load, read by `view()` to
    /// render the startup progress indicator.
    pub(crate) commit_progress: LoadProgress,
    /// When the current load began, or `None` when idle. Drives the ~500ms
    /// grace period before a loading indicator is shown (so fast loads don't
    /// flash one).
    pub(crate) loading_since: Option<Instant>,
    /// Session cache of resolved empty status keyed by commit-id. A commit's
    /// emptiness never changes, so background results computed once survive
    /// reloads — only newly-seen merges are recomputed.
    pub(crate) empty_cache: HashMap<String, bool>,
    /// Append state for an in-flight *streaming* cold load (jj only). `None`
    /// whenever no stream is running (idle, or after a refresh that swaps the
    /// graph atomically instead).
    pub(crate) load: Option<LoadCursor>,

    // ── Multi-repo ──────────────────────────────────────────────────────
    /// Every open repository, in tab order. The *active* tab's heavy view
    /// state lives in the inline fields above; every other tab keeps its in
    /// `Tab::stash`. Switching tabs swaps the two. Empty ⇒ the empty-state
    /// view owns the window.
    pub(crate) tabs: Vec<Tab>,
    /// Index into `tabs` of the active tab. Meaningless when `tabs` is empty.
    pub(crate) active_tab: usize,
    /// Monotonic source of `TabId`s, so a tab keeps a stable identity even as
    /// its index shifts when other tabs open/close.
    pub(crate) next_tab_id: u64,
    /// Monotonic source of streaming-load `version`s. Each (re)load gets a
    /// fresh value so late batches from a superseded or backgrounded load are
    /// dropped by the version guard rather than corrupting the active tab.
    pub(crate) next_load_version: u64,
    /// `Some` while the "open repository" path dialog is showing.
    pub(crate) open_repo_dialog: Option<OpenRepoDialog>,
    /// Most-recently-opened repo roots (newest first), surfaced as quick-pick
    /// rows in the open dialog. Seeded from / persisted to `WindowState`.
    pub(crate) recent_repos: Vec<String>,

    // ── Toolbar / activity / revset (per-tab where noted) ───────────────
    /// The revset (jj) / revision-range (git) controlling which commits the
    /// log shows. Per-tab; persisted per repo root. Empty or `all()` is the
    /// default (every visible head's ancestry).
    pub(crate) revset: String,
    /// The active tab's activity log (long-running ops: load, refresh, revset
    /// eval, fetch, undo, push). Per-tab; inactive tabs keep theirs in
    /// `RepoState::activities`.
    pub(crate) activities: activity::ActivityLog,
    /// The activity wrapping the in-flight graph (re)load, finished when the
    /// terminal load message arrives. Per-tab so a backgrounded load's entry is
    /// resolved against the right log.
    pub(crate) pending_load_activity: Option<activity::ActivityId>,
    /// Monotonic source of `ActivityId`s across every tab.
    pub(crate) next_activity_id: u64,
    /// Open toolbar dropdown (fetch-branches / revset-presets), if any.
    pub(crate) toolbar_menu: Option<ToolbarMenu>,
    /// Whether the activity popover is showing.
    pub(crate) activity_popover_open: bool,
    /// The caret control the cursor is currently over, if any — drives the
    /// hover highlight that `mouse_area` (unlike `button`) doesn't provide.
    pub(crate) hovered: Option<HoverTarget>,
}

/// Which toolbar dropdown is open. Both render as iced overlays anchored near
/// their trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolbarMenu {
    /// The fetch split-button's caret: one row per known remote branch.
    FetchBranches,
    /// The revset input's caret: built-in preset revsets.
    RevsetPresets,
}

/// A `mouse_area`-based control whose hover state we track manually (the caret
/// triggers can't be `button`s — they must open their menu on mouse-down for
/// the native menu's press-drag-release — so they don't get button hover for
/// free). At most one is hovered at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoverTarget {
    /// The Fetch split button's caret half (the main half uses `button`'s own
    /// hover state; only the `mouse_area` caret needs manual tracking).
    FetchCaret,
    RevsetCaret,
}

/// What a fetch should pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchTarget {
    /// Every remote, all branches (`jj git fetch --all-remotes` /
    /// `git fetch --all`).
    AllRemotes,
    /// One branch on one remote (`name@remote`).
    RemoteBranch { remote: String, branch: String },
}

/// Per-repository view state. The active tab's copy is spread across the
/// `Diffui` inline fields (so the view/update code reads it as flat fields);
/// inactive tabs keep theirs here, stashed in `Tab::stash`. The field set is
/// the exact per-repo subset of `Diffui` — `stash_active_state` /
/// `restore_active_state` move every field between the two, so the two lists
/// MUST stay in sync.
#[derive(Debug, Clone)]
pub(crate) struct RepoState {
    pub(crate) repository: Option<Repository>,
    pub(crate) status: LoadStatus,
    pub(crate) document: DiffDocument,
    pub(crate) commits: CommitStore,
    pub(crate) selected_revision: RevisionSelection,
    pub(crate) file_list_expanded: bool,
    pub(crate) pending_revision: Option<RevisionSelection>,
    pub(crate) repository_snapshot: Option<RepositorySnapshot>,
    pub(crate) snapshot_pending: bool,
    pub(crate) selected_file: usize,
    pub(crate) revision_details: Option<RevisionDetails>,
    pub(crate) branch_status: Option<BranchStatus>,
    pub(crate) bookmarks: BookmarksInfo,
    pub(crate) revision_reveal_token: u64,
    pub(crate) pending_revision_reveal: bool,
    pub(crate) commits_version: u64,
    pub(crate) graph: graph_layout::GraphLayout,
    pub(crate) sidebar_prefix_lens: Vec<usize>,
    pub(crate) selected_commit_index: Option<usize>,
    pub(crate) commit_progress: LoadProgress,
    pub(crate) loading_since: Option<Instant>,
    pub(crate) empty_cache: HashMap<String, bool>,
    pub(crate) load: Option<LoadCursor>,
    pub(crate) revset: String,
    pub(crate) activities: activity::ActivityLog,
    pub(crate) pending_load_activity: Option<activity::ActivityId>,
}

impl RepoState {
    /// A never-loaded tab for `repository`: empty graph, `Loading` status, no
    /// task in flight. `ensure_active_loaded` kicks the real load when this
    /// becomes the active tab (`status != Loaded`). `revset` is the persisted
    /// (or default) filter for this repo.
    fn unloaded(repository: Option<Repository>, revset: String) -> Self {
        Self {
            repository,
            status: LoadStatus::Loading,
            document: DiffDocument::default(),
            commits: CommitStore::default(),
            selected_revision: RevisionSelection::WorkingCopy,
            file_list_expanded: true,
            pending_revision: None,
            repository_snapshot: None,
            snapshot_pending: false,
            selected_file: 0,
            revision_details: None,
            branch_status: None,
            bookmarks: BookmarksInfo::default(),
            revision_reveal_token: 0,
            pending_revision_reveal: false,
            commits_version: 0,
            graph: graph_layout::GraphLayout::default(),
            sidebar_prefix_lens: Vec::new(),
            selected_commit_index: None,
            commit_progress: LoadProgress::default(),
            loading_since: None,
            empty_cache: HashMap::new(),
            load: None,
            revset,
            activities: activity::ActivityLog::default(),
            pending_load_activity: None,
        }
    }

    /// The inline state when no repository is open at all (closed the last
    /// tab). `Loaded` so `view()` doesn't try to show a loading indicator
    /// behind the empty state.
    fn empty() -> Self {
        Self {
            status: LoadStatus::Loaded,
            ..Self::unloaded(None, String::new())
        }
    }
}

/// The default revset for a freshly-opened repo of `vcs`, before any persisted
/// value is applied: jj shows `all()` (every visible head's ancestry — the
/// current hardcoded behavior); git falls back to its `git log` default (the
/// current branch's history), expressed as an empty range.
fn default_revset(vcs: Vcs) -> String {
    match vcs {
        Vcs::Jj => "all()".to_owned(),
        Vcs::Git => String::new(),
    }
}

/// Built-in revset presets for the caret menu, as `(label, expression)`. jj
/// uses revset functions; git uses `git log` revision-range shortcuts. Shared
/// by the native macOS menu and the iced fallback so they stay in sync.
pub(crate) fn revset_presets(vcs: Option<Vcs>) -> &'static [(&'static str, &'static str)] {
    match vcs {
        Some(Vcs::Git) => &[("All branches", "--all"), ("Current", "HEAD")],
        _ => &[
            ("Everything", "all()"),
            ("Mine", "mine()"),
            ("Current line", "ancestors(@)"),
            ("Conflicts", "conflicts()"),
        ],
    }
}

/// A stable identity for an open tab, independent of its position in `tabs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TabId(pub(crate) u64);

/// One open repository: its identity + display metadata, plus the stashed
/// per-repo state while it's inactive. The active tab's `stash` is `None` —
/// its state is checked out into the `Diffui` inline fields.
#[derive(Debug, Clone)]
pub(crate) struct Tab {
    pub(crate) id: TabId,
    /// Dimmed prefix in the tab label — the repo root's parent directory.
    pub(crate) owner: String,
    /// Emphasized repo name — the repo root's directory name.
    pub(crate) name: String,
    pub(crate) vcs: Vcs,
    /// Repository root, used to de-duplicate opens and key the watcher.
    pub(crate) root: PathBuf,
    /// `None` for the active tab (state is inline); `Some` for an inactive
    /// tab (loaded, or a fresh `RepoState::unloaded`).
    pub(crate) stash: Option<RepoState>,
}

/// Transient state of the open-repository path dialog.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenRepoDialog {
    pub(crate) path: String,
    /// Populated when the last submit failed to resolve a repository, so the
    /// dialog stays open with the reason shown.
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoadStatus {
    Loading,
    Loaded,
    Failed(String),
}

/// What triggered a repository refresh — decides how much we reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshOrigin {
    /// The filesystem watcher (a working-tree file edit). It ignores `.jj`/
    /// `.git`, so the change is always a working-copy tree edit — topology is
    /// unchanged, so we skip the full graph re-walk and just reload the diff.
    Watcher,
    /// Focus regain or a manual "Refresh repository" command. These can follow
    /// an external jj op (rebase, new, bookmark move) that *did* change
    /// topology, so they do the full reload.
    Focus,
}

/// Carries the transient builder state of a streaming cold load across the
/// `CommitsBatch` messages: the author interner and the lane fold, which both
/// must persist between batches as rows append to `commits` / `graph`. The
/// `version` is the `commits_version` the stream was started under — batches
/// tagged with a stale version (e.g. a refresh superseded the stream) are
/// dropped. Freed when the stream finishes, so none of it sticks around.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoadCursor {
    version: u64,
    interner: HashMap<String, u32>,
    fold: graph_layout::LaneFoldState,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    BackendLoaded(RevisionSelection, Box<Result<BackendOutput, String>>),
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
    DiffLoaded(
        RevisionSelection,
        Box<Result<(DiffDocument, Option<RevisionDetails>), String>>,
    ),
    RepositorySnapshotLoaded(RefreshOrigin, Result<RepositorySnapshot, String>),
    /// Background-resolved empty status for the merge/root commits the loader
    /// left unknown, tagged with the `commits_version` it was computed against
    /// so results from a superseded load are dropped.
    EmptyStatusComputed(u64, Vec<(usize, bool)>),
    SelectFile(usize),
    SelectRowKey(revision_list::RowSelectionKey),
    /// Right-click on a revision row — opens the native context menu. Carries
    /// the row's on-screen rect (window-content points) so the native glow can
    /// be anchored over it while the menu is open.
    RevisionContextMenu(revision_list::RowSelectionKey, iced::Rectangle),
    /// A context-menu mutation (new/edit/abandon/bookmark/push) finished,
    /// tab-addressed with its activity id so push remote output lands in the
    /// right log.
    MutationCompleted(
        TabId,
        activity::ActivityId,
        Box<Result<mutations::MutationOutcome, String>>,
    ),
    SelectTheme(ThemePreference),
    SystemThemeChanged(iced_theme::Mode),
    WindowFocusChanged(bool),
    RefreshRepository,
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
    PaletteOpen,
    PaletteClose,
    PaletteQueryChanged(String),
    /// Fired after the query debounce. Carries `(column depth, query version)`
    /// so a recompute is dropped if the user has typed past it.
    PaletteRecompute(usize, u64),
    /// Move the highlighted result by `±1`. `-1` for up, `+1` for down.
    PaletteMoveSelection(i32),
    /// Set the highlighted index to a specific row (used by hover).
    PaletteSelectIndex(usize),
    /// Enter / on_submit on the input. Acts on the currently highlighted
    /// row.
    PaletteAccept,
    /// Click on a specific row — same as Accept but with an explicit
    /// index to defend against the click landing during a re-render.
    PaletteAcceptIndex(usize),
    /// Tab: push an actions column for the highlighted result.
    PalettePushActions,
    /// Esc / Backspace at empty: pop the rightmost column. Closes the
    /// palette when the stack would be left empty.
    PalettePopColumn,
    /// Drop-floor for events that need to be captured but produce no
    /// state change — e.g. scroll wheel ticks on the palette scrim, so
    /// they don't fall through to the diff view behind.
    PaletteNoOp,
    /// Per-frame tick that drives the column push/pop slide animation.
    /// Subscribed only while `PaletteState::is_animating` is true. The
    /// handler is a no-op — the goal is just to keep iced rendering each
    /// frame so the view can sample the animation's interpolated value.
    PaletteTick,
    /// Open the in-diff find bar (⌘F / Ctrl+F).
    FindOpen,
    FindClose,
    FindQueryChanged(String),
    /// Fired after the debounce delay. The version cookie lets us drop
    /// the result if the user has typed past it.
    FindRecompute(u64),
    FindToggleCase,
    FindToggleRegex,
    /// Enter: advance to the next match (wraps around).
    FindNext,
    /// Shift+Enter: advance to the previous match.
    FindPrev,

    // ── Toolbar / activity / revset ─────────────────────────────────────
    /// Toolbar "Refresh": force a working-copy snapshot + full graph reload.
    ToolbarRefresh,
    /// Toolbar "Fetch" (main button or a caret-menu item).
    Fetch(FetchTarget),
    /// A fetch finished: captured output lines, or an error. Tab-addressed so a
    /// fetch that completes after a tab switch resolves against the right log.
    FetchCompleted(
        TabId,
        activity::ActivityId,
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
    /// A revset preset was picked from the caret menu.
    RevsetPreset(String),
    /// Open a toolbar dropdown (fetch branches / revset presets).
    OpenToolbarMenu(ToolbarMenu),
    /// Close any open toolbar dropdown.
    CloseToolbarMenu,
    /// Open/close the activity popover.
    ActivityToggle,
    /// Expand/collapse one activity row's captured output.
    ActivityExpand(activity::ActivityId),
    /// Clear finished activities from the active tab's log.
    ActivityClear,
    /// Swallow clicks on the activity card / dropdown so they don't dismiss it.
    ActivityNoOp,
    /// Open a URL (from an activity's remote output) in the default browser.
    OpenUrl(String),
    /// Cursor entered/left a caret control — drives its hover highlight.
    SetHover(Option<HoverTarget>),
}

impl Diffui {
    fn new(cli: Cli, saved: WindowState) -> (Self, Task<Message>) {
        let config = AppConfig::load();
        let sidebar_min_width = sidebar::min_width(config);
        // Restore the persisted sidebar split and window geometry. The sidebar
        // is clamped to its min so a stale width from a narrower font config
        // can't leave it unusable. The window size/position seed the in-memory
        // tracking; the compositor's `Opened` event overwrites them with the
        // real values a frame later, but seeding keeps them correct in between.
        let sidebar_width = saved
            .sidebar_width
            .filter(|w| w.is_finite() && *w > 0.0)
            .unwrap_or(sidebar::DEFAULT_WIDTH)
            .max(sidebar_min_width);
        let window_size = saved
            .size()
            .map(|(w, h)| Size::new(w, h))
            .unwrap_or_else(|| window::Settings::default().size);
        let window_position = saved.position().map(|(x, y)| Point::new(x, y));

        // Repositories to open: explicit CLI paths win; otherwise restore last
        // session's open repos; otherwise the current directory. Unresolvable
        // paths are skipped (keeping the first error so a single bad path still
        // surfaces a message); the survivors each become a tab.
        let requested: Vec<PathBuf> = if !cli.paths.is_empty() {
            cli.paths
        } else if !saved.open_repos.is_empty() {
            saved.open_repos.iter().map(PathBuf::from).collect()
        } else {
            vec![PathBuf::from(".")]
        };
        let mut repositories = Vec::new();
        let mut first_error = None;
        for path in &requested {
            match prepare_repository(path) {
                Ok(repository) => repositories.push(repository),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!("{error:#}"));
                    }
                }
            }
        }

        // Re-focus the tab that was active last session (matched by repo root),
        // falling back to the first.
        let active_index = saved
            .active_repo
            .as_deref()
            .and_then(|active| {
                repositories
                    .iter()
                    .position(|repository| repository.root.to_string_lossy() == active)
            })
            .unwrap_or(0);

        // Resolve a repo's persisted revset (keyed by root), else its default.
        let revset_for = |repository: &Repository| -> String {
            saved
                .revsets
                .get(&repository.root.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| default_revset(repository.vcs))
        };

        let mut next_tab_id = 0u64;
        let mut tabs = Vec::with_capacity(repositories.len());
        for (index, repository) in repositories.iter().enumerate() {
            let (owner, name) = repo_label(&repository.root);
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            // The active tab's state lives inline (no stash); the rest start
            // unloaded and load lazily on first activation.
            let stash = if index == active_index {
                None
            } else {
                Some(RepoState::unloaded(
                    Some(repository.clone()),
                    revset_for(repository),
                ))
            };
            tabs.push(Tab {
                id,
                owner,
                name,
                vcs: repository.vcs,
                root: repository.root.clone(),
                stash,
            });
        }

        let active_repository = repositories.get(active_index).cloned();
        let active_revset = active_repository
            .as_ref()
            .map(revset_for)
            .unwrap_or_default();
        let active_tab = if repositories.is_empty() {
            0
        } else {
            active_index
        };
        let status = match (&active_repository, &first_error) {
            (Some(_), _) => LoadStatus::Loading,
            (None, Some(error)) => LoadStatus::Failed(error.clone()),
            (None, None) => LoadStatus::Loaded,
        };

        // Recent-repos MRU: prior history from disk, with the repos opening this
        // session promoted to the front (newest first) so they're remembered
        // even after they're later closed.
        let mut recent_repos = saved.recent_repos.clone();
        for repository in repositories.iter().rev() {
            let key = repository.root.to_string_lossy().into_owned();
            recent_repos.retain(|root| root != &key);
            recent_repos.insert(0, key);
        }
        recent_repos.truncate(RECENT_REPOS_MAX);

        // The active tab starts as a blank `unloaded` shell; `kick_initial_load`
        // below fills it in (and streams the rest). Inactive tabs load on
        // first activation.
        let mut app = Self {
            repository: active_repository.clone(),
            status,
            document: DiffDocument::default(),
            commits: CommitStore::default(),
            selected_revision: RevisionSelection::WorkingCopy,
            file_list_expanded: true,
            pending_revision: None,
            repository_snapshot: None,
            snapshot_pending: false,
            app_focused: true,
            selected_theme: config.theme,
            system_theme: iced_theme::Mode::None,
            selected_file: 0,
            sidebar_width,
            sidebar_min_width,
            window_size,
            window_position,
            geometry_dirty_since: None,
            config,
            revision_details: None,
            branch_status: None,
            bookmarks: BookmarksInfo::default(),
            palette: None,
            recents: Recents::load(),
            find: None,
            revision_reveal_token: 0,
            pending_revision_reveal: false,
            commits_version: 0,
            graph: graph_layout::GraphLayout::default(),
            sidebar_prefix_lens: Vec::new(),
            selected_commit_index: None,
            commit_progress: LoadProgress::default(),
            loading_since: None,
            empty_cache: HashMap::new(),
            load: None,
            tabs,
            active_tab,
            next_tab_id,
            next_load_version: 0,
            open_repo_dialog: None,
            recent_repos,
            revset: active_revset,
            activities: activity::ActivityLog::default(),
            pending_load_activity: None,
            next_activity_id: 0,
            toolbar_menu: None,
            activity_popover_open: false,
            hovered: None,
        };

        let theme_task = system::theme().map(Message::SystemThemeChanged);
        let load_task = if active_repository.is_some() {
            app.kick_initial_load()
        } else {
            Task::none()
        };
        (app, Task::batch([load_task, theme_task]))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(revision, result) => match *result {
                Ok(output) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    let revision_changed = self.selected_revision != revision;
                    self.selected_revision = revision;
                    self.pending_revision = None;
                    self.loading_since = None;
                    self.status = LoadStatus::Loaded;
                    self.document = output.document;
                    self.commits = output.commits;
                    self.graph = output.graph;
                    // A refresh swaps the graph atomically; if a cold stream was
                    // somehow still in flight, supersede it so its late batches
                    // (which assume the now-replaced row indices) are dropped.
                    self.load = None;
                    self.commits_version = self.commits_version.wrapping_add(1);
                    self.repository_snapshot = Some(output.snapshot);
                    self.branch_status = output.branch_status;
                    self.bookmarks = output.bookmarks;
                    self.revision_details = output.details;
                    self.selected_file = if revision_changed {
                        0
                    } else {
                        self.selected_file
                            .min(self.document.files.len().saturating_sub(1))
                    };
                    // If this load was triggered by the palette, the
                    // sidebar didn't yet know the new selected_revision
                    // when the user accepted; bump the reveal token now
                    // that it's been written so the *next* render scrolls
                    // the correct row into view.
                    if self.pending_revision_reveal {
                        self.pending_revision_reveal = false;
                        self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
                    }
                    // Recompute the on-demand sidebar index (lane fold, prefix
                    // lengths, selected-row index) for the new graph.
                    self.rebuild_sidebar_index();
                    self.finish_load_activity(activity::ActivityStatus::Done, None);
                    // Fill in merge/root empty status (left unknown by the
                    // loader) from cache, and kick off a background task for
                    // any not seen before.
                    return self.resolve_empty_status();
                }
                Err(error) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    self.pending_revision = None;
                    self.loading_since = None;
                    self.status = LoadStatus::Failed(error.clone());
                    self.finish_load_activity(activity::ActivityStatus::Error, Some(error));
                }
            },
            Message::CommitsBatch(version, rows) => {
                // Take the cursor out so the appends below borrow `self` fields
                // freely (same idiom the palette uses). Drop batches from a
                // superseded load — their row indices no longer line up.
                // `take_if` (not `take().filter()`) leaves the cursor in place
                // when a *stale* batch arrives mid-stream: taking it out and
                // dropping it would orphan the live load, so its later batches
                // would be lost and the store would end up shorter than the
                // loader's `empty_updates` indices (an out-of-bounds panic).
                let Some(mut cursor) = self.load.take_if(|c| c.version == version) else {
                    return Task::none();
                };
                let selecting_wc = matches!(self.selected_revision, RevisionSelection::WorkingCopy);
                for row in rows {
                    let index = self.commits.len();
                    // The graph fold consumes the frame + the row's bookmarks
                    // (still owned by the summary), so push it before the
                    // summary moves into the store.
                    self.graph
                        .push(&row.frame, &row.summary.bookmarks, &mut cursor.fold);
                    // jj precomputes the shortest-unique-prefix length per row,
                    // so the sidebar index grows by one O(1) push instead of an
                    // O(n) rescan per batch.
                    let total = row.summary.change_id.chars().count();
                    let prefix = row.summary.shortest_change_id_len.unwrap_or(1).min(total);
                    self.sidebar_prefix_lens.push(prefix);
                    if selecting_wc && row.summary.is_working_copy {
                        self.selected_commit_index = Some(index);
                    }
                    self.commits.push(row.summary, &mut cursor.interner);
                }
                self.load = Some(cursor);
                // First batch on screen: lift the full-window loading indicator
                // and reveal the (still-growing) sidebar.
                if matches!(self.status, LoadStatus::Loading) {
                    self.status = LoadStatus::Loaded;
                    self.loading_since = None;
                }
            }
            Message::CommitsFinished(version, result) => {
                if self.load.as_ref().map(|c| c.version) != Some(version) {
                    return Task::none();
                }
                self.load = None;
                match *result {
                    Ok(tail) => {
                        self.repository_snapshot = Some(tail.snapshot);
                        self.branch_status = tail.branch_status;
                        self.bookmarks = tail.bookmarks;
                        // Apply the single-parent emptiness resolved in the
                        // loader's final pass, caching each so reloads skip it.
                        for (index, empty) in tail.empty_updates {
                            // Defensive: a superseded/shorter store must never
                            // index past its end (`set_is_empty` already guards
                            // with `get_mut`; the row read did not).
                            if index >= self.commits.len() {
                                continue;
                            }
                            let commit_id = self.commits.row(index).commit_id().to_owned();
                            self.empty_cache.insert(commit_id, empty);
                            self.commits.set_is_empty(index, empty);
                        }
                        self.commits_version = self.commits_version.wrapping_add(1);
                        self.selected_commit_index = self.find_selected_commit_index();
                        self.finish_load_activity(activity::ActivityStatus::Done, None);
                        // Fill in the merges/roots the loader left unknown.
                        return self.resolve_empty_status();
                    }
                    Err(error) => {
                        self.status = LoadStatus::Failed(error.clone());
                        self.loading_since = None;
                        self.finish_load_activity(activity::ActivityStatus::Error, Some(error));
                    }
                }
            }
            Message::InitialDiff(version, result) => {
                // Apply only while this stream is the active load and the user
                // hasn't navigated off the working copy (e.g. via the palette
                // during load). Leaves `status` as `Loading` so the sidebar
                // stays empty (rather than flashing a stale graph) until the
                // first commit batch; loading feedback is in the toolbar.
                let active = self.load.as_ref().map(|c| c.version) == Some(version)
                    && self.pending_revision.as_ref() == Some(&RevisionSelection::WorkingCopy);
                if !active {
                    return Task::none();
                }
                self.pending_revision = None;
                match *result {
                    Ok((document, details)) => {
                        self.document = document;
                        self.revision_details = details;
                        self.selected_file = 0;
                    }
                    Err(error) => {
                        eprintln!("diffui: working-copy diff failed during load: {error}");
                    }
                }
            }
            Message::DiffLoaded(revision, result) => match *result {
                Ok((document, details)) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    // A working-copy diff is the definitive emptiness signal for
                    // @ (files present ⇒ not empty) — capture it before
                    // `document` moves so a watcher-refresh edit toggles the @
                    // "empty" chip without a graph re-walk.
                    let wc_empty = matches!(revision, RevisionSelection::WorkingCopy)
                        .then(|| document.files.is_empty());

                    let revision_changed = self.selected_revision != revision;
                    self.selected_revision = revision;
                    self.pending_revision = None;
                    self.loading_since = None;
                    self.status = LoadStatus::Loaded;
                    self.document = document;
                    self.revision_details = details;
                    // The graph is unchanged on a diff-only load; just relocate
                    // the selected row.
                    self.selected_commit_index = self.find_selected_commit_index();
                    if let Some(empty) = wc_empty
                        && let Some(index) = self.selected_commit_index
                    {
                        self.commits.set_is_empty(index, empty);
                        self.commits_version = self.commits_version.wrapping_add(1);
                    }
                    self.selected_file = if revision_changed {
                        0
                    } else {
                        self.selected_file
                            .min(self.document.files.len().saturating_sub(1))
                    };
                    if self.pending_revision_reveal {
                        self.pending_revision_reveal = false;
                        self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
                    }
                }
                Err(error) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    self.pending_revision = None;
                    self.loading_since = None;
                    self.status = LoadStatus::Failed(error);
                }
            },
            Message::RepositorySnapshotLoaded(origin, Ok(snapshot)) => {
                self.snapshot_pending = false;
                if self.repository_snapshot.as_ref() != Some(&snapshot)
                    && self.pending_revision.is_none()
                    && let Some(repository) = self.repository.clone()
                {
                    match origin {
                        RefreshOrigin::Watcher => {
                            // A working-tree edit moved @'s tree but not the
                            // graph, so skip the (up to ~1M-commit) re-walk and
                            // just reload @'s diff if it's on screen (the wc
                            // snapshot already ran in `load_repository_snapshot`,
                            // so `load_diff` sees the edit; `DiffLoaded` re-syncs
                            // @'s empty chip). Viewing another commit ⇒ its diff
                            // is unchanged, nothing to reload.
                            //
                            // We deliberately do NOT advance `repository_snapshot`
                            // here: it tracks the op the *graph* reflects, and a
                            // lightweight reload didn't re-walk. Recording the new
                            // op would make a later focus-regain compare equal and
                            // skip its full reload — so an external `jj git
                            // fetch`/rebase that landed between edits would never
                            // appear. Leaving it stale lets the next focus (origin
                            // `Focus`) re-walk and reconcile topology.
                            if matches!(self.selected_revision, RevisionSelection::WorkingCopy) {
                                let revision = self.selected_revision.clone();
                                self.pending_revision = Some(revision.clone());
                                self.loading_since = Some(Instant::now());
                                return Task::perform(
                                    load_diff(repository, revision.clone()),
                                    move |result| Message::DiffLoaded(revision, Box::new(result)),
                                );
                            }
                        }
                        RefreshOrigin::Focus => {
                            // Focus regain / manual refresh can follow an external
                            // jj op that changed topology — full reload. The
                            // resulting `BackendLoaded` records the snapshot.
                            let revision = self.selected_revision.clone();
                            self.pending_revision = Some(revision.clone());
                            self.loading_since = Some(Instant::now());
                            let progress = LoadProgress::default();
                            self.commit_progress = progress.clone();
                            let revset = self.revset.clone();
                            return Task::perform(
                                load_backend(repository, revision.clone(), revset, progress),
                                move |result| Message::BackendLoaded(revision, Box::new(result)),
                            );
                        }
                    }
                }
                // We reach here only when no reload was kicked (snapshot
                // unchanged, or viewing a non-@ revision on a watcher tick). A
                // toolbar Refresh still wants its activity resolved — there was
                // simply nothing to reload.
                self.finish_load_activity(
                    activity::ActivityStatus::Done,
                    Some("Already up to date".to_owned()),
                );
            }
            Message::RepositorySnapshotLoaded(_, Err(error)) => {
                self.snapshot_pending = false;
                self.finish_load_activity(activity::ActivityStatus::Error, Some(error.clone()));
                self.status = LoadStatus::Failed(error);
            }
            Message::EmptyStatusComputed(version, updates) => {
                // Drop results computed against a graph that's since been
                // replaced — their row indices would no longer line up.
                if version != self.commits_version || updates.is_empty() {
                    return Task::none();
                }
                for &(index, empty) in &updates {
                    let commit_id = self.commits.row(index).commit_id().to_owned();
                    self.empty_cache.insert(commit_id, empty);
                    self.commits.set_is_empty(index, empty);
                }
                self.commits_version = self.commits_version.wrapping_add(1);
            }
            Message::SelectFile(index) => {
                if index < self.document.files.len() {
                    self.selected_file = index;
                    return scroll_sidebar_to_file(index, self);
                }
            }
            Message::SelectRowKey(key) => {
                let selection = match key {
                    revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
                    revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id),
                };
                // Re-clicking the already-selected revision toggles its file
                // list without re-running the backend or changing the diff.
                // The toggled value persists across revision switches, so
                // collapsing once stays collapsed wherever the user moves
                // next.
                if self.selected_revision == selection {
                    self.file_list_expanded = !self.file_list_expanded;
                } else if self.pending_revision.as_ref() != Some(&selection)
                    && let Some(repository) = self.repository.clone()
                {
                    self.pending_revision = Some(selection.clone());
                    self.loading_since = Some(Instant::now());
                    let revision = selection.clone();
                    return Task::perform(load_diff(repository, selection), move |result| {
                        Message::DiffLoaded(revision, Box::new(result))
                    });
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
            }
            Message::SystemThemeChanged(theme) => {
                self.system_theme = theme;
            }
            Message::RevisionContextMenu(key, row_rect) => {
                let Some(repository) = self.repository.clone() else {
                    return Task::none();
                };
                // jj-only for now — the mutations are jj-lib transactions.
                if !matches!(repository.vcs, Vcs::Jj) {
                    return Task::none();
                }
                // Opens the native menu (blocking) with a pulsing glow anchored
                // over `row_rect`; returns the chosen mutation as a task.
                return self.open_revision_context_menu(
                    repository,
                    selection_from_key(&key),
                    row_rect,
                );
            }
            Message::MutationCompleted(tab_id, id, result) => match *result {
                Ok(outcome) => {
                    if let Some(log) = self.activity_log_for(tab_id) {
                        if !outcome.output.is_empty() {
                            log.extend_output(id, outcome.output);
                        }
                        log.finish(
                            id,
                            activity::ActivityStatus::Done,
                            Some(outcome.message.clone()),
                        );
                    }
                    // Reload the graph to reflect the mutation. Only snap the
                    // selection back to `@` when the op actually moved it
                    // (new/edit/abandon); bookmark ops leave it put.
                    if outcome.moved_working_copy {
                        self.selected_revision = RevisionSelection::WorkingCopy;
                    }
                    return self.start_repository_snapshot(RefreshOrigin::Focus);
                }
                Err(error) => {
                    // Surface the failure in the activity log rather than failing
                    // the whole view — a rejected push shouldn't blank the panes.
                    if let Some(log) = self.activity_log_for(tab_id) {
                        log.append_output(id, error.clone());
                        log.finish(id, activity::ActivityStatus::Error, Some(error));
                    }
                }
            },
            Message::WindowFocusChanged(focused) => {
                let gained_focus = focused && !self.app_focused;
                let lost_focus = !focused && self.app_focused;
                self.app_focused = focused;

                // Flush pending geometry immediately on focus loss. App-switch
                // and quit almost always blur the window first, so this closes
                // the gap between a resize and the debounce timer firing.
                if lost_focus && self.geometry_dirty_since.is_some() {
                    self.geometry_dirty_since = None;
                    self.current_window_state().save();
                }

                if gained_focus {
                    return self.start_repository_snapshot(RefreshOrigin::Focus);
                }
            }
            Message::RefreshRepository => {
                if self.app_focused {
                    return self.start_repository_snapshot(RefreshOrigin::Watcher);
                }
            }
            Message::LoadingTick => {}
            Message::SelectNextFile => {
                if !self.document.files.is_empty() {
                    self.selected_file =
                        (self.selected_file + 1).min(self.document.files.len().saturating_sub(1));
                    return scroll_sidebar_to_file(self.selected_file, self);
                }
            }
            Message::SelectPreviousFile => {
                let previous = self.selected_file.saturating_sub(1);
                if previous != self.selected_file {
                    self.selected_file = previous;
                    return scroll_sidebar_to_file(self.selected_file, self);
                }
            }
            Message::CopyToClipboard(text) => {
                return iced::clipboard::write(text).discard();
            }
            Message::SidebarWidthChanged(width) => {
                let clamped = width.max(self.sidebar_min_width);
                if clamped != self.sidebar_width {
                    self.sidebar_width = clamped;
                    self.mark_geometry_dirty();
                }
            }
            Message::WindowOpened(position, size) => {
                // Seed tracking from the real window without marking dirty: the
                // geometry we'd persist already matches what's on disk.
                self.window_size = size;
                if position.is_some() {
                    self.window_position = position;
                }
                // Center the native window controls on the tab strip, and arm
                // the native resize observer that keeps them centered without a
                // frame of lag while the window is dragged (see
                // `chrome::install_window_resize_observer`).
                return Task::batch([
                    self.reposition_window_controls(),
                    self.install_resize_observer(),
                ]);
            }
            Message::WindowResized(size) => {
                if self.window_size != size {
                    self.window_size = size;
                    self.mark_geometry_dirty();
                }
                // The native resize observer (armed on open) re-centers the
                // traffic lights in step with AppKit's layout. This message-loop
                // reposition stays as a harmless fallback — it runs a frame
                // later and just re-applies the same position the observer
                // already set, so it can't reintroduce the jump.
                return self.reposition_window_controls();
            }
            Message::WindowMoved(position) => {
                if self.window_position != Some(position) {
                    self.window_position = Some(position);
                    self.mark_geometry_dirty();
                }
            }
            Message::PersistWindowState => {
                // Only write once the changes have settled — a drag keeps
                // bumping `geometry_dirty_since`, so the elapsed check holds the
                // write back until the burst stops.
                if let Some(since) = self.geometry_dirty_since
                    && since.elapsed() >= WINDOW_STATE_DEBOUNCE
                {
                    self.geometry_dirty_since = None;
                    self.current_window_state().save();
                }
            }
            Message::SelectTab(id) => {
                return self.activate_tab(id);
            }
            Message::SelectTabIndex(index) => {
                if let Some(tab) = self.tabs.get(index) {
                    let id = tab.id;
                    return self.activate_tab(id);
                }
            }
            Message::CloseTab(id) => {
                return self.close_tab(id);
            }
            Message::CloseActiveTab => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let id = tab.id;
                    return self.close_tab(id);
                }
            }
            Message::OpenRepoDialogOpen => {
                // Mutually exclusive with the other overlays.
                self.palette = None;
                self.find = None;
                self.open_repo_dialog = Some(OpenRepoDialog::default());
                return widget::operation::focus(tab_bar::OPEN_REPO_INPUT_ID);
            }
            Message::OpenRepoDialogClose => {
                self.open_repo_dialog = None;
            }
            Message::OpenRepoPathChanged(path) => {
                if let Some(dialog) = self.open_repo_dialog.as_mut() {
                    dialog.path = path;
                    // Clear a stale error as soon as the user edits the path.
                    dialog.error = None;
                }
            }
            Message::OpenRepoSubmit => {
                let path = self
                    .open_repo_dialog
                    .as_ref()
                    .map(|dialog| dialog.path.clone())
                    .unwrap_or_default();
                return self.open_repository(&path);
            }
            Message::OpenRecentRepo(path) => {
                return self.open_repository(&path);
            }
            Message::OpenRepoNoOp => {}
            Message::TitleBarDrag => {
                // Resolve the (single) window and begin an interactive drag.
                // No-op if the window id isn't available yet.
                return window::latest().then(|id| id.map_or_else(Task::none, window::drag));
            }
            Message::PaletteOpen => {
                if self.palette.is_none() {
                    // Mutually exclusive with the find bar / open-repo dialog:
                    // opening the palette pulls keyboard focus and the others
                    // would sit behind the modal anyway.
                    self.find = None;
                    self.open_repo_dialog = None;
                    self.palette = Some(PaletteState::open(self));
                    return widget::operation::focus(palette::PALETTE_INPUT_ID);
                }
            }
            Message::PaletteClose => {
                self.palette = None;
            }
            Message::PaletteQueryChanged(query) => {
                // Take the palette out of `self` so the matcher can borrow
                // `&self` (commits / files / recents) directly. Previously
                // this cloned the entire app per keystroke; on a 40k-commit
                // repo that deep clone was the bulk of the typing latency.
                let Some(mut state) = self.palette.take() else {
                    return Task::none();
                };
                let depth = state.stack.len().saturating_sub(1);
                let mut task = Task::none();
                if let Some(column) = state.top_mut() {
                    column.query = query;
                    column.dirty = true;
                    // Editing resets `:` commit-search back to its "press ⏎"
                    // prompt (the prior results are for a stale query).
                    column.searched = false;
                    column.query_version = column.query_version.wrapping_add(1);
                    let version = column.query_version;
                    // Debounce: the matcher scans every commit, so coalesce
                    // fast typing rather than re-matching on each keystroke.
                    task = Task::perform(
                        async move {
                            tokio::time::sleep(PALETTE_QUERY_DEBOUNCE).await;
                            (depth, version)
                        },
                        |(depth, version)| Message::PaletteRecompute(depth, version),
                    );
                }
                self.palette = Some(state);
                return task;
            }
            Message::PaletteRecompute(depth, version) => {
                let Some(mut state) = self.palette.take() else {
                    return Task::none();
                };
                let mut task = Task::none();
                if let Some(column) = state.stack.get_mut(depth)
                    && column.query_version == version
                {
                    column.selected = 0;
                    // Re-running the matcher invalidates row positions; jump
                    // the scroll back to the top so the first row is visible.
                    column.scroll_y = 0.0;
                    column.dirty = false;
                    palette::recompute_matches(column, self, false);
                    task = widget::operation::scroll_to(
                        palette::results_scrollable_id(depth),
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
                self.palette = Some(state);
                return task;
            }
            Message::PaletteMoveSelection(delta) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && !column.matches.is_empty()
                {
                    let len = column.matches.len() as i32;
                    let next = (column.selected as i32 + delta).rem_euclid(len);
                    column.selected = next as usize;
                    let depth = state.stack.len().saturating_sub(1);
                    let column = state.stack.last_mut().expect("top column");
                    if column.ensure_selected_visible() {
                        return widget::operation::scroll_to(
                            palette::results_scrollable_id(depth),
                            iced::widget::scrollable::AbsoluteOffset {
                                x: 0.0,
                                y: column.scroll_y,
                            },
                        );
                    }
                }
            }
            Message::PaletteSelectIndex(index) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
            }
            Message::PaletteAccept => {
                return self.palette_submit();
            }
            Message::PaletteAcceptIndex(index) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
                return self.palette_accept_current();
            }
            Message::PalettePushActions => {
                let Some(mut state) = self.palette.take() else {
                    return Task::none();
                };
                let pushed = state.push_actions(self);
                self.palette = Some(state);
                if pushed {
                    return widget::operation::focus(palette::PALETTE_INPUT_ID);
                }
            }
            Message::PaletteNoOp => {}
            Message::PaletteTick => {}
            Message::PalettePopColumn => {
                if let Some(state) = self.palette.as_mut() {
                    if state.pop() {
                        return widget::operation::focus(palette::PALETTE_INPUT_ID);
                    } else {
                        self.palette = None;
                    }
                }
            }
            Message::FindOpen => {
                // Mutually exclusive with the palette / open-repo dialog: same
                // keyboard focus arbiter, and stacking overlays makes the find
                // bar look broken.
                self.palette = None;
                self.open_repo_dialog = None;
                if self.find.is_none() {
                    self.find = Some(FindState::default());
                }
                return widget::operation::focus(find::FIND_INPUT_ID);
            }
            Message::FindClose => {
                self.find = None;
            }
            Message::FindQueryChanged(query) => {
                if let Some(state) = self.find.as_mut() {
                    state.query = query;
                    state.error = None;
                    state.query_version = state.query_version.wrapping_add(1);
                    let version = state.query_version;
                    return Task::perform(
                        async move {
                            tokio::time::sleep(find::DEBOUNCE).await;
                            version
                        },
                        Message::FindRecompute,
                    );
                }
            }
            Message::FindRecompute(version) => {
                if let Some(state) = self.find.as_mut()
                    && state.query_version == version
                {
                    let (matches, error) = find::compute_matches(state, &self.document);
                    state.matches = matches;
                    state.error = error;
                    state.active = if state.matches.is_empty() {
                        None
                    } else {
                        Some(0)
                    };
                    state.scroll_token = state.scroll_token.wrapping_add(1);
                }
            }
            Message::FindToggleCase => {
                if let Some(state) = self.find.as_mut() {
                    state.case_sensitive = !state.case_sensitive;
                    return self.refind_now();
                }
            }
            Message::FindToggleRegex => {
                if let Some(state) = self.find.as_mut() {
                    state.regex = !state.regex;
                    return self.refind_now();
                }
            }
            Message::FindNext => {
                self.find_advance(1);
            }
            Message::FindPrev => {
                self.find_advance(-1);
            }

            // ── Toolbar / activity / revset ─────────────────────────────
            Message::ToolbarRefresh => {
                return self.toolbar_refresh();
            }
            Message::Fetch(target) => {
                return self.start_fetch(target);
            }
            Message::FetchCompleted(tab_id, id, result) => {
                return self.finish_remote_op(tab_id, id, *result);
            }
            Message::Undo => {
                return self.start_undo();
            }
            Message::UndoCompleted(tab_id, id, result) => {
                return self.finish_remote_op(tab_id, id, *result);
            }
            Message::RevsetChanged(value) => {
                self.revset = value;
            }
            Message::RevsetSubmit => {
                return self.evaluate_revset();
            }
            Message::RevsetPreset(value) => {
                self.revset = value;
                self.toolbar_menu = None;
                return self.evaluate_revset();
            }
            Message::OpenToolbarMenu(menu) => {
                self.activity_popover_open = false;
                return self.open_toolbar_menu(menu);
            }
            Message::CloseToolbarMenu => {
                self.toolbar_menu = None;
            }
            Message::ActivityToggle => {
                self.activity_popover_open = !self.activity_popover_open;
                self.toolbar_menu = None;
            }
            Message::ActivityExpand(id) => {
                self.activities.toggle_expand(id);
            }
            Message::ActivityClear => {
                self.activities.clear_finished();
            }
            Message::ActivityNoOp => {}
            Message::OpenUrl(url) => {
                open_url(&url);
            }
            Message::SetHover(target) => {
                self.hovered = target;
            }
        }

        Task::none()
    }

    /// Recompute find matches immediately (no debounce). Used by toggle
    /// presses where the user's intent is immediate.
    fn refind_now(&mut self) -> Task<Message> {
        let Some(state) = self.find.as_mut() else {
            return Task::none();
        };
        state.query_version = state.query_version.wrapping_add(1);
        let (matches, error) = find::compute_matches(state, &self.document);
        state.matches = matches;
        state.error = error;
        state.active = if state.matches.is_empty() {
            None
        } else {
            Some(0)
        };
        state.scroll_token = state.scroll_token.wrapping_add(1);
        Task::none()
    }

    fn find_advance(&mut self, delta: i32) {
        let Some(state) = self.find.as_mut() else {
            return;
        };
        if state.matches.is_empty() {
            return;
        }
        let len = state.matches.len() as i32;
        let current = state.active.map(|i| i as i32).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        state.active = Some(next as usize);
        state.scroll_token = state.scroll_token.wrapping_add(1);
    }

    /// Handle ⏎ in the palette. In `:` commit-search mode the all-commits scan
    /// is deferred to here (too slow to run per keystroke on a 1M-commit repo):
    /// the first ⏎ runs the scan and shows results; once searched, ⏎ accepts the
    /// highlighted row like any other mode.
    fn palette_submit(&mut self) -> Task<Message> {
        let Some(mut state) = self.palette.take() else {
            return Task::none();
        };
        let trigger_search = state.top().is_some_and(|column| {
            matches!(column.source, ColumnSource::Root)
                && !column.searched
                && palette::revision_mode_needle(&column.query)
                    .is_some_and(|needle| !needle.trim().is_empty())
        });
        if trigger_search {
            let depth = state.stack.len().saturating_sub(1);
            if let Some(column) = state.top_mut() {
                column.searched = true;
                column.dirty = false;
                column.selected = 0;
                column.scroll_y = 0.0;
                // Invalidate the pending debounced recompute so it can't wipe
                // the results we're about to compute.
                column.query_version = column.query_version.wrapping_add(1);
                // `self.palette` is `None` here (taken above), so this borrows
                // `self` cleanly while mutating the detached column.
                palette::recompute_matches(column, self, true);
            }
            self.palette = Some(state);
            return widget::operation::scroll_to(
                palette::results_scrollable_id(depth),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
            );
        }
        self.palette = Some(state);
        self.palette_accept_current()
    }

    /// Execute the highlighted result in the rightmost column. Returns the
    /// `Task` chain that performs the corresponding action plus any
    /// followup state (closing the palette, focusing input, etc.).
    fn palette_accept_current(&mut self) -> Task<Message> {
        let Some(state) = self.palette.as_ref() else {
            return Task::none();
        };
        let Some(top) = state.top() else {
            return Task::none();
        };
        let Some(selected) = top.matches.get(top.selected) else {
            return Task::none();
        };
        let item = selected.item.clone();
        let target = match &top.source {
            ColumnSource::Root => None,
            ColumnSource::Actions(t) => Some(t.clone()),
        };

        match (&top.source, &item) {
            // Top-level: command rows run directly; revision/file rows
            // primary-action without going through the Actions column.
            (ColumnSource::Root, ResultRef::Command(cmd)) => {
                self.recents.push_command(*cmd);
                self.recents.save();
                self.palette = None;
                self.run_palette_command(*cmd, None)
            }
            (
                ColumnSource::Root,
                ResultRef::WorkingCopy | ResultRef::Commit(_) | ResultRef::Bookmark(_),
            ) => {
                if let Some(change_id) = change_id_for_recents(&item, self) {
                    self.recents.push_revision(change_id);
                    self.recents.save();
                }
                self.palette = None;
                self.jump_to_revision_ref(&item)
            }
            (ColumnSource::Root, ResultRef::File(path)) => {
                self.palette = None;
                self.jump_to_file_path(path);
                Task::none()
            }
            // Actions column: the row is always a Command — run it against
            // the column's target.
            (ColumnSource::Actions(_), ResultRef::Command(cmd)) => {
                self.recents.push_command(*cmd);
                self.recents.save();
                self.palette = None;
                self.run_palette_command(*cmd, target)
            }
            _ => Task::none(),
        }
    }

    fn run_palette_command(
        &mut self,
        cmd: PaletteCommand,
        target: Option<ResultRef>,
    ) -> Task<Message> {
        match cmd {
            PaletteCommand::RefreshRepository => {
                if self.app_focused {
                    // Manual refresh = full reload (the user may have run an
                    // external jj op since the last load).
                    return self.start_repository_snapshot(RefreshOrigin::Focus);
                }
                Task::none()
            }
            PaletteCommand::SelectNextFile => Task::done(Message::SelectNextFile),
            PaletteCommand::SelectPreviousFile => Task::done(Message::SelectPreviousFile),
            PaletteCommand::ThemeSystem => {
                Task::done(Message::SelectTheme(ThemePreference::System))
            }
            PaletteCommand::ThemeLight => Task::done(Message::SelectTheme(ThemePreference::Light)),
            PaletteCommand::ThemeDark => Task::done(Message::SelectTheme(ThemePreference::Dark)),
            PaletteCommand::ThemeHighContrast => {
                Task::done(Message::SelectTheme(ThemePreference::HighContrast))
            }
            PaletteCommand::CopyFileDiff => {
                if let Some(text) = current_file_diff_text(self) {
                    Task::done(Message::CopyToClipboard(text))
                } else {
                    Task::none()
                }
            }
            PaletteCommand::OpenFind => Task::done(Message::FindOpen),
            PaletteCommand::JumpToRevision => {
                if let Some(t) = target.as_ref() {
                    if let Some(change_id) = change_id_for_recents(t, self) {
                        self.recents.push_revision(change_id);
                        self.recents.save();
                    }
                    self.jump_to_revision_ref(t)
                } else {
                    Task::none()
                }
            }
            PaletteCommand::CopyChangeId => {
                // Resolve through the unified helper so bookmarks /
                // working-copy / explicit commits all surface their
                // change-id consistently.
                let payload = target.and_then(|t| change_id_for_recents(&t, self));
                payload
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::CopyCommitMessage => {
                let payload = target.and_then(|t| commit_for_ref(self, &t)).map(|c| {
                    if c.has_description() {
                        c.description().to_owned()
                    } else {
                        String::new()
                    }
                });
                payload
                    .filter(|s| !s.is_empty())
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::CopyAuthor => {
                let payload = target
                    .and_then(|t| commit_for_ref(self, &t))
                    .map(|c| c.author().to_owned());
                payload
                    .filter(|s| !s.is_empty())
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::OpenFile => {
                if let Some(ResultRef::File(path)) = target.as_ref() {
                    self.jump_to_file_path(path);
                }
                Task::none()
            }
            PaletteCommand::CopyFilePath => {
                if let Some(ResultRef::File(path)) = target.as_ref() {
                    Task::done(Message::CopyToClipboard(path.clone()))
                } else {
                    Task::none()
                }
            }
        }
    }

    fn jump_to_revision_ref(&mut self, target: &ResultRef) -> Task<Message> {
        let Some(selection) = revision_selection(target, self) else {
            return Task::none();
        };

        // Already current — no load, no async wait, bump the token now so
        // the next render scrolls the sidebar row into view.
        if self.selected_revision == selection {
            self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
            return Task::none();
        }

        // A load is already in flight for the same revision; piggyback so
        // the eventual `BackendLoaded` bumps the token for us.
        if self.pending_revision.as_ref() == Some(&selection) {
            self.pending_revision_reveal = true;
            return Task::none();
        }

        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        self.pending_revision = Some(selection.clone());
        self.loading_since = Some(Instant::now());
        // Deferred bump — see comment on `pending_revision_reveal`.
        self.pending_revision_reveal = true;
        let revision = selection.clone();
        Task::perform(load_diff(repository, selection), move |result| {
            Message::DiffLoaded(revision.clone(), Box::new(result))
        })
    }

    fn jump_to_file_path(&mut self, path: &str) {
        if let Some(index) = self.document.files.iter().position(|f| f.path == path) {
            self.selected_file = index;
        }
    }

    fn start_repository_snapshot(&mut self, origin: RefreshOrigin) -> Task<Message> {
        // Don't refresh while a cold stream is still appending — a refresh
        // re-walks and swaps the graph wholesale, which would race the
        // in-flight batches. The stream finishes in seconds; refresh resumes
        // normally once it clears the load cursor.
        if self.snapshot_pending || self.load.is_some() {
            return Task::none();
        }

        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };

        self.snapshot_pending = true;
        Task::perform(load_repository_snapshot(repository), move |result| {
            Message::RepositorySnapshotLoaded(origin, result.map_err(|error| format!("{error:#}")))
        })
    }

    /// Show the native context menu for a right-clicked revision and dispatch
    /// the chosen mutation. `popup_menu` blocks (a nested `NSMenu` loop) on the
    /// main thread until the user picks — fine for a modal menu — then the
    /// mutation itself runs off-thread.
    ///
    /// The menu is built from `self.bookmarks` (loaded with the graph), so it
    /// can offer per-bookmark actions for the bookmarks sitting on this revision
    /// without any synchronous repo read.
    #[cfg(target_os = "macos")]
    fn open_revision_context_menu(
        &mut self,
        repository: Repository,
        selection: RevisionSelection,
        row_rect: iced::Rectangle,
    ) -> Task<Message> {
        use macos_native::MenuItem;
        use mutations::MutationOp;

        // Each leaf carries an index into `actions`; the popup returns that
        // index. `register` keeps the menu tree and the action list in lockstep.
        fn register(actions: &mut Vec<MenuAction>, action: MenuAction) -> u32 {
            let id = actions.len() as u32;
            actions.push(action);
            id
        }

        let mut actions: Vec<MenuAction> = Vec::new();

        // ── Commit-level actions ────────────────────────────────────────────
        let mut top = vec![
            MenuItem::entry(
                "New child",
                register(
                    &mut actions,
                    MenuAction::Mutate(MutationOp::New {
                        parent: selection.clone(),
                    }),
                ),
            ),
            MenuItem::entry(
                "Edit",
                register(
                    &mut actions,
                    MenuAction::Mutate(MutationOp::Edit {
                        target: selection.clone(),
                    }),
                ),
            ),
            MenuItem::entry(
                "Abandon",
                register(
                    &mut actions,
                    MenuAction::Mutate(MutationOp::Abandon {
                        target: selection.clone(),
                    }),
                ),
            ),
        ];

        // ── Copy revision metadata ──────────────────────────────────────────
        // Values read from the already-loaded graph row (so the menu stays
        // instant); author/committer copies, which need a date the graph
        // doesn't keep, are read on demand when picked.
        let copy_fields = {
            let row = match &selection {
                RevisionSelection::WorkingCopy => self.commits.working_copy(),
                RevisionSelection::Commit(hex) => self.commits.find_by_commit_id(hex),
            };
            row.map(|row| {
                (
                    row.change_id().to_owned(),
                    row.commit_id().to_owned(),
                    row.description().to_owned(),
                    row.author().to_owned(),
                    row.bookmarks().to_vec(),
                )
            })
        };
        if let Some((change_id, commit_id, description, author, bookmarks)) = copy_fields {
            let mut copy_items = vec![
                MenuItem::entry(
                    "Revision ID",
                    register(&mut actions, MenuAction::CopyText(change_id)),
                ),
                MenuItem::entry(
                    "Commit hash",
                    register(&mut actions, MenuAction::CopyText(commit_id)),
                ),
            ];
            match bookmarks.len() {
                0 => {}
                1 => copy_items.push(MenuItem::entry(
                    "Bookmark",
                    register(&mut actions, MenuAction::CopyText(bookmarks[0].clone())),
                )),
                _ => {
                    let subs = bookmarks
                        .iter()
                        .map(|name| {
                            MenuItem::entry(
                                name.clone(),
                                register(&mut actions, MenuAction::CopyText(name.clone())),
                            )
                        })
                        .collect();
                    copy_items.push(MenuItem::submenu("Bookmark", subs));
                }
            }
            if !description.is_empty() {
                copy_items.push(MenuItem::entry(
                    "Description",
                    register(
                        &mut actions,
                        MenuAction::CopyDetail {
                            field: DetailField::Description,
                            // The in-memory subject line, in case the full read
                            // fails — better than copying nothing.
                            fallback: description,
                        },
                    ),
                ));
            }
            copy_items.push(MenuItem::entry(
                "Author",
                register(
                    &mut actions,
                    MenuAction::CopyDetail {
                        field: DetailField::Author,
                        fallback: author.clone(),
                    },
                ),
            ));
            copy_items.push(MenuItem::entry(
                "Committer",
                register(
                    &mut actions,
                    MenuAction::CopyDetail {
                        field: DetailField::Committer,
                        fallback: author,
                    },
                ),
            ));
            top.push(MenuItem::Separator);
            top.push(MenuItem::submenu("Copy", copy_items));
        }

        // ── Move a bookmark onto this revision ──────────────────────────────
        // Candidate local bookmarks (name + target commit). Collected first so
        // the `self.bookmarks` borrow ends before we mutate `actions`, then
        // ordered nearest-first to the right-clicked revision. An empty submenu
        // renders as a disabled row.
        let mut moves: Vec<(String, String)> = self
            .bookmarks
            .bookmarks
            .iter()
            .filter_map(|b| b.local_target.as_ref().map(|t| (b.name.clone(), t.clone())))
            .collect();
        moves.sort(); // alphabetical baseline (stable tiebreak below)
        let move_reference = match &selection {
            RevisionSelection::Commit(hex) => Some(hex.clone()),
            RevisionSelection::WorkingCopy => self.bookmarks.working_copy_commit.clone(),
        };
        self.sort_by_proximity(&mut moves, move_reference.as_deref(), |(_, t)| t.as_str());
        let move_items: Vec<MenuItem> = moves
            .into_iter()
            .map(|(name, _target)| {
                let id = register(
                    &mut actions,
                    MenuAction::Mutate(MutationOp::MoveBookmark {
                        name: name.clone(),
                        to: selection.clone(),
                    }),
                );
                MenuItem::entry(name, id)
            })
            .collect();
        top.push(MenuItem::Separator);
        top.push(MenuItem::submenu("Move bookmark here", move_items));

        // ── Per-bookmark actions for bookmarks on this revision ─────────────
        let target_hex: Option<&str> = match &selection {
            RevisionSelection::Commit(hex) => Some(hex.as_str()),
            RevisionSelection::WorkingCopy => self.bookmarks.working_copy_commit.as_deref(),
        };
        let mut bookmark_items: Vec<MenuItem> = Vec::new();
        if let Some(hex) = target_hex {
            for entry in &self.bookmarks.bookmarks {
                // A local bookmark sitting here → push (if tracked) + delete.
                if entry.local_target.as_deref() == Some(hex) {
                    let mut sub = Vec::new();
                    if let Some(remote) = entry.tracked_remote() {
                        let id = register(
                            &mut actions,
                            MenuAction::Mutate(MutationOp::PushBookmark {
                                name: entry.name.clone(),
                                remote: remote.to_owned(),
                            }),
                        );
                        sub.push(MenuItem::entry(format!("Push to {remote}"), id));
                    }
                    let id = register(
                        &mut actions,
                        MenuAction::Mutate(MutationOp::DeleteBookmark {
                            name: entry.name.clone(),
                        }),
                    );
                    sub.push(MenuItem::entry("Delete", id));
                    bookmark_items.push(MenuItem::submenu(entry.name.clone(), sub));
                }
                // An untracked remote ref sitting here → offer to track it.
                for remote_ref in &entry.remotes {
                    if remote_ref.target.as_str() == hex && !remote_ref.tracked {
                        let id = register(
                            &mut actions,
                            MenuAction::Mutate(MutationOp::TrackBookmark {
                                name: entry.name.clone(),
                                remote: remote_ref.remote.clone(),
                            }),
                        );
                        bookmark_items.push(MenuItem::submenu(
                            format!("{}@{}", entry.name, remote_ref.remote),
                            vec![MenuItem::entry("Track", id)],
                        ));
                    }
                }
            }
        }
        if !bookmark_items.is_empty() {
            top.push(MenuItem::Separator);
            top.append(&mut bookmark_items);
        }

        let glow = macos_native::GlowRect {
            x: row_rect.x,
            y: row_rect.y,
            width: row_rect.width,
            height: row_rect.height,
        };
        let Some(chosen) = macos_native::popup_menu(&top, Some(glow)) else {
            return Task::none();
        };
        let Some(action) = actions.get(chosen as usize).cloned() else {
            return Task::none();
        };
        let op = match action {
            MenuAction::Mutate(op) => op,
            // Ready-to-paste values write to the clipboard immediately.
            MenuAction::CopyText(text) => return iced::clipboard::write(text).discard(),
            // Author / committer / full description aren't kept in the graph, so
            // read the revision off-thread, format the field, and copy —
            // falling back to the in-memory value on failure.
            MenuAction::CopyDetail { field, fallback } => {
                return Task::perform(
                    backend::load_revision_details(repository, selection),
                    move |result| {
                        let text = result
                            .ok()
                            .and_then(|details| format_detail(&details, field))
                            .unwrap_or(fallback);
                        Message::CopyToClipboard(text)
                    },
                );
            }
        };
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        // Surface the mutation as an activity (push captures its remote output).
        let label = match &op {
            MutationOp::New { .. } => "New change".to_owned(),
            MutationOp::Edit { .. } => "Edit".to_owned(),
            MutationOp::Abandon { .. } => "Abandon".to_owned(),
            MutationOp::MoveBookmark { name, .. } => format!("Move bookmark {name}"),
            MutationOp::DeleteBookmark { name } => format!("Delete bookmark {name}"),
            MutationOp::TrackBookmark { name, remote } => format!("Track {name}@{remote}"),
            MutationOp::PushBookmark { name, remote } => format!("Push {name} to {remote}"),
        };
        // Only push reports real progress (git transfer); the rest are quick
        // local ops, so they stay indeterminate.
        let determinate = matches!(op, MutationOp::PushBookmark { .. });
        let (activity_id, progress) = self.begin_activity(label, determinate);
        Task::perform(
            mutations::run_mutation(repository, op, progress),
            move |result| Message::MutationCompleted(tab_id, activity_id, Box::new(result)),
        )
    }

    /// Non-macOS stub: the native popup isn't available, so right-click is a
    /// no-op for now (the mutations themselves are portable).
    #[cfg(not(target_os = "macos"))]
    fn open_revision_context_menu(
        &self,
        _repository: Repository,
        _selection: RevisionSelection,
        _row_rect: iced::Rectangle,
    ) -> Task<Message> {
        Task::none()
    }

    /// Open a toolbar dropdown (fetch branches / revset presets) as a native
    /// `NSMenu` at the cursor — it auto-sizes to the longest label and never
    /// word-wraps, unlike the iced overlay (kept as the non-macOS fallback).
    /// The menu is modal/blocking like the revision context menu, so the chosen
    /// action is dispatched directly on return.
    #[cfg(target_os = "macos")]
    fn open_toolbar_menu(&mut self, menu: ToolbarMenu) -> Task<Message> {
        use macos_native::MenuItem;

        match menu {
            ToolbarMenu::FetchBranches => {
                // id 0 = all remotes; each known `name@remote` follows, ordered
                // by proximity to the working copy.
                let mut targets = vec![FetchTarget::AllRemotes];
                let mut items = vec![MenuItem::entry("Fetch all remotes", 0)];
                let branches = self.remote_branches_by_proximity();
                if !branches.is_empty() {
                    items.push(MenuItem::Separator);
                    for (branch, remote) in branches {
                        let id = targets.len() as u32;
                        items.push(MenuItem::entry(format!("{branch}@{remote}"), id));
                        targets.push(FetchTarget::RemoteBranch { remote, branch });
                    }
                }
                let Some(chosen) = macos_native::popup_menu(&items, None) else {
                    return Task::none();
                };
                let Some(target) = targets.get(chosen as usize).cloned() else {
                    return Task::none();
                };
                self.start_fetch(target)
            }
            ToolbarMenu::RevsetPresets => {
                let presets = revset_presets(self.repository.as_ref().map(|r| r.vcs));
                let items: Vec<MenuItem> = presets
                    .iter()
                    .enumerate()
                    .map(|(index, (label, expr))| {
                        MenuItem::entry(format!("{label}  ·  {expr}"), index as u32)
                    })
                    .collect();
                let Some(chosen) = macos_native::popup_menu(&items, None) else {
                    return Task::none();
                };
                let Some((_, expr)) = presets.get(chosen as usize) else {
                    return Task::none();
                };
                self.revset = (*expr).to_owned();
                self.evaluate_revset()
            }
        }
    }

    /// Non-macOS: fall back to the iced overlay dropdown.
    #[cfg(not(target_os = "macos"))]
    fn open_toolbar_menu(&mut self, menu: ToolbarMenu) -> Task<Message> {
        self.toolbar_menu = Some(menu);
        Task::none()
    }

    /// Resolve the merge/root commits the loader left with unknown empty
    /// status: apply any cached results immediately, and spawn a background
    /// task for the rest (single-parent commits were already decided cheaply
    /// during load).
    fn resolve_empty_status(&mut self) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };

        // Each background resolution is an ~8ms parent-tree merge, so on a
        // repo with hundreds of thousands of merge commits (nixpkgs) computing
        // them all would burn tens of minutes of CPU. Cap how many we resolve
        // per load; beyond that, merges simply keep no "empty" chip. Cached
        // results still apply to every row, so this only bounds *new* work.
        const EMPTY_STATUS_LIMIT: usize = 5_000;

        let mut cached_updates = Vec::new();
        let mut targets = Vec::new();
        for (index, row) in self.commits.iter().enumerate() {
            if row.is_empty().is_some() {
                continue;
            }
            match self.empty_cache.get(row.commit_id()) {
                Some(&empty) => cached_updates.push((index, empty)),
                None if targets.len() < EMPTY_STATUS_LIMIT => {
                    targets.push((index, row.commit_id().to_owned()))
                }
                None => {}
            }
        }

        let had_cached = !cached_updates.is_empty();
        for (index, empty) in cached_updates {
            self.commits.set_is_empty(index, empty);
        }
        if had_cached {
            self.commits_version = self.commits_version.wrapping_add(1);
        }

        if targets.is_empty() {
            return Task::none();
        }

        let version = self.commits_version;
        Task::perform(compute_empty_status(repository, targets), move |updates| {
            Message::EmptyStatusComputed(version, updates)
        })
    }

    /// Recompute the per-row sidebar index (lane fold, shortest-unique-prefix
    /// lengths, selected-row index) after the commit graph changes. O(n) once
    /// per graph load, so the per-frame `view()` stays O(visible rows).
    fn rebuild_sidebar_index(&mut self) {
        // The compact lane store (`graph`) is built by the loader and assigned
        // from `BackendOutput` in the `BackendLoaded` handler; here we only
        // refresh the cheap per-row indices that depend on the commit list.
        self.sidebar_prefix_lens = sidebar::shortest_unique_prefix_lens(&self.commits);
        self.selected_commit_index = self.find_selected_commit_index();
    }

    fn find_selected_commit_index(&self) -> Option<usize> {
        match &self.selected_revision {
            RevisionSelection::WorkingCopy => {
                self.commits.iter().position(|row| row.is_working_copy())
            }
            RevisionSelection::Commit(id) => self
                .commits
                .iter()
                .position(|row| !row.is_working_copy() && id == row.commit_id()),
        }
    }

    /// One-pass `commit-id hex → index` lookup in the loaded log for the wanted
    /// hexes (early-exit once all are found). Used to order the bookmark menus by
    /// proximity to a reference revision — far cheaper than a graph-distance
    /// revset, and the index distance matches the sidebar's visual order.
    fn commit_indices<'a>(
        &self,
        wanted: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, usize> {
        let want: std::collections::HashSet<&str> = wanted.into_iter().collect();
        let mut out: HashMap<String, usize> = HashMap::new();
        if want.is_empty() {
            return out;
        }
        for (index, row) in self.commits.iter().enumerate() {
            let id = row.commit_id();
            if want.contains(id) {
                out.entry(id.to_owned()).or_insert(index);
                if out.len() == want.len() {
                    break;
                }
            }
        }
        out
    }

    /// Stable-sort `items` in place so each item's target commit (via
    /// `target_of`) sits nearest-first to `reference` in the loaded log. Items
    /// whose target — or the reference — isn't loaded sink to the bottom, where
    /// the caller's prior ordering (e.g. an alphabetical pre-sort) breaks ties.
    fn sort_by_proximity<T>(
        &self,
        items: &mut [T],
        reference: Option<&str>,
        target_of: impl Fn(&T) -> &str,
    ) {
        let index_of = self.commit_indices(items.iter().map(&target_of).chain(reference));
        let reference_index = reference.and_then(|r| index_of.get(r).copied());
        items.sort_by_key(|item| proximity_key(&index_of, reference_index, target_of(item)));
    }

    /// Every known remote-tracking bookmark as `(branch, remote)`, ordered
    /// nearest-first to the working copy (alphabetical tiebreak). Shared by the
    /// native fetch menu and the iced fallback so they list identically.
    pub(crate) fn remote_branches_by_proximity(&self) -> Vec<(String, String)> {
        // (branch, remote, target-commit-hex) per known remote bookmark.
        let mut branches: Vec<(String, String, String)> = self
            .bookmarks
            .bookmarks
            .iter()
            .flat_map(|entry| {
                entry
                    .remotes
                    .iter()
                    .map(move |r| (entry.name.clone(), r.remote.clone(), r.target.clone()))
            })
            .collect();
        branches.sort(); // alphabetical baseline (stable tiebreak below)
        branches.dedup();
        let reference = self.bookmarks.working_copy_commit.clone();
        self.sort_by_proximity(&mut branches, reference.as_deref(), |(_, _, t)| t.as_str());
        branches
            .into_iter()
            .map(|(branch, remote, _)| (branch, remote))
            .collect()
    }

    /// (Re)start the initial load for the active tab's repository — a streaming
    /// cold load for jj, a one-shot load for git. Resets the per-repo view
    /// fields first, so a re-kick (after returning to a tab whose load was
    /// abandoned while it sat in the background) starts from a clean slate.
    fn kick_initial_load(&mut self) -> Task<Message> {
        self.kick_load(None)
    }

    /// As [`kick_initial_load`], with an optional activity label (defaults to
    /// "Load <repo>"). This is the **streaming cold load** — it clears the graph
    /// and regrows it as batches arrive, for the initial open / tab activation
    /// where there's nothing on screen to preserve. A revset switch instead uses
    /// the atomic-swap load in [`evaluate_revset`] to avoid flashing.
    fn kick_load(&mut self, activity_label: Option<String>) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        // A previous load's activity is being superseded by this (re)load —
        // resolve it so it doesn't spin forever.
        self.finish_load_activity(activity::ActivityStatus::Done, None);
        self.status = LoadStatus::Loading;
        self.loading_since = Some(Instant::now());
        self.selected_revision = RevisionSelection::WorkingCopy;
        self.pending_revision = Some(RevisionSelection::WorkingCopy);
        self.pending_revision_reveal = false;
        self.selected_file = 0;
        self.document = DiffDocument::default();
        self.commits = CommitStore::default();
        self.graph = graph_layout::GraphLayout::default();
        self.sidebar_prefix_lens.clear();
        self.selected_commit_index = None;
        self.repository_snapshot = None;
        self.snapshot_pending = false;
        // Wrap the load in a determinate activity; its progress handle is what
        // the loader bumps, so the toolbar progress line + popover track it.
        let label =
            activity_label.unwrap_or_else(|| format!("Load {}", repo_label(&repository.root).1));
        let (activity_id, progress) = self.begin_activity(label, true);
        self.pending_load_activity = Some(activity_id);
        self.commit_progress = progress.clone();
        // Fresh version so a backgrounded load's late batches are dropped.
        let version = self.allocate_load_version();
        self.commits_version = version;
        let revset = self.revset.clone();
        match repository.vcs {
            Vcs::Jj => {
                self.load = Some(LoadCursor {
                    version,
                    ..Default::default()
                });
                stream_jj_initial_load(repository, revset, progress, version)
            }
            Vcs::Git => {
                self.load = None;
                let revision = RevisionSelection::WorkingCopy;
                Task::perform(
                    load_backend(repository, revision.clone(), revset, progress),
                    move |result| Message::BackendLoaded(revision, Box::new(result)),
                )
            }
        }
    }

    /// Hand out the next streaming-load version. Monotonic across every tab
    /// and reload, so a backgrounded load's late batches never collide with
    /// the active tab's cursor.
    fn allocate_load_version(&mut self) -> u64 {
        self.next_load_version = self.next_load_version.wrapping_add(1);
        self.next_load_version
    }

    /// Hand out the next activity id (monotonic across every tab).
    fn allocate_activity_id(&mut self) -> activity::ActivityId {
        let id = activity::ActivityId(self.next_activity_id);
        self.next_activity_id = self.next_activity_id.wrapping_add(1);
        id
    }

    /// Start an activity on the active tab's log, returning its id and the
    /// progress handle the worker reports through.
    fn begin_activity(
        &mut self,
        label: impl Into<String>,
        determinate: bool,
    ) -> (activity::ActivityId, LoadProgress) {
        let id = self.allocate_activity_id();
        let progress = self.activities.start(id, label, determinate);
        (id, progress)
    }

    /// The active tab's stable id, if any tab is open.
    fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.get(self.active_tab).map(|tab| tab.id)
    }

    /// The activity log for `tab_id`: the inline one when it's the active tab,
    /// else the matching stash. `None` if the tab has since closed.
    fn activity_log_for(&mut self, tab_id: TabId) -> Option<&mut activity::ActivityLog> {
        if self.active_tab_id() == Some(tab_id) {
            return Some(&mut self.activities);
        }
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.stash.as_mut())
            .map(|state| &mut state.activities)
    }

    /// Finish a remote op's activity (fetch / undo) on its tab, recording the
    /// captured output or error, then — on success, if it's still the active
    /// tab — reload so the new commits/state appear.
    fn finish_remote_op(
        &mut self,
        tab_id: TabId,
        id: activity::ActivityId,
        result: Result<Vec<String>, String>,
    ) -> Task<Message> {
        let ok = result.is_ok();
        if let Some(log) = self.activity_log_for(tab_id) {
            match result {
                Ok(lines) => {
                    log.extend_output(id, lines);
                    log.finish(id, activity::ActivityStatus::Done, None);
                }
                Err(error) => {
                    log.append_output(id, error.clone());
                    log.finish(id, activity::ActivityStatus::Error, Some(error));
                }
            }
        }
        if ok && self.active_tab_id() == Some(tab_id) {
            self.start_repository_snapshot(RefreshOrigin::Focus)
        } else {
            Task::none()
        }
    }

    /// Finish the activity that wraps the in-flight graph (re)load, if one is
    /// tracked for this tab. Called from the terminal load handlers.
    fn finish_load_activity(&mut self, status: activity::ActivityStatus, result: Option<String>) {
        if let Some(id) = self.pending_load_activity.take() {
            self.activities.finish(id, status, result);
        }
    }

    /// Toolbar "Refresh": a full reload (working-copy snapshot + graph re-walk),
    /// surfaced as an activity. No-op while a load is already in flight.
    fn toolbar_refresh(&mut self) -> Task<Message> {
        if self.repository.is_none() || self.snapshot_pending || self.load.is_some() {
            return Task::none();
        }
        let (id, _progress) = self.begin_activity("Refresh", false);
        self.pending_load_activity = Some(id);
        self.start_repository_snapshot(RefreshOrigin::Focus)
    }

    /// Toolbar "Fetch": fetch the given target (all remotes / one branch),
    /// surfaced as an activity whose expanded output shows the remote messages.
    /// On success `finish_remote_op` reloads so new commits appear.
    fn start_fetch(&mut self, target: FetchTarget) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        self.toolbar_menu = None;
        let label = match &target {
            FetchTarget::AllRemotes => "Fetch all remotes".to_owned(),
            FetchTarget::RemoteBranch { remote, branch } => format!("Fetch {branch}@{remote}"),
        };
        let (id, progress) = self.begin_activity(label, true);
        Task::perform(
            backend::fetch(repository, target, progress),
            move |result| Message::FetchCompleted(tab_id, id, Box::new(result)),
        )
    }

    /// Toolbar "Undo": revert the latest jj operation, surfaced as an activity.
    /// jj-only; `finish_remote_op` reloads on success.
    fn start_undo(&mut self) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        if !matches!(repository.vcs, Vcs::Jj) {
            return Task::none();
        }
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        let (id, _progress) = self.begin_activity("Undo", false);
        Task::perform(backend::undo(repository), move |result| {
            Message::UndoCompleted(tab_id, id, Box::new(result))
        })
    }

    /// Re-evaluate the log against the current `self.revset` (Enter in the
    /// revset input, or a preset pick), surfaced as an activity, persisting the
    /// filter for this repo.
    ///
    /// Uses the **atomic-swap** load (`load_backend` → `BackendLoaded`) rather
    /// than the streaming cold load: the current graph/diff stay on screen the
    /// whole time and are replaced in one shot when the new walk is ready, so
    /// switching revsets doesn't flash an empty sidebar. The selection is kept
    /// (it just won't be highlighted if it falls outside the new set).
    fn evaluate_revset(&mut self) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        self.toolbar_menu = None;
        // Persist the new filter (debounced) for this repo.
        self.mark_geometry_dirty();
        // Supersede any prior in-flight load (cold stream or a previous eval) so
        // its results/activity don't linger.
        self.finish_load_activity(activity::ActivityStatus::Done, None);
        self.load = None;

        let shown = self.revset.trim();
        let label = if shown.is_empty() {
            "Evaluate revset: all()".to_owned()
        } else {
            format!("Evaluate revset: {shown}")
        };
        let (id, progress) = self.begin_activity(label, true);
        self.pending_load_activity = Some(id);
        self.commit_progress = progress.clone();

        // Keep the current view; only `pending_revision` is set, which lights
        // the toolbar progress line. `BackendLoaded` swaps the graph atomically.
        let revision = self.selected_revision.clone();
        self.pending_revision = Some(revision.clone());
        self.loading_since = Some(Instant::now());
        let revset = self.revset.clone();
        Task::perform(
            load_backend(repository, revision.clone(), revset, progress),
            move |result| Message::BackendLoaded(revision, Box::new(result)),
        )
    }

    /// Move the active tab's inline view state out into a `RepoState`, leaving
    /// the inline fields at cheap placeholders. Always paired with an
    /// immediate `restore_active_state` of the incoming tab. Keep the field
    /// list in sync with `RepoState`.
    fn stash_active_state(&mut self) -> RepoState {
        RepoState {
            repository: self.repository.take(),
            status: std::mem::replace(&mut self.status, LoadStatus::Loading),
            document: std::mem::take(&mut self.document),
            commits: std::mem::take(&mut self.commits),
            selected_revision: std::mem::replace(
                &mut self.selected_revision,
                RevisionSelection::WorkingCopy,
            ),
            file_list_expanded: self.file_list_expanded,
            pending_revision: self.pending_revision.take(),
            repository_snapshot: self.repository_snapshot.take(),
            snapshot_pending: self.snapshot_pending,
            selected_file: self.selected_file,
            revision_details: self.revision_details.take(),
            branch_status: self.branch_status.take(),
            bookmarks: std::mem::take(&mut self.bookmarks),
            revision_reveal_token: self.revision_reveal_token,
            pending_revision_reveal: self.pending_revision_reveal,
            commits_version: self.commits_version,
            graph: std::mem::take(&mut self.graph),
            sidebar_prefix_lens: std::mem::take(&mut self.sidebar_prefix_lens),
            selected_commit_index: self.selected_commit_index.take(),
            commit_progress: std::mem::take(&mut self.commit_progress),
            loading_since: self.loading_since.take(),
            empty_cache: std::mem::take(&mut self.empty_cache),
            load: self.load.take(),
            revset: std::mem::take(&mut self.revset),
            activities: std::mem::take(&mut self.activities),
            pending_load_activity: self.pending_load_activity.take(),
        }
    }

    /// Move a stashed `RepoState` into the inline fields, making it the active
    /// view. The previous inline state is overwritten (its caller has already
    /// stashed it, or is intentionally discarding it).
    fn restore_active_state(&mut self, state: RepoState) {
        self.repository = state.repository;
        self.status = state.status;
        self.document = state.document;
        self.commits = state.commits;
        self.selected_revision = state.selected_revision;
        self.file_list_expanded = state.file_list_expanded;
        self.pending_revision = state.pending_revision;
        self.repository_snapshot = state.repository_snapshot;
        self.snapshot_pending = state.snapshot_pending;
        self.selected_file = state.selected_file;
        self.revision_details = state.revision_details;
        self.branch_status = state.branch_status;
        self.bookmarks = state.bookmarks;
        self.revision_reveal_token = state.revision_reveal_token;
        self.pending_revision_reveal = state.pending_revision_reveal;
        self.commits_version = state.commits_version;
        self.graph = state.graph;
        self.sidebar_prefix_lens = state.sidebar_prefix_lens;
        self.selected_commit_index = state.selected_commit_index;
        self.commit_progress = state.commit_progress;
        self.loading_since = state.loading_since;
        self.empty_cache = state.empty_cache;
        self.load = state.load;
        self.revset = state.revset;
        self.activities = state.activities;
        self.pending_load_activity = state.pending_load_activity;
    }

    /// Switch to the tab `id`: stash the current active tab, restore the
    /// target's state, scroll its selection back into view, and kick a load if
    /// it hasn't loaded yet (or its load was abandoned while backgrounded). A
    /// fully-loaded tab is restored instantly and losslessly.
    fn activate_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(target) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        if target == self.active_tab {
            return Task::none();
        }
        // Persist the new active tab so it's re-focused next launch.
        self.mark_geometry_dirty();

        let current = self.stash_active_state();
        self.tabs[self.active_tab].stash = Some(current);
        self.active_tab = target;
        // Inactive tabs always carry a stash; the fallback only guards against
        // an impossible invariant break.
        let restored = self.tabs[target]
            .stash
            .take()
            .unwrap_or_else(RepoState::empty);
        self.restore_active_state(restored);
        // The sidebar widget's scroll offset is shared across tabs, so nudge it
        // to reveal this repo's restored selection.
        self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
        self.ensure_active_loaded()
    }

    /// Kick a load for the active tab unless it's already loaded. A tab that
    /// has never loaded — or whose load was abandoned while backgrounded — has
    /// `status != Loaded`, so activating it (re)starts the load.
    fn ensure_active_loaded(&mut self) -> Task<Message> {
        if self.repository.is_some() && !matches!(self.status, LoadStatus::Loaded) {
            self.kick_initial_load()
        } else {
            Task::none()
        }
    }

    /// Close the tab `id`. Closing an inactive tab just drops it; closing the
    /// active tab activates a neighbour (previous, else next), or falls back to
    /// the empty state when it was the last tab.
    fn close_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        // The open-tab set is changing — re-persist the session.
        self.mark_geometry_dirty();

        if index != self.active_tab {
            self.tabs.remove(index);
            if index < self.active_tab {
                self.active_tab -= 1;
            }
            return Task::none();
        }

        if self.tabs.len() == 1 {
            self.tabs.clear();
            self.active_tab = 0;
            self.restore_active_state(RepoState::empty());
            return Task::none();
        }

        // Prefer the previous neighbour, matching the design's close behaviour.
        let neighbour = if index > 0 { index - 1 } else { 1 };
        let neighbour_id = self.tabs[neighbour].id;
        self.tabs.remove(index);
        self.active_tab = self
            .tabs
            .iter()
            .position(|tab| tab.id == neighbour_id)
            .unwrap_or(0);
        // Overwriting the inline fields here discards the closed tab's state.
        let restored = self.tabs[self.active_tab]
            .stash
            .take()
            .unwrap_or_else(RepoState::empty);
        self.restore_active_state(restored);
        self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
        self.ensure_active_loaded()
    }

    /// Resolve `raw` to a repository and open it as a tab (or focus it if it's
    /// already open). On failure the dialog stays open with the reason shown.
    fn open_repository(&mut self, raw: &str) -> Task<Message> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Task::none();
        }
        match prepare_repository(&expand_user_path(trimmed)) {
            Ok(repository) => {
                self.open_repo_dialog = None;
                self.push_recent_repo(&repository.root);
                // Re-persist the session with the newly-opened repo.
                self.mark_geometry_dirty();
                if let Some(existing) = self.tabs.iter().find(|tab| tab.root == repository.root) {
                    let id = existing.id;
                    return self.activate_tab(id);
                }
                let (owner, name) = repo_label(&repository.root);
                let id = TabId(self.next_tab_id);
                self.next_tab_id += 1;
                let was_empty = self.tabs.is_empty();
                let revset = default_revset(repository.vcs);
                self.tabs.push(Tab {
                    id,
                    owner,
                    name,
                    vcs: repository.vcs,
                    root: repository.root.clone(),
                    stash: Some(RepoState::unloaded(Some(repository), revset)),
                });
                if was_empty {
                    // No active tab to switch from — check the new one out
                    // directly.
                    self.active_tab = 0;
                    if let Some(state) = self.tabs[0].stash.take() {
                        self.restore_active_state(state);
                    }
                    self.ensure_active_loaded()
                } else {
                    self.activate_tab(id)
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if let Some(dialog) = self.open_repo_dialog.as_mut() {
                    dialog.error = Some(message);
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let theme = self.resolved_theme().spec();

        // No repositories open: the empty state owns the whole window.
        if self.tabs.is_empty() {
            return container(empty_state(self, theme))
                .height(Length::Fill)
                .width(Length::Fill)
                .style(move |_| app_shell_style(theme))
                .into();
        }

        let tab_bar = tab_bar::build_tab_bar(self, theme);
        let toolbar = toolbar::build_toolbar(self, theme);

        // Body is always the sidebar + diff panes. All loading feedback lives in
        // the toolbar now (progress line + activity indicator) — there's no
        // full-window cold-load takeover and no diff-pane spinner. On a cold
        // load the sidebar simply grows from empty as batches arrive; a revision
        // switch keeps the prior diff until `DiffLoaded` replaces it.
        let sidebar = sidebar::build_sidebar(self, theme);
        let diff_pane = diff_panel::build_diff_panel(self, theme);
        let panels = row![sidebar, vertical_divider(theme), diff_pane]
            .spacing(0)
            .height(Length::Fill);
        let resize_overlay = ResizeHandle::new(
            self.sidebar_width,
            self.sidebar_min_width,
            sidebar::RESIZE_HIT_PADDING,
            Message::SidebarWidthChanged,
        );
        let palette_overlay = palette::build_overlay(self, theme);
        let body: Element<'_, Message> = stack![panels, resize_overlay, palette_overlay]
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let shell = column![tab_bar, toolbar, horizontal_divider(theme), body]
            .width(Length::Fill)
            .height(Length::Fill);

        // Overlays float above the whole shell. Each returns an empty `Space`
        // when inactive, so they can always be stacked.
        let content: Element<'_, Message> = stack![
            shell,
            activity::activity_popover(self, theme),
            toolbar::build_menu_overlay(self, theme),
            tab_bar::build_open_repo_dialog(self, theme),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        container(content)
            .padding(0)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(move |_| app_shell_style(theme))
            .into()
    }

    fn theme(&self) -> Theme {
        self.resolved_theme().iced_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        // Three-track keyboard handling:
        //   * global: owns ⌘K / ⌘F (overlay entry) and j/k/arrow file nav
        //     when nothing is open
        //   * palette track: ↑/↓/Tab/Esc when the palette is open
        //   * find track: Enter/Shift+Enter/Esc when the find bar is open
        // The text input still consumes character keys when focused, so
        // typing inside an overlay never falls through to file nav.
        // We *must* see Esc even when an iced text_input is focused —
        // text_input captures Esc to clear focus, and `keyboard::listen()`
        // only fires for `Status::Ignored` events, so we'd lose Esc to
        // the input and force the user to press Esc twice (once to
        // unfocus, once to close). `event::listen_with` ignores the
        // capture status and gives us every event, so the palette / find
        // overlays close on the first Esc regardless of focus.
        //
        // Subscription closures must be non-capturing, so we hand the
        // open/closed flags in through `Subscription::with`, which
        // becomes part of the subscription identity and arrives as a
        // tuple alongside each event.
        let flags = (
            self.palette.is_some(),
            self.find.is_some(),
            self.open_repo_dialog.is_some(),
            self.toolbar_menu.is_some(),
            self.activity_popover_open,
        );

        let keyboard = event::listen_with(|event, status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                // `ignored` = no focused widget consumed it. The revset input is
                // inline (not behind an overlay flag), so we use this to keep its
                // keystrokes from leaking into the global j/k file nav.
                Some((key, modifiers, matches!(status, event::Status::Ignored)))
            }
            _ => None,
        })
        .with(flags)
        .filter_map(
            |(
                (palette_open, find_open, dialog_open, menu_open, popover_open),
                (key, modifiers, ignored),
            )| {
                // A toolbar dropdown / activity popover is open: Esc dismisses it
                // and other keys are swallowed so they don't reach file nav.
                if menu_open || popover_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => Some(if menu_open {
                            Message::CloseToolbarMenu
                        } else {
                            Message::ActivityToggle
                        }),
                        _ => None,
                    };
                }

                // Cmd/Ctrl+K opens (or toggles closed) the palette.
                if modifiers.command()
                    && matches!(
                        key.as_ref(),
                        keyboard::Key::Character("k") | keyboard::Key::Character("K")
                    )
                {
                    return Some(if palette_open {
                        Message::PaletteClose
                    } else {
                        Message::PaletteOpen
                    });
                }

                // Cmd/Ctrl+F opens the in-diff find bar. No toggle; Esc
                // closes.
                if modifiers.command()
                    && matches!(
                        key.as_ref(),
                        keyboard::Key::Character("f") | keyboard::Key::Character("F")
                    )
                {
                    return Some(Message::FindOpen);
                }

                // Open-repo dialog owns the keyboard: Esc dismisses, everything
                // else falls through to its text input. (Enter is handled by the
                // input's `on_submit`.)
                if dialog_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::OpenRepoDialogClose)
                        }
                        _ => None,
                    };
                }

                if palette_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::PalettePopColumn)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                            Some(Message::PaletteMoveSelection(1))
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                            Some(Message::PaletteMoveSelection(-1))
                        }
                        keyboard::Key::Named(keyboard::key::Named::Tab) => {
                            Some(Message::PalettePushActions)
                        }
                        _ => None,
                    };
                }

                if find_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::FindClose)
                        }
                        // Enter / Shift+Enter — handle both here since
                        // text_input intentionally has no on_submit (it
                        // would route every Enter to FindNext and swallow
                        // Shift+Enter on the way).
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(if modifiers.shift() {
                                Message::FindPrev
                            } else {
                                Message::FindNext
                            })
                        }
                        _ => None,
                    };
                }

                // Tab management — only with no overlay holding the keyboard, so
                // these never steal keystrokes from a focused text input. ⌘W
                // closes the active tab, ⌘O opens the path dialog, ⌘1–9 jump to a
                // tab by position.
                if modifiers.command() && !modifiers.shift() && !modifiers.alt() {
                    match key.as_ref() {
                        keyboard::Key::Character("w") | keyboard::Key::Character("W") => {
                            return Some(Message::CloseActiveTab);
                        }
                        keyboard::Key::Character("o") | keyboard::Key::Character("O") => {
                            return Some(Message::OpenRepoDialogOpen);
                        }
                        keyboard::Key::Character(c) => {
                            if let Some(digit) = c.chars().next().and_then(|c| c.to_digit(10))
                                && (1..=9).contains(&digit)
                            {
                                return Some(Message::SelectTabIndex((digit - 1) as usize));
                            }
                        }
                        _ => {}
                    }
                }

                // No overlay — global j/k/arrow file shortcuts apply. Only
                // fire when no modifier is held, otherwise ⌘J / ⌘K combos
                // would also trigger file nav.
                if modifiers.command() || modifiers.alt() || modifiers.control() {
                    return None;
                }
                // A focused widget (the revset input) consumed this key — don't also
                // route it to file nav.
                if !ignored {
                    return None;
                }
                match key.as_ref() {
                    keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                    | keyboard::Key::Character("j") => Some(Message::SelectNextFile),
                    keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                    | keyboard::Key::Character("k") => Some(Message::SelectPreviousFile),
                    _ => None,
                }
            },
        );

        let window_events = event::listen().filter_map(|event| match event {
            Event::Window(window::Event::Focused) => Some(Message::WindowFocusChanged(true)),
            Event::Window(window::Event::Unfocused) => Some(Message::WindowFocusChanged(false)),
            Event::Window(window::Event::Opened { position, size, .. }) => {
                Some(Message::WindowOpened(position, size))
            }
            Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized(size)),
            Event::Window(window::Event::Moved(position)) => Some(Message::WindowMoved(position)),
            _ => None,
        });
        // Watch the working tree for changes instead of polling. The
        // subscription identity is keyed on the repo root, so the watcher
        // starts once and persists; `RefreshRepository` itself is gated on
        // focus, so edits made while unfocused are picked up on focus-regain.
        let refresh = match &self.repository {
            Some(repository) => Subscription::run_with(repository.root.clone(), watch_repository),
            None => Subscription::none(),
        };

        // Per-frame ticks during a palette push/pop animation. iced's
        // `Animation::interpolate_with` is read-only — to actually drive
        // the interpolation forward in time we need iced to keep
        // re-rendering. Subscribing to a 60-Hz timer while the animation
        // is in progress keeps the view function re-running; the handler
        // is a no-op, the side effect is the render itself.
        let palette_animating = self
            .palette
            .as_ref()
            .map(|p| p.is_animating(std::time::Instant::now()))
            .unwrap_or(false);
        let palette_tick = if palette_animating {
            time::every(Duration::from_millis(16)).map(|_| Message::PaletteTick)
        } else {
            Subscription::none()
        };

        // While anything is in flight — a load, a diff switch, or any running
        // activity (fetch/undo/push) — tick so the toolbar progress line +
        // spinner animate and reflect live progress.
        let work_in_flight = self.loading_since.is_some()
            || self.pending_revision.is_some()
            || self.activities.any_running();
        let loading_tick = if work_in_flight {
            time::every(Duration::from_millis(120)).map(|_| Message::LoadingTick)
        } else {
            Subscription::none()
        };

        // Drives the debounced geometry save. Active only while a change is
        // pending; the handler writes once the changes settle, then clears the
        // dirty flag, which tears this subscription back down.
        let window_state_tick = if self.geometry_dirty_since.is_some() {
            time::every(WINDOW_STATE_DEBOUNCE).map(|_| Message::PersistWindowState)
        } else {
            Subscription::none()
        };

        Subscription::batch([
            keyboard,
            window_events,
            refresh,
            palette_tick,
            loading_tick,
            window_state_tick,
            system::theme_changes().map(Message::SystemThemeChanged),
        ])
    }

    /// Re-center the native OS window controls (macOS traffic lights) on the tab
    /// strip. macOS pins them to the native title bar, so we reach the window on
    /// the main thread via `window::run` and nudge them through `chrome`. A
    /// no-op where the strip doesn't stand in for the title bar.
    fn reposition_window_controls(&self) -> Task<Message> {
        let Some(bar_height) = chrome::title_bar_height() else {
            return Task::none();
        };
        window::latest()
            .then(move |maybe_id| {
                maybe_id.map_or_else(Task::none, move |id| {
                    window::run(id, move |window| {
                        if let Ok(handle) = window.window_handle() {
                            chrome::position_window_controls(handle.as_raw(), bar_height);
                        }
                    })
                })
            })
            .discard()
    }

    /// Arm the native resize observer once the window exists, so the traffic
    /// lights track resizes on AppKit's timeline rather than a frame behind via
    /// the message loop. Called once on `WindowOpened`.
    fn install_resize_observer(&self) -> Task<Message> {
        let Some(bar_height) = chrome::title_bar_height() else {
            return Task::none();
        };
        window::latest()
            .then(move |maybe_id| {
                maybe_id.map_or_else(Task::none, move |id| {
                    window::run(id, move |window| {
                        if let Ok(handle) = window.window_handle() {
                            chrome::install_window_resize_observer(handle.as_raw(), bar_height);
                        }
                    })
                })
            })
            .discard()
    }

    /// Mark the persisted session (window geometry, sidebar width, open tabs)
    /// as changed, arming the debounce timer the subscription runs while a save
    /// is pending.
    fn mark_geometry_dirty(&mut self) {
        self.geometry_dirty_since = Some(Instant::now());
    }

    /// Record `root` as the most-recently-opened repository (deduped, newest
    /// first, capped). Surfaced by the open dialog's quick-pick list.
    fn push_recent_repo(&mut self, root: &std::path::Path) {
        let key = root.to_string_lossy().into_owned();
        self.recent_repos.retain(|existing| existing != &key);
        self.recent_repos.insert(0, key);
        self.recent_repos.truncate(RECENT_REPOS_MAX);
    }

    /// Snapshot the current geometry + sidebar width into the persisted form.
    fn current_window_state(&self) -> WindowState {
        WindowState {
            width: Some(self.window_size.width),
            height: Some(self.window_size.height),
            x: self.window_position.map(|p| p.x),
            y: self.window_position.map(|p| p.y),
            sidebar_width: Some(self.sidebar_width),
            open_repos: self
                .tabs
                .iter()
                .map(|tab| tab.root.to_string_lossy().into_owned())
                .collect(),
            active_repo: self
                .tabs
                .get(self.active_tab)
                .map(|tab| tab.root.to_string_lossy().into_owned()),
            revsets: self.collect_revsets(),
            recent_repos: self.recent_repos.clone(),
        }
    }

    /// Gather each open tab's revset (active inline + stashed), keyed by repo
    /// root, dropping empties so the persisted map stays tidy.
    fn collect_revsets(&self) -> BTreeMap<String, String> {
        let mut revsets = BTreeMap::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let revset = if index == self.active_tab {
                self.revset.clone()
            } else {
                tab.stash
                    .as_ref()
                    .map(|state| state.revset.clone())
                    .unwrap_or_default()
            };
            if !revset.is_empty() {
                revsets.insert(tab.root.to_string_lossy().into_owned(), revset);
            }
        }
        revsets
    }

    fn resolved_theme(&self) -> ResolvedTheme {
        self.selected_theme.active(self.system_theme)
    }
}

fn scroll_sidebar_to_file(_file_index: usize, _ui: &Diffui) -> Task<Message> {
    // TODO: re-implement scroll-to-reveal against `RevisionList`'s internal
    // scroll state once the widget exposes a scrollable operation.
    Task::none()
}

/// Map a sidebar row key back to the app's revision selection enum.
fn selection_from_key(key: &revision_list::RowSelectionKey) -> RevisionSelection {
    match key {
        revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
        revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id.clone()),
    }
}

/// Centered empty state shown when no repositories are open (e.g. after
/// closing the last tab, or when the launch path wasn't a repository).
fn empty_state<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let heading = match &ui.status {
        LoadStatus::Failed(error) => format!("Couldn't open repository: {error}"),
        _ => "No repositories open".to_owned(),
    };
    let body = column![
        text(heading)
            .size(15)
            .color(theme.text)
            .font(ui.config.ui_font),
        text("Press \u{2318}O or + to open a repository.")
            .size(12)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
    ]
    .spacing(8)
    .align_x(alignment::Horizontal::Center);

    container(body)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

/// Split a repository root into a dimmed `owner` (the parent directory name)
/// and an emphasized `name` (the directory name) for the tab label, e.g.
/// `/Users/me/code/diffui` → (`code`, `diffui`). The owner is empty when the
/// root has no usable parent.
/// One entry in the revision context menu's action table. The native popup
/// returns the chosen leaf's index into a `Vec<MenuAction>`; this records what
/// to do with that choice.
#[derive(Debug, Clone)]
enum MenuAction {
    /// A jj mutation (new / edit / abandon / bookmark op).
    Mutate(mutations::MutationOp),
    /// Copy a value already in hand (revision id, commit hash, a bookmark name).
    CopyText(String),
    /// Copy author / committer / the full description — read on demand. The
    /// in-memory graph keeps only the description's first line and no dates, so
    /// these need a fresh read; `fallback` is copied if that read fails.
    CopyDetail {
        field: DetailField,
        fallback: String,
    },
}

#[derive(Debug, Clone, Copy)]
enum DetailField {
    Author,
    Committer,
    Description,
}

/// Format a `Copy → {Author,Committer,Description}` value from a freshly-read
/// revision. Signatures render as `Name <email>  <timestamp>` (absent parts
/// skipped); the description is the full message. Returns `None` when the field
/// is absent (no committer, empty description), so the caller can fall back.
fn format_detail(details: &RevisionDetails, field: DetailField) -> Option<String> {
    fn signature(sig: &SignatureInfo) -> Option<String> {
        let mut out = sig.name.clone();
        if !sig.email.is_empty() {
            out.push_str(&format!(" <{}>", sig.email));
        }
        if let Some(timestamp) = &sig.timestamp {
            if !out.is_empty() {
                out.push_str("  ");
            }
            out.push_str(timestamp);
        }
        (!out.is_empty()).then_some(out)
    }
    match field {
        DetailField::Author => signature(&details.author),
        DetailField::Committer => details.committer.as_ref().and_then(signature),
        DetailField::Description => {
            let description = details.description.trim_end();
            (!description.is_empty()).then(|| description.to_owned())
        }
    }
}

pub(crate) fn repo_label(root: &Path) -> (String, String) {
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let owner = root
        .parent()
        .and_then(|parent| parent.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (owner, name)
}

/// Contract a leading `$HOME` back to `~` for display — the inverse of
/// `expand_user_path`, used so recent-repo rows show the short `~/code/foo`
/// form rather than the absolute path.
pub(crate) fn contract_user_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_owned()
}

/// Expand a leading `~` / `~/` to `$HOME` so the open-repo dialog accepts the
/// shell-style paths users type. Everything else is passed through verbatim;
/// `prepare_repository` canonicalizes from there.
fn expand_user_path(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(input)
}

/// Open `url` in the user's default browser via the platform opener. Used by
/// clickable links in an activity's captured remote output (e.g. a GitHub
/// "create a pull request" URL). Fire-and-forget — a failure to launch is
/// non-fatal and silently ignored.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = command.spawn();
}

/// Sort key for ordering a bookmark by how close its `target` commit sits to a
/// reference revision in the loaded log: the absolute index distance, or
/// `usize::MAX` when either commit isn't loaded (so unknowns sink to the bottom
/// and a stable sort keeps the prior alphabetical order among them).
fn proximity_key(
    index_of: &HashMap<String, usize>,
    reference_index: Option<usize>,
    target: &str,
) -> usize {
    match (index_of.get(target), reference_index) {
        (Some(&target_index), Some(reference_index)) => target_index.abs_diff(reference_index),
        _ => usize::MAX,
    }
}

fn commit_for_ref<'a>(ui: &'a Diffui, item: &ResultRef) -> Option<RowView<'a>> {
    match item {
        ResultRef::Commit(id) => ui.commits.find_by_change_id(id),
        ResultRef::Bookmark(name) => ui
            .commits
            .iter()
            .find(|c| c.bookmarks().iter().any(|b| b == name)),
        ResultRef::WorkingCopy => ui.commits.working_copy(),
        _ => None,
    }
}

/// Concatenate the currently-selected file's diff into plain text. Used by
/// the palette's "Copy current file diff" command. Mirrors `git diff`'s
/// hunk-then-rows format closely enough that pasted output reads correctly.
fn current_file_diff_text(ui: &Diffui) -> Option<String> {
    let file = ui.document.files.get(ui.selected_file)?;
    let mut out = String::new();
    out.push_str(&format!("diff: {}\n", file.path));
    if let Some(old) = &file.old_path
        && old != &file.path
    {
        out.push_str(&format!("--- {old}\n+++ {}\n", file.path));
    }
    for hunk in &file.hunks {
        out.push_str(&hunk.header);
        out.push('\n');
        for line in &hunk.lines {
            let prefix = match line.kind {
                diff_view::DiffLineKind::Addition => '+',
                diff_view::DiffLineKind::Deletion => '-',
                diff_view::DiffLineKind::Context => ' ',
                diff_view::DiffLineKind::Conflict => '!',
                diff_view::DiffLineKind::Note => '\\',
            };
            out.push(prefix);
            out.push_str(&line.content);
            out.push('\n');
        }
    }
    Some(out)
}

/// Streaming cold load for a jj repo: snapshot the working copy, emit the
/// working-copy diff, then walk the graph emitting `CommitsBatch` messages so
/// the sidebar paints after the first batch instead of after the whole (up to
/// ~1M-row) history.
///
/// Mirrors `watch_repository`'s bridge: the heavy walk runs on a blocking task
/// and emits through an unbounded tokio channel; a forwarder relays to iced
/// with backpressure. Every message carries `version` so a superseded load's
/// batches are dropped (see `LoadCursor`).
fn stream_jj_initial_load(
    repository: Repository,
    revset: String,
    progress: LoadProgress,
    version: u64,
) -> Task<Message> {
    // First batch ships after this many commits — small enough that the first
    // screenful paints quickly, large enough that ~1M commits don't flood the
    // update loop with batch messages.
    const COMMIT_BATCH_SIZE: usize = 256;

    Task::stream(iced::stream::channel(
        16,
        async move |mut output: futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

            let worker = tokio::task::spawn_blocking(move || {
                let handle = tokio::runtime::Handle::current();
                handle.block_on(async move {
                    // `load_jj_cold` snapshots the working copy first (so the
                    // graph + diff reflect on-disk state), then reuses that one
                    // loaded repo for the diff + walk. `emit_diff` fires the
                    // working-copy diff (this doesn't lift the loading screen —
                    // the first `CommitsBatch` does); `emit_batch` fires per
                    // commit batch.
                    let tx_diff = tx.clone();
                    let mut emit_diff = move |diff| {
                        let _ = tx_diff.send(Message::InitialDiff(version, Box::new(diff)));
                    };
                    let tx_batches = tx.clone();
                    let mut emit_batch = move |batch: Vec<StreamRow>| {
                        let _ = tx_batches.send(Message::CommitsBatch(version, batch));
                    };
                    let finished = crate::jj::load_jj_cold(
                        repository,
                        revset,
                        progress,
                        COMMIT_BATCH_SIZE,
                        &mut emit_diff,
                        &mut emit_batch,
                    )
                    .await
                    .map(
                        |(snapshot, branch_status, empty_updates, bookmarks)| CommitsTail {
                            snapshot,
                            empty_updates,
                            branch_status,
                            bookmarks,
                        },
                    )
                    .map_err(|error| format!("{error:#}"));
                    let _ = tx.send(Message::CommitsFinished(version, Box::new(finished)));
                });
            });

            // Relay worker messages to iced, honoring its backpressure.
            while let Some(message) = rx.recv().await {
                if output.send(message).await.is_err() {
                    break;
                }
            }
            let _ = worker.await;
        },
    ))
}

/// Filesystem-watch subscription: emits `RefreshRepository` (debounced) when
/// the working tree changes. Replaces the old fixed-interval poll — between
/// edits there is zero work, and the off-thread snapshot only runs when files
/// actually change.
///
/// `.git` / `.jj` are deliberately not treated as relevant: watching them
/// would feed our own snapshot's writes back as events (a refresh loop) and
/// bury real edits under VCS-internal churn. External VCS operations surface
/// through window focus-regain instead.
// `&PathBuf` (not `&Path`) is required: `Subscription::run_with` keys on
// `D = PathBuf` and hands the builder a `fn(&D)`.
#[allow(clippy::ptr_arg)]
fn watch_repository(root: &PathBuf) -> Pin<Box<dyn Stream<Item = Message> + Send>> {
    let root = root.clone();
    iced::stream::channel(
        8,
        async move |mut output: futures::channel::mpsc::Sender<Message>| {
            // notify's handler runs on its own thread; bridge it to this async
            // task over an unbounded channel so the handler never blocks.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let watcher =
                notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                    if let Ok(event) = result
                        && !matches!(event.kind, notify::EventKind::Access(_))
                        && event_touches_worktree(&event)
                    {
                        let _ = tx.send(());
                    }
                });
            let mut watcher = match watcher {
                Ok(watcher) => watcher,
                Err(error) => {
                    eprintln!(
                        "diffui: filesystem watcher unavailable, auto-refresh disabled: {error}"
                    );
                    return;
                }
            };
            if let Err(error) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
                eprintln!(
                    "diffui: failed to watch {}, auto-refresh disabled: {error}",
                    root.display()
                );
                return;
            }

            // Coalesce bursts: wait for an event, then keep draining until the
            // tree goes quiet for `WATCH_DEBOUNCE`, then emit a single refresh.
            while rx.recv().await.is_some() {
                while tokio::time::timeout(WATCH_DEBOUNCE, rx.recv())
                    .await
                    .is_ok()
                {}
                if output.send(Message::RefreshRepository).await.is_err() {
                    break;
                }
            }

            // Hold the watcher for the lifetime of the stream.
            drop(watcher);
        },
    )
    .boxed()
}

/// Whether any path in `event` lies outside `.git` / `.jj` — i.e. it's a
/// working-tree change we should refresh on, rather than VCS-internal churn.
fn event_touches_worktree(event: &notify::Event) -> bool {
    event.paths.iter().any(|path| {
        !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Normal(name) if name == ".git" || name == ".jj"
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{expand_user_path, repo_label};
    use std::path::{Path, PathBuf};

    #[test]
    fn repo_label_splits_owner_and_name() {
        assert_eq!(
            repo_label(Path::new("/Users/me/code/diffui")),
            ("code".to_owned(), "diffui".to_owned())
        );
        // A root-level repo has no usable parent name → empty owner.
        assert_eq!(
            repo_label(Path::new("/diffui")),
            (String::new(), "diffui".to_owned())
        );
        // A bare relative name likewise yields no owner.
        assert_eq!(
            repo_label(Path::new("repo")),
            (String::new(), "repo".to_owned())
        );
    }

    #[test]
    fn expand_user_path_passes_through_non_tilde() {
        assert_eq!(
            expand_user_path("/abs/path/repo"),
            PathBuf::from("/abs/path/repo")
        );
        assert_eq!(
            expand_user_path("relative/repo"),
            PathBuf::from("relative/repo")
        );
        // A tilde mid-path is not a home shortcut and must be left intact.
        assert_eq!(expand_user_path("/etc/we~rd"), PathBuf::from("/etc/we~rd"));
    }

    #[test]
    fn expand_user_path_expands_leading_tilde() {
        // Only assert when HOME is available (it is in normal test runs);
        // skip otherwise rather than mutate process-global env.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(expand_user_path("~"), home);
            assert_eq!(expand_user_path("~/code/repo"), home.join("code/repo"));
        }
    }
}
