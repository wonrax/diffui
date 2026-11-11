//! The bookmark workflow and `jj git push` from the UI: the name prompt behind
//! create / rename, push of new bookmarks, `--change` pushes and deletions,
//! the dialog for a push jj's checks refused, the working-copy push warning,
//! and the bookmark rows of the revision menu.
//!
//! Everything lowers to a [`MutationOp`] and goes through
//! [`Diffui::start_mutation_op`], so activity rows, queueing and the
//! confirmation stack work the way they do for every other mutation.

use crate::{Action, Cx, input, ui};
use diffui_core::{PushAllow, PushRejection, PushRequest, PushTarget, RevisionSelection};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::commands::CommandArg;
use crate::menu::MenuEntry;
use crate::modes::{Mode, ModeKind};
use crate::mutations::MutationOp;
use crate::prose::Prose;
use crate::theme::{self, ThemeSpec};
use crate::ui::Look;
use crate::{ConfirmDialog, Diffui, PendingMutation, TabState, activity};

pub(crate) const NAME_PROMPT_INPUT_ID: &str = "bookmark-name-input";

/// A user intent of the bookmark workflow — see [`Diffui::update_bookmark`].
#[derive(Debug, Clone)]
pub(crate) enum BookmarkAction {
    /// Open the name prompt for a new bookmark at `at`.
    Create {
        at: RevisionSelection,
    },
    /// Open the name prompt to rename local bookmark `name`.
    Rename {
        name: String,
    },
    PromptInput(String),
    PromptSubmit,
    PromptCancel,
    /// `jj git push -b <name> --remote <remote>`; `None` picks the remote
    /// the bookmark tracks, else the default push remote.
    Push {
        name: String,
        remote: Option<String>,
    },
    /// `jj git push --change <at>`; `None` picks the default push remote.
    PushChange {
        at: RevisionSelection,
        remote: Option<String>,
    },
    /// Delete the bookmark here and push the deletion to `remote`.
    DeleteEverywhere {
        name: String,
        remote: String,
    },
    /// Plain `jj git push` to the default remote.
    PushTracked,
}

#[derive(Debug, Clone)]
pub(crate) enum PromptKind {
    Create { at: RevisionSelection },
    Rename { old: String },
}

/// The open bookmark-name prompt.
#[derive(Debug, Clone)]
pub(crate) struct NamePrompt {
    pub(crate) kind: PromptKind,
    pub(crate) value: String,
}

/// What submitting the prompt's current text would do.
enum PromptVerdict {
    Invalid(Prose),
    Create(String),
    /// The name already exists: `jj bookmark set` moves it here instead.
    Move(String),
    Rename {
        old: String,
        new: String,
    },
}

impl Diffui {
    pub(crate) fn update_bookmark(&mut self, action: BookmarkAction, cx: &mut Cx) {
        match action {
            BookmarkAction::Create { at } => {
                self.open_name_prompt(PromptKind::Create { at }, "", cx)
            }
            BookmarkAction::Rename { name } => {
                let value = name.clone();
                self.open_name_prompt(PromptKind::Rename { old: name }, &value, cx)
            }
            BookmarkAction::PromptInput(value) => {
                if let Some(prompt) = self.name_prompt_mut() {
                    prompt.value = value;
                }
            }
            BookmarkAction::PromptCancel => {
                self.pop_mode(ModeKind::NamePrompt);
            }
            BookmarkAction::PromptSubmit => {
                let Some(prompt) = self.name_prompt() else {
                    return;
                };
                let op = match (self.prompt_verdict(prompt), &prompt.kind) {
                    (PromptVerdict::Invalid(_), _) => return,
                    (PromptVerdict::Create(name), PromptKind::Create { at }) => {
                        MutationOp::CreateBookmark {
                            name,
                            to: at.clone(),
                        }
                    }
                    (PromptVerdict::Move(name), PromptKind::Create { at }) => {
                        MutationOp::MoveBookmark {
                            name,
                            to: at.clone(),
                            push_remote: None,
                        }
                    }
                    (PromptVerdict::Rename { old, new }, _) => {
                        MutationOp::RenameBookmark { old, new }
                    }
                    _ => return,
                };
                self.pop_mode(ModeKind::NamePrompt);
                self.start_mutation_op(op)
            }
            BookmarkAction::Push { name, remote } => {
                let remote = remote.or_else(|| {
                    let info = &self.active().session.bookmarks;
                    info.entry(&name)
                        .and_then(|entry| entry.tracked_remote())
                        .or_else(|| info.default_push_remote())
                        .map(str::to_owned)
                });
                let Some(remote) = remote else {
                    return self.no_push_remote();
                };
                self.start_push(remote, vec![PushTarget::Bookmark(name)])
            }
            BookmarkAction::PushChange { at, remote } => {
                let remote = remote.or_else(|| {
                    self.active()
                        .session
                        .bookmarks
                        .default_push_remote()
                        .map(str::to_owned)
                });
                let Some(remote) = remote else {
                    return self.no_push_remote();
                };
                self.start_push(remote, vec![PushTarget::Change(at)])
            }
            BookmarkAction::DeleteEverywhere { name, remote } => {
                self.start_push(remote, vec![PushTarget::DeleteBookmark(name)])
            }
            BookmarkAction::PushTracked => {
                let Some(remote) = self
                    .active()
                    .session
                    .bookmarks
                    .default_push_remote()
                    .map(str::to_owned)
                else {
                    return self.no_push_remote();
                };
                self.start_push(remote, vec![PushTarget::Tracked])
            }
        }
    }

    fn start_push(&mut self, remote: String, targets: Vec<PushTarget>) {
        // A plain push sends whatever tracked bookmarks moved, which nobody
        // picked one by one: confirm the plan first.
        let confirm = targets.contains(&PushTarget::Tracked);
        self.start_mutation_op(MutationOp::Push(PushRequest {
            remote,
            targets,
            allow: PushAllow::default(),
            confirm,
        }))
    }

    /// `jj git push` without `--remote` falls back to `origin`; with several
    /// remotes, none called that, and no `git.push`, there's nothing to guess.
    fn no_push_remote(&mut self) {
        let remotes = &self.active().session.bookmarks.remotes;
        let body = if remotes.is_empty() {
            "This repository has no git remotes.".to_owned()
        } else {
            let remotes: Vec<String> = remotes.iter().map(|remote| format!("`{remote}`")).collect();
            format!(
                "Several remotes ({}) and no `git.push`. Pick one from the revision's menu, \
                 or set `git.push`.",
                remotes.join(", ")
            )
        };
        self.push_error_toast("Push needs a remote", &body);
    }

    fn open_name_prompt(&mut self, kind: PromptKind, value: &str, cx: &mut Cx) {
        if !self.active().session.capabilities.mutate {
            return;
        }
        self.push_mode(Mode::NamePrompt(NamePrompt {
            kind,
            value: value.to_owned(),
        }));
        crate::input::focus_and_select_all(NAME_PROMPT_INPUT_ID, cx);
    }

    pub(crate) fn name_prompt(&self) -> Option<&NamePrompt> {
        match self.mode(ModeKind::NamePrompt) {
            Some(Mode::NamePrompt(prompt)) => Some(prompt),
            _ => None,
        }
    }

    fn name_prompt_mut(&mut self) -> Option<&mut NamePrompt> {
        match self.mode_mut(ModeKind::NamePrompt) {
            Some(Mode::NamePrompt(prompt)) => Some(prompt),
            _ => None,
        }
    }

    fn prompt_verdict(&self, prompt: &NamePrompt) -> PromptVerdict {
        let name = prompt.value.trim().to_owned();
        if name.is_empty() {
            return PromptVerdict::Invalid(Prose::new());
        }
        // The name is right there in the field; the core error only repeats it.
        if diffui_core::jj::validate_bookmark_name(&name).is_err() {
            return PromptVerdict::Invalid("Not a valid bookmark name.".into());
        }
        let info = &self.active().session.bookmarks;
        // A local bookmark, or a deleted one whose tracked remote copy still
        // waits for its deletion push: either way `jj bookmark create` refuses
        // the name and `jj bookmark set` is what puts it here.
        let taken = info
            .entry(&name)
            .is_some_and(|entry| !entry.local_targets.is_empty() || entry.is_tracked_anywhere());
        match &prompt.kind {
            PromptKind::Create { .. } if taken => PromptVerdict::Move(name),
            PromptKind::Create { .. } => PromptVerdict::Create(name),
            PromptKind::Rename { old } if *old == name => {
                PromptVerdict::Invalid("That's its current name.".into())
            }
            PromptKind::Rename { .. }
                if info
                    .entry(&name)
                    .is_some_and(|entry| !entry.local_targets.is_empty()) =>
            {
                PromptVerdict::Invalid(Prose::new().bookmark(name).text(" already exists."))
            }
            PromptKind::Rename { old } => PromptVerdict::Rename {
                old: old.clone(),
                new: name,
            },
        }
    }

    /// `jj git push`'s commit checks refused `pending`. Where the CLI has an
    /// escape hatch for every reason, offer the push again with exactly those
    /// allowances; otherwise it's an error with the list.
    pub(crate) fn push_rejected(&mut self, pending: PendingMutation, rejection: PushRejection) {
        if rejection.is_confirmation() {
            let mut request = rejection.request;
            request.confirm = false;
            self.ask(ConfirmDialog {
                title: push_title(&rejection.moves, &request.remote),
                body: push_explanation(&rejection.moves, &request.remote),
                confirm_label: "Push".to_owned(),
                look: crate::ConfirmLook {
                    routine: true,
                    moves: rejection.moves,
                    callouts: Vec::new(),
                },
                pending: PendingMutation {
                    op: MutationOp::Push(request),
                    ..pending
                },
            });
            return;
        }
        let lines = rejection.lines();
        let Some(allow) = rejection.allowances() else {
            let error = format!(
                "jj git push would refuse this, and has no override for it:\n{}",
                lines.join("\n")
            );
            return self.finish_mutation_error(pending, error);
        };
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            log.extend_output(pending.activity_id, lines);
        }
        let count = rejection.commits.len();
        let commits = if count == 1 && !rejection.truncated {
            "a commit".to_owned()
        } else {
            format!(
                "{count}{} commits",
                if rejection.truncated { "+" } else { "" }
            )
        };
        let mut body = Prose::from(format!(
            "This publishes {commits} that pushes refuse by default:"
        ));
        for commit in &rejection.commits {
            let reasons: Vec<&str> = commit.reasons.iter().map(|r| r.describe()).collect();
            let subject = if commit.subject.is_empty() {
                "(no description set)"
            } else {
                commit.subject.as_str()
            };
            body = body
                .text("\n")
                .rev(&commit.change_id)
                .text(format!(" {subject}: {}", reasons.join(" and ")));
        }
        if rejection.truncated {
            body = body.text("\n\u{2026} and more");
        }
        if let Some(revset) = &rejection.private_commits {
            body = body
                .text("\nPrivate commits are ")
                .code(revset)
                .text(" (git.private-commits).");
        }
        let mut callouts = Vec::new();
        if rejection.commits.iter().any(|commit| commit.working_copy) {
            callouts.push((
                Prose::from("One of them is ")
                    .rev("@")
                    .text(", still being edited."),
                Some(working_copy_push_warning()),
            ));
        }
        let mut request = rejection.request;
        request.allow = allow;
        request.confirm = false;
        self.ask(ConfirmDialog {
            title: Prose::from("Push to ")
                .remote(&request.remote)
                .text(" anyway?"),
            body,
            confirm_label: "Push anyway".to_owned(),
            look: crate::ConfirmLook {
                routine: false,
                moves: rejection.moves,
                callouts,
            },
            pending: PendingMutation {
                op: MutationOp::Push(request),
                ..pending
            },
        });
    }

    fn finish_mutation_error(&mut self, pending: PendingMutation, error: String) {
        let mut failure = None;
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            let title = log
                .label(pending.activity_id)
                .map(|label| label.clone().text(" failed"))
                .unwrap_or_else(|| "Push failed".into());
            log.append_output(pending.activity_id, error.clone());
            log.finish(
                pending.activity_id,
                activity::ActivityStatus::Error,
                Some(error.clone()),
            );
            failure = Some(title);
        }
        if let Some(title) = failure {
            self.push_error_toast(title, &error);
        }
    }

    /// Run `pending`, unless it publishes the working-copy commit: then ask
    /// first. `@` is the commit still being edited — anything saved after the
    /// push is snapshotted into it and the bookmark moves off what the remote
    /// has. The CLI doesn't refuse it outright, so this is a warning, not a
    /// guard (an undescribed `@` is refused by the push checks regardless).
    pub(crate) fn run_mutation_checking_wc_push(&mut self, pending: PendingMutation) {
        let publishes = self
            .tab(pending.tab_id)
            .is_some_and(|tab| tab.publishes_working_copy(&pending.op));
        if !publishes {
            return self.run_mutation(pending);
        }
        self.ask(ConfirmDialog {
            title: Prose::from("Push the working-copy commit?"),
            body: Prose::from("This puts ")
                .rev("@")
                .text(" on the remote as it stands now."),
            confirm_label: "Push anyway".to_owned(),
            look: crate::ConfirmLook {
                routine: true,
                moves: Vec::new(),
                callouts: vec![(
                    Prose::from("The working copy is part of this push."),
                    Some(working_copy_push_warning()),
                )],
            },
            pending,
        });
    }

    // ── Revision context menu ───────────────────────────────────────────

    /// The bookmark-workflow rows every revision menu carries: create one
    /// here, and push this revision under a generated name — one row per
    /// remote when there's no default to pick.
    pub(crate) fn revision_bookmark_rows(&self, selection: &RevisionSelection) -> Vec<MenuEntry> {
        let mut rows =
            vec![self.menu_item("bookmark.create", CommandArg::Revision(selection.clone()))];
        let info = &self.active().session.bookmarks;
        let push_change = |remote: &str| CommandArg::PushRevision {
            revision: selection.clone(),
            remote: Some(remote.to_owned()),
        };
        match info.default_push_remote() {
            Some(remote) => rows.push(MenuEntry::Item {
                label: "Push as new bookmark".to_owned(),
                detail: Some(remote.to_owned()),
                emphasized: false,
                command: "revision.push-change",
                arg: push_change(remote),
            }),
            None if !info.remotes.is_empty() => rows.push(MenuEntry::Submenu {
                label: "Push as new bookmark".to_owned(),
                items: info
                    .remotes
                    .iter()
                    .map(|remote| {
                        self.menu_row(remote, "revision.push-change", push_change(remote))
                    })
                    .collect(),
            }),
            None => {}
        }
        rows
    }

    /// Where "Move bookmark here & push" sends `name`: its tracked remote,
    /// or — for a bookmark no remote tracks yet — the default push remote,
    /// which creates it there. `(remote, is_new)`.
    pub(crate) fn move_push_remote(&self, name: &str) -> Option<(String, bool)> {
        let info = &self.active().session.bookmarks;
        let entry = info.entry(name)?;
        if let Some(remote) = entry.tracked_remote() {
            return Some((remote.to_owned(), false));
        }
        // An untracked remote bookmark of the same name is in the way; the
        // push would be refused, so offer tracking (on its own row) instead.
        let remote = info.default_push_remote()?;
        if entry.remotes.iter().any(|r| r.remote == remote) {
            return None;
        }
        Some((remote.to_owned(), true))
    }

    /// The submenu rows for a local bookmark sitting on the clicked revision.
    pub(crate) fn local_bookmark_rows(&self, entry: &diffui_core::BookmarkEntry) -> Vec<MenuEntry> {
        let info = &self.active().session.bookmarks;
        let bookmark = |remote: Option<&str>| CommandArg::Bookmark {
            name: entry.name.clone(),
            remote: remote.map(str::to_owned),
            to: None,
        };
        let mut rows = Vec::new();
        if !entry.is_conflicted() {
            if let Some(remote) = entry.tracked_remote() {
                rows.push(self.menu_row(
                    format!("Push to {remote}"),
                    "bookmark.push",
                    bookmark(Some(remote)),
                ));
            } else {
                // Not on any remote yet: push creates it (and tracks it).
                // One row per remote unless an untracked copy is in the way
                // there — that one wants "Track" first.
                for remote in &info.remotes {
                    if entry.remotes.iter().any(|r| r.remote == *remote) {
                        continue;
                    }
                    rows.push(self.menu_row(
                        format!("Push to {remote} (new)"),
                        "bookmark.push-to",
                        bookmark(Some(remote)),
                    ));
                }
            }
        }
        rows.push(self.menu_item("diff.since-trunk", bookmark(None)));
        rows.push(self.menu_item("bookmark.rename", bookmark(None)));
        rows.push(self.menu_item("bookmark.delete", bookmark(None)));
        for remote in entry.remotes.iter().filter(|r| r.tracked) {
            rows.push(self.menu_row(
                format!("Delete here and on {}", remote.remote),
                "bookmark.delete-everywhere",
                bookmark(Some(&remote.remote)),
            ));
        }
        rows.push(self.menu_item("bookmark.forget", bookmark(None)));
        for remote in entry.remotes.iter().filter(|r| r.tracked) {
            rows.push(self.menu_row(
                format!("Untrack {}@{}", entry.name, remote.remote),
                "bookmark.untrack",
                bookmark(Some(&remote.remote)),
            ));
        }
        rows
    }

    /// The submenu rows for a remote bookmark `name@remote` on the clicked
    /// revision: track an untracked one; for a tracked one whose local
    /// bookmark was deleted, push the deletion or let it go.
    pub(crate) fn remote_bookmark_rows(
        &self,
        entry: &diffui_core::BookmarkEntry,
        remote: &diffui_core::RemoteBookmarkRef,
    ) -> Vec<MenuEntry> {
        let arg = CommandArg::Bookmark {
            name: entry.name.clone(),
            remote: Some(remote.remote.clone()),
            to: None,
        };
        if !remote.tracked {
            return vec![self.menu_item("bookmark.track", arg)];
        }
        vec![
            self.menu_row(
                format!("Delete on {} (push deletion)", remote.remote),
                "bookmark.push",
                arg.clone(),
            ),
            self.menu_row(
                format!("Untrack {}@{}", entry.name, remote.remote),
                "bookmark.untrack",
                arg,
            ),
        ]
    }
}

impl TabState {
    /// Whether `op` would put the working-copy commit on a remote.
    pub(crate) fn publishes_working_copy(&self, op: &MutationOp) -> bool {
        let session = &self.session;
        let Some(wc) = session
            .bookmarks
            .working_copy_commit
            .as_deref()
            .or_else(|| session.commits.working_copy().map(|row| row.commit_id()))
        else {
            return false;
        };
        let is_wc = |selection: &RevisionSelection| match selection {
            RevisionSelection::WorkingCopy => true,
            RevisionSelection::Commit(hex) => hex == wc,
        };
        let bookmark_at_wc = |name: &str| {
            session
                .bookmarks
                .entry(name)
                .is_some_and(|entry| entry.local_targets.iter().any(|t| t == wc))
        };
        match op {
            MutationOp::MoveBookmark {
                to,
                push_remote: Some(_),
                ..
            } => is_wc(to),
            MutationOp::PushBookmark { name, .. } => bookmark_at_wc(name),
            MutationOp::Push(request) => request.targets.iter().any(|target| match target {
                PushTarget::Bookmark(name) => bookmark_at_wc(name),
                PushTarget::SetBookmark { to, .. } => is_wc(to),
                PushTarget::Change(revision) => is_wc(revision),
                PushTarget::DeleteBookmark(_) | PushTarget::Tracked => false,
            }),
            _ => false,
        }
    }
}

/// Why pushing `@` is usually a mistake.
pub(crate) fn working_copy_push_warning() -> Prose {
    Prose::new()
        .rev("@")
        .text(" is still being edited, so later saves drift from what the remote has. Usually you push ")
        .rev("@-")
        .text(" instead.")
}

/// What a push sends, by kind: "bookmark", "tag", or "bookmarks and tags"
/// when both go; plural when `plural`.
pub(crate) fn ref_noun(tags: usize, total: usize, plural: bool) -> &'static str {
    match (tags, plural) {
        (0, false) => "bookmark",
        (0, true) => "bookmarks",
        (t, false) if t == total => "tag",
        (t, true) if t == total => "tags",
        _ => "bookmarks and tags",
    }
}

/// "1 tag", "2 bookmarks", "3 bookmarks and tags".
pub(crate) fn ref_count(tags: usize, total: usize) -> String {
    format!("{total} {}", ref_noun(tags, total, total != 1))
}

/// [`ref_noun`] for a push's moves.
fn moves_noun<'a>(moves: impl IntoIterator<Item = &'a diffui_core::BookmarkMove>) -> &'static str {
    let (tags, total) = moves.into_iter().fold((0, 0), |(tags, total), m| {
        (tags + usize::from(m.tag), total + 1)
    });
    ref_noun(tags, total, total > 1)
}

/// A push confirmation's title: how many bookmarks and tags go to which
/// remote.
pub(crate) fn push_title(moves: &[diffui_core::BookmarkMove], remote: &str) -> Prose {
    let count = ref_count(moves.iter().filter(|m| m.tag).count(), moves.len());
    Prose::from(format!("Push {count} to "))
        .remote(remote)
        .text("?")
}

/// One plain line on what a push does to the remote: whether anything there
/// is lost.
pub(crate) fn push_explanation(moves: &[diffui_core::BookmarkMove], remote: &str) -> Prose {
    use diffui_core::MoveKind;
    let noun = moves_noun(moves);
    let rewinds = moves
        .iter()
        .filter(|m| matches!(m.kind, MoveKind::Backward | MoveKind::Sideways));
    if rewinds.clone().next().is_some() {
        return Prose::from("Not a fast-forward: revisions only ")
            .remote(remote)
            .text(format!(
                " has are dropped from its {}.",
                moves_noun(rewinds)
            ));
    }
    if moves.iter().any(|m| m.kind == MoveKind::Delete) {
        return Prose::from("Deletes the bookmark on ")
            .remote(remote)
            .text(". The revisions stay here.");
    }
    let verb = if moves.len() > 1 { "are" } else { "is" };
    if moves.iter().all(|m| m.kind == MoveKind::Add) {
        return Prose::from(format!("The {noun} {verb} new on "))
            .remote(remote)
            .text(", and tracked from here on.");
    }
    Prose::from(format!("This moves the remote {noun} forward. "))
        .text("Nobody else's work is overwritten.")
}

/// Activity label for a push.
pub(crate) fn push_label(request: &PushRequest) -> Prose {
    let mut label = Prose::from("Push ");
    for (index, target) in request.targets.iter().enumerate() {
        if index > 0 {
            label = label.text(", ");
        }
        label = match target {
            PushTarget::Bookmark(name) | PushTarget::SetBookmark { name, .. } => {
                label.bookmark(name)
            }
            PushTarget::DeleteBookmark(name) => label.text("deletion of ").bookmark(name),
            PushTarget::Change(_) => label.text("new bookmark"),
            PushTarget::Tracked => label.text("tracked bookmarks and tags"),
        };
    }
    label.text(" to ").remote(&request.remote)
}

/// A local-bookmark name from a palette bookmark result's label: `main` and
/// its decorated forms (`main*`, `main??`), never a remote `name@remote`.
pub(crate) fn local_bookmark_label(label: &str) -> Option<&str> {
    if label.contains('@') {
        return None;
    }
    Some(
        label
            .strip_suffix("??")
            .or_else(|| label.strip_suffix('*'))
            .unwrap_or(label),
    )
}

/// The bookmark-name prompt: a modal card with the field, a line under it
/// only when there is something to say (what Enter will do instead, or why
/// it can't), and the buttons. Empty when closed.
pub(crate) fn build_name_prompt(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let look = ui.look(theme);
    let Some(prompt) = ui.name_prompt() else {
        return div().into_any_element();
    };
    let mono = ui.config.mono_font;
    let cancel = Action::Bookmark(BookmarkAction::PromptCancel);
    let submit = Action::Bookmark(BookmarkAction::PromptSubmit);

    let verdict = ui.prompt_verdict(prompt);
    let (title, verb) = match &prompt.kind {
        PromptKind::Create { at } => (
            Prose::from("New bookmark at ").then(ui.active().revision_prose(at)),
            "Create",
        ),
        PromptKind::Rename { old } => (Prose::from("Rename ").bookmark(old), "Rename"),
    };
    let (note, error, verb) = match &verdict {
        PromptVerdict::Invalid(reason) => (reason.clone(), true, verb),
        PromptVerdict::Create(_) => (Prose::new(), false, verb),
        PromptVerdict::Move(name) => (
            Prose::new()
                .bookmark(name)
                .text(" exists. This moves it here."),
            false,
            "Move here",
        ),
        PromptVerdict::Rename { old, .. } => {
            let on_remote = ui
                .active()
                .session
                .bookmarks
                .entry(old)
                .is_some_and(|entry| entry.is_tracked_anywhere());
            let note = if on_remote {
                Prose::new()
                    .bookmark(old)
                    .text(" stays on the remote until you push.")
            } else {
                Prose::new()
            };
            (note, false, verb)
        }
    };

    let on_input = Diffui::listen(cx, |this, value: &String, _, cx| {
        this.perform(
            Action::Bookmark(BookmarkAction::PromptInput(value.clone())),
            cx,
        );
    });
    let on_submit = Diffui::on_action(submit.clone(), cx);
    let spec = input::FieldSpec::new(NAME_PROMPT_INPUT_ID, &prompt.value)
        .placeholder("bookmark name")
        .on_input(move |value, window, cx| on_input(&value, window, cx))
        .on_submit(move |window, cx| on_submit(&(), window, cx));
    let mut card = crate::overlay_kit::Dialog::new(&title, look).child(crate::field::dialog_input(
        spec,
        Look { font: mono, ..look },
        window,
        cx,
    ));
    if !note.is_empty() {
        let color = if error {
            theme.removed_text
        } else {
            theme.muted_text
        };
        card = card.child(
            div()
                .w_full()
                .text_size(px(theme::text_size::BODY))
                .child(note.view(look, color)),
        );
    }

    let enabled = !matches!(verdict, PromptVerdict::Invalid(_));
    let mut accept = look
        .button("name-prompt-accept", verb)
        .primary()
        .disabled(!enabled);
    if enabled {
        accept = accept.on_click(Diffui::on_action(submit, cx));
    }
    let cancel_button = look
        .cancel_button("name-prompt-cancel")
        .on_click(Diffui::on_action(cancel, cx));

    let card = card
        .hint(&[crate::keymap::key_label("enter")], verb)
        .bound_hint(
            ui,
            crate::commands::Context::Prompt,
            &["bookmark.prompt-cancel"],
            "Cancel",
        )
        .buttons([cancel_button.into_any_element(), accept.into_any_element()])
        .build();
    ui::input_modal("name-prompt-modal", card, crate::toolbar::modal_top()).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TabId;
    use crate::tabs::tests::{app, push_tab};
    use diffui_core::{BookmarkEntry, BookmarksInfo, PushCheck, RejectedCommit, RemoteBookmarkRef};

    const WC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MAIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn ui_with_bookmarks(_cx: &mut crate::Cx) -> (Diffui, TabId) {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/repo");
        let session = &mut ui.tab_mut(tab).unwrap().session;
        session.capabilities.mutate = true;
        session.bookmarks = BookmarksInfo {
            bookmarks: vec![
                BookmarkEntry {
                    name: "main".to_owned(),
                    local_targets: vec![MAIN.to_owned()],
                    remotes: vec![RemoteBookmarkRef {
                        remote: "origin".to_owned(),
                        target: MAIN.to_owned(),
                        tracked: true,
                    }],
                },
                BookmarkEntry {
                    name: "wip".to_owned(),
                    local_targets: vec![WC.to_owned()],
                    remotes: Vec::new(),
                },
            ],
            working_copy_commit: Some(WC.to_owned()),
            // Several remotes, none of them `origin`, no `git.push`: there is
            // no default push remote.
            remotes: vec!["fork".to_owned(), "upstream".to_owned()],
            git_push: None,
            tags: Default::default(),
            trunk: None,
            tracked_push: Vec::new(),
        };
        (ui, tab)
    }

    fn pending(ui: &mut Diffui, tab: TabId, op: MutationOp) -> PendingMutation {
        let (activity_id, _) = ui.begin_activity(tab, "Push", true);
        PendingMutation {
            op,
            tab_id: tab,
            activity_id,
            allow_immutable: false,
        }
    }

    fn rejection(reasons: Vec<PushCheck>) -> PushRejection {
        PushRejection {
            remote: "origin".to_owned(),
            commits: vec![RejectedCommit {
                change_id: "zzzzzzzz".to_owned(),
                commit_id: "aaaaaaaaaaaa".to_owned(),
                subject: String::new(),
                reasons,
                working_copy: true,
            }],
            truncated: false,
            private_commits: None,
            request: PushRequest {
                remote: "origin".to_owned(),
                targets: vec![PushTarget::SetBookmark {
                    name: "main".to_owned(),
                    to: RevisionSelection::WorkingCopy,
                }],
                allow: PushAllow::default(),
                confirm: false,
            },
            moves: vec![diffui_core::BookmarkMove {
                bookmark: "main".to_owned(),
                tag: false,
                from: None,
                to: Some("aaaaaaaaaaaa".to_owned()),
                kind: diffui_core::MoveKind::Add,
                revisions: vec!["zzzzzzzz".to_owned()],
                revision_count: 1,
            }],
        }
    }

    #[test]
    fn an_overridable_rejection_offers_the_push_with_those_allowances() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, tab) = ui_with_bookmarks(cx);
            let held = pending(
                &mut ui,
                tab,
                MutationOp::PushBookmark {
                    name: "main".to_owned(),
                    remote: "origin".to_owned(),
                },
            );
            ui.push_rejected(held, rejection(vec![PushCheck::EmptyDescription]));
            let dialog = ui.confirm().expect("a confirmation is raised");
            let body = dialog.body.to_string();
            assert!(body.contains("has no description"), "{body}");
            match &dialog.pending.op {
                MutationOp::Push(request) => {
                    assert!(request.allow.empty_description);
                    assert!(!request.allow.private && !request.allow.conflicts);
                    assert_eq!(
                        request.targets,
                        vec![PushTarget::SetBookmark {
                            name: "main".to_owned(),
                            to: RevisionSelection::WorkingCopy,
                        }],
                        "the rerun carries the move the refusal rolled back"
                    );
                }
                other => panic!("expected a push rerun, got {other:?}"),
            }
        });
    }

    #[test]
    fn a_rejection_without_an_escape_hatch_is_just_an_error() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, tab) = ui_with_bookmarks(cx);
            let held = pending(
                &mut ui,
                tab,
                MutationOp::PushBookmark {
                    name: "main".to_owned(),
                    remote: "origin".to_owned(),
                },
            );
            ui.push_rejected(
                held,
                rejection(vec![
                    PushCheck::EmptyDescription,
                    PushCheck::MissingIdentity,
                ]),
            );
            assert!(ui.confirm().is_none(), "no override for a missing author");
            assert!(!ui.active().activities.any_running());
        });
    }

    #[test]
    fn pushing_the_working_copy_asks_first() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, tab) = ui_with_bookmarks(cx);
            let wc_push = MutationOp::Push(PushRequest {
                remote: "origin".to_owned(),
                targets: vec![PushTarget::Bookmark("wip".to_owned())],
                allow: PushAllow::default(),
                confirm: false,
            });
            assert!(ui.active().publishes_working_copy(&wc_push));
            assert!(
                !ui.active()
                    .publishes_working_copy(&MutationOp::PushBookmark {
                        name: "main".to_owned(),
                        remote: "origin".to_owned(),
                    })
            );
            assert!(
                ui.active()
                    .publishes_working_copy(&MutationOp::MoveBookmark {
                        name: "main".to_owned(),
                        to: RevisionSelection::Commit(WC.to_owned()),
                        push_remote: Some("origin".to_owned()),
                    })
            );
            assert!(
                !ui.active()
                    .publishes_working_copy(&MutationOp::MoveBookmark {
                        name: "main".to_owned(),
                        to: RevisionSelection::WorkingCopy,
                        push_remote: None,
                    })
            );

            let held = pending(&mut ui, tab, wc_push);
            ui.run_mutation_checking_wc_push(held);
            assert!(ui.confirm().is_some());
            assert!(ui.tab_mut(tab).unwrap().jobs.mutation().is_none());
        });
    }

    #[test]
    fn the_prompt_moves_an_existing_name_and_refuses_bad_ones() {
        crate::tabs::tests::with_cx(|cx| {
            let (mut ui, _tab) = ui_with_bookmarks(cx);
            ui.update_bookmark(
                BookmarkAction::Create {
                    at: RevisionSelection::WorkingCopy,
                },
                cx,
            );
            let verdict = |ui: &mut Diffui, value: &str, cx: &mut crate::Cx| {
                ui.update_bookmark(BookmarkAction::PromptInput(value.to_owned()), cx);
                ui.prompt_verdict(ui.name_prompt().unwrap())
            };
            assert!(
                matches!(verdict(&mut ui, "feature", cx), PromptVerdict::Create(name) if name == "feature")
            );
            assert!(
                matches!(verdict(&mut ui, "main", cx), PromptVerdict::Move(name) if name == "main")
            );
            assert!(matches!(
                verdict(&mut ui, "a b", cx),
                PromptVerdict::Invalid(_)
            ));
            assert!(matches!(
                verdict(&mut ui, "", cx),
                PromptVerdict::Invalid(_)
            ));

            ui.update_bookmark(BookmarkAction::PromptCancel, cx);
            ui.update_bookmark(
                BookmarkAction::Rename {
                    name: "wip".to_owned(),
                },
                cx,
            );
            assert!(matches!(
                verdict(&mut ui, "main", cx),
                PromptVerdict::Invalid(_)
            ));
            assert!(matches!(
                verdict(&mut ui, "wip", cx),
                PromptVerdict::Invalid(_)
            ));
            assert!(matches!(
                verdict(&mut ui, "done", cx),
                PromptVerdict::Rename { old, new } if old == "wip" && new == "done"
            ));
        });
    }

    #[test]
    fn several_remotes_without_a_default_get_a_chooser() {
        crate::tabs::tests::with_cx(|cx| {
            let (ui, _tab) = ui_with_bookmarks(cx);
            assert_eq!(ui.active().session.bookmarks.default_push_remote(), None);
            let rows = ui.revision_bookmark_rows(&RevisionSelection::WorkingCopy);
            assert!(
            rows.iter().any(|row| matches!(
                row,
                MenuEntry::Submenu { label, items } if label == "Push as new bookmark" && items.len() == 2
            )),
            "{rows:?}"
        );
            // A bookmark no remote tracks can still be pushed: one row per remote.
            let wip = ui.active().session.bookmarks.entry("wip").unwrap().clone();
            let labels: Vec<String> = ui
                .local_bookmark_rows(&wip)
                .into_iter()
                .filter_map(|row| match row {
                    MenuEntry::Item { label, .. } => Some(label),
                    _ => None,
                })
                .collect();
            assert!(
                labels.contains(&"Push to fork (new)".to_owned()),
                "{labels:?}"
            );
            assert!(
                labels.contains(&"Push to upstream (new)".to_owned()),
                "{labels:?}"
            );
        });
    }

    #[test]
    fn palette_bookmark_labels_resolve_to_local_names() {
        assert_eq!(local_bookmark_label("main"), Some("main"));
        assert_eq!(local_bookmark_label("main*"), Some("main"));
        assert_eq!(local_bookmark_label("main??"), Some("main"));
        assert_eq!(local_bookmark_label("main@origin"), None);
        assert_eq!(local_bookmark_label("ws@"), None);
    }
}
