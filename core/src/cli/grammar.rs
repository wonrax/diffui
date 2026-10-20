//! jj's command grammar, read from jj-cli's own clap tree, and a scanner that
//! walks a word list through it the way clap would. Clap validates; the
//! scanner is what knows which word is which argument, so values keep their
//! spans and completion knows what sits under the cursor.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::LazyLock;

use clap::{Arg, ArgAction, Command};

use crate::completion::ValueKind;

/// jj-cli 0.45.1's `jj` command tree, built once (global flags propagated
/// into every subcommand, positional indices assigned).
pub(crate) fn app() -> &'static Command {
    static APP: LazyLock<Command> = LazyLock::new(|| {
        let mut app = jj_cli::commands::default_app()
            .color(clap::ColorChoice::Never)
            .bin_name("jj");
        app.build();
        app
    });
    &APP
}

/// Match `args` (after `jj`) the way jj-cli's `parse_args` does. The
/// command is kept per thread, so a keystroke doesn't clone the whole tree.
pub(crate) fn validate(args: &[String]) -> Result<clap::ArgMatches, clap::Error> {
    thread_local! {
        static VALIDATOR: std::cell::RefCell<Command> = std::cell::RefCell::new(
            app().clone().subcommand_required(true).arg_required_else_help(true),
        );
    }
    VALIDATOR.with_borrow_mut(|command| {
        command
            .try_get_matches_from_mut(std::iter::once("jj").chain(args.iter().map(String::as_str)))
    })
}

/// Every name the subcommand answers to, canonical first.
pub(crate) fn subcommand_names(command: &Command) -> impl Iterator<Item = &str> {
    std::iter::once(command.get_name()).chain(command.get_all_aliases())
}

pub(crate) fn find_subcommand<'a>(parent: &'a Command, name: &str) -> Option<&'a Command> {
    parent
        .get_subcommands()
        .find(|sub| subcommand_names(sub).any(|n| n == name))
}

/// Whether the argument consumes a value (`-r X`), as opposed to a switch.
pub(crate) fn takes_value(arg: &Arg) -> bool {
    !matches!(
        arg.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::HelpShort
            | ArgAction::HelpLong
            | ArgAction::Version
    ) && arg
        .get_num_args()
        .is_none_or(|range| range.max_values() > 0)
}

/// What a value of `arg` is, judged by the placeholder jj gives it: every
/// revset argument in jj-cli is `REVSET` or `REVSETS`, every path list
/// `FILESETS`, and so on.
pub(crate) fn value_kind(command: &Command, arg: &Arg) -> ValueKind {
    let name = arg
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.as_str())
        .unwrap_or_default();
    match name {
        "REVSET" | "REVSETS" => ValueKind::Revset,
        "FILESETS" | "PATH" | "PATHS" => ValueKind::Path,
        "MESSAGE" => ValueKind::Text,
        "REMOTE" | "REMOTES" => ValueKind::Remote,
        "BOOKMARK" | "BOOKMARK[@REMOTE]" | "BRANCH" | "OLD" => ValueKind::Bookmark,
        // `bookmark create/set/delete/forget/move <NAMES>`.
        "NAMES" if is_bookmark_subcommand(command) => ValueKind::Bookmark,
        _ => ValueKind::Other,
    }
}

fn is_bookmark_subcommand(command: &Command) -> bool {
    // A built subcommand's bin name is its full path, e.g. `jj bookmark set`.
    command
        .get_bin_name()
        .is_some_and(|bin| bin.split(' ').nth(1) == Some("bookmark"))
}

/// One argument occurrence the scanner recognised.
#[derive(Debug, Clone)]
pub(crate) struct Occurrence {
    /// Clap's id of the argument (`revisions`, `onto`, …).
    pub id: String,
    /// The word that named the flag; `None` for a positional.
    pub flag_word: Option<usize>,
    /// The value, when the argument takes one and it was given.
    pub value: Option<WordRange>,
}

/// A run of bytes inside one word of the scanned list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WordRange {
    pub word: usize,
    pub range: Range<usize>,
}

/// Where the scan stands: the command reached and what the next word means.
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    /// Root first, then each subcommand entered.
    pub commands: Vec<&'a Command>,
    /// Word indices of the subcommand names, parallel to `commands[1..]`.
    pub command_words: Vec<usize>,
    pub occurrences: Vec<Occurrence>,
    /// The word index of every word the scanner couldn't place.
    pub unknown: Vec<usize>,
    /// The option still waiting for its value at the end of the list.
    pub expecting: Option<&'a Arg>,
    /// Positional values given at the current command so far.
    pub positionals: usize,
    /// A bare `--` was seen: everything after is positional.
    pub after_double_dash: bool,
}

impl<'a> Scan<'a> {
    pub fn command(&self) -> &'a Command {
        self.commands.last().expect("the root is always there")
    }

    /// The subcommand path below `jj`, canonical names (`["git", "push"]`).
    pub fn path(&self) -> Vec<&'a str> {
        self.commands[1..].iter().map(|c| c.get_name()).collect()
    }

    /// Ids given at least once.
    pub fn used_ids(&self) -> HashSet<&str> {
        self.occurrences.iter().map(|o| o.id.as_str()).collect()
    }

    /// The positional argument the next positional value would fill.
    pub fn next_positional(&self) -> Option<&'a Arg> {
        positional_for(self.command(), self.positionals)
    }
}

/// The positional argument that value number `n` (0-based) fills.
pub(crate) fn positional_for(command: &Command, n: usize) -> Option<&Arg> {
    let mut positionals: Vec<&Arg> = command.get_positionals().collect();
    positionals.sort_by_key(|arg| arg.get_index().unwrap_or(usize::MAX));
    let mut remaining = n;
    for arg in positionals {
        let many = arg.get_num_args().is_some_and(|r| r.max_values() > 1)
            || matches!(arg.get_action(), ArgAction::Append);
        if many || remaining == 0 {
            return Some(arg);
        }
        remaining -= 1;
    }
    None
}

pub(crate) fn find_long<'a>(command: &'a Command, name: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

pub(crate) fn find_short(command: &Command, ch: char) -> Option<&Arg> {
    command.get_arguments().find(|arg| {
        arg.get_short() == Some(ch)
            || arg
                .get_all_short_aliases()
                .is_some_and(|aliases| aliases.contains(&ch))
    })
}

/// Walk `words` (the arguments after `jj`) through the grammar.
pub(crate) fn scan<'w>(words: impl IntoIterator<Item = &'w str>) -> Scan<'static> {
    let root = app();
    let mut scan = Scan {
        commands: vec![root],
        command_words: Vec::new(),
        occurrences: Vec::new(),
        unknown: Vec::new(),
        expecting: None,
        positionals: 0,
        after_double_dash: false,
    };
    for (index, word) in words.into_iter().enumerate() {
        if let Some(arg) = scan.expecting.take() {
            scan.occurrences.push(Occurrence {
                id: arg.get_id().to_string(),
                flag_word: index.checked_sub(1),
                value: Some(WordRange {
                    word: index,
                    range: 0..word.len(),
                }),
            });
            continue;
        }
        let command = scan.command();
        if !scan.after_double_dash && word == "--" {
            scan.after_double_dash = true;
            continue;
        }
        if !scan.after_double_dash
            && let Some(long) = word.strip_prefix("--")
        {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match find_long(command, name) {
                Some(arg) => {
                    let id = arg.get_id().to_string();
                    match (takes_value(arg), attached) {
                        (true, Some(value)) => scan.occurrences.push(Occurrence {
                            id,
                            flag_word: Some(index),
                            value: Some(WordRange {
                                word: index,
                                range: word.len() - value.len()..word.len(),
                            }),
                        }),
                        (true, None) => scan.expecting = Some(arg),
                        (false, _) => scan.occurrences.push(Occurrence {
                            id,
                            flag_word: Some(index),
                            value: None,
                        }),
                    }
                }
                None => scan.unknown.push(index),
            }
            continue;
        }
        if !scan.after_double_dash && word.len() > 1 && word.starts_with('-') {
            // A cluster of short flags; the first that takes a value eats the
            // rest of the word, or the next word.
            for (offset, ch) in word.char_indices().skip(1) {
                let Some(arg) = find_short(command, ch) else {
                    scan.unknown.push(index);
                    break;
                };
                let id = arg.get_id().to_string();
                if !takes_value(arg) {
                    scan.occurrences.push(Occurrence {
                        id,
                        flag_word: Some(index),
                        value: None,
                    });
                    continue;
                }
                let rest = offset + ch.len_utf8();
                let rest = rest + usize::from(word[rest..].starts_with('='));
                if rest < word.len() {
                    scan.occurrences.push(Occurrence {
                        id,
                        flag_word: Some(index),
                        value: Some(WordRange {
                            word: index,
                            range: rest..word.len(),
                        }),
                    });
                } else {
                    scan.expecting = Some(arg);
                }
                break;
            }
            continue;
        }
        if scan.positionals == 0
            && !scan.after_double_dash
            && let Some(sub) = find_subcommand(command, word)
        {
            scan.commands.push(sub);
            scan.command_words.push(index);
            continue;
        }
        match scan.next_positional() {
            Some(arg) => {
                scan.occurrences.push(Occurrence {
                    id: arg.get_id().to_string(),
                    flag_word: None,
                    value: Some(WordRange {
                        word: index,
                        range: 0..word.len(),
                    }),
                });
                scan.positionals += 1;
            }
            None => scan.unknown.push(index),
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values<'a>(scan: &Scan, words: &[&'a str], id: &str) -> Vec<&'a str> {
        scan.occurrences
            .iter()
            .filter(|o| o.id == id)
            .filter_map(|o| o.value.as_ref())
            .map(|v| &words[v.word][v.range.clone()])
            .collect()
    }

    #[test]
    fn scans_flags_values_and_subcommands() {
        let words = [
            "--ignore-immutable",
            "rebase",
            "-s",
            "a",
            "--onto=b",
            "-oc",
            "--destination",
            "d",
        ];
        let s = scan(words);
        assert_eq!(s.path(), ["rebase"]);
        assert_eq!(values(&s, &words, "source"), ["a"]);
        assert_eq!(values(&s, &words, "onto"), ["b", "c", "d"]);
        assert!(s.used_ids().contains("ignore_immutable"));
        assert!(s.unknown.is_empty());
        assert!(s.expecting.is_none());
    }

    #[test]
    fn positionals_fill_in_order_and_aliases_resolve() {
        let words = ["op", "restore", "abc123"];
        let s = scan(words);
        assert_eq!(s.path(), ["operation", "restore"]);
        assert_eq!(values(&s, &words, "operation"), ["abc123"]);

        let words = ["bookmark", "rename", "old", "new"];
        let s = scan(words);
        assert_eq!(values(&s, &words, "old"), ["old"]);
        assert_eq!(values(&s, &words, "new"), ["new"]);

        let words = ["new", "a", "b", "--", "-c"];
        let s = scan(words);
        assert_eq!(values(&s, &words, "revisions_pos"), ["a", "b", "-c"]);
    }

    #[test]
    fn an_option_at_the_end_is_still_expecting_its_value() {
        let expecting = scan(["rebase", "-s"]).expecting;
        assert_eq!(expecting.map(|a| a.get_id().as_str()), Some("source"));
        assert_eq!(scan(["diff", "--frobnicate"]).unknown, [1]);
    }

    #[test]
    fn value_kinds_follow_jj_placeholders() {
        let rebase = find_subcommand(app(), "rebase").unwrap();
        let onto = rebase
            .get_arguments()
            .find(|a| a.get_id() == "onto")
            .unwrap();
        assert_eq!(value_kind(rebase, onto), ValueKind::Revset);
        let bookmark = find_subcommand(app(), "bookmark").unwrap();
        let set = find_subcommand(bookmark, "set").unwrap();
        let names = set.get_arguments().find(|a| a.get_id() == "names").unwrap();
        assert_eq!(value_kind(set, names), ValueKind::Bookmark);
        let describe = find_subcommand(app(), "describe").unwrap();
        let message = describe
            .get_arguments()
            .find(|a| a.get_id() == "message_paragraphs")
            .unwrap();
        assert_eq!(value_kind(describe, message), ValueKind::Text);
        let push = find_subcommand(find_subcommand(app(), "git").unwrap(), "push").unwrap();
        let remote = push
            .get_arguments()
            .find(|a| a.get_id() == "remote")
            .unwrap();
        assert_eq!(value_kind(push, remote), ValueKind::Remote);
    }
}
