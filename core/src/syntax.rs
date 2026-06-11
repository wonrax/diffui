//! Tree-sitter (arborium) syntax highlighting for diff lines: reconstruct each
//! side of a file as one document, parse once, map captures back to per-line
//! spans. Quarantines the arborium dependency.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use arborium::{
    GrammarStore,
    advanced::{CompiledGrammar, ParseContext},
};

use crate::model::{DiffFile, DiffLineKind, SyntaxKind, SyntaxSpan};

/// Apply syntax highlighting to all visible diff lines for `file`, from the
/// diff's own lines alone. Equivalent to
/// [`apply_syntax_highlighting_with_sources`] with no sources — see there for
/// the mapping rules and the reconstruction's limitations.
pub fn apply_syntax_highlighting(file: &mut DiffFile) {
    apply_syntax_highlighting_with_sources(file, None, None);
}

/// Apply syntax highlighting to all visible diff lines for `file`.
///
/// We previously fed each line to tree-sitter individually, which was
/// fundamentally wrong: tree-sitter expects a complete document, so a line
/// like `fn foo(` parses as an error, `}` on its own gets no captures, and
/// every multi-line construct (string literals, function bodies, doc
/// comments, raw strings) is invisible to the parser.
///
/// When a side's **full source** is provided, that side is parsed as the real
/// document and the captures are mapped to the diff's lines by their source
/// line numbers — the correct result even for constructs that span hunk
/// boundaries (a string opened above the hunk, an enclosing class, a block
/// comment cut mid-diff).
///
/// Without a source, the side is *reconstructed* from the diff's own lines:
/// one contiguous document with blank lines filling the gaps between hunks so
/// each surviving line sits at its true line number. Tree-sitter is
/// reasonably tolerant of the missing code and recovers local syntax within
/// each hunk, but anything depending on the elided regions parses wrong —
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
    static GRAMMAR_STORE: OnceLock<GrammarStore> = OnceLock::new();

    let Some(language) = arborium::detect_language(&file.path) else {
        return;
    };
    let store = GRAMMAR_STORE.get_or_init(GrammarStore::new);
    let Some(grammar) = store.get(language) else {
        return;
    };

    let new_spans = match new_source {
        Some(source) => parse_full_side(&grammar, file, Side::New, source),
        None => parse_side(&grammar, file, Side::New),
    };
    let old_spans = match old_source {
        Some(source) => parse_full_side(&grammar, file, Side::Old, source),
        None => parse_side(&grammar, file, Side::Old),
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

/// Parse one side's **real, complete source** and slice its captures into
/// per-line span lists keyed by `(hunk_index, line_index)` — only for the
/// lines the diff actually shows on that side. Line numbers anchor the
/// mapping: the diff's `old_line`/`new_line` index straight into the
/// source's line table, so offsets agree even though the parse saw the
/// whole document.
fn parse_full_side(
    grammar: &Arc<CompiledGrammar>,
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

    let Ok(mut context) = ParseContext::for_grammar(grammar) else {
        return HashMap::new();
    };
    let result = grammar.parse(&mut context, source);

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();
    for span in result.spans {
        let Some(kind) = syntax_kind_for_capture(&span.capture) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start >= end || end > source.len() {
            continue;
        }

        // First source line the span touches (0-based into `line_starts`),
        // then walk forward over every covered line — multi-line constructs
        // (block comments, raw strings) highlight each one.
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

    for spans in per_line.values_mut() {
        *spans = normalize_syntax_spans(std::mem::take(spans));
    }
    per_line
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
    use crate::model::{DiffFile, DiffFileStatus, DiffHunkView, DiffLine};

    fn context_line(old: usize, new: usize, content: &str) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Context,
            old_line: Some(old),
            new_line: Some(new),
            content: content.to_owned(),
            syntax: Vec::new(),
        }
    }

    /// The bug full-context highlighting fixes: a multi-line string opened
    /// above the hunk. The diff shows only a line from the string's middle —
    /// the reconstruction parses it as bare code, the real source knows it's
    /// string interior.
    #[test]
    fn full_source_highlights_across_hunk_boundaries() {
        let source = "const GREETING: &str = \"hello\nworld of let keywords\nbye\";\nfn main() {}\n";
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
