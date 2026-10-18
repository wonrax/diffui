//! The bookmark workflow and `jj git push` from the UI: the name prompt behind
//! create / rename, push of new bookmarks, `--change` pushes and deletions,
//! the dialog for a push jj's checks refused, the working-copy push warning,
//! and the bookmark rows of the revision menu.
//!
//! Everything lowers to a [`MutationOp`] and goes through
//! [`Diffui::start_mutation_op`], so activity rows, queueing and the
//! confirmation stack work the way they do for every other mutation.

use iced::{
    Element, Length, Padding, Task, alignment,
    font::Weight,
    widget::{Space, column, container, mouse_area, opaque, row, stack, text, text_input},
};

use diffui_core::{PushAllow, PushRejection, PushRequest, PushTarget, RevisionSelection};

use crate::commands::CommandArg;
use crate::menu::MenuEntry;
use crate::modes::{Mode, ModeKind};
use crate::mutations::MutationOp;
use crate::theme::{
    self, ThemeSpec, emphasis_font, input_style, modal_style, scrim_style, text_size,
};
use crate::{Action, ConfirmDialog, Diffui, Message, PendingMutation, activity, ui};

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
    Invalid(String),
    Create(String),
    /// The name already exists: `jj bookmark set` moves it here instead.
    Move(String),
    Rename {
        old: String,
        new: String,
    },
}

impl Diffui {
    pub(crate) fn update_bookmark(&mut self, action: BookmarkAction) -> Task<Message> {
        match action {
            BookmarkAction::Create { at } => self.open_name_prompt(PromptKind::Create { at }, ""),
            BookmarkAction::Rename { name } => {
                let value = name.clone();
                self.open_name_prompt(PromptKind::Rename { old: name }, &value)
            }
            BookmarkAction::PromptInput(value) => {
                if let Some(prompt) = self.name_prompt_mut() {
                    prompt.value = value;
                }
                Task::none()
            }
            BookmarkAction::PromptCancel => {
                self.pop_mode(ModeKind::NamePrompt);
                Task::none()
            }
            BookmarkAction::PromptSubmit => {
                let Some(prompt) = self.name_prompt() else {
                    return Task::none();
                };
                let op = match (self.prompt_verdict(prompt), &prompt.kind) {
                    (PromptVerdict::Invalid(_), _) => return Task::none(),
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
                    _ => return Task::none(),
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

    fn start_push(&mut self, remote: String, targets: Vec<PushTarget>) -> Task<Message> {
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
    fn no_push_remote(&mut self) -> Task<Message> {
        let remotes = &self.active().session.bookmarks.remotes;
        let body = if remotes.is_empty() {
            "This repository has no git remotes.".to_owned()
        } else {
            format!(
                "There are several remotes ({}) and git.push isn't set. Pick one from the \
                 revision's context menu, or set git.push.",
                remotes.join(", ")
            )
        };
        self.push_error_toast("Push needs a remote".to_owned(), &body);
        Task::none()
    }

    fn open_name_prompt(&mut self, kind: PromptKind, value: &str) -> Task<Message> {
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        self.push_mode(Mode::NamePrompt(NamePrompt {
            kind,
            value: value.to_owned(),
        }));
        iced::widget::operation::focus(NAME_PROMPT_INPUT_ID)
            .chain(iced::widget::operation::select_all(NAME_PROMPT_INPUT_ID))
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
            return PromptVerdict::Invalid(String::new());
        }
        if let Err(error) = diffui_core::jj::validate_bookmark_name(&name) {
            return PromptVerdict::Invalid(error.to_string());
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
                PromptVerdict::Invalid("That's its current name.".to_owned())
            }
            PromptKind::Rename { .. }
                if info
                    .entry(&name)
                    .is_some_and(|entry| !entry.local_targets.is_empty()) =>
            {
                PromptVerdict::Invalid(format!("Bookmark {name} already exists."))
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
    pub(crate) fn push_rejected(
        &mut self,
        pending: PendingMutation,
        rejection: PushRejection,
    ) -> Task<Message> {
        if rejection.is_confirmation() {
            if let Some(log) = self.activity_log_for(pending.tab_id) {
                log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
            }
            let mut request = rejection.request;
            request.confirm = false;
            self.push_mode(Mode::Confirm(ConfirmDialog {
                title: format!("Push to \u{201c}{}\u{201d}?", request.remote),
                body: format!(
                    "Changes to push to {}:\n\n{}",
                    request.remote,
                    rejection.plan.join("\n")
                ),
                confirm_label: "Push".to_owned(),
                pending: PendingMutation {
                    op: MutationOp::Push(request),
                    ..pending
                },
            }));
            return Task::none();
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
            log.extend_output(pending.activity_id, lines.clone());
            log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
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
        let mut body = format!(
            "This would publish {commits} that jj git push refuses by default:\n\n{}\n\n\
             Changes to push to {}:\n{}\n\nPushing anyway is `jj git push {}`.",
            lines.join("\n"),
            rejection.remote,
            rejection.plan.join("\n"),
            allow.flags().join(" ")
        );
        if rejection.commits.iter().any(|commit| commit.working_copy) {
            body.push_str(" One of them is the working-copy commit, which is still in progress.");
        }
        let mut request = rejection.request;
        request.allow = allow;
        request.confirm = false;
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title: format!("Push to \u{201c}{}\u{201d} refused", request.remote),
            body,
            confirm_label: "Push anyway".to_owned(),
            pending: PendingMutation {
                op: MutationOp::Push(request),
                ..pending
            },
        }));
        Task::none()
    }

    fn finish_mutation_error(&mut self, pending: PendingMutation, error: String) -> Task<Message> {
        let mut failure = None;
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            let title = log
                .label(pending.activity_id)
                .map(|label| format!("{label} failed"))
                .unwrap_or_else(|| "Push failed".to_owned());
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
        Task::none()
    }

    /// Run `pending`, unless it publishes the working-copy commit: then ask
    /// first. `@` is the commit still being edited — anything saved after the
    /// push is snapshotted into it and the bookmark moves off what the remote
    /// has. The CLI doesn't refuse it outright, so this is a warning, not a
    /// guard (an undescribed `@` is refused by the push checks regardless).
    pub(crate) fn run_mutation_checking_wc_push(
        &mut self,
        pending: PendingMutation,
    ) -> Task<Message> {
        if !self.publishes_working_copy(&pending.op) {
            return self.run_mutation(pending);
        }
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
        }
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title: "Push the working-copy commit?".to_owned(),
            body: WORKING_COPY_PUSH_WARNING.to_owned(),
            confirm_label: "Push anyway".to_owned(),
            pending,
        }));
        Task::none()
    }

    /// Whether `op` would put the working-copy commit on a remote.
    pub(crate) fn publishes_working_copy(&self, op: &MutationOp) -> bool {
        let session = &self.active().session;
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

pub(crate) const WORKING_COPY_PUSH_WARNING: &str = "This pushes the working-copy commit (@), the one you're still editing. Anything saved \
     after the push is snapshotted into it and the bookmark moves off what the remote has. \
     Usually you push @- after starting a new change.";

/// Activity label for a push.
pub(crate) fn push_label(request: &PushRequest) -> String {
    let what: Vec<String> = request
        .targets
        .iter()
        .map(|target| match target {
            PushTarget::Bookmark(name) | PushTarget::SetBookmark { name, .. } => name.clone(),
            PushTarget::DeleteBookmark(name) => format!("deletion of {name}"),
            PushTarget::Change(_) => "new bookmark".to_owned(),
            PushTarget::Tracked => "tracked bookmarks".to_owned(),
        })
        .collect();
    format!("Push {} to {}", what.join(", "), request.remote)
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

// ── Name prompt view ────────────────────────────────────────────────────

const PROMPT_PADDING: f32 = theme::space::XL + theme::space::XS;

/// The bookmark-name prompt: a modal card with the field, a line saying what
/// Enter will do (or why it can't), and the buttons. Empty when closed.
pub(crate) fn build_name_prompt(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(prompt) = ui.name_prompt() else {
        return Space::new().into();
    };
    let cancel = Message::Action(Action::Bookmark(BookmarkAction::PromptCancel));
    let submit = Message::Action(Action::Bookmark(BookmarkAction::PromptSubmit));

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| scrim_style()),
    )
    .on_press(cancel.clone());

    let verdict = ui.prompt_verdict(prompt);
    let (title, verb) = match &prompt.kind {
        PromptKind::Create { at } => (
            format!("New bookmark at {}", ui.revision_short_label(at)),
            "Create",
        ),
        PromptKind::Rename { old } => (format!("Rename bookmark {old}"), "Rename"),
    };
    let (note, note_color, verb) = match &verdict {
        PromptVerdict::Invalid(reason) => (reason.clone(), theme.removed_text, verb),
        PromptVerdict::Create(_) => (String::new(), theme.muted_text, verb),
        PromptVerdict::Move(name) => (
            format!("{name} already exists \u{2014} this moves it here (jj bookmark set)."),
            theme.muted_text,
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
                format!("The remote keeps {old} until you push its deletion.")
            } else {
                String::new()
            };
            (note, theme.muted_text, verb)
        }
    };

    let input = text_input("bookmark name", &prompt.value)
        .id(NAME_PROMPT_INPUT_ID)
        .padding(Padding::from([theme::space::SM, theme::space::MD]))
        .size(text_size::BODY_LG)
        .font(ui.config.mono_font)
        .on_input(|value| Message::Action(Action::Bookmark(BookmarkAction::PromptInput(value))))
        .on_submit(submit.clone())
        .style(move |_, _| input_style(theme));

    let cancel_button = ui::text_button(
        None,
        "Cancel",
        ui::Variant::Raised,
        ui::Size::Standard,
        ui.config.ui_font,
        theme,
    )
    .on_press(cancel);
    let mut accept = ui::text_button(
        None,
        verb,
        ui::Variant::Primary,
        ui::Size::Standard,
        ui.config.ui_font,
        theme,
    );
    if !matches!(verdict, PromptVerdict::Invalid(_)) {
        accept = accept.on_press(submit);
    }

    let body = column![
        text(title)
            .size(text_size::TITLE)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        input,
        text(note)
            .size(text_size::UI)
            .color(note_color)
            .font(ui.config.ui_font),
        row![Space::new().width(Length::Fill), cancel_button, accept]
            .spacing(theme::space::MD)
            .align_y(alignment::Vertical::Center),
    ]
    .spacing(12);

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(460.0))
            .padding(PROMPT_PADDING)
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::Ui(crate::UiEvent::ConfirmNoOp));

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: crate::theme::modal_top(),
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });
    opaque(stack![scrim, centered])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TabId;
    use crate::tabs::tests::{app, push_tab};
    use diffui_core::{BookmarkEntry, BookmarksInfo, PushCheck, RejectedCommit, RemoteBookmarkRef};

    const WC: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MAIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn ui_with_bookmarks() -> (Diffui, TabId) {
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
            plan: vec!["bookmark: main [add to aaaaaaaaaaaa]".to_owned()],
        }
    }

    #[test]
    fn an_overridable_rejection_offers_the_push_with_those_allowances() {
        let (mut ui, tab) = ui_with_bookmarks();
        let held = pending(
            &mut ui,
            tab,
            MutationOp::PushBookmark {
                name: "main".to_owned(),
                remote: "origin".to_owned(),
            },
        );
        let _ = ui.push_rejected(held, rejection(vec![PushCheck::EmptyDescription]));
        let dialog = ui.confirm().expect("a confirmation is raised");
        assert!(
            dialog.body.contains("--allow-empty-description"),
            "{}",
            dialog.body
        );
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
    }

    #[test]
    fn a_rejection_without_an_escape_hatch_is_just_an_error() {
        let (mut ui, tab) = ui_with_bookmarks();
        let held = pending(
            &mut ui,
            tab,
            MutationOp::PushBookmark {
                name: "main".to_owned(),
                remote: "origin".to_owned(),
            },
        );
        let _ = ui.push_rejected(
            held,
            rejection(vec![
                PushCheck::EmptyDescription,
                PushCheck::MissingIdentity,
            ]),
        );
        assert!(ui.confirm().is_none(), "no override for a missing author");
        assert!(!ui.active().activities.any_running());
    }

    #[test]
    fn pushing_the_working_copy_asks_first() {
        let (mut ui, tab) = ui_with_bookmarks();
        let wc_push = MutationOp::Push(PushRequest {
            remote: "origin".to_owned(),
            targets: vec![PushTarget::Bookmark("wip".to_owned())],
            allow: PushAllow::default(),
            confirm: false,
        });
        assert!(ui.publishes_working_copy(&wc_push));
        assert!(!ui.publishes_working_copy(&MutationOp::PushBookmark {
            name: "main".to_owned(),
            remote: "origin".to_owned(),
        }));
        assert!(ui.publishes_working_copy(&MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::Commit(WC.to_owned()),
            push_remote: Some("origin".to_owned()),
        }));
        assert!(!ui.publishes_working_copy(&MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::WorkingCopy,
            push_remote: None,
        }));

        let held = pending(&mut ui, tab, wc_push);
        let _ = ui.run_mutation_checking_wc_push(held);
        assert!(ui.confirm().is_some());
        assert!(ui.tab_mut(tab).unwrap().jobs.mutation.is_none());
    }

    #[test]
    fn the_prompt_moves_an_existing_name_and_refuses_bad_ones() {
        let (mut ui, _tab) = ui_with_bookmarks();
        let _ = ui.update_bookmark(BookmarkAction::Create {
            at: RevisionSelection::WorkingCopy,
        });
        let verdict = |ui: &mut Diffui, value: &str| {
            let _ = ui.update_bookmark(BookmarkAction::PromptInput(value.to_owned()));
            ui.prompt_verdict(ui.name_prompt().unwrap())
        };
        assert!(
            matches!(verdict(&mut ui, "feature"), PromptVerdict::Create(name) if name == "feature")
        );
        assert!(matches!(verdict(&mut ui, "main"), PromptVerdict::Move(name) if name == "main"));
        assert!(matches!(verdict(&mut ui, "a b"), PromptVerdict::Invalid(_)));
        assert!(matches!(verdict(&mut ui, ""), PromptVerdict::Invalid(_)));

        let _ = ui.update_bookmark(BookmarkAction::PromptCancel);
        let _ = ui.update_bookmark(BookmarkAction::Rename {
            name: "wip".to_owned(),
        });
        assert!(matches!(
            verdict(&mut ui, "main"),
            PromptVerdict::Invalid(_)
        ));
        assert!(matches!(verdict(&mut ui, "wip"), PromptVerdict::Invalid(_)));
        assert!(matches!(
            verdict(&mut ui, "done"),
            PromptVerdict::Rename { old, new } if old == "wip" && new == "done"
        ));
    }

    #[test]
    fn several_remotes_without_a_default_get_a_chooser() {
        let (ui, _tab) = ui_with_bookmarks();
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
