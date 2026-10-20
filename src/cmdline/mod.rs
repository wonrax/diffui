//! The palette speaks jj: a ⌘K query that starts with `jj ` is a command
//! line. It parses as the user types (through `diffui_core::cli`), completes
//! the word under the cursor, resolves each revset argument against the
//! repository, tints the rows the focused one matches, and says in the footer
//! what Enter will do. Enter runs it through whatever diffui already has: a
//! view, the target-mode draft, a mutation, an editor, or the jj CLI itself.
//!
//! The line is edited in a one-line `text_editor`: its text, caret and
//! selection come from one layout, the words are coloured by a highlighter
//! over that same layout, and its cursor is the command line's cursor.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use diffui_core::cli::{
    JjCli, MutationCommand, NativeAction, ParseError, ParsedCommand, RevsetSlot, UiCommand,
};
use diffui_core::completion::{Completion, CompletionKind};
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};
use diffui_core::{DraftKind, JobId, MutationOp, PlacementKind, RevisionSelection, mutations};
use iced::Task;
use iced::keyboard::{self, key::Named};

use crate::modes::ModeKind;
use crate::palette;
use crate::{Action, Diffui, MainView, Message, TabId};

mod plan;

pub(crate) use plan::{PlanState, ResolvedView, Run, sentence};

/// Focus target of the command line's editor.
pub(crate) const COMMAND_INPUT_ID: &str = "palette-command-input";

/// What a query starts with to be a command line.
pub(crate) const PREFIX: &str = "jj ";

/// How long typing must pause before the revsets resolve and errors show.
const SETTLE: Duration = Duration::from_millis(160);

/// Previously run commands offered at the top of the list, at most.
const HISTORY_CANDIDATES: usize = 3;

/// Whether an error at `span` is about the word the caret is typing: it
/// touches that word, or only blanks lie between it and the caret (`-s |`).
pub(crate) fn touches_caret(line: &str, cursor: usize, span: &Range<usize>) -> bool {
    let word = diffui_core::cli::tokenize::tokenize(line)
        .into_iter()
        .map(|token| token.span)
        .find(|word| word.start <= cursor && cursor <= word.end)
        .unwrap_or(cursor..cursor);
    let gap = line.get(span.end.min(cursor)..cursor).unwrap_or("x");
    (span.start <= word.end && word.start <= span.end) || gap.trim().is_empty()
}

/// The flag a revset argument was given to, as typed (`-s`, not
/// `--source`); empty for a positional.
pub(crate) fn typed_flag(line: &str, slot: &RevsetSlot) -> String {
    if !slot.flag.starts_with('-') {
        return String::new();
    }
    let tokens = diffui_core::cli::tokenize::tokenize(line);
    let Some(index) = tokens.iter().position(|token| {
        token.span.start <= slot.span.start
            && slot.span.start < token.span.end.max(token.span.start + 1)
    }) else {
        return slot.flag.clone();
    };
    let token = &tokens[index];
    // `--onto=x`, `-ox`: the flag is the token's head.
    if token.value.starts_with('-') {
        let head = &line[token.span.start..slot.span.start];
        return head.trim_end_matches('=').to_owned();
    }
    match index.checked_sub(1).map(|i| &tokens[i]) {
        Some(previous) if previous.value.starts_with('-') => previous.value.clone(),
        _ => slot.flag.clone(),
    }
}

/// The revset arguments of `line`: the parse's, or while the line doesn't
/// parse yet (`jj rebase -r main -o`, a flag still waiting for its value)
/// the ones jj's grammar already places, so they stay coloured, counted and
/// tinted while the next one is typed.
pub(crate) fn revset_slots(
    cli: &JjCli,
    line: &str,
    parsed: &Result<ParsedCommand, ParseError>,
) -> Vec<RevsetSlot> {
    match parsed {
        Ok(parsed) => parsed.revsets.clone(),
        Err(_) => cli.revset_slots(line),
    }
}

/// Whether a palette query is a command line.
pub(crate) fn is_command(query: &str) -> bool {
    query.starts_with(PREFIX)
}

/// Intents the command line adds to the registry.
#[derive(Debug, Clone)]
pub(crate) enum CmdAction {
    /// Open the palette as a command line, `jj ` or the given line typed.
    Open,
    /// From target mode: open it on the draft's own command.
    FromDraft,
    /// The CLI output panel.
    Output(crate::cli_output::OutputAction),
}

/// Widget plumbing for the command line and the revset field.
#[derive(Debug, Clone)]
pub(crate) enum CmdEvent {
    /// Typing paused: resolve the revsets of this version of the line.
    Settle(u64),
    /// The revset field's typing paused.
    FieldSettle(u64),
    /// Hover over a revset field candidate.
    FieldSelect(usize),
    /// Click on a revset field candidate.
    FieldAccept(usize),
}

/// How a revset argument resolved, kept by its text so it survives edits
/// elsewhere on the line.
#[derive(Debug, Clone)]
pub(crate) struct Resolution {
    pub(crate) result: Result<RevsetMatch, RevsetError>,
    /// The matched commit ids, for tinting rows without a scan per row.
    pub(crate) set: HashSet<String>,
}

/// One walk through command history with ↑/↓.
#[derive(Debug, Clone)]
pub(crate) struct HistoryWalk {
    /// Index into the history, newest first.
    pub(crate) index: usize,
    /// The line the walk put in the field: typing anything else ends it.
    pub(crate) line: String,
    /// What was typed before the walk began, restored by ↓ past the newest.
    pub(crate) typed: String,
}

/// The command line's one-line editor. `Content` is neither `Clone` nor
/// worth printing; the palette state is both.
pub(crate) struct LineEditor(pub(crate) iced::widget::text_editor::Content);

impl LineEditor {
    fn new(line: &str, cursor: usize) -> Self {
        let mut editor = Self(iced::widget::text_editor::Content::with_text(line));
        editor.place(cursor);
        editor
    }

    /// The caret's byte offset into the (one-line) text.
    pub(crate) fn cursor(&self) -> usize {
        self.0.cursor().position.index
    }

    /// One line, whatever was pasted.
    pub(crate) fn text(&self) -> String {
        self.0.text().replace(['\n', '\r'], " ")
    }

    fn place(&mut self, cursor: usize) {
        use iced::advanced::text::editor::Cursor;
        self.0.move_to(Cursor {
            position: iced::advanced::text::Position {
                line: 0,
                index: cursor,
            },
            selection: None,
        });
    }

    /// Show `line` with the caret at `cursor`, leaving the editor (and its
    /// undo history) alone when it already does.
    fn sync(&mut self, line: &str, cursor: usize) {
        if self.text() != line {
            *self = Self::new(line, cursor);
        } else if self.cursor() != cursor && self.0.selection().is_none() {
            self.place(cursor);
        }
    }
}

impl Clone for LineEditor {
    fn clone(&self) -> Self {
        Self::new(&self.text(), self.cursor())
    }
}

impl std::fmt::Debug for LineEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LineEditor").field(&self.text()).finish()
    }
}

/// The command-line half of the root palette column.
#[derive(Debug, Clone)]
pub(crate) struct CommandLine {
    /// Where the line is edited.
    pub(crate) editor: LineEditor,
    /// The line as last parsed.
    pub(crate) line: String,
    /// Byte offset of the caret in the line.
    pub(crate) cursor: usize,
    pub(crate) parsed: Result<ParsedCommand, ParseError>,
    /// The revset arguments: the parse's, or while the line doesn't parse
    /// yet, the ones it has so far (see [`revset_slots`]).
    slots: Vec<RevsetSlot>,
    pub(crate) completions: Vec<Completion>,
    pub(crate) selected: usize,
    pub(crate) scroll_y: f32,
    /// ↑/↓ moved through the list since the last edit, so Enter picks the
    /// highlighted candidate instead of running the line.
    pub(crate) navigated: bool,
    /// Typing has paused since the last edit: errors may show.
    pub(crate) settled: bool,
    /// Enter was refused: the error shows even mid-word.
    pub(crate) insisted: bool,
    pub(crate) version: u64,
    pub(crate) resolved: HashMap<String, Resolution>,
    pub(crate) in_flight: Vec<(JobId, String)>,
    pub(crate) history: Option<HistoryWalk>,
    /// Enter came while a revset was still resolving: run once it lands.
    pub(crate) run_when_ready: bool,
}

impl CommandLine {
    fn new(cli: &JjCli, line: &str) -> Self {
        Self {
            editor: LineEditor::new(line, line.len()),
            line: line.to_owned(),
            cursor: line.len(),
            parsed: cli.parse(line),
            slots: Vec::new(),
            completions: Vec::new(),
            selected: 0,
            scroll_y: 0.0,
            navigated: false,
            settled: false,
            insisted: false,
            version: 0,
            resolved: HashMap::new(),
            in_flight: Vec::new(),
            history: None,
            run_when_ready: false,
        }
    }

    /// The revset arguments as typed.
    pub(crate) fn slots(&self) -> &[RevsetSlot] {
        &self.slots
    }

    /// The argument the caret is in (or just after): the one whose rows the
    /// graph tints. Past every argument, the last one before the caret.
    pub(crate) fn focused_slot(&self) -> Option<usize> {
        let slots = self.slots();
        slots
            .iter()
            .position(|slot| slot.span.start <= self.cursor && self.cursor <= slot.span.end)
            .or_else(|| slots.iter().rposition(|slot| slot.span.end <= self.cursor))
    }

    /// The rows to tint, and the flag they answer to as typed.
    pub(crate) fn tint(&self) -> Option<(String, &HashSet<String>)> {
        let slot = self.slots().get(self.focused_slot()?)?;
        let resolution = self.resolved.get(&slot.revset)?;
        resolution
            .result
            .is_ok()
            .then(|| (typed_flag(&self.line, slot), &resolution.set))
    }

    pub(crate) fn resolution(&self, revset: &str) -> Option<&Resolution> {
        self.resolved.get(revset)
    }

    /// Whether an error at `span` should show. Not while typing goes on,
    /// nor for the word under the caret while there are candidates for it:
    /// `tr` on its way to `trunk()` isn't wrong yet. Enter shows it anyway.
    pub(crate) fn error_visible(&self, line: &str, span: Option<&Range<usize>>) -> bool {
        if self.insisted {
            return true;
        }
        if !self.settled {
            return false;
        }
        let Some(span) = span else {
            return true;
        };
        !(!self.completions.is_empty() && touches_caret(line, self.cursor, span))
    }
}

// ── cursor tracking ─────────────────────────────────────────────────────

/// Where the caret is after the line went from `old` to `new`, given it was
/// at `cursor` in `old`. An insert, a backspace and a forward delete all
/// happen at the caret, so those are checked first; anything else (a
/// selection replaced, a caret this side didn't see move) falls back to the
/// end of the changed run.
pub(crate) fn cursor_after_edit(old: &str, new: &str, cursor: usize) -> usize {
    let cursor = floor_boundary(old, cursor.min(old.len()));
    let (head, tail) = old.split_at(cursor);
    if new.len() >= old.len() && new.starts_with(head) && new.ends_with(tail) {
        return new.len() - tail.len();
    }
    if new.len() < old.len() {
        let removed = old.len() - new.len();
        // Backspace: the bytes just before the caret went.
        if cursor >= removed
            && old.is_char_boundary(cursor - removed)
            && new.starts_with(&old[..cursor - removed])
            && new.ends_with(tail)
        {
            return cursor - removed;
        }
        // Forward delete: the bytes just after it.
        if cursor + removed <= old.len()
            && old.is_char_boundary(cursor + removed)
            && new.starts_with(head)
            && new.ends_with(&old[cursor + removed..])
        {
            return cursor;
        }
    }
    let prefix = common_prefix(old, new);
    let limit = old.len().min(new.len()) - prefix;
    let suffix = common_suffix(old, new).min(limit);
    floor_boundary(new, new.len() - suffix)
}

fn common_prefix(a: &str, b: &str) -> usize {
    let mut len = 0;
    for ((i, x), y) in a.char_indices().zip(b.chars()) {
        if x != y {
            return i;
        }
        len = i + x.len_utf8();
    }
    len
}

fn common_suffix(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (x, y) in a.chars().rev().zip(b.chars().rev()) {
        if x != y {
            break;
        }
        len += x.len_utf8();
    }
    len
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Where a caret-moving key puts the caret, mirroring the platform's text
/// field: arrows (and ⌃B/⌃F) by character, ⌥ by word, ⌘ (and Home/End,
/// ⌃A/⌃E) to the ends. `None` for any key that doesn't move it.
pub(crate) fn cursor_after_key(
    line: &str,
    cursor: usize,
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
) -> Option<usize> {
    let cursor = floor_boundary(line, cursor);
    let previous = |at: usize| line[..at].char_indices().last().map_or(0, |(i, _)| i);
    let next = |at: usize| {
        line[at..]
            .chars()
            .next()
            .map_or(line.len(), |c| at + c.len_utf8())
    };
    let word_start = |at: usize| {
        let trimmed = line[..at].trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(0, |(i, c)| i + c.len_utf8())
    };
    let word_end = |at: usize| {
        let rest = &line[at..];
        let skipped = rest.len()
            - rest
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .len();
        let word = &rest[skipped..];
        let len = word
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(word.len(), |(i, _)| i);
        at + skipped + len
    };
    let physical = keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified);
    let cocoa = crate::ui::cocoa_key(key, physical, modifiers, false);
    let (key, modifiers) = cocoa
        .as_ref()
        .map_or((key, modifiers), |(key, modifiers)| (key, *modifiers));
    let command = modifiers.command() || modifiers.logo();
    match key.as_ref() {
        keyboard::Key::Named(Named::ArrowLeft) if command => Some(0),
        keyboard::Key::Named(Named::ArrowRight) if command => Some(line.len()),
        keyboard::Key::Named(Named::ArrowLeft) if modifiers.alt() => Some(word_start(cursor)),
        keyboard::Key::Named(Named::ArrowRight) if modifiers.alt() => Some(word_end(cursor)),
        keyboard::Key::Named(Named::ArrowLeft) => Some(previous(cursor)),
        keyboard::Key::Named(Named::ArrowRight) => Some(next(cursor)),
        keyboard::Key::Named(Named::Home) => Some(0),
        keyboard::Key::Named(Named::End) => Some(line.len()),
        _ => None,
    }
}

/// `line` with `completion` accepted, and where the caret goes.
pub(crate) fn apply_completion(line: &str, completion: &Completion) -> (String, usize) {
    let start = floor_boundary(line, completion.replace.start);
    let end = floor_boundary(line, completion.replace.end.max(start));
    let mut out = String::with_capacity(line.len() + completion.insert.len());
    out.push_str(&line[..start]);
    out.push_str(&completion.insert);
    out.push_str(&line[end..]);
    (out, start + completion.insert.len())
}

/// Focus the command line's editor.
fn focus_command_input() -> Task<Message> {
    iced::widget::operation::focus(iced::widget::Id::new(COMMAND_INPUT_ID))
}

/// The text-input operation that puts the caret at byte `at` of `line`.
pub(crate) fn place_caret(id: &'static str, line: &str, at: usize) -> Task<Message> {
    iced::widget::operation::move_cursor_to(
        iced::widget::Id::new(id),
        iced::advanced::text::Position {
            line: 0,
            index: floor_boundary(line, at),
        },
    )
}

// ── refresh ─────────────────────────────────────────────────────────────

/// History entries that extend what is typed, as candidates for the whole
/// line.
fn history_candidates(line: &str, cursor: usize, history: &[String]) -> Vec<Completion> {
    if cursor != line.len() {
        return Vec::new();
    }
    history
        .iter()
        .filter(|entry| entry.as_str() != line && entry.starts_with(line))
        .take(HISTORY_CANDIDATES)
        .map(|entry| Completion {
            replace: 0..line.len(),
            insert: entry.clone(),
            label: entry.strip_prefix(PREFIX).unwrap_or(entry).to_owned(),
            detail: "Recent".to_owned(),
            kind: CompletionKind::History,
        })
        .collect()
}

impl Diffui {
    pub(crate) fn on_command_line_action(&mut self, action: CmdAction) -> Task<Message> {
        match action {
            CmdAction::Open => self.open_command_line(PREFIX.to_owned()),
            CmdAction::FromDraft => match self.draft_command_line() {
                Some(line) => self.open_command_line(line),
                None => Task::none(),
            },
            CmdAction::Output(action) => self.on_output_action(action),
        }
    }

    pub(crate) fn on_cmd_event(&mut self, event: CmdEvent) -> Task<Message> {
        match event {
            CmdEvent::Settle(version) => self.settle_command_line(version),
            CmdEvent::FieldSettle(version) => self.settle_revset_field(version),
            CmdEvent::FieldSelect(index) => {
                self.revset_field_select(index);
                Task::none()
            }
            CmdEvent::FieldAccept(index) => self.accept_revset_field(index),
        }
    }

    /// The jj grammar and aliases for the active tab's repository, loaded
    /// once per tab.
    pub(crate) fn command_line_cli(&mut self) -> JjCli {
        let state = self.active_mut();
        if state.cmdline.cli.is_none() {
            let cli = state
                .repository
                .as_ref()
                .filter(|repository| repository.vcs == crate::Vcs::Jj)
                .map(|repository| JjCli::for_repo(&repository.root))
                .unwrap_or_default();
            state.cmdline.cli = Some(cli);
        }
        state.cmdline.cli.clone().unwrap_or_default()
    }

    /// Re-parse and re-complete the line in `column` with the caret at
    /// `cursor`. Cheap (well under a millisecond), so it runs per keystroke.
    fn refresh_command_line(&mut self, column: &mut palette::PaletteColumn, cursor: usize) {
        let cli = self.command_line_cli();
        let line = column.query.clone();
        let command = column
            .command
            .get_or_insert_with(|| Box::new(CommandLine::new(&cli, &line)));
        command.cursor = floor_boundary(&line, cursor);
        command.editor.sync(&line, command.cursor);
        command.parsed = cli.parse(&line);
        command.slots = revset_slots(&cli, &line, &command.parsed);
        command.line = line.clone();
        command.insisted = false;
        let history: Vec<String> = self.recents.revsets.iter().cloned().collect();
        let completer = self.active().session.revset_completer(&history);
        let mut completions = history_candidates(
            &line,
            command.cursor,
            &self.recents.cli.iter().cloned().collect::<Vec<_>>(),
        );
        completions.extend(cli.complete(&line, command.cursor, &completer));
        command.completions = completions;
        command.selected = 0;
        command.scroll_y = 0.0;
    }

    /// The palette's query changed. Returns the settle timer for a command
    /// line, or `None` when the query isn't one (the caller runs the normal
    /// search debounce instead).
    pub(crate) fn command_line_edited(
        &mut self,
        column: &mut palette::PaletteColumn,
        old: &str,
    ) -> Option<Task<Message>> {
        if !is_command(&column.query) {
            if let Some(command) = column.command.take() {
                self.cancel_command_jobs(&command);
            }
            return None;
        }
        // Only the search input reports edits here: the line just became a
        // command line, typed at its end.
        let entering = column.command.is_none();
        let cursor = cursor_after_edit(old, &column.query, old.len());
        let task = self.command_line_changed(column, cursor)?;
        Some(if entering {
            Task::batch([task, focus_command_input()])
        } else {
            task
        })
    }

    /// The line is now `column.query`, with the caret at `cursor`: re-parse,
    /// end a history walk typed over, and restart the settle timer.
    fn command_line_changed(
        &mut self,
        column: &mut palette::PaletteColumn,
        cursor: usize,
    ) -> Option<Task<Message>> {
        let walk = column.command.as_ref().and_then(|c| c.history.clone());
        self.refresh_command_line(column, cursor);
        let command = column.command.as_mut()?;
        // Typing over a recalled line ends the walk.
        command.history = walk.filter(|walk| walk.line == column.query);
        command.navigated = false;
        command.settled = false;
        command.run_when_ready = false;
        Some(self.schedule_settle(command))
    }

    /// An edit or a caret move in the command line's editor.
    pub(crate) fn command_line_edit(
        &mut self,
        action: iced::widget::text_editor::Action,
    ) -> Task<Message> {
        let Some(mut state) = self.take_palette() else {
            return Task::none();
        };
        let mut task = Task::none();
        if let Some(column) = state.top_mut()
            && let Some(command) = column.command.as_mut()
        {
            command.editor.0.perform(action);
            let line = command.editor.text();
            let cursor = command.editor.cursor().min(line.len());
            if line == column.query {
                // Only the caret moved: complete what is under it now.
                if cursor != command.cursor {
                    self.refresh_command_line(column, cursor);
                }
            } else {
                column.query = line.clone();
                if is_command(&line) {
                    task = self
                        .command_line_changed(column, cursor)
                        .unwrap_or_else(Task::none);
                } else {
                    // Back to a search: the search input takes over, caret
                    // and all.
                    if let Some(command) = column.command.take() {
                        self.cancel_command_jobs(&command);
                    }
                    column.dirty = true;
                    column.query_version = column.query_version.wrapping_add(1);
                    let version = column.query_version;
                    task = Task::batch([
                        iced::widget::operation::focus(iced::widget::Id::new(
                            palette::PALETTE_INPUT_ID,
                        )),
                        place_caret(palette::PALETTE_INPUT_ID, &line, cursor),
                        Task::done(Message::Palette(palette::PaletteMessage::Recompute(
                            0, version,
                        ))),
                    ]);
                }
            }
        }
        self.push_mode(crate::modes::Mode::Palette(state));
        task
    }

    fn schedule_settle(&self, command: &mut CommandLine) -> Task<Message> {
        command.version = command.version.wrapping_add(1);
        let version = command.version;
        Task::perform(tokio::time::sleep(SETTLE), move |()| {
            Message::CommandLine(CmdEvent::Settle(version))
        })
    }

    /// Typing paused: show errors, and resolve the revsets nobody has yet.
    pub(crate) fn settle_command_line(&mut self, version: u64) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let Some(command) = self.command_line() else {
            return Task::none();
        };
        if command.version != version {
            return Task::none();
        }
        let wanted: Vec<String> = command.slots().iter().map(|slot| slot.revset.clone()).fold(
            Vec::new(),
            |mut kept, revset| {
                if !kept.contains(&revset) {
                    kept.push(revset);
                }
                kept
            },
        );
        let stale: Vec<JobId> = command
            .in_flight
            .iter()
            .filter(|(_, revset)| !wanted.contains(revset))
            .map(|(job, _)| *job)
            .collect();
        let missing: Vec<String> = wanted
            .into_iter()
            .filter(|revset| {
                !command.resolved.contains_key(revset)
                    && !command.in_flight.iter().any(|(_, r)| r == revset)
            })
            .collect();
        let mut sent = Vec::new();
        let mut tasks = Vec::new();
        if let Some(state) = self.tab_mut(tab) {
            for job in &stale {
                state.jobs.cmdline.remove(job);
                if let Some(handle) = &state.handle {
                    handle.send(diffui_core::Command::Cancel { job: *job });
                }
            }
            for revset in missing {
                let (job, effects) = state.session.resolve_revset(RevsetQuery::new(&revset));
                state.jobs.cmdline.insert(job);
                sent.push((job, revset));
                tasks.push(effects);
            }
        }
        let task = Task::batch(
            tasks
                .into_iter()
                .map(|effects| self.run_effects(tab, effects))
                .collect::<Vec<_>>(),
        );
        if let Some(command) = self.command_line_mut() {
            command.settled = true;
            command.in_flight.retain(|(job, _)| !stale.contains(job));
            command.in_flight.extend(sent);
        }
        task
    }

    /// A revset the command line asked about resolved.
    pub(crate) fn command_revset_resolved(
        &mut self,
        tab: TabId,
        job: JobId,
        revset: &str,
        result: Result<RevsetMatch, RevsetError>,
    ) -> Task<Message> {
        if let Some(state) = self.tab_mut(tab) {
            state.jobs.cmdline.remove(&job);
        }
        if !self.is_active(tab) {
            return Task::none();
        }
        let Some(command) = self.command_line_mut() else {
            return Task::none();
        };
        let Some(position) = command.in_flight.iter().position(|(id, _)| *id == job) else {
            return Task::none();
        };
        command.in_flight.remove(position);
        let set = result
            .as_ref()
            .map(|found| found.commits.iter().cloned().collect())
            .unwrap_or_default();
        command
            .resolved
            .insert(revset.to_owned(), Resolution { result, set });
        if command.run_when_ready && command.in_flight.is_empty() {
            command.run_when_ready = false;
            return self.run_command_line();
        }
        Task::none()
    }

    fn cancel_command_jobs(&mut self, command: &CommandLine) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        if let Some(state) = self.tab_mut(tab) {
            for (job, _) in &command.in_flight {
                state.jobs.cmdline.remove(job);
                if let Some(handle) = &state.handle {
                    handle.send(diffui_core::Command::Cancel { job: *job });
                }
            }
        }
    }

    /// The palette's command line, when the palette is open on one.
    pub(crate) fn command_line(&self) -> Option<&CommandLine> {
        match self.palette()?.stack.as_slice() {
            [root] => root.command.as_deref(),
            _ => None,
        }
    }

    fn command_line_mut(&mut self) -> Option<&mut CommandLine> {
        let state = self.palette_mut()?;
        if state.stack.len() != 1 {
            return None;
        }
        state.stack.first_mut()?.command.as_deref_mut()
    }

    /// Open the palette with `line` typed and the caret at its end.
    pub(crate) fn open_command_line(&mut self, line: String) -> Task<Message> {
        let open = self.perform(Action::OpenPalette);
        let Some(mut state) = self.take_palette() else {
            return open;
        };
        let mut settle = Task::none();
        if let Some(column) = state.top_mut() {
            column.query = line.clone();
            column.matches.clear();
            self.refresh_command_line(column, line.len());
            if let Some(command) = column.command.as_mut() {
                settle = self.schedule_settle(command);
            }
        }
        self.push_mode(crate::modes::Mode::Palette(state));
        Task::batch([open, focus_command_input(), settle])
    }

    /// ↑/↓ in command mode: walk history from an untouched line, else move
    /// through the candidates.
    pub(crate) fn command_line_move(&mut self, delta: i32) -> Option<Task<Message>> {
        let command = self.command_line()?;
        let line = self.palette()?.top()?.query.clone();
        let walking = command.history.is_some();
        let untouched = line == PREFIX && !command.navigated;
        if walking || (untouched && delta < 0) {
            return Some(self.walk_history(delta));
        }
        let command = self.command_line_mut()?;
        if command.completions.is_empty() {
            return Some(Task::none());
        }
        let len = command.completions.len() as i32;
        command.selected = (command.selected as i32 + delta).rem_euclid(len) as usize;
        command.navigated = true;
        let top = command.selected as f32 * palette::ROW_HEIGHT;
        let bottom = top + palette::ROW_HEIGHT;
        let viewport = palette::command_results_viewport();
        let y = if top < command.scroll_y {
            top
        } else if bottom > command.scroll_y + viewport {
            bottom - viewport
        } else {
            return Some(Task::none());
        };
        command.scroll_y = y.max(0.0);
        Some(iced::widget::operation::scroll_to(
            // The command line is always the root column.
            palette::results_scrollable_id(0),
            iced::widget::scrollable::AbsoluteOffset {
                x: 0.0,
                y: command.scroll_y,
            },
        ))
    }

    fn walk_history(&mut self, delta: i32) -> Task<Message> {
        let history: Vec<String> = self.recents.cli.iter().cloned().collect();
        let Some(mut state) = self.take_palette() else {
            return Task::none();
        };
        let mut task = Task::none();
        if let Some(column) = state.top_mut()
            && let Some(command) = column.command.as_ref()
        {
            let walk = command.history.clone();
            let typed = walk
                .as_ref()
                .map_or_else(|| column.query.clone(), |walk| walk.typed.clone());
            let next = match (&walk, delta < 0) {
                (None, true) => Some(0),
                (None, false) => None,
                (Some(walk), true) => Some((walk.index + 1).min(history.len().saturating_sub(1))),
                (Some(walk), false) => walk.index.checked_sub(1),
            };
            let (line, walk) = match next.and_then(|index| history.get(index).map(|l| (index, l))) {
                Some((index, line)) => (
                    line.clone(),
                    Some(HistoryWalk {
                        index,
                        line: line.clone(),
                        typed,
                    }),
                ),
                None => (typed, None),
            };
            column.query = line.clone();
            self.refresh_command_line(column, line.len());
            if let Some(command) = column.command.as_mut() {
                command.history = walk;
                command.navigated = false;
                command.settled = false;
                task = self.schedule_settle(command);
            }
        }
        self.push_mode(crate::modes::Mode::Palette(state));
        task
    }

    /// Tab: take the highlighted candidate.
    pub(crate) fn command_line_complete(&mut self) -> Option<Task<Message>> {
        let command = self.command_line()?;
        let completion = command.completions.get(command.selected)?.clone();
        Some(self.accept_completion(completion))
    }

    /// Accept the candidate at `index` (a click).
    pub(crate) fn command_line_accept_index(&mut self, index: usize) -> Task<Message> {
        let Some(completion) = self
            .command_line()
            .and_then(|command| command.completions.get(index))
            .cloned()
        else {
            return Task::none();
        };
        self.accept_completion(completion)
    }

    pub(crate) fn command_line_select(&mut self, index: usize) {
        if let Some(command) = self.command_line_mut()
            && index < command.completions.len()
        {
            command.selected = index;
        }
    }

    fn accept_completion(&mut self, completion: Completion) -> Task<Message> {
        let Some(mut state) = self.take_palette() else {
            return Task::none();
        };
        let mut task = Task::none();
        if let Some(column) = state.top_mut() {
            let (line, cursor) = apply_completion(&column.query, &completion);
            column.query = line.clone();
            self.refresh_command_line(column, cursor);
            if let Some(command) = column.command.as_mut() {
                command.history = None;
                command.navigated = false;
                command.settled = false;
                task = self.schedule_settle(command);
            }
        }
        self.push_mode(crate::modes::Mode::Palette(state));
        task
    }

    /// Enter: take the highlighted candidate when the user went looking for
    /// one or the line can't run as it is; otherwise run the line.
    pub(crate) fn command_line_submit(&mut self) -> Task<Message> {
        let Some(command) = self.command_line() else {
            return Task::none();
        };
        let picked = command.completions.get(command.selected).cloned();
        let runnable = command.parsed.is_ok();
        if let Some(completion) = picked
            && (command.navigated || !runnable)
        {
            return self.accept_completion(completion);
        }
        self.run_command_line()
    }

    /// Run the line, or wait for its revsets and run then.
    pub(crate) fn run_command_line(&mut self) -> Task<Message> {
        let Some(command) = self.command_line() else {
            return Task::none();
        };
        let Some(line) = self
            .palette()
            .and_then(|s| s.top())
            .map(|c| c.query.clone())
        else {
            return Task::none();
        };
        // Errors show at once on Enter, settled or not.
        let pending_settle = !command.settled;
        let version = command.version;
        let plan = self.plan(command);
        match plan.state {
            PlanState::Blocked { .. } => {
                if let Some(command) = self.command_line_mut() {
                    command.settled = true;
                    command.insisted = true;
                }
                Task::none()
            }
            PlanState::Pending => {
                if let Some(command) = self.command_line_mut() {
                    command.run_when_ready = true;
                }
                if pending_settle {
                    self.settle_command_line(version)
                } else {
                    Task::none()
                }
            }
            PlanState::Ready(run) => {
                self.recents.push_cli(line.trim_end().to_owned());
                for slot in self
                    .command_line()
                    .map(|c| c.slots().to_vec())
                    .unwrap_or_default()
                {
                    self.recents.push_revset(slot.revset);
                }
                self.recents.save();
                if let Some(command) = self
                    .palette_mut()
                    .and_then(|state| state.stack.first_mut())
                    .and_then(|column| column.command.take())
                {
                    self.cancel_command_jobs(&command);
                }
                self.pop_mode(ModeKind::Palette);
                self.execute(run)
            }
        }
    }

    fn execute(&mut self, run: Run) -> Task<Message> {
        // What the typed line does replaces a draft in progress; a view
        // leaves it alone.
        if !matches!(run, Run::View(_)) {
            self.active_mut().pop_mode(ModeKind::Draft);
        }
        match run {
            Run::View(view) => self.run_view(view),
            Run::Native { action, command } => self.run_native(action, command),
            Run::Ui { command, target } => self.run_ui(command, target),
            Run::Cli { args } => self.start_cli_run(args),
        }
    }

    fn show_diff_view(&mut self) -> Task<Message> {
        if self.active().main_view == MainView::Diff {
            Task::none()
        } else {
            self.perform(Action::SetMainView(MainView::Diff))
        }
    }

    fn run_view(&mut self, view: ResolvedView) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        match view {
            ResolvedView::Diff(request) => {
                let show = self.show_diff_view();
                let state = self.active_mut();
                state.pending_revision_reveal = true;
                let effects = state.session.diff_revsets(request);
                Task::batch([show, self.run_effects(tab, effects)])
            }
            ResolvedView::Log(revset) => {
                let revset = revset.unwrap_or_else(|| self.active().default_revset.clone());
                self.perform(Action::SetRevset(revset))
            }
            ResolvedView::Evolog(revision) => self.perform(Action::History(
                crate::history_panel::HistoryAction::ShowEvolog(revision),
            )),
            ResolvedView::OpLog => self.perform(Action::History(
                crate::history_panel::HistoryAction::ShowOpLog,
            )),
            ResolvedView::Annotate { revision, path } => {
                let browse = self.perform(Action::BrowseSource {
                    revision,
                    path: Some(path),
                });
                let annotate = if self.active().history.annotating() {
                    Task::none()
                } else {
                    self.perform(Action::History(
                        crate::history_panel::HistoryAction::ToggleAnnotate,
                    ))
                };
                Task::batch([browse, annotate])
            }
        }
    }

    fn run_native(&mut self, action: NativeAction, command: MutationCommand) -> Task<Message> {
        let op = match action {
            NativeAction::Fetch(target) => return self.start_fetch(target),
            NativeAction::Mutate(op) => op,
        };
        if let Some((kind, placement)) = command.draft_kind()
            && let Some(task) = self.draft_from_op(kind, placement, &op)
        {
            return task;
        }
        let allow_backwards = matches!(
            command,
            MutationCommand::BookmarkSet {
                allow_backwards: true,
                ..
            }
        );
        self.start_mutation(op, allow_backwards)
    }

    /// Land a typed rebase / squash / placed new or duplicate / parallelize
    /// in target mode, armed on its destination, so it previews like a picked
    /// one and Enter applies it. `None` when target mode can't hold it (a
    /// destination between two revisions, or one the log doesn't show): the
    /// op then runs as typed.
    fn draft_from_op(
        &mut self,
        kind: DraftKind,
        placement: PlacementKind,
        op: &MutationOp,
    ) -> Option<Task<Message>> {
        use mutations::{Destination, SquashTarget};
        let (sources, destination) = match op {
            MutationOp::Rebase {
                sources,
                destination,
                ..
            } => (sources.clone(), Some(destination)),
            MutationOp::Squash {
                from,
                into: SquashTarget::Revision(into),
            } => {
                let armed = Some((into.clone(), PlacementKind::Onto));
                return self.arm_draft(kind, from.clone(), armed);
            }
            MutationOp::NewAt { destination } => (Vec::new(), Some(destination)),
            MutationOp::DuplicateTo {
                targets,
                destination: Some(destination),
            } => (targets.clone(), Some(destination)),
            MutationOp::Parallelize { targets } => (targets.clone(), None),
            _ => return None,
        };
        let armed = match destination {
            None => None,
            Some(Destination::Between { .. }) => return None,
            Some(
                Destination::Onto(target)
                | Destination::After(target)
                | Destination::Before(target),
            ) => Some((target.clone(), placement)),
        };
        self.arm_draft(kind, sources, armed)
    }

    fn arm_draft(
        &mut self,
        kind: DraftKind,
        sources: Vec<RevisionSelection>,
        armed: Option<(RevisionSelection, PlacementKind)>,
    ) -> Option<Task<Message>> {
        let loaded = |selection: &RevisionSelection| self.draft_source_for(selection).is_some();
        if !sources.iter().all(loaded) || armed.as_ref().is_some_and(|(target, _)| !loaded(target))
        {
            return None;
        }
        let task = self.start_draft_with(kind, sources, armed);
        self.active().op_draft().is_some().then_some(task)
    }

    fn run_ui(&mut self, command: UiCommand, target: RevisionSelection) -> Task<Message> {
        match command {
            UiCommand::Describe { .. } => self.perform(Action::EditDescription {
                target: Some(target),
            }),
            UiCommand::Commit => {
                self.perform(Action::Revops(crate::revops::RevopsAction::BeginCommit))
            }
            UiCommand::Split { .. } => {
                let show = self.show_diff_view();
                Task::batch([show, self.perform(Action::SelectRevision(target))])
            }
            UiCommand::Resolve { paths, .. } => {
                let show = self.show_diff_view();
                let select = self.perform(Action::SelectRevision(target));
                let file = match paths.first() {
                    Some(path) => self.perform(Action::JumpToFile(path.value.clone())),
                    None => Task::none(),
                };
                Task::batch([show, select, file])
            }
        }
    }

    // ── the draft → command bridge ──────────────────────────────────────

    /// The active draft as a command line, exactly as its banner shows it:
    /// the whole command when a destination is armed, else up to where the
    /// destination goes.
    pub(crate) fn draft_command_line(&self) -> Option<String> {
        let ui = self.active().op_draft()?;
        let armed = ui
            .draft
            .candidate
            .and_then(|index| self.selection_at_index(index))
            .filter(|target| !self.draft_blocks_target(target))
            .and_then(|target| ui.draft.op_for(target, ui.draft.placement));
        let rendered = crate::plan::draft_command(self, &ui.draft, armed.as_ref())?;
        let rendered = rendered.strip_prefix(PREFIX).unwrap_or(&rendered);
        Some(format!("{PREFIX}{rendered}"))
    }
}

/// The tab state the command line keeps per repository.
#[derive(Debug, Default)]
pub(crate) struct TabCommandLine {
    pub(crate) cli: Option<JjCli>,
    pub(crate) runs: Vec<crate::cli_output::CliRun>,
    pub(crate) field: crate::revset_assist::FieldAssist,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_moves_the_caret_with_the_text() {
        assert_eq!(cursor_after_edit("jj re", "jj reb", 5), 6);
        // Inserting mid-line, where the caret is.
        assert_eq!(
            cursor_after_edit("jj rebase -o x", "jj rebase -s  -o x", 10),
            14
        );
        // A run of equal characters: the caret, not the end of the run.
        assert_eq!(cursor_after_edit("jj aa", "jj aaa", 4), 5);
        // Backspace and forward delete.
        assert_eq!(cursor_after_edit("jj abc", "jj ac", 5), 4);
        assert_eq!(cursor_after_edit("jj abc", "jj ac", 4), 4);
        // A paste.
        assert_eq!(cursor_after_edit("jj ", "jj log -r @", 3), 11);
        // An edit somewhere the caret wasn't: the end of the changed run.
        assert_eq!(cursor_after_edit("jj log", "jj blog", 6), 4);
    }

    #[test]
    fn caret_keys_move_like_a_text_field() {
        let line = "jj rebase -s abc";
        let key = |named| keyboard::Key::Named(named);
        let none = keyboard::Modifiers::empty();
        assert_eq!(
            cursor_after_key(line, 16, &key(Named::ArrowLeft), none),
            Some(15)
        );
        assert_eq!(
            cursor_after_key(line, 16, &key(Named::ArrowRight), none),
            Some(16)
        );
        assert_eq!(
            cursor_after_key(line, 16, &key(Named::ArrowLeft), keyboard::Modifiers::ALT),
            Some(13)
        );
        assert_eq!(
            cursor_after_key(line, 3, &key(Named::ArrowRight), keyboard::Modifiers::ALT),
            Some(9)
        );
        assert_eq!(
            cursor_after_key(line, 9, &key(Named::ArrowLeft), keyboard::Modifiers::LOGO),
            Some(0)
        );
        assert_eq!(cursor_after_key(line, 2, &key(Named::End), none), Some(16));
        assert_eq!(
            cursor_after_key(line, 2, &keyboard::Key::Character("x".into()), none),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn caret_follows_cocoa_chords() {
        let line = "jj rebase -s abc";
        let ctrl = |letter: &str| {
            (
                keyboard::Key::Character(letter.into()),
                keyboard::Modifiers::CTRL,
            )
        };
        for (letter, from, to) in [
            ("a", 9, Some(0)),
            ("e", 2, Some(16)),
            ("b", 9, Some(8)),
            ("f", 9, Some(10)),
            ("k", 9, None),
        ] {
            let (key, modifiers) = ctrl(letter);
            assert_eq!(
                cursor_after_key(line, from, &key, modifiers),
                to,
                "⌃{letter}"
            );
        }
    }

    #[test]
    fn accepting_a_candidate_splices_it_in() {
        let completion = Completion {
            replace: 13..15,
            insert: "trunk()".to_owned(),
            label: "trunk()".to_owned(),
            detail: String::new(),
            kind: CompletionKind::Function,
        };
        let (line, cursor) = apply_completion("jj rebase -o tr -s x", &completion);
        assert_eq!(line, "jj rebase -o trunk() -s x");
        assert_eq!(cursor, 20);
    }

    #[test]
    fn flags_read_as_typed() {
        let slot = |flag: &str, span: Range<usize>| RevsetSlot {
            flag: flag.to_owned(),
            revset: String::new(),
            span,
        };
        let line = "jj rebase -s abc --onto=trunk() -dxyz";
        assert_eq!(typed_flag(line, &slot("--source", 13..16)), "-s");
        assert_eq!(typed_flag(line, &slot("--onto", 24..31)), "--onto");
        assert_eq!(typed_flag(line, &slot("--destination", 34..37)), "-d");
        // A positional has no flag to show.
        assert_eq!(typed_flag("jj new abc", &slot("REVSETS", 7..10)), "");
    }

    #[test]
    fn an_error_at_the_caret_waits_for_the_word_to_finish() {
        // Typing `tr` on its way to `trunk()`.
        assert!(touches_caret("jj new tr", 9, &(7..9)));
        // `-s` still waiting for its value.
        assert!(touches_caret("jj rebase -s ", 13, &(10..12)));
        // An error earlier in the line is the user's to see.
        assert!(!touches_caret("jj rebase -s nope -o x", 22, &(13..17)));
    }

    #[test]
    fn arguments_keep_their_colour_while_the_next_flag_waits_for_a_value() {
        let cli = JjCli::default();
        let slots = |line: &str| {
            let parsed = cli.parse(line);
            revset_slots(&cli, line, &parsed)
                .into_iter()
                .map(|slot| (slot.revset, slot.span))
                .collect::<Vec<_>>()
        };
        // Before `-o`, while it waits for a value, and once it has one.
        assert_eq!(
            slots("jj rebase -r main"),
            vec![("main".to_owned(), 13..17)]
        );
        assert_eq!(
            slots("jj rebase -r main -o"),
            vec![("main".to_owned(), 13..17)]
        );
        assert_eq!(
            slots("jj rebase -r main -o x"),
            vec![("main".to_owned(), 13..17), ("x".to_owned(), 21..22)]
        );
        // And the colours follow: `main` is still a revset in the roles.
        let line = "jj rebase -r main -o";
        let mut command = CommandLine::new(&cli, line);
        command.slots = revset_slots(&cli, line, &command.parsed);
        assert!(crate::cmdline_view::revset_roles(line, &command).contains(&(13..17)));
    }

    #[test]
    fn the_line_editor_holds_the_line_and_its_caret() {
        let mut editor = LineEditor::new("jj rebase -s x", 9);
        assert_eq!(editor.text(), "jj rebase -s x");
        assert_eq!(editor.cursor(), 9);
        // A new line from outside (a completion, history) moves the caret
        // with it; the same line just moves the caret.
        editor.sync("jj rebase -s xyz -o ", 20);
        assert_eq!(
            (editor.text().as_str(), editor.cursor()),
            ("jj rebase -s xyz -o ", 20)
        );
        editor.sync("jj rebase -s xyz -o ", 13);
        assert_eq!(editor.cursor(), 13);
        // A paste with a newline stays one line.
        let pasted = LineEditor::new("jj log\n-r @", 0);
        assert_eq!(pasted.text(), "jj log -r @");
    }

    #[test]
    fn history_offers_lines_that_extend_the_typed_one() {
        let history = vec![
            "jj rebase -s a -o b".to_owned(),
            "jj log".to_owned(),
            "jj rebase -s c -o d".to_owned(),
        ];
        let found = history_candidates("jj reb", 6, &history);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].insert, "jj rebase -s a -o b");
        assert_eq!(found[0].replace, 0..6);
        // Not with the caret mid-line.
        assert!(history_candidates("jj reb", 4, &history).is_empty());
    }
}
