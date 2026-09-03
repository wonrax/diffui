use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use jj_lib::{
    git::{
        GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress, GitPushOptions,
        GitPushRefTargets, GitSettings, GitSidebandLineTerminator, GitSubprocessCallback,
        GitSubprocessOptions, expand_fetch_refspecs, get_all_remote_names, push_refs,
    },
    merge::Diff,
    ref_name::{RefName, RefNameBuf, RemoteName, RemoteNameBuf, WorkspaceName},
    repo::{MutableRepo, Repo},
    settings::UserSettings,
    str_util::{StringExpression, StringPattern},
    workspace::Workspace,
};

use crate::FetchTarget;
use jj_lib::object_id::ObjectId;

use super::workspace::{LockOutcome, LockedWorkingCopy, SnapshotContext, lock_working_copy};
use crate::model::LoadProgress;

/// Push a single local bookmark to `remote` inside the caller's transaction.
/// jj-lib's [`push_refs`] spawns `git push` under the hood, so authentication
/// uses the user's existing git credential setup (SSH agent, credential
/// helper). It also updates the local remote-tracking ref, keeping the
/// sidebar's ahead/behind correct after the push.
pub(super) fn push_bookmark(
    settings: &UserSettings,
    repo: &mut MutableRepo,
    name: &str,
    remote: &str,
    progress: &LoadProgress,
) -> Result<(String, Vec<String>)> {
    let ref_name = RefName::new(name);
    let remote_name = RemoteName::new(remote);

    // `after` = where the local bookmark now points; `before` = where we last
    // recorded the remote (its tracking ref). jj uses `before` as the expected
    // on-remote position for its push lease check.
    let after = repo
        .view()
        .get_local_bookmark(ref_name)
        .added_ids()
        .next()
        .cloned();
    if after.is_none() {
        bail!("bookmark {name} has no local target to push");
    }
    let before = repo
        .view()
        .get_remote_bookmark(ref_name.to_remote_symbol(remote_name))
        .target
        .added_ids()
        .next()
        .cloned();
    if before == after {
        return Ok((format!("{name} already up to date on {remote}"), Vec::new()));
    }

    let subprocess_options = GitSubprocessOptions::from_settings(settings)
        .context("failed to read git subprocess options")?;
    let targets = GitPushRefTargets {
        bookmarks: vec![(RefNameBuf::from(name), Diff { before, after })],
    };
    // Collect the remote sideband (GitHub's "create a pull request" hint + URL)
    // for the activity log, and forward git's transfer progress to the bar.
    let mut callback = CollectingCallback::new(progress.clone());
    let stats = push_refs(
        repo,
        subprocess_options,
        remote_name,
        &targets,
        &mut callback,
        &GitPushOptions::default(),
    )
    .with_context(|| format!("failed to push {name} to {remote}"))?;

    if !stats.all_ok() {
        let mut problems = Vec::new();
        for (git_ref, reason) in stats.rejected.iter().chain(stats.remote_rejected.iter()) {
            let reason = reason.as_deref().unwrap_or("rejected");
            problems.push(format!("{}: {reason}", git_ref.as_str()));
        }
        if problems.is_empty() {
            bail!("push of {name} to {remote} did not complete");
        }
        bail!("push rejected — {}", problems.join("; "));
    }

    Ok((format!("Pushed {name} to {remote}"), callback.lines))
}

/// Scale for mapping git's normalized [`GitProgress::overall`] (0..1) onto the
/// integer `(loaded, total)` the activity bar reads. Arbitrary granularity.
pub(super) const GIT_PROGRESS_SCALE: usize = 1000;

/// [`GitSubprocessCallback`] for push/fetch: captures the remote/local sideband
/// lines git emits — shown in the activity's expanded row (a fetch's progress
/// summary, or a push's GitHub "create a pull request" hint + URL) — and mirrors
/// git's transfer progress onto the activity's [`LoadProgress`] so the toolbar
/// shows a determinate bar while the transfer runs.
pub(super) struct CollectingCallback {
    lines: Vec<String>,
    progress: LoadProgress,
}

impl CollectingCallback {
    fn new(progress: LoadProgress) -> Self {
        Self {
            lines: Vec::new(),
            progress,
        }
    }
}

impl GitSubprocessCallback for CollectingCallback {
    fn needs_progress(&self) -> bool {
        true
    }

    fn progress(&mut self, progress: &GitProgress) -> std::io::Result<()> {
        // git reports a running fraction; mirror it onto the activity's integer
        // (loaded, total). Only set the total once there's real progress so the
        // bar stays indeterminate (pulsing) until the transfer starts, rather
        // than flashing a 0/0 determinate bar (see `Activity::determinate`).
        let overall = progress.overall();
        if overall > 0.0 {
            self.progress.set_total(GIT_PROGRESS_SCALE);
            let loaded = (overall * GIT_PROGRESS_SCALE as f32) as usize;
            self.progress.set_loaded(loaded.min(GIT_PROGRESS_SCALE));
        }
        Ok(())
    }

    fn local_sideband(
        &mut self,
        message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        collect_sideband_lines(&mut self.lines, message);
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        collect_sideband_lines(&mut self.lines, message);
        Ok(())
    }
}

/// Split git sideband output into individual lines (git interleaves `\r` for
/// progress redraws and `\n` for real lines), dropping blanks, so each entry is
/// one display line in the activity's expanded output.
pub(super) fn collect_sideband_lines(lines: &mut Vec<String>, message: &[u8]) {
    let text = String::from_utf8_lossy(message);
    for piece in text.split(['\n', '\r']) {
        let piece = piece.trim_end();
        if !piece.is_empty() {
            lines.push(piece.to_owned());
        }
    }
}

/// In-process `git fetch` via jj-lib: fetch the requested remote(s) / branch,
/// import the new remote-tracking refs into the jj repo, and commit the
/// resulting operation. Returns the captured sideband output.
///
/// jj-lib's [`GitFetch`] spawns `git fetch` under the hood, so authentication
/// reuses the user's git credential setup (SSH agent, credential helper) —
/// the same path the context-menu push takes.
pub(crate) async fn fetch_jj(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    target: &FetchTarget,
    progress: &LoadProgress,
) -> Result<Vec<String>> {
    // A fetch imports refs and can abandon now-unreachable commits, so it
    // rewrites the view exactly like a mutation does — and so it goes through
    // the same locked prologue. Before, fetch skipped the snapshot entirely
    // and could race a mutation for the working-copy lock.
    let workspace_name = workspace.workspace_name().to_owned();
    let repo_loader = workspace.repo_loader().clone();
    let options = context.options();
    let mut locked =
        match lock_working_copy(workspace, &repo_loader, &workspace_name, &options).await? {
            LockOutcome::Locked(locked) => locked,
            // A recovered checkout already finished its own lock at the merged
            // operation; the caller refreshes and the user can fetch again.
            LockOutcome::Recovered { .. } => {
                bail!("the working copy was stale and has been recovered; try the fetch again")
            }
        };
    let result = fetch_locked(&mut locked, settings, &workspace_name, target, progress).await;
    match result {
        Ok(lines) => Ok(lines),
        Err(error) => {
            locked.abandon();
            Err(error)
        }
    }
}

/// The fetch itself, with the lock held. Split out so [`fetch_jj`] has exactly
/// one place to abandon from.
async fn fetch_locked(
    locked: &mut LockedWorkingCopy<'_>,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    target: &FetchTarget,
    progress: &LoadProgress,
) -> Result<Vec<String>> {
    let repo = locked.repo.clone();
    let git_settings =
        GitSettings::from_settings(settings).context("failed to read git settings")?;
    let import_options = GitImportOptions {
        auto_local_bookmark: git_settings.auto_local_bookmark,
        abandon_unreachable_commits: git_settings.abandon_unreachable_commits,
        remote_auto_track_bookmarks: HashMap::new(),
    };

    // Resolve which remotes to fetch from.
    let remotes: Vec<RemoteNameBuf> = match target {
        FetchTarget::AllRemotes => {
            get_all_remote_names(repo.store()).context("failed to list git remotes")?
        }
        FetchTarget::RemoteBranch { remote, .. } => vec![RemoteName::new(remote).to_owned()],
    };
    if remotes.is_empty() {
        bail!("no git remotes are configured");
    }

    let mut tx = repo.start_transaction();
    // Fold any uncommitted on-disk changes into `@` first, so the fetch's
    // operation carries them instead of stranding them behind an op that
    // rewrote the view.
    if locked.tree_changed() {
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&locked.wc_commit)
            .set_tree(locked.tree.clone())
            .write()
            .await
            .context("failed to fold working-copy changes before fetch")?;
        tx.repo_mut()
            .set_wc_commit(workspace_name.to_owned(), rewritten.id().clone())
            .context("failed to update working-copy pointer before fetch")?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .context("failed to rebase descendants after working-copy fold")?;
    }
    let lines;
    {
        let mut fetcher = GitFetch::new(
            tx.repo_mut(),
            git_settings.to_subprocess_options(),
            &import_options,
        )
        .context("failed to start git fetch")?;
        let mut callback = CollectingCallback::new(progress.clone());
        for remote in &remotes {
            // All branches for a whole-remote fetch; the single branch for a
            // targeted `name@remote` fetch.
            let bookmark = match target {
                FetchTarget::AllRemotes => StringExpression::all(),
                FetchTarget::RemoteBranch { branch, .. } => {
                    StringExpression::pattern(StringPattern::exact(branch))
                }
            };
            let ref_expr = GitFetchRefExpression {
                bookmark,
                tag: StringExpression::none(),
            };
            let refspecs = expand_fetch_refspecs(remote, ref_expr)
                .context("failed to expand fetch refspecs")?;
            fetcher
                .fetch(remote, refspecs, &mut callback, None, None)
                .with_context(|| format!("failed to fetch from {}", remote.as_str()))?;
        }
        fetcher
            .import_refs()
            .await
            .context("failed to import fetched refs")?;
        lines = callback.lines;
    }
    // import_refs can abandon now-unreachable commits; reconcile descendants
    // before recording the op (a no-op when nothing changed).
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after fetch")?;
    let new_repo = tx
        .commit("diffui: fetch")
        .await
        .context("failed to commit fetch")?;

    // Check out the post-fetch `@` (an import that abandoned commits can move
    // it) and finish the lock at the fetch's operation.
    let new_wc_id = new_repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit after fetch")?
        .clone();
    let new_wc_commit = new_repo
        .store()
        .get_commit_async(&new_wc_id)
        .await
        .with_context(|| format!("failed to load new working-copy commit {}", new_wc_id.hex()))?;
    locked.check_out(&new_wc_commit).await?;
    locked.finish(new_repo.op_id().clone()).await?;

    // No sideband output means the fetch was a no-op (up to date); the caller
    // words the summary.
    Ok(lines)
}
