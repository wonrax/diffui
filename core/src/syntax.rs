//! Syntax highlighting for diff lines: reconstruct each side of a file as one
//! document, highlight it once, map the spans back to per-line spans. The
//! highlighter itself lives in [`crate::highlights`].

use std::collections::HashMap;

use crate::{
    highlights::{self, Language},
    model::{DiffFile, DiffLineKind, SyntaxSpan},
};

/// Apply syntax highlighting to all visible diff lines for `file`, from the
/// diff's own lines alone. Equivalent to
/// [`apply_syntax_highlighting_with_sources`] with no sources — see there for
/// the mapping rules and the reconstruction's limitations.
pub fn apply_syntax_highlighting(file: &mut DiffFile) {
    apply_syntax_highlighting_with_sources(file, None, None);
}

/// Apply syntax highlighting to all visible diff lines for `file`.
///
/// We previously fed each line to the highlighter individually, which was
/// fundamentally wrong: the lexer carries state from line to line, so every
/// multi-line construct (string literals, doc comments, raw strings) was
/// invisible to it.
///
/// When a side's **full source** is provided, that side is parsed as the real
/// document and the spans are mapped to the diff's lines by their source
/// line numbers — the correct result even for constructs that span hunk
/// boundaries (a string opened above the hunk, an enclosing class, a block
/// comment cut mid-diff).
///
/// Without a source, the side is *reconstructed* from the diff's own lines:
/// one contiguous document with blank lines filling the gaps between hunks so
/// each surviving line sits at its true line number. The lexer recovers
/// local syntax within each hunk, but anything depending on the elided
/// regions comes out wrong —
/// which is why callers should prefer passing sources when they have them.
///
/// Context lines are highlighted from the new side (they're identical on
/// both sides, but we only need to look them up once); deletions come from
/// the old side; additions from the new side. Note/Conflict lines are
/// rendered as plain text — they aren't real source content.
pub fn apply_syntax_highlighting_with_sources(
    file: &mut DiffFile,
    old_source: Option<&str>,
    new_source: Option<&str>,
) {
    let Some(language) = highlights::detect_language(&file.path) else {
        return;
    };

    let new_spans = match new_source {
        Some(source) => parse_full_side(language, file, Side::New, source),
        None => parse_side(language, file, Side::New),
    };
    let old_spans = match old_source {
        Some(source) => parse_full_side(language, file, Side::Old, source),
        None => parse_side(language, file, Side::Old),
    };

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
                line.syntax = clamp_spans_to_content(spans, line.content.len());
            }
        }
    }
}

/// Clip spans to the diff line they'll be painted on. The full-source parse
/// measures against the file as stored, whose lines keep the `\r` of a CRLF
/// file that [`DiffLine::content`](crate::model::DiffLine::content) has
/// already dropped; the renderer discards any span reaching past the content,
/// so on a CRLF file every comment came out uncoloured.
fn clamp_spans_to_content(spans: &[SyntaxSpan], len: usize) -> Vec<SyntaxSpan> {
    spans
        .iter()
        .filter(|span| span.start < len)
        .map(|span| SyntaxSpan {
            start: span.start,
            end: span.end.min(len),
            kind: span.kind,
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// Highlight one side's **real, complete source** and slice its spans into
/// per-line span lists keyed by `(hunk_index, line_index)` — only for the
/// lines the diff actually shows on that side. Line numbers anchor the
/// mapping: the diff's `old_line`/`new_line` index straight into the
/// source's line table, so offsets agree even though the highlighter saw the
/// whole document.
fn parse_full_side(
    language: Language,
    file: &DiffFile,
    side: Side,
    source: &str,
) -> HashMap<(usize, usize), Vec<SyntaxSpan>> {
    // source line number (1-based) → the diff line showing it.
    let mut wanted: HashMap<usize, (usize, usize)> = HashMap::new();
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
            if let Some(target) = source_line {
                wanted.insert(target, (hunk_index, line_index));
            }
        }
    }
    if wanted.is_empty() {
        return HashMap::new();
    }

    // Byte offset of each source line's start; a trailing sentinel closes the
    // last line so every line is `starts[i]..starts[i + 1]`.
    let mut line_starts: Vec<usize> = Vec::with_capacity(source.len() / 32 + 2);
    line_starts.push(0);
    for (offset, _) in source.match_indices('\n') {
        line_starts.push(offset + 1);
    }
    line_starts.push(source.len() + 1);

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();
    for SyntaxSpan { start, end, kind } in highlights::highlight(language, source) {
        // First source line the span touches (0-based into `line_starts`),
        // then walk forward over every covered line.
        let first = line_starts
            .partition_point(|&line_start| line_start <= start)
            .saturating_sub(1);
        for line_idx in first.. {
            let Some(&line_start) = line_starts.get(line_idx) else {
                break;
            };
            if line_start >= end {
                break;
            }
            let line_end = line_starts
                .get(line_idx + 1)
                .map(|&next| next - 1)
                .unwrap_or(source.len());
            // 1-based in the diff's numbering.
            let Some(&(hunk_index, line_index)) = wanted.get(&(line_idx + 1)) else {
                continue;
            };
            let local_start = start.saturating_sub(line_start);
            let local_end = (end - line_start).min(line_end - line_start);
            if local_start >= local_end {
                continue;
            }
            per_line
                .entry((hunk_index, line_index))
                .or_default()
                .push(SyntaxSpan {
                    start: local_start,
                    end: local_end,
                    kind,
                });
        }
    }
    per_line
}

/// Reconstruct one side of `file` as a single document, highlight it, and
/// slice the resulting spans into per-line span lists keyed by
/// `(hunk_index, line_index)`.
fn parse_side(
    language: Language,
    file: &DiffFile,
    side: Side,
) -> HashMap<(usize, usize), Vec<SyntaxSpan>> {
    // For each line we keep on this side, record its byte range in the
    // reconstructed buffer so we can map spans back later.
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

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();

    for SyntaxSpan { start, end, kind } in highlights::highlight(language, &buf) {
        // Walk every line that the span overlaps.
        // `ranges` is sorted by start and end (the buffer only grows as lines
        // are appended), so the ranges overlapping this span form one contiguous
        // run. Binary-search to the first range ending past the span's start,
        // then stop as soon as a range begins at/after the span's end — instead
        // of scanning every line's range for every span.
        let first = ranges.partition_point(|range| range.end <= start);
        for range in &ranges[first..] {
            if range.start >= end {
                break;
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
    per_line
}

/// Highlight one diff file against its full old/new sources and return the
/// per-line spans sparsely as `(hunk, line, spans)`; empty when the language is
/// unknown.
///
/// Sparse because the caller applies these into a document it still owns —
/// shipping the whole file back would copy every line for the handful that
/// gained colour. Tens of milliseconds on a large file, so callers run it off
/// their UI thread.
pub fn highlight_file(
    mut file: DiffFile,
    old_source: Option<&str>,
    new_source: Option<&str>,
) -> Vec<(usize, usize, Vec<SyntaxSpan>)> {
    apply_syntax_highlighting_with_sources(&mut file, old_source, new_source);
    file.hunks
        .into_iter()
        .enumerate()
        .flat_map(|(hunk_index, hunk)| {
            hunk.lines
                .into_iter()
                .enumerate()
                .filter(|(_, line)| !line.syntax.is_empty())
                .map(move |(line_index, line)| (hunk_index, line_index, line.syntax))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiffFile, DiffFileStatus, DiffHunkView, DiffLine, SyntaxKind};

    fn context_line(old: usize, new: usize, content: &str) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Context,
            old_line: Some(old),
            new_line: Some(new),
            content: content.to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        }
    }

    /// The bug full-context highlighting fixes: a multi-line string opened
    /// above the hunk. The diff shows only a line from the string's middle —
    /// the reconstruction parses it as bare code, the real source knows it's
    /// string interior. A raw string, because highlights' Rust lexer drops a
    /// plain `"…"` string's state at the line break.
    #[test]
    fn full_source_highlights_across_hunk_boundaries() {
        let source =
            "const GREETING: &str = r#\"hello\nworld of let keywords\nbye\"#;\nfn main() {}\n";
        let make_file = || DiffFile {
            path: "test.rs".to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: vec![DiffHunkView {
                header: "@@ -2,1 +2,1 @@".to_owned(),
                lines: vec![context_line(2, 2, "world of let keywords")],
            }],
            additions: 0,
            deletions: 0,
            collapsed: None,
            blob: None,
        };

        let mut with_source = make_file();
        apply_syntax_highlighting_with_sources(&mut with_source, None, Some(source));
        let spans = &with_source.hunks[0].lines[0].syntax;
        assert!(
            spans
                .iter()
                .any(|span| span.kind == SyntaxKind::String && span.start == 0),
            "line inside a multi-line string should carry a String span from \
             its start; got {spans:?}",
        );
        assert!(
            !spans.iter().any(|span| span.kind == SyntaxKind::Keyword),
            "'let' inside a string must not highlight as a keyword; got {spans:?}",
        );

        // Without the source, the reconstruction sees the line as bare code —
        // 'let' wrongly reads as a keyword (this is the bug, pinned down so
        // the difference is visible; if a smarter fallback ever fixes it,
        // flip this assertion).
        let mut without_source = make_file();
        apply_syntax_highlighting(&mut without_source);
        let spans = &without_source.hunks[0].lines[0].syntax;
        assert!(
            !spans
                .iter()
                .any(|span| span.kind == SyntaxKind::String && span.start == 0),
            "reconstruction can't know about the enclosing string; got {spans:?}",
        );
    }

    /// A CRLF file: the source's lines are one byte longer than the diff's,
    /// which dropped the `\r`. A span reaching that byte is one the renderer
    /// throws away, so the whole line loses its colour.
    #[test]
    fn crlf_spans_clamp_to_the_diff_line() {
        let source = "// a comment\r\nlet x = 1;\r\n";
        let mut file = DiffFile {
            path: "test.rs".to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: vec![DiffHunkView {
                header: "@@ -1,2 +1,2 @@".to_owned(),
                lines: vec![
                    context_line(1, 1, "// a comment"),
                    context_line(2, 2, "let x = 1;"),
                ],
            }],
            additions: 0,
            deletions: 0,
            collapsed: None,
            blob: None,
        };
        apply_syntax_highlighting_with_sources(&mut file, None, Some(source));

        for line in &file.hunks[0].lines {
            assert!(
                line.syntax
                    .iter()
                    .all(|span| span.end <= line.content.len()),
                "spans must fit {:?}: {:?}",
                line.content,
                line.syntax,
            );
        }
        let comment = &file.hunks[0].lines[0].syntax;
        assert!(
            comment.iter().any(|span| span.kind == SyntaxKind::Comment),
            "comment line: {comment:?}",
        );
    }

    /// Spans from the real source must clip to each diff line and use
    /// line-local byte offsets.
    #[test]
    fn full_source_spans_are_line_local() {
        let source = "// a comment\nlet x = 1;\n";
        let mut file = DiffFile {
            path: "test.rs".to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: vec![DiffHunkView {
                header: "@@ -1,2 +1,2 @@".to_owned(),
                lines: vec![
                    context_line(1, 1, "// a comment"),
                    context_line(2, 2, "let x = 1;"),
                ],
            }],
            additions: 0,
            deletions: 0,
            collapsed: None,
            blob: None,
        };
        apply_syntax_highlighting_with_sources(&mut file, None, Some(source));
        let comment = &file.hunks[0].lines[0].syntax;
        assert!(
            comment
                .iter()
                .any(|span| span.kind == SyntaxKind::Comment && span.start == 0 && span.end <= 12),
            "comment line: {comment:?}",
        );
        let code = &file.hunks[0].lines[1].syntax;
        assert!(
            code.iter()
                .any(|span| span.kind == SyntaxKind::Keyword && span.start == 0 && span.end == 3),
            "'let' keyword at line-local 0..3: {code:?}",
        );
    }
}
