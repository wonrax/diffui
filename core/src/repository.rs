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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// The working-copy commit id after the snapshot. A snapshot that wrote an
    /// op rewrote `@`, so its old id is gone; the projection patches the row
    /// with this instead of re-walking. `None` for git.
    pub working_copy_commit_id: Option<String>,
    /// Whether the snapshot rebased descendants of `@` (the user is editing a
    /// commit with children). Their commit ids all changed, so the rows on
    /// screen address commits that are no longer visible and the graph has to
    /// be reloaded rather than patched.
    pub rewrote_descendants: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Jj,
    Git,
}

/// What a fetch should pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchTarget {
    /// `jj git fetch`: the remotes `git.fetch` names, else `origin` (or the
    /// only remote), with each remote's configured bookmarks and tags
    /// (`git fetch` for a plain git repo).
    Default,
    /// Every remote, each with its configured bookmarks and tags
    /// (`jj git fetch --all-remotes` / `git fetch --all`).
    AllRemotes,
    /// One branch on one remote (`name@remote`), no tags.
    RemoteBranch { remote: String, branch: String },
}

/// What a finished fetch reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchOutcome {
    /// The remotes fetched from, in order.
    pub remotes: Vec<String>,
    /// Status lines in jj's words (what was imported, abandoned, rebased),
    /// then git's own output.
    pub output: Vec<String>,
    /// jj's warnings, in its words, without the `Warning: ` prefix.
    pub warnings: Vec<String>,
    /// Whether the fetch changed the repo. jj says "Nothing changed." and
    /// writes no operation otherwise.
    pub changed: bool,
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

#[derive(Debug, Clone)]
pub(crate) struct GitDirectories {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

pub(crate) fn resolve_git_dirs(root: &Path) -> Result<GitDirectories> {
    let entry = root.join(".git");
    let git_dir = if entry.is_file() {
        let bytes = std::fs::read(&entry).context("failed to read gitdir pointer")?;
        let pointer = bytes
            .strip_prefix(b"gitdir: ")
            .context("invalid gitdir pointer")?;
        root.join(git_pointer_path(pointer))
    } else {
        entry
    }
    .canonicalize()
    .context("failed to resolve git directory")?;
    let common_entry = git_dir.join("commondir");
    let common_dir = if common_entry.is_file() {
        let pointer = std::fs::read(&common_entry).context("failed to read commondir pointer")?;
        git_dir
            .join(git_pointer_path(&pointer))
            .canonicalize()
            .context("failed to resolve common git directory")?
    } else {
        git_dir.clone()
    };
    Ok(GitDirectories {
        git_dir,
        common_dir,
    })
}

fn git_pointer_path(bytes: &[u8]) -> PathBuf {
    let bytes = bytes.trim_ascii_end();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
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
