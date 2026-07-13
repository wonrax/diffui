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
    Background, Border, Color, Element, Event, Length, Padding, Rectangle, Shadow, Size, Theme,
    Vector,
    advanced::{
        Layout, Renderer as _, Shell, Widget, layout, mouse as advanced_mouse, overlay, renderer,
        widget::{Operation, Tree},
    },
    alignment,
    font::Weight,
    mouse,
    widget::{
        Row, Space, button, column, container, mouse_area, opaque, row, stack, text, text_input,
    },
};

use crate::chrome;
use crate::icons;
use crate::palette::PaletteMessage;
use crate::repository::Vcs;
use crate::theme::{
    ThemeSpec, chip_background, destructive_button_style, dialog_button_style, emphasis_font,
    ghost_button_style, input_style, modal_style, primary_button_style, radius, scrim_style,
    text_size, well_fill,
};
use crate::{Diffui, HoverTarget, Message};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = text_size::UI;

/// Fixed tab height: tall enough for the label row plus the hover wash's
/// breathing room, and the unit the strip's other controls center against.
/// Tabs run flush to the strip's bottom edge (browser-style), so this — plus
/// the strip's top padding — decides the whole strip's height.
const TAB_HEIGHT: f32 = 29.0;

/// Gap between the inactive hover wash's bottom edge and the strip floor —
/// what keeps the wash reading as floating rather than connected like the
/// active tab.
const WASH_BOTTOM_GAP: f32 = 3.0;

/// Gap between neighboring tabs, and between the final tab and the add button.
const TAB_GAP: f32 = 3.0;

/// Label padding inside a tab's fill / hover wash.
const LABEL_PAD_X: f32 = 8.0;

/// Both modal cards (confirm, open-repo) hang at the same distance below the
/// top edge, like the command palette — anchored rather than centered, so
/// they don't jump when their content grows (error lines, recents).
const DIALOG_TOP_OFFSET: f32 = 120.0;

/// Build the title-bar tab strip. Returns an empty `Space` when no repos are
/// open (the empty-state view owns the window in that case).
pub fn build_tab_bar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }

    let mut tabs = Vec::with_capacity(ui.tabs.len() + 1);
    for (index, tab) in ui.tabs.iter().enumerate() {
        let active = index == ui.active_tab;
        tabs.push(tab_widget(ui, theme, tab, active));
    }
    tabs.push(add_button(theme));
    let tabs_row = TabStrip {
        row: Row::with_children(tabs)
            .spacing(TAB_GAP)
            .align_y(alignment::Vertical::Center),
        active: ui.active_tab,
        theme,
    };

    // Top padding only: the tabs run flush to the strip's bottom edge so the
    // active one connects to the toolbar band below (see `tab_widget`). When
    // the strip stands in for the title bar (macOS) it's pinned to a fixed
    // height with the content bottom-aligned instead; the traffic lights stay
    // centered on the full strip (see `chrome::position_window_controls`),
    // like a native browser window.
    let titlebar_height = chrome::title_bar_height();
    let v_pad = if titlebar_height.is_some() { 0.0 } else { 6.0 };
    let mut strip = row![]
        .align_y(alignment::Vertical::Center)
        .spacing(6)
        .padding(Padding {
            top: v_pad,
            right: 8.0,
            bottom: 0.0,
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
            .align_y(alignment::Vertical::Bottom);
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

/// Background for the tab strip / title-bar surface: a recessed band, darker
/// than the toolbar below it, so the active tab — filled with the toolbar's
/// color — reads as a connected block against it (browser-style). No border:
/// the seam between the active tab and the toolbar must stay invisible.
fn bar_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(well_fill(theme))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
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

    // Browser-style tab: the active tab is a top-rounded block filled with the
    // toolbar band's own color and running flush to the strip's bottom edge.
    // Its concave joins share the top corners' radius and are painted outside
    // layout in the strip's background pass, continuing beneath neighboring
    // inactive tab areas.
    // Inactive tabs stay quiet; hovering one paints a floating wash over the
    // same box minus `WASH_BOTTOM_GAP`. The label centers in the same
    // top-anchored box in every state, so nothing shifts on hover or selection.
    let hovered = ui.hovered == Some(HoverTarget::Tab(id));
    let wash_fill = (!active && hovered).then(|| chip_background(theme.muted_text));
    let wash = container(inner)
        .padding(Padding::from([0.0, LABEL_PAD_X]))
        .height(Length::Fixed(TAB_HEIGHT - WASH_BOTTOM_GAP))
        .align_y(alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: wash_fill.map(Background::Color),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: radius::BUTTON.into(),
            },
            ..container::Style::default()
        });

    let body = container(wash)
        .height(Length::Fixed(TAB_HEIGHT))
        .align_y(alignment::Vertical::Top)
        .style(move |_| container::Style {
            background: active.then_some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: iced::border::top(radius::BUTTON),
            },
            ..container::Style::default()
        });

    // The close `×` captures its own press first (iced dispatches to children
    // before the parent), so hitting `×` closes rather than selects. Hover is
    // tracked in app state; `on_move` re-asserts it every frame the cursor is
    // over the tab, so the one-frame flicker that `on_enter`/`on_exit` race
    // into when crossing between adjacent tabs is immediately corrected.
    let interactive = mouse_area(body)
        .on_press(Message::SelectTab(id))
        .on_enter(Message::SetHover(Some(HoverTarget::Tab(id))))
        .on_move(move |_| Message::SetHover(Some(HoverTarget::Tab(id))))
        .on_exit(Message::SetHover(None))
        .interaction(mouse::Interaction::Pointer);

    interactive.into()
}

struct TabStrip<'a> {
    row: Row<'a, Message>,
    active: usize,
    theme: ThemeSpec,
}

impl Widget<Message, Theme, iced::Renderer> for TabStrip<'_> {
    fn children(&self) -> Vec<Tree> {
        self.row.children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.row.diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.row.size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.row.layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.row.operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.row
            .update(tree, event, layout, cursor, renderer, shell, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> advanced_mouse::Interaction {
        self.row
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let active_bounds = layout.children().nth(self.active).map(|tab| tab.bounds());
        if let Some(bounds) = active_bounds {
            draw_join_fillet(renderer, self.theme, bounds, true);
            draw_join_fillet(renderer, self.theme, bounds, false);
        }
        self.row
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.row
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

impl<'a> From<TabStrip<'a>> for Element<'a, Message> {
    fn from(strip: TabStrip<'a>) -> Self {
        Element::new(strip)
    }
}

fn draw_join_fillet(renderer: &mut iced::Renderer, theme: ThemeSpec, tab: Rectangle, left: bool) {
    let panel_bounds = Rectangle {
        x: if left {
            tab.x - radius::BUTTON
        } else {
            tab.x + tab.width
        },
        y: tab.y + tab.height - radius::BUTTON,
        width: radius::BUTTON,
        height: radius::BUTTON,
    };
    renderer.fill_quad(
        renderer::Quad {
            bounds: panel_bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        theme.panel_background_elevated,
    );

    let cover_bounds = Rectangle {
        x: if left {
            tab.x - radius::BUTTON * 2.0
        } else {
            tab.x + tab.width
        },
        y: tab.y + tab.height - radius::BUTTON * 2.0,
        width: radius::BUTTON * 2.0,
        height: radius::BUTTON * 2.0,
    };
    renderer.fill_quad(
        renderer::Quad {
            bounds: cover_bounds,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: if left {
                    iced::border::bottom_right(radius::BUTTON)
                } else {
                    iced::border::bottom_left(radius::BUTTON)
                },
            },
            shadow: Shadow::default(),
            snap: true,
        },
        well_fill(theme),
    );
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

/// `+` button — opens the path dialog to add another repository. A square
/// sized and anchored exactly like an inactive tab's hover wash (same
/// height, same top edge, same clearance from the strip floor), so the
/// strip's controls sit on one visual grid.
fn add_button(theme: ThemeSpec) -> Element<'static, Message> {
    let side = TAB_HEIGHT - WASH_BOTTOM_GAP;
    container(
        button(container(icons::icon(icons::PLUS, 15.0, theme.muted_text)).center(Length::Fill))
            .width(Length::Fixed(side))
            .height(Length::Fixed(side))
            .padding(0)
            .on_press(Message::OpenRepoDialogOpen)
            .style(move |_, status| ghost_button_style(theme, status)),
    )
    .height(Length::Fixed(TAB_HEIGHT))
    .align_y(alignment::Vertical::Top)
    .into()
}

/// The right-hand `⌘K` control. The design put a repo-search `⌘P` here; per
/// the spec it instead opens our existing command palette. Styled as a
/// keycap — hairline border, mono glyphs — so it reads as "press this key",
/// not as another action button.
fn palette_hint(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    let chip = container(
        text(chrome::cmd_label("K"))
            .size(text_size::CAPTION)
            .color(theme.muted_text)
            .font(mono),
    )
    .padding(Padding::from([1, 5]))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(theme.muted_text))),
        border: Border {
            width: 1.0,
            color: theme.border,
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
                radius: radius::CONTROL.into(),
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
    .padding(Padding::from([7, 16]))
    .on_press(Message::ConfirmCancel)
    .style(move |_, status| dialog_button_style(theme, status));

    // Red fill: the confirm runs a mutation the jj CLI refuses by default.
    let accept = button(
        text(dialog.confirm_label.as_str())
            .size(text_size::BODY)
            .color(theme.background)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 16]))
    .on_press(Message::ConfirmAccept)
    .style(move |_, _| destructive_button_style(theme));

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
            .padding(Padding::from([20, 22]))
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::ConfirmNoOp);

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: DIALOG_TOP_OFFSET,
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
                radius: radius::CONTROL.into(),
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
            // Recessed into the elevated card, otherwise identical to the
            // shared input identity.
            background: Background::Color(theme.background),
            ..input_style(theme)
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
    .padding(Padding::from([7, 16]))
    .on_press(Message::OpenRepoDialogClose)
    .style(move |_, status| dialog_button_style(theme, status));

    let open = button(
        text("Open")
            .size(text_size::BODY)
            .color(theme.background)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 16]))
    .on_press(Message::OpenRepoSubmit)
    .style(move |_, _| primary_button_style(theme));

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
            .padding(Padding::from([20, 22]))
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::OpenRepoNoOp);

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: DIALOG_TOP_OFFSET,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal (see `activity_popover` for the mechanics).
    opaque(stack![scrim, centered])
}
