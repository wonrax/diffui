//! Reading `[[commands]]` rows into [`UserCommand`]s. A row that's wrong is
//! left out with a `path:line:col` diagnostic naming the field; the rest of
//! the file, and every other row, still load.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;
use toml::de::{DeTable, DeValue, ValueDeserializer};

use super::template::Template;
use super::{Output, Places, Prompt, PromptKind, Step, UserCommand};
use crate::when::When;

/// The rows diffui ships, read like a user's and replaced the same way.
pub(crate) const SHIPPED: &str = include_str!("../../assets/commands.toml");

/// How long a declared command may run when its row doesn't say.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: Spanned<String>,
    label: String,
    hint: Option<String>,
    group: Option<Spanned<String>>,
    when: Option<Spanned<String>>,
    menu: Option<Vec<Spanned<String>>>,
    #[serde(default)]
    keys: Vec<Spanned<String>>,
    exec: Option<Spanned<Vec<Spanned<String>>>>,
    run: Option<Spanned<String>>,
    agent: Option<Spanned<String>>,
    edit: Option<Spanned<String>>,
    stdin: Option<Spanned<String>>,
    output: Option<Spanned<String>>,
    confirm: Option<Spanned<String>>,
    #[serde(default)]
    prompts: Vec<Spanned<RawPrompt>>,
    #[serde(default, rename = "override")]
    replaces: bool,
    /// Seconds.
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrompt {
    name: Spanned<String>,
    kind: Option<Spanned<String>>,
    label: Option<String>,
    choices: Option<Vec<String>>,
}

/// `text`'s `[[commands]]`, and what was wrong with the rows left out.
/// `file` names it in the diagnostics.
pub(crate) fn parse_commands(file: &Path, text: &str) -> (Vec<UserCommand>, Vec<String>) {
    let at = Locator { file, text };
    let table = match DeTable::parse(text) {
        Ok(table) => table,
        // `config.toml` as a whole reports this one.
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let Some((key, rows)) = table
        .get_ref()
        .iter()
        .find(|(key, _)| key.get_ref() == "commands")
    else {
        return (Vec::new(), Vec::new());
    };
    let DeValue::Array(rows) = rows.get_ref() else {
        return (
            Vec::new(),
            vec![at.say(
                key.span(),
                "commands",
                "is a list: write [[commands]] tables",
            )],
        );
    };
    let mut commands: Vec<UserCommand> = Vec::new();
    let mut problems = Vec::new();
    for row in rows.iter() {
        let span = row.span();
        let raw = match RawCommand::deserialize(ValueDeserializer::from(row.clone())) {
            Ok(raw) => raw,
            Err(error) => {
                let message = error.message().lines().next().unwrap_or("").trim();
                problems.push(at.say(error.span().unwrap_or(span), "[[commands]]", message));
                continue;
            }
        };
        match command(&at, raw) {
            Ok((command, warnings)) => {
                problems.extend(warnings);
                if let Some(first) = commands.iter().find(|seen| seen.id == command.id) {
                    problems.push(format!(
                        "{}: [[commands]] {:?}: also declared at {}; the first one stays",
                        command.source, command.id, first.source
                    ));
                } else {
                    commands.push(command);
                }
            }
            Err(problem) => problems.push(problem),
        }
    }
    (commands, problems)
}

/// Turns byte spans into `path:line:col`.
struct Locator<'a> {
    file: &'a Path,
    text: &'a str,
}

impl Locator<'_> {
    fn place(&self, offset: usize) -> String {
        let before = &self.text[..offset.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("{}:{line}:{column}", crate::config::display_path(self.file))
    }

    fn say(&self, span: std::ops::Range<usize>, what: &str, message: &str) -> String {
        format!("{}: {what}: {message}", self.place(span.start))
    }

    /// Where byte `offset` of a string value lands, for a basic or literal
    /// string written on one line (the quote is one byte).
    fn inside(&self, value: &Spanned<String>, offset: usize) -> usize {
        value.span().start + 1 + offset
    }
}

fn command(at: &Locator<'_>, raw: RawCommand) -> Result<(UserCommand, Vec<String>), String> {
    let id = raw.id.get_ref().clone();
    let source = at.place(raw.id.span().start);
    let say = |span: std::ops::Range<usize>, message: &str| {
        format!("{}: [[commands]] {id:?}: {message}", at.place(span.start))
    };
    let mut warnings = Vec::new();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(say(
            raw.id.span(),
            "an id is letters, digits, `.`, `-` and `_`",
        ));
    }
    if raw.label.trim().is_empty() {
        return Err(say(raw.id.span(), "needs a label"));
    }

    // Prompts first: templates may name them.
    let mut prompts = Vec::new();
    for prompt in &raw.prompts {
        let RawPrompt {
            name,
            kind,
            label,
            choices,
        } = prompt.get_ref();
        let valid = !name.get_ref().is_empty()
            && name
                .get_ref()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if !valid {
            return Err(say(
                name.span(),
                "a prompt name is letters, digits, `-` and `_`",
            ));
        }
        if prompts
            .iter()
            .any(|seen: &Prompt| seen.name == *name.get_ref())
        {
            return Err(say(name.span(), "two prompts share this name"));
        }
        let kind = match (kind.as_ref().map(|kind| kind.get_ref().as_str()), choices) {
            (None | Some("input"), None) => PromptKind::Input,
            (Some("confirm"), None) => PromptKind::Confirm,
            (Some("choose"), Some(choices)) if !choices.is_empty() => {
                PromptKind::Choose(choices.clone())
            }
            (Some("choose"), _) => {
                return Err(say(prompt.span(), "a choose prompt needs choices"));
            }
            (None | Some("input" | "confirm"), Some(_)) => {
                return Err(say(prompt.span(), "only a choose prompt has choices"));
            }
            (Some(_), _) => {
                let span = kind.as_ref().map_or(prompt.span(), Spanned::span);
                return Err(say(span, "a prompt's kind is input, confirm or choose"));
            }
        };
        prompts.push(Prompt {
            name: name.get_ref().clone(),
            label: label.clone().unwrap_or_else(|| name.get_ref().clone()),
            kind,
        });
    }
    let names: Vec<&str> = prompts.iter().map(|prompt| prompt.name.as_str()).collect();
    let template = |value: &Spanned<String>| {
        Template::parse(value.get_ref(), &names).map_err(|error| {
            format!(
                "{}: [[commands]] {id:?}: {}",
                at.place(at.inside(value, error.offset)),
                error.message
            )
        })
    };

    let step = match (&raw.exec, &raw.run, &raw.agent, &raw.edit) {
        (Some(exec), None, None, None) => {
            if exec.get_ref().is_empty() {
                return Err(say(exec.span(), "exec needs a program"));
            }
            Step::Exec(
                exec.get_ref()
                    .iter()
                    .map(template)
                    .collect::<Result<_, _>>()?,
            )
        }
        (None, Some(run), None, None) => Step::Run(run_words(at, &id, run, &names)?),
        (None, None, Some(agent), None) => Step::Agent(template(agent)?),
        (None, None, None, Some(edit)) => Step::Edit(template(edit)?),
        _ => {
            return Err(say(
                raw.id.span(),
                "needs exactly one of exec, run, agent or edit",
            ));
        }
    };
    let pipes = matches!(step, Step::Exec(_) | Step::Agent(_));
    let stdin = match &raw.stdin {
        Some(stdin) if !pipes => return Err(say(stdin.span(), "stdin is for exec and agent")),
        Some(stdin) => Some(template(stdin)?),
        None => None,
    };
    let output = match &raw.output {
        Some(output) if !pipes => {
            return Err(say(output.span(), "output is for exec and agent"));
        }
        Some(output) => Output::parse(output.get_ref()).ok_or_else(|| {
            say(
                output.span(),
                "output is activity, toast, clipboard, description, open-url or none",
            )
        })?,
        None => Output::Activity,
    };
    let confirm = match &raw.confirm {
        Some(confirm) => {
            let parsed = template(confirm)?;
            if parsed.vars().any(|var| var.is_fetched()) {
                return Err(say(
                    confirm.span(),
                    "a confirm can't show {diff} or {recent_descriptions}",
                ));
            }
            Some(parsed)
        }
        None => None,
    };
    let when = match &raw.when {
        Some(text) => match When::parse(text.get_ref()) {
            Ok(when) => {
                for key in when.unknown_keys() {
                    warnings.push(format!(
                        "{}: [[commands]] {id:?}: no key `{key}`; it reads as false",
                        at.place(text.span().start)
                    ));
                }
                Some((text.get_ref().clone(), when))
            }
            Err(error) => {
                return Err(format!(
                    "{}: [[commands]] {id:?}: {}",
                    at.place(at.inside(text, error.offset)),
                    error.message
                ));
            }
        },
        None => None,
    };
    let mut places = Places::default();
    match &raw.menu {
        None => places.palette = true,
        Some(menus) => {
            for menu in menus {
                match menu.get_ref().as_str() {
                    "palette" => places.palette = true,
                    "revision" => places.revision = true,
                    "bookmark" => places.bookmark = true,
                    "file" => places.file = true,
                    _ => {
                        return Err(say(
                            menu.span(),
                            "a menu is palette, revision, bookmark or file",
                        ));
                    }
                }
            }
        }
    }
    let mut keys = Vec::new();
    for key in &raw.keys {
        if crate::keymap::Chord::parse(key.get_ref()).is_none() {
            warnings.push(say(
                key.span(),
                &format!("{:?} is not a chord", key.get_ref()),
            ));
        } else {
            keys.push(key.get_ref().clone());
        }
    }
    let group = match &raw.group {
        Some(group) => crate::commands::GROUPS
            .iter()
            .find(|known| known.eq_ignore_ascii_case(group.get_ref()))
            .copied()
            .ok_or_else(|| {
                say(
                    group.span(),
                    &format!("a group is one of {}", crate::commands::GROUPS.join(", ")),
                )
            })?,
        None => "Commands",
    };
    let command = UserCommand {
        label: raw.label.clone(),
        hint: raw.hint.clone().unwrap_or_default(),
        group,
        when,
        places,
        keys,
        step,
        stdin,
        output,
        confirm,
        prompts,
        replaces: raw.replaces,
        timeout: raw.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
        source,
        id,
    };
    Ok((command, warnings))
}

/// `run`'s line, split into words the way the command line splits it, each
/// word a template. The leading `jj` is dropped.
fn run_words(
    at: &Locator<'_>,
    id: &str,
    run: &Spanned<String>,
    prompts: &[&str],
) -> Result<Vec<Template>, String> {
    use diffui_core::cli::tokenize::{check_terminated, tokenize};
    let line = run.get_ref();
    let tokens = tokenize(line);
    if let Err(error) = check_terminated(line, &tokens) {
        return Err(format!(
            "{}: [[commands]] {id:?}: run: unclosed {}",
            at.place(at.inside(run, error.span.start)),
            error.quote
        ));
    }
    let mut words = tokens.iter();
    if words.next().map(|token| token.value.as_str()) != Some("jj") {
        return Err(format!(
            "{}: [[commands]] {id:?}: run is a jj command: \"jj …\"",
            at.place(run.span().start)
        ));
    }
    words
        .map(|token| {
            Template::parse(&token.value, prompts).map_err(|error| {
                let offset = token.input_range(error.offset..error.offset).start;
                format!(
                    "{}: [[commands]] {id:?}: {}",
                    at.place(at.inside(run, offset)),
                    error.message
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Vec<UserCommand>, Vec<String>) {
        parse_commands(Path::new("/etc/diffui/config.toml"), text)
    }

    #[test]
    fn a_row_reads_into_a_command() {
        let (commands, problems) = parse(
            r#"
[[commands]]
id = "user.open-in-zed"
label = "Open in Zed"
when = "file"
menu = ["file", "palette"]
keys = ["cmd+shift+e"]
exec = ["zed", "{root}/{path}:{line}"]

[[commands.prompts]]
name = "msg"
kind = "choose"
choices = ["a", "b"]
"#,
        );
        assert!(problems.is_empty(), "{problems:?}");
        let [command] = commands.as_slice() else {
            panic!("{commands:?}")
        };
        assert_eq!(command.id, "user.open-in-zed");
        assert!(command.places.file && command.places.palette && !command.places.revision);
        assert_eq!(command.keys, ["cmd+shift+e"]);
        assert!(matches!(&command.step, Step::Exec(argv) if argv.len() == 2));
        assert!(matches!(command.prompts[0].kind, PromptKind::Choose(ref c) if c.len() == 2));
        assert_eq!(command.source, "/etc/diffui/config.toml:3:6");
    }

    #[test]
    fn errors_name_the_file_line_and_column() {
        let (commands, problems) = parse(
            r#"
[[commands]]
id = "ok"
label = "Fine"
run = "jj new {change_id}"

[[commands]]
id = "bad-when"
label = "Bad"
when = "file &&"
exec = ["true"]

[[commands]]
id = "bad-var"
label = "Bad"
exec = ["echo", "{nope}"]

[[commands]]
id = "typo"
label = "Bad"
exce = ["echo"]

[[commands]]
id = "two"
label = "Bad"
exec = ["true"]
run = "jj log"
"#,
        );
        assert_eq!(
            commands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["ok"]
        );
        let at = |prefix: &str| {
            problems
                .iter()
                .find(|problem| problem.starts_with(prefix))
                .unwrap_or_else(|| panic!("nothing at {prefix}: {problems:#?}"))
        };
        // `when = "file &&"`: the clause runs out after its 7 bytes.
        assert!(at("/etc/diffui/config.toml:10:16:").contains("bad-when"));
        // `{nope}` starts at column 17 of `exec = ["echo", "{nope}"]`.
        assert!(at("/etc/diffui/config.toml:16:18:").contains("{nope}"));
        assert!(at("/etc/diffui/config.toml:21:1:").contains("exce"));
        assert!(at("/etc/diffui/config.toml:24:6:").contains("exactly one"));
        assert_eq!(problems.len(), 4, "{problems:#?}");
    }

    #[test]
    fn a_duplicate_id_keeps_the_first() {
        let (commands, problems) = parse(
            r#"
[[commands]]
id = "x"
label = "First"
exec = ["true"]

[[commands]]
id = "x"
label = "Second"
exec = ["true"]
"#,
        );
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].label, "First");
        assert!(problems[0].contains("also declared"), "{problems:?}");
    }

    #[test]
    fn an_unknown_when_key_warns_and_keeps_the_row() {
        let (commands, problems) = parse(
            r#"
[[commands]]
id = "x"
label = "X"
when = "file && bookmark.pr"
exec = ["true"]
"#,
        );
        assert_eq!(commands.len(), 1);
        assert!(problems[0].contains("bookmark.pr"), "{problems:?}");
    }

    #[test]
    fn the_shipped_rows_load_cleanly() {
        let (commands, problems) = parse_commands(Path::new("commands.toml"), SHIPPED);
        assert!(problems.is_empty(), "{problems:#?}");
        let ids: Vec<&str> = commands.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["file.open-in-editor", "ai.describe"]);
    }
}
