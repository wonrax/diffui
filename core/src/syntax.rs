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
