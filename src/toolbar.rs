//! The actions toolbar: a full-width band below the tab strip. On the left,
//! the view switcher plus the view's actions — Refresh, Fetch (a split
//! button), and Undo (jj only) in the diff view; the browsed-revision label
//! in the source view. On the right, the activity indicator then the display
//! toggles (wrap / split), with a thin progress line along the bar's bottom
//! edge. The toolbar dropdowns (fetch branches / revset presets) render as
//! iced overlays anchored near their trigger.

use crate::Cx;
use crate::Diffui;
use crate::activity;
use crate::theme::{self, ThemeSpec};
use crate::theme::{Color, Font};
use crate::ui;
use gpui_kit::{AnyElement, Window};

/// Every control in the bar is one [`ui::Size::Compact`] step tall.
const CONTROL: ui::Size = ui::Size::Compact;

/// The bar's own height, without the progress line and the rule under it.
pub const HEIGHT: f32 = theme::control::COMPACT + 2.0 * theme::space::SM;

/// Where the window chrome (tab strip, toolbar, progress line, rule) ends and
/// the panels begin — what floating surfaces anchored under the toolbar hang
/// from.
pub fn chrome_bottom() -> f32 {
    crate::tab_bar::height() + HEIGHT + activity::PROGRESS_LINE_HEIGHT + 1.0
}

/// Where every modal surface (command palette, open-repo and confirm
/// dialogs) hangs from: a fixed distance under the window chrome rather than
/// centered, so a card doesn't jump as its content grows and stacked
/// takeovers line up with one another.
pub fn modal_top() -> f32 {
    chrome_bottom() + theme::space::XXL * 2.0
}

/// Size of the dropdown carets (fetch split button, revset presets). A touch
/// smaller than the action icons so the caret reads as a subordinate affordance.
const CARET_ICON_SIZE: f32 = 12.0;

/// Width of the fetch split button's caret half.
const CARET_WIDTH: f32 = 24.0;

/// Char budget for the browsed revision's description line in the toolbar.
const DESCRIPTION_MAX_CHARS: usize = 72;

/// One tab of a [`segmented`] control. No `on_press` disables it.
pub(crate) struct Segment {
    pub(crate) icon: Option<&'static str>,
    pub(crate) label: &'static str,
    pub(crate) active: bool,
    pub(crate) on_press: Option<crate::Action>,
}

/// Char-budget ellipsis for the toolbar's description line — the toolbar has
/// no text-truncation layout, so an explicit cut keeps a long summary from
/// shoving the right-edge toggles around.
fn ellipsize(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Stub: the toolbar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn caret_glyph(
    color: Color,
    box_height: f32,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("toolbar::caret_glyph")
}

/// Stub: the toolbar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_toolbar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::bar("toolbar::build_toolbar", HEIGHT)
}

/// Stub: the toolbar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn segmented(
    segments: Vec<Segment>,
    font: Font,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("toolbar::segmented")
}
