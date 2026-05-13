use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result};
use arborium::{
    GrammarStore,
    advanced::{CompiledGrammar, ParseContext},
};

pub use crate::diff_view::{DiffHunkView, DiffLine, DiffLineKind, SyntaxKind, SyntaxSpan};
use crate::graph::LaneFrame;
use crate::repository::{Repository, RepositorySnapshot, Vcs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionSelection {
    WorkingCopy,
    Commit(String),
}

impl RevisionSelection {
    pub fn view_key(&self) -> String {
        match self {
            Self::WorkingCopy => "working-copy".to_owned(),
            Self::Commit(change_id) => format!("commit:{change_id}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub total_additions: usize,
    pub total_deletions: usize,
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: DiffFileStatus,
    pub hunks: Vec<DiffHunkView>,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub change_id: String,
    pub commit_id: String,
    pub revision_id: String,
    pub shortest_change_id_len: Option<usize>,
    pub description: String,
    pub author: String,
    pub has_description: bool,
    pub is_empty: Option<bool>,
    /// Whether the commit's tree is in a conflicted state (jj only — git
    /// commits can't carry an unresolved conflict, so this stays `false`
    /// for the git backend).
    pub has_conflict: bool,
    pub lane_frame: LaneFrame,
    pub is_working_copy: bool,
    /// Bookmarks pointing at this commit. Local bookmarks are bare
    /// names; remote-tracking ones are `name@remote`. Order matches
    /// `jj show`'s "Bookmarks:" line — local first, then remotes.
    pub bookmarks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl DiffFileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Modified => "Modified",
            Self::Renamed => "Renamed",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendOutput {
    pub document: DiffDocument,
    pub commits: Vec<CommitSummary>,
    pub snapshot: RepositorySnapshot,
    pub details: Option<RevisionDetails>,
}

/// `jj show`-style summary of a single revision, used to render the header
/// strip above the diff view.
#[derive(Debug, Clone, Default)]
pub struct RevisionDetails {
    pub commit_id: String,
    pub change_id: Option<String>,
    pub bookmarks: Vec<String>,
    pub author: SignatureInfo,
    pub committer: Option<SignatureInfo>,
    pub signature: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Default)]
pub struct SignatureInfo {
    pub name: String,
    pub email: String,
    pub timestamp: Option<String>,
}

pub async fn load_backend(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<BackendOutput, String> {
    run_backend(repository, revision)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_backend(repository: Repository, revision: RevisionSelection) -> Result<BackendOutput> {
    let commits = load_commits(&repository).await?;
    let (document, details) = match repository.vcs {
        Vcs::Jj => {
            let repository = repository.clone();
            let revision = revision.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_diff(repository, revision))
            })
            .await
            .context("jj diff loader task failed")??
        }
        Vcs::Git => crate::git::load_git_diff(&repository, &revision).await?,
    };
    let snapshot = run_repository_snapshot(repository).await?;

    Ok(BackendOutput {
        document,
        commits,
        snapshot,
        details,
    })
}

pub async fn load_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    run_repository_snapshot(repository).await
}

async fn run_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_repository_snapshot(repository))
            })
            .await
            .context("jj repository snapshot task failed")?
        }
        Vcs::Git => crate::git::load_git_repository_snapshot(&repository.root).await,
    }
}

async fn load_commits(repository: &Repository) -> Result<Vec<CommitSummary>> {
    match repository.vcs {
        Vcs::Jj => {
            let root = repository.root.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(crate::jj::load_jj_commits(root)))
                .await
                .context("jj commit loader task failed")?
        }
        Vcs::Git => crate::git::load_git_commits(repository).await,
    }
}

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

/// Parse `git diff` / `git show` style unified diff output into a
/// `DiffDocument`. Used by the git backend; the jj backend builds files
/// directly from materialized trees and only feeds individual hunks through
/// `format_hunk_header` + line construction.
pub fn parse_unified_diff(output: &str) -> DiffDocument {
    let mut files = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<PendingHunk> = None;

    for line in output.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            flush_current_file(&mut files, &mut current_file, &mut current_hunk);

            let (old_path, path) = parse_diff_git_paths(paths);
            current_file = Some(DiffFile {
                path,
                old_path,
                status: DiffFileStatus::Modified,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("@@") {
            flush_current_hunk(file, &mut current_hunk);
            let (next_old_line, next_new_line) = parse_hunk_header(line);
            current_hunk = Some(PendingHunk {
                header: line.to_owned(),
                rows: Vec::new(),
                next_old_line,
                next_new_line,
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            push_hunk_row(file, hunk, line);
        } else {
            update_file_metadata(file, line);
        }
    }

    flush_current_file(&mut files, &mut current_file, &mut current_hunk);

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    DiffDocument {
        files,
        total_additions,
        total_deletions,
    }
}

fn flush_current_file(
    files: &mut Vec<DiffFile>,
    current_file: &mut Option<DiffFile>,
    current_hunk: &mut Option<PendingHunk>,
) {
    if let Some(file) = current_file.as_mut() {
        flush_current_hunk(file, current_hunk);
    }

    if let Some(mut file) = current_file.take() {
        apply_syntax_highlighting(&mut file);
        files.push(file);
    }
}

fn flush_current_hunk(file: &mut DiffFile, current_hunk: &mut Option<PendingHunk>) {
    if let Some(hunk) = current_hunk.take() {
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

/// Apply syntax highlighting to all visible diff lines for `file`.
///
/// We previously fed each line to tree-sitter individually, which was
/// fundamentally wrong: tree-sitter expects a complete document, so a line
/// like `fn foo(` parses as an error, `}` on its own gets no captures, and
/// every multi-line construct (string literals, function bodies, doc
/// comments, raw strings) is invisible to the parser. The result was that
/// keywords mid-block went un-highlighted and noisy single-character lines
/// would silently fall back to plain text.
///
/// The fix: reconstruct each "side" (old and new) of the file as a single
/// contiguous document, parse it once, and map the resulting spans back to
/// individual lines. Blank lines fill the gaps between hunks so each
/// surviving line still sits at its original line number — the parser
/// won't see the surrounding code, but tree-sitter is reasonably tolerant
/// of missing top-level constructs and will still recover local syntax
/// (literals, identifiers, comments, keywords) correctly within each hunk.
///
/// Context lines are highlighted from the new side (they're identical on
/// both sides, but we only need to look them up once); deletions come from
/// the old side; additions from the new side. Note/Conflict lines are
/// rendered as plain text — they aren't real source content.
pub fn apply_syntax_highlighting(file: &mut DiffFile) {
    static GRAMMAR_STORE: OnceLock<GrammarStore> = OnceLock::new();

    let Some(language) = arborium::detect_language(&file.path) else {
        return;
    };
    let store = GRAMMAR_STORE.get_or_init(GrammarStore::new);
    let Some(grammar) = store.get(language) else {
        return;
    };

    let new_spans = parse_side(&grammar, file, Side::New);
    let old_spans = parse_side(&grammar, file, Side::Old);

    for (hunk_index, hunk) in file.hunks.iter_mut().enumerate() {
        for (line_index, line) in hunk.lines.iter_mut().enumerate() {
            if matches!(line.kind, DiffLineKind::Note | DiffLineKind::Conflict) {
                continue;
            }

            let key = (hunk_index, line_index);
            let spans = match line.kind {
                DiffLineKind::Deletion => old_spans.get(&key),
                DiffLineKind::Addition | DiffLineKind::Context => new_spans.get(&key),
                DiffLineKind::Note | DiffLineKind::Conflict => None,
            };

            if let Some(spans) = spans {
                line.syntax = spans.clone();
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// Reconstruct one side of `file` as a single document, parse it, and slice
/// the resulting captures into per-line span lists keyed by
/// `(hunk_index, line_index)`.
fn parse_side(
    grammar: &Arc<CompiledGrammar>,
    file: &DiffFile,
    side: Side,
) -> HashMap<(usize, usize), Vec<SyntaxSpan>> {
    // For each line we keep on this side, record its byte range in the
    // reconstructed buffer so we can map captures back later.
    struct LineRange {
        hunk_index: usize,
        line_index: usize,
        start: usize,
        end: usize,
    }

    let mut buf = String::new();
    let mut ranges: Vec<LineRange> = Vec::new();
    // 1-indexed cursor over the source-file line numbers we've reached so far.
    let mut current_source_line: usize = 1;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let included = matches!(
                (side, line.kind),
                (Side::Old, DiffLineKind::Context | DiffLineKind::Deletion)
                    | (Side::New, DiffLineKind::Context | DiffLineKind::Addition)
            );
            if !included {
                continue;
            }

            let source_line = match side {
                Side::Old => line.old_line,
                Side::New => line.new_line,
            };
            let Some(target) = source_line else {
                continue;
            };

            // Pad blank lines so this content sits at its true line number.
            // Tree-sitter will see structurally-meaningless gaps but its
            // error-recovery handles that cleanly for most languages.
            while current_source_line < target {
                buf.push('\n');
                current_source_line += 1;
            }

            let start = buf.len();
            buf.push_str(&line.content);
            let end = buf.len();
            buf.push('\n');
            current_source_line += 1;

            ranges.push(LineRange {
                hunk_index,
                line_index,
                start,
                end,
            });
        }
    }

    if buf.trim().is_empty() || ranges.is_empty() {
        return HashMap::new();
    }

    let Ok(mut context) = ParseContext::for_grammar(grammar) else {
        return HashMap::new();
    };
    let result = grammar.parse(&mut context, &buf);

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();

    for span in result.spans {
        let Some(kind) = syntax_kind_for_capture(&span.capture) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start >= end || end > buf.len() {
            continue;
        }
        if !buf.is_char_boundary(start) || !buf.is_char_boundary(end) {
            continue;
        }

        // Walk every line that the span overlaps. Most spans live entirely
        // within one line so this loop usually fires once, but multi-line
        // constructs (block comments, raw strings) need to highlight every
        // covered line.
        for range in &ranges {
            if range.end <= start || range.start >= end {
                continue;
            }
            let local_start = start.saturating_sub(range.start);
            let local_end = (end - range.start).min(range.end - range.start);
            if local_start >= local_end {
                continue;
            }
            per_line
                .entry((range.hunk_index, range.line_index))
                .or_default()
                .push(SyntaxSpan {
                    start: local_start,
                    end: local_end,
                    kind,
                });
        }
    }

    for spans in per_line.values_mut() {
        *spans = normalize_syntax_spans(std::mem::take(spans));
    }
    per_line
}

fn syntax_kind_for_capture(capture: &str) -> Option<SyntaxKind> {
    if capture.starts_with("comment") {
        Some(SyntaxKind::Comment)
    } else if capture.starts_with("string") || capture == "character" {
        Some(SyntaxKind::String)
    } else if capture.starts_with("number")
        || capture.starts_with("constant")
        || capture == "boolean"
    {
        Some(SyntaxKind::Number)
    } else if capture.starts_with("keyword")
        || capture == "operator"
        || capture == "include"
        || capture == "storageclass"
    {
        Some(SyntaxKind::Keyword)
    } else if capture.starts_with("function") || capture == "constructor" || capture == "method" {
        Some(SyntaxKind::Function)
    } else if capture.starts_with("type") || capture == "variable.builtin" {
        Some(SyntaxKind::Type)
    } else if capture.starts_with("property")
        || capture == "variable.parameter"
        || capture == "field"
        || capture == "attribute"
        || capture == "tag"
    {
        Some(SyntaxKind::Property)
    } else if capture.starts_with("punctuation") {
        Some(SyntaxKind::Punctuation)
    } else {
        None
    }
}

fn normalize_syntax_spans(mut spans: Vec<SyntaxSpan>) -> Vec<SyntaxSpan> {
    spans.sort_by_key(|span| (span.start, span.end));

    let mut normalized: Vec<SyntaxSpan> = Vec::with_capacity(spans.len());
    for mut span in spans {
        if let Some(previous) = normalized.last()
            && span.start < previous.end
        {
            span.start = previous.end;
        }

        if span.start < span.end {
            normalized.push(span);
        }
    }

    normalized
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
        assert!(!file.hunks[0].lines[2].syntax.is_empty());
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
}
