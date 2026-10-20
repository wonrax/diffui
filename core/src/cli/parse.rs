//! From a typed line to a [`ParsedCommand`]: tokenise, expand aliases and
//! the default command the way jj-cli 0.45.1 does, validate with jj's own
//! clap tree, then classify.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use jj_lib::config::{ConfigGetResultExt as _, StackedConfig};

use super::command::{
    CliCommand, MutationCommand, RevsetArg, RevsetDestination, RevsetSlot, Spanned, UiCommand,
    ViewCommand,
};
use super::grammar::{self, Scan};
use super::tokenize::{self, Token};
use crate::completion::ValueKind;
use crate::mutations::RebaseSourceMode;
use crate::push::PushAllow;

/// The slice of the jj config the command line needs: `[aliases]` and
/// `ui.default-command`, with jj-cli's own defaults (`b`, `ci`, `desc`,
/// `st`; `log`) underneath the user's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    pub aliases: BTreeMap<String, Vec<String>>,
    pub default_command: Vec<String>,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self::builtin()
    }
}

impl CliConfig {
    /// jj-cli's shipped defaults only.
    pub fn builtin() -> Self {
        static BUILTIN: LazyLock<CliConfig> = LazyLock::new(|| {
            let mut config = StackedConfig::empty();
            config.extend_layers(jj_cli::config::default_config_layers());
            let mut cli = CliConfig {
                aliases: BTreeMap::new(),
                default_command: vec!["log".to_owned()],
            };
            cli.overlay(&config);
            cli
        });
        BUILTIN.clone()
    }

    /// jj-cli's defaults with the user's config on top. `settings` is what
    /// diffui already resolved for the repository (see
    /// `jj::settings::jj_settings`), which lacks jj-cli's default layer.
    pub fn from_settings(settings: &jj_lib::settings::UserSettings) -> Self {
        let mut cli = Self::builtin();
        cli.overlay(settings.config());
        cli
    }

    /// [`from_settings`](Self::from_settings) for the workspace at `root`.
    pub fn load(repo_root: &Path) -> anyhow::Result<Self> {
        Ok(Self::from_settings(&crate::jj::settings::jj_settings(
            repo_root,
        )?))
    }

    fn overlay(&mut self, config: &StackedConfig) {
        for name in config.table_keys("aliases") {
            // A real command can't be shadowed; jj warns and drops it.
            if grammar::find_subcommand(grammar::app(), name).is_some() {
                continue;
            }
            let definition = config
                .get::<Vec<String>>(["aliases", name])
                .or_else(|_| config.get::<Vec<String>>(["aliases", name, "definition"]));
            if let Ok(definition) = definition {
                self.aliases.insert(name.to_owned(), definition);
            }
        }
        if let Ok(command) = config.get::<String>("ui.default-command") {
            self.default_command = vec![command];
        } else if let Ok(Some(command)) = config.get::<Vec<String>>("ui.default-command").optional()
        {
            self.default_command = command;
        }
    }
}

/// A parse failure, pointing into the typed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// One line, jj's wording where jj has one.
    pub message: String,
    /// What to underline. Empty at the end of the line for "missing …".
    pub span: Range<usize>,
    /// clap's "tip: …" line, if it had one.
    pub hint: Option<String>,
}

/// A successfully parsed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub command: CliCommand,
    /// The subcommand path after alias expansion, canonical names
    /// (`["git", "push"]`, `["operation", "log"]`).
    pub path: Vec<String>,
    /// The arguments after alias and default-command expansion, without the
    /// leading `jj`: what jj itself would run.
    pub args: Vec<String>,
    /// The words as typed, without the leading `jj`: what a passthrough run
    /// hands the CLI (which expands aliases itself).
    pub passthrough_args: Vec<String>,
    /// Every revset argument, for live counts and graph tinting.
    pub revsets: Vec<RevsetSlot>,
    /// `--ignore-immutable` was given: run the mutation with the
    /// immutable-commit guards off.
    pub ignore_immutable: bool,
}

/// Where an expanded word came from, for spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Typed as this token.
    Typed(usize),
    /// Produced by expanding the alias typed as this token.
    Alias(usize),
    /// `ui.default-command`, inserted because no subcommand was typed.
    Default,
}

#[derive(Debug, Clone)]
pub(crate) struct Word {
    pub value: String,
    origin: Origin,
}

/// The words after the optional leading `jj`, with their tokens.
pub(crate) struct Line {
    pub tokens: Vec<Token>,
    /// Index of the first token after `jj`.
    pub first: usize,
    pub len: usize,
}

impl Line {
    pub fn new(input: &str) -> Self {
        let tokens = tokenize::tokenize(input);
        let first = usize::from(tokens.first().is_some_and(|t| t.value == "jj"));
        Self {
            tokens,
            first,
            len: input.len(),
        }
    }

    /// Token `i` as a word.
    pub fn word(&self, i: usize) -> Word {
        Word {
            value: self.tokens[i].value.clone(),
            origin: Origin::Typed(i),
        }
    }

    fn typed_words(&self) -> Vec<Word> {
        (self.first..self.tokens.len())
            .map(|i| self.word(i))
            .collect()
    }

    /// Where everything after `jj` sits: the fallback span.
    fn rest_span(&self) -> Range<usize> {
        match self.tokens.get(self.first) {
            Some(token) => token.span.start..self.len,
            None => self.len..self.len,
        }
    }

    fn word_span(&self, word: &Word) -> Range<usize> {
        match word.origin {
            Origin::Typed(i) | Origin::Alias(i) => self.tokens[i].span.clone(),
            Origin::Default => self.rest_span(),
        }
    }

    /// The input span of `range` inside `word`; the whole alias for a word
    /// an alias produced.
    fn range_span(&self, word: &Word, range: Range<usize>) -> Range<usize> {
        match word.origin {
            Origin::Typed(i) => self.tokens[i].input_range(range),
            _ => self.word_span(word),
        }
    }
}

/// jj's alias expansion (`cli_util::resolve_aliases`): while the first
/// subcommand isn't a real one, splice in its alias definition. Every word
/// an alias produces points back at the alias token.
pub(crate) fn expand_aliases(
    config: &CliConfig,
    mut words: Vec<Word>,
    line: &Line,
) -> Result<Vec<Word>, ParseError> {
    let mut recursion_stack: Vec<(String, Range<usize>)> = Vec::new();
    loop {
        let scan = grammar::scan(words.iter().map(|w| w.value.as_str()));
        if !scan.command_words.is_empty() {
            return Ok(words);
        }
        // The root has no positionals, so its first stray non-flag word is
        // what clap would take for an (external) subcommand.
        let Some(position) = scan
            .unknown
            .iter()
            .copied()
            .find(|&i| !words[i].value.starts_with('-'))
        else {
            return Ok(words);
        };
        let name = words[position].value.clone();
        let Some(definition) = config.aliases.get(&name) else {
            return Ok(words);
        };
        while let Some((_, range)) = recursion_stack.last() {
            if range.contains(&position) {
                if recursion_stack.iter().any(|(alias, _)| *alias == name) {
                    return Err(ParseError {
                        message: format!("Recursive alias definition involving `{name}`"),
                        span: line.word_span(&words[position]),
                        hint: None,
                    });
                }
                break;
            }
            let range = range.clone();
            recursion_stack.pop();
            if let Some((_, next)) = recursion_stack.last_mut() {
                next.end += range.end - range.start - 1;
            }
        }
        recursion_stack.push((name, position..position + definition.len()));
        let origin = match words[position].origin {
            Origin::Typed(i) | Origin::Alias(i) => Origin::Alias(i),
            Origin::Default => Origin::Default,
        };
        words.splice(
            position..=position,
            definition.iter().map(|value| Word {
                value: value.clone(),
                origin,
            }),
        );
    }
}

/// jj's default command: with no subcommand (and no `--help`/`--version`),
/// `ui.default-command` goes right after `jj`.
fn insert_default_command(config: &CliConfig, words: &mut Vec<Word>) {
    const PRIORITY_FLAGS: &[&str] = &["--help", "-h", "--version", "-V"];
    if words
        .iter()
        .any(|word| PRIORITY_FLAGS.contains(&word.value.as_str()))
    {
        return;
    }
    let scan = grammar::scan(words.iter().map(|w| w.value.as_str()));
    let has_subcommand = !scan.command_words.is_empty()
        || scan
            .unknown
            .iter()
            .any(|&i| !words[i].value.starts_with('-'));
    if !has_subcommand {
        words.splice(
            0..0,
            config.default_command.iter().map(|value| Word {
                value: value.clone(),
                origin: Origin::Default,
            }),
        );
    }
}

/// Parse a typed line. A leading `jj` is optional.
pub fn parse(config: &CliConfig, input: &str) -> Result<ParsedCommand, ParseError> {
    let line = Line::new(input);
    if let Err(open) = tokenize::check_terminated(input, &line.tokens) {
        return Err(ParseError {
            message: format!("unterminated {} quote", open.quote),
            span: open.span,
            hint: None,
        });
    }
    let typed = line.typed_words();
    let passthrough_args = typed.iter().map(|w| w.value.clone()).collect();
    let mut words = expand_aliases(config, typed, &line)?;
    insert_default_command(config, &mut words);
    let words = expand_aliases(config, words, &line)?;

    let scan = grammar::scan(words.iter().map(|w| w.value.as_str()));
    let args: Vec<String> = words.iter().map(|w| w.value.clone()).collect();
    let matched = grammar::validate(&args);
    let help = match matched {
        Ok(_) => false,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            true
        }
        Err(error) => return Err(clap_error(&error, &scan, &words, &line)),
    };

    let view = ArgsView {
        scan: &scan,
        words: &words,
        line: &line,
    };
    let command = if help {
        CliCommand::Passthrough
    } else {
        classify(&view)
    };
    Ok(ParsedCommand {
        command,
        path: scan.path().iter().map(|s| (*s).to_owned()).collect(),
        args,
        passthrough_args,
        revsets: view.revset_slots(),
        ignore_immutable: view.flag("ignore_immutable"),
    })
}

/// Turn clap's error into one line and a span.
fn clap_error(error: &clap::Error, scan: &Scan, words: &[Word], line: &Line) -> ParseError {
    let rendered = error.render().to_string();
    let mut message = rendered
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches("error: ")
        .to_owned();
    let hint = rendered
        .lines()
        .find_map(|l| l.trim().strip_prefix("tip: "))
        .map(str::to_owned);
    let end = line.len..line.len;
    let unknown_span = || scan.unknown.first().map(|&i| line.word_span(&words[i]));
    let span = match error.kind() {
        ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand => unknown_span(),
        ErrorKind::MissingRequiredArgument | ErrorKind::MissingSubcommand => Some(end.clone()),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let path = scan.path().join(" ");
            message = format!("`jj {path}` needs a subcommand");
            Some(end.clone())
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            context_str(error, ContextKind::InvalidValue)
                .filter(|value| !value.is_empty())
                .and_then(|value| words.iter().rposition(|w| w.value.ends_with(value)))
                .map(|i| line.word_span(&words[i]))
                .or_else(|| arg_span(error, scan, words, line))
        }
        _ => arg_span(error, scan, words, line).or_else(unknown_span),
    };
    ParseError {
        message,
        span: span.unwrap_or_else(|| line.rest_span()),
        hint,
    }
}

fn context_str(error: &clap::Error, kind: ContextKind) -> Option<&str> {
    match error.get(kind) {
        Some(ContextValue::String(value)) => Some(value),
        _ => None,
    }
}

/// The span of the argument clap's error names (`--onto <REVSETS>`,
/// `<NAMES>...`), at its last occurrence.
fn arg_span(error: &clap::Error, scan: &Scan, words: &[Word], line: &Line) -> Option<Range<usize>> {
    // A conflict names both sides; underline the one typed last.
    let mut named: Vec<&str> = context_str(error, ContextKind::InvalidArg)
        .into_iter()
        .collect();
    match error.get(ContextKind::PriorArg) {
        Some(ContextValue::String(prior)) => named.push(prior),
        Some(ContextValue::Strings(prior)) => named.extend(prior.iter().map(String::as_str)),
        _ => {}
    }
    named
        .into_iter()
        .filter_map(|display| named_arg_span(display, scan, words, line))
        .max_by_key(|span| span.start)
}

fn named_arg_span(display: &str, scan: &Scan, words: &[Word], line: &Line) -> Option<Range<usize>> {
    let head = display.split_whitespace().next()?;
    let command = scan.command();
    let arg = command.get_arguments().find(|arg| {
        if let Some(long) = head.strip_prefix("--") {
            arg.get_long() == Some(long)
        } else if let Some(short) = head.strip_prefix('-') {
            short.chars().next() == arg.get_short()
        } else {
            let name = head.trim_start_matches('<').split('>').next().unwrap_or("");
            arg.is_positional()
                && arg
                    .get_value_names()
                    .is_some_and(|names| names.iter().any(|n| n.as_str() == name))
        }
    })?;
    // A flag at the very end, still waiting for its value.
    if scan
        .expecting
        .is_some_and(|expecting| expecting.get_id() == arg.get_id())
    {
        return words.last().map(|word| line.word_span(word));
    }
    let occurrence = scan
        .occurrences
        .iter()
        .rev()
        .find(|o| o.id == arg.get_id().as_str())?;
    let word = occurrence
        .flag_word
        .or(occurrence.value.as_ref().map(|v| v.word))?;
    Some(line.word_span(&words[word]))
}

/// Read access to the scanned arguments of the leaf command.
pub(crate) struct ArgsView<'a> {
    scan: &'a Scan<'static>,
    words: &'a [Word],
    line: &'a Line,
}

/// Flags that don't change what a native command does.
const NEUTRAL_GLOBALS: &[&str] = &["ignore_immutable", "quiet", "no_pager", "color", "debug"];

impl ArgsView<'_> {
    fn path(&self) -> Vec<&str> {
        self.scan.path()
    }

    /// Every argument given is in `allowed` (or a neutral global flag).
    fn only(&self, allowed: &[&str]) -> bool {
        self.scan
            .used_ids()
            .iter()
            .all(|id| allowed.contains(id) || NEUTRAL_GLOBALS.contains(id))
    }

    fn flag(&self, id: &str) -> bool {
        self.scan.occurrences.iter().any(|o| o.id == id)
    }

    fn count(&self, id: &str) -> usize {
        self.scan.occurrences.iter().filter(|o| o.id == id).count()
    }

    fn values(&self, id: &str) -> Vec<Spanned<String>> {
        self.scan
            .occurrences
            .iter()
            .filter(|o| o.id == id)
            .filter_map(|o| o.value.as_ref())
            .map(|v| {
                let word = &self.words[v.word];
                Spanned {
                    value: word.value[v.range.clone()].to_owned(),
                    span: self.line.range_span(word, v.range.clone()),
                }
            })
            .collect()
    }

    fn value(&self, id: &str) -> Option<Spanned<String>> {
        self.values(id).pop()
    }

    fn revsets(&self, id: &str) -> Vec<RevsetArg> {
        self.values(id)
            .into_iter()
            .map(|v| RevsetArg {
                revset: v.value,
                span: Some(v.span),
            })
            .collect()
    }

    /// The revsets of `ids` (positional and hidden `-r` spellings of one
    /// argument), or the implicit `@`.
    fn revsets_or_at(&self, ids: &[&str]) -> Vec<RevsetArg> {
        let revsets: Vec<RevsetArg> = ids.iter().flat_map(|id| self.revsets(id)).collect();
        if revsets.is_empty() {
            vec![RevsetArg::implicit("@")]
        } else {
            revsets
        }
    }

    /// A single-revision argument: `None` if given more than once.
    fn single_or_at(&self, ids: &[&str]) -> Option<RevsetArg> {
        let mut revsets = self.revsets_or_at(ids);
        (revsets.len() == 1).then(|| revsets.remove(0))
    }

    fn message(&self) -> Option<String> {
        let paragraphs = self.values("message_paragraphs");
        (!paragraphs.is_empty()).then(|| {
            paragraphs
                .into_iter()
                .map(|p| p.value)
                .collect::<Vec<_>>()
                .join("\n\n")
        })
    }

    /// `-o`/`-A`/`-B`, when each is given at most once.
    fn destination(&self) -> Option<Option<RevsetDestination>> {
        let onto = self.revsets("onto");
        let after = self.revsets("insert_after");
        let before = self.revsets("insert_before");
        if onto.len() > 1 || after.len() > 1 || before.len() > 1 {
            return None;
        }
        let first = |mut v: Vec<RevsetArg>| v.pop();
        Some(match (first(onto), first(after), first(before)) {
            (None, None, None) => None,
            (Some(onto), None, None) => Some(RevsetDestination::Onto(onto)),
            (None, Some(after), None) => Some(RevsetDestination::After(after)),
            (None, None, Some(before)) => Some(RevsetDestination::Before(before)),
            (None, Some(after), Some(before)) => Some(RevsetDestination::Between { after, before }),
            // clap rejects `-o` with `-A`/`-B`.
            _ => return None,
        })
    }

    fn revset_slots(&self) -> Vec<RevsetSlot> {
        let command = self.scan.command();
        let mut slots = Vec::new();
        for occurrence in &self.scan.occurrences {
            let Some(value) = &occurrence.value else {
                continue;
            };
            let Some(arg) = self.scan.commands.iter().rev().find_map(|c| {
                c.get_arguments()
                    .find(|a| a.get_id() == occurrence.id.as_str())
            }) else {
                continue;
            };
            if grammar::value_kind(command, arg) != ValueKind::Revset {
                continue;
            }
            let word = &self.words[value.word];
            let flag = match (arg.get_long(), arg.get_short()) {
                (Some(long), _) => format!("--{long}"),
                (None, Some(short)) => format!("-{short}"),
                (None, None) => arg
                    .get_value_names()
                    .and_then(|names| names.first())
                    .map_or_else(String::new, |name| name.to_string()),
            };
            slots.push(RevsetSlot {
                flag,
                revset: word.value[value.range.clone()].to_owned(),
                span: self.line.range_span(word, value.range.clone()),
            });
        }
        slots
    }
}

/// A plain bookmark name: no string-pattern prefix and no glob characters,
/// so it names exactly one bookmark. `exact:NAME` is unwrapped.
fn plain_name(name: Spanned<String>) -> Option<Spanned<String>> {
    if let Some(exact) = name.value.strip_prefix("exact:") {
        let value = exact.trim_matches('"').to_owned();
        return Some(Spanned {
            value,
            span: name.span,
        });
    }
    let pattern = name.value.contains(['*', '?', '[', ']'])
        || [
            "glob:",
            "regex:",
            "substring:",
            "glob-i:",
            "exact-i:",
            "regex-i:",
            "substring-i:",
        ]
        .iter()
        .any(|prefix| name.value.starts_with(prefix));
    (!pattern).then_some(name)
}

fn single_name(view: &ArgsView, id: &str) -> Option<Spanned<String>> {
    let mut names = view.values(id);
    if names.len() != 1 {
        return None;
    }
    plain_name(names.remove(0))
}

/// Which kind the line is, and its typed arguments when diffui runs it.
fn classify(view: &ArgsView) -> CliCommand {
    let native = |command: Option<MutationCommand>| match command {
        Some(command) => CliCommand::Mutation(command),
        None => CliCommand::Passthrough,
    };
    let path = view.path();
    match path.as_slice() {
        ["diff"] if view.only(&["revisions"]) => CliCommand::View(ViewCommand::Diff {
            revisions: view.revsets_or_at(&["revisions"]),
        }),
        ["diff"] if view.only(&["from", "to"]) => CliCommand::View(ViewCommand::DiffRange {
            from: view
                .single_or_at(&["from"])
                .expect("--from is a single value"),
            to: view.single_or_at(&["to"]).expect("--to is a single value"),
        }),
        ["interdiff"] if view.only(&["from", "to"]) => CliCommand::View(ViewCommand::Interdiff {
            from: view
                .single_or_at(&["from"])
                .expect("--from is a single value"),
            to: view.single_or_at(&["to"]).expect("--to is a single value"),
        }),
        ["show"] if view.only(&["revisions_pos", "revisions_opt"]) => {
            match view.single_or_at(&["revisions_pos", "revisions_opt"]) {
                Some(revision) => CliCommand::View(ViewCommand::Show { revision }),
                None => CliCommand::Passthrough,
            }
        }
        ["log"] if view.only(&["revisions"]) => {
            let revisions = view.revsets("revisions");
            CliCommand::View(ViewCommand::Log {
                revisions: (!revisions.is_empty()).then_some(revisions),
            })
        }
        ["evolog"] if view.only(&["revisions"]) => match view.single_or_at(&["revisions"]) {
            Some(revision) => CliCommand::View(ViewCommand::Evolog { revision }),
            None => CliCommand::Passthrough,
        },
        ["operation", "log"] if view.only(&["limit"]) => {
            let limit = view.value("limit").and_then(|v| v.value.parse().ok());
            CliCommand::View(ViewCommand::OpLog { limit })
        }
        ["file", "annotate"] if view.only(&["path", "revision"]) => match view.value("path") {
            Some(path) => CliCommand::View(ViewCommand::FileAnnotate {
                path,
                revision: view.single_or_at(&["revision"]).expect("-r is single"),
            }),
            None => CliCommand::Passthrough,
        },

        ["new"]
            if view.only(&[
                "revisions_pos",
                "revisions_opt",
                "insert_after",
                "insert_before",
            ]) =>
        {
            let parents = view.revsets_or_at(&["revisions_pos", "revisions_opt"]);
            let placed = view.flag("insert_after") || view.flag("insert_before");
            native(match view.destination() {
                // `jj new X -A Y` mixes parents with a placement.
                Some(Some(destination)) if placed && parents[0].is_implicit() => {
                    Some(MutationCommand::NewAt { destination })
                }
                Some(None) => Some(MutationCommand::New { parents }),
                _ => None,
            })
        }
        ["edit"] if view.only(&["revision_pos", "revision_opt"]) => native(
            view.single_or_at(&["revision_pos", "revision_opt"])
                .map(|target| MutationCommand::Edit { target }),
        ),
        ["abandon"] if view.only(&["revisions_pos", "revisions_opt"]) => {
            native(Some(MutationCommand::Abandon {
                targets: view.revsets_or_at(&["revisions_pos", "revisions_opt"]),
            }))
        }
        ["describe"]
            if view.only(&[
                "revisions_pos",
                "revisions_opt",
                "message_paragraphs",
                "editor",
            ]) =>
        {
            let Some(target) = view.single_or_at(&["revisions_pos", "revisions_opt"]) else {
                return CliCommand::Passthrough;
            };
            match view.message() {
                Some(message) if !view.flag("editor") => {
                    CliCommand::Mutation(MutationCommand::Describe { target, message })
                }
                Some(_) => CliCommand::Passthrough,
                None => CliCommand::OpensUi(UiCommand::Describe { target }),
            }
        }
        ["commit"] if view.only(&["message_paragraphs", "editor"]) => match view.message() {
            Some(message) if !view.flag("editor") => {
                CliCommand::Mutation(MutationCommand::Commit { message })
            }
            Some(_) => CliCommand::Passthrough,
            None => CliCommand::OpensUi(UiCommand::Commit),
        },
        ["rebase"]
            if view.only(&[
                "branch",
                "source",
                "revisions",
                "onto",
                "insert_after",
                "insert_before",
            ]) =>
        {
            let (mode, sources) = if view.flag("revisions") {
                (RebaseSourceMode::Revisions, view.revsets("revisions"))
            } else if view.flag("source") {
                (RebaseSourceMode::WithDescendants, view.revsets("source"))
            } else {
                (RebaseSourceMode::Branch, view.revsets_or_at(&["branch"]))
            };
            native(match view.destination() {
                Some(Some(destination)) => Some(MutationCommand::Rebase {
                    mode,
                    sources,
                    destination,
                }),
                _ => None,
            })
        }
        ["squash"] if view.only(&["revision", "from", "into"]) => {
            native(Some(if view.flag("from") || view.flag("into") {
                MutationCommand::Squash {
                    from: view.revsets_or_at(&["from"]),
                    into: view.single_or_at(&["into"]),
                }
            } else {
                MutationCommand::Squash {
                    from: view.revsets_or_at(&["revision"]),
                    into: None,
                }
            }))
        }
        ["duplicate"]
            if view.only(&[
                "revisions_pos",
                "revisions_opt",
                "onto",
                "insert_after",
                "insert_before",
            ]) =>
        {
            native(
                view.destination()
                    .map(|destination| MutationCommand::Duplicate {
                        targets: view.revsets_or_at(&["revisions_pos", "revisions_opt"]),
                        destination,
                    }),
            )
        }
        ["parallelize"] if view.only(&["revisions_pos", "revisions_opt"]) => {
            let targets: Vec<RevsetArg> = ["revisions_pos", "revisions_opt"]
                .iter()
                .flat_map(|id| view.revsets(id))
                .collect();
            native((!targets.is_empty()).then_some(MutationCommand::Parallelize { targets }))
        }
        ["absorb"] if view.only(&["from"]) => native(
            view.single_or_at(&["from"])
                .map(|from| MutationCommand::Absorb { from }),
        ),
        ["metaedit"]
            if view.only(&[
                "revisions_pos",
                "revisions_opt",
                "update_author",
                "update_author_timestamp",
            ]) && (view.flag("update_author") || view.flag("update_author_timestamp")) =>
        {
            native(Some(MutationCommand::Metaedit {
                targets: view.revsets_or_at(&["revisions_pos", "revisions_opt"]),
                update_author: view.flag("update_author"),
                update_author_timestamp: view.flag("update_author_timestamp"),
            }))
        }
        ["bookmark", "create"] if view.only(&["names", "revision"]) => native(
            single_name(view, "names").map(|name| MutationCommand::BookmarkCreate {
                name,
                to: view.single_or_at(&["revision"]).expect("-r is single"),
            }),
        ),
        ["bookmark", "set"] if view.only(&["names", "revision", "allow_backwards"]) => native(
            single_name(view, "names").map(|name| MutationCommand::BookmarkSet {
                name,
                to: view.single_or_at(&["revision"]).expect("-r is single"),
                allow_backwards: view.flag("allow_backwards"),
            }),
        ),
        ["bookmark", "move"] if view.only(&["names", "to", "allow_backwards"]) => native(
            single_name(view, "names").map(|name| MutationCommand::BookmarkSet {
                name,
                to: view.single_or_at(&["to"]).expect("--to is single"),
                allow_backwards: view.flag("allow_backwards"),
            }),
        ),
        ["bookmark", "delete"] if view.only(&["names"]) => {
            native(single_name(view, "names").map(|name| MutationCommand::BookmarkDelete { name }))
        }
        ["bookmark", "forget"] if view.only(&["names"]) => {
            native(single_name(view, "names").map(|name| MutationCommand::BookmarkForget { name }))
        }
        ["bookmark", "rename"] if view.only(&["old", "new"]) => {
            match (view.value("old"), view.value("new")) {
                (Some(old), Some(new)) => {
                    CliCommand::Mutation(MutationCommand::BookmarkRename { old, new })
                }
                _ => CliCommand::Passthrough,
            }
        }
        ["bookmark", verb @ ("track" | "untrack")] if view.only(&["names", "remotes"]) => {
            let names = view.values("names");
            let remotes = view.values("remotes");
            let target = match (names.as_slice(), remotes.as_slice()) {
                ([symbol], []) => symbol
                    .value
                    .rsplit_once('@')
                    .map(|(name, remote)| (name.to_owned(), remote.to_owned())),
                ([name], [remote]) if !name.value.contains('@') => {
                    Some((name.value.clone(), remote.value.clone()))
                }
                _ => None,
            }
            .filter(|(name, remote)| {
                !name.is_empty()
                    && !remote.is_empty()
                    && plain_name(Spanned {
                        value: name.clone(),
                        span: 0..0,
                    })
                    .is_some()
                    && !remote.contains(['*', '?', '[', ':'])
            });
            native(target.map(|(name, remote)| match *verb {
                "track" => MutationCommand::BookmarkTrack { name, remote },
                _ => MutationCommand::BookmarkUntrack { name, remote },
            }))
        }
        ["git", "push"]
            if view.only(&[
                "remote",
                "bookmark",
                "change",
                "allow_empty_description",
                "allow_private",
                "allow_conflicts",
            ]) =>
        {
            let bookmarks: Option<Vec<Spanned<String>>> = view
                .values("bookmark")
                .into_iter()
                .map(plain_name)
                .collect();
            native(bookmarks.map(|bookmarks| MutationCommand::Push {
                remote: view.value("remote").map(|r| r.value),
                bookmarks,
                changes: view.revsets("change"),
                allow: PushAllow {
                    empty_description: view.flag("allow_empty_description"),
                    private: view.flag("allow_private"),
                    conflicts: view.flag("allow_conflicts"),
                },
            }))
        }
        ["git", "fetch"] if view.only(&["all_remotes"]) && view.flag("all_remotes") => {
            CliCommand::Mutation(MutationCommand::FetchAll)
        }
        ["git", "fetch"]
            if view.only(&["remotes", "branches"])
                && view.count("remotes") == 1
                && view.count("branches") == 1 =>
        {
            let remote = view.value("remotes").expect("counted").value;
            let branch = view.value("branches").expect("counted");
            native(
                plain_name(branch).map(|branch| MutationCommand::FetchBranch {
                    remote,
                    branch: branch.value,
                }),
            )
        }
        ["undo"] if view.only(&[]) => CliCommand::Mutation(MutationCommand::Undo),
        ["redo"] if view.only(&[]) => CliCommand::Mutation(MutationCommand::Redo),
        ["operation", "restore"] if view.only(&["operation"]) => native(
            view.value("operation")
                .map(|operation| MutationCommand::OpRestore { operation }),
        ),
        ["operation", "revert"] if view.only(&["operation"]) => {
            CliCommand::Mutation(MutationCommand::OpRevert {
                operation: view.value("operation"),
            })
        }
        ["split"] if view.only(&["revision"]) => CliCommand::OpensUi(UiCommand::Split {
            target: view.single_or_at(&["revision"]).expect("-r is single"),
        }),
        ["resolve"] if view.only(&["revision", "paths"]) => {
            CliCommand::OpensUi(UiCommand::Resolve {
                target: view.single_or_at(&["revision"]).expect("-r is single"),
                paths: view.values("paths"),
            })
        }
        _ => CliCommand::Passthrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> ParsedCommand {
        parse(&CliConfig::builtin(), input).unwrap_or_else(|e| panic!("{input}: {e:?}"))
    }

    fn command(input: &str) -> CliCommand {
        parse_ok(input).command
    }

    fn mutation(input: &str) -> MutationCommand {
        match command(input) {
            CliCommand::Mutation(m) => m,
            other => panic!("{input}: expected a mutation, got {other:?}"),
        }
    }

    fn error(input: &str) -> ParseError {
        parse(&CliConfig::builtin(), input).expect_err(input)
    }

    /// `text` as a typed argument: its first occurrence that starts a word
    /// or follows `=` or a quote.
    fn typed(input: &str, text: &str) -> RevsetArg {
        let start = input
            .match_indices(text)
            .map(|(i, _)| i)
            .find(|&i| i > 0 && matches!(input.as_bytes()[i - 1], b' ' | b'=' | b'\''))
            .expect("text in input");
        RevsetArg {
            revset: text.to_owned(),
            span: Some(start..start + text.len()),
        }
    }

    #[test]
    fn rebase_spells_like_jj() {
        let input = "jj rebase -s xyz -o 'trunk()'";
        let parsed = parse_ok(input);
        assert_eq!(parsed.path, ["rebase"]);
        assert_eq!(
            parsed.command,
            CliCommand::Mutation(MutationCommand::Rebase {
                mode: RebaseSourceMode::WithDescendants,
                sources: vec![typed(input, "xyz")],
                destination: RevsetDestination::Onto(typed(input, "trunk()")),
            })
        );
        assert_eq!(
            parsed
                .revsets
                .iter()
                .map(|s| (s.flag.as_str(), s.revset.as_str(), &input[s.span.clone()]))
                .collect::<Vec<_>>(),
            [("--source", "xyz", "xyz"), ("--onto", "trunk()", "trunk()")]
        );
        assert_eq!(
            parsed.passthrough_args,
            ["rebase", "-s", "xyz", "-o", "trunk()"]
        );

        // No source flag: jj's `-b @`.
        assert_eq!(
            mutation("jj rebase -o main"),
            MutationCommand::Rebase {
                mode: RebaseSourceMode::Branch,
                sources: vec![RevsetArg::implicit("@")],
                destination: RevsetDestination::Onto(typed("jj rebase -o main", "main")),
            }
        );
        let input = "jj rebase -r a --insert-after=b -B c";
        assert_eq!(
            mutation(input),
            MutationCommand::Rebase {
                mode: RebaseSourceMode::Revisions,
                sources: vec![typed(input, "a")],
                destination: RevsetDestination::Between {
                    after: typed(input, "b"),
                    before: typed(input, "c"),
                },
            }
        );
        // `--destination` is jj's hidden alias of `-o`.
        assert!(matches!(
            mutation("jj rebase -b x --destination y"),
            MutationCommand::Rebase {
                mode: RebaseSourceMode::Branch,
                destination: RevsetDestination::Onto(_),
                ..
            }
        ));
        // What diffui's op can't say goes to the CLI.
        assert_eq!(command("jj rebase -s a -o b -o c"), CliCommand::Passthrough);
        assert_eq!(
            command("jj rebase -s a -o b --skip-emptied"),
            CliCommand::Passthrough
        );
    }

    #[test]
    fn new_and_its_placements() {
        assert_eq!(
            mutation("jj new"),
            MutationCommand::New {
                parents: vec![RevsetArg::implicit("@")]
            }
        );
        let input = "jj new a b";
        assert_eq!(
            mutation(input),
            MutationCommand::New {
                parents: vec![typed(input, "a"), typed(input, "b")]
            }
        );
        let input = "jj new -A x";
        assert_eq!(
            mutation(input),
            MutationCommand::NewAt {
                destination: RevsetDestination::After(typed(input, "x"))
            }
        );
        assert_eq!(command("jj new -m wip"), CliCommand::Passthrough);
        // jj itself refuses parents together with a placement.
        error("jj new a -A b");
    }

    #[test]
    fn squash_defaults_follow_jj() {
        assert_eq!(
            mutation("jj squash"),
            MutationCommand::Squash {
                from: vec![RevsetArg::implicit("@")],
                into: None
            }
        );
        let input = "jj squash --into x";
        assert_eq!(
            mutation(input),
            MutationCommand::Squash {
                from: vec![RevsetArg::implicit("@")],
                into: Some(typed(input, "x"))
            }
        );
        let input = "jj squash -r x";
        assert_eq!(
            mutation(input),
            MutationCommand::Squash {
                from: vec![typed(input, "x")],
                into: None
            }
        );
        assert_eq!(command("jj squash -i"), CliCommand::Passthrough);
        assert_eq!(command("jj squash src/"), CliCommand::Passthrough);
    }

    #[test]
    fn describe_with_and_without_a_message() {
        assert_eq!(
            mutation("jj desc -m one -m two"),
            MutationCommand::Describe {
                target: RevsetArg::implicit("@"),
                message: "one\n\ntwo".to_owned(),
            }
        );
        let input = "jj describe x";
        assert_eq!(
            command(input),
            CliCommand::OpensUi(UiCommand::Describe {
                target: typed(input, "x")
            })
        );
        assert_eq!(command("jj commit"), CliCommand::OpensUi(UiCommand::Commit));
        assert_eq!(
            mutation("jj ci -m 'feat: x'"),
            MutationCommand::Commit {
                message: "feat: x".to_owned()
            }
        );
        assert!(matches!(
            command("jj split"),
            CliCommand::OpensUi(UiCommand::Split { .. })
        ));
        assert_eq!(command("jj split -m first a.rs"), CliCommand::Passthrough);
    }

    #[test]
    fn bookmarks_need_plain_names() {
        assert!(matches!(
            mutation("jj b set main -r @-"),
            MutationCommand::BookmarkSet {
                allow_backwards: false,
                ..
            }
        ));
        assert!(matches!(
            mutation("jj bookmark move main --to x -B"),
            MutationCommand::BookmarkSet {
                allow_backwards: true,
                ..
            }
        ));
        assert_eq!(
            command("jj bookmark delete 'glob:feat-*'"),
            CliCommand::Passthrough
        );
        assert_eq!(command("jj bookmark delete a b"), CliCommand::Passthrough);
        assert!(matches!(
            mutation("jj bookmark delete exact:main"),
            MutationCommand::BookmarkDelete { name } if name.value == "main"
        ));
        assert_eq!(
            mutation("jj bookmark track main@origin"),
            MutationCommand::BookmarkTrack {
                name: "main".to_owned(),
                remote: "origin".to_owned()
            }
        );
        assert_eq!(
            mutation("jj bookmark untrack main --remote upstream"),
            MutationCommand::BookmarkUntrack {
                name: "main".to_owned(),
                remote: "upstream".to_owned()
            }
        );
        assert_eq!(command("jj bookmark track main"), CliCommand::Passthrough);
    }

    #[test]
    fn push_and_fetch() {
        let input = "jj git push -b main -c @- --allow-empty-description";
        match mutation(input) {
            MutationCommand::Push {
                remote: None,
                bookmarks,
                changes,
                allow,
            } => {
                assert_eq!(bookmarks[0].value, "main");
                assert_eq!(changes, [typed(input, "@-")]);
                assert!(allow.empty_description && !allow.private);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(command("jj git push --dry-run"), CliCommand::Passthrough);
        assert_eq!(command("jj git push --all"), CliCommand::Passthrough);
        assert_eq!(
            mutation("jj git fetch --all-remotes"),
            MutationCommand::FetchAll
        );
        assert_eq!(
            mutation("jj git fetch --remote origin -b main"),
            MutationCommand::FetchBranch {
                remote: "origin".to_owned(),
                branch: "main".to_owned()
            }
        );
        // Plain fetch follows `git.fetch`, which the CLI reads exactly.
        assert_eq!(command("jj git fetch"), CliCommand::Passthrough);
    }

    #[test]
    fn views() {
        let input = "jj diff -r 'trunk()..@'";
        assert_eq!(
            command(input),
            CliCommand::View(ViewCommand::Diff {
                revisions: vec![typed(input, "trunk()..@")]
            })
        );
        let input = "jj diff --from a --to b";
        assert_eq!(
            command(input),
            CliCommand::View(ViewCommand::DiffRange {
                from: typed(input, "a"),
                to: typed(input, "b"),
            })
        );
        assert_eq!(command("jj diff --stat"), CliCommand::Passthrough);
        assert_eq!(
            command("jj log"),
            CliCommand::View(ViewCommand::Log { revisions: None })
        );
        assert_eq!(
            command("jj op log -n 5"),
            CliCommand::View(ViewCommand::OpLog { limit: Some(5) })
        );
        assert!(matches!(
            command("jj show x"),
            CliCommand::View(ViewCommand::Show { .. })
        ));
        assert!(matches!(
            command("jj file annotate src/main.rs -r @-"),
            CliCommand::View(ViewCommand::FileAnnotate { path, .. }) if path.value == "src/main.rs"
        ));
        assert_eq!(
            command("jj log -T builtin_log_oneline"),
            CliCommand::Passthrough
        );
    }

    #[test]
    fn default_command_and_passthrough() {
        // A bare `jj` is `ui.default-command`, `log` out of the box.
        assert_eq!(parse_ok("jj").path, ["log"]);
        assert_eq!(parse_ok("jj --quiet").path, ["log"]);
        let parsed = parse_ok("jj st");
        assert_eq!(parsed.path, ["status"]);
        assert_eq!(parsed.command, CliCommand::Passthrough);
        assert_eq!(parsed.passthrough_args, ["st"]);
        assert_eq!(command("jj fix -s @"), CliCommand::Passthrough);
        assert_eq!(command("jj rebase --help"), CliCommand::Passthrough);
        assert_eq!(command("jj -R ../other new"), CliCommand::Passthrough);
        // The leading `jj` is optional.
        assert_eq!(parse_ok("new").path, ["new"]);
    }

    #[test]
    fn ignore_immutable_rides_along() {
        let parsed = parse_ok("jj --ignore-immutable abandon x");
        assert!(parsed.ignore_immutable);
        assert!(matches!(
            parsed.command,
            CliCommand::Mutation(MutationCommand::Abandon { .. })
        ));
    }

    #[test]
    fn user_aliases_expand_and_point_at_the_alias() {
        let mut config = CliConfig::builtin();
        config.aliases.insert(
            "mylog".into(),
            vec!["log".into(), "-r".into(), "mine()".into()],
        );
        config.aliases.insert("loop".into(), vec!["loop2".into()]);
        config.aliases.insert("loop2".into(), vec!["loop".into()]);
        let input = "jj mylog";
        let parsed = parse(&config, input).unwrap();
        assert_eq!(parsed.args, ["log", "-r", "mine()"]);
        assert_eq!(
            parsed.command,
            CliCommand::View(ViewCommand::Log {
                revisions: Some(vec![RevsetArg {
                    revset: "mine()".to_owned(),
                    span: Some(3..8),
                }])
            })
        );
        let error = parse(&config, "jj loop").unwrap_err();
        assert!(error.message.contains("Recursive alias"), "{error:?}");
        assert_eq!(error.span, 3..7);
    }

    #[test]
    fn errors_point_at_the_problem() {
        let input = "jj frobnicate -r x";
        let e = error(input);
        assert_eq!(&input[e.span.clone()], "frobnicate");
        assert!(e.message.contains("frobnicate"), "{e:?}");

        let input = "jj rebase --frob -o x";
        let e = error(input);
        assert_eq!(&input[e.span.clone()], "--frob");

        let input = "jj rebase -s a";
        let e = error(input);
        assert_eq!(e.span, input.len()..input.len());

        let input = "jj rebase -r a -s b -o c";
        let e = error(input);
        assert_eq!(&input[e.span.clone()], "-s", "{e:?}");

        let input = "jj log -r 'mine() &";
        let e = error(input);
        assert_eq!(&input[e.span.clone()], "'mine() &");

        let e = error("jj bookmark");
        assert_eq!(e.message, "`jj bookmark` needs a subcommand");

        let input = "jj new -r";
        let e = error(input);
        assert_eq!(&input[e.span.clone()], "-r", "{e:?}");
    }

    #[test]
    fn builtin_config_has_jjs_default_aliases() {
        let config = CliConfig::builtin();
        assert_eq!(config.aliases["b"], ["bookmark"]);
        assert_eq!(config.aliases["ci"], ["commit"]);
        assert_eq!(config.default_command, ["log"]);
    }
}
