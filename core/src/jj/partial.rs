//! Partial-change mutations against jj-lib: `jj split`, `jj squash` and
//! `jj restore` restricted to a [`DiffSelection`].
//!
//! Everything starts from the same building block jj-cli's diff editors
//! produce — a *selected tree*: the parent tree with just the picked changes
//! applied — and hands it to the same jj-lib machinery the CLI uses
//! ([`squash_commits`], `transform_descendants`), so the results match the
//! CLI's for the same selection.

use anyhow::{Context, Result, anyhow, bail};
use jj_lib::{
    backend::{ChangeId, CommitId, CopyId, FileId, TreeValue},
    commit::Commit,
    merge::Merge,
    merged_tree::MergedTree,
    merged_tree_builder::MergedTreeBuilder,
    repo::{MutableRepo, Repo},
    repo_path::{RepoPath, RepoPathBuf},
    rewrite::{CommitWithSelection, squash_commits},
    store::Store,
};

use super::mutate::{combined_squash_description, short_change_id};
use crate::partial::{DiffSelection, HunkPick, apply_hunks, pick_mask, text_hunks};

/// A resolved regular file at one side of a hunk-level pick.
struct PlainFile {
    id: FileId,
    executable: bool,
    copy_id: CopyId,
}

pub(super) fn repo_path(path: &str) -> Result<RepoPathBuf> {
    RepoPathBuf::from_internal_string(path.to_owned())
        .map_err(|error| anyhow!("invalid repository path {path}: {error}"))
}

/// The value at `path` as a plain file (or absent). Hunk-level picks only
/// make sense for text files; everything else has to be taken whole.
async fn plain_file(tree: &MergedTree, path: &RepoPath, label: &str) -> Result<Option<PlainFile>> {
    let value = tree
        .path_value(path)
        .await
        .with_context(|| format!("failed to read {label}"))?;
    match value.into_resolved() {
        Ok(None) => Ok(None),
        Ok(Some(TreeValue::File {
            id,
            executable,
            copy_id,
        })) => Ok(Some(PlainFile {
            id,
            executable,
            copy_id,
        })),
        Ok(Some(_)) => bail!("{label} is not a regular file — select the whole file instead"),
        Err(_) => bail!("{label} is conflicted — select the whole file instead"),
    }
}

pub(super) async fn read_file(store: &Store, path: &RepoPath, id: &FileId) -> Result<Vec<u8>> {
    use futures::AsyncReadExt as _;
    let mut reader = store
        .read_file(path, id)
        .await
        .with_context(|| format!("failed to open {}", path.as_internal_file_string()))?;
    let mut content = Vec::new();
    reader
        .read_to_end(&mut content)
        .await
        .with_context(|| format!("failed to read {}", path.as_internal_file_string()))?;
    Ok(content)
}

/// Build a tree from `base` with the selected changes of `parent → commit`
/// either applied (`apply_selected`, base = the parent tree: the "selected
/// tree" of `jj split -i`) or reverted (base = the commit's tree: what
/// `jj restore -i` leaves behind).
pub(super) async fn tree_with_selection(
    base: &MergedTree,
    parent: &MergedTree,
    commit: &MergedTree,
    selection: &DiffSelection,
    apply_selected: bool,
) -> Result<MergedTree> {
    let store = commit.store().clone();
    let mut builder = MergedTreeBuilder::new(base.clone());
    for file in &selection.files {
        let path = repo_path(&file.path)?;
        let refs = match &file.hunks {
            HunkPick::All => {
                let source = if apply_selected { commit } else { parent };
                let value = source
                    .path_value(&path)
                    .await
                    .with_context(|| format!("failed to read {}", file.path))?;
                builder.set_or_remove(path, value);
                continue;
            }
            HunkPick::Hunks(refs) if refs.is_empty() => continue,
            HunkPick::Hunks(refs) => refs,
        };
        let before = plain_file(parent, &path, &file.path).await?;
        let after = plain_file(commit, &path, &file.path).await?;
        let executable = match (&before, &after) {
            (Some(before), Some(after)) if before.executable != after.executable => bail!(
                "{} also changes its executable bit — select the whole file instead",
                file.path
            ),
            (_, Some(after)) => after.executable,
            (Some(before), None) => before.executable,
            (None, None) => continue,
        };
        let before_content = match &before {
            Some(before) => read_file(&store, &path, &before.id).await?,
            None => Vec::new(),
        };
        let after_content = match &after {
            Some(after) => read_file(&store, &path, &after.id).await?,
            None => Vec::new(),
        };
        let hunks = text_hunks(&before_content, &after_content);
        let mask = pick_mask(&file.path, &hunks, refs).map_err(|message| anyhow!(message))?;
        if mask.iter().all(|&picked| picked) {
            // Every hunk: the file's whole value, so a deletion deletes rather
            // than leaving an empty file behind.
            let source = if apply_selected { commit } else { parent };
            let value = source
                .path_value(&path)
                .await
                .with_context(|| format!("failed to read {}", file.path))?;
            builder.set_or_remove(path, value);
            continue;
        }
        let content = apply_hunks(&before_content, &after_content, &hunks, |index| {
            mask[index] == apply_selected
        });
        let id = store
            .write_file(&path, &mut content.as_slice())
            .await
            .with_context(|| format!("failed to write {}", file.path))?;
        let copy_id = after
            .map(|file| file.copy_id)
            .or(before.map(|file| file.copy_id))
            .unwrap_or_else(CopyId::placeholder);
        builder.set_or_remove(
            path,
            Merge::normal(TreeValue::File {
                id,
                executable,
                copy_id,
            }),
        );
    }
    builder
        .write_tree()
        .await
        .context("failed to write the selected tree")
}

/// The commit a change id now names, if exactly one visible commit has it —
/// how a caller viewing a rewritten commit follows it to its new id.
pub(super) async fn visible_commit_of(repo: &MutableRepo, change: &ChangeId) -> Option<CommitId> {
    let targets = repo.resolve_change_id(change).await.ok().flatten()?;
    let mut visible = targets.visible_with_offsets().map(|(_, id)| id.clone());
    let first = visible.next()?;
    visible.next().is_none().then_some(first)
}

/// `jj split`: `selection` stays in `commit`, the rest goes to a new child.
/// Returns `(first, second)`.
pub(super) async fn split(
    repo: &mut MutableRepo,
    commit: &Commit,
    selection: &DiffSelection,
    first_description: Option<&str>,
    second_description: &str,
) -> Result<(Commit, Commit)> {
    let parent_tree = commit
        .parent_tree(repo)
        .await
        .context("failed to load the split revision's parent tree")?;
    let selected =
        tree_with_selection(&parent_tree, &parent_tree, &commit.tree(), selection, true).await?;
    if selected.tree_ids() == parent_tree.tree_ids() {
        bail!("nothing selected — the new revision would take every change");
    }
    // Workspaces whose `@` is the split commit follow the remaining changes,
    // as `jj split` does. Read before anything is rewritten.
    let workspaces: Vec<_> = repo
        .view()
        .wc_commit_ids()
        .iter()
        .filter(|(_, id)| *id == commit.id())
        .map(|(name, _)| name.clone())
        .collect();

    let mut first = repo.rewrite_commit(commit).set_tree(selected);
    if let Some(description) = first_description {
        first = first.set_description(description);
    }
    let first = first
        .write()
        .await
        .context("failed to write the selected changes")?;
    let second = repo
        .rewrite_commit(commit)
        .set_parents(vec![first.id().clone()])
        .set_tree(commit.tree())
        // A new change id, so the split commit doesn't become divergent.
        .clear_rewrite_source()
        .generate_new_change_id()
        .set_description(second_description)
        .write()
        .await
        .context("failed to write the remaining changes")?;
    repo.transform_descendants(
        vec![commit.id().clone()],
        async |mut rewriter: jj_lib::rewrite::CommitRewriter<'_>| {
            rewriter.replace_parent(first.id(), [second.id()]);
            rewriter.rebase().await?.write().await?;
            Ok(())
        },
    )
    .await
    .context("failed to rebase the split revision's descendants")?;
    for name in workspaces {
        repo.edit(name, &second)
            .await
            .context("failed to move the working copy to the remaining changes")?;
    }
    Ok((first, second))
}

/// What a partial squash did.
pub(super) struct PartialSquash {
    /// The rewritten destination.
    pub destination: Commit,
    /// The selection took everything, so the source was abandoned.
    pub abandoned_source: bool,
}

/// `jj squash --from source --into destination <selection>`.
pub(super) async fn squash_selection(
    repo: &mut MutableRepo,
    source: &Commit,
    destination: &Commit,
    selection: &DiffSelection,
) -> Result<PartialSquash> {
    let parent_tree = source
        .parent_tree(repo)
        .await
        .context("failed to load the squash source's parent tree")?;
    let selected_tree =
        tree_with_selection(&parent_tree, &parent_tree, &source.tree(), selection, true).await?;
    let picked = CommitWithSelection {
        commit: source.clone(),
        selected_tree,
        parent_tree,
    };
    if picked.is_empty_selection() {
        bail!(
            "nothing to move from {} — the selection changes nothing",
            short_change_id(source)
        );
    }
    let Some(squashed) = squash_commits(repo, std::slice::from_ref(&picked), destination, false)
        .await
        .context("failed to squash")?
    else {
        bail!("nothing to move from {}", short_change_id(source));
    };
    let abandoned_source = !squashed.abandoned_commits.is_empty();
    let mut builder = squashed.commit_builder;
    if abandoned_source {
        // Only a fully moved source is abandoned, and only then does its
        // description travel with it (`jj squash`'s combine rule).
        builder = builder.set_description(combined_squash_description(
            destination,
            &squashed.abandoned_commits,
        ));
    }
    let destination = builder
        .write()
        .await
        .context("failed to write the squash destination")?;
    Ok(PartialSquash {
        destination,
        abandoned_source,
    })
}

/// `jj restore --changes-in commit <selection>`: the commit with the picked
/// changes put back to the parent's content.
pub(super) async fn restore_selection(
    repo: &mut MutableRepo,
    commit: &Commit,
    selection: &DiffSelection,
) -> Result<Commit> {
    let parent_tree = commit
        .parent_tree(repo)
        .await
        .context("failed to load the parent tree")?;
    let tree = commit.tree();
    let restored = tree_with_selection(&tree, &parent_tree, &tree, selection, false).await?;
    if restored.tree_ids() == tree.tree_ids() {
        bail!("nothing to discard — the selection changes nothing");
    }
    repo.rewrite_commit(commit)
        .set_tree(restored)
        .write()
        .await
        .context("failed to write the restored revision")
}
