//! The shared control kit: every chrome button, icon button and tooltip is
//! built here, so heights, insets, icon sizes and label sizes come from one
//! table instead of being re-derived (and drifting) per call site.
//!
//! The styles themselves stay in [`crate::theme`]; this module owns the
//! geometry. A control's height is always one of the fixed [`Size`] steps —
//! never a text line box plus padding, whose fractional height made
//! neighbouring controls disagree by a pixel and snap unevenly.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::scroll::{Scrollable, ScrollableElement as _};
use gpui_kit::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{
    App, Bounds, Context, Div, ElementId, Entity, MouseButton, Pixels, ScrollHandle, SharedString,
    Stateful, Window, anchored, canvas, div, point, px,
};

use crate::icons;
use crate::theme::{self, Color, Font, ThemeSpec, Weight, text_size};
use crate::typography;

/// Control height step. Everything sharing a row should share a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Toolbars, panel headers, inline editors, find bar.
    Compact,
    /// Dialog footers, empty-state actions.
    Standard,
}

impl Size {
    pub fn height(self) -> f32 {
        match self {
            Size::Compact => theme::control::COMPACT,
            Size::Standard => theme::control::STANDARD,
        }
    }

    /// Horizontal inset of a labelled button.
    pub fn pad_x(self) -> f32 {
        match self {
            Size::Compact => 10.0,
            Size::Standard => 14.0,
        }
    }

    pub fn text_size(self) -> f32 {
        match self {
            Size::Compact => text_size::UI,
            Size::Standard => text_size::BODY,
        }
    }
}

/// Glyph size for an icon that leads a label or fills an icon button. A step
/// over the 12px label because Lucide's 24px grid carries ~2px of internal
/// padding, so at equal size the mark reads smaller than the text beside it.
pub const ICON_SIZE: f32 = 14.0;
/// Gap between a leading icon and its label.
const ICON_GAP: f32 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Invisible at rest: for controls that sit on a framed surface.
    Ghost,
    /// Visible at rest: standalone secondary actions, and every Cancel.
    Raised,
    /// The one call-to-action of a surface.
    Primary,
    /// A confirm that destroys or overrides something.
    Destructive,
}

fn style(variant: Variant, theme: ThemeSpec) -> theme::ButtonStyle {
    match variant {
        Variant::Ghost => theme::ghost_button_style(theme),
        Variant::Raised => theme::raised_button_style(theme),
        Variant::Primary => theme::primary_button_style(theme),
        Variant::Destructive => theme::destructive_button_style(theme),
    }
}

/// kit's button in one of diffui's variants at one of its height steps. Add the
/// content with `.child(...)` ([`label`] for the common case) and the action
/// with `.on_click(...)`.
pub fn button(
    id: impl Into<ElementId>,
    variant: Variant,
    size: Size,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    frame(id, style(variant, theme), theme, cx)
        .h(px(size.height()))
        .px(px(size.pad_x()))
}

fn frame(
    id: impl Into<ElementId>,
    style: theme::ButtonStyle,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    let colors = ButtonCustomVariant::new(cx)
        .color(style.rest.unwrap_or(theme::TRANSPARENT).into())
        .foreground(style.label.into())
        .hover(style.hover.into())
        .active(style.active.into());
    // kit tints a custom variant's fill to 20%, for its own translucent style; ours
    // is the fill itself. Instance styles win over the variant, hover over both.
    let mut button = Button::new(id)
        .custom(colors)
        .bg(style.rest.unwrap_or(theme::TRANSPARENT))
        .rounded(px(theme::radius::BUTTON));
    if let Some(shadow) = style.shadow {
        button = button.shadow(shadow(theme));
    }
    button
}

/// An optional leading icon and a label, sized for `size`. The building block
/// of [`text_button`], exposed for buttons that dress the label themselves.
pub fn label(
    icon: Option<(&'static str, Color)>,
    label: impl Into<SharedString>,
    size: Size,
    font: Font,
) -> Div {
    // Labels sit a weight over body text, as the prototype's buttons do.
    let font = theme::emphasis_font(font, Weight::MEDIUM);
    let text = div()
        .font(font.gpui())
        .text_size(px(size.text_size()))
        .line_height(px(size.text_size() * typography::DEFAULT_LINE_HEIGHT))
        .whitespace_nowrap()
        .child(label.into());
    // Optically centred: the control centres the caps, not the line box.
    let text = typography::centered(text, font, size.text_size());
    let row = div().flex().flex_row().items_center().gap(px(ICON_GAP));
    match icon {
        Some((glyph, color)) => row.child(icons::icon(glyph, ICON_SIZE, color)).child(text),
        None => row.child(text),
    }
}

/// The common button: an optional icon and a label.
#[allow(clippy::too_many_arguments)]
pub fn text_button(
    id: impl Into<ElementId>,
    icon: Option<(&'static str, Color)>,
    text: impl Into<SharedString>,
    variant: Variant,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    button(id, variant, size, theme, cx).child(label(icon, text, size, font))
}

/// [`text_button`] in the primary variant, greyed out and inert unless
/// `enabled`.
pub fn primary_button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    enabled: bool,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    let colors = if enabled {
        theme::primary_button_style(theme)
    } else {
        theme::disabled_button_style(theme)
    };
    frame(id, colors, theme, cx)
        .h(px(size.height()))
        .px(px(size.pad_x()))
        .disabled(!enabled)
        .child(label(None, text, size, font))
}

/// Gap between buttons sitting side by side, in dialogs, editors and bars.
pub const BUTTON_GAP: f32 = theme::space::SM;

/// The Cancel of a dialog or editor footer: always the raised secondary
/// button, beside the footer's one filled call-to-action.
pub fn cancel_button(
    id: impl Into<ElementId>,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    text_button(id, None, "Cancel", Variant::Raised, size, font, theme, cx)
}

/// A square ghost button holding one glyph.
pub fn icon_button(
    id: impl Into<ElementId>,
    glyph: &'static str,
    color: Color,
    size: Size,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    let side = px(size.height());
    frame(id, theme::ghost_button_style(theme), theme, cx)
        .size(side)
        .p(px(0.0))
        .child(icons::icon(glyph, ICON_SIZE, color))
}

/// The close (or clear) ✕ at the end of a banner or strip: a small square
/// wash, so it sits inside the banner with even insets instead of filling
/// its height.
pub fn close_button(id: impl Into<ElementId>, color: Color, theme: ThemeSpec, cx: &App) -> Button {
    frame(id, theme::ghost_button_style(theme), theme, cx)
        .size(px(CLOSE_SIDE))
        .p(px(0.0))
        .child(icons::icon(icons::CLOSE, CLOSE_ICON_SIZE, color))
}

/// Side of [`close_button`]'s square.
pub const CLOSE_SIDE: f32 = 20.0;
/// A banner's inset around its [`close_button`] on the top, right and
/// bottom, so the ✕ sits in an even frame on a compact-control-tall banner.
pub const BANNER_INSET: f32 = (theme::control::COMPACT - CLOSE_SIDE) / 2.0;
const CLOSE_ICON_SIZE: f32 = 12.0;

// ── Modals ──────────────────────────────────────────────────────────────

/// A card over the scrim, hung `top` px below the window's top edge. A press
/// on the scrim runs `dismiss`; one on the card stays on the card. The layer
/// is occluding so wheel events and the cursor don't bleed through to the
/// shell below the modal. Place it as the last child of the root.
pub fn modal(
    id: impl Into<ElementId>,
    card: impl IntoElement,
    top: f32,
    dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(top))
        .bg(theme::SCRIM)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            dismiss(window, cx);
        })
        .child(
            div()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(card),
        )
}

/// Every plain scroll view in the app: `content` scrolling vertically under
/// kit's bar, which [`theme::apply_to_kit`] colours like the hand-drawn bar of
/// the graph and the diff. Like there, the content runs under the bar: row
/// washes may reach the edge, but text keeps [`crate::scrollbar::GUTTER`] clear
/// on the right ([`gutter`]).
pub fn scrollable(
    id: impl Into<ElementId>,
    content: impl IntoElement,
) -> Scrollable<Stateful<Div>> {
    div()
        .id(id)
        .size_full()
        .child(content)
        .overflow_y_scrollbar()
}

/// [`scrollable`] on a handle the caller holds, for a list that scrolls itself
/// to a row. The handle usually lives in [`scroll_handle`].
pub fn scrollable_with(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    content: impl IntoElement,
) -> Stateful<Div> {
    scrollable_inset(id, handle, content, 0.0)
}

/// [`scrollable_with`] whose bar's track stops `bar_inset` short of the top
/// and bottom: for a list filling a rounded card, so the bar keeps clear of
/// the corners. The list scrolls over the whole height all the same.
pub fn scrollable_inset(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    content: impl IntoElement,
    bar_inset: f32,
) -> Stateful<Div> {
    let id = id.into();
    div()
        .id(id.clone())
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .id((id, "area"))
                .size_full()
                .flex()
                .flex_col()
                .track_scroll(handle)
                .overflow_y_scroll()
                .child(div().w_full().flex_none().child(content)),
        )
        // The bar fills the box it's mounted on: one inset from the ends.
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(bar_inset))
                .bottom(px(bar_inset))
                .vertical_scrollbar(handle),
        )
}

/// The scroll handle of list `id`, kept while the list is on screen.
pub fn scroll_handle(
    id: impl Into<ElementId>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ScrollHandle> {
    window.use_keyed_state(id, cx, |_, _| ScrollHandle::default())
}

/// A scroll view's text column: its content keeps the bar's footprint clear.
pub fn gutter() -> Div {
    div().w_full().pr(px(crate::scrollbar::GUTTER))
}

/// Between a control's bottom edge and its tooltip.
const TOOLTIP_GAP: f32 = 6.0;
/// The least a tooltip keeps from the window's edges.
const TOOLTIP_MARGIN: f32 = 6.0;

/// The hover label every chrome control uses: `.tip(label, font, theme)` on an
/// element with an id. GPUI drops a tooltip at the cursor, over what's under
/// it; this one hangs from the control's bottom edge instead, left-aligned
/// with it, and moves up or in to stay in the window.
pub trait Tip: StatefulInteractiveElement + ParentElement + Sized {
    fn tip(self, label: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Self {
        let anchor: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
        let recorded = anchor.clone();
        let label = label.into();
        // An absolute child sits on its parent's box (taffy positions it
        // against its own parent), so this reads the control's bounds.
        self.child(
            canvas(
                move |bounds, _, _| recorded.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .tooltip(move |_, cx| {
            let label = label.clone();
            let anchor = anchor.clone();
            cx.new(|_| TooltipView {
                label,
                font,
                theme,
                anchor,
            })
            .into()
        })
    }
}

impl<T: StatefulInteractiveElement + ParentElement + Sized> Tip for T {}

struct TooltipView {
    label: SharedString,
    font: Font,
    theme: ThemeSpec,
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Render for TooltipView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let card = theme::tooltip_card(div(), self.theme)
            .px(px(8.0))
            .py(px(4.0))
            .font(self.font.gpui())
            .text_size(px(text_size::UI))
            .child(self.label.clone());
        match self.anchor.get() {
            Some(control) => anchored()
                .position(point(control.left(), control.bottom() + px(TOOLTIP_GAP)))
                .snap_to_window_with_margin(px(TOOLTIP_MARGIN))
                .child(card)
                .into_any_element(),
            None => card.into_any_element(),
        }
    }
}
