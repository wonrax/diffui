//! Virtualized list rendering revisions with their graph gutter, and the file
//! trees that share its row language.
//!
//! Three reasons it's a custom element in its own entity rather than a column
//! of divs:
//!   - Only on-screen rows pay layout and paint cost. A row's view is built on
//!     demand from the caller's closures, so a million-commit graph holds no
//!     per-row state here.
//!   - Revisions and the file rows under the selected revision live in the
//!     same list, so the graph gutter can run continuously through them.
//!   - Scrolling and hovering repaint this list without going through the
//!     app's reconcile.
//!
//! The entity owns its scroll offset and pointer state. The parent builds a
//! [`RevisionListProps`] on every render and hands it to [`view`]. Visual
//! styling (id chip colours, description colour, file status background) is
//! computed by the caller and passed in per row, so this module stays decoupled
//! from the rest of the app's theming logic.

use std::rc::Rc;
use std::time::Instant;

use diffui_core::graph::LaneFrame;
use diffui_core::graph_layout::GraphEdgeType;
use gpui_kit::{
    AnyElement, App, AvailableSpace, BorderStyle, Bounds, Context, CursorStyle, DispatchPhase,
    Element, ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, ScrollEvent, Style, Subscription, TextAlign, Window, div, fill,
    linear_color_stop, linear_gradient, point, prelude::*, px, quad, relative, size,
};

use crate::chip::{self, Chip};
use crate::ellipsis;
use crate::geometry::{Point, Rectangle};
use crate::graph_view::{self, GraphFrame, LANE_WIDTH, RevisionGraphStyle, lane_strip_width};
use crate::icons;
use crate::measure::LINE_HEIGHT_MULTIPLIER;
use crate::scroll_resample::{self, Resampler};
use crate::scrollbar;
use crate::theme::{Color, Font, chip_background};
use crate::typography::{self, TextBox};
use crate::width_memo::WidthMemo;
use gpui_kit::component::scroll::ScrollableElement as _;

pub const REVISION_ROW_HEIGHT: f32 = 46.0;
pub const FILE_ROW_HEIGHT: f32 = 26.0;
/// Selection/hover chrome is an inset rounded card, not an edge-to-edge
/// band: rows separate by whitespace, and the card gives the active row a
/// modern object-like read. Y-inset keeps consecutive cards from fusing
/// into a column; the X-inset equals the resulting card-to-card gap
/// (2 × Y) so the margin against the pane edges reads the same as the
/// spacing between rows.
pub const ROW_CARD_INSET_X: f32 = 2.0 * ROW_CARD_INSET_Y;
pub const ROW_CARD_INSET_Y: f32 = 1.0;
/// Corner radius of a row's card — and of the highlight a context menu puts
/// over it, which must sit exactly on the card.
pub const ROW_CARD_RADIUS: f32 = crate::theme::radius::PUSH;
/// Horizontal indent per tree depth level for file/directory rows.
pub const FILE_TREE_INDENT: f32 = 14.0;
/// Width of the collapse-chevron column in file/directory rows (empty for
/// files). Indent guides center under it.
const FILE_CHEVRON_COL: f32 = 14.0;
/// Width of the file/folder icon column that follows the chevron column.
const FILE_ICON_COL: f32 = 18.0;
/// Glyph size of the file/folder icon.
const FILE_ICON_SIZE: f32 = 12.0;
const FILE_NAME_TEXT_SIZE: f32 = crate::theme::text_size::BODY;

pub const GUTTER_LEFT_PADDING: f32 = 8.0;
pub const GUTTER_PADDING: f32 = 8.0;
/// Right inset of row content: the scrollbar's gutter, so text stays clear
/// of the bar the row cards run under.
pub const CONTENT_PADDING: f32 = crate::scrollbar::GUTTER;
pub const SMALL_TEXT_SIZE: f32 = crate::theme::text_size::BODY_LG;
pub const CAPTION_TEXT_SIZE: f32 = crate::theme::text_size::CAPTION;
/// Gap between a revision row's description and its id line.
pub const ROW_LINE_GAP: f32 = 4.0;
/// Text size for the collapse/expand chevron on the selected revision row. The
/// Lucide glyph fills its box, so a near-text size reads clearly as a chevron
/// without dominating the row.
const CHEVRON_TEXT_SIZE: f32 = 15.0;

pub const FILE_ROW_GAP: f32 = 6.0;
pub const FILE_ROW_RIGHT_PAD: f32 = crate::scrollbar::GUTTER;
const TOOLTIP_RADIUS: f32 = crate::theme::radius::SURFACE;
// Wider horizontally than vertically: the SURFACE radius eats into the
// card's left/right ends, so the text needs extra x inset to read centered.
const TOOLTIP_PADDING_X: f32 = 9.0;
const TOOLTIP_PADDING_Y: f32 = 6.0;
const TOOLTIP_GAP: f32 = 8.0;
/// How much of a ghost row's text shows through (see
/// [`RevisionRowView::ghost`]).
const GHOST_TEXT_ALPHA: f32 = 0.4;
/// How far a press must travel before it becomes a drag.
const DRAG_THRESHOLD: f32 = 6.0;
/// How close to a boundary between two revision rows a drag reads as the gap
/// between them.
const GAP_ZONE: f64 = 7.0;

#[derive(Debug, Clone, Copy)]
pub struct RevisionListStyle {
    pub graph: RevisionGraphStyle,
    pub background: Color,
    pub selected_background: Color,
    /// Saturated accent — drives the left-edge tint of the working-copy
    /// row's gradient and recolors the working-copy node disc in the
    /// graph.
    pub accent: Color,
    /// Fill of a checked change-selection box (see
    /// [`crate::diff_view::ChangeMarks::control`]).
    pub control: Color,
    pub border: Color,
    pub muted_text: Color,
    pub subtle_text: Color,
    pub accent_text: Color,
    pub primary_font: Font,
    pub mono_font: Font,
    pub file_badge_width: f32,
    pub tooltip_background: Color,
    pub tooltip_text: Color,
    pub tooltip_border: Color,
}

/// Key the widget reports back so the parent can map it to its own
/// selection enum without leaking app types into this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSelectionKey {
    WorkingCopy,
    Commit(String),
}

/// Where a revision drag would land, in commit-store indices. Reported to
/// the app on hover changes and on drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropSpot {
    /// Onto this revision (drop lands as its child).
    OnRow(usize),
    /// Into the gap between two adjacent revision rows: `above` is the newer
    /// (child-side) row, `below` the older (parent-side) one — the moved
    /// commits slot exactly between them.
    Gap { above: usize, below: usize },
}

/// Target-mode decoration for a revision row, computed by the caller from
/// the active draft (keyboard candidate + placement). The widget draws it;
/// live *drag* indicators are widget-internal instead (they follow the
/// cursor, not app state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftMarker {
    /// The destination ring: the moved commits become children of this row
    /// (rebase onto / squash into).
    Ring,
    /// Insert-after: the moved commits land just above this row.
    LineAbove,
    /// Insert-before: the moved commits land just below this row.
    LineBelow,
}

#[derive(Debug, Clone)]
pub enum Item {
    Revision(RevisionRowView),
    File(FileRowView),
}

#[derive(Debug, Clone)]
pub struct RevisionRowView {
    pub selection_key: RowSelectionKey,
    pub change_id_prefix: String,
    pub change_id_suffix: String,
    pub commit_id_short: String,
    pub author: String,
    pub description: String,
    pub description_color: Color,
    /// Bookmark chips, in display order. May be partially hidden behind
    /// a synthetic `+N` overflow chip when there's not enough horizontal
    /// room — see the layout logic in `draw_revision`. The lane color
    /// these chips wear is also the color of the overflow chip.
    pub bookmark_chips: Vec<Chip>,
    /// Status chips (e.g. `empty`, `conflict`). Always shown — they're
    /// load-bearing semantics and shouldn't compress.
    pub status_chips: Vec<Chip>,
    /// Color for the `+N` overflow chip (= the row's lane color).
    pub lane_color: Color,
    pub frame: LaneFrame,
    /// Warped lane → display-column maps for this row and the previous one
    /// (`LaneFrame::display_columns`): x positions only — every lane-keyed
    /// lookup (labels, segments, colors) stays on original indices.
    pub columns: Vec<Option<usize>>,
    pub prev_columns: Vec<Option<usize>>,
    /// Bookmarks naming each lane visible at this row. Indexed by lane.
    /// An empty inner vec means the lane segment is anonymous (no
    /// bookmark up to this point in the walk). Used for the
    /// graph-stroke hover tooltip.
    pub lane_labels: Vec<Vec<String>>,
    /// Per-lane segment ids at this row, split by row half. The
    /// "incoming" half (`before`) covers strokes from the row's top edge
    /// down to the disc midline; the "outgoing" half (`after`) covers
    /// strokes from the disc midline down to the bottom edge. Two rows
    /// share an id on the same lane iff they belong to the same
    /// continuous lane segment.
    ///
    /// The two halves can disagree at a merge commit whose lane index
    /// is reused by an outgoing branch (the merged-in branch terminates
    /// in `before`, the new branch starts in `after`). Drawing emphasis
    /// at that row only highlights the half whose segment id matches the
    /// hovered branch — otherwise the merge stub of the dying branch
    /// would also glow when hovering the new branch.
    pub lane_segments_before: Vec<Option<usize>>,
    pub lane_segments_after: Vec<Option<usize>>,
    /// Whether to draw the inline collapse/expand chevron on the
    /// description row. `None` skips the glyph entirely (non-selected
    /// rows have no file list to toggle). `Some(true)` shows the
    /// "expanded" chevron, `Some(false)` shows the "collapsed" one.
    pub collapse_chevron: Option<bool>,
    /// Target mode: this row is a draft source (the thing being moved) —
    /// worn as an accent wash so it reads as "lifted off".
    pub draft_source: bool,
    /// The row is marked in the sidebar's multi-selection (⌘/⇧-click) —
    /// wears the same accent wash as a draft source: both mean "this row is
    /// part of the set the next action applies to".
    pub multi_selected: bool,
    /// Target mode: the keyboard candidate's destination decoration.
    pub draft_marker: Option<DraftMarker>,
    /// A previewed row that only shows the result (where commits land, a
    /// created commit, a ghost): it can't be selected or aimed at.
    pub inert: bool,
    /// A previewed ghost: where a moved or abandoned commit used to be,
    /// drawn faded with a hollow node.
    pub ghost: bool,
}

#[derive(Debug, Clone)]
pub struct FileRowView {
    /// Display label: a file's basename, or a directory's (possibly
    /// chain-compacted) name.
    pub primary: String,
    pub raw_path: String,
    pub status_label: String,
    pub status_background: Color,
    pub status_text: Color,
    pub additions: usize,
    pub deletions: usize,
    pub additions_text: Color,
    pub deletions_text: Color,
    /// Continuation lane state for the parent revision, shared (`Rc`) across
    /// every file row under it rather than deep-copied per row — the data is
    /// identical for all of them and these rows rebuild on every `view()`.
    pub continuation: Rc<[Option<GraphEdgeType>]>,
    /// Warped display columns of the parent revision row (see
    /// [`RevisionRowView::columns`]); the continuation strokes inherit its
    /// packing so the strip doesn't jump at the revision/file boundary.
    pub columns: Rc<[Option<usize>]>,
    pub additions_width: f32,
    pub deletions_width: f32,
    pub primary_color: Color,
    /// Tint of the file/folder icon ahead of the name (dimmer than the
    /// name for regular rows; follows the row tone for untracked/ignored).
    pub icon_color: Color,
    /// Tree indentation in px (depth × step), applied left of the badge.
    pub indent: f32,
    /// `Some(collapsed)` renders this as a directory row: a chevron instead
    /// of the status badge, no +/- stats. `None` is a file leaf.
    pub chevron: Option<bool>,
    /// The document file index for leaves (selection highlight + click);
    /// `usize::MAX` for directory rows, which never match a selection.
    pub file_index: usize,
    /// Lane → bookmark labels for the continuation strokes at this file
    /// row. Equals the post-trim snapshot of the parent revision's
    /// label state, so a lane that terminates at the parent isn't
    /// labeled here.
    pub lane_labels: Rc<[Vec<String>]>,
    /// Per-lane segment id, matching the parent revision row for any
    /// lane that survives into the continuation. See
    /// [`RevisionRowView::lane_segments`].
    pub lane_segments: Rc<[Option<usize>]>,
    /// Change-selection checkbox drawn in place of the file/folder icon, or
    /// `None` for no checkbox (see [`RevisionListProps::on_file_check`]).
    pub check: Option<crate::diff_view::CheckState>,
}

/// What a flat row index maps to: a commit (by index) or a file row under the
/// expanded commit (by its display index into the file tree).
#[derive(Clone, Copy)]
enum RowKind {
    Revision(usize),
    File(usize),
}

fn row_height_of(kind: RowKind) -> f32 {
    match kind {
        RowKind::Revision(_) => REVISION_ROW_HEIGHT,
        RowKind::File(_) => FILE_ROW_HEIGHT,
    }
}

/// Times a list has painted. A frame benchmark reads this to know it timed real
/// frames rather than cached ones.
pub static PAINTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Builds one revision row's view, by commit index.
pub type BuildRevision = Rc<dyn Fn(usize, &App) -> RevisionRowView>;
/// Builds one file row's view, by display index into the file tree.
pub type BuildFile = Rc<dyn Fn(usize, &App) -> FileRowView>;
/// Whether the gap below a commit row is a real parent edge.
pub type GapEdge = Rc<dyn Fn(usize, &App) -> bool>;
/// A list's way back to the app. `Diffui::listen` and `cx.listener` both make one.
pub type Callback<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

/// Callbacks for the drag-to-rebase gesture.
pub struct DragHooks {
    /// The drag crossed its activation threshold on this commit row.
    pub start: Callback<usize>,
    /// The drop spot under the cursor changed (`None` = no valid spot).
    pub hover: Callback<Option<DropSpot>>,
    /// Released: on a spot, or `None` when let go outside any spot.
    pub drop: Callback<Option<DropSpot>>,
}

/// Everything a list is told per render.
///
/// Virtualized at the *data* level: instead of holding an `Item` per commit
/// (~600MB on a million-commit repo), it holds the commit count, the
/// expanded-files span, and closures that materialize a single row's display
/// view on demand. Layout and hit tests use arithmetic; paint builds only the
/// rows that are actually on screen. The closures run during paint and event
/// handling, never inside the parent's render, so they read the app through an
/// entity handle they capture.
pub struct RevisionListProps {
    commit_count: usize,
    /// `(flat index where file rows start, file count)`. For an expanded
    /// commit that's the commit's index + 1; a files-only list (the source
    /// tree) passes `(0, files)` with a zero `commit_count`. `None` when
    /// nothing is expanded.
    expanded: Option<(usize, usize)>,
    style: RevisionListStyle,
    build_revision: Option<BuildRevision>,
    build_file: Option<BuildFile>,
    selected_row: Option<RowSelectionKey>,
    selected_file: Option<usize>,
    /// Flat row index of the selected row, for the reveal-on-token scroll.
    selected_flat: Option<usize>,
    /// Bumped by the caller each time it wants the selected row scrolled into
    /// view.
    reveal_token: Option<u64>,
    /// Reveal an explicit file row (keyboard file navigation) instead of the
    /// selected revision: a bumped token plus the file's flat row. Kept separate
    /// from `reveal_token` so scrolling to a file doesn't also re-centre the
    /// selected revision.
    file_reveal_token: Option<u64>,
    reveal_file_flat: Option<usize>,
    on_select_revision: Option<Callback<RowSelectionKey>>,
    /// Receives the clicked file row's *display* index into the file tree
    /// (dir rows included), not a document file index.
    on_select_file: Option<Callback<usize>>,
    /// Right-click on a revision row: opens the context menu. Receives the
    /// row's selection key, its card's rectangle in window coordinates (what
    /// anchors a native highlight) and the cursor.
    on_context_menu: Option<Callback<(RowSelectionKey, Rectangle, Point)>>,
    /// Right-click on a file row: the row's *display* index into the file tree
    /// (like `on_select_file`), its rect, and the cursor.
    on_file_context_menu: Option<Callback<(usize, Rectangle, Point)>>,
    /// A click on a file row's checkbox (its icon column), by display index.
    on_file_check: Option<Callback<usize>>,
    /// Reports the scroll offset (content-space px) whenever it changes, so the
    /// app can persist it per-tab.
    on_scroll: Option<Callback<f64>>,
    /// One-shot scroll restore: when `restore_token` differs from the value the
    /// list last saw, the offset jumps to `restore_offset` (clamped). Used to
    /// re-apply a tab's saved scroll on activation, since one list serves every
    /// tab and would otherwise leak the prior tab's position.
    restore_offset: f64,
    restore_token: u64,
    /// Drag-to-rebase hooks; `None` disables dragging (git repos, PR tabs).
    on_drag: Option<DragHooks>,
    /// Whether the gap *below* commit index `i` is a real parent edge (the
    /// next display row is a direct parent of row `i`). Adjacent rows in a
    /// branched log are often unrelated: inserting "between" them would
    /// silently rewire the upper row onto the dragged commit, so gap drop
    /// spots only exist where this returns `true`. `None` disables gaps.
    gap_edge: Option<GapEdge>,
    /// Target mode's plain-hover channel: the commit row under the cursor
    /// (no drag needed), `None` when the cursor leaves the rows. The app
    /// arms it as the draft *candidate*, same as keyboard `j`/`k`, honoring
    /// the armed `o`/`a`/`b` placement, so the op bar's "hover … to preview"
    /// holds for mouse users. Drags keep their own spot channel
    /// (`DragHooks::hover`); gaps stay a drag-only gesture.
    on_target_hover: Option<Callback<Option<usize>>>,
    /// Extra leading inset for the row cards: see
    /// [`RevisionListProps::at_window_edge`].
    leading_edge: f32,
    /// The drop spot to draw while dragging, when the caller knows better than
    /// the cursor: see [`RevisionListProps::drop_indicator`].
    drop_indicator: Option<Option<DropSpot>>,
    /// How many rows the caller currently marks. The drag ghost names the
    /// count when a drag starts on one of them: see
    /// [`RevisionListProps::multi_selection_count`].
    multi_selection_count: usize,
}

impl RevisionListProps {
    pub fn new(
        commit_count: usize,
        expanded: Option<(usize, usize)>,
        style: RevisionListStyle,
    ) -> Self {
        Self {
            commit_count,
            expanded,
            style,
            build_revision: None,
            build_file: None,
            selected_row: None,
            selected_file: None,
            selected_flat: None,
            reveal_token: None,
            file_reveal_token: None,
            reveal_file_flat: None,
            on_select_revision: None,
            on_select_file: None,
            on_context_menu: None,
            on_file_context_menu: None,
            on_file_check: None,
            on_scroll: None,
            restore_offset: 0.0,
            restore_token: 0,
            on_drag: None,
            gap_edge: None,
            on_target_hover: None,
            leading_edge: 0.0,
            drop_indicator: None,
            multi_selection_count: 0,
        }
    }

    /// The revision rows: how to build one, which is selected, and where a
    /// click on one goes.
    pub fn revisions(
        mut self,
        build: BuildRevision,
        selected_row: Option<RowSelectionKey>,
        selected_flat: Option<usize>,
        on_select: impl Fn(&RowSelectionKey, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.build_revision = Some(build);
        self.selected_row = selected_row;
        self.selected_flat = selected_flat;
        self.on_select_revision = Some(Rc::new(on_select));
        self
    }

    /// The file rows: how to build one, which document file is selected, and
    /// where a click on one goes.
    pub fn files(
        mut self,
        build: BuildFile,
        selected_file: Option<usize>,
        on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.build_file = Some(build);
        self.selected_file = selected_file;
        self.on_select_file = Some(Rc::new(on_select));
        self
    }

    /// Tell the list how many rows are marked, so a drag that starts on one
    /// of them can name the batch ("3 revisions") instead of the grabbed
    /// row's change id.
    pub fn multi_selection_count(mut self, count: usize) -> Self {
        self.multi_selection_count = count;
        self
    }

    /// Draw the live drag's drop decoration at `spot` (list rows) instead of
    /// at the spot under the cursor. For when the rows were rearranged under
    /// a still cursor (a previewed result), so the spot it last reported is
    /// stale while the drop itself stays where it was.
    pub fn drop_indicator(mut self, spot: Option<DropSpot>) -> Self {
        self.drop_indicator = Some(spot);
        self
    }

    /// The list's leading edge is the window's: push the row cards in by the
    /// width the OS border covers, so their margin there matches the one
    /// against the divider on the other side.
    pub fn at_window_edge(mut self) -> Self {
        self.leading_edge = crate::chrome::window_border();
        self
    }

    /// Report the hovered commit row while an op draft is picking a
    /// destination: see the `on_target_hover` field.
    pub fn on_target_hover(
        mut self,
        callback: impl Fn(&Option<usize>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_target_hover = Some(Rc::new(callback));
        self
    }

    /// Enable drag-to-rebase on revision rows.
    pub fn on_drag(mut self, hooks: DragHooks) -> Self {
        self.on_drag = Some(hooks);
        self
    }

    /// Provide the parent-edge test that legitimizes insert-between gaps
    /// (see the `gap_edge` field). Without it, drops only land on rows.
    pub fn gap_edges(mut self, edge: impl Fn(usize, &App) -> bool + 'static) -> Self {
        self.gap_edge = Some(Rc::new(edge));
        self
    }

    /// Register a right-click handler for revision rows (the context menu).
    pub fn on_context_menu(
        mut self,
        callback: impl Fn(&(RowSelectionKey, Rectangle, Point), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_context_menu = Some(Rc::new(callback));
        self
    }

    /// Register a right-click handler for file rows.
    pub fn on_file_context_menu(
        mut self,
        callback: impl Fn(&(usize, Rectangle, Point), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_file_context_menu = Some(Rc::new(callback));
        self
    }

    /// Register a click handler for file-row checkboxes (rows whose
    /// [`FileRowView::check`] is set). Gets the row's display index.
    pub fn on_file_check(
        mut self,
        callback: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_file_check = Some(Rc::new(callback));
        self
    }

    /// Scroll the currently-selected row into view the next time `token`
    /// changes. Calling with the same value twice in a row is a no-op.
    pub fn reveal_selected(mut self, token: u64) -> Self {
        self.reveal_token = Some(token);
        self
    }

    /// Like [`Self::reveal_selected`], but scrolls an explicit file row into
    /// view (keyboard file navigation) the next time `token` changes. `flat` is
    /// the file's flat row, or `None` when the file list isn't open, in which
    /// case the token change schedules no scroll.
    pub fn reveal_file(mut self, token: u64, flat: Option<usize>) -> Self {
        self.file_reveal_token = Some(token);
        self.reveal_file_flat = flat;
        self
    }

    /// Report scroll-offset changes (content-space px) so the caller can persist
    /// the position. See [`Self::restore_scroll`] for the inverse.
    pub fn on_scroll(mut self, callback: impl Fn(&f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_scroll = Some(Rc::new(callback));
        self
    }

    /// Jump the scroll offset to `offset` the next time `token` changes. Calling
    /// with the same `token` twice is a no-op, so live scrolling is preserved
    /// between restores. Takes precedence over a `reveal_selected` scheduled in
    /// the same render (a tab restore wants the exact saved offset, not a
    /// re-centred selection).
    pub fn restore_scroll(mut self, offset: f64, token: u64) -> Self {
        self.restore_offset = offset;
        self.restore_token = token;
        self
    }

    fn row_count(&self) -> usize {
        rows_total(self.commit_count, self.expanded)
    }

    fn row_kind(&self, flat: usize) -> RowKind {
        row_kind_at(self.expanded, flat)
    }

    /// Materialize the display view for a single row. Builds only what the
    /// caller asked for, so paint and hit tests pay only for on-screen rows.
    fn item_at(&self, flat: usize, cx: &App) -> Option<Item> {
        match self.row_kind(flat) {
            RowKind::Revision(commit) => self
                .build_revision
                .as_ref()
                .map(|build| Item::Revision(build(commit, cx))),
            RowKind::File(file) => self
                .build_file
                .as_ref()
                .map(|build| Item::File(build(file, cx))),
        }
    }

    fn content_height(&self) -> f64 {
        rows_content_height(self.commit_count, self.expanded)
    }

    fn row_at_offset(&self, offset: f64) -> Option<usize> {
        row_at_offset_in(self.commit_count, self.expanded, offset)
    }

    fn row_top(&self, flat: usize) -> f64 {
        row_top_at(self.expanded, flat)
    }

    /// Flat row index of a commit-store index (the inverse of
    /// `row_kind_at`'s `Revision` arm, accounting for the expanded file
    /// block sitting between revisions).
    fn commit_flat(&self, commit: usize) -> usize {
        match self.expanded {
            Some((start, files)) if commit >= start => commit + files,
            _ => commit,
        }
    }

    /// The commit a flat row resolves to for drop purposes: revision rows
    /// map to themselves, file rows to the revision they're expanded under.
    fn drop_commit_of(&self, flat: usize) -> Option<usize> {
        match self.row_kind(flat) {
            RowKind::Revision(commit) => Some(commit),
            // The file block starts right after its revision's flat row, and
            // revisions before it are 1:1 with flats, so the parent's commit
            // index is `start - 1`.
            RowKind::File(_) => self.expanded.and_then(|(start, _)| start.checked_sub(1)),
        }
    }

    /// Resolve a drag cursor position (content-space y) to a drop spot: the
    /// row under it, or, within a few px of a boundary between two revision
    /// rows, the gap between them. File rows resolve to their parent revision;
    /// only genuine revision/revision boundaries that are also real parent
    /// edges (`gap_edge`) read as gaps, so a drop can't "insert between" two
    /// unrelated branches that merely render adjacently.
    fn drop_spot_at(&self, local_y: f64, cx: &App) -> Option<DropSpot> {
        let flat = self.row_at_offset(local_y)?;
        let commit = self.drop_commit_of(flat)?;
        if !matches!(self.row_kind(flat), RowKind::Revision(_)) {
            return Some(DropSpot::OnRow(commit));
        }
        let is_edge = |upper: usize| self.gap_edge.as_ref().is_some_and(|edge| edge(upper, cx));
        let top = self.row_top(flat);
        let height = f64::from(row_height_of(self.row_kind(flat)));
        if local_y - top < GAP_ZONE
            && flat > 0
            && let Some(above) = self.drop_commit_of(flat - 1)
            && above != commit
            && is_edge(above)
        {
            return Some(DropSpot::Gap {
                above,
                below: commit,
            });
        }
        if top + height - local_y < GAP_ZONE
            && flat + 1 < self.row_count()
            && let Some(below) = self.drop_commit_of(flat + 1)
            && below != commit
            && is_edge(commit)
        {
            return Some(DropSpot::Gap {
                above: commit,
                below,
            });
        }
        Some(DropSpot::OnRow(commit))
    }

    /// A row's selection/hover card: inset from the row on every side.
    fn card_bounds(&self, row: Rectangle) -> Rectangle {
        let left = ROW_CARD_INSET_X + self.leading_edge;
        Rectangle {
            x: row.x + left,
            y: row.y + ROW_CARD_INSET_Y,
            width: (row.width - left - ROW_CARD_INSET_X).max(0.0),
            height: (row.height - ROW_CARD_INSET_Y * 2.0).max(0.0),
        }
    }

    /// Horizontal layout of a file row's content columns:
    /// [indent][chevron][icon] name … [+N] [-N] [status chip] pad.
    /// Shared between painting (which ellipsizes the name into `path_w`) and
    /// the tooltip's "is the name actually truncated?" check, so the two can
    /// never disagree.
    fn file_row_columns(
        &self,
        row_x: f32,
        row_width: f32,
        gutter_total: f32,
        file: &FileRowView,
    ) -> FileRowColumns {
        let chip_col = self.style.file_badge_width;
        let chip_left = row_x + row_width - CONTENT_PADDING - chip_col;
        let chip_gap = if chip_col > 0.0 { FILE_ROW_GAP } else { 0.0 };
        let minus_x = chip_left - chip_gap - file.deletions_width;
        let plus_x = minus_x - FILE_ROW_GAP - file.additions_width;
        let path_x =
            row_x + gutter_total + file.indent + FILE_CHEVRON_COL + FILE_ICON_COL + FILE_ROW_GAP;
        let path_right = if file.chevron.is_some() {
            row_x + row_width - FILE_ROW_RIGHT_PAD
        } else {
            plus_x - FILE_ROW_GAP
        };
        FileRowColumns {
            chip_left,
            minus_x,
            plus_x,
            path_x,
            path_w: (path_right - path_x).max(1.0),
        }
    }
}

fn item_gutter_width(item: &Item) -> f32 {
    // Warped width: the strip is as wide as the rightmost *display*
    // column in use, not the rightmost original lane index.
    let lanes = match item {
        Item::Revision(row) => revision_strip_columns(
            &row.columns,
            &row.prev_columns,
            &row.frame.before,
            row.frame.lane_count(),
        ),
        Item::File(row) => row
            .continuation
            .iter()
            .enumerate()
            .filter(|(_, kind)| kind.is_some())
            .filter_map(|(lane, _)| row.columns.get(lane).copied().flatten())
            .max()
            .map_or(row.continuation.len(), |max| max + 1),
    };
    GUTTER_LEFT_PADDING + lane_strip_width(lanes) + GUTTER_PADDING
}

// Pure row-geometry helpers, split out so they can be unit-tested without
// constructing a full list. `expanded` is `(flat index where file rows
// start, file count)`; revision rows fill the flats on either side.

fn rows_total(commit_count: usize, expanded: Option<(usize, usize)>) -> usize {
    commit_count + expanded.map_or(0, |(_, files)| files)
}

fn row_kind_at(expanded: Option<(usize, usize)>, flat: usize) -> RowKind {
    match expanded {
        Some((start, files)) if flat >= start && flat < start + files => {
            RowKind::File(flat - start)
        }
        Some((start, files)) if flat >= start + files => RowKind::Revision(flat - files),
        _ => RowKind::Revision(flat),
    }
}

// These work in `f64` content-space pixels deliberately. A ~1M-commit list is
// ~50M px tall, which is past `f32`'s exact-integer ceiling (2^24 ≈ 16.7M), so
// in `f32` the paint path's `flat * H` and the hit-test path's `y / H` rounded
// to different multiples: a click near the bottom selected the neighbouring
// row. `f64` is exact for integers to 2^53 (~9e15), far beyond any real repo,
// so the two paths agree again. Row heights stay `f32` constants (whole px).
fn rows_content_height(commit_count: usize, expanded: Option<(usize, usize)>) -> f64 {
    commit_count as f64 * REVISION_ROW_HEIGHT as f64
        + expanded.map_or(0, |(_, files)| files) as f64 * FILE_ROW_HEIGHT as f64
}

fn row_top_at(expanded: Option<(usize, usize)>, flat: usize) -> f64 {
    let (revisions_before, files_before) = match expanded {
        Some((start, files)) if flat >= start => {
            let files_before = (flat - start).min(files);
            (flat - files_before, files_before)
        }
        _ => (flat, 0),
    };
    revisions_before as f64 * REVISION_ROW_HEIGHT as f64
        + files_before as f64 * FILE_ROW_HEIGHT as f64
}

fn row_at_offset_in(
    commit_count: usize,
    expanded: Option<(usize, usize)>,
    offset: f64,
) -> Option<usize> {
    if offset < 0.0 {
        return None;
    }
    let rev_h = REVISION_ROW_HEIGHT as f64;
    let file_h = FILE_ROW_HEIGHT as f64;
    let flat = match expanded {
        Some((start, files)) => {
            let files_top = start as f64 * rev_h;
            let files_bottom = files_top + files as f64 * file_h;
            if offset < files_top {
                (offset / rev_h) as usize
            } else if offset < files_bottom {
                start + ((offset - files_top) / file_h) as usize
            } else {
                start + files + ((offset - files_bottom) / rev_h) as usize
            }
        }
        None => (offset / rev_h) as usize,
    };
    (flat < rows_total(commit_count, expanded)).then_some(flat)
}

/// The offset that brings a row into view: a third of the way down the viewport
/// when it has to move, which gives context above and below without pinning it
/// to the top edge, and the current offset when it's already visible.
fn reveal_offset(current: f64, row_top: f64, row_h: f64, view_h: f64, max: f64) -> f64 {
    let preferred_top = (row_top - view_h * 0.33).max(0.0);
    let must_top = (row_top + row_h - view_h).max(0.0);
    let visible = current..current + view_h;
    let target = if visible.contains(&row_top) && visible.contains(&(row_top + row_h)) {
        current
    } else if current > row_top {
        preferred_top
    } else {
        must_top.max(preferred_top)
    };
    target.clamp(0.0, max)
}

/// Which half of a revision row the cursor is on, used to pick the
/// right segment id when a lane is split (its incoming and outgoing
/// halves carry different segment ids: see
/// [`RevisionRowView::lane_segments_before`]).
///
/// File rows aren't split (their continuation is a single half), so
/// this only matters for `Item::Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneHalf {
    /// Top half of the row: the strokes drawn into the disc.
    Before,
    /// Bottom half of the row: the strokes drawn out of the disc.
    After,
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    /// Commit-store index the drag started on.
    source_commit: usize,
    /// Press position: the activation threshold anchor.
    origin: Point,
    /// Whether the threshold was crossed (a plain click never activates).
    active: bool,
    /// The spot currently under the cursor, mirrored to the app on change.
    spot: Option<DropSpot>,
    /// How many marked rows this drag moves. 1 for a single-row drag;
    /// captured at activation because the app clears the marks when the
    /// draft's source wash takes over.
    batch_count: usize,
    /// Live cursor position for the ghost chip.
    cursor: Point,
}

/// Calls to make once the list's lease has ended: the app's handlers may read
/// the list's rows back, and they run outside any update of this entity.
type Effect = Box<dyn FnOnce(&mut Window, &mut App)>;
type Effects = Vec<Effect>;

fn call<T: 'static>(callback: &Callback<T>, value: T) -> Effect {
    let callback = callback.clone();
    Box::new(move |window, cx| callback(&value, window, cx))
}

/// Something drawn over the rest of the window, and where.
type Floating = (AnyElement, gpui_kit::Point<Pixels>);

/// The list's entity: what it remembers across frames.
pub struct RevisionList {
    props: Rc<RevisionListProps>,
    /// Painted bounds, in window coordinates.
    bounds: Rectangle,
    /// Scroll position in `f64` content-space px (see the row-geometry helpers
    /// for why `f64`). Cast to `f32` only at the scrollbar/paint boundary,
    /// where values are viewport-small and `f32` is exact.
    vertical_offset: f64,
    /// Item index of the file row currently hovered (only set for files,
    /// not revisions). Drives the tooltip.
    hovered_file_item: Option<usize>,
    /// Item index of the revision row currently hovered. Drives the row's
    /// hover wash (files get theirs via `hovered_file_item`).
    hovered_revision_item: Option<usize>,
    /// Item index + lane index + which half of the row the cursor is on,
    /// when the cursor is over a graph stroke whose lane has bookmark
    /// labels at that row. Drives the branch-name tooltip on the graph
    /// gutter.
    hovered_lane: Option<(usize, usize, LaneHalf)>,
    /// Last cursor position observed inside the list's bounds. Anchors the
    /// tooltip.
    cursor_position: Option<Point>,
    over_scrollbar: bool,
    /// What the bar over the list reads, and where a drag on it asks to go.
    bar: scrollbar::Shared,
    /// Wheel motion still to be drawn, spread evenly over frames.
    wheel: Resampler,
    /// Most recent `reveal_token` we acted on. `None` until first reveal.
    last_reveal_token: Option<u64>,
    /// Most recent `file_reveal_token` we acted on. `None` until first reveal.
    last_file_reveal_token: Option<u64>,
    /// Row to bring into view once bounds are known: set when the props change,
    /// consumed in prepaint.
    pending_reveal_row: Option<usize>,
    /// Most recent `restore_token` we acted on. A change schedules a one-shot
    /// jump to the caller's `restore_offset`. Starts at 0 to match the caller's
    /// initial token, so a cold load doesn't trigger a spurious restore.
    last_restore_token: u64,
    /// Offset to jump to once bounds are known, set when `restore_token`
    /// changed.
    pending_set_offset: Option<f64>,
    /// In-flight drag-to-rebase gesture, armed on a revision-row press and
    /// activated past a small movement threshold.
    drag: Option<DragState>,
    /// A revision-row press waiting for its release to become a click
    /// (`(flat row, selection key)`). Cleared when a drag activates instead.
    pending_click: Option<(usize, RowSelectionKey)>,
    /// Last commit row published through `on_target_hover` (change-detection
    /// so plain mouse moves don't re-publish the same row). Reset around
    /// drags and when target mode ends so the next hover re-publishes.
    target_hover: Option<usize>,
    /// Shaped text widths, kept across frames. A row measures eight to fifteen
    /// strings to lay itself out, and they are the same strings every frame:
    /// see [`WidthMemo`].
    widths: WidthMemo,
    _escape: Subscription,
}

/// The list named `id`, drawing `props`. See [`RevisionListProps`].
pub fn view(
    id: &'static str,
    props: RevisionListProps,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let list = window.use_keyed_state(id, cx, |_, cx| RevisionList::new(cx));
    list.update(cx, |list, _| list.set_props(props));
    list.into_any_element()
}

impl RevisionList {
    fn new(cx: &mut Context<Self>) -> Self {
        // Esc abandons an active drag visually; the app's own Esc handling (the
        // draft cancel) rides the same keystroke through its own interceptor.
        // Every interceptor sees every keystroke, whatever an earlier one did.
        let list = cx.weak_entity();
        let escape = cx.intercept_keystrokes(move |event, window, cx| {
            if event.keystroke.key != "escape" {
                return;
            }
            let effects = list
                .update(cx, |list, cx| list.cancel_drag(cx))
                .unwrap_or_default();
            for effect in effects {
                effect(window, cx);
            }
        });
        Self {
            props: Rc::new(RevisionListProps::new(0, None, placeholder_style())),
            bounds: Rectangle::default(),
            vertical_offset: 0.0,
            hovered_file_item: None,
            hovered_revision_item: None,
            hovered_lane: None,
            cursor_position: None,
            over_scrollbar: false,
            bar: scrollbar::Shared::default(),
            wheel: Resampler::default(),
            last_reveal_token: None,
            last_file_reveal_token: None,
            pending_reveal_row: None,
            last_restore_token: 0,
            pending_set_offset: None,
            drag: None,
            pending_click: None,
            target_hover: None,
            widths: WidthMemo::default(),
            _escape: escape,
        }
    }

    /// Takes this render's props, noticing the tokens that schedule a scroll.
    /// The scroll itself waits for prepaint, which knows the bounds it clamps
    /// and centres against.
    fn set_props(&mut self, props: RevisionListProps) {
        if props.reveal_token != self.last_reveal_token {
            self.last_reveal_token = props.reveal_token;
            self.pending_reveal_row = props.selected_flat;
        }
        if props.file_reveal_token != self.last_file_reveal_token {
            self.last_file_reveal_token = props.file_reveal_token;
            // A file reveal targets an explicit row; skip when the file list is
            // closed (`None`). Scheduled after the revision reveal so a file
            // selection wins if both somehow change in one render.
            if let Some(flat) = props.reveal_file_flat {
                self.pending_reveal_row = Some(flat);
            }
        }
        if props.restore_token != self.last_restore_token {
            self.last_restore_token = props.restore_token;
            // Applied after the reveal in prepaint, so an exact restore wins
            // over a re-centred selection when both land in the same render.
            self.pending_set_offset = Some(props.restore_offset);
        }
        // No notify: this runs in the render that's about to draw the element, and a
        // notify during a draw would dirty the view, and every view above it, for the
        // next frame too.
        self.props = Rc::new(props);
    }

    fn max_offset(&self) -> f64 {
        (self.props.content_height() - f64::from(self.bounds.height)).max(0.0)
    }

    /// Applies what waited for bounds: the clamp, a reveal, a restore. Returns
    /// the offset to report when one of them moved it.
    fn settle_offset(&mut self) -> Option<f64> {
        let before = self.vertical_offset;
        let max = self.max_offset();
        self.vertical_offset = self.vertical_offset.min(max);
        if let Some(flat) = self.pending_reveal_row.take()
            && flat < self.props.row_count()
        {
            self.vertical_offset = reveal_offset(
                self.vertical_offset,
                self.props.row_top(flat),
                f64::from(row_height_of(self.props.row_kind(flat))),
                f64::from(self.bounds.height),
                max,
            );
        }
        // Tab restore: jump straight to the saved offset, overriding any reveal
        // scheduled above.
        if let Some(offset) = self.pending_set_offset.take() {
            self.vertical_offset = offset.clamp(0.0, max);
        }
        (self.vertical_offset != before).then_some(self.vertical_offset)
    }

    fn cancel_drag(&mut self, cx: &mut Context<Self>) -> Effects {
        let mut effects = Effects::new();
        if let Some(drag) = self.drag.take() {
            self.pending_click = None;
            if drag.active
                && let Some(hooks) = &self.props.on_drag
            {
                effects.push(call(&hooks.hover, None));
            }
            cx.notify();
        }
        effects
    }

    fn clear_hover(&mut self) -> bool {
        self.hovered_file_item.take().is_some()
            | self.hovered_revision_item.take().is_some()
            | self.hovered_lane.take().is_some()
    }

    fn recompute_hover(&mut self, cx: &App) {
        self.hovered_file_item = None;
        self.hovered_revision_item = None;
        self.hovered_lane = None;
        let Some(pos) = self.cursor_position else {
            return;
        };
        if !self.bounds.contains(pos) {
            return;
        }
        let local_x = pos.x - self.bounds.x;
        let local_y = f64::from(pos.y - self.bounds.y) + self.vertical_offset;
        let props = self.props.clone();
        let Some(row_idx) = props.row_at_offset(local_y) else {
            return;
        };
        let Some(item) = props.item_at(row_idx, cx) else {
            return;
        };
        let gutter_total = item_gutter_width(&item);
        // Lane hit-test first: a cursor in the lane strip with a
        // labeled lane wins over the content-area file tooltip.
        if local_x >= GUTTER_LEFT_PADDING && local_x < gutter_total - GUTTER_PADDING {
            let strip_x = local_x - GUTTER_LEFT_PADDING;
            let display = (strip_x / LANE_WIDTH).floor() as usize;
            // Pick the half from the cursor's vertical position within the
            // row, so a click on the incoming stub of a split lane resolves to
            // that branch's segment (not the unrelated new branch starting
            // below).
            let row_h = f64::from(row_height_of(props.row_kind(row_idx)));
            let half = if local_y - props.row_top(row_idx) < row_h / 2.0 {
                LaneHalf::Before
            } else {
                LaneHalf::After
            };
            // The cursor x is a *display* column under lane warping;
            // everything downstream (labels, segments) keys on the
            // original lane, so map it back first.
            if let Some(lane) = item_display_to_lane(&item, display, half)
                && !item_lane_labels(&item, lane).is_empty()
            {
                self.hovered_lane = Some((row_idx, lane, half));
                return;
            }
        }
        match item {
            Item::File(_) => self.hovered_file_item = Some(row_idx),
            Item::Revision(_) => self.hovered_revision_item = Some(row_idx),
        }
    }

    /// Runs a pointer handler, then reports the offset if it moved, whichever
    /// path moved it, exactly once.
    fn with_scroll_report(
        &mut self,
        handler: impl FnOnce(&mut Self, &mut Effects, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Effects {
        let before = self.vertical_offset;
        let mut effects = Effects::new();
        handler(self, &mut effects, cx);
        if self.vertical_offset != before
            && let Some(on_scroll) = &self.props.on_scroll
        {
            effects.push(call(on_scroll, self.vertical_offset));
        }
        effects
    }

    fn on_mouse_move(
        &mut self,
        position: Point,
        hovered: bool,
        effects: &mut Effects,
        cx: &mut Context<Self>,
    ) {
        let props = self.props.clone();
        let bounds = self.bounds;
        // Drag-to-rebase tracking: past a small threshold the press becomes a
        // drag; from then on the list owns the cursor (no hover washes) and
        // mirrors spot changes to the app.
        if let Some(hooks) = &props.on_drag
            && let Some(mut drag) = self.drag
        {
            drag.cursor = position;
            if !drag.active && position.distance(drag.origin) >= DRAG_THRESHOLD {
                drag.active = true;
                // A grab on a marked row carries the whole batch; remember its
                // size before the app clears the marks for the draft.
                drag.batch_count = match props.item_at(props.commit_flat(drag.source_commit), cx) {
                    Some(Item::Revision(rev))
                        if rev.multi_selected && props.multi_selection_count > 1 =>
                    {
                        props.multi_selection_count
                    }
                    _ => 1,
                };
                // The drag owns spot publishing from here; reset the plain-hover
                // dedupe so the first post-drag hover re-publishes whatever it
                // lands on.
                self.target_hover = None;
                effects.push(call(&hooks.start, drag.source_commit));
            }
            if drag.active {
                let spot = if bounds.contains(position) {
                    let local_y = f64::from(position.y - bounds.y) + self.vertical_offset;
                    props.drop_spot_at(local_y, cx)
                } else {
                    None
                };
                if spot != drag.spot {
                    drag.spot = spot;
                    effects.push(call(&hooks.hover, spot));
                }
                self.cursor_position = None;
                self.clear_hover();
                self.drag = Some(drag);
                cx.notify();
                return;
            }
            self.drag = Some(drag);
        }
        // Target mode without a live drag: mirror the hovered commit row to the
        // app so mouse users get the destination hint + preview without
        // dragging. File rows resolve to their revision, matching drop
        // semantics.
        let inside = hovered && bounds.contains(position);
        if let Some(on_target_hover) = &props.on_target_hover {
            let hover = if inside {
                let local_y = f64::from(position.y - bounds.y) + self.vertical_offset;
                props
                    .row_at_offset(local_y)
                    .and_then(|flat| props.drop_commit_of(flat))
            } else {
                None
            };
            if self.target_hover != hover {
                self.target_hover = hover;
                effects.push(call(on_target_hover, hover));
            }
        } else {
            self.target_hover = None;
        }
        self.cursor_position = inside.then_some(position);
        let before = (
            self.hovered_file_item,
            self.hovered_revision_item,
            self.hovered_lane,
        );
        self.recompute_hover(cx);
        // The cursor shape is set at paint, so crossing onto the bar repaints.
        let over_scrollbar =
            inside && scrollbar::hits_container(bounds, position, props.content_height() as f32);
        if before
            != (
                self.hovered_file_item,
                self.hovered_revision_item,
                self.hovered_lane,
            )
            || over_scrollbar != self.over_scrollbar
        {
            self.over_scrollbar = over_scrollbar;
            cx.notify();
        }
    }

    fn on_mouse_exit(&mut self, effects: &mut Effects, cx: &mut Context<Self>) {
        self.cursor_position = None;
        if self.target_hover.take().is_some()
            && let Some(on_target_hover) = &self.props.on_target_hover
        {
            effects.push(call(on_target_hover, None));
        }
        if self.clear_hover() {
            cx.notify();
        }
    }

    fn on_wheel(&mut self, delta_y: f32, cx: &mut Context<Self>) {
        if delta_y == 0.0 {
            return;
        }
        // A positive delta moves the content down, towards the top of the list.
        if scroll_resample::delay().is_zero() {
            self.vertical_offset =
                (self.vertical_offset - f64::from(delta_y)).clamp(0.0, self.max_offset());
            self.recompute_hover(cx);
        } else {
            // Drawn over the next frames: see `scroll_resample`.
            self.wheel.push(Instant::now(), -f64::from(delta_y));
        }
        cx.notify();
    }

    /// Returns whether the press was the list's.
    fn on_left_press(
        &mut self,
        cursor: Point,
        effects: &mut Effects,
        cx: &mut Context<Self>,
    ) -> bool {
        let props = self.props.clone();
        let bounds = self.bounds;
        let local_y = f64::from(cursor.y - bounds.y) + self.vertical_offset;
        let Some(row_idx) = props.row_at_offset(local_y) else {
            return false;
        };
        match props.row_kind(row_idx) {
            RowKind::Revision(commit) => {
                // Arm a potential drag; it only activates past the movement
                // threshold, so plain clicks stay clicks.
                if props.on_drag.is_some() {
                    self.drag = Some(DragState {
                        source_commit: commit,
                        origin: cursor,
                        active: false,
                        spot: None,
                        batch_count: 1,
                        cursor,
                    });
                }
                // The select fires on *release* (same row, no drag): publishing
                // on press meant starting a drag on the selected row toggled
                // its file list before the drag ever armed.
                if let Some(Item::Revision(rev)) = props.item_at(row_idx, cx)
                    && !rev.inert
                {
                    self.pending_click = Some((row_idx, rev.selection_key));
                }
            }
            // The callback wants the *display* row index into the file tree
            // (directories toggle their collapse state by position), not
            // `FileRowView::file_index`, which is the document index and
            // `usize::MAX` for dirs. File rows never drag, so their click stays
            // on press.
            RowKind::File(display_index) => {
                self.pending_click = None;
                let on_check = props.on_file_check.as_ref().filter(|_| {
                    let Some(item) = props.item_at(row_idx, cx) else {
                        return false;
                    };
                    let gutter_total = item_gutter_width(&item);
                    let Item::File(file) = item else {
                        return false;
                    };
                    let icon_x = bounds.x + gutter_total + file.indent + FILE_CHEVRON_COL;
                    file.check.is_some()
                        && cursor.x >= icon_x - 2.0
                        && cursor.x < icon_x + FILE_ICON_COL + 2.0
                });
                if let Some(on_check) = on_check {
                    effects.push(call(on_check, display_index));
                } else if let Some(on_select) = &props.on_select_file {
                    effects.push(call(on_select, display_index));
                }
            }
        }
        true
    }

    fn on_left_release(&mut self, cursor: Point, effects: &mut Effects, cx: &mut Context<Self>) {
        let props = self.props.clone();
        let pending_click = self.pending_click.take();
        if let Some(drag) = self.drag.take()
            && drag.active
        {
            self.target_hover = None;
            if let Some(hooks) = &props.on_drag {
                effects.push(call(&hooks.drop, drag.spot));
            }
            cx.notify();
        } else if let Some((pressed_flat, key)) = pending_click {
            // A click completes only when the release lands on the row it
            // pressed (button semantics): releasing after drifting off the row
            // aborts instead of mis-selecting.
            let released_on_same_row = self.bounds.contains(cursor)
                && props.row_at_offset(f64::from(cursor.y - self.bounds.y) + self.vertical_offset)
                    == Some(pressed_flat);
            if released_on_same_row && let Some(on_select) = &props.on_select_revision {
                effects.push(call(on_select, key));
            }
        }
    }

    /// Returns whether the press was the list's.
    fn on_right_press(
        &mut self,
        cursor: Point,
        effects: &mut Effects,
        cx: &mut Context<Self>,
    ) -> bool {
        let props = self.props.clone();
        let local_y = f64::from(cursor.y - self.bounds.y) + self.vertical_offset;
        let Some(row_idx) = props.row_at_offset(local_y) else {
            return false;
        };
        // The row card's on-screen rect: the highlight drawn while the menu is
        // open lies exactly over the hover/selection card, at ROW_CARD_RADIUS.
        let row_rect = props.card_bounds(Rectangle {
            x: self.bounds.x,
            y: self.row_screen_y(row_idx),
            width: self.bounds.width,
            height: row_height_of(props.row_kind(row_idx)),
        });
        match props.row_kind(row_idx) {
            RowKind::Revision(_) => {
                let Some(callback) = &props.on_context_menu else {
                    return false;
                };
                let Some(Item::Revision(rev)) = props.item_at(row_idx, cx) else {
                    return false;
                };
                effects.push(call(callback, (rev.selection_key, row_rect, cursor)));
            }
            RowKind::File(display_index) => {
                let Some(callback) = &props.on_file_context_menu else {
                    return false;
                };
                effects.push(call(callback, (display_index, row_rect, cursor)));
            }
        }
        true
    }

    /// Where a row's top lands in the window. `row_top - offset` is
    /// viewport-relative (small) and exact in `f64`; only after that subtraction
    /// cancels the large magnitude is it safe to narrow to `f32`.
    fn row_screen_y(&self, flat: usize) -> f32 {
        self.bounds.y + (self.props.row_top(flat) - self.vertical_offset) as f32
    }

    /// The hover tooltip due this frame, if any: a lane's labels, or the full
    /// path of a file row whose name is ellipsized. Text, the row's box in the
    /// window and where to put it relative to that.
    fn tooltip(&self, window: &Window, cx: &App) -> Option<Floating> {
        let cursor = self.cursor_position?;
        let props = &self.props;
        let style = &props.style;
        let bounds = self.bounds;

        // Lane tooltip wins over the file path tooltip when both could fire at
        // the same time: the cursor is in the gutter, which is mutually
        // exclusive with the content area anyway.
        let (text, anchor_x, row_y, row_height) =
            if let Some((item_idx, lane, _half)) = self.hovered_lane {
                if item_idx >= props.row_count() {
                    return None;
                }
                let item = props.item_at(item_idx, cx)?;
                let labels = item_lane_labels(&item, lane);
                if labels.is_empty() {
                    return None;
                }
                (
                    labels.join(", "),
                    // Anchor just past the gutter so the tip sits next to the
                    // stroke rather than at the far right edge.
                    bounds.x + item_gutter_width(&item),
                    self.row_screen_y(item_idx),
                    row_height_of(props.row_kind(item_idx)),
                )
            } else {
                let item_idx = self.hovered_file_item?;
                if item_idx >= props.row_count() {
                    return None;
                }
                let item = props.item_at(item_idx, cx)?;
                let gutter_total = item_gutter_width(&item);
                let Item::File(file) = item else {
                    return None;
                };
                // Only show the path tooltip when the name is actually
                // ellipsized: same column math as painting it.
                let path_w = props
                    .file_row_columns(bounds.x, bounds.width, gutter_total, &file)
                    .path_w;
                let name_width =
                    self.widths
                        .width(&file.primary, FILE_NAME_TEXT_SIZE, style.primary_font);
                if name_width <= path_w {
                    return None;
                }
                (
                    file.raw_path,
                    bounds.x + bounds.width,
                    self.row_screen_y(item_idx),
                    FILE_ROW_HEIGHT,
                )
            };

        let viewport = window.viewport_size();
        let text_w = self
            .widths
            .width(&text, CAPTION_TEXT_SIZE, style.primary_font);
        let box_w = text_w + TOOLTIP_PADDING_X * 2.0 + 2.0;
        let box_h = CAPTION_TEXT_SIZE * LINE_HEIGHT_MULTIPLIER + TOOLTIP_PADDING_Y * 2.0;
        // Anchor to the right of the row at its vertical center, shifted by the
        // gap. Snap into the viewport if it would clip.
        let mut x = anchor_x + TOOLTIP_GAP;
        let mut y = row_y + (row_height - box_h) / 2.0;
        if x + box_w > f32::from(viewport.width) {
            // Fall back to following the cursor on the left.
            x = (cursor.x - box_w - TOOLTIP_GAP).max(4.0);
        }
        if y + box_h > f32::from(viewport.height) {
            y = (f32::from(viewport.height) - box_h - 4.0).max(0.0);
        }
        if y < 0.0 {
            y = 4.0;
        }
        Some((
            div()
                .w(px(box_w))
                .h(px(box_h))
                .flex()
                .items_center()
                .px(px(TOOLTIP_PADDING_X))
                .bg(style.tooltip_background)
                .border_1()
                .border_color(style.tooltip_border)
                .rounded(px(TOOLTIP_RADIUS))
                // Same soft lift every other floating surface carries: shadowless,
                // the card sank into the busy rows behind it.
                .shadow(vec![crate::theme::floating_shadow(3.0, 10.0)])
                .font(style.primary_font.gpui())
                .text_size(px(CAPTION_TEXT_SIZE))
                .line_height(relative(LINE_HEIGHT_MULTIPLIER))
                .text_color(style.tooltip_text)
                .whitespace_nowrap()
                .child(text)
                .into_any_element(),
            point(px(x), px(y)),
        ))
    }

    /// The chip trailing the cursor during a drag: the dragged revision's
    /// change id, or the size of the marked batch it carries.
    fn drag_ghost(&self, cx: &App) -> Option<Floating> {
        let drag = self.drag.filter(|drag| drag.active)?;
        let props = &self.props;
        let style = &props.style;
        let source_flat = props.commit_flat(drag.source_commit);
        if source_flat >= props.row_count() {
            return None;
        }
        let Some(Item::Revision(rev)) = props.item_at(source_flat, cx) else {
            return None;
        };
        let label = if drag.batch_count > 1 {
            format!("{} revisions", drag.batch_count)
        } else {
            format!("{}{}", rev.change_id_prefix, rev.change_id_suffix)
        };
        Some((
            div()
                .px(px(8.0))
                .py(px(4.0))
                .bg(style.tooltip_background)
                .border_1()
                .border_color(Color {
                    a: 0.6,
                    ..style.accent
                })
                .rounded(px(crate::theme::radius::CONTROL))
                .shadow(vec![crate::theme::floating_shadow(3.0, 10.0)])
                .font(style.mono_font.gpui())
                .text_size(px(CAPTION_TEXT_SIZE))
                .text_color(style.accent)
                .whitespace_nowrap()
                .child(label)
                .into_any_element(),
            point(px(drag.cursor.x + 14.0), px(drag.cursor.y + 16.0)),
        ))
    }

    fn paint(&self, window: &mut Window, cx: &mut Context<Self>) {
        PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let bounds = self.bounds;
        let props = self.props.clone();
        let style = &props.style;
        window.paint_quad(fill(bounds.bounds(), style.background));

        // Resolve the hovered (row, lane, half) -> (segment id, lane) once so
        // each drawn row can do a single segment lookup instead of re-resolving
        // the hover row's segment. The half matters at split lanes: the same
        // lane index at the hover row may carry two different segment ids in
        // its top vs bottom half.
        let emphasized_segment = self.hovered_lane.and_then(|(row, lane, half)| {
            if row >= props.row_count() {
                return None;
            }
            let item = props.item_at(row, cx)?;
            item_lane_segment(&item, lane, half).map(|segment| (segment, lane))
        });

        // Indent-guide emphasis: hovering a tree row brightens the guide of its
        // parent level across the contiguous sibling run (rows at the same or
        // deeper indent around it), VSCode's "active indent guide". Resolved
        // once here as (level, first flat, last flat).
        let guide_emphasis = self.hovered_file_item.and_then(|hovered| {
            // The walk is bounded: emphasis only matters for drawn rows, and a
            // run longer than this already spans several screens.
            const RUN_SCAN_CAP: usize = 400;
            if hovered >= props.row_count() {
                return None;
            }
            let depth_of = |flat: usize| match props.item_at(flat, cx) {
                Some(Item::File(row)) => Some((row.indent / FILE_TREE_INDENT).round() as usize),
                _ => None,
            };
            let depth = depth_of(hovered)?;
            let level = depth.checked_sub(1)?;
            let mut start = hovered;
            while start > 0
                && hovered - start < RUN_SCAN_CAP
                && depth_of(start - 1).is_some_and(|d| d >= depth)
            {
                start -= 1;
            }
            let mut end = hovered;
            while end + 1 < props.row_count()
                && end - hovered < RUN_SCAN_CAP
                && depth_of(end + 1).is_some_and(|d| d >= depth)
            {
                end += 1;
            }
            Some((level, start, end))
        });

        let mask = gpui_kit::ContentMask {
            bounds: bounds.bounds(),
        };
        window.with_content_mask(Some(mask), |window| {
            let mut graph = GraphFrame::default();
            let mut markers: Vec<(Rectangle, f32, DraftMarker)> = Vec::new();

            // Walk only the rows that intersect the viewport, materializing
            // each one's view on demand: off-screen rows cost nothing.
            let count = props.row_count();
            let visible_top = self.vertical_offset;
            let visible_bottom = visible_top + f64::from(bounds.height);
            let scale = window.scale_factor();
            let mut flat = props.row_at_offset(visible_top).unwrap_or(0);
            let mut row_top_local = props.row_top(flat);
            while flat < count && row_top_local < visible_bottom {
                let row_h = row_height_of(props.row_kind(flat));
                // Whole device pixels: quads and glyphs snap on their own, paths
                // don't, and a fractional trackpad offset would otherwise shear
                // the discs off their lines.
                let screen_y = bounds.y + (row_top_local - visible_top) as f32;
                let screen_y = (screen_y * scale).round() / scale;
                let row = Rectangle {
                    x: bounds.x,
                    y: screen_y,
                    width: bounds.width,
                    height: row_h,
                };
                if let Some(item) = props.item_at(flat, cx) {
                    let gutter_total = item_gutter_width(&item);
                    let emphasized = |half| {
                        emphasized_segment.and_then(|(segment, lane)| {
                            let here = item_lane_segment(&item, lane, half)?;
                            (here == segment).then_some(lane)
                        })
                    };
                    let before = emphasized(LaneHalf::Before);
                    let after = emphasized(LaneHalf::After);
                    match &item {
                        Item::Revision(rev) => {
                            self.paint_revision(
                                row,
                                rev,
                                gutter_total,
                                before,
                                after,
                                self.hovered_revision_item == Some(flat),
                                &mut graph,
                                window,
                                cx,
                            );
                            if let Some(marker) = rev.draft_marker {
                                markers.push((row, gutter_total, marker));
                            }
                        }
                        Item::File(file) => {
                            // File rows are continuation-only: they sit under
                            // the parent revision's `after` snapshot, so the
                            // after-half emphasis is what applies.
                            let guide = guide_emphasis
                                .filter(|&(_, start, end)| flat >= start && flat <= end)
                                .map(|(level, ..)| level);
                            self.paint_file(
                                row,
                                file,
                                gutter_total,
                                after,
                                guide,
                                self.hovered_file_item == Some(flat),
                                &mut graph,
                                window,
                                cx,
                            );
                        }
                    }
                }
                row_top_local += f64::from(row_h);
                flat += 1;
            }

            // The graph sits on top of any row chrome.
            graph.paint(window);

            // Target-mode destination decorations, on top of everything: the
            // ring (lands as a child of this row) or the insertion line
            // above/below (insert-after / insert-before).
            for (row, gutter_total, marker) in markers {
                match marker {
                    DraftMarker::Ring => stroke_rounded(
                        props.card_bounds(row),
                        style.accent,
                        1.5,
                        ROW_CARD_RADIUS,
                        window,
                    ),
                    DraftMarker::LineAbove => {
                        paint_insertion_line(row, gutter_total, true, style.accent, window)
                    }
                    DraftMarker::LineBelow => {
                        paint_insertion_line(row, gutter_total, false, style.accent, window)
                    }
                }
            }

            // Live drag-to-rebase indicator: the drop decoration under the
            // cursor. The ghost chip is deferred: it follows the cursor out of
            // the list.
            if let Some(drag) = self.drag.filter(|drag| drag.active) {
                let row_rect = |commit: usize| -> Option<Rectangle> {
                    let flat = props.commit_flat(commit);
                    (flat < props.row_count()).then(|| Rectangle {
                        x: bounds.x,
                        y: self.row_screen_y(flat),
                        width: bounds.width,
                        height: REVISION_ROW_HEIGHT,
                    })
                };
                match props.drop_indicator.unwrap_or(drag.spot) {
                    Some(DropSpot::OnRow(commit)) => {
                        if let Some(rect) = row_rect(commit) {
                            stroke_rounded(
                                props.card_bounds(rect),
                                style.accent,
                                1.5,
                                ROW_CARD_RADIUS,
                                window,
                            );
                        }
                    }
                    Some(DropSpot::Gap { below, .. }) => {
                        // The gap line sits on the *top* edge of the parent-side
                        // (lower) row.
                        if let Some(rect) = row_rect(below)
                            && let Some(item) = props.item_at(props.commit_flat(below), cx)
                        {
                            paint_insertion_line(
                                rect,
                                item_gutter_width(&item),
                                true,
                                style.accent,
                                window,
                            );
                        }
                    }
                    None => {}
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_revision(
        &self,
        row: Rectangle,
        rev: &RevisionRowView,
        gutter_total: f32,
        emphasized_lane_before: Option<usize>,
        emphasized_lane_after: Option<usize>,
        hovered: bool,
        graph: &mut GraphFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let props = &self.props;
        let style = &props.style;
        let widths = &self.widths;
        let selected = !rev.inert && props.selected_row.as_ref() == Some(&rev.selection_key);
        let hovered = hovered && !rev.inert;
        let is_working_copy = matches!(rev.selection_key, RowSelectionKey::WorkingCopy);

        // Row chrome is an inset rounded card rather than an edge-to-edge
        // band, so selection/hover read as objects sitting in the pane
        // (rows separate by whitespace: there is no per-row hairline).
        let card = props.card_bounds(row);
        // Working-copy rows get a left-to-right gradient with a fixed light
        // accent on the left fading into the row's base color on the right
        // (panel background normally, the selected gray when selected). The
        // left tint is pre-mixed opaque so the orange reads identically whether
        // or not the row is selected.
        if is_working_copy {
            let right = if selected {
                style.selected_background
            } else {
                style.background
            };
            let left = crate::theme::mix(style.background, style.accent, 0.20);
            window.paint_quad(
                fill(
                    card.bounds(),
                    linear_gradient(
                        90.0,
                        linear_color_stop(left, 0.0),
                        linear_color_stop(right, 1.0),
                    ),
                )
                .corner_radii(px(ROW_CARD_RADIUS)),
            );
        } else if selected {
            fill_rounded(card, style.selected_background, ROW_CARD_RADIUS, window);
        } else if hovered {
            fill_rounded(
                card,
                Color {
                    a: 0.06,
                    ..style.muted_text
                },
                ROW_CARD_RADIUS,
                window,
            );
        }
        // Marked rows, a draft's sources or the multi-selection, wear an accent
        // wash + hairline so they read as "part of the pending set".
        if rev.draft_source || rev.multi_selected {
            fill_rounded(
                card,
                Color {
                    a: 0.08,
                    ..style.accent
                },
                ROW_CARD_RADIUS,
                window,
            );
            stroke_rounded(
                card,
                Color {
                    a: 0.45,
                    ..style.accent
                },
                1.0,
                ROW_CARD_RADIUS,
                window,
            );
        }

        let content_left = row.x + gutter_total;
        let content_right_pad = CONTENT_PADDING;
        let content_width = (row.width - gutter_total - content_right_pad).max(1.0);

        // Two-line stack: human-readable description first, identifiers and
        // status second. History is scanned by intent; ids remain close by for
        // disambiguation and copy workflows.
        // Sizes use cap-height for stack math rather than the rendered
        // line-box so the gap stays visually tight.
        let id_size = CAPTION_TEXT_SIZE;
        let desc_size = SMALL_TEXT_SIZE;
        let stack_height = id_size + ROW_LINE_GAP + desc_size;
        let stack_top = row.y + ((row.height - stack_height) / 2.0).max(0.0);
        let desc_mid_y = stack_top + desc_size / 2.0;
        let title_mid_y = stack_top + desc_size + ROW_LINE_GAP + id_size / 2.0;

        let mono = style.mono_font;
        let primary = style.primary_font;
        let prefix_w = widths.width(&rev.change_id_prefix, id_size, mono);
        let suffix_w = widths.width(&rev.change_id_suffix, id_size, mono);
        let commit_w = widths.width(&rev.commit_id_short, id_size, mono);

        // === Measurements ===
        let id_gap = 8.0;
        let chip_gap = 6.0;
        let at_marker = is_working_copy;
        let at_gap = 4.0;
        let at_w = if at_marker {
            widths.width("@", id_size, mono)
        } else {
            0.0
        };
        let author_full_w = widths.width(&rev.author, id_size, primary);
        let ellipsis_w = widths.width("…", id_size, mono);

        let bm_widths: Vec<f32> = rev
            .bookmark_chips
            .iter()
            .map(|c| chip::width_memoized(widths, &c.label, c.icon, c.font))
            .collect();
        let status_widths: Vec<f32> = rev
            .status_chips
            .iter()
            .map(|c| chip::width_memoized(widths, &c.label, c.icon, c.font))
            .collect();
        let status_total: f32 = status_widths.iter().sum::<f32>()
            + chip_gap * status_widths.len().saturating_sub(1) as f32;

        // === Layout budget ===
        let content_right = row.x + row.width - content_right_pad;
        let total_width = (content_right - content_left).max(0.0);
        let layout = rail_layout(
            RailInput {
                total_width,
                mandatory_left: at_w + (if at_marker { at_gap } else { 0.0 }) + prefix_w + suffix_w,
                status_total,
                has_status: !status_widths.is_empty(),
                commit_w,
                has_commit_id: !rev.commit_id_short.is_empty(),
                author_full_w,
                has_author: !rev.author.is_empty(),
                ellipsis_w,
                id_gap,
                chip_gap,
            },
            &bm_widths,
            |overflow| chip::width_memoized(widths, &format!("+{overflow}"), None, primary),
        );

        // === Color selection ===
        // A ghost's text recedes; its lanes (other branches passing through)
        // stay at full strength.
        let fade = |color: Color| {
            if rev.ghost {
                Color {
                    a: color.a * GHOST_TEXT_ALPHA,
                    ..color
                }
            } else {
                color
            }
        };

        // === Left-side flowing content ===
        // One baseline for the whole meta line: the ids' caps are centred on
        // the line, and the author (another font) and the chips join them on
        // that baseline rather than centring their own boxes.
        let meta_baseline = TextBox::new(mono, id_size, id_size * LINE_HEIGHT_MULTIPLIER)
            .baseline_for_center(title_mid_y);
        let on_meta_line = |font: Font, size: f32| {
            TextBox::new(font, size, size * LINE_HEIGHT_MULTIPLIER)
                .center_for_baseline(meta_baseline)
        };
        let mut x_cursor = content_left;
        if at_marker {
            paint_centered(
                "@",
                x_cursor,
                title_mid_y,
                id_size,
                mono,
                fade(style.accent),
                window,
                cx,
            );
            x_cursor += at_w + at_gap;
        }
        paint_centered(
            &rev.change_id_prefix,
            x_cursor,
            title_mid_y,
            id_size,
            mono,
            fade(style.accent_text),
            window,
            cx,
        );
        x_cursor += prefix_w;
        paint_centered(
            &rev.change_id_suffix,
            x_cursor,
            title_mid_y,
            id_size,
            mono,
            fade(style.subtle_text),
            window,
            cx,
        );
        x_cursor += suffix_w;

        if layout.show_commit_id {
            x_cursor += id_gap;
            paint_centered(
                &rev.commit_id_short,
                x_cursor,
                title_mid_y,
                id_size,
                mono,
                fade(style.muted_text),
                window,
                cx,
            );
            x_cursor += commit_w;
        }

        if layout.author_width > 0.0 {
            x_cursor += id_gap;
            let author = ellipsis::cut_end(&rev.author, layout.author_width, id_size, primary);
            paint_centered(
                &author,
                x_cursor,
                on_meta_line(primary, id_size),
                id_size,
                primary,
                fade(style.subtle_text),
                window,
                cx,
            );
        } else if layout.need_ellipsis {
            // Single `…` after the last left-side glyph signals that
            // commit_id and/or author got hidden for lack of room.
            x_cursor += id_gap;
            paint_centered(
                "…",
                x_cursor,
                title_mid_y,
                id_size,
                mono,
                fade(style.subtle_text),
                window,
                cx,
            );
        }

        // === Right-anchored chip rail ===
        // Compose the rail left-to-right starting from its left edge: any
        // bookmarks, then the `+N` overflow chip (if any), then status
        // chips. The leading edge is computed from the cumulative width.
        let mut chip_x = content_right - layout.rail_width(&bm_widths, status_total);
        for (c, width) in rev
            .bookmark_chips
            .iter()
            .zip(&bm_widths)
            .take(layout.visible_bookmarks)
        {
            let center = chip::center_for_baseline(c, meta_baseline);
            chip::paint(c, widths, chip_x, center, window, cx);
            chip_x += width + chip_gap;
        }
        if layout.overflow_count > 0 {
            let overflow_chip = Chip {
                label: format!("+{}", layout.overflow_count),
                font: primary,
                background: chip_background(rev.lane_color),
                text_color: rev.lane_color,
                border_color: None,
                border_dashed: false,
                icon: None,
            };
            let center = chip::center_for_baseline(&overflow_chip, meta_baseline);
            chip::paint(&overflow_chip, widths, chip_x, center, window, cx);
            chip_x += layout.overflow_width + chip_gap;
        }
        for (c, width) in rev.status_chips.iter().zip(&status_widths) {
            let center = chip::center_for_baseline(c, meta_baseline);
            chip::paint(c, widths, chip_x, center, window, cx);
            chip_x += width + chip_gap;
        }

        // Reserve room on the right of the description for the collapse/
        // expand chevron when this is the selected row. The chevron sizes
        // up past the description so the affordance reads at a glance.
        let chevron_glyph = rev.collapse_chevron.map(|expanded| {
            if expanded {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            }
        });
        let chevron_width = chevron_glyph.map_or(0.0, |glyph| {
            widths.width(glyph, CHEVRON_TEXT_SIZE, icons::ICON_FONT)
        });
        let chevron_gap = if chevron_glyph.is_some() { 6.0 } else { 0.0 };
        let description_width = (content_width - chevron_width - chevron_gap).max(1.0);

        let description =
            ellipsis::cut_end(&rev.description, description_width, desc_size, primary);
        paint_centered(
            &description,
            content_left,
            desc_mid_y,
            desc_size,
            primary,
            fade(rev.description_color),
            window,
            cx,
        );

        if let Some(glyph) = chevron_glyph {
            paint_centered(
                glyph,
                row.x + row.width - content_right_pad - chevron_width,
                desc_mid_y,
                CHEVRON_TEXT_SIZE,
                icons::ICON_FONT,
                style.subtle_text,
                window,
                cx,
            );
        }

        // Working-copy node sits on the gradient's faded right side, so the
        // saturated accent reads cleanly without a contrast swap.
        graph_view::revision_row(
            graph,
            row.x + GUTTER_LEFT_PADDING,
            row.y,
            row.height,
            &graph_view::RevisionRow {
                frame_data: &rev.frame,
                columns: &rev.columns,
                prev_columns: &rev.prev_columns,
                node_color_override: is_working_copy.then_some(style.accent),
                hollow_node: rev.ghost.then_some(style.background),
                emphasized_lane_before,
                emphasized_lane_after,
            },
            &style.graph,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_file(
        &self,
        row: Rectangle,
        file: &FileRowView,
        gutter_total: f32,
        emphasized_lane: Option<usize>,
        emphasized_guide: Option<usize>,
        hovered: bool,
        graph: &mut GraphFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let props = &self.props;
        let style = &props.style;
        let widths = &self.widths;
        let selected = props.selected_file == Some(file.file_index);

        // Same inset rounded card as revision rows, so every selectable row
        // shares one selection language.
        let card = props.card_bounds(row);
        if selected {
            fill_rounded(card, style.selected_background, ROW_CARD_RADIUS, window);
        } else if hovered {
            // Soft hover wash, weaker than selection so the two never compete
            // when the cursor crosses the selected row.
            fill_rounded(
                card,
                Color {
                    a: 0.06,
                    ..style.muted_text
                },
                ROW_CARD_RADIUS,
                window,
            );
        }
        // Deliberately no per-row separator: the indent guides supply the
        // vertical structure, and both line sets together read as a grid.

        let content_x = row.x + gutter_total + file.indent;
        let row_mid_y = row.y + row.height / 2.0;

        // Indent guides: one hairline per ancestor level, aligned under the
        // parent rows' chevron column. The hovered row's sibling run wears a
        // brighter guide at its parent level.
        let depth = (file.indent / FILE_TREE_INDENT).round() as usize;
        for level in 0..depth {
            let x = row.x + gutter_total + level as f32 * FILE_TREE_INDENT + FILE_CHEVRON_COL / 2.0;
            let color = if emphasized_guide == Some(level) {
                Color {
                    a: 0.8,
                    ..style.subtle_text
                }
            } else {
                style.border
            };
            window.paint_quad(fill(
                Rectangle {
                    x: x.round(),
                    y: row.y,
                    width: 1.0,
                    height: row.height,
                }
                .bounds(),
                color,
            ));
        }

        // Left columns: [chevron (dirs)] [file/folder icon] name…
        if let Some(collapsed) = file.chevron {
            let glyph = if collapsed {
                icons::CHEVRON_RIGHT
            } else {
                icons::CHEVRON_DOWN
            };
            paint_glyph(
                glyph,
                content_x + FILE_CHEVRON_COL / 2.0,
                row_mid_y,
                CAPTION_TEXT_SIZE,
                file.primary_color,
                window,
                cx,
            );
        }
        let icon_center = content_x + FILE_CHEVRON_COL + FILE_ICON_COL / 2.0;
        if let Some(check) = file.check {
            paint_file_check(icon_center, row_mid_y, check, style, window, cx);
        } else {
            let icon_glyph = match file.chevron {
                Some(true) => icons::FOLDER,
                Some(false) => icons::FOLDER_OPEN,
                None => icons::FILE,
            };
            paint_glyph(
                icon_glyph,
                icon_center,
                row_mid_y,
                FILE_ICON_SIZE,
                file.icon_color,
                window,
                cx,
            );
        }

        // Layout: [indent][chevron][icon] name … [+N] [-N] [status chip] pad.
        // The status chip is the rightmost column so badges align down the
        // tree; directory rows have neither stats nor chip, so their name runs
        // to the right edge. The chip column clears the scrollbar band with the
        // same right padding the revision rows' bookmark rail uses.
        let columns = props.file_row_columns(row.x, row.width, gutter_total, file);
        let chip_col = style.file_badge_width;
        let primary = style.primary_font;

        // Cut in the middle, so a name wider than `path_w` keeps both its start
        // and its extension (and a compacted `a/b/c` chain its leaf) without
        // bleeding into the +N / -N columns; the full path is on the hover
        // tooltip. The name's caps are centred on the row; the stats and the
        // status chip (other sizes and fonts) join it on its baseline.
        let name_baseline = TextBox::new(
            primary,
            FILE_NAME_TEXT_SIZE,
            FILE_NAME_TEXT_SIZE * LINE_HEIGHT_MULTIPLIER,
        )
        .baseline_for_center(row_mid_y);
        let stats_center = TextBox::new(
            primary,
            CAPTION_TEXT_SIZE,
            CAPTION_TEXT_SIZE * LINE_HEIGHT_MULTIPLIER,
        )
        .center_for_baseline(name_baseline);
        let name =
            ellipsis::cut_middle(&file.primary, columns.path_w, FILE_NAME_TEXT_SIZE, primary);
        // Too narrow for even the ellipsis: nothing, rather than a mark over the stats.
        if crate::measure::line_width(&name, FILE_NAME_TEXT_SIZE, primary) <= columns.path_w + 1.0 {
            paint_centered(
                &name,
                columns.path_x,
                row_mid_y,
                FILE_NAME_TEXT_SIZE,
                primary,
                file.primary_color,
                window,
                cx,
            );
        }

        // Status chip, right-aligned in the rightmost column (single-letter
        // labels center within it so mixed labels still line up). An empty
        // label draws nothing.
        if file.chevron.is_none() && !file.status_label.is_empty() {
            let status_chip = Chip {
                label: file.status_label.clone(),
                font: style.mono_font,
                background: file.status_background,
                text_color: file.status_text,
                border_color: None,
                border_dashed: false,
                icon: None,
            };
            let chip_w = chip::width_memoized(widths, &file.status_label, None, style.mono_font);
            chip::paint(
                &status_chip,
                widths,
                columns.chip_left + ((chip_col - chip_w) / 2.0).max(0.0),
                chip::center_for_baseline(&status_chip, name_baseline),
                window,
                cx,
            );
        }

        // Directory rows carry no stats; zero-width stat columns (the source
        // tree) skip them too.
        if file.chevron.is_none() && file.additions_width > 0.0 {
            // Right-aligned, so the digits of rows stacked above each other
            // line up like a table column. The anchor is the right edge.
            let stat =
                |text: String, right: f32, color: Color, window: &mut Window, cx: &mut App| {
                    let line_height = CAPTION_TEXT_SIZE * LINE_HEIGHT_MULTIPLIER;
                    let shift = TextBox::new(primary, CAPTION_TEXT_SIZE, line_height).trim_shift();
                    typography::paint_line(
                        &text,
                        right,
                        stats_center + shift - line_height / 2.0,
                        CAPTION_TEXT_SIZE,
                        line_height,
                        primary,
                        color,
                        TextAlign::Right,
                        window,
                        cx,
                    );
                };
            stat(
                format!("+{}", file.additions),
                columns.plus_x + file.additions_width,
                file.additions_text,
                window,
                cx,
            );
            stat(
                format!("\u{2212}{}", file.deletions),
                columns.minus_x + file.deletions_width,
                file.deletions_text,
                window,
                cx,
            );
        }

        graph_view::continuation_row(
            graph,
            row.x + GUTTER_LEFT_PADDING,
            row.y,
            row.height,
            &file.continuation,
            &file.columns,
            &style.graph,
            emphasized_lane,
        );
    }
}

/// A list before its first props arrive: nothing draws with it, since
/// [`view`] sets the real ones in the same breath.
fn placeholder_style() -> RevisionListStyle {
    let color = crate::theme::BLACK;
    RevisionListStyle {
        graph: RevisionGraphStyle {
            lane_base_color: color,
            missing_color: color,
        },
        background: color,
        selected_background: color,
        accent: color,
        control: color,
        border: color,
        muted_text: color,
        subtle_text: color,
        accent_text: color,
        primary_font: Font::DEFAULT,
        mono_font: Font::MONOSPACE,
        file_badge_width: 0.0,
        tooltip_background: color,
        tooltip_text: color,
        tooltip_border: color,
    }
}

/// The widths a revision row's meta line divides up.
struct RailInput {
    total_width: f32,
    /// `@`, its gap and the change id.
    mandatory_left: f32,
    status_total: f32,
    has_status: bool,
    commit_w: f32,
    has_commit_id: bool,
    author_full_w: f32,
    has_author: bool,
    ellipsis_w: f32,
    id_gap: f32,
    chip_gap: f32,
}

/// What of a revision row's meta line fits.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RailLayout {
    show_commit_id: bool,
    visible_bookmarks: usize,
    overflow_count: usize,
    overflow_width: f32,
    /// How wide the author draws, 0 when it's dropped.
    author_width: f32,
    need_ellipsis: bool,
    chip_gap: f32,
}

impl RailLayout {
    fn rail_width(&self, bookmarks: &[f32], status_total: f32) -> f32 {
        let mut width = 0.0;
        let mut add = |piece: f32| {
            if width > 0.0 {
                width += self.chip_gap;
            }
            width += piece;
        };
        for bookmark in bookmarks.iter().take(self.visible_bookmarks) {
            add(*bookmark);
        }
        if self.overflow_count > 0 {
            add(self.overflow_width);
        }
        if status_total > 0.0 {
            add(status_total);
        }
        width
    }
}

/// Priority when the row is short of room: commit id, then bookmark chips (with
/// a `+N` chip for the rest), then the author. Status chips and the change id
/// are load-bearing and never compress.
fn rail_layout(
    input: RailInput,
    bookmarks: &[f32],
    overflow_width: impl Fn(usize) -> f32,
) -> RailLayout {
    let RailInput {
        total_width,
        mandatory_left,
        status_total,
        has_status,
        commit_w,
        has_commit_id,
        author_full_w,
        has_author,
        ellipsis_w,
        id_gap,
        chip_gap,
    } = input;
    // Always-shown widths consume from the budget first.
    let mandatory = mandatory_left + status_total + if has_status { chip_gap } else { 0.0 };
    let mut budget = (total_width - mandatory).max(0.0);

    // 1. commit_id (with leading id_gap).
    let mut show_commit_id = has_commit_id && commit_w + id_gap <= budget;
    if show_commit_id {
        budget -= commit_w + id_gap;
    }

    // 2. Bookmarks: fit greedily, reserving space for a `+N` overflow chip
    //    when not all fit. The chip rail's leading gap separates it from the
    //    left-side text.
    let mut visible_bookmarks = 0;
    let mut overflow_count = 0;
    let mut overflow_w = 0.0;
    if !bookmarks.is_empty() {
        let all_w: f32 =
            bookmarks.iter().sum::<f32>() + chip_gap * bookmarks.len().saturating_sub(1) as f32;
        let block_w = if chip_gap + all_w <= budget {
            visible_bookmarks = bookmarks.len();
            all_w
        } else {
            let mut acc = 0.0;
            for (index, width) in bookmarks.iter().enumerate() {
                let gap = if index == 0 { 0.0 } else { chip_gap };
                let candidate = acc + gap + width;
                let remaining = bookmarks.len() - index - 1;
                let plus_overflow = if remaining > 0 {
                    chip_gap + overflow_width(remaining)
                } else {
                    0.0
                };
                if chip_gap + candidate + plus_overflow <= budget {
                    visible_bookmarks = index + 1;
                    acc = candidate;
                } else {
                    break;
                }
            }
            if visible_bookmarks < bookmarks.len() {
                overflow_count = bookmarks.len() - visible_bookmarks;
                overflow_w = overflow_width(overflow_count);
            }
            acc + if overflow_count > 0 {
                chip_gap + overflow_w
            } else {
                0.0
            }
        };
        // Subtract the chip rail's leading gap once, plus the block.
        budget -= chip_gap + block_w;
        // Even a lone `+N` may not fit beside the commit id. The chip says more
        // refs exist; the id gives way rather than running under it.
        if budget < 0.0 && show_commit_id {
            show_commit_id = false;
        }
    }

    // 3. Author squeezes into whatever's left (with a leading id_gap).
    let author_max_w = (budget - id_gap).max(0.0);
    let author_width = if has_author && author_max_w >= ellipsis_w {
        author_max_w.min(author_full_w)
    } else {
        0.0
    };

    RailLayout {
        show_commit_id,
        visible_bookmarks,
        overflow_count,
        overflow_width: overflow_w,
        author_width,
        // Ellipsis indicator when commit_id or author got dropped entirely.
        need_ellipsis: (has_commit_id && !show_commit_id) || (author_width == 0.0 && has_author),
        chip_gap,
    }
}

/// Number of display columns a revision row's gutter must reserve. It's not
/// enough to count this row's own live columns: the top half draws each
/// incoming lane (`before`) starting from the column it held in the *previous*
/// row, and the warp may have packed that origin further right than anything in
/// this row. Since the revision id is drawn in that same top half, a strip
/// sized to this row alone lets a lane sliding in from far right run over the
/// id (and leaves those top-half lanes outside the hover hit-region, which also
/// keys off `prev_columns`). Lanes that don't continue into this row aren't
/// drawn here, so dead columns to the right don't count.
fn revision_strip_columns(
    columns: &[Option<usize>],
    prev_columns: &[Option<usize>],
    before: &[Option<GraphEdgeType>],
    fallback: usize,
) -> usize {
    let here = columns.iter().flatten().copied();
    let incoming_origins = before
        .iter()
        .enumerate()
        .filter(|(_, kind)| kind.is_some())
        .filter_map(|(lane, _)| prev_columns.get(lane).copied().flatten());
    here.chain(incoming_origins)
        .max()
        .map_or(fallback, |max| max + 1)
}

/// Original lane index occupying warped `display` column at this item's
/// row, or `None` for an empty column. The top half of a transitioning row
/// reads the previous row's packing (that's where the slide starts).
fn item_display_to_lane(item: &Item, display: usize, half: LaneHalf) -> Option<usize> {
    let columns: &[Option<usize>] = match (item, half) {
        (Item::Revision(row), LaneHalf::Before) if !row.prev_columns.is_empty() => {
            &row.prev_columns
        }
        (Item::Revision(row), _) => &row.columns,
        (Item::File(row), _) => &row.columns,
    };
    columns.iter().position(|column| *column == Some(display))
}

/// Bookmark labels for `lane` at this item's row, or an empty slice
/// when the lane has none. Both revision and file rows carry their own
/// snapshot of the lane-label state: see [`RevisionRowView::lane_labels`]
/// and [`FileRowView::lane_labels`].
fn item_lane_labels(item: &Item, lane: usize) -> &[String] {
    match item {
        Item::Revision(row) => row
            .lane_labels
            .get(lane)
            .map(|v| v.as_slice())
            .unwrap_or(&[]),
        Item::File(row) => row
            .lane_labels
            .get(lane)
            .map(|v| v.as_slice())
            .unwrap_or(&[]),
    }
}

/// Segment id for `lane` at this item's row in the given half. `None`
/// means the lane is dead at this row in that half (no stroke drawn)
/// or the column is out of range. File rows aren't split: both halves
/// query the same `lane_segments`.
fn item_lane_segment(item: &Item, lane: usize, half: LaneHalf) -> Option<usize> {
    let slot = match item {
        Item::Revision(row) => match half {
            LaneHalf::Before => row.lane_segments_before.get(lane),
            LaneHalf::After => row.lane_segments_after.get(lane),
        },
        Item::File(row) => row.lane_segments.get(lane),
    };
    slot.copied().flatten()
}

fn fill_rounded(rect: Rectangle, color: Color, radius: f32, window: &mut Window) {
    window.paint_quad(fill(rect.bounds(), color).corner_radii(px(radius)));
}

/// Border-only rounded rect: the target ring for draft/drag destinations.
fn stroke_rounded(rect: Rectangle, color: Color, width: f32, radius: f32, window: &mut Window) {
    window.paint_quad(quad(
        rect.bounds(),
        px(radius),
        gpui_kit::transparent_black(),
        px(width),
        color,
        BorderStyle::Solid,
    ));
}

/// The insert-here caret: a 2px accent line across a row boundary (top or
/// bottom edge of `row`) with a small dot at its left end, spanning from the
/// graph gutter to the row's right padding.
fn paint_insertion_line(
    row: Rectangle,
    gutter_total: f32,
    at_top: bool,
    color: Color,
    window: &mut Window,
) {
    let y = if at_top {
        row.y - 1.0
    } else {
        row.y + row.height - 1.0
    };
    let x = row.x + gutter_total.min(row.width);
    let width = (row.x + row.width - CONTENT_PADDING - x).max(0.0);
    fill_rounded(
        Rectangle {
            x,
            y,
            width,
            height: 2.0,
        },
        color,
        1.0,
        window,
    );
    fill_rounded(
        Rectangle {
            x: x - 6.0,
            y: y - 2.0,
            width: 6.0,
            height: 6.0,
        },
        color,
        3.0,
        window,
    );
}

/// Text optically centred on `center_y`: its cap box (cap top to baseline), not
/// its line box, is what's centred. See [`crate::typography`]. Runs that must
/// share a baseline with another font or size pass
/// [`TextBox::center_for_baseline`] of that baseline.
#[allow(clippy::too_many_arguments)]
fn paint_centered(
    content: &str,
    x: f32,
    center_y: f32,
    size: f32,
    font: Font,
    color: Color,
    window: &mut Window,
    cx: &mut App,
) {
    let line_height = size * LINE_HEIGHT_MULTIPLIER;
    let shift = TextBox::new(font, size, line_height).trim_shift();
    typography::paint_line(
        content,
        x,
        center_y + shift - line_height / 2.0,
        size,
        line_height,
        font,
        color,
        TextAlign::Left,
        window,
        cx,
    );
}

/// A Lucide glyph centered on `(center_x, center_y)`.
fn paint_glyph(
    glyph: &str,
    center_x: f32,
    center_y: f32,
    size: f32,
    color: Color,
    window: &mut Window,
    cx: &mut App,
) {
    typography::paint_line(
        glyph,
        center_x,
        center_y - size / 2.0,
        size,
        size,
        icons::ICON_FONT,
        color,
        TextAlign::Center,
        window,
        cx,
    );
}

/// A file-tree change-selection checkbox centered at `(center_x, mid_y)`.
fn paint_file_check(
    center_x: f32,
    mid_y: f32,
    check: crate::diff_view::CheckState,
    style: &RevisionListStyle,
    window: &mut Window,
    cx: &mut App,
) {
    use crate::diff_view::CheckState;
    const SIDE: f32 = 12.0;
    let rect = Rectangle {
        x: center_x - SIDE / 2.0,
        y: mid_y - SIDE / 2.0,
        width: SIDE,
        height: SIDE,
    };
    let radius = 3.0;
    let glyph = match check {
        CheckState::Off => {
            stroke_rounded(rect, style.muted_text, 1.0, radius, window);
            return;
        }
        CheckState::Partial => icons::MINUS,
        CheckState::On => icons::CHECK,
    };
    fill_rounded(rect, style.control, radius, window);
    paint_glyph(
        glyph,
        center_x,
        mid_y,
        SIDE - 2.0,
        style.background,
        window,
        cx,
    );
}

struct FileRowColumns {
    chip_left: f32,
    minus_x: f32,
    plus_x: f32,
    path_x: f32,
    path_w: f32,
}

impl Render for RevisionList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(ElementId::NamedInteger(
                "revision-list".into(),
                cx.entity_id().as_u64(),
            ))
            .relative()
            .size_full()
            .child(ListElement { list: cx.entity() })
            .vertical_scrollbar(&self.bar)
    }
}

/// Paints [`RevisionList`]. Fills what it's given.
struct ListElement {
    list: Entity<RevisionList>,
}

impl IntoElement for ListElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ListElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(relative(1.0).into(), relative(1.0).into()),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let (moved, on_scroll, tooltip, ghost) = self.list.update(cx, |list, cx| {
            list.bounds = bounds.into();
            let mut moved = list.settle_offset();
            if moved.is_some() {
                // A jump: the wheel's leftover motion would drag the list off it.
                list.wheel.reset();
            }
            if let Some(offset) = list.bar.take_requested() {
                list.vertical_offset = f64::from(offset).clamp(0.0, list.max_offset());
                list.wheel.reset();
                moved = Some(list.vertical_offset);
            }
            let step = list.wheel.take(Instant::now());
            if step != 0.0 {
                crate::frame_log::value("sy", step);
            }
            if step != 0.0 {
                let before = list.vertical_offset;
                list.vertical_offset = (before + step).clamp(0.0, list.max_offset());
                if list.vertical_offset != before {
                    moved = Some(list.vertical_offset);
                    // Rows slid under a still cursor: its hover moves with them.
                    list.recompute_hover(cx);
                }
            }
            if list.wheel.pending() {
                window.request_animation_frame();
            }
            list.bar.publish(
                list.bounds,
                list.props.content_height() as f32,
                list.vertical_offset as f32,
            );
            (
                moved,
                list.props.on_scroll.clone(),
                list.tooltip(window, cx),
                list.drag_ghost(cx),
            )
        });
        // Outside the draw: the app's handler isn't allowed to run in here.
        if let (Some(offset), Some(on_scroll)) = (moved, on_scroll) {
            window.defer(cx, move |window, cx| on_scroll(&offset, window, cx));
        }
        // Both float over the rest of the window and aren't clipped to the list.
        for (mut floating, at) in [tooltip, ghost].into_iter().flatten() {
            floating.layout_as_root(AvailableSpace::min_size(), window, cx);
            window.defer_draw(floating, at, 1, None);
        }
        hitbox
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let cursor = self.list.update(cx, |list, cx| {
            crate::frame_log::time("list.paint", || list.paint(window, cx));
            if list.drag.is_some_and(|drag| drag.active) {
                CursorStyle::ClosedHand
            } else if scrollbar::hits_container(
                list.bounds,
                window.mouse_position().into(),
                list.props.content_height() as f32,
            ) {
                CursorStyle::Arrow
            } else {
                CursorStyle::PointingHand
            }
        });
        window.set_cursor_style(cursor, hitbox);

        let run = |effects: Effects, window: &mut Window, cx: &mut App| {
            for effect in effects {
                effect(window, cx);
            }
        };

        let list = self.list.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let position: Point = event.position.into();
            let hovered = hit.is_hovered(window);
            let effects = list.update(cx, |list, cx| {
                list.with_scroll_report(
                    |list, effects, cx| list.on_mouse_move(position, hovered, effects, cx),
                    cx,
                )
            });
            run(effects, window, cx);
        });

        let list = self.list.clone();
        window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let effects = list.update(cx, |list, cx| {
                list.with_scroll_report(|list, effects, cx| list.on_mouse_exit(effects, cx), cx)
            });
            run(effects, window, cx);
        });

        let list = self.list.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hit.should_handle_scroll(window) {
                return;
            }
            let delta_y = f32::from(event.delta.pixel_delta(window.line_height()).y);
            crate::frame_log::value("wy", f64::from(delta_y));
            let effects = list.update(cx, |list, cx| {
                list.with_scroll_report(|list, _, cx| list.on_wheel(delta_y, cx), cx)
            });
            cx.stop_propagation();
            run(effects, window, cx);
        });

        let list = self.list.clone();
        let hit = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hit.is_hovered(window) {
                return;
            }
            let cursor: Point = event.position.into();
            let mut handled = false;
            let effects = match event.button {
                MouseButton::Left => list.update(cx, |list, cx| {
                    list.with_scroll_report(
                        |list, effects, cx| handled = list.on_left_press(cursor, effects, cx),
                        cx,
                    )
                }),
                MouseButton::Right => list.update(cx, |list, cx| {
                    list.with_scroll_report(
                        |list, effects, cx| handled = list.on_right_press(cursor, effects, cx),
                        cx,
                    )
                }),
                _ => return,
            };
            if handled {
                if event.button == MouseButton::Left {
                    // Moves and the release reach us wherever the cursor goes.
                    window.capture_pointer(hit.id);
                    // A press on the list takes the keyboard back from a field,
                    // so the keymap sees the next key.
                    window.blur(cx);
                }
                cx.stop_propagation();
            }
            run(effects, window, cx);
        });

        let list = self.list.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let cursor: Point = event.position.into();
            let effects = list.update(cx, |list, cx| {
                list.with_scroll_report(
                    |list, effects, cx| list.on_left_release(cursor, effects, cx),
                    cx,
                )
            });
            run(effects, window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Three commits with the middle one (index 1) expanded over 2 file rows,
    // so the file block starts at flat 2.
    // Flat layout: [rev0, rev1, file0, file1, rev2].
    const EXP: Option<(usize, usize)> = Some((2, 2));

    #[test]
    fn row_count_includes_expanded_files() {
        assert_eq!(rows_total(3, None), 3);
        assert_eq!(rows_total(3, EXP), 5);
    }

    #[test]
    fn row_kind_maps_flat_indices() {
        let kinds: Vec<_> = (0..5).map(|f| row_kind_at(EXP, f)).collect();
        assert!(matches!(kinds[0], RowKind::Revision(0)));
        assert!(matches!(kinds[1], RowKind::Revision(1)));
        assert!(matches!(kinds[2], RowKind::File(0)));
        assert!(matches!(kinds[3], RowKind::File(1)));
        assert!(matches!(kinds[4], RowKind::Revision(2)));
    }

    #[test]
    fn row_top_accumulates_mixed_heights() {
        let rev = f64::from(REVISION_ROW_HEIGHT);
        let file = f64::from(FILE_ROW_HEIGHT);
        assert_eq!(row_top_at(EXP, 0), 0.0);
        assert_eq!(row_top_at(EXP, 1), rev);
        assert_eq!(row_top_at(EXP, 2), 2.0 * rev);
        assert_eq!(row_top_at(EXP, 3), 2.0 * rev + file);
        assert_eq!(row_top_at(EXP, 4), 2.0 * rev + 2.0 * file);
        // content_height equals the top of the one-past-the-last row.
        assert_eq!(rows_content_height(3, EXP), row_top_at(EXP, 5));
    }

    #[test]
    fn row_at_offset_inverts_row_top() {
        for flat in 0..5 {
            let top = row_top_at(EXP, flat);
            let height = f64::from(row_height_of(row_kind_at(EXP, flat)));
            assert_eq!(row_at_offset_in(3, EXP, top), Some(flat), "top of {flat}");
            assert_eq!(
                row_at_offset_in(3, EXP, top + height / 2.0),
                Some(flat),
                "mid of {flat}"
            );
        }
        assert_eq!(
            row_at_offset_in(3, EXP, rows_content_height(3, EXP) + 1.0),
            None
        );
        assert_eq!(row_at_offset_in(3, EXP, -1.0), None);
    }

    // The source sidebar's shape: no revision rows, files from flat 0.
    #[test]
    fn files_only_layout_starts_at_flat_zero() {
        let exp = Some((0, 3));
        let file = f64::from(FILE_ROW_HEIGHT);
        assert_eq!(rows_total(0, exp), 3);
        assert!(matches!(row_kind_at(exp, 0), RowKind::File(0)));
        assert!(matches!(row_kind_at(exp, 2), RowKind::File(2)));
        assert_eq!(row_top_at(exp, 0), 0.0);
        assert_eq!(row_top_at(exp, 2), 2.0 * file);
        assert_eq!(row_at_offset_in(0, exp, 2.0 * file + 1.0), Some(2));
        assert_eq!(row_at_offset_in(0, exp, 3.0 * file + 1.0), None);
    }

    #[test]
    fn unexpanded_layout_is_uniform() {
        let rev = f64::from(REVISION_ROW_HEIGHT);
        assert_eq!(rows_total(10, None), 10);
        assert_eq!(row_top_at(None, 5), 5.0 * rev);
        assert_eq!(row_at_offset_in(10, None, 5.0 * rev + 1.0), Some(5));
        assert_eq!(row_at_offset_in(10, None, 1000.0 * rev), None);
    }

    #[test]
    fn gutter_reserves_for_far_incoming_slant() {
        let direct = Some(GraphEdgeType::Direct);
        // Lanes 0 and 4 survive into this row; 1–3 died, so the warp packs
        // lane 4 from display column 4 (its slot in the row above) down to
        // column 1 here. The incoming slant starts at column 4 in the top
        // half, so the strip must stay 5 columns wide — not the 2 this row's
        // own columns would suggest — or it runs over the revision id.
        let columns = [Some(0), None, None, None, Some(1)];
        let prev_columns = [Some(0), Some(1), Some(2), Some(3), Some(4)];
        let before = [direct, None, None, None, direct];
        assert_eq!(
            revision_strip_columns(&columns, &prev_columns, &before, 5),
            5
        );
    }

    #[test]
    fn gutter_ignores_dead_rightmost_prev_column() {
        let direct = Some(GraphEdgeType::Direct);
        // Lane 2 occupied display column 2 in the row above but doesn't
        // continue here (no `before` edge), so nothing is drawn at column 2 —
        // it must not widen the strip past this row's own two columns.
        let columns = [Some(0), Some(1)];
        let prev_columns = [Some(0), Some(1), Some(2)];
        let before = [direct, direct];
        assert_eq!(
            revision_strip_columns(&columns, &prev_columns, &before, 2),
            2
        );
    }

    #[test]
    fn row_geometry_exact_at_large_indices() {
        // Regression: in `f32`, integers stop being exact past 2^24 ≈ 16.7M,
        // and a ~1M-commit list is ~50M px tall. There, paint's `flat * H` and
        // hit-test's `y / H` rounded to different rows, so a click near the
        // bottom selected the neighbour. `f64` keeps both exact, so the
        // round-trip `row_at_offset(row_top(flat)) == flat` holds everywhere.
        let n = 1_100_000usize;
        let rev = f64::from(REVISION_ROW_HEIGHT);
        for &flat in &[1_000_000usize, 1_086_956, n - 1] {
            let top = row_top_at(None, flat);
            assert_eq!(row_at_offset_in(n, None, top), Some(flat), "top of {flat}");
            assert_eq!(
                row_at_offset_in(n, None, top + rev / 2.0),
                Some(flat),
                "mid of {flat}"
            );
            // Last sub-pixel before the next row still maps to `flat`.
            assert_eq!(
                row_at_offset_in(n, None, top + rev - 0.001),
                Some(flat),
                "end of {flat}"
            );
        }
    }

    #[test]
    fn a_reveal_leaves_a_visible_row_alone_and_centres_a_hidden_one() {
        let row = f64::from(REVISION_ROW_HEIGHT);
        // In view: nothing moves.
        assert_eq!(reveal_offset(100.0, 200.0, row, 400.0, 10_000.0), 100.0);
        // Below the fold: it lands a third of the way down.
        assert_eq!(
            reveal_offset(0.0, 5_000.0, row, 400.0, 10_000.0),
            5_000.0 - 400.0 * 0.33
        );
        // Above: the same, not pinned to the top edge.
        assert_eq!(
            reveal_offset(9_000.0, 5_000.0, row, 400.0, 10_000.0),
            5_000.0 - 400.0 * 0.33
        );
        // Clamped to what the list can scroll to.
        assert_eq!(reveal_offset(0.0, 5_000.0, row, 400.0, 1_000.0), 1_000.0);
    }

    fn rail(total_width: f32, bookmarks: &[f32]) -> RailLayout {
        rail_layout(
            RailInput {
                total_width,
                mandatory_left: 60.0,
                status_total: 40.0,
                has_status: true,
                commit_w: 50.0,
                has_commit_id: true,
                author_full_w: 80.0,
                has_author: true,
                ellipsis_w: 8.0,
                id_gap: 8.0,
                chip_gap: 6.0,
            },
            bookmarks,
            |_| 20.0,
        )
    }

    #[test]
    fn a_short_row_sheds_the_author_then_bookmarks_then_the_commit_id() {
        // Room for everything.
        let wide = rail(400.0, &[50.0, 50.0]);
        assert!(wide.show_commit_id);
        assert_eq!(wide.visible_bookmarks, 2);
        assert_eq!(wide.overflow_count, 0);
        assert_eq!(wide.author_width, 80.0);
        assert!(!wide.need_ellipsis);

        // The author is squeezed first, and ellipsises rather than vanishing.
        let squeezed = rail(300.0, &[50.0, 50.0]);
        assert_eq!(squeezed.visible_bookmarks, 2);
        assert!(squeezed.author_width > 0.0 && squeezed.author_width < 80.0);

        // Then a bookmark goes behind `+N`.
        let overflowed = rail(250.0, &[50.0, 50.0]);
        assert_eq!(overflowed.visible_bookmarks, 1);
        assert_eq!(overflowed.overflow_count, 1);
        assert_eq!(overflowed.author_width, 0.0);
        assert!(overflowed.need_ellipsis);

        // The commit id goes last, and status chips never do.
        let tight = rail(120.0, &[50.0, 50.0]);
        assert!(!tight.show_commit_id);
        assert_eq!(tight.visible_bookmarks, 0);
        assert_eq!(tight.overflow_count, 2);
    }

    #[test]
    fn a_lone_overflow_chip_never_runs_over_the_commit_id() {
        // The id fits on its own, but not with the `+2` beside it.
        let layout = rail(170.0, &[50.0, 50.0]);
        assert_eq!(layout.visible_bookmarks, 0);
        assert_eq!(layout.overflow_count, 2);
        assert!(!layout.show_commit_id);
        assert_eq!(
            layout.author_width, 0.0,
            "the author is lower priority still"
        );
        assert!(layout.need_ellipsis);
    }

    #[test]
    fn the_rail_width_counts_only_what_shows() {
        let layout = rail(250.0, &[50.0, 50.0]);
        // One bookmark, the `+1`, the status block, two gaps.
        assert_eq!(
            layout.rail_width(&[50.0, 50.0], 40.0),
            50.0 + 20.0 + 40.0 + 12.0
        );
        assert_eq!(rail(400.0, &[]).rail_width(&[], 0.0), 0.0);
    }
}
