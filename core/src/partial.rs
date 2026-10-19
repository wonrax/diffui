//! Partial changes: picking files, hunks and lines out of a revision's diff.
//!
//! The selection types here are what the frontend's checkboxes and line
//! picks produce and what `jj split` / `jj squash -i` / `jj restore -i` style
//! mutations consume (see [`crate::mutations::MutationOp::Split`] and
//! friends). The text side — cutting a file's diff into hunks and splicing a
//! chosen subset of their lines onto the parent's content — lives here too,
//! independent of jj-lib's stores, so it can be tested byte for byte.
//!
//! Hunks are cut exactly the way the diff pane cuts them (jj's unified diff
//! with three lines of context), so "hunk 2" means the same lines in the
//! checkbox the user ticked and in the tree the backend writes. Each picked
//! hunk also carries the header it was displayed with; if the file changed
//! under the selection (a working-copy edit, a concurrent rewrite) the headers
//! stop matching and the operation is refused instead of applying the wrong
//! lines.
//!
//! Line picks follow `jj split -i`: a picked added line is part of the
//! selection and an unpicked one is left out of it; a picked removed line is
//! removed by the selection and an unpicked one stays.

use std::collections::BTreeSet;
use std::ops::Range;

use bstr::BStr;
use jj_lib::diff_presentation::{
    LineCompareMode,
    unified::{DiffLineType, unified_diff_hunks},
};
use jj_lib::merge::Diff;

use crate::diff_parse::format_hunk_header;
use crate::model::{DiffLine, DiffLineKind};

/// Lines of context around each hunk. Must match the diff pane's (see
/// `jj::diff::diff_jj_with_repo`), or hunk indices would name different lines
/// on the two sides.
pub const HUNK_CONTEXT: usize = 3;

/// A subset of one revision's changes, by file, hunk and line.
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
    /// Only these hunks (or lines of them) of a text change (never empty in a
    /// well-formed selection). The rest of the file keeps the parent's lines.
    Hunks(Vec<HunkRef>),
}

/// One hunk, by position in the file's diff, with the header it was shown
/// with as a staleness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRef {
    pub index: usize,
    pub header: String,
    /// `None` picks every changed line of the hunk; `Some` only these.
    pub lines: Option<Vec<LineRef>>,
}

/// One changed line of a hunk, by its 0-based index into the content of the
/// side it lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LineRef {
    /// A line of the parent's content that the change removes.
    Removed(usize),
    /// A line of the changed content that the change adds.
    Added(usize),
}

impl LineRef {
    /// The changed line a diff pane row shows, from its 1-based line
    /// numbers. `None` for context, conflict and note rows.
    pub fn of(line: &DiffLine) -> Option<Self> {
        match line.kind {
            DiffLineKind::Deletion => Some(Self::Removed(line.old_line?.checked_sub(1)?)),
            DiffLineKind::Addition => Some(Self::Added(line.new_line?.checked_sub(1)?)),
            _ => None,
        }
    }
}

impl DiffSelection {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Picked whole hunks, counting a whole-file pick as one.
    pub fn hunk_count(&self) -> usize {
        self.files
            .iter()
            .map(|file| match &file.hunks {
                HunkPick::All => 1,
                HunkPick::Hunks(hunks) => hunks.iter().filter(|hunk| hunk.lines.is_none()).count(),
            })
            .sum()
    }

    /// Lines picked out of partially picked hunks.
    pub fn line_count(&self) -> usize {
        self.files
            .iter()
            .filter_map(|file| match &file.hunks {
                HunkPick::All => None,
                HunkPick::Hunks(hunks) => Some(hunks),
            })
            .flatten()
            .filter_map(|hunk| hunk.lines.as_ref().map(Vec::len))
            .sum()
    }

    /// Whether any file is picked by hunk or line rather than whole.
    pub fn has_partial_files(&self) -> bool {
        self.files
            .iter()
            .any(|file| matches!(file.hunks, HunkPick::Hunks(_)))
    }

    /// "2 files" / "3 hunks in 2 files" / "1 hunk and 4 lines in a.rs" —
    /// what the activity log and the selection bar call this selection.
    pub fn summary(&self) -> String {
        let files = self.file_count();
        if !self.has_partial_files() {
            return match self.files.as_slice() {
                [only] => only.path.clone(),
                _ => format!("{files} files"),
            };
        }
        let count = |n: usize, noun: &str| match n {
            1 => format!("1 {noun}"),
            _ => format!("{n} {noun}s"),
        };
        let (hunks, lines) = (self.hunk_count(), self.line_count());
        let picked = match (hunks, lines) {
            (_, 0) => count(hunks, "hunk"),
            (0, _) => count(lines, "line"),
            _ => format!("{} and {}", count(hunks, "hunk"), count(lines, "line")),
        };
        match self.files.as_slice() {
            [only] => format!("{picked} in {}", only.path),
            _ => format!("{picked} in {files} files"),
        }
    }
}

/// One row of a [`TextHunk`], with its 0-based line index on the side it
/// is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkLine {
    /// Unchanged; the index is the parent's (both sides hold the same bytes).
    Context(usize),
    Removed(usize),
    Added(usize),
}

/// One hunk of a text diff: its line ranges on both sides (indices into the
/// content split after each `\n`, the last line possibly unterminated), its
/// `@@ -a,b +c,d @@` header, and its rows in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextHunk {
    pub left: Range<usize>,
    pub right: Range<usize>,
    pub header: String,
    pub lines: Vec<HunkLine>,
}

impl TextHunk {
    /// The hunk's changed lines.
    pub fn changed(&self) -> impl Iterator<Item = LineRef> + '_ {
        self.lines.iter().filter_map(|line| match *line {
            HunkLine::Context(_) => None,
            HunkLine::Removed(index) => Some(LineRef::Removed(index)),
            HunkLine::Added(index) => Some(LineRef::Added(index)),
        })
    }
}

/// Cut `before → after` into the hunks the diff pane shows.
pub fn text_hunks(before: &[u8], after: &[u8]) -> Vec<TextHunk> {
    unified_diff_hunks(
        Diff::new(BStr::new(before), BStr::new(after)),
        HUNK_CONTEXT,
        LineCompareMode::Exact,
    )
    .into_iter()
    .map(|hunk| {
        let (mut left, mut right) = (hunk.left_line_range.start, hunk.right_line_range.start);
        let lines = hunk
            .lines
            .iter()
            .map(|(kind, _)| match kind {
                DiffLineType::Context => {
                    left += 1;
                    right += 1;
                    HunkLine::Context(left - 1)
                }
                DiffLineType::Removed => {
                    left += 1;
                    HunkLine::Removed(left - 1)
                }
                DiffLineType::Added => {
                    right += 1;
                    HunkLine::Added(right - 1)
                }
            })
            .collect();
        TextHunk {
            header: format_hunk_header(&hunk.left_line_range, &hunk.right_line_range),
            left: hunk.left_line_range,
            right: hunk.right_line_range,
            lines,
        }
    })
    .collect()
}

/// Lines with their terminators, so joining them back is byte-exact: CRLF
/// stays CRLF and a missing final newline stays missing.
fn lines(content: &[u8]) -> Vec<&[u8]> {
    content.split_inclusive(|byte| *byte == b'\n').collect()
}

/// `before` with the changed lines `take` picks applied — the file content
/// of "the parent plus just those changes". `take(hunk, line)` is asked about
/// every changed line: a taken removal drops the parent's line, a taken
/// addition inserts the child's.
///
/// `hunks` must be [`text_hunks`] of the same two contents. Context lines
/// are byte-identical on both sides (the diff compares lines exactly), so
/// taking every line of a hunk yields exactly its `after` side.
pub fn apply_lines(
    before: &[u8],
    after: &[u8],
    hunks: &[TextHunk],
    take: impl Fn(usize, LineRef) -> bool,
) -> Vec<u8> {
    let left = lines(before);
    let right = lines(after);
    let mut out = Vec::with_capacity(before.len().max(after.len()));
    let mut push = |line: &[u8]| {
        // Only a side's last line can lack its terminator. Keeping the old
        // last line while taking lines added after it would fuse the two.
        if out.last().is_some_and(|byte| *byte != b'\n') {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
    };
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        for line in &left[cursor..hunk.left.start] {
            push(line);
        }
        for line in &hunk.lines {
            match *line {
                HunkLine::Context(at) => push(left[at]),
                HunkLine::Removed(at) => {
                    if !take(index, LineRef::Removed(at)) {
                        push(left[at]);
                    }
                }
                HunkLine::Added(at) => {
                    if take(index, LineRef::Added(at)) {
                        push(right[at]);
                    }
                }
            }
        }
        cursor = hunk.left.end;
    }
    for line in &left[cursor..] {
        push(line);
    }
    out
}

/// How much of one hunk a selection picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkMask {
    Nothing,
    Everything,
    /// Some of its changed lines (never none or all of them).
    Lines(BTreeSet<LineRef>),
}

impl HunkMask {
    pub fn picks(&self, line: LineRef) -> bool {
        match self {
            HunkMask::Nothing => false,
            HunkMask::Everything => true,
            HunkMask::Lines(lines) => lines.contains(&line),
        }
    }
}

/// Check `refs` against the file's actual hunks and turn them into a
/// per-hunk pick mask. Errors name the file so the user knows what to
/// reselect.
pub fn pick_mask(
    path: &str,
    hunks: &[TextHunk],
    refs: &[HunkRef],
) -> Result<Vec<HunkMask>, String> {
    let stale = || format!("the diff of {path} changed since it was picked; pick again");
    let mut picked: Vec<Option<BTreeSet<LineRef>>> = vec![Some(BTreeSet::new()); hunks.len()];
    for hunk_ref in refs {
        let hunk = match hunks.get(hunk_ref.index) {
            Some(hunk) if hunk.header == hunk_ref.header => hunk,
            _ => return Err(stale()),
        };
        let slot = &mut picked[hunk_ref.index];
        match (&hunk_ref.lines, slot.as_mut()) {
            (None, _) => *slot = None,
            (Some(_), None) => {}
            (Some(lines), Some(set)) => {
                for line in lines {
                    if !hunk.changed().any(|changed| changed == *line) {
                        return Err(stale());
                    }
                    set.insert(*line);
                }
            }
        }
    }
    Ok(picked
        .into_iter()
        .zip(hunks)
        .map(|(lines, hunk)| match lines {
            None => HunkMask::Everything,
            Some(lines) if lines.is_empty() => HunkMask::Nothing,
            Some(lines) if lines.len() == hunk.changed().count() => HunkMask::Everything,
            Some(lines) => HunkMask::Lines(lines),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(before: &str, after: &str, take: &[usize]) -> String {
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let bytes = apply_lines(before.as_bytes(), after.as_bytes(), &hunks, |index, _| {
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
            lines: None,
        };
        assert_eq!(
            pick_mask("f", &hunks, std::slice::from_ref(&good)),
            Ok(vec![HunkMask::Everything])
        );
        let moved = HunkRef {
            index: 0,
            header: "@@ -3,5 +3,5 @@".to_owned(),
            lines: None,
        };
        assert!(pick_mask("f", &hunks, &[moved]).is_err());
        let gone = HunkRef {
            index: 4,
            header: good.header,
            lines: None,
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
                            lines: None,
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

    /// `before → after` with only the changed lines `picks` names taken, as
    /// split/squash build it (`invert: false`) or as restore leaves the child
    /// (`invert: true`: everything but the picks).
    fn apply_picked(before: &str, after: &str, picks: &[LineRef], invert: bool) -> String {
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let refs: Vec<HunkRef> = hunks
            .iter()
            .enumerate()
            .filter_map(|(index, hunk)| {
                let lines: Vec<LineRef> = hunk.changed().filter(|l| picks.contains(l)).collect();
                (!lines.is_empty()).then(|| HunkRef {
                    index,
                    header: hunk.header.clone(),
                    lines: Some(lines),
                })
            })
            .collect();
        let mask = pick_mask("f", &hunks, &refs).expect("fresh refs");
        let bytes = apply_lines(
            before.as_bytes(),
            after.as_bytes(),
            &hunks,
            |index, line| mask[index].picks(line) != invert,
        );
        String::from_utf8(bytes).expect("utf-8")
    }

    fn text(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn hunk_rows_carry_their_line_indices() {
        let before = text(&["a", "b", "c", "d"]);
        let after = text(&["a", "B", "c", "d", "e"]);
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].lines,
            vec![
                HunkLine::Context(0),
                HunkLine::Removed(1),
                HunkLine::Added(1),
                HunkLine::Context(2),
                HunkLine::Context(3),
                HunkLine::Added(4),
            ]
        );
    }

    #[test]
    fn mixed_picks_inside_one_hunk_follow_jj_split() {
        // One hunk replacing three lines with three others.
        let before = text(&["keep", "old 1", "old 2", "old 3", "tail"]);
        let after = text(&["keep", "new 1", "new 2", "new 3", "tail"]);
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 1);

        // Pick removal of "old 2" and addition of "new 1" only: the unpicked
        // removals stay, the unpicked additions are left out, and the kept
        // old lines come before the taken new ones.
        let picks = [LineRef::Removed(2), LineRef::Added(1)];
        assert_eq!(
            apply_picked(&before, &after, &picks, false),
            text(&["keep", "old 1", "old 3", "new 1", "tail"])
        );
        // What restore leaves in the child: the child with exactly those
        // two changes undone.
        assert_eq!(
            apply_picked(&before, &after, &picks, true),
            text(&["keep", "old 2", "new 2", "new 3", "tail"])
        );

        // Only additions: nothing is removed.
        assert_eq!(
            apply_picked(&before, &after, &[LineRef::Added(3)], false),
            text(&["keep", "old 1", "old 2", "old 3", "new 3", "tail"])
        );
        // Only removals: nothing is added.
        assert_eq!(
            apply_picked(
                &before,
                &after,
                &[LineRef::Removed(1), LineRef::Removed(3)],
                false
            ),
            text(&["keep", "old 2", "tail"])
        );
    }

    #[test]
    fn line_picks_and_unpicked_hunks_mix_across_a_file() {
        let before = numbered(|_| None);
        let after = numbered(|n| match n {
            2 => Some("two"),
            18 => Some("eighteen"),
            _ => None,
        });
        // Just the addition of hunk 0: line 2 doubles up, hunk 1 untouched.
        let mut expected: Vec<String> = before.lines().map(|l| format!("{l}\n")).collect();
        expected.insert(2, "two\n".to_owned());
        assert_eq!(
            apply_picked(&before, &after, &[LineRef::Added(1)], false),
            expected.concat()
        );
        // Every changed line picked one by one is the child.
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let all: Vec<LineRef> = hunks.iter().flat_map(TextHunk::changed).collect();
        assert_eq!(apply_picked(&before, &after, &all, false), after);
        assert_eq!(apply_picked(&before, &after, &all, true), before);
    }

    #[test]
    fn line_picks_at_the_start_and_end_of_a_file() {
        let before = text(&["first", "a", "b", "c", "d", "e", "f", "g", "last"]);
        let after = text(&["FIRST", "a", "b", "c", "d", "e", "f", "g", "LAST", "extra"]);
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        assert_eq!(hunks[0].left.start, 0);
        assert_eq!(hunks[1].right.end, 10);

        // Top hunk: take the new first line but keep the old one too.
        assert_eq!(
            apply_picked(&before, &after, &[LineRef::Added(0)], false),
            text(&["first", "FIRST", "a", "b", "c", "d", "e", "f", "g", "last"])
        );
        // Bottom hunk: replace the last line, skip the extra one.
        assert_eq!(
            apply_picked(
                &before,
                &after,
                &[LineRef::Removed(8), LineRef::Added(8)],
                false
            ),
            text(&["first", "a", "b", "c", "d", "e", "f", "g", "LAST"])
        );
        // Bottom hunk: only the appended line.
        assert_eq!(
            apply_picked(&before, &after, &[LineRef::Added(9)], false),
            text(&["first", "a", "b", "c", "d", "e", "f", "g", "last", "extra"])
        );
    }

    #[test]
    fn a_kept_unterminated_last_line_does_not_fuse_with_added_lines() {
        let before = "a\nb\nc";
        let after = "a\nb\nc\nd\n";
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let added: Vec<LineRef> = hunks[0]
            .changed()
            .filter(|line| matches!(line, LineRef::Added(_)))
            .collect();
        assert_eq!(
            apply_picked(before, after, &added, false),
            "a\nb\nc\nc\nd\n"
        );
        // Taking only the removal of the unterminated line drops it cleanly.
        assert_eq!(
            apply_picked(before, after, &[LineRef::Removed(2)], false),
            "a\nb\n"
        );
    }

    #[test]
    fn line_masks_normalize_and_refuse_lines_the_hunk_does_not_change() {
        let before = text(&["a", "b", "c"]);
        let after = text(&["a", "B", "c"]);
        let hunks = text_hunks(before.as_bytes(), after.as_bytes());
        let lines = |lines: Vec<LineRef>| {
            vec![HunkRef {
                index: 0,
                header: hunks[0].header.clone(),
                lines: Some(lines),
            }]
        };
        assert_eq!(
            pick_mask("f", &hunks, &lines(vec![LineRef::Removed(1)])),
            Ok(vec![HunkMask::Lines(BTreeSet::from([LineRef::Removed(1)]))])
        );
        assert_eq!(
            pick_mask(
                "f",
                &hunks,
                &lines(vec![LineRef::Added(1), LineRef::Removed(1)])
            ),
            Ok(vec![HunkMask::Everything])
        );
        assert_eq!(
            pick_mask("f", &hunks, &lines(Vec::new())),
            Ok(vec![HunkMask::Nothing])
        );
        // Line 0 is context, and there is no added line 2 in this hunk.
        assert!(pick_mask("f", &hunks, &lines(vec![LineRef::Removed(0)])).is_err());
        assert!(pick_mask("f", &hunks, &lines(vec![LineRef::Added(2)])).is_err());
    }

    #[test]
    fn diff_pane_rows_name_their_changed_line() {
        let row = |kind, old_line, new_line| DiffLine {
            kind,
            old_line,
            new_line,
            content: String::new(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        };
        assert_eq!(
            LineRef::of(&row(DiffLineKind::Deletion, Some(3), None)),
            Some(LineRef::Removed(2))
        );
        assert_eq!(
            LineRef::of(&row(DiffLineKind::Addition, None, Some(1))),
            Some(LineRef::Added(0))
        );
        assert_eq!(
            LineRef::of(&row(DiffLineKind::Context, Some(1), Some(1))),
            None
        );
    }

    #[test]
    fn summary_counts_lines_beside_hunks() {
        let hunk = |index: usize, lines: Option<usize>| HunkRef {
            index,
            header: String::new(),
            lines: lines.map(|n| (0..n).map(LineRef::Added).collect()),
        };
        let file = |path: &str, hunks: Vec<HunkRef>| FileSelection {
            path: path.to_owned(),
            hunks: HunkPick::Hunks(hunks),
        };
        let one = DiffSelection {
            files: vec![file("a.rs", vec![hunk(0, Some(3))])],
        };
        assert_eq!(one.summary(), "3 lines in a.rs");
        let mixed = DiffSelection {
            files: vec![
                file("a.rs", vec![hunk(0, None), hunk(1, Some(1))]),
                file("b.rs", vec![hunk(0, Some(2))]),
            ],
        };
        assert_eq!(mixed.summary(), "1 hunk and 3 lines in 2 files");
    }
}
