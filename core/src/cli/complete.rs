//! Completion for a `jj …` line: subcommands (with aliases), flags with
//! their help, and, for a value position, whatever the [`ValueCompleter`]
//! offers for that argument's [`ValueKind`]. Pure: no repository access,
//! so it runs on every keystroke.

use std::ops::Range;

use clap::{Arg, Command};

use super::grammar::{self, Scan};
use super::parse::{CliConfig, Line, Word, expand_aliases};
use super::tokenize::{Token, shell_quote};
use crate::completion::{Completion, CompletionKind, ValueCompleter, ValueKind};

/// Candidates for the word under `cursor` (a byte offset into `input`).
/// Every [`Completion::replace`] is a range of `input`.
pub fn complete(
    config: &CliConfig,
    input: &str,
    cursor: usize,
    values: &dyn ValueCompleter,
) -> Vec<Completion> {
    let cursor = cursor.min(input.len());
    let line = Line::new(input);
    // The token the cursor is in or touching; else a fresh one at the cursor.
    let at = line
        .tokens
        .iter()
        .position(|t| t.span.start <= cursor && cursor <= t.span.end);
    let before = at.unwrap_or_else(|| {
        line.tokens
            .iter()
            .take_while(|t| t.span.end < cursor)
            .count()
    });
    if line.tokens.is_empty() || before < line.first {
        // Nothing typed at all, or the cursor is on the leading `jj`.
        return Vec::new();
    }
    let fresh;
    let token = match at {
        Some(i) => &line.tokens[i],
        None => {
            fresh = Token::empty_at(cursor);
            &fresh
        }
    };
    let words: Vec<Word> = (line.first..before).map(|i| line.word(i)).collect();
    let words = expand_aliases(config, words.clone(), &line).unwrap_or(words);
    let scan = grammar::scan(words.iter().map(|w| w.value.as_str()));
    let context = Context {
        input,
        token,
        cursor: token.value_offset(cursor),
        values,
    };
    context.complete(config, &scan)
}

struct Context<'a> {
    input: &'a str,
    token: &'a Token,
    /// The cursor as an offset into the token's value.
    cursor: usize,
    values: &'a dyn ValueCompleter,
}

impl Context<'_> {
    fn text(&self) -> &str {
        &self.token.value
    }

    /// The whole word, as an input range.
    fn word_range(&self) -> Range<usize> {
        self.token.input_range(0..self.token.value.len())
    }

    /// A trailing space for a finished word, unless one is already there.
    fn finish(&self, word: String) -> String {
        let end = self.word_range().end;
        let next_is_space = self.input[end..]
            .chars()
            .find(|c| *c != '\'' && *c != '"')
            .is_some_and(char::is_whitespace);
        if next_is_space { word } else { word + " " }
    }

    fn complete(&self, config: &CliConfig, scan: &Scan) -> Vec<Completion> {
        let command = scan.command();
        if let Some(arg) = scan.expecting {
            return self.values_for(command, arg, 0);
        }
        let text = self.text();
        if !scan.after_double_dash {
            if let Some(long) = text.strip_prefix("--")
                && let Some((name, _)) = long.split_once('=')
            {
                return match grammar::find_long(command, name) {
                    Some(arg) => self.values_for(command, arg, name.len() + 3),
                    None => Vec::new(),
                };
            }
            if let Some(short) = text.strip_prefix('-') {
                // `-rfoo`: a short option with its value attached.
                if let Some(ch) = short.chars().next()
                    && ch != '-'
                    && short.len() > ch.len_utf8()
                    && let Some(arg) = grammar::find_short(command, ch)
                    && grammar::takes_value(arg)
                {
                    let skip = 1 + ch.len_utf8();
                    let skip = skip + usize::from(text[skip..].starts_with('='));
                    return self.values_for(command, arg, skip);
                }
                return self.flags(scan);
            }
        }
        let has_subcommands = command.get_subcommands().next().is_some();
        if has_subcommands && scan.positionals == 0 && !scan.after_double_dash {
            return self.subcommands(command, scan.commands.len() == 1, config);
        }
        let mut out = match scan.next_positional() {
            Some(arg) => self.values_for(command, arg, 0),
            None => Vec::new(),
        };
        if text.is_empty() && !scan.after_double_dash {
            out.extend(self.flags(scan));
        }
        out
    }

    fn subcommands(&self, command: &Command, root: bool, config: &CliConfig) -> Vec<Completion> {
        let prefix = &self.text()[..self.cursor.min(self.text().len())];
        let mut out = Vec::new();
        for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
            let names = std::iter::once(sub.get_name()).chain(sub.get_visible_aliases());
            let Some(name) = names.into_iter().find(|n| n.starts_with(prefix)) else {
                continue;
            };
            let aliases: Vec<&str> = sub.get_visible_aliases().collect();
            let label = if aliases.is_empty() {
                sub.get_name().to_owned()
            } else {
                format!("{} ({})", sub.get_name(), aliases.join(", "))
            };
            out.push(Completion {
                replace: self.word_range(),
                insert: self.finish(name.to_owned()),
                label,
                detail: first_line(sub.get_about().map(|s| s.to_string())),
                kind: CompletionKind::Subcommand,
            });
        }
        if root {
            for (alias, definition) in &config.aliases {
                if alias.starts_with(prefix) {
                    out.push(Completion {
                        replace: self.word_range(),
                        insert: self.finish(alias.clone()),
                        label: alias.clone(),
                        detail: format!(
                            "jj {}",
                            definition
                                .iter()
                                .map(|w| shell_quote(w))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        kind: CompletionKind::Subcommand,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.label.cmp(&b.label));
        out
    }

    fn flags(&self, scan: &Scan) -> Vec<Completion> {
        let command = scan.command();
        let text = self.text();
        let used = scan.used_ids();
        let long_only = text.starts_with("--");
        let mut local = Vec::new();
        let mut global = Vec::new();
        for arg in command.get_arguments() {
            if arg.is_positional() || arg.is_hide_set() {
                continue;
            }
            if used.contains(arg.get_id().as_str()) && !repeatable(arg) {
                continue;
            }
            let long = arg.get_long().map(|l| format!("--{l}"));
            let short = arg.get_short().map(|s| format!("-{s}"));
            let matches = match (text, &long, &short) {
                ("" | "-", _, _) => true,
                (_, Some(long), _) if long.starts_with(text) => true,
                (_, _, Some(short)) if !long_only && short == text => true,
                _ => false,
            };
            if !matches {
                continue;
            }
            let insert = match (&short, &long) {
                (Some(short), _) if !long_only => short.clone(),
                (_, Some(long)) => long.clone(),
                (Some(short), None) => short.clone(),
                (None, None) => continue,
            };
            let label = match (&short, &long) {
                (Some(short), Some(long)) => format!("{short}, {long}"),
                (Some(flag), None) | (None, Some(flag)) => flag.clone(),
                (None, None) => continue,
            };
            let label = match arg
                .get_value_names()
                .and_then(|names| names.first())
                .filter(|_| grammar::takes_value(arg))
            {
                Some(name) => format!("{label} <{name}>"),
                None => label,
            };
            let completion = Completion {
                replace: self.word_range(),
                insert: self.finish(insert),
                label,
                detail: first_line(arg.get_help().map(|s| s.to_string())),
                kind: CompletionKind::Flag,
            };
            if arg.is_global_set() {
                global.push(completion);
            } else {
                local.push(completion);
            }
        }
        local.extend(global);
        local
    }

    /// Candidates for `arg`'s value, which starts `skip` bytes into the
    /// token (after `--onto=` or `-o`).
    fn values_for(&self, command: &Command, arg: &Arg, skip: usize) -> Vec<Completion> {
        let text = self.text();
        let skip = skip.min(text.len());
        let value = &text[skip..];
        let cursor = self.cursor.saturating_sub(skip).min(value.len());
        let possible: Vec<_> = arg
            .get_possible_values()
            .into_iter()
            .filter(|v| !v.is_hide_set())
            .collect();
        if !possible.is_empty() {
            return possible
                .iter()
                .filter(|v| v.get_name().starts_with(&value[..cursor]))
                .map(|v| Completion {
                    replace: self.token.input_range(skip..text.len()),
                    insert: self.finish(v.get_name().to_owned()),
                    label: v.get_name().to_owned(),
                    detail: first_line(v.get_help().map(|s| s.to_string())),
                    kind: CompletionKind::Value,
                })
                .collect();
        }
        let kind = grammar::value_kind(command, arg);
        if kind == ValueKind::Text {
            return Vec::new();
        }
        let quoted = self.token.is_quoted();
        self.values
            .complete(kind, value, cursor)
            .into_iter()
            .map(|mut completion| {
                let replace = completion.replace.start.min(value.len())
                    ..completion.replace.end.min(value.len());
                let needs_quotes = !quoted
                    && completion
                        .insert
                        .contains(|c: char| c.is_whitespace() || c == '\'' || c == '"');
                if needs_quotes {
                    // Rewrite the whole value quoted, so the word stays one.
                    let mut new_value = value.to_owned();
                    new_value.replace_range(replace, &completion.insert);
                    completion.insert = shell_quote(&new_value);
                    completion.replace = self.token.input_range(skip..text.len());
                } else {
                    completion.replace = self
                        .token
                        .input_range(skip + replace.start..skip + replace.end);
                }
                completion
            })
            .collect()
    }
}

fn repeatable(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Append | clap::ArgAction::Count
    )
}

fn first_line(text: Option<String>) -> String {
    text.as_deref()
        .and_then(|t| t.lines().next())
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Revsets complete to `trunk()` / `kkkk`, bookmarks to `main`, each
    /// recording the kind it was asked for.
    struct Fake;

    impl ValueCompleter for Fake {
        fn complete(&self, kind: ValueKind, text: &str, cursor: usize) -> Vec<Completion> {
            let words: &[&str] = match kind {
                ValueKind::Revset => &["trunk()", "kkkk"],
                ValueKind::Bookmark => &["main"],
                ValueKind::Path => &["src/a b.rs"],
                _ => &[],
            };
            words
                .iter()
                .filter(|w| w.starts_with(&text[..cursor]))
                .map(|w| Completion {
                    replace: 0..text.len(),
                    insert: (*w).to_owned(),
                    label: (*w).to_owned(),
                    detail: format!("{kind:?}"),
                    kind: CompletionKind::Revision,
                })
                .collect()
        }
    }

    fn complete_at_end(input: &str) -> Vec<Completion> {
        complete(&CliConfig::builtin(), input, input.len(), &Fake)
    }

    fn labels(completions: &[Completion]) -> Vec<&str> {
        completions.iter().map(|c| c.label.as_str()).collect()
    }

    /// Apply the first candidate.
    fn accept(input: &str, completions: &[Completion]) -> String {
        let first = completions.first().expect("a candidate");
        let mut out = input.to_owned();
        out.replace_range(first.replace.clone(), &first.insert);
        out
    }

    #[test]
    fn subcommands_with_help_and_aliases() {
        let out = complete_at_end("jj reb");
        assert_eq!(labels(&out), ["rebase"]);
        assert_eq!(out[0].kind, CompletionKind::Subcommand);
        assert!(
            out[0].detail.starts_with("Move revisions"),
            "{:?}",
            out[0].detail
        );
        assert_eq!(accept("jj reb", &out), "jj rebase ");

        // jj's default aliases complete too, with what they expand to.
        let out = complete_at_end("jj c");
        assert!(labels(&out).contains(&"ci"), "{:?}", labels(&out));
        let ci = out.iter().find(|c| c.label == "ci").unwrap();
        assert_eq!(ci.detail, "jj commit");

        // Nested: `git` lists its own subcommands, `op` resolves.
        assert!(labels(&complete_at_end("jj git ")).contains(&"push"));
        assert_eq!(labels(&complete_at_end("jj op re")), ["restore", "revert"]);
        // Nothing for the leading `jj` itself, or an empty line.
        assert!(complete_at_end("jj").is_empty());
        assert!(complete_at_end("").is_empty());
    }

    #[test]
    fn flags_with_help_skip_the_ones_used() {
        let out = complete_at_end("jj rebase --on");
        assert_eq!(labels(&out), ["-o, --onto <REVSETS>"]);
        assert_eq!(accept("jj rebase --on", &out), "jj rebase --onto ");
        assert_eq!(out[0].kind, CompletionKind::Flag);
        assert!(!out[0].detail.is_empty());

        let out = complete_at_end("jj rebase -");
        let all = labels(&out);
        assert!(all.contains(&"-s, --source <REVSETS>"));
        assert!(all.contains(&"--skip-emptied"));
        // Short insert when the user is typing short flags.
        let source = out.iter().find(|c| c.label.starts_with("-s,")).unwrap();
        assert_eq!(source.insert, "-s ");
        // Command flags first, globals after.
        let first_global = all.iter().position(|l| l.contains("--repository")).unwrap();
        let last_local = all
            .iter()
            .rposition(|l| l.contains("--skip-emptied"))
            .unwrap();
        assert!(last_local < first_global);

        // A one-shot flag already given isn't offered again.
        let out = complete_at_end("jj describe --stdin --");
        assert!(!labels(&out).contains(&"--stdin"));
    }

    #[test]
    fn values_delegate_by_kind_and_map_back() {
        // After a revset flag.
        let input = "jj rebase -s tr";
        let out = complete_at_end(input);
        assert_eq!(labels(&out), ["trunk()"]);
        assert_eq!(out[0].detail, "Revset");
        assert_eq!(accept(input, &out), "jj rebase -s trunk()");

        // Attached forms.
        assert_eq!(
            accept(
                "jj rebase --onto=tr",
                &complete_at_end("jj rebase --onto=tr")
            ),
            "jj rebase --onto=trunk()"
        );
        assert_eq!(
            accept("jj rebase -otr", &complete_at_end("jj rebase -otr")),
            "jj rebase -otrunk()"
        );

        // Inside quotes the replacement stays inside them.
        let input = "jj log -r 'tr";
        assert_eq!(accept(input, &complete_at_end(input)), "jj log -r 'trunk()");

        // A positional revset, an empty word: values first, then flags.
        let out = complete_at_end("jj new ");
        assert_eq!(&labels(&out)[..2], ["trunk()", "kkkk"]);
        assert!(labels(&out).iter().any(|l| l.starts_with("-A")));

        // Bookmark names for `bookmark set`, nothing for `-m`.
        assert_eq!(labels(&complete_at_end("jj b set ma")), ["main"]);
        assert!(complete_at_end("jj describe -m ").is_empty());

        // A path with a space gets quoted.
        let input = "jj diff sr";
        assert_eq!(
            accept(input, &complete_at_end(input)),
            "jj diff 'src/a b.rs'"
        );
    }

    #[test]
    fn fixed_choices_complete_as_values() {
        let out = complete_at_end("jj log --color ne");
        assert_eq!(labels(&out), ["never"]);
        assert_eq!(out[0].kind, CompletionKind::Value);
    }

    #[test]
    fn cursor_in_the_middle_completes_that_word() {
        let input = "jj reb -s x";
        let out = complete(&CliConfig::builtin(), input, 6, &Fake);
        assert_eq!(labels(&out), ["rebase"]);
        // The next character is already a space: no second one.
        assert_eq!(accept(input, &out), "jj rebase -s x");
    }

    #[test]
    fn user_aliases_set_the_context() {
        let mut config = CliConfig::builtin();
        config
            .aliases
            .insert("rb".into(), vec!["rebase".into(), "-s".into()]);
        let out = complete(&config, "jj rb tr", 8, &Fake);
        assert_eq!(labels(&out), ["trunk()"]);
        let out = complete(&config, "jj r", 4, &Fake);
        assert!(labels(&out).contains(&"rb"));
    }
}
