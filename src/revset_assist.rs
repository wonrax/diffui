//! The sidebar's revset field speaks the same revsets as the command line:
//! a completion list under the field as you type (functions, aliases,
//! bookmarks, change ids, recent revsets), and jj's error once typing pauses,
//! pointed at where the revset goes wrong.

use std::ops::Range;
use std::time::Duration;

use diffui_core::JobId;
use diffui_core::completion::Completion;
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};
use iced::advanced::text::Wrapping;
use iced::keyboard::{self, key::Named};
use iced::widget::text::Span;
use iced::widget::{column, container, mouse_area, rich_text};
use iced::{Element, Font, Length, Padding, Task};

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
        // ⌃N/⌃P walk the list like ↓/↑, as in the palette.
        let list_key = crate::ui::cocoa_list_key(key, modifiers);
        let key = list_key.as_ref().unwrap_or(key);
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
        self.recents.push_revset(revset.clone());
        self.recents.save();
        // Check it now rather than after the typing pause, so a bad revset
        // is explained by the field, under it, as soon as it is run.
        let check = if self.active().cmdline.field.checked.as_deref() == Some(revset.as_str()) {
            Task::none()
        } else {
            let version = self.active().cmdline.field.version;
            self.settle_revset_field(version)
        };
        Task::batch([self.evaluate_revset(), check])
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

/// jj's error for the revset in the field, when it is the one to show: the
/// check is for what the field holds, and not about a word still being
/// typed (`desc` on its way to `description(` isn't wrong yet).
fn shown_error(ui: &Diffui) -> Option<&RevsetError> {
    let field = &ui.active().cmdline.field;
    let error = field.error.as_ref()?;
    let value = &ui.active().session.revset;
    if field.checked.as_deref() != Some(value.as_str()) {
        return None;
    }
    if field.open
        && let Some(span) = &error.span
        && crate::cmdline::touches_caret(value, field.cursor, span)
    {
        return None;
    }
    Some(error)
}

/// Whether the field already explains a revset error (or is about to: its
/// check of this revset is in flight), so a failed load of the same revset
/// mustn't say it again.
pub(crate) fn explains_revset_error(ui: &Diffui) -> bool {
    let field = &ui.active().cmdline.field;
    shown_error(ui).is_some()
        || field.job.is_some()
            && field.checked.as_deref() != Some(ui.active().session.revset.as_str())
}

/// jj's error under the field: the message, the revset with the bad part
/// underlined, and the way back to the default. The icon hangs in the
/// field's icon column and the rest starts where the field's text does.
pub(crate) fn error_strip(ui: &Diffui, theme: ThemeSpec) -> Option<Element<'_, Message>> {
    use crate::field::{LEADING_ICON_INSET, LEADING_ICON_SIZE, LEADING_TEXT_X, PANEL_HEADER_LEAD};
    let error = shown_error(ui)?;
    let value = &ui.active().session.revset;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let message = typography::baseline_row(
        [
            Aligned::glyph(
                container(icons::icon(icons::ALERT_TRIANGLE, 12.0, theme.removed_text))
                    .center_x(Length::Fixed(LEADING_ICON_SIZE)),
                12.0,
            ),
            Aligned::gap(LEADING_TEXT_X - LEADING_ICON_INSET - LEADING_ICON_SIZE),
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
    let column_inset = Padding {
        left: LEADING_TEXT_X - LEADING_ICON_INSET,
        ..Padding::ZERO
    };
    let mut body = column![message].spacing(theme::space::SM);
    if let Some(span) = error.span.clone() {
        let room = ui.sidebar_width - PANEL_HEADER_LEAD - LEADING_TEXT_X - PANEL_HEADER_LEAD;
        if let Some(echo) = echo(value, span, room, mono, theme) {
            body = body.push(container(echo).padding(column_inset).clip(true));
        }
    }
    // The way back, beside the only place the error is told.
    if let Some(reset) = crate::empty_states::reset_button(ui, theme) {
        body = body.push(container(reset).padding(column_inset));
    }
    Some(
        container(body)
            .width(Length::Fill)
            .padding(Padding {
                top: 0.0,
                right: PANEL_HEADER_LEAD,
                bottom: theme::space::SM,
                left: PANEL_HEADER_LEAD + LEADING_ICON_INSET,
            })
            .into(),
    )
}

/// The revset on one line, `span` underlined (or a caret where something is
/// missing), cut to what fits in `room` around it.
fn echo<'a>(
    value: &str,
    span: Range<usize>,
    room: f32,
    mono: Font,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    let start = span.start.min(value.len());
    let end = span.end.min(value.len()).max(start);
    if !value.is_char_boundary(start) || !value.is_char_boundary(end) {
        return None;
    }
    // Nothing to underline: a caret marks where something is missing.
    let (line, span) = if start == end {
        const CARET: &str = "\u{2038}";
        (
            format!("{}{CARET}{}", &value[..start], &value[start..]),
            start..start + CARET.len(),
        )
    } else {
        (value.to_owned(), start..end)
    };
    let shown = window(&line, span.clone(), room, |text| {
        crate::measure::line_width(text, text_size::UI, mono)
    });
    let bad = span.start.max(shown.range.start)..span.end.min(shown.range.end);
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    let plain = |text: &str, spans: &mut Vec<Span<'_, ()>>| {
        if !text.is_empty() {
            spans.push(Span::new(text.to_owned()).color(theme.muted_text));
        }
    };
    if shown.lead {
        plain(ELLIPSIS, &mut spans);
    }
    plain(&line[shown.range.start..bad.start], &mut spans);
    spans.push(
        Span::new(line[bad.clone()].to_owned())
            .color(theme.removed_text)
            .underline(end > start),
    );
    plain(&line[bad.end..shown.range.end], &mut spans);
    if shown.trail {
        plain(ELLIPSIS, &mut spans);
    }
    Some(
        rich_text(spans)
            .size(text_size::UI)
            .font(mono)
            .wrapping(Wrapping::None)
            .into(),
    )
}

const ELLIPSIS: &str = "\u{2026}";

/// The part of a one-line `text` to show, so that `span` stays in sight.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Window {
    range: Range<usize>,
    /// An ellipsis stands for what's cut before `range`, and after it.
    lead: bool,
    trail: bool,
}

/// The most of `text` that fits in `room` (as `width` measures it) with
/// `span` in sight. An error near the start keeps the start, one near the
/// end keeps the end, and one in the middle gets context on both sides. A
/// span too wide for the room shows from its start.
fn window(text: &str, span: Range<usize>, room: f32, width: impl Fn(&str) -> f32) -> Window {
    let mut bounds: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    bounds.push(text.len());
    let last = bounds.len() - 1;
    let index = |at: usize| bounds.partition_point(|bound| *bound < at.min(text.len()));
    let (first, past) = (index(span.start), index(span.end));
    let past = past.max(first);
    // A cut next to a space puts the ellipsis there instead of after it.
    let shown = |from: usize, to: usize| {
        let (lead, trail) = (from > 0, to < last);
        let (mut start, mut end) = (bounds[from], bounds[to]);
        if lead {
            start = (start + text[start..end].len() - text[start..end].trim_start().len())
                .min(span.start.max(start));
        }
        if trail {
            end = (start + text[start..end].trim_end().len()).max(span.end.min(end));
        }
        Window {
            range: start..end,
            lead,
            trail,
        }
    };
    let fits = |from: usize, to: usize| {
        let window = shown(from, to);
        let mut line = String::new();
        if window.lead {
            line.push_str(ELLIPSIS);
        }
        line.push_str(&text[window.range]);
        if window.trail {
            line.push_str(ELLIPSIS);
        }
        width(&line) <= room
    };
    // The largest `n` in `low..=high` that `ok` takes, given it takes `low`
    // and takes every smaller `n` it takes a larger one for.
    let widest = |low: usize, high: usize, ok: &dyn Fn(usize) -> bool| {
        let (mut low, mut high) = (low, high);
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if ok(mid) { low = mid } else { high = mid - 1 }
        }
        low
    };
    if fits(0, last) {
        return shown(0, last);
    }
    let head = || fits(0, past).then(|| shown(0, widest(past, last, &|to| fits(0, to))));
    let tail = || {
        fits(first, last).then(|| {
            shown(
                first - widest(0, first, &|cut| fits(first - cut, last)),
                last,
            )
        })
    };
    let near_end = first + past > last;
    let either = if near_end {
        tail().or_else(head)
    } else {
        head().or_else(tail)
    };
    if let Some(window) = either {
        return window;
    }
    if !fits(first, past) {
        let to = widest(first, past, &|to| fits(first, to));
        return shown(first, to.max((first + 1).min(last)));
    }
    // Context on both sides, as much on each as fits.
    let around = |pad: usize| (first.saturating_sub(pad), (past + pad).min(last));
    let pad = widest(0, last, &|pad| {
        let (from, to) = around(pad);
        fits(from, to)
    });
    let (from, to) = around(pad);
    if to < last && fits(from, to + 1) {
        return shown(from, to + 1);
    }
    shown(from, to)
}

#[cfg(test)]
mod tests {
    use super::{ELLIPSIS, Window, window};

    /// What the window shows, one unit of room per char.
    fn shown(text: &str, span: std::ops::Range<usize>, room: f32) -> String {
        let Window { range, lead, trail } =
            window(text, span, room, |line| line.chars().count() as f32);
        let line = format!(
            "{}{}{}",
            if lead { ELLIPSIS } else { "" },
            &text[range],
            if trail { ELLIPSIS } else { "" }
        );
        assert!(
            line.chars().count() as f32 <= room,
            "{line:?} overflows {room}"
        );
        line
    }

    const REVSET: &str = "present(x) | ancestors(immutable_heads().., 2) | trunk()";

    fn span_of(needle: &str) -> std::ops::Range<usize> {
        let at = REVSET.find(needle).unwrap();
        at..at + needle.len()
    }

    #[test]
    fn a_line_that_fits_is_whole() {
        assert_eq!(shown(REVSET, span_of("x"), 100.0), REVSET);
    }

    #[test]
    fn an_error_at_the_start_keeps_the_start() {
        assert_eq!(shown(REVSET, span_of("x"), 20.0), "present(x) | ancest…");
    }

    #[test]
    fn an_error_at_the_end_keeps_the_end() {
        assert_eq!(
            shown(REVSET, span_of("trunk"), 20.0),
            "…s().., 2) | trunk()"
        );
    }

    #[test]
    fn an_error_in_the_middle_gets_context_on_both_sides() {
        let line = shown(REVSET, span_of("immutable_heads"), 25.0);
        assert_eq!(line, "…ors(immutable_heads()..…");
    }

    #[test]
    fn a_span_wider_than_the_room_shows_from_its_start() {
        assert_eq!(
            shown(REVSET, span_of("ancestors(immutable_heads()"), 12.0),
            "…ancestors(…"
        );
        assert_eq!(shown(REVSET, 0..20, 8.0), "present…");
    }

    #[test]
    fn an_empty_span_stays_in_sight() {
        let at = REVSET.find("2)").unwrap();
        let line = shown(REVSET, at..at, 12.0);
        let Window { range, .. } = window(REVSET, at..at, 12.0, |line| line.chars().count() as f32);
        assert!(range.contains(&at), "{line:?}");
        assert!(line.starts_with('…') && line.ends_with('…'), "{line:?}");
    }

    #[test]
    fn an_ellipsis_takes_the_place_of_a_space_at_the_cut() {
        assert_eq!(shown(REVSET, span_of("x"), 14.0), "present(x) |…");
        assert_eq!(shown(REVSET, span_of("trunk"), 11.0), "…| trunk()");
    }

    #[test]
    fn cuts_land_on_char_boundaries() {
        let text = "αβγδε(ζ) | ηθικλμ";
        let at = text.find('ζ').unwrap();
        let line = shown(text, at..at + 'ζ'.len_utf8(), 8.0);
        assert!(line.contains('ζ'), "{line:?}");
    }
}
