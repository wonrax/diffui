//! `jj git push`, with the CLI's safety checks, plus the bookmark bookkeeping
//! (`create` / `rename` / `forget` / `untrack`) the push workflow needs.
//!
//! Mirrors jj-cli 0.45.1 `cli/src/commands/git/push.rs`: the ref-level
//! classification (conflicted bookmark, untracked remote bookmark in the way,
//! refusing to create or delete a remote bookmark the user didn't name) and
//! the per-commit checks over everything the push would make newly reachable
//! on the remote — empty description, missing author/committer, conflicts,
//! `git.private-commits`. The lease (`--force-with-lease` against the last
//! fetched position) is jj-lib's `push_refs` itself.
//!
//! Everything here runs inside the caller's transaction, so a refused push
//! commits nothing — not even the bookmark a `--change` push would create.
//!
//! `git.sign-on-push` is the CLI's `sign_commits_before_push`: the commits
//! about to be pushed that aren't signed yet are signed (immutable ones are
//! skipped with a warning) and their descendants rebased. The caller runs that
//! in a transaction of its own under the working-copy lock, so a failed push
//! keeps the signatures.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, TryStreamExt};
use jj_lib::{
    backend::{BackendError, CommitId},
    commit::Commit,
    config::ConfigGetResultExt as _,
    git::{
        GitPushOptions, GitPushRefTargets, GitSubprocessOptions, REMOTE_NAME_FOR_LOCAL_GIT_REPO,
        get_all_remote_names, push_refs,
    },
    merge::Diff,
    object_id::ObjectId,
    op_store::RefTarget,
    ref_name::{RefName, RefNameBuf, RemoteName, WorkspaceName},
    refs::{LocalAndRemoteRef, RefPushAction, classify_ref_push_action},
    repo::{MutableRepo, Repo},
    revset::{ResolvedRevsetExpression, RevsetDiagnostics, RevsetExpression, SymbolResolver},
    settings::UserSettings,
    signing::{SignBehavior, SignError},
    ssh_signing::SshError,
};

use super::mutate::{resolve_mutation_target, short_change_id};
use super::remote::CollectingCallback;
use super::walk::parse_user_revset;
use crate::model::LoadProgress;
use crate::push::{PushAllow, PushCheck, PushRejection, PushRequest, PushTarget, RejectedCommit};

/// jj-cli's default `templates.git_push_bookmark`.
pub const DEFAULT_PUSH_BOOKMARK_TEMPLATE: &str = r#""push-" ++ change_id.short()"#;

/// How many offending commits a rejection lists before it says "and more".
const MAX_REJECTED: usize = 20;

/// Where the push runs from: the repo's root (revsets resolve paths against
/// it), the pushing workspace, and its `@`.
pub(super) struct PushContext<'a> {
    pub settings: &'a UserSettings,
    pub repo_root: &'a Path,
    pub workspace_name: &'a WorkspaceName,
    pub wc_commit_id: &'a CommitId,
}

/// A finished push: the activity summary and the remote's sideband output.
pub(super) struct PushOutcome {
    pub message: String,
    pub output: Vec<String>,
    /// Set when the remote refused some refs but others went through: the
    /// transaction must still be committed (the remote did change), and the
    /// caller reports this as the failure it is afterwards.
    pub partial_failure: Option<String>,
}

/// One bookmark the request names, with the CLI's per-flag allowances.
struct Named {
    name: RefNameBuf,
    allow_new: bool,
    allow_delete: bool,
}

/// A push that passed every check, ready to sign and send. `updates` is empty
/// when every bookmark it names already matches the remote.
pub(super) struct PlannedPush {
    remote: String,
    updates: Vec<(RefNameBuf, Diff<Option<CommitId>>)>,
    /// Lines that lead the activity output: bookmarks created or already in
    /// place, and what signing did.
    pub notes: Vec<String>,
    plan: Vec<String>,
}

/// Plan `request` inside `repo` (a transaction's mutable repo): apply its
/// local bookmark changes, classify every bookmark it names, and check the
/// commits the push would publish. A [`PushRejection`] comes back as the error
/// (downcastable through anyhow) when a commit check fails.
pub(super) async fn plan_push(
    context: &PushContext<'_>,
    repo: &mut MutableRepo,
    request: &PushRequest,
) -> Result<PlannedPush> {
    let remote = RemoteName::new(&request.remote);
    if remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO {
        bail!("git remote named 'git' is reserved for the local Git repository");
    }
    let remotes = get_all_remote_names(repo.store()).context("failed to list git remotes")?;
    if !remotes.iter().any(|name| name.as_str() == remote.as_str()) {
        bail!("no git remote named '{}'", request.remote);
    }

    let mut notes: Vec<String> = Vec::new();
    let mut named: Vec<Named> = Vec::new();
    let mut tracked_mode = false;
    for target in &request.targets {
        match target {
            PushTarget::Bookmark(name) => {
                let name = RefName::new(name);
                let local_present = repo.view().get_local_bookmark(name).is_present();
                let remote_tracked = repo
                    .view()
                    .get_remote_bookmark(name.to_remote_symbol(remote))
                    .is_tracked();
                if !local_present && !remote_tracked {
                    bail!("no bookmark named {} to push", name.as_str());
                }
                named.push(Named {
                    name: name.to_owned(),
                    allow_new: !has_tracked_remote_bookmarks(repo, name),
                    allow_delete: true,
                });
            }
            PushTarget::SetBookmark { name, to } => {
                let commit = resolve_mutation_target(repo, context.wc_commit_id, to).await?;
                let name = RefName::new(name);
                repo.set_local_bookmark_target(name, RefTarget::normal(commit.id().clone()));
                named.push(Named {
                    name: name.to_owned(),
                    allow_new: !has_tracked_remote_bookmarks(repo, name),
                    allow_delete: true,
                });
            }
            PushTarget::DeleteBookmark(name) => {
                let name = RefName::new(name);
                if !repo
                    .view()
                    .get_remote_bookmark(name.to_remote_symbol(remote))
                    .is_tracked()
                {
                    bail!(
                        "bookmark {} doesn't track {}; there is nothing to delete there",
                        name.as_str(),
                        request.remote
                    );
                }
                repo.set_local_bookmark_target(name, RefTarget::absent());
                named.push(Named {
                    name: name.to_owned(),
                    allow_new: false,
                    allow_delete: true,
                });
            }
            PushTarget::Change(revision) => {
                let commit = resolve_mutation_target(repo, context.wc_commit_id, revision).await?;
                let name = push_bookmark_name(context.settings, &commit)?;
                let name = RefNameBuf::from(name);
                let target = RefTarget::normal(commit.id().clone());
                if repo.view().get_local_bookmark(&name) != &target {
                    ensure_new_bookmark_name(repo, &name)?;
                    repo.set_local_bookmark_target(&name, target);
                    notes.push(format!(
                        "Creating bookmark {} for revision {}",
                        name.as_str(),
                        short_change_id(&commit)
                    ));
                }
                named.push(Named {
                    name,
                    allow_new: true,
                    allow_delete: false,
                });
            }
            PushTarget::Tracked => tracked_mode = true,
        }
    }

    // Classify. An explicitly named bookmark that can't be pushed fails the
    // whole push, as it does in the CLI.
    let mut updates: Vec<(RefNameBuf, Diff<Option<CommitId>>)> = Vec::new();
    let mut seen: HashSet<RefNameBuf> = HashSet::new();
    for item in &named {
        if !seen.insert(item.name.clone()) {
            continue;
        }
        match classify(repo, &item.name, remote, item.allow_new, item.allow_delete)? {
            Some(update) => updates.push((item.name.clone(), update)),
            None => notes.push(format!(
                "Bookmark {}@{} already matches {}",
                item.name.as_str(),
                remote.as_str(),
                item.name.as_str()
            )),
        }
    }
    if tracked_mode {
        // `jj git push` with no target flags: tracked bookmarks pointing into
        // `remote_bookmarks(remote=<remote>)..@`. Ones that can't be pushed are
        // skipped with a note rather than failing the rest.
        let in_range = default_push_range(repo, remote, context.wc_commit_id).await?;
        let candidates: Vec<RefNameBuf> = repo
            .view()
            .local_remote_bookmarks(remote)
            .filter(|(_, targets)| targets.remote_ref.is_tracked())
            .filter(|(_, targets)| {
                targets
                    .local_target
                    .added_ids()
                    .any(|id| in_range.contains(id))
            })
            .map(|(name, _)| name.to_owned())
            .collect();
        for name in candidates {
            if !seen.insert(name.clone()) {
                continue;
            }
            match classify(repo, &name, remote, false, false) {
                Ok(Some(update)) => updates.push((name, update)),
                Ok(None) => {}
                Err(error) => notes.push(format!("skipped: {error}")),
            }
        }
    }
    if updates.is_empty() {
        return Ok(PlannedPush {
            remote: request.remote.clone(),
            updates,
            notes,
            plan: Vec::new(),
        });
    }

    let new_heads: Vec<CommitId> = updates
        .iter()
        .filter_map(|(_, update)| update.after.clone())
        .collect();
    let (commits, truncated, private_revset) =
        check_commits(context, repo, remote, &new_heads, request.allow).await?;
    let plan = describe_updates(repo, &updates).await;
    if !commits.is_empty() || request.confirm {
        return Err(anyhow!(PushRejection {
            remote: request.remote.clone(),
            plan,
            commits,
            truncated,
            private_commits: private_revset,
            request: request.clone(),
        }));
    }
    Ok(PlannedPush {
        remote: request.remote.clone(),
        updates,
        notes,
        plan,
    })
}

/// Push `planned` from `repo`, the transaction it was planned in.
pub(super) fn send_push(
    context: &PushContext<'_>,
    repo: &mut MutableRepo,
    planned: PlannedPush,
    progress: &LoadProgress,
) -> Result<PushOutcome> {
    let PlannedPush {
        remote: remote_name,
        updates,
        mut notes,
        plan,
    } = planned;
    if updates.is_empty() {
        notes.push("Nothing changed.".to_owned());
        return Ok(PushOutcome {
            message: format!("Nothing to push to {remote_name}"),
            output: notes,
            partial_failure: None,
        });
    }
    let remote = RemoteName::new(&remote_name);
    let targets = GitPushRefTargets {
        bookmarks: updates.clone(),
        tags: Vec::new(),
    };
    let mut subprocess_options = GitSubprocessOptions::from_settings(context.settings)
        .context("failed to read git subprocess options")?;
    let askpass = crate::askpass::Scope::new();
    subprocess_options.environment.extend(askpass.environment());
    // Collect the remote sideband (GitHub's "create a pull request" hint + URL)
    // for the activity log, and forward git's transfer progress to the bar.
    let mut callback = CollectingCallback::new(progress.clone());
    let stats = push_refs(
        repo,
        subprocess_options,
        remote,
        &targets,
        &mut callback,
        &GitPushOptions::default(),
    )
    .map_err(|error| askpass.explain(error.into()))
    .with_context(|| format!("failed to push to {remote_name}"))?;

    let mut output = notes;
    output.extend(plan);
    output.append(&mut callback.lines);
    let names: Vec<&str> = updates.iter().map(|(name, _)| name.as_str()).collect();
    let message = format!("Pushed {} to {remote_name}", names.join(", "));

    if stats.all_ok() {
        return Ok(PushOutcome {
            message,
            output,
            partial_failure: None,
        });
    }
    let mut problems = Vec::new();
    for (git_ref, reason) in &stats.rejected {
        let reason = reason.as_deref().unwrap_or("rejected");
        problems.push(format!(
            "{}: {reason} (the remote moved since the last fetch; fetch and try again)",
            git_ref.as_str()
        ));
    }
    for (git_ref, reason) in &stats.remote_rejected {
        let reason = reason.as_deref().unwrap_or("rejected by the remote");
        problems.push(format!("{}: {reason}", git_ref.as_str()));
    }
    for (symbol, reason) in &stats.unexported_bookmarks {
        problems.push(format!("{symbol}: couldn't export to git ({reason})"));
    }
    let failure = format!("push rejected: {}", problems.join("; "));
    if !stats.some_exported() {
        return Err(PushFailed(failure).into());
    }
    Ok(PushOutcome {
        message,
        output,
        partial_failure: Some(failure),
    })
}

/// The remote refused refs the push sent. Typed so the actor reports git's
/// own reason (`stale info` for a failed lease) as-is instead of matching on
/// its words.
#[derive(Debug)]
pub struct PushFailed(pub String);

impl std::fmt::Display for PushFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PushFailed {}

/// What `git.sign-on-push` asks of a planned push.
#[derive(Default)]
pub(super) struct SignPlan {
    /// Mutable commits to sign before the push goes out.
    pub to_sign: Vec<CommitId>,
    /// The CLI's "Skipped signing N immutable commits" warning, when any are.
    pub skipped_note: Option<String>,
}

/// jj-cli's `sign_commits_before_push` selection: the commits in
/// `remote_bookmarks(remote)..new_heads` that aren't signed yet and that
/// `signing.behavior = "own"` would sign. Immutable ones are left alone and
/// named in the warning instead. Empty when `git.sign-on-push` is off.
pub(super) async fn sign_plan(
    context: &PushContext<'_>,
    repo: &MutableRepo,
    planned: &PlannedPush,
) -> Result<SignPlan> {
    let sign_on_push = context
        .settings
        .get_bool("git.sign-on-push")
        .optional()
        .context("failed to read git.sign-on-push")?
        .unwrap_or(false);
    let new_heads: Vec<CommitId> = planned
        .updates
        .iter()
        .filter_map(|(_, update)| update.after.clone())
        .collect();
    if !sign_on_push || new_heads.is_empty() {
        return Ok(SignPlan::default());
    }
    let old_heads: Vec<CommitId> = repo
        .view()
        .remote_bookmarks(RemoteName::new(&planned.remote))
        .flat_map(|(_, remote_ref)| remote_ref.target.added_ids())
        .cloned()
        .collect();
    let to_push = RevsetExpression::commits(old_heads).range(&RevsetExpression::commits(new_heads));
    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let immutable = parse_user_revset(
        context.repo_root,
        context.settings,
        context.workspace_name,
        "immutable()",
    )?
    .resolve_user_expression(repo, &symbol_resolver)
    .context("failed to resolve immutable()")?;

    let mut sign_settings = context.settings.sign_settings();
    sign_settings.behavior = SignBehavior::Own;
    let needs_signing =
        |commit: &Commit| !commit.is_signed() && sign_settings.should_sign(commit.store_commit());

    // Like the CLI, name up to ten and say "10+" past that.
    let mut skipped = Vec::new();
    let mut stream = to_push
        .intersection(&immutable)
        .evaluate(repo)
        .context("failed to evaluate the commits to sign")?
        .stream();
    while let Some(id) = stream.next().await {
        let commit = load_commit(repo, &id.context("failed to walk the commits to sign")?).await?;
        if needs_signing(&commit) {
            skipped.push(short_change_id(&commit));
            if skipped.len() > 10 {
                break;
            }
        }
    }
    let mut to_sign = Vec::new();
    let mut stream = to_push
        .minus(&immutable)
        .evaluate(repo)
        .context("failed to evaluate the commits to sign")?
        .stream();
    while let Some(id) = stream.next().await {
        let commit = load_commit(repo, &id.context("failed to walk the commits to sign")?).await?;
        if needs_signing(&commit) {
            to_sign.push(commit.id().clone());
        }
    }
    if !to_sign.is_empty() && !repo.store().signer().can_sign() {
        bail!("git.sign-on-push is set, but no signing.backend is configured");
    }

    let skipped_note = (!skipped.is_empty()).then(|| {
        let count = match skipped.len() {
            1 => "1 immutable commit".to_owned(),
            n if n > 10 => "10+ immutable commits".to_owned(),
            n => format!("{n} immutable commits"),
        };
        skipped.truncate(10);
        format!("Skipped signing {count}: {}", skipped.join(", "))
    });
    Ok(SignPlan {
        to_sign,
        skipped_note,
    })
}

async fn load_commit(repo: &MutableRepo, id: &CommitId) -> Result<Commit> {
    repo.store()
        .get_commit_async(id)
        .await
        .with_context(|| format!("failed to load jj commit {}", id.hex()))
}

/// What signing before a push rewrote.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Signed {
    pub signed: usize,
    pub rebased: usize,
}

impl Signed {
    /// The activity line, `Signed 2 commits, rebased 1 descendant`.
    pub fn note(&self) -> String {
        let plural = |n: usize, word: &str| {
            if n == 1 {
                format!("{n} {word}")
            } else {
                format!("{n} {word}s")
            }
        };
        let mut note = format!("Signed {}", plural(self.signed, "commit"));
        if self.rebased > 0 {
            note.push_str(&format!(", rebased {}", plural(self.rebased, "descendant")));
        }
        note
    }
}

/// Rewrite `ids` signed and rebase their descendants onto them (`@` among
/// them, when it descends), as jj-cli does before a push. Bookmarks on the
/// rewritten commits follow them. A failure in the signing backend itself (a
/// cancelled 1Password prompt, a missing key) comes back as [`SignFailed`].
pub(super) async fn sign_commits(repo: &mut MutableRepo, ids: &[CommitId]) -> Result<Signed> {
    let wanted: HashSet<&CommitId> = ids.iter().collect();
    let mut stats = Signed::default();
    repo.transform_descendants(ids.to_vec(), async |rewriter| {
        if wanted.contains(rewriter.old_commit().id()) {
            rewriter
                .reparent()
                .set_sign_behavior(SignBehavior::Own)
                .write()
                .await?;
            stats.signed += 1;
        } else {
            rewriter.reparent().write().await?;
            stats.rebased += 1;
        }
        Ok(())
    })
    .await
    .map_err(sign_error)?;
    // Settle the rewrites the walk recorded, as committing a transaction
    // requires (the CLI's `tx.finish` does the same).
    repo.rebase_descendants()
        .await
        .context("failed to rebase descendants after signing")?;
    Ok(stats)
}

/// [`SignFailed`] with the backend's own words when the signer is what
/// failed; any other write error keeps its chain.
fn sign_error(error: BackendError) -> anyhow::Error {
    let mut is_sign_error = false;
    let mut root = error.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(cause) = source {
        is_sign_error |= cause.is::<SignError>();
        root = match cause.downcast_ref::<SshError>() {
            // The signing program's own words ("user cancelled", say) beat
            // jj's "SSH sign failed with exit status" wrapper.
            Some(SshError::Command { stderr, .. }) if !stderr.trim().is_empty() => stderr.clone(),
            _ => cause.to_string(),
        };
        source = cause.source();
    }
    if !is_sign_error {
        return anyhow::Error::new(error).context("failed to rewrite the commits to sign");
    }
    let reason = root.split_whitespace().collect::<Vec<_>>().join(" ");
    let reason = reason.trim_end_matches(':');
    SignFailed(format!("signing failed, nothing was pushed: {reason}")).into()
}

/// The signing backend refused to sign the commits a push needed signed.
#[derive(Debug)]
pub struct SignFailed(pub String);

impl std::fmt::Display for SignFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignFailed {}

/// The CLI's `classify_bookmark_update`, with its messages and hints.
fn classify(
    repo: &MutableRepo,
    name: &RefName,
    remote: &RemoteName,
    allow_new: bool,
    allow_delete: bool,
) -> Result<Option<Diff<Option<CommitId>>>> {
    let symbol = name.to_remote_symbol(remote);
    let targets = LocalAndRemoteRef {
        local_target: repo.view().get_local_bookmark(name),
        remote_ref: repo.view().get_remote_bookmark(symbol),
    };
    match classify_ref_push_action(targets) {
        RefPushAction::AlreadyMatches => Ok(None),
        RefPushAction::LocalConflicted => bail!(
            "bookmark {} is conflicted; move it onto one revision to resolve it first",
            name.as_str()
        ),
        RefPushAction::RemoteConflicted => {
            bail!("bookmark {symbol} is conflicted; fetch to update the conflicted remote bookmark")
        }
        RefPushAction::RemoteUntracked => bail!(
            "non-tracking remote bookmark {symbol} exists; track it (jj bookmark track \
             {symbol}) to push over it"
        ),
        RefPushAction::Update(_) if !targets.remote_ref.is_tracked() && !allow_new => {
            bail!("refusing to create new remote bookmark {symbol}; track it and try again")
        }
        RefPushAction::Update(update) if update.after.is_none() && !allow_delete => {
            bail!("refusing to push deleted bookmark {}", name.as_str())
        }
        RefPushAction::Update(update) => Ok(Some(update)),
    }
}

/// The CLI's `CommitsValidator`, collecting every offending commit instead of
/// stopping at the first: the range `(remote heads | immutable_heads())..new_heads`
/// is exactly what the push would make newly reachable on the remote.
async fn check_commits(
    context: &PushContext<'_>,
    repo: &MutableRepo,
    remote: &RemoteName,
    new_heads: &[CommitId],
    allow: PushAllow,
) -> Result<(Vec<RejectedCommit>, bool, Option<String>)> {
    if new_heads.is_empty() {
        return Ok((Vec::new(), false, None));
    }
    let known_heads: Vec<CommitId> = repo
        .view()
        .remote_bookmarks(remote)
        .flat_map(|(_, remote_ref)| remote_ref.target.added_ids())
        .cloned()
        .collect();
    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let immutable_heads = parse_user_revset(
        context.repo_root,
        context.settings,
        context.workspace_name,
        "immutable_heads()",
    )?
    .resolve_user_expression(repo, &symbol_resolver)
    .context("failed to resolve immutable_heads()")?;

    let private_revset = if allow.private {
        None
    } else {
        context
            .settings
            .get_string("git.private-commits")
            .optional()
            .context("failed to read git.private-commits")?
            .filter(|revset| revset.trim() != "none()")
    };
    let private = match &private_revset {
        Some(revset) => Some(
            parse_user_revset(
                context.repo_root,
                context.settings,
                context.workspace_name,
                revset,
            )
            .context("failed to parse git.private-commits")?
            .resolve_user_expression(repo, &symbol_resolver)
            .context("failed to resolve git.private-commits")?
            .evaluate(repo)
            .context("failed to evaluate git.private-commits")?,
        ),
        None => None,
    };
    let is_private = private.as_ref().map(|revset| revset.containing_fn());

    let new_commits: std::sync::Arc<ResolvedRevsetExpression> =
        RevsetExpression::commits(known_heads)
            .union(&immutable_heads)
            .range(&RevsetExpression::commits(new_heads.to_vec()));
    let mut stream = new_commits
        .evaluate(repo)
        .context("failed to evaluate the commits to push")?
        .stream();
    let mut rejected = Vec::new();
    let mut truncated = false;
    let mut any_private = false;
    while let Some(id) = stream.next().await {
        let id = id.context("failed to walk the commits to push")?;
        let commit = repo
            .store()
            .get_commit_async(&id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        let mut reasons = Vec::new();
        if commit.description().is_empty() && !allow.empty_description {
            reasons.push(PushCheck::EmptyDescription);
        }
        if commit.author().name.is_empty()
            || commit.author().email.is_empty()
            || commit.committer().name.is_empty()
            || commit.committer().email.is_empty()
        {
            reasons.push(PushCheck::MissingIdentity);
        }
        if commit.has_conflict() && !allow.conflicts {
            reasons.push(PushCheck::Conflicts);
        }
        if let Some(is_private) = &is_private
            && is_private(commit.id())
                .await
                .context("failed to check git.private-commits")?
        {
            reasons.push(PushCheck::Private);
            any_private = true;
        }
        if reasons.is_empty() {
            continue;
        }
        if rejected.len() == MAX_REJECTED {
            truncated = true;
            break;
        }
        rejected.push(rejected_commit(&commit, reasons, context.wc_commit_id));
    }
    Ok((rejected, truncated, private_revset.filter(|_| any_private)))
}

fn rejected_commit(commit: &Commit, reasons: Vec<PushCheck>, wc: &CommitId) -> RejectedCommit {
    RejectedCommit {
        change_id: short_change_id(commit),
        commit_id: commit.id().hex().chars().take(12).collect(),
        subject: commit
            .description()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_owned(),
        reasons,
        working_copy: commit.id() == wc,
    }
}

/// Commits in `remote_bookmarks(remote=<remote>)..@` — the default push range.
async fn default_push_range(
    repo: &MutableRepo,
    remote: &RemoteName,
    wc_commit_id: &CommitId,
) -> Result<HashSet<CommitId>> {
    let remote_heads: Vec<CommitId> = repo
        .view()
        .remote_bookmarks(remote)
        .flat_map(|(_, remote_ref)| remote_ref.target.added_ids())
        .cloned()
        .collect();
    let range: std::sync::Arc<ResolvedRevsetExpression> = RevsetExpression::commits(remote_heads)
        .range(&RevsetExpression::commits(vec![wc_commit_id.clone()]));
    range
        .evaluate(repo)
        .context("failed to evaluate the default push range")?
        .stream()
        .map(|entry| entry.context("failed to walk the default push range"))
        .try_collect()
        .await
}

/// `  bookmark: main [move forward from abc to def]`, as the CLI prints the
/// plan before pushing.
async fn describe_updates(
    repo: &MutableRepo,
    updates: &[(RefNameBuf, Diff<Option<CommitId>>)],
) -> Vec<String> {
    let short = |id: &CommitId| id.hex().chars().take(12).collect::<String>();
    let mut lines = Vec::new();
    for (name, update) in updates {
        let what = match (&update.before, &update.after) {
            (Some(old), Some(new)) => {
                let index = repo.index();
                let direction = if index.is_ancestor(old, new).await.unwrap_or(false) {
                    "forward"
                } else if index.is_ancestor(new, old).await.unwrap_or(false) {
                    "backward"
                } else {
                    "sideways"
                };
                format!("move {direction} from {} to {}", short(old), short(new))
            }
            (Some(old), None) => format!("delete from {}", short(old)),
            (None, Some(new)) => format!("add to {}", short(new)),
            (None, None) => continue,
        };
        lines.push(format!("bookmark: {} [{what}]", name.as_str()));
    }
    lines
}

/// Whether any real remote (not the colocated `git` pseudo-remote) tracks
/// `name` — the CLI's `has_tracked_remote_bookmarks`.
pub(super) fn has_tracked_remote_bookmarks(repo: &dyn Repo, name: &RefName) -> bool {
    repo.view()
        .all_remote_bookmarks()
        .any(|(symbol, remote_ref)| {
            symbol.name == name
                && symbol.remote != REMOTE_NAME_FOR_LOCAL_GIT_REPO
                && remote_ref.is_tracked()
        })
}

/// The CLI's `ensure_new_bookmark_name` for `--change`.
fn ensure_new_bookmark_name(repo: &dyn Repo, name: &RefName) -> Result<()> {
    if repo.view().get_local_bookmark(name).is_present() {
        bail!(
            "bookmark already exists: {}; move it and push it by name instead",
            name.as_str()
        );
    }
    if has_tracked_remote_bookmarks(repo, name) {
        bail!(
            "tracked remote bookmarks exist for deleted bookmark: {}; recreate it with \
             `jj bookmark set` or untrack them first",
            name.as_str()
        );
    }
    Ok(())
}

/// The bookmark name `jj git push --change` would generate for `commit`.
///
/// jj-cli evaluates `templates.git_push_bookmark` with its template engine,
/// which lives in jj-cli, not jj-lib. This evaluates the shapes that setting
/// takes in practice — string literals concatenated with `++` and
/// `change_id.short([n])` / `commit_id.short([n])` — and refuses anything else
/// rather than inventing a name the user's config didn't ask for.
pub(super) fn push_bookmark_name(settings: &UserSettings, commit: &Commit) -> Result<String> {
    let template = settings
        .get_string("templates.git_push_bookmark")
        .optional()
        .context("failed to read templates.git_push_bookmark")?
        .unwrap_or_else(|| DEFAULT_PUSH_BOOKMARK_TEMPLATE.to_owned());
    let name = eval_push_bookmark_template(
        &template,
        &commit.change_id().reverse_hex(),
        &commit.id().hex(),
    )?;
    validate_bookmark_name(&name)
        .with_context(|| format!("templates.git_push_bookmark produced {name:?}"))?;
    Ok(name)
}

pub(super) fn eval_push_bookmark_template(
    template: &str,
    change_id: &str,
    commit_id: &str,
) -> Result<String> {
    let unsupported = || {
        anyhow!(
            "templates.git_push_bookmark = {template:?} uses template features diffui can't \
             evaluate (it supports string literals, ++, and change_id/commit_id.short()); \
             push with `jj git push --change` instead"
        )
    };
    let mut out = String::new();
    let mut rest = template.trim();
    loop {
        if let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let mut chars = rest[1..].char_indices();
            let mut literal = String::new();
            let mut end = None;
            while let Some((index, c)) = chars.next() {
                match c {
                    '\\' if quote == '"' => match chars.next() {
                        Some((_, 'n')) => literal.push('\n'),
                        Some((_, 't')) => literal.push('\t'),
                        Some((_, escaped)) => literal.push(escaped),
                        None => return Err(unsupported()),
                    },
                    c if c == quote => {
                        end = Some(index + 2);
                        break;
                    }
                    c => literal.push(c),
                }
            }
            let end = end.ok_or_else(unsupported)?;
            out.push_str(&literal);
            rest = rest[end..].trim_start();
        } else {
            let (id, after) = if let Some(after) = rest.strip_prefix("change_id.short(") {
                (change_id, after)
            } else if let Some(after) = rest.strip_prefix("commit_id.short(") {
                (commit_id, after)
            } else {
                return Err(unsupported());
            };
            let close = after.find(')').ok_or_else(unsupported)?;
            let arg = after[..close].trim();
            let len = if arg.is_empty() {
                12
            } else {
                arg.parse::<usize>().map_err(|_| unsupported())?
            };
            out.extend(id.chars().take(len));
            rest = after[close + 1..].trim_start();
        }
        if rest.is_empty() {
            break;
        }
        rest = rest
            .strip_prefix("++")
            .ok_or_else(unsupported)?
            .trim_start();
    }
    if out.is_empty() {
        bail!("templates.git_push_bookmark generated an empty bookmark name");
    }
    Ok(out)
}

/// A bookmark name jj accepts unquoted that is also a valid git branch name
/// (`git check-ref-format --branch`), so it can be exported and pushed.
pub fn validate_bookmark_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("bookmark name is empty");
    }
    match jj_lib::revset::parse_symbol(name) {
        Ok(parsed) if parsed == name => {}
        _ => bail!("{name:?} isn't a valid jj bookmark name"),
    }
    let bad = name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name.split('/').any(|part| part.starts_with('.'))
        || name
            .chars()
            .any(|c| c.is_control() || " ~^:?*[\\".contains(c));
    if bad {
        bail!("{name:?} isn't a valid git branch name");
    }
    Ok(())
}

/// `jj bookmark create -r <to> <name>`, including the
/// `remotes.<remote>.auto-track-(created-)bookmarks` tracking.
pub(super) async fn create_bookmark(
    settings: &UserSettings,
    repo: &mut MutableRepo,
    name: &str,
    target: &Commit,
) -> Result<()> {
    validate_bookmark_name(name)?;
    let ref_name = RefName::new(name);
    if repo.view().get_local_bookmark(ref_name).is_present() {
        bail!("bookmark already exists: {name}");
    }
    if has_tracked_remote_bookmarks(repo, ref_name) {
        bail!(
            "tracked remote bookmarks exist for deleted bookmark: {name}; move it back with \
             \"Move bookmark here\" or untrack them first"
        );
    }
    repo.set_local_bookmark_target(ref_name, RefTarget::normal(target.id().clone()));
    let remote_settings = settings
        .remote_settings()
        .context("failed to read remotes.* settings")?;
    for (remote, remote_setting) in &remote_settings {
        let mut exprs = Vec::new();
        for (key, text) in [
            ("auto-track-bookmarks", &remote_setting.auto_track_bookmarks),
            (
                "auto-track-created-bookmarks",
                &remote_setting.auto_track_created_bookmarks,
            ),
        ] {
            let Some(text) = text else { continue };
            let expr = jj_lib::revset::parse_string_expression(&mut RevsetDiagnostics::new(), text)
                .with_context(|| format!("invalid remotes.{}.{key}", remote.as_str()))?;
            exprs.push(expr);
        }
        if exprs.is_empty() {
            continue;
        }
        let matcher = jj_lib::str_util::StringExpression::union_all(exprs).to_matcher();
        if !matcher.is_match(name) || repo.view().get_remote_view(remote).is_none() {
            continue;
        }
        repo.track_remote_bookmark(ref_name.to_remote_symbol(remote))
            .await
            .with_context(|| format!("failed to track {name}@{}", remote.as_str()))?;
    }
    Ok(())
}

/// `jj bookmark rename <old> <new>`: the local target moves to the new name
/// and tracking carries over to the new name's remote refs where nothing
/// untracked is in the way. The remote bookmarks under the old name stay —
/// renaming on the remote is a delete push plus a push.
pub(super) async fn rename_bookmark(repo: &mut MutableRepo, old: &str, new: &str) -> Result<()> {
    validate_bookmark_name(new)?;
    let old_name = RefName::new(old);
    let new_name = RefName::new(new);
    let target = repo.view().get_local_bookmark(old_name).clone();
    if target.is_absent() {
        bail!("no such bookmark: {old}");
    }
    if old == new {
        return Ok(());
    }
    if repo.view().get_local_bookmark(new_name).is_present() {
        bail!("bookmark already exists: {new}");
    }
    let tracked_remotes: Vec<_> = repo
        .view()
        .all_remote_bookmarks()
        .filter(|(symbol, remote_ref)| {
            symbol.name == old_name
                && symbol.remote != REMOTE_NAME_FOR_LOCAL_GIT_REPO
                && remote_ref.is_tracked()
        })
        .map(|(symbol, _)| symbol.remote.to_owned())
        .collect();
    repo.set_local_bookmark_target(new_name, target);
    repo.set_local_bookmark_target(old_name, RefTarget::absent());
    for remote in tracked_remotes {
        let symbol = new_name.to_remote_symbol(&remote);
        let existing = repo.view().get_remote_bookmark(symbol);
        if existing.is_present() && !existing.is_tracked() {
            continue;
        }
        repo.track_remote_bookmark(symbol)
            .await
            .with_context(|| format!("failed to track {new}@{}", remote.as_str()))?;
    }
    Ok(())
}

/// `jj bookmark forget <name>`: drop the local bookmark and stop tracking its
/// remotes, so nothing is ever pushed for it (unlike delete, whose deletion a
/// later push propagates).
pub(super) fn forget_bookmark(repo: &mut MutableRepo, name: &str) -> Result<()> {
    let ref_name = RefName::new(name);
    let remotes: Vec<_> = repo
        .view()
        .all_remote_bookmarks()
        .filter(|(symbol, _)| symbol.name == ref_name)
        .map(|(symbol, _)| symbol.remote.to_owned())
        .collect();
    if repo.view().get_local_bookmark(ref_name).is_absent() && remotes.is_empty() {
        bail!("no such bookmark: {name}");
    }
    repo.set_local_bookmark_target(ref_name, RefTarget::absent());
    for remote in remotes {
        // Git-tracking remote bookmarks can't be untracked.
        if remote == *REMOTE_NAME_FOR_LOCAL_GIT_REPO {
            continue;
        }
        repo.untrack_remote_bookmark(ref_name.to_remote_symbol(&remote));
    }
    Ok(())
}

/// `jj bookmark untrack <name>@<remote>`.
pub(super) fn untrack_bookmark(repo: &mut MutableRepo, name: &str, remote: &str) -> Result<()> {
    let remote_name = RemoteName::new(remote);
    if remote_name == REMOTE_NAME_FOR_LOCAL_GIT_REPO {
        bail!("git-tracking bookmarks can't be untracked");
    }
    let symbol = RefName::new(name).to_remote_symbol(remote_name);
    if !repo.view().get_remote_bookmark(symbol).is_tracked() {
        bail!("{name}@{remote} isn't tracked");
    }
    repo.untrack_remote_bookmark(symbol);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_names_push_and_twelve_change_id_chars() {
        let name = eval_push_bookmark_template(
            DEFAULT_PUSH_BOOKMARK_TEMPLATE,
            "zyxwvutsrqponmlkzyxw",
            "0123456789abcdef",
        )
        .unwrap();
        assert_eq!(name, "push-zyxwvutsrqpo");
    }

    #[test]
    fn template_supports_prefixes_lengths_and_commit_ids() {
        let name = eval_push_bookmark_template(
            r#"'me/' ++ change_id.short(8) ++ "-" ++ commit_id.short(4)"#,
            "zyxwvutsrqponmlk",
            "0123456789abcdef",
        )
        .unwrap();
        assert_eq!(name, "me/zyxwvuts-0123");
    }

    #[test]
    fn template_refuses_what_it_cannot_evaluate() {
        for template in [
            r#""push-" ++ change_id.shortest()"#,
            r#"description.first_line()"#,
            r#""a" "b""#,
            r#""unterminated"#,
        ] {
            assert!(
                eval_push_bookmark_template(template, "zyxw", "0123").is_err(),
                "{template} should be refused"
            );
        }
    }

    #[test]
    fn bookmark_names_follow_jj_and_git_rules() {
        for good in ["main", "feature/x-1", "push-abc", "v1.2"] {
            assert!(validate_bookmark_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "has space",
            "a..b",
            "x.lock",
            "-lead",
            "trail/",
            "@",
            "a@b",
            "q?",
            "a:b",
        ] {
            assert!(validate_bookmark_name(bad).is_err(), "{bad:?}");
        }
    }
}
