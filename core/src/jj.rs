use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bstr::BStr;
use futures::StreamExt;
use jj_lib::{
    absorb::{AbsorbSource, absorb_hunks, split_hunks_to_trees},
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
    op_store::{LocalRemoteRefTarget, OperationId, RefTarget},
    operation::Operation,
    ref_name::{RefName, RefNameBuf, RemoteName, RemoteNameBuf, WorkspaceName, WorkspaceNameBuf},
    repo::{MutableRepo, ReadonlyRepo, Repo, RepoLoader, StoreFactories},
    repo_path::{RepoPath, RepoPathBuf, RepoPathUiConverter},
    revset::{
        RevsetAliasesMap, RevsetDiagnostics, RevsetExpression, RevsetExtensions,
        RevsetParseContext, RevsetWorkspaceContext, SymbolResolver, UserRevsetExpression,
        parse as parse_revset,
    },
    rewrite::{
        CommitWithSelection, MoveCommitsLocation, MoveCommitsStats, MoveCommitsTarget,
        RebaseOptions, RebasedCommit, duplicate_commits_onto_parents, merge_commit_trees,
        move_commits, squash_commits,
    },
    settings::{HumanByteSize, UserSettings},
    str_util::{StringExpression, StringPattern},
    tree_merge::MergeOptions,
    working_copy::{SnapshotOptions, WorkingCopyFreshness},
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
use crate::mutations::{Destination, MutationOp, MutationOutcome, RebaseSourceMode, SquashTarget};
use crate::repository::{Repository, RepositorySnapshot};
use crate::source_browse::{SourceEntry, SourceEntryStatus, SourceFileData};

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
/// `@`, `mine()`, `conflicts()` resolve against `workspace_name` (the loaded
/// workspace — so `@` is *this* workspace's working copy, not the default
/// one's); a parse error is surfaced so the revset activity can report it.
/// `default_ignored_remote` is jj's colocated-git pseudo-remote, matching jj's
/// own parsing.
fn parse_user_revset(
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
        WorkspaceView {
            wc_commit_id: &wc_commit_id,
            workspace_name,
        },
        &repository_root,
        &revset,
        progress,
        batch_size,
        emit,
    )
    .await
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
/// its working-copy commit id. Split out so the cold streaming load
/// ([`load_jj_cold`]) reuses the repo it loaded for the snapshot instead of
/// reading the (large) commit index a second time.
pub async fn walk_jj_with_repo(
    repo: &ReadonlyRepo,
    workspace: WorkspaceView<'_>,
    repo_root: &Path,
    revset: &str,
    progress: LoadProgress,
    batch_size: usize,
    emit: &mut dyn FnMut(Vec<StreamRow>),
) -> Result<(Vec<(usize, bool)>, Option<BranchStatus>, BookmarksInfo)> {
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
fn collect_bookmark_labels(
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
fn describe_resolution_error(error: jj_lib::revset::RevsetResolutionError) -> anyhow::Error {
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
fn compute_bookmarks_info(repo: &ReadonlyRepo, wc_commit_id: &CommitId) -> BookmarksInfo {
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
    let (snapshot, repo, wc_commit_id, workspace_name) =
        load_jj_repository_snapshot(repository.clone()).await?;

    // Emit the working-copy diff up front so the diff pane is ready the moment
    // the first commit batch lifts the loading screen.
    let diff = diff_jj_with_repo(repo.as_ref(), &wc_commit_id, &repository)
        .await
        .map_err(|error| format!("{error:#}"));
    emit_diff(diff);

    let (empty_updates, branch_status, bookmarks) = walk_jj_with_repo(
        repo.as_ref(),
        WorkspaceView {
            wc_commit_id: &wc_commit_id,
            workspace_name: &workspace_name,
        },
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
    drop(stream);

    // A conflicted commit's parent-tree diff can miss its conflicts entirely:
    // a fresh conflicted merge's tree *is* the merge of its parent trees, so
    // both sides materialize identically and the stream above yields nothing
    // (the same reason jj counts such a merge "empty"). Surface every
    // conflicted path the stream didn't already cover as a synthetic
    // `Conflicted` entry whose hunks are the materialized conflict regions —
    // `jj resolve --list`, but with content.
    if new_tree.has_conflict() {
        let covered: std::collections::HashSet<&str> =
            files.iter().map(|file| file.path.as_str()).collect();
        let mut conflict_files = Vec::new();
        for (repo_path, value) in new_tree.conflicts_matching(matcher.as_ref()) {
            let path = repo_path_label(&repo_path);
            if covered.contains(path.as_str()) {
                continue;
            }
            let value = match value {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("diffui: failed to read conflict at {path}: {error}");
                    continue;
                }
            };
            let materialized =
                materialize_tree_value(repo.store(), &repo_path, value, new_tree.labels())
                    .await
                    .with_context(|| format!("failed to materialize conflict at {path}"))?;
            let part = git_diff_part(&repo_path, materialized, &materialize_options)
                .await
                .with_context(|| format!("failed to read conflict content for {path}"))?;
            let mut file = DiffFile {
                path,
                old_path: None,
                status: DiffFileStatus::Conflicted,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            };
            if part.content.is_binary {
                file.hunks.push(DiffHunkView {
                    header: "binary file conflict".to_owned(),
                    lines: Vec::new(),
                });
            } else {
                let text = String::from_utf8_lossy(&part.content.contents);
                let (hunks, additions, deletions) = conflict_hunks(&text);
                file.hunks = hunks;
                file.additions = additions;
                file.deletions = deletions;
            }
            conflict_files.push(file);
        }
        if !conflict_files.is_empty() {
            files.extend(conflict_files);
            files.sort_by(|a, b| a.path.cmp(&b.path));
        }
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
/// repo, working-copy commit id, and workspace name. The cold streaming load
/// ([`load_jj_cold`]) reuses that repo for the diff + graph walk so it reads
/// the commit index once instead of three times; the refresh path
/// ([`run_repository_snapshot`]) drops the repo and keeps only the fingerprint.
pub async fn load_jj_repository_snapshot(
    repository: Repository,
) -> Result<(
    RepositorySnapshot,
    Arc<ReadonlyRepo>,
    CommitId,
    WorkspaceNameBuf,
)> {
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

    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: auto_track.as_ref(),
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size,
    };

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
    let (base_repo, wc_commit_id, wc_commit) = match freshness {
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
                .get_wc_commit_id(&workspace_name)
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
                .get_wc_commit_id(&workspace_name)
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
                .snapshot(&snapshot_options)
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
                    .set_wc_commit(workspace_name.clone(), new_commit.id().clone())
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
                .get_wc_commit_id(&workspace_name)
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

            let working_copy_empty = desired.is_empty(merged_repo.as_ref()).await.ok();
            let snapshot = RepositorySnapshot {
                fingerprint: merged_repo.op_id().hex(),
                working_copy_empty,
                // Deliberately equal to `fingerprint`: the graph on screen
                // reflects some pre-recovery op, so the mismatch escalates
                // the refresh to a full reload — external ops (the rebase
                // that made us stale, the recovery itself) always landed.
                parent_fingerprint: Some(merged_repo.op_id().hex()),
            };
            return Ok((snapshot, merged_repo, desired_id, workspace_name));
        }
    };
    let old_tree = wc_commit.tree();

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
            // No op written: the snapshot *is* its own base.
            parent_fingerprint: Some(base_repo.op_id().hex()),
        };
        return Ok((snapshot, base_repo, wc_commit_id, workspace_name));
    }

    // The op our snapshot tx is parented on — the frontend compares this to
    // the op its graph reflects to spot external ops (see
    // `RepositorySnapshot::parent_fingerprint`).
    let base_op_id = base_repo.op_id().hex();
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
        .set_wc_commit(workspace_name.clone(), new_commit.id().clone())
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
        parent_fingerprint: Some(base_op_id),
    };
    Ok((snapshot, new_repo, new_wc_commit_id, workspace_name))
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
    // Resolve through the `.jj/repo` pointer file so a secondary workspace
    // (whose op store lives in the primary repo) reads the right heads.
    let repo_dir = crate::repository::resolve_jj_repo_dir(&repository.root)?;
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

/// Resolve `revision` to a loaded commit, sharing the workspace/repo load
/// boilerplate between the source-browser reads. Read-only: no snapshot, no
/// working-copy lock — the working copy resolves to the current `@` commit.
async fn load_jj_commit_at(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<(Arc<ReadonlyRepo>, Commit)> {
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

    let commit_id = match revision {
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
    Ok((repo, commit))
}

/// List every path at `revision` for the source browser. A commit lists its
/// tree; the working copy walks the directory on disk so untracked/ignored
/// files appear too (classified via the same gitignore chain jj's snapshotter
/// uses), with each tracked file's diff status against the parents attached
/// for the status chips. See [`crate::source_browse::list_source_tree`].
pub async fn list_jj_source_tree(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<Vec<SourceEntry>> {
    let (repo, commit) = load_jj_commit_at(&repository, &revision).await?;
    let tree = commit.tree();

    let mut tracked: Vec<String> = Vec::new();
    for (path, _value) in tree.entries() {
        tracked.push(path.as_internal_file_string().to_owned());
    }

    if !matches!(revision, RevisionSelection::WorkingCopy) {
        return Ok(tracked
            .into_iter()
            .map(|path| SourceEntry::new(path, false, SourceEntryStatus::Tracked))
            .collect());
    }

    // The wc's changes vs its parents, for the per-file status chips. Rename
    // detection is skipped (chips, not a diff): a rename reads as an add.
    let changes = jj_change_statuses(repo.as_ref(), &commit).await?;

    // Working copy: walk the real directory. The tree gives the tracked set;
    // gitignore rules split the rest into ignored vs untracked. Directories
    // that are ignored and contain no tracked file are emitted unenumerated —
    // walking a `target/` would dominate the listing for nothing.
    let tracked_set: HashSet<String> = tracked.into_iter().collect();
    let mut tracked_dirs: HashSet<String> = HashSet::new();
    for path in &tracked_set {
        for (index, byte) in path.bytes().enumerate() {
            if byte == b'/' {
                tracked_dirs.insert(path[..index].to_owned());
            }
        }
    }

    let base_ignores = snapshot_base_ignores(&repository.root)?;
    let mut entries: Vec<SourceEntry> = Vec::new();
    walk_working_copy_dir(
        &repository.root,
        "",
        &base_ignores,
        &tracked_set,
        &tracked_dirs,
        &mut entries,
    )?;
    // Tracked files deleted from disk but still in @'s tree don't show — the
    // browser mirrors the directory, and the diff view is where a deletion
    // reads as a change.
    for entry in &mut entries {
        if entry.status == SourceEntryStatus::Tracked {
            entry.change = changes.get(&entry.path).copied();
        }
    }
    Ok(entries)
}

/// Per-path diff status of `commit` against its parent tree — Added /
/// Modified / Conflicted, keyed by the new-side path. Deletions are omitted
/// (a deleted file has no row in the directory listing to chip).
async fn jj_change_statuses(
    repo: &ReadonlyRepo,
    commit: &Commit,
) -> Result<HashMap<String, DiffFileStatus>> {
    let new_tree = commit.tree();
    let old_tree = commit
        .parent_tree(repo)
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit.id().hex()))?;

    let mut changes = HashMap::new();
    let mut stream = old_tree.diff_stream(&new_tree, &EverythingMatcher);
    while let Some(entry) = stream.next().await {
        let Ok(values) = entry.values else {
            continue;
        };
        if values.after.is_absent() {
            continue;
        }
        let status = if !values.after.is_resolved() {
            DiffFileStatus::Conflicted
        } else if values.before.is_absent() {
            DiffFileStatus::Added
        } else {
            DiffFileStatus::Modified
        };
        changes.insert(entry.path.as_internal_file_string().to_owned(), status);
    }
    Ok(changes)
}

/// One directory level of the working-copy walk. `dir_rel` is the
/// `/`-separated repo-relative path (`""` for the root). Recursion chains the
/// directory's own `.gitignore` exactly like jj's snapshotter.
fn walk_working_copy_dir(
    root: &Path,
    dir_rel: &str,
    ignores: &Arc<GitIgnoreFile>,
    tracked: &HashSet<String>,
    tracked_dirs: &HashSet<String>,
    out: &mut Vec<SourceEntry>,
) -> Result<()> {
    let disk_dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_rel)
    };
    let prefix = if dir_rel.is_empty() {
        String::new()
    } else {
        format!("{dir_rel}/")
    };
    let ignores = ignores
        .chain_with_file(&prefix, disk_dir.join(".gitignore"))
        .with_context(|| format!("failed to read {}/.gitignore", disk_dir.display()))?;

    let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
    let read = std::fs::read_dir(&disk_dir)
        .with_context(|| format!("failed to read directory {}", disk_dir.display()))?;
    for item in read {
        let Ok(item) = item else { continue };
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".jj" || name == ".git" {
            continue;
        }
        names.push((name, file_type));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, file_type) in names {
        let rel = format!("{prefix}{name}");
        // Symlinks list as leaf entries — following them risks cycles, and
        // jj tracks them as entries, not trees.
        if file_type.is_dir() && !file_type.is_symlink() {
            let dir_ignored = ignores.matches(&format!("{rel}/"));
            if dir_ignored && !tracked_dirs.contains(&rel) {
                out.push(SourceEntry::new(rel, true, SourceEntryStatus::Ignored));
            } else {
                walk_working_copy_dir(root, &rel, &ignores, tracked, tracked_dirs, out)?;
            }
            continue;
        }
        let status = if tracked.contains(&rel) {
            SourceEntryStatus::Tracked
        } else if ignores.matches(&rel) {
            SourceEntryStatus::Ignored
        } else {
            SourceEntryStatus::Untracked
        };
        out.push(SourceEntry::new(rel, false, status));
    }
    Ok(())
}

/// Read one file for the source browser. The working copy reads straight off
/// the disk (ignored/untracked files exist only there); a commit materializes
/// its tree entry, so conflicted files come back with jj's conflict markers.
pub async fn read_jj_source_file(
    repository: Repository,
    revision: RevisionSelection,
    path: String,
) -> Result<SourceFileData> {
    if matches!(revision, RevisionSelection::WorkingCopy) {
        // Paths come from our own tree listing, but reject traversal anyway —
        // the read must stay inside the workspace.
        if Path::new(&path)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            bail!("invalid path {path}");
        }
        let bytes = std::fs::read(repository.root.join(&path))
            .with_context(|| format!("failed to read {path}"))?;
        return Ok(crate::source_browse::classify_disk_bytes(bytes));
    }

    let (repo, commit) = load_jj_commit_at(&repository, &revision).await?;
    let tree = commit.tree();
    let repo_path = RepoPathBuf::from_internal_string(path.clone())
        .map_err(|_| anyhow::anyhow!("invalid repo path {path}"))?;
    let value = tree
        .path_value(&repo_path)
        .await
        .with_context(|| format!("failed to look up {path}"))?;
    if value.is_absent() {
        bail!("{path} does not exist in this revision");
    }
    let materialized = materialize_tree_value(repo.store(), &repo_path, value, tree.labels())
        .await
        .with_context(|| format!("failed to materialize {path}"))?;
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
        .with_context(|| format!("failed to read {path}"))?;
    let byte_len = part.content.contents.len();
    if part.content.is_binary {
        return Ok(SourceFileData {
            content: None,
            binary: true,
            too_large: false,
            byte_len,
        });
    }
    if byte_len > crate::source_browse::MAX_SOURCE_FILE_BYTES {
        return Ok(SourceFileData {
            content: None,
            binary: false,
            too_large: true,
            byte_len,
        });
    }
    Ok(SourceFileData {
        content: Some(String::from_utf8_lossy(&part.content.contents).into_owned()),
        binary: false,
        too_large: false,
        byte_len,
    })
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
    allow_immutable: bool,
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
    // doesn't lose them. The fold normally rides inside the mutation's own
    // transaction (one op per action; the folded commit stays referenced by
    // the resulting view) — but an *undo* removes commits from the view, so a
    // same-tx fold could end up referenced by no operation at all and be
    // unrecoverable from the op log. For undo the fold is committed as its
    // own snapshot op first, mirroring the CLI's snapshot-before-command.
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
        if matches!(op, MutationOp::Undo { .. }) {
            tx.set_is_snapshot(true);
            let snapped = tx
                .commit("snapshot working copy")
                .await
                .context("failed to commit pre-undo snapshot")?;
            tx = snapped.start_transaction();
        }
    }

    // The post-fold `@`, used to resolve a `WorkingCopy` target after the fold
    // may have rewritten it.
    let current_wc_id = tx
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    // Captured side output: remote sideband for push, skipped/absorbed notes
    // for absorb; empty for the other mutations.
    let mut output: Vec<String> = Vec::new();
    let mut rewritten_commit: Option<String> = None;
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
            // The CLI refuses `jj edit` on immutable commits: a working copy
            // parked there would amend them on the next snapshot.
            if !allow_immutable {
                ensure_rewritable(
                    &repository.root,
                    &settings,
                    &workspace_name,
                    tx.repo(),
                    &[commit.id().clone()],
                )
                .await?;
            }
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
                let commit = resolve_mutation_target(tx.repo(), &current_wc_id, selection).await?;
                if seen.insert(commit.id().clone()) {
                    commits.push(commit);
                }
            }
            if !allow_immutable {
                let ids: Vec<CommitId> = commits.iter().map(|c| c.id().clone()).collect();
                ensure_rewritable(
                    &repository.root,
                    &settings,
                    &workspace_name,
                    tx.repo(),
                    &ids,
                )
                .await?;
            }
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
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, target).await?;
            if !allow_immutable {
                ensure_rewritable(
                    &repository.root,
                    &settings,
                    &workspace_name,
                    tx.repo(),
                    &[commit.id().clone()],
                )
                .await?;
            }
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
                let commit = resolve_mutation_target(tx.repo(), &current_wc_id, selection).await?;
                if seen.insert(commit.id().clone()) {
                    source_commits.push(commit);
                }
            }
            if source_commits.is_empty() {
                bail!("rebase needs at least one source revision");
            }
            let source_ids: Vec<CommitId> = source_commits.iter().map(|c| c.id().clone()).collect();
            let (new_parent_ids, mut new_child_ids, anchor) =
                resolve_rebase_location(tx.repo(), &current_wc_id, destination).await?;
            // `-A parent-of-X` lists X itself among the target's children;
            // moved commits can't also be insertion children (the CLI
            // subtracts the target set the same way).
            new_child_ids.retain(|id| !seen.contains(id));
            match resolve_move_target(tx.repo(), *mode, &source_ids, &new_parent_ids).await? {
                // Branch mode with the destination inside the branch: a
                // benign no-op, like the CLI's "Nothing changed".
                None => format!(
                    "Nothing to rebase — the branch is already based on {}",
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
                    if !allow_immutable {
                        ensure_rewritable(
                            &repository.root,
                            &settings,
                            &workspace_name,
                            tx.repo(),
                            &rewritten,
                        )
                        .await?;
                    }
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
                let commit = resolve_mutation_target(tx.repo(), &current_wc_id, selection).await?;
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
                            "{} is a merge — pick an explicit squash destination",
                            short_change_id(first_source)
                        ),
                    }
                }
                SquashTarget::Revision(target) => {
                    resolve_mutation_target(tx.repo(), &current_wc_id, target).await?
                }
            };
            if seen.contains(destination.id()) {
                bail!("can't squash a revision into itself");
            }
            let mut rewritten: Vec<CommitId> = seen.iter().cloned().collect();
            rewritten.push(destination.id().clone());
            if !allow_immutable {
                ensure_rewritable(
                    &repository.root,
                    &settings,
                    &workspace_name,
                    tx.repo(),
                    &rewritten,
                )
                .await?;
            }
            let source_names: Vec<String> = source_commits.iter().map(short_change_id).collect();
            let short_dest = short_change_id(&destination);
            let combined = combined_squash_description(&destination, &source_commits);
            let mut selections: Vec<CommitWithSelection> = Vec::new();
            for source in source_commits {
                selections.push(CommitWithSelection {
                    selected_tree: source.tree(),
                    parent_tree: source
                        .parent_tree(tx.repo())
                        .await
                        .context("failed to load squash source parent tree")?,
                    commit: source,
                });
            }
            let Some(squashed) = squash_commits(tx.repo_mut(), &selections, &destination, false)
                .await
                .context("failed to squash")?
            else {
                bail!(
                    "nothing to squash from {} — no changes there",
                    source_names.join(", ")
                );
            };
            let new_destination = squashed
                .commit_builder
                .set_description(combined)
                .write()
                .await
                .context("failed to write squashed commit")?;
            rewritten_commit = Some(new_destination.id().hex());
            format!("Squashed {} into {short_dest}", source_names.join(", "))
        }
        MutationOp::Merge { parents } => {
            let mut parent_commits: Vec<Commit> = Vec::new();
            let mut seen: HashSet<CommitId> = HashSet::new();
            for selection in parents {
                let commit = resolve_mutation_target(tx.repo(), &current_wc_id, selection).await?;
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
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, target).await?;
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
            let source = resolve_mutation_target(tx.repo(), &current_wc_id, from).await?;
            let short = short_change_id(&source);
            if !allow_immutable {
                ensure_rewritable(
                    &repository.root,
                    &settings,
                    &workspace_name,
                    tx.repo(),
                    &[source.id().clone()],
                )
                .await?;
            }
            let absorb_source = AbsorbSource::from_commit(tx.repo(), source.clone())
                .await
                .context("failed to prepare absorb source")?;
            // Destinations mirror `jj absorb`'s default `--into`: the mutable
            // ancestors of the source's parents. Scoped so the resolver's
            // borrow of `tx` ends before the mutating absorb below.
            let destinations = {
                let mutable =
                    parse_user_revset(&repository.root, &settings, &workspace_name, "mutable()")?;
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
                bail!(
                    "nothing to absorb from {short} — no mutable ancestor touches the same lines"
                );
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
        MutationOp::MoveBookmark {
            name,
            to,
            push_remote,
        } => {
            let commit = resolve_mutation_target(tx.repo(), &current_wc_id, to).await?;
            let short = short_change_id(&commit);
            tx.repo_mut().set_local_bookmark_target(
                RefName::new(name),
                RefTarget::normal(commit.id().clone()),
            );
            match push_remote {
                // The push reads the bookmark's target back out of this
                // transaction's view, so it pushes the position set above.
                Some(remote) => {
                    let (push_message, remote_output) =
                        push_bookmark(&settings, tx.repo_mut(), name, remote, &progress)?;
                    output = remote_output;
                    format!("Moved bookmark {name} to {short} \u{b7} {push_message}")
                }
                None => format!("Moved bookmark {name} to {short}"),
            }
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
            let (message, remote_output) =
                push_bookmark(&settings, tx.repo_mut(), name, remote, &progress)?;
            output = remote_output;
            message
        }
        MutationOp::Undo { operation_id } => {
            let op_to_undo = match operation_id {
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
                    // diffui auto-snapshots the working copy on focus/refresh,
                    // so the head op is often a pure snapshot; walk past those
                    // so Undo targets the user's last real operation. Repeated
                    // Undo toggles (undo-the-undo = redo) rather than walking
                    // an undo stack.
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
            let parents = op_to_undo
                .parents()
                .await
                .context("failed to read operation parents")?;
            let op_parent = match parents.as_slice() {
                [parent] => parent.clone(),
                [] => bail!("nothing to undo"),
                _ => bail!("can't undo a merge operation"),
            };
            // Merge `(parent(op) − op)` onto the current view through jj's own
            // op-merge machinery (exactly what `jj undo <op>` does), so
            // unrelated later work — including this transaction's
            // working-copy fold above — is preserved rather than wiped the
            // way an op *restore* would.
            let op_repo = repo_loader
                .load_at(&op_to_undo)
                .await
                .context("failed to load the repo at the operation to undo")?;
            let parent_repo = repo_loader
                .load_at(&op_parent)
                .await
                .context("failed to load the repo before the operation to undo")?;
            tx.repo_mut()
                .merge(&op_repo, &parent_repo)
                .await
                .context("failed to merge the undo into the current view")?;
            let undone = op_to_undo
                .metadata()
                .description
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .unwrap_or("operation")
                .to_owned();
            format!("Undid: {undone}")
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

    let moved_working_copy = match &op {
        MutationOp::New { .. }
        | MutationOp::Edit { .. }
        | MutationOp::Abandon { .. }
        | MutationOp::Merge { .. } => true,
        // An undo may or may not move `@` depending on what the undone op
        // did — compare against the post-fold pointer instead of guessing.
        MutationOp::Undo { .. } => new_wc_id != current_wc_id,
        _ => false,
    };
    Ok(MutationOutcome {
        message,
        moved_working_copy,
        rewritten_commit,
        output,
        operation_id: Some(new_repo.op_id().hex()),
    })
}

/// Resolve a rebase [`Destination`] into jj-lib's location parts: the new
/// parents, the new children (the commits that get the moved set inserted
/// under them), and the anchor commit for labels. Mirrors `jj rebase`'s
/// `-d` / `-A` / `-B` / `-A x -B y` resolution.
async fn resolve_rebase_location(
    repo: &MutableRepo,
    current_wc_id: &CommitId,
    destination: &Destination,
) -> Result<(Vec<CommitId>, Vec<CommitId>, Commit)> {
    Ok(match destination {
        Destination::Onto(target) => {
            let anchor = resolve_mutation_target(repo, current_wc_id, target).await?;
            (vec![anchor.id().clone()], Vec::new(), anchor)
        }
        Destination::After(target) => {
            let anchor = resolve_mutation_target(repo, current_wc_id, target).await?;
            let children = RevsetExpression::commits(vec![anchor.id().clone()])
                .children()
                .evaluate(repo)
                .context("failed to resolve the target's children")?
                .iter()
                .map(|entry| entry.context("failed to walk the target's children"))
                .collect::<Result<Vec<CommitId>>>()?;
            (vec![anchor.id().clone()], children, anchor)
        }
        Destination::Before(target) => {
            let anchor = resolve_mutation_target(repo, current_wc_id, target).await?;
            (
                anchor.parent_ids().to_vec(),
                vec![anchor.id().clone()],
                anchor,
            )
        }
        Destination::Between { parent, child } => {
            let parent = resolve_mutation_target(repo, current_wc_id, parent).await?;
            let child = resolve_mutation_target(repo, current_wc_id, child).await?;
            (vec![parent.id().clone()], vec![child.id().clone()], parent)
        }
    })
}

/// The ids in the reverse-topological order (children first) that
/// `MoveCommitsTarget::Commits` requires — revset iteration order guarantees
/// it.
async fn reverse_topo_order(repo: &MutableRepo, ids: &[CommitId]) -> Result<Vec<CommitId>> {
    RevsetExpression::commits(ids.to_vec())
        .evaluate(repo)
        .context("failed to order rebase sources")?
        .iter()
        .map(|entry| entry.context("failed to walk rebase sources"))
        .collect()
}

/// Lower a rebase mode + picked sources into jj-lib's move target. Branch
/// mode resolves here — against the destination — because the moved set is
/// `roots(destination..sources)` (the CLI's `-b`): every commit reachable
/// from the picked revisions but not from the new parents, entered at its
/// fork-point roots.
/// `None` means the branch has no commits outside the destination (the
/// destination is the branch itself or one of its descendants) — a benign
/// nothing-to-do, mirroring the CLI's "Nothing changed", not an error.
async fn resolve_move_target(
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
                .iter()
                .map(|entry| entry.context("failed to walk the branch roots"))
                .collect::<Result<_>>()?;
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
            "commit {} is immutable — it's reachable from immutable_heads() \
             (usually pushed/shared history)",
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
async fn ensure_rewritable(
    repo_root: &Path,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    repo: &MutableRepo,
    ids: &[CommitId],
) -> Result<()> {
    if ids.is_empty() {
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
        .iter()
        .next()
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
fn rebase_stats_message(
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
        return "Nothing to rebase — already in place".to_owned();
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

/// Squash description policy: keep whichever side has one; when both do,
/// join them with a blank line (destination first, like `jj squash`'s
/// combined-editor prefill). The user can refine it afterwards with the
/// inline description editor.
fn combined_squash_description(destination: &Commit, sources: &[Commit]) -> String {
    let parts: Vec<&str> = std::iter::once(destination)
        .chain(sources)
        .map(|commit| commit.description().trim())
        .filter(|description| !description.is_empty())
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    // Stored descriptions end with a newline (jj's own convention).
    let mut combined = parts.join("\n\n");
    combined.push('\n');
    combined
}

/// Predict a merge draft's outcome: merge the parents' trees in a throwaway
/// transaction and list the paths that stay conflicted. Same storage caveat
/// as [`preview_rebase`] — unreachable objects only, no op written.
pub(crate) async fn preview_merge(
    repository: Repository,
    parents: Vec<RevisionSelection>,
) -> Result<crate::mutations::MergePreview> {
    const CONFLICT_LIST_CAP: usize = 6;

    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    let tx = repo.start_transaction();
    let mut parent_commits: Vec<Commit> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in &parents {
        let commit = resolve_mutation_target(tx.repo(), &wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            parent_commits.push(commit);
        }
    }
    if parent_commits.len() < 2 {
        bail!("a merge needs at least two distinct parents");
    }
    let tree = merge_commit_trees(tx.repo(), &parent_commits)
        .await
        .context("failed to merge parent trees")?;
    let mut conflicts: Vec<String> = Vec::new();
    let mut truncated = false;
    for (path, _value) in tree.conflicts() {
        if conflicts.len() == CONFLICT_LIST_CAP {
            truncated = true;
            break;
        }
        conflicts.push(path.as_internal_file_string().to_owned());
    }
    // `tx` dropped — nothing committed.
    Ok(crate::mutations::MergePreview {
        conflicts,
        truncated,
    })
}

/// Predict a rebase draft's outcome by running the real `move_commits` inside
/// a transaction that is never committed: exact moved/descendant counts and
/// which commits become conflicted that weren't before. The simulation writes
/// unreachable content-addressed objects to the backend store (like any
/// abandoned jj op would); the op log and views are untouched, so nothing is
/// visible and `jj util gc` reclaims them. Skipped past
/// [`REBASE_PREVIEW_SIMULATION_CAP`] affected commits — counts only then.
pub(crate) async fn preview_rebase(
    repository: Repository,
    mode: RebaseSourceMode,
    sources: Vec<RevisionSelection>,
    destination: Destination,
) -> Result<crate::mutations::RebasePreview> {
    const REBASE_PREVIEW_SIMULATION_CAP: usize = 400;

    let settings = jj_settings(&repository.root)?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();

    let mut tx = repo.start_transaction();
    let mut source_ids: Vec<CommitId> = Vec::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    for selection in &sources {
        let commit = resolve_mutation_target(tx.repo(), &wc_commit_id, selection).await?;
        if seen.insert(commit.id().clone()) {
            source_ids.push(commit.id().clone());
        }
    }
    if source_ids.is_empty() {
        bail!("rebase needs at least one source revision");
    }
    let (new_parent_ids, mut new_child_ids, _anchor) =
        resolve_rebase_location(tx.repo(), &wc_commit_id, &destination).await?;
    new_child_ids.retain(|id| !seen.contains(id));
    let Some(target) = resolve_move_target(tx.repo(), mode, &source_ids, &new_parent_ids).await?
    else {
        // Branch mode with the destination inside the branch itself: nothing
        // would move. A first-class empty preview (not an error) so the op
        // bar can say so while the candidate walks the branch's own rows.
        return Ok(crate::mutations::RebasePreview {
            moved: 0,
            descendants: 0,
            abandoned_empty: 0,
            new_conflicts: Vec::new(),
            entry_points: Vec::new(),
            moved_commit_ids: Vec::new(),
            simulated: true,
        });
    };

    // Entry points of the moved set — for branch mode these are the resolved
    // fork-point roots, i.e. the answer to "which branch would this move?".
    let entry_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) | MoveCommitsTarget::Roots(ids) => ids.clone(),
    };
    let mut entry_points: Vec<String> = Vec::new();
    for id in &entry_ids {
        let commit = tx
            .repo()
            .store()
            .get_commit_async(id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        entry_points.push(short_change_id(&commit));
    }
    entry_points.sort_unstable();

    // The full moved set (Roots targets move their entire subtree; Commits
    // targets move exactly themselves) — doubles as the size guard: past the
    // cap each preview would cost a real rebase's worth of work, so it
    // degrades to counts + entry points only. Also the sidebar's
    // whole-branch wash.
    let moved_ids: Vec<CommitId> = match &target {
        MoveCommitsTarget::Commits(ids) => ids.clone(),
        MoveCommitsTarget::Roots(ids) => RevsetExpression::commits(ids.clone())
            .descendants()
            .evaluate(tx.repo())
            .context("failed to enumerate the moved set")?
            .iter()
            .take(REBASE_PREVIEW_SIMULATION_CAP + 1)
            .map(|entry| entry.context("failed to walk the moved set"))
            .collect::<Result<_>>()?,
    };
    if moved_ids.len() > REBASE_PREVIEW_SIMULATION_CAP {
        return Ok(crate::mutations::RebasePreview {
            moved: moved_ids.len() as u32,
            descendants: 0,
            abandoned_empty: 0,
            new_conflicts: Vec::new(),
            entry_points,
            moved_commit_ids: Vec::new(),
            simulated: false,
        });
    }
    let moved_commit_ids: Vec<String> = moved_ids.iter().map(|id| id.hex()).collect();

    let location = MoveCommitsLocation {
        new_parent_ids,
        new_child_ids,
        target,
    };
    let stats = move_commits(tx.repo_mut(), &location, &RebaseOptions::default())
        .await
        .context("failed to simulate rebase")?;

    let mut new_conflicts: Vec<String> = Vec::new();
    for (old_id, rebased) in &stats.rebased_commits {
        let RebasedCommit::Rewritten(new_commit) = rebased else {
            continue;
        };
        if new_commit.tree_ids().is_resolved() {
            continue;
        }
        let old = tx
            .repo()
            .store()
            .get_commit_async(old_id)
            .await
            .with_context(|| format!("failed to load jj commit {}", old_id.hex()))?;
        if old.tree_ids().is_resolved() {
            new_conflicts.push(short_change_id(&old));
        }
    }
    new_conflicts.sort_unstable();
    // Dropping `tx` here discards the simulation — no op is committed.
    Ok(crate::mutations::RebasePreview {
        moved: stats.num_rebased_targets,
        descendants: stats.num_rebased_descendants,
        abandoned_empty: stats.num_abandoned_empty,
        new_conflicts,
        entry_points,
        moved_commit_ids,
        simulated: true,
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
    // `to_string` is the k–z reverse-hex form jj log (and the sidebar) shows;
    // `.hex()` would print the raw hex nobody ever sees.
    commit.change_id().to_string().chars().take(8).collect()
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

    // No sideband output means the fetch was a no-op (up to date); the caller
    // words the summary.
    Ok(lines)
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

    // Through the `.jj/repo` pointer so a secondary workspace picks up the
    // primary repo's config. Best-effort: an unresolvable pointer just means
    // no repo-level config layer (Workspace::load will surface the breakage).
    if let Ok(repo_dir) = crate::repository::resolve_jj_repo_dir(repo_root) {
        // jj ≤ 0.40 kept the repo config inside the repo dir; jj 0.41 moved it
        // to `<user config dir>/repos/<config-id>/config.toml` with a
        // `config-id` pointer file in the repo dir. Honor both, or a repo
        // config written by a newer `jj config set --repo` is silently
        // invisible here (revsets.log, immutable_heads() overrides, …).
        let mut repo_config = repo_dir.join("config.toml");
        if !repo_config.is_file()
            && let Ok(id) = std::fs::read_to_string(repo_dir.join("config-id"))
        {
            let id = id.trim();
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) {
                for dir in jj_user_config_paths() {
                    let candidate = dir.join("repos").join(id).join("config.toml");
                    if candidate.is_file() {
                        repo_config = candidate;
                        break;
                    }
                }
            }
        }
        if repo_config.is_file() {
            config
                .load_file(ConfigSource::Repo, repo_config.clone())
                .with_context(|| {
                    format!("failed to load jj repo config {}", repo_config.display())
                })?;
        }
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

/// Build display hunks for a materialized conflicted file: one hunk per
/// conflict-marker block (`<<<<<<<` … `>>>>>>>`) with up to three lines of
/// surrounding context, blocks whose context windows touch merged into one
/// hunk. Returns `(hunks, additions, deletions)`, counting the `+`/`-` body
/// lines inside `%%%%%%%` diff sections so the file list shows the conflict's
/// size.
///
/// Classification follows jj's materialization format: a marker is a run of
/// ≥7 identical marker characters followed by a space or end-of-line. (jj
/// lengthens markers past 7 only when the content contains lookalike lines,
/// so ≥7 matches every marker jj emits — at the cost of also matching those
/// rare lookalikes, a cosmetic mislabel at worst.) Marker lines render as
/// `Conflict`; inside a `%%%%%%%` diff section `-`/`+` prefixes render as
/// deletion/addition; everything else is context. Lines number on the new
/// side only — a conflict has no meaningful old-side numbering. A
/// materialization with no markers at all (a non-file conflict's short
/// description) becomes a single all-context hunk.
fn conflict_hunks(text: &str) -> (Vec<DiffHunkView>, usize, usize) {
    const CONTEXT_LINES: usize = 3;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return (Vec::new(), 0, 0);
    }
    let last = lines.len() - 1;

    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        match conflict_marker_char(line) {
            Some(b'<') if open.is_none() => open = Some(index),
            Some(b'>') => {
                if let Some(start) = open.take() {
                    blocks.push((start, index));
                }
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        // Unterminated block (truncated content or a lookalike line): show
        // through to the end rather than dropping it.
        blocks.push((start, last));
    }
    if blocks.is_empty() {
        blocks.push((0, last));
    }

    // Expand each block by the context margin and merge windows that touch,
    // so no hunk ever begins inside a block — the classification below
    // re-walks marker state from each hunk's first line.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (block_start, block_end) in blocks {
        let start = block_start.saturating_sub(CONTEXT_LINES);
        let end = (block_end + CONTEXT_LINES).min(last);
        match ranges.last_mut() {
            Some((_, prev_end)) if start <= *prev_end + 1 => *prev_end = (*prev_end).max(end),
            _ => ranges.push((start, end)),
        }
    }

    let mut hunks = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for (start, end) in ranges {
        let mut rows = Vec::with_capacity(end - start + 1);
        let mut header: Option<String> = None;
        let mut in_block = false;
        let mut diff_section = false;
        for (index, line) in lines[start..=end].iter().enumerate() {
            let kind = match conflict_marker_char(line) {
                Some(b'<') => {
                    in_block = true;
                    diff_section = false;
                    if header.is_none() {
                        // The opening marker carries jj's own label
                        // ("Conflict 1 of 2") — reuse it as the hunk header.
                        let label = line.trim_start_matches('<').trim();
                        if !label.is_empty() {
                            header = Some(label.to_owned());
                        }
                    }
                    DiffLineKind::Conflict
                }
                Some(b'>') => {
                    in_block = false;
                    diff_section = false;
                    DiffLineKind::Conflict
                }
                Some(marker) if in_block => {
                    diff_section = marker == b'%';
                    DiffLineKind::Conflict
                }
                _ if in_block && diff_section => match line.as_bytes().first() {
                    Some(b'-') => DiffLineKind::Deletion,
                    Some(b'+') => DiffLineKind::Addition,
                    _ => DiffLineKind::Context,
                },
                _ => DiffLineKind::Context,
            };
            match kind {
                DiffLineKind::Addition => additions += 1,
                DiffLineKind::Deletion => deletions += 1,
                _ => {}
            }
            rows.push(DiffLine {
                kind,
                old_line: None,
                new_line: Some(start + index + 1),
                content: (*line).to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
        }
        hunks.push(DiffHunkView {
            header: header.unwrap_or_else(|| "conflict".to_owned()),
            lines: rows,
        });
    }
    (hunks, additions, deletions)
}

/// The marker character opening `line` when it is a jj conflict-marker line:
/// a run of ≥7 identical characters from the marker alphabet, followed by a
/// space or end-of-line.
fn conflict_marker_char(line: &str) -> Option<u8> {
    const MIN_MARKER_LEN: usize = 7;
    let bytes = line.as_bytes();
    let first = *bytes.first()?;
    if !matches!(first, b'<' | b'>' | b'%' | b'+' | b'|' | b'=') {
        return None;
    }
    let run = bytes.iter().take_while(|&&b| b == first).count();
    if run < MIN_MARKER_LEN {
        return None;
    }
    match bytes.get(run) {
        None | Some(b' ') => Some(first),
        _ => None,
    }
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
mod conflict_hunk_tests {
    use super::*;

    const MATERIALIZED: &str = "fn one() {}\n\
        context a\n\
        context b\n\
        context c\n\
        <<<<<<< Conflict 1 of 1\n\
        %%%%%%% Changes from base to side #1\n\
        -old line\n\
        +new line\n\
        +++++++ Contents of side #2\n\
        other side\n\
        >>>>>>> Conflict 1 of 1 ends\n\
        context d\n\
        context e\n\
        context f\n\
        fn two() {}\n";

    #[test]
    fn single_block_becomes_one_hunk_with_context() {
        let (hunks, additions, deletions) = conflict_hunks(MATERIALIZED);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.header, "Conflict 1 of 1");
        // 3 context above + 7 block lines + 3 context below.
        assert_eq!(hunk.lines.len(), 13);
        // "fn one() {}" and "fn two() {}" sit outside the context margin.
        assert!(hunk.lines.iter().all(|l| !l.content.contains("fn ")));
        assert_eq!(additions, 1);
        assert_eq!(deletions, 1);
        assert_eq!(
            hunk.lines
                .iter()
                .filter(|l| l.kind == DiffLineKind::Conflict)
                .count(),
            4,
            "the four marker lines render as Conflict"
        );
        // The snapshot-section body line is plain context, not an addition.
        let other = hunk
            .lines
            .iter()
            .find(|l| l.content == "other side")
            .expect("side #2 content present");
        assert_eq!(other.kind, DiffLineKind::Context);
        // Line numbers are 1-based positions in the materialized file.
        assert_eq!(hunk.lines[0].new_line, Some(2));
    }

    #[test]
    fn adjacent_blocks_merge_into_one_hunk() {
        let text = "\
            <<<<<<< Conflict 1 of 2\n\
            +++++++ Contents of side #1\n\
            a\n\
            >>>>>>> Conflict 1 of 2 ends\n\
            between\n\
            <<<<<<< Conflict 2 of 2\n\
            +++++++ Contents of side #1\n\
            b\n\
            >>>>>>> Conflict 2 of 2 ends\n";
        let (hunks, _, _) = conflict_hunks(text);
        assert_eq!(hunks.len(), 1, "touching context windows merge");
        assert_eq!(hunks[0].header, "Conflict 1 of 2");
        assert_eq!(hunks[0].lines.len(), 9);
    }

    #[test]
    fn markerless_content_is_one_context_hunk() {
        let (hunks, additions, deletions) = conflict_hunks("Conflict:\n  weird tree thing\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header, "conflict");
        assert_eq!(hunks[0].lines.len(), 2);
        assert_eq!(additions + deletions, 0);
        assert!(
            hunks[0]
                .lines
                .iter()
                .all(|l| l.kind == DiffLineKind::Context)
        );
    }

    #[test]
    fn marker_detection_requires_run_and_separator() {
        assert_eq!(conflict_marker_char("<<<<<<< Conflict 1 of 1"), Some(b'<'));
        assert_eq!(conflict_marker_char("<<<<<<<"), Some(b'<'));
        assert_eq!(conflict_marker_char("<<<<<<<<<<< longer"), Some(b'<'));
        assert_eq!(conflict_marker_char("<<<<<< too short"), None);
        assert_eq!(conflict_marker_char("<<<<<<<not-a-marker"), None);
        assert_eq!(
            conflict_marker_char("+++++++ Contents of side #1"),
            Some(b'+')
        );
        assert_eq!(conflict_marker_char("+e"), None);
        assert_eq!(conflict_marker_char(""), None);
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
