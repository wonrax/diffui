//! What the words of a jj command name: the revisions, bookmarks and
//! remotes in it, found through jj's grammar rather than by how a word looks.
//! `jj bookmark set push-sknyzssrnkun` names one bookmark, not a change id
//! inside one, because `bookmark set` takes bookmark names there.

use std::ops::Range;

use super::JjCli;
use crate::completion::ValueKind;
use crate::plan::NAME_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// A change id, a commit id, or a working copy (`@`, `ws@`).
    Revision,
    Bookmark,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marked {
    pub mark: Mark,
    pub range: Range<usize>,
}

/// The names in a command line, in order and not overlapping. A value is
/// read by the kind of argument it is given to: a revset's symbols, a
/// bookmark argument's `name@remote`, a remote argument. Values whose text
/// isn't on the line as typed (an alias's expansion, an escaped quote) stay
/// unmarked.
pub fn command(cli: &JjCli, line: &str) -> Vec<Marked> {
    let mut marks = Vec::new();
    for slot in cli.value_slots(line) {
        if slot.value.is_empty() || line.get(slot.span.clone()) != Some(slot.value.as_str()) {
            continue;
        }
        let at = slot.span.start;
        let found = match slot.kind {
            ValueKind::Revset => revset(&slot.value),
            ValueKind::Bookmark => bookmark(&slot.value),
            ValueKind::Remote => vec![Marked {
                mark: Mark::Remote,
                range: 0..slot.value.len(),
            }],
            ValueKind::Path | ValueKind::Text | ValueKind::Other => Vec::new(),
        };
        marks.extend(found.into_iter().map(|marked| Marked {
            mark: marked.mark,
            range: marked.range.start + at..marked.range.end + at,
        }));
    }
    marks.sort_by_key(|marked| marked.range.start);
    let mut end = 0;
    marks.retain(|marked| {
        let keep = marked.range.start >= end;
        if keep {
            end = marked.range.end;
        }
        keep
    });
    marks
}

/// A bookmark argument: `name`, or `name@remote`.
fn bookmark(value: &str) -> Vec<Marked> {
    match value.rsplit_once('@') {
        Some((name, remote)) if !name.is_empty() && !remote.is_empty() => vec![
            Marked {
                mark: Mark::Bookmark,
                range: 0..name.len(),
            },
            Marked {
                mark: Mark::Remote,
                range: name.len() + 1..value.len(),
            },
        ],
        _ => vec![Marked {
            mark: Mark::Bookmark,
            range: 0..value.len(),
        }],
    }
}

/// The names in a revset: its symbols read whole by jj's `identifier` rule,
/// so a change-id-looking part of a longer name isn't one. Marked are
/// symbols that are revision ids, working copies (`@`, `ws@`) and
/// `name@remote` pairs. A bare name could be a bookmark or a tag; without
/// the repository that can't be told, so it stays unmarked.
pub fn revset(text: &str) -> Vec<Marked> {
    let tokens = revset_tokens(text);
    let mut marks = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let next = tokens.get(index + 1);
        let adjacent = |other: Option<&Token>, kind: TokenKind| {
            other.is_some_and(|other| other.kind == kind && other.range.start == token.range.end)
        };
        match token.kind {
            TokenKind::Identifier if adjacent(next, TokenKind::Open) => {}
            TokenKind::Identifier if adjacent(next, TokenKind::At) => {
                let at = &tokens[index + 1];
                let remote = tokens.get(index + 2).filter(|remote| {
                    remote.kind == TokenKind::Identifier && remote.range.start == at.range.end
                });
                match remote {
                    Some(remote) => {
                        marks.push(Marked {
                            mark: Mark::Bookmark,
                            range: token.range.clone(),
                        });
                        marks.push(Marked {
                            mark: Mark::Remote,
                            range: remote.range.clone(),
                        });
                        index += 2;
                    }
                    // `ws@`: that workspace's working copy.
                    None => {
                        marks.push(Marked {
                            mark: Mark::Revision,
                            range: token.range.start..at.range.end,
                        });
                        index += 1;
                    }
                }
            }
            TokenKind::Identifier if names_revision(&text[token.range.clone()]) => {
                marks.push(Marked {
                    mark: Mark::Revision,
                    range: token.range.clone(),
                });
            }
            TokenKind::At => marks.push(Marked {
                mark: Mark::Revision,
                range: token.range.clone(),
            }),
            TokenKind::Identifier | TokenKind::Open => {}
        }
        index += 1;
    }
    marks
}

/// Whether a symbol is a revision id the way diffui writes them: a short
/// change id (jj's `k`–`z` alphabet) or a 12-digit commit id prefix.
pub fn names_revision(word: &str) -> bool {
    let len = word.len();
    let change_id =
        (NAME_CHARS..=32).contains(&len) && word.bytes().all(|b| (b'k'..=b'z').contains(&b));
    let commit_id = len == 12
        && word
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && word.bytes().any(|b| b.is_ascii_digit());
    change_id || commit_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Identifier,
    At,
    /// `(`: the identifier before it is a function.
    Open,
}

#[derive(Debug)]
struct Token {
    kind: TokenKind,
    range: Range<usize>,
}

/// The tokens of a revset the marks care about; operators, string literals
/// and whitespace are skipped.
fn revset_tokens(text: &str) -> Vec<Token> {
    let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | '*' | '/');
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            '@' => tokens.push(Token {
                kind: TokenKind::At,
                range: start..start + 1,
            }),
            '(' => tokens.push(Token {
                kind: TokenKind::Open,
                range: start..start + 1,
            }),
            '"' | '\'' => {
                let mut escaped = false;
                for (_, inner) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if inner == '\\' && c == '"' {
                        escaped = true;
                    } else if inner == c {
                        break;
                    }
                }
            }
            c if part(c) => {
                // jj's `identifier`: parts joined by `.`, a run of `-`, or
                // `+`, never ending on a joiner (`a..b` is a range).
                let mut end = start + c.len_utf8();
                loop {
                    let rest = &text[end..];
                    let mut joined = rest.chars();
                    let first = joined.next();
                    let joiner_len = match first {
                        Some(c) if part(c) => {
                            end += c.len_utf8();
                            chars.next();
                            continue;
                        }
                        Some('-') => rest.len() - rest.trim_start_matches('-').len(),
                        Some('.' | '+') => 1,
                        _ => break,
                    };
                    if !rest[joiner_len..].starts_with(part) {
                        break;
                    }
                    for _ in 0..joiner_len {
                        chars.next();
                    }
                    end += joiner_len;
                }
                tokens.push(Token {
                    kind: TokenKind::Identifier,
                    range: start..end,
                });
            }
            _ => {}
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(line: &str) -> Vec<(Mark, &str)> {
        command(&JjCli::default(), line)
            .into_iter()
            .map(|marked| (marked.mark, &line[marked.range]))
            .collect()
    }

    #[test]
    fn a_bookmark_name_is_one_name_whatever_it_contains() {
        assert_eq!(
            marks("jj bookmark set push-sknyzssrnkun -r ed200ca71ab8"),
            [
                (Mark::Bookmark, "push-sknyzssrnkun"),
                (Mark::Revision, "ed200ca71ab8"),
            ]
        );
        // Moved with `--to`, and two at once.
        assert_eq!(
            marks("jj bookmark move zzzzzzzz-kkkkkkkk main --to kmnopqrs"),
            [
                (Mark::Bookmark, "zzzzzzzz-kkkkkkkk"),
                (Mark::Bookmark, "main"),
                (Mark::Revision, "kmnopqrs"),
            ]
        );
    }

    #[test]
    fn remote_bookmarks_split_into_name_and_remote() {
        assert_eq!(
            marks("jj bookmark track push-sknyzssrnkun@origin"),
            [
                (Mark::Bookmark, "push-sknyzssrnkun"),
                (Mark::Remote, "origin"),
            ]
        );
        assert_eq!(
            marks("jj log -r 'push-sknyzssrnkun@origin | sknyzssrnkun'"),
            [
                (Mark::Bookmark, "push-sknyzssrnkun"),
                (Mark::Remote, "origin"),
                (Mark::Revision, "sknyzssrnkun"),
            ]
        );
        assert_eq!(
            marks("jj git push --remote upstream -b main"),
            [(Mark::Remote, "upstream"), (Mark::Bookmark, "main")]
        );
    }

    #[test]
    fn revisions_inside_revsets_are_whole_symbols() {
        assert_eq!(
            marks("jj rebase -s kmnopqrs -o 'trunk()..0123456789ab'"),
            [
                (Mark::Revision, "kmnopqrs"),
                (Mark::Revision, "0123456789ab"),
            ]
        );
        // `@`, a workspace's `ws@`, and an id glued to `-` (a parent).
        assert_eq!(
            marks("jj new @ 'default@' kmnopqrs-"),
            [
                (Mark::Revision, "@"),
                (Mark::Revision, "default@"),
                (Mark::Revision, "kmnopqrs"),
            ]
        );
        // A name with an id-shaped tail is a name; functions aren't ids.
        assert!(marks("jj new feat-kmnopqrs 'ancestors(xyz)'").is_empty());
    }

    #[test]
    fn values_that_name_nothing_stay_unmarked() {
        // A message and paths, however id-like, and a multi-line message.
        assert_eq!(
            marks("jj describe -r kmnopqrs -m 'fix zzzzzzzz\n\n0123456789ab'"),
            [(Mark::Revision, "kmnopqrs")]
        );
        assert!(marks("jj diff kmnopqrs").is_empty());
        // Not a jj command at all.
        assert!(marks("jj frobnicate kmnopqrs").is_empty());
    }

    #[test]
    fn revsets_on_their_own() {
        let revset_marks = |text: &'static str| -> Vec<(Mark, &'static str)> {
            revset(text)
                .into_iter()
                .map(|marked| (marked.mark, &text[marked.range]))
                .collect()
        };
        assert_eq!(
            revset_marks("present(@) | ancestors(immutable_heads().., 2) | trunk()"),
            [(Mark::Revision, "@")]
        );
        assert_eq!(
            revset_marks("kmnopqrs::\"zzzzzzzz\" ~ v1.2-kmnopqrs+x"),
            [(Mark::Revision, "kmnopqrs")]
        );
    }
}
