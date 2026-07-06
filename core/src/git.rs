use std::{ffi::OsString, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use jj_lib::graph::GraphEdge;
use tokio::process::Command;

use crate::FetchTarget;
use crate::diff_parse::parse_unified_diff;
use crate::graph::assign_lanes;
use crate::graph_layout::{GraphLayout, GraphLayoutBuilder};
use crate::model::{
    CommitSummary, DiffDocument, RevisionDetails, RevisionSelection, SignatureInfo,
};
use crate::repository::{Repository, RepositorySnapshot};

/// Sentinel commit_id used for the synthetic git working-copy row.
/// Selection short-circuits on `is_working_copy` before this value is ever
/// passed to git, so it just needs to be visually distinct and not collide
/// with a real hex hash.
pub const GIT_WORKING_COPY_ID: &str = "wc";

pub async fn load_git_diff(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let args = git_backend_command(repository, revision);
    let output = run_command(&repository.root, "git", args).await?;
    let document = parse_unified_diff(&output);
    let details = load_git_revision_details(repository, revision).await.ok();
    Ok((document, details))
}

pub async fn load_git_commits(
    repository: &Repository,
    revision_range: &str,
) -> Result<(Vec<CommitSummary>, GraphLayout)> {
    // The revision range is the git analog of the jj revset: extra `git log`
    // arguments (e.g. `--all`, `main..HEAD`). Empty keeps the default (the
    // current branch's history).
    let mut args = vec![OsString::from("log"), OsString::from("--topo-order")];
    args.extend(revision_range.split_whitespace().map(OsString::from));
    args.push(OsString::from(
        "--pretty=format:%h%x09%H%x09%P%x09%an%x09%x09%s",
    ));
    let output = run_command(&repository.root, "git", args).await?;

    let mut rows = parse_commit_log_rows(&output);
    // Git has no native @ commit, so synthesize a working-copy row at
    // the top whenever the tree differs from HEAD. The row's parent
    // is HEAD so the graph keeps a continuous lane.
    if !rows.is_empty() && git_has_uncommitted_changes(&repository.root).await? {
        let head_id = rows[0].commit_id.clone();
        rows.insert(
            0,
            ParsedCommitRow {
                change_id: GIT_WORKING_COPY_ID.to_owned(),
                commit_id: GIT_WORKING_COPY_ID.to_owned(),
                parents: vec![head_id],
                author: String::new(),
                is_empty: None,
                description: "Working copy".to_owned(),
                has_description: true,
                is_working_copy: true,
            },
        );
    }
    Ok(build_commit_summaries(rows))
}

pub async fn load_git_repository_snapshot(repository_root: &Path) -> Result<RepositorySnapshot> {
    let output = run_command(
        repository_root,
        "git",
        vec![
            OsString::from("status"),
            OsString::from("--porcelain=v1"),
            OsString::from("--branch"),
            OsString::from("--untracked-files=normal"),
        ],
    )
    .await?;
    Ok(RepositorySnapshot {
        fingerprint: output,
        working_copy_empty: None,
        // Git has no op log — the external-op escalation never applies.
        parent_fingerprint: None,
    })
}

fn git_backend_command(repository: &Repository, revision: &RevisionSelection) -> Vec<OsString> {
    let mut args: Vec<OsString> = match revision {
        RevisionSelection::WorkingCopy => {
            // `git diff HEAD` covers both staged and unstaged changes
            // against the last committed state — the closest analog to
            // jj's @ working-copy diff. Untracked files are not included
            // (git diff only walks tracked paths).
            ["diff", "HEAD", "--no-ext-diff", "--no-color", "--"]
                .into_iter()
                .map(OsString::from)
                .collect()
        }
        RevisionSelection::Commit(revision) => {
            vec![
                OsString::from("show"),
                OsString::from("--format="),
                OsString::from("--no-ext-diff"),
                OsString::from("--no-color"),
                OsString::from(revision),
                OsString::from("--"),
            ]
        }
    };

    if !repository.scope.as_os_str().is_empty() {
        args.push(repository.scope.as_os_str().to_owned());
    }

    args
}

/// `git fetch` (all remotes, or a single remote/branch). Captures both stdout
/// and stderr as lines for the activity log — git writes progress and remote
/// messages to stderr even on success. No output means the fetch was a no-op
/// (up to date); the caller words the summary.
pub async fn fetch_git(repository: &Repository, target: &FetchTarget) -> Result<Vec<String>> {
    let args: Vec<OsString> = match target {
        FetchTarget::AllRemotes => {
            vec![OsString::from("fetch"), OsString::from("--all")]
        }
        FetchTarget::RemoteBranch { remote, branch } => vec![
            OsString::from("fetch"),
            OsString::from(remote),
            OsString::from(branch),
        ],
    };
    run_command_lines(&repository.root, "git", args).await
}

/// Run a command and return its combined stdout+stderr split into non-empty
/// lines, regardless of exit status — used where the interesting output (git's
/// progress / remote sideband) lands on stderr even on success. Still errors on
/// a non-zero exit, surfacing the captured lines.
async fn run_command_lines(
    current_dir: &Path,
    program: &str,
    args: Vec<OsString>,
) -> Result<Vec<String>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;

    let mut lines = Vec::new();
    for chunk in [&output.stdout, &output.stderr] {
        for line in String::from_utf8_lossy(chunk).split(['\n', '\r']) {
            let line = line.trim_end();
            if !line.is_empty() {
                lines.push(line.to_owned());
            }
        }
    }

    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            lines.join("; ")
        );
    }
    Ok(lines)
}

async fn run_command(current_dir: &Path, program: &str, args: Vec<OsString>) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} exited with {}: {}", output.status, stderr.trim());
    }

    String::from_utf8(output.stdout).with_context(|| format!("{program} emitted non-utf8 output"))
}

/// Read the full old/new contents of one file at `revision`, for full-context
/// syntax highlighting. Best-effort: a side that doesn't resolve (added or
/// deleted file, root commit's parent, binary/non-UTF-8 content, oversized)
/// comes back `None` and the caller falls back to the diff-only
/// reconstruction.
pub async fn read_git_file_pair(
    repository: &Repository,
    revision: &RevisionSelection,
    path: &str,
    old_path: Option<&str>,
) -> (Option<String>, Option<String>) {
    const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
    let cap = |content: String| (content.len() <= MAX_SOURCE_BYTES).then_some(content);
    let old_path = old_path.unwrap_or(path);

    match revision {
        RevisionSelection::WorkingCopy => {
            // The new side is the working tree itself; the old side is what
            // the diff compared against (`git diff HEAD`).
            let new = tokio::fs::read(repository.root.join(path))
                .await
                .ok()
                .and_then(|bytes| {
                    (bytes.len() <= MAX_SOURCE_BYTES)
                        .then(|| String::from_utf8_lossy(&bytes).into_owned())
                });
            let old = git_show_file(&repository.root, &format!("HEAD:{old_path}"))
                .await
                .and_then(cap);
            (old, new)
        }
        RevisionSelection::Commit(id) => {
            let new = git_show_file(&repository.root, &format!("{id}:{path}"))
                .await
                .and_then(cap);
            let old = git_show_file(&repository.root, &format!("{id}^:{old_path}"))
                .await
                .and_then(cap);
            (old, new)
        }
    }
}

/// `git show <rev>:<path>`, `None` on any failure (missing path, bad rev,
/// non-UTF-8 output — `run_command` rejects those).
async fn git_show_file(repository_root: &Path, spec: &str) -> Option<String> {
    run_command(
        repository_root,
        "git",
        vec![OsString::from("show"), OsString::from(spec)],
    )
    .await
    .ok()
}

async fn git_has_uncommitted_changes(repository_root: &Path) -> Result<bool> {
    // `git status --porcelain` prints one line per change (staged, unstaged,
    // or untracked) and nothing on a clean tree.
    let output = run_command(
        repository_root,
        "git",
        vec![OsString::from("status"), OsString::from("--porcelain")],
    )
    .await?;
    Ok(!output.trim().is_empty())
}

async fn load_git_revision_details(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<RevisionDetails> {
    // %x1f is a unit-separator byte chosen to be unlikely in commit metadata,
    // so we can split the fields cleanly even when names or descriptions
    // contain tabs/newlines.
    const SEP: &str = "\x1f";
    let target = match revision {
        RevisionSelection::WorkingCopy => "HEAD".to_owned(),
        RevisionSelection::Commit(id) => id.clone(),
    };
    let format = format!("%H{SEP}%an{SEP}%ae{SEP}%aI{SEP}%cn{SEP}%ce{SEP}%cI{SEP}%D{SEP}%B");
    let output = run_command(
        &repository.root,
        "git",
        vec![
            OsString::from("show"),
            OsString::from("--no-patch"),
            OsString::from(format!("--format={format}")),
            OsString::from(target),
        ],
    )
    .await?;

    let mut parts = output.splitn(9, '\x1f');
    let commit_id = parts.next().unwrap_or("").trim().to_owned();
    let author_name = parts.next().unwrap_or("").to_owned();
    let author_email = parts.next().unwrap_or("").to_owned();
    let author_date = parts.next().unwrap_or("").to_owned();
    let committer_name = parts.next().unwrap_or("").to_owned();
    let committer_email = parts.next().unwrap_or("").to_owned();
    let committer_date = parts.next().unwrap_or("").to_owned();
    let refs = parts.next().unwrap_or("").to_owned();
    let description = parts.next().unwrap_or("").trim_end().to_owned();

    let bookmarks: Vec<String> = refs
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(RevisionDetails {
        commit_id,
        change_id: None,
        bookmarks,
        author: SignatureInfo {
            name: author_name,
            email: author_email,
            timestamp: Some(author_date).filter(|s| !s.is_empty()),
        },
        committer: Some(SignatureInfo {
            name: committer_name,
            email: committer_email,
            timestamp: Some(committer_date).filter(|s| !s.is_empty()),
        }),
        signature: None,
        description,
    })
}

struct ParsedCommitRow {
    change_id: String,
    commit_id: String,
    parents: Vec<String>,
    author: String,
    is_empty: Option<bool>,
    description: String,
    has_description: bool,
    is_working_copy: bool,
}

fn parse_commit_log_rows(output: &str) -> Vec<ParsedCommitRow> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '\t');
            let change_id = parts.next()?.trim();
            let commit_id = parts.next()?.trim();
            let parents_field = parts.next().unwrap_or("");
            let author = parts.next()?.trim();
            let remainder = parts.next().unwrap_or("");
            let (empty, description) =
                if let Some((empty, description)) = remainder.split_once('\t') {
                    (parse_optional_bool(empty.trim()), description.trim())
                } else {
                    (None, remainder.trim())
                };

            if change_id.is_empty() || commit_id.is_empty() {
                return None;
            }

            let parents: Vec<String> = parents_field
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let has_description = !description.is_empty();
            Some(ParsedCommitRow {
                change_id: change_id.to_owned(),
                commit_id: commit_id.to_owned(),
                parents,
                author: author.to_owned(),
                is_empty: empty,
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                has_description,
                is_working_copy: false,
            })
        })
        .collect()
}

fn build_commit_summaries(rows: Vec<ParsedCommitRow>) -> (Vec<CommitSummary>, GraphLayout) {
    // Walk the rows in their existing topo order and assign lanes from
    // parent edges. Parents not present in the listing (shallow clone, etc.)
    // become Missing edges so the renderer can draw a stub.
    let known: std::collections::HashSet<&str> =
        rows.iter().map(|row| row.commit_id.as_str()).collect();
    let lane_inputs = rows.iter().map(|row| {
        let edges: Vec<GraphEdge<String>> = row
            .parents
            .iter()
            .map(|parent| {
                if known.contains(parent.as_str()) {
                    GraphEdge::direct(parent.clone())
                } else {
                    GraphEdge::missing(parent.clone())
                }
            })
            .collect();
        (row.commit_id.clone(), edges)
    });
    let lane_frames = assign_lanes(lane_inputs);

    // Git carries no bookmarks, so the lane fold sees empty labels everywhere.
    let mut graph_builder = GraphLayoutBuilder::new();
    for frame in &lane_frames {
        graph_builder.push(frame, &[]);
    }

    let summaries = rows
        .into_iter()
        .zip(lane_frames)
        .map(|(row, _frame)| CommitSummary {
            change_id: row.change_id,
            commit_id: row.commit_id,
            shortest_change_id_len: None,
            description: row.description,
            author: row.author,
            has_description: row.has_description,
            is_empty: row.is_empty,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: row.is_working_copy,
            bookmarks: Vec::new(),
        })
        .collect();
    (summaries, graph_builder.finish())
}

fn parse_optional_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::Vcs;
    use std::path::PathBuf;

    fn parse_commit_log(output: &str) -> Vec<CommitSummary> {
        build_commit_summaries(parse_commit_log_rows(output)).0
    }

    fn parse_commit_log_graph(output: &str) -> GraphLayout {
        build_commit_summaries(parse_commit_log_rows(output)).1
    }

    #[test]
    fn git_commit_diff_uses_selected_revision() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        };

        let args =
            git_backend_command(&repository, &RevisionSelection::Commit("abc123".to_owned()));

        assert!(args.contains(&OsString::from("show")));
        assert!(args.contains(&OsString::from("abc123")));
        assert_eq!(args.last(), Some(&OsString::from("--")));
    }

    #[test]
    fn parses_commit_log_rows_basic() {
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\tfalse\tadd commit sidebar\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def");
        assert_eq!(commits[0].author, "me@example.com");
        assert_eq!(commits[0].description, "add commit sidebar");
        assert!(commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(false));
    }

    #[test]
    fn parses_commit_log_rows_without_description() {
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\ttrue\t\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].description, "(no description set)");
        assert!(!commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(true));
    }

    #[test]
    fn commit_log_rows_select_full_revision_id() {
        let commits = parse_commit_log(
            "abc\tdef123456789abcdef\t\tme@example.com\tfalse\tadd commit sidebar\n",
        );

        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def123456789abcdef");
    }

    #[test]
    fn git_log_merge_assigns_distinct_lanes_for_second_parent() {
        // Topo order, descendants first:
        //   M (merge of T and W) - parents: T W
        //   T - parent: A
        //   W - parent: A
        //   A - root, no parents
        let graph = parse_commit_log_graph(
            "M\tM\tT W\tme@example.com\tfalse\tmerge\n\
             T\tT\tA\tme@example.com\tfalse\ttrunk\n\
             W\tW\tA\tme@example.com\tfalse\tside\n\
             A\tA\t\tme@example.com\tfalse\troot\n",
        );

        assert_eq!(graph.len(), 4);
        assert_eq!(graph.frame(0, usize::MAX).node_lane, 0);
        assert_eq!(graph.frame(1, usize::MAX).node_lane, 0);
        // Second parent of the merge spawns a new lane to the right.
        assert_eq!(graph.frame(2, usize::MAX).node_lane, 1);
        // Both lanes converge back at A.
        assert_eq!(graph.frame(3, usize::MAX).node_lane, 0);
        assert_eq!(graph.frame(3, usize::MAX).merging_lanes, vec![0, 1]);
    }

    #[test]
    fn git_log_marks_unknown_parents_as_missing() {
        // Single commit whose parent isn't in the listing — e.g. a shallow clone.
        let graph = parse_commit_log_graph("abc\tabc\tdeadbeef\tme@example.com\tfalse\thead\n");

        assert_eq!(graph.len(), 1);
        assert_eq!(graph.frame(0, usize::MAX).missing_parents, 1);
        assert!(graph.frame(0, usize::MAX).after.is_empty());
    }
}
