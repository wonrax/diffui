//! One logical line's layout at one wrap width: what the engine is handed (tabs
//! expanded), where it breaks, and how a source byte maps to a column and back.
//!
//! Everything that must land on the glyphs the painter drew reads its geometry
//! from here: the height index, hit testing, selection, find and emphasis
//! rectangles, the no-wrap horizontal extent. The painter also takes its break
//! points from here, shaping each visual line on its own. So a row's height and
//! its painted line count cannot disagree: there is one answer, not two
//! derivations that have to match. Under-counting was a real bug when the
//! renderer and the measurement each made up their own minds.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use icu_segmenter::LineSegmenter;
use icu_segmenter::options::LineBreakOptions;

use crate::measure;
use crate::theme::Font;

/// Columns between tab stops. CoreText snaps a tab to a fixed 28px stop, which
/// is neither eight columns nor anything a byte offset can be mapped through, so
/// tabs never reach it: they are expanded to spaces first.
pub(super) const TAB_COLUMNS: usize = 8;

/// Column budget for one non-ASCII char in [`column_bound`]. A monospace advance
/// is half an em to 0.6 em, so three columns buys at least 1.5 em, above the
/// widest glyph a fallback hands back for a single char (a full-width ideograph
/// is one em, a colour emoji around 1.3). The budget has to sit above them,
/// never below: see [`column_bound`].
pub(super) const MAX_COLUMNS_PER_CHAR: f32 = 3.0;

/// Columns `content` occupies unwrapped when the plain monospace grid can answer
/// on its own: printable ASCII advances exactly one column per byte, and a byte
/// is a char. Tabs and non-ASCII go to [`column_bound`].
pub(super) fn monospace_columns(content: &str) -> Option<f32> {
    content
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        .then_some(content.len() as f32)
}

/// Upper bound on the columns `content` occupies unwrapped, without asking the
/// text engine: exact for printable ASCII, tabs included.
///
/// The height index walks every line of the document, and all it needs from most
/// of them is whether they fit on one visual line. Shaping each one to find out
/// costs hundreds of milliseconds on a non-Latin diff, once per sidebar-drag
/// frame while wrapping. A bound that only ever errs high answers the question
/// outright: a line that clears the width cannot wrap, and one that doesn't goes
/// on to be measured for real.
///
/// Erring high is the whole safety property. Under-counting would reserve a row
/// too few, and the next row would paint over the overflow.
pub(super) fn column_bound(content: &str) -> f32 {
    if let Some(columns) = monospace_columns(content) {
        return columns;
    }
    let mut columns = 0usize;
    let mut extra = 0.0_f32;
    for ch in content.chars() {
        match ch {
            '\t' => columns = (columns / TAB_COLUMNS + 1) * TAB_COLUMNS,
            ch if ch.is_control() => {}
            ch if ch.is_ascii() => columns += 1,
            _ => {
                columns += 1;
                extra += MAX_COLUMNS_PER_CHAR - 1.0;
            }
        }
    }
    columns as f32 + extra
}

/// One place the display text differs from the source: a tab grown to spaces, or
/// a control character dropped.
#[derive(Debug, Clone, Copy)]
struct Edit {
    src: u32,
    src_len: u32,
    disp: u32,
    disp_len: u32,
}

/// `content` as the engine sees it, with the map back to source bytes.
#[derive(Debug, Clone)]
pub(super) struct Display {
    pub(super) text: String,
    edits: Vec<Edit>,
}

impl Display {
    /// `None` when the source can go to the engine as it is: the common case,
    /// with no allocation.
    pub(super) fn of(content: &str) -> Option<Self> {
        if !content.chars().any(char::is_control) {
            return None;
        }
        let mut text = String::with_capacity(content.len() + 16);
        let mut edits = Vec::new();
        let mut columns = 0usize;
        for (src, ch) in content.char_indices() {
            if ch == '\t' {
                let stop = (columns / TAB_COLUMNS + 1) * TAB_COLUMNS;
                let spaces = stop - columns;
                edits.push(Edit {
                    src: src as u32,
                    src_len: 1,
                    disp: text.len() as u32,
                    disp_len: spaces as u32,
                });
                text.extend(std::iter::repeat_n(' ', spaces));
                columns = stop;
            } else if ch.is_control() {
                edits.push(Edit {
                    src: src as u32,
                    src_len: ch.len_utf8() as u32,
                    disp: text.len() as u32,
                    disp_len: 0,
                });
            } else {
                text.push(ch);
                columns += 1;
            }
        }
        Some(Self { text, edits })
    }

    /// The source byte a display byte stands for. Every space of a tab stands
    /// for the tab.
    pub(super) fn src_of(&self, disp: usize) -> usize {
        let disp = disp as u32;
        let slot = self.edits.partition_point(|edit| edit.disp <= disp);
        // Dropped characters sit at the same display offset as what follows
        // them: the last edit at or before `disp` is the one to measure from.
        let Some(edit) = slot.checked_sub(1).map(|slot| self.edits[slot]) else {
            return disp as usize;
        };
        if disp < edit.disp + edit.disp_len {
            edit.src as usize
        } else {
            (edit.src + edit.src_len + (disp - edit.disp - edit.disp_len)) as usize
        }
    }

    /// The display byte a source byte lands on.
    pub(super) fn disp_of(&self, src: usize) -> usize {
        let src = src as u32;
        let slot = self.edits.partition_point(|edit| edit.src <= src);
        let Some(edit) = slot.checked_sub(1).map(|slot| self.edits[slot]) else {
            return src as usize;
        };
        if src < edit.src + edit.src_len {
            edit.disp as usize
        } else {
            (edit.disp + edit.disp_len + (src - edit.src - edit.src_len)) as usize
        }
    }
}

/// How a byte offset inside a visual line maps to its column.
#[derive(Debug)]
enum Columns {
    /// Printable ASCII without tabs: one byte, one char, one column. The byte
    /// offset inside a visual line *is* its column, so the common case needs
    /// neither a table nor a shaping pass.
    Monospace,
    /// `(source byte, column)` at every glyph boundary, per visual line, each
    /// line's table closed by a terminator at its trailing edge so a selection
    /// running to the end of a line has a column to stop at.
    Table(Vec<Vec<(u32, f32)>>),
}

/// What one measurement needs: the diff typography and the width a line wraps
/// at.
#[derive(Debug, Clone, Copy)]
pub(super) struct GeometryParams {
    pub(super) size: f32,
    pub(super) font: Font,
    pub(super) char_width: f32,
    /// The width one visual line may occupy, or `f32::INFINITY` with wrapping
    /// off, where the painter shapes the line whole and clips it, so the
    /// geometry has to see the whole line to scroll across it.
    pub(super) wrap_width: f32,
}

/// See the module docs. Columns are in units of the monospace advance, measured
/// from the start of the visual line the byte sits on, which is what the painter
/// offsets from.
#[derive(Debug)]
pub(super) struct LineGeometry {
    /// Source byte each visual line starts at. Always begins with 0; a single
    /// entry means the line doesn't wrap. Two can coincide when a forced break
    /// lands inside one tab's spaces.
    pub(super) starts: Vec<usize>,
    /// Where each visual line starts in the display text.
    pub(super) display_starts: Vec<usize>,
    /// Byte length of the line this was measured from.
    len: usize,
    columns: Columns,
    /// Widest visual line, in columns.
    pub(super) width_columns: f32,
    /// The text the painter shapes, when it isn't the source.
    pub(super) display: Option<Display>,
}

impl LineGeometry {
    /// Measure `content` the way the painter will draw it, for a line nothing keys:
    /// a revision header's field. A printable-ASCII one needs no shaping at all.
    pub(super) fn measure(content: &str, params: GeometryParams) -> Self {
        Self::from_shape(&LineShape::of(content, params), content.len(), params)
    }

    /// This line's break points and columns at one width.
    pub(super) fn from_shape(shape: &LineShape, len: usize, params: GeometryParams) -> Self {
        let display_starts = shape.wrap(params.wrap_width);
        let display = shape.display.as_ref();
        let text_len = display.map_or(len, |it| it.text.len());
        let visual_end =
            |visual: usize| display_starts.get(visual + 1).copied().unwrap_or(text_len);
        let src_of = |disp: usize| {
            if disp >= text_len {
                len
            } else {
                display.map_or(disp, |it| it.src_of(disp))
            }
        };
        let starts: Vec<usize> = display_starts.iter().map(|&start| src_of(start)).collect();
        let cw = params.char_width.max(1.0);
        let columns = match (&shape.glyphs, display) {
            // One byte, one column.
            (None, None) => Columns::Monospace,
            (glyphs, _) => {
                // The x a display byte sits at, along the unwrapped line.
                let x_of = |disp: usize| match glyphs {
                    None => disp as f32 * cw,
                    Some(glyphs) => {
                        let at = glyphs.partition_point(|&(index, _)| (index as usize) < disp);
                        glyphs.get(at).map_or(shape.width, |&(_, x)| x)
                    }
                };
                Columns::Table(
                    (0..display_starts.len())
                        .map(|visual| {
                            let (start, end) = (display_starts[visual], visual_end(visual));
                            let line_x = x_of(start);
                            let mut entries = Vec::new();
                            let mut last = None;
                            let mut push = |disp: usize| {
                                let src = src_of(disp);
                                if last != Some(src) {
                                    entries.push((src as u32, (x_of(disp) - line_x) / cw));
                                    last = Some(src);
                                }
                            };
                            match glyphs {
                                None => (start..end).for_each(&mut push),
                                Some(glyphs) => {
                                    let first =
                                        glyphs.partition_point(|&(i, _)| (i as usize) < start);
                                    let past = glyphs.partition_point(|&(i, _)| (i as usize) < end);
                                    glyphs[first..past]
                                        .iter()
                                        .for_each(|&(index, _)| push(index as usize));
                                }
                            }
                            entries.push((src_of(end) as u32, (x_of(end) - line_x) / cw));
                            entries
                        })
                        .collect(),
                )
            }
        };
        let width_columns = match &columns {
            Columns::Monospace => (0..display_starts.len())
                .map(|visual| (visual_end(visual) - display_starts[visual]) as f32)
                .fold(0.0, f32::max),
            Columns::Table(table) => table
                .iter()
                .filter_map(|entries| entries.last())
                .fold(0.0_f32, |widest, &(_, column)| widest.max(column)),
        };
        Self {
            starts,
            display_starts,
            len,
            columns,
            width_columns,
            display: display.cloned(),
        }
    }

    pub(super) fn visual_lines(&self) -> usize {
        self.starts.len()
    }

    /// Byte range of visual line `visual`. The end is the next line's start, so
    /// a word break's trailing blank hangs on the line it ends, or the line's
    /// own end for the last one.
    pub(super) fn line_range(&self, visual: usize) -> (usize, usize) {
        let start = self.starts.get(visual).copied().unwrap_or(self.len);
        let end = self
            .starts
            .get(visual + 1)
            .copied()
            .unwrap_or(self.len)
            .max(start);
        (start, end)
    }

    /// The visual line the byte at `byte` is painted on.
    pub(super) fn visual_line_of(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1)
    }

    /// Column of `byte`, measured from the start of visual line `visual`.
    pub(super) fn column_at(&self, visual: usize, byte: usize) -> f32 {
        let (start, end) = self.line_range(visual);
        let byte = byte.clamp(start, end);
        match &self.columns {
            Columns::Monospace => (byte - start) as f32,
            Columns::Table(table) => {
                let Some(entries) = table.get(visual) else {
                    return 0.0;
                };
                let at = entries
                    .partition_point(|&(boundary, _)| boundary as usize <= byte)
                    .saturating_sub(1);
                entries.get(at).map_or(0.0, |&(_, column)| column)
            }
        }
    }

    /// Byte offset nearest `column` on visual line `visual`: the inverse of
    /// [`Self::column_at`], rounded to the closest glyph boundary so a click past
    /// the middle of a glyph puts the caret after it.
    pub(super) fn byte_at(&self, visual: usize, column: f32) -> usize {
        let (start, end) = self.line_range(visual);
        match &self.columns {
            Columns::Monospace => {
                let offset = column.max(0.0).round() as usize;
                (start + offset).min(end)
            }
            Columns::Table(table) => {
                let Some(entries) = table.get(visual).filter(|entries| !entries.is_empty()) else {
                    return start;
                };
                let past = entries.partition_point(|&(_, boundary)| boundary <= column);
                let before = past.saturating_sub(1);
                let after = past.min(entries.len() - 1);
                let nearest = if column - entries[before].1 <= entries[after].1 - column {
                    before
                } else {
                    after
                };
                (entries[nearest].0 as usize).clamp(start, end)
            }
        }
    }
}

/// Where the first non-blank of each line-break segment falls: where a word-first wrap
/// may start a new line. The first segment is the line's own start, so it offers nothing.
/// `is_break` filters the segmenter's opportunities.
fn break_candidates(text: &str, is_break: impl Fn(usize) -> bool) -> Vec<u32> {
    let bytes = text.as_bytes();
    let mut candidates = Vec::new();
    let mut seen_word = false;
    let mut start = 0usize;
    let segmenter = LineSegmenter::new_for_non_complex_scripts(LineBreakOptions::default());
    let ends = segmenter
        .segment_str(text)
        .filter(|&end| end == text.len() || (end > 0 && is_break(end)));
    for end in ends {
        if let Some(first) = bytes[start..end].iter().position(|byte| *byte != b' ') {
            if seen_word {
                candidates.push((start + first) as u32);
            }
            seen_word = true;
        }
        start = end;
    }
    candidates
}

/// What a line is regardless of the width: its display text, where a word-first wrap
/// may break it, and, unless it's plain ASCII on the grid, where its glyphs sit. Measured
/// once per line, so a width change over long lines is arithmetic rather than a pass
/// through the segmenter and the shaper.
#[derive(Debug)]
pub(super) struct LineShape {
    display: Option<Display>,
    /// Display bytes a line may start at.
    candidates: Vec<u32>,
    /// `(display byte, x)` of each glyph along the unwrapped line, deduplicated by byte.
    /// `None` on the grid: x is the byte times the advance.
    glyphs: Option<Vec<(u32, f32)>>,
    /// Display bytes in the text.
    len: usize,
    /// The unwrapped width, px.
    width: f32,
    char_width: f32,
}

impl LineShape {
    pub(super) fn of(content: &str, params: GeometryParams) -> Self {
        let display = Display::of(content);
        let text = display.as_ref().map_or(content, |it| it.text.as_str());
        let cw = params.char_width.max(1.0);
        let wraps = params.wrap_width.is_finite();
        if text.is_ascii() {
            return Self {
                candidates: if wraps {
                    break_candidates(text, |_| true)
                } else {
                    Vec::new()
                },
                glyphs: None,
                len: text.len(),
                width: text.len() as f32 * cw,
                char_width: cw,
                display,
            };
        }
        let line = measure::shape_wrapped(text, params.size, params.font, None);
        let mut glyphs: Vec<(u32, f32)> = line.as_ref().map_or(Vec::new(), |line| {
            line.runs()
                .iter()
                .flat_map(|run| run.glyphs.iter())
                .map(|glyph| (glyph.index as u32, f32::from(glyph.position.x)))
                .collect()
        });
        glyphs.dedup_by_key(|glyph| glyph.0);
        let width = line.map_or(0.0, |line| f32::from(line.unwrapped_layout.width));
        // A face with broken metrics puts a glyph at infinity and every column after it;
        // nothing downstream can place a rectangle there, so fall back to the bound.
        if !width.is_finite() || glyphs.iter().any(|glyph| !glyph.1.is_finite()) {
            return Self {
                candidates: Vec::new(),
                glyphs: Some(Vec::new()),
                len: text.len(),
                width: column_bound(content) * cw,
                char_width: cw,
                display,
            };
        }
        Self {
            // Shaping can combine characters across a line break opportunity, so those
            // don't count.
            candidates: if wraps {
                break_candidates(text, |index| {
                    glyphs
                        .binary_search_by_key(&(index as u32), |glyph| glyph.0)
                        .is_ok()
                })
            } else {
                Vec::new()
            },
            glyphs: Some(glyphs),
            len: text.len(),
            width,
            char_width: cw,
            display,
        }
    }

    /// Where each visual line starts in the display text at `wrap_width`.
    ///
    /// GPUI's wrap rule (`LineLayout::compute_wrap_boundaries`), in `O(lines)`: when a
    /// glyph's right edge passes the width, the line breaks at the last word start since
    /// the previous break, or, with none, at that glyph. A trailing blank doesn't get to
    /// hang past the width. `geometry_starts_match_the_engine` pins it to the real thing.
    pub(super) fn wrap(&self, wrap_width: f32) -> Vec<usize> {
        let mut starts = vec![0usize];
        if !wrap_width.is_finite() || self.width <= wrap_width {
            return starts;
        }
        match &self.glyphs {
            None => {
                let cw = self.char_width;
                // Glyphs a line holds before the next one overflows.
                let mut fit = (wrap_width / cw).floor() as usize;
                while (fit + 1) as f32 * cw <= wrap_width {
                    fit += 1;
                }
                while fit > 0 && fit as f32 * cw > wrap_width {
                    fit -= 1;
                }
                let mut start = 0usize;
                loop {
                    // The first glyph over the edge; a line always holds at least one.
                    let over = start + fit.max(1);
                    if over >= self.len {
                        return starts;
                    }
                    start = self.next_start(start, over);
                    starts.push(start);
                }
            }
            Some(glyphs) => {
                let right = |at: usize| glyphs.get(at + 1).map_or(self.width, |glyph| glyph.1);
                let mut first = 0usize;
                loop {
                    let line_x = glyphs.get(first).map_or(0.0, |glyph| glyph.1);
                    // The first glyph whose right edge passes the width, past the line's
                    // first: right edges only grow along a left-to-right line.
                    let mut low = first + 1;
                    let mut high = glyphs.len();
                    while low < high {
                        let mid = (low + high) / 2;
                        if right(mid) - line_x > wrap_width {
                            high = mid;
                        } else {
                            low = mid + 1;
                        }
                    }
                    let Some(&(over, _)) = glyphs.get(low) else {
                        return starts;
                    };
                    let start = glyphs[first].0 as usize;
                    let next = self.next_start(start, over as usize);
                    starts.push(next);
                    first = glyphs.partition_point(|&(index, _)| (index as usize) < next);
                }
            }
        }
    }

    /// Where the next line starts, when the glyph at `over` overflows the line at `start`.
    fn next_start(&self, start: usize, over: usize) -> usize {
        let candidate = self.candidates.partition_point(|&at| at as usize <= over);
        candidate
            .checked_sub(1)
            .map(|slot| self.candidates[slot] as usize)
            .filter(|&at| at > start)
            .unwrap_or(over)
    }
}

/// Which diff row, at which wrap width, in which glyph advance (a font or size
/// change moves every column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct GeometryKey {
    pub(super) file_index: u32,
    pub(super) hunk_index: u32,
    pub(super) line_index: u32,
    pub(super) wrap_width_bits: u32,
    pub(super) char_width_bits: u32,
}

/// [`GeometryKey`] of a diff body line at `wrap_width`.
pub(super) fn body_geometry_key(
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    wrap_width: f32,
    char_width: f32,
) -> GeometryKey {
    GeometryKey {
        file_index: file_index as u32,
        hunk_index: hunk_index as u32,
        line_index: line_index as u32,
        wrap_width_bits: wrap_width.to_bits(),
        char_width_bits: char_width.to_bits(),
    }
}

/// Per-(line, width) [`LineGeometry`], held in the view's state so a visible
/// wrapped row is measured once per width rather than once per frame.
///
/// Bounded, because a height-index rebuild streams the whole document through it:
/// past [`GEOMETRY_MEMO_CAP`] entries it is dropped wholesale instead of growing
/// to the document's size. The cap is two orders of magnitude past a viewport's
/// worth of rows, so what is on screen survives the sweep and is re-measured at
/// most once after it.
#[derive(Default)]
pub(super) struct GeometryMemo {
    pub(super) lines: HashMap<GeometryKey, Rc<LineGeometry>>,
    /// Lines that didn't fit at some width: what's true of them at every width. Lives as
    /// long as the content does, because this is what makes the next width cheap.
    pub(super) shapes: HashMap<(u32, u32, u32), Rc<LineShape>>,
    /// [`column_bound`] of every line in document order, once per document, for the same
    /// reason: `bounds_key` names the document, `line_base` in the index walks it.
    pub(super) bounds: Vec<f32>,
    pub(super) bounds_key: Option<(u64, usize)>,
}

impl GeometryMemo {
    /// The content changed: nothing here says anything about it any more.
    pub(super) fn clear(&mut self) {
        self.lines.clear();
        self.shapes.clear();
        self.bounds.clear();
        self.bounds_key = None;
    }
}

const GEOMETRY_MEMO_CAP: usize = 4_096;
/// Lines kept measured: 12 MB or so of long non-Latin lines, far past any real diff's long
/// lines, and past it a width change just measures them again.
const SHAPE_MEMO_CAP: usize = 200_000;

pub(super) fn shape_of(
    memo: &RefCell<GeometryMemo>,
    key: GeometryKey,
    content: &str,
    params: GeometryParams,
) -> Rc<LineShape> {
    let id = (key.file_index, key.hunk_index, key.line_index);
    if let Some(hit) = memo.borrow().shapes.get(&id) {
        return Rc::clone(hit);
    }
    // Measured as though it wraps, so a no-wrap frame's measurement serves the next
    // wrapped one.
    let shape = Rc::new(LineShape::of(
        content,
        GeometryParams {
            wrap_width: 1.0,
            ..params
        },
    ));
    let mut memo = memo.borrow_mut();
    if memo.shapes.len() >= SHAPE_MEMO_CAP {
        memo.shapes.clear();
    }
    memo.shapes.insert(id, Rc::clone(&shape));
    shape
}

pub(super) fn memoized(
    memo: &RefCell<GeometryMemo>,
    key: GeometryKey,
    measure: impl FnOnce() -> LineGeometry,
) -> Rc<LineGeometry> {
    if let Some(hit) = memo.borrow().lines.get(&key) {
        return Rc::clone(hit);
    }
    let geometry = Rc::new(measure());
    let mut memo = memo.borrow_mut();
    if memo.lines.len() >= GEOMETRY_MEMO_CAP {
        memo.lines.clear();
    }
    memo.lines.insert(key, Rc::clone(&geometry));
    geometry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_expands_to_its_stop_and_maps_back_to_itself() {
        let display = Display::of("\tab\tc").unwrap();
        assert_eq!(display.text, "        ab      c");
        // Every space of the first tab stands for byte 0.
        assert_eq!(display.src_of(0), 0);
        assert_eq!(display.src_of(7), 0);
        assert_eq!(display.src_of(8), 1);
        assert_eq!(display.src_of(9), 2);
        assert_eq!(display.src_of(10), 3);
        assert_eq!(display.src_of(16), 4);
        assert_eq!(display.disp_of(1), 8);
        assert_eq!(display.disp_of(3), 10);
        assert_eq!(display.disp_of(4), 16);
        assert_eq!(display.disp_of(5), 17);
        assert!(Display::of("no tabs here").is_none());
    }

    #[test]
    fn a_carriage_return_is_not_shown_and_keeps_the_map_straight() {
        let display = Display::of("a\tb\r").unwrap();
        assert_eq!(display.text, "a       b");
        assert_eq!(display.src_of(8), 2);
        assert_eq!(display.disp_of(2), 8);
        assert_eq!(display.disp_of(3), 9);
        assert_eq!(display.src_of(9), 4);
    }
}
