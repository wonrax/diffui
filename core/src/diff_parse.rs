//! Unified-diff (`git diff`/`git show`) parser into a `DiffDocument`, plus the
//! shared hunk-header formatter. Pure `std`; highlighting is applied via
//! `crate::syntax` as each file is flushed.

use crate::model::{DiffDocument, DiffFile, DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind};

#[derive(Debug, Clone)]
struct PendingHunk {
    header: String,
    rows: Vec<DiffLine>,
    next_old_line: usize,
    next_new_line: usize,
}

pub fn format_hunk_header(
    old_range: &std::ops::Range<usize>,
    new_range: &std::ops::Range<usize>,
) -> String {
    format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_range),
        format_hunk_range(new_range)
    )
}

fn format_hunk_range(range: &std::ops::Range<usize>) -> String {
    let len = range.end.saturating_sub(range.start);
    let start = if len == 0 {
        range.start
    } else {
        range.start + 1
    };

    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

/// Incremental unified-diff parser: feed lines as they arrive (e.g. off a
/// `gh pr diff` pipe) and receive each completed [`DiffFile`] — highlighted,
/// ready to display — as soon as the next file's header shows it's done, so a
/// huge diff renders progressively instead of after the whole download.
/// [`parse_unified_diff`] is the one-shot wrapper over this.
#[derive(Default)]
pub struct DiffStreamParser {
    current_file: Option<DiffFile>,
    current_hunk: Option<PendingHunk>,
}

impl DiffStreamParser {
    /// Consume one line (without its trailing newline). Returns the previous
    /// file when `line` starts a new one.
    pub fn push_line(&mut self, line: &str) -> Option<DiffFile> {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            let finished = self.take_file();

            let (old_path, path) = parse_diff_git_paths(paths);
            self.current_file = Some(DiffFile {
                path,
                old_path,
                status: DiffFileStatus::Modified,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            return finished;
        }

        let file = self.current_file.as_mut()?;

        if line.starts_with("@@") {
            flush_current_hunk(file, &mut self.current_hunk);
            let (next_old_line, next_new_line) = parse_hunk_header(line);
            self.current_hunk = Some(PendingHunk {
                header: line.to_owned(),
                rows: Vec::new(),
                next_old_line,
                next_new_line,
            });
            return None;
        }

        if let Some(hunk) = self.current_hunk.as_mut() {
            push_hunk_row(file, hunk, line);
        } else {
            update_file_metadata(file, line);
        }
        None
    }

    /// Seed the parser as if a `diff --git` header for `file` was just
    /// consumed, so bare hunk sequences — the shape GitHub's files API returns
    /// in its `patch` field — can be fed straight in. Returns the previously
    /// in-progress file like [`push_line`](Self::push_line) does.
    pub fn begin_file(&mut self, file: DiffFile) -> Option<DiffFile> {
        let finished = self.take_file();
        self.current_file = Some(file);
        finished
    }

    /// End of input: the file still in progress, if any.
    pub fn finish(mut self) -> Option<DiffFile> {
        self.take_file()
    }

    fn take_file(&mut self) -> Option<DiffFile> {
        let mut file = self.current_file.take()?;
        flush_current_hunk(&mut file, &mut self.current_hunk);
        // No highlighting here: tree-sitter over whole documents is seconds of
        // CPU on big files, so it runs in the background after the document is
        // already on screen (see `source::highlight_file`).
        Some(file)
    }
}

/// Parse `git diff` / `git show` style unified diff output into a
/// `DiffDocument`. Used by the git backend; the jj backend builds files
/// directly from materialized trees and only feeds individual hunks through
/// `format_hunk_header` + line construction.
pub fn parse_unified_diff(output: &str) -> DiffDocument {
    let mut parser = DiffStreamParser::default();
    let mut files = Vec::new();
    for line in output.lines() {
        files.extend(parser.push_line(line));
    }
    files.extend(parser.finish());

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    DiffDocument {
        files,
        total_additions,
        total_deletions,
    }
}

fn flush_current_hunk(file: &mut DiffFile, current_hunk: &mut Option<PendingHunk>) {
    if let Some(mut hunk) = current_hunk.take() {
        mark_intra_line_changes(&mut hunk.rows);
        file.hunks.push(DiffHunkView {
            header: hunk.header,
            lines: hunk.rows,
        });
    }
}

fn update_file_metadata(file: &mut DiffFile, line: &str) {
    if let Some(path) = line.strip_prefix("rename from ") {
        file.old_path = Some(path.to_owned());
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.path = path.to_owned();
        file.status = DiffFileStatus::Renamed;
    } else if line.starts_with("new file mode ") || line == "--- /dev/null" {
        file.status = DiffFileStatus::Added;
    } else if line.starts_with("deleted file mode ") || line == "+++ /dev/null" {
        file.status = DiffFileStatus::Deleted;
    } else if let Some(path) = line.strip_prefix("--- a/") {
        file.old_path = Some(path.to_owned());
    } else if let Some(path) = line.strip_prefix("+++ b/") {
        file.path = path.to_owned();
    }
}

fn push_hunk_row(file: &mut DiffFile, hunk: &mut PendingHunk, line: &str) {
    match line.chars().next() {
        Some('+') => {
            file.additions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_new_line += 1;
        }
        Some('-') => {
            file.deletions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Deletion,
                old_line: Some(hunk.next_old_line),
                new_line: None,
                content: line[1..].to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_old_line += 1;
        }
        Some(' ') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(hunk.next_old_line),
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
            hunk.next_old_line += 1;
            hunk.next_new_line += 1;
        }
        Some('\\') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Note,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
        }
        _ => {
            let kind = if is_conflict_marker(line) {
                DiffLineKind::Conflict
            } else {
                DiffLineKind::Note
            };

            hunk.rows.push(DiffLine {
                kind,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
                emphasis: Vec::new(),
            });
        }
    }
}

fn is_conflict_marker(line: &str) -> bool {
    line.starts_with("<<<<<<<")
        || line.starts_with("|||||||")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
}

fn parse_diff_git_paths(paths: &str) -> (Option<String>, String) {
    let mut parts = paths.split_whitespace();
    let old_path = parts.next().map(clean_git_diff_path);
    let path = parts
        .next()
        .map(clean_git_diff_path)
        .or_else(|| old_path.clone())
        .unwrap_or_else(|| "<unknown>".to_owned());

    (old_path, path)
}

fn clean_git_diff_path(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

fn parse_hunk_header(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    let _marker = parts.next();
    let old = parts.next().unwrap_or_default();
    let new = parts.next().unwrap_or_default();

    (
        parse_hunk_range(old, '-').unwrap_or(0),
        parse_hunk_range(new, '+').unwrap_or(0),
    )
}

fn parse_hunk_range(part: &str, prefix: char) -> Option<usize> {
    part.strip_prefix(prefix)
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<usize>().ok())
}

// ---- Intra-line (word-level) change emphasis ----

/// Skip token-diffing pathological lines so emphasis never slows a load: a
/// minified one-liner blows past the byte cap, generated code past the token
/// cap. Such lines simply render without emphasis.
const EMPHASIS_MAX_LINE_BYTES: usize = 4096;
const EMPHASIS_MAX_TOKENS: usize = 256;

/// Fill [`DiffLine::emphasis`] for the deletion/addition lines of one hunk.
///
/// Each maximal run of `-` lines followed by a run of `+` lines is paired
/// index-wise (the shape unified diffs always emit for a modification) and
/// every pair is token-diffed; leftover unpaired lines are pure
/// removals/insertions whose line tint already says everything. The jj
/// backend doesn't come through here — jj-lib's word-level refinement
/// already yields per-token ranges (see `jj::diff_tokens_to_line`) — but
/// both paths share [`finish_line_emphasis`] so gating stays consistent.
pub fn mark_intra_line_changes(lines: &mut [DiffLine]) {
    let mut scratch = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != DiffLineKind::Deletion {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == DiffLineKind::Deletion {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == DiffLineKind::Addition {
            i += 1;
        }
        let pair_count = (add_start - del_start).min(i - add_start);
        for offset in 0..pair_count {
            let (old_emphasis, new_emphasis) = intra_line_emphasis(
                &lines[del_start + offset].content,
                &lines[add_start + offset].content,
                &mut scratch,
            );
            lines[del_start + offset].emphasis = old_emphasis;
            lines[add_start + offset].emphasis = new_emphasis;
        }
    }
}

/// Byte ranges of the changed tokens within one line — the type behind
/// [`crate::model::DiffLine::emphasis`].
type EmphasisRanges = Vec<(usize, usize)>;

/// Token-diff one old/new line pair into per-side changed byte ranges.
/// `scratch` is the LCS table buffer, reused across pairs to keep the hot
/// parse path allocation-light.
fn intra_line_emphasis(
    old: &str,
    new: &str,
    scratch: &mut Vec<u16>,
) -> (EmphasisRanges, EmphasisRanges) {
    let none = (Vec::new(), Vec::new());
    if old.len() > EMPHASIS_MAX_LINE_BYTES || new.len() > EMPHASIS_MAX_LINE_BYTES {
        return none;
    }
    let old_tokens = token_ranges(old);
    let new_tokens = token_ranges(new);
    let (n, m) = (old_tokens.len(), new_tokens.len());
    if n == 0 || m == 0 || n > EMPHASIS_MAX_TOKENS || m > EMPHASIS_MAX_TOKENS {
        return none;
    }

    // Classic LCS table over token text, walked back to flag the tokens
    // outside the common subsequence. Quadratic, but bounded by the token
    // cap (256² u16 = 128KiB, reused via `scratch`).
    fn tok(s: &str, r: (usize, usize)) -> &str {
        &s[r.0..r.1]
    }
    let cols = m + 1;
    scratch.clear();
    scratch.resize((n + 1) * cols, 0);
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            scratch[i * cols + j] = if tok(old, old_tokens[i]) == tok(new, new_tokens[j]) {
                scratch[(i + 1) * cols + j + 1] + 1
            } else {
                scratch[(i + 1) * cols + j].max(scratch[i * cols + j + 1])
            };
        }
    }

    let mut old_raw = Vec::new();
    let mut new_raw = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if tok(old, old_tokens[i]) == tok(new, new_tokens[j]) {
            i += 1;
            j += 1;
        } else if scratch[(i + 1) * cols + j] >= scratch[i * cols + j + 1] {
            old_raw.push(old_tokens[i]);
            i += 1;
        } else {
            new_raw.push(new_tokens[j]);
            j += 1;
        }
    }
    old_raw.extend_from_slice(&old_tokens[i..]);
    new_raw.extend_from_slice(&new_tokens[j..]);

    (
        finish_line_emphasis(old, old_raw),
        finish_line_emphasis(new, new_raw),
    )
}

/// Split a line into emphasis tokens: identifier runs (alphanumeric + `_`),
/// whitespace runs, and individual punctuation characters — so `;` → `,`
/// emphasizes one character, not the whole operator neighborhood.
fn token_ranges(content: &str) -> Vec<(usize, usize)> {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class_of = |ch: char| {
        if ch.is_alphanumeric() || ch == '_' {
            Class::Word
        } else if ch.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };

    let mut tokens = Vec::new();
    let mut start = 0;
    let mut current: Option<Class> = None;
    for (idx, ch) in content.char_indices() {
        let class = class_of(ch);
        // `Other` never extends: each punctuation char is its own token.
        if current == Some(class) && class != Class::Other {
            continue;
        }
        if current.is_some() {
            tokens.push((start, idx));
        }
        start = idx;
        current = Some(class);
    }
    if current.is_some() {
        tokens.push((start, content.len()));
    }
    tokens
}

/// Normalize raw changed-token ranges into display-ready emphasis: clamp to
/// the content, merge ranges separated only by whitespace, trim whitespace
/// edges, and drop the emphasis entirely when it covers nearly the whole
/// line — there the line tint already tells the story and per-token paint
/// is pure noise. Shared by the parser path above and the jj backend.
pub(crate) fn finish_line_emphasis(content: &str, raw: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut raw: Vec<(usize, usize)> = raw
        .into_iter()
        .map(|(start, end)| (start.min(content.len()), end.min(content.len())))
        .filter(|&(start, end)| {
            start < end && content.is_char_boundary(start) && content.is_char_boundary(end)
        })
        .collect();
    if raw.is_empty() {
        return raw;
    }
    raw.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(raw.len());
    for (start, end) in raw {
        match merged.last_mut() {
            Some(last)
                if start <= last.1 || content[last.1..start].chars().all(char::is_whitespace) =>
            {
                last.1 = last.1.max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut result = Vec::with_capacity(merged.len());
    for (start, end) in merged {
        let segment = &content[start..end];
        let trimmed_start = start + (segment.len() - segment.trim_start().len());
        let trimmed_end = end - (segment.len() - segment.trim_end().len());
        if trimmed_start < trimmed_end {
            result.push((trimmed_start, trimmed_end));
        }
    }

    let total: usize = content.chars().filter(|c| !c.is_whitespace()).count();
    let changed: usize = result
        .iter()
        .map(|&(start, end)| {
            content[start..end]
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
        })
        .sum();
    // "Nearly the whole line": ≥85% of its non-whitespace characters.
    if total == 0 || changed * 20 >= total * 17 {
        return Vec::new();
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_diff_into_hunks_and_rows() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,2 +10,3 @@ fn demo()\n let old_value = 0;\n-let old_value = 1;\n+let new_value = 1;\n+let second_line = 2;\n",
        );

        assert_eq!(document.files.len(), 1);
        assert_eq!(document.total_additions, 2);
        assert_eq!(document.total_deletions, 1);

        let file = &document.files[0];
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(file.hunks[0].lines[1].old_line, Some(11));
        assert_eq!(file.hunks[0].lines[1].new_line, None);
        assert_eq!(file.hunks[0].lines[2].kind, DiffLineKind::Addition);
        assert_eq!(file.hunks[0].lines[2].old_line, None);
        assert_eq!(file.hunks[0].lines[2].new_line, Some(11));
        // Highlighting no longer runs in the parser — it's applied in the
        // background after the document is on screen.
        assert!(file.hunks[0].lines[2].syntax.is_empty());
    }

    #[test]
    fn parses_conflict_markers() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n<<<<<<< mine\n",
        );

        assert_eq!(
            document.files[0].hunks[0].lines[0].kind,
            DiffLineKind::Conflict
        );
    }

    #[test]
    fn parses_rename_metadata() {
        let document = parse_unified_diff(
            "diff --git a/src/old.rs b/src/new.rs\nsimilarity index 100%\nrename from src/old.rs\nrename to src/new.rs\n",
        );

        let file = &document.files[0];
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.path, "src/new.rs");
    }

    #[test]
    fn empty_diff_yields_no_files() {
        let document = parse_unified_diff("");

        assert!(document.files.is_empty());
        assert_eq!(document.total_additions, 0);
        assert_eq!(document.total_deletions, 0);
    }

    #[test]
    fn marks_changed_tokens_in_paired_lines() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-let old_value = 1;\n+let new_value = 1;\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        assert_eq!(lines[0].emphasis, vec![(4, 13)]);
        assert_eq!(lines[1].emphasis, vec![(4, 13)]);
    }

    #[test]
    fn bridges_whitespace_between_adjacent_changed_tokens() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a b c\n+x y c\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        assert_eq!(lines[0].emphasis, vec![(0, 3)]);
        assert_eq!(lines[1].emphasis, vec![(0, 3)]);
    }

    #[test]
    fn rewrites_and_unpaired_lines_get_no_emphasis() {
        let document = parse_unified_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,1 @@\n-foo bar baz\n-second removed line\n+qux quux corge\n",
        );

        let lines = &document.files[0].hunks[0].lines;
        // The (0 ↔ 2) pair shares no tokens, so the coverage gate drops both
        // sides; the second deletion has no partner at all.
        assert!(lines[0].emphasis.is_empty());
        assert!(lines[1].emphasis.is_empty());
        assert!(lines[2].emphasis.is_empty());
    }

    #[test]
    fn finish_line_emphasis_clamps_trims_and_gates() {
        assert_eq!(finish_line_emphasis("foo bar", vec![(0, 3)]), vec![(0, 3)]);
        // Out-of-range end clamps to the line, which then covers everything
        // and gets gated away.
        assert!(finish_line_emphasis("foo", vec![(0, 4)]).is_empty());
        // Whitespace-only emphasis trims to nothing.
        assert!(finish_line_emphasis("a  b", vec![(1, 3)]).is_empty());
    }
}
