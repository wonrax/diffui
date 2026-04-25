use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{
    Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size, Theme,
    Vector, alignment,
};

const ROW_HEIGHT: f32 = 24.0;
const HEADER_HEIGHT: f32 = 30.0;
const GUTTER_WIDTH: f32 = 112.0;
const PREFIX_WIDTH: f32 = 24.0;
const HORIZONTAL_STEP: f32 = 48.0;
const CHANGE_MARK_WIDTH: f32 = 3.0;

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
    pub hunk_header: Color,
    pub addition_background: Color,
    pub deletion_background: Color,
    pub note_background: Color,
    pub gutter_background: Color,
    pub border: Color,
}

pub struct DiffView<'a> {
    hunks: &'a [DiffHunkView],
    file_key: usize,
    palette: Palette,
    font: Font,
    text_size: f32,
}

#[derive(Debug)]
struct State {
    file_key: usize,
    vertical_offset: f32,
    horizontal_offset: f32,
}

#[derive(Debug, Clone, Copy)]
struct RowRenderParams {
    bounds: Rectangle,
    content_clip_bounds: Rectangle,
    y: f32,
    horizontal_offset: f32,
}

#[derive(Debug, Clone, Copy)]
struct FragmentRenderParams {
    position: Point,
    color: Color,
    clip_bounds: Rectangle,
}

#[derive(Debug, Clone, Copy)]
struct VisibleHeader {
    hunk_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleRow {
    hunk_index: usize,
    line_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleBand {
    kind: DiffLineKind,
    y: f32,
    height: f32,
}

impl<'a> DiffView<'a> {
    pub fn new(
        hunks: &'a [DiffHunkView],
        file_key: usize,
        palette: Palette,
        font: Font,
        text_size: f32,
    ) -> Self {
        Self {
            hunks,
            file_key,
            palette,
            font,
            text_size,
        }
    }

    fn content_height(&self) -> f32 {
        self.hunks
            .iter()
            .map(|hunk| HEADER_HEIGHT + hunk.lines.len() as f32 * ROW_HEIGHT)
            .sum()
    }

    fn max_horizontal_offset(&self, viewport_width: f32) -> f32 {
        let available_width = (viewport_width - GUTTER_WIDTH - PREFIX_WIDTH - 16.0).max(1.0);
        (self.max_content_chars() as f32 * self.char_width() - available_width).max(0.0)
    }

    fn char_width(&self) -> f32 {
        self.text_size * 0.62
    }

    fn max_content_chars(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| {
                std::iter::once(hunk.header.as_str())
                    .chain(hunk.lines.iter().map(|line| line.content.as_str()))
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

        let bounds = render.bounds;
        let y = render.y;
        let gutter = format_gutter(line.old_line, line.new_line);
        let prefix = prefix_for_kind(line.kind);
        let text_width = self.text_width(&line.content);

        self.draw_text(
            renderer,
            &gutter,
            GUTTER_WIDTH - 16.0,
            Point::new(bounds.x + 8.0, y + 4.0),
            self.palette.text_muted,
            bounds,
        );
        self.draw_text(
            renderer,
            prefix,
            PREFIX_WIDTH,
            Point::new(bounds.x + GUTTER_WIDTH + 8.0, y + 4.0),
            text_color,
            bounds,
        );
        let content_position = Point::new(
            bounds.x + GUTTER_WIDTH + PREFIX_WIDTH + 8.0 - render.horizontal_offset,
            y + 4.0,
        );

        if line.syntax.is_empty() {
            self.draw_text(
                renderer,
                &line.content,
                text_width,
                content_position,
                text_color,
                render.content_clip_bounds,
            );
        } else {
            self.draw_syntax_text(
                renderer,
                &line.content,
                &line.syntax,
                content_position,
                text_color,
                render.content_clip_bounds,
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
            file_key: self.file_key,
            vertical_offset: 0.0,
            horizontal_offset: 0.0,
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();

        if state.file_key != self.file_key {
            *state = State {
                file_key: self.file_key,
                vertical_offset: 0.0,
                horizontal_offset: 0.0,
            };
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

        let Some(_cursor_position) = cursor.position_over(bounds) else {
            return;
        };

        let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event else {
            return;
        };

        let state = tree.state.downcast_mut::<State>();
        let movement = match *delta {
            mouse::ScrollDelta::Lines { x, y } => {
                Vector::new(-x * HORIZONTAL_STEP, -y * ROW_HEIGHT * 3.0)
            }
            mouse::ScrollDelta::Pixels { x, y } => Vector::new(-x, -y),
        };

        let max_vertical = (self.content_height() - bounds.height).max(0.0);
        state.vertical_offset = (state.vertical_offset + movement.y).clamp(0.0, max_vertical);

        if movement.x != 0.0 {
            let max_horizontal = self.max_horizontal_offset(bounds.width);
            state.horizontal_offset =
                (state.horizontal_offset + movement.x).clamp(0.0, max_horizontal);
        }

        shell.capture_event();
        shell.request_redraw();
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
            let visible_capacity = (bounds.height / ROW_HEIGHT).ceil() as usize + 4;
            let mut visible_headers = Vec::new();
            let mut visible_rows = Vec::with_capacity(visible_capacity);
            let mut visible_bands = Vec::new();

            let mut hunk_index = 0;

            for hunk in self.hunks {
                let lines_height = hunk.lines.len() as f32 * ROW_HEIGHT;
                let hunk_height = HEADER_HEIGHT + lines_height;
                let hunk_top = content_y;
                let hunk_bottom = hunk_top + hunk_height;

                if hunk_bottom < visible_top {
                    content_y = hunk_bottom;
                    hunk_index += 1;
                    continue;
                }

                if hunk_top > visible_bottom {
                    break;
                }

                let header_screen_y = bounds.y + (hunk_top - visible_top);
                if header_screen_y <= bounds.y + bounds.height
                    && header_screen_y + HEADER_HEIGHT >= bounds.y
                {
                    visible_headers.push(VisibleHeader {
                        hunk_index,
                        y: header_screen_y,
                    });
                }

                let lines_top = hunk_top + HEADER_HEIGHT;
                let first_line = ((visible_top - lines_top) / ROW_HEIGHT).floor().max(0.0) as usize;
                let last_line =
                    ((visible_bottom - lines_top) / ROW_HEIGHT).ceil().max(0.0) as usize;
                let line_start = first_line.min(hunk.lines.len());
                let line_end = (last_line + 1).min(hunk.lines.len());

                for line_idx in line_start..line_end {
                    let line_content_y = lines_top + line_idx as f32 * ROW_HEIGHT;
                    let y = bounds.y + (line_content_y - visible_top);

                    if y + ROW_HEIGHT < bounds.y || y > bounds.y + bounds.height {
                        continue;
                    }

                    visible_rows.push(VisibleRow {
                        hunk_index,
                        line_index: line_idx,
                        y,
                    });

                    push_visible_band(&mut visible_bands, hunk.lines[line_idx].kind, y);
                }

                content_y = hunk_bottom;
                hunk_index += 1;
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

            for header in &visible_headers {
                self.draw_background(
                    renderer,
                    bounds.x + GUTTER_WIDTH,
                    header.y + HEADER_HEIGHT - 1.0,
                    (bounds.width - GUTTER_WIDTH).max(1.0),
                    1.0,
                    self.palette.hunk_header,
                );
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y,
                    CHANGE_MARK_WIDTH,
                    HEADER_HEIGHT,
                    self.palette.modified_token,
                );
                self.draw_text(
                    renderer,
                    &self.hunks[header.hunk_index].header,
                    self.text_width(&self.hunks[header.hunk_index].header),
                    Point::new(bounds.x + 14.0 - state.horizontal_offset, header.y + 7.0),
                    self.palette.text,
                    bounds,
                );
            }

            for row in &visible_rows {
                let line = &self.hunks[row.hunk_index].lines[row.line_index];
                self.draw_row(
                    renderer,
                    line,
                    RowRenderParams {
                        bounds,
                        content_clip_bounds,
                        y: row.y,
                        horizontal_offset: state.horizontal_offset,
                    },
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
        if cursor.position_over(layout.bounds()).is_some() {
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

    fn make_text(&self, content: &str, width: f32) -> text::Text<String, Font> {
        text::Text {
            content: content.to_owned(),
            bounds: Size::new(width.max(1.0), ROW_HEIGHT),
            size: Pixels(self.text_size),
            line_height: text::LineHeight::Absolute(Pixels(ROW_HEIGHT)),
            font: self.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::None,
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

    fn draw_text<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        width: f32,
        position: Point,
        color: Color,
        clip_bounds: Rectangle,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        renderer.fill_text(self.make_text(content, width), position, color, clip_bounds);
    }

    fn draw_syntax_text<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        spans: &[SyntaxSpan],
        position: Point,
        fallback: Color,
        clip_bounds: Rectangle,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let mut cursor = 0;

        for span in spans {
            if span.start > cursor {
                self.draw_text_fragment(
                    renderer,
                    content,
                    cursor,
                    span.start,
                    FragmentRenderParams {
                        position,
                        color: fallback,
                        clip_bounds,
                    },
                );
            }

            self.draw_text_fragment(
                renderer,
                content,
                span.start,
                span.end,
                FragmentRenderParams {
                    position,
                    color: self.syntax_color(span.kind),
                    clip_bounds,
                },
            );
            cursor = span.end;
        }

        if cursor < content.len() {
            self.draw_text_fragment(
                renderer,
                content,
                cursor,
                content.len(),
                FragmentRenderParams {
                    position,
                    color: fallback,
                    clip_bounds,
                },
            );
        }
    }

    fn draw_text_fragment<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        start: usize,
        end: usize,
        render: FragmentRenderParams,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        if start >= end {
            return;
        }

        let Some(fragment) = content.get(start..end) else {
            return;
        };

        let x = render.position.x + content[..start].chars().count() as f32 * self.char_width();
        let width = self.text_width(fragment);
        self.draw_text(
            renderer,
            fragment,
            width,
            Point::new(x, render.position.y),
            render.color,
            render.clip_bounds,
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

fn push_visible_band(bands: &mut Vec<VisibleBand>, kind: DiffLineKind, y: f32) {
    if kind == DiffLineKind::Context {
        return;
    }

    match bands.last_mut() {
        Some(band) if band.kind == kind && (band.y + band.height - y).abs() < 0.5 => {
            band.height += ROW_HEIGHT;
        }
        _ => bands.push(VisibleBand {
            kind,
            y,
            height: ROW_HEIGHT,
        }),
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

impl<'a, Message, Renderer> From<DiffView<'a>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Renderer: text::Renderer<Font = Font> + 'a,
{
    fn from(diff_view: DiffView<'a>) -> Self {
        Element::new(diff_view)
    }
}
