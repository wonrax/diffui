//! Rebase onto trunk, in one go: `jj rebase -b X -o trunk()`.
//!
//! It applies straight away, since undo is one key away. The one thing worth
//! stopping for is a forecast conflict, so the rebase preview runs first and
//! a confirmation names the changes that would end up conflicted.

use iced::Task;

use diffui_core::{
    Destination, DraftSimulation, JobId, PreviewRequest, RebaseSourceMode, RevisionSelection,
};

use crate::commands::{self, CommandArg};
use crate::modes::Mode;
use crate::mutations::MutationOp;
use crate::prose::Prose;
use crate::{ConfirmDialog, Diffui, Message, PendingMutation, activity};

/// Why X's branch can't be rebased onto trunk, or `None` when it can.
pub(crate) fn blocker(ui: &Diffui, arg: &CommandArg) -> Option<&'static str> {
    let session = &ui.active().session;
    let target = commands::target_revision(ui, arg)?;
    let row = match &target {
        RevisionSelection::WorkingCopy => session.commits.working_copy(),
        RevisionSelection::Commit(hex) => session.commits.find_by_commit_id(hex),
    };
    let Some(row) = row else {
        return Some("Revision isn't in the log");
    };
    if session.bookmarks.trunk.is_none() {
        return Some("trunk() doesn't resolve to a bookmark");
    }
    if row.is_immutable() {
        return Some("Revision is immutable");
    }
    if row.on_trunk() {
        return Some("Already on trunk");
    }
    None
}

/// The op the command runs: the whole branch `source` sits on, onto trunk.
fn rebase_op(source: RevisionSelection, trunk: String) -> MutationOp {
    MutationOp::Rebase {
        mode: RebaseSourceMode::Branch,
        sources: vec![source],
        destination: Destination::Onto(RevisionSelection::Commit(trunk)),
    }
}

impl Diffui {
    /// Preview the rebase, then run it (or ask first, if it would conflict).
    pub(crate) fn rebase_onto_trunk(&mut self, source: RevisionSelection) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let Some(trunk) = self.active().session.bookmarks.trunk.clone() else {
            return Task::none();
        };
        let op = rebase_op(source.clone(), trunk.clone());
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        let mut effects = Vec::new();
        if let Some((previous, _)) = state.jobs.trunk_rebase.take() {
            effects.push(diffui_core::Effect::Send(diffui_core::Command::Cancel {
                job: previous,
            }));
        }
        let (job, preview) = state.session.preview(PreviewRequest::Rebase {
            mode: RebaseSourceMode::Branch,
            sources: vec![source],
            destination: Destination::Onto(RevisionSelection::Commit(trunk)),
        });
        state.jobs.trunk_rebase = Some((job, op));
        effects.extend(preview);
        self.run_effects(tab, effects)
    }

    /// The preview for a rebase onto trunk came back (`Err`: it failed, which
    /// only costs the forecast — the rebase still runs and reports for itself).
    pub(crate) fn finish_trunk_preview(
        &mut self,
        tab: crate::TabId,
        job: JobId,
        simulation: Result<&DraftSimulation, String>,
    ) -> Task<Message> {
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        let Some((_, op)) = state.jobs.trunk_rebase.take_if(|(id, _)| *id == job) else {
            return Task::none();
        };
        let conflicts = match simulation {
            Ok(DraftSimulation::Rebase(preview)) => preview.new_conflicts.clone(),
            Ok(_) => Vec::new(),
            Err(error) => {
                tracing::warn!("rebase onto trunk: preview failed: {error}");
                Vec::new()
            }
        };
        if conflicts.is_empty() || self.active_tab_id() != Some(tab) {
            return self.start_mutation_op(op);
        }
        let MutationOp::Rebase { sources, .. } = &op else {
            return Task::none();
        };
        let title = Prose::from("Rebase ")
            .then(self.revision_prose(&sources[0]))
            .text(" onto trunk?");
        let mut body = Prose::from(if conflicts.len() == 1 {
            "This leaves "
        } else {
            "This leaves these conflicted: "
        });
        for (index, change) in conflicts.iter().enumerate() {
            if index > 0 {
                body = body.text(", ");
            }
            body = body.rev(change.clone());
        }
        if conflicts.len() == 1 {
            body = body.text(" conflicted");
        }
        body = body.text(". Undo takes it back.");
        let (activity_id, _) = self.begin_activity(tab, "Rebase onto trunk", false);
        if let Some(log) = self.activity_log_for(tab) {
            log.set_status(activity_id, activity::ActivityStatus::Queued);
        }
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title,
            body,
            confirm_label: "Rebase".to_owned(),
            pending: PendingMutation {
                op,
                tab_id: tab,
                activity_id,
                allow_immutable: false,
            },
        }));
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};
    use diffui_core::{CommitStoreBuilder, CommitSummary};

    fn row(id: &str, on_trunk: bool, is_immutable: bool) -> CommitSummary {
        CommitSummary {
            change_id: id.repeat(4),
            commit_id: id.repeat(8),
            shortest_change_id_len: None,
            description: String::new(),
            author: String::new(),
            has_description: false,
            is_empty: None,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: false,
            is_immutable,
            on_trunk,
            bookmarks: Vec::new(),
            parent_ids: Vec::new(),
        }
    }

    /// "s" sits on trunk "t", "f" is a feature branch off older history,
    /// "o" is old immutable history.
    fn ui(trunk: Option<&str>) -> Diffui {
        let mut ui = app();
        push_tab(&mut ui, "/tmp/trunk");
        let tab = ui.active_mut();
        tab.session.capabilities.mutate = true;
        tab.session.bookmarks.trunk = trunk.map(|id| id.repeat(8));
        let mut builder = CommitStoreBuilder::with_capacity(4);
        for summary in [
            row("s", true, false),
            row("t", true, true),
            row("f", false, false),
            row("o", false, true),
        ] {
            builder.push(summary);
        }
        tab.session.commits = builder.finish();
        ui
    }

    fn on(id: &str) -> CommandArg {
        CommandArg::Revision(RevisionSelection::Commit(id.repeat(8)))
    }

    #[test]
    fn says_why_a_branch_cannot_move_onto_trunk() {
        let ui = ui(Some("t"));
        assert_eq!(blocker(&ui, &on("f")), None);
        assert_eq!(blocker(&ui, &on("s")), Some("Already on trunk"));
        assert_eq!(blocker(&ui, &on("t")), Some("Revision is immutable"));
        assert_eq!(blocker(&ui, &on("o")), Some("Revision is immutable"));
        assert_eq!(blocker(&ui, &on("x")), Some("Revision isn't in the log"));

        let no_trunk = super::tests::ui(None);
        assert_eq!(
            blocker(&no_trunk, &on("f")),
            Some("trunk() doesn't resolve to a bookmark")
        );
    }

    #[test]
    fn the_op_moves_the_whole_branch_onto_trunk() {
        let source = RevisionSelection::Commit("f".repeat(8));
        let MutationOp::Rebase {
            mode,
            sources,
            destination,
        } = rebase_op(source.clone(), "t".repeat(8))
        else {
            panic!("a rebase");
        };
        assert_eq!(mode, RebaseSourceMode::Branch);
        assert_eq!(sources, vec![source]);
        assert_eq!(
            destination,
            Destination::Onto(RevisionSelection::Commit("t".repeat(8)))
        );
    }
}
