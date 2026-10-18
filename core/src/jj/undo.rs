//! `jj undo`, `jj redo` and `jj op revert`, as jj 0.45 runs them, adapted to a
//! working copy that is snapshotted on every file save.
//!
//! Undo and redo walk the CLI's own stacks: an undo writes an operation
//! described `undo: restore to operation <id>`, and the next undo (from diffui
//! or the CLI) reads that id to step one further back instead of undoing the
//! undo; redo walks the matching `redo: …` stack forward. The descriptions are
//! byte-for-byte jj's, which is what lets the two interleave.
//!
//! The one deliberate difference is snapshot operations. The CLI snapshots
//! right before it undoes, so `jj undo` after a file edit undoes *the edit*.
//! diffui writes a snapshot op on every save, so under that rule ⌘Z would
//! almost always revert the last save. Here snapshot operations are invisible
//! to the stacks — the op undone is the last real one — and the file edits
//! those snapshots recorded since it are carried over onto whatever `@` the
//! restore lands on, the way an editor keeps unsaved text across an undo of
//! something else. When no snapshot came in between, the result is exactly
//! the view `jj undo` / `jj redo` would have restored.

use anyhow::{Context, Result, bail};
use jj_lib::{
    backend::CommitId,
    merge::Merge,
    merged_tree::MergedTree,
    object_id::ObjectId,
    op_store::{OperationId, View},
    operation::Operation,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo, RepoLoader},
    transaction::Transaction,
};

/// jj-cli's `UNDO_OP_DESC_PREFIX` (`cli/src/commands/undo.rs`).
pub(crate) const UNDO_OP_DESC_PREFIX: &str = "undo: restore to operation ";
/// jj-cli's `REDO_OP_DESC_PREFIX` (`cli/src/commands/redo.rs`).
pub(crate) const REDO_OP_DESC_PREFIX: &str = "redo: restore to operation ";

/// What an undo, redo or revert will do, worked out before anything is
/// written: every refusal ("nothing to redo", "can't undo a merge operation")
/// has to happen before the pre-op working-copy fold is committed.
pub(crate) struct OpRestorePlan {
    kind: PlanKind,
    /// The operation description jj would write.
    pub(crate) op_description: String,
    /// What the activity row says.
    pub(crate) message: String,
}

enum PlanKind {
    /// `jj undo` / `jj redo`: put the repo portions of `restore`'s view back,
    /// then carry the snapshotted edits made since `latest_real` onto `@`.
    Restore {
        restore: Operation,
        latest_real: Operation,
    },
    /// `jj op revert <op>`: merge `parent - op` into the current view, which
    /// keeps everything done since.
    Revert {
        operation: Operation,
        parent: Operation,
    },
}

/// `jj undo`: the latest real operation (snapshots skipped), or — when that is
/// itself an undo — the operation it restored to, one step further back.
pub(crate) async fn plan_undo(loader: &RepoLoader, head: &Operation) -> Result<OpRestorePlan> {
    let latest_real = skip_snapshots(head.clone()).await?;
    let mut target = latest_real.clone();
    if let Some(restored) = pointer(&target, UNDO_OP_DESC_PREFIX, loader).await? {
        // Undoing an undo steps further back instead of redoing. A snapshot
        // the stack points at is a file save; step past it as well.
        target = skip_snapshots(restored).await?;
    }
    let mut parent = match target.parents().await?.as_slice() {
        [] => bail!("nothing to undo: this is the repository's first operation"),
        [parent] => parent.clone(),
        _ => bail!("can't undo a merge operation; revert a specific operation instead"),
    };
    // Restore the original rather than chain undo operations (jj does the
    // same, so a long `new; undo` history stays a short walk).
    if let Some(original) = pointer(&parent, UNDO_OP_DESC_PREFIX, loader).await? {
        parent = original;
    }
    Ok(OpRestorePlan {
        op_description: format!("{UNDO_OP_DESC_PREFIX}{}", parent.id().hex()),
        message: format!("Undid: {}", summary(&target)),
        kind: PlanKind::Restore {
            restore: parent,
            latest_real,
        },
    })
}

/// `jj redo`: re-apply what the latest undo took back, walking forward
/// through earlier redos the same way undo walks back.
pub(crate) async fn plan_redo(loader: &RepoLoader, head: &Operation) -> Result<OpRestorePlan> {
    let latest_real = skip_snapshots(head.clone()).await?;
    let mut target = latest_real.clone();
    if let Some(redone) = pointer(&target, REDO_OP_DESC_PREFIX, loader).await? {
        target = redone;
    }
    if !target
        .metadata()
        .description
        .starts_with(UNDO_OP_DESC_PREFIX)
    {
        bail!("nothing to redo");
    }
    let mut parent = match target.parents().await?.as_slice() {
        [parent] => parent.clone(),
        _ => bail!("the undo operation to redo has no single parent"),
    };
    if let Some(original) = pointer(&parent, REDO_OP_DESC_PREFIX, loader).await? {
        parent = original;
    }
    // Name what comes back: the real operation the restored state ends on.
    let redone = skip_snapshots(parent.clone()).await?;
    Ok(OpRestorePlan {
        op_description: format!("{REDO_OP_DESC_PREFIX}{}", parent.id().hex()),
        message: format!("Redid: {}", summary(&redone)),
        kind: PlanKind::Restore {
            restore: parent,
            latest_real,
        },
    })
}

/// `jj op revert <op>`: undo exactly that operation, keeping everything done
/// since — the activity row's per-operation Revert.
pub(crate) async fn plan_revert(
    loader: &RepoLoader,
    base_repo: &ReadonlyRepo,
    hex: &str,
) -> Result<OpRestorePlan> {
    let target = super::mutate::resolve_undo_target(loader, base_repo, Some(hex)).await?;
    Ok(OpRestorePlan {
        op_description: format!("revert operation {}", target.operation.id().hex()),
        message: format!(
            "Reverted: {}",
            target
                .description
                .strip_prefix("diffui: ")
                .unwrap_or(&target.description)
        ),
        kind: PlanKind::Revert {
            operation: target.operation,
            parent: target.parent,
        },
    })
}

impl OpRestorePlan {
    /// Apply the plan to `tx`, whose base is the current head (with any
    /// on-disk edits already committed as a snapshot op of their own).
    pub(crate) async fn apply(
        &self,
        tx: &mut Transaction,
        loader: &RepoLoader,
        workspace_name: &WorkspaceName,
    ) -> Result<()> {
        let current = tx.base_repo().view().store_view().clone();
        match &self.kind {
            PlanKind::Revert { operation, parent } => {
                let op_repo = loader
                    .load_at(operation)
                    .await
                    .context("failed to load the repo at the operation to revert")?;
                let parent_repo = loader
                    .load_at(parent)
                    .await
                    .context("failed to load the repo before the operation to revert")?;
                tx.repo_mut()
                    .merge(&op_repo, &parent_repo)
                    .await
                    .context("failed to merge the revert into the current view")?;
                let merged = tx.repo().view().store_view().clone();
                if !merged.wc_commit_ids.contains_key(workspace_name) {
                    bail!("reverting this operation would remove this workspace's working copy");
                }
                tx.repo_mut()
                    .set_view(view_with_repo_portions_restored(&merged, &current));
            }
            PlanKind::Restore {
                restore,
                latest_real,
            } => {
                let restored = restore
                    .view()
                    .await
                    .context("failed to read the view to restore")?;
                tx.repo_mut().set_view(view_with_repo_portions_restored(
                    restored.store_view(),
                    &current,
                ));
                carry_snapshotted_edits(tx, loader, workspace_name, latest_real).await?;
            }
        }
        Ok(())
    }
}

/// jj-cli's `view_with_desired_portions_restored` with its default `--what`
/// (the repo and the remote-tracking bookmarks): everything comes from
/// `restored` except the git-ref bookkeeping, which has to keep describing
/// what the git repo actually holds now so the next export diffs correctly.
fn view_with_repo_portions_restored(restored: &View, current: &View) -> View {
    View {
        head_ids: restored.head_ids.clone(),
        local_bookmarks: restored.local_bookmarks.clone(),
        local_tags: restored.local_tags.clone(),
        remote_views: restored.remote_views.clone(),
        git_refs: current.git_refs.clone(),
        git_heads: current.git_heads.clone(),
        wc_commit_ids: restored.wc_commit_ids.clone(),
    }
}

/// Re-apply the file edits that snapshots recorded after `latest_real` onto
/// the `@` the restored view names, as a three-way merge (`restored @` +
/// `head @` − `@ at latest_real`). A no-op when no snapshot came in between,
/// which keeps a plain undo identical to the CLI's.
async fn carry_snapshotted_edits(
    tx: &mut Transaction,
    loader: &RepoLoader,
    workspace_name: &WorkspaceName,
    latest_real: &Operation,
) -> Result<()> {
    let head = tx.base_repo().clone();
    if head.op_id() == latest_real.id() {
        return Ok(());
    }
    let Some(edited) = wc_tree(&head, workspace_name).await? else {
        return Ok(());
    };
    let before_repo = loader
        .load_at(latest_real)
        .await
        .context("failed to load the repo at the last real operation")?;
    let Some(before) = wc_tree(&before_repo, workspace_name).await? else {
        return Ok(());
    };
    if edited.tree_ids_and_labels() == before.tree_ids_and_labels() {
        return Ok(());
    }
    let Some(target_id) = tx.repo().view().get_wc_commit_id(workspace_name).cloned() else {
        return Ok(());
    };
    let target = tx
        .repo()
        .store()
        .get_commit_async(&target_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", target_id.hex()))?;
    let tree = MergedTree::merge(Merge::from_vec(vec![
        (target.tree(), "restored working copy".to_owned()),
        (before, "working copy before the edits".to_owned()),
        (edited, "edited working copy".to_owned()),
    ]))
    .await
    .context("failed to carry the working-copy edits over")?;
    let carried = tx
        .repo_mut()
        .rewrite_commit(&target)
        .set_tree(tree)
        .write()
        .await
        .context("failed to write the working copy with the carried edits")?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.to_owned(), carried.id().clone())
        .context("failed to update the working-copy pointer")?;
    Ok(())
}

async fn wc_tree(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
) -> Result<Option<MergedTree>> {
    let Some(id): Option<&CommitId> = repo.view().get_wc_commit_id(workspace_name) else {
        return Ok(None);
    };
    let commit = repo
        .store()
        .get_commit_async(id)
        .await
        .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
    Ok(Some(commit.tree()))
}

/// Walk back past snapshot operations to the latest real one. Stops at an
/// operation with several parents (a concurrent-op merge), which is real.
async fn skip_snapshots(mut operation: Operation) -> Result<Operation> {
    while operation.metadata().is_snapshot {
        let parents = operation
            .parents()
            .await
            .context("failed to read operation parents")?;
        match parents.as_slice() {
            [parent] => operation = parent.clone(),
            _ => break,
        }
    }
    Ok(operation)
}

/// The operation an undo/redo operation's description names, if `operation`
/// is one of those (`prefix` picks which stack).
async fn pointer(
    operation: &Operation,
    prefix: &str,
    loader: &RepoLoader,
) -> Result<Option<Operation>> {
    let Some(hex) = operation.metadata().description.strip_prefix(prefix) else {
        return Ok(None);
    };
    let id = OperationId::try_from_hex(hex.trim()).with_context(|| {
        format!(
            "malformed operation id in {:?}",
            operation.metadata().description
        )
    })?;
    let target = loader
        .load_operation(&id)
        .await
        .context("failed to load the operation the undo stack points at")?;
    Ok(Some(target))
}

/// One line naming an operation for the activity log: its description's first
/// line without diffui's own `diffui: ` tag.
fn summary(operation: &Operation) -> String {
    let line = operation
        .metadata()
        .description
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    let line = line.strip_prefix("diffui: ").unwrap_or(line);
    if line.is_empty() {
        "operation".to_owned()
    } else {
        line.to_owned()
    }
}
