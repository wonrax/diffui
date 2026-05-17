use std::{
    collections::HashMap,
    path::PathBuf,
    pin::Pin,
    time::{Duration, Instant},
};

mod backend;
mod config;
mod diff_panel;
mod diff_view;
mod find;
mod git;
mod graph;
mod graph_layout;
mod graph_view;
mod jj;
mod mutations;
mod palette;
mod repository;
mod resize_handle;
mod revision_list;
mod scrollbar;
mod sidebar;
mod theme;

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
    BackendOutput, CommitStore, CommitsTail, DiffDocument, LoadProgress, RevisionDetails,
    RevisionSelection, RowView, Selection, StreamRow, compute_empty_status, load_backend,
    load_diff, load_repository_snapshot,
};
use clap::Parser;
use config::AppConfig;
use find::FindState;
use futures::{SinkExt, Stream, StreamExt};
use iced::theme as iced_theme;
use iced::{
    Element, Length, Subscription, Task, Theme, alignment,
    event::{self, Event},
    keyboard, system, time,
    widget::{self, column, container, progress_bar, row, stack, text, text_editor},
    window,
};
use notify::Watcher;
use palette::{
    ColumnSource, CommandId as PaletteCommand, PaletteState, Recents, ResultRef,
    change_id_for_recents, revision_selection,
};
use repository::{Repository, RepositorySnapshot, Vcs, prepare_repository};
use resize_handle::ResizeHandle;
use theme::{ResolvedTheme, ThemePreference, ThemeSpec, app_shell_style, vertical_divider};

/// Quiet period after the last filesystem event before we refresh. A single
/// editor save typically emits a burst of events; coalescing them avoids
/// snapshotting several times for one logical change.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// Debounce before re-running the palette matcher. It scans every commit, so
/// coalescing fast typing keeps the input responsive on large repos.
const PALETTE_QUERY_DEBOUNCE: Duration = Duration::from_millis(120);

fn main() -> iced::Result {
    let cli = Cli::parse();

    iced::application(
        move || Diffui::new(cli.clone()),
        Diffui::update,
        Diffui::view,
    )
    .title("diffui")
    .subscription(Diffui::subscription)
    .theme(Diffui::theme)
    .run()
}

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Native GUI diff viewer for jj and git")]
struct Cli {
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct Diffui {
    pub(crate) repository: Option<Repository>,
    pub(crate) status: LoadStatus,
    pub(crate) document: DiffDocument,
    pub(crate) commits: CommitStore,
    /// Multi-select aware revision selection. The `primary` field is what
    /// drives the diff view and the backend reload; `additional` is the
    /// rest of the multi-selection (cmd/shift-click) that destructive ops
    /// act on. See `backend::Selection`.
    pub(crate) selection: Selection,
    /// Sticky inline-file-list preference. Always reflects whether the
    /// *primary* revision's file list is shown; the user toggles it by
    /// re-clicking the primary row, and the value persists across
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
    pub(crate) config: AppConfig,
    pub(crate) revision_details: Option<RevisionDetails>,
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
    /// the result — at that point `selection.primary` is still the old
    /// value, so the sidebar would scroll the wrong row into view. The
    /// `BackendLoaded` handler reads this flag once the new revision has
    /// actually been written into `selection.primary`, *then* bumps the
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
    /// Current transient notification (e.g. "Reverted operation a1b2c3 —
    /// ⌘Z to undo"). `None` when no toast is visible.
    pub(crate) toast: Option<Toast>,
    /// Monotonic counter handed out as each toast's `generation`. Stale
    /// dismiss tasks (fired by an earlier toast's timer) compare against
    /// this so they don't accidentally clear a later toast.
    pub(crate) next_toast_generation: u64,
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

/// Transient bottom-right notification banner. Shown after a mutation or
/// any other op the user should be aware of. Auto-dismisses after a
/// fixed delay; the `generation` token lets a stale dismiss task fired
/// from an earlier toast no-op against the current one.
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub kind: ToastKind,
    pub message: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ToastKind {
    Success,
    Error,
}

/// How Enter should be interpreted when the palette is open. Picker
/// columns route Enter through their text_input's `on_submit`; op-pad
/// columns route through this global handler. We use the same `⌘⏎`
/// binding for every op pad (whether or not it has a message editor) so
/// the keybinding is consistent across the surface — plain Enter inside
/// the message editor stays free to insert a newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PaletteSubmitMode {
    /// No palette open, or top column handles Enter itself.
    None,
    /// Top is an op pad — `⌘⏎` / `Ctrl+⏎` applies.
    CmdEnter,
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
    SelectRowKey(
        revision_list::RowSelectionKey,
        revision_list::SelectionGesture,
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
    /// Forwarded from the op-pad's message text_editor. Applied to the
    /// topmost column's `OpDraft.message` Content.
    OpPadMessageAction(text_editor::Action),
    /// Fired when a jj mutation task started by `apply_op_pad` (or by a
    /// keyboard shortcut like ⌘Z) completes. Always followed by a
    /// commits/diff reload via `BackendLoaded`.
    MutationApplied(Box<Result<mutations::MutationOutcome, String>>),
    /// ⌘Z global shortcut — fires `op undo` against the current head.
    /// Skipped while the palette or find bar is open so the user's
    /// in-flight text input keeps its own undo.
    ApplyOpUndo,
    /// Stale-token-aware toast dismissal. The handler ignores the
    /// message unless `generation` matches the current toast.
    DismissToast(u64),
    /// Hotkey shortcut: open the palette and push an op-pad column for
    /// the given mutation command pre-filled from the current selection.
    /// Bypasses the palette's search step entirely.
    OpenOpPadFor(palette::CommandId),
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
}

impl Diffui {
    fn new(cli: Cli) -> (Self, Task<Message>) {
        let config = AppConfig::load();
        let sidebar_min_width = sidebar::min_width(config);
        match prepare_repository(&cli.path) {
            Ok(repository) => {
                let commit_progress = LoadProgress::default();
                // jj streams the cold load for a progressive first paint; git
                // loads in one shot (its `git log` parse isn't incremental).
                let (backend_task, load, commits_version) = match repository.vcs {
                    Vcs::Jj => {
                        let version = 1;
                        (
                            stream_jj_initial_load(
                                repository.clone(),
                                commit_progress.clone(),
                                version,
                            ),
                            Some(LoadCursor {
                                version,
                                ..Default::default()
                            }),
                            version,
                        )
                    }
                    Vcs::Git => {
                        let revision = RevisionSelection::WorkingCopy;
                        (
                            Task::perform(
                                load_backend(
                                    repository.clone(),
                                    revision.clone(),
                                    commit_progress.clone(),
                                ),
                                move |result| Message::BackendLoaded(revision, Box::new(result)),
                            ),
                            None,
                            0,
                        )
                    }
                };
                let theme_task = system::theme().map(Message::SystemThemeChanged);

                (
                    Self {
                        repository: Some(repository),
                        status: LoadStatus::Loading,
                        document: DiffDocument::default(),
                        commits: CommitStore::default(),
                        selection: Selection::new(RevisionSelection::WorkingCopy),
                        file_list_expanded: true,
                        pending_revision: Some(RevisionSelection::WorkingCopy),
                        repository_snapshot: None,
                        snapshot_pending: false,
                        app_focused: true,
                        selected_theme: ThemePreference::System,
                        system_theme: iced_theme::Mode::None,
                        selected_file: 0,
                        sidebar_width: sidebar::DEFAULT_WIDTH.max(sidebar_min_width),
                        sidebar_min_width,
                        config,
                        revision_details: None,
                        palette: None,
                        recents: Recents::load(),
                        find: None,
                        revision_reveal_token: 0,
                        pending_revision_reveal: false,
                        commits_version,
                        graph: graph_layout::GraphLayout::default(),
                        sidebar_prefix_lens: Vec::new(),
                        selected_commit_index: None,
                        commit_progress,
                        loading_since: Some(Instant::now()),
                        empty_cache: HashMap::new(),
                        load,
                        toast: None,
                        next_toast_generation: 0,
                    },
                    Task::batch([backend_task, theme_task]),
                )
            }
            Err(error) => (
                Self {
                    repository: None,
                    status: LoadStatus::Failed(format!("{error:#}")),
                    document: DiffDocument::default(),
                    commits: CommitStore::default(),
                    selection: Selection::new(RevisionSelection::WorkingCopy),
                    file_list_expanded: true,
                    pending_revision: None,
                    repository_snapshot: None,
                    snapshot_pending: false,
                    app_focused: true,
                    selected_theme: ThemePreference::System,
                    system_theme: iced_theme::Mode::None,
                    selected_file: 0,
                    sidebar_width: sidebar::DEFAULT_WIDTH.max(sidebar_min_width),
                    sidebar_min_width,
                    config,
                    revision_details: None,
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
                    toast: None,
                    next_toast_generation: 0,
                },
                system::theme().map(Message::SystemThemeChanged),
            ),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(revision, result) => match *result {
                Ok(output) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    let revision_changed = self.selection.primary != revision;
                    self.selection.primary = revision;
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
                    self.revision_details = output.details;
                    self.selected_file = if revision_changed {
                        0
                    } else {
                        self.selected_file
                            .min(self.document.files.len().saturating_sub(1))
                    };
                    // If this load was triggered by the palette, the
                    // sidebar didn't yet know the new selection.primary
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
                    self.status = LoadStatus::Failed(error);
                }
            },
            Message::CommitsBatch(version, rows) => {
                // Take the cursor out so the appends below borrow `self` fields
                // freely (same idiom the palette uses). Drop batches from a
                // superseded load — their row indices no longer line up.
                let Some(mut cursor) = self.load.take().filter(|c| c.version == version) else {
                    return Task::none();
                };
                let selecting_wc = matches!(self.selection.primary, RevisionSelection::WorkingCopy);
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
                        // Apply the single-parent emptiness resolved in the
                        // loader's final pass, caching each so reloads skip it.
                        for (index, empty) in tail.empty_updates {
                            let commit_id = self.commits.row(index).commit_id().to_owned();
                            self.empty_cache.insert(commit_id, empty);
                            self.commits.set_is_empty(index, empty);
                        }
                        self.commits_version = self.commits_version.wrapping_add(1);
                        self.selected_commit_index = self.find_selected_commit_index();
                        // Fill in the merges/roots the loader left unknown.
                        return self.resolve_empty_status();
                    }
                    Err(error) => {
                        self.status = LoadStatus::Failed(error);
                        self.loading_since = None;
                    }
                }
            }
            Message::InitialDiff(version, result) => {
                // Apply only while this stream is the active load and the user
                // hasn't navigated off the working copy (e.g. via the palette
                // during load). Leaves `status` as `Loading` so the full-window
                // indicator stays up until the first commit batch.
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

                    let revision_changed = self.selection.primary != revision;
                    self.selection.primary = revision;
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
                            if matches!(self.selection.primary, RevisionSelection::WorkingCopy) {
                                let revision = self.selection.primary.clone();
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
                            let revision = self.selection.primary.clone();
                            self.pending_revision = Some(revision.clone());
                            self.loading_since = Some(Instant::now());
                            let progress = LoadProgress::default();
                            self.commit_progress = progress.clone();
                            return Task::perform(
                                load_backend(repository, revision.clone(), progress),
                                move |result| Message::BackendLoaded(revision, Box::new(result)),
                            );
                        }
                    }
                }
            }
            Message::RepositorySnapshotLoaded(_, Err(error)) => {
                self.snapshot_pending = false;
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
            Message::SelectRowKey(key, gesture) => {
                let target = revision_from_row_key(key);
                match gesture {
                    revision_list::SelectionGesture::Replace => {
                        // Re-clicking the current primary toggles its file
                        // list without re-running the backend or changing
                        // the diff. The toggled value persists across
                        // revision switches.
                        if self.selection.primary == target {
                            self.file_list_expanded = !self.file_list_expanded;
                            self.selection.additional.clear();
                        } else {
                            let load_needed = self.pending_revision.as_ref() != Some(&target);
                            // A plain click drops any multi-select set —
                            // user has "committed" to a single row.
                            self.selection.additional.clear();
                            if load_needed && let Some(repository) = self.repository.clone() {
                                self.pending_revision = Some(target.clone());
                                self.loading_since = Some(Instant::now());
                                let revision = target.clone();
                                // A revision switch leaves the commit graph
                                // intact — only the diff changes — so load
                                // just the diff instead of re-walking the
                                // (up to ~1M-row) history.
                                return Task::perform(
                                    load_diff(repository, target),
                                    move |result| Message::DiffLoaded(revision, Box::new(result)),
                                );
                            }
                        }
                    }
                    revision_list::SelectionGesture::Toggle => {
                        // Toggle on the primary is a no-op (we always need
                        // a diff target). Toggle on any other row flips
                        // its membership in `additional`. No reload.
                        self.selection.toggle(target);
                    }
                    revision_list::SelectionGesture::RangeExtend => {
                        // Build the visible ordering from the current
                        // commits list and fill `additional` with the
                        // contiguous span from primary to target.
                        let ordered: Vec<RevisionSelection> =
                            self.commits.iter().map(row_to_revision).collect();
                        self.selection.extend_range(target, &ordered);
                    }
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
            }
            Message::SystemThemeChanged(theme) => {
                self.system_theme = theme;
            }
            Message::WindowFocusChanged(focused) => {
                let gained_focus = focused && !self.app_focused;
                self.app_focused = focused;

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
                self.sidebar_width = width.max(self.sidebar_min_width);
            }
            Message::PaletteOpen => {
                if self.palette.is_none() {
                    // Mutually exclusive with the find bar: opening the
                    // palette pulls keyboard focus and the find bar would
                    // sit behind the modal anyway.
                    self.find = None;
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
            Message::OpPadMessageAction(action) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(draft) = state.top_op_draft_mut()
                {
                    draft.message.perform(action);
                }
            }
            Message::MutationApplied(result) => {
                return self.handle_mutation_result(*result);
            }
            Message::ApplyOpUndo => {
                if self.palette.is_some() || self.find.is_some() {
                    // Defer to in-app text widgets' own undo when the
                    // user has them open. ⌘Z otherwise routes to op undo.
                    return Task::none();
                }
                let Some(repository) = self.repository.clone() else {
                    return Task::none();
                };
                return Task::perform(
                    mutations::run_mutation(repository, mutations::MutationOp::OpUndo),
                    |result| Message::MutationApplied(Box::new(result)),
                );
            }
            Message::DismissToast(generation) => {
                if matches!(&self.toast, Some(t) if t.generation == generation) {
                    self.toast = None;
                }
            }
            Message::OpenOpPadFor(command) => {
                // Arity check up front so a hotkey on the wrong shape of
                // selection gives a clean error toast instead of opening
                // an unfillable op pad.
                let count = self.selection.count();
                let shape = command.mutation_shape();
                let arity_ok = shape
                    .map(|s| match s.source_arity {
                        palette::Arity::Zero => true,
                        palette::Arity::One => count == 1,
                        palette::Arity::OneOrMany => count >= 1,
                    })
                    .unwrap_or(true);
                if !arity_ok {
                    return self.show_toast(
                        ToastKind::Error,
                        format!(
                            "{} needs {} — you have {} selected",
                            command.label(),
                            match shape.map(|s| s.source_arity) {
                                Some(palette::Arity::One) => "exactly 1 revision",
                                Some(palette::Arity::OneOrMany) => "at least 1 revision",
                                _ => "no selection",
                            },
                            count,
                        ),
                    );
                }
                let mut state = palette::PaletteState::open(self);
                state.push_op_pad(command, self);
                self.palette = Some(state);
                self.recents.push_command(command);
                self.recents.save();
                return op_pad_focus_task(command);
            }
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
                // Mutually exclusive with the palette: same keyboard focus
                // arbiter, and stacking the palette over a find bar makes
                // the find bar look broken.
                self.palette = None;
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

    /// Handle ⏎ in the palette. In a Root `:` query the all-commits scan is
    /// too slow to run per keystroke on a 1M-commit repo, so ⏎ opens it in a
    /// dedicated commit-search column (the scan runs once as that column is
    /// pushed). Inside that column ⏎ re-runs the scan after an edit, or — once
    /// results are showing — accepts the highlighted row like any other mode.
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

    /// Read by the subscription to decide whether Enter should apply
    /// the current op pad. See `PaletteSubmitMode` for the modes.
    fn palette_submit_mode(&self) -> PaletteSubmitMode {
        let Some(state) = &self.palette else {
            return PaletteSubmitMode::None;
        };
        let Some(top) = state.top() else {
            return PaletteSubmitMode::None;
        };
        match &top.source {
            ColumnSource::OpPad(_) => PaletteSubmitMode::CmdEnter,
            _ => PaletteSubmitMode::None,
        }
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

        // Op-pad columns short-circuit: Enter = apply (or arm the
        // target slot if it's the next missing requirement).
        if let ColumnSource::OpPad(draft) = &top.source {
            return self.apply_op_pad(draft.clone());
        }

        // Target picker: Enter on a revision row fills the op-pad
        // below's `placement_target` and pops back to it.
        if matches!(top.source, ColumnSource::OpPadTargetPicker) {
            let Some(selected) = top.matches.get(top.selected) else {
                return Task::none();
            };
            let item = selected.item.clone();
            let Some(rev) = palette::revision_selection(&item, self) else {
                return Task::none();
            };
            if let Some(state) = self.palette.as_mut() {
                state.fill_target_and_pop(rev);
            }
            return Task::none();
        }

        let Some(selected) = top.matches.get(top.selected) else {
            return Task::none();
        };
        let item = selected.item.clone();
        let target = match &top.source {
            ColumnSource::Root => None,
            ColumnSource::Actions(t) => Some(t.clone()),
            ColumnSource::OpPad(_) | ColumnSource::OpPadTargetPicker => {
                unreachable!("handled above")
            }
        };

        match (&top.source, &item) {
            // Top-level: a mutation command pushes an op pad column;
            // built-in commands run inline; revision/file rows
            // primary-action without going through the Actions column.
            (ColumnSource::Root, ResultRef::Command(cmd)) if cmd.is_mutation() => {
                self.recents.push_command(*cmd);
                self.recents.save();
                if let Some(mut state) = self.palette.take() {
                    state.push_op_pad(*cmd, self);
                    self.palette = Some(state);
                }
                op_pad_focus_task(*cmd)
            }
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
            (ColumnSource::OpPad(_) | ColumnSource::OpPadTargetPicker, _) => {
                unreachable!("handled above");
            }
            _ => Task::none(),
        }
    }

    /// Apply an op-pad draft. If the op needs a destination and none
    /// has been picked, push a destination-picker column instead and let
    /// the user fill the slot first. Otherwise translate the draft into
    /// a `MutationOp` and fire the async mutation task.
    fn apply_op_pad(&mut self, draft: palette::OpDraft) -> Task<Message> {
        let needs_target = match draft.command {
            // `new` uses its source list directly as parents; there's no
            // separate target slot for the `Onto` placement we support
            // today.
            palette::CommandId::New => false,
            other => other
                .mutation_shape()
                .map(|s| !s.allowed_placements.is_empty())
                .unwrap_or(false),
        };

        if needs_target && draft.placement_target.is_none() {
            // Arm the slot — push a picker column on top of the op pad
            // and let the user pick a destination. The picker's accept
            // pops back to the op pad with the slot filled.
            if let Some(mut state) = self.palette.take() {
                state.push_target_picker(self);
                self.palette = Some(state);
            }
            return Task::none();
        }

        self.palette = None;
        let Some(op) = mutation_from_draft(&draft) else {
            return self.show_toast(
                ToastKind::Error,
                format!("{}: not yet wired", draft.command.label()),
            );
        };
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        Task::perform(mutations::run_mutation(repository, op), |result| {
            Message::MutationApplied(Box::new(result))
        })
    }

    /// Display `message` as a toast of `kind`, returning a `Task` that
    /// auto-dismisses it after a fixed delay. The dismiss is
    /// generation-aware so a stale timer can't clear a later toast.
    fn show_toast(&mut self, kind: ToastKind, message: String) -> Task<Message> {
        let generation = self.next_toast_generation;
        self.next_toast_generation = self.next_toast_generation.wrapping_add(1);
        self.toast = Some(Toast {
            kind,
            message,
            generation,
        });
        Task::perform(
            async move {
                tokio::time::sleep(Duration::from_millis(4000)).await;
            },
            move |_| Message::DismissToast(generation),
        )
    }

    /// Shared handler for the `MutationApplied` message — show a toast +
    /// kick off the post-mutation reload of commits/diff for the
    /// currently-primary revision.
    fn handle_mutation_result(
        &mut self,
        result: Result<mutations::MutationOutcome, String>,
    ) -> Task<Message> {
        match result {
            Ok(outcome) => {
                let toast = self.show_toast(
                    ToastKind::Success,
                    format!("{} — ⌘Z to undo", outcome.message),
                );
                // Mutations like abandon / squash / describe can change
                // or remove the commit_id our primary points at. Reset
                // to the working copy so the post-reload never lands on
                // a stale id. A future polish: switch the primary key
                // from commit_id to change_id so non-destructive
                // rewrites (describe, rebase) preserve focus.
                self.selection.primary = RevisionSelection::WorkingCopy;
                self.selection.additional.clear();
                let reload = self.start_post_mutation_reload();
                Task::batch([toast, reload])
            }
            Err(error) => self.show_toast(ToastKind::Error, error),
        }
    }

    /// Re-run the backend against the current `selection.primary`. Used
    /// after a successful mutation so the commit list / diff reflect the
    /// new state. Mirrors the pattern in `RepositorySnapshotLoaded`.
    fn start_post_mutation_reload(&mut self) -> Task<Message> {
        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };
        let revision = self.selection.primary.clone();
        self.pending_revision = Some(revision.clone());
        self.loading_since = Some(Instant::now());
        let progress = LoadProgress::default();
        self.commit_progress = progress.clone();
        Task::perform(
            load_backend(repository, revision.clone(), progress),
            move |result| Message::BackendLoaded(revision, Box::new(result)),
        )
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
            // Mutation commands are intercepted in `palette_accept_current`
            // and routed through `apply_op_pad` after the user fills the
            // op-pad form. They should never reach this path — if they
            // do, it's a bug (e.g. an action menu surfacing a mutation
            // without going through `push_op_pad`).
            PaletteCommand::New
            | PaletteCommand::Edit
            | PaletteCommand::Abandon
            | PaletteCommand::Describe
            | PaletteCommand::Squash
            | PaletteCommand::Rebase
            | PaletteCommand::OpUndo
            | PaletteCommand::BookmarkSet
            | PaletteCommand::BookmarkDelete => {
                debug_assert!(false, "mutation command reached run_palette_command");
                Task::none()
            }
        }
    }

    fn jump_to_revision_ref(&mut self, target: &ResultRef) -> Task<Message> {
        let Some(selection) = revision_selection(target, self) else {
            return Task::none();
        };

        // Already current — no load, no async wait, bump the token now so
        // the next render scrolls the sidebar row into view.
        if self.selection.primary == selection {
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
        match &self.selection.primary {
            RevisionSelection::WorkingCopy => {
                self.commits.iter().position(|row| row.is_working_copy())
            }
            RevisionSelection::Commit(id) => self
                .commits
                .iter()
                .position(|row| !row.is_working_copy() && id == row.commit_id()),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let theme = self.resolved_theme().spec();

        // A loading indicator appears only after a short grace period, so
        // quick loads don't flash one. `dots` animates a simple ellipsis.
        let loading_visible = self
            .loading_since
            .is_some_and(|since| since.elapsed() >= Duration::from_millis(500));
        let dots = self
            .loading_since
            .map_or(0, |since| (since.elapsed().as_millis() / 350 % 4) as usize);

        // Startup: nothing to render behind a spinner yet, so the progress
        // indicator takes over the whole window.
        if loading_visible && matches!(self.status, LoadStatus::Loading) {
            let body = loading_indicator(
                format!("Loading repository{}", ".".repeat(dots)),
                Some(self.commit_progress.snapshot()),
                theme,
            );
            return container(body)
                .height(Length::Fill)
                .width(Length::Fill)
                .style(move |_| app_shell_style(theme))
                .into();
        }

        // The sidebar builds rows on demand from the precomputed per-row index
        // (lane fold + prefix lengths), so constructing it each frame is
        // O(visible rows) — no memoization needed.
        let sidebar = sidebar::build_sidebar(self, theme);

        // A revision's diff is loading: swap just the diff pane for an
        // indicator, leaving the commit graph and selection in place.
        let diff_pane: Element<'_, Message> = if loading_visible
            && self.pending_revision.is_some()
            && matches!(self.status, LoadStatus::Loaded)
        {
            loading_indicator(format!("Loading diff{}", ".".repeat(dots)), None, theme)
        } else {
            diff_panel::build_diff_panel(self, theme)
        };

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
        let toast_overlay = build_toast_overlay(self, theme);
        let content = stack![panels, resize_overlay, palette_overlay, toast_overlay]
            .width(Length::Fill)
            .height(Length::Fill);

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
            self.palette_submit_mode(),
        );

        let keyboard = event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some((key, modifiers))
            }
            _ => None,
        })
        .with(flags)
        .filter_map(
            |((palette_open, find_open, submit_mode), (key, modifiers))| {
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

                // Cmd/Ctrl+Z routes to `op undo` whenever there isn't an in-
                // app text widget that owns its own undo. The handler in
                // `update` re-checks palette/find state defensively so a
                // keypress that races with an overlay close still does the
                // right thing.
                if (modifiers.command() || modifiers.control())
                    && !modifiers.shift()
                    && !palette_open
                    && !find_open
                    && matches!(
                        key.as_ref(),
                        keyboard::Key::Character("z") | keyboard::Key::Character("Z")
                    )
                {
                    return Some(Message::ApplyOpUndo);
                }

                if palette_open {
                    // Op-pad Enter handling. Picker columns route Enter
                    // through their text_input's `on_submit`, but op pads
                    // render a `text_editor` (no on_submit), so we fire
                    // PaletteAccept from here. Cmd/Ctrl+Enter when a message
                    // editor exists; plain Enter when it doesn't (Enter
                    // belongs to the editor for newlines otherwise).
                    if matches!(
                        key.as_ref(),
                        keyboard::Key::Named(keyboard::key::Named::Enter)
                    ) && matches!(submit_mode, PaletteSubmitMode::CmdEnter)
                        && (modifiers.command() || modifiers.control())
                        && !modifiers.shift()
                    {
                        return Some(Message::PaletteAccept);
                    }

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

                // No overlay — global j/k/arrow file shortcuts apply. Only
                // fire when no modifier is held, otherwise ⌘J / ⌘K combos
                // would also trigger file nav.
                if modifiers.command() || modifiers.alt() || modifiers.control() {
                    return None;
                }
                match key.as_ref() {
                    keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                    | keyboard::Key::Character("j") => Some(Message::SelectNextFile),
                    keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                    | keyboard::Key::Character("k") => Some(Message::SelectPreviousFile),
                    // Mutation hotkeys. All open an op pad pre-filled from
                    // the current selection; the handler in `update` does
                    // the arity check and shows an error toast if the
                    // selection is the wrong shape.
                    keyboard::Key::Character("n") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::New))
                    }
                    keyboard::Key::Character("e") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::Edit))
                    }
                    keyboard::Key::Character("x") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::Abandon))
                    }
                    keyboard::Key::Character("d") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::Describe))
                    }
                    keyboard::Key::Character("s") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::Squash))
                    }
                    keyboard::Key::Character("r") => {
                        Some(Message::OpenOpPadFor(palette::CommandId::Rebase))
                    }
                    _ => None,
                }
            },
        );

        let focus = event::listen().filter_map(|event| match event {
            Event::Window(window::Event::Focused) => Some(Message::WindowFocusChanged(true)),
            Event::Window(window::Event::Unfocused) => Some(Message::WindowFocusChanged(false)),
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

        // While a load is in flight, tick so the loading indicator can cross
        // its grace period, animate, and reflect live commit-load progress.
        let loading_tick = if self.loading_since.is_some() {
            time::every(Duration::from_millis(120)).map(|_| Message::LoadingTick)
        } else {
            Subscription::none()
        };

        Subscription::batch([
            keyboard,
            focus,
            refresh,
            palette_tick,
            loading_tick,
            system::theme_changes().map(Message::SystemThemeChanged),
        ])
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

/// Task that focuses the op-pad's message editor if the chosen command
/// has one. For ops without an editor (edit / abandon / rebase / etc.)
/// nothing is focused — keyboard input there is just `⌘⏎` to apply.
fn op_pad_focus_task(command: palette::CommandId) -> Task<Message> {
    let needs_message = command.mutation_shape().is_some_and(|s| s.needs_message);
    if needs_message {
        widget::operation::focus(palette::OP_PAD_MESSAGE_ID)
    } else {
        Task::none()
    }
}

/// Translate an `OpDraft` into a concrete `MutationOp` the mutations
/// module knows how to run. Returns `None` for ops that haven't been
/// wired yet (the user sees a stub toast in that case).
fn mutation_from_draft(draft: &palette::OpDraft) -> Option<mutations::MutationOp> {
    use palette::CommandId;
    match draft.command {
        CommandId::OpUndo => Some(mutations::MutationOp::OpUndo),
        CommandId::Describe => {
            let target = draft.source.first().cloned()?;
            Some(mutations::MutationOp::Describe {
                target,
                message: draft.message.text(),
            })
        }
        CommandId::Edit => {
            let target = draft.source.first().cloned()?;
            Some(mutations::MutationOp::Edit { target })
        }
        CommandId::Abandon => {
            if draft.source.is_empty() {
                return None;
            }
            Some(mutations::MutationOp::Abandon {
                targets: draft.source.clone(),
            })
        }
        CommandId::Squash => {
            if draft.source.is_empty() {
                return None;
            }
            let destination = draft.placement_target.clone()?;
            Some(mutations::MutationOp::Squash {
                sources: draft.source.clone(),
                destination,
            })
        }
        CommandId::Rebase => {
            if draft.source.is_empty() {
                return None;
            }
            let destination = draft.placement_target.clone()?;
            let placement = draft.placement_kind?;
            Some(mutations::MutationOp::Rebase {
                sources: draft.source.clone(),
                destination,
                placement,
            })
        }
        CommandId::New => {
            if draft.source.is_empty() {
                return None;
            }
            Some(mutations::MutationOp::New {
                parents: draft.source.clone(),
                message: draft.message.text(),
            })
        }
        CommandId::BookmarkSet => {
            let target = draft.source.first().cloned()?;
            Some(mutations::MutationOp::BookmarkSet {
                name: draft.message.text(),
                target,
            })
        }
        CommandId::BookmarkDelete => Some(mutations::MutationOp::BookmarkDelete {
            name: draft.message.text(),
        }),
        _ => None,
    }
}

/// Map a row-selection token from the sidebar widget back to the
/// app-level `RevisionSelection`. Working-copy rows carry no id; commit
/// rows carry the change-id string that `RowView::change_id` produces.
fn revision_from_row_key(key: revision_list::RowSelectionKey) -> RevisionSelection {
    match key {
        revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
        revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id),
    }
}

/// Mirror of `revision_from_row_key`, but for a `RowView` out of the
/// commit store. Keeps the working-copy row identified as `WorkingCopy`
/// rather than a `Commit(commit_id)` so it round-trips against the
/// sidebar's own keys (which carry the commit id, see `build_revision_row`).
fn row_to_revision(row: RowView<'_>) -> RevisionSelection {
    if row.is_working_copy() {
        RevisionSelection::WorkingCopy
    } else {
        RevisionSelection::Commit(row.commit_id().to_owned())
    }
}

/// Build the bottom-right toast overlay. Returns an empty space when no
/// toast is showing — the parent `stack![...]` is cheap to over-build.
fn build_toast_overlay<'a>(ui: &'a Diffui, theme: theme::ThemeSpec) -> Element<'a, Message> {
    use iced::widget::{Space, container, mouse_area, text};
    let Some(toast) = ui.toast.as_ref() else {
        return Space::new().into();
    };
    let (background, foreground) = match toast.kind {
        ToastKind::Success => (theme.accent, theme.background),
        ToastKind::Error => (theme.conflict_marker, theme.background),
    };
    let card = container(
        text(toast.message.clone())
            .size(13)
            .font(ui.config.ui_font)
            .color(foreground),
    )
    .padding(iced::Padding::from([10, 16]))
    .style(move |_| container::Style {
        background: Some(iced::Background::Color(background)),
        border: iced::Border {
            width: 0.0,
            color: iced::Color::TRANSPARENT,
            radius: 8.0.into(),
        },
        shadow: iced::Shadow {
            color: iced::Color {
                a: 0.30,
                ..iced::Color::BLACK
            },
            offset: iced::Vector::new(0.0, 6.0),
            blur_radius: 16.0,
        },
        ..container::Style::default()
    });

    // Click-to-dismiss: the toast captures clicks itself, the rest of the
    // overlay passes through so the user can keep working underneath.
    let generation = toast.generation;
    let clickable = mouse_area(card).on_press(Message::DismissToast(generation));

    container(clickable)
        .padding(iced::Padding {
            top: 0.0,
            right: 16.0,
            bottom: 24.0,
            left: 0.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom)
        .into()
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
                        progress,
                        COMMIT_BATCH_SIZE,
                        &mut emit_diff,
                        &mut emit_batch,
                    )
                    .await
                    .map(|(snapshot, empty_updates)| CommitsTail {
                        snapshot,
                        empty_updates,
                    })
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

/// Centered loading indicator. With a known total it renders a determinate
/// bar plus a commit count; otherwise just the (caller-animated) label.
fn loading_indicator(
    label: String,
    progress: Option<(usize, usize)>,
    theme: ThemeSpec,
) -> Element<'static, Message> {
    let mut body = column![text(label).size(16).color(theme.text)]
        .spacing(12)
        .align_x(alignment::Horizontal::Center);

    if let Some((loaded, total)) = progress {
        if total > 0 {
            body = body
                .push(
                    progress_bar(0.0..=total as f32, loaded as f32)
                        .length(Length::Fixed(240.0))
                        .girth(Length::Fixed(6.0)),
                )
                .push(
                    text(format!("{loaded} / {total} commits"))
                        .size(12)
                        .color(theme.muted_text),
                );
        } else if loaded > 0 {
            body = body.push(
                text(format!("{loaded} commits"))
                    .size(12)
                    .color(theme.muted_text),
            );
        }
    }

    container(body)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}
