//! The multi-repo title-bar strip: one tab per open repository plus the
//! controls that manage them.
//!
//! Layout, left → right: a tab for each open repo (VCS badge, `owner/name`
//! with the owner dimmed, an optional uncommitted-changes dot, and a close
//! `×`), a `+` button that opens the path dialog, and — pinned right — a
//! `⌘K` control that opens the command palette (the design's repo-search
//! `⌘P` is folded into our existing palette per the spec).
//!
//! The strip and the path dialog both read straight off `&Diffui`, mirroring
//! `sidebar` / `palette` / `find`, so there is no intermediate view model to
//! keep in sync.

use iced::{
    Background, Border, Color, Element, Length, Padding,
    alignment,
    font::Weight,
    widget::{Space, button, column, container, mouse_area, row, stack, text, text_input},
};

use crate::chrome;
use crate::repository::Vcs;
use crate::theme::{ThemeSpec, chip_background, emphasis_font};
use crate::{Diffui, Message};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = 12.5;

/// Build the title-bar tab strip. Returns an empty `Space` when no repos are
/// open (the empty-state view owns the window in that case).
pub fn build_tab_bar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }

    let mut tabs_row = row![].spacing(4).align_y(alignment::Vertical::Center);
    for (index, tab) in ui.tabs.iter().enumerate() {
        let active = index == ui.active_tab;
        tabs_row = tabs_row.push(tab_widget(ui, theme, tab, active));
    }
    tabs_row = tabs_row.push(add_button(theme, ui.config.ui_font));

    // When the strip stands in for the title bar (macOS) it's pinned to a fixed
    // height and the content is centered — the native traffic lights are
    // repositioned to that same center (see `chrome::position_window_controls`),
    // so the two line up. Below a native title bar it sizes to its content with
    // balanced 6px vertical padding (matching the 6px spacing).
    let titlebar_height = chrome::title_bar_height();
    let v_pad = if titlebar_height.is_some() { 0.0 } else { 6.0 };
    let mut strip = row![]
        .align_y(alignment::Vertical::Center)
        .spacing(6)
        .padding(Padding {
            top: v_pad,
            right: 8.0,
            bottom: v_pad,
            left: 8.0,
        });
    // Reserve space at the leading edge for OS window controls that overlap our
    // content (macOS traffic lights); zero elsewhere.
    let inset = chrome::leading_inset();
    if inset > 0.0 {
        strip = strip.push(Space::new().width(Length::Fixed(inset)));
    }
    let strip = strip
        .push(tabs_row)
        .push(Space::new().width(Length::Fill))
        .push(palette_hint(theme, ui.config.mono_font));

    let mut bar = container(strip)
        .width(Length::Fill)
        .style(move |_| bar_style(theme));
    if let Some(height) = titlebar_height {
        bar = bar
            .height(Length::Fixed(height))
            .align_y(alignment::Vertical::Center);
    }

    // When the native title bar is hidden/replaced (macOS today), the strip's
    // empty area doubles as the window-drag handle: tabs and buttons consume
    // their own clicks, so only the gaps initiate a drag.
    if chrome::drag_region() {
        mouse_area(bar).on_press(Message::TitleBarDrag).into()
    } else {
        bar.into()
    }
}

/// Background for the tab strip / title-bar surface.
fn bar_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            // A single hairline at the bottom separates the strip from the
            // panes; iced borders are uniform, so the divider is drawn by the
            // panel below instead — keep this borderless.
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// One repo tab. The label area selects the tab; the trailing `×` closes it
/// (kept as a separate widget so the two clicks never fight over the event).
fn tab_widget<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    tab: &'a crate::Tab,
    active: bool,
) -> Element<'a, Message> {
    let badge = vcs_badge(tab.vcs, theme, ui.config.mono_font);

    let name_color = if active {
        theme.text
    } else {
        theme.muted_text
    };
    // owner + name as a tight unit (no gap), matching the design's RepoLabel —
    // "code/diffui" reads as one path. The badge and dirty dot get the 6px
    // rhythm from the outer row instead.
    let mut repo_label = row![].spacing(0).align_y(alignment::Vertical::Center);
    if !tab.owner.is_empty() {
        repo_label = repo_label.push(
            text(format!("{}/", tab.owner))
                .size(TAB_TEXT_SIZE)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        );
    }
    repo_label = repo_label.push(
        text(tab.name.as_str())
            .size(TAB_TEXT_SIZE)
            .color(name_color)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
    );

    let mut label = row![badge, repo_label]
        .spacing(6)
        .align_y(alignment::Vertical::Center);
    if tab_is_dirty(ui, tab, active) {
        label = label.push(dirty_dot(theme));
    }

    let id = tab.id;
    let select = mouse_area(label).on_press(Message::SelectTab(id));

    let close = button(
        text("\u{00d7}") // × multiplication sign
            .size(13)
            .color(theme.subtle_text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([0, 4]))
    .on_press(Message::CloseTab(id))
    .style(move |_, status| button::Style {
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
            radius: 4.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    });

    let inner = row![select, close]
        .spacing(4)
        .align_y(alignment::Vertical::Center);

    // The active tab is raised: a lighter fill (the pane color) plus a soft
    // outline, matching the design's `--tab-active-bg`. Inactive tabs sit
    // flush against the strip.
    let (background, border) = if active {
        (
            Some(Background::Color(theme.panel_background)),
            Border {
                width: 1.0,
                color: theme.border,
                radius: 6.0.into(),
            },
        )
    } else {
        (
            None,
            Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 6.0.into(),
            },
        )
    };

    container(inner)
        .padding(Padding::from([4, 8]))
        .style(move |_| container::Style {
            background,
            border,
            ..container::Style::default()
        })
        .into()
}

/// `jj` / `git` badge — a soft colored chip carrying the VCS kind, so a
/// mixed set of repos reads at a glance.
fn vcs_badge(vcs: Vcs, theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    let (label, color) = match vcs {
        // Violet for jj (the trunk/lane hue), warm amber for git — distinct
        // from the diff add/del greens and reds.
        Vcs::Jj => ("jj", theme.lane_base),
        Vcs::Git => ("git", theme.modified_token),
    };
    container(text(label).size(9.5).color(color).font(mono))
        .padding(Padding::from([1, 4]))
        .style(move |_| container::Style {
            background: Some(Background::Color(chip_background(color))),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// Small filled dot signalling the repo has uncommitted changes.
fn dirty_dot(theme: ThemeSpec) -> Element<'static, Message> {
    container(Space::new())
        .width(Length::Fixed(6.0))
        .height(Length::Fixed(6.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.accent)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// `+` button — opens the path dialog to add another repository.
fn add_button(theme: ThemeSpec, font: iced::Font) -> Element<'static, Message> {
    button(text("+").size(15).color(theme.muted_text).font(font))
        .padding(Padding::from([2, 8]))
        .on_press(Message::OpenRepoDialogOpen)
        .style(move |_, status| button::Style {
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
        })
        .into()
}

/// The right-hand `⌘K` control. The design put a repo-search `⌘P` here; per
/// the spec it instead opens our existing command palette.
fn palette_hint(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    let chip = container(text("\u{2318}K").size(10.5).color(theme.muted_text).font(mono))
        .padding(Padding::from([1, 5]))
        .style(move |_| container::Style {
            background: Some(Background::Color(chip_background(theme.muted_text))),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        });

    button(chip)
        .padding(Padding::from([2, 4]))
        .on_press(Message::PaletteOpen)
        .style(move |_, _| button::Style {
            background: None,
            text_color: theme.muted_text,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 6.0.into(),
            },
            shadow: Default::default(),
            snap: true,
        })
        .into()
}

/// True when the repo behind `tab` has a non-empty working copy. Best-effort:
/// the working copy's emptiness may still be unresolved (it resolves lazily),
/// in which case we draw no dot rather than guess.
fn tab_is_dirty(ui: &Diffui, tab: &crate::Tab, active: bool) -> bool {
    let commits = if active {
        Some(&ui.commits)
    } else {
        tab.stash.as_ref().map(|s| &s.commits)
    };
    commits
        .and_then(|c| c.working_copy())
        .and_then(|wc| wc.is_empty())
        == Some(false)
}

/// Modal overlay for opening a repository from a path. Returns an empty
/// `Space` when the dialog is closed. Mirrors the palette's scrim + card
/// construction so click-outside dismisses and the card itself doesn't.
pub fn build_open_repo_dialog(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = &ui.open_repo_dialog else {
        return Space::new().into();
    };

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.40,
                    ..Color::BLACK
                })),
                ..container::Style::default()
            }),
    )
    .on_press(Message::OpenRepoDialogClose);

    let input = text_input("~/code/your-repo", &dialog.path)
        .id(OPEN_REPO_INPUT_ID)
        .padding(Padding::from([8, 10]))
        .size(13)
        .font(ui.config.mono_font)
        .on_input(Message::OpenRepoPathChanged)
        .on_submit(Message::OpenRepoSubmit)
        .style(move |_, _| text_input::Style {
            background: Background::Color(theme.background),
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: 8.0.into(),
            },
            icon: theme.muted_text,
            placeholder: theme.subtle_text,
            value: theme.text,
            selection: Color {
                a: 0.25,
                ..theme.accent
            },
        });

    let mut body = column![
        text("Open repository")
            .size(15)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        text("Enter the path to a jj or git working copy.")
            .size(12)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
        input,
    ]
    .spacing(10);

    if let Some(error) = &dialog.error {
        body = body.push(
            text(error.as_str())
                .size(12)
                .color(theme.removed_text)
                .font(ui.config.ui_font),
        );
    }

    let cancel = button(
        text("Cancel")
            .size(13)
            .color(theme.text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 15]))
    .on_press(Message::OpenRepoDialogClose)
    .style(move |_, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(theme.selected_file))
            }
            _ => Some(Background::Color(theme.panel_background)),
        },
        text_color: theme.text,
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 8.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    });

    let open = button(
        text("Open")
            .size(13)
            .color(theme.background)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 15]))
    .on_press(Message::OpenRepoSubmit)
    .style(move |_, _| button::Style {
        background: Some(Background::Color(theme.accent)),
        text_color: theme.background,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 8.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    });

    body = body.push(
        row![Space::new().width(Length::Fill), cancel, open]
            .spacing(8)
            .align_y(alignment::Vertical::Center),
    );

    // Catch clicks on the card so they don't fall through to the scrim and
    // dismiss the dialog while the user is interacting with it.
    let card = mouse_area(
        container(body)
            .width(Length::Fixed(460.0))
            .padding(Padding::from([18, 20]))
            .style(move |_| container::Style {
                background: Some(Background::Color(theme.panel_background_elevated)),
                border: Border {
                    width: 1.0,
                    color: theme.border,
                    radius: 14.0.into(),
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
            }),
    )
    .on_press(Message::OpenRepoNoOp);

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: 120.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    stack![scrim, centered].into()
}
