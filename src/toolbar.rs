//! The actions toolbar: a full-width band below the tab strip. On the left,
//! the view switcher plus the view's actions — Refresh, Fetch (a split
//! button), and Undo (jj only) in the diff view; the browsed-revision label
//! in the source view. On the right, the activity indicator then the display
//! toggles (wrap / split), with a thin progress line along the bar's bottom
//! edge. The toolbar dropdowns (fetch branches / revset presets) are native
//! menus on macOS and anchored overlays elsewhere.

use diffui_core::RevisionSelection;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, ElementId, MouseButton, MouseDownEvent, SharedString, Window, div, px};

use crate::repository::Vcs;
use crate::theme::{
    self, Color, Font, ThemeSpec, Weight, chip_background, emphasis_font, radius, text_size,
    well_fill,
};
use crate::typography::{self, Aligned};
use crate::ui::Tip as _;
use crate::{Action, Cx, Diffui, FetchTarget, MainView, ToolbarMenu, activity, icons, sidebar, ui};

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

/// Where every modal surface (command palette, open-repo and confirm
/// dialogs) hangs from: a fixed distance under the window chrome rather than
/// centered, so a card doesn't jump as its content grows and stacked
/// takeovers line up with one another.
pub fn modal_top() -> f32 {
    chrome_bottom() + theme::space::XXL * 2.0
}

/// Size of the dropdown carets (fetch split button, revset presets). A touch
/// smaller than the action icons so the caret reads as a subordinate affordance.
const CARET_ICON_SIZE: f32 = 12.0;

/// Between the controls of one group; separators part the groups.
const GROUP_GAP: f32 = theme::space::XXS;

/// The push target chip: between its glyph, name and count, and before it.
const PUSH_CHIP_GAP: f32 = 4.0;
/// Between Push's glyph and its label, as in every labelled button.
const PUSH_ICON_GAP: f32 = 6.0;
/// The widest a bookmark name in the push chip gets before it ellipsises.
const PUSH_TARGET_MAX_WIDTH: f32 = 200.0;

/// A group separator's height.
const SEPARATOR_HEIGHT: f32 = 18.0;

/// Width of the fetch split button's caret half.
const CARET_WIDTH: f32 = 24.0;

/// Char budget for the browsed revision's description line in the toolbar.
const DESCRIPTION_MAX_CHARS: usize = 72;

/// One tab of a [`segmented`] control. No `on_press` disables it.
pub(crate) struct Segment {
    pub(crate) icon: Option<&'static str>,
    pub(crate) label: &'static str,
    pub(crate) active: bool,
    pub(crate) on_press: Option<crate::Action>,
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

/// The shared dropdown caret: a Lucide chevron centered in a `box_height`-tall
/// box. The caller adds the hover fill around this, so sizing the box to the
/// neighbour's height keeps the fill exactly as tall as the button or input beside
/// it. Reused by the revset caret in the sidebar.
pub(crate) fn caret_glyph(
    color: Color,
    box_height: f32,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(box_height))
        .child(icons::icon(icons::CHEVRON_DOWN, CARET_ICON_SIZE, color))
        .into_any_element()
}

/// Hover fill of a dropdown caret (fetch / revset), so both highlight identically.
/// `radius` follows the caret's context: BUTTON beside toolbar buttons, CONTROL
/// inside a filter field.
pub(crate) fn caret_hover<E: StatefulInteractiveElement + Styled>(
    caret: E,
    theme: ThemeSpec,
    radius: f32,
) -> E {
    caret
        .rounded(px(radius))
        .hover(move |caret| caret.bg(theme::hover_wash(theme)))
}

/// Build the actions toolbar. Empty when no repo is open (the welcome screen owns
/// the window then).
pub(crate) fn build_toolbar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.tabs.is_empty() {
        return div().into_any_element();
    }
    let font = ui.config.ui_font;
    let is_jj = matches!(
        ui.active().repository.as_ref().map(|r| r.vcs),
        Some(Vcs::Jj)
    );

    let mut actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(GROUP_GAP))
        .flex_none()
        .child(panel_toggles(ui, theme, font, cx))
        .child(separator(theme));
    // Diff ↔ Source view switcher, so the "what am I looking at" control leads
    // the bar after the panels. Repo tabs only: a PR has no tree to browse.
    let is_repo = ui.active().repository.is_some();
    let in_source = is_repo && ui.active().main_view == MainView::Source;
    if is_repo {
        actions = actions
            .child(view_switcher(ui, theme, font, window, cx))
            .child(separator(theme));
    }
    if in_source {
        // The repo-mutating actions don't apply to browsing a snapshot; instead
        // say *what* is being browsed.
        actions = actions.child(browsed_revision_label(ui, theme, is_jj));
    } else {
        actions = actions
            .child(toolbar_button(
                "toolbar-refresh",
                icons::REFRESH,
                "Refresh",
                Action::Refresh,
                theme,
                font,
                cx,
            ))
            .child(fetch_split_button(theme, font, window, cx));
        // Plain `jj git push` (tracked bookmarks that moved), behind a
        // confirmation listing what goes out.
        if is_jj
            && ui.active().session.capabilities.mutate
            && !ui.active().session.bookmarks.remotes.is_empty()
        {
            actions = actions.child(push_button(ui, theme, font, cx));
        }
        // jj-only: git has no operation log to undo.
        if is_jj {
            actions = actions
                .child(separator(theme))
                .child(toggle_button(
                    "toolbar-undo",
                    icons::UNDO,
                    with_shortcut(ui, "Undo the last operation", "repo.undo"),
                    false,
                    Some(Action::Undo),
                    theme,
                    font,
                    cx,
                ))
                .child(toggle_button(
                    "toolbar-redo",
                    icons::REDO,
                    with_shortcut(ui, "Redo the last undone operation", "repo.redo"),
                    false,
                    Some(Action::Redo),
                    theme,
                    font,
                    cx,
                ));
        }
    }

    // Display toggles live at the far right edge, past the activity indicator, so
    // actions (left) and view options (right) read as two separate groups.
    let mut toggles = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(GROUP_GAP));
    if crate::changes::can_select_lines(ui) {
        toggles = toggles.child(toolbar_toggle_button(
            "toolbar-lines",
            icons::LIST_CHECKS,
            "Select lines (v)".into(),
            crate::changes::live_line_select(ui.active()).is_some(),
            Action::Changes(crate::changes::ChangesMsg::Lines(
                crate::changes::LineMsg::Toggle,
            )),
            theme,
            font,
            cx,
        ));
    }
    if crate::media::shows_svg_preview(ui) {
        toggles = toggles.child(svg_switcher(ui, theme, font, window, cx));
    }
    toggles = toggles.child(toolbar_toggle_button(
        "toolbar-wrap",
        icons::WRAP,
        "Wrap lines".into(),
        ui.diff_wrap,
        Action::ToggleWrap,
        theme,
        font,
        cx,
    ));
    // Side-by-side only applies to the diff; the source view is one column by
    // nature, so the toggle hides rather than sitting there inert.
    if ui.active().main_view != MainView::Source {
        // On but too narrow to honour: the diff falls back to unified, and the
        // tooltip says why rather than the toggle looking broken.
        let split_label = if ui.diff_split && !crate::diff_panel::split_fits(ui) {
            "Side-by-side diff (window too narrow, showing unified)"
        } else {
            "Side-by-side diff"
        };
        toggles = toggles.child(toolbar_toggle_button(
            "toolbar-split",
            icons::SPLIT,
            split_label.into(),
            ui.diff_split,
            Action::ToggleSplit,
            theme,
            font,
            cx,
        ));
    }

    // The same 8px edge inset as the tab strip above and the panel headers below,
    // so the bar's first and last controls line up with both.
    let bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .w_full()
        .h(px(HEIGHT))
        .px(px(theme::space::MD))
        // The window's chrome, one section with the tab strip above.
        .bg(well_fill(theme))
        // Too narrow for both, the actions stay whole and the right group runs off
        // the edge.
        .overflow_hidden()
        .child(actions)
        .child(div().flex_1().min_w_0())
        .child(
            div()
                .flex_none()
                .child(activity::activity_indicator(ui, theme, window, cx)),
        )
        .child(toggles);

    div()
        .flex()
        .flex_col()
        .w_full()
        .flex_none()
        .bg(well_fill(theme))
        .child(bar)
        .child(
            div()
                .w_full()
                .h(px(activity::PROGRESS_LINE_HEIGHT))
                .overflow_hidden()
                .child(activity::activity_progress_line(ui, theme, window, cx)),
        )
        .into_any_element()
}

/// The Diff ↔ Source view switcher, a [`segmented`] control. Deliberately *not*
/// the bordered-button-group look of the fetch split button: this switches what
/// the window shows, so it reads as tabs, not as an action.
fn view_switcher(
    ui: &Diffui,
    theme: ThemeSpec,
    font: Font,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let segment = |icon: &'static str, label: &'static str, view: MainView| Segment {
        icon: Some(icon),
        label,
        active: ui.active().main_view == view,
        on_press: Some(Action::SetMainView(view)),
    };
    let mut segments = vec![
        segment(icons::FILE_DIFF, "Diff", MainView::Diff),
        segment(icons::CODE, "Source", MainView::Source),
    ];
    if crate::history_panel::available(ui) {
        segments.push(segment(icons::HISTORY, "History", MainView::History));
    }
    segmented(segments, font, theme, window, cx)
}

/// How the diff shows SVG files: rendered, or as their markup's text diff. Only on
/// screen while the diff holds an SVG it can render.
fn svg_switcher(
    ui: &Diffui,
    theme: ThemeSpec,
    font: Font,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let segment = |icon: &'static str, label: &'static str, source: bool| Segment {
        icon: Some(icon),
        label,
        active: ui.svg_source == source,
        on_press: Some(Action::SetSvgSource(source)),
    };
    segmented(
        vec![
            segment(icons::IMAGE, "Rendered", false),
            segment(icons::CODE_XML, "Markup", true),
        ],
        font,
        theme,
        window,
        cx,
    )
}

/// A segmented control, for switching between views of one thing: a recessed well
/// (window-background fill) holding the tabs, the active one raised as a ringed
/// pill.
pub(crate) fn segmented(
    segments: Vec<Segment>,
    font: Font,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    // The well is one control step tall; the raised pill sits inside it on an even
    // 2px inset, so its corners stay concentric with the well's.
    const INSET: f32 = 2.0;
    const SEGMENT_HEIGHT: f32 = theme::control::COMPACT - 2.0 * INSET;
    let mut well = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::XXS))
        .p(px(INSET))
        .rounded(px(radius::BUTTON))
        // A step into the chrome it sits on.
        .bg(Color {
            a: 0.07,
            ..theme.text
        });
    for segment in segments {
        let Segment {
            icon,
            label,
            active,
            on_press,
        } = segment;
        let enabled = on_press.is_some();
        let icon_color = if active {
            theme.accent
        } else {
            theme.subtle_text
        };
        let text_color = if active {
            theme.text
        } else if enabled {
            theme.muted_text
        } else {
            theme.subtle_text
        };
        let id = ElementId::Name(SharedString::from(format!("segment-{label}")));
        // Only the active segment carries a fill: inactive segments stay quiet
        // even under the cursor; the pointer + label color are enough affordance
        // inside a two-option control.
        let mut tab = div()
            .id(id)
            .test_support()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(SEGMENT_HEIGHT))
            .px(px(10.0 - INSET))
            .rounded(px(radius::BUTTON - INSET))
            .when(active, |tab| {
                tab.bg(theme.panel_background_elevated)
                    .shadow(theme::raised_shadow(theme))
            })
            .children(icon.map(|icon| icons::icon(icon, ui::ICON_SIZE, icon_color)))
            .child(typography::centered(
                typography::line(label, font, text_size::UI, text_color),
                font,
                text_size::UI,
            ));
        if let Some(action) = on_press {
            tab = tab.cursor_pointer().on_click(Diffui::on_action(action, cx));
        }
        well = well.child(tab);
    }
    well.into_any_element()
}

/// A toolbar action: icon + label, invisible at rest with a soft wash on hover, so
/// the bar reads as one calm surface.
fn toolbar_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    action: Action,
    theme: ThemeSpec,
    font: Font,
    cx: &mut Cx,
) -> impl IntoElement {
    ui::text_button(
        id,
        Some((icon, theme.muted_text)),
        label,
        ui::Variant::Ghost,
        CONTROL,
        font,
        theme,
        cx,
    )
    .flex_none()
    .on_click(Diffui::on_action(action, cx))
}

/// A toolbar toggle: an icon-only button with a persistent accent tint while
/// active so the on state reads at a glance. The label lives in a hover tooltip
/// instead of beside the glyph: these are view options, not actions, so they stay
/// compact.
#[allow(clippy::too_many_arguments)]
fn toolbar_toggle_button(
    id: &'static str,
    icon: &'static str,
    label: SharedString,
    active: bool,
    action: Action,
    theme: ThemeSpec,
    font: Font,
    cx: &mut Cx,
) -> impl IntoElement {
    toggle_button(id, icon, label, active, Some(action), theme, font, cx)
}

/// [`toolbar_toggle_button`], inert (greyed, no press) without an action.
#[allow(clippy::too_many_arguments)]
fn toggle_button(
    id: &'static str,
    icon: &'static str,
    label: SharedString,
    active: bool,
    action: Option<Action>,
    theme: ThemeSpec,
    font: Font,
    cx: &mut Cx,
) -> AnyElement {
    let icon_color = match (&action, active) {
        (None, _) => theme.subtle_text,
        (Some(_), true) => theme.accent,
        (Some(_), false) => theme.muted_text,
    };
    let mut button = ui::icon_button(id, icon, icon_color, CONTROL, theme, cx);
    if active {
        button = button.bg(chip_background(theme.accent));
    }
    button = match action {
        Some(action) => button.on_click(Diffui::on_action(action, cx)),
        None => button.disabled(true),
    };
    div()
        .id(ElementId::Name(SharedString::from(format!("{id}-tip"))))
        .flex_none()
        .tip(label, font, theme)
        .child(button)
        .into_any_element()
}

/// `label`, with the first key bound to `command` in parentheses.
pub(crate) fn with_shortcut(ui: &Diffui, label: &str, command: &'static str) -> SharedString {
    match ui.keymap.chord_for(crate::commands::Context::Base, command) {
        Some(chord) => format!("{label} ({})", chord.display()).into(),
        None => SharedString::from(label.to_owned()),
    }
}

/// The two panel toggles that lead the bar: the revision list and the changed
/// files beside the diff. Only the Diff view hides its panels; the others keep
/// theirs, so there the toggles sit inert.
fn panel_toggles(ui: &Diffui, theme: ThemeSpec, font: Font, cx: &mut Cx) -> impl IntoElement {
    let tab = ui.active();
    let in_diff = tab.main_view == MainView::Diff;
    let toggle = |id: &'static str,
                  icon: &'static str,
                  shown: bool,
                  what: &'static str,
                  command: &'static str,
                  action: Action,
                  cx: &mut Cx| {
        let label = if !in_diff {
            SharedString::from(format!("Only the Diff view hides its {what}"))
        } else {
            with_shortcut(
                ui,
                &format!("{} {what}", if shown { "Hide" } else { "Show" }),
                command,
            )
        };
        toggle_button(
            id,
            icon,
            label,
            shown && in_diff,
            in_diff.then_some(action),
            theme,
            font,
            cx,
        )
    };
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(GROUP_GAP))
        .child(toggle(
            "toolbar-revisions",
            icons::PANEL_LEFT,
            !tab.history_panel_collapsed,
            "revisions",
            "view.toggle-revisions",
            Action::ToggleHistoryPanel,
            cx,
        ))
        .child(toggle(
            "toolbar-changed-files",
            icons::COLUMNS_3,
            !tab.files_panel_collapsed,
            "changed files",
            "view.toggle-changed-files",
            Action::ToggleFilesPanel,
            cx,
        ))
}

/// Push, saying what it pushes: the bookmark a plain `jj git push` would send
/// and how far it is ahead of the remote, or how many bookmarks go. Plain
/// "Push" when nothing would.
fn push_button(ui: &Diffui, theme: ThemeSpec, font: Font, cx: &mut Cx) -> AnyElement {
    let session = &ui.active().session;
    let bookmarks = &session.bookmarks;
    let remote = bookmarks.default_push_remote();
    let candidates = &bookmarks.tracked_push;

    let tip = match (remote, candidates.as_slice()) {
        (None, _) => "Push needs a remote: several are set up, and no git.push".to_owned(),
        (Some(remote), []) => format!("Nothing to push to {remote}"),
        (Some(remote), [one]) => format!(
            "Push {} to {remote}: {} {} ahead",
            one.name,
            one.ahead,
            if one.ahead == 1 {
                "revision"
            } else {
                "revisions"
            }
        ),
        (Some(remote), many) => format!(
            "Push {} bookmarks to {remote}: {}",
            many.len(),
            many.iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let size = CONTROL.text_size();
    let mut line = vec![
        Aligned::glyph(
            icons::icon(icons::UPLOAD, ui::ICON_SIZE, theme.muted_text),
            size,
        ),
        Aligned::gap(PUSH_ICON_GAP),
        Aligned::text(typography::line("Push", font, size, theme.text), font, size),
    ];
    let target = match candidates.as_slice() {
        [] => None,
        [one] => {
            let lane = session
                .commits
                .find_by_bookmark(&one.name)
                .map(|row| {
                    let lane = session.graph.frame(row.index(), usize::MAX).node_lane;
                    sidebar::revision_graph_style(theme).lane_color(lane)
                })
                .unwrap_or(theme.lanes[0]);
            Some((one.name.clone(), Some(one.ahead), lane))
        }
        many => Some((format!("{} bookmarks", many.len()), None, theme.lanes[0])),
    };
    if let Some((name, ahead, lane)) = target {
        let counts = emphasis_font(font, Weight::SEMIBOLD);
        let content = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(PUSH_CHIP_GAP))
            .child(icons::icon(icons::BOOKMARK, crate::chip::ICON_SIZE, lane))
            .child(
                div()
                    .max_w(px(PUSH_TARGET_MAX_WIDTH))
                    .overflow_hidden()
                    .child(typography::line(name, font, size, theme.muted_text).text_ellipsis()),
            )
            .children(
                ahead
                    .filter(|ahead| *ahead > 0)
                    .map(|ahead| typography::line(format!("↑{ahead}"), counts, size, theme.accent)),
            );
        line.push(Aligned::gap(PUSH_CHIP_GAP));
        line.push(Aligned::chip(
            content,
            font,
            size,
            [2.0, 6.0].into(),
            move |chip| {
                chip.bg(chip_background(theme.muted_text))
                    .rounded(px(crate::chip::RADIUS))
            },
        ));
    }

    let button = ui::button("toolbar-push", ui::Variant::Ghost, CONTROL, theme, cx)
        .flex_none()
        .child(typography::baseline_row(line, 0.0))
        .on_click(Diffui::on_action(
            Action::Bookmark(crate::bookmarks::BookmarkAction::PushTracked),
            cx,
        ));
    div()
        .id("toolbar-push-tip")
        .flex_none()
        .tip(with_shortcut(ui, &tip, "repo.push"), font, theme)
        .child(button)
        .into_any_element()
}

/// The hairline between two groups of the bar.
fn separator(theme: ThemeSpec) -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(SEPARATOR_HEIGHT))
        .mx(px(theme::space::XS))
        .flex_none()
        .bg(theme.border)
}

/// What the source browser is looking at, in the toolbar slot the diff view's
/// actions occupy: revision id (change id for jj, commit hash for git) plus the
/// first line of its description. The working copy trades the id for an
/// accent-tinted "@ working copy" chip so a live, still-changing snapshot can't be
/// mistaken for a pinned commit.
fn browsed_revision_label(ui: &Diffui, theme: ThemeSpec, is_jj: bool) -> impl IntoElement {
    let mono = ui.config.mono_font;
    let revision = crate::source_panel::browsed_revision(&ui.active().source);
    let (commit, working_copy) = match &revision {
        RevisionSelection::WorkingCopy => (ui.active().session.commits.working_copy(), true),
        RevisionSelection::Commit(hex) => {
            (ui.active().session.commits.find_by_commit_id(hex), false)
        }
    };

    // The chip, the mono id and the sans description share one baseline.
    let mut label: Vec<Aligned> = Vec::new();

    if working_copy {
        let accent = theme.accent;
        label.push(Aligned::chip(
            typography::line("@ working copy", mono, text_size::CAPTION, accent),
            mono,
            text_size::CAPTION,
            [1.0, 6.0].into(),
            move |chip| {
                chip.bg(chip_background(accent))
                    .rounded(px(crate::chip::RADIUS))
            },
        ));
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
        label.push(Aligned::text(
            typography::line(id, mono, text_size::BODY, theme.muted_text),
            mono,
            text_size::BODY,
        ));
    }

    // Git's working copy is synthesized and its "description" is just the words
    // the chip already says.
    if let Some(row) = commit.filter(|_| is_jj || !working_copy) {
        let (line, color) = if row.has_description() {
            let first = row.description().lines().next().unwrap_or("");
            (ellipsize(first, DESCRIPTION_MAX_CHARS), theme.text)
        } else {
            ("No description".to_owned(), theme.subtle_text)
        };
        label.push(
            Aligned::text(
                typography::line(line, ui.config.ui_font, text_size::BODY, color).text_ellipsis(),
                ui.config.ui_font,
                text_size::BODY,
            )
            .shrink(),
        );
    }

    div()
        .flex()
        .items_center()
        .min_w_0()
        .h(px(CONTROL.height()))
        .px(px(theme::space::XS))
        .child(typography::baseline_row(label, theme::space::MD))
}

/// The Fetch split button: a main "Fetch" action + a caret that opens the
/// remote-branch menu, joined by a short hairline: the seam is what marks the two
/// ghost halves as one group. They highlight **independently** on hover so the
/// seam also reads as "two actions".
fn fetch_split_button(
    theme: ThemeSpec,
    font: Font,
    window: &mut Window,
    cx: &mut Cx,
) -> impl IntoElement {
    let main = ui::text_button(
        "toolbar-fetch",
        Some((icons::FETCH, theme.muted_text)),
        "Fetch",
        ui::Variant::Ghost,
        CONTROL,
        font,
        theme,
        cx,
    )
    .on_click(Diffui::on_action(Action::Fetch(FetchTarget::Default), cx));

    // The whole split button's rect, so the dropdown anchors edge-to-edge below it.
    let (anchor, tracker) = crate::menu::anchor_tracker();
    // Opens on the press, as a native pop-up button does.
    let open = Diffui::listen(cx, move |this, _: &MouseDownEvent, _, cx| {
        this.on_open_toolbar_menu(ToolbarMenu::FetchBranches, anchor.get(), cx);
        cx.stop_propagation();
    });
    let caret = caret_hover(
        div().id("toolbar-fetch-caret").test_support().flex_none(),
        theme,
        radius::BUTTON,
    )
    .flex()
    .justify_center()
    .w(px(CARET_WIDTH))
    .cursor_pointer()
    .child(caret_glyph(theme.muted_text, CONTROL.height(), window, cx))
    .on_mouse_down(MouseButton::Left, open);

    // As tall as the glyphs beside it.
    let divider = div()
        .w(px(1.0))
        .h(px(ui::ICON_SIZE))
        .flex_none()
        .bg(theme.border);

    div()
        .relative()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .child(tracker)
        .child(main)
        .child(divider)
        .child(caret)
}
