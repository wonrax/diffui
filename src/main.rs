use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

mod action;
mod activity;
mod app_menu;
mod bookmarks;
mod changes;
#[cfg(test)]
mod chrome_scenes;
mod cli_output;
mod cmdline;
mod cmdline_view;
mod commands;
mod commit_info;
mod compare;
mod config;
mod conflicts;
mod credentials;
mod diff_panel;
mod empty_states;
mod field;
mod find;
mod graph_preview;
#[cfg(test)]
mod harness;
mod history_panel;
mod jj_version;
mod keymap;
mod launch_env;
mod logging;
// Under test `menus::run_popup` stands in for the tracking loop and no bar goes up, so
// the AppKit side has no callers.
#[cfg(test)]
mod field_scenes;
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
mod macos_native;
mod media;
mod menu;
mod menus;
mod meta_line;
mod mode_hints;
mod modes;
mod overlay_kit;
#[cfg(test)]
mod overlay_scenes;
mod palette;
#[cfg(test)]
mod pane_scenes;
#[cfg(test)]
mod panels_scenes;
mod path_browse;
mod plan;
mod range;
mod revision_rows;
mod revops;
mod revset_assist;
mod root;
#[cfg(test)]
mod scene_suite;
mod shortcuts;
mod sidebar;
mod source_panel;
mod spawn;
mod status_bar;
mod tab_bar;
mod tabs;
mod toolbar;
mod trunk;
mod update;
mod window_state;

// Domain logic now lives in the headless `diffui-core` crate. Re-export the
// modules the app still reaches into by path (`crate::jj`, `crate::mutations`,
// …) so those call sites stay unchanged.
pub(crate) use action::Action;
pub(crate) use diffui_core::{
    FetchTarget, JobId, RepoHandle, github, graph_layout, jj, mutations, repository,
};
// The same for the widget layer in `diffui-widgets`: `crate::theme`,
// `crate::ui`, `crate::diff_view` and the rest resolve as they did when they
// were modules of this crate.
pub(crate) use diffui_widgets::{
    chip, chrome, diff_view, ellipsis, geometry, graph_view, hunk_actions, icons, input, keys,
    measure, prose, resize_handle, revision_list, scrollbar, squircle, theme, typography, ui,
};

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
use commands::{CommandArg, Context};
use config::AppConfig;
use diffui_core::{DiffFile, LoadProgress, RevisionDetails, RevisionSelection, SignatureInfo};
use find::FindState;
use geometry::{Point, Rectangle, Size};
use keymap::{Chord, Keymap};
use modes::{Mode, ModeKind, TabMode};
use palette::{
    ColumnSource, PaletteState, Recents, ResultRef, change_id_for_recents, revision_selection,
};
use repository::{Repository, Vcs, prepare_repository};
use theme::{ResolvedTheme, SystemAppearance, ThemePreference};
use window_state::WindowState;

/// What every piece of update code takes: the root entity's context.
pub(crate) type Cx<'a> = gpui_kit::Context<'a, Diffui>;

/// The window's size when nothing better is known.
const DEFAULT_WINDOW_SIZE: Size = Size::new(1280.0, 800.0);

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

fn main() {
    // Started by git or ssh to ask for a credential: relay it and exit.
    if let Some(code) = diffui_core::askpass::run_helper() {
        std::process::exit(code);
    }
    // `diffui jj …`, or a passthrough child started as `jj`: be the jj CLI.
    if let Some(code) = diffui_core::cli::passthrough::jj_main() {
        std::process::exit(code.into());
    }
    // First, while the process is still single-threaded: it may `set_var`.
    let path_repair = launch_env::repair_path();
    let cli = Cli::parse();
    logging::init();
    if let Some(repair) = path_repair {
        tracing::info!("{repair}");
    }
    if let Err(error) = diffui_core::askpass::start() {
        tracing::warn!("credential prompts unavailable: {error}");
    }

    // The UI thread runs inside the tokio runtime: the repository actor's channels
    // and the core's process spawns look for one where they're called from.
    let _tokio = spawn::runtime().enter();
    gpui_kit::application().run(move |cx| {
        root::init(cx);
        let saved = WindowState::load();
        if let Err(error) = root::open_window(cli, saved, cx) {
            tracing::error!("couldn't open the window: {error}");
            cx.quit();
        }
    });
}

/// The primary display's usable size, where the platform can tell us before
/// the window exists.
fn screen_visible_size() -> Option<(f32, f32)> {
    #[cfg(target_os = "macos")]
    return macos_native::primary_screen_visible_size();
    #[cfg(not(target_os = "macos"))]
    None
}

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Native GUI diff viewer for jj and git")]
struct Cli {
    /// Repository path (or GitHub PR reference) to open as a tab; repeat to
    /// open several. Same as passing it positionally.
    #[arg(short = 'p', long = "path", value_name = "REPO")]
    paths: Vec<PathBuf>,

    /// Repositories (or GitHub PR references) to open, e.g. `diffui .`. With
    /// none, diffui restores your last session and, when started inside a
    /// repository, opens and focuses that one too.
    #[arg(value_name = "REPO")]
    repos: Vec<PathBuf>,
}

impl Cli {
    /// Everything named on the command line, flag or positional, in order.
    fn requested_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.iter().chain(&self.repos)
    }
}

/// A revision mutation on its way to the repository actor. The actor is
/// single-threaded, so mutations serialize there rather than in a queue here;
/// this only carries what the *frontend* has to remember about one — which tab
/// asked, and which activity row is showing it.
#[derive(Debug, Clone)]
pub(crate) struct PendingMutation {
    op: mutations::MutationOp,
    tab_id: TabId,
    activity_id: activity::ActivityId,
    /// Run with the immutable-commit guards off (jj's `--ignore-immutable`).
    /// Only ever set by the confirm dialog's accept — never at op creation —
    /// so a mutation can't skip the guards without the user saying so.
    allow_immutable: bool,
}

#[derive(Debug)]
pub(crate) struct Diffui {
    pub(crate) app_focused: bool,
    pub(crate) selected_theme: ThemePreference,
    pub(crate) system_theme: SystemAppearance,
    /// Last usable widths for the two resizable navigation panes. Collapsing a
    /// pane keeps its width here so reopening restores the user's split.
    pub(crate) sidebar_width: f32,
    pub(crate) file_nav_width: f32,
    /// Whether the diff pane wraps long lines (default) or clips them at the
    /// pane edge. Global across tabs, persisted with the window state.
    pub(crate) diff_wrap: bool,
    /// Two-column (side-by-side) diff layout. Global across tabs, persisted
    /// with the window state.
    pub(crate) diff_split: bool,
    /// The diff header shows only the title and one metadata line instead
    /// of the description body and the field grid. Global, persisted with
    /// the window state.
    pub(crate) commit_details_collapsed: bool,
    /// SVG files show their text diff rather than the rendered preview.
    /// Global across tabs and diff sources.
    pub(crate) svg_source: bool,
    /// Decoded image and SVG previews, by blob id (see [`media`]).
    pub(crate) media: media::MediaCache,
    /// View-time memo for the file list's stat-column widths and flattened file
    /// tree, keyed on document identity. Without it, the sidebar re-shaped ~5·N
    /// strings on every diff-scroll file-boundary crossing
    /// (each crossing publishes `SelectFile`, which forces a full rebuild) and tanked the frame rate on large PRs. Interior-mutable because
    /// `view()` only has `&self`; global rather than per-tab since every cache
    /// key carries `document_id`, which is unique across tabs — a switch is
    /// already a miss.
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
    /// The display `window_position` is relative to, where that's how the platform
    /// reports it.
    pub(crate) window_display: Option<String>,
    /// Set when the window geometry or sidebar width changes; cleared once the
    /// new value is persisted. While it's `Some`, the subscription runs a
    /// debounce timer whose tick flushes the geometry to disk after the
    /// changes settle. `None` when nothing is pending.
    pub(crate) geometry_dirty_since: Option<Instant>,
    pub(crate) config: AppConfig,
    /// Chord → command id per context, built from the registry's defaults
    /// with the `[keys]` section of `config.toml` layered on top.
    pub(crate) keymap: Keymap,
    /// Window-level modes, innermost last. The top of this stack (or, when it
    /// is empty, the top of the active tab's) owns the keyboard — see
    /// [`crate::modes`].
    pub(crate) modes: Vec<Mode>,
    /// In-session recents (revisions + commands) used to score palette
    /// matches. Persisted to the XDG data dir between sessions.
    pub(crate) recents: Recents,
    /// Bumped by [`scroll_sidebar_to_file`] when keyboard file navigation moves
    /// the selection. The sidebar's `RevisionList` reveals the selected file's
    /// row (which the sidebar computes from the current file tree) into view on
    /// the change. Separate from `TabState::revision_reveal_token` so revealing
    /// a file doesn't also re-centre the selected revision. Transient, so it
    /// isn't per-tab.
    pub(crate) sidebar_file_reveal_token: u64,
    /// Bumped whenever a tab is (re)activated. The sidebar/diff widgets watch
    /// it and, on a change, jump to the active tab's saved
    /// `sidebar_scroll_offset` / `diff_scroll_offset` — re-applying them over
    /// whatever the shared widget state leaked from the previous tab.
    pub(crate) scroll_restore_token: u64,
    /// Bumped whenever the shown document is replaced (a diff reload,
    /// working-copy edit, or tab switch). The diff view watches it to drop its
    /// per-line shaped-paragraph cache, whose `(file, hunk, line)` keys would
    /// otherwise render stale text — most visibly between two tabs both on `@`,
    /// which share the constant `"working-copy"` revision key. Global/monotonic,
    /// not per-tab: the widget state it guards is itself shared across tabs, so
    /// a switch bumps it and correctly re-clears the cache the other tab
    /// populated. The source browser bumps it too — the code widget's
    /// shaped-paragraph cache is shared between the two views.
    pub(crate) document_version: u64,

    // ── Multi-repo ──────────────────────────────────────────────────────
    /// Every open repository, in tab order. Each tab owns its whole per-repo
    /// state for as long as it exists (see [`TabState`]). Empty ⇒ the
    /// empty-state view owns the window.
    pub(crate) tabs: Vec<Tab>,
    /// Index into `tabs` of the active tab. Out of range only while `tabs` is
    /// empty, which [`Diffui::active`] resolves to `no_tab`.
    pub(crate) active: usize,
    /// The state reached by [`Diffui::active`] when no tab is open at all (a
    /// launch with nothing to restore, or after the last tab is closed). Its
    /// `Session::empty` is `Loaded`, so `view()` renders the welcome screen
    /// rather than a spinner, and handlers that fire with no tab open write
    /// here harmlessly instead of forcing an `Option` on every call site.
    pub(crate) no_tab: TabState,
    /// Monotonic source of `TabId`s, so a tab keeps a stable identity even as
    /// its index shifts when other tabs open/close.
    pub(crate) next_tab_id: u64,
    /// Monotonic source of document identities (`Session::document_id`),
    /// stamped on every document replacement across every tab. Background
    /// per-file work (syntax highlighting) routes its results by this id.
    pub(crate) next_document_id: u64,
    /// Most-recently-opened repo roots (newest first), surfaced as quick-pick
    /// rows in the open dialog. Seeded from / persisted to `WindowState`.
    pub(crate) recent_repos: Vec<String>,

    // ── Toolbar / activity / revset ─────────────────────────────────────
    /// Monotonic source of `ActivityId`s across every tab.
    pub(crate) next_activity_id: u64,
    /// The caret control the cursor is currently over, if any — drives the
    /// hover highlight that `mouse_area` (unlike `button`) doesn't provide.
    pub(crate) hovered: Option<HoverTarget>,
    /// A hidden panel shown over the diff for a moment (see `root::Panel`).
    pub(crate) panel_peek: Option<root::PanelPeek>,
    /// The tab strip's drag and slide state.
    pub(crate) tab_strip: tab_bar::Strip,
    /// Transient error toasts (failed mutation/fetch/undo), newest last.
    /// The activity log keeps the durable record; these only make a failure
    /// impossible to miss. Auto-pruned after a few seconds, click to dismiss.
    pub(crate) toasts: Vec<Toast>,
    pub(crate) next_toast_id: u64,
    /// Live keyboard modifier state, tracked from the event subscription so
    /// pointer gestures can read it at decision time (⌥-drop = rebase with
    /// descendants).
    pub(crate) modifiers: keys::Modifiers,
    /// The window this view lives in, once it has one. Update code holds no
    /// `Window`; see [`Diffui::with_window`].
    pub(crate) window: Option<gpui_kit::AnyWindowHandle>,
    /// The loops that run while their condition holds.
    pub(crate) background: root::Background,
    /// The palette's matcher, waiting for typing to pause.
    pub(crate) palette_query: spawn::Debounce,
    /// The command line's check, waiting for typing to pause.
    pub(crate) command_line_settle: spawn::Debounce,
    /// The window's regions as cached views, once it has a window.
    pub(crate) panes: Option<root::Panes>,
    /// How many times the root has rendered.
    #[cfg(test)]
    pub(crate) renders: usize,
}

/// Frontend wrapper around the core [`diffui_core::OpDraft`]: the draft
/// itself plus the preview bookkeeping.
#[derive(Debug, Clone)]
pub(crate) struct DraftUi {
    pub(crate) draft: diffui_core::OpDraft,
    /// The live drag's drop spot, mirrored from the sidebar widget. While
    /// `Some`, the widget draws the drop indicators itself and the keyboard
    /// candidate's marker is suppressed.
    pub(crate) hover_spot: Option<revision_list::DropSpot>,
    pub(crate) preview: DraftPreviewState,
    /// Commit ids (hex) of the moved set the last rebase simulation resolved
    /// — the sidebar washes these rows so a branch-mode draft shows exactly
    /// which branch is coming along. Kept until the next simulation lands
    /// (so it doesn't flicker per candidate hop); dropped with the draft.
    pub(crate) moved_highlight: HashSet<String>,
    /// The derived simulation parked while [`TabState::preview_debounce`]
    /// runs, so rapid candidate hops expire here without ever reaching the
    /// backend (each stale hop would otherwise run a full simulation and
    /// write throwaway objects).
    pub(crate) preview_request: Option<Box<PreviewRequest>>,
    /// The graph the last simulation would leave, laid over the loaded rows.
    /// Kept until the next one lands, like `moved_highlight`, so j/k sweeps
    /// don't flash the plain graph between candidates.
    pub(crate) graph: Option<graph_preview::Laid>,
}

/// A ready-to-run draft simulation: everything `kick_draft_preview` resolved
/// at kick time, frozen so the post-debounce run simulates exactly what the
/// user saw when the timer started.
#[derive(Debug, Clone)]
pub(crate) struct PreviewRequest {
    pub(crate) kind: diffui_core::DraftKind,
    pub(crate) sources: Vec<diffui_core::RevisionSelection>,
    pub(crate) destination: diffui_core::Destination,
    /// A squash draft's picked changes ("Move selected into…").
    pub(crate) selection: Option<Box<diffui_core::DiffSelection>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum DraftPreviewState {
    /// No candidate armed yet (or the draft kind has no preview).
    #[default]
    Idle,
    /// A debounced simulation is in flight.
    Loading,
    Ready(diffui_core::DraftSimulation),
    Failed(String),
}

impl DraftUi {
    pub(crate) fn new(draft: diffui_core::OpDraft) -> Self {
        Self {
            draft,
            hover_spot: None,
            preview: DraftPreviewState::Idle,
            moved_highlight: HashSet::new(),
            preview_request: None,
            graph: None,
        }
    }
}

/// One floating error card — see [`Diffui::toasts`].
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) id: u64,
    /// What failed, e.g. "Rebase failed".
    pub(crate) title: prose::Prose,
    /// First line of the error, truncated; the full text lives in the
    /// activity row. Names jj quotes in backticks render as code.
    pub(crate) detail: prose::Prose,
    pub(crate) born: Instant,
    /// Stays up until dismissed instead of expiring after [`TOAST_TTL`] — for
    /// startup problems the user may not be looking at the window for.
    pub(crate) sticky: bool,
}

/// How long a toast lingers before the tick prunes it.
pub(crate) const TOAST_TTL: Duration = Duration::from_secs(7);

#[derive(Debug, Clone)]
pub(crate) struct DescriptionEditor {
    pub(crate) target: RevisionSelection,
    pub(crate) original: String,
    pub(crate) content: String,
    /// The mutation activity that owns an in-flight save. `None` means the
    /// draft is editable.
    pub(crate) saving_activity: Option<activity::ActivityId>,
    pub(crate) switch_blocked: bool,
    /// Commit mode ("Commit…"): saving runs `jj commit` — describe `@` and
    /// start a new change on top — instead of a plain describe.
    pub(crate) commit: bool,
    /// The change `@` was on when the editor opened. A save that addresses
    /// `@` is refused once `@` has moved to another change, so the text never
    /// lands on a change the user wasn't describing.
    pub(crate) wc_change: Option<String>,
}

impl DescriptionEditor {
    pub(crate) fn text(&self) -> String {
        self.content.clone()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.text().trim_end() != self.original.trim_end()
    }
}

/// A modal confirmation gating a mutation the jj CLI would refuse outright:
/// a backwards/sideways bookmark move (the CLI demands `--allow-backwards`),
/// or an op touching immutable revisions (the CLI demands
/// `--ignore-immutable`; the held pending has `allow_immutable` armed).
/// Accept runs the held mutation; cancel resolves its queued activity. The
/// `PendingMutation` is fully self-addressed (repo/tab/activity), so
/// accepting still runs correctly after a tab switch.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmDialog {
    pub(crate) title: prose::Prose,
    pub(crate) body: prose::Prose,
    pub(crate) confirm_label: String,
    /// What the dialog shows beyond its words, and how its confirm looks.
    pub(crate) look: ConfirmLook,
    pub(crate) pending: PendingMutation,
}

/// The parts of a [`ConfirmDialog`] only some confirms have. The default is
/// the plain guard: words and a destructive confirm.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfirmLook {
    /// The confirm goes ahead with something ordinary (a push the user
    /// started), rather than destroying or overriding something: the
    /// primary button, not the red one.
    pub(crate) routine: bool,
    /// The bookmarks a push moves, a row each.
    pub(crate) moves: Vec<diffui_core::BookmarkMove>,
    /// Things to know before confirming that don't stop it, as warning
    /// callouts: what's wrong, then what it means.
    pub(crate) callouts: Vec<(prose::Prose, Option<prose::Prose>)>,
}

/// Which main view the active tab shows: the diff (default) or the source
/// browser. Per-tab, toggled by the toolbar switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MainView {
    #[default]
    Diff,
    Source,
    /// The operation log or a revision's evolog (jj only).
    History,
}

/// One loaded file in the source browser: the synthesized all-context
/// document plus render bookkeeping.
#[derive(Debug, Clone)]
pub(crate) struct SourceFileView {
    pub(crate) file: diffui_core::DiffFile,
    pub(crate) line_count: usize,
    pub(crate) byte_len: usize,
    pub(crate) binary: bool,
    pub(crate) too_large: bool,
    /// Layout identity for the diff-view height index, allocated from the
    /// app's global document-id counter on every load.
    pub(crate) doc_id: u64,
}

/// Per-tab source-browser state. Lives in [`TabState`] alongside the diff
/// view's state (the browser is a second lens over the same repo).
/// `revision == None` means the browser was never opened in this tab.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceState {
    /// The revision being browsed. Independent of the diff view's selection —
    /// jumping into the browser pins it until the next explicit browse.
    pub(crate) revision: Option<RevisionSelection>,
    /// The listed tree, or `None` while it loads.
    pub(crate) tree: Option<Vec<diffui_core::SourceEntry>>,
    pub(crate) tree_error: Option<String>,
    /// Lazily-listed contents of ignored directories, keyed by dir path.
    /// Kept beside `tree` (not merged in) so watcher re-lists of the base
    /// tree don't wipe an exploration of `target/`; the view composes the
    /// two (see [`source_panel::SourceTreeCache`]).
    pub(crate) dir_children: HashMap<String, Vec<diffui_core::SourceEntry>>,
    /// Bumped whenever `tree` or `dir_children` change — the composed-entry
    /// cache key (`version` alone misses lazy-dir merges).
    pub(crate) tree_epoch: u64,
    /// Expanded directories, by full path prefix. Directories are collapsed
    /// by default, so this is the inverse of the diff tree's collapse set.
    pub(crate) expanded: HashSet<String>,
    /// Fuzzy file-search query typed into the sidebar's filter box. While
    /// non-empty, the tree renders as a flat ranked match list instead.
    pub(crate) filter: String,
    /// Selected file path (the one shown / being loaded).
    pub(crate) selected: Option<String>,
    pub(crate) file: Option<SourceFileView>,
    /// Path whose content load is in flight, if any.
    pub(crate) loading: Option<String>,
    pub(crate) file_error: Option<String>,
    /// Guard for async results: bumped on every (re)browse, carried by the
    /// tree/file load messages so a superseded browse's results are dropped.
    pub(crate) version: u64,
    /// Saved scroll offsets, mirrored like the diff view's (the widgets'
    /// state is shared across views/tabs).
    pub(crate) scroll_offset: f32,
    pub(crate) tree_scroll_offset: f64,
    /// Bumped to scroll the sidebar tree to the selected file's row (jumps
    /// from the diff view / palette).
    pub(crate) reveal_token: u64,
    /// A jump landed before the tree listing did — re-arm the reveal when
    /// the listing arrives (one-shot, so watcher re-lists don't yank the
    /// scroll back to the selection).
    pub(crate) reveal_pending: bool,
}

/// Which toolbar dropdown is open. Off macOS both render as overlays anchored
/// near their trigger.
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
    RevsetCaret,
    /// A title-bar tab, keyed by id — drives its inset hover fill. At most one is
    /// hovered at a time.
    Tab(TabId),
}

/// Everything one tab owns: its core [`Session`] (all domain + orchestration
/// state) plus the frontend-only view state that rides alongside it. A tab
/// holds this for its whole life, on screen or not, so "which tab does this
/// belong to" is answered by resolving a [`TabId`] (or a stream version) —
/// never by writing to whichever tab happens to be active when a result lands.
#[derive(Debug)]
pub(crate) struct TabState {
    pub(crate) session: Session,
    /// The repository this tab views, or `None` for a GitHub-PR tab. Held by
    /// the frontend (not the session) because it is what names the tab, keys
    /// persistence, and builds the subscription that owns the actor.
    pub(crate) repository: Option<Repository>,
    /// The command sender for this tab's repository, handed over by the
    /// actor's first event. `None` for the brief window before that lands, and
    /// for the no-tab empty state.
    pub(crate) handle: Option<RepoHandle>,
    /// Jobs this tab issued whose results it routes itself, because the
    /// projection has no state to fold them into.
    pub(crate) jobs: JobTable,
    /// Mutations waiting for [`JobTable::mutation`] to free up. The actor
    /// serializes the work; this keeps the ones behind it addressable, so each
    /// completion still finds the activity row that started it.
    pub(crate) queued_mutations: VecDeque<PendingMutation>,
    /// This repo's default revset (its `revsets.log`, or jj's default) — what
    /// the "Default" preset in the revset menu applies. Read from jj's config
    /// on disk once, when the tab is created, so neither a render nor a tab
    /// switch does config-file I/O on the UI thread.
    pub(crate) default_revset: String,
    /// Per-tab pane visibility so a focused review layout survives tab hops.
    pub(crate) history_panel_collapsed: bool,
    pub(crate) files_panel_collapsed: bool,
    /// Collapsed directories of the sidebar file tree, by full path prefix.
    pub(crate) collapsed_dirs: HashSet<String>,
    /// Files folded to their headers in the diff, by path, and the diff they
    /// were folded in (see `diff_panel::folded_files`).
    pub(crate) folded_files: (String, HashSet<String>),
    pub(crate) selected_file: usize,
    /// Bumped whenever the user picks a revision through a path that doesn't go
    /// through the revision list (currently: the palette). The sidebar's
    /// `RevisionList` watches this and scrolls the matching row into view on
    /// disagreement.
    pub(crate) revision_reveal_token: u64,
    /// True while a palette-initiated revision load is in flight. We can't bump
    /// `revision_reveal_token` at the moment the user accepts the result — at
    /// that point `selected_revision` is still the old value, so the sidebar
    /// would scroll the wrong row into view. The diff's completion reads this
    /// flag once the new revision has actually been written into
    /// `selected_revision`, *then* bumps the token.
    pub(crate) pending_revision_reveal: bool,
    /// Last-known scroll offsets of the sidebar (content-space px) and diff
    /// view, kept current by the widgets' `on_scroll` callbacks. The widgets own
    /// their live offset in tree `State`, but that state is shared across tabs;
    /// mirroring it per tab lets an activation push this tab's position back in
    /// via `scroll_restore_token`.
    pub(crate) sidebar_scroll_offset: f64,
    pub(crate) file_tree_scroll_offset: f64,
    pub(crate) diff_scroll_offset: f32,
    /// This tab's activity log (long-running ops: load, refresh, revset eval,
    /// fetch, undo, push).
    pub(crate) activities: activity::ActivityLog,
    /// The activity wrapping the in-flight graph (re)load, finished when the
    /// terminal load message arrives.
    pub(crate) pending_load_activity: Option<activity::ActivityId>,
    /// Which main view this tab shows (diff / source browser).
    pub(crate) main_view: MainView,
    pub(crate) source: SourceState,
    /// View-time memo of the flattened source tree, keyed on browse version +
    /// tree epoch + collapse set (mirrors `sidebar_file_cache`'s rationale).
    /// Per-tab because those keys are per-tab counters that all start at zero:
    /// one shared cache let a tab render — and click through to — another tab's
    /// tree.
    pub(crate) source_tree_cache: std::cell::RefCell<source_panel::SourceTreeCache>,
    /// This tab's mode stack (find bar, description editor, target-mode
    /// draft), innermost last. Per-tab because each names rows, files or text
    /// belonging to *this* document; the keyboard only ever reads the active
    /// tab's, so a switch cannot leave an off-screen mode holding the keys.
    pub(crate) modes: Vec<TabMode>,
    /// A context-menu edit requested for a row that is still loading into the
    /// detail pane. The editor opens when that revision's diff lands. Per-tab:
    /// `WorkingCopy` matches every tab, so a global one opened the editor on
    /// whichever tab produced the next `@` diff.
    pub(crate) pending_description_edit: Option<RevisionSelection>,
    /// Commit ids marked via ⌘-click / ⇧-click for a batch action (the context
    /// menu's "Abandon N revisions"), in mark order. A separate axis from
    /// `session.selected_revision` — marking never changes which diff is shown.
    /// Cleared on plain click, Esc, and draft start; ids whose rows left the
    /// loaded graph are ignored at use rather than eagerly pruned.
    pub(crate) revision_multi_selection: Vec<String>,
    /// The pending description edit is a "Commit…" — the editor opens in
    /// commit mode.
    pub(crate) pending_commit: bool,
    /// The diff pane's file/hunk pick and conflict-resolution state.
    pub(crate) changes: changes::ChangesState,
    /// The history views: op log, evolog, file filter, annotate.
    pub(crate) history: history_panel::HistoryState,
    /// The post-op graph the sidebar draws in place of the loaded one.
    pub(crate) graph_preview: graph_preview::GraphPreviewState,
    /// The command line's per-repository state: its grammar, CLI runs and
    /// the revset field's completion.
    pub(crate) cmdline: cmdline::TabCommandLine,
    /// The find bar's matcher, waiting for typing to pause.
    pub(crate) find_debounce: spawn::Debounce,
    /// The draft's simulation, waiting for candidate hops to pause.
    pub(crate) preview_debounce: spawn::Debounce,
}

impl TabState {
    /// A never-loaded tab for `repository`: a fresh `Session::unloaded` plus
    /// default UI state. `ensure_active_loaded` kicks the real load when this
    /// becomes the active tab (`status != Loaded`). `revset` is the persisted
    /// filter for this repo, or `None` to start on its default.
    fn unloaded(repository: Option<Repository>, revset: Option<String>) -> Self {
        let default_revset = repository.as_ref().map(default_revset).unwrap_or_default();
        let revset = revset.unwrap_or_else(|| default_revset.clone());
        Self {
            session: Session::unloaded(revset),
            repository,
            handle: None,
            jobs: JobTable::default(),
            queued_mutations: VecDeque::new(),
            default_revset,
            modes: Vec::new(),
            history_panel_collapsed: false,
            files_panel_collapsed: false,
            collapsed_dirs: HashSet::new(),
            folded_files: (String::new(), HashSet::new()),
            selected_file: 0,
            revision_reveal_token: 0,
            pending_revision_reveal: false,
            sidebar_scroll_offset: 0.0,
            file_tree_scroll_offset: 0.0,
            diff_scroll_offset: 0.0,
            activities: activity::ActivityLog::default(),
            pending_load_activity: None,
            main_view: MainView::default(),
            source: SourceState::default(),
            source_tree_cache: Default::default(),
            pending_description_edit: None,
            revision_multi_selection: Vec::new(),
            pending_commit: false,
            changes: Default::default(),
            history: Default::default(),
            graph_preview: Default::default(),
            cmdline: Default::default(),
            find_debounce: Default::default(),
            preview_debounce: Default::default(),
        }
    }

    /// A never-loaded GitHub-PR tab: a `Session` whose source is the PR. No
    /// local repository, so the watcher/snapshot/mutation machinery stays off
    /// and `ensure_active_loaded` routes to the streaming PR load.
    fn unloaded_pr() -> Self {
        Self::unloaded(None, Some(String::new()))
    }

    /// The state shown when no repository is open at all. `Session::empty` is
    /// `Loaded` so `view()` shows the empty state rather than a loading
    /// indicator.
    pub(crate) fn empty() -> Self {
        Self {
            session: Session::empty(),
            ..Self::unloaded(None, Some(String::new()))
        }
    }

    /// Finish the activity wrapping this tab's in-flight graph (re)load, if one
    /// is tracked. Called from the terminal load handlers, which have already
    /// resolved the owning tab.
    pub(crate) fn finish_load_activity(
        &mut self,
        status: activity::ActivityStatus,
        result: Option<String>,
    ) {
        if let Some(id) = self.pending_load_activity.take() {
            self.activities.finish(id, status, result);
        }
    }

    /// The files the find bar searches: the diff document, or the source
    /// browser's loaded file when that view is active.
    pub(crate) fn find_files(&self) -> &[DiffFile] {
        match self.main_view {
            MainView::Diff => &self.session.document.files,
            MainView::Source => self
                .source
                .file
                .as_ref()
                .map(|view| std::slice::from_ref(&view.file))
                .unwrap_or(&[]),
            MainView::History => &[],
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

/// Revset presets for the caret menu, as `(label, expression)`. jj uses
/// revset functions and can be configured with `[[revset_presets]]` (see
/// [`config`]); git uses `git log` revision-range shortcuts. Shared by the
/// native macOS menu and the overlay fallback so they stay in sync.
pub(crate) fn revset_presets(vcs: Option<Vcs>) -> Vec<(String, String)> {
    let builtin: &[(&str, &str)] = match vcs {
        Some(Vcs::Git) => &[("All branches", "--all"), ("Current", "HEAD")],
        _ => {
            if let Some(configured) = config::revset_presets() {
                return configured
                    .iter()
                    .map(|preset| (preset.name.clone(), preset.revset.clone()))
                    .collect();
            }
            &[
                ("Everything", "all()"),
                ("Mine", "mine()"),
                ("Current line", "ancestors(@)"),
                ("Conflicts", "conflicts()"),
            ]
        }
    };
    builtin
        .iter()
        .map(|(label, expr)| ((*label).to_owned(), (*expr).to_owned()))
        .collect()
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

/// One open tab: its identity + display metadata, plus the per-tab state it
/// owns outright — active or not.
#[derive(Debug)]
pub(crate) struct Tab {
    pub(crate) id: TabId,
    /// Dimmed prefix in the tab label — the repo root's parent directory, or
    /// the PR's GitHub org.
    pub(crate) owner: String,
    /// Emphasized name — the repo directory, or `repo#123` for a PR.
    pub(crate) name: String,
    pub(crate) source: TabSource,
    pub(crate) state: TabState,
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

    /// How this tab addresses its repository actor — the subscription key, and
    /// what the actor's events are attributed to.
    pub(crate) fn open_spec(&self) -> diffui_core::OpenSpec {
        match &self.source {
            TabSource::Repo { root, .. } => diffui_core::OpenSpec::Local {
                root: root.clone(),
                scope: self
                    .state
                    .repository
                    .as_ref()
                    .map(|repository| repository.scope.clone())
                    .unwrap_or_default(),
            },
            TabSource::GitHubPr(spec) => diffui_core::OpenSpec::GitHubPr(spec.clone()),
        }
    }

    pub(crate) fn repo_id(&self) -> Option<diffui_core::RepoId> {
        Some(diffui_core::repo::repo_id(&self.open_spec()))
    }
}

/// What a tab waits on a job for, holding what it needs to finish it. The
/// projection owns the graph/diff/snapshot/mutation slots; these are the jobs
/// with nowhere in the session to fold into.
#[derive(Debug, Clone)]
pub(crate) enum Pending {
    /// The draft simulation.
    Preview,
    /// The forecast a rebase onto trunk waits on, holding that rebase.
    TrunkRebase(mutations::MutationOp),
    /// The source browser's tree listing, of this revision.
    Tree(RevisionSelection),
    /// The source browser's file read, of this path.
    File(String),
    /// A fetch, with the activity row showing it and the target it names.
    Fetch(activity::ActivityId, FetchTarget),
    /// The mutation the actor is running, with everything the frontend must
    /// remember to finish (or retry) it.
    Mutation(PendingMutation),
    /// A backwards-bookmark-move check, holding the mutation it gates.
    BookmarkCheck(PendingMutation),
    /// A file's full sources for background syntax highlighting: the
    /// document id and the file's index in it.
    FilePair(u64, usize),
    /// One of the command line's revsets.
    CommandRevset(String),
    /// The revset field's check of this revset.
    RevsetField(String),
    /// A passthrough jj CLI run, with its activity row. Its output streams,
    /// so each batch puts it back.
    Cli(activity::ActivityId),
    /// A `Copy → Author / Committer / Description` waiting on the revision
    /// header, with the in-memory value to copy if that never comes.
    DetailCopy(DetailField, String),
}

impl Pending {
    /// Whether the tab waits on one job of this kind at a time, so a new one
    /// supersedes the last.
    fn is_slot(&self) -> bool {
        !matches!(
            self,
            Self::FilePair(..) | Self::CommandRevset(_) | Self::Cli(_)
        )
    }
}

/// The jobs a tab issued whose results it routes itself, by job.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobTable(HashMap<JobId, Pending>);

impl JobTable {
    /// Wait on `job` for `pending`. Returns the job this one supersedes: the
    /// one of the same kind, for a kind held one at a time. Its result is
    /// dropped when it lands, and the caller may cancel it.
    pub(crate) fn issue(&mut self, job: JobId, pending: Pending) -> Option<JobId> {
        let superseded = pending
            .is_slot()
            .then(|| self.find(|held| same_kind(held, &pending)))
            .flatten();
        if let Some(old) = superseded {
            self.0.remove(&old);
        }
        self.0.insert(job, pending);
        superseded
    }

    /// Stop waiting on `job`, handing back what it was for.
    pub(crate) fn take(&mut self, job: JobId) -> Option<Pending> {
        self.0.remove(&job)
    }

    pub(crate) fn owns(&self, job: JobId) -> bool {
        self.0.contains_key(&job)
    }

    /// A job held for something `wanted` picks.
    pub(crate) fn find(&self, wanted: impl Fn(&Pending) -> bool) -> Option<JobId> {
        self.0
            .iter()
            .find_map(|(job, pending)| wanted(pending).then_some(*job))
    }

    /// Stop waiting on whichever job `wanted` picks, handing it back.
    pub(crate) fn take_where(
        &mut self,
        wanted: impl Fn(&Pending) -> bool,
    ) -> Option<(JobId, Pending)> {
        let job = self.find(wanted)?;
        self.take(job).map(|pending| (job, pending))
    }

    /// The mutation the actor is running: one at a time per tab.
    pub(crate) fn mutation(&self) -> Option<JobId> {
        self.find(|pending| matches!(pending, Pending::Mutation(_)))
    }
}

fn same_kind(a: &Pending, b: &Pending) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Transient state of the open-repository path dialog.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenRepoDialog {
    pub(crate) path: String,
    /// Populated when the last submit failed to resolve a repository, so the
    /// dialog stays open with the reason shown.
    pub(crate) error: Option<String>,
    /// The subdirectories of the folder the path is in, once read. May lag
    /// the path for a beat after it moves to another folder.
    pub(crate) listing: Option<path_browse::Listing>,
    /// The last path typed inside `listing`'s folder, so its rows stay up
    /// while another folder is read instead of the list flashing empty.
    pub(crate) listed_query: Option<path_browse::Query>,
    /// The folder a read is in flight for, so typing within it doesn't
    /// queue another.
    pub(crate) listing_pending: Option<PathBuf>,
    /// The highlighted suggestion (or recent, while the recents show);
    /// `None` until the arrows pick one, so a plain Enter still opens what
    /// was typed.
    pub(crate) selected: Option<usize>,
    /// What the typed path was found to be, for the status line. May lag the
    /// path for a beat; see [`path_browse::Probe`].
    pub(crate) probe: Option<path_browse::Probe>,
}

impl OpenRepoDialog {
    /// The folders offered under the path, or `None` when the path doesn't
    /// browse (empty, a PR reference, relative) or no folder has been read
    /// yet, and the recents show instead.
    pub(crate) fn suggestions(&self) -> Option<(path_browse::Query, Vec<&path_browse::Entry>)> {
        let query = path_browse::Query::parse(&self.path)?;
        let listing = self.listing.as_ref()?;
        if listing.dir == query.dir {
            let matches = query.matches(listing);
            return Some((query, matches));
        }
        let listed = self.listed_query.as_ref()?;
        Some((listed.clone(), listed.matches(listing)))
    }
}

// Domain + orchestration types now live in `diffui-core`'s orchestration
// engine — the streaming cold-load state (`ColdCursor`) + fold, the serial
// mutation queue, refresh coalescing, `RefreshOrigin`, `LoadStatus`, and the
// whole per-repo `Session`. Re-export so the app refers to them unqualified.
pub(crate) use diffui_core::session::RefreshOrigin;
pub(crate) use diffui_core::{LoadStatus, Session};

/// Reveal the keyboard-selected file's row in the sidebar tree. Bumps the
/// file-reveal token so the sidebar's `RevisionList` scrolls the file's row
/// (which the sidebar resolves from the current file tree) into view on the
/// next render. The scroll itself is a no-op when the file list is closed — the
/// sidebar passes `None` as the reveal target in that case.
fn scroll_sidebar_to_file(ui: &mut Diffui) {
    ui.sidebar_file_reveal_token = ui.sidebar_file_reveal_token.wrapping_add(1);
}

/// Map a sidebar row key back to the app's revision selection enum.
fn selection_from_key(key: &revision_list::RowSelectionKey) -> RevisionSelection {
    match key {
        revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
        revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id.clone()),
    }
}

/// Split a repository root into a dimmed `owner` (the parent directory name)
/// and an emphasized `name` (the directory name) for the tab label, e.g.
/// `/Users/me/code/diffui` → (`code`, `diffui`). The owner is empty when the
/// root has no usable parent.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DetailField {
    Author,
    Committer,
    Description,
}

/// Lower a shared [`menu::MenuEntry`] tree to a native `NSMenu` tree, assigning
/// each actionable leaf the next index into `picks` (what `popup_menu` returns
/// when it's picked). A row's detail — a revset expression, a push remote, the
/// command's chord — folds into the label; an empty submenu becomes a disabled
/// row.
#[cfg(target_os = "macos")]
fn lower_menu_to_native(
    entries: &[menu::MenuEntry],
    picks: &mut Vec<(commands::CommandId, CommandArg)>,
) -> Vec<macos_native::MenuItem> {
    use macos_native::MenuItem;
    use menu::MenuEntry;

    entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Separator => MenuItem::Separator,
            MenuEntry::Disabled { label, reason } => MenuItem::Entry {
                label: label.clone(),
                id: 0,
                enabled: false,
                subtitle: reason.clone(),
            },
            MenuEntry::Item {
                label,
                detail,
                command,
                arg,
                ..
            } => {
                let id = picks.len() as u32;
                picks.push((*command, arg.clone()));
                let label = match detail {
                    Some(detail) => format!("{label}  ·  {detail}"),
                    None => label.clone(),
                };
                MenuItem::entry(label, id)
            }
            MenuEntry::Submenu { label, items } => {
                MenuItem::submenu(label.clone(), lower_menu_to_native(items, picks))
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
pub(crate) fn expand_user_path(input: &str) -> PathBuf {
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
    // Not `cmd /C start`: the URL comes from a remote's output (a forge's
    // "create a pull request" line), and handing it to a shell would let `&`
    // or `|` in it run a command. `rundll32` takes the URL as an argument and
    // hands it to the registered protocol handler, no shell involved.
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
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

/// Put `text` on the system clipboard.
pub(crate) fn write_clipboard(text: String, cx: &mut gpui_kit::App) {
    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
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

/// Concatenate the currently-selected file's diff into plain text. Used by
/// the palette's "Copy current file diff" command. Mirrors `git diff`'s
/// hunk-then-rows format closely enough that pasted output reads correctly.
fn current_file_diff_text(ui: &Diffui) -> Option<String> {
    let tab = ui.active();
    let file = tab.session.document.files.get(tab.selected_file)?;
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

        let mut store = diffui_core::CommitStore::default();
        let mut graph = diffui_core::graph_layout::GraphLayout::default();
        let mut cursor = diffui_core::ColdCursor::default();
        {
            let mut emit = |rows: Vec<diffui_core::StreamRow>| {
                diffui_core::fold_cold_batch(&mut store, &mut graph, &mut cursor, rows, false);
            };
            runtime
                .block_on(diffui_core::jj::walk_jj_commits(
                    repo.clone().into(),
                    "all()".to_owned(),
                    progress,
                    4096,
                    &mut emit,
                ))
                .expect("walk commits");
        }

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
