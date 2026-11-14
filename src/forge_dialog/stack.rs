//! Submit stack: the pull requests the bookmarks in `trunk()..rev` would
//! become, previewed before anything leaves the machine.
//!
//! Opening it only reads: the stack (a revset), each bookmarked commit's
//! description, each range's commit count and line totals and whether a push
//! would rewrite the remote's history (the jj CLI, read-only), and which pull
//! requests the forge already has (one list call). The user reads the plan,
//! edits each pull request's title and description (or has the AI provider
//! write them), and confirms; only then does the [`Submission`] push and
//! create or update, row by row, its progress shown here and on the activity
//! row. Closing the dialog doesn't stop a submission that has started.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use diffui_core::forge::{ForgeError, ForgeRemote, PullRequest};
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};
use diffui_core::{BookmarksInfo, RevisionSelection};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use super::{
    Head, STACK_LIMIT, Step, Submission, in_sync, revset_of, title_and_body, trunk_branch,
};
use crate::activity::ActivityId;
use crate::commands::{CommandArg, Context};
use crate::forge::bookmark_targets;
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::{self, text_size};
use crate::ui::{self, Look};
use crate::user_commands::Generation;
use crate::{Action, Cx, Diffui, Pending, TabId, icons, input};

pub(crate) const TITLE_INPUT_ID: &str = "stack-title";
pub(crate) const BODY_INPUT_ID: &str = "stack-body";

const WIDTH: f32 = 600.0;
/// The description field's height.
const BODY_HEIGHT: f32 = 120.0;
/// About how tall a collapsed row is, and what expanding one adds: for the
/// card's height, which then scrolls what doesn't fit.
const ROW_ESTIMATE: f32 = 92.0;
const EXPANDED_ESTIMATE: f32 = 270.0;

/// Tells one opening of the dialog from the next, so a late answer for a
/// closed one doesn't land in a new one.
static SERIAL: AtomicU64 = AtomicU64::new(1);

/// A user intent in the stack preview.
#[derive(Debug, Clone)]
pub(crate) enum StackAction {
    /// Open the row for editing, or close it.
    Expand(usize),
    /// The open row's title.
    Title(String),
    /// The open row's description.
    Body(String),
    Draft(usize, bool),
    /// Push and create or update.
    Confirm,
    /// Cancel before confirming; close after.
    Close,
    /// Read the stack again after a failure, keeping the unsent rows' text.
    Retry,
    /// Tab and ⇧Tab: the open row's title, its description, the next row's.
    NextField,
    PreviousField,
}

/// What pushing a row's bookmark does to the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Push {
    /// The remote has it as it is here: nothing to push.
    InSync,
    /// The remote doesn't have it yet.
    New,
    /// The remote has an older one, and gets the commits after it.
    Move,
    /// The remote has one that isn't an ancestor: its history is replaced.
    Force,
}

/// A row's commits over its base: how many, and the lines they add and
/// remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) commits: usize,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

/// Where a confirmed row is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Not confirmed yet.
    Idle,
    Waiting,
    Running,
    /// Done: what the forge did, shortly ("Opened #3").
    Done(String),
    /// Done, with nothing to change on the forge: only pushed, if that.
    Unchanged,
    /// Nothing to do, so not sent.
    Skipped,
    Failed(String),
    /// A row before it failed, so it wasn't sent.
    NotSent,
}

/// What a row will do, as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    Create,
    Update {
        /// Its base moves.
        base: bool,
        /// The push gives it new commits.
        commits: bool,
        /// Its title or description changes.
        text: bool,
    },
    UpToDate,
}

/// One pull request to be: a bookmarked commit of the stack.
#[derive(Debug, Clone)]
pub(crate) struct StackRow {
    pub(crate) bookmark: String,
    pub(crate) commit: String,
    /// The bookmark below, or trunk's.
    pub(crate) base: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) draft: bool,
    /// The open pull request it updates.
    pub(crate) existing: Option<PullRequest>,
    pub(crate) push: Push,
    pub(crate) stats: Option<Stats>,
    pub(crate) progress: Progress,
    /// An AI provider writing the title and body.
    pub(crate) generating: Option<Generation>,
    /// The commit's description as a title and body, once read.
    described: Option<(String, String)>,
}

impl StackRow {
    pub(crate) fn plan(&self) -> Plan {
        let Some(pr) = &self.existing else {
            return Plan::Create;
        };
        let base = pr.base != self.base;
        let commits = self.push != Push::InSync;
        let text = pr.title != self.title.trim() || !self.body.trim().is_empty();
        if base || commits || text {
            Plan::Update {
                base,
                commits,
                text,
            }
        } else {
            Plan::UpToDate
        }
    }

    fn writing(&self) -> bool {
        self.generating
            .as_ref()
            .is_some_and(|generation| generation.running)
    }
}

/// Which of the open row's fields has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Title,
    Body,
}

/// What the dialog still waits on before it can be confirmed.
#[derive(Debug, Clone, Copy, Default)]
struct Reading {
    stack: bool,
    details: usize,
    pull_requests: bool,
    stats: bool,
}

impl Reading {
    fn done(self) -> bool {
        !self.stack && self.details == 0 && !self.pull_requests && !self.stats
    }
}

/// The open stack preview.
#[derive(Debug)]
pub(crate) struct StackDialog {
    pub(crate) tab: TabId,
    serial: u64,
    pub(crate) remote_name: String,
    pub(crate) remote: ForgeRemote,
    /// The revision the stack ends at.
    revision: RevisionSelection,
    /// Bottom to top.
    pub(crate) rows: Vec<StackRow>,
    pub(crate) expanded: Option<usize>,
    /// The open row's field with the keyboard, as last drawn.
    focused: Cell<Option<Field>>,
    reading: Reading,
    /// Why it can't go ahead: the stack or the forge couldn't be read.
    pub(crate) problem: Option<String>,
    /// The submission's activity row, once confirmed.
    pub(crate) run: Option<ActivityId>,
    /// How the submission ended: its summary, or why it stopped.
    pub(crate) outcome: Option<Result<String, String>>,
    /// Text from the last try's unsent rows, by bookmark, for a retry.
    carried: HashMap<String, (String, String, bool)>,
}

impl StackDialog {
    pub(crate) fn ready(&self) -> bool {
        self.reading.done() && self.problem.is_none()
    }

    /// The rows Confirm would send.
    fn sending(&self) -> impl Iterator<Item = &StackRow> {
        self.rows.iter().filter(|row| row.plan() != Plan::UpToDate)
    }

    /// Why Confirm can't run now, if it can't.
    pub(crate) fn blocked(&self) -> Option<&'static str> {
        if self.run.is_some() {
            return Some("Already submitted");
        }
        if !self.ready() {
            return Some(if self.problem.is_some() {
                "The stack couldn't be read"
            } else {
                "Still reading the stack"
            });
        }
        if self.rows.iter().any(StackRow::writing) {
            return Some("Wait for the AI provider to finish");
        }
        if self.sending().any(|row| row.title.trim().is_empty()) {
            return Some("Every row needs a title");
        }
        if self.sending().next().is_none() {
            return Some("Everything is up to date");
        }
        None
    }

    fn editable(&self) -> bool {
        self.ready() && self.run.is_none()
    }

    /// The prefill, once everything is read: an open pull request's own
    /// title (its description isn't sent unless one is written), else the
    /// commit's; then what the last try kept.
    fn prefill(&mut self) {
        for row in &mut self.rows {
            match &row.existing {
                Some(pr) => {
                    row.title = pr.title.clone();
                    row.body = String::new();
                    row.draft = pr.state == diffui_core::forge::PrState::Draft;
                }
                None => {
                    if let Some((title, body)) = &row.described {
                        if !title.is_empty() {
                            row.title = title.clone();
                        }
                        row.body = body.clone();
                    }
                }
            }
            if let Some((title, body, draft)) = self.carried.remove(&row.bookmark) {
                row.title = title;
                row.body = body;
                row.draft = draft && row.existing.is_none();
            }
        }
    }

    /// The bookmarks Confirm pushes, and those of them it force-pushes.
    fn pushes(&self) -> (Vec<&str>, Vec<&str>) {
        let pushed: Vec<&StackRow> = self
            .sending()
            .filter(|row| row.push != Push::InSync)
            .collect();
        (
            pushed.iter().map(|row| row.bookmark.as_str()).collect(),
            pushed
                .iter()
                .filter(|row| row.push == Push::Force)
                .map(|row| row.bookmark.as_str())
                .collect(),
        )
    }
}

/// The title, body and generation an `ai.pr-description` run writes into:
/// the pull request dialog's, or a stack row's.
pub(crate) struct PrFields<'a> {
    pub(crate) title: &'a mut String,
    pub(crate) body: &'a mut String,
    pub(crate) generating: &'a mut Option<Generation>,
}

impl Diffui {
    /// Open the preview of the stack under the revision `arg` names, and
    /// start reading it. `carried`: text a retry keeps, by bookmark.
    pub(super) fn open_stack(
        &mut self,
        arg: CommandArg,
        carried: HashMap<String, (String, String, bool)>,
    ) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let state = self.active();
        if !state.session.capabilities.mutate {
            return;
        }
        let Some((remote_name, remote)) = state.forge.home.clone() else {
            return;
        };
        let revision = crate::forge::selection_of(&arg, self);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        self.push_mode(Mode::StackSubmit(StackDialog {
            tab,
            serial,
            remote_name,
            remote,
            revision: revision.clone(),
            rows: Vec::new(),
            expanded: None,
            focused: Cell::new(None),
            reading: Reading {
                stack: true,
                ..Reading::default()
            },
            problem: None,
            run: None,
            outcome: None,
            carried,
        }));
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let query = RevsetQuery {
            limit: STACK_LIMIT,
            ..RevsetQuery::new(format!("trunk()..({})", revset_of(&revision)))
        };
        let (job, effects) = state.session.resolve_revset(query);
        state
            .jobs
            .issue(job, Pending::Forge(super::ForgeQuery::Stack(serial)));
        self.run_effects(tab, effects);
    }

    /// The open stack preview opened as `serial` on `tab`.
    fn stack_reading(&mut self, tab: TabId, serial: u64) -> Option<&mut StackDialog> {
        self.stack_dialog_mut()
            .filter(|dialog| dialog.tab == tab && dialog.serial == serial)
    }

    /// The stack preview that `activity` submits.
    fn stack_running(&mut self, tab: TabId, activity: ActivityId) -> Option<&mut StackDialog> {
        self.stack_dialog_mut()
            .filter(|dialog| dialog.tab == tab && dialog.run == Some(activity))
    }

    /// The stack's commits are known: one row per bookmarked commit, oldest
    /// first, each based on the one before and the first on trunk. Then read
    /// their descriptions, the forge's pull requests and the ranges' totals.
    pub(super) fn stack_resolved(
        &mut self,
        tab: TabId,
        serial: u64,
        result: Result<RevsetMatch, RevsetError>,
        cx: &mut Cx,
    ) {
        let found = match result {
            Ok(found) => found,
            Err(error) => return self.stack_unreadable(tab, serial, error.to_string()),
        };
        let forges = self.forge.forges.clone();
        let Some(state) = self.tab(tab) else {
            return;
        };
        let Some(dialog) = self
            .stack_dialog()
            .filter(|dialog| dialog.tab == tab && dialog.serial == serial)
        else {
            return;
        };
        let remote_name = dialog.remote_name.clone();
        let remote = dialog.remote.clone();
        let rows = match stack_rows(&found.commits, state, &remote_name) {
            Ok(rows) => rows,
            Err(error) => {
                let error = error.to_owned();
                if let Some(dialog) = self.stack_reading(tab, serial) {
                    dialog.reading.stack = false;
                    dialog.problem = Some(error);
                }
                return;
            }
        };
        let info = &state.session.bookmarks;
        let ranges = ranges_of(&rows, info, &remote_name);
        let root = state
            .repository
            .as_ref()
            .filter(|repository| repository.vcs == crate::Vcs::Jj)
            .map(|repository| repository.root.display().to_string());
        let details = state.session.capabilities.details;
        let heads: Vec<String> = rows.iter().map(|row| row.bookmark.clone()).collect();
        let commits: Vec<String> = rows.iter().map(|row| row.commit.clone()).collect();
        let provider = forges.provider(remote.kind);
        let Some(dialog) = self.stack_reading(tab, serial) else {
            return;
        };
        dialog.rows = rows;
        dialog.reading = Reading {
            stack: false,
            details: if details { commits.len() } else { 0 },
            pull_requests: provider.is_some(),
            stats: root.is_some(),
        };
        if provider.is_none() {
            dialog.problem = Some(format!("{} isn't available.", remote.kind.name()));
        }

        if details && let Some(state) = self.tab_mut(tab) {
            for commit in commits {
                let job = state.session.next_job();
                state.jobs.issue(
                    job,
                    Pending::Forge(super::ForgeQuery::StackDetails(serial, commit.clone())),
                );
                if let Some(handle) = &state.handle {
                    handle.send(diffui_core::Command::RevisionDetails {
                        job,
                        revision: RevisionSelection::Commit(commit),
                    });
                }
            }
        }
        if let Some(provider) = provider {
            Self::run_async(
                async move { provider.pull_requests(remote, heads).await },
                cx,
                move |this, listed, _| this.stack_listed(tab, serial, listed),
            );
        }
        if let Some(root) = root {
            Self::run_async(read_stats(root, ranges), cx, move |this, stats, _| {
                this.stack_stats_read(tab, serial, stats)
            });
        }
        self.stack_maybe_ready(tab, serial);
    }

    /// The stack couldn't be read.
    pub(super) fn stack_unreadable(&mut self, tab: TabId, serial: u64, error: String) {
        if let Some(dialog) = self.stack_reading(tab, serial) {
            dialog.reading.stack = false;
            dialog.problem = Some(format!("Couldn't read the stack: {error}"));
        }
    }

    /// A stack commit's description arrived, or couldn't (`None`).
    pub(super) fn stack_description(
        &mut self,
        tab: TabId,
        serial: u64,
        commit: &str,
        description: Option<&str>,
    ) {
        let Some(dialog) = self.stack_reading(tab, serial) else {
            return;
        };
        if let Some(description) = description {
            let described = title_and_body(description);
            for row in dialog.rows.iter_mut().filter(|row| row.commit == commit) {
                row.described = Some(described.clone());
            }
        }
        dialog.reading.details = dialog.reading.details.saturating_sub(1);
        self.stack_maybe_ready(tab, serial);
    }

    fn stack_listed(
        &mut self,
        tab: TabId,
        serial: u64,
        listed: Result<Vec<PullRequest>, ForgeError>,
    ) {
        let Some(dialog) = self.stack_reading(tab, serial) else {
            return;
        };
        dialog.reading.pull_requests = false;
        match listed {
            Ok(prs) => {
                for row in &mut dialog.rows {
                    row.existing = prs
                        .iter()
                        .find(|pr| pr.head == row.bookmark && pr.state.is_open())
                        .cloned();
                }
            }
            Err(error) => {
                let forge = dialog.remote.kind.name();
                dialog.problem = Some(format!(
                    "Couldn't ask {forge} which pull requests exist: {error}"
                ));
            }
        }
        self.stack_maybe_ready(tab, serial);
    }

    fn stack_stats_read(&mut self, tab: TabId, serial: u64, stats: Vec<(Option<Stats>, bool)>) {
        let Some(dialog) = self.stack_reading(tab, serial) else {
            return;
        };
        dialog.reading.stats = false;
        for (row, (stats, force)) in dialog.rows.iter_mut().zip(stats) {
            row.stats = stats;
            if force && row.push == Push::Move {
                row.push = Push::Force;
            }
        }
        self.stack_maybe_ready(tab, serial);
    }

    /// Everything is read: fill the fields.
    fn stack_maybe_ready(&mut self, tab: TabId, serial: u64) {
        if let Some(dialog) = self.stack_reading(tab, serial)
            && dialog.reading.done()
        {
            dialog.prefill();
        }
    }

    pub(super) fn update_stack(&mut self, action: StackAction, cx: &mut Cx) {
        match action {
            StackAction::Expand(index) => {
                let Some(dialog) = self.stack_dialog_mut() else {
                    return;
                };
                if !dialog.editable() || index >= dialog.rows.len() {
                    return;
                }
                if dialog.expanded == Some(index) {
                    dialog.expanded = None;
                } else {
                    dialog.expanded = Some(index);
                    input::focus(TITLE_INPUT_ID, cx);
                }
            }
            StackAction::Title(value) => self.edit_stack_row(|row| row.title = value),
            StackAction::Body(value) => self.edit_stack_row(|row| row.body = value),
            StackAction::Draft(index, draft) => {
                if let Some(dialog) = self.stack_dialog_mut()
                    && dialog.editable()
                    && let Some(row) = dialog.rows.get_mut(index)
                    && row.existing.is_none()
                {
                    row.draft = draft;
                }
            }
            StackAction::Confirm => self.confirm_stack(),
            StackAction::Close => self.close_stack(),
            StackAction::Retry => self.retry_stack(),
            StackAction::NextField => self.move_stack_field(true, cx),
            StackAction::PreviousField => self.move_stack_field(false, cx),
        }
    }

    fn edit_stack_row(&mut self, edit: impl FnOnce(&mut StackRow)) {
        if let Some(dialog) = self.stack_dialog_mut()
            && dialog.editable()
            && let Some(index) = dialog.expanded
            && let Some(row) = dialog.rows.get_mut(index)
            && !row.writing()
        {
            edit(row);
        }
    }

    /// Tab walks the fields: the open row's title, its description, then the
    /// next row's title (opening that row), round to the first; ⇧Tab back.
    fn move_stack_field(&mut self, forward: bool, cx: &mut Cx) {
        let Some(dialog) = self.stack_dialog_mut() else {
            return;
        };
        if !dialog.editable() || dialog.rows.is_empty() {
            return;
        }
        let fields = dialog.rows.len() * 2;
        let at = match (dialog.expanded, dialog.focused.get()) {
            (Some(row), Some(Field::Title)) => Some(row * 2),
            (Some(row), Some(Field::Body)) => Some(row * 2 + 1),
            _ => None,
        };
        let next = match (at, forward) {
            (Some(at), true) => (at + 1) % fields,
            (Some(at), false) => (at + fields - 1) % fields,
            (None, true) => dialog.expanded.unwrap_or(0) * 2,
            (None, false) => dialog.expanded.map_or(fields - 1, |row| row * 2 + 1),
        };
        dialog.expanded = Some(next / 2);
        let (field, id) = if next % 2 == 0 {
            (Field::Title, TITLE_INPUT_ID)
        } else {
            (Field::Body, BODY_INPUT_ID)
        };
        dialog.focused.set(Some(field));
        input::focus(id, cx);
    }

    /// Push the rows that have something to do and create or update their
    /// pull requests, in stack order. The first time anything leaves the
    /// machine.
    fn confirm_stack(&mut self) {
        let Some(dialog) = self.stack_dialog() else {
            return;
        };
        if dialog.blocked().is_some() {
            return;
        }
        let tab = dialog.tab;
        let steps: Vec<Step> = dialog
            .sending()
            .map(|row| Step {
                head: Head::Bookmark(row.bookmark.clone()),
                base: row.base.clone(),
                title: row.title.trim().to_owned(),
                body: row.body.trim().to_owned(),
                draft: row.draft,
            })
            .collect();
        let existing: HashMap<String, PullRequest> = dialog
            .rows
            .iter()
            .filter_map(|row| row.existing.clone())
            .map(|pr| (pr.head.clone(), pr))
            .collect();
        let (remote_name, remote) = (dialog.remote_name.clone(), dialog.remote.clone());
        let (activity, _) = self.begin_activity(tab, "Submit stack", false);
        let mut submission = Submission::new(activity, remote_name, remote, steps);
        submission.existing = Some(existing);
        let Some(dialog) = self.stack_dialog_mut() else {
            return;
        };
        dialog.run = Some(activity);
        dialog.expanded = None;
        for row in &mut dialog.rows {
            row.progress = if row.plan() == Plan::UpToDate {
                Progress::Skipped
            } else {
                Progress::Waiting
            };
        }
        self.start_submission(tab, submission);
    }

    /// Close the dialog. Before Confirm that cancels it, and stops what the
    /// AI provider is writing; after, the submission goes on.
    fn close_stack(&mut self) {
        if let Some(Mode::StackSubmit(dialog)) = self.pop_mode(ModeKind::StackSubmit) {
            for generation in dialog.rows.iter().filter_map(|row| row.generating.as_ref()) {
                if generation.running {
                    generation.cancel.cancel();
                }
            }
        }
    }

    /// After a failure: read the stack again, which finds what the last try
    /// made, keeping the text of the rows it didn't send.
    fn retry_stack(&mut self) {
        let Some(dialog) = self.stack_dialog() else {
            return;
        };
        if !matches!(dialog.outcome, Some(Err(_))) {
            return;
        }
        let carried = dialog
            .rows
            .iter()
            .filter(|row| !matches!(row.progress, Progress::Done(_) | Progress::Unchanged))
            .map(|row| {
                (
                    row.bookmark.clone(),
                    (row.title.clone(), row.body.clone(), row.draft),
                )
            })
            .collect();
        let arg = CommandArg::Revision(dialog.revision.clone());
        self.pop_mode(ModeKind::StackSubmit);
        self.open_stack(arg, carried);
    }

    /// The row whose head is `head` in the stack preview `activity` submits
    /// is at `progress`.
    pub(super) fn stack_progress(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        head: &str,
        progress: Progress,
    ) {
        if let Some(dialog) = self.stack_running(tab, activity)
            && let Some(row) = dialog.rows.iter_mut().find(|row| row.bookmark == head)
        {
            row.progress = progress;
        }
    }

    /// The submission `activity` ended: its summary, or why it stopped. The
    /// row under way failed; the ones after it weren't sent.
    pub(super) fn stack_finished(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        outcome: Result<String, String>,
    ) {
        let Some(dialog) = self.stack_running(tab, activity) else {
            return;
        };
        if let Err(error) = &outcome {
            for row in &mut dialog.rows {
                row.progress = match row.progress {
                    Progress::Running => Progress::Failed(error.clone()),
                    Progress::Waiting => Progress::NotSent,
                    ref other => other.clone(),
                };
            }
        }
        dialog.outcome = Some(outcome);
    }

    /// The stack row `arg` names, for an `ai.pr-description` run to write
    /// into: the open preview on `tab` has a row on that commit.
    pub(crate) fn stack_row_for(&mut self, tab: TabId, arg: &CommandArg) -> Option<&mut StackRow> {
        let CommandArg::Revision(RevisionSelection::Commit(commit)) = arg else {
            return None;
        };
        let dialog = self.stack_dialog_mut().filter(|dialog| dialog.tab == tab)?;
        if !dialog.editable() {
            return None;
        }
        dialog.rows.iter_mut().find(|row| &row.commit == commit)
    }

    /// The fields `activity` writes into: the pull request dialog's, or a
    /// stack row's.
    pub(crate) fn generating_pr_fields(&mut self, activity: ActivityId) -> Option<PrFields<'_>> {
        let writes = |generating: &Option<Generation>| {
            generating
                .as_ref()
                .is_some_and(|generation| generation.activity == activity)
        };
        if self
            .pull_request_dialog()
            .is_some_and(|dialog| writes(&dialog.generating))
        {
            let Some(Mode::PullRequest(dialog)) = self.mode_mut(ModeKind::PullRequest) else {
                return None;
            };
            return Some(PrFields {
                title: &mut dialog.title,
                body: &mut dialog.body,
                generating: &mut dialog.generating,
            });
        }
        let row = self
            .stack_dialog_mut()?
            .rows
            .iter_mut()
            .find(|row| writes(&row.generating))?;
        Some(PrFields {
            title: &mut row.title,
            body: &mut row.body,
            generating: &mut row.generating,
        })
    }

    /// The generations writing into the stack preview.
    pub(crate) fn stack_generations(&self) -> impl Iterator<Item = &Generation> {
        self.stack_dialog()
            .into_iter()
            .flat_map(|dialog| dialog.rows.iter())
            .filter_map(|row| row.generating.as_ref())
    }
}

/// The rows of a stack whose commits are `commits` (`trunk()..rev`, newest
/// first): one per commit carrying a local bookmark, oldest first, the one
/// with a pull request preferred where a commit has several. Each is titled
/// after its commit's first line until the description is read.
fn stack_rows(
    commits: &[String],
    state: &crate::TabState,
    remote: &str,
) -> Result<Vec<StackRow>, &'static str> {
    let info = &state.session.bookmarks;
    let trunk = trunk_branch(info).ok_or("There's no trunk bookmark to base the stack on.")?;
    let targets = bookmark_targets(info);
    let mut at: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, target) in &targets {
        at.entry(target.as_str()).or_default().push(name.as_str());
    }
    let mut rows: Vec<StackRow> = Vec::new();
    let mut base = trunk;
    for commit in commits.iter().rev() {
        let Some(names) = at.get_mut(commit.as_str()) else {
            continue;
        };
        names.sort_by_key(|name| (state.forge.of(name).is_none(), *name));
        let name = names[0].to_owned();
        let title = state
            .session
            .commits
            .find_by_commit_id(commit)
            .filter(|row| row.has_description())
            .map(|row| row.description().to_owned())
            .unwrap_or_else(|| name.clone());
        rows.push(StackRow {
            push: push_of(info, &name, remote),
            bookmark: name.clone(),
            commit: commit.clone(),
            base: std::mem::replace(&mut base, name),
            title,
            body: String::new(),
            draft: false,
            existing: None,
            stats: None,
            progress: Progress::Idle,
            generating: None,
            described: None,
        });
    }
    if rows.is_empty() {
        return Err("There are no bookmarks between trunk and this revision.");
    }
    Ok(rows)
}

/// What pushing `name` to `remote` does, as far as the bookmarks say: a
/// push that moves it may still turn out to force ([`read_stats`] tells).
fn push_of(info: &BookmarksInfo, name: &str, remote: &str) -> Push {
    if in_sync(info, name, remote) {
        return Push::InSync;
    }
    let tracked = info.entry(name).is_some_and(|entry| {
        entry
            .remotes
            .iter()
            .any(|copy| copy.remote == remote && copy.tracked)
    });
    if tracked { Push::Move } else { Push::New }
}

/// One row's range for [`read_stats`]: its commits are `from..to`, and
/// `remote`, when a push moves the bookmark, is where the remote has it.
#[derive(Debug, Clone)]
struct Range {
    from: String,
    to: String,
    remote: Option<String>,
}

fn ranges_of(rows: &[StackRow], info: &BookmarksInfo, remote: &str) -> Vec<Range> {
    let mut from = info.trunk.clone().unwrap_or_else(|| "trunk()".to_owned());
    rows.iter()
        .map(|row| {
            let remote = (row.push == Push::Move)
                .then(|| {
                    info.entry(&row.bookmark)?
                        .remotes
                        .iter()
                        .find(|copy| copy.remote == remote && copy.tracked)
                        .map(|copy| copy.target.clone())
                })
                .flatten();
            Range {
                from: std::mem::replace(&mut from, row.commit.clone()),
                to: row.commit.clone(),
                remote,
            }
        })
        .collect()
}

/// Each range's commit count and line totals (none when jj can't say), and
/// whether moving the remote's bookmark to its end rewrites history there.
/// Read-only runs of the jj CLI, all at once.
async fn read_stats(root: String, ranges: Vec<Range>) -> Vec<(Option<Stats>, bool)> {
    let root = &root;
    futures::future::join_all(ranges.into_iter().map(|range| async move {
        let span = format!("{}..{}", range.from, range.to);
        let count = ["log", "--no-graph", "-r", &span, "-T", "\"x\\n\""];
        let stat = ["diff", "--stat", "-r", &span];
        let (commits, diff) = futures::join!(jj_read(root, &count), jj_read(root, &stat));
        let stats = commits.zip(diff.as_deref().and_then(stat_totals)).map(
            |(commits, (added, removed))| Stats {
                commits: commits.lines().count(),
                added,
                removed,
            },
        );
        let force = match &range.remote {
            Some(remote) => {
                let lost = format!("{remote} ~ ::{}", range.to);
                jj_read(root, &["log", "--no-graph", "-r", &lost, "-T", "\"x\""])
                    .await
                    .is_some_and(|out| !out.is_empty())
            }
            None => false,
        };
        (stats, force)
    }))
    .await
}

/// `jj <args>` in `root` without touching the working copy: its output, or
/// `None` when it fails.
async fn jj_read(root: &str, args: &[&str]) -> Option<String> {
    let output = crate::user_commands::embedded_jj()
        .args(["--ignore-working-copy", "--color=never", "-R", root])
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The lines added and removed in `jj diff --stat`'s last line: `2 files
/// changed, 5 insertions(+), 1 deletion(-)`.
fn stat_totals(stat: &str) -> Option<(usize, usize)> {
    let line = stat.lines().rev().find(|line| line.contains(" changed"))?;
    let (mut added, mut removed) = (0, 0);
    for part in line.split(", ") {
        let count: usize = part.split_whitespace().next()?.parse().ok()?;
        if part.contains("insertion") {
            added = count;
        } else if part.contains("deletion") {
            removed = count;
        }
    }
    Some((added, removed))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

// ── The dialog ───────────────────────────────────────────────────────────────

fn send(
    action: StackAction,
    cx: &mut Cx,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(Action::Forge(super::ForgeAction::Stack(action)), cx)
}

fn field_input(
    cx: &mut Cx,
    make: fn(String) -> StackAction,
) -> impl Fn(String, &mut Window, &mut gpui_kit::App) + 'static {
    let on_input = Diffui::listen(cx, move |this, value: &String, _, cx| {
        this.perform(
            Action::Forge(super::ForgeAction::Stack(make(value.clone()))),
            cx,
        );
    });
    move |value, window, cx| on_input(&value, window, cx)
}

pub(crate) fn build_dialog(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let Some(dialog) = ui.stack_dialog() else {
        return div().into_any_element();
    };
    let kind = dialog.remote.kind;
    let noun = kind.noun();
    let submission = ui
        .tab(dialog.tab)
        .and_then(|state| state.forge.submission.as_ref())
        .filter(|submission| Some(submission.activity) == dialog.run);

    // What it does, as a sentence; then what it pushes.
    let creates = dialog
        .sending()
        .filter(|row| row.existing.is_none())
        .count();
    let updates = dialog.sending().count() - creates;
    let mut lede = if !dialog.reading.done() {
        Prose::from(format!(
            "Reading the stack and asking {} which {noun}s exist\u{2026}",
            kind.name()
        ))
    } else {
        let what = match (creates, updates) {
            (0, 0) => format!("Every {noun} is up to date on "),
            (c, 0) => format!("Opens {c} {noun}{} on ", plural(c)),
            (0, u) => format!("Updates {u} {noun}{} on ", plural(u)),
            (c, u) => format!("Opens {c} {noun}{} and updates {u} on ", plural(c)),
        };
        Prose::from(what)
            .remote(dialog.remote.slug())
            .text(format!(" ({})", kind.name()))
    };
    if dialog.reading.done() && dialog.problem.is_none() && !dialog.rows.is_empty() {
        lede = lede.text(", bottom to top.");
    }
    let mut card = crate::overlay_kit::Dialog::new(&Prose::from("Submit stack"), look)
        .width(WIDTH)
        .lede(&lede);

    let mut body = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::MD_LG))
        .pr(px(crate::overlay_kit::DIALOG_PAD));
    let mut estimate = 0.0;
    if let Some(problem) = &dialog.problem {
        body = body.child(banner(
            ui::Tone::Danger,
            Prose::from(problem.as_str()),
            look,
        ));
        estimate += 56.0;
    } else if dialog.reading.done() {
        let (pushed, forced) = dialog.pushes();
        let line = if pushed.is_empty() {
            Prose::from("Nothing to push: ")
                .remote(&dialog.remote_name)
                .text(" has every bookmark as it is here.")
        } else {
            let mut line = Prose::from("Pushes ");
            line = names(line, &pushed);
            line.text(" to ")
                .remote(&dialog.remote_name)
                .text(" first.")
        };
        body = body.child(
            div()
                .w_full()
                .text_size(px(text_size::BODY))
                .child(line.view(look, theme.muted_text)),
        );
        estimate += 24.0;
        if !forced.is_empty() {
            let lead = names(Prose::from("Force-pushes "), &forced);
            let detail = Prose::from("Their history was rewritten here, so ")
                .remote(&dialog.remote_name)
                .text("'s commits are replaced.");
            body = body.child(crate::overlay_kit::callout(&lead, Some(&detail), look));
            estimate += 64.0;
        }
    }
    if let Some((tone, status)) = run_status(dialog, submission) {
        body = body.child(banner(tone, status, look));
        estimate += 56.0;
    }
    for (index, row) in dialog.rows.iter().enumerate() {
        let expanded = dialog.expanded == Some(index);
        body = body.child(row_card(ui, dialog, index, row, expanded, look, window, cx));
        estimate += ROW_ESTIMATE + if expanded { EXPANDED_ESTIMATE } else { 0.0 };
    }

    // The card is as tall as what it holds, up to the window; past that the
    // rows scroll.
    let content = 120.0 + 76.0 + estimate;
    let height = (ui.window_size.height - crate::toolbar::modal_top() - theme::space::XXL)
        .min(content)
        .max(240.0);
    card = card
        .height(height)
        .child(div().size_full().child(ui::scrollable("stack-rows", body)));

    let running = dialog.run.is_some() && dialog.outcome.is_none();
    let close_label = if dialog.run.is_some() {
        "Close"
    } else {
        "Cancel"
    };
    let close = look
        .button("stack-close", close_label)
        .on_click(send(StackAction::Close, cx));
    let mut buttons = vec![close.into_any_element()];
    match (&dialog.run, &dialog.outcome) {
        (None, _) => {
            let blocked = dialog.blocked();
            let mut accept = look
                .button("stack-accept", "Submit")
                .primary()
                .icon(icons::UPLOAD, theme.background)
                .disabled(blocked.is_some());
            if let Some(why) = blocked {
                accept = accept.tip(why);
            } else {
                accept = accept.on_click(send(StackAction::Confirm, cx));
            }
            buttons.push(accept.into_any_element());
            card = card.bound_hint(ui, Context::StackSubmit, &["forge.stack-confirm"], "Submit");
        }
        (Some(_), Some(Err(_))) => buttons.push(
            look.button("stack-retry", "Try again")
                .primary()
                .on_click(send(StackAction::Retry, cx))
                .into_any_element(),
        ),
        _ => {}
    }
    card = card.bound_hint(
        ui,
        Context::StackSubmit,
        &["forge.stack-cancel"],
        close_label,
    );
    if dialog.run.is_none() {
        card = card.bound_hint(
            ui,
            Context::StackSubmit,
            &["forge.stack-next-field"],
            "Next field",
        );
    }
    if running {
        card = card.note(crate::overlay_kit::hint_note(
            "Closing doesn't stop it: what's sent stays sent",
            theme.subtle_text,
            look.font,
        ));
    }
    let card = card.buttons(buttons).build();
    ui::input_modal("stack-modal", card, crate::toolbar::modal_top()).into_any_element()
}

/// `prose` then `names` as bookmarks, comma separated.
fn names(mut prose: Prose, names: &[&str]) -> Prose {
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            prose = prose.text(if index + 1 == names.len() {
                " and "
            } else {
                ", "
            });
        }
        prose = prose.bookmark(*name);
    }
    prose
}

fn banner(tone: ui::Tone, text: Prose, look: Look) -> gpui_kit::Div {
    let icon = match tone {
        ui::Tone::Ok => icons::CHECK,
        ui::Tone::Accent => icons::UPLOAD,
        _ => icons::ALERT_TRIANGLE,
    };
    let Look { theme, .. } = look;
    ui::Banner::new(tone)
        .icon(icon)
        .child(
            div()
                .text_size(px(text_size::BODY))
                .child(text.view(look, theme.text)),
        )
        .build(look)
}

/// Where a confirmed submission is, above the rows.
fn run_status(dialog: &StackDialog, submission: Option<&Submission>) -> Option<(ui::Tone, Prose)> {
    Some(match (&dialog.outcome, submission) {
        (Some(Ok(summary)), _) => (ui::Tone::Ok, Prose::from(format!("{summary}."))),
        (Some(Err(_)), _) => (
            ui::Tone::Danger,
            Prose::from(
                "It stopped at the failed row, and the rows after it weren't sent. Try again \
                 picks up where it stopped.",
            ),
        ),
        (None, Some(submission)) if submission.pushing() => (
            ui::Tone::Accent,
            Prose::from("Pushing to ")
                .remote(&dialog.remote_name)
                .text("\u{2026}"),
        ),
        (None, Some(_)) => (
            ui::Tone::Accent,
            Prose::from(format!("Talking to {}\u{2026}", dialog.remote.kind.name())),
        ),
        (None, None) => return None,
    })
}

/// The tag a row shows: its plan, or once confirmed, its progress.
fn row_tag(row: &StackRow, theme: theme::ThemeSpec) -> (String, theme::Color) {
    match &row.progress {
        Progress::Idle => match row.plan() {
            Plan::Create => ("New".to_owned(), theme.accent),
            Plan::Update { .. } => ("Update".to_owned(), theme.note_text),
            Plan::UpToDate => ("Up to date".to_owned(), theme.subtle_text),
        },
        Progress::Waiting => ("Waiting".to_owned(), theme.subtle_text),
        Progress::Running => (
            if row.existing.is_some() {
                "Updating\u{2026}"
            } else {
                "Opening\u{2026}"
            }
            .to_owned(),
            theme.accent,
        ),
        Progress::Done(done) => (done.clone(), theme.added_text),
        Progress::Unchanged if row.push != Push::InSync => ("Pushed".to_owned(), theme.added_text),
        Progress::Unchanged => ("Up to date".to_owned(), theme.added_text),
        Progress::Skipped => ("Up to date, skipped".to_owned(), theme.subtle_text),
        Progress::Failed(_) => ("Failed".to_owned(), theme.removed_text),
        Progress::NotSent => ("Not sent".to_owned(), theme.subtle_text),
    }
}

/// The quiet facts under a row's title: its pull request, commits and
/// lines, draft, and for an update what changes.
fn row_facts(row: &StackRow, kind: diffui_core::forge::ForgeKind) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(pr) = &row.existing {
        facts.push(format!("{}{}", kind.sigil(), pr.number));
    }
    if let Some(stats) = row.stats {
        facts.push(format!(
            "{} commit{} \u{b7} +{} \u{2212}{}",
            stats.commits,
            plural(stats.commits),
            stats.added,
            stats.removed
        ));
    }
    if row.draft {
        facts.push("Draft".to_owned());
    }
    if let Plan::Update { base, commits, .. } = row.plan() {
        if base && let Some(pr) = &row.existing {
            facts.push(format!("base was {}", pr.base));
        }
        if commits {
            facts.push("new commits".to_owned());
        }
    }
    if row.push == Push::Force {
        facts.push("force-pushed".to_owned());
    }
    facts
}

#[allow(clippy::too_many_arguments)]
fn row_card(
    ui: &Diffui,
    dialog: &StackDialog,
    index: usize,
    row: &StackRow,
    expanded: bool,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let kind = dialog.remote.kind;
    let (tag, ink) = row_tag(row, theme);
    let chevron = if expanded {
        icons::CHEVRON_DOWN
    } else {
        icons::CHEVRON_RIGHT
    };
    // Each name ellipsises on its own: GPUI cuts one run of one font.
    let name = |text: &str| {
        crate::typography::line(text.to_owned(), look.mono, text_size::BODY, theme.lane_base)
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
    };
    let one_line = |text: String, size: f32, ink| {
        crate::typography::line(text, font, size, ink)
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
    };
    let route = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .child(name(&row.bookmark))
        .child(
            crate::typography::line("\u{2192}", font, text_size::BODY, theme.subtle_text)
                .flex_none(),
        )
        .child(name(&row.base));
    let head = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .child(icons::icon(
            chevron,
            ui::ICON_SIZE,
            if dialog.editable() {
                theme.muted_text
            } else {
                theme::TRANSPARENT
            },
        ))
        .child(route)
        .child(ui::tag(tag, ink, ui::TagSize::Regular, font).view());
    let title = if row.title.trim().is_empty() {
        "No title".to_owned()
    } else {
        // One line: the first, should a title have more.
        row.title.lines().next().unwrap_or_default().to_owned()
    };
    let title_ink = if row.title.trim().is_empty() {
        theme.removed_text
    } else {
        theme.text
    };
    let facts = row_facts(row, kind).join(" \u{b7} ");
    let mut summary = div()
        .id(("stack-row", index))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::XS))
        .child(head)
        .child(
            div()
                .w_full()
                .flex()
                .pl(px(ui::ICON_SIZE + theme::space::MD))
                .child(one_line(title, text_size::BODY, title_ink)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .pl(px(ui::ICON_SIZE + theme::space::MD))
                .child(one_line(
                    if facts.is_empty() {
                        "\u{2026}".to_owned()
                    } else {
                        facts
                    },
                    text_size::CAPTION,
                    theme.subtle_text,
                )),
        );
    if dialog.editable() {
        summary = summary
            .cursor_pointer()
            .on_click(send(StackAction::Expand(index), cx));
    }
    let mut card = div()
        .id(("stack-card", index))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::LG))
        .p(px(theme::space::LG))
        .map(|card| theme::squircle(card, theme::radius::MD, None, Some(theme.border)))
        .child(summary);
    if let Progress::Failed(error) = &row.progress {
        card = card.child(
            div()
                .w_full()
                .pl(px(ui::ICON_SIZE + theme::space::MD))
                .text_size(px(text_size::BODY))
                .text_color(theme.removed_text)
                .child(error.clone()),
        );
    }
    if expanded {
        card = card.child(row_fields(ui, dialog, index, row, look, window, cx));
    }
    card.into_any_element()
}

/// The open row's title, description, draft toggle and Generate.
fn row_fields(
    ui: &Diffui,
    dialog: &StackDialog,
    index: usize,
    row: &StackRow,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let writing = row.writing();
    let label = |text: &'static str| {
        div()
            .text_size(px(text_size::BODY))
            .text_color(theme.muted_text)
            .child(text)
    };
    let mut title_spec = input::FieldSpec::new(TITLE_INPUT_ID, &row.title)
        .placeholder("Title")
        .undoable(true);
    let mut body_spec = input::FieldSpec::new(BODY_INPUT_ID, &row.body)
        .placeholder(if row.existing.is_some() {
            "Leave empty to keep the current description"
        } else {
            "Description"
        })
        .undoable(true);
    if !writing {
        title_spec = title_spec.on_input(field_input(cx, StackAction::Title));
        body_spec = body_spec.on_input(field_input(cx, StackAction::Body));
    }
    let (title, title_focused) =
        crate::field::dialog_input_with_focus(title_spec, look, window, cx);
    let (editor, body_focused) = input::multi_line_with_focus(body_spec, look.font, window, cx);
    dialog.focused.set(if title_focused {
        Some(Field::Title)
    } else if body_focused {
        Some(Field::Body)
    } else {
        None
    });
    let body = crate::field::field_well(body_focused, theme)
        .h(px(BODY_HEIGHT))
        .child(
            editor
                .appearance(false)
                .text_size(px(text_size::BODY))
                .h_full(),
        );
    let column = |label_text, field: AnyElement| {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(theme::space::XS))
            .child(label(label_text))
            .child(field)
    };
    let mut controls = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD));
    if row.existing.is_none() {
        let segment = |id, text: &'static str, active: bool, draft: bool, cx: &mut Cx| {
            ui::segment(
                (id, index),
                ui::Segment {
                    icon: None,
                    label: text.into(),
                    count: None,
                    active,
                    enabled: true,
                },
                ui::Segmented::Compact,
                look,
            )
            .on_click(send(StackAction::Draft(index, draft), cx))
        };
        controls = controls.child(
            ui::segmented(theme)
                .child(segment(
                    "stack-ready",
                    "Ready for review",
                    !row.draft,
                    false,
                    cx,
                ))
                .child(segment("stack-draft", "Draft", row.draft, true, cx)),
        );
    }
    controls = controls.child(div().flex_1());
    if let Some(generate) = crate::ai::ai_button(
        ui,
        look,
        crate::ai::AiButton {
            element: "stack-generate",
            command: "ai.pr-description",
            arg: CommandArg::Revision(RevisionSelection::Commit(row.commit.clone())),
            label: "Generate",
            generating: writing,
            held: false,
            size: ui::Size::Compact,
        },
        cx,
    ) {
        controls = controls.child(generate);
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::MD_LG))
        .child(column("Title", title))
        .child(column("Description", body.into_any_element()))
        .child(controls)
        .into_any_element()
}

#[cfg(test)]
pub(crate) mod tests;
