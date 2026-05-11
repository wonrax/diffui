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

const ROW_HEIGHT: f32 = 24.0;
const FILE_HEADER_HEIGHT: f32 = 40.0;
const HUNK_HEADER_HEIGHT: f32 = 26.0;
const PREFIX_WIDTH: f32 = 24.0;
const CHANGE_MARK_WIDTH: f32 = 2.0;
const TEXT_X_PADDING: f32 = 8.0;
const TEXT_Y_PADDING: f32 = 2.0;
const LINE_SCROLL_ROWS: f32 = 1.5;
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

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub content: String,
    pub syntax: Vec<SyntaxSpan>,
}

#[derive(Debug, Clone)]
pub struct DiffHunkView {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Conflict,
    Note,
}

#[derive(Debug, Clone)]
pub struct DiffFileView<'a> {
    pub title: String,
    pub status: &'a str,
    pub hunks: &'a [DiffHunkView],
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone)]
pub struct SyntaxSpan {
    pub start: usize,
    pub end: usize,
    pub kind: SyntaxKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Property,
    Punctuation,
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
    /// A line of the description, rendered indented under the metadata
    /// block. Stored without indentation; the renderer prepends four
    /// spaces.
    Description(String),
    /// Blank separator between the metadata block and the description.
    Blank,
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
    gutter_width: f32,
    gutter_digit_count: usize,
    header: Vec<HeaderLine>,
    on_selected_file_changed: fn(usize) -> Message,
    on_copy: Option<fn(String) -> Message>,
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
    vertical_offset: f32,
    paragraphs: RefCell<Vec<Paragraph>>,
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
    /// Cached glyph advance for `self.font` at `self.text_size`. The diff
    /// font is monospace, so one measured width is enough for hit-testing.
    /// `None` until the first draw measures it. `Cell` so the immutable
    /// `draw()` path can fill it without going through `RefCell` ceremony.
    measured_char_advance: std::cell::Cell<Option<f32>>,
    scrollbar: ScrollbarState,
}

/// Stable cursor position inside the diff document. We index by
/// `(file, hunk, line)` instead of by screen y so the position stays valid
/// across scrolling, file selection, and re-renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPosition {
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    /// Byte offset within `line.content`. Bytes (not chars) so we can slice
    /// the source string directly when copying.
    byte: usize,
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
        // Width of the rendered gutter text "{old:>D} {new:>D}" plus padding.
        // Computed once at construction so layout, hit-testing and rendering
        // all agree on the column width without each having to remeasure.
        // Uses the same heuristic as the fallback char-advance — fine for
        // sizing the column, since the gutter text is whitespace-padded.
        let char_width = text_size * 0.62;
        let gutter_text_chars = gutter_digit_count * 2 + 1; // two columns + one separating space
        let gutter_width = (gutter_text_chars as f32 * char_width
            + GUTTER_HORIZONTAL_PADDING * 2.0)
            .max(GUTTER_MIN_WIDTH);
        Self {
            files,
            selected_file,
            revision_key: revision_key.into(),
            palette,
            font,
            text_size,
            multi_click_ms,
            gutter_width,
            gutter_digit_count,
            header: Vec::new(),
            on_selected_file_changed,
            on_copy: None,
        }
    }

    pub fn with_header(mut self, header: Vec<HeaderLine>) -> Self {
        self.header = header;
        self
    }

    pub fn on_copy(mut self, on_copy: fn(String) -> Message) -> Self {
        self.on_copy = Some(on_copy);
        self
    }

    /// Total height of the revision header block (metadata lines +
    /// description + vertical padding). Zero when no header is set.
    fn header_height(&self) -> f32 {
        if self.header.is_empty() {
            0.0
        } else {
            self.header.len() as f32 * ROW_HEIGHT + HEADER_VERTICAL_PADDING * 2.0
        }
    }

    fn content_height(&self, width: f32) -> f32 {
        let content_width = self.content_width(width);

        self.header_height()
            + self
                .files
                .iter()
                .map(|file| {
                    FILE_HEADER_HEIGHT
                        + file
                            .hunks
                            .iter()
                            .map(|hunk| {
                                HUNK_HEADER_HEIGHT
                                    + hunk
                                        .lines
                                        .iter()
                                        .map(|line| self.row_height(line, content_width))
                                        .sum::<f32>()
                            })
                            .sum::<f32>()
                })
                .sum::<f32>()
    }

    fn file_offset(&self, file_index: usize, width: f32) -> f32 {
        let content_width = self.content_width(width);

        self.header_height()
            + self
                .files
                .iter()
                .take(file_index)
                .map(|file| {
                    FILE_HEADER_HEIGHT
                        + file
                            .hunks
                            .iter()
                            .map(|hunk| {
                                HUNK_HEADER_HEIGHT
                                    + hunk
                                        .lines
                                        .iter()
                                        .map(|line| self.row_height(line, content_width))
                                        .sum::<f32>()
                            })
                            .sum::<f32>()
                })
                .sum::<f32>()
    }

    fn file_at_offset(&self, offset: f32, width: f32) -> usize {
        let content_width = self.content_width(width);
        let mut content_y = self.header_height();

        for (file_index, file) in self.files.iter().enumerate() {
            let file_height = FILE_HEADER_HEIGHT
                + file
                    .hunks
                    .iter()
                    .map(|hunk| {
                        HUNK_HEADER_HEIGHT
                            + hunk
                                .lines
                                .iter()
                                .map(|line| self.row_height(line, content_width))
                                .sum::<f32>()
                    })
                    .sum::<f32>();

            if offset < content_y + file_height {
                return file_index;
            }

            content_y += file_height;
        }

        self.files.len().saturating_sub(1)
    }

    fn content_width(&self, viewport_width: f32) -> f32 {
        (viewport_width - self.gutter_width - PREFIX_WIDTH - 16.0).max(self.char_width(None))
    }

    /// Width of a single monospace glyph for this font/size. Hit-tests and
    /// selection rendering reach for the value cached on the widget state
    /// after the first draw (`measured`), falling back to a `text_size *
    /// 0.62` heuristic until that runs. The heuristic over-counts for fonts
    /// narrower than Menlo by ~0.5 char/line, which is what caused selection
    /// rectangles to drift past the rightmost glyph and copies to drop the
    /// trailing character.
    fn char_width(&self, measured: Option<f32>) -> f32 {
        measured.unwrap_or(self.text_size * 0.62)
    }

    fn row_height(&self, line: &DiffLine, content_width: f32) -> f32 {
        let chars_per_line = (content_width / self.char_width(None)).floor().max(1.0) as usize;
        let wrapped_lines = line.content.chars().count().max(1).div_ceil(chars_per_line);

        wrapped_lines as f32 * ROW_HEIGHT
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
        point: Point,
        bounds: Rectangle,
        vertical_offset: f32,
        measured_char_advance: Option<f32>,
    ) -> Option<TextPosition> {
        let content_width = self.content_width(bounds.width);
        let text_x = bounds.x + self.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
        let target_y = point.y - bounds.y + vertical_offset;
        // The header sits above all file content; skip past it before
        // walking files so a click on the header doesn't try to position
        // inside the diff body.
        let mut content_y = self.header_height();
        let mut last_row: Option<(usize, usize, usize, f32, f32, usize)> = None;

        for (file_index, file) in self.files.iter().enumerate() {
            content_y += FILE_HEADER_HEIGHT;
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                content_y += HUNK_HEADER_HEIGHT;
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    let height = self.row_height(line, content_width);
                    let row_top = content_y;
                    let row_bottom = row_top + height;
                    let char_count = line.content.chars().count();
                    last_row = Some((
                        file_index, hunk_index, line_index, row_top, height, char_count,
                    ));

                    if target_y >= row_top && target_y < row_bottom {
                        let byte = self.byte_offset_for_x(
                            point.x,
                            text_x,
                            &line.content,
                            measured_char_advance,
                        );
                        return Some(TextPosition {
                            file_index,
                            hunk_index,
                            line_index,
                            byte,
                        });
                    }
                    content_y += height;
                }
            }
        }

        // Cursor is past the last row (e.g. user dragged below content). Snap
        // to the end of the document so selection covers everything up to here.
        last_row.map(
            |(file_index, hunk_index, line_index, _row_top, _height, char_count)| {
                let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
                let byte = byte_offset_for_char(&line.content, char_count);
                TextPosition {
                    file_index,
                    hunk_index,
                    line_index,
                    byte,
                }
            },
        )
    }

    /// Translate the screen-x of a click into a byte offset inside `content`.
    /// Uses the measured char advance for accuracy and rounds half-cells up
    /// so a click in the right half of a glyph lands after it (the editor
    /// convention).
    fn byte_offset_for_x(
        &self,
        x: f32,
        text_x: f32,
        content: &str,
        measured_char_advance: Option<f32>,
    ) -> usize {
        let cw = self.char_width(measured_char_advance).max(1.0);
        let relative_x = (x - text_x).max(0.0);
        // Round to nearest cell boundary — clicks in the right half of a
        // glyph snap after it, clicks in the left half snap before it.
        let char_offset = (relative_x / cw + 0.5).floor() as usize;
        let max_chars = content.chars().count();
        byte_offset_for_char(content, char_offset.min(max_chars))
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

        for (file_index, file) in self.files.iter().enumerate() {
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    let pos_start = TextPosition {
                        file_index,
                        hunk_index,
                        line_index,
                        byte: 0,
                    };
                    let pos_end = TextPosition {
                        file_index,
                        hunk_index,
                        line_index,
                        byte: line.content.len(),
                    };
                    if pos_end < start || pos_start >= end {
                        continue;
                    }

                    let line_start = if pos_start < start { start.byte } else { 0 };
                    let line_end = if pos_end > end {
                        end.byte
                    } else {
                        line.content.len()
                    };
                    let line_start = line_start.min(line.content.len());
                    let line_end = line_end.min(line.content.len()).max(line_start);

                    if !first {
                        output.push('\n');
                    }
                    first = false;

                    if line.content.is_char_boundary(line_start)
                        && line.content.is_char_boundary(line_end)
                    {
                        output.push_str(&line.content[line_start..line_end]);
                    }
                }
            }
        }
        output
    }

    fn draw_row<Renderer>(
        &self,
        renderer: &mut Renderer,
        line: &DiffLine,
        render: RowRenderParams,
        paragraphs: &RefCell<Vec<Renderer::Paragraph>>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let text_color = self.line_text_color(line.kind);
        let gutter = format_gutter(line.old_line, line.new_line, self.gutter_digit_count);
        let prefix = prefix_for_kind(line.kind);
        let bounds = render.bounds;

        self.draw_text(
            renderer,
            &gutter,
            TextRenderParams {
                width: (self.gutter_width - GUTTER_HORIZONTAL_PADDING * 2.0).max(1.0),
                height: ROW_HEIGHT,
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
                height: ROW_HEIGHT,
                position: Point::new(
                    bounds.x + self.gutter_width + TEXT_X_PADDING,
                    render.y + TEXT_Y_PADDING,
                ),
                color: text_color,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );

        let position = Point::new(
            bounds.x + self.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
            render.y + TEXT_Y_PADDING,
        );

        self.draw_code_text(
            renderer,
            line,
            TextRenderParams {
                width: render.content_width,
                height: render.height,
                position,
                color: text_color,
                clip_bounds: render.content_clip_bounds,
                wrapping: text::Wrapping::WordOrGlyph,
            },
            paragraphs,
        );
    }

    fn draw_revision_header<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        visible_top: f32,
        header_height: f32,
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
        let line_width = (bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - left_x).max(1.0);

        for line in &self.header {
            match line {
                HeaderLine::Field { label, value } => {
                    self.draw_text(
                        renderer,
                        label,
                        TextRenderParams {
                            width: label_width.max(1.0),
                            height: ROW_HEIGHT,
                            position: Point::new(left_x, y + TEXT_Y_PADDING),
                            color: label_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    self.draw_text(
                        renderer,
                        value,
                        TextRenderParams {
                            width: value_width,
                            height: ROW_HEIGHT,
                            position: Point::new(value_x, y + TEXT_Y_PADDING),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                }
                HeaderLine::Description(line) => {
                    let indented = format!("    {line}");
                    self.draw_text(
                        renderer,
                        &indented,
                        TextRenderParams {
                            width: line_width,
                            height: ROW_HEIGHT,
                            position: Point::new(left_x, y + TEXT_Y_PADDING),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                }
                HeaderLine::Blank => {}
            }
            y += ROW_HEIGHT;
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
            vertical_offset: 0.0,
            paragraphs: RefCell::new(Vec::new()),
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
            measured_char_advance: std::cell::Cell::new(None),
            scrollbar: ScrollbarState::default(),
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();

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
            return;
        }

        if state.selected_file != self.selected_file {
            state.pending_file_jump = Some(self.selected_file);
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
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        let max_vertical = (self.content_height(bounds.width) - bounds.height).max(0.0);

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
                self.file_offset(file_index, bounds.width)
            };
            state.vertical_offset = target.clamp(0.0, max_vertical);
            state.selected_file = file_index;
            shell.request_redraw();
        }

        let content_height = self.content_height(bounds.width);

        let measured = state.measured_char_advance.get();
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
                            self.advance_drag_selection(state, cursor_pos, bounds, measured);
                            let selected_file =
                                self.file_at_offset(state.vertical_offset, bounds.width);
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
                        Vector::new(0.0, -y * ROW_HEIGHT * LINE_SCROLL_ROWS)
                    }
                    mouse::ScrollDelta::Pixels { x: _, y } => {
                        Vector::new(0.0, -y * PIXEL_SCROLL_SCALE)
                    }
                };

                if movement.y != 0.0 {
                    state.vertical_offset =
                        (state.vertical_offset + movement.y).clamp(0.0, max_vertical);
                    let selected_file = self.file_at_offset(state.vertical_offset, bounds.width);
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
                    &self.palette.scrollbar,
                ) {
                    scrollbar::ScrollbarEvent::OffsetChanged(new_offset) => {
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file =
                            self.file_at_offset(state.vertical_offset, bounds.width);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
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
                let Some(position) =
                    self.position_at_point(point, bounds, state.vertical_offset, measured)
                else {
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

                let (anchor_start, anchor_end) = expand_to_unit(&self.files, position, unit);
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
                            &self.palette.scrollbar,
                        )
                    {
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file =
                            self.file_at_offset(state.vertical_offset, bounds.width);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
                        shell.capture_event();
                        shell.request_redraw();
                    }
                    return;
                }
                if !state.is_selecting {
                    return;
                }
                state.last_drag_cursor = Some(*position);
                self.advance_drag_selection(state, *position, bounds, measured);
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
        state.paragraphs.borrow_mut().clear();
        let content_width = self.content_width(bounds.width);
        let content_clip_bounds = Rectangle {
            x: bounds.x + self.gutter_width + PREFIX_WIDTH,
            y: bounds.y,
            width: (bounds.width - self.gutter_width - PREFIX_WIDTH).max(1.0),
            height: bounds.height,
        };

        // Measure the actual glyph advance for the code font on the first
        // draw and cache it on `state`. The constructor's `text_size * 0.62`
        // heuristic was off by ~0.5px/char for Menlo at 13pt, which made
        // long selection rectangles drift past the rightmost glyph.
        if state.measured_char_advance.get().is_none() {
            let advance = measure_char_advance(self.font, self.text_size);
            if advance.is_finite() && advance > 0.0 {
                state.measured_char_advance.set(Some(advance));
            }
        }
        let measured_char_advance = state.measured_char_advance.get();

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
            let mut content_y = header_height;
            let visible_capacity = (bounds.height / ROW_HEIGHT).ceil() as usize + 8;
            let mut visible_file_headers = Vec::new();
            let mut visible_hunk_headers = Vec::new();
            let mut visible_rows = Vec::with_capacity(visible_capacity);
            let mut visible_bands = Vec::new();

            for (file_index, file) in self.files.iter().enumerate() {
                let file_header_top = content_y;
                push_if_visible(
                    &mut visible_file_headers,
                    VisibleFileHeader {
                        file_index,
                        y: bounds.y + (file_header_top - visible_top),
                    },
                    file_header_top,
                    FILE_HEADER_HEIGHT,
                    visible_top,
                    visible_bottom,
                );
                content_y += FILE_HEADER_HEIGHT;

                for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                    let hunk_top = content_y;
                    push_if_visible(
                        &mut visible_hunk_headers,
                        VisibleHunkHeader {
                            file_index,
                            hunk_index,
                            y: bounds.y + (hunk_top - visible_top),
                        },
                        hunk_top,
                        HUNK_HEADER_HEIGHT,
                        visible_top,
                        visible_bottom,
                    );
                    content_y += HUNK_HEADER_HEIGHT;

                    for (line_index, line) in hunk.lines.iter().enumerate() {
                        let height = self.row_height(line, content_width);
                        let row_top = content_y;
                        let y = bounds.y + (row_top - visible_top);

                        if row_top + height >= visible_top && row_top <= visible_bottom {
                            visible_rows.push(VisibleRow {
                                file_index,
                                hunk_index,
                                line_index,
                                y,
                                height,
                            });
                            push_visible_band(&mut visible_bands, line.kind, y, height);
                        }

                        content_y += height;
                    }
                }
            }

            self.draw_background(
                renderer,
                bounds.x,
                bounds.y,
                self.gutter_width,
                bounds.height,
                self.palette.gutter_background,
            );
            self.draw_background(
                renderer,
                bounds.x + self.gutter_width,
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
                self.draw_background(
                    renderer,
                    bounds.x,
                    band.y,
                    CHANGE_MARK_WIDTH,
                    band.height,
                    self.changed_line_mark_color(band.kind),
                );
            }

            if header_height > 0.0 {
                self.draw_revision_header(renderer, bounds, visible_top, header_height);
            }

            for header in &visible_file_headers {
                let file = &self.files[header.file_index];
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
                    header.y,
                    bounds.width,
                    FILE_HEADER_HEIGHT,
                    self.palette.file_header,
                );
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y + FILE_HEADER_HEIGHT - 1.0,
                    bounds.width,
                    1.0,
                    self.palette.border,
                );
                self.draw_text(
                    renderer,
                    &file.title,
                    TextRenderParams {
                        width: (bounds.width - summary_width - 28.0).max(1.0),
                        height: ROW_HEIGHT,
                        position: Point::new(
                            bounds.x + 12.0,
                            centered_text_y(header.y, ROW_HEIGHT),
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
                        height: ROW_HEIGHT,
                        position: Point::new(
                            (bounds.x + bounds.width - summary_width - 8.0).max(bounds.x + 12.0),
                            centered_text_y(header.y, ROW_HEIGHT),
                        ),
                        color: self.palette.text_muted,
                        clip_bounds: bounds,
                        wrapping: text::Wrapping::None,
                    },
                );
            }

            for header in &visible_hunk_headers {
                let hunk = &self.files[header.file_index].hunks[header.hunk_index];
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y + HUNK_HEADER_HEIGHT - 1.0,
                    bounds.width,
                    1.0,
                    self.palette.hunk_header,
                );
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y,
                    CHANGE_MARK_WIDTH,
                    HUNK_HEADER_HEIGHT,
                    self.palette.modified_token,
                );
                self.draw_text(
                    renderer,
                    &hunk.header,
                    TextRenderParams {
                        width: self.text_width(&hunk.header),
                        height: HUNK_HEADER_HEIGHT,
                        position: Point::new(
                            bounds.x + self.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
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
                    let cw = self.char_width(measured_char_advance).max(1.0);
                    let text_x = bounds.x + self.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
                    for row in &visible_rows {
                        let line =
                            &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
                        let row_pos_start = TextPosition {
                            file_index: row.file_index,
                            hunk_index: row.hunk_index,
                            line_index: row.line_index,
                            byte: 0,
                        };
                        let row_pos_end = TextPosition {
                            file_index: row.file_index,
                            hunk_index: row.hunk_index,
                            line_index: row.line_index,
                            byte: line.content.len(),
                        };
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
                        let is_full_line = sel_start <= row_pos_start && row_pos_end < sel_end;
                        let mut x = text_x + start_chars as f32 * cw;
                        let mut width =
                            (end_chars.saturating_sub(start_chars) as f32 * cw).max(0.0);
                        // Pad full-line selections so they read as a "select
                        // through end-of-line" highlight, mimicking what
                        // editors do on shift+down.
                        if is_full_line {
                            width += cw * 0.6;
                        }
                        if width <= 0.0 {
                            // Empty selection on this row (e.g. cursor sits
                            // at column 0 on the trailing line).
                            continue;
                        }
                        // Constrain to the content region so a long
                        // selection on a wide row doesn't bleed into the
                        // gutter on horizontal overflow.
                        let max_right = content_clip_bounds.x + content_clip_bounds.width;
                        if x < content_clip_bounds.x {
                            let trim = content_clip_bounds.x - x;
                            x = content_clip_bounds.x;
                            width = (width - trim).max(0.0);
                        }
                        if x + width > max_right {
                            width = (max_right - x).max(0.0);
                        }
                        if width > 0.0 {
                            self.draw_background(
                                renderer,
                                x,
                                row.y,
                                width,
                                row.height,
                                self.palette.selection,
                            );
                        }
                    }
                }
            }

            for row in &visible_rows {
                let line = &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
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
                    &state.paragraphs,
                );
            }

            let geom = scrollbar::geometry(
                bounds,
                self.content_height(bounds.width),
                state.vertical_offset,
                &self.palette.scrollbar,
            );
            scrollbar::draw(renderer, &geom, &self.palette.scrollbar);
        });
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
        if scrollbar::is_dragging(&state.scrollbar)
            || scrollbar::hits_container(
                bounds,
                point,
                self.content_height(bounds.width),
                &self.palette.scrollbar,
            )
        {
            return mouse::Interaction::Idle;
        }
        if point.x >= bounds.x + self.gutter_width + PREFIX_WIDTH {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::Idle
        }
    }
}

impl<Message> DiffView<'_, Message> {
    fn text_width(&self, content: &str) -> f32 {
        (content.chars().count() as f32 * self.char_width(None) + 16.0).max(1.0)
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
        measured_char_advance: Option<f32>,
    ) {
        let Some(focus_pos) = self.position_at_point(
            cursor_pos,
            bounds,
            state.vertical_offset,
            measured_char_advance,
        ) else {
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
            expand_to_unit(&self.files, focus_pos, unit)
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
            line_height: text::LineHeight::Absolute(Pixels(height.min(ROW_HEIGHT))),
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
        paragraphs: &RefCell<Vec<Renderer::Paragraph>>,
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

        let paragraph = <Renderer::Paragraph as text::Paragraph>::with_spans(text::Text {
            content: spans.as_slice(),
            bounds: Size::new(render.width.max(1.0), render.height.max(1.0)),
            size: Pixels(self.text_size),
            line_height: text::LineHeight::Absolute(Pixels(render.height.min(ROW_HEIGHT))),
            font: self.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: render.wrapping,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        });

        renderer.fill_paragraph(
            &paragraph,
            render.position,
            render.color,
            render.clip_bounds,
        );
        paragraphs.borrow_mut().push(paragraph);
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

    fn changed_line_mark_color(&self, kind: DiffLineKind) -> Color {
        match kind {
            DiffLineKind::Addition => self.palette.addition_text,
            DiffLineKind::Deletion => self.palette.deletion_text,
            DiffLineKind::Conflict => self.palette.conflict_marker,
            DiffLineKind::Note => self.palette.note_text,
            DiffLineKind::Context => self.palette.border,
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

fn push_if_visible<T>(
    items: &mut Vec<T>,
    item: T,
    item_top: f32,
    item_height: f32,
    visible_top: f32,
    visible_bottom: f32,
) {
    if item_top + item_height >= visible_top && item_top <= visible_bottom {
        items.push(item);
    }
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
    let mut max_line = 0usize;
    for file in files {
        for hunk in file.hunks {
            for line in &hunk.lines {
                if let Some(n) = line.old_line {
                    max_line = max_line.max(n);
                }
                if let Some(n) = line.new_line {
                    max_line = max_line.max(n);
                }
            }
        }
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

/// Expand `pos` to cover the unit (word or line) that contains it. For
/// character-mode selection this is a no-op — the caller never reaches
/// this path with `unit == Character`.
fn expand_to_unit(
    files: &[DiffFileView<'_>],
    pos: TextPosition,
    unit: SelectionUnit,
) -> (TextPosition, TextPosition) {
    let Some(line) = files
        .get(pos.file_index)
        .and_then(|file| file.hunks.get(pos.hunk_index))
        .and_then(|hunk| hunk.lines.get(pos.line_index))
    else {
        return (pos, pos);
    };

    match unit {
        SelectionUnit::Character => (pos, pos),
        SelectionUnit::Line => {
            let start = TextPosition { byte: 0, ..pos };
            let end = TextPosition {
                byte: line.content.len(),
                ..pos
            };
            (start, end)
        }
        SelectionUnit::Word => {
            let (start_byte, end_byte) = word_bounds(&line.content, pos.byte);
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

fn centered_text_y(container_y: f32, text_height: f32) -> f32 {
    container_y + (FILE_HEADER_HEIGHT - text_height) / 2.0
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
