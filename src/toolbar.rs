//! The actions toolbar: a full-width band below the tab strip. On the left,
//! the view switcher plus the view's actions — Refresh, Fetch (a split
//! button), and Undo (jj only) in the diff view; the browsed-revision label
//! in the source view. On the right, the activity indicator then the display
//! toggles (wrap / split), with a thin progress line along the bar's bottom
//! edge. The toolbar dropdowns (fetch branches / revset presets) render as
//! iced overlays anchored near their trigger.

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment, mouse,
    widget::{Space, button, column, container, mouse_area, row, text, tooltip},
};

use crate::activity;
use crate::icons;
use crate::repository::Vcs;
use crate::sidebar;
use crate::theme::{self, ThemeSpec, chip_background, radius, text_size, well_fill};
use crate::ui;
use crate::{Action, Diffui, FetchTarget, HoverTarget, MainView, Message, ToolbarMenu, UiEvent};
use diffui_core::RevisionSelection;

/// Every control in the bar is one [`ui::Size::Compact`] step tall.
const CONTROL: ui::Size = ui::Size::Compact;

/// The bar's own height, without the progress line and the rule under it.
pub const HEIGHT: f32 = theme::control::COMPACT + 2.0 * theme::space::SM;

/// Where the window chrome (tab strip, toolbar, progress line, rule) ends and
/// the panels begin — what floating surfaces anchored under the toolbar hang
/// from.
pub fn chrome_bottom() -> f32 {
    crate::tab_bar::height() + HEIGHT + activity::PROGRESS_LINE_HEIGHT + 1.0
}

/// Size of the dropdown carets (fetch split button, revset presets). A touch
/// smaller than the action icons so the caret reads as a subordinate affordance.
const CARET_ICON_SIZE: f32 = 12.0;

/// Width of the fetch split button's caret half.
const CARET_WIDTH: f32 = 24.0;

/// Char budget for the browsed revision's description line in the toolbar.
const DESCRIPTION_MAX_CHARS: usize = 72;

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
    let is_jj = matches!(
        ui.active().repository.as_ref().map(|r| r.vcs),
        Some(Vcs::Jj)
    );

    let caret_hovered = ui.hovered == Some(HoverTarget::FetchCaret);
    let mut actions = row![]
        .spacing(theme::space::SM)
        .align_y(alignment::Vertical::Center);
    // Diff ↔ Source view switcher, leftmost so the "what am I looking at"
    // control leads the bar. Repo tabs only — a PR has no tree to browse.
    let is_repo = ui.active().repository.is_some();
    let in_source = is_repo && ui.active().main_view == MainView::Source;
    if is_repo {
        actions = actions.push(view_switcher(ui, theme, font));
    }
    if in_source {
        // The repo-mutating actions don't apply to browsing a snapshot;
        // instead say *what* is being browsed.
        actions = actions.push(browsed_revision_label(ui, theme, is_jj));
    } else {
        actions = actions.push(toolbar_button(
            icons::REFRESH,
            "Refresh",
            Message::Action(Action::Refresh),
            theme,
            font,
        ));
        actions = actions.push(fetch_split_button(theme, font, caret_hovered));
        // jj-only: git has no operation log to undo.
        if is_jj {
            actions = actions.push(toolbar_button(
                icons::UNDO,
                "Undo",
                Message::Action(Action::Undo),
                theme,
                font,
            ));
        }
    }

    // Display toggles live at the far right edge, past the activity
    // indicator, so actions (left) and view options (right) read as two
    // separate groups.
    let mut toggles = row![]
        .spacing(theme::space::SM)
        .align_y(alignment::Vertical::Center);
    toggles = toggles.push(toolbar_toggle_button(
        icons::WRAP,
        "Wrap lines",
        ui.diff_wrap,
        Message::Action(Action::ToggleWrap),
        theme,
        font,
    ));
    // Side-by-side only applies to the diff; the source view is one column
    // by nature, so the toggle hides rather than sitting there inert.
    if ui.active().main_view == MainView::Diff {
        toggles = toggles.push(toolbar_toggle_button(
            icons::SPLIT,
            "Side-by-side diff",
            ui.diff_split,
            Message::Action(Action::ToggleSplit),
            theme,
            font,
        ));
    }

    let bar = row![
        actions,
        Space::new().width(Length::Fill),
        activity::activity_indicator(ui, theme),
        toggles,
    ]
    .align_y(alignment::Vertical::Center)
    .spacing(theme::space::SM)
    .height(Length::Fixed(HEIGHT))
    // The same 8px edge inset as the tab strip above and the panel headers
    // below, so the bar's first and last controls line up with both.
    .padding(Padding::from([theme::space::SM, theme::space::MD]));

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

/// The Diff ↔ Source view switcher: a segmented control — a recessed well
/// (window-background fill) holding two tabs, the active one raised as a
/// bordered pill. Deliberately *not* the bordered-button-group look of the
/// fetch split button: this switches what the window shows, so it reads as
/// tabs, not as an action.
fn view_switcher(ui: &Diffui, theme: ThemeSpec, font: iced::Font) -> Element<'static, Message> {
    // The well is one control step tall; the raised pill sits inside it on
    // an even 2px inset, so its corners stay concentric with the well's.
    const INSET: f32 = 2.0;
    const SEGMENT_HEIGHT: f32 = theme::control::COMPACT - 2.0 * INSET;
    let segment = |icon: &'static str, label: &str, view: MainView| {
        let active = ui.active().main_view == view;
        let icon_color = if active {
            theme.accent
        } else {
            theme.subtle_text
        };
        let text_color = if active { theme.text } else { theme.muted_text };
        button(theme::centered_control_content(
            row![
                icons::icon(icon, ui::ICON_SIZE, icon_color),
                text(label.to_owned())
                    .size(text_size::UI)
                    .color(text_color)
                    .font(font),
            ]
            .spacing(6)
            .align_y(alignment::Vertical::Center),
        ))
        .height(Length::Fixed(SEGMENT_HEIGHT))
        .padding(Padding::from([0.0, 10.0 - INSET]))
        .on_press(Message::Action(Action::SetMainView(view)))
        .style(move |_, _| button::Style {
            // Only the active segment carries a fill — inactive segments
            // stay quiet even under the cursor; the pointer + label color
            // are enough affordance inside a two-option control.
            background: active.then_some(Background::Color(theme.panel_background_elevated)),
            text_color,
            border: Border {
                width: if active { 1.0 } else { 0.0 },
                color: if active {
                    theme.border
                } else {
                    Color::TRANSPARENT
                },
                radius: (radius::BUTTON - INSET).into(),
            },
            shadow: iced::Shadow::default(),
            snap: true,
        })
    };

    container(
        row![
            segment(icons::FILE_DIFF, "Diff", MainView::Diff),
            segment(icons::CODE, "Source", MainView::Source),
        ]
        .spacing(theme::space::XXS)
        .align_y(alignment::Vertical::Center),
    )
    .padding(INSET)
    .style(move |_| container::Style {
        background: Some(Background::Color(well_fill(theme))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::BUTTON.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// A toolbar action: icon + label, invisible at rest with a soft wash on
/// hover, so the bar reads as one calm surface.
fn toolbar_button(
    icon: &'static str,
    label: &str,
    message: Message,
    theme: ThemeSpec,
    font: iced::Font,
) -> Element<'static, Message> {
    ui::text_button(
        Some((icon, theme.muted_text)),
        label.to_owned(),
        ui::Variant::Ghost,
        CONTROL,
        font,
        theme,
    )
    .on_press(message)
    .into()
}

/// A toolbar toggle: an icon-only [`toolbar_button`] with a persistent accent
/// tint while active so the on state reads at a glance. The label lives in a
/// hover tooltip instead of beside the glyph — these are view options, not
/// actions, so they stay compact at the bar's right edge.
fn toolbar_toggle_button(
    icon: &'static str,
    label: &'static str,
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
    let btn = ui::icon_button(icon, icon_color, CONTROL, theme)
        .on_press(message)
        .style(move |_, status| {
            let mut style = theme::ghost_button_style(theme, status);
            if active {
                style.background = Some(Background::Color(chip_background(theme.accent)));
            }
            style
        });
    ui::tooltip(btn, label, tooltip::Position::Bottom, font, theme)
}

/// What the source browser is looking at, in the toolbar slot the diff view's
/// actions occupy: revision id (change id for jj, commit hash for git) plus
/// the first line of its description. The working copy trades the id for an
/// accent-tinted "@ working copy" chip so a live, still-changing snapshot
/// can't be mistaken for a pinned commit.
fn browsed_revision_label(ui: &Diffui, theme: ThemeSpec, is_jj: bool) -> Element<'_, Message> {
    let mono = ui.config.mono_font;
    let revision = crate::source_panel::browsed_revision(&ui.active().source);
    let (commit, working_copy) = match &revision {
        RevisionSelection::WorkingCopy => (ui.active().session.commits.working_copy(), true),
        RevisionSelection::Commit(hex) => {
            (ui.active().session.commits.find_by_commit_id(hex), false)
        }
    };

    let mut label = row![]
        .spacing(theme::space::MD)
        .height(Length::Fixed(CONTROL.height()))
        .align_y(alignment::Vertical::Center)
        .padding(Padding::from([0.0, theme::space::XS]));

    if working_copy {
        label = label.push(
            container(
                text("@ working copy")
                    .size(text_size::CAPTION)
                    .font(mono)
                    .color(theme.accent),
            )
            .padding(Padding::from([1, 6]))
            .style(move |_| container::Style {
                background: Some(Background::Color(chip_background(theme.accent))),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: crate::chip::RADIUS.into(),
                },
                ..container::Style::default()
            }),
        );
    } else {
        let id = match (&revision, commit) {
            (_, Some(row)) if is_jj => {
                sidebar::truncate_end(row.change_id(), sidebar::REVISION_ID_CHARS)
            }
            (_, Some(row)) => sidebar::truncate_end(row.commit_id(), sidebar::REVISION_ID_CHARS),
            (RevisionSelection::Commit(hex), None) => {
                sidebar::truncate_end(hex, sidebar::REVISION_ID_CHARS)
            }
            (RevisionSelection::WorkingCopy, None) => String::new(),
        };
        // Same size as the description beside it: iced centers line boxes
        // rather than aligning baselines, so equal sizes are what keep the
        // two texts sitting on one line.
        label = label.push(
            text(id)
                .size(text_size::BODY)
                .font(mono)
                .color(theme.muted_text),
        );
    }

    // Git's working copy is synthesized and its "description" is just the
    // words the chip already says.
    if let Some(row) = commit.filter(|_| is_jj || !working_copy) {
        let (line, color) = if row.has_description() {
            let first = row.description().lines().next().unwrap_or("").to_owned();
            (ellipsize(&first, DESCRIPTION_MAX_CHARS), theme.text)
        } else {
            ("No description".to_owned(), theme.subtle_text)
        };
        label = label.push(
            text(line)
                .size(text_size::BODY)
                .font(ui.config.ui_font)
                .color(color),
        );
    }

    label.into()
}

/// Char-budget ellipsis for the toolbar's description line — the toolbar has
/// no text-truncation layout, so an explicit cut keeps a long summary from
/// shoving the right-edge toggles around.
fn ellipsize(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The Fetch split button: a main "Fetch" action + a caret that opens the
/// remote-branch menu, joined by a short hairline — the seam is what marks
/// the two ghost halves as one group. They highlight **independently** on
/// hover so the seam also reads as "two actions".
fn fetch_split_button(
    theme: ThemeSpec,
    font: iced::Font,
    caret_hovered: bool,
) -> Element<'static, Message> {
    // `button` highlights its own half on hover (its `Hovered` status) and shows
    // the pointer cursor automatically.
    let main = ui::text_button(
        Some((icons::FETCH, theme.muted_text)),
        "Fetch",
        ui::Variant::Ghost,
        CONTROL,
        font,
        theme,
    )
    .on_press(Message::Action(Action::Fetch(FetchTarget::AllRemotes)));

    // A `mouse_area` (not a `button`) with no press handler: the press falls
    // through to the wrapping `AnchorArea` (the main "Fetch" `button` captures
    // its own, so only a press on this caret half opens the menu), which reports
    // the whole split button's rect so the dropdown anchors edge-to-edge below
    // it. Hover + cursor are set manually since mouse_area has neither.
    let caret = mouse_area(
        container(caret_glyph(theme.muted_text, CONTROL.height()))
            .width(Length::Fixed(CARET_WIDTH))
            .center_x(Length::Fixed(CARET_WIDTH))
            .style(move |_| caret_hover_style(theme, caret_hovered, radius::BUTTON)),
    )
    .on_enter(Message::Ui(UiEvent::SetHover(Some(
        HoverTarget::FetchCaret,
    ))))
    .on_exit(Message::Ui(UiEvent::SetHover(None)))
    .interaction(mouse::Interaction::Pointer);

    // As tall as the glyphs beside it.
    let divider = container(Space::new())
        .width(Length::Fixed(1.0))
        .height(Length::Fixed(ui::ICON_SIZE))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        });

    let split = row![main, divider, caret]
        .spacing(0)
        .align_y(alignment::Vertical::Center);

    crate::menu::anchor_area(split, |rect| {
        Message::Ui(UiEvent::OpenToolbarMenu(ToolbarMenu::FetchBranches, rect))
    })
    .into()
}

/// Hover background for a `mouse_area`-based caret (fetch / revset). Shared so
/// both carets highlight identically. `hovered` is tracked in app state since
/// `mouse_area` has no built-in hover style. `radius` follows the caret's
/// context: BUTTON beside toolbar buttons, CONTROL inside a filter field.
pub(crate) fn caret_hover_style(theme: ThemeSpec, hovered: bool, radius: f32) -> container::Style {
    container::Style {
        background: hovered.then(|| Background::Color(chip_background(theme.muted_text))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius.into(),
        },
        ..container::Style::default()
    }
}
