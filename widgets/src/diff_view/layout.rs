//! Where everything goes: the metrics, the height index, and the hit tests. All
//! of it pure, against a [`View`] of the props and the document, so it runs and
//! tests without a window.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::App;

use super::line::{
    GeometryKey, GeometryMemo, GeometryParams, LineGeometry, body_geometry_key, column_bound,
    memoized, shape_of,
};
use super::{
    AnnotationLabel, CheckState, DiffFileView, DiffHunkView, DiffLine, DiffLineKind, DiffProps,
    FindMatch, HEADER_HORIZONTAL_PADDING, HEADER_VERTICAL_PADDING, HeaderAction, HeaderField,
    HeaderLine, HunkMark, HunkMarks, LineAt, LineMode, media,
};
use crate::chip;
use crate::geometry::{Point, Rectangle};
use crate::hunk_actions::{self, FilePlaced, Placed};
use crate::measure;
use crate::scrollbar;
use crate::theme::Font;
use crate::typography::{CodeTypography, TextBox};

// Padding above and below the centered title row inside the file-header strip.
pub(super) const FILE_HEADER_VPAD: f32 = 8.0;
// Padding above and below the centered title row inside the hunk-header strip.
pub(super) const HUNK_HEADER_VPAD: f32 = 3.0;
/// Padding above and below a file note's line.
const NOTE_VPAD: f32 = 12.0;
/// A file note's inset from the pane's left edge, and its button's size.
pub(super) const NOTE_INSET: f32 = 20.0;
pub(super) const NOTE_BUTTON_HEIGHT: f32 = 24.0;
pub(super) const NOTE_BUTTON_PAD_X: f32 = 10.0;
/// The size of a note's prose.
pub(super) const NOTE_TEXT_SIZE: f32 = crate::theme::text_size::BODY;
pub(super) const PREFIX_WIDTH: f32 = 24.0;
pub(super) const TEXT_X_PADDING: f32 = 8.0;
/// A plain document's inset of its code from the gutter's edge and from the
/// top, to the glyphs: it has no prefix column to clear.
pub(super) const PLAIN_INSET: f32 = 12.0;
// Floor for a plain document's gutter so a single-digit column still looks
// intentional.
const GUTTER_MIN_WIDTH: f32 = 28.0;
// Padding flanking the gutter text on both sides.
pub(super) const GUTTER_HORIZONTAL_PADDING: f32 = 8.0;
/// A diff's line-number column: never narrower than this, and the space right
/// of its right-aligned numbers and left of the widest one.
pub(super) const NUMBER_COLUMN_MIN: f32 = 44.0;
pub(super) const NUMBER_PAD_RIGHT: f32 = 10.0;
const NUMBER_PAD_LEFT: f32 = 6.0;
// Space drawn between the label column and the value column.
pub(super) const HEADER_LABEL_GAP: f32 = 8.0;
/// Gap between a rich field's value, its badges and its buttons.
pub(super) const HEADER_CHIP_GAP: f32 = 6.0;
/// Side of a rich field's icon buttons (copy).
pub(super) const HEADER_BUTTON_SIZE: f32 = 20.0;
// Description lines lead the header as its title, flush with the labels.
const HEADER_DESCRIPTION_INDENT: f32 = 0.0;
pub(super) const HEADER_EDIT_WIDTH: f32 = 66.0;
pub(super) const HEADER_EDIT_HEIGHT: f32 = 26.0;
/// Side of a change-selection checkbox.
pub(super) const CHECK_SIZE: f32 = 14.0;
/// A file header: never shorter than this, its insets, the gap between its
/// pieces, the side of its icon buttons, and the gaps around its counts.
pub(super) const FILE_HEADER_MIN_HEIGHT: f32 = 38.0;
const FILE_HEADER_PAD_LEFT: f32 = 8.0;
/// The file controls' glyphs end on the pane's trailing edge,
/// [`HEADER_HORIZONTAL_PADDING`] in, as the commit bar's do.
const FILE_HEADER_PAD_RIGHT: f32 =
    HEADER_HORIZONTAL_PADDING - (FILE_CONTROL_SIZE - super::paint::BROWSE_ICON_SIZE) / 2.0;
const FILE_HEADER_GAP: f32 = 8.0;
const FILE_CONTROL_SIZE: f32 = 22.0;
const FILE_HEADER_META_GAP: f32 = 10.0;
const FILE_HEADER_STAT_GAP: f32 = 6.0;

/// A file header's controls (see [`View::file_header_layout`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileControl {
    Fold,
    Check,
    Browse,
    More,
}

/// What a file header count is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatKind {
    Added,
    Deleted,
    /// The label standing in for counts ("binary").
    Tag,
}

/// A file header laid out: see [`View::file_header_layout`].
#[derive(Debug, Clone)]
pub(super) struct FileHeaderLayout {
    pub(super) fold: Rectangle,
    pub(super) check: Option<Rectangle>,
    pub(super) badge: Rectangle,
    /// The path runs from here to `title_right`, ellipsised to fit.
    pub(super) title_x: f32,
    pub(super) title_right: f32,
    /// Top of the path's line box (a code row's), and its baseline, which every
    /// text piece of the header sits on.
    pub(super) line_top: f32,
    pub(super) baseline: f32,
    /// "3 hunks" and its left edge.
    pub(super) hunks: Option<(String, f32)>,
    /// The counts (or the tag) and their left edges.
    pub(super) stats: Vec<(String, f32, StatKind)>,
    pub(super) browse: Option<Rectangle>,
    pub(super) more: Option<Rectangle>,
}

/// Sizes derived from the caller's typography, so the diff view stays visually
/// proportional under a non-default code font size.
#[derive(Debug, Clone, Copy)]
pub(super) struct LayoutMetrics {
    pub(super) row_height: f32,
    /// Vertical offset of a row's text line box below its top edge: the shift
    /// that centres the code font's caps in the row (see
    /// [`TextBox::trim_shift`]), plus the user's `code_baseline_offset`.
    pub(super) text_y_pad: f32,
    pub(super) file_header_height: f32,
    pub(super) hunk_header_height: f32,
    /// A file note's band (see [`super::FileNote`]).
    pub(super) note_height: f32,
    /// The `+`/`-` column between the gutter and the code; narrower in a plain
    /// document, which has none to draw.
    pub(super) prefix_width: f32,
    /// Space above the first row: a plain document's top inset.
    pub(super) top_inset: f32,
    pub(super) gutter_width: f32,
    /// One line-number column of a diff: the gutter holds two (old, new) in
    /// unified, one per side in split.
    pub(super) number_column: f32,
    pub(super) gutter_digit_count: usize,
    /// Real monospace advance for the diff font + size. Wrap math and selection
    /// geometry both rely on it.
    pub(super) char_width: f32,
}

impl LayoutMetrics {
    pub(super) fn new(
        typography: CodeTypography,
        gutter_digit_count: usize,
        font: Font,
        plain: bool,
    ) -> Self {
        let text_size = typography.size;
        let row_height = (text_size * typography.line_height).round();
        // The line box is the row; the engine centres ascent + descent in it,
        // and this moves the caps to the middle instead, from the font's own
        // metrics. Any line height gets the same treatment.
        let text_y_pad =
            TextBox::new(font, text_size, row_height).trim_shift() + typography.baseline_offset;
        // Plain (source-browse) documents render no file/hunk header strips;
        // zero heights keep the prefix-sum index, hit tests, and draw all
        // consistent without a parallel layout mode.
        let (file_header_height, hunk_header_height) = if plain {
            (0.0, 0.0)
        } else {
            (
                (row_height + 2.0 * FILE_HEADER_VPAD).max(FILE_HEADER_MIN_HEIGHT),
                row_height + 2.0 * HUNK_HEADER_VPAD,
            )
        };
        let char_width = char_advance(font, text_size).max(1.0);
        // Two line-number columns, old and new; plain documents have no old
        // side, so a single column suffices.
        let number_column =
            (gutter_digit_count as f32 * char_width + NUMBER_PAD_LEFT + NUMBER_PAD_RIGHT)
                .max(NUMBER_COLUMN_MIN)
                .round();
        let gutter_width = if plain {
            (gutter_digit_count as f32 * char_width + GUTTER_HORIZONTAL_PADDING * 2.0)
                .max(GUTTER_MIN_WIDTH)
        } else {
            2.0 * number_column
        };
        // Inset to the caps: the row already holds half its leading above them.
        let (prefix_width, top_inset) = if plain {
            let caps = TextBox::new(font, text_size, row_height).cap_height();
            (
                PLAIN_INSET - TEXT_X_PADDING,
                (PLAIN_INSET - (row_height - caps) / 2.0).max(0.0),
            )
        } else {
            (PREFIX_WIDTH, 0.0)
        };
        Self {
            row_height,
            text_y_pad,
            file_header_height,
            hunk_header_height,
            note_height: row_height + 2.0 * NOTE_VPAD,
            prefix_width,
            top_inset,
            gutter_width,
            number_column,
            gutter_digit_count,
            char_width,
        }
    }
}

/// One glyph's advance in the code font, read once per (font, size): this runs
/// for every view of the document.
fn char_advance(font: Font, size: f32) -> f32 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(Font, u32), f32>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cache
        .entry((font, size.to_bits()))
        // "M" is a stable choice: it dominates hinting noise at small sizes.
        .or_insert_with(|| measure::line_width("M", size, font))
}

/// The shape of [`HeightIndex::key`].
pub(super) type HeightKey = (u64, usize, usize, u32, bool, bool, Option<u64>, u64);

/// `pair_lines` sentinel: this side of the row has no line (padding).
pub(super) const NO_LINE: u32 = u32::MAX;

/// [`HeightIndex::key`] width component for a layout that doesn't depend on the
/// pane width (no-wrap mode).
pub(super) const WIDTH_AGNOSTIC: u32 = u32::MAX;

/// Prefix-sum layout index over the rendered document: the "position checkpoint"
/// idea from pierre.computer's diff-rendering writeup. Built once per
/// `(content shape, wrap width)` and consulted by every height/position query, so
/// scroll frames, file jumps, and hit tests are O(log n) binary searches instead
/// of O(total lines) walks.
///
/// ~16 bytes per row: ~16 MB at 1M lines, an order of magnitude under the lines
/// themselves.
#[derive(Debug, Default)]
pub(super) struct HeightIndex {
    /// Rebuild key: `(layout_version, files.len(), header height bits, wrap width
    /// bits, wrap, side_by_side, media key, file strips key)`. The file/header
    /// counts are part of the key because streaming PR loads *append* files (and
    /// the header appears when `gh pr view` lands) without replacing the document.
    /// Keyed on the layout id, NOT the paint `content_version`: highlight merges
    /// bump the latter to re-shape paint without moving anything. The wrap and
    /// side-by-side flags are in the key because toggling either moves every row.
    ///
    /// The width component is [`WIDTH_AGNOSTIC`] with wrapping off: every row is
    /// then exactly one line tall no matter how wide the pane is, so a sidebar
    /// drag must not spend an O(document) rebuild per frame proving it.
    pub(super) key: Option<HeightKey>,
    /// Content-space y of each file's header top, plus one trailing sentinel
    /// holding the content end. `file_tops[0]` equals the revision-header height.
    pub(super) file_tops: Vec<f32>,
    /// Content-space y of each hunk header, in document order.
    pub(super) hunk_tops: Vec<f32>,
    /// `(file, hunk)` of each `hunk_tops` entry.
    pub(super) hunk_ids: Vec<(u32, u32)>,
    /// Content-space y of each diff row, in document order.
    pub(super) row_tops: Vec<f32>,
    /// Height of each `row_tops` entry. Stored rather than re-derived: the
    /// painter asks for every visible row's height on every frame.
    pub(super) row_heights: Vec<f32>,
    /// `(file, hunk, line)` of each `row_tops` entry. Document order makes this
    /// lexicographically sorted, so a row is also findable *by id*. In
    /// side-by-side mode the line component is the pair's *first* member (which
    /// preserves the sort); the full pair lives in `pair_lines`.
    pub(super) row_ids: Vec<(u32, u32, u32)>,
    /// Side-by-side only (empty in unified mode): each row's `(left, right)` line
    /// indices into its hunk, [`NO_LINE`] for a padded side. Context / note rows
    /// carry the same line on both sides; rows of full-width files carry
    /// `(line, line)` purely to keep this aligned with `row_tops`.
    pub(super) pair_lines: Vec<(u32, u32)>,
    /// Side-by-side only: files rendered as a single full-width column even in
    /// split mode. A single-sided file (all additions or all deletions) has
    /// nothing to mirror, so splitting it would waste half the pane on padding.
    pub(super) full_width_files: Vec<bool>,
    /// Widest full-width line of the document in columns (every line in unified
    /// mode; a one-sided file's in split): what the no-wrap mode can scroll across
    /// the whole text area. Left at zero while wrapping.
    pub(super) max_columns: f32,
    /// Widest line of a split file in columns, scrolled across one column's text
    /// area. Left at zero while wrapping or unified.
    pub(super) max_split_columns: f32,
    /// Lines whose width the two maxima above don't know yet: non-ASCII ones,
    /// which [`column_bound`] only bounds. `(bound, in a split file, key)`, shaped
    /// for real by [`View::settle_extent`] instead of during the rebuild.
    pub(super) unmeasured_wide: Vec<(f32, bool, GeometryKey)>,
    /// Total content height (revision header + every file).
    pub(super) total_height: f32,
    /// `(file, top, height)` of each image preview block, in document order.
    pub(super) media_bands: Vec<(usize, f32, f32)>,
    /// `(file, top)` of each file strip, in document order.
    pub(super) file_strips: Vec<(usize, f32)>,
    /// `(file, top)` of each file note, in document order.
    pub(super) note_bands: Vec<(usize, f32)>,
}

impl HeightIndex {
    /// True when `file` renders as one full-width column despite split mode.
    pub(super) fn is_full_width(&self, file: usize) -> bool {
        self.full_width_files.get(file).copied().unwrap_or(false)
    }

    /// Index into `row_tops`/`row_ids` of the last row starting at or above
    /// `target_y`: the candidate row containing that y (the caller checks the
    /// row's actual height; `target_y` may sit in a header band).
    pub(super) fn row_at(&self, target_y: f32) -> Option<usize> {
        self.row_tops
            .partition_point(|&top| top <= target_y)
            .checked_sub(1)
    }

    /// For a `target_y` between rows (a file or hunk header band, a media block)
    /// or above the first one: the nearer row, and whether it lies above `target_y`.
    /// `None` on a row and past the last one.
    pub(super) fn nearest_row_off_band(&self, target_y: f32) -> Option<(usize, bool)> {
        let above = self.row_at(target_y);
        let below = above.map_or(0, |row| row + 1);
        let below_top = *self.row_tops.get(below)?;
        let Some(above) = above else {
            return Some((below, false));
        };
        let above_bottom = self.row_tops[above] + self.row_heights[above];
        if target_y < above_bottom {
            return None;
        }
        Some(if target_y - above_bottom <= below_top - target_y {
            (above, true)
        } else {
            (below, false)
        })
    }

    /// Index of row `(file, hunk, line)`, by binary search over the sorted ids.
    pub(super) fn row_index_of(&self, file: usize, hunk: usize, line: usize) -> Option<usize> {
        self.row_ids
            .binary_search(&(file as u32, hunk as u32, line as u32))
            .ok()
    }

    /// Index of the row *containing* `(file, hunk, line)` on either side. In
    /// unified mode this is [`Self::row_index_of`]; in side-by-side a right
    /// member's row is keyed by its left partner, so walk back from the last row
    /// at or before the id until a pair carries the line. The walk is bounded by
    /// one change run (an addition can only pair leftward within its own run).
    pub(super) fn row_of_line(&self, file: usize, hunk: usize, line: usize) -> Option<usize> {
        if self.pair_lines.is_empty() {
            return self.row_index_of(file, hunk, line);
        }
        let target = (file as u32, hunk as u32, line as u32);
        let mut row = self
            .row_ids
            .partition_point(|id| *id <= target)
            .checked_sub(1)?;
        loop {
            let (row_file, row_hunk, _) = *self.row_ids.get(row)?;
            if (row_file, row_hunk) != (file as u32, hunk as u32) {
                return None;
            }
            let &(left, right) = self.pair_lines.get(row)?;
            if left == line as u32 || right == line as u32 {
                return Some(row);
            }
            row = row.checked_sub(1)?;
        }
    }
}

/// Stable cursor position inside the diff document. We index by
/// `(file, hunk, line)` instead of by screen y so the position stays valid across
/// scrolling, file selection, and re-renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TextPosition {
    /// Ordered first so every header position sorts before every body position:
    /// the revision header sits above the diff body.
    pub(super) region: Region,
    /// `Body`: the file index. Unused (`0`) for `Header`.
    pub(super) file_index: usize,
    /// `Body`: the hunk index. Unused (`0`) for `Header`.
    pub(super) hunk_index: usize,
    /// `Body`: the line within the hunk. `Header`: the header-line index.
    pub(super) line_index: usize,
    /// Byte offset within the line's selectable text. Bytes (not chars) so we can
    /// slice the source string directly when copying.
    pub(super) byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Region {
    Header,
    Body,
}

pub(super) fn header_position(line_index: usize, byte: usize) -> TextPosition {
    TextPosition {
        region: Region::Header,
        file_index: 0,
        hunk_index: 0,
        line_index,
        byte,
    }
}

/// The member of a split row's `(left, right)` pair a point resolves to: the side
/// asked for, falling back to the populated side of a padded pair.
fn split_member(left: u32, right: u32, in_right: bool) -> (u32, SplitSide) {
    let pick_right = if in_right {
        right != NO_LINE || left == NO_LINE
    } else {
        left == NO_LINE
    };
    if pick_right {
        (right, SplitSide::Right)
    } else {
        (left, SplitSide::Left)
    }
}

pub(super) fn body_position(
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    byte: usize,
) -> TextPosition {
    TextPosition {
        region: Region::Body,
        file_index,
        hunk_index,
        line_index,
        byte,
    }
}

pub(super) fn ordered(a: TextPosition, b: TextPosition) -> (TextPosition, TextPosition) {
    if a <= b { (a, b) } else { (b, a) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionUnit {
    Character,
    Word,
    Line,
}

/// One column of the side-by-side layout, or the whole text area in unified mode
/// (`None`). Selection/find/emphasis passes iterate per lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SplitSide {
    Left,
    Right,
}

/// A strip button: `(row, button)` of a hunk strip, or `(strip, button)` of a file
/// strip (see [`hunk_actions::HunkActions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionHit {
    Hunk(usize, usize),
    File(usize, usize),
}

/// A change-selection checkbox under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckHit {
    File(usize),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct VisibleRow {
    pub(super) file_index: usize,
    pub(super) hunk_index: usize,
    pub(super) line_index: usize,
    /// The row's `(left, right)` pair ([`NO_LINE`] for a padded side) when it
    /// renders as two split columns; `None` when it renders full-width: unified
    /// mode, or a full-width (single-sided) file in split mode.
    pub(super) pair: Option<(u32, u32)>,
    pub(super) y: f32,
    pub(super) height: f32,
}

impl VisibleRow {
    /// The row's line shown in `lane`, or `None` when that side is padding or the
    /// row doesn't participate in the lane at all (full-width rows live only in
    /// the `None` lane; split rows only in the column lanes). Context (and note)
    /// rows appear in both column lanes.
    pub(super) fn line_in_lane(&self, lane: Option<SplitSide>) -> Option<usize> {
        match (lane, self.pair) {
            (None, None) => Some(self.line_index),
            (Some(SplitSide::Left), Some((left, _))) if left != NO_LINE => Some(left as usize),
            (Some(SplitSide::Right), Some((_, right))) if right != NO_LINE => Some(right as usize),
            _ => None,
        }
    }

    /// Lowest and highest line index this row shows. A split row's two members
    /// straddle a range of the hunk's lines, so bracketing a document-ordered list
    /// against the visible window needs both ends.
    pub(super) fn line_bounds(&self) -> (usize, usize) {
        match self.pair {
            None => (self.line_index, self.line_index),
            Some((left, right)) => {
                let low = left.min(right) as usize;
                let high = if right == NO_LINE { left } else { right } as usize;
                (low.min(high), high)
            }
        }
    }
}

/// Per-column x-offsets of the side-by-side layout, relative to the pane's left
/// edge. See [`View::split_layout`]. (The left gutter sits at 0.)
pub(super) struct SplitLayout {
    pub(super) gutter_width: f32,
    pub(super) text_width: f32,
    pub(super) left_text_x: f32,
    pub(super) divider_x: f32,
    pub(super) right_gutter_x: f32,
    pub(super) right_text_x: f32,
}

/// The run of lines from one commit (`start..end`, 0-based) that `line` belongs
/// to, when its entry is a link: its commit is still visible.
pub(super) fn annotation_run(labels: &[AnnotationLabel], line: usize) -> Option<(usize, usize)> {
    if labels.get(line)?.hidden {
        return None;
    }
    let start = labels[..=line]
        .iter()
        .rposition(|label| !label.id.is_empty())?;
    let end = labels[line + 1..]
        .iter()
        .position(|label| !label.id.is_empty())
        .map_or(labels.len(), |after| line + 1 + after);
    Some((start, end))
}

/// A find match's position in document order.
pub(super) fn match_id(m: &FindMatch) -> (usize, usize, usize) {
    (m.file_index, m.hunk_index, m.line_index)
}

/// The slice of document-ordered `matches` that can land on the lines
/// `first..=last`: the visible window, found by two binary searches instead of a
/// scan over every match in the diff.
pub(super) fn matches_between(
    matches: &[FindMatch],
    first: (usize, usize, usize),
    last: (usize, usize, usize),
) -> std::ops::Range<usize> {
    let start = matches.partition_point(|m| match_id(m) < first);
    let end = matches.partition_point(|m| match_id(m) <= last);
    start..end.max(start)
}

/// The run of `matches` sitting on exactly `line`, and where in `matches` it
/// starts (the caller needs the index back to spot the active match).
pub(super) fn matches_on_line(
    matches: &[FindMatch],
    line: (usize, usize, usize),
) -> (usize, &[FindMatch]) {
    let start = matches.partition_point(|m| match_id(m) < line);
    let end = start + matches[start..].partition_point(|m| match_id(m) == line);
    (start, &matches[start..end])
}

/// Maximum line-number digit count across all visible files. Used to size the
/// gutter just wide enough for the largest line number plus a small padding, so a
/// 30k-line file isn't truncated and a 50-line file doesn't waste a third of the
/// viewport on whitespace.
pub(super) fn compute_gutter_digit_count(files: &[&[DiffHunkView]]) -> usize {
    // Must not walk the whole diff. Line numbers grow monotonically through a
    // file's hunks, so each file's maximum lives at the *tail of its last hunk*:
    // scan that tail backwards until both counters have been seen. The cap bounds
    // the pathological single-sided hunk (e.g. a 1M-line pure addition never
    // carries an old number); past it the other counter is at most one digit off
    // anyway, and the found side dominates the gutter width.
    const TAIL_SCAN_CAP: usize = 4_096;
    let mut max_line = 0usize;
    for hunks in files {
        let Some(hunk) = hunks.last() else {
            continue;
        };
        let mut last_old = None;
        let mut last_new = None;
        for line in hunk.lines.iter().rev().take(TAIL_SCAN_CAP) {
            if last_old.is_none() {
                last_old = line.old_line;
            }
            if last_new.is_none() {
                last_new = line.new_line;
            }
            if last_old.is_some() && last_new.is_some() {
                break;
            }
        }
        max_line = max_line
            .max(last_old.unwrap_or(0))
            .max(last_new.unwrap_or(0));
    }
    // Floor at 3 so the gutter still feels like a column for tiny files.
    digits(max_line).max(3)
}

fn digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut value = n;
    while value > 0 {
        count += 1;
        value /= 10;
    }
    count
}

/// Find the `[start, end)` byte range of the word the cursor is currently inside.
/// "Word" is the run of characters with the same `word_class` as the character
/// under the cursor, where word_class folds identifier chars (alphanumeric + `_`)
/// into one bucket, whitespace into another, and everything else (punctuation,
/// operators) into a third. Clicking on whitespace selects that whitespace run;
/// clicking on punctuation selects that punctuation run; clicking on a letter
/// selects the identifier: the convention every text editor uses.
pub(super) fn word_bounds(content: &str, byte_pos: usize) -> (usize, usize) {
    if content.is_empty() {
        return (0, 0);
    }
    let byte_pos = byte_pos.min(content.len());

    // Anchor on the char under the cursor: floor the click to a char boundary (it
    // may land mid-glyph), stepping back onto the last char when the cursor sits at
    // the very end so double-clicking past the last word still selects it.
    let mut anchor_off = byte_pos;
    if anchor_off == content.len() {
        anchor_off -= 1;
    }
    while !content.is_char_boundary(anchor_off) {
        anchor_off -= 1;
    }
    let anchor_ch = content[anchor_off..].chars().next().unwrap_or(' ');
    let target_class = word_class(anchor_ch);

    let mut start_byte = anchor_off;
    for ch in content[..anchor_off].chars().rev() {
        if word_class(ch) != target_class {
            break;
        }
        start_byte -= ch.len_utf8();
    }
    let mut end_byte = anchor_off + anchor_ch.len_utf8();
    for ch in content[end_byte..].chars() {
        if word_class(ch) != target_class {
            break;
        }
        end_byte += ch.len_utf8();
    }
    (start_byte, end_byte)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Whitespace,
    Punctuation,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        WordClass::Word
    } else {
        WordClass::Punctuation
    }
}

/// `rect` grown by `by` on every side: a forgiving hit target.
pub(super) fn grow(rect: Rectangle, by: f32) -> Rectangle {
    Rectangle {
        x: rect.x - by,
        y: rect.y - by,
        width: rect.width + 2.0 * by,
        height: rect.height + 2.0 * by,
    }
}

/// The props and the document, read for one frame or one event. The app owns the
/// lines, so this borrows them out of it; nothing here outlives the call it was
/// made for.
pub(super) struct View<'a> {
    pub(super) props: &'a DiffProps,
    pub(super) hunks: Vec<&'a [DiffHunkView]>,
    pub(super) labels: &'a [AnnotationLabel],
    pub(super) matches: &'a [FindMatch],
    pub(super) metrics: LayoutMetrics,
}

impl<'a> View<'a> {
    /// Reads the document through the props' sources.
    pub(super) fn read(props: &'a DiffProps, cx: &'a App) -> Self {
        let hunks = (0..props.files.len())
            .map(|file| props.source.hunks(file, cx))
            .collect();
        let labels = props
            .annotations
            .as_ref()
            .map_or(&[][..], |gutter| gutter.labels.get(cx));
        let matches = props
            .find
            .as_ref()
            .map_or(&[][..], |find| find.matches.get(cx));
        Self::new(props, hunks, labels, matches)
    }

    pub(super) fn new(
        props: &'a DiffProps,
        hunks: Vec<&'a [DiffHunkView]>,
        labels: &'a [AnnotationLabel],
        matches: &'a [FindMatch],
    ) -> Self {
        let digits = props.digits.get().unwrap_or_else(|| {
            let digits = compute_gutter_digit_count(&hunks);
            props.digits.set(Some(digits));
            digits
        });
        let mut metrics = LayoutMetrics::new(props.typography, digits, props.font, props.plain);
        if props.plain
            && let Some(gutter) = &props.annotations
        {
            metrics.gutter_width +=
                gutter.width_chars as f32 * metrics.char_width + GUTTER_HORIZONTAL_PADDING;
        }
        Self {
            props,
            hunks,
            labels,
            matches,
            metrics,
        }
    }

    pub(super) fn file(&self, file: usize) -> Option<&'a DiffFileView> {
        self.props.files.get(file)
    }

    pub(super) fn line(&self, file: usize, hunk: usize, line: usize) -> Option<&'a DiffLine> {
        self.hunks.get(file)?.get(hunk)?.lines.get(line)
    }

    pub(super) fn annotation_width(&self) -> f32 {
        match &self.props.annotations {
            Some(gutter) if self.props.plain => {
                gutter.width_chars as f32 * self.metrics.char_width + GUTTER_HORIZONTAL_PADDING
            }
            _ => 0.0,
        }
    }

    /// The blame run under `point` (see [`annotation_run`]).
    pub(super) fn annotation_run_at(
        &self,
        index: &HeightIndex,
        bounds: Rectangle,
        vertical_offset: f32,
        point: Point,
    ) -> Option<(usize, usize)> {
        self.props.annotations.as_ref()?;
        let line = self.annotation_line_at(index, bounds, vertical_offset, point)?;
        annotation_run(self.labels, line)
    }

    /// The 0-based line under `point` when it sits in the blame column.
    pub(super) fn annotation_line_at(
        &self,
        index: &HeightIndex,
        bounds: Rectangle,
        vertical_offset: f32,
        point: Point,
    ) -> Option<usize> {
        let width = self.annotation_width();
        if width <= 0.0 || point.x >= bounds.x + width {
            return None;
        }
        let row = index.row_at(point.y - bounds.y + vertical_offset)?;
        let &(file, hunk, line) = index.row_ids.get(row)?;
        self.line(file as usize, hunk as usize, line as usize)?
            .new_line
            .map(|number| number.saturating_sub(1))
    }

    /// Height of the strip `file` gets under its header, above its first hunk
    /// (zero for none): as tall as a hunk strip.
    pub(super) fn file_strip_height(&self, file: usize) -> f32 {
        match &self.props.hunk_actions {
            Some(actions) if !self.props.plain && actions.file_strip(file).is_some() => {
                self.metrics.hunk_header_height
            }
            _ => 0.0,
        }
    }

    /// Names which files have a strip or a note and which are folded, for
    /// [`HeightIndex::key`].
    fn file_strips_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(actions) = &self.props.hunk_actions {
            for strip in &actions.files {
                strip.file.hash(&mut hasher);
            }
        }
        self.props.folded.hash(&mut hasher);
        for file in &self.props.files {
            file.note.is_some().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Where a strip's own text starts in `file`: the hunk headers' x.
    pub(super) fn strip_text_x(&self, index: &HeightIndex, file: usize, bounds: Rectangle) -> f32 {
        let region_x = match self.props.side_by_side && !index.is_full_width(file) {
            true => self.split_layout(bounds.width).gutter_width + PREFIX_WIDTH,
            false => self.metrics.gutter_width + self.metrics.prefix_width,
        };
        bounds.x + region_x + TEXT_X_PADDING
    }

    /// File strip `strip` laid out with its top at screen-y `y`.
    pub(super) fn file_strip_layout(
        &self,
        index: &HeightIndex,
        strip: usize,
        y: f32,
        bounds: Rectangle,
    ) -> Option<FilePlaced> {
        let actions = self.props.hunk_actions.as_ref()?;
        let spec = actions.files.get(strip)?;
        let rect = Rectangle {
            x: bounds.x,
            y,
            width: bounds.width,
            height: self.metrics.hunk_header_height,
        };
        Some(hunk_actions::place_file(
            spec,
            &actions.style,
            rect,
            HEADER_HORIZONTAL_PADDING,
            self.strip_text_x(index, spec.file, bounds),
            self.text_width(&spec.label),
        ))
    }

    /// The hunk-strip buttons of `(file, hunk)` when its header is drawn at
    /// screen-y `y`: the row's index and where each button sits.
    pub(super) fn hunk_action_layout(
        &self,
        file: usize,
        hunk: usize,
        y: f32,
        bounds: Rectangle,
    ) -> Option<(usize, Vec<Placed>)> {
        let actions = self.props.hunk_actions.as_ref()?;
        if self.props.plain {
            return None;
        }
        let row = actions.row(file, hunk)?;
        let header = &self.hunks.get(file)?.get(hunk)?.header;
        let strip = Rectangle {
            x: bounds.x,
            y,
            width: bounds.width,
            height: self.metrics.hunk_header_height,
        };
        // Where the strip's own text ends (the unified gutter; the split one is
        // narrower, which only leaves more room).
        let min_x = bounds.x
            + self.metrics.gutter_width
            + self.metrics.prefix_width
            + TEXT_X_PADDING
            + self.text_width(header);
        Some((
            row,
            hunk_actions::place(
                &actions.rows[row],
                &actions.style,
                strip,
                HEADER_HORIZONTAL_PADDING,
                min_x,
            ),
        ))
    }

    /// The hunk- or file-strip button under `point`, against the layout `index`
    /// scrolled to `vertical_offset`.
    pub(super) fn strip_action_at(
        &self,
        index: &HeightIndex,
        bounds: Rectangle,
        vertical_offset: f32,
        point: Point,
    ) -> Option<ActionHit> {
        let actions = self.props.hunk_actions.as_ref()?;
        // A (sticky) file header drawn over the strip wins.
        let headers = self.visible_file_header_positions(index, bounds, vertical_offset);
        if headers
            .iter()
            .any(|&(_, y)| point.y >= y && point.y < y + self.metrics.file_header_height)
        {
            return None;
        }
        let content_y = point.y - bounds.y + vertical_offset;
        if let Some(slot) = index
            .file_strips
            .partition_point(|&(_, top)| top <= content_y)
            .checked_sub(1)
            && let (file, top) = index.file_strips[slot]
            && content_y < top + self.metrics.hunk_header_height
        {
            let strip = actions.file_strip(file)?;
            let y = bounds.y + (top - vertical_offset);
            let placed = self.file_strip_layout(index, strip, y, bounds)?;
            return placed
                .hit(&actions.files[strip], point)
                .map(|button| ActionHit::File(strip, button));
        }
        let slot = index
            .hunk_tops
            .partition_point(|&top| top <= content_y)
            .checked_sub(1)?;
        let top = index.hunk_tops[slot];
        if content_y >= top + self.metrics.hunk_header_height {
            return None;
        }
        let (file, hunk) = index.hunk_ids[slot];
        let y = bounds.y + (top - vertical_offset);
        let (row, placed) = self.hunk_action_layout(file as usize, hunk as usize, y, bounds)?;
        placed
            .iter()
            .position(|button| button.rect.contains(point))
            .map(|button| ActionHit::Hunk(row, button))
    }

    /// The button of `file`'s note drawn with its band's top at screen-y `top`: a
    /// held-back file's "show" button, when the view can expand it.
    pub(super) fn note_button_rect(
        &self,
        file: usize,
        top: f32,
        bounds: Rectangle,
    ) -> Option<Rectangle> {
        self.props.on_expand_file.as_ref()?;
        if !self.is_collapsed(file) {
            return None;
        }
        let label = self.file(file)?.note.as_ref()?.action.as_deref()?;
        let width = measure::line_width(label, NOTE_TEXT_SIZE, self.props.ui_font)
            + 2.0 * NOTE_BUTTON_PAD_X;
        Some(Rectangle {
            x: (bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - width).round(),
            y: (top + (self.metrics.note_height - NOTE_BUTTON_HEIGHT) / 2.0).round(),
            width: width.round(),
            height: NOTE_BUTTON_HEIGHT,
        })
    }

    /// The file whose note button is under `point`.
    pub(super) fn note_button_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
    ) -> Option<usize> {
        if self
            .file_header_at(index, vertical_offset, bounds, point)
            .is_some()
        {
            return None;
        }
        let content_y = point.y - bounds.y + vertical_offset;
        let slot = index
            .note_bands
            .partition_point(|&(_, top)| top <= content_y)
            .checked_sub(1)?;
        let (file, top) = index.note_bands[slot];
        if content_y >= top + self.metrics.note_height {
            return None;
        }
        let rect = self.note_button_rect(file, bounds.y + (top - vertical_offset), bounds)?;
        rect.contains(point).then_some(file)
    }

    /// The hunk under `point`: its header or one of its rows.
    pub(super) fn hunk_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
    ) -> Option<(usize, usize)> {
        if !bounds.contains(point) || self.props.plain {
            return None;
        }
        if self
            .file_header_at(index, vertical_offset, bounds, point)
            .is_some()
        {
            return None;
        }
        let content_y = point.y - bounds.y + vertical_offset;
        let slot = index
            .hunk_tops
            .partition_point(|&top| top <= content_y)
            .checked_sub(1)?;
        let (file, hunk) = index.hunk_ids[slot];
        let end = match index.hunk_ids.get(slot + 1) {
            Some(&(next_file, _)) if next_file == file => index.hunk_tops[slot + 1],
            _ => *index.file_tops.get(file as usize + 1)?,
        };
        (content_y < end).then_some((file as usize, hunk as usize))
    }

    pub(super) fn is_collapsed(&self, file_index: usize) -> bool {
        self.props
            .collapsed_files
            .get(file_index)
            .copied()
            .unwrap_or(false)
    }

    /// The collapsed file whose placeholder header is under `point`.
    pub(super) fn expand_header_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
    ) -> Option<usize> {
        self.props.on_expand_file.as_ref()?;
        if self.props.plain {
            return None;
        }
        let y = point.y - bounds.y + vertical_offset;
        let hunk = index
            .hunk_tops
            .partition_point(|&top| top <= y)
            .checked_sub(1)?;
        if y >= index.hunk_tops[hunk] + self.metrics.hunk_header_height {
            return None;
        }
        let file_index = index.hunk_ids[hunk].0 as usize;
        self.is_collapsed(file_index).then_some(file_index)
    }

    /// Total height of the revision header block (metadata lines + description +
    /// vertical padding). Zero when no header is set.
    pub(super) fn header_height(&self) -> f32 {
        if self.props.header.is_empty() {
            0.0
        } else {
            self.props
                .header
                .iter()
                .map(|line| self.header_line_height(line))
                .sum::<f32>()
                + HEADER_VERTICAL_PADDING * 2.0
        }
    }

    pub(super) fn header_line_height(&self, line: &HeaderLine) -> f32 {
        match line {
            HeaderLine::Spacer(height) => *height,
            HeaderLine::Title { .. } => super::HEADER_TITLE_ROW_HEIGHT,
            _ => self.metrics.row_height,
        }
    }

    /// Header row at a content-space y coordinate, accounting for variable-height
    /// spacer rows used by the inline editor.
    pub(super) fn header_line_at_y(&self, target_y: f32) -> Option<usize> {
        let mut y = HEADER_VERTICAL_PADDING;
        for (index, line) in self.props.header.iter().enumerate() {
            let height = self.header_line_height(line);
            if target_y >= y && target_y < y + height {
                return Some(index);
            }
            y += height;
        }
        None
    }

    fn description_geometry(&self) -> Option<(f32, f32)> {
        let mut y = HEADER_VERTICAL_PADDING;
        let mut start = None;
        let mut height = 0.0;
        for line in &self.props.header {
            if matches!(line, HeaderLine::Description(_)) {
                start.get_or_insert(y);
                height += self.header_line_height(line);
            } else if start.is_some() {
                break;
            }
            y += self.header_line_height(line);
        }
        start.map(|start| (start, height))
    }

    pub(super) fn description_edit_bounds(
        &self,
        bounds: Rectangle,
        vertical_offset: f32,
    ) -> Option<Rectangle> {
        self.props.on_edit_description.as_ref()?;
        let (description_y, description_height) = self.description_geometry()?;
        let description_top = bounds.y + description_y - vertical_offset;
        let description_bottom = description_top + description_height;
        if description_bottom <= bounds.y || description_top >= bounds.y + bounds.height {
            return None;
        }
        // Keep the affordance visible while any part of a long description is on
        // screen. It tracks the description block rather than becoming a sticky
        // header: once the block scrolls away, the button does too.
        let y = (description_top + (self.metrics.row_height - HEADER_EDIT_HEIGHT) / 2.0)
            .max(bounds.y)
            .min(description_bottom - HEADER_EDIT_HEIGHT);
        Some(Rectangle {
            x: bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - HEADER_EDIT_WIDTH,
            y,
            width: HEADER_EDIT_WIDTH,
            height: HEADER_EDIT_HEIGHT,
        })
    }

    pub(super) fn description_row_contains(
        &self,
        bounds: Rectangle,
        vertical_offset: f32,
        point: Point,
    ) -> bool {
        let Some((y, height)) = self.description_geometry() else {
            return false;
        };
        Rectangle {
            x: bounds.x + HEADER_HORIZONTAL_PADDING,
            y: bounds.y + y - vertical_offset,
            width: (bounds.width - HEADER_HORIZONTAL_PADDING * 2.0).max(1.0),
            height,
        }
        .contains(point)
    }

    /// The selectable text of header line `index`, if any: field *values* and
    /// description lines. Labels, bookmark chips, and blank lines are not.
    pub(super) fn header_selectable_text(&self, index: usize) -> Option<&'a str> {
        match self.props.header.get(index)? {
            HeaderLine::Field { value, .. } => Some(value),
            HeaderLine::Description(line) => Some(line),
            HeaderLine::Rich(field) => Some(&field.value),
            _ => None,
        }
    }

    /// The x where header field values begin: left edge + the (padded) label
    /// column + gap. Mirrors the header painter's layout so hit-testing and
    /// selection rendering land on the same glyphs.
    pub(super) fn header_value_x(&self, bounds: Rectangle) -> f32 {
        bounds.x + HEADER_HORIZONTAL_PADDING + self.header_label_width() + HEADER_LABEL_GAP
    }

    /// The label column: as wide as the widest label of any field.
    pub(super) fn header_label_width(&self) -> f32 {
        self.props
            .header
            .iter()
            .filter_map(|line| match line {
                HeaderLine::Field { label, .. } | HeaderLine::Bookmarks { label, .. } => {
                    Some(label.trim_end())
                }
                HeaderLine::Rich(field) => Some(field.label.trim_end()),
                _ => None,
            })
            .map(|label| self.text_width(label))
            .fold(0.0, f32::max)
    }

    /// Content-space y of header line `index`'s top.
    pub(super) fn header_line_top(&self, index: usize) -> f32 {
        HEADER_VERTICAL_PADDING
            + self.props.header[..index.min(self.props.header.len())]
                .iter()
                .map(|line| self.header_line_height(line))
                .sum::<f32>()
    }

    /// The link and buttons of rich header line `index` on screen, as
    /// (slot, rect, action): slot 0 is the link, 1.. the buttons.
    pub(super) fn rich_targets(
        &self,
        index: usize,
        field: &HeaderField,
        bounds: Rectangle,
        vertical_offset: f32,
    ) -> Vec<(usize, Rectangle, HeaderAction)> {
        let top = bounds.y + self.header_line_top(index) - vertical_offset;
        let cw = self.metrics.char_width;
        let value_x = self.header_value_x(bounds);
        // Measured, not counted: a CJK author is wider than its chars.
        let geometry = self.measure_line(&field.value, f32::INFINITY);
        let mut targets = Vec::new();
        if let Some((chars, action)) = &field.link {
            let end = field
                .value
                .char_indices()
                .nth(*chars)
                .map_or(field.value.len(), |(byte, _)| byte);
            targets.push((
                0,
                Rectangle {
                    x: value_x,
                    y: top,
                    width: geometry.column_at(0, end) * cw,
                    height: self.metrics.row_height,
                },
                action.clone(),
            ));
        }
        let mut x = value_x + geometry.width_columns * cw;
        for chip in &field.chips {
            x += HEADER_CHIP_GAP + chip::full_width(chip);
        }
        let side = HEADER_BUTTON_SIZE.min(self.metrics.row_height);
        for (slot, (_, action)) in field.buttons.iter().enumerate() {
            x += HEADER_CHIP_GAP;
            targets.push((
                slot + 1,
                Rectangle {
                    x,
                    y: top + (self.metrics.row_height - side) / 2.0,
                    width: side,
                    height: side,
                },
                action.clone(),
            ));
            x += side;
        }
        targets
    }

    /// The rich-field link or button under `point`, if any.
    pub(super) fn header_action_at(
        &self,
        bounds: Rectangle,
        vertical_offset: f32,
        point: Point,
    ) -> Option<(usize, usize, HeaderAction)> {
        self.props.on_header_action.as_ref()?;
        let index = self.header_line_at_y(point.y - bounds.y + vertical_offset)?;
        let HeaderLine::Rich(field) = &self.props.header[index] else {
            return None;
        };
        self.rich_targets(index, field, bounds, vertical_offset)
            .into_iter()
            .find(|(_, rect, _)| rect.contains(point))
            .map(|(slot, _, action)| (index, slot, action))
    }

    /// Screen-x where header line `index`'s selectable text starts: the value
    /// column for fields, or the indented column for description lines.
    pub(super) fn header_text_origin_x(&self, index: usize, bounds: Rectangle) -> f32 {
        match self.props.header.get(index) {
            Some(HeaderLine::Description(_)) => {
                bounds.x
                    + HEADER_HORIZONTAL_PADDING
                    + HEADER_DESCRIPTION_INDENT * self.metrics.char_width
            }
            _ => self.header_value_x(bounds),
        }
    }

    /// Expand `pos` to cover the unit (word or line) that contains it, in either
    /// the header or the body. Character mode is a no-op.
    pub(super) fn expand_to_unit(
        &self,
        pos: TextPosition,
        unit: SelectionUnit,
    ) -> (TextPosition, TextPosition) {
        let text: &str = match pos.region {
            Region::Header => match self.header_selectable_text(pos.line_index) {
                Some(text) => text,
                None => return (pos, pos),
            },
            Region::Body => match self.line(pos.file_index, pos.hunk_index, pos.line_index) {
                Some(line) => &line.content,
                None => return (pos, pos),
            },
        };

        match unit {
            SelectionUnit::Character => (pos, pos),
            SelectionUnit::Line => (
                TextPosition { byte: 0, ..pos },
                TextPosition {
                    byte: text.len(),
                    ..pos
                },
            ),
            SelectionUnit::Word => {
                let (start_byte, end_byte) = word_bounds(text, pos.byte);
                (
                    TextPosition {
                        byte: start_byte,
                        ..pos
                    },
                    TextPosition {
                        byte: end_byte,
                        ..pos
                    },
                )
            }
        }
    }

    /// Rebuild `cell`'s [`HeightIndex`] if the content shape or wrap width changed
    /// since it was built. One O(total lines) pass per change; every per-frame
    /// query then reads prefix sums instead of re-walking.
    pub(super) fn ensure_height_index(
        &self,
        cell: &RefCell<HeightIndex>,
        memo: &RefCell<GeometryMemo>,
        width: f32,
    ) {
        // Keyed on the viewport width (not a derived text width): split and
        // full-width rows wrap against different widths, both functions of it.
        // With wrapping off the layout doesn't depend on the width at all. But
        // previews fit the pane's width, so a document with any keys on it even
        // without wrapping.
        let props = self.props;
        let key = Some((
            props.layout_version,
            props.files.len(),
            self.header_height().to_bits() as usize,
            if props.wrap || props.media_key.is_some() {
                width.to_bits()
            } else {
                WIDTH_AGNOSTIC
            },
            props.wrap,
            props.side_by_side,
            props.media_key,
            self.file_strips_key(),
        ));
        if cell.borrow().key == key {
            return;
        }

        let mut index = cell.borrow_mut();
        let row_count: usize = self
            .hunks
            .iter()
            .map(|hunks| hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>())
            .sum();
        index.file_tops.clear();
        index.file_tops.reserve(props.files.len() + 1);
        index.hunk_tops.clear();
        index.hunk_ids.clear();
        index.row_tops.clear();
        index.row_tops.reserve(row_count);
        index.row_heights.clear();
        index.row_heights.reserve(row_count);
        index.row_ids.clear();
        index.row_ids.reserve(row_count);
        index.pair_lines.clear();
        if props.side_by_side {
            index.pair_lines.reserve(row_count);
        }
        index.media_bands.clear();
        index.file_strips.clear();
        index.note_bands.clear();
        index.max_columns = 0.0;
        index.max_split_columns = 0.0;
        index.unmeasured_wide.clear();
        index.full_width_files = if props.side_by_side {
            self.hunks
                .iter()
                .map(|hunks| file_is_single_sided(hunks))
                .collect()
        } else {
            Vec::new()
        };
        let unified_width = self.wrap_width(self.content_width(width));
        let split_width = self.wrap_width(self.effective_text_width(width));
        self.ensure_bounds(memo, row_count);
        // Where the current hunk's lines start in the bounds.
        let mut base = 0usize;

        let mut y = self.header_height() + self.metrics.top_inset;
        for (file_index, hunks) in self.hunks.iter().enumerate() {
            index.file_tops.push(y);
            y += self.metrics.file_header_height;
            if self.is_folded(file_index) {
                base += hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>();
                continue;
            }
            let strip = self.file_strip_height(file_index);
            if strip > 0.0 {
                index.file_strips.push((file_index, y));
                y += strip;
            }
            if let Some(media) = props.media_of(file_index) {
                // The preview stands in for the file's rows.
                let height = media::layout(media, width, self.metrics.row_height).height;
                index.media_bands.push((file_index, y, height));
                y += height;
                base += hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>();
                continue;
            }
            if self
                .file(file_index)
                .is_some_and(|file| file.note.is_some())
            {
                // The note stands in for the file's rows (placeholders, if any).
                index.note_bands.push((file_index, y));
                y += self.metrics.note_height;
                base += hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>();
                continue;
            }
            let split_file = props.side_by_side && !index.is_full_width(file_index);
            for (hunk_index, hunk) in hunks.iter().enumerate() {
                index.hunk_tops.push(y);
                index.hunk_ids.push((file_index as u32, hunk_index as u32));
                y += self.metrics.hunk_header_height;
                if split_file {
                    y = self.push_split_rows(
                        &mut index,
                        memo,
                        (file_index, hunk_index),
                        hunk,
                        y,
                        split_width,
                        base,
                    );
                } else {
                    for (line_index, line) in hunk.lines.iter().enumerate() {
                        let key = body_geometry_key(
                            file_index,
                            hunk_index,
                            line_index,
                            unified_width,
                            self.metrics.char_width,
                        );
                        let (height, bound) =
                            self.indexed_row_height(memo, key, line, base, unified_width);
                        index.row_tops.push(y);
                        index.row_heights.push(height);
                        index.row_ids.push((
                            file_index as u32,
                            hunk_index as u32,
                            line_index as u32,
                        ));
                        if props.side_by_side {
                            // Keep `pair_lines` aligned with `row_tops` for
                            // full-width files; consumers see `pair: None` via the
                            // `is_full_width` filter.
                            index
                                .pair_lines
                                .push((line_index as u32, line_index as u32));
                        }
                        if !props.wrap {
                            if line.content.is_ascii() {
                                index.max_columns = index.max_columns.max(bound);
                            } else {
                                index.unmeasured_wide.push((bound, false, key));
                            }
                        }
                        y += height;
                    }
                }
                base += hunk.lines.len();
            }
        }
        index.file_tops.push(y);
        index.total_height = y;
        index.key = key;
    }

    /// Append one hunk's side-by-side row pairs to the index: context (and
    /// note/conflict) lines sit on both sides; each deletion run + addition run
    /// pairs index-wise, padding whichever side is shorter. The row id's line
    /// component is the pair's first member, which keeps `row_ids` sorted within
    /// the hunk. Returns the new running `y`.
    #[allow(clippy::too_many_arguments)]
    fn push_split_rows(
        &self,
        index: &mut HeightIndex,
        memo: &RefCell<GeometryMemo>,
        (file_index, hunk_index): (usize, usize),
        hunk: &DiffHunkView,
        mut y: f32,
        content_width: f32,
        base: usize,
    ) -> f32 {
        let lines = &hunk.lines;
        let mut push = |index: &mut HeightIndex, rep: usize, pair: (u32, u32), height: f32| {
            index.row_tops.push(y);
            index.row_heights.push(height);
            index
                .row_ids
                .push((file_index as u32, hunk_index as u32, rep as u32));
            index.pair_lines.push(pair);
            y += height;
        };
        let mut max_columns = 0.0_f32;
        let mut unmeasured = Vec::new();
        let mut height_of = |line_index: usize, line: &DiffLine| {
            let key = body_geometry_key(
                file_index,
                hunk_index,
                line_index,
                content_width,
                self.metrics.char_width,
            );
            let (height, bound) = self.indexed_row_height(memo, key, line, base, content_width);
            if !self.props.wrap {
                if line.content.is_ascii() {
                    max_columns = max_columns.max(bound);
                } else {
                    unmeasured.push((bound, true, key));
                }
            }
            height
        };
        let mut i = 0;
        while i < lines.len() {
            if lines[i].kind == DiffLineKind::Addition {
                // An addition run with no deletion run in front of it (those are
                // consumed below as pairs): new lines only. They belong to the
                // right column, the left side is padding.
                let height = height_of(i, &lines[i]);
                push(&mut *index, i, (NO_LINE, i as u32), height);
                i += 1;
                continue;
            }
            if lines[i].kind != DiffLineKind::Deletion {
                let height = height_of(i, &lines[i]);
                push(&mut *index, i, (i as u32, i as u32), height);
                i += 1;
                continue;
            }
            let del_start = i;
            while i < lines.len() && lines[i].kind == DiffLineKind::Deletion {
                i += 1;
            }
            let add_start = i;
            while i < lines.len() && lines[i].kind == DiffLineKind::Addition {
                i += 1;
            }
            let dels = add_start - del_start;
            let adds = i - add_start;
            for k in 0..dels.max(adds) {
                let left = (k < dels).then_some(del_start + k);
                let right = (k < adds).then_some(add_start + k);
                let rep = left.or(right).unwrap_or(del_start);
                let height = [left, right]
                    .into_iter()
                    .flatten()
                    .filter_map(|line_index| Some((line_index, lines.get(line_index)?)))
                    .map(|(line_index, line)| height_of(line_index, line))
                    .fold(self.metrics.row_height, f32::max);
                push(
                    &mut *index,
                    rep,
                    (
                        left.map_or(NO_LINE, |l| l as u32),
                        right.map_or(NO_LINE, |r| r as u32),
                    ),
                    height,
                );
            }
        }
        index.max_split_columns = index.max_split_columns.max(max_columns);
        index.unmeasured_wide.extend(unmeasured);
        y
    }

    pub(super) fn file_offset(&self, index: &HeightIndex, file_index: usize) -> f32 {
        index
            .file_tops
            .get(file_index)
            .copied()
            .unwrap_or(index.total_height)
    }

    pub(super) fn file_at_offset(&self, index: &HeightIndex, offset: f32) -> usize {
        // The trailing sentinel is the content end, not a file.
        let file_count = self.props.files.len();
        let tops = &index.file_tops[..file_count.min(index.file_tops.len())];
        // Number of file tops at or above `offset`, minus one = the file whose
        // span contains it. An offset in the revision header maps to file 0; past
        // the end clamps to the last file.
        tops.partition_point(|&top| top <= offset).saturating_sub(1)
    }

    /// The text area's width: what's right of the text's left edge, less the
    /// scrollbar's gutter so wrapped text never runs beneath the bar.
    pub(super) fn content_width(&self, viewport_width: f32) -> f32 {
        (viewport_width
            - self.metrics.gutter_width
            - self.metrics.prefix_width
            - TEXT_X_PADDING
            - scrollbar::GUTTER)
            .max(self.metrics.char_width)
    }

    /// The width one logical line wraps against: the whole text area in unified
    /// mode, one column's text area in side-by-side.
    pub(super) fn effective_text_width(&self, viewport_width: f32) -> f32 {
        if self.props.side_by_side {
            self.split_layout(viewport_width).text_width
        } else {
            self.content_width(viewport_width)
        }
    }

    /// Per-column x-geometry for the side-by-side layout. Both columns share one
    /// `text_width`, so the wrap math stays column-agnostic; only x origins
    /// differ. Each side gets a single-number gutter (old line numbers left, new
    /// right).
    pub(super) fn split_layout(&self, viewport_width: f32) -> SplitLayout {
        let gutter_width = self.metrics.number_column;
        let column_width = ((viewport_width - 1.0) / 2.0).max(1.0);
        // Both columns keep the scrollbar's gutter, so the right one's text stays
        // clear of the bar and the two still wrap alike.
        let text_width =
            (column_width - gutter_width - PREFIX_WIDTH - TEXT_X_PADDING - scrollbar::GUTTER)
                .max(self.metrics.char_width);
        SplitLayout {
            gutter_width,
            text_width,
            left_text_x: gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
            divider_x: column_width,
            right_gutter_x: column_width + 1.0,
            right_text_x: column_width + 1.0 + gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
        }
    }

    /// The width a line wraps at: the pane's text width while wrapping, unbounded
    /// otherwise.
    pub(super) fn wrap_width(&self, text_width: f32) -> f32 {
        if self.props.wrap {
            text_width
        } else {
            f32::INFINITY
        }
    }

    /// The width `file`'s rows are laid out against: one split column for a split
    /// file, the whole text area for unified and full-width ones.
    pub(super) fn text_width_for_file(
        &self,
        index: &HeightIndex,
        file: usize,
        viewport_width: f32,
    ) -> f32 {
        if index.is_full_width(file) {
            self.content_width(viewport_width)
        } else {
            self.effective_text_width(viewport_width)
        }
    }

    /// [`LineGeometry`] for one diff row, memoized per (row, width). Every consumer
    /// goes through here, so a row is measured once per width no matter how many
    /// of them ask.
    pub(super) fn geometry(
        &self,
        memo: &RefCell<GeometryMemo>,
        key: GeometryKey,
        content: &str,
        wrap_width: f32,
    ) -> Rc<LineGeometry> {
        memoized(memo, key, || {
            let params = self.geometry_params(wrap_width);
            let shape = shape_of(memo, key, content, params);
            LineGeometry::from_shape(&shape, content.len(), params)
        })
    }

    /// [`LineGeometry`] for a line the memo doesn't hold: the revision header's
    /// field values and description. Nothing keys those stably, and there are only
    /// ever a handful on screen, none of which wraps, so they are measured where
    /// they are used. An ASCII one costs no shaping at all.
    pub(super) fn measure_line(&self, content: &str, wrap_width: f32) -> LineGeometry {
        LineGeometry::measure(content, self.geometry_params(wrap_width))
    }

    pub(super) fn geometry_params(&self, wrap_width: f32) -> GeometryParams {
        GeometryParams {
            size: self.props.typography.size,
            font: self.props.font,
            char_width: self.metrics.char_width,
            wrap_width,
        }
    }

    /// Visual lines `content` spans at `wrap_width`. A line the cheap [`column_bound`]
    /// already fits inside the width cannot wrap, so only the ones that might are measured,
    /// once each: a later width is arithmetic over that measurement.
    #[cfg(test)]
    pub(super) fn visual_rows(
        &self,
        memo: &RefCell<GeometryMemo>,
        key: GeometryKey,
        content: &str,
        wrap_width: f32,
    ) -> usize {
        self.visual_rows_bounded(memo, key, content, column_bound(content), wrap_width)
    }

    fn visual_rows_bounded(
        &self,
        memo: &RefCell<GeometryMemo>,
        key: GeometryKey,
        content: &str,
        bound: f32,
        wrap_width: f32,
    ) -> usize {
        if bound * self.metrics.char_width <= wrap_width {
            return 1;
        }
        shape_of(memo, key, content, self.geometry_params(wrap_width))
            .wrap(wrap_width)
            .len()
    }

    #[cfg(test)]
    pub(super) fn row_height(
        &self,
        memo: &RefCell<GeometryMemo>,
        key: GeometryKey,
        line: &DiffLine,
        wrap_width: f32,
    ) -> f32 {
        self.visual_rows(memo, key, &line.content, wrap_width) as f32 * self.metrics.row_height
    }

    /// [`column_bound`] of every line in document order, worked out once per document:
    /// the rebuild a width change runs reads this instead of scanning each line's bytes.
    fn ensure_bounds(&self, memo: &RefCell<GeometryMemo>, row_count: usize) {
        let key = Some((self.props.layout_version, row_count));
        let mut memo = memo.borrow_mut();
        if memo.bounds_key == key && memo.bounds.len() == row_count {
            return;
        }
        memo.bounds.clear();
        memo.bounds.reserve(row_count);
        for hunks in &self.hunks {
            for hunk in hunks.iter() {
                memo.bounds
                    .extend(hunk.lines.iter().map(|line| column_bound(&line.content)));
            }
        }
        memo.bounds_key = key;
    }

    /// A row's height at `wrap_width` and its line's bound, read from
    /// [`Self::ensure_bounds`] at `base + line_index`.
    fn indexed_row_height(
        &self,
        memo: &RefCell<GeometryMemo>,
        key: GeometryKey,
        line: &DiffLine,
        base: usize,
        wrap_width: f32,
    ) -> (f32, f32) {
        let bound = memo.borrow().bounds[base + key.line_index as usize];
        let rows = self.visual_rows_bounded(memo, key, &line.content, bound, wrap_width);
        (rows as f32 * self.metrics.row_height, bound)
    }

    /// How far the no-wrap mode can scroll sideways: the furthest any line
    /// reaches past the text area it is laid out in (plus one column of breathing
    /// room), zero while wrapping. In split mode a one-sided file spans the whole
    /// text area and a split one a column's, so each is held to its own.
    pub(super) fn max_horizontal(&self, index: &HeightIndex, viewport_width: f32) -> f32 {
        if self.props.wrap {
            return 0.0;
        }
        let cw = self.metrics.char_width;
        let overflow = |columns: f32, text_width: f32| {
            if columns > 0.0 {
                (columns + 1.0) * cw - text_width
            } else {
                0.0
            }
        };
        let full = overflow(index.max_columns, self.content_width(viewport_width));
        let split = if self.props.side_by_side {
            overflow(
                index.max_split_columns,
                self.split_layout(viewport_width).text_width,
            )
        } else {
            0.0
        };
        full.max(split).max(0.0)
    }

    /// Folds the non-ASCII lines a rebuild left unmeasured into the sideways
    /// extent. [`column_bound`] errs high on them (three columns a char), so each is
    /// shaped for its real width, widest bound first, and only while its bound could
    /// still beat what's known. Runs once per rebuild, off the rebuild itself: that
    /// one walks every line on every width change while wrapping and must not shape.
    pub(super) fn settle_extent(&self, cell: &RefCell<HeightIndex>, memo: &RefCell<GeometryMemo>) {
        let mut index = cell.borrow_mut();
        if index.unmeasured_wide.is_empty() {
            return;
        }
        let mut wide = std::mem::take(&mut index.unmeasured_wide);
        wide.sort_by(|a, b| b.0.total_cmp(&a.0));
        let cw = self.metrics.char_width.max(1.0);
        for (bound, split, key) in wide {
            let widest = if split {
                index.max_split_columns
            } else {
                index.max_columns
            };
            if bound <= widest {
                continue;
            }
            let Some(line) = self.line(
                key.file_index as usize,
                key.hunk_index as usize,
                key.line_index as usize,
            ) else {
                continue;
            };
            let columns = shape_of(
                memo,
                key,
                &line.content,
                self.geometry_params(f32::INFINITY),
            )
            .width()
                / cw;
            let slot = if split {
                &mut index.max_split_columns
            } else {
                &mut index.max_columns
            };
            *slot = slot.max(columns);
        }
    }

    /// Height of index row `row`: the taller member's in side-by-side. Read off the
    /// index rather than re-measured.
    pub(super) fn index_row_height(&self, index: &HeightIndex, row: usize) -> f32 {
        index
            .row_heights
            .get(row)
            .copied()
            .unwrap_or(self.metrics.row_height)
    }

    /// Y position (in content space, before viewport scroll) of the visual line
    /// containing the byte at `byte_offset` within `(file_idx, hunk_idx, line_idx)`.
    /// `None` if any of the indices are out of bounds.
    pub(super) fn match_target_y(
        &self,
        index: &HeightIndex,
        memo: &RefCell<GeometryMemo>,
        viewport_width: f32,
        (file_idx, hunk_idx, line_idx, byte_offset): (usize, usize, usize, usize),
    ) -> Option<f32> {
        let line = self.line(file_idx, hunk_idx, line_idx)?;
        let row = index.row_of_line(file_idx, hunk_idx, line_idx)?;
        let mut y = *index.row_tops.get(row)?;

        // Offset within the wrapped row: figure out which visual line the byte sits
        // on so a match on the 5th wrap row of a 200-char line doesn't scroll to the
        // row top and leave the match off-screen.
        let wrap_width = self.wrap_width(self.text_width_for_file(index, file_idx, viewport_width));
        let key = body_geometry_key(
            file_idx,
            hunk_idx,
            line_idx,
            wrap_width,
            self.metrics.char_width,
        );
        let geometry = self.geometry(memo, key, &line.content, wrap_width);
        y += geometry.visual_line_of(byte_offset) as f32 * self.metrics.row_height;
        Some(y)
    }

    /// Locate the document position under `point`, and the column it resolved in
    /// for split rows. Unlike a normal "click to put cursor here" hit-test this
    /// clamps to the nearest valid row even when the click is in the chrome / below
    /// the last row, so dragging the mouse outside the viewport still produces a
    /// sensible selection endpoint.
    ///
    /// `lock` holds split-row resolution to one column regardless of the cursor's
    /// x: a drag stays in the column the selection started in.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn position_at_point(
        &self,
        index: &HeightIndex,
        memo: &RefCell<GeometryMemo>,
        point: Point,
        bounds: Rectangle,
        vertical_offset: f32,
        horizontal_offset: f32,
        lock: Option<SplitSide>,
    ) -> Option<(TextPosition, Option<SplitSide>)> {
        let split = self
            .props
            .side_by_side
            .then(|| self.split_layout(bounds.width));
        let unified_text_x =
            bounds.x + self.metrics.gutter_width + self.metrics.prefix_width + TEXT_X_PADDING;
        let text_x = match &split {
            // Side picked by the divider below; seed with the left column.
            Some(split) => bounds.x + split.left_text_x,
            None => unified_text_x,
        };
        let target_y = point.y - bounds.y + vertical_offset;
        let header_height = self.header_height();

        // The header sits above all file content. Field values (Commit ID, Author,
        // …) are selectable; clicks on labels, bookmark chips, or blank lines return
        // `None`.
        if header_height > 0.0 && target_y < header_height {
            let line_index = self.header_line_at_y(target_y)?;
            let text = self.header_selectable_text(line_index)?;
            let origin_x = self.header_text_origin_x(line_index, bounds);
            let geometry = self.measure_line(text, f32::INFINITY);
            let column = (point.x - origin_x).max(0.0) / self.metrics.char_width;
            return Some((
                header_position(line_index, geometry.byte_at(0, column)),
                None,
            ));
        }

        // Candidate row by binary search: the last row starting at or above the
        // target y. The target may instead sit in a file/hunk header band (or past
        // the end): those fall through to the end-of-document snap below. A drag
        // doesn't get that far for a band: see `drag_position_at_point`.
        if let Some(row) = index.row_at(target_y) {
            let &(file_index, hunk_index, line_index) = index.row_ids.get(row)?;
            let (file_index, hunk_index, mut line_index) = (
                file_index as usize,
                hunk_index as usize,
                line_index as usize,
            );
            // Side-by-side: resolve which column the point is in (held to `lock`'s
            // column when set), falling back to the populated side of a padded pair,
            // and shift the text origin to that column's. Full-width files don't
            // have columns: they hit-test like unified rows.
            let mut text_x = text_x;
            let mut resolved_lane = None;
            if index.is_full_width(file_index) {
                text_x = unified_text_x;
            } else if let (Some(split), Some(&(left, right))) = (&split, index.pair_lines.get(row))
            {
                let in_right = match lock {
                    Some(SplitSide::Right) => true,
                    Some(SplitSide::Left) => false,
                    None => point.x >= bounds.x + split.divider_x,
                };
                let (member, side) = split_member(left, right, in_right);
                if member == NO_LINE {
                    return None;
                }
                if side == SplitSide::Right {
                    text_x = bounds.x + split.right_text_x;
                }
                resolved_lane = Some(side);
                line_index = member as usize;
            }
            let line = self.line(file_index, hunk_index, line_index)?;
            let row_top = index.row_tops[row];
            let wrap_width =
                self.wrap_width(self.text_width_for_file(index, file_index, bounds.width));
            let height = self.index_row_height(index, row);
            if target_y < row_top + height {
                // Each row may span multiple wrapped visual lines. Figure out which
                // visual line the click lands on, then ask the geometry which byte
                // of the source sits at that column: the same geometry the glyphs
                // were painted from, so the caret lands where the user aimed even
                // across tabs and wide glyphs.
                let key = body_geometry_key(
                    file_index,
                    hunk_index,
                    line_index,
                    wrap_width,
                    self.metrics.char_width,
                );
                let geometry = self.geometry(memo, key, &line.content, wrap_width);
                let visual_idx = (((target_y - row_top) / self.metrics.row_height).floor()
                    as usize)
                    .min(geometry.visual_lines() - 1);
                // The text is drawn shifted left by the horizontal scroll; shift the
                // cursor the other way to land on the same glyph.
                let column =
                    (point.x - text_x + horizontal_offset).max(0.0) / self.metrics.char_width;
                let byte = geometry.byte_at(visual_idx, column);
                return Some((
                    body_position(file_index, hunk_index, line_index, byte),
                    resolved_lane,
                ));
            }
        }

        // Not on a row (a press on a header band, or past the last row). Snap to the
        // end of the document so a drag below content selects everything up to it.
        let &(file, hunk, line) = index.row_ids.last()?;
        let (file_index, hunk_index, mut line_index) =
            (file as usize, hunk as usize, line as usize);
        if let Some(&(left, right)) = index.pair_lines.last() {
            // The last pair's later member is the true document tail.
            let tail = if right != NO_LINE { right } else { left };
            if tail != NO_LINE {
                line_index = line_index.max(tail as usize);
            }
        }
        let line = self.line(file_index, hunk_index, line_index)?;
        Some((
            body_position(file_index, hunk_index, line_index, line.content.len()),
            None,
        ))
    }

    /// The row nearest `target_y` when it's off every row but still among them: on a
    /// file or hunk header band, not in the revision header or past the last row.
    pub(super) fn band_snap(&self, index: &HeightIndex, target_y: f32) -> Option<(usize, bool)> {
        let header_height = self.header_height();
        if header_height > 0.0 && target_y < header_height {
            return None;
        }
        index.nearest_row_off_band(target_y)
    }

    /// Where a selection drag at `point` ends. As [`Self::position_at_point`], except
    /// that a header band between rows snaps to the nearest row: to its end when
    /// that row is above the band, to its start when it's below, so nothing past the
    /// band gets selected. Past the last row it still selects to the end.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn drag_position_at_point(
        &self,
        index: &HeightIndex,
        memo: &RefCell<GeometryMemo>,
        point: Point,
        bounds: Rectangle,
        vertical_offset: f32,
        horizontal_offset: f32,
        lock: Option<SplitSide>,
    ) -> Option<(TextPosition, Option<SplitSide>)> {
        let target_y = point.y - bounds.y + vertical_offset;
        let Some((row, above)) = self.band_snap(index, target_y) else {
            return self.position_at_point(
                index,
                memo,
                point,
                bounds,
                vertical_offset,
                horizontal_offset,
                lock,
            );
        };
        let &(file, hunk, line) = index.row_ids.get(row)?;
        let (file_index, hunk_index, mut line_index) =
            (file as usize, hunk as usize, line as usize);
        let mut lane = None;
        if !index.is_full_width(file_index)
            && self.props.side_by_side
            && let Some(&(left, right)) = index.pair_lines.get(row)
        {
            let in_right = match lock {
                Some(side) => side == SplitSide::Right,
                None => point.x >= bounds.x + self.split_layout(bounds.width).divider_x,
            };
            let (member, side) = split_member(left, right, in_right);
            if member == NO_LINE {
                return None;
            }
            line_index = member as usize;
            lane = Some(side);
        }
        let byte = if above {
            self.line(file_index, hunk_index, line_index)?.content.len()
        } else {
            0
        };
        Some((
            body_position(file_index, hunk_index, line_index, byte),
            lane,
        ))
    }

    /// Build the substring inside the selection range `[start, end)`, walking
    /// files/hunks/lines in document order so the pasted text reads naturally
    /// regardless of which direction the user dragged. `side` is the column a
    /// side-by-side selection lives in: lines the *other* column owns (deletions
    /// for a right-side selection, additions for a left-side one) aren't part of
    /// what the user sees selected, so they're skipped.
    pub(super) fn collect_selected_text(
        &self,
        start: TextPosition,
        end: TextPosition,
        side: Option<SplitSide>,
    ) -> String {
        if start == end {
            return String::new();
        }
        let mut output = String::new();
        let mut first = true;

        // Slice one logical line's selectable `text` against the selection and
        // append the covered part. Shared by the header and body passes.
        let mut emit = |text: &str, pos_start: TextPosition, pos_end: TextPosition| {
            if pos_end < start || pos_start >= end {
                return;
            }
            let line_start = if pos_start < start { start.byte } else { 0 };
            let line_end = if pos_end > end { end.byte } else { text.len() };
            let line_start = line_start.min(text.len());
            let line_end = line_end.min(text.len()).max(line_start);

            if !first {
                output.push('\n');
            }
            first = false;

            if text.is_char_boundary(line_start) && text.is_char_boundary(line_end) {
                output.push_str(&text[line_start..line_end]);
            }
        };

        // Header field values sort before the body (Region::Header < Body).
        for line_index in 0..self.props.header.len() {
            if let Some(text) = self.header_selectable_text(line_index) {
                emit(
                    text,
                    header_position(line_index, 0),
                    header_position(line_index, text.len()),
                );
            }
        }

        // Then the diff body, in document order.
        for (file_index, hunks) in self.hunks.iter().enumerate() {
            // Full-width (single-sided) files show every line regardless of the
            // selection's column, so nothing is filtered there.
            let hidden_kind = match side {
                Some(SplitSide::Left) => Some(DiffLineKind::Addition),
                Some(SplitSide::Right) => Some(DiffLineKind::Deletion),
                None => None,
            }
            .filter(|_| self.props.side_by_side && !file_is_single_sided(hunks));
            for (hunk_index, hunk) in hunks.iter().enumerate() {
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    if Some(line.kind) == hidden_kind {
                        continue;
                    }
                    emit(
                        &line.content,
                        body_position(file_index, hunk_index, line_index, 0),
                        body_position(file_index, hunk_index, line_index, line.content.len()),
                    );
                }
            }
        }
        output
    }

    pub(super) fn text_width(&self, content: &str) -> f32 {
        (content.chars().count() as f32 * self.metrics.char_width + 16.0).max(1.0)
    }

    /// Where file `file`'s header puts its pieces when drawn with its top at `y`:
    /// `[fold] [pick] [badge] dir/name … [N hunks] [+a −d] [source] [more]`. Shared
    /// by the painter and every hit test, so they can't disagree. The counts and
    /// then the hunk count give way before the path does.
    pub(super) fn file_header_layout(
        &self,
        file: usize,
        y: f32,
        bounds: Rectangle,
    ) -> Option<FileHeaderLayout> {
        let view = self.file(file)?;
        let height = self.metrics.file_header_height;
        let control = |x: f32, side: f32| Rectangle {
            x,
            y: (y + (height - side) / 2.0).round(),
            width: side,
            height: side,
        };
        let line_top = y + (height - self.metrics.row_height) / 2.0 + self.metrics.text_y_pad;
        let baseline = line_top
            + TextBox::new(
                self.props.font,
                self.props.typography.size,
                self.metrics.row_height,
            )
            .baseline();

        let mut x = bounds.x + FILE_HEADER_PAD_LEFT;
        let fold = control(x, FILE_CONTROL_SIZE);
        x += FILE_CONTROL_SIZE + FILE_HEADER_GAP;
        let check = self
            .props
            .marks
            .as_ref()
            .filter(|marks| marks.files.get(file).is_some())
            .map(|_| {
                let rect = control(x, CHECK_SIZE);
                x += CHECK_SIZE + FILE_HEADER_GAP;
                rect
            });
        // Centred on the path's capitals, as the controls beside it are on
        // the header: its smaller letter on the path's baseline would sit low.
        let path_line = TextBox::new(
            self.props.font,
            self.props.typography.size,
            self.metrics.row_height,
        );
        let badge = crate::status_badge::rect(x, path_line.center_for_baseline(baseline));
        x += badge.width + FILE_HEADER_GAP;
        let title_x = x;

        let mut right = bounds.x + bounds.width - FILE_HEADER_PAD_RIGHT;
        let more = self.props.on_file_menu.as_ref().map(|_| {
            right -= FILE_CONTROL_SIZE;
            control(right, FILE_CONTROL_SIZE)
        });
        let browse = self.props.on_browse_file.as_ref().map(|_| {
            right -= FILE_CONTROL_SIZE + if more.is_some() { 2.0 } else { 0.0 };
            control(right, FILE_CONTROL_SIZE)
        });
        if browse.is_some() || more.is_some() {
            right -= FILE_HEADER_META_GAP;
        }
        let meta = |text: &str| measure::line_width(text, self.meta_size(), self.props.font);
        let mut stats: Vec<(String, StatKind)> = Vec::new();
        if view.additions > 0 {
            stats.push((format!("+{}", view.additions), StatKind::Added));
        }
        if view.deletions > 0 {
            stats.push((format!("\u{2212}{}", view.deletions), StatKind::Deleted));
        }
        if stats.is_empty()
            && let Some(tag) = &view.tag
        {
            stats.push((tag.clone(), StatKind::Tag));
        }
        let stats_width = stats.iter().map(|(text, _)| meta(text)).sum::<f32>()
            + FILE_HEADER_STAT_GAP * stats.len().saturating_sub(1) as f32;
        let hunk_count = self.real_hunk_count(file);
        let hunks = (hunk_count > 0).then(|| {
            if hunk_count == 1 {
                "1 hunk".to_owned()
            } else {
                format!("{hunk_count} hunks")
            }
        });
        let hunks_width = hunks.as_deref().map_or(0.0, meta);
        // The path keeps at least this much before the meta gives way.
        let min_title = 12.0 * self.metrics.char_width;
        let fits = |used: f32| right - used - FILE_HEADER_GAP - title_x >= min_title;
        let with_stats = stats_width > 0.0 && fits(stats_width);
        let used_stats = if with_stats {
            stats_width + FILE_HEADER_META_GAP
        } else {
            0.0
        };
        let with_hunks = hunks_width > 0.0 && fits(used_stats + hunks_width);
        let mut placed_stats = Vec::new();
        if with_stats {
            let mut at = right - stats_width;
            for (text, kind) in stats {
                let width = meta(&text);
                placed_stats.push((text, at, kind));
                at += width + FILE_HEADER_STAT_GAP;
            }
            right -= used_stats;
        }
        let hunks = hunks.filter(|_| with_hunks).map(|text| {
            right -= hunks_width;
            let at = right;
            right -= FILE_HEADER_META_GAP;
            (text, at)
        });
        Some(FileHeaderLayout {
            fold,
            check,
            badge,
            title_x,
            title_right: (right - FILE_HEADER_GAP).max(title_x + 1.0),
            line_top,
            baseline,
            hunks,
            stats: placed_stats,
            browse,
            more,
        })
    }

    /// The size of a file header's counts and hunk count: a step under the code.
    pub(super) fn meta_size(&self) -> f32 {
        (self.props.typography.size - 1.0).max(9.0)
    }

    /// How many of `file`'s hunks are real ones, with rows: placeholders stand in
    /// for a binary or a held-back file.
    pub(super) fn real_hunk_count(&self, file: usize) -> usize {
        self.hunks.get(file).map_or(0, |hunks| {
            hunks.iter().filter(|hunk| !hunk.lines.is_empty()).count()
        })
    }

    /// Whether `file`'s body is folded away under its header.
    pub(super) fn is_folded(&self, file: usize) -> bool {
        self.props.folded.get(file).copied().unwrap_or(false)
    }

    /// `(file index, header screen y)` for every file header currently visible, with
    /// the sticky pinned header replacing its natural position: hit tests must match
    /// where headers actually draw.
    pub(super) fn visible_file_header_positions(
        &self,
        index: &HeightIndex,
        bounds: Rectangle,
        visible_top: f32,
    ) -> Vec<(usize, f32)> {
        if self.props.plain || self.metrics.file_header_height <= 0.0 {
            return Vec::new();
        }
        let visible_bottom = visible_top + bounds.height;
        let file_tops = &index.file_tops[..self.props.files.len().min(index.file_tops.len())];
        let mut out = Vec::new();
        let first =
            file_tops.partition_point(|&top| top + self.metrics.file_header_height < visible_top);
        for (file_index, &top) in file_tops.iter().enumerate().skip(first) {
            if top > visible_bottom {
                break;
            }
            out.push((file_index, bounds.y + (top - visible_top)));
        }
        // Mirror the painter's sticky pin: that file's header sits pinned, not at its
        // natural offset.
        let content_end = index.total_height;
        if let Some(sticky) = file_tops
            .partition_point(|&top| top <= visible_top)
            .checked_sub(1)
            .filter(|&i| file_tops[i] < visible_top)
        {
            let next_top = file_tops.get(sticky + 1).copied().unwrap_or(content_end);
            let pinned = visible_top.min(next_top - self.metrics.file_header_height);
            let y = bounds.y + (pinned - visible_top);
            if let Some(slot) = out.iter_mut().find(|(file, _)| *file == sticky) {
                slot.1 = y;
            } else {
                out.insert(0, (sticky, y));
            }
        }
        out
    }

    /// The file header under `point`, and the control in it the point is on.
    pub(super) fn file_header_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
    ) -> Option<(usize, Option<FileControl>)> {
        let (file, y) = self
            .visible_file_header_positions(index, bounds, vertical_offset)
            .into_iter()
            .find(|&(_, y)| point.y >= y && point.y < y + self.metrics.file_header_height)?;
        let layout = self.file_header_layout(file, y, bounds)?;
        let control = [
            (Some(layout.fold), FileControl::Fold),
            (layout.check.map(|rect| grow(rect, 3.0)), FileControl::Check),
            (layout.browse, FileControl::Browse),
            (layout.more, FileControl::More),
        ]
        .into_iter()
        .find_map(|(rect, control)| rect.filter(|rect| rect.contains(point)).map(|_| control));
        Some((file, control))
    }

    /// How much of hunk `(file, hunk)` is picked, or `None` when it has no checkbox.
    pub(super) fn hunk_mark(&self, file: usize, hunk: usize) -> Option<CheckState> {
        if self.props.plain {
            return None;
        }
        let mark = self.props.marks.as_ref()?.files.get(file)?;
        match &mark.hunks {
            HunkMarks::None => None,
            HunkMarks::Unpicked => (hunk < self.hunks.get(file)?.len()).then_some(CheckState::Off),
            HunkMarks::Picked(picks) => Some(match picks.get(hunk)? {
                HunkMark::Off => CheckState::Off,
                HunkMark::On => CheckState::On,
                HunkMark::Lines(_) => CheckState::Partial,
            }),
        }
    }

    pub(super) fn line_mode(&self) -> Option<&'a LineMode> {
        self.props.marks.as_ref()?.lines.as_ref()
    }

    /// Whether the changed line `at` is picked, or `None` when it shows no pick: it
    /// is not a changed line of a pickable hunk, or line-select mode is off and its
    /// hunk is picked all or nothing (the checkbox says it).
    pub(super) fn line_mark(&self, at: LineAt) -> Option<bool> {
        if self.props.plain {
            return None;
        }
        let marks = self.props.marks.as_ref()?;
        let line = self.line(at.file, at.hunk, at.line)?;
        if !matches!(line.kind, DiffLineKind::Addition | DiffLineKind::Deletion) {
            return None;
        }
        let selecting = marks.lines.is_some();
        match &marks.files.get(at.file)?.hunks {
            HunkMarks::None => None,
            HunkMarks::Unpicked => selecting.then_some(false),
            HunkMarks::Picked(picks) => match picks.get(at.hunk)? {
                HunkMark::Off => selecting.then_some(false),
                HunkMark::On => selecting.then_some(true),
                HunkMark::Lines(lines) => Some(lines.get(at.line).copied().unwrap_or(false)),
            },
        }
    }

    /// The pickable changed line under `point` in line-select mode, and the
    /// side-by-side column it is in. `lock` holds a split row to one column, as a
    /// drag that started there does.
    pub(super) fn pickable_line_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
        lock: Option<Option<SplitSide>>,
    ) -> Option<(LineAt, Option<SplitSide>)> {
        self.line_mode()?;
        if !bounds.contains(point) {
            return None;
        }
        let over_header = self
            .visible_file_header_positions(index, bounds, vertical_offset)
            .into_iter()
            .any(|(_, y)| point.y >= y && point.y < y + self.metrics.file_header_height);
        if over_header {
            return None;
        }
        let content_y = point.y - bounds.y + vertical_offset;
        let row = index.row_at(content_y)?;
        if content_y >= index.row_tops[row] + index.row_heights[row] {
            return None;
        }
        let (file, hunk, line) = index.row_ids[row];
        let (file, hunk) = (file as usize, hunk as usize);
        let (line, lane) = match index.pair_lines.get(row) {
            Some(&(left, right)) if self.props.side_by_side && !index.is_full_width(file) => {
                let lane = lock.flatten().unwrap_or(
                    if point.x >= bounds.x + self.split_layout(bounds.width).divider_x {
                        SplitSide::Right
                    } else {
                        SplitSide::Left
                    },
                );
                let member = match lane {
                    SplitSide::Left => left,
                    SplitSide::Right => right,
                };
                if member == NO_LINE {
                    return None;
                }
                (member as usize, Some(lane))
            }
            _ => (line as usize, None),
        };
        let at = LineAt { file, hunk, line };
        self.line_mark(at).map(|_| (at, lane))
    }

    /// The file header checkbox under `point`, if any (the pinned sticky header
    /// included).
    pub(super) fn check_at(
        &self,
        index: &HeightIndex,
        vertical_offset: f32,
        bounds: Rectangle,
        point: Point,
    ) -> Option<CheckHit> {
        self.props.marks.as_ref()?;
        if self.props.plain {
            return None;
        }
        let (file, control) = self.file_header_at(index, vertical_offset, bounds, point)?;
        (control == Some(FileControl::Check)).then_some(CheckHit::File(file))
    }
}

/// True when every line of the file sits on one side of the diff: all additions
/// or all deletions (context appears on both sides, so its presence makes the file
/// two-sided). Such a file renders as a single full-width column in split mode:
/// there is nothing to mirror, and splitting it would waste half the pane on
/// padding.
///
/// `Note` lines are skipped rather than counted: `\ No newline at end of file`
/// belongs to the line above it, not to a side, and counting it made every new file
/// that lacks a trailing newline render split with one column fully hatched.
pub(super) fn file_is_single_sided(hunks: &[DiffHunkView]) -> bool {
    let mut kinds = hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .map(|line| line.kind)
        .filter(|kind| *kind != DiffLineKind::Note);
    let Some(first) = kinds.next() else {
        return false;
    };
    matches!(first, DiffLineKind::Addition | DiffLineKind::Deletion)
        && kinds.all(|kind| kind == first)
}
