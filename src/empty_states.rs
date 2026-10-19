//! What the revision list and the diff pane say when there is nothing (or not
//! the expected thing) to show: a revset that matches nothing, a revset that
//! doesn't evaluate, a load still running, and a diff pane holding on to a
//! revision the current view no longer lists.

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    font::Weight,
    widget::{Space, column, container, row, text},
};

use diffui_core::RevisionSelection;

use crate::prose::Prose;
use crate::theme::{self, ThemeSpec, chip_background, emphasis_font, text_size};
use crate::{Action, Diffui, LoadStatus, Message, icons, ui};

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

/// The revision list's stand-in while it has no rows: a load in flight, or a
/// revset that matched nothing. `None` when there are rows (or a failure,
/// which [`load_error`] reports above the list).
pub(crate) fn revision_list_placeholder<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    let tab = ui.active();
    if !tab.session.commits.is_empty() {
        return None;
    }
    let body: Element<'a, Message> = match &tab.session.status {
        LoadStatus::Failed(_) => return None,
        LoadStatus::Loading => text("Loading revisions\u{2026}")
            .size(text_size::UI)
            .color(theme.subtle_text)
            .font(ui.config.ui_font)
            .into(),
        LoadStatus::Loaded => {
            let revset = tab.session.revset.trim();
            let mut body = column![
                text("No revisions match")
                    .size(text_size::BODY)
                    .color(theme.text)
                    .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
                text(if revset.is_empty() { "(empty)" } else { revset })
                    .size(text_size::UI)
                    .color(theme.accent)
                    .font(ui.config.mono_font)
                    .wrapping(text::Wrapping::WordOrGlyph),
                text("Try another, or pick one from the presets menu on the filter.")
                    .size(text_size::CAPTION)
                    .color(theme.subtle_text)
                    .font(ui.config.ui_font),
            ]
            .spacing(theme::space::SM)
            .align_x(alignment::Horizontal::Center);
            if let Some(reset) = reset_button(ui, theme) {
                body = body.push(Space::new().height(Length::Fixed(theme::space::XS)));
                body = body.push(reset);
            }
            body.into()
        }
    };
    Some(
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Top)
            .padding(Padding {
                top: theme::space::XXL * 2.0,
                ..Padding::from(theme::space::XL)
            })
            .into(),
    )
}

/// "Reset to default" — the repo's own `revsets.log` (or jj's default) —
/// unless that is what is already applied.
fn reset_button<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Option<Element<'a, Message>> {
    let tab = ui.active();
    let default = tab.default_revset.clone();
    if tab.repository.is_none() || tab.session.revset.trim() == default.trim() {
        return None;
    }
    Some(
        ui::text_button(
            None,
            "Reset to default",
            ui::Variant::Raised,
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
        )
        .on_press(Message::Action(Action::SetRevset(default)))
        .into(),
    )
}

/// The alert card above the list when the graph load failed. A revset error
/// gets its own wording, the parser's message in mono (it points a caret at
/// the offending column), and a way back to the default.
pub(crate) fn load_error<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    error: &'a str,
) -> Element<'a, Message> {
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
    let mut body = column![
        row![
            icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.removed_text),
            text(title)
                .size(text_size::UI)
                .color(theme.removed_text)
                .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        ]
        .spacing(theme::space::SM)
        .align_y(alignment::Vertical::Center),
        text(detail)
            .size(text_size::CAPTION)
            .color(theme.text)
            .font(if revset {
                ui.config.mono_font
            } else {
                ui.config.ui_font
            })
            .wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(theme::space::SM);
    if revset && let Some(reset) = reset_button(ui, theme) {
        body = body.push(reset);
    }
    container(
        container(body)
            .width(Length::Fill)
            .padding([theme::space::MD, theme::space::LG])
            .style(move |_| container::Style {
                background: Some(Background::Color(chip_background(theme.removed_text))),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: theme::radius::CONTROL.into(),
                },
                ..container::Style::default()
            }),
    )
    .width(Length::Fill)
    .padding([6, 8])
    .into()
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

/// A strip over the diff saying so, when [`selection_outside_view`].
pub(crate) fn outside_view_banner<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    if !selection_outside_view(ui) {
        return None;
    }
    let label = match &ui.active().session.selected_revision {
        RevisionSelection::WorkingCopy => Prose::from("The working copy ").rev("@"),
        selection => ui.revision_prose(selection),
    };
    Some(
        container(
            row![
                icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.info),
                label
                    .text(" isn't in the current revset.")
                    .view(
                        ui.config.ui_font,
                        ui.config.mono_font,
                        theme.muted_text,
                        theme,
                    )
                    .size(text_size::UI)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End),
            ]
            .spacing(theme::space::MD)
            .align_y(alignment::Vertical::Center),
        )
        .width(Length::Fill)
        .padding([theme::space::SM, theme::space::LG])
        .style(move |_| container::Style::default().background(chip_background(theme.info)))
        .into(),
    )
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
