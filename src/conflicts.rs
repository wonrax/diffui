//! Resolving conflicted files from the diff pane: take a side or a base of
//! the jj conflict, for the whole file or one conflict region at a time; edit
//! the file with its conflict markers in place; or run the user's
//! `ui.merge-editor` and write its output back (`jj resolve`).
//!
//! The conflicted paths (with jj's labels for their terms) are fetched once
//! per loaded document of a conflicted revision. The merge tool runs on a
//! blocking thread here, never in the repository actor, so a merge session
//! left open for minutes doesn't stall the tab; its result lands as an
//! ordinary [`MutationOp::ResolveConflict`].

use diffui_core::{
    ConflictFile, ConflictPick, ConflictResolution, DiffFileStatus, JobId, MergeToolInvocation,
    MutationOp, RevisionSelection, hunk_conflict_region,
};
use iced::widget::{Space, column, container, responsive, row, text, text_editor};
use iced::{Element, Length, Padding, Task, alignment};

use crate::changes::{ChangesMsg, editable};
use crate::commands::CommandArg;
use crate::prose::Prose;
use crate::theme::{ThemeSpec, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Diffui, Message, PendingMutation, TabId, TabState, activity, icons, ui};

#[derive(Debug, Clone)]
pub(crate) enum ConflictMsg {
    Resolve {
        path: String,
        resolution: ConflictResolution,
    },
    OpenMergeTool(String),
    MergeToolDone {
        tab: TabId,
        activity: activity::ActivityId,
        result: Result<(Box<MergeToolInvocation>, ConflictResolution), String>,
    },
    /// Move the keyboard's region to the next (`1`) or previous (`-1`) one
    /// and scroll it into view.
    StepRegion(isize),
    /// Take `pick` for the keyboard's region of `path` (the selected file's
    /// when `None`).
    TakeRegion {
        path: Option<String>,
        pick: ConflictPick,
    },
    /// Open the marker editor on `path`.
    EditMarkers(String),
    EditorAction(text_editor::Action),
    EditorSave,
    EditorCancel,
}

/// Conflicts of one loaded document.
#[derive(Debug, Clone)]
struct LoadedConflicts {
    revision: RevisionSelection,
    document_id: u64,
    files: Vec<ConflictFile>,
}

/// The region the keyboard acts on: the `region`th of `path` in `revision`.
/// Resolving it renumbers the rest, so the index then names the region that
/// followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegionFocus {
    revision: RevisionSelection,
    path: String,
    region: usize,
}

/// The marker editor: one conflicted file as jj writes it, conflict markers
/// and all, edited in place and saved as a [`ConflictResolution::Content`].
#[derive(Debug)]
pub(crate) struct MarkerEditor {
    revision: RevisionSelection,
    path: String,
    original: String,
    content: text_editor::Content,
    /// The marker length jj wrote, to parse the edit back with.
    marker_len: usize,
    /// The save in flight; the editor is read-only meanwhile.
    saving: Option<activity::ActivityId>,
}

pub(crate) const MARKER_EDITOR_ID: &str = "conflict-marker-editor";

/// Per-tab conflict-resolution state.
#[derive(Debug, Default)]
pub(crate) struct ConflictsState {
    loaded: Option<LoadedConflicts>,
    job: Option<(JobId, RevisionSelection, u64)>,
    tool_job: Option<(JobId, String)>,
    /// Path whose merge tool is open.
    tool_running: Option<String>,
    focus: Option<RegionFocus>,
    /// Bumped when the focus moves by keyboard: the diff scrolls to it.
    reveal: u64,
    /// A region was just taken: scroll back to the focus once the reloaded
    /// diff's conflicts are in, rather than leave the reload at the top.
    reveal_on_load: bool,
    /// The marker text being read for the editor.
    text_job: Option<(JobId, RevisionSelection, String)>,
    editor: Option<MarkerEditor>,
}

impl ConflictsState {
    /// Whether `job` is one of ours (event routing asks).
    pub(crate) fn owns(&self, job: JobId) -> bool {
        self.job.as_ref().is_some_and(|(id, ..)| *id == job)
            || self.tool_job.as_ref().is_some_and(|(id, _)| *id == job)
            || self.text_job.as_ref().is_some_and(|(id, ..)| *id == job)
    }

    /// The conflicts of the document `tab` shows, once loaded.
    fn current(&self, tab: &TabState) -> Option<&[ConflictFile]> {
        self.loaded
            .as_ref()
            .filter(|loaded| {
                loaded.revision == tab.session.selected_revision
                    && loaded.document_id == tab.session.document_id
            })
            .map(|loaded| loaded.files.as_slice())
    }
}

fn conflict_of<'a>(tab: &'a TabState, path: &str) -> Option<&'a ConflictFile> {
    tab.changes
        .conflicts
        .current(tab)?
        .iter()
        .find(|conflict| conflict.path == path)
}

/// Whether `path` is conflicted in the revision `tab` shows (as far as is
/// known yet).
pub(crate) fn is_conflicted(tab: &TabState, path: &str) -> bool {
    conflict_of(tab, path).is_some()
}

/// The conflict at `path` in the active tab's revision.
pub(crate) fn conflict_at<'a>(ui: &'a Diffui, path: &str) -> Option<&'a ConflictFile> {
    conflict_of(ui.active(), path)
}

/// The resolve action for the conflicted file a command names: side `n`, or
/// the (first) base for `None`.
pub(crate) fn command_resolution(
    ui: &Diffui,
    arg: &CommandArg,
    side: Option<usize>,
) -> Option<Action> {
    let path = crate::changes::command_file_path(ui, arg)?;
    let conflict = conflict_at(ui, &path)?;
    if !conflict.pickable {
        return None;
    }
    let resolution = match side {
        Some(index) if index < conflict.sides.len() => ConflictResolution::Side(index),
        None if !conflict.bases.is_empty() => ConflictResolution::Base(0),
        _ => return None,
    };
    Some(Action::Changes(ChangesMsg::Conflict(
        ConflictMsg::Resolve { path, resolution },
    )))
}

pub(crate) fn command_merge_tool(ui: &Diffui, arg: &CommandArg) -> Option<Action> {
    let path = crate::changes::command_file_path(ui, arg)?;
    let conflict = conflict_at(ui, &path)?;
    conflict
        .merge_tool_blocker
        .is_none()
        .then_some(Action::Changes(ChangesMsg::Conflict(
            ConflictMsg::OpenMergeTool(path),
        )))
}

/// A conflict region the diff shows: where its hunk is, and which
/// `conflict k of n` it is (0-based `region`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionAt {
    file: usize,
    hunk: usize,
    region: usize,
    regions: usize,
}

/// Every region of the tab's conflicted files that resolves on its own, in
/// document order.
fn regions(tab: &TabState) -> Vec<RegionAt> {
    let Some(conflicts) = tab.changes.conflicts.current(tab) else {
        return Vec::new();
    };
    let by_region = |path: &str| {
        conflicts
            .iter()
            .any(|conflict| conflict.path == path && conflict.pickable && conflict.regions > 0)
    };
    tab.session
        .document
        .files
        .iter()
        .enumerate()
        .filter(|(_, file)| by_region(&file.path))
        .flat_map(|(file, diff)| {
            diff.hunks
                .iter()
                .enumerate()
                .filter_map(move |(hunk, view)| {
                    let (region, regions) = hunk_conflict_region(view)?;
                    Some(RegionAt {
                        file,
                        hunk,
                        region,
                        regions,
                    })
                })
        })
        .collect()
}

fn region_path<'a>(tab: &'a TabState, at: &RegionAt) -> &'a str {
    &tab.session.document.files[at.file].path
}

/// The focused region's index in `all`, when the keyboard has one: the
/// focus itself, or (once it was resolved) the last region of its file
/// before it.
fn focused(tab: &TabState, all: &[RegionAt]) -> Option<usize> {
    let focus = tab
        .changes
        .conflicts
        .focus
        .as_ref()
        .filter(|focus| focus.revision == tab.session.selected_revision)?;
    let in_file = |at: &&RegionAt| region_path(tab, at) == focus.path;
    all.iter()
        .position(|at| in_file(&at) && at.region == focus.region)
        .or_else(|| {
            all.iter()
                .rposition(|at| in_file(&at) && at.region < focus.region)
        })
}

/// The region a key or palette pick acts on: the focused one, else the
/// first of `path` (the selected file when `None`).
fn target_region(tab: &TabState, path: Option<&str>) -> Option<RegionAt> {
    let all = regions(tab);
    if let Some(index) = focused(tab, &all)
        && path.is_none_or(|path| region_path(tab, &all[index]) == path)
    {
        return Some(all[index]);
    }
    let path = match path {
        Some(path) => path,
        None => &tab.session.document.files.get(tab.selected_file)?.path,
    };
    all.into_iter().find(|at| region_path(tab, at) == path)
}

/// Whether `pick` names a term of `conflict`.
fn has_term(conflict: &ConflictFile, pick: ConflictPick) -> bool {
    match pick {
        ConflictPick::Side(index) => index < conflict.sides.len(),
        ConflictPick::Base(index) => index < conflict.bases.len(),
    }
}

/// The take-a-side action for the keyboard's region of the file a command
/// names (any region there, when the command names none).
pub(crate) fn command_region(ui: &Diffui, arg: &CommandArg, pick: ConflictPick) -> Option<Action> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let path = match arg {
        CommandArg::None => None,
        _ => Some(crate::changes::command_file_path(ui, arg)?),
    };
    let at = target_region(tab, path.as_deref())?;
    let path = region_path(tab, &at).to_owned();
    has_term(conflict_of(tab, &path)?, pick).then_some(Action::Changes(ChangesMsg::Conflict(
        ConflictMsg::TakeRegion {
            path: Some(path),
            pick,
        },
    )))
}

/// Whether the active diff shows a region the keyboard can move to.
pub(crate) fn has_regions(ui: &Diffui) -> bool {
    editable(ui.active()) && !regions(ui.active()).is_empty()
}

/// The marker editor action for the file a command names.
pub(crate) fn command_edit_markers(ui: &Diffui, arg: &CommandArg) -> Option<Action> {
    let path = crate::changes::command_file_path(ui, arg)?;
    conflict_at(ui, &path)?
        .pickable
        .then_some(Action::Changes(ChangesMsg::Conflict(
            ConflictMsg::EditMarkers(path),
        )))
}

/// The marker editor, when it belongs on screen: open on the file and
/// revision the diff shows.
pub(crate) fn editor_shown(ui: &Diffui) -> Option<&MarkerEditor> {
    let tab = ui.active();
    let file = tab.session.document.files.get(tab.selected_file)?;
    tab.changes.conflicts.editor.as_ref().filter(|editor| {
        editable(tab)
            && editor.revision == tab.session.selected_revision
            && editor.path == file.path
    })
}

/// How many conflicts `text` still opens: `<` markers of `marker_len` or
/// longer, ended by whitespace or the line, as jj parses them in any marker
/// style.
fn markers_left(text: &str, marker_len: usize) -> usize {
    text.lines()
        .filter(|line| {
            let run = line.bytes().take_while(|&byte| byte == b'<').count();
            run >= marker_len
                && line
                    .as_bytes()
                    .get(run)
                    .is_none_or(|byte| byte.is_ascii_whitespace())
        })
        .count()
}

impl Diffui {
    pub(crate) fn on_conflict(&mut self, msg: ConflictMsg) -> Task<Message> {
        match msg {
            ConflictMsg::Resolve { path, resolution } => {
                if !editable(self.active()) {
                    return Task::none();
                }
                let target = self.active().session.selected_revision.clone();
                // A region pick keeps the keyboard on that region's slot.
                if let ConflictResolution::Region { region, .. } = &resolution {
                    let conflicts = &mut self.active_mut().changes.conflicts;
                    conflicts.focus = Some(RegionFocus {
                        revision: target.clone(),
                        path: path.clone(),
                        region: *region,
                    });
                    conflicts.reveal_on_load = true;
                } else {
                    self.close_marker_editor(&target, &path);
                }
                self.start_changes_op(MutationOp::ResolveConflict {
                    target,
                    path,
                    resolution,
                })
            }
            ConflictMsg::StepRegion(by) => match self.step_region(by) {
                // The banner follows the region's file.
                Some(file) if file != self.active().selected_file => {
                    self.perform(Action::SelectFile(file))
                }
                _ => Task::none(),
            },
            ConflictMsg::TakeRegion { path, pick } => {
                let tab = self.active();
                let Some(at) = target_region(tab, path.as_deref()) else {
                    return Task::none();
                };
                let path = region_path(tab, &at).to_owned();
                self.on_conflict(ConflictMsg::Resolve {
                    path,
                    resolution: ConflictResolution::Region {
                        region: at.region,
                        regions: at.regions,
                        pick,
                    },
                })
            }
            ConflictMsg::EditMarkers(path) => self.open_marker_editor(path),
            ConflictMsg::EditorAction(action) => {
                if let Some(editor) = self.active_mut().changes.conflicts.editor.as_mut()
                    && editor.saving.is_none()
                {
                    editor.content.perform(action);
                }
                Task::none()
            }
            ConflictMsg::EditorSave => self.save_marker_editor(),
            ConflictMsg::EditorCancel => {
                let conflicts = &mut self.active_mut().changes.conflicts;
                if conflicts
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.saving.is_none())
                {
                    conflicts.editor = None;
                }
                Task::none()
            }
            ConflictMsg::OpenMergeTool(path) => self.open_merge_tool(path),
            ConflictMsg::MergeToolDone {
                tab,
                activity,
                result,
            } => {
                if let Some(state) = self.tab_mut(tab) {
                    state.changes.conflicts.tool_running = None;
                }
                match result {
                    Ok((invocation, resolution)) => {
                        if let Some(log) = self.activity_log_for(tab) {
                            log.finish(
                                activity,
                                activity::ActivityStatus::Done,
                                Some(format!("{} closed", invocation.tool)),
                            );
                        }
                        let label = Prose::from("Resolve ").path(&invocation.path);
                        let op = MutationOp::ResolveConflict {
                            target: invocation.revision,
                            path: invocation.path,
                            resolution,
                        };
                        self.resolve_in(tab, op, label)
                    }
                    Err(error) => {
                        if let Some(log) = self.activity_log_for(tab) {
                            log.append_output(activity, error.clone());
                            log.finish(
                                activity,
                                activity::ActivityStatus::Error,
                                Some(error.clone()),
                            );
                        }
                        self.push_error_toast("Merge tool".to_owned(), &error);
                        Task::none()
                    }
                }
            }
        }
    }

    /// Run `op` on `tab`, which may no longer be the active one (a merge
    /// tool finishing after a tab switch).
    fn resolve_in(&mut self, tab: TabId, op: MutationOp, label: Prose) -> Task<Message> {
        if self.tab_mut(tab).is_none() {
            return Task::none();
        }
        let (activity_id, _) = self.begin_activity(tab, label, false);
        self.run_mutation(PendingMutation {
            op,
            tab_id: tab,
            activity_id,
            allow_immutable: false,
        })
    }

    /// Move the keyboard's region `by` regions through the diff, starting
    /// from the selected file when none is focused yet. Returns the file it
    /// lands in.
    fn step_region(&mut self, by: isize) -> Option<usize> {
        let tab = self.active();
        let all = regions(tab);
        if all.is_empty() {
            return None;
        }
        let next = match focused(tab, &all) {
            Some(index) => index.saturating_add_signed(by).min(all.len() - 1),
            None if by > 0 => all
                .iter()
                .position(|at| at.file >= tab.selected_file)
                .unwrap_or(all.len() - 1),
            None => all
                .iter()
                .rposition(|at| at.file <= tab.selected_file)
                .unwrap_or(0),
        };
        let at = all[next];
        let focus = RegionFocus {
            revision: tab.session.selected_revision.clone(),
            path: region_path(tab, &at).to_owned(),
            region: at.region,
        };
        let conflicts = &mut self.active_mut().changes.conflicts;
        conflicts.focus = Some(focus);
        conflicts.reveal += 1;
        Some(at.file)
    }

    /// Ask for `path`'s text with its markers; the editor opens when it
    /// arrives.
    fn open_marker_editor(&mut self, path: String) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let state = self.active_mut();
        if !editable(state) {
            return Task::none();
        }
        let revision = state.session.selected_revision.clone();
        let conflicts = &state.changes.conflicts;
        if let Some(editor) = &conflicts.editor
            && editor.revision == revision
            && editor.path == path
        {
            // Already open (with any edits in it): just show it.
            return self.select_file_path(&path);
        }
        if conflicts
            .editor
            .as_ref()
            .is_some_and(|editor| editor.saving.is_some())
        {
            return Task::none();
        }
        let job = state.session.next_job();
        state.changes.conflicts.text_job = Some((job, revision.clone(), path.clone()));
        let select = self.select_file_path(&path);
        Task::batch([
            select,
            self.send(
                tab,
                diffui_core::Command::ConflictText {
                    job,
                    revision,
                    path,
                },
            ),
        ])
    }

    /// Show `path` in the diff pane, if the document has it.
    fn select_file_path(&mut self, path: &str) -> Task<Message> {
        match self
            .active()
            .session
            .document
            .files
            .iter()
            .position(|file| file.path == path)
        {
            Some(index) if index != self.active().selected_file => {
                self.perform(Action::SelectFile(index))
            }
            _ => Task::none(),
        }
    }

    fn save_marker_editor(&mut self) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let Some(editor) = editor_shown(self) else {
            return Task::none();
        };
        if editor.saving.is_some() {
            return Task::none();
        }
        let text = editor.content.text();
        if text == editor.original {
            // Nothing edited: nothing to write.
            self.active_mut().changes.conflicts.editor = None;
            return Task::none();
        }
        let op = MutationOp::ResolveConflict {
            target: editor.revision.clone(),
            path: editor.path.clone(),
            resolution: ConflictResolution::Content {
                content: text.into_bytes(),
                marker_len: Some(editor.marker_len),
            },
        };
        let label = Prose::from("Resolve ").path(&editor.path);
        let (activity_id, _) = self.begin_activity(tab, label, false);
        if let Some(editor) = self.active_mut().changes.conflicts.editor.as_mut() {
            editor.saving = Some(activity_id);
        }
        self.run_mutation(PendingMutation {
            op,
            tab_id: tab,
            activity_id,
            allow_immutable: false,
        })
    }

    /// A mutation finished (`saved`) or was dropped: the marker editor
    /// saving it closes, or opens up for another try.
    pub(crate) fn marker_save_finished(
        &mut self,
        tab: TabId,
        activity: activity::ActivityId,
        saved: bool,
    ) {
        let Some(conflicts) = self.tab_mut(tab).map(|state| &mut state.changes.conflicts) else {
            return;
        };
        if conflicts
            .editor
            .as_ref()
            .is_some_and(|editor| editor.saving == Some(activity))
        {
            if saved {
                conflicts.editor = None;
            } else if let Some(editor) = conflicts.editor.as_mut() {
                editor.saving = None;
            }
        }
    }

    /// Drop an unsaved marker editor on `path`, which a whole-file resolve
    /// is about to make stale.
    fn close_marker_editor(&mut self, revision: &RevisionSelection, path: &str) {
        let conflicts = &mut self.active_mut().changes.conflicts;
        if conflicts.editor.as_ref().is_some_and(|editor| {
            editor.saving.is_none() && &editor.revision == revision && editor.path == path
        }) {
            conflicts.editor = None;
        }
    }

    fn open_merge_tool(&mut self, path: String) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let state = self.active_mut();
        let conflicts = &state.changes.conflicts;
        if !editable(state) || conflicts.tool_job.is_some() || conflicts.tool_running.is_some() {
            return Task::none();
        }
        let job = state.session.next_job();
        let revision = state.session.selected_revision.clone();
        state.changes.conflicts.tool_job = Some((job, path.clone()));
        self.send(
            tab,
            diffui_core::Command::PrepareMergeTool {
                job,
                revision,
                path,
            },
        )
    }

    /// Ask for the conflicts of the active tab's revision when it has some
    /// and they aren't loaded yet. Cheap enough to call after every message.
    pub(crate) fn sync_conflicts(&mut self) -> Task<Message> {
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        let tab = self.active();
        if !editable(tab) {
            return Task::none();
        }
        let revision = tab.session.selected_revision.clone();
        let document_id = tab.session.document_id;
        let conflicts = &tab.changes.conflicts;
        let loaded = conflicts
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.revision == revision && loaded.document_id == document_id);
        let asked = conflicts
            .job
            .as_ref()
            .is_some_and(|(_, asked, id)| *asked == revision && *id == document_id);
        if loaded || asked || tab.session.diff_in_flight() {
            return Task::none();
        }
        let row = match &revision {
            RevisionSelection::WorkingCopy => tab.session.commits.working_copy(),
            RevisionSelection::Commit(hex) => tab.session.commits.find_by_commit_id(hex),
        };
        let conflicted = row.is_some_and(|row| row.has_conflict())
            || tab
                .session
                .document
                .files
                .iter()
                .any(|file| file.status == DiffFileStatus::Conflicted);
        let state = self.active_mut();
        if !conflicted {
            // Nothing to ask; remember the answer so the check stays cheap.
            state.changes.conflicts.loaded = Some(LoadedConflicts {
                revision,
                document_id,
                files: Vec::new(),
            });
            state.changes.conflicts.reveal_on_load = false;
            return Task::none();
        }
        let job = state.session.next_job();
        state.changes.conflicts.job = Some((job, revision.clone(), document_id));
        self.send(tab_id, diffui_core::Command::ConflictInfo { job, revision })
    }

    /// Route a repository event answering one of our jobs. `None` when the
    /// event isn't ours.
    pub(crate) fn take_conflict_job(
        &mut self,
        tab: TabId,
        event: &diffui_core::Event,
    ) -> Option<Task<Message>> {
        use diffui_core::Payload;
        let job = event.job()?;
        let conflicts = &mut self.tab_mut(tab)?.changes.conflicts;
        if !conflicts.owns(job) {
            return None;
        }
        Some(match &event.payload {
            Payload::ConflictInfoLoaded { files, .. } => {
                if let Some((_, revision, document_id)) = conflicts.job.take() {
                    if std::mem::take(&mut conflicts.reveal_on_load) {
                        // Resolving rewrote a commit: the focus follows it to
                        // its new id.
                        if let Some(focus) = conflicts.focus.as_mut() {
                            focus.revision = revision.clone();
                        }
                        conflicts.reveal += 1;
                    }
                    conflicts.loaded = Some(LoadedConflicts {
                        revision,
                        document_id,
                        files: files.clone(),
                    });
                }
                Task::none()
            }
            Payload::MergeToolPrepared { invocation, .. } => {
                let path = conflicts
                    .tool_job
                    .take()
                    .map(|(_, path)| path)
                    .unwrap_or_else(|| invocation.path.clone());
                conflicts.tool_running = Some(path.clone());
                let invocation = Box::new(invocation.clone());
                let (activity, _) = self.begin_activity(
                    tab,
                    Prose::from("Resolving ")
                        .path(&path)
                        .text(format!(" in {}", invocation.tool)),
                    false,
                );
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let result = invocation.run();
                            invocation.cleanup();
                            result.map(|resolution| (invocation, resolution))
                        })
                        .await
                        .unwrap_or_else(|error| Err(format!("the merge tool thread died: {error}")))
                    },
                    move |result| {
                        Message::Changes(ChangesMsg::Conflict(ConflictMsg::MergeToolDone {
                            tab,
                            activity,
                            result,
                        }))
                    },
                )
            }
            Payload::ConflictTextLoaded {
                text, marker_len, ..
            } => {
                let Some((_, revision, path)) = conflicts.text_job.take() else {
                    return Some(Task::none());
                };
                conflicts.editor = Some(MarkerEditor {
                    revision,
                    path,
                    original: text.clone(),
                    content: text_editor::Content::with_text(text),
                    marker_len: *marker_len,
                    saving: None,
                });
                iced::widget::operation::focus(iced::widget::Id::new(MARKER_EDITOR_ID))
            }
            Payload::Failed { error, .. } => {
                let tool = conflicts.tool_job.take_if(|(id, _)| *id == job);
                let text = conflicts.text_job.take_if(|(id, ..)| *id == job);
                conflicts.job.take_if(|(id, ..)| *id == job);
                if tool.is_some() {
                    self.push_error_toast("Merge tool".to_owned(), &error.to_string());
                } else if text.is_some() {
                    self.push_error_toast("Edit markers".to_owned(), &error.to_string());
                }
                Task::none()
            }
            _ => {
                conflicts.job.take_if(|(id, ..)| *id == job);
                conflicts.tool_job.take_if(|(id, _)| *id == job);
                conflicts.text_job.take_if(|(id, ..)| *id == job);
                Task::none()
            }
        })
    }
}

/// jj's label for a conflict term, taken apart:
/// `nlqwxzwn 7dd24e73 "subject" (rebase destination)`.
#[derive(Debug, Default, PartialEq, Eq)]
struct TermLabel<'a> {
    change: Option<&'a str>,
    subject: Option<&'a str>,
    role: Option<&'a str>,
}

fn parse_label(label: &str) -> TermLabel<'_> {
    let change = label
        .split_whitespace()
        .next()
        .map(|word| word.trim_end_matches(','))
        .filter(|word| word.len() == 8 && word.bytes().all(|byte| (b'k'..=b'z').contains(&byte)));
    let subject = match (label.find('"'), label.rfind('"')) {
        (Some(start), Some(end)) if end > start + 1 => Some(&label[start + 1..end]),
        _ => None,
    };
    let tail = label.rfind('"').map_or(label, |end| &label[end + 1..]);
    let role = tail
        .trim_end()
        .strip_suffix(')')
        .and_then(|rest| rest.rfind('(').map(|start| &rest[start + 1..]))
        .filter(|role| !role.is_empty());
    TermLabel {
        change,
        subject,
        role,
    }
}

/// `name` cut to `max` characters.
fn shorten(name: &str, max: usize) -> String {
    let mut short: String = name.chars().take(max).collect();
    if name.chars().count() > max {
        short.push('\u{2026}');
    }
    short
}

/// A term's button label: the change id it came from and what jj calls it
/// (its role in a rebase, else its subject), or `Side n` with no labels.
fn term_label(term: &diffui_core::ConflictTerm, index: usize) -> (Option<String>, String) {
    let label = parse_label(&term.label);
    let name = label.role.or(label.subject).map(|name| shorten(name, 28));
    let deleted = if term.absent { " (deleted)" } else { "" };
    match (label.change, name) {
        (Some(change), Some(name)) => (Some(change.to_owned()), format!(" {name}{deleted}")),
        (Some(change), None) => (Some(change.to_owned()), deleted.to_owned()),
        (None, _) => (None, format!("Side {}{deleted}", index + 1)),
    }
}

fn base_label(conflict: &ConflictFile, index: usize) -> String {
    if conflict.bases.len() == 1 {
        "Base".to_owned()
    } else {
        format!("Base {}", index + 1)
    }
}

/// Every term of `conflict` a region can take, sides first.
fn picks(conflict: &ConflictFile) -> Vec<ConflictPick> {
    (0..conflict.sides.len())
        .map(ConflictPick::Side)
        .chain((0..conflict.bases.len()).map(ConflictPick::Base))
        .collect()
}

fn resolve(path: &str, resolution: ConflictResolution) -> Message {
    Message::Changes(ChangesMsg::Conflict(ConflictMsg::Resolve {
        path: path.to_owned(),
        resolution,
    }))
}

/// The take-a-side buttons on every conflict region's hunk header.
pub(crate) fn hunk_actions<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<crate::hunk_actions::HunkActions<'a, Message>> {
    use crate::hunk_actions::{Button, Row, Run};
    let tab = ui.active();
    if !editable(tab) || editor_shown(ui).is_some() {
        return None;
    }
    let all = regions(tab);
    if all.is_empty() {
        return None;
    }
    let conflicts = tab.changes.conflicts.current(tab)?;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let focus = focused(tab, &all);
    let run = |text: String, font, color| Run { text, font, color };
    let mut rows = Vec::with_capacity(all.len());
    let mut targets = Vec::with_capacity(all.len());
    for (index, at) in all.iter().enumerate() {
        let path = region_path(tab, at);
        let conflict = conflicts.iter().find(|conflict| conflict.path == path)?;
        let buttons = picks(conflict)
            .into_iter()
            .map(|pick| Button {
                runs: match pick {
                    ConflictPick::Side(side) => match term_label(&conflict.sides[side], side) {
                        (Some(change), name) => {
                            vec![run(change, mono, theme.accent), run(name, font, theme.text)]
                        }
                        (None, name) => vec![run(name, font, theme.text)],
                    },
                    ConflictPick::Base(base) => {
                        vec![run(base_label(conflict, base), font, theme.text)]
                    }
                },
            })
            .collect();
        rows.push(Row {
            file: at.file,
            hunk: at.hunk,
            focused: focus == Some(index),
            lead: Some(run("Take".to_owned(), font, theme.muted_text)),
            buttons,
        });
        targets.push((path.to_owned(), at.region, at.regions, picks(conflict)));
    }
    let fill = |status| match crate::theme::raised_button_style(theme, status).background {
        Some(iced::Background::Color(color)) => color,
        _ => theme.panel_background,
    };
    Some(crate::hunk_actions::HunkActions {
        rows,
        style: crate::hunk_actions::Style {
            size: text_size::UI,
            fill: fill(iced::widget::button::Status::Active),
            hover_fill: fill(iced::widget::button::Status::Hovered),
            border: theme.border,
            focus: theme.accent,
            radius: crate::theme::radius::BUTTON,
        },
        on_press: Box::new(move |row, button| {
            let (path, region, regions, picks) = &targets[row];
            resolve(
                path,
                ConflictResolution::Region {
                    region: *region,
                    regions: *regions,
                    pick: picks[button],
                },
            )
        }),
        reveal: focus.map(|index| {
            (
                all[index].file,
                all[index].hunk,
                tab.changes.conflicts.reveal,
            )
        }),
    })
}

/// The marker editor in place of the diff: the file with jj's conflict
/// markers in the app's editor, and a footer saying how many conflicts are
/// left in it.
pub(crate) fn marker_editor<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Option<Element<'a, Message>> {
    const GAP: f32 = 8.0;
    let editor = editor_shown(ui)?;
    let saving = editor.saving.is_some();
    let mono = ui.config.mono_font;
    let size = ui.config.code_type.size;
    let input = responsive(move |bounds| {
        let height = (bounds.height - crate::theme::control::COMPACT - GAP).max(60.0);
        let on_action =
            |action| Message::Changes(ChangesMsg::Conflict(ConflictMsg::EditorAction(action)));
        let input = crate::input::multi_line(
            &editor.content,
            crate::input::MultiLine {
                id: Some(MARKER_EDITOR_ID),
                placeholder: "",
                font: mono,
                size,
                height,
            },
            theme,
            (!saving).then_some(on_action),
        );
        column![input, editor_footer(ui, theme, editor)].spacing(GAP)
    });
    Some(
        container(input)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([10.0, crate::theme::space::XL])
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into(),
    )
}

/// How many conflicts the edit still holds, Cancel and Save.
fn editor_footer<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    editor: &'a MarkerEditor,
) -> Element<'a, Message> {
    let font = ui.config.ui_font;
    let saving = editor.saving.is_some();
    let hint = match markers_left(&editor.content.text(), editor.marker_len) {
        0 => "No conflict markers left: saving resolves the file".to_owned(),
        1 => "1 conflict left: saving keeps it conflicted".to_owned(),
        n => format!("{n} conflicts left: saving keeps them conflicted"),
    };
    let hint = typography::centered(
        text(hint)
            .size(text_size::UI)
            .font(font)
            .color(theme.subtle_text),
        font,
        text_size::UI,
    );
    let keyed = |button: iced::widget::Button<'a, Message>, key: &'static str| {
        ui::tooltip(
            button,
            key,
            iced::widget::tooltip::Position::Top,
            font,
            theme,
        )
    };
    let message = |msg| Message::Changes(ChangesMsg::Conflict(msg));
    let cancel = ui::cancel_button(ui::Size::Compact, font, theme)
        .on_press_maybe((!saving).then(|| message(ConflictMsg::EditorCancel)));
    let save = ui::text_button(
        None,
        if saving { "Saving\u{2026}" } else { "Save" },
        ui::Variant::Primary,
        ui::Size::Compact,
        font,
        theme,
    )
    .on_press_maybe((!saving).then(|| message(ConflictMsg::EditorSave)));
    ui::button_row(
        Some(hint),
        [keyed(cancel, "Esc"), keyed(save, "\u{2318}\u{21b5}")],
    )
    .into()
}

/// The resolve strip above the diff when the selected file is conflicted.
pub(crate) fn conflict_banner<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    let tab = ui.active();
    if !editable(tab) {
        return None;
    }
    let file = tab.session.document.files.get(tab.selected_file)?;
    let conflict = conflict_of(tab, &file.path)?;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let path = conflict.path.as_str();
    let tip = |content: Element<'a, Message>, label: String| {
        ui::tooltip(
            content,
            label,
            iced::widget::tooltip::Position::Bottom,
            font,
            theme,
        )
    };
    let mut buttons = row![].spacing(6).align_y(alignment::Vertical::Center);
    if conflict.pickable {
        for (index, side) in conflict.sides.iter().enumerate() {
            let label = match term_label(side, index) {
                (Some(change), name) => Prose::new().rev(change).text(name),
                (None, name) => Prose::from(name),
            };
            let button = ui::button(
                label
                    .view(font, mono, theme.text, theme)
                    .size(text_size::UI)
                    .wrapping(text::Wrapping::None),
                ui::Variant::Raised,
                ui::Size::Compact,
                theme,
            )
            .on_press(resolve(path, ConflictResolution::Side(index)));
            buttons = buttons.push(tip(
                button.into(),
                format!("Take side {} as the whole file: {}", index + 1, side.label),
            ));
        }
        for (index, base) in conflict.bases.iter().enumerate() {
            let button = ui::text_button(
                None,
                base_label(conflict, index),
                ui::Variant::Raised,
                ui::Size::Compact,
                font,
                theme,
            )
            .on_press(resolve(path, ConflictResolution::Base(index)));
            buttons = buttons.push(tip(
                button.into(),
                format!("Drop every side's change: {}", base.label),
            ));
        }
    }
    let conflicts = &tab.changes.conflicts;
    let running = conflicts.tool_running.as_deref() == Some(path)
        || conflicts
            .tool_job
            .as_ref()
            .is_some_and(|(_, job_path)| job_path == path);
    let mut tool = ui::text_button(
        Some((icons::GIT_MERGE, theme.muted_text)),
        if running {
            "Waiting for the merge tool\u{2026}"
        } else {
            "Merge tool"
        },
        ui::Variant::Ghost,
        ui::Size::Compact,
        font,
        theme,
    );
    if conflict.merge_tool_blocker.is_none() && !running {
        tool = tool.on_press(Message::Changes(ChangesMsg::Conflict(
            ConflictMsg::OpenMergeTool(path.to_owned()),
        )));
    }
    let editing = editor_shown(ui).is_some();
    let mut edit = ui::text_button(
        Some((icons::PENCIL, theme.muted_text)),
        "Edit markers",
        ui::Variant::Ghost,
        ui::Size::Compact,
        font,
        theme,
    );
    if conflict.pickable && !editing {
        edit = edit.on_press(Message::Changes(ChangesMsg::Conflict(
            ConflictMsg::EditMarkers(path.to_owned()),
        )));
    }
    buttons = buttons
        .push(Space::new().width(Length::Fixed(6.0)))
        .push(tip(
            edit.into(),
            if conflict.pickable {
                "Edit the file with jj's conflict markers. Markers left in keep it conflicted"
                    .to_owned()
            } else {
                "A term isn't a regular file".to_owned()
            },
        ))
        .push(tip(
            tool.into(),
            conflict.merge_tool_blocker.clone().unwrap_or_else(|| {
                "Runs ui.merge-editor from your jj config and writes the result back".to_owned()
            }),
        ));

    let regions = if conflict.regions > 0 {
        format!(
            ", {} region{}",
            conflict.regions,
            if conflict.regions == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let headline = typography::baseline_row(
        [
            Aligned::glyph(
                icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.conflict_marker),
                ui::ICON_SIZE,
            ),
            Aligned::text(
                Prose::new()
                    .path(path)
                    .text(format!(
                        " is conflicted: {} sides{regions}.",
                        conflict.sides.len()
                    ))
                    .view(font, mono, theme.text, theme)
                    .size(text_size::UI),
                font,
                text_size::UI,
            ),
        ],
        6.0,
    );
    let hint = if editing {
        "Editing the markers below."
    } else if !conflict.pickable {
        "A term is a symlink, directory or submodule. Resolve it in a terminal."
    } else if conflict.regions > 0 {
        "Take a side for the whole file, or for each region below."
    } else {
        "This conflict only resolves whole: take a side, or edit the markers."
    };
    let hint = text(hint)
        .size(text_size::CAPTION)
        .font(font)
        .color(theme.muted_text);
    Some(
        container(column![headline, buttons.wrap().vertical_spacing(6), hint].spacing(6))
            .width(Length::Fill)
            .padding(Padding::from([8.0, 16.0]))
            .style(move |_| {
                container::Style::default()
                    .background(iced::Color {
                        a: 0.08,
                        ..theme.conflict_marker
                    })
                    .border(iced::Border {
                        width: 1.0,
                        color: theme.border,
                        radius: 0.0.into(),
                    })
            })
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_come_apart_into_id_subject_and_role() {
        assert_eq!(
            parse_label("nlqwxzwn 7dd24e73 \"left edit\""),
            TermLabel {
                change: Some("nlqwxzwn"),
                subject: Some("left edit"),
                role: None,
            }
        );
        assert_eq!(
            parse_label("nlqwxzwn 7dd24e73 \"fix (again)\" (rebase destination)"),
            TermLabel {
                change: Some("nlqwxzwn"),
                subject: Some("fix (again)"),
                role: Some("rebase destination"),
            }
        );
        assert_eq!(
            parse_label("kkmpptxz 12345678, nlqwxzwn 7dd24e73 (parents of rebased revision)"),
            TermLabel {
                change: Some("kkmpptxz"),
                subject: None,
                role: Some("parents of rebased revision"),
            }
        );
        assert_eq!(parse_label("side #1"), TermLabel::default());
    }

    #[test]
    fn markers_left_counts_only_full_length_openers() {
        let text = "a\n<<<<<<<<< conflict 1 of 2\nx\n>>>>>>>>> conflict 1 of 2 ends\n\
                    <<<<<<< conflict 2 of 2\n";
        assert_eq!(markers_left(text, 9), 1);
        assert_eq!(markers_left(text, 7), 2);
        // Git-style openers name a side instead.
        assert_eq!(markers_left("<<<<<<< rysurqmy c676892a\nx\n", 7), 1);
        assert_eq!(markers_left("<<<<<<<x\n", 7), 0);
    }
}
