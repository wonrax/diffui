use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Repository {
    pub root: PathBuf,
    pub vcs: Vcs,
    pub scope: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub fingerprint: String,
    /// Whether the post-snapshot working-copy commit is empty (no diff from its
    /// parent), or `None` when unknown (git — no cheap per-commit emptiness).
    /// Lets the sidebar keep @'s "empty" chip live on a watcher refresh even
    /// when the diff pane is showing a *different* revision (so the row can flip
    /// empty→non-empty without a graph re-walk); jj always fills it.
    pub working_copy_empty: Option<bool>,
    /// The op head the snapshot was taken *from* — the repo state before our
    /// own snapshot op (equal to `fingerprint` when the snapshot wrote no op).
    /// `None` for backends without an op log (git).
    ///
    /// This is how a watcher refresh tells "our own snapshot advanced the op"
    /// from "an external op (CLI edit/rebase) landed since the graph we show":
    /// if the fingerprint the graph reflects isn't this snapshot's parent, ops
    /// other than ours happened and the topology may have changed — the
    /// frontend escalates a diff-only watcher refresh to a full graph reload.
    pub parent_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Jj,
    Git,
}

/// What a fetch should pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    /// Every remote, all branches (`jj git fetch --all-remotes` /
    /// `git fetch --all`).
    AllRemotes,
    /// One branch on one remote (`name@remote`).
    RemoteBranch { remote: String, branch: String },
}

pub fn prepare_repository(input: &Path) -> Result<Repository> {
    let target = normalize_input_path(input)?;
    let search_start = if target.is_file() {
        target
            .parent()
            .context("target file has no parent directory")?
            .to_path_buf()
    } else {
        target.clone()
    };

    let (root, vcs) = discover_repository(&search_start).with_context(|| {
        format!(
            "could not find a jj or git repository above {}",
            search_start.display()
        )
    })?;

    let scope = target
        .strip_prefix(&root)
        .unwrap_or(target.as_path())
        .to_path_buf();

    Ok(Repository { root, vcs, scope })
}

fn normalize_input_path(input: &Path) -> Result<PathBuf> {
    let input = if input.is_absolute() {
        input.to_path_buf()
    } else {
        env::current_dir()
            .context("failed to read current directory")?
            .join(input)
    };

    input
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", input.display()))
}

/// The directory holding the jj repo backing `root`'s workspace: `.jj/repo`
/// itself, or — for a secondary workspace created by `jj workspace add`, whose
/// `.jj/repo` is a *file* — the directory it points at (contents are a path
/// relative to the workspace's `.jj` dir). Mirrors jj-lib's workspace loader,
/// which reads the pointer bytes verbatim and canonicalizes the joined path.
///
/// Lives here (not `jj.rs`) because the fs watcher needs it too: a secondary
/// workspace's op log lives in the primary repo, outside the workspace root.
pub fn resolve_jj_repo_dir(root: &Path) -> Result<PathBuf> {
    let jj_dir = root.join(".jj");
    let repo_entry = jj_dir.join("repo");
    if !repo_entry.is_file() {
        return Ok(repo_entry);
    }
    let buf = std::fs::read(&repo_entry)
        .with_context(|| format!("failed to read jj repo pointer {}", repo_entry.display()))?;
    #[cfg(unix)]
    let pointer = {
        use std::os::unix::ffi::OsStrExt as _;
        std::ffi::OsStr::from_bytes(&buf).to_owned()
    };
    #[cfg(not(unix))]
    let pointer = std::ffi::OsString::from(String::from_utf8_lossy(&buf).into_owned());
    let pointee = jj_dir.join(&pointer);
    pointee
        .canonicalize()
        .with_context(|| format!("failed to resolve jj repo dir {}", pointee.display()))
}

fn discover_repository(start: &Path) -> Result<(PathBuf, Vcs)> {
    for directory in start.ancestors() {
        if directory.join(".jj").is_dir() {
            return Ok((directory.to_path_buf(), Vcs::Jj));
        }

        if directory.join(".git").exists() {
            return Ok((directory.to_path_buf(), Vcs::Git));
        }
    }

    bail!("not inside a repository")
}
