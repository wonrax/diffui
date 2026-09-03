use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use jj_lib::{
    commit::Commit,
    conflicts::{ConflictMarkerStyle, ConflictMaterializeOptions, materialize_tree_value},
    diff_presentation::unified::git_diff_part,
    files::FileMergeHunkLevel,
    gitignore::GitIgnoreFile,
    matchers::EverythingMatcher,
    merge::SameChange,
    object_id::ObjectId,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo},
    repo_path::RepoPathBuf,
    tree_merge::MergeOptions,
};

use super::settings::*;
use super::workspace::resolve_revision;
use crate::model::{DiffFileStatus, RevisionSelection};
use crate::repository::Repository;
use crate::source_browse::{SourceEntry, SourceEntryStatus, SourceFileData};

/// Read the full old/new contents of one file at `revision`, for full-context
/// syntax highlighting (the old side comes from the first parent — matching
/// what the diff was computed against). Best-effort by design: absent, binary,
/// conflicted or oversized sides come back `None` and the caller falls back to
/// diff-only reconstruction. Read-only: no snapshot, no lock.
pub(crate) async fn read_jj_file_pair_inner(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
    path: &str,
    old_path: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let commit = resolve_browse_commit(repo, workspace_name, revision).await?;
    let new_tree = commit.tree();
    let old_tree = commit
        .parent_tree(repo)
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit.id().hex()))?;

    let new = read_jj_tree_file(repo, &new_tree, path).await;
    let old = read_jj_tree_file(repo, &old_tree, old_path.unwrap_or(path)).await;
    Ok((old, new))
}

/// Materialize one tree entry as text, `None` for anything full-context
/// highlighting can't use (absent, binary, conflicted, oversized, bad path).
pub(super) async fn read_jj_tree_file(
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
pub(crate) async fn resolve_browse_commit(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
) -> Result<Commit> {
    let commit_id = resolve_revision(repo, workspace_name, revision).await?;
    repo.store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))
}

/// List every path at `revision` for the source browser. A commit lists its
/// tree; the working copy walks the directory on disk so untracked/ignored
/// files appear too (classified via the same gitignore chain jj's snapshotter
/// uses), with each tracked file's diff status against the parents attached
/// for the status chips. See [`crate::source_browse::list_source_tree`].
pub(crate) async fn list_jj_source_tree(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<Vec<SourceEntry>> {
    let commit = resolve_browse_commit(repo, workspace_name, revision).await?;
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
    let changes = jj_change_statuses(repo, &commit).await?;

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
pub(super) async fn jj_change_statuses(
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
pub(super) fn walk_working_copy_dir(
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
pub(crate) async fn read_jj_source_file(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    repository: &Repository,
    revision: &RevisionSelection,
    path: &str,
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
        let bytes = std::fs::read(repository.root.join(path))
            .with_context(|| format!("failed to read {path}"))?;
        return Ok(crate::source_browse::classify_disk_bytes(bytes));
    }

    let commit = resolve_browse_commit(repo, workspace_name, revision).await?;
    let tree = commit.tree();
    let repo_path = RepoPathBuf::from_internal_string(path.to_owned())
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
