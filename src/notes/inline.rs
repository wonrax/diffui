//! Notes in the diff: each thread laid under its line, at its file's header,
//! or above the files ([`crate::diff_view::Inset`]); the overview strip along
//! the scrollbar ([`crate::diff_view::Mark`]); next and previous.

use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, ElementId, SharedString, Window, div, px};

use super::app::{Note, NoteTarget, NotesMsg, SpotKind, diff_notes, shown_change, spots};
use super::locate::Place;
use super::store::{Role, Side};
use crate::commands::Context;
use crate::diff_view::{DiffProps, DocSpot, Inset, Mark, Marks};
use crate::theme::{self, Weight, text_size};
use crate::typography::Aligned;
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
                    .filter(|reply| reply.thread() == Some(id.as_str()))
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
    let mut insets = insets;
    if let Some(reply) = replying
        && let super::app::ReplyTo::New(note) = &reply.to
    {
        let title = match &note.anchor {
            None => "on the whole change".to_owned(),
            Some(anchor) => match &anchor.lines {
                None => anchor.path.clone(),
                Some(lines) => {
                    let range = if lines.start == lines.end() {
                        format!("Line {}", lines.start)
                    } else {
                        format!("Lines {}\u{2013}{}", lines.start, lines.end())
                    };
                    let side = match lines.side {
                        super::store::Side::New => "",
                        super::store::Side::Old => ", before the change",
                    };
                    format!("{range} of {}{side}", anchor.path)
                }
            },
        };
        let card = Rc::new(NewCard {
            title,
            draft: reply.draft.clone(),
            send_keys: send_keys.clone(),
            cancel_keys: cancel_keys.clone(),
            look,
            on: on.clone(),
        });
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        note.spot.hash(&mut hasher);
        reply.draft.lines().count().hash(&mut hasher);
        insets.push(Inset {
            at: note.spot,
            key: hasher.finish(),
            build: Rc::new(move |window, cx| card.render(window, cx)),
        });
    }
    if !insets.is_empty() {
        props = props.insets(insets);
    }
    if shown_change(tab).is_some() {
        let start = on.clone();
        let pick = on.clone();
        props = props.add_button(crate::diff_view::AddButton {
            on_press: Rc::new(move |press, window, cx| {
                start(&NotesMsg::StartNote(NoteTarget::Line(*press)), window, cx);
            }),
            on_pick: Rc::new(move |range, window, cx| {
                pick(&NotesMsg::Picking(*range), window, cx);
            }),
            tip: "Add a note",
            look,
        });
        let files = &tab.session.document.files;
        let counts = (0..files.len())
            .map(|index| super::app::file_note_count(ui, index))
            .collect();
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        let start = on.clone();
        props = props.note_buttons(crate::diff_view::NoteButtons {
            counts,
            on_press: Rc::new(move |index: &usize, window, cx| {
                if let Some(path) = paths.get(*index) {
                    start(
                        &NotesMsg::StartNote(NoteTarget::File(path.clone())),
                        window,
                        cx,
                    );
                }
            }),
            look,
        });
        let highlights = super::app::highlights(tab);
        if !highlights.is_empty() {
            props = props.highlights(highlights);
        }
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

/// `note`'s card, open, for the Notes view: its messages, and its reply
/// field when one is being written.
pub(crate) fn thread_card(
    ui: &Diffui,
    look: Look,
    note: &Note,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let tab = ui.active();
    let on: On = Rc::new(Diffui::listen(cx, |this, message: &NotesMsg, _, cx| {
        this.perform(Action::Notes(message.clone()), cx);
    }));
    let id = &note.thread.id;
    let card = Card {
        note: note.clone(),
        blocks: (0..note.thread.messages.len())
            .map(|index| {
                tab.notes
                    .parsed
                    .get(&(id.clone(), index))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect(),
        expanded: true,
        replying: tab
            .note_reply()
            .filter(|reply| reply.thread() == Some(id.as_str()))
            .map(|reply| reply.draft.clone()),
        off_line: None,
        send_keys: crate::overlay_kit::bound_keys(ui, Context::NoteReply, &["notes.reply.send"]),
        cancel_keys: crate::overlay_kit::bound_keys(
            ui,
            Context::NoteReply,
            &["notes.reply.cancel"],
        ),
        now: super::store::now(),
        look,
        on,
    };
    card.card(window, cx)
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
        div()
            .w_full()
            .pl(px(theme::space::XL))
            .pr(px(crate::scrollbar::GUTTER + theme::space::MD))
            .py(px(theme::space::SM))
            .block_mouse_except_scroll()
            .child(self.card(window, cx))
            .into_any_element()
    }

    /// The card alone: in the diff under its line, and in the Notes view.
    fn card(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let Look { theme, .. } = self.look;
        let thread = &self.note.thread;
        let hover = self.on.clone();
        let hovered_id = thread.id.clone();
        let mut card = div()
            .id(id(&thread.id, "card"))
            .on_hover(move |on: &bool, window, cx| {
                hover(&NotesMsg::Hover(hovered_id.clone(), *on), window, cx);
            })
            .flex()
            .flex_col()
            .w_full()
            .overflow_hidden()
            // The overlay cards' radius on a hairline, no lift: it sits in the
            // content.
            .map(|card| {
                theme::squircle(
                    card,
                    theme::radius::CARD,
                    Some(theme.panel_background),
                    Some(theme.border),
                )
            });
        if !self.open() {
            return card.child(self.folded()).into_any_element();
        }
        if let Some(lost) = self.lost_lines() {
            card = card.child(lost);
        }
        card = card.children(self.messages());
        card = match &self.replying {
            Some(draft) => card.child(reply_field(
                draft,
                "Send",
                &self.send_keys,
                &self.cancel_keys,
                self.look,
                &self.on,
                window,
                cx,
            )),
            None => card.child(self.footer()),
        };
        card.into_any_element()
    }

    fn open(&self) -> bool {
        self.expanded || self.replying.is_some()
    }

    /// The thread's states worth a word: resolved, outdated. Kit tags, as
    /// the History view's badges are.
    fn tags(&self) -> Vec<Aligned> {
        let Look { theme, font, .. } = self.look;
        let mut tags = Vec::new();
        if self.note.thread.resolved.is_some() {
            tags.push(
                crate::ui::tag(
                    "Resolved",
                    theme.subtle_text,
                    crate::ui::TagSize::Small,
                    font,
                )
                .aligned(),
            );
        }
        if self.note.located.outdated() {
            tags.push(
                crate::ui::tag("Outdated", theme.note_text, crate::ui::TagSize::Small, font)
                    .aligned(),
            );
        }
        tags
    }

    /// Folded: one line of the author, the latest message's first line, the
    /// reply count and the time. A press opens it.
    fn folded(&self) -> AnyElement {
        let Look { theme, font, .. } = self.look;
        let thread = &self.note.thread;
        let resolved = thread.resolved.is_some();
        let latest = thread.last();
        let first_line = latest.map_or(String::new(), |message| plain_line(&message.body));
        let replies = thread.messages.len().saturating_sub(1);
        let mut line = self.tags();
        line.push(author_on_line(
            latest,
            if resolved {
                theme.muted_text
            } else {
                theme.text
            },
            self.look,
        ));
        line.push(
            Aligned::text(
                typography::line(first_line, font, text_size::BODY, theme.muted_text)
                    .text_ellipsis(),
                font,
                text_size::BODY,
            )
            .shrink(),
        );
        if replies > 0 {
            let noun = if replies == 1 { "reply" } else { "replies" };
            line.push(caption_on_line(
                format!("{replies} {noun}"),
                theme.subtle_text,
                font,
            ));
        }
        if let Some(latest) = latest {
            line.push(caption_on_line(
                age(latest.at, self.now),
                theme.subtle_text,
                font,
            ));
        }
        div()
            .id(id(&thread.id, "folded"))
            .w_full()
            .min_w_0()
            .px(px(theme::space::MD_LG))
            .py(px(theme::space::MD))
            .cursor_pointer()
            .on_click(self.send(NotesMsg::Toggle(thread.id.clone())))
            .when(resolved, |row| row.opacity(0.75))
            .child(typography::baseline_row(line, theme::space::SM).min_w_0())
            .into_any_element()
    }

    /// Every message alike: the author and the time dimmed on one baseline,
    /// the Markdown body under them. The first message's line folds the
    /// thread, and carries its tags.
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
                let mut line = if index == 0 { self.tags() } else { Vec::new() };
                line.push(author_on_line(Some(message), theme.text, self.look));
                line.push(caption_on_line(
                    age(message.at, self.now),
                    theme.subtle_text,
                    font,
                ));
                if index == 0
                    && let Some(off_line) = &self.off_line
                {
                    line.push(
                        Aligned::text(
                            typography::line(
                                off_line.clone(),
                                mono,
                                text_size::CAPTION,
                                theme.subtle_text,
                            )
                            .text_ellipsis(),
                            mono,
                            text_size::CAPTION,
                        )
                        .shrink(),
                    );
                }
                let mut head = div()
                    .id(id(&thread.id, &format!("message-{index}")))
                    .w_full()
                    .min_w_0()
                    .child(typography::baseline_row(line, theme::space::SM).min_w_0());
                if index == 0 {
                    head = head
                        .cursor_pointer()
                        .on_click(self.send(NotesMsg::Toggle(thread.id.clone())));
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme::space::XS))
                    .w_full()
                    .px(px(theme::space::MD_LG))
                    .pt(px(theme::space::MD))
                    .child(head)
                    .child(body)
                    .into_any_element()
            })
            .collect()
    }

    /// An outdated note's lines as they were, in a well: what it was about.
    fn lost_lines(&self) -> Option<AnyElement> {
        if !self.note.located.outdated() {
            return None;
        }
        let Look { theme, mono, .. } = self.look;
        let lines = self.note.thread.anchor.as_ref()?.lines.as_ref()?;
        Some(
            div()
                .px(px(theme::space::MD_LG))
                .pt(px(theme::space::MD))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .px(px(theme::space::MD))
                        .py(px(theme::space::XS))
                        .map(|well| {
                            theme::squircle(
                                well,
                                theme::radius::MD,
                                Some(theme.background),
                                Some(theme.border),
                            )
                        })
                        .children(lines.text.iter().map(|line| {
                            typography::line(
                                line.clone(),
                                mono,
                                text_size::CAPTION,
                                theme.muted_text,
                            )
                            .text_ellipsis()
                        })),
                )
                .into_any_element(),
        )
    }

    /// The thread's actions, once for the whole thread: who it waits on, in
    /// words, then Copy, Reply and Resolve (or Reopen), the kit's quiet
    /// buttons.
    fn footer(&self) -> AnyElement {
        let Look { theme, font, .. } = self.look;
        let thread = &self.note.thread;
        let hint = if let Some(resolved) = &thread.resolved {
            let by = if resolved.by == "you" {
                "you"
            } else {
                resolved.by.as_str()
            };
            format!("Resolved by {by} {}", age(resolved.at, self.now))
        } else if thread.waiting_on_agent() {
            "Waiting on the agent".to_owned()
        } else {
            "Waiting on you".to_owned()
        };
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::XS))
            .w_full()
            .min_w_0()
            .px(px(theme::space::MD_LG))
            .py(px(theme::space::SM))
            .child(div().flex_1().min_w_0().child(typography::centered(
                typography::line(hint, font, text_size::CAPTION, theme.subtle_text).text_ellipsis(),
                font,
                text_size::CAPTION,
            )))
            .child(
                self.look
                    .icon_button(id(&thread.id, "copy"), icons::COPY, theme.muted_text)
                    .small()
                    .tip("Copy as Markdown")
                    .on_click(self.send(NotesMsg::Copy(vec![thread.id.clone()]))),
            );
        if thread.resolved.is_none() {
            row = row.child(
                self.look
                    .button(id(&thread.id, "reply"), "Reply")
                    .ghost()
                    .small()
                    .on_click(self.send(NotesMsg::StartReply(thread.id.clone()))),
            );
        }
        let (label, message) = if thread.resolved.is_some() {
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
}

/// A message's author on a line: the agent's name or "You", semibold.
fn author_on_line(
    message: Option<&super::store::Message>,
    ink: crate::theme::Color,
    look: Look,
) -> Aligned {
    let font = theme::emphasis_font(look.font, Weight::SEMIBOLD);
    let name = match message {
        Some(message) if message.role == Role::User => "You".to_owned(),
        Some(message) => message.author.clone(),
        None => String::new(),
    };
    Aligned::text(
        typography::line(name, font, text_size::BODY, ink).flex_none(),
        font,
        text_size::BODY,
    )
}

/// A caption on a line's baseline.
fn caption_on_line(text: String, ink: crate::theme::Color, font: crate::theme::Font) -> Aligned {
    Aligned::text(
        typography::line(text, font, text_size::CAPTION, ink).flex_none(),
        font,
        text_size::CAPTION,
    )
}

/// A message's first line as plain words: its Markdown marks dropped.
pub(crate) fn plain_line(body: &str) -> String {
    let first = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    first
        .trim()
        .trim_start_matches(['#', '>', '-', '*', ' '])
        .replace("**", "")
        .replace("__", "")
        .replace('`', "")
}

/// The reply field and its footer: for a reply (`send` "Reply") or a new
/// note ("Add note").
#[allow(clippy::too_many_arguments)]
fn reply_field(
    draft: &str,
    send_label: &'static str,
    send_keys: &[String],
    cancel_keys: &[String],
    look: Look,
    on: &On,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let typed = on.clone();
    let spec = crate::input::FieldSpec::new(REPLY_FIELD, draft)
        .placeholder("Write in Markdown\u{2026}")
        .on_input(move |text, window, cx| typed(&NotesMsg::ReplyEdited(text), window, cx));
    let (field, focused) = crate::input::multi_line_with_focus(spec, font, window, cx);
    let lines = draft.lines().count().clamp(2, 10) as f32;
    let height =
        lines * text_size::BODY * crate::typography::DEFAULT_LINE_HEIGHT + 2.0 * theme::space::MD;
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
    if !send_keys.is_empty() {
        hints.push(
            crate::overlay_kit::key_hint(&keys(send_keys), send_label, look).into_any_element(),
        );
    }
    if !cancel_keys.is_empty() {
        hints.push(
            crate::overlay_kit::key_hint(&keys(cancel_keys), "Cancel", look).into_any_element(),
        );
    }
    let click = |message: NotesMsg| {
        let on = on.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| on(&message, window, cx)
    };
    let cancel = look
        .cancel_button("note-reply-cancel")
        .compact()
        .on_click(click(NotesMsg::CancelReply));
    let send = look
        .button("note-reply-send", send_label)
        .primary()
        .compact()
        .disabled(draft.trim().is_empty())
        .on_click(click(NotesMsg::SendReply));
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

/// The card a new note of the user's is written in, at its spot.
struct NewCard {
    title: String,
    draft: String,
    send_keys: Vec<String>,
    cancel_keys: Vec<String>,
    look: Look,
    on: On,
}

impl NewCard {
    fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let Look { theme, font, mono } = self.look;
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::SM))
            .w_full()
            .min_w_0()
            .px(px(theme::space::MD_LG))
            .py(px(theme::space::SM))
            .child(icons::icon(
                icons::MESSAGE_SQUARE,
                theme::icon::SM,
                theme.accent,
            ))
            .child(
                typography::line(
                    "New note",
                    theme::emphasis_font(font, Weight::SEMIBOLD),
                    text_size::BODY,
                    theme.text,
                )
                .flex_none(),
            )
            .child(
                typography::line(
                    self.title.clone(),
                    mono,
                    text_size::CAPTION,
                    theme.muted_text,
                )
                .min_w_0()
                .text_ellipsis(),
            );
        let card = div()
            .id("note-new-card")
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
            .child(header)
            .child(reply_field(
                &self.draft,
                "Add note",
                &self.send_keys,
                &self.cancel_keys,
                self.look,
                &self.on,
                window,
                cx,
            ));
        div()
            .w_full()
            .pl(px(theme::space::XL))
            .pr(px(crate::scrollbar::GUTTER + theme::space::MD))
            .py(px(theme::space::SM))
            .block_mouse_except_scroll()
            .child(card)
            .into_any_element()
    }
}
