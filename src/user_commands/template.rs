//! Templates: the text of a declared command with `{variables}` in it.
//!
//! A template is one word. `exec`'s argv is a template per element and
//! `run`'s line is split into words before anything is substituted, so a
//! value is always exactly one argument: a path with a space, a description
//! with quotes or a `{root}` inside a branch name is never split, re-read or
//! expanded again. `{{` and `}}` are literal braces.

use std::collections::HashMap;
use std::fmt;

/// A variable a template can name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Var {
    /// The repository's root directory.
    Root,
    ChangeId,
    CommitId,
    Bookmark,
    /// The file in context, relative to the root.
    Path,
    /// The line in context: line-select mode's cursor in the file, else the
    /// file's first changed line, else 1.
    Line,
    Description,
    /// The revision's diff in git format.
    Diff,
    /// The last ten descriptions on the working copy's line, for an agent to
    /// match their style.
    RecentDescriptions,
    /// What the prompt named `.0` was answered with.
    Prompt(String),
}

/// Every variable but the prompts, by the name a template uses. What each
/// holds is documented for users in `assets/agent/SKILL.md`.
pub(crate) const VARS: &[(&str, Var)] = &[
    ("root", Var::Root),
    ("change_id", Var::ChangeId),
    ("commit_id", Var::CommitId),
    ("bookmark", Var::Bookmark),
    ("path", Var::Path),
    ("line", Var::Line),
    ("description", Var::Description),
    ("diff", Var::Diff),
    ("recent_descriptions", Var::RecentDescriptions),
];

impl Var {
    pub(crate) fn name(&self) -> String {
        match self {
            Var::Prompt(name) => format!("prompt.{name}"),
            var => VARS
                .iter()
                .find(|(_, known)| known == var)
                .map_or_else(String::new, |(name, _)| (*name).to_owned()),
        }
    }

    /// Read from the repository when the command runs, rather than from what
    /// is on screen.
    pub(crate) fn is_fetched(&self) -> bool {
        matches!(self, Var::Diff | Var::RecentDescriptions)
    }
}

/// The largest `{diff}` that goes in an argument rather than stdin.
pub(crate) const ARG_DIFF_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    Var(Var),
}

/// One parsed word.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Template(Vec<Part>);

/// A template that doesn't parse: what's wrong, at a byte offset into it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TemplateError {
    pub(crate) message: String,
    pub(crate) offset: usize,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The values a run has gathered, by variable.
pub(crate) type Values = HashMap<Var, String>;

impl Template {
    /// Parse `text`, whose prompt variables must be among `prompts`.
    pub(crate) fn parse(text: &str, prompts: &[&str]) -> Result<Self, TemplateError> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = text.char_indices().peekable();
        while let Some((at, c)) = rest.next() {
            match c {
                '{' if rest.peek().is_some_and(|(_, next)| *next == '{') => {
                    rest.next();
                    literal.push('{');
                }
                '}' if rest.peek().is_some_and(|(_, next)| *next == '}') => {
                    rest.next();
                    literal.push('}');
                }
                '}' => {
                    return Err(TemplateError {
                        message: "a lone `}`: write `}}` for a brace".to_owned(),
                        offset: at,
                    });
                }
                '{' => {
                    let Some(length) = text[at + 1..].find('}') else {
                        return Err(TemplateError {
                            message: "unclosed `{`: write `{{` for a brace".to_owned(),
                            offset: at,
                        });
                    };
                    let name = &text[at + 1..at + 1 + length];
                    let var = parse_var(name, prompts).ok_or_else(|| TemplateError {
                        message: format!("no variable `{{{name}}}`"),
                        offset: at,
                    })?;
                    let close = at + 1 + length;
                    while rest.next_if(|(index, _)| *index <= close).is_some() {}
                    if !literal.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Var(var));
                }
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Ok(Template(parts))
    }

    /// The variables it names, in order.
    pub(crate) fn vars(&self) -> impl Iterator<Item = &Var> {
        self.0.iter().filter_map(|part| match part {
            Part::Var(var) => Some(var),
            Part::Text(_) => None,
        })
    }

    /// The word with every variable replaced by its value, each value
    /// inserted as it is. `Err` names a variable with no value.
    pub(crate) fn render(&self, values: &Values) -> Result<String, Var> {
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Var(var) => out.push_str(values.get(var).ok_or_else(|| var.clone())?),
            }
        }
        Ok(out)
    }
}

fn parse_var(name: &str, prompts: &[&str]) -> Option<Var> {
    if let Some(prompt) = name.strip_prefix("prompt.") {
        return prompts
            .contains(&prompt)
            .then(|| Var::Prompt(prompt.to_owned()));
    }
    VARS.iter()
        .find(|(known, _)| *known == name)
        .map(|(_, var)| var.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(Var, &str)]) -> Values {
        pairs
            .iter()
            .map(|(var, value)| (var.clone(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn variables_are_replaced_and_braces_escape() {
        let template = Template::parse("{root}/{path}:{line} {{x}}", &[]).expect("parses");
        let rendered = template.render(&values(&[
            (Var::Root, "/repo"),
            (Var::Path, "src/a.rs"),
            (Var::Line, "12"),
        ]));
        assert_eq!(rendered, Ok("/repo/src/a.rs:12 {x}".to_owned()));
    }

    /// A value is inserted once, as text: braces in it are not variables and
    /// nothing in it is expanded again.
    #[test]
    fn a_value_is_never_expanded_again() {
        let template = Template::parse("{bookmark}", &[]).expect("parses");
        let rendered = template.render(&values(&[
            (Var::Bookmark, "{root} $(rm -rf ~) `x` 'y' \"z\""),
            (Var::Root, "/repo"),
        ]));
        assert_eq!(rendered, Ok("{root} $(rm -rf ~) `x` 'y' \"z\"".to_owned()));
    }

    #[test]
    fn a_missing_value_is_named() {
        let template = Template::parse("{path}", &[]).expect("parses");
        assert_eq!(template.render(&Values::new()), Err(Var::Path));
    }

    #[test]
    fn prompts_must_be_declared() {
        assert!(Template::parse("{prompt.msg}", &["msg"]).is_ok());
        let error = Template::parse("-m {prompt.nope}", &["msg"]).expect_err("undeclared");
        assert_eq!(error.offset, 3);
        assert!(error.message.contains("prompt.nope"), "{}", error.message);
    }

    #[test]
    fn bad_braces_are_errors() {
        assert_eq!(
            Template::parse("a{path", &[]).expect_err("unclosed").offset,
            1
        );
        assert_eq!(Template::parse("a}b", &[]).expect_err("lone").offset, 1);
        assert_eq!(
            Template::parse("{nope}", &[]).expect_err("unknown").offset,
            0
        );
    }
}
