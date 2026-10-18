//! Size caps for jj diffs.
//!
//! A vendored-dependencies commit or a regenerated lockfile can carry a
//! million changed lines. Materializing every one of them as a display row
//! cost seconds of CPU and gigabytes of memory before anything painted, and
//! the diff view then shaped every row to lay the document out. So a diff is
//! built in two tiers: every file is diffed by line to count its changes, but
//! only files under [`DiffLimits::file_rows`], and only until the document
//! has spent [`DiffLimits::total_rows`], get rows. The rest come back
//! collapsed ([`DiffCollapse`]) with their counts, and are fetched one at a
//! time on demand ([`crate::repo::Command::ExpandFile`]).
//!
//! Word-level emphasis is the other cliff: jj refines every changed region
//! word by word, which on a wholesale rewrite of a big generated file is one
//! enormous region. Past [`DiffLimits::word_diff_bytes`] a file is diffed by
//! line only.

use bstr::BStr;
use jj_lib::diff::{ContentDiff, DiffHunkKind};
use jj_lib::diff_presentation::{
    DiffTokenType, LineCompareMode,
    unified::{DiffLineType, UnifiedDiffHunk, unified_diff_hunks},
};
use jj_lib::merge::Diff;

use crate::model::DiffCollapse;

/// How much of a diff gets display rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffLimits {
    /// A file with more changed lines than this is collapsed on its own.
    pub file_rows: usize,
    /// Once the files given rows add up to this many changed lines, every
    /// further file is collapsed.
    pub total_rows: usize,
    /// Past this many bytes (both sides together) a file is diffed by line,
    /// without word-level emphasis.
    pub word_diff_bytes: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            file_rows: 3_000,
            total_rows: 30_000,
            word_diff_bytes: 512 * 1024,
        }
    }
}

impl DiffLimits {
    /// No caps at all: a file the user asked to see in full.
    pub fn unlimited() -> Self {
        Self {
            file_rows: usize::MAX,
            total_rows: usize::MAX,
            ..Self::default()
        }
    }
}

/// Running budget across one document.
#[derive(Debug)]
pub(crate) struct RowBudget {
    limits: DiffLimits,
    spent: usize,
}

impl RowBudget {
    pub fn new(limits: DiffLimits) -> Self {
        Self { limits, spent: 0 }
    }

    pub fn limits(&self) -> DiffLimits {
        self.limits
    }

    /// Whether a file changing `changed` lines gets rows, and if it does,
    /// charge it to the budget.
    pub fn admit(&mut self, changed: usize) -> Result<(), DiffCollapse> {
        if changed > self.limits.file_rows {
            return Err(DiffCollapse::LargeFile);
        }
        if self.spent.saturating_add(changed) > self.limits.total_rows {
            return Err(DiffCollapse::OverBudget);
        }
        self.spent += changed;
        Ok(())
    }
}

/// `(additions, deletions)` between two texts, by line — no rows built.
pub(crate) fn count_changed_lines(before: &[u8], after: &[u8]) -> (usize, usize) {
    if before.is_empty() {
        return (line_count(after), 0);
    }
    if after.is_empty() {
        return (0, line_count(before));
    }
    let diff = ContentDiff::by_line([before, after]);
    let (mut additions, mut deletions) = (0, 0);
    for hunk in diff.hunks() {
        if hunk.kind == DiffHunkKind::Different {
            deletions += line_count(hunk.contents[0]);
            additions += line_count(hunk.contents[1]);
        }
    }
    (additions, deletions)
}

fn line_count(text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    newline_count(text) + usize::from(!text.ends_with(b"\n"))
}

fn newline_count(text: &[u8]) -> usize {
    text.iter().filter(|&&b| b == b'\n').count()
}

/// Unified hunks with `context` lines around each change, refined word by
/// word unless the file is past `limits.word_diff_bytes`.
pub(crate) fn hunks<'a>(
    before: &'a [u8],
    after: &'a [u8],
    context: usize,
    limits: DiffLimits,
) -> Vec<UnifiedDiffHunk<'a>> {
    if before.len() + after.len() <= limits.word_diff_bytes {
        return unified_diff_hunks(
            Diff::new(BStr::new(before), BStr::new(after)),
            context,
            LineCompareMode::Exact,
        );
    }
    line_hunks(before, after, context)
}

/// [`unified_diff_hunks`] without the word-level refinement: changed lines
/// come back as single matching-typed tokens, so they carry no emphasis.
fn line_hunks<'a>(before: &'a [u8], after: &'a [u8], context: usize) -> Vec<UnifiedDiffHunk<'a>> {
    let mut hunks = Vec::new();
    let mut current = UnifiedDiffHunk {
        left_line_range: 0..0,
        right_line_range: 0..0,
        lines: Vec::new(),
    };
    let push = |hunk: &mut UnifiedDiffHunk<'a>, kind: DiffLineType, line: &'a [u8]| {
        hunk.lines
            .push((kind, vec![(DiffTokenType::Matching, line)]));
        match kind {
            DiffLineType::Context => {
                hunk.left_line_range.end += 1;
                hunk.right_line_range.end += 1;
            }
            DiffLineType::Removed => hunk.left_line_range.end += 1,
            DiffLineType::Added => hunk.right_line_range.end += 1,
        }
    };
    let diff = ContentDiff::by_line([before, after]);
    let mut diff_hunks = diff.hunks().peekable();
    while let Some(hunk) = diff_hunks.next() {
        match hunk.kind {
            DiffHunkKind::Matching => {
                let right: &'a [u8] = hunk.contents[1];
                let mut lines = right.split_inclusive(|b| *b == b'\n').fuse();
                if !current.lines.is_empty() {
                    for line in lines.by_ref().take(context) {
                        push(&mut current, DiffLineType::Context, line);
                    }
                }
                let before_lines: Vec<&[u8]> = if diff_hunks.peek().is_some() {
                    lines.by_ref().rev().take(context).collect()
                } else {
                    Vec::new()
                };
                let skipped = lines.count();
                if skipped > 0 {
                    let left_start = current.left_line_range.end + skipped;
                    let right_start = current.right_line_range.end + skipped;
                    let next = UnifiedDiffHunk {
                        left_line_range: left_start..left_start,
                        right_line_range: right_start..right_start,
                        lines: Vec::new(),
                    };
                    let done = std::mem::replace(&mut current, next);
                    if !done.lines.is_empty() {
                        hunks.push(done);
                    }
                }
                for line in before_lines.into_iter().rev() {
                    push(&mut current, DiffLineType::Context, line);
                }
            }
            DiffHunkKind::Different => {
                let (left, right): (&'a [u8], &'a [u8]) = (hunk.contents[0], hunk.contents[1]);
                for line in left.split_inclusive(|b| *b == b'\n') {
                    push(&mut current, DiffLineType::Removed, line);
                }
                for line in right.split_inclusive(|b| *b == b'\n') {
                    push(&mut current, DiffLineType::Added, line);
                }
            }
        }
    }
    if !current.lines.is_empty() {
        hunks.push(current);
    }
    hunks
}

/// The header of a collapsed file's single placeholder hunk.
pub(crate) fn collapsed_header(reason: DiffCollapse, changed: usize) -> String {
    let lines = format_count(changed);
    // Action first: a narrow pane clips the tail of the header.
    match reason {
        DiffCollapse::LargeFile => format!("Show large diff · {lines} changed lines"),
        DiffCollapse::OverBudget => {
            format!("Show diff · {lines} changed lines (held back: this revision is large)")
        }
    }
}

fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(hunks: &[UnifiedDiffHunk<'_>]) -> Vec<(usize, usize, usize, usize, Vec<String>)> {
        hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.left_line_range.start,
                    hunk.left_line_range.end,
                    hunk.right_line_range.start,
                    hunk.right_line_range.end,
                    hunk.lines
                        .iter()
                        .map(|(kind, tokens)| {
                            let prefix = match kind {
                                DiffLineType::Context => ' ',
                                DiffLineType::Removed => '-',
                                DiffLineType::Added => '+',
                            };
                            let text: Vec<u8> =
                                tokens.iter().flat_map(|(_, t)| t.iter().copied()).collect();
                            format!("{prefix}{}", String::from_utf8_lossy(&text))
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// The line-only builder lays hunks out exactly like jj's own, minus the
    /// word refinement.
    #[test]
    fn line_hunks_match_jj_layout() {
        let before = (1..=40).map(|n| format!("line {n}\n")).collect::<String>();
        let after = before
            .replace("line 5\n", "line five\n")
            .replace("line 30\n", "")
            .replace("line 40\n", "line 40\nline 41\n");
        let ours = line_hunks(before.as_bytes(), after.as_bytes(), 3);
        let theirs = unified_diff_hunks(
            Diff::new(BStr::new(before.as_bytes()), BStr::new(after.as_bytes())),
            3,
            LineCompareMode::Exact,
        );
        assert_eq!(flatten(&ours), flatten(&theirs));
        assert!(
            ours.iter()
                .flat_map(|hunk| &hunk.lines)
                .flat_map(|(_, tokens)| tokens)
                .all(|(kind, _)| *kind == DiffTokenType::Matching),
            "no word emphasis"
        );
    }

    #[test]
    fn counts_changed_lines_without_rows() {
        assert_eq!(count_changed_lines(b"", b"a\nb\nc"), (3, 0));
        assert_eq!(count_changed_lines(b"a\nb\n", b""), (0, 2));
        assert_eq!(count_changed_lines(b"a\nb\nc\n", b"a\nB\nc\nd\n"), (2, 1));
    }

    #[test]
    fn budget_collapses_large_files_and_the_overflow() {
        let mut budget = RowBudget::new(DiffLimits {
            file_rows: 10,
            total_rows: 15,
            word_diff_bytes: 0,
        });
        assert_eq!(budget.admit(11), Err(DiffCollapse::LargeFile));
        assert_eq!(budget.admit(8), Ok(()));
        assert_eq!(budget.admit(8), Err(DiffCollapse::OverBudget));
        assert_eq!(budget.admit(7), Ok(()));
        assert_eq!(budget.admit(1), Err(DiffCollapse::OverBudget));
    }

    #[test]
    fn counts_format_with_separators() {
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }
}
