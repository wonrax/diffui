//! Virtualized sidebar list rendering revisions with their graph gutter.
//!
//! Two reasons it's a single widget rather than a `column!` of buttons:
//!   - Only on-screen rows pay layout / draw cost.
//!   - Revisions and the file rows under the selected revision live in the
//!     same widget, so the graph gutter can run continuously through them.
//!
//! The widget owns its scroll offset in tree state. Click on a row fires
//! the `on_select_revision` / `on_select_file` callbacks the caller
//! supplied. Visual styling (id chip colours, description colour, file
//! status background) is computed by the caller and passed in per row, so
//! this module stays decoupled from the rest of the app's theming logic.

use std::rc::Rc;

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    text::{self, Text},
    widget::{Tree, tree},
};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Point, Radians, Rectangle,
    Shadow, Size, Theme, Vector, alignment, gradient,
};
use jj_lib::graph::GraphEdgeType;

use crate::chip::{self, Chip};
use crate::graph::LaneFrame;
use crate::graph_view::{
    LANE_WIDTH, RevisionGraphStyle, draw_continuation_row, draw_revision_row, lane_strip_width,
};
use crate::icons;
use crate::measure::{self, LINE_HEIGHT_MULTIPLIER};
use crate::scrollbar::{self, ScrollbarState, ScrollbarStyle};
use crate::theme::chip_background;

const LINE_SCROLL_ROWS: f32 = 1.5;
const PIXEL_SCROLL_SCALE: f32 = 0.65;

pub const REVISION_ROW_HEIGHT: f32 = 46.0;
pub const FILE_ROW_HEIGHT: f32 = 26.0;
/// Horizontal indent per tree depth level for file/directory rows.
pub const FILE_TREE_INDENT: f32 = 14.0;
/// Width of the collapse-chevron column in file/directory rows (empty for
/// files). Indent guides center under it.
const FILE_CHEVRON_COL: f32 = 14.0;
/// Width of the file/folder icon column that follows the chevron column.
const FILE_ICON_COL: f32 = 18.0;
/// Glyph size of the file/folder icon.
const FILE_ICON_SIZE: f32 = 12.0;
pub const GUTTER_LEFT_PADDING: f32 = 8.0;
pub const GUTTER_PADDING: f32 = 8.0;
const CONTENT_PADDING: f32 = 12.0;
const SMALL_TEXT_SIZE: f32 = crate::theme::text_size::BODY_LG;
const CAPTION_TEXT_SIZE: f32 = crate::theme::text_size::BODY;
/// Text size for the collapse/expand chevron on the selected revision row. The
/// old `▾` triangle needed an oversize bump to stay legible (it shrank to ~5px
/// of actual ink); the Lucide glyph fills its box, so a near-text size reads
/// clearly as a chevron without dominating the row.
const CHEVRON_TEXT_SIZE: f32 = 15.0;
pub const FILE_ROW_GAP: f32 = 6.0;
pub const FILE_ROW_RIGHT_PAD: f32 = 10.0;
const TOOLTIP_RADIUS: f32 = crate::theme::radius::CONTROL;
const TOOLTIP_PADDING: f32 = 6.0;
const TOOLTIP_GAP: f32 = 8.0;

#[derive(Debug, Clone, Copy)]
pub struct RevisionListStyle {
    pub graph: RevisionGraphStyle,
    pub background: Color,
    pub selected_background: Color,
    /// Saturated accent — drives the left-edge tint of the working-copy
    /// row's gradient and recolors the working-copy node disc in the
    /// graph.
    pub accent: Color,
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
    pub scrollbar: ScrollbarStyle,
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
}

/// Key the widget reports back so the parent can map it to its own
/// selection enum without leaking app types into this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSelectionKey {
    WorkingCopy,
    Commit(String),
}

#[derive(Debug, Clone)]
pub enum Item {
    Revision(RevisionRowView),
    File(FileRowView),
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

/// Virtualized at the *data* level: instead of holding an `Item` per commit
/// (~600MB on a million-commit repo), the widget holds the commit count, the
/// expanded-files span, and closures that materialize a single row's display
/// view on demand. Layout/hit-test use arithmetic; draw builds only the rows
/// that are actually on screen.
pub struct RevisionList<'a, Message> {
    commit_count: usize,
    /// `(commit index of the expanded row, file count)`. File rows render
    /// immediately after that commit. `None` when nothing is expanded.
    expanded: Option<(usize, usize)>,
    build_revision: Box<dyn Fn(usize) -> RevisionRowView + 'a>,
    build_file: Box<dyn Fn(usize) -> FileRowView + 'a>,
    selected_row: Option<RowSelectionKey>,
    selected_file: Option<usize>,
    /// Flat row index of the selected row, for the reveal-on-token scroll.
    selected_flat: Option<usize>,
    style: RevisionListStyle,
    width: Length,
    /// Bumped by the caller each time it wants the selected row scrolled
    /// into view. The widget compares against `State::last_scroll_token`
    /// and triggers a scroll on disagreement — analogous to the
    /// `scroll_token` mechanism in `DiffView` for find matches.
    reveal_token: Option<u64>,
    /// Reveal an explicit file row (keyboard file navigation) instead of the
    /// selected revision: a bumped token plus the file's flat row. Kept separate
    /// from `reveal_token` so scrolling to a file doesn't also re-centre the
    /// selected revision.
    file_reveal_token: Option<u64>,
    reveal_file_flat: Option<usize>,
    on_select_revision: fn(RowSelectionKey) -> Message,
    /// Receives the clicked file row's *display* index into the file tree
    /// (dir rows included), not a document file index.
    on_select_file: fn(usize) -> Message,
    /// Optional right-click handler for a revision row — opens the context
    /// menu. Receives the row's selection key and its on-screen rectangle (in
    /// window-content points), the latter used to anchor a native highlight.
    on_context_menu: Option<fn(RowSelectionKey, Rectangle, Point) -> Message>,
    /// Optional right-click handler for a file row — receives the row's
    /// *display* index into the file tree (like `on_select_file`), its
    /// on-screen rect, and the cursor point.
    on_file_context_menu: Option<fn(usize, Rectangle, Point) -> Message>,
    /// Reports the scroll offset (content-space px) whenever it changes, so the
    /// app can persist it per-tab. `None` ⇒ scroll position isn't tracked.
    on_scroll: Option<fn(f64) -> Message>,
    /// One-shot scroll restore: when `restore_token` differs from the value the
    /// widget last saw, the offset jumps to `restore_offset` (clamped). Used to
    /// re-apply a tab's saved scroll on activation — the widget's `State` is
    /// shared across tabs and would otherwise leak the prior tab's position.
    /// Mirrors the `reveal_token` trigger pattern.
    restore_offset: f64,
    restore_token: u64,
}

impl<'a, Message> RevisionList<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        commit_count: usize,
        expanded: Option<(usize, usize)>,
        build_revision: Box<dyn Fn(usize) -> RevisionRowView + 'a>,
        build_file: Box<dyn Fn(usize) -> FileRowView + 'a>,
        selected_row: Option<RowSelectionKey>,
        selected_file: Option<usize>,
        selected_flat: Option<usize>,
        style: RevisionListStyle,
        on_select_revision: fn(RowSelectionKey) -> Message,
        on_select_file: fn(usize) -> Message,
    ) -> Self {
        Self {
            commit_count,
            expanded,
            build_revision,
            build_file,
            selected_row,
            selected_file,
            selected_flat,
            style,
            width: Length::Fill,
            reveal_token: None,
            file_reveal_token: None,
            reveal_file_flat: None,
            on_select_revision,
            on_select_file,
            on_context_menu: None,
            on_file_context_menu: None,
            on_scroll: None,
            restore_offset: 0.0,
            restore_token: 0,
        }
    }

    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    /// Register a right-click handler for revision rows (the context menu). The
    /// callback gets the row key and its on-screen rect (window-content points).
    pub fn on_context_menu(
        mut self,
        callback: fn(RowSelectionKey, Rectangle, Point) -> Message,
    ) -> Self {
        self.on_context_menu = Some(callback);
        self
    }

    /// Register a right-click handler for file rows. The callback gets the
    /// row's display index into the file tree, its rect, and the cursor.
    pub fn on_file_context_menu(
        mut self,
        callback: fn(usize, Rectangle, Point) -> Message,
    ) -> Self {
        self.on_file_context_menu = Some(callback);
        self
    }

    /// Cause the widget to scroll the currently-selected row into view
    /// the next time `token` changes. Calling with the same value twice
    /// in a row is a no-op.
    pub fn reveal_selected(mut self, token: u64) -> Self {
        self.reveal_token = Some(token);
        self
    }

    /// Like [`Self::reveal_selected`], but scrolls an explicit file row into
    /// view (keyboard file navigation) the next time `token` changes. `flat` is
    /// the file's flat row, or `None` when the file list isn't open — in which
    /// case the token change schedules no scroll.
    pub fn reveal_file(mut self, token: u64, flat: Option<usize>) -> Self {
        self.file_reveal_token = Some(token);
        self.reveal_file_flat = flat;
        self
    }

    /// Report scroll-offset changes (content-space px) so the caller can persist
    /// the position. See [`Self::restore_scroll`] for the inverse.
    pub fn on_scroll(mut self, callback: fn(f64) -> Message) -> Self {
        self.on_scroll = Some(callback);
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
    /// caller asked for, so draw/hit-test pay only for on-screen rows.
    fn item_at(&self, flat: usize) -> Item {
        match self.row_kind(flat) {
            RowKind::Revision(commit) => Item::Revision((self.build_revision)(commit)),
            RowKind::File(file) => Item::File((self.build_file)(file)),
        }
    }

    fn row_height(&self, item: &Item) -> f32 {
        match item {
            Item::Revision(_) => REVISION_ROW_HEIGHT,
            Item::File(_) => FILE_ROW_HEIGHT,
        }
    }

    fn content_height(&self) -> f64 {
        rows_content_height(self.commit_count, self.expanded)
    }

    fn item_gutter_width(&self, item: &Item) -> f32 {
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

    fn row_at_offset(&self, offset: f64) -> Option<usize> {
        row_at_offset_in(self.commit_count, self.expanded, offset)
    }

    fn row_top(&self, flat: usize) -> f64 {
        row_top_at(self.expanded, flat)
    }
}

// Pure row-geometry helpers, split out so they can be unit-tested without
// constructing a full widget. `expanded` is `(commit index of the expanded
// row, file count)`; file rows sit immediately after that commit.

fn rows_total(commit_count: usize, expanded: Option<(usize, usize)>) -> usize {
    commit_count + expanded.map_or(0, |(_, files)| files)
}

fn row_kind_at(expanded: Option<(usize, usize)>, flat: usize) -> RowKind {
    match expanded {
        Some((commit, files)) if flat > commit && flat <= commit + files => {
            RowKind::File(flat - commit - 1)
        }
        Some((commit, files)) if flat > commit + files => RowKind::Revision(flat - files),
        _ => RowKind::Revision(flat),
    }
}

// These work in `f64` content-space pixels deliberately. A ~1M-commit list is
// ~50M px tall, which is past `f32`'s exact-integer ceiling (2^24 ≈ 16.7M), so
// in `f32` the draw path's `flat * H` and the hit-test path's `y / H` rounded
// to different multiples — a click near the bottom selected the neighbouring
// row. `f64` is exact for integers to 2^53 (~9e15), far beyond any real repo,
// so the two paths agree again. Row heights stay `f32` constants (whole px).
fn rows_content_height(commit_count: usize, expanded: Option<(usize, usize)>) -> f64 {
    commit_count as f64 * REVISION_ROW_HEIGHT as f64
        + expanded.map_or(0, |(_, files)| files) as f64 * FILE_ROW_HEIGHT as f64
}

fn row_top_at(expanded: Option<(usize, usize)>, flat: usize) -> f64 {
    let (revisions_before, files_before) = match expanded {
        Some((commit, files)) if flat > commit => {
            let files_before = (flat - commit - 1).min(files);
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
        Some((commit, files)) => {
            let files_top = (commit + 1) as f64 * rev_h;
            let files_bottom = files_top + files as f64 * file_h;
            if offset < files_top {
                (offset / rev_h) as usize
            } else if offset < files_bottom {
                commit + 1 + ((offset - files_top) / file_h) as usize
            } else {
                commit + 1 + files + ((offset - files_bottom) / rev_h) as usize
            }
        }
        None => (offset / rev_h) as usize,
    };
    (flat < rows_total(commit_count, expanded)).then_some(flat)
}

/// Which half of a revision row the cursor is on, used to pick the
/// right segment id when a lane is split (its incoming and outgoing
/// halves carry different segment ids — see
/// [`RevisionRowView::lane_segments_before`]).
///
/// File rows aren't split (their continuation is a single half), so
/// this only matters for `Item::Revision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneHalf {
    /// Top half of the row — the strokes drawn into the disc.
    Before,
    /// Bottom half of the row — the strokes drawn out of the disc.
    After,
}

struct State {
    /// Scroll position in `f64` content-space px (see the row-geometry helpers
    /// for why `f64`). Cast to `f32` only at the scrollbar/render boundary,
    /// where values are viewport-small and `f32` is exact.
    vertical_offset: f64,
    /// Item index of the file row currently hovered (only set for files,
    /// not revisions). Drives the tooltip overlay.
    hovered_file_item: Option<usize>,
    /// Item index + lane index + which half of the row the cursor is on,
    /// when the cursor is over a graph stroke whose lane has bookmark
    /// labels at that row. Drives the branch-name tooltip on the graph
    /// gutter.
    hovered_lane: Option<(usize, usize, LaneHalf)>,
    /// Last cursor position observed inside the widget bounds, in screen
    /// coordinates. Used to anchor the tooltip.
    cursor_position: Option<Point>,
    scrollbar: ScrollbarState,
    /// Most recent `reveal_token` we acted on. `None` until first reveal.
    last_reveal_token: Option<u64>,
    /// Most recent `file_reveal_token` we acted on. `None` until first reveal.
    last_file_reveal_token: Option<u64>,
    /// Row to bring into view on the next `update()` pass — `None` when
    /// no reveal is pending. Set in `diff()` (which sees prop changes
    /// before layout); consumed in `update()` (which has bounds).
    pending_reveal_row: Option<usize>,
    /// Most recent `restore_token` we acted on. A change schedules a one-shot
    /// jump to the caller's `restore_offset`, consumed in `update()` (bounds
    /// clamp). Starts at 0 to match the caller's initial token, so a cold load
    /// doesn't trigger a spurious restore.
    last_restore_token: u64,
    /// Offset to jump to on the next `update()` pass, set when `restore_token`
    /// changed. Consumed (and clamped) once bounds are known.
    pending_set_offset: Option<f64>,
}

impl State {
    fn new() -> Self {
        Self {
            vertical_offset: 0.0,
            hovered_file_item: None,
            hovered_lane: None,
            cursor_position: None,
            scrollbar: ScrollbarState::default(),
            last_reveal_token: None,
            last_file_reveal_token: None,
            pending_reveal_row: None,
            last_restore_token: 0,
            pending_set_offset: None,
        }
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for RevisionList<'a, Message>
where
    Renderer: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::new())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Fill,
        }
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if self.reveal_token != state.last_reveal_token {
            state.last_reveal_token = self.reveal_token;
            // Defer the actual scroll to `update()` — that's where bounds
            // (needed to clamp + center) are available.
            state.pending_reveal_row = self.selected_flat;
        }
        if self.file_reveal_token != state.last_file_reveal_token {
            state.last_file_reveal_token = self.file_reveal_token;
            // A file reveal targets an explicit row; skip when the file list is
            // closed (`None`). Scheduled after the revision reveal so a file
            // selection wins if both somehow change in one render.
            if let Some(flat) = self.reveal_file_flat {
                state.pending_reveal_row = Some(flat);
            }
        }
        if self.restore_token != state.last_restore_token {
            state.last_restore_token = self.restore_token;
            // Applied after the reveal in `update()`, so an exact restore wins
            // over a re-centred selection when both land in the same render.
            state.pending_set_offset = Some(self.restore_offset);
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let on_scroll = self.on_scroll;
        let state = tree.state.downcast_mut::<State>();
        // Offset on entry; compared on the way out so any change this pass
        // (wheel, scrollbar, reveal, restore, clamp) is reported once via
        // `on_scroll`. `fn` pointers are `Copy`, so this borrows nothing.
        let prev_offset = state.vertical_offset;
        let max_vertical = (self.content_height() - bounds.height as f64).max(0.0);
        if state.vertical_offset > max_vertical {
            state.vertical_offset = max_vertical;
            shell.request_redraw();
        }

        if let Some(row_idx) = state.pending_reveal_row.take()
            && row_idx < self.row_count()
        {
            // Scroll so the selected row sits roughly a third of the way
            // down the viewport — gives the user context above and below
            // without pinning to the top edge. Falls back to "just bring
            // it into view" when the viewport is too small to spare a
            // third for offset.
            let row_top = self.row_top(row_idx);
            let row_h = row_height_of(self.row_kind(row_idx)) as f64;
            let view_h = bounds.height as f64;
            let preferred_top = (row_top - view_h * 0.33).max(0.0);
            let must_top = (row_top + row_h - view_h).max(0.0);
            let must_bottom = row_top;
            let target = if (state.vertical_offset..state.vertical_offset + view_h)
                .contains(&row_top)
                && (state.vertical_offset..state.vertical_offset + view_h)
                    .contains(&(row_top + row_h))
            {
                // Already visible — leave the offset alone.
                state.vertical_offset
            } else if state.vertical_offset > must_bottom {
                preferred_top
            } else {
                must_top.max(preferred_top)
            };
            let target = target.clamp(0.0, max_vertical);
            if (target - state.vertical_offset).abs() > f64::EPSILON {
                state.vertical_offset = target;
                shell.request_redraw();
            }
        }

        // Tab restore: jump straight to the saved offset, overriding any reveal
        // scheduled above.
        if let Some(offset) = state.pending_set_offset.take() {
            let target = offset.clamp(0.0, max_vertical);
            if (target - state.vertical_offset).abs() > f64::EPSILON {
                state.vertical_offset = target;
                shell.request_redraw();
            }
        }

        let recompute_hover = |state: &mut State, this: &Self| {
            state.hovered_file_item = None;
            state.hovered_lane = None;
            let Some(pos) = state.cursor_position else {
                return;
            };
            if !bounds.contains(pos) {
                return;
            }
            let local_x = pos.x - bounds.x;
            let local_y = (pos.y - bounds.y) as f64 + state.vertical_offset;
            let Some(row_idx) = this.row_at_offset(local_y) else {
                return;
            };
            let item = this.item_at(row_idx);
            let gutter_total = this.item_gutter_width(&item);
            // Lane hit-test first: a cursor in the lane strip with a
            // labeled lane wins over the content-area file tooltip.
            if local_x >= GUTTER_LEFT_PADDING && local_x < gutter_total - GUTTER_PADDING {
                let strip_x = local_x - GUTTER_LEFT_PADDING;
                let display = (strip_x / LANE_WIDTH).floor() as usize;
                // Pick the half from the cursor's vertical position
                // within the row, so a click on the incoming stub of
                // a split lane resolves to that branch's segment
                // (not the unrelated new branch starting below).
                let row_top = this.row_top(row_idx);
                let row_h = this.row_height(&item);
                let half = if local_y - row_top < f64::from(row_h) / 2.0 {
                    LaneHalf::Before
                } else {
                    LaneHalf::After
                };
                // The cursor x is a *display* column under lane warping;
                // everything downstream (labels, segments) keys on the
                // original lane, so map it back first.
                if let Some(lane) = item_display_to_lane(&item, display, half) {
                    let labels = item_lane_labels(&item, lane);
                    if !labels.is_empty() {
                        state.hovered_lane = Some((row_idx, lane, half));
                        return;
                    }
                }
            }
            if matches!(item, Item::File(_)) {
                state.hovered_file_item = Some(row_idx);
            }
        };

        // The scrollbar is purely visual (thumb position/size), so it stays in
        // `f32`; a few px of thumb imprecision on a 50M-tall list is invisible,
        // and any offset it produces is widened back to `f64` before storage.
        let content_height = self.content_height() as f32;

        match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if scrollbar::is_dragging(&state.scrollbar) {
                    if let scrollbar::ScrollbarEvent::OffsetChanged(new_offset) =
                        scrollbar::on_cursor_moved(
                            &mut state.scrollbar,
                            *position,
                            bounds,
                            content_height,
                        )
                    {
                        state.vertical_offset = (new_offset as f64).clamp(0.0, max_vertical);
                        shell.capture_event();
                        shell.request_redraw();
                    }
                    state.cursor_position = None;
                    let had_hover = state.hovered_file_item.take().is_some()
                        || state.hovered_lane.take().is_some();
                    if had_hover {
                        shell.request_redraw();
                    }
                    if state.vertical_offset != prev_offset
                        && let Some(cb) = on_scroll
                    {
                        shell.publish(cb(state.vertical_offset));
                    }
                    return;
                }
                if bounds.contains(*position) {
                    state.cursor_position = Some(*position);
                } else {
                    state.cursor_position = None;
                }
                let prev_file = state.hovered_file_item;
                let prev_lane = state.hovered_lane;
                recompute_hover(state, self);
                if state.hovered_file_item != prev_file || state.hovered_lane != prev_lane {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::CursorLeft) => {
                state.cursor_position = None;
                let had_hover =
                    state.hovered_file_item.take().is_some() || state.hovered_lane.take().is_some();
                if had_hover {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if cursor.position_over(bounds).is_none() {
                    return;
                }
                let movement = match *delta {
                    mouse::ScrollDelta::Lines { x: _, y } => {
                        Vector::new(0.0, -y * REVISION_ROW_HEIGHT * LINE_SCROLL_ROWS)
                    }
                    mouse::ScrollDelta::Pixels { x: _, y } => {
                        Vector::new(0.0, -y * PIXEL_SCROLL_SCALE)
                    }
                };
                if movement.y != 0.0 {
                    state.vertical_offset =
                        (state.vertical_offset + movement.y as f64).clamp(0.0, max_vertical);
                    let prev = state.hovered_file_item;
                    recompute_hover(state, self);
                    shell.capture_event();
                    shell.request_redraw();
                    let _ = prev;
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(cursor_pos) = cursor.position_over(bounds) else {
                    return;
                };
                match scrollbar::on_button_pressed(
                    &mut state.scrollbar,
                    cursor_pos,
                    bounds,
                    content_height,
                    state.vertical_offset as f32,
                ) {
                    scrollbar::ScrollbarEvent::OffsetChanged(new_offset) => {
                        state.vertical_offset = (new_offset as f64).clamp(0.0, max_vertical);
                        shell.capture_event();
                        shell.request_redraw();
                        if state.vertical_offset != prev_offset
                            && let Some(cb) = on_scroll
                        {
                            shell.publish(cb(state.vertical_offset));
                        }
                        return;
                    }
                    scrollbar::ScrollbarEvent::Captured => {
                        shell.capture_event();
                        return;
                    }
                    scrollbar::ScrollbarEvent::None => {}
                }
                let local_y = (cursor_pos.y - bounds.y) as f64 + state.vertical_offset;
                if let Some(row_idx) = self.row_at_offset(local_y) {
                    match self.row_kind(row_idx) {
                        RowKind::Revision(_) => {
                            if let Item::Revision(rev) = self.item_at(row_idx) {
                                shell.publish((self.on_select_revision)(rev.selection_key));
                            }
                            shell.capture_event();
                        }
                        // The callback wants the *display* row index into the
                        // file tree (directories toggle their collapse state
                        // by position), not `FileRowView::file_index` — which
                        // is the document index and `usize::MAX` for dirs.
                        RowKind::File(display_index) => {
                            shell.publish((self.on_select_file)(display_index));
                            shell.capture_event();
                        }
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let scrollbar::ScrollbarEvent::Captured =
                    scrollbar::on_button_released(&mut state.scrollbar)
                {
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let Some(cursor_pos) = cursor.position_over(bounds) else {
                    return;
                };
                let local_y = (cursor_pos.y - bounds.y) as f64 + state.vertical_offset;
                let Some(row_idx) = self.row_at_offset(local_y) else {
                    return;
                };
                // The row's on-screen rect (window-content points) anchors
                // the native highlight drawn while the menu is open.
                let row_h = row_height_of(self.row_kind(row_idx));
                let screen_y = bounds.y + (self.row_top(row_idx) - state.vertical_offset) as f32;
                let row_rect = Rectangle {
                    x: bounds.x,
                    y: screen_y,
                    width: bounds.width,
                    height: row_h,
                };
                match self.row_kind(row_idx) {
                    RowKind::Revision(_) => {
                        let Some(callback) = self.on_context_menu else {
                            return;
                        };
                        if let Item::Revision(rev) = self.item_at(row_idx) {
                            shell.publish(callback(rev.selection_key, row_rect, cursor_pos));
                            shell.capture_event();
                        }
                    }
                    RowKind::File(display_index) => {
                        let Some(callback) = self.on_file_context_menu else {
                            return;
                        };
                        shell.publish(callback(display_index, row_rect, cursor_pos));
                        shell.capture_event();
                    }
                }
            }
            _ => {}
        }

        // Report any offset change from this pass (wheel, reveal, restore,
        // clamp) once. The scrollbar early-returns above publish on their own.
        if state.vertical_offset != prev_offset
            && let Some(cb) = on_scroll
        {
            shell.publish(cb(state.vertical_offset));
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let state = tree.state.downcast_ref::<State>();
        let cursor_pos = state.cursor_position?;
        let bounds = layout.bounds();

        // Lane tooltip wins over the file path tooltip when both could
        // fire at the same time — the cursor is in the gutter, which
        // is mutually exclusive with the content area anyway.
        if let Some((item_idx, lane, _half)) = state.hovered_lane {
            if item_idx >= self.row_count() {
                return None;
            }
            let item = self.item_at(item_idx);
            let labels = item_lane_labels(&item, lane);
            if labels.is_empty() {
                return None;
            }
            let text = labels.join(", ");
            let row_top = self.row_top(item_idx) - state.vertical_offset;
            let row_screen_y = bounds.y + row_top as f32;
            let row_height = self.row_height(&item);
            let gutter_total = self.item_gutter_width(&item);
            let text_size = measure::line_bounds(&text, CAPTION_TEXT_SIZE, self.style.primary_font);
            return Some(overlay::Element::new(Box::new(TooltipOverlay {
                text,
                cursor: cursor_pos + translation,
                // Anchor just past the gutter so the tip sits next to
                // the stroke rather than at the far right edge.
                row_anchor_x: bounds.x + gutter_total + translation.x,
                row_anchor_y: row_screen_y + translation.y,
                row_height,
                style: self.style,
                text_size,
            })));
        }

        let item_idx = state.hovered_file_item?;
        if item_idx >= self.row_count() {
            return None;
        }
        let Item::File(file) = self.item_at(item_idx) else {
            return None;
        };
        let row_top = self.row_top(item_idx) - state.vertical_offset;
        let row_screen_y = bounds.y + row_top as f32;
        let row_height = FILE_ROW_HEIGHT;

        let text_size =
            measure::line_bounds(&file.raw_path, CAPTION_TEXT_SIZE, self.style.primary_font);

        Some(overlay::Element::new(Box::new(TooltipOverlay {
            text: file.raw_path,
            cursor: cursor_pos + translation,
            row_anchor_x: bounds.x + bounds.width + translation.x,
            row_anchor_y: row_screen_y + translation.y,
            row_height,
            style: self.style,
            text_size,
        })))
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let Some(point) = cursor.position_over(bounds) else {
            return mouse::Interaction::None;
        };
        if scrollbar::is_dragging(&state.scrollbar)
            || scrollbar::hits_container(bounds, point, self.content_height() as f32)
        {
            return mouse::Interaction::Idle;
        }
        mouse::Interaction::Pointer
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _renderer_style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if bounds.intersection(viewport).is_none() {
            return;
        }
        let state = tree.state.downcast_ref::<State>();

        let visible_top = state.vertical_offset;
        let visible_bottom = visible_top + bounds.height as f64;

        renderer.with_layer(bounds, |renderer| {
            fill_background(renderer, bounds, self.style.background);

            // Resolve the hovered (row, lane, half) → (segment id, lane)
            // once so each drawn row can do a single segment lookup
            // instead of re-resolving the hover row's segment. The half
            // matters at split lanes: the same lane index at the hover
            // row may carry two different segment ids in its top vs
            // bottom half.
            let emphasized_segment =
                state
                    .hovered_lane
                    .and_then(|(hov_idx, hov_lane, hov_half)| {
                        if hov_idx >= self.row_count() {
                            return None;
                        }
                        let item = self.item_at(hov_idx);
                        item_lane_segment(&item, hov_lane, hov_half).map(|seg| (seg, hov_lane))
                    });

            // Indent-guide emphasis: hovering a tree row brightens the guide
            // of its parent level across the contiguous sibling run (rows at
            // the same or deeper indent around it) — VSCode's "active indent
            // guide". Resolved once here as (level, first flat, last flat).
            let guide_emphasis = state.hovered_file_item.and_then(|hovered| {
                // The walk is bounded: emphasis only matters for drawn rows,
                // and a run longer than this already spans several screens.
                const RUN_SCAN_CAP: usize = 400;
                if hovered >= self.row_count() {
                    return None;
                }
                let depth_of = |flat: usize| match self.item_at(flat) {
                    Item::File(row) => Some((row.indent / FILE_TREE_INDENT).round() as usize),
                    Item::Revision(_) => None,
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
                while end + 1 < self.row_count()
                    && end - hovered < RUN_SCAN_CAP
                    && depth_of(end + 1).is_some_and(|d| d >= depth)
                {
                    end += 1;
                }
                Some((level, start, end))
            });

            // Walk only the rows that intersect the viewport, materializing
            // each one's view on demand — off-screen rows cost nothing.
            let count = self.row_count();
            let first = self.row_at_offset(visible_top).unwrap_or(0);
            let mut flat = first;
            let mut row_top_local = self.row_top(first);
            while flat < count && row_top_local < visible_bottom {
                let row_h = row_height_of(self.row_kind(flat));
                // `row_top_local - visible_top` is viewport-relative (small) and
                // exact in `f64`; only here, after the subtraction cancels the
                // large magnitude, is it safe to narrow to `f32` for rendering.
                let screen_y = bounds.y + (row_top_local - visible_top) as f32;
                let row_bounds = Rectangle {
                    x: bounds.x,
                    y: screen_y,
                    width: bounds.width,
                    height: row_h,
                };
                let item = self.item_at(flat);
                let gutter_total = self.item_gutter_width(&item);
                let emphasized_lane_before = emphasized_segment.and_then(|(seg, lane)| {
                    let this_seg = item_lane_segment(&item, lane, LaneHalf::Before)?;
                    (this_seg == seg).then_some(lane)
                });
                let emphasized_lane_after = emphasized_segment.and_then(|(seg, lane)| {
                    let this_seg = item_lane_segment(&item, lane, LaneHalf::After)?;
                    (this_seg == seg).then_some(lane)
                });

                match item {
                    Item::Revision(rev) => {
                        self.draw_revision(
                            renderer,
                            row_bounds,
                            &rev,
                            gutter_total,
                            emphasized_lane_before,
                            emphasized_lane_after,
                        );
                    }
                    Item::File(f) => {
                        // File rows are continuation-only — they sit
                        // under the parent revision's `after` snapshot,
                        // so the after-half emphasis is what applies.
                        let emphasized_guide = guide_emphasis
                            .filter(|&(_, start, end)| flat >= start && flat <= end)
                            .map(|(level, ..)| level);
                        self.draw_file(
                            renderer,
                            row_bounds,
                            &f,
                            gutter_total,
                            emphasized_lane_after,
                            emphasized_guide,
                            state.hovered_file_item == Some(flat),
                        );
                    }
                }

                row_top_local += row_h as f64;
                flat += 1;
            }

            let geom = scrollbar::geometry(
                bounds,
                self.content_height() as f32,
                state.vertical_offset as f32,
            );
            scrollbar::draw(renderer, &geom, &self.style.scrollbar);
        });
    }
}

impl<'a, Message> RevisionList<'a, Message> {
    fn draw_revision<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        rev: &RevisionRowView,
        gutter_total: f32,
        emphasized_lane_before: Option<usize>,
        emphasized_lane_after: Option<usize>,
    ) where
        R: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
    {
        let selected = self
            .selected_row
            .as_ref()
            .map(|key| key == &rev.selection_key)
            .unwrap_or(false);
        let is_working_copy = matches!(rev.selection_key, RowSelectionKey::WorkingCopy);

        // Row chrome extends edge-to-edge. The graph is drawn last in this
        // method so its lines & node still sit visually above any chrome.
        // Working-copy rows get a left→right gradient with a fixed light
        // accent on the left fading into the row's base color on the
        // right (panel background normally, the selected gray when
        // selected). The left tint is pre-mixed opaque so the orange
        // reads identically whether or not the row is selected.
        if is_working_copy {
            let right = if selected {
                self.style.selected_background
            } else {
                self.style.background
            };
            let left = mix_color(self.style.accent, self.style.background, 0.20);
            let gradient = gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                .add_stop(0.0, left)
                .add_stop(1.0, right);
            fill_gradient(renderer, row_bounds, gradient);
        } else if selected {
            fill_background(renderer, row_bounds, self.style.selected_background);
        }
        fill_background(
            renderer,
            Rectangle {
                x: row_bounds.x,
                y: row_bounds.y + row_bounds.height - 1.0,
                width: row_bounds.width,
                height: 1.0,
            },
            self.style.border,
        );

        let content_left = row_bounds.x + gutter_total;
        let content_right_pad = CONTENT_PADDING;
        let row_clip = row_bounds;
        let content_width = (row_bounds.width - gutter_total - content_right_pad).max(1.0);

        // Two-line stack: ids/author/chips on top, description below.
        // Sizes use cap-height for stack math rather than the rendered
        // line-box so the gap stays visually tight.
        let id_size = CAPTION_TEXT_SIZE;
        let desc_size = SMALL_TEXT_SIZE;
        let line_gap = 4.0;
        let stack_height = id_size + line_gap + desc_size;
        let stack_top = row_bounds.y + ((row_bounds.height - stack_height) / 2.0).max(0.0);
        let title_mid_y = stack_top + id_size / 2.0;
        let desc_mid_y = stack_top + id_size + line_gap + desc_size / 2.0;

        let prefix_w = measure::line_width(&rev.change_id_prefix, id_size, self.style.mono_font);
        let suffix_w = measure::line_width(&rev.change_id_suffix, id_size, self.style.mono_font);
        let commit_w = measure::line_width(&rev.commit_id_short, id_size, self.style.mono_font);

        // === Measurements ===
        let id_gap = 8.0;
        let chip_gap = 6.0;
        let at_marker = matches!(rev.selection_key, RowSelectionKey::WorkingCopy);
        let at_gap = 4.0;
        let at_w = if at_marker {
            measure::line_width("@", id_size, self.style.mono_font)
        } else {
            0.0
        };
        let author_full_w = measure::line_width(&rev.author, id_size, self.style.primary_font);
        let ellipsis_w = measure::line_width("…", id_size, self.style.mono_font);

        let bm_widths: Vec<f32> = rev
            .bookmark_chips
            .iter()
            .map(|c| chip::width(&c.label, c.icon, c.font))
            .collect();
        let status_widths: Vec<f32> = rev
            .status_chips
            .iter()
            .map(|c| chip::width(&c.label, c.icon, c.font))
            .collect();
        let status_total: f32 = status_widths.iter().sum::<f32>()
            + chip_gap * status_widths.len().saturating_sub(1) as f32;

        // === Layout budget ===
        let content_right = row_bounds.x + row_bounds.width - content_right_pad;
        let total_width = (content_right - content_left).max(0.0);

        // Always-shown widths consume from the budget first.
        let id_w = prefix_w + suffix_w;
        let mandatory_left = at_w + (if at_marker { at_gap } else { 0.0 }) + id_w;
        let mandatory_right_gap = if !status_widths.is_empty() {
            chip_gap
        } else {
            0.0
        };
        let mandatory = mandatory_left + status_total + mandatory_right_gap;
        let mut budget = (total_width - mandatory).max(0.0);

        // === Priority: commit_id > bookmark chips > author ===
        // 1. commit_id (with leading id_gap).
        let show_commit_id = commit_w + id_gap <= budget;
        if show_commit_id {
            budget -= commit_w + id_gap;
        }

        // 2. Bookmarks: fit greedily, reserving space for a `+N` overflow
        //    chip when not all fit. The chip rail's leading gap separates
        //    it from the left-side text.
        let mut visible_bookmarks: usize = 0;
        let mut overflow_count: usize = 0;
        let mut overflow_w: f32 = 0.0;
        if !rev.bookmark_chips.is_empty() {
            let all_w: f32 =
                bm_widths.iter().sum::<f32>() + chip_gap * bm_widths.len().saturating_sub(1) as f32;
            let bm_block_w = if chip_gap + all_w <= budget {
                visible_bookmarks = rev.bookmark_chips.len();
                all_w
            } else {
                let mut acc = 0.0;
                for (i, &w) in bm_widths.iter().enumerate() {
                    let gap = if i == 0 { 0.0 } else { chip_gap };
                    let candidate = acc + gap + w;
                    let remaining = rev.bookmark_chips.len() - i - 1;
                    let need_overflow = remaining > 0;
                    let overflow_label = format!("+{}", remaining.max(1));
                    let overflow_chip_w =
                        chip::width(&overflow_label, None, self.style.primary_font);
                    let plus_overflow = if need_overflow {
                        chip_gap + overflow_chip_w
                    } else {
                        0.0
                    };
                    if chip_gap + candidate + plus_overflow <= budget {
                        visible_bookmarks = i + 1;
                        acc = candidate;
                    } else {
                        break;
                    }
                }
                if visible_bookmarks < rev.bookmark_chips.len() {
                    overflow_count = rev.bookmark_chips.len() - visible_bookmarks;
                    let label = format!("+{overflow_count}");
                    overflow_w = chip::width(&label, None, self.style.primary_font);
                }
                acc + if overflow_count > 0 {
                    chip_gap + overflow_w
                } else {
                    0.0
                }
            };
            // Subtract the chip rail's leading gap once, plus the block.
            budget -= chip_gap + bm_block_w;
        }

        // 3. Author squeezes into whatever's left (with a leading id_gap).
        let author_max_w = (budget - id_gap).max(0.0);
        let (show_author, author_draw_w) = if !rev.author.is_empty() && author_max_w >= ellipsis_w {
            (true, author_max_w.min(author_full_w))
        } else {
            (false, 0.0)
        };

        // Ellipsis indicator when commit_id or author got dropped entirely.
        let need_ellipsis = (!show_commit_id) || (!show_author && !rev.author.is_empty());

        // === Color selection ===
        let (col_at, col_id_prefix, col_id_suffix, col_commit, col_author, col_ellipsis) = (
            self.style.accent,
            self.style.accent_text,
            self.style.subtle_text,
            self.style.muted_text,
            self.style.subtle_text,
            self.style.subtle_text,
        );

        // === Draw left-side flowing content ===
        let mut x_cursor = content_left;
        if at_marker {
            fill_text_centered_y(
                renderer,
                "@",
                x_cursor,
                title_mid_y,
                at_w.max(1.0),
                id_size,
                col_at,
                self.style.mono_font,
                row_clip,
                text::Alignment::Left,
            );
            x_cursor += at_w + at_gap;
        }
        fill_text_centered_y(
            renderer,
            &rev.change_id_prefix,
            x_cursor,
            title_mid_y,
            prefix_w.max(1.0),
            id_size,
            col_id_prefix,
            self.style.mono_font,
            row_clip,
            text::Alignment::Left,
        );
        x_cursor += prefix_w;
        fill_text_centered_y(
            renderer,
            &rev.change_id_suffix,
            x_cursor,
            title_mid_y,
            suffix_w.max(1.0),
            id_size,
            col_id_suffix,
            self.style.mono_font,
            row_clip,
            text::Alignment::Left,
        );
        x_cursor += suffix_w;

        if show_commit_id {
            x_cursor += id_gap;
            fill_text_centered_y(
                renderer,
                &rev.commit_id_short,
                x_cursor,
                title_mid_y,
                commit_w.max(1.0),
                id_size,
                col_commit,
                self.style.mono_font,
                row_clip,
                text::Alignment::Left,
            );
            x_cursor += commit_w;
        }

        if show_author {
            x_cursor += id_gap;
            fill_text_truncated(
                renderer,
                &rev.author,
                x_cursor,
                title_mid_y,
                author_draw_w,
                id_size,
                col_author,
                self.style.primary_font,
                row_clip,
            );
        } else if need_ellipsis {
            // Single `…` after the last left-side glyph signals that
            // commit_id and/or author got hidden for lack of room.
            x_cursor += id_gap;
            fill_text_centered_y(
                renderer,
                "…",
                x_cursor,
                title_mid_y,
                ellipsis_w.max(1.0),
                id_size,
                col_ellipsis,
                self.style.mono_font,
                row_clip,
                text::Alignment::Left,
            );
        }

        // === Draw right-anchored chip rail ===
        // Compose the rail left-to-right starting from its left edge: any
        // bookmarks, then the `+N` overflow chip (if any), then status
        // chips. The leading edge is computed from the cumulative width.
        let rail_w: f32 = {
            let mut w = 0.0;
            for (i, &bw) in bm_widths.iter().enumerate().take(visible_bookmarks) {
                if i > 0 {
                    w += chip_gap;
                }
                w += bw;
            }
            if overflow_count > 0 {
                if w > 0.0 {
                    w += chip_gap;
                }
                w += overflow_w;
            }
            if !status_widths.is_empty() {
                if w > 0.0 {
                    w += chip_gap;
                }
                w += status_total;
            }
            w
        };
        let mut chip_x = content_right - rail_w;
        let overflow_color = rev.lane_color;
        for (c, bw) in rev
            .bookmark_chips
            .iter()
            .zip(bm_widths.iter())
            .take(visible_bookmarks)
        {
            chip::draw(renderer, c, chip_x, title_mid_y, row_clip);
            chip_x += bw + chip_gap;
        }
        if overflow_count > 0 {
            let overflow_chip = Chip {
                label: format!("+{overflow_count}"),
                font: self.style.primary_font,
                background: chip_background(overflow_color),
                text_color: overflow_color,
                border_color: None,
                border_dashed: false,
                icon: None,
            };
            chip::draw(renderer, &overflow_chip, chip_x, title_mid_y, row_clip);
            chip_x += overflow_w + chip_gap;
        }
        for (i, c) in rev.status_chips.iter().enumerate() {
            chip::draw(renderer, c, chip_x, title_mid_y, row_clip);
            chip_x += status_widths[i] + chip_gap;
        }

        // Reserve room on the right of the description for the collapse/
        // expand chevron when this is the selected row. The chevron sizes
        // up past the description so the affordance reads at a glance —
        // at the description's own size it shrinks to ~5px on the y axis
        // and disappears against the row chrome.
        let chevron_glyph = rev.collapse_chevron.map(|expanded| {
            if expanded {
                icons::CHEVRON_DOWN
            } else {
                icons::CHEVRON_RIGHT
            }
        });
        let chevron_size = CHEVRON_TEXT_SIZE;
        let chevron_width = chevron_glyph
            .map(|glyph| measure::line_width(glyph, chevron_size, icons::ICON_FONT))
            .unwrap_or(0.0);
        let chevron_gap = if chevron_glyph.is_some() { 6.0 } else { 0.0 };
        let description_width = (content_width - chevron_width - chevron_gap).max(1.0);

        fill_text_truncated(
            renderer,
            &rev.description,
            content_left,
            desc_mid_y,
            description_width,
            desc_size,
            rev.description_color,
            self.style.primary_font,
            row_clip,
        );

        if let Some(glyph) = chevron_glyph {
            let chevron_x = row_bounds.x + row_bounds.width - content_right_pad - chevron_width;
            fill_text_centered_y(
                renderer,
                glyph,
                chevron_x,
                desc_mid_y,
                chevron_width.max(1.0),
                chevron_size,
                self.style.subtle_text,
                icons::ICON_FONT,
                row_clip,
                text::Alignment::Left,
            );
        }

        // Graph painted last so node + lines sit on top of any row chrome.
        let gutter_bounds = Rectangle {
            x: row_bounds.x + GUTTER_LEFT_PADDING,
            y: row_bounds.y,
            width: gutter_total - GUTTER_LEFT_PADDING - GUTTER_PADDING,
            height: row_bounds.height,
        };
        // Working-copy node sits on the gradient's faded right side, so
        // the saturated accent reads cleanly without a contrast swap.
        let node_override = if is_working_copy {
            Some(self.style.accent)
        } else {
            None
        };
        draw_revision_row(
            renderer,
            gutter_bounds,
            &rev.frame,
            &rev.columns,
            &rev.prev_columns,
            &self.style.graph,
            node_override,
            emphasized_lane_before,
            emphasized_lane_after,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_file<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        f: &FileRowView,
        gutter_total: f32,
        emphasized_lane: Option<usize>,
        emphasized_guide: Option<usize>,
        hovered: bool,
    ) where
        R: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
    {
        let selected = self
            .selected_file
            .map(|idx| idx == f.file_index)
            .unwrap_or(false);

        if selected {
            fill_background(renderer, row_bounds, self.style.selected_background);
        } else if hovered {
            // Soft hover wash — weaker than selection so the two never
            // compete when the cursor crosses the selected row.
            fill_background(
                renderer,
                row_bounds,
                Color {
                    a: 0.06,
                    ..self.style.muted_text
                },
            );
        }
        // Deliberately no per-row separator: the indent guides supply the
        // vertical structure, and both line sets together read as a grid.

        let content_x = row_bounds.x + gutter_total + f.indent;
        let row_clip = row_bounds;
        let row_mid_y = row_bounds.y + row_bounds.height / 2.0;

        // Indent guides: one hairline per ancestor level, aligned under the
        // parent rows' chevron column. The hovered row's sibling run wears a
        // brighter guide at its parent level (see `guide_emphasis` in draw).
        let depth = (f.indent / FILE_TREE_INDENT).round() as usize;
        if depth > 0 {
            let guide_offset = FILE_CHEVRON_COL / 2.0;
            for level in 0..depth {
                let x =
                    row_bounds.x + gutter_total + level as f32 * FILE_TREE_INDENT + guide_offset;
                let color = if emphasized_guide == Some(level) {
                    Color {
                        a: 0.8,
                        ..self.style.subtle_text
                    }
                } else {
                    self.style.border
                };
                fill_background(
                    renderer,
                    Rectangle {
                        x: x.round(),
                        y: row_bounds.y,
                        width: 1.0,
                        height: row_bounds.height,
                    },
                    color,
                );
            }
        }

        // Left columns: [chevron (dirs)] [file/folder icon] name…
        if let Some(collapsed) = f.chevron {
            let glyph = if collapsed {
                icons::CHEVRON_RIGHT
            } else {
                icons::CHEVRON_DOWN
            };
            fill_text_centered_y(
                renderer,
                glyph,
                content_x + FILE_CHEVRON_COL / 2.0,
                row_mid_y,
                FILE_CHEVRON_COL,
                CAPTION_TEXT_SIZE,
                f.primary_color,
                icons::ICON_FONT,
                row_clip,
                text::Alignment::Center,
            );
        }
        let icon_glyph = match f.chevron {
            Some(true) => icons::FOLDER,
            Some(false) => icons::FOLDER_OPEN,
            None => icons::FILE,
        };
        fill_text_centered_y(
            renderer,
            icon_glyph,
            content_x + FILE_CHEVRON_COL + FILE_ICON_COL / 2.0,
            row_mid_y,
            FILE_ICON_COL,
            FILE_ICON_SIZE,
            f.icon_color,
            icons::ICON_FONT,
            row_clip,
            text::Alignment::Center,
        );

        // Layout: [indent][chevron][icon] name … [+N] [-N] [status chip] pad
        // — the status chip is the rightmost column so badges align down the
        // tree; directory rows have neither stats nor chip, so their name
        // runs to the right edge. The chip column clears the scrollbar band
        // with the same right padding the revision rows' bookmark rail uses.
        let row_gap = FILE_ROW_GAP;
        let right_pad = FILE_ROW_RIGHT_PAD;
        let chip_col = self.style.file_badge_width;
        let chip_left = row_bounds.x + row_bounds.width - CONTENT_PADDING - chip_col;
        let chip_gap = if chip_col > 0.0 { row_gap } else { 0.0 };
        let minus_x = chip_left - chip_gap - f.deletions_width;
        let plus_x = minus_x - row_gap - f.additions_width;

        let path_x = content_x + FILE_CHEVRON_COL + FILE_ICON_COL + row_gap;
        let path_right = if f.chevron.is_some() {
            row_bounds.x + row_bounds.width - right_pad
        } else {
            plus_x - row_gap
        };
        let path_w = (path_right - path_x).max(1.0);

        // `fill_text_truncated` applies an end-ellipsis at the renderer
        // level, so a name wider than `path_w` doesn't bleed into the +N /
        // -N columns; the full path is on the hover tooltip.
        fill_text_truncated(
            renderer,
            &f.primary,
            path_x,
            row_mid_y,
            path_w,
            CAPTION_TEXT_SIZE,
            f.primary_color,
            self.style.primary_font,
            row_clip,
        );

        // Status chip, right-aligned in the rightmost column (single-letter
        // labels center within it so mixed labels still line up). An empty
        // label draws nothing.
        if f.chevron.is_none() && !f.status_label.is_empty() {
            let status_chip = Chip {
                label: f.status_label.clone(),
                font: self.style.mono_font,
                background: f.status_background,
                text_color: f.status_text,
                border_color: None,
                border_dashed: false,
                icon: None,
            };
            let chip_w = chip::width(&f.status_label, None, self.style.mono_font);
            chip::draw(
                renderer,
                &status_chip,
                chip_left + ((chip_col - chip_w) / 2.0).max(0.0),
                row_mid_y,
                row_clip,
            );
        }

        // Directory rows carry no stats; zero-width stat columns (the source
        // tree) skip them too.
        if f.chevron.is_none() && f.additions_width > 0.0 {
            let plus = format!("+{}", f.additions);
            let minus = format!("-{}", f.deletions);
            fill_text_centered_y(
                renderer,
                &plus,
                plus_x,
                row_mid_y,
                f.additions_width,
                CAPTION_TEXT_SIZE,
                f.additions_text,
                self.style.primary_font,
                row_clip,
                text::Alignment::Left,
            );
            fill_text_centered_y(
                renderer,
                &minus,
                minus_x,
                row_mid_y,
                f.deletions_width,
                CAPTION_TEXT_SIZE,
                f.deletions_text,
                self.style.primary_font,
                row_clip,
                text::Alignment::Left,
            );
        }

        // Graph last — same z-order rationale as draw_revision.
        let gutter_bounds = Rectangle {
            x: row_bounds.x + GUTTER_LEFT_PADDING,
            y: row_bounds.y,
            width: gutter_total - GUTTER_LEFT_PADDING - GUTTER_PADDING,
            height: row_bounds.height,
        };
        draw_continuation_row(
            renderer,
            gutter_bounds,
            &f.continuation,
            &f.columns,
            &self.style.graph,
            emphasized_lane,
        );
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
/// snapshot of the lane-label state — see [`RevisionRowView::lane_labels`]
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
/// or the column is out of range. File rows aren't split — both halves
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

fn fill_background<R: renderer::Renderer>(renderer: &mut R, rect: Rectangle, color: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        color,
    );
}

/// Blend `fg` onto `bg` as if drawing `fg` (with its alpha replaced by
/// `weight`) over an opaque `bg`. Returns an opaque color.
fn mix_color(fg: Color, bg: Color, weight: f32) -> Color {
    let w = weight.clamp(0.0, 1.0);
    Color {
        r: fg.r * w + bg.r * (1.0 - w),
        g: fg.g * w + bg.g * (1.0 - w),
        b: fg.b * w + bg.b * (1.0 - w),
        a: 1.0,
    }
}

fn fill_gradient<R: renderer::Renderer>(
    renderer: &mut R,
    rect: Rectangle,
    gradient: gradient::Linear,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        gradient,
    );
}

/// Truncating text run, vertically centered on `center_y`. Used for
/// description/detail rows where text might overflow horizontally and we
/// want an end-ellipsis. Centering goes through `align_y: Center` so all
/// rows in this widget share the same y-positioning model.
#[allow(clippy::too_many_arguments)]
fn fill_text_truncated<R: text::Renderer<Font = Font>>(
    renderer: &mut R,
    content: &str,
    x: f32,
    center_y: f32,
    width: f32,
    size: f32,
    color: Color,
    font: Font,
    clip: Rectangle,
) {
    if content.is_empty() {
        return;
    }
    let height = size * LINE_HEIGHT_MULTIPLIER;
    renderer.fill_text(
        Text {
            content: content.to_owned(),
            bounds: Size::new(width.max(1.0), height.max(1.0)),
            size: Pixels(size),
            line_height: text::LineHeight::Absolute(Pixels(height)),
            font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
            ellipsis: text::Ellipsis::End,
            hint_factor: None,
        },
        Point::new(x, center_y),
        color,
        clip,
    );
}

/// Fill text whose bounds box is centered vertically on `center_y`. We push
/// this through `fill_text` (the `Cached` text path) rather than
/// `fill_paragraph` because Cached text is the only path where iced applies
/// `align_y` placement, and it owns its `String` so we don't have to manage
/// paragraph lifetimes.
#[allow(clippy::too_many_arguments)]
fn fill_text_centered_y<R: text::Renderer<Font = Font>>(
    renderer: &mut R,
    content: &str,
    x: f32,
    center_y: f32,
    width: f32,
    size: f32,
    color: Color,
    font: Font,
    clip: Rectangle,
    align_x: text::Alignment,
) {
    if content.is_empty() {
        return;
    }
    let height = size * LINE_HEIGHT_MULTIPLIER;
    renderer.fill_text(
        Text {
            content: content.to_owned(),
            bounds: Size::new(width.max(1.0), height.max(1.0)),
            size: Pixels(size),
            line_height: text::LineHeight::Absolute(Pixels(height)),
            font,
            align_x,
            align_y: alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        },
        Point::new(x, center_y),
        color,
        clip,
    );
}

struct TooltipOverlay {
    text: String,
    cursor: Point,
    row_anchor_x: f32,
    row_anchor_y: f32,
    row_height: f32,
    style: RevisionListStyle,
    /// Pre-measured text size (via [`measure::line_bounds`] in `overlay()`),
    /// so layout uses true glyph extents instead of a `chars * size * 0.55`
    /// guess that left a gap on the right.
    text_size: Size,
}

impl TooltipOverlay {
    fn box_size(&self) -> Size {
        Size::new(
            self.text_size.width + TOOLTIP_PADDING * 2.0,
            self.text_size.height + TOOLTIP_PADDING * 2.0,
        )
    }
}

impl<Message, Renderer> overlay::Overlay<Message, Theme, Renderer> for TooltipOverlay
where
    Renderer: text::Renderer<Font = Font>,
{
    fn layout(&mut self, _renderer: &Renderer, viewport: Size) -> layout::Node {
        let size = self.box_size();
        // Anchor to the right of the sidebar at the row's vertical center,
        // shifted by tooltip_gap. Snap into the viewport if it would clip.
        let mut x = self.row_anchor_x + TOOLTIP_GAP;
        let mut y = self.row_anchor_y + (self.row_height - size.height) / 2.0;
        if x + size.width > viewport.width {
            // Fall back to following the cursor on the left.
            x = (self.cursor.x - size.width - TOOLTIP_GAP).max(4.0);
        }
        if y + size.height > viewport.height {
            y = (viewport.height - size.height - 4.0).max(0.0);
        }
        if y < 0.0 {
            y = 4.0;
        }
        layout::Node::new(size).move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
    ) {
        let bounds = layout.bounds();
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border {
                    color: self.style.tooltip_border,
                    width: 1.0,
                    radius: iced::border::Radius::from(TOOLTIP_RADIUS),
                },
                shadow: Shadow::default(),
                snap: true,
            },
            Background::Color(self.style.tooltip_background),
        );
        // Use fill_text (Cached) — owns its String, so we don't have to keep
        // a Paragraph alive past the end of this draw call. align_y: Center
        // does the vertical math against the box.
        fill_text_centered_y(
            renderer,
            &self.text,
            bounds.x + TOOLTIP_PADDING,
            bounds.y + bounds.height / 2.0,
            bounds.width - TOOLTIP_PADDING * 2.0,
            CAPTION_TEXT_SIZE,
            self.style.tooltip_text,
            self.style.primary_font,
            bounds,
            text::Alignment::Left,
        );
    }
}

impl<'a, Message: 'a, Renderer> From<RevisionList<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer + 'a,
{
    fn from(widget: RevisionList<'a, Message>) -> Self {
        Element::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Three commits with the middle one (index 1) expanded over 2 file rows.
    // Flat layout: [rev0, rev1, file0, file1, rev2].
    const EXP: Option<(usize, usize)> = Some((1, 2));

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
        // and a ~1M-commit list is ~50M px tall. There, draw's `flat * H` and
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
}
