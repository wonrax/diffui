//! One line of rich text cut to the width it gets, ending in an ellipsis.
//!
//! [`Line`] measures its spans with [`crate::measure`] and can say whether it
//! has to cut, so a row offers the rest only when there is more. The cut itself
//! is done here rather than by GPUI's `text_ellipsis` when a width is known, and
//! by GPUI when it isn't ([`Line::view`]).

use std::borrow::Cow;

use gpui_kit::prelude::*;
use gpui_kit::{Div, Hsla, SharedString, StyledText, TextRun, div, px};

use crate::theme::{Color, Font};
use crate::width_memo::WidthMemo;

/// How far over its width a line may measure and still count as fitting:
/// the spans are shaped one at a time and the sum can drift from the
/// shaped line by a fraction of a pixel.
const TOLERANCE: f32 = 0.5;

const ELLIPSIS: &str = "\u{2026}";

thread_local! {
    /// Shaped widths, shared by every line drawn on this thread: rows are
    /// rebuilt on every layout, their text mostly unchanged.
    static WIDTHS: WidthMemo = WidthMemo::default();
}

/// A run of a [`Line`] with its own font or colour.
#[derive(Debug, Clone)]
pub struct Span<'a> {
    pub text: Cow<'a, str>,
    pub font: Option<Font>,
    pub color: Option<Color>,
}

impl<'a> Span<'a> {
    pub fn new(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: text.into(),
            font: None,
            color: None,
        }
    }

    pub fn font(mut self, font: Font) -> Self {
        self.font = Some(font);
        self
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }
}

/// `spans` as one styled text in a base font and colour. Size, wrapping and
/// width are the container's.
pub fn rich_text(spans: &[Span<'_>], font: Font, color: Color) -> StyledText {
    let mut content = String::new();
    let mut runs = Vec::with_capacity(spans.len());
    for span in spans {
        content.push_str(&span.text);
        runs.push(TextRun {
            len: span.text.len(),
            font: span.font.unwrap_or(font).gpui(),
            color: Hsla::from(span.color.unwrap_or(color)),
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }
    runs.retain(|run| run.len > 0);
    StyledText::new(SharedString::from(content)).with_runs(runs)
}

/// Spans in a base font, size and colour, drawn on one line cut to fit, or
/// in full.
#[derive(Debug, Clone)]
pub struct Line<'a> {
    spans: Vec<Span<'a>>,
    font: Font,
    size: f32,
    color: Color,
}

impl<'a> Line<'a> {
    pub fn new(spans: Vec<Span<'a>>, font: Font, size: f32, color: Color) -> Self {
        Self {
            spans,
            font,
            size,
            color,
        }
    }

    /// Whether the text spans several lines of its own (newlines in it).
    pub fn is_multiline(&self) -> bool {
        self.spans
            .iter()
            .any(|span| span.text.contains(['\n', '\r']))
    }

    /// Whether it fits `width` on one line, uncut.
    pub fn fits(&self, width: f32) -> bool {
        self.cut_point(width).is_none()
    }

    /// Its width unwrapped: the widest of its own lines.
    pub fn natural_width(&self) -> f32 {
        let mut widest: f32 = 0.0;
        let mut line = 0.0;
        for span in &self.spans {
            let mut pieces = span.text.split('\n').peekable();
            while let Some(piece) = pieces.next() {
                line += self.measure(span, piece.trim_end_matches('\r'));
                if pieces.peek().is_some() {
                    widest = widest.max(line);
                    line = 0.0;
                }
            }
        }
        widest.max(line)
    }

    /// One line, newlines read as spaces, cut with an ellipsis where it
    /// doesn't fit `width`.
    pub fn cut(&self, width: f32) -> Div {
        let mut spans = self.single();
        if let Some((index, keep)) = self.cut_point(width) {
            spans.truncate(index + 1);
            let last = spans.last_mut().expect("the cut span");
            let kept = last.text[..keep].trim_end().to_owned();
            let mut ellipsis = last.clone();
            ellipsis.text = Cow::Borrowed(ELLIPSIS);
            last.text = Cow::Owned(kept);
            spans.push(ellipsis);
        }
        self.frame()
            .whitespace_nowrap()
            .child(rich_text(&spans, self.font, self.color))
    }

    /// The whole text, wrapping wherever it is laid out.
    pub fn wrapped(&self) -> Div {
        self.frame()
            .child(rich_text(&self.spans, self.font, self.color))
    }

    /// One line at whatever width it is given: it fills its row's free space
    /// and GPUI cuts it, ellipsis included.
    pub fn view(self) -> Div {
        self.frame()
            .w_full()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(rich_text(&self.single(), self.font, self.color))
    }

    fn frame(&self) -> Div {
        div()
            .font(self.font.gpui())
            .text_size(px(self.size))
            .text_color(self.color)
    }

    /// The spans with each newline (and the blank space around it) read as
    /// one space.
    fn single(&self) -> Vec<Span<'a>> {
        self.spans
            .iter()
            .map(|span| {
                let mut span = span.clone();
                if let Cow::Owned(text) = one_line(&span.text) {
                    span.text = Cow::Owned(text);
                }
                span
            })
            .collect()
    }

    fn measure(&self, span: &Span<'_>, content: &str) -> f32 {
        text_width(content, self.size, span.font.unwrap_or(self.font))
    }

    fn cut_point(&self, width: f32) -> Option<(usize, usize)> {
        let spans = self.single();
        let texts: Vec<&str> = spans.iter().map(|span| span.text.as_ref()).collect();
        cut_point(
            &texts,
            width,
            |index, content| self.measure(&spans[index], content),
            |index| self.measure(&spans[index], ELLIPSIS),
        )
    }
}

/// The width of one line of plain `content`, from the same memo the lines
/// measure with: for sizing what sits beside a [`Line`].
pub fn text_width(content: &str, size: f32, font: Font) -> f32 {
    WIDTHS.with(|widths| widths.width(content, size, font))
}

/// Plain `content` cut at its end so it and an ellipsis fit `width`; borrowed
/// back when it fits.
pub fn cut_end<'a>(content: &'a str, width: f32, size: f32, font: Font) -> Cow<'a, str> {
    let measure = |_: usize, part: &str| text_width(part, size, font);
    match cut_point(&[content], width, measure, |_| {
        text_width(ELLIPSIS, size, font)
    }) {
        None => Cow::Borrowed(content),
        Some((_, keep)) => Cow::Owned(format!("{}{ELLIPSIS}", content[..keep].trim_end())),
    }
}

/// Plain `content` cut at its start so an ellipsis and it fit `width`, keeping
/// its end (a path's nearest directories); borrowed back when it fits.
pub fn cut_start<'a>(content: &'a str, width: f32, size: f32, font: Font) -> Cow<'a, str> {
    start_cut(
        content,
        width,
        |part| text_width(part, size, font),
        text_width(ELLIPSIS, size, font),
    )
}

fn start_cut<'a>(
    content: &'a str,
    width: f32,
    measure: impl Fn(&str) -> f32,
    ellipsis: f32,
) -> Cow<'a, str> {
    if measure(content) <= width + TOLERANCE {
        return Cow::Borrowed(content);
    }
    let room = width + TOLERANCE - ellipsis;
    let starts: Vec<usize> = content
        .char_indices()
        .map(|(at, _)| at)
        .chain([content.len()])
        .collect();
    // The earliest start, on a char boundary, whose rest fits the room.
    let (mut low, mut high) = (0, starts.len() - 1);
    while low < high {
        let mid = (low + high) / 2;
        if measure(&content[starts[mid]..]) <= room {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    Cow::Owned(format!("{ELLIPSIS}{}", content[starts[low]..].trim_start()))
}

/// Plain `content` cut in the middle so it and an ellipsis fit `width`, keeping
/// both its start and its end (a file's extension, a compacted chain's leaf).
pub fn cut_middle<'a>(content: &'a str, width: f32, size: f32, font: Font) -> Cow<'a, str> {
    middle_cut(
        content,
        width,
        |part| text_width(part, size, font),
        text_width(ELLIPSIS, size, font),
    )
}

fn middle_cut<'a>(
    content: &'a str,
    width: f32,
    measure: impl Fn(&str) -> f32,
    ellipsis: f32,
) -> Cow<'a, str> {
    if measure(content) <= width + TOLERANCE {
        return Cow::Borrowed(content);
    }
    let room = width + TOLERANCE - ellipsis;
    let bytes: Vec<usize> = content
        .char_indices()
        .map(|(at, _)| at)
        .chain([content.len()])
        .collect();
    let chars = bytes.len() - 1;
    let head_of = |kept: usize| &content[..bytes[kept]];
    let tail_of = |kept: usize| &content[bytes[chars - kept]..];
    // Split by width rather than by chars, so a run of narrow letters (`file.txt`)
    // keeps what it has room for: the narrower side takes the next char, the start
    // on a tie, and the other side goes on when that one doesn't fit.
    let (mut head, mut tail) = (0, 0);
    let (mut head_width, mut tail_width) = (0.0, 0.0);
    while head + tail < chars {
        let grown_head = measure(head_of(head + 1));
        let grown_tail = measure(tail_of(tail + 1));
        let head_fits = grown_head + tail_width <= room;
        let tail_fits = head_width + grown_tail <= room;
        if head_fits && (head_width <= tail_width || !tail_fits) {
            head += 1;
            head_width = grown_head;
        } else if tail_fits {
            tail += 1;
            tail_width = grown_tail;
        } else {
            break;
        }
    }
    // A cut against an extension's dot reads as four dots (`fra….rs`): give the
    // tail the char before the dot, paying from the start if it has to.
    if tail_of(tail).starts_with('.') && tail + 1 + head <= chars {
        let grown_tail = measure(tail_of(tail + 1));
        let mut kept = head;
        while kept > 1 && measure(head_of(kept)) + grown_tail > room {
            kept -= 1;
        }
        if measure(head_of(kept)) + grown_tail <= room {
            head = kept;
            tail += 1;
        }
    }
    Cow::Owned(format!(
        "{}{ELLIPSIS}{}",
        head_of(head).trim_end_matches('.'),
        tail_of(tail)
    ))
}

/// `text` with each run of whitespace holding a line break turned into one
/// space; borrowed back when there is none.
fn one_line(text: &str) -> Cow<'_, str> {
    if !text.contains(['\n', '\r']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut pending = String::new();
    let mut broken = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending.push(c);
            broken |= matches!(c, '\n' | '\r');
            continue;
        }
        if broken {
            out.push(' ');
        } else {
            out.push_str(&pending);
        }
        pending.clear();
        broken = false;
        out.push(c);
    }
    if broken {
        out.push(' ');
    } else {
        out.push_str(&pending);
    }
    Cow::Owned(out)
}

/// Where a line of `parts` has to be cut so that it and an ellipsis after
/// it fit `width`: the part to end in and how many of its bytes to keep.
/// `None` when the whole line fits. `measure` is a part's (or a prefix of
/// it) width, `ellipsis` the width of an ellipsis in a part's style.
fn cut_point(
    parts: &[&str],
    width: f32,
    measure: impl Fn(usize, &str) -> f32,
    ellipsis: impl Fn(usize) -> f32,
) -> Option<(usize, usize)> {
    let widths: Vec<f32> = parts
        .iter()
        .enumerate()
        .map(|(index, part)| measure(index, part))
        .collect();
    if widths.iter().sum::<f32>() <= width + TOLERANCE {
        return None;
    }
    let mut used = 0.0;
    for (index, part) in parts.iter().enumerate() {
        let room = width + TOLERANCE - used - ellipsis(index);
        if widths[index] > room {
            // The longest prefix, on a char boundary, that fits the room.
            let ends: Vec<usize> = part
                .char_indices()
                .map(|(at, _)| at)
                .skip(1)
                .chain([part.len()])
                .collect();
            let (mut low, mut high) = (0, ends.len());
            while low < high {
                let mid = (low + high).div_ceil(2);
                if measure(index, &part[..ends[mid - 1]]) <= room {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            let keep = if low == 0 { 0 } else { ends[low - 1] };
            return Some((index, keep));
        }
        used += widths[index];
    }
    // Only an ellipsis wider than the last part's overflow lands here.
    parts
        .len()
        .checked_sub(1)
        .map(|last| (last, parts[last].len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every char is 1 wide, the ellipsis too.
    fn cut(parts: &[&str], width: f32) -> Option<(usize, usize)> {
        cut_point(parts, width, |_, text| text.chars().count() as f32, |_| 1.0)
    }

    #[test]
    fn a_line_that_fits_is_not_cut() {
        assert_eq!(cut(&["Move ", "main"], 9.0), None);
        assert_eq!(cut(&["Move ", "main"], 20.0), None);
        assert_eq!(cut(&[], 0.0), None);
    }

    #[test]
    fn the_cut_leaves_room_for_the_ellipsis() {
        // 9 wide in 8: keep 7 chars and the ellipsis.
        assert_eq!(cut(&["Move ", "main"], 8.0), Some((1, 2)));
        // The cut can land in the first part, or before all of a part.
        assert_eq!(cut(&["Move ", "main"], 4.0), Some((0, 3)));
        assert_eq!(cut(&["Move ", "main"], 6.0), Some((1, 0)));
        // Multi-byte chars are kept whole.
        assert_eq!(cut(&["a…b…c"], 3.0), Some((0, 4)));
    }

    #[test]
    fn spans_are_measured_in_their_own_font() {
        // The second part is twice as wide per char (a mono run).
        let widths = |index: usize, text: &str| text.chars().count() as f32 * (index + 1) as f32;
        assert_eq!(
            cut_point(&["ab", "cdef"], 7.0, widths, |_| 1.0),
            Some((1, 2))
        );
    }

    #[test]
    fn a_multi_line_command_draws_as_one_line() {
        // An op's recorded args: a description with a blank line in it.
        let args = "jj describe -r @ --message 'fix(x): y\n\nAlign z'";
        let line = Line::new(
            vec![
                Span::new("jj describe -r "),
                Span::new("@"),
                Span::new(&args[16..]),
            ],
            Font::MONOSPACE,
            11.0,
            crate::theme::BLACK,
        );
        assert!(line.is_multiline());
        let single: String = line
            .single()
            .iter()
            .map(|span| span.text.as_ref())
            .collect();
        assert_eq!(single, "jj describe -r @ --message 'fix(x): y Align z'");
        // One line is as wide as the folded text, and a narrow room cuts it.
        assert!(line.fits(10_000.0));
        assert!(!line.fits(20.0));
    }

    #[test]
    fn a_start_cut_keeps_the_end() {
        let count = |text: &str| text.chars().count() as f32;
        assert_eq!(start_cut("src/", 4.0, count, 1.0), "src/");
        assert_eq!(start_cut("abc/def/ghi/", 6.0, count, 1.0), "…/ghi/");
        assert_eq!(start_cut("abc/def/ghi/", 7.0, count, 1.0), "…f/ghi/");
        // No room for anything but the ellipsis.
        assert_eq!(start_cut("abcdefghij", 0.5, count, 1.0), "…");
        // Multi-byte chars stay whole.
        assert_eq!(start_cut("ééééé", 3.0, count, 1.0), "…éé");
    }

    #[test]
    fn a_middle_cut_keeps_both_ends() {
        let count = |text: &str| text.chars().count() as f32;
        assert_eq!(middle_cut("main.rs", 7.0, count, 1.0), "main.rs");
        assert_eq!(middle_cut("abcdefghij", 7.0, count, 1.0), "abc…hij");
        assert_eq!(middle_cut("abcdefghij", 6.0, count, 1.0), "abc…ij");
        // No room for anything but the ellipsis.
        assert_eq!(middle_cut("abcdefghij", 0.5, count, 1.0), "…");
        // Multi-byte chars stay whole.
        assert_eq!(middle_cut("ééééééééé", 4.0, count, 1.0), "éé…é");
    }

    #[test]
    fn a_middle_cut_stays_off_an_extensions_dot() {
        let count = |part: &str| part.chars().count() as f32;
        // Even split would give `abc….txt`.
        assert_eq!(middle_cut("abcdefgh.txt", 9.0, count, 1.0), "abc…h.txt");
        // A dot at the end of the start goes.
        assert_eq!(middle_cut("a.bcdefgh", 4.0, count, 1.0), "a…h");
    }

    #[test]
    fn a_middle_cut_splits_by_width() {
        // `i` is a third as wide as the rest: the narrow tail keeps more chars.
        let thin = |part: &str| part.chars().map(|c| if c == 'i' { 1.0 } else { 3.0 }).sum();
        assert_eq!(middle_cut("wwwwwwwwiiiiii", 15.0, thin, 3.0), "ww…iiiiii");
    }

    #[test]
    fn line_breaks_read_as_one_space() {
        assert_eq!(one_line("no breaks  here"), "no breaks  here");
        assert_eq!(
            one_line("'fix(x): y\n\n  Align a\nwith b'"),
            "'fix(x): y Align a with b'"
        );
        assert_eq!(one_line("end\r\n"), "end ");
    }
}
