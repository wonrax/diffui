use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt};
use jj_cli::description_util::try_combine_messages;
use jj_lib::{
    absorb::{AbsorbSource, absorb_hunks, split_hunks_to_trees},
    backend::{ChangeId, CommitId},
    commit::Commit,
    git::export_refs,
    matchers::EverythingMatcher,
    object_id::ObjectId,
    op_store::{OperationId, RefTarget},
    operation::Operation,
    ref_name::{RefName, RefNameBuf, RemoteName, WorkspaceName},
    repo::{MutableRepo, ReadonlyRepo, Repo, RepoLoader},
    revset::{RevsetExpression, SymbolResolver},
    rewrite::{
        CommitWithSelection, MoveCommitsLocation, MoveCommitsStats, MoveCommitsTarget,
        RebaseOptions, RebasedCommit, duplicate_commits_onto_parents, merge_commit_trees,
        move_commits, squash_commits,
    },
    settings::UserSettings,
    transaction::Transaction,
    workspace::Workspace,
};

use super::push::{
    PlannedPush, PushContext, Signed, create_bookmark, forget_bookmark, plan_push, rename_bookmark,
    send_push, sign_commits, sign_plan, untrack_bookmark,
};
use super::walk::*;
use super::workspace::{
    LockOutcome, LockedWorkingCopy, SnapshotContext, TargetContext, follow_rewritten,
    lock_working_copy,
};
use crate::model::{LoadProgress, RevisionSelection};
use crate::mutations::{Destination, MutationOp, MutationOutcome, RebaseSourceMode, SquashTarget};
use crate::push::{PushAllow, PushRequest, PushTarget};
use crate::repository::Repository;

/// Whether moving local bookmark `name` to `to` is a **backwards or sideways**
/// move — the bookmark exists and its current target is not an ancestor of the
/// new one. The jj CLI refuses such moves without `--allow-backwards`, so the
/// UI asks for confirmation first instead of silently diverging from it.
/// A missing bookmark (or one with no local target) is a creation and never
/// backwards; a conflicted bookmark reports `true` (the conservative answer —
/// jj can't fast-forward those either). Read-only: no snapshot, no wc lock.
pub(crate) async fn check_bookmark_move_backwards(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    name: &str,
    to: &RevisionSelection,
    seen_op: Option<&str>,
) -> Result<bool> {
    let current = repo.view().get_local_bookmark(RefName::new(name));
    if current.is_absent() {
        return Ok(false);
    }
    let Some(current) = current.as_normal() else {
        return Ok(true);
    };

    let wc_id = repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    let context = TargetContext::new(repo.loader(), wc_id, seen_op);
    let new_target = resolve_mutation_target(repo, &context, to).await?;

    let forward = repo
        .index()
        .is_ancestor(current, new_target.id())
        .await
        .context("failed to check bookmark ancestry")?;
    Ok(!forward)
}

/// Apply a revision mutation and reconcile the working copy on disk.
///
/// Ops that only move a *view* — setting, deleting or tracking a bookmark, and
/// pushing one — never touch the disk, so they never take the working-copy
/// lock. A push in particular must not: it talks to a network, and holding
/// jj's working-copy lock across that stalls every snapshot for as long as the
/// remote takes to answer. (Signing commits for `git.sign-on-push` does take
/// it, briefly and before the network: see [`apply_view_only`].)
///
/// Everything else runs one locked working-copy session, mirroring what `jj`
/// itself runs: snapshot the current on-disk state into `@` (so uncommitted
/// work survives `@` moving), apply the mutation in a transaction, then check
/// out the resulting `@` so the files on disk match it. Skipping the checkout
/// would leave the old files in place, and the next snapshot would record them
/// into the moved-to commit.
///
/// The flag beside the outcome says the checkout was stale and had to be
/// recovered first, which the caller tells the user about.
///
/// `seen_op` is the operation the sender's rows reflect, which tells a stale
/// row from a hidden commit picked on purpose (see [`follow_rewritten`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_mutation(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    repository: &Repository,
    op: &MutationOp,
    progress: &LoadProgress,
    allow_immutable: bool,
    seen_op: Option<&str>,
) -> Result<(MutationOutcome, bool)> {
    if is_view_only(op) {
        return Ok((
            apply_view_only(
                workspace,
                settings,
                context,
                &repository.root,
                op,
                progress,
                seen_op,
            )
            .await?,
            false,
        ));
    }
    // Recovering a stale checkout finishes the lock session the mutation needs,
    // so the attempt reports back without mutating and the retry runs against
    // the recovered `@`. A second stale report means something outside is still
    // rewriting this workspace's `@`; refuse rather than keep taking the lock.
    for attempt in 0..2 {
        if let Some(outcome) = apply_mutation_attempt(
            workspace,
            settings,
            context,
            repository,
            op,
            progress,
            allow_immutable,
            seen_op,
        )
        .await?
        {
            return Ok((outcome, attempt > 0));
        }
    }
    bail!("jj working copy kept going stale while applying the mutation")
}

/// Whether `op` only rewrites the view — no commit is rewritten and no file on
/// disk changes, so no working-copy lock is warranted.
fn is_view_only(op: &MutationOp) -> bool {
    matches!(
        op,
        MutationOp::MoveBookmark { .. }
            | MutationOp::DeleteBookmark { .. }
            | MutationOp::TrackBookmark { .. }
            | MutationOp::PushBookmark { .. }
            | MutationOp::CreateBookmark { .. }
            | MutationOp::RenameBookmark { .. }
            | MutationOp::ForgetBookmark { .. }
            | MutationOp::UntrackBookmark { .. }
            | MutationOp::Push(_)
    )
}

/// The lock-free path for the bookmark ops. A move that also pushes keeps both
/// halves in one transaction, so a failed push still rolls the move back and
/// the action never half-lands — the whole thing simply runs outside the lock.
///
/// The exception is `git.sign-on-push` with commits still to sign: those are
/// signed first, in an operation of their own under the working-copy lock
/// ([`sign_before_push`]), and the push is then planned again over the signed
/// commits. A push that fails after that keeps the signatures, as in the CLI.
async fn apply_view_only(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    repo_root: &Path,
    op: &MutationOp,
    progress: &LoadProgress,
    seen_op: Option<&str>,
) -> Result<MutationOutcome> {
    let workspace_name = workspace.workspace_name().to_owned();
    let mut signed: Option<Signed> = None;
    loop {
        let base_repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .context("failed to load jj repo")?;
        let wc_commit_id = base_repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .context("jj workspace has no working-copy commit")?
            .clone();

        let mut tx = base_repo.start_transaction();
        tx.set_attribute(
            "args".to_owned(),
            super::op_args::op_args(base_repo.as_ref(), op, false).await,
        );
        let targets = TargetContext::new(base_repo.loader(), wc_commit_id, seen_op);
        let push_context = PushContext {
            settings,
            repo_root,
            workspace_name: &workspace_name,
            targets: &targets,
        };
        let (moved, planned) = stage_view_only(&push_context, tx.repo_mut(), op).await?;
        let mut output: Vec<String> = Vec::new();
        let mut partial_failure: Option<String> = None;
        let message = match planned {
            Some(mut planned) => {
                let signing = sign_plan(&push_context, tx.repo(), &planned).await?;
                if !signing.to_sign.is_empty() {
                    if signed.is_some() {
                        bail!(
                            "the commits to push changed while they were being signed; push again"
                        );
                    }
                    drop(tx);
                    signed = Some(
                        sign_before_push(workspace, settings, context, repo_root, op, seen_op)
                            .await?,
                    );
                    continue;
                }
                planned.notes.extend(
                    signed
                        .filter(|signed| signed.signed > 0)
                        .as_ref()
                        .map(Signed::note),
                );
                planned.notes.extend(signing.skipped_note);
                let pushed = send_push(&push_context, tx.repo_mut(), planned, progress)?;
                output = pushed.output;
                partial_failure = pushed.partial_failure;
                match moved {
                    Some(moved) => format!("{moved} \u{b7} {}", pushed.message),
                    None => pushed.message,
                }
            }
            None => moved.context("a bookmark op without a push describes itself")?,
        };

        // Mirror jj's own post-operation behavior: export the updated bookmarks to
        // the colocated git repo so the real git branches follow the move.
        export_refs(tx.repo_mut()).context("failed to export bookmarks to the git backend")?;
        let new_repo = tx
            .commit(format!("diffui: {message}"))
            .await
            .context("failed to commit bookmark transaction")?;
        // Some refs reached the remote and some didn't: what did land is recorded
        // above (as the CLI does), and the refusal is still the outcome.
        if let Some(failure) = partial_failure {
            return Err(super::push::PushFailed(failure).into());
        }

        return Ok(MutationOutcome {
            message,
            moved_working_copy: false,
            rewritten_commit: None,
            output,
            operation_id: Some(new_repo.op_id().hex()),
        });
    }
}

/// Apply a view-only `op`'s bookmark changes inside `repo`. Returns the
/// message for what changed locally, and the planned push when the op pushes
/// (whose own message follows once it has gone out).
async fn stage_view_only(
    push_context: &PushContext<'_>,
    repo: &mut MutableRepo,
    op: &MutationOp,
) -> Result<(Option<String>, Option<PlannedPush>)> {
    let settings = push_context.settings;
    let request = match op {
        MutationOp::MoveBookmark {
            name,
            to,
            push_remote,
        } => {
            let commit = resolve_mutation_target(repo, push_context.targets, to).await?;
            let short = short_change_id(&commit);
            repo.set_local_bookmark_target(
                RefName::new(name),
                RefTarget::normal(commit.id().clone()),
            );
            let moved = format!("Moved bookmark {name} to {short}");
            match push_remote {
                // The push reads the bookmark's target back out of this
                // transaction's view, so it pushes the position set above —
                // through the same checks `jj git push -b` runs.
                Some(remote) => {
                    let request = PushRequest {
                        remote: remote.clone(),
                        // `SetBookmark`, not `Bookmark`: a refused push rolls
                        // the move back, so the rejection's request has to
                        // carry the move for a rerun with allowances.
                        targets: vec![PushTarget::SetBookmark {
                            name: name.clone(),
                            to: to.clone(),
                        }],
                        allow: PushAllow::default(),
                        confirm: false,
                    };
                    let planned = plan_push(push_context, repo, &request).await?;
                    return Ok((Some(moved), Some(planned)));
                }
                None => return Ok((Some(moved), None)),
            }
        }
        MutationOp::PushBookmark { name, remote } => &PushRequest {
            remote: remote.clone(),
            targets: vec![PushTarget::Bookmark(name.clone())],
            allow: PushAllow::default(),
            confirm: false,
        },
        MutationOp::Push(request) => request,
        MutationOp::DeleteBookmark { name } => {
            repo.set_local_bookmark_target(RefName::new(name), RefTarget::absent());
            return Ok((Some(format!("Deleted bookmark {name}")), None));
        }
        MutationOp::TrackBookmark { name, remote } => {
            let symbol = RefName::new(name).to_remote_symbol(RemoteName::new(remote));
            repo.track_remote_bookmark(symbol)
                .await
                .with_context(|| format!("failed to track {name}@{remote}"))?;
            return Ok((Some(format!("Tracking {name}@{remote}")), None));
        }
        MutationOp::CreateBookmark { name, to } => {
            let commit = resolve_mutation_target(repo, push_context.targets, to).await?;
            create_bookmark(settings, repo, name, &commit).await?;
            let message = format!("Created bookmark {name} at {}", short_change_id(&commit));
            return Ok((Some(message), None));
        }
        MutationOp::RenameBookmark { old, new } => {
            rename_bookmark(repo, old, new).await?;
            return Ok((Some(format!("Renamed bookmark {old} to {new}")), None));
        }
        MutationOp::ForgetBookmark { name } => {
            forget_bookmark(repo, name)?;
            return Ok((Some(format!("Forgot bookmark {name}")), None));
        }
        MutationOp::UntrackBookmark { name, remote } => {
            untrack_bookmark(repo, name, remote)?;
            return Ok((Some(format!("Stopped tracking {name}@{remote}")), None));
        }
        // `is_view_only` gates this function; anything else is a caller bug.
        _ => bail!("not a view-only mutation"),
    };
    let planned = plan_push(push_context, repo, request).await?;
    Ok((None, Some(planned)))
}

/// Sign what `op`'s push needs signed, as an operation of its own under the
/// working-copy lock. Signing rewrites the pushed commits and rebases their
/// descendants, `@` among them, so this is a rewrite like any other: the disk
/// is folded into `@` first and the new `@` checked out after. The push is
/// planned again inside the transaction to pick the commits (the repo may have
/// moved since the lock-free plan); its bookmark changes are then rolled back,
/// since they belong to the push, which runs afterwards without the lock.
async fn sign_before_push(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    repo_root: &Path,
    op: &MutationOp,
    seen_op: Option<&str>,
) -> Result<Signed> {
    let workspace_name = workspace.workspace_name().to_owned();
    let repo_loader = workspace.repo_loader().clone();
    let options = context.options();
    // A stale checkout is recovered instead of locked; the retry signs against
    // the recovered `@`, as `apply_mutation` does.
    for _ in 0..2 {
        let mut locked =
            match lock_working_copy(workspace, &repo_loader, &workspace_name, &options).await? {
                LockOutcome::Locked(locked) => locked,
                LockOutcome::Recovered { .. } => continue,
            };
        let result = sign_locked(
            &mut locked,
            settings,
            repo_root,
            &workspace_name,
            op,
            seen_op,
        )
        .await;
        if result.is_err() {
            locked.abandon();
        }
        return result;
    }
    bail!("jj working copy kept going stale while signing the commits to push")
}

/// The body of [`sign_before_push`], with the lock held.
async fn sign_locked(
    locked: &mut LockedWorkingCopy<'_>,
    settings: &UserSettings,
    repo_root: &Path,
    workspace_name: &WorkspaceName,
    op: &MutationOp,
    seen_op: Option<&str>,
) -> Result<Signed> {
    let base_repo = locked.repo.clone();
    let mut tx = base_repo.start_transaction();
    fold_working_copy(&mut tx, locked, workspace_name).await?;
    let wc_commit_id = tx
        .repo()
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    let targets = TargetContext::new(base_repo.loader(), wc_commit_id, seen_op);
    let push_context = PushContext {
        settings,
        repo_root,
        workspace_name,
        targets: &targets,
    };

    let bookmarks_before: HashMap<RefNameBuf, RefTarget> = tx
        .repo()
        .view()
        .local_bookmarks()
        .map(|(name, target)| (name.to_owned(), target.clone()))
        .collect();
    let (_, planned) = stage_view_only(&push_context, tx.repo_mut(), op).await?;
    let to_sign = match &planned {
        Some(planned) => sign_plan(&push_context, tx.repo(), planned).await?.to_sign,
        None => Vec::new(),
    };
    let mut names: HashSet<RefNameBuf> = bookmarks_before.keys().cloned().collect();
    names.extend(
        tx.repo()
            .view()
            .local_bookmarks()
            .map(|(name, _)| name.to_owned()),
    );
    for name in names {
        let before = bookmarks_before
            .get(&name)
            .cloned()
            .unwrap_or_else(RefTarget::absent);
        if *tx.repo().view().get_local_bookmark(&name) != before {
            tx.repo_mut().set_local_bookmark_target(&name, before);
        }
    }

    let signed = sign_commits(tx.repo_mut(), &to_sign).await?;
    if !tx.repo().has_changes() {
        locked.finish(base_repo.op_id().clone()).await?;
        return Ok(signed);
    }
    super::colocate::sync_git_after_transaction(tx.repo_mut(), workspace_name, repo_root).await?;
    let new_repo = tx
        .commit("diffui: sign commits before push")
        .await
        .context("failed to commit the signing transaction")?;
    let new_wc_id = new_repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit after signing")?
        .clone();
    let new_wc_commit = new_repo
        .store()
        .get_commit_async(&new_wc_id)
        .await
        .with_context(|| format!("failed to load new working-copy commit {}", new_wc_id.hex()))?;
    locked.check_out(&new_wc_commit).await?;
    locked.finish(new_repo.op_id().clone()).await?;
    Ok(signed)
}

/// Fold the disk tree the lock's snapshot read into `@`, so a rewrite that
/// moves `@` doesn't lose uncommitted work. Returns whether `@` changed.
async fn fold_working_copy(
    tx: &mut Transaction,
    locked: &LockedWorkingCopy<'_>,
    workspace_name: &WorkspaceName,
) -> Result<bool> {
    if !locked.tree_changed() {
        return Ok(false);
    }
    let rewritten = tx
        .repo_mut()
        .rewrite_commit(&locked.wc_commit)
        .set_tree(locked.tree.clone())
        .write()
        .await
        .context("failed to fold working-copy changes before mutation")?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.to_owned(), rewritten.id().clone())
        .context("failed to update working-copy pointer before mutation")?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after working-copy fold")?;
    Ok(true)
}

/// One pass of [`apply_mutation`]. `Ok(None)` means the working copy was stale
/// and has been recovered instead of mutated — see
/// [`LockOutcome`](super::workspace::LockOutcome).
#[allow(clippy::too_many_arguments)]
async fn apply_mutation_attempt(
    workspace: &mut Workspace,
    settings: &UserSettings,
    context: &SnapshotContext,
    repository: &Repository,
    op: &MutationOp,
    progress: &LoadProgress,
    allow_immutable: bool,
    seen_op: Option<&str>,
) -> Result<Option<MutationOutcome>> {
    let workspace_name = workspace.workspace_name().to_owned();
    let repo_loader = workspace.repo_loader().clone();
    let options = context.options();

    let mut locked =
        match lock_working_copy(workspace, &repo_loader, &workspace_name, &options).await? {
            LockOutcome::Locked(locked) => locked,
            LockOutcome::Recovered { .. } => return Ok(None),
        };

    let result = mutate_locked(
        &mut locked,
        &repo_loader,
        settings,
        repository,
        &workspace_name,
        op,
        progress,
        allow_immutable,
        seen_op,
    )
    .await;
    match result {
        Ok(outcome) => Ok(Some(outcome)),
        Err(error) => {
            // Nothing landed, so there is nothing to persist — say so, rather
            // than letting the lock fall out of scope unfinished, which leaves
            // the checkout pointing at an operation the repo has moved past
            // and jj then reports `@` as divergent.
            locked.abandon();
            Err(error)
        }
    }
}

/// The body of one mutation, with the lock held. Split out so
/// [`apply_mutation_attempt`] has exactly one place to abandon from.
#[allow(clippy::too_many_arguments)]
async fn mutate_locked(
    locked: &mut LockedWorkingCopy<'_>,
    repo_loader: &RepoLoader,
    settings: &UserSettings,
    repository: &Repository,
    workspace_name: &WorkspaceName,
    op: &MutationOp,
    _progress: &LoadProgress,
    allow_immutable: bool,
    seen_op: Option<&str>,
) -> Result<MutationOutcome> {
    let base_repo = locked.repo.clone();
    let workspace_name = workspace_name.to_owned();
    let args = super::op_args::op_args(base_repo.as_ref(), op, allow_immutable).await;

    // An undo, redo or revert has to know its target *before* the pre-op fold
    // is committed: resolving it can fail (a merge operation has no single
    // parent to undo to, there may be nothing to redo), and bailing after the
    // fold left a snapshot op in the log with the lock unfinished — `@`
    // divergent, and the user staring at an error.
    let op_plan = match op {
        MutationOp::Undo {
            operation_id: Some(hex),
        }
        | MutationOp::RevertOperation { operation_id: hex } => {
            Some(super::undo::plan_revert(repo_loader, &base_repo, hex).await?)
        }
        MutationOp::Undo { operation_id: None } => {
            Some(super::undo::plan_undo(repo_loader, base_repo.operation()).await?)
        }
        MutationOp::Redo => Some(super::undo::plan_redo(repo_loader, base_repo.operation()).await?),
        _ => None,
    };
    // Same discipline for a restore: an operation from before this workspace
    // existed has no `@` for it, and finding that out after the fold landed
    // would strand the lock.
    let restore_target = match op {
        MutationOp::RestoreOperation { operation_id } => {
            Some(super::history::restore_target(repo_loader, operation_id, &workspace_name).await?)
        }
        _ => None,
    };

    let mut tx = base_repo.start_transaction();

    // Fold any uncommitted on-disk changes into `@` first so moving off it
    // doesn't lose them. The fold normally rides inside the mutation's own
    // transaction (one op per action; the folded commit stays referenced by
    // the resulting view) — but an *undo* (or redo) replaces the view, so a
    // same-tx fold could end up referenced by no operation at all and be
    // unrecoverable from the op log. For those the fold is committed as its
    // own snapshot op first, mirroring the CLI's snapshot-before-command; the
    // undo then carries the edit over onto the `@` it restores.
    if fold_working_copy(&mut tx, locked, &workspace_name).await?
        && (op_plan.is_some() || restore_target.is_some())
    {
        tx.set_is_snapshot(true);
        let snapped = tx
            .commit("snapshot working copy")
            .await
            .context("failed to commit pre-undo snapshot")?;
        tx = snapped.start_transaction();
    }

    // The post-fold `@`, used to resolve a `WorkingCopy` target after the fold
    // may have rewritten it.
    let current_wc_id = tx
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    // The *change* the working copy is on, not the commit: a describe or a
    // rebase upstream of `@` rewrites `@`'s commit id through the descendant
    // rebase without the user having gone anywhere, and only the change id
    // stays put across that.
    let current_wc_change = tx
        .repo()
        .store()
        .get_commit_async(&current_wc_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", current_wc_id.hex()))?
        .change_id()
        .clone();
    let context = TargetContext::new(repo_loader, current_wc_id.clone(), seen_op);

    // Captured side output: remote sideband for push, skipped/absorbed notes
    // for absorb; empty for the other mutations.
    let mut output: Vec<String> = Vec::new();
    let mut rewritten_commit: Option<String> = None;
    let message = match op {
        MutationOp::New { parent } => {
            let parent_commit = resolve_mutation_target(tx.repo(), &context, parent).await?;
            let short = short_change_id(&parent_commit);
            // Single parent: `merge_commit_trees` returns the parent's tree.
            let tree = merge_commit_trees(tx.repo(), std::slice::from_ref(&parent_commit))
                .await
                .context("failed to build tree for new commit")?;
            let new_commit = tx
                .repo_mut()
                .new_commit(vec![parent_commit.id().clone()], tree)
                .write()
                .await
                .context("failed to write new commit")?;
            tx.repo_mut()
                .edit(workspace_name.clone(), &new_commit)
                .await
                .context("failed to point working copy at new commit")?;
            format!("New change on {short}")
        }
        MutationOp::Edit { target } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            // The CLI refuses `jj edit` on immutable commits: a working copy
            // parked there would amend them on the next snapshot.
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[commit.id().clone()],
                allow_immutable,
            )
            .await?;
            let short = short_change_id(&commit);
            tx.repo_mut()
                .edit(workspace_name.clone(), &commit)
                .await
                .context("failed to set working copy to target commit")?;
            format!("Working copy now at {short}")
        }
        MutationOp::Abandon { targets } => {
            let mut commits: Vec<Commit> = Vec::new();
            let mut seen: HashSet<CommitId> = HashSet::new();
            for selection in targets {
                let commit = resolve_mutation_target(tx.repo(), &context, selection).await?;
                if seen.insert(commit.id().clone()) {
                    commits.push(commit);
                }
            }
            let ids: Vec<CommitId> = commits.iter().map(|c| c.id().clone()).collect();
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &ids,
                allow_immutable,
            )
            .await?;
            match commits.as_slice() {
                [] => bail!("abandon needs at least one revision"),
                [only] => {
                    let short = short_change_id(only);
                    tx.repo_mut().record_abandoned_commit(only);
                    format!("Abandoned {short}")
                }
                many => {
                    for commit in many {
                        tx.repo_mut().record_abandoned_commit(commit);
                    }
                    format!("Abandoned {} revisions", many.len())
                }
            }
        }
        MutationOp::Describe {
            target,
            description,
        } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[commit.id().clone()],
                allow_immutable,
            )
            .await?;
            let short = short_change_id(&commit);
            let rewritten = tx
                .repo_mut()
                .rewrite_commit(&commit)
                .set_description(description.clone())
                .write()
                .await
                .with_context(|| format!("failed to describe revision {short}"))?;
            if matches!(target, RevisionSelection::Commit(_)) {
                rewritten_commit = Some(rewritten.id().hex());
            }
            format!("Updated description for {short}")
        }
        MutationOp::Rebase {
            mode,
            sources,
            destination,
        } => {
            let mut source_commits: Vec<Commit> = Vec::new();
            let mut seen: HashSet<CommitId> = HashSet::new();
            for selection in sources {
                let commit = resolve_mutation_target(tx.repo(), &context, selection).await?;
                if seen.insert(commit.id().clone()) {
                    source_commits.push(commit);
                }
            }
            if source_commits.is_empty() {
                bail!("rebase needs at least one source revision");
            }
            let source_ids: Vec<CommitId> = source_commits.iter().map(|c| c.id().clone()).collect();
            let (new_parent_ids, mut new_child_ids, anchor) =
                resolve_rebase_location(tx.repo(), &context, destination).await?;
            // `-A parent-of-X` lists X itself among the target's children;
            // moved commits can't also be insertion children (the CLI
            // subtracts the target set the same way).
            new_child_ids.retain(|id| !seen.contains(id));
            match resolve_move_target(tx.repo(), *mode, &source_ids, &new_parent_ids).await? {
                // Branch mode with the destination inside the branch: a
                // benign no-op, like the CLI's "Nothing changed".
                None => format!(
                    "Nothing to rebase: the branch is already based on {}",
                    short_change_id(&anchor)
                ),
                Some(target) => {
                    // Both the moved commits (the target set's entry points)
                    // and the commits that gain a parent (insert-after's
                    // children / insert-before's target) get rewritten. For
                    // branch mode the moved roots imply the whole subtree,
                    // and immutability is ancestor-closed — an immutable
                    // descendant means an immutable root — so checking the
                    // roots covers the set.
                    let mut rewritten: Vec<CommitId> = match &target {
                        MoveCommitsTarget::Commits(ids) | MoveCommitsTarget::Roots(ids) => {
                            ids.clone()
                        }
                    };
                    rewritten.extend(new_child_ids.iter().cloned());
                    ensure_rewritable(
                        &repository.root,
                        settings,
                        &workspace_name,
                        tx.repo(),
                        &rewritten,
                        allow_immutable,
                    )
                    .await?;
                    let location = MoveCommitsLocation {
                        new_parent_ids,
                        new_child_ids,
                        target,
                    };
                    let stats = move_commits(tx.repo_mut(), &location, &RebaseOptions::default())
                        .await
                        .context("failed to rebase")?;
                    // Follow a lone rebased source under its new commit id,
                    // so the row the user acted on doesn't go stale in the
                    // sidebar selection.
                    if let [RevisionSelection::Commit(_)] = sources.as_slice()
                        && let Some(RebasedCommit::Rewritten(new_commit)) =
                            stats.rebased_commits.get(&source_ids[0])
                    {
                        rewritten_commit = Some(new_commit.id().hex());
                    }
                    rebase_stats_message(&stats, &short_change_id(&anchor), destination)
                }
            }
        }
        MutationOp::Squash { from, into } => {
            let mut source_commits: Vec<Commit> = Vec::new();
            let mut seen: HashSet<CommitId> = HashSet::new();
            for selection in from {
                let commit = resolve_mutation_target(tx.repo(), &context, selection).await?;
                if seen.insert(commit.id().clone()) {
                    source_commits.push(commit);
                }
            }
            let [first_source, ..] = source_commits.as_slice() else {
                bail!("squash needs at least one source revision");
            };
            let destination = match into {
                SquashTarget::Parent => {
                    if source_commits.len() > 1 {
                        bail!("pick an explicit destination when squashing several revisions");
                    }
                    let parents = first_source
                        .parents()
                        .await
                        .context("failed to load squash source parents")?;
                    match parents.as_slice() {
                        [parent] => parent.clone(),
                        [] => bail!(
                            "{} has no parent to squash into",
                            short_change_id(first_source)
                        ),
                        _ => bail!(
                            "{} is a merge, so pick a squash destination",
                            short_change_id(first_source)
                        ),
                    }
                }
                SquashTarget::Revision(target) => {
                    resolve_mutation_target(tx.repo(), &context, target).await?
                }
            };
            if seen.contains(destination.id()) {
                bail!("can't squash a revision into itself");
            }
            let mut rewritten: Vec<CommitId> = seen.iter().cloned().collect();
            rewritten.push(destination.id().clone());
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &rewritten,
                allow_immutable,
            )
            .await?;
            let source_names: Vec<String> = source_commits.iter().map(short_change_id).collect();
            let short_dest = short_change_id(&destination);
            let Some(new_destination) =
                squash_whole(tx.repo_mut(), source_commits, &destination).await?
            else {
                bail!(
                    "nothing to squash from {}: it has no changes",
                    source_names.join(", ")
                );
            };
            rewritten_commit = Some(new_destination.id().hex());
            format!("Squashed {} into {short_dest}", source_names.join(", "))
        }
        MutationOp::Merge { parents } => {
            let mut parent_commits: Vec<Commit> = Vec::new();
            let mut seen: HashSet<CommitId> = HashSet::new();
            for selection in parents {
                let commit = resolve_mutation_target(tx.repo(), &context, selection).await?;
                if seen.insert(commit.id().clone()) {
                    parent_commits.push(commit);
                }
            }
            if parent_commits.len() < 2 {
                bail!("a merge needs at least two distinct parents");
            }
            let names: Vec<String> = parent_commits.iter().map(short_change_id).collect();
            let tree = merge_commit_trees(tx.repo(), &parent_commits)
                .await
                .context("failed to merge parent trees")?;
            let parent_ids: Vec<CommitId> = parent_commits.iter().map(|c| c.id().clone()).collect();
            let merge_commit = tx
                .repo_mut()
                .new_commit(parent_ids, tree)
                .write()
                .await
                .context("failed to write merge commit")?;
            tx.repo_mut()
                .edit(workspace_name.clone(), &merge_commit)
                .await
                .context("failed to point working copy at merge commit")?;
            format!("New merge of {}", names.join(" + "))
        }
        MutationOp::Duplicate { target } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            let short = short_change_id(&commit);
            let stats = duplicate_commits_onto_parents(
                tx.repo_mut(),
                &[commit.id().clone()],
                &HashMap::new(),
            )
            .await
            .context("failed to duplicate")?;
            match stats.duplicated_commits.get(commit.id()) {
                Some(duplicate) => {
                    rewritten_commit = Some(duplicate.id().hex());
                    format!("Duplicated {short} as {}", short_change_id(duplicate))
                }
                None => format!("Duplicated {short}"),
            }
        }
        MutationOp::Absorb { from } => {
            let source = resolve_mutation_target(tx.repo(), &context, from).await?;
            let short = short_change_id(&source);
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[source.id().clone()],
                allow_immutable,
            )
            .await?;
            let absorb_source = AbsorbSource::from_commit(tx.repo(), source.clone())
                .await
                .context("failed to prepare absorb source")?;
            // Destinations mirror `jj absorb`'s default `--into`: the mutable
            // ancestors of the source's parents. Scoped so the resolver's
            // borrow of `tx` ends before the mutating absorb below.
            let destinations = {
                let mutable =
                    parse_user_revset(&repository.root, settings, &workspace_name, "mutable()")?;
                let symbol_resolver = SymbolResolver::new(
                    tx.repo(),
                    &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
                );
                let mutable = mutable
                    .resolve_user_expression(tx.repo(), &symbol_resolver)
                    .context("failed to resolve mutable()")?;
                RevsetExpression::commits(source.parent_ids().to_vec())
                    .ancestors()
                    .intersection(&mutable)
            };
            let selected =
                split_hunks_to_trees(tx.repo(), &absorb_source, &destinations, &EverythingMatcher)
                    .await
                    .context("failed to plan absorb")?;
            for (path, reason) in &selected.skipped_paths {
                output.push(format!(
                    "skipped {}: {reason}",
                    path.as_internal_file_string()
                ));
            }
            if selected.target_commits.is_empty() {
                bail!("nothing to absorb from {short}: no mutable ancestor touches the same lines");
            }
            let stats = absorb_hunks(tx.repo_mut(), &absorb_source, selected.target_commits)
                .await
                .context("failed to absorb")?;
            for commit in &stats.rewritten_destinations {
                let subject = commit.description().lines().next().unwrap_or("").trim();
                output.push(format!(
                    "absorbed into {} {subject}",
                    short_change_id(commit)
                ));
            }
            let count = stats.rewritten_destinations.len();
            let plural = if count == 1 { "" } else { "s" };
            format!("Absorbed {short} into {count} revision{plural}")
        }
        MutationOp::Commit { .. }
        | MutationOp::NewAt { .. }
        | MutationOp::Parallelize { .. }
        | MutationOp::DuplicateTo { .. }
        | MutationOp::KeepDivergent { .. }
        | MutationOp::Metaedit { .. } => {
            let guard = super::revops::RewriteGuard {
                repo_root: &repository.root,
                settings,
                workspace_name: &workspace_name,
                allow_immutable,
            };
            let outcome =
                super::revops::apply(tx.repo_mut(), &guard, &workspace_name, &context, op).await?;
            rewritten_commit = outcome.rewritten_commit;
            outcome.message
        }
        // Routed to `apply_view_only` before the lock was ever taken.
        MutationOp::MoveBookmark { .. }
        | MutationOp::DeleteBookmark { .. }
        | MutationOp::TrackBookmark { .. }
        | MutationOp::PushBookmark { .. }
        | MutationOp::CreateBookmark { .. }
        | MutationOp::RenameBookmark { .. }
        | MutationOp::ForgetBookmark { .. }
        | MutationOp::UntrackBookmark { .. }
        | MutationOp::Push(_) => {
            bail!("bookmark ops do not take the working-copy lock")
        }
        MutationOp::Undo { .. } | MutationOp::Redo | MutationOp::RevertOperation { .. } => {
            let plan = op_plan
                .as_ref()
                .expect("an undo resolves its plan before the fold");
            plan.apply(&mut tx, repo_loader, &workspace_name).await?;
            plan.message.clone()
        }
        MutationOp::Split {
            target,
            selection,
            first_description,
            second_description,
        } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[commit.id().clone()],
                allow_immutable,
            )
            .await?;
            let (first, second) = super::partial::split(
                tx.repo_mut(),
                &commit,
                selection,
                first_description.as_deref(),
                second_description,
            )
            .await?;
            if matches!(target, RevisionSelection::Commit(_)) {
                rewritten_commit = Some(first.id().hex());
            }
            format!(
                "Split {}: the rest of it is now {}",
                short_change_id(&first),
                short_change_id(&second)
            )
        }
        MutationOp::SquashChanges {
            from,
            selection,
            into,
        } => {
            let source = resolve_mutation_target(tx.repo(), &context, from).await?;
            let destination = match into {
                SquashTarget::Parent => {
                    let parents = source
                        .parents()
                        .await
                        .context("failed to load squash source parents")?;
                    match parents.as_slice() {
                        [parent] => parent.clone(),
                        [] => bail!("{} has no parent to squash into", short_change_id(&source)),
                        _ => bail!(
                            "{} is a merge, so pick a squash destination",
                            short_change_id(&source)
                        ),
                    }
                }
                SquashTarget::Revision(target) => {
                    resolve_mutation_target(tx.repo(), &context, target).await?
                }
            };
            if source.id() == destination.id() {
                bail!("can't move changes into the revision they come from");
            }
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[source.id().clone(), destination.id().clone()],
                allow_immutable,
            )
            .await?;
            let squashed =
                super::partial::squash_selection(tx.repo_mut(), &source, &destination, selection)
                    .await?;
            tx.repo_mut()
                .rebase_descendants()
                .await
                .context("failed to rebase descendants after the squash")?;
            // Keep showing the source (the diff the selection was made in)
            // unless it is gone, in which case follow the changes.
            if squashed.abandoned_source {
                rewritten_commit = Some(squashed.destination.id().hex());
            } else if matches!(from, RevisionSelection::Commit(_)) {
                rewritten_commit = super::partial::visible_commit_of(tx.repo(), source.change_id())
                    .await
                    .map(|id| id.hex());
            }
            let mut message = format!(
                "Moved {} from {} into {}",
                selection.summary(),
                short_change_id(&source),
                short_change_id(&squashed.destination)
            );
            if squashed.abandoned_source {
                message.push_str(" (source emptied, abandoned)");
            }
            message
        }
        MutationOp::RestoreChanges { target, selection } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[commit.id().clone()],
                allow_immutable,
            )
            .await?;
            let restored =
                super::partial::restore_selection(tx.repo_mut(), &commit, selection).await?;
            if matches!(target, RevisionSelection::Commit(_)) {
                rewritten_commit = Some(restored.id().hex());
            }
            format!(
                "Discarded {} in {}",
                selection.summary(),
                short_change_id(&commit)
            )
        }
        MutationOp::ResolveConflict {
            target,
            path,
            resolution,
        } => {
            let commit = resolve_mutation_target(tx.repo(), &context, target).await?;
            ensure_rewritable(
                &repository.root,
                settings,
                &workspace_name,
                tx.repo(),
                &[commit.id().clone()],
                allow_immutable,
            )
            .await?;
            // A merge tool may have been open for a while; refuse to resolve a
            // commit that has been rewritten since (that would resurrect it as
            // a divergent copy).
            if let Some(targets) = tx
                .repo()
                .resolve_change_id(commit.change_id())
                .await
                .context("failed to look up the conflicted revision")?
                && !targets.has_visible(commit.id())
            {
                bail!(
                    "{} changed while it was being resolved. Resolve it again.",
                    short_change_id(&commit)
                );
            }
            let resolved =
                super::resolve::resolve_conflict(tx.repo_mut(), &commit, path, resolution).await?;
            if matches!(target, RevisionSelection::Commit(_)) {
                rewritten_commit = Some(resolved.id().hex());
            }
            let how = match resolution {
                crate::resolve::ConflictResolution::Side(index) => {
                    format!("took side {}", index + 1)
                }
                crate::resolve::ConflictResolution::Base(index) => {
                    format!("took base {}", index + 1)
                }
                crate::resolve::ConflictResolution::Content { .. } => "merged".to_owned(),
                crate::resolve::ConflictResolution::Region { region, pick, .. } => {
                    let term = match pick {
                        crate::resolve::ConflictPick::Side(index) => format!("side {}", index + 1),
                        crate::resolve::ConflictPick::Base(index) => format!("base {}", index + 1),
                    };
                    format!("took {term} for conflict {}", region + 1)
                }
            };
            let path_conflicted = !resolved
                .tree()
                .path_value(&super::partial::repo_path(path)?)
                .await
                .context("failed to read the resolved path")?
                .is_resolved();
            let still = if path_conflicted {
                " (still conflicted)"
            } else if resolved.tree().has_conflict() {
                " (other conflicts remain)"
            } else {
                ""
            };
            format!(
                "Resolved {path} in {}: {how}{still}",
                short_change_id(&commit)
            )
        }
        MutationOp::RestoreOperation { .. } => {
            let (operation, view) =
                restore_target.expect("a restore resolves its target before the fold");
            let restored = super::history::restored_view(&view, tx.base_repo().view().store_view());
            tx.repo_mut().set_view(restored);
            format!(
                "Restored to operation {}",
                jj_cli::cli_util::short_operation_hash(operation.id())
            )
        }
    };

    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after mutation")?;

    // Mirror jj's own post-operation behavior: export the updated bookmarks to
    // the git repo so the real git branches (which jj surfaces as `name@git`)
    // follow the move, and — colocated — move this worktree's git HEAD and
    // index to `@`'s parent, so `git status` agrees with the graph.
    super::colocate::sync_git_after_transaction(tx.repo_mut(), &workspace_name, &repository.root)
        .await?;

    // Undo and redo are described exactly as jj describes them: the next
    // `jj undo` (or ours) reads the description to walk the stack.
    let description = match &op_plan {
        Some(plan) => plan.op_description.clone(),
        None => format!("diffui: {message}"),
    };
    tx.set_attribute("args".to_owned(), args);
    let new_repo = tx
        .commit(description)
        .await
        .context("failed to commit mutation transaction")?;

    // Check out the resulting `@` so the on-disk files match it.
    let new_wc_id = new_repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit after mutation")?
        .clone();
    let new_wc_commit = new_repo
        .store()
        .get_commit_async(&new_wc_id)
        .await
        .with_context(|| format!("failed to load new working-copy commit {}", new_wc_id.hex()))?;
    locked.check_out(&new_wc_commit).await?;
    locked.finish(new_repo.op_id().clone()).await?;

    Ok(mutation_outcome(
        op,
        message,
        output,
        rewritten_commit,
        &current_wc_change,
        new_wc_commit.change_id(),
        new_repo.op_id(),
    ))
}

/// The operation a revert targets, resolved before anything has been written.
pub(super) struct UndoTarget {
    pub(super) operation: Operation,
    pub(super) parent: Operation,
    pub(super) description: String,
}

/// Pick the operation `jj op revert` would revert — `operation_id`, or with
/// `None` the latest operation that isn't a working-copy snapshot — and check
/// that it *can* be reverted. Deliberately called before the pre-op
/// working-copy fold is committed: both failures here ("nothing to undo",
/// "can't undo a merge operation") used to surface only after that fold had
/// already landed as its own operation, with the lock left unfinished behind
/// it. (The toolbar's step-back undo walks jj's undo stack instead; see
/// [`super::undo`].)
pub(super) async fn resolve_undo_target(
    repo_loader: &RepoLoader,
    base_repo: &ReadonlyRepo,
    operation_id: Option<&str>,
) -> Result<UndoTarget> {
    let operation = match operation_id {
        Some(hex) => {
            let op_id = OperationId::try_from_hex(hex)
                .with_context(|| format!("invalid jj operation id {hex}"))?;
            let data = repo_loader
                .op_store()
                .read_operation(&op_id)
                .await
                .context("failed to load the operation to undo")?;
            Operation::new(repo_loader.op_store().clone(), op_id, data)
        }
        None => {
            let mut op = base_repo.operation().clone();
            while op.metadata().is_snapshot {
                let parents = op
                    .parents()
                    .await
                    .context("failed to read operation parents")?;
                match parents.as_slice() {
                    [parent] => op = parent.clone(),
                    _ => break,
                }
            }
            op
        }
    };
    let parents = operation
        .parents()
        .await
        .context("failed to read operation parents")?;
    let parent = match parents.as_slice() {
        [parent] => parent.clone(),
        [] => bail!("nothing to undo"),
        _ => bail!("can't undo a merge operation"),
    };
    let description = operation
        .metadata()
        .description
        .lines()
        .next()
        .filter(|line| !line.is_empty())
        .unwrap_or("operation")
        .to_owned();
    Ok(UndoTarget {
        operation,
        parent,
        description,
    })
}

/// The one place a mutation's user-visible aftermath is derived: whether `@`
/// moved, and which commit a target the caller still addresses by its old id
/// has become.
///
/// `moved_working_copy` compares the pre- and post-mutation working-copy
/// *change* ids rather than guessing per op — so an abandon that happened to
/// take `@` with it reports the move, while a describe upstream of `@` (which
/// rewrites `@`'s commit through the descendant rebase, leaving the user
/// exactly where they were) does not.
fn mutation_outcome(
    op: &MutationOp,
    message: String,
    output: Vec<String>,
    rewritten_commit: Option<String>,
    before: &ChangeId,
    after: &ChangeId,
    operation_id: &OperationId,
) -> MutationOutcome {
    let moved_working_copy = match op {
        // These move `@` by definition, even when the id happens to match
        // (re-editing the commit already checked out).
        MutationOp::New { .. } | MutationOp::Edit { .. } | MutationOp::Merge { .. } => true,
        _ => before != after,
    };
    MutationOutcome {
        message,
        moved_working_copy,
        rewritten_commit,
        output,
        operation_id: Some(operation_id.hex()),
    }
}

/// Resolve a rebase [`Destination`] into jj-lib's location parts: the new
/// parents, the new children (the commits that get the moved set inserted
/// under them), and the anchor commit for labels. Mirrors `jj rebase`'s
/// `-d` / `-A` / `-B` / `-A x -B y` resolution.
pub(super) async fn resolve_rebase_location(
    repo: &MutableRepo,
    context: &TargetContext,
    destination: &Destination,
) -> Result<(Vec<CommitId>, Vec<CommitId>, Commit)> {
    Ok(match destination {
        Destination::Onto(target) => {
            let anchor = resolve_mutation_target(repo, context, target).await?;
            (vec![anchor.id().clone()], Vec::new(), anchor)
        }
        Destination::After(target) => {
            let anchor = resolve_mutation_target(repo, context, target).await?;
            let children = RevsetExpression::commits(vec![anchor.id().clone()])
                .children()
                .evaluate(repo)
                .context("failed to resolve the target's children")?
                .stream()
                .map(|entry| entry.context("failed to walk the target's children"))
                .try_collect::<Vec<CommitId>>()
                .await?;
            (vec![anchor.id().clone()], children, anchor)
        }
        Destination::Before(target) => {
            let anchor = resolve_mutation_target(repo, context, target).await?;
            (
                anchor.parent_ids().to_vec(),
                vec![anchor.id().clone()],
                anchor,
            )
        }
        Destination::Between { parent, child } => {
            let parent = resolve_mutation_target(repo, context, parent).await?;
            let child = resolve_mutation_target(repo, context, child).await?;
            (vec![parent.id().clone()], vec![child.id().clone()], parent)
        }
    })
}

/// The ids in the reverse-topological order (children first) that
/// `MoveCommitsTarget::Commits` requires — revset iteration order guarantees
/// it.
pub(super) async fn reverse_topo_order(
    repo: &MutableRepo,
    ids: &[CommitId],
) -> Result<Vec<CommitId>> {
    RevsetExpression::commits(ids.to_vec())
        .evaluate(repo)
        .context("failed to order rebase sources")?
        .stream()
        .map(|entry| entry.context("failed to walk rebase sources"))
        .try_collect()
        .await
}

/// Lower a rebase mode + picked sources into jj-lib's move target. Branch
/// mode resolves here — against the destination — because the moved set is
/// `roots(destination..sources)` (the CLI's `-b`): every commit reachable
/// from the picked revisions but not from the new parents, entered at its
/// fork-point roots.
/// `None` means the branch has no commits outside the destination (the
/// destination is the branch itself or one of its descendants) — a benign
/// nothing-to-do, mirroring the CLI's "Nothing changed", not an error.
pub(super) async fn resolve_move_target(
    repo: &MutableRepo,
    mode: RebaseSourceMode,
    source_ids: &[CommitId],
    new_parent_ids: &[CommitId],
) -> Result<Option<MoveCommitsTarget>> {
    Ok(Some(match mode {
        RebaseSourceMode::Revisions => {
            MoveCommitsTarget::Commits(reverse_topo_order(repo, source_ids).await?)
        }
        RebaseSourceMode::WithDescendants => MoveCommitsTarget::Roots(source_ids.to_vec()),
        RebaseSourceMode::Branch => {
            let roots: Vec<CommitId> = RevsetExpression::commits(new_parent_ids.to_vec())
                .range(&RevsetExpression::commits(source_ids.to_vec()))
                .roots()
                .evaluate(repo)
                .context("failed to resolve the branch's fork-point roots")?
                .stream()
                .map(|entry| entry.context("failed to walk the branch roots"))
                .try_collect()
                .await?;
            if roots.is_empty() {
                return Ok(None);
            }
            MoveCommitsTarget::Roots(roots)
        }
    }))
}

/// A mutation refused because it would touch a commit in `immutable()` —
/// rewrite it, abandon it, or check it out for editing. Typed (and kept at
/// the root of the anyhow chain) so the frontend can downcast it and offer an
/// explicit rerun with `allow_immutable` instead of a dead-end failure.
#[derive(Debug, Clone)]
pub struct ImmutableRewriteError {
    /// Short change id of the first immutable commit the op hit.
    pub short_id: String,
}

impl std::fmt::Display for ImmutableRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "commit {} is immutable (usually pushed or shared history)",
            self.short_id
        )
    }
}

impl std::error::Error for ImmutableRewriteError {}

/// jj CLI parity: refuse to touch immutable commits (rewrites, and `edit`'s
/// checkout — a working copy parked on an immutable commit would amend it on
/// the next snapshot). `immutable()` resolves through the same alias map the
/// revset filter uses, so a user override of `immutable_heads()` is honored.
/// Callers skip this under the frontend's confirmed `allow_immutable`
/// override, mirroring `jj --ignore-immutable`.
pub(super) async fn ensure_rewritable(
    repo_root: &Path,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    repo: &MutableRepo,
    ids: &[CommitId],
    allow_immutable: bool,
) -> Result<()> {
    // The root commit is refused whatever the caller passed. It has no
    // content, no parent and no author; "rewrite anyway" is not a thing a user
    // can mean about it, so it is not offered — unlike an immutable commit,
    // where the override exists precisely because the user may know better.
    if ids.contains(repo.store().root_commit_id()) {
        bail!("the root commit cannot be rewritten");
    }
    if allow_immutable || ids.is_empty() {
        return Ok(());
    }
    let expr = parse_user_revset(repo_root, settings, workspace_name, "immutable()")?;
    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let resolved = expr
        .resolve_user_expression(repo, &symbol_resolver)
        .context("failed to resolve immutable()")?;
    let check = resolved.intersection(&RevsetExpression::commits(ids.to_vec()));
    let first = check
        .evaluate(repo)
        .context("failed to evaluate immutable()")?
        .stream()
        .next()
        .await
        .transpose()
        .context("failed to check immutability")?;
    if let Some(id) = first {
        let commit = repo
            .store()
            .get_commit_async(&id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        return Err(ImmutableRewriteError {
            short_id: short_change_id(&commit),
        }
        .into());
    }
    Ok(())
}

/// One-line activity summary for a finished rebase, from jj-lib's stats.
pub(super) fn rebase_stats_message(
    stats: &MoveCommitsStats,
    anchor: &str,
    destination: &Destination,
) -> String {
    let place = match destination {
        Destination::Onto(_) => "onto",
        Destination::After(_) => "after",
        Destination::Before(_) => "before",
        Destination::Between { .. } => "between",
    };
    let moved = stats.num_rebased_targets;
    if moved == 0 && stats.num_skipped_rebases > 0 {
        return "Nothing to rebase: already in place".to_owned();
    }
    let plural = if moved == 1 { "" } else { "s" };
    let mut message = format!("Rebased {moved} revision{plural} {place} {anchor}");
    if stats.num_rebased_descendants > 0 {
        let n = stats.num_rebased_descendants;
        let plural = if n == 1 { "" } else { "s" };
        message.push_str(&format!(" ({n} descendant{plural} followed)"));
    }
    if stats.num_abandoned_empty > 0 {
        message.push_str(&format!(
            " ({} emptied, abandoned)",
            stats.num_abandoned_empty
        ));
    }
    message
}

/// `jj squash`'s description for the destination: the one non-empty
/// description among it and the abandoned sources, as is (jj-cli's
/// `try_combine_messages`). When several have one, jj opens an editor on all
/// of them; diffui writes them destination first, a blank line apart, which
/// is what saving that editor unchanged gives. The user can refine it with
/// the inline description editor.
pub(super) fn combined_squash_description(destination: &Commit, abandoned: &[Commit]) -> String {
    if let Some(description) = try_combine_messages(abandoned, destination) {
        return description;
    }
    let parts: Vec<&str> = std::iter::once(destination)
        .chain(abandoned)
        .map(|commit| commit.description().trim())
        .filter(|description| !description.is_empty())
        .collect();
    // Stored descriptions end with a newline (jj's own convention).
    let mut combined = parts.join("\n\n");
    combined.push('\n');
    combined
}

/// Fold every change of `sources` into `destination` and abandon the
/// emptied sources, combining the descriptions (`jj squash --from … --into`).
/// `None` when the sources have nothing to move.
pub(super) async fn squash_whole(
    repo: &mut MutableRepo,
    sources: Vec<Commit>,
    destination: &Commit,
) -> Result<Option<Commit>> {
    let mut selections: Vec<CommitWithSelection> = Vec::new();
    for source in sources {
        selections.push(CommitWithSelection {
            selected_tree: source.tree(),
            parent_tree: source
                .parent_tree(repo)
                .await
                .context("failed to load squash source parent tree")?,
            commit: source,
        });
    }
    let Some(squashed) = squash_commits(repo, &selections, destination, false)
        .await
        .context("failed to squash")?
    else {
        return Ok(None);
    };
    let combined = combined_squash_description(destination, &squashed.abandoned_commits);
    let new_destination = squashed
        .commit_builder
        .set_description(combined)
        .write()
        .await
        .context("failed to write squashed commit")?;
    Ok(Some(new_destination))
}

/// Resolve a `RevisionSelection` to a `Commit`. The working-copy case uses
/// `context.wc_id` (the post-snapshot `@`); a commit the caller still names by
/// an id that has since been rewritten resolves to its visible successor (see
/// [`follow_rewritten`]).
pub(super) async fn resolve_mutation_target(
    repo: &dyn Repo,
    context: &TargetContext,
    target: &RevisionSelection,
) -> Result<Commit> {
    let commit_id = match target {
        RevisionSelection::WorkingCopy => context.wc_id.clone(),
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    if matches!(target, RevisionSelection::WorkingCopy) {
        return Ok(commit);
    }
    follow_rewritten(repo, commit, Some(context)).await
}

pub(super) fn short_change_id(commit: &Commit) -> String {
    // `to_string` is the k–z reverse-hex form jj log (and the sidebar) shows;
    // `.hex()` would print the raw hex nobody ever sees.
    commit.change_id().to_string().chars().take(8).collect()
}
