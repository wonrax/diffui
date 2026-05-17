//! Mutating jj operations. Wraps the jj-lib transaction api so the rest
//! of the app talks to a single `run_mutation(...)` entry point.
//!
//! Each op opens a transaction, applies its changes, and commits. The
//! returned `MutationOutcome` carries the new operation id (used by the
//! toast / undo subscription) and a short description of what was done
//! (used by the toast text).
//!
//! All ops here are recoverable via `jj op undo` — the operation itself
//! is also a mutation we route through this module.

use anyhow::{Context, Result, anyhow};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    object_id::ObjectId,
    op_store::RefTarget,
    ref_name::RefName,
    repo::{ReadonlyRepo, Repo, StoreFactories},
    rewrite::{CommitWithSelection, merge_commit_trees, rebase_commit, squash_commits},
    workspace::{Workspace, default_working_copy_factories},
};
use std::sync::Arc;

use crate::backend::RevisionSelection;
use crate::jj::jj_settings;
use crate::palette::PlacementKind;
use crate::repository::Repository;

/// A concrete mutation request. Built from an `OpDraft` at apply time —
/// the palette layer translates user-facing knobs (selection, source mode,
/// placement) into the raw jj-lib arguments captured here.
///
/// Source-side revisions are passed as `RevisionSelection` so the
/// working-copy case (no stable change-id) stays representable.
#[derive(Debug, Clone)]
pub enum MutationOp {
    /// Revert the most recent visible operation (`jj op undo`). Applies
    /// the reverse of the current op's diff to head.
    OpUndo,
    /// Rewrite the target's commit message.
    Describe {
        target: RevisionSelection,
        message: String,
    },
    /// Move the working copy to the target (`jj edit`).
    Edit { target: RevisionSelection },
    /// Discard the targets and re-parent any descendants onto the
    /// targets' parents (`jj abandon`). Operates on each target
    /// independently.
    Abandon { targets: Vec<RevisionSelection> },
    /// Squash the source commits into the destination (`jj squash`).
    /// Each source is abandoned and its tree changes are folded into
    /// the destination. Uses whole-commit selection — interactive hunk
    /// picking is a future feature.
    Squash {
        sources: Vec<RevisionSelection>,
        destination: RevisionSelection,
    },
    /// Rebase the source commits onto a destination (`jj rebase`). Only
    /// the `Onto` placement is wired right now — `--insert-after` /
    /// `--insert-before` come in a later chunk. Source mode is always
    /// `-s` (descendants follow) regardless of what the op pad shows;
    /// the mode radio is currently read-only.
    Rebase {
        sources: Vec<RevisionSelection>,
        destination: RevisionSelection,
        placement: PlacementKind,
    },
    /// Create a new commit (`jj new`). `parents` becomes the parent list
    /// (multi-parent = merge commit). `message` is set on the new commit
    /// at creation time. The working copy moves to the new commit.
    New {
        parents: Vec<RevisionSelection>,
        message: String,
    },
    /// Create or move a local bookmark to `target` (`jj bookmark set`).
    /// Errors if `name` is empty or contains whitespace.
    BookmarkSet {
        name: String,
        target: RevisionSelection,
    },
    /// Delete a local bookmark (`jj bookmark delete`). Errors if the
    /// bookmark doesn't exist.
    BookmarkDelete { name: String },
}

/// Result of a successful mutation. Drives the toast text and the
/// post-mutation reload.
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    /// User-facing summary, e.g. `"Reverted operation a1b2c3"`. Becomes
    /// the toast body.
    pub message: String,
    /// New operation id this mutation produced.
    #[allow(dead_code)]
    pub new_op_id: String,
}

/// Run `op` against `repository` and return the outcome.
///
/// jj-lib's internals hold `!Send` state (`RefCell`, `OnceCell`, etc.), so
/// the inner future itself isn't `Send`. We wrap the whole call in
/// `spawn_blocking + handle.block_on(...)` — the same pattern `jj.rs` uses
/// for the read path — to keep the iced async runtime happy while still
/// running the jj-lib work on a dedicated thread.
pub async fn run_mutation(
    repository: Repository,
    op: MutationOp,
) -> std::result::Result<MutationOutcome, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(run_mutation_inner(repository, op)))
        .await
        .map_err(|e| format!("mutation task panicked: {e}"))?
}

async fn run_mutation_inner(
    repository: Repository,
    op: MutationOp,
) -> std::result::Result<MutationOutcome, String> {
    let result = match op {
        MutationOp::OpUndo => jj_op_undo(repository).await,
        MutationOp::Describe { target, message } => jj_describe(repository, target, message).await,
        MutationOp::Edit { target } => jj_edit(repository, target).await,
        MutationOp::Abandon { targets } => jj_abandon(repository, targets).await,
        MutationOp::Squash {
            sources,
            destination,
        } => jj_squash(repository, sources, destination).await,
        MutationOp::Rebase {
            sources,
            destination,
            placement,
        } => jj_rebase(repository, sources, destination, placement).await,
        MutationOp::New { parents, message } => jj_new(repository, parents, message).await,
        MutationOp::BookmarkSet { name, target } => jj_bookmark_set(repository, name, target).await,
        MutationOp::BookmarkDelete { name } => jj_bookmark_delete(repository, name).await,
    };
    result.map_err(|e| format!("{e:#}"))
}

/// Load a workspace + head repo. Shared by every mutation entry point —
/// each then opens a transaction from the returned `base_repo`.
async fn load_workspace(repository: &Repository) -> Result<(Workspace, Arc<ReadonlyRepo>)> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let base_repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo head")?;
    Ok((workspace, base_repo))
}

/// Resolve a `RevisionSelection` against the loaded repo to a `Commit`.
async fn resolve_target(
    repo: &Arc<ReadonlyRepo>,
    workspace: &Workspace,
    target: &RevisionSelection,
) -> Result<Commit> {
    let commit_id = match target {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };
    repo.store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))
}

async fn jj_op_undo(repository: Repository) -> Result<MutationOutcome> {
    let (workspace, base_repo) = load_workspace(&repository).await?;
    let _ = &workspace; // keep workspace alive for the duration of the tx
    let repo_loader = base_repo.loader().clone();

    let current_op = base_repo.operation().clone();
    let parents = current_op
        .parents()
        .await
        .context("failed to read parents of current operation")?;

    // We only know how to undo a linear op. The op log can branch when
    // concurrent jj processes race; reconciling those requires choosing
    // which side to undo, which we don't have a ui for yet.
    let parent_op = match parents.as_slice() {
        [single] => single.clone(),
        [] => return Err(anyhow!("there is no operation to undo (already at root)")),
        many => {
            return Err(anyhow!(
                "current operation has {} parents — refusing to undo a divergent op (resolve via `jj op log` first)",
                many.len()
            ));
        }
    };

    let bad_op_repo = repo_loader
        .load_at(&current_op)
        .await
        .context("failed to load repo at current operation")?;
    let parent_op_repo = repo_loader
        .load_at(&parent_op)
        .await
        .context("failed to load repo at parent operation")?;

    let mut tx = base_repo.start_transaction();
    // Three-way merge: self = current head (current op), base = current
    // op state, other = parent state. The merge applies (parent - current)
    // to self → self becomes parent. That's exactly the "undo" effect.
    tx.repo_mut()
        .merge(&bad_op_repo, &parent_op_repo)
        .await
        .context("failed to merge parent-op state into current head")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after op undo merge")?;

    let undone_short = short_op_id(&current_op);
    let description = format!("undo operation {undone_short}");
    let new_repo = tx
        .commit(description.clone())
        .await
        .context("failed to commit op-undo transaction")?;
    let new_op_id = new_repo.op_id().hex();

    Ok(MutationOutcome {
        message: format!("Reverted operation {undone_short}"),
        new_op_id,
    })
}

fn short_op_id(op: &jj_lib::operation::Operation) -> String {
    let hex = op.id().hex();
    hex.chars().take(8).collect()
}

fn short_change_id(commit: &Commit) -> String {
    commit.change_id().hex().chars().take(8).collect()
}

async fn jj_describe(
    repository: Repository,
    target: RevisionSelection,
    message: String,
) -> Result<MutationOutcome> {
    let (workspace, base_repo) = load_workspace(&repository).await?;
    let commit = resolve_target(&base_repo, &workspace, &target).await?;
    let short = short_change_id(&commit);

    let mut tx = base_repo.start_transaction();
    tx.repo_mut()
        .rewrite_commit(&commit)
        .set_description(message)
        .write()
        .await
        .context("failed to write describe rewrite")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after describe")?;

    let new_repo = tx
        .commit(format!("describe commit {short}"))
        .await
        .context("failed to commit describe transaction")?;

    Ok(MutationOutcome {
        message: format!("Updated message of {short}"),
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_edit(repository: Repository, target: RevisionSelection) -> Result<MutationOutcome> {
    let (workspace, base_repo) = load_workspace(&repository).await?;
    let commit = resolve_target(&base_repo, &workspace, &target).await?;
    let short = short_change_id(&commit);
    let workspace_name = workspace.workspace_name().to_owned();

    let mut tx = base_repo.start_transaction();
    tx.repo_mut()
        .edit(workspace_name, &commit)
        .await
        .context("failed to set working copy to target commit")?;
    // `edit` may have abandoned the previous wc commit (if it was empty)
    // — that records a rewrite the transaction insists on resolving
    // before commit.
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after edit")?;

    let new_repo = tx
        .commit(format!("edit commit {short}"))
        .await
        .context("failed to commit edit transaction")?;

    Ok(MutationOutcome {
        message: format!("Working copy now at {short}"),
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_abandon(
    repository: Repository,
    targets: Vec<RevisionSelection>,
) -> Result<MutationOutcome> {
    if targets.is_empty() {
        return Err(anyhow!("abandon requires at least one target"));
    }
    let (workspace, base_repo) = load_workspace(&repository).await?;

    let mut resolved = Vec::with_capacity(targets.len());
    for t in &targets {
        resolved.push(resolve_target(&base_repo, &workspace, t).await?);
    }

    let mut tx = base_repo.start_transaction();
    for commit in &resolved {
        tx.repo_mut().record_abandoned_commit(commit);
    }
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after abandon")?;

    let new_repo = tx
        .commit(format!("abandon {} commit(s)", resolved.len()))
        .await
        .context("failed to commit abandon transaction")?;

    let summary = if resolved.len() == 1 {
        format!("Abandoned {}", short_change_id(&resolved[0]))
    } else {
        format!("Abandoned {} commits", resolved.len())
    };

    Ok(MutationOutcome {
        message: summary,
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_squash(
    repository: Repository,
    sources: Vec<RevisionSelection>,
    destination: RevisionSelection,
) -> Result<MutationOutcome> {
    if sources.is_empty() {
        return Err(anyhow!("squash requires at least one source"));
    }
    let (workspace, base_repo) = load_workspace(&repository).await?;

    let dest_commit = resolve_target(&base_repo, &workspace, &destination).await?;
    let mut source_commits = Vec::with_capacity(sources.len());
    for s in &sources {
        source_commits.push(resolve_target(&base_repo, &workspace, s).await?);
    }
    // Refuse to squash a commit into itself — jj-lib's squash_commits
    // would generate an empty rewrite and we'd burn an op on it.
    if source_commits.iter().any(|c| c.id() == dest_commit.id()) {
        return Err(anyhow!("cannot squash a commit into itself"));
    }

    let mut tx = base_repo.start_transaction();

    // Build full-selection CommitWithSelection entries (whole-commit
    // squash). Interactive hunk selection would set selected_tree to a
    // subset of the source tree; we don't expose that knob yet.
    let mut selections = Vec::with_capacity(source_commits.len());
    for commit in &source_commits {
        let parent_tree = commit
            .parent_tree(tx.repo())
            .await
            .context("failed to load parent tree of squash source")?;
        selections.push(CommitWithSelection {
            commit: commit.clone(),
            selected_tree: commit.tree(),
            parent_tree,
        });
    }

    let squashed = squash_commits(tx.repo_mut(), &selections, &dest_commit, false)
        .await
        .context("failed to squash commits")?;
    let Some(squashed) = squashed else {
        return Err(anyhow!("squash produced no changes"));
    };
    // squash_commits returns a builder for the new destination commit;
    // we keep the original destination's message — that's the standard
    // squash behavior without `--use-destination-message`.
    squashed
        .commit_builder
        .write()
        .await
        .context("failed to write squashed destination commit")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after squash")?;

    let dest_short = short_change_id(&dest_commit);
    let new_repo = tx
        .commit(format!(
            "squash {} commit(s) into {dest_short}",
            sources.len()
        ))
        .await
        .context("failed to commit squash transaction")?;

    Ok(MutationOutcome {
        message: if sources.len() == 1 {
            format!("Squashed into {dest_short}")
        } else {
            format!("Squashed {} commits into {dest_short}", sources.len())
        },
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_rebase(
    repository: Repository,
    sources: Vec<RevisionSelection>,
    destination: RevisionSelection,
    placement: PlacementKind,
) -> Result<MutationOutcome> {
    if sources.is_empty() {
        return Err(anyhow!("rebase requires at least one source"));
    }
    if !matches!(placement, PlacementKind::Onto) {
        return Err(anyhow!(
            "only the `Onto` placement is wired in this build — `--insert-after` / `--insert-before` coming soon"
        ));
    }
    let (workspace, base_repo) = load_workspace(&repository).await?;

    let dest_commit = resolve_target(&base_repo, &workspace, &destination).await?;
    let mut source_commits = Vec::with_capacity(sources.len());
    for s in &sources {
        source_commits.push(resolve_target(&base_repo, &workspace, s).await?);
    }
    if source_commits.iter().any(|c| c.id() == dest_commit.id()) {
        return Err(anyhow!("cannot rebase a commit onto itself"));
    }

    let mut tx = base_repo.start_transaction();
    let new_parents = vec![dest_commit.id().clone()];
    for src in &source_commits {
        rebase_commit(tx.repo_mut(), src.clone(), new_parents.clone())
            .await
            .with_context(|| format!("failed to rebase commit {}", short_change_id(src)))?;
    }
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after rebase")?;

    let dest_short = short_change_id(&dest_commit);
    let new_repo = tx
        .commit(format!(
            "rebase {} commit(s) onto {dest_short}",
            sources.len()
        ))
        .await
        .context("failed to commit rebase transaction")?;

    Ok(MutationOutcome {
        message: if sources.len() == 1 {
            format!("Rebased onto {dest_short}")
        } else {
            format!("Rebased {} commits onto {dest_short}", sources.len())
        },
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_new(
    repository: Repository,
    parents: Vec<RevisionSelection>,
    message: String,
) -> Result<MutationOutcome> {
    if parents.is_empty() {
        return Err(anyhow!("new requires at least one parent"));
    }
    let (workspace, base_repo) = load_workspace(&repository).await?;
    let workspace_name = workspace.workspace_name().to_owned();

    let mut parent_commits = Vec::with_capacity(parents.len());
    for p in &parents {
        parent_commits.push(resolve_target(&base_repo, &workspace, p).await?);
    }

    let mut tx = base_repo.start_transaction();
    // For a multi-parent (merge) commit, the initial tree is the merge
    // of all parent trees. `merge_commit_trees` does the right thing for
    // both single-parent (returns parent tree) and multi-parent cases.
    let merged_tree = merge_commit_trees(tx.repo(), &parent_commits)
        .await
        .context("failed to merge parent trees for new commit")?;
    let parent_ids: Vec<CommitId> = parent_commits.iter().map(|c| c.id().clone()).collect();

    let mut builder = tx.repo_mut().new_commit(parent_ids, merged_tree);
    if !message.trim().is_empty() {
        builder = builder.set_description(message);
    }
    let new_commit = builder
        .write()
        .await
        .context("failed to write new commit")?;

    // Move the working copy to the new commit so the user can start
    // adding changes immediately.
    tx.repo_mut()
        .edit(workspace_name, &new_commit)
        .await
        .context("failed to point working copy at new commit")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after new")?;

    let new_repo = tx
        .commit("new commit")
        .await
        .context("failed to commit new-commit transaction")?;

    Ok(MutationOutcome {
        message: format!("Created new commit {}", short_change_id(&new_commit)),
        new_op_id: new_repo.op_id().hex(),
    })
}

fn validate_bookmark_name(raw: &str) -> Result<&str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(anyhow!("bookmark name is required"));
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return Err(anyhow!("bookmark name cannot contain whitespace"));
    }
    Ok(name)
}

async fn jj_bookmark_set(
    repository: Repository,
    name: String,
    target: RevisionSelection,
) -> Result<MutationOutcome> {
    let name = validate_bookmark_name(&name)?.to_owned();
    let (workspace, base_repo) = load_workspace(&repository).await?;
    let commit = resolve_target(&base_repo, &workspace, &target).await?;

    let mut tx = base_repo.start_transaction();
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new(&name), RefTarget::normal(commit.id().clone()));

    let short = short_change_id(&commit);
    let new_repo = tx
        .commit(format!("set bookmark {name} to {short}"))
        .await
        .context("failed to commit bookmark-set transaction")?;

    Ok(MutationOutcome {
        message: format!("Bookmark `{name}` → {short}"),
        new_op_id: new_repo.op_id().hex(),
    })
}

async fn jj_bookmark_delete(repository: Repository, name: String) -> Result<MutationOutcome> {
    let name = validate_bookmark_name(&name)?.to_owned();
    let (_workspace, base_repo) = load_workspace(&repository).await?;

    // Refuse to delete a bookmark that doesn't exist — silently
    // succeeding would burn an op and confuse the user.
    let current = base_repo.view().get_local_bookmark(RefName::new(&name));
    if current.is_absent() {
        return Err(anyhow!("no local bookmark named `{name}`"));
    }

    let mut tx = base_repo.start_transaction();
    tx.repo_mut()
        .set_local_bookmark_target(RefName::new(&name), RefTarget::absent());

    let new_repo = tx
        .commit(format!("delete bookmark {name}"))
        .await
        .context("failed to commit bookmark-delete transaction")?;

    Ok(MutationOutcome {
        message: format!("Deleted bookmark `{name}`"),
        new_op_id: new_repo.op_id().hex(),
    })
}
