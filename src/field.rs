//! The app-wide single-line text field: a fixed-height bordered well with an
//! optional in-field dropdown caret. Shared by the diff sidebar's revset
//! input, the source sidebar's file search, and the find bar so every text
//! input has the same chrome.

use iced::{
    Background, Border, Color, Element, Font, Length, Padding, alignment, mouse,
    widget::{Space, container, mouse_area, row, text_input},
};

use crate::theme::{self, ThemeSpec};
use crate::{HoverTarget, Message, ToolbarMenu};

/// Fixed height of the field well; the caret square derives from it.
pub const FIELD_HEIGHT: f32 = 28.0;
/// Gap between the caret square and the field's top/right/bottom edges —
/// equal on all three sides so the caret reads as a centered inset button.
const CARET_MARGIN: f32 = 3.0;

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
    let mut bar = row![input].align_y(alignment::Vertical::Center);
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
        .on_enter(Message::SetHover(Some(caret.target)))
        .on_exit(Message::SetHover(None))
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
        Some(menu) => {
            crate::menu::anchor_area(field, move |rect| Message::OpenToolbarMenu(menu, rect)).into()
        }
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
        .padding(Padding::from([6, 8]))
        .into()
}
