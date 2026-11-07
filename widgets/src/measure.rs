//! One-line and wrapped text measurement, shared by chip sizing, sidebar path
//! truncation, menu layout, and the diff view's column metrics.
//!
//! These answers go through the same text system the renderer shapes with, so
//! measurements match painted pixels exactly. They're free functions because
//! much of the app measures in places with no `Window` at hand, so the module
//! holds on to the app's text system ([`install`]). In a test with no app it
//! makes its own.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use gpui_kit::{Hsla, Pixels, SharedString, Size, TextRun, TextSystem, WindowTextSystem, px, size};

use crate::theme::Font;

/// Line box used by single-line UI text throughout the app.
pub const LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

static TEXT_SYSTEM: OnceLock<Arc<TextSystem>> = OnceLock::new();

/// Measures with the app's text system, so fonts it loaded (Lucide) resolve.
/// Call once at startup, before anything measures.
pub fn install(system: Arc<TextSystem>) {
    let _ = TEXT_SYSTEM.set(system);
}

/// The text system measurements go through.
pub fn text_system() -> Arc<TextSystem> {
    TEXT_SYSTEM
        .get_or_init(|| Arc::new(TextSystem::new(platform_text_system())))
        .clone()
}

/// A fresh platform text system, without a platform. Building a whole platform
/// demands the main thread on macOS, which tests don't run on.
pub fn platform_text_system() -> Arc<dyn gpui_kit::PlatformTextSystem> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(gpui_apple::AppleTextSystem::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        gpui_kit::platform::current_platform(true).text_system()
    }
}

/// Measurements before the shaper is rebuilt. Its layout cache only drops
/// entries at a frame boundary, and this thread has no frames of its own.
const SHAPER_FRAME: u32 = 4096;

thread_local! {
    static SHAPER: RefCell<(Option<WindowTextSystem>, u32)> = const { RefCell::new((None, 0)) };
}

fn with_shaper<R>(f: impl FnOnce(&WindowTextSystem) -> R) -> R {
    SHAPER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let (shaper, count) = &mut *cell;
        *count += 1;
        if *count >= SHAPER_FRAME {
            *shaper = None;
            *count = 0;
        }
        f(shaper.get_or_insert_with(|| WindowTextSystem::new(text_system())))
    })
}

fn run(len: usize, font: Font) -> TextRun {
    TextRun {
        len,
        font: font.gpui(),
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Rendered width of one line of `content`.
pub fn line_width(content: &str, size: f32, font: Font) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    let width = with_shaper(|shaper| {
        shaper
            .layout_line(content, px(size), &[run(content.len(), font)], None)
            .width
    });
    f32::from(width)
}

/// Rendered extent (width × line box) of one line of `content`, for callers
/// sizing a box around the text, e.g. the sidebar's hover tooltip.
pub fn line_bounds(content: &str, text_size: f32, font: Font) -> Size<Pixels> {
    size(
        px(line_width(content, text_size, font)),
        px((text_size * LINE_HEIGHT_MULTIPLIER).max(1.0)),
    )
}

/// How many visual lines `content` occupies when word-wrapped at `max_width`
/// (1 for a line that fits). Engine truth: the same shaping the renderer uses,
/// so reserved heights match painted pixels.
pub fn wrapped_line_count(
    content: &str,
    size: f32,
    font: Font,
    line_height: f32,
    max_width: f32,
) -> usize {
    content
        .split('\n')
        .map(|line| wrapped_line_starts(line, size, font, line_height, max_width).len())
        .sum()
}

/// Byte offset at which each visual line of word-wrapped `content` starts:
/// index 0 is always present (and 0); one entry means no wrapping. The break
/// points come from the shaped line itself, so consumers slicing the line for
/// hit tests or highlight rects agree with the renderer byte-for-byte.
pub fn wrapped_line_starts(
    content: &str,
    size: f32,
    font: Font,
    _line_height: f32,
    max_width: f32,
) -> Vec<usize> {
    if content.is_empty() {
        return vec![0];
    }
    let one_line: String = content.replace('\n', " ");
    let lines = with_shaper(|shaper| {
        shaper.shape_text(
            SharedString::from(one_line),
            px(size),
            &[run(content.len(), font)],
            Some(px(max_width.max(1.0))),
            None,
        )
    });
    let Ok(lines) = lines else {
        return vec![0];
    };
    let mut starts = vec![0];
    if let Some(line) = lines.first() {
        for boundary in line.wrap_boundaries() {
            if let Some(glyph) = line
                .runs()
                .get(boundary.run_ix)
                .and_then(|run| run.glyphs.get(boundary.glyph_ix))
            {
                starts.push(glyph.index);
            }
        }
    }
    starts
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
        // GPUI doesn't let a trailing blank hang past the width: 8 chars fit
        // "aaa bbb " and then "ccc", while 7 send "bbb" down to join "ccc".
        assert_eq!(starts("aaa bbb ccc", 8.4), vec![0, 8]);
        assert_eq!(starts("aaa bbb ccc", 7.4), vec![0, 4]);
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
                assert!(
                    starts.last().copied().unwrap_or(0) <= content.len(),
                    "start past the content for {content:?}: {starts:?}"
                );
            }
        }
    }
}
