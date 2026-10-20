//! One line of rich text cut to the width it gets, ending in an ellipsis.
//!
//! iced's rich text takes an `Ellipsis` but never applies it (only plain text
//! hands it to the shaper), so a line of mixed fonts, a title with a mono
//! bookmark in it or a highlighted command, ran on past its box. [`Line`]
//! measures its spans with [`crate::measure`] and cuts them itself, and can
//! say whether it had to, so a row offers the rest only when there is more.

use std::borrow::Cow;

use iced::widget::text::{Rich, Span, Wrapping};
use iced::widget::{responsive, text};
use iced::{Color, Element, Font, Length};

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

/// Spans in a base font, size and colour, drawn on one line cut to fit, or
/// in full.
#[derive(Clone)]
pub(crate) struct Line<'a> {
    spans: Vec<Span<'a>>,
    font: Font,
    size: f32,
    color: Color,
}

impl<'a> Line<'a> {
    pub(crate) fn new(spans: Vec<Span<'a>>, font: Font, size: f32, color: Color) -> Self {
        Self {
            spans,
            font,
            size,
            color,
        }
    }

    /// Whether the text spans several lines of its own (newlines in it).
    pub(crate) fn is_multiline(&self) -> bool {
        self.spans
            .iter()
            .any(|span| span.text.contains(['\n', '\r']))
    }

    /// Whether it fits `width` on one line, uncut.
    pub(crate) fn fits(&self, width: f32) -> bool {
        self.cut_point(width).is_none()
    }

    /// Its width unwrapped: the widest of its own lines.
    pub(crate) fn natural_width(&self) -> f32 {
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
    pub(crate) fn cut<Message: 'a>(&self, width: f32) -> Rich<'a, (), Message> {
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
        self.rich(spans).wrapping(Wrapping::None)
    }

    /// The whole text, wrapping wherever it is laid out.
    pub(crate) fn wrapped<Message: 'a>(&self) -> Rich<'a, (), Message> {
        self.rich(self.spans.clone())
            .wrapping(Wrapping::WordOrGlyph)
    }

    /// [`Line::cut`] at whatever width the line is given: it fills its
    /// row's free space.
    pub(crate) fn view<Message: 'a>(self) -> Element<'a, Message> {
        responsive(move |size| self.cut::<Message>(size.width))
            .height(Length::Shrink)
            .into()
    }

    fn rich<Message: 'a>(&self, spans: Vec<Span<'a>>) -> Rich<'a, (), Message> {
        text::Rich::with_spans(spans)
            .font(self.font)
            .size(self.size)
            .color(self.color)
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
        let font = span.font.unwrap_or(self.font);
        let size = span.size.map_or(self.size, |size| size.0);
        text_width(content, size, font)
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
pub(crate) fn text_width(content: &str, size: f32, font: Font) -> f32 {
    WIDTHS.with(|widths| widths.width(content, size, font))
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
            Color::BLACK,
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
    fn line_breaks_read_as_one_space() {
        assert_eq!(one_line("no breaks  here"), "no breaks  here");
        assert_eq!(
            one_line("'fix(x): y\n\n  Align a\nwith b'"),
            "'fix(x): y Align a with b'"
        );
        assert_eq!(one_line("end\r\n"), "end ");
    }
}
