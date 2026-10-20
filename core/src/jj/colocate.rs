//! Keeping a colocated git checkout in step with jj, the way jj-cli does at
//! the end of every transaction (`finish_transaction`) and snapshot
//! (`export_working_copy_changes_to_git`).
//!
//! Exporting the bookmarks alone left git's `HEAD` and index wherever the last
//! jj CLI command put them, so after a diffui `new`/`edit`/squash `git status`
//! and IDE git views described a commit `@` had long left.
//!
//! Ported from jj-cli 0.45.1. Check each against its source on a jj bump:
//! - `is_colocated`: `git_util::is_colocated_git_workspace`
//!   (`cli/src/git_util.rs`), which takes a `Workspace` the transaction tail
//!   doesn't hold.
//! - `sync_git_after_transaction`: the git tail of
//!   `WorkspaceCommandHelper::finish_transaction` and `try_reset_git_head`
//!   (`cli/src/cli_util.rs`).

use std::path::Path;

use anyhow::{Context, Result};
use jj_cli::{cli_util::export_working_copy_changes_to_git, ui::Ui};
use jj_lib::{
    git::{GitResetHeadError, export_refs, get_git_backend, reset_head},
    git_backend::GitRepoAtWorkdirError,
    merged_tree::MergedTree,
    ref_name::WorkspaceName,
    repo::{MutableRepo, Repo},
    store::Store,
};

use super::settings::cli_error;

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

/// The tail of a mutating transaction, before it commits: in a colocated
/// workspace point this worktree's git `HEAD` at `@`'s first parent and reset
/// the index to match, then export the bookmarks. Elsewhere only the export
/// runs, as it always did here.
///
/// A `HEAD` some other process moved in the meantime is left alone with a
/// warning — the CLI does the same, and the next snapshot imports it.
pub(crate) async fn sync_git_after_transaction(
    mut_repo: &mut MutableRepo,
    workspace_name: &WorkspaceName,
    workspace_root: &Path,
) -> Result<()> {
    if is_colocated(mut_repo.store(), workspace_root)?
        && let Some(wc_id) = mut_repo.view().get_wc_commit_id(workspace_name).cloned()
    {
        let wc_commit = mut_repo
            .store()
            .get_commit_async(&wc_id)
            .await
            .context("failed to load the working-copy commit for git HEAD")?;
        match reset_head(mut_repo, workspace_name, workspace_root, &wc_commit).await {
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

/// A snapshot in a colocated workspace: mark newly tracked files
/// intent-to-add in git's index (so `git diff` shows them) and export the
/// bookmarks the snapshot's descendant rebase may have moved. That's jj-cli's
/// `export_working_copy_changes_to_git`, which the CLI also runs only when
/// colocated.
pub(crate) async fn sync_git_after_snapshot(
    mut_repo: &mut MutableRepo,
    workspace_root: &Path,
    old_tree: &MergedTree,
    new_tree: &MergedTree,
) -> Result<()> {
    if !is_colocated(mut_repo.store(), workspace_root)? {
        return Ok(());
    }
    export_working_copy_changes_to_git(&Ui::null(), mut_repo, workspace_root, old_tree, new_tree)
        .await
        .map_err(cli_error)
        .context("failed to update the colocated git index")
}
