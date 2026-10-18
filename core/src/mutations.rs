//! Revision mutations — `new`, `edit`, `abandon`, `rebase`, `squash`, ….
//!
//! Only the op types and the drafts that build them live here; the jj-lib
//! execution belongs to the repository actor, which runs it inside the one
//! working-copy lock so a mutation that moves `@` also updates the files on
//! disk (and doesn't lose uncommitted work).

use crate::model::RevisionSelection;
use crate::partial::DiffSelection;
use crate::resolve::ConflictResolution;

/// Which commits a rebase moves, relative to the picked source revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseSourceMode {
    /// Only the picked revisions (`jj rebase -r`) — their descendants stay
    /// behind, re-parented across the hole.
    Revisions,
    /// The picked revisions and every descendant (`jj rebase -s`).
    WithDescendants,
    /// The whole branch the picked revision sits on (`jj rebase -b`): every
    /// commit reachable from it but not from the destination, moved from its
    /// fork-point roots — so pointing at the branch head (or any commit on
    /// it) moves the entire branch without hunting for its first commit.
    /// The moved set depends on the destination, so it's resolved at
    /// execution/preview time, not draft time.
    Branch,
}

/// Where moved (or duplicated) commits land relative to a target revision.
/// Mirrors `jj rebase`'s `-d` / `-A` / `-B` and the combined `-A x -B y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// New children of the target (`jj rebase -d`); the target's existing
    /// children are unaffected.
    Onto(RevisionSelection),
    /// Between the target and all of its current children (`jj rebase -A`).
    After(RevisionSelection),
    /// Between the target's parents and the target (`jj rebase -B`).
    Before(RevisionSelection),
    /// Exactly into the edge between two revisions
    /// (`jj rebase -A parent -B child`) — the drag-into-a-gap gesture.
    Between {
        parent: RevisionSelection,
        child: RevisionSelection,
    },
}

impl Destination {
    /// The revision the destination is anchored on — what target-mode
    /// highlights and preview summaries name.
    pub fn anchor(&self) -> &RevisionSelection {
        match self {
            Self::Onto(target) | Self::After(target) | Self::Before(target) => target,
            Self::Between { parent, .. } => parent,
        }
    }
}

/// Where a squash folds its source into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SquashTarget {
    /// The source's single parent (`jj squash` with no `--into`). Errors on a
    /// merge commit — an explicit destination is required there.
    Parent,
    Revision(RevisionSelection),
}

/// A revision mutation. Targets are `RevisionSelection`s so the working-copy
/// case (which has no stable change-id) stays representable.
#[derive(Debug, Clone)]
pub enum MutationOp {
    /// Create a new child commit of `parent` and move `@` onto it (`jj new`).
    New { parent: RevisionSelection },
    /// Move the working copy to `target` (`jj edit`).
    Edit { target: RevisionSelection },
    /// Discard the `targets`, re-parenting their descendants onto their
    /// parents (`jj abandon`). One transaction for the whole set, so a batch
    /// abandon is a single op-log entry (and a single undo).
    Abandon { targets: Vec<RevisionSelection> },
    /// Replace the full description of `target` (`jj describe -r <target>`).
    Describe {
        target: RevisionSelection,
        description: String,
    },
    /// Move revisions to a new location in the graph (`jj rebase`).
    Rebase {
        mode: RebaseSourceMode,
        sources: Vec<RevisionSelection>,
        destination: Destination,
    },
    /// Fold the `from` revisions' changes into another revision and
    /// (normally) abandon the emptied sources (`jj squash --from … --into`).
    /// Descriptions are combined: destination's, then each source's, joined
    /// by blank lines. [`SquashTarget::Parent`] needs exactly one source
    /// (whose parent would it be otherwise).
    Squash {
        from: Vec<RevisionSelection>,
        into: SquashTarget,
    },
    /// Create a merge commit of `parents` and move `@` onto it
    /// (`jj new p1 p2 …`). Needs at least two distinct parents.
    Merge { parents: Vec<RevisionSelection> },
    /// Copy `target` onto its own parents (`jj duplicate`).
    Duplicate { target: RevisionSelection },
    /// Move `from`'s hunks into the mutable ancestors that last touched those
    /// lines (`jj absorb --from`).
    Absorb { from: RevisionSelection },
    /// Point local bookmark `name` at `to`, creating it if needed
    /// (`jj bookmark set -r <to> <name>`). With `push_remote`, the same
    /// transaction then pushes the bookmark there (`jj git push -b <name>`)
    /// — one activity, and a failed push rolls the move back too, so the
    /// action never half-lands.
    MoveBookmark {
        name: String,
        to: RevisionSelection,
        push_remote: Option<String>,
    },
    /// Delete local bookmark `name` (`jj bookmark delete <name>`).
    DeleteBookmark { name: String },
    /// Start tracking remote bookmark `name@remote`
    /// (`jj bookmark track <name>@<remote>`).
    TrackBookmark { name: String, remote: String },
    /// Push local bookmark `name` to `remote` (`jj git push -b <name>`).
    PushBookmark { name: String, remote: String },
    /// `None`: `jj undo` — step back one operation, walking further back on
    /// each repeat (jj 0.45's undo stack, recorded in the op descriptions so
    /// the CLI and diffui share it). Snapshot ops are skipped, and file edits
    /// they recorded since the undone op are carried onto the restored `@`.
    ///
    /// `Some(op)`: `jj op revert <op>` — undo exactly that operation and keep
    /// everything since (an activity row's per-operation Undo).
    ///
    /// Routed through the same mutation pipeline as everything else so it
    /// serializes behind the queue and gets the snapshot-before-mutate
    /// discipline (an undo that moves `@` must not clobber unsnapshotted
    /// on-disk edits).
    Undo { operation_id: Option<String> },
    /// `jj redo`: re-apply what the latest undo took back; repeated, walk
    /// forward through earlier undos. Fails with "nothing to redo" once a real
    /// operation has landed on top of the undos.
    Redo,
    /// Create local bookmark `name` at `to` (`jj bookmark create -r <to>
    /// <name>`). Refuses an existing name — moving one is `MoveBookmark`.
    CreateBookmark { name: String, to: RevisionSelection },
    /// `jj bookmark rename <old> <new>`.
    RenameBookmark { old: String, new: String },
    /// `jj bookmark forget <name>`: drop it locally and stop tracking its
    /// remotes, so no later push deletes it there.
    ForgetBookmark { name: String },
    /// `jj bookmark untrack <name>@<remote>`.
    UntrackBookmark { name: String, remote: String },
    /// `jj git push` with the CLI's checks: new bookmarks, `--change`,
    /// deletions, and every allowance the user accepted.
    Push(crate::push::PushRequest),
    /// Describe `@` and start a new empty child as the new `@`, in one
    /// operation (`jj commit -m <description>`). The new `@` keeps the
    /// committed tree, so nothing on disk changes.
    Commit { description: String },
    /// A new empty change placed at `destination` with `@` moved onto it
    /// (`jj new -A/-B`, or `jj new -A x -B y` for a gap). `After` / `Before`
    /// / `Between` rebase the commits on the far side of the insertion onto
    /// the new change, like the CLI; `Onto` is a plain `jj new <target>`.
    NewAt { destination: Destination },
    /// Make the `targets` siblings instead of a chain (`jj parallelize`):
    /// each keeps its content, takes its in-set parents' outside parents,
    /// and the children of the set's heads end up merging them.
    Parallelize { targets: Vec<RevisionSelection> },
    /// Copy the `targets` (`jj duplicate`), onto their own parents when
    /// `destination` is `None`, else at `-d` / `-A` / `-B` of it — in which
    /// case the commits on the far side of an insertion are rebased onto the
    /// copies.
    DuplicateTo {
        targets: Vec<RevisionSelection>,
        destination: Option<Destination>,
    },
    /// Resolve a divergent change by keeping `keep` and abandoning every
    /// other visible commit that shares its change id (`jj abandon` of the
    /// other copies).
    KeepDivergent { keep: RevisionSelection },
    /// Rewrite authorship metadata (`jj metaedit --update-author` /
    /// `--update-author-timestamp`): the author becomes the configured user,
    /// and/or the author timestamp becomes now.
    Metaedit {
        targets: Vec<RevisionSelection>,
        update_author: bool,
        update_author_timestamp: bool,
    },
    // ── Partial changes and conflict resolution ─────────────────────────
    /// `jj split` with the changes picked in the diff pane: `selection` stays
    /// in `target` (same change id, description replaced by
    /// `first_description` when given), everything else moves into a new
    /// child described `second_description`. `target`'s descendants follow
    /// the new child, and a workspace whose `@` was `target` moves onto it.
    Split {
        target: RevisionSelection,
        selection: DiffSelection,
        first_description: Option<String>,
        second_description: String,
    },
    /// `jj squash --from <from> --into <into> <selection>`: move only the
    /// picked changes. Like the CLI, the source is abandoned (descriptions
    /// combined) only when the selection takes all of it.
    SquashChanges {
        from: RevisionSelection,
        selection: DiffSelection,
        into: SquashTarget,
    },
    /// `jj restore --changes-in <target> <selection>`: discard the picked
    /// changes, putting the parent's content back. Undoable through the op
    /// log like everything else.
    RestoreChanges {
        target: RevisionSelection,
        selection: DiffSelection,
    },
    /// `jj resolve <path>` on `target`, with a side, a base, or a merge
    /// tool's output.
    ResolveConflict {
        target: RevisionSelection,
        path: String,
        resolution: ConflictResolution,
    },
    /// Put the repository back to how it was at `operation_id`
    /// (`jj op restore`): heads, bookmarks, tags, remote-tracking refs and
    /// working-copy pointers as recorded there. The operations since stay in
    /// the log, so a restore is itself undoable.
    RestoreOperation { operation_id: String },
    /// Reverse exactly what `operation_id` did while keeping everything since
    /// (`jj op revert`).
    RevertOperation { operation_id: String },
}

/// User-facing summary of a successful mutation — becomes the status message.
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    pub message: String,
    /// Whether the mutation moved `@` (new/edit/abandon). Bookmark ops leave the
    /// working copy where it is, so the UI keeps the user's current selection
    /// instead of snapping back to the working copy.
    pub moved_working_copy: bool,
    /// New commit id for a rewritten target that the frontend may still be
    /// addressing by its old commit id. Lets selection follow the visible
    /// rewritten commit after a description change.
    pub rewritten_commit: Option<String>,
    /// Captured remote/sideband output (push only) — e.g. GitHub's "create a
    /// pull request" hint + URL. Shown in the activity's expanded row. Empty for
    /// local mutations.
    pub output: Vec<String>,
    /// Hex id of the jj operation this mutation committed — the handle the
    /// activity row's per-op "Undo" uses to revert exactly this mutation
    /// (not just the latest op).
    pub operation_id: Option<String>,
}

/// What a draft simulation produced — the op bar renders the kind-specific
/// summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftSimulation {
    Rebase(RebasePreview),
    Merge(MergePreview),
}

/// Predicted outcome of a merge draft: which paths the merged tree would
/// leave conflicted — see `jj::preview_merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePreview {
    /// Repo-relative conflicted paths, capped — see `truncated`.
    pub conflicts: Vec<String>,
    /// More conflicts exist than were listed.
    pub truncated: bool,
}

/// Predicted outcome of a rebase draft — see `jj::preview_rebase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePreview {
    /// Commits in the moved set.
    pub moved: u32,
    /// Descendants rewritten alongside them.
    pub descendants: u32,
    /// Commits the rebase would empty out and abandon.
    pub abandoned_empty: u32,
    /// Short change ids of commits that end up conflicted that weren't
    /// before, sorted.
    pub new_conflicts: Vec<String>,
    /// Short change ids of the moved set's entry points. For branch mode
    /// these are the resolved fork-point roots — the answer to "which branch
    /// is this actually going to move?".
    pub entry_points: Vec<String>,
    /// Commit ids (hex) of every commit in the moved set, so the sidebar can
    /// wash the whole branch/subtree that would move. Empty when the set was
    /// too large to enumerate (`!simulated`).
    pub moved_commit_ids: Vec<String>,
    /// `false` when the affected set was too large to simulate — the counts
    /// are then estimates and `new_conflicts` is unknown, not empty.
    pub simulated: bool,
}

// ── Target-mode drafts ──────────────────────────────────────────────────

/// What kind of op a target-mode draft builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    Rebase {
        mode: RebaseSourceMode,
    },
    Squash,
    /// `jj new A B …`: the sources are the parents in pick order, and a picked
    /// target joins as the last one. Stacking extra sources (space / ⌘-click)
    /// builds an octopus merge, which [`OpDraft::op_from_sources`] lowers
    /// without a target.
    Merge,
    /// `jj new` at the picked target with the chosen placement (`-A`/`-B`).
    /// Has no sources: what lands is a fresh empty change.
    New,
    /// `jj duplicate` the sources at the picked target with the chosen
    /// placement.
    Duplicate,
    /// `jj parallelize` the sources. Complete from the sources alone — the
    /// draft only exists to preview the result and to let space / ⌘-click
    /// adjust the set.
    Parallelize,
}

impl DraftKind {
    /// Whether the draft lands relative to its target with an
    /// onto / after / before placement.
    pub fn uses_placement(self) -> bool {
        matches!(self, Self::Rebase { .. } | Self::New | Self::Duplicate)
    }

    /// Whether confirming needs no destination, only the sources.
    pub fn completes_from_sources(self) -> bool {
        matches!(self, Self::Merge | Self::Parallelize)
    }
}

/// Placement choice while a rebase draft is picking its destination — the op
/// bar's segmented control and the `o`/`a`/`b` keys. Squash ignores it (the
/// target is always the fold-into revision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementKind {
    #[default]
    Onto,
    After,
    Before,
}

/// One draft source, resolved against the loaded graph when the draft starts
/// so validity checks and labels don't re-scan the store per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSource {
    pub selection: RevisionSelection,
    /// Commit id hex — what target-validity compares against.
    pub commit_id: String,
    /// Short change-id label for the op bar ("k1x2…").
    pub label: String,
}

/// An in-progress "pick a destination" interaction: the op kind + sources are
/// fixed; the destination is whatever the user clicks / keyboard-focuses /
/// drops onto. Pure state — every input method (context menu, keyboard,
/// drag & drop) mutates the same draft, and confirmation lowers it into a
/// [`MutationOp`] through [`OpDraft::op_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpDraft {
    pub kind: DraftKind,
    pub sources: Vec<DraftSource>,
    pub placement: PlacementKind,
    /// Keyboard candidate: index into the loaded commit store, or `None`
    /// until the user starts navigating (arrows / j / k).
    pub candidate: Option<usize>,
    /// Squash drafts only: move just these changes of the (single) source
    /// rather than all of it — "Move selected into…".
    /// Boxed: drafts are held in a mode enum that shouldn't grow for this.
    pub selection: Option<Box<DiffSelection>>,
}

impl OpDraft {
    pub fn rebase(mode: RebaseSourceMode, sources: Vec<DraftSource>) -> Self {
        Self {
            kind: DraftKind::Rebase { mode },
            sources,
            placement: PlacementKind::default(),
            candidate: None,
            selection: None,
        }
    }

    pub fn squash(source: DraftSource) -> Self {
        Self {
            kind: DraftKind::Squash,
            sources: vec![source],
            placement: PlacementKind::default(),
            candidate: None,
            selection: None,
        }
    }

    /// A squash draft that moves only `selection` out of `source`.
    pub fn squash_changes(source: DraftSource, selection: DiffSelection) -> Self {
        Self {
            selection: Some(Box::new(selection)),
            ..Self::squash(source)
        }
    }

    /// The squash-draft op for `target`: the whole sources, or just the
    /// selection when there is one.
    fn squash_op(&self, target: RevisionSelection) -> MutationOp {
        match (&self.selection, self.sources.as_slice()) {
            (Some(selection), [source, ..]) => MutationOp::SquashChanges {
                from: source.selection.clone(),
                selection: (**selection).clone(),
                into: SquashTarget::Revision(target),
            },
            _ => MutationOp::Squash {
                from: self.sources.iter().map(|s| s.selection.clone()).collect(),
                into: SquashTarget::Revision(target),
            },
        }
    }

    pub fn merge(source: DraftSource) -> Self {
        Self {
            kind: DraftKind::Merge,
            sources: vec![source],
            placement: PlacementKind::default(),
            candidate: None,
            selection: None,
        }
    }

    /// A draft of `kind` over `sources` (empty for [`DraftKind::New`]).
    pub fn with_sources(kind: DraftKind, sources: Vec<DraftSource>) -> Self {
        Self {
            kind,
            sources,
            placement: PlacementKind::default(),
            candidate: None,
            selection: None,
        }
    }

    /// Whether `commit_id` is one of the draft's sources — sources can't be
    /// their own destination. (Deeper cycle checks — e.g. rebasing a subtree
    /// onto its own descendant — are left to jj-lib, whose error surfaces in
    /// the activity log.)
    pub fn is_source(&self, commit_id: &str) -> bool {
        self.sources.iter().any(|s| s.commit_id == commit_id)
    }

    pub fn target_valid(&self, commit_id: &str) -> bool {
        !self.is_source(commit_id)
    }

    /// Op-bar headline, e.g. `Rebase k1x2 + descendants` / `Squash k1x2`.
    pub fn headline(&self) -> String {
        let names: Vec<&str> = self.sources.iter().map(|s| s.label.as_str()).collect();
        let names = names.join(", ");
        match self.kind {
            DraftKind::Rebase {
                mode: RebaseSourceMode::Revisions,
            } => format!("Rebase {names}"),
            DraftKind::Rebase {
                mode: RebaseSourceMode::WithDescendants,
            } => format!("Rebase {names} + descendants"),
            DraftKind::Rebase {
                mode: RebaseSourceMode::Branch,
            } => format!("Rebase branch of {names}"),
            DraftKind::Squash => match &self.selection {
                Some(selection) => format!("Move {} from {names}", selection.summary()),
                None => format!("Squash {names}"),
            },
            DraftKind::Merge => format!("Merge {names}"),
            DraftKind::New => "New change".to_owned(),
            DraftKind::Duplicate => format!("Duplicate {names}"),
            DraftKind::Parallelize => format!("Parallelize {names}"),
        }
    }

    /// Lower an operation that is complete from its selected sources alone.
    /// Merge drafts become complete after selecting at least two parents;
    /// rebase and squash still need a separate destination.
    pub fn op_from_sources(&self) -> Option<MutationOp> {
        match self.kind {
            DraftKind::Merge if self.sources.len() >= 2 => Some(MutationOp::Merge {
                parents: self
                    .sources
                    .iter()
                    .map(|source| source.selection.clone())
                    .collect(),
            }),
            DraftKind::Parallelize if self.sources.len() >= 2 => Some(MutationOp::Parallelize {
                targets: self.sources.iter().map(|s| s.selection.clone()).collect(),
            }),
            _ => None,
        }
    }

    /// Lower the draft onto `target` with `placement`, or `None` when the
    /// target is a draft source (invalid).
    pub fn op_for(
        &self,
        target: RevisionSelection,
        placement: PlacementKind,
    ) -> Option<MutationOp> {
        if let RevisionSelection::Commit(id) = &target
            && self.is_source(id)
        {
            return None;
        }
        Some(match self.kind {
            DraftKind::Rebase { mode } => MutationOp::Rebase {
                mode,
                sources: self.sources.iter().map(|s| s.selection.clone()).collect(),
                destination: match placement {
                    PlacementKind::Onto => Destination::Onto(target),
                    PlacementKind::After => Destination::After(target),
                    PlacementKind::Before => Destination::Before(target),
                },
            },
            DraftKind::Squash => self.squash_op(target),
            DraftKind::Merge => MutationOp::Merge {
                parents: self
                    .sources
                    .iter()
                    .map(|s| s.selection.clone())
                    .chain([target])
                    .collect(),
            },
            DraftKind::New => MutationOp::NewAt {
                destination: placed(target, placement),
            },
            DraftKind::Duplicate => MutationOp::DuplicateTo {
                targets: self.sources.iter().map(|s| s.selection.clone()).collect(),
                destination: Some(placed(target, placement)),
            },
            // Nothing to aim at: the set itself is the whole op.
            DraftKind::Parallelize => return self.op_from_sources(),
        })
    }

    /// Lower a drag-into-a-gap drop: the moved commits land exactly between
    /// `parent` (the row below the gap) and `child` (the row above it).
    /// Squash drafts have no between-gesture — the gap resolves to squashing
    /// into the gap's parent side.
    pub fn op_for_gap(
        &self,
        parent: RevisionSelection,
        child: RevisionSelection,
    ) -> Option<MutationOp> {
        for side in [&parent, &child] {
            if let RevisionSelection::Commit(id) = side
                && self.is_source(id)
            {
                return None;
            }
        }
        Some(match self.kind {
            DraftKind::Rebase { mode } => MutationOp::Rebase {
                mode,
                sources: self.sources.iter().map(|s| s.selection.clone()).collect(),
                destination: Destination::Between { parent, child },
            },
            DraftKind::Squash => self.squash_op(parent),
            // A merge has no between-gesture; the gap resolves to merging
            // with its parent side, like squash.
            DraftKind::Merge => MutationOp::Merge {
                parents: self
                    .sources
                    .iter()
                    .map(|s| s.selection.clone())
                    .chain([parent])
                    .collect(),
            },
            DraftKind::New => MutationOp::NewAt {
                destination: Destination::Between { parent, child },
            },
            DraftKind::Duplicate => MutationOp::DuplicateTo {
                targets: self.sources.iter().map(|s| s.selection.clone()).collect(),
                destination: Some(Destination::Between { parent, child }),
            },
            DraftKind::Parallelize => return self.op_from_sources(),
        })
    }
}

/// The [`Destination`] a placement means relative to `target`.
pub fn placed(target: RevisionSelection, placement: PlacementKind) -> Destination {
    match placement {
        PlacementKind::Onto => Destination::Onto(target),
        PlacementKind::After => Destination::After(target),
        PlacementKind::Before => Destination::Before(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> DraftSource {
        DraftSource {
            selection: RevisionSelection::Commit(id.to_owned()),
            commit_id: id.to_owned(),
            label: id.chars().take(4).collect(),
        }
    }

    #[test]
    fn draft_rejects_sources_as_targets() {
        let draft = OpDraft::rebase(RebaseSourceMode::Revisions, vec![source("aaaa")]);
        assert!(!draft.target_valid("aaaa"));
        assert!(draft.target_valid("bbbb"));
        assert!(
            draft
                .op_for(
                    RevisionSelection::Commit("aaaa".into()),
                    PlacementKind::Onto
                )
                .is_none()
        );
    }

    #[test]
    fn rebase_draft_lowers_each_placement() {
        let draft = OpDraft::rebase(RebaseSourceMode::WithDescendants, vec![source("aaaa")]);
        let target = RevisionSelection::Commit("bbbb".to_owned());
        for (placement, expect_dest) in [
            (PlacementKind::Onto, Destination::Onto(target.clone())),
            (PlacementKind::After, Destination::After(target.clone())),
            (PlacementKind::Before, Destination::Before(target.clone())),
        ] {
            let Some(MutationOp::Rebase {
                mode,
                sources,
                destination,
            }) = draft.op_for(target.clone(), placement)
            else {
                panic!("expected a rebase op");
            };
            assert_eq!(mode, RebaseSourceMode::WithDescendants);
            assert_eq!(sources, vec![RevisionSelection::Commit("aaaa".to_owned())]);
            assert_eq!(destination, expect_dest);
        }
    }

    #[test]
    fn gap_drop_lowers_to_between_and_squash_to_parent_side() {
        let rebase = OpDraft::rebase(RebaseSourceMode::Revisions, vec![source("aaaa")]);
        let parent = RevisionSelection::Commit("pppp".to_owned());
        let child = RevisionSelection::Commit("cccc".to_owned());
        match rebase.op_for_gap(parent.clone(), child.clone()) {
            Some(MutationOp::Rebase {
                destination:
                    Destination::Between {
                        parent: p,
                        child: c,
                    },
                ..
            }) => {
                assert_eq!(p, parent);
                assert_eq!(c, child);
            }
            other => panic!("expected between destination, got {other:?}"),
        }
        // A gap drop with a source on either side is invalid.
        assert!(
            rebase
                .op_for_gap(RevisionSelection::Commit("aaaa".into()), child.clone())
                .is_none()
        );

        let squash = OpDraft::squash(source("aaaa"));
        match squash.op_for_gap(parent.clone(), child) {
            Some(MutationOp::Squash {
                into: SquashTarget::Revision(into),
                ..
            }) => assert_eq!(into, parent),
            other => panic!("expected squash into parent side, got {other:?}"),
        }
    }

    #[test]
    fn merge_draft_lowers_to_both_parents() {
        let draft = OpDraft::merge(source("aaaa"));
        let target = RevisionSelection::Commit("bbbb".to_owned());
        match draft.op_for(target.clone(), PlacementKind::Onto) {
            Some(MutationOp::Merge { parents }) => assert_eq!(
                parents,
                vec![RevisionSelection::Commit("aaaa".to_owned()), target]
            ),
            other => panic!("expected a merge op, got {other:?}"),
        }
        // Merging a revision with itself is invalid.
        assert!(
            draft
                .op_for(
                    RevisionSelection::Commit("aaaa".to_owned()),
                    PlacementKind::Onto
                )
                .is_none()
        );
    }

    #[test]
    fn merge_draft_lowers_selected_parents_without_a_target() {
        let mut draft = OpDraft::merge(source("aaaa"));
        assert!(draft.op_from_sources().is_none());

        draft.sources.push(source("bbbb"));
        match draft.op_from_sources() {
            Some(MutationOp::Merge { parents }) => assert_eq!(
                parents,
                vec![
                    RevisionSelection::Commit("aaaa".to_owned()),
                    RevisionSelection::Commit("bbbb".to_owned()),
                ]
            ),
            other => panic!("expected a source-only merge op, got {other:?}"),
        }
    }

    #[test]
    fn new_draft_lowers_each_placement_and_the_gap() {
        let draft = OpDraft::with_sources(DraftKind::New, Vec::new());
        let target = RevisionSelection::Commit("bbbb".to_owned());
        for placement in [
            PlacementKind::Onto,
            PlacementKind::After,
            PlacementKind::Before,
        ] {
            match draft.op_for(target.clone(), placement) {
                Some(MutationOp::NewAt { destination }) => {
                    assert_eq!(destination, placed(target.clone(), placement))
                }
                other => panic!("expected a placed new, got {other:?}"),
            }
        }
        let parent = RevisionSelection::Commit("pppp".to_owned());
        let child = RevisionSelection::Commit("cccc".to_owned());
        match draft.op_for_gap(parent.clone(), child.clone()) {
            Some(MutationOp::NewAt {
                destination:
                    Destination::Between {
                        parent: p,
                        child: c,
                    },
            }) => assert_eq!((p, c), (parent, child)),
            other => panic!("expected a new between, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_draft_carries_every_source_to_the_destination() {
        let draft =
            OpDraft::with_sources(DraftKind::Duplicate, vec![source("aaaa"), source("bbbb")]);
        let target = RevisionSelection::Commit("cccc".to_owned());
        match draft.op_for(target.clone(), PlacementKind::After) {
            Some(MutationOp::DuplicateTo {
                targets,
                destination,
            }) => {
                assert_eq!(targets.len(), 2);
                assert_eq!(destination, Some(Destination::After(target)));
            }
            other => panic!("expected a placed duplicate, got {other:?}"),
        }
        // A source can't be its own destination.
        assert!(
            draft
                .op_for(
                    RevisionSelection::Commit("aaaa".into()),
                    PlacementKind::Onto
                )
                .is_none()
        );
    }

    #[test]
    fn parallelize_draft_completes_from_two_sources() {
        let mut draft = OpDraft::with_sources(DraftKind::Parallelize, vec![source("aaaa")]);
        assert!(draft.op_from_sources().is_none(), "one revision is no set");
        draft.sources.push(source("bbbb"));
        assert!(matches!(
            draft.op_from_sources(),
            Some(MutationOp::Parallelize { targets }) if targets.len() == 2
        ));
        // Any target confirms the same op: there is nothing to aim at.
        assert!(matches!(
            draft.op_for(
                RevisionSelection::Commit("cccc".into()),
                PlacementKind::Onto
            ),
            Some(MutationOp::Parallelize { .. })
        ));
        assert!(DraftKind::Parallelize.completes_from_sources());
        assert!(!DraftKind::Parallelize.uses_placement());
        assert!(DraftKind::New.uses_placement() && DraftKind::Duplicate.uses_placement());
    }

    #[test]
    fn squash_draft_with_a_selection_lowers_to_a_partial_squash() {
        let selection = DiffSelection {
            files: vec![crate::partial::FileSelection {
                path: "a.rs".to_owned(),
                hunks: crate::partial::HunkPick::All,
            }],
        };
        let draft = OpDraft::squash_changes(source("aaaa"), selection.clone());
        assert_eq!(draft.headline(), "Move a.rs from aaaa");
        let target = RevisionSelection::Commit("bbbb".to_owned());
        match draft.op_for(target.clone(), PlacementKind::Onto) {
            Some(MutationOp::SquashChanges {
                from,
                selection: picked,
                into: SquashTarget::Revision(into),
            }) => {
                assert_eq!(from, RevisionSelection::Commit("aaaa".to_owned()));
                assert_eq!(picked, selection);
                assert_eq!(into, target);
            }
            other => panic!("expected a partial squash, got {other:?}"),
        }
    }
}
