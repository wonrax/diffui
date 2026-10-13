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
    widget::{self, Button, container, row, text},
};

use crate::icons;
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
    /// Visible at rest: standalone secondary actions.
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
