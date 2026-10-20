//! `jj git fetch`, as jj-cli 0.45.1 runs it: which remotes, which bookmarks
//! and tags from each, the import (auto-tracking, abandoning what the remote
//! dropped), and the transaction tail (rebasing mutable descendants, a fresh
//! `@` when the old one became immutable, the colocated git sync). It runs
//! under the same locked prologue as a mutation, with git's progress and the
//! credential prompt wired in.
//!
//! Ported from jj-cli 0.45.1, where the command is private. Check each against
//! its source on a jj bump:
//! - `fetch_locked`: `cmd_git_fetch` (`cli/src/commands/git/fetch.rs`).
//! - `default_fetch_remotes`: `get_default_fetch_remotes` (same file) and
//!   `get_single_remote` (`cli/src/commands/git/mod.rs`).
//! - `missing_branches` and the ignored-refspec warning:
//!   `warn_if_branches_not_found` and `warn_ignored_refspecs` (same file).
//! - `import_lines` and `failed_import_warning`: `print_imported_changes` and
//!   `print_failed_git_import` (`cli/src/git_util.rs`).
//! - The transaction tail: `WorkspaceCommandTransaction::finish`,
//!   `rebase_mutable_descendants` and `finish_transaction`
//!   (`cli/src/cli_util.rs`).

use anyhow::{Context, Result, bail};
use jj_cli::{
    revset_util::{
        parse_remote_fetch_bookmarks, parse_remote_fetch_tags, parse_union_name_patterns,
    },
    ui::Ui,
};
use jj_lib::{
    commit::Commit,
    config::ConfigGetResultExt as _,
    git::{
        GitFetch, GitFetchRefExpression, GitImportStats, GitSettings, IgnoredRefspecs,
        expand_fetch_refspecs, get_all_remote_names, get_git_backend, load_default_fetch_bookmarks,
    },
    object_id::ObjectId,
    op_store::RemoteRefState,
    ref_name::{RefName, RemoteName, RemoteNameBuf, WorkspaceName},
    repo::Repo,
    revset::{ResolvedRevsetExpression, RevsetExpression, SymbolResolver},
    rewrite::RebaseOptions,
    settings::UserSettings,
    store::Store,
    str_util::StringExpression,
    workspace::Workspace,
};

use super::remote::CollectingCallback;
use super::settings::cli_error;
use super::walk::parse_user_revset;
use super::workspace::{LockOutcome, LockedWorkingCopy, SnapshotContext, lock_working_copy};
use crate::model::LoadProgress;
use crate::repository::{FetchOutcome, FetchTarget};

/// Fetch `target` and import it, committing one operation unless nothing
/// changed. A fetch imports refs and can abandon now-unreachable commits, so
/// it rewrites the view exactly like a mutation does and goes through the
/// same locked prologue.
///
/// jj-lib's [`GitFetch`] spawns `git fetch`, so authentication reuses the
/// user's git credential setup, plus diffui's credential prompt.
pub(crate) async fn fetch_jj(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    target: &FetchTarget,
    progress: &LoadProgress,
) -> Result<FetchOutcome> {
    let workspace_name = workspace.workspace_name().to_owned();
    let workspace_root = workspace.workspace_root().to_owned();
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
    let place = Place {
        settings,
        workspace_name: &workspace_name,
        workspace_root: &workspace_root,
    };
    match fetch_locked(&mut locked, &place, target, progress).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            locked.abandon();
            Err(error)
        }
    }
}

struct Place<'a> {
    settings: &'a UserSettings,
    workspace_name: &'a WorkspaceName,
    workspace_root: &'a std::path::Path,
}

/// `cmd_git_fetch`, with the lock held. Split out so [`fetch_jj`] has exactly
/// one place to abandon from.
async fn fetch_locked(
    locked: &mut LockedWorkingCopy<'_>,
    place: &Place<'_>,
    target: &FetchTarget,
    progress: &LoadProgress,
) -> Result<FetchOutcome> {
    let ui = Ui::null();
    let settings = place.settings;
    let repo = locked.repo.clone();
    let mut outcome = FetchOutcome::default();

    let remote_expr = match target {
        FetchTarget::Default => default_fetch_remotes(settings, repo.store(), &mut outcome)?,
        FetchTarget::AllRemotes => StringExpression::all(),
        FetchTarget::RemoteBranch { remote, .. } => StringExpression::exact(remote.as_str()),
    };
    let remote_matcher = remote_expr.to_matcher();
    let all_remotes = get_all_remote_names(repo.store()).context("failed to list git remotes")?;
    let matching_remotes: Vec<&RemoteName> = all_remotes
        .iter()
        .filter(|remote| remote_matcher.is_match(remote.as_str()))
        .map(AsRef::as_ref)
        .collect();
    let unmatched: Vec<String> = remote_expr
        .exact_strings()
        .map(RemoteName::new)
        .filter(|&name| all_remotes.iter().all(|remote| remote != name))
        .map(|name| name.as_symbol().to_string())
        .collect();
    if !unmatched.is_empty() {
        outcome.warnings.push(format!(
            "No matching remotes for names: {}",
            unmatched.join(", ")
        ));
    }
    if matching_remotes.is_empty() {
        match outcome.warnings.first() {
            Some(warning) => bail!("No git remotes to fetch from ({warning})"),
            None => bail!("No git remotes to fetch from"),
        }
    }
    outcome.remotes = matching_remotes
        .iter()
        .map(|remote| remote.as_str().to_owned())
        .collect();

    let mut tx = repo.start_transaction();
    tx.set_attribute(
        "args".to_owned(),
        crate::plan::args_tag(&crate::plan::fetch_command(target)),
    );
    // Fold any uncommitted on-disk changes into `@` first, so the fetch's
    // operation carries them instead of stranding them behind an op that
    // rewrote the view. (jj snapshots into an operation of its own first.)
    if locked.tree_changed() {
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&locked.wc_commit)
            .set_tree(locked.tree.clone())
            .write()
            .await
            .context("failed to fold working-copy changes before fetch")?;
        tx.repo_mut()
            .set_wc_commit(place.workspace_name.to_owned(), rewritten.id().clone())
            .context("failed to update working-copy pointer before fetch")?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .context("failed to rebase descendants after working-copy fold")?;
    }

    let remote_settings = settings
        .remote_settings()
        .context("failed to read remotes.* settings")?;
    // A named branch fetches that branch and no tags, like `--branch`.
    let (common_bookmark, common_tag) = match target {
        FetchTarget::RemoteBranch { branch, .. } => (
            Some(StringExpression::exact(branch.as_str())),
            Some(StringExpression::none()),
        ),
        FetchTarget::Default | FetchTarget::AllRemotes => (None, None),
    };
    let mut expansions = Vec::with_capacity(matching_remotes.len());
    {
        let git_repo = get_git_backend(tx.repo().store())
            .context("fetching needs a git-backed repo")?
            .git_repo();
        for &remote in &matching_remotes {
            let bookmark = if let Some(expr) = &common_bookmark {
                expr.clone()
            } else if let Some(expr) =
                parse_remote_fetch_bookmarks(&ui, &remote_settings, remote).map_err(cli_error)?
            {
                expr
            } else {
                let (ignored, expr) = load_default_fetch_bookmarks(remote, &git_repo)
                    .with_context(|| {
                        format!(
                            "failed to read the fetch refspecs of {}",
                            remote.as_symbol()
                        )
                    })?;
                warn_ignored_refspecs(&mut outcome, remote, ignored);
                expr
            };
            let tag = if let Some(expr) = &common_tag {
                expr.clone()
            } else if let Some(expr) =
                parse_remote_fetch_tags(&ui, &remote_settings, remote).map_err(cli_error)?
            {
                expr
            } else {
                StringExpression::all()
            };
            let expanded = expand_fetch_refspecs(remote, GitFetchRefExpression { bookmark, tag })
                .with_context(|| {
                format!("invalid fetch patterns for {}", remote.as_symbol())
            })?;
            expansions.push((remote, expanded));
        }
    }

    let git_settings =
        GitSettings::from_settings(settings).context("failed to read git settings")?;
    let import_options =
        jj_cli::git_util::load_git_import_options(&ui, &git_settings, &remote_settings)
            .map_err(cli_error)?;
    let stats;
    let git_output;
    {
        let askpass = crate::askpass::Scope::new();
        let mut subprocess_options = git_settings.to_subprocess_options();
        subprocess_options.environment.extend(askpass.environment());
        let mut fetcher = GitFetch::new(tx.repo_mut(), subprocess_options, &import_options)
            .context("failed to start git fetch")?;
        let mut callback = CollectingCallback::new(progress.clone());
        for (remote, expanded) in expansions {
            fetcher
                .fetch(remote, expanded, &mut callback, None)
                .map_err(|error| askpass.explain(error.into()))
                .with_context(|| format!("failed to fetch from {}", remote.as_symbol()))?;
        }
        stats = fetcher
            .import_refs()
            .await
            .context("failed to import fetched refs")?;
        git_output = callback.lines;
    }
    outcome.output.extend(import_lines(tx.repo(), &stats));
    outcome.warnings.extend(failed_import_warning(&stats));
    if let Some(bookmark) = &common_bookmark {
        let missing = missing_branches(&tx, bookmark, &matching_remotes);
        if !missing.is_empty() {
            outcome.warnings.push(format!(
                "No matching branches found on any specified/configured remote: {}",
                missing.join(", ")
            ));
        }
    }

    if !tx.repo().has_changes() {
        outcome.output.push("Nothing changed.".to_owned());
        outcome.output.extend(git_output);
        locked.finish(repo.op_id().clone()).await?;
        return Ok(outcome);
    }
    outcome.changed = true;

    // Fetching can move immutable commits to match the remote; their
    // immutable descendants stay put.
    let immutable = immutable_set(place, tx.base_repo().as_ref())?;
    let mut rebased = 0usize;
    tx.repo_mut()
        .rebase_descendants_with_options(&immutable, &RebaseOptions::default(), |_, _| rebased += 1)
        .await
        .context("failed to rebase descendants after fetch")?;
    if rebased > 0 {
        outcome
            .output
            .push(format!("Rebased {rebased} descendant commits."));
    }

    // A working-copy commit the fetch made immutable gets a fresh one on top.
    if let Some(wc_id) = tx
        .repo()
        .view()
        .get_wc_commit_id(place.workspace_name)
        .cloned()
    {
        let immutable_now = immutable_set(place, tx.repo())?;
        let is_immutable = immutable_now
            .intersection(&RevsetExpression::commit(wc_id.clone()))
            .evaluate(tx.repo())
            .context("failed to check the working-copy commit")?
            .is_empty()
            .map(|empty| !empty)
            .context("failed to check the working-copy commit")?;
        if is_immutable {
            let wc_commit = tx
                .repo()
                .store()
                .get_commit_async(&wc_id)
                .await
                .context("failed to load the working-copy commit")?;
            let fresh = tx
                .repo_mut()
                .new_commit(vec![wc_id], wc_commit.tree())
                .write()
                .await
                .context("failed to write a new working-copy commit")?;
            tx.repo_mut()
                .set_wc_commit(place.workspace_name.to_owned(), fresh.id().clone())
                .context("failed to move the working copy")?;
            outcome.warnings.push(
                "The working-copy commit became immutable; a new commit has been created on top \
                 of it."
                    .to_owned(),
            );
        }
    }

    // Only a colocated workspace exports to git here, as in jj-cli; elsewhere
    // the fetched refs stay jj's until a push or `jj git export`.
    if super::colocate::is_colocated(tx.repo().store(), place.workspace_root)? {
        super::colocate::sync_git_after_transaction(
            tx.repo_mut(),
            place.workspace_name,
            place.workspace_root,
        )
        .await?;
    }
    let remotes = matching_remotes
        .iter()
        .map(|remote| remote.as_symbol().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let new_repo = tx
        .commit(format!("diffui: fetch from git remote(s) {remotes}"))
        .await
        .context("failed to commit fetch")?;

    // Check out the post-fetch `@` (an import that abandoned commits can move
    // it) and finish the lock at the fetch's operation.
    let new_wc_id = new_repo
        .view()
        .get_wc_commit_id(place.workspace_name)
        .context("jj workspace has no working-copy commit after fetch")?
        .clone();
    let new_wc_commit: Commit = new_repo
        .store()
        .get_commit_async(&new_wc_id)
        .await
        .with_context(|| format!("failed to load new working-copy commit {}", new_wc_id.hex()))?;
    locked.check_out(&new_wc_commit).await?;
    locked.finish(new_repo.op_id().clone()).await?;

    outcome.output.extend(git_output);
    Ok(outcome)
}

/// `get_default_fetch_remotes`: `git.fetch` (a name, a pattern or a list of
/// them), else the only remote, else `origin`.
fn default_fetch_remotes(
    settings: &UserSettings,
    store: &Store,
    outcome: &mut FetchOutcome,
) -> Result<StringExpression> {
    const KEY: &str = "git.fetch";
    let ui = Ui::null();
    if let Ok(remotes) = settings.get::<Vec<String>>(KEY) {
        return parse_union_name_patterns(&ui, &remotes).map_err(cli_error);
    }
    if let Some(remote) = settings
        .get_string(KEY)
        .optional()
        .context("invalid git.fetch")?
    {
        return parse_union_name_patterns(&ui, [&remote]).map_err(cli_error);
    }
    const DEFAULT_REMOTE: &str = "origin";
    let mut names = get_all_remote_names(store).context("failed to list git remotes")?;
    let only: Option<RemoteNameBuf> = (names.len() == 1).then(|| names.remove(0));
    Ok(match only {
        Some(remote) => {
            if remote.as_str() != DEFAULT_REMOTE {
                outcome.output.push(format!(
                    "Fetching from the only existing remote: {}",
                    remote.as_symbol()
                ));
            }
            StringExpression::exact(remote.as_str())
        }
        None => StringExpression::exact(DEFAULT_REMOTE),
    })
}

/// `immutable()` as jj resolves it for the transaction tail.
fn immutable_set(
    place: &Place<'_>,
    repo: &dyn Repo,
) -> Result<std::sync::Arc<ResolvedRevsetExpression>> {
    parse_user_revset(
        place.workspace_root,
        place.settings,
        place.workspace_name,
        "immutable()",
    )?
    .resolve_user_expression(
        repo,
        &SymbolResolver::new(
            repo,
            &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
        ),
    )
    .context("failed to resolve immutable()")
}

fn warn_ignored_refspecs(
    outcome: &mut FetchOutcome,
    remote: &RemoteName,
    IgnoredRefspecs(ignored): IgnoredRefspecs,
) {
    for refspec in ignored {
        outcome.warnings.push(format!(
            "Ignored refspec `{}` from `{}`: {}",
            refspec.refspec,
            remote.as_symbol(),
            refspec.reason
        ));
    }
}

/// `warn_if_branches_not_found`: exact names the fetch asked for that no
/// remote had, before or after.
fn missing_branches(
    tx: &jj_lib::transaction::Transaction,
    bookmark: &StringExpression,
    remotes: &[&RemoteName],
) -> Vec<String> {
    let matcher = bookmark.to_matcher();
    bookmark
        .exact_strings()
        .filter(|name| matcher.is_match(name))
        .map(RefName::new)
        .filter(|name| {
            remotes.iter().all(|&remote| {
                let symbol = name.to_remote_symbol(remote);
                tx.repo().view().get_remote_bookmark(symbol).is_absent()
                    && tx
                        .base_repo()
                        .view()
                        .get_remote_bookmark(symbol)
                        .is_absent()
            })
        })
        .map(|name| name.as_symbol().to_string())
        .collect()
}

/// `print_imported_changes`: one line per remote bookmark or tag the import
/// touched (`bookmark: main@origin [updated] tracked`), then the abandoned and
/// rewritten counts.
fn import_lines(repo: &dyn Repo, stats: &GitImportStats) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, changes, is_tag) in [
        ("bookmark", &stats.changed_remote_bookmarks, false),
        ("tag", &stats.changed_remote_tags, true),
    ] {
        let width = changes
            .iter()
            .map(|update| update.symbol.to_string().chars().count())
            .max()
            .unwrap_or(0);
        for update in changes {
            let symbol = update.symbol.to_string();
            let remote_ref = if is_tag {
                repo.view().get_remote_tag(update.symbol.as_ref())
            } else {
                repo.view().get_remote_bookmark(update.symbol.as_ref())
            };
            let status = match (
                update.old_remote_ref.target.is_absent(),
                update.new_target.is_absent(),
            ) {
                (true, false) => "new",
                (false, true) => "deleted",
                _ => "updated",
            };
            let tracking = match remote_ref.state {
                RemoteRefState::New => "untracked",
                RemoteRefState::Tracked => "tracked",
            };
            lines.push(format!("{label}: {symbol:width$} [{status}] {tracking}"));
        }
    }
    if !stats.abandoned_commits.is_empty() {
        lines.push(format!(
            "Abandoned {} commits that are no longer reachable.",
            stats.abandoned_commits.len()
        ));
    }
    if !stats.rewritten_commit_ids.is_empty() {
        lines.push(format!(
            "Updated {} rewritten commits.",
            stats.rewritten_commit_ids.len()
        ));
    }
    lines
}

/// `print_failed_git_import`, as one warning.
fn failed_import_warning(stats: &GitImportStats) -> Option<String> {
    if stats.failed_ref_names.is_empty() {
        return None;
    }
    let names: Vec<String> = stats
        .failed_ref_names
        .iter()
        .map(|name| name.to_string())
        .collect();
    Some(format!(
        "Failed to import some Git refs: {}",
        names.join(", ")
    ))
}
