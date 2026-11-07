//! The diff view's data types and pure geometry. Rendering is the diff lane's;
//! `crate::diff_view::DiffView` doesn't exist until then.

use crate::chip::Chip;
use crate::scrollbar::ScrollbarStyle;
use crate::theme::{Color, Font};

mod media;
pub use media::{Media, MediaImage, MediaSide};

// Padding above and below the revision-header block when it's present.
pub const HEADER_VERTICAL_PADDING: f32 = 12.0;
// Left/right padding inside the header block (between the panel edge and
// the first character of label/description text).
pub const HEADER_HORIZONTAL_PADDING: f32 = 16.0;

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

/// Tri-state of a change-selection checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckState {
    #[default]
    Off,
    /// Some of the file's hunks, or the hunk's lines, are picked.
    Partial,
    On,
}

/// A file's picks for the change-selection checkboxes.
#[derive(Debug, Clone, Default)]
pub struct FileMark {
    pub state: CheckState,
    pub hunks: HunkMarks,
}

/// Hunk-level checkboxes of one file.
#[derive(Debug, Clone, Default)]
pub enum HunkMarks {
    /// Picked whole only (binary, conflicted, mode-only changes): its hunk
    /// headers get no checkbox.
    #[default]
    None,
    /// Every hunk has a checkbox; none is picked.
    Unpicked,
    /// Every hunk has a checkbox; this is how much of each is picked.
    Picked(Vec<HunkMark>),
}

/// How much of one hunk is picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkMark {
    Off,
    On,
    /// Some of its changed lines: per row of the hunk, whether it is picked.
    Lines(Vec<bool>),
}

/// One diff row, by document index. Orders in document order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineAt {
    pub file: usize,
    pub hunk: usize,
    pub line: usize,
}

/// A press on a pickable changed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinePress {
    pub line: LineAt,
    /// Shift was held: extend from the last press instead of toggling.
    pub extend: bool,
    /// The press landed in one column of a side-by-side row, so a range
    /// from it should stay in that column.
    pub one_side: bool,
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
    /// A field of the commit info grid: a selectable value in colour runs,
    /// with badges, a link and buttons around it.
    Rich(Box<HeaderField>),
}

/// Height a header-only diff view needs to show `header` whole.
pub fn header_block_height(
    header: &[HeaderLine],
    typography: crate::typography::CodeTypography,
) -> f32 {
    if header.is_empty() {
        return 0.0;
    }
    let row_height = (typography.size * typography.line_height).round();
    header
        .iter()
        .map(|line| match line {
            HeaderLine::Spacer(height) => *height,
            _ => row_height,
        })
        .sum::<f32>()
        + HEADER_VERTICAL_PADDING * 2.0
}

/// What a press on a [`HeaderField`]'s link or button asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderAction {
    /// Copy this text.
    Copy(String),
    /// Open the revision with this commit id.
    Open(String),
}

/// One field of the commit info grid (see [`HeaderLine::Rich`]). Its label
/// shares the header's label column; its value stays selectable like the
/// plain fields'.
#[derive(Debug, Clone)]
pub struct HeaderField {
    pub label: String,
    /// The selectable text.
    pub value: String,
    /// Colour runs over `value` as (first char, colour), in order. Text
    /// before the first run, or all of it when empty, is in the text colour.
    pub runs: Vec<(usize, Color)>,
    /// Badges after the value (`divergent`, `hidden`).
    pub chips: Vec<Chip>,
    /// The value's first `n` characters are a link (a parent's id): a press
    /// on them fires the action instead of starting a selection.
    pub link: Option<(usize, HeaderAction)>,
    /// Icon buttons after the value and its badges.
    pub buttons: Vec<(&'static str, HeaderAction)>,
}

impl HeaderField {
    pub fn new(label: &str, value: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            value: value.into(),
            runs: Vec::new(),
            chips: Vec::new(),
            link: None,
            buttons: Vec::new(),
        }
    }
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

/// One line's entry in the blame column of a plain document: the change id,
/// author and age of the commit it came from, on the first line of each run
/// from one commit.
#[derive(Debug, Clone, Default)]
pub struct AnnotationLabel {
    /// Empty on the continuation lines of a run.
    pub id: String,
    /// How much of `id` is its shortest unique prefix, drawn brighter.
    pub unique: usize,
    pub author: String,
    pub age: String,
    /// The commit is no longer visible: drawn receding.
    pub hidden: bool,
}

/// Type and colours of the blame column. The id is mono, as the graph
/// draws it; the author and age are in `font`.
#[derive(Debug, Clone, Copy)]
pub struct AnnotationStyle {
    pub font: Font,
    /// The id's unique prefix, and the rest of it.
    pub id: Color,
    pub id_rest: Color,
    pub author: Color,
    pub age: Color,
    /// Every part of a hidden commit's entry.
    pub hidden: Color,
    /// Behind the run of lines of the entry under the cursor.
    pub hover: Color,
}

/// One find hit: a byte range on one line of the document.
#[derive(Debug, Clone)]
pub struct FindMatch {
    pub file_index: usize,
    pub hunk_index: usize,
    pub line_index: usize,
    /// Byte range within `DiffLine::content`. Inclusive-start, exclusive-end.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Per-render find-match data fed into `DiffView::with_find`. The widget
/// renders one rectangle per match on the lines it overlaps and scrolls
/// the active match into view when `scroll_token` changes between
/// renders.
#[derive(Debug, Clone)]
pub struct FindOverlay<'a> {
    pub matches: &'a [FindMatch],
    pub active: Option<usize>,
    /// Bumped by the caller whenever the active match changes. The widget
    /// keeps the previous token in its `State` and scrolls when the two
    /// disagree — this is how we trigger scroll without tearing into the
    /// widget's private state from outside.
    pub scroll_token: u64,
    pub highlight: Color,
    pub active_highlight: Color,
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
