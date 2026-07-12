//! The actions toolbar: a full-width band below the tab strip with three
//! actions — Refresh, Fetch (a split button), and Undo (jj only) — plus the
//! activity indicator pinned to its right edge and a thin progress line along
//! its bottom edge. The toolbar dropdowns (fetch branches / revset presets)
//! render as iced overlays anchored near their trigger.

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment, mouse,
    widget::{Space, button, column, container, mouse_area, row, text},
};

use crate::activity;
use crate::icons;
use crate::repository::Vcs;
use crate::theme::{
    ThemeSpec, bordered_button_style, chip_background, ghost_button_style, text_size,
};
use crate::{Diffui, FetchTarget, HoverTarget, Message, ToolbarMenu};

/// Toolbar icon size. Slightly larger than the 12px labels so the Lucide marks
/// (which carry ~2px of internal padding in their 24px grid) read as balanced
/// next to the text rather than visually smaller.
const ICON_SIZE: f32 = 14.0;

/// Size of the dropdown carets (fetch split button, revset presets). A touch
/// smaller than the action icons so the caret reads as a subordinate affordance.
const CARET_ICON_SIZE: f32 = 12.0;

/// The shared dropdown caret: a Lucide chevron centered in a `box_height`-tall
/// box. The caller adds the hover fill via padding around this, so sizing the
/// box to the neighbour's text **line box** (iced's `Relative(1.3)` line-height
/// ⇒ `1.3 × text_size`, which is font-independent) keeps the fill exactly as
/// tall as the button / input beside it. Reused by the revset caret in
/// `sidebar`. `Fill` can't be used for the height: nothing up the toolbar tree
/// bounds it, so it would stretch to the whole window.
pub(crate) fn caret_glyph(color: Color, box_height: f32) -> Element<'static, Message> {
    container(icons::icon(icons::CHEVRON_DOWN, CARET_ICON_SIZE, color))
        .height(Length::Fixed(box_height))
        .center_y(Length::Fixed(box_height))
        .into()
}

/// Build the actions toolbar. Returns an empty `Space` when no repo is open
/// (the empty-state view owns the window then).
pub fn build_toolbar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }
    let font = ui.config.ui_font;
    let is_jj = matches!(ui.session.repository.as_ref().map(|r| r.vcs), Some(Vcs::Jj));

    let caret_hovered = ui.hovered == Some(HoverTarget::FetchCaret);
    // 6px between actions — the same rhythm the tab strip uses between tabs, so
    // both title-bar bands share one consistent item spacing.
    let mut actions = row![
        toolbar_button(
            icons::REFRESH,
            "Refresh",
            Message::ToolbarRefresh,
            theme,
            font
        ),
        fetch_split_button(theme, font, caret_hovered),
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    // jj-only: git has no operation log to undo.
    if is_jj {
        actions = actions.push(toolbar_button(
            icons::UNDO,
            "Undo",
            Message::Undo,
            theme,
            font,
        ));
    }
    actions = actions.push(toolbar_toggle_button(
        icons::WRAP,
        "Wrap",
        ui.diff_wrap,
        Message::ToggleDiffWrap,
        theme,
        font,
    ));
    actions = actions.push(toolbar_toggle_button(
        icons::SPLIT,
        "Split",
        ui.diff_split,
        Message::ToggleDiffSplit,
        theme,
        font,
    ));

    let bar = row![
        actions,
        Space::new().width(Length::Fill),
        activity::activity_indicator(ui, theme),
    ]
    .align_y(alignment::Vertical::Center)
    .spacing(6)
    .padding(Padding::from([5, 8]));

    column![
        container(bar)
            .width(Length::Fill)
            .style(move |_| bar_style(theme)),
        activity::activity_progress_line(ui, theme),
    ]
    .into()
}

fn bar_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        ..container::Style::default()
    }
}

/// A bordered toolbar action: icon + label, transparent fill until hovered.
fn toolbar_button(
    icon: &'static str,
    label: &str,
    message: Message,
    theme: ThemeSpec,
    font: iced::Font,
) -> Element<'static, Message> {
    let content = row![
        icons::icon(icon, ICON_SIZE, theme.muted_text),
        text(label.to_owned())
            .size(text_size::UI)
            .color(theme.text)
            .font(font),
    ]
    .spacing(5)
    .align_y(alignment::Vertical::Center);
    button(content)
        .padding(Padding::from([4, 9]))
        .on_press(message)
        .style(move |_, status| bordered_button_style(theme, status))
        .into()
}

/// A toolbar toggle: like [`toolbar_button`], but with a persistent accent
/// tint while active so the on state reads at a glance (the diff-wrap
/// toggle). The glyph carries the accent; the fill/border deepen with it.
fn toolbar_toggle_button(
    icon: &'static str,
    label: &str,
    active: bool,
    message: Message,
    theme: ThemeSpec,
    font: iced::Font,
) -> Element<'static, Message> {
    let icon_color = if active {
        theme.accent
    } else {
        theme.muted_text
    };
    let content = row![
        icons::icon(icon, ICON_SIZE, icon_color),
        text(label.to_owned())
            .size(text_size::UI)
            .color(theme.text)
            .font(font),
    ]
    .spacing(5)
    .align_y(alignment::Vertical::Center);
    button(content)
        .padding(Padding::from([4, 9]))
        .on_press(message)
        .style(move |_, status| {
            let mut style = bordered_button_style(theme, status);
            if active {
                style
                    .background
                    .get_or_insert(Background::Color(chip_background(theme.accent)));
                style.border.color = theme.accent;
            }
            style
        })
        .into()
}

/// The Fetch split button: a main "Fetch" action + a caret that opens the
/// remote-branch menu, in one rounded outline. The two halves highlight
/// **independently** on hover. The outer container carries the border with 1px
/// of padding, so each half's translucent hover fill sits *inside* the border
/// rather than over it (which would darken it).
fn fetch_split_button(
    theme: ThemeSpec,
    font: iced::Font,
    caret_hovered: bool,
) -> Element<'static, Message> {
    // `button` highlights its own half on hover (its `Hovered` status) and shows
    // the pointer cursor automatically.
    let main = button(
        row![
            icons::icon(icons::FETCH, ICON_SIZE, theme.muted_text),
            text("Fetch")
                .size(text_size::UI)
                .color(theme.text)
                .font(font),
        ]
        .spacing(5)
        .align_y(alignment::Vertical::Center),
    )
    .padding(Padding::from([4, 9]))
    .on_press(Message::Fetch(FetchTarget::AllRemotes))
    .style(move |_, status| ghost_button_style(theme, status));

    // A `mouse_area` (not a `button`) with no press handler: the press falls
    // through to the wrapping `AnchorArea` (the main "Fetch" `button` captures
    // its own, so only a press on this caret half opens the menu), which reports
    // the whole split button's rect so the dropdown anchors edge-to-edge below
    // it. Hover + cursor are set manually since mouse_area has neither.
    let caret = mouse_area(
        // Box height = the main half's size-13 text line box, and the same
        // vertical padding (4) — so the caret's hover fill is exactly as tall as
        // the "Fetch" half rather than hugging the small glyph.
        container(caret_glyph(theme.muted_text, 13.0 * 1.3))
            .padding(Padding::from([4, 7]))
            .align_y(alignment::Vertical::Center)
            .style(move |_| caret_hover_style(theme, caret_hovered)),
    )
    .on_enter(Message::SetHover(Some(HoverTarget::FetchCaret)))
    .on_exit(Message::SetHover(None))
    .interaction(mouse::Interaction::Pointer);

    let divider = container(Space::new())
        .width(Length::Fixed(1.0))
        .height(Length::Fixed(16.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        });

    let split = container(
        row![main, divider, caret]
            .spacing(0)
            .align_y(alignment::Vertical::Center),
    )
    // Horizontal-only 1px inset keeps the halves' hover fills off the rounded
    // left/right corners. Vertical padding is 0 so the split button is exactly
    // as tall as the plain Refresh / Undo buttons (whose fills already meet
    // their top/bottom borders the same way) — the extra vertical ring is what
    // made Fetch read as taller than the rest.
    .padding(Padding::from([0, 1]))
    .style(move |_| container::Style {
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    });

    crate::menu::anchor_area(split, |rect| {
        Message::OpenToolbarMenu(ToolbarMenu::FetchBranches, rect)
    })
    .into()
}

/// Hover background for a `mouse_area`-based caret (fetch / revset). Shared so
/// both carets highlight identically. `hovered` is tracked in app state since
/// `mouse_area` has no built-in hover style.
pub(crate) fn caret_hover_style(theme: ThemeSpec, hovered: bool) -> container::Style {
    container::Style {
        background: hovered.then(|| Background::Color(chip_background(theme.muted_text))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    }
}
