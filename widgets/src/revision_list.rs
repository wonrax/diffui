//! Virtualized sidebar list rendering revisions with their graph gutter.
//!
//! Two reasons it's a single widget rather than a `column!` of buttons:
//!   - Only on-screen rows pay layout / draw cost.
//!   - Revisions and the file rows under the selected revision live in the
//!     same widget, so the graph gutter can run continuously through them.
//!
//! The widget owns its scroll offset in tree state. Click on a row fires
//! the `on_select_revision` / `on_select_file` callbacks the caller
//! supplied. Visual styling (id chip colours, description colour, file
//! status background) is computed by the caller and passed in per row, so
//! this module stays decoupled from the rest of the app's theming logic.

use std::rc::Rc;

use diffui_core::graph::LaneFrame;
use diffui_core::graph_layout::GraphEdgeType;

use crate::chip::Chip;
use crate::graph_view::RevisionGraphStyle;
use crate::scrollbar::ScrollbarStyle;
use crate::theme::{Color, Font};

pub const REVISION_ROW_HEIGHT: f32 = 46.0;
pub const FILE_ROW_HEIGHT: f32 = 26.0;
/// Selection/hover chrome is an inset rounded card, not an edge-to-edge
/// band: rows separate by whitespace, and the card gives the active row a
/// modern object-like read. Y-inset keeps consecutive cards from fusing
/// into a column; the X-inset equals the resulting card-to-card gap
/// (2 × Y) so the margin against the pane edges reads the same as the
/// spacing between rows.
pub const ROW_CARD_INSET_X: f32 = 2.0 * ROW_CARD_INSET_Y;
pub const ROW_CARD_INSET_Y: f32 = 1.0;
/// Corner radius of a row's card — and of the highlight a context menu puts
/// over it, which must sit exactly on the card.
pub const ROW_CARD_RADIUS: f32 = crate::theme::radius::PUSH;
/// Horizontal indent per tree depth level for file/directory rows.
pub const FILE_TREE_INDENT: f32 = 14.0;

pub const GUTTER_LEFT_PADDING: f32 = 8.0;
pub const GUTTER_PADDING: f32 = 8.0;
/// Right inset of row content: the scrollbar's gutter, so text stays clear
/// of the bar the row cards run under.
pub const CONTENT_PADDING: f32 = crate::scrollbar::GUTTER;
pub const SMALL_TEXT_SIZE: f32 = crate::theme::text_size::BODY_LG;
pub const CAPTION_TEXT_SIZE: f32 = crate::theme::text_size::CAPTION;
/// Gap between a revision row's description and its id line.
pub const ROW_LINE_GAP: f32 = 4.0;

pub const FILE_ROW_GAP: f32 = 6.0;
pub const FILE_ROW_RIGHT_PAD: f32 = crate::scrollbar::GUTTER;

#[derive(Debug, Clone, Copy)]
pub struct RevisionListStyle {
    pub graph: RevisionGraphStyle,
    pub background: Color,
    pub selected_background: Color,
    /// Saturated accent — drives the left-edge tint of the working-copy
    /// row's gradient and recolors the working-copy node disc in the
    /// graph.
    pub accent: Color,
    /// Fill of a checked change-selection box (see
    /// [`crate::diff_view::ChangeMarks::control`]).
    pub control: Color,
    pub border: Color,
    pub muted_text: Color,
    pub subtle_text: Color,
    pub accent_text: Color,
    pub primary_font: Font,
    pub mono_font: Font,
    pub file_badge_width: f32,
    pub tooltip_background: Color,
    pub tooltip_text: Color,
    pub tooltip_border: Color,
    pub scrollbar: ScrollbarStyle,
}

/// Key the widget reports back so the parent can map it to its own
/// selection enum without leaking app types into this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSelectionKey {
    WorkingCopy,
    Commit(String),
}

/// Where a revision drag would land, in commit-store indices. Reported to
/// the app on hover changes and on drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropSpot {
    /// Onto this revision (drop lands as its child).
    OnRow(usize),
    /// Into the gap between two adjacent revision rows: `above` is the newer
    /// (child-side) row, `below` the older (parent-side) one — the moved
    /// commits slot exactly between them.
    Gap { above: usize, below: usize },
}

/// Target-mode decoration for a revision row, computed by the caller from
/// the active draft (keyboard candidate + placement). The widget draws it;
/// live *drag* indicators are widget-internal instead (they follow the
/// cursor, not app state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftMarker {
    /// The destination ring: the moved commits become children of this row
    /// (rebase onto / squash into).
    Ring,
    /// Insert-after: the moved commits land just above this row.
    LineAbove,
    /// Insert-before: the moved commits land just below this row.
    LineBelow,
}

#[derive(Debug, Clone)]
pub enum Item {
    Revision(RevisionRowView),
    File(FileRowView),
}

// Pure row-geometry helpers, split out so they can be unit-tested without
// constructing a full widget. `expanded` is `(flat index where file rows
// start, file count)`; revision rows fill the flats on either side.

#[derive(Debug, Clone)]
pub struct RevisionRowView {
    pub selection_key: RowSelectionKey,
    pub change_id_prefix: String,
    pub change_id_suffix: String,
    pub commit_id_short: String,
    pub author: String,
    pub description: String,
    pub description_color: Color,
    /// Bookmark chips, in display order. May be partially hidden behind
    /// a synthetic `+N` overflow chip when there's not enough horizontal
    /// room — see the layout logic in `draw_revision`. The lane color
    /// these chips wear is also the color of the overflow chip.
    pub bookmark_chips: Vec<Chip>,
    /// Status chips (e.g. `empty`, `conflict`). Always shown — they're
    /// load-bearing semantics and shouldn't compress.
    pub status_chips: Vec<Chip>,
    /// Color for the `+N` overflow chip (= the row's lane color).
    pub lane_color: Color,
    pub frame: LaneFrame,
    /// Warped lane → display-column maps for this row and the previous one
    /// (`LaneFrame::display_columns`): x positions only — every lane-keyed
    /// lookup (labels, segments, colors) stays on original indices.
    pub columns: Vec<Option<usize>>,
    pub prev_columns: Vec<Option<usize>>,
    /// Bookmarks naming each lane visible at this row. Indexed by lane.
    /// An empty inner vec means the lane segment is anonymous (no
    /// bookmark up to this point in the walk). Used for the
    /// graph-stroke hover tooltip.
    pub lane_labels: Vec<Vec<String>>,
    /// Per-lane segment ids at this row, split by row half. The
    /// "incoming" half (`before`) covers strokes from the row's top edge
    /// down to the disc midline; the "outgoing" half (`after`) covers
    /// strokes from the disc midline down to the bottom edge. Two rows
    /// share an id on the same lane iff they belong to the same
    /// continuous lane segment.
    ///
    /// The two halves can disagree at a merge commit whose lane index
    /// is reused by an outgoing branch (the merged-in branch terminates
    /// in `before`, the new branch starts in `after`). Drawing emphasis
    /// at that row only highlights the half whose segment id matches the
    /// hovered branch — otherwise the merge stub of the dying branch
    /// would also glow when hovering the new branch.
    pub lane_segments_before: Vec<Option<usize>>,
    pub lane_segments_after: Vec<Option<usize>>,
    /// Whether to draw the inline collapse/expand chevron on the
    /// description row. `None` skips the glyph entirely (non-selected
    /// rows have no file list to toggle). `Some(true)` shows the
    /// "expanded" chevron, `Some(false)` shows the "collapsed" one.
    pub collapse_chevron: Option<bool>,
    /// Target mode: this row is a draft source (the thing being moved) —
    /// worn as an accent wash so it reads as "lifted off".
    pub draft_source: bool,
    /// The row is marked in the sidebar's multi-selection (⌘/⇧-click) —
    /// wears the same accent wash as a draft source: both mean "this row is
    /// part of the set the next action applies to".
    pub multi_selected: bool,
    /// Target mode: the keyboard candidate's destination decoration.
    pub draft_marker: Option<DraftMarker>,
    /// A previewed row that only shows the result (where commits land, a
    /// created commit, a ghost): it can't be selected or aimed at.
    pub inert: bool,
    /// A previewed ghost: where a moved or abandoned commit used to be,
    /// drawn faded with a hollow node.
    pub ghost: bool,
}

#[derive(Debug, Clone)]
pub struct FileRowView {
    /// Display label: a file's basename, or a directory's (possibly
    /// chain-compacted) name.
    pub primary: String,
    pub raw_path: String,
    pub status_label: String,
    pub status_background: Color,
    pub status_text: Color,
    pub additions: usize,
    pub deletions: usize,
    pub additions_text: Color,
    pub deletions_text: Color,
    /// Continuation lane state for the parent revision, shared (`Rc`) across
    /// every file row under it rather than deep-copied per row — the data is
    /// identical for all of them and these rows rebuild on every `view()`.
    pub continuation: Rc<[Option<GraphEdgeType>]>,
    /// Warped display columns of the parent revision row (see
    /// [`RevisionRowView::columns`]); the continuation strokes inherit its
    /// packing so the strip doesn't jump at the revision/file boundary.
    pub columns: Rc<[Option<usize>]>,
    pub additions_width: f32,
    pub deletions_width: f32,
    pub primary_color: Color,
    /// Tint of the file/folder icon ahead of the name (dimmer than the
    /// name for regular rows; follows the row tone for untracked/ignored).
    pub icon_color: Color,
    /// Tree indentation in px (depth × step), applied left of the badge.
    pub indent: f32,
    /// `Some(collapsed)` renders this as a directory row: a chevron instead
    /// of the status badge, no +/- stats. `None` is a file leaf.
    pub chevron: Option<bool>,
    /// The document file index for leaves (selection highlight + click);
    /// `usize::MAX` for directory rows, which never match a selection.
    pub file_index: usize,
    /// Lane → bookmark labels for the continuation strokes at this file
    /// row. Equals the post-trim snapshot of the parent revision's
    /// label state, so a lane that terminates at the parent isn't
    /// labeled here.
    pub lane_labels: Rc<[Vec<String>]>,
    /// Per-lane segment id, matching the parent revision row for any
    /// lane that survives into the continuation. See
    /// [`RevisionRowView::lane_segments`].
    pub lane_segments: Rc<[Option<usize>]>,
    /// Change-selection checkbox drawn in place of the file/folder icon, or
    /// `None` for no checkbox (see [`RevisionList::on_file_check`]).
    pub check: Option<crate::diff_view::CheckState>,
}
