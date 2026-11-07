//! The app-wide single-line text field: a fixed-height bordered well with an
//! optional in-field dropdown caret. Shared by the diff sidebar's revset
//! input, the source sidebar's file search, and the find bar so every text
//! input has the same chrome.

use crate::theme::{self, ThemeSpec};
use crate::theme::{Font, Padding};
use crate::{Action, HoverTarget, ToolbarMenu};
use crate::{Cx, Diffui};
use gpui_kit::{AnyElement, Window};

/// Fixed height of the field well; the caret square derives from it.
pub const FIELD_HEIGHT: f32 = theme::control::COMPACT;
/// Equal breathing room around panel collapse buttons. Panel headers use this
/// on all four sides so the square hover wash sits optically centered between
/// the surrounding content and panel edges.
pub const PANEL_TOGGLE_INSET: f32 = theme::space::XS;

/// Inset of a panel header's leading content (a filter field's left edge) —
/// the toolbar's edge inset, so the header's field lines up under the
/// toolbar's first control.
pub const PANEL_HEADER_LEAD: f32 = theme::space::MD;

/// Padding of a panel header whose trailing element is a collapse toggle:
/// the toggle's square inset on top/right/bottom, the lead on the left.
pub const PANEL_HEADER_PADDING: Padding = Padding {
    top: PANEL_TOGGLE_INSET,
    right: PANEL_TOGGLE_INSET,
    bottom: PANEL_TOGGLE_INSET,
    left: PANEL_HEADER_LEAD,
};

/// Height of a panel header row: one control step and its toggle inset
/// above and below. Strips that sit beside panel headers (the source
/// browser's info bar) take the same height so their text shares a line.
pub const PANEL_HEADER_HEIGHT: f32 = theme::control::COMPACT + 2.0 * PANEL_TOGGLE_INSET;

/// Padding of a collapsed panel's rail, which holds only its toggle: the
/// toggle sits exactly where the expanded header's toggle row does.
pub const COLLAPSED_RAIL_PADDING: f32 = PANEL_TOGGLE_INSET;
/// Gap between the caret square and the field's top/right/bottom edges —
/// equal on all three sides so the caret reads as a centered inset button.
const CARET_MARGIN: f32 = 3.0;
/// A field's leading icon: its inset from the field's left edge, and size.
pub const LEADING_ICON_INSET: f32 = 9.0;
pub const LEADING_ICON_SIZE: f32 = 13.0;
/// The text's inset from what precedes it, the field's edge or its icon.
const TEXT_INSET: f32 = 9.0;
/// Where the text of a field with a leading icon starts, from the field's
/// left edge: the column what hangs under the field (its error) lines up on.
pub const LEADING_TEXT_X: f32 = LEADING_ICON_INSET + LEADING_ICON_SIZE + TEXT_INSET;

/// The trailing in-field caret of a [`filter_field`]: opens `menu` on press,
/// with its hover wash driven by app-tracked state (`hovered` / `target`).
pub(crate) struct FilterCaret {
    pub hovered: bool,
    pub target: HoverTarget,
    pub menu: ToolbarMenu,
}

/// What varies between [`filter_field`] instances: the input's identity and
/// wiring, plus the optional in-field presets caret.
pub(crate) struct FilterField<'a> {
    pub id: &'static str,
    pub leading_icon: Option<&'static str>,
    pub placeholder: &'a str,
    pub value: &'a str,
    pub on_input: fn(&mut Diffui, String, &mut Cx),
    /// `None` when Enter is handled elsewhere (e.g. the find bar's keys own
    /// Enter / Shift+Enter).
    pub on_submit: Option<fn(&mut Diffui, &mut Cx)>,
    pub caret: Option<FilterCaret>,
}

/// Stub: the field view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn panel_toggle_button(
    font: Font,
    theme: ThemeSpec,
    glyph: &'static str,
    label: &'static str,
    action: Action,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("field::panel_toggle_button")
}

/// Stub: the field view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn panel_toggle_button_bare(
    theme: ThemeSpec,
    glyph: &'static str,
    action: Action,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("field::panel_toggle_button_bare")
}

/// Stub: the field view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("field::filter_field")
}

/// Stub: the field view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn sidebar_filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("field::sidebar_filter_field")
}
