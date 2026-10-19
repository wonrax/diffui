//! Partial changes: picking files and hunks out of a revision's diff.
//!
//! The selection types here are what the frontend's checkboxes produce and
//! what `jj split` / `jj squash -i` / `jj restore -i` style mutations consume
//! (see [`crate::mutations::MutationOp::Split`] and friends). The text side —
//! cutting a file's diff into hunks and splicing a chosen subset of them onto
//! the parent's content — lives here too, independent of jj-lib's stores, so
//! it can be tested byte for byte.
//!
//! Hunks are cut exactly the way the diff pane cuts them (jj's unified diff
//! with three lines of context), so "hunk 2" means the same lines in the
//! checkbox the user ticked and in the tree the backend writes. Each picked
//! hunk also carries the header it was displayed with; if the file changed
//! under the selection (a working-copy edit, a concurrent rewrite) the headers
//! stop matching and the operation is refused instead of applying the wrong
//! lines.

use std::ops::Range;

use bstr::BStr;
use jj_lib::diff_presentation::{LineCompareMode, unified::unified_diff_hunks};
use jj_lib::merge::Diff;

use crate::diff_parse::format_hunk_header;

/// Lines of context around each hunk. Must match the diff pane's (see
/// `jj::diff::diff_jj_with_repo`), or hunk indices would name different lines
/// on the two sides.
pub const HUNK_CONTEXT: usize = 3;

/// A subset of one revision's changes, by file and hunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffSelection {
    pub files: Vec<FileSelection>,
}

/// The picked part of one file's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
    /// Repo-relative path, as the diff lists it.
    pub path: String,
    pub hunks: HunkPick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkPick {
    /// The whole file: its tree value as is — content, executable bit,
    /// deletion, binary or conflicted state. Path granularity, what
    /// `jj squash <path>` does.
    All,
    /// Only these hunks of a text change (never empty in a well-formed
    /// selection). The rest of the file keeps the parent's lines.
    Hunks(Vec<HunkRef>),
}

/// One hunk, by position in the file's diff, with the header it was shown
/// with as a staleness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRef {
    pub index: usize,
    pub header: String,
}

impl DiffSelection {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Picked hunks, counting a whole-file pick as one.
    pub fn hunk_count(&self) -> usize {
        self.files
            .iter()
            .map(|file| match &file.hunks {
                HunkPick::All => 1,
                HunkPick::Hunks(hunks) => hunks.len(),
            })
            .sum()
    }

    /// Whether any file is picked by hunk rather than whole.
    pub fn has_partial_files(&self) -> bool {
        self.files
            .iter()
            .any(|file| matches!(file.hunks, HunkPick::Hunks(_)))
    }

    /// "2 files" / "3 hunks in 2 files" / "1 hunk in a.rs" — what the
    /// activity log and the selection bar call this selection.
    pub fn summary(&self) -> String {
        let files = self.file_count();
        if !self.has_partial_files() {
            return match self.files.as_slice() {
                [only] => only.path.clone(),
                _ => format!("{files} files"),
            };
        }
        let hunks = self.hunk_count();
        let hunk_noun = if hunks == 1 { "hunk" } else { "hunks" };
        match self.files.as_slice() {
            [only] => format!("{hunks} {hunk_noun} in {}", only.path),
            _ => format!("{hunks} {hunk_noun} in {files} files"),
        }
    }
}

/// One hunk of a text diff: its line ranges on both sides (indices into the
/// content split after each `\n`, the last line possibly unterminated) and
/// its `@@ -a,b +c,d @@` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHunk {
    pub left: Range<usize>,
    pub right: Range<usize>,
    pub header: String,
}

/// Cut `before → after` into the hunks the diff pane shows.
pub fn text_hunks(before: &[u8], after: &[u8]) -> Vec<TextHunk> {
    unified_diff_hunks(
        Diff::new(BStr::new(before), BStr::new(after)),
        HUNK_CONTEXT,
        LineCompareMode::Exact,
    )
    .into_iter()
    .map(|hunk| TextHunk {
        header: format_hunk_header(&hunk.left_line_range, &hunk.right_line_range),
        left: hunk.left_line_range,
        right: hunk.right_line_range,
    })
    .collect()
}

/// Lines with their terminators, so joining them back is byte-exact: CRLF
/// stays CRLF and a missing final newline stays missing.
fn lines(content: &[u8]) -> Vec<&[u8]> {
    content.split_inclusive(|byte| *byte == b'\n').collect()
}

/// `before` with the hunks `take` picks replaced by their `after` side — the
/// file content of "the parent plus just those changes".
///
/// `hunks` must be [`text_hunks`] of the same two contents. Context lines
/// inside a hunk are byte-identical on both sides (the diff compares lines
/// exactly), so swapping a hunk's whole left range for its right range moves
/// exactly its changed lines.
pub fn apply_hunks(
    before: &[u8],
    after: &[u8],
    hunks: &[TextHunk],
    take: impl Fn(usize) -> bool,
) -> Vec<u8> {
    let left = lines(before);
    let right = lines(after);
    let mut out = Vec::with_capacity(before.len().max(after.len()));
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        for line in &left[cursor..hunk.left.start] {
            out.extend_from_slice(line);
        }
        let picked = if take(index) {
            &right[hunk.right.clone()]
        } else {
            &left[hunk.left.clone()]
        };
        for line in picked {
            out.extend_from_slice(line);
        }
        cursor = hunk.left.end;
    }
    for line in &left[cursor..] {
        out.extend_from_slice(line);
    }
    out
}

/// Check `refs` against the file's actual hunks and turn them into a
/// per-hunk pick mask. Errors name the file so the user knows what to
/// reselect.
pub fn pick_mask(path: &str, hunks: &[TextHunk], refs: &[HunkRef]) -> Result<Vec<bool>, String> {
    let mut mask = vec![false; hunks.len()];
    for hunk_ref in refs {
        match hunks.get(hunk_ref.index) {
            Some(hunk) if hunk.header == hunk_ref.header => mask[hunk_ref.index] = true,
            _ => {
                return Err(format!(
                    "the diff of {path} changed since its hunks were selected. Select them again."
                ));
            }
        }
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(before: &str, after: &str, take: &[usize]) -> String {
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let bytes = apply_hunks(before.as_bytes(), after.as_bytes(), &hunks, |index| {
            take.contains(&index)
        });
        String::from_utf8(bytes).expect("utf-8")
    }

    /// Ten numbered lines; far-apart edits land in separate hunks.
    fn numbered(edit: impl Fn(usize) -> Option<&'static str>) -> String {
        (1..=20)
            .map(|n| match edit(n) {
                Some(text) => format!("{text}\n"),
                None => format!("line {n}\n"),
            })
            .collect()
    }

    #[test]
    fn separate_edits_are_separate_hunks() {
        let before = numbered(|_| None);
        let after = numbered(|n| match n {
            2 => Some("two"),
            18 => Some("eighteen"),
            _ => None,
        });
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(hunks[0].header, "@@ -1,5 +1,5 @@");
    }

    #[test]
    fn taking_no_hunk_is_the_parent_and_every_hunk_is_the_child() {
        let before = numbered(|_| None);
        let after = numbered(|n| match n {
            2 => Some("two"),
            18 => Some("eighteen"),
            _ => None,
        });
        assert_eq!(apply(&before, &after, &[]), before);
        assert_eq!(apply(&before, &after, &[0, 1]), after);
    }

    #[test]
    fn one_hunk_applies_alone() {
        let before = numbered(|_| None);
        let after = numbered(|n| match n {
            2 => Some("two"),
            18 => Some("eighteen"),
            _ => None,
        });
        assert_eq!(
            apply(&before, &after, &[0]),
            numbered(|n| (n == 2).then_some("two"))
        );
        assert_eq!(
            apply(&before, &after, &[1]),
            numbered(|n| (n == 18).then_some("eighteen"))
        );
    }

    #[test]
    fn insertions_and_deletions_shift_later_hunks_correctly() {
        let before = numbered(|_| None);
        // Insert two lines near the top, delete one near the bottom.
        let mut after_lines: Vec<String> = before.lines().map(|l| format!("{l}\n")).collect();
        after_lines.remove(17); // "line 18"
        after_lines.insert(2, "new a\n".to_owned());
        after_lines.insert(3, "new b\n".to_owned());
        let after: String = after_lines.concat();
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 2);

        let only_deletion = apply(&before, &after, &[1]);
        assert!(!only_deletion.contains("line 18\n"));
        assert!(!only_deletion.contains("new a"));
        let only_insertion = apply(&before, &after, &[0]);
        assert!(only_insertion.contains("line 18\n"));
        assert!(only_insertion.contains("line 2\nnew a\nnew b\nline 3\n"));
    }

    #[test]
    fn crlf_lines_survive_byte_exact() {
        let crlf = |text: String| text.replace('\n', "\r\n");
        let before = crlf(numbered(|_| None));
        let after = crlf(numbered(|n| match n {
            2 => Some("two"),
            18 => Some("eighteen"),
            _ => None,
        }));
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            apply(&before, &after, &[0]),
            crlf(numbered(|n| (n == 2).then_some("two")))
        );
    }

    #[test]
    fn missing_final_newline_is_preserved_on_either_side() {
        // The child adds a final newline and edits the top.
        let before = numbered(|_| None);
        let before = before.trim_end_matches('\n');
        let after = numbered(|n| (n == 1).then_some("one"));
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 2);
        let top_only = apply(before, &after, &[0]);
        assert!(top_only.starts_with("one\n"));
        assert!(top_only.ends_with("line 20"), "{top_only:?}");
        let tail_only = apply(before, &after, &[1]);
        assert!(tail_only.starts_with("line 1\n"));
        assert!(tail_only.ends_with("line 20\n"));

        // The child drops the final newline.
        let after_no_eol = numbered(|_| None);
        let after_no_eol = after_no_eol.trim_end_matches('\n');
        let full = numbered(|_| None);
        assert_eq!(apply(&full, after_no_eol, &[0]), after_no_eol);
        assert_eq!(apply(&full, after_no_eol, &[]), full);
    }

    #[test]
    fn added_and_deleted_files_split_by_hunk() {
        // An added file is a diff from nothing: one hunk, all or nothing.
        let after = "x\ny\n";
        assert_eq!(apply("", after, &[0]), after);
        assert_eq!(apply("", after, &[]), "");
        // A deletion is one hunk the other way.
        assert_eq!(apply(after, "", &[0]), "");
    }

    #[test]
    fn stale_hunk_refs_are_refused() {
        let before = numbered(|_| None);
        let after = numbered(|n| (n == 2).then_some("two"));
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let good = HunkRef {
            index: 0,
            header: hunks[0].header.clone(),
        };
        assert_eq!(
            pick_mask("f", &hunks, std::slice::from_ref(&good)),
            Ok(vec![true])
        );
        let moved = HunkRef {
            index: 0,
            header: "@@ -3,5 +3,5 @@".to_owned(),
        };
        assert!(pick_mask("f", &hunks, &[moved]).is_err());
        let gone = HunkRef {
            index: 4,
            header: good.header,
        };
        assert!(pick_mask("f", &hunks, &[gone]).is_err());
    }

    #[test]
    fn summary_counts_hunks_and_files() {
        let pick = |path: &str, hunks: &[usize]| FileSelection {
            path: path.to_owned(),
            hunks: if hunks.is_empty() {
                HunkPick::All
            } else {
                HunkPick::Hunks(
                    hunks
                        .iter()
                        .map(|&index| HunkRef {
                            index,
                            header: String::new(),
                        })
                        .collect(),
                )
            },
        };
        let selection = DiffSelection {
            files: vec![pick("a.rs", &[0, 2]), pick("b.rs", &[])],
        };
        assert_eq!(selection.summary(), "3 hunks in 2 files");
        let whole = DiffSelection {
            files: vec![pick("a.rs", &[])],
        };
        assert_eq!(whole.summary(), "a.rs");
        let one = DiffSelection {
            files: vec![pick("a.rs", &[1])],
        };
        assert_eq!(one.summary(), "1 hunk in a.rs");
    }
}
