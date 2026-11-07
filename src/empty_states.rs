//! What the revision list and the diff pane say when there is nothing (or not
//! the expected thing) to show: a revset that matches nothing, a revset that
//! doesn't evaluate, a load still running, and a diff pane holding on to a
//! revision the current view no longer lists.

use diffui_core::{LoadStatus, RevisionSelection};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::prose::Prose;
use crate::theme::{self, ThemeSpec, Weight, chip_background, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Cx, Diffui, icons, ui};

/// The prefixes jj-lib's revset errors arrive with once the actor has
/// flattened the chain (see `classify` in the core actor). Stripped for
/// display: the card's title already says the revset failed.
const REVSET_ERROR_PREFIXES: &[&str] = &["failed to parse revset: ", "failed to parse revset"];

/// Whether a load failure is the user's revset rather than the repository.
pub(crate) fn is_revset_error(message: &str) -> bool {
    message.starts_with("failed to parse revset")
        || message.contains("failed to resolve")
        || (message.contains("Revision") && message.contains("doesn't exist"))
}

/// Whether the diff pane is showing a revision the loaded list doesn't
/// contain — the revset changed under it, or it was picked from the palette.
pub(crate) fn selection_outside_view(ui: &Diffui) -> bool {
    let tab = ui.active();
    if tab.repository.is_none() || !matches!(tab.session.status, LoadStatus::Loaded) {
        return false;
    }
    let commits = &tab.session.commits;
    match &tab.session.selected_revision {
        RevisionSelection::WorkingCopy => commits.working_copy().is_none(),
        RevisionSelection::Commit(hex) => commits.find_by_commit_id(hex).is_none(),
    }
}

/// The revision list's stand-in while it has no rows: a load in flight, or a
/// revset that matched nothing. `None` when there are rows (or a failure,
/// which [`load_error`] reports above the list).
pub(crate) fn revision_list_placeholder(
    ui: &Diffui,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let tab = ui.active();
    if !tab.session.commits.is_empty() {
        return None;
    }
    let (ui_font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let body = match &tab.session.status {
        LoadStatus::Failed(_) => return None,
        LoadStatus::Loading => div().child(typography::line(
            "Loading revisions\u{2026}",
            ui_font,
            text_size::UI,
            theme.subtle_text,
        )),
        LoadStatus::Loaded => {
            let revset = tab.session.revset.trim();
            let revset = if revset.is_empty() { "(empty)" } else { revset };
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(theme::space::SM))
                .child(typography::line(
                    "No revisions match",
                    emphasis_font(ui_font, Weight::MEDIUM),
                    text_size::BODY,
                    theme.text,
                ))
                // The expression can be long: this one wraps.
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .font(mono.gpui())
                        .text_size(px(text_size::UI))
                        .text_color(theme.accent)
                        .child(revset.to_owned()),
                )
                .child(typography::line(
                    "Try another, or pick one from the presets menu on the filter.",
                    ui_font,
                    text_size::CAPTION,
                    theme.subtle_text,
                ))
                .children(
                    reset_button(ui, theme, cx)
                        .map(|reset| div().pt(px(theme::space::XS)).child(reset)),
                )
        }
    };
    Some(
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(theme::space::XXL * 2.0))
            .px(px(theme::space::XL))
            .child(body)
            .into_any_element(),
    )
}

/// "Reset to default" (the repo's own `revsets.log`, or jj's default), unless
/// that is what is already applied.
pub(crate) fn reset_button(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> Option<AnyElement> {
    let tab = ui.active();
    let default = tab.default_revset.clone();
    if tab.repository.is_none() || tab.session.revset.trim() == default.trim() {
        return None;
    }
    Some(
        ui::text_button(
            "reset-revset",
            None,
            "Reset to default",
            ui::Variant::Raised,
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
            cx,
        )
        .on_click(Diffui::on_action(Action::SetRevset(default), cx))
        .into_any_element(),
    )
}

/// The alert card above the list when the graph load failed. A revset error
/// gets its own wording, the parser's message in mono (it points a caret at
/// the offending column), and a way back to the default.
pub(crate) fn load_error(ui: &Diffui, theme: ThemeSpec, error: &str, cx: &mut Cx) -> AnyElement {
    let revset = is_revset_error(error);
    let (title, detail) = if revset {
        let detail = REVSET_ERROR_PREFIXES
            .iter()
            .find_map(|prefix| error.strip_prefix(prefix))
            .unwrap_or(error)
            .trim();
        ("Couldn't evaluate this revset", detail)
    } else {
        ("Couldn't load the history", error)
    };
    let title_font = emphasis_font(ui.config.ui_font, Weight::MEDIUM);
    let card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::SM))
        .px(px(theme::space::LG))
        .py(px(theme::space::MD))
        .rounded(px(theme::radius::CONTROL))
        .bg(chip_background(theme.removed_text))
        .child(typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.removed_text),
                    ui::ICON_SIZE,
                ),
                Aligned::text(
                    typography::line(title, title_font, text_size::UI, theme.removed_text),
                    title_font,
                    text_size::UI,
                ),
            ],
            theme::space::SM,
        ))
        .child(
            div()
                .font(
                    if revset {
                        ui.config.mono_font
                    } else {
                        ui.config.ui_font
                    }
                    .gpui(),
                )
                .text_size(px(text_size::CAPTION))
                .text_color(theme.text)
                .child(detail.to_owned()),
        )
        .when_some(
            if revset {
                reset_button(ui, theme, cx)
            } else {
                None
            },
            // A column stretches its children; the button keeps its own width.
            |card, reset| card.child(div().flex().child(reset)),
        );
    div()
        .w_full()
        .px(px(8.0))
        .py(px(6.0))
        .child(card)
        .into_any_element()
}

/// A strip over the diff saying so, when [`selection_outside_view`].
pub(crate) fn outside_view_banner(ui: &Diffui, theme: ThemeSpec) -> Option<AnyElement> {
    if !selection_outside_view(ui) {
        return None;
    }
    let label = match &ui.active().session.selected_revision {
        RevisionSelection::WorkingCopy => Prose::from("The working copy ").rev("@"),
        selection => ui.revision_prose(selection),
    };
    let sentence = label.text(" isn't in the current revset.").view(
        ui.config.ui_font,
        ui.config.mono_font,
        theme.muted_text,
        theme,
    );
    Some(
        div()
            .w_full()
            .px(px(theme::space::LG))
            .py(px(theme::space::SM))
            .bg(chip_background(theme.info))
            .child(typography::baseline_row(
                [
                    Aligned::glyph(
                        icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.info),
                        ui::ICON_SIZE,
                    ),
                    Aligned::text(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(text_size::UI))
                            .child(sentence),
                        ui.config.ui_font,
                        text_size::UI,
                    ),
                ],
                theme::space::MD,
            ))
            .into_any_element(),
    )
}

/// Welcome screen shown whenever no repository is open: a fresh launch with
/// nothing to restore (including from the macOS/Spotlight launcher, where cwd is
/// `/`), or after the last tab is closed. It is the open prompt itself, the same
/// card ⌘O raises over a tab, inline and focused, with the recent repositories,
/// under the wordmark, then a few tips. Only an explicit `--path` that failed to
/// resolve surfaces an error; the session and cwd fallbacks resolve silently to
/// this screen instead.
pub(crate) fn welcome(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    // The wordmark alone is the identity: big enough to anchor the page.
    let mut body = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(theme::space::XXL))
        .child(typography::line(
            "Diffui",
            emphasis_font(font, Weight::SEMIBOLD),
            text_size::DISPLAY,
            theme.text,
        ));
    // A `Failed` status can now only come from an explicit `--path` (the session
    // and cwd fallbacks resolve silently), so it's always worth surfacing.
    if let LoadStatus::Failed(error) = &ui.active().session.status {
        body = body.child(typography::line(
            format!("Couldn't open repository: {error}"),
            font,
            text_size::UI,
            theme.removed_text,
        ));
    }
    // `reconcile` keeps the prompt's state alive whenever no tab is open.
    if let Some(dialog) = ui.open_repo_dialog() {
        body = body.child(crate::tab_bar::open_repo_card(
            ui, theme, dialog, window, cx,
        ));
    }
    body = body.child(crate::shortcuts::welcome_hints(ui, theme, window, cx));

    // Optically centered: a touch above the true middle, where a centered block
    // reads as centered rather than as sinking.
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .pb(px(theme::space::XXL * 3.0))
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revset_failures_are_told_apart_from_repository_ones() {
        assert!(is_revset_error(
            "failed to parse revset:  --> 1:4\n  |\n1 | (((\n  |    ^---"
        ));
        assert!(is_revset_error(
            "failed to resolve: Revision `nope` doesn't exist"
        ));
        assert!(!is_revset_error(
            "another process holds the working-copy lock"
        ));
    }

    #[test]
    fn a_selection_the_list_does_not_hold_is_outside_the_view() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/repo");
        ui.tabs[0].state.session.status = LoadStatus::Loaded;
        ui.tabs[0].state.session.selected_revision = RevisionSelection::Commit("abc123".to_owned());
        assert!(selection_outside_view(&ui));
        // Still loading: the list is not the last word yet.
        ui.tabs[0].state.session.status = LoadStatus::Loading;
        assert!(!selection_outside_view(&ui));
    }
}
