//! Where glyphs actually sit, from the fonts the app loads.
//!
//! iced (cosmic-text) lays a line of text out in a *line box*: the font's
//! ascent + descent, centred in the line height, with the baseline `ascent`
//! below the top of that inner box. Centring the line box (what
//! `align_y(Center)` and a centred container do) therefore centres
//! ascent + descent, not the ink — and since a font's descent is roughly a
//! third of its cap height, capitals and digits ride visibly high or low,
//! by an amount that differs per font. Rows that put fonts or sizes side by
//! side by their boxes disagree on where their baselines are.
//!
//! This module reads each font's real ascent, descent and cap height (via the
//! same cosmic-text font system the renderer draws with), and answers the two
//! questions the UI keeps asking:
//!
//! * **Baseline alignment** — where a run's baseline lies in its line box
//!   ([`TextBox::baseline`]), so runs in different fonts, sizes or containers
//!   can be placed on one shared baseline ([`baseline_row`], [`group`],
//!   [`TextBox::center_for_baseline`]).
//! * **Optical centring** — the CSS `text-box: trim-both cap alphabetic`
//!   model: a text's visual box is cap-height top to alphabetic baseline, and
//!   that is what gets centred in a control ([`TextBox::trim_shift`],
//!   [`centered`], [`TextBox::baseline_for_center`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use iced::advanced::graphics::text::{Paragraph, font_system};
use iced::advanced::text::{self, Paragraph as _, Shaping, Text};
use iced::widget::{container, row};
use iced::{Element, Font, Length, Padding, Pixels, Size, alignment};

/// A font's vertical metrics, as fractions of its em size. `descent` is
/// positive (below the baseline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub cap_height: f32,
}

impl FontMetrics {
    /// Used when a font can't be resolved: typical sans proportions.
    const FALLBACK: Self = Self {
        ascent: 0.95,
        descent: 0.25,
        cap_height: 0.70,
    };

    /// The metrics of the face `font` resolves to for Latin text, read once
    /// and cached.
    pub fn of(font: Font) -> Self {
        static CACHE: OnceLock<Mutex<HashMap<Font, FontMetrics>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(metrics) = cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&font).copied())
        {
            return metrics;
        }
        let mut metrics = read_metrics(font).unwrap_or(Self::FALLBACK);
        if font == crate::icons::ICON_FONT {
            // An icon's ink fills its em box rather than reaching cap
            // height: its visual box is the whole ascent + descent.
            metrics.cap_height = metrics.ascent;
        }
        if let Ok(mut cache) = cache.lock() {
            cache.insert(font, metrics);
        }
        metrics
    }
}

/// Shape a capital in `font` the way the renderer would, and read the metrics
/// of the face the shaper actually picked (a generic family resolves to a
/// platform face; a named one may fall back).
fn read_metrics(font: Font) -> Option<FontMetrics> {
    let paragraph = Paragraph::with_text(Text {
        content: "H",
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(100.0),
        line_height: text::LineHeight::Relative(1.0),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: text::Wrapping::None,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    });
    let glyph = paragraph
        .buffer()
        .layout_runs()
        .next()?
        .glyphs
        .first()?
        .clone();
    let mut system = font_system().write().ok()?;
    let face = system.raw().get_font(glyph.font_id, glyph.font_weight)?;
    let metrics = face.metrics();
    let em = f32::from(metrics.units_per_em);
    if em <= 0.0 {
        return None;
    }
    let ascent = metrics.ascent / em;
    let descent = -metrics.descent / em;
    // Fonts without an OS/2 cap height: capitals reach ~70% of the em.
    let cap_height = metrics.cap_height.map_or(0.70, |cap| cap / em);
    (ascent.is_finite() && descent.is_finite() && cap_height > 0.0).then_some(FontMetrics {
        ascent,
        descent,
        cap_height,
    })
}

/// One line of text in one font and size, laid out in a line box
/// `line_height` pixels tall. All positions are measured down from the top
/// of the line box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextBox {
    metrics: FontMetrics,
    size: f32,
    line_height: f32,
}

impl TextBox {
    pub fn new(font: Font, size: f32, line_height: f32) -> Self {
        Self::with_metrics(FontMetrics::of(font), size, line_height)
    }

    /// A box with iced's default line height (1.3 × size), which every
    /// `text` widget without an explicit `line_height` uses.
    pub fn default_line(font: Font, size: f32) -> Self {
        Self::new(font, size, size * DEFAULT_LINE_HEIGHT)
    }

    pub fn with_metrics(metrics: FontMetrics, size: f32, line_height: f32) -> Self {
        Self {
            metrics,
            size,
            line_height,
        }
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// The baseline: cosmic-text centres ascent + descent in the line box and
    /// puts the baseline `ascent` below the top of that.
    pub fn baseline(&self) -> f32 {
        let ascent = self.metrics.ascent * self.size;
        let descent = self.metrics.descent * self.size;
        (self.line_height - (ascent + descent)) / 2.0 + ascent
    }

    /// Height of capitals above the baseline.
    pub fn cap_height(&self) -> f32 {
        self.metrics.cap_height * self.size
    }

    /// Top of the capitals.
    pub fn cap_top(&self) -> f32 {
        self.baseline() - self.cap_height()
    }

    /// How far down to move the text so its trimmed box (cap top to
    /// baseline) is centred where the line box's centre is. Negative moves
    /// it up.
    pub fn trim_shift(&self) -> f32 {
        self.line_height / 2.0 - (self.cap_top() + self.baseline()) / 2.0
    }

    /// The baseline y that centres the trimmed box on `center_y`.
    pub fn baseline_for_center(&self, center_y: f32) -> f32 {
        center_y + self.cap_height() / 2.0
    }

    /// The y to optically centre this text on (see [`Self::baseline_for_center`])
    /// so that its baseline lands on `baseline_y`: how a run in another font
    /// or size joins a line whose baseline is already set.
    pub fn center_for_baseline(&self, baseline_y: f32) -> f32 {
        baseline_y - self.cap_height() / 2.0
    }
}

/// iced's default `LineHeight::Relative` for a `text` widget.
pub const DEFAULT_LINE_HEIGHT: f32 = 1.3;

/// Default row-height factor of the code grid — 1.85 × the font size gives
/// the historical 22px rows at the default 12px code font.
pub const DEFAULT_CODE_LINE_HEIGHT: f32 = 1.85;

/// Typography of the code surfaces, resolved and clamped at load. One value
/// object so the diff view's metrics take the whole set at once.
#[derive(Debug, Clone, Copy)]
pub struct CodeTypography {
    /// Font size in px (`code_font_size`).
    pub size: f32,
    /// Row height as a multiple of `size` (`code_line_height`).
    pub line_height: f32,
    /// Vertical nudge of text within its row, px, positive = down
    /// (`code_baseline_offset`).
    pub baseline_offset: f32,
}

impl Default for CodeTypography {
    fn default() -> Self {
        Self {
            size: 12.0,
            line_height: DEFAULT_CODE_LINE_HEIGHT,
            baseline_offset: 0.0,
        }
    }
}

/// Asymmetric padding that moves a line box down by `shift` while keeping
/// it centred in whatever centres it: the box grows by `2 × |shift|` on one
/// side only, so centring the grown box moves the text by exactly `shift`.
pub fn shift_padding(shift: f32) -> Padding {
    if shift >= 0.0 {
        Padding {
            top: 2.0 * shift,
            ..Padding::ZERO
        }
    } else {
        Padding {
            bottom: -2.0 * shift,
            ..Padding::ZERO
        }
    }
}

/// `padding` around one line of text (a text input, a chip, a badge), moved
/// so the text is optically centred: the line box shifts by
/// [`TextBox::trim_shift`] and the box keeps its height.
pub fn trimmed_padding(padding: Padding, font: Font, size: f32) -> Padding {
    trim(padding, TextBox::default_line(font, size).trim_shift())
}

/// `padding` with `shift` moved from bottom to top. Where that would go
/// negative, both sides grow by the deficit instead: the box gets a touch
/// taller, the text still lands `shift` lower.
fn trim(padding: Padding, shift: f32) -> Padding {
    let (top, bottom) = (padding.top + shift, padding.bottom - shift);
    let deficit = (-top).max(-bottom).max(0.0);
    Padding {
        top: top + deficit,
        bottom: bottom + deficit,
        ..padding
    }
}

/// `content`, a single-line text in `font` at `size` with iced's default
/// line height, padded so that when a container centres it the cap box is
/// what lands in the middle (`text-box: trim-both cap alphabetic`). The
/// building block of every centred label: buttons, tabs, chips, inputs.
pub fn centered<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    font: Font,
    size: f32,
) -> Element<'a, Message> {
    container(content)
        .padding(shift_padding(
            TextBox::default_line(font, size).trim_shift(),
        ))
        .into()
}

/// One child of a [`baseline_row`]: an element, where its baseline and cap
/// top fall (measured down from its top edge), and its height.
pub struct Aligned<'a, Message> {
    element: Element<'a, Message>,
    line: Line,
}

/// The vertical facts [`baseline_row`] needs about a child.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Line {
    baseline: f32,
    cap_height: f32,
    height: f32,
    /// No text: centred on the line's caps rather than on its baseline.
    glyph: bool,
}

impl<'a, Message: 'a> Aligned<'a, Message> {
    /// A single-line `text` in `font` at `size` with iced's default line
    /// height.
    pub fn text(element: impl Into<Element<'a, Message>>, font: Font, size: f32) -> Self {
        Self::boxed(element, Padding::ZERO, TextBox::default_line(font, size))
    }

    /// A container (chip, badge) around one line of text, `padding` around
    /// the text's line box.
    pub fn boxed(
        element: impl Into<Element<'a, Message>>,
        padding: Padding,
        text: TextBox,
    ) -> Self {
        Self {
            element: element.into(),
            line: Line {
                baseline: padding.top + text.baseline(),
                cap_height: text.cap_height(),
                height: padding.top + text.line_height() + padding.bottom,
                glyph: false,
            },
        }
    }

    /// A chip, badge or keycap: one line of text in a styled box, its
    /// padding trimmed so the text is optically centred in the box
    /// ([`trimmed_padding`]) and the box placed by that text's baseline.
    pub fn chip(
        content: impl Into<Element<'a, Message>>,
        font: Font,
        size: f32,
        padding: Padding,
        style: impl Fn(&iced::Theme) -> container::Style + 'a,
    ) -> Self {
        let padding = trimmed_padding(padding, font, size);
        Self::boxed(
            container(content).padding(padding).style(style),
            padding,
            TextBox::default_line(font, size),
        )
    }

    /// The same child wrapped in something that doesn't move it vertically
    /// (a tooltip, a width limit).
    pub fn map(self, wrap: impl FnOnce(Element<'a, Message>) -> Element<'a, Message>) -> Self {
        Self {
            element: wrap(self.element),
            line: self.line,
        }
    }

    /// Fixed horizontal space between two children.
    pub fn gap(width: f32) -> Self {
        Self::glyph(iced::widget::Space::new().width(Length::Fixed(width)), 0.0)
    }

    /// Something with no text of its own (an icon, a dot): centred on the
    /// line's caps.
    pub fn glyph(element: impl Into<Element<'a, Message>>, height: f32) -> Self {
        Self {
            element: element.into(),
            line: Line {
                baseline: height,
                cap_height: height,
                height,
                glyph: true,
            },
        }
    }
}

/// A line whose children share one text baseline, whatever their fonts,
/// sizes or containers — where `align_y(Center)` would line up their box
/// centres instead. Each child drops by the difference between its baseline
/// and the deepest one, and the line as a whole is padded so that a parent
/// centring it centres its caps (see [`centered`]). Glyph children (icons)
/// are centred on the caps instead of sitting on the baseline.
pub fn baseline_row<'a, Message: 'a>(
    items: impl IntoIterator<Item = Aligned<'a, Message>>,
    spacing: f32,
) -> Element<'a, Message> {
    let line = group(items, spacing);
    let shift = line.line.height / 2.0 - (line.line.baseline - line.line.cap_height / 2.0);
    container(line.element).padding(shift_padding(shift)).into()
}

/// [`baseline_row`] without the centring, as one [`Aligned`] child of an
/// outer line: how a group with its own spacing (an icon and its label, an
/// id's two colours) joins a line and stays on its baseline.
pub fn group<'a, Message: 'a>(
    items: impl IntoIterator<Item = Aligned<'a, Message>>,
    spacing: f32,
) -> Aligned<'a, Message> {
    let items: Vec<Aligned<'a, Message>> = items.into_iter().collect();
    let lines: Vec<Line> = items.iter().map(|item| item.line).collect();
    let layout = line_layout(&lines);
    let mut row = row![].spacing(spacing).align_y(alignment::Vertical::Top);
    for (item, top) in items.into_iter().zip(&layout.tops) {
        row = row.push(container(item.element).padding(Padding {
            top: *top,
            ..Padding::ZERO
        }));
    }
    Aligned {
        element: row.into(),
        line: layout.line,
    }
}

/// Where each child of a [`group`] starts, and the line they make.
#[derive(Debug, PartialEq)]
struct LineLayout {
    tops: Vec<f32>,
    line: Line,
}

fn line_layout(lines: &[Line]) -> LineLayout {
    let texts = || lines.iter().filter(|line| !line.glyph);
    let baseline = texts().map(|line| line.baseline).fold(0.0, f32::max);
    let cap_height = texts().map(|line| line.cap_height).fold(0.0, f32::max);
    // The caps' centre, from the top of the row.
    let cap_center = baseline - cap_height / 2.0;
    let tops: Vec<f32> = lines
        .iter()
        .map(|line| {
            if line.glyph {
                // Centred on the caps (never above the row's top).
                (cap_center - line.height / 2.0).max(0.0)
            } else {
                drop_for(line.baseline, baseline)
            }
        })
        .collect();
    let height = lines
        .iter()
        .zip(&tops)
        .map(|(line, top)| top + line.height)
        .fold(0.0, f32::max);
    LineLayout {
        tops,
        line: Line {
            baseline,
            cap_height,
            height,
            glyph: texts().next().is_none(),
        },
    }
}

/// How far a child with `baseline` drops to meet `shared`.
fn drop_for(baseline: f32, shared: f32) -> f32 {
    (shared - baseline).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SF-like proportions: ascent 0.95, descent 0.25, caps 0.70.
    const SANS: FontMetrics = FontMetrics {
        ascent: 0.95,
        descent: 0.25,
        cap_height: 0.70,
    };
    /// A mono with a deeper descent and shorter caps.
    const MONO: FontMetrics = FontMetrics {
        ascent: 0.80,
        descent: 0.30,
        cap_height: 0.66,
    };

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} != {b}");
    }

    /// The fonts the app draws with resolve to real faces whose metrics are
    /// sane: caps under the ascent, a positive descent.
    #[test]
    fn real_fonts_report_sane_metrics() {
        for font in [Font::DEFAULT, Font::MONOSPACE] {
            let metrics = FontMetrics::of(font);
            println!("{font:?}: {metrics:?}");
            assert!(metrics.ascent > metrics.cap_height, "{font:?}: {metrics:?}");
            assert!(metrics.cap_height > 0.4, "{font:?}: {metrics:?}");
            assert!(metrics.descent > 0.0, "{font:?}: {metrics:?}");
        }
        // Whatever face a named family resolves to here (or the fallback),
        // its metrics are usable.
        let named = FontMetrics::of(Font::new("Cascadia Code"));
        println!("Cascadia Code: {named:?}");
        assert!(named.ascent > 0.0 && named.cap_height > 0.0);
    }

    #[test]
    fn the_baseline_follows_cosmic_texts_line_box() {
        // 10px text in a 13px box: ascent 9.5 + descent 2.5 = 12, centred
        // leaves 0.5 above, so the baseline is 0.5 + 9.5 down.
        let line = TextBox::with_metrics(SANS, 10.0, 13.0);
        close(line.baseline(), 10.0);
        close(line.cap_top(), 3.0);
    }

    #[test]
    fn trimming_centres_the_caps_not_the_line_box() {
        let line = TextBox::with_metrics(SANS, 10.0, 13.0);
        // Caps span 3.0..10.0, centred at 6.5 — the box's middle already.
        close(line.trim_shift(), 0.0);
        // A deeper descent pushes the caps up; the shift brings them back.
        let mono = TextBox::with_metrics(MONO, 10.0, 13.0);
        // ascent 8 + descent 3 = 11, 1.0 above → baseline 9, caps 2.4..9.
        close(mono.baseline(), 9.0);
        close(mono.trim_shift(), 6.5 - (2.4 + 9.0) / 2.0);
        // After shifting, the caps' centre is the box centre.
        let centre = (mono.cap_top() + mono.baseline()) / 2.0 + mono.trim_shift();
        close(centre, 6.5);
    }

    #[test]
    fn baseline_for_center_puts_the_caps_around_it() {
        let line = TextBox::with_metrics(MONO, 12.0, 16.0);
        let baseline = line.baseline_for_center(100.0);
        close(baseline - line.cap_height() / 2.0, 100.0);
        // And a run joining that line centres where it lands on it.
        close(line.center_for_baseline(baseline), 100.0);
    }

    fn line(text: TextBox, padding_top: f32, padding_bottom: f32) -> Line {
        Line {
            baseline: padding_top + text.baseline(),
            cap_height: text.cap_height(),
            height: padding_top + text.line_height() + padding_bottom,
            glyph: false,
        }
    }

    #[test]
    fn mixed_fonts_and_chips_meet_on_one_baseline() {
        let sans = TextBox::with_metrics(SANS, 11.0, 11.0 * DEFAULT_LINE_HEIGHT);
        let mono = TextBox::with_metrics(MONO, 13.0, 13.0 * DEFAULT_LINE_HEIGHT);
        let lines = [
            line(sans, 0.0, 0.0),
            line(mono, 0.0, 0.0),
            line(sans, 2.0, 2.0),
        ];
        let layout = line_layout(&lines);
        let baselines: Vec<f32> = lines
            .iter()
            .zip(&layout.tops)
            .map(|(line, top)| top + line.baseline)
            .collect();
        for baseline in &baselines {
            close(*baseline, baselines[0]);
        }
        // Nothing is pushed further down than it must be.
        assert!(layout.tops.contains(&0.0));
    }

    #[test]
    fn a_line_is_centred_on_its_tallest_caps_and_glyphs_on_those() {
        let sans = TextBox::with_metrics(SANS, 10.0, 13.0);
        let icon = Line {
            baseline: 14.0,
            cap_height: 14.0,
            height: 14.0,
            glyph: true,
        };
        let layout = line_layout(&[line(sans, 0.0, 0.0), icon]);
        // Caps span 3..10 (centre 6.5); the 14px icon centres there,
        // clamped so it doesn't start above the row.
        close(layout.tops[1], 0.0);
        // The row is 14 tall with the caps' centre at 6.5.
        close(layout.line.height, 14.0);
        close(layout.line.baseline - layout.line.cap_height / 2.0, 6.5);
    }

    #[test]
    fn trimmed_padding_never_goes_negative() {
        let padding = trim(Padding::from([0.0, 5.0]), -0.5);
        close(padding.top, 0.0);
        close(padding.bottom, 1.0);
        let padding = trim(Padding::from([6.0, 9.0]), 0.75);
        close(padding.top, 6.75);
        close(padding.bottom, 5.25);
        close(padding.left, 9.0);
    }

    #[test]
    fn shift_padding_moves_a_centred_box_by_the_shift() {
        // A box of height h centred in H has top (H - h) / 2; padding grows
        // it on one side, and the text inside starts after the top padding.
        let text_top = |shift: f32| {
            let padding = shift_padding(shift);
            let grown = 10.0 + padding.top + padding.bottom;
            (40.0 - grown) / 2.0 + padding.top
        };
        close(text_top(1.5) - text_top(0.0), 1.5);
        close(text_top(-2.0) - text_top(0.0), -2.0);
    }
}
