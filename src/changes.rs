//! Partial changes in the diff pane: picking files and hunks with checkboxes,
//! single lines in line-select mode, and acting on the pick — split, squash
//! into the parent, move into a picked revision, discard.
//!
//! The pick lives per tab and belongs to the revision it was made in; showing
//! another revision drops it. Hunks are remembered by their header and lines
//! by their line numbers, so a reload that leaves a hunk where it was keeps it
//! picked, and one that moves it silently unpicks it (the backend refuses
//! stale headers and lines either way).
//!
//! Everything lands as an ordinary [`MutationOp`] through the same activity /
//! immutable-guard pipeline every other mutation uses.

use std::collections::{BTreeMap, BTreeSet};

use diffui_core::{
    DiffFile, DiffFileStatus, DiffHunkView, DiffSelection, FileSelection, HunkPick, HunkRef,
    LineRef, MutationOp, RevisionSelection, SquashTarget,
};
use iced::widget::{Space, column, container, row, text, text_editor};
use iced::{Element, Length, Padding, Task, alignment};

use crate::commands::CommandArg;
use crate::diff_view::{
    ChangeMarks, CheckState, FileMark, HunkMark, HunkMarks, LineAt, LineMode, LinePress,
};
use crate::modes::{ModeKind, TabMode};
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
    /// Line-select mode — see [`LineMsg`].
    Lines(LineMsg),
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

/// An intent of line-select mode.
#[derive(Debug, Clone)]
pub(crate) enum LineMsg {
    /// Enter the mode, or leave it if it is on.
    Toggle,
    Exit,
    /// Move the line cursor to the next (`1`) or previous (`-1`) pickable
    /// line; `extend` gives the line it lands on the anchor's state.
    Step {
        by: isize,
        extend: bool,
    },
    /// Toggle the line under the cursor.
    ToggleCursor,
    /// A press on a changed line.
    Press(LinePress),
    /// A drag from the pressed line onto this one.
    Paint(LineAt),
}

/// One file's pick.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Whole,
    /// Picked hunks by header. Never empty, and never every hunk with
    /// [`HunkLines::All`] (that is [`Pick::Whole`]).
    Hunks(BTreeMap<String, HunkLines>),
}

/// How much of a picked hunk is picked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HunkLines {
    All,
    /// Some of its changed lines: never none, never all.
    Only(BTreeSet<LineRef>),
}

/// The changed lines of `hunk`, with their row index in it.
fn changed_lines(hunk: &DiffHunkView) -> impl Iterator<Item = (usize, LineRef)> + '_ {
    hunk.lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| LineRef::of(line).map(|line| (index, line)))
}

/// Line-select mode: changed lines become pickable one by one. A
/// [`crate::modes::TabMode`], so it owns j/k/space/esc while it is on.
#[derive(Debug, Clone)]
pub(crate) struct LineSelect {
    /// The revision it was entered in; the mode is dead once another one
    /// shows.
    revision: RevisionSelection,
    /// The document the cursor and anchor index into; a reload drops them.
    document: u64,
    /// The keyboard's line.
    cursor: Option<LineAt>,
    /// Where the last click or toggle happened: shift-click and drags
    /// extend from here.
    anchor: Option<LineAt>,
    /// What the last click or toggle set its line to, and so what a range
    /// from the anchor sets.
    paint: bool,
    /// The range only takes lines of the anchor's kind (a side-by-side press
    /// lands in one column).
    one_side: bool,
    /// Bumped when the cursor moves by keyboard: the diff view scrolls it
    /// into view.
    reveal: u64,
}

impl LineSelect {
    pub(crate) fn new(revision: RevisionSelection, document: u64) -> Self {
        Self {
            revision,
            document,
            cursor: None,
            anchor: None,
            paint: true,
            one_side: false,
            reveal: 0,
        }
    }
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

    /// Toggle a file whole: picked whole goes to none, anything else
    /// (including a partial pick) to whole.
    fn toggle_file(&mut self, revision: &RevisionSelection, file: &DiffFile) {
        let picks = self.picks_mut(revision);
        if picks.remove(&file.path) != Some(Pick::Whole) {
            picks.insert(file.path.clone(), Pick::Whole);
        }
    }

    /// Toggle one hunk: picked whole goes to none, anything else to whole. A
    /// file whose every hunk ends up picked is picked whole; one with none
    /// left is dropped.
    fn toggle_hunk(&mut self, revision: &RevisionSelection, file: &DiffFile, hunk: usize) {
        let Some(view) = file.hunks.get(hunk) else {
            return;
        };
        let lines: Vec<LineRef> = changed_lines(view).map(|(_, line)| line).collect();
        let picked = self.hunk_state(revision, file, hunk) == CheckState::On;
        self.set_lines(revision, file, hunk, &lines, !picked);
    }

    /// How much of hunk `hunk` of `file` is picked.
    fn hunk_state(&self, revision: &RevisionSelection, file: &DiffFile, hunk: usize) -> CheckState {
        let Some(header) = file.hunks.get(hunk).map(|hunk| &hunk.header) else {
            return CheckState::Off;
        };
        match self
            .picks_for(revision)
            .and_then(|picks| picks.get(&file.path))
        {
            None => CheckState::Off,
            Some(Pick::Whole) => CheckState::On,
            Some(Pick::Hunks(hunks)) => match hunks.get(header) {
                None => CheckState::Off,
                Some(HunkLines::All) => CheckState::On,
                Some(HunkLines::Only(_)) => CheckState::Partial,
            },
        }
    }

    /// Whether changed line `line` of hunk `hunk` is picked.
    fn line_picked(
        &self,
        revision: &RevisionSelection,
        file: &DiffFile,
        hunk: usize,
        line: LineRef,
    ) -> bool {
        let Some(header) = file.hunks.get(hunk).map(|hunk| &hunk.header) else {
            return false;
        };
        match self
            .picks_for(revision)
            .and_then(|picks| picks.get(&file.path))
        {
            None => false,
            Some(Pick::Whole) => true,
            Some(Pick::Hunks(hunks)) => match hunks.get(header) {
                None => false,
                Some(HunkLines::All) => true,
                Some(HunkLines::Only(lines)) => lines.contains(&line),
            },
        }
    }

    /// Pick (or unpick) `lines` of hunk `hunk`, keeping the pick in its
    /// normal form: a hunk with every changed line picked is picked whole,
    /// a file with every hunk picked whole is picked whole, and empty picks
    /// are dropped.
    fn set_lines(
        &mut self,
        revision: &RevisionSelection,
        file: &DiffFile,
        hunk: usize,
        lines: &[LineRef],
        picked: bool,
    ) {
        let Some(view) = file.hunks.get(hunk) else {
            return;
        };
        let changed: BTreeSet<LineRef> = changed_lines(view).map(|(_, line)| line).collect();
        let picks = self.picks_mut(revision);
        let mut hunks = match picks.remove(&file.path) {
            None => BTreeMap::new(),
            Some(Pick::Whole) => file
                .hunks
                .iter()
                .map(|hunk| (hunk.header.clone(), HunkLines::All))
                .collect(),
            Some(Pick::Hunks(hunks)) => hunks,
        };
        let mut set = match hunks.remove(&view.header) {
            None => BTreeSet::new(),
            Some(HunkLines::All) => changed.clone(),
            Some(HunkLines::Only(set)) => set,
        };
        for line in lines.iter().filter(|line| changed.contains(line)) {
            if picked {
                set.insert(*line);
            } else {
                set.remove(line);
            }
        }
        if set == changed && !changed.is_empty() {
            hunks.insert(view.header.clone(), HunkLines::All);
        } else if !set.is_empty() {
            hunks.insert(view.header.clone(), HunkLines::Only(set));
        }
        hunks.retain(|header, _| file.hunks.iter().any(|hunk| hunk.header == *header));
        if hunks.is_empty() {
            return;
        }
        let whole = file
            .hunks
            .iter()
            .all(|hunk| hunks.get(&hunk.header) == Some(&HunkLines::All));
        let pick = if whole {
            Pick::Whole
        } else {
            Pick::Hunks(hunks)
        };
        picks.insert(file.path.clone(), pick);
    }

    /// Whether the changed line at `at` is picked.
    fn picked_at(&self, revision: &RevisionSelection, files: &[DiffFile], at: LineAt) -> bool {
        let Some(file) = files.get(at.file) else {
            return false;
        };
        line_ref_at(files, at).is_some_and(|line| self.line_picked(revision, file, at.hunk, line))
    }

    /// Flip the changed line at `at`; returns whether it is picked now.
    fn toggle_line(
        &mut self,
        revision: &RevisionSelection,
        files: &[DiffFile],
        at: LineAt,
    ) -> bool {
        let (Some(file), Some(line)) = (files.get(at.file), line_ref_at(files, at)) else {
            return false;
        };
        let picked = !self.line_picked(revision, file, at.hunk, line);
        self.set_lines(revision, file, at.hunk, &[line], picked);
        picked
    }

    /// Set every changed line from `anchor` to `to` (one file, either
    /// direction) to `picked`. `one_side` keeps to the anchor's kind of line:
    /// a side-by-side drag down the old column leaves the new one alone.
    fn paint_range(
        &mut self,
        revision: &RevisionSelection,
        files: &[DiffFile],
        anchor: LineAt,
        to: LineAt,
        picked: bool,
        one_side: bool,
    ) {
        let Some(file) = files.get(anchor.file).filter(|_| anchor.file == to.file) else {
            return;
        };
        let side = line_ref_at(files, anchor).map(|line| matches!(line, LineRef::Added(_)));
        let (from, to) = (anchor.min(to), anchor.max(to));
        for hunk in from.hunk..=to.hunk {
            let Some(view) = file.hunks.get(hunk) else {
                continue;
            };
            let lines: Vec<LineRef> = changed_lines(view)
                .filter(|(index, _)| {
                    let at = LineAt {
                        file: anchor.file,
                        hunk,
                        line: *index,
                    };
                    from <= at && at <= to
                })
                .map(|(_, line)| line)
                .filter(|line| !one_side || side == Some(matches!(line, LineRef::Added(_))))
                .collect();
            if !lines.is_empty() {
                self.set_lines(revision, file, hunk, &lines, picked);
            }
        }
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
                Pick::Hunks(hunks) => {
                    let refs: Vec<HunkRef> = file
                        .hunks
                        .iter()
                        .enumerate()
                        .filter_map(|(index, hunk)| {
                            let lines = match hunks.get(&hunk.header)? {
                                HunkLines::All => None,
                                HunkLines::Only(set) => {
                                    // Lines a reload took out of the hunk
                                    // are no longer picked.
                                    let lines: Vec<LineRef> = changed_lines(hunk)
                                        .map(|(_, line)| line)
                                        .filter(|line| set.contains(line))
                                        .collect();
                                    if lines.is_empty() {
                                        return None;
                                    }
                                    Some(lines)
                                }
                            };
                            Some(HunkRef {
                                index,
                                header: hunk.header.clone(),
                                lines,
                            })
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
        // A comparison's hunks are the difference between two revisions, not
        // a change either one holds, so there is nothing to split off.
        && !tab.session.diff_base.is_compare()
        && !tab.session.document_base.is_compare()
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

/// The changed line a document position names.
fn line_ref_at(files: &[DiffFile], at: LineAt) -> Option<LineRef> {
    LineRef::of(files.get(at.file)?.hunks.get(at.hunk)?.lines.get(at.line)?)
}

/// The pickable changed line `by` steps (±1) from `from` in document
/// order. Without a cursor, the first one at or after the start of
/// `selected_file`.
fn step_line(
    files: &[DiffFile],
    pickable: &dyn Fn(usize) -> bool,
    from: Option<LineAt>,
    selected_file: usize,
    by: isize,
) -> Option<LineAt> {
    let forward = by >= 0 || from.is_none();
    let start = from.unwrap_or(LineAt {
        file: selected_file.min(files.len().saturating_sub(1)),
        hunk: 0,
        line: 0,
    });
    let changed = |at: LineAt| pickable(at.file) && line_ref_at(files, at).is_some();
    if from.is_none() && changed(start) {
        return Some(start);
    }
    let mut at = start;
    loop {
        at = if forward {
            next_position(files, at)?
        } else {
            previous_position(files, at)?
        };
        if changed(at) {
            return Some(at);
        }
    }
}

/// The diff row after `at`, across hunks and files.
fn next_position(files: &[DiffFile], at: LineAt) -> Option<LineAt> {
    let (mut file, mut hunk, mut line) = (at.file, at.hunk, at.line + 1);
    loop {
        let hunks = &files.get(file)?.hunks;
        if let Some(view) = hunks.get(hunk) {
            if line < view.lines.len() {
                return Some(LineAt { file, hunk, line });
            }
            hunk += 1;
        } else {
            file += 1;
            hunk = 0;
        }
        line = 0;
    }
}

/// The diff row before `at`, across hunks and files.
fn previous_position(files: &[DiffFile], at: LineAt) -> Option<LineAt> {
    let (mut file, mut hunk) = (at.file, at.hunk);
    if at.line > 0 {
        return Some(LineAt {
            line: at.line - 1,
            ..at
        });
    }
    loop {
        if hunk > 0 {
            hunk -= 1;
        } else {
            file = file.checked_sub(1)?;
            hunk = files.get(file)?.hunks.len();
            continue;
        }
        let len = files.get(file)?.hunks.get(hunk)?.lines.len();
        if len > 0 {
            return Some(LineAt {
                file,
                hunk,
                line: len - 1,
            });
        }
    }
}

fn file_check(file: &DiffFile, pick: Option<&Pick>) -> CheckState {
    match pick {
        None => CheckState::Off,
        Some(Pick::Whole) => CheckState::On,
        Some(Pick::Hunks(hunks)) => {
            let present = file
                .hunks
                .iter()
                .filter_map(|hunk| hunks.get(&hunk.header))
                .collect::<Vec<_>>();
            if present.is_empty() {
                CheckState::Off
            } else if present.len() == file.hunks.len()
                && present.iter().all(|lines| **lines == HunkLines::All)
            {
                CheckState::On
            } else {
                CheckState::Partial
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
    let revision = &tab.session.selected_revision;
    let picks = tab.changes.picks_for(revision);
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
            } else if pick.is_none() {
                HunkMarks::Unpicked
            } else {
                HunkMarks::Picked(
                    (0..file.hunks.len())
                        .map(|hunk| match tab.changes.hunk_state(revision, file, hunk) {
                            CheckState::Off => HunkMark::Off,
                            CheckState::On => HunkMark::On,
                            CheckState::Partial => HunkMark::Lines(
                                file.hunks[hunk]
                                    .lines
                                    .iter()
                                    .map(|line| {
                                        LineRef::of(line).is_some_and(|line| {
                                            tab.changes.line_picked(revision, file, hunk, line)
                                        })
                                    })
                                    .collect(),
                            ),
                        })
                        .collect(),
                )
            };
            FileMark {
                state: file_check(file, pick),
                hunks,
            }
        })
        .collect();
    let lines = live_line_select(tab).map(|mode| LineMode {
        cursor: mode
            .cursor
            .filter(|_| mode.document == tab.session.document_id),
        reveal: mode.reveal,
        on_press: |press| Message::Changes(ChangesMsg::Lines(LineMsg::Press(press))),
        on_paint: |line| Message::Changes(ChangesMsg::Lines(LineMsg::Paint(line))),
    });
    Some(ChangeMarks {
        files,
        accent: theme.accent,
        on_toggle_file: |file| Message::Changes(ChangesMsg::ToggleFile(file)),
        on_toggle_hunk: |file, hunk| Message::Changes(ChangesMsg::ToggleHunk(file, hunk)),
        lines,
    })
}

/// The active tab's line-select mode, if it is on and still applies: the
/// diff is pickable and shows the revision the mode was entered in.
pub(crate) fn live_line_select(tab: &TabState) -> Option<&LineSelect> {
    tab.line_select()
        .filter(|mode| editable(tab) && mode.revision == tab.session.selected_revision)
}

/// Whether line-select mode can be entered on the active tab.
pub(crate) fn can_select_lines(ui: &Diffui) -> bool {
    editable(ui.active())
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
            ChangesMsg::Lines(message) => {
                self.on_lines(message);
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

    fn on_lines(&mut self, message: LineMsg) {
        let tab = self.active_mut();
        if let LineMsg::Toggle = message {
            if live_line_select(tab).is_some() {
                tab.pop_mode(ModeKind::Lines);
            } else if editable(tab) {
                let revision = tab.session.selected_revision.clone();
                let document = tab.session.document_id;
                tab.push_mode(TabMode::Lines(LineSelect::new(revision, document)));
            }
            return;
        }
        if live_line_select(tab).is_none() {
            return;
        }
        let revision = tab.session.selected_revision.clone();
        let selected_file = tab.selected_file;
        let pickable: Vec<bool> = tab
            .session
            .document
            .files
            .iter()
            .map(|file| hunk_pickable(file, crate::conflicts::is_conflicted(tab, &file.path)))
            .collect();
        let TabState {
            session,
            changes,
            modes,
            ..
        } = tab;
        let files = &session.document.files;
        let Some(TabMode::Lines(mode)) = modes
            .iter_mut()
            .rev()
            .find(|mode| mode.kind() == ModeKind::Lines)
        else {
            return;
        };
        if mode.document != session.document_id {
            mode.document = session.document_id;
            mode.cursor = None;
            mode.anchor = None;
        }
        match message {
            LineMsg::Toggle => {}
            LineMsg::Exit => {
                modes.retain(|mode| mode.kind() != ModeKind::Lines);
            }
            LineMsg::Step { by, extend } => {
                let from = mode.cursor;
                let pickable = |file: usize| pickable.get(file).copied().unwrap_or(false);
                let Some(next) = step_line(files, &pickable, from, selected_file, by) else {
                    return;
                };
                if extend {
                    let anchor = *mode.anchor.get_or_insert(from.unwrap_or(next));
                    if anchor.file == next.file {
                        if from.is_none() {
                            mode.paint = !changes.picked_at(&revision, files, next);
                        }
                        changes.paint_range(&revision, files, anchor, next, mode.paint, false);
                    }
                }
                mode.cursor = Some(next);
                mode.reveal = mode.reveal.wrapping_add(1);
            }
            LineMsg::ToggleCursor => {
                if let Some(cursor) = mode.cursor {
                    mode.paint = changes.toggle_line(&revision, files, cursor);
                    mode.anchor = Some(cursor);
                    mode.one_side = false;
                }
            }
            LineMsg::Press(press) => {
                let extend = press.extend
                    && mode
                        .anchor
                        .is_some_and(|anchor| anchor.file == press.line.file);
                if extend && let Some(anchor) = mode.anchor {
                    changes.paint_range(
                        &revision,
                        files,
                        anchor,
                        press.line,
                        mode.paint,
                        mode.one_side,
                    );
                } else {
                    mode.paint = changes.toggle_line(&revision, files, press.line);
                    mode.anchor = Some(press.line);
                    mode.one_side = press.one_side;
                }
                mode.cursor = Some(press.line);
            }
            LineMsg::Paint(line) => {
                if let Some(anchor) = mode.anchor
                    && anchor.file == line.file
                {
                    changes.paint_range(&revision, files, anchor, line, mode.paint, mode.one_side);
                    mode.cursor = Some(line);
                }
            }
        }
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
    let selecting = live_line_select(tab).is_some();
    if selection.is_empty() && !selecting {
        return None;
    }
    let button = |label: &'static str, msg: ChangesMsg, variant: ui::Variant| {
        ui::text_button(None, label, variant, ui::Size::Compact, font, theme)
            .on_press(Message::Changes(msg))
    };
    let clear = ui::tooltip(
        ui::icon_button(icons::CLOSE, theme.muted_text, ui::Size::Compact, theme)
            .on_press(Message::Changes(ChangesMsg::Clear)),
        "Clear selection",
        iced::widget::tooltip::Position::Top,
        font,
        theme,
    );
    let summary = if selecting {
        let picked = if selection.is_empty() {
            "Nothing picked yet".to_owned()
        } else {
            format!("{} picked", selection.summary())
        };
        row![
            icons::icon(icons::LIST_CHECKS, ui::ICON_SIZE, theme.accent),
            text("Selecting lines")
                .size(text_size::UI)
                .font(crate::theme::emphasis_font(
                    font,
                    iced::font::Weight::Medium
                ))
                .color(theme.text),
            text(picked)
                .size(text_size::UI)
                .font(font)
                .color(theme.muted_text),
        ]
    } else {
        row![
            icons::icon(icons::CHECK, ui::ICON_SIZE, theme.accent),
            text(format!("{} selected", selection.summary()))
                .size(text_size::UI)
                .font(font)
                .color(theme.text),
        ]
    }
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    // Two lines so the bar fits a narrow diff pane: what is picked (and the
    // clear button), then the actions, wrapping when they have to.
    let mut header = row![summary, Space::new().width(Length::Fill)]
        .spacing(6)
        .align_y(alignment::Vertical::Center);
    if !selection.is_empty() {
        header = header.push(clear);
    }
    if selecting {
        header = header.push(button(
            "Done",
            ChangesMsg::Lines(LineMsg::Exit),
            ui::Variant::Ghost,
        ));
    }
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
    let mut bar = column![header].spacing(4).width(Length::Fill.max(420));
    if !selection.is_empty() {
        bar = bar.push(actions);
    }
    if selecting {
        bar = bar.push(
            text("Click or drag lines \u{b7} Shift-click a range \u{b7} j/k and Space \u{b7} Esc when done")
                .size(text_size::CAPTION)
                .font(font)
                .color(theme.muted_text),
        );
    }
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
            .key_binding(ui::editor_key_binding)
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
            collapsed: None,
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

    fn row(kind: DiffLineKind, old_line: Option<usize>, new_line: Option<usize>) -> DiffLine {
        DiffLine {
            kind,
            old_line,
            new_line,
            content: "x".to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        }
    }

    /// Two hunks: lines 2–3 rewritten (rows 1–4: two removals, two
    /// additions), and line 21 rewritten (rows 1–2).
    fn rewrite(path: &str) -> DiffFile {
        use DiffLineKind::{Addition, Context, Deletion};
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            collapsed: None,
            hunks: vec![
                DiffHunkView {
                    header: "@@ -1,4 +1,4 @@".to_owned(),
                    lines: vec![
                        row(Context, Some(1), Some(1)),
                        row(Deletion, Some(2), None),
                        row(Deletion, Some(3), None),
                        row(Addition, None, Some(2)),
                        row(Addition, None, Some(3)),
                        row(Context, Some(4), Some(4)),
                    ],
                },
                DiffHunkView {
                    header: "@@ -20,3 +20,3 @@".to_owned(),
                    lines: vec![
                        row(Context, Some(20), Some(20)),
                        row(Deletion, Some(21), None),
                        row(Addition, None, Some(21)),
                        row(Context, Some(22), Some(22)),
                    ],
                },
            ],
            additions: 3,
            deletions: 3,
        }
    }

    fn at(file: usize, hunk: usize, line: usize) -> LineAt {
        LineAt { file, hunk, line }
    }

    fn refs(selection: &DiffSelection) -> Vec<(usize, Option<Vec<LineRef>>)> {
        match &selection.files[0].hunks {
            HunkPick::All => Vec::new(),
            HunkPick::Hunks(refs) => refs
                .iter()
                .map(|hunk| (hunk.index, hunk.lines.clone()))
                .collect(),
        }
    }

    #[test]
    fn single_lines_pick_partially_and_collapse_to_whole_hunks_and_files() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![rewrite("a.rs")];
        let mut state = ChangesState::default();

        assert!(state.toggle_line(&wc, &files, at(0, 0, 1)));
        assert_eq!(state.hunk_state(&wc, &files[0], 0), CheckState::Partial);
        assert_eq!(
            file_check(&files[0], state.picks.get("a.rs")),
            CheckState::Partial
        );
        let selection = state.selection(&wc, &files);
        assert_eq!(selection.summary(), "1 line in a.rs");
        assert_eq!(refs(&selection), vec![(0, Some(vec![LineRef::Removed(1)]))]);

        // The rest of the first hunk's changes: it is picked whole.
        for line in 2..=4 {
            state.toggle_line(&wc, &files, at(0, 0, line));
        }
        assert_eq!(state.hunk_state(&wc, &files[0], 0), CheckState::On);
        assert_eq!(refs(&state.selection(&wc, &files)), vec![(0, None)]);
        // And the second hunk's: the file is.
        state.toggle_line(&wc, &files, at(0, 1, 1));
        state.toggle_line(&wc, &files, at(0, 1, 2));
        assert_eq!(state.picks.get("a.rs"), Some(&Pick::Whole));

        // Unpicking one line of a whole file leaves everything else.
        assert!(!state.toggle_line(&wc, &files, at(0, 1, 2)));
        assert_eq!(
            refs(&state.selection(&wc, &files)),
            vec![(0, None), (1, Some(vec![LineRef::Removed(20)]))]
        );
        // Context rows are not lines to pick.
        assert!(!state.toggle_line(&wc, &files, at(0, 0, 0)));
    }

    #[test]
    fn a_mixed_checkbox_picks_everything_under_it() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![rewrite("a.rs")];
        let mut state = ChangesState::default();
        state.toggle_line(&wc, &files, at(0, 0, 3));
        state.toggle_hunk(&wc, &files[0], 0);
        assert_eq!(state.hunk_state(&wc, &files[0], 0), CheckState::On);
        state.toggle_hunk(&wc, &files[0], 0);
        assert!(state.selection(&wc, &files).is_empty());

        state.toggle_line(&wc, &files, at(0, 1, 1));
        state.toggle_file(&wc, &files[0]);
        assert_eq!(state.picks.get("a.rs"), Some(&Pick::Whole));
        state.toggle_file(&wc, &files[0]);
        assert!(state.selection(&wc, &files).is_empty());
    }

    #[test]
    fn a_range_paints_every_changed_line_between_or_one_column_of_them() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![rewrite("a.rs")];

        // Unified: from the first removal down into the second hunk.
        let mut state = ChangesState::default();
        state.paint_range(&wc, &files, at(0, 0, 1), at(0, 1, 1), true, false);
        assert_eq!(
            refs(&state.selection(&wc, &files)),
            vec![(0, None), (1, Some(vec![LineRef::Removed(20)]))]
        );

        // Side by side, dragged up the new column: only additions.
        let mut state = ChangesState::default();
        state.paint_range(&wc, &files, at(0, 1, 2), at(0, 0, 0), true, true);
        assert_eq!(
            refs(&state.selection(&wc, &files)),
            vec![
                (0, Some(vec![LineRef::Added(1), LineRef::Added(2)])),
                (1, Some(vec![LineRef::Added(20)])),
            ]
        );
        // Painting the same range off again empties the pick.
        state.paint_range(&wc, &files, at(0, 1, 2), at(0, 0, 0), false, true);
        assert!(state.selection(&wc, &files).is_empty());

        // Ranges never cross files.
        let two = vec![rewrite("a.rs"), rewrite("b.rs")];
        let mut state = ChangesState::default();
        state.paint_range(&wc, &two, at(0, 0, 1), at(1, 0, 1), true, false);
        assert!(state.selection(&wc, &two).is_empty());
    }

    #[test]
    fn the_line_cursor_walks_changed_lines_of_pickable_files() {
        let files = vec![
            rewrite("a.rs"),
            file("b.bin", &["binary files differ"]),
            rewrite("c.rs"),
        ];
        let pickable = |index: usize| index != 1;
        let step = |from: Option<LineAt>, by: isize| step_line(&files, &pickable, from, 0, by);
        // No cursor yet: the first changed line of the selected file.
        assert_eq!(step(None, 1), Some(at(0, 0, 1)));
        assert_eq!(step(Some(at(0, 0, 4)), 1), Some(at(0, 1, 1)));
        // Over the context row, the hunk boundary and the binary file.
        assert_eq!(step(Some(at(0, 1, 2)), 1), Some(at(2, 0, 1)));
        assert_eq!(step(Some(at(2, 0, 1)), -1), Some(at(0, 1, 2)));
        assert_eq!(step(Some(at(0, 0, 1)), -1), None);
        assert_eq!(step(Some(at(2, 1, 2)), 1), None);
        // Starting from a file without pickable lines finds the next one.
        assert_eq!(step_line(&files, &pickable, None, 1, 1), Some(at(2, 0, 1)));
    }

    #[test]
    fn line_picks_a_reload_moved_are_dropped() {
        let wc = RevisionSelection::WorkingCopy;
        let files = vec![rewrite("a.rs")];
        let mut state = ChangesState::default();
        state.toggle_line(&wc, &files, at(0, 0, 1));
        // Same header, but the removal now sits on another line.
        let mut reloaded = rewrite("a.rs");
        reloaded.hunks[0].lines[1].old_line = Some(5);
        assert!(state.selection(&wc, &[reloaded]).is_empty());
    }
}
