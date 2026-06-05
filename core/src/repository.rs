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
}

#[derive(Debug, Clone, Copy)]
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
