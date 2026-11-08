//! Rebase onto trunk, in one go: `jj rebase -b X -o trunk()`.
//!
//! It applies straight away, since undo is one key away. The one thing worth
//! stopping for is a forecast conflict, so the rebase preview runs first and
//! a confirmation names the changes that would end up conflicted.

use diffui_core::{
    Destination, DraftSimulation, JobId, PreviewRequest, RebaseSourceMode, RevisionSelection,
};

use crate::commands::{self, CommandArg};
use crate::modes::Mode;
use crate::mutations::MutationOp;
use crate::prose::Prose;
use crate::{ConfirmDialog, Diffui, PendingMutation, activity};

/// Why X's branch can't be rebased onto trunk, or `None` when it can.
pub(crate) fn blocker(ui: &Diffui, arg: &CommandArg) -> Option<&'static str> {
    let targets = match arg {
        CommandArg::Revisions(targets) => targets.clone(),
        _ => commands::target_revision(ui, arg).into_iter().collect(),
    };
    blocker_for(ui, &targets)
}

/// [`blocker`] over a batch: the first marked revision that would refuse
/// blocks the whole gesture, with the same wording a single one gets.
pub(crate) fn blocker_for(ui: &Diffui, targets: &[RevisionSelection]) -> Option<&'static str> {
    if targets.is_empty() {
        return Some("No revisions marked");
    }
    let session = &ui.active().session;
    if session.bookmarks.trunk.is_none() {
        return Some("trunk() doesn't resolve to a bookmark");
    }
    for target in targets {
        let row = match target {
            RevisionSelection::WorkingCopy => session.commits.working_copy(),
            RevisionSelection::Commit(hex) => session.commits.find_by_commit_id(hex),
        };
        let Some(row) = row else {
            return Some("Revision isn't in the log");
        };
        if row.is_immutable() {
            return Some("Revision is immutable");
        }
        if row.on_trunk() {
            return Some("Already on trunk");
        }
    }
    None
}

/// The op the command runs: the whole branches the `sources` sit on, onto
/// trunk.
fn rebase_op(sources: Vec<RevisionSelection>, trunk: String) -> MutationOp {
    MutationOp::Rebase {
        mode: RebaseSourceMode::Branch,
        sources,
        destination: Destination::Onto(RevisionSelection::Commit(trunk)),
    }
}

impl Diffui {
    /// Preview the rebase, then run it (or ask first, if it would conflict).
    pub(crate) fn rebase_onto_trunk(&mut self, sources: Vec<RevisionSelection>) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let Some(trunk) = self.active().session.bookmarks.trunk.clone() else {
            return;
        };
        let op = rebase_op(sources.clone(), trunk.clone());
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let mut effects = Vec::new();
        if let Some((previous, _)) = state.jobs.trunk_rebase.take() {
            effects.push(diffui_core::Effect::Send(diffui_core::Command::Cancel {
                job: previous,
            }));
        }
        let (job, preview) = state.session.preview(PreviewRequest::Rebase {
            mode: RebaseSourceMode::Branch,
            sources,
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
    ) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some((_, op)) = state.jobs.trunk_rebase.take_if(|(id, _)| *id == job) else {
            return;
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
            return;
        };
        let title = if let [source] = sources.as_slice() {
            Prose::from("Rebase ")
                .then(self.revision_prose(source))
                .text(" onto trunk?")
        } else {
            Prose::from(format!(
                "Rebase the branches of {} revisions onto trunk?",
                sources.len()
            ))
        };
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
            look: Default::default(),
            pending: PendingMutation {
                op,
                tab_id: tab,
                activity_id,
                allow_immutable: false,
            },
        }));
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
            author_timestamp: None,
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
    fn ui(trunk: Option<&str>, _cx: &mut crate::Cx) -> Diffui {
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
        crate::tabs::tests::with_cx(|cx| {
            let ui = ui(Some("t"), cx);
            assert_eq!(blocker(&ui, &on("f")), None);
            assert_eq!(blocker(&ui, &on("s")), Some("Already on trunk"));
            assert_eq!(blocker(&ui, &on("t")), Some("Revision is immutable"));
            assert_eq!(blocker(&ui, &on("o")), Some("Revision is immutable"));
            assert_eq!(blocker(&ui, &on("x")), Some("Revision isn't in the log"));

            let no_trunk = super::tests::ui(None, cx);
            assert_eq!(
                blocker(&no_trunk, &on("f")),
                Some("trunk() doesn't resolve to a bookmark")
            );
        });
    }

    #[test]
    fn the_op_moves_the_whole_branch_onto_trunk() {
        let source = RevisionSelection::Commit("f".repeat(8));
        let MutationOp::Rebase {
            mode,
            sources,
            destination,
        } = rebase_op(vec![source.clone()], "t".repeat(8))
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

    #[test]
    fn a_batch_blocks_on_the_first_revision_that_cannot_move() {
        crate::tabs::tests::with_cx(|cx| {
            let ui = ui(Some("t"), cx);
            let f = RevisionSelection::Commit("f".repeat(8));
            let s = RevisionSelection::Commit("s".repeat(8));
            let o = RevisionSelection::Commit("o".repeat(8));

            assert_eq!(blocker_for(&ui, std::slice::from_ref(&f)), None);
            assert_eq!(blocker_for(&ui, &[]), Some("No revisions marked"));
            assert_eq!(blocker_for(&ui, &[f, s]), Some("Already on trunk"));
            assert_eq!(blocker_for(&ui, &[o]), Some("Revision is immutable"));
        });
    }
}
