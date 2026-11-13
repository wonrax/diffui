//! Answering what a script asks the app for: the repository behind a handle,
//! jj commands, and the UI slots.
//!
//! A repository call is answered only while its handle is current (see
//! [`Diffui::resolve_handle`]). Reads of the session answer at once; reads of
//! the repository run the embedded jj CLI at the handle's operation, off this
//! thread and off the repository's actor ([`super::query`]); `jj.run` goes
//! the palette's way, on the handle's tab.

use diffui_core::Vcs;
use diffui_plugins::Reply;
use diffui_plugins::protocol::{
    Bookmark, Choice, HostCall, JjRun, Remote, RemoteBookmark, RepoCall, RevisionRef, Selection,
    Tone, UiCall,
};

use super::query::{self, At};
use crate::activity::ActivityStatus;
use crate::modes::Mode;
use crate::prose::Prose;
use crate::user_commands::RunId;
use crate::{Cx, Diffui, TabId};

/// A plugin's prompt in front of the user. It is shown in the dialogs a
/// declared command's prompts use, and answered through their paths.
pub(super) struct Asking {
    id: RunId,
    reply: Reply,
    kind: AskKind,
}

enum AskKind {
    Input,
    Confirm,
    Choose(Vec<Choice>),
}

impl Asking {
    /// Resolve as dismissed: `null`, or `false` for a confirm.
    fn dismiss(self) {
        match self.kind {
            AskKind::Confirm => self.reply.ok(false),
            AskKind::Input | AskKind::Choose(_) => self.reply.ok(serde_json::Value::Null),
        }
    }
}

impl Diffui {
    pub(super) fn answer_plugin(&mut self, id: &str, call: HostCall, reply: Reply, cx: &mut Cx) {
        match call {
            HostCall::Repo { repo, call } => match self.resolve_handle(&repo) {
                Some(tab) => self.answer_repo_call(tab, repo.op, call, reply, cx),
                None => reply.stale(),
            },
            HostCall::Ui(call) => self.answer_ui_call(id, call, reply, cx),
        }
    }

    fn answer_repo_call(
        &mut self,
        tab: TabId,
        op: String,
        call: RepoCall,
        reply: Reply,
        cx: &mut Cx,
    ) {
        let Some(state) = self.tab(tab) else {
            return reply.stale();
        };
        let Some(repository) = state.repository.as_ref() else {
            return reply.stale();
        };
        let root = repository.root.clone();
        let jj = repository.vcs == Vcs::Jj;
        // The reads below go through jj at the handle's operation.
        let at = || {
            if !jj {
                Err(
                    "This is a git repository: only bookmarks, remotes and the selection can be read",
                )
            } else if op.is_empty() {
                Err("The repository is still loading")
            } else {
                Ok(At {
                    root: root.clone(),
                    op: op.clone(),
                })
            }
        };
        match call {
            RepoCall::Bookmarks => {
                let bookmarks: Vec<Bookmark> = state
                    .session
                    .bookmarks
                    .bookmarks
                    .iter()
                    .map(|entry| Bookmark {
                        name: entry.name.clone(),
                        targets: entry.local_targets.clone(),
                        remotes: entry
                            .remotes
                            .iter()
                            .map(|remote| RemoteBookmark {
                                remote: remote.remote.clone(),
                                target: remote.target.clone(),
                                tracked: remote.tracked,
                            })
                            .collect(),
                    })
                    .collect();
                reply.ok(bookmarks)
            }
            RepoCall::Remotes => {
                let remotes: Vec<Remote> = diffui_core::forge::remotes(&root)
                    .into_iter()
                    .map(|(name, url)| Remote {
                        name,
                        url: Some(url),
                    })
                    .collect();
                reply.ok(remotes)
            }
            RepoCall::Selection => {
                let session = &state.session;
                let reference = |row: diffui_core::RowView<'_>| RevisionRef {
                    change_id: row.change_id().to_owned(),
                    commit_id: row.commit_id().to_owned(),
                };
                let mut revisions: Vec<RevisionRef> = session
                    .commits
                    .find(&session.selected_revision)
                    .map(reference)
                    .into_iter()
                    .collect();
                for marked in &state.revision_multi_selection {
                    if let Some(row) = session.commits.find_by_commit_id(marked)
                        && !revisions.iter().any(|seen| seen.commit_id == *marked)
                    {
                        revisions.push(reference(row));
                    }
                }
                reply.ok(Selection {
                    revisions,
                    path: session
                        .document
                        .files
                        .get(state.selected_file)
                        .map(|file| file.path.clone()),
                })
            }
            RepoCall::Rows { revset, limit } => match at() {
                Ok(at) => {
                    let line = query::rows_line(&at, &revset, limit);
                    read(at, line, reply, |ran| {
                        if ran.ok {
                            query::parse_rows(&ran.stdout).map(|rows| serde_json::json!(rows))
                        } else {
                            Err(ran.stderr)
                        }
                    })
                }
                Err(why) => reply.err(why),
            },
            RepoCall::Diff {
                revset,
                format,
                max_bytes,
            } => match at() {
                Ok(at) => {
                    let line = query::diff_line(&at, &revset, format);
                    read(at, line, reply, move |ran| {
                        if ran.ok {
                            Ok(serde_json::json!(query::cut_diff(ran.stdout, max_bytes)))
                        } else {
                            Err(ran.stderr)
                        }
                    })
                }
                Err(why) => reply.err(why),
            },
            RepoCall::ReadFile { rev, path } => match at() {
                Ok(at) => {
                    let line = query::file_line(&at, &rev, &path);
                    read(at, line, reply, |ran| {
                        if ran.ok {
                            Ok(serde_json::json!(ran.stdout))
                        } else if query::is_missing_file(&ran.stderr) {
                            Ok(serde_json::Value::Null)
                        } else {
                            Err(ran.stderr)
                        }
                    })
                }
                Err(why) => reply.err(why),
            },
            RepoCall::JjQuery { args } => match at().map_err(str::to_owned).and_then(|at| {
                let line = query::query_line(&at, &args)?;
                Ok((at, line))
            }) {
                Ok((at, line)) => read(at, line, reply, |ran| {
                    if ran.ok {
                        Ok(serde_json::json!(ran.stdout))
                    } else {
                        Err(ran.stderr)
                    }
                }),
                Err(why) => reply.err(why),
            },
            RepoCall::JjRun { args } => {
                if !jj {
                    return reply.err("This is a git repository: there is no jj to run");
                }
                let line = crate::user_commands::jj_line(&args);
                let reply = std::rc::Rc::new(std::cell::Cell::new(Some(reply)));
                let answer = {
                    let reply = reply.clone();
                    move |outcome: Result<String, String>| {
                        if let Some(reply) = reply.take() {
                            match outcome {
                                Ok(summary) => reply.ok(JjRun { summary }),
                                Err(error) => reply.err(error),
                            }
                        }
                    }
                };
                let planned = answer.clone();
                let started = self.run_jj_line(
                    tab,
                    &line,
                    Some(Box::new(move |_, outcome| {
                        planned(outcome.map_err(|error| error.0))
                    })),
                    cx,
                );
                if let Err(error) = started {
                    answer(Err(error.0));
                }
            }
        }
    }

    fn answer_ui_call(&mut self, id: &str, call: UiCall, reply: Reply, cx: &mut Cx) {
        let name = self
            .plugins
            .get(id)
            .map_or_else(|| id.to_owned(), |found| found.name.clone());
        match call {
            UiCall::Toast { message, tone } => {
                match tone {
                    Tone::Info => self.push_info_toast(name, &message),
                    Tone::Error => self.push_error_toast(name, &message),
                }
                reply.ok(())
            }
            UiCall::ActivityStart { label } => {
                // A row lives in one tab's log: the one on screen when the
                // work starts. What follows goes to that tab by its number.
                let Some(tab) = self.active_tab_id() else {
                    return reply.err("No repository is open to show an activity in");
                };
                let (activity, _) = self.begin_activity(tab, Prose::from(label), false);
                self.plugins.next_activity += 1;
                let number = self.plugins.next_activity;
                self.plugins
                    .activities
                    .insert(number, (id.to_owned(), tab, activity));
                reply.ok(number)
            }
            UiCall::ActivityAppend { activity, text } => {
                if let Some((_, tab, row)) = self.plugins.activities.get(&activity).cloned()
                    && let Some(log) = self.activity_log_for(tab)
                {
                    log.extend_output(row, text.lines().map(str::to_owned));
                }
                reply.ok(())
            }
            UiCall::ActivityFinish { activity, error } => {
                if let Some((_, tab, row)) = self.plugins.activities.remove(&activity)
                    && let Some(log) = self.activity_log_for(tab)
                {
                    let status = match &error {
                        Some(_) => ActivityStatus::Error,
                        None => ActivityStatus::Done,
                    };
                    log.finish(row, status, error);
                }
                reply.ok(())
            }
            UiCall::PromptInput {
                label,
                value,
                placeholder,
            } => {
                let Some(id) = self.ask_for_plugin(AskKind::Input, reply) else {
                    return;
                };
                self.ask_input(
                    id,
                    &label,
                    value.as_deref().unwrap_or_default(),
                    placeholder.as_deref().unwrap_or_default(),
                    cx,
                );
            }
            UiCall::PromptConfirm {
                title,
                body,
                confirm_label,
                destructive,
            } => {
                let Some(id) = self.ask_for_plugin(AskKind::Confirm, reply) else {
                    return;
                };
                self.ask(crate::ConfirmDialog {
                    title: Prose::from(title),
                    body: body.map(Prose::from).unwrap_or_default(),
                    confirm_label: confirm_label.unwrap_or_else(|| "OK".to_owned()),
                    look: crate::ConfirmLook {
                        routine: !destructive,
                        ..Default::default()
                    },
                    pending: crate::Held::Command(id),
                });
            }
            UiCall::PromptChoose { label, choices } => {
                if choices.is_empty() {
                    return reply.err("ui.prompt.choose needs at least one choice");
                }
                let labels = choices.iter().map(|choice| choice.label.clone()).collect();
                let Some(id) = self.ask_for_plugin(AskKind::Choose(choices), reply) else {
                    return;
                };
                self.open_choices(id, &label, labels, cx);
            }
            UiCall::OpenUrl { url } => {
                if url.starts_with("http://") || url.starts_with("https://") {
                    crate::open_url(&url);
                    reply.ok(())
                } else {
                    reply.err("ui.openUrl opens http and https links only")
                }
            }
            UiCall::ClipboardWrite { text } => {
                crate::write_clipboard(text, cx);
                reply.ok(())
            }
            UiCall::StatusSet { text } => {
                match text.map(|text| text.trim().to_owned()) {
                    Some(text) if !text.is_empty() => {
                        self.plugins.status.insert(id.to_owned(), text);
                    }
                    _ => {
                        self.plugins.status.remove(id);
                    }
                }
                reply.ok(())
            }
        }
    }

    /// The plugin `id` stopped, failed or was reloaded: the activity rows its
    /// script left running end here, because nothing will finish them now.
    pub(super) fn end_plugin_activities(&mut self, id: &str) {
        let left: Vec<u64> = self
            .plugins
            .activities
            .iter()
            .filter(|(_, (plugin, ..))| plugin == id)
            .map(|(number, _)| *number)
            .collect();
        for number in left {
            if let Some((_, tab, row)) = self.plugins.activities.remove(&number)
                && let Some(log) = self.activity_log_for(tab)
            {
                log.finish(
                    row,
                    ActivityStatus::Cancelled,
                    Some("The plugin stopped".to_owned()),
                );
            }
        }
    }

    /// Take the prompt slot for a plugin: whatever was asking is dismissed.
    /// `None` (and `reply` is answered) when there is no tab to ask over.
    fn ask_for_plugin(&mut self, kind: AskKind, reply: Reply) -> Option<RunId> {
        let Some(tab) = self.active_tab_id() else {
            reply.err("No repository is open to ask over");
            return None;
        };
        if let Some(previous) = self.plugins.asking.take() {
            previous.dismiss();
        }
        let id = self.user_runs.mint(tab);
        self.plugins.asking = Some(Asking { id, reply, kind });
        Some(id)
    }

    /// The prompt `id` was answered (`None`: dismissed). `Err` with the
    /// answer when `id` isn't a plugin's prompt.
    pub(crate) fn plugin_prompt_answered(
        &mut self,
        id: RunId,
        answer: Option<String>,
    ) -> Result<(), Option<String>> {
        let Some(asking) = self.plugins.asking.take_if(|asking| asking.id == id) else {
            return Err(answer);
        };
        match (answer, &asking.kind) {
            (None, _) => asking.dismiss(),
            (Some(_), AskKind::Confirm) => asking.reply.ok(true),
            (Some(text), AskKind::Input) => asking.reply.ok(text),
            (Some(label), AskKind::Choose(choices)) => {
                let value = choices
                    .iter()
                    .find(|choice| choice.label == label)
                    .map_or(label, |choice| choice.value.clone());
                asking.reply.ok(value)
            }
        }
        Ok(())
    }

    /// The hint of the choice labelled `label` in the plugin prompt on
    /// screen, if it has one.
    pub(crate) fn plugin_choice_hint(&self, label: &str) -> Option<String> {
        let AskKind::Choose(choices) = &self.plugins.asking.as_ref()?.kind else {
            return None;
        };
        choices
            .iter()
            .find(|choice| choice.label == label)?
            .hint
            .clone()
    }

    /// Dismiss a plugin's prompt whose dialog is no longer on screen: closed
    /// with Esc, replaced by another, or gone with its tab.
    pub(super) fn sweep_plugin_prompt(&mut self) {
        let Some(asking) = &self.plugins.asking else {
            return;
        };
        let id = asking.id;
        let shown = self.modes.iter().any(|mode| match (&asking.kind, mode) {
            (AskKind::Input, Mode::NamePrompt(prompt)) => matches!(
                prompt.kind,
                crate::bookmarks::PromptKind::Value { run, .. } if run == id
            ),
            (AskKind::Confirm, Mode::Confirm(dialog)) => {
                matches!(dialog.pending, crate::Held::Command(run) if run == id)
            }
            (AskKind::Choose(_), Mode::Palette(palette)) => palette.chooses_for(id),
            _ => false,
        });
        if !shown && let Some(asking) = self.plugins.asking.take() {
            asking.dismiss();
        }
    }
}

/// Run `line` through jj off this thread and answer `reply` with what `read`
/// makes of it.
fn read(
    at: At,
    line: Vec<String>,
    reply: Reply,
    read: impl FnOnce(query::Ran) -> Result<serde_json::Value, String> + Send + 'static,
) {
    crate::spawn::runtime().spawn(async move {
        match query::run(at, line).await.and_then(read) {
            Ok(value) => reply.ok(value),
            Err(error) => reply.err(error),
        }
    });
}
