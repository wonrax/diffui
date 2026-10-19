//! History views.
//!
//! * The **History** main view — a third lens beside Diff and Source — lists
//!   the operation log on the left (snapshot runs folded) and, on the right,
//!   what the selected operation changed with the diff of one changed commit,
//!   plus restore/revert. The same view shows a revision's evolution log
//!   (`jj evolog`) with interdiffs between versions.
//! * **File history** narrows the graph to the revisions that touched a path,
//!   with a banner above the revision list that undoes it.
//! * **Annotate** adds a blame column to the source browser.
//!
//! All the repository work is `Command::History` on the tab's actor; this
//! module owns the per-tab state, routes its own jobs, and builds the panes.

use std::collections::HashSet;

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    widget::{Space, button, column, container, row, text},
};

use diffui_core::history::{
    ChangedCommit, CommitBrief, CommitChangeKind, EvologEntry, FileAnnotation, HistoryDiffTarget,
    HistoryRequest, HistoryResponse, OperationChanges, OperationEntry, RefChange, RefKind,
    file_history_revset, now_millis, relative_age,
};
use diffui_core::{DiffDocument, JobId, MutationOp, RevisionDetails, RevisionSelection, Vcs};

use crate::diff_view::{AnnotationGutter, AnnotationLabel, DiffFileView, DiffView};
use crate::prose::Prose;
use crate::theme::{
    ThemeSpec, chip_background, diff_palette, diff_panel_style, emphasis_font, file_status_color,
    radius, sidebar_panel_style, space, text_size,
};
use crate::{
    Action, ConfirmDialog, Diffui, MainView, Message, PendingMutation, TabId, activity, icons,
    modes::Mode, ui,
};

/// Operations read per page of the op log.
const OP_LOG_PAGE: usize = 300;

/// Width of the blame column, in monospace cells: change id, author, age.
const ANNOTATION_CHARS: usize = 25;
const ANNOTATION_AUTHOR_CHARS: usize = 10;

/// What the History view lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum HistoryKind {
    #[default]
    OpLog,
    /// The versions of one revision's change.
    Evolog {
        revision: RevisionSelection,
        /// The change id the view was opened for, for the header.
        label: String,
    },
}

/// What an evolog version is diffed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EvologDiff {
    /// The interdiff from the version it replaced (`jj evolog -p`).
    #[default]
    Previous,
    /// The version's own patch against its parents.
    Parents,
}

/// Every intent the history views produce, from their widgets, the palette,
/// or a context menu.
#[derive(Debug, Clone)]
pub(crate) enum HistoryAction {
    ShowOpLog,
    ShowEvolog(RevisionSelection),
    /// Re-read the op log (or the evolog) from the repository.
    Reload,
    LoadMoreOperations,
    SelectOperation(String),
    /// Fold or unfold the run of snapshot operations starting at this id.
    ToggleSnapshotRun(String),
    /// Show the diff of the n-th commit the selected operation changed.
    SelectChange(usize),
    SelectVersion(usize),
    SetEvologDiff(EvologDiff),
    /// Ask before restoring the repository to this operation.
    RestoreOperation(String),
    /// Ask before reverting this operation.
    RevertOperation(String),
    /// Show a (visible) commit in the diff view.
    OpenRevision(String),
    DiffScrolled(f32),
    /// The history diff's scroll-tracked file changed; nothing to do.
    DiffFileChanged,
    /// Narrow the graph to the revisions that touched `path`, up to
    /// `revision`.
    FileHistory {
        revision: RevisionSelection,
        path: String,
    },
    /// The file currently on screen (diff or source), for the palette.
    CurrentFileHistory,
    ClearFileFilter,
    ToggleAnnotate,
    /// A click in the blame column, by 0-based line.
    AnnotationClicked(usize),
}

/// A graph narrowed to one file's history.
#[derive(Debug, Clone)]
pub(crate) struct FileFilter {
    pub(crate) path: String,
    /// The revset applied; the filter is live only while the tab still has it.
    pub(crate) revset: String,
    /// What the revset was before, restored by the banner's ✕.
    pub(crate) previous: String,
}

#[derive(Debug)]
struct HistoryDiff {
    target: HistoryDiffTarget,
    document: DiffDocument,
    details: Option<RevisionDetails>,
    doc_id: u64,
}

#[derive(Debug)]
struct AnnotationView {
    annotation: FileAnnotation,
    labels: Vec<AnnotationLabel>,
}

/// One tab's history-view state.
#[derive(Debug, Default)]
pub(crate) struct HistoryState {
    pub(crate) kind: HistoryKind,
    ops: Vec<OperationEntry>,
    ops_truncated: bool,
    ops_limit: usize,
    ops_job: Option<JobId>,
    /// A re-read is in flight: the listed ops are replaced by its first
    /// batch rather than cleared up front, so the list doesn't flash empty.
    ops_stale: bool,
    /// The op head the listed log was read at (`None` until first read); a
    /// move re-reads it.
    ops_read_at: Option<Option<String>>,
    expanded_runs: HashSet<String>,
    selected_op: Option<String>,
    op_changes: Option<OperationChanges>,
    op_job: Option<JobId>,
    selected_change: Option<usize>,
    evolog: Vec<EvologEntry>,
    evolog_job: Option<JobId>,
    selected_version: Option<usize>,
    evolog_diff: EvologDiff,
    diff: Option<HistoryDiff>,
    diff_job: Option<(JobId, HistoryDiffTarget)>,
    diff_scroll: f32,
    /// The last failure of a list or detail read, shown in place of it.
    error: Option<String>,
    pub(crate) file_filter: Option<FileFilter>,
    /// The document id the file filter last jumped to its file in.
    filter_jumped: Option<u64>,
    /// The filtered graph is loading; when it lands, its newest revision is
    /// selected so the diff shows a commit that actually touched the file.
    filter_loading: bool,
    /// A blame click's file, opened in the next diff that isn't the one on
    /// screen when it was clicked.
    blame_jump: Option<(String, Option<u64>)>,
    annotate: bool,
    annotation: Option<AnnotationView>,
    annotation_job: Option<(JobId, RevisionSelection, String)>,
    annotation_error: Option<(RevisionSelection, String, String)>,
}

impl HistoryState {
    /// Whether `job` is one of this view's reads.
    pub(crate) fn owns(&self, job: JobId) -> bool {
        self.ops_job == Some(job)
            || self.op_job == Some(job)
            || self.evolog_job == Some(job)
            || self.diff_job.as_ref().is_some_and(|(id, _)| *id == job)
            || self
                .annotation_job
                .as_ref()
                .is_some_and(|(id, ..)| *id == job)
    }

    fn selected_operation(&self) -> Option<&OperationEntry> {
        let id = self.selected_op.as_deref()?;
        self.ops.iter().find(|op| op.id == id)
    }

    /// Whether the annotation on hand (or in flight, or failed) is for this
    /// file, so the sync doesn't ask again.
    fn annotation_settled(&self, revision: &RevisionSelection, path: &str) -> bool {
        let matches = |r: &RevisionSelection, p: &str| r == revision && p == path;
        self.annotation
            .as_ref()
            .is_some_and(|view| matches(&view.annotation.revision, &view.annotation.path))
            || self
                .annotation_job
                .as_ref()
                .is_some_and(|(_, r, p)| matches(r, p))
            || self
                .annotation_error
                .as_ref()
                .is_some_and(|(r, p, _)| matches(r, p))
    }
}

impl Diffui {
    pub(crate) fn perform_history(&mut self, action: HistoryAction) -> Task {
        let Some(tab) = self.active_tab_id() else {
            return iced::Task::none();
        };
        match action {
            HistoryAction::ShowOpLog => {
                let history = &mut self.active_mut().history;
                if history.kind != HistoryKind::OpLog {
                    history.kind = HistoryKind::OpLog;
                    history.diff = None;
                    history.error = None;
                    // Back from an evolog: the op's own diff comes back.
                    if let Some(index) = history.selected_change {
                        return iced::Task::batch([
                            self.enter_history_view(),
                            self.history_select_change(tab, index),
                        ]);
                    }
                }
                self.enter_history_view()
            }
            HistoryAction::ShowEvolog(revision) => {
                let label = self.revision_short_label(&revision);
                let history = &mut self.active_mut().history;
                history.kind = HistoryKind::Evolog {
                    revision: revision.clone(),
                    label,
                };
                history.evolog.clear();
                history.selected_version = None;
                history.diff = None;
                history.error = None;
                let job = JobId::next();
                history.evolog_job = Some(job);
                let enter = self.enter_history_view();
                let send = self.history_send(tab, job, HistoryRequest::Evolog { revision });
                iced::Task::batch([enter, send])
            }
            HistoryAction::Reload => match self.active().history.kind.clone() {
                HistoryKind::OpLog => self.reload_op_log(tab),
                HistoryKind::Evolog { revision, .. } => {
                    self.perform_history(HistoryAction::ShowEvolog(revision))
                }
            },
            HistoryAction::LoadMoreOperations => {
                let history = &mut self.active_mut().history;
                history.ops_limit += OP_LOG_PAGE;
                self.reload_op_log(tab)
            }
            HistoryAction::SelectOperation(id) => self.history_select_operation(tab, id),
            HistoryAction::ToggleSnapshotRun(id) => {
                let runs = &mut self.active_mut().history.expanded_runs;
                if !runs.remove(&id) {
                    runs.insert(id);
                }
                iced::Task::none()
            }
            HistoryAction::SelectChange(index) => self.history_select_change(tab, index),
            HistoryAction::SelectVersion(index) => self.history_select_version(tab, index),
            HistoryAction::SetEvologDiff(mode) => {
                self.active_mut().history.evolog_diff = mode;
                match self.active().history.selected_version {
                    Some(index) => self.history_select_version(tab, index),
                    None => iced::Task::none(),
                }
            }
            HistoryAction::RestoreOperation(id) => self.confirm_operation(tab, id, true),
            HistoryAction::RevertOperation(id) => self.confirm_operation(tab, id, false),
            HistoryAction::OpenRevision(commit_id) => {
                let selection = self.active().session.canonical_selection(&commit_id);
                iced::Task::batch([
                    self.perform(Action::SetMainView(MainView::Diff)),
                    self.perform(Action::SelectRevision(selection)),
                ])
            }
            HistoryAction::DiffScrolled(offset) => {
                self.active_mut().history.diff_scroll = offset;
                iced::Task::none()
            }
            HistoryAction::DiffFileChanged => iced::Task::none(),
            HistoryAction::FileHistory { revision, path } => self.show_file_history(revision, path),
            HistoryAction::CurrentFileHistory => match current_file(self) {
                Some((revision, path)) => self.show_file_history(revision, path),
                None => iced::Task::none(),
            },
            HistoryAction::ClearFileFilter => {
                let Some(filter) = self.active_mut().history.file_filter.take() else {
                    return iced::Task::none();
                };
                if self.active().session.revset == filter.revset {
                    return self.perform(Action::SetRevset(filter.previous));
                }
                iced::Task::none()
            }
            HistoryAction::ToggleAnnotate => {
                let history = &mut self.active_mut().history;
                history.annotate = !history.annotate;
                if !history.annotate {
                    history.annotation = None;
                    history.annotation_error = None;
                }
                // Widening the gutter reflows every line.
                self.document_version = self.document_version.wrapping_add(1);
                self.sync_history()
            }
            HistoryAction::AnnotationClicked(line) => {
                let target = self.active().history.annotation.as_ref().and_then(|view| {
                    let index = *view.annotation.lines.get(line)? as usize;
                    let origin = view.annotation.origins.get(index)?;
                    origin
                        .visible
                        .then(|| (origin.commit_id.clone(), view.annotation.path.clone()))
                });
                match target {
                    Some((commit_id, path)) => {
                        // Already showing that revision: no new diff will
                        // land, so jump in the one on screen.
                        let session = &self.active().session;
                        let seen = (session.selected_revision
                            != session.canonical_selection(&commit_id))
                        .then_some(session.document_id);
                        self.active_mut().history.blame_jump = Some((path, seen));
                        self.perform_history(HistoryAction::OpenRevision(commit_id))
                    }
                    None => iced::Task::none(),
                }
            }
        }
    }

    /// Switch the active tab to the History view, reading the op log when
    /// it has never been read or the repository moved since.
    pub(crate) fn enter_history_view(&mut self) -> Task {
        let Some(tab) = self.active_tab_id() else {
            return iced::Task::none();
        };
        if !is_jj(self) {
            return iced::Task::none();
        }
        let target = self.active_mut();
        let entering = target.main_view != MainView::History;
        target.main_view = MainView::History;
        if entering {
            // The shared code widget swaps documents; same discipline as the
            // Diff ↔ Source switch.
            self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
            self.document_version = self.document_version.wrapping_add(1);
        }
        self.sync_history_for(tab)
    }

    fn reload_op_log(&mut self, tab: TabId) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        let fingerprint = state
            .session
            .repository_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.clone());
        let history = &mut state.history;
        let cancel = history.ops_job.take();
        let job = JobId::next();
        history.ops_job = Some(job);
        history.ops_read_at = Some(fingerprint);
        history.ops_stale = true;
        if history.ops_limit == 0 {
            history.ops_limit = OP_LOG_PAGE;
        }
        let limit = history.ops_limit;
        let mut tasks = Vec::new();
        if let Some(old) = cancel {
            tasks.push(self.send(tab, diffui_core::Command::Cancel { job: old }));
        }
        tasks.push(self.history_send(tab, job, HistoryRequest::OpLog { limit }));
        iced::Task::batch(tasks)
    }

    fn history_send(&mut self, tab: TabId, job: JobId, request: HistoryRequest) -> Task {
        self.send(tab, diffui_core::Command::History { job, request })
    }

    fn history_select_operation(&mut self, tab: TabId, id: String) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        let history = &mut state.history;
        if history.selected_op.as_deref() == Some(id.as_str()) && history.op_changes.is_some() {
            return iced::Task::none();
        }
        history.selected_op = Some(id.clone());
        history.op_changes = None;
        history.selected_change = None;
        history.diff = None;
        history.error = None;
        let job = JobId::next();
        history.op_job = Some(job);
        self.history_send(tab, job, HistoryRequest::ShowOperation { operation_id: id })
    }

    fn history_select_change(&mut self, tab: TabId, index: usize) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        let history = &mut state.history;
        let Some(target) = history
            .op_changes
            .as_ref()
            .and_then(|changes| changes.commits.get(index))
            .map(ChangedCommit::diff_target)
        else {
            return iced::Task::none();
        };
        history.selected_change = Some(index);
        self.history_load_diff(tab, target)
    }

    fn history_select_version(&mut self, tab: TabId, index: usize) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        let history = &mut state.history;
        let Some(entry) = history.evolog.get(index) else {
            return iced::Task::none();
        };
        let target = match (history.evolog_diff, entry.interdiff_target()) {
            (EvologDiff::Previous, Some(target)) => target,
            _ => HistoryDiffTarget::Commit {
                commit_id: entry.commit.commit_id.clone(),
            },
        };
        history.selected_version = Some(index);
        self.history_load_diff(tab, target)
    }

    fn history_load_diff(&mut self, tab: TabId, target: HistoryDiffTarget) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        let history = &mut state.history;
        if history
            .diff
            .as_ref()
            .is_some_and(|diff| diff.target == target)
            || history
                .diff_job
                .as_ref()
                .is_some_and(|(_, pending)| *pending == target)
        {
            return iced::Task::none();
        }
        let cancel = history.diff_job.take().map(|(job, _)| job);
        let job = JobId::next();
        history.diff_job = Some((job, target.clone()));
        let mut tasks = Vec::new();
        if let Some(old) = cancel {
            tasks.push(self.send(tab, diffui_core::Command::Cancel { job: old }));
        }
        tasks.push(self.history_send(tab, job, HistoryRequest::Diff { target }));
        iced::Task::batch(tasks)
    }

    /// Raise the confirmation for a restore or a revert, holding the
    /// mutation (and its queued activity row) until the user answers.
    fn confirm_operation(&mut self, tab: TabId, id: String, restore: bool) -> Task {
        if !self.active().session.capabilities.mutate {
            return iced::Task::none();
        }
        let short = &id[..id.len().min(12)];
        let summary = self
            .active()
            .history
            .ops
            .iter()
            .find(|op| op.id == id)
            .map(|op| op.summary().to_owned())
            .unwrap_or_default();
        let (title, body, confirm_label, label, op) = if restore {
            (
                Prose::from("Restore to operation ").op(short).text("?"),
                Prose::from(format!(
                    "Every revision and bookmark goes back to how \u{201c}{summary}\u{201d} \
                     left them. This can be undone too."
                )),
                "Restore",
                Prose::from("Restore to ").op(short),
                MutationOp::RestoreOperation {
                    operation_id: id.clone(),
                },
            )
        } else {
            (
                Prose::from("Revert operation ").op(short).text("?"),
                Prose::from(format!(
                    "Undoes just \u{201c}{summary}\u{201d}, keeping the work done since."
                )),
                "Revert",
                Prose::from("Revert ").op(short),
                MutationOp::RevertOperation {
                    operation_id: id.clone(),
                },
            )
        };
        let (activity_id, _) = self.begin_activity(tab, label, false);
        if let Some(log) = self.activity_log_for(tab) {
            log.set_status(activity_id, activity::ActivityStatus::Queued);
        }
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title,
            body,
            confirm_label: confirm_label.to_owned(),
            pending: PendingMutation {
                op,
                tab_id: tab,
                activity_id,
                allow_immutable: false,
            },
        }));
        iced::Task::none()
    }

    fn show_file_history(&mut self, revision: RevisionSelection, path: String) -> Task {
        if !is_jj(self) {
            return iced::Task::none();
        }
        let symbol = match &revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(hex) => hex.clone(),
        };
        let revset = file_history_revset(&symbol, &path);
        let target = self.active_mut();
        // Stacking filters would make ✕ restore the previous file's history;
        // it should land back on what the user had before any of them.
        let previous = match target.history.file_filter.take() {
            Some(filter) if target.session.revset == filter.revset => filter.previous,
            _ => target.session.revset.clone(),
        };
        target.history.file_filter = Some(FileFilter {
            path,
            revset: revset.clone(),
            previous,
        });
        target.history.filter_jumped = None;
        let view = self.perform(Action::SetMainView(MainView::Diff));
        let load = self.perform(Action::SetRevset(revset));
        let target = self.active_mut();
        target.history.filter_loading = target.session.streaming();
        iced::Task::batch([view, load])
    }

    /// Route a history job's event. `None` when the event isn't one.
    pub(crate) fn history_event(&mut self, tab: TabId, event: &diffui_core::Event) -> Option<Task> {
        use diffui_core::Payload;
        let job = event.job()?;
        let state = self.tab_mut(tab)?;
        if !state.history.owns(job) {
            return None;
        }
        let history = &mut state.history;
        let mut follow_up: Option<HistoryAction> = None;
        match &event.payload {
            Payload::OpLogBatch { entries, .. } if history.ops_job == Some(job) => {
                if std::mem::take(&mut history.ops_stale) {
                    history.ops.clear();
                }
                let first = history.ops.is_empty();
                history.ops.extend(entries.iter().cloned());
                // Land on the current operation the first time the log
                // shows, so the pane never opens empty.
                let keep = history
                    .selected_op
                    .as_ref()
                    .is_some_and(|id| history.ops.iter().any(|op| &op.id == id));
                if first
                    && !keep
                    && let Some(op) = history.ops.first()
                {
                    follow_up = Some(HistoryAction::SelectOperation(op.id.clone()));
                }
            }
            Payload::History { response, .. } => match response {
                HistoryResponse::OpLogDone { truncated } => {
                    history.ops_job = None;
                    if std::mem::take(&mut history.ops_stale) {
                        history.ops.clear();
                    }
                    history.ops_truncated = *truncated;
                }
                HistoryResponse::Operation(changes) => {
                    history.op_job = None;
                    // The op's own rewrite, not the descendants it dragged
                    // along, is the diff worth opening on.
                    let first = changes
                        .commits
                        .iter()
                        .position(|change| !change.rebased)
                        .or((!changes.commits.is_empty()).then_some(0));
                    history.op_changes = Some(changes.clone());
                    if history.kind == HistoryKind::OpLog {
                        follow_up = first.map(HistoryAction::SelectChange);
                    }
                }
                HistoryResponse::Evolog(entries) => {
                    history.evolog_job = None;
                    history.evolog = entries.clone();
                    if !entries.is_empty() {
                        follow_up = Some(HistoryAction::SelectVersion(0));
                    }
                }
                HistoryResponse::Diff {
                    target,
                    document,
                    details,
                } => {
                    history.diff_job = None;
                    let target = target.clone();
                    let (document, details) = (document.clone(), details.clone());
                    let doc_id = self.allocate_document_id();
                    self.document_version = self.document_version.wrapping_add(1);
                    self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
                    let history = &mut self.tab_mut(tab)?.history;
                    history.diff_scroll = 0.0;
                    history.diff = Some(HistoryDiff {
                        target,
                        document,
                        details,
                        doc_id,
                    });
                }
                HistoryResponse::Annotation(annotation) => {
                    history.annotation_job = None;
                    let labels = annotation_labels(annotation, &self.resolved_theme().spec());
                    self.tab_mut(tab)?.history.annotation = Some(AnnotationView {
                        annotation: annotation.clone(),
                        labels,
                    });
                    self.document_version = self.document_version.wrapping_add(1);
                }
            },
            Payload::Failed { error, .. } => {
                if let Some((_, revision, path)) =
                    history.annotation_job.take_if(|(id, ..)| *id == job)
                {
                    history.annotation_error = Some((revision, path, error.to_string()));
                } else {
                    clear_job(history, job);
                    history.error = Some(error.to_string());
                }
            }
            Payload::Cancelled { .. } => clear_job(history, job),
            _ => {}
        }
        Some(match follow_up {
            Some(action) if self.is_active(tab) => self.perform_history(action),
            _ => iced::Task::none(),
        })
    }

    /// Bring the active tab's history views in line with what is on screen:
    /// re-read a stale op log, ask for a missing annotation, and keep the
    /// file filter pointed at its file. Called once per update.
    pub(crate) fn sync_history(&mut self) -> Task {
        match self.active_tab_id() {
            Some(tab) => self.sync_history_for(tab),
            None => iced::Task::none(),
        }
    }

    fn sync_history_for(&mut self, tab: TabId) -> Task {
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::none();
        };
        if state.handle.is_none() {
            return iced::Task::none();
        }
        let fingerprint = state
            .session
            .repository_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.clone());
        let mut tasks = Vec::new();

        // The op log is current while the view shows it.
        if state.main_view == MainView::History
            && state.history.kind == HistoryKind::OpLog
            && state.history.ops_job.is_none()
            && state.history.ops_read_at.as_ref() != Some(&fingerprint)
        {
            tasks.push(self.reload_op_log(tab));
        }

        // The blame column follows the file the source browser shows.
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::batch(tasks);
        };
        if state.main_view == MainView::Source
            && state.history.annotate
            && let Some(path) = state.source.selected.clone()
            && state.source.file.as_ref().is_some_and(|file| !file.binary)
        {
            let revision = crate::source_panel::browsed_revision(&state.source);
            if !state.history.annotation_settled(&revision, &path) {
                let job = JobId::next();
                state.history.annotation = None;
                state.history.annotation_job = Some((job, revision.clone(), path.clone()));
                tasks.push(self.history_send(
                    tab,
                    job,
                    HistoryRequest::Annotate { revision, path },
                ));
            }
        }

        // A file filter the user replaced by typing another revset is gone;
        // a live one opens its file in each diff it lands on.
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::batch(tasks);
        };
        if let Some(filter) = &state.history.file_filter
            && state.session.revset != filter.revset
        {
            state.history.file_filter = None;
            state.history.filter_loading = false;
        }
        if state.history.filter_loading && !state.session.streaming() {
            state.history.filter_loading = false;
            if state.history.file_filter.is_some() && !state.session.commits.is_empty() {
                let commit_id = state.session.commits.row(0).commit_id().to_owned();
                let selection = state.session.canonical_selection(&commit_id);
                tasks.push(self.perform(Action::SelectRevision(selection)));
            }
        }
        let Some(state) = self.tab_mut(tab) else {
            return iced::Task::batch(tasks);
        };
        let wanted = state
            .history
            .file_filter
            .as_ref()
            .map(|filter| (filter.path.clone(), state.history.filter_jumped))
            .or_else(|| state.history.blame_jump.clone());
        if let Some((path, seen)) = wanted
            && state.main_view == MainView::Diff
            && seen != Some(state.session.document_id)
            && state
                .session
                .document
                .files
                .iter()
                .any(|file| file.path == path)
        {
            let document = Some(state.session.document_id);
            if state.history.file_filter.is_some() {
                state.history.filter_jumped = document;
            } else {
                state.history.blame_jump = None;
            }
            // A beat later: the code view only scrolls to a selected file
            // once it has laid the new document out, and a selection made in
            // the same pass that swapped the document in is lost to that.
            tasks.push(iced::Task::perform(
                tokio::time::sleep(std::time::Duration::from_millis(80)),
                move |()| Message::Action(Action::JumpToFile(path.clone())),
            ));
        }
        iced::Task::batch(tasks)
    }
}

type Task = iced::Task<Message>;

/// "5m ago", or "just now".
fn age_phrase(then: i64, now: i64) -> String {
    match relative_age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

fn clear_job(history: &mut HistoryState, job: JobId) {
    if history.ops_job == Some(job) {
        history.ops_job = None;
    }
    if history.op_job == Some(job) {
        history.op_job = None;
    }
    if history.evolog_job == Some(job) {
        history.evolog_job = None;
    }
    history.diff_job.take_if(|(id, _)| *id == job);
    history.annotation_job.take_if(|(id, ..)| *id == job);
}

fn is_jj(ui: &Diffui) -> bool {
    ui.active()
        .repository
        .as_ref()
        .is_some_and(|repository| repository.vcs == Vcs::Jj)
}

/// Whether the active tab can show the history views at all.
pub(crate) fn available(ui: &Diffui) -> bool {
    is_jj(ui) && ui.active().session.capabilities.mutate
}

/// The file the user is looking at: the source browser's, or the diff's
/// selected file at the selected revision.
pub(crate) fn current_file(ui: &Diffui) -> Option<(RevisionSelection, String)> {
    let tab = ui.active();
    match tab.main_view {
        MainView::Source => Some((
            crate::source_panel::browsed_revision(&tab.source),
            tab.source.selected.clone()?,
        )),
        MainView::Diff => Some((
            tab.session.selected_revision.clone(),
            tab.session
                .document
                .files
                .get(tab.selected_file)?
                .path
                .clone(),
        )),
        MainView::History => None,
    }
}

/// Whether the source browser has a file the blame column could annotate.
pub(crate) fn can_annotate(ui: &Diffui) -> bool {
    let tab = ui.active();
    is_jj(ui)
        && tab.main_view == MainView::Source
        && tab.source.file.as_ref().is_some_and(|file| !file.binary)
}

/// The selected operation's id, for the palette's restore/revert rows.
pub(crate) fn selected_operation(ui: &Diffui) -> Option<String> {
    let tab = ui.active();
    (tab.main_view == MainView::History && tab.history.kind == HistoryKind::OpLog)
        .then(|| tab.history.selected_op.clone())
        .flatten()
}

pub(crate) fn file_filter_active(ui: &Diffui) -> bool {
    ui.active()
        .history
        .file_filter
        .as_ref()
        .is_some_and(|filter| filter.revset == ui.active().session.revset)
}

fn annotation_labels(annotation: &FileAnnotation, theme: &ThemeSpec) -> Vec<AnnotationLabel> {
    let now = now_millis();
    let mut previous: Option<u32> = None;
    annotation
        .lines
        .iter()
        .map(|&origin| {
            let first = previous != Some(origin);
            previous = Some(origin);
            let Some(commit) = annotation.origins.get(origin as usize).filter(|_| first) else {
                return AnnotationLabel {
                    text: String::new(),
                    color: theme.subtle_text,
                };
            };
            let author: String = commit
                .author
                .chars()
                .take(ANNOTATION_AUTHOR_CHARS)
                .collect();
            AnnotationLabel {
                text: format!(
                    "{} {:<width$} {:>4}",
                    commit.short_change_id(),
                    author,
                    relative_age(commit.time_millis, now),
                    width = ANNOTATION_AUTHOR_CHARS
                ),
                color: if commit.visible {
                    theme.muted_text
                } else {
                    theme.subtle_text
                },
            }
        })
        .collect()
}

/// The blame column for the source browser's code view, when annotate is on
/// and the annotation for the shown file has landed.
pub(crate) fn source_annotations<'a>(ui: &'a Diffui) -> Option<AnnotationGutter<'a, Message>> {
    let tab = ui.active();
    if !tab.history.annotate {
        return None;
    }
    let view = tab.history.annotation.as_ref()?;
    let revision = crate::source_panel::browsed_revision(&tab.source);
    if view.annotation.revision != revision
        || tab.source.selected.as_deref() != Some(view.annotation.path.as_str())
    {
        return None;
    }
    Some(AnnotationGutter {
        labels: &view.labels,
        width_chars: ANNOTATION_CHARS,
        on_click: |line| Message::Action(Action::History(HistoryAction::AnnotationClicked(line))),
    })
}

/// The source info bar's blame status, when there is something to say.
pub(crate) fn annotation_status(ui: &Diffui) -> Option<(String, bool)> {
    let tab = ui.active();
    if !tab.history.annotate {
        return None;
    }
    let revision = crate::source_panel::browsed_revision(&tab.source);
    let path = tab.source.selected.as_deref()?;
    if let Some((r, p, error)) = &tab.history.annotation_error
        && *r == revision
        && p == path
    {
        return Some((format!("Annotate failed: {error}"), true));
    }
    if tab.history.annotation_job.is_some() {
        return Some(("Annotating\u{2026}".to_owned(), false));
    }
    let view = tab.history.annotation.as_ref()?;
    let lines = tab.source.file.as_ref()?.line_count;
    (view.annotation.lines.len() != lines && matches!(revision, RevisionSelection::WorkingCopy))
        .then(|| ("Blame is for the last snapshot".to_owned(), false))
}

pub(crate) fn annotate_on(ui: &Diffui) -> bool {
    ui.active().history.annotate
}

// ── View ────────────────────────────────────────────────────────────────

/// The source info bar's history controls: the blame toggle and the browsed
/// file's history.
pub(crate) fn source_bar_actions<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let on = annotate_on(ui);
    let blame = ui::text_button(
        Some((
            icons::SCROLL_TEXT,
            if on { theme.accent } else { theme.muted_text },
        )),
        "Blame",
        if on {
            ui::Variant::Raised
        } else {
            ui::Variant::Ghost
        },
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
    )
    .on_press(Message::Action(Action::History(
        HistoryAction::ToggleAnnotate,
    )));
    let history = ui::text_button(
        Some((icons::FILE_CLOCK, theme.muted_text)),
        "History",
        ui::Variant::Ghost,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
    )
    .on_press(Message::Action(Action::History(
        HistoryAction::CurrentFileHistory,
    )));
    row![
        ui::tooltip(
            blame,
            "Show which change last touched each line",
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ),
        ui::tooltip(
            history,
            "Show the revisions that touched this file",
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ),
    ]
    .spacing(space::XS)
    .align_y(alignment::Vertical::Center)
    .into()
}

/// The banner above the revision list while the graph shows one file's
/// history.
pub(crate) fn file_filter_banner<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<Element<'a, Message>> {
    if !file_filter_active(ui) {
        return None;
    }
    let filter = ui.active().history.file_filter.as_ref()?;
    let content = row![
        icons::icon(icons::FILE_CLOCK, ui::ICON_SIZE, theme.accent),
        Prose::from("History of ")
            .path(&filter.path)
            .view(ui.config.ui_font, ui.config.mono_font, theme.text, theme)
            .size(text_size::UI)
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
        ui::tooltip(
            ui::close_button(theme.muted_text, theme).on_press(Message::Action(Action::History(
                HistoryAction::ClearFileFilter
            ))),
            "Show the whole graph again",
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ),
    ]
    .spacing(space::SM)
    .align_y(alignment::Vertical::Center);
    Some(
        container(
            container(content)
                .width(Length::Fill)
                .padding(Padding {
                    left: space::MD,
                    ..Padding::from(ui::BANNER_INSET)
                })
                .style(move |_| container::Style {
                    background: Some(Background::Color(chip_background(theme.accent))),
                    border: Border {
                        width: 0.0,
                        color: Color::TRANSPARENT,
                        radius: radius::CONTROL.into(),
                    },
                    ..container::Style::default()
                }),
        )
        .width(Length::Fill)
        .padding(Padding::from([space::XS, space::MD]))
        .into(),
    )
}

fn row_style(theme: ThemeSpec, selected: bool, status: button::Status) -> button::Style {
    let background = if selected {
        Some(Background::Color(theme.selected_file))
    } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        Some(Background::Color(chip_background(theme.muted_text)))
    } else {
        None
    };
    button::Style {
        background,
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::PUSH.into(),
        },
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

fn caption<'a>(value: impl text::IntoFragment<'a>, color: Color, ui: &Diffui) -> text::Text<'a> {
    text(value)
        .size(text_size::CAPTION)
        .font(ui.config.ui_font)
        .color(color)
        .wrapping(text::Wrapping::None)
}

fn mono<'a>(value: impl text::IntoFragment<'a>, color: Color, ui: &Diffui) -> text::Text<'a> {
    text(value)
        .size(text_size::UI)
        .font(ui.config.mono_font)
        .color(color)
        .wrapping(text::Wrapping::None)
}

/// One displayed op-log row: an operation, or a folded run of snapshots.
enum OpRow<'a> {
    Op(&'a OperationEntry),
    Run {
        first: &'a OperationEntry,
        count: usize,
        oldest: &'a OperationEntry,
    },
}

fn op_rows<'a>(history: &'a HistoryState) -> Vec<OpRow<'a>> {
    let mut rows = Vec::new();
    let ops = &history.ops;
    let mut index = 0;
    while index < ops.len() {
        let op = &ops[index];
        let run_end = ops[index..]
            .iter()
            .position(|candidate| {
                !candidate.is_snapshot || candidate.is_current && candidate.id != op.id
            })
            .map_or(ops.len(), |offset| index + offset);
        let run = run_end - index;
        // A lone snapshot reads fine on its own; two or more fold.
        if op.is_snapshot && !op.is_current && run >= 2 {
            rows.push(OpRow::Run {
                first: op,
                count: run,
                oldest: &ops[run_end - 1],
            });
            if history.expanded_runs.contains(&op.id) {
                rows.extend(ops[index..run_end].iter().map(OpRow::Op));
            }
            index = run_end;
        } else {
            rows.push(OpRow::Op(op));
            index += 1;
        }
    }
    rows
}

fn op_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    op: &'a OperationEntry,
    now: i64,
) -> Element<'a, Message> {
    let history = &ui.active().history;
    let selected = history.selected_op.as_deref() == Some(op.id.as_str());
    let age = relative_age(op.time_millis, now);
    let content: Element<'a, Message> = if op.is_snapshot {
        row![
            Space::new().width(Length::Fixed(space::LG)),
            mono(op.short_id(), theme.subtle_text, ui),
            caption(op.summary(), theme.subtle_text, ui)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
            caption(age, theme.subtle_text, ui),
        ]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center)
        .into()
    } else {
        let id_color = if op.is_current {
            theme.accent
        } else {
            theme.muted_text
        };
        let mut head = row![mono(op.short_id(), id_color, ui)]
            .spacing(space::SM)
            .align_y(alignment::Vertical::Center);
        if op.is_current {
            head = head.push(chip("current", theme.accent, ui));
        }
        head = head.push(
            caption(op.user.as_str(), theme.subtle_text, ui)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
        );
        head = head.push(caption(age, theme.subtle_text, ui));
        column![
            head,
            text(op.summary())
                .size(text_size::BODY)
                .font(ui.config.ui_font)
                .color(theme.text)
                .wrapping(text::Wrapping::None)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
        ]
        .spacing(2)
        .into()
    };
    button(content)
        .width(Length::Fill)
        .padding(Padding::from([space::XS, space::MD]))
        .on_press(Message::Action(Action::History(
            HistoryAction::SelectOperation(op.id.clone()),
        )))
        .style(move |_, status| row_style(theme, selected, status))
        .into()
}

fn run_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    first: &'a OperationEntry,
    count: usize,
    oldest: &'a OperationEntry,
    now: i64,
) -> Element<'a, Message> {
    let expanded = ui.active().history.expanded_runs.contains(&first.id);
    let chevron = if expanded {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    let span = format!(
        "{}\u{2013}{}",
        relative_age(oldest.time_millis, now),
        relative_age(first.time_millis, now)
    );
    button(
        row![
            icons::icon(chevron, 12.0, theme.subtle_text),
            caption(
                format!("{count} working-copy snapshots"),
                theme.subtle_text,
                ui
            )
            .width(Length::Fill),
            caption(span, theme.subtle_text, ui),
        ]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center),
    )
    .width(Length::Fill)
    .padding(Padding::from([space::XXS, space::MD]))
    .on_press(Message::Action(Action::History(
        HistoryAction::ToggleSnapshotRun(first.id.clone()),
    )))
    .style(move |_, status| row_style(theme, false, status))
    .into()
}

fn chip<'a>(label: &'a str, color: Color, ui: &Diffui) -> Element<'a, Message> {
    container(
        text(label)
            .size(text_size::BADGE)
            .font(ui.config.ui_font)
            .color(color),
    )
    .padding(Padding::from([0, 5]))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(color))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: crate::chip::RADIUS.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn version_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    index: usize,
    entry: &'a EvologEntry,
    now: i64,
) -> Element<'a, Message> {
    let selected = ui.active().history.selected_version == Some(index);
    let mut head = row![mono(entry.commit.short_commit_id(), theme.muted_text, ui)]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center);
    if entry.commit.visible {
        head = head.push(chip("current", theme.accent, ui));
    }
    if entry.commit.empty {
        head = head.push(chip("empty", theme.subtle_text, ui));
    }
    head = head.push(Space::new().width(Length::Fill));
    head = head.push(caption(
        relative_age(entry.commit.time_millis, now),
        theme.subtle_text,
        ui,
    ));
    let summary = if entry.commit.summary.is_empty() {
        "(no description set)"
    } else {
        entry.commit.summary.as_str()
    };
    let operation = entry
        .operation
        .as_ref()
        .map(|op| op.summary.as_str())
        .unwrap_or("(operation no longer in the log)");
    button(
        column![
            head,
            text(summary)
                .size(text_size::BODY)
                .font(ui.config.ui_font)
                .color(if entry.commit.summary.is_empty() {
                    theme.subtle_text
                } else {
                    theme.text
                })
                .wrapping(text::Wrapping::None)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
            caption(operation, theme.subtle_text, ui)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
        ]
        .spacing(2),
    )
    .width(Length::Fill)
    .padding(Padding::from([space::XS, space::MD]))
    .on_press(Message::Action(Action::History(
        HistoryAction::SelectVersion(index),
    )))
    .style(move |_, status| row_style(theme, selected, status))
    .into()
}

fn header_bar<'a>(
    content: impl Into<Element<'a, Message>>,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .height(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
        .padding(Padding::from([0.0, space::MD]))
        .center_y(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background)),
            ..container::Style::default()
        })
        .into()
}

fn note<'a>(
    message: impl text::IntoFragment<'a>,
    color: Color,
    ui: &Diffui,
) -> Element<'a, Message> {
    container(
        text(message)
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(color),
    )
    .width(Length::Fill)
    .padding(Padding::from([space::SM, space::LG]))
    .into()
}

/// The History view's left pane: the op log, or the evolog it was switched to.
pub(crate) fn build_history_sidebar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let history = &ui.active().history;
    let now = now_millis();
    let reload = ui::tooltip(
        ui::icon_button(icons::REFRESH, theme.muted_text, ui::Size::Compact, theme)
            .on_press(Message::Action(Action::History(HistoryAction::Reload))),
        "Reload",
        iced::widget::tooltip::Position::Bottom,
        ui.config.ui_font,
        theme,
    );
    let title_font = emphasis_font(ui.config.ui_font, iced::font::Weight::Medium);

    let (header, list): (Element<'a, Message>, Element<'a, Message>) = match &history.kind {
        HistoryKind::OpLog => {
            let header = row![
                icons::icon(icons::HISTORY, ui::ICON_SIZE, theme.subtle_text),
                text("Operations")
                    .size(text_size::UI)
                    .font(title_font)
                    .color(theme.text)
                    .width(Length::Fill),
                reload,
            ]
            .spacing(space::SM)
            .align_y(alignment::Vertical::Center);
            let mut rows = column![]
                .spacing(1)
                .padding(Padding::from([space::XS, space::XS]));
            for entry in op_rows(history) {
                rows = rows.push(match entry {
                    OpRow::Op(op) => op_row(ui, theme, op, now),
                    OpRow::Run {
                        first,
                        count,
                        oldest,
                    } => run_row(ui, theme, first, count, oldest, now),
                });
            }
            if history.ops_job.is_some() {
                rows = rows.push(note(
                    "Reading the operation log\u{2026}",
                    theme.subtle_text,
                    ui,
                ));
            } else if history.ops_truncated {
                rows = rows.push(
                    container(
                        ui::text_button(
                            None,
                            "Load older operations",
                            ui::Variant::Ghost,
                            ui::Size::Compact,
                            ui.config.ui_font,
                            theme,
                        )
                        .on_press(Message::Action(Action::History(
                            HistoryAction::LoadMoreOperations,
                        ))),
                    )
                    .width(Length::Fill)
                    .center_x(Length::Fill)
                    .padding(space::SM),
                );
            }
            (
                header.into(),
                ui::scrollable(rows, theme).height(Length::Fill).into(),
            )
        }
        HistoryKind::Evolog { label, .. } => {
            let header = row![
                ui::tooltip(
                    ui::icon_button(
                        icons::ARROW_LEFT,
                        theme.muted_text,
                        ui::Size::Compact,
                        theme
                    )
                    .on_press(Message::Action(Action::History(HistoryAction::ShowOpLog))),
                    "Back to the operation log",
                    iced::widget::tooltip::Position::Bottom,
                    ui.config.ui_font,
                    theme,
                ),
                text("Evolution of ")
                    .size(text_size::UI)
                    .font(title_font)
                    .color(theme.text),
                mono(label.as_str(), theme.accent, ui).width(Length::Fill),
                reload,
            ]
            .spacing(space::XS)
            .align_y(alignment::Vertical::Center);
            let mut rows = column![]
                .spacing(1)
                .padding(Padding::from([space::XS, space::XS]));
            for (index, entry) in history.evolog.iter().enumerate() {
                rows = rows.push(version_row(ui, theme, index, entry, now));
            }
            if history.evolog_job.is_some() {
                rows = rows.push(note(
                    "Reading the change's history\u{2026}",
                    theme.subtle_text,
                    ui,
                ));
            }
            (
                header.into(),
                ui::scrollable(rows, theme).height(Length::Fill).into(),
            )
        }
    };

    container(column![header_bar(header, theme), list])
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| sidebar_panel_style(theme))
        .into()
}

fn commit_line<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    commit: &'a CommitBrief,
) -> iced::widget::Row<'a, Message> {
    let summary = if commit.summary.is_empty() {
        "(no description set)"
    } else {
        commit.summary.as_str()
    };
    let mut line = row![
        mono(commit.short_change_id(), theme.accent, ui),
        mono(commit.short_commit_id(), theme.subtle_text, ui),
    ]
    .spacing(space::SM)
    .align_y(alignment::Vertical::Center);
    if !commit.visible {
        line = line.push(chip("hidden", theme.subtle_text, ui));
    }
    if commit.empty {
        line = line.push(chip("empty", theme.subtle_text, ui));
    }
    line.push(
        text(summary)
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(if commit.summary.is_empty() {
                theme.subtle_text
            } else {
                theme.text
            })
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
    )
}

fn change_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    index: usize,
    change: &'a ChangedCommit,
) -> Element<'a, Message> {
    let selected = ui.active().history.selected_change == Some(index);
    let (glyph, color, verb) = match change.kind {
        CommitChangeKind::Created => ("+", theme.added_text, "created"),
        CommitChangeKind::Rewritten if change.rebased => ("~", theme.subtle_text, "rebased"),
        CommitChangeKind::Rewritten => ("~", theme.modified_token, "rewritten"),
        CommitChangeKind::Abandoned => ("\u{2212}", theme.removed_text, "abandoned"),
    };
    let mut line = row![
        container(mono(glyph, color, ui)).width(Length::Fixed(12.0)),
        commit_line(ui, theme, &change.commit),
        caption(verb, theme.subtle_text, ui),
    ]
    .spacing(space::SM)
    .align_y(alignment::Vertical::Center);
    if change.commit.visible {
        line = line.push(ui::tooltip(
            ui::icon_button(icons::FILE_DIFF, theme.muted_text, ui::Size::Compact, theme).on_press(
                Message::Action(Action::History(HistoryAction::OpenRevision(
                    change.commit.commit_id.clone(),
                ))),
            ),
            "Show in the diff view",
            iced::widget::tooltip::Position::Left,
            ui.config.ui_font,
            theme,
        ));
    }
    button(line)
        .width(Length::Fill)
        .padding(Padding::from([space::XXS, space::SM]))
        .on_press(Message::Action(Action::History(
            HistoryAction::SelectChange(index),
        )))
        .style(move |_, status| row_style(theme, selected, status))
        .into()
}

fn ref_side_label(side: &[CommitBrief]) -> String {
    match side {
        [] => "(absent)".to_owned(),
        [only] => only.short_change_id().to_owned(),
        many => format!(
            "conflict: {}",
            many.iter()
                .map(CommitBrief::short_change_id)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn ref_row<'a>(ui: &'a Diffui, theme: ThemeSpec, change: &'a RefChange) -> Element<'a, Message> {
    let kind = match change.kind {
        RefKind::WorkingCopy => "working copy",
        RefKind::Bookmark => "bookmark",
        RefKind::RemoteBookmark => "remote bookmark",
        RefKind::Tag => "tag",
    };
    container(
        row![
            container(caption(kind, theme.subtle_text, ui)).width(Length::Fixed(96.0)),
            mono(change.name.as_str(), theme.text, ui),
            mono(ref_side_label(&change.from), theme.removed_text, ui),
            caption("\u{2192}", theme.subtle_text, ui),
            mono(ref_side_label(&change.to), theme.added_text, ui),
        ]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center),
    )
    .padding(Padding::from([space::XXS, space::SM]))
    .into()
}

fn section_title<'a>(label: String, ui: &Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    container(
        text(label)
            .size(text_size::CAPTION)
            .font(emphasis_font(ui.config.ui_font, iced::font::Weight::Medium))
            .color(theme.muted_text),
    )
    .padding(Padding {
        top: space::SM,
        right: space::SM,
        bottom: space::XXS,
        left: space::SM,
    })
    .into()
}

/// The operation's header: description, metadata, and the two actions.
fn operation_header<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    op: &'a OperationEntry,
    changes: Option<&'a OperationChanges>,
) -> Element<'a, Message> {
    let now = now_millis();
    let mut meta = row![mono(op.short_id(), theme.muted_text, ui)]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center);
    if op.is_current {
        meta = meta.push(chip("current", theme.accent, ui));
    }
    if op.is_snapshot {
        meta = meta.push(chip("snapshot", theme.subtle_text, ui));
    }
    meta = meta.push(caption(op.user.as_str(), theme.subtle_text, ui));
    meta = meta.push(caption(
        format!("{} ({})", op.time, age_phrase(op.time_millis, now)),
        theme.subtle_text,
        ui,
    ));

    let can_mutate = ui.active().session.capabilities.mutate;
    let has_parent = !op.parent_ids.is_empty();
    let is_merge = changes.is_some_and(|changes| changes.is_merge) || op.parent_ids.len() > 1;
    let restore = ui::text_button(
        Some((icons::ROTATE_CCW, theme.muted_text)),
        "Restore to here",
        ui::Variant::Raised,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
    )
    .on_press_maybe((can_mutate && !op.is_current && has_parent).then(|| {
        Message::Action(Action::History(HistoryAction::RestoreOperation(
            op.id.clone(),
        )))
    }));
    let revert = ui::text_button(
        Some((icons::UNDO, theme.muted_text)),
        "Revert",
        ui::Variant::Raised,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
    )
    .on_press_maybe((can_mutate && has_parent && !is_merge).then(|| {
        Message::Action(Action::History(HistoryAction::RevertOperation(
            op.id.clone(),
        )))
    }));

    let mut block = column![
        row![
            text(op.summary())
                .size(text_size::BODY_LG)
                .font(emphasis_font(ui.config.ui_font, iced::font::Weight::Medium))
                .color(theme.text)
                .wrapping(text::Wrapping::None)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
            ui::tooltip(
                restore,
                "Put every revision and bookmark back to how this operation left them (jj op restore)",
                iced::widget::tooltip::Position::Bottom,
                ui.config.ui_font,
                theme,
            ),
            ui::tooltip(
                revert,
                "Undo just this operation, keeping later work (jj op revert)",
                iced::widget::tooltip::Position::Bottom,
                ui.config.ui_font,
                theme,
            ),
        ]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center),
        meta,
    ]
    .spacing(space::XS);
    if let Some(args) = &op.args {
        block = block.push(
            caption(args.as_str(), theme.subtle_text, ui)
                .font(ui.config.mono_font)
                .ellipsis(text::Ellipsis::End)
                .width(Length::Fill),
        );
    }
    container(block)
        .width(Length::Fill)
        .padding(Padding::from([space::MD, space::LG]))
        .into()
}

fn operation_changes<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    changes: &'a OperationChanges,
) -> Element<'a, Message> {
    let mut list = column![].spacing(1);
    if changes.is_merge {
        list = list.push(note(
            "A merge of concurrent operations \u{2014} like the jj CLI, no changes are listed.",
            theme.subtle_text,
            ui,
        ));
    }
    if !changes.commits.is_empty() {
        let mut title = format!("Changed commits ({})", changes.commits.len());
        if changes.commits_truncated {
            title.push_str(", more not listed");
        }
        list = list.push(section_title(title, ui, theme));
        for (index, change) in changes.commits.iter().enumerate() {
            list = list.push(change_row(ui, theme, index, change));
        }
    }
    if !changes.refs.is_empty() {
        list = list.push(section_title("Moved refs".to_owned(), ui, theme));
        for change in &changes.refs {
            list = list.push(ref_row(ui, theme, change));
        }
    }
    if changes.commits.is_empty() && changes.refs.is_empty() && !changes.is_merge {
        list = list.push(note(
            "This operation changed no commits or refs.",
            theme.subtle_text,
            ui,
        ));
    }
    ui::scrollable(
        list.padding(Padding {
            bottom: space::SM,
            ..Padding::from([0.0, space::SM])
        }),
        theme,
    )
    .width(Length::Fill)
    .height(Length::Shrink.max(260.0))
    .into()
}

fn evolog_header<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Option<Element<'a, Message>> {
    let history = &ui.active().history;
    let entry = history.evolog.get(history.selected_version?)?;
    let now = now_millis();
    let segment = |label: &'static str, mode: EvologDiff, enabled: bool| {
        let active = history.evolog_diff == mode;
        ui::text_button(
            None,
            label,
            if active {
                ui::Variant::Raised
            } else {
                ui::Variant::Ghost
            },
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
        )
        .on_press_maybe(enabled.then_some(Message::Action(Action::History(
            HistoryAction::SetEvologDiff(mode),
        ))))
    };
    let has_previous = !entry.predecessor_ids.is_empty();
    let mut meta = row![caption(entry.commit.author.as_str(), theme.subtle_text, ui)]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center);
    if let Some(op) = &entry.operation {
        meta = meta.push(mono(op.short_id(), theme.subtle_text, ui));
        meta = meta.push(
            caption(
                format!("{} \u{b7} {}", op.summary, age_phrase(op.time_millis, now)),
                theme.subtle_text,
                ui,
            )
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
        );
    }
    let mut actions = row![
        segment(
            "Changes from previous version",
            EvologDiff::Previous,
            has_previous
        ),
        segment("Full diff", EvologDiff::Parents, true),
    ]
    .spacing(space::XS)
    .align_y(alignment::Vertical::Center);
    if entry.commit.visible {
        actions = actions.push(Space::new().width(Length::Fill));
        actions = actions.push(
            ui::text_button(
                Some((icons::FILE_DIFF, theme.muted_text)),
                "Show in diff view",
                ui::Variant::Ghost,
                ui::Size::Compact,
                ui.config.ui_font,
                theme,
            )
            .on_press(Message::Action(Action::History(
                HistoryAction::OpenRevision(entry.commit.commit_id.clone()),
            ))),
        );
    }
    Some(
        container(
            column![
                row![
                    mono(entry.commit.short_commit_id(), theme.muted_text, ui),
                    text(if entry.commit.summary.is_empty() {
                        "(no description set)"
                    } else {
                        entry.commit.summary.as_str()
                    })
                    .size(text_size::BODY_LG)
                    .font(emphasis_font(ui.config.ui_font, iced::font::Weight::Medium))
                    .color(theme.text)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End)
                    .width(Length::Fill),
                ]
                .spacing(space::SM)
                .align_y(alignment::Vertical::Center),
                meta,
                actions,
            ]
            .spacing(space::XS),
        )
        .width(Length::Fill)
        .padding(Padding::from([space::MD, space::LG]))
        .into(),
    )
}

/// First-line descriptions before and after the selected rewrite, when they
/// differ — what an interdiff with no file changes was actually about.
fn description_change(history: &HistoryState) -> Option<(&str, &str)> {
    let (old, new) = match &history.kind {
        HistoryKind::OpLog => {
            let change = history
                .op_changes
                .as_ref()?
                .commits
                .get(history.selected_change?)?;
            (change.predecessors.first()?, &change.commit)
        }
        HistoryKind::Evolog { .. } => {
            let entry = history.evolog.get(history.selected_version?)?;
            if history.evolog_diff != EvologDiff::Previous {
                return None;
            }
            let previous_id = entry.predecessor_ids.first()?;
            let previous = history
                .evolog
                .iter()
                .find(|candidate| &candidate.commit.commit_id == previous_id)?;
            (&previous.commit, &entry.commit)
        }
    };
    fn name(commit: &CommitBrief) -> &str {
        if commit.summary.is_empty() {
            "(no description set)"
        } else {
            commit.summary.as_str()
        }
    }
    (old.summary != new.summary).then(|| (name(old), name(new)))
}

fn history_diff_view<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let history = &ui.active().history;
    let Some(diff) = &history.diff else {
        let no_commits = history.kind == HistoryKind::OpLog
            && history
                .op_changes
                .as_ref()
                .is_some_and(|changes| changes.commits.is_empty());
        let message = if history.diff_job.is_some() || no_commits {
            ""
        } else {
            "Select a change to see its diff"
        };
        return container(
            text(message)
                .size(text_size::BODY_LG)
                .color(theme.subtle_text),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .into();
    };
    if diff.document.files.is_empty() {
        let message = match description_change(history) {
            Some((old, new)) => format!(
                "No file changes \u{2014} the description changed from \u{201c}{old}\u{201d} to \u{201c}{new}\u{201d}"
            ),
            None => "No file changes".to_owned(),
        };
        return container(
            text(message)
                .size(text_size::BODY_LG)
                .color(theme.subtle_text),
        )
        .padding(space::XXL)
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .into();
    }
    let files = diff
        .document
        .files
        .iter()
        .map(|file| {
            let status_color = file_status_color(file.status, theme);
            DiffFileView {
                title: match &file.old_path {
                    Some(old_path) if old_path != &file.path => {
                        format!("{old_path} -> {}", file.path)
                    }
                    _ => file.path.clone(),
                },
                status: file.status,
                status_color,
                status_fill: chip_background(status_color),
                hunks: &file.hunks,
                additions: file.additions,
                deletions: file.deletions,
            }
        })
        .collect::<Vec<_>>();
    let mut view = DiffView::new(
        files,
        0,
        format!("history:{}", diff.doc_id),
        diff_palette(theme),
        ui.config.mono_font,
        ui.config.code_type,
        ui.config.multi_click_ms,
        |_| Message::Action(Action::History(HistoryAction::DiffFileChanged)),
    )
    .on_copy(|text| Message::Action(Action::Copy(text)))
    .on_scroll(|offset| Message::Action(Action::History(HistoryAction::DiffScrolled(offset))))
    .restore_scroll(history.diff_scroll, ui.scroll_restore_token)
    .content_version(ui.document_version)
    .layout_version(diff.doc_id)
    .wrap(ui.diff_wrap)
    .side_by_side(ui.diff_split);
    if let Some(details) = &diff.details {
        view = view.with_header(crate::diff_panel::revision_header_lines(details));
    }
    view.into()
}

/// The History view's right pane.
pub(crate) fn build_history_panel<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let history = &ui.active().history;
    let divider = || {
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(Background::Color(theme.border)),
                ..container::Style::default()
            })
    };
    let mut body = column![].spacing(0);
    if let Some(error) = &history.error {
        body = body.push(note(error.as_str(), theme.removed_text, ui));
    }
    match &history.kind {
        HistoryKind::OpLog => match history.selected_operation() {
            Some(op) => {
                let changes = history
                    .op_changes
                    .as_ref()
                    .filter(|changes| changes.operation.id == op.id);
                body = body.push(operation_header(ui, theme, op, changes));
                match changes {
                    Some(changes) => body = body.push(operation_changes(ui, theme, changes)),
                    None if history.op_job.is_some() => {
                        body =
                            body.push(note("Reading the operation\u{2026}", theme.subtle_text, ui));
                    }
                    None => {}
                }
                body = body.push(divider());
                body = body.push(history_diff_view(ui, theme));
            }
            None => {
                body = body.push(
                    container(
                        text(if history.ops_job.is_some() {
                            ""
                        } else {
                            "Select an operation"
                        })
                        .size(text_size::BODY_LG)
                        .color(theme.subtle_text),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center(Length::Fill),
                );
            }
        },
        HistoryKind::Evolog { .. } => {
            if let Some(header) = evolog_header(ui, theme) {
                body = body.push(header);
                body = body.push(divider());
            }
            body = body.push(history_diff_view(ui, theme));
        }
    }
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: &str, snapshot: bool, current: bool) -> OperationEntry {
        OperationEntry {
            id: id.to_owned(),
            parent_ids: vec!["p".to_owned()],
            description: if snapshot {
                "snapshot working copy".to_owned()
            } else {
                "describe".to_owned()
            },
            user: "me@host".to_owned(),
            time_millis: 0,
            time: String::new(),
            is_snapshot: snapshot,
            is_current: current,
            args: None,
        }
    }

    fn shape(history: &HistoryState) -> Vec<String> {
        op_rows(history)
            .into_iter()
            .map(|row| match row {
                OpRow::Op(op) => op.id.clone(),
                OpRow::Run { first, count, .. } => format!("{}x{count}", first.id),
            })
            .collect()
    }

    fn jj_app() -> (Diffui, diffui_core::RepoId) {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/diffui-history-test");
        let state = &mut ui.tabs[0].state;
        if let Some(repository) = state.repository.as_mut() {
            repository.vcs = Vcs::Jj;
        }
        state.session.capabilities.mutate = true;
        let repo = ui.tabs[0].repo_id().expect("a repo tab has an id");
        (ui, repo)
    }

    fn deliver(ui: &mut Diffui, repo: &diffui_core::RepoId, payload: diffui_core::Payload) {
        let _ = ui.update(Message::Repo(Box::new(diffui_core::Event::new(
            repo.clone(),
            payload,
        ))));
    }

    #[test]
    fn the_op_log_streams_in_and_lands_on_the_current_operation() {
        let (mut ui, repo) = jj_app();
        let _ = ui.perform_history(HistoryAction::ShowOpLog);
        assert_eq!(ui.active().main_view, MainView::History);
        // No actor in a unit test, so the view's own read never starts.
        let _ = ui.perform_history(HistoryAction::Reload);
        let job = ui
            .active()
            .history
            .ops_job
            .expect("the op log is being read");

        deliver(
            &mut ui,
            &repo,
            diffui_core::Payload::OpLogBatch {
                job,
                entries: vec![op("head", false, true), op("older", true, false)],
            },
        );
        deliver(
            &mut ui,
            &repo,
            diffui_core::Payload::History {
                job,
                response: HistoryResponse::OpLogDone { truncated: true },
            },
        );
        let history = &ui.active().history;
        assert_eq!(history.ops.len(), 2);
        assert!(history.ops_truncated);
        assert!(history.ops_job.is_none());
        assert_eq!(history.selected_op.as_deref(), Some("head"));
        assert!(
            history.op_job.is_some(),
            "selecting asks for the op's changes"
        );
    }

    #[test]
    fn a_failed_history_read_stays_in_its_pane() {
        let (mut ui, repo) = jj_app();
        let _ = ui.perform_history(HistoryAction::ShowOpLog);
        let _ = ui.perform_history(HistoryAction::Reload);
        let job = ui
            .active()
            .history
            .ops_job
            .expect("the op log is being read");
        deliver(
            &mut ui,
            &repo,
            diffui_core::Payload::Failed {
                job,
                error: diffui_core::RepoError::Other("boom".to_owned()),
            },
        );
        assert_eq!(ui.active().history.error.as_deref(), Some("boom"));
        assert!(ui.active().history.ops_job.is_none());
        assert!(
            ui.toasts.is_empty(),
            "the projection's generic failure toast is for its own jobs"
        );
    }

    #[test]
    fn a_file_filter_restores_the_previous_revset() {
        let (mut ui, _repo) = jj_app();
        ui.active_mut().session.revset = "mine()".to_owned();
        let _ = ui.perform_history(HistoryAction::FileHistory {
            revision: RevisionSelection::WorkingCopy,
            path: "src/main.rs".to_owned(),
        });
        assert_eq!(
            ui.active().session.revset,
            r#"::@ & files(root:"src/main.rs")"#
        );
        assert!(file_filter_active(&ui));
        // A second file replaces the first instead of stacking on it.
        let _ = ui.perform_history(HistoryAction::FileHistory {
            revision: RevisionSelection::WorkingCopy,
            path: "README.md".to_owned(),
        });
        let _ = ui.perform_history(HistoryAction::ClearFileFilter);
        assert_eq!(ui.active().session.revset, "mine()");
        assert!(!file_filter_active(&ui));
    }

    #[test]
    fn snapshot_runs_fold_and_unfold() {
        let mut history = HistoryState {
            ops: vec![
                op("a", true, true),
                op("b", true, false),
                op("c", true, false),
                op("d", false, false),
                op("e", true, false),
                op("f", false, false),
            ],
            ..HistoryState::default()
        };
        // The current op always shows; a lone snapshot isn't worth a fold.
        assert_eq!(shape(&history), ["a", "bx2", "d", "e", "f"]);
        history.expanded_runs.insert("b".to_owned());
        assert_eq!(shape(&history), ["a", "bx2", "b", "c", "d", "e", "f"]);
    }
}
