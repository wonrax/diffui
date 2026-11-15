//! Notes in the diff: each thread laid under its line, at its file's header,
//! or above the files ([`crate::diff_view::Inset`]); the overview strip along
//! the scrollbar ([`crate::diff_view::Mark`]); next and previous.

use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ElementId, SharedString, Window, div, px};

use super::app::{Note, NotesMsg, SpotKind, diff_notes, spots};
use super::locate::Place;
use super::store::{Role, Side};
use crate::commands::Context;
use crate::diff_view::{DiffProps, DocSpot, Inset, Mark, Marks};
use crate::theme::{self, Weight, text_size};
use crate::ui::Look;
use crate::{Action, Cx, Diffui, icons, typography};

/// The reply field's id: one reply is written at a time.
pub(crate) const REPLY_FIELD: &str = "note-reply-field";

type On = Rc<dyn Fn(&NotesMsg, &mut Window, &mut App)>;

/// Everything a thread's card is drawn from, owned: the card is built each
/// frame it shows, long after the render that made this.
struct Card {
    note: Note,
    blocks: Vec<Rc<Vec<crate::markdown::Block>>>,
    expanded: bool,
    /// The draft, while the reply field is open under this thread.
    replying: Option<String>,
    /// Where the anchored lines are, when the card doesn't sit on them.
    off_line: Option<String>,
    send_keys: Vec<String>,
    cancel_keys: Vec<String>,
    now: u64,
    look: Look,
    on: On,
}

/// The diff's notes as insets, its notes and conflicts on the overview strip,
/// and the spot next/previous asked to show.
pub(crate) fn decorate(ui: &Diffui, look: Look, mut props: DiffProps, cx: &mut Cx) -> DiffProps {
    let tab = ui.active();
    let on: On = Rc::new(Diffui::listen(cx, |this, message: &NotesMsg, _, cx| {
        this.perform(Action::Notes(message.clone()), cx);
    }));
    let replying = tab.note_reply();
    let send_keys = crate::overlay_kit::bound_keys(ui, Context::NoteReply, &["notes.reply.send"]);
    let cancel_keys =
        crate::overlay_kit::bound_keys(ui, Context::NoteReply, &["notes.reply.cancel"]);
    let now = super::store::now();
    let insets: Vec<Inset> = diff_notes(tab)
        .into_iter()
        .map(|(spot, note)| {
            let id = &note.thread.id;
            let blocks = (0..note.thread.messages.len())
                .map(|index| {
                    tab.notes
                        .parsed
                        .get(&(id.clone(), index))
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            let off_line = match (spot, &note.located.place, &note.thread.anchor) {
                (DocSpot::File(_) | DocSpot::Top, Place::Lines { start, end }, Some(anchor)) => {
                    Some(lines_label(&anchor.path, *start, *end, side_of(note)))
                }
                (DocSpot::Top, Place::File { .. }, Some(anchor)) => Some(anchor.path.clone()),
                _ => None,
            };
            let card = Card {
                note: note.clone(),
                blocks,
                expanded: tab.notes.expanded.contains(id),
                replying: replying
                    .filter(|reply| reply.id == *id)
                    .map(|reply| reply.draft.clone()),
                off_line,
                send_keys: send_keys.clone(),
                cancel_keys: cancel_keys.clone(),
                now,
                look,
                on: on.clone(),
            };
            let key = card.key();
            let card = Rc::new(card);
            Inset {
                at: spot,
                key,
                build: Rc::new(move |window, cx| card.render(window, cx)),
            }
        })
        .collect();
    if !insets.is_empty() {
        props = props.insets(insets);
    }
    let Look { theme, .. } = look;
    let spots = spots(tab);
    if !spots.is_empty() {
        let targets: Vec<DocSpot> = spots.iter().map(|(spot, _)| *spot).collect();
        let marks = spots
            .into_iter()
            .map(|(at, kind)| match kind {
                SpotKind::Note { label, .. } => Mark {
                    at,
                    color: theme.accent,
                    label: crate::ellipsis::cut_end(&label, 480.0, text_size::CAPTION, look.font)
                        .into_owned(),
                },
                SpotKind::Conflict { label } => Mark {
                    at,
                    color: theme.conflict_marker,
                    label,
                },
            })
            .collect();
        let jump = Diffui::listen(cx, move |this, index: &usize, _, _| {
            if let Some(spot) = targets.get(*index) {
                this.reveal_spot(*spot);
            }
        });
        props = props.overview(Marks {
            marks,
            on_click: Rc::new(jump),
            tooltip: look,
        });
    }
    if let Some((spot, token)) = tab.notes.reveal {
        props = props.reveal(spot, token);
    }
    props
}

impl Diffui {
    /// Scroll the diff to `spot`, and make it where next/previous go on from.
    pub(crate) fn reveal_spot(&mut self, spot: DocSpot) {
        let tab = self.active_mut();
        let token = tab.notes.reveal.map_or(1, |(_, token)| token + 1);
        tab.notes.reveal = Some((spot, token));
    }
}

fn side_of(note: &Note) -> Side {
    note.thread
        .anchor
        .as_ref()
        .and_then(|anchor| anchor.lines.as_ref())
        .map_or(Side::New, |lines| lines.side)
}

/// `src/lib.rs:40-52`, with the old side said.
pub(crate) fn lines_label(path: &str, start: usize, end: usize, side: Side) -> String {
    let lines = if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}-{end}")
    };
    match side {
        Side::New => lines,
        Side::Old => format!("{lines} (before)"),
    }
}

/// "5m ago" for unix seconds.
pub(crate) fn age(at: u64, now: u64) -> String {
    crate::meta_line::age_phrase(at as i64 * 1000, now as i64 * 1000)
}

fn id(thread: &str, part: &str) -> ElementId {
    ElementId::Name(SharedString::from(format!("note-{thread}-{part}")))
}

impl Card {
    /// What the card's height depends on.
    fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let thread = &self.note.thread;
        thread.id.hash(&mut hasher);
        thread.messages.len().hash(&mut hasher);
        thread.resolved.is_some().hash(&mut hasher);
        self.expanded.hash(&mut hasher);
        self.replying
            .as_ref()
            .map(|draft| draft.lines().count())
            .hash(&mut hasher);
        self.off_line.hash(&mut hasher);
        self.note.located.outdated().hash(&mut hasher);
        hasher.finish()
    }

    fn send(
        &self,
        message: NotesMsg,
    ) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static {
        let on = self.on.clone();
        move |_, window, cx| on(&message, window, cx)
    }

    fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let Look { theme, .. } = self.look;
        let thread = &self.note.thread;
        let resolved = thread.resolved.is_some();
        let mut card = div()
            .id(id(&thread.id, "card"))
            .flex()
            .flex_col()
            .w_full()
            .overflow_hidden()
            .map(|card| {
                theme::squircle(
                    card,
                    theme::radius::CARD,
                    Some(theme.panel_background_elevated),
                    Some(theme.border),
                )
            })
            .child(self.header());
        if self.expanded && !resolved || self.replying.is_some() {
            card = card.children(self.messages());
            if self.replying.is_some() {
                card = card.child(self.reply_field(window, cx));
            }
        } else {
            card = card.child(self.summary());
        }
        div()
            .w_full()
            .pl(px(theme::space::XL))
            .pr(px(crate::scrollbar::GUTTER + theme::space::MD))
            .py(px(theme::space::SM))
            .block_mouse_except_scroll()
            .child(card)
            .into_any_element()
    }

    /// Who wrote it and when, what state it's in, and its buttons. A click
    /// on it folds or unfolds the thread.
    fn header(&self) -> AnyElement {
        let Look { theme, font, mono } = self.look;
        let thread = &self.note.thread;
        let first = thread.messages.first();
        let author = first.map_or("agent", |message| message.author.as_str());
        let resolved = thread.resolved.is_some();
        let ink = if resolved {
            theme.subtle_text
        } else {
            theme.text
        };
        let mut row = div()
            .id(id(&thread.id, "header"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::SM))
            .w_full()
            .min_w_0()
            .px(px(theme::space::MD_LG))
            .py(px(theme::space::SM))
            .cursor_pointer()
            .on_click(self.send(NotesMsg::Toggle(thread.id.clone())))
            .child(icons::icon(
                icons::MESSAGE_SQUARE,
                theme::icon::SM,
                if resolved {
                    theme.subtle_text
                } else {
                    theme.accent
                },
            ))
            .child(
                typography::line(
                    author.to_owned(),
                    theme::emphasis_font(font, Weight::SEMIBOLD),
                    text_size::BODY,
                    ink,
                )
                .flex_none(),
            )
            .child(
                typography::line(
                    first.map_or_else(String::new, |message| age(message.at, self.now)),
                    font,
                    text_size::CAPTION,
                    theme.subtle_text,
                )
                .flex_none(),
            );
        if let Some(off_line) = &self.off_line {
            row = row.child(
                typography::line(off_line.clone(), mono, text_size::CAPTION, theme.muted_text)
                    .min_w_0()
                    .text_ellipsis(),
            );
        }
        let state = if resolved {
            Some(("resolved", theme.subtle_text))
        } else if self.note.located.outdated() {
            Some(("outdated", theme.note_text))
        } else if thread.waiting_on_agent() {
            Some(("replied", theme.info))
        } else {
            None
        };
        if let Some((label, color)) = state {
            row = row.child(
                typography::line(label, mono, text_size::BADGE, color)
                    .flex_none()
                    .px(px(theme::space::XS))
                    .rounded(px(theme::radius::CHIP))
                    .bg(theme::chip_background(color)),
            );
        }
        row = row.child(div().flex_1());
        let replying = self.replying.is_some();
        if !resolved && !replying {
            row = row.child(
                self.look
                    .button(id(&thread.id, "reply"), "Reply")
                    .icon(icons::REPLY, theme.muted_text)
                    .ghost()
                    .small()
                    .on_click(self.send(NotesMsg::StartReply(thread.id.clone()))),
            );
        }
        let (label, message) = if resolved {
            ("Reopen", NotesMsg::Reopen(vec![thread.id.clone()]))
        } else {
            ("Resolve", NotesMsg::Resolve(vec![thread.id.clone()]))
        };
        row.child(
            self.look
                .button(id(&thread.id, "resolve"), label)
                .ghost()
                .small()
                .on_click(self.send(message)),
        )
        .into_any_element()
    }

    /// Folded: the first message's first line, and how many replies.
    fn summary(&self) -> AnyElement {
        let Look { theme, font, .. } = self.look;
        let thread = &self.note.thread;
        let first = thread
            .messages
            .first()
            .map_or("", |message| message.body.lines().next().unwrap_or(""));
        let replies = thread.messages.len().saturating_sub(1);
        let mut row = div()
            .id(id(&thread.id, "summary"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::SM))
            .w_full()
            .min_w_0()
            .px(px(theme::space::MD_LG))
            .pb(px(theme::space::SM))
            .cursor_pointer()
            .on_click(self.send(NotesMsg::Toggle(thread.id.clone())))
            .child(
                typography::line(
                    first.to_owned(),
                    font,
                    text_size::BODY,
                    if thread.resolved.is_some() {
                        theme.subtle_text
                    } else {
                        theme.muted_text
                    },
                )
                .flex_1()
                .min_w_0()
                .text_ellipsis(),
            );
        if replies > 0 {
            let noun = if replies == 1 { "reply" } else { "replies" };
            row = row.child(
                typography::line(
                    format!("{replies} {noun}"),
                    font,
                    text_size::CAPTION,
                    theme.subtle_text,
                )
                .flex_none(),
            );
        }
        row.into_any_element()
    }

    /// Unfolded: every message, the first under the header, each reply under
    /// its author and age.
    fn messages(&self) -> Vec<AnyElement> {
        let Look { theme, font, mono } = self.look;
        let thread = &self.note.thread;
        thread
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let blocks = self.blocks.get(index).cloned().unwrap_or_default();
                let body = crate::markdown::render(
                    &format!("note-{}-{index}", thread.id),
                    &blocks,
                    crate::markdown::MarkdownLook {
                        theme,
                        font,
                        mono,
                        size: text_size::BODY,
                    },
                    Rc::new(|url: &str, _: &mut Window, _: &mut App| {
                        if url.starts_with("http://") || url.starts_with("https://") {
                            crate::open_url(url);
                        }
                    }),
                );
                let mut block = div()
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::XS))
                    .w_full()
                    .px(px(theme::space::MD_LG))
                    .pb(px(theme::space::MD));
                if index > 0 {
                    let who = match message.role {
                        Role::User => "You".to_owned(),
                        Role::Agent => message.author.clone(),
                    };
                    block = block
                        .pt(px(theme::space::MD))
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(theme::space::SM))
                                .child(typography::line(
                                    who,
                                    theme::emphasis_font(font, Weight::SEMIBOLD),
                                    text_size::BODY,
                                    theme.text,
                                ))
                                .child(typography::line(
                                    age(message.at, self.now),
                                    font,
                                    text_size::CAPTION,
                                    theme.subtle_text,
                                )),
                        );
                }
                block.child(body).into_any_element()
            })
            .collect()
    }

    /// The reply field, and its footer.
    fn reply_field(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let Look { theme, font, mono } = self.look;
        let draft = self.replying.clone().unwrap_or_default();
        let on = self.on.clone();
        let spec = crate::input::FieldSpec::new(REPLY_FIELD, &draft)
            .placeholder("Reply, in Markdown\u{2026}")
            .on_input(move |text, window, cx| on(&NotesMsg::ReplyEdited(text), window, cx));
        let (field, focused) = crate::input::multi_line_with_focus(spec, font, window, cx);
        let lines = draft.lines().count().clamp(2, 10) as f32;
        let height = lines * text_size::BODY * crate::typography::DEFAULT_LINE_HEIGHT
            + 2.0 * theme::space::MD;
        let input = div()
            .w_full()
            .h(px(height))
            .font(font.gpui())
            .text_size(px(text_size::BODY))
            .child(
                field
                    .appearance(false)
                    .text_size(px(text_size::BODY))
                    .h_full(),
            );
        fn keys(keys: &[String]) -> Vec<&str> {
            keys.iter().map(String::as_str).collect()
        }
        let mut hints = Vec::new();
        if !self.send_keys.is_empty() {
            hints.push(
                crate::overlay_kit::key_hint(&keys(&self.send_keys), "Reply", self.look)
                    .into_any_element(),
            );
        }
        if !self.cancel_keys.is_empty() {
            hints.push(
                crate::overlay_kit::key_hint(&keys(&self.cancel_keys), "Cancel", self.look)
                    .into_any_element(),
            );
        }
        let _ = mono;
        let cancel = self
            .look
            .cancel_button("note-reply-cancel")
            .compact()
            .on_click(self.send(NotesMsg::CancelReply));
        let send = self
            .look
            .button("note-reply-send", "Reply")
            .primary()
            .compact()
            .disabled(draft.trim().is_empty())
            .on_click(self.send(NotesMsg::SendReply));
        let footer = crate::overlay_kit::footer_with_buttons(
            crate::overlay_kit::Density::Well,
            theme::radius::inner(theme::radius::CONTROL, theme::HAIRLINE),
            hints,
            [cancel.into_any_element(), send.into_any_element()],
            theme,
        );
        div()
            .w_full()
            .px(px(theme::space::MD_LG))
            .pb(px(theme::space::MD_LG))
            .child(
                crate::field::field_well(focused, theme)
                    .flex()
                    .flex_col()
                    .child(input)
                    .child(footer),
            )
            .into_any_element()
    }
}
