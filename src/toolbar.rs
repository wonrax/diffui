//! The actions toolbar: a full-width band below the tab strip with three
//! actions — Refresh, Fetch (a split button), and Undo (jj only) — plus the
//! activity indicator pinned to its right edge and a thin progress line along
//! its bottom edge. The toolbar dropdowns (fetch branches / revset presets)
//! render as iced overlays anchored near their trigger.

use iced::{
    Background, Border, Color, Element, Length, Padding, Point, Rectangle, alignment,
    font::Weight,
    mouse,
    widget::{Space, button, canvas, column, container, mouse_area, row, stack, text},
};

use crate::activity;
use crate::repository::Vcs;
use crate::theme::{ThemeSpec, chip_background, emphasis_font};
use crate::{Diffui, FetchTarget, HoverTarget, Message, ToolbarMenu};

/// A down-pointing caret (▾) drawn as a filled triangle rather than a text
/// glyph. The `U+25BE` glyph sits off-center within the line box in many UI
/// fonts (notably the macOS system font), which left the fetch / revset carets
/// looking vertically misaligned no matter how the text box was centered. A
/// geometric triangle centers exactly on its bounds, so both carets line up
/// regardless of the configured font. Reused by the revset caret in `sidebar`.
struct Caret {
    color: Color,
}

impl canvas::Program<Message> for Caret {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let half_w = 4.0; // 8px wide
        let half_h = 2.0; // 4px tall — flat, matching ▾'s proportions
        let cx = bounds.width / 2.0;
        let cy = bounds.height / 2.0;
        let triangle = canvas::Path::new(|b| {
            b.move_to(Point::new(cx - half_w, cy - half_h));
            b.line_to(Point::new(cx + half_w, cy - half_h));
            b.line_to(Point::new(cx, cy + half_h));
            b.close();
        });
        frame.fill(&triangle, self.color);
        vec![frame.into_geometry()]
    }
}

/// The shared caret glyph: a fixed-width canvas drawing a centered triangle in
/// `color`. `box_height` should be the neighbour's text **line box** (iced's
/// `Relative(1.3)` line-height ⇒ `1.3 × text_size`, which is font-independent),
/// so that with matching vertical padding the caret's hover fill is exactly as
/// tall as the button / input beside it. `Fill` can't be used here: nothing up
/// the toolbar tree bounds the height, so it would stretch to the whole window.
pub(crate) fn caret_glyph(color: Color, box_height: f32) -> Element<'static, Message> {
    canvas(Caret { color })
        .width(Length::Fixed(9.0))
        .height(Length::Fixed(box_height))
        .into()
}

/// Build the actions toolbar. Returns an empty `Space` when no repo is open
/// (the empty-state view owns the window then).
pub fn build_toolbar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }
    let font = ui.config.ui_font;
    let is_jj = matches!(ui.repository.as_ref().map(|r| r.vcs), Some(Vcs::Jj));

    let caret_hovered = ui.hovered == Some(HoverTarget::FetchCaret);
    // 6px between actions — the same rhythm the tab strip uses between tabs, so
    // both title-bar bands share one consistent item spacing.
    let mut actions = row![
        toolbar_button("\u{21BB}", "Refresh", Message::ToolbarRefresh, theme, font),
        fetch_split_button(theme, font, caret_hovered),
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    // jj-only: git has no operation log to undo.
    if is_jj {
        actions = actions.push(toolbar_button(
            "\u{21A9}",
            "Undo",
            Message::Undo,
            theme,
            font,
        ));
    }

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

/// A bordered toolbar action: glyph + label, transparent fill until hovered.
fn toolbar_button(
    glyph: &str,
    label: &str,
    message: Message,
    theme: ThemeSpec,
    font: iced::Font,
) -> Element<'static, Message> {
    let content = row![
        text(glyph.to_owned())
            .size(13)
            .color(theme.muted_text)
            .font(font),
        text(label.to_owned()).size(12).color(theme.text).font(font),
    ]
    .spacing(5)
    .align_y(alignment::Vertical::Center);
    button(content)
        .padding(Padding::from([4, 9]))
        .on_press(message)
        .style(move |_, status| bordered_button_style(theme, status))
        .into()
}

/// A bordered toolbar action — same 1px outline + radius as the Fetch split
/// button, so Refresh / Undo read as a consistent set with it. Transparent
/// fill until hovered.
fn bordered_button_style(theme: ThemeSpec, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(chip_background(theme.muted_text)))
            }
            _ => None,
        },
        text_color: theme.text,
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 6.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    }
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
            text("\u{2193}").size(13).color(theme.muted_text).font(font),
            text("Fetch").size(12).color(theme.text).font(font),
        ]
        .spacing(5)
        .align_y(alignment::Vertical::Center),
    )
    .padding(Padding::from([4, 9]))
    .on_press(Message::Fetch(FetchTarget::AllRemotes))
    .style(move |_, status| ghost_button_style(theme, status));

    // A `mouse_area` (not a `button`) so the menu opens on mouse-*down* while
    // held — that's what lets the native NSMenu track a press-drag-release
    // selection (iced `button` only fires on release). Hover + cursor are set
    // manually since mouse_area has neither.
    let caret = mouse_area(
        // Box height = the main half's size-13 text line box, and the same
        // vertical padding (4) — so the caret's hover fill is exactly as tall as
        // the "Fetch" half rather than hugging the small glyph.
        container(caret_glyph(theme.muted_text, 13.0 * 1.3))
            .padding(Padding::from([4, 7]))
            .align_y(alignment::Vertical::Center)
            .style(move |_| caret_hover_style(theme, caret_hovered)),
    )
    .on_press(Message::OpenToolbarMenu(ToolbarMenu::FetchBranches))
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

    container(
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

fn ghost_button_style(theme: ThemeSpec, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(chip_background(theme.muted_text)))
            }
            _ => None,
        },
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 6.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    }
}

/// Overlay for whichever toolbar dropdown is open (fetch branches / revset
/// presets). Empty `Space` when none is open. Anchored near its trigger.
pub fn build_menu_overlay(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(menu) = ui.toolbar_menu else {
        return Space::new().into();
    };

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::CloseToolbarMenu);

    let (items, pad_top, pad_left) = match menu {
        ToolbarMenu::FetchBranches => (fetch_menu_items(ui, theme), 78.0, 90.0),
        ToolbarMenu::RevsetPresets => (revset_preset_items(ui, theme), 118.0, 12.0),
    };

    let card = mouse_area(
        container(column(items).spacing(1))
            .width(Length::Fixed(240.0))
            .padding(Padding::from([6, 6]))
            .style(move |_| menu_card_style(theme)),
    )
    .on_press(Message::ActivityNoOp);

    let anchored = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Left)
        .padding(Padding {
            top: pad_top,
            right: 0.0,
            bottom: 0.0,
            left: pad_left,
        });

    stack![scrim, anchored].into()
}

/// Fetch dropdown: "Fetch all remotes" + one row per known remote branch
/// (`name@remote`, from the already-loaded bookmark table). git repos have no
/// bookmark table, so they show only the "all remotes" row.
fn fetch_menu_items(ui: &Diffui, theme: ThemeSpec) -> Vec<Element<'_, Message>> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let mut items: Vec<Element<'_, Message>> = vec![menu_item(
        "Fetch all remotes",
        Message::Fetch(FetchTarget::AllRemotes),
        font,
        theme,
        true,
    )];

    // Ordered the same as the native menu (see `remote_branches_by_proximity`).
    let branches = ui.remote_branches_by_proximity();
    if !branches.is_empty() {
        items.push(menu_separator(theme));
        for (branch, remote) in branches {
            let label = format!("{branch}@{remote}");
            items.push(menu_item(
                &label,
                Message::Fetch(FetchTarget::RemoteBranch { remote, branch }),
                mono,
                theme,
                false,
            ));
        }
    }
    items
}

/// Revset presets: jj built-in functions, or git rev-range shortcuts.
fn revset_preset_items(ui: &Diffui, theme: ThemeSpec) -> Vec<Element<'_, Message>> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    ui.revset_menu_entries()
        .into_iter()
        .map(|(label, expr)| {
            let row = row![
                text(label).size(12).color(theme.text).font(font),
                Space::new().width(Length::Fill),
                text(expr.clone())
                    .size(11)
                    .color(theme.subtle_text)
                    .font(mono),
            ]
            .spacing(8)
            .align_y(alignment::Vertical::Center);
            button(row)
                .width(Length::Fill)
                .padding(Padding::from([5, 8]))
                .on_press(Message::RevsetPreset(expr))
                .style(move |_, status| ghost_button_style(theme, status))
                .into()
        })
        .collect()
}

fn menu_item<'a>(
    label: &str,
    message: Message,
    font: iced::Font,
    theme: ThemeSpec,
    emphasized: bool,
) -> Element<'a, Message> {
    let label_font = if emphasized {
        emphasis_font(font, Weight::Medium)
    } else {
        font
    };
    button(
        text(label.to_owned())
            .size(12)
            .color(theme.text)
            .font(label_font),
    )
    .width(Length::Fill)
    .padding(Padding::from([5, 8]))
    .on_press(message)
    .style(move |_, status| ghost_button_style(theme, status))
    .into()
}

fn menu_separator<'a>(theme: ThemeSpec) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .padding(Padding::from([0, 4]))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        })
        .into()
}

fn menu_card_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 10.0.into(),
        },
        shadow: iced::Shadow {
            color: Color {
                a: 0.30,
                ..Color::BLACK
            },
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}
