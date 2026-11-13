//! Opening and updating pull requests: the dialog for one, stack submission
//! for a run of them, and the [`Submission`] both become.
//!
//! A submission pushes first, through the ordinary push path with its checks
//! and confirmations, then asks the forge which of its branches already have
//! a pull request, then creates or updates each in order, one activity row
//! line per pull request. It stops at the first failure. Run again, it finds
//! the pull requests it made and only moves their bases, so it is idempotent.

use std::collections::HashMap;
use std::sync::Arc;

use diffui_core::forge::{
    CreatePr, ForgeError, ForgeKind, ForgeProvider, ForgeRemote, PullRequest, UpdatePr,
};
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};
use diffui_core::{
    BookmarksInfo, MutationOp, PushAllow, PushRequest, PushTarget, RevisionSelection,
};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::activity::{ActivityId, ActivityStatus};
use crate::commands::CommandArg;
use crate::forge::bookmark_targets;
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::{self, text_size};
use crate::ui::Look;
use crate::{Action, Cx, Diffui, Pending, TabId, input};

pub(crate) const TITLE_INPUT_ID: &str = "pull-request-title";
pub(crate) const BODY_INPUT_ID: &str = "pull-request-body";
pub(crate) const BASE_INPUT_ID: &str = "pull-request-base";

/// The most commits a stack or a base search looks through.
const STACK_LIMIT: usize = 500;

/// A user intent of the pull-request workflow.
#[derive(Debug, Clone)]
pub(crate) enum ForgeAction {
    /// Ask the forge for the tab's pull requests again.
    Refresh,
    /// The create or update dialog, for a bookmark or a revision.
    Open(CommandArg),
    Title(String),
    Body(String),
    Base(String),
    Draft(bool),
    Submit,
    Cancel,
    /// Push the bookmarks in `trunk()..rev` and open or update their pull
    /// requests, each based on the one before.
    SubmitStack(CommandArg),
}

/// What a pull request's head is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Head {
    Bookmark(String),
    /// A revision without a bookmark: pushed under a generated one, whose
    /// name the push reports.
    Generated(RevisionSelection),
}

/// The open create / update dialog.
#[derive(Debug, Clone)]
pub(crate) struct PrDialog {
    pub(crate) tab: TabId,
    pub(crate) kind: ForgeKind,
    /// The remote it pushes to, and its project.
    pub(crate) remote_name: String,
    pub(crate) remote: ForgeRemote,
    pub(crate) head: Head,
    /// The open pull request this updates.
    pub(crate) existing: Option<PullRequest>,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) base: String,
    pub(crate) draft: bool,
}

impl PrDialog {
    fn verb(&self) -> &'static str {
        if self.existing.is_some() {
            "Update"
        } else {
            "Create"
        }
    }

    /// Why it can't be submitted as it stands.
    fn problem(&self) -> Option<&'static str> {
        if self.title.trim().is_empty() {
            return Some("A title is needed.");
        }
        let base = self.base.trim();
        if base.is_empty() {
            return Some("A base branch is needed.");
        }
        if matches!(&self.head, Head::Bookmark(name) if name == base) {
            return Some("The base can't be the branch itself.");
        }
        None
    }
}

/// A forge job a tab issued to its repository actor.
#[derive(Debug, Clone)]
pub(crate) enum ForgeQuery {
    /// The dialog's head revision, for its title and body.
    DialogDetails,
    /// The commits between trunk and the dialog head's parents, nearest
    /// first, for its base.
    DialogAncestors,
    /// `trunk()..rev`, for the stack submission with this activity row.
    Stack(ActivityId),
    /// One stack commit's description.
    StackDetails(ActivityId, String),
}

/// One pull request a submission opens or updates.
#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub(crate) head: Head,
    pub(crate) base: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) draft: bool,
    /// The commit it's for: what a stack reads the description of.
    commit: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Reading the stack's descriptions.
    Gathering,
    /// Waiting on the push with this activity row.
    Pushing(ActivityId),
    /// Talking to the forge.
    Talking,
}

/// A create, update or stack submission under way, on its activity row.
#[derive(Debug)]
pub(crate) struct Submission {
    pub(crate) activity: ActivityId,
    remote_name: String,
    remote: ForgeRemote,
    phase: Phase,
    steps: Vec<Step>,
    /// An update rewrites the title and body, not only the base: the dialog's
    /// update does, a stack resubmission doesn't.
    rewrite: bool,
    /// What the forge already has, by head branch, once asked.
    existing: Option<HashMap<String, PullRequest>>,
    next: usize,
    running: bool,
    /// Descriptions a stack still waits on.
    awaiting: usize,
    /// What was done, by step.
    opened: usize,
    updated: usize,
}

impl Submission {
    fn new(
        activity: ActivityId,
        remote_name: String,
        remote: ForgeRemote,
        steps: Vec<Step>,
    ) -> Self {
        Self {
            activity,
            remote_name,
            remote,
            phase: Phase::Talking,
            steps,
            rewrite: false,
            existing: None,
            next: 0,
            running: false,
            awaiting: 0,
            opened: 0,
            updated: 0,
        }
    }
}

/// `trunk()`'s branch: a local bookmark on it, `main` / `master` / `trunk`
/// first, else a remote bookmark there.
pub(crate) fn trunk_branch(info: &BookmarksInfo) -> Option<String> {
    let trunk = info.trunk.as_deref()?;
    let mut local: Vec<&str> = info
        .bookmarks
        .iter()
        .filter(|entry| entry.local_target() == Some(trunk))
        .map(|entry| entry.name.as_str())
        .collect();
    local.sort_by_key(|name| (!matches!(*name, "main" | "master" | "trunk"), *name));
    local.first().map(|name| (*name).to_owned()).or_else(|| {
        info.bookmarks
            .iter()
            .find(|entry| entry.remotes.iter().any(|remote| remote.target == trunk))
            .map(|entry| entry.name.clone())
    })
}

/// Whether `name` is on `remote` as it is here, so a push would send nothing.
pub(crate) fn in_sync(info: &BookmarksInfo, name: &str, remote: &str) -> bool {
    info.entry(name).is_some_and(|entry| {
        let local = entry.local_target();
        local.is_some()
            && entry.remotes.iter().any(|copy| {
                copy.remote == remote && copy.tracked && Some(copy.target.as_str()) == local
            })
    })
}

/// The revset naming `selection`.
fn revset_of(selection: &RevisionSelection) -> String {
    match selection {
        RevisionSelection::WorkingCopy => "@".to_owned(),
        RevisionSelection::Commit(hex) => hex.clone(),
    }
}

/// Split a description into a title (its first line) and a body (the rest).
pub(crate) fn title_and_body(description: &str) -> (String, String) {
    let description = description.trim();
    match description.split_once('\n') {
        Some((title, body)) => (title.trim().to_owned(), body.trim().to_owned()),
        None => (description.to_owned(), String::new()),
    }
}

/// The base a new pull request at `head` gets: the nearest of `ancestors`
/// (nearest first) carrying a bookmark with an open pull request, else
/// trunk's branch.
pub(crate) fn default_base(
    ancestors: &[String],
    info: &BookmarksInfo,
    forge: &crate::forge::TabForge,
) -> Option<String> {
    let mut at: HashMap<&str, Vec<&str>> = HashMap::new();
    for entry in &info.bookmarks {
        if let Some(target) = entry.local_target() {
            at.entry(target).or_default().push(entry.name.as_str());
        }
    }
    ancestors
        .iter()
        .find_map(|commit| {
            at.get(commit.as_str())?
                .iter()
                .find(|name| forge.of(name).is_some_and(|d| d.pr.state.is_open()))
                .map(|name| (*name).to_owned())
        })
        .or_else(|| trunk_branch(info))
}

impl Diffui {
    pub(crate) fn update_forge(&mut self, action: ForgeAction, cx: &mut Cx) {
        match action {
            ForgeAction::Refresh => self.refresh_pull_requests_now(cx),
            ForgeAction::Open(arg) => self.open_pull_request_dialog(arg, cx),
            ForgeAction::Title(value) => self.edit_dialog(|dialog| dialog.title = value),
            ForgeAction::Body(value) => self.edit_dialog(|dialog| dialog.body = value),
            ForgeAction::Base(value) => self.edit_dialog(|dialog| dialog.base = value),
            ForgeAction::Draft(draft) => self.edit_dialog(|dialog| dialog.draft = draft),
            ForgeAction::Cancel => {
                self.pop_mode(ModeKind::PullRequest);
            }
            ForgeAction::Submit => self.submit_dialog(),
            ForgeAction::SubmitStack(arg) => self.submit_stack(arg),
        }
    }

    pub(crate) fn pull_request_dialog(&self) -> Option<&PrDialog> {
        match self.mode(ModeKind::PullRequest) {
            Some(Mode::PullRequest(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn edit_dialog(&mut self, edit: impl FnOnce(&mut PrDialog)) {
        if let Some(Mode::PullRequest(dialog)) = self.mode_mut(ModeKind::PullRequest) {
            edit(dialog);
        }
    }

    /// The dialog for the bookmark `arg` names, or for the revision's first
    /// bookmark, or for a revision without one (pushed under a generated
    /// name). An open pull request makes it the update dialog.
    fn open_pull_request_dialog(&mut self, arg: CommandArg, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let state = self.active();
        if !state.session.capabilities.mutate {
            return;
        }
        let Some((home_name, home)) = state.forge.home.clone() else {
            return;
        };
        let forge = &state.forge;
        let info = &state.session.bookmarks;
        let (head, revision) = match &arg {
            CommandArg::Bookmark { name, .. } => (
                Head::Bookmark(name.clone()),
                crate::forge::selection_of(&arg, self),
            ),
            _ => {
                let revision = crate::forge::selection_of(&arg, self);
                let bookmarks = self.bookmarks_on(&CommandArg::Revision(revision.clone()));
                let name = bookmarks
                    .iter()
                    .find(|name| forge.of(name).is_some())
                    .or(bookmarks.first())
                    .cloned();
                match name {
                    Some(name) => (Head::Bookmark(name), revision),
                    None => (Head::Generated(revision.clone()), revision),
                }
            }
        };
        let (remote_name, remote) = match &head {
            Head::Bookmark(name) => self
                .forge
                .forges
                .pick_remote(
                    &forge.remotes,
                    info.entry(name).and_then(|entry| entry.tracked_remote()),
                )
                .unwrap_or((home_name, home)),
            Head::Generated(_) => {
                let default = info.default_push_remote();
                default
                    .and_then(|name| {
                        let url = forge.remotes.iter().find(|(remote, _)| remote == name)?;
                        Some((name.to_owned(), self.forge.forges.claim(&url.1)?))
                    })
                    .unwrap_or((home_name, home))
            }
        };
        let existing = match &head {
            Head::Bookmark(name) => forge
                .of(name)
                .filter(|decoration| decoration.pr.state.is_open())
                .map(|decoration| decoration.pr.clone()),
            Head::Generated(_) => None,
        };
        let summary = state
            .session
            .commits
            .find(&revision)
            .filter(|row| row.has_description())
            .map(|row| row.description().to_owned())
            .unwrap_or_default();
        let dialog = PrDialog {
            tab,
            kind: remote.kind,
            remote_name,
            remote,
            title: existing.as_ref().map_or(summary, |pr| pr.title.clone()),
            body: String::new(),
            base: existing
                .as_ref()
                .map(|pr| pr.base.clone())
                .or_else(|| trunk_branch(info))
                .unwrap_or_default(),
            draft: false,
            head,
            existing,
        };
        let creating = dialog.existing.is_none();
        self.push_mode(Mode::PullRequest(dialog));
        input::focus(TITLE_INPUT_ID, cx);

        // The full description, and (for a new one) the base to stack on.
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        if !state.session.capabilities.details {
            return;
        }
        let job = state.session.next_job();
        state
            .jobs
            .issue(job, Pending::Forge(ForgeQuery::DialogDetails));
        self.send(
            tab,
            diffui_core::Command::RevisionDetails {
                job,
                revision: revision.clone(),
            },
        );
        if creating && let Some(state) = self.tab_mut(tab) {
            let query = RevsetQuery {
                limit: STACK_LIMIT,
                ..RevsetQuery::new(format!("trunk()..({})-", revset_of(&revision)))
            };
            let (job, effects) = state.session.resolve_revset(query);
            state
                .jobs
                .issue(job, Pending::Forge(ForgeQuery::DialogAncestors));
            self.run_effects(tab, effects);
        }
    }

    /// A forge job the tab issued answered.
    pub(crate) fn forge_job_done(
        &mut self,
        tab: TabId,
        query: ForgeQuery,
        payload: diffui_core::Payload,
    ) {
        use diffui_core::Payload;
        match (query, payload) {
            (ForgeQuery::DialogDetails, Payload::DetailsLoaded { details, .. }) => {
                let (title, body) = title_and_body(&details.description);
                self.edit_dialog(|dialog| {
                    if dialog.tab != tab || dialog.existing.is_some() {
                        return;
                    }
                    if !title.is_empty() {
                        dialog.title = title;
                    }
                    if dialog.body.is_empty() {
                        dialog.body = body;
                    }
                });
            }
            (ForgeQuery::DialogAncestors, Payload::RevsetResolved { result, .. }) => {
                self.dialog_ancestors(tab, result)
            }
            (ForgeQuery::Stack(activity), Payload::RevsetResolved { result, .. }) => {
                self.stack_resolved(tab, activity, result)
            }
            (
                ForgeQuery::StackDetails(activity, commit),
                Payload::DetailsLoaded { details, .. },
            ) => self.stack_description(tab, activity, &commit, Some(&details.description)),
            _ => {}
        }
    }

    /// A forge job the tab issued failed.
    pub(crate) fn forge_job_failed(&mut self, tab: TabId, query: ForgeQuery, error: String) {
        match query {
            // The dialog keeps what it guessed.
            ForgeQuery::DialogDetails | ForgeQuery::DialogAncestors => {}
            ForgeQuery::Stack(activity) => {
                self.fail_submission(tab, activity, format!("Couldn't read the stack: {error}"))
            }
            // The step keeps its title, without a body.
            ForgeQuery::StackDetails(activity, commit) => {
                self.stack_description(tab, activity, &commit, None)
            }
        }
    }

    fn dialog_ancestors(&mut self, tab: TabId, result: Result<RevsetMatch, RevsetError>) {
        let Ok(found) = result else {
            return;
        };
        let Some(state) = self.tab(tab) else {
            return;
        };
        let Some(base) = default_base(&found.commits, &state.session.bookmarks, &state.forge)
        else {
            return;
        };
        self.edit_dialog(|dialog| {
            if dialog.tab == tab && dialog.existing.is_none() {
                dialog.base = base;
            }
        });
    }

    fn submit_dialog(&mut self) {
        let Some(dialog) = self.pull_request_dialog().cloned() else {
            return;
        };
        if dialog.problem().is_some() {
            return;
        }
        self.pop_mode(ModeKind::PullRequest);
        let noun = dialog.kind.noun();
        let label = match &dialog.existing {
            Some(pr) => Prose::from(format!(
                "Update {noun} {}{}",
                dialog.kind.sigil(),
                pr.number
            )),
            None => Prose::from(format!("Create {noun}")),
        };
        let (activity, _) = self.begin_activity(dialog.tab, label, false);
        let step = Step {
            head: dialog.head.clone(),
            base: dialog.base.trim().to_owned(),
            title: dialog.title.trim().to_owned(),
            body: dialog.body.trim().to_owned(),
            draft: dialog.draft,
            commit: None,
        };
        let mut submission =
            Submission::new(activity, dialog.remote_name, dialog.remote, vec![step]);
        submission.rewrite = true;
        self.start_submission(dialog.tab, submission);
    }

    /// Hand `submission` to its tab and push what it needs pushed.
    fn start_submission(&mut self, tab: TabId, submission: Submission) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        if let Some(old) = state.forge.submission.take() {
            state.activities.finish(
                old.activity,
                ActivityStatus::Cancelled,
                Some("Replaced by a newer submission".to_owned()),
            );
        }
        state.forge.submission = Some(submission);
        self.push_submission(tab);
    }

    /// Push the submission's branches that aren't on the remote as they are
    /// here, all in one push, then talk to the forge.
    fn push_submission(&mut self, tab: TabId) {
        let Some(state) = self.tab(tab) else {
            return;
        };
        let Some(submission) = &state.forge.submission else {
            return;
        };
        let info = &state.session.bookmarks;
        let targets: Vec<PushTarget> = submission
            .steps
            .iter()
            .filter_map(|step| match &step.head {
                Head::Bookmark(name) if in_sync(info, name, &submission.remote_name) => None,
                Head::Bookmark(name) => Some(PushTarget::Bookmark(name.clone())),
                Head::Generated(revision) => Some(PushTarget::Change(revision.clone())),
            })
            .collect();
        if targets.is_empty() {
            if let Some(submission) = self
                .tab_mut(tab)
                .and_then(|state| state.forge.submission.as_mut())
            {
                submission.phase = Phase::Talking;
            }
            return;
        }
        let request = PushRequest {
            remote: submission.remote_name.clone(),
            targets,
            allow: PushAllow::default(),
            confirm: false,
        };
        let activity = submission.activity;
        match self.start_mutation_on(tab, MutationOp::Push(request), false, false) {
            Some(push) => {
                if let Some(submission) = self
                    .tab_mut(tab)
                    .and_then(|state| state.forge.submission.as_mut())
                {
                    submission.phase = Phase::Pushing(push);
                }
            }
            None => self.fail_submission(tab, activity, "Couldn't push.".to_owned()),
        }
    }

    /// The push a submission waits on finished. A push the checks refused
    /// comes back under the same row once the user answers, so only a plain
    /// failure ends the submission.
    pub(crate) fn submission_push_finished(
        &mut self,
        tab: TabId,
        pending: &crate::PendingMutation,
        result: &Result<diffui_core::MutationOutcome, diffui_core::RepoError>,
    ) {
        let Some(submission) = self
            .tab_mut(tab)
            .and_then(|state| state.forge.submission.as_mut())
        else {
            return;
        };
        if submission.phase != Phase::Pushing(pending.activity_id) {
            return;
        }
        let activity = submission.activity;
        match result {
            Ok(outcome) => {
                // The push named the generated branches in the order it was
                // asked for them.
                let mut named = outcome.bookmarks.iter();
                for step in &mut submission.steps {
                    let generated = matches!(step.head, Head::Generated(_));
                    let name = match &step.head {
                        Head::Bookmark(name) => named.find(|pushed| *pushed == name).cloned(),
                        Head::Generated(_) => named.next().cloned(),
                    };
                    if generated {
                        match name {
                            Some(name) => step.head = Head::Bookmark(name),
                            None => {
                                return self.fail_submission(
                                    tab,
                                    activity,
                                    "The push didn't say which branch it made.".to_owned(),
                                );
                            }
                        }
                    }
                }
                submission.phase = Phase::Talking;
            }
            Err(
                diffui_core::RepoError::PushRejected(_)
                | diffui_core::RepoError::Immutable { .. }
                | diffui_core::RepoError::UndoesPush { .. },
            ) => {}
            Err(error) => {
                let error = format!("The push failed, so nothing was sent to the forge: {error}");
                self.fail_submission(tab, activity, error)
            }
        }
    }

    /// Move `tab`'s submission on, if it can: notice a push that was
    /// cancelled, ask the forge what it has, then open or update the next
    /// pull request. Each answer comes back through an update, whose
    /// reconcile calls this again.
    pub(crate) fn advance_submission(&mut self, tab: TabId, cx: &mut Cx) {
        let forges = self.forge.forges.clone();
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(submission) = state.forge.submission.as_mut() else {
            return;
        };
        if submission.running {
            return;
        }
        let activity = submission.activity;
        match submission.phase {
            Phase::Gathering => {}
            Phase::Pushing(push) => {
                if matches!(
                    state.activities.status(push),
                    Some(ActivityStatus::Cancelled | ActivityStatus::Error) | None
                ) {
                    self.fail_submission(
                        tab,
                        activity,
                        "The push didn't go through, so nothing was sent to the forge.".to_owned(),
                    );
                }
            }
            Phase::Talking => {
                let Some(provider) = forges.provider(submission.remote.kind) else {
                    return;
                };
                let remote = submission.remote.clone();
                submission.running = true;
                match &submission.existing {
                    None => {
                        let heads: Vec<String> = submission
                            .steps
                            .iter()
                            .filter_map(|step| match &step.head {
                                Head::Bookmark(name) => Some(name.clone()),
                                Head::Generated(_) => None,
                            })
                            .collect();
                        Self::run_async(
                            async move { provider.pull_requests(remote, heads).await },
                            cx,
                            move |this, listed, _| this.submission_listed(tab, activity, listed),
                        );
                    }
                    Some(existing) => {
                        let Some(step) = submission.steps.get(submission.next).cloned() else {
                            submission.running = false;
                            return self.finish_submission(tab, activity);
                        };
                        let Head::Bookmark(head) = step.head.clone() else {
                            submission.running = false;
                            return self.fail_submission(
                                tab,
                                activity,
                                "A branch was never pushed.".to_owned(),
                            );
                        };
                        let open = existing.get(&head).filter(|pr| pr.state.is_open()).cloned();
                        let rewrite = submission.rewrite;
                        Self::run_async(
                            run_step(provider, remote, head, step, open, rewrite),
                            cx,
                            move |this, done, _| this.submission_stepped(tab, activity, done),
                        );
                    }
                }
            }
        }
    }

    fn submission_listed(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        listed: Result<Vec<PullRequest>, ForgeError>,
    ) {
        let Some(submission) = self.submission_mut(tab, activity) else {
            return;
        };
        submission.running = false;
        match listed {
            Ok(prs) => {
                submission.existing =
                    Some(prs.into_iter().map(|pr| (pr.head.clone(), pr)).collect());
            }
            Err(error) => self.fail_submission(tab, activity, error.to_string()),
        }
    }

    fn submission_stepped(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        done: Result<StepDone, ForgeError>,
    ) {
        let Some(submission) = self.submission_mut(tab, activity) else {
            return;
        };
        submission.running = false;
        let kind = submission.remote.kind;
        let line = match done {
            Ok(StepDone::Opened(pr)) => {
                submission.opened += 1;
                format!(
                    "Opened {}{} {} \u{2192} {}",
                    kind.sigil(),
                    pr.number,
                    pr.head,
                    pr.base
                )
            }
            Ok(StepDone::Updated(pr)) => {
                submission.updated += 1;
                format!(
                    "Updated {}{} {} \u{2192} {}",
                    kind.sigil(),
                    pr.number,
                    pr.head,
                    pr.base
                )
            }
            Ok(StepDone::Unchanged(pr)) => {
                format!("{}{} {} is up to date", kind.sigil(), pr.number, pr.head)
            }
            Err(error) => {
                let head = submission
                    .steps
                    .get(submission.next)
                    .map(|step| match &step.head {
                        Head::Bookmark(name) => name.clone(),
                        Head::Generated(_) => String::new(),
                    })
                    .unwrap_or_default();
                return self.fail_submission(tab, activity, format!("{head}: {error}"));
            }
        };
        submission.next += 1;
        if let Some(log) = self.activity_log_for(tab) {
            log.append_output(activity, line);
        }
    }

    fn submission_mut(&mut self, tab: TabId, activity: ActivityId) -> Option<&mut Submission> {
        self.tab_mut(tab)
            .and_then(|state| state.forge.submission.as_mut())
            .filter(|submission| submission.activity == activity)
    }

    fn finish_submission(&mut self, tab: TabId, activity: ActivityId) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(submission) = state.forge.submission.take() else {
            return;
        };
        let noun = submission.remote.kind.noun();
        let summary = match (submission.opened, submission.updated) {
            (0, 0) => "Nothing to change".to_owned(),
            (opened, 0) => format!("Opened {opened} {noun}{}", plural(opened)),
            (0, updated) => format!("Updated {updated} {noun}{}", plural(updated)),
            (opened, updated) => format!("Opened {opened}, updated {updated}"),
        };
        state
            .activities
            .finish(activity, ActivityStatus::Done, Some(summary));
        state.forge.wanted = true;
    }

    pub(crate) fn fail_submission(&mut self, tab: TabId, activity: ActivityId, error: String) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        if state
            .forge
            .submission
            .as_ref()
            .is_some_and(|submission| submission.activity == activity)
        {
            state.forge.submission = None;
            state.forge.wanted = true;
        }
        let title = state
            .activities
            .label(activity)
            .map(|label| label.clone().text(" failed"))
            .unwrap_or_else(|| "Pull request failed".into());
        state.activities.append_output(activity, error.clone());
        state
            .activities
            .finish(activity, ActivityStatus::Error, Some(error.clone()));
        self.push_error_toast(title, &error);
    }

    /// Submit the stack under the revision `arg` names.
    fn submit_stack(&mut self, arg: CommandArg) {
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
        let (activity, _) = self.begin_activity(tab, "Submit stack", false);
        let mut submission = Submission::new(activity, remote_name, remote, Vec::new());
        submission.phase = Phase::Gathering;
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        state.forge.submission = Some(submission);
        let query = RevsetQuery {
            limit: STACK_LIMIT,
            ..RevsetQuery::new(format!("trunk()..({})", revset_of(&revision)))
        };
        let (job, effects) = state.session.resolve_revset(query);
        state
            .jobs
            .issue(job, Pending::Forge(ForgeQuery::Stack(activity)));
        self.run_effects(tab, effects);
    }

    /// The stack's commits are known: one step per bookmarked commit, oldest
    /// first, each based on the one before and the first on trunk. Then read
    /// their descriptions.
    fn stack_resolved(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        result: Result<RevsetMatch, RevsetError>,
    ) {
        let found = match result {
            Ok(found) => found,
            Err(error) => {
                return self.fail_submission(
                    tab,
                    activity,
                    format!("Couldn't read the stack: {error}"),
                );
            }
        };
        let Some(state) = self.tab(tab) else {
            return;
        };
        let steps = match stack_steps(&found.commits, state) {
            Ok(steps) => steps,
            Err(error) => return self.fail_submission(tab, activity, error.to_owned()),
        };
        let commits: Vec<String> = steps
            .iter()
            .filter_map(|step| step.commit.clone())
            .collect();
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(submission) = state
            .forge
            .submission
            .as_mut()
            .filter(|submission| submission.activity == activity)
        else {
            return;
        };
        submission.steps = steps;
        if !state.session.capabilities.details || commits.is_empty() {
            return self.push_submission(tab);
        }
        submission.awaiting = commits.len();
        for commit in commits {
            let job = state.session.next_job();
            state.jobs.issue(
                job,
                Pending::Forge(ForgeQuery::StackDetails(activity, commit.clone())),
            );
            if let Some(handle) = &state.handle {
                handle.send(diffui_core::Command::RevisionDetails {
                    job,
                    revision: RevisionSelection::Commit(commit),
                });
            }
        }
    }

    /// A stack commit's description arrived (or couldn't): once every one
    /// has, push.
    fn stack_description(
        &mut self,
        tab: TabId,
        activity: ActivityId,
        commit: &str,
        description: Option<&str>,
    ) {
        let Some(submission) = self.submission_mut(tab, activity) else {
            return;
        };
        if let Some(description) = description {
            let (title, body) = title_and_body(description);
            for step in submission
                .steps
                .iter_mut()
                .filter(|step| step.commit.as_deref() == Some(commit))
            {
                if !title.is_empty() {
                    step.title = title.clone();
                }
                step.body = body.clone();
            }
        }
        submission.awaiting = submission.awaiting.saturating_sub(1);
        if submission.awaiting == 0 && submission.phase == Phase::Gathering {
            self.push_submission(tab);
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The steps of a stack whose commits are `commits` (`trunk()..rev`, newest
/// first): one per commit carrying a local bookmark, oldest first, the one
/// with a pull request preferred where a commit has several.
fn stack_steps(commits: &[String], state: &crate::TabState) -> Result<Vec<Step>, &'static str> {
    let info = &state.session.bookmarks;
    let trunk = trunk_branch(info).ok_or("There's no trunk bookmark to base the stack on.")?;
    let targets = bookmark_targets(info);
    let mut at: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, target) in &targets {
        at.entry(target.as_str()).or_default().push(name.as_str());
    }
    let mut steps: Vec<Step> = Vec::new();
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
        steps.push(Step {
            head: Head::Bookmark(name.clone()),
            base: std::mem::replace(&mut base, name),
            title,
            body: String::new(),
            draft: false,
            commit: Some(commit.clone()),
        });
    }
    if steps.is_empty() {
        return Err("There are no bookmarks between trunk and this revision.");
    }
    Ok(steps)
}

/// What one step did.
enum StepDone {
    Opened(PullRequest),
    Updated(PullRequest),
    Unchanged(PullRequest),
}

/// Open `step`'s pull request, or bring the `open` one in line: its base,
/// and with `rewrite` its title and body.
async fn run_step(
    provider: Arc<dyn ForgeProvider>,
    remote: ForgeRemote,
    head: String,
    step: Step,
    open: Option<PullRequest>,
    rewrite: bool,
) -> Result<StepDone, ForgeError> {
    let Some(open) = open else {
        let created = provider
            .create_pull_request(
                remote,
                CreatePr {
                    head,
                    base: step.base,
                    title: step.title,
                    body: step.body,
                    draft: step.draft,
                },
            )
            .await?;
        return Ok(StepDone::Opened(created));
    };
    let patch = UpdatePr {
        base: (open.base != step.base).then_some(step.base),
        title: (rewrite && open.title != step.title).then_some(step.title),
        body: (rewrite && !step.body.is_empty()).then_some(step.body),
    };
    if patch.is_empty() {
        return Ok(StepDone::Unchanged(open));
    }
    let updated = provider
        .update_pull_request(remote, open.number, patch)
        .await?;
    Ok(StepDone::Updated(updated))
}

// ── The dialog ───────────────────────────────────────────────────────────────

/// A dialog field's input handler: its text, as `make` wraps it.
fn field_input(
    cx: &mut Cx,
    make: fn(String) -> ForgeAction,
) -> impl Fn(String, &mut Window, &mut gpui_kit::App) + 'static {
    let on_input = Diffui::listen(cx, move |this, value: &String, _, cx| {
        this.perform(Action::Forge(make(value.clone())), cx);
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
    let Some(dialog) = ui.pull_request_dialog() else {
        return div().into_any_element();
    };
    let noun = dialog.kind.noun();
    let verb = dialog.verb();
    let title = match (&dialog.existing, &dialog.head) {
        (Some(pr), _) => Prose::from(format!(
            "Update {noun} {}{}",
            dialog.kind.sigil(),
            pr.number
        )),
        (None, Head::Bookmark(name)) => Prose::from(format!("New {noun} for ")).bookmark(name),
        (None, Head::Generated(revision)) => {
            Prose::from(format!("New {noun} for ")).then(ui.active().revision_prose(revision))
        }
    };
    let mut lede = Prose::from("Into ")
        .bookmark(dialog.base.trim())
        .text(" on ")
        .remote(dialog.remote.slug());
    if matches!(dialog.head, Head::Generated(_)) {
        lede = lede.text(". Pushed under a generated bookmark first.");
    } else if let Head::Bookmark(name) = &dialog.head
        && !in_sync(&ui.active().session.bookmarks, name, &dialog.remote_name)
    {
        lede = lede
            .text(". Pushes ")
            .bookmark(name)
            .text(" to ")
            .remote(&dialog.remote_name)
            .text(" first.");
    }

    let field_label = |text: &'static str| {
        div()
            .text_size(px(text_size::BODY))
            .text_color(theme.muted_text)
            .child(text)
    };
    let submit = Action::Forge(ForgeAction::Submit);
    let on_submit = Diffui::on_action(submit.clone(), cx);
    let title_spec = input::FieldSpec::new(TITLE_INPUT_ID, &dialog.title)
        .placeholder("Title")
        .on_input(field_input(cx, ForgeAction::Title))
        .on_submit(move |window, cx| on_submit(&(), window, cx));
    let base_spec = input::FieldSpec::new(BASE_INPUT_ID, &dialog.base)
        .placeholder("main")
        .on_input(field_input(cx, ForgeAction::Base));
    let body_placeholder = if dialog.existing.is_some() {
        "Leave empty to keep the current description"
    } else {
        "Description"
    };
    let body_spec = input::FieldSpec::new(BODY_INPUT_ID, &dialog.body)
        .placeholder(body_placeholder)
        .on_input(field_input(cx, ForgeAction::Body));
    let body = {
        let (editor, focused) = input::multi_line_with_focus(body_spec, look.font, window, cx);
        crate::field::field_well(focused, theme).h(px(120.0)).child(
            editor
                .appearance(false)
                .text_size(px(text_size::BODY))
                .h_full(),
        )
    };
    let row = |label, field: AnyElement| {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(theme::space::XS))
            .child(field_label(label))
            .child(field)
    };

    let mut card = crate::overlay_kit::Dialog::new(&title, look)
        .lede(&lede)
        .child(row(
            "Title",
            crate::field::dialog_input(title_spec, look, window, cx),
        ))
        .child(row(
            "Base",
            crate::field::dialog_input(
                base_spec,
                Look {
                    font: look.mono,
                    ..look
                },
                window,
                cx,
            ),
        ))
        .child(row("Description", body.into_any_element()));
    if dialog.existing.is_none() {
        let draft = dialog.draft;
        let segment = |id, label: &'static str, active: bool| {
            crate::ui::segment(
                id,
                crate::ui::Segment {
                    icon: None,
                    label: label.into(),
                    count: None,
                    active,
                    enabled: true,
                },
                crate::ui::Segmented::Compact,
                look,
            )
        };
        card =
            card.child(
                crate::ui::segmented(theme)
                    .child(
                        segment("pull-request-ready", "Ready for review", !draft).on_click(
                            Diffui::on_action(Action::Forge(ForgeAction::Draft(false)), cx),
                        ),
                    )
                    .child(segment("pull-request-draft", "Draft", draft).on_click(
                        Diffui::on_action(Action::Forge(ForgeAction::Draft(true)), cx),
                    )),
            );
    }
    if let Some(problem) = dialog.problem() {
        card = card.child(
            div()
                .w_full()
                .text_size(px(text_size::BODY))
                .text_color(theme.muted_text)
                .child(problem),
        );
    }
    let enabled = dialog.problem().is_none();
    let mut accept = look
        .button("pull-request-accept", format!("{verb} {noun}"))
        .primary()
        .disabled(!enabled);
    if enabled {
        accept = accept.on_click(Diffui::on_action(submit, cx));
    }
    let cancel = look
        .cancel_button("pull-request-cancel")
        .on_click(Diffui::on_action(Action::Forge(ForgeAction::Cancel), cx));
    let card = card
        .hint(&[crate::keymap::key_label("enter")], verb)
        .bound_hint(
            ui,
            crate::commands::Context::PullRequest,
            &["forge.dialog-cancel"],
            "Cancel",
        )
        .buttons([cancel.into_any_element(), accept.into_any_element()])
        .build();
    crate::ui::input_modal("pull-request-modal", card, crate::toolbar::modal_top())
        .into_any_element()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use diffui_core::{BookmarkEntry, RemoteBookmarkRef};
    use std::path::{Path, PathBuf};

    /// A fake `gh` / `glab` (`core/tests/fixtures/fake-forge-cli.sh`) in a
    /// directory of its own: rules in, recorded calls out. Never reaches a
    /// forge.
    pub(crate) struct FakeCli {
        dir: PathBuf,
    }

    impl FakeCli {
        pub(crate) fn new(name: &str, rules: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("diffui-app-forge-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("core/tests/fixtures/fake-forge-cli.sh");
            std::fs::copy(script, dir.join("cli")).unwrap();
            let fake = Self { dir };
            fake.answer(rules);
            fake
        }

        /// Replace the rules, and forget the calls so far.
        pub(crate) fn answer(&self, rules: &[(&str, &str)]) {
            let mut table = String::new();
            for (index, (pattern, response)) in rules.iter().enumerate() {
                let file = format!("response-{index}");
                std::fs::write(self.dir.join(&file), response).unwrap();
                table.push_str(&format!("{pattern}\t{file}\t0\n"));
            }
            std::fs::write(self.dir.join("rules"), table).unwrap();
            let _ = std::fs::remove_file(self.dir.join("calls"));
        }

        pub(crate) fn program(&self) -> PathBuf {
            self.dir.join("cli")
        }

        /// Each call's arguments, `HOST=…` first.
        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(self.dir.join("calls"))
                .unwrap_or_default()
                .split("\u{1}\n")
                .filter(|call| !call.is_empty())
                .map(|call| {
                    call.split('\0')
                        .filter(|arg| !arg.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .collect()
        }
    }

    fn entry(name: &str, target: &str, remote: Option<&str>) -> BookmarkEntry {
        BookmarkEntry {
            name: name.to_owned(),
            local_targets: vec![target.to_owned()],
            remotes: remote
                .map(|target| RemoteBookmarkRef {
                    remote: "origin".to_owned(),
                    target: target.to_owned(),
                    tracked: true,
                })
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn titles_and_bodies_come_from_the_description() {
        assert_eq!(
            title_and_body("Add the thing\n\nIt does stuff.\nMore.\n"),
            (
                "Add the thing".to_owned(),
                "It does stuff.\nMore.".to_owned()
            )
        );
        assert_eq!(
            title_and_body("Just a title"),
            ("Just a title".to_owned(), String::new())
        );
    }

    #[test]
    fn trunk_branch_prefers_the_usual_names() {
        let info = BookmarksInfo {
            bookmarks: vec![entry("release", "t", None), entry("main", "t", None)],
            trunk: Some("t".to_owned()),
            ..BookmarksInfo::default()
        };
        assert_eq!(trunk_branch(&info).as_deref(), Some("main"));
        let remote_only = BookmarksInfo {
            bookmarks: vec![BookmarkEntry {
                name: "master".to_owned(),
                local_targets: Vec::new(),
                remotes: vec![RemoteBookmarkRef {
                    remote: "origin".to_owned(),
                    target: "t".to_owned(),
                    tracked: true,
                }],
            }],
            trunk: Some("t".to_owned()),
            ..BookmarksInfo::default()
        };
        assert_eq!(trunk_branch(&remote_only).as_deref(), Some("master"));
    }

    #[test]
    fn a_bookmark_in_sync_needs_no_push() {
        let info = BookmarksInfo {
            bookmarks: vec![
                entry("pushed", "a", Some("a")),
                entry("ahead", "b", Some("a")),
                entry("local", "c", None),
            ],
            ..BookmarksInfo::default()
        };
        assert!(in_sync(&info, "pushed", "origin"));
        assert!(!in_sync(&info, "ahead", "origin"));
        assert!(!in_sync(&info, "local", "origin"));
        assert!(!in_sync(&info, "pushed", "fork"));
    }

    #[test]
    fn the_base_stacks_on_the_nearest_open_pull_request() {
        use crate::forge::{Decoration, TabForge};
        use diffui_core::forge::PrState;
        let info = BookmarksInfo {
            bookmarks: vec![
                entry("main", "t", Some("t")),
                entry("plain", "p", None),
                entry("merged", "m", Some("m")),
                entry("below", "b", Some("b")),
            ],
            trunk: Some("t".to_owned()),
            ..BookmarksInfo::default()
        };
        let mut forge = TabForge::default();
        let decorate = |forge: &mut TabForge, name: &str, state| {
            forge.decorations.insert(
                name.to_owned(),
                Decoration {
                    remote: ForgeRemote::new(ForgeKind::GitHub, "github.com", "o", "r"),
                    pr: PullRequest {
                        number: 1,
                        url: String::new(),
                        title: String::new(),
                        head: name.to_owned(),
                        base: "main".to_owned(),
                        head_sha: String::new(),
                        state,
                        review: None,
                    },
                    checks: None,
                    target: String::new(),
                },
            );
        };
        decorate(&mut forge, "merged", PrState::Merged);
        decorate(&mut forge, "below", PrState::Open);
        // Nearest first: a bookmark without a pull request and one whose
        // pull request is merged are passed over.
        let ancestors = ["p".to_owned(), "m".to_owned(), "b".to_owned()];
        assert_eq!(
            default_base(&ancestors, &info, &forge).as_deref(),
            Some("below")
        );
        assert_eq!(
            default_base(&ancestors[..2], &info, &forge).as_deref(),
            Some("main"),
            "trunk's branch when nothing below has an open pull request"
        );
    }

    fn rest_pr(number: u64, head: &str, base: &str) -> String {
        format!(
            r#"{{"number": {number}, "html_url": "https://github.invalid/team/app/pull/{number}",
            "title": "t", "state": "open", "draft": false, "merged_at": null,
            "head": {{"ref": "{head}", "sha": "s{number}"}}, "base": {{"ref": "{base}", "sha": "b"}}}}"#
        )
    }

    fn listed(prs: &[(u64, &str, &str)]) -> String {
        let rows: Vec<String> = prs
            .iter()
            .map(|(number, head, base)| {
                format!(
                    r#"{{"number": {number}, "url": "https://github.invalid/team/app/pull/{number}",
                    "title": "t", "headRefName": "{head}", "baseRefName": "{base}",
                    "headRefOid": "s{number}", "state": "OPEN", "isDraft": false,
                    "reviewDecision": "", "isCrossRepository": false}}"#
                )
            })
            .collect();
        format!("[{}]", rows.join(","))
    }

    fn has(call: &[String], args: &[&str]) -> bool {
        call.windows(args.len())
            .any(|window| window.iter().zip(args).all(|(a, b)| a == b))
    }

    /// The fixture with a two-commit stack on `main` (`stack-1`, `stack-2`,
    /// and an empty `@` on top), opened, its `origin` looking like a GitHub
    /// project that git sends to the fixture's bare remote, and the app's
    /// GitHub going through `fake`. `.invalid` never resolves, whatever
    /// happens. Returns the harness and `stack-2`'s commit.
    fn stacked_repo(fake: &FakeCli) -> (crate::harness::Harness, String) {
        use crate::harness::{Harness, fixture_jj, fixture_repo};
        let repo = fixture_repo();
        let jj = |args: &[&str]| {
            let output = std::process::Command::new(fixture_jj())
                .arg("-R")
                .arg(&repo)
                .args(args)
                .output()
                .expect("run jj");
            assert!(
                output.status.success(),
                "jj {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        jj(&["new", "main", "-m", "Stack one\n\nThe first body."]);
        std::fs::write(repo.join("stack-one.txt"), "one\n").unwrap();
        jj(&["bookmark", "create", "stack-1", "-r", "@"]);
        jj(&["new", "-m", "Stack two"]);
        std::fs::write(repo.join("stack-two.txt"), "two\n").unwrap();
        jj(&["bookmark", "create", "stack-2", "-r", "@"]);
        jj(&["new"]);
        let top = jj(&["log", "--no-graph", "-r", "stack-2", "-T", "commit_id"]);

        let mut h = Harness::open(Some(&repo));
        h.wait_for_graph();
        h.act(|ui, _| {
            ui.forge.forges = diffui_core::forge::Forges::with_programs(
                fake.program(),
                "/nonexistent/diffui-test/glab".into(),
            )
        });
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(repo.join(".git"))
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?}");
        };
        let url = "https://github.invalid/team/app.git";
        let bare = repo.join(".jj/fixture-remote.git");
        git(&["config", "remote.origin.url", url]);
        git(&["config", &format!("url.{}.insteadOf", bare.display()), url]);
        h.act(|ui, cx| {
            let tab = ui.active_tab_id().expect("a tab");
            ui.refresh_pull_requests(tab, true, cx);
        });
        h.settle();
        assert!(
            h.read(|ui| ui.active().forge.home.is_some()),
            "origin is on a forge"
        );
        (h, top.trim().to_owned())
    }

    /// Waits for the activity row `activity` to finish, and says how.
    fn finished(h: &mut crate::harness::Harness, activity: ActivityId) -> Option<ActivityStatus> {
        h.wait_until("the submission", |ui| {
            !matches!(
                ui.active().activities.status(activity),
                Some(ActivityStatus::Running | ActivityStatus::Queued)
            )
        });
        h.settle();
        h.read(|ui| ui.active().activities.status(activity))
    }

    /// Submit stack against a fake `gh`: the first run pushes both bookmarks
    /// in one push and opens a pull request for each, the second based on
    /// the first; a rerun changes nothing; a base moved on the forge is moved
    /// back.
    #[test]
    fn submit_stack_chains_bases_and_is_idempotent() {
        use crate::harness::Harness;
        let fake = FakeCli::new(
            "submit-stack",
            &[
                ("pr list *", "[]"),
                (
                    "api -X POST repos/team/app/pulls *head=stack-1 *",
                    &rest_pr(1, "stack-1", "main"),
                ),
                (
                    "api -X POST repos/team/app/pulls *head=stack-2 *",
                    &rest_pr(2, "stack-2", "stack-1"),
                ),
                ("api graphql *", "SUCCESS\n"),
            ],
        );
        let (mut h, top) = stacked_repo(&fake);
        let submit = |h: &mut Harness| {
            h.perform(Action::Forge(ForgeAction::SubmitStack(
                CommandArg::Revision(RevisionSelection::Commit(top.clone())),
            )));
            let activity = h
                .read(|ui| ui.active().forge.submission.as_ref().map(|s| s.activity))
                .expect("a submission");
            finished(h, activity)
        };

        assert_eq!(submit(&mut h), Some(ActivityStatus::Done));
        let calls = fake.calls();
        let posts: Vec<&Vec<String>> = calls.iter().filter(|call| has(call, &["POST"])).collect();
        assert_eq!(posts.len(), 2, "{calls:?}");
        assert!(has(posts[0], &["-f", "head=stack-1"]) && has(posts[0], &["-f", "base=main"]));
        assert!(has(posts[0], &["-f", "title=Stack one"]));
        assert!(has(posts[0], &["-f", "body=The first body."]));
        assert!(has(posts[1], &["-f", "head=stack-2"]) && has(posts[1], &["-f", "base=stack-1"]));
        h.read(|ui| {
            let info = &ui.active().session.bookmarks;
            assert!(in_sync(info, "stack-1", "origin") && in_sync(info, "stack-2", "origin"));
        });

        // Again, with both open as asked: nothing to push, nothing to change.
        fake.answer(&[
            (
                "pr list *",
                &listed(&[(1, "stack-1", "main"), (2, "stack-2", "stack-1")]),
            ),
            ("api graphql *", "SUCCESS\n"),
        ]);
        assert_eq!(submit(&mut h), Some(ActivityStatus::Done));
        let calls = fake.calls();
        assert!(
            !calls
                .iter()
                .any(|call| has(call, &["POST"]) || has(call, &["PATCH"])),
            "{calls:?}"
        );

        // Someone retargeted the second onto main: only its base moves back.
        fake.answer(&[
            (
                "pr list *",
                &listed(&[(1, "stack-1", "main"), (2, "stack-2", "main")]),
            ),
            (
                "api -X PATCH repos/team/app/pulls/2 *",
                &rest_pr(2, "stack-2", "stack-1"),
            ),
            ("api graphql *", "SUCCESS\n"),
        ]);
        assert_eq!(submit(&mut h), Some(ActivityStatus::Done));
        let patches: Vec<Vec<String>> = fake
            .calls()
            .into_iter()
            .filter(|call| has(call, &["PATCH"]))
            .collect();
        assert_eq!(patches.len(), 1);
        assert!(has(&patches[0], &["-f", "base=stack-1"]));
        assert!(
            !patches[0]
                .iter()
                .any(|arg| arg.starts_with("body=") || arg.starts_with("title=")),
            "a resubmission leaves the text alone"
        );
    }

    /// The dialog on a revision without a bookmark: its title and body come
    /// from the description, its base from the stack under it, and it
    /// pushes the revision under a generated bookmark before asking the
    /// forge for a draft.
    #[test]
    fn the_dialog_pushes_a_bare_revision_under_a_generated_bookmark() {
        let fake = FakeCli::new(
            "dialog",
            &[
                ("pr list *", &listed(&[(1, "stack-1", "main")])),
                (
                    "api -X POST repos/team/app/pulls *",
                    &rest_pr(3, "push-generated", "stack-1"),
                ),
                ("api graphql *", "PENDING\n"),
            ],
        );
        let (mut h, _) = stacked_repo(&fake);
        h.wait_until("the badge", |ui| ui.active().forge.of("stack-1").is_some());
        // `@`, the empty change on stack-2, has no bookmark; describe it.
        let commit = h.read(|ui| {
            ui.active()
                .session
                .commits
                .working_copy()
                .map(|row| row.commit_id().to_owned())
                .expect("a working copy")
        });
        h.perform(Action::Forge(ForgeAction::Open(CommandArg::Revision(
            RevisionSelection::Commit(commit),
        ))));
        h.settle();
        let dialog = h
            .read(|ui| ui.pull_request_dialog().cloned())
            .expect("the dialog");
        assert!(matches!(dialog.head, Head::Generated(_)));
        assert_eq!(
            dialog.base, "stack-1",
            "stacked on the nearest open pull request"
        );
        h.perform(Action::Forge(ForgeAction::Title(
            "Top of the stack".to_owned(),
        )));
        h.perform(Action::Forge(ForgeAction::Draft(true)));
        h.perform(Action::Forge(ForgeAction::Submit));
        let activity = h
            .read(|ui| ui.active().forge.submission.as_ref().map(|s| s.activity))
            .expect("a submission");
        // The push asks first, as any push of an undescribed working copy
        // does; the submission waits on the answer.
        h.wait_until("the push's question", |ui| ui.confirm().is_some());
        while h.read(|ui| ui.confirm().is_some()) {
            h.perform(Action::ConfirmAccept);
            h.settle();
        }
        assert_eq!(finished(&mut h, activity), Some(ActivityStatus::Done));
        let calls = fake.calls();
        let post = calls
            .iter()
            .find(|call| has(call, &["POST"]))
            .expect("a create");
        let head = post
            .iter()
            .find_map(|arg| arg.strip_prefix("head="))
            .expect("a head");
        assert!(head.starts_with("push-"), "{head}");
        assert!(has(post, &["-f", "base=stack-1"]));
        assert!(has(post, &["-f", "title=Top of the stack"]));
        assert!(has(post, &["-F", "draft=true"]));
        h.read(|ui| assert!(in_sync(&ui.active().session.bookmarks, head, "origin")));
    }
}
