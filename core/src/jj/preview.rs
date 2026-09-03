use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    object_id::ObjectId,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo},
    revset::RevsetExpression,
    rewrite::{
        MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit, merge_commit_trees,
        move_commits,
    },
    settings::UserSettings,
};

use super::mutate::*;
use crate::model::RevisionSelection;
use crate::mutations::{Destination, RebaseSourceMode};
use crate::repository::Repository;

/// Predict a merge draft's outcome: merge the parents' trees in a throwaway
/// transaction and list the paths that stay conflicted. Same storage caveat
/// as [`preview_rebase`] — unreachable objects only, no op written.
pub(crate) async fn preview_merge(
    repo: &Arc<ReadonlyRepo>,
    workspace_name: &WorkspaceName,
    parents: &[RevisionSelection],
) -> Result<crate::mutations::MergePreview> {
    const CONFLICT_LIST_CAP: usize = 6;

    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    let tx = repo.start_transaction();
    let mut parent_commits: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in parents {
        let commit = resolve_mutation_target(tx.repo(), &wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            parent_commits.push(commit);
        }
    }
    if parent_commits.len() < 2 {
        bail!("a merge needs at least two distinct parents");
    }
    let tree = merge_commit_trees(tx.repo(), &parent_commits)
        .await
        .context("failed to merge parent trees")?;
    let mut conflicts: Vec<String> = Vec::new();
    let mut truncated = false;
    for (path, _value) in tree.conflicts() {
        if conflicts.len() == CONFLICT_LIST_CAP {
            truncated = true;
            break;
        }
        conflicts.push(path.as_internal_file_string().to_owned());
    }
    // `tx` dropped — nothing committed.
    Ok(crate::mutations::MergePreview {
        conflicts,
        truncated,
    })
}

/// Predict a rebase draft's outcome by running the real `move_commits` inside
/// a transaction that is never committed: exact moved/descendant counts and
/// which commits become conflicted that weren't before. The simulation writes
/// unreachable content-addressed objects to the backend store (like any
/// abandoned jj op would); the op log and views are untouched, so nothing is
/// visible and `jj util gc` reclaims them. Skipped past
/// [`REBASE_PREVIEW_SIMULATION_CAP`] affected commits — counts only then.
pub(crate) async fn preview_rebase(
    repo: &Arc<ReadonlyRepo>,
    _settings: &UserSettings,
    _repository: &Repository,
    workspace_name: &WorkspaceName,
    mode: RebaseSourceMode,
    sources: &[RevisionSelection],
    destination: &Destination,
) -> Result<crate::mutations::RebasePreview> {
    const REBASE_PREVIEW_SIMULATION_CAP: usize = 400;

    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    let mut tx = repo.start_transaction();
    let mut source_ids: Vec<CommitId> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in sources {
        let commit = resolve_mutation_target(tx.repo(), &wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            source_ids.push(commit.id().clone());
        }
    }
    if source_ids.is_empty() {
        bail!("rebase needs at least one source revision");
    }
    let (new_parent_ids, mut new_child_ids, _anchor) =
        resolve_rebase_location(tx.repo(), &wc_commit_id, destination).await?;
    new_child_ids.retain(|id| !seen.contains(id));
    let Some(target) = resolve_move_target(tx.repo(), mode, &source_ids, &new_parent_ids).await?
    else {
        // Branch mode with the destination inside the branch itself: nothing
        // would move. A first-class empty preview (not an error) so the op
        // bar can say so while the candidate walks the branch's own rows.
        return Ok(crate::mutations::RebasePreview {
            moved: 0,
            descendants: 0,
            abandoned_empty: 0,
            new_conflicts: Vec::new(),
            entry_points: Vec::new(),
            moved_commit_ids: Vec::new(),
            simulated: true,
        });
    };

    // Entry points of the moved set — for branch mode these are the resolved
    // fork-point roots, i.e. the answer to "which branch would this move?".
    let entry_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) | MoveCommitsTarget::Roots(ids) => ids.clone(),
    };
    let mut entry_points: Vec<String> = Vec::new();
    for id in &entry_ids {
        let commit = tx
            .repo()
            .store()
            .get_commit_async(id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        entry_points.push(short_change_id(&commit));
    }
    entry_points.sort_unstable();

    // The full moved set (Roots targets move their entire subtree; Commits
    // targets move exactly themselves) — doubles as the size guard: past the
    // cap each preview would cost a real rebase's worth of work, so it
    // degrades to counts + entry points only. Also the sidebar's
    // whole-branch wash.
    let moved_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) => ids.clone(),
        MoveCommitsTarget::Roots(ids) => RevsetExpression::commits(ids.clone())
            .descendants()
            .evaluate(tx.repo())
            .context("failed to enumerate the moved set")?
            .iter()
            .take(REBASE_PREVIEW_SIMULATION_CAP + 1)
            .map(|entry| entry.context("failed to walk the moved set"))
            .collect::<Result<_>>()?,
    };
    if moved_ids.len() > REBASE_PREVIEW_SIMULATION_CAP {
        return Ok(crate::mutations::RebasePreview {
            moved: moved_ids.len() as u32,
            descendants: 0,
            abandoned_empty: 0,
            new_conflicts: Vec::new(),
            entry_points,
            moved_commit_ids: Vec::new(),
            simulated: false,
        });
    }
    let moved_commit_ids: Vec<String> = moved_ids.iter().map(|id| id.hex()).collect();

    let location = MoveCommitsLocation {
        new_parent_ids,
        new_child_ids,
        target,
    };
    let stats = move_commits(tx.repo_mut(), &location, &RebaseOptions::default())
        .await
        .context("failed to simulate rebase")?;

    let mut new_conflicts: Vec<String> = Vec::new();
    for (old_id, rebased) in &stats.rebased_commits {
        let RebasedCommit::Rewritten(new_commit) = rebased else {
            continue;
        };
        if new_commit.tree_ids().is_resolved() {
            continue;
        }
        let old = tx
            .repo()
            .store()
            .get_commit_async(old_id)
            .await
            .with_context(|| format!("failed to load jj commit {}", old_id.hex()))?;
        if old.tree_ids().is_resolved() {
            new_conflicts.push(short_change_id(&old));
        }
    }
    new_conflicts.sort_unstable();
    // Dropping `tx` here discards the simulation — no op is committed.
    Ok(crate::mutations::RebasePreview {
        moved: stats.num_rebased_targets,
        descendants: stats.num_rebased_descendants,
        abandoned_empty: stats.num_abandoned_empty,
        new_conflicts,
        entry_points,
        moved_commit_ids,
        simulated: true,
    })
}
