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

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};
use std::collections::{BTreeMap, BTreeSet};

use diffui_core::{
    DiffFile, DiffFileStatus, DiffHunkView, DiffLineKind, DiffSelection, FileSelection, HunkPick,
    HunkRef, LineRef, MutationOp, RevisionSelection, SquashTarget,
};

use crate::commands::CommandArg;
use crate::diff_view::{
    ChangeMarks, CheckState, FileMark, HunkMark, HunkMarks, LineAt, LineMode, LinePress,
};
use crate::modes::{ModeKind, TabMode};
use crate::palette::ResultRef;
use crate::prose::Prose;
use crate::theme::{self, Color, Font, Weight, text_size};
use crate::typography::{self, Aligned};
use crate::ui::Look;
use crate::ui::Tip as _;
use crate::{Action, ConfirmDialog, Cx, Diffui, MainView, TabState, icons, ui};

/// A change-selection intent or event.
#[derive(Debug, Clone)]
pub(crate) enum ChangesMsg {
    /// Toggle a file (document index) whole.
    ToggleFile(usize),
    /// Toggle one hunk: `(file, hunk)` document indices.
    ToggleHunk(usize, usize),
    /// Discard one hunk (asks first).
    RestoreHunk(usize, usize),
    /// Copy one hunk as a patch.
    CopyHunk(usize, usize),
    /// Copy the pick as a patch.
    CopyPatch,
    /// Absorb the picked files into the ancestors that last touched their
    /// lines (`jj absorb` takes paths, so whole files only).
    Absorb,
    /// The picks' "more" menu.
    MoreMenu,
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
    SplitFirstEdited(String),
    SplitSecondEdited(String),
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
    /// What stays and what moves, under the title.
    summary: Prose,
    original: String,
    first: String,
    second: String,
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

#[cfg(test)]
impl ChangesState {
    /// The first description as the split prompt holds it.
    pub(crate) fn split_first(&self) -> Option<&str> {
        self.split.as_ref().map(|prompt| prompt.first.as_str())
    }
}

impl ChangesState {
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
    match tab.session.commits.find(revision) {
        Some(row) => row.change_id().chars().take(8).collect(),
        None => match revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(hex) => hex.chars().take(8).collect(),
        },
    }
}

/// [`DiffSelection::summary`] with a single file's path marked as one.
pub(crate) fn selection_prose(selection: &DiffSelection) -> Prose {
    let summary = selection.summary();
    if let [only] = selection.files.as_slice()
        && let Some(lead) = summary.strip_suffix(only.path.as_str())
    {
        return Prose::from(lead).path(&only.path);
    }
    Prose::from(summary)
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
pub(crate) fn diff_marks(ui: &Diffui, look: Look, cx: &mut Cx) -> Option<ChangeMarks> {
    let Look { theme, .. } = look;
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
    let on_press = crate::Diffui::listen(cx, |this, press: &LinePress, _, cx| {
        this.perform(
            crate::Action::Changes(ChangesMsg::Lines(LineMsg::Press(*press))),
            cx,
        );
    });
    let on_paint = crate::Diffui::listen(cx, |this, line: &LineAt, _, cx| {
        this.perform(
            crate::Action::Changes(ChangesMsg::Lines(LineMsg::Paint(*line))),
            cx,
        );
    });
    let lines = live_line_select(tab).map(|mode| LineMode {
        cursor: mode
            .cursor
            .filter(|_| mode.document == tab.session.document_id),
        reveal: mode.reveal,
        on_press: std::rc::Rc::new(on_press),
        on_paint: std::rc::Rc::new(on_paint),
    });
    Some(ChangeMarks {
        files,
        control: theme.info,
        on_toggle_file: std::rc::Rc::new(crate::Diffui::listen(cx, |this, file: &usize, _, cx| {
            this.perform(crate::Action::Changes(ChangesMsg::ToggleFile(*file)), cx);
        })),
        on_toggle_hunk: std::rc::Rc::new(crate::Diffui::listen(
            cx,
            |this, &(file, hunk): &(usize, usize), _, cx| {
                this.perform(
                    crate::Action::Changes(ChangesMsg::ToggleHunk(file, hunk)),
                    cx,
                );
            },
        )),
        lines,
        picking: !selection_of(tab).is_empty(),
    })
}

/// The active tab's line-select mode, if it is on and still applies: the
/// diff is pickable and shows the revision the mode was entered in.
pub(crate) fn live_line_select(tab: &TabState) -> Option<&LineSelect> {
    tab.line_select()
        .filter(|mode| editable(tab) && mode.revision == tab.session.selected_revision)
}

/// The file path and the line number (new side, else old) under line-select
/// mode's cursor on the active tab.
pub(crate) fn cursor_line(ui: &Diffui) -> Option<(String, usize)> {
    let tab = ui.active();
    let mode = live_line_select(tab).filter(|mode| mode.document == tab.session.document_id)?;
    let at = mode.cursor?;
    let file = tab.session.document.files.get(at.file)?;
    let line = file.hunks.get(at.hunk)?.lines.get(at.line)?;
    Some((file.path.clone(), line.new_line.or(line.old_line)?))
}

/// Whether line-select mode can be entered on the active tab.
pub(crate) fn can_select_lines(ui: &Diffui) -> bool {
    editable(ui.active())
}

/// Hunk `hunk` of file `file`, whole, as a selection.
fn one_hunk(files: &[DiffFile], file: usize, hunk: usize) -> Option<DiffSelection> {
    let file = files.get(file)?;
    let view = file.hunks.get(hunk)?;
    Some(DiffSelection {
        files: vec![FileSelection {
            path: file.path.clone(),
            hunks: HunkPick::Hunks(vec![HunkRef {
                index: hunk,
                header: view.header.clone(),
                lines: None,
            }]),
        }],
    })
}

/// `selection` of `files` as a unified patch, the way `git apply` takes one.
/// A partly picked hunk keeps its unpicked removals as context and leaves its
/// unpicked additions out, so the patch carries exactly the picked lines.
pub(crate) fn selection_patch(files: &[DiffFile], selection: &DiffSelection) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for picked in &selection.files {
        let Some(file) = files.iter().find(|file| file.path == picked.path) else {
            continue;
        };
        let old = file.old_path.as_deref().unwrap_or(&file.path);
        let _ = writeln!(out, "diff --git a/{old} b/{}", file.path);
        if let Some((from, to)) = file.mode_change {
            let _ = writeln!(out, "old mode {from:o}\nnew mode {to:o}");
        }
        let hunks: Vec<(&DiffHunkView, Option<&[LineRef]>)> = match &picked.hunks {
            HunkPick::All => file.hunks.iter().map(|hunk| (hunk, None)).collect(),
            HunkPick::Hunks(refs) => refs
                .iter()
                .filter_map(|at| Some((file.hunks.get(at.index)?, at.lines.as_deref())))
                .collect(),
        };
        if hunks.iter().all(|(hunk, _)| hunk.lines.is_empty()) {
            continue;
        }
        let side = |prefix: &str, path: &str, absent: bool| {
            if absent {
                "/dev/null".to_owned()
            } else {
                format!("{prefix}/{path}")
            }
        };
        let _ = writeln!(
            out,
            "--- {}\n+++ {}",
            side("a", old, file.status == DiffFileStatus::Added),
            side("b", &file.path, file.status == DiffFileStatus::Deleted),
        );
        for (hunk, lines) in hunks {
            write_hunk(&mut out, hunk, lines);
        }
    }
    out
}

/// One hunk of [`selection_patch`]: all of it, or only the `picked` lines.
fn write_hunk(out: &mut String, hunk: &DiffHunkView, picked: Option<&[LineRef]>) {
    use std::fmt::Write as _;
    if hunk.lines.is_empty() {
        return;
    }
    let mut body = String::new();
    let (mut old_count, mut new_count) = (0usize, 0usize);
    for line in &hunk.lines {
        let take =
            picked.is_none_or(|picked| LineRef::of(line).is_some_and(|at| picked.contains(&at)));
        let prefix = match (line.kind, take) {
            (DiffLineKind::Addition, true) => {
                new_count += 1;
                '+'
            }
            (DiffLineKind::Addition, false) => continue,
            (DiffLineKind::Deletion, true) => {
                old_count += 1;
                '-'
            }
            (DiffLineKind::Note, _) => {
                let _ = writeln!(body, "{}", line.content.trim_end_matches('\n'));
                continue;
            }
            _ => {
                old_count += 1;
                new_count += 1;
                ' '
            }
        };
        let _ = writeln!(body, "{prefix}{}", line.content.trim_end_matches('\n'));
    }
    let old_start = hunk
        .lines
        .iter()
        .find_map(|line| line.old_line)
        .unwrap_or(0);
    let new_start = hunk
        .lines
        .iter()
        .find_map(|line| line.new_line)
        .unwrap_or(0);
    let context = hunk
        .header
        .strip_prefix("@@")
        .and_then(|rest| rest.find("@@").map(|end| rest[end + 2..].trim()))
        .unwrap_or("");
    let _ = write!(
        out,
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
    );
    if !context.is_empty() {
        let _ = write!(out, " {context}");
    }
    out.push('\n');
    out.push_str(&body);
}

/// The pick actions riding each pickable hunk's header, shown while the
/// cursor is on the hunk or while it's picked: pick it, restore it, copy it.
/// A header's buttons are built when it's on screen.
pub(crate) fn hunk_actions(
    ui: &Diffui,
    look: Look,
    cx: &mut Cx,
) -> Option<crate::hunk_actions::HunkActions> {
    let Look { theme, .. } = look;
    use crate::hunk_actions::{Button, HunkActions, Row, Run, Style, Tone};
    if !editable(ui.active()) {
        return None;
    }
    let font = look.font;
    let app = cx.entity();
    let rows = move |file: usize, hunk: usize, cx: &gpui_kit::App| {
        let tab = app.read(cx).active();
        let document = tab.session.document.files.get(file)?;
        if hunk >= document.hunks.len()
            || !hunk_pickable(
                document,
                crate::conflicts::is_conflicted(tab, &document.path),
            )
        {
            return None;
        }
        let button = |text: &str, tone: Tone| Button {
            runs: vec![Run {
                text: text.to_owned(),
                font,
                color: match tone {
                    Tone::On => theme.panel_background,
                    _ => theme.muted_text,
                },
            }],
            tone,
        };
        let pick = match tab
            .changes
            .hunk_state(&tab.session.selected_revision, document, hunk)
        {
            CheckState::Off => button("Pick hunk", Tone::Ghost),
            CheckState::On => button("Picked", Tone::On),
            CheckState::Partial => button("Picked lines", Tone::On),
        };
        Some(Row {
            file,
            hunk,
            focused: false,
            lead: None,
            buttons: vec![
                pick,
                button("Restore hunk", Tone::Ghost),
                button("Copy", Tone::Ghost),
            ],
            hover: true,
        })
    };
    Some(HunkActions {
        rows: std::rc::Rc::new(rows),
        style: Style {
            size: text_size::CAPTION,
            fill: theme.panel_background,
            hover_fill: theme::ghost_button_style(theme).hover,
            ghost_hover_fill: theme::ghost_button_style(theme).hover,
            border: theme.border,
            focus: theme.accent,
            on_fill: theme.accent,
            radius: theme::radius::XS,
        },
        on_press: std::rc::Rc::new(Diffui::listen(
            cx,
            move |this, &(file, hunk, button): &(usize, usize, usize), _, cx| {
                let msg = match button {
                    0 => ChangesMsg::ToggleHunk(file, hunk),
                    1 => ChangesMsg::RestoreHunk(file, hunk),
                    _ => ChangesMsg::CopyHunk(file, hunk),
                };
                this.perform(Action::Changes(msg), cx);
            },
        )),
        files: Vec::new(),
        on_file_press: std::rc::Rc::new(|_, _, _| {}),
        reveal: None,
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
    pub(crate) fn on_changes(&mut self, msg: ChangesMsg, cx: &mut Cx) {
        match msg {
            ChangesMsg::ToggleFile(index) => {
                self.toggle_file(index);
            }
            ChangesMsg::ToggleHunk(file, hunk) => {
                self.toggle_hunk(file, hunk);
            }
            ChangesMsg::RestoreHunk(file, hunk) => {
                let tab = self.active();
                if !editable(tab) {
                    return;
                }
                let Some(selection) = one_hunk(&tab.session.document.files, file, hunk) else {
                    return;
                };
                let revision = tab.session.selected_revision.clone();
                self.confirm_discard(revision, selection)
            }
            ChangesMsg::CopyPatch => {
                let tab = self.active();
                let selection = selection_of(tab);
                if !selection.is_empty() {
                    let patch = selection_patch(&tab.session.document.files, &selection);
                    self.perform(Action::Copy(patch), cx);
                }
            }
            ChangesMsg::Absorb => {
                let Some((revision, selection)) = self.current_selection() else {
                    return;
                };
                if !whole_files_only(&selection) {
                    return;
                }
                self.start_changes_op(MutationOp::Absorb {
                    from: revision,
                    paths: selection.files.into_iter().map(|file| file.path).collect(),
                })
            }
            ChangesMsg::MoreMenu => self.open_picks_menu(cx),
            ChangesMsg::CopyHunk(file, hunk) => {
                let files = &self.active().session.document.files;
                if let Some(selection) = one_hunk(files, file, hunk) {
                    let patch = selection_patch(files, &selection);
                    self.perform(Action::Copy(patch), cx);
                }
            }
            ChangesMsg::ToggleTreeRow(display) => {
                self.toggle_tree_row(display);
            }
            ChangesMsg::Lines(message) => {
                self.on_lines(message);
            }
            ChangesMsg::Clear => {
                let tab = self.active_mut();
                tab.changes.picks.clear();
                tab.changes.split = None;
            }
            ChangesMsg::SquashIntoParent => {
                let Some((revision, selection)) = self.current_selection() else {
                    return;
                };
                self.start_changes_op(MutationOp::SquashChanges {
                    from: revision,
                    selection,
                    into: SquashTarget::Parent,
                })
            }
            ChangesMsg::MoveInto => {
                let Some((revision, selection)) = self.current_selection() else {
                    return;
                };
                self.move_into(revision, selection, cx)
            }
            ChangesMsg::Discard => {
                let Some((revision, selection)) = self.current_selection() else {
                    return;
                };
                self.confirm_discard(revision, selection)
            }
            ChangesMsg::OpenSplit => {
                let Some((revision, selection)) = self.current_selection() else {
                    return;
                };
                let summary = Prose::from("Keep ")
                    .then(selection_prose(&selection))
                    .text(", move the rest to a new change.");
                self.open_split(revision, selection, summary, cx)
            }
            ChangesMsg::SplitFirstEdited(text) => {
                if let Some(prompt) = self.active_mut().changes.split.as_mut() {
                    prompt.first = text;
                }
            }
            ChangesMsg::SplitSecondEdited(text) => {
                if let Some(prompt) = self.active_mut().changes.split.as_mut() {
                    prompt.second = text;
                }
            }
            ChangesMsg::SplitCancel => {
                self.active_mut().changes.split = None;
            }
            ChangesMsg::SplitConfirm => {
                let Some(prompt) = self.active_mut().changes.split.take() else {
                    return;
                };
                if prompt.revision != self.active().session.selected_revision {
                    return;
                }
                let first = stored_description(&prompt.first);
                let first_description =
                    (first != stored_description(&prompt.original)).then_some(first);
                self.start_changes_op(MutationOp::Split {
                    target: prompt.revision,
                    selection: prompt.selection,
                    first_description,
                    second_description: stored_description(&prompt.second),
                })
            }
            ChangesMsg::RestoreFile(path) => {
                if !editable(self.active()) {
                    return;
                }
                let revision = self.active().session.selected_revision.clone();
                self.confirm_discard(revision, whole_file(&path))
            }
            ChangesMsg::MoveFile(path) => {
                if !editable(self.active()) {
                    return;
                }
                let revision = self.active().session.selected_revision.clone();
                self.move_into(revision, whole_file(&path), cx)
            }
            ChangesMsg::SplitFile(path) => {
                let tab = self.active();
                if !editable(tab) {
                    return;
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
                        "Nothing to split",
                        &format!("`{path}` is the only change here, so nothing would stay."),
                    );
                    return;
                }
                let revision = tab.session.selected_revision.clone();
                self.open_split(
                    revision,
                    DiffSelection { files: rest },
                    Prose::from("Move ").path(&path).text(" to a new change."),
                    cx,
                )
            }
            ChangesMsg::Conflict(message) => self.on_conflict(message, cx),
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

    fn open_split(
        &mut self,
        revision: RevisionSelection,
        selection: DiffSelection,
        summary: Prose,
        cx: &mut Cx,
    ) {
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
            summary,
            first: (original.trim_end().to_owned()),
            second: (original.trim_end().to_owned()),
            original,
        });
        crate::input::focus(SPLIT_FIRST_EDITOR_ID, cx)
    }

    /// Target mode for moving `selection` out of `revision`.
    fn move_into(&mut self, revision: RevisionSelection, selection: DiffSelection, cx: &mut Cx) {
        self.start_draft(diffui_core::DraftKind::Squash, revision, cx);
        if let Some(draft) = self.active_mut().op_draft_mut() {
            draft.draft.selection = Some(Box::new(selection));
        }
    }

    /// Discarding destroys changes: ask first, and say how to get them back.
    fn confirm_discard(&mut self, revision: RevisionSelection, selection: DiffSelection) {
        let Some(tab_id) = self.active_tab_id() else {
            return;
        };
        let label = revision_label(self.active(), &revision);
        let title = Prose::from("Discard ")
            .then(selection_prose(&selection))
            .text("?");
        let pending = self.begin_mutation(
            tab_id,
            Prose::from("Discard ").then(selection_prose(&selection)),
            MutationOp::RestoreChanges {
                target: revision,
                selection,
            },
        );
        self.ask(ConfirmDialog {
            title,
            body: Prose::from("The changes leave ")
                .rev(label)
                .text(". Undo brings them back."),
            confirm_label: "Discard".to_owned(),
            look: Default::default(),
            pending,
        });
    }

    /// The picks' "more" menu: what the bar has no room for.
    fn open_picks_menu(&mut self, cx: &mut Cx) {
        let tree = vec![
            self.menu_item("lines.toggle", CommandArg::None),
            self.menu_item("changes.clear", CommandArg::None),
        ];
        #[cfg(target_os = "macos")]
        Self::popup_tree(&tree, None, cx);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            let at = crate::geometry::Point::new(
                self.window_size.width / 2.0,
                self.window_size.height - BAR_BOTTOM - BAR_BUTTON_HEIGHT,
            );
            self.push_mode(crate::modes::Mode::Menu(crate::menu::OverlayMenu::new(
                tree,
                crate::menu::AnchorSpec::At(at),
                None,
            )));
        }
    }

    /// Run a partial-change op on the active tab, dropping the pick it used.
    pub(crate) fn start_changes_op(&mut self, op: MutationOp) {
        let state = self.active_mut();
        state.changes.picks.clear();
        state.changes.split = None;
        self.start_mutation_op(op)
    }
}

pub(crate) const SPLIT_FIRST_EDITOR_ID: &str = "split-first-description";
const SPLIT_SECOND_EDITOR_ID: &str = "split-second-description";
const SPLIT_WIDTH: f32 = 560.0;

/// Whether the split prompt is up, and so owns the keyboard and pointer.
pub(crate) fn split_shown(ui: &Diffui) -> bool {
    split_prompt_shown(ui).is_some()
}

/// The split prompt, when it belongs on screen: open, and for the revision
/// the diff shows.
fn split_prompt_shown(ui: &Diffui) -> Option<&SplitPrompt> {
    let tab = ui.active();
    tab.changes
        .split
        .as_ref()
        .filter(|prompt| editable(tab) && prompt.revision == tab.session.selected_revision)
}

/// The split card's height without its two editors: the head (title and summary),
/// the body's insets and the gap between the fields, the fields' captions and the
/// footer strip.
const SPLIT_CHROME_HEIGHT: f32 = 68.0 + (16.0 + 20.0 + 10.0) + 2.0 * 24.0 + 57.0;

/// Whether a picked file is whole: `jj absorb` takes paths, not hunks.
fn whole_files_only(selection: &DiffSelection) -> bool {
    selection
        .files
        .iter()
        .all(|file| matches!(file.hunks, HunkPick::All))
}

/// "2 files · 1 hunk · 3 lines": what is picked whole, by hunk and by line.
fn pick_detail(selection: &DiffSelection) -> String {
    let count = |n: usize, noun: &str| match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    };
    let files = selection
        .files
        .iter()
        .filter(|file| matches!(file.hunks, HunkPick::All))
        .count();
    let parts = [
        (files, "file"),
        (selection.hunk_count() - files, "hunk"),
        (selection.line_count(), "line"),
    ];
    parts
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, noun)| count(n, noun))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// The labels the bar's buttons carry when it has room: see [`bar_fits_labels`].
const BAR_LABELS: [&str; 6] = [
    "Split\u{2026}",
    "Squash",
    "Move to\u{2026}",
    "Absorb",
    "Restore",
    "Copy patch",
];
/// The bar's distance from the window's bottom edge.
const BAR_BOTTOM: f32 = 40.0;
const BAR_BUTTON_HEIGHT: f32 = 30.0;

/// Whether the bar, its buttons labelled, fits over a diff pane `pane_width`
/// wide; if not, they drop their labels for their icons. Measured from the
/// labels themselves, so a longer summary or the Done of line-select mode
/// can't push the bar's last buttons off the window.
fn bar_fits_labels(
    pane_width: f32,
    (title, detail): (&str, &str),
    actions: bool,
    selecting: bool,
    font: Font,
) -> bool {
    let width = |text: &str, font: Font| crate::measure::line_width(text, text_size::BODY, font);
    // The bar's padding, the summary's own and the side margins around the bar.
    let mut needed = 2.0 * 5.0 + 8.0 + 10.0 + 2.0 * 16.0;
    needed += width(title, theme::emphasis_font(font, Weight::SEMIBOLD))
        + theme::space::SM
        + width(detail, font);
    // A labelled button: icon, gap, label, padding, and the bar's gap.
    let button = |label: &str| ui::ICON_SIZE + 6.0 + width(label, font) + 2.0 * 9.0 + 2.0;
    let separator = 1.0 + 2.0 * 4.0 + 2.0;
    if actions {
        needed += BAR_LABELS.iter().map(|label| button(label)).sum::<f32>()
            + BAR_BUTTON_HEIGHT
            + 2.0 * separator;
        // Clear and its `esc` keycap.
        needed += width("Clear", font) + 6.0 + 30.0 + 2.0 * 9.0;
    }
    if selecting {
        needed += separator + width("Done", font) + 2.0 * 9.0;
    }
    pane_width >= needed
}

/// The picks' floating bar: what is picked and what can be done with it, over
/// the diff, in inverted colours. Line-select mode shows it too, with its way
/// out. Empty when nothing is picked. Mounted once for the window.
pub(crate) fn action_bar(ui: &Diffui, look: Look, _window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    use gpui_kit::base::TestSupportExt as _;
    let tab = ui.active();
    let empty = || div().into_any_element();
    if !editable(tab) || split_prompt_shown(ui).is_some() {
        return empty();
    }
    let selection = selection_of(tab);
    let selecting = live_line_select(tab).is_some();
    if selection.is_empty() && !selecting {
        return empty();
    }
    let font = look.font;
    let bold = theme::emphasis_font(font, Weight::SEMIBOLD);
    let ink = theme.panel_background;
    let faded = Color { a: 0.7, ..ink };
    let pane_width = crate::diff_panel::diff_pane_width(ui);
    let pane_left = (ui.window_size.width - pane_width).max(0.0);

    let (title, detail) = match (selecting, selection.is_empty()) {
        (true, true) => ("Selecting lines", "Nothing picked yet".to_owned()),
        (true, false) => ("Selecting lines", pick_detail(&selection)),
        (false, _) => ("Picked", pick_detail(&selection)),
    };
    let labels = bar_fits_labels(
        pane_width,
        (title, &detail),
        !selection.is_empty(),
        selecting,
        font,
    );
    let label = typography::baseline_row(
        vec![
            Aligned::text(
                typography::line(title, bold, text_size::BODY, ink),
                bold,
                text_size::BODY,
            ),
            Aligned::gap(theme::space::SM),
            Aligned::text(
                typography::line(detail, font, text_size::BODY, faded),
                font,
                text_size::BODY,
            ),
        ],
        0.0,
    );
    let separator = || {
        div()
            .flex_none()
            .w(px(1.0))
            .h(px(18.0))
            .mx(px(4.0))
            .bg(Color { a: 0.18, ..ink })
            .into_any_element()
    };
    let hover = Color {
        r: 0.5,
        g: 0.5,
        b: 0.5,
        a: 0.24,
    };
    let button = |id: &'static str,
                  icon: Option<&'static str>,
                  text: &'static str,
                  tip: &'static str,
                  enabled: bool,
                  msg: ChangesMsg,
                  cx: &mut Cx| {
        let show_label = labels || icon.is_none();
        let mut content = div().flex().flex_row().items_center().gap(px(6.0));
        if let Some(icon) = icon {
            content = content.child(icons::icon(icon, ui::ICON_SIZE, ink));
        }
        if show_label {
            content = content.child(typography::centered(
                typography::line(text, font, text_size::BODY, ink),
                font,
                text_size::BODY,
            ));
        }
        let mut element = div()
            .id(id)
            .test_support()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(BAR_BUTTON_HEIGHT))
            // The hover wash comes before the content it sits under.
            .when(enabled, |element| {
                element.child(crate::squircle::hover_background(
                    crate::squircle::Fill::new(crate::squircle::Corners::all(
                        theme::radius::CONTROL,
                    ))
                    .background(hover),
                    0.0,
                ))
            })
            .child(content)
            .tip(tip, look);
        element = if show_label {
            element.px(px(9.0))
        } else {
            element.w(px(BAR_BUTTON_HEIGHT))
        };
        if enabled {
            element = element
                .cursor_pointer()
                .on_click(Diffui::on_action(Action::Changes(msg), cx));
        } else {
            element = element.opacity(0.4);
        }
        element.into_any_element()
    };

    let mut items = vec![
        div()
            .flex_none()
            .pl(px(8.0))
            .pr(px(10.0))
            .child(label)
            .into_any_element(),
    ];
    if !selection.is_empty() {
        let absorb = whole_files_only(&selection);
        items.push(separator());
        items.push(button(
            "picks-split",
            Some(icons::SCISSORS),
            "Split\u{2026}",
            "jj split: move the picked changes into a new revision",
            true,
            ChangesMsg::OpenSplit,
            cx,
        ));
        items.push(button(
            "picks-squash",
            Some(icons::ARROW_DOWN_TO_LINE),
            "Squash",
            "jj squash: fold the picked changes into the parent",
            true,
            ChangesMsg::SquashIntoParent,
            cx,
        ));
        items.push(button(
            "picks-move",
            Some(icons::MOVE),
            "Move to\u{2026}",
            "jj squash --into: move the picked changes to another revision",
            true,
            ChangesMsg::MoveInto,
            cx,
        ));
        items.push(button(
            "picks-absorb",
            Some(icons::COMBINE),
            "Absorb",
            if absorb {
                "jj absorb: send each picked line to the revision that last touched it"
            } else {
                "jj absorb takes whole files: pick files, not hunks or lines"
            },
            absorb,
            ChangesMsg::Absorb,
            cx,
        ));
        items.push(button(
            "picks-discard",
            Some(icons::ROTATE_CCW),
            "Restore",
            "jj restore: undo the picked changes",
            true,
            ChangesMsg::Discard,
            cx,
        ));
        items.push(separator());
        items.push(button(
            "picks-copy",
            Some(icons::COPY),
            "Copy patch",
            "Copy the picked changes as a patch",
            true,
            ChangesMsg::CopyPatch,
            cx,
        ));
        items.push(button(
            "picks-more",
            Some(icons::ELLIPSIS),
            "",
            "More actions for the picked changes",
            true,
            ChangesMsg::MoreMenu,
            cx,
        ));
    }
    items.push(separator());
    if selecting {
        items.push(button(
            "lines-done",
            None,
            "Done",
            "Stop selecting lines; the picked lines stay picked",
            true,
            ChangesMsg::Lines(LineMsg::Exit),
            cx,
        ));
    }
    if !selection.is_empty() {
        let keycaps = crate::overlay_kit::bound_keys(
            ui,
            crate::commands::Context::Base,
            &["selection.clear"],
        )
        .into_iter()
        .map(|key| {
            ui::keycap(key, look)
                .colors(faded, theme::TRANSPARENT, Color { a: 0.6, ..ink })
                .view()
        });
        let clear = div()
            .id("picks-clear")
            .test_support()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(BAR_BUTTON_HEIGHT))
            .px(px(9.0))
            .child(crate::squircle::hover_background(
                crate::squircle::Fill::new(crate::squircle::Corners::all(theme::radius::CONTROL))
                    .background(hover),
                0.0,
            ))
            .cursor_pointer()
            .tip("Clear picks", look)
            .on_click(Diffui::on_action(Action::Changes(ChangesMsg::Clear), cx))
            .child(typography::centered(
                typography::line("Clear", font, text_size::BODY, ink),
                font,
                text_size::BODY,
            ))
            .children(keycaps);
        items.push(clear.into_any_element());
    }

    let bar = div()
        .id("picks-bar")
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
        .left(px(pane_left))
        .right(px(0.0))
        .bottom(px(BAR_BOTTOM))
        .px(px(16.0))
        .flex()
        .justify_center()
        .child(bar)
        .into_any_element()
}

/// The split prompt as a modal: what stays and what moves, the two
/// descriptions in the app's editor, and the footer. Empty when closed. A
/// stray click outside doesn't throw the typed descriptions away; Esc and
/// Cancel do.
pub(crate) fn split_modal(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    let Some(prompt) = split_prompt_shown(ui) else {
        return div().into_any_element();
    };
    let mono = look.mono;
    let label = revision_label(ui.active(), &prompt.revision);
    let size = ui.config.code_type.size;
    let width = SPLIT_WIDTH - 2.0 * crate::overlay_kit::DIALOG_PAD;
    // Two editors share what the window leaves under the card's fixed parts.
    let max = ((ui.window_size.height - crate::toolbar::modal_top() - SPLIT_CHROME_HEIGHT) / 2.0)
        .clamp(88.0, crate::input::editor_max_height(ui.window_size.height));
    let field = |caption: Prose,
                 content: &str,
                 id: &'static str,
                 placeholder: &'static str,
                 on_edit: fn(String) -> ChangesMsg,
                 window: &mut Window,
                 cx: &mut Cx| {
        let on_input = Diffui::listen(cx, move |this, text: &String, _, cx| {
            this.on_changes(on_edit(text.clone()), cx);
        });
        let spec = crate::input::FieldSpec::new(id, content)
            .placeholder(placeholder)
            .on_input(move |text, window, cx| on_input(&text, window, cx));
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(theme::space::SM))
            .child(
                div()
                    .text_size(px(text_size::BODY))
                    .child(caption.view(look, theme.muted_text)),
            )
            .child({
                let (editor, focused) = crate::input::multi_line_with_focus(spec, mono, window, cx);
                crate::field::field_well(focused, theme)
                    .h(px(crate::input::fit_height(
                        content, mono, size, width, 88.0, max,
                    )))
                    .child(editor.appearance(false).text_size(px(size)).h_full())
            })
    };
    let first = field(
        Prose::from("Stays in ").rev(label.clone()),
        &prompt.first,
        SPLIT_FIRST_EDITOR_ID,
        "Description of the changes that stay",
        ChangesMsg::SplitFirstEdited,
        window,
        cx,
    );
    let second = field(
        Prose::from("New change on top"),
        &prompt.second,
        SPLIT_SECOND_EDITOR_ID,
        "Description of the new change (empty is fine)",
        ChangesMsg::SplitSecondEdited,
        window,
        cx,
    );
    let cancel = look
        .cancel_button("split-cancel")
        .on_click(Diffui::on_action(
            Action::Changes(ChangesMsg::SplitCancel),
            cx,
        ));
    let split = look
        .button("split-confirm", "Split")
        .primary()
        .on_click(Diffui::on_action(
            Action::Changes(ChangesMsg::SplitConfirm),
            cx,
        ));

    let card = crate::overlay_kit::Dialog::new(&Prose::from("Split ").rev(label), look)
        .width(SPLIT_WIDTH)
        .lede(&prompt.summary)
        .child(first)
        .child(second)
        .bound_hint(
            ui,
            crate::commands::Context::Split,
            &["split.confirm"],
            "Split",
        )
        .bound_hint(
            ui,
            crate::commands::Context::Split,
            &["split.cancel"],
            "Cancel",
        )
        .buttons([cancel.into_any_element(), split.into_any_element()])
        .build();
    ui::input_modal("split-modal", card, crate::toolbar::modal_top()).into_any_element()
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
            blob: None,
            mode_change: None,
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

    /// The split prompt is a modal: Esc and ⌘⏎ are its own, and plain keys
    /// type into its editors instead of driving the graph behind it.
    #[test]
    fn the_split_prompt_owns_the_keyboard() {
        crate::tabs::tests::with_cx(|cx| {
            use crate::commands::Context;
            use crate::keymap::Chord;
            let mut ui = crate::tabs::tests::app();
            let tab = crate::tabs::tests::push_tab(&mut ui, "/tmp/repo");
            let state = ui.tab_mut(tab).unwrap();
            state.session.capabilities.mutate = true;
            state.session.document.files = vec![file("a.rs", &[H1]), file("b.rs", &[H1])];
            ui.open_split(
                RevisionSelection::WorkingCopy,
                whole_file("a.rs"),
                Prose::from("Keep a.rs"),
                cx,
            );
            assert!(split_shown(&ui));
            let context = ui.key_context();
            assert_eq!(context, Context::Split);
            let resolve = |chord: &str| {
                ui.resolve_chord(context, &Chord::parse(chord).expect("a chord"), false)
                    .map(|id| id.to_string())
            };
            assert_eq!(resolve("esc").as_deref(), Some("split.cancel"));
            assert_eq!(resolve("cmd+enter").as_deref(), Some("split.confirm"));
            assert_eq!(resolve("j"), None);
        });
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
            blob: None,
            mode_change: None,
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

    #[test]
    fn the_bar_counts_whole_files_hunks_and_lines_apart() {
        let selection = DiffSelection {
            files: vec![
                FileSelection {
                    path: "a.rs".to_owned(),
                    hunks: HunkPick::All,
                },
                FileSelection {
                    path: "b.rs".to_owned(),
                    hunks: HunkPick::Hunks(vec![
                        HunkRef {
                            index: 0,
                            header: String::new(),
                            lines: None,
                        },
                        HunkRef {
                            index: 1,
                            header: String::new(),
                            lines: Some(vec![LineRef::Added(0), LineRef::Added(1)]),
                        },
                    ]),
                },
            ],
        };
        assert_eq!(
            pick_detail(&selection),
            "1 file \u{b7} 1 hunk \u{b7} 2 lines"
        );
        assert!(!whole_files_only(&selection));
        let files = DiffSelection {
            files: selection.files[..1].to_vec(),
        };
        assert_eq!(pick_detail(&files), "1 file");
        assert!(whole_files_only(&files));
    }

    /// A partly picked hunk copies as a patch of its picked lines: an unpicked
    /// removal stays as context, an unpicked addition is left out.
    #[test]
    fn a_partial_hunk_copies_as_only_its_picked_lines() {
        let file = rewrite("src/a.rs");
        let picked = |row: usize| LineRef::of(&file.hunks[0].lines[row]).unwrap();
        let selection = DiffSelection {
            files: vec![FileSelection {
                path: file.path.clone(),
                hunks: HunkPick::Hunks(vec![HunkRef {
                    index: 0,
                    header: file.hunks[0].header.clone(),
                    lines: Some(vec![picked(1), picked(3)]),
                }]),
            }],
        };
        assert_eq!(
            selection_patch(std::slice::from_ref(&file), &selection),
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -1,4 +1,4 @@\n x\n-x\n x\n+x\n x\n"
        );
    }
}
