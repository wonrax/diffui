use anyhow::{Context, Result};
use bstr::BStr;
use futures::StreamExt;
use jj_lib::{
    backend::CommitId,
    conflicts::{
        ConflictMarkerStyle, ConflictMaterializeOptions, materialize_tree_value,
        materialized_diff_stream,
    },
    copies::CopyRecords,
    diff_presentation::{
        LineCompareMode,
        unified::{DiffLineType, git_diff_part, unified_diff_hunks},
    },
    files::FileMergeHunkLevel,
    matchers::{EverythingMatcher, Matcher, PrefixMatcher},
    merge::{Diff, SameChange},
    object_id::ObjectId,
    repo::{ReadonlyRepo, Repo},
    repo_path::{RepoPath, RepoPathBuf},
    tree_merge::MergeOptions,
};

use super::walk::*;
use crate::diff_parse::format_hunk_header;
use crate::model::{
    DiffDocument, DiffFile, DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind, RevisionDetails,
    SignatureInfo,
};
use crate::repository::Repository;

/// Diff `commit_id` against its parents on an already-loaded repo. The actor
/// keeps that repo between commands, so a diff switch costs a tree walk rather
/// than another read of the (large) commit index.
pub(crate) async fn diff_jj_with_repo(
    repo: &ReadonlyRepo,
    commit_id: &CommitId,
    repository: &Repository,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let commit = repo
        .store()
        .get_commit_async(commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    let details = jj_revision_details(repo, &commit);
    let old_tree = commit
        .parent_tree(repo)
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit_id.hex()))?;
    let new_tree = commit.tree();
    let matcher = repo_scope_matcher(repository)?;
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
    if new_tree.has_conflict() {
        let covered: std::collections::HashSet<&str> =
            files.iter().map(|file| file.path.as_str()).collect();
        let mut conflict_files = Vec::new();
        for (repo_path, value) in new_tree.conflicts_matching(matcher.as_ref()) {
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

    Ok((
        DiffDocument {
            files,
            total_additions,
            total_deletions,
        },
        Some(details),
    ))
}

pub(crate) fn jj_revision_details(
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
        RepoPathBuf::parse_fs_path(&repository.root, &repository.root, &repository.scope)
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

    // Expand each block by the context margin and merge windows that touch,
    // so no hunk ever begins inside a block — the classification below
    // re-walks marker state from each hunk's first line.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (block_start, block_end) in blocks {
        let start = block_start.saturating_sub(CONTEXT_LINES);
        let end = (block_end + CONTEXT_LINES).min(last);
        match ranges.last_mut() {
            Some((_, prev_end)) if start <= *prev_end + 1 => *prev_end = (*prev_end).max(end),
            _ => ranges.push((start, end)),
        }
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
    fn adjacent_blocks_merge_into_one_hunk() {
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
        assert_eq!(hunks.len(), 1, "touching context windows merge");
        assert_eq!(hunks[0].header, "Conflict 1 of 2");
        assert_eq!(hunks[0].lines.len(), 9);
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
