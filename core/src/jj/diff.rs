use anyhow::{Context, Result};
use futures::StreamExt;
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    conflicts::{
        ConflictMarkerStyle, ConflictMaterializeOptions, materialize_tree_value,
        materialized_diff_stream,
    },
    copies::CopyRecords,
    diff_presentation::unified::{DiffLineType, git_diff_part},
    files::FileMergeHunkLevel,
    matchers::{EverythingMatcher, FilesMatcher, Matcher, PrefixMatcher},
    merge::{Diff, SameChange},
    merged_tree::MergedTree,
    object_id::ObjectId,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo},
    repo_path::{RepoPath, RepoPathBuf},
    rewrite::rebase_to_dest_parent,
    tree_merge::MergeOptions,
};

use super::large_diff::{self, DiffLimits, RowBudget};
use super::walk::*;
use super::workspace::resolve_revision;
use crate::diff_parse::format_hunk_header;
use crate::model::{
    DiffBase, DiffDocument, DiffFile, DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind,
    ParentBrief, RevisionDetails, SignatureInfo,
};
use crate::repository::Repository;

/// What a diff load covers beyond its two trees.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiffOptions<'a> {
    pub limits: DiffLimits,
    /// Paths diffed in full regardless of `limits`.
    pub expand: &'a [String],
    /// Diff only this path (expanding one collapsed file).
    pub only: Option<&'a str>,
}

/// The "from" side of a diff ([`DiffBase`]), resolved to a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedBase {
    Parents,
    Tree(CommitId),
    Interdiff(CommitId),
}

pub(crate) async fn resolve_base(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    base: &DiffBase,
) -> Result<ResolvedBase> {
    Ok(match base {
        DiffBase::Parents => ResolvedBase::Parents,
        DiffBase::Revision(revision) => {
            ResolvedBase::Tree(resolve_revision(repo, workspace_name, revision).await?)
        }
        DiffBase::Interdiff(revision) => {
            ResolvedBase::Interdiff(resolve_revision(repo, workspace_name, revision).await?)
        }
        // A range names both sides; its loads go through `diff_jj_range`.
        DiffBase::Range(revset) => {
            anyhow::bail!("the range {revset} has no single base revision")
        }
    })
}

/// The tree `commit`'s diff is taken from: its parents' merge, another
/// revision's tree (`jj diff --from`), or another revision's own change
/// rebased onto `commit`'s parents (`jj interdiff --from`).
pub(crate) async fn base_tree(
    repo: &ReadonlyRepo,
    commit: &Commit,
    base: &ResolvedBase,
) -> Result<MergedTree> {
    match base {
        ResolvedBase::Parents => commit
            .parent_tree(repo)
            .await
            .with_context(|| format!("failed to load jj parent tree for {}", commit.id().hex())),
        ResolvedBase::Tree(from) => Ok(load_commit(repo, from).await?.tree()),
        ResolvedBase::Interdiff(from) => {
            let from = load_commit(repo, from).await?;
            rebase_to_dest_parent(repo, std::slice::from_ref(&from), commit)
                .await
                .with_context(|| {
                    format!(
                        "failed to rebase {} onto the parents of {}",
                        from.id().hex(),
                        commit.id().hex()
                    )
                })
        }
    }
}

async fn load_commit(repo: &ReadonlyRepo, commit_id: &CommitId) -> Result<Commit> {
    repo.store()
        .get_commit_async(commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))
}

/// Diff `commit_id` against `base` (its parents, unless comparing) on an
/// already-loaded repo. The actor keeps that repo between commands, so a diff
/// switch costs a tree walk rather than another read of the (large) commit
/// index.
pub(crate) async fn diff_jj_with_repo(
    repo: &ReadonlyRepo,
    commit_id: &CommitId,
    base: &ResolvedBase,
    repository: &Repository,
    options: &DiffOptions<'_>,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let commit = load_commit(repo, commit_id).await?;
    let details = jj_revision_details(repo, &commit).await;
    let old_tree = base_tree(repo, &commit, base).await?;
    let matcher = diff_matcher(repository, options)?;
    // A comparison shows only what differs between its two sides, like
    // `jj diff --from`, so only a diff against the parents lists conflicts.
    let document = diff_trees(
        repo,
        &old_tree,
        &commit.tree(),
        matcher.as_ref(),
        options,
        *base == ResolvedBase::Parents,
    )
    .await?;
    Ok((document, Some(details)))
}

/// `jj diff -r REVSET` over several commits: the range's merged base against
/// its merged heads, carrying what the range holds. Conflicts surface as they
/// do for a single revision, since the base is the roots' parents.
pub(crate) async fn diff_jj_range(
    repo: &ReadonlyRepo,
    range: super::revset::ResolvedRange,
    repository: &Repository,
    options: &DiffOptions<'_>,
) -> Result<DiffDocument> {
    let matcher = diff_matcher(repository, options)?;
    let mut document = diff_trees(
        repo,
        &range.from,
        &range.to,
        matcher.as_ref(),
        options,
        true,
    )
    .await?;
    document.range = Some(Box::new(range.info));
    Ok(document)
}

/// The paths a diff load covers: the one file being expanded, or the tab's
/// scope.
fn diff_matcher(repository: &Repository, options: &DiffOptions<'_>) -> Result<Box<dyn Matcher>> {
    Ok(match options.only {
        Some(path) => Box::new(FilesMatcher::new([RepoPathBuf::from_internal_string(
            path.to_owned(),
        )
        .with_context(|| format!("invalid repo path {path}"))?])),
        None => repo_scope_matcher(repository)?,
    })
}

/// How the diff cuts a conflicted file into regions. Resolving one region
/// ([`crate::ConflictResolution::Region`]) cuts with the same options, so its
/// `conflict k of n` is the one the pane showed.
pub(super) fn conflict_merge_options() -> MergeOptions {
    MergeOptions {
        hunk_level: FileMergeHunkLevel::Line,
        same_change: SameChange::Accept,
    }
}

/// Diff two trees into a [`DiffDocument`], within `options`' caps, and —
/// with `surface_conflicts` — list the new side's conflicts. Shared by the
/// revision diff and the history views' interdiffs, which compare trees that
/// aren't a commit and its parents.
pub(crate) async fn diff_trees(
    repo: &ReadonlyRepo,
    old_tree: &MergedTree,
    new_tree: &MergedTree,
    matcher: &dyn Matcher,
    options: &DiffOptions<'_>,
    surface_conflicts: bool,
) -> Result<DiffDocument> {
    let mut budget = RowBudget::new(options.limits);
    let copy_records = CopyRecords::default();
    let tree_diff = old_tree.diff_stream_with_copies(new_tree, matcher, &copy_records);
    let labels = Diff::new(old_tree.labels(), new_tree.labels());
    let mut stream = materialized_diff_stream(repo.store(), tree_diff, labels);
    let materialize_options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Diff,
        marker_len: None,
        merge: conflict_merge_options(),
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
        // The conflicted side of a diff gets a hunk per conflict region, like
        // a `Conflicted` entry (see `split_conflict_regions`).
        let after_conflicted = surface_conflicts
            && matches!(
                values.after,
                jj_lib::conflicts::MaterializedTreeValue::FileConflict(_)
            );
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
            collapsed: None,
        };

        let binary = before.content.is_binary || after.content.is_binary;
        let (before_bytes, after_bytes) = (
            before.content.contents.as_slice(),
            after.content.contents.as_slice(),
        );
        // Past the caps a file keeps its counts but gets no rows (see
        // `large_diff`); one the user already expanded is exempt.
        let collapse = if binary || options.expand.contains(&file.path) {
            None
        } else {
            let (additions, deletions) = large_diff::count_changed_lines(before_bytes, after_bytes);
            budget
                .admit(additions + deletions)
                .err()
                .map(|reason| (reason, additions, deletions))
        };

        if binary {
            file.hunks.push(DiffHunkView {
                header: "binary files differ".to_owned(),
                lines: Vec::new(),
            });
        } else if let Some((reason, additions, deletions)) = collapse {
            file.hunks.push(DiffHunkView {
                header: large_diff::collapsed_header(reason, additions + deletions),
                lines: Vec::new(),
            });
            file.additions = additions;
            file.deletions = deletions;
            file.collapsed = Some(reason);
        } else {
            let hunks = large_diff::hunks(before_bytes, after_bytes, 3, budget.limits());
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
            if after_conflicted {
                file.hunks = split_conflict_regions(std::mem::take(&mut file.hunks));
            }
        }

        // Highlighting is deliberately NOT applied here: it's tree-sitter
        // over whole documents — seconds of CPU on big files — and runs in
        // the background instead (see `syntax::highlight_file`), so the diff
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
    if surface_conflicts && new_tree.has_conflict() {
        let covered: std::collections::HashSet<&str> =
            files.iter().map(|file| file.path.as_str()).collect();
        let mut conflict_files = Vec::new();
        for (repo_path, value) in new_tree.conflicts_matching(matcher) {
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
                collapsed: None,
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

    Ok(DiffDocument {
        files,
        total_additions,
        total_deletions,
        range: None,
    })
}

pub(crate) async fn jj_revision_details(
    repo: &dyn Repo,
    commit: &jj_lib::commit::Commit,
) -> RevisionDetails {
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
    // An unreadable parent is left out rather than failing the header.
    let parents = commit
        .parents()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|parent| ParentBrief {
            commit_id: parent.id().hex(),
            change_id: Some(parent.change_id().to_string()),
            summary: parent
                .description()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect();

    RevisionDetails {
        commit_id: commit.id().hex(),
        change_id: Some(change_id),
        bookmarks,
        author,
        committer: Some(committer),
        signature: None,
        description: commit.description().to_owned(),
        parents,
    }
}

pub(super) fn jj_signature_info(signature: &jj_lib::backend::Signature) -> SignatureInfo {
    SignatureInfo {
        name: signature.name.clone(),
        email: signature.email.clone(),
        timestamp: Some(format_jj_timestamp(&signature.timestamp)),
    }
}

pub(super) fn format_jj_timestamp(ts: &jj_lib::backend::Timestamp) -> String {
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
pub(super) fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
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

pub(super) fn repo_scope_matcher(repository: &Repository) -> Result<Box<dyn Matcher>> {
    if repository.scope.as_os_str().is_empty() {
        return Ok(Box::new(EverythingMatcher));
    }

    let repo_path =
        jj_lib::ui_path::parse_fs_path(&repository.root, &repository.root, &repository.scope)
            .with_context(|| format!("failed to parse jj path {}", repository.scope.display()))?;
    Ok(Box::new(PrefixMatcher::new([repo_path])))
}

pub(super) fn repo_path_label(path: &RepoPath) -> String {
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
pub(super) fn conflict_hunks(text: &str) -> (Vec<DiffHunkView>, usize, usize) {
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

    // Every block is a hunk of its own, so its header can carry that
    // region's resolve controls: expand each by the context margin, and split
    // the lines between two blocks closer than two margins apart rather than
    // merging them. No hunk ever begins inside a block, which the
    // classification below relies on (it re-walks marker state from each
    // hunk's first line).
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (index, &(block_start, block_end)) in blocks.iter().enumerate() {
        let start = match ranges.last() {
            Some(&(_, prev_end)) => block_start.saturating_sub(CONTEXT_LINES).max(prev_end + 1),
            None => block_start.saturating_sub(CONTEXT_LINES),
        };
        let mut end = (block_end + CONTEXT_LINES).min(last);
        if let Some(&(next_start, _)) = blocks.get(index + 1) {
            let between = next_start - block_end - 1;
            if between < 2 * CONTEXT_LINES {
                end = block_end + between / 2;
            }
        }
        ranges.push((start, end));
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

/// Cut diff hunks so none holds more than one conflict region's opening
/// marker: the conflicted side of a file's diff, given the hunk-per-region
/// shape `conflict_hunks` gives a `Conflicted` entry. A cut falls halfway
/// between one region's closing marker and the next one's opening marker,
/// and the pieces get `@@` headers of their own.
pub(super) fn split_conflict_regions(hunks: Vec<DiffHunkView>) -> Vec<DiffHunkView> {
    let mut out = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        let new_side = |line: &DiffLine| line.new_line.is_some();
        let openers: Vec<usize> = hunk
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                new_side(line) && crate::resolve::conflict_region(&line.content).is_some()
            })
            .map(|(index, _)| index)
            .collect();
        if openers.len() < 2 {
            out.push(hunk);
            continue;
        }
        let mut cuts = vec![0];
        for pair in openers.windows(2) {
            let (open, next) = (pair[0], pair[1]);
            let close = (open + 1..next).rev().find(|&index| {
                let line = &hunk.lines[index];
                new_side(line) && conflict_marker_char(&line.content) == Some(b'>')
            });
            cuts.push(match close {
                Some(close) => close + 1 + (next - close - 1) / 2,
                None => next,
            });
        }
        cuts.push(hunk.lines.len());
        // 0-based positions of the hunk's first old and new lines.
        let first = |pick: fn(&DiffLine) -> Option<usize>| {
            hunk.lines
                .iter()
                .find_map(pick)
                .map_or(0, |line| line.saturating_sub(1))
        };
        let mut old_at = first(|line| line.old_line);
        let mut new_at = first(|line| line.new_line);
        for cut in cuts.windows(2) {
            let lines = hunk.lines[cut[0]..cut[1]].to_vec();
            let olds = lines.iter().filter(|line| line.old_line.is_some()).count();
            let news = lines.iter().filter(|line| line.new_line.is_some()).count();
            out.push(DiffHunkView {
                header: format_hunk_header(&(old_at..old_at + olds), &(new_at..new_at + news)),
                lines,
            });
            old_at += olds;
            new_at += news;
        }
    }
    out
}

/// The marker character opening `line` when it is a jj conflict-marker line:
/// a run of ≥7 identical characters from the marker alphabet, followed by a
/// space or end-of-line.
pub(super) fn conflict_marker_char(line: &str) -> Option<u8> {
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
pub(super) fn diff_tokens_to_line(
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
    fn adjacent_blocks_get_a_hunk_each() {
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
        assert_eq!(hunks.len(), 2, "a region per hunk");
        assert_eq!(hunks[0].header, "Conflict 1 of 2");
        assert_eq!(hunks[1].header, "Conflict 2 of 2");
        // The line between the blocks goes to the second; nothing repeats.
        assert_eq!(hunks[0].lines.len(), 4);
        assert_eq!(hunks[1].lines.len(), 5);
        assert_eq!(hunks[1].lines[0].content, "between");
        assert_eq!(hunks[1].lines[0].new_line, Some(5));
    }

    #[test]
    fn context_between_close_blocks_is_shared_out() {
        let text = "\
            <<<<<<< conflict 1 of 2\n\
            a\n\
            >>>>>>> conflict 1 of 2 ends\n\
            one\n\
            two\n\
            three\n\
            four\n\
            five\n\
            <<<<<<< conflict 2 of 2\n\
            b\n\
            >>>>>>> conflict 2 of 2 ends\n";
        let (hunks, _, _) = conflict_hunks(text);
        assert_eq!(hunks.len(), 2);
        let contents = |hunk: &DiffHunkView| -> Vec<String> {
            hunk.lines.iter().map(|line| line.content.clone()).collect()
        };
        assert_eq!(contents(&hunks[0])[3..], ["one", "two"]);
        assert_eq!(contents(&hunks[1])[..3], ["three", "four", "five"]);
    }

    fn line(kind: DiffLineKind, old: Option<usize>, new: Option<usize>, content: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: old,
            new_line: new,
            content: content.to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        }
    }

    #[test]
    fn a_diff_hunk_with_two_regions_is_cut_between_them() {
        use DiffLineKind::{Addition, Context, Deletion};
        let lines = vec![
            line(Context, Some(1), Some(1), "top"),
            line(Deletion, Some(2), None, "old"),
            line(Addition, None, Some(2), "<<<<<<< conflict 1 of 2"),
            line(Addition, None, Some(3), "x"),
            line(Addition, None, Some(4), ">>>>>>> conflict 1 of 2 ends"),
            line(Context, Some(3), Some(5), "mid 1"),
            line(Context, Some(4), Some(6), "mid 2"),
            line(Addition, None, Some(7), "<<<<<<< conflict 2 of 2"),
            line(Addition, None, Some(8), "y"),
            line(Addition, None, Some(9), ">>>>>>> conflict 2 of 2 ends"),
            line(Context, Some(5), Some(10), "bottom"),
        ];
        let hunks = split_conflict_regions(vec![DiffHunkView {
            header: "@@ -1,5 +1,10 @@".to_owned(),
            lines,
        }]);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].header, "@@ -1,3 +1,5 @@");
        assert_eq!(hunks[1].header, "@@ -4,2 +6,5 @@");
        assert_eq!(hunks[1].lines[0].content, "mid 2");
        assert_eq!(
            crate::resolve::hunk_conflict_region(&hunks[1]),
            Some((1, 2))
        );
        // One region or none: left alone.
        let single = split_conflict_regions(vec![hunks[0].clone()]);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].header, "@@ -1,3 +1,5 @@");
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
