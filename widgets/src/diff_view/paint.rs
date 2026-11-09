//! Turning one frame of the view into draw operations.
//!
//! GPUI paints with `&mut App`, and the lines live inside an entity the `App` owns,
//! so a painter can't hold the document and paint at once. So this runs during
//! prepaint with the document borrowed, and everything it decides lands in a
//! [`Plan`]: quads and shaped text, in painter's order, owning what they need. The
//! element then flushes the plan in paint with no document in sight.
//!
//! Order matters and it's simply call order: GPUI stacks primitives as they arrive,
//! so a sticky header over text and a scrollbar over that is just drawing later.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    App, Background, BorderStyle, Bounds, ContentMask, Corners, Edges, Hsla, PaintQuad, Path,
    Pixels, RenderImage, ShapedLine, SharedString, TextAlign, TextRun, Window, WindowTextSystem,
    fill, point, px,
};

use super::element::State;
use super::layout;
use super::layout::{
    ActionHit, FileControl, FileHeaderLayout, GUTTER_HORIZONTAL_PADDING, HEADER_CHIP_GAP,
    HEADER_EDIT_WIDTH, HEADER_LABEL_GAP, HeightIndex, NO_LINE, PREFIX_WIDTH, SplitLayout,
    SplitSide, StatKind, TEXT_X_PADDING, TextPosition, View, VisibleRow, body_position,
    header_position, matches_between, matches_on_line, ordered,
};
use super::line::{GeometryMemo, LineGeometry, body_geometry_key};
use super::{
    AnnotationGutter, AnnotationLabel, CheckState, DiffLine, DiffLineKind, FindOverlay,
    HEADER_HORIZONTAL_PADDING, HEADER_VERTICAL_PADDING, HeaderField, HeaderLine, LineAt,
    SyntaxKind, media,
};
use crate::chip::{self, Chip};
use crate::geometry::{Point, Rectangle};
use crate::hunk_actions::{self, FilePlaced, Placed, Run};
use crate::icons;
use crate::measure;
use crate::status_badge;
use crate::theme::{self, Color, Font};
use crate::typography::TextBox;

// Corner radius of the intra-line word-diff emphasis rectangles (find match
// highlights share it), so token tints read as soft chips rather than hard blocks.
const EMPHASIS_CORNER_RADIUS: f32 = 3.0;
/// Width of the accent bar a picked line wears at its gutter's left edge.
const PICK_BAR_WIDTH: f32 = 3.0;
// Glyph size of the browse button's icon within its square.
const BROWSE_ICON_SIZE: f32 = 13.0;
const HEADER_BUTTON_ICON_SIZE: f32 = 12.0;
const HEADER_EDIT_ICON_SIZE: f32 = 13.0;
const HEADER_EDIT_LABEL_SIZE: f32 = crate::theme::text_size::UI;
const HEADER_EDIT_CONTENT_GAP: f32 = 5.0;
/// Spacing of the hatch on a padded split column.
const HATCH_STEP: f32 = 6.0;

/// One draw operation, in painter's order. They live for one frame, so boxing the big
/// ones would only add an allocation each.
#[allow(clippy::large_enum_variant)]
enum Op {
    Quad(PaintQuad),
    Line {
        line: ShapedLine,
        origin: gpui_kit::Point<Pixels>,
        line_height: Pixels,
        clip: Bounds<Pixels>,
    },
    Code {
        lines: Rc<[ShapedLine]>,
        origin: gpui_kit::Point<Pixels>,
        line_height: Pixels,
        clip: Bounds<Pixels>,
    },
    Image {
        bounds: Bounds<Pixels>,
        data: Arc<RenderImage>,
        clip: Bounds<Pixels>,
    },
    Path(Path<Pixels>, Background),
}

/// What a frame draws; see the module docs.
pub(super) struct Plan {
    ops: Vec<Op>,
    text: Rc<WindowTextSystem>,
}

impl Plan {
    pub(super) fn new(text: Rc<WindowTextSystem>, capacity: usize) -> Self {
        Self {
            ops: Vec::with_capacity(capacity),
            text,
        }
    }

    pub(super) fn flush(self, window: &mut Window, cx: &mut App) {
        for op in self.ops {
            match op {
                Op::Quad(quad) => window.paint_quad(quad),
                Op::Line {
                    line,
                    origin,
                    line_height,
                    clip,
                } => window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    line.paint(origin, line_height, TextAlign::Left, None, window, cx)
                        .ok();
                }),
                Op::Code {
                    lines,
                    origin,
                    line_height,
                    clip,
                } => window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    let mut origin = origin;
                    for line in lines.iter() {
                        line.paint(origin, line_height, TextAlign::Left, None, window, cx)
                            .ok();
                        origin.y += line_height;
                    }
                }),
                Op::Image { bounds, data, clip } => {
                    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                        window
                            .paint_image(bounds, bounds, Corners::default(), data, 0, false)
                            .ok();
                    });
                }
                Op::Path(path, color) => window.paint_path(path, color),
            }
        }
    }

    pub(super) fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: Color) {
        let rect = Rectangle {
            x,
            y,
            width,
            height,
        };
        self.ops.push(Op::Quad(fill(rect.bounds(), color)));
    }

    fn rounded(&mut self, rect: Rectangle, radius: f32, color: Color) {
        if radius >= crate::squircle::MIN_RADIUS
            && let Some(path) =
                crate::squircle::fill_path(rect.bounds(), crate::squircle::Corners::all(radius))
        {
            self.ops.push(Op::Path(path, color.into()));
            return;
        }
        self.ops.push(Op::Quad(
            fill(rect.bounds(), color).corner_radii(px(radius)),
        ));
    }

    fn boxed(
        &mut self,
        rect: Rectangle,
        radius: f32,
        background: Color,
        border: Color,
        style: BorderStyle,
    ) {
        // A dashed edge stays a quad: a path can't dash.
        if radius >= crate::squircle::MIN_RADIUS && matches!(style, BorderStyle::Solid) {
            let corners = crate::squircle::Corners::all(radius);
            if background.a > 0.0
                && let Some(path) = crate::squircle::fill_path(rect.bounds(), corners)
            {
                self.ops.push(Op::Path(path, background.into()));
            }
            if let Some(path) = crate::squircle::stroke_path(rect.bounds(), corners, 1.0) {
                self.ops.push(Op::Path(path, border.into()));
            }
            return;
        }
        self.ops.push(Op::Quad(PaintQuad {
            bounds: rect.bounds(),
            corner_radii: Corners::all(px(radius)),
            background: background.into(),
            border_widths: Edges::all(px(1.0)),
            border_color: Hsla::from(border),
            border_style: style,
        }));
    }

    pub(super) fn outline(&mut self, rect: Rectangle, color: Color) {
        self.boxed(rect, 0.0, theme::TRANSPARENT, color, BorderStyle::Solid);
    }

    pub(super) fn image(&mut self, frame: Rectangle, data: Arc<RenderImage>, clip: Rectangle) {
        self.ops.push(Op::Image {
            bounds: frame.bounds(),
            data,
            clip: clip.bounds(),
        });
    }

    /// One line of text, its line box `line_height` tall with its top at `top`.
    /// Returns its width.
    #[allow(clippy::too_many_arguments)]
    fn line(
        &mut self,
        content: &str,
        font: Font,
        size: f32,
        x: f32,
        top: f32,
        line_height: f32,
        color: Color,
        clip: Rectangle,
    ) -> f32 {
        if content.is_empty() {
            return 0.0;
        }
        let line = self.text.shape_line(
            SharedString::new(content),
            px(size),
            &[text_run(content.len(), font, color)],
            None,
        );
        let width = f32::from(line.width);
        self.ops.push(Op::Line {
            line,
            origin: point(px(x), px(top)),
            line_height: px(line_height),
            clip: clip.bounds(),
        });
        width
    }

    /// An icon centred in `rect`.
    fn icon(&mut self, glyph: &str, size: f32, rect: Rectangle, color: Color, clip: Rectangle) {
        let width = measure::line_width(glyph, size, icons::ICON_FONT);
        let line = TextBox::new(icons::ICON_FONT, size, rect.height);
        let top = line.baseline_for_center(rect.center_y()) - line.baseline();
        self.line(
            glyph,
            icons::ICON_FONT,
            size,
            rect.center_x() - width / 2.0,
            top,
            rect.height,
            color,
            clip,
        );
    }

    /// A chip painted by hand, its left edge at `x`, centred on `center_y`. Returns
    /// its width. The same geometry as [`chip::view`].
    fn chip(&mut self, chip: &Chip, x: f32, center_y: f32, clip: Rectangle) -> f32 {
        let width = chip::full_width(chip);
        let height = chip::height();
        let rect = Rectangle {
            x,
            y: center_y - height / 2.0,
            width,
            height,
        };
        match chip.border_color {
            Some(border) => self.boxed(
                rect,
                chip::RADIUS,
                chip.background,
                border,
                if chip.border_dashed {
                    BorderStyle::Dashed
                } else {
                    BorderStyle::Solid
                },
            ),
            None if chip.background.a > f32::EPSILON => {
                self.rounded(rect, chip::RADIUS, chip.background)
            }
            None => {}
        }
        let mut text_x = x + chip::PAD_X;
        if let Some(glyph) = chip.icon {
            let glyph_width = measure::line_width(glyph, chip::ICON_SIZE, icons::ICON_FONT);
            self.icon(
                glyph,
                chip::ICON_SIZE,
                Rectangle {
                    x: text_x,
                    width: glyph_width,
                    ..rect
                },
                chip.icon_color.unwrap_or(chip.text_color),
                clip,
            );
            text_x += glyph_width + chip::ICON_GAP;
        }
        let size = chip::text_size(chip.font);
        let shift = TextBox::new(chip.font, size, height).trim_shift();
        self.line(
            &chip.label,
            chip.font,
            size,
            text_x,
            rect.y + shift,
            height,
            chip.text_color,
            clip,
        );
        if let Some((suffix, color)) = &chip.suffix {
            self.line(
                suffix,
                chip.font,
                size,
                text_x + measure::line_width(&chip.label, size, chip.font),
                rect.y + shift,
                height,
                *color,
                clip,
            );
        }
        width
    }

    /// A file's status badge in `rect` (see [`status_badge`]).
    fn status_badge(
        &mut self,
        badge: &status_badge::StatusBadge,
        mono: Font,
        rect: Rectangle,
        clip: Rectangle,
    ) {
        self.rounded(rect, status_badge::RADIUS, status_badge::fill_color(badge));
        let font = status_badge::font(mono);
        let width = measure::line_width(badge.letter, status_badge::TEXT_SIZE, font);
        self.line(
            badge.letter,
            font,
            status_badge::TEXT_SIZE,
            rect.center_x() - width / 2.0,
            status_badge::letter_top(mono, rect),
            rect.height,
            badge.ink,
            clip,
        );
    }

    /// Paints a change-selection checkbox; see [`paint_checkbox`].
    #[allow(clippy::too_many_arguments)]
    fn checkbox(
        &mut self,
        rect: Rectangle,
        state: CheckState,
        fill_color: Color,
        muted: Color,
        mark: Color,
        clip: Rectangle,
    ) {
        let checked = state != CheckState::Off;
        self.boxed(
            rect,
            3.0,
            if checked {
                fill_color
            } else {
                theme::TRANSPARENT
            },
            if checked {
                fill_color
            } else {
                Color { a: 0.55, ..muted }
            },
            BorderStyle::Solid,
        );
        match state {
            CheckState::Off => {}
            CheckState::Partial => {
                let width = (rect.width * 0.5).round();
                let height = 2.0;
                self.rounded(
                    Rectangle {
                        x: (rect.center_x() - width / 2.0).round(),
                        y: (rect.center_y() - height / 2.0).round(),
                        width,
                        height,
                    },
                    height / 2.0,
                    mark,
                );
            }
            CheckState::On => self.icon(icons::CHECK, rect.height - 3.0, rect, mark, clip),
        }
    }
}

/// A change-selection checkbox in `rect`, painted now. Checked and mixed share the
/// `fill` and shape and differ only in the mark: a tick, or the short bar of a macOS
/// mixed checkbox (drawn, so it can't be read as the diff's `−` prefix).
#[allow(clippy::too_many_arguments)]
pub fn paint_checkbox(
    rect: Rectangle,
    state: CheckState,
    fill_color: Color,
    muted: Color,
    mark: Color,
    clip: Rectangle,
    window: &mut Window,
    cx: &mut App,
) {
    let mut plan = Plan::new(window.text_system().clone(), 4);
    plan.checkbox(rect, state, fill_color, muted, mark, clip);
    plan.flush(window, cx);
}

fn text_run(len: usize, font: Font, color: Color) -> TextRun {
    TextRun {
        len,
        font: font.gpui(),
        color: Hsla::from(color),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Single right-aligned line-number column for plain source documents.
fn format_gutter_plain(line: Option<usize>, digit_count: usize) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(digit_count);
    if let Some(line) = line {
        let _ = write!(out, "{line:>digit_count$}");
    } else {
        out.extend(std::iter::repeat_n(' ', digit_count));
    }
    out
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

fn centered_text_y(container_y: f32, container_height: f32, text_height: f32) -> f32 {
    container_y + (container_height - text_height) / 2.0
}

/// Shared geometry for byte-range background rects (find matches, intra-line
/// emphasis, selection): the parameters that map a byte range on a wrapped row to
/// screen rectangles.
struct HighlightGeometry {
    char_width: f32,
    /// Wrap width the lane's rows were laid out against: the key their
    /// [`LineGeometry`] is measured and memoized under.
    wrap_width: f32,
    text_x: f32,
    clip_left: f32,
    clip_right: f32,
    row_height: f32,
}

/// Shaped code per `(file, hunk, line, wrap width)`: reused across frames while
/// scrolling, and evicted once it's a screenful or two out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct CodeKey {
    file_index: u32,
    hunk_index: u32,
    line_index: u32,
    wrap_width_bits: u32,
}

const CODE_CACHE_CAP: usize = 1_024;

#[derive(Default)]
pub(super) struct CodeCache {
    entries: HashMap<CodeKey, (u64, Rc<[ShapedLine]>)>,
    frame: u64,
}

impl CodeCache {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(super) fn begin_frame(&mut self) {
        self.frame += 1;
        if self.entries.len() > CODE_CACHE_CAP {
            let frame = self.frame.saturating_sub(1);
            self.entries.retain(|_, (seen, _)| *seen >= frame);
        }
    }
}

impl View<'_> {
    /// [`Self::code_text`] with a throwaway key, for tests of what the painter shapes.
    #[cfg(test)]
    pub(super) fn shaped_for_test(
        &self,
        plan: &Plan,
        line: &DiffLine,
        geometry: &LineGeometry,
        cache: &RefCell<CodeCache>,
    ) -> Rc<[ShapedLine]> {
        cache.borrow_mut().clear();
        let key = CodeKey {
            file_index: 0,
            hunk_index: 0,
            line_index: 0,
            wrap_width_bits: 0,
        };
        self.code_text(plan, line, geometry, key, cache)
    }

    /// Text in the code font on one row's line box, unwrapped. The row's own code
    /// goes through [`Self::code_text`]; this is for strips and captions.
    pub(super) fn code_line(
        &self,
        plan: &mut Plan,
        content: &str,
        x: f32,
        top: f32,
        color: Color,
        clip: Rectangle,
    ) -> f32 {
        plan.line(
            content,
            self.props.font,
            self.props.typography.size,
            x,
            top,
            self.metrics.row_height,
            color,
            clip,
        )
    }

    /// A line number right-aligned in its column, which ends at `column_right`.
    fn line_number(
        &self,
        plan: &mut Plan,
        number: usize,
        column_right: f32,
        top: f32,
        color: Color,
        clip: Rectangle,
    ) {
        let text = number.to_string();
        let width = text.len() as f32 * self.metrics.char_width;
        self.code_line(
            plan,
            &text,
            column_right - layout::NUMBER_PAD_RIGHT - width,
            top,
            color,
            clip,
        );
    }

    /// A line number's colour: the change's own on a changed line, receding
    /// elsewhere.
    fn number_color(&self, kind: DiffLineKind) -> Color {
        let palette = &self.props.palette;
        match kind {
            DiffLineKind::Addition => palette.addition_text,
            DiffLineKind::Deletion => palette.deletion_text,
            _ => palette.text_muted,
        }
    }

    /// Where the baseline of code text drawn with its line box's top at `line_top`
    /// lands, so chips and other runs can sit on it.
    fn code_baseline(&self, line_top: f32) -> f32 {
        line_top
            + TextBox::new(
                self.props.font,
                self.props.typography.size,
                self.metrics.row_height,
            )
            .baseline()
    }

    fn line_text_color(&self, kind: DiffLineKind) -> Color {
        let palette = &self.props.palette;
        match kind {
            DiffLineKind::Addition => palette.addition_text,
            DiffLineKind::Deletion => palette.deletion_text,
            DiffLineKind::Conflict => palette.conflict_marker,
            DiffLineKind::Context => palette.text,
            DiffLineKind::Note => palette.note_text,
        }
    }

    fn changed_line_background_color(&self, kind: DiffLineKind) -> Option<Color> {
        let palette = &self.props.palette;
        match kind {
            DiffLineKind::Addition => Some(palette.addition_background),
            DiffLineKind::Deletion => Some(palette.deletion_background),
            DiffLineKind::Conflict | DiffLineKind::Note => Some(palette.note_background),
            DiffLineKind::Context => None,
        }
    }

    fn syntax_color(&self, kind: SyntaxKind) -> Color {
        let palette = &self.props.palette;
        match kind {
            SyntaxKind::Comment | SyntaxKind::Punctuation => palette.text_muted,
            SyntaxKind::String | SyntaxKind::Number => palette.syntax_literal,
            SyntaxKind::Keyword => palette.syntax_keyword,
            SyntaxKind::Function => palette.syntax_function,
            SyntaxKind::Type => palette.syntax_type,
            SyntaxKind::Property => palette.syntax_property,
        }
    }

    /// The colours over `line`'s display text, as `(end byte, colour)`, in order.
    /// Spans out of order or off a char boundary are skipped, as a highlighter that
    /// raced an edit can hand back.
    fn color_stops(
        &self,
        line: &DiffLine,
        geometry: &LineGeometry,
        base: Color,
    ) -> Vec<(usize, Color)> {
        let content = line.content.as_str();
        let display = geometry.display.as_ref();
        let to_display = |byte: usize| display.map_or(byte, |it| it.disp_of(byte));
        let total = display.map_or(content.len(), |it| it.text.len());
        let mut stops = Vec::with_capacity(line.syntax.len() * 2 + 1);
        let mut cursor = 0usize;
        for span in &line.syntax {
            if span.start < cursor
                || span.start >= span.end
                || span.end > content.len()
                || !content.is_char_boundary(span.start)
                || !content.is_char_boundary(span.end)
            {
                continue;
            }
            if cursor < span.start {
                stops.push((to_display(span.start), base));
            }
            stops.push((to_display(span.end), self.syntax_color(span.kind)));
            cursor = span.end;
        }
        stops.push((total, base));
        stops
    }

    /// `line`'s visual lines, shaped each on its own at the geometry's break points:
    /// the painter breaks a line exactly where the height index counted, because
    /// both read the same table.
    fn code_text(
        &self,
        plan: &Plan,
        line: &DiffLine,
        geometry: &LineGeometry,
        key: CodeKey,
        cache: &RefCell<CodeCache>,
    ) -> Rc<[ShapedLine]> {
        let mut cache = cache.borrow_mut();
        let frame = cache.frame;
        if let Some((seen, lines)) = cache.entries.get_mut(&key) {
            *seen = frame;
            return Rc::clone(lines);
        }
        let color = self.line_text_color(line.kind);
        let stops = self.color_stops(line, geometry, color);
        let text = geometry
            .display
            .as_ref()
            .map_or(line.content.as_str(), |display| display.text.as_str());
        let starts = &geometry.display_starts;
        let font = self.props.font;
        let size = px(self.props.typography.size);
        let mut lines = Vec::with_capacity(starts.len());
        let mut stop = 0usize;
        for (visual, &start) in starts.iter().enumerate() {
            let end = starts.get(visual + 1).copied().unwrap_or(text.len());
            if start >= end {
                lines.push(
                    plan.text
                        .shape_line(SharedString::default(), size, &[], None),
                );
                continue;
            }
            let mut runs = Vec::with_capacity(4);
            let mut at = start;
            while at < end {
                while stops.get(stop).is_some_and(|&(stop_end, _)| stop_end <= at) {
                    stop += 1;
                }
                let (stop_end, run_color) = stops.get(stop).copied().unwrap_or((end, color));
                let run_end = stop_end.min(end);
                runs.push(text_run(run_end - at, font, run_color));
                at = run_end;
            }
            lines.push(plan.text.shape_line(
                SharedString::new(&text[start..end]),
                size,
                &runs,
                None,
            ));
        }
        let lines: Rc<[ShapedLine]> = lines.into();
        cache.entries.insert(key, (frame, Rc::clone(&lines)));
        lines
    }

    /// The highlight lanes of the current mode: the full text area (the `None` lane:
    /// every row in unified mode, full-width files' rows in split mode) plus one lane
    /// per column in side-by-side.
    fn lane_geometries(
        &self,
        bounds: Rectangle,
        split: Option<&SplitLayout>,
        horizontal_offset: f32,
    ) -> Vec<(HighlightGeometry, Option<SplitSide>)> {
        let metrics = &self.metrics;
        let unified = (
            HighlightGeometry {
                char_width: metrics.char_width,
                wrap_width: self.wrap_width(self.content_width(bounds.width)),
                text_x: bounds.x + metrics.gutter_width + metrics.prefix_width + TEXT_X_PADDING
                    - horizontal_offset,
                clip_left: bounds.x + metrics.gutter_width + metrics.prefix_width,
                clip_right: bounds.x + bounds.width,
                row_height: metrics.row_height,
            },
            None,
        );
        let Some(split) = split else {
            return vec![unified];
        };
        let lane = |text_x: f32, clip_left: f32, clip_right: f32| HighlightGeometry {
            char_width: metrics.char_width,
            wrap_width: self.wrap_width(split.text_width),
            text_x,
            clip_left,
            clip_right,
            row_height: metrics.row_height,
        };
        vec![
            unified,
            (
                lane(
                    bounds.x + split.left_text_x - horizontal_offset,
                    bounds.x + split.gutter_width + PREFIX_WIDTH,
                    bounds.x + split.divider_x,
                ),
                Some(SplitSide::Left),
            ),
            (
                lane(
                    bounds.x + split.right_text_x - horizontal_offset,
                    bounds.x + split.right_gutter_x + split.gutter_width + PREFIX_WIDTH,
                    bounds.x + bounds.width,
                ),
                Some(SplitSide::Right),
            ),
        ]
    }

    /// Translucent rectangles behind `[byte_start, byte_end)` of a line, one per visual
    /// sub-line the range crosses on a wrapped row. Columns come from the row's
    /// [`LineGeometry`], which is what keeps the rects glued to the text across tabs and
    /// glyphs wider than a cell.
    ///
    /// `tail` widens the row's last rectangle: the "selected through the end of the
    /// line" cue a selection spanning the whole row draws.
    #[allow(clippy::too_many_arguments)]
    fn byte_range_highlight(
        &self,
        plan: &mut Plan,
        line: &LineGeometry,
        row_y: f32,
        byte_start: usize,
        byte_end: usize,
        color: Color,
        corner_radius: f32,
        tail: f32,
        lane: &HighlightGeometry,
    ) {
        if byte_start >= byte_end {
            return;
        }
        for visual_idx in 0..line.visual_lines() {
            let (vline_start, vline_end) = line.line_range(visual_idx);
            let seg_start = byte_start.max(vline_start);
            let seg_end = byte_end.min(vline_end);
            if seg_start >= seg_end {
                continue;
            }
            let start_column = line.column_at(visual_idx, seg_start);
            let mut x = lane.text_x + start_column * lane.char_width;
            let mut width = (line.column_at(visual_idx, seg_end) - start_column) * lane.char_width;
            if visual_idx + 1 == line.visual_lines() {
                width += tail;
            }
            if x < lane.clip_left {
                let trim = lane.clip_left - x;
                x = lane.clip_left;
                width = (width - trim).max(0.0);
            }
            if x + width > lane.clip_right {
                width = (lane.clip_right - x).max(0.0);
            }
            if width <= 0.0 {
                continue;
            }
            plan.rounded(
                Rectangle {
                    x,
                    y: row_y + visual_idx as f32 * lane.row_height,
                    width,
                    height: lane.row_height,
                },
                corner_radius,
                color,
            );
        }
    }

    /// Find matches intersecting the visible rows. The active match uses a stronger
    /// tint so it stands out against the surrounding non-active hits.
    fn find_highlights(
        &self,
        plan: &mut Plan,
        find: &FindOverlay,
        memo: &RefCell<GeometryMemo>,
        visible_rows: &[VisibleRow],
        lanes: &[(HighlightGeometry, Option<SplitSide>)],
    ) {
        // Matches arrive in document order, so the ones that can land on the visible
        // rows are one contiguous slice. Bracketing it once turns the pass from
        // lanes × rows × *every* match in the document (a `\w` regex over a large diff
        // runs to hundreds of thousands) into lanes × rows × the handful on screen.
        let (Some(first), Some(last)) = (visible_rows.first(), visible_rows.last()) else {
            return;
        };
        let window = matches_between(
            self.matches,
            (first.file_index, first.hunk_index, first.line_bounds().0),
            (last.file_index, last.hunk_index, last.line_bounds().1),
        );
        let visible = &self.matches[window.clone()];
        if visible.is_empty() {
            return;
        }

        for (lane_geometry, lane) in lanes {
            for row in visible_rows {
                let Some(line_index) = row.line_in_lane(*lane) else {
                    continue;
                };
                let Some(line) = self.line(row.file_index, row.hunk_index, line_index) else {
                    continue;
                };
                let (offset, hits) =
                    matches_on_line(visible, (row.file_index, row.hunk_index, line_index));
                if hits.is_empty() {
                    continue;
                }
                let key = body_geometry_key(
                    row.file_index,
                    row.hunk_index,
                    line_index,
                    lane_geometry.wrap_width,
                    lane_geometry.char_width,
                );
                let geometry = self.geometry(memo, key, &line.content, lane_geometry.wrap_width);
                for (i, m) in hits.iter().enumerate() {
                    if !line.content.is_char_boundary(m.byte_start)
                        || !line
                            .content
                            .is_char_boundary(m.byte_end.min(line.content.len()))
                    {
                        continue;
                    }
                    let color = if find.active == Some(window.start + offset + i) {
                        find.active_highlight
                    } else {
                        find.highlight
                    };
                    self.byte_range_highlight(
                        plan,
                        &geometry,
                        row.y,
                        m.byte_start,
                        m.byte_end,
                        color,
                        EMPHASIS_CORNER_RADIUS,
                        0.0,
                        lane_geometry,
                    );
                }
            }
        }
    }

    /// Tint the changed tokens inside modified lines (intra-line word diff). Drawn over
    /// the add/del line band, under selection/find highlights and the text itself.
    fn emphasis_highlights(
        &self,
        plan: &mut Plan,
        memo: &RefCell<GeometryMemo>,
        visible_rows: &[VisibleRow],
        lanes: &[(HighlightGeometry, Option<SplitSide>)],
    ) {
        for (lane_geometry, lane) in lanes {
            for row in visible_rows {
                let Some(line_index) = row.line_in_lane(*lane) else {
                    continue;
                };
                let Some(line) = self.line(row.file_index, row.hunk_index, line_index) else {
                    continue;
                };
                if line.emphasis.is_empty() {
                    continue;
                }
                let color = match line.kind {
                    DiffLineKind::Addition => self.props.palette.addition_emphasis,
                    DiffLineKind::Deletion => self.props.palette.deletion_emphasis,
                    _ => continue,
                };
                let key = body_geometry_key(
                    row.file_index,
                    row.hunk_index,
                    line_index,
                    lane_geometry.wrap_width,
                    lane_geometry.char_width,
                );
                let geometry = self.geometry(memo, key, &line.content, lane_geometry.wrap_width);
                for &(byte_start, byte_end) in &line.emphasis {
                    self.byte_range_highlight(
                        plan,
                        &geometry,
                        row.y,
                        byte_start,
                        byte_end,
                        color,
                        EMPHASIS_CORNER_RADIUS,
                        0.0,
                        lane_geometry,
                    );
                }
            }
        }
    }

    /// Per-side row tints for the split layout (the unified view's merged full-width
    /// bands don't apply: each half tints by its own line's kind). A padded side, with
    /// no line to show, gets a faint neutral wash plus a diagonal hatch.
    fn split_row_tints(
        &self,
        plan: &mut Plan,
        visible_rows: &[VisibleRow],
        split: &SplitLayout,
        bounds: Rectangle,
    ) {
        let palette = &self.props.palette;
        let mut padded: Vec<Rectangle> = Vec::new();
        for row in visible_rows {
            let Some((left, right)) = row.pair else {
                continue;
            };
            let halves = [
                (left, bounds.x, split.divider_x),
                (
                    right,
                    bounds.x + split.right_gutter_x,
                    bounds.width - split.right_gutter_x,
                ),
            ];
            for (member, x, width) in halves {
                let changed = member != NO_LINE;
                let color = if !changed {
                    padded.push(Rectangle {
                        x,
                        y: row.y,
                        width,
                        height: row.height,
                    });
                    Some(Color {
                        a: 0.5,
                        ..palette.gutter_background
                    })
                } else {
                    self.line(row.file_index, row.hunk_index, member as usize)
                        .and_then(|line| self.changed_line_background_color(line.kind))
                };
                if let Some(color) = color {
                    plan.rect(x, row.y, width, row.height, color);
                    if changed {
                        plan.rect(x, row.y, split.gutter_width, row.height, color);
                    }
                }
            }
        }
        if !padded.is_empty() {
            self.padding_hatch(plan, &padded, bounds);
        }
    }

    /// 45° hatching over the padded halves of split rows, one path for the frame's
    /// worth of rects. The stripe phase is anchored globally (`x - y ≡ 0 mod step`) so
    /// the pattern runs seamlessly across vertically adjacent padded rows.
    fn padding_hatch(&self, plan: &mut Plan, rects: &[Rectangle], bounds: Rectangle) {
        let color = Color {
            a: 0.4,
            ..self.props.palette.border
        };
        let mut path: Option<Path<Pixels>> = None;
        let at = |x: f32, y: f32| point(px(x), px(y));
        for rect in rects {
            let top = rect.y.max(bounds.y);
            let bottom = (rect.y + rect.height).min(bounds.y + bounds.height);
            // Stripes are the lines x - y = c. Visible c values run from the
            // bottom-left corner to the top-right one.
            let c_min = rect.x - bottom;
            let c_max = rect.x + rect.width - top;
            let mut c = (c_min / HATCH_STEP).ceil() * HATCH_STEP;
            while c <= c_max {
                let y_start = top.max(rect.x - c);
                let y_end = bottom.min(rect.x + rect.width - c);
                if y_start < y_end {
                    // A thin parallelogram: a stroke through the tessellator would
                    // cost a lot more per frame for the same pixels.
                    let (x0, x1) = (y_start + c, y_end + c);
                    let right_of = |x: f32| (x + 1.0).min(rect.x + rect.width);
                    let path = path.get_or_insert_with(|| Path::new(at(x0, y_start)));
                    path.move_to(at(x0, y_start));
                    path.line_to(at(right_of(x0), y_start));
                    path.line_to(at(right_of(x1), y_end));
                    path.line_to(at(x1, y_end));
                }
                c += HATCH_STEP;
            }
        }
        if let Some(path) = path {
            plan.ops.push(Op::Path(path, color.into()));
        }
    }

    /// One unified-rendered row: unified mode, or a full-width (single-sided) file in
    /// split mode.
    #[allow(clippy::too_many_arguments)]
    fn draw_row(
        &self,
        plan: &mut Plan,
        row: &VisibleRow,
        line: &DiffLine,
        content_width: f32,
        horizontal_offset: f32,
        clip: Rectangle,
        bounds: Rectangle,
        memo: &RefCell<GeometryMemo>,
        code: &RefCell<CodeCache>,
    ) {
        let metrics = &self.metrics;
        let text_color = self.line_text_color(line.kind);
        let annotation_width = self.annotation_width();
        if annotation_width > 0.0
            && let Some(gutter) = &self.props.annotations
            && let Some(label) = line
                .new_line
                .and_then(|number| self.labels.get(number.saturating_sub(1)))
            && !label.id.is_empty()
        {
            self.draw_annotation(plan, gutter, label, bounds, row.y);
        }
        let text_top = row.y + metrics.text_y_pad;
        if self.props.plain {
            // Plain source documents have one side, so a single right-aligned
            // line-number column (the metrics sized the gutter to match).
            let gutter = format_gutter_plain(line.new_line, metrics.gutter_digit_count);
            self.code_line(
                plan,
                &gutter,
                bounds.x + annotation_width + GUTTER_HORIZONTAL_PADDING,
                text_top,
                self.props.palette.text_muted,
                bounds,
            );
        } else {
            let color = self.number_color(line.kind);
            for (column, number) in [line.old_line, line.new_line].into_iter().enumerate() {
                if let Some(number) = number {
                    let right = bounds.x + (column + 1) as f32 * metrics.number_column;
                    self.line_number(plan, number, right, text_top, color, bounds);
                }
            }
        }
        if !self.props.plain {
            self.code_line(
                plan,
                prefix_for_kind(line.kind),
                bounds.x + metrics.gutter_width + TEXT_X_PADDING,
                text_top,
                text_color,
                bounds,
            );
        }
        let wrap_width = self.wrap_width(content_width);
        let geometry = self.geometry(
            memo,
            body_geometry_key(
                row.file_index,
                row.hunk_index,
                row.line_index,
                wrap_width,
                metrics.char_width,
            ),
            &line.content,
            wrap_width,
        );
        let key = CodeKey {
            file_index: row.file_index as u32,
            hunk_index: row.hunk_index as u32,
            line_index: row.line_index as u32,
            wrap_width_bits: wrap_width.to_bits(),
        };
        let lines = self.code_text(plan, line, &geometry, key, code);
        plan.ops.push(Op::Code {
            lines,
            origin: point(
                px(
                    bounds.x + metrics.gutter_width + metrics.prefix_width + TEXT_X_PADDING
                        - horizontal_offset,
                ),
                px(text_top),
            ),
            line_height: px(metrics.row_height),
            clip: clip.bounds(),
        });
    }

    /// One side-by-side row: each populated column gets its own single-number gutter,
    /// prefix, and (independently wrapped) code text, clipped to its column. A shared
    /// line (context) appears in both columns; the cache key is per line, so it shapes
    /// once.
    #[allow(clippy::too_many_arguments)]
    fn draw_split_row(
        &self,
        plan: &mut Plan,
        row: &VisibleRow,
        split: &SplitLayout,
        bounds: Rectangle,
        content_width: f32,
        horizontal_offset: f32,
        memo: &RefCell<GeometryMemo>,
        code: &RefCell<CodeCache>,
    ) {
        let metrics = &self.metrics;
        let wrap_width = self.wrap_width(content_width);
        let sides = [
            (
                SplitSide::Left,
                0.0,
                split.left_text_x,
                bounds.x + split.gutter_width + PREFIX_WIDTH,
                bounds.x + split.divider_x,
            ),
            (
                SplitSide::Right,
                split.right_gutter_x,
                split.right_text_x,
                bounds.x + split.right_gutter_x + split.gutter_width + PREFIX_WIDTH,
                bounds.x + bounds.width,
            ),
        ];
        for (side, gutter_x, text_x, clip_left, clip_right) in sides {
            let Some(line_index) = row.line_in_lane(Some(side)) else {
                continue;
            };
            let Some(line) = self.line(row.file_index, row.hunk_index, line_index) else {
                continue;
            };
            let clip = Rectangle {
                x: clip_left,
                y: bounds.y,
                width: (clip_right - clip_left).max(1.0),
                height: bounds.height,
            };
            let number = match side {
                SplitSide::Left => line.old_line,
                SplitSide::Right => line.new_line,
            };
            let text_color = self.line_text_color(line.kind);
            let text_top = row.y + metrics.text_y_pad;
            if let Some(number) = number {
                let right = bounds.x + gutter_x + split.gutter_width;
                let color = self.number_color(line.kind);
                self.line_number(plan, number, right, text_top, color, bounds);
            }
            self.code_line(
                plan,
                prefix_for_kind(line.kind),
                bounds.x + gutter_x + split.gutter_width + TEXT_X_PADDING,
                text_top,
                text_color,
                bounds,
            );
            let geometry = self.geometry(
                memo,
                body_geometry_key(
                    row.file_index,
                    row.hunk_index,
                    line_index,
                    wrap_width,
                    metrics.char_width,
                ),
                &line.content,
                wrap_width,
            );
            let key = CodeKey {
                file_index: row.file_index as u32,
                hunk_index: row.hunk_index as u32,
                line_index: line_index as u32,
                wrap_width_bits: wrap_width.to_bits(),
            };
            let lines = self.code_text(plan, line, &geometry, key, code);
            plan.ops.push(Op::Code {
                lines,
                origin: point(px(bounds.x + text_x - horizontal_offset), px(text_top)),
                line_height: px(metrics.row_height),
                clip: clip.bounds(),
            });
        }
    }

    /// The selection tint behind a header line's selectable `text`. No-op when nothing
    /// is selected on this line.
    fn header_value_selection(
        &self,
        plan: &mut Plan,
        line_index: usize,
        text: &str,
        origin: Point,
        selection: Option<(TextPosition, TextPosition)>,
    ) {
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
        if end_byte > start_byte {
            let geometry = self.measure_line(text, f32::INFINITY);
            let start_column = geometry.column_at(0, start_byte);
            let cw = self.metrics.char_width;
            plan.rect(
                origin.x + start_column * cw,
                origin.y,
                (geometry.column_at(0, end_byte) - start_column) * cw,
                self.metrics.row_height,
                self.props.palette.selection,
            );
        }
    }

    /// A rich field's value: selection tint, the value in its colour runs (the link
    /// underlined under the cursor), trailing badges and icon buttons.
    #[allow(clippy::too_many_arguments)]
    fn draw_rich_field(
        &self,
        plan: &mut Plan,
        line_index: usize,
        field: &HeaderField,
        bounds: Rectangle,
        clip: Rectangle,
        y: f32,
        visible_top: f32,
        selection: Option<(TextPosition, TextPosition)>,
        hovered: Option<usize>,
    ) {
        let palette = &self.props.palette;
        let metrics = &self.metrics;
        let value_x = self.header_value_x(bounds);
        self.header_value_selection(
            plan,
            line_index,
            &field.value,
            Point::new(value_x, y),
            selection,
        );
        // One shaped line with colour runs, so glyphs wider than a cell can't collide
        // across a run boundary.
        let byte_at = |char_index: usize| {
            field
                .value
                .char_indices()
                .nth(char_index)
                .map_or(field.value.len(), |(byte, _)| byte)
        };
        let mut runs: Vec<(usize, Color)> = Vec::with_capacity(field.runs.len() + 1);
        if field.runs.first().is_none_or(|(start, _)| *start > 0) {
            runs.push((0, palette.text));
        }
        runs.extend(field.runs.iter().copied());
        let text_runs: Vec<TextRun> = runs
            .iter()
            .enumerate()
            .filter_map(|(index, &(start, color))| {
                let start = byte_at(start);
                let end = runs
                    .get(index + 1)
                    .map_or(field.value.len(), |(next, _)| byte_at(*next));
                (end > start).then(|| text_run(end - start, self.props.font, color))
            })
            .collect();
        let shaped = plan.text.shape_line(
            SharedString::new(&field.value),
            px(self.props.typography.size),
            &text_runs,
            None,
        );
        let mut x = value_x + f32::from(shaped.width);
        plan.ops.push(Op::Line {
            line: shaped,
            origin: point(px(value_x), px(y + metrics.text_y_pad)),
            line_height: px(metrics.row_height),
            clip: clip.bounds(),
        });
        // Chips sit on the value's baseline, not the row's middle.
        let baseline = self.code_baseline(y + metrics.text_y_pad);
        for chip in &field.chips {
            x += HEADER_CHIP_GAP;
            plan.chip(chip, x, chip::center_for_baseline(chip, baseline), clip);
            x += chip::full_width(chip);
        }
        for (slot, rect, _) in self.rich_targets(line_index, field, bounds, visible_top) {
            let hot = hovered == Some(slot);
            if slot == 0 {
                if hot {
                    plan.rect(
                        rect.x,
                        rect.y + metrics.row_height - 3.0,
                        rect.width,
                        1.0,
                        palette.text_muted,
                    );
                }
                continue;
            }
            if hot {
                plan.rounded(
                    rect,
                    theme::radius::CONTROL,
                    Color {
                        a: 0.14,
                        ..palette.text_muted
                    },
                );
            }
            plan.icon(
                field.buttons[slot - 1].0,
                HEADER_BUTTON_ICON_SIZE,
                rect,
                if hot {
                    palette.text
                } else {
                    palette.text_muted
                },
                clip,
            );
        }
    }

    fn draw_revision_header(
        &self,
        plan: &mut Plan,
        bounds: Rectangle,
        visible_top: f32,
        selection: Option<(TextPosition, TextPosition)>,
        show_description_edit: bool,
        hovered_action: Option<(usize, usize)>,
    ) {
        // Header occupies content_y in `[0, header_height)`. Translate to screen coords
        // for the section that intersects the viewport.
        let palette = &self.props.palette;
        let metrics = &self.metrics;
        let header_height = self.header_height();
        let header_screen_y = bounds.y - visible_top;

        // Tint the strip so the header reads as distinct chrome.
        plan.rect(
            bounds.x,
            header_screen_y,
            bounds.width,
            header_height,
            palette.file_header,
        );
        // Border across the bottom edge of the header strip, matching file headers.
        plan.rect(
            bounds.x,
            header_screen_y + header_height - 1.0,
            bounds.width,
            1.0,
            palette.border,
        );

        // Unwrapped header values stop at the header's right inset instead of running
        // on under the overlay scrollbar to the pane edge.
        let clip = Rectangle {
            width: (bounds.width - HEADER_HORIZONTAL_PADDING).max(0.0),
            ..bounds
        };
        let label_width = self.header_label_width();

        let mut y = header_screen_y + HEADER_VERTICAL_PADDING;
        let left_x = bounds.x + HEADER_HORIZONTAL_PADDING;
        let value_x = left_x + label_width + HEADER_LABEL_GAP;
        let first_description = self
            .props
            .header
            .iter()
            .position(|line| matches!(line, HeaderLine::Description(_)));

        for (line_index, line) in self.props.header.iter().enumerate() {
            let text_top = y + metrics.text_y_pad;
            match line {
                HeaderLine::Field { label, value } => {
                    self.code_line(plan, label, left_x, text_top, palette.text_muted, clip);
                    self.header_value_selection(
                        plan,
                        line_index,
                        value,
                        Point::new(value_x, y),
                        selection,
                    );
                    self.code_line(plan, value, value_x, text_top, palette.text, clip);
                }
                HeaderLine::Bookmarks { label, chips } => {
                    self.code_line(plan, label, left_x, text_top, palette.text_muted, clip);
                    let gap = 6.0;
                    let right_edge = bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING;
                    // The chips' labels sit on the field label's baseline.
                    let baseline = self.code_baseline(text_top);
                    let mut chip_x = value_x;
                    for chip in chips {
                        let chip_w = chip::full_width(chip);
                        // Drop chips that would overflow the panel rather than clip them
                        // mid-glyph.
                        if chip_x + chip_w > right_edge {
                            break;
                        }
                        plan.chip(
                            chip,
                            chip_x,
                            chip::center_for_baseline(chip, baseline),
                            clip,
                        );
                        chip_x += chip_w + gap;
                    }
                }
                HeaderLine::Description(line) => {
                    // Draw the raw text at an indented origin (rather than a
                    // four-space-prefixed string) so the selectable text and the
                    // hit-test/selection origin line up.
                    let desc_x = self.header_text_origin_x(line_index, bounds);
                    self.header_value_selection(
                        plan,
                        line_index,
                        line,
                        Point::new(desc_x, y),
                        selection,
                    );
                    let text_clip = Rectangle {
                        width: (bounds.x + bounds.width
                            - HEADER_HORIZONTAL_PADDING
                            - bounds.x
                            - if self.props.on_edit_description.is_some() {
                                HEADER_EDIT_WIDTH + HEADER_LABEL_GAP
                            } else {
                                0.0
                            })
                        .max(1.0),
                        ..bounds
                    };
                    self.code_line(plan, line, desc_x, text_top, palette.text, text_clip);
                    if show_description_edit
                        && Some(line_index) == first_description
                        && let Some(button) = self.description_edit_bounds(bounds, visible_top)
                    {
                        plan.rounded(
                            button,
                            theme::radius::BUTTON,
                            Color {
                                a: 0.12,
                                ..palette.text_muted
                            },
                        );
                        self.description_edit_label(plan, button, clip);
                    }
                }
                HeaderLine::Rich(field) => {
                    self.code_line(
                        plan,
                        &field.label,
                        left_x,
                        text_top,
                        palette.text_muted,
                        clip,
                    );
                    self.draw_rich_field(
                        plan,
                        line_index,
                        field,
                        bounds,
                        clip,
                        y,
                        visible_top,
                        selection,
                        hovered_action
                            .filter(|(line, _)| *line == line_index)
                            .map(|(_, slot)| slot),
                    );
                }
                HeaderLine::Title { text, placeholder } => {
                    let font = theme::emphasis_font(self.props.ui_font, theme::Weight::SEMIBOLD);
                    let height = super::HEADER_TITLE_ROW_HEIGHT;
                    let line_box = TextBox::new(font, super::HEADER_TITLE_SIZE, height);
                    let top = line_box.baseline_for_center(y + height / 2.0) - line_box.baseline();
                    plan.line(
                        text,
                        font,
                        super::HEADER_TITLE_SIZE,
                        left_x,
                        top,
                        height,
                        if *placeholder {
                            palette.text_muted
                        } else {
                            palette.text
                        },
                        clip,
                    );
                }
                HeaderLine::Blank | HeaderLine::Spacer(_) => {}
            }
            y += self.header_line_height(line);
        }
    }

    fn description_edit_label(&self, plan: &mut Plan, button: Rectangle, clip: Rectangle) {
        let palette = &self.props.palette;
        let label = "Edit";
        let font = self.props.font;
        let label_width = measure::line_width(label, HEADER_EDIT_LABEL_SIZE, font);
        let content_width = HEADER_EDIT_ICON_SIZE + HEADER_EDIT_CONTENT_GAP + label_width;
        let content_x = button.center_x() - content_width / 2.0;
        plan.icon(
            icons::PENCIL,
            HEADER_EDIT_ICON_SIZE,
            Rectangle {
                x: content_x,
                width: HEADER_EDIT_ICON_SIZE,
                ..button
            },
            palette.text_muted,
            clip,
        );
        let line = TextBox::new(font, HEADER_EDIT_LABEL_SIZE, button.height);
        plan.line(
            label,
            font,
            HEADER_EDIT_LABEL_SIZE,
            content_x + HEADER_EDIT_ICON_SIZE + HEADER_EDIT_CONTENT_GAP,
            button.y + line.trim_shift(),
            button.height,
            palette.text,
            clip,
        );
    }

    /// One blame entry on the row at screen-y `y`: the id as the graph draws it, the
    /// author after it (cut short with an ellipsis), and the age right-aligned at the
    /// column's end, all on the code's baseline.
    fn draw_annotation(
        &self,
        plan: &mut Plan,
        gutter: &AnnotationGutter,
        label: &AnnotationLabel,
        bounds: Rectangle,
        y: f32,
    ) {
        let style = &gutter.style;
        let width = self.annotation_width();
        let clip = Rectangle { width, ..bounds };
        let row = self.metrics.row_height;
        let size = self.props.typography.size;
        let baseline = self.code_baseline(y + self.metrics.text_y_pad);
        let paint = |plan: &mut Plan, content: &str, font: Font, x: f32, color: Color| {
            let top = baseline - TextBox::new(font, size, row).baseline();
            plan.line(content, font, size, x, top, row, color, clip);
        };
        let colors = |color: Color| if label.hidden { style.hidden } else { color };
        let x = bounds.x + GUTTER_HORIZONTAL_PADDING;
        let unique = label
            .id
            .char_indices()
            .nth(label.unique)
            .map_or(label.id.len(), |(at, _)| at);
        let (prefix, rest) = label.id.split_at(unique);
        let cell = self.metrics.char_width;
        let prefix_width = prefix.chars().count() as f32 * cell;
        paint(plan, prefix, self.props.font, x, colors(style.id));
        paint(
            plan,
            rest,
            self.props.font,
            x + prefix_width,
            colors(style.id_rest),
        );
        let end = bounds.x + width - GUTTER_HORIZONTAL_PADDING;
        let age_width = measure::line_width(&label.age, size, style.font);
        paint(
            plan,
            &label.age,
            style.font,
            end - age_width,
            colors(style.age),
        );
        let author_x = x + label.id.chars().count() as f32 * cell + cell;
        let author_end = end - age_width - cell;
        let author = truncate_end(&label.author, size, style.font, author_end - author_x);
        paint(plan, &author, style.font, author_x, colors(style.author));
    }

    /// The hovered blame run (`start..end` lines): a wash behind its rows' entries, and
    /// its id underlined.
    fn draw_annotation_hover(
        &self,
        plan: &mut Plan,
        gutter: &AnnotationGutter,
        (start, end): (usize, usize),
        rows: &[VisibleRow],
        bounds: Rectangle,
    ) {
        let number = |row: &VisibleRow| {
            self.line(row.file_index, row.hunk_index, row.line_index)
                .and_then(|line| line.new_line)
        };
        let mut span = rows
            .iter()
            .filter(|row| number(row).is_some_and(|n| (start + 1..=end).contains(&n)));
        let Some(first) = span.next() else {
            return;
        };
        let bottom = span
            .next_back()
            .map_or(first.y + first.height, |last| last.y + last.height);
        let top = first.y;
        plan.rounded(
            Rectangle {
                x: bounds.x + 2.0,
                y: top + 1.0,
                width: self.annotation_width() - 4.0,
                height: (bottom - top - 2.0).max(1.0),
            },
            theme::radius::CONTROL,
            gutter.style.hover,
        );
        // The id is on the run's first line, when that one shows.
        let starts_here = number(first) == Some(start + 1);
        if let Some(label) = self.labels.get(start).filter(|_| starts_here) {
            let baseline = self.code_baseline(first.y + self.metrics.text_y_pad);
            plan.rect(
                bounds.x + GUTTER_HORIZONTAL_PADDING,
                (baseline + 2.0).round(),
                label.id.chars().count() as f32 * self.metrics.char_width,
                1.0,
                gutter.style.id,
            );
        }
    }

    /// `row`'s lead word before its first button, and its focus bar (when focused) over
    /// the strip's left edge.
    fn strip_row(
        &self,
        plan: &mut Plan,
        row: &hunk_actions::Row,
        placed: &[Placed],
        style: &hunk_actions::Style,
        strip: Rectangle,
        clip: Rectangle,
    ) {
        if let (Some(lead), Some(first)) = (&row.lead, placed.first()) {
            let width = hunk_actions::run_width(lead, style.size);
            let baseline = hunk_actions::baseline(row, style, first.rect);
            draw_run(
                plan,
                lead,
                Point::new(first.rect.x - hunk_actions::GAP - width, baseline),
                first.rect,
                style.size,
                clip,
            );
        }
        if row.focused {
            plan.rect(
                strip.x,
                strip.y,
                hunk_actions::FOCUS_BAR,
                strip.height,
                style.focus,
            );
        }
    }

    /// A file strip at screen-y `y`: a hunk strip's band washed in the conflict colour,
    /// its text where a hunk header's is, its tools after that and its buttons
    /// right-aligned.
    fn draw_file_strip(
        &self,
        plan: &mut Plan,
        index: &HeightIndex,
        strip: usize,
        y: f32,
        bounds: Rectangle,
        hovered: Option<ActionHit>,
    ) {
        let Some(actions) = &self.props.hunk_actions else {
            return;
        };
        let palette = &self.props.palette;
        let spec = &actions.files[strip];
        let height = self.metrics.hunk_header_height;
        plan.rect(bounds.x, y, bounds.width, height, palette.hunk_header);
        plan.rect(
            bounds.x,
            y,
            bounds.width,
            height,
            Color {
                a: 0.08,
                ..palette.conflict_marker
            },
        );
        let Some(placed): Option<FilePlaced> = self.file_strip_layout(index, strip, y, bounds)
        else {
            return;
        };
        if placed.label {
            let text_x = self.strip_text_x(index, spec.file, bounds);
            self.code_line(
                plan,
                &spec.label,
                text_x,
                centered_text_y(y, height, self.metrics.row_height) + self.metrics.text_y_pad,
                palette.text_muted,
                Rectangle {
                    x: text_x,
                    width: (bounds.x + bounds.width - text_x).max(1.0),
                    ..bounds
                },
            );
        }
        let rect = Rectangle {
            x: bounds.x,
            y,
            width: bounds.width,
            height,
        };
        self.strip_row(
            plan,
            &spec.row,
            &placed.buttons,
            &actions.style,
            rect,
            bounds,
        );
        let baseline = placed
            .buttons
            .first()
            .or(placed.tools.first())
            .map(|first| hunk_actions::baseline(&spec.row, &actions.style, first.rect));
        for (button, (tool, at)) in spec.tools.iter().zip(&placed.tools).enumerate() {
            if hovered == Some(ActionHit::File(strip, button)) && tool.enabled {
                plan.rounded(
                    at.rect,
                    actions.style.radius,
                    actions.style.ghost_hover_fill,
                );
            }
            draw_runs(
                plan,
                &tool.runs,
                at,
                &actions.style,
                baseline.unwrap_or(at.rect.center_y()),
                bounds,
            );
        }
        for (button, (spec_button, at)) in spec.row.buttons.iter().zip(&placed.buttons).enumerate()
        {
            draw_button(
                plan,
                &spec.row,
                spec_button,
                at,
                &actions.style,
                hovered == Some(ActionHit::File(strip, spec.tools.len() + button)),
                bounds,
            );
        }
    }

    /// One file's header strip at screen-y `y`: `[fold] [pick] [badge] dir/name …
    /// [N hunks] [+a −d] [source] [more]` (see [`View::file_header_layout`]). Shared
    /// by the normal scrolling draw and the pinned sticky draw. `hovered` is set when
    /// the cursor is on this header, with the control it is on.
    fn draw_file_header(
        &self,
        plan: &mut Plan,
        file_index: usize,
        y: f32,
        bounds: Rectangle,
        hovered: Option<Option<FileControl>>,
    ) {
        let Some(file) = self.file(file_index) else {
            return;
        };
        let Some(layout) = self.file_header_layout(file_index, y, bounds) else {
            return;
        };
        let palette = &self.props.palette;
        let metrics = &self.metrics;
        let folded = self.is_folded(file_index);

        plan.rect(
            bounds.x,
            y,
            bounds.width,
            metrics.file_header_height,
            palette.file_header,
        );
        plan.rect(
            bounds.x,
            y + metrics.file_header_height - 1.0,
            bounds.width,
            1.0,
            palette.border,
        );

        let control_hovered = |control: FileControl| hovered == Some(Some(control));
        let icon_button = |plan: &mut Plan, rect: Rectangle, glyph: &str, hover: bool| {
            if hover {
                plan.rounded(
                    rect,
                    theme::radius::XS,
                    Color {
                        a: 0.14,
                        ..palette.text_muted
                    },
                );
            }
            plan.icon(
                glyph,
                BROWSE_ICON_SIZE,
                rect,
                if hover {
                    palette.text
                } else {
                    palette.text_muted
                },
                bounds,
            );
        };
        icon_button(
            plan,
            layout.fold,
            if folded {
                icons::CHEVRON_RIGHT
            } else {
                icons::CHEVRON_DOWN
            },
            control_hovered(FileControl::Fold),
        );
        if let Some(rect) = layout.check {
            let state = self
                .props
                .marks
                .as_ref()
                .and_then(|marks| marks.files.get(file_index))
                .map(|mark| mark.state)
                .unwrap_or_default();
            let picking = self.props.marks.as_ref().is_some_and(|marks| marks.picking);
            // Out of the way until it's wanted: on hover, or while anything is picked.
            if hovered.is_some() || picking || state != CheckState::Off {
                self.draw_check(plan, rect, state, bounds);
            }
        }
        plan.status_badge(
            &status_badge::StatusBadge {
                letter: file.status.short_label(),
                ink: file.status_color,
            },
            self.props.font,
            layout.badge,
            bounds,
        );

        self.draw_file_path(plan, file, &layout, bounds);

        let meta_size = self.meta_size();
        let meta_line = TextBox::new(self.props.font, meta_size, metrics.row_height);
        let meta_top = layout.baseline - meta_line.baseline();
        if let Some((text, x)) = &layout.hunks {
            plan.line(
                text,
                self.props.font,
                meta_size,
                *x,
                meta_top,
                metrics.row_height,
                palette.text_muted,
                bounds,
            );
        }
        for (text, x, kind) in &layout.stats {
            let color = match kind {
                StatKind::Added => palette.addition_text,
                StatKind::Deleted => palette.deletion_text,
                StatKind::Tag => palette.text_muted,
            };
            plan.line(
                text,
                self.props.font,
                meta_size,
                *x,
                meta_top,
                metrics.row_height,
                color,
                bounds,
            );
        }
        if let Some(rect) = layout.browse {
            icon_button(
                plan,
                rect,
                icons::CODE,
                control_hovered(FileControl::Browse),
            );
        }
        if let Some(rect) = layout.more {
            icon_button(
                plan,
                rect,
                icons::ELLIPSIS,
                control_hovered(FileControl::More),
            );
        }
    }

    /// A file header's path: the directory in the muted colour, the name in the text
    /// colour at semibold, ellipsised from the front of the directory so the name
    /// stays. A rename shows `old -> new` in one muted run with the new name bold.
    fn draw_file_path(
        &self,
        plan: &mut Plan,
        file: &super::DiffFileView,
        layout: &FileHeaderLayout,
        bounds: Rectangle,
    ) {
        let palette = &self.props.palette;
        let size = self.props.typography.size;
        let regular = self.props.font;
        let bold = theme::emphasis_font(regular, theme::Weight::SEMIBOLD);
        let available = (layout.title_right - layout.title_x).max(1.0);
        let clip = Rectangle {
            x: layout.title_x,
            width: available,
            ..bounds
        };
        let split = file.title.rfind('/').map_or(0, |at| at + 1);
        let (dir, name) = file.title.split_at(split);
        let name_width = measure::line_width(name, size, bold);
        let gone = file.status == super::DiffFileStatus::Deleted;
        let name_color = if gone {
            palette.text_muted
        } else {
            palette.text
        };
        let (dir, name) = if name_width >= available {
            (String::new(), truncate_end(name, size, bold, available))
        } else {
            (
                truncate_start(dir, size, regular, available - name_width),
                name.to_owned(),
            )
        };
        let dir_width = plan.line(
            &dir,
            regular,
            size,
            layout.title_x,
            layout.line_top,
            self.metrics.row_height,
            palette.text_muted,
            clip,
        );
        let name_x = layout.title_x + dir_width;
        let drawn = plan.line(
            &name,
            bold,
            size,
            name_x,
            layout.line_top,
            self.metrics.row_height,
            name_color,
            clip,
        );
        if gone {
            let x_height = 0.52 * size;
            plan.rect(
                name_x,
                (layout.baseline - x_height / 2.0).round(),
                drawn.min(available - dir_width),
                1.0,
                palette.text_muted,
            );
        }
    }

    /// A hunk header's text: `@@` centred in the gutter in the accent, the ranges
    /// after it, then the enclosing function. A header that isn't a range (a
    /// placeholder, a conflict region's) is drawn whole, as it comes.
    #[allow(clippy::too_many_arguments)]
    fn draw_hunk_title(
        &self,
        plan: &mut Plan,
        header: &str,
        y: f32,
        gutter: f32,
        text_x: f32,
        emphasized: bool,
        clip: Rectangle,
        bounds: Rectangle,
    ) {
        let palette = &self.props.palette;
        let metrics = &self.metrics;
        let font = self.props.font;
        let size = self.meta_size();
        let height = metrics.hunk_header_height;
        let top = y + TextBox::new(font, size, height).trim_shift();
        let Some((ranges, context)) = split_hunk_header(header) else {
            self.code_line(
                plan,
                header,
                text_x,
                centered_text_y(y, height, metrics.row_height) + metrics.text_y_pad,
                if emphasized {
                    palette.text
                } else {
                    palette.text_muted
                },
                clip,
            );
            return;
        };
        let marker_width = measure::line_width("@@", size, font);
        plan.line(
            "@@",
            font,
            size,
            bounds.x + (gutter - marker_width) / 2.0,
            top,
            height,
            palette.accent,
            bounds,
        );
        let ranges_width = plan.line(
            ranges,
            font,
            size,
            text_x,
            top,
            height,
            palette.text_muted,
            clip,
        );
        if !context.is_empty() {
            plan.line(
                context,
                font,
                size,
                text_x + ranges_width + 2.0 * metrics.char_width,
                top,
                height,
                palette.text,
                clip,
            );
        }
    }

    /// A file note at screen-y `top`: its icon, its runs, and its button.
    fn draw_note(
        &self,
        plan: &mut Plan,
        file: usize,
        top: f32,
        bounds: Rectangle,
        button_hovered: bool,
    ) {
        let Some(note) = self.file(file).and_then(|file| file.note.as_ref()) else {
            return;
        };
        let palette = &self.props.palette;
        let height = self.metrics.note_height;
        let ui = self.props.ui_font;
        let mono = self.props.font;
        // Over the gutter's band and edge: a note has no line numbers.
        plan.rect(bounds.x, top, bounds.width, height, palette.panel);
        let center_y = top + height / 2.0;
        let line = TextBox::new(ui, layout::NOTE_TEXT_SIZE, height);
        let baseline = line.baseline_for_center(center_y);
        let icon_size = 14.0;
        let mut x = bounds.x + layout::NOTE_INSET;
        plan.icon(
            note.icon,
            icon_size,
            Rectangle {
                x,
                y: center_y - icon_size / 2.0,
                width: icon_size,
                height: icon_size,
            },
            palette.text_muted,
            bounds,
        );
        x += icon_size + 10.0;
        let button = self.note_button_rect(file, top, bounds);
        let clip = Rectangle {
            x,
            width: (button.map_or(bounds.x + bounds.width, |rect| rect.x - 8.0) - x).max(1.0),
            ..bounds
        };
        for (text, code) in &note.runs {
            let (font, size, color) = if *code {
                (mono, self.meta_size(), palette.text)
            } else {
                (ui, layout::NOTE_TEXT_SIZE, palette.text_muted)
            };
            let line_top = baseline - TextBox::new(font, size, height).baseline();
            x += plan.line(text, font, size, x, line_top, height, color, clip);
        }
        if let (Some(rect), Some(label)) = (button, note.action.as_deref()) {
            plan.boxed(
                rect,
                theme::radius::BUTTON,
                if button_hovered {
                    palette.hunk_header
                } else {
                    palette.file_header
                },
                palette.border,
                BorderStyle::Solid,
            );
            let label_line = TextBox::new(ui, layout::NOTE_TEXT_SIZE, rect.height);
            plan.line(
                label,
                ui,
                layout::NOTE_TEXT_SIZE,
                rect.x + layout::NOTE_BUTTON_PAD_X,
                label_line.baseline_for_center(rect.center_y()) - label_line.baseline(),
                rect.height,
                palette.text,
                bounds,
            );
        }
    }

    /// Picks on changed lines: an accent bar at the gutter edge of a picked line, a
    /// faded tint on an unpicked one, a faint bar under the mouse, and the keyboard
    /// cursor's outline.
    fn draw_line_marks(
        &self,
        plan: &mut Plan,
        rows: &[VisibleRow],
        split: Option<&SplitLayout>,
        bounds: Rectangle,
        hovered: Option<LineAt>,
    ) {
        let Some(marks) = &self.props.marks else {
            return;
        };
        if self.props.plain {
            return;
        }
        let palette = &self.props.palette;
        let cursor = marks.lines.as_ref().and_then(|mode| mode.cursor);
        let accent = marks.control;
        for row in rows {
            // (line, lane x, lane width) per column.
            let mut members = [None, None];
            match (split, row.pair) {
                (Some(split), Some((left, right))) => {
                    if left != NO_LINE {
                        members[0] = Some((left as usize, 0.0, split.divider_x));
                    }
                    if right != NO_LINE {
                        members[1] = Some((
                            right as usize,
                            split.right_gutter_x,
                            bounds.width - split.right_gutter_x,
                        ));
                    }
                }
                _ => {
                    members[0] = Some((row.line_index, 0.0, bounds.width));
                }
            }
            for (line, x, width) in members.into_iter().flatten() {
                let at = LineAt {
                    file: row.file_index,
                    hunk: row.hunk_index,
                    line,
                };
                let Some(picked) = self.line_mark(at) else {
                    continue;
                };
                let x = bounds.x + x;
                if picked {
                    plan.rect(x, row.y, PICK_BAR_WIDTH, row.height, accent);
                } else {
                    // Left out of the pick: the change tint fades back toward the panel,
                    // so what is in reads at a glance.
                    plan.rect(
                        x,
                        row.y,
                        width,
                        row.height,
                        Color {
                            a: 0.5,
                            ..palette.panel
                        },
                    );
                    // Hover previews the bar a click would add.
                    if hovered == Some(at) {
                        plan.rect(
                            x,
                            row.y,
                            PICK_BAR_WIDTH,
                            row.height,
                            Color { a: 0.4, ..accent },
                        );
                    }
                }
                if cursor == Some(at) {
                    plan.boxed(
                        Rectangle {
                            x: x + 0.5,
                            y: row.y + 0.5,
                            width: (width - 1.0).max(0.0),
                            height: (row.height - 1.0).max(0.0),
                        },
                        2.0,
                        theme::TRANSPARENT,
                        Color {
                            a: 0.45,
                            ..palette.text
                        },
                        BorderStyle::Solid,
                    );
                }
            }
        }
    }

    fn draw_check(&self, plan: &mut Plan, rect: Rectangle, state: CheckState, clip: Rectangle) {
        let Some(marks) = &self.props.marks else {
            return;
        };
        plan.checkbox(
            rect,
            state,
            marks.control,
            self.props.palette.text_muted,
            self.props.palette.panel,
            clip,
        );
    }

    /// One frame's worth of drawing, in painter's order. Everything here is
    /// O(log n + visible) against the prefix-sum index, never a walk over the document.
    pub(super) fn build_plan(&self, state: &State, bounds: Rectangle, plan: &mut Plan) {
        let memo = &state.geometry;
        let code = &state.code;
        code.borrow_mut().begin_frame();
        let props = self.props;
        let palette = &props.palette;
        let metrics = &self.metrics;
        // Split-column wrap width in split mode, the whole text area otherwise;
        // full-width rows always use `unified_width`.
        let content_width = self.effective_text_width(bounds.width);
        let unified_width = self.content_width(bounds.width);
        let split = props.side_by_side.then(|| self.split_layout(bounds.width));
        let height_index = state.height_index.borrow();
        let horizontal_offset = state.horizontal_offset;
        // Everything text-shaped clips out of the (left) gutter band; the split layout's
        // narrower gutter widens this accordingly.
        let text_region_x = match &split {
            Some(split) => split.gutter_width + PREFIX_WIDTH,
            None => metrics.gutter_width + metrics.prefix_width,
        };
        let content_clip = Rectangle {
            x: bounds.x + text_region_x,
            width: (bounds.width - text_region_x).max(1.0),
            ..bounds
        };
        // Clip for unified-rendered rows: in split mode their (wider) double gutter sits
        // to the right of the split columns' clip edge, so they need their own rect or
        // h-scrolled text would paint over it.
        let unified_region_x = metrics.gutter_width + metrics.prefix_width;
        let unified_clip = Rectangle {
            x: bounds.x + unified_region_x,
            width: (bounds.width - unified_region_x).max(1.0),
            ..bounds
        };

        plan.rect(
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            palette.panel,
        );

        let visible_top = state.painted_offset;
        let visible_bottom = visible_top + bounds.height;
        let header_height = self.header_height();
        let file_tops =
            &height_index.file_tops[..props.files.len().min(height_index.file_tops.len())];

        let mut visible_rows: Vec<VisibleRow> =
            Vec::with_capacity((bounds.height / metrics.row_height).ceil() as usize + 8);
        // The candidate first row may still end above the viewport when the top lands in
        // a header band: the in-loop check skips it.
        let first_row = height_index
            .row_tops
            .partition_point(|&top| top <= visible_top)
            .saturating_sub(1);
        // Runs of one change kind, merged into one band so a hunk's tint is one quad
        // rather than one per row.
        let mut bands: Vec<(DiffLineKind, f32, f32)> = Vec::new();
        for (i, &row_top) in height_index.row_tops.iter().enumerate().skip(first_row) {
            if row_top > visible_bottom {
                break;
            }
            let (file_index, hunk_index, line_index) = height_index.row_ids[i];
            let (file_index, hunk_index, line_index) = (
                file_index as usize,
                hunk_index as usize,
                line_index as usize,
            );
            let height = self.index_row_height(&height_index, i);
            if row_top + height < visible_top {
                continue;
            }
            let y = bounds.y + (row_top - visible_top);
            // Full-width files drop their alignment-only pair so the row renders (and
            // band-tints) like a unified one.
            let pair = height_index
                .pair_lines
                .get(i)
                .copied()
                .filter(|_| !height_index.is_full_width(file_index));
            visible_rows.push(VisibleRow {
                file_index,
                hunk_index,
                line_index,
                pair,
                y,
                height,
            });
            if pair.is_none()
                && let Some(line) = self.line(file_index, hunk_index, line_index)
                && line.kind != DiffLineKind::Context
            {
                match bands.last_mut() {
                    Some((kind, band_y, band_height))
                        if *kind == line.kind && (*band_y + *band_height - y).abs() < 0.5 =>
                    {
                        *band_height += height;
                    }
                    _ => bands.push((line.kind, y, height)),
                }
            }
        }

        // Sticky file header: the file occupying the top of the viewport keeps its name
        // pinned while its hunks scroll under it, until the next file's header slides up
        // to push it off. Only kicks in once that file's header has scrolled above the
        // viewport top, which is always below the revision header, so the two never
        // overlap.
        let content_end = height_index.total_height;
        let sticky_file = (!props.plain)
            .then(|| {
                file_tops
                    .partition_point(|&top| top <= visible_top)
                    .checked_sub(1)
                    .filter(|&i| file_tops[i] < visible_top)
            })
            .flatten();
        let sticky_pin_y = sticky_file.map(|i| {
            let next_top = file_tops.get(i + 1).copied().unwrap_or(content_end);
            let pinned_content_y = visible_top.min(next_top - metrics.file_header_height);
            bounds.y + (pinned_content_y - visible_top)
        });

        match &split {
            Some(split) => {
                // One gutter strip per column, plus the center divider, segmented per
                // file, because full-width (single-sided) files keep the unified double
                // gutter instead. The last segment runs to the pane bottom so short
                // documents keep their chrome below the content.
                let split_chrome = |plan: &mut Plan, y: f32, height: f32| {
                    for gutter_x in [0.0, split.right_gutter_x] {
                        plan.rect(
                            bounds.x + gutter_x,
                            y,
                            split.gutter_width,
                            height,
                            palette.gutter_background,
                        );
                        plan.rect(
                            bounds.x + gutter_x + split.gutter_width,
                            y,
                            1.0,
                            height,
                            palette.border,
                        );
                    }
                    plan.rect(bounds.x + split.divider_x, y, 1.0, height, palette.border);
                };
                if props.files.is_empty() {
                    split_chrome(plan, bounds.y, bounds.height);
                }
                let first = file_tops
                    .partition_point(|&top| top <= visible_top)
                    .saturating_sub(1);
                for (file_index, &top) in file_tops.iter().enumerate().skip(first) {
                    if top > visible_bottom {
                        break;
                    }
                    let bottom = if file_index + 1 < props.files.len() {
                        height_index.file_tops[file_index + 1]
                    } else {
                        f32::MAX
                    };
                    let seg_top = top.max(visible_top);
                    let y = bounds.y + (seg_top - visible_top);
                    let height = bottom.min(visible_bottom) - seg_top;
                    if height <= 0.0 {
                        continue;
                    }
                    if height_index.is_full_width(file_index) {
                        plan.rect(
                            bounds.x,
                            y,
                            metrics.gutter_width,
                            height,
                            palette.gutter_background,
                        );
                        plan.rect(
                            bounds.x + metrics.gutter_width,
                            y,
                            1.0,
                            height,
                            palette.border,
                        );
                    } else {
                        split_chrome(plan, y, height);
                    }
                }
            }
            None => {
                plan.rect(
                    bounds.x,
                    bounds.y,
                    metrics.gutter_width,
                    bounds.height,
                    palette.gutter_background,
                );
                plan.rect(
                    bounds.x + metrics.gutter_width,
                    bounds.y,
                    1.0,
                    bounds.height,
                    palette.border,
                );
            }
        }

        for &(kind, y, height) in &bands {
            if let Some(background) = self.changed_line_background_color(kind) {
                plan.rect(bounds.x, y, bounds.width, height, background);
                // The number cells a step stronger, so a change reads down the
                // gutter too.
                if !props.plain {
                    plan.rect(bounds.x, y, metrics.gutter_width, height, background);
                }
            }
        }
        if let Some(split) = &split {
            self.split_row_tints(plan, &visible_rows, split, bounds);
        }
        if let (Some(gutter), Some(run)) = (&props.annotations, state.hovered_annotation) {
            self.draw_annotation_hover(plan, gutter, run, &visible_rows, bounds);
        }
        if !props.plain {
            let first = height_index
                .media_bands
                .partition_point(|&(_, top, height)| top + height < visible_top);
            for &(file_index, top, _) in &height_index.media_bands[first..] {
                if top > visible_bottom {
                    break;
                }
                if let Some(preview) = props.media_of(file_index) {
                    media::draw(self, plan, preview, bounds.y + (top - visible_top), bounds);
                }
            }
            let first = height_index
                .note_bands
                .partition_point(|&(_, top)| top + metrics.note_height < visible_top);
            for &(file_index, top) in &height_index.note_bands[first..] {
                if top > visible_bottom {
                    break;
                }
                self.draw_note(
                    plan,
                    file_index,
                    bounds.y + (top - visible_top),
                    bounds,
                    state.hovered_note_button == Some(file_index),
                );
            }
        }

        // One lane list for the frame: the emphasis, selection and find passes all walk
        // the same columns.
        let lanes = self.lane_geometries(bounds, split.as_ref(), horizontal_offset);
        self.emphasis_highlights(plan, memo, &visible_rows, &lanes);
        // After the emphasis, so a line left out of the pick fades whole.
        self.draw_line_marks(
            plan,
            &visible_rows,
            split.as_ref(),
            bounds,
            state.hovered_line,
        );

        let selection_range = match (state.selection_anchor, state.selection_focus) {
            (Some(anchor), Some(focus)) if anchor != focus => Some(ordered(anchor, focus)),
            _ => None,
        };

        if header_height > 0.0 && visible_top < header_height {
            self.draw_revision_header(
                plan,
                bounds,
                visible_top,
                selection_range,
                state.hovered_description,
                state.hovered_header_action,
            );
        }

        if !props.plain {
            let first_file =
                file_tops.partition_point(|&top| top + metrics.file_header_height < visible_top);
            for (file_index, &top) in file_tops.iter().enumerate().skip(first_file) {
                if top > visible_bottom {
                    break;
                }
                // The sticky file's header is drawn pinned, on top, after the rows.
                if Some(file_index) == sticky_file {
                    continue;
                }
                self.draw_file_header(
                    plan,
                    file_index,
                    bounds.y + (top - visible_top),
                    bounds,
                    state.header_hover(file_index),
                );
            }

            let first_hunk = height_index
                .hunk_tops
                .partition_point(|&top| top + metrics.hunk_header_height < visible_top);
            for (i, &top) in height_index.hunk_tops.iter().enumerate().skip(first_hunk) {
                if top > visible_bottom {
                    break;
                }
                let (file_index, hunk_index) = height_index.hunk_ids[i];
                let (file_index, hunk_index) = (file_index as usize, hunk_index as usize);
                let y = bounds.y + (top - visible_top);
                let Some(hunk) = self.hunks.get(file_index).and_then(|h| h.get(hunk_index)) else {
                    continue;
                };
                // A soft full-width info-tinted band: a faint 1px underline alone made hunk
                // boundaries nearly invisible while scrolling.
                plan.rect(
                    bounds.x,
                    y,
                    bounds.width,
                    metrics.hunk_header_height,
                    palette.hunk_header,
                );
                let picked = self
                    .hunk_mark(file_index, hunk_index)
                    .is_some_and(|mark| mark != CheckState::Off);
                if picked {
                    plan.rect(
                        bounds.x,
                        y,
                        bounds.width,
                        metrics.hunk_header_height,
                        Color {
                            a: 0.14,
                            ..palette.accent
                        },
                    );
                }
                let (header_x, header_clip) = if height_index.is_full_width(file_index) {
                    (unified_region_x, unified_clip)
                } else {
                    (text_region_x, content_clip)
                };
                let gutter = header_x - metrics.prefix_width;
                self.draw_hunk_title(
                    plan,
                    &hunk.header,
                    y,
                    gutter,
                    bounds.x + header_x + TEXT_X_PADDING,
                    self.is_collapsed(file_index),
                    header_clip,
                    bounds,
                );
                if let Some(actions) = &props.hunk_actions
                    && let Some((row, placed)) =
                        self.hunk_action_layout(file_index, hunk_index, y, bounds)
                    && (!actions.rows[row].hover
                        || picked
                        || state.hovered_hunk == Some((file_index, hunk_index)))
                {
                    let row_spec = &actions.rows[row];
                    let strip = Rectangle {
                        x: bounds.x,
                        y,
                        width: bounds.width,
                        height: metrics.hunk_header_height,
                    };
                    self.strip_row(plan, row_spec, &placed, &actions.style, strip, bounds);
                    for (button, (spec, at)) in row_spec.buttons.iter().zip(&placed).enumerate() {
                        draw_button(
                            plan,
                            row_spec,
                            spec,
                            at,
                            &actions.style,
                            state.hovered_hunk_action == Some(ActionHit::Hunk(row, button)),
                            bounds,
                        );
                    }
                }
            }

            if props.hunk_actions.is_some() {
                let height = metrics.hunk_header_height;
                let first = height_index
                    .file_strips
                    .partition_point(|&(_, top)| top + height < visible_top);
                let actions = props.hunk_actions.as_ref();
                for &(file_index, top) in &height_index.file_strips[first..] {
                    if top > visible_bottom {
                        break;
                    }
                    let Some(strip) = actions.and_then(|actions| actions.file_strip(file_index))
                    else {
                        continue;
                    };
                    self.draw_file_strip(
                        plan,
                        &height_index,
                        strip,
                        bounds.y + (top - visible_top),
                        bounds,
                        state.hovered_hunk_action,
                    );
                }
            }
        }

        // Selection backgrounds *before* the text so glyphs render on top; before-and-
        // translucent is how text editors render selection.
        if let Some((sel_start, sel_end)) = selection_range {
            for (lane_geometry, lane) in &lanes {
                // A side-by-side selection lives in one column; the mirror column shows no
                // highlight even where it carries the same (context) line.
                if let (Some(sel_lane), Some(lane_side)) = (state.selection_lane, *lane)
                    && sel_lane != lane_side
                {
                    continue;
                }
                let cw = lane_geometry.char_width;
                for row in &visible_rows {
                    let Some(line_index) = row.line_in_lane(*lane) else {
                        continue;
                    };
                    let Some(line) = self.line(row.file_index, row.hunk_index, line_index) else {
                        continue;
                    };
                    let row_pos_start =
                        body_position(row.file_index, row.hunk_index, line_index, 0);
                    let row_pos_end = body_position(
                        row.file_index,
                        row.hunk_index,
                        line_index,
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
                    // A row selected end to end shows a short tail past its last glyph: the
                    // usual cue that the line break is part of the selection.
                    let is_full_line = sel_start <= row_pos_start && row_pos_end < sel_end;
                    let key = body_geometry_key(
                        row.file_index,
                        row.hunk_index,
                        line_index,
                        lane_geometry.wrap_width,
                        cw,
                    );
                    let geometry =
                        self.geometry(memo, key, &line.content, lane_geometry.wrap_width);
                    self.byte_range_highlight(
                        plan,
                        &geometry,
                        row.y,
                        line_start_byte,
                        line_end_byte,
                        palette.selection,
                        0.0,
                        if is_full_line { cw * 0.6 } else { 0.0 },
                        lane_geometry,
                    );
                }
            }
        }

        if let Some(find) = &props.find {
            self.find_highlights(plan, find, memo, &visible_rows, &lanes);
        }

        for row in &visible_rows {
            if let (Some(split), Some(_)) = (&split, row.pair) {
                self.draw_split_row(
                    plan,
                    row,
                    split,
                    bounds,
                    content_width,
                    horizontal_offset,
                    memo,
                    code,
                );
                continue;
            }
            let Some(line) = self.line(row.file_index, row.hunk_index, row.line_index) else {
                continue;
            };
            self.draw_row(
                plan,
                row,
                line,
                unified_width,
                horizontal_offset,
                unified_clip,
                bounds,
                memo,
                code,
            );
        }

        // Pinned sticky file header, over the code scrolling under it.
        if let (Some(file_index), Some(y)) = (sticky_file, sticky_pin_y) {
            self.draw_file_header(plan, file_index, y, bounds, state.header_hover(file_index));
        }
    }
}

/// A hunk header `@@ -a,b +c,d @@ context` as its ranges and its context, or
/// `None` when it isn't one.
fn split_hunk_header(header: &str) -> Option<(&str, &str)> {
    let rest = header.strip_prefix("@@")?;
    let end = rest.find("@@")?;
    Some((rest[..end].trim(), rest[end + 2..].trim()))
}

/// `text` with its start cut to an ellipsis to fit `max_width`: a path keeps the
/// directories nearest its file.
fn truncate_start(text: &str, size: f32, font: Font, max_width: f32) -> String {
    if measure::line_width(text, size, font) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for start in 1..chars.len() {
        let candidate: String = std::iter::once('\u{2026}')
            .chain(chars[start..].iter().copied())
            .collect();
        if measure::line_width(&candidate, size, font) <= max_width {
            return candidate;
        }
    }
    String::new()
}

/// `text` cut with an ellipsis to fit `max_width`.
fn truncate_end(text: &str, size: f32, font: Font, max_width: f32) -> String {
    if measure::line_width(text, size, font) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = |count: usize| -> String {
        chars[..count]
            .iter()
            .copied()
            .chain(std::iter::once('\u{2026}'))
            .collect()
    };
    // The longest prefix that still fits, found by halving.
    let (mut low, mut high) = (0, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if measure::line_width(&cut(mid), size, font) <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    cut(low)
}

/// A placed button: fill, border and label.
fn draw_button(
    plan: &mut Plan,
    row: &hunk_actions::Row,
    button: &hunk_actions::Button,
    placed: &Placed,
    style: &hunk_actions::Style,
    hovered: bool,
    clip: Rectangle,
) {
    match button.tone {
        hunk_actions::Tone::Raised => plan.boxed(
            placed.rect,
            style.radius,
            if hovered {
                style.hover_fill
            } else {
                style.fill
            },
            style.border,
            BorderStyle::Solid,
        ),
        hunk_actions::Tone::Ghost if hovered => {
            plan.rounded(placed.rect, style.radius, style.ghost_hover_fill)
        }
        hunk_actions::Tone::Ghost => {}
        hunk_actions::Tone::On => plan.rounded(placed.rect, style.radius, style.on_fill),
    }
    draw_runs(
        plan,
        &button.runs,
        placed,
        style,
        hunk_actions::baseline(row, style, placed.rect),
        clip,
    );
}

/// A placed button's label runs, from its inset on, on `baseline`.
fn draw_runs(
    plan: &mut Plan,
    runs: &[Run],
    placed: &Placed,
    style: &hunk_actions::Style,
    baseline: f32,
    clip: Rectangle,
) {
    let runs = if placed.compact { &runs[..1] } else { runs };
    let mut x = placed.rect.x + hunk_actions::PAD_X;
    for run in runs {
        draw_run(
            plan,
            run,
            Point::new(x, baseline),
            placed.rect,
            style.size,
            clip,
        );
        x += hunk_actions::run_width(run, style.size);
    }
}

/// One run, left-aligned at `at.x` with its baseline on `at.y`, in a line box as tall
/// as `button` and centred on it if it's an icon.
fn draw_run(plan: &mut Plan, run: &Run, at: Point, button: Rectangle, size: f32, clip: Rectangle) {
    let height = button.height;
    plan.line(
        &run.text,
        run.font,
        size,
        at.x,
        hunk_actions::run_top(run, size, height, at.y, button.center_y()),
        height,
        run.color,
        clip,
    );
}
