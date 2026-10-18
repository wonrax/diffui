//! Partial changes in the diff pane: picking files and hunks with checkboxes
//! and acting on the pick — split, squash into the parent, move into a
//! picked revision, discard.
//!
//! The pick lives per tab and belongs to the revision it was made in; showing
//! another revision drops it. Hunks are remembered by their header, so a
//! reload that leaves a hunk where it was keeps it picked, and one that moves
//! it silently unpicks it (the backend refuses stale headers either way).
//!
//! Everything lands as an ordinary [`MutationOp`] through the same activity /
//! immutable-guard pipeline every other mutation uses.

use std::collections::{BTreeMap, BTreeSet};

use diffui_core::{
    DiffFile, DiffFileStatus, DiffSelection, FileSelection, HunkPick, HunkRef, MutationOp,
    RevisionSelection, SquashTarget,
};
use iced::widget::{Space, column, container, row, text, text_editor};
use iced::{Element, Length, Padding, Task, alignment};

use crate::commands::CommandArg;
use crate::diff_view::{ChangeMarks, CheckState, FileMark, HunkMarks};
use crate::palette::ResultRef;
use crate::theme::{ThemeSpec, text_size};
use crate::{
    ConfirmDialog, Diffui, MainView, Message, PendingMutation, TabState, activity, icons, ui,
};

/// A change-selection intent or event.
#[derive(Debug, Clone)]
pub(crate) enum ChangesMsg {
    /// Toggle a file (document index) whole.
    ToggleFile(usize),
    /// Toggle one hunk: `(file, hunk)` document indices.
    ToggleHunk(usize, usize),
    /// Toggle a changed-files tree row (display index); a directory toggles
    /// every file under it.
    ToggleTreeRow(usize),
    Clear,
    SquashIntoParent,
    /// Pick a revision to move the selection into (target mode).
    MoveInto,
    /// Discard the selection (asks first).
    Discard,
    /// Open the split prompt for the selection.
    OpenSplit,
    SplitFirstEdited(text_editor::Action),
    SplitSecondEdited(text_editor::Action),
    SplitConfirm,
    SplitCancel,
    // ── One file, from its context menu or the palette ──────────────────
    RestoreFile(String),
    MoveFile(String),
    /// Split this file's changes off into a new change.
    SplitFile(String),
    /// Conflict resolution — see [`crate::conflicts`].
    Conflict(crate::conflicts::ConflictMsg),
}

/// One file's pick.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Whole,
    /// Hunk headers.
    Hunks(BTreeSet<String>),
}

/// The split prompt: which selection, and the two descriptions.
#[derive(Debug, Clone)]
pub(crate) struct SplitPrompt {
    revision: RevisionSelection,
    selection: DiffSelection,
    /// What the prompt is splitting, for its title.
    title: String,
    original: String,
    first: text_editor::Content,
    second: text_editor::Content,
}

/// Per-tab change-selection state.
#[derive(Debug, Default)]
pub(crate) struct ChangesState {
    /// The revision the picks were made in.
    revision: Option<RevisionSelection>,
    picks: BTreeMap<String, Pick>,
    split: Option<SplitPrompt>,
    pub(crate) conflicts: crate::conflicts::ConflictsState,
}

impl ChangesState {
    /// Whether the split prompt's editors are open (and so own plain keys).
    pub(crate) fn owns_keyboard(&self) -> bool {
        self.split.is_some()
    }

    /// The picks, if they belong to `revision`.
    fn picks_for(&self, revision: &RevisionSelection) -> Option<&BTreeMap<String, Pick>> {
        (self.revision.as_ref() == Some(revision) && !self.picks.is_empty()).then_some(&self.picks)
    }

    /// Start editing picks for `revision`, dropping any made elsewhere.
    fn picks_mut(&mut self, revision: &RevisionSelection) -> &mut BTreeMap<String, Pick> {
        if self.revision.as_ref() != Some(revision) {
            self.revision = Some(revision.clone());
            self.picks.clear();
        }
        &mut self.picks
    }

    fn toggle_file(&mut self, revision: &RevisionSelection, file: &DiffFile) {
        let picks = self.picks_mut(revision);
        if picks.remove(&file.path).is_none() {
            picks.insert(file.path.clone(), Pick::Whole);
        }
    }

    /// Toggle one hunk. A file whose every hunk ends up picked is picked
    /// whole; one with none left is dropped.
    fn toggle_hunk(&mut self, revision: &RevisionSelection, file: &DiffFile, hunk: usize) {
        let Some(header) = file.hunks.get(hunk).map(|hunk| hunk.header.clone()) else {
            return;
        };
        let all: BTreeSet<String> = file.hunks.iter().map(|hunk| hunk.header.clone()).collect();
        let picks = self.picks_mut(revision);
        let mut headers = match picks.remove(&file.path) {
            None => BTreeSet::new(),
            Some(Pick::Whole) => all.clone(),
            Some(Pick::Hunks(headers)) => headers,
        };
        if !headers.remove(&header) {
            headers.insert(header);
        }
        headers.retain(|header| all.contains(header));
        if headers.is_empty() {
            return;
        }
        let pick = if headers == all {
            Pick::Whole
        } else {
            Pick::Hunks(headers)
        };
        picks.insert(file.path.clone(), pick);
    }

    /// Toggle every file under `prefix` (`"dir/"`): all picked whole goes
    /// to none, anything else to all.
    fn toggle_dir(&mut self, revision: &RevisionSelection, files: &[DiffFile], prefix: &str) {
        let paths: Vec<&str> = files
            .iter()
            .filter(|file| file.path.starts_with(prefix))
            .map(|file| file.path.as_str())
            .collect();
        let picks = self.picks_mut(revision);
        let all_whole = paths
            .iter()
            .all(|path| picks.get(*path) == Some(&Pick::Whole));
        for path in paths {
            if all_whole {
                picks.remove(path);
            } else {
                picks.insert(path.to_owned(), Pick::Whole);
            }
        }
    }

    /// The picks for `revision` as the backend's selection, in `files`
    /// order. Hunks that left the document are dropped.
    fn selection(&self, revision: &RevisionSelection, files: &[DiffFile]) -> DiffSelection {
        let Some(picks) = self.picks_for(revision) else {
            return DiffSelection::default();
        };
        let mut selected = Vec::new();
        for file in files {
            let Some(pick) = picks.get(&file.path) else {
                continue;
            };
            let hunks = match pick {
                Pick::Whole => HunkPick::All,
                Pick::Hunks(headers) => {
                    let refs: Vec<HunkRef> = file
                        .hunks
                        .iter()
                        .enumerate()
                        .filter(|(_, hunk)| headers.contains(&hunk.header))
                        .map(|(index, hunk)| HunkRef {
                            index,
                            header: hunk.header.clone(),
                        })
                        .collect();
                    if refs.is_empty() {
                        continue;
                    }
                    HunkPick::Hunks(refs)
                }
            };
            selected.push(FileSelection {
                path: file.path.clone(),
                hunks,
            });
        }
        DiffSelection { files: selected }
    }
}

/// Whether the selected revision is one partial changes can act on at all.
pub(crate) fn editable(tab: &TabState) -> bool {
    tab.session.capabilities.mutate
        && tab.repository.is_some()
        && tab.main_view == MainView::Diff
        && !tab.session.document.files.is_empty()
}

/// Whether `file` can be picked hunk by hunk — a plain text change. Binary
/// and conflicted files, and mode-only changes (no hunks), go whole.
fn hunk_pickable(file: &DiffFile, conflicted: bool) -> bool {
    !conflicted
        && file.status != DiffFileStatus::Conflicted
        && !file.hunks.is_empty()
        && file
            .hunks
            .iter()
            .all(|hunk| hunk.header.starts_with("@@") && !hunk.lines.is_empty())
}

fn file_check(file: &DiffFile, pick: Option<&Pick>) -> CheckState {
    match pick {
        None => CheckState::Off,
        Some(Pick::Whole) => CheckState::On,
        Some(Pick::Hunks(headers)) => {
            let picked = file
                .hunks
                .iter()
                .filter(|hunk| headers.contains(&hunk.header))
                .count();
            match picked {
                0 => CheckState::Off,
                n if n == file.hunks.len() => CheckState::On,
                _ => CheckState::Partial,
            }
        }
    }
}

/// The current pick of `tab` as the backend's selection.
fn selection_of(tab: &TabState) -> DiffSelection {
    tab.changes
        .selection(&tab.session.selected_revision, &tab.session.document.files)
}

/// Short display id of the selected revision ("k1x2…" / "@").
fn revision_label(tab: &TabState, revision: &RevisionSelection) -> String {
    let row = match revision {
        RevisionSelection::WorkingCopy => tab.session.commits.working_copy(),
        RevisionSelection::Commit(hex) => tab.session.commits.find_by_commit_id(hex),
    };
    match row {
        Some(row) => row.change_id().chars().take(8).collect(),
        None => match revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(hex) => hex.chars().take(8).collect(),
        },
    }
}

fn whole_file(path: &str) -> DiffSelection {
    DiffSelection {
        files: vec![FileSelection {
            path: path.to_owned(),
            hunks: HunkPick::All,
        }],
    }
}

/// A description as jj stores it: trailing whitespace trimmed, one newline.
fn stored_description(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

// ── What the registry and the views read ────────────────────────────────

/// Whether the active tab has a non-empty pick.
pub(crate) fn has_selection(ui: &Diffui) -> bool {
    editable(ui.active()) && !selection_of(ui.active()).is_empty()
}

// ── Command-registry helpers ────────────────────────────────────────────

/// The document index of the file a command names — its path argument (a
/// menu row, a palette file result), else the selected file — when partial
/// changes are possible here.
pub(crate) fn command_file(ui: &Diffui, arg: &CommandArg) -> Option<usize> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let files = &tab.session.document.files;
    match arg {
        CommandArg::Path(path) | CommandArg::Result(ResultRef::File(path)) => {
            files.iter().position(|file| file.path == *path)
        }
        CommandArg::None => files.get(tab.selected_file).map(|_| tab.selected_file),
        _ => None,
    }
}

pub(crate) fn command_file_path(ui: &Diffui, arg: &CommandArg) -> Option<String> {
    let index = command_file(ui, arg)?;
    Some(ui.active().session.document.files[index].path.clone())
}

/// Checkbox marks for the diff view, or `None` when nothing can be picked.
pub(crate) fn diff_marks(ui: &Diffui, theme: ThemeSpec) -> Option<ChangeMarks<Message>> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let picks = tab.changes.picks_for(&tab.session.selected_revision);
    let files = tab
        .session
        .document
        .files
        .iter()
        .map(|file| {
            let pick = picks.and_then(|picks| picks.get(&file.path));
            let conflicted = crate::conflicts::is_conflicted(tab, &file.path);
            let hunks = if !hunk_pickable(file, conflicted) {
                HunkMarks::None
            } else {
                match pick {
                    None => HunkMarks::Unpicked,
                    Some(Pick::Whole) => HunkMarks::Picked(vec![true; file.hunks.len()]),
                    Some(Pick::Hunks(headers)) => HunkMarks::Picked(
                        file.hunks
                            .iter()
                            .map(|hunk| headers.contains(&hunk.header))
                            .collect(),
                    ),
                }
            };
            FileMark {
                state: file_check(file, pick),
                hunks,
            }
        })
        .collect();
    Some(ChangeMarks {
        files,
        accent: theme.accent,
        on_toggle_file: |file| Message::Changes(ChangesMsg::ToggleFile(file)),
        on_toggle_hunk: |file, hunk| Message::Changes(ChangesMsg::ToggleHunk(file, hunk)),
    })
}

/// The changed-files tree checkbox for a row: a file's own state, or for a
/// directory (`dir_prefix` = `"path/"`) the aggregate of the files under it.
/// `None` when nothing can be picked.
pub(crate) fn tree_check(
    ui: &Diffui,
    file_index: Option<usize>,
    dir_prefix: Option<&str>,
) -> Option<CheckState> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let files = &tab.session.document.files;
    let picks = tab.changes.picks_for(&tab.session.selected_revision);
    if let Some(index) = file_index {
        let file = files.get(index)?;
        return Some(file_check(
            file,
            picks.and_then(|picks| picks.get(&file.path)),
        ));
    }
    let prefix = dir_prefix?;
    let Some(picks) = picks else {
        return Some(CheckState::Off);
    };
    if !picks.keys().any(|path| path.starts_with(prefix)) {
        return Some(CheckState::Off);
    }
    let mut all = true;
    for file in files.iter().filter(|file| file.path.starts_with(prefix)) {
        if file_check(file, picks.get(&file.path)) != CheckState::On {
            all = false;
            break;
        }
    }
    Some(if all {
        CheckState::On
    } else {
        CheckState::Partial
    })
}

// ── Update ──────────────────────────────────────────────────────────────

impl Diffui {
    pub(crate) fn on_changes(&mut self, msg: ChangesMsg) -> Task<Message> {
        match msg {
            ChangesMsg::ToggleFile(index) => {
                self.toggle_file(index);
                Task::none()
            }
            ChangesMsg::ToggleHunk(file, hunk) => {
                self.toggle_hunk(file, hunk);
                Task::none()
            }
            ChangesMsg::ToggleTreeRow(display) => {
                self.toggle_tree_row(display);
                Task::none()
            }
            ChangesMsg::Clear => {
                let tab = self.active_mut();
                tab.changes.picks.clear();
                tab.changes.split = None;
                Task::none()
            }
            ChangesMsg::SquashIntoParent => {
                let Some((revision, selection)) = self.current_selection() else {
                    return Task::none();
                };
                self.start_changes_op(MutationOp::SquashChanges {
                    from: revision,
                    selection,
                    into: SquashTarget::Parent,
                })
            }
            ChangesMsg::MoveInto => {
                let Some((revision, selection)) = self.current_selection() else {
                    return Task::none();
                };
                self.move_into(revision, selection)
            }
            ChangesMsg::Discard => {
                let Some((revision, selection)) = self.current_selection() else {
                    return Task::none();
                };
                self.confirm_discard(revision, selection)
            }
            ChangesMsg::OpenSplit => {
                let Some((revision, selection)) = self.current_selection() else {
                    return Task::none();
                };
                let title = format!("Keep {} here, move the rest", selection.summary());
                self.open_split(revision, selection, title);
                Task::none()
            }
            ChangesMsg::SplitFirstEdited(action) => {
                if let Some(prompt) = self.active_mut().changes.split.as_mut() {
                    prompt.first.perform(action);
                }
                Task::none()
            }
            ChangesMsg::SplitSecondEdited(action) => {
                if let Some(prompt) = self.active_mut().changes.split.as_mut() {
                    prompt.second.perform(action);
                }
                Task::none()
            }
            ChangesMsg::SplitCancel => {
                self.active_mut().changes.split = None;
                Task::none()
            }
            ChangesMsg::SplitConfirm => {
                let Some(prompt) = self.active_mut().changes.split.take() else {
                    return Task::none();
                };
                if prompt.revision != self.active().session.selected_revision {
                    return Task::none();
                }
                let first = stored_description(&prompt.first.text());
                let first_description =
                    (first != stored_description(&prompt.original)).then_some(first);
                self.start_changes_op(MutationOp::Split {
                    target: prompt.revision,
                    selection: prompt.selection,
                    first_description,
                    second_description: stored_description(&prompt.second.text()),
                })
            }
            ChangesMsg::RestoreFile(path) => {
                if !editable(self.active()) {
                    return Task::none();
                }
                let revision = self.active().session.selected_revision.clone();
                self.confirm_discard(revision, whole_file(&path))
            }
            ChangesMsg::MoveFile(path) => {
                if !editable(self.active()) {
                    return Task::none();
                }
                let revision = self.active().session.selected_revision.clone();
                self.move_into(revision, whole_file(&path))
            }
            ChangesMsg::SplitFile(path) => {
                let tab = self.active();
                if !editable(tab) {
                    return Task::none();
                }
                // Everything but this file stays; the file becomes the new
                // change (`jj split` keeps the selection in the original).
                let rest: Vec<FileSelection> = tab
                    .session
                    .document
                    .files
                    .iter()
                    .filter(|file| file.path != path)
                    .map(|file| FileSelection {
                        path: file.path.clone(),
                        hunks: HunkPick::All,
                    })
                    .collect();
                if rest.is_empty() {
                    self.push_error_toast(
                        "Nothing to split".to_owned(),
                        &format!(
                            "{path} is the only change in this revision — there is nothing \
                             to leave behind"
                        ),
                    );
                    return Task::none();
                }
                let revision = tab.session.selected_revision.clone();
                self.open_split(
                    revision,
                    DiffSelection { files: rest },
                    format!("Move {path} into a new change"),
                );
                Task::none()
            }
            ChangesMsg::Conflict(message) => self.on_conflict(message),
        }
    }

    /// The active tab's revision and non-empty pick.
    fn current_selection(&self) -> Option<(RevisionSelection, DiffSelection)> {
        let tab = self.active();
        if !editable(tab) {
            return None;
        }
        let selection = selection_of(tab);
        (!selection.is_empty()).then(|| (tab.session.selected_revision.clone(), selection))
    }

    fn toggle_file(&mut self, index: usize) {
        let tab = self.active_mut();
        let revision = tab.session.selected_revision.clone();
        if let Some(file) = tab.session.document.files.get(index) {
            tab.changes.toggle_file(&revision, file);
        }
    }

    fn toggle_hunk(&mut self, file_index: usize, hunk_index: usize) {
        let tab = self.active_mut();
        let revision = tab.session.selected_revision.clone();
        if let Some(file) = tab.session.document.files.get(file_index) {
            tab.changes.toggle_hunk(&revision, file, hunk_index);
        }
    }

    fn toggle_tree_row(&mut self, display: usize) {
        let tab = self.active_mut();
        let rows = diffui_core::file_tree_rows(&tab.session.document.files, &tab.collapsed_dirs);
        let revision = tab.session.selected_revision.clone();
        match rows.get(display) {
            Some(diffui_core::FileTreeRow::File { file_index, .. }) => {
                if let Some(file) = tab.session.document.files.get(*file_index) {
                    tab.changes.toggle_file(&revision, file);
                }
            }
            Some(diffui_core::FileTreeRow::Dir { path, .. }) => {
                let prefix = format!("{}/", path.trim_end_matches('/'));
                tab.changes
                    .toggle_dir(&revision, &tab.session.document.files, &prefix);
            }
            None => {}
        }
    }

    fn open_split(&mut self, revision: RevisionSelection, selection: DiffSelection, title: String) {
        let tab = self.active_mut();
        let original = tab
            .session
            .revision_details
            .as_ref()
            .map(|details| details.description.clone())
            .unwrap_or_default();
        // `jj split`: the selected part keeps the description; the rest gets
        // the same one to edit, or stays empty when there was none.
        tab.changes.split = Some(SplitPrompt {
            revision,
            selection,
            title,
            first: text_editor::Content::with_text(original.trim_end()),
            second: text_editor::Content::with_text(original.trim_end()),
            original,
        });
    }

    /// Target mode for moving `selection` out of `revision`.
    fn move_into(
        &mut self,
        revision: RevisionSelection,
        selection: DiffSelection,
    ) -> Task<Message> {
        let task = self.start_draft(diffui_core::DraftKind::Squash, revision);
        if let Some(draft) = self.active_mut().op_draft_mut() {
            draft.draft.selection = Some(Box::new(selection));
        }
        task
    }

    /// Discarding destroys changes: ask first, and say how to get them back.
    fn confirm_discard(
        &mut self,
        revision: RevisionSelection,
        selection: DiffSelection,
    ) -> Task<Message> {
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        let summary = selection.summary();
        let label = revision_label(self.active(), &revision);
        let (activity_id, _) = self.begin_activity(tab_id, format!("Discard {summary}"), false);
        if let Some(log) = self.activity_log_for(tab_id) {
            log.set_status(activity_id, activity::ActivityStatus::Queued);
        }
        self.push_mode(crate::modes::Mode::Confirm(ConfirmDialog {
            title: format!("Discard {summary}?"),
            body: format!(
                "This puts the parent's content back for the selected changes in {label} \
                 (jj restore). The changes leave the revision, but nothing is lost for good: \
                 Undo in the toolbar or the activity log brings them back."
            ),
            confirm_label: "Discard".to_owned(),
            pending: PendingMutation {
                op: MutationOp::RestoreChanges {
                    target: revision,
                    selection,
                },
                tab_id,
                activity_id,
                allow_immutable: false,
            },
        }));
        Task::none()
    }

    /// Run a partial-change op on the active tab, dropping the pick it used.
    pub(crate) fn start_changes_op(&mut self, op: MutationOp) -> Task<Message> {
        let state = self.active_mut();
        state.changes.picks.clear();
        state.changes.split = None;
        self.start_mutation_op(op)
    }
}

// ── Views ───────────────────────────────────────────────────────────────

/// The floating bar over the diff: the pick's summary and its actions, or
/// the split prompt. `None` (nothing drawn) when nothing is picked.
pub(crate) fn selection_bar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Option<Element<'a, Message>> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let font = ui.config.ui_font;
    if let Some(prompt) = tab.changes.split.as_ref()
        && prompt.revision == tab.session.selected_revision
    {
        return Some(floating(split_prompt(ui, prompt, theme), theme));
    }
    let selection = selection_of(tab);
    if selection.is_empty() {
        return None;
    }
    let button = |label: &'static str, msg: ChangesMsg, variant: ui::Variant| {
        ui::text_button(None, label, variant, ui::Size::Compact, font, theme)
            .on_press(Message::Changes(msg))
    };
    let summary = row![
        icons::icon(icons::CHECK, ui::ICON_SIZE, theme.accent),
        text(format!("{} selected", selection.summary()))
            .size(text_size::UI)
            .font(font)
            .color(theme.text),
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    // Two lines so the bar fits a narrow diff pane: what is picked (and the
    // clear button), then the actions, wrapping when they have to.
    let header = row![
        summary,
        Space::new().width(Length::Fill),
        ui::tooltip(
            ui::icon_button(icons::CLOSE, theme.muted_text, ui::Size::Compact, theme)
                .on_press(Message::Changes(ChangesMsg::Clear)),
            "Clear selection",
            iced::widget::tooltip::Position::Top,
            font,
            theme,
        ),
    ]
    .align_y(alignment::Vertical::Center);
    let actions = row![
        button("Split\u{2026}", ChangesMsg::OpenSplit, ui::Variant::Raised),
        button(
            "Squash into parent",
            ChangesMsg::SquashIntoParent,
            ui::Variant::Raised
        ),
        button(
            "Move into\u{2026}",
            ChangesMsg::MoveInto,
            ui::Variant::Raised
        ),
        button("Discard\u{2026}", ChangesMsg::Discard, ui::Variant::Raised),
    ]
    .spacing(6)
    .wrap()
    .vertical_spacing(6);
    let bar = column![header, actions]
        .spacing(4)
        .width(Length::Fill.max(420));
    Some(floating(bar.into(), theme))
}

/// Bottom-centered floating card.
fn floating<'a>(content: Element<'a, Message>, theme: ThemeSpec) -> Element<'a, Message> {
    container(
        container(content)
            .padding(Padding::from([8.0, 12.0]))
            .style(move |_| crate::theme::popover_style(theme)),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(alignment::Horizontal::Center)
    .align_y(alignment::Vertical::Bottom)
    .padding(16)
    .into()
}

fn split_prompt<'a>(
    ui: &'a Diffui,
    prompt: &'a SplitPrompt,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let font = ui.config.ui_font;
    let tab = ui.active();
    let label = revision_label(tab, &prompt.revision);
    let caption = |content: String| {
        text(content)
            .size(text_size::CAPTION)
            .font(font)
            .color(theme.muted_text)
    };
    let editor = |content: &'a text_editor::Content,
                  placeholder: &'static str,
                  on_edit: fn(text_editor::Action) -> ChangesMsg| {
        text_editor(content)
            .placeholder(placeholder)
            .size(ui.config.code_type.size)
            .font(ui.config.mono_font)
            .height(Length::Fixed(64.0))
            .padding(Padding::from([6.0, 8.0]))
            .on_action(move |action| Message::Changes(on_edit(action)))
    };
    let actions = row![
        Space::new().width(Length::Fill),
        ui::text_button(
            None,
            "Cancel",
            ui::Variant::Ghost,
            ui::Size::Compact,
            font,
            theme
        )
        .on_press(Message::Changes(ChangesMsg::SplitCancel)),
        ui::text_button(
            None,
            "Split",
            ui::Variant::Primary,
            ui::Size::Compact,
            font,
            theme
        )
        .on_press(Message::Changes(ChangesMsg::SplitConfirm)),
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    column![
        text(format!("Split {label}: {}", prompt.title))
            .size(text_size::UI)
            .font(crate::theme::emphasis_font(
                font,
                iced::font::Weight::Medium
            ))
            .color(theme.text),
        caption(format!("Stays in {label} ({})", prompt.selection.summary())),
        editor(
            &prompt.first,
            "Description of the changes that stay",
            ChangesMsg::SplitFirstEdited
        ),
        caption(
            "Moves to a new change on top (the working copy follows it if it was here)".to_owned()
        ),
        editor(
            &prompt.second,
            "Description of the new change (empty is fine)",
            ChangesMsg::SplitSecondEdited
        ),
        actions,
    ]
    .spacing(6)
    .width(Length::Fill.max(520))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diffui_core::{DiffHunkView, DiffLine, DiffLineKind};

    fn file(path: &str, headers: &[&str]) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: headers
                .iter()
                .map(|header| DiffHunkView {
                    header: (*header).to_owned(),
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Addition,
                        old_line: None,
                        new_line: Some(1),
                        content: "x".to_owned(),
                        syntax: Vec::new(),
                        emphasis: Vec::new(),
                    }],
                })
                .collect(),
            additions: headers.len(),
            deletions: 0,
        }
    }

    const H1: &str = "@@ -1,3 +1,3 @@";
    const H2: &str = "@@ -20,3 +20,3 @@";

    #[test]
    fn hunks_collapse_to_a_whole_file_and_back() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![file("a.rs", &[H1, H2]), file("b.rs", &[H1])];
        let mut state = ChangesState::default();
        state.toggle_hunk(&wc, &files[0], 1);
        let selection = state.selection(&wc, &files);
        assert_eq!(selection.summary(), "1 hunk in a.rs");
        assert_eq!(
            file_check(&files[0], state.picks.get("a.rs")),
            CheckState::Partial
        );
        // Picking the other hunk too makes it whole.
        state.toggle_hunk(&wc, &files[0], 0);
        assert_eq!(state.picks.get("a.rs"), Some(&Pick::Whole));
        // Unpicking one hunk of a whole file leaves the rest.
        state.toggle_hunk(&wc, &files[0], 0);
        match state.selection(&wc, &files).files[0].hunks {
            HunkPick::Hunks(ref refs) => {
                assert_eq!(refs.len(), 1);
                assert_eq!(refs[0].index, 1);
                assert_eq!(refs[0].header, H2);
            }
            ref other => panic!("expected one hunk, got {other:?}"),
        }
        // And unpicking the last one drops the file.
        state.toggle_hunk(&wc, &files[0], 1);
        assert!(state.selection(&wc, &files).is_empty());
    }

    #[test]
    fn picks_belong_to_their_revision_and_follow_hunk_headers() {
        let wc = RevisionSelection::WorkingCopy;
        let other = RevisionSelection::Commit("abc".to_owned());
        let files = vec![file("a.rs", &[H1, H2])];
        let mut state = ChangesState::default();
        state.toggle_hunk(&wc, &files[0], 1);
        // Another revision sees nothing, and picking there drops these.
        assert!(state.selection(&other, &files).is_empty());
        state.toggle_file(&other, &files[0]);
        assert!(state.selection(&wc, &files).is_empty());
        assert_eq!(state.selection(&other, &files).summary(), "a.rs");

        // A reload that moved the picked hunk unpicks it.
        let mut state = ChangesState::default();
        state.toggle_hunk(&wc, &files[0], 1);
        let reloaded = vec![file("a.rs", &[H1, "@@ -22,3 +22,3 @@"])];
        assert!(state.selection(&wc, &reloaded).is_empty());
    }

    #[test]
    fn a_directory_toggles_every_file_under_it() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![
            file("src/a.rs", &[H1]),
            file("src/b.rs", &[H1]),
            file("x.rs", &[H1]),
        ];
        let mut state = ChangesState::default();
        state.toggle_file(&wc, &files[0]);
        state.toggle_dir(&wc, &files, "src/");
        assert_eq!(state.selection(&wc, &files).summary(), "2 files");
        state.toggle_dir(&wc, &files, "src/");
        assert!(state.selection(&wc, &files).is_empty());
    }

    #[test]
    fn only_plain_text_changes_split_by_hunk() {
        assert!(hunk_pickable(&file("a.rs", &[H1]), false));
        assert!(!hunk_pickable(&file("a.rs", &[H1]), true));
        assert!(!hunk_pickable(
            &file("a.bin", &["binary files differ"]),
            false
        ));
        assert!(!hunk_pickable(&file("mode.sh", &[]), false));
        let mut conflicted = file("c.rs", &[H1]);
        conflicted.status = DiffFileStatus::Conflicted;
        assert!(!hunk_pickable(&conflicted, false));
    }

    #[test]
    fn descriptions_are_stored_like_jj_stores_them() {
        assert_eq!(stored_description("feat: x\n\n\n"), "feat: x\n");
        assert_eq!(stored_description("  \n"), "");
    }
}
