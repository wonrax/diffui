//! jj-lib side of the history views: `jj op log` / `jj op show`,
//! `jj evolog`, `jj file annotate`, and the interdiffs they show. Read-only
//! against a repo the actor already holds; the op restore/revert mutations
//! live with the other mutations in [`super::mutate`].

use std::collections::{HashMap, HashSet};
use std::slice;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use jj_lib::{
    annotate::FileAnnotator,
    backend::{ChangeId, CommitId, MergedTreeValueExt as _},
    commit::Commit,
    evolution::{accumulate_predecessors, walk_predecessors},
    git::REMOTE_NAME_FOR_LOCAL_GIT_REPO,
    object_id::ObjectId,
    op_store::{OperationId, RefTarget},
    op_walk,
    operation::Operation,
    ref_name::WorkspaceName,
    refs::{diff_named_commit_ids, diff_named_ref_targets, diff_named_remote_refs},
    repo::{ReadonlyRepo, Repo, RepoLoader},
    repo_path::RepoPathBuf,
    revset::RevsetExpression,
    rewrite::rebase_to_dest_parent,
};

use super::diff::{diff_trees, jj_revision_details, repo_scope_matcher};
use super::workspace::resolve_revision;
use crate::history::{
    ChangedCommit, CommitBrief, CommitChangeKind, EvologEntry, FileAnnotation, HistoryDiffTarget,
    OperationChanges, OperationEntry, OperationRef, RefChange, RefKind,
};
use crate::model::{DiffDocument, RevisionDetails, RevisionSelection};
use crate::repo::CancelFlag;
use crate::repository::Repository;

/// Most commits an `op show` lists. A `jj git fetch` into a busy repository
/// can touch thousands; past this the list stops being something to read.
const MAX_CHANGED_COMMITS: usize = 200;

/// Most versions an evolog lists.
const MAX_EVOLOG_ENTRIES: usize = 500;

fn operation_entry(op: &Operation, current: &OperationId) -> OperationEntry {
    let metadata = op.metadata();
    OperationEntry {
        id: op.id().hex(),
        parent_ids: op.parent_ids().iter().map(ObjectId::hex).collect(),
        description: metadata.description.clone(),
        user: format!("{}@{}", metadata.username, metadata.hostname),
        time_millis: metadata.time.end.timestamp.0,
        time: super::diff::format_jj_timestamp(&metadata.time.end),
        is_snapshot: metadata.is_snapshot,
        is_current: op.id() == current,
        args: metadata.attributes.get("args").cloned(),
    }
}

/// Stream the operation log from `repo`'s operation backwards, `batch` at a
/// time, stopping after `limit`. Returns whether older operations were left
/// unread, or `None` if cancelled.
pub(crate) async fn stream_op_log(
    repo: &ReadonlyRepo,
    limit: usize,
    batch: usize,
    cancel: &CancelFlag,
    emit: &mut dyn FnMut(Vec<OperationEntry>),
) -> Result<Option<bool>> {
    let current = repo.op_id().clone();
    let mut ops = op_walk::walk_ancestors(slice::from_ref(repo.operation())).boxed_local();
    let mut pending = Vec::with_capacity(batch);
    let mut seen = 0;
    while let Some(op) = ops.try_next().await.context("failed to walk the op log")? {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        if seen == limit {
            emit(std::mem::take(&mut pending));
            return Ok(Some(true));
        }
        seen += 1;
        pending.push(operation_entry(&op, &current));
        if pending.len() == batch {
            emit(std::mem::replace(&mut pending, Vec::with_capacity(batch)));
            // The actor reads cancels on the same thread; let it.
            tokio::task::yield_now().await;
        }
    }
    if !pending.is_empty() {
        emit(pending);
    }
    Ok(Some(false))
}

pub(crate) async fn load_operation(loader: &RepoLoader, hex: &str) -> Result<Operation> {
    let id =
        OperationId::try_from_hex(hex).with_context(|| format!("invalid jj operation id {hex}"))?;
    let data = loader
        .op_store()
        .read_operation(&id)
        .await
        .with_context(|| format!("failed to read operation {hex}"))?;
    Ok(Operation::new(loader.op_store().clone(), id, data))
}

async fn brief(repo: &ReadonlyRepo, commit: &Commit) -> Result<CommitBrief> {
    let mut brief = brief_without_emptiness(repo, commit).await?;
    brief.empty = commit.is_empty(repo).await.unwrap_or(false);
    Ok(brief)
}

async fn brief_without_emptiness(repo: &ReadonlyRepo, commit: &Commit) -> Result<CommitBrief> {
    let visible = !commit
        .is_hidden(repo)
        .await
        .context("failed to check commit visibility")?;
    Ok(CommitBrief {
        commit_id: commit.id().hex(),
        change_id: commit.change_id().to_string(),
        summary: commit.description().lines().next().unwrap_or("").to_owned(),
        author: commit.author().name.clone(),
        time_millis: commit.committer().timestamp.timestamp.0,
        empty: false,
        visible,
    })
}

async fn load_commit_id(repo: &ReadonlyRepo, id: &CommitId) -> Result<Commit> {
    repo.store()
        .get_commit_async(id)
        .await
        .with_context(|| format!("failed to load jj commit {}", id.hex()))
}

async fn brief_by_id(repo: &ReadonlyRepo, id: &CommitId) -> Result<CommitBrief> {
    brief(repo, &load_commit_id(repo, id).await?).await
}

async fn ref_side(repo: &ReadonlyRepo, target: &RefTarget) -> Result<Vec<CommitBrief>> {
    let mut out = Vec::new();
    for id in target.added_ids() {
        out.push(brief_by_id(repo, id).await?);
    }
    Ok(out)
}

/// What `operation_hex` changed relative to its parent — `jj op show`'s
/// "Changed commits" plus its working-copy, bookmark and tag sections.
/// `current` is the repo at head, which decides what is still visible.
pub(crate) async fn show_operation(
    current: &ReadonlyRepo,
    operation_hex: &str,
) -> Result<OperationChanges> {
    let loader = current.loader();
    let op = load_operation(loader, operation_hex).await?;
    let entry = operation_entry(&op, current.op_id());
    let parents = op
        .parents()
        .await
        .context("failed to read operation parents")?;
    let parent = match parents.as_slice() {
        [parent] => parent.clone(),
        [] => {
            return Ok(OperationChanges {
                operation: entry,
                commits: Vec::new(),
                commits_truncated: false,
                refs: Vec::new(),
                is_merge: false,
            });
        }
        _ => {
            return Ok(OperationChanges {
                operation: entry,
                commits: Vec::new(),
                commits_truncated: false,
                refs: Vec::new(),
                is_merge: true,
            });
        }
    };
    let from_repo = loader
        .load_at(&parent)
        .await
        .context("failed to load the repo before the operation")?;
    let to_repo = loader
        .load_at(&op)
        .await
        .context("failed to load the repo at the operation")?;

    let (commits, commits_truncated) = changed_commits(current, &from_repo, &to_repo).await?;

    let mut refs = Vec::new();
    for (name, (from, to)) in diff_named_commit_ids(
        from_repo.view().wc_commit_ids(),
        to_repo.view().wc_commit_ids(),
    ) {
        let side = async |id: Option<&CommitId>| -> Result<Vec<CommitBrief>> {
            match id {
                Some(id) => Ok(vec![brief_by_id(current, id).await?]),
                None => Ok(Vec::new()),
            }
        };
        refs.push(RefChange {
            kind: RefKind::WorkingCopy,
            name: format!("{}@", name.as_str()),
            from: side(from).await?,
            to: side(to).await?,
        });
    }
    for (name, (from, to)) in diff_named_ref_targets(
        from_repo.view().local_bookmarks(),
        to_repo.view().local_bookmarks(),
    ) {
        refs.push(RefChange {
            kind: RefKind::Bookmark,
            name: name.as_str().to_owned(),
            from: ref_side(current, from).await?,
            to: ref_side(current, to).await?,
        });
    }
    for (name, (from, to)) in
        diff_named_ref_targets(from_repo.view().local_tags(), to_repo.view().local_tags())
    {
        refs.push(RefChange {
            kind: RefKind::Tag,
            name: name.as_str().to_owned(),
            from: ref_side(current, from).await?,
            to: ref_side(current, to).await?,
        });
    }
    for (symbol, (from, to)) in diff_named_remote_refs(
        from_repo.view().all_remote_bookmarks(),
        to_repo.view().all_remote_bookmarks(),
    ) {
        // The colocated git repo's refs mirror the local bookmarks; `jj op
        // show` skips them too.
        if symbol.remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO {
            continue;
        }
        refs.push(RefChange {
            kind: RefKind::RemoteBookmark,
            name: format!("{}@{}", symbol.name.as_str(), symbol.remote.as_str()),
            from: ref_side(current, &from.target).await?,
            to: ref_side(current, &to.target).await?,
        });
    }

    Ok(OperationChanges {
        operation: entry,
        commits,
        commits_truncated,
        refs,
        is_merge: false,
    })
}

/// `compute_operation_commits_diff` from jj-cli, without the
/// `op-diff-changes-in` elision: commits that became visible are created or
/// rewritten (their predecessors come from the operation's own records, else
/// a hidden commit with the same change id), and whatever went hidden without
/// a successor was abandoned.
async fn changed_commits(
    current: &ReadonlyRepo,
    from_repo: &ReadonlyRepo,
    to_repo: &ReadonlyRepo,
) -> Result<(Vec<ChangedCommit>, bool)> {
    let from_heads: Vec<CommitId> = from_repo.view().heads().iter().cloned().collect();
    let to_heads: Vec<CommitId> = to_repo.view().heads().iter().cloned().collect();
    let from_expr = RevsetExpression::commits(from_heads);
    let to_expr = RevsetExpression::commits(to_heads);
    let predecessors = accumulate_predecessors(
        slice::from_ref(to_repo.operation()),
        slice::from_ref(from_repo.operation()),
    )
    .await
    .context("failed to read the operation's predecessor records")?;

    // Evaluated in the repo *at* the operation: its index holds both sides.
    let repo: &dyn Repo = to_repo;
    let mut hidden_by_change: HashMap<ChangeId, CommitId> = HashMap::new();
    let mut abandoned: Vec<CommitId> = Vec::new();
    let newly_hidden = to_expr
        .range(&from_expr)
        .evaluate(repo)
        .context("failed to evaluate newly hidden commits")?;
    let mut stream = newly_hidden.commit_change_ids();
    while let Some((commit_id, change_id)) = stream
        .try_next()
        .await
        .context("failed to walk newly hidden commits")?
    {
        hidden_by_change
            .entry(change_id)
            .or_insert_with(|| commit_id.clone());
        abandoned.push(commit_id);
    }
    drop(stream);

    let mut commits = Vec::new();
    let mut truncated = false;
    let mut replaced: HashSet<CommitId> = HashSet::new();
    let newly_visible = from_expr
        .range(&to_expr)
        .evaluate(repo)
        .context("failed to evaluate newly visible commits")?;
    let mut stream = newly_visible.commit_change_ids();
    while let Some((commit_id, change_id)) = stream
        .try_next()
        .await
        .context("failed to walk newly visible commits")?
    {
        let predecessor_ids: Vec<CommitId> = match predecessors.get(&commit_id) {
            Some(ids) => ids.clone(),
            None => hidden_by_change
                .get(&change_id)
                .cloned()
                .into_iter()
                .collect(),
        };
        replaced.extend(predecessor_ids.iter().cloned());
        if commits.len() == MAX_CHANGED_COMMITS {
            truncated = true;
            continue;
        }
        let mut previous = Vec::with_capacity(predecessor_ids.len());
        for id in &predecessor_ids {
            previous.push(brief_by_id(current, id).await?);
        }
        let commit = load_commit_id(current, &commit_id).await?;
        let rebased = match predecessor_ids.as_slice() {
            [only] => {
                let predecessor = load_commit_id(current, only).await?;
                predecessor.parent_ids() != commit.parent_ids()
                    && predecessor.description() == commit.description()
            }
            _ => false,
        };
        commits.push(ChangedCommit {
            kind: if previous.is_empty() {
                CommitChangeKind::Created
            } else {
                CommitChangeKind::Rewritten
            },
            commit: brief(current, &commit).await?,
            predecessors: previous,
            rebased,
        });
    }
    drop(stream);

    for commit_id in abandoned {
        if replaced.contains(&commit_id) {
            continue;
        }
        if commits.len() == MAX_CHANGED_COMMITS {
            truncated = true;
            break;
        }
        commits.push(ChangedCommit {
            kind: CommitChangeKind::Abandoned,
            commit: brief_by_id(current, &commit_id).await?,
            predecessors: Vec::new(),
            rebased: false,
        });
    }
    Ok((commits, truncated))
}

/// The versions of `revision`'s change, newest first (`jj evolog`).
pub(crate) async fn evolog(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
) -> Result<Vec<EvologEntry>> {
    let start = resolve_revision(repo, workspace_name, revision).await?;
    let mut walk = walk_predecessors(repo, slice::from_ref(&start)).boxed_local();
    let mut entries = Vec::new();
    while let Some(entry) = walk
        .try_next()
        .await
        .context("failed to walk the change's predecessors")?
    {
        if entries.len() == MAX_EVOLOG_ENTRIES {
            break;
        }
        let operation = entry.operation.as_ref().map(|op| OperationRef {
            id: op.id().hex(),
            summary: op
                .metadata()
                .description
                .lines()
                .next()
                .unwrap_or("")
                .to_owned(),
            time_millis: op.metadata().time.end.timestamp.0,
        });
        entries.push(EvologEntry {
            commit: brief(repo, &entry.commit).await?,
            operation,
            predecessor_ids: entry.predecessor_ids().iter().map(ObjectId::hex).collect(),
        });
    }
    Ok(entries)
}

async fn load_commit(repo: &ReadonlyRepo, hex: &str) -> Result<Commit> {
    let id = CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?;
    repo.store()
        .get_commit_async(&id)
        .await
        .with_context(|| format!("failed to load jj commit {hex}"))
}

/// Diff a commit (visible or not) against its parents, or against its
/// predecessors rebased onto its parents.
pub(crate) async fn history_diff(
    repo: &ReadonlyRepo,
    repository: &Repository,
    target: &HistoryDiffTarget,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let matcher = repo_scope_matcher(repository)?;
    match target {
        HistoryDiffTarget::Commit { commit_id } => {
            let commit = load_commit(repo, commit_id).await?;
            let old_tree = commit
                .parent_tree(repo)
                .await
                .context("failed to load the parent tree")?;
            let document = diff_trees(repo, &old_tree, &commit.tree(), matcher.as_ref()).await?;
            Ok((document, Some(jj_revision_details(repo, &commit))))
        }
        HistoryDiffTarget::InterDiff { from, to } => {
            let to = load_commit(repo, to).await?;
            let mut sources = Vec::with_capacity(from.len());
            for hex in from {
                sources.push(load_commit(repo, hex).await?);
            }
            if sources.is_empty() {
                bail!("an interdiff needs at least one predecessor");
            }
            let old_tree = rebase_to_dest_parent(repo, &sources, &to)
                .await
                .context("failed to rebase the predecessors onto the new parents")?;
            let document = diff_trees(repo, &old_tree, &to.tree(), matcher.as_ref()).await?;
            Ok((document, Some(jj_revision_details(repo, &to))))
        }
    }
}

/// Which commit introduced each line of `path` at `revision`, searching all
/// of its ancestors (`jj file annotate`).
pub(crate) async fn annotate(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
    path: &str,
) -> Result<FileAnnotation> {
    let start = resolve_revision(repo, workspace_name, revision).await?;
    let commit = repo
        .store()
        .get_commit_async(&start)
        .await
        .with_context(|| format!("failed to load jj commit {}", start.hex()))?;
    let repo_path = RepoPathBuf::from_internal_string(path.to_owned())
        .map_err(|_| anyhow::anyhow!("invalid repo path {path}"))?;
    let value = commit
        .tree()
        .path_value(&repo_path)
        .await
        .with_context(|| format!("failed to look up {path}"))?;
    if value.is_absent() {
        bail!("{path} does not exist in this revision");
    }
    if value.to_file_merge().is_none() {
        bail!("{path} is not a regular file");
    }
    let mut annotator = FileAnnotator::from_commit(&commit, &repo_path)
        .await
        .with_context(|| format!("failed to read {path}"))?;
    annotator
        .compute(repo, &RevsetExpression::all())
        .await
        .with_context(|| format!("failed to annotate {path}"))?;
    let annotation = annotator.to_annotation();

    let mut origins: Vec<CommitBrief> = Vec::new();
    let mut index_of: HashMap<CommitId, u32> = HashMap::new();
    let mut lines = Vec::new();
    // Lines whose origin lies outside the searched domain point at the commit
    // the search stopped at, which is what the CLI prints too.
    for (origin, _line) in annotation.lines() {
        let id = match origin {
            Ok(id) | Err(id) => id,
        };
        let index = match index_of.get(id) {
            Some(index) => *index,
            None => {
                let index = origins.len() as u32;
                // No emptiness check: a commit a line came from is never
                // empty, and the check costs a tree diff per origin.
                let commit = load_commit_id(repo, id).await?;
                origins.push(brief_without_emptiness(repo, &commit).await?);
                index_of.insert(id.clone(), index);
                index
            }
        };
        lines.push(index);
    }
    Ok(FileAnnotation {
        revision: revision.clone(),
        path: path.to_owned(),
        lines,
        origins,
    })
}

/// The operation a restore goes back to, and its view, checked for this
/// workspace's working copy before anything is written.
pub(crate) async fn restore_target(
    loader: &RepoLoader,
    operation_hex: &str,
    workspace_name: &WorkspaceName,
) -> Result<(Operation, jj_lib::op_store::View)> {
    let operation = load_operation(loader, operation_hex).await?;
    let view = operation
        .view()
        .await
        .context("failed to read the operation's view")?
        .store_view()
        .clone();
    if !view.wc_commit_ids.contains_key(workspace_name) {
        bail!(
            "this workspace did not exist at that operation; restoring it would remove its working copy"
        );
    }
    Ok((operation, view))
}

/// jj-cli's `view_with_desired_portions_restored` with its default `--what`
/// (the repo and the remote-tracking refs): everything comes from `restored`
/// except the git ref mirrors, which keep tracking the real git repository.
pub(crate) fn restored_view(
    restored: &jj_lib::op_store::View,
    current: &jj_lib::op_store::View,
) -> jj_lib::op_store::View {
    jj_lib::op_store::View {
        head_ids: restored.head_ids.clone(),
        local_bookmarks: restored.local_bookmarks.clone(),
        local_tags: restored.local_tags.clone(),
        remote_views: restored.remote_views.clone(),
        git_refs: current.git_refs.clone(),
        git_heads: current.git_heads.clone(),
        wc_commit_ids: restored.wc_commit_ids.clone(),
    }
}

/// [`annotate`] against a repo of its own, loaded at `operation_hex` — so a
/// caller can run it on another thread instead of holding up the actor for
/// the seconds a long file's ancestry takes.
pub(crate) async fn annotate_at_operation(
    settings: &jj_lib::settings::UserSettings,
    root: &std::path::Path,
    operation_hex: &str,
    revision: &RevisionSelection,
    path: &str,
) -> Result<FileAnnotation> {
    let workspace = super::workspace::load_workspace(settings, root)?;
    let loader = workspace.repo_loader();
    let operation = load_operation(loader, operation_hex).await?;
    let repo = loader
        .load_at(&operation)
        .await
        .context("failed to load the repo to annotate")?;
    annotate(&repo, workspace.workspace_name(), revision, path).await
}
