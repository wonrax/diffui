//! Keeping a colocated git checkout in step with jj, the way jj-cli does at
//! the end of every transaction and snapshot (`finish_transaction` and
//! `snapshot_working_copy`), which since jj 0.47 run the same git tail.
//!
//! Exporting the bookmarks alone left git's `HEAD` and index wherever the last
//! jj CLI command put them, so after a diffui `new`/`edit`/squash `git status`
//! and IDE git views described a commit `@` had long left.
//!
//! Ported from jj-cli 0.47.1. Check each against its source on a jj bump:
//! - `is_colocated`: `git_util::is_colocated_git_workspace` (jj-frontend's
//!   `src/git_util.rs` since 0.47), which takes a `Workspace` the transaction
//!   tail doesn't hold.
//! - `sync_git_after_transaction`: the git tail of
//!   `WorkspaceCommandHelper::finish_transaction`, `snapshot_working_copy` and
//!   `try_reset_git_head` (`cli/src/cli_util.rs`).

use std::path::Path;

use anyhow::{Context, Result};
use jj_lib::{
    git::{GitResetHeadError, export_refs, get_git_backend, reset_head},
    git_backend::GitRepoAtWorkdirError,
    ref_name::WorkspaceName,
    repo::{MutableRepo, Repo},
    store::Store,
};

/// Whether `workspace_root` is a git worktree of the repo jj stores into —
/// jj-cli's `is_colocated_git_workspace`. Each git worktree has its own `HEAD`,
/// and this is what picks the right one for a secondary workspace.
pub(crate) fn is_colocated(store: &Store, workspace_root: &Path) -> Result<bool> {
    let Ok(backend) = get_git_backend(store) else {
        return Ok(false);
    };
    match backend.open_git_repo_at_workdir(workspace_root) {
        Ok(_) => Ok(true),
        Err(GitRepoAtWorkdirError::NotFound { .. } | GitRepoAtWorkdirError::Unrelated { .. }) => {
            Ok(false)
        }
        Err(error) => Err(error).context("failed to open the colocated git repository"),
    }
}

/// The tail of a mutating transaction or snapshot, before it commits: in a
/// colocated workspace point this worktree's git `HEAD` at `@`'s first parent,
/// reset the index to match when `@` moved (otherwise only mark new files
/// intent-to-add), and export the bookmarks. Elsewhere nothing runs, as
/// in jj-cli: a backing git repo that isn't this working copy (`jj git init
/// --git-repo=<clone>`) keeps its branches until a push or `jj git export`.
///
/// A `HEAD` some other process moved in the meantime is left alone with a
/// warning — the CLI does the same, and the next prologue imports it.
pub(crate) async fn sync_git_after_transaction(
    mut_repo: &mut MutableRepo,
    workspace_name: &WorkspaceName,
    workspace_root: &Path,
) -> Result<()> {
    if !is_colocated(mut_repo.store(), workspace_root)? {
        return Ok(());
    }
    if let Some(wc_id) = mut_repo.view().get_wc_commit_id(workspace_name).cloned() {
        let wc_commit = mut_repo
            .store()
            .get_commit_async(&wc_id)
            .await
            .context("failed to load the working-copy commit for git HEAD")?;
        // The `@` the transaction started from: with the same change and
        // parents, jj 0.47 leaves git's index (what the user staged) alone and
        // only marks new files intent-to-add.
        let old_wc_commit = match mut_repo.base_repo().view().get_wc_commit_id(workspace_name) {
            Some(id) => Some(
                mut_repo
                    .store()
                    .get_commit_async(id)
                    .await
                    .context("failed to load the previous working-copy commit")?,
            ),
            None => None,
        };
        let diff_wc_commit = (old_wc_commit.as_ref(), &wc_commit);
        match reset_head(mut_repo, workspace_name, workspace_root, diff_wc_commit).await {
            Ok(()) => {}
            Err(error @ GitResetHeadError::UpdateHeadRef(_)) => {
                tracing::warn!(%error, "git HEAD moved concurrently; leaving it for the next import");
            }
            Err(error) => return Err(error).context("failed to update the colocated git HEAD"),
        }
    }
    // Bookmarks that can't be represented as a single git ref (e.g.
    // conflicted) come back in `failed_bookmarks`; that's expected and
    // non-fatal, exactly as jj treats it, so only a hard backend error is
    // propagated.
    export_refs(mut_repo).context("failed to export bookmarks to the git backend")?;
    Ok(())
}
