use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, Instant},
};

mod activity;
mod chrome;
mod config;
mod diff_panel;
mod diff_view;
mod find;
mod graph_view;
mod icons;
#[cfg(target_os = "macos")]
mod macos_native;
mod menu;
mod menus;
mod message;
mod palette;
mod resize_handle;
mod revision_list;
mod scrollbar;
mod sidebar;
mod tab_bar;
mod tabs;
mod theme;
mod toolbar;
mod update;
mod window_state;

// Domain logic now lives in the headless `diffui-core` crate. Re-export the
// modules the app still reaches into by path (`crate::graph`, `crate::jj`,
// `crate::mutations`, …) so those call sites stay unchanged.
pub(crate) use diffui_core::{FetchTarget, github, graph, graph_layout, jj, mutations, repository};
pub(crate) use message::Message;

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

use clap::Parser;
use config::AppConfig;
use diffui_core::{
    CommitStore, CommitsTail, DiffDocument, DiffFile, LoadProgress, RevisionDetails,
    RevisionSelection, RowView, SignatureInfo, StreamRow,
};
use find::FindState;
use futures::{SinkExt, Stream, StreamExt};
use iced::theme as iced_theme;
use iced::{
    Background, Border, Color, Element, Length, Padding, Point, Size, Subscription, Task, Theme,
    alignment,
    event::{self, Event},
    font::Weight,
    keyboard, system, time,
    widget::{self, button, column, container, row, stack, text},
    window,
};
use palette::{
    ColumnSource, CommandId as PaletteCommand, PaletteState, Recents, ResultRef,
    change_id_for_recents, revision_selection,
};
use repository::{Repository, Vcs, prepare_repository};
use resize_handle::ResizeHandle;
use theme::{
    ResolvedTheme, ThemePreference, ThemeSpec, app_shell_style, emphasis_font, horizontal_divider,
    vertical_divider,
};
use window_state::WindowState;

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

/// Fallback duration for the custom zoom animation when AppKit's own
/// `animationResizeTime:` isn't available (e.g. un-zooming with no saved
/// duration). The real path queries the native value so the timing matches a
/// native zoom exactly; this is only a sane default.
pub(crate) const ZOOM_FALLBACK_SECS: f64 = 0.25;

/// True when two window frames (`[x, y, w, h]`) match within a pixel or two —
/// used to tell whether the window is already sitting at its zoom target.
pub(crate) fn frames_approx_eq(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 2.0)
}

/// The frame to un-zoom to when we have no saved pre-zoom frame (e.g. the window
/// opened already maximized): a centered 70% of the screen's visible area.
pub(crate) fn zoom_default_restore(visible: [f64; 4]) -> [f64; 4] {
    let [vx, vy, vw, vh] = visible;
    let (w, h) = (vw * 0.7, vh * 0.7);
    [vx + (vw - w) / 2.0, vy + (vh - h) / 2.0, w, h]
}

fn main() -> iced::Result {
    let cli = Cli::parse();

    // Positional paths are reserved for a future file-diff mode
    // (`diffui SIDE_1 SIDE_2`) and aren't implemented yet. Reject them up front
    // with a pointer to `--path`, so the pre-existing `diffui <repo>` muscle
    // memory gets a clear nudge instead of silently doing nothing.
    if !cli.diff_args.is_empty() {
        eprintln!(
            "diffui: file diff (diffui <side1> <side2>) isn't supported yet — \
             positional paths are reserved for it."
        );
        eprintln!("        to open a repository, use: diffui --path <repo>");
        std::process::exit(2);
    }

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
    .font(icons::ICON_FONT_BYTES)
    .window(window_settings)
    .subscription(Diffui::subscription)
    .theme(Diffui::theme)
    .run()
}

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Native GUI diff viewer for jj and git")]
struct Cli {
    /// Repository path to open as a tab; repeat to open several. When omitted,
    /// diffui restores your last session, falls back to the current directory
    /// if it's a repository, and otherwise opens the welcome screen.
    #[arg(short = 'p', long = "path", value_name = "REPO")]
    paths: Vec<PathBuf>,

    /// Reserved for a future file/dir diff mode (`diffui SIDE_1 SIDE_2`); not
    /// wired up yet. Kept as a positional (hidden from `--help`) so the old
    /// `diffui <repo>` habit gets a clear pointer to `--path` rather than
    /// silently opening the wrong thing — see the guard in `main`.
    #[arg(value_name = "PATH", hide = true)]
    diff_args: Vec<PathBuf>,
}

/// A revision-context-menu mutation captured for serial execution. Mutations
/// take jj's working-copy lock, so we run at most one of our own at a time and
/// queue the rest (see [`Diffui::enqueue_or_run_mutation`]). `progress` is the
/// handle the worker reports through, allocated alongside the activity up front
/// so a queued entry is fully wired before it ever starts.
#[derive(Debug, Clone)]
pub(crate) struct PendingMutation {
    repository: Repository,
    op: mutations::MutationOp,
    tab_id: TabId,
    activity_id: activity::ActivityId,
    progress: LoadProgress,
}

#[derive(Debug, Clone)]
pub(crate) struct Diffui {
    /// All per-repo **domain + orchestration** state for the active tab — the
    /// commit graph, the shown diff, the selection, and the load/refresh/version
    /// bookkeeping — owned by the headless core so the frontend neither
    /// reimplements nor hand-syncs it. Inactive tabs keep theirs in `Tab::stash`;
    /// switching tabs swaps the two. See [`Session`].
    pub(crate) session: Session,
    /// Sticky inline-file-list preference. Always reflects whether the
    /// *selected* revision's file list is shown; the user toggles it by
    /// re-clicking the selected row, and the value persists across
    /// revision switches so collapsing once stays collapsed for whatever
    /// revision the user moves to next.
    pub(crate) file_list_expanded: bool,
    pub(crate) app_focused: bool,
    pub(crate) selected_theme: ThemePreference,
    pub(crate) system_theme: iced_theme::Mode,
    pub(crate) selected_file: usize,
    pub(crate) sidebar_width: f32,
    /// Whether the diff pane wraps long lines (default) or clips them at the
    /// pane edge. Global across tabs, persisted with the window state.
    pub(crate) diff_wrap: bool,
    /// Two-column (side-by-side) diff layout. Global across tabs, persisted
    /// with the window state.
    pub(crate) diff_split: bool,
    /// Collapsed directories of the sidebar file tree, by full path prefix.
    /// Per-tab (stashed with the rest of the view state); collapsing once
    /// stays collapsed across revision switches within the tab.
    pub(crate) collapsed_dirs: HashSet<String>,
    /// View-time memo for the file list's stat-column widths and flattened file
    /// tree, keyed on document identity. Without it, the sidebar re-shaped ~5·N
    /// strings through `cosmic_text` on every diff-scroll file-boundary crossing
    /// (each crossing publishes `SelectFile`, which forces a full `view()`
    /// rebuild) and tanked the frame rate on large PRs. Interior-mutable because
    /// `view()` only has `&self`; not stashed per-tab since a tab switch
    /// reassigns `document_id`, which the cache keys already treat as a miss.
    /// See [`sidebar::SidebarFileCache`].
    pub(crate) sidebar_file_cache: std::cell::RefCell<sidebar::SidebarFileCache>,
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
    /// In-flight custom double-click zoom animation (macOS), or `None` when idle.
    /// We drive the zoom ourselves frame-by-frame instead of using AppKit's
    /// `performZoom:` so the layout re-flows each step like an edge-drag resize
    /// rather than the whole GPU surface morphing mid-animation. While `Some`,
    /// the subscription ticks it.
    pub(crate) zoom_anim: Option<ZoomAnim>,
    /// The pre-zoom window frame to restore on the next un-zoom (AppKit screen
    /// coords `[x, y, w, h]`) paired with the native animation duration used to
    /// zoom in — reused for the symmetric un-zoom so its timing matches without a
    /// second `animationResizeTime:` round-trip. `None` when not in our zoomed
    /// state.
    pub(crate) zoom_restore: Option<([f64; 4], f64)>,
    pub(crate) config: AppConfig,
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
    /// Bumped by [`scroll_sidebar_to_file`] when keyboard file navigation moves
    /// the selection. The sidebar's `RevisionList` reveals the selected file's
    /// row (which the sidebar computes from the current file tree) into view on
    /// the change. Separate from `revision_reveal_token` so revealing a file
    /// doesn't also re-centre the selected revision. Transient, so it isn't
    /// stashed per-tab.
    pub(crate) sidebar_file_reveal_token: u64,
    /// Last-known scroll offsets of the sidebar (content-space px) and diff
    /// view, kept current by the widgets' `on_scroll` callbacks. The widgets
    /// own their live offset in tree `State`, but that state is shared across
    /// tabs; mirroring it here lets [`stash_active_state`] save a per-tab
    /// position and [`restore_active_state`] push it back via
    /// `scroll_restore_token`.
    pub(crate) sidebar_scroll_offset: f64,
    pub(crate) diff_scroll_offset: f32,
    /// Bumped whenever a tab is (re)activated. The sidebar/diff widgets watch
    /// it and, on a change, jump to `sidebar_scroll_offset` / `diff_scroll_offset`
    /// — re-applying the restored tab's saved scroll over whatever the shared
    /// widget state leaked from the previous tab.
    pub(crate) scroll_restore_token: u64,
    /// Bumped whenever `document` is replaced (a diff reload, working-copy edit,
    /// or tab switch). The diff view watches it to drop its per-line shaped-
    /// paragraph cache, whose `(file, hunk, line)` keys would otherwise render
    /// stale text — most visibly between two tabs both on `@`, which share the
    /// constant `"working-copy"` revision key. Global/monotonic, not per-tab:
    /// since a tab restore reassigns `document` (and bumps this), returning to a
    /// tab correctly re-clears the cache the other tab populated. Set only via
    /// [`set_document`].
    pub(crate) document_version: u64,

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
    /// Monotonic source of document identities (`Session::document_id`),
    /// stamped on every document replacement across every tab. Background
    /// per-file work (syntax highlighting) routes its results by this id.
    pub(crate) next_document_id: u64,
    /// `Some` while the "open repository" path dialog is showing.
    pub(crate) open_repo_dialog: Option<OpenRepoDialog>,
    /// Most-recently-opened repo roots (newest first), surfaced as quick-pick
    /// rows in the open dialog. Seeded from / persisted to `WindowState`.
    pub(crate) recent_repos: Vec<String>,

    // ── Toolbar / activity / revset (per-tab where noted) ───────────────
    /// The active repo's default revset (its `revsets.log`, or jj's default) —
    /// what the "Default" preset in the revset menu applies. A derived cache of
    /// `default_revset(active repo)`, recomputed on every active-repo change
    /// (`restore_active_state`) rather than per render, so the menu never does
    /// config-file I/O while painting. Not stashed: it's re-derivable per repo.
    pub(crate) default_revset: String,
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
    /// Serial mutation execution (the core `MutationQueue`): at most one
    /// revision-menu mutation runs at a time (they contend on jj's working-copy
    /// lock); the rest drain in order. Global rather than per-tab — a
    /// `PendingMutation` carries its own repo/tab/activity, so cross-tab
    /// serialization costs nothing and avoids stash/restore churn.
    pub(crate) mutation_queue: diffui_core::session::MutationQueue<PendingMutation>,
    /// Open popup menu (toolbar fetch/revset dropdown or revision right-click),
    /// if any. macOS uses native `NSMenu`s instead and leaves this `None`.
    pub(crate) menu: Option<menu::OverlayMenu>,
    /// Modal confirmation for a guarded mutation (backwards bookmark move),
    /// or `None` when closed.
    pub(crate) confirm: Option<ConfirmDialog>,
    /// Whether the activity popover is showing.
    pub(crate) activity_popover_open: bool,
    /// The caret control the cursor is currently over, if any — drives the
    /// hover highlight that `mouse_area` (unlike `button`) doesn't provide.
    pub(crate) hovered: Option<HoverTarget>,
}

/// A modal confirmation gating a mutation the jj CLI would refuse outright
/// (today: a backwards/sideways bookmark move, where the CLI demands
/// `--allow-backwards`). Accept runs the held mutation; cancel resolves its
/// queued activity. The `PendingMutation` is fully self-addressed
/// (repo/tab/activity), so accepting still runs correctly after a tab switch.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmDialog {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) confirm_label: String,
    pub(crate) pending: PendingMutation,
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
    /// A title-bar tab, keyed by id — drives its inset hover fill. At most one is
    /// hovered at a time.
    Tab(TabId),
}

/// One in-flight custom zoom animation: interpolate the window frame from `from`
/// to `to` (AppKit screen coords `[x, y, w, h]`) over `duration` seconds, applying
/// each step as a non-animated `setFrame:` so the resize routes through the same
/// live-repaint path an edge-drag uses. `duration` is AppKit's own
/// `animationResizeTime:` for this resize, so the timing matches a native zoom;
/// `start` is the wall-clock the animation began and the tick handler eases
/// `elapsed / duration`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZoomAnim {
    pub(crate) start: Instant,
    pub(crate) from: [f64; 4],
    pub(crate) to: [f64; 4],
    pub(crate) duration: f64,
}

/// The per-tab state of an *inactive* tab: its core [`Session`] (all domain +
/// orchestration state, swapped as one unit) plus the handful of frontend-only
/// UI fields that ride alongside it. The active tab's copy lives in the `Diffui`
/// inline fields; `stash_active_state` / `restore_active_state` move a whole
/// `RepoState` between the two — the `Session` swaps atomically, so the old
/// 20-field domain hand-sync can no longer drift out of step.
#[derive(Debug, Clone)]
pub(crate) struct RepoState {
    pub(crate) session: Session,
    pub(crate) file_list_expanded: bool,
    pub(crate) collapsed_dirs: HashSet<String>,
    pub(crate) selected_file: usize,
    pub(crate) revision_reveal_token: u64,
    pub(crate) pending_revision_reveal: bool,
    pub(crate) sidebar_scroll_offset: f64,
    pub(crate) diff_scroll_offset: f32,
    pub(crate) activities: activity::ActivityLog,
    pub(crate) pending_load_activity: Option<activity::ActivityId>,
}

impl RepoState {
    /// A never-loaded tab for `repository`: a fresh `Session::unloaded` plus
    /// default UI state. `ensure_active_loaded` kicks the real load when this
    /// becomes the active tab (`status != Loaded`). `revset` is the persisted
    /// (or default) filter for this repo.
    fn unloaded(repository: Option<Repository>, revset: String) -> Self {
        Self {
            session: Session::unloaded(repository, revset),
            file_list_expanded: true,
            collapsed_dirs: HashSet::new(),
            selected_file: 0,
            revision_reveal_token: 0,
            pending_revision_reveal: false,
            sidebar_scroll_offset: 0.0,
            diff_scroll_offset: 0.0,
            activities: activity::ActivityLog::default(),
            pending_load_activity: None,
        }
    }

    /// A never-loaded GitHub-PR tab: a `Session` whose source is the PR. No
    /// local repository, so the watcher/snapshot/mutation machinery stays off
    /// and `ensure_active_loaded` routes to the streaming PR load.
    fn unloaded_pr(spec: &github::PrSpec) -> Self {
        Self {
            session: Session::for_source(diffui_core::SourceHandle::new(github::PrSource::new(
                spec.clone(),
            ))),
            ..Self::unloaded(None, String::new())
        }
    }

    /// The state when no repository is open at all (closed the last tab).
    /// `Session::empty` is `Loaded` so `view()` shows the empty state rather
    /// than a loading indicator.
    fn empty() -> Self {
        Self {
            session: Session::empty(),
            ..Self::unloaded(None, String::new())
        }
    }
}

/// The default revset for a freshly-opened repo of `vcs`, before any persisted
/// value is applied: jj shows `all()` (every visible head's ancestry — the
/// current hardcoded behavior); git falls back to its `git log` default (the
/// current branch's history), expressed as an empty range.
/// The revset a freshly-opened repo starts on when nothing is persisted for it.
/// jj mirrors `jj log` (the user's `revsets.log`, or jj's default); git keeps
/// its empty default (the backend's own "current branch" view).
fn default_revset(repository: &Repository) -> String {
    match repository.vcs {
        Vcs::Jj => crate::jj::jj_log_revset(&repository.root),
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

/// What a tab views: a local repository, or a GitHub pull request fetched
/// through the `gh` CLI. Equality is identity — it's what de-duplicates opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TabSource {
    Repo {
        vcs: Vcs,
        /// Repository root, used to de-duplicate opens and key the watcher.
        root: PathBuf,
    },
    GitHubPr(github::PrSpec),
}

/// One open tab: its identity + display metadata, plus the stashed per-tab
/// state while it's inactive. The active tab's `stash` is `None` — its state
/// is checked out into the `Diffui` inline fields.
#[derive(Debug, Clone)]
pub(crate) struct Tab {
    pub(crate) id: TabId,
    /// Dimmed prefix in the tab label — the repo root's parent directory, or
    /// the PR's GitHub org.
    pub(crate) owner: String,
    /// Emphasized name — the repo directory, or `repo#123` for a PR.
    pub(crate) name: String,
    pub(crate) source: TabSource,
    /// `None` for the active tab (state is inline); `Some` for an inactive
    /// tab (loaded, or a fresh `RepoState::unloaded`).
    pub(crate) stash: Option<RepoState>,
}

impl Tab {
    /// The local repository root, or `None` for a GitHub-PR tab. Persistence
    /// and the recent-repos list key off this, so PR tabs (session-only for
    /// now) drop out of both naturally.
    pub(crate) fn root(&self) -> Option<&Path> {
        match &self.source {
            TabSource::Repo { root, .. } => Some(root),
            TabSource::GitHubPr(_) => None,
        }
    }
}

/// Transient state of the open-repository path dialog.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenRepoDialog {
    pub(crate) path: String,
    /// Populated when the last submit failed to resolve a repository, so the
    /// dialog stays open with the reason shown.
    pub(crate) error: Option<String>,
}

// Domain + orchestration types now live in `diffui-core`'s orchestration
// engine — the streaming cold-load state (`ColdCursor`) + fold, the serial
// mutation queue, refresh coalescing, `RefreshOrigin`, `LoadStatus`, and the
// whole per-repo `Session`. Re-export so the app refers to them unqualified.
pub(crate) use diffui_core::session::{RefreshOrigin, coalesce_refresh};
pub(crate) use diffui_core::{LoadStatus, Session};

/// Reveal the keyboard-selected file's row in the sidebar tree. Bumps the
/// file-reveal token so the sidebar's `RevisionList` scrolls the file's row
/// (which the sidebar resolves from the current file tree) into view on the
/// next render. The scroll itself is a no-op when the file list is closed — the
/// sidebar passes `None` as the reveal target in that case.
fn scroll_sidebar_to_file(ui: &mut Diffui) -> Task<Message> {
    ui.sidebar_file_reveal_token = ui.sidebar_file_reveal_token.wrapping_add(1);
    Task::none()
}

/// Map a sidebar row key back to the app's revision selection enum.
fn selection_from_key(key: &revision_list::RowSelectionKey) -> RevisionSelection {
    match key {
        revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
        revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id.clone()),
    }
}

/// Welcome screen shown whenever no repository is open: a fresh launch with
/// nothing to restore (including from the macOS/Spotlight launcher, where cwd is
/// `/`), or after the last tab is closed. It's the "select a repo" entry point —
/// a primary open button (the strip's `+` isn't here) plus a click-to-reopen
/// list of recents. Only an explicit `--path` that failed to resolve surfaces an
/// error; the session and cwd fallbacks resolve silently to this screen instead.
fn empty_state<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let mut body = column![
        text("Diffui")
            .size(22)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
    ]
    .spacing(14)
    .align_x(alignment::Horizontal::Center);

    // A `Failed` status can now only come from an explicit `--path` (the session
    // and cwd fallbacks resolve silently), so it's always worth surfacing.
    if let LoadStatus::Failed(error) = &ui.session.status {
        body = body.push(
            text(format!("Couldn't open repository: {error}"))
                .size(12)
                .color(theme.removed_text)
                .font(ui.config.ui_font),
        );
    }

    body = body.push(
        button(
            text("Open repository\u{2026}")
                .size(13)
                .color(theme.background)
                .font(ui.config.ui_font),
        )
        .padding(Padding::from([8, 18]))
        .on_press(Message::OpenRepoDialogOpen)
        .style(move |_, _| button::Style {
            background: Some(Background::Color(theme.accent)),
            text_color: theme.background,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 8.0.into(),
            },
            shadow: Default::default(),
            snap: true,
        }),
    );

    // Click-to-reopen recents — reuses the open dialog's row builder so both
    // entry points look identical. `tabs` is empty in this state, so (unlike the
    // dialog) there's nothing already-open to filter out.
    let recents: Vec<&String> = ui.recent_repos.iter().take(6).collect();
    if !recents.is_empty() {
        let mut list = column![
            text("Recent")
                .size(11)
                .color(theme.subtle_text)
                .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        ]
        .spacing(2);
        for root in recents {
            list = list.push(tab_bar::recent_repo_row(ui, theme, root));
        }
        body = body.push(container(list).width(Length::Fixed(320.0)));
    }

    body = body.push(
        text("or press \u{2318}O")
            .size(12)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
    );

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
pub(crate) enum MenuAction {
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
    /// Run a fetch (toolbar fetch menu). Constructed only by the non-macOS iced
    /// overlay menu; macOS dispatches fetch through the native `NSMenu` path
    /// (`start_fetch` directly), so this variant is unused on macOS.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Fetch(FetchTarget),
    /// Replace the revset and re-evaluate (toolbar revset menu). Non-macOS only,
    /// for the same reason as `Fetch`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SetRevset(String),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DetailField {
    Author,
    Committer,
    Description,
}

/// Lower a shared [`menu::MenuEntry`] tree to a native `NSMenu` tree, assigning
/// each actionable leaf the next index into `actions` (what `popup_menu` returns
/// when it's picked). A revset row folds its expression into the label; an empty
/// submenu becomes a disabled row.
#[cfg(target_os = "macos")]
fn lower_menu_to_native(
    entries: &[menu::MenuEntry],
    actions: &mut Vec<MenuAction>,
) -> Vec<macos_native::MenuItem> {
    use macos_native::MenuItem;
    use menu::MenuEntry;

    entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Separator => MenuItem::Separator,
            MenuEntry::Disabled { label } => MenuItem::Entry {
                label: label.clone(),
                id: 0,
                enabled: false,
            },
            MenuEntry::Item {
                label,
                detail,
                action,
                ..
            } => {
                let id = actions.len() as u32;
                actions.push(action.clone());
                let label = match detail {
                    Some(detail) => format!("{label}  ·  {detail}"),
                    None => label.clone(),
                };
                MenuItem::entry(label, id)
            }
            MenuEntry::Submenu { label, items } => {
                MenuItem::submenu(label.clone(), lower_menu_to_native(items, actions))
            }
        })
        .collect()
}

/// Format a `Copy → {Author,Committer,Description}` value from a freshly-read
/// revision. Signatures render as `Name <email>  <timestamp>` (absent parts
/// skipped); the description is the full message. Returns `None` when the field
/// is absent (no committer, empty description), so the caller can fall back.
fn format_detail(details: &RevisionDetails, field: DetailField) -> Option<String> {
    fn signature(sig: &SignatureInfo) -> Option<String> {
        let mut out = sig.name.clone();
        if !sig.email.is_empty() {
            out.push_str(" <");
            out.push_str(&sig.email);
            out.push('>');
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
        ResultRef::Commit(id) => ui.session.commits.find_by_change_id(id.as_str()),
        ResultRef::Bookmark(name) => ui
            .session
            .commits
            .iter()
            .find(|c| c.bookmarks().iter().any(|b| b == name)),
        ResultRef::WorkingCopy => ui.session.commits.working_copy(),
        _ => None,
    }
}

/// Concatenate the currently-selected file's diff into plain text. Used by
/// the palette's "Copy current file diff" command. Mirrors `git diff`'s
/// hunk-then-rows format closely enough that pasted output reads correctly.
fn current_file_diff_text(ui: &Diffui) -> Option<String> {
    let file = ui.session.document.files.get(ui.selected_file)?;
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

/// Streaming load for a GitHub-PR tab: fetch the header metadata and stream
/// the diff concurrently (`gh pr view` + `gh pr diff`), batching completed
/// files so a huge PR paints progressively while it downloads. Mirrors
/// [`stream_jj_initial_load`]'s channel bridge; every message carries
/// `version` so a superseded load's output is dropped by the cursor guard.
fn stream_github_pr_load(
    spec: github::PrSpec,
    progress: LoadProgress,
    version: u64,
) -> Task<Message> {
    /// Flush the pending file batch once it holds this many diff lines (always
    /// flushing on stream end). Small enough that the first screenful paints
    /// quickly; large enough that a million-line PR doesn't flood the update
    /// loop with per-file messages.
    const BATCH_LINE_LIMIT: usize = 4_096;

    Task::stream(iced::stream::channel(
        16,
        async move |mut output: futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

            // Header metadata rides alongside the diff; its `changedFiles`
            // count is what turns the activity's progress determinate.
            let meta_tx = tx.clone();
            let meta_spec = spec.clone();
            let meta_progress = progress.clone();
            let meta_task = tokio::spawn(async move {
                let result = github::fetch_pr_info(&meta_spec)
                    .await
                    .map_err(|error| format!("{error:#}"));
                if let Ok(info) = &result {
                    meta_progress.set_total(info.changed_files);
                }
                let _ = meta_tx.send(Message::PrMetaLoaded(version, Box::new(result)));
            });

            // The commit list fills the sidebar; independent of the diff.
            let commits_tx = tx.clone();
            let commits_spec = spec.clone();
            let commits_task = tokio::spawn(async move {
                let result = github::fetch_pr_commits(&commits_spec)
                    .await
                    .map_err(|error| format!("{error:#}"));
                let _ = commits_tx.send(Message::PrCommitsLoaded(version, Box::new(result)));
            });

            let diff_tx = tx;
            let diff_task = tokio::spawn(async move {
                let mut batch: Vec<DiffFile> = Vec::new();
                let mut batch_lines = 0usize;
                let result = github::stream_pr_diff(&spec, |file| {
                    progress.increment();
                    batch_lines += file
                        .hunks
                        .iter()
                        .map(|hunk| hunk.lines.len())
                        .sum::<usize>();
                    batch.push(file);
                    if batch_lines >= BATCH_LINE_LIMIT {
                        let _ = diff_tx
                            .send(Message::PrFilesBatch(version, std::mem::take(&mut batch)));
                        batch_lines = 0;
                    }
                })
                .await
                .map_err(|error| format!("{error:#}"));
                if !batch.is_empty() {
                    let _ = diff_tx.send(Message::PrFilesBatch(version, batch));
                }
                let _ = diff_tx.send(Message::PrFinished(version, Box::new(result)));
            });

            // Relay worker messages to iced, honoring its backpressure. The
            // loop ends once all workers finished and dropped their senders.
            while let Some(message) = rx.recv().await {
                if output.send(message).await.is_err() {
                    break;
                }
            }
            let _ = tokio::join!(meta_task, commits_task, diff_task);
        },
    ))
}

/// Filesystem-watch subscription. The watcher mechanism — `notify` backend, path
/// classification, debounce — lives in [`diffui_core::watcher`]; here we only map
/// its coalesced batches onto messages, honoring iced's backpressure:
///
/// - a **working-tree** edit → `RefreshRepository` (snapshot + reload @'s diff).
/// - a write under **`.jj/repo/op_heads`** → `OpLogChanged`, which an op-id dedup
///   turns into a reload only when the op was *external*.
// `&PathBuf` (not `&Path`) is required: `Subscription::run_with` keys on
// `D = PathBuf` and hands the builder a `fn(&D)`.
#[allow(clippy::ptr_arg)]
fn watch_repository(root: &PathBuf) -> Pin<Box<dyn Stream<Item = Message> + Send>> {
    let root = root.clone();
    iced::stream::channel(
        8,
        async move |mut output: futures::channel::mpsc::Sender<Message>| {
            let mut watcher = match diffui_core::watcher::RepoWatcher::start(&root) {
                Ok(watcher) => watcher,
                Err(error) => {
                    eprintln!(
                        "diffui: filesystem watcher unavailable for {}, auto-refresh disabled: {error}",
                        root.display()
                    );
                    return;
                }
            };
            // `next_batch` holds the watcher and applies the debounce; the loop
            // ends when the watch is dropped or iced closes the channel.
            while let Some(batch) = watcher.next_batch().await {
                if batch.worktree && output.send(Message::RefreshRepository).await.is_err() {
                    break;
                }
                if batch.op_log && output.send(Message::OpLogChanged).await.is_err() {
                    break;
                }
            }
        },
    )
    .boxed()
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

/// Memory profile for a real jj load: retained store size vs the transient
/// allocation peak. Lives app-side because it reads the app's tracking global
/// allocator (`track_alloc::{CURRENT, PEAK}`); the load itself is core's.
///   DIFFUI_PROFILE_REPO=/path \
///   cargo test --features track-alloc profile_load_memory -- --ignored --nocapture
#[cfg(all(test, feature = "track-alloc"))]
mod mem_profile {
    use crate::track_alloc::{CURRENT, PEAK};
    use diffui_core::LoadProgress;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    #[ignore]
    fn profile_load_memory() {
        let repo = std::env::var("DIFFUI_PROFILE_REPO")
            .unwrap_or_else(|_| format!("{}/code/bun", std::env::var("HOME").expect("HOME set")));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let progress = LoadProgress::default();
        let baseline = CURRENT.load(Relaxed);
        PEAK.store(baseline, Relaxed);

        let (store, graph, _branch_status, _bookmarks) = runtime
            .block_on(diffui_core::jj::load_jj_commits(
                repo.clone().into(),
                "all()".to_owned(),
                progress,
            ))
            .expect("load commits");

        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        let live = CURRENT.load(Relaxed).saturating_sub(baseline);
        let store_heap = store.heap_bytes();
        let n = store.len().max(1);
        let mb = |bytes: usize| bytes as f64 / 1.0e6;

        eprintln!("\n=== diffui memory profile (logical bytes) ===");
        eprintln!("repo            : {repo}");
        eprintln!("commits         : {}", store.len());
        eprintln!("transient peak  : {:>9.1} MB", mb(peak));
        eprintln!(
            "live after load : {:>9.1} MB  (allocator current)",
            mb(live)
        );
        eprintln!("store.heap()    : {:>9.1} MB  (accounted)", mb(store_heap));
        eprintln!(
            "per commit      : store {:.0} B    peak {:.0} B",
            store_heap as f64 / n as f64,
            peak as f64 / n as f64
        );
        eprintln!(
            "peak / live     : {:.2}x  (how much of the high-water mark is transient)",
            peak as f64 / store_heap.max(1) as f64
        );
        eprintln!("=============================================\n");

        drop((store, graph));
    }
}
