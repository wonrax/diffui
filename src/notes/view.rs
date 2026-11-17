//! The Notes view, built like History: the left pane becomes the list of
//! notes (grouped under their change, filtered from its head), the main area
//! shows the selected thread in context, and picks act through a floating bar
//! like the diff's picks.

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ElementId, SharedString, Window, canvas, div, px};

use super::app::{Filter, Note, NotesMsg};
use super::inline::{age, lines_label, plain_line};
use super::locate::Place;
use super::store::Role;
use crate::diff_view::CheckState;
use crate::history_panel::{
    DETAIL_EDGE, ROW_CONTENT_LEFT, caption_on_line, fill_on_line, header_bar, list_column,
    list_row, note, two_line,
};
use crate::overlay_kit::GROUP_HEIGHT;
use crate::revision_list::{
    AGE_TEXT_SIZE, CAPTION_TEXT_SIZE, REVISION_ROW_HEIGHT, ROW_CARD_INSET_Y, SMALL_TEXT_SIZE,
};
use crate::theme::{self, Color, Weight, space, text_size};
use crate::typography::{self, Aligned};
use crate::ui::{self, Look};
use crate::{Action, Cx, Diffui, TabState, icons, meta_line};

/// The notes the view lists: its filter, author and change applied, grouped
/// by change (the change with the latest activity first), newest first in
/// each.
pub(crate) fn listed(tab: &TabState) -> Vec<&Note> {
    let view = &tab.notes.view;
    let notes: Vec<&Note> = tab
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
                .is_none_or(|change| note.thread.change_id() == change)
        })
        .collect();
    let latest = |change: &str| {
        notes
            .iter()
            .filter(|note| note.thread.change_id() == change)
            .map(|note| note.thread.activity())
            .max()
            .unwrap_or(0)
    };
    let mut keyed: Vec<(u64, &Note)> = notes
        .iter()
        .map(|note| (latest(note.thread.change_id()), *note))
        .collect();
    keyed.sort_by(|(a_latest, a), (b_latest, b)| {
        b_latest
            .cmp(a_latest)
            .then_with(|| a.thread.change_id().cmp(b.thread.change_id()))
            // Within a change, the order its stack reads in.
            .then_with(|| super::app::document_order(a).cmp(&super::app::document_order(b)))
    });
    keyed.into_iter().map(|(_, note)| note).collect()
}

/// A row's checkbox, and the slot it keeps at the row's end (its gap
/// before it): a group header keeps the same slot, so its count lines up
/// with its rows' ages.
const CHECK_SIDE: f32 = 14.0;
const CHECK_SLOT: f32 = space::SM + CHECK_SIDE;
/// Between one change's group and the next: History's section spacing, a
/// hairline in its middle.
const GROUP_GAP: f32 = space::LG;

fn id(name: &str, part: &str) -> ElementId {
    ElementId::Name(SharedString::from(format!("notes-view-{name}-{part}")))
}

/// A change's title as the log shows it: `(no description set)` in the
/// log's colour for a change without one.
fn change_title(note: &Note, theme: theme::ThemeSpec) -> (String, Color) {
    let title = note
        .located
        .now
        .as_ref()
        .map_or(note.thread.title(), |now| now.title.as_str());
    if title.is_empty() {
        ("(no description set)".to_owned(), theme.note_text)
    } else {
        (title.to_owned(), theme.text)
    }
}

fn short(change: &str) -> &str {
    &change[..change.len().min(8)]
}

type Send = std::rc::Rc<dyn Fn(&NotesMsg, &mut Window, &mut App)>;

/// A listener for a notes message.
fn sender(cx: &mut Cx) -> Send {
    std::rc::Rc::new(Diffui::listen(cx, |this, message: &NotesMsg, _, cx| {
        this.perform(Action::Notes(message.clone()), cx);
    }))
}

fn click(
    on: &Send,
    message: NotesMsg,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static {
    let on = on.clone();
    move |_, window, cx| on(&message, window, cx)
}

/// The left pane while the Notes view is open: its head with the filter,
/// then the notes under their changes.
pub(crate) fn build_notes_sidebar(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let tab = ui.active();
    let view = &tab.notes.view;
    let on = sender(cx);

    // One filter control filling the head, the "…" keeping its own room:
    // the three that matter, short labels so they don't need cutting at the
    // pane's usual widths, counts quiet at zero.
    let mut filters = ui::segmented_in(theme, ui::SegmentFit::Fill)
        .id("notes-view-filter-track")
        .test_support();
    for filter in Filter::SHOWN {
        let count = tab
            .notes
            .notes
            .iter()
            .filter(|note| filter.admits(note))
            .count();
        let mut segment = ui::segment_in(
            id(filter.name(), "filter"),
            ui::Segment {
                icon: None,
                label: filter.short_label().into(),
                count: Some((count, count > 0)),
                active: view.filter == filter,
                enabled: true,
            },
            ui::Segmented::Small,
            ui::SegmentFit::Fill,
            look,
        )
        .on_click(click(&on, NotesMsg::Filter(filter)));
        // The short label's full name.
        if filter.short_label() != filter.label() {
            segment = segment.tip(filter.label());
        }
        filters = filters.child(segment);
    }
    let more = look
        .icon_button("notes-view-filter-menu", icons::ELLIPSIS, theme.muted_text)
        .tip("More filters")
        .on_click(click(&on, NotesMsg::FilterMenu));
    let head = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM))
        .child(div().flex_1().min_w_0().flex().child(filters))
        .child(div().flex_none().child(more));

    let mut pane = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
        // No rule under the head: the list fades under it once scrolled,
        // as History's does.
        .child(header_bar(head, false, theme));

    // What the head's control doesn't say: a filter from the menu, an
    // author, a change. Each clears with its own ✕.
    let mut applied: Vec<(String, NotesMsg)> = Vec::new();
    if !Filter::SHOWN.contains(&view.filter) {
        applied.push((
            view.filter.label().to_owned(),
            NotesMsg::Filter(Filter::WaitingOnYou),
        ));
    }
    if let Some(author) = &view.author {
        applied.push((
            format!("By {author}"),
            NotesMsg::FilterNamed("everyone".to_owned()),
        ));
    }
    if let Some(change) = &view.change {
        applied.push((format!("On {}", short(change)), NotesMsg::AllChanges));
    }
    if !applied.is_empty() {
        let mut strip = div()
            .w_full()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(space::XS))
            .pl(px(DETAIL_EDGE))
            .pr(px(crate::scrollbar::GUTTER))
            .pb(px(space::SM));
        for (index, (label, clear)) in applied.into_iter().enumerate() {
            strip = strip.child(
                look.button(id(&index.to_string(), "applied"), label)
                    .icon(icons::CLOSE, theme.muted_text)
                    .ghost()
                    .small()
                    .on_click(click(&on, clear)),
            );
        }
        pane = pane.child(strip);
    }

    let listed = listed(tab);
    if listed.is_empty() {
        let message = if tab.notes.notes.is_empty() {
            "No notes yet"
        } else {
            match view.filter {
                Filter::WaitingOnYou => "Nothing waiting on you",
                Filter::WaitingOnAgent => "Nothing waiting on an agent",
                Filter::Resolved => "No resolved notes",
                Filter::Outdated => "No outdated notes",
                Filter::Archived => "No archived notes",
                Filter::Open => "No open notes",
            }
        };
        return pane
            .child(
                div()
                    .w_full()
                    .pt(px(space::XL))
                    .child(note(message, theme.subtle_text, ui)),
            )
            .into_any_element();
    }

    let picking = !view.selected.is_empty();
    let now = super::store::now();
    // Every row and group header has a fixed height, so where each group
    // starts is known here: the group the list has scrolled into keeps its
    // header pinned at the top.
    let handle = ui::scroll_handle("notes-view-list-scroll", window, cx);
    let scrolled = -f32::from(handle.read(cx).offset().y);
    let mut list = div().w_full().flex().flex_col().pb(px(space::XXL * 3.0));
    let mut groups: Vec<(f32, &Note)> = Vec::new();
    let mut y = 0.0;
    let mut change: Option<&str> = None;
    for note in &listed {
        let thread = &note.thread;
        if change != Some(thread.change_id()) {
            if change.is_some() {
                list = list.child(group_break(theme));
                y += GROUP_GAP;
            }
            change = Some(thread.change_id());
            groups.push((y, note));
            list = list.child(group_header(note, &listed, ui, look, &on));
            y += GROUP_HEIGHT;
        }
        let picked = view.selected.contains(&thread.id);
        let selected = view.thread.as_deref() == Some(thread.id.as_str());
        list = list.child(list_column().py(px(ROW_CARD_INSET_Y)).child(row(
            note,
            (selected, picked, picking),
            now,
            ui,
            look,
            &on,
            cx,
        )));
        y += REVISION_ROW_HEIGHT;
    }
    let pinned = groups
        .iter()
        .rev()
        .find(|(top, _)| *top < scrolled)
        .map(|(_, note)| *note);
    let handle = handle.read(cx).clone();
    pane.child(
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(ui::scroll_list(
                "notes-view-list",
                &handle,
                ui::ScrollList {
                    fade: Some(theme.panel_background),
                    ..Default::default()
                },
                list,
            ))
            // The pinned header stands where the list's top is, so the fade
            // the list casts once scrolled hangs under it instead.
            .children(pinned.map(|note| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right(px(crate::scrollbar::GUTTER))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .bg(theme.panel_background)
                            .child(group_header(note, &listed, ui, look, &on)),
                    )
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(theme::list::TOP_FADE))
                            .child(ui::top_fade(theme.panel_background)),
                    )
            })),
    )
    .into_any_element()
}

/// A change's section over its notes, as a list's group band is drawn: its
/// id styled as an id, its description (or the log's no-description words)
/// in the section label's weight, ellipsised, and its open notes' count at
/// the end. Its text starts where its rows' does.
/// The thread in focus in the stack: its anchored rows marked as an open
/// note's are in the diff, the excerpt's context left alone.
pub(crate) fn focused_highlight(
    view: &super::app::ViewState,
) -> Option<crate::diff_view::Highlight> {
    let file = view
        .thread
        .as_deref()
        .and_then(|id| view.stack.iter().position(|item| item.id == id))?;
    let item = &view.stack[file];
    let end = item.hunks.first()?.lines.len().checked_sub(1)?;
    let (first, last) = item.anchored?;
    let at = |line: usize| crate::diff_view::LineAt {
        file,
        hunk: 0,
        line: line.min(end),
    };
    Some(crate::diff_view::Highlight {
        from: at(first),
        to: at(last),
        strong: false,
    })
}

/// The break between two changes' groups: History's section spacing with a
/// hairline across the list in its middle.
fn group_break(theme: theme::ThemeSpec) -> AnyElement {
    div()
        .w_full()
        .h(px(GROUP_GAP))
        .flex_none()
        .flex()
        .items_center()
        .px(px(crate::revision_list::ROW_CARD_INSET_X))
        .child(div().w_full().h(px(1.0)).bg(theme.border))
        .into_any_element()
}

fn group_header(note: &Note, listed: &[&Note], ui: &Diffui, look: Look, on: &Send) -> AnyElement {
    let Look { theme, font, mono } = look;
    let thread = &note.thread;
    let (title, ink) = change_title(note, theme);
    let label = theme::emphasis_font(font, Weight::SEMIBOLD);
    let open = listed
        .iter()
        .filter(|other| other.thread.change_id() == thread.change_id() && other.counts())
        .count();
    let mut line = vec![Aligned::text(
        typography::line(
            short(thread.change_id()).to_owned(),
            mono,
            text_size::BODY,
            theme.change_id,
        )
        .flex_none(),
        mono,
        text_size::BODY,
    )];
    if note.located.archived() {
        line.push(ui::tag("Abandoned", theme.subtle_text, ui::TagSize::Small, font).aligned());
    }
    line.push(fill_on_line(
        typography::line(
            title,
            label,
            text_size::BODY,
            if ink == theme.text {
                theme.muted_text
            } else {
                ink
            },
        )
        .text_ellipsis(),
        label,
        text_size::BODY,
    ));
    if open > 0 {
        line.push(Aligned::text(
            div()
                .id(id(thread.change_id(), "count"))
                .test_support()
                .child(typography::line(
                    open.to_string(),
                    font,
                    text_size::BODY,
                    theme.subtle_text,
                )),
            font,
            text_size::BODY,
        ));
    }
    let _ = ui;
    div()
        .id(id(thread.change_id(), "group"))
        .test_support()
        .cursor_pointer()
        .on_click(click(
            on,
            NotesMsg::SelectChange(thread.change_id().to_owned()),
        ))
        .w_full()
        .h(px(GROUP_HEIGHT))
        .flex_none()
        .flex()
        .flex_col()
        .justify_end()
        .pb(px(space::XS))
        .pl(px(ROW_CONTENT_LEFT))
        .pr(px(theme::list::PAD_END + CHECK_SLOT))
        .bg(theme.panel_background)
        .child(typography::baseline_row(line, space::SM).min_w_0())
        .into_any_element()
}

/// Where a note is, quietly, for its row and its header: `path:12–18` (or
/// the file's path) in mono at the size rows give ids and paths, or "Whole
/// change" in prose at that size.
fn location(note: &Note, look: Look, ink: Color) -> Aligned {
    let Look { font, mono, .. } = look;
    let size = CAPTION_TEXT_SIZE;
    match &note.thread.anchor {
        None => Aligned::text(
            typography::line("Whole change", font, size, ink).text_ellipsis(),
            font,
            size,
        ),
        Some(anchor) => {
            let text = match (&note.located.place, &anchor.lines) {
                (Place::Lines { start, end }, Some(lines)) => {
                    lines_label(&anchor.path, *start, *end, lines.side)
                }
                (_, Some(lines)) => lines_label(&anchor.path, lines.start, lines.end(), lines.side),
                (_, None) => anchor.path.clone(),
            };
            Aligned::text(
                typography::line(text, mono, size, ink).text_ellipsis(),
                mono,
                size,
            )
        }
    }
}

/// One note, as History's operation rows are laid out: the latest message's
/// first line, the age at its end; under it, quieter, where it is, who wrote
/// last and how many replies. Every row the same shape. Its box sits at the
/// row's end, as the file list's does, on hover or for every row once
/// picking.
fn row(
    note: &Note,
    (selected, picked, picking): (bool, bool, bool),
    now: u64,
    ui: &Diffui,
    look: Look,
    on: &Send,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let thread = &note.thread;
    let dim = thread.resolved.is_some();
    let latest = thread.last();
    let title = vec![
        fill_on_line(
            typography::line(
                latest.map_or(String::new(), |message| plain_line(&message.body)),
                font,
                SMALL_TEXT_SIZE,
                if dim { theme.muted_text } else { theme.text },
            )
            .text_ellipsis(),
            font,
            SMALL_TEXT_SIZE,
        ),
        Aligned::text(
            div()
                .id(id(&thread.id, "age"))
                .test_support()
                .child(typography::line(
                    latest.map_or(String::new(), |message| age(message.at, now)),
                    font,
                    AGE_TEXT_SIZE,
                    theme.subtle_text,
                )),
            font,
            AGE_TEXT_SIZE,
        ),
    ];
    let author = latest.map_or(String::new(), |message| match message.role {
        Role::User => "You".to_owned(),
        Role::Agent => message.author.clone(),
    });
    let mut meta = vec![location(note, look, theme.muted_text).shrink()];
    if note.located.outdated() {
        meta.push(ui::tag("Outdated", theme.note_text, ui::TagSize::Small, font).aligned());
    }
    meta.push(caption_on_line(
        crate::history_panel::caption(author, theme.subtle_text, ui),
        ui,
    ));
    let replies = thread.messages.len().saturating_sub(1);
    if replies > 0 {
        let noun = if replies == 1 { "reply" } else { "replies" };
        meta.push(caption_on_line(
            crate::history_panel::caption(format!("{replies} {noun}"), theme.subtle_text, ui),
            ui,
        ));
    }
    let group = SharedString::from(format!("notes-view-row-{}", thread.id));
    let toggle = {
        let on = on.clone();
        let message = NotesMsg::Select(thread.id.clone());
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            on(&message, window, cx)
        }
    };
    let check = div()
        .id(id(&thread.id, "check"))
        .flex_none()
        .ml(px(CHECK_SLOT - CHECK_SIDE))
        .on_click(toggle)
        .child(checkbox(
            if picked {
                CheckState::On
            } else {
                CheckState::Off
            },
            look,
        ))
        .when(!picking, |check| {
            check
                .opacity(0.0)
                .group_hover(group.clone(), |style| style.opacity(1.0))
        });
    list_row(
        group.clone(),
        REVISION_ROW_HEIGHT,
        selected,
        Action::Notes(NotesMsg::SelectThread(thread.id.clone())),
        theme,
        cx,
    )
    .group(group)
    .child(two_line(
        typography::baseline_row(title, space::MD).min_w_0(),
        typography::baseline_row(meta, space::SM).min_w_0(),
    ))
    .child(check)
    .into_any_element()
}

/// The main area while the Notes view is open: the shown change's threads
/// stacked in one scrolling diff, and the picks' bar.
pub(crate) fn build_notes_panel(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let tab = ui.active();
    let view = &tab.notes.view;
    let mut panel = div()
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.panel_background);
    let notes = super::app::stack_notes(tab);
    panel = match notes.first() {
        Some(first) => panel
            .child(stack_header(first, notes.len(), ui, look, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(stack(&notes, ui, look, window, cx)),
            ),
        None if tab.notes.notes.is_empty() => panel.child(empty(
            "No review notes yet",
            "Agents leave them with diffui notes add. Start one with the + on a line of a diff.",
            look,
        )),
        None if view.shown.is_some() => panel.child(empty(
            "Nothing here under this filter",
            "Pick another filter, or show all of the change's notes.",
            look,
        )),
        None => panel.child(empty(
            "Select a change or a note",
            "Its notes show here, one after another.",
            look,
        )),
    };
    panel.child(action_bar(ui, look, cx)).into_any_element()
}

/// An empty state as the revision list's: a title and a caption under it.
fn empty(title: &'static str, detail: &'static str, look: Look) -> AnyElement {
    let Look { theme, font, .. } = look;
    div()
        .flex_1()
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            ui::empty_state(
                title,
                ui::Placement::Top,
                theme::emphasis_font(font, Weight::MEDIUM),
                theme.text,
            )
            .child(
                div()
                    .w_full()
                    .text_center()
                    .font(font.gpui())
                    .text_size(px(text_size::CAPTION))
                    .text_color(theme.subtle_text)
                    .child(detail),
            ),
        )
        .into_any_element()
}

/// The stack's header, naming the change once (its id and description as
/// the log draws them) with Open in diff, and how many of its notes show:
/// "3 of 5 notes, filtered" with a way to show them all.
fn stack_header(note: &Note, shown: usize, ui: &Diffui, look: Look, cx: &mut Cx) -> AnyElement {
    let Look { theme, font, .. } = look;
    let thread = &note.thread;
    let on = sender(cx);
    let tab = ui.active();
    let view = &tab.notes.view;
    let (title, ink) = change_title(note, theme);
    let total = tab
        .notes
        .notes
        .iter()
        .filter(|other| other.thread.change_id() == thread.change_id())
        .count();
    let open = look
        .button("notes-view-open", "Open in diff")
        .compact()
        .icon(icons::FILE_DIFF, theme.muted_text)
        .disabled(note.located.archived())
        .tip("Show the change's diff, at this note")
        .on_click(click(
            &on,
            NotesMsg::Open(view.thread.clone().unwrap_or_else(|| thread.id.clone())),
        ));
    let mut change = vec![
        meta_line::id_text(
            meta_line::Id::plain(short(thread.change_id()), theme.change_id),
            ui,
        ),
        fill_on_line(
            typography::line(title, font, SMALL_TEXT_SIZE, ink).text_ellipsis(),
            font,
            SMALL_TEXT_SIZE,
        ),
    ];
    if note.located.archived() {
        change.insert(
            1,
            ui::tag("Abandoned", theme.subtle_text, ui::TagSize::Small, font).aligned(),
        );
    }
    let noun = |count: usize| if count == 1 { "note" } else { "notes" };
    let (count, toggle) = if shown < total {
        (
            format!("{shown} of {total} {}, filtered", noun(total)),
            Some(("Show all", NotesMsg::AllInChange(true))),
        )
    } else if view.all_in_change {
        (
            format!("{total} {}", noun(total)),
            Some(("Filter them", NotesMsg::AllInChange(false))),
        )
    } else {
        (format!("{total} {}", noun(total)), None)
    };
    let mut second = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM))
        .child(typography::centered(
            typography::line(count, font, text_size::CAPTION, theme.subtle_text),
            font,
            text_size::CAPTION,
        ));
    if let Some((label, message)) = toggle {
        second = second.child(
            look.button("notes-view-all", label)
                .ghost()
                .small()
                .on_click(click(&on, message)),
        );
    }
    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::XS))
        .px(px(DETAIL_EDGE))
        .pt(px(space::LG))
        .pb(px(space::MD))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SM))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(typography::baseline_row(change, space::SM).min_w_0()),
                )
                .child(open),
        )
        .child(second)
        .into_any_element()
}

/// The stack itself: one diff view, a "file" per thread holding its excerpt
/// (or, for a note without lines, a line saying what it's on), its card laid
/// under its last row. Drawn lazily, as the diff draws its files; scrolling
/// past a thread moves the list's selection.
fn stack(notes: &[&Note], ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    let view = &ui.active().notes.view;
    let files: Vec<crate::diff_view::DiffFileView> = view
        .stack
        .iter()
        .map(|item| {
            let status_color = if item.title == "Whole change" {
                theme.subtle_text
            } else {
                theme::file_status_color(item.status, theme)
            };
            crate::diff_view::DiffFileView {
                title: item.title.clone(),
                status: item.status,
                status_color,
                status_fill: theme::chip_background(status_color),
                additions: item.additions,
                deletions: item.deletions,
                tag: None,
                // The header names it ("Whole change", the file); the card
                // under it is the body.
                note: None,
                // The change's own notes are a section, not a file.
                section: item.title == "Whole change",
            }
        })
        .collect();
    let insets = notes
        .iter()
        .enumerate()
        .filter_map(|(file, note)| {
            let item = view.stack.get(file)?;
            let at = match item.hunks.first() {
                Some(hunk) if !hunk.lines.is_empty() => {
                    crate::diff_view::DocSpot::Line(crate::diff_view::LineAt {
                        file,
                        hunk: 0,
                        line: hunk.lines.len() - 1,
                    })
                }
                _ => crate::diff_view::DocSpot::File(file),
            };
            Some(crate::notes::inline::stack_inset(ui, look, note, at, cx))
        })
        .collect();
    // The thread in focus: its rows marked as an open note's are.
    let focused = focused_highlight(view).into_iter().collect();
    let selected = view
        .thread
        .as_deref()
        .and_then(|id| view.stack.iter().position(|item| item.id == id))
        .unwrap_or(0);
    let mut props = crate::diff_view::DiffProps::new(
        files,
        crate::diff_view::DocSource::new(cx.entity(), |ui: &Diffui, file| {
            ui.active()
                .notes
                .view
                .stack
                .get(file)
                .map_or(&[][..], |item| &item.hunks)
        }),
        selected,
        format!("notes-stack-{}", view.shown.as_deref().unwrap_or_default()),
        crate::theme::diff_palette(theme),
        look.mono,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .ui_font(look.font)
    .wrap(ui.diff_wrap)
    .content_version(ui.document_version)
    .layout_version(view.stack_version)
    .insets(insets)
    .highlights(focused)
    .on_selected_file_changed(Diffui::listen(cx, |this, index: &usize, _, cx| {
        this.perform(Action::Notes(NotesMsg::StackScrolled(*index)), cx);
    }));
    // The last card keeps the room History's detail leaves after its last
    // item.
    props = props.end_space(space::LG);
    if let Some((spot, token)) = view.reveal {
        props = props.reveal(spot, token).reveal_at_top();
    }
    crate::diff_view::view("notes-stack", props, window, cx)
}

/// The picks' floating bar, as the diff's picks have it: what's picked, then
/// Resolve, Copy and Delete; Delete asks first, in the bar.
fn action_bar(ui: &Diffui, look: Look, cx: &mut Cx) -> AnyElement {
    let Look { theme, font, .. } = look;
    let view = &ui.active().notes.view;
    let picked: Vec<String> = listed(ui.active())
        .into_iter()
        .filter(|note| view.selected.contains(&note.thread.id))
        .map(|note| note.thread.id.clone())
        .collect();
    if picked.is_empty() && view.confirm_delete.is_none() {
        return div().into_any_element();
    }
    let on = sender(cx);
    let bold = theme::emphasis_font(font, Weight::SEMIBOLD);
    let ink = theme.panel_background;
    let faded = Color { a: 0.7, ..ink };
    let hover = Color {
        r: 0.5,
        g: 0.5,
        b: 0.5,
        a: 0.24,
    };
    let noun = |count: usize| if count == 1 { "note" } else { "notes" };
    let (title, detail) = match &view.confirm_delete {
        Some(ids) => (
            format!("Delete {} {}?", ids.len(), noun(ids.len())),
            "This can't be undone".to_owned(),
        ),
        None => (
            "Picked".to_owned(),
            format!("{} {}", picked.len(), noun(picked.len())),
        ),
    };
    let label = typography::baseline_row(
        vec![
            Aligned::text(
                typography::line(title, bold, text_size::BODY, ink),
                bold,
                text_size::BODY,
            ),
            Aligned::gap(space::SM),
            Aligned::text(
                typography::line(detail, font, text_size::BODY, faded),
                font,
                text_size::BODY,
            ),
        ],
        0.0,
    );
    let button = |name: &'static str,
                  icon: Option<&'static str>,
                  text: &'static str,
                  message: NotesMsg| {
        let mut content = div().flex().flex_row().items_center().gap(px(6.0));
        if let Some(icon) = icon {
            content = content.child(icons::icon(icon, ui::ICON_SIZE, ink));
        }
        content = content.child(typography::centered(
            typography::line(text, font, text_size::BODY, ink),
            font,
            text_size::BODY,
        ));
        div()
            .id(name)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(30.0))
            .px(px(9.0))
            .child(crate::squircle::hover_background(
                crate::squircle::Fill::new(crate::squircle::Corners::all(theme::radius::CONTROL))
                    .background(hover),
                0.0,
            ))
            .child(content)
            .cursor_pointer()
            .on_click(click(&on, message))
            .into_any_element()
    };
    let separator = || {
        div()
            .flex_none()
            .w(px(1.0))
            .h(px(18.0))
            .mx(px(4.0))
            .bg(Color { a: 0.18, ..ink })
            .into_any_element()
    };
    let mut items = vec![
        div()
            .flex_none()
            .pl(px(8.0))
            .pr(px(10.0))
            .child(label)
            .into_any_element(),
        separator(),
    ];
    match &view.confirm_delete {
        Some(ids) => {
            items.push(button(
                "notes-bar-cancel",
                None,
                "Cancel",
                NotesMsg::CancelDelete,
            ));
            items.push(button(
                "notes-bar-delete-confirm",
                Some(icons::TRASH),
                "Delete",
                NotesMsg::Delete(ids.clone()),
            ));
        }
        None => {
            items.push(button(
                "notes-bar-resolve",
                Some(icons::CHECK),
                "Resolve",
                NotesMsg::Resolve(picked.clone()),
            ));
            items.push(button(
                "notes-bar-copy",
                Some(icons::COPY),
                "Copy",
                NotesMsg::Copy(picked.clone()),
            ));
            items.push(button(
                "notes-bar-delete",
                Some(icons::TRASH),
                "Delete\u{2026}",
                NotesMsg::AskDelete(picked.clone()),
            ));
            items.push(separator());
            items.push(button(
                "notes-bar-clear",
                None,
                "Clear",
                NotesMsg::SelectAll(false),
            ));
        }
    }
    let bar = div()
        .id("notes-bar")
        .occlude()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .p(px(5.0))
        .max_w_full()
        .overflow_hidden()
        .map(|bar| theme::squircle(bar, theme::radius::LG, Some(theme.text), None))
        .shadow(vec![theme::Elevation::Popover.shadow(theme)])
        .children(items);
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom(px(40.0))
        .px(px(16.0))
        .flex()
        .justify_center()
        .child(bar)
        .into_any_element()
}

/// The change-selection checkbox, as the diff and the file list draw it.
fn checkbox(state: CheckState, look: Look) -> AnyElement {
    let Look { theme, .. } = look;
    div()
        .size(px(CHECK_SIDE))
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
