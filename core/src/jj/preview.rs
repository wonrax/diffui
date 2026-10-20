use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use jj_lib::{
    backend::{ChangeId, CommitId},
    commit::Commit,
    graph::{GraphEdge, GraphEdgeType, TopoGroupedGraph},
    object_id::ObjectId,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo},
    revset::{ResolvedRevsetExpression, RevsetExpression, SymbolResolver},
    rewrite::{
        MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit, merge_commit_trees,
        move_commits,
    },
    settings::UserSettings,
    transaction::Transaction,
};

use super::mutate::*;
use super::walk::{describe_resolution_error, parse_user_revset, ref_labels};
use crate::model::{CommitSummary, RevisionSelection};
use crate::mutations::{
    Destination, DraftSimulation, InsertPreview, MergePreview, MutationOp, RebasePreview,
    RebaseSourceMode, SquashPreview,
};
use crate::partial::DiffSelection;
use crate::preview_graph::{PostOpGraph, PostOpRole, PostOpRow};
use crate::repo::{CancelFlag, PreviewRequest};
use crate::repository::Repository;

/// Past this many commits a simulation would cost a real operation's worth of
/// work per candidate hop, so the preview degrades to counts.
const SIMULATION_CAP: usize = 400;

/// Past this many rows the post-op log isn't walked; the sidebar keeps its
/// moved-set wash instead of the full graph.
const GRAPH_CAP: usize = 20_000;

/// What a preview runs against: the repo at head and this workspace.
pub(crate) struct PreviewScope<'a> {
    pub(crate) repo: &'a Arc<ReadonlyRepo>,
    pub(crate) settings: &'a UserSettings,
    pub(crate) repository: &'a Repository,
    pub(crate) workspace_name: &'a WorkspaceName,
}

/// One draft simulation, before the graph walk.
struct Simulated {
    simulation: DraftSimulation,
    /// Pre-op ids of the commits the draft moves when the result is worth
    /// walking; `None` skips the graph (nothing simulated, or too large).
    walk: Option<HashSet<CommitId>>,
    /// The transaction's changes, when the summary already needed them.
    diff: Option<TxDiff>,
}

/// Predict a draft's outcome by running it inside a transaction that is never
/// committed: the op bar's summary and, given the log revset, the graph the
/// operation would leave behind. The simulation writes unreachable
/// content-addressed objects to the backend store (like any abandoned jj op
/// would); the op log and views are untouched, so nothing is visible and
/// `jj util gc` reclaims them.
pub(crate) async fn preview(
    scope: &PreviewScope<'_>,
    request: &PreviewRequest,
    graph_revset: Option<&str>,
    cancel: &CancelFlag,
) -> Result<(DraftSimulation, Option<PostOpGraph>)> {
    let wc_commit_id = scope
        .repo
        .view()
        .get_wc_commit_id(scope.workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    let mut tx = scope.repo.start_transaction();
    let simulated = match request {
        PreviewRequest::Rebase {
            mode,
            sources,
            destination,
        } => simulate_rebase(&mut tx, &wc_commit_id, *mode, sources, destination, cancel).await?,
        PreviewRequest::Merge { parents } => {
            simulate_merge(&mut tx, scope.workspace_name, &wc_commit_id, parents).await?
        }
        PreviewRequest::Parallelize { targets } => {
            simulate_parallelize(&mut tx, &wc_commit_id, targets).await?
        }
        PreviewRequest::Squash {
            sources,
            into,
            selection,
        } => {
            simulate_squash(
                &mut tx,
                &wc_commit_id,
                sources,
                into,
                selection.as_ref(),
                cancel,
            )
            .await?
        }
        PreviewRequest::NewAt { destination } => {
            let op = MutationOp::NewAt {
                destination: destination.clone(),
            };
            simulate_insert(scope, &mut tx, &wc_commit_id, &op, cancel).await?
        }
        PreviewRequest::Duplicate {
            targets,
            destination,
        } => {
            let op = MutationOp::DuplicateTo {
                targets: targets.clone(),
                destination: Some(destination.clone()),
            };
            simulate_insert(scope, &mut tx, &wc_commit_id, &op, cancel).await?
        }
    };
    let graph = match (graph_revset, simulated.walk) {
        (Some(revset), Some(moved)) if !cancel.is_cancelled() => {
            let diff = match simulated.diff {
                Some(diff) => Some(diff),
                None => tx_diff(&tx, cancel).await?,
            };
            match diff {
                Some(diff) => post_op_graph(scope, &tx, revset, &moved, &diff, cancel).await?,
                None => None,
            }
        }
        _ => None,
    };
    // Dropping `tx` discards the simulation: no op is committed.
    Ok((simulated.simulation, graph))
}

/// [`preview`] against the repo as it was at `operation_hex`, opened afresh
/// so it can run off the repository actor's thread: a simulation that takes
/// seconds must not hold up the diffs and loads queued behind it, and the
/// jj-lib types it works with can't cross threads.
pub(crate) async fn preview_at_operation(
    settings: &UserSettings,
    repository: &Repository,
    operation_hex: &str,
    request: &PreviewRequest,
    graph_revset: Option<&str>,
    cancel: &CancelFlag,
) -> Result<(DraftSimulation, Option<PostOpGraph>)> {
    let workspace = super::workspace::load_workspace(settings, &repository.root)?;
    let loader = workspace.repo_loader();
    let operation = super::history::load_operation(loader, operation_hex).await?;
    checkpoint(cancel)?;
    let repo = loader
        .load_at(&operation)
        .await
        .context("failed to load the repo to simulate the draft on")?;
    checkpoint(cancel)?;
    let scope = PreviewScope {
        repo: &repo,
        settings,
        repository,
        workspace_name: workspace.workspace_name(),
    };
    preview(&scope, request, graph_revset, cancel).await
}

/// Merge the parents' trees and write the merge as the new `@`, like the real
/// `jj new p1 p2 …`; the summary lists the paths that stay conflicted.
async fn simulate_merge(
    tx: &mut Transaction,
    workspace_name: &WorkspaceName,
    wc_commit_id: &CommitId,
    parents: &[RevisionSelection],
) -> Result<Simulated> {
    const CONFLICT_LIST_CAP: usize = 6;

    let mut parent_commits: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in parents {
        let commit = resolve_mutation_target(tx.repo(), wc_commit_id, selection).await?;
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
    let parent_ids: Vec<CommitId> = parent_commits.iter().map(|c| c.id().clone()).collect();
    let merge_commit = tx
        .repo_mut()
        .new_commit(parent_ids, tree)
        .write()
        .await
        .context("failed to write merge commit")?;
    tx.repo_mut()
        .edit(workspace_name.to_owned(), &merge_commit)
        .await
        .context("failed to point working copy at merge commit")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants")?;
    Ok(Simulated {
        simulation: DraftSimulation::Merge(MergePreview {
            conflicts,
            truncated,
        }),
        walk: Some(HashSet::new()),
        diff: None,
    })
}

/// Run the real `move_commits` and report exact moved/descendant counts and
/// which commits become conflicted that weren't before. Past
/// [`SIMULATION_CAP`] affected commits it reports counts only.
async fn simulate_rebase(
    tx: &mut Transaction,
    wc_commit_id: &CommitId,
    mode: RebaseSourceMode,
    sources: &[RevisionSelection],
    destination: &Destination,
    cancel: &CancelFlag,
) -> Result<Simulated> {
    let mut source_ids: Vec<CommitId> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in sources {
        let commit = resolve_mutation_target(tx.repo(), wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            source_ids.push(commit.id().clone());
        }
    }
    if source_ids.is_empty() {
        bail!("rebase needs at least one source revision");
    }
    let (new_parent_ids, mut new_child_ids, _anchor) =
        resolve_rebase_location(tx.repo(), wc_commit_id, destination).await?;
    new_child_ids.retain(|id| !seen.contains(id));
    let Some(target) = resolve_move_target(tx.repo(), mode, &source_ids, &new_parent_ids).await?
    else {
        // Branch mode with the destination inside the branch itself: nothing
        // would move. A first-class empty preview (not an error) so the op
        // bar can say so while the candidate walks the branch's own rows.
        return Ok(Simulated {
            simulation: DraftSimulation::Rebase(RebasePreview {
                moved: 0,
                descendants: 0,
                abandoned_empty: 0,
                new_conflicts: Vec::new(),
                entry_points: Vec::new(),
                moved_commit_ids: Vec::new(),
                simulated: true,
            }),
            walk: None,
            diff: None,
        });
    };

    // Entry points of the moved set — for branch mode these are the resolved
    // fork-point roots, i.e. the answer to "which branch would this move?".
    let entry_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) | MoveCommitsTarget::Roots(ids) => ids.clone(),
    };
    let mut entry_points: Vec<String> = Vec::new();
    for id in &entry_ids {
        entry_points.push(short_change_id(&load_commit(tx.repo(), id).await?));
    }
    entry_points.sort_unstable();

    // The full moved set (Roots targets move their entire subtree; Commits
    // targets move exactly themselves) — doubles as the size guard, and as
    // the sidebar's whole-branch wash.
    let moved_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) => ids.clone(),
        MoveCommitsTarget::Roots(ids) => {
            RevsetExpression::commits(ids.clone())
                .descendants()
                .evaluate(tx.repo())
                .context("failed to enumerate the moved set")?
                .stream()
                .take(SIMULATION_CAP + 1)
                .map(|entry| entry.context("failed to walk the moved set"))
                .try_collect()
                .await?
        }
    };
    if moved_ids.len() > SIMULATION_CAP {
        return Ok(Simulated {
            simulation: DraftSimulation::Rebase(RebasePreview {
                moved: moved_ids.len() as u32,
                descendants: 0,
                abandoned_empty: 0,
                new_conflicts: Vec::new(),
                entry_points,
                moved_commit_ids: Vec::new(),
                simulated: false,
            }),
            walk: None,
            diff: None,
        });
    }
    let moved_commit_ids: Vec<String> = moved_ids.iter().map(|id| id.hex()).collect();

    checkpoint(cancel)?;
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
        checkpoint(cancel)?;
        let RebasedCommit::Rewritten(new_commit) = rebased else {
            continue;
        };
        if new_commit.tree_ids().is_resolved() {
            continue;
        }
        let old = load_commit(tx.repo(), old_id).await?;
        if old.tree_ids().is_resolved() {
            new_conflicts.push(short_change_id(&old));
        }
    }
    new_conflicts.sort_unstable();
    Ok(Simulated {
        simulation: DraftSimulation::Rebase(RebasePreview {
            moved: stats.num_rebased_targets,
            descendants: stats.num_rebased_descendants,
            abandoned_empty: stats.num_abandoned_empty,
            new_conflicts,
            entry_points,
            moved_commit_ids,
            simulated: true,
        }),
        walk: Some(moved_ids.into_iter().collect()),
        diff: None,
    })
}

/// Predict `jj parallelize` over `targets`, rebase-preview-shaped: how many
/// targets move, how many descendants follow, and which commits newly
/// conflict — parallelizing commits that build on each other is exactly how
/// conflicts appear.
async fn simulate_parallelize(
    tx: &mut Transaction,
    wc_commit_id: &CommitId,
    targets: &[RevisionSelection],
) -> Result<Simulated> {
    let mut ids: Vec<CommitId> = Vec::new();
    for selection in targets {
        let commit = resolve_mutation_target(tx.repo(), wc_commit_id, selection).await?;
        if !ids.contains(commit.id()) {
            ids.push(commit.id().clone());
        }
    }
    let plan = super::revops::plan_parallelize(tx.repo(), &ids).await?;
    let moved_commit_ids: Vec<String> = ids.iter().map(|id| id.hex()).collect();
    let affected = super::revops::descendants_capped(tx.repo(), &ids, SIMULATION_CAP).await?;
    if affected.len() > SIMULATION_CAP {
        return Ok(Simulated {
            simulation: DraftSimulation::Rebase(RebasePreview {
                moved: plan.needs_rewrite.len() as u32,
                descendants: 0,
                abandoned_empty: 0,
                new_conflicts: Vec::new(),
                entry_points: Vec::new(),
                moved_commit_ids,
                simulated: false,
            }),
            walk: None,
            diff: None,
        });
    }
    let rewritten = super::revops::apply_parallelize(tx.repo_mut(), &plan).await?;
    let mut new_conflicts: Vec<String> = Vec::new();
    for (old_id, new_commit) in &rewritten {
        if new_commit.tree_ids().is_resolved() {
            continue;
        }
        let old = load_commit(tx.repo(), old_id).await?;
        if old.tree_ids().is_resolved() {
            new_conflicts.push(short_change_id(&old));
        }
    }
    new_conflicts.sort_unstable();
    let moved = plan.needs_rewrite.len();
    Ok(Simulated {
        simulation: DraftSimulation::Rebase(RebasePreview {
            moved: moved as u32,
            descendants: rewritten.len().saturating_sub(moved) as u32,
            abandoned_empty: 0,
            new_conflicts,
            entry_points: Vec::new(),
            moved_commit_ids,
            simulated: true,
        }),
        // The targets keep their place in the log; only their edges change.
        walk: Some(HashSet::new()),
        diff: None,
    })
}

/// Fold the sources (or the picked changes of one source) into `into`, the
/// way the confirm path does.
async fn simulate_squash(
    tx: &mut Transaction,
    wc_commit_id: &CommitId,
    sources: &[RevisionSelection],
    into: &RevisionSelection,
    selection: Option<&DiffSelection>,
    cancel: &CancelFlag,
) -> Result<Simulated> {
    let mut source_commits: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in sources {
        let commit = resolve_mutation_target(tx.repo(), wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            source_commits.push(commit);
        }
    }
    let destination = resolve_mutation_target(tx.repo(), wc_commit_id, into).await?;
    if seen.contains(destination.id()) {
        bail!("can't squash a revision into itself");
    }
    let abandons_sources = match (selection, source_commits.as_slice()) {
        (Some(selection), [source]) => {
            super::partial::squash_selection(tx.repo_mut(), source, &destination, selection)
                .await?
                .abandoned_source
        }
        (_, []) => bail!("squash needs at least one source revision"),
        _ => {
            if squash_whole(tx.repo_mut(), source_commits, &destination)
                .await?
                .is_none()
            {
                bail!("nothing to squash: the sources have no changes");
            }
            true
        }
    };
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after the squash")?;
    let diff = tx_diff(tx, cancel).await?;
    let new_conflicts = diff.as_ref().map(TxDiff::new_conflicts).unwrap_or_default();
    Ok(Simulated {
        simulation: DraftSimulation::Squash(SquashPreview {
            abandons_sources,
            new_conflicts,
        }),
        walk: diff.is_some().then(HashSet::new),
        diff,
    })
}

/// A new change or copies placed into the graph, through the same revops the
/// confirm path runs (immutability is the confirm path's question, not the
/// preview's).
async fn simulate_insert(
    scope: &PreviewScope<'_>,
    tx: &mut Transaction,
    wc_commit_id: &CommitId,
    op: &MutationOp,
    cancel: &CancelFlag,
) -> Result<Simulated> {
    let guard = super::revops::RewriteGuard {
        repo_root: &scope.repository.root,
        settings: scope.settings,
        workspace_name: scope.workspace_name,
        allow_immutable: true,
    };
    super::revops::apply(
        tx.repo_mut(),
        &guard,
        scope.workspace_name,
        wc_commit_id,
        op,
    )
    .await?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants")?;
    let diff = tx_diff(tx, cancel).await?;
    let (created, rebased) = diff.as_ref().map_or((0, 0), |diff| {
        let created = diff
            .written
            .iter()
            .filter(|(pred, _)| pred.is_none())
            .count();
        (created, diff.written.len() - created)
    });
    let new_conflicts = diff.as_ref().map(TxDiff::new_conflicts).unwrap_or_default();
    Ok(Simulated {
        simulation: DraftSimulation::Insert(InsertPreview {
            created: created as u32,
            rebased: rebased as u32,
            new_conflicts,
        }),
        walk: diff.is_some().then(HashSet::new),
        diff,
    })
}

/// What a simulated transaction changed, read off the visible heads before
/// and after it.
struct TxDiff {
    /// Every commit the transaction made visible, with the pre-op commit of
    /// the same change it replaces (`None` for a new change).
    written: Vec<(Option<Commit>, Commit)>,
    /// Pre-op commits the transaction hid whose change has no visible commit
    /// left: abandoned outright.
    abandoned: Vec<CommitId>,
}

impl TxDiff {
    /// Short change ids of the written commits that are conflicted when what
    /// they replace wasn't, sorted.
    fn new_conflicts(&self) -> Vec<String> {
        let mut conflicts: Vec<String> = self
            .written
            .iter()
            .filter(|(pred, commit)| {
                commit.has_conflict() && !pred.as_ref().is_some_and(Commit::has_conflict)
            })
            .map(|(_, commit)| short_change_id(commit))
            .collect();
        conflicts.sort_unstable();
        conflicts
    }
}

/// Diff `tx` against its base. `None` past [`SIMULATION_CAP`] commits on
/// either side.
async fn tx_diff(tx: &Transaction, cancel: &CancelFlag) -> Result<Option<TxDiff>> {
    let repo = tx.repo();
    let before = RevsetExpression::commits(tx.base_repo().view().heads().iter().cloned().collect())
        .ancestors();
    let after =
        RevsetExpression::commits(repo.view().heads().iter().cloned().collect()).ancestors();
    let walk = |expression: Arc<ResolvedRevsetExpression>| async move {
        expression
            .evaluate(repo)
            .context("failed to diff the simulation")?
            .stream()
            .take(SIMULATION_CAP + 1)
            .map(|entry| entry.context("failed to walk the simulation's changes"))
            .try_collect::<Vec<CommitId>>()
            .await
    };
    let added = walk(after.minus(&before)).await?;
    let removed = walk(before.minus(&after)).await?;
    if added.len() > SIMULATION_CAP || removed.len() > SIMULATION_CAP {
        return Ok(None);
    }
    let mut replaced: HashMap<ChangeId, Commit> = HashMap::new();
    for id in &removed {
        checkpoint(cancel)?;
        let commit = load_commit(repo, id).await?;
        replaced.insert(commit.change_id().clone(), commit);
    }
    let mut written: Vec<(Option<Commit>, Commit)> = Vec::new();
    let mut living: HashSet<ChangeId> = HashSet::new();
    for id in &added {
        checkpoint(cancel)?;
        let commit = load_commit(repo, id).await?;
        living.insert(commit.change_id().clone());
        written.push((replaced.get(commit.change_id()).cloned(), commit));
    }
    let abandoned = replaced
        .into_values()
        .filter(|commit| !living.contains(commit.change_id()))
        .map(|commit| commit.id().clone())
        .collect();
    Ok(Some(TxDiff { written, abandoned }))
}

/// Walk `revset` over the simulated result in the order the log will show it
/// after the operation (jj's topo grouping, `@`'s branch first, exactly like
/// the graph load), and say for every row what the operation did to it.
/// `None` past [`GRAPH_CAP`] rows or when cancelled.
async fn post_op_graph(
    scope: &PreviewScope<'_>,
    tx: &Transaction,
    revset: &str,
    moved: &HashSet<CommitId>,
    diff: &TxDiff,
    cancel: &CancelFlag,
) -> Result<Option<PostOpGraph>> {
    let repo = tx.repo();
    let base = tx.base_repo();
    let name = scope.workspace_name;
    let root = &scope.repository.root;
    let expression = parse_user_revset(root, base.settings(), name, revset)?;
    let resolve = |repo: &dyn Repo| {
        let resolver = SymbolResolver::new(
            repo,
            &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
        );
        expression
            .resolve_user_expression(repo, &resolver)
            .map_err(describe_resolution_error)
            .context("failed to resolve jj revset")
    };
    // What the loaded graph shows: rows it has need no data shipped back.
    let base_set = resolve(base.as_ref())?
        .evaluate(base.as_ref())
        .context("failed to evaluate jj revset")?;
    let shown_before = base_set.containing_fn();
    let post_set = resolve(repo)?
        .evaluate(repo)
        .context("failed to evaluate jj revset")?;
    if let Ok((lower, _)) = post_set.count_estimate()
        && lower > GRAPH_CAP
    {
        return Ok(None);
    }

    let wc_before = base.view().get_wc_commit_id(name).cloned();
    let wc_after = repo.view().get_wc_commit_id(name).cloned();
    let mut nodes: Vec<(CommitId, Vec<GraphEdge<CommitId>>)> = Vec::new();
    {
        let mut topo = TopoGroupedGraph::new(post_set.stream_graph(), |id: &CommitId| id);
        let has_commit = post_set.containing_fn();
        if let Some(wc) = &wc_after
            && has_commit(wc).await.unwrap_or(false)
        {
            topo.prioritize_branch(wc.clone());
        }
        let mut stream = std::pin::pin!(topo.stream());
        while let Some(node) = stream.next().await {
            nodes.push(node.context("failed to walk the simulated graph")?);
            if nodes.len() > GRAPH_CAP {
                return Ok(None);
            }
            checkpoint(cancel)?;
        }
    }

    let row_of: HashMap<&CommitId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, (id, _))| (id, index))
        .collect();
    let written: HashMap<&CommitId, Option<&Commit>> = diff
        .written
        .iter()
        .map(|(pred, commit)| (commit.id(), pred.as_ref()))
        .collect();
    let labels_before = ref_labels(base.view(), name);
    let labels_after = ref_labels(repo.view(), name);
    let immutable = parse_user_revset(root, base.settings(), name, "immutable()")
        .and_then(|expression| {
            let resolver = SymbolResolver::new(
                repo,
                &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
            );
            expression
                .resolve_user_expression(repo, &resolver)
                .context("failed to resolve immutable()")
        })
        .and_then(|resolved| {
            resolved
                .evaluate(repo)
                .context("failed to evaluate immutable()")
        })
        .ok();
    let is_immutable = immutable.as_ref().map(|set| set.containing_fn());

    let mut missing: HashMap<CommitId, usize> = HashMap::new();
    let mut rows: Vec<PostOpRow> = Vec::with_capacity(nodes.len());
    for (id, edges) in &nodes {
        checkpoint(cancel)?;
        let edges = edges
            .iter()
            .map(|edge| match (edge.edge_type, row_of.get(&edge.target)) {
                (GraphEdgeType::Direct, Some(&row)) => GraphEdge::direct(row),
                (GraphEdgeType::Indirect, Some(&row)) => GraphEdge::indirect(row),
                _ => {
                    let next = usize::MAX - missing.len();
                    GraphEdge::missing(*missing.entry(edge.target.clone()).or_insert(next))
                }
            })
            .collect();
        let labels = labels_after.get(id).cloned().unwrap_or_default();
        let is_wc = wc_after.as_ref() == Some(id);
        let immutable = match &is_immutable {
            Some(contains) => contains(id).await.unwrap_or(false),
            None => false,
        };
        let row = match written.get(id) {
            Some(pred) => {
                let commit = load_commit(repo, id).await?;
                let (origin, role) = match pred {
                    Some(pred) if moved.contains(pred.id()) => (pred.id(), PostOpRole::Moved),
                    Some(pred) => (pred.id(), PostOpRole::Rewritten),
                    None => (id, PostOpRole::Created),
                };
                PostOpRow {
                    origin: (role != PostOpRole::Created).then(|| origin.hex()),
                    role,
                    edges,
                    new_conflict: commit.has_conflict()
                        && !pred.is_some_and(|pred| pred.has_conflict()),
                    summary: Some(
                        summary_of(repo, &commit, origin.hex(), labels, is_wc, immutable).await,
                    ),
                }
            }
            None => {
                let shown = shown_before(id).await.unwrap_or(false);
                let same_refs = labels_before.get(id).map(Vec::as_slice).unwrap_or_default()
                    == labels.as_slice();
                let same_wc = (wc_before.as_ref() == Some(id)) == is_wc;
                let summary = if shown && same_refs && same_wc {
                    None
                } else {
                    let commit = load_commit(repo, id).await?;
                    Some(summary_of(repo, &commit, id.hex(), labels, is_wc, immutable).await)
                };
                PostOpRow {
                    origin: Some(id.hex()),
                    role: PostOpRole::Unchanged,
                    edges,
                    summary,
                    new_conflict: false,
                }
            }
        };
        rows.push(row);
    }
    Ok(Some(PostOpGraph {
        rows,
        abandoned: diff.abandoned.iter().map(ObjectId::hex).collect(),
    }))
}

/// A post-op row's data, shaped like the graph load's. `commit_id` is the id
/// the row answers to: the pre-op commit's for a rewrite, so selections and
/// draft sources keep matching it.
async fn summary_of(
    repo: &dyn Repo,
    commit: &Commit,
    commit_id: String,
    bookmarks: Vec<String>,
    is_working_copy: bool,
    is_immutable: bool,
) -> CommitSummary {
    let description = commit.description().lines().next().unwrap_or("").trim();
    let is_empty = match commit.parent_ids() {
        [parent] => match repo.store().get_commit_async(parent).await {
            Ok(parent) => Some(parent.tree_ids() == commit.tree_ids()),
            Err(_) => None,
        },
        _ => None,
    };
    let (is_divergent, change_offset) = match repo.resolve_change_id(commit.change_id()).await {
        Ok(Some(targets)) if targets.is_divergent() => (true, targets.find_offset(commit.id())),
        _ => (false, None),
    };
    CommitSummary {
        change_id: commit.change_id().to_string(),
        commit_id,
        shortest_change_id_len: repo
            .shortest_unique_change_id_prefix_len(commit.change_id())
            .await
            .ok(),
        description: if description.is_empty() {
            "(no description set)".to_owned()
        } else {
            description.to_owned()
        },
        author: commit.author().name.clone(),
        has_description: !description.is_empty(),
        is_empty,
        has_conflict: commit.has_conflict(),
        is_divergent,
        is_hidden: false,
        change_offset,
        is_working_copy,
        is_immutable,
        // Only "rebase onto trunk" reads it, and no command runs on a
        // preview row.
        on_trunk: false,
        bookmarks,
        parent_ids: commit.parent_ids().iter().map(ObjectId::hex).collect(),
    }
}

async fn load_commit(repo: &dyn Repo, id: &CommitId) -> Result<Commit> {
    repo.store()
        .get_commit_async(id)
        .await
        .with_context(|| format!("failed to load jj commit {}", id.hex()))
}

/// Bail out of a preview whose job was cancelled: a superseded candidate or
/// a draft that ended. Cheap (one atomic load), so the loops call it per
/// commit or row.
fn checkpoint(cancel: &CancelFlag) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("preview cancelled");
    }
    Ok(())
}
