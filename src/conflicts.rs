//! Resolving conflicted files from the diff pane: take a side or a base of
//! the jj conflict, or run the user's `ui.merge-editor` and write its output
//! back (`jj resolve`).
//!
//! The conflicted paths (with jj's labels for their terms) are fetched once
//! per loaded document of a conflicted revision. The merge tool runs on a
//! blocking thread here, never in the repository actor, so a merge session
//! left open for minutes doesn't stall the tab; its result lands as an
//! ordinary [`MutationOp::ResolveConflict`].

use diffui_core::{
    ConflictFile, ConflictResolution, DiffFileStatus, JobId, MergeToolInvocation, MutationOp,
    RevisionSelection,
};
use iced::widget::{column, container, row, text};
use iced::{Element, Length, Padding, Task, alignment};

use crate::changes::{ChangesMsg, editable};
use crate::commands::CommandArg;
use crate::theme::{ThemeSpec, text_size};
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
}

/// Conflicts of one loaded document.
#[derive(Debug, Clone)]
struct LoadedConflicts {
    revision: RevisionSelection,
    document_id: u64,
    files: Vec<ConflictFile>,
}

/// Per-tab conflict-resolution state.
#[derive(Debug, Default)]
pub(crate) struct ConflictsState {
    loaded: Option<LoadedConflicts>,
    job: Option<(JobId, RevisionSelection, u64)>,
    tool_job: Option<(JobId, String)>,
    /// Path whose merge tool is open.
    tool_running: Option<String>,
}

impl ConflictsState {
    /// Whether `job` is one of ours (event routing asks).
    pub(crate) fn owns(&self, job: JobId) -> bool {
        self.job.as_ref().is_some_and(|(id, ..)| *id == job)
            || self.tool_job.as_ref().is_some_and(|(id, _)| *id == job)
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

impl Diffui {
    pub(crate) fn on_conflict(&mut self, msg: ConflictMsg) -> Task<Message> {
        match msg {
            ConflictMsg::Resolve { path, resolution } => {
                if !editable(self.active()) {
                    return Task::none();
                }
                let target = self.active().session.selected_revision.clone();
                self.start_changes_op(MutationOp::ResolveConflict {
                    target,
                    path,
                    resolution,
                })
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
                        let label = format!("Resolve {}", invocation.path);
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
    fn resolve_in(&mut self, tab: TabId, op: MutationOp, label: String) -> Task<Message> {
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
                    format!("Resolving {path} in {}", invocation.tool),
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
            Payload::Failed { error, .. } => {
                let tool = conflicts.tool_job.take_if(|(id, _)| *id == job);
                conflicts.job.take_if(|(id, ..)| *id == job);
                if tool.is_some() {
                    self.push_error_toast("Merge tool".to_owned(), &error.to_string());
                }
                Task::none()
            }
            _ => {
                conflicts.job.take_if(|(id, ..)| *id == job);
                conflicts.tool_job.take_if(|(id, _)| *id == job);
                Task::none()
            }
        })
    }
}

/// A short name for a conflict term: the quoted description jj puts in its
/// labels (`nlqwxzwn 7dd24e73 "subject"`), else the label itself.
fn term_name(label: &str) -> String {
    let name = match (label.find('"'), label.rfind('"')) {
        (Some(start), Some(end)) if end > start + 1 => &label[start + 1..end],
        _ => label,
    };
    let mut short: String = name.chars().take(28).collect();
    if name.chars().count() > 28 {
        short.push('\u{2026}');
    }
    short
}

fn resolve(path: &str, resolution: ConflictResolution) -> Message {
    Message::Changes(ChangesMsg::Conflict(ConflictMsg::Resolve {
        path: path.to_owned(),
        resolution,
    }))
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
            let deleted = if side.absent { " (deleted)" } else { "" };
            let button = ui::text_button(
                None,
                format!("Side {}: {}{deleted}", index + 1, term_name(&side.label)),
                ui::Variant::Raised,
                ui::Size::Compact,
                font,
                theme,
            )
            .on_press(resolve(path, ConflictResolution::Side(index)));
            buttons = buttons.push(tip(
                button.into(),
                format!("Take side {} as the file: {}", index + 1, side.label),
            ));
        }
        for (index, base) in conflict.bases.iter().enumerate() {
            let label = if conflict.bases.len() == 1 {
                "Base".to_owned()
            } else {
                format!("Base {}", index + 1)
            };
            let button = ui::text_button(
                None,
                label,
                ui::Variant::Ghost,
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
            "Open in merge tool"
        },
        ui::Variant::Raised,
        ui::Size::Compact,
        font,
        theme,
    );
    if conflict.merge_tool_blocker.is_none() && !running {
        tool = tool.on_press(Message::Changes(ChangesMsg::Conflict(
            ConflictMsg::OpenMergeTool(path.to_owned()),
        )));
    }
    buttons = buttons.push(tip(
        tool.into(),
        conflict.merge_tool_blocker.clone().unwrap_or_else(|| {
            "Runs ui.merge-editor from your jj config and writes the result back".to_owned()
        }),
    ));

    let headline = row![
        icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.conflict_marker),
        text(format!(
            "{path} is conflicted ({} sides) \u{2014} take:",
            conflict.sides.len()
        ))
        .size(text_size::UI)
        .font(font)
        .color(theme.text),
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    let caption = |content: &'static str| {
        text(content)
            .size(text_size::CAPTION)
            .font(font)
            .color(theme.muted_text)
    };
    let hint: Element<'a, Message> = if matches!(
        tab.session.selected_revision,
        RevisionSelection::WorkingCopy
    ) {
        caption(
            "Or edit the conflict markers in the file: jj records it as resolved once the \
                 markers are gone.",
        )
        .into()
    } else {
        row![
            caption(
                "Or by hand: start a new change on top, edit the conflict markers there, \
                     then squash it back.",
            ),
            ui::text_button(
                None,
                "New change on top",
                ui::Variant::Ghost,
                ui::Size::Compact,
                font,
                theme
            )
            .on_press(Message::Action(Action::Mutate(MutationOp::New {
                parent: tab.session.selected_revision.clone(),
            }))),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center)
        .wrap()
        .into()
    };
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
    fn term_names_prefer_the_quoted_subject() {
        assert_eq!(term_name("nlqwxzwn 7dd24e73 \"left edit\""), "left edit");
        assert_eq!(term_name("side #1"), "side #1");
    }
}
