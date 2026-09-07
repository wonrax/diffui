//! The app-wide single-line text field: a fixed-height bordered well with an
//! optional in-field dropdown caret. Shared by the diff sidebar's revset
//! input, the source sidebar's file search, and the find bar so every text
//! input has the same chrome.

use iced::{
    Background, Border, Color, Element, Font, Length, Padding, alignment, mouse,
    widget::{Space, button, container, mouse_area, row, text, tooltip},
};

use crate::input::text_input;

use crate::theme::{self, ThemeSpec};
use crate::{Action, HoverTarget, Message, ToolbarMenu, UiEvent};

/// Fixed height of the field well; the caret square derives from it.
pub const FIELD_HEIGHT: f32 = theme::control::COMPACT;
/// Equal breathing room around panel collapse buttons. Panel headers use this
/// on all four sides so the square hover wash sits optically centered between
/// the surrounding content and panel edges.
pub const PANEL_TOGGLE_INSET: f32 = theme::space::XS;
/// Gap between the caret square and the field's top/right/bottom edges —
/// equal on all three sides so the caret reads as a centered inset button.
const CARET_MARGIN: f32 = 3.0;

pub(crate) fn panel_toggle_button<'a>(
    font: Font,
    theme: ThemeSpec,
    glyph: &'static str,
    label: &'static str,
    action: Action,
) -> Element<'a, Message> {
    tooltip(
        panel_toggle_button_bare(theme, glyph, action),
        container(text(label).size(theme::text_size::UI).font(font))
            .padding([4, 8])
            .style(move |_| theme::tooltip_style(theme)),
        tooltip::Position::Right,
    )
    .gap(6)
    .into()
}

/// The same collapse control without a tooltip. Used when hovering the control
/// opens a self-labeling preview, where a second floating label would overlap
/// the preview and add noise.
pub(crate) fn panel_toggle_button_bare<'a>(
    theme: ThemeSpec,
    glyph: &'static str,
    action: Action,
) -> Element<'a, Message> {
    button(crate::icons::icon(glyph, 13.0, theme.muted_text))
        .width(Length::Fixed(theme::control::COMPACT))
        .height(Length::Fixed(theme::control::COMPACT))
        .padding(0)
        .on_press(Message::Action(action))
        .style(move |_, status| theme::ghost_button_style(theme, status))
        .into()
}

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
    pub on_input: fn(String) -> Message,
    /// `None` when Enter is handled elsewhere (e.g. the find bar's keyboard
    /// subscription owns Enter / Shift+Enter).
    pub on_submit: Option<Message>,
    pub caret: Option<FilterCaret>,
}

/// The field chrome (well + hairline border) lives on a wrapping container
/// rather than the `text_input` itself so the optional presets caret can sit
/// *inside* the field.
pub(crate) fn filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField<'_>,
) -> Element<'_, Message> {
    let mut input = text_input(spec.placeholder, spec.value)
        .id(spec.id)
        .padding(Padding::from([6, 9]))
        .size(theme::text_size::UI)
        .font(font)
        .width(Length::Fill)
        .on_input(spec.on_input)
        .style(move |_, _| {
            // Bare input: the wrapping container carries the well + border.
            let mut style = theme::input_style(theme);
            style.background = Background::Color(Color::TRANSPARENT);
            style.border.width = 0.0;
            style
        });
    if let Some(on_submit) = spec.on_submit {
        input = input.on_submit(on_submit);
    }

    let menu = spec.caret.as_ref().map(|caret| caret.menu);
    let mut bar = row![].align_y(alignment::Vertical::Center);
    if let Some(icon) = spec.leading_icon {
        bar = bar.push(
            container(crate::icons::icon(icon, 13.0, theme.subtle_text)).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 9.0,
            }),
        );
    }
    bar = bar.push(input);
    if let Some(caret) = spec.caret {
        // `mouse_area` (not `button`) so the presets menu opens on
        // mouse-*down* while held — required for the native NSMenu's
        // press-drag-release select. Hover is tracked manually (mouse_area
        // has no built-in hover style), and the press falls through to the
        // wrapping `AnchorArea` (the `text_input` captures its own).
        // Square: fills the field height minus the margin, width matched to
        // height so the hover wash is a perfect square.
        let caret_side = FIELD_HEIGHT - CARET_MARGIN * 2.0;
        let caret_el = mouse_area(
            container(crate::toolbar::caret_glyph(theme.muted_text, caret_side))
                .width(Length::Fixed(caret_side))
                .center_x(Length::Fixed(caret_side))
                .style(move |_| {
                    crate::toolbar::caret_hover_style(theme, caret.hovered, theme::radius::CONTROL)
                }),
        )
        .on_enter(Message::Ui(UiEvent::SetHover(Some(caret.target))))
        .on_exit(Message::Ui(UiEvent::SetHover(None)))
        .interaction(mouse::Interaction::Pointer);
        bar = bar
            .push(caret_el)
            .push(Space::new().width(Length::Fixed(CARET_MARGIN)));
    }

    let field = container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(FIELD_HEIGHT))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background)),
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: theme::radius::PUSH.into(),
            },
            ..container::Style::default()
        });

    match menu {
        // The AnchorArea wraps the whole field so the presets menu anchors
        // edge-to-edge below it.
        Some(menu) => crate::menu::anchor_area(field, move |rect| {
            Message::Ui(UiEvent::OpenToolbarMenu(menu, rect))
        })
        .into(),
        None => field.into(),
    }
}

/// A [`filter_field`] wrapped in the sidebar top-bar inset, shared by both
/// sidebars so their top bars are identical. No rule under the field — its
/// bordered well already separates it from the list below.
pub(crate) fn sidebar_filter_field(
    theme: ThemeSpec,
    font: Font,
    spec: FilterField<'_>,
) -> Element<'_, Message> {
    container(filter_field(theme, font, spec))
        .width(Length::Fill)
        .padding(Padding::from([theme::space::XXS, theme::space::MD]))
        .into()
}
