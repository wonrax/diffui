use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    gitignore::GitIgnoreFile,
    matchers::{Matcher, NothingMatcher},
    merged_tree::MergedTree,
    object_id::ObjectId,
    op_store::OperationId,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo, RepoLoader, StoreFactories},
    settings::{HumanByteSize, UserSettings},
    working_copy::{SnapshotOptions, SnapshotStats, UntrackedReason, WorkingCopyFreshness},
    workspace::{LockedWorkspace, Workspace, default_working_copy_factories},
};

use super::settings::*;
use crate::model::RevisionSelection;
use crate::repository::{Repository, RepositorySnapshot};

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
/// with, and snapshot the on-disk tree. A stale checkout is recovered rather
/// than snapshotted.
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
    snapshot_options: &SnapshotOptions<'_>,
) -> Result<LockOutcome<'a>> {
    // Take the working-copy lock *before* reading the repo head. Otherwise a
    // jj-cli command running between `load_at_head` and the lock can rewrite
    // the wc commit out from under us, and our snapshot tx — still parented on
    // the stale op — lands as a sibling of the cli's op. Both ops touch the
    // same change_id with different commit_ids, which jj's concurrent-op
    // resolver presents as a divergent change.
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .context("failed to lock jj working copy")?;

    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = base_repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
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
    let freshness =
        WorkingCopyFreshness::check_stale(locked_ws.locked_wc(), &wc_commit, &base_repo)
            .await
            .context("failed to check jj working-copy freshness")?;
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
            // `jj workspace update-stale` parity, run automatically (the
            // CLI's `recover_stale_working_copy`, single-lock edition):
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
                let mut tx = old_repo.start_transaction();
                tx.set_is_snapshot(true);
                let new_commit = tx
                    .repo_mut()
                    .rewrite_commit(&old_wc_commit)
                    .set_tree(disk_tree)
                    .write()
                    .await
                    .context("failed to preserve stale jj working-copy edits")?;
                tx.repo_mut()
                    .set_wc_commit(workspace_name.to_owned(), new_commit.id().clone())
                    .context("failed to update jj working-copy pointer")?;
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
        repo: base_repo,
        wc_commit,
        tree,
        warnings: untracked_warnings(&stats),
    }))
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

/// Resolve a [`RevisionSelection`] against the repo at head.
///
/// A hex naming a commit that is no longer visible — abandoned, or rewritten
/// out from under a menu the user left open — is rejected rather than loaded:
/// the diff would render, the sidebar would have no row for it, and every
/// follow-up action would address a commit nobody can see.
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
    if commit
        .is_hidden(repo)
        .context("failed to check whether the commit is still visible")?
    {
        bail!("{} is no longer in the repository", commit_id.hex());
    }
    Ok(commit_id)
}

/// Everything the snapshot needs from config, resolved once per workspace
/// rather than once per call. The auto-track matcher is owned here because
/// [`SnapshotOptions`] borrows it.
pub(crate) struct SnapshotContext {
    base_ignores: Arc<GitIgnoreFile>,
    auto_track: Box<dyn Matcher>,
    max_new_file_size: u64,
}

impl SnapshotContext {
    pub(crate) fn load(settings: &UserSettings, root: &Path) -> Result<Self> {
        Ok(Self {
            base_ignores: snapshot_base_ignores(root)?,
            auto_track: snapshot_auto_track_matcher(settings, root)?,
            max_new_file_size: snapshot_max_new_file_size(settings)?,
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
        &StoreFactories::default(),
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
    let repo_loader = workspace.repo_loader().clone();
    let options = context.options();

    let mut locked =
        match lock_working_copy(workspace, &repo_loader, &workspace_name, &options).await? {
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
                };
                return Ok(SnapshotOutcome {
                    snapshot,
                    repo,
                    warnings: Vec::new(),
                });
            }
        };

    let result = absorb_snapshot(&mut locked, &workspace_name).await;
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
) -> Result<SnapshotOutcome> {
    let warnings = std::mem::take(&mut locked.warnings);
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
            },
            repo,
            warnings,
        });
    }

    // The op our snapshot tx is parented on — the projection compares this to
    // the op its graph reflects to spot external ops (see
    // `RepositorySnapshot::parent_fingerprint`).
    let base_op_id = locked.repo.op_id().hex();
    let mut tx = locked.repo.start_transaction();
    tx.set_is_snapshot(true);
    let new_commit = tx
        .repo_mut()
        .rewrite_commit(&locked.wc_commit)
        .set_tree(locked.tree.clone())
        .write()
        .await
        .context("failed to rewrite jj working-copy commit with new tree")?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.to_owned(), new_commit.id().clone())
        .context("failed to update jj working-copy pointer")?;
    // `rewrite_commit` records a rewrite that the transaction insists on
    // resolving before commit, even when the wc commit has no descendants.
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after jj snapshot")?;
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
        },
        repo: new_repo,
        warnings,
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
    let loader = RepoLoader::init_from_file_system(settings, &repo_dir, &StoreFactories::default())
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
