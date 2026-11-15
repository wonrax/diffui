//! The Notes view, built like History: the left pane becomes the list of
//! notes (grouped under their change, filtered from its head), the main area
//! shows the selected thread in context, and picks act through a floating bar
//! like the diff's picks.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ElementId, SharedString, Window, canvas, div, px};

use super::app::{Filter, Note, NotesMsg};
use super::inline::{age, lines_label, plain_line};
use super::locate::Place;
use super::store::Role;
use crate::diff_view::CheckState;
use crate::history_panel::{
    DETAIL_EDGE, caption_on_line, fill_on_line, header_bar, list_column, list_row, mono_on_line,
    note, two_line,
};
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
            .then_with(|| b.thread.activity().cmp(&a.thread.activity()))
    });
    keyed.into_iter().map(|(_, note)| note).collect()
}

fn id(name: &str, part: &str) -> ElementId {
    ElementId::Name(SharedString::from(format!("notes-view-{name}-{part}")))
}

/// The detail column's widest: a comfortable measure of body text, past
/// which a wide window only lengthens its lines.
const DETAIL_MAX_WIDTH: f32 = 760.0;

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
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let tab = ui.active();
    let view = &tab.notes.view;
    let on = sender(cx);

    // One filter control: the three that matter, their counts quiet at zero.
    let mut filters = ui::segmented(theme);
    for filter in Filter::SHOWN {
        let count = tab
            .notes
            .notes
            .iter()
            .filter(|note| filter.admits(note))
            .count();
        filters = filters.child(
            ui::segment(
                id(filter.name(), "filter"),
                ui::Segment {
                    icon: None,
                    label: filter.label().into(),
                    count: Some((count, count > 0)),
                    active: view.filter == filter,
                    enabled: true,
                },
                ui::Segmented::Small,
                look,
            )
            .on_click(click(&on, NotesMsg::Filter(filter))),
        );
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
        .child(div().flex_1().min_w_0().overflow_hidden().child(filters))
        .child(more);

    let mut pane = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
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
    let mut list = div().w_full().flex().flex_col().pb(px(space::XXL * 3.0));
    let mut change: Option<&str> = None;
    for note in &listed {
        let thread = &note.thread;
        if change != Some(thread.change_id()) {
            change = Some(thread.change_id());
            list = list.child(group_header(note, ui, look));
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
    }
    pane.child(
        div()
            .flex_1()
            .min_h_0()
            .child(ui::scrollable("notes-view-list", list)),
    )
    .into_any_element()
}

/// A change's header over its notes: its id, as ids are styled, and its
/// title.
fn group_header(note: &Note, ui: &Diffui, look: Look) -> AnyElement {
    let Look { theme, font, .. } = look;
    let thread = &note.thread;
    let (title, ink) = change_title(note, theme);
    let mut line = vec![mono_on_line(
        short(thread.change_id()).to_owned(),
        theme.change_id,
        ui,
    )];
    if note.located.archived() {
        line.push(ui::tag("Abandoned", theme.subtle_text, ui::TagSize::Small, font).aligned());
    }
    line.push(fill_on_line(
        typography::line(title, font, CAPTION_TEXT_SIZE, ink).text_ellipsis(),
        font,
        CAPTION_TEXT_SIZE,
    ));
    div()
        .w_full()
        .pl(px(DETAIL_EDGE))
        .pr(px(crate::scrollbar::GUTTER))
        .pt(px(space::LG))
        .pb(px(space::XXS))
        .child(typography::baseline_row(line, space::SM).min_w_0())
        .into_any_element()
}

/// Where a note is, for its row and its header: `path:12–18`, or the file's
/// path, in mono at the size the rows give ids and paths; "Whole change" in
/// prose at `size`.
fn location(note: &Note, look: Look, size: f32, ink: Color) -> Aligned {
    let Look { font, mono, .. } = look;
    let path_size = CAPTION_TEXT_SIZE;
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
                typography::line(text, mono, path_size, ink).text_ellipsis(),
                mono,
                path_size,
            )
        }
    }
}

/// One note, as a revision row is laid out: its location and age on the
/// title line; the author, the latest message's first line and the reply
/// count under it. Its box shows on hover, or for every row once picking.
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
    let mut title = vec![
        location(
            note,
            look,
            SMALL_TEXT_SIZE,
            if dim { theme.muted_text } else { theme.text },
        )
        .shrink(),
    ];
    if note.located.outdated() {
        title.push(ui::tag("Outdated", theme.note_text, ui::TagSize::Small, font).aligned());
    }
    title.push(fill_on_line(div(), font, SMALL_TEXT_SIZE));
    title.push(Aligned::text(
        typography::line(
            latest.map_or(String::new(), |message| age(message.at, now)),
            font,
            AGE_TEXT_SIZE,
            theme.subtle_text,
        ),
        font,
        AGE_TEXT_SIZE,
    ));
    let author = latest.map_or(String::new(), |message| match message.role {
        Role::User => "You".to_owned(),
        Role::Agent => message.author.clone(),
    });
    let bold = theme::emphasis_font(font, Weight::MEDIUM);
    let mut meta = vec![
        Aligned::text(
            typography::line(author, bold, CAPTION_TEXT_SIZE, theme.muted_text).flex_none(),
            bold,
            CAPTION_TEXT_SIZE,
        ),
        fill_on_line(
            typography::line(
                latest.map_or(String::new(), |message| plain_line(&message.body)),
                font,
                CAPTION_TEXT_SIZE,
                theme.subtle_text,
            )
            .text_ellipsis(),
            font,
            CAPTION_TEXT_SIZE,
        ),
    ];
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
        .mr(px(space::SM))
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
    .child(check)
    .child(two_line(
        typography::baseline_row(title, space::SM).min_w_0(),
        typography::baseline_row(meta, space::SM).min_w_0(),
    ))
    .into_any_element()
}

/// The main area while the Notes view is open: the selected thread in
/// context, and the picks' bar.
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
    let thread = view
        .thread
        .as_deref()
        .and_then(|id| tab.notes.get(id))
        .filter(|note| listed(tab).iter().any(|it| it.thread.id == note.thread.id));
    panel = match thread {
        Some(note) => panel.child(ui::scrollable(
            "notes-view-thread",
            detail(note, ui, look, window, cx),
        )),
        None if tab.notes.notes.is_empty() => panel.child(empty(
            "No review notes yet",
            "Agents leave them with diffui notes add. Start one with the + on a line of a diff.",
            look,
        )),
        None => panel.child(empty(
            "Select a note",
            "Its lines and its thread show here.",
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

/// A thread in context, as History shows an operation: a header naming
/// where it sits (its change's id and description, as the log draws them,
/// and its location), the lines it's on as the diff draws them, then the
/// thread itself. Who wrote it and when is on the card.
fn detail(note: &Note, ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, font, .. } = look;
    let thread = &note.thread;
    let on = sender(cx);
    let (title, ink) = change_title(note, theme);
    let open = look
        .button("notes-view-open", "Open in diff")
        .compact()
        .icon(icons::FILE_DIFF, theme.muted_text)
        .disabled(note.located.archived())
        .tip("Show the note in its change's diff")
        .on_click(click(&on, NotesMsg::Open(thread.id.clone())));
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
    let mut place = vec![location(note, look, SMALL_TEXT_SIZE, theme.muted_text).shrink()];
    if thread.resolved.is_some() {
        place.push(ui::tag("Resolved", theme.subtle_text, ui::TagSize::Small, font).aligned());
    }
    if note.located.outdated() {
        place.push(ui::tag("Outdated", theme.note_text, ui::TagSize::Small, font).aligned());
    }
    let mut block = div()
        .w_full()
        .max_w(px(DETAIL_MAX_WIDTH))
        .flex()
        .flex_col()
        .gap(px(space::MD))
        .px(px(DETAIL_EDGE))
        .pt(px(space::XL))
        .pb(px(space::XXL * 3.0))
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
        .child(typography::baseline_row(place, space::SM).min_w_0());
    if let Some(excerpt) = excerpt(note, ui, look, window, cx) {
        block = block.child(excerpt);
    }
    block
        .child(crate::notes::inline::thread_card(
            ui, look, note, window, cx,
        ))
        .into_any_element()
}

/// The note's lines as the diff draws them, read-only: a plain document of
/// just those lines, in a well.
fn excerpt(
    note: &Note,
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let Look { theme, .. } = look;
    let anchor = note.thread.anchor.as_ref()?;
    // Only a note on lines has an excerpt.
    anchor.lines.as_ref()?;
    let tab = ui.active();
    if tab.notes.view.excerpt.is_empty() {
        return None;
    }
    let view = &tab.notes.view;
    let rows: usize = view.excerpt.iter().map(|hunk| hunk.lines.len()).sum();
    // The well's hairline edges sit inside its box.
    let height = crate::diff_view::unwrapped_height(
        ui.config.code_type,
        look.mono,
        1,
        view.excerpt.len(),
        rows,
    ) + 2.0 * theme::HAIRLINE;
    let (status, additions, deletions) = view
        .excerpt_file
        .as_ref()
        .map_or((diffui_core::DiffFileStatus::Modified, 0, 0), |file| {
            (file.status, file.additions, file.deletions)
        });
    let status_color = theme::file_status_color(status, theme);
    let file = crate::diff_view::DiffFileView {
        title: anchor.path.clone(),
        status,
        status_color,
        status_fill: theme::chip_background(status_color),
        additions,
        deletions,
        tag: None,
        note: None,
    };
    let props = crate::diff_view::DiffProps::new(
        vec![file],
        crate::diff_view::DocSource::new(cx.entity(), |ui: &Diffui, _| {
            &ui.active().notes.view.excerpt
        }),
        0,
        format!("notes-excerpt-{}", note.thread.id),
        crate::theme::diff_palette(theme),
        look.mono,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .ui_font(look.font)
    .wrap(false)
    .content_version(ui.document_version)
    .layout_version(ui.document_version);
    Some(
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .h(px(height))
                    .overflow_hidden()
                    .map(|well| {
                        theme::squircle(
                            well,
                            theme::radius::CARD,
                            Some(theme.panel_background),
                            Some(theme.border),
                        )
                    })
                    .child(crate::diff_view::view("notes-excerpt", props, window, cx)),
            )
            .into_any_element(),
    )
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
