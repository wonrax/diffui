//! Headless one-line text measurement, shared by chip sizing, sidebar path
//! truncation, menu layout, and the diff view's column metrics.
//!
//! We previously approximated text width with `chars * 7px`-style heuristics,
//! which silently misbehaved for any glyph wider or narrower than the assumed
//! average — `@` clipped into `…` in revision IDs, abbreviated paths over- or
//! under-shot the available room, and badges would clip if the user ever
//! switched to a larger font. Going through real `cosmic_text` shaping fixes
//! the entire class of bug because it's the same engine the wgpu renderer
//! draws with, so measurements match painted pixels exactly.
//!
//! Why headless `iced::advanced::graphics::text::Paragraph` rather than the
//! renderer's `R::Paragraph`: much of this measurement runs in `view()`, well
//! before any `draw()` call, where the renderer's `Paragraph` type isn't
//! reachable without threading renderer generics through `main.rs`. The wgpu
//! renderer is built on top of `iced_graphics`, so the headless `Paragraph`
//! shapes text identically.

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::text::{self, Paragraph as _, Shaping, Text};
use iced::{Font, Pixels, Size, alignment};

/// Line box used by single-line UI text throughout the app.
pub const LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Rendered width of one line of `content`, shaped like UI text
/// (`Shaping::Advanced`).
pub fn line_width(content: &str, size: f32, font: Font) -> f32 {
    line_width_shaped(content, size, font, Shaping::Advanced)
}

/// Rendered extent (width × line box) of one line of `content`, for callers
/// sizing a box around the text — e.g. the sidebar's hover tooltip.
pub fn line_bounds(content: &str, size: f32, font: Font) -> Size {
    one_line(content, size, font, Shaping::Advanced).min_bounds()
}

/// [`line_width`] with an explicit shaping strategy, for callers that must
/// match a renderer path that draws with `Shaping::Basic` (the diff grid).
pub fn line_width_shaped(content: &str, size: f32, font: Font, shaping: Shaping) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    one_line(content, size, font, shaping).min_width()
}

/// Shape `content` exactly the way the diff grid draws a wrapped code line —
/// same font/size, absolute `line_height` box, `Shaping::Basic`, word-first
/// wrapping (`WordOrGlyph`: break at word boundaries, split only words wider
/// than the whole line) — bounded to `max_width`.
fn wrapped_code_line(
    content: &str,
    size: f32,
    font: Font,
    line_height: f32,
    max_width: f32,
) -> Paragraph {
    Paragraph::with_text(Text {
        content,
        bounds: Size::new(max_width.max(1.0), f32::INFINITY),
        size: Pixels(size),
        line_height: text::LineHeight::Absolute(Pixels(line_height.max(1.0))),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping: Shaping::Basic,
        wrapping: text::Wrapping::WordOrGlyph,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    })
}

/// How many visual lines `content` occupies when word-wrapped at `max_width`
/// (1 for a line that fits). Engine truth — the same shaping the renderer
/// uses, so reserved heights match painted pixels.
pub fn wrapped_line_count(
    content: &str,
    size: f32,
    font: Font,
    line_height: f32,
    max_width: f32,
) -> usize {
    let paragraph = wrapped_code_line(content, size, font, line_height, max_width);
    let lines = (paragraph.min_bounds().height / line_height.max(1.0)).round() as usize;
    lines.max(1)
}

/// Char offset at which each visual line of word-wrapped `content` starts:
/// index 0 is always present (and 0); one entry means no wrapping. The break
/// points come from hit-testing the shaped paragraph itself, so consumers
/// slicing the line for hit tests or highlight rects agree with the renderer
/// char-for-char.
///
/// May hold *fewer* entries than [`wrapped_line_count`]: at extreme widths
/// cosmic can lay out trailing line boxes that start no new character (they
/// paint nothing selectable), and the table stops at the last advancing one.
/// Consumers clamp their visual-row index into the table, so clicks on such
/// a phantom row resolve to the final real range.
pub fn wrapped_line_starts(
    content: &str,
    size: f32,
    font: Font,
    line_height: f32,
    max_width: f32,
) -> Vec<usize> {
    let line_height = line_height.max(1.0);
    let paragraph = wrapped_code_line(content, size, font, line_height, max_width);
    let lines = ((paragraph.min_bounds().height / line_height).round() as usize).max(1);
    let mut starts = Vec::with_capacity(lines);
    starts.push(0);
    for visual in 1..lines {
        let Some(hit) = paragraph.hit_test(iced::Point::new(
            0.0,
            visual as f32 * line_height + line_height / 2.0,
        )) else {
            break;
        };
        let byte = hit.cursor().min(content.len());
        let chars = content[..byte].chars().count();
        // A non-advancing hit means the remaining line boxes start no new
        // character — the table ends at the last real break (see the doc).
        if chars <= starts.last().copied().unwrap_or(0) {
            break;
        }
        starts.push(chars);
    }
    starts
}

fn one_line(content: &str, size: f32, font: Font, shaping: Shaping) -> Paragraph {
    let line_height = (size * LINE_HEIGHT_MULTIPLIER).max(1.0);
    Paragraph::with_text(Text {
        content,
        bounds: Size::new(f32::INFINITY, line_height),
        size: Pixels(size),
        line_height: text::LineHeight::Absolute(Pixels(line_height)),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping,
        wrapping: text::Wrapping::None,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: f32 = 12.0;
    const LINE_HEIGHT: f32 = 22.0;

    fn char_width() -> f32 {
        line_width("M", SIZE, Font::MONOSPACE).max(1.0)
    }

    fn starts(content: &str, width_chars: f32) -> Vec<usize> {
        wrapped_line_starts(
            content,
            SIZE,
            Font::MONOSPACE,
            LINE_HEIGHT,
            char_width() * width_chars,
        )
    }

    /// Word-first wrapping: breaks land after words (their trailing space
    /// hanging on the previous line), not mid-word at the width limit.
    #[test]
    fn wrap_breaks_at_word_boundaries() {
        // 7 chars fit: "aaa bbb" + hanging blank, then "ccc".
        assert_eq!(starts("aaa bbb ccc", 7.4), vec![0, 8]);
        // 5 chars fit: one word (+ hanging blank) per visual line.
        assert_eq!(starts("aaa bbb ccc", 5.4), vec![0, 4, 8]);
    }

    /// A word wider than the whole line falls back to glyph splitting —
    /// preserving it whole is impossible, so it breaks at the width.
    #[test]
    fn oversized_words_split_by_glyph() {
        assert_eq!(starts("abcdefghij", 4.4), vec![0, 4, 8]);
    }

    /// The two oracle views agree with each other and stay well-formed for
    /// arbitrary content across widths: counts match the break table, and
    /// the table is strictly increasing from zero. (Both are consumed by the
    /// diff view's height/hit-test/highlight math, which does exclusive
    /// range slicing over them.)
    #[test]
    fn starts_and_counts_agree_across_widths() {
        let corpus = [
            "fn main() { println!(\"hello, world\"); }".to_owned(),
            "    indented, with punctuation: foo_bar(baz)->quux[0] != <|>".to_owned(),
            "https://example.com/some/long/path?with=queries&and=params".to_owned(),
            "word ".repeat(30),
            "x".repeat(120),
            "short".to_owned(),
            "ends with spaces      ".to_owned(),
            "      leading spaces then words follow here".to_owned(),
        ];
        for content in corpus.iter().map(String::as_str) {
            for width_chars in 3..40 {
                let width = char_width() * width_chars as f32 + 0.4;
                let starts =
                    wrapped_line_starts(content, SIZE, Font::MONOSPACE, LINE_HEIGHT, width);
                let count = wrapped_line_count(content, SIZE, Font::MONOSPACE, LINE_HEIGHT, width);
                // The painted height can include trailing line boxes that
                // start no new character; the break table never exceeds it.
                assert!(
                    starts.len() <= count,
                    "break table larger than painted lines for {content:?} at \
                     {width_chars} chars: {starts:?} vs {count}"
                );
                assert_eq!(starts[0], 0);
                assert!(
                    starts.windows(2).all(|pair| pair[0] < pair[1]),
                    "non-monotonic starts for {content:?} at {width_chars} chars: {starts:?}"
                );
                let total = content.chars().count();
                assert!(
                    starts.last().copied().unwrap_or(0) <= total,
                    "start past the content for {content:?}: {starts:?}"
                );
            }
        }
    }
}
