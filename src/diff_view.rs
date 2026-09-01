use std::cell::RefCell;
use std::time::Instant;

use iced::advanced::{
    Layout, Shell, Widget,
    graphics::geometry::{self, Frame, LineCap, Path, Stroke},
    layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{
    Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size, Theme,
    Vector, alignment, keyboard, window,
};

use crate::chip::{self, Chip};
use crate::icons;
use crate::measure;
use crate::scrollbar::{self, ScrollbarState, ScrollbarStyle};

// Row height is `text_size * line_height` rounded to the nearest int, so
// glyphs (which iced renders at `text_size * 1.4` line height) sit inside
// the row with a few px of breathing room above and below. The default
// factor (`config::DEFAULT_CODE_LINE_HEIGHT`, 1.85) gives the historical
// fixed row height at the default code font size; the user can widen or
// tighten it via `code_line_height`.
// Padding above and below the centered title row inside the file-header strip.
const FILE_HEADER_VPAD: f32 = 8.0;
// Padding above and below the centered title row inside the hunk-header strip.
const HUNK_HEADER_VPAD: f32 = 3.0;
const PREFIX_WIDTH: f32 = 24.0;
const TEXT_X_PADDING: f32 = 8.0;
/// Baseline text drop within a default-ratio row. A custom `code_line_height`
/// splits its extra (or removed) leading evenly around this, and
/// `code_baseline_offset` shifts it — see [`LayoutMetrics::new`].
const TEXT_Y_PADDING: f32 = 2.0;
// Rows advanced per wheel-notch line on Linux/Windows (macOS trackpads take the
// pixel path below). Above the ~3-line OS default — browsing a long diff with a
// wheel felt sluggish otherwise.
const LINE_SCROLL_ROWS: f32 = 5.0;
const PIXEL_SCROLL_SCALE: f32 = 0.5;
// A pause this long between wheel events ends a scroll gesture, unlocking the
// axis the gesture was pinned to. Touchpads stream events every few ms while
// a finger moves (inertia included), so a real gap means a new intent.
const SCROLL_GESTURE_GAP_MS: u64 = 200;

/// Which axis a scroll gesture is locked to (see `State::scroll_axis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}
// Corner radius of the intra-line word-diff emphasis rectangles (find
// match highlights share it), so token tints read as soft chips rather
// than hard blocks.
const EMPHASIS_CORNER_RADIUS: f32 = 3.0;
// Floor for the gutter so two single-digit columns still look intentional.
const GUTTER_MIN_WIDTH: f32 = 56.0;
// Padding flanking the gutter text on both sides.
const GUTTER_HORIZONTAL_PADDING: f32 = 8.0;
// Padding above and below the revision-header block when it's present.
pub(crate) const HEADER_VERTICAL_PADDING: f32 = 12.0;
// Left/right padding inside the header block (between the panel edge and
// the first character of label/description text).
pub(crate) const HEADER_HORIZONTAL_PADDING: f32 = 16.0;
// Space drawn between the label column and the value column.
const HEADER_LABEL_GAP: f32 = 8.0;
// Description lines lead the header as its title, flush with the labels.
const HEADER_DESCRIPTION_INDENT: f32 = 0.0;
const HEADER_EDIT_WIDTH: f32 = 66.0;
const HEADER_EDIT_HEIGHT: f32 = 26.0;
const HEADER_EDIT_ICON_SIZE: f32 = 13.0;
const HEADER_EDIT_LABEL_SIZE: f32 = 12.0;
const HEADER_EDIT_CONTENT_GAP: f32 = 5.0;
// Click within this distance of the previous click counts as a multi-click
// rather than a fresh selection anchor.
const MULTI_CLICK_RADIUS: f32 = 4.0;
// Auto-scroll speed (px/sec) when the mouse is dragged just outside the
// viewport. Scaled by how far past the edge the cursor sits.
const AUTO_SCROLL_MAX_SPEED: f32 = 1200.0;
// Width of the "edge zone" outside the viewport over which the auto-scroll
// speed ramps up. The cursor sitting just past the edge scrolls slowly; a
// cursor far outside the viewport scrolls at full speed.
const AUTO_SCROLL_RAMP_PX: f32 = 80.0;
// Square hit target of the per-file "browse source" button in file headers.
const BROWSE_BUTTON_SIZE: f32 = 22.0;
// Glyph size of the browse button's icon within that square.
const BROWSE_ICON_SIZE: f32 = 13.0;

// The diff data model lives in `diffui_core`; re-export the types this widget
// renders so `crate::diff_view::DiffLine` etc. still resolve for callers.
pub use diffui_core::{
    DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind, SyntaxKind, SyntaxSpan,
};

#[derive(Debug, Clone)]
pub struct DiffFileView<'a> {
    pub title: String,
    pub status: DiffFileStatus,
    /// Saturated status color (chip glyph, matching the sidebar's file list).
    pub status_color: Color,
    /// Translucent tint behind the status letter, pre-resolved by the caller
    /// (`chip_background(status_color)`) so this widget stays theme-agnostic.
    pub status_fill: Color,
    pub hunks: &'a [DiffHunkView],
    pub additions: usize,
    pub deletions: usize,
}

/// One line of the `jj show`-style revision header rendered at the top of
/// the diff scroll area. Kept as a plain enum so `main.rs` decides the text
/// content without leaking `RevisionDetails` into this module.
#[derive(Debug, Clone)]
pub enum HeaderLine {
    /// "label  value" — label colored muted, value colored as text. The
    /// `label` field is padded to the column width by the caller so values
    /// stack in a column across the block (no colons — the color split
    /// already separates label from value).
    Field { label: String, value: String },
    /// Bookmarks rendered as colored chips that match the sidebar. The chips'
    /// colors are resolved by the caller (the selected commit's lane color);
    /// remote `name@remote` bookmarks render outlined.
    Bookmarks { label: String, chips: Vec<Chip> },
    /// A line of the description. Leads the header as its title — the "what
    /// is this change" — with the metadata block following below.
    Description(String),
    /// Blank separator between the description and the metadata block.
    Blank,
    /// Fixed-height space occupied by an interactive child layered over the
    /// custom-rendered header (currently the description editor).
    Spacer(f32),
}

impl HeaderLine {
    /// Build a metadata row with the label padded to nine characters — the
    /// width of "committer" / "bookmarks" / "signature", the longest labels
    /// we ship — so values stack in a column across the block.
    pub fn field(label: &str, value: &str) -> Self {
        Self::Field {
            label: format!("{label:<9}"),
            value: value.to_owned(),
        }
    }

    /// Bookmarks row — `label` padded like `field` so it aligns with the
    /// metadata column.
    pub fn bookmarks(label: &str, chips: Vec<Chip>) -> Self {
        Self::Bookmarks {
            label: format!("{label:<9}"),
            chips,
        }
    }

    pub fn description(line: &str) -> Self {
        Self::Description(line.to_owned())
    }

    pub fn blank() -> Self {
        Self::Blank
    }

    pub fn spacer(height: f32) -> Self {
        Self::Spacer(height)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub text: Color,
    pub text_muted: Color,
    pub addition_text: Color,
    pub deletion_text: Color,
    pub modified_token: Color,
    pub conflict_marker: Color,
    pub note_text: Color,
    pub syntax_keyword: Color,
    pub syntax_type: Color,
    pub syntax_function: Color,
    pub syntax_literal: Color,
    pub syntax_property: Color,
    pub panel: Color,
    pub file_header: Color,
    pub hunk_header: Color,
    pub addition_background: Color,
    pub deletion_background: Color,
    /// Stronger tint over the changed tokens *inside* a modified line
    /// (intra-line word diff), layered on the add/del line backgrounds.
    pub addition_emphasis: Color,
    pub deletion_emphasis: Color,
    pub note_background: Color,
    pub gutter_background: Color,
    pub border: Color,
    /// Background tint drawn under selected text.
    pub selection: Color,
    pub scrollbar: ScrollbarStyle,
}

pub struct DiffView<'a, Message> {
    files: Vec<DiffFileView<'a>>,
    selected_file: usize,
    revision_key: String,
    palette: Palette,
    font: Font,
    typography: crate::config::CodeTypography,
    multi_click_ms: u64,
    metrics: LayoutMetrics,
    header: Vec<HeaderLine>,
    on_selected_file_changed: fn(usize) -> Message,
    on_copy: Option<fn(String) -> Message>,
    /// In-diff find highlights. `None` when the find bar is closed.
    find: Option<FindOverlay<'a>>,
    /// Reports the scroll offset whenever it changes, so the app can persist it
    /// per-tab. `None` ⇒ scroll position isn't tracked.
    on_scroll: Option<fn(f32) -> Message>,
    /// One-shot scroll restore: when `restore_token` differs from the value the
    /// widget last saw, the offset jumps to `restore_offset` (clamped),
    /// overriding the reset-to-top a `revision_key` change would otherwise do.
    /// Used to re-apply a tab's saved scroll on activation.
    restore_offset: f32,
    restore_token: u64,
    /// Monotonic version of the diff content's *paint*, bumped by the app on
    /// every document swap and on background-highlight merges. The per-line
    /// paragraph cache is keyed by `(file, hunk, line)`, which point at
    /// *different* text after a reload, a working-copy edit, or a tab switch
    /// (the widget `State` — cache included — is shared across tabs).
    /// `revision_key` alone can't catch those: it's a constant
    /// `"working-copy"` for any `@`. A version bump drops the cache.
    content_version: u64,
    /// Whether long lines wrap (see [`Self::wrap`]).
    wrap: bool,
    /// Two-column old/new layout (see [`Self::side_by_side`]).
    side_by_side: bool,
    /// Monotonic identity of the document's *layout* (the app's per-document
    /// id), bumped only when the document is replaced — not when highlight
    /// spans merge in. Keys the [`HeightIndex`]: span merges repaint rows but
    /// never move them, so rebuilding the (potentially 1M-row) index for each
    /// would be pure waste.
    layout_version: u64,
    /// Plain source-document rendering (the source browser): no file/hunk
    /// header strips and a single line-number gutter column. See
    /// [`Self::plain`].
    plain: bool,
    /// Per-file "browse source" affordance drawn in each file header
    /// (diff mode only — plain documents have no headers). Receives the
    /// file index on click.
    on_browse_file: Option<fn(usize) -> Message>,
    /// Opens the selected revision's description editor. The custom widget
    /// draws and hit-tests the affordance beside the first description line;
    /// the actual editor is layered by the caller into the reserved spacer.
    on_edit_description: Option<fn() -> Message>,
}

/// Per-render find-match data fed into `DiffView::with_find`. The widget
/// renders one rectangle per match on the lines it overlaps and scrolls
/// the active match into view when `scroll_token` changes between
/// renders.
#[derive(Debug, Clone)]
pub struct FindOverlay<'a> {
    pub matches: &'a [crate::find::FindMatch],
    pub active: Option<usize>,
    /// Bumped by the caller whenever the active match changes. The widget
    /// keeps the previous token in its `State` and scrolls when the two
    /// disagree — this is how we trigger scroll without tearing into the
    /// widget's private state from outside.
    pub scroll_token: u64,
    pub highlight: Color,
    pub active_highlight: Color,
}

/// Sizes derived once at widget-construction time from the caller's
/// `text_size`. Replaces the historical fixed `self.metrics.row_height` /
/// `self.metrics.file_header_height` / `self.metrics.hunk_header_height` constants so the diff view
/// stays visually proportional under a non-default code font size.
#[derive(Debug, Clone, Copy)]
struct LayoutMetrics {
    row_height: f32,
    /// Vertical offset of a row's text below its top edge. The historical
    /// [`TEXT_Y_PADDING`] at the default typography, shifted by half of any
    /// configured extra leading plus the user's baseline offset.
    text_y_pad: f32,
    file_header_height: f32,
    hunk_header_height: f32,
    gutter_width: f32,
    gutter_digit_count: usize,
    /// Real monospace glyph advance for the diff font + size, measured
    /// once via cosmic_text at construction. Wrap-line counting (in
    /// `row_height`) and selection geometry (in `draw()` /
    /// `position_at_point`) both rely on this — agreeing on a single
    /// value keeps row heights aligned with where iced actually breaks
    /// long lines, so we don't reserve an empty wrap row before the
    /// renderer actually wraps.
    char_width: f32,
}

impl LayoutMetrics {
    fn new(
        typography: crate::config::CodeTypography,
        gutter_digit_count: usize,
        font: Font,
        plain: bool,
    ) -> Self {
        let text_size = typography.size;
        let row_height = (text_size * typography.line_height).round();
        // Pixel-identical to the fixed TEXT_Y_PADDING at default typography;
        // a custom line height splits its leading delta evenly above/below,
        // and the baseline offset then nudges the result.
        let default_row_height = (text_size * crate::config::DEFAULT_CODE_LINE_HEIGHT).round();
        let text_y_pad =
            TEXT_Y_PADDING + (row_height - default_row_height) / 2.0 + typography.baseline_offset;
        // Plain (source-browse) documents render no file/hunk header strips;
        // zero heights keep the prefix-sum index, hit tests, and draw all
        // consistent without a parallel layout mode.
        let (file_header_height, hunk_header_height) = if plain {
            (0.0, 0.0)
        } else {
            (
                row_height + 2.0 * FILE_HEADER_VPAD,
                row_height + 2.0 * HUNK_HEADER_VPAD,
            )
        };
        // Headless cosmic_text shaping gives the actual glyph advance, so
        // the row-height wrap math matches iced's renderer instead of the
        // historical `text_size * 0.62` heuristic that consistently
        // under-counted chars-per-line and produced phantom trailing
        // wrap rows just before the renderer hit its real break point.
        // Cached per (font, size) — iced rebuilds the widget on every
        // `view()` cycle, and uncached this re-shapes "M" each time
        // (~40µs release / ~450µs debug per rebuild).
        let char_width = measure_char_advance_cached(font, text_size).max(1.0);
        // Two line-number columns + a separating space; plain documents have
        // no old side, so a single column suffices.
        let gutter_text_chars = if plain {
            gutter_digit_count
        } else {
            gutter_digit_count * 2 + 1
        };
        let gutter_min = if plain {
            GUTTER_MIN_WIDTH / 2.0
        } else {
            GUTTER_MIN_WIDTH
        };
        let gutter_width = (gutter_text_chars as f32 * char_width
            + GUTTER_HORIZONTAL_PADDING * 2.0)
            .max(gutter_min);
        Self {
            row_height,
            text_y_pad,
            file_header_height,
            hunk_header_height,
            gutter_width,
            gutter_digit_count,
            char_width,
        }
    }
}

/// Cache key for per-line shaped `Paragraph`s. The `(file, hunk, line)`
/// triple maps to stable line content under a fixed `revision_key`
/// (`diff()` clears the cache when that changes). `content_width_bits`
/// invalidates entries when the panel is resized — wrap points move and
/// the old layout would no longer match the renderer's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ParagraphKey {
    file_index: u32,
    hunk_index: u32,
    line_index: u32,
    content_width_bits: u32,
}

/// Prefix-sum layout index over the rendered document — the "position
/// checkpoint" idea from pierre.computer's diff-rendering writeup. Built once
/// per `(content shape, wrap width)` and consulted by every height/position
/// query, so scroll frames, file jumps, and hit tests are O(log n) binary
/// searches instead of O(total lines) walks re-counting every line's chars
/// (which at ~1M lines costs hundreds of ms *per frame*).
///
/// ~16 bytes per row — ~16 MB at 1M lines, an order of magnitude under the
/// lines themselves.
#[derive(Debug, Default)]
struct HeightIndex {
    /// Rebuild key: `(layout_version, files.len(), header.len(),
    /// content_width bits)`. The file/header counts are part of the key
    /// because streaming PR loads *append* files (and the header appears when
    /// `gh pr view` lands) without replacing the document — appends leave
    /// existing rows in place, but they extend the layout. Keyed on the
    /// layout id, NOT the paint `content_version`: highlight merges bump the
    /// latter to re-shape paint without moving anything. The wrap and
    /// side-by-side flags are in the key because toggling either moves
    /// every row.
    key: Option<(u64, usize, usize, u32, bool, bool)>,
    /// Content-space y of each file's header top, plus one trailing sentinel
    /// holding the content end. `file_tops[0]` equals the revision-header
    /// height.
    file_tops: Vec<f32>,
    /// Content-space y of each hunk header, in document order.
    hunk_tops: Vec<f32>,
    /// `(file, hunk)` of each `hunk_tops` entry.
    hunk_ids: Vec<(u32, u32)>,
    /// Content-space y of each diff row, in document order.
    row_tops: Vec<f32>,
    /// `(file, hunk, line)` of each `row_tops` entry. Document order makes
    /// this lexicographically sorted, so a row is also findable *by id*. In
    /// side-by-side mode the line component is the pair's *first* member
    /// (which preserves the sort); the full pair lives in `pair_lines`.
    row_ids: Vec<(u32, u32, u32)>,
    /// Side-by-side only (empty in unified mode): each row's `(left, right)`
    /// line indices into its hunk, [`NO_LINE`] for a padded side. Context /
    /// note rows carry the same line on both sides; rows of full-width files
    /// carry `(line, line)` purely to keep this aligned with `row_tops`.
    pair_lines: Vec<(u32, u32)>,
    /// Side-by-side only: files rendered as a single full-width column even
    /// in split mode. A single-sided file (all additions or all deletions)
    /// has nothing to mirror, so splitting it would waste half the pane on
    /// padding. Empty in unified mode.
    full_width_files: Vec<bool>,
    /// Wrap width of one split column's text area; what split-file rows were
    /// measured against. Equals `unified_text_width` in unified mode.
    split_text_width: f32,
    /// Wrap width of the whole text area; what unified-mode and full-width
    /// rows were measured against.
    unified_text_width: f32,
    /// Longest line of the document in chars — the horizontal extent the
    /// no-wrap mode can scroll across. (Wrap mode never scrolls sideways.)
    max_line_chars: usize,
    /// Total content height (revision header + every file).
    total_height: f32,
}

/// `pair_lines` sentinel: this side of the row has no line (padding).
const NO_LINE: u32 = u32::MAX;

impl HeightIndex {
    /// True when `file` renders as one full-width column despite split mode.
    fn is_full_width(&self, file: usize) -> bool {
        self.full_width_files.get(file).copied().unwrap_or(false)
    }

    /// The wrap width `file`'s rows were measured against: one split column
    /// for split files, the whole text area for unified/full-width ones.
    fn text_width_for_file(&self, file: usize) -> f32 {
        if self.is_full_width(file) {
            self.unified_text_width
        } else {
            self.split_text_width
        }
    }

    /// Index into `row_tops`/`row_ids` of the last row starting at or above
    /// `target_y` — the candidate row containing that y (the caller checks
    /// the row's actual height; `target_y` may sit in a header band).
    fn row_at(&self, target_y: f32) -> Option<usize> {
        self.row_tops
            .partition_point(|&top| top <= target_y)
            .checked_sub(1)
    }

    /// Index of row `(file, hunk, line)`, by binary search over the sorted ids.
    fn row_index_of(&self, file: usize, hunk: usize, line: usize) -> Option<usize> {
        self.row_ids
            .binary_search(&(file as u32, hunk as u32, line as u32))
            .ok()
    }

    /// Index of the row *containing* `(file, hunk, line)` on either side. In
    /// unified mode this is [`Self::row_index_of`]; in side-by-side a right
    /// member's row is keyed by its left partner, so walk back from the last
    /// row at or before the id until a pair carries the line. The walk is
    /// bounded by one change run (an addition can only pair leftward within
    /// its own run).
    fn row_of_line(&self, file: usize, hunk: usize, line: usize) -> Option<usize> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionUnit {
    Character,
    Word,
    Line,
}

struct State<Paragraph> {
    selected_file: usize,
    revision_key: String,
    pending_file_jump: Option<usize>,
    /// (file, hunk, line, byte_in_line) of a match we should scroll into
    /// view on the next pass. Set in `diff()` when the incoming find
    /// scroll_token differs from `last_find_scroll_token`; consumed in
    /// `update()` where layout bounds are available.
    pending_find_scroll: Option<(usize, usize, usize, usize)>,
    /// Last find scroll token we acted on. The widget compares this against
    /// the incoming `FindOverlay::scroll_token` and, when they disagree,
    /// schedules a scroll. Wrapping `u64` is fine — only equality matters.
    last_find_scroll_token: Option<u64>,
    vertical_offset: f32,
    /// Horizontal scroll of the code text in no-wrap mode (px). Forced to
    /// zero while wrapping — wrapped content never exceeds the pane. The
    /// gutter/prefix chrome stays fixed; only text and its highlight rects
    /// shift. Side-by-side scrolls both columns in lockstep.
    horizontal_offset: f32,
    /// Wrap flag last seen, to zero `horizontal_offset` when wrap turns on.
    last_wrap: bool,
    /// Axis a two-dimensional scroll gesture locked onto, IDE-style: the
    /// first wheel event of a gesture picks the dominant axis and the other
    /// component is discarded until the gesture ends (a pause in events —
    /// see [`SCROLL_GESTURE_GAP_MS`]). Keeps touchpad panning from drifting
    /// diagonally in no-wrap mode. `None` between gestures.
    scroll_axis: Option<ScrollAxis>,
    /// When the last wheel event arrived — a gap ends the gesture.
    last_scroll_at: Option<Instant>,
    /// Live keyboard modifiers, tracked so the wheel handler can redirect
    /// shift+scroll into horizontal panning.
    modifiers: keyboard::Modifiers,
    /// Shaped `Paragraph`s for syntax-highlighted code lines, keyed by
    /// `(file, hunk, line, content_width)`. Reused across frames during
    /// scrolling — `with_spans` shaping is ~280µs/row in release and
    /// dominates the per-frame draw cost on dense diffs. Cleared in
    /// `diff()` whenever `revision_key` changes (line indices stop
    /// pointing at the same text) and unused entries are evicted at the
    /// end of each `draw()`.
    paragraph_cache: RefCell<std::collections::HashMap<ParagraphKey, Paragraph>>,
    /// Anchor (mouse-down position) of the in-progress or completed selection.
    selection_anchor: Option<TextPosition>,
    /// Focus (current/release position) of the selection.
    selection_focus: Option<TextPosition>,
    /// Side-by-side only: the column the selection lives in, locked at
    /// mouse-down. Drags stay in this column, the highlight draws only
    /// there, and copy skips the other column's lines. `None` for unified
    /// mode, header, and full-width-file selections.
    selection_lane: Option<SplitSide>,
    /// When the user drags after a double/triple click, the selection grows
    /// in word- or line-sized chunks instead of by character. We remember
    /// where the anchor "click unit" started/ended so the resulting
    /// selection always covers full units in both directions.
    selection_anchor_unit_start: Option<TextPosition>,
    selection_anchor_unit_end: Option<TextPosition>,
    selection_unit: SelectionUnit,
    /// True while the user is mid-drag.
    is_selecting: bool,
    /// Multi-click bookkeeping: timestamp + screen position + count of the
    /// last left-button press, so a second/third press in quick succession
    /// at the same spot upgrades the selection to word/line scope.
    last_click_time: Option<Instant>,
    last_click_screen: Option<Point>,
    click_count: u32,
    /// Last cursor position observed while dragging — used to drive
    /// auto-scroll and to reproject the selection focus after the viewport
    /// shifts under the cursor.
    last_drag_cursor: Option<Point>,
    scrollbar: ScrollbarState,
    /// Most recent `restore_token` acted on; a change schedules a one-shot jump
    /// to the caller's `restore_offset`, consumed in `update()` where bounds
    /// clamp. Starts at 0 to match the caller's initial token.
    last_restore_token: u64,
    /// Offset to jump to on the next `update()` pass, set when `restore_token`
    /// changed. Overrides the `revision_key`-change reset and file jump;
    /// clamped once bounds are known.
    pending_set_offset: Option<f32>,
    /// Most recent `content_version` acted on. A change drops the paragraph
    /// cache (whose keys would otherwise map to stale text). Starts at 0 to
    /// match the app's initial version.
    last_content_version: u64,
    /// Last revision-header height. Opening/closing the embedded editor changes
    /// header row geometry without changing the revision key; selections use
    /// header row indices, so they must be cleared when this shape changes.
    last_header_height_bits: u32,
    /// Palette identity last seen. Cached paragraphs bake the syntax span
    /// colors in at shaping time, so a theme switch must drop the cache or
    /// stale-theme text keeps rendering (near-invisible when the old theme's
    /// text color lands on the new theme's background). `text` alone
    /// distinguishes every built-in theme.
    last_palette_text: Option<Color>,
    /// File whose header's browse button the cursor is over, if any —
    /// drives its hover wash (manual, like every hover in this widget).
    hovered_browse: Option<usize>,
    /// Whether the cursor is over the description block. Reveals the edit
    /// affordance at its right edge.
    hovered_description: bool,
    /// Prefix-sum layout index (see [`HeightIndex`]). `RefCell` because
    /// `draw`/`mouse_interaction` only get `&State` but must be able to
    /// (re)build it lazily; borrows are short and never overlap.
    height_index: RefCell<HeightIndex>,
}

/// Stable cursor position inside the diff document. We index by
/// `(file, hunk, line)` instead of by screen y so the position stays valid
/// across scrolling, file selection, and re-renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPosition {
    /// Which part of the document the position is in. Ordered first so every
    /// header position sorts before every body position — the revision header
    /// sits above the diff body.
    region: Region,
    /// `Body`: the file index. Unused (`0`) for `Header`.
    file_index: usize,
    /// `Body`: the hunk index. Unused (`0`) for `Header`.
    hunk_index: usize,
    /// `Body`: the line within the hunk. `Header`: the header-line index.
    line_index: usize,
    /// Byte offset within the line's selectable text. Bytes (not chars) so we
    /// can slice the source string directly when copying.
    byte: usize,
}

/// Which selectable region of the diff document a `TextPosition` lives in.
/// `Header` (the `jj show`-style revision metadata) sorts before `Body` (the
/// file diffs), matching their visual stacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Region {
    Header,
    Body,
}

fn header_position(line_index: usize, byte: usize) -> TextPosition {
    TextPosition {
        region: Region::Header,
        file_index: 0,
        hunk_index: 0,
        line_index,
        byte,
    }
}

fn body_position(
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

#[derive(Debug, Clone, Copy)]
struct RowRenderParams {
    /// Horizontal scroll (px) of the code text in no-wrap mode; the
    /// gutter/prefix chrome stays fixed.
    horizontal_offset: f32,
    bounds: Rectangle,
    content_clip_bounds: Rectangle,
    y: f32,
    height: f32,
    content_width: f32,
}

#[derive(Debug, Clone, Copy)]
struct TextRenderParams {
    width: f32,
    height: f32,
    position: Point,
    color: Color,
    clip_bounds: Rectangle,
    wrapping: text::Wrapping,
}

#[derive(Debug, Clone, Copy)]
struct VisibleHunkHeader {
    file_index: usize,
    hunk_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleFileHeader {
    file_index: usize,
    y: f32,
}

#[derive(Debug, Clone, Copy)]
struct VisibleRow {
    file_index: usize,
    hunk_index: usize,
    line_index: usize,
    /// The row's `(left, right)` pair ([`NO_LINE`] for a padded side) when
    /// it renders as two split columns; `None` when it renders full-width —
    /// unified mode, or a full-width (single-sided) file in split mode.
    pair: Option<(u32, u32)>,
    y: f32,
    height: f32,
}

/// One column of the side-by-side layout, or the whole text area in unified
/// mode (`None`). Selection/find/emphasis passes iterate per lane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitSide {
    Left,
    Right,
}

impl VisibleRow {
    /// The row's line shown in `lane`, or `None` when that side is padding
    /// or the row doesn't participate in the lane at all (full-width rows
    /// live only in the `None` lane; split rows only in the column lanes).
    /// Context (and note) rows appear in both column lanes.
    fn line_in_lane(&self, lane: Option<SplitSide>) -> Option<usize> {
        match (lane, self.pair) {
            (None, None) => Some(self.line_index),
            (Some(SplitSide::Left), Some((left, _))) if left != NO_LINE => Some(left as usize),
            (Some(SplitSide::Right), Some((_, right))) if right != NO_LINE => Some(right as usize),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct VisibleBand {
    kind: DiffLineKind,
    y: f32,
    height: f32,
}

impl<'a, Message> DiffView<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        files: Vec<DiffFileView<'a>>,
        selected_file: usize,
        revision_key: impl Into<String>,
        palette: Palette,
        font: Font,
        typography: crate::config::CodeTypography,
        multi_click_ms: u64,
        on_selected_file_changed: fn(usize) -> Message,
    ) -> Self {
        let gutter_digit_count = compute_gutter_digit_count(&files);
        let metrics = LayoutMetrics::new(typography, gutter_digit_count, font, false);
        Self {
            files,
            selected_file,
            revision_key: revision_key.into(),
            palette,
            font,
            typography,
            multi_click_ms,
            metrics,
            header: Vec::new(),
            on_selected_file_changed,
            on_copy: None,
            find: None,
            on_scroll: None,
            restore_offset: 0.0,
            restore_token: 0,
            content_version: 0,
            layout_version: 0,
            wrap: true,
            side_by_side: false,
            plain: false,
            on_browse_file: None,
            on_edit_description: None,
        }
    }

    /// Render as a plain source document (the source browser): no file/hunk
    /// header strips, a single line-number column, and never side-by-side.
    pub fn plain(mut self, plain: bool) -> Self {
        if self.plain != plain {
            self.plain = plain;
            self.metrics = LayoutMetrics::new(
                self.typography,
                self.metrics.gutter_digit_count,
                self.font,
                plain,
            );
        }
        if plain {
            // A source document has one side; a split layout would just
            // mirror it. Forced off so a caller can't combine the two.
            self.side_by_side = false;
        }
        self
    }

    /// Draw a "browse source" icon button in every file header, firing with
    /// the file index when clicked. Ignored for plain documents.
    pub fn on_browse_file(mut self, callback: fn(usize) -> Message) -> Self {
        self.on_browse_file = Some(callback);
        self
    }

    pub fn with_header(mut self, header: Vec<HeaderLine>) -> Self {
        self.header = header;
        self
    }

    pub fn on_edit_description(mut self, callback: fn() -> Message) -> Self {
        self.on_edit_description = Some(callback);
        self
    }

    /// Whether long lines wrap into extra visual lines (the default) or clip
    /// at the pane edge, leaving every row exactly one line tall.
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Two-column layout: deletions/context on the left, additions/context
    /// on the right, paired index-wise within each change run and padded
    /// where one side is longer. Off (the default) is the unified view.
    /// Ignored for plain documents (see [`Self::plain`]).
    pub fn side_by_side(mut self, side_by_side: bool) -> Self {
        self.side_by_side = side_by_side && !self.plain;
        self
    }

    /// Report scroll-offset changes so the caller can persist the position.
    /// See [`Self::restore_scroll`] for the inverse.
    pub fn on_scroll(mut self, callback: fn(f32) -> Message) -> Self {
        self.on_scroll = Some(callback);
        self
    }

    /// Jump the scroll offset to `offset` the next time `token` changes. Calling
    /// with the same `token` twice is a no-op, so live scrolling is preserved.
    /// Wins over the `revision_key`-change reset in the same render — a tab
    /// restore wants the exact saved offset, not the top.
    pub fn restore_scroll(mut self, offset: f32, token: u64) -> Self {
        self.restore_offset = offset;
        self.restore_token = token;
        self
    }

    /// Set the diff content's version. A change since the widget last saw it
    /// drops the per-line paragraph cache (see the field docs). The app bumps
    /// this whenever it swaps the displayed document.
    pub fn content_version(mut self, version: u64) -> Self {
        self.content_version = version;
        self
    }

    /// Set the document's layout identity (see the field docs) — keys the
    /// height index, unlike `content_version` which keys the paint cache.
    /// Span merges bump only the latter, so the (potentially 1M-row) index
    /// survives them.
    pub fn layout_version(mut self, version: u64) -> Self {
        self.layout_version = version;
        self
    }

    pub fn on_copy(mut self, on_copy: fn(String) -> Message) -> Self {
        self.on_copy = Some(on_copy);
        self
    }

    pub fn with_find(mut self, find: FindOverlay<'a>) -> Self {
        self.find = Some(find);
        self
    }

    /// Total height of the revision header block (metadata lines +
    /// description + vertical padding). Zero when no header is set.
    fn header_height(&self) -> f32 {
        if self.header.is_empty() {
            0.0
        } else {
            self.header
                .iter()
                .map(|line| self.header_line_height(line))
                .sum::<f32>()
                + HEADER_VERTICAL_PADDING * 2.0
        }
    }

    fn header_line_height(&self, line: &HeaderLine) -> f32 {
        match line {
            HeaderLine::Spacer(height) => *height,
            _ => self.metrics.row_height,
        }
    }

    /// Header row at a content-space y coordinate, accounting for variable-
    /// height spacer rows used by the inline editor.
    fn header_line_at_y(&self, target_y: f32) -> Option<usize> {
        let mut y = HEADER_VERTICAL_PADDING;
        for (index, line) in self.header.iter().enumerate() {
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
        for line in &self.header {
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

    fn description_edit_bounds(
        &self,
        bounds: Rectangle,
        vertical_offset: f32,
    ) -> Option<Rectangle> {
        self.on_edit_description?;
        let (description_y, description_height) = self.description_geometry()?;
        let description_top = bounds.y + description_y - vertical_offset;
        let description_bottom = description_top + description_height;
        if description_bottom <= bounds.y || description_top >= bounds.y + bounds.height {
            return None;
        }
        // Keep the affordance visible while any part of a long description is
        // on screen. It tracks the description block rather than becoming a
        // sticky header: once the block scrolls away, the button does too.
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

    fn description_row_contains(
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

    /// The selectable text of header line `index`, if any — field *values* and
    /// description lines. Labels, bookmark chips, and blank lines are not.
    fn header_selectable_text(&self, index: usize) -> Option<&str> {
        match self.header.get(index)? {
            HeaderLine::Field { value, .. } => Some(value),
            HeaderLine::Description(line) => Some(line),
            _ => None,
        }
    }

    /// The x where header field values begin — left edge + the (padded) label
    /// column + gap. Mirrors `draw_revision_header`'s layout so hit-testing and
    /// selection rendering land on the same glyphs the renderer drew.
    fn header_value_x(&self, bounds: Rectangle) -> f32 {
        let left_x = bounds.x + HEADER_HORIZONTAL_PADDING;
        let label_width = self
            .header
            .iter()
            .find_map(|line| match line {
                HeaderLine::Field { label, .. } => Some(self.text_width(label)),
                _ => None,
            })
            .unwrap_or(0.0);
        left_x + label_width + HEADER_LABEL_GAP
    }

    /// Screen-x where header line `index`'s selectable text starts: the value
    /// column for fields, or the indented column for description lines.
    fn header_text_origin_x(&self, index: usize, bounds: Rectangle) -> f32 {
        match self.header.get(index) {
            Some(HeaderLine::Description(_)) => {
                bounds.x
                    + HEADER_HORIZONTAL_PADDING
                    + HEADER_DESCRIPTION_INDENT * self.metrics.char_width
            }
            _ => self.header_value_x(bounds),
        }
    }

    /// Paint the selection tint behind a header line's selectable `text` at
    /// `origin_x` / `y`. Drawn before the text so the glyphs sit on top, like
    /// the diff body. No-op when nothing is selected on this line.
    fn draw_header_value_selection<Renderer>(
        &self,
        renderer: &mut Renderer,
        line_index: usize,
        text: &str,
        origin_x: f32,
        y: f32,
        selection: Option<(TextPosition, TextPosition)>,
    ) where
        Renderer: renderer::Renderer,
    {
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
        let start_chars = char_count_at_byte(text, start_byte);
        let end_chars = char_count_at_byte(text, end_byte);
        if end_chars > start_chars {
            let cw = self.metrics.char_width;
            self.draw_background(
                renderer,
                origin_x + start_chars as f32 * cw,
                y,
                (end_chars - start_chars) as f32 * cw,
                self.metrics.row_height,
                self.palette.selection,
            );
        }
    }

    /// Expand `pos` to cover the unit (word or line) that contains it, in
    /// either the header or the body. Character mode is a no-op.
    fn expand_to_unit(
        &self,
        pos: TextPosition,
        unit: SelectionUnit,
    ) -> (TextPosition, TextPosition) {
        let text: &str = match pos.region {
            Region::Header => match self.header_selectable_text(pos.line_index) {
                Some(text) => text,
                None => return (pos, pos),
            },
            Region::Body => match self
                .files
                .get(pos.file_index)
                .and_then(|file| file.hunks.get(pos.hunk_index))
                .and_then(|hunk| hunk.lines.get(pos.line_index))
            {
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

    /// Rebuild `cell`'s [`HeightIndex`] if the content shape or wrap width
    /// changed since it was built. One O(total lines) pass per change — every
    /// per-frame query then reads prefix sums instead of re-walking.
    fn ensure_height_index(&self, cell: &RefCell<HeightIndex>, width: f32) {
        // Keyed on the viewport width (not a derived text width): split and
        // full-width rows wrap against different widths, both functions of it.
        let key = Some((
            self.layout_version,
            self.files.len(),
            self.header_height().to_bits() as usize,
            width.to_bits(),
            self.wrap,
            self.side_by_side,
        ));
        if cell.borrow().key == key {
            return;
        }

        let mut index = cell.borrow_mut();
        let row_count: usize = self
            .files
            .iter()
            .map(|file| {
                file.hunks
                    .iter()
                    .map(|hunk| hunk.lines.len())
                    .sum::<usize>()
            })
            .sum();
        index.file_tops.clear();
        index.file_tops.reserve(self.files.len() + 1);
        index.hunk_tops.clear();
        index.hunk_ids.clear();
        index.row_tops.clear();
        index.row_tops.reserve(row_count);
        index.row_ids.clear();
        index.row_ids.reserve(row_count);
        index.pair_lines.clear();
        if self.side_by_side {
            index.pair_lines.reserve(row_count);
        }
        index.max_line_chars = 0;
        index.unified_text_width = self.content_width(width);
        index.split_text_width = self.effective_text_width(width);
        index.full_width_files = if self.side_by_side {
            self.files.iter().map(Self::file_is_single_sided).collect()
        } else {
            Vec::new()
        };

        let mut y = self.header_height();
        for (file_index, file) in self.files.iter().enumerate() {
            index.file_tops.push(y);
            y += self.metrics.file_header_height;
            let split_file = self.side_by_side && !index.is_full_width(file_index);
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                index.hunk_tops.push(y);
                index.hunk_ids.push((file_index as u32, hunk_index as u32));
                y += self.metrics.hunk_header_height;
                if split_file {
                    let split_width = index.split_text_width;
                    y = self.push_split_rows(
                        &mut index,
                        file_index,
                        hunk_index,
                        hunk,
                        y,
                        split_width,
                    );
                } else {
                    let unified_width = index.unified_text_width;
                    for (line_index, line) in hunk.lines.iter().enumerate() {
                        index.row_tops.push(y);
                        index.row_ids.push((
                            file_index as u32,
                            hunk_index as u32,
                            line_index as u32,
                        ));
                        if self.side_by_side {
                            // Keep `pair_lines` aligned with `row_tops` for
                            // full-width files; consumers see `pair: None`
                            // via the `is_full_width` filter.
                            index
                                .pair_lines
                                .push((line_index as u32, line_index as u32));
                        }
                        let chars = line.content.chars().count();
                        index.max_line_chars = index.max_line_chars.max(chars);
                        y += self.row_height_for_chars(chars, unified_width);
                    }
                }
            }
        }
        index.file_tops.push(y);
        index.total_height = y;
        index.key = key;
    }

    /// Append one hunk's side-by-side row pairs to the index: context (and
    /// note/conflict) lines sit on both sides; each deletion run + addition
    /// run pairs index-wise, padding whichever side is shorter. The row id's
    /// line component is the pair's first member, which keeps `row_ids`
    /// sorted within the hunk. Returns the new running `y`.
    fn push_split_rows(
        &self,
        index: &mut HeightIndex,
        file_index: usize,
        hunk_index: usize,
        hunk: &DiffHunkView,
        mut y: f32,
        content_width: f32,
    ) -> f32 {
        let lines = &hunk.lines;
        let mut push = |index: &mut HeightIndex, rep: usize, pair: (u32, u32), height: f32| {
            index.row_tops.push(y);
            index
                .row_ids
                .push((file_index as u32, hunk_index as u32, rep as u32));
            index.pair_lines.push(pair);
            y += height;
        };
        let mut max_chars = 0usize;
        let mut height_of = |this: &Self, line: &DiffLine| {
            let chars = line.content.chars().count();
            max_chars = max_chars.max(chars);
            this.row_height_for_chars(chars, content_width)
        };
        let mut i = 0;
        while i < lines.len() {
            if lines[i].kind == DiffLineKind::Addition {
                // An addition run with no deletion run in front of it (those
                // are consumed below as pairs): new lines only — they belong
                // to the right column, the left side is padding.
                let height = height_of(self, &lines[i]);
                push(&mut *index, i, (NO_LINE, i as u32), height);
                i += 1;
                continue;
            }
            if lines[i].kind != DiffLineKind::Deletion {
                let height = height_of(self, &lines[i]);
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
                    .filter_map(|line| lines.get(line))
                    .map(|line| height_of(self, line))
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
        index.max_line_chars = index.max_line_chars.max(max_chars);
        y
    }

    /// True when every line of `file` sits on one side of the diff — all
    /// additions or all deletions (context and notes appear on both sides,
    /// so their presence makes the file two-sided). Such a file renders as
    /// a single full-width column in split mode: there is nothing to mirror,
    /// and splitting it would waste half the pane on padding.
    fn file_is_single_sided(file: &DiffFileView<'_>) -> bool {
        let mut kinds = file
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| line.kind);
        let Some(first) = kinds.next() else {
            return false;
        };
        matches!(first, DiffLineKind::Addition | DiffLineKind::Deletion)
            && kinds.all(|kind| kind == first)
    }

    fn file_offset(&self, index: &HeightIndex, file_index: usize) -> f32 {
        index
            .file_tops
            .get(file_index)
            .copied()
            .unwrap_or(index.total_height)
    }

    fn file_at_offset(&self, index: &HeightIndex, offset: f32) -> usize {
        // The trailing sentinel is the content end, not a file.
        let file_count = self.files.len();
        let tops = &index.file_tops[..file_count.min(index.file_tops.len())];
        // Number of file tops at or above `offset`, minus one = the file whose
        // span contains it. An offset in the revision header maps to file 0;
        // past the end clamps to the last file — both as the walk did.
        tops.partition_point(|&top| top <= offset).saturating_sub(1)
    }

    fn content_width(&self, viewport_width: f32) -> f32 {
        (viewport_width - self.metrics.gutter_width - PREFIX_WIDTH - 16.0)
            .max(self.metrics.char_width)
    }

    /// The width one logical line wraps against: the whole text area in
    /// unified mode, one column's text area in side-by-side. Every wrap /
    /// row-height / hit-test computation keys off this, so both modes share
    /// the same visual-line math.
    fn effective_text_width(&self, viewport_width: f32) -> f32 {
        if self.side_by_side {
            self.split_layout(viewport_width).text_width
        } else {
            self.content_width(viewport_width)
        }
    }

    /// Per-column x-geometry for the side-by-side layout (offsets relative
    /// to the pane's left edge). Both columns share one `text_width`, so the
    /// wrap math stays column-agnostic; only x origins differ. Each side
    /// gets a single-number gutter (old line numbers left, new right).
    fn split_layout(&self, viewport_width: f32) -> SplitLayout {
        let gutter_width = (self.metrics.gutter_digit_count as f32 * self.metrics.char_width
            + GUTTER_HORIZONTAL_PADDING * 2.0)
            .max(GUTTER_MIN_WIDTH * 0.5);
        let column_width = ((viewport_width - 1.0) / 2.0).max(1.0);
        let text_width =
            (column_width - gutter_width - PREFIX_WIDTH - 12.0).max(self.metrics.char_width);
        SplitLayout {
            gutter_width,
            text_width,
            left_text_x: gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
            divider_x: column_width,
            right_gutter_x: column_width + 1.0,
            right_text_x: column_width + 1.0 + gutter_width + PREFIX_WIDTH + TEXT_X_PADDING,
        }
    }

    fn row_height(&self, line: &DiffLine, content_width: f32) -> f32 {
        self.row_height_for_chars(line.content.chars().count(), content_width)
    }

    fn row_height_for_chars(&self, chars: usize, content_width: f32) -> f32 {
        let chars_per_line = self.chars_per_line(content_width);
        let wrapped_lines = chars.max(1).div_ceil(chars_per_line);

        wrapped_lines as f32 * self.metrics.row_height
    }

    /// How far the no-wrap mode can scroll sideways: the longest line's
    /// width beyond the visible text area (plus one char of breathing
    /// room), zero while wrapping.
    fn max_horizontal(&self, index: &HeightIndex, viewport_width: f32) -> f32 {
        if self.wrap {
            return 0.0;
        }
        let text_width = self.effective_text_width(viewport_width);
        let content = (index.max_line_chars as f32 + 1.0) * self.metrics.char_width;
        (content - text_width).max(0.0)
    }

    /// Height of index row `row`: the single line's height in unified mode,
    /// the taller member's in side-by-side. The wrap width comes from the
    /// index — split columns and full-width files measure differently.
    fn index_row_height(&self, index: &HeightIndex, row: usize) -> f32 {
        let Some(&(file, hunk, line)) = index.row_ids.get(row) else {
            return self.metrics.row_height;
        };
        let content_width = index.text_width_for_file(file as usize);
        let lines = &self.files[file as usize].hunks[hunk as usize].lines;
        match index.pair_lines.get(row) {
            Some(&(left, right)) => [left, right]
                .into_iter()
                .filter(|&l| l != NO_LINE)
                .filter_map(|l| lines.get(l as usize))
                .map(|line| self.row_height(line, content_width))
                .fold(self.metrics.row_height, f32::max),
            None => lines
                .get(line as usize)
                .map(|line| self.row_height(line, content_width))
                .unwrap_or(self.metrics.row_height),
        }
    }

    /// Effective wrap column: the real chars-per-visual-line when wrapping,
    /// else a huge sentinel every line length stays under, so all the shared
    /// visual-line math degenerates to one visual line per row. (`/ 4` keeps
    /// the `(idx + 1) * chars_per_line` products comfortably overflow-free.)
    fn chars_per_line(&self, content_width: f32) -> usize {
        if self.wrap {
            chars_per_visual_line(content_width, self.metrics.char_width)
        } else {
            usize::MAX / 4
        }
    }

    /// Y position (in content space, before viewport scroll) of the visual
    /// line containing the byte at `byte_offset` within
    /// `(file_idx, hunk_idx, line_idx)`. Returns `None` if any of the
    /// indices are out of bounds.
    fn match_target_y(
        &self,
        index: &HeightIndex,
        file_idx: usize,
        hunk_idx: usize,
        line_idx: usize,
        byte_offset: usize,
    ) -> Option<f32> {
        let line = self
            .files
            .get(file_idx)?
            .hunks
            .get(hunk_idx)?
            .lines
            .get(line_idx)?;
        let row = index.row_of_line(file_idx, hunk_idx, line_idx)?;
        let mut y = *index.row_tops.get(row)?;

        // Offset within the wrapped row: figure out which visual line the
        // byte sits on so a match on the 5th wrap row of a 200-char line
        // doesn't scroll to the row top and leave the match off-screen.
        let content_width = index.text_width_for_file(file_idx);
        let chars_per_line = self.chars_per_line(content_width);
        let char_offset = char_count_at_byte(&line.content, byte_offset);
        let visual_idx = char_offset / chars_per_line;
        y += visual_idx as f32 * self.metrics.row_height;
        Some(y)
    }

    /// Convert a screen point into a `TextPosition` (and, for split rows,
    /// the column it resolved in) if it falls on a row's
    /// text area. Returns `None` for clicks on the gutter, file/hunk
    /// headers, or empty space below the last row.
    ///
    /// Hit-testing assumes a monospace font (see `char_width`); this is
    /// fine for code text in Menlo/Cascadia Code, but tab characters and
    /// wide glyphs (CJK, emoji) will land slightly off. We accept that
    /// trade-off because real glyph hit-testing would require keeping a
    /// `Paragraph` per visible row alive across the event loop, which iced
    /// doesn't make easy from a custom widget.
    /// Locate the document position under `point`. Unlike a normal
    /// "click to put cursor here" hit-test this clamps to the nearest valid
    /// row even when the click is in the chrome / below the last row, so
    /// dragging the mouse outside the viewport still produces a sensible
    /// selection endpoint.
    ///
    /// `lock` holds split-row resolution to one column regardless of the
    /// cursor's x — a drag stays in the column the selection started in.
    #[allow(clippy::too_many_arguments)]
    fn position_at_point(
        &self,
        index: &HeightIndex,
        point: Point,
        bounds: Rectangle,
        vertical_offset: f32,
        horizontal_offset: f32,
        lock: Option<SplitSide>,
    ) -> Option<(TextPosition, Option<SplitSide>)> {
        let split = self.side_by_side.then(|| self.split_layout(bounds.width));
        let unified_text_x = bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING;
        let text_x = match &split {
            // Side picked by the divider below; seed with the left column.
            Some(split) => bounds.x + split.left_text_x,
            None => unified_text_x,
        };
        let target_y = point.y - bounds.y + vertical_offset;
        let header_height = self.header_height();

        // The header sits above all file content. Field values (Commit ID,
        // Author, …) are selectable; clicks on labels, bookmark chips, or blank
        // lines return `None`.
        if header_height > 0.0 && target_y < header_height {
            let line_index = self.header_line_at_y(target_y)?;
            let text = self.header_selectable_text(line_index)?;
            let origin_x = self.header_text_origin_x(line_index, bounds);
            let char_count = text.chars().count();
            let relative_x = (point.x - origin_x).max(0.0);
            let char_offset =
                ((relative_x / self.metrics.char_width + 0.5).floor() as usize).min(char_count);
            return Some((
                TextPosition {
                    region: Region::Header,
                    file_index: 0,
                    hunk_index: 0,
                    line_index,
                    byte: byte_offset_for_char(text, char_offset),
                },
                None,
            ));
        }

        // Candidate row by binary search: the last row starting at or above
        // the target y. The target may instead sit in a file/hunk header band
        // (or past the end) — those fall through to the end-of-document snap
        // below, exactly like the walk did.
        if let Some(row) = index.row_at(target_y) {
            let &(file_index, hunk_index, line_index) = index.row_ids.get(row)?;
            let (file_index, hunk_index, mut line_index) = (
                file_index as usize,
                hunk_index as usize,
                line_index as usize,
            );
            // Side-by-side: resolve which column the point is in (held to
            // `lock`'s column when set), falling back to the populated side
            // of a padded pair, and shift the text origin to that column's.
            // Full-width files don't have columns — they hit-test like
            // unified rows.
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
                let pick_right = if in_right {
                    right != NO_LINE || left == NO_LINE
                } else {
                    left == NO_LINE
                };
                let (member, side) = if pick_right {
                    (right, SplitSide::Right)
                } else {
                    (left, SplitSide::Left)
                };
                if member == NO_LINE {
                    return None;
                }
                if side == SplitSide::Right {
                    text_x = bounds.x + split.right_text_x;
                }
                resolved_lane = Some(side);
                line_index = member as usize;
            }
            let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
            let row_top = index.row_tops[row];
            let content_width = index.text_width_for_file(file_index);
            let height = self.index_row_height(index, row);
            if target_y < row_top + height {
                // Each row may span multiple wrapped visual lines. Figure out
                // which visual line the click lands on, then translate the
                // horizontal click into a char offset within that visual
                // line's slice of the source content.
                let char_count = line.content.chars().count();
                let cw = self.metrics.char_width;
                let chars_per_line = self.chars_per_line(content_width);
                let visual_idx = ((target_y - row_top) / self.metrics.row_height).floor() as usize;
                let line_char_start = visual_idx.saturating_mul(chars_per_line);
                // The text is drawn shifted left by the horizontal scroll;
                // shift the cursor the other way to land on the same char.
                let relative_x = (point.x - text_x + horizontal_offset).max(0.0);
                let local_char = (relative_x / cw + 0.5).floor() as usize;
                let char_offset = (line_char_start + local_char).min(char_count);
                let byte = byte_offset_for_char(&line.content, char_offset);
                return Some((
                    TextPosition {
                        region: Region::Body,
                        file_index,
                        hunk_index,
                        line_index,
                        byte,
                    },
                    resolved_lane,
                ));
            }
        }

        // Not on a row (a header band, or past the last row). Snap to the end
        // of the document so a drag below content selects everything up to it.
        index.row_ids.last().map(|&(file, hunk, line)| {
            let (file_index, hunk_index, mut line_index) =
                (file as usize, hunk as usize, line as usize);
            if let Some(&(left, right)) = index.pair_lines.last() {
                // The last pair's later member is the true document tail.
                let tail = if right != NO_LINE { right } else { left };
                if tail != NO_LINE {
                    line_index = (line_index).max(tail as usize);
                }
            }
            let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
            (
                TextPosition {
                    region: Region::Body,
                    file_index,
                    hunk_index,
                    line_index,
                    byte: line.content.len(),
                },
                None,
            )
        })
    }

    /// Build the substring inside the inclusive selection range
    /// `[start, end)`, walking files/hunks/lines in document order so the
    /// pasted text reads naturally regardless of which direction the user
    /// dragged. `side` is the column a side-by-side selection lives in:
    /// lines the *other* column owns (deletions for a right-side selection,
    /// additions for a left-side one) aren't part of what the user sees
    /// selected, so they're skipped.
    fn collect_selected_text(
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
        for line_index in 0..self.header.len() {
            if let Some(text) = self.header_selectable_text(line_index) {
                let pos_start = header_position(line_index, 0);
                let pos_end = header_position(line_index, text.len());
                emit(text, pos_start, pos_end);
            }
        }

        // Then the diff body, in document order.
        for (file_index, file) in self.files.iter().enumerate() {
            // Full-width (single-sided) files show every line regardless of
            // the selection's column, so nothing is filtered there.
            let hidden_kind = match side {
                Some(SplitSide::Left) => Some(DiffLineKind::Addition),
                Some(SplitSide::Right) => Some(DiffLineKind::Deletion),
                None => None,
            }
            .filter(|_| self.side_by_side && !Self::file_is_single_sided(file));
            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    if Some(line.kind) == hidden_kind {
                        continue;
                    }
                    let pos_start = body_position(file_index, hunk_index, line_index, 0);
                    let pos_end =
                        body_position(file_index, hunk_index, line_index, line.content.len());
                    emit(&line.content, pos_start, pos_end);
                }
            }
        }
        output
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_row<Renderer>(
        &self,
        renderer: &mut Renderer,
        line: &DiffLine,
        render: RowRenderParams,
        cache_key: ParagraphKey,
        paragraph_cache: &RefCell<std::collections::HashMap<ParagraphKey, Renderer::Paragraph>>,
        paragraph_seen: &mut std::collections::HashSet<ParagraphKey>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let text_color = self.line_text_color(line.kind);
        // Plain source documents have one side, so a single right-aligned
        // line-number column (the metrics sized the gutter to match).
        let gutter = if self.plain {
            format_gutter_plain(line.new_line, self.metrics.gutter_digit_count)
        } else {
            format_gutter(
                line.old_line,
                line.new_line,
                self.metrics.gutter_digit_count,
            )
        };
        let prefix = prefix_for_kind(line.kind);
        let bounds = render.bounds;

        self.draw_text(
            renderer,
            &gutter,
            TextRenderParams {
                width: (self.metrics.gutter_width - GUTTER_HORIZONTAL_PADDING * 2.0).max(1.0),
                height: self.metrics.row_height,
                position: Point::new(
                    bounds.x + GUTTER_HORIZONTAL_PADDING,
                    render.y + self.metrics.text_y_pad,
                ),
                color: self.palette.text_muted,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );
        self.draw_text(
            renderer,
            prefix,
            TextRenderParams {
                width: PREFIX_WIDTH,
                height: self.metrics.row_height,
                position: Point::new(
                    bounds.x + self.metrics.gutter_width + TEXT_X_PADDING,
                    render.y + self.metrics.text_y_pad,
                ),
                color: text_color,
                clip_bounds: bounds,
                wrapping: text::Wrapping::None,
            },
        );

        let position = Point::new(
            bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING
                - render.horizontal_offset,
            render.y + self.metrics.text_y_pad,
        );

        // Glyph wrapping (hard column break) instead of `WordOrGlyph`
        // so the renderer's wrap points match our chars-per-line column
        // math exactly. Word-aware wrapping breaks at spaces, which means
        // each visual line ends at a different column than the math
        // predicts — and that's what made selection rectangles on wrapped
        // code drift before/after the true text on the last visual line.
        // For monospaced source code, glyph wrapping is also visually
        // tighter (no ragged whitespace gaps on the right edge). With wrap
        // off, rows are one visual line and clip at the pane edge instead.
        self.draw_code_text(
            renderer,
            line,
            TextRenderParams {
                // No-wrap shaping must not be bounded by the pane, or the
                // scrolled-into tail of a long line would never be laid out.
                width: if self.wrap {
                    render.content_width
                } else {
                    f32::INFINITY
                },
                height: render.height,
                position,
                color: text_color,
                clip_bounds: render.content_clip_bounds,
                wrapping: if self.wrap {
                    text::Wrapping::Glyph
                } else {
                    text::Wrapping::None
                },
            },
            cache_key,
            paragraph_cache,
            paragraph_seen,
        );
    }

    /// Paint translucent rectangles behind every find match that intersects
    /// the currently visible rows. The active match uses a stronger tint so
    /// it stands out against the surrounding non-active hits.
    fn draw_find_highlights<Renderer>(
        &self,
        renderer: &mut Renderer,
        find: &FindOverlay<'_>,
        visible_rows: &[VisibleRow],
        bounds: Rectangle,
        split: Option<&SplitLayout>,
        horizontal_offset: f32,
    ) where
        Renderer: renderer::Renderer,
    {
        if find.matches.is_empty() {
            return;
        }

        // Single pass over visible rows; for each, find matches landing on
        // it. With small numbers of matches per row this is fine; for
        // pathological cases (e.g. a `\w` regex with thousands of hits) we
        // could pre-sort matches by row and binary-search, but typical
        // queries match a few dozen times max.
        for (geometry, lane) in self.lane_geometries(bounds, split, horizontal_offset) {
            for row in visible_rows {
                let Some(line_index) = row.line_in_lane(lane) else {
                    continue;
                };
                let line = &self.files[row.file_index].hunks[row.hunk_index].lines[line_index];
                for (match_idx, m) in find.matches.iter().enumerate() {
                    if m.file_index != row.file_index
                        || m.hunk_index != row.hunk_index
                        || m.line_index != line_index
                    {
                        continue;
                    }
                    if !line.content.is_char_boundary(m.byte_start)
                        || !line
                            .content
                            .is_char_boundary(m.byte_end.min(line.content.len()))
                    {
                        continue;
                    }
                    let color = if find.active == Some(match_idx) {
                        find.active_highlight
                    } else {
                        find.highlight
                    };
                    self.draw_byte_range_highlight(
                        renderer,
                        &line.content,
                        row.y,
                        m.byte_start,
                        m.byte_end,
                        color,
                        EMPHASIS_CORNER_RADIUS,
                        &geometry,
                    );
                }
            }
        }
    }

    /// Tint the changed tokens inside modified lines (intra-line word diff).
    /// Drawn over the add/del line band, under selection/find highlights and
    /// the text itself.
    fn draw_emphasis_highlights<Renderer>(
        &self,
        renderer: &mut Renderer,
        visible_rows: &[VisibleRow],
        bounds: Rectangle,
        split: Option<&SplitLayout>,
        horizontal_offset: f32,
    ) where
        Renderer: renderer::Renderer,
    {
        for (geometry, lane) in self.lane_geometries(bounds, split, horizontal_offset) {
            for row in visible_rows {
                let Some(line_index) = row.line_in_lane(lane) else {
                    continue;
                };
                let line = &self.files[row.file_index].hunks[row.hunk_index].lines[line_index];
                if line.emphasis.is_empty() {
                    continue;
                }
                let color = match line.kind {
                    DiffLineKind::Addition => self.palette.addition_emphasis,
                    DiffLineKind::Deletion => self.palette.deletion_emphasis,
                    _ => continue,
                };
                for &(byte_start, byte_end) in &line.emphasis {
                    self.draw_byte_range_highlight(
                        renderer,
                        &line.content,
                        row.y,
                        byte_start,
                        byte_end,
                        color,
                        EMPHASIS_CORNER_RADIUS,
                        &geometry,
                    );
                }
            }
        }
    }

    /// The highlight lanes of the current mode: the full text area (the
    /// `None` lane — every row in unified mode, full-width files' rows in
    /// split mode) plus one lane per column in side-by-side. Selection/
    /// find/emphasis passes loop these so their per-row geometry stays
    /// mode-agnostic; `VisibleRow::line_in_lane` keeps each row in the
    /// lanes it actually renders in.
    fn lane_geometries(
        &self,
        bounds: Rectangle,
        split: Option<&SplitLayout>,
        horizontal_offset: f32,
    ) -> Vec<(HighlightGeometry, Option<SplitSide>)> {
        let unified = (
            HighlightGeometry {
                char_width: self.metrics.char_width,
                chars_per_line: self.chars_per_line(self.content_width(bounds.width)),
                text_x: bounds.x + self.metrics.gutter_width + PREFIX_WIDTH + TEXT_X_PADDING
                    - horizontal_offset,
                clip_left: bounds.x + self.metrics.gutter_width + PREFIX_WIDTH,
                clip_right: bounds.x + bounds.width,
                row_height: self.metrics.row_height,
            },
            None,
        );
        let Some(split) = split else {
            return vec![unified];
        };
        let lane = |text_x: f32, clip_left: f32, clip_right: f32| HighlightGeometry {
            char_width: self.metrics.char_width,
            chars_per_line: self.chars_per_line(split.text_width),
            text_x,
            clip_left,
            clip_right,
            row_height: self.metrics.row_height,
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

    /// Per-side row tints for the split layout (the unified view's merged
    /// full-width bands don't apply: each half tints by its own line's
    /// kind). A padded side — no line to show — gets a faint neutral wash
    /// plus a diagonal hatch, the classic "nothing here" treatment.
    fn draw_split_row_tints<Renderer>(
        &self,
        renderer: &mut Renderer,
        visible_rows: &[VisibleRow],
        split: &SplitLayout,
        bounds: Rectangle,
    ) where
        Renderer: renderer::Renderer + geometry::Renderer,
    {
        let mut padded: Vec<Rectangle> = Vec::new();
        for row in visible_rows {
            let Some((left, right)) = row.pair else {
                continue;
            };
            let lines = &self.files[row.file_index].hunks[row.hunk_index].lines;
            let halves = [
                (left, bounds.x, split.divider_x),
                (
                    right,
                    bounds.x + split.right_gutter_x,
                    bounds.width - split.right_gutter_x,
                ),
            ];
            for (member, x, width) in halves {
                let color = if member == NO_LINE {
                    padded.push(Rectangle {
                        x,
                        y: row.y,
                        width,
                        height: row.height,
                    });
                    Some(Color {
                        a: 0.5,
                        ..self.palette.gutter_background
                    })
                } else {
                    lines
                        .get(member as usize)
                        .and_then(|line| self.changed_line_background_color(line.kind))
                };
                if let Some(color) = color {
                    self.draw_background(renderer, x, row.y, width, row.height, color);
                }
            }
        }
        if !padded.is_empty() {
            self.draw_padding_hatch(renderer, bounds, &padded);
        }
    }

    /// 45° hatching over the padded halves of split rows. One geometry
    /// frame for the whole frame's worth of rects; the stripe phase is
    /// anchored globally (`x - y ≡ 0 mod step`) so the pattern runs
    /// seamlessly across vertically adjacent padded rows.
    fn draw_padding_hatch<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        rects: &[Rectangle],
    ) where
        Renderer: geometry::Renderer,
    {
        const STEP: f32 = 6.0;
        let stroke = Stroke::default()
            .with_color(Color {
                a: 0.4,
                ..self.palette.border
            })
            .with_width(1.0)
            .with_line_cap(LineCap::Butt);
        let mut frame = Frame::new(
            renderer,
            Size::new(bounds.x + bounds.width, bounds.y + bounds.height),
        );
        let path = Path::new(|builder| {
            for rect in rects {
                // Stripes are the lines x - y = c. Visible c values run from
                // the bottom-left corner to the top-right one.
                let c_min = rect.x - (rect.y + rect.height);
                let c_max = rect.x + rect.width - rect.y;
                let mut c = (c_min / STEP).ceil() * STEP;
                while c <= c_max {
                    let y_start = rect.y.max(rect.x - c);
                    let y_end = (rect.y + rect.height).min(rect.x + rect.width - c);
                    if y_start < y_end {
                        builder.move_to(Point::new(y_start + c, y_start));
                        builder.line_to(Point::new(y_end + c, y_end));
                    }
                    c += STEP;
                }
            }
        });
        frame.stroke(&path, stroke);
        renderer.draw_geometry(frame.into_geometry());
    }

    /// Draw one side-by-side row: each populated column gets its own
    /// single-number gutter, prefix, and (independently wrapped) code text,
    /// clipped to its column. A shared line (context) appears in both
    /// columns — the paragraph cache key is per line, so it shapes once.
    #[allow(clippy::too_many_arguments)]
    fn draw_split_row<Renderer>(
        &self,
        renderer: &mut Renderer,
        row: &VisibleRow,
        split: &SplitLayout,
        bounds: Rectangle,
        content_width: f32,
        horizontal_offset: f32,
        paragraph_cache: &RefCell<std::collections::HashMap<ParagraphKey, Renderer::Paragraph>>,
        paragraph_seen: &mut std::collections::HashSet<ParagraphKey>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let content_width_bits = content_width.to_bits();
        let lines = &self.files[row.file_index].hunks[row.hunk_index].lines;
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
            let Some(line) = lines.get(line_index) else {
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
            let gutter = match number {
                Some(n) => format!("{n:>width$}", width = self.metrics.gutter_digit_count),
                None => String::new(),
            };
            let text_color = self.line_text_color(line.kind);
            self.draw_text(
                renderer,
                &gutter,
                TextRenderParams {
                    width: (split.gutter_width - GUTTER_HORIZONTAL_PADDING * 2.0).max(1.0),
                    height: self.metrics.row_height,
                    position: Point::new(
                        bounds.x + gutter_x + GUTTER_HORIZONTAL_PADDING,
                        row.y + self.metrics.text_y_pad,
                    ),
                    color: self.palette.text_muted,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::None,
                },
            );
            self.draw_text(
                renderer,
                prefix_for_kind(line.kind),
                TextRenderParams {
                    width: PREFIX_WIDTH,
                    height: self.metrics.row_height,
                    position: Point::new(
                        bounds.x + gutter_x + split.gutter_width + TEXT_X_PADDING,
                        row.y + self.metrics.text_y_pad,
                    ),
                    color: text_color,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::None,
                },
            );
            self.draw_code_text(
                renderer,
                line,
                TextRenderParams {
                    // See `draw_row`: unbounded shaping in no-wrap mode.
                    width: if self.wrap {
                        content_width
                    } else {
                        f32::INFINITY
                    },
                    height: row.height,
                    position: Point::new(
                        bounds.x + text_x - horizontal_offset,
                        row.y + self.metrics.text_y_pad,
                    ),
                    color: text_color,
                    clip_bounds: clip,
                    wrapping: if self.wrap {
                        text::Wrapping::Glyph
                    } else {
                        text::Wrapping::None
                    },
                },
                ParagraphKey {
                    file_index: row.file_index as u32,
                    hunk_index: row.hunk_index as u32,
                    line_index: line_index as u32,
                    content_width_bits,
                },
                paragraph_cache,
                paragraph_seen,
            );
        }
    }

    /// Paint translucent rectangles behind `content[byte_start..byte_end]`,
    /// one per visual sub-line the range crosses on a wrapped row. The char
    /// math mirrors `row_height`/hit-testing (glyph wrapping at a fixed
    /// column), which is what keeps the rects glued to the glyphs.
    #[allow(clippy::too_many_arguments)]
    fn draw_byte_range_highlight<Renderer>(
        &self,
        renderer: &mut Renderer,
        content: &str,
        row_y: f32,
        byte_start: usize,
        byte_end: usize,
        color: Color,
        corner_radius: f32,
        geometry: &HighlightGeometry,
    ) where
        Renderer: renderer::Renderer,
    {
        let start_chars = char_count_at_byte(content, byte_start);
        let end_chars = char_count_at_byte(content, byte_end.min(content.len()));
        if start_chars >= end_chars {
            return;
        }
        let total_chars = content.chars().count();
        let visual_lines = total_chars.max(1).div_ceil(geometry.chars_per_line);
        for visual_idx in 0..visual_lines {
            let vline_start = visual_idx * geometry.chars_per_line;
            let vline_end = ((visual_idx + 1) * geometry.chars_per_line).min(total_chars);
            let seg_start = start_chars.max(vline_start);
            let seg_end = end_chars.min(vline_end);
            if seg_start >= seg_end {
                continue;
            }
            let mut x = geometry.text_x + (seg_start - vline_start) as f32 * geometry.char_width;
            let mut width = (seg_end - seg_start) as f32 * geometry.char_width;
            if x < geometry.clip_left {
                let trim = geometry.clip_left - x;
                x = geometry.clip_left;
                width = (width - trim).max(0.0);
            }
            if x + width > geometry.clip_right {
                width = (geometry.clip_right - x).max(0.0);
            }
            if width <= 0.0 {
                continue;
            }
            let y = row_y + visual_idx as f32 * geometry.row_height;
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x,
                        y,
                        width,
                        height: geometry.row_height,
                    },
                    border: Border {
                        radius: corner_radius.into(),
                        ..Border::default()
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                color,
            );
        }
    }

    fn draw_revision_header<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        visible_top: f32,
        header_height: f32,
        selection: Option<(TextPosition, TextPosition)>,
        show_description_edit: bool,
    ) where
        Renderer: text::Renderer<Font = Font> + geometry::Renderer,
    {
        // Header occupies content_y in `[0, header_height)`. Translate to
        // screen coords for the section that intersects the viewport.
        let header_screen_y = bounds.y - visible_top;

        // Tint the strip so the header reads as distinct chrome.
        self.draw_background(
            renderer,
            bounds.x,
            header_screen_y,
            bounds.width,
            header_height,
            self.palette.file_header,
        );

        // Border across the bottom edge of the header strip, matching the
        // visual treatment of file headers.
        self.draw_background(
            renderer,
            bounds.x,
            header_screen_y + header_height - 1.0,
            bounds.width,
            1.0,
            self.palette.border,
        );

        let clip = bounds;
        let label_color = self.palette.text_muted;
        let value_color = self.palette.text;
        // Measure the label column once. All field labels are padded to the
        // same width in `HeaderLine::field`, so any non-empty one gives us
        // the column extent.
        let label_width = self
            .header
            .iter()
            .find_map(|line| match line {
                HeaderLine::Field { label, .. } => Some(self.text_width(label)),
                _ => None,
            })
            .unwrap_or(0.0);

        let mut y = header_screen_y + HEADER_VERTICAL_PADDING;
        let left_x = bounds.x + HEADER_HORIZONTAL_PADDING;
        let value_x = left_x + label_width + HEADER_LABEL_GAP;
        let value_width = (bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING - value_x).max(1.0);

        for (line_index, line) in self.header.iter().enumerate() {
            match line {
                HeaderLine::Field { label, value } => {
                    self.draw_text(
                        renderer,
                        label,
                        TextRenderParams {
                            width: label_width.max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(left_x, y + self.metrics.text_y_pad),
                            color: label_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    self.draw_header_value_selection(
                        renderer, line_index, value, value_x, y, selection,
                    );
                    self.draw_text(
                        renderer,
                        value,
                        TextRenderParams {
                            width: value_width,
                            height: self.metrics.row_height,
                            position: Point::new(value_x, y + self.metrics.text_y_pad),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                }
                HeaderLine::Bookmarks { label, chips } => {
                    self.draw_text(
                        renderer,
                        label,
                        TextRenderParams {
                            width: label_width.max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(left_x, y + self.metrics.text_y_pad),
                            color: label_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    let gap = 6.0;
                    let right_edge = bounds.x + bounds.width - HEADER_HORIZONTAL_PADDING;
                    let center_y = y + self.metrics.row_height / 2.0;
                    let mut chip_x = value_x;
                    for c in chips {
                        let chip_w = chip::width(&c.label, c.icon, c.font);
                        // Drop chips that would overflow the panel rather than
                        // clip them mid-glyph.
                        if chip_x + chip_w > right_edge {
                            break;
                        }
                        chip::draw(renderer, c, chip_x, center_y, clip);
                        chip_x += chip_w + gap;
                    }
                }
                HeaderLine::Description(line) => {
                    // Draw the raw text at an indented origin (rather than a
                    // four-space-prefixed string) so the selectable text and
                    // the hit-test/selection origin line up.
                    let desc_x = left_x + HEADER_DESCRIPTION_INDENT * self.metrics.char_width;
                    self.draw_header_value_selection(
                        renderer, line_index, line, desc_x, y, selection,
                    );
                    self.draw_text(
                        renderer,
                        line,
                        TextRenderParams {
                            width: (bounds.x + bounds.width
                                - HEADER_HORIZONTAL_PADDING
                                - desc_x
                                - if self.on_edit_description.is_some() {
                                    HEADER_EDIT_WIDTH + HEADER_LABEL_GAP
                                } else {
                                    0.0
                                })
                            .max(1.0),
                            height: self.metrics.row_height,
                            position: Point::new(desc_x, y + self.metrics.text_y_pad),
                            color: value_color,
                            clip_bounds: clip,
                            wrapping: text::Wrapping::None,
                        },
                    );
                    if show_description_edit
                        && Some(line_index)
                            == self
                                .header
                                .iter()
                                .position(|line| matches!(line, HeaderLine::Description(_)))
                        && let Some(button) = self.description_edit_bounds(bounds, visible_top)
                    {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: button,
                                border: Border {
                                    radius: crate::theme::radius::BUTTON.into(),
                                    ..Border::default()
                                },
                                shadow: Shadow::default(),
                                snap: true,
                            },
                            Color {
                                a: 0.12,
                                ..self.palette.text_muted
                            },
                        );
                        self.draw_description_edit_label(renderer, button, clip);
                    }
                }
                HeaderLine::Blank | HeaderLine::Spacer(_) => {}
            }
            y += self.header_line_height(line);
        }
    }
}

impl<'a, Message, Renderer> Widget<Message, Theme, Renderer> for DiffView<'a, Message>
where
    Renderer: text::Renderer<Font = Font> + geometry::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph> {
            selected_file: self.selected_file,
            revision_key: self.revision_key.clone(),
            pending_file_jump: None,
            pending_find_scroll: None,
            last_find_scroll_token: None,
            vertical_offset: 0.0,
            horizontal_offset: 0.0,
            last_wrap: self.wrap,
            scroll_axis: None,
            last_scroll_at: None,
            modifiers: keyboard::Modifiers::default(),
            paragraph_cache: RefCell::new(std::collections::HashMap::new()),
            selection_anchor: None,
            selection_focus: None,
            selection_lane: None,
            selection_anchor_unit_start: None,
            selection_anchor_unit_end: None,
            selection_unit: SelectionUnit::Character,
            is_selecting: false,
            last_click_time: None,
            last_click_screen: None,
            click_count: 0,
            last_drag_cursor: None,
            scrollbar: ScrollbarState::default(),
            last_restore_token: 0,
            pending_set_offset: None,
            last_content_version: 0,
            last_header_height_bits: 0,
            last_palette_text: None,
            hovered_browse: None,
            hovered_description: false,
            height_index: RefCell::new(HeightIndex::default()),
        })
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();

        let header_height_bits = self.header_height().to_bits();
        if state.last_header_height_bits != header_height_bits {
            state.last_header_height_bits = header_height_bits;
            state.selection_anchor = None;
            state.selection_focus = None;
            state.selection_lane = None;
            state.is_selecting = false;
        }

        // Checked before the revision-key reset below (which early-returns), so
        // a tab restore is never skipped. Applied last in `update()`, so it
        // overrides the reset-to-top + file jump that reset schedules.
        if self.restore_token != state.last_restore_token {
            state.last_restore_token = self.restore_token;
            state.pending_set_offset = Some(self.restore_offset);
        }

        // The content changed under us (a reload, a working-copy edit, or a tab
        // switch — the cache lives in shared widget `State`). Drop the per-line
        // paragraph cache, whose `(file, hunk, line)` keys now map to different
        // text. Done before the revision-key reset (which early-returns) so
        // `last_content_version` always advances — otherwise it would re-clear
        // every frame after a revision change. Cache-only: a working-copy edit
        // must refresh the rendered text without resetting scroll or selection.
        if self.content_version != state.last_content_version {
            state.last_content_version = self.content_version;
            state.paragraph_cache.borrow_mut().clear();
        }

        // Theme switch: the cached paragraphs carry the old theme's span
        // colors baked in — drop them so text reshapes in the new palette.
        if state.last_palette_text != Some(self.palette.text) {
            state.last_palette_text = Some(self.palette.text);
            state.paragraph_cache.borrow_mut().clear();
        }

        // Wrap-mode flips zero the sideways scroll: wrapped content never
        // overflows, and a stale offset would blank the whole pane.
        if self.wrap != state.last_wrap {
            state.last_wrap = self.wrap;
            state.horizontal_offset = 0.0;
        }

        if state.revision_key != self.revision_key {
            state.revision_key = self.revision_key.clone();
            state.vertical_offset = 0.0;
            state.horizontal_offset = 0.0;
            state.selected_file = self.selected_file;
            state.pending_file_jump = Some(self.selected_file);
            // A revision change means the underlying line indices no longer
            // refer to the same text — drop the selection rather than risk
            // copying stale content.
            state.selection_anchor = None;
            state.selection_focus = None;
            state.selection_lane = None;
            state.selection_anchor_unit_start = None;
            state.selection_anchor_unit_end = None;
            state.selection_unit = SelectionUnit::Character;
            state.is_selecting = false;
            state.last_drag_cursor = None;
            state.click_count = 0;
            // Cache entries are keyed by (file, hunk, line) which now
            // points at different content.
            state.paragraph_cache.borrow_mut().clear();
            return;
        }

        if state.selected_file != self.selected_file {
            state.pending_file_jump = Some(self.selected_file);
        }

        if let Some(find) = &self.find
            && state.last_find_scroll_token != Some(find.scroll_token)
        {
            state.last_find_scroll_token = Some(find.scroll_token);
            if let Some(active_idx) = find.active
                && let Some(m) = find.matches.get(active_idx)
            {
                state.pending_find_scroll =
                    Some((m.file_index, m.hunk_index, m.line_index, m.byte_start));
            }
        }
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let on_scroll = self.on_scroll;
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        // (Re)build the layout index up front: every height/position read this
        // pass — and the following `draw` — is a prefix-sum lookup against it.
        self.ensure_height_index(&state.height_index, bounds.width);
        let content_height = state.height_index.borrow().total_height;
        // Offset on entry; compared on the way out so any change this pass
        // (wheel, scrollbar, file jump, find, restore, clamp) is reported once
        // via `on_scroll`. `fn` pointers are `Copy`, so this borrows nothing.
        let prev_offset = state.vertical_offset;
        let max_vertical = (content_height - bounds.height).max(0.0);
        let max_horizontal = self.max_horizontal(&state.height_index.borrow(), bounds.width);

        if state.vertical_offset > max_vertical {
            state.vertical_offset = max_vertical;
            shell.request_redraw();
        }
        if state.horizontal_offset > max_horizontal {
            state.horizontal_offset = max_horizontal;
            shell.request_redraw();
        }

        if let Some(file_index) = state
            .pending_file_jump
            .take()
            .or_else(|| (state.selected_file != self.selected_file).then_some(self.selected_file))
        {
            // For file 0, scroll to the very top so the revision header stays
            // visible — `file_offset(0)` equals `header_height()`, which would
            // park the file's content header right at the top and hide the
            // revision metadata above it.
            let target = if file_index == 0 {
                0.0
            } else {
                self.file_offset(&state.height_index.borrow(), file_index)
            };
            state.vertical_offset = target.clamp(0.0, max_vertical);
            state.selected_file = file_index;
            shell.request_redraw();
        }

        if let Some((file_idx, hunk_idx, line_idx, byte_offset)) = state.pending_find_scroll.take()
            && let Some(target) = {
                let index = state.height_index.borrow();
                self.match_target_y(&index, file_idx, hunk_idx, line_idx, byte_offset)
            }
        {
            // Center the row in the viewport when there's room; clamp
            // otherwise. Centering keeps the match's surrounding context
            // visible instead of pinning it to the top edge.
            let centered = target - (bounds.height - self.metrics.row_height) / 2.0;
            state.vertical_offset = centered.clamp(0.0, max_vertical);
            // In no-wrap mode the match may sit past the right edge —
            // scroll sideways so it lands ~1/3 into the text area (some
            // leading context, most of the room for what follows).
            if max_horizontal > 0.0
                && let Some(line) = self
                    .files
                    .get(file_idx)
                    .and_then(|file| file.hunks.get(hunk_idx))
                    .and_then(|hunk| hunk.lines.get(line_idx))
            {
                let match_x =
                    char_count_at_byte(&line.content, byte_offset) as f32 * self.metrics.char_width;
                let text_width = self.effective_text_width(bounds.width);
                let off_screen = match_x < state.horizontal_offset
                    || match_x > state.horizontal_offset + text_width - self.metrics.char_width;
                if off_screen {
                    state.horizontal_offset =
                        (match_x - text_width / 3.0).clamp(0.0, max_horizontal);
                }
            }
            let selected_file =
                self.file_at_offset(&state.height_index.borrow(), state.vertical_offset);
            if selected_file != state.selected_file {
                state.selected_file = selected_file;
                shell.publish((self.on_selected_file_changed)(selected_file));
            }
            shell.request_redraw();
        }

        // Tab restore: jump straight to the saved offset, overriding the
        // reset-to-top a revision change scheduled above. Only the offset is
        // restored — `selected_file` was already set by the file-jump block
        // above (to the tab's saved file), and republishing it here would fire
        // `SelectFile` → `scroll_sidebar_to_file`, fighting the sidebar's own
        // restore. The saved offset and saved file were captured together, so
        // they stay consistent without recomputing one from the other.
        if let Some(offset) = state.pending_set_offset.take() {
            let target = offset.clamp(0.0, max_vertical);
            if (target - state.vertical_offset).abs() > f32::EPSILON {
                state.vertical_offset = target;
                shell.request_redraw();
            }
        }

        match event {
            Event::Window(window::Event::RedrawRequested(_)) => {
                if state.is_selecting
                    && let Some(cursor_pos) = state.last_drag_cursor
                {
                    let scroll_delta = auto_scroll_delta(cursor_pos.y, bounds);
                    if scroll_delta != 0.0 {
                        let new_offset =
                            (state.vertical_offset + scroll_delta).clamp(0.0, max_vertical);
                        if (new_offset - state.vertical_offset).abs() > f32::EPSILON {
                            state.vertical_offset = new_offset;
                            self.advance_drag_selection(state, cursor_pos, bounds);
                            let selected_file = self.file_at_offset(
                                &state.height_index.borrow(),
                                state.vertical_offset,
                            );
                            if selected_file != state.selected_file {
                                state.selected_file = selected_file;
                                shell.publish((self.on_selected_file_changed)(selected_file));
                            }
                        }
                        shell.request_redraw();
                    }
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let Some(_cursor_position) = cursor.position_over(bounds) else {
                    return;
                };

                // Shift+wheel pans sideways, matching editors. Only redirect
                // when the event has no native x component: macOS folds
                // shift+wheel into the x delta itself, and trackpads / tilt
                // wheels emit real x deltas — swapping those would just
                // re-derive vertical scroll.
                let shift_swap = |x: f32, y: f32| {
                    if state.modifiers.shift() && x == 0.0 {
                        (y, 0.0)
                    } else {
                        (x, y)
                    }
                };
                let mut movement = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => {
                        let (x, y) = shift_swap(x, y);
                        Vector::new(
                            -x * self.metrics.char_width * LINE_SCROLL_ROWS,
                            -y * self.metrics.row_height * LINE_SCROLL_ROWS,
                        )
                    }
                    mouse::ScrollDelta::Pixels { x, y } => {
                        let (x, y) = shift_swap(x, y);
                        Vector::new(-x * PIXEL_SCROLL_SCALE, -y * PIXEL_SCROLL_SCALE)
                    }
                };

                // IDE-style axis lock while horizontal scrolling exists (no
                // wrap, overflowing lines): the gesture's first event picks
                // the dominant axis and the other component is discarded, so
                // a touchpad pan doesn't drift diagonally. A pause in events
                // ends the gesture and the next one re-picks.
                if max_horizontal > 0.0 {
                    let now = Instant::now();
                    let gesture_ended = state.last_scroll_at.is_none_or(|at| {
                        now.saturating_duration_since(at).as_millis() as u64
                            >= SCROLL_GESTURE_GAP_MS
                    });
                    state.last_scroll_at = Some(now);
                    if gesture_ended {
                        state.scroll_axis = None;
                    }
                    let axis = *state.scroll_axis.get_or_insert(
                        if movement.x.abs() > movement.y.abs() {
                            ScrollAxis::Horizontal
                        } else {
                            ScrollAxis::Vertical
                        },
                    );
                    match axis {
                        ScrollAxis::Horizontal => movement.y = 0.0,
                        ScrollAxis::Vertical => movement.x = 0.0,
                    }
                } else {
                    state.scroll_axis = None;
                    state.last_scroll_at = None;
                }

                if movement.y != 0.0 {
                    state.vertical_offset =
                        (state.vertical_offset + movement.y).clamp(0.0, max_vertical);
                    let selected_file =
                        self.file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                    if selected_file != state.selected_file {
                        state.selected_file = selected_file;
                        shell.publish((self.on_selected_file_changed)(selected_file));
                    }
                }
                // Sideways: trackpads and tilt wheels report an x delta
                // (macOS also folds shift+wheel into it). Only live in
                // no-wrap mode, where content can actually overflow.
                if movement.x != 0.0 && max_horizontal > 0.0 {
                    state.horizontal_offset =
                        (state.horizontal_offset + movement.x).clamp(0.0, max_horizontal);
                }

                // Headers slid under the (stationary) cursor — refresh the
                // browse-button hover so a wash doesn't linger on a moved row.
                let hovered = cursor
                    .position_over(bounds)
                    .and_then(|point| self.browse_button_at(state, bounds, point));
                if hovered != state.hovered_browse {
                    state.hovered_browse = hovered;
                }
                state.hovered_description = cursor.position_over(bounds).is_some_and(|point| {
                    self.description_row_contains(bounds, state.vertical_offset, point)
                });

                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position_over(bounds) else {
                    // Click outside our bounds — drop any existing selection
                    // so it doesn't visually persist after the user moves on.
                    if state.selection_anchor.is_some() {
                        state.selection_anchor = None;
                        state.selection_focus = None;
                        state.selection_lane = None;
                        state.selection_anchor_unit_start = None;
                        state.selection_anchor_unit_end = None;
                        state.selection_unit = SelectionUnit::Character;
                        shell.request_redraw();
                    }
                    return;
                };
                match scrollbar::on_button_pressed(
                    &mut state.scrollbar,
                    point,
                    bounds,
                    content_height,
                    state.vertical_offset,
                ) {
                    scrollbar::ScrollbarEvent::OffsetChanged(new_offset) => {
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file = self
                            .file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
                        shell.capture_event();
                        shell.request_redraw();
                        if state.vertical_offset != prev_offset
                            && let Some(cb) = on_scroll
                        {
                            shell.publish(cb(state.vertical_offset));
                        }
                        return;
                    }
                    scrollbar::ScrollbarEvent::Captured => {
                        shell.capture_event();
                        return;
                    }
                    scrollbar::ScrollbarEvent::None => {}
                }
                // The per-file browse button wins over selection: a press on
                // it fires the callback instead of anchoring a drag.
                if self
                    .description_edit_bounds(bounds, state.vertical_offset)
                    .is_some_and(|button| button.contains(point))
                {
                    if let Some(on_edit) = self.on_edit_description {
                        shell.publish(on_edit());
                    }
                    shell.capture_event();
                    return;
                }
                if let Some(file_index) = self.browse_button_at(state, bounds, point) {
                    if let Some(on_browse) = self.on_browse_file {
                        shell.publish(on_browse(file_index));
                    }
                    shell.capture_event();
                    return;
                }
                let position = {
                    let index = state.height_index.borrow();
                    self.position_at_point(
                        &index,
                        point,
                        bounds,
                        state.vertical_offset,
                        state.horizontal_offset,
                        None,
                    )
                };
                let Some((position, lane)) = position else {
                    return;
                };

                let now = Instant::now();
                let within_window = state
                    .last_click_time
                    .map(|t| now.duration_since(t).as_millis() as u64 <= self.multi_click_ms)
                    .unwrap_or(false);
                let same_spot = state
                    .last_click_screen
                    .map(|p| p.distance(point) <= MULTI_CLICK_RADIUS)
                    .unwrap_or(false);
                let next_count = if within_window && same_spot {
                    state.click_count + 1
                } else {
                    1
                };
                state.click_count = next_count;
                state.last_click_time = Some(now);
                state.last_click_screen = Some(point);

                // Cycle through Char → Word → Line on each successive click.
                let unit = match ((next_count - 1) % 3, next_count) {
                    (0, _) => SelectionUnit::Character,
                    (1, _) => SelectionUnit::Word,
                    _ => SelectionUnit::Line,
                };
                state.selection_unit = unit;

                let (anchor_start, anchor_end) = self.expand_to_unit(position, unit);
                state.selection_anchor_unit_start = Some(anchor_start);
                state.selection_anchor_unit_end = Some(anchor_end);
                state.selection_anchor = Some(anchor_start);
                state.selection_focus = Some(anchor_end);
                state.selection_lane = lane;
                state.is_selecting = true;
                state.last_drag_cursor = Some(point);
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if scrollbar::is_dragging(&state.scrollbar) {
                    if let scrollbar::ScrollbarEvent::OffsetChanged(new_offset) =
                        scrollbar::on_cursor_moved(
                            &mut state.scrollbar,
                            *position,
                            bounds,
                            content_height,
                        )
                    {
                        state.vertical_offset = new_offset.clamp(0.0, max_vertical);
                        let selected_file = self
                            .file_at_offset(&state.height_index.borrow(), state.vertical_offset);
                        if selected_file != state.selected_file {
                            state.selected_file = selected_file;
                            shell.publish((self.on_selected_file_changed)(selected_file));
                        }
                        shell.capture_event();
                        shell.request_redraw();
                    }
                    if state.vertical_offset != prev_offset
                        && let Some(cb) = on_scroll
                    {
                        shell.publish(cb(state.vertical_offset));
                    }
                    return;
                }
                if !state.is_selecting {
                    // Browse-button hover wash (manual — this widget has no
                    // per-region hover for free).
                    let hovered = cursor
                        .position_over(bounds)
                        .and_then(|point| self.browse_button_at(state, bounds, point));
                    if hovered != state.hovered_browse {
                        state.hovered_browse = hovered;
                        shell.request_redraw();
                    }
                    let hovered_description =
                        self.description_row_contains(bounds, state.vertical_offset, *position);
                    if hovered_description != state.hovered_description {
                        state.hovered_description = hovered_description;
                        shell.request_redraw();
                    }
                    return;
                }
                state.last_drag_cursor = Some(*position);
                self.advance_drag_selection(state, *position, bounds);
                // Kick the redraw loop so the auto-scroll handler picks up
                // where the cursor is even if the mouse stays still.
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if let scrollbar::ScrollbarEvent::Captured =
                    scrollbar::on_button_released(&mut state.scrollbar)
                {
                    shell.capture_event();
                    return;
                }
                if !state.is_selecting {
                    return;
                }
                state.is_selecting = false;
                state.last_drag_cursor = None;
                // If the user clicked without dragging in character mode,
                // anchor == focus — clear so we don't leave a phantom
                // zero-width selection. Multi-click selections are kept
                // even without drag, since the user expects them to stick
                // (e.g. double-click a word, then Cmd+C).
                if state.selection_unit == SelectionUnit::Character
                    && state.selection_anchor == state.selection_focus
                {
                    state.selection_anchor = None;
                    state.selection_focus = None;
                    state.selection_lane = None;
                }
                shell.request_redraw();
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                if !is_copy_shortcut(key, *modifiers) {
                    return;
                }
                let (Some(anchor), Some(focus)) = (state.selection_anchor, state.selection_focus)
                else {
                    return;
                };
                let (start, end) = ordered(anchor, focus);
                if start == end {
                    return;
                }
                let Some(on_copy) = self.on_copy else {
                    return;
                };
                let text = self.collect_selected_text(start, end, state.selection_lane);
                if !text.is_empty() {
                    shell.publish(on_copy(text));
                    shell.capture_event();
                }
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                // Tracked for shift+wheel panning; deliberately not captured
                // — other widgets follow modifier changes too.
                state.modifiers = *modifiers;
            }
            _ => {}
        }

        // Report any offset change from this pass (wheel, auto-scroll, file
        // jump, find, restore, clamp) once. The scrollbar early-returns above
        // publish on their own.
        if state.vertical_offset != prev_offset
            && let Some(cb) = on_scroll
        {
            shell.publish(cb(state.vertical_offset));
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(_) = bounds.intersection(viewport) else {
            return;
        };

        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        // Split-column wrap width in split mode, the whole text area
        // otherwise; full-width rows always use `unified_width`.
        let content_width = self.effective_text_width(bounds.width);
        let unified_width = self.content_width(bounds.width);
        let split = self.side_by_side.then(|| self.split_layout(bounds.width));
        // Normally a no-op — `update` ran first and built it — but draw must
        // not rely on event ordering for correctness.
        self.ensure_height_index(&state.height_index, bounds.width);
        let height_index = state.height_index.borrow();
        // Keys touched this frame; entries not in this set get evicted
        // at the end so the cache doesn't grow unbounded as the user
        // scrolls past new rows. Eviction-at-end is safe because the
        // renderer only holds `Weak` refs to paragraphs we *did* draw
        // (i.e. ones we add to `seen` here).
        let mut paragraph_seen: std::collections::HashSet<ParagraphKey> =
            std::collections::HashSet::new();
        let horizontal_offset = state.horizontal_offset;
        // Everything text-shaped clips out of the (left) gutter band; the
        // split layout's narrower gutter widens this accordingly.
        let text_region_x = match &split {
            Some(split) => split.gutter_width + PREFIX_WIDTH,
            None => self.metrics.gutter_width + PREFIX_WIDTH,
        };
        let content_clip_bounds = Rectangle {
            x: bounds.x + text_region_x,
            y: bounds.y,
            width: (bounds.width - text_region_x).max(1.0),
            height: bounds.height,
        };
        // Clip for unified-rendered rows — in split mode their (wider)
        // double gutter sits to the right of the split columns' clip edge,
        // so they need their own rect or h-scrolled text would paint over it.
        let unified_region_x = self.metrics.gutter_width + PREFIX_WIDTH;
        let unified_clip_bounds = Rectangle {
            x: bounds.x + unified_region_x,
            y: bounds.y,
            width: (bounds.width - unified_region_x).max(1.0),
            height: bounds.height,
        };

        renderer.with_layer(bounds, |renderer| {
            self.draw_background(
                renderer,
                bounds.x,
                bounds.y,
                bounds.width,
                bounds.height,
                self.palette.panel,
            );

            let visible_top = state.vertical_offset;
            let visible_bottom = visible_top + bounds.height;
            let header_height = self.header_height();
            let visible_capacity = (bounds.height / self.metrics.row_height).ceil() as usize + 8;
            let mut visible_file_headers = Vec::new();
            let mut visible_hunk_headers = Vec::new();
            let mut visible_rows = Vec::with_capacity(visible_capacity);
            let mut visible_bands = Vec::new();

            // Everything below is O(log n + visible) against the prefix-sum
            // index — never a walk over the whole document.
            // Content-space top of each file's header (sans the sentinel).
            let file_tops =
                &height_index.file_tops[..self.files.len().min(height_index.file_tops.len())];

            // Plain documents draw no header strips (their heights are zero
            // anyway — collecting them would just emit invisible-height rows
            // whose text still paints).
            if !self.plain {
                let first_file = file_tops
                    .partition_point(|&top| top + self.metrics.file_header_height < visible_top);
                for (file_index, &top) in file_tops.iter().enumerate().skip(first_file) {
                    if top > visible_bottom {
                        break;
                    }
                    visible_file_headers.push(VisibleFileHeader {
                        file_index,
                        y: bounds.y + (top - visible_top),
                    });
                }

                let first_hunk = height_index
                    .hunk_tops
                    .partition_point(|&top| top + self.metrics.hunk_header_height < visible_top);
                for (i, &top) in height_index.hunk_tops.iter().enumerate().skip(first_hunk) {
                    if top > visible_bottom {
                        break;
                    }
                    let (file_index, hunk_index) = height_index.hunk_ids[i];
                    visible_hunk_headers.push(VisibleHunkHeader {
                        file_index: file_index as usize,
                        hunk_index: hunk_index as usize,
                        y: bounds.y + (top - visible_top),
                    });
                }
            }

            // The candidate first row may still end above the viewport when
            // the top lands in a header band — the in-loop check skips it.
            let first_row = height_index
                .row_tops
                .partition_point(|&top| top <= visible_top)
                .saturating_sub(1);
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
                // Full-width files drop their alignment-only pair so the row
                // renders (and band-tints) like a unified one.
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
                if pair.is_none() {
                    let line = &self.files[file_index].hunks[hunk_index].lines[line_index];
                    push_visible_band(&mut visible_bands, line.kind, y, height);
                }
            }

            // Sticky file header: the file occupying the top of the viewport
            // keeps its name pinned while its hunks scroll under it, until the
            // next file's header slides up to push it off. Only kicks in once
            // that file's header has scrolled above the viewport top — which is
            // always below the revision header, so the two never overlap.
            let content_end = height_index.total_height;
            let sticky_file = (!self.plain)
                .then(|| {
                    file_tops
                        .partition_point(|&top| top <= visible_top)
                        .checked_sub(1)
                        .filter(|&i| file_tops[i] < visible_top)
                })
                .flatten();
            let sticky_pin_y = sticky_file.map(|i| {
                let next_top = file_tops.get(i + 1).copied().unwrap_or(content_end);
                let pinned_content_y = visible_top.min(next_top - self.metrics.file_header_height);
                bounds.y + (pinned_content_y - visible_top)
            });

            match &split {
                Some(split) => {
                    // One gutter strip per column, plus the center divider —
                    // segmented per file, because full-width (single-sided)
                    // files keep the unified double gutter instead. The last
                    // segment runs to the pane bottom so short documents
                    // keep their chrome below the content, as before.
                    let split_chrome = |renderer: &mut Renderer, y: f32, height: f32| {
                        for gutter_x in [0.0, split.right_gutter_x] {
                            self.draw_background(
                                renderer,
                                bounds.x + gutter_x,
                                y,
                                split.gutter_width,
                                height,
                                self.palette.gutter_background,
                            );
                            self.draw_background(
                                renderer,
                                bounds.x + gutter_x + split.gutter_width,
                                y,
                                1.0,
                                height,
                                self.palette.border,
                            );
                        }
                        self.draw_background(
                            renderer,
                            bounds.x + split.divider_x,
                            y,
                            1.0,
                            height,
                            self.palette.border,
                        );
                    };
                    if self.files.is_empty() {
                        split_chrome(renderer, bounds.y, bounds.height);
                    }
                    for (file_index, &top) in file_tops.iter().enumerate() {
                        let bottom = if file_index + 1 < self.files.len() {
                            height_index.file_tops[file_index + 1]
                        } else {
                            f32::MAX
                        };
                        if bottom < visible_top || top > visible_bottom {
                            continue;
                        }
                        let seg_top = top.max(visible_top);
                        let y = bounds.y + (seg_top - visible_top);
                        let height = bottom.min(visible_bottom) - seg_top;
                        if height <= 0.0 {
                            continue;
                        }
                        if height_index.is_full_width(file_index) {
                            self.draw_background(
                                renderer,
                                bounds.x,
                                y,
                                self.metrics.gutter_width,
                                height,
                                self.palette.gutter_background,
                            );
                            self.draw_background(
                                renderer,
                                bounds.x + self.metrics.gutter_width,
                                y,
                                1.0,
                                height,
                                self.palette.border,
                            );
                        } else {
                            split_chrome(renderer, y, height);
                        }
                    }
                }
                None => {
                    self.draw_background(
                        renderer,
                        bounds.x,
                        bounds.y,
                        self.metrics.gutter_width,
                        bounds.height,
                        self.palette.gutter_background,
                    );
                    self.draw_background(
                        renderer,
                        bounds.x + self.metrics.gutter_width,
                        bounds.y,
                        1.0,
                        bounds.height,
                        self.palette.border,
                    );
                }
            }

            for band in &visible_bands {
                let Some(background) = self.changed_line_background_color(band.kind) else {
                    continue;
                };

                self.draw_background(
                    renderer,
                    bounds.x,
                    band.y,
                    bounds.width,
                    band.height,
                    background,
                );
            }
            if let Some(split) = &split {
                self.draw_split_row_tints(renderer, &visible_rows, split, bounds);
            }

            self.draw_emphasis_highlights(
                renderer,
                &visible_rows,
                bounds,
                split.as_ref(),
                horizontal_offset,
            );

            let selection_range = match (state.selection_anchor, state.selection_focus) {
                (Some(anchor), Some(focus)) if anchor != focus => Some(ordered(anchor, focus)),
                _ => None,
            };

            if header_height > 0.0 {
                self.draw_revision_header(
                    renderer,
                    bounds,
                    visible_top,
                    header_height,
                    selection_range,
                    state.hovered_description,
                );
            }

            for header in &visible_file_headers {
                // The sticky file's header is drawn pinned, on top, after the
                // rows — skip its natural (scrolling) draw here.
                if Some(header.file_index) == sticky_file {
                    continue;
                }
                self.draw_file_header(
                    renderer,
                    header.file_index,
                    header.y,
                    bounds,
                    state.hovered_browse == Some(header.file_index),
                );
            }

            for header in &visible_hunk_headers {
                let hunk = &self.files[header.file_index].hunks[header.hunk_index];
                // A soft full-width info-tinted band — a faint 1px underline
                // alone made hunk boundaries nearly invisible while scrolling.
                self.draw_background(
                    renderer,
                    bounds.x,
                    header.y,
                    bounds.width,
                    self.metrics.hunk_header_height,
                    self.palette.hunk_header,
                );
                let (header_x, header_clip) = if height_index.is_full_width(header.file_index) {
                    (unified_region_x, unified_clip_bounds)
                } else {
                    (text_region_x, content_clip_bounds)
                };
                self.draw_text(
                    renderer,
                    &hunk.header,
                    TextRenderParams {
                        width: self.text_width(&hunk.header),
                        height: self.metrics.hunk_header_height,
                        position: Point::new(
                            bounds.x + header_x + TEXT_X_PADDING,
                            header.y + self.metrics.text_y_pad,
                        ),
                        color: self.palette.text_muted,
                        clip_bounds: header_clip,
                        wrapping: text::Wrapping::None,
                    },
                );
            }

            // Draw selection backgrounds *before* the text so glyphs render
            // on top. Painting after would either occlude the text or
            // require alpha blending tricks; before-and-translucent is the
            // common pattern (matches how text editors render selection).
            if let (Some(anchor), Some(focus)) = (state.selection_anchor, state.selection_focus) {
                let (sel_start, sel_end) = ordered(anchor, focus);
                if sel_start != sel_end {
                    let visual_line_height = self.metrics.row_height;
                    for (geometry, lane) in
                        self.lane_geometries(bounds, split.as_ref(), horizontal_offset)
                    {
                        // A side-by-side selection lives in one column; the
                        // mirror column shows no highlight even where it
                        // carries the same (context) line.
                        if let (Some(sel_lane), Some(lane_side)) = (state.selection_lane, lane)
                            && sel_lane != lane_side
                        {
                            continue;
                        }
                        let cw = geometry.char_width;
                        let chars_per_line = geometry.chars_per_line;
                        for row in &visible_rows {
                            let Some(line_index) = row.line_in_lane(lane) else {
                                continue;
                            };
                            let line =
                                &self.files[row.file_index].hunks[row.hunk_index].lines[line_index];
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
                            let start_chars = char_count_at_byte(&line.content, line_start_byte);
                            let end_chars = char_count_at_byte(&line.content, line_end_byte);
                            let total_chars = line.content.chars().count();
                            let is_full_line = sel_start <= row_pos_start && row_pos_end < sel_end;

                            // Walk each visual sub-line the row contains and
                            // intersect the selection char range with it. Without
                            // this loop a wrapped row would render a single full-
                            // width rectangle across every visual line, ignoring
                            // where the selection actually starts and ends.
                            let visual_lines = total_chars.max(1).div_ceil(chars_per_line);
                            for visual_idx in 0..visual_lines {
                                let vline_start = visual_idx * chars_per_line;
                                let vline_end =
                                    ((visual_idx + 1) * chars_per_line).min(total_chars);
                                let seg_start = start_chars.max(vline_start);
                                let seg_end = end_chars.min(vline_end);
                                if seg_start >= seg_end {
                                    continue;
                                }
                                let mut x = geometry.text_x + (seg_start - vline_start) as f32 * cw;
                                let mut width = (seg_end - seg_start) as f32 * cw;
                                // The "select through end-of-line" tail only
                                // belongs on the trailing visual line of a full
                                // logical row, not on every wrapped segment.
                                let is_trailing_visual = visual_idx + 1 == visual_lines;
                                if is_full_line && is_trailing_visual {
                                    width += cw * 0.6;
                                }
                                if x < geometry.clip_left {
                                    let trim = geometry.clip_left - x;
                                    x = geometry.clip_left;
                                    width = (width - trim).max(0.0);
                                }
                                if x + width > geometry.clip_right {
                                    width = (geometry.clip_right - x).max(0.0);
                                }
                                if width <= 0.0 {
                                    continue;
                                }
                                let y = row.y + visual_idx as f32 * visual_line_height;
                                self.draw_background(
                                    renderer,
                                    x,
                                    y,
                                    width,
                                    visual_line_height,
                                    self.palette.selection,
                                );
                            }
                        }
                    }
                }
            }

            if let Some(find) = &self.find {
                self.draw_find_highlights(
                    renderer,
                    find,
                    &visible_rows,
                    bounds,
                    split.as_ref(),
                    horizontal_offset,
                );
            }

            let unified_width_bits = unified_width.to_bits();
            for row in &visible_rows {
                if let (Some(split), Some(_)) = (&split, row.pair) {
                    self.draw_split_row(
                        renderer,
                        row,
                        split,
                        bounds,
                        content_width,
                        horizontal_offset,
                        &state.paragraph_cache,
                        &mut paragraph_seen,
                    );
                    continue;
                }
                // Unified-rendered row: unified mode, or a full-width
                // (single-sided) file in split mode.
                let line = &self.files[row.file_index].hunks[row.hunk_index].lines[row.line_index];
                let key = ParagraphKey {
                    file_index: row.file_index as u32,
                    hunk_index: row.hunk_index as u32,
                    line_index: row.line_index as u32,
                    content_width_bits: unified_width_bits,
                };
                self.draw_row(
                    renderer,
                    line,
                    RowRenderParams {
                        bounds,
                        content_clip_bounds: unified_clip_bounds,
                        y: row.y,
                        height: row.height,
                        content_width: unified_width,
                        horizontal_offset,
                    },
                    key,
                    &state.paragraph_cache,
                    &mut paragraph_seen,
                );
            }

            // Pinned sticky file header. Drawn in its own layer so the opaque
            // strip composites *over* the code beneath it: within a single
            // layer `fill_quad` always renders behind `fill_text`, so the
            // header background alone can't hide the diff text scrolling under
            // it — only a new layer does.
            if let (Some(file_index), Some(y)) = (sticky_file, sticky_pin_y) {
                renderer.with_layer(bounds, |renderer| {
                    self.draw_file_header(
                        renderer,
                        file_index,
                        y,
                        bounds,
                        state.hovered_browse == Some(file_index),
                    );
                });
            }

            let geom =
                scrollbar::geometry(bounds, height_index.total_height, state.vertical_offset);
            // Draw the scrollbar in its own layer, created *after* the sticky
            // header's, so it composites above it — sub-layers stack in creation
            // order, so otherwise the full-width sticky strip would hide the
            // scrollbar's top.
            renderer.with_layer(bounds, |renderer| {
                scrollbar::draw(renderer, &geom, &self.palette.scrollbar);
            });
        });

        // Evict entries that weren't used this frame. Their last
        // `fill_paragraph` was at least one frame ago, so the renderer's
        // present queue no longer references them.
        state
            .paragraph_cache
            .borrow_mut()
            .retain(|k, _| paragraph_seen.contains(k));
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        // Match the convention of every code editor: text cursor over the
        // text area, default arrow over the gutter / chrome. (Avoid
        // `AllScroll` — implies drag-to-pan, which we don't support.)
        let bounds = layout.bounds();
        let Some(point) = cursor.position_over(bounds) else {
            return mouse::Interaction::None;
        };
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        self.ensure_height_index(&state.height_index, bounds.width);
        let content_height = state.height_index.borrow().total_height;
        if scrollbar::is_dragging(&state.scrollbar)
            || scrollbar::hits_container(bounds, point, content_height)
        {
            return mouse::Interaction::Idle;
        }
        if self.browse_button_at(state, bounds, point).is_some() {
            return mouse::Interaction::Pointer;
        }
        if self
            .description_edit_bounds(bounds, state.vertical_offset)
            .is_some_and(|button| button.contains(point))
        {
            return mouse::Interaction::Pointer;
        }
        // In the revision-header strip, show the text cursor only over the
        // selectable values (field values + description), and the arrow over
        // labels, bookmark chips, and blank space.
        let target_y = point.y - bounds.y + state.vertical_offset;
        if target_y < self.header_height() {
            let over_value = self.header_line_at_y(target_y).is_some_and(|line_index| {
                self.header_selectable_text(line_index).is_some()
                    && point.x >= self.header_text_origin_x(line_index, bounds)
            });
            return if over_value {
                mouse::Interaction::Text
            } else {
                mouse::Interaction::Idle
            };
        }
        if self.side_by_side {
            let index = state.height_index.borrow();
            // Full-width (single-sided) files hit-test like unified rows.
            if !index.is_full_width(self.file_at_offset(&index, target_y)) {
                let split = self.split_layout(bounds.width);
                let x = point.x - bounds.x;
                let over_text = (x >= split.gutter_width + PREFIX_WIDTH && x < split.divider_x)
                    || x >= split.right_gutter_x + split.gutter_width + PREFIX_WIDTH;
                return if over_text {
                    mouse::Interaction::Text
                } else {
                    mouse::Interaction::Idle
                };
            }
        }
        if point.x >= bounds.x + self.metrics.gutter_width + PREFIX_WIDTH {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::Idle
        }
    }
}

impl<Message> DiffView<'_, Message> {
    fn text_width(&self, content: &str) -> f32 {
        (content.chars().count() as f32 * self.metrics.char_width + 16.0).max(1.0)
    }

    /// The right-aligned `+N -N  K Hunks` summary of a file header, and its
    /// clamped width — shared by the header draw and the browse-button hit
    /// test so the two agree on geometry.
    fn file_header_summary(
        &self,
        file: &DiffFileView<'_>,
        bounds: Rectangle,
    ) -> (String, String, String, f32) {
        let hunk_label = if file.hunks.len() == 1 {
            "1 Hunk".to_owned()
        } else {
            format!("{} Hunks", file.hunks.len())
        };
        let additions = format!("+{}", file.additions);
        let deletions = format!("-{}", file.deletions);
        let mono_width = |content: &str| content.chars().count() as f32 * self.metrics.char_width;
        let gap = self.metrics.char_width;
        let summary_width = (mono_width(&additions)
            + gap
            + mono_width(&deletions)
            + 2.0 * gap
            + mono_width(&hunk_label))
        .min((bounds.width - 24.0).max(1.0));
        (additions, deletions, hunk_label, summary_width)
    }

    /// Screen rect of a file header's browse button when the header is drawn
    /// at `header_y`, or `None` when the affordance is off or there's no room.
    fn browse_button_rect(
        &self,
        file_index: usize,
        header_y: f32,
        bounds: Rectangle,
    ) -> Option<Rectangle> {
        self.on_browse_file?;
        if self.plain {
            return None;
        }
        let file = self.files.get(file_index)?;
        let (_, _, _, summary_width) = self.file_header_summary(file, bounds);
        let summary_x = (bounds.x + bounds.width - summary_width - 8.0).max(bounds.x + 12.0);
        let size = BROWSE_BUTTON_SIZE;
        let x = summary_x - 10.0 - size;
        let y = header_y + (self.metrics.file_header_height - size) / 2.0;
        // Vanishes rather than colliding with the title when the pane is
        // squeezed to nothing.
        (x > bounds.x + 48.0).then_some(Rectangle {
            x,
            y,
            width: size,
            height: size,
        })
    }

    /// `(file index, header screen y)` for every file header currently
    /// visible, with the sticky pinned header replacing its natural position
    /// — the browse-button hit test must match where headers actually draw.
    fn visible_file_header_positions(
        &self,
        index: &HeightIndex,
        bounds: Rectangle,
        visible_top: f32,
    ) -> Vec<(usize, f32)> {
        if self.plain || self.metrics.file_header_height <= 0.0 {
            return Vec::new();
        }
        let visible_bottom = visible_top + bounds.height;
        let file_tops = &index.file_tops[..self.files.len().min(index.file_tops.len())];
        let mut out = Vec::new();
        let first =
            file_tops.partition_point(|&top| top + self.metrics.file_header_height < visible_top);
        for (file_index, &top) in file_tops.iter().enumerate().skip(first) {
            if top > visible_bottom {
                break;
            }
            out.push((file_index, bounds.y + (top - visible_top)));
        }
        // Mirror draw()'s sticky pin: that file's header sits pinned, not at
        // its natural offset.
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

    /// The browse button under `point`, if any.
    fn browse_button_at<P>(
        &self,
        state: &State<P>,
        bounds: Rectangle,
        point: Point,
    ) -> Option<usize> {
        self.on_browse_file?;
        let headers = {
            let index = state.height_index.borrow();
            self.visible_file_header_positions(&index, bounds, state.vertical_offset)
        };
        headers.into_iter().find_map(|(file_index, y)| {
            self.browse_button_rect(file_index, y, bounds)
                .filter(|rect| rect.contains(point))
                .map(|_| file_index)
        })
    }

    /// Update the selection focus based on the cursor's current screen
    /// position. Handles word/line-mode expansion: in those modes the
    /// selection always covers full units in both directions, so dragging
    /// past the next word boundary jumps an entire word at once.
    fn advance_drag_selection<P>(
        &self,
        state: &mut State<P>,
        cursor_pos: Point,
        bounds: Rectangle,
    ) {
        let focus_pos = {
            let index = state.height_index.borrow();
            self.position_at_point(
                &index,
                cursor_pos,
                bounds,
                state.vertical_offset,
                state.horizontal_offset,
                // The drag stays in the column the selection started in.
                state.selection_lane,
            )
        };
        let Some((focus_pos, _)) = focus_pos else {
            return;
        };

        let (Some(anchor_start), Some(anchor_end)) = (
            state.selection_anchor_unit_start,
            state.selection_anchor_unit_end,
        ) else {
            return;
        };

        let unit = state.selection_unit;
        let (focus_start, focus_end) = if unit == SelectionUnit::Character {
            (focus_pos, focus_pos)
        } else {
            self.expand_to_unit(focus_pos, unit)
        };

        let (new_anchor, new_focus) = if focus_end <= anchor_start {
            // Drag has crossed to the left of the original click — selection
            // grows from the *end* of the anchor unit backwards to the
            // *start* of the focus unit.
            (anchor_end, focus_start)
        } else if focus_start >= anchor_end {
            // Drag is to the right of the anchor unit.
            (anchor_start, focus_end)
        } else {
            // Drag is still inside the original anchor unit.
            (anchor_start, anchor_end)
        };

        if state.selection_anchor != Some(new_anchor) || state.selection_focus != Some(new_focus) {
            state.selection_anchor = Some(new_anchor);
            state.selection_focus = Some(new_focus);
        }
    }

    fn make_text(
        &self,
        content: &str,
        width: f32,
        height: f32,
        wrapping: text::Wrapping,
    ) -> text::Text<String, Font> {
        text::Text {
            content: content.to_owned(),
            bounds: Size::new(width.max(1.0), height.max(1.0)),
            size: Pixels(self.typography.size),
            line_height: text::LineHeight::Absolute(Pixels(height.min(self.metrics.row_height))),
            font: self.font,
            align_x: text::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        }
    }

    fn draw_background<Renderer>(
        &self,
        renderer: &mut Renderer,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
    ) where
        Renderer: renderer::Renderer,
    {
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x,
                    y,
                    width,
                    height,
                },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            color,
        );
    }

    /// Draw one file's header strip (background + bottom border + title +
    /// stats) at screen-y `y`. Shared by the normal scrolling draw and the
    /// pinned sticky draw.
    fn draw_file_header<Renderer>(
        &self,
        renderer: &mut Renderer,
        file_index: usize,
        y: f32,
        bounds: Rectangle,
        browse_hovered: bool,
    ) where
        Renderer: text::Renderer<Font = Font> + geometry::Renderer,
    {
        let file = &self.files[file_index];
        let (additions, deletions, hunk_label, summary_width) =
            self.file_header_summary(file, bounds);
        // Tight monospace widths (like the bookmark chips) — `text_width`
        // adds breathing room meant for wrapped text.
        let mono_width = |content: &str| content.chars().count() as f32 * self.metrics.char_width;
        let gap = self.metrics.char_width;
        let additions_width = mono_width(&additions);
        let deletions_width = mono_width(&deletions);
        let hunk_width = mono_width(&hunk_label);

        self.draw_background(
            renderer,
            bounds.x,
            y,
            bounds.width,
            self.metrics.file_header_height,
            self.palette.file_header,
        );
        self.draw_background(
            renderer,
            bounds.x,
            y + self.metrics.file_header_height - 1.0,
            bounds.width,
            1.0,
            self.palette.border,
        );

        let text_y = centered_text_y(y, self.metrics.file_header_height, self.metrics.row_height);
        let status_chip = Chip {
            label: file.status.short_label().to_owned(),
            font: self.font,
            background: file.status_fill,
            text_color: file.status_color,
            border_color: None,
            border_dashed: false,
            icon: None,
        };
        let chip_w = chip::draw(
            renderer,
            &status_chip,
            bounds.x + 12.0,
            y + self.metrics.file_header_height / 2.0,
            bounds,
        );

        let browse_rect = self.browse_button_rect(file_index, y, bounds);
        let browse_reserve = if browse_rect.is_some() {
            BROWSE_BUTTON_SIZE + 10.0
        } else {
            0.0
        };
        let title_x = bounds.x + 12.0 + chip_w + 8.0;
        let title_width =
            (bounds.width - (title_x - bounds.x) - summary_width - 16.0 - browse_reserve).max(1.0);
        // Dim the directory prefix so the basename carries the header —
        // the path tail is what distinguishes files at a glance. Renames
        // ("old -> new") and squeezed headers fall back to one plain run.
        let split_at = (!file.title.contains(" -> "))
            .then(|| file.title.rfind('/').map(|i| i + 1))
            .flatten()
            .filter(|&i| mono_width(&file.title) <= title_width && i < file.title.len());
        if let Some(split) = split_at {
            let (dir, base) = file.title.split_at(split);
            let dir_w = mono_width(dir);
            self.draw_text(
                renderer,
                dir,
                TextRenderParams {
                    width: dir_w.max(1.0),
                    height: self.metrics.row_height,
                    position: Point::new(title_x, text_y),
                    color: self.palette.text_muted,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::None,
                },
            );
            self.draw_text(
                renderer,
                base,
                TextRenderParams {
                    width: (title_width - dir_w).max(1.0),
                    height: self.metrics.row_height,
                    position: Point::new(title_x + dir_w, text_y),
                    color: self.palette.text,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::None,
                },
            );
        } else {
            self.draw_text(
                renderer,
                &file.title,
                TextRenderParams {
                    width: title_width,
                    height: self.metrics.row_height,
                    position: Point::new(title_x, text_y),
                    color: self.palette.text,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::WordOrGlyph,
                },
            );
        }

        if let Some(rect) = browse_rect {
            // Hover wash mirrors the app's ghost buttons: a translucent
            // muted-text tint behind the glyph.
            if browse_hovered {
                renderer.fill_quad(
                    iced::advanced::renderer::Quad {
                        bounds: rect,
                        border: Border {
                            radius: crate::theme::radius::CONTROL.into(),
                            ..Border::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Color {
                        a: 0.14,
                        ..self.palette.text_muted
                    },
                );
            }
            renderer.fill_text(
                text::Text {
                    content: crate::icons::CODE.to_owned(),
                    bounds: Size::new(rect.width, rect.height),
                    size: Pixels(BROWSE_ICON_SIZE),
                    line_height: text::LineHeight::Absolute(Pixels(rect.height)),
                    font: crate::icons::ICON_FONT,
                    align_x: text::Alignment::Center,
                    align_y: alignment::Vertical::Center,
                    shaping: text::Shaping::Basic,
                    wrapping: text::Wrapping::None,
                    ellipsis: text::Ellipsis::None,
                    hint_factor: None,
                },
                Point::new(rect.center_x(), rect.center_y()),
                if browse_hovered {
                    self.palette.text
                } else {
                    self.palette.text_muted
                },
                bounds,
            );
        }

        let summary_x = (bounds.x + bounds.width - summary_width - 8.0).max(bounds.x + 12.0);
        let segments = [
            (
                additions.as_str(),
                additions_width,
                self.palette.addition_text,
            ),
            (
                deletions.as_str(),
                deletions_width,
                self.palette.deletion_text,
            ),
            (hunk_label.as_str(), hunk_width, self.palette.text_muted),
        ];
        let mut segment_x = summary_x;
        for (index, (content, width, color)) in segments.into_iter().enumerate() {
            self.draw_text(
                renderer,
                content,
                TextRenderParams {
                    width,
                    height: self.metrics.row_height,
                    position: Point::new(segment_x, text_y),
                    color,
                    clip_bounds: bounds,
                    wrapping: text::Wrapping::None,
                },
            );
            segment_x += width + if index == 0 { gap } else { 2.0 * gap };
        }
    }

    fn draw_text<Renderer>(&self, renderer: &mut Renderer, content: &str, render: TextRenderParams)
    where
        Renderer: text::Renderer<Font = Font>,
    {
        renderer.fill_text(
            self.make_text(content, render.width, render.height, render.wrapping),
            render.position,
            render.color,
            render.clip_bounds,
        );
    }

    fn draw_description_edit_label<Renderer>(
        &self,
        renderer: &mut Renderer,
        bounds: Rectangle,
        clip_bounds: Rectangle,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        let label = "Edit";
        let label_width = measure::line_width(label, HEADER_EDIT_LABEL_SIZE, self.font);
        let content_width = HEADER_EDIT_ICON_SIZE + HEADER_EDIT_CONTENT_GAP + label_width;
        let content_x = bounds.center_x() - content_width / 2.0;
        let center_y = bounds.center_y();

        renderer.fill_text(
            text::Text {
                content: icons::PENCIL.to_owned(),
                bounds: Size::new(HEADER_EDIT_ICON_SIZE, HEADER_EDIT_ICON_SIZE),
                size: Pixels(HEADER_EDIT_ICON_SIZE),
                line_height: text::LineHeight::Absolute(Pixels(HEADER_EDIT_ICON_SIZE)),
                font: icons::ICON_FONT,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Center,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::None,
                ellipsis: text::Ellipsis::None,
                hint_factor: None,
            },
            Point::new(content_x, center_y),
            self.palette.text_muted,
            clip_bounds,
        );
        renderer.fill_text(
            text::Text {
                content: label.to_owned(),
                bounds: Size::new(label_width.max(1.0), bounds.height),
                size: Pixels(HEADER_EDIT_LABEL_SIZE),
                line_height: text::LineHeight::Absolute(Pixels(bounds.height)),
                font: self.font,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Center,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::None,
                ellipsis: text::Ellipsis::None,
                hint_factor: None,
            },
            Point::new(
                content_x + HEADER_EDIT_ICON_SIZE + HEADER_EDIT_CONTENT_GAP,
                center_y,
            ),
            self.palette.text,
            clip_bounds,
        );
    }

    fn draw_code_text<Renderer>(
        &self,
        renderer: &mut Renderer,
        line: &DiffLine,
        render: TextRenderParams,
        cache_key: ParagraphKey,
        paragraph_cache: &RefCell<std::collections::HashMap<ParagraphKey, Renderer::Paragraph>>,
        paragraph_seen: &mut std::collections::HashSet<ParagraphKey>,
    ) where
        Renderer: text::Renderer<Font = Font>,
    {
        if line.syntax.is_empty() {
            self.draw_text(renderer, &line.content, render);
            return;
        }

        let spans = self.syntax_spans(&line.content, &line.syntax);
        if spans.is_empty() {
            self.draw_text(renderer, &line.content, render);
            return;
        }

        paragraph_seen.insert(cache_key);
        let mut cache = paragraph_cache.borrow_mut();
        let paragraph = cache.entry(cache_key).or_insert_with(|| {
            <Renderer::Paragraph as text::Paragraph>::with_spans(text::Text {
                content: spans.as_slice(),
                bounds: Size::new(render.width.max(1.0), render.height.max(1.0)),
                size: Pixels(self.typography.size),
                line_height: text::LineHeight::Absolute(Pixels(
                    render.height.min(self.metrics.row_height),
                )),
                font: self.font,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Top,
                shaping: text::Shaping::Basic,
                wrapping: render.wrapping,
                ellipsis: text::Ellipsis::None,
                hint_factor: None,
            })
        });

        renderer.fill_paragraph(paragraph, render.position, render.color, render.clip_bounds);
    }

    fn syntax_spans<'a>(
        &self,
        content: &'a str,
        syntax: &'a [SyntaxSpan],
    ) -> Vec<text::Span<'a, (), Font>> {
        let mut spans = Vec::with_capacity(syntax.len().saturating_mul(2).saturating_add(1));
        let mut cursor = 0;

        for span in syntax {
            if span.start < cursor
                || span.start >= span.end
                || span.end > content.len()
                || !content.is_char_boundary(span.start)
                || !content.is_char_boundary(span.end)
            {
                continue;
            }

            if cursor < span.start {
                spans.push(text::Span::new(&content[cursor..span.start]));
            }

            spans.push(
                text::Span::new(&content[span.start..span.end]).color(self.syntax_color(span.kind)),
            );
            cursor = span.end;
        }

        if cursor < content.len() {
            spans.push(text::Span::new(&content[cursor..]));
        }

        spans
    }

    fn line_text_color(&self, kind: DiffLineKind) -> Color {
        match kind {
            DiffLineKind::Addition => self.palette.addition_text,
            DiffLineKind::Deletion => self.palette.deletion_text,
            DiffLineKind::Conflict => self.palette.conflict_marker,
            DiffLineKind::Context => self.palette.text,
            DiffLineKind::Note => self.palette.note_text,
        }
    }

    fn changed_line_background_color(&self, kind: DiffLineKind) -> Option<Color> {
        match kind {
            DiffLineKind::Addition => Some(self.palette.addition_background),
            DiffLineKind::Deletion => Some(self.palette.deletion_background),
            DiffLineKind::Conflict => Some(self.palette.note_background),
            DiffLineKind::Note => Some(self.palette.note_background),
            DiffLineKind::Context => None,
        }
    }

    fn syntax_color(&self, kind: SyntaxKind) -> Color {
        match kind {
            SyntaxKind::Comment => self.palette.text_muted,
            SyntaxKind::String => self.palette.syntax_literal,
            SyntaxKind::Number => self.palette.syntax_literal,
            SyntaxKind::Keyword => self.palette.syntax_keyword,
            SyntaxKind::Function => self.palette.syntax_function,
            SyntaxKind::Type => self.palette.syntax_type,
            SyntaxKind::Property => self.palette.syntax_property,
            SyntaxKind::Punctuation => self.palette.text_muted,
        }
    }
}

/// How many characters fit on one visual line at the current monospace
/// glyph advance. Mirrors `row_height`'s wrap-count math so hit-tests and
/// selection geometry stay consistent with the way rows are laid out.
fn chars_per_visual_line(content_width: f32, cw: f32) -> usize {
    (content_width / cw.max(1.0)).floor().max(1.0) as usize
}

/// Per-column x-offsets of the side-by-side layout, relative to the pane's
/// left edge. See `DiffView::split_layout`. (The left gutter sits at 0.)
struct SplitLayout {
    gutter_width: f32,
    text_width: f32,
    left_text_x: f32,
    divider_x: f32,
    right_gutter_x: f32,
    right_text_x: f32,
}

/// Shared geometry for byte-range background rects (find matches, intra-line
/// emphasis): the char-grid parameters that map a char range on a wrapped
/// row to screen rectangles.
struct HighlightGeometry {
    char_width: f32,
    chars_per_line: usize,
    text_x: f32,
    clip_left: f32,
    clip_right: f32,
    row_height: f32,
}

fn push_visible_band(bands: &mut Vec<VisibleBand>, kind: DiffLineKind, y: f32, height: f32) {
    if kind == DiffLineKind::Context {
        return;
    }

    match bands.last_mut() {
        Some(band) if band.kind == kind && (band.y + band.height - y).abs() < 0.5 => {
            band.height += height;
        }
        _ => bands.push(VisibleBand { kind, y, height }),
    }
}

fn format_gutter(old_line: Option<usize>, new_line: Option<usize>, digit_count: usize) -> String {
    use core::fmt::Write as _;
    // Two right-aligned, space-padded columns in one buffer. Writing the
    // numbers straight in avoids the two throwaway `to_string()` allocations
    // this once did on every visible row.
    let mut out = String::with_capacity(digit_count * 2 + 1);
    if let Some(line) = old_line {
        let _ = write!(out, "{line:>digit_count$}");
    } else {
        out.extend(std::iter::repeat_n(' ', digit_count));
    }
    out.push(' ');
    if let Some(line) = new_line {
        let _ = write!(out, "{line:>digit_count$}");
    } else {
        out.extend(std::iter::repeat_n(' ', digit_count));
    }
    out
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

/// Maximum line-number digit count across all visible files. Used to size
/// the gutter just wide enough for the largest line number plus a small
/// padding, so a 30k-line file isn't truncated and a 50-line file doesn't
/// waste a third of the viewport on whitespace.
fn compute_gutter_digit_count(files: &[DiffFileView<'_>]) -> usize {
    // Runs on every `view()` rebuild, so it must not walk the whole diff.
    // Line numbers grow monotonically through a file's hunks, so each file's
    // maximum lives at the *tail of its last hunk* — scan that tail backwards
    // until both counters have been seen. The cap bounds the pathological
    // single-sided hunk (e.g. a 1M-line pure addition never carries an old
    // number); past it the other counter is at most one digit off anyway,
    // and the found side dominates the gutter width.
    const TAIL_SCAN_CAP: usize = 4_096;
    let mut max_line = 0usize;
    for file in files {
        let Some(hunk) = file.hunks.last() else {
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

fn char_count_at_byte(content: &str, byte: usize) -> usize {
    let cap = byte.min(content.len());
    content
        .char_indices()
        .take_while(|(idx, _)| *idx < cap)
        .count()
}

fn ordered(a: TextPosition, b: TextPosition) -> (TextPosition, TextPosition) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Translate a character index inside `content` to a byte offset, clamped
/// to the string length. Used so the click-to-position logic can store
/// byte offsets (cheap to slice) while the hit-test math operates in chars
/// (which is what monospace `x / char_width` gives us).
fn byte_offset_for_char(content: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    let mut bytes = 0;
    for (i, ch) in content.chars().enumerate() {
        if i == char_index {
            return bytes;
        }
        bytes += ch.len_utf8();
    }
    bytes.min(content.len())
}

fn is_copy_shortcut(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> bool {
    if !modifiers.command() {
        return false;
    }
    matches!(
        key.as_ref(),
        keyboard::Key::Character("c") | keyboard::Key::Character("C")
    )
}

/// Find the inclusive [start, end) byte range of the word the cursor is
/// currently inside. "Word" is the run of characters with the same
/// `word_class` as the character under the cursor, where word_class folds
/// identifier chars (alphanumeric + `_`) into one bucket, whitespace into
/// another, and everything else (punctuation, operators) into a third.
/// Clicking on whitespace selects that whitespace run; clicking on
/// punctuation selects that punctuation run; clicking on a letter selects
/// the identifier — matching the convention every text editor uses.
fn word_bounds(content: &str, byte_pos: usize) -> (usize, usize) {
    if content.is_empty() {
        return (0, 0);
    }
    let byte_pos = byte_pos.min(content.len());

    // Anchor on the char under the cursor: floor the click to a char boundary
    // (it may land mid-glyph), stepping back onto the last char when the cursor
    // sits at the very end so double-clicking past the last word still selects
    // it. Walk the `&str` directly instead of collecting every (offset, char).
    let mut anchor_off = byte_pos;
    if anchor_off == content.len() {
        anchor_off -= 1;
    }
    while !content.is_char_boundary(anchor_off) {
        anchor_off -= 1;
    }
    let anchor_ch = content[anchor_off..].chars().next().unwrap_or(' ');
    let target_class = word_class(anchor_ch);

    // Expand to the maximal run sharing the anchor's word class, scanning left
    // (reverse char iterator) and right from the anchor.
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

/// Returns the y-delta (in px) to apply to `vertical_offset` on this frame
/// based on how far past the top/bottom edge the cursor sits. Zero when the
/// cursor is inside the viewport. Frame-rate–independent: assumes ~60fps,
/// which is fine for selection feel.
fn auto_scroll_delta(cursor_y: f32, bounds: Rectangle) -> f32 {
    let frame_seconds = 1.0 / 60.0;
    let bottom = bounds.y + bounds.height;
    let speed_for = |distance: f32| {
        let ramp = (distance / AUTO_SCROLL_RAMP_PX).clamp(0.0, 1.0);
        ramp * AUTO_SCROLL_MAX_SPEED * frame_seconds
    };
    if cursor_y < bounds.y {
        -speed_for(bounds.y - cursor_y)
    } else if cursor_y > bottom {
        speed_for(cursor_y - bottom)
    } else {
        0.0
    }
}

fn measure_char_advance_cached(font: Font, text_size: f32) -> f32 {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(Font, u32), f32>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (font, text_size.to_bits());
    // One lock acquisition for the get/insert pair. `measure_char_advance`
    // doesn't touch this cache, so holding the (uncontended, layout-thread)
    // guard across the miss path is safe and saves a re-lock.
    let mut cache = cache.lock().unwrap();
    if let Some(&v) = cache.get(&key) {
        return v;
    }
    let v = measure_char_advance(font, text_size);
    cache.insert(key, v);
    v
}

fn measure_char_advance(font: Font, text_size: f32) -> f32 {
    // "M" is a stable choice for monospace measurement: it dominates hinting
    // noise at small sizes. For monospace fonts the width of one char *is*
    // the advance, which is what we cache here. `Shaping::Basic` matches how
    // the grid draws its rows.
    measure::line_width_shaped("M", text_size, font, text::Shaping::Basic)
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

fn centered_text_y(container_y: f32, container_height: f32, text_height: f32) -> f32 {
    container_y + (container_height - text_height) / 2.0
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

impl<'a, Message, Renderer> From<DiffView<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Renderer: text::Renderer<Font = Font> + geometry::Renderer + 'a,
{
    fn from(diff_view: DiffView<'a, Message>) -> Self {
        Element::new(diff_view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typography config drives the layout metrics: default values stay
    /// pixel-identical to the historical constants, a custom line height
    /// splits its leading delta evenly around the text, and the baseline
    /// offset shifts it verbatim.
    #[test]
    fn layout_metrics_follow_code_typography() {
        use crate::config::CodeTypography;
        let metrics = |typography| LayoutMetrics::new(typography, 3, Font::MONOSPACE, false);

        let default = metrics(CodeTypography::default());
        assert_eq!(default.row_height, 22.0, "12px × 1.85 rounds to 22");
        assert_eq!(default.text_y_pad, TEXT_Y_PADDING);

        let airy = metrics(CodeTypography {
            line_height: 2.35,
            ..CodeTypography::default()
        });
        assert_eq!(airy.row_height, 28.0, "12px × 2.35 rounds to 28");
        // 6px of extra leading, half above the text.
        assert_eq!(airy.text_y_pad, TEXT_Y_PADDING + 3.0);

        let nudged = metrics(CodeTypography {
            baseline_offset: -1.5,
            ..CodeTypography::default()
        });
        assert_eq!(nudged.row_height, default.row_height);
        assert_eq!(nudged.text_y_pad, TEXT_Y_PADDING - 1.5);

        let bigger = metrics(CodeTypography {
            size: 16.0,
            ..CodeTypography::default()
        });
        assert_eq!(bigger.row_height, 30.0, "16px × 1.85 rounds to 30");
        // Size alone keeps the historical fixed padding (the default-ratio
        // delta is zero by construction), matching the pre-config behavior
        // for any `text_size`.
        assert_eq!(bigger.text_y_pad, TEXT_Y_PADDING);
    }

    pub(super) fn test_palette() -> Palette {
        let c = Color::WHITE;
        Palette {
            text: c,
            text_muted: c,
            addition_text: c,
            deletion_text: c,
            modified_token: c,
            conflict_marker: c,
            note_text: c,
            syntax_keyword: c,
            syntax_type: c,
            syntax_function: c,
            syntax_literal: c,
            syntax_property: c,
            panel: c,
            file_header: c,
            hunk_header: c,
            addition_background: c,
            deletion_background: c,
            addition_emphasis: c,
            deletion_emphasis: c,
            note_background: c,
            gutter_background: c,
            border: c,
            selection: c,
            scrollbar: ScrollbarStyle {
                track_color: c,
                thumb_color: c,
            },
        }
    }

    fn line(kind: DiffLineKind, content: &str, n: usize) -> DiffLine {
        DiffLine {
            kind,
            old_line: Some(n),
            new_line: Some(n),
            content: content.to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        }
    }

    /// A doc with several files/hunks and one long line that wraps, so the
    /// index has non-uniform row heights to account for.
    fn test_hunks() -> Vec<Vec<DiffHunkView>> {
        let long = "x".repeat(500);
        vec![
            vec![DiffHunkView {
                header: "@@ -1,3 +1,3 @@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Context, "alpha", 1),
                    line(DiffLineKind::Addition, &long, 2),
                    line(DiffLineKind::Deletion, "gamma", 3),
                ],
            }],
            vec![
                DiffHunkView {
                    header: "@@ -10,2 +10,2 @@".to_owned(),
                    lines: vec![
                        line(DiffLineKind::Context, "delta", 10),
                        line(DiffLineKind::Addition, "", 11),
                    ],
                },
                DiffHunkView {
                    header: "@@ -40,1 +40,1 @@".to_owned(),
                    lines: vec![line(DiffLineKind::Context, "epsilon", 40)],
                },
            ],
        ]
    }

    fn test_view(hunks: &[Vec<DiffHunkView>]) -> DiffView<'_, ()> {
        let files = hunks
            .iter()
            .enumerate()
            .map(|(i, hunks)| DiffFileView {
                title: format!("file-{i}"),
                status: DiffFileStatus::Modified,
                status_color: Color::WHITE,
                status_fill: Color::TRANSPARENT,
                hunks,
                additions: 1,
                deletions: 1,
            })
            .collect();
        DiffView::new(
            files,
            0,
            "test",
            test_palette(),
            Font::MONOSPACE,
            crate::config::CodeTypography {
                size: 13.0,
                ..crate::config::CodeTypography::default()
            },
            500,
            |_| (),
        )
    }

    /// Brute-force row walk mirroring the pre-index geometry, used as the
    /// oracle the prefix sums must agree with.
    fn brute_force_tops(view: &DiffView<'_, ()>, content_width: f32) -> (Vec<f32>, Vec<f32>, f32) {
        let mut file_tops = Vec::new();
        let mut row_tops = Vec::new();
        let mut y = view.header_height();
        for file in &view.files {
            file_tops.push(y);
            y += view.metrics.file_header_height;
            for hunk in file.hunks {
                y += view.metrics.hunk_header_height;
                for l in &hunk.lines {
                    row_tops.push(y);
                    y += view.row_height(l, content_width);
                }
            }
        }
        (file_tops, row_tops, y)
    }

    #[test]
    fn height_index_matches_brute_force_walk() {
        let hunks = test_hunks();
        let view = test_view(&hunks);
        let width = 400.0;
        let content_width = view.content_width(width);

        let cell = RefCell::new(HeightIndex::default());
        view.ensure_height_index(&cell, width);
        let index = cell.borrow();

        let (file_tops, row_tops, total) = brute_force_tops(&view, content_width);
        assert_eq!(&index.file_tops[..file_tops.len()], file_tops.as_slice());
        assert_eq!(index.file_tops.last().copied(), Some(total));
        assert_eq!(index.row_tops, row_tops);
        assert_eq!(index.total_height, total);
        // The long line wraps: its row is taller than one row height, so the
        // following row's top reflects the wrap.
        assert!(index.row_tops[2] - index.row_tops[1] > view.metrics.row_height * 1.5);

        // file_offset / file_at_offset agree with the prefix sums.
        assert_eq!(view.file_offset(&index, 0), file_tops[0]);
        assert_eq!(view.file_offset(&index, 1), file_tops[1]);
        assert_eq!(view.file_at_offset(&index, 0.0), 0);
        assert_eq!(view.file_at_offset(&index, file_tops[1] - 1.0), 0);
        assert_eq!(view.file_at_offset(&index, file_tops[1]), 1);
        assert_eq!(view.file_at_offset(&index, total + 100.0), 1);

        // Row lookup by id and by y agree with the walk.
        for (i, &(f, h, l)) in index.row_ids.iter().enumerate() {
            assert_eq!(
                view.match_target_y(&index, f as usize, h as usize, l as usize, 0),
                Some(index.row_tops[i]),
            );
            assert_eq!(index.row_at(index.row_tops[i] + 0.5), Some(i));
        }
        // A y above the first row (in the file-0 header band) still resolves
        // to no candidate row... the candidate is `None` only above row 0.
        assert_eq!(index.row_at(index.row_tops[0] - 1.0), None);
    }

    #[test]
    fn height_index_rebuilds_on_shape_or_width_change() {
        let hunks = test_hunks();
        let mut view = test_view(&hunks);
        let cell = RefCell::new(HeightIndex::default());

        view.ensure_height_index(&cell, 400.0);
        let narrow_total = cell.borrow().total_height;
        let narrow_key = cell.borrow().key;

        // Wider viewport: the 500-char line wraps fewer times, so the total
        // shrinks and the key changes.
        view.ensure_height_index(&cell, 4000.0);
        assert_ne!(cell.borrow().key, narrow_key);
        assert!(cell.borrow().total_height < narrow_total);

        // Same shape + width: cache hit, key stable.
        let key = cell.borrow().key;
        view.ensure_height_index(&cell, 4000.0);
        assert_eq!(cell.borrow().key, key);

        // A replaced document (new layout id) must rebuild.
        view.layout_version = 7;
        view.ensure_height_index(&cell, 4000.0);
        assert_ne!(cell.borrow().key, key);
    }

    #[test]
    fn header_spacer_reserves_variable_height_and_maps_following_rows() {
        let hunks = test_hunks();
        let mut view = test_view(&hunks);
        view.header = vec![
            HeaderLine::spacer(150.0),
            HeaderLine::field("commit", "abc"),
        ];

        assert_eq!(
            view.header_height(),
            HEADER_VERTICAL_PADDING * 2.0 + 150.0 + view.metrics.row_height
        );
        assert_eq!(
            view.header_line_at_y(HEADER_VERTICAL_PADDING + 149.0),
            Some(0)
        );
        assert_eq!(
            view.header_line_at_y(HEADER_VERTICAL_PADDING + 150.0),
            Some(1)
        );
    }

    #[test]
    fn side_by_side_pairs_runs_and_pads() {
        let hunks = vec![vec![DiffHunkView {
            header: "@@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "ctx", 1),
                line(DiffLineKind::Deletion, "old a", 2),
                line(DiffLineKind::Deletion, "old b", 3),
                line(DiffLineKind::Addition, "new a", 2),
                line(DiffLineKind::Addition, "new b", 3),
                line(DiffLineKind::Addition, "new c", 4),
                line(DiffLineKind::Context, "tail", 5),
            ],
        }]];
        let mut view = test_view(&hunks);
        view.side_by_side = true;
        let cell = RefCell::new(HeightIndex::default());
        view.ensure_height_index(&cell, 800.0);

        let index = cell.borrow();
        // ctx + 3 pairs (2 del × 3 add) + tail.
        assert_eq!(index.row_tops.len(), 5);
        assert_eq!(
            index.pair_lines,
            vec![(0, 0), (1, 3), (2, 4), (NO_LINE, 5), (6, 6)]
        );
        // Reps stay sorted, and the padded pair is keyed by its right member.
        assert_eq!(
            index.row_ids,
            vec![(0, 0, 0), (0, 0, 1), (0, 0, 2), (0, 0, 5), (0, 0, 6)]
        );
        // Both members of a pair resolve to the same row.
        assert_eq!(index.row_of_line(0, 0, 3), Some(1));
        assert_eq!(index.row_of_line(0, 0, 1), Some(1));
        assert_eq!(index.row_of_line(0, 0, 5), Some(3));
        assert_eq!(index.row_of_line(0, 0, 6), Some(4));
    }

    #[test]
    fn side_by_side_addition_only_runs_pad_left() {
        // Additions with no deletion run in front belong to the right
        // column alone — they must not mirror into the left side.
        let hunks = vec![vec![DiffHunkView {
            header: "@@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "ctx", 1),
                line(DiffLineKind::Addition, "new a", 2),
                line(DiffLineKind::Addition, "new b", 3),
                line(DiffLineKind::Context, "tail", 4),
            ],
        }]];
        let mut view = test_view(&hunks);
        view.side_by_side = true;
        let cell = RefCell::new(HeightIndex::default());
        view.ensure_height_index(&cell, 800.0);

        let index = cell.borrow();
        assert_eq!(
            index.pair_lines,
            vec![(0, 0), (NO_LINE, 1), (NO_LINE, 2), (3, 3)]
        );
    }

    #[test]
    fn side_by_side_single_sided_files_render_full_width() {
        // File 0 is additions-only (a new file): it renders as one
        // full-width column. File 1 mixes kinds and stays split.
        let long = "x".repeat(120);
        let hunks = vec![
            vec![DiffHunkView {
                header: "@@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Addition, "a", 1),
                    line(DiffLineKind::Addition, &long, 2),
                ],
            }],
            vec![DiffHunkView {
                header: "@@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Context, "ctx", 1),
                    line(DiffLineKind::Deletion, "old", 2),
                    line(DiffLineKind::Addition, "new", 2),
                ],
            }],
        ];
        let mut view = test_view(&hunks);
        view.side_by_side = true;
        let cell = RefCell::new(HeightIndex::default());
        view.ensure_height_index(&cell, 800.0);

        let index = cell.borrow();
        assert_eq!(index.full_width_files, vec![true, false]);
        // Full-width rows keep `pair_lines` aligned via identity pairs; the
        // mixed file still pairs its deletion/addition run.
        assert_eq!(index.pair_lines, vec![(0, 0), (1, 1), (0, 0), (1, 2)]);
        // Full-width rows measure against the whole text area, not a column:
        // the long line wraps less (or not at all) compared to a split row.
        assert!(index.unified_text_width > index.split_text_width);
        assert_eq!(
            view.index_row_height(&index, 1),
            view.row_height_for_chars(120, index.unified_text_width)
        );
        assert!(
            view.row_height_for_chars(120, index.split_text_width)
                > view.index_row_height(&index, 1)
        );
    }

    #[test]
    fn split_selection_copies_only_its_column() {
        let hunks = vec![vec![DiffHunkView {
            header: "@@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "ctx", 1),
                line(DiffLineKind::Deletion, "old", 2),
                line(DiffLineKind::Addition, "new", 2),
                line(DiffLineKind::Context, "tail", 3),
            ],
        }]];
        let mut view = test_view(&hunks);
        view.side_by_side = true;
        let start = body_position(0, 0, 0, 0);
        let end = body_position(0, 0, 3, 4);
        assert_eq!(
            view.collect_selected_text(start, end, Some(SplitSide::Right)),
            "ctx\nnew\ntail"
        );
        assert_eq!(
            view.collect_selected_text(start, end, Some(SplitSide::Left)),
            "ctx\nold\ntail"
        );
        // No side (unified mode): everything in range, both columns.
        assert_eq!(
            view.collect_selected_text(start, end, None),
            "ctx\nold\nnew\ntail"
        );
    }

    #[test]
    fn no_wrap_makes_rows_uniform_height() {
        let hunks = test_hunks();
        let mut view = test_view(&hunks);
        let cell = RefCell::new(HeightIndex::default());

        // Narrow viewport: the 500-char line wraps, so heights are mixed.
        view.ensure_height_index(&cell, 400.0);
        let wrapped_key = cell.borrow().key;
        let wrapped_total = cell.borrow().total_height;

        // Wrap off: same width, new key, every row exactly one line tall.
        view.wrap = false;
        view.ensure_height_index(&cell, 400.0);
        assert_ne!(cell.borrow().key, wrapped_key);
        assert!(cell.borrow().total_height < wrapped_total);
        {
            let index = cell.borrow();
            let row_count = index.row_tops.len();
            for i in 1..row_count {
                let delta = index.row_tops[i] - index.row_tops[i - 1];
                // Consecutive rows within one hunk sit exactly one row apart;
                // larger gaps are hunk/file header bands.
                assert!(
                    (delta - view.metrics.row_height).abs() < 0.01
                        || delta > view.metrics.row_height
                );
            }
            let line = &view.files[0].hunks[0].lines[0];
            assert!(
                (view.row_height(line, view.content_width(400.0)) - view.metrics.row_height).abs()
                    < 0.01
            );
        }
    }

    #[test]
    fn gutter_digits_use_hunk_tails() {
        let long = vec![
            DiffHunkView {
                header: String::new(),
                lines: vec![line(DiffLineKind::Context, "a", 5)],
            },
            DiffHunkView {
                header: String::new(),
                lines: vec![
                    line(DiffLineKind::Context, "b", 99_950),
                    // Trailing note without line numbers — the tail scan must
                    // step past it.
                    DiffLine {
                        kind: DiffLineKind::Note,
                        old_line: None,
                        new_line: None,
                        content: "\\ No newline at end of file".to_owned(),
                        syntax: Vec::new(),
                        emphasis: Vec::new(),
                    },
                ],
            },
        ];
        let files = vec![DiffFileView {
            title: "f".to_owned(),
            status: DiffFileStatus::Modified,
            status_color: Color::WHITE,
            status_fill: Color::TRANSPARENT,
            hunks: &long,
            additions: 0,
            deletions: 0,
        }];
        assert_eq!(compute_gutter_digit_count(&files), 5);
        assert_eq!(compute_gutter_digit_count(&[]), 3);
    }
}

/// Height-index scaling profile on a synthetic ~1M-line diff:
///   cargo test -p diffui profile_height_index -- --ignored --nocapture
#[cfg(test)]
mod height_profile {
    use super::*;

    #[test]
    #[ignore]
    fn profile_height_index() {
        let lines: Vec<DiffLine> = (0..1_000_000)
            .map(|i| DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(i + 1),
                new_line: Some(i + 1),
                content: format!("    let value_{i} = compute({i}) + offset; // padding padding"),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            })
            .collect();
        let hunks: Vec<DiffHunkView> = lines
            .chunks(5_000)
            .map(|chunk| DiffHunkView {
                header: "@@ @@".to_owned(),
                lines: chunk.to_vec(),
            })
            .collect();
        let files: Vec<Vec<DiffHunkView>> = hunks.chunks(20).map(<[_]>::to_vec).collect();
        let views: Vec<DiffFileView<'_>> = files
            .iter()
            .enumerate()
            .map(|(i, hunks)| DiffFileView {
                title: format!("file-{i}"),
                status: DiffFileStatus::Modified,
                status_color: Color::WHITE,
                status_fill: Color::TRANSPARENT,
                hunks,
                additions: 0,
                deletions: 0,
            })
            .collect();
        let view: DiffView<'_, ()> = DiffView::new(
            views,
            0,
            "profile",
            tests::test_palette(),
            Font::MONOSPACE,
            crate::config::CodeTypography {
                size: 13.0,
                ..crate::config::CodeTypography::default()
            },
            500,
            |_| (),
        );

        let cell = RefCell::new(HeightIndex::default());
        let t = std::time::Instant::now();
        view.ensure_height_index(&cell, 1200.0);
        let build = t.elapsed();

        let index = cell.borrow();
        let t = std::time::Instant::now();
        let mut acc = 0usize;
        for i in 0..10_000 {
            acc += view.file_at_offset(&index, (i * 137) as f32 % index.total_height);
            acc += index
                .row_at((i * 631) as f32 % index.total_height)
                .unwrap_or(0);
        }
        let queries = t.elapsed();

        eprintln!("\n=== height index profile (1M lines) ===");
        eprintln!("build (once per content/width change): {build:?}");
        eprintln!("20k mixed queries                    : {queries:?}  (sink {acc})");
        eprintln!(
            "per query                            : {:?}",
            queries / 20_000
        );
        eprintln!("=======================================\n");
    }
}
