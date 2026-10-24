//! Buttons riding a diff hunk's header strip, right-aligned: what a conflict
//! region's take-a-side controls are drawn with. A file strip under a file's
//! header carries the same buttons for the whole file, after ghost tools
//! that follow its own text. The diff view is one custom widget, so these
//! are painted and hit-tested by it rather than being iced buttons; this
//! module holds their model and geometry, and paints them in the button
//! colours the caller resolves from the theme.

use iced::advanced::{renderer, text};
use iced::{Border, Color, Font, Pixels, Point, Rectangle, Shadow, Size, alignment};

use crate::measure;
use crate::typography::TextBox;

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

/// A ghost button (no fill or border at rest) following a file strip's
/// text. The first run is kept when the strip is too narrow for the whole
/// label.
#[derive(Debug, Clone)]
pub struct Tool {
    pub runs: Vec<Run>,
    pub enabled: bool,
}

/// The strip of its own a file gets under its header, above its first
/// hunk: what a conflicted file's whole-file controls ride on.
#[derive(Debug, Clone)]
pub struct FileStrip {
    pub file: usize,
    /// Its text, at the left where a hunk strip has its header.
    pub label: String,
    pub tools: Vec<Tool>,
    /// The right-aligned buttons, as on a hunk strip (`hunk` unused).
    pub row: Row,
}

/// Colours and type, resolved by the caller from the theme.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub fill: Color,
    pub hover_fill: Color,
    /// A hovered tool's fill.
    pub ghost_hover_fill: Color,
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
    /// Sorted by `file`.
    pub files: Vec<FileStrip>,
    /// `(strip, button)` of a press on a file strip, its tools counted
    /// before its row's buttons.
    pub on_file_press: Box<dyn Fn(usize, usize) -> Message + 'a>,
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

    /// The strip under `file`'s header, by index.
    pub fn file_strip(&self, file: usize) -> Option<usize> {
        self.files
            .binary_search_by(|strip| strip.file.cmp(&file))
            .ok()
    }
}

/// A file strip laid out: whether its label shows, its tools' boxes, then
/// its row's buttons.
pub struct FilePlaced {
    pub label: bool,
    pub tools: Vec<Placed>,
    pub buttons: Vec<Placed>,
}

impl FilePlaced {
    /// The enabled button under `point`, tools counted first.
    pub fn hit(&self, strip: &FileStrip, point: Point) -> Option<usize> {
        let tool = self
            .tools
            .iter()
            .zip(&strip.tools)
            .position(|(placed, tool)| tool.enabled && placed.rect.contains(point));
        tool.or_else(|| {
            self.buttons
                .iter()
                .position(|placed| placed.rect.contains(point))
                .map(|button| strip.tools.len() + button)
        })
    }
}

fn runs_width(runs: &[Run], size: f32, compact: bool) -> f32 {
    let runs = if compact { &runs[..1] } else { runs };
    runs.iter().map(|run| run_width(run, size)).sum::<f32>() + 2.0 * PAD_X
}

/// Lay a file strip out in `strip`: its label at `text_x`, `label_width`
/// wide, its tools left-aligned after it, its row's buttons as [`place`]
/// puts them. When they don't all fit whole, the label goes first (the
/// buttons and the region strips below say as much), then the buttons
/// shrink to their first runs (the change ids, named in full on the region
/// strips), then the tools to their icons.
pub fn place_file(
    file: &FileStrip,
    style: &Style,
    strip: Rectangle,
    right_inset: f32,
    text_x: f32,
    label_width: f32,
) -> FilePlaced {
    let lay = |label: bool, compact: bool| {
        let start = if label {
            text_x + label_width + LEAD_GAP
        } else {
            text_x
        };
        let mut x = start;
        let tools: Vec<Placed> = file
            .tools
            .iter()
            .map(|tool| {
                let width = runs_width(&tool.runs, style.size, compact);
                let rect = Rectangle {
                    x,
                    width,
                    ..button_box(strip)
                };
                x += width + GAP;
                Placed { rect, compact }
            })
            .collect();
        let end = tools
            .last()
            .map_or(start - LEAD_GAP, |tool| tool.rect.x + tool.rect.width);
        let buttons = place(&file.row, style, strip, right_inset, end);
        FilePlaced {
            label,
            tools,
            buttons,
        }
    };
    let whole = |placed: &FilePlaced| placed.buttons.first().is_none_or(|first| !first.compact);
    let lead = file
        .row
        .lead
        .as_ref()
        .map_or(0.0, |lead| run_width(lead, style.size) + GAP);
    let fits = |placed: &FilePlaced| match (placed.tools.last(), placed.buttons.first()) {
        (Some(tool), Some(first)) => {
            tool.rect.x + tool.rect.width + LEAD_GAP <= first.rect.x - lead
        }
        _ => true,
    };
    let with_label = lay(true, false);
    if whole(&with_label) {
        return with_label;
    }
    let without = lay(false, false);
    if fits(&without) {
        return without;
    }
    lay(false, true)
}

/// A button's box in `strip`, before its x and width.
fn button_box(strip: Rectangle) -> Rectangle {
    let height = (strip.height - 6.0).clamp(1.0, MAX_HEIGHT);
    Rectangle {
        x: strip.x,
        y: strip.y + (strip.height - height) / 2.0,
        width: 0.0,
        height,
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
    runs_width(&button.runs, size, compact)
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
    let Rectangle { y, height, .. } = button_box(strip);
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
            Point::new(first.rect.x - GAP - width, baseline(row, style, first.rect)),
            first.rect,
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

/// The baseline every run of `row` sits on when its buttons are `rect`
/// tall: the one that optically centres the row's tallest caps in the
/// button, so mono ids and UI-font words read as one line.
pub fn baseline(row: &Row, style: &Style, rect: Rectangle) -> f32 {
    let cap_height = row
        .lead
        .iter()
        .chain(row.buttons.iter().flat_map(|button| &button.runs))
        .filter(|run| !is_glyph(run))
        .map(|run| TextBox::new(run.font, style.size, rect.height).cap_height())
        .fold(0.0, f32::max);
    rect.center_y() + cap_height / 2.0
}

/// An icon: centred on the caps rather than set on the baseline.
fn is_glyph(run: &Run) -> bool {
    run.font == crate::icons::ICON_FONT
}

/// Paint one placed tool of a file strip, its text on `baseline` (the
/// strip's row's).
pub fn draw_tool<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    tool: &Tool,
    placed: &Placed,
    style: &Style,
    baseline: f32,
    hovered: bool,
    clip: Rectangle,
) {
    let rect = placed.rect;
    if hovered && tool.enabled {
        renderer.fill_quad(
            renderer::Quad {
                bounds: rect,
                border: Border {
                    radius: style.radius.into(),
                    ..Border::default()
                },
                shadow: Shadow::default(),
                snap: true,
            },
            style.ghost_hover_fill,
        );
    }
    draw_runs(renderer, &tool.runs, placed, style, baseline, clip);
}

/// Paint one placed button of `row`.
pub fn draw_button<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    row: &Row,
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
    draw_runs(
        renderer,
        &button.runs,
        placed,
        style,
        baseline(row, style, rect),
        clip,
    );
}

/// A placed button's label runs, from its inset on, on `baseline`.
fn draw_runs<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    runs: &[Run],
    placed: &Placed,
    style: &Style,
    baseline: f32,
    clip: Rectangle,
) {
    let runs = if placed.compact { &runs[..1] } else { runs };
    let mut x = placed.rect.x + PAD_X;
    for run in runs {
        draw_run(
            renderer,
            run,
            Point::new(x, baseline),
            placed.rect,
            style.size,
            clip,
        );
        x += run_width(run, style.size);
    }
}

/// Where a run's line box starts, `height` tall, for its baseline to land
/// on `baseline`; an icon's, for it to centre on `center_y` instead.
fn run_top(run: &Run, size: f32, height: f32, baseline: f32, center_y: f32) -> f32 {
    let line = TextBox::new(run.font, size, height);
    let baseline = if is_glyph(run) {
        line.baseline_for_center(center_y)
    } else {
        baseline
    };
    baseline - line.baseline()
}

/// One run, left-aligned at `at.x` with its baseline on `at.y`, in a line
/// box as tall as `button` and centred on it if it's an icon.
fn draw_run<Renderer: text::Renderer>(
    renderer: &mut Renderer,
    run: &Run,
    at: Point,
    button: Rectangle,
    size: f32,
    clip: Rectangle,
) {
    let width = run_width(run, size);
    let height = button.height;
    renderer.fill_text(
        text::Text {
            content: run.text.clone(),
            bounds: Size::new(width + 1.0, height),
            size: Pixels(size),
            line_height: text::LineHeight::Absolute(Pixels(height)),
            font: run.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        },
        Point::new(at.x, run_top(run, size, height, at.y, button.center_y())),
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
            ghost_hover_fill: Color::WHITE,
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
    fn mono_and_ui_runs_share_the_optically_centred_baseline() {
        let mut row = row(&[&["sknyzssr", " rebase destination"]]);
        row.buttons[0].runs[0].font = Font::MONOSPACE;
        row.lead = Some(Run {
            text: "Take".to_owned(),
            font: Font::DEFAULT,
            color: Color::BLACK,
        });
        let style = style();
        let rect = Rectangle::new(Point::new(0.0, 100.0), Size::new(200.0, 24.0));
        let shared = baseline(&row, &style, rect);
        let runs = row.buttons[0].runs.iter().chain(row.lead.as_ref());
        let mut tallest: f32 = 0.0;
        for run in runs {
            let line = TextBox::new(run.font, style.size, rect.height);
            let top = run_top(run, style.size, rect.height, shared, rect.center_y());
            assert!((top + line.baseline() - shared).abs() < 1e-4);
            tallest = tallest.max(line.cap_height());
        }
        // The tallest caps are centred in the button.
        assert!((shared - tallest / 2.0 - rect.center_y()).abs() < 1e-4);
    }

    fn file(tools: &[&[&str]], buttons: &[&[&str]]) -> FileStrip {
        let tools = row(tools)
            .buttons
            .into_iter()
            .map(|button| Tool {
                runs: button.runs,
                enabled: true,
            })
            .collect();
        FileStrip {
            file: 0,
            label: "2 sides".to_owned(),
            tools,
            row: row(buttons),
        }
    }

    #[test]
    fn a_file_strip_drops_its_label_then_shrinks_its_buttons_then_its_tools() {
        let strip = file(
            &[&["E", " Edit markers"], &["M", " Merge tool"]],
            &[&["abc", " rebase destination"], &["Base"]],
        );
        let at = |width: f32| {
            place_file(
                &strip,
                &style(),
                Rectangle::new(Point::ORIGIN, Size::new(width, 30.0)),
                16.0,
                100.0,
                50.0,
            )
        };
        let wide = at(2000.0);
        assert!(wide.label);
        assert!(wide.tools.iter().chain(&wide.buttons).all(|p| !p.compact));
        assert!((wide.tools[0].rect.x - 166.0).abs() < 0.01);
        let tools_end = |placed: &FilePlaced| {
            let last = placed.tools.last().unwrap().rect;
            last.x + last.width
        };
        assert!(tools_end(&wide) < wide.buttons[0].rect.x);
        let full = wide.buttons.iter().map(|p| p.rect.width).sum::<f32>();
        let tools = wide.tools.iter().map(|p| p.rect.width).sum::<f32>();
        // Whole with the label: 100 + 50 + 16 + tools + 6 + 16 + full + 6
        // + 16. Without it, 50 + 16 less.
        let no_label = at(100.0 + tools + full + 50.0);
        assert!(!no_label.label);
        assert!((no_label.tools[0].rect.x - 100.0).abs() < 0.01);
        assert!(
            no_label
                .tools
                .iter()
                .chain(&no_label.buttons)
                .all(|p| !p.compact)
        );
        // Narrower still: the buttons go to their ids, then the tools to
        // their icons.
        let short = at(100.0 + tools + full + 20.0);
        assert!(!short.label);
        assert!(short.tools.iter().all(|p| !p.compact));
        assert!(short.buttons.iter().all(|p| p.compact));
        assert!(tools_end(&short) < short.buttons[0].rect.x);
        let narrow = at(100.0 + full);
        assert!(narrow.tools.iter().all(|p| p.compact));
        assert!(narrow.buttons.iter().all(|p| p.compact));
    }

    #[test]
    fn a_narrow_strip_keeps_only_the_first_runs() {
        let row = row(&[&["abc", " a long description here"], &["Base"]]);
        let strip = Rectangle::new(Point::ORIGIN, Size::new(300.0, 30.0));
        let placed = place(&row, &style(), strip, 16.0, 200.0);
        assert!(placed.iter().all(|p| p.compact));
    }
}
