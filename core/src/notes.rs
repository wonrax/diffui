//! What review notes need from a jj repository, outside the repository actor:
//! the `diffui notes` CLI runs with the app closed, and the app re-anchors
//! notes on its own thread while the actor keeps the diff.
//!
//! A note is anchored to a change id, a path, a side and the text of its
//! lines, never to a commit: agents amend and rebase after every reply. A
//! [`NotesRepo`] finds what a change is now, reads its files, and
//! [`reattach`] finds the anchored lines in them again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use jj_lib::{
    backend::{ChangeId, CommitId},
    commit::Commit,
    object_id::ObjectId as _,
    repo::{ReadonlyRepo, Repo as _},
    workspace::Workspace,
};

use crate::jj;
use crate::jj::revset::RevsetScope;
use crate::repo::DiskSync;

/// Which side of a change a line is on: what the change removed or kept from
/// its parents (`Old`), or what it has now (`New`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Old,
    New,
}

/// One change as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeNow {
    /// Full, in jj's reverse hex.
    pub change_id: String,
    pub commit_id: String,
    /// The description's first line.
    pub title: String,
}

/// A jj workspace opened for notes: the repository at head, after folding the
/// working copy in when asked. Not `Send`: open it on the thread that uses it.
pub struct NotesRepo {
    runtime: tokio::runtime::Runtime,
    workspace: Workspace,
    repo: Arc<ReadonlyRepo>,
}

impl NotesRepo {
    /// Open the workspace at or above `path`. `snapshot` folds edits on disk
    /// into `@` first, as any jj command would, so a note an agent leaves on
    /// a file it just wrote sees the file; the app passes `false` and reads
    /// the repository its actor keeps current.
    pub fn open(path: &Path, snapshot: bool) -> Result<Self> {
        let root = workspace_root(path)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("start a runtime")?;
        let settings = jj::settings::jj_settings(&root)?;
        let mut workspace = jj::load_workspace(&settings, &root)?;
        let repo = runtime.block_on(async {
            if snapshot {
                let context = jj::SnapshotContext::load(&settings, &root)?;
                let outcome =
                    jj::snapshot_working_copy(&mut workspace, &context, DiskSync::Fold, false)
                        .await?;
                Ok::<_, anyhow::Error>(outcome.repo)
            } else {
                workspace
                    .repo_loader()
                    .load_at_head(Some(workspace.workspace_name()))
                    .await
                    .context("failed to load jj repo")
            }
        })?;
        Ok(Self {
            runtime,
            workspace,
            repo,
        })
    }

    /// The workspace's root directory.
    pub fn root(&self) -> &Path {
        self.workspace.workspace_root()
    }

    /// The repository every workspace of it shares (`.jj/repo` resolved),
    /// which is what notes are keyed by.
    pub fn repo_dir(&self) -> PathBuf {
        repo_dir(self.workspace.workspace_root())
            .unwrap_or_else(|| self.workspace.repo_path().to_owned())
    }

    /// The changes `revset` names, newest first, or the error in jj's words.
    pub fn resolve(&self, revset: &str) -> Result<Vec<ChangeNow>, String> {
        let scope = RevsetScope {
            repo: &self.repo,
            workspace_name: self.workspace.workspace_name(),
            repo_root: self.workspace.workspace_root(),
        };
        self.runtime.block_on(async {
            let expression = scope.parse(revset).map_err(|error| error.to_string())?;
            let ids = scope
                .ids(revset, &expression)
                .await
                .map_err(|error| error.to_string())?;
            let mut changes = Vec::with_capacity(ids.len());
            for id in ids {
                let commit = self.commit(&id).await.map_err(|error| error.to_string())?;
                changes.push(change_now(&commit));
            }
            Ok(changes)
        })
    }

    /// What `change_id` is now, or `None` when no visible commit has it (it
    /// was abandoned). A divergent change answers with its newest commit.
    pub fn change(&self, change_id: &str) -> Option<ChangeNow> {
        let id = ChangeId::try_from_reverse_hex(change_id)?;
        self.runtime.block_on(async {
            let targets = self.repo.resolve_change_id(&id).await.ok()??;
            let commit_id = targets.into_visible()?.into_iter().next()?;
            let commit = self.commit(&commit_id).await.ok()?;
            Some(change_now(&commit))
        })
    }

    /// `path`'s text on `side` of the commit `commit_id`, `None` when the
    /// file isn't there (or is binary, or isn't a commit of this repo).
    pub fn read(&self, commit_id: &str, path: &str, side: Side) -> Option<String> {
        let id = CommitId::try_from_hex(commit_id)?;
        self.runtime.block_on(async {
            let commit = self.commit(&id).await.ok()?;
            let tree = match side {
                Side::New => commit.tree(),
                Side::Old => commit.parent_tree(self.repo.index()).await.ok()?,
            };
            jj::browse::read_jj_tree_file(&self.repo, &tree, path).await
        })
    }

    /// `path`'s diff in the commit `commit_id` against its parents, as the
    /// diff pane draws it (word emphasis included), or `None` when the commit
    /// doesn't touch it.
    pub fn file_diff(&self, commit_id: &str, path: &str) -> Option<crate::model::DiffFile> {
        let repository = crate::repository::Repository {
            root: self.root().to_owned(),
            vcs: crate::repository::Vcs::Jj,
            scope: self.root().to_owned(),
        };
        self.runtime.block_on(async {
            let sides = jj::DiffSides::resolve(
                &self.repo,
                self.workspace.workspace_name(),
                self.root(),
                &crate::model::RevisionSelection::Commit(commit_id.to_owned()),
                &crate::model::DiffBase::Parents,
            )
            .await
            .ok()?;
            let expand = [path.to_owned()];
            let options = jj::DiffOptions {
                limits: Default::default(),
                expand: &expand,
                only: Some(path),
            };
            let document = jj::diff_jj(
                &self.repo,
                &sides,
                &repository,
                &options,
                &crate::repo::CancelFlag::default(),
            )
            .await
            .ok()?;
            document.files.into_iter().find(|file| file.path == path)
        })
    }

    async fn commit(&self, id: &CommitId) -> Result<Commit> {
        self.repo
            .store()
            .get_commit_async(id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))
    }
}

fn change_now(commit: &Commit) -> ChangeNow {
    ChangeNow {
        change_id: commit.change_id().reverse_hex(),
        commit_id: commit.id().hex(),
        title: commit
            .description()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// The repository the workspace at `root` shares (its `.jj/repo`,
/// followed and resolved as jj-lib's loader does), without loading anything:
/// what notes are keyed by.
pub fn repo_dir(root: &Path) -> Option<PathBuf> {
    let jj_dir = root.join(".jj");
    let mut repo = jj_dir.join("repo");
    if repo.is_file() {
        let bytes = std::fs::read(&repo).ok()?;
        repo = jj_dir.join(String::from_utf8(bytes).ok()?.trim_end_matches('\n'));
    }
    repo.canonicalize().ok().filter(|dir| dir.is_dir())
}

/// The jj workspace `path` is in: the nearest directory holding a `.jj`.
pub fn workspace_root(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("no such directory: {}", path.display()))?;
    path.ancestors()
        .find(|dir| dir.join(".jj").is_dir())
        .map(Path::to_owned)
        .with_context(|| format!("{} isn't in a jj repository", path.display()))
}

/// Where lines that read `text` and started at line `start` (1-based) are in
/// `file` now, or `None` when they're gone.
///
/// The same lines in the same place win. Otherwise the copy nearest `start`
/// does, matched exactly, then with each line's surrounding whitespace
/// ignored (a re-indent keeps the note). Lines that are all blank only stay
/// where they were: any other run of blank lines would match them.
pub fn reattach(text: &[String], start: usize, file: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    let lines: Vec<&str> = file.lines().collect();
    let at = |first: usize, same: &dyn Fn(&str, &str) -> bool| {
        text.iter()
            .enumerate()
            .all(|(offset, line)| lines.get(first + offset).is_some_and(|it| same(it, line)))
    };
    let exact = |a: &str, b: &str| a == b;
    let trimmed = |a: &str, b: &str| a.trim() == b.trim();
    let first = start.checked_sub(1)?;
    if at(first, &exact) {
        return Some(start);
    }
    if text.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    for same in [&exact as &dyn Fn(&str, &str) -> bool, &trimmed] {
        let nearest = (0..lines.len().saturating_sub(text.len() - 1))
            .filter(|&candidate| at(candidate, same))
            .min_by_key(|&candidate| candidate.abs_diff(first));
        if let Some(found) = nearest {
            return Some(found + 1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn lines_in_place_stay() {
        let file = "a\nb\nc\n";
        assert_eq!(reattach(&text(&["b", "c"]), 2, file), Some(2));
    }

    #[test]
    fn moved_lines_are_found_nearest_where_they_were() {
        let file = "x\nb\nnew\nnew\nnew\nb\n";
        assert_eq!(reattach(&text(&["b"]), 5, file), Some(6));
        assert_eq!(reattach(&text(&["b"]), 3, file), Some(2));
    }

    #[test]
    fn a_reindent_keeps_the_note() {
        let file = "fn f() {\n        let x = 1;\n}\n";
        assert_eq!(reattach(&text(&["    let x = 1;"]), 2, file), Some(2));
        assert_eq!(reattach(&text(&["    let x = 1;"]), 9, file), Some(2));
    }

    #[test]
    fn rewritten_lines_are_gone() {
        let file = "a\nB\nc\n";
        assert_eq!(reattach(&text(&["b"]), 2, file), None);
        assert_eq!(reattach(&text(&["b", "c"]), 2, ""), None);
    }

    #[test]
    fn blank_lines_never_move() {
        let file = "a\n\nb\n\n";
        assert_eq!(reattach(&text(&[""]), 2, file), Some(2));
        assert_eq!(reattach(&text(&[""]), 3, file), None);
    }
}
