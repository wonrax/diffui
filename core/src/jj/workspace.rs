use std::cell::OnceCell;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use jj_cli::{cli_util::short_operation_hash, ui::Ui};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    default_backend_factories::{default_backend_factories, default_working_copy_factories},
    git::{GitSettings, import_head, import_refs},
    gitignore::GitIgnoreFile,
    lock::FileLock,
    matchers::{Matcher, NothingMatcher},
    merged_tree::MergedTree,
    object_id::ObjectId,
    op_store::{OpStoreError, OperationId},
    ref_name::WorkspaceName,
    repo::{MutableRepo, ReadonlyRepo, Repo, RepoLoader},
    revset::{ResolvedRevsetExpression, RevsetExpression, SymbolResolver, SymbolResolverExtension},
    rewrite::RebaseOptions,
    settings::{HumanByteSize, UserSettings},
    working_copy::{SnapshotOptions, SnapshotStats, UntrackedReason, WorkingCopyFreshness},
    workspace::{LockedWorkspace, Workspace},
};

use super::settings::*;
use super::walk::parse_user_revset;
use crate::model::RevisionSelection;
use crate::repository::{Repository, RepositorySnapshot};

/// A stale working copy the user has to update, worded like jj-cli's error
/// with the command that fixes it. With `snapshot.auto-update-stale` unset (its
/// default) the CLI stops here rather than rewrite files under the user's
/// editors, and so does diffui.
#[derive(Debug)]
pub(crate) struct StaleWorkingCopy(pub(crate) String);

impl std::fmt::Display for StaleWorkingCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StaleWorkingCopy {}

/// What [`lock_working_copy`] leaves behind: either a locked working copy
/// whose disk has been snapshotted, or a recovered one whose lock is already
/// finished.
pub(crate) enum LockOutcome<'a> {
    Locked(LockedWorkingCopy<'a>),
    /// The checkout was stale, so it was recovered instead of snapshotted: the
    /// lock is finished at `repo`'s operation (the merged head) and `wc_commit`
    /// is what now sits on disk. A caller that needs a lock must take a new one.
    Recovered {
        repo: Arc<ReadonlyRepo>,
        wc_commit: Commit,
    },
}

/// A held working-copy lock over a snapshotted disk. `repo` / `wc_commit` are
/// what the disk is synced with; `tree` is what the disk holds now.
///
/// The lock leaves through exactly two doors: [`finish`](Self::finish), the
/// only happy-path exit, and [`abandon`](Self::abandon), which throws the work
/// away on purpose. Both are explicit calls — an early `?` that skipped them
/// used to leave the working copy pointing at an operation the repo had
/// already moved past, which jj then reports as a divergent `@`.
pub(crate) struct LockedWorkingCopy<'a> {
    locked_ws: Option<LockedWorkspace<'a>>,
    /// jj-cli's git import/export lock, held for the whole session in a
    /// colocated workspace so another jj can't import or export in between.
    _git_lock: Option<FileLock>,
    pub(crate) repo: Arc<ReadonlyRepo>,
    pub(crate) wc_commit: Commit,
    pub(crate) tree: MergedTree,
    /// Paths the snapshot declined to track (oversized files, most often).
    /// Surfaced once by the caller — the prologue is single now, so there is
    /// exactly one place this can come from.
    pub(crate) warnings: Vec<String>,
}

impl LockedWorkingCopy<'_> {
    /// Whether the disk differs from the commit it was checked out from.
    pub(crate) fn tree_changed(&self) -> bool {
        self.tree.tree_ids_and_labels() != self.wc_commit.tree().tree_ids_and_labels()
    }

    /// Put `commit`'s tree on disk. Only a mutation that moves `@` needs this;
    /// skipping it would leave the old files in place for the next snapshot to
    /// record into the moved-to commit.
    pub(crate) async fn check_out(&mut self, commit: &Commit) -> Result<()> {
        let locked = self
            .locked_ws
            .as_mut()
            .context("the jj working-copy lock was already released")?;
        locked
            .locked_wc()
            .check_out(commit)
            .await
            .context("failed to check out the new working copy")?;
        Ok(())
    }

    /// Persist the lock at `op` — the only happy-path exit.
    pub(crate) async fn finish(&mut self, op: OperationId) -> Result<()> {
        let locked = self
            .locked_ws
            .take()
            .context("the jj working-copy lock was already released")?;
        locked
            .finish(op)
            .await
            .context("failed to finish the jj working-copy mutation")?;
        Ok(())
    }

    /// Give the lock up without persisting anything. Explicit, so "we bailed"
    /// reads differently from "we forgot".
    pub(crate) fn abandon(&mut self) {
        self.locked_ws = None;
    }
}

impl Drop for LockedWorkingCopy<'_> {
    fn drop(&mut self) {
        if self.locked_ws.is_some() {
            tracing::warn!(
                "jj working-copy lock dropped without finish or abandon; \
                 the checkout may now report as stale"
            );
        }
    }
}

/// Lock the working copy, resolve the repo and `@` the disk is actually synced
/// with, and snapshot the on-disk tree. A stale checkout is never snapshotted:
/// with `snapshot.auto-update-stale` it is recovered, otherwise refused with
/// [`StaleWorkingCopy`], as jj-cli does.
///
/// Both prologues that fold the disk into `@` start here (the refresh path,
/// [`load_jj_repository_snapshot`], and the mutation path, [`apply_mutation`]),
/// so the stale check can't go missing from one of them: a mutation that
/// snapshotted a stale checkout would replace the rebased `@`'s tree with the
/// old disk tree, reverting whatever the other workspace or the CLI put there.
pub(crate) async fn lock_working_copy<'a>(
    workspace: &'a mut Workspace,
    repo_loader: &RepoLoader,
    workspace_name: &WorkspaceName,
    context: &SnapshotContext,
) -> Result<LockOutcome<'a>> {
    let snapshot_options = &context.options();
    // Take the working-copy lock *before* reading the repo head. Otherwise a
    // jj-cli command running between `load_at_head` and the lock can rewrite
    // the wc commit out from under us, and our snapshot tx — still parented on
    // the stale op — lands as a sibling of the cli's op. Both ops touch the
    // same change_id with different commit_ids, which jj's concurrent-op
    // resolver presents as a divergent change.
    let workspace_root = workspace.workspace_root().to_owned();
    // A colocated checkout first takes in what git did since the last look —
    // a `git commit`, a checkout, a pull — as jj-cli does before every
    // command, under its git import/export lock. Without it the graph shows
    // the old state and the next mutation resets git's index under a `HEAD`
    // that moved, so a plain `git commit` would commit a revert.
    let git_lock = if super::colocate::is_colocated(repo_loader.store(), &workspace_root)? {
        let git_lock = FileLock::lock(workspace.repo_path().join("git_import_export.lock"))
            .context("failed to take the git import/export lock")?;
        let place = Place {
            settings: repo_loader.settings(),
            workspace_name,
            workspace_root: &workspace_root,
        };
        import_git_head(workspace, repo_loader, &place).await?;
        import_git_refs(workspace, repo_loader, &place, snapshot_options).await?;
        Some(git_lock)
    } else {
        None
    };
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .await
        .context("failed to lock jj working copy")?;

    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let Some(wc_commit_id) = base_repo.view().get_wc_commit_id(workspace_name).cloned() else {
        return Err(StaleWorkingCopy(
            "This workspace was forgotten in another workspace (`jj workspace forget`), so it has \
             no working copy any more."
                .to_owned(),
        )
        .into());
    };
    let wc_commit = base_repo
        .store()
        .get_commit_async(&wc_commit_id)
        .await
        .with_context(|| {
            format!(
                "failed to load jj working-copy commit {}",
                wc_commit_id.hex()
            )
        })?;

    // The disk may have been checked out from a *different* commit than the
    // head view's `@` — another workspace's snapshot rebases this one's
    // working-copy commit (a mega merge of workspace heads, most notably),
    // leaving this working copy stale. Snapshotting regardless would amend
    // the rebased commit with the old on-disk tree, silently reverting the
    // other workspace's changes inside it — the exact rewrite the jj CLI's
    // "working copy is stale" error exists to prevent. Check first; recover
    // a stale copy like `jj workspace update-stale` instead of snapshotting.
    let old_op_id = locked_ws.locked_wc().old_operation_id().clone();
    let freshness = match WorkingCopyFreshness::check_stale(
        locked_ws.locked_wc(),
        &wc_commit,
        &base_repo,
    )
    .await
    {
        Ok(freshness) => freshness,
        Err(OpStoreError::ObjectNotFound { .. }) => {
            return Err(StaleWorkingCopy(
                "The operation the working copy was last updated at is gone (garbage \
                     collected). Run `jj workspace update-stale` to recover it."
                    .to_owned(),
            )
            .into());
        }
        Err(error) => {
            return Err(error).context("failed to check jj working-copy freshness");
        }
    };
    match freshness {
        WorkingCopyFreshness::WorkingCopyStale if !context.auto_update_stale => {
            return Err(StaleWorkingCopy(format!(
                "The working copy is stale (not updated since operation {}). Run \
                 `jj workspace update-stale` to update it.",
                short_operation_hash(&old_op_id)
            ))
            .into());
        }
        WorkingCopyFreshness::SiblingOperation if !context.auto_update_stale => {
            let short = short_operation_hash(&old_op_id);
            return Err(StaleWorkingCopy(format!(
                "The working copy was updated at operation {short}, which the operation log \
                 doesn't contain. Run `jj op integrate {short}` to add it."
            ))
            .into());
        }
        _ => {}
    }
    let (base_repo, _wc_commit_id, wc_commit) = match freshness {
        WorkingCopyFreshness::Fresh => (base_repo, wc_commit_id, wc_commit),
        // The working copy was updated under an operation newer than the
        // head we read — reload at that operation and snapshot against it,
        // like the CLI does.
        WorkingCopyFreshness::Updated(op) => {
            let repo = repo_loader
                .load_at(&op)
                .await
                .context("failed to load jj repo at the working copy's operation")?;
            let id = repo
                .view()
                .get_wc_commit_id(workspace_name)
                .context("jj workspace has no working-copy commit")?
                .clone();
            let commit =
                repo.store().get_commit_async(&id).await.with_context(|| {
                    format!("failed to load jj working-copy commit {}", id.hex())
                })?;
            (repo, id, commit)
        }
        WorkingCopyFreshness::WorkingCopyStale | WorkingCopyFreshness::SiblingOperation => {
            // `snapshot.auto-update-stale`: `jj workspace update-stale`
            // parity, run automatically (the CLI's
            // `recover_stale_working_copy`, single-lock edition):
            //
            // 1. Snapshot the disk against the operation it was actually
            //    checked out at, so local edits land in the op graph first
            //    (as a concurrent op branch) instead of being clobbered.
            // 2. Reload at head — jj merges the op branches.
            // 3. Check the merged view's working-copy commit out onto disk
            //    and finish the lock at the merged operation.
            let old_op = repo_loader
                .load_operation(locked_ws.locked_wc().old_operation_id())
                .await
                .context("failed to load the operation the stale jj working copy was synced at")?;
            let old_repo = repo_loader
                .load_at(&old_op)
                .await
                .context("failed to load jj repo at the stale working copy's operation")?;
            let old_wc_id = old_repo
                .view()
                .get_wc_commit_id(workspace_name)
                .context("stale jj workspace has no working-copy commit at its own operation")?
                .clone();
            let old_wc_commit = old_repo
                .store()
                .get_commit_async(&old_wc_id)
                .await
                .with_context(|| {
                    format!(
                        "failed to load stale jj working-copy commit {}",
                        old_wc_id.hex()
                    )
                })?;
            // CLI-parity guard: the disk must actually hold that commit's
            // tree, else some other process is mid-mutation.
            if old_wc_commit.tree().tree_ids_and_labels()
                != locked_ws.locked_wc().old_tree().tree_ids_and_labels()
            {
                bail!("concurrent jj working-copy operation while recovering a stale workspace");
            }

            let (disk_tree, _stats) = locked_ws
                .locked_wc()
                .snapshot(snapshot_options)
                .await
                .context("failed to snapshot the stale jj working copy")?;
            if disk_tree.tree_ids_and_labels() != old_wc_commit.tree().tree_ids_and_labels() {
                let place = Place {
                    settings: repo_loader.settings(),
                    workspace_name,
                    workspace_root: &workspace_root,
                };
                let mut tx = old_repo.start_transaction();
                tx.set_is_snapshot(true);
                fold_into_working_copy(&place, tx.repo_mut(), &old_wc_commit, disk_tree)
                    .await
                    .context("failed to preserve stale jj working-copy edits")?;
                tx.repo_mut()
                    .rebase_descendants()
                    .await
                    .context("failed to rebase descendants after jj snapshot")?;
                tx.commit("snapshot working copy")
                    .await
                    .context("failed to commit jj snapshot transaction")?;
            }

            let merged_repo = repo_loader
                .load_at_head()
                .await
                .context("failed to reload jj repo after stale-workspace recovery")?;
            let desired_id = merged_repo
                .view()
                .get_wc_commit_id(workspace_name)
                .context("jj workspace has no working-copy commit")?
                .clone();
            let desired = merged_repo
                .store()
                .get_commit_async(&desired_id)
                .await
                .with_context(|| {
                    format!("failed to load jj working-copy commit {}", desired_id.hex())
                })?;
            locked_ws
                .locked_wc()
                .check_out(&desired)
                .await
                .context("failed to update the stale jj working copy")?;
            locked_ws
                .finish(merged_repo.op_id().clone())
                .await
                .context("failed to finish jj working-copy recovery")?;
            tracing::info!(
                operation = %merged_repo.op_id().hex(),
                "working copy was stale (changed by another process); updated it to the latest operation"
            );

            return Ok(LockOutcome::Recovered {
                repo: merged_repo,
                wc_commit: desired,
            });
        }
    };

    let (tree, stats) = locked_ws
        .locked_wc()
        .snapshot(snapshot_options)
        .await
        .context("failed to snapshot jj working copy")?;

    Ok(LockOutcome::Locked(LockedWorkingCopy {
        locked_ws: Some(locked_ws),
        _git_lock: git_lock,
        repo: base_repo,
        wc_commit,
        tree,
        warnings: untracked_warnings(&stats),
    }))
}

/// The working copy as the repo at head has it, with the lock held, when the
/// checkout is fresh. Anything else is left to [`lock_working_copy`]'s own
/// stale handling.
async fn fresh_working_copy(
    locked_ws: &mut LockedWorkspace<'_>,
    repo_loader: &RepoLoader,
    workspace_name: &WorkspaceName,
) -> Result<Option<(Arc<ReadonlyRepo>, Commit)>> {
    let repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let Some(wc_id) = repo.view().get_wc_commit_id(workspace_name).cloned() else {
        return Ok(None);
    };
    let wc_commit = repo
        .store()
        .get_commit_async(&wc_id)
        .await
        .with_context(|| format!("failed to load jj working-copy commit {}", wc_id.hex()))?;
    let fresh = matches!(
        WorkingCopyFreshness::check_stale(locked_ws.locked_wc(), &wc_commit, &repo).await,
        Ok(WorkingCopyFreshness::Fresh)
    );
    Ok(fresh.then_some((repo, wc_commit)))
}

/// jj-cli's `import_git_head`: when git's `HEAD` moved, start `@` on top of
/// the new `HEAD` (abandoning the old `@` if it was discardable) and record
/// that the files on disk already match it — git put them there.
async fn import_git_head(
    workspace: &mut Workspace,
    repo_loader: &RepoLoader,
    place: &Place<'_>,
) -> Result<()> {
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .await
        .context("failed to lock jj working copy")?;
    let Some((repo, _)) =
        fresh_working_copy(&mut locked_ws, repo_loader, place.workspace_name).await?
    else {
        return Ok(());
    };
    let mut tx = repo.start_transaction();
    import_head(tx.repo_mut(), place.workspace_name, place.workspace_root)
        .await
        .context("failed to import the colocated git HEAD")?;
    if !tx.repo().has_changes() {
        return Ok(());
    }
    let new_head = tx.repo().view().git_head(place.workspace_name).clone();
    let wc_commit = match new_head.as_normal() {
        Some(head_id) => {
            let head = tx
                .repo()
                .store()
                .get_commit_async(head_id)
                .await
                .context("failed to load the colocated git HEAD commit")?;
            let wc_commit = tx
                .repo_mut()
                .check_out(place.workspace_name.to_owned(), &head)
                .await
                .context("failed to start a working-copy commit on git HEAD")?;
            locked_ws
                .locked_wc()
                .reset(&wc_commit)
                .await
                .context("failed to reset the working copy to git HEAD")?;
            Some(wc_commit)
        }
        // `HEAD` deleted under git: nothing to check out.
        None => None,
    };
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after importing git HEAD")?;
    let new_repo = tx
        .commit("import git head")
        .await
        .context("failed to commit the git HEAD import")?;
    if wc_commit.is_some() {
        locked_ws
            .finish(new_repo.op_id().clone())
            .await
            .context("failed to finish the git HEAD import")?;
    }
    Ok(())
}

/// jj-cli's `import_git_refs`, which it runs right after its snapshot: take in
/// branches and tags git moved, abandon what they no longer reach, and rebase
/// mutable descendants. Only when the import changes something is the disk
/// folded into `@` first (the CLI's snapshot op, here in the same operation)
/// and the resulting `@` checked out.
async fn import_git_refs(
    workspace: &mut Workspace,
    repo_loader: &RepoLoader,
    place: &Place<'_>,
    snapshot_options: &SnapshotOptions<'_>,
) -> Result<()> {
    let git_settings =
        GitSettings::from_settings(place.settings).context("failed to read git settings")?;
    let remote_settings = place
        .settings
        .remote_settings()
        .context("failed to read remotes.* settings")?;
    let import_options =
        jj_cli::git_util::load_git_import_options(&Ui::null(), &git_settings, &remote_settings)
            .map_err(cli_error)?;
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .await
        .context("failed to lock jj working copy")?;
    let Some((repo, wc_commit)) =
        fresh_working_copy(&mut locked_ws, repo_loader, place.workspace_name).await?
    else {
        return Ok(());
    };
    // A dry run first: almost always git moved nothing, and then neither the
    // disk nor the op log is touched.
    let mut probe = repo.start_transaction();
    import_refs(probe.repo_mut(), &import_options)
        .await
        .context("failed to import git refs")?;
    if !probe.repo().has_changes() {
        return Ok(());
    }
    drop(probe);

    let (tree, _stats) = locked_ws
        .locked_wc()
        .snapshot(snapshot_options)
        .await
        .context("failed to snapshot jj working copy")?;
    let mut tx = repo.start_transaction();
    if tree.tree_ids_and_labels() != wc_commit.tree().tree_ids_and_labels() {
        fold_into_working_copy(place, tx.repo_mut(), &wc_commit, tree).await?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .context("failed to rebase descendants after jj snapshot")?;
    }
    import_refs(tx.repo_mut(), &import_options)
        .await
        .context("failed to import git refs")?;
    // jj-cli's `rebase_mutable_descendants`: commits git rewrote can be
    // immutable; their immutable descendants stay put.
    let immutable = place.immutable(tx.base_repo().as_ref())?;
    tx.repo_mut()
        .rebase_descendants_with_options(&immutable, &RebaseOptions::default(), |_, _| {})
        .await
        .context("failed to rebase descendants after importing git refs")?;
    renew_immutable_working_copy(place, tx.repo_mut()).await?;
    super::colocate::sync_git_after_transaction(
        tx.repo_mut(),
        place.workspace_name,
        place.workspace_root,
    )
    .await?;
    let new_repo = tx
        .commit("import git refs")
        .await
        .context("failed to commit the git refs import")?;
    let wc_id = new_repo
        .view()
        .get_wc_commit_id(place.workspace_name)
        .context("jj workspace has no working-copy commit")?;
    let new_wc = new_repo
        .store()
        .get_commit_async(wc_id)
        .await
        .with_context(|| format!("failed to load jj working-copy commit {}", wc_id.hex()))?;
    locked_ws
        .locked_wc()
        .check_out(&new_wc)
        .await
        .context("failed to check out the working copy after importing git refs")?;
    locked_ws
        .finish(new_repo.op_id().clone())
        .await
        .context("failed to finish the git refs import")?;
    Ok(())
}

/// One line per path the snapshot refused to track, worded like jj's own
/// warning. Only the reasons a user can act on are listed; the rest of the
/// stats are bookkeeping.
fn untracked_warnings(stats: &SnapshotStats) -> Vec<String> {
    stats
        .untracked_paths
        .iter()
        .filter_map(|(path, reason)| match reason {
            UntrackedReason::FileTooLarge { size, max_size } => Some(format!(
                "{} is {} and was not snapshotted (snapshot.max-new-file-size is {})",
                path.as_internal_file_string(),
                HumanByteSize(*size),
                HumanByteSize(*max_size),
            )),
            _ => None,
        })
        .collect()
}

/// Where a transaction's working copy lives: what `immutable()` and the
/// working-copy helpers below resolve against.
pub(crate) struct Place<'a> {
    pub(crate) settings: &'a UserSettings,
    pub(crate) workspace_name: &'a WorkspaceName,
    pub(crate) workspace_root: &'a Path,
}

/// jj-cli's warning when a snapshot finds `@` immutable (`snapshot_working_copy`).
pub(crate) const WC_IMMUTABLE_NOTE: &str =
    "The working-copy commit is immutable; a new commit has been created on top of it.";
/// jj-cli's warning when a transaction made `@` immutable (`finish_transaction`).
pub(crate) const WC_BECAME_IMMUTABLE_NOTE: &str =
    "The working-copy commit became immutable; a new commit has been created on top of it.";

impl Place<'_> {
    /// `immutable()` as jj resolves it for a transaction.
    pub(crate) fn immutable(&self, repo: &dyn Repo) -> Result<Arc<ResolvedRevsetExpression>> {
        parse_user_revset(
            self.workspace_root,
            self.settings,
            self.workspace_name,
            "immutable()",
        )?
        .resolve_user_expression(
            repo,
            &SymbolResolver::new(repo, &[] as &[Box<dyn SymbolResolverExtension>]),
        )
        .context("failed to resolve immutable()")
    }

    fn is_immutable(&self, repo: &dyn Repo, id: &CommitId) -> Result<bool> {
        self.immutable(repo)?
            .intersection(&RevsetExpression::commit(id.clone()))
            .evaluate(repo)
            .context("failed to check whether the working-copy commit is immutable")?
            .is_empty()
            .map(|empty| !empty)
            .context("failed to check whether the working-copy commit is immutable")
    }
}

/// A disk tree folded into `@`.
pub(crate) struct FoldedWorkingCopy {
    pub(crate) commit: Commit,
    /// `@` was immutable, so the tree went into a new commit on top of it
    /// instead ([`WC_IMMUTABLE_NOTE`]).
    pub(crate) on_top: bool,
}

/// Fold `tree` into the working-copy commit `wc_commit` and point `@` at the
/// result, as jj-cli's snapshot does: an immutable `@` is never amended, the
/// tree goes into a new commit on top of it. `--ignore-immutable` doesn't
/// change this in the CLI either. The caller rebases descendants.
pub(crate) async fn fold_into_working_copy(
    place: &Place<'_>,
    repo: &mut MutableRepo,
    wc_commit: &Commit,
    tree: MergedTree,
) -> Result<FoldedWorkingCopy> {
    let on_top = place.is_immutable(repo, wc_commit.id())?;
    let commit = if on_top {
        repo.new_commit(vec![wc_commit.id().clone()], tree)
            .write()
            .await
    } else {
        repo.rewrite_commit(wc_commit).set_tree(tree).write().await
    }
    .context("failed to fold the working-copy changes into a commit")?;
    repo.set_wc_commit(place.workspace_name.to_owned(), commit.id().clone())
        .context("failed to update jj working-copy pointer")?;
    Ok(FoldedWorkingCopy { commit, on_top })
}

/// The step every jj-cli transaction ends with (`finish_transaction`): when
/// the transaction made `@` immutable (a bookmark moved onto it, a push, an
/// undo that landed on shared history), start a new commit on top so the next
/// snapshot doesn't amend it. Returns whether it did.
pub(crate) async fn renew_immutable_working_copy(
    place: &Place<'_>,
    repo: &mut MutableRepo,
) -> Result<bool> {
    let Some(wc_id) = repo.view().get_wc_commit_id(place.workspace_name).cloned() else {
        return Ok(false);
    };
    if !place.is_immutable(repo, &wc_id)? {
        return Ok(false);
    }
    let wc_commit = repo
        .store()
        .get_commit_async(&wc_id)
        .await
        .context("failed to load the working-copy commit")?;
    let fresh = repo
        .new_commit(vec![wc_id], wc_commit.tree())
        .write()
        .await
        .context("failed to write a new working-copy commit")?;
    repo.set_wc_commit(place.workspace_name.to_owned(), fresh.id().clone())
        .context("failed to move the working copy")?;
    Ok(true)
}

/// Resolve a [`RevisionSelection`] against the repo at head.
///
/// A hex naming a commit that was rewritten out from under the row (or a menu
/// the user left open) resolves to its visible successor; one that is gone for
/// good — abandoned, or split into divergent copies — is rejected rather than
/// loaded: the diff would render, the sidebar would have no row for it, and
/// every follow-up action would address a commit nobody can see. See
/// [`follow_rewritten`].
pub(crate) async fn resolve_revision(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
) -> Result<CommitId> {
    let commit_id = match revision {
        RevisionSelection::WorkingCopy => {
            return repo
                .view()
                .get_wc_commit_id(workspace_name)
                .context("jj workspace has no working-copy commit")
                .cloned();
        }
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    Ok(follow_rewritten(repo, commit, None).await?.id().clone())
}

/// What a command's revision targets resolve against: the commit `@` stands
/// for, and the operation the UI's rows reflected when it named them. Built
/// once per command and handed to every resolution in it.
pub(crate) struct TargetContext {
    pub(crate) wc_id: CommitId,
    seen: Option<SeenOp>,
    /// The change `@` was on when the sender picked its targets.
    seen_wc: Option<String>,
}

/// A command addressed `@`, and `@` is on another change than the one the
/// sender saw: a terminal `jj new` or an agent moved it in between.
#[derive(Debug)]
pub(crate) struct WorkingCopyMoved;

impl std::fmt::Display for WorkingCopyMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The working copy moved. Check the selection and try again.")
    }
}

impl std::error::Error for WorkingCopyMoved {}

/// The operation a command's sender last saw, with the repo at it loaded on
/// first use: only a hidden target ever needs it.
struct SeenOp {
    loader: RepoLoader,
    id: OperationId,
    repo: OnceCell<Option<Arc<ReadonlyRepo>>>,
}

impl TargetContext {
    /// `seen_op` is the hex id of the operation the sender's rows reflect;
    /// `None` (or an id that doesn't parse) follows every hidden commit.
    pub(crate) fn new(loader: &RepoLoader, wc_id: CommitId, seen_op: Option<&str>) -> Self {
        let seen = seen_op
            .and_then(OperationId::try_from_hex)
            .map(|id| SeenOp {
                loader: loader.clone(),
                id,
                repo: OnceCell::new(),
            });
        Self {
            wc_id,
            seen,
            seen_wc: None,
        }
    }

    /// `seen_wc` is the change id (jj's reverse hex) of `@` as the sender saw
    /// it; `None` takes whatever `@` is now.
    pub(crate) fn seeing_working_copy(mut self, seen_wc: Option<&str>) -> Self {
        self.seen_wc = seen_wc.map(str::to_owned);
        self
    }

    /// The commit `@` stands for — refused when it is on another change than
    /// the one the sender saw, rather than acting on whatever `@` is now.
    pub(crate) async fn working_copy(&self, repo: &dyn Repo) -> Result<Commit> {
        let commit = repo
            .store()
            .get_commit_async(&self.wc_id)
            .await
            .with_context(|| format!("failed to load jj commit {}", self.wc_id.hex()))?;
        if let Some(seen) = &self.seen_wc
            && commit.change_id().to_string() != *seen
        {
            return Err(WorkingCopyMoved.into());
        }
        Ok(commit)
    }

    /// Whether `commit` was visible at the seen operation. `None` when there
    /// is no seen operation, or it can't be loaded any more.
    async fn visible_when_seen(&self, commit: &Commit) -> Option<bool> {
        let seen = self.seen.as_ref()?;
        if seen.repo.get().is_none() {
            let repo = match seen.loader.load_operation(&seen.id).await {
                Ok(op) => seen.loader.load_at(&op).await.ok(),
                Err(_) => None,
            };
            if repo.is_none() {
                tracing::debug!(op = %seen.id.hex(), "the seen operation didn't load");
            }
            let _ = seen.repo.set(repo);
        }
        let repo = seen.repo.get()?.as_ref()?;
        let targets = repo.resolve_change_id(commit.change_id()).await.ok()?;
        Some(targets.is_some_and(|targets| targets.has_visible(commit.id())))
    }
}

/// The commit a command that names `commit` means.
///
/// A row names its commit by id, and a rewrite the row hasn't seen yet — the
/// watcher's snapshot rebasing the children of an edited `@`, a CLI command,
/// another workspace — leaves that id pointing at a hidden predecessor.
/// Acting on it rewrites a commit nobody can see and resurrects it next to its
/// successor as a divergent change, which is what the user is never asking for:
/// the row on screen *is* the change. So a hidden commit follows its change id
/// to the one visible commit carrying it, the way `jj` resolves a change id.
/// A change that was abandoned or has gone divergent is refused instead,
/// because there is no single commit the row could mean.
///
/// A commit that was already hidden when the sender saw the repo (a remote
/// bookmark's old position, a typed commit id) is the user's pick, not a stale
/// row, and resolves exactly, as `jj` resolves a commit id.
pub(crate) async fn follow_rewritten(
    repo: &dyn Repo,
    commit: Commit,
    context: Option<&TargetContext>,
) -> Result<Commit> {
    let targets = repo
        .resolve_change_id(commit.change_id())
        .await
        .context("failed to check whether the commit is still visible")?;
    let Some(targets) = targets else {
        bail!("{} is no longer in the repository", commit.id().hex());
    };
    if targets.has_visible(commit.id()) {
        return Ok(commit);
    }
    if let Some(context) = context
        && context.visible_when_seen(&commit).await == Some(false)
    {
        return Ok(commit);
    }
    let visible: Vec<&CommitId> = targets.visible_with_offsets().map(|(_, id)| id).collect();
    match visible.as_slice() {
        [only] => {
            let only = (*only).clone();
            tracing::debug!(
                stale = %commit.id().hex(),
                current = %only.hex(),
                "following a rewritten commit to its visible successor"
            );
            repo.store()
                .get_commit_async(&only)
                .await
                .with_context(|| format!("failed to load jj commit {}", only.hex()))
        }
        [] => bail!("{} is no longer in the repository", commit.id().hex()),
        _ => bail!(
            "{} was rewritten into several divergent commits; pick one in the refreshed graph",
            commit.id().hex()
        ),
    }
}

/// Everything the snapshot needs from config, resolved once per workspace
/// rather than once per call. The auto-track matcher is owned here because
/// [`SnapshotOptions`] borrows it.
pub(crate) struct SnapshotContext {
    base_ignores: Arc<GitIgnoreFile>,
    auto_track: Box<dyn Matcher>,
    max_new_file_size: u64,
    /// `snapshot.auto-update-stale`: recover a stale checkout instead of
    /// refusing it.
    auto_update_stale: bool,
}

impl SnapshotContext {
    pub(crate) fn load(settings: &UserSettings, root: &Path) -> Result<Self> {
        Ok(Self {
            base_ignores: snapshot_base_ignores(root)?,
            auto_track: snapshot_auto_track_matcher(settings, root)?,
            max_new_file_size: snapshot_max_new_file_size(settings)?,
            auto_update_stale: settings
                .get_bool("snapshot.auto-update-stale")
                .context("invalid snapshot.auto-update-stale")?,
        })
    }

    pub(crate) fn options(&self) -> SnapshotOptions<'_> {
        SnapshotOptions {
            base_ignores: self.base_ignores.clone(),
            progress: None,
            start_tracking_matcher: self.auto_track.as_ref(),
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: self.max_new_file_size,
        }
    }
}

/// Open the workspace at `root`. The actor calls this once and keeps the
/// result for the repository's whole life; nothing else should.
pub(crate) fn load_workspace(settings: &UserSettings, root: &Path) -> Result<Workspace> {
    Workspace::load(
        settings,
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")
}

/// What folding the disk into `@` produced.
pub(crate) struct SnapshotOutcome {
    pub(crate) snapshot: RepositorySnapshot,
    /// The repo at the post-snapshot operation, handed back so the caller can
    /// diff and walk against it without reading the commit index again.
    pub(crate) repo: Arc<ReadonlyRepo>,
    pub(crate) warnings: Vec<String>,
    /// The checkout was stale and got updated instead of snapshotted — worth
    /// telling the user, since files changed on disk under them.
    pub(crate) recovered_stale: bool,
    /// The checkout is stale and was left alone: what to tell the user, with
    /// the command that updates it.
    pub(crate) stale: Option<String>,
}

/// Fold the on-disk tree into `@` and finish the lock.
///
/// The lock always closes with [`LockedWorkingCopy::finish`], including when
/// the tree did not move: the snapshot still refreshed the working copy's
/// per-file states (mtimes, sizes), and dropping the lock instead of finishing
/// it threw that away, so the next snapshot re-stat'd the whole tree.
pub(crate) async fn snapshot_working_copy(
    workspace: &mut Workspace,
    context: &SnapshotContext,
) -> Result<SnapshotOutcome> {
    let workspace_name = workspace.workspace_name().to_owned();
    let workspace_root = workspace.workspace_root().to_owned();
    let repo_loader = workspace.repo_loader().clone();

    let outcome = match lock_working_copy(workspace, &repo_loader, &workspace_name, context).await {
        Ok(outcome) => outcome,
        Err(error) => match error.downcast::<StaleWorkingCopy>() {
            // Shown read-only at head, the way `--ignore-working-copy` would:
            // the files on disk stay as they are until the user updates them.
            Ok(stale) => {
                let repo = repo_loader
                    .load_at_head()
                    .await
                    .context("failed to load jj repo")?;
                let wc_commit = match repo.view().get_wc_commit_id(&workspace_name) {
                    Some(id) => {
                        Some(repo.store().get_commit_async(id).await.with_context(|| {
                            format!("failed to load jj working-copy commit {}", id.hex())
                        })?)
                    }
                    None => None,
                };
                let working_copy_empty = match &wc_commit {
                    Some(commit) => commit.is_empty(repo.as_ref()).await.ok(),
                    None => None,
                };
                return Ok(SnapshotOutcome {
                    snapshot: RepositorySnapshot {
                        fingerprint: repo.op_id().hex(),
                        working_copy_empty,
                        parent_fingerprint: Some(repo.op_id().hex()),
                        working_copy_commit_id: wc_commit.map(|commit| commit.id().hex()),
                        rewrote_descendants: false,
                    },
                    repo,
                    warnings: Vec::new(),
                    recovered_stale: false,
                    stale: Some(stale.0),
                });
            }
            Err(error) => return Err(error),
        },
    };
    let mut locked = match outcome {
        LockOutcome::Locked(locked) => locked,
        LockOutcome::Recovered {
            repo, wc_commit, ..
        } => {
            let working_copy_empty = wc_commit.is_empty(repo.as_ref()).await.ok();
            let snapshot = RepositorySnapshot {
                fingerprint: repo.op_id().hex(),
                working_copy_empty,
                // Deliberately equal to `fingerprint`: the graph on screen
                // reflects some pre-recovery op, so the mismatch escalates
                // the refresh to a full reload — external ops (the rebase
                // that made us stale, the recovery itself) always landed.
                parent_fingerprint: Some(repo.op_id().hex()),
                working_copy_commit_id: Some(wc_commit.id().hex()),
                rewrote_descendants: false,
            };
            return Ok(SnapshotOutcome {
                snapshot,
                repo,
                warnings: Vec::new(),
                recovered_stale: true,
                stale: None,
            });
        }
    };

    let result = absorb_snapshot(&mut locked, &workspace_name, &workspace_root).await;
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            locked.abandon();
            Err(error)
        }
    }
}

/// The half of [`snapshot_working_copy`] that can fail with the lock held, so
/// the caller has exactly one place to `abandon` from.
async fn absorb_snapshot(
    locked: &mut LockedWorkingCopy<'_>,
    workspace_name: &WorkspaceName,
    workspace_root: &Path,
) -> Result<SnapshotOutcome> {
    let mut warnings = std::mem::take(&mut locked.warnings);
    if !locked.tree_changed() {
        // No file changes: nothing to write to the op log (the common case on
        // idle ticks, and it keeps `jj op log` clean), but still finish at the
        // op we read so the refreshed file states persist.
        let repo = locked.repo.clone();
        let working_copy_empty = locked.wc_commit.is_empty(repo.as_ref()).await.ok();
        locked.finish(repo.op_id().clone()).await?;
        return Ok(SnapshotOutcome {
            snapshot: RepositorySnapshot {
                fingerprint: repo.op_id().hex(),
                working_copy_empty,
                // No op written: the snapshot *is* its own base.
                parent_fingerprint: Some(repo.op_id().hex()),
                working_copy_commit_id: Some(locked.wc_commit.id().hex()),
                rewrote_descendants: false,
            },
            repo,
            warnings,
            recovered_stale: false,
            stale: None,
        });
    }

    // The op our snapshot tx is parented on — the projection compares this to
    // the op its graph reflects to spot external ops (see
    // `RepositorySnapshot::parent_fingerprint`).
    let base_op_id = locked.repo.op_id().hex();
    let base_repo = locked.repo.clone();
    let place = Place {
        settings: base_repo.settings(),
        workspace_name,
        workspace_root,
    };
    let mut tx = base_repo.start_transaction();
    tx.set_is_snapshot(true);
    let folded = fold_into_working_copy(
        &place,
        tx.repo_mut(),
        &locked.wc_commit,
        locked.tree.clone(),
    )
    .await?;
    let new_commit = folded.commit;
    // `rewrite_commit` records a rewrite that the transaction insists on
    // resolving before commit, even when the wc commit has no descendants.
    // When it has some (the user is editing a commit mid-stack), every one of
    // them gets a new commit id, which the projection has to know about.
    let rebased = tx
        .repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after jj snapshot")?;
    // Best-effort, unlike the CLI: this runs on every save, and an IDE
    // holding git's index lock for a moment must not make the save fail to
    // land. The next mutation resets the index anyway. A new `@` on top
    // moves git's `HEAD` as well, as the CLI's snapshot does then.
    let synced = if folded.on_top {
        warnings.push(WC_IMMUTABLE_NOTE.to_owned());
        super::colocate::sync_git_after_transaction(tx.repo_mut(), workspace_name, workspace_root)
            .await
    } else {
        super::colocate::sync_git_after_snapshot(
            tx.repo_mut(),
            workspace_root,
            &locked.wc_commit.tree(),
            &locked.tree,
        )
        .await
    };
    if let Err(error) = synced {
        tracing::warn!(
            error = format!("{error:#}"),
            "failed to sync the colocated git index after a snapshot"
        );
    }
    let new_repo = tx
        .commit("snapshot working copy")
        .await
        .context("failed to commit jj snapshot transaction")?;
    let new_op_id = new_repo.op_id().clone();
    locked.finish(new_op_id.clone()).await?;

    let working_copy_empty = new_commit.is_empty(new_repo.as_ref()).await.ok();
    Ok(SnapshotOutcome {
        snapshot: RepositorySnapshot {
            fingerprint: new_op_id.hex(),
            working_copy_empty,
            parent_fingerprint: Some(base_op_id),
            working_copy_commit_id: Some(new_commit.id().hex()),
            rewrote_descendants: rebased > 0,
        },
        repo: new_repo,
        warnings,
        recovered_stale: false,
        stale: None,
    })
}

/// Read the current jj operation-head id(s) *without* loading the working copy,
/// taking a lock, or walking commits. `get_op_heads` is a bare readdir of
/// `.jj/repo/op_heads/heads`, so the actor can run it before every read to
/// decide whether its cached repo is still at head.
///
/// Heads are sorted and joined so the string is stable across readdir order and
/// still changes when a divergent head set does. The single-head common case
/// yields exactly the same hex as `RepositorySnapshot::fingerprint`
/// (`op_id().hex()`), so the two compare directly.
pub async fn read_jj_op_head(repository: Repository) -> Result<String> {
    let settings = jj_settings(&repository.root)?;
    read_op_head_with(&settings, &repository.root).await
}

pub(crate) async fn read_op_head_with(settings: &UserSettings, root: &Path) -> Result<String> {
    // Resolve through the `.jj/repo` pointer file so a secondary workspace
    // (whose op store lives in the primary repo) reads the right heads.
    let repo_dir = crate::repository::resolve_jj_repo_dir(root)?;
    let loader =
        RepoLoader::init_from_file_system(settings, &repo_dir, &default_backend_factories())
            .context("failed to init jj repo loader for op-head read")?;
    let mut heads: Vec<String> = loader
        .op_heads_store()
        .get_op_heads()
        .await
        .context("failed to read jj op heads")?
        .iter()
        .map(|id| id.hex())
        .collect();
    heads.sort_unstable();
    Ok(heads.join(","))
}
