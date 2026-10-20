//! The sidebar's revset field speaks the same revsets as the command line:
//! a completion list under the field as you type (functions, aliases,
//! bookmarks, change ids, recent revsets), and jj's error once typing pauses,
//! pointed at where the revset goes wrong.

use std::time::Duration;

use diffui_core::JobId;
use diffui_core::completion::Completion;
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};
use iced::advanced::text::{Ellipsis, Wrapping};
use iced::keyboard::{self, key::Named};
use iced::widget::text::Span;
use iced::widget::{column, container, mouse_area, rich_text};
use iced::{Element, Length, Padding, Task};

use crate::cmdline::{
    CmdEvent, apply_completion, cursor_after_edit, cursor_after_key, place_caret,
};
use crate::icons;
use crate::palette::{RESULTS_INSET, ROW_HEIGHT};
use crate::theme::{self, ThemeSpec, text_size};
use crate::typography::{self, Aligned};
use crate::{Diffui, Message, TabId};

/// How long typing must pause before the revset is checked.
const SETTLE: Duration = Duration::from_millis(300);
/// Rows the dropdown shows before it scrolls.
const VISIBLE_ROWS: usize = 8;

/// The field's completion and error state, per tab.
#[derive(Debug, Default)]
pub(crate) struct FieldAssist {
    /// The dropdown is up.
    pub(crate) open: bool,
    pub(crate) cursor: usize,
    pub(crate) completions: Vec<Completion>,
    pub(crate) selected: usize,
    /// ↑/↓ moved through the list since the last edit.
    pub(crate) navigated: bool,
    pub(crate) version: u64,
    /// jj's verdict on `checked`, when it refused it.
    pub(crate) error: Option<RevsetError>,
    pub(crate) checked: Option<String>,
    pub(crate) job: Option<JobId>,
}

impl Diffui {
    /// The field's text changed.
    pub(crate) fn revset_field_edited(&mut self, value: String) -> Task<Message> {
        let old = std::mem::replace(&mut self.active_mut().session.revset, value);
        let new = self.active().session.revset.clone();
        let cursor = cursor_after_edit(&old, &new, self.active().cmdline.field.cursor);
        self.refresh_revset_field(cursor);
        let field = &mut self.active_mut().cmdline.field;
        field.navigated = false;
        field.error = None;
        field.checked = None;
        field.version = field.version.wrapping_add(1);
        let version = field.version;
        Task::perform(tokio::time::sleep(SETTLE), move |()| {
            Message::CommandLine(CmdEvent::FieldSettle(version))
        })
    }

    fn refresh_revset_field(&mut self, cursor: usize) {
        let history: Vec<String> = self.recents.revsets.iter().cloned().collect();
        let state = self.active();
        let value = state.session.revset.clone();
        let cursor = cursor.min(value.len());
        let completions = state
            .session
            .revset_completer(&history)
            .complete_revset(&value, cursor);
        let field = &mut self.active_mut().cmdline.field;
        field.cursor = cursor;
        field.open = !completions.is_empty();
        field.completions = completions;
        field.selected = 0;
    }

    /// Typing paused: ask jj whether the revset holds up.
    pub(crate) fn settle_revset_field(&mut self, version: u64) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        if self.active().cmdline.field.version != version
            || !self.active().session.capabilities.range_diff
        {
            return Task::none();
        }
        let value = self.active().session.revset.clone();
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        if let Some(old) = state.cmdline.field.job.take()
            && let Some(handle) = &state.handle
        {
            handle.send(diffui_core::Command::Cancel { job: old });
        }
        let (job, effects) = state.session.resolve_revset(RevsetQuery::new(&value));
        state.cmdline.field.job = Some(job);
        state.jobs.revset_field = Some(job);
        self.run_effects(tab, effects)
    }

    pub(crate) fn revset_field_resolved(
        &mut self,
        tab: TabId,
        revset: &str,
        result: Result<RevsetMatch, RevsetError>,
    ) -> Task<Message> {
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        state.jobs.revset_field = None;
        state.cmdline.field.job = None;
        if state.session.revset != revset {
            return Task::none();
        }
        state.cmdline.field.checked = Some(revset.to_owned());
        state.cmdline.field.error = result.err();
        Task::none()
    }

    /// Keys while the dropdown is up: ↑/↓ move, Tab takes the candidate, Esc
    /// closes it; the caret keys keep the cursor in step. Returns `None` for
    /// a key it leaves alone.
    pub(crate) fn revset_field_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
        consumed: bool,
    ) -> Option<Task<Message>> {
        let field = &self.active().cmdline.field;
        if !field.open {
            return None;
        }
        // A plain key nobody typed means the field lost focus.
        if !consumed
            && !matches!(
                key.as_ref(),
                keyboard::Key::Named(
                    Named::ArrowUp | Named::ArrowDown | Named::Tab | Named::Escape | Named::Enter
                )
            )
        {
            self.close_revset_field();
            return None;
        }
        let value = self.active().session.revset.clone();
        if let Some(cursor) = cursor_after_key(&value, field.cursor, key, modifiers) {
            self.refresh_revset_field(cursor);
            return None;
        }
        match key.as_ref() {
            keyboard::Key::Named(Named::ArrowDown) => Some(self.move_revset_field(1)),
            keyboard::Key::Named(Named::ArrowUp) => Some(self.move_revset_field(-1)),
            keyboard::Key::Named(Named::Tab) => {
                let index = self.active().cmdline.field.selected;
                Some(self.accept_revset_field(index))
            }
            keyboard::Key::Named(Named::Escape) => {
                self.close_revset_field();
                Some(Task::none())
            }
            _ => None,
        }
    }

    fn move_revset_field(&mut self, delta: i32) -> Task<Message> {
        let field = &mut self.active_mut().cmdline.field;
        if field.completions.is_empty() {
            return Task::none();
        }
        let len = field.completions.len() as i32;
        field.selected = (field.selected as i32 + delta).rem_euclid(len) as usize;
        field.navigated = true;
        let top = field.selected as f32 * ROW_HEIGHT;
        let viewport = VISIBLE_ROWS as f32 * ROW_HEIGHT;
        let y = (top + ROW_HEIGHT - viewport).max(0.0);
        iced::widget::operation::scroll_to(
            iced::widget::Id::new(DROPDOWN_ID),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
        )
    }

    pub(crate) fn accept_revset_field(&mut self, index: usize) -> Task<Message> {
        let Some(completion) = self.active().cmdline.field.completions.get(index).cloned() else {
            return Task::none();
        };
        let (value, cursor) = apply_completion(&self.active().session.revset, &completion);
        self.active_mut().session.revset = value.clone();
        self.refresh_revset_field(cursor);
        let field = &mut self.active_mut().cmdline.field;
        field.navigated = false;
        field.error = None;
        field.checked = None;
        field.version = field.version.wrapping_add(1);
        let version = field.version;
        Task::batch([
            iced::widget::operation::focus(iced::widget::Id::new(crate::sidebar::REVSET_INPUT_ID)),
            place_caret(crate::sidebar::REVSET_INPUT_ID, &value, cursor),
            Task::perform(tokio::time::sleep(SETTLE), move |()| {
                Message::CommandLine(CmdEvent::FieldSettle(version))
            }),
        ])
    }

    /// Enter in the field: take the candidate the user went looking for,
    /// else run the revset.
    pub(crate) fn submit_revset_field(&mut self) -> Task<Message> {
        let field = &self.active().cmdline.field;
        if field.open && field.navigated {
            let index = field.selected;
            return self.accept_revset_field(index);
        }
        self.close_revset_field();
        let revset = self.active().session.revset.clone();
        self.recents.push_revset(revset);
        self.recents.save();
        self.evaluate_revset()
    }

    pub(crate) fn close_revset_field(&mut self) {
        let field = &mut self.active_mut().cmdline.field;
        field.open = false;
        field.navigated = false;
    }

    pub(crate) fn revset_field_select(&mut self, index: usize) {
        let field = &mut self.active_mut().cmdline.field;
        if index < field.completions.len() {
            field.selected = index;
        }
    }
}

const DROPDOWN_ID: &str = "revset-completions";

/// The completion list, hung under the field over the graph.
pub(crate) fn dropdown(ui: &Diffui, theme: ThemeSpec) -> Option<Element<'_, Message>> {
    let field = &ui.active().cmdline.field;
    if !field.open || field.completions.is_empty() {
        return None;
    }
    let mut list = column![].spacing(0);
    for (index, completion) in field.completions.iter().enumerate() {
        list = list.push(
            mouse_area(crate::palette::row_shell(
                crate::cmdline_view::completion_row(ui, theme, completion),
                index == field.selected,
                theme,
            ))
            .on_press(Message::CommandLine(CmdEvent::FieldAccept(index)))
            .on_enter(Message::CommandLine(CmdEvent::FieldSelect(index))),
        );
    }
    let rows = field.completions.len().min(VISIBLE_ROWS);
    let card = container(
        crate::ui::scrollable(list.padding(RESULTS_INSET), theme)
            .id(iced::widget::Id::new(DROPDOWN_ID))
            .height(Length::Fixed(
                rows as f32 * ROW_HEIGHT + 2.0 * RESULTS_INSET,
            )),
    )
    .width(Length::Fill)
    .style(move |_| theme::popover_style(theme));
    Some(
        container(card)
            .width(Length::Fill)
            .padding(Padding {
                top: theme::space::XXS,
                right: crate::field::PANEL_TOGGLE_INSET,
                bottom: 0.0,
                left: crate::field::PANEL_TOGGLE_INSET,
            })
            .into(),
    )
}

/// jj's error under the field: the revset with the bad part underlined, and
/// the message.
pub(crate) fn error_strip(ui: &Diffui, theme: ThemeSpec) -> Option<Element<'_, Message>> {
    let field = &ui.active().cmdline.field;
    let error = field.error.as_ref()?;
    let value = &ui.active().session.revset;
    if field.checked.as_deref() != Some(value.as_str()) {
        return None;
    }
    // `desc` on its way to `description(` isn't wrong yet.
    if field.open
        && let Some(span) = &error.span
        && crate::cmdline::touches_caret(value, field.cursor, span)
    {
        return None;
    }
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let message = typography::baseline_row(
        [
            Aligned::glyph(
                icons::icon(icons::ALERT_TRIANGLE, 12.0, theme.removed_text),
                12.0,
            ),
            Aligned::gap(theme::space::SM),
            Aligned::text(
                crate::prose::Prose::from_backticks(&crate::cmdline::sentence(
                    &error.message,
                    error.hint.as_deref(),
                ))
                .view(font, mono, theme.removed_text, theme)
                .size(text_size::UI)
                .width(Length::Fill),
                font,
                text_size::UI,
            ),
        ],
        0.0,
    );
    let mut body = column![message].spacing(theme::space::XXS);
    if let Some(span) = error.span.clone() {
        let start = span.start.min(value.len());
        let end = span.end.min(value.len()).max(start);
        if value.is_char_boundary(start) && value.is_char_boundary(end) {
            let mut spans: Vec<Span<'_, ()>> =
                vec![Span::new(value[..start].to_owned()).color(theme.muted_text)];
            if end > start {
                spans.push(
                    Span::new(value[start..end].to_owned())
                        .color(theme.removed_text)
                        .underline(true),
                );
            } else {
                spans.push(Span::new("\u{2038}".to_owned()).color(theme.removed_text));
            }
            spans.push(Span::new(value[end..].to_owned()).color(theme.muted_text));
            body = body.push(
                container(
                    rich_text(spans)
                        .size(text_size::UI)
                        .font(mono)
                        .wrapping(Wrapping::None)
                        .ellipsis(Ellipsis::End),
                )
                .padding(Padding {
                    left: 12.0 + theme::space::SM,
                    ..Padding::ZERO
                }),
            );
        }
    }
    Some(
        container(body)
            .width(Length::Fill)
            .padding(Padding {
                top: 0.0,
                right: crate::field::PANEL_TOGGLE_INSET,
                bottom: theme::space::SM,
                left: crate::field::PANEL_HEADER_LEAD,
            })
            .into(),
    )
}
