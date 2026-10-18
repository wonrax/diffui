//! Comparing two revisions in the diff pane: `jj diff --from A --to B`, and
//! `jj interdiff --from A --to B` as a mode of it.
//!
//! The session owns the state ([`diffui_core::Session::diff_base`]): the
//! selected revision is always the "to" side and the base is sticky, so once
//! a comparison is on, clicking another revision in the graph re-picks the
//! "to" side against the same base. That is also how "Compare with…" picks
//! its other side — it pins the base and leaves the click to the graph. The
//! banner above the diff says what is being compared and carries the swap,
//! interdiff and exit controls.

use diffui_core::{DiffBase, RevisionSelection};
use iced::widget::{Space, container, row, text};
use iced::{Background, Border, Color, Element, Length, Task, alignment};

use crate::action::Action;
use crate::message::Message;
use crate::theme::{ThemeSpec, text_size};
use crate::{Diffui, icons, ui};

#[derive(Debug, Clone)]
pub(crate) enum CompareAction {
    /// Pin `from` as the base. The browsed revision is the other side, and
    /// any revision clicked next replaces it.
    PickWith(RevisionSelection),
    /// `from` → the working copy.
    WithWorkingCopy(RevisionSelection),
    /// The sidebar's two marked rows, older → newer.
    Marked,
    Swap,
    SetInterdiff(bool),
    Exit,
}

impl Diffui {
    pub(crate) fn perform_compare(&mut self, action: CompareAction) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let target = self.active_mut();
        if !target.session.capabilities.compare {
            return Task::none();
        }
        let interdiff = matches!(target.session.diff_base, DiffBase::Interdiff(_));
        let base_for = |from: RevisionSelection| {
            if interdiff {
                DiffBase::Interdiff(from)
            } else {
                DiffBase::Revision(from)
            }
        };
        let effects = match action {
            CompareAction::PickWith(from) => {
                let to = target.session.selected_revision.clone();
                target.session.compare(base_for(from), to)
            }
            CompareAction::WithWorkingCopy(from) => target
                .session
                .compare(base_for(from), RevisionSelection::WorkingCopy),
            CompareAction::Marked => {
                let Some((from, to)) = marked_pair(target) else {
                    return Task::none();
                };
                target.revision_multi_selection.clear();
                target.session.compare(base_for(from), to)
            }
            CompareAction::Swap => target.session.swap_compare(),
            CompareAction::SetInterdiff(on) => target.session.set_interdiff(on),
            CompareAction::Exit => target.session.exit_compare(),
        };
        self.run_effects(tab, effects)
    }
}

/// The two marked rows as `(older, newer)`. The graph lists newest first, so
/// the row further down is the base.
fn marked_pair(tab: &crate::TabState) -> Option<(RevisionSelection, RevisionSelection)> {
    let [a, b] = tab.revision_multi_selection.as_slice() else {
        return None;
    };
    let commits = &tab.session.commits;
    let index_of = |id: &str| commits.iter().position(|row| row.commit_id() == id);
    let (ia, ib) = (index_of(a)?, index_of(b)?);
    let selection = |index: usize| {
        let row = commits.row(index);
        if row.is_working_copy() {
            RevisionSelection::WorkingCopy
        } else {
            RevisionSelection::Commit(row.commit_id().to_owned())
        }
    };
    let (older, newer) = if ia > ib { (ia, ib) } else { (ib, ia) };
    Some((selection(older), selection(newer)))
}

/// Whether the palette's "Compare marked revisions" applies.
pub(crate) fn has_marked_pair(ui: &Diffui) -> bool {
    ui.active().session.capabilities.compare && marked_pair(ui.active()).is_some()
}

/// Whether a comparison is on (requested or on screen).
pub(crate) fn comparing(ui: &Diffui) -> bool {
    let session = &ui.active().session;
    session.diff_base.is_compare() || session.document_base.is_compare()
}

/// `k1x2m3n4 · first line of the description`, or `@` for the working copy.
fn revision_label(ui: &Diffui, revision: &RevisionSelection) -> (String, String) {
    let commits = &ui.active().session.commits;
    let row = match revision {
        RevisionSelection::WorkingCopy => commits.working_copy(),
        RevisionSelection::Commit(hex) => commits.find_by_commit_id(hex),
    };
    let Some(row) = row else {
        let id = match revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(hex) => hex.chars().take(8).collect(),
        };
        return (id, String::new());
    };
    let mut id: String = row.change_id().chars().take(8).collect();
    if row.is_working_copy() {
        id = format!("@ {id}");
    }
    let title = row
        .description()
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("(no description)")
        .to_owned();
    (id, title)
}

/// The "Comparing A → B" strip atop the diff pane, or `None` when the pane
/// shows a plain revision diff.
pub(crate) fn build_compare_banner<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    let session = &ui.active().session;
    // While a switch is in flight the requested base is what's coming; once it
    // lands the two agree. Showing the requested one makes Exit/Swap respond
    // at once instead of on the next load.
    let base = if session.diff_in_flight() {
        &session.diff_base
    } else {
        &session.document_base
    };
    let from = base.revision()?;
    let to = session.diff_pending().unwrap_or(&session.selected_revision);
    let interdiff = matches!(base, DiffBase::Interdiff(_));
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;

    let (from_id, from_title) = revision_label(ui, from);
    let (to_id, to_title) = revision_label(ui, to);
    let id = |id: String| {
        text(id)
            .size(text_size::UI)
            .font(mono)
            .color(theme.accent)
            .wrapping(text::Wrapping::None)
    };
    let label = if interdiff { "Interdiff" } else { "Comparing" };
    let headline = row![
        icons::icon(icons::GIT_COMPARE, ui::ICON_SIZE, theme.accent),
        text(label)
            .size(text_size::UI)
            .font(crate::theme::emphasis_font(
                font,
                iced::font::Weight::Medium
            ))
            .color(theme.text)
            .wrapping(text::Wrapping::None),
        id(from_id),
        icons::icon(icons::ARROW_RIGHT, ui::ICON_SIZE, theme.muted_text),
        id(to_id),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);
    // The titles ride on a quieter second line so the ids and controls never
    // get clipped by a narrow pane; a revision compared with itself turns the
    // line into the hint for picking the other side.
    let subline = if from == to {
        "Click another revision in the graph to compare against this one".to_owned()
    } else {
        format!("{from_title}  →  {to_title}")
    };
    let sides = iced::widget::column![
        headline,
        text(subline)
            .size(text_size::CAPTION)
            .font(font)
            .color(theme.subtle_text)
            .wrapping(text::Wrapping::None),
    ]
    .spacing(3);

    let action = |action: CompareAction| Message::Action(Action::Compare(action));
    let controls = row![
        ui::tooltip(
            ui::icon_button(
                icons::ARROW_LEFT_RIGHT,
                theme.muted_text,
                ui::Size::Compact,
                theme
            )
            .on_press(action(CompareAction::Swap)),
            "Swap the two sides",
            iced::widget::tooltip::Position::Bottom,
            font,
            theme,
        ),
        ui::tooltip(
            ui::text_button(
                None,
                if interdiff { "Tree diff" } else { "Interdiff" },
                ui::Variant::Ghost,
                ui::Size::Compact,
                font,
                theme,
            )
            .on_press(action(CompareAction::SetInterdiff(!interdiff))),
            if interdiff {
                "Diff the two trees (jj diff --from)"
            } else {
                "Diff the two versions of the change itself (jj interdiff)"
            },
            iced::widget::tooltip::Position::Bottom,
            font,
            theme,
        ),
        ui::tooltip(
            ui::icon_button(icons::CLOSE, theme.muted_text, ui::Size::Compact, theme)
                .on_press(action(CompareAction::Exit)),
            "Stop comparing",
            iced::widget::tooltip::Position::Left,
            font,
            theme,
        ),
    ]
    .spacing(4)
    .align_y(alignment::Vertical::Center);

    let strip = row![
        container(sides).width(Length::Fill).clip(true),
        Space::new().width(Length::Fixed(8.0)),
        controls,
    ]
    .align_y(alignment::Vertical::Center);

    Some(
        container(strip)
            .width(Length::Fill)
            .padding([8.0, crate::theme::space::XL])
            .style(move |_| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.10,
                    ..theme.accent
                })),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into(),
    )
}

impl Diffui {
    /// Fetch the full diff of a file the backend collapsed for size.
    pub(crate) fn expand_collapsed_file(&mut self, index: usize) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let target = self.active_mut();
        let Some(path) = target
            .session
            .document
            .files
            .get(index)
            .map(|file| file.path.clone())
        else {
            return Task::none();
        };
        let effects = target.session.expand_file(&path);
        self.run_effects(tab, effects)
    }
}
