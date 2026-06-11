use std::cell::RefCell;
use std::time::Instant;

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{
    Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size, Theme,
    Vector, alignment, keyboard, window,
};

use crate::scrollbar::{self, ScrollbarState, ScrollbarStyle};

// Row height is `text_size * ROW_HEIGHT_RATIO` rounded to the nearest int,
// so glyphs (which iced renders at `text_size * 1.4` line height) sit
// inside the row with a few px of breathing room above and below. 1.85
// gives 24px at the default 13pt code font, matching the historical fixed
// row height, and scales linearly when the caller passes a larger size.
const ROW_HEIGHT_RATIO: f32 = 1.85;
// Padding above and below the centered title row inside the file-header strip.
const FILE_HEADER_VPAD: f32 = 8.0;
// Padding above and below the centered title row inside the hunk-header strip.
const HUNK_HEADER_VPAD: f32 = 1.0;
const PREFIX_WIDTH: f32 = 24.0;
const TEXT_X_PADDING: f32 = 8.0;
const TEXT_Y_PADDING: f32 = 2.0;
// Rows advanced per wheel-notch line on Linux/Windows (macOS trackpads take the
// pixel path below). Above the ~3-line OS default — browsing a long diff with a
// wheel felt sluggish otherwise.
const LINE_SCROLL_ROWS: f32 = 5.0;
const PIXEL_SCROLL_SCALE: f32 = 0.65;
// Floor for the gutter so two single-digit columns still look intentional.
const GUTTER_MIN_WIDTH: f32 = 56.0;
// Padding flanking the gutter text on both sides.
const GUTTER_HORIZONTAL_PADDING: f32 = 8.0;
// Padding above and below the revision-header block when it's present.
const HEADER_VERTICAL_PADDING: f32 = 10.0;
// Left/right padding inside the header block (between the panel edge and
// the first character of label/description text).
const HEADER_HORIZONTAL_PADDING: f32 = 14.0;
// Space drawn between the label column and the value column so the colon
// sits a hair away from the value text.
const HEADER_LABEL_GAP: f32 = 6.0;
// Description lines are indented by this many monospace columns under the
// metadata block (matches the four-space indent jj uses).
const HEADER_DESCRIPTION_INDENT: f32 = 4.0;
// Click within this distance of the previous click counts as a multi-click
// rather than a fresh selection anchor.
const MULTI_CLICK_RADIUS: f32 = 4.0;
// Auto-scroll speed (px/sec) when the mouse is dragged just outside the
// viewport. Scaled by how far past the edge the cursor sits.
const AUTO_SCROLL_MAX_SPEED: f32 = 1200.0;
// Width of the "edge zone" outside the viewport over which the auto-scroll
// speed ramps up. The cursor sitting just past the edge scrolls slowly; a
// cursor far outside the viewport scrolls at full speed.
const AUTO_SCROLL_RAMP_PX: f32 = 80.0;

// The diff data model lives in `diffui_core`; re-export the types this widget
// renders so `crate::diff_view::DiffLine` etc. still resolve for callers.
pub use diffui_core::{DiffHunkView, DiffLine, DiffLineKind, SyntaxKind, SyntaxSpan};

#[derive(Debug, Clone)]
pub struct DiffFileView<'a> {
    pub title: String,
    pub status: &'a str,
    pub hunks: &'a [DiffHunkView],
    pub additions: usize,
    pub deletions: usize,
}

/// One line of the `jj show`-style revision header rendered at the top of
/// the diff scroll area. Kept as a plain enum so `main.rs` decides the text
/// content without leaking `RevisionDetails` into this module.
#[derive(Debug, Clone)]
pub enum HeaderLine {
    /// "Label: value" — label colored muted, value colored as text. The
    /// `label` field is the rendered label including its trailing colon
    /// (e.g. `"Commit ID:"`), padded to the column width by the caller.
    Field { label: String, value: String },
    /// Bookmarks rendered as colored chips that match the sidebar. The chips'
    /// colors are resolved by the caller (the selected commit's lane color);
    /// remote `name@remote` bookmarks render outlined.
    Bookmarks {
        label: String,
        chips: Vec<HeaderChip>,
    },
    /// A line of the description, rendered indented under the metadata
    /// block. Stored without indentation; the renderer prepends four
    /// spaces.
    Description(String),
    /// Blank separator between the metadata block and the description.
    Blank,
}

/// One bookmark chip in the revision header, with its styling pre-resolved so
/// the renderer just paints it. Mirrors the sidebar's `IndicatorChip`.
#[derive(Debug, Clone)]
pub struct HeaderChip {
    pub label: String,
    pub fill: Color,
    pub text: Color,
    /// `Some` for outlined (remote) chips; `None` for filled (local) ones.
    pub border: Option<Color>,
}

impl HeaderLine {
    /// Build a metadata row with the label padded to nine characters — the
    /// width of "Commit ID" / "Committer" / "Signature", the longest labels
    /// jj ships — so colons stack in a column across the block.
    pub fn field(label: &str, value: &str) -> Self {
        Self::Field {
            label: format!("{label:<9}:"),
            value: value.to_owned(),
        }
    }

    /// Bookmarks row — `label` padded like `field` so it aligns with the
    /// metadata column.
    pub fn bookmarks(label: &str, chips: Vec<HeaderChip>) -> Self {
        Self::Bookmarks {
            label: format!("{label:<9}:"),
            chips,
        }
    }

    pub fn description(line: &str) -> Self {
        Self::Description(line.to_owned())
    }

    pub fn blank() -> Self {
        Self::Blank
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub text: Color,
    pub text_muted: Color,
    pub addition_text: Color,
    pub deletion_text: Color,
    pub modified_token: Color,
    pub conflict_marker: Color,
    pub note_text: Color,
    pub panel: Color,
    pub file_header: Color,
    pub hunk_header: Color,
    pub addition_background: Color,
    pub deletion_background: Color,
    pub note_background: Color,
    pub gutter_background: Color,
    pub border: Color,
    /// Background tint drawn under selected text.
    pub selection: Color,
    pub scrollbar: ScrollbarStyle,
}

pub struct DiffView<'a, Message> {
    files: Vec<DiffFileView<'a>>,
    selected_file: usize,
    revision_key: String,
    palette: Palette,
    font: Font,
    text_size: f32,
    multi_click_ms: u64,
    metrics: LayoutMetrics,
    header: Vec<HeaderLine>,
    on_selected_file_changed: fn(usize) -> Message,
    on_copy: Option<fn(String) -> Message>,
    /// In-diff find highlights. `None` when the find bar is closed.
    find: Option<FindOverlay<'a>>,
    /// Reports the scroll offset whenever it changes, so the app can persist it
    /// per-tab. `None` ⇒ scroll position isn't tracked.
    on_scroll: Option<fn(f32) -> Message>,
    /// One-shot scroll restore: when `restore_token` differs from the value the
    /// widget last saw, the offset jumps to `restore_offset` (clamped),
    /// overriding the reset-to-top a `revision_key` change would otherwise do.
    /// Used to re-apply a tab's saved scroll on activation.
    restore_offset: f32,
    restore_token: u64,
    /// Monotonic version of the diff content's *paint*, bumped by the app on
    /// every document swap and on background-highlight merges. The per-line
    /// paragraph cache is keyed by `(file, hunk, line)`, which point at
    /// *different* text after a reload, a working-copy edit, or a tab switch
    /// (the widget `State` — cache included — is shared across tabs).
    /// `revision_key` alone can't catch those: it's a constant
    /// `"working-copy"` for any `@`. A version bump drops the cache.
    content_version: u64,
    /// Monotonic identity of the document's *layout* (the app's per-document
    /// id), bumped only when the document is replaced — not when highlight
    /// spans merge in. Keys the [`HeightIndex`]: span merges repaint rows but
    /// never move them, so rebuilding the (potentially 1M-row) index for each
    /// would be pure waste.
    layout_version: u64,
}

/// Per-render find-match data fed into `DiffView::with_find`. The widget
/// renders one rectangle per match on the lines it overlaps and scrolls
/// the active match into view when `scroll_token` changes between
/// renders.
#[derive(Debug, Clone)]
pub struct FindOverlay<'a> {
    pub matches: &'a [crate::find::FindMatch],
    pub active: Option<usize>,
    /// Bumped by the caller whenever the active match changes. The widget
    /// keeps the previous token in its `State` and scrolls when the two
    /// disagree — this is how we trigger scroll without tearing into the
    /// widget's private state from outside.
    pub scroll_token: u64,
    pub highlight: Color,
    pub active_highlight: Color,
}

/// Sizes derived once at widget-construction time from the caller's
/// `text_size`. Replaces the historical fixed `self.metrics.row_height` /
/// `self.metrics.file_header_height` / `self.metrics.hunk_header_height` constants so the diff view
/// stays visually proportional under a non-default code font size.
#[derive(Debug, Clone, Copy)]
struct LayoutMetrics {
    row_height: f32,
    file_header_height: f32,
    hunk_header_height: f32,
    gutter_width: f32,
    gutter_digit_count: usize,
    /// Real monospace glyph advance for the diff font + size, measured
    /// once via cosmic_text at construction. Wrap-line counting (in
    /// `row_height`) and selection geometry (in `draw()` /
    /// `position_at_point`) both rely on this — agreeing on a single
    /// value keeps row heights aligned with where iced actually breaks
    /// long lines, so we don't reserve an empty wrap row before the
    /// renderer actually wraps.
    char_width: f32,
}

impl LayoutMetrics {
    fn new(text_size: f32, gutter_digit_count: usize, font: Font) -> Self {
        let row_height = (text_size * ROW_HEIGHT_RATIO).round();
        let file_header_height = row_height + 2.0 * FILE_HEADER_VPAD;
        let hunk_header_height = row_height + 2.0 * HUNK_HEADER_VPAD;
        // Headless cosmic_text shaping gives the actual glyph advance, so
        // the row-height wrap math matches iced's renderer instead of the
        // historical `text_size * 0.62` heuristic that consistently
        // under-counted chars-per-line and produced phantom trailing
        // wrap rows just before the renderer hit its real break point.
        // Cached per (font, size) — iced rebuilds the widget on every
        // `view()` cycle, and uncached this re-shapes "M" each time
        // (~40µs release / ~450µs debug per rebuild).
        let char_width = measure_char_advance_cached(font, text_size).max(1.0);
        let gutter_text_chars = gutter_digit_count * 2 + 1; // two columns + one separating space
        let gutter_width = (gutter_text_chars as f32 * char_width
            + GUTTER_HORIZONTAL_PADDING * 2.0)
            .max(GUTTER_MIN_WIDTH);
        Self {
            row_height,
            file_header_height,
            hunk_header_height,
            gutter_width,
            gutter_digit_count,
            char_width,
        }
    }
}

/// Cache key for per-line shaped `Paragraph`s. The `(file, hunk, line)`
/// triple maps to stable line content under a fixed `revision_key`
/// (`diff()` clears the cache when that changes). `content_width_bits`
/// invalidates entries when the panel is resized — wrap points move and
/// the old layout would no longer match the renderer's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ParagraphKey {
    file_index: u32,
    hunk_index: u32,
    line_index: u32,
    content_width_bits: u32,
}

/// Prefix-sum layout index over the rendered document — the "position
/// checkpoint" idea from pierre.computer's diff-rendering writeup. Built once
/// per `(content shape, wrap width)` and consulted by every height/position
/// query, so scroll frames, file jumps, and hit tests are O(log n) binary
/// searches instead of O(total lines) walks re-counting every line's chars
/// (which at ~1M lines costs hundreds of ms *per frame*).
///
/// ~16 bytes per row — ~16 MB at 1M lines, an order of magnitude under the
/// lines themselves.
#[derive(Debug, Default)]
struct HeightIndex {
    /// Rebuild key: `(layout_version, files.len(), header.len(),
    /// content_width bits)`. The file/header counts are part of the key
    /// because streaming PR loads *append* files (and the header appears when
    /// `gh pr view` lands) without replacing the document — appends leave
    /// existing rows in place, but they extend the layout. Keyed on the
    /// layout id, NOT the paint `content_version`: highlight merges bump the
    /// latter to re-shape paint without moving anything.
    key: Option<(u64, usize, usize, u32)>,
    /// Content-space y of each file's header top, plus one trailing sentinel
    /// holding the content end. `file_tops[0]` equals the revision-header
    /// height.
    file_tops: Vec<f32>,
    /// Content-space y of each hunk header, in document order.
    hunk_tops: Vec<f32>,
    /// `(file, hunk)` of each `hunk_tops` entry.
    hunk_ids: Vec<(u32, u32)>,
    /// Content-space y of each diff row, in document order.
    row_tops: Vec<f32>,
    /// `(file, hunk, line)` of each `row_tops` entry. Document order makes
    /// this lexicographically sorted, so a row is also findable *by id*.
    row_ids: Vec<(u32, u32, u32)>,
    /// Total content height (revision header + every file).
    total_height: f32,
}

impl HeightIndex {
    /// Index into `row_tops`/`row_ids` of the last row starting at or above
    /// `target_y` — the candidate row containing that y (the caller checks
    /// the row's actual height; `target_y` may sit in a header band).
    fn row_at(&self, target_y: f32) -> Option<usize> {
        self.row_tops
            .partition_point(|&top| top <= target_y)
            .checked_sub(1)
    }

    /// Index of row `(file, hunk, line)`, by binary search over the sorted ids.
    fn row_index_of(&self, file: usize, hunk: usize, line: usize) -> Option<usize> {
        self.row_ids
            .binary_search(&(file as u32, hunk as u32, line as u32))
            .ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionUnit {
    Character,
    Word,
    Line,
}

struct State<Paragraph> {
    selected_file: usize,
    revision_key: String,
    pending_file_jump: Option<usize>,
    /// (file, hunk, line, byte_in_line) of a match we should scroll into
    /// view on the next pass. Set in `diff()` when the incoming find
    /// scroll_token differs from `last_find_scroll_token`; consumed in
    /// `update()` where layout bounds are available.
    pending_find_scroll: Option<(usize, usize, usize, usize)>,
    /// Last find scroll token we acted on. The widget compares this against
    /// the incoming `FindOverlay::scroll_token` and, when they disagree,
    /// schedules a scroll. Wrapping `u64` is fine — only equality matters.
    last_find_scroll_token: Option<u64>,
    vertical_offset: f32,
    /// Shaped `Paragraph`s for syntax-highlighted code lines, keyed by
    /// `(file, hunk, line, content_width)`. Reused across frames during
    /// scrolling — `with_spans` shaping is ~280µs/row in release and
    /// dominates the per-frame draw cost on dense diffs. Cleared in
    /// `diff()` whenever `revision_key` changes (line indices stop
    /// pointing at the same text) and unused entries are evicted at the
    /// end of each `draw()`.
    paragraph_cache: RefCell<std::collections::HashMap<ParagraphKey, Paragraph>>,
    /// Anchor (mouse-down position) of the in-progress or completed selection.
    selection_anchor: Option<TextPosition>,
    /// Focus (current/release position) of the selection.
    selection_focus: Option<TextPosition>,
    /// When the user drags after a double/triple click, the selection grows
    /// in word- or line-sized chunks instead of by character. We remember
    /// where the anchor "click unit" started/ended so the resulting
    /// selection always covers full units in both directions.
    selection_anchor_unit_start: Option<TextPosition>,
    selection_anchor_unit_end: Option<TextPosition>,
    selection_unit: SelectionUnit,
    /// True while the user is mid-drag.
    is_selecting: bool,
    /// Multi-click bookkeeping: timestamp + screen position + count of the
    /// last left-button press, so a second/third press in quick succession
    /// at the same spot upgrades the selection to word/line scope.
    last_click_time: Option<Instant>,
    last_click_screen: Option<Point>,
    click_count: u32,
    /// Last cursor position observed while dragging — used to drive
    /// auto-scroll and to reproject the selection focus after the viewport
    /// shifts under the cursor.
    last_drag_cursor: Option<Point>,
    scrollbar: ScrollbarState,
    /// Most recent `restore_token` acted on; a change schedules a one-shot jump
    /// to the caller's `restore_offset`, consumed in `update()` where bounds
    /// clamp. Starts at 0 to match the caller's initial token.
    last_restore_token: u64,
    /// Offset to jump to on the next `update()` pass, set when `restore_token`
    /// changed. Overrides the `revision_key`-change reset and file jump;
    /// clamped once bounds are known.
    pending_set_offset: Option<f32>,
    /// Most recent `content_version` acted on. A change drops the paragraph
    /// cache (whose keys would otherwise map to stale text). Starts at 0 to
    /// match the app's initial version.
    last_content_version: u64,
    /// Prefix-sum layout index (see [`HeightIndex`]). `RefCell` because
    /// `draw`/`mouse_interaction` only get `&State` but must be able to
    /// (re)build it lazily; borrows are short and never overlap.
    height_index: RefCell<HeightIndex>,
}

/// Stable cursor position inside the diff document. We index by
/// `(file, hunk, line)` instead of by screen y so the position stays valid
/// across scrolling, file selection, and re-renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPosition {
    /// Which part of the document the position is in. Ordered first so every
    /// header position sorts before every body position — the revision header
    /// sits above the diff body.
    region: Region,
    /// `Body`: the file index. Unused (`0`) for `Header`.
    file_index: usize,
    /// `Body`: the hunk index. Unused (`0`) for `Header`.
    hunk_index: usize,
    /// `Body`: the line within the hunk. `Header`: the header-line index.
    line_index: usize,
    /// Byte offset within the line's selectable text. Bytes (not chars) so we
    /// can slice the source string directly when copying.
    byte: usize,
}

/// Which selectable region of the diff document a `TextPosition` lives in.
/// `Header` (the `jj show`-style revision metadata) sorts before `Body` (the
/// file diffs), matching their visual stacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Region {
    Header,
    Body,
}

fn header_position(line_index: usize, byte: usize) -> TextPosition {
    TextPosition {
        region: Region::Header,
        file_index: 0,
        hunk_index: 0,
        line_index,
        byte,
    }
}

fn body_position(
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    byte: usize,
) -> TextPosition {
    TextPosition {
        region: Region::Body,
        file_index,
        hunk_index,
        line_index,
        byte,
    }
}

#[derive(Debug, Clone, Copy)]
struct RowRenderParams {
    bounds: Rectangle,
    content_clip_bounds: Rectangle,
    y: f32,
    height: f32,
    content_width: f32,
}

#[derive(Debug, Clone, Copy)]
struct TextRenderParams {
    width: f32,
    height: f32,
    position: Point,
    color: Color,
    clip_bounds: Rectangle,
    wrapping: text::Wrapping,
}

#[derive(Debug, Clone, Copy)]
struct VisibleHunkHeader {
    file_index: usize,
    hunk_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleFileHeader {
    file_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleRow {
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    y: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleBand {
    kind: DiffLineKind,
    y: f32,
    height: f32,
}

impl<'a, Message> DiffView<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        files: Vec<DiffFileView<'a>>,
        selected_file: usize,
        revision_key: impl Into<String>,
        palette: Palette,
        font: Font,
        text_size: f32,
        multi_click_ms: u64,
        on_selected_file_changed: fn(usize) -> Message,
    ) -> Self {
        let gutter_digit_count = compute_gutter_digit_count(&files);
        let metrics = LayoutMetrics::new(text_size, gutter_digit_count, font);
        Self {
            files,
            selected_file,
            revision_key: revision_key.into(),
            palette,
            font,
            text_size,
            multi_click_ms,
            metrics,
            header: Vec::new(),
            on_selected_file_changed,
            on_copy: None,
            find: None,
            on_scroll: None,
            restore_offset: 0.0,
            restore_token: 0,
            content_version: 0,
            layout_version: 0,
        }
    }

    pub fn with_header(mut self, header: Vec<HeaderLine>) -> Self {
        self.header = header;
        self
    }

    /// Report scroll-offset changes so the caller can persist the position.
    /// See [`Self::restore_scroll`] for the inverse.
    pub fn on_scroll(mut self, callback: fn(f32) -> Message) -> Self {
        self.on_scroll = Some(callback);
        self
    }

    /// Jump the scroll offset to `offset` the next time `token` changes. Calling
    /// with the same `token` twice is a no-op, so live scrolling is preserved.
    /// Wins over the `revision_key`-change reset in the same render — a tab
    /// restore wants the exact saved offset, not the top.
    pub fn restore_scroll(mut self, offset: f32, token: u64) -> Self {
        self.restore_offset = offset;
        self.restore_token = token;
        self
    }

    /// Set the diff content's version. A change since the widget last saw it
    /// drops the per-line paragraph cache (see the field docs). The app bumps
    /// this whenever it swaps the displayed document.
    pub fn content_version(mut self, version: u64) -> Self {
        self.content_version = version;
        self
    }

    /// Set the document's layout identity (see the field docs) — keys the
    /// height index, unlike `content_version` which keys the paint cache.
    /// Span merges bump only the latter, so the (potentially 1M-row) index
    /// survives them.
    pub fn layout_version(mut self, version: u64) -> Self {
        self.layout_version = version;
        self
    }

    pub fn on_copy(mut self, on_copy: fn(String) -> Message) -> Self {
        self.on_copy = Some(on_copy);
        self
    }

    pub fn with_find(mut self, find: FindOverlay<'a>) -> Self {
        self.find = Some(find);
        self
    }

    /// Total height of the revision header block (metadata lines +
    /// description + vertical padding). Zero when no header is set.
    fn header_height(&self) -> f32 {
        if self.header.is_empty() {
            0.0
        } else {
            self.header.len() as f32 * self.metrics.row_height + HEADER_VERTICAL_PADDING * 2.0
        }
    }

    /// The selectable text of header line `index`, if any — field *values* and
    /// description lines. Labels, bookmark chips, and blank lines are not.
    fn header_selectable_text(&self, index: usize) -> Option<&str> {
        match self.header.get(index)? {
            HeaderLine::Field { value, .. } => Some(value),
            HeaderLine::Description(line) => Some(line),
            _ => None,
        }
    }

    /// The x where header field values begin — left edge + the (padded) label
    /// column + gap. Mirrors `draw_revision_header`'s layout so hit-testing and
    /// selection rendering land on the same glyphs the renderer drew.
    fn header_value_x(&self, bounds: Rectangle) -> f32 {
        let left_x = bounds.x + HEADER_HORIZONTAL_PADDING;
        let label_width = self
            .header
            .iter()
            .find_map(|line| match line {
                HeaderLine::Field { label, .. } => Some(self.text_width(label)),
                _ => None,
            })
            .unwrap_or(0.0);
        left_x + label_width + HEADER_LABEL_GAP
    }

    /// Screen-x where header line `index`'s selectable text starts: the value
    /// column for fields, or the indented column for description lines.
    fn header_text_origin_x(&self, index: usize, bounds: Rectangle) -> f32 {
        match self.header.get(index) {
            Some(HeaderLine::Description(_)) => {
                bounds.x
                    + HEADER_HORIZONTAL_PADDING
                    + HEADER_DESCRIPTION_INDENT * self.metrics.char_width
            }
            _ => self.header_value_x(bounds),
        }
    }

    /// Paint the selection tint behind a header line's selectable `text` at
    /// `origin_x` / `y`. Drawn before the text so the glyphs sit on top, like
    /// the diff body. No-op when nothing is selected on this line.
    fn draw_header_value_selection<Renderer>(
        &self,
        renderer: &mut Renderer,
        line_index: usize,
        text: &str,
        origin_x: f32,
        y: f32,
        selection: Option<(TextPosition, TextPosition)>,
    ) where
        Renderer: renderer::Renderer,
    {
        let Some((sel_start, sel_end)) = selection else {
            return;
        };
        let pos_start = header_position(line_index, 0);
        let pos_end = header_position(line_index, text.len());
        if pos_end < sel_start || pos_start >= sel_end {
            return;
        }
        let start_byte = if pos_start < sel_start {
            sel_start.byte
        } else {
            0
        };
        let end_byte = if pos_end > sel_end {
            sel_end.byte
        } else {
            text.len()
        };
        let start_chars = char_count_at_byte(text, start_byte);
        let end_chars = char_count_at_byte(text, end_byte);
        if end_chars > start_chars {
            let cw = self.metrics.char_width;
            self.draw_background(
                renderer,
                origin_x + start_chars as f32 * cw,
                y,
                (end_chars - start_chars) as f32 * cw,
                self.metrics.row_height,
                self.palette.selection,
            );
        }
    }

    /// Expand `pos` to cover the unit (word or line) that contains it, in
    /// either the header or the body. Character mode is a no-op.
    fn expand_to_unit(
        &self,
        pos: TextPosition,
        unit: SelectionUnit,
    ) -> (TextPosition, TextPosition) {
        let text: &str = match pos.region {
            Region::Header => match self.header_selectable_text(pos.line_index) {
                Some(text) => text,
                None => return (pos, pos),
            },
            Region::Body => match self
                .files
                .get(pos.file_index)
                .and_then(|file| file.hunks.get(pos.hunk_index))
                .and_then(|hunk| hunk.lines.get(pos.line_index))
            {
                Some(line) => &line.content,
                None => return (pos, pos),
            },
        };

        match unit {
            SelectionUnit::Character => (pos, pos),
            SelectionUnit::Line => (
                TextPosition { byte: 0, ..pos },
                TextPosition {
                    byte: text.len(),
                    ..pos
                },
            ),
            SelectionUnit::Word => {
                let (start_byte, end_byte) = word_bounds(text, pos.byte);
                (
                    TextPosition {
                        byte: start_byte,
                        ..pos
                    },
                    TextPosition {
                        byte: end_byte,
                        ..pos
                    },
                )
            }
        }
    }

    /// Rebuild `cell`'s [`HeightIndex`] if the content shape or wrap width
    /// changed since it was built. One O(total lines) pass per change — every
    /// per-frame query then reads prefix sums instead of re-walking.
    fn ensure_height_index(&self, cell: &RefCell<HeightIndex>, width: f32) {
        let content_width = self.content_width(width);
        let key = Some((
            self.layout_version,
            self.files.len(),
            self.header.len(),
            content_width.to_bits(),
        ));
        if cell.borrow().key == key {
            return;
        }

        let mut index = cell.borrow_mut();
        let row_count: usize = self
            .files
            .iter()
            .map(|file| file.hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>())
            .sum();
        index.file_tops.clear();
        index.file_tops.reserve(self.files.len() + 1);
        index.hunk_tops.clear();
        index.hunk_ids.clear();
        index.row_tops.clear();
        index.row_tops.reserve(row_count);
        index.row_ids.clear();
        index.row_ids.reserve(row_count);

        let mut y = self.header_height();
        for (file_index, file) in self.files.iter().enumerate() {
            index.file_tops.push(y);
            y += self.metrics.file_header_height;
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                index.hunk_tops.push(y);
                index.hunk_ids.push((file_index as u32, hunk_index as u32));
                y += self.metrics.hunk_header_height;
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    index.row_tops.push(y);
                    index
                        .row_ids
                        .push((file_index as u32, hunk_index as u32, line_index as u32));
                    y += self.row_height(line, content_width);
                }
            }
        }
        index.file_tops.push(y);
        index.total_height = y;
        index.key = key;
    }

    fn file_offset(&self, index: &HeightIndex, file_index: usize) -> f32 {
        index
            .file_tops
            .get(file_index)
            .copied()
            .unwrap_or(index.total_height)
    }

    fn file_at_offset(&self, index: &HeightIndex, offset: f32) -> usize {
        // The trailing sentinel is the content end, not a file.
        let file_count = self.files.len();
        let tops = &index.file_tops[..file_count.min(index.file_tops.len())];
        // Number of file tops at or above `offset`, minus one = the file whose
        // span contains it. An offset in the revision header maps to file 0;
        // past the end clamps to the last file — both as the walk did.
        tops.partition_point(|&top| top <= offset).saturating_sub(1)
    }

    fn content_width(&self, viewport_width: f32) -> f32 {
        (viewport_width - self.metrics.gutter_width - PREFIX_WIDTH - 16.0)
            .max(self.metrics.char_width)
    }

    fn row_height(&self, line: &DiffLine, content_width: f32) -> f32 {
        let chars_per_line = chars_per_visual_line(content_width, self.metrics.char_width);
        let wrapped_lines = line.content.chars().count().max(1).div_ceil(chars_per_line);

        wrapped_lines as f32 * self.metrics.row_height
    }

    /// Y position (in content space, before viewport scroll) of the visual
    /// line containing the byte at `byte_offset` within
    /// `(file_idx, hunk_idx, line_idx)`. Returns `None` if any of the
    /// indices are out of bounds.
    fn match_target_y(
        &self,
        index: &HeightIndex,
        file_idx: usize,
        hunk_idx: usize,
        line_idx: usize,
        byte_offset: usize,
        bounds: Rectangle,
    ) -> Option<f32> {
        let line = self
            .files
            .get(file_idx)?
            .hunks
            .get(hunk_idx)?
            .lines
            .get(line_idx)?;
        let row = index.row_index_of(file_idx, hunk_idx, line_idx)?;
        let mut y = *index.row_tops.get(row)?;

        // Offset within the wrapped row: figure out which visual line the
        // byte sits on so a match on the 5th wrap row of a 200-char line
        // doesn't scroll to the row top and leave the match off-screen.
        let content_width = self.content_width(bounds.width);
        let chars_per_line = chars_per_visual_line(content_width, self.metrics.char_width);
        let char_offset = char_count_at_byte(&line.content, byte_offset);
        let visual_idx = char_offset / chars_per_line;
        y += visual_idx as f32 * self.metrics.row_height;
        Some(y)
    }

    /// Convert a screen point into a `TextPosition` if it falls on a row's
    /// text area. Returns `None` for clicks on the gutter, file/hunk
    /// headers, or empty space below the last row.
    ///
    /// Hit-testing assumes a monospace font (see `char_width`); this is
    /// fine for code text in Menlo/Cascadia Code, but tab characters and
    /// wide glyphs (CJK, emoji) will land slightly off. We accept that
    /// trade-off because real glyph hit-testing would require keeping a
    /// `Paragraph` per visible row alive across the event loop, which iced
    /// doesn't make easy from a custom widget.
    /// Locate the document position under `point`. Unlike a normal
    /// "click to put cursor here" hit-test this clamps to the nearest valid
    /// row even when the click is in the chrome / below the last row, so
    /// dragging the mouse outside the viewport still produces a sensible
    /// selection endpoint.
    fn position_at_point(
        &self,
        index: &HeightIndex,
        point: Point,
        bounds: Rectangle,
        vertical_offset: f32,
    ) -> Option<TextPosition> {
        let content_width = self.content_width(bounds.width);
        let text_x = bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
        let target_y = point.y - bounds.y + vertical_offset;
        let header_height = self.header_height();

        // The header sits above all file content. Field values (Commit ID,
        // Author, …) are selectable; clicks on labels, bookmark chips, or blank
        // lines return `None`.
        if header_height > 0.0 && target_y < header_height {
            let line_offset = (target_y - HEADER_VERTICAL_PADDING) / self.metrics.row_height;
            if line_offset < 0.0 {
                return None;
            }
            let line_index = line_offset.floor() as usize;
            let text = self.header_selectable_text(line_index)?;
            let origin_x = self.header_text_origin_x(line_index, bounds);
            let char_count = text.chars().count();
            let relative_x = (point.x - origin_x).max(0.0);
            let char_offset =
                ((relative_x / self.metrics.char_width + 0.5).floor() as usize).min(char_count);
            return Some(TextPosition {
                region: Region::Header,
                file_index: 0,
                hunk_index: 0,
                line_index,
                byte: byte_offset_for_char(text, char_offset),
            });
        }

        // Candidate row by binary search: the last row starting at or above
        // the target y. The target may instead sit in a file/hunk header band
        // (or past the end) — those fall through to the end-of-document snap
        // below, exactly like the walk did.
        if let Some(row) = index.row_at(target_y) {
            let &(file_index, hunk_index, line_index) = index.row_ids.get(row)?;
            let (file_index, hunk_index, line_index) =
                (file_index as usize, hunk_index as usize, line_index as usize);
            let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
            let row_top = index.row_tops[row];
            let height = self.row_height(line, content_width);
            if target_y < row_top + height {
                // Each row may span multiple wrapped visual lines. Figure out
                // which visual line the click lands on, then translate the
                // horizontal click into a char offset within that visual
                // line's slice of the source content.
                let char_count = line.content.chars().count();
                let cw = self.metrics.char_width;
                let chars_per_line = chars_per_visual_line(content_width, cw);
                let visual_idx = ((target_y - row_top) / self.metrics.row_height).floor() as usize;
                let line_char_start = visual_idx.saturating_mul(chars_per_line);
                let relative_x = (point.x - text_x).max(0.0);
                let local_char = (relative_x / cw + 0.5).floor() as usize;
                let char_offset = (line_char_start + local_char).min(char_count);
                let byte = byte_offset_for_char(&line.content, char_offset);
                return Some(TextPosition {
                    region: Region::Body,
                    file_index,
                    hunk_index,
                    line_index,
                    byte,
                });
            }
        }

        // Not on a row (a header band, or past the last row). Snap to the end
        // of the document so a drag below content selects everything up to it.
        index.row_ids.last().map(|&(file, hunk, line)| {
            let (file_index, hunk_index, line_index) =
                (file as usize, hunk as usize, line as usize);
            let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
            TextPosition {
                region: Region::Body,
                file_index,
                hunk_index,
                line_index,
                byte: line.content.len(),
            }
        })
    }

    /// Build the substring inside the inclusive selection range
    /// `[start, end)`, walking files/hunks/lines in document order so the
    /// pasted text reads naturally regardless of which direction the user
    /// dragged.
    fn collect_selected_text(&self, start: TextPosition, end: TextPosition) -> String {
        if start == end {
            return String::new();
        }
        let mut output = String::new();
        let mut first = true;

        // Slice one logical line's selectable `text` against the selection and
        // append the covered part. Shared by the header and body passes.
        let mut emit = |text: &str, pos_start: TextPosition, pos_end: TextPosition| {
            if pos_end < start || pos_start >= end {
                return;
            }
            let line_start = if pos_start < start { start.byte } else { 0 };
            let line_end = if pos_end > end { end.byte } else { text.len() };
            let line_start = line_start.min(text.len());
            let line_end = line_end.min(text.len()).max(line_start);

            if !first {
                output.push('\n');
            }
            first = false;

            if text.is_char_boundary(line_start) && text.is_char_boundary(line_end) {
                output.push_str(&text[line_start..line_end]);
            }
        };

        // Header field values sort before the body (Region::Header < Body).
        for line_index in 0..self.header.len() {
            if let Some(text) = self.header_selectable_text(line_index) {
                let pos_start = header_position(line_index, 0);
                let pos_end = header_position(line_index, text.len());
                emit(text, pos_start, pos_end);
            }
        }

        // Then the diff body, in document order.
        for (file_index, file) in self.files.iter().enumerate() {
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    let pos_start = body_position(file_index, hunk_index, line_index, 0);
                    let pos_end =
                        body_position(file_index, hunk_index, line_index, line.content.len());
                    emit(&line.content, pos_start, pos_end);
                }
            }
        }
        output
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row<Renderer>(
        &self,
        renderer: &mut Renderer,
        line: &DiffLine,
        render: RowRenderParams,
        cache_key: ParagraphKey,
        paragraph_cache: &RefCell<std::collections::HashMap<ParagraphKey, Renderer::Paragraph>>,
        paragraph_seen: &mut std::collections::HashSet<ParagraphKey>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let text_color = self.line_text_color(line.kind);
        let gutter = format_gutter(
            line.old_line,
            line.new_line,
            self.metrics.gutter_digit_count,
        );
        let prefix = prefix_for_kind(line.kind);
        let bounds = render.bounds;

        self.draw_text(
            renderer,
            &gutter,
            TextRenderParams {
                width: (self.metrics.gutter_width - GUTTER_HORIZONTAL_PADDING * 2.0).max(1.0),
                height: self.metrics.row_height,
                position: Point::new(
                    bounds.x + GUTTER_HORIZONTAL_PADDING,
                    render.y + TEXT_Y_PADDING,
                ),
                color: self.palette.text_muted,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );
        self.draw_text(
            renderer,
            prefix,
            TextRenderParams {
                width: PREFIX_WIDTH,
                height: self.metrics.row_height,
                position: Point::new(
                    bounds.x + self.metrics.gutter_width + TEXT_X_PADDING,
                    render.y + TEXT_Y_PADDING,
                ),
                color: text_color,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );

        let position = Point::new(
            bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
            render.y + TEXT_Y_PADDING,
        );

        // Glyph wrapping (hard column break) instead of `WordOrGlyph`
        // so the renderer's wrap points match our chars-per-line column
        // math exactly. Word-aware wrapping breaks at spaces, which means
        // each visual line ends at a different column than the math
        // predicts — and that's what made selection rectangles on wrapped
        // code drift before/after the true text on the last visual line.
        // For monospaced source code, glyph wrapping is also visually
        // tighter (no ragged whitespace gaps on the right edge).
        self.draw_code_text(
            renderer,
            line,
            TextRenderParams {
                width: render.content_width,
                height: render.height,
                position,
                color: text_color,
                clip_bounds: render.content_clip_bounds,
                wrapping: text::Wrapping::Glyph,
            },
            cache_key,
            paragraph_cache,
            paragraph_seen,
        );
    }

    /// Paint translucent rectangles behind every find match that intersects
    /// the currently visible rows. The active match uses a stronger tint so
    /// it stands out against the surrounding non-active hits.
    fn draw_find_highlights<Renderer>(
        &self,
        renderer: &mut Renderer,
        find: &FindOverlay<'_>,
        visible_rows: &[VisibleRow],
        content_clip_bounds: Rectangle,
        bounds: Rectangle,
        content_width: f32,
    ) where
        Renderer: renderer::Renderer,
    {
        if find.matches.is_empty() {
            return;
        }
        let cw = self.metrics.char_width;
        let chars_per_line = chars_per_visual_line(content_width, cw);
        let text_x = bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
        let max_right = content_clip_bounds.x + content_clip_bounds.width;

        // Single pass over visible rows; for each, find matches landing on
        // it. With small numbers of matches per row this is fine; for
        // pathological cases (e.g. a `\w` regex with thousands of hits) we
        // could pre-sort matches by row and binary-search, but typical
        // queries match a few dozen times max.
        for row in visible_rows {
            let line = &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
            for (match_idx, m) in find.matches.iter().enumerate() {
                if m.file_index != row.file_index
                    || m.hunk_index != row.hunk_index
                    || m.line_index != row.line_index
                {
                    continue;
                }
                if !line.content.is_char_boundary(m.byte_start)
                    || !line
                        .content
                        .is_char_boundary(m.byte_end.min(line.content.len()))
                {
                    continue;
                }
                let color = if find.active == Some(match_idx) {
                    find.active_highlight
                } else {
                    find.highlight
                };
                let start_chars = char_count_at_byte(&line.content, m.byte_start);
                let end_chars =
                    char_count_at_byte(&line.content, m.byte_end.min(line.content.len()));
                let total_chars = line.content.chars().count();
                let visual_lines = total_chars.max(1).div_ceil(chars_per_line);
                for visual_idx in 0..visual_lines {
                    let vline_start = visual_idx * chars_per_line;
                    let vline_end = ((visual_idx + 1) * chars_per_line).min(total_chars);
                    let seg_start = start_chars.max(vline_start);
                    let seg_end = end_chars.min(vline_end);
                    if seg_start >= seg_end {
                        continue;
                    }
                    let mut x = text_x + (seg_start - vline_start) as f32 * cw;
                    let mut width = (seg_end - seg_start) as f32 * cw;
                    if x < content_clip_bounds.x {
                        let trim = content_clip_bounds.x - x;
                        x = content_clip_bounds.x;
                        width = (width - trim).max(0.0);
                    }
                    if x + width > max_right {
                        width = (max_right - x).max(0.0);
                    }
                    if width <= 0.0 {
                        continue;
                    }
                    let y = row.y + visual_idx as f32 * self.metrics.row_height;
                    self.draw_background(renderer, x, y, width, self.metrics.row_height, color);
                }
            }
        }
    }

    fn draw_revision_header<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        visible_top: f32,
        header_height: f32,
        selection: Option<(TextPosition, TextPosition)>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        // Header occupies content_y in `[0, header_height)`. Translate to
        // screen coords for the section that intersects the viewport.
        let header_screen_y = bounds.y - visible_top;

        // Tint the strip so the header reads as distinct chrome.
        self.draw_background(
            renderer,
            bounds.x,
            header_screen_y,
            bounds.width,
            header_height,
            self.palette.file_header,
        );

        // Border across the bottom edge of the header strip, matching the
        // visual treatment of file headers.
        self.draw_background(
            renderer,
            bounds.x,
            header_screen_y + header_height - 1.0,
            bounds.width,
            1.0,
            self.palette.border,
        );

        let clip = bounds;
        let label_color = self.palette.text_muted;
        let value_color = self.palette.text;
        // Measure the label column once. All field labels are padded to the
        // same width in `HeaderLine::field`, so any non-empty one gives us
        // the column extent.
        let label_width = self
            .header
            .iter()
            .find_map(|line| match line {
                HeaderLine::Field { label, .. } => Some(self.text_width(label)),
                _ => None,
            })
            .unwrap_or(0.0);

        let mut y = header_screen_y + HEADER_VERTICAL_PADDING;
        let left_x = bounds.x + HEADER_HORIZONTAL_PADDING;
        let value_x = left_x + label_width + HEADER_LABEL_GAP;
        let value_width = (bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - value_x).max(1.0);

        for (line_index, line) in self.header.iter().enumerate() {
            match line {
                HeaderLine::Field { label, value } => {
                    self.draw_text(
                        renderer,
                        label,
                        TextRenderParams {
                            width: label_width.max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(left_x, y + TEXT_Y_PADDING),
                            color: label_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    self.draw_header_value_selection(
                        renderer, line_index, value, value_x, y, selection,
                    );
                    self.draw_text(
                        renderer,
                        value,
                        TextRenderParams {
                            width: value_width,
                            height: self.metrics.row_height,
                            position: Point::new(value_x, y + TEXT_Y_PADDING),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                }
                HeaderLine::Bookmarks { label, chips } => {
                    self.draw_text(
                        renderer,
                        label,
                        TextRenderParams {
                            width: label_width.max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(left_x, y + TEXT_Y_PADDING),
                            color: label_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    let chip_h = (self.metrics.row_height - 4.0).max(1.0);
                    let chip_y = y + 2.0;
                    let pad_x = 5.0;
                    let gap = 6.0;
                    let right_edge = bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING;
                    let mut chip_x = value_x;
                    for chip in chips {
                        // Tight monospace width — `text_width` adds breathing
                        // room meant for wrapped text, which would bloat the
                        // chip's right side.
                        let label_w = chip.label.chars().count() as f32 * self.metrics.char_width;
                        let chip_w = label_w + pad_x * 2.0;
                        // Drop chips that would overflow the panel rather than
                        // clip them mid-glyph.
                        if chip_x + chip_w > right_edge {
                            break;
                        }
                        self.draw_chip(
                            renderer,
                            Rectangle {
                                x: chip_x,
                                y: chip_y,
                                width: chip_w,
                                height: chip_h,
                            },
                            chip.fill,
                            chip.border,
                        );
                        self.draw_text(
                            renderer,
                            &chip.label,
                            TextRenderParams {
                                width: label_w,
                                height: self.metrics.row_height,
                                position: Point::new(chip_x + pad_x, y + TEXT_Y_PADDING),
                                color: chip.text,
                                clip_bounds: clip,
                                wrapping: text::Wrapping::None,
                            },
                        );
                        chip_x += chip_w + gap;
                    }
                }
                HeaderLine::Description(line) => {
                    // Draw the raw text at an indented origin (rather than a
                    // four-space-prefixed string) so the selectable text and
                    // the hit-test/selection origin line up.
                    let desc_x = left_x + HEADER_DESCRIPTION_INDENT * self.metrics.char_width;
                    self.draw_header_value_selection(
                        renderer, line_index, line, desc_x, y, selection,
                    );
                    self.draw_text(
                        renderer,
                        line,
                        TextRenderParams {
                            width: (bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - desc_x)
                                .max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(desc_x, y + TEXT_Y_PADDING),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                }
                HeaderLine::Blank => {}
            }
            y += self.metrics.row_height;
        }
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for DiffView<'a, Message>
where
    Renderer: text::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph> {
            selected_file: self.selected_file,
            revision_key: self.revision_key.clone(),
            pending_file_jump: None,
            pending_find_scroll: None,
            last_find_scroll_token: None,
            vertical_offset: 0.0,
            paragraph_cache: RefCell::new(std::collections::HashMap::new()),
            selection_anchor: None,
            selection_focus: None,
            selection_anchor_unit_start: None,
            selection_anchor_unit_end: None,
            selection_unit: SelectionUnit::Character,
            is_selecting: false,
            last_click_time: None,
            last_click_screen: None,
            click_count: 0,
            last_drag_cursor: None,
            scrollbar: ScrollbarState::default(),
            last_restore_token: 0,
            pending_set_offset: None,
            last_content_version: 0,
            height_index: RefCell::new(HeightIndex::default()),
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();

        // Checked before the revision-key reset below (which early-returns), so
        // a tab restore is never skipped. Applied last in `update()`, so it
        // overrides the reset-to-top + file jump that reset schedules.
        if self.restore_token != state.last_restore_token {
            state.last_restore_token = self.restore_token;
            state.pending_set_offset = Some(self.restore_offset);
        }

        // The content changed under us (a reload, a working-copy edit, or a tab
        // switch — the cache lives in shared widget `State`). Drop the per-line
        // paragraph cache, whose `(file, hunk, line)` keys now map to different
        // text. Done before the revision-key reset (which early-returns) so
        // `last_content_version` always advances — otherwise it would re-clear
        // every frame after a revision change. Cache-only: a working-copy edit
        // must refresh the rendered text without resetting scroll or selection.
        if self.content_version != state.last_content_version {
            state.last_content_version = self.content_version;
            state.paragraph_cache.borrow_mut().clear();
        }

        if state.revision_key != self.revision_key {
            state.revision_key = self.revision_key.clone();
            state.vertical_offset = 0.0;
            state.selected_file = self.selected_file;
            state.pending_file_jump = Some(self.selected_file);
            // A revision change means the underlying line indices no longer
            // refer to the same text — drop the selection rather than risk
            // copying stale content.
            state.selection_anchor = None;
            state.selection_focus = None;
            state.selection_anchor_unit_start = None;
            state.selection_anchor_unit_end = None;
            state.selection_unit = SelectionUnit::Character;
            state.is_selecting = false;
            state.last_drag_cursor = None;
            state.click_count = 0;
            // Cache entries are keyed by (file, hunk, line) which now
            // points at different content.
            state.paragraph_cache.borrow_mut().clear();
            return;
        }

        if state.selected_file != self.selected_file {
            state.pending_file_jump = Some(self.selected_file);
        }

        if let Some(find) = &self.find
            && state.last_find_scroll_token != Some(find.scroll_token)
        {
            state.last_find_scroll_token = Some(find.scroll_token);
            if let Some(active_idx) = find.active
                && let Some(m) = find.matches.get(active_idx)
            {
                state.pending_find_scroll =
                    Some((m.file_index, m.hunk_index, m.line_index, m.byte_start));
            }
        }
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
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
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        // (Re)build the layout index up front: every height/position read this
        // pass — and the following `draw` — is a prefix-sum lookup against it.
        self.ensure_height_index(&state.height_index, bounds.width);
        let content_height = state.height_index.borrow().total_height;
        // Offset on entry; compared on the way out so any change this pass
        // (wheel, scrollbar, file jump, find, restore, clamp) is reported once
        // via `on_scroll`. `fn` pointers are `Copy`, so this borrows nothing.
        let prev_offset = state.vertical_offset;
        let max_vertical = (content_height - bounds.height).max(0.0);

        if state.vertical_offset > max_vertical {
            state.vertical_offset = max_vertical;
            shell.request_redraw();
        }

        if let Some(file_index) = state
            .pending_file_jump
            .take()
            .or_else(|| (state.selected_file != self.selected_file).then_some(self.selected_file))
        {
            // For file 0, scroll to the very top so the revision header stays
            // visible — `file_offset(0)` equals `header_height()`, which would
            // park the file's content header right at the top and hide the
            // revision metadata above it.
            let target = if file_index == 0 {
                0.0
            } else {
                self.file_offset(&state.height_index.borrow(), file_index)
            };
            state.vertical_offset = target.clamp(0.0, max_vertical);
            state.selected_file = file_index;
            shell.request_redraw();
        }

        if let Some((file_idx, hunk_idx, line_idx, byte_offset)) = state.pending_find_scroll.take()
            && let Some(target) = {
                let index = state.height_index.borrow();
                self.match_target_y(&index, file_idx, hunk_idx, line_idx, byte_offset, bounds)
            }
        {
            // Center the row in the viewport when there's room; clamp
            // otherwise. Centering keeps the match's surrounding context
            // visible instead of pinning it to the top edge.
            let centered = target - (bounds.height - self.metrics.row_height) / 2.0;
            state.vertical_offset = centered.clamp(0.0, max_vertical);
            let selected_file =
                self.file_at_offset(&state.height_index.borrow(), state.vertical_offset);
            if selected_file != state.selected_file {
                state.selected_file = selected_file;
                shell.publish((self.on_selected_file_changed)(selected_file));
            }
            shell.request_redraw();
        }

        // Tab restore: jump straight to the saved offset, overriding the
        // reset-to-top a revision change scheduled above. Only the offset is
        // restored — `selected_file` was already set by the file-jump block
        // above (to the tab's saved file), and republishing it here would fire
        // `SelectFile` → `scroll_sidebar_to_file`, fighting the sidebar's own
        // restore. The saved offset and saved file were captured together, so
        // they stay consistent without recomputing one from the other.
        if let Some(offset) = state.pending_set_offset.take() {
            let target = offset.clamp(0.0, max_vertical);
            if (target - state.vertical_offset).abs() > f32::EPSILON {
                state.vertical_offset = target;
                shell.request_redraw();
            }
        }

        match event {
            Event::Window(window::Event::RedrawRequested(_)) => {
                if state.is_selecting
                    && let Some(cursor_pos) = state.last_drag_cursor
                {
                    let scroll_delta = auto_scroll_delta(cursor_pos.y, bounds);
                    if scroll_delta != 0.0 {
                        let new_offset =
                            (state.vertical_offset + scroll_delta).clamp(0.0, max_vertical);
                        if (new_offset - state.vertical_offset).abs() > f32::EPSILON {
                            state.vertical_offset = new_offset;
                            self.advance_drag_selection(state, cursor_pos, bounds);
                            let selected_file = self
                                .file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                            if selected_file != state.selected_file {
                                state.selected_file = selected_file;
                                shell.publish((self.on_selected_file_changed)(selected_file));
                            }
                        }
                        shell.request_redraw();
                    }
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(_cursor_position) = cursor.position_over(bounds) else {
                    return;
                };

                let movement = match *delta {
                    mouse::ScrollDelta::Lines { x: _, y } => {
                        Vector::new(0.0, -y * self.metrics.row_height * LINE_SCROLL_ROWS)
                    }
                    mouse::ScrollDelta::Pixels { x: _, y } => {
                        Vector::new(0.0, -y * PIXEL_SCROLL_SCALE)
                    }
                };

                if movement.y != 0.0 {
                    state.vertical_offset =
                        (state.vertical_offset + movement.y).clamp(0.0, max_vertical);
                    let selected_file =
                        self.file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                    if selected_file != state.selected_file {
                        state.selected_file = selected_file;
                        shell.publish((self.on_selected_file_changed)(selected_file));
                    }
                }

                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position_over(bounds) else {
                    // Click outside our bounds — drop any existing selection
                    // so it doesn't visually persist after the user moves on.
                    if state.selection_anchor.is_some() {
                        state.selection_anchor = None;
                        state.selection_focus = None;
                        state.selection_anchor_unit_start = None;
                        state.selection_anchor_unit_end = None;
                        state.selection_unit = SelectionUnit::Character;
                        shell.request_redraw();
                    }
                    return;
                };
                match scrollbar::on_button_pressed(
                    &mut state.scrollbar,
                    point,
                    bounds,
                    content_height,
                    state.vertical_offset,
                ) {
                    scrollbar::ScrollbarEvent::OffsetChanged(new_offset) => {
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file = self
                            .file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
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
                let position = {
                    let index = state.height_index.borrow();
                    self.position_at_point(&index, point, bounds, state.vertical_offset)
                };
                let Some(position) = position else {
                    return;
                };

                let now = Instant::now();
                let within_window = state
                    .last_click_time
                    .map(|t| now.duration_since(t).as_millis() as u64 <= self.multi_click_ms)
                    .unwrap_or(false);
                let same_spot = state
                    .last_click_screen
                    .map(|p| p.distance(point) <= MULTI_CLICK_RADIUS)
                    .unwrap_or(false);
                let next_count = if within_window && same_spot {
                    state.click_count + 1
                } else {
                    1
                };
                state.click_count = next_count;
                state.last_click_time = Some(now);
                state.last_click_screen = Some(point);

                // Cycle through Char → Word → Line on each successive click.
                let unit = match ((next_count - 1) % 3, next_count) {
                    (0, _) => SelectionUnit::Character,
                    (1, _) => SelectionUnit::Word,
                    _ => SelectionUnit::Line,
                };
                state.selection_unit = unit;

                let (anchor_start, anchor_end) = self.expand_to_unit(position, unit);
                state.selection_anchor_unit_start = Some(anchor_start);
                state.selection_anchor_unit_end = Some(anchor_end);
                state.selection_anchor = Some(anchor_start);
                state.selection_focus = Some(anchor_end);
                state.is_selecting = true;
                state.last_drag_cursor = Some(point);
                shell.capture_event();
                shell.request_redraw();
            }
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
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file = self
                            .file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
                        shell.capture_event();
                        shell.request_redraw();
                    }
                    if state.vertical_offset != prev_offset
                        && let Some(cb) = on_scroll
                    {
                        shell.publish(cb(state.vertical_offset));
                    }
                    return;
                }
                if !state.is_selecting {
                    return;
                }
                state.last_drag_cursor = Some(*position);
                self.advance_drag_selection(state, *position, bounds);
                // Kick the redraw loop so the auto-scroll handler picks up
                // where the cursor is even if the mouse stays still.
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let scrollbar::ScrollbarEvent::Captured =
                    scrollbar::on_button_released(&mut state.scrollbar)
                {
                    shell.capture_event();
                    return;
                }
                if !state.is_selecting {
                    return;
                }
                state.is_selecting = false;
                state.last_drag_cursor = None;
                // If the user clicked without dragging in character mode,
                // anchor == focus — clear so we don't leave a phantom
                // zero-width selection. Multi-click selections are kept
                // even without drag, since the user expects them to stick
                // (e.g. double-click a word, then Cmd+C).
                if state.selection_unit == SelectionUnit::Character
                    && state.selection_anchor == state.selection_focus
                {
                    state.selection_anchor = None;
                    state.selection_focus = None;
                }
                shell.request_redraw();
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                if !is_copy_shortcut(key, *modifiers) {
                    return;
                }
                let (Some(anchor), Some(focus)) = (state.selection_anchor, state.selection_focus)
                else {
                    return;
                };
                let (start, end) = ordered(anchor, focus);
                if start == end {
                    return;
                }
                let Some(on_copy) = self.on_copy else {
                    return;
                };
                let text = self.collect_selected_text(start, end);
                if !text.is_empty() {
                    shell.publish(on_copy(text));
                    shell.capture_event();
                }
            }
            _ => {}
        }

        // Report any offset change from this pass (wheel, auto-scroll, file
        // jump, find, restore, clamp) once. The scrollbar early-returns above
        // publish on their own.
        if state.vertical_offset != prev_offset
            && let Some(cb) = on_scroll
        {
            shell.publish(cb(state.vertical_offset));
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(_) = bounds.intersection(viewport) else {
            return;
        };

        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        let content_width = self.content_width(bounds.width);
        // Normally a no-op — `update` ran first and built it — but draw must
        // not rely on event ordering for correctness.
        self.ensure_height_index(&state.height_index, bounds.width);
        let height_index = state.height_index.borrow();
        // Keys touched this frame; entries not in this set get evicted
        // at the end so the cache doesn't grow unbounded as the user
        // scrolls past new rows. Eviction-at-end is safe because the
        // renderer only holds `Weak` refs to paragraphs we *did* draw
        // (i.e. ones we add to `seen` here).
        let mut paragraph_seen: std::collections::HashSet<ParagraphKey> =
            std::collections::HashSet::new();
        let content_clip_bounds = Rectangle {
            x: bounds.x + self.metrics.gutter_width + PREFIX_WIDTH,
            y: bounds.y,
            width: (bounds.width - self.metrics.gutter_width - PREFIX_WIDTH).max(1.0),
            height: bounds.height,
        };

        renderer.with_layer(bounds, |renderer| {
            self.draw_background(
                renderer,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                self.palette.panel,
            );

            let visible_top = state.vertical_offset;
            let visible_bottom = visible_top + bounds.height;
            let header_height = self.header_height();
            let visible_capacity = (bounds.height / self.metrics.row_height).ceil() as usize + 8;
            let mut visible_file_headers = Vec::new();
            let mut visible_hunk_headers = Vec::new();
            let mut visible_rows = Vec::with_capacity(visible_capacity);
            let mut visible_bands = Vec::new();

            // Everything below is O(log n + visible) against the prefix-sum
            // index — never a walk over the whole document.
            // Content-space top of each file's header (sans the sentinel).
            let file_tops = &height_index.file_tops[..self
                .files
                .len()
                .min(height_index.file_tops.len())];

            let first_file = file_tops
                .partition_point(|&top| top + self.metrics.file_header_height < visible_top);
            for (file_index, &top) in file_tops.iter().enumerate().skip(first_file) {
                if top > visible_bottom {
                    break;
                }
                visible_file_headers.push(VisibleFileHeader {
                    file_index,
                    y: bounds.y + (top - visible_top),
                });
            }

            let first_hunk = height_index
                .hunk_tops
                .partition_point(|&top| top + self.metrics.hunk_header_height < visible_top);
            for (i, &top) in height_index
                .hunk_tops
                .iter()
                .enumerate()
                .skip(first_hunk)
            {
                if top > visible_bottom {
                    break;
                }
                let (file_index, hunk_index) = height_index.hunk_ids[i];
                visible_hunk_headers.push(VisibleHunkHeader {
                    file_index: file_index as usize,
                    hunk_index: hunk_index as usize,
                    y: bounds.y + (top - visible_top),
                });
            }

            // The candidate first row may still end above the viewport when
            // the top lands in a header band — the in-loop check skips it.
            let first_row = height_index
                .row_tops
                .partition_point(|&top| top <= visible_top)
                .saturating_sub(1);
            for (i, &row_top) in height_index.row_tops.iter().enumerate().skip(first_row) {
                if row_top > visible_bottom {
                    break;
                }
                let (file_index, hunk_index, line_index) = height_index.row_ids[i];
                let (file_index, hunk_index, line_index) =
                    (file_index as usize, hunk_index as usize, line_index as usize);
                let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
                let height = self.row_height(line, content_width);
                if row_top + height < visible_top {
                    continue;
                }
                let y = bounds.y + (row_top - visible_top);
                visible_rows.push(VisibleRow {
                    file_index,
                    hunk_index,
                    line_index,
                    y,
                    height,
                });
                push_visible_band(&mut visible_bands, line.kind, y, height);
            }

            // Sticky file header: the file occupying the top of the viewport
            // keeps its name pinned while its hunks scroll under it, until the
            // next file's header slides up to push it off. Only kicks in once
            // that file's header has scrolled above the viewport top — which is
            // always below the revision header, so the two never overlap.
            let content_end = height_index.total_height;
            let sticky_file = file_tops
                .partition_point(|&top| top <= visible_top)
                .checked_sub(1)
                .filter(|&i| file_tops[i] < visible_top);
            let sticky_pin_y = sticky_file.map(|i| {
                let next_top = file_tops.get(i + 1).copied().unwrap_or(content_end);
                let pinned_content_y = visible_top.min(next_top - self.metrics.file_header_height);
                bounds.y + (pinned_content_y - visible_top)
            });

            self.draw_background(
                renderer,
                bounds.x,
                bounds.y,
                self.metrics.gutter_width,
                bounds.height,
                self.palette.gutter_background,
            );
            self.draw_background(
                renderer,
                bounds.x + self.metrics.gutter_width,
                bounds.y,
                1.0,
                bounds.height,
                self.palette.border,
            );

            for band in &visible_bands {
                let Some(background) = self.changed_line_background_color(band.kind) else {
                    continue;
                };

                self.draw_background(
                    renderer,
                    bounds.x,
                    band.y,
                    bounds.width,
                    band.height,
                    background,
                );
            }

            let selection_range = match (state.selection_anchor, state.selection_focus) {
                (Some(anchor), Some(focus)) if anchor != focus => Some(ordered(anchor, focus)),
                _ => None,
            };

            if header_height > 0.0 {
                self.draw_revision_header(
                    renderer,
                    bounds,
                    visible_top,
                    header_height,
                    selection_range,
                );
            }

            for header in &visible_file_headers {
                // The sticky file's header is drawn pinned, on top, after the
                // rows — skip its natural (scrolling) draw here.
                if Some(header.file_index) == sticky_file {
                    continue;
                }
                self.draw_file_header(renderer, header.file_index, header.y, bounds);
            }

            for header in &visible_hunk_headers {
                let hunk = &self.files[header.file_index].hunks[header.hunk_index];
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y + self.metrics.hunk_header_height - 1.0,
                    bounds.width,
                    1.0,
                    self.palette.hunk_header,
                );
                self.draw_text(
                    renderer,
                    &hunk.header,
                    TextRenderParams {
                        width: self.text_width(&hunk.header),
                        height: self.metrics.hunk_header_height,
                        position: Point::new(
                            bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
                            header.y + TEXT_Y_PADDING,
                        ),
                        color: self.palette.text_muted,
                        clip_bounds: content_clip_bounds,
                        wrapping: text::Wrapping::None,
                    },
                );
            }

            // Draw selection backgrounds *before* the text so glyphs render
            // on top. Painting after would either occlude the text or
            // require alpha blending tricks; before-and-translucent is the
            // common pattern (matches how text editors render selection).
            if let (Some(anchor), Some(focus)) = (state.selection_anchor, state.selection_focus) {
                let (sel_start, sel_end) = ordered(anchor, focus);
                if sel_start != sel_end {
                    let cw = self.metrics.char_width;
                    let chars_per_line = chars_per_visual_line(content_width, cw);
                    let text_x =
                        bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
                    let visual_line_height = self.metrics.row_height;
                    let max_right = content_clip_bounds.x + content_clip_bounds.width;
                    for row in &visible_rows {
                        let line =
                            &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
                        let row_pos_start =
                            body_position(row.file_index, row.hunk_index, row.line_index, 0);
                        let row_pos_end = body_position(
                            row.file_index,
                            row.hunk_index,
                            row.line_index,
                            line.content.len(),
                        );
                        if row_pos_end < sel_start || row_pos_start >= sel_end {
                            continue;
                        }

                        let line_start_byte = if row_pos_start < sel_start {
                            sel_start.byte
                        } else {
                            0
                        };
                        let line_end_byte = if row_pos_end > sel_end {
                            sel_end.byte
                        } else {
                            line.content.len()
                        };
                        let start_chars = char_count_at_byte(&line.content, line_start_byte);
                        let end_chars = char_count_at_byte(&line.content, line_end_byte);
                        let total_chars = line.content.chars().count();
                        let is_full_line = sel_start <= row_pos_start && row_pos_end < sel_end;

                        // Walk each visual sub-line the row contains and
                        // intersect the selection char range with it. Without
                        // this loop a wrapped row would render a single full-
                        // width rectangle across every visual line, ignoring
                        // where the selection actually starts and ends.
                        let visual_lines = total_chars.max(1).div_ceil(chars_per_line);
                        for visual_idx in 0..visual_lines {
                            let vline_start = visual_idx * chars_per_line;
                            let vline_end = ((visual_idx + 1) * chars_per_line).min(total_chars);
                            let seg_start = start_chars.max(vline_start);
                            let seg_end = end_chars.min(vline_end);
                            if seg_start >= seg_end {
                                continue;
                            }
                            let mut x = text_x + (seg_start - vline_start) as f32 * cw;
                            let mut width = (seg_end - seg_start) as f32 * cw;
                            // The "select through end-of-line" tail only
                            // belongs on the trailing visual line of a full
                            // logical row, not on every wrapped segment.
                            let is_trailing_visual = visual_idx + 1 == visual_lines;
                            if is_full_line && is_trailing_visual {
                                width += cw * 0.6;
                            }
                            if x < content_clip_bounds.x {
                                let trim = content_clip_bounds.x - x;
                                x = content_clip_bounds.x;
                                width = (width - trim).max(0.0);
                            }
                            if x + width > max_right {
                                width = (max_right - x).max(0.0);
                            }
                            if width <= 0.0 {
                                continue;
                            }
                            let y = row.y + visual_idx as f32 * visual_line_height;
                            self.draw_background(
                                renderer,
                                x,
                                y,
                                width,
                                visual_line_height,
                                self.palette.selection,
                            );
                        }
                    }
                }
            }

            if let Some(find) = &self.find {
                self.draw_find_highlights(
                    renderer,
                    find,
                    &visible_rows,
                    content_clip_bounds,
                    bounds,
                    content_width,
                );
            }

            let content_width_bits = content_width.to_bits();
            for row in &visible_rows {
                let line = &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
                let key = ParagraphKey {
                    file_index: row.file_index as u32,
                    hunk_index: row.hunk_index as u32,
                    line_index: row.line_index as u32,
                    content_width_bits,
                };
                self.draw_row(
                    renderer,
                    line,
                    RowRenderParams {
                        bounds,
                        content_clip_bounds,
                        y: row.y,
                        height: row.height,
                        content_width,
                    },
                    key,
                    &state.paragraph_cache,
                    &mut paragraph_seen,
                );
            }

            // Pinned sticky file header. Drawn in its own layer so the opaque
            // strip composites *over* the code beneath it: within a single
            // layer `fill_quad` always renders behind `fill_text`, so the
            // header background alone can't hide the diff text scrolling under
            // it — only a new layer does.
            if let (Some(file_index), Some(y)) = (sticky_file, sticky_pin_y) {
                renderer.with_layer(bounds, |renderer| {
                    self.draw_file_header(renderer, file_index, y, bounds);
                });
            }

            let geom = scrollbar::geometry(bounds, height_index.total_height, state.vertical_offset);
            // Draw the scrollbar in its own layer, created *after* the sticky
            // header's, so it composites above it — sub-layers stack in creation
            // order, so otherwise the full-width sticky strip would hide the
            // scrollbar's top.
            renderer.with_layer(bounds, |renderer| {
                scrollbar::draw(renderer, &geom, &self.palette.scrollbar);
            });
        });

        // Evict entries that weren't used this frame. Their last
        // `fill_paragraph` was at least one frame ago, so the renderer's
        // present queue no longer references them.
        state
            .paragraph_cache
            .borrow_mut()
            .retain(|k, _| paragraph_seen.contains(k));
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        // Match the convention of every code editor: text cursor over the
        // text area, default arrow over the gutter / chrome. (Avoid
        // `AllScroll` — implies drag-to-pan, which we don't support.)
        let bounds = layout.bounds();
        let Some(point) = cursor.position_over(bounds) else {
            return mouse::Interaction::None;
        };
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        self.ensure_height_index(&state.height_index, bounds.width);
        let content_height = state.height_index.borrow().total_height;
        if scrollbar::is_dragging(&state.scrollbar)
            || scrollbar::hits_container(bounds, point, content_height)
        {
            return mouse::Interaction::Idle;
        }
        // In the revision-header strip, show the text cursor only over the
        // selectable values (field values + description), and the arrow over
        // labels, bookmark chips, and blank space.
        let target_y = point.y - bounds.y + state.vertical_offset;
        if target_y < self.header_height() {
            let line_offset = (target_y - HEADER_VERTICAL_PADDING) / self.metrics.row_height;
            let over_value = line_offset >= 0.0 && {
                let line_index = line_offset.floor() as usize;
                self.header_selectable_text(line_index).is_some()
                    && point.x >= self.header_text_origin_x(line_index, bounds)
            };
            return if over_value {
                mouse::Interaction::Text
            } else {
                mouse::Interaction::Idle
            };
        }
        if point.x >= bounds.x + self.metrics.gutter_width + PREFIX_WIDTH {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::Idle
        }
    }
}

impl<Message> DiffView<'_, Message> {
    fn text_width(&self, content: &str) -> f32 {
        (content.chars().count() as f32 * self.metrics.char_width + 16.0).max(1.0)
    }

    /// Update the selection focus based on the cursor's current screen
    /// position. Handles word/line-mode expansion: in those modes the
    /// selection always covers full units in both directions, so dragging
    /// past the next word boundary jumps an entire word at once.
    fn advance_drag_selection<P>(
        &self,
        state: &mut State<P>,
        cursor_pos: Point,
        bounds: Rectangle,
    ) {
        let focus_pos = {
            let index = state.height_index.borrow();
            self.position_at_point(&index, cursor_pos, bounds, state.vertical_offset)
        };
        let Some(focus_pos) = focus_pos else {
            return;
        };

        let (Some(anchor_start), Some(anchor_end)) = (
            state.selection_anchor_unit_start,
            state.selection_anchor_unit_end,
        ) else {
            return;
        };

        let unit = state.selection_unit;
        let (focus_start, focus_end) = if unit == SelectionUnit::Character {
            (focus_pos, focus_pos)
        } else {
            self.expand_to_unit(focus_pos, unit)
        };

        let (new_anchor, new_focus) = if focus_end <= anchor_start {
            // Drag has crossed to the left of the original click — selection
            // grows from the *end* of the anchor unit backwards to the
            // *start* of the focus unit.
            (anchor_end, focus_start)
        } else if focus_start >= anchor_end {
            // Drag is to the right of the anchor unit.
            (anchor_start, focus_end)
        } else {
            // Drag is still inside the original anchor unit.
            (anchor_start, anchor_end)
        };

        if state.selection_anchor != Some(new_anchor) || state.selection_focus != Some(new_focus) {
            state.selection_anchor = Some(new_anchor);
            state.selection_focus = Some(new_focus);
        }
    }

    fn make_text(
        &self,
        content: &str,
        width: f32,
        height: f32,
        wrapping: text::Wrapping,
    ) -> text::Text<String, Font> {
        text::Text {
            content: content.to_owned(),
            bounds: Size::new(width.max(1.0), height.max(1.0)),
            size: Pixels(self.text_size),
            line_height: text::LineHeight::Absolute(Pixels(height.min(self.metrics.row_height))),
            font: self.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        }
    }

    fn draw_background<Renderer>(
        &self,
        renderer: &mut Renderer,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
    ) where
        Renderer: renderer::Renderer,
    {
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x,
                    y,
                    width,
                    height,
                },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            color,
        );
    }

    /// Draw a pill-shaped chip (filled, or outlined when `border` is `Some`).
    /// Used for bookmark chips in the revision header.
    fn draw_chip<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        fill: Color,
        border: Option<Color>,
    ) where
        Renderer: renderer::Renderer,
    {
        let radius = (bounds.height / 2.0).into();
        let border = match border {
            Some(color) => Border {
                color,
                width: 1.0,
                radius,
            },
            None => Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius,
            },
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border,
                shadow: Shadow::default(),
                snap: true,
            },
            fill,
        );
    }

    /// Draw one file's header strip (background + bottom border + title +
    /// stats) at screen-y `y`. Shared by the normal scrolling draw and the
    /// pinned sticky draw.
    fn draw_file_header<Renderer>(
        &self,
        renderer: &mut Renderer,
        file_index: usize,
        y: f32,
        bounds: Rectangle,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let file = &self.files[file_index];
        let hunk_label = if file.hunks.len() == 1 {
            "1 Hunk".to_owned()
        } else {
            format!("{} Hunks", file.hunks.len())
        };
        let summary = format!(
            "{}  +{} -{}  {}",
            file.status, file.additions, file.deletions, hunk_label
        );
        let summary_width = self
            .text_width(&summary)
            .min((bounds.width - 24.0).max(1.0));

        self.draw_background(
            renderer,
            bounds.x,
            y,
            bounds.width,
            self.metrics.file_header_height,
            self.palette.file_header,
        );
        self.draw_background(
            renderer,
            bounds.x,
            y + self.metrics.file_header_height - 1.0,
            bounds.width,
            1.0,
            self.palette.border,
        );
        self.draw_text(
            renderer,
            &file.title,
            TextRenderParams {
                width: (bounds.width - summary_width - 28.0).max(1.0),
                height: self.metrics.row_height,
                position: Point::new(
                    bounds.x + 12.0,
                    centered_text_y(y, self.metrics.file_header_height, self.metrics.row_height),
                ),
                color: self.palette.text,
                clip_bounds: bounds,
                wrapping: text::Wrapping::WordOrGlyph,
            },
        );
        self.draw_text(
            renderer,
            &summary,
            TextRenderParams {
                width: summary_width,
                height: self.metrics.row_height,
                position: Point::new(
                    (bounds.x + bounds.width - summary_width - 8.0).max(bounds.x + 12.0),
                    centered_text_y(y, self.metrics.file_header_height, self.metrics.row_height),
                ),
                color: self.palette.text_muted,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );
    }

    fn draw_text<Renderer>(&self, renderer: &mut Renderer, content: &str, render: TextRenderParams)
    where
        Renderer: text::Renderer<Font = Font>,
    {
        renderer.fill_text(
            self.make_text(content, render.width, render.height, render.wrapping),
            render.position,
            render.color,
            render.clip_bounds,
        );
    }

    fn draw_code_text<Renderer>(
        &self,
        renderer: &mut Renderer,
        line: &DiffLine,
        render: TextRenderParams,
        cache_key: ParagraphKey,
        paragraph_cache: &RefCell<std::collections::HashMap<ParagraphKey, Renderer::Paragraph>>,
        paragraph_seen: &mut std::collections::HashSet<ParagraphKey>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        if line.syntax.is_empty() {
            self.draw_text(renderer, &line.content, render);
            return;
        }

        let spans = self.syntax_spans(&line.content, &line.syntax);
        if spans.is_empty() {
            self.draw_text(renderer, &line.content, render);
            return;
        }

        paragraph_seen.insert(cache_key);
        let mut cache = paragraph_cache.borrow_mut();
        let paragraph = cache.entry(cache_key).or_insert_with(|| {
            <Renderer::Paragraph as text::Paragraph>::with_spans(text::Text {
                content: spans.as_slice(),
                bounds: Size::new(render.width.max(1.0), render.height.max(1.0)),
                size: Pixels(self.text_size),
                line_height: text::LineHeight::Absolute(Pixels(
                    render.height.min(self.metrics.row_height),
                )),
                font: self.font,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Top,
                shaping: text::Shaping::Basic,
                wrapping: render.wrapping,
                ellipsis: text::Ellipsis::None,
                hint_factor: None,
            })
        });

        renderer.fill_paragraph(paragraph, render.position, render.color, render.clip_bounds);
    }

    fn syntax_spans<'a>(
        &self,
        content: &'a str,
        syntax: &'a [SyntaxSpan],
    ) -> Vec<text::Span<'a, (), Font>> {
        let mut spans = Vec::with_capacity(syntax.len().saturating_mul(2).saturating_add(1));
        let mut cursor = 0;

        for span in syntax {
            if span.start < cursor
                || span.start >= span.end
                || span.end > content.len()
                || !content.is_char_boundary(span.start)
                || !content.is_char_boundary(span.end)
            {
                continue;
            }

            if cursor < span.start {
                spans.push(text::Span::new(&content[cursor..span.start]));
            }

            spans.push(
                text::Span::new(&content[span.start..span.end]).color(self.syntax_color(span.kind)),
            );
            cursor = span.end;
        }

        if cursor < content.len() {
            spans.push(text::Span::new(&content[cursor..]));
        }

        spans
    }

    fn line_text_color(&self, kind: DiffLineKind) -> Color {
        match kind {
            DiffLineKind::Addition => self.palette.addition_text,
            DiffLineKind::Deletion => self.palette.deletion_text,
            DiffLineKind::Conflict => self.palette.conflict_marker,
            DiffLineKind::Context => self.palette.text,
            DiffLineKind::Note => self.palette.note_text,
        }
    }

    fn changed_line_background_color(&self, kind: DiffLineKind) -> Option<Color> {
        match kind {
            DiffLineKind::Addition => Some(self.palette.addition_background),
            DiffLineKind::Deletion => Some(self.palette.deletion_background),
            DiffLineKind::Conflict => Some(self.palette.note_background),
            DiffLineKind::Note => Some(self.palette.note_background),
            DiffLineKind::Context => None,
        }
    }

    fn syntax_color(&self, kind: SyntaxKind) -> Color {
        match kind {
            SyntaxKind::Comment => self.palette.text_muted,
            SyntaxKind::String => self.palette.modified_token,
            SyntaxKind::Number => self.palette.modified_token,
            SyntaxKind::Keyword => self.palette.conflict_marker,
            SyntaxKind::Function => self.palette.text,
            SyntaxKind::Type => self.palette.addition_text,
            SyntaxKind::Property => self.palette.modified_token,
            SyntaxKind::Punctuation => self.palette.text_muted,
        }
    }
}

/// How many characters fit on one visual line at the current monospace
/// glyph advance. Mirrors `row_height`'s wrap-count math so hit-tests and
/// selection geometry stay consistent with the way rows are laid out.
fn chars_per_visual_line(content_width: f32, cw: f32) -> usize {
    (content_width / cw.max(1.0)).floor().max(1.0) as usize
}

fn push_visible_band(bands: &mut Vec<VisibleBand>, kind: DiffLineKind, y: f32, height: f32) {
    if kind == DiffLineKind::Context {
        return;
    }

    match bands.last_mut() {
        Some(band) if band.kind == kind && (band.y + band.height - y).abs() < 0.5 => {
            band.height += height;
        }
        _ => bands.push(VisibleBand { kind, y, height }),
    }
}

fn format_gutter(old_line: Option<usize>, new_line: Option<usize>, digit_count: usize) -> String {
    let old = old_line.map(|line| line.to_string()).unwrap_or_default();
    let new = new_line.map(|line| line.to_string()).unwrap_or_default();
    format!("{old:>digit_count$} {new:>digit_count$}")
}

/// Maximum line-number digit count across all visible files. Used to size
/// the gutter just wide enough for the largest line number plus a small
/// padding, so a 30k-line file isn't truncated and a 50-line file doesn't
/// waste a third of the viewport on whitespace.
fn compute_gutter_digit_count(files: &[DiffFileView<'_>]) -> usize {
    // Runs on every `view()` rebuild, so it must not walk the whole diff.
    // Line numbers grow monotonically through a file's hunks, so each file's
    // maximum lives at the *tail of its last hunk* — scan that tail backwards
    // until both counters have been seen. The cap bounds the pathological
    // single-sided hunk (e.g. a 1M-line pure addition never carries an old
    // number); past it the other counter is at most one digit off anyway,
    // and the found side dominates the gutter width.
    const TAIL_SCAN_CAP: usize = 4_096;
    let mut max_line = 0usize;
    for file in files {
        let Some(hunk) = file.hunks.last() else {
            continue;
        };
        let mut last_old = None;
        let mut last_new = None;
        for line in hunk.lines.iter().rev().take(TAIL_SCAN_CAP) {
            if last_old.is_none() {
                last_old = line.old_line;
            }
            if last_new.is_none() {
                last_new = line.new_line;
            }
            if last_old.is_some() && last_new.is_some() {
                break;
            }
        }
        max_line = max_line
            .max(last_old.unwrap_or(0))
            .max(last_new.unwrap_or(0));
    }
    // Floor at 3 so the gutter still feels like a column for tiny files.
    digits(max_line).max(3)
}

fn char_count_at_byte(content: &str, byte: usize) -> usize {
    let cap = byte.min(content.len());
    content
        .char_indices()
        .take_while(|(idx, _)| *idx < cap)
        .count()
}

fn ordered(a: TextPosition, b: TextPosition) -> (TextPosition, TextPosition) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Translate a character index inside `content` to a byte offset, clamped
/// to the string length. Used so the click-to-position logic can store
/// byte offsets (cheap to slice) while the hit-test math operates in chars
/// (which is what monospace `x / char_width` gives us).
fn byte_offset_for_char(content: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    let mut bytes = 0;
    for (i, ch) in content.chars().enumerate() {
        if i == char_index {
            return bytes;
        }
        bytes += ch.len_utf8();
    }
    bytes.min(content.len())
}

fn is_copy_shortcut(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> bool {
    if !modifiers.command() {
        return false;
    }
    matches!(
        key.as_ref(),
        keyboard::Key::Character("c") | keyboard::Key::Character("C")
    )
}

/// Find the inclusive [start, end) byte range of the word the cursor is
/// currently inside. "Word" is the run of characters with the same
/// `word_class` as the character under the cursor, where word_class folds
/// identifier chars (alphanumeric + `_`) into one bucket, whitespace into
/// another, and everything else (punctuation, operators) into a third.
/// Clicking on whitespace selects that whitespace run; clicking on
/// punctuation selects that punctuation run; clicking on a letter selects
/// the identifier — matching the convention every text editor uses.
fn word_bounds(content: &str, byte_pos: usize) -> (usize, usize) {
    if content.is_empty() {
        return (0, 0);
    }
    let byte_pos = byte_pos.min(content.len());
    let chars: Vec<(usize, char)> = content.char_indices().collect();
    if chars.is_empty() {
        return (0, 0);
    }

    // Find the char index closest to byte_pos (the click might land between
    // two char boundaries when the user clicks on a wide glyph). Bias to
    // the char to the *left* of the cursor when the click lands at a
    // boundary, so double-clicking the gap between two words selects the
    // word to the left rather than picking arbitrarily.
    let mut anchor_index = chars
        .iter()
        .rposition(|(idx, _)| *idx <= byte_pos)
        .unwrap_or(0);
    if byte_pos == content.len() {
        anchor_index = chars.len() - 1;
    }

    let target_class = word_class(chars[anchor_index].1);
    let mut start = anchor_index;
    while start > 0 && word_class(chars[start - 1].1) == target_class {
        start -= 1;
    }
    let mut end = anchor_index;
    while end + 1 < chars.len() && word_class(chars[end + 1].1) == target_class {
        end += 1;
    }

    let start_byte = chars[start].0;
    let end_byte = chars[end].0 + chars[end].1.len_utf8();
    (start_byte, end_byte)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Whitespace,
    Punctuation,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else {
        WordClass::Punctuation
    }
}

/// Returns the y-delta (in px) to apply to `vertical_offset` on this frame
/// based on how far past the top/bottom edge the cursor sits. Zero when the
/// cursor is inside the viewport. Frame-rate–independent: assumes ~60fps,
/// which is fine for selection feel.
fn auto_scroll_delta(cursor_y: f32, bounds: Rectangle) -> f32 {
    let frame_seconds = 1.0 / 60.0;
    let bottom = bounds.y + bounds.height;
    let speed_for = |distance: f32| {
        let ramp = (distance / AUTO_SCROLL_RAMP_PX).clamp(0.0, 1.0);
        ramp * AUTO_SCROLL_MAX_SPEED * frame_seconds
    };
    if cursor_y < bounds.y {
        -speed_for(bounds.y - cursor_y)
    } else if cursor_y > bottom {
        speed_for(cursor_y - bottom)
    } else {
        0.0
    }
}

fn measure_char_advance_cached(font: Font, text_size: f32) -> f32 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(Font, u32), f32>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (font, text_size.to_bits());
    if let Some(&v) = cache.lock().unwrap().get(&key) {
        return v;
    }
    let v = measure_char_advance(font, text_size);
    cache.lock().unwrap().insert(key, v);
    v
}

fn measure_char_advance(font: Font, text_size: f32) -> f32 {
    use iced::advanced::graphics::text::Paragraph;
    use iced::advanced::text::Paragraph as _;

    // "M" is a stable choice for monospace measurement: it dominates hinting
    // noise at small sizes. For monospace fonts the width of one char *is*
    // the advance, which is what we cache here.
    let line_height = (text_size * 1.4).max(1.0);
    let paragraph = Paragraph::with_text(text::Text {
        content: "M",
        bounds: Size::new(f32::INFINITY, line_height),
        size: Pixels(text_size),
        line_height: text::LineHeight::Absolute(Pixels(line_height)),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    });
    paragraph.min_width()
}

fn digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut value = n;
    while value > 0 {
        count += 1;
        value /= 10;
    }
    count
}

fn centered_text_y(container_y: f32, container_height: f32, text_height: f32) -> f32 {
    container_y + (container_height - text_height) / 2.0
}

fn prefix_for_kind(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Addition => "+",
        DiffLineKind::Deletion => "-",
        DiffLineKind::Conflict => "!",
        DiffLineKind::Context => " ",
        DiffLineKind::Note => "\\",
    }
}

impl<'a, Message, Renderer> From<DiffView<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(diff_view: DiffView<'a, Message>) -> Self {
        Element::new(diff_view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_palette() -> Palette {
        let c = Color::WHITE;
        Palette {
            text: c,
            text_muted: c,
            addition_text: c,
            deletion_text: c,
            modified_token: c,
            conflict_marker: c,
            note_text: c,
            panel: c,
            file_header: c,
            hunk_header: c,
            addition_background: c,
            deletion_background: c,
            note_background: c,
            gutter_background: c,
            border: c,
            selection: c,
            scrollbar: ScrollbarStyle {
                track_color: c,
                thumb_color: c,
            },
        }
    }

    fn line(kind: DiffLineKind, content: &str, n: usize) -> DiffLine {
        DiffLine {
            kind,
            old_line: Some(n),
            new_line: Some(n),
            content: content.to_owned(),
            syntax: Vec::new(),
        }
    }

    /// A doc with several files/hunks and one long line that wraps, so the
    /// index has non-uniform row heights to account for.
    fn test_hunks() -> Vec<Vec<DiffHunkView>> {
        let long = "x".repeat(500);
        vec![
            vec![DiffHunkView {
                header: "@@ -1,3 +1,3 @@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Context, "alpha", 1),
                    line(DiffLineKind::Addition, &long, 2),
                    line(DiffLineKind::Deletion, "gamma", 3),
                ],
            }],
            vec![
                DiffHunkView {
                    header: "@@ -10,2 +10,2 @@".to_owned(),
                    lines: vec![
                        line(DiffLineKind::Context, "delta", 10),
                        line(DiffLineKind::Addition, "", 11),
                    ],
                },
                DiffHunkView {
                    header: "@@ -40,1 +40,1 @@".to_owned(),
                    lines: vec![line(DiffLineKind::Context, "epsilon", 40)],
                },
            ],
        ]
    }

    fn test_view(hunks: &[Vec<DiffHunkView>]) -> DiffView<'_, ()> {
        let files = hunks
            .iter()
            .enumerate()
            .map(|(i, hunks)| DiffFileView {
                title: format!("file-{i}"),
                status: "M",
                hunks,
                additions: 1,
                deletions: 1,
            })
            .collect();
        DiffView::new(
            files,
            0,
            "test",
            test_palette(),
            Font::MONOSPACE,
            13.0,
            500,
            |_| (),
        )
    }

    /// Brute-force row walk mirroring the pre-index geometry, used as the
    /// oracle the prefix sums must agree with.
    fn brute_force_tops(view: &DiffView<'_, ()>, content_width: f32) -> (Vec<f32>, Vec<f32>, f32) {
        let mut file_tops = Vec::new();
        let mut row_tops = Vec::new();
        let mut y = view.header_height();
        for file in &view.files {
            file_tops.push(y);
            y += view.metrics.file_header_height;
            for hunk in file.hunks {
                y += view.metrics.hunk_header_height;
                for l in &hunk.lines {
                    row_tops.push(y);
                    y += view.row_height(l, content_width);
                }
            }
        }
        (file_tops, row_tops, y)
    }

    #[test]
    fn height_index_matches_brute_force_walk() {
        let hunks = test_hunks();
        let view = test_view(&hunks);
        let width = 400.0;
        let content_width = view.content_width(width);

        let cell = RefCell::new(HeightIndex::default());
        view.ensure_height_index(&cell, width);
        let index = cell.borrow();

        let (file_tops, row_tops, total) = brute_force_tops(&view, content_width);
        assert_eq!(&index.file_tops[..file_tops.len()], file_tops.as_slice());
        assert_eq!(index.file_tops.last().copied(), Some(total));
        assert_eq!(index.row_tops, row_tops);
        assert_eq!(index.total_height, total);
        // The long line wraps: its row is taller than one row height, so the
        // following row's top reflects the wrap.
        assert!(index.row_tops[2] - index.row_tops[1] > view.metrics.row_height * 1.5);

        // file_offset / file_at_offset agree with the prefix sums.
        assert_eq!(view.file_offset(&index, 0), file_tops[0]);
        assert_eq!(view.file_offset(&index, 1), file_tops[1]);
        assert_eq!(view.file_at_offset(&index, 0.0), 0);
        assert_eq!(view.file_at_offset(&index, file_tops[1] - 1.0), 0);
        assert_eq!(view.file_at_offset(&index, file_tops[1]), 1);
        assert_eq!(view.file_at_offset(&index, total + 100.0), 1);

        // Row lookup by id and by y agree with the walk.
        for (i, &(f, h, l)) in index.row_ids.iter().enumerate() {
            assert_eq!(
                view.match_target_y(
                    &index,
                    f as usize,
                    h as usize,
                    l as usize,
                    0,
                    Rectangle::new(Point::ORIGIN, Size::new(width, 600.0)),
                ),
                Some(index.row_tops[i]),
            );
            assert_eq!(index.row_at(index.row_tops[i] + 0.5), Some(i));
        }
        // A y above the first row (in the file-0 header band) still resolves
        // to no candidate row... the candidate is `None` only above row 0.
        assert_eq!(index.row_at(index.row_tops[0] - 1.0), None);
    }

    #[test]
    fn height_index_rebuilds_on_shape_or_width_change() {
        let hunks = test_hunks();
        let mut view = test_view(&hunks);
        let cell = RefCell::new(HeightIndex::default());

        view.ensure_height_index(&cell, 400.0);
        let narrow_total = cell.borrow().total_height;
        let narrow_key = cell.borrow().key;

        // Wider viewport: the 500-char line wraps fewer times, so the total
        // shrinks and the key changes.
        view.ensure_height_index(&cell, 4000.0);
        assert_ne!(cell.borrow().key, narrow_key);
        assert!(cell.borrow().total_height < narrow_total);

        // Same shape + width: cache hit, key stable.
        let key = cell.borrow().key;
        view.ensure_height_index(&cell, 4000.0);
        assert_eq!(cell.borrow().key, key);

        // A replaced document (new layout id) must rebuild.
        view.layout_version = 7;
        view.ensure_height_index(&cell, 4000.0);
        assert_ne!(cell.borrow().key, key);
    }

    #[test]
    fn gutter_digits_use_hunk_tails() {
        let long = vec![
            DiffHunkView {
                header: String::new(),
                lines: vec![line(DiffLineKind::Context, "a", 5)],
            },
            DiffHunkView {
                header: String::new(),
                lines: vec![
                    line(DiffLineKind::Context, "b", 99_950),
                    // Trailing note without line numbers — the tail scan must
                    // step past it.
                    DiffLine {
                        kind: DiffLineKind::Note,
                        old_line: None,
                        new_line: None,
                        content: "\\ No newline at end of file".to_owned(),
                        syntax: Vec::new(),
                    },
                ],
            },
        ];
        let files = vec![DiffFileView {
            title: "f".to_owned(),
            status: "M",
            hunks: &long,
            additions: 0,
            deletions: 0,
        }];
        assert_eq!(compute_gutter_digit_count(&files), 5);
        assert_eq!(compute_gutter_digit_count(&[]), 3);
    }
}

/// Height-index scaling profile on a synthetic ~1M-line diff:
///   cargo test -p diffui profile_height_index -- --ignored --nocapture
#[cfg(test)]
mod height_profile {
    use super::*;

    #[test]
    #[ignore]
    fn profile_height_index() {
        let lines: Vec<DiffLine> = (0..1_000_000)
            .map(|i| DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(i + 1),
                new_line: Some(i + 1),
                content: format!("    let value_{i} = compute({i}) + offset; // padding padding"),
                syntax: Vec::new(),
            })
            .collect();
        let hunks: Vec<DiffHunkView> = lines
            .chunks(5_000)
            .map(|chunk| DiffHunkView {
                header: "@@ @@".to_owned(),
                lines: chunk.to_vec(),
            })
            .collect();
        let files: Vec<Vec<DiffHunkView>> = hunks.chunks(20).map(<[_]>::to_vec).collect();
        let views: Vec<DiffFileView<'_>> = files
            .iter()
            .enumerate()
            .map(|(i, hunks)| DiffFileView {
                title: format!("file-{i}"),
                status: "M",
                hunks,
                additions: 0,
                deletions: 0,
            })
            .collect();
        let view: DiffView<'_, ()> = DiffView::new(
            views,
            0,
            "profile",
            tests::test_palette(),
            Font::MONOSPACE,
            13.0,
            500,
            |_| (),
        );

        let cell = RefCell::new(HeightIndex::default());
        let t = std::time::Instant::now();
        view.ensure_height_index(&cell, 1200.0);
        let build = t.elapsed();

        let index = cell.borrow();
        let t = std::time::Instant::now();
        let mut acc = 0usize;
        for i in 0..10_000 {
            acc += view.file_at_offset(&index, (i * 137) as f32 % index.total_height);
            acc += index.row_at((i * 631) as f32 % index.total_height).unwrap_or(0);
        }
        let queries = t.elapsed();

        eprintln!("\n=== height index profile (1M lines) ===");
        eprintln!("build (once per content/width change): {build:?}");
        eprintln!("20k mixed queries                    : {queries:?}  (sink {acc})");
        eprintln!("per query                            : {:?}", queries / 20_000);
        eprintln!("=======================================\n");
    }
}
