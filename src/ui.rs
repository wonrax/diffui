//! The shared control kit: every chrome button, icon button and tooltip is
//! built here, so heights, insets, icon sizes and label sizes come from one
//! table instead of being re-derived (and drifting) per call site.
//!
//! The styles themselves stay in [`crate::theme`]; this module owns the
//! geometry. A control's height is always one of the fixed [`Size`] steps —
//! never a text line box plus padding, whose fractional height made
//! neighbouring controls disagree by a pixel and snap unevenly.

use iced::{
    Color, Element, Font, Length, Padding, alignment,
    font::Weight,
    widget::{
        self, Button, Container, Row, Scrollable, Space, container, mouse_area, opaque, row,
        scrollable::{Direction, Scrollbar},
        stack, text,
    },
};

use crate::icons;
use crate::prose::Prose;
use crate::theme::{self, ThemeSpec, text_size};

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
    fn pad_x(self) -> f32 {
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

fn style(
    variant: Variant,
    theme: ThemeSpec,
    status: widget::button::Status,
) -> widget::button::Style {
    match variant {
        Variant::Ghost => theme::ghost_button_style(theme, status),
        Variant::Raised => theme::raised_button_style(theme, status),
        Variant::Primary => theme::primary_button_style(theme, status),
        Variant::Destructive => theme::destructive_button_style(theme),
    }
}

/// A button of the given step around arbitrary `content`, which is centered
/// optically (see [`theme::centered_control_content`]). Text inside should
/// leave its color unset so it follows the variant's label color.
pub fn button<'a, Message: Clone + 'a>(
    content: impl Into<Element<'a, Message>>,
    variant: Variant,
    size: Size,
    theme: ThemeSpec,
) -> Button<'a, Message> {
    Button::new(theme::centered_control_content(content))
        .height(Length::Fixed(size.height()))
        .padding(Padding::from([0.0, size.pad_x()]))
        .style(move |_, status| style(variant, theme, status))
}

/// An optional leading icon and a label, sized for `size`. The building block
/// of [`text_button`], exposed for buttons that dress the label themselves.
pub fn label<'a, Message: 'a>(
    icon: Option<(&'static str, Color)>,
    label: impl text::IntoFragment<'a>,
    size: Size,
    font: Font,
) -> Element<'a, Message> {
    let label = text(label).size(size.text_size()).font(font);
    match icon {
        Some((glyph, color)) => row![icons::icon(glyph, ICON_SIZE, color), label]
            .spacing(ICON_GAP)
            .align_y(alignment::Vertical::Center)
            .into(),
        None => label.into(),
    }
}

/// The common button: an optional icon and a label.
pub fn text_button<'a, Message: Clone + 'a>(
    icon: Option<(&'static str, Color)>,
    text: impl text::IntoFragment<'a>,
    variant: Variant,
    size: Size,
    font: Font,
    theme: ThemeSpec,
) -> Button<'a, Message> {
    button(label(icon, text, size, font), variant, size, theme)
}

/// Gap between buttons sitting side by side, in dialogs, editors and bars.
pub const BUTTON_GAP: f32 = theme::space::SM;

/// A row of buttons ending at the right edge, `leading` (a hint, a status)
/// at the left. Every dialog and editor footer is one of these.
pub fn button_row<'a, Message: 'a>(
    leading: Option<Element<'a, Message>>,
    buttons: impl IntoIterator<Item = Element<'a, Message>>,
) -> Row<'a, Message> {
    let mut row = row![]
        .spacing(BUTTON_GAP)
        .align_y(alignment::Vertical::Center);
    if let Some(leading) = leading {
        row = row.push(leading);
    }
    row.push(Space::new().width(Length::Fill)).extend(buttons)
}

/// The Cancel of a dialog or editor footer: always the raised secondary
/// button, beside the footer's one filled call-to-action.
pub fn cancel_button<'a, Message: Clone + 'a>(
    size: Size,
    font: Font,
    theme: ThemeSpec,
) -> Button<'a, Message> {
    text_button(None, "Cancel", Variant::Raised, size, font, theme)
}

/// A square ghost button holding one glyph.
pub fn icon_button<'a, Message: Clone + 'a>(
    glyph: &'static str,
    color: Color,
    size: Size,
    theme: ThemeSpec,
) -> Button<'a, Message> {
    let side = size.height();
    Button::new(
        container(icons::icon(glyph, ICON_SIZE, color))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fixed(side))
    .height(Length::Fixed(side))
    .padding(0)
    .style(move |_, status| theme::ghost_button_style(theme, status))
}

/// The close (or clear) ✕ at the end of a banner or strip: a small square
/// wash, so it sits inside the banner with even insets instead of filling
/// its height.
pub fn close_button<'a, Message: Clone + 'a>(
    color: Color,
    theme: ThemeSpec,
) -> Button<'a, Message> {
    Button::new(
        container(icons::icon(icons::CLOSE, CLOSE_ICON_SIZE, color))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fixed(CLOSE_SIDE))
    .height(Length::Fixed(CLOSE_SIDE))
    .padding(0)
    .style(move |_, status| theme::ghost_button_style(theme, status))
}

/// Side of [`close_button`]'s square.
pub const CLOSE_SIDE: f32 = 20.0;
/// A banner's inset around its [`close_button`] on the top, right and
/// bottom, so the ✕ sits in an even frame on a compact-control-tall banner.
pub const BANNER_INSET: f32 = (theme::control::COMPACT - CLOSE_SIDE) / 2.0;
const CLOSE_ICON_SIZE: f32 = 12.0;

// ── Modals ──────────────────────────────────────────────────────────────

/// Inset between a dialog card's edge and its content.
pub const DIALOG_PADDING: f32 = theme::space::XL + theme::space::XS;
/// Gap between a dialog's title, body, fields and footer.
pub const DIALOG_SPACING: f32 = theme::space::LG;
/// Width of a dialog that asks one question.
pub const DIALOG_WIDTH: f32 = 460.0;

/// A card over the scrim, hung from [`theme::modal_top`]. A press on the
/// scrim sends `dismiss`; one on the card sends `swallow`, which must do
/// nothing (it only keeps the press from reaching the scrim).
pub fn modal<'a, Message: Clone + 'a>(
    card: impl Into<Element<'a, Message>>,
    dismiss: Message,
    swallow: Message,
) -> Element<'a, Message> {
    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| theme::scrim_style()),
    )
    .on_press(dismiss);
    let centered = container(mouse_area(card).on_press(swallow))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: theme::modal_top(),
            ..Padding::ZERO
        });
    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal.
    opaque(stack![scrim, centered])
}

/// The dialog card: the modal surface with the shared inset.
pub fn dialog_card<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    theme: ThemeSpec,
) -> Container<'a, Message> {
    container(content)
        .width(Length::Fixed(width))
        .padding(DIALOG_PADDING)
        .style(move |_| theme::modal_style(theme))
}

/// A dialog's title line.
pub fn dialog_title<'a, Message: 'a>(
    title: &Prose,
    font: Font,
    mono: Font,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    title
        .view(
            theme::emphasis_font(font, Weight::Medium),
            mono,
            theme.text,
            theme,
        )
        .size(text_size::TITLE)
        .into()
}

/// A dialog's explanatory text.
pub fn dialog_text<'a, Message: 'a>(
    body: &Prose,
    font: Font,
    mono: Font,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    body.view(font, mono, theme.muted_text, theme)
        .size(text_size::UI)
        .width(Length::Fill)
        .into()
}

/// Every iced scroll view in the app: a vertical bar floating over the
/// content's right edge, 2px clear of the view's edges, styled and placed
/// like the hand-drawn bar of the graph and the diff. Like there, the
/// content runs under the bar: row washes may reach the edge, but text keeps
/// [`crate::scrollbar::GUTTER`] clear on the right.
pub fn scrollable<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    theme: ThemeSpec,
) -> Scrollable<'a, Message> {
    widget::scrollable(content)
        .direction(Direction::Vertical(
            Scrollbar::new()
                .width(theme::SCROLLBAR_WIDTH)
                .scroller_width(theme::SCROLLBAR_WIDTH)
                .margin(theme::SCROLLBAR_MARGIN)
                .padding(theme::SCROLLBAR_MARGIN),
        ))
        .style(move |_, status| theme::iced_scrollable_style(theme, status))
}

/// The hover label every chrome control uses.
pub fn tooltip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl text::IntoFragment<'a>,
    position: widget::tooltip::Position,
    font: Font,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    widget::tooltip(
        content,
        container(text(label).size(text_size::UI).font(font).color(theme.text))
            .padding([4, 8])
            .style(move |_| theme::tooltip_style(theme)),
        position,
    )
    .gap(6)
    .into()
}

/// A multi-line editor's default key handling minus ⌘⏎. That chord submits
/// the form the editor sits in, and iced's default binding would also insert
/// a newline at the caret just before the submit reads the text.
pub fn editor_key_binding<Message>(
    key_press: widget::text_editor::KeyPress,
) -> Option<widget::text_editor::Binding<Message>> {
    let submit = key_press.modifiers.command()
        && matches!(
            key_press.key,
            iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
        );
    if submit {
        return None;
    }
    widget::text_editor::Binding::from_key_press(key_press)
}
