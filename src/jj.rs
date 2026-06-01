use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use bstr::BStr;
use futures::StreamExt;
use jj_lib::{
    backend::{CommitId, TreeId},
    config::{ConfigSource, StackedConfig},
    conflicts::{ConflictMarkerStyle, ConflictMaterializeOptions, materialized_diff_stream},
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
    gitignore::GitIgnoreFile,
    graph::{GraphNode, TopoGroupedGraphIterator},
    matchers::{EverythingMatcher, Matcher, NothingMatcher, PrefixMatcher},
    merge::{Diff, Merge, SameChange},
    object_id::ObjectId,
    repo::{Repo, StoreFactories},
    repo_path::{RepoPath, RepoPathBuf, RepoPathUiConverter},
    revset::{RevsetExpression, SymbolResolver},
    settings::{HumanByteSize, UserSettings},
    tree_merge::MergeOptions,
    working_copy::SnapshotOptions,
    workspace::{Workspace, default_working_copy_factories},
};

use crate::backend::{
    CommitStore, CommitStoreBuilder, CommitSummary, DiffDocument, DiffFile, DiffFileStatus,
    DiffHunkView, DiffLine, DiffLineKind, LoadProgress, RevisionDetails, RevisionSelection,
    SignatureInfo, apply_syntax_highlighting, format_hunk_header,
};
use crate::graph::LaneAssigner;
use crate::graph_layout::{GraphLayout, GraphLayoutBuilder};
use crate::repository::{Repository, RepositorySnapshot};

// Fallback used only when the user has not configured `snapshot.max-new-file-size`.
// Matches jj-cli's shipped default (1 MiB).
const DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE: u64 = 1024 * 1024;

pub async fn load_jj_commits(
    repository_root: PathBuf,
    progress: LoadProgress,
) -> Result<(CommitStore, GraphLayout)> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())
        .context("failed to load jj settings")?;
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

    // Default revset: `all()` — ancestors of every visible head plus any
    // referenced commit. Covers the working copy, all local bookmarks,
    // and tracked/untracked remote bookmarks, so branches that haven't
    // been merged into the WC still show up in the graph.
    let expr = RevsetExpression::all();
    let symbol_resolver = SymbolResolver::new(
        repo.as_ref(),
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let resolved = expr
        .resolve_user_expression(repo.as_ref(), &symbol_resolver)
        .context("failed to resolve jj revset")?;
    let revset = resolved
        .evaluate(repo.as_ref())
        .context("failed to evaluate jj revset")?;

    let nodes: Vec<GraphNode<CommitId>> = {
        let mut topo = TopoGroupedGraphIterator::new(revset.iter_graph(), |id: &CommitId| id);
        topo.prioritize_branch(wc_commit_id.clone());
        topo.collect::<Result<Vec<_>, _>>()
            .context("failed to walk jj revset graph")?
    };
    drop(revset);
    progress.set_total(nodes.len());

    // Index bookmarks by commit id once so the per-commit loop below is a
    // map lookup instead of an O(bookmarks) scan per revision.
    let mut bookmarks_by_commit: HashMap<CommitId, Vec<String>> = HashMap::new();
    for (name, target) in repo.view().bookmarks() {
        for id in target.local_target.added_ids() {
            bookmarks_by_commit
                .entry(id.clone())
                .or_default()
                .push(name.as_str().to_owned());
        }
        for (remote, remote_ref) in &target.remote_refs {
            for id in remote_ref.target.added_ids() {
                bookmarks_by_commit
                    .entry(id.clone())
                    .or_default()
                    .push(format!("{}@{}", name.as_str(), remote.as_str()));
            }
        }
    }

    // Build the store in a single pass, dropping each jj `Commit` as soon as
    // its data is extracted rather than holding all of them at once — that Vec
    // is ~400MB on a million-commit repo. Single-parent emptiness needs a
    // parent's tree-id, which (in descendants-first order) hasn't been loaded
    // yet, so we keep just the tree-id map + each commit's lone parent and
    // resolve it cheaply in a second pass. Merges/roots stay unknown and are
    // resolved off the load path (see `compute_jj_empty_status`).
    let mut builder = CommitStoreBuilder::with_capacity(nodes.len());
    let mut tree_ids: HashMap<CommitId, Merge<TreeId>> = HashMap::with_capacity(nodes.len());
    let mut single_parents: Vec<Option<CommitId>> = Vec::with_capacity(nodes.len());
    let mut lane_assigner = LaneAssigner::new();
    let mut graph_builder = GraphLayoutBuilder::new();
    for (id, edges) in nodes.iter() {
        // Advance the lane state for every node in topo order. The assigner is
        // stateful, so this must run once per node — keep it first in the loop.
        let lane_frame = lane_assigner.push(id, edges);
        let commit = repo
            .store()
            .get_commit_async(id)
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
        let bookmarks = bookmarks_by_commit.get(id).cloned().unwrap_or_default();
        graph_builder.push(&lane_frame, &bookmarks);
        builder.push(CommitSummary {
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
            is_working_copy: id == &wc_commit_id,
            bookmarks,
        });
        progress.increment();
        // `commit` dropped here — we never hold more than one at a time.
    }

    // Resolve single-parent emptiness now that every tree-id is known: a
    // commit is empty iff its tree matches its parent's (a cheap id compare).
    let mut store = builder.finish();
    for index in 0..store.len() {
        let Some(parent) = &single_parents[index] else {
            continue;
        };
        if let (Some(own), Some(parent_tree)) =
            (tree_ids.get(&nodes[index].0), tree_ids.get(parent))
        {
            store.set_is_empty(index, own == parent_tree);
        }
    }

    Ok((store, graph_builder.finish()))
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
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    let details = jj_revision_details(repo.as_ref(), &commit);
    let old_tree = commit
        .parent_tree(repo.as_ref())
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit_id.hex()))?;
    let new_tree = commit.tree();
    let matcher = repo_scope_matcher(&repository)?;
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
                    let content = diff_tokens_to_string(tokens);
                    match line_type {
                        DiffLineType::Context => {
                            rows.push(DiffLine {
                                kind: DiffLineKind::Context,
                                old_line: Some(old_line),
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
                            });
                            old_line += 1;
                            new_line += 1;
                        }
                        DiffLineType::Removed => {
                            file.deletions += 1;
                            rows.push(DiffLine {
                                kind: DiffLineKind::Deletion,
                                old_line: Some(old_line),
                                new_line: None,
                                content,
                                syntax: Vec::new(),
                            });
                            old_line += 1;
                        }
                        DiffLineType::Added => {
                            file.additions += 1;
                            rows.push(DiffLine {
                                kind: DiffLineKind::Addition,
                                old_line: None,
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
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

        apply_syntax_highlighting(&mut file);
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

pub async fn load_jj_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
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
        // common case on idle ticks and keeps `jj op log` clean.
        return Ok(RepositorySnapshot {
            fingerprint: base_repo.op_id().hex(),
        });
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
    let new_repo = tx
        .commit("snapshot working copy")
        .await
        .context("failed to commit jj snapshot transaction")?;
    let new_op_id = new_repo.op_id().clone();
    locked_ws
        .finish(new_op_id.clone())
        .await
        .context("failed to finish jj working-copy mutation")?;

    Ok(RepositorySnapshot {
        fingerprint: new_op_id.hex(),
    })
}

fn jj_revision_details(repo: &dyn Repo, commit: &jj_lib::commit::Commit) -> RevisionDetails {
    let commit_id = commit.id().clone();
    let change_id = commit.change_id().to_string();

    // Build a flat list mirroring `jj show`'s "Bookmarks:" line:
    // local name first ("main"), then each remote tracking ref as
    // `name@remote` ("main@git", "main@origin"). Skip names whose targets
    // don't actually point at this commit.
    let mut bookmarks: Vec<String> = Vec::new();
    for (name, target) in repo.view().bookmarks() {
        if target.local_target.added_ids().any(|id| id == &commit_id) {
            bookmarks.push(name.as_str().to_owned());
        }
        for (remote, remote_ref) in &target.remote_refs {
            if remote_ref.target.added_ids().any(|id| id == &commit_id) {
                bookmarks.push(format!("{}@{}", name.as_str(), remote.as_str()));
            }
        }
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
    let offset_hours = total_minutes.abs() / 60;
    let offset_mins = total_minutes.abs() % 60;
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

fn jj_settings(repo_root: &Path) -> Result<UserSettings> {
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

fn diff_tokens_to_string(tokens: Vec<(jj_lib::diff_presentation::DiffTokenType, &[u8])>) -> String {
    let mut bytes = Vec::new();
    for (_, token) in tokens {
        bytes.extend_from_slice(token);
    }

    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(all(test, feature = "track-alloc"))]
mod mem_profile {
    use crate::backend::LoadProgress;
    use crate::track_alloc::{CURRENT, PEAK};
    use std::sync::atomic::Ordering::Relaxed;

    /// Loads a real jj repo and prints the retained store size against the
    /// transient allocation peak during the load — the breakdown behind "why
    /// does RSS dwarf the live data". Point it at a repo with:
    ///   DIFFUI_PROFILE_REPO=/path \
    ///   cargo test --features track-alloc profile_load_memory -- --ignored --nocapture
    /// Defaults to the bun benchmark clone. `#[ignore]`d since it needs a repo
    /// on disk; counts are logical bytes (no allocator rounding), so true RSS
    /// runs higher — the peak/live ratio is the signal.
    #[test]
    #[ignore]
    fn profile_load_memory() {
        let repo = std::env::var("DIFFUI_PROFILE_REPO")
            .unwrap_or_else(|_| format!("{}/code/bun", std::env::var("HOME").expect("HOME set")));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        let progress = LoadProgress::default();
        let baseline = CURRENT.load(Relaxed);
        PEAK.store(baseline, Relaxed);

        let (store, graph) = runtime
            .block_on(super::load_jj_commits(repo.clone().into(), progress))
            .expect("load commits");

        let peak = PEAK.load(Relaxed).saturating_sub(baseline);
        let live = CURRENT.load(Relaxed).saturating_sub(baseline);
        // `store.heap_bytes()` no longer includes lanes (they moved to the
        // `GraphLayout`); `live` (allocator current) still counts everything,
        // including `graph`, so keep it alive until after we read the counters.
        let store_heap = store.heap_bytes();
        let n = store.len().max(1);
        let mb = |bytes: usize| bytes as f64 / 1.0e6;

        eprintln!("\n=== diffui memory profile (logical bytes) ===");
        eprintln!("repo            : {repo}");
        eprintln!("commits         : {}", store.len());
        eprintln!("transient peak  : {:>9.1} MB", mb(peak));
        eprintln!("live after load : {:>9.1} MB  (allocator current)", mb(live));
        eprintln!("store.heap()    : {:>9.1} MB  (accounted)", mb(store_heap));
        eprintln!(
            "per commit      : store {:.0} B    peak {:.0} B",
            store_heap as f64 / n as f64,
            peak as f64 / n as f64
        );
        eprintln!(
            "peak / live     : {:.2}x  (how much of the high-water mark is transient)",
            peak as f64 / store_heap.max(1) as f64
        );
        eprintln!("=============================================\n");

        drop((store, graph));
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
        let repo = std::env::var("DIFFUI_PROFILE_REPO")
            .unwrap_or_else(|_| format!("{}/code/nixpkgs", std::env::var("HOME").expect("HOME set")));
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
            let nodes: Vec<GraphNode<CommitId>> = {
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
