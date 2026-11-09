//! The app-wide single-line text field: a fixed-height bordered well with an
//! optional in-field dropdown caret. Shared by the diff sidebar's revset
//! input, the source sidebar's file search, and the find bar so every text
//! input has the same chrome.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Bounds, Div, MouseButton, Pixels, Window, canvas, div, px};

use crate::theme::{self, ThemeSpec};
use crate::theme::{Color, Font, Padding};
use crate::{Cx, Diffui, icons, input};
use crate::{HoverTarget, ToolbarMenu};

/// Fixed height of the field well; the caret square derives from it.
pub const FIELD_HEIGHT: f32 = theme::control::COMPACT;
/// Equal breathing room around a panel header's controls, on all four sides.
pub const PANEL_TOGGLE_INSET: f32 = theme::space::XS;

/// Inset of a panel header's leading content (a filter field's left edge) —
/// the toolbar's edge inset, so the header's field lines up under the
/// toolbar's first control.
pub const PANEL_HEADER_LEAD: f32 = theme::space::MD;

/// Padding of a panel header: the controls' square inset on top/right/bottom,
/// the lead on the left.
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

/// Gap between the caret square and the field's top/right/bottom edges —
/// equal on all three sides so the caret reads as a centered inset button.
const CARET_MARGIN: f32 = 3.0;
/// A field's leading icon: its inset from the field's left edge, and size.
pub const LEADING_ICON_INSET: f32 = 9.0;
pub const LEADING_ICON_SIZE: f32 = 13.0;
/// The row a medium kit input centres its line in (its `h_8`), whatever box
/// it's given: the line hangs from the box's top. The field gives the input a
/// box of exactly this height and centres that in the well, so the line (and
/// its selection) sits in the middle rather than low.
const INPUT_ROW: f32 = 32.0;
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

/// The field chrome (well + hairline border) lives on a wrapping element rather
/// than the input itself so the optional presets caret can sit *inside* the field.
pub(crate) fn filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let on_input = spec.on_input;
    let on_input = Diffui::listen(cx, move |this, text: &String, _, cx| {
        on_input(this, text.clone(), cx);
    });
    let mut field = input::FieldSpec::new(spec.id, spec.value)
        .placeholder(spec.placeholder)
        .on_input(move |text, window, cx| on_input(&text, window, cx));
    if let Some(on_submit) = spec.on_submit {
        let on_submit = Diffui::listen(cx, move |this, _: &(), _, cx| on_submit(this, cx));
        field = field.on_submit(move |window, cx| on_submit(&(), window, cx));
    }
    // No vertical padding: the kit's 8px would push the line off its row.
    let text = input::single_line(field, font, window, cx)
        .appearance(false)
        .px(px(TEXT_INSET))
        .py(px(0.0))
        .h_full();

    // The field's bounds, for the presets menu that anchors edge-to-edge below it.
    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let mut bar = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(FIELD_HEIGHT))
        .bg(theme.panel_background)
        .border_1()
        .border_color(theme.border)
        .rounded(px(theme::radius::PUSH))
        .font(font.gpui());
    if spec.caret.is_some() {
        let bounds = bounds.clone();
        bar = bar.child(
            canvas(move |rect, _, _| bounds.set(rect), |_, _, _, _| {})
                .absolute()
                .size_full(),
        );
    }
    if let Some(icon) = spec.leading_icon {
        bar = bar.child(
            div()
                .flex_none()
                .pl(px(LEADING_ICON_INSET))
                .child(icons::icon(icon, LEADING_ICON_SIZE, theme.subtle_text)),
        );
    }
    bar = bar.child(div().flex_1().min_w_0().h(px(INPUT_ROW)).child(text));
    if let Some(caret) = spec.caret {
        bar = bar.child(presets_caret(caret, bounds, theme, window, cx));
    }
    bar.into_any_element()
}

/// Opens on mouse-*down*, while held: the native NSMenu's press-drag-release
/// select needs that. Square: fills the field height minus the margin.
fn presets_caret(
    caret: FilterCaret,
    field: Rc<Cell<Bounds<Pixels>>>,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let side = FIELD_HEIGHT - CARET_MARGIN * 2.0;
    let FilterCaret {
        hovered,
        target,
        menu,
    } = caret;
    let on_hover = cx.listener(move |this: &mut Diffui, hovering: &bool, _, cx| {
        this.on_set_hover(hovering.then_some(target));
        cx.notify();
    });
    let on_press = Diffui::listen(cx, move |this, _: &gpui_kit::MouseDownEvent, _, cx| {
        cx.stop_propagation();
        this.on_open_toolbar_menu(menu, field.get().into(), cx);
    });
    div()
        .id("field-caret")
        .flex_none()
        .mr(px(CARET_MARGIN))
        .size(px(side))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::radius::CONTROL))
        .cursor_pointer()
        .when(hovered, |this| this.bg(theme::hover_wash(theme)))
        .on_hover(on_hover)
        .on_mouse_down(MouseButton::Left, on_press)
        .child(crate::toolbar::caret_glyph(
            theme.muted_text,
            side,
            window,
            cx,
        ))
        .into_any_element()
}

/// A side panel's field insets, left and right: the row cards' under it, so
/// the field and the rows stand the same width.
pub(crate) fn list_field_insets() -> (f32, f32) {
    let inset = crate::revision_list::ROW_CARD_INSET_X;
    (inset + crate::chrome::window_border(), inset)
}

/// A strip stacked between a side panel's field and its list (a banner, a
/// notice): the field's width, and the gap the field keeps to the list's
/// first card above and below it. The header's bottom padding and the
/// strip's top make one such gap; the strip's bottom and the first card's own
/// inset make the other.
pub(crate) fn panel_strip() -> Div {
    let (lead, trail) = list_field_insets();
    div()
        .w_full()
        .flex_none()
        .pl(px(lead))
        .pr(px(trail))
        .pt(px(crate::revision_list::ROW_CARD_INSET_Y))
        .pb(px(PANEL_HEADER_PADDING.bottom))
}

/// How far the fade under a panel's header reaches into its list.
const SCROLL_FADE: f32 = 28.0;

/// The fade a panel's header casts over its list once the list has scrolled
/// off its top: the pane's colour into nothing, so rows slide under the
/// header instead of being cut by it, and the cut says there's more above.
/// `None` at the top. Hung over the list's box (which is `relative`); it takes
/// no presses.
pub(crate) fn scroll_fade(offset: f64, ground: Color) -> Option<Div> {
    (offset > 0.0).then(|| {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(SCROLL_FADE))
            .bg(gpui_kit::linear_gradient(
                180.0,
                gpui_kit::linear_color_stop(ground, 0.0),
                gpui_kit::linear_color_stop(Color { a: 0.0, ..ground }, 1.0),
            ))
    })
}

/// A [`filter_field`] alone in a panel header (the source browser's file
/// search): the same row geometry as the diff sidebar's field + toggle
/// header, with the lead inset mirrored on the right. No rule under the
/// field; its bordered well already separates it from the list below.
pub(crate) fn sidebar_filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let (lead, trail) = list_field_insets();
    div()
        .w_full()
        .pt(px(PANEL_HEADER_PADDING.top))
        .pb(px(PANEL_HEADER_PADDING.bottom))
        .pl(px(lead))
        .pr(px(trail))
        .child(filter_field(theme, font, spec, window, cx))
        .into_any_element()
}

const DIALOG_FIELD_HEIGHT: f32 = 28.0;

/// A dialog's one-line field: the shared well, at the dialog's control height.
pub(crate) fn dialog_input(
    spec: input::FieldSpec<'_>,
    font: Font,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    // The well is drawn here, around the input's own row: see [`INPUT_ROW`].
    let input = input::single_line(spec, font, window, cx)
        .appearance(false)
        .text_size(px(theme::text_size::BODY))
        .py(px(0.0))
        .h_full();
    div()
        .w_full()
        .h(px(DIALOG_FIELD_HEIGHT))
        .flex()
        .items_center()
        .bg(theme.panel_background)
        .border_1()
        .border_color(theme.border)
        .rounded(px(theme::radius::CONTROL))
        .child(div().w_full().h(px(INPUT_ROW)).child(input))
        .into_any_element()
}
