//! Buttons riding a diff hunk's header strip, right-aligned: what a conflict
//! region's take-a-side controls are drawn with. The diff view is one custom
//! widget, so these are painted and hit-tested by it rather than being iced
//! buttons; this module holds their model and geometry, and paints them in
//! the raised button's colours the caller resolves from the theme.

use iced::advanced::{renderer, text};
use iced::{Border, Color, Font, Pixels, Point, Rectangle, Shadow, Size, alignment};

use crate::measure;

/// Inset of a label inside its button, and the gap between buttons.
const PAD_X: f32 = 8.0;
const GAP: f32 = 6.0;
/// Space kept between a strip's own header text and the first button.
const LEAD_GAP: f32 = 16.0;
/// Tallest a button gets; the strip's height minus a margin below that.
const MAX_HEIGHT: f32 = 24.0;
/// Width of the bar marking the focused strip at its left edge.
const FOCUS_BAR: f32 = 3.0;

/// One run of a button's label.
#[derive(Debug, Clone)]
pub struct Run {
    pub text: String,
    pub font: Font,
    pub color: Color,
}

/// A button: its label runs. The first run is kept when the strip is too
/// narrow for the whole label.
#[derive(Debug, Clone)]
pub struct Button {
    pub runs: Vec<Run>,
}

/// The buttons of one hunk header.
#[derive(Debug, Clone)]
pub struct Row {
    pub file: usize,
    pub hunk: usize,
    /// The keyboard's region: a bar marks the strip.
    pub focused: bool,
    /// A word before the buttons ("Take").
    pub lead: Option<Run>,
    pub buttons: Vec<Button>,
}

/// Colours and type, resolved by the caller from the theme.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub fill: Color,
    pub hover_fill: Color,
    pub border: Color,
    pub focus: Color,
    pub radius: f32,
}

/// Hunk-strip buttons for a diff view, and where their presses go.
pub struct HunkActions<'a, Message> {
    /// Sorted by `(file, hunk)`.
    pub rows: Vec<Row>,
    pub style: Style,
    /// `(row, button)` of a press to the message it sends.
    pub on_press: Box<dyn Fn(usize, usize) -> Message + 'a>,
    /// Scroll this `(file, hunk)` into view whenever the token changes.
    pub reveal: Option<(usize, usize, u64)>,
}

impl<Message> HunkActions<'_, Message> {
    /// The row for `(file, hunk)`, by index.
    pub fn row(&self, file: usize, hunk: usize) -> Option<usize> {
        self.rows
            .binary_search_by(|row| (row.file, row.hunk).cmp(&(file, hunk)))
            .ok()
    }
}

/// A laid-out button: its box, and whether its label was cut to the first
/// run.
pub struct Placed {
    pub rect: Rectangle,
    pub compact: bool,
}

fn run_width(run: &Run, size: f32) -> f32 {
    measure::line_width(&run.text, size, run.font)
}

fn button_width(button: &Button, size: f32, compact: bool) -> f32 {
    let runs = if compact {
        &button.runs[..1]
    } else {
        &button.runs
    };
    runs.iter().map(|run| run_width(run, size)).sum::<f32>() + 2.0 * PAD_X
}

/// Lay `row`'s buttons out right-aligned in `strip`, ending `right_inset`
/// short of its right edge and starting no further left than `min_x` (the
/// end of the strip's own text) if they can help it: labels shrink to their
/// first run before that.
pub fn place(
    row: &Row,
    style: &Style,
    strip: Rectangle,
    right_inset: f32,
    min_x: f32,
) -> Vec<Placed> {
    let height = (strip.height - 6.0).clamp(1.0, MAX_HEIGHT);
    let y = strip.y + (strip.height - height) / 2.0;
    let right = strip.x + strip.width - right_inset;
    let lead = row
        .lead
        .as_ref()
        .map_or(0.0, |lead| run_width(lead, style.size) + GAP);
    let total = |compact: bool| {
        row.buttons
            .iter()
            .map(|button| button_width(button, style.size, compact))
            .sum::<f32>()
            + GAP * row.buttons.len().saturating_sub(1) as f32
            + lead
    };
    let compact = right - total(false) < min_x + LEAD_GAP;
    let mut x = right - total(compact) + lead;
    row.buttons
        .iter()
        .map(|button| {
            let width = button_width(button, style.size, compact);
            let rect = Rectangle {
                x,
                y,
                width,
                height,
            };
            x += width + GAP;
            Placed { rect, compact }
        })
        .collect()
}

/// Paint `row`'s lead word before its first button, and its focus bar
/// (when focused) over the strip's left edge.
pub fn draw_row<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    row: &Row,
    placed: &[Placed],
    style: &Style,
    strip: Rectangle,
    clip: Rectangle,
) {
    if let (Some(lead), Some(first)) = (&row.lead, placed.first()) {
        let width = run_width(lead, style.size);
        draw_run(
            renderer,
            lead,
            Point::new(first.rect.x - GAP - width, first.rect.center_y()),
            width,
            first.rect.height,
            style.size,
            clip,
        );
    }
    if !row.focused {
        return;
    }
    renderer.fill_quad(
        renderer::Quad {
            bounds: Rectangle {
                width: FOCUS_BAR,
                ..strip
            },
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        style.focus,
    );
}

/// Paint one placed button.
pub fn draw_button<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    button: &Button,
    placed: &Placed,
    style: &Style,
    hovered: bool,
    clip: Rectangle,
) {
    let rect = placed.rect;
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border {
                width: 1.0,
                color: style.border,
                radius: style.radius.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        if hovered {
            style.hover_fill
        } else {
            style.fill
        },
    );
    let runs = if placed.compact {
        &button.runs[..1]
    } else {
        &button.runs[..]
    };
    let mut x = rect.x + PAD_X;
    for run in runs {
        let width = run_width(run, style.size);
        draw_run(
            renderer,
            run,
            Point::new(x, rect.center_y()),
            width,
            rect.height,
            style.size,
            clip,
        );
        x += width;
    }
}

/// One run, left-aligned at `at.x` and centred on `at.y`.
fn draw_run<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    run: &Run,
    at: Point,
    width: f32,
    height: f32,
    size: f32,
    clip: Rectangle,
) {
    renderer.fill_text(
        text::Text {
            content: run.text.clone(),
            bounds: Size::new(width + 1.0, height),
            size: Pixels(size),
            line_height: text::LineHeight::Absolute(Pixels(height)),
            font: run.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        },
        at,
        run.color,
        clip,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(labels: &[&[&str]]) -> Row {
        Row {
            file: 0,
            hunk: 0,
            focused: false,
            lead: None,
            buttons: labels
                .iter()
                .map(|runs| Button {
                    runs: runs
                        .iter()
                        .map(|text| Run {
                            text: (*text).to_owned(),
                            font: Font::DEFAULT,
                            color: Color::BLACK,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn style() -> Style {
        Style {
            size: 12.0,
            fill: Color::WHITE,
            hover_fill: Color::WHITE,
            border: Color::BLACK,
            focus: Color::BLACK,
            radius: 7.0,
        }
    }

    #[test]
    fn buttons_end_at_the_inset_in_order() {
        let row = row(&[&["abc", " first"], &["Base"]]);
        let strip = Rectangle::new(Point::new(0.0, 100.0), Size::new(800.0, 30.0));
        let placed = place(&row, &style(), strip, 16.0, 100.0);
        assert_eq!(placed.len(), 2);
        assert!(placed[0].rect.x < placed[1].rect.x);
        let end = placed[1].rect.x + placed[1].rect.width;
        assert!((end - 784.0).abs() < 0.01, "{end}");
        assert!(placed.iter().all(|p| !p.compact));
        // Centred in the strip.
        assert!((placed[0].rect.center_y() - 115.0).abs() < 0.01);
    }

    #[test]
    fn a_narrow_strip_keeps_only_the_first_runs() {
        let row = row(&[&["abc", " a long description here"], &["Base"]]);
        let strip = Rectangle::new(Point::ORIGIN, Size::new(300.0, 30.0));
        let placed = place(&row, &style(), strip, 16.0, 200.0);
        assert!(placed.iter().all(|p| p.compact));
    }
}
