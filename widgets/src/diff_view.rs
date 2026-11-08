//! The diff surface: one custom element, inside its own entity, painting a
//! continuous multi-file document. Sticky file headers, hunk strips, unified and
//! split, wrap on and off, selection, find, previews and plain source documents
//! all live here, because the prefix-sum height index has to know about every one
//! of them.
//!
//! The pieces: `line` is one line's geometry, `layout` the index and the hit tests
//! (all pure, testable without a window), `paint` turns a frame into draw
//! operations, `element` is the GPUI element and its events.
//!
//! # Who owns what
//!
//! The app owns the document, the entity owns the scroll offset, the selection and
//! the caches. Scrolling, hovering and selecting notify this entity alone. The
//! lines are borrowed out of the app when a frame or an event needs them (see
//! [`DocSource`]), so nothing is copied per frame and there is no snapshot to go
//! stale.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, FocusHandle, Global, WeakEntity, Window, div,
};

use crate::chip::Chip;
use crate::hunk_actions::HunkActions;
use crate::theme::{Color, Font};
use crate::typography::CodeTypography;

mod element;
mod layout;
mod line;
mod media;
mod paint;
#[cfg(test)]
mod tests;

pub use media::{Media, MediaImage, MediaSide};
pub use paint::paint_checkbox;

use element::{DiffElement, State};

/// What a prop callback looks like: `Diffui::listen` and `cx.listener` fit.
pub type Callback<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

fn callback<T: 'static>(f: impl Fn(&T, &mut Window, &mut App) + 'static) -> Callback<T> {
    Rc::new(f)
}

/// The document's hunks, per file, read from the entity that owns them when a
/// frame or an event needs them. An entity's `read` borrows from `cx`, so a 1M-line
/// document costs the same as an empty one to hand over.
#[derive(Clone)]
pub struct DocSource(Rc<ReadHunks>);

type ReadHunks = dyn for<'a> Fn(usize, &'a App) -> &'a [DiffHunkView];
type ReadSlice<T> = dyn for<'a> Fn(&'a App) -> &'a [T];

impl DocSource {
    /// `read` must not panic on stale state: an event can land between a state
    /// change and the next render. Return `&[]`.
    pub fn new<E: 'static>(entity: Entity<E>, read: fn(&E, usize) -> &[DiffHunkView]) -> Self {
        Self(Rc::new(move |file, cx| read(entity.read(cx), file)))
    }

    /// No document: a header-only view.
    pub fn empty() -> Self {
        Self(Rc::new(|_, _| &[]))
    }

    fn hunks<'a>(&self, file: usize, cx: &'a App) -> &'a [DiffHunkView] {
        (self.0)(file, cx)
    }
}

/// [`DocSource`] for any other slice the app owns: find matches, blame labels.
pub struct Slice<T>(Rc<ReadSlice<T>>);

impl<T> Clone for Slice<T> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<T: 'static> Slice<T> {
    /// `read` must not panic on stale state; see [`DocSource::new`].
    pub fn new<E: 'static>(entity: Entity<E>, read: fn(&E) -> &[T]) -> Self {
        Self(Rc::new(move |cx| read(entity.read(cx))))
    }

    fn get<'a>(&self, cx: &'a App) -> &'a [T] {
        (self.0)(cx)
    }
}

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
pub struct DiffFileView {
    pub title: String,
    pub status: DiffFileStatus,
    /// Saturated status color (chip glyph, matching the sidebar's file list).
    pub status_color: Color,
    /// Translucent tint behind the status letter, pre-resolved by the caller
    /// (`chip_background(status_color)`) so this widget stays theme-agnostic.
    pub status_fill: Color,
    pub additions: usize,
    pub deletions: usize,
    /// What the header says in place of counts when there are none ("binary").
    pub tag: Option<String>,
    /// One line standing in for the file's rows (a mode-only change, an empty
    /// file, a binary, a file held back for size).
    pub note: Option<FileNote>,
}

/// One muted line in place of a file's rows: an icon, the text in runs (a run
/// flagged `true` is in the code font, for a mode or a size), and, for a file the
/// backend held back, a button that fires [`DiffProps::on_expand_file`].
#[derive(Debug, Clone)]
pub struct FileNote {
    pub icon: &'static str,
    pub runs: Vec<(String, bool)>,
    pub action: Option<String>,
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

/// Find-match data fed into [`DiffProps::with_find`]. The view paints one rectangle
/// per match on the lines it overlaps and scrolls the active match into view when
/// `scroll_token` changes.
#[derive(Clone)]
pub struct FindOverlay {
    /// In document order.
    pub matches: Slice<FindMatch>,
    pub active: Option<usize>,
    /// Bumped by the caller whenever the active match changes. The view keeps the
    /// previous token and scrolls when the two disagree.
    pub scroll_token: u64,
    pub highlight: Color,
    pub active_highlight: Color,
}

/// A blame column drawn left of a plain document's line numbers: one label per
/// line (by 0-based line number), and a click on it reports that line.
pub struct AnnotationGutter {
    pub labels: Slice<AnnotationLabel>,
    /// Column width, in monospace cells.
    pub width_chars: usize,
    pub on_click: Callback<usize>,
    pub style: AnnotationStyle,
}

/// Line-select mode: changed lines take clicks and drags, and the keyboard has a
/// line cursor.
pub struct LineMode {
    pub cursor: Option<LineAt>,
    /// Bumped when the cursor moves by keyboard; a change scrolls it into view.
    pub reveal: u64,
    pub on_press: Callback<LinePress>,
    /// A drag from the pressed line has reached this one.
    pub on_paint: Callback<LineAt>,
}

/// The change-selection checkboxes drawn in file and hunk headers: the marks
/// (indexed like the view's files) and where clicks go.
pub struct ChangeMarks {
    pub files: Vec<FileMark>,
    /// Fill of a checked box and a picked line's bar: the control colour the primary
    /// button wears, kept off the diff's reds and greens.
    pub control: Color,
    pub on_toggle_file: Callback<usize>,
    pub on_toggle_hunk: Callback<(usize, usize)>,
    /// Line-select mode, when it is on.
    pub lines: Option<LineMode>,
    /// Anything is picked: every file header shows its box, not only the
    /// hovered one.
    pub picking: bool,
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
    /// A hunk header's `@@`, and the wash of a picked hunk's header.
    pub accent: Color,
}

/// Everything the app decides about a [`DiffView`], set from a render.
pub struct DiffProps {
    files: Vec<DiffFileView>,
    source: DocSource,
    selected_file: usize,
    revision_key: String,
    palette: Palette,
    font: Font,
    typography: CodeTypography,
    multi_click_ms: u64,
    header: Vec<HeaderLine>,
    on_selected_file_changed: Option<Callback<usize>>,
    on_copy: Option<Callback<String>>,
    /// In-diff find highlights. `None` when the find bar is closed.
    find: Option<FindOverlay>,
    /// Reports the scroll offset whenever it changes, so the app can persist it
    /// per-tab.
    on_scroll: Option<Callback<f32>>,
    /// One-shot scroll restore: when `restore_token` differs from the value the view
    /// last saw, the offset jumps to `restore_offset` (clamped), overriding the
    /// reset-to-top a `revision_key` change would otherwise do. Used to re-apply a
    /// tab's saved scroll on activation.
    restore_offset: f32,
    restore_token: u64,
    /// Monotonic version of the diff content's *paint*, bumped by the app on every
    /// document swap and on background-highlight merges. The per-line caches are
    /// keyed by `(file, hunk, line)`, which point at *different* text after a reload,
    /// a working-copy edit, or a tab switch. `revision_key` alone can't catch those:
    /// it's a constant `"working-copy"` for any `@`.
    content_version: u64,
    /// Whether long lines wrap into extra visual lines (the default) or clip at the
    /// pane edge, leaving every row exactly one line tall.
    wrap: bool,
    /// Two-column layout: deletions/context on the left, additions/context on the
    /// right, paired index-wise within each change run and padded where one side is
    /// longer. Ignored for plain documents.
    side_by_side: bool,
    /// Monotonic identity of the document's *layout*, bumped only when the document
    /// is replaced, not when highlight spans merge in. Keys the height index: span
    /// merges repaint rows but never move them, so rebuilding the (potentially
    /// 1M-row) index for each would be pure waste.
    layout_version: u64,
    /// Plain source-document rendering: no file/hunk header strips and a single
    /// line-number gutter column.
    plain: bool,
    /// Per-file "browse source" affordance drawn in each file header (diff mode
    /// only). Receives the file index on click.
    on_browse_file: Option<Callback<usize>>,
    /// Opens the selected revision's description editor.
    on_edit_description: Option<Callback<()>>,
    /// Fires for the links and buttons of [`HeaderLine::Rich`] fields.
    on_header_action: Option<Callback<HeaderAction>>,
    /// Change-selection checkboxes in file/hunk headers.
    marks: Option<ChangeMarks>,
    /// Blame column of a plain document.
    annotations: Option<AnnotationGutter>,
    /// Per file: whether the backend collapsed it for size. Its placeholder hunk
    /// header is then a button firing `on_expand_file`.
    collapsed_files: Vec<bool>,
    on_expand_file: Option<Callback<usize>>,
    /// Per file: the image preview drawn in place of its rows.
    media: Vec<Option<Media>>,
    /// Identity of the previews' layout, part of the height index's key.
    media_key: Option<u64>,
    /// Buttons on hunk header strips.
    hunk_actions: Option<HunkActions>,
    /// Per file: its body is folded away under its header.
    folded: Vec<bool>,
    /// The UI's font, for the prose in notes.
    ui_font: Font,
    on_toggle_fold: Option<Callback<usize>>,
    /// The "more" button of a file header: the file and the button's rect.
    on_file_menu: Option<Callback<(usize, crate::geometry::Rectangle)>>,
    /// The gutter's digit count, worked out once per set of props.
    digits: Cell<Option<usize>>,
}

impl DiffProps {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        files: Vec<DiffFileView>,
        source: DocSource,
        selected_file: usize,
        revision_key: impl Into<String>,
        palette: Palette,
        font: Font,
        typography: CodeTypography,
        multi_click_ms: u64,
    ) -> Self {
        Self {
            files,
            source,
            selected_file,
            revision_key: revision_key.into(),
            palette,
            font,
            typography,
            multi_click_ms,
            header: Vec::new(),
            on_selected_file_changed: None,
            on_copy: None,
            find: None,
            on_scroll: None,
            restore_offset: 0.0,
            restore_token: 0,
            content_version: 0,
            wrap: true,
            side_by_side: false,
            layout_version: 0,
            plain: false,
            on_browse_file: None,
            on_edit_description: None,
            on_header_action: None,
            marks: None,
            annotations: None,
            collapsed_files: Vec::new(),
            on_expand_file: None,
            media: Vec::new(),
            media_key: None,
            hunk_actions: None,
            folded: Vec::new(),
            ui_font: font,
            on_toggle_fold: None,
            on_file_menu: None,
            digits: Cell::new(None),
        }
    }

    /// Scrolling across a file boundary reports the new file here.
    pub fn on_selected_file_changed(
        mut self,
        f: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_selected_file_changed = Some(callback(f));
        self
    }

    /// Image previews, by file index: a file with one shows it in place of its rows.
    /// `key` names the layout (which files preview), so the height index rebuilds
    /// when it changes.
    pub fn media(mut self, media: Vec<Option<Media>>, key: u64) -> Self {
        self.media = media;
        self.media_key = Some(key);
        self
    }

    fn media_of(&self, file: usize) -> Option<&Media> {
        self.media.get(file)?.as_ref()
    }

    /// Render as a plain source document: no file/hunk header strips, a single
    /// line-number column, and never side-by-side.
    pub fn plain(mut self, plain: bool) -> Self {
        self.plain = plain;
        if plain {
            // A source document has one side; a split layout would just mirror it.
            self.side_by_side = false;
        }
        self
    }

    /// Draw a blame column left of the line numbers. Plain documents only; widens the
    /// gutter, so every text position shifts with it.
    pub fn annotations(mut self, gutter: AnnotationGutter) -> Self {
        self.annotations = Some(gutter);
        self
    }

    /// Fire for the links and buttons of the header's rich fields.
    pub fn on_header_action(
        mut self,
        f: impl Fn(&HeaderAction, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_header_action = Some(callback(f));
        self
    }

    /// Draw a "browse source" icon button in every file header, firing with the file
    /// index when clicked. Ignored for plain documents.
    pub fn on_browse_file(mut self, f: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_browse_file = Some(callback(f));
        self
    }

    /// Draw change-selection checkboxes in file headers (and in the hunk headers of
    /// files picked by hunk). Ignored for plain documents.
    pub fn change_marks(mut self, marks: ChangeMarks) -> Self {
        self.marks = Some(marks);
        self
    }

    /// Make the placeholder hunk header of every file flagged in `collapsed` a button
    /// that fires `f` with the file index.
    pub fn on_expand_file(
        mut self,
        collapsed: Vec<bool>,
        f: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.collapsed_files = collapsed;
        self.on_expand_file = Some(callback(f));
        self
    }

    /// Draw buttons at the right end of some hunks' header strips (a conflict
    /// region's take-a-side controls), and a strip of controls under some files'
    /// headers (a conflicted file's). Ignored for plain documents.
    pub fn hunk_actions(mut self, actions: HunkActions) -> Self {
        self.hunk_actions = Some(actions);
        self
    }

    /// Set the prose in notes in `font` (the code font by default).
    pub fn ui_font(mut self, font: Font) -> Self {
        self.ui_font = font;
        self
    }

    /// Fold the files flagged in `folded` to their headers; a press on a header's
    /// chevron fires `f` with the file index.
    pub fn folds(
        mut self,
        folded: Vec<bool>,
        f: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.folded = folded;
        self.on_toggle_fold = Some(callback(f));
        self
    }

    /// Draw a "more" button in every file header, firing with the file index and
    /// the button's rect.
    pub fn on_file_menu(
        mut self,
        f: impl Fn(&(usize, crate::geometry::Rectangle), &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_file_menu = Some(callback(f));
        self
    }

    pub fn with_header(mut self, header: Vec<HeaderLine>) -> Self {
        self.header = header;
        self
    }

    pub fn on_edit_description(mut self, f: impl Fn(&(), &mut Window, &mut App) + 'static) -> Self {
        self.on_edit_description = Some(callback(f));
        self
    }

    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    pub fn side_by_side(mut self, side_by_side: bool) -> Self {
        self.side_by_side = side_by_side && !self.plain;
        self
    }

    /// Report scroll-offset changes so the caller can persist the position. It fires
    /// on every scroll tick, so it should store the offset and not notify.
    pub fn on_scroll(mut self, f: impl Fn(&f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_scroll = Some(callback(f));
        self
    }

    /// Jump the scroll offset to `offset` the next time `token` changes. Wins over the
    /// `revision_key`-change reset in the same render: a tab restore wants the exact
    /// saved offset, not the top.
    pub fn restore_scroll(mut self, offset: f32, token: u64) -> Self {
        self.restore_offset = offset;
        self.restore_token = token;
        self
    }

    pub fn content_version(mut self, version: u64) -> Self {
        self.content_version = version;
        self
    }

    pub fn layout_version(mut self, version: u64) -> Self {
        self.layout_version = version;
        self
    }

    pub fn on_copy(mut self, f: impl Fn(&String, &mut Window, &mut App) + 'static) -> Self {
        self.on_copy = Some(callback(f));
        self
    }

    pub fn with_find(mut self, find: FindOverlay) -> Self {
        self.find = Some(find);
        self
    }
}

/// Frames planned, across every diff view: lets a benchmark tell real frames from
/// cached ones.
pub static PAINTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The retained half of the diff: its props, and the scroll offset, selection and
/// caches that outlive a frame.
pub struct DiffView {
    props: Option<DiffProps>,
    state: State,
    focus: FocusHandle,
}

/// The diff views alive right now, so the copy shortcut can reach a selection
/// from wherever focus has gone since it was made.
#[derive(Default)]
struct LiveViews(Vec<WeakEntity<DiffView>>);

impl Global for LiveViews {}

/// Copies the selection of whichever diff view holds one, wherever focus is: GPUI
/// runs a key listener only on the focused element's path, and a selection outlives
/// the diff's focus (the palette, the find field). Says whether anything was copied.
pub fn copy_selection(window: &mut Window, cx: &mut App) -> bool {
    let live = cx.default_global::<LiveViews>();
    live.0.retain(|view| view.upgrade().is_some());
    let views = live.0.clone();
    let selected = views
        .iter()
        .filter_map(WeakEntity::upgrade)
        .find_map(|view| view.read(cx).copy(cx));
    let Some((text, callback)) = selected else {
        return false;
    };
    callback(&text, window, cx);
    true
}

impl DiffView {
    fn new(cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        cx.default_global::<LiveViews>().0.push(weak);
        Self {
            props: None,
            state: State::default(),
            focus: cx.focus_handle(),
        }
    }

    /// Sets what the view shows.
    pub fn set_props(&mut self, props: DiffProps) {
        self.props = Some(props);
    }

    /// Moves the scroll offset, as a scroll tick does. For the benchmark and tests:
    /// the app scrolls with [`DiffProps::restore_scroll`].
    pub fn scroll_to(&mut self, offset: f32, cx: &mut Context<Self>) {
        self.state.vertical_offset = offset.max(0.0);
        cx.notify();
    }

    /// What the view has done so far, for the benchmark.
    pub fn frame_stats(&self) -> FrameStats {
        FrameStats {
            frames: self.state.frames,
            total_height: self.state.height_index.borrow().total_height,
            last_index_build: self.state.last_index_build,
            offset: self.state.vertical_offset,
        }
    }
}

/// See [`DiffView::frame_stats`].
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    /// Frames planned.
    pub frames: u64,
    /// The document's height in the current layout.
    pub total_height: f32,
    /// How long the last height-index rebuild took.
    pub last_index_build: std::time::Duration,
    pub offset: f32,
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(ElementId::NamedInteger(
                "diff-view".into(),
                cx.entity_id().as_u64(),
            ))
            .relative()
            .size_full()
            .child(DiffElement::new(cx.entity()))
            .vertical_scrollbar(&self.state.bar)
    }
}

/// The diff view named `id`, showing `props`. It fills its parent. The state behind
/// `id` (scroll, selection, caches) lives while the element is on screen.
pub fn view(id: &'static str, props: DiffProps, window: &mut Window, cx: &mut App) -> AnyElement {
    entity(id, props, window, cx).into_any_element()
}

/// [`view`], handing back the entity.
pub fn entity(
    id: &'static str,
    props: DiffProps,
    window: &mut Window,
    cx: &mut App,
) -> Entity<DiffView> {
    let state = window.use_keyed_state(ElementId::Name(id.into()), cx, |_, cx| DiffView::new(cx));
    state.update(cx, |view, _| view.set_props(props));
    state
}
