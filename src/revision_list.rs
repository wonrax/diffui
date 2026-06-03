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

use std::cell::RefCell;

use iced::advanced::{
    Layout, Shell, Widget,
    graphics::geometry::{self, Frame, LineCap, LineJoin, Path, Stroke},
    layout, mouse, overlay, renderer,
    text::{self, Paragraph, Text},
    widget::{Tree, tree},
};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Point, Radians, Rectangle,
    Shadow, Size, Theme, Vector, alignment, gradient,
};
use jj_lib::graph::GraphEdgeType;

use crate::graph::LaneFrame;
use crate::graph_view::{
    LANE_WIDTH, RevisionGraphStyle, draw_continuation_row, draw_revision_row, lane_strip_width,
};
use crate::scrollbar::{self, ScrollbarState, ScrollbarStyle};
use crate::theme::chip_background;

const LINE_SCROLL_ROWS: f32 = 1.5;
const PIXEL_SCROLL_SCALE: f32 = 0.65;

const REVISION_ROW_HEIGHT: f32 = 46.0;
const FILE_ROW_HEIGHT: f32 = 42.0;
pub const GUTTER_LEFT_PADDING: f32 = 8.0;
pub const GUTTER_PADDING: f32 = 8.0;
const CONTENT_PADDING: f32 = 12.0;
const INDICATOR_RADIUS: f32 = 5.0;
const SMALL_TEXT_SIZE: f32 = 14.0;
const CAPTION_TEXT_SIZE: f32 = 13.0;
/// Status-badge text size. Smaller than the row's caption text so the
/// single-letter chip (M/A/D/R) reads as a compact tag rather than another
/// full-weight column on the file row.
const FILE_BADGE_TEXT_SIZE: f32 = 10.0;
pub const FILE_ROW_GAP: f32 = 6.0;
pub const FILE_ROW_RIGHT_PAD: f32 = 10.0;
const TOOLTIP_RADIUS: f32 = 5.0;
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
pub struct IndicatorChip {
    pub label: String,
    pub background: Color,
    pub text_color: Color,
    /// Optional 1px chip border. When `border_dashed` is `true`, the
    /// stroke is dashed (used by the `empty` chip in the design
    /// system). When `false`, it's a solid 1px line (used by remote
    /// bookmark chips — outlined, lane-colored).
    pub border_color: Option<Color>,
    pub border_dashed: bool,
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
    pub bookmark_chips: Vec<IndicatorChip>,
    /// Status chips (e.g. `empty`, `conflict`). Always shown — they're
    /// load-bearing semantics and shouldn't compress.
    pub status_chips: Vec<IndicatorChip>,
    /// Color for the `+N` overflow chip (= the row's lane color).
    pub lane_color: Color,
    pub frame: LaneFrame,
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
    pub primary: String,
    pub secondary: String,
    pub raw_path: String,
    pub status_label: String,
    pub status_background: Color,
    pub status_text: Color,
    pub additions: usize,
    pub deletions: usize,
    pub additions_text: Color,
    pub deletions_text: Color,
    pub continuation: Vec<Option<GraphEdgeType>>,
    pub additions_width: f32,
    pub deletions_width: f32,
    pub primary_color: Color,
    pub secondary_color: Color,
    pub file_index: usize,
    /// Lane → bookmark labels for the continuation strokes at this file
    /// row. Equals the post-trim snapshot of the parent revision's
    /// label state, so a lane that terminates at the parent isn't
    /// labeled here.
    pub lane_labels: Vec<Vec<String>>,
    /// Per-lane segment id, matching the parent revision row for any
    /// lane that survives into the continuation. See
    /// [`RevisionRowView::lane_segments`].
    pub lane_segments: Vec<Option<usize>>,
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
/// expanded commit (by file index).
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
    on_select_revision: fn(RowSelectionKey) -> Message,
    on_select_file: fn(usize) -> Message,
    /// Optional right-click handler for a revision row — opens the context
    /// menu. Receives the row's selection key and its on-screen rectangle (in
    /// window-content points), the latter used to anchor a native highlight.
    on_context_menu: Option<fn(RowSelectionKey, Rectangle) -> Message>,
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
            on_select_revision,
            on_select_file,
            on_context_menu: None,
        }
    }

    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    /// Register a right-click handler for revision rows (the context menu). The
    /// callback gets the row key and its on-screen rect (window-content points).
    pub fn on_context_menu(mut self, callback: fn(RowSelectionKey, Rectangle) -> Message) -> Self {
        self.on_context_menu = Some(callback);
        self
    }

    /// Cause the widget to scroll the currently-selected row into view
    /// the next time `token` changes. Calling with the same value twice
    /// in a row is a no-op.
    pub fn reveal_selected(mut self, token: u64) -> Self {
        self.reveal_token = Some(token);
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
        let lanes = match item {
            Item::Revision(row) => row.frame.lane_count(),
            Item::File(row) => row.continuation.len(),
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

struct State<Paragraph> {
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
    paragraphs: RefCell<Vec<Paragraph>>,
    scrollbar: ScrollbarState,
    /// Most recent `reveal_token` we acted on. `None` until first reveal.
    last_reveal_token: Option<u64>,
    /// Row to bring into view on the next `update()` pass — `None` when
    /// no reveal is pending. Set in `diff()` (which sees prop changes
    /// before layout); consumed in `update()` (which has bounds).
    pending_reveal_row: Option<usize>,
}

impl<Paragraph> State<Paragraph> {
    fn new() -> Self {
        Self {
            vertical_offset: 0.0,
            hovered_file_item: None,
            hovered_lane: None,
            cursor_position: None,
            paragraphs: RefCell::new(Vec::new()),
            scrollbar: ScrollbarState::default(),
            last_reveal_token: None,
            pending_reveal_row: None,
        }
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for RevisionList<'a, Message>
where
    Renderer: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph>::new())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Fill,
        }
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        if self.reveal_token != state.last_reveal_token {
            state.last_reveal_token = self.reveal_token;
            // Defer the actual scroll to `update()` — that's where bounds
            // (needed to clamp + center) are available.
            state.pending_reveal_row = self.selected_flat;
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
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
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

        let recompute_hover = |state: &mut State<Renderer::Paragraph>, this: &Self| {
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
                let lane = (strip_x / LANE_WIDTH).floor() as usize;
                let labels = item_lane_labels(&item, lane);
                if !labels.is_empty() {
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
                    state.hovered_lane = Some((row_idx, lane, half));
                    return;
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
                    match self.item_at(row_idx) {
                        Item::Revision(rev) => {
                            shell.publish((self.on_select_revision)(rev.selection_key));
                            shell.capture_event();
                        }
                        Item::File(f) => {
                            shell.publish((self.on_select_file)(f.file_index));
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
                let Some(callback) = self.on_context_menu else {
                    return;
                };
                let Some(cursor_pos) = cursor.position_over(bounds) else {
                    return;
                };
                let local_y = (cursor_pos.y - bounds.y) as f64 + state.vertical_offset;
                // Only revision rows get a context menu — file rows don't.
                if let Some(row_idx) = self.row_at_offset(local_y)
                    && let Item::Revision(rev) = self.item_at(row_idx)
                {
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
                    shell.publish(callback(rev.selection_key, row_rect));
                    shell.capture_event();
                }
            }
            _ => {}
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
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
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
            let measure_para =
                make_paragraph::<Renderer>(&text, CAPTION_TEXT_SIZE, self.style.primary_font);
            let text_size = measure_para.min_bounds();
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

        let measure_para =
            make_paragraph::<Renderer>(&file.raw_path, CAPTION_TEXT_SIZE, self.style.primary_font);
        let text_size = measure_para.min_bounds();

        Some(overlay::Element::new(Box::new(TooltipOverlay {
            text: file.raw_path.clone(),
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
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
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
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        state.paragraphs.borrow_mut().clear();

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
            let emphasized_segment = state.hovered_lane.and_then(|(hov_idx, hov_lane, hov_half)| {
                if hov_idx >= self.row_count() {
                    return None;
                }
                let item = self.item_at(hov_idx);
                item_lane_segment(&item, hov_lane, hov_half).map(|seg| (seg, hov_lane))
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
                            &state.paragraphs,
                            gutter_total,
                            emphasized_lane_before,
                            emphasized_lane_after,
                        );
                    }
                    Item::File(f) => {
                        // File rows are continuation-only — they sit
                        // under the parent revision's `after` snapshot,
                        // so the after-half emphasis is what applies.
                        self.draw_file(
                            renderer,
                            row_bounds,
                            &f,
                            &state.paragraphs,
                            gutter_total,
                            emphasized_lane_after,
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
    #[allow(clippy::too_many_arguments)]
    fn draw_revision<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        rev: &RevisionRowView,
        paragraphs: &RefCell<Vec<R::Paragraph>>,
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

        let prefix_w = measure_text_width::<R>(
            &rev.change_id_prefix,
            id_size,
            self.style.mono_font,
            paragraphs,
        );
        let suffix_w = measure_text_width::<R>(
            &rev.change_id_suffix,
            id_size,
            self.style.mono_font,
            paragraphs,
        );
        let commit_w = measure_text_width::<R>(
            &rev.commit_id_short,
            id_size,
            self.style.mono_font,
            paragraphs,
        );

        // === Measurements ===
        let id_gap = 8.0;
        let chip_gap = 6.0;
        let at_marker = matches!(rev.selection_key, RowSelectionKey::WorkingCopy);
        let at_gap = 4.0;
        let at_w = if at_marker {
            measure_text_width::<R>("@", id_size, self.style.mono_font, paragraphs)
        } else {
            0.0
        };
        let author_full_w =
            measure_text_width::<R>(&rev.author, id_size, self.style.primary_font, paragraphs);
        let ellipsis_w = measure_text_width::<R>("…", id_size, self.style.mono_font, paragraphs);

        let bm_widths: Vec<f32> = rev
            .bookmark_chips
            .iter()
            .map(|c| self.measure_chip_width::<R>(&c.label, paragraphs))
            .collect();
        let status_widths: Vec<f32> = rev
            .status_chips
            .iter()
            .map(|c| self.measure_chip_width::<R>(&c.label, paragraphs))
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
                    let overflow_chip_w = self.measure_chip_width::<R>(&overflow_label, paragraphs);
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
                    overflow_w = self.measure_chip_width::<R>(&label, paragraphs);
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
        for (chip, bw) in rev
            .bookmark_chips
            .iter()
            .zip(bm_widths.iter())
            .take(visible_bookmarks)
        {
            self.draw_chip(
                renderer,
                paragraphs,
                chip_x,
                title_mid_y,
                &chip.label,
                chip.background,
                chip.text_color,
                chip.border_color,
                chip.border_dashed,
                row_clip,
            );
            chip_x += bw + chip_gap;
        }
        if overflow_count > 0 {
            let label = format!("+{overflow_count}");
            self.draw_chip(
                renderer,
                paragraphs,
                chip_x,
                title_mid_y,
                &label,
                chip_background(overflow_color),
                overflow_color,
                None,
                false,
                row_clip,
            );
            chip_x += overflow_w + chip_gap;
        }
        for (i, chip) in rev.status_chips.iter().enumerate() {
            self.draw_chip(
                renderer,
                paragraphs,
                chip_x,
                title_mid_y,
                &chip.label,
                chip.background,
                chip.text_color,
                chip.border_color,
                chip.border_dashed,
                row_clip,
            );
            chip_x += status_widths[i] + chip_gap;
        }

        // Reserve room on the right of the description for the collapse/
        // expand chevron when this is the selected row. The chevron sizes
        // up past the description so the affordance reads at a glance —
        // at the description's own size it shrinks to ~5px on the y axis
        // and disappears against the row chrome.
        let chevron_glyph = rev
            .collapse_chevron
            .map(|expanded| if expanded { "\u{25BE}" } else { "\u{25B8}" });
        let chevron_size = SMALL_TEXT_SIZE + 6.0;
        let chevron_width = chevron_glyph
            .map(|glyph| {
                measure_text_width::<R>(glyph, chevron_size, self.style.primary_font, paragraphs)
            })
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
                self.style.primary_font,
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
            &self.style.graph,
            node_override,
            emphasized_lane_before,
            emphasized_lane_after,
        );
    }

    fn draw_file<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        f: &FileRowView,
        _paragraphs: &RefCell<Vec<R::Paragraph>>,
        gutter_total: f32,
        emphasized_lane: Option<usize>,
    ) where
        R: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
    {
        let selected = self
            .selected_file
            .map(|idx| idx == f.file_index)
            .unwrap_or(false);

        if selected {
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

        let content_x = row_bounds.x + gutter_total;
        let row_clip = row_bounds;
        let row_mid_y = row_bounds.y + row_bounds.height / 2.0;

        // Status badge — vertically centered on the row mid-line.
        let badge_w = self.style.file_badge_width;
        let badge_h = 14.0;
        let badge_y = row_mid_y - badge_h / 2.0;
        fill_quad(
            renderer,
            Rectangle {
                x: content_x,
                y: badge_y,
                width: badge_w,
                height: badge_h,
            },
            f.status_background,
            INDICATOR_RADIUS,
        );
        fill_text_centered_y(
            renderer,
            &f.status_label,
            content_x + badge_w / 2.0,
            row_mid_y,
            badge_w,
            FILE_BADGE_TEXT_SIZE,
            f.status_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Center,
        );

        // Layout: [badge] gap [path] gap [+N width=additions_width] gap [-N width=deletions_width] right_pad
        let row_gap = FILE_ROW_GAP;
        let right_pad = FILE_ROW_RIGHT_PAD;
        let minus_x = row_bounds.x + row_bounds.width - f.deletions_width - right_pad;
        let plus_x = minus_x - row_gap - f.additions_width;

        // Path: primary line + (optional) secondary parent line.
        let path_x = content_x + badge_w + row_gap;
        let path_w = (plus_x - path_x - row_gap).max(1.0);

        // `fill_text_truncated` applies an end-ellipsis at the renderer level.
        // The display models from main.rs already pick a smart truncation
        // (preserves the basename, middle-ellipses the prefix); the renderer
        // ellipsis is a safety net for the cases that logic can't squeeze —
        // e.g. a basename that itself is wider than `path_w` — so the text
        // doesn't bleed visually into the +N / -N columns to the right.
        if f.secondary.is_empty() {
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
        } else {
            let secondary_size = (CAPTION_TEXT_SIZE - 2.0).max(9.0);
            // Stack two text rows visually centered on the row mid-line.
            let primary_mid = row_mid_y - secondary_size * 0.55;
            let secondary_mid = row_mid_y + CAPTION_TEXT_SIZE * 0.55;
            fill_text_truncated(
                renderer,
                &f.primary,
                path_x,
                primary_mid,
                path_w,
                CAPTION_TEXT_SIZE,
                f.primary_color,
                self.style.primary_font,
                row_clip,
            );
            fill_text_truncated(
                renderer,
                &f.secondary,
                path_x,
                secondary_mid,
                path_w,
                secondary_size,
                f.secondary_color,
                self.style.primary_font,
                row_clip,
            );
        }

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
            f.deletions_width + right_pad,
            CAPTION_TEXT_SIZE,
            f.deletions_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Left,
        );

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
            &self.style.graph,
            emphasized_lane,
        );
    }

    fn measure_chip_width<R: text::Renderer<Font = Font>>(
        &self,
        label: &str,
        paragraphs: &RefCell<Vec<R::Paragraph>>,
    ) -> f32 {
        let size = CAPTION_TEXT_SIZE;
        let label_w = measure_text_width::<R>(label, size, self.style.primary_font, paragraphs);
        let pad_x = 5.0;
        label_w + pad_x * 2.0
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chip<R>(
        &self,
        renderer: &mut R,
        paragraphs: &RefCell<Vec<R::Paragraph>>,
        x: f32,
        center_y: f32,
        label: &str,
        background: Color,
        text_color: Color,
        border_color: Option<Color>,
        border_dashed: bool,
        clip: Rectangle,
    ) -> f32
    where
        R: text::Renderer<Font = Font> + geometry::Renderer,
    {
        let size = CAPTION_TEXT_SIZE;
        let label_w = measure_text_width::<R>(label, size, self.style.primary_font, paragraphs);
        let pad_x = 5.0;
        // Tight box: just enough vertical room for the cap-height plus a hair
        // of breathing room. Anything more makes the chip dwarf the title text.
        let chip_h = (size + 3.0).round();
        let chip_w = label_w + pad_x * 2.0;
        let chip_top = (center_y - chip_h / 2.0).round();
        let rect = Rectangle {
            x,
            y: chip_top,
            width: chip_w,
            height: chip_h,
        };
        // Skip the fill when the chip is meant to read as outlined-only —
        // a translucent fill with `a == 0` would still emit a quad but
        // avoiding it keeps the layer count down and makes intent clear.
        if background.a > f32::EPSILON {
            fill_quad(renderer, rect, background, INDICATOR_RADIUS);
        }
        if let Some(border_color) = border_color {
            if border_dashed {
                stroke_dashed_rounded_rect(renderer, rect, border_color, INDICATOR_RADIUS);
            } else {
                stroke_solid_rounded_rect(renderer, rect, border_color, INDICATOR_RADIUS);
            }
        }
        // Let iced center the glyphs vertically within the chip via align_y.
        fill_text_centered_y(
            renderer,
            label,
            x + chip_w / 2.0,
            center_y,
            chip_w,
            size,
            text_color,
            self.style.primary_font,
            clip,
            text::Alignment::Center,
        );
        chip_w
    }
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

fn fill_quad<R: renderer::Renderer>(renderer: &mut R, rect: Rectangle, color: Color, radius: f32) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border {
                radius: iced::border::Radius::from(radius),
                ..Border::default()
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(color),
    );
}

/// Solid 1px rounded-rect outline. Cheaper than the dashed variant —
/// goes through iced's quad border instead of a geometry path. Used by
/// remote-bookmark chips that want a crisp outlined look.
fn stroke_solid_rounded_rect<R: renderer::Renderer>(
    renderer: &mut R,
    rect: Rectangle,
    color: Color,
    radius: f32,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border {
                color,
                width: 1.0,
                radius: iced::border::Radius::from(radius),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(Color::TRANSPARENT),
    );
}

/// Dashed 1px rounded-rect outline. Used by outlined chips (e.g. the
/// `empty` indicator). Iced's quad border can't be dashed, so we stroke
/// a canvas path with a `LineDash` pattern.
fn stroke_dashed_rounded_rect<R: geometry::Renderer>(
    renderer: &mut R,
    rect: Rectangle,
    color: Color,
    radius: f32,
) {
    // Inset by half a pixel so the 1px stroke sits on the rect's
    // edge rather than straddling it (avoids fuzzy aliasing).
    let inset_rect = Rectangle {
        x: rect.x + 0.5,
        y: rect.y + 0.5,
        width: rect.width - 1.0,
        height: rect.height - 1.0,
    };
    let path = Path::rounded_rectangle(
        Point::new(inset_rect.x, inset_rect.y),
        Size::new(inset_rect.width, inset_rect.height),
        iced::border::Radius::from(radius),
    );
    let segments: [f32; 2] = [4.0, 2.2];
    let mut stroke = Stroke::default()
        .with_color(color)
        .with_width(1.0)
        .with_line_cap(LineCap::Butt)
        .with_line_join(LineJoin::Miter);
    stroke.line_dash = geometry::LineDash {
        segments: &segments,
        offset: 0,
    };
    let mut frame = Frame::new(
        renderer,
        Size::new(rect.x + rect.width + 1.0, rect.y + rect.height + 1.0),
    );
    frame.stroke(&path, stroke);
    renderer.draw_geometry(frame.into_geometry());
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
    let height = size * 1.4;
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

/// Build a `Paragraph` whose `min_bounds()` reflect the actual rendered
/// width. Used purely for measurement so chip/id placement stops relying on
/// a `chars * size * 0.55` heuristic that both over- and under-shot and
/// caused glyphs to clip into ellipses.
fn make_paragraph<R: text::Renderer<Font = Font>>(
    content: &str,
    size: f32,
    font: Font,
) -> R::Paragraph {
    let line_height = size * 1.4;
    R::Paragraph::with_text(Text {
        content,
        bounds: Size::new(f32::INFINITY, line_height.max(1.0)),
        size: Pixels(size),
        line_height: text::LineHeight::Absolute(Pixels(line_height)),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    })
}

/// Measure the rendered width of `content` using a real `Paragraph`. The
/// paragraph is stashed in the widget's cache so its backing buffer outlives
/// the draw frame (otherwise iced's `Weak` reference upgrade fails when the
/// renderer flushes).
fn measure_text_width<R: text::Renderer<Font = Font>>(
    content: &str,
    size: f32,
    font: Font,
    paragraphs: &RefCell<Vec<R::Paragraph>>,
) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    let para = make_paragraph::<R>(content, size, font);
    let width = para.min_width();
    paragraphs.borrow_mut().push(para);
    width
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
    let height = size * 1.4;
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
    /// Pre-measured text size (computed from a real `Paragraph` in
    /// `overlay()`), so layout uses true glyph extents instead of a
    /// `chars * size * 0.55` guess that left a gap on the right.
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
