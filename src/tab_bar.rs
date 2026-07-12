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
    Background, Border, Color, Element, Length, Padding, alignment,
    font::Weight,
    mouse,
    widget::{Space, button, column, container, mouse_area, opaque, row, stack, text, text_input},
};

use crate::chrome;
use crate::icons;
use crate::palette::PaletteMessage;
use crate::repository::Vcs;
use crate::theme::{
    ThemeSpec, chip_background, dialog_button_style, emphasis_font, ghost_button_style,
    modal_style, scrim_style, text_size,
};
use crate::{Diffui, HoverTarget, Message};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = text_size::UI;

/// Build the title-bar tab strip. Returns an empty `Space` when no repos are
/// open (the empty-state view owns the window in that case).
pub fn build_tab_bar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }

    // Tabs sit 1px apart so the strip reads as one tight segmented control; the
    // per-tab hover/active fill is inset inside each tab (see `tab_widget`), so
    // the highlights still keep their own breathing room despite the tight gap.
    let mut tabs_row = row![].spacing(1).align_y(alignment::Vertical::Center);
    for (index, tab) in ui.tabs.iter().enumerate() {
        let active = index == ui.active_tab;
        tabs_row = tabs_row.push(tab_widget(ui, theme, tab, active));
    }
    tabs_row = tabs_row.push(add_button(theme));

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
    // their own clicks, so only the gaps initiate a drag. A double-click on that
    // same empty area runs the system zoom/minimize action, matching a native
    // title bar (the native double-click handling is lost with the title bar).
    if chrome::drag_region() {
        mouse_area(bar)
            .on_press(Message::TitleBarDrag)
            .on_double_click(Message::TitleBarDoubleClick)
            .into()
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
    let badge = match &tab.source {
        crate::TabSource::Repo { vcs, .. } => vcs_badge(*vcs, theme, ui.config.mono_font),
        crate::TabSource::GitHubPr(_) => pr_badge(theme, ui.config.mono_font),
    };

    let name_color = if active { theme.text } else { theme.muted_text };
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
    // Always reserve the dot's slot (a transparent placeholder when clean) so a
    // tab's width doesn't jump as its dirty status changes — e.g. when a revset
    // that excludes `@` makes the working copy drop out of the loaded set.
    label = label.push(if tab_is_dirty(ui, tab, active) {
        dirty_dot(theme)
    } else {
        Space::new()
            .width(Length::Fixed(6.0))
            .height(Length::Fixed(6.0))
            .into()
    });

    let id = tab.id;

    let close = button(icons::icon(icons::CLOSE, 13.0, theme.subtle_text))
        // Symmetric padding around the fixed-size icon box makes the hover
        // target a square that fills the tab's text line height. The old
        // asymmetric padding (0 vertical, 4 horizontal) left it short and wide.
        .padding(Padding::from([2, 2]))
        .on_press(Message::CloseTab(id))
        .style(move |_, status| ghost_button_style(theme, status));

    let inner = row![label, close]
        .spacing(4)
        .align_y(alignment::Vertical::Center);

    // Border and fill live on *separate* elements so the highlight reads as
    // inner: the inner `fill` carries the background, and the outer `frame`
    // carries the active tab's 1px outline plus a 1px inset, so the fill sits
    // strictly inside the outline (and inside the tab edge) instead of painting
    // out to — and under — it. Active: a raised fill (the design's
    // `--tab-active-bg`), lifting a touch on hover. Inactive: empty until
    // hovered, when it takes the same faint chip wash the strip's other controls
    // use.
    let hovered = ui.hovered == Some(HoverTarget::Tab(id));
    let fill_color = match (active, hovered) {
        (true, false) => Some(theme.panel_background),
        (true, true) => Some(theme.selected_file),
        (false, true) => Some(chip_background(theme.muted_text)),
        (false, false) => None,
    };
    let fill = container(inner)
        .padding(Padding::from([3, 7]))
        .style(move |_| container::Style {
            background: fill_color.map(Background::Color),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        });

    // The whole frame is the select target, so a press anywhere up to the tab's
    // edge selects it (the old label-only hit box left the padding dead). The
    // close `×` sits inside and captures its own press first (iced dispatches to
    // children before the parent), so hitting `×` closes rather than selects.
    // Hover is tracked in app state; `on_move` re-asserts it every frame the
    // cursor is over the tab, so the one-frame flicker that `on_enter`/`on_exit`
    // race into when crossing between adjacent tabs is immediately corrected.
    let frame = container(fill)
        .padding(Padding::from([1, 1]))
        .style(move |_| container::Style {
            background: None,
            border: Border {
                width: if active { 1.0 } else { 0.0 },
                color: if active {
                    theme.border
                } else {
                    Color::TRANSPARENT
                },
                radius: 6.0.into(),
            },
            ..container::Style::default()
        });

    mouse_area(frame)
        .on_press(Message::SelectTab(id))
        .on_enter(Message::SetHover(Some(HoverTarget::Tab(id))))
        .on_move(move |_| Message::SetHover(Some(HoverTarget::Tab(id))))
        .on_exit(Message::SetHover(None))
        .interaction(mouse::Interaction::Pointer)
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
    badge_chip(label, color, mono)
}

/// `pr` badge for GitHub pull-request tabs — accent-colored so it reads as
/// "remote" next to the local jj/git chips.
fn pr_badge(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    badge_chip("pr", theme.accent, mono)
}

fn badge_chip(label: &'static str, color: Color, mono: iced::Font) -> Element<'static, Message> {
    container(text(label).size(text_size::BADGE).color(color).font(mono))
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
fn add_button(theme: ThemeSpec) -> Element<'static, Message> {
    button(icons::icon(icons::PLUS, 15.0, theme.muted_text))
        // Symmetric padding → a square button, instead of the wide, short pill
        // the old asymmetric [2, 8] padding made around the square icon box.
        .padding(Padding::from([4, 4]))
        .on_press(Message::OpenRepoDialogOpen)
        .style(move |_, status| ghost_button_style(theme, status))
        .into()
}

/// The right-hand `⌘K` control. The design put a repo-search `⌘P` here; per
/// the spec it instead opens our existing command palette.
fn palette_hint(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    let chip = container(
        text("\u{2318}K")
            .size(text_size::CAPTION)
            .color(theme.muted_text)
            .font(mono),
    )
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
        .on_press(Message::Palette(PaletteMessage::Open))
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

/// Modal confirmation for a guarded mutation (see [`crate::ConfirmDialog`]).
/// Returns an empty `Space` when closed. Lives here because it mirrors the
/// open-repository modal's scrim + card construction exactly.
pub fn build_confirm_dialog(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = &ui.confirm else {
        return Space::new().into();
    };

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| scrim_style()),
    )
    .on_press(Message::ConfirmCancel);

    let cancel = button(
        text("Cancel")
            .size(text_size::BODY)
            .color(theme.text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 15]))
    .on_press(Message::ConfirmCancel)
    .style(move |_, status| dialog_button_style(theme, status));

    // Red fill: the confirm runs a mutation the jj CLI refuses by default.
    let accept = button(
        text(dialog.confirm_label.as_str())
            .size(text_size::BODY)
            .color(theme.background)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 15]))
    .on_press(Message::ConfirmAccept)
    .style(move |_, _| button::Style {
        background: Some(Background::Color(theme.removed_text)),
        text_color: theme.background,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 8.0.into(),
        },
        shadow: Default::default(),
        snap: true,
    });

    let body = column![
        text(dialog.title.as_str())
            .size(text_size::TITLE)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        text(dialog.body.as_str())
            .size(text_size::UI)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
        row![Space::new().width(Length::Fill), cancel, accept]
            .spacing(8)
            .align_y(alignment::Vertical::Center),
    ]
    .spacing(12);

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(460.0))
            .padding(Padding::from([18, 20]))
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::ConfirmNoOp);

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: 140.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal (see `activity_popover` for the mechanics).
    opaque(stack![scrim, centered])
}

/// True when the repo behind `tab` has a non-empty working copy. Best-effort:
/// the working copy's emptiness may still be unresolved (it resolves lazily),
/// in which case we draw no dot rather than guess.
fn tab_is_dirty(ui: &Diffui, tab: &crate::Tab, active: bool) -> bool {
    // PR tabs have no working copy; their synthetic "All changes" row would
    // otherwise read as permanently dirty.
    if matches!(tab.source, crate::TabSource::GitHubPr(_)) {
        return false;
    }
    let commits = if active {
        Some(&ui.session.commits)
    } else {
        tab.stash.as_ref().map(|s| &s.session.commits)
    };
    commits
        .and_then(|c| c.working_copy())
        .and_then(|wc| wc.is_empty())
        == Some(false)
}

/// One quick-pick row in the open dialog's recent list: `owner/name` over the
/// home-contracted path, clicking it reopens that repo. Shared with the welcome
/// screen (`empty_state`) so both recent lists render identically.
pub(crate) fn recent_repo_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    root: &'a str,
) -> Element<'a, Message> {
    let (owner, name) = crate::repo_label(std::path::Path::new(root));
    let mut label = row![].spacing(0).align_y(alignment::Vertical::Center);
    if !owner.is_empty() {
        label = label.push(
            text(format!("{owner}/"))
                .size(text_size::UI)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        );
    }
    label = label.push(
        text(name)
            .size(text_size::UI)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
    );

    let content = column![
        label,
        text(crate::contract_user_path(root))
            .size(text_size::CAPTION)
            .color(theme.subtle_text)
            .font(ui.config.mono_font),
    ]
    .spacing(1);

    button(content)
        .width(Length::Fill)
        .padding(Padding::from([6, 8]))
        .on_press(Message::OpenRecentRepo(root.to_owned()))
        .style(move |_, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(Background::Color(theme.selected_file))
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
            .style(|_| scrim_style()),
    )
    .on_press(Message::OpenRepoDialogClose);

    let input = text_input("~/code/your-repo or a GitHub PR URL", &dialog.path)
        .id(OPEN_REPO_INPUT_ID)
        .padding(Padding::from([8, 10]))
        .size(text_size::BODY)
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
            .size(text_size::TITLE)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        text("Enter the path to a jj or git working copy, or a GitHub pull request (URL or owner/repo#123).")
            .size(text_size::UI)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
        input,
    ]
    .spacing(10);

    if let Some(error) = &dialog.error {
        body = body.push(
            text(error.as_str())
                .size(text_size::UI)
                .color(theme.removed_text)
                .font(ui.config.ui_font),
        );
    }

    // Recent repositories quick-pick: anything in the MRU that isn't already
    // open. One click reopens it (no need to retype the path).
    let open_roots: Vec<&str> = ui
        .tabs
        .iter()
        .filter_map(|tab| tab.root()?.to_str())
        .collect();
    let recents: Vec<&String> = ui
        .recent_repos
        .iter()
        .filter(|root| !open_roots.contains(&root.as_str()))
        .take(6)
        .collect();
    if !recents.is_empty() {
        body = body.push(
            text("Recent")
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        );
        let mut list = column![].spacing(2);
        for root in recents {
            list = list.push(recent_repo_row(ui, theme, root));
        }
        body = body.push(list);
    }

    let cancel = button(
        text("Cancel")
            .size(text_size::BODY)
            .color(theme.text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 15]))
    .on_press(Message::OpenRepoDialogClose)
    .style(move |_, status| dialog_button_style(theme, status));

    let open = button(
        text("Open")
            .size(text_size::BODY)
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
            .style(move |_| modal_style(theme)),
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

    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal (see `activity_popover` for the mechanics).
    opaque(stack![scrim, centered])
}
