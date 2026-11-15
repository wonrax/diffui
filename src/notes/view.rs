//! The Notes view: every note in the repository, grouped by change, newest
//! activity first, filtered, and cleaned up in bulk. It takes the main area
//! the way the history view does.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ElementId, SharedString, Window, canvas, div, px};

use super::app::{Filter, Note, NotesMsg};
use super::inline::{age, lines_label};
use super::locate::Place;
use super::store::Role;
use crate::diff_view::CheckState;
use crate::theme::{self, Weight, text_size};
use crate::ui::{self, Look};
use crate::{Action, Cx, Diffui, TabState, icons, typography};

/// The notes the view lists: its filter, author and change applied, grouped
/// by change (the change with the latest activity first), newest first in
/// each.
pub(crate) fn listed(tab: &TabState) -> Vec<&Note> {
    let view = &tab.notes.view;
    let mut notes: Vec<&Note> = tab
        .notes
        .notes
        .iter()
        .filter(|note| view.filter.admits(note))
        .filter(|note| {
            view.author
                .as_deref()
                .is_none_or(|author| note.thread.agents().any(|agent| agent == author))
        })
        .filter(|note| {
            view.change
                .as_deref()
                .is_none_or(|change| note.thread.change_id == change)
        })
        .collect();
    let latest = |change: &str| {
        notes
            .iter()
            .filter(|note| note.thread.change_id == change)
            .map(|note| note.thread.activity())
            .max()
            .unwrap_or(0)
    };
    let mut keyed: Vec<(u64, &Note)> = notes
        .iter()
        .map(|note| (latest(&note.thread.change_id), *note))
        .collect();
    keyed.sort_by(|(a_latest, a), (b_latest, b)| {
        b_latest
            .cmp(a_latest)
            .then_with(|| a.thread.change_id.cmp(&b.thread.change_id))
            .then_with(|| b.thread.activity().cmp(&a.thread.activity()))
    });
    notes = keyed.into_iter().map(|(_, note)| note).collect();
    notes
}

/// Every agent that wrote a note, for the author filter.
fn authors(tab: &TabState) -> Vec<String> {
    let mut authors: Vec<String> = tab
        .notes
        .notes
        .iter()
        .flat_map(|note| note.thread.agents().map(str::to_owned))
        .collect();
    authors.sort();
    authors.dedup();
    authors
}

fn id(name: &str, part: &str) -> ElementId {
    ElementId::Name(SharedString::from(format!("notes-view-{name}-{part}")))
}

pub(crate) fn build_notes_panel(
    ui: &Diffui,
    look: Look,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let tab = ui.active();
    let view = &tab.notes.view;
    let on = std::rc::Rc::new(Diffui::listen(cx, |this, message: &NotesMsg, _, cx| {
        this.perform(Action::Notes(message.clone()), cx);
    }));
    let send = |message: NotesMsg| {
        let on = on.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| on(&message, window, cx)
    };

    // The filter, each with its count.
    let mut filters = ui::segmented(theme);
    for filter in Filter::ALL {
        let count = tab
            .notes
            .notes
            .iter()
            .filter(|note| filter.admits(note))
            .count();
        filters = filters.child(
            ui::segment(
                id(filter.label(), "filter"),
                ui::Segment {
                    icon: None,
                    label: filter.label().into(),
                    count: Some((count, true)),
                    active: view.filter == filter,
                    enabled: true,
                },
                ui::Segmented::Small,
                look,
            )
            .on_click(send(NotesMsg::Filter(filter))),
        );
    }
    let mut bar = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(theme::space::MD))
        .w_full()
        .px(px(theme::space::XL))
        .py(px(theme::space::MD_LG))
        .border_b_1()
        .border_color(theme.border)
        .child(filters);
    let authors = authors(tab);
    if authors.len() > 1 || view.author.is_some() {
        let mut by = ui::segmented(theme).child(
            ui::segment(
                "notes-view-author-all",
                ui::Segment {
                    icon: None,
                    label: "Everyone".into(),
                    count: None,
                    active: view.author.is_none(),
                    enabled: true,
                },
                ui::Segmented::Small,
                look,
            )
            .on_click(send(NotesMsg::Author(None))),
        );
        for author in authors {
            by = by.child(
                ui::segment(
                    id(&author, "author"),
                    ui::Segment {
                        icon: None,
                        label: author.clone().into(),
                        count: None,
                        active: view.author.as_deref() == Some(author.as_str()),
                        enabled: true,
                    },
                    ui::Segmented::Small,
                    look,
                )
                .on_click(send(NotesMsg::Author(Some(author)))),
            );
        }
        bar = bar.child(by);
    }
    if let Some(change) = &view.change {
        bar = bar.child(
            look.button("notes-view-change", format!("Only {}", short(change)))
                .icon(icons::CLOSE, theme.muted_text)
                .ghost()
                .small()
                .tip("Show every change's notes")
                .on_click(send(NotesMsg::AllChanges)),
        );
    }

    let listed = listed(tab);
    let selected: Vec<String> = listed
        .iter()
        .filter(|note| view.selected.contains(&note.thread.id))
        .map(|note| note.thread.id.clone())
        .collect();

    // The selection's actions, or the question before a delete.
    let actions = match &view.confirm_delete {
        Some(ids) => {
            let noun = if ids.len() == 1 { "note" } else { "notes" };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::MD))
                .w_full()
                .px(px(theme::space::XL))
                .py(px(theme::space::SM))
                .bg(theme.panel_background_elevated)
                .border_b_1()
                .border_color(theme.border)
                .child(
                    typography::line(
                        format!("Delete {} {noun}? This can't be undone.", ids.len()),
                        font,
                        text_size::BODY,
                        theme.text,
                    )
                    .flex_1(),
                )
                .child(
                    look.cancel_button("notes-view-delete-cancel")
                        .compact()
                        .on_click(send(NotesMsg::CancelDelete)),
                )
                .child(
                    look.button("notes-view-delete-confirm", format!("Delete {}", ids.len()))
                        .destructive()
                        .compact()
                        .on_click(send(NotesMsg::Delete(ids.clone()))),
                )
        }
        None => {
            let all = !listed.is_empty() && selected.len() == listed.len();
            let state = if all {
                CheckState::On
            } else if selected.is_empty() {
                CheckState::Off
            } else {
                CheckState::Partial
            };
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::MD))
                .w_full()
                .px(px(theme::space::XL))
                .py(px(theme::space::SM))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .id("notes-view-select-all")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(theme::space::SM))
                        .cursor_pointer()
                        .on_click(send(NotesMsg::SelectAll(!all)))
                        .child(checkbox(state, look))
                        .child(typography::line(
                            if selected.is_empty() {
                                format!("{} notes", listed.len())
                            } else {
                                format!("{} of {} selected", selected.len(), listed.len())
                            },
                            font,
                            text_size::CAPTION,
                            theme.muted_text,
                        )),
                )
                .child(div().flex_1());
            if !selected.is_empty() {
                row = row
                    .child(
                        look.button("notes-view-resolve", "Resolve")
                            .compact()
                            .on_click(send(NotesMsg::Resolve(selected.clone()))),
                    )
                    .child(
                        look.button("notes-view-delete", "Delete\u{2026}")
                            .icon(icons::TRASH, theme.removed_text)
                            .compact()
                            .on_click(send(NotesMsg::AskDelete(selected.clone()))),
                    );
            }
            row
        }
    };

    let mut list = div().flex().flex_col().w_full().pb(px(theme::space::XL));
    let mut change: Option<&str> = None;
    let now = super::store::now();
    for note in &listed {
        let thread = &note.thread;
        if change != Some(thread.change_id.as_str()) {
            change = Some(thread.change_id.as_str());
            list = list.child(group_header(note, look));
        }
        list = list.child(row(
            note,
            view.selected.contains(&thread.id),
            now,
            look,
            &on,
        ));
    }
    if listed.is_empty() {
        let message = match view.filter {
            Filter::Open => "No open notes. Agents leave them with diffui notes add.",
            Filter::WaitingOnYou => "Nothing waiting on you",
            Filter::WaitingOnAgent => "Nothing waiting on an agent",
            Filter::Resolved => "No resolved notes",
            Filter::Outdated => "No outdated notes",
            Filter::Archived => "No archived notes",
        };
        list = list.child(
            div()
                .w_full()
                .py(px(theme::space::XXL))
                .child(ui::empty_state(
                    message,
                    ui::Placement::Centered,
                    font,
                    theme.subtle_text,
                )),
        );
    }

    div()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.panel_background)
        .child(bar)
        .child(actions)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(ui::scrollable("notes-view-list", list)),
        )
        .into_any_element()
}

fn short(change: &str) -> &str {
    &change[..change.len().min(8)]
}

/// A change's header: its id and title.
fn group_header(note: &Note, look: Look) -> AnyElement {
    let Look { theme, font, mono } = look;
    let thread = &note.thread;
    let title = note
        .located
        .now
        .as_ref()
        .map_or(thread.title.as_str(), |now| now.title.as_str());
    let (title, ink) = if title.is_empty() {
        ("No description", theme.subtle_text)
    } else {
        (title, theme.text)
    };
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .w_full()
        .min_w_0()
        .px(px(theme::space::XL))
        .pt(px(theme::space::LG))
        .pb(px(theme::space::XS))
        .child(
            typography::line(
                short(&thread.change_id).to_owned(),
                mono,
                text_size::BODY,
                theme.change_id,
            )
            .flex_none(),
        )
        .child(
            typography::line(
                title.to_owned(),
                theme::emphasis_font(font, Weight::MEDIUM),
                text_size::BODY,
                ink,
            )
            .min_w_0()
            .text_ellipsis(),
        );
    if note.located.archived() {
        header = header.child(
            typography::line("abandoned", mono, text_size::BADGE, theme.subtle_text)
                .flex_none()
                .px(px(theme::space::XS))
                .rounded(px(theme::radius::CHIP))
                .bg(theme::chip_background(theme.subtle_text)),
        );
    }
    header.into_any_element()
}

/// One note: its box, where it is, who had the last word and what they said.
fn row(
    note: &Note,
    selected: bool,
    now: u64,
    look: Look,
    on: &std::rc::Rc<impl Fn(&NotesMsg, &mut Window, &mut App) + 'static>,
) -> AnyElement {
    let Look { theme, font, mono } = look;
    let thread = &note.thread;
    let where_ = match (&thread.anchor, &note.located.place) {
        (None, _) => "change".to_owned(),
        (Some(anchor), Place::Lines { start, end }) => lines_label(
            &anchor.path,
            *start,
            *end,
            anchor
                .lines
                .as_ref()
                .map_or(super::store::Side::New, |lines| lines.side),
        ),
        (Some(anchor), _) => anchor.path.clone(),
    };
    let last = thread.last();
    let who = last.map_or_else(String::new, |message| match message.role {
        Role::User => "You".to_owned(),
        Role::Agent => message.author.clone(),
    });
    let said = last.map_or("", |message| message.body.lines().next().unwrap_or(""));
    let state = if thread.resolved.is_some() {
        Some(("resolved", theme.subtle_text))
    } else if note.located.outdated() {
        Some(("outdated", theme.note_text))
    } else if thread.waiting_on_agent() {
        Some(("replied", theme.info))
    } else {
        None
    };
    let open = {
        let on = on.clone();
        let message = NotesMsg::Open(thread.id.clone());
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| on(&message, window, cx)
    };
    let toggle = {
        let on = on.clone();
        let message = NotesMsg::Select(thread.id.clone());
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            on(&message, window, cx)
        }
    };
    let mut line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .w_full()
        .min_w_0()
        .child(
            typography::line(where_, mono, text_size::CAPTION, theme.muted_text)
                .min_w_0()
                .text_ellipsis(),
        );
    if let Some((label, color)) = state {
        line = line.child(
            typography::line(label, mono, text_size::BADGE, color)
                .flex_none()
                .px(px(theme::space::XS))
                .rounded(px(theme::radius::CHIP))
                .bg(theme::chip_background(color)),
        );
    }
    line = line.child(div().flex_1()).child(
        typography::line(
            last.map_or_else(String::new, |message| age(message.at, now)),
            font,
            text_size::CAPTION,
            theme.subtle_text,
        )
        .flex_none(),
    );
    div()
        .id(ElementId::Name(SharedString::from(format!(
            "notes-view-row-{}",
            thread.id
        ))))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(theme::space::MD))
        .w_full()
        .px(px(theme::space::XL))
        .pr(px(crate::scrollbar::GUTTER + theme::space::MD))
        .py(px(theme::space::SM))
        .cursor_pointer()
        .when(selected, |row| row.bg(theme.selected_file))
        .hover(|row| row.bg(theme::chip_background(theme.muted_text)))
        .on_click(open)
        .child(
            div()
                .id(ElementId::Name(SharedString::from(format!(
                    "notes-view-check-{}",
                    thread.id
                ))))
                .flex_none()
                .pt(px(2.0))
                .on_click(toggle)
                .child(checkbox(
                    if selected {
                        CheckState::On
                    } else {
                        CheckState::Off
                    },
                    look,
                )),
        )
        .child(icons::icon(
            icons::MESSAGE_SQUARE,
            theme::icon::SM,
            if thread.resolved.is_some() {
                theme.subtle_text
            } else {
                theme.accent
            },
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(theme::space::XXS))
                .flex_1()
                .min_w_0()
                .child(line)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(theme::space::SM))
                        .min_w_0()
                        .child(
                            typography::line(
                                who,
                                theme::emphasis_font(font, Weight::SEMIBOLD),
                                text_size::BODY,
                                theme.text,
                            )
                            .flex_none(),
                        )
                        .child(
                            typography::line(
                                said.to_owned(),
                                font,
                                text_size::BODY,
                                theme.muted_text,
                            )
                            .min_w_0()
                            .text_ellipsis(),
                        ),
                ),
        )
        .into_any_element()
}

/// The change-selection checkbox, as the diff and the file list draw it.
fn checkbox(state: CheckState, look: Look) -> AnyElement {
    let Look { theme, .. } = look;
    const SIDE: f32 = 14.0;
    div()
        .size(px(SIDE))
        .flex_none()
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    let rect: crate::geometry::Rectangle = bounds.into();
                    crate::diff_view::paint_checkbox(
                        rect,
                        state,
                        theme.info,
                        theme.muted_text,
                        theme.panel_background,
                        rect,
                        window,
                        cx,
                    );
                },
            )
            .size_full(),
        )
        .into_any_element()
}
