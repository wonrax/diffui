use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use jj_lib::{
    backend::{CommitId, TreeId},
    config::{ConfigLayer, ConfigSource},
    fileset::FilesetAliasesMap,
    git::REMOTE_NAME_FOR_LOCAL_GIT_REPO,
    graph::TopoGroupedGraphIterator,
    merge::Merge,
    object_id::ObjectId,
    op_store::LocalRemoteRefTarget,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo, StoreFactories},
    repo_path::RepoPathUiConverter,
    revset::{
        RevsetAliasesMap, RevsetDiagnostics, RevsetExpression, RevsetExtensions,
        RevsetParseContext, RevsetWorkspaceContext, SymbolResolver, UserRevsetExpression,
        parse as parse_revset,
    },
    settings::UserSettings,
    workspace::{Workspace, default_working_copy_factories},
};

use super::settings::*;
use crate::graph::LaneAssigner;
use crate::model::{
    BookmarkEntry, BookmarksInfo, BranchStatus, CommitSummary, LoadProgress, RemoteBookmarkRef,
    StreamRow,
};
use crate::repo::CancelFlag;

/// jj's everyday revset aliases — `trunk()`, `immutable_heads()`, `mutable()`,
/// `immutable()`, … — are *not* jj-lib builtins. They ship in jj-**cli**'s
/// embedded config, which we don't depend on, so a jj-lib-only client starts
/// with an empty alias map and any revset mentioning them fails to parse
/// ("Function `trunk` doesn't exist"). This is a verbatim copy of jj's
/// `cli/src/config/revsets.toml` `[revset-aliases]` table; keep it in sync when
/// bumping jj-lib. The alias *bodies* only reference real jj-lib builtins
/// (`remote_bookmarks`, `tags`, `untracked_remote_bookmarks`, `visible_heads`,
/// `root`), so they resolve once seeded.
pub(super) const DEFAULT_REVSET_ALIASES: &str = r#"
[revset-aliases]
# trunk() can be overridden as '<bookmark>@<remote>'.
'trunk()' = '''
latest(
  remote_bookmarks(exact:"main", exact:"origin") |
  remote_bookmarks(exact:"master", exact:"origin") |
  remote_bookmarks(exact:"trunk", exact:"origin") |
  remote_bookmarks(exact:"main", exact:"upstream") |
  remote_bookmarks(exact:"master", exact:"upstream") |
  remote_bookmarks(exact:"trunk", exact:"upstream") |
  root()
)
'''
'builtin_immutable_heads()' = 'trunk() | tags() | untracked_remote_bookmarks()'
'immutable_heads()' = 'builtin_immutable_heads()'
'immutable()' = '::(immutable_heads() | root())'
'mutable()' = '~immutable()'
'visible()' = '::visible_heads()'
'hidden()' = '~visible()'
"#;

/// Build the revset alias map jj itself would use: our embedded copy of jj's
/// default aliases at the bottom, then the user's configured `[revset-aliases]`
/// layered on top so a user-defined `trunk()`/`immutable_heads()` overrides
/// ours. This mirrors jj-cli's `load_aliases_map` — later (higher-precedence)
/// config layers win, and a malformed individual alias is logged and skipped
/// rather than breaking every revset parse.
pub(super) fn revset_aliases_map(settings: &UserSettings) -> Result<RevsetAliasesMap> {
    let defaults = ConfigLayer::parse(ConfigSource::Default, DEFAULT_REVSET_ALIASES)
        .context("failed to parse builtin revset aliases")?;
    let mut map = RevsetAliasesMap::new();
    let layers = std::iter::once(&defaults).chain(settings.config().layers().iter().map(|l| &**l));
    for layer in layers {
        let table = match layer.look_up_table(["revset-aliases"]) {
            Ok(Some(table)) => table,
            // Absent, or present but not a table (malformed config): skip.
            Ok(None) | Err(_) => continue,
        };
        for (decl, item) in table.iter() {
            let Some(defn) = item.as_str() else {
                eprintln!("diffui: ignoring revset-alias `{decl}`: expected a string value");
                continue;
            };
            if let Err(err) = map.insert(decl, defn) {
                eprintln!("diffui: ignoring invalid revset-alias `{decl}`: {err}");
            }
        }
    }
    Ok(map)
}

/// jj-cli's default `revsets.log` — the revset `jj log` displays. Like the
/// alias defaults above it ships in jj-**cli**'s embedded config rather than
/// jj-lib, so we re-embed it as the fallback when the user hasn't set their own.
pub(super) const DEFAULT_LOG_REVSET: &str =
    "present(@) | ancestors(immutable_heads().., 2) | trunk()";

/// The revset diffui opens a jj repo with when the user has no per-repo revset
/// saved: their configured `revsets.log` if set, else jj's default (so the
/// initial view matches `jj log`). Falls back to the default if the repo's
/// settings can't be loaded.
pub fn jj_log_revset(repo_root: &Path) -> String {
    jj_settings(repo_root)
        .ok()
        .and_then(|settings| settings.get_string("revsets.log").ok())
        .unwrap_or_else(|| DEFAULT_LOG_REVSET.to_owned())
}

/// Parse a user-entered revset string into a jj expression for the graph
/// loaders. Empty or `all()` short-circuits to [`RevsetExpression::all`] (the
/// app default — and avoids any parse risk on the common path). Symbols like
/// `@`, `mine()`, `conflicts()` resolve against `workspace_name` (the loaded
/// workspace — so `@` is *this* workspace's working copy, not the default
/// one's); a parse error is surfaced so the revset activity can report it.
/// `default_ignored_remote` is jj's colocated-git pseudo-remote, matching jj's
/// own parsing.
pub(super) fn parse_user_revset(
    repo_root: &Path,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    src: &str,
) -> Result<Arc<UserRevsetExpression>> {
    let trimmed = src.trim();
    if trimmed.is_empty() || trimmed == "all()" {
        return Ok(RevsetExpression::all());
    }
    let aliases = revset_aliases_map(settings)?;
    let fileset_aliases = FilesetAliasesMap::new();
    let extensions = RevsetExtensions::default();
    let path_converter = RepoPathUiConverter::Fs {
        cwd: repo_root.to_path_buf(),
        base: repo_root.to_path_buf(),
    };
    let workspace_ctx = RevsetWorkspaceContext {
        path_converter: &path_converter,
        workspace_name,
    };
    let context = RevsetParseContext {
        aliases_map: &aliases,
        local_variables: HashMap::new(),
        user_email: settings.user_email(),
        date_pattern_context: chrono::Local::now().into(),
        default_ignored_remote: Some(REMOTE_NAME_FOR_LOCAL_GIT_REPO),
        fileset_aliases_map: &fileset_aliases,
        use_glob_by_default: true,
        extensions: &extensions,
        workspace: Some(workspace_ctx),
    };
    parse_revset(&mut RevsetDiagnostics::new(), trimmed, &context).context("failed to parse revset")
}

/// Walk the jj revset graph, emitting commits in batches as they're built, and
/// return the single-parent emptiness updates once every tree-id is known.
///
/// Unlike a collect-then-loop, this pulls the topo iterator *lazily* and ships
/// each `batch_size` chunk through `emit` the moment it fills — so a streaming
/// consumer can paint the first screen after the first batch instead of waiting
/// for the whole (up to ~1M-row) history: the actor's `emit` ships each batch
/// as a `Batch` event, and the projection folds it into the growing store.
///
/// Each jj `Commit` is dropped as soon as its data is extracted rather than
/// holding all of them at once (~400MB on a million-commit repo). Single-parent
/// emptiness needs a parent's tree-id, which in descendants-first order hasn't
/// been loaded yet, so we keep the tree-id map + each commit's lone parent and
/// resolve it in a final pass; merges/roots stay unknown and are resolved off
/// the load path (see `compute_jj_empty_status`).
///
/// This is the standalone entry point, opening its own workspace: the memory
/// profile and the revset tests use it. The actor drives
/// [`walk_jj_with_repo`] against the repo it already holds.
pub async fn walk_jj_commits(
    repository_root: PathBuf,
    revset: String,
    progress: LoadProgress,
    batch_size: usize,
    emit: &mut dyn FnMut(Vec<StreamRow>),
) -> Result<(Vec<(usize, bool)>, Option<BranchStatus>, BookmarksInfo)> {
    // Load the *user's* jj config (not just defaults) so config-dependent
    // revsets resolve correctly — `mine()` lowers to `author(your-email)`, which
    // is empty unless `user.email` is read from the user/repo config. The cold
    // load already does this via `jj_settings`; this is the refresh path.
    let settings = jj_settings(&repository_root)?;
    let workspace = Workspace::load(
        &settings,
        &repository_root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name();
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    walk_jj_with_repo(
        repo.as_ref(),
        WorkspaceView {
            wc_commit_id: &wc_commit_id,
            workspace_name,
        },
        &repository_root,
        &revset,
        progress,
        batch_size,
        &CancelFlag::default(),
        emit,
    )
    .await
    .map(|walked| walked.expect("an uncancellable walk always completes"))
}

/// The workspace-scoped identity a graph walk renders relative to: which
/// commit is `@`, and which workspace it belongs to — for revset resolution
/// (`@` must be *this* workspace's working copy) and for labeling the other
/// workspaces' working copies as `name@` chips.
pub struct WorkspaceView<'a> {
    pub wc_commit_id: &'a CommitId,
    pub workspace_name: &'a WorkspaceName,
}

/// The graph-walk half of [`walk_jj_commits`], given an already-loaded repo and
/// its working-copy commit id. Split out so the actor walks the repo it is
/// already holding instead of reading the (large) commit index again.
#[allow(clippy::too_many_arguments)]
pub async fn walk_jj_with_repo(
    repo: &ReadonlyRepo,
    workspace: WorkspaceView<'_>,
    repo_root: &Path,
    revset: &str,
    progress: LoadProgress,
    batch_size: usize,
    cancel: &CancelFlag,
    emit: &mut dyn FnMut(Vec<StreamRow>),
) -> Result<Option<(Vec<(usize, bool)>, Option<BranchStatus>, BookmarksInfo)>> {
    let WorkspaceView {
        wc_commit_id,
        workspace_name,
    } = workspace;
    // The user's revset controls which revisions load. The default (`all()`)
    // covers the working copy, every local bookmark, and tracked/untracked
    // remote bookmarks, so unmerged branches still appear in the graph.
    let expr = parse_user_revset(repo_root, repo.settings(), workspace_name, revset)?;
    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let resolved = expr
        .resolve_user_expression(repo, &symbol_resolver)
        .map_err(describe_resolution_error)
        .context("failed to resolve jj revset")?;
    let revset = resolved
        .evaluate(repo)
        .context("failed to evaluate jj revset")?;

    // Determinate progress: count the revset's commits up front so the toolbar
    // shows N / total during a big cold load. This is a position-only walk (no
    // commit/tree loading) — ~20ms for 43k commits, so a fraction of the
    // streaming load it precedes; best-effort, we stay indeterminate on error.
    if let Ok((lower, upper)) = revset.count_estimate() {
        progress.set_total(upper.unwrap_or(lower));
    }

    // Index ref labels by commit id once so the per-commit loop below is a
    // map lookup instead of an O(refs) scan per revision. Other workspaces'
    // working copies render as `name@` chips (jj log's `working_copies`
    // keyword) — `name@` is also valid revset syntax, so the chip doubles as
    // a palette-jumpable symbol. Our own workspace's `@` keeps the dedicated
    // working-copy marker instead of a chip. Workspace labels go first,
    // matching jj log's template order (working_copies before bookmarks), so
    // the chip rail's tail-dropping `+N` overflow sheds bookmarks before it
    // sheds a working-copy marker.
    let mut bookmarks_by_commit: HashMap<CommitId, Vec<String>> = HashMap::new();
    for (name, id) in repo.view().wc_commit_ids() {
        if name.as_str() != workspace_name.as_str() {
            bookmarks_by_commit
                .entry(id.clone())
                .or_default()
                .push(format!("{}@", name.as_str()));
        }
    }
    for (name, target) in repo.view().bookmarks() {
        collect_bookmark_labels(name.as_str(), &target, |id, label| {
            bookmarks_by_commit
                .entry(id.clone())
                .or_default()
                .push(label);
        });
    }

    // Membership test for `immutable()`, resolved through the same alias map
    // as the log revset so a user override of `immutable_heads()` is honored.
    // Flags each row for the UI's rewrite-confirmation dialogs. Best-effort:
    // on any failure every row reads as mutable — the mutation path still
    // guards for real (`ensure_rewritable`), this only degrades the warning
    // from up-front to after-the-fact.
    let immutable_revset =
        parse_user_revset(repo_root, repo.settings(), workspace_name, "immutable()")
            .and_then(|expr| {
                expr.resolve_user_expression(repo, &symbol_resolver)
                    .context("failed to resolve immutable()")
            })
            .and_then(|resolved| {
                resolved
                    .evaluate(repo)
                    .context("failed to evaluate immutable()")
            });
    if let Err(error) = &immutable_revset {
        eprintln!("diffui: immutable() unavailable for log flags: {error:#}");
    }
    let is_immutable_fn = immutable_revset.as_ref().ok().map(|rs| rs.containing_fn());

    let mut lane_assigner = LaneAssigner::new();
    let mut tree_ids: HashMap<CommitId, Merge<TreeId>> = HashMap::new();
    let mut ids: Vec<CommitId> = Vec::new();
    let mut single_parents: Vec<Option<CommitId>> = Vec::new();
    let mut batch: Vec<StreamRow> = Vec::with_capacity(batch_size);

    // Scope the topo iterator so its borrow of `revset` ends before the empty
    // pass; the iterator is pulled lazily (no up-front `collect`), so the first
    // batch ships after only `batch_size` commits are walked.
    {
        let mut topo = TopoGroupedGraphIterator::new(revset.iter_graph(), |id: &CommitId| id);
        // Prioritize `@` only when the revset actually contains it. A revset that
        // excludes the working copy (e.g. `mine()` when `@` isn't yours,
        // `conflicts()`, a narrow range) has no such node, and the topo iterator
        // panics ("parent or prioritized node should exist") on a prioritized
        // node missing from its input. jj-cli guards `jj log` the same way.
        let has_commit = revset.containing_fn();
        if has_commit(wc_commit_id).unwrap_or(false) {
            topo.prioritize_branch(wc_commit_id.clone());
        }
        for node in topo {
            // Advance the lane state for every node in topo order. The assigner
            // is stateful, so this must run once per node — keep it first.
            let (id, edges) = node.context("failed to walk jj revset graph")?;
            let frame = lane_assigner.push(&id, &edges);
            let commit = repo
                .store()
                .get_commit_async(&id)
                .await
                .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
            tree_ids.insert(id.clone(), commit.tree_ids().clone());
            single_parents.push(match commit.parent_ids() {
                [parent] => Some(parent.clone()),
                _ => None,
            });

            let description = commit.description().lines().next().unwrap_or("").trim();
            let shortest_change_id_len = repo
                .shortest_unique_change_id_prefix_len(commit.change_id())
                .with_context(|| {
                    format!(
                        "failed to resolve shortest unique jj change id for {}",
                        commit.change_id().hex()
                    )
                })?;
            let bookmarks = bookmarks_by_commit.get(&id).cloned().unwrap_or_default();
            // Divergent = the change id maps to more than one visible commit;
            // hidden = this commit isn't among them (rewritten, but still in
            // the walk because a ref — e.g. a stale remote bookmark — pins it
            // into the revset). Either way jj log suffixes the change id with
            // the copy's offset (`xyz/1`), which revsets accept to address
            // one copy — record it so the sidebar can render the same suffix.
            // Resolved against the repo's change-id index — the same one the
            // shortest-prefix call above already built — so this is a lookup
            // per row, not a scan. Best-effort: an index error just reads as
            // a plain visible commit.
            let (is_divergent, is_hidden, change_offset) = repo
                .resolve_change_id(commit.change_id())
                .ok()
                .flatten()
                .map(|targets| {
                    let divergent = targets.is_divergent();
                    let hidden = !targets.has_visible(&id);
                    let offset = (divergent || hidden)
                        .then(|| targets.find_offset(&id))
                        .flatten();
                    (divergent, hidden, offset)
                })
                .unwrap_or((false, false, None));
            let summary = CommitSummary {
                change_id: commit.change_id().to_string(),
                commit_id: id.hex(),
                shortest_change_id_len: Some(shortest_change_id_len),
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                author: commit.author().name.clone(),
                has_description: !description.is_empty(),
                is_empty: None,
                has_conflict: commit.has_conflict(),
                is_divergent,
                is_hidden,
                change_offset,
                is_working_copy: id == *wc_commit_id,
                is_immutable: is_immutable_fn
                    .as_ref()
                    .is_some_and(|contains| contains(&id).unwrap_or(false)),
                bookmarks,
                parent_ids: commit.parent_ids().iter().map(|id| id.hex()).collect(),
            };
            ids.push(id);
            batch.push(StreamRow { summary, frame });
            progress.increment();
            if batch.len() >= batch_size {
                // One flag check per batch, not per row: a superseded walk on a
                // million-commit repo used to run to completion in the
                // background, competing for the same disk as the walk that
                // replaced it.
                if cancel.is_cancelled() {
                    return Ok(None);
                }
                emit(std::mem::take(&mut batch));
                batch.reserve(batch_size);
                // Hand the runtime back between batches. The actor reads its
                // command channel on a sibling future, and a warm commit cache
                // can carry this loop a long way without ever awaiting, so
                // without this a cancel could wait out the whole walk.
                tokio::task::yield_now().await;
            }
            // `commit` dropped here — we never hold more than one at a time.
        }
    }
    drop(revset);
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if !batch.is_empty() {
        emit(batch);
    }

    // Resolve single-parent emptiness now that every tree-id is known: a commit
    // is empty iff its tree matches its lone parent's (a cheap id compare).
    let mut empty_updates = Vec::new();
    for (index, parent) in single_parents.iter().enumerate() {
        let Some(parent) = parent else {
            continue;
        };
        if let (Some(own), Some(parent_tree)) = (tree_ids.get(&ids[index]), tree_ids.get(parent)) {
            empty_updates.push((index, own == parent_tree));
        }
    }

    // Branch summary for the sidebar footer — reuses the already-loaded repo so
    // it costs a few small revset evals, not another index load.
    let branch_status = compute_branch_status(repo, wc_commit_id);
    // Repo-wide bookmark table for the revision context menu (move/track/
    // delete/push) — a single bookmarks() walk on the same repo.
    let bookmarks = compute_bookmarks_info(repo, wc_commit_id);
    Ok(Some((empty_updates, branch_status, bookmarks)))
}

/// Emit the bookmark chip label(s) for one bookmark onto the commit(s) they sit
/// on, following jj's `bookmarks` template semantics:
/// - the local bookmark renders as `name`, or `name*` when it diverges from any
///   of its tracked remotes (i.e. there are unpushed/unpulled changes);
/// - a *conflicted* ref (multiple targets after concurrent moves / a
///   force-pushed remote) renders as `name??` on every side, taking
///   precedence over `*`;
/// - a tracked remote pointing at the same commit as the local bookmark is
///   redundant and dropped, while a diverged or untracked remote renders as
///   `name@remote`;
/// - jj's colocated-git pseudo-remote (`name@git`) is never shown — it just
///   mirrors the local bookmark and is an implementation detail, so it also
///   never contributes to the `*` divergence check.
///
/// `emit` receives `(commit_id, label)` per chip, so the per-commit graph index
/// and the single-revision diff header can share one rule.
pub(super) fn collect_bookmark_labels(
    name: &str,
    target: &LocalRemoteRefTarget<'_>,
    mut emit: impl FnMut(&CommitId, String),
) {
    let local_id = target.local_target.added_ids().next();
    // Conflicted (several added ids — concurrent moves, a force-pushed
    // remote): jj log suffixes `??`, and the chip lands on *every* side. The
    // conflict marker wins over the divergence `*` — jj renders it the same
    // way, and "this name means two commits" is the more urgent fact.
    let local_conflicted = target.local_target.added_ids().nth(1).is_some();
    let diverged = target.remote_refs.iter().any(|(remote, remote_ref)| {
        remote.as_str() != REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str()
            && remote_ref.is_tracked()
            && remote_ref.target.added_ids().next() != local_id
    });
    let local_label = if local_conflicted {
        format!("{name}??")
    } else if diverged {
        format!("{name}*")
    } else {
        name.to_owned()
    };
    for id in target.local_target.added_ids() {
        emit(id, local_label.clone());
    }
    for (remote, remote_ref) in &target.remote_refs {
        if remote.as_str() == REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str() {
            continue;
        }
        let tracked = remote_ref.is_tracked();
        // A conflicted remote ref (concurrent fetches) gets the same `??`.
        let remote_conflicted = remote_ref.target.added_ids().nth(1).is_some();
        for id in remote_ref.target.added_ids() {
            // A tracked remote in sync with the local bookmark is redundant.
            if tracked && !local_conflicted && !remote_conflicted && Some(id) == local_id {
                continue;
            }
            let suffix = if remote_conflicted { "??" } else { "" };
            emit(id, format!("{}@{}{}", name, remote.as_str(), suffix));
        }
    }
}

/// jj CLI parity for symbol-resolution dead ends: the bare jj-lib messages
/// ("Name `x` is conflicted") say what's wrong but not what to type instead.
/// The two ambiguity errors get their escape hatches appended — the same
/// ways forward the CLI prints as hints.
pub(super) fn describe_resolution_error(
    error: jj_lib::revset::RevsetResolutionError,
) -> anyhow::Error {
    use jj_lib::revset::RevsetResolutionError as E;
    let short = |id: &CommitId| id.hex().chars().take(12).collect::<String>();
    match &error {
        E::ConflictedRef {
            kind: "bookmark",
            symbol,
            targets,
        } => {
            let sides: Vec<String> = targets.iter().map(short).collect();
            anyhow::anyhow!(
                "{error} — it points at {}; select every side with \
                 bookmarks(exact:\"{symbol}\"), pick one by commit id, or move \
                 the bookmark onto a revision to resolve the conflict",
                sides.join(", ")
            )
        }
        E::ConflictedRef { targets, .. } => {
            let sides: Vec<String> = targets.iter().map(short).collect();
            anyhow::anyhow!(
                "{error} — it points at {}; pick one side by commit id",
                sides.join(", ")
            )
        }
        E::DivergentChangeId {
            symbol,
            visible_targets,
        } => {
            let copies: Vec<String> = visible_targets
                .iter()
                .map(|(offset, _)| format!("{symbol}/{offset}"))
                .collect();
            anyhow::anyhow!(
                "{error} — address one copy as {} (the sidebar shows each \
                 row's /N suffix)",
                copies.join(", ")
            )
        }
        _ => anyhow::Error::new(error),
    }
}

/// Snapshot every bookmark in the repo with the state the revision context menu
/// needs: each bookmark's local target commit, and each remote ref's target +
/// tracking state. `@`'s commit id is recorded so a working-copy right-click can
/// resolve the bookmarks sitting on it.
pub(super) fn compute_bookmarks_info(
    repo: &ReadonlyRepo,
    wc_commit_id: &CommitId,
) -> BookmarksInfo {
    let mut bookmarks = Vec::new();
    for (name, target) in repo.view().bookmarks() {
        // Every added id, not just the first: a conflicted bookmark carries
        // all of its sides so the menu can flag it, match any side's row,
        // and withhold the push actions jj would refuse.
        let local_targets: Vec<String> =
            target.local_target.added_ids().map(|id| id.hex()).collect();
        let mut remotes = Vec::new();
        for (remote, remote_ref) in &target.remote_refs {
            // Skip jj's colocated-git pseudo-remote ("git"): it mirrors the
            // local Git repo's branches, isn't a real push/track target, and
            // jj rejects pushing to it ("reserved for local Git repository").
            if remote.as_str() == REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str() {
                continue;
            }
            if let Some(id) = remote_ref.target.added_ids().next() {
                remotes.push(RemoteBookmarkRef {
                    remote: remote.as_str().to_owned(),
                    target: id.hex(),
                    tracked: remote_ref.is_tracked(),
                });
            }
        }
        if local_targets.is_empty() && remotes.is_empty() {
            continue;
        }
        bookmarks.push(BookmarkEntry {
            name: name.as_str().to_owned(),
            local_targets,
            remotes,
        });
    }
    bookmarks.sort_by(|a, b| a.name.cmp(&b.name));
    BookmarksInfo {
        bookmarks,
        working_copy_commit: Some(wc_commit_id.hex()),
    }
}

/// Compute the working-copy's branch summary: the nearest local bookmark at or
/// behind `@`, its tracked upstream, and `@`'s ahead/behind counts vs that
/// upstream. Best-effort — any failure (or no local bookmark in `@`'s
/// ancestry) yields `None`, so the footer falls back to the change count.
pub(super) fn compute_branch_status(
    repo: &ReadonlyRepo,
    wc_commit_id: &CommitId,
) -> Option<BranchStatus> {
    match branch_status_inner(repo, wc_commit_id) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("diffui: failed to compute branch status: {error:#}");
            None
        }
    }
}

pub(super) fn branch_status_inner(
    repo: &ReadonlyRepo,
    wc_commit_id: &CommitId,
) -> Result<Option<BranchStatus>> {
    let view = repo.view();

    // Local bookmark targets, and — per local-bookmark name — its tracked
    // remote (display + target id).
    let mut local_targets: Vec<CommitId> = Vec::new();
    let mut local_by_commit: HashMap<CommitId, Vec<String>> = HashMap::new();
    let mut tracked_upstream: HashMap<String, (String, CommitId)> = HashMap::new();
    for (name, target) in view.bookmarks() {
        let name_str = name.as_str().to_owned();
        for id in target.local_target.added_ids() {
            local_targets.push(id.clone());
            local_by_commit
                .entry(id.clone())
                .or_default()
                .push(name_str.clone());
        }
        for (remote, remote_ref) in &target.remote_refs {
            // Skip jj's colocated-git pseudo-remote — it mirrors the local Git
            // branches, so treating it as the upstream would always read as
            // "in sync" instead of comparing against the real remote.
            if remote.as_str() == REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str() {
                continue;
            }
            if remote_ref.is_tracked()
                && let Some(id) = remote_ref.target.added_ids().next()
            {
                tracked_upstream
                    .entry(name_str.clone())
                    .or_insert_with(|| (format!("{name_str}@{}", remote.as_str()), id.clone()));
            }
        }
    }
    if local_targets.is_empty() {
        return Ok(None);
    }

    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );

    // Nearest local bookmark at/behind `@` = (bookmark commits) ∩ ancestors(@),
    // taking the first in topo order (children before parents → closest to `@`).
    let nearest_expr = RevsetExpression::commits(local_targets)
        .intersection(&RevsetExpression::commit(wc_commit_id.clone()).ancestors());
    let nearest = {
        let resolved = nearest_expr
            .resolve_user_expression(repo, &symbol_resolver)
            .context("failed to resolve nearest-bookmark revset")?;
        let revset = resolved
            .evaluate(repo)
            .context("failed to evaluate nearest-bookmark revset")?;
        match revset.iter().next() {
            Some(result) => result.context("failed to read nearest bookmark commit")?,
            None => return Ok(None),
        }
    };

    let names = local_by_commit.get(&nearest).cloned().unwrap_or_default();
    // Prefer a name that tracks a remote so ahead/behind is meaningful;
    // otherwise fall back to the first bookmark on the commit.
    let branch = match names
        .iter()
        .find(|n| tracked_upstream.contains_key(*n))
        .or_else(|| names.first())
        .cloned()
    {
        Some(branch) => branch,
        None => return Ok(None),
    };

    let Some((upstream_display, remote_id)) = tracked_upstream.get(&branch).cloned() else {
        // Bookmark with no tracking remote — show the name only.
        return Ok(Some(BranchStatus {
            branch,
            upstream: None,
            ahead: 0,
            behind: 0,
        }));
    };

    let at = RevsetExpression::commit(wc_commit_id.clone());
    let remote = RevsetExpression::commit(remote_id);
    // ahead = remote..@ (reachable from @, not the remote); behind = @..remote.
    let ahead = count_revset(repo, &symbol_resolver, &remote.range(&at))
        .context("failed to count ahead commits")?;
    let behind = count_revset(repo, &symbol_resolver, &at.range(&remote))
        .context("failed to count behind commits")?;

    Ok(Some(BranchStatus {
        branch,
        upstream: Some(upstream_display),
        ahead,
        behind,
    }))
}

/// Evaluate `expr` and count the commits it yields.
pub(super) fn count_revset(
    repo: &ReadonlyRepo,
    symbol_resolver: &SymbolResolver,
    expr: &Arc<UserRevsetExpression>,
) -> Result<usize> {
    let resolved = expr
        .resolve_user_expression(repo, symbol_resolver)
        .context("failed to resolve count revset")?;
    let revset = resolved
        .evaluate(repo)
        .context("failed to evaluate count revset")?;
    let mut count = 0usize;
    for result in revset.iter() {
        result.context("failed to read commit while counting revset")?;
        count += 1;
    }
    Ok(count)
}

/// Resolve the empty status of specific commits (the merges/roots the walk
/// left unknown) off the load path. `targets` carries the caller's row index
/// alongside the hex commit-id so results can be applied back without a second
/// lookup. Per-commit failures are skipped rather than failing the whole batch.
///
/// Each resolution is a parent-tree merge — milliseconds each, and a repo with
/// hundreds of thousands of merges would spend minutes here — so the flag is
/// polled per chunk and a superseded sweep stops where it is.
pub(crate) async fn compute_jj_empty_status(
    repo: &ReadonlyRepo,
    targets: Vec<(usize, String)>,
    cancel: &CancelFlag,
) -> Option<Vec<(usize, bool)>> {
    /// Commits between flag checks. Small enough that a cancel lands promptly,
    /// large enough that the check itself costs nothing.
    const CANCEL_CHECK_CHUNK: usize = 64;

    let mut out = Vec::with_capacity(targets.len());
    for (chunk_index, (index, commit_id_hex)) in targets.into_iter().enumerate() {
        if chunk_index.is_multiple_of(CANCEL_CHECK_CHUNK) {
            if cancel.is_cancelled() {
                return None;
            }
            // Same reason as the walk: give the actor's reader a turn.
            tokio::task::yield_now().await;
        }
        let Some(id) = CommitId::try_from_hex(&commit_id_hex) else {
            continue;
        };
        let Ok(commit) = repo.store().get_commit_async(&id).await else {
            continue;
        };
        if let Ok(empty) = commit.is_empty(repo).await {
            out.push((index, empty));
        }
    }
    Some(out)
}

#[cfg(test)]
mod revset_tests {
    use jj_lib::config::StackedConfig;

    use super::*;

    fn settings() -> UserSettings {
        UserSettings::from_config(StackedConfig::with_defaults()).expect("default settings")
    }

    #[test]
    fn empty_and_all_are_accepted() {
        let s = settings();
        let root = Path::new("/tmp");
        let ws = WorkspaceName::DEFAULT;
        assert!(parse_user_revset(root, &s, ws, "").is_ok());
        assert!(parse_user_revset(root, &s, ws, "   ").is_ok());
        assert!(parse_user_revset(root, &s, ws, "all()").is_ok());
        assert!(parse_user_revset(root, &s, ws, "  all()  ").is_ok());
    }

    #[test]
    fn built_in_functions_and_working_copy_parse() {
        let s = settings();
        let root = Path::new("/tmp");
        let ws = WorkspaceName::DEFAULT;
        // `@` needs the workspace context; the preset functions are built-ins.
        assert!(parse_user_revset(root, &s, ws, "@").is_ok());
        assert!(parse_user_revset(root, &s, ws, "ancestors(@)").is_ok());
        assert!(parse_user_revset(root, &s, ws, "mine()").is_ok());
        assert!(parse_user_revset(root, &s, ws, "conflicts()").is_ok());
    }

    #[test]
    fn malformed_revset_is_rejected() {
        let s = settings();
        let root = Path::new("/tmp");
        assert!(parse_user_revset(root, &s, WorkspaceName::DEFAULT, "(((").is_err());
    }

    /// Regression: a revset that excludes the working-copy commit must not panic
    /// the graph walk (`prioritize_branch` on a missing node). `none()` excludes
    /// everything, including `@`. Needs the diffui jj repo on disk, so it's
    /// `#[ignore]`d — run with `cargo test -- --ignored excluding_revset`.
    #[test]
    #[ignore = "needs the diffui jj repo on disk"]
    fn excluding_revset_loads_without_panicking() {
        use crate::model::LoadProgress;
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let mut rows = 0usize;
        let mut emit = |batch: Vec<StreamRow>| rows += batch.len();
        let result = runtime.block_on(walk_jj_commits(
            root,
            "none()".to_owned(),
            LoadProgress::default(),
            4096,
            &mut emit,
        ));
        result.expect("none() should walk, not panic");
        assert_eq!(rows, 0, "none() should yield an empty graph");
    }

    /// Regression: the refresh path must load the user's jj config, not just
    /// defaults — otherwise config-dependent revsets like `mine()` resolve
    /// against an empty `user.email` and return nothing. Proves `jj_settings`
    /// reads a real email distinct from the bare-defaults one.
    #[test]
    #[ignore = "needs the diffui jj repo + a configured user.email"]
    fn refresh_path_loads_user_email() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let default_email = UserSettings::from_config(StackedConfig::with_defaults())
            .expect("default settings")
            .user_email()
            .to_owned();
        let loaded_email = jj_settings(&root)
            .expect("jj settings")
            .user_email()
            .to_owned();
        assert!(!loaded_email.is_empty(), "user.email should be configured");
        assert_ne!(
            loaded_email, default_email,
            "jj_settings must load the user's email, not the bare default"
        );
    }

    /// The op-head reader the fs-watcher dedup relies on must return the current
    /// op id(s) — exactly the filenames under `.jj/repo/op_heads/heads`, which
    /// is what `RepositorySnapshot::fingerprint` (`op_id().hex()`) records, so
    /// the two compare directly. Read-only (no wc snapshot, no signing), so it's
    /// safe against the diffui repo.
    #[test]
    #[ignore = "needs the diffui jj repo on disk"]
    fn read_op_head_matches_op_heads_dir() {
        use crate::repository::{Repository, Vcs};
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repository = Repository {
            root: root.clone(),
            vcs: Vcs::Jj,
            scope: std::path::PathBuf::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let head = runtime
            .block_on(crate::jj::workspace::read_jj_op_head(repository))
            .expect("read op head");

        let on_disk: Vec<String> = std::fs::read_dir(root.join(".jj/repo/op_heads/heads"))
            .expect("read op_heads/heads")
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        for part in head.split(',') {
            assert_eq!(part.len(), 128, "each op id is a 128-char hex");
            assert!(
                on_disk.contains(&part.to_owned()),
                "read_jj_op_head part {part} must be a head on disk: {on_disk:?}"
            );
        }
    }
}

#[cfg(all(test, feature = "track-alloc"))]
mod lane_width_probe {
    use super::*;
    use crate::graph::LaneAssigner;

    // Settles "how wide is the graph really" by running the lane assigner
    // UNCAPPED over a repo's topology and histogramming the per-row lane count.
    // Walks the revset only (no `get_commit_async`), so it's fast and
    // memory-light (never stores the per-row fold), and it can't OOM. Run:
    //   DIFFUI_PROFILE_REPO=/path \
    //   cargo test --features track-alloc profile_lane_width -- --ignored --nocapture
    // Defaults to the nixpkgs clone.
    #[test]
    #[ignore]
    fn profile_lane_width() {
        let repo = std::env::var("DIFFUI_PROFILE_REPO").unwrap_or_else(|_| {
            format!("{}/code/nixpkgs", std::env::var("HOME").expect("HOME set"))
        });
        let root = std::path::PathBuf::from(&repo);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let widths: Vec<u32> = runtime.block_on(async {
            let settings =
                UserSettings::from_config(StackedConfig::with_defaults()).expect("jj settings");
            let workspace = Workspace::load(
                &settings,
                &root,
                &StoreFactories::default(),
                &default_working_copy_factories(),
            )
            .expect("jj workspace");
            let workspace_name = workspace.workspace_name();
            let repo = workspace
                .repo_loader()
                .load_at_head()
                .await
                .expect("jj repo");
            let wc = repo
                .view()
                .get_wc_commit_id(workspace_name)
                .expect("wc commit")
                .clone();
            let expr = RevsetExpression::all();
            let resolver = SymbolResolver::new(
                repo.as_ref(),
                &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
            );
            let resolved = expr
                .resolve_user_expression(repo.as_ref(), &resolver)
                .expect("resolve revset");
            let revset = resolved.evaluate(repo.as_ref()).expect("evaluate revset");
            let nodes: Vec<jj_lib::graph::GraphNode<CommitId>> = {
                let mut topo =
                    TopoGroupedGraphIterator::new(revset.iter_graph(), |id: &CommitId| id);
                topo.prioritize_branch(wc.clone());
                topo.collect::<Result<Vec<_>, _>>().expect("walk graph")
            };
            drop(revset);

            let mut assigner = LaneAssigner::uncapped();
            nodes
                .iter()
                .map(|(id, edges)| assigner.push(id, edges).lane_count() as u32)
                .collect()
        });

        let mut widths = widths;
        widths.sort_unstable();
        let n = widths.len().max(1);
        let pct = |p: f64| {
            widths
                .get(((n as f64 * p) as usize).min(n - 1))
                .copied()
                .unwrap_or(0)
        };
        let over = |t: u32| widths.iter().filter(|&&w| w > t).count();
        let mean = widths.iter().map(|&w| u64::from(w)).sum::<u64>() as f64 / n as f64;

        eprintln!("\n=== diffui lane-width profile (UNCAPPED) ===");
        eprintln!("repo : {repo}");
        eprintln!("rows : {n}");
        eprintln!("max  : {}", widths.last().copied().unwrap_or(0));
        eprintln!("mean : {mean:.1}");
        eprintln!("p50  : {}", pct(0.50));
        eprintln!("p90  : {}", pct(0.90));
        eprintln!("p99  : {}", pct(0.99));
        eprintln!("p999 : {}", pct(0.999));
        for threshold in [32u32, 64, 96, 128, 192, 256, 384, 512, 768, 1024] {
            let count = over(threshold);
            eprintln!(
                "rows > {threshold:>4} lanes : {count:>9}  ({:.3}%)",
                count as f64 / n as f64 * 100.0
            );
        }
        eprintln!("============================================\n");
    }
}

#[cfg(test)]
mod bookmark_label_tests {
    use super::*;
    use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
    use jj_lib::ref_name::RemoteName;

    fn cid(hex: &str) -> CommitId {
        CommitId::try_from_hex(hex).expect("valid hex commit id")
    }

    fn remote(target: &str, tracked: bool) -> RemoteRef {
        RemoteRef {
            target: RefTarget::normal(cid(target)),
            state: if tracked {
                RemoteRefState::Tracked
            } else {
                RemoteRefState::New
            },
        }
    }

    /// `(commit_hex, label)` chips `collect_bookmark_labels` emits for "main".
    fn labels(target: &LocalRemoteRefTarget<'_>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        collect_bookmark_labels("main", target, |id, label| out.push((id.hex(), label)));
        out
    }

    #[test]
    fn local_only_bookmark_has_no_asterisk() {
        let local = RefTarget::normal(cid("aa"));
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![],
        };
        assert_eq!(labels(&target), vec![("aa".into(), "main".into())]);
    }

    #[test]
    fn tracked_remote_in_sync_is_omitted() {
        let local = RefTarget::normal(cid("aa"));
        let origin = remote("aa", true);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &origin)],
        };
        // Just the local chip — no redundant `main@origin`, no `*`.
        assert_eq!(labels(&target), vec![("aa".into(), "main".into())]);
    }

    #[test]
    fn diverged_tracked_remote_adds_asterisk_and_chip() {
        let local = RefTarget::normal(cid("aa"));
        let origin = remote("bb", true);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &origin)],
        };
        assert_eq!(
            labels(&target),
            vec![
                ("aa".into(), "main*".into()),
                ("bb".into(), "main@origin".into()),
            ]
        );
    }

    #[test]
    fn untracked_remote_shows_chip_but_no_asterisk() {
        let local = RefTarget::normal(cid("aa"));
        let origin = remote("bb", false);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &origin)],
        };
        assert_eq!(
            labels(&target),
            vec![
                ("aa".into(), "main".into()),
                ("bb".into(), "main@origin".into()),
            ]
        );
    }

    #[test]
    fn git_pseudo_remote_is_hidden_and_excluded_from_asterisk() {
        // `@git` diverges from the local target, but it must neither render a
        // chip nor flip the local bookmark to `main*`; only the in-sync origin
        // matters, and it's redundant — so just `main` is shown.
        let local = RefTarget::normal(cid("aa"));
        let git = remote("bb", true);
        let origin = remote("aa", true);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            // jj yields remotes lexicographically: "git" before "origin".
            remote_refs: vec![
                (RemoteName::new("git"), &git),
                (RemoteName::new("origin"), &origin),
            ],
        };
        assert_eq!(labels(&target), vec![("aa".into(), "main".into())]);
    }

    #[test]
    fn conflicted_local_bookmark_marks_every_side() {
        // Two added ids = a conflicted bookmark (concurrent moves / a
        // force-pushed remote). Every side wears `main??`, the conflict
        // marker wins over the divergence `*`, and the tracked remote chip
        // stays visible even on a side it matches — during a conflict,
        // which side the remote is on is exactly the interesting fact.
        let local = RefTarget::from_legacy_form([], [cid("aa"), cid("bb")]);
        let origin = remote("aa", true);
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &origin)],
        };
        assert_eq!(
            labels(&target),
            vec![
                ("aa".into(), "main??".into()),
                ("bb".into(), "main??".into()),
                ("aa".into(), "main@origin".into()),
            ]
        );
    }

    #[test]
    fn conflicted_remote_ref_marks_its_sides() {
        let local = RefTarget::normal(cid("aa"));
        let origin = RemoteRef {
            target: RefTarget::from_legacy_form([], [cid("bb"), cid("cc")]),
            state: RemoteRefState::Tracked,
        };
        let target = LocalRemoteRefTarget {
            local_target: &local,
            remote_refs: vec![(RemoteName::new("origin"), &origin)],
        };
        assert_eq!(
            labels(&target),
            vec![
                // The local side diverges from the conflicted remote → `*`.
                ("aa".into(), "main*".into()),
                ("bb".into(), "main@origin??".into()),
                ("cc".into(), "main@origin??".into()),
            ]
        );
    }
}
