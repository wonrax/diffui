use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bstr::BStr;
use futures::StreamExt;
use jj_lib::{
    backend::{CommitId, TreeId},
    commit::Commit,
    config::{ConfigLayer, ConfigSource, StackedConfig},
    conflicts::{
        ConflictMarkerStyle, ConflictMaterializeOptions, materialize_tree_value,
        materialized_diff_stream,
    },
    copies::CopyRecords,
    diff_presentation::{
        LineCompareMode,
        unified::{DiffLineType, git_diff_part, unified_diff_hunks},
    },
    files::FileMergeHunkLevel,
    fileset::{
        FilesetAliasesMap, FilesetDiagnostics, FilesetExpression, FilesetParseContext,
        parse as parse_fileset,
    },
    git::{
        GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress, GitPushOptions,
        GitPushRefTargets, GitSettings, GitSidebandLineTerminator, GitSubprocessCallback,
        GitSubprocessOptions, REMOTE_NAME_FOR_LOCAL_GIT_REPO, expand_fetch_refspecs, export_refs,
        get_all_remote_names, push_refs,
    },
    gitignore::GitIgnoreFile,
    graph::TopoGroupedGraphIterator,
    matchers::{EverythingMatcher, Matcher, NothingMatcher, PrefixMatcher},
    merge::{Diff, Merge, SameChange},
    object_id::ObjectId,
    op_store::{LocalRemoteRefTarget, RefTarget, View},
    ref_name::{RefName, RefNameBuf, RemoteName, RemoteNameBuf, WorkspaceName},
    repo::{MutableRepo, ReadonlyRepo, Repo, RepoLoader, StoreFactories},
    repo_path::{RepoPath, RepoPathBuf, RepoPathUiConverter},
    revset::{
        RevsetAliasesMap, RevsetDiagnostics, RevsetExpression, RevsetExtensions,
        RevsetParseContext, RevsetWorkspaceContext, SymbolResolver, UserRevsetExpression,
        parse as parse_revset,
    },
    rewrite::merge_commit_trees,
    settings::{HumanByteSize, UserSettings},
    str_util::{StringExpression, StringPattern},
    tree_merge::MergeOptions,
    working_copy::SnapshotOptions,
    workspace::{Workspace, default_working_copy_factories},
};

use crate::FetchTarget;
use crate::diff_parse::format_hunk_header;
// (`crate::syntax` is no longer called on the load path — highlighting moved
// to the background; see `source::highlight_file`.)
use crate::graph::LaneAssigner;
use crate::graph_layout::{GraphLayout, LaneFoldState};
use crate::model::{
    BookmarkEntry, BookmarksInfo, BranchStatus, CommitStore, CommitSummary, DiffDocument, DiffFile,
    DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind, LoadProgress, RemoteBookmarkRef,
    RevisionDetails, RevisionSelection, SignatureInfo, StreamRow,
};
use crate::mutations::{MutationOp, MutationOutcome};
use crate::repository::{Repository, RepositorySnapshot};

// Fallback used only when the user has not configured `snapshot.max-new-file-size`.
// Matches jj-cli's shipped default (1 MiB).
const DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE: u64 = 1024 * 1024;

/// jj's everyday revset aliases — `trunk()`, `immutable_heads()`, `mutable()`,
/// `immutable()`, … — are *not* jj-lib builtins. They ship in jj-**cli**'s
/// embedded config, which we don't depend on, so a jj-lib-only client starts
/// with an empty alias map and any revset mentioning them fails to parse
/// ("Function `trunk` doesn't exist"). This is a verbatim copy of jj's
/// `cli/src/config/revsets.toml` `[revset-aliases]` table; keep it in sync when
/// bumping jj-lib. The alias *bodies* only reference real jj-lib builtins
/// (`remote_bookmarks`, `tags`, `untracked_remote_bookmarks`, `visible_heads`,
/// `root`), so they resolve once seeded.
const DEFAULT_REVSET_ALIASES: &str = r#"
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
fn revset_aliases_map(settings: &UserSettings) -> Result<RevsetAliasesMap> {
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
const DEFAULT_LOG_REVSET: &str = "present(@) | ancestors(immutable_heads().., 2) | trunk()";

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
/// `@`, `mine()`, `conflicts()` resolve against the default workspace; a parse
/// error is surfaced so the revset activity can report it. `default_ignored_remote`
/// is jj's colocated-git pseudo-remote, matching jj's own parsing.
fn parse_user_revset(
    repo_root: &Path,
    settings: &UserSettings,
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
        workspace_name: WorkspaceName::DEFAULT,
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
/// for the whole (up to ~1M-row) history. The batch [`load_jj_commits`] passes
/// an `emit` that accumulates into a store+graph; the UI's streaming loader
/// passes one that ships each batch as a `CommitsBatch` message. Both append
/// through the same `GraphLayout::push` / `CommitStore::push`, so they can't
/// diverge.
///
/// Each jj `Commit` is dropped as soon as its data is extracted rather than
/// holding all of them at once (~400MB on a million-commit repo). Single-parent
/// emptiness needs a parent's tree-id, which in descendants-first order hasn't
/// been loaded yet, so we keep the tree-id map + each commit's lone parent and
/// resolve it in a final pass; merges/roots stay unknown and are resolved off
/// the load path (see `compute_jj_empty_status`).
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
        &wc_commit_id,
        &repository_root,
        &revset,
        progress,
        batch_size,
        emit,
    )
    .await
}

/// The graph-walk half of [`walk_jj_commits`], given an already-loaded repo and
/// its working-copy commit id. Split out so the cold streaming load
/// ([`load_jj_cold`]) reuses the repo it loaded for the snapshot instead of
/// reading the (large) commit index a second time.
pub async fn walk_jj_with_repo(
    repo: &ReadonlyRepo,
    wc_commit_id: &CommitId,
    repo_root: &Path,
    revset: &str,
    progress: LoadProgress,
    batch_size: usize,
    emit: &mut dyn FnMut(Vec<StreamRow>),
) -> Result<(Vec<(usize, bool)>, Option<BranchStatus>, BookmarksInfo)> {
    // The user's revset controls which revisions load. The default (`all()`)
    // covers the working copy, every local bookmark, and tracked/untracked
    // remote bookmarks, so unmerged branches still appear in the graph.
    let expr = parse_user_revset(repo_root, repo.settings(), revset)?;
    let symbol_resolver = SymbolResolver::new(
        repo,
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let resolved = expr
        .resolve_user_expression(repo, &symbol_resolver)
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

    // Index bookmarks by commit id once so the per-commit loop below is a
    // map lookup instead of an O(bookmarks) scan per revision.
    let mut bookmarks_by_commit: HashMap<CommitId, Vec<String>> = HashMap::new();
    for (name, target) in repo.view().bookmarks() {
        collect_bookmark_labels(name.as_str(), &target, |id, label| {
            bookmarks_by_commit
                .entry(id.clone())
                .or_default()
                .push(label);
        });
    }

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
                is_working_copy: id == *wc_commit_id,
                bookmarks,
            };
            ids.push(id);
            batch.push(StreamRow { summary, frame });
            progress.increment();
            if batch.len() >= batch_size {
                emit(std::mem::take(&mut batch));
                batch.reserve(batch_size);
            }
            // `commit` dropped here — we never hold more than one at a time.
        }
    }
    drop(revset);
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
    Ok((empty_updates, branch_status, bookmarks))
}

/// Emit the bookmark chip label(s) for one bookmark onto the commit(s) they sit
/// on, following jj's `bookmarks` template semantics:
/// - the local bookmark renders as `name`, or `name*` when it diverges from any
///   of its tracked remotes (i.e. there are unpushed/unpulled changes);
/// - a tracked remote pointing at the same commit as the local bookmark is
///   redundant and dropped, while a diverged or untracked remote renders as
///   `name@remote`;
/// - jj's colocated-git pseudo-remote (`name@git`) is never shown — it just
///   mirrors the local bookmark and is an implementation detail, so it also
///   never contributes to the `*` divergence check.
///
/// `emit` receives `(commit_id, label)` per chip, so the per-commit graph index
/// and the single-revision diff header can share one rule.
fn collect_bookmark_labels(
    name: &str,
    target: &LocalRemoteRefTarget<'_>,
    mut emit: impl FnMut(&CommitId, String),
) {
    let local_id = target.local_target.added_ids().next();
    let diverged = target.remote_refs.iter().any(|(remote, remote_ref)| {
        remote.as_str() != REMOTE_NAME_FOR_LOCAL_GIT_REPO.as_str()
            && remote_ref.is_tracked()
            && remote_ref.target.added_ids().next() != local_id
    });
    let local_label = if diverged {
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
        for id in remote_ref.target.added_ids() {
            // A tracked remote in sync with the local bookmark is redundant.
            if tracked && Some(id) == local_id {
                continue;
            }
            emit(id, format!("{}@{}", name, remote.as_str()));
        }
    }
}

/// Snapshot every bookmark in the repo with the state the revision context menu
/// needs: each bookmark's local target commit, and each remote ref's target +
/// tracking state. `@`'s commit id is recorded so a working-copy right-click can
/// resolve the bookmarks sitting on it.
fn compute_bookmarks_info(repo: &ReadonlyRepo, wc_commit_id: &CommitId) -> BookmarksInfo {
    let mut bookmarks = Vec::new();
    for (name, target) in repo.view().bookmarks() {
        // `added_ids().next()` (not `as_normal`) so a conflicted bookmark still
        // resolves to one side rather than vanishing from the menu.
        let local_target = target.local_target.added_ids().next().map(|id| id.hex());
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
        if local_target.is_none() && remotes.is_empty() {
            continue;
        }
        bookmarks.push(BookmarkEntry {
            name: name.as_str().to_owned(),
            local_target,
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
fn compute_branch_status(repo: &ReadonlyRepo, wc_commit_id: &CommitId) -> Option<BranchStatus> {
    match branch_status_inner(repo, wc_commit_id) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("diffui: failed to compute branch status: {error:#}");
            None
        }
    }
}

fn branch_status_inner(
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
fn count_revset(
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

/// Batch loader for refreshes: walk the whole graph and fold it into a compact
/// store + layout in one shot (no progressive paint — a refresh swaps the
/// result in atomically so the old graph stays on screen until it's ready). The
/// cold initial load uses [`walk_jj_commits`] directly to stream instead.
pub async fn load_jj_commits(
    repository_root: PathBuf,
    revset: String,
    progress: LoadProgress,
) -> Result<(
    CommitStore,
    GraphLayout,
    Option<BranchStatus>,
    BookmarksInfo,
)> {
    let mut store = CommitStore::default();
    let mut graph = GraphLayout::default();
    let mut interner: HashMap<String, u32> = HashMap::new();
    let mut fold = LaneFoldState::default();

    let (empty_updates, branch_status, bookmarks) = {
        let mut emit = |batch: Vec<StreamRow>| {
            for row in batch {
                graph.push(&row.frame, &row.summary.bookmarks, &mut fold);
                store.push(row.summary, &mut interner);
            }
        };
        walk_jj_commits(repository_root, revset, progress, 4096, &mut emit).await?
    };

    for (index, empty) in empty_updates {
        store.set_is_empty(index, empty);
    }
    Ok((store, graph, branch_status, bookmarks))
}

/// The working-copy diff (or a stringified error) handed to `load_jj_cold`'s
/// `emit_diff` callback — i.e. what becomes a `Message::InitialDiff`.
pub type ColdDiffResult = Result<(DiffDocument, Option<RevisionDetails>), String>;

/// Cold streaming load for a jj repo: snapshot the working copy, then run the
/// working-copy diff and the graph walk **reusing the snapshot's repo**, so the
/// (large) commit index is read once instead of three times — that triple
/// `load_at_head` was the dominant floor before first paint on a 1M-commit repo.
///
/// `emit_diff` fires once with the working-copy diff (cheap now that the repo is
/// already loaded); `emit_batch` fires per commit batch. Returns the snapshot
/// fingerprint + single-parent emptiness for the load's tail.
pub async fn load_jj_cold(
    repository: Repository,
    revset: String,
    progress: LoadProgress,
    batch_size: usize,
    emit_diff: &mut dyn FnMut(ColdDiffResult),
    emit_batch: &mut dyn FnMut(Vec<StreamRow>),
) -> Result<(
    RepositorySnapshot,
    Option<BranchStatus>,
    Vec<(usize, bool)>,
    BookmarksInfo,
)> {
    let (snapshot, repo, wc_commit_id) = load_jj_repository_snapshot(repository.clone()).await?;

    // Emit the working-copy diff up front so the diff pane is ready the moment
    // the first commit batch lifts the loading screen.
    let diff = diff_jj_with_repo(repo.as_ref(), &wc_commit_id, &repository)
        .await
        .map_err(|error| format!("{error:#}"));
    emit_diff(diff);

    let (empty_updates, branch_status, bookmarks) = walk_jj_with_repo(
        repo.as_ref(),
        &wc_commit_id,
        &repository.root,
        &revset,
        progress,
        batch_size,
        emit_batch,
    )
    .await?;
    Ok((snapshot, branch_status, empty_updates, bookmarks))
}

/// Resolve the empty status of specific commits (the merges/roots the loader
/// left unknown) off the load path. `targets` carries the
/// caller's row index alongside the hex commit-id so results can be applied
/// back without a second lookup. Per-commit failures are skipped rather than
/// failing the whole batch.
pub async fn compute_jj_empty_status(
    repository_root: PathBuf,
    targets: Vec<(usize, String)>,
) -> Result<Vec<(usize, bool)>> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())
        .context("failed to load jj settings")?;
    let workspace = Workspace::load(
        &settings,
        &repository_root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    let mut out = Vec::with_capacity(targets.len());
    for (index, commit_id_hex) in targets {
        let Some(id) = CommitId::try_from_hex(&commit_id_hex) else {
            continue;
        };
        let Ok(commit) = repo.store().get_commit_async(&id).await else {
            continue;
        };
        if let Ok(empty) = commit.is_empty(repo.as_ref()).await {
            out.push((index, empty));
        }
    }
    Ok(out)
}

pub async fn load_jj_diff(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
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
    let commit_id = match revision {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace_name)
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(revision) => CommitId::try_from_hex(&revision)
            .with_context(|| format!("invalid jj commit id {revision}"))?,
    };
    diff_jj_with_repo(repo.as_ref(), &commit_id, &repository).await
}

/// Load just the `jj show`-style header for `revision` (no diff): ids,
/// bookmarks, author/committer signatures, description. Used by the revision
/// context menu's copy actions, which need the author/committer dates the
/// in-memory graph doesn't carry. Mirrors [`load_jj_diff`]'s workspace + commit
/// resolution but stops at [`jj_revision_details`].
pub async fn load_jj_revision_details(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<RevisionDetails> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
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
    let commit_id = match revision {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace_name)
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(revision) => CommitId::try_from_hex(&revision)
            .with_context(|| format!("invalid jj commit id {revision}"))?,
    };
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    Ok(jj_revision_details(repo.as_ref(), &commit))
}

/// The diff half of [`load_jj_diff`], given an already-loaded repo. Split out so
/// the cold streaming load ([`load_jj_cold`]) reuses the snapshot's repo for the
/// working-copy diff instead of reading the index again.
async fn diff_jj_with_repo(
    repo: &ReadonlyRepo,
    commit_id: &CommitId,
    repository: &Repository,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let commit = repo
        .store()
        .get_commit_async(commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    let details = jj_revision_details(repo, &commit);
    let old_tree = commit
        .parent_tree(repo)
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit_id.hex()))?;
    let new_tree = commit.tree();
    let matcher = repo_scope_matcher(repository)?;
    let copy_records = CopyRecords::default();
    let tree_diff = old_tree.diff_stream_with_copies(&new_tree, matcher.as_ref(), &copy_records);
    let labels = Diff::new(old_tree.labels(), new_tree.labels());
    let mut stream = materialized_diff_stream(repo.store(), tree_diff, labels);
    let materialize_options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Diff,
        marker_len: None,
        merge: MergeOptions {
            hunk_level: FileMergeHunkLevel::Line,
            same_change: SameChange::Accept,
        },
    };
    let mut files = Vec::new();

    while let Some(entry) = stream.next().await {
        let values = entry.values.with_context(|| {
            format!(
                "failed to read jj diff for {}",
                repo_path_label(entry.path.target())
            )
        })?;
        let old_path = entry
            .path
            .to_diff()
            .map(|paths| repo_path_label(paths.before));
        let path = repo_path_label(entry.path.target());
        let before_absent = values.before.is_absent();
        let after_absent = values.after.is_absent();
        let status = if before_absent {
            DiffFileStatus::Added
        } else if after_absent {
            DiffFileStatus::Deleted
        } else if old_path.is_some() {
            DiffFileStatus::Renamed
        } else {
            DiffFileStatus::Modified
        };
        let before = git_diff_part(entry.path.source(), values.before, &materialize_options)
            .await
            .with_context(|| {
                format!(
                    "failed to read previous content for {}",
                    repo_path_label(entry.path.source())
                )
            })?;
        let after = git_diff_part(entry.path.target(), values.after, &materialize_options)
            .await
            .with_context(|| {
                format!(
                    "failed to read current content for {}",
                    repo_path_label(entry.path.target())
                )
            })?;

        let mut file = DiffFile {
            path,
            old_path,
            status,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        };

        if before.content.is_binary || after.content.is_binary {
            file.hunks.push(DiffHunkView {
                header: "binary files differ".to_owned(),
                lines: Vec::new(),
            });
        } else {
            let hunks = unified_diff_hunks(
                Diff::new(
                    BStr::new(before.content.contents.as_slice()),
                    BStr::new(after.content.contents.as_slice()),
                ),
                3,
                LineCompareMode::Exact,
            );
            for hunk in hunks {
                let mut rows = Vec::new();
                let mut old_line = hunk.left_line_range.start + 1;
                let mut new_line = hunk.right_line_range.start + 1;
                for (line_type, tokens) in hunk.lines {
                    let (content, raw_emphasis) = diff_tokens_to_line(tokens);
                    match line_type {
                        DiffLineType::Context => {
                            rows.push(DiffLine {
                                kind: DiffLineKind::Context,
                                old_line: Some(old_line),
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
                                emphasis: Vec::new(),
                            });
                            old_line += 1;
                            new_line += 1;
                        }
                        DiffLineType::Removed => {
                            file.deletions += 1;
                            let emphasis =
                                crate::diff_parse::finish_line_emphasis(&content, raw_emphasis);
                            rows.push(DiffLine {
                                kind: DiffLineKind::Deletion,
                                old_line: Some(old_line),
                                new_line: None,
                                content,
                                syntax: Vec::new(),
                                emphasis,
                            });
                            old_line += 1;
                        }
                        DiffLineType::Added => {
                            file.additions += 1;
                            let emphasis =
                                crate::diff_parse::finish_line_emphasis(&content, raw_emphasis);
                            rows.push(DiffLine {
                                kind: DiffLineKind::Addition,
                                old_line: None,
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
                                emphasis,
                            });
                            new_line += 1;
                        }
                    }
                }
                file.hunks.push(DiffHunkView {
                    header: format_hunk_header(&hunk.left_line_range, &hunk.right_line_range),
                    lines: rows,
                });
            }
        }

        // Highlighting is deliberately NOT applied here: it's tree-sitter
        // over whole documents — seconds of CPU on big files — and runs in
        // the background instead (see `source::highlight_file`), so the diff
        // paints plain immediately and colorizes progressively.
        files.push(file);
    }

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    Ok((
        DiffDocument {
            files,
            total_additions,
            total_deletions,
        },
        Some(details),
    ))
}

/// Snapshot the working copy, returning the fingerprint plus the post-snapshot
/// repo and working-copy commit id. The cold streaming load ([`load_jj_cold`])
/// reuses that repo for the diff + graph walk so it reads the commit index once
/// instead of three times; the refresh path ([`run_repository_snapshot`]) drops
/// the repo and keeps only the fingerprint.
pub async fn load_jj_repository_snapshot(
    repository: Repository,
) -> Result<(RepositorySnapshot, Arc<ReadonlyRepo>, CommitId)> {
    let settings = jj_settings(&repository.root)?;
    let mut workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();

    let auto_track = snapshot_auto_track_matcher(&settings, &repository.root)?;
    let base_ignores = snapshot_base_ignores(&repository.root)?;
    let max_new_file_size = snapshot_max_new_file_size(&settings)?;

    // Take the working-copy lock *before* reading the repo head. Otherwise a
    // jj-cli command running between `load_at_head` and the lock can rewrite
    // the wc commit out from under us, and our snapshot tx — still parented on
    // the stale op — lands as a sibling of the cli's op. Both ops touch the
    // same change_id with different commit_ids, which jj's concurrent-op
    // resolver presents as a divergent change.
    let repo_loader = workspace.repo_loader().clone();
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .context("failed to lock jj working copy")?;

    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = base_repo
        .view()
        .get_wc_commit_id(&workspace_name)
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
    let old_tree = wc_commit.tree();

    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: auto_track.as_ref(),
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size,
    };
    let (new_tree, _stats) = locked_ws
        .locked_wc()
        .snapshot(&snapshot_options)
        .await
        .context("failed to snapshot jj working copy")?;

    if new_tree.tree_ids_and_labels() == old_tree.tree_ids_and_labels() {
        // No file changes: drop the lock without writing an op. This is the
        // common case on idle ticks and keeps `jj op log` clean. `base_repo` is
        // already at the post-snapshot head, so hand it back for reuse.
        let working_copy_empty = wc_commit.is_empty(base_repo.as_ref()).await.ok();
        let snapshot = RepositorySnapshot {
            fingerprint: base_repo.op_id().hex(),
            working_copy_empty,
        };
        return Ok((snapshot, base_repo, wc_commit_id));
    }

    let mut tx = base_repo.start_transaction();
    tx.set_is_snapshot(true);
    let new_commit = tx
        .repo_mut()
        .rewrite_commit(&wc_commit)
        .set_tree(new_tree)
        .write()
        .await
        .context("failed to rewrite jj working-copy commit with new tree")?;
    tx.repo_mut()
        .set_wc_commit(workspace_name, new_commit.id().clone())
        .context("failed to update jj working-copy pointer")?;
    // `rewrite_commit` records a rewrite that the transaction insists on
    // resolving before commit, even when the wc commit has no descendants.
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after jj snapshot")?;
    let new_wc_commit_id = new_commit.id().clone();
    let new_repo = tx
        .commit("snapshot working copy")
        .await
        .context("failed to commit jj snapshot transaction")?;
    let new_op_id = new_repo.op_id().clone();
    locked_ws
        .finish(new_op_id.clone())
        .await
        .context("failed to finish jj working-copy mutation")?;

    let working_copy_empty = new_commit.is_empty(new_repo.as_ref()).await.ok();
    let snapshot = RepositorySnapshot {
        fingerprint: new_op_id.hex(),
        working_copy_empty,
    };
    Ok((snapshot, new_repo, new_wc_commit_id))
}

/// Read the current jj operation-head id(s) *without* loading the working copy,
/// taking a lock, or walking commits. `get_op_heads` is a bare readdir of
/// `.jj/repo/op_heads/heads`, so this is safe to run on the fs-watcher hot path
/// to decide whether an op-log change is one of ours (dedup) or external.
///
/// Heads are sorted and joined so the string is stable across readdir order and
/// still changes when a divergent head set does. The single-head common case
/// yields exactly the same hex as `RepositorySnapshot::fingerprint`
/// (`op_id().hex()`), so the two compare directly.
pub async fn read_jj_op_head(repository: Repository) -> Result<String> {
    let settings = jj_settings(&repository.root)?;
    let repo_dir = repository.root.join(".jj").join("repo");
    let loader =
        RepoLoader::init_from_file_system(&settings, &repo_dir, &StoreFactories::default())
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

/// Whether moving local bookmark `name` to `to` is a **backwards or sideways**
/// move — the bookmark exists and its current target is not an ancestor of the
/// new one. The jj CLI refuses such moves without `--allow-backwards`, so the
/// UI asks for confirmation first instead of silently diverging from it.
/// A missing bookmark (or one with no local target) is a creation and never
/// backwards; a conflicted bookmark reports `true` (the conservative answer —
/// jj can't fast-forward those either). Read-only: no snapshot, no wc lock.
pub async fn bookmark_move_is_backwards(
    repository: Repository,
    name: String,
    to: RevisionSelection,
) -> Result<bool, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(check_bookmark_move_backwards(repository, name, to))
    })
    .await
    .map_err(|error| format!("bookmark ancestry check task failed: {error}"))?
    .map_err(|error| format!("{error:#}"))
}

async fn check_bookmark_move_backwards(
    repository: Repository,
    name: String,
    to: RevisionSelection,
) -> Result<bool> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    let current = repo.view().get_local_bookmark(RefName::new(&name));
    if current.is_absent() {
        return Ok(false);
    }
    let Some(current) = current.as_normal() else {
        return Ok(true);
    };

    let new_target = match &to {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };

    let forward = repo
        .index()
        .is_ancestor(current, &new_target)
        .context("failed to check bookmark ancestry")?;
    Ok(!forward)
}

/// Read the full old/new contents of one file at `revision`, for full-context
/// syntax highlighting (the old side comes from the first parent — matching
/// what the diff was computed against). Best-effort by design: absent,
/// binary, conflicted, or oversized sides come back `None` and the caller
/// falls back to diff-only reconstruction. Read-only: no snapshot, no lock.
pub async fn read_jj_file_pair(
    repository: Repository,
    revision: RevisionSelection,
    path: String,
    old_path: Option<String>,
) -> Result<(Option<String>, Option<String>)> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(read_jj_file_pair_inner(
            repository, revision, path, old_path,
        ))
    })
    .await
    .context("jj file-pair read task failed")?
}

async fn read_jj_file_pair_inner(
    repository: Repository,
    revision: RevisionSelection,
    path: String,
    old_path: Option<String>,
) -> Result<(Option<String>, Option<String>)> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    let commit_id = match &revision {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;

    let new_tree = commit.tree();
    let old_tree = commit
        .parent_tree(repo.as_ref())
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit_id.hex()))?;

    let new = read_jj_tree_file(repo.as_ref(), &new_tree, &path).await;
    let old = read_jj_tree_file(
        repo.as_ref(),
        &old_tree,
        old_path.as_deref().unwrap_or(&path),
    )
    .await;
    Ok((old, new))
}

/// Materialize one tree entry as text, `None` for anything full-context
/// highlighting can't use (absent, binary, conflicted, oversized, bad path).
async fn read_jj_tree_file(
    repo: &ReadonlyRepo,
    tree: &jj_lib::merged_tree::MergedTree,
    path: &str,
) -> Option<String> {
    // Past this, a parse costs more than the highlight is worth — the caller
    // falls back to the (cheap) diff-only reconstruction.
    const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

    let repo_path = RepoPathBuf::from_internal_string(path.to_owned()).ok()?;
    let value = tree.path_value(&repo_path).await.ok()?;
    if value.is_absent() {
        return None;
    }
    let materialized = materialize_tree_value(repo.store(), &repo_path, value, tree.labels())
        .await
        .ok()?;
    let options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Diff,
        marker_len: None,
        merge: MergeOptions {
            hunk_level: FileMergeHunkLevel::Line,
            same_change: SameChange::Accept,
        },
    };
    let part = git_diff_part(&repo_path, materialized, &options)
        .await
        .ok()?;
    if part.content.is_binary || part.content.contents.len() > MAX_SOURCE_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&part.content.contents).into_owned())
}

/// Apply a revision-context-menu mutation (`new` / `edit` / `abandon`) and
/// reconcile the working copy on disk.
///
/// One locked working-copy session does the whole thing, mirroring what `jj`
/// itself runs: snapshot the current on-disk state into `@` (so uncommitted
/// work survives `@` moving), apply the mutation in a transaction, then check
/// out the resulting `@` so the files on disk match it. Skipping the checkout
/// would leave the old files in place, and the next snapshot would record them
/// into the moved-to commit.
pub(crate) async fn apply_mutation(
    repository: Repository,
    op: MutationOp,
    progress: LoadProgress,
) -> Result<MutationOutcome> {
    let settings = jj_settings(&repository.root)?;
    let mut workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();

    let auto_track = snapshot_auto_track_matcher(&settings, &repository.root)?;
    let base_ignores = snapshot_base_ignores(&repository.root)?;
    let max_new_file_size = snapshot_max_new_file_size(&settings)?;

    let repo_loader = workspace.repo_loader().clone();
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .context("failed to lock jj working copy")?;
    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    let wc_commit_id = base_repo
        .view()
        .get_wc_commit_id(&workspace_name)
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

    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: auto_track.as_ref(),
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size,
    };
    let (new_tree, _stats) = locked_ws
        .locked_wc()
        .snapshot(&snapshot_options)
        .await
        .context("failed to snapshot jj working copy")?;

    let mut tx = base_repo.start_transaction();

    // Fold any uncommitted on-disk changes into `@` first so moving off it
    // doesn't lose them.
    if new_tree.tree_ids_and_labels() != wc_commit.tree().tree_ids_and_labels() {
        let rewritten = tx
            .repo_mut()
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .await
            .context("failed to fold working-copy changes before mutation")?;
        tx.repo_mut()
            .set_wc_commit(workspace_name.clone(), rewritten.id().clone())
            .context("failed to update working-copy pointer before mutation")?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .context("failed to rebase descendants after working-copy fold")?;
    }

    // The post-fold `@`, used to resolve a `WorkingCopy` target after the fold
    // may have rewritten it.
    let current_wc_id = tx
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    // Captured remote output (push only); empty for local mutations.
    let mut push_output: Vec<String> = Vec::new();
    let message = match &op {
        MutationOp::New { parent } => {
            let parent_commit = resolve_mutation_target(tx.repo(), &current_wc_id, parent).await?;
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
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, target).await?;
            let short = short_change_id(&commit);
            tx.repo_mut()
                .edit(workspace_name.clone(), &commit)
                .await
                .context("failed to set working copy to target commit")?;
            format!("Working copy now at {short}")
        }
        MutationOp::Abandon { target } => {
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, target).await?;
            let short = short_change_id(&commit);
            tx.repo_mut().record_abandoned_commit(&commit);
            format!("Abandoned {short}")
        }
        MutationOp::MoveBookmark { name, to } => {
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, to).await?;
            let short = short_change_id(&commit);
            tx.repo_mut().set_local_bookmark_target(
                RefName::new(name),
                RefTarget::normal(commit.id().clone()),
            );
            format!("Moved bookmark {name} to {short}")
        }
        MutationOp::DeleteBookmark { name } => {
            tx.repo_mut()
                .set_local_bookmark_target(RefName::new(name), RefTarget::absent());
            format!("Deleted bookmark {name}")
        }
        MutationOp::TrackBookmark { name, remote } => {
            let symbol = RefName::new(name).to_remote_symbol(RemoteName::new(remote));
            tx.repo_mut()
                .track_remote_bookmark(symbol)
                .with_context(|| format!("failed to track {name}@{remote}"))?;
            format!("Tracking {name}@{remote}")
        }
        MutationOp::PushBookmark { name, remote } => {
            let (message, output) =
                push_bookmark(&settings, tx.repo_mut(), name, remote, &progress)?;
            push_output = output;
            message
        }
    };

    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after mutation")?;

    // Mirror jj's own post-operation behavior: export the updated bookmarks to
    // the colocated git repo so the real git branches (which jj surfaces as
    // `name@git`) follow the move. Without this the jj bookmark moves but the
    // git branch stays put, unlike the `jj` CLI. Bookmarks that can't be
    // represented as a single git ref (e.g. conflicted) come back in
    // `failed_bookmarks`; that's expected and non-fatal, exactly as jj treats
    // it, so only a hard backend error is propagated.
    export_refs(tx.repo_mut()).context("failed to export bookmarks to the git backend")?;

    let new_repo = tx
        .commit(format!("diffui: {message}"))
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
    locked_ws
        .locked_wc()
        .check_out(&new_wc_commit)
        .await
        .context("failed to check out new working copy")?;
    locked_ws
        .finish(new_repo.op_id().clone())
        .await
        .context("failed to finish working-copy mutation")?;

    let moved_working_copy = matches!(
        op,
        MutationOp::New { .. } | MutationOp::Edit { .. } | MutationOp::Abandon { .. }
    );
    Ok(MutationOutcome {
        message,
        moved_working_copy,
        output: push_output,
    })
}

/// Resolve a `RevisionSelection` to a `Commit`. The working-copy case uses
/// `current_wc_id` (the post-snapshot `@`).
async fn resolve_mutation_target(
    repo: &MutableRepo,
    current_wc_id: &CommitId,
    target: &RevisionSelection,
) -> Result<Commit> {
    let commit_id = match target {
        RevisionSelection::WorkingCopy => current_wc_id.clone(),
        RevisionSelection::Commit(hex) => {
            CommitId::try_from_hex(hex).with_context(|| format!("invalid jj commit id {hex}"))?
        }
    };
    repo.store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))
}

fn short_change_id(commit: &Commit) -> String {
    commit.change_id().hex().chars().take(8).collect()
}

/// Push a single local bookmark to `remote` inside the caller's transaction.
/// jj-lib's [`push_refs`] spawns `git push` under the hood, so authentication
/// uses the user's existing git credential setup (SSH agent, credential
/// helper). It also updates the local remote-tracking ref, keeping the
/// sidebar's ahead/behind correct after the push.
fn push_bookmark(
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
const GIT_PROGRESS_SCALE: usize = 1000;

/// [`GitSubprocessCallback`] for push/fetch: captures the remote/local sideband
/// lines git emits — shown in the activity's expanded row (a fetch's progress
/// summary, or a push's GitHub "create a pull request" hint + URL) — and mirrors
/// git's transfer progress onto the activity's [`LoadProgress`] so the toolbar
/// shows a determinate bar while the transfer runs.
struct CollectingCallback {
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
fn collect_sideband_lines(lines: &mut Vec<String>, message: &[u8]) {
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
    repository: Repository,
    target: FetchTarget,
    progress: LoadProgress,
) -> Result<Vec<String>> {
    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    let git_settings =
        GitSettings::from_settings(&settings).context("failed to read git settings")?;
    let import_options = GitImportOptions {
        auto_local_bookmark: git_settings.auto_local_bookmark,
        abandon_unreachable_commits: git_settings.abandon_unreachable_commits,
        remote_auto_track_bookmarks: HashMap::new(),
    };

    // Resolve which remotes to fetch from.
    let remotes: Vec<RemoteNameBuf> = match &target {
        FetchTarget::AllRemotes => {
            get_all_remote_names(repo.store()).context("failed to list git remotes")?
        }
        FetchTarget::RemoteBranch { remote, .. } => vec![RemoteName::new(remote).to_owned()],
    };
    if remotes.is_empty() {
        bail!("no git remotes are configured");
    }

    let mut tx = repo.start_transaction();
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
            let bookmark = match &target {
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
    tx.commit("diffui: fetch")
        .await
        .context("failed to commit fetch")?;

    if lines.is_empty() {
        Ok(vec!["Fetch complete.".to_owned()])
    } else {
        Ok(lines)
    }
}

/// In-process `jj undo`: restore the working state to the parent of the last
/// meaningful operation, then check out the restored `@` so on-disk files
/// match. Mirrors jj-cli's `cmd_undo` (restore-to-parent of the *view*, not a
/// merge-revert), with two diffui-specific adaptations:
///
///   * **Skip diffui's own background snapshot ops.** diffui auto-snapshots the
///     working copy on focus/refresh, so the head op is often a "pure snapshot".
///     jj-cli's interactive undo rarely hits that; here we walk past snapshot
///     ops (`metadata().is_snapshot`) so Undo targets the user's last real
///     operation rather than a no-op snapshot.
///   * We don't replicate the undo/redo *stack-walking* — repeated Undo simply
///     toggles (undo, then undo-the-undo = redo). The op description uses jj's
///     own `undo: restore to operation <id>` prefix, so the result still
///     composes with the jj CLI's undo/redo.
pub(crate) async fn undo_jj(repository: Repository) -> Result<Vec<String>> {
    let settings = jj_settings(&repository.root)?;
    let mut workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();

    let repo_loader = workspace.repo_loader().clone();
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .context("failed to lock jj working copy")?;
    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;

    // Walk past diffui's background snapshot ops to the last meaningful op.
    let mut op_to_undo = base_repo.operation().clone();
    while op_to_undo.metadata().is_snapshot {
        let parents = op_to_undo
            .parents()
            .await
            .context("failed to read operation parents")?;
        match parents.as_slice() {
            [parent] => op_to_undo = parent.clone(),
            _ => break,
        }
    }

    let parents = op_to_undo
        .parents()
        .await
        .context("failed to read operation parents")?;
    let op_to_restore = match parents.as_slice() {
        [parent] => parent.clone(),
        [] => bail!("nothing to undo"),
        _ => bail!("can't undo a merge operation"),
    };

    let undone_description = op_to_undo.metadata().description.clone();

    // Restore the parent op's view (repo state + remote-tracking bookmarks),
    // keeping the current git refs/head — exactly jj's DEFAULT_REVERT_WHAT.
    let restored_view = op_to_restore
        .view()
        .await
        .context("failed to load operation view")?;
    let current_view = base_repo.view().store_view();
    let new_view = restore_repo_and_remote_tracking(restored_view.store_view(), current_view);

    let mut tx = base_repo.start_transaction();
    tx.repo_mut().set_view(new_view);
    let description = format!("undo: restore to operation {}", op_to_restore.id().hex());
    let new_repo = tx
        .commit(description)
        .await
        .context("failed to commit undo")?;

    // Check out the restored `@` so the working-copy files match it.
    let new_wc_id = new_repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit after undo")?
        .clone();
    let new_wc_commit = new_repo
        .store()
        .get_commit_async(&new_wc_id)
        .await
        .with_context(|| format!("failed to load working-copy commit {}", new_wc_id.hex()))?;
    locked_ws
        .locked_wc()
        .check_out(&new_wc_commit)
        .await
        .context("failed to check out working copy after undo")?;
    locked_ws
        .finish(new_repo.op_id().clone())
        .await
        .context("failed to finish working-copy mutation after undo")?;

    let undone = if undone_description.is_empty() {
        "operation".to_owned()
    } else {
        undone_description
            .lines()
            .next()
            .unwrap_or("operation")
            .to_owned()
    };
    Ok(vec![format!("Undid: {undone}")])
}

/// jj's `view_with_desired_portions_restored` for the default `undo` set
/// (`Repo` + `RemoteTracking`): take heads, local bookmarks/tags, the
/// working-copy pointer, and remote-tracking views from the op being restored,
/// but keep the current git refs/head. Inlined so we don't depend on jj-cli.
fn restore_repo_and_remote_tracking(restored: &View, current: &View) -> View {
    View {
        head_ids: restored.head_ids.clone(),
        local_bookmarks: restored.local_bookmarks.clone(),
        local_tags: restored.local_tags.clone(),
        remote_views: restored.remote_views.clone(),
        git_refs: current.git_refs.clone(),
        git_head: current.git_head.clone(),
        wc_commit_ids: restored.wc_commit_ids.clone(),
    }
}

fn jj_revision_details(repo: &dyn Repo, commit: &jj_lib::commit::Commit) -> RevisionDetails {
    let commit_id = commit.id().clone();
    let change_id = commit.change_id().to_string();

    // The bookmark chips sitting on this commit, matching jj's `bookmarks`
    // template: the local name (suffixed `*` when it diverges from a tracked
    // remote), plus any diverged/untracked `name@remote`, with jj's
    // colocated-git pseudo-remote (`name@git`) hidden.
    let mut bookmarks: Vec<String> = Vec::new();
    for (name, target) in repo.view().bookmarks() {
        collect_bookmark_labels(name.as_str(), &target, |id, label| {
            if id == &commit_id {
                bookmarks.push(label);
            }
        });
    }

    let author = jj_signature_info(commit.author());
    let committer = jj_signature_info(commit.committer());

    RevisionDetails {
        commit_id: commit.id().hex(),
        change_id: Some(change_id),
        bookmarks,
        author,
        committer: Some(committer),
        signature: None,
        description: commit.description().to_owned(),
    }
}

fn jj_signature_info(signature: &jj_lib::backend::Signature) -> SignatureInfo {
    SignatureInfo {
        name: signature.name.clone(),
        email: signature.email.clone(),
        timestamp: Some(format_jj_timestamp(&signature.timestamp)),
    }
}

fn format_jj_timestamp(ts: &jj_lib::backend::Timestamp) -> String {
    // jj_lib::backend::Timestamp is a (millis_since_epoch, tz_offset_minutes)
    // pair. We render it using the recorded offset so the timestamp matches
    // what the author actually saw on their clock.
    let total_minutes = ts.tz_offset;
    let total_secs = ts.timestamp.0 / 1000 + total_minutes as i64 * 60;
    let secs = total_secs.rem_euclid(86_400);
    let day = total_secs.div_euclid(86_400);
    let (year, month, mday) = civil_date_from_days(day);
    let hour = (secs / 3600) as u32;
    let minute = ((secs / 60) % 60) as u32;
    let second = (secs % 60) as u32;
    let sign = if total_minutes >= 0 { '+' } else { '-' };
    let offset_hours = total_minutes.unsigned_abs() / 60;
    let offset_mins = total_minutes.unsigned_abs() % 60;
    format!(
        "{year:04}-{month:02}-{mday:02} {hour:02}:{minute:02}:{second:02} {sign}{offset_hours:02}{offset_mins:02}"
    )
}

/// Convert a count of days since the Unix epoch (1970-01-01) into a
/// proleptic Gregorian (year, month, day) tuple. Used so we don't have to
/// pull in `chrono`/`time` just to print timestamps in the revision header.
fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
    // Algorithm from Howard Hinnant's "chrono-Compatible Low-Level Date
    // Algorithms" — converts shifted-era days into year/month/day, then
    // rotates back to a calendar starting in January.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year as i32, month, day)
}

pub(crate) fn jj_settings(repo_root: &Path) -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();

    for path in jj_user_config_paths() {
        load_jj_user_config_path(&mut config, &path)?;
    }

    let repo_config = repo_root.join(".jj").join("repo").join("config.toml");
    if repo_config.is_file() {
        config
            .load_file(ConfigSource::Repo, repo_config.clone())
            .with_context(|| format!("failed to load jj repo config {}", repo_config.display()))?;
    }

    UserSettings::from_config(config).context("failed to build jj settings")
}

fn jj_user_config_paths() -> Vec<PathBuf> {
    if let Ok(env_paths) = env::var("JJ_CONFIG")
        && !env_paths.is_empty()
    {
        let sep = if cfg!(windows) { ';' } else { ':' };
        return env_paths
            .split(sep)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    let mut paths = Vec::new();
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        paths.push(PathBuf::from(xdg).join("jj"));
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".config").join("jj"));
        if cfg!(target_os = "macos") {
            paths.push(home.join("Library").join("Application Support").join("jj"));
        }
    }
    paths
}

fn load_jj_user_config_path(config: &mut StackedConfig, path: &Path) -> Result<()> {
    if path.is_file() {
        config
            .load_file(ConfigSource::User, path.to_path_buf())
            .with_context(|| format!("failed to load jj config file {}", path.display()))?;
    } else if path.is_dir() {
        config
            .load_dir(ConfigSource::User, path)
            .with_context(|| format!("failed to load jj config dir {}", path.display()))?;
    }
    Ok(())
}

fn snapshot_max_new_file_size(settings: &UserSettings) -> Result<u64> {
    use jj_lib::config::ConfigGetError;
    match settings.get_value_with("snapshot.max-new-file-size", HumanByteSize::try_from) {
        Ok(size) => Ok(size.0),
        Err(ConfigGetError::NotFound { .. }) => Ok(DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE),
        Err(err) => Err(err).context("invalid snapshot.max-new-file-size"),
    }
}

fn snapshot_auto_track_matcher(
    settings: &UserSettings,
    repo_root: &Path,
) -> Result<Box<dyn Matcher>> {
    use jj_lib::config::ConfigGetError;
    let raw = match settings.get_string("snapshot.auto-track") {
        Ok(value) => value,
        Err(ConfigGetError::NotFound { .. }) => "all()".to_string(),
        Err(err) => return Err(err).context("invalid snapshot.auto-track"),
    };
    let aliases = FilesetAliasesMap::new();
    let path_converter = RepoPathUiConverter::Fs {
        cwd: repo_root.to_path_buf(),
        base: repo_root.to_path_buf(),
    };
    let context = FilesetParseContext {
        aliases_map: &aliases,
        path_converter: &path_converter,
    };
    let mut diagnostics = FilesetDiagnostics::new();
    let expr: FilesetExpression = parse_fileset(&mut diagnostics, &raw, &context)
        .with_context(|| format!("failed to parse snapshot.auto-track {raw:?}"))?;
    Ok(expr.to_matcher())
}

// `LocalWorkingCopy` walks the repo tree and reads in-tree `.gitignore` files
// itself, so we only need to provide the *out-of-tree* ignores: the user's
// global git ignore and (for git-backed repos) `.git/info/exclude`.
fn snapshot_base_ignores(repo_root: &Path) -> Result<Arc<GitIgnoreFile>> {
    let mut ignores = GitIgnoreFile::empty();

    if let Some(global) = user_global_git_ignore_path() {
        ignores = ignores
            .chain_with_file("", global.clone())
            .with_context(|| format!("failed to read user gitignore {}", global.display()))?;
    }

    let info_exclude = repo_root.join(".git").join("info").join("exclude");
    ignores = ignores
        .chain_with_file("", info_exclude.clone())
        .with_context(|| format!("failed to read {}", info_exclude.display()))?;

    Ok(ignores)
}

fn user_global_git_ignore_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("git").join("ignore"));
    }
    let home = env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("ignore"),
    )
}

fn repo_scope_matcher(repository: &Repository) -> Result<Box<dyn Matcher>> {
    if repository.scope.as_os_str().is_empty() {
        return Ok(Box::new(EverythingMatcher));
    }

    let repo_path =
        RepoPathBuf::parse_fs_path(&repository.root, &repository.root, &repository.scope)
            .with_context(|| format!("failed to parse jj path {}", repository.scope.display()))?;
    Ok(Box::new(PrefixMatcher::new([repo_path])))
}

fn repo_path_label(path: &RepoPath) -> String {
    path.as_internal_file_string().to_owned()
}

/// Flatten one line's diff tokens to its content string plus the byte ranges
/// of the `Different` tokens — jj-lib's word-level refinement, reused as the
/// intra-line emphasis the parser-based backends compute themselves. Ranges
/// are tracked in output-string coordinates so lossy UTF-8 conversion can't
/// shift them; the trailing-newline trim may leave the last range pointing
/// past the content, which `finish_line_emphasis` clamps.
fn diff_tokens_to_line(
    tokens: Vec<(jj_lib::diff_presentation::DiffTokenType, &[u8])>,
) -> (String, Vec<(usize, usize)>) {
    let mut content = String::new();
    let mut raw = Vec::new();
    for (token_type, token) in tokens {
        let start = content.len();
        match std::str::from_utf8(token) {
            Ok(text) => content.push_str(text),
            Err(_) => content.push_str(&String::from_utf8_lossy(token)),
        }
        if token_type == jj_lib::diff_presentation::DiffTokenType::Different {
            raw.push((start, content.len()));
        }
    }

    while content.ends_with(['\n', '\r']) {
        content.pop();
    }

    (content, raw)
}

#[cfg(test)]
mod revset_tests {
    use super::*;

    fn settings() -> UserSettings {
        UserSettings::from_config(StackedConfig::with_defaults()).expect("default settings")
    }

    #[test]
    fn empty_and_all_are_accepted() {
        let s = settings();
        let root = Path::new("/tmp");
        assert!(parse_user_revset(root, &s, "").is_ok());
        assert!(parse_user_revset(root, &s, "   ").is_ok());
        assert!(parse_user_revset(root, &s, "all()").is_ok());
        assert!(parse_user_revset(root, &s, "  all()  ").is_ok());
    }

    #[test]
    fn built_in_functions_and_working_copy_parse() {
        let s = settings();
        let root = Path::new("/tmp");
        // `@` needs the workspace context; the preset functions are built-ins.
        assert!(parse_user_revset(root, &s, "@").is_ok());
        assert!(parse_user_revset(root, &s, "ancestors(@)").is_ok());
        assert!(parse_user_revset(root, &s, "mine()").is_ok());
        assert!(parse_user_revset(root, &s, "conflicts()").is_ok());
    }

    #[test]
    fn malformed_revset_is_rejected() {
        let s = settings();
        let root = Path::new("/tmp");
        assert!(parse_user_revset(root, &s, "(((").is_err());
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
        let result = runtime.block_on(super::load_jj_commits(
            root,
            "none()".to_owned(),
            LoadProgress::default(),
        ));
        let (store, _graph, _branch, _bookmarks) = result.expect("none() should load, not panic");
        assert_eq!(store.len(), 0, "none() should yield an empty graph");
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
        let loaded_email = super::jj_settings(&root)
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
            .block_on(super::read_jj_op_head(repository))
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
    use jj_lib::op_store::{RemoteRef, RemoteRefState};

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
}
