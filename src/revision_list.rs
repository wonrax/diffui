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
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    text::{self, Paragraph, Text},
    widget::{Tree, tree},
};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow,
    Size, Theme, Vector, alignment,
};
use jj_lib::graph::GraphEdgeType;

use crate::graph::LaneFrame;
use crate::graph_view::{
    RevisionGraphStyle, draw_continuation_row, draw_revision_row, lane_strip_width,
};

const LINE_SCROLL_ROWS: f32 = 1.5;
const PIXEL_SCROLL_SCALE: f32 = 0.65;

#[derive(Debug, Clone, Copy)]
pub struct RevisionListStyle {
    pub graph: RevisionGraphStyle,
    pub revision_row_height: f32,
    pub file_row_height: f32,
    pub gutter_padding: f32,
    pub content_padding: f32,
    pub background: Color,
    pub selected_background: Color,
    pub border: Color,
    pub muted_text: Color,
    pub subtle_text: Color,
    pub accent_text: Color,
    pub file_count_background: Color,
    pub indicator_radius: f32,
    pub small_text_size: f32,
    pub caption_text_size: f32,
    pub primary_font: Font,
    pub file_badge_width: f32,
    pub file_row_gap: f32,
    pub file_row_right_pad: f32,
    pub tooltip_background: Color,
    pub tooltip_text: Color,
    pub tooltip_border: Color,
    pub tooltip_radius: f32,
    pub tooltip_padding: f32,
    pub tooltip_gap: f32,
}

#[derive(Debug, Clone)]
pub struct IndicatorChip {
    pub label: String,
    pub background: Color,
    pub text_color: Color,
}

#[derive(Debug, Clone)]
pub struct RevisionRowView {
    pub selection_key: RowSelectionKey,
    pub change_id_prefix: String,
    pub change_id_suffix: String,
    pub description: String,
    pub description_color: Color,
    pub detail: String,
    pub indicators: Vec<IndicatorChip>,
    pub file_count_chip: Option<String>,
    pub frame: LaneFrame,
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

pub struct RevisionList<Message> {
    items: Vec<Item>,
    selected_row: Option<RowSelectionKey>,
    selected_file: Option<usize>,
    style: RevisionListStyle,
    width: Length,
    on_select_revision: fn(RowSelectionKey) -> Message,
    on_select_file: fn(usize) -> Message,
}

impl<Message> RevisionList<Message> {
    pub fn new(
        items: Vec<Item>,
        selected_row: Option<RowSelectionKey>,
        selected_file: Option<usize>,
        style: RevisionListStyle,
        on_select_revision: fn(RowSelectionKey) -> Message,
        on_select_file: fn(usize) -> Message,
    ) -> Self {
        Self {
            items,
            selected_row,
            selected_file,
            style,
            width: Length::Fill,
            on_select_revision,
            on_select_file,
        }
    }

    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    fn row_height(&self, item: &Item) -> f32 {
        match item {
            Item::Revision(_) => self.style.revision_row_height,
            Item::File(_) => self.style.file_row_height,
        }
    }

    fn content_height(&self) -> f32 {
        self.items.iter().map(|item| self.row_height(item)).sum()
    }

    fn item_gutter_width(&self, item: &Item) -> f32 {
        let lanes = match item {
            Item::Revision(row) => row.frame.lane_count(),
            Item::File(row) => row.continuation.len(),
        };
        lane_strip_width(lanes, &self.style.graph) + self.style.gutter_padding
    }

    fn row_at_offset(&self, offset: f32) -> Option<usize> {
        let mut y = 0.0;
        for (idx, item) in self.items.iter().enumerate() {
            let h = self.row_height(item);
            if offset < y + h {
                return Some(idx);
            }
            y += h;
        }
        None
    }

    fn row_top(&self, item_index: usize) -> f32 {
        self.items
            .iter()
            .take(item_index)
            .map(|item| self.row_height(item))
            .sum()
    }
}

struct State<Paragraph> {
    vertical_offset: f32,
    /// Item index of the file row currently hovered (only set for files,
    /// not revisions). Drives the tooltip overlay.
    hovered_file_item: Option<usize>,
    /// Last cursor position observed inside the widget bounds, in screen
    /// coordinates. Used to anchor the tooltip.
    cursor_position: Option<Point>,
    paragraphs: RefCell<Vec<Paragraph>>,
}

impl<Paragraph> State<Paragraph> {
    fn new() -> Self {
        Self {
            vertical_offset: 0.0,
            hovered_file_item: None,
            cursor_position: None,
            paragraphs: RefCell::new(Vec::new()),
        }
    }
}

impl<Message, Renderer> Widget<Message, Theme, Renderer> for RevisionList<Message>
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
        let max_vertical = (self.content_height() - bounds.height).max(0.0);
        if state.vertical_offset > max_vertical {
            state.vertical_offset = max_vertical;
            shell.request_redraw();
        }

        let recompute_hover = |state: &mut State<Renderer::Paragraph>, this: &Self| {
            let Some(pos) = state.cursor_position else {
                state.hovered_file_item = None;
                return;
            };
            if !bounds.contains(pos) {
                state.hovered_file_item = None;
                return;
            }
            let local_y = pos.y - bounds.y + state.vertical_offset;
            state.hovered_file_item = this
                .row_at_offset(local_y)
                .filter(|&idx| matches!(this.items.get(idx), Some(Item::File(_))));
        };

        match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if bounds.contains(*position) {
                    state.cursor_position = Some(*position);
                } else {
                    state.cursor_position = None;
                }
                let prev = state.hovered_file_item;
                recompute_hover(state, self);
                if state.hovered_file_item != prev {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::CursorLeft) => {
                state.cursor_position = None;
                if state.hovered_file_item.take().is_some() {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if cursor.position_over(bounds).is_none() {
                    return;
                }
                let movement = match *delta {
                    mouse::ScrollDelta::Lines { x: _, y } => {
                        Vector::new(0.0, -y * self.style.revision_row_height * LINE_SCROLL_ROWS)
                    }
                    mouse::ScrollDelta::Pixels { x: _, y } => {
                        Vector::new(0.0, -y * PIXEL_SCROLL_SCALE)
                    }
                };
                if movement.y != 0.0 {
                    state.vertical_offset =
                        (state.vertical_offset + movement.y).clamp(0.0, max_vertical);
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
                let local_y = cursor_pos.y - bounds.y + state.vertical_offset;
                if let Some(row_idx) = self.row_at_offset(local_y) {
                    match &self.items[row_idx] {
                        Item::Revision(rev) => {
                            shell.publish((self.on_select_revision)(rev.selection_key.clone()));
                            shell.capture_event();
                        }
                        Item::File(f) => {
                            shell.publish((self.on_select_file)(f.file_index));
                            shell.capture_event();
                        }
                    }
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
        let item_idx = state.hovered_file_item?;
        let cursor_pos = state.cursor_position?;
        let Some(Item::File(file)) = self.items.get(item_idx) else {
            return None;
        };
        let bounds = layout.bounds();
        let row_top = self.row_top(item_idx) - state.vertical_offset;
        let row_screen_y = bounds.y + row_top;
        let row_height = self.style.file_row_height;

        let measure_para = make_paragraph::<Renderer>(
            &file.raw_path,
            self.style.caption_text_size,
            self.style.primary_font,
        );
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
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.position_over(layout.bounds()).is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
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
        let visible_bottom = visible_top + bounds.height;

        renderer.with_layer(bounds, |renderer| {
            fill_background(renderer, bounds, self.style.background);

            let mut y_cursor = 0.0;
            for item in &self.items {
                let row_h = self.row_height(item);
                let row_top_local = y_cursor;
                let row_bot_local = y_cursor + row_h;
                y_cursor = row_bot_local;
                if row_bot_local < visible_top || row_top_local > visible_bottom {
                    continue;
                }
                let screen_y = bounds.y + (row_top_local - visible_top);
                let row_bounds = Rectangle {
                    x: bounds.x,
                    y: screen_y,
                    width: bounds.width,
                    height: row_h,
                };
                let gutter_total = self.item_gutter_width(item);

                match item {
                    Item::Revision(rev) => {
                        self.draw_revision(
                            renderer,
                            row_bounds,
                            rev,
                            &state.paragraphs,
                            gutter_total,
                        );
                    }
                    Item::File(f) => {
                        self.draw_file(renderer, row_bounds, f, &state.paragraphs, gutter_total);
                    }
                }
            }
        });
    }
}

impl<Message> RevisionList<Message> {
    fn draw_revision<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        rev: &RevisionRowView,
        paragraphs: &RefCell<Vec<R::Paragraph>>,
        gutter_total: f32,
    ) where
        R: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer,
    {
        let selected = self
            .selected_row
            .as_ref()
            .map(|key| key == &rev.selection_key)
            .unwrap_or(false);

        // Row chrome (selection highlight + bottom border) extends edge-to-edge.
        // The graph is drawn last in this method so its lines & node still sit
        // visually above any chrome painted underneath.
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

        let content_left = row_bounds.x + gutter_total;
        let content_right_pad = self.style.content_padding;
        let row_clip = row_bounds;
        let content_width = (row_bounds.width - gutter_total - content_right_pad).max(1.0);

        // Three-line content stack (title / description / detail) centered
        // vertically inside the row. Visual line height per row is just `size`
        // (cap-to-baseline) — using full `size * 1.4` line_box would push the
        // gaps wider than we want.
        let title_size = self.style.small_text_size;
        let desc_size = self.style.small_text_size;
        let detail_size = self.style.caption_text_size;
        let line_gap = 3.0;
        let stack_height = title_size + line_gap + desc_size + line_gap + detail_size;
        let stack_top = row_bounds.y + ((row_bounds.height - stack_height) / 2.0).max(0.0);
        let title_mid_y = stack_top + title_size / 2.0;
        let desc_mid_y = stack_top + title_size + line_gap + desc_size / 2.0;
        let detail_mid_y =
            stack_top + title_size + line_gap + desc_size + line_gap + detail_size / 2.0;

        let id_size = self.style.small_text_size;
        let prefix_w = measure_text_width::<R>(
            &rev.change_id_prefix,
            id_size,
            self.style.primary_font,
            paragraphs,
        );
        fill_text_centered_y(
            renderer,
            &rev.change_id_prefix,
            content_left,
            title_mid_y,
            prefix_w.max(1.0),
            id_size,
            self.style.accent_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Left,
        );
        let suffix_w = measure_text_width::<R>(
            &rev.change_id_suffix,
            id_size,
            self.style.primary_font,
            paragraphs,
        );
        fill_text_centered_y(
            renderer,
            &rev.change_id_suffix,
            content_left + prefix_w,
            title_mid_y,
            suffix_w.max(1.0),
            id_size,
            self.style.subtle_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Left,
        );

        let chip_gap = 6.0;
        let mut chip_x = content_left + prefix_w + suffix_w + 8.0;
        for chip in &rev.indicators {
            let advance = self.draw_chip(
                renderer,
                paragraphs,
                chip_x,
                title_mid_y,
                &chip.label,
                chip.background,
                chip.text_color,
                row_clip,
            );
            chip_x += advance + chip_gap;
        }
        if let Some(label) = &rev.file_count_chip {
            self.draw_chip(
                renderer,
                paragraphs,
                chip_x,
                title_mid_y,
                label,
                self.style.file_count_background,
                self.style.accent_text,
                row_clip,
            );
        }

        fill_text_truncated(
            renderer,
            &rev.description,
            content_left,
            desc_mid_y,
            content_width,
            desc_size,
            rev.description_color,
            self.style.primary_font,
            row_clip,
        );
        fill_text_truncated(
            renderer,
            &rev.detail,
            content_left,
            detail_mid_y,
            content_width,
            detail_size,
            self.style.subtle_text,
            self.style.primary_font,
            row_clip,
        );

        // Graph painted last so node + lines sit on top of any row chrome.
        let gutter_bounds = Rectangle {
            x: row_bounds.x,
            y: row_bounds.y,
            width: gutter_total - self.style.gutter_padding,
            height: row_bounds.height,
        };
        draw_revision_row(renderer, gutter_bounds, &rev.frame, &self.style.graph);
    }

    fn draw_file<R>(
        &self,
        renderer: &mut R,
        row_bounds: Rectangle,
        f: &FileRowView,
        _paragraphs: &RefCell<Vec<R::Paragraph>>,
        gutter_total: f32,
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
        let badge_h = 18.0;
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
            self.style.indicator_radius,
        );
        fill_text_centered_y(
            renderer,
            &f.status_label,
            content_x + badge_w / 2.0,
            row_mid_y,
            badge_w,
            self.style.caption_text_size,
            f.status_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Center,
        );

        // Layout: [badge] gap [path] gap [+N width=additions_width] gap [-N width=deletions_width] right_pad
        let row_gap = self.style.file_row_gap;
        let right_pad = self.style.file_row_right_pad;
        let minus_x = row_bounds.x + row_bounds.width - f.deletions_width - right_pad;
        let plus_x = minus_x - row_gap - f.additions_width;

        // Path: primary line + (optional) secondary parent line.
        let path_x = content_x + badge_w + row_gap;
        let path_w = (plus_x - path_x - row_gap).max(1.0);

        if f.secondary.is_empty() {
            fill_text_centered_y(
                renderer,
                &f.primary,
                path_x,
                row_mid_y,
                path_w,
                self.style.caption_text_size,
                f.primary_color,
                self.style.primary_font,
                row_clip,
                text::Alignment::Left,
            );
        } else {
            let secondary_size = (self.style.caption_text_size - 2.0).max(9.0);
            // Stack two text rows visually centered on the row mid-line.
            let primary_mid = row_mid_y - secondary_size * 0.55;
            let secondary_mid = row_mid_y + self.style.caption_text_size * 0.55;
            fill_text_centered_y(
                renderer,
                &f.primary,
                path_x,
                primary_mid,
                path_w,
                self.style.caption_text_size,
                f.primary_color,
                self.style.primary_font,
                row_clip,
                text::Alignment::Left,
            );
            fill_text_centered_y(
                renderer,
                &f.secondary,
                path_x,
                secondary_mid,
                path_w,
                secondary_size,
                f.secondary_color,
                self.style.primary_font,
                row_clip,
                text::Alignment::Left,
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
            self.style.caption_text_size,
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
            self.style.caption_text_size,
            f.deletions_text,
            self.style.primary_font,
            row_clip,
            text::Alignment::Left,
        );

        // Graph last — same z-order rationale as draw_revision.
        let gutter_bounds = Rectangle {
            x: row_bounds.x,
            y: row_bounds.y,
            width: gutter_total - self.style.gutter_padding,
            height: row_bounds.height,
        };
        draw_continuation_row(renderer, gutter_bounds, &f.continuation, &self.style.graph);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chip<R: text::Renderer<Font = Font>>(
        &self,
        renderer: &mut R,
        paragraphs: &RefCell<Vec<R::Paragraph>>,
        x: f32,
        center_y: f32,
        label: &str,
        background: Color,
        text_color: Color,
        clip: Rectangle,
    ) -> f32 {
        let size = self.style.caption_text_size;
        let label_w = measure_text_width::<R>(label, size, self.style.primary_font, paragraphs);
        let pad_x = 5.0;
        // Tight box: just enough vertical room for the cap-height plus a hair
        // of breathing room. Anything more makes the chip dwarf the title text.
        let chip_h = (size + 3.0).round();
        let chip_w = label_w + pad_x * 2.0;
        let chip_top = (center_y - chip_h / 2.0).round();
        fill_quad(
            renderer,
            Rectangle {
                x,
                y: chip_top,
                width: chip_w,
                height: chip_h,
            },
            background,
            self.style.indicator_radius,
        );
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
            self.text_size.width + self.style.tooltip_padding * 2.0,
            self.text_size.height + self.style.tooltip_padding * 2.0,
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
        let mut x = self.row_anchor_x + self.style.tooltip_gap;
        let mut y = self.row_anchor_y + (self.row_height - size.height) / 2.0;
        if x + size.width > viewport.width {
            // Fall back to following the cursor on the left.
            x = (self.cursor.x - size.width - self.style.tooltip_gap).max(4.0);
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
                    radius: iced::border::Radius::from(self.style.tooltip_radius),
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
            bounds.x + self.style.tooltip_padding,
            bounds.y + bounds.height / 2.0,
            bounds.width - self.style.tooltip_padding * 2.0,
            self.style.caption_text_size,
            self.style.tooltip_text,
            self.style.primary_font,
            bounds,
            text::Alignment::Left,
        );
    }
}

impl<Message: 'static, Renderer> From<RevisionList<Message>>
    for Element<'_, Message, Theme, Renderer>
where
    Renderer: text::Renderer<Font = Font> + iced::advanced::graphics::geometry::Renderer + 'static,
{
    fn from(widget: RevisionList<Message>) -> Self {
        Element::new(widget)
    }
}
