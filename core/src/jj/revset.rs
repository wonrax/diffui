//! Revsets as the user types them: parsed with the graph walk's aliases and
//! workspace, errors kept in jj's words with the position they point at, and
//! resolved to commit ids with jj's single-revision and no-gaps rules.

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt as _;
use jj_lib::{
    backend::CommitId,
    object_id::ObjectId as _,
    ref_name::WorkspaceName,
    repo::ReadonlyRepo,
    revset::{
        ResolvedRevsetExpression, RevsetExpression, RevsetParseError, RevsetParseErrorKind,
        RevsetResolutionError, SymbolResolver, UserRevsetExpression,
    },
};

use super::walk::parse_revset_raw;
use crate::repo::CancelFlag;
use crate::revset::{COUNT_CAP, RevsetError, RevsetExpect, RevsetMatch, RevsetQuery};

/// Where a revset is parsed and resolved: the repo at head, the workspace
/// whose `@` it means, and the root its file patterns are relative to.
#[derive(Clone, Copy)]
pub(crate) struct RevsetScope<'a> {
    pub repo: &'a ReadonlyRepo,
    pub workspace_name: &'a WorkspaceName,
    pub repo_root: &'a Path,
}

impl RevsetScope<'_> {
    /// Parse `src` the way the graph walk does: the same aliases, the same
    /// `@`, and an empty revset or `all()` meaning everything.
    pub fn parse(&self, src: &str) -> Result<Arc<UserRevsetExpression>, RevsetError> {
        let trimmed = src.trim();
        if trimmed.is_empty() || trimmed == "all()" {
            return Ok(RevsetExpression::all());
        }
        let lead = src.len() - src.trim_start().len();
        parse_revset_raw(
            self.repo_root,
            self.repo.settings(),
            self.workspace_name,
            trimmed,
        )
        .map_err(|error| RevsetError::new(format!("{error:#}")))?
        .map_err(|error| parse_error(trimmed, lead, &error))
    }

    /// Resolve `expression`'s symbols, pointing an unknown one out in `src`.
    fn resolve(
        &self,
        src: &str,
        expression: &Arc<UserRevsetExpression>,
    ) -> Result<Arc<ResolvedRevsetExpression>, RevsetError> {
        let resolver = SymbolResolver::new(
            self.repo,
            &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
        );
        expression
            .resolve_user_expression(self.repo, &resolver)
            .map_err(|error| resolution_error(src, error))
    }

    /// jj's no-gaps rule for `jj diff -r`: every commit between the set's
    /// roots and heads must be in the set.
    async fn check_gaps(
        &self,
        src: &str,
        expression: &Arc<UserRevsetExpression>,
    ) -> Result<(), RevsetError> {
        let gaps = expression
            .roots()
            .range(&expression.heads())
            .minus(expression);
        let resolved = self.resolve(src, &gaps)?;
        let revset = resolved.evaluate(self.repo).map_err(evaluation_error)?;
        let first = revset.stream().next().await;
        match first {
            None => Ok(()),
            Some(id) => {
                let id = id.map_err(evaluation_error)?;
                Err(
                    RevsetError::new("Cannot diff revsets with gaps in.").with_hint(format!(
                        "Revision {} would need to be in the set.",
                        short_hash(&id)
                    )),
                )
            }
        }
    }

    /// Answer a [`RevsetQuery`]. `None` when cancelled midway.
    pub async fn query(
        &self,
        query: &RevsetQuery,
        cancel: &CancelFlag,
    ) -> Option<Result<RevsetMatch, RevsetError>> {
        /// Commits between flag checks while counting.
        const CANCEL_CHECK_CHUNK: usize = 1024;

        let src = query.revset.as_str();
        let expression = match self.parse(src) {
            Ok(expression) => expression,
            Err(error) => return Some(Err(error)),
        };
        if query.expect == RevsetExpect::Contiguous
            && let Err(error) = self.check_gaps(src, &expression).await
        {
            return Some(Err(error));
        }
        let revset = match self
            .resolve(src, &expression)
            .and_then(|resolved| resolved.evaluate(self.repo).map_err(evaluation_error))
        {
            Ok(revset) => revset,
            Err(error) => return Some(Err(error)),
        };
        let mut found = RevsetMatch {
            commits: Vec::new(),
            count: 0,
            count_exact: true,
        };
        let mut stream = revset.stream();
        while let Some(id) = stream.next().await {
            let id = match id {
                Ok(id) => id,
                Err(error) => return Some(Err(evaluation_error(error))),
            };
            if found.count == COUNT_CAP {
                found.count_exact = false;
                break;
            }
            if found.commits.len() < query.limit {
                found.commits.push(id.hex());
            }
            found.count += 1;
            if found.count.is_multiple_of(CANCEL_CHECK_CHUNK) {
                if cancel.is_cancelled() {
                    return None;
                }
                tokio::task::yield_now().await;
            }
        }
        if query.expect == RevsetExpect::Single
            && let Err(error) = single(src, found.count)
        {
            return Some(Err(error));
        }
        Some(Ok(found))
    }
}

/// jj-cli's `resolve_single_rev` wording.
fn single(src: &str, count: usize) -> Result<(), RevsetError> {
    match count {
        1 => Ok(()),
        0 => Err(RevsetError::new(format!(
            "Revset `{src}` didn't resolve to any revisions"
        ))),
        _ => Err(RevsetError::new(format!(
            "Revset `{src}` resolved to more than one revision"
        ))),
    }
}

/// jj's `short_commit_hash`: the first 12 hex digits.
fn short_hash(id: &CommitId) -> String {
    id.hex().chars().take(12).collect()
}

fn evaluation_error(error: jj_lib::revset::RevsetEvaluationError) -> RevsetError {
    RevsetError::new(error.to_string())
}

// ── errors ──────────────────────────────────────────────────────────────

/// A parse error in jj's words, with the span it underlines. `src` is the
/// text jj-lib parsed and `offset` where it starts in what the user typed.
fn parse_error(src: &str, offset: usize, error: &RevsetParseError) -> RevsetError {
    let (message, hint) = parse_message(error);
    let span =
        pest_span(src, &error.to_string()).map(|span| span.start + offset..span.end + offset);
    RevsetError {
        message,
        hint,
        span,
    }
}

fn parse_message(error: &RevsetParseError) -> (String, Option<String>) {
    match error.kind() {
        RevsetParseErrorKind::SyntaxError => {
            // pest says what it expected on its `= …` line.
            let expected = error
                .to_string()
                .lines()
                .find_map(|line| line.trim_start().strip_prefix("= "))
                .map(|expected| capitalize(expected.trim()));
            ("Syntax error".to_owned(), expected)
        }
        RevsetParseErrorKind::NoSuchFunction { name, candidates } => (
            format!("Function `{name}` doesn't exist"),
            did_you_mean(candidates),
        ),
        RevsetParseErrorKind::InAliasExpansion(_)
        | RevsetParseErrorKind::InParameterExpansion(_) => match error.origin() {
            Some(origin) => {
                let (inner, hint) = parse_message(origin);
                (format!("{}: {inner}", error.kind()), hint)
            }
            None => (error.kind().to_string(), None),
        },
        kind => (kind.to_string(), None),
    }
}

fn did_you_mean(candidates: &[String]) -> Option<String> {
    (!candidates.is_empty()).then(|| {
        let names: Vec<String> = candidates.iter().map(|name| format!("`{name}`")).collect();
        format!("Did you mean {}?", names.join(", "))
    })
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The byte range a pest error report underlines in `src`:
///
/// ```text
///  --> 1:6
///   |
/// 1 | foo(
///   |      ^---
/// ```
///
/// `^---` marks a position (an empty range), `^----^` a span. jj-lib keeps
/// pest's error private, so its report is the only place the span survives.
fn pest_span(src: &str, report: &str) -> Option<std::ops::Range<usize>> {
    let location = report
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("--> "))?;
    let (line, column) = location.trim().split_once(':')?;
    let (line, column): (usize, usize) = (line.parse().ok()?, column.parse().ok()?);
    let line_start: usize = src
        .split('\n')
        .take(line.saturating_sub(1))
        .map(|text| text.len() + 1)
        .sum();
    let line_text = src.get(line_start..)?.split('\n').next()?;
    let to_byte = |chars: usize| {
        line_start
            + line_text
                .char_indices()
                .nth(chars)
                .map_or(line_text.len(), |(at, _)| at)
    };
    let start = to_byte(column.saturating_sub(1));
    // The marker line is the first `|` line with a caret on it.
    let marker = report
        .lines()
        .filter_map(|line| line.split_once('|').map(|(_, rest)| rest))
        .find(|rest| rest.contains('^'))?;
    let underline = marker.trim();
    let width = underline.chars().count();
    let end = if underline.ends_with('^') && width > 1 {
        to_byte(column - 1 + width)
    } else if underline == "^" {
        to_byte(column)
    } else {
        start
    };
    Some(start..end.max(start))
}

/// A resolution error in jj's words. An unknown or ambiguous symbol gets a
/// span when it can be found in `src` as a whole word.
fn resolution_error(src: &str, error: RevsetResolutionError) -> RevsetError {
    let symbol = match &error {
        RevsetResolutionError::NoSuchRevision { name, .. } => Some(name.clone()),
        RevsetResolutionError::AmbiguousCommitIdPrefix(prefix)
        | RevsetResolutionError::AmbiguousChangeIdPrefix(prefix) => Some(prefix.clone()),
        RevsetResolutionError::DivergentChangeId { symbol, .. }
        | RevsetResolutionError::ConflictedRef { symbol, .. } => Some(symbol.clone()),
        _ => None,
    };
    let hint = match &error {
        RevsetResolutionError::NoSuchRevision { candidates, .. } => did_you_mean(candidates),
        _ => None,
    };
    let span = symbol.and_then(|symbol| find_symbol(src, &symbol));
    // The ambiguity errors gain the CLI's way forward.
    let message = match error {
        RevsetResolutionError::DivergentChangeId { .. }
        | RevsetResolutionError::ConflictedRef { .. } => {
            format!("{}", super::walk::describe_resolution_error(error))
        }
        error => error.to_string(),
    };
    RevsetError {
        message,
        hint,
        span,
    }
}

/// The first place `symbol` appears in `src` as a whole symbol (not inside a
/// longer name).
fn find_symbol(src: &str, symbol: &str) -> Option<std::ops::Range<usize>> {
    if symbol.is_empty() {
        return None;
    }
    let is_name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '/' | '-' | '.' | '+' | '*');
    let mut from = 0;
    while let Some(found) = src[from..].find(symbol) {
        let start = from + found;
        let end = start + symbol.len();
        let before = src[..start].chars().next_back();
        let after = src[end..].chars().next();
        let bounded = |c: Option<char>| c.is_none_or(|c| !is_name(c));
        // `..` before it is the range operator, not part of a name.
        if (bounded(before) || src[..start].ends_with("..")) && bounded(after) {
            return Some(start..end);
        }
        from = start + symbol.chars().next().map_or(1, char::len_utf8);
    }
    None
}

#[cfg(test)]
mod tests {
    use jj_lib::config::StackedConfig;
    use jj_lib::settings::UserSettings;

    use super::*;

    fn parse(src: &str) -> RevsetError {
        let settings = UserSettings::from_config(StackedConfig::with_defaults()).unwrap();
        let trimmed = src.trim();
        let lead = src.len() - src.trim_start().len();
        let error = parse_revset_raw(
            Path::new("/tmp"),
            &settings,
            WorkspaceName::DEFAULT,
            trimmed,
        )
        .unwrap()
        .expect_err("the revset should not parse");
        parse_error(trimmed, lead, &error)
    }

    #[test]
    fn syntax_errors_point_at_the_position() {
        let error = parse("trunk()|foo(");
        assert_eq!(error.message, "Syntax error");
        assert_eq!(
            error.hint.as_deref(),
            Some("Expected <strict_identifier> or <expression>")
        );
        assert_eq!(error.span, Some(12..12));
        let error = parse("x &");
        assert_eq!(error.span, Some(3..3));
    }

    #[test]
    fn unknown_functions_span_their_name() {
        let error = parse("  @ | nosuch()");
        assert_eq!(error.message, "Function `nosuch` doesn't exist");
        assert_eq!(error.span, Some(6..12), "jj underlines the name");
        let error = parse("ancestor(@)");
        assert_eq!(error.span, Some(0..8));
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("`ancestors`")),
            "{error:?}"
        );
    }

    #[test]
    fn alias_errors_name_the_alias() {
        let mut config = StackedConfig::with_defaults();
        config.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::User,
                "[revset-aliases]\n'broken()' = 'nosuch()'\n",
            )
            .unwrap(),
        );
        let settings = UserSettings::from_config(config).unwrap();
        let error = parse_revset_raw(
            Path::new("/tmp"),
            &settings,
            WorkspaceName::DEFAULT,
            "@ | broken()",
        )
        .unwrap()
        .expect_err("the alias body doesn't parse");
        let error = parse_error("@ | broken()", 0, &error);
        assert_eq!(
            error.message,
            "In alias `broken()`: Function `nosuch` doesn't exist"
        );
        assert_eq!(error.span, Some(4..12), "the alias call, in what was typed");
    }

    #[test]
    fn spans_count_characters_not_bytes() {
        let error = parse("description(é) | nosuch()");
        assert_eq!(error.span, Some(18..24));
    }

    #[test]
    fn symbols_are_found_as_whole_words() {
        assert_eq!(find_symbol("main-2 | main", "main"), Some(9..13));
        assert_eq!(find_symbol("trunk()..xyz", "xyz"), Some(9..12));
        assert_eq!(find_symbol("xyzzy", "xyz"), None);
    }

    #[test]
    fn single_revision_wording_matches_jj() {
        assert_eq!(
            single("all()", 5).unwrap_err().message,
            "Revset `all()` resolved to more than one revision"
        );
        assert_eq!(
            single("none()", 0).unwrap_err().message,
            "Revset `none()` didn't resolve to any revisions"
        );
        assert!(single("@", 1).is_ok());
    }
}
