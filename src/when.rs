//! When-clauses: the predicate a declared command carries to say where it
//! applies, e.g. `repo.mutate && selection.count >= 2`. The host evaluates it
//! synchronously against the app's state each time a menu, the palette or a
//! key asks, so a command's availability never waits on anything.
//!
//! ## Syntax
//!
//! ```text
//! expr    := or
//! or      := and ( "||" and )*
//! and     := unary ( "&&" unary )*
//! unary   := "!" unary | compare
//! compare := operand ( ( "==" | "!=" | ">=" | "<=" | ">" | "<" ) operand )?
//! operand := key | "string" | 'string' | number | true | false | "(" expr ")"
//! key     := [a-z_][a-z0-9_.-]*
//! ```
//!
//! A key on its own is true when its value is: `true`, a non-zero number, a
//! non-empty string. `==` and `!=` compare like values (a string with a
//! string, a number with a number); the ordering operators compare numbers
//! only and are false otherwise. A key the host doesn't know is reported when
//! the clause loads and reads as false.
//!
//! The keys are [`KEYS`]: one table, so adding a key (a plugin's own, say)
//! is one row.

use std::fmt;

use crate::Diffui;
use crate::commands::CommandArg;

/// A value a key reads as, or a literal in a clause.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
    /// The key has no value here (no bookmark in context, an unknown key).
    Missing,
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Bool(value) => *value,
            Value::Number(value) => *value != 0.0,
            Value::Text(text) => !text.is_empty(),
            Value::Missing => false,
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

/// Where a clause reads its keys.
pub(crate) trait Keys {
    fn get(&self, key: &str) -> Value;
}

/// One key the host answers: its name, and how it reads the app. What each
/// means is documented for users in `assets/agent/SKILL.md`.
pub(crate) struct KeyDef {
    pub(crate) name: &'static str,
    read: fn(&Diffui, &CommandArg) -> Value,
}

/// Every key a clause can name.
pub(crate) static KEYS: &[KeyDef] = &[
    KeyDef {
        // "jj", "git" or "pr" (a pull-request tab)
        name: "repo.kind",
        read: |ui, _| match &ui.active().repository {
            Some(repository) => Value::Text(
                match repository.vcs {
                    crate::repository::Vcs::Jj => "jj",
                    crate::repository::Vcs::Git => "git",
                }
                .to_owned(),
            ),
            None if ui.tabs.is_empty() => Value::Missing,
            None => Value::Text("pr".to_owned()),
        },
    },
    KeyDef {
        // the repository can be rewritten (jj)
        name: "repo.mutate",
        read: |ui, _| ui.active().session.capabilities.mutate.into(),
    },
    KeyDef {
        // "github", "gitlab" or a forge plugin's id: the forge that claims a
        // remote
        name: "repo.forge",
        read: |ui, _| match ui.active().forge.kind() {
            Some(kind) => Value::Text(kind.id().to_owned()),
            None => Value::Missing,
        },
    },
    KeyDef {
        // a revision is in context and in the log
        name: "revision.selected",
        read: |ui, arg| context::revision_row(ui, arg).is_some().into(),
    },
    KeyDef {
        // the revision in context isn't immutable
        name: "revision.mutable",
        read: |ui, arg| match context::revision_row(ui, arg) {
            Some(row) => (!row.is_immutable()).into(),
            None => Value::Missing,
        },
    },
    KeyDef {
        // the revision in context is the working copy
        name: "revision.is_working_copy",
        read: |ui, arg| match context::revision_row(ui, arg) {
            Some(row) => row.is_working_copy().into(),
            None => Value::Missing,
        },
    },
    KeyDef {
        // how many rows are marked in the log
        name: "selection.count",
        read: |ui, _| Value::Number(context::marked_count(ui) as f64),
    },
    KeyDef {
        // a bookmark is in context: the one picked, else the revision's first
        name: "bookmark",
        read: |ui, arg| context::bookmark(ui, arg).is_some().into(),
    },
    KeyDef {
        // the bookmark in context exists on a remote
        name: "bookmark.has_remote",
        read: |ui, arg| match context::bookmark(ui, arg) {
            Some(name) => context::has_remote(ui, &name).into(),
            None => Value::Missing,
        },
    },
    KeyDef {
        // the bookmark in context's pull request: "open", "draft", "merged"
        // or "closed"
        name: "bookmark.pr",
        read: |ui, arg| {
            let forge = &ui.active().forge;
            context::bookmark(ui, arg)
                .and_then(|name| Some(forge.of(crate::forge::local_name(&name)?)?.pr.state))
                .map_or(Value::Missing, |state| {
                    Value::Text(state.label().to_owned())
                })
        },
    },
    KeyDef {
        // a file is in context: the one picked, else the diff's selected file
        name: "file",
        read: |ui, arg| context::file(ui, arg).is_some().into(),
    },
    KeyDef {
        // a diff line is in context (line-select mode has a cursor)
        name: "diff.line",
        read: |ui, _| context::cursor_line(ui).is_some().into(),
    },
];

pub(crate) mod context {
    //! What a command's context is: the reads keys and template variables
    //! share, so `{bookmark}` and `bookmark` never disagree.

    use diffui_core::RowView;

    use crate::Diffui;
    use crate::commands::{CommandArg, target_revision};
    use crate::palette::ResultRef;

    pub(crate) fn revision_row<'a>(ui: &'a Diffui, arg: &CommandArg) -> Option<RowView<'a>> {
        let commits = &ui.active().session.commits;
        if let CommandArg::Bookmark { name, .. } | CommandArg::Result(ResultRef::Bookmark(name)) =
            arg
        {
            return commits.find_by_bookmark(name);
        }
        commits.find(&target_revision(ui, arg)?)
    }

    pub(crate) fn marked_count(ui: &Diffui) -> usize {
        let tab = ui.active();
        tab.revision_multi_selection
            .iter()
            .filter(|id| tab.session.commits.find_by_commit_id(id).is_some())
            .count()
    }

    pub(crate) fn bookmark(ui: &Diffui, arg: &CommandArg) -> Option<String> {
        match arg {
            CommandArg::Bookmark { name, .. } | CommandArg::Result(ResultRef::Bookmark(name)) => {
                Some(name.clone())
            }
            _ => revision_row(ui, arg)?
                .bookmarks()
                .first()
                .map(|name| name.trim_end_matches('?').to_owned()),
        }
    }

    pub(crate) fn has_remote(ui: &Diffui, name: &str) -> bool {
        ui.active()
            .session
            .bookmarks
            .entry(name)
            .is_some_and(|entry| !entry.remotes.is_empty())
    }

    pub(crate) fn file(ui: &Diffui, arg: &CommandArg) -> Option<String> {
        match arg {
            CommandArg::Path(path) | CommandArg::Result(ResultRef::File(path)) => {
                Some(path.clone())
            }
            CommandArg::Browse { path, .. } => path.clone(),
            CommandArg::None => {
                let tab = ui.active();
                tab.session
                    .document
                    .files
                    .get(tab.selected_file)
                    .map(|file| file.path.clone())
            }
            _ => None,
        }
    }

    /// The path and line number under line-select mode's cursor.
    pub(crate) fn cursor_line(ui: &Diffui) -> Option<(String, usize)> {
        crate::changes::cursor_line(ui)
    }
}

/// The app as a clause sees it, for a command run on `arg`.
pub(crate) struct AppKeys<'a> {
    pub(crate) ui: &'a Diffui,
    pub(crate) arg: &'a CommandArg,
}

impl Keys for AppKeys<'_> {
    fn get(&self, key: &str) -> Value {
        KEYS.iter()
            .find(|def| def.name == key)
            .map_or(Value::Missing, |def| (def.read)(self.ui, self.arg))
    }
}

/// A parsed clause.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct When(Expr);

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Key(String),
    Literal(Value),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(Box<Expr>, Op, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Ge,
    Le,
    Gt,
    Lt,
}

/// A clause that doesn't parse: what's wrong, at a byte offset into it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WhenError {
    pub(crate) message: String,
    pub(crate) offset: usize,
}

impl fmt::Display for WhenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.message, self.offset + 1)
    }
}

impl When {
    pub(crate) fn parse(text: &str) -> Result<Self, WhenError> {
        let tokens = lex(text)?;
        let mut parser = Parser {
            tokens: &tokens,
            at: 0,
            end: text.len(),
        };
        let expr = parser.or()?;
        match parser.peek() {
            None => Ok(When(expr)),
            Some((offset, token)) => Err(WhenError {
                message: format!("unexpected {}", token.describe()),
                offset,
            }),
        }
    }

    pub(crate) fn eval(&self, keys: &dyn Keys) -> bool {
        eval(&self.0, keys).truthy()
    }

    /// The keys it names that [`KEYS`] doesn't have.
    pub(crate) fn unknown_keys(&self) -> Vec<String> {
        let mut unknown = Vec::new();
        collect_keys(&self.0, &mut |key| {
            if !KEYS.iter().any(|def| def.name == key) && !unknown.iter().any(|seen| seen == key) {
                unknown.push(key.to_owned());
            }
        });
        unknown
    }
}

fn collect_keys(expr: &Expr, found: &mut impl FnMut(&str)) {
    match expr {
        Expr::Key(key) => found(key),
        Expr::Literal(_) => {}
        Expr::Not(inner) => collect_keys(inner, found),
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Compare(left, _, right) => {
            collect_keys(left, found);
            collect_keys(right, found);
        }
    }
}

fn eval(expr: &Expr, keys: &dyn Keys) -> Value {
    match expr {
        Expr::Key(key) => keys.get(key),
        Expr::Literal(value) => value.clone(),
        Expr::Not(inner) => (!eval(inner, keys).truthy()).into(),
        Expr::And(left, right) => (eval(left, keys).truthy() && eval(right, keys).truthy()).into(),
        Expr::Or(left, right) => (eval(left, keys).truthy() || eval(right, keys).truthy()).into(),
        Expr::Compare(left, op, right) => {
            compare(&eval(left, keys), *op, &eval(right, keys)).into()
        }
    }
}

fn compare(left: &Value, op: Op, right: &Value) -> bool {
    use std::cmp::Ordering;
    let ordering = match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
        (Value::Text(a), Value::Text(b)) if matches!(op, Op::Eq | Op::Ne) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) if matches!(op, Op::Eq | Op::Ne) => Some(a.cmp(b)),
        _ => None,
    };
    match (op, ordering) {
        (Op::Eq, ordering) => ordering == Some(Ordering::Equal),
        (Op::Ne, ordering) => ordering != Some(Ordering::Equal),
        (_, None) => false,
        (Op::Ge, Some(ordering)) => ordering != Ordering::Less,
        (Op::Le, Some(ordering)) => ordering != Ordering::Greater,
        (Op::Gt, Some(ordering)) => ordering == Ordering::Greater,
        (Op::Lt, Some(ordering)) => ordering == Ordering::Less,
    }
}

// ── Lexing and parsing ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Key(String),
    Text(String),
    Number(f64),
    Bool(bool),
    Op(Op),
    And,
    Or,
    Not,
    Open,
    Close,
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::Key(key) => format!("`{key}`"),
            Token::Text(text) => format!("\"{text}\""),
            Token::Number(number) => format!("`{number}`"),
            Token::Bool(value) => format!("`{value}`"),
            Token::Op(_) => "comparison".to_owned(),
            Token::And => "`&&`".to_owned(),
            Token::Or => "`||`".to_owned(),
            Token::Not => "`!`".to_owned(),
            Token::Open => "`(`".to_owned(),
            Token::Close => "`)`".to_owned(),
        }
    }
}

fn lex(text: &str) -> Result<Vec<(usize, Token)>, WhenError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    let error = |message: &str, offset: usize| WhenError {
        message: message.to_owned(),
        offset,
    };
    while at < bytes.len() {
        let start = at;
        let rest = &text[at..];
        let two = rest.get(..2).unwrap_or("");
        let token = match bytes[at] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                at += 1;
                continue;
            }
            _ if two == "&&" => Token::And,
            _ if two == "||" => Token::Or,
            _ if two == "==" => Token::Op(Op::Eq),
            _ if two == "!=" => Token::Op(Op::Ne),
            _ if two == ">=" => Token::Op(Op::Ge),
            _ if two == "<=" => Token::Op(Op::Le),
            b'>' => Token::Op(Op::Gt),
            b'<' => Token::Op(Op::Lt),
            b'!' => Token::Not,
            b'(' => Token::Open,
            b')' => Token::Close,
            b'&' | b'|' => return Err(error("expected `&&` or `||`", at)),
            b'=' => return Err(error("expected `==`", at)),
            quote @ (b'"' | b'\'') => {
                let Some(length) = rest[1..].find(quote as char) else {
                    return Err(error("unterminated string", at));
                };
                at += length + 2;
                tokens.push((start, Token::Text(rest[1..1 + length].to_owned())));
                continue;
            }
            b'0'..=b'9' | b'-' => {
                let length = rest
                    .char_indices()
                    .skip(1)
                    .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
                    .map_or(rest.len(), |(index, _)| index);
                let number = rest[..length]
                    .parse()
                    .map_err(|_| error("not a number", at))?;
                at += length;
                tokens.push((start, Token::Number(number)));
                continue;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let length = rest
                    .char_indices()
                    .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
                    .map_or(rest.len(), |(index, _)| index);
                let word = &rest[..length];
                at += length;
                tokens.push((
                    start,
                    match word {
                        "true" => Token::Bool(true),
                        "false" => Token::Bool(false),
                        _ => Token::Key(word.to_owned()),
                    },
                ));
                continue;
            }
            _ => {
                let c = rest.chars().next().unwrap_or(' ');
                return Err(error(&format!("unexpected `{c}`"), at));
            }
        };
        at += match token {
            Token::And | Token::Or | Token::Op(Op::Eq | Op::Ne | Op::Ge | Op::Le) => 2,
            _ => 1,
        };
        tokens.push((start, token));
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [(usize, Token)],
    at: usize,
    /// The clause's length: where "ran out" errors point.
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<(usize, &Token)> {
        self.tokens
            .get(self.at)
            .map(|(offset, token)| (*offset, token))
    }

    fn next(&mut self) -> Option<(usize, Token)> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn or(&mut self) -> Result<Expr, WhenError> {
        let mut left = self.and()?;
        while matches!(self.peek(), Some((_, Token::Or))) {
            self.at += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, WhenError> {
        let mut left = self.unary()?;
        while matches!(self.peek(), Some((_, Token::And))) {
            self.at += 1;
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, WhenError> {
        if matches!(self.peek(), Some((_, Token::Not))) {
            self.at += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        let left = self.operand()?;
        if let Some((_, Token::Op(op))) = self.peek() {
            let op = *op;
            self.at += 1;
            let right = self.operand()?;
            return Ok(Expr::Compare(Box::new(left), op, Box::new(right)));
        }
        Ok(left)
    }

    fn operand(&mut self) -> Result<Expr, WhenError> {
        let end = self.end;
        match self.next() {
            Some((_, Token::Key(key))) => Ok(Expr::Key(key)),
            Some((_, Token::Text(text))) => Ok(Expr::Literal(Value::Text(text))),
            Some((_, Token::Number(number))) => Ok(Expr::Literal(Value::Number(number))),
            Some((_, Token::Bool(value))) => Ok(Expr::Literal(Value::Bool(value))),
            Some((open, Token::Open)) => {
                let inner = self.or()?;
                match self.next() {
                    Some((_, Token::Close)) => Ok(inner),
                    Some((offset, token)) => Err(WhenError {
                        message: format!("expected `)`, found {}", token.describe()),
                        offset,
                    }),
                    None => Err(WhenError {
                        message: "unclosed `(`".to_owned(),
                        offset: open,
                    }),
                }
            }
            Some((offset, token)) => Err(WhenError {
                message: format!("expected a key or a value, found {}", token.describe()),
                offset,
            }),
            None => Err(WhenError {
                message: "expected a key or a value".to_owned(),
                offset: end,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Map(HashMap<&'static str, Value>);

    impl Keys for Map {
        fn get(&self, key: &str) -> Value {
            self.0.get(key).cloned().unwrap_or(Value::Missing)
        }
    }

    fn keys() -> Map {
        Map(HashMap::from([
            ("repo.kind", Value::Text("jj".to_owned())),
            ("repo.mutate", Value::Bool(true)),
            ("selection.count", Value::Number(2.0)),
            ("bookmark", Value::Bool(false)),
        ]))
    }

    fn holds(clause: &str) -> bool {
        When::parse(clause)
            .unwrap_or_else(|error| panic!("{clause}: {error}"))
            .eval(&keys())
    }

    #[test]
    fn keys_compare_and_combine() {
        assert!(holds("repo.mutate"));
        assert!(!holds("bookmark"));
        assert!(holds("!bookmark"));
        assert!(holds("repo.kind == \"jj\""));
        assert!(holds("repo.kind != 'git'"));
        assert!(holds("selection.count >= 2 && selection.count < 3"));
        assert!(!holds("selection.count > 2"));
        assert!(holds("selection.count <= 2"));
        assert!(holds("bookmark || repo.mutate"));
        assert!(!holds("bookmark && repo.mutate"));
        assert!(holds("!(bookmark || !repo.mutate)"));
        assert!(holds("true"));
        assert!(!holds("false || 0"));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // (true && false) || true, not true && (false || true): both true;
        // so check the other way round.
        assert!(holds("repo.mutate || bookmark && false"));
        assert!(!holds("(repo.mutate || bookmark) && false"));
    }

    #[test]
    fn mismatched_and_missing_values_are_false() {
        assert!(!holds("repo.kind >= 1"));
        assert!(!holds("repo.kind == 1"));
        assert!(holds("repo.kind != 1"));
        assert!(!holds("no.such.key"));
        assert!(!holds("no.such.key == \"\""));
    }

    #[test]
    fn parse_errors_point_at_the_problem() {
        let error = |clause: &str| When::parse(clause).expect_err(clause);
        assert_eq!(error("repo.mutate &&").offset, 14);
        assert_eq!(error("repo.mutate & bookmark").offset, 12);
        assert_eq!(error("(repo.mutate").offset, 0);
        assert_eq!(error("repo.kind = \"jj\"").offset, 10);
        assert_eq!(error("repo.kind == \"jj").offset, 13);
        assert_eq!(error("a b").offset, 2);
        assert!(error("a b").message.contains("`b`"));
        assert_eq!(error("").offset, 0);
    }

    #[test]
    fn unknown_keys_are_named_once() {
        let when = When::parse("repo.mutate && nope || nope.too && nope").expect("parses");
        assert_eq!(when.unknown_keys(), ["nope", "nope.too"]);
    }

    #[test]
    fn every_key_is_named_like_a_key() {
        for def in KEYS {
            assert_eq!(
                When::parse(def.name).expect("a key parses"),
                When(Expr::Key(def.name.to_owned())),
                "{}",
                def.name
            );
        }
    }
}
