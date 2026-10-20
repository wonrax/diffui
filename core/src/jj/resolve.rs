//! Conflict resolution against jj-lib — the `jj resolve` half of
//! [`crate::resolve`].

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use bstr::BString;
use jj_lib::{
    backend::{CopyId, MergedTreeValueExt as _, TreeValue},
    commit::Commit,
    conflicts::{
        ConflictMarkerStyle, ConflictMaterializeOptions, MIN_CONFLICT_MARKER_LEN,
        MaterializedFileConflictValue, choose_materialized_conflict_marker_len,
        materialize_merge_result_to_bytes, try_materialize_file_conflict_value,
        update_from_content,
    },
    files::{self, MergeResult},
    merge::Merge,
    merged_tree::MergedTree,
    merged_tree_builder::MergedTreeBuilder,
    ref_name::WorkspaceName,
    repo::{MutableRepo, ReadonlyRepo, Repo},
    repo_path::RepoPath,
    settings::UserSettings,
};

use super::partial::repo_path;
use super::workspace::resolve_revision;
use crate::model::RevisionSelection;
use crate::resolve::{
    ConflictFile, ConflictPick, ConflictResolution, ConflictTerm, MergeToolInvocation,
    merge_tool_from_settings,
};

/// The conflicted path's tree value and its file-level view, when every term
/// is a file or absent.
async fn conflict_at(
    tree: &MergedTree,
    path: &RepoPath,
    label: &str,
) -> Result<(
    Merge<Option<TreeValue>>,
    Option<MaterializedFileConflictValue>,
)> {
    let value = tree
        .path_value(path)
        .await
        .with_context(|| format!("failed to read {label}"))?;
    let conflict = match value.into_resolved() {
        Err(conflict) => conflict,
        Ok(_) => bail!("{label} is not conflicted in this revision"),
    };
    let file = try_materialize_file_conflict_value(tree.store(), path, &conflict, tree.labels())
        .await
        .with_context(|| format!("failed to read the conflict in {label}"))?;
    Ok((conflict, file))
}

fn term_label(label: Option<&str>, kind: &str, index: usize) -> String {
    match label.map(str::trim).filter(|label| !label.is_empty()) {
        Some(label) => label.to_owned(),
        None => format!("{kind} #{}", index + 1),
    }
}

/// Every conflicted path of `commit`, labeled for the resolve actions.
pub(crate) async fn conflict_files(commit: &Commit) -> Result<Vec<ConflictFile>> {
    let tree = commit.tree();
    if !tree.has_conflict() {
        return Ok(Vec::new());
    }
    let paths: Vec<_> = tree.conflicts().map(|(path, _)| path).collect();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let label = path.as_internal_file_string().to_owned();
        let (conflict, file) = conflict_at(&tree, &path, &label).await?;
        let entry = match file {
            Some(file) => {
                let terms = |kind: &str, ids: Vec<bool>, get: &dyn Fn(usize) -> Option<String>| {
                    ids.into_iter()
                        .enumerate()
                        .map(|(index, absent)| ConflictTerm {
                            label: term_label(get(index).as_deref(), kind, index),
                            absent,
                        })
                        .collect::<Vec<_>>()
                };
                let side_absent: Vec<bool> = file.ids.adds().map(Option::is_none).collect();
                let base_absent: Vec<bool> = file.ids.removes().map(Option::is_none).collect();
                let sides = terms("side", side_absent, &|index| {
                    file.labels.get_add(index).map(str::to_owned)
                });
                let bases = terms("base", base_absent, &|index| {
                    file.labels.get_remove(index).map(str::to_owned)
                });
                let blocker = if file.ids.num_sides() > 2 {
                    Some(format!(
                        "a {}-sided conflict. Merge tools take two sides; pick a side \
                         or edit the conflict markers in the working copy",
                        file.ids.num_sides()
                    ))
                } else if file.executable.is_none() {
                    Some("the sides disagree on the executable bit".to_owned())
                } else {
                    None
                };
                ConflictFile {
                    path: label,
                    sides,
                    bases,
                    pickable: true,
                    merge_tool_blocker: blocker,
                    regions: region_count(&file),
                }
            }
            None => {
                let labels = tree.labels();
                ConflictFile {
                    path: label,
                    sides: (0..conflict.num_sides())
                        .map(|index| ConflictTerm {
                            label: term_label(labels.get_add(index), "side", index),
                            absent: conflict.get_add(index).is_some_and(Option::is_none),
                        })
                        .collect(),
                    bases: (0..conflict.num_sides().saturating_sub(1))
                        .map(|index| ConflictTerm {
                            label: term_label(labels.get_remove(index), "base", index),
                            absent: conflict.get_remove(index).is_some_and(Option::is_none),
                        })
                        .collect(),
                    pickable: false,
                    merge_tool_blocker: Some(
                        "a term is a symlink, directory or submodule. Resolve it in the \
                         working copy"
                            .to_owned(),
                    ),
                    regions: 0,
                }
            }
        };
        files.push(entry);
    }
    Ok(files)
}

/// How many conflict regions `file` shows in the diff, when they can be
/// resolved one at a time: not when a term is absent (taking the deleting
/// side of one region would leave an empty file, not a deleted one) or
/// binary.
fn region_count(file: &MaterializedFileConflictValue) -> usize {
    if file.ids.iter().any(Option::is_none)
        || file.contents.iter().any(|content| content.contains(&0))
    {
        return 0;
    }
    match files::merge_hunks(&file.contents, &super::diff::conflict_merge_options()) {
        MergeResult::Resolved(_) => 0,
        MergeResult::Conflict(hunks) => hunks.iter().filter(|hunk| !hunk.is_resolved()).count(),
    }
}

/// `contents` with conflict region `region` (0-based, of `regions`) taken
/// from `pick` in every term, and the other regions left as they were.
/// Regions are cut the way the diff cuts them, so `region` is the
/// `conflict k of n` the user saw.
pub(crate) fn resolve_region(
    contents: &Merge<BString>,
    region: usize,
    regions: usize,
    pick: ConflictPick,
) -> Result<Merge<BString>> {
    let hunks = match files::merge_hunks(contents, &super::diff::conflict_merge_options()) {
        MergeResult::Conflict(hunks) => hunks,
        MergeResult::Resolved(_) => bail!("the file has no conflict regions left"),
    };
    let found = hunks.iter().filter(|hunk| !hunk.is_resolved()).count();
    if found != regions {
        bail!("the conflict changed ({found} regions, not {regions}). Resolve it again.");
    }
    if region >= found {
        bail!("there is no conflict {} of {found}", region + 1);
    }
    let mut terms: Vec<Vec<u8>> = contents.iter().map(|_| Vec::new()).collect();
    let mut conflicts = 0;
    for hunk in &hunks {
        let taken = match hunk.as_resolved() {
            Some(text) => Some(text),
            None if conflicts == region => {
                conflicts += 1;
                let text = match pick {
                    ConflictPick::Side(side) => hunk.get_add(side),
                    ConflictPick::Base(base) => hunk.get_remove(base),
                };
                Some(text.ok_or_else(|| anyhow!("conflict {conflicts} has no {pick:?}"))?)
            }
            None => {
                conflicts += 1;
                None
            }
        };
        match taken {
            Some(text) => terms
                .iter_mut()
                .for_each(|term| term.extend_from_slice(text)),
            None => {
                for (term, text) in terms.iter_mut().zip(hunk.iter()) {
                    term.extend_from_slice(text);
                }
            }
        }
    }
    Ok(Merge::from_vec(
        terms.into_iter().map(BString::from).collect::<Vec<_>>(),
    ))
}

/// `jj resolve` on one path of `commit`, writing the rewritten commit.
pub(super) async fn resolve_conflict(
    repo: &mut MutableRepo,
    commit: &Commit,
    path: &str,
    resolution: &ConflictResolution,
) -> Result<Commit> {
    let tree = commit.tree();
    let repo_path = repo_path(path)?;
    let (conflict, file) = conflict_at(&tree, &repo_path, path).await?;
    let file = file.ok_or_else(|| {
        anyhow!(
            "{path}'s conflict involves a symlink, directory or submodule. Resolve it in \
             the working copy"
        )
    })?;
    let executables = conflict
        .to_executable_merge()
        .ok_or_else(|| anyhow!("{path} is not a file conflict"))?;
    let file_value = |id: Option<jj_lib::backend::FileId>, executable: bool| {
        Merge::resolved(id.map(|id| TreeValue::File {
            id,
            executable,
            copy_id: CopyId::placeholder(),
        }))
    };
    let new_value = match resolution {
        ConflictResolution::Side(index) => {
            let id = file
                .ids
                .get_add(*index)
                .ok_or_else(|| anyhow!("{path} has no side {}", index + 1))?
                .clone();
            let executable = side_executable(&executables, &file, |m| m.get_add(*index));
            file_value(id, executable)
        }
        ConflictResolution::Base(index) => {
            let id = file
                .ids
                .get_remove(*index)
                .ok_or_else(|| anyhow!("{path} has no base {}", index + 1))?
                .clone();
            let executable = side_executable(&executables, &file, |m| m.get_remove(*index));
            file_value(id, executable)
        }
        ConflictResolution::Content {
            content,
            marker_len,
        } => {
            let store = tree.store();
            let ids = match marker_len {
                Some(marker_len) => update_from_content(
                    &file.unsimplified_ids,
                    store,
                    &repo_path,
                    content,
                    *marker_len,
                )
                .await
                .with_context(|| format!("failed to parse the merged {path}"))?,
                None => Merge::normal(
                    store
                        .write_file(&repo_path, &mut content.as_slice())
                        .await
                        .with_context(|| format!("failed to write the merged {path}"))?,
                ),
            };
            match ids.into_resolved() {
                Ok(id) => file_value(id, file.executable.unwrap_or(false)),
                Err(ids) => conflict.with_new_file_ids(&ids),
            }
        }
        ConflictResolution::Region {
            region,
            regions,
            pick,
        } => {
            if region_count(&file) == 0 {
                bail!("{path} can only be resolved whole");
            }
            let terms = resolve_region(&file.contents, *region, *regions, *pick)
                .with_context(|| format!("can't resolve {path} by region"))?;
            let store = tree.store();
            if *regions == 1 {
                // The last region: every term now reads the same.
                let content = terms.first();
                let id = store
                    .write_file(&repo_path, &mut content.as_slice())
                    .await
                    .with_context(|| format!("failed to write the resolved {path}"))?;
                file_value(Some(id), file.executable.unwrap_or(false))
            } else {
                let mut ids = Vec::new();
                for content in terms.iter() {
                    ids.push(Some(
                        store
                            .write_file(&repo_path, &mut content.as_slice())
                            .await
                            .with_context(|| format!("failed to write {path}"))?,
                    ));
                }
                let ids = Merge::from_vec(ids);
                // Back to the shape of the tree's conflict when jj simplified
                // it for materializing.
                let ids = if ids.iter().len() == file.unsimplified_ids.iter().len() {
                    ids
                } else {
                    file.unsimplified_ids.clone().update_from_simplified(ids)
                };
                conflict.with_new_file_ids(&ids)
            }
        }
    };
    let mut builder = MergedTreeBuilder::new(tree);
    builder.set_or_remove(repo_path, new_value);
    let new_tree = builder
        .write_tree()
        .await
        .context("failed to write the resolved tree")?;
    repo.rewrite_commit(commit)
        .set_tree(new_tree)
        .write()
        .await
        .context("failed to write the resolved revision")
}

/// A picked term's executable bit: its own when the (unsimplified) merge
/// lines up with the simplified one, else the merged bit.
fn side_executable(
    executables: &Merge<Option<bool>>,
    file: &MaterializedFileConflictValue,
    pick: impl Fn(&Merge<Option<bool>>) -> Option<&Option<bool>>,
) -> bool {
    if executables.num_sides() == file.ids.num_sides()
        && let Some(Some(executable)) = pick(executables)
    {
        return *executable;
    }
    file.executable.unwrap_or(false)
}

/// `path` at `revision` with jj's conflict markers, as `jj` writes it into a
/// working copy (`ui.conflict-marker-style`), and the marker length to parse
/// an edit of it back with ([`ConflictResolution::Content`]).
pub(crate) async fn conflict_text(
    repo: &ReadonlyRepo,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
    path: &str,
) -> Result<(String, usize)> {
    let commit_id = resolve_revision(repo, workspace_name, revision).await?;
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .context("failed to load the conflicted revision")?;
    let repo_path = repo_path(path)?;
    let (_, file) = conflict_at(&commit.tree(), &repo_path, path).await?;
    let file = file
        .ok_or_else(|| anyhow!("{path}'s conflict involves a symlink, directory or submodule"))?;
    let marker_len = choose_materialized_conflict_marker_len(&file.contents);
    let options = ConflictMaterializeOptions {
        marker_style: settings
            .get("ui.conflict-marker-style")
            .unwrap_or(ConflictMarkerStyle::Diff),
        marker_len: Some(marker_len),
        merge: repo.store().merge_options().clone(),
    };
    let bytes = materialize_merge_result_to_bytes(&file.contents, &file.labels, &options);
    let text = String::from_utf8(bytes.into())
        .map_err(|_| anyhow!("{path} isn't text, so it can't be edited here"))?;
    Ok((text, marker_len))
}

/// Write the merge tool's input files for `path` at `revision` and build its
/// command line from the user's `ui.merge-editor`. Read-only on the repo.
pub(crate) async fn prepare_merge_tool(
    repo: &ReadonlyRepo,
    settings: &UserSettings,
    workspace_name: &WorkspaceName,
    revision: &RevisionSelection,
    path: &str,
) -> Result<MergeToolInvocation> {
    let tool = merge_tool_from_settings(settings).map_err(|message| anyhow!(message))?;
    let commit_id = resolve_revision(repo, workspace_name, revision).await?;
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .context("failed to load the conflicted revision")?;
    let tree = commit.tree();
    let repo_path = repo_path(path)?;
    let (_, file) = conflict_at(&tree, &repo_path, path).await?;
    let file = file
        .ok_or_else(|| anyhow!("{path}'s conflict involves a symlink, directory or submodule"))?;
    if file.contents.num_sides() != 2 {
        bail!(
            "{path} is a {}-sided conflict, and merge tools take two sides. Pick a side or \
             edit the conflict markers in the working copy",
            file.contents.num_sides()
        );
    }
    if file.executable.is_none() {
        bail!("{path}'s sides disagree on the executable bit");
    }

    let marker_len = if tool.edits_markers || tool.uses_marker_length() {
        choose_materialized_conflict_marker_len(&file.contents)
    } else {
        MIN_CONFLICT_MARKER_LEN
    };
    let initial_output = if tool.edits_markers {
        let marker_style = tool
            .marker_style
            .or_else(|| settings.get("ui.conflict-marker-style").ok())
            .unwrap_or(ConflictMarkerStyle::Diff);
        let options = ConflictMaterializeOptions {
            marker_style,
            marker_len: Some(marker_len),
            merge: repo.store().merge_options().clone(),
        };
        materialize_merge_result_to_bytes(&file.contents, &file.labels, &options).to_vec()
    } else {
        Vec::new()
    };

    let dir = scratch_dir()?;
    let suffix = repo_path
        .components()
        .next_back()
        .map(|name| format!("_{}", name.as_internal_str()))
        .unwrap_or_default();
    let contents: [(&str, &[u8]); 4] = [
        (
            "base",
            file.contents.get_remove(0).expect("2-sided").as_slice(),
        ),
        (
            "left",
            file.contents.get_add(0).expect("2-sided").as_slice(),
        ),
        (
            "right",
            file.contents.get_add(1).expect("2-sided").as_slice(),
        ),
        ("output", initial_output.as_slice()),
    ];
    let mut variables: HashMap<&str, String> = HashMap::new();
    for (role, content) in contents {
        let file_path = dir.join(format!("{role}{suffix}"));
        std::fs::write(&file_path, content)
            .with_context(|| format!("failed to write {}", file_path.display()))?;
        if role != "output" {
            let mut permissions = std::fs::metadata(&file_path)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&file_path, permissions)?;
        }
        variables.insert(role, file_path.display().to_string());
    }
    variables.insert("marker_length", marker_len.to_string());
    variables.insert("path", path.to_owned());

    Ok(MergeToolInvocation {
        args: jj_cli::config::interpolate_variables(&tool.merge_args, &variables),
        output: dir.join(format!("output{suffix}")),
        dir,
        tool: tool.name,
        program: tool.program,
        initial_output,
        conflict_exit_codes: tool.conflict_exit_codes,
        marker_len,
        edits_markers: tool.edits_markers,
        // Resolve against the change as it stands when the tool is done: the
        // working copy keeps being snapshotted while the tool is open.
        revision: match revision {
            RevisionSelection::WorkingCopy => RevisionSelection::WorkingCopy,
            RevisionSelection::Commit(_) => {
                use jj_lib::object_id::ObjectId as _;
                RevisionSelection::Commit(commit.id().hex())
            }
        },
        path: path.to_owned(),
    })
}

/// A fresh, private scratch directory under the system temp dir.
fn scratch_dir() -> Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("diffui-resolve-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}
