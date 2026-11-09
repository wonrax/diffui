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
use crate::ui::Tip as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Div, ElementId, SharedString, Stateful, Window, div, px};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use diffui_widgets::scroll_resample::{self, Resampler};

use diffui_core::history::{
    ChangedCommit, CommitBrief, CommitChangeKind, EvologEntry, FileAnnotation, HistoryDiffTarget,
    HistoryRequest, HistoryResponse, OperationChanges, OperationEntry, RefChange, RefKind,
    file_history_revset, now_millis, relative_age,
};
use diffui_core::{DiffDocument, JobId, MutationOp, RevisionDetails, RevisionSelection, Vcs};

use crate::diff_view::{AnnotationLabel, DiffProps, DocSource};
use crate::ellipsis::{Line, Span};
use crate::prose::Prose;
use crate::revision_list::{
    CAPTION_TEXT_SIZE, CONTENT_PADDING, FILE_ROW_HEIGHT, GUTTER_LEFT_PADDING, GUTTER_PADDING,
    REVISION_ROW_HEIGHT, ROW_CARD_INSET_X, ROW_CARD_INSET_Y, ROW_CARD_RADIUS, ROW_LINE_GAP,
    SMALL_TEXT_SIZE,
};
use crate::theme::{
    Color, Font, ThemeSpec, Weight, chip_background, diff_palette, emphasis_font, radius, space,
    text_size,
};
use crate::typography::{self, Aligned};
use crate::{
    Action, ConfirmDialog, Diffui, MainView, PendingMutation, TabId, activity, icons, meta_line,
    modes::Mode, ui,
};

/// Operations read per page of the op log.
const OP_LOG_PAGE: usize = 300;

/// Width of the blame column, in monospace cells: change id, author, age.
const ANNOTATION_CHARS: usize = 25;

/// Height of every row in an operation's changed-commit and moved-ref
/// lists: a compact icon button on an even inset, so a row without the
/// button is just as tall as one with it.
const CHANGE_ROW_HEIGHT: f32 = crate::theme::control::COMPACT + 2.0 * space::XXS;

/// Tallest an operation's changes get above the diff.
const OP_CHANGES_MAX_HEIGHT: f32 = 260.0;

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
    /// The op log's rows as the list last measured them, and its scroll.
    op_list: OpLogList,
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
                            self.history_select_change(tab, index);
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
                self.history_send(tab, job, HistoryRequest::Evolog { revision });
                {}
            }
            HistoryAction::Reload => match self.active().history.kind.clone() {
                HistoryKind::OpLog => self.reload_op_log(tab),
                HistoryKind::Evolog { revision, .. } => {
                    self.perform_history(HistoryAction::ShowEvolog(revision), cx)
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
            }
            HistoryAction::SelectChange(index) => self.history_select_change(tab, index),
            HistoryAction::SelectVersion(index) => self.history_select_version(tab, index),
            HistoryAction::SetEvologDiff(mode) => {
                self.active_mut().history.evolog_diff = mode;
                if let Some(index) = self.active().history.selected_version {
                    self.history_select_version(tab, index)
                }
            }
            HistoryAction::RestoreOperation(id) => self.confirm_operation(tab, id, true),
            HistoryAction::RevertOperation(id) => self.confirm_operation(tab, id, false),
            HistoryAction::OpenRevision(commit_id) => {
                let selection = self.active().session.canonical_selection(&commit_id);
                {
                    self.perform(Action::SetMainView(MainView::Diff), cx);
                    self.perform(Action::SelectRevision(selection), cx);
                }
            }
            HistoryAction::DiffFileChanged(index) => {
                self.active_mut().history.diff_file = index;
            }
            HistoryAction::FileHistory { revision, path } => {
                self.show_file_history(revision, path, cx)
            }
            HistoryAction::CurrentFileHistory => {
                if let Some((revision, path)) = current_file(self) {
                    self.show_file_history(revision, path, cx)
                }
            }
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
                if let Some((commit_id, path)) = target {
                    // Already showing that revision: no new diff will
                    // land, so jump in the one on screen.
                    let session = &self.active().session;
                    let seen = (session.selected_revision
                        != session.canonical_selection(&commit_id))
                    .then_some(session.document_id);
                    self.active_mut().history.blame_jump = Some((path, seen));
                    self.perform_history(HistoryAction::OpenRevision(commit_id), cx)
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

    fn reload_op_log(&mut self, tab: TabId) {
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
            self.send(tab, diffui_core::Command::Cancel { job: old });
        }
        self.history_send(tab, job, HistoryRequest::OpLog { limit });
    }

    fn history_send(&mut self, tab: TabId, job: JobId, request: HistoryRequest) {
        self.send(tab, diffui_core::Command::History { job, request })
    }

    fn history_select_operation(&mut self, tab: TabId, id: String) {
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
        self.history_send(tab, job, HistoryRequest::ShowOperation { operation_id: id })
    }

    fn history_select_change(&mut self, tab: TabId, index: usize) {
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
        self.history_load_diff(tab, target)
    }

    fn history_select_version(&mut self, tab: TabId, index: usize) {
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
        self.history_load_diff(tab, target)
    }

    fn history_load_diff(&mut self, tab: TabId, target: HistoryDiffTarget) {
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
            self.send(tab, diffui_core::Command::Cancel { job: old });
        }
        self.history_send(tab, job, HistoryRequest::Diff { target });
    }

    /// Raise the confirmation for a restore or a revert, holding the
    /// mutation (and its queued activity row) until the user answers.
    fn confirm_operation(&mut self, tab: TabId, id: String, restore: bool) {
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
            look: Default::default(),
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
        let _: () = match follow_up {
            Some(action) if self.is_active(tab) => self.perform_history(action, cx),
            _ => {}
        };
        Some(())
    }

    /// Bring the active tab's history views in line with what is on screen:
    /// re-read a stale op log, ask for a missing annotation, and keep the
    /// file filter pointed at its file. Called once per update.
    pub(crate) fn sync_history(&mut self, cx: &mut Cx) {
        if let Some(tab) = self.active_tab_id() {
            self.sync_history_for(tab, cx)
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
            self.reload_op_log(tab);
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
                self.history_send(tab, job, HistoryRequest::Annotate { revision, path });
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
/// GPUI centres text in a line box 1.3× its size; the graph stacks bare
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
/// An operation's age on its row's title line.
const AGE_TEXT_SIZE: f32 = 11.5;
/// The op detail's title.
const DETAIL_TITLE_SIZE: f32 = 18.0;
/// A section label: "Changed revisions 1".
const SECTION_LABEL_SIZE: f32 = 12.5;

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

/// The source info bar's history controls: the blame toggle and the browsed
/// file's history.
pub(crate) fn source_bar_actions(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let on = annotate_on(ui);
    let blame = ui::text_button(
        "source-blame",
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
        cx,
    )
    .on_click(Diffui::on_action(
        Action::History(HistoryAction::ToggleAnnotate),
        cx,
    ));
    let history = ui::text_button(
        "source-file-history",
        Some((icons::FILE_CLOCK, theme.muted_text)),
        "History",
        ui::Variant::Ghost,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
        cx,
    )
    .on_click(Diffui::on_action(
        Action::History(HistoryAction::CurrentFileHistory),
        cx,
    ));
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::XS))
        .child(with_tooltip(
            "source-blame-tip",
            blame,
            "Show which change last touched each line",
            ui,
            theme,
        ))
        .child(with_tooltip(
            "source-file-history-tip",
            history,
            "Show the revisions that touched this file",
            ui,
            theme,
        ))
        .into_any_element()
}

/// `control` with the app's hover label.
fn with_tooltip(
    id: &'static str,
    control: impl IntoElement,
    label: &'static str,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .tip(label, ui.config.ui_font, theme)
        .child(control)
}

/// The banner above the revision list while the graph shows one file's
/// history.
pub(crate) fn file_filter_banner(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    if !file_filter_active(ui) {
        return None;
    }
    let filter = ui.active().history.file_filter.as_ref()?;
    let title = Prose::from("History of ")
        .path(&filter.path)
        .line(
            ui.config.ui_font,
            ui.config.mono_font,
            theme.text,
            theme,
            text_size::UI,
        )
        .view();
    let close = ui::close_button("file-filter-close", theme.muted_text, theme, cx).on_click(
        Diffui::on_action(Action::History(HistoryAction::ClearFileFilter), cx),
    );
    // The icon and the title share a line; the close button is a control,
    // centred in the banner's box with an even inset, not hung off the text.
    let line = typography::centered(
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(icons::FILE_CLOCK, ui::ICON_SIZE, theme.accent),
                    ui::ICON_SIZE,
                ),
                Aligned::text(title, ui.config.ui_font, text_size::UI).fill(),
            ],
            space::SM,
        ),
        ui.config.ui_font,
        text_size::UI,
    );
    Some(
        crate::field::panel_strip()
            .child(
                div()
                    .w_full()
                    .h(px(crate::theme::control::COMPACT))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::SM))
                    .pl(px(space::MD))
                    .pr(px(ui::BANNER_INSET))
                    .bg(chip_background(theme.accent))
                    .rounded(px(radius::CONTROL))
                    .child(div().flex_1().min_w_0().child(line))
                    .child(with_tooltip(
                        "file-filter-close-tip",
                        close,
                        "Show the whole graph again",
                        ui,
                        theme,
                    )),
            )
            .into_any_element(),
    )
}

// The lists' rows sit like the revision graph's: row cards inset from the
// row by the graph's card insets, content starting where a lane-less graph
// row's would and stopping at the scrollbar gutter, two-line rows as tall
// as a revision row and one-line rows as a file row.

/// Left inset of a list of rows: the graph's card inset, plus the window border
/// at the leading edge as the graph adds it.
fn list_left() -> f32 {
    ROW_CARD_INSET_X + crate::chrome::window_border()
}

/// Padding inside a row card, putting content at [`ROW_CONTENT_LEFT`].
fn card_left() -> f32 {
    ROW_CONTENT_LEFT - ROW_CARD_INSET_X - crate::chrome::window_border()
}

/// A clickable row card, `height` tall: the app's selection and hover washes.
fn row_card(
    id: impl Into<ElementId>,
    height: f32,
    selected: bool,
    action: Action,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Stateful<Div> {
    let mut card = div()
        .id(id)
        .w_full()
        .h(px(height))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .rounded(px(ROW_CARD_RADIUS))
        .cursor_pointer()
        .text_color(theme.text)
        .on_click(Diffui::on_action(action, cx));
    if selected {
        card = card.bg(theme.selected_file);
    } else {
        let wash = chip_background(theme.muted_text);
        card = card.hover(move |style| style.bg(wash));
    }
    card
}

/// A row card holding one or two lines of content at the graph's inset.
fn list_row(
    id: impl Into<ElementId>,
    row_height: f32,
    selected: bool,
    action: Action,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Stateful<Div> {
    row_card(
        id,
        row_height - 2.0 * ROW_CARD_INSET_Y,
        selected,
        action,
        theme,
        cx,
    )
    .pl(px(card_left()))
    .pr(px(CONTENT_PADDING - ROW_CARD_INSET_X))
}

/// The graph's two-line stack: a title, and a caption-size line of ids and
/// meta under it.
fn two_line(title: impl IntoElement, meta: impl IntoElement) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(TWO_LINE_SPACING))
        .child(title)
        .child(meta)
}

fn caption(value: impl Into<SharedString>, color: Color, ui: &Diffui) -> Div {
    typography::line(value, ui.config.ui_font, text_size::CAPTION, color)
}

/// [`caption`] taking what's left of its row, cut to fit.
fn caption_fill(value: impl Into<String>, color: Color, ui: &Diffui) -> AnyElement {
    let value: String = value.into();
    Line::new(
        vec![Span::new(value)],
        ui.config.ui_font,
        text_size::CAPTION,
        color,
    )
    .view()
    .into_any_element()
}

/// A mono id placed on its line's baseline.
fn mono_on_line(value: impl Into<SharedString>, color: Color, ui: &Diffui) -> Aligned {
    Aligned::text(
        typography::line(value, ui.config.mono_font, CAPTION_TEXT_SIZE, color),
        ui.config.mono_font,
        CAPTION_TEXT_SIZE,
    )
}

/// A [`caption`] (or anything at its font and size), placed on its line's
/// baseline.
fn caption_on_line(caption: impl IntoElement, ui: &Diffui) -> Aligned {
    Aligned::text(caption, ui.config.ui_font, text_size::CAPTION)
}

/// A caption that takes what's left of a baseline row, cut to fit.
fn fill_on_line(element: impl IntoElement, font: Font, size: f32) -> Aligned {
    Aligned::text(element, font, size).fill()
}

fn select_op(id: &str) -> Action {
    Action::History(HistoryAction::SelectOperation(id.to_owned()))
}

fn op_row(
    ui: &Diffui,
    theme: ThemeSpec,
    op: &OperationEntry,
    now: i64,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if op.is_snapshot {
        return snapshot_row(ui, theme, op, now, window, cx);
    }
    let history = &ui.active().history;
    let selected = history.selected_op.as_deref() == Some(op.id.as_str());
    let age = relative_age(op.time_millis, now);
    let id_color = if op.is_current {
        theme.accent
    } else {
        theme.subtle_text
    };
    let mut meta = vec![mono_on_line(op.short_id().to_owned(), id_color, ui)];
    if op.is_current {
        meta.push(meta_line::badge("current", theme.accent, ui, window, cx));
    }
    // The command that wrote the op, when one was recorded (the jj CLI's,
    // or diffui's equivalent), says more than who ran it; the header below
    // the list still names the user.
    // One line whatever the command holds (a message's line breaks read as
    // spaces), cut short at the row's edge.
    match &op.args {
        Some(args) => meta.push(fill_on_line(
            crate::plan::command_prose(args)
                .line(
                    ui.config.mono_font,
                    ui.config.mono_font,
                    theme.subtle_text,
                    theme,
                    CAPTION_TEXT_SIZE,
                )
                .view(),
            ui.config.mono_font,
            CAPTION_TEXT_SIZE,
        )),
        None => meta.push(fill_on_line(
            caption_fill(op.user.clone(), theme.subtle_text, ui),
            ui.config.ui_font,
            text_size::CAPTION,
        )),
    }
    let meta = typography::baseline_row(meta, space::SM);
    let title = title_with_age(
        meta_line::description(
            op.summary(),
            SMALL_TEXT_SIZE,
            ui.config.ui_font,
            theme.text,
            ui,
            theme,
        )
        .view(),
        age,
        ui,
        theme,
    );
    list_row(
        SharedString::from(format!("op-{}", op.id)),
        REVISION_ROW_HEIGHT,
        selected,
        select_op(&op.id),
        theme,
        cx,
    )
    .child(two_line(title, meta))
    .into_any_element()
}

/// A snapshot on one quiet line, on its own or inside an unfolded run.
fn snapshot_row(
    ui: &Diffui,
    theme: ThemeSpec,
    op: &OperationEntry,
    now: i64,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let selected = ui.active().history.selected_op.as_deref() == Some(op.id.as_str());
    let mut line = vec![
        Aligned::glyph(snapshot_dot(theme), RUN_CHEVRON),
        Aligned::text(
            caption_fill(op.summary().to_owned(), theme.subtle_text, ui),
            ui.config.ui_font,
            text_size::CAPTION,
        )
        .shrink(),
        mono_on_line(
            op.short_id().to_owned(),
            if op.is_current {
                theme.accent
            } else {
                theme.subtle_text
            },
            ui,
        ),
    ];
    if op.is_current {
        line.push(meta_line::badge("current", theme.accent, ui, window, cx));
    }
    line.push(fill_on_line(div(), ui.config.ui_font, text_size::CAPTION));
    line.push(caption_on_line(
        caption(relative_age(op.time_millis, now), theme.subtle_text, ui),
        ui,
    ));
    list_row(
        SharedString::from(format!("op-{}", op.id)),
        FILE_ROW_HEIGHT,
        selected,
        select_op(&op.id),
        theme,
        cx,
    )
    .child(typography::baseline_row(line, space::SM).flex_1().min_w_0())
    .into_any_element()
}

/// A snapshot's marker: a small quiet dot in the chevron column, so a lone
/// snapshot lines up with a folded run's label.
fn snapshot_dot(theme: ThemeSpec) -> Div {
    const DOT: f32 = 5.0;
    div()
        .flex_none()
        .size(px(RUN_CHEVRON))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .size(px(DOT))
                .rounded(px(DOT / 2.0))
                .bg(crate::theme::mix(theme.border, theme.subtle_text, 0.35)),
        )
}

/// A two-line row's title: the text cut to fit, and the age right-aligned on
/// its baseline.
fn title_with_age(title: impl IntoElement, age: String, ui: &Diffui, theme: ThemeSpec) -> Div {
    let font = ui.config.ui_font;
    typography::baseline_row(
        [
            fill_on_line(title, font, SMALL_TEXT_SIZE),
            Aligned::text(
                typography::line(age, font, AGE_TEXT_SIZE, theme.subtle_text),
                font,
                AGE_TEXT_SIZE,
            ),
        ],
        space::MD,
    )
}

/// A folded run of snapshots, and its operations under it when unfolded:
/// indented behind a guide line dropped from the chevron, the way the file
/// tree nests a folder's files.
#[allow(clippy::too_many_arguments)]
fn run_row(
    ui: &Diffui,
    theme: ThemeSpec,
    first: &OperationEntry,
    count: usize,
    oldest: &OperationEntry,
    children: Option<&[OperationEntry]>,
    now: i64,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let chevron = if children.is_some() {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    let (from, to) = (
        relative_age(oldest.time_millis, now),
        relative_age(first.time_millis, now),
    );
    let span = if from == to {
        to
    } else {
        format!("{from}\u{2013}{to}")
    };
    let header = list_row(
        SharedString::from(format!("run-{}", first.id)),
        FILE_ROW_HEIGHT,
        false,
        Action::History(HistoryAction::ToggleSnapshotRun(first.id.clone())),
        theme,
        cx,
    )
    .child(
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(chevron, RUN_CHEVRON, theme.subtle_text),
                    RUN_CHEVRON,
                ),
                fill_on_line(
                    caption(
                        format!("{count} working-copy snapshots"),
                        theme.subtle_text,
                        ui,
                    ),
                    ui.config.ui_font,
                    text_size::CAPTION,
                ),
                caption_on_line(caption(span, theme.subtle_text, ui), ui),
            ],
            space::SM,
        )
        .flex_1()
        .min_w_0(),
    );
    let Some(children) = children else {
        return header.into_any_element();
    };

    let mut rows = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.0 * ROW_CARD_INSET_Y));
    for op in children {
        rows = rows.child(snapshot_row(ui, theme, op, now, window, cx));
    }
    // The guide hangs from the chevron's centre; the children's ids line up
    // with the run's label. Positions are within the list's padding, where
    // a card's content starts at the card padding's left.
    let content = card_left();
    let guide_x = content + RUN_CHEVRON / 2.0 - 0.5;
    let label_x = content + RUN_CHEVRON + space::SM;
    let child_x = label_x - content;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(2.0 * ROW_CARD_INSET_Y))
        .child(header)
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .child(div().flex_none().w(px(guide_x)))
                .child(div().flex_none().w(px(1.0)).bg(theme.border))
                .child(div().flex_none().w(px(child_x - guide_x - 1.0)))
                .child(rows),
        )
        .into_any_element()
}

fn version_row(
    ui: &Diffui,
    theme: ThemeSpec,
    index: usize,
    entry: &EvologEntry,
    now: i64,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let selected = ui.active().history.selected_version == Some(index);
    let mut meta = vec![mono_on_line(
        entry.commit.short_commit_id().to_owned(),
        theme.muted_text,
        ui,
    )];
    if entry.commit.visible {
        meta.push(meta_line::badge("current", theme.accent, ui, window, cx));
    }
    if entry.commit.empty {
        meta.push(meta_line::badge("empty", theme.subtle_text, ui, window, cx));
    }
    let operation = entry.operation.as_ref().map_or_else(
        || "(operation no longer in the log)".to_owned(),
        |op| meta_line::shorten_ids(&op.summary),
    );
    meta.push(fill_on_line(
        caption_fill(operation, theme.subtle_text, ui),
        ui.config.ui_font,
        text_size::CAPTION,
    ));
    meta.push(caption_on_line(
        caption(
            relative_age(entry.commit.time_millis, now),
            theme.subtle_text,
            ui,
        ),
        ui,
    ));
    let meta = typography::baseline_row(meta, space::SM);
    let title = summary_line(&entry.commit, SMALL_TEXT_SIZE, ui.config.ui_font, theme);
    list_row(
        SharedString::from(format!("version-{index}")),
        REVISION_ROW_HEIGHT,
        selected,
        Action::History(HistoryAction::SelectVersion(index)),
        theme,
        cx,
    )
    .child(two_line(title, meta))
    .into_any_element()
}

/// A commit's first line, `(no description set)` in a quieter colour when it has
/// none, cut to fit.
fn summary_line(commit: &CommitBrief, size: f32, font: Font, theme: ThemeSpec) -> Div {
    let (summary, color) = if commit.summary.is_empty() {
        ("(no description set)".to_owned(), theme.subtle_text)
    } else {
        (commit.summary.clone(), theme.text)
    };
    Line::new(vec![Span::new(summary)], font, size, color).view()
}

/// A list's header, its leading glyph on the rows' content edge and its
/// trailing button's glyph on their right edge. `leading_button`: the
/// header starts with an icon button, whose glyph sits centred in it.
fn header_bar(content: impl IntoElement, leading_button: bool, theme: ThemeSpec) -> Div {
    let glyph_inset = (ui::Size::Compact.height() - ui::ICON_SIZE) / 2.0;
    div()
        .w_full()
        .flex_none()
        .h(px(crate::field::PANEL_HEADER_HEIGHT))
        .flex()
        .items_center()
        .pl(px(
            ROW_CONTENT_LEFT - if leading_button { glyph_inset } else { 0.0 }
        ))
        .pr(px((CONTENT_PADDING - glyph_inset).max(0.0)))
        .bg(theme.panel_background)
        .child(content)
}

fn note(message: impl Into<SharedString>, color: Color, ui: &Diffui) -> Div {
    div()
        .w_full()
        .flex_none()
        .px(px(space::LG))
        .py(px(space::SM))
        .font(ui.config.ui_font.gpui())
        .text_size(px(text_size::UI))
        .text_color(color)
        .child(message.into())
}

/// The History view's left pane: the op log, or the evolog it was switched to.
pub(crate) fn build_history_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let history = &ui.active().history;
    let now = now_millis();
    let reload = with_tooltip(
        "history-reload-tip",
        ui::icon_button(
            "history-reload",
            icons::REFRESH,
            theme.muted_text,
            ui::Size::Compact,
            theme,
            cx,
        )
        .on_click(Diffui::on_action(
            Action::History(HistoryAction::Reload),
            cx,
        )),
        "Reload",
        ui,
        theme,
    );
    let title_font = emphasis_font(ui.config.ui_font, Weight::MEDIUM);

    let (header, list): (Div, AnyElement) = match &history.kind {
        HistoryKind::OpLog => {
            let header = div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SM))
                .child(icons::icon(
                    icons::HISTORY,
                    ui::ICON_SIZE,
                    theme.subtle_text,
                ))
                .child(div().flex_1().child(typography::centered(
                    typography::line("Operations", title_font, text_size::UI, theme.text),
                    title_font,
                    text_size::UI,
                )))
                .child(reload);
            (header, op_log_list(ui, cx))
        }
        HistoryKind::Evolog { label, .. } => {
            let back = with_tooltip(
                "history-back-tip",
                ui::icon_button(
                    "history-back",
                    icons::ARROW_LEFT,
                    theme.muted_text,
                    ui::Size::Compact,
                    theme,
                    cx,
                )
                .on_click(Diffui::on_action(
                    Action::History(HistoryAction::ShowOpLog),
                    cx,
                )),
                "Back to the operation log",
                ui,
                theme,
            );
            let header = div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::XS))
                .child(back)
                .child(div().flex_1().min_w_0().child(typography::baseline_row(
                    [
                        Aligned::text(
                            typography::line(
                                "Evolution of ",
                                title_font,
                                text_size::UI,
                                theme.text,
                            ),
                            title_font,
                            text_size::UI,
                        ),
                        mono_on_line(label.clone(), theme.accent, ui),
                    ],
                    0.0,
                )))
                .child(reload);
            let mut rows = list_column()
                .gap(px(2.0 * ROW_CARD_INSET_Y))
                .pt(px(ROW_CARD_INSET_Y))
                .pb(px(ROW_CARD_INSET_Y));
            for (index, entry) in history.evolog.iter().enumerate() {
                rows = rows.child(version_row(ui, theme, index, entry, now, window, cx));
            }
            if history.evolog_job.is_some() {
                rows = rows.child(note(
                    "Reading the change's history\u{2026}",
                    theme.subtle_text,
                    ui,
                ));
            }
            (
                header,
                ui::scrollable("history-evolog", rows).into_any_element(),
            )
        }
    };

    let leading_button = matches!(history.kind, HistoryKind::Evolog { .. });
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
        .child(header_bar(header, leading_button, theme))
        .child(div().flex_1().min_h_0().child(list))
        .into_any_element()
}

/// A column of history rows, inset like the graph's.
fn list_column() -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .pl(px(list_left()))
        .pr(px(ROW_CARD_INSET_X))
}

/// The op log, building only the rows on screen. A page holds hundreds of
/// operations, and a scroll tick rebuilds the pane holding the list: with every
/// row built that cost about 15ms a tick, over a 120 Hz frame. Each row is
/// still measured once when it arrives (see [`OpLogList`]), for the scrollbar.
fn op_log_list(ui: &Diffui, cx: &mut Cx) -> AnyElement {
    let list = &ui.active().history.op_list;
    list.sync(op_log_keys(ui));
    let state = list.state.clone();
    let app = cx.entity();
    let now = now_millis();
    // The panes repaint when the list leaves or reaches its top, for the
    // header's fade; scrolling otherwise only moves the list.
    let scrolled = list.scrolled.clone();
    let app_id = app.entity_id();
    state.set_scroll_handler({
        let scrolled = scrolled.clone();
        move |event, _, cx| note_scrolled(&scrolled, event.is_scrolled, app_id, cx)
    });
    let wheel = op_log_wheel(&state, &list.wheel, &scrolled, app_id);
    let fade = crate::field::scroll_fade(
        if state.logical_scroll_top() == gpui_kit::ListOffset::default() {
            0.0
        } else {
            1.0
        },
        ui.resolved_theme().spec().panel_background,
    );
    let rows = gpui_kit::list(state.clone(), move |index, window, cx| {
        diffui_widgets::frame_log::time("oplog.row", || {
            app.update(cx, |ui, cx| {
                let theme = ui.resolved_theme().spec();
                let history = &ui.active().history;
                let row = match op_rows(history).get(index) {
                    Some(OpRow::Op(op)) => op_row(ui, theme, op, now, window, cx),
                    Some(&OpRow::Run {
                        first,
                        count,
                        oldest,
                        children,
                    }) => run_row(ui, theme, first, count, oldest, children, now, window, cx),
                    None => op_log_footer(ui, theme, cx),
                };
                // Half the gap on each side of a row: the gap between rows, and
                // half of it at either end, as a stacked column would have.
                list_column()
                    .py(px(ROW_CARD_INSET_Y))
                    .child(row)
                    .into_any_element()
            })
        })
    })
    .size_full();
    div()
        .relative()
        .size_full()
        .child(wheel)
        .child(rows)
        .children(fade)
        .vertical_scrollbar(&state)
        .into_any_element()
}

/// Records whether the op log is off its top, notifying the app when that
/// changes.
fn note_scrolled(
    flag: &Cell<bool>,
    scrolled: bool,
    app: gpui_kit::EntityId,
    cx: &mut gpui_kit::App,
) {
    if flag.replace(scrolled) != scrolled {
        cx.notify(app);
    }
}

/// Takes the op log's wheel events from the list and scrolls it evenly over the
/// frames that follow, as the graph and the diff do (`scroll_resample`). It sits
/// under the list: laid out first, so a frame's motion lands before the list's
/// layout, and its capture-phase handler runs before the list's own.
fn op_log_wheel(
    state: &gpui_kit::ListState,
    wheel: &Rc<RefCell<Resampler>>,
    scrolled: &Rc<Cell<bool>>,
    app: gpui_kit::EntityId,
) -> AnyElement {
    if scroll_resample::delay().is_zero() {
        return div().into_any_element();
    }
    let (state, wheel, scrolled) = (state.clone(), wheel.clone(), scrolled.clone());
    let events = wheel.clone();
    gpui_kit::canvas(
        move |bounds, window, cx| {
            let step = wheel.borrow_mut().take(Instant::now());
            if step != 0.0 {
                diffui_widgets::frame_log::value("sy", step);
            }
            if step != 0.0 {
                let top = -f64::from(state.scroll_px_offset_for_scrollbar().y);
                let max = f64::from(state.max_offset_for_scrollbar().y);
                let top = (top + step).clamp(0.0, max) as f32;
                state.set_offset_from_scrollbar(gpui_kit::point(px(0.0), px(-top)));
                note_scrolled(&scrolled, top > 0.0, app, cx);
            }
            if wheel.borrow().pending() {
                window.request_animation_frame();
            }
            window.insert_hitbox(bounds, gpui_kit::HitboxBehavior::Normal)
        },
        move |_, hitbox, window, _| {
            let view = window.current_view();
            let events = events.clone();
            window.on_mouse_event(move |event: &gpui_kit::ScrollEvent, phase, window, cx| {
                if phase != gpui_kit::DispatchPhase::Capture || !hitbox.should_handle_scroll(window)
                {
                    return;
                }
                let delta = diffui_widgets::wheel::points(event.delta).y;
                diffui_widgets::frame_log::value("wy", f64::from(delta));
                events.borrow_mut().push(Instant::now(), -f64::from(delta));
                cx.notify(view);
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// How far past each edge of the op log rows are built. A row is built in the
/// frame it scrolls into view either way; past that, every row built is paid for
/// again on each scroll tick, so this stays near one row.
const OP_LOG_OVERDRAW: f32 = 48.0;

/// The op log's list, and what each of its rows showed when it was measured.
#[derive(Debug)]
struct OpLogList {
    state: gpui_kit::ListState,
    keys: std::cell::RefCell<Vec<OpLogKey>>,
    wheel: Rc<RefCell<Resampler>>,
    /// Off its top, as the panes last drew it: see [`note_scrolled`].
    scrolled: Rc<Cell<bool>>,
}

impl Default for OpLogList {
    fn default() -> Self {
        Self {
            // Every row measured once, so the scrollbar is right from the start
            // instead of shrinking as rows scroll in; a scroll tick still builds
            // only the rows on screen.
            state: gpui_kit::ListState::new(0, gpui_kit::ListAlignment::Top, px(OP_LOG_OVERDRAW))
                .measure_all(),
            keys: Default::default(),
            wheel: Default::default(),
            scrolled: Default::default(),
        }
    }
}

/// What decides an op log row's height: which operation (or run) it is, whether a
/// run is open, and for the footer, what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpLogKey {
    Op(String),
    Run {
        first: String,
        count: usize,
        open: bool,
    },
    Loading,
    LoadOlder,
}

impl OpLogList {
    /// Tells the list which rows changed. Only those are measured again, so an
    /// opened run or a new operation keeps the scroll position.
    ///
    /// The list keeps the row it's scrolled to in view as rows arrive above it.
    /// At the top, that would carry the view down past every newer operation:
    /// the log's first batch lands above the "Reading" row the list started on.
    /// A log read at its top stays there.
    fn sync(&self, keys: Vec<OpLogKey>) {
        let mut old = self.keys.borrow_mut();
        if keys == *old {
            return;
        }
        let at_top = self.state.logical_scroll_top() == gpui_kit::ListOffset::default();
        let prefix = old
            .iter()
            .zip(&keys)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(keys[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let inserted = keys.len() - prefix - suffix;
        self.state.splice(prefix..old.len() - suffix, inserted);
        // GPUI's splice leaves a measure-all list believing it's measured
        // everything, so the new rows would only be measured as they scroll
        // in and the scrollbar would shrink as they did. Asking for them again
        // has the next layout measure just those.
        if inserted > 0 {
            self.state.remeasure_items(prefix..prefix + inserted);
        }
        if at_top {
            self.state.scroll_to(gpui_kit::ListOffset::default());
        }
        *old = keys;
    }
}

fn op_log_keys(ui: &Diffui) -> Vec<OpLogKey> {
    let history = &ui.active().history;
    let mut keys: Vec<OpLogKey> = op_rows(history)
        .iter()
        .map(|row| match row {
            OpRow::Op(op) => OpLogKey::Op(op.id.clone()),
            OpRow::Run {
                first,
                count,
                children,
                ..
            } => OpLogKey::Run {
                first: first.id.clone(),
                count: *count,
                open: children.is_some(),
            },
        })
        .collect();
    if history.ops_job.is_some() {
        keys.push(OpLogKey::Loading);
    } else if history.ops_truncated {
        keys.push(OpLogKey::LoadOlder);
    }
    keys
}

/// Below the last operation: that more is on its way, or a button for it.
fn op_log_footer(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> AnyElement {
    let history = &ui.active().history;
    if history.ops_job.is_some() {
        return note("Reading the operation log\u{2026}", theme.subtle_text, ui).into_any_element();
    }
    if !history.ops_truncated {
        return div().into_any_element();
    }
    div()
        .w_full()
        .flex()
        .justify_center()
        .p(px(space::SM))
        .child(
            ui::text_button(
                "history-load-older",
                None,
                "Load older operations",
                ui::Variant::Ghost,
                ui::Size::Compact,
                ui.config.ui_font,
                theme,
                cx,
            )
            .on_click(Diffui::on_action(
                Action::History(HistoryAction::LoadMoreOperations),
                cx,
            )),
        )
        .into_any_element()
}

/// A commit's change id, commit id, its states and its first line.
fn commit_line(
    ui: &Diffui,
    theme: ThemeSpec,
    commit: &CommitBrief,
    window: &mut Window,
    cx: &mut Cx,
) -> Vec<Aligned> {
    let mut line = vec![
        mono_on_line(commit.short_change_id().to_owned(), theme.change_id, ui),
        mono_on_line(commit.short_commit_id().to_owned(), theme.subtle_text, ui),
    ];
    if !commit.visible {
        line.push(meta_line::badge(
            "hidden",
            theme.subtle_text,
            ui,
            window,
            cx,
        ));
    }
    if commit.empty {
        line.push(meta_line::badge("empty", theme.subtle_text, ui, window, cx));
    }
    line.push(fill_on_line(
        summary_line(commit, text_size::UI, ui.config.ui_font, theme),
        ui.config.ui_font,
        text_size::UI,
    ));
    line
}

/// One changed revision, a row of its table. `(first, last)`: where it sits
/// in the table, whose rounded corners its selection wash follows.
#[allow(clippy::too_many_arguments)]
fn change_row(
    ui: &Diffui,
    theme: ThemeSpec,
    index: usize,
    change: &ChangedCommit,
    (first, last): (bool, bool),
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let selected = ui.active().history.selected_change == Some(index);
    let (glyph, color, verb) = match change.kind {
        CommitChangeKind::Created => ("+", theme.added_text, "created"),
        CommitChangeKind::Rewritten if change.rebased => ("~", theme.subtle_text, "rebased"),
        CommitChangeKind::Rewritten => ("~", theme.modified_token, "rewritten"),
        CommitChangeKind::Abandoned => ("\u{2212}", theme.removed_text, "abandoned"),
    };
    // A hidden commit has nothing to open, but keeps the slot so every
    // row's verb ends at the same edge.
    let open: AnyElement = if change.commit.visible {
        let id = SharedString::from(format!("change-open-{index}"));
        div()
            .id(SharedString::from(format!("change-open-tip-{index}")))
            .flex_none()
            .tip("Show in the diff view", ui.config.ui_font, theme)
            // The row is a button too: this one mustn't also select it.
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                ui::icon_button(
                    id,
                    icons::FILE_DIFF,
                    theme.muted_text,
                    ui::Size::Compact,
                    theme,
                    cx,
                )
                .on_click(Diffui::on_action(
                    Action::History(HistoryAction::OpenRevision(change.commit.commit_id.clone())),
                    cx,
                )),
            )
            .into_any_element()
    } else {
        div()
            .flex_none()
            .w(px(ui::Size::Compact.height()))
            .into_any_element()
    };
    let mut line: Vec<Aligned> = vec![mono_on_line(glyph, color, ui).map(|glyph| {
        div()
            .flex_none()
            .w(px(12.0))
            .child(glyph)
            .into_any_element()
    })];
    line.extend(commit_line(ui, theme, &change.commit, window, cx));
    line.push(caption_on_line(caption(verb, theme.subtle_text, ui), ui));
    let inner = px(radius::MD - 1.0);
    let mut card = row_card(
        SharedString::from(format!("change-{index}")),
        CHANGE_ROW_HEIGHT,
        selected,
        Action::History(HistoryAction::SelectChange(index)),
        theme,
        cx,
    )
    .rounded(px(0.0));
    if first {
        card = card.rounded_t(inner);
    }
    if last {
        card = card.rounded_b(inner);
    }
    card.px(px(space::LG))
        .gap(px(space::SM))
        .child(typography::baseline_row(line, space::SM).flex_1().min_w_0())
        .child(open)
        .into_any_element()
}

/// A moved ref, laid out like a changed commit's row: a kind marker in the
/// glyph column, the ref, where it moved from and to, and the kind where a
/// commit row has its verb.
fn ref_row(ui: &Diffui, theme: ThemeSpec, change: &RefChange) -> AnyElement {
    let glyph =
        |glyph: &'static str| Aligned::glyph(icons::icon(glyph, 12.0, theme.subtle_text), 12.0);
    let (marker, kind): (Aligned, &str) = match change.kind {
        RefKind::WorkingCopy => (mono_on_line("@", theme.subtle_text, ui), "working copy"),
        RefKind::Bookmark => (glyph(icons::GIT_BRANCH), "bookmark"),
        RefKind::RemoteBookmark => (glyph(icons::GIT_BRANCH), "remote bookmark"),
        RefKind::Tag => (glyph(icons::TAG), "tag"),
    };
    let side = |commits: &[CommitBrief], color: Color| -> Aligned {
        if commits.is_empty() {
            caption_on_line(caption("(absent)", theme.subtle_text, ui), ui)
        } else {
            mono_on_line(ref_side_label(commits), color, ui)
        }
    };
    // Mono names and ids, sans captions and the glyphs on one line (see
    // `crate::typography`).
    div()
        .w_full()
        .flex_none()
        .h(px(CHANGE_ROW_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SM))
        .px(px(space::LG))
        .child(typography::baseline_row(
            [
                marker.map(|marker| {
                    div()
                        .flex_none()
                        .w(px(12.0))
                        .child(marker)
                        .into_any_element()
                }),
                mono_on_line(change.name.clone(), theme.text, ui),
                side(&change.from, theme.removed_text),
                glyph(icons::ARROW_RIGHT),
                side(&change.to, theme.added_text),
            ],
            space::SM,
        ))
        .child(div().flex_1())
        .child(typography::baseline_row(
            [caption_on_line(caption(kind, theme.subtle_text, ui), ui)],
            0.0,
        ))
        .child(div().flex_none().w(px(ui::Size::Compact.height())))
        .into_any_element()
}

/// A section's label, sentence case and semibold, with its count in subtle
/// text after it: "Changed revisions 1".
fn section_title(label: &str, count: String, ui: &Diffui, theme: ThemeSpec) -> Div {
    let font = emphasis_font(ui.config.ui_font, Weight::SEMIBOLD);
    let plain = ui.config.ui_font;
    div()
        .flex_none()
        .pt(px(space::LG))
        .pb(px(space::MD))
        .child(typography::baseline_row(
            [
                Aligned::text(
                    typography::line(label.to_owned(), font, SECTION_LABEL_SIZE, theme.text),
                    font,
                    SECTION_LABEL_SIZE,
                ),
                Aligned::text(
                    typography::line(count, plain, SECTION_LABEL_SIZE, theme.subtle_text),
                    plain,
                    SECTION_LABEL_SIZE,
                ),
            ],
            space::SM,
        ))
}

/// Rows in a rounded box, hairlines between them.
fn table(rows: Vec<AnyElement>, theme: ThemeSpec) -> Div {
    let mut table = div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .rounded(px(radius::MD))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            table = table.child(div().w_full().h(px(1.0)).flex_none().bg(theme.border));
        }
        table = table.child(row);
    }
    table
}

/// The operation's header: description, metadata, and the two actions.
fn operation_header(
    ui: &Diffui,
    theme: ThemeSpec,
    op: &OperationEntry,
    changes: Option<&OperationChanges>,
    window: &mut Window,
    cx: &mut Cx,
) -> Div {
    let mut badges = Vec::new();
    if op.is_current {
        badges.push(meta_line::Badge {
            label: "current",
            color: theme.accent,
        });
    }
    if op.is_snapshot {
        badges.push(meta_line::Badge {
            label: "snapshot",
            color: theme.subtle_text,
        });
    }
    let meta = meta_line::view(
        meta_line::Meta {
            ids: vec![meta_line::Id::plain(
                op.short_id(),
                if op.is_current {
                    theme.accent
                } else {
                    theme.muted_text
                },
            )],
            badges,
            who: Some((op.user.clone(), None)),
            when: Some((Some(op.time_millis), op.time.clone())),
            operation: None,
        },
        ui,
        theme,
        window,
        cx,
    );

    let can_mutate = ui.active().session.capabilities.mutate;
    let has_parent = !op.parent_ids.is_empty();
    let is_merge = changes.is_some_and(|changes| changes.is_merge) || op.parent_ids.len() > 1;
    let action_button = |id: &'static str,
                         glyph: &'static str,
                         label: &'static str,
                         action: Option<HistoryAction>,
                         cx: &mut Cx| {
        let mut button = ui::text_button(
            id,
            Some((glyph, theme.muted_text)),
            label,
            ui::Variant::Raised,
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
            cx,
        )
        .disabled(action.is_none());
        if let Some(action) = action {
            button = button.on_click(Diffui::on_action(Action::History(action), cx));
        }
        button
    };
    let restore = action_button(
        "op-restore",
        icons::ROTATE_CCW,
        "Restore to here",
        (can_mutate && !op.is_current && has_parent)
            .then(|| HistoryAction::RestoreOperation(op.id.clone())),
        cx,
    );
    let revert = action_button(
        "op-revert",
        icons::UNDO,
        "Revert",
        (can_mutate && has_parent && !is_merge)
            .then(|| HistoryAction::RevertOperation(op.id.clone())),
        cx,
    );

    let title_font = emphasis_font(ui.config.ui_font, Weight::SEMIBOLD);
    let mut block = div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(space::MD))
        .px(px(space::XXL))
        .pt(px(space::XL))
        .pb(px(space::XS))
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SM))
                // Centred on its caps against the buttons beside it.
                .child(div().flex_1().min_w_0().child(typography::centered(
                    meta_line::description(
                        op.summary(),
                        DETAIL_TITLE_SIZE,
                        title_font,
                        theme.text,
                        ui,
                        theme,
                    )
                    .wrapped(),
                    title_font,
                    DETAIL_TITLE_SIZE,
                )))
                .child(with_tooltip(
                    "op-restore-tip",
                    restore,
                    "Put every revision and bookmark back to how this operation left them (jj op restore)",
                    ui,
                    theme,
                ))
                .child(with_tooltip(
                    "op-revert-tip",
                    revert,
                    "Undo just this operation, keeping later work (jj op revert)",
                    ui,
                    theme,
                )),
        )
        .child(meta);
    if let Some(args) = &op.args {
        // The command in a well of its own, copyable.
        block = block.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .min_h(px(34.0))
                .py(px(space::XS))
                .pl(px(space::LG))
                .pr(px(space::SM))
                .rounded(px(radius::MD))
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .child(crate::plan::wrapped_command_line(
                    ui,
                    theme,
                    args,
                    text_size::UI,
                    window,
                    cx,
                )),
        );
    }
    block
}

fn operation_changes(
    ui: &Diffui,
    theme: ThemeSpec,
    changes: &OperationChanges,
    window: &mut Window,
    cx: &mut Cx,
) -> Div {
    let mut list = div()
        .w_full()
        .flex()
        .flex_col()
        .px(px(space::XXL))
        .pb(px(space::LG));
    if changes.is_merge {
        list = list.child(note(
            "A merge of concurrent operations. Like the jj CLI, it lists no changes.",
            theme.subtle_text,
            ui,
        ));
    }
    if !changes.commits.is_empty() {
        let mut count = changes.commits.len().to_string();
        if changes.commits_truncated {
            count.push_str(", more not listed");
        }
        list = list.child(section_title("Changed revisions", count, ui, theme));
        let last = changes.commits.len() - 1;
        let rows = changes
            .commits
            .iter()
            .enumerate()
            .map(|(index, change)| {
                change_row(
                    ui,
                    theme,
                    index,
                    change,
                    (index == 0, index == last),
                    window,
                    cx,
                )
            })
            .collect();
        list = list.child(table(rows, theme));
    }
    if !changes.refs.is_empty() {
        list = list.child(section_title(
            "Moved refs",
            changes.refs.len().to_string(),
            ui,
            theme,
        ));
        let rows = changes
            .refs
            .iter()
            .map(|change| ref_row(ui, theme, change))
            .collect();
        list = list.child(table(rows, theme));
    }
    if changes.commits.is_empty() && changes.refs.is_empty() && !changes.is_merge {
        list = list.child(note(
            "This operation changed no commits or refs.",
            theme.subtle_text,
            ui,
        ));
    }
    // As tall as its rows, up to a limit: the diff below keeps the rest.
    div()
        .w_full()
        .flex_none()
        .max_h(px(OP_CHANGES_MAX_HEIGHT))
        .child(ui::scrollable("op-changes", list))
}

fn evolog_header(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> Option<Div> {
    let history = &ui.active().history;
    let entry = history.evolog.get(history.selected_version?)?;
    let has_previous = !entry.predecessor_ids.is_empty();
    let meta = meta_line::view(
        meta_line::Meta {
            who: Some((entry.commit.author.clone(), None)),
            when: entry
                .operation
                .as_ref()
                .map(|op| (Some(op.time_millis), String::new())),
            operation: entry
                .operation
                .as_ref()
                .map(|op| (op.short_id().to_owned(), op.summary.clone())),
            ..meta_line::Meta::default()
        },
        ui,
        theme,
        window,
        cx,
    );
    // The first version has nothing before it, so it can only show its
    // full diff (which is what it loads).
    let shown = if has_previous {
        history.evolog_diff
    } else {
        EvologDiff::Parents
    };
    let segment = |icon, label, mode: EvologDiff, enabled: bool| crate::toolbar::Segment {
        icon: Some(icon),
        label,
        active: shown == mode,
        on_press: enabled.then_some(Action::History(HistoryAction::SetEvologDiff(mode))),
    };
    let mut actions = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::XS))
        .child(crate::toolbar::segmented(
            vec![
                segment(
                    icons::GIT_COMPARE,
                    "Since previous version",
                    EvologDiff::Previous,
                    has_previous,
                ),
                segment(icons::FILE_DIFF, "Full diff", EvologDiff::Parents, true),
            ],
            ui.config.ui_font,
            theme,
            window,
            cx,
        ));
    if entry.commit.visible {
        actions = actions.child(div().flex_1()).child(
            ui::text_button(
                "evolog-open",
                Some((icons::FILE_DIFF, theme.muted_text)),
                "Show in diff view",
                ui::Variant::Ghost,
                ui::Size::Compact,
                ui.config.ui_font,
                theme,
                cx,
            )
            .on_click(Diffui::on_action(
                Action::History(HistoryAction::OpenRevision(entry.commit.commit_id.clone())),
                cx,
            )),
        );
    }
    let title_font = emphasis_font(ui.config.ui_font, Weight::MEDIUM);
    Some(
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::XS))
            .px(px(space::LG))
            .py(px(space::MD))
            .child(typography::baseline_row(
                [
                    mono_on_line(
                        entry.commit.short_commit_id().to_owned(),
                        theme.muted_text,
                        ui,
                    ),
                    fill_on_line(
                        summary_line(&entry.commit, text_size::BODY_LG, title_font, theme)
                            .text_color(theme.text),
                        title_font,
                        text_size::BODY_LG,
                    ),
                ],
                space::SM,
            ))
            .child(meta)
            .child(actions),
    )
}

/// A message filling the diff's slot.
fn diff_message(message: String, padded: bool, ui: &Diffui, theme: ThemeSpec) -> AnyElement {
    let mut slot = div()
        .w_full()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .font(ui.config.ui_font.gpui())
        .text_size(px(text_size::BODY_LG))
        .text_color(theme.subtle_text)
        .child(message);
    if padded {
        slot = slot.p(px(space::XXL));
    }
    slot.into_any_element()
}

fn history_diff_view(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
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
        return diff_message(message.to_owned(), false, ui, theme);
    };
    if diff.document.files.is_empty() {
        let message = match description_change(history) {
            Some((old, new)) => format!(
                "No file changes. The description changed from \u{201c}{old}\u{201d} to \u{201c}{new}\u{201d}"
            ),
            None => "No file changes".to_owned(),
        };
        return diff_message(message, true, ui, theme);
    }
    history_diff(ui, theme, window, cx)
}

/// The selected change's diff.
fn history_diff(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let history = &ui.active().history;
    let Some(diff) = &history.diff else {
        return div().into_any_element();
    };
    let doc = DocSource::new(cx.entity(), |ui: &Diffui, file| {
        ui.active()
            .history
            .diff
            .as_ref()
            .and_then(|diff| diff.document.files.get(file))
            .map_or(&[][..], |file| &file.hunks)
    });
    let mut props = DiffProps::new(
        crate::diff_panel::file_views(&diff.document.files, theme),
        doc,
        history.diff_file,
        format!("history:{}", diff.doc_id),
        diff_palette(theme),
        ui.config.mono_font,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .on_selected_file_changed(Diffui::listen(cx, |this, index: &usize, _, cx| {
        this.perform(Action::History(HistoryAction::DiffFileChanged(*index)), cx);
    }))
    .on_copy(Diffui::listen(cx, |this, text: &String, _, cx| {
        this.perform(Action::Copy(text.clone()), cx);
    }))
    // Every scroll tick: stored, with no reconcile and no redraw.
    .on_scroll(cx.listener(|this: &mut Diffui, offset: &f32, _, _| {
        this.active_mut().history.diff_scroll = *offset;
    }))
    .restore_scroll(history.diff_scroll, ui.scroll_restore_token)
    .content_version(ui.document_version)
    .layout_version(diff.doc_id)
    .wrap(ui.diff_wrap)
    .side_by_side(ui.diff_split);
    if let Some((media, key)) = crate::media::document_media(ui, &diff.document) {
        props = props.media(media, key);
    }
    if let Some(details) = &diff.details {
        props = props
            .with_header(crate::commit_info::header_lines(
                ui,
                details,
                theme,
                false,
                // The History view's right pane: the window less its list.
                ui.window_size.width - ui.sidebar_width - 1.0,
            ))
            .on_header_action(Diffui::listen(
                cx,
                |this, action: &crate::diff_view::HeaderAction, _, cx| {
                    this.perform(crate::commit_info::header_action(action.clone()), cx);
                },
            ));
    }
    div()
        .w_full()
        .flex_1()
        .min_h_0()
        .child(crate::diff_view::view("history-diff", props, window, cx))
        .into_any_element()
}

/// The History view's right pane.
pub(crate) fn build_history_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let history = &ui.active().history;
    let divider = || div().w_full().flex_none().h(px(1.0)).bg(theme.border);
    let mut body = div()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.panel_background);
    if let Some(error) = &history.error {
        body = body.child(note(error.clone(), theme.removed_text, ui));
    }
    match &history.kind {
        HistoryKind::OpLog => match history.selected_operation() {
            Some(op) => {
                let changes = history
                    .op_changes
                    .as_ref()
                    .filter(|changes| changes.operation.id == op.id);
                body = body.child(operation_header(ui, theme, op, changes, window, cx));
                match changes {
                    Some(changes) => {
                        body = body.child(operation_changes(ui, theme, changes, window, cx));
                    }
                    None if history.op_job.is_some() => {
                        body = body.child(note(
                            "Reading the operation\u{2026}",
                            theme.subtle_text,
                            ui,
                        ));
                    }
                    None => {}
                }
                body = body.child(divider());
                body = body.child(history_diff_view(ui, theme, window, cx));
            }
            None => {
                let message = if history.ops_job.is_some() {
                    ""
                } else {
                    "Select an operation"
                };
                body = body.child(diff_message(message.to_owned(), false, ui, theme));
            }
        },
        HistoryKind::Evolog { .. } => {
            if let Some(header) = evolog_header(ui, theme, window, cx) {
                body = body.child(header);
                body = body.child(divider());
            }
            body = body.child(history_diff_view(ui, theme, window, cx));
        }
    }
    body.into_any_element()
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

    /// A batch lands above the "Reading" row while the log streams in: a list
    /// at its top stays there, on the newest operation. One scrolled down keeps
    /// its rows in view.
    #[test]
    fn the_op_log_stays_at_its_top_as_batches_land() {
        use gpui_kit::{ListOffset, px};
        let key = |id: &str| OpLogKey::Op(id.to_owned());
        let list = OpLogList::default();
        list.sync(vec![OpLogKey::Loading]);
        // Where layout anchors a list shorter than its viewport.
        list.state.scroll_to(ListOffset::default());
        list.sync(vec![key("a"), key("b"), OpLogKey::Loading]);
        assert_eq!(list.state.logical_scroll_top(), ListOffset::default());
        list.sync(vec![key("a"), key("b"), key("c"), OpLogKey::Loading]);
        assert_eq!(list.state.logical_scroll_top(), ListOffset::default());

        let scrolled = ListOffset {
            item_ix: 1,
            offset_in_item: px(4.0),
        };
        list.state.scroll_to(scrolled);
        list.sync(vec![
            key("new"),
            key("a"),
            key("b"),
            key("c"),
            OpLogKey::Loading,
        ]);
        assert_eq!(
            list.state.logical_scroll_top(),
            ListOffset {
                item_ix: 2,
                ..scrolled
            },
            "the same row, now one further down"
        );
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

#[cfg(test)]
mod scenes {
    use diffui_core::RevisionSelection;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{div, point, px};

    use super::*;
    use crate::harness::{Harness, fixture_repo};
    use crate::theme::ThemePreference;

    const LANE: &str = "history";
    const SIZE: (f32, f32) = (1440.0, 900.0);

    fn history_view() -> Harness {
        let mut h = Harness::open_sized(Some(&fixture_repo()), SIZE);
        h.wait_for_graph();
        h.perform(Action::History(HistoryAction::ShowOpLog));
        h.wait_until("the op log", |ui| {
            let history = &ui.active().history;
            history.ops_job.is_none() && !history.ops.is_empty()
        });
        h.settle();
        h
    }

    #[test]
    fn the_operation_log() {
        let mut h = history_view();
        h.shot(LANE, "history-oplog");
        h.shot(LANE, "view-switcher-main-view");
        h.perform(Action::SetTheme(ThemePreference::Dark));
        h.shot(LANE, "history-dark");
    }

    #[test]
    fn selecting_an_older_operation_by_clicking_its_row() {
        let mut h = history_view();
        // A rewrite that lists changed commits and refs, from the list itself.
        let (id, row) = h.read(|ui| {
            let history = &ui.active().history;
            let rows = op_rows(history);
            rows.iter()
                .enumerate()
                .find_map(|(row, entry)| match entry {
                    OpRow::Op(op)
                        if !op.is_current && !op.is_snapshot && op.summary().contains("rebase") =>
                    {
                        Some((op.id.clone(), row))
                    }
                    _ => None,
                })
                .or_else(|| {
                    rows.iter()
                        .enumerate()
                        .find_map(|(row, entry)| match entry {
                            OpRow::Op(op) if !op.is_current && !op.is_snapshot => {
                                Some((op.id.clone(), row))
                            }
                            _ => None,
                        })
                })
                .expect("an older operation")
        });
        let before = h.read(|ui| {
            op_rows(&ui.active().history)
                .iter()
                .take(row)
                .map(|entry| match entry {
                    OpRow::Op(op) if !op.is_snapshot => REVISION_ROW_HEIGHT,
                    _ => FILE_ROW_HEIGHT,
                })
                .sum::<f32>()
        });
        // Under the chrome and the list's header, if the row is in view; a long
        // list goes through the action.
        let top = crate::toolbar::chrome_bottom() + crate::field::PANEL_HEADER_HEIGHT;
        let y = top + before + row as f32 * 2.0 * ROW_CARD_INSET_Y + REVISION_ROW_HEIGHT / 2.0;
        if y < SIZE.1 - 20.0 {
            h.click_point(point(px(150.0), px(y)));
        } else {
            h.perform(select_op(&id));
        }
        h.wait_until("the operation", |ui| {
            let history = &ui.active().history;
            history.selected_op.as_deref() == Some(id.as_str()) && history.op_job.is_none()
        });
        h.settle();
        h.shot(LANE, "history-op-selected");

        // The row's own button opens the commit rather than selecting the row.
        let visible = h.read(|ui| {
            let changes = ui.active().history.op_changes.as_ref();
            changes.is_some_and(|changes| changes.commits.first().is_some_and(|c| c.commit.visible))
        });
        if visible {
            h.click("change-open-0");
            h.settle();
            assert_eq!(h.read(|ui| ui.active().main_view), MainView::Diff);
        }
    }

    #[test]
    fn a_revisions_evolution() {
        let mut h = Harness::open_sized(Some(&fixture_repo()), SIZE);
        h.wait_for_graph();
        h.perform(Action::SetRevset("all()".to_owned()));
        h.wait_for_graph();
        let id = h.read(|ui| {
            ui.active()
                .session
                .commits
                .iter()
                .find(|row| row.description().contains("retry with backoff"))
                .expect("the rewritten revision")
                .commit_id()
                .to_owned()
        });
        h.perform(Action::History(HistoryAction::ShowEvolog(
            RevisionSelection::Commit(id),
        )));
        h.wait_until("the evolog", |ui| {
            let history = &ui.active().history;
            history.evolog_job.is_none() && !history.evolog.is_empty()
        });
        h.settle();
        h.shot(LANE, "history-evolog");
        let older = h.read(|ui| ui.active().history.evolog.len() - 1);
        h.perform(Action::History(HistoryAction::SelectVersion(older)));
        h.settle();
        assert_eq!(
            h.read(|ui| ui.active().history.selected_version),
            Some(older)
        );
        h.shot(LANE, "history-evolog-selected");
    }

    #[test]
    fn a_files_history_narrows_the_list_under_a_banner() {
        let mut h = Harness::open_sized(Some(&fixture_repo()), SIZE);
        h.wait_for_graph();
        crate::revision_rows::scenes::show_list(&mut h);
        h.show(|ui, theme, window, cx| {
            let banner = file_filter_banner(ui, theme, window, cx);
            let list = crate::revision_rows::build_revision_list(ui, theme, window, cx);
            div()
                .w(px(ui.sidebar_width))
                .h_full()
                .pt(px(80.0))
                .flex()
                .flex_col()
                .bg(theme.panel_background)
                .children(banner)
                .child(div().flex_1().min_h_0().child(list))
                .into_any_element()
        });
        assert!(
            h.read(|ui| !file_filter_active(ui)),
            "no banner before there's a filter"
        );
        let revision = h.read(|ui| {
            let commit = ui
                .active()
                .session
                .commits
                .iter()
                .find(|row| row.description().contains("rework the pipeline"))
                .expect("the revision");
            RevisionSelection::Commit(commit.commit_id().to_owned())
        });
        h.perform(Action::SelectRevision(revision.clone()));
        h.settle();
        h.perform(Action::History(HistoryAction::FileHistory {
            revision,
            path: "src/main.rs".to_owned(),
        }));
        h.wait_for_graph();
        assert!(h.read(file_filter_active));
        h.shot(LANE, "file-history");
    }

    /// Every row of a page is measured before it scrolls in, so the scrollbar
    /// knows the whole log from the start instead of shrinking as rows arrive.
    /// Every row of a page is measured before it scrolls in, so the scrollbar
    /// knows the whole log from the start instead of shrinking as rows arrive.
    #[test]
    fn the_op_log_knows_its_whole_height_before_it_scrolls() {
        let mut h = history_view();
        h.act(|ui, _| {
            let history = &mut ui.active_mut().history;
            let template = history
                .ops
                .iter()
                .find(|op| !op.is_snapshot && !op.is_current)
                .expect("an operation")
                .clone();
            history.ops = (0..OP_LOG_PAGE)
                .map(|index| OperationEntry {
                    id: format!("{index:0128x}"),
                    is_current: false,
                    ..template.clone()
                })
                .collect();
        });
        h.settle();
        let range = |h: &mut Harness| {
            h.read(|ui| {
                f32::from(
                    ui.active()
                        .history
                        .op_list
                        .state
                        .max_offset_for_scrollbar()
                        .y,
                )
            })
        };
        let before = range(&mut h);
        assert!(before > 0.0, "a page of operations overflows the pane");
        for _ in 0..3 {
            h.act(|ui, _| ui.active().history.op_list.state.scroll_by(px(800.0)));
            h.settle();
        }
        assert_eq!(range(&mut h), before);
    }

    /// What a frame of the op log costs: it isn't virtualized, as it wasn't
    /// before the port. See `frame_timing_over_a_large_graph` for how to run it.
    #[test]
    #[ignore = "a benchmark: run on demand, optimized"]
    fn frame_timing_of_a_full_op_log_page() {
        let mut h = history_view();
        h.act(|ui, _| {
            let history = &mut ui.active_mut().history;
            let template = history
                .ops
                .iter()
                .find(|op| !op.is_snapshot && !op.is_current)
                .expect("an operation")
                .clone();
            history.ops = (0..OP_LOG_PAGE)
                .map(|index| OperationEntry {
                    id: format!("{index:0128x}"),
                    is_current: false,
                    ..template.clone()
                })
                .collect();
        });
        h.settle();
        let mut samples: Vec<_> = (0..100)
            .map(|_| {
                h.timed(|window, cx| {
                    window.refresh();
                    window.render_frame(cx);
                })
            })
            .collect();
        samples.sort();
        println!(
            "op log, {OP_LOG_PAGE} rows, full redraw: p50={:.2?} p99={:.2?}",
            samples[50], samples[98]
        );
    }
}

#[cfg(test)]
impl HistoryState {
    /// The `nth` operation's id, for scenes that pick one.
    pub(crate) fn operation_id(&self, nth: usize) -> Option<String> {
        self.ops.get(nth).map(|op| op.id.clone())
    }
}
