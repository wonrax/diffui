use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{
    Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size, Theme,
    Vector, alignment, keyboard,
};

const ROW_HEIGHT: f32 = 24.0;
const FILE_HEADER_HEIGHT: f32 = 42.0;
const HUNK_HEADER_HEIGHT: f32 = 28.0;
const METADATA_ROW_HEIGHT: f32 = 20.0;
const GUTTER_WIDTH: f32 = 112.0;
const PREFIX_WIDTH: f32 = 24.0;
const HORIZONTAL_STEP: f32 = 48.0;
const CHANGE_MARK_WIDTH: f32 = 3.0;
const SCROLLBAR_THICKNESS: f32 = 12.0;
const SCROLLBAR_MARGIN: f32 = 4.0;
const SCROLLBAR_THUMB_THICKNESS: f32 = 7.0;

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
    pub metadata: &'a [String],
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
}

pub struct DiffView<'a> {
    files: Vec<DiffFileView<'a>>,
    selected_file: usize,
    palette: Palette,
    font: Font,
    text_size: f32,
}

#[derive(Debug)]
struct State {
    selected_file: usize,
    pending_file_jump: Option<usize>,
    vertical_offset: f32,
    horizontal_offset: f32,
    horizontal_drag: Option<f32>,
    shift_pressed: bool,
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
struct SyntaxRenderParams {
    fallback: Color,
    content_width: f32,
    position: Point,
    clip_bounds: Rectangle,
}

#[derive(Debug, Clone, Copy)]
struct FragmentRenderParams {
    line_start: usize,
    start: usize,
    end: usize,
    color: Color,
    position: Point,
    y: f32,
    clip_bounds: Rectangle,
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
struct VisibleMetadata {
    file_index: usize,
    line_index: usize,
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

impl<'a> DiffView<'a> {
    pub fn new(
        files: Vec<DiffFileView<'a>>,
        selected_file: usize,
        palette: Palette,
        font: Font,
        text_size: f32,
    ) -> Self {
        Self {
            files,
            selected_file,
            palette,
            font,
            text_size,
        }
    }

    fn content_height(&self, width: f32) -> f32 {
        let content_width = self.content_width(width);

        self.files
            .iter()
            .map(|file| {
                FILE_HEADER_HEIGHT
                    + file.metadata.len() as f32 * METADATA_ROW_HEIGHT
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
            .sum()
    }

    fn file_offset(&self, file_index: usize, width: f32) -> f32 {
        let content_width = self.content_width(width);

        self.files
            .iter()
            .take(file_index)
            .map(|file| {
                FILE_HEADER_HEIGHT
                    + file.metadata.len() as f32 * METADATA_ROW_HEIGHT
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
            .sum()
    }

    fn max_horizontal_offset(&self, viewport_width: f32) -> f32 {
        let available_width = self.content_width(viewport_width);
        (self.max_content_chars() as f32 * self.char_width() - available_width).max(0.0)
    }

    fn content_width(&self, viewport_width: f32) -> f32 {
        (viewport_width - GUTTER_WIDTH - PREFIX_WIDTH - 16.0).max(self.char_width())
    }

    fn char_width(&self) -> f32 {
        self.text_size * 0.62
    }

    fn row_height(&self, line: &DiffLine, content_width: f32) -> f32 {
        let chars_per_line = (content_width / self.char_width()).floor().max(1.0) as usize;
        let wrapped_lines = line.content.chars().count().max(1).div_ceil(chars_per_line);

        wrapped_lines as f32 * ROW_HEIGHT
    }

    fn max_content_chars(&self) -> usize {
        self.files
            .iter()
            .flat_map(|file| {
                file.metadata
                    .iter()
                    .map(String::as_str)
                    .chain(file.hunks.iter().map(|hunk| hunk.header.as_str()))
            })
            .map(str::chars)
            .map(Iterator::count)
            .max()
            .unwrap_or(0)
    }

    fn draw_row<Renderer>(&self, renderer: &mut Renderer, line: &DiffLine, render: RowRenderParams)
    where
        Renderer: text::Renderer<Font = Font>,
    {
        let text_color = self.line_text_color(line.kind);
        let gutter = format_gutter(line.old_line, line.new_line);
        let prefix = prefix_for_kind(line.kind);
        let bounds = render.bounds;

        self.draw_text(
            renderer,
            &gutter,
            TextRenderParams {
                width: GUTTER_WIDTH - 16.0,
                height: ROW_HEIGHT,
                position: Point::new(bounds.x + 8.0, render.y + 4.0),
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
                position: Point::new(bounds.x + GUTTER_WIDTH + 8.0, render.y + 4.0),
                color: text_color,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );

        let position = Point::new(bounds.x + GUTTER_WIDTH + PREFIX_WIDTH + 8.0, render.y + 4.0);

        if line.syntax.is_empty() {
            self.draw_text(
                renderer,
                &line.content,
                TextRenderParams {
                    width: render.content_width,
                    height: render.height,
                    position,
                    color: text_color,
                    clip_bounds: render.content_clip_bounds,
                    wrapping: text::Wrapping::WordOrGlyph,
                },
            );
        } else {
            self.draw_syntax_text(
                renderer,
                &line.content,
                &line.syntax,
                SyntaxRenderParams {
                    fallback: text_color,
                    content_width: render.content_width,
                    position,
                    clip_bounds: render.content_clip_bounds,
                },
            );
        }
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for DiffView<'a>
where
    Renderer: text::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            selected_file: self.selected_file,
            pending_file_jump: None,
            vertical_offset: 0.0,
            horizontal_offset: 0.0,
            horizontal_drag: None,
            shift_pressed: false,
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();

        if state.selected_file != self.selected_file {
            state.pending_file_jump = Some(self.selected_file);
            state.horizontal_offset = 0.0;
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
        let state = tree.state.downcast_mut::<State>();

        if let Some(file_index) = state
            .pending_file_jump
            .take()
            .or_else(|| (state.selected_file != self.selected_file).then_some(self.selected_file))
        {
            let max_vertical = (self.content_height(bounds.width) - bounds.height).max(0.0);
            state.vertical_offset = self
                .file_offset(file_index, bounds.width)
                .clamp(0.0, max_vertical);
            state.selected_file = file_index;
            state.horizontal_offset = 0.0;
            shell.request_redraw();
        }

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(cursor_position) = cursor.position_over(bounds) else {
                    return;
                };
                let Some(scroller) = horizontal_scroller_bounds(
                    bounds,
                    state.horizontal_offset,
                    self.max_horizontal_offset(bounds.width),
                ) else {
                    return;
                };

                if scroller.contains(cursor_position) {
                    state.horizontal_drag = Some(cursor_position.x - scroller.x);
                } else if horizontal_scrollbar_bounds(bounds).contains(cursor_position) {
                    let max_horizontal = self.max_horizontal_offset(bounds.width);
                    state.horizontal_offset = horizontal_offset_from_cursor(
                        bounds,
                        max_horizontal,
                        cursor_position.x,
                        scroller.width / 2.0,
                    );
                } else {
                    return;
                }

                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(grab_offset) = state.horizontal_drag else {
                    return;
                };

                let max_horizontal = self.max_horizontal_offset(bounds.width);
                state.horizontal_offset =
                    horizontal_offset_from_cursor(bounds, max_horizontal, position.x, grab_offset);
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.horizontal_drag.take().is_some() {
                    shell.capture_event();
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(_cursor_position) = cursor.position_over(bounds) else {
                    return;
                };

                let mut movement = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => {
                        Vector::new(-x * HORIZONTAL_STEP, -y * ROW_HEIGHT * 3.0)
                    }
                    mouse::ScrollDelta::Pixels { x, y } => Vector::new(-x, -y),
                };

                if state.shift_pressed && movement.x == 0.0 {
                    movement.x = movement.y;
                    movement.y = 0.0;
                }

                if movement.y != 0.0 {
                    let max_vertical = (self.content_height(bounds.width) - bounds.height).max(0.0);
                    state.vertical_offset =
                        (state.vertical_offset + movement.y).clamp(0.0, max_vertical);
                }

                if movement.x != 0.0 {
                    let max_horizontal = self.max_horizontal_offset(bounds.width);
                    state.horizontal_offset =
                        (state.horizontal_offset + movement.x).clamp(0.0, max_horizontal);
                }

                shell.capture_event();
                shell.request_redraw();
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.shift_pressed = modifiers.shift();
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

        let state = tree.state.downcast_ref::<State>();
        let content_width = self.content_width(bounds.width);
        let content_clip_bounds = Rectangle {
            x: bounds.x + GUTTER_WIDTH + PREFIX_WIDTH,
            y: bounds.y,
            width: (bounds.width - GUTTER_WIDTH - PREFIX_WIDTH).max(1.0),
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
            let mut content_y = 0.0;
            let visible_capacity = (bounds.height / ROW_HEIGHT).ceil() as usize + 8;
            let mut visible_file_headers = Vec::new();
            let mut visible_metadata = Vec::new();
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

                for (line_index, _) in file.metadata.iter().enumerate() {
                    let row_top = content_y;
                    push_if_visible(
                        &mut visible_metadata,
                        VisibleMetadata {
                            file_index,
                            line_index,
                            y: bounds.y + (row_top - visible_top),
                        },
                        row_top,
                        METADATA_ROW_HEIGHT,
                        visible_top,
                        visible_bottom,
                    );
                    content_y += METADATA_ROW_HEIGHT;
                }

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
                GUTTER_WIDTH,
                bounds.height,
                self.palette.gutter_background,
            );
            self.draw_background(
                renderer,
                bounds.x + GUTTER_WIDTH,
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

            for header in &visible_file_headers {
                let file = &self.files[header.file_index];
                let summary = format!(
                    "{}  +{} -{}  {} hunk(s)",
                    file.status,
                    file.additions,
                    file.deletions,
                    file.hunks.len()
                );

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
                        width: (bounds.width - 200.0).max(1.0),
                        height: ROW_HEIGHT,
                        position: Point::new(bounds.x + 12.0, header.y + 5.0),
                        color: self.palette.text,
                        clip_bounds: bounds,
                        wrapping: text::Wrapping::WordOrGlyph,
                    },
                );
                self.draw_text(
                    renderer,
                    &summary,
                    TextRenderParams {
                        width: 180.0,
                        height: ROW_HEIGHT,
                        position: Point::new(
                            (bounds.x + bounds.width - 188.0).max(bounds.x + 12.0),
                            header.y + 6.0,
                        ),
                        color: self.palette.text_muted,
                        clip_bounds: bounds,
                        wrapping: text::Wrapping::None,
                    },
                );
            }

            for metadata in &visible_metadata {
                let line = &self.files[metadata.file_index].metadata[metadata.line_index];
                self.draw_text(
                    renderer,
                    line,
                    TextRenderParams {
                        width: content_width,
                        height: METADATA_ROW_HEIGHT,
                        position: Point::new(
                            bounds.x + GUTTER_WIDTH + PREFIX_WIDTH + 8.0 - state.horizontal_offset,
                            metadata.y + 2.0,
                        ),
                        color: self.palette.text_muted,
                        clip_bounds: content_clip_bounds,
                        wrapping: text::Wrapping::None,
                    },
                );
            }

            for header in &visible_hunk_headers {
                let hunk = &self.files[header.file_index].hunks[header.hunk_index];
                self.draw_background(
                    renderer,
                    bounds.x + GUTTER_WIDTH,
                    header.y + HUNK_HEADER_HEIGHT - 1.0,
                    (bounds.width - GUTTER_WIDTH).max(1.0),
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
                            bounds.x + 14.0 - state.horizontal_offset,
                            header.y + 5.0,
                        ),
                        color: self.palette.text,
                        clip_bounds: bounds,
                        wrapping: text::Wrapping::None,
                    },
                );
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
                );
            }

            if let Some(scroller) = horizontal_scroller_bounds(
                bounds,
                state.horizontal_offset,
                self.max_horizontal_offset(bounds.width),
            ) {
                let rail = horizontal_scrollbar_bounds(bounds);
                self.draw_background(
                    renderer,
                    rail.x,
                    rail.y,
                    rail.width,
                    rail.height,
                    self.palette.panel,
                );
                self.draw_background(
                    renderer,
                    scroller.x,
                    scroller.y + (scroller.height - SCROLLBAR_THUMB_THICKNESS) / 2.0,
                    scroller.width,
                    SCROLLBAR_THUMB_THICKNESS,
                    self.palette.text_muted,
                );
            }
        });
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        let over_scrollbar = cursor
            .position_over(bounds)
            .is_some_and(|position| horizontal_scrollbar_bounds(bounds).contains(position));

        if over_scrollbar {
            mouse::Interaction::Pointer
        } else if cursor.position_over(bounds).is_some() {
            mouse::Interaction::AllScroll
        } else {
            mouse::Interaction::None
        }
    }
}

impl DiffView<'_> {
    fn text_width(&self, content: &str) -> f32 {
        (content.chars().count() as f32 * self.char_width() + 16.0).max(1.0)
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
            line_height: text::LineHeight::Absolute(Pixels(ROW_HEIGHT)),
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

    fn draw_syntax_text<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        spans: &[SyntaxSpan],
        render: SyntaxRenderParams,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let chars_per_line = (render.content_width / self.char_width()).floor().max(1.0) as usize;
        let char_count = content.chars().count().max(1);

        for line_start_char in (0..char_count).step_by(chars_per_line) {
            let line_end_char = (line_start_char + chars_per_line).min(char_count);
            let line_start_byte = byte_index_at_char(content, line_start_char);
            let line_end_byte = byte_index_at_char(content, line_end_char);
            let mut cursor = line_start_byte;
            let y = render.position.y + (line_start_char / chars_per_line) as f32 * ROW_HEIGHT;

            for span in spans {
                let start = span.start.max(line_start_byte);
                let end = span.end.min(line_end_byte);

                if start >= end {
                    continue;
                }

                if cursor < start {
                    self.draw_text_fragment(
                        renderer,
                        content,
                        FragmentRenderParams {
                            line_start: line_start_byte,
                            start: cursor,
                            end: start,
                            color: render.fallback,
                            position: render.position,
                            y,
                            clip_bounds: render.clip_bounds,
                        },
                    );
                }

                self.draw_text_fragment(
                    renderer,
                    content,
                    FragmentRenderParams {
                        line_start: line_start_byte,
                        start,
                        end,
                        color: self.syntax_color(span.kind),
                        position: render.position,
                        y,
                        clip_bounds: render.clip_bounds,
                    },
                );
                cursor = end;
            }

            if cursor < line_end_byte {
                self.draw_text_fragment(
                    renderer,
                    content,
                    FragmentRenderParams {
                        line_start: line_start_byte,
                        start: cursor,
                        end: line_end_byte,
                        color: render.fallback,
                        position: render.position,
                        y,
                        clip_bounds: render.clip_bounds,
                    },
                );
            }
        }
    }

    fn draw_text_fragment<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        render: FragmentRenderParams,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let Some(fragment) = content.get(render.start..render.end) else {
            return;
        };

        let x = render.position.x
            + content[render.line_start..render.start].chars().count() as f32 * self.char_width();
        let width = self.text_width(fragment);

        self.draw_text(
            renderer,
            fragment,
            TextRenderParams {
                width,
                height: ROW_HEIGHT,
                position: Point::new(x, render.y),
                color: render.color,
                clip_bounds: render.clip_bounds,
                wrapping: text::Wrapping::None,
            },
        );
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
            SyntaxKind::String => self.palette.note_text,
            SyntaxKind::Number => self.palette.modified_token,
            SyntaxKind::Keyword => self.palette.modified_token,
            SyntaxKind::Function => self.palette.addition_text,
            SyntaxKind::Type => self.palette.deletion_text,
            SyntaxKind::Property => self.palette.text,
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

fn format_gutter(old_line: Option<usize>, new_line: Option<usize>) -> String {
    let old = old_line.map(|line| line.to_string()).unwrap_or_default();
    let new = new_line.map(|line| line.to_string()).unwrap_or_default();
    format!("{old:>5} {new:>5}")
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

fn byte_index_at_char(content: &str, char_index: usize) -> usize {
    content
        .char_indices()
        .nth(char_index)
        .map_or(content.len(), |(index, _)| index)
}

fn horizontal_scrollbar_bounds(bounds: Rectangle) -> Rectangle {
    Rectangle {
        x: bounds.x + SCROLLBAR_MARGIN,
        y: bounds.y + bounds.height - SCROLLBAR_THICKNESS - SCROLLBAR_MARGIN,
        width: (bounds.width - SCROLLBAR_MARGIN * 2.0).max(1.0),
        height: SCROLLBAR_THICKNESS,
    }
}

fn horizontal_scroller_bounds(
    bounds: Rectangle,
    horizontal_offset: f32,
    max_horizontal_offset: f32,
) -> Option<Rectangle> {
    if max_horizontal_offset <= 0.0 {
        return None;
    }

    let rail = horizontal_scrollbar_bounds(bounds);
    let content_width = bounds.width + max_horizontal_offset;
    let width = (rail.width * bounds.width / content_width).clamp(32.0, rail.width);
    let travel = (rail.width - width).max(1.0);
    let x = rail.x + travel * (horizontal_offset / max_horizontal_offset).clamp(0.0, 1.0);

    Some(Rectangle { x, width, ..rail })
}

fn horizontal_offset_from_cursor(
    bounds: Rectangle,
    max_horizontal_offset: f32,
    cursor_x: f32,
    grab_offset: f32,
) -> f32 {
    let Some(scroller) = horizontal_scroller_bounds(bounds, 0.0, max_horizontal_offset) else {
        return 0.0;
    };

    let rail = horizontal_scrollbar_bounds(bounds);
    let travel = (rail.width - scroller.width).max(1.0);
    let scroller_x = (cursor_x - grab_offset).clamp(rail.x, rail.x + travel);

    ((scroller_x - rail.x) / travel * max_horizontal_offset).clamp(0.0, max_horizontal_offset)
}

impl<'a, Message, Renderer> From<DiffView<'a>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(diff_view: DiffView<'a>) -> Self {
        Element::new(diff_view)
    }
}
