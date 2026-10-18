//! Whole-revision operations beyond the original set: `jj commit`,
//! `jj new -A/-B`, `jj parallelize`, `jj duplicate -d/-A/-B`, divergent-change
//! resolution and `jj metaedit`'s author updates.
//!
//! Each runs inside the transaction [`super::mutate`] opened under the
//! working-copy lock, after the on-disk fold, and mirrors the jj CLI v0.45.1
//! command of the same name. They return the activity message; the caller
//! commits the transaction and checks the result out.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    object_id::ObjectId,
    ref_name::WorkspaceName,
    repo::{MutableRepo, Repo},
    revset::RevsetExpression,
    rewrite::{
        duplicate_commits, duplicate_commits_onto_parents, merge_commit_trees, rebase_commit,
    },
    settings::UserSettings,
};

use super::mutate::{
    ensure_rewritable, resolve_mutation_target, resolve_rebase_location, reverse_topo_order,
    short_change_id,
};
use crate::model::RevisionSelection;
use crate::mutations::Destination;

/// What [`ensure_rewritable`] needs, bundled so each op can check the commits
/// it is about to rewrite without threading four arguments through.
pub(super) struct RewriteGuard<'a> {
    pub(super) repo_root: &'a Path,
    pub(super) settings: &'a UserSettings,
    pub(super) workspace_name: &'a WorkspaceName,
    pub(super) allow_immutable: bool,
}

impl RewriteGuard<'_> {
    pub(super) async fn check(&self, repo: &MutableRepo, ids: &[CommitId]) -> Result<()> {
        ensure_rewritable(
            self.repo_root,
            self.settings,
            self.workspace_name,
            repo,
            ids,
            self.allow_immutable,
        )
        .await
    }
}

/// A finished op's activity message, plus the commit a single addressed
/// target became (so the sidebar selection can follow it).
pub(super) struct RevopOutcome {
    pub(super) message: String,
    pub(super) rewritten_commit: Option<String>,
}

impl From<String> for RevopOutcome {
    fn from(message: String) -> Self {
        Self {
            message,
            rewritten_commit: None,
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Resolve `selections` to commits, dropping duplicates, in the given order.
async fn resolve_all(
    repo: &MutableRepo,
    wc_id: &CommitId,
    selections: &[RevisionSelection],
) -> Result<Vec<Commit>> {
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    for selection in selections {
        let commit = resolve_mutation_target(repo, wc_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            commits.push(commit);
        }
    }
    Ok(commits)
}

/// The CLI's `ensure_no_commit_loop`: inserting between `parents` and
/// `children` must not make a commit both an ancestor and a descendant of the
/// new commits.
async fn ensure_no_loop(
    repo: &MutableRepo,
    children: &[CommitId],
    parents: &[CommitId],
    what: &str,
) -> Result<()> {
    let looped = RevsetExpression::commits(children.to_vec())
        .dag_range_to(&RevsetExpression::commits(parents.to_vec()))
        .evaluate(repo)
        .context("failed to check for a commit loop")?
        .stream()
        .next()
        .await
        .transpose()
        .context("failed to check for a commit loop")?;
    if let Some(id) = looped {
        let commit = repo
            .store()
            .get_commit_async(&id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        bail!(
            "refusing to create a loop: {} would be both an ancestor and a descendant of the {what}",
            short_change_id(&commit)
        );
    }
    Ok(())
}

/// The parents and children an insertion at `destination` resolves to, with
/// the CLI's guards: the children must be rewritable and must not loop back
/// into the parents.
async fn insertion_point(
    repo: &MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    destination: &Destination,
    what: &str,
) -> Result<(Vec<CommitId>, Vec<CommitId>, Commit)> {
    let (parents, children, anchor) = resolve_rebase_location(repo, wc_id, destination).await?;
    if !children.is_empty() {
        guard.check(repo, &children).await?;
        ensure_no_loop(repo, &children, &parents, what).await?;
    }
    if parents.is_empty() {
        bail!("no revisions found to use as parent");
    }
    Ok((parents, children, anchor))
}

fn placement_word(destination: &Destination) -> &'static str {
    match destination {
        Destination::Onto(_) => "on",
        Destination::After(_) => "after",
        Destination::Before(_) => "before",
        Destination::Between { .. } => "between",
    }
}

/// jj stores descriptions newline-terminated (`jj commit -m` writes them that
/// way); an empty one stays empty.
fn complete_newline(description: &str) -> String {
    let trimmed = description.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// `jj commit -m <description>`: describe `@`, then put every workspace that
/// had it checked out on a new empty child carrying the same tree.
pub(super) async fn commit(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    description: &str,
) -> Result<RevopOutcome> {
    let commit = repo
        .store()
        .get_commit_async(wc_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", wc_id.hex()))?;
    guard.check(repo, std::slice::from_ref(wc_id)).await?;
    let committed = repo
        .rewrite_commit(&commit)
        .set_description(complete_newline(description))
        .write()
        .await
        .context("failed to describe the working-copy commit")?;
    let workspaces = repo.view().workspaces_for_wc_commit_id(commit.id());
    let new_wc = repo
        .new_commit(vec![committed.id().clone()], commit.tree())
        .write()
        .await
        .context("failed to write the new working-copy commit")?;
    for name in workspaces {
        repo.edit(name, &new_wc)
            .await
            .context("failed to move the working copy onto the new commit")?;
    }
    Ok(format!(
        "Committed {} \u{b7} working copy now at {}",
        short_change_id(&committed),
        short_change_id(&new_wc)
    )
    .into())
}

/// `jj new -A/-B` (or `-A x -B y`, or a plain `jj new <target>` for `Onto`):
/// a new empty change at `destination`, the commits on the far side of the
/// insertion rebased onto it, and `workspace` moved onto it.
pub(super) async fn new_at(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    workspace: &WorkspaceName,
    wc_id: &CommitId,
    destination: &Destination,
) -> Result<RevopOutcome> {
    let (parent_ids, child_ids, anchor) =
        insertion_point(repo, guard, wc_id, destination, "new commit").await?;
    let mut parents = Vec::new();
    for id in &parent_ids {
        parents.push(
            repo.store()
                .get_commit_async(id)
                .await
                .with_context(|| format!("failed to load jj commit {}", id.hex()))?,
        );
    }
    let tree = merge_commit_trees(repo, &parents)
        .await
        .context("failed to build tree for new commit")?;
    let new_commit = repo
        .new_commit(parent_ids.clone(), tree)
        .write()
        .await
        .context("failed to write new commit")?;
    // The CLI's child rebase: a child's parent that is one of the new
    // commit's parents is replaced by the new commit, and the new commit is
    // added if it isn't there yet.
    let parent_set: HashSet<&CommitId> = parent_ids.iter().collect();
    for child_id in &child_ids {
        let child = repo
            .store()
            .get_commit_async(child_id)
            .await
            .with_context(|| format!("failed to load jj commit {}", child_id.hex()))?;
        let mut new_parents: Vec<CommitId> = Vec::new();
        for old_parent in child.parent_ids() {
            let parent = if parent_set.contains(old_parent) {
                new_commit.id()
            } else {
                old_parent
            };
            if !new_parents.contains(parent) {
                new_parents.push(parent.clone());
            }
        }
        if !new_parents.contains(new_commit.id()) {
            new_parents.push(new_commit.id().clone());
        }
        rebase_commit(repo, child, new_parents)
            .await
            .context("failed to rebase a child onto the new commit")?;
    }
    repo.edit(workspace.to_owned(), &new_commit)
        .await
        .context("failed to point working copy at new commit")?;
    let mut message = format!(
        "New change {} {}",
        placement_word(destination),
        short_change_id(&anchor)
    );
    if !child_ids.is_empty() {
        message.push_str(&format!(
            " ({} rebased onto it)",
            plural(child_ids.len(), "child", "children")
        ));
    }
    Ok(message.into())
}

/// What `jj parallelize` will do to a target set: the targets (children
/// first), each target's new parents, and the targets whose parents change.
pub(super) struct ParallelizePlan {
    targets: Vec<CommitId>,
    new_target_parents: HashMap<CommitId, Vec<CommitId>>,
    new_child_parents: HashMap<CommitId, Vec<CommitId>>,
    pub(super) needs_rewrite: Vec<CommitId>,
}

/// Plan `jj parallelize` over `ids`, exactly as the CLI computes it.
pub(super) async fn plan_parallelize(
    repo: &MutableRepo,
    ids: &[CommitId],
) -> Result<ParallelizePlan> {
    if ids.len() < 2 {
        bail!("pick at least two revisions to parallelize");
    }
    if ids.contains(repo.store().root_commit_id()) {
        bail!("the root commit cannot be parallelized");
    }
    // Children before parents.
    let targets = reverse_topo_order(repo, ids).await?;
    let mut commits = Vec::new();
    for id in &targets {
        commits.push(
            repo.store()
                .get_commit_async(id)
                .await
                .with_context(|| format!("failed to load jj commit {}", id.hex()))?,
        );
    }
    // Targets inherit their in-set parents' outside parents, recursively.
    let mut new_target_parents: HashMap<CommitId, Vec<CommitId>> = HashMap::new();
    let mut needs_rewrite = Vec::new();
    for commit in commits.iter().rev() {
        let mut new_parents = Vec::new();
        for old_parent in commit.parent_ids() {
            if let Some(grand_parents) = new_target_parents.get(old_parent) {
                new_parents.extend_from_slice(grand_parents);
                if !needs_rewrite.contains(commit.id()) {
                    needs_rewrite.push(commit.id().clone());
                }
            } else {
                new_parents.push(old_parent.clone());
            }
        }
        new_target_parents.insert(commit.id().clone(), new_parents);
    }
    // An outside commit with a target as parent gets that target's in-set
    // ancestors as direct parents too.
    let mut new_child_parents: HashMap<CommitId, Vec<CommitId>> = HashMap::new();
    for commit in commits.iter().rev() {
        let mut new_parents: Vec<CommitId> = Vec::new();
        for old_parent in commit.parent_ids() {
            if let Some(parents) = new_child_parents.get(old_parent) {
                for parent in parents {
                    if !new_parents.contains(parent) {
                        new_parents.push(parent.clone());
                    }
                }
            }
        }
        if !new_parents.contains(commit.id()) {
            new_parents.push(commit.id().clone());
        }
        new_child_parents.insert(commit.id().clone(), new_parents);
    }
    Ok(ParallelizePlan {
        targets,
        new_target_parents,
        new_child_parents,
        needs_rewrite,
    })
}

/// Carry out a [`ParallelizePlan`], returning every rewritten commit as
/// `(old id, new commit)` — targets and their descendants alike.
pub(super) async fn apply_parallelize(
    repo: &mut MutableRepo,
    plan: &ParallelizePlan,
) -> Result<Vec<(CommitId, Commit)>> {
    let mut rewritten: Vec<(CommitId, Commit)> = Vec::new();
    repo.transform_descendants(plan.targets.clone(), async |mut rewriter| {
        let old_id = rewriter.old_commit().id().clone();
        if let Some(new_parents) = plan.new_target_parents.get(&old_id) {
            rewriter.set_new_rewritten_parents(new_parents);
        } else if rewriter
            .old_commit()
            .parent_ids()
            .iter()
            .any(|id| plan.new_child_parents.contains_key(id))
        {
            let mut new_parents = Vec::new();
            for parent in rewriter.old_commit().parent_ids() {
                match plan.new_child_parents.get(parent) {
                    Some(parents) => new_parents.extend(parents.iter().cloned()),
                    None => new_parents.push(parent.clone()),
                }
            }
            rewriter.set_new_rewritten_parents(&new_parents);
        }
        if rewriter.parents_changed() {
            let builder = rewriter.rebase().await?;
            let new_commit = builder.write().await?;
            rewritten.push((old_id, new_commit));
        }
        Ok(())
    })
    .await
    .context("failed to parallelize")?;
    Ok(rewritten)
}

/// `jj parallelize <targets>`.
pub(super) async fn parallelize(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    targets: &[RevisionSelection],
) -> Result<RevopOutcome> {
    let commits = resolve_all(repo, wc_id, targets).await?;
    let ids: Vec<CommitId> = commits.iter().map(|c| c.id().clone()).collect();
    let plan = plan_parallelize(repo, &ids).await?;
    guard.check(repo, &plan.needs_rewrite).await?;
    let rewritten = apply_parallelize(repo, &plan).await?;
    let descendants = rewritten.len().saturating_sub(plan.needs_rewrite.len());
    let mut message = format!(
        "Parallelized {}",
        plural(ids.len(), "revision", "revisions")
    );
    if descendants > 0 {
        message.push_str(&format!(
            " ({} followed)",
            plural(descendants, "descendant", "descendants")
        ));
    }
    Ok(message.into())
}

/// `jj duplicate <targets> [-d/-A/-B <destination>]`.
pub(super) async fn duplicate_to(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    targets: &[RevisionSelection],
    destination: Option<&Destination>,
) -> Result<RevopOutcome> {
    let commits = resolve_all(repo, wc_id, targets).await?;
    if commits.is_empty() {
        bail!("duplicate needs at least one revision");
    }
    let ids: Vec<CommitId> = commits.iter().map(|c| c.id().clone()).collect();
    if ids.contains(repo.store().root_commit_id()) {
        bail!("the root commit cannot be duplicated");
    }
    let ordered = reverse_topo_order(repo, &ids).await?;
    let (stats, place) = match destination {
        None => (
            duplicate_commits_onto_parents(repo, &ordered, &HashMap::new())
                .await
                .context("failed to duplicate")?,
            None,
        ),
        Some(destination) => {
            let (parents, children, anchor) =
                insertion_point(repo, guard, wc_id, destination, "duplicated commits").await?;
            let stats = duplicate_commits(repo, &ordered, &HashMap::new(), &parents, &children)
                .await
                .context("failed to duplicate")?;
            (
                stats,
                Some(format!(
                    "{} {}",
                    placement_word(destination),
                    short_change_id(&anchor)
                )),
            )
        }
    };
    let mut message = match commits.as_slice() {
        [only] => match stats.duplicated_commits.get(only.id()) {
            Some(copy) => format!(
                "Duplicated {} as {}",
                short_change_id(only),
                short_change_id(copy)
            ),
            None => format!("Duplicated {}", short_change_id(only)),
        },
        many => format!("Duplicated {}", plural(many.len(), "revision", "revisions")),
    };
    if let Some(place) = place {
        message.push(' ');
        message.push_str(&place);
    }
    if stats.num_rebased > 0 {
        message.push_str(&format!(
            " ({} rebased onto the copies)",
            plural(stats.num_rebased as usize, "commit", "commits")
        ));
    }
    let rewritten_commit = match (targets, commits.as_slice()) {
        ([RevisionSelection::Commit(_)], [only]) => stats
            .duplicated_commits
            .get(only.id())
            .map(|copy| copy.id().hex()),
        _ => None,
    };
    Ok(RevopOutcome {
        message,
        rewritten_commit,
    })
}

/// Resolve a divergent change by abandoning every visible copy except `keep`.
pub(super) async fn keep_divergent(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    keep: &RevisionSelection,
) -> Result<RevopOutcome> {
    let kept = resolve_mutation_target(repo, wc_id, keep).await?;
    let targets = repo
        .resolve_change_id(kept.change_id())
        .await
        .context("failed to resolve the change's copies")?;
    let others: Vec<CommitId> = targets
        .iter()
        .flat_map(|targets| targets.visible_with_offsets())
        .map(|(_, id)| id.clone())
        .filter(|id| id != kept.id())
        .collect();
    if others.is_empty() {
        bail!(
            "{} is not divergent — no other visible copy to abandon",
            short_change_id(&kept)
        );
    }
    guard.check(repo, &others).await?;
    for id in &others {
        let commit = repo
            .store()
            .get_commit_async(id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        repo.record_abandoned_commit(&commit);
    }
    Ok(format!(
        "Kept {} \u{b7} abandoned {}",
        short_change_id(&kept),
        plural(others.len(), "other copy", "other copies")
    )
    .into())
}

/// `jj metaedit --update-author` / `--update-author-timestamp` over
/// `targets`: the author becomes the configured user (the committer the
/// rewrite stamps), and/or the author timestamp becomes the rewrite's.
pub(super) async fn metaedit(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    wc_id: &CommitId,
    targets: &[RevisionSelection],
    update_author: bool,
    update_author_timestamp: bool,
) -> Result<RevopOutcome> {
    let commits = resolve_all(repo, wc_id, targets).await?;
    if commits.is_empty() {
        bail!("pick at least one revision");
    }
    let ids: Vec<CommitId> = commits.iter().map(|c| c.id().clone()).collect();
    guard.check(repo, &ids).await?;
    let id_set: HashSet<CommitId> = ids.iter().cloned().collect();
    let mut modified: Vec<(CommitId, Commit)> = Vec::new();
    // `transform_descendants` so a target that descends from another target
    // is rewritten once, not once per ancestor rewrite.
    repo.transform_descendants(ids.clone(), async |rewriter| {
        let old_id = rewriter.old_commit().id().clone();
        if id_set.contains(&old_id) {
            let old_author = rewriter.old_commit().author().clone();
            let mut rewrite = rewriter.parents_changed();
            let mut builder = rewriter.reparent();
            let mut new_author = builder.author().clone();
            if update_author {
                new_author.name = builder.committer().name.clone();
                new_author.email = builder.committer().email.clone();
            }
            if update_author_timestamp {
                new_author.timestamp = builder.committer().timestamp;
            }
            if new_author.name != old_author.name
                || new_author.email != old_author.email
                || (new_author.timestamp != builder.author().timestamp
                    && new_author.timestamp != old_author.timestamp)
            {
                builder = builder.set_author(new_author);
                rewrite = true;
            }
            if rewrite {
                modified.push((old_id, builder.write().await?));
            }
        } else if rewriter.parents_changed() {
            rewriter.reparent().write().await?;
        }
        Ok(())
    })
    .await
    .context("failed to rewrite commit metadata")?;
    let what = match (update_author, update_author_timestamp) {
        (true, true) => "author and author date",
        (true, false) => "author",
        _ => "author date",
    };
    if modified.is_empty() {
        return Ok(format!("Nothing to update \u{b7} {what} already current").into());
    }
    let rewritten_commit = match (targets, modified.as_slice()) {
        ([RevisionSelection::Commit(_)], [(_, new_commit)]) => Some(new_commit.id().hex()),
        _ => None,
    };
    Ok(RevopOutcome {
        message: format!(
            "Updated {what} on {}",
            plural(modified.len(), "revision", "revisions")
        ),
        rewritten_commit,
    })
}

/// Commit ids of `ids` and every descendant, capped at `cap + 1` so callers
/// can tell "too many" without walking a huge history.
pub(super) async fn descendants_capped(
    repo: &MutableRepo,
    ids: &[CommitId],
    cap: usize,
) -> Result<Vec<CommitId>> {
    RevsetExpression::commits(ids.to_vec())
        .descendants()
        .evaluate(repo)
        .context("failed to enumerate descendants")?
        .stream()
        .take(cap + 1)
        .map(|entry| entry.context("failed to walk descendants"))
        .try_collect()
        .await
}

/// Dispatch one of this module's ops — the single arm `mutate_locked` routes
/// them through.
pub(super) async fn apply(
    repo: &mut MutableRepo,
    guard: &RewriteGuard<'_>,
    workspace: &WorkspaceName,
    wc_id: &CommitId,
    op: &crate::mutations::MutationOp,
) -> Result<RevopOutcome> {
    use crate::mutations::MutationOp;
    match op {
        MutationOp::Commit { description } => commit(repo, guard, wc_id, description).await,
        MutationOp::NewAt { destination } => {
            new_at(repo, guard, workspace, wc_id, destination).await
        }
        MutationOp::Parallelize { targets } => parallelize(repo, guard, wc_id, targets).await,
        MutationOp::DuplicateTo {
            targets,
            destination,
        } => duplicate_to(repo, guard, wc_id, targets, destination.as_ref()).await,
        MutationOp::KeepDivergent { keep } => keep_divergent(repo, guard, wc_id, keep).await,
        MutationOp::Metaedit {
            targets,
            update_author,
            update_author_timestamp,
        } => {
            metaedit(
                repo,
                guard,
                wc_id,
                targets,
                *update_author,
                *update_author_timestamp,
            )
            .await
        }
        _ => bail!("not a revops mutation"),
    }
}
