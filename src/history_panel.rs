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

use crate::Cx;
use gpui_kit::{AnyElement, Window};
use std::collections::HashSet;

use diffui_core::history::{
    ChangedCommit, CommitBrief, EvologEntry, FileAnnotation, HistoryDiffTarget, HistoryRequest,
    HistoryResponse, OperationChanges, OperationEntry, file_history_revset, now_millis,
    relative_age,
};
use diffui_core::{DiffDocument, JobId, MutationOp, RevisionDetails, RevisionSelection, Vcs};

use crate::diff_view::AnnotationLabel;
use crate::prose::Prose;
use crate::revision_list::{
    CAPTION_TEXT_SIZE, GUTTER_LEFT_PADDING, GUTTER_PADDING, ROW_LINE_GAP, SMALL_TEXT_SIZE,
};
use crate::theme::{ThemeSpec, space};
use crate::{
    Action, ConfirmDialog, Diffui, MainView, PendingMutation, TabId, activity, meta_line,
    modes::Mode,
};

/// Operations read per page of the op log.
const OP_LOG_PAGE: usize = 300;

/// Width of the blame column, in monospace cells: change id, author, age.
const ANNOTATION_CHARS: usize = 25;

/// Height of every row in an operation's changed-commit and moved-ref
/// lists: a compact icon button on an even inset, so a row without the
/// button is just as tall as one with it.
const CHANGE_ROW_HEIGHT: f32 = crate::theme::control::COMPACT + 2.0 * space::XXS;

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
    /// The history diff scrolled onto another file.
    DiffFileChanged(usize),
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
    /// The file the history diff is on. The diff widget treats the file it
    /// is given as the one to show, so this must follow its scroll reports
    /// or it pulls the view back to the file it was given.
    diff_file: usize,
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
    /// The diff on screen and its document id.
    pub(crate) fn diff_document(&self) -> Option<(&DiffDocument, u64)> {
        self.diff.as_ref().map(|diff| (&diff.document, diff.doc_id))
    }

    /// Whether the source view shows its blame gutter.
    pub(crate) fn annotating(&self) -> bool {
        self.annotate
    }

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
    pub(crate) fn perform_history(&mut self, action: HistoryAction, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
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
                        return {
                            self.enter_history_view(cx);
                            self.history_select_change(tab, index, cx);
                        };
                    }
                }
                self.enter_history_view(cx)
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
                self.enter_history_view(cx);
                self.history_send(tab, job, HistoryRequest::Evolog { revision }, cx);
                {}
            }
            HistoryAction::Reload => match self.active().history.kind.clone() {
                HistoryKind::OpLog => self.reload_op_log(tab, cx),
                HistoryKind::Evolog { revision, .. } => {
                    self.perform_history(HistoryAction::ShowEvolog(revision), cx)
                }
            },
            HistoryAction::LoadMoreOperations => {
                let history = &mut self.active_mut().history;
                history.ops_limit += OP_LOG_PAGE;
                self.reload_op_log(tab, cx)
            }
            HistoryAction::SelectOperation(id) => self.history_select_operation(tab, id, cx),
            HistoryAction::ToggleSnapshotRun(id) => {
                let runs = &mut self.active_mut().history.expanded_runs;
                if !runs.remove(&id) {
                    runs.insert(id);
                }
            }
            HistoryAction::SelectChange(index) => self.history_select_change(tab, index, cx),
            HistoryAction::SelectVersion(index) => self.history_select_version(tab, index, cx),
            HistoryAction::SetEvologDiff(mode) => {
                self.active_mut().history.evolog_diff = mode;
                match self.active().history.selected_version {
                    Some(index) => self.history_select_version(tab, index, cx),
                    None => {}
                }
            }
            HistoryAction::RestoreOperation(id) => self.confirm_operation(tab, id, true, cx),
            HistoryAction::RevertOperation(id) => self.confirm_operation(tab, id, false, cx),
            HistoryAction::OpenRevision(commit_id) => {
                let selection = self.active().session.canonical_selection(&commit_id);
                {
                    self.perform(Action::SetMainView(MainView::Diff), cx);
                    self.perform(Action::SelectRevision(selection), cx);
                }
            }
            HistoryAction::DiffScrolled(offset) => {
                self.active_mut().history.diff_scroll = offset;
            }
            HistoryAction::DiffFileChanged(index) => {
                self.active_mut().history.diff_file = index;
            }
            HistoryAction::FileHistory { revision, path } => {
                self.show_file_history(revision, path, cx)
            }
            HistoryAction::CurrentFileHistory => match current_file(self) {
                Some((revision, path)) => self.show_file_history(revision, path, cx),
                None => {}
            },
            HistoryAction::ClearFileFilter => {
                let Some(filter) = self.active_mut().history.file_filter.take() else {
                    return;
                };
                if self.active().session.revset == filter.revset {
                    self.perform(Action::SetRevset(filter.previous), cx);
                }
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
                self.sync_history(cx)
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
                        self.perform_history(HistoryAction::OpenRevision(commit_id), cx)
                    }
                    None => {}
                }
            }
        }
    }

    /// Switch the active tab to the History view, reading the op log when
    /// it has never been read or the repository moved since.
    pub(crate) fn enter_history_view(&mut self, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        if !is_jj(self) {
            return;
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
        self.sync_history_for(tab, cx)
    }

    fn reload_op_log(&mut self, tab: TabId, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
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
        if let Some(old) = cancel {
            self.send(tab, diffui_core::Command::Cancel { job: old }, cx);
        }
        self.history_send(tab, job, HistoryRequest::OpLog { limit }, cx);
    }

    fn history_send(&mut self, tab: TabId, job: JobId, request: HistoryRequest, cx: &mut Cx) {
        self.send(tab, diffui_core::Command::History { job, request }, cx)
    }

    fn history_select_operation(&mut self, tab: TabId, id: String, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let history = &mut state.history;
        if history.selected_op.as_deref() == Some(id.as_str()) && history.op_changes.is_some() {
            return;
        }
        history.selected_op = Some(id.clone());
        history.op_changes = None;
        history.selected_change = None;
        history.diff = None;
        history.error = None;
        let job = JobId::next();
        history.op_job = Some(job);
        self.history_send(
            tab,
            job,
            HistoryRequest::ShowOperation { operation_id: id },
            cx,
        )
    }

    fn history_select_change(&mut self, tab: TabId, index: usize, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let history = &mut state.history;
        let Some(target) = history
            .op_changes
            .as_ref()
            .and_then(|changes| changes.commits.get(index))
            .map(ChangedCommit::diff_target)
        else {
            return;
        };
        history.selected_change = Some(index);
        self.history_load_diff(tab, target, cx)
    }

    fn history_select_version(&mut self, tab: TabId, index: usize, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let history = &mut state.history;
        let Some(entry) = history.evolog.get(index) else {
            return;
        };
        let target = match (history.evolog_diff, entry.interdiff_target()) {
            (EvologDiff::Previous, Some(target)) => target,
            _ => HistoryDiffTarget::Commit {
                commit_id: entry.commit.commit_id.clone(),
            },
        };
        history.selected_version = Some(index);
        self.history_load_diff(tab, target, cx)
    }

    fn history_load_diff(&mut self, tab: TabId, target: HistoryDiffTarget, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
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
            return;
        }
        let cancel = history.diff_job.take().map(|(job, _)| job);
        let job = JobId::next();
        history.diff_job = Some((job, target.clone()));
        if let Some(old) = cancel {
            self.send(tab, diffui_core::Command::Cancel { job: old }, cx);
        }
        self.history_send(tab, job, HistoryRequest::Diff { target }, cx);
    }

    /// Raise the confirmation for a restore or a revert, holding the
    /// mutation (and its queued activity row) until the user answers.
    fn confirm_operation(&mut self, tab: TabId, id: String, restore: bool, cx: &mut Cx) {
        if !self.active().session.capabilities.mutate {
            return;
        }
        let short = &id[..id.len().min(12)];
        let summary = self
            .active()
            .history
            .ops
            .iter()
            .find(|op| op.id == id)
            .map(|op| meta_line::shorten_ids(op.summary()))
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
    }

    fn show_file_history(&mut self, revision: RevisionSelection, path: String, cx: &mut Cx) {
        if !is_jj(self) {
            return;
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
        self.perform(Action::SetMainView(MainView::Diff), cx);
        self.perform(Action::SetRevset(revset), cx);
        let target = self.active_mut();
        target.history.filter_loading = target.session.streaming();
        {}
    }

    /// Route a history job's event. `None` when the event isn't one.
    pub(crate) fn history_event(
        &mut self,
        tab: TabId,
        event: &diffui_core::Event,
        cx: &mut Cx,
    ) -> Option<()> {
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
                    history.diff_file = 0;
                    history.diff = Some(HistoryDiff {
                        target,
                        document,
                        details,
                        doc_id,
                    });
                }
                HistoryResponse::Annotation(annotation) => {
                    history.annotation_job = None;
                    let labels = annotation_labels(annotation);
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
            Some(action) if self.is_active(tab) => self.perform_history(action, cx),
            _ => {}
        })
    }

    /// Bring the active tab's history views in line with what is on screen:
    /// re-read a stale op log, ask for a missing annotation, and keep the
    /// file filter pointed at its file. Called once per update.
    pub(crate) fn sync_history(&mut self, cx: &mut Cx) {
        match self.active_tab_id() {
            Some(tab) => self.sync_history_for(tab, cx),
            None => {}
        }
    }

    fn sync_history_for(&mut self, tab: TabId, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        if state.handle.is_none() {
            return;
        }
        let fingerprint = state
            .session
            .repository_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.clone());

        // The op log is current while the view shows it.
        if state.main_view == MainView::History
            && state.history.kind == HistoryKind::OpLog
            && state.history.ops_job.is_none()
            && state.history.ops_read_at.as_ref() != Some(&fingerprint)
        {
            self.reload_op_log(tab, cx);
        }

        // The blame column follows the file the source browser shows.
        let Some(state) = self.tab_mut(tab) else {
            return;
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
                self.history_send(tab, job, HistoryRequest::Annotate { revision, path }, cx);
            }
        }

        // A file filter the user replaced by typing another revset is gone;
        // a live one opens its file in each diff it lands on.
        let Some(state) = self.tab_mut(tab) else {
            return;
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
                self.perform(Action::SelectRevision(selection), cx);
            }
        }
        let Some(state) = self.tab_mut(tab) else {
            return;
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
            Self::after(std::time::Duration::from_millis(80), cx, move |this, cx| {
                this.perform(Action::JumpToFile(path), cx);
            });
        }
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

/// One blame entry per line of `annotation`.
fn annotation_labels(annotation: &FileAnnotation) -> Vec<AnnotationLabel> {
    let now = now_millis();
    let mut previous: Option<u32> = None;
    annotation
        .lines
        .iter()
        .map(|&origin| {
            let first = previous != Some(origin);
            previous = Some(origin);
            let Some(commit) = annotation.origins.get(origin as usize) else {
                return AnnotationLabel::default();
            };
            if !first {
                return AnnotationLabel {
                    hidden: !commit.visible,
                    ..AnnotationLabel::default()
                };
            }
            let id = commit.short_change_id().to_owned();
            AnnotationLabel {
                unique: match annotation.unique_prefix_lens.get(origin as usize) {
                    Some(&len) if len > 0 => len.min(id.len()),
                    _ => id.len(),
                },
                id,
                author: commit.author.clone(),
                age: relative_age(commit.time_millis, now),
                hidden: !commit.visible,
            }
        })
        .collect()
}

/// The blame labels of the active tab's source browser.
fn shown_annotation_labels(ui: &Diffui) -> &[AnnotationLabel] {
    ui.active()
        .history
        .annotation
        .as_ref()
        .map_or(&[], |view| view.labels.as_slice())
}

/// The blame column for the source browser's code view, when annotate is on and the
/// annotation for the shown file has landed.
pub(crate) fn source_annotations(
    ui: &Diffui,
    cx: &mut crate::Cx,
) -> Option<crate::diff_view::AnnotationGutter> {
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
    let theme = ui.resolved_theme().spec();
    Some(crate::diff_view::AnnotationGutter {
        labels: crate::diff_view::Slice::new(cx.entity(), shown_annotation_labels),
        width_chars: ANNOTATION_CHARS,
        on_click: std::rc::Rc::new(Diffui::listen(cx, |this, line: &usize, _, cx| {
            this.perform(Action::History(HistoryAction::AnnotationClicked(*line)), cx);
        })),
        style: crate::diff_view::AnnotationStyle {
            font: ui.config.ui_font,
            id: theme.accent,
            id_rest: theme.subtle_text,
            author: theme.muted_text,
            age: theme.subtle_text,
            hidden: theme.subtle_text,
            hover: crate::theme::chip_background(theme.muted_text),
        },
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

// The lists' rows sit like the revision graph's: row cards inset from the
// row by the graph's card insets, content starting where a lane-less graph
// row's would and stopping at the scrollbar gutter, two-line rows as tall
// as a revision row and one-line rows as a file row.

/// Where row content starts, from the pane's leading edge.
const ROW_CONTENT_LEFT: f32 = GUTTER_LEFT_PADDING + GUTTER_PADDING;
/// iced centres text in a line box 1.3× its size; the graph stacks bare
/// text boxes [`ROW_LINE_GAP`] apart, so a two-line row's spacing gives
/// back both lines' half-leading.
const TWO_LINE_SPACING: f32 = ROW_LINE_GAP - 0.15 * (SMALL_TEXT_SIZE + CAPTION_TEXT_SIZE);

/// One displayed op-log row: an operation, or a folded run of snapshots.
enum OpRow<'a> {
    Op(&'a OperationEntry),
    Run {
        first: &'a OperationEntry,
        count: usize,
        oldest: &'a OperationEntry,
        /// The run's operations, when it is unfolded.
        children: Option<&'a [OperationEntry]>,
    },
}

/// Width of the chevron column in a snapshot run's row.
const RUN_CHEVRON: f32 = 12.0;

fn ref_side_label(side: &[CommitBrief]) -> String {
    match side {
        [] => String::new(),
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
                children: history
                    .expanded_runs
                    .contains(&op.id)
                    .then(|| &ops[index..run_end]),
            });
            index = run_end;
        } else {
            rows.push(OpRow::Op(op));
            index += 1;
        }
    }
    rows
}

/// Stub: the history_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn source_bar_actions(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("history_panel::source_bar_actions")
}

/// Stub: the history_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn file_filter_banner(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    None
}

/// Stub: the history_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_history_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("history_panel::build_history_sidebar")
}

/// Stub: the history_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_history_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("history_panel::build_history_panel")
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
                OpRow::Run {
                    first,
                    count,
                    children,
                    ..
                } => match children {
                    Some(children) => format!(
                        "{}x{count}[{}]",
                        first.id,
                        children
                            .iter()
                            .map(|op| op.id.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    None => format!("{}x{count}", first.id),
                },
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

    fn deliver(
        ui: &mut Diffui,
        repo: &diffui_core::RepoId,
        payload: diffui_core::Payload,
        cx: &mut crate::Cx,
    ) {
        ui.act(cx, |ui, cx| {
            ui.on_repo_event(diffui_core::Event::new(repo.clone(), payload), cx)
        });
    }

    #[test]
    fn the_op_log_streams_in_and_lands_on_the_current_operation() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, repo) = jj_app();
            ui.perform_history(HistoryAction::ShowOpLog, cx);
            assert_eq!(ui.active().main_view, MainView::History);
            // No actor in a unit test, so the view's own read never starts.
            ui.perform_history(HistoryAction::Reload, cx);
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
                cx,
            );
            deliver(
                &mut ui,
                &repo,
                diffui_core::Payload::History {
                    job,
                    response: HistoryResponse::OpLogDone { truncated: true },
                },
                cx,
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
        });
    }

    #[test]
    fn a_failed_history_read_stays_in_its_pane() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, repo) = jj_app();
            ui.perform_history(HistoryAction::ShowOpLog, cx);
            ui.perform_history(HistoryAction::Reload, cx);
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
                cx,
            );
            assert_eq!(ui.active().history.error.as_deref(), Some("boom"));
            assert!(ui.active().history.ops_job.is_none());
            assert!(
                ui.toasts.is_empty(),
                "the projection's generic failure toast is for its own jobs"
            );
        });
    }

    #[test]
    fn a_file_filter_restores_the_previous_revset() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, _repo) = jj_app();
            ui.active_mut().session.revset = "mine()".to_owned();
            ui.perform_history(
                HistoryAction::FileHistory {
                    revision: RevisionSelection::WorkingCopy,
                    path: "src/main.rs".to_owned(),
                },
                cx,
            );
            assert_eq!(
                ui.active().session.revset,
                r#"::@ & files(root:"src/main.rs")"#
            );
            assert!(file_filter_active(&ui));
            // A second file replaces the first instead of stacking on it.
            ui.perform_history(
                HistoryAction::FileHistory {
                    revision: RevisionSelection::WorkingCopy,
                    path: "README.md".to_owned(),
                },
                cx,
            );
            ui.perform_history(HistoryAction::ClearFileFilter, cx);
            assert_eq!(ui.active().session.revset, "mine()");
            assert!(!file_filter_active(&ui));
        });
    }

    #[test]
    fn the_history_diff_keeps_the_file_it_was_scrolled_to() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, repo) = jj_app();
            ui.perform_history(HistoryAction::ShowOpLog, cx);
            let land = |ui: &mut Diffui, commit_id: &str, cx: &mut crate::Cx| {
                let job = JobId::next();
                let target = HistoryDiffTarget::Commit {
                    commit_id: commit_id.to_owned(),
                };
                ui.active_mut().history.diff_job = Some((job, target.clone()));
                deliver(
                    ui,
                    &repo,
                    diffui_core::Payload::History {
                        job,
                        response: HistoryResponse::Diff {
                            target,
                            document: DiffDocument::default(),
                            details: None,
                        },
                    },
                    cx,
                );
            };
            land(&mut ui, "a", cx);
            assert_eq!(ui.active().history.diff_file, 0);
            // The diff widget scrolls the file it is handed into view, so the
            // file it reports scrolling onto must be what it is handed next;
            // a fixed 0 snapped every scroll past the first file back up.
            ui.act(cx, |ui, cx| {
                ui.perform(Action::History(HistoryAction::DiffFileChanged(2)), cx)
            });
            assert_eq!(ui.active().history.diff_file, 2);
            land(&mut ui, "b", cx);
            assert_eq!(
                ui.active().history.diff_file,
                0,
                "a new diff opens at its top"
            );
        });
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
        assert_eq!(shape(&history), ["a", "bx2[b,c]", "d", "e", "f"]);
    }
}
