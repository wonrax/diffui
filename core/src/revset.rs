//! Revset intelligence for every field that takes one: the request and
//! answer shapes for resolving a revset through the repository actor
//! ([`crate::repo::Command::ResolveRevset`]), which parses with the same
//! aliases and workspace context as the graph walk.

use std::fmt;
use std::ops::Range;

// ── resolution ──────────────────────────────────────────────────────────

/// What the caller needs a revset to resolve to, so the actor can refuse it
/// the way the jj command taking it would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevsetExpect {
    /// Any number of commits, none included (`jj log -r`, `jj rebase -s`).
    #[default]
    Any,
    /// Exactly one commit (`jj diff --from`, `jj new -A`): jj's "resolved to
    /// more than one revision" otherwise.
    Single,
    /// A set without gaps (`jj diff -r`): jj's "Cannot diff revsets with gaps
    /// in." otherwise.
    Contiguous,
}

/// One revset to parse and resolve against the current repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevsetQuery {
    pub revset: String,
    /// At most this many commit ids come back; the count is still exact up to
    /// [`COUNT_CAP`].
    pub limit: usize,
    pub expect: RevsetExpect,
}

impl RevsetQuery {
    /// A query for `revset` with the default id cap and no expectation.
    pub fn new(revset: impl Into<String>) -> Self {
        Self {
            revset: revset.into(),
            limit: DEFAULT_MATCH_LIMIT,
            expect: RevsetExpect::Any,
        }
    }

    pub fn expect(mut self, expect: RevsetExpect) -> Self {
        self.expect = expect;
        self
    }
}

/// How many commit ids a [`RevsetQuery::new`] asks for: plenty to tint every
/// row on screen, little enough to send on each debounced keystroke.
pub const DEFAULT_MATCH_LIMIT: usize = 2_000;

/// Counting stops here. Past it the count is a lower bound
/// ([`RevsetMatch::count_exact`] is false), so `all()` on a huge repository
/// doesn't walk every commit to print "100000+ revisions".
pub const COUNT_CAP: usize = 100_000;

/// A revset that resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevsetMatch {
    /// Commit ids (hex), newest first, at most [`RevsetQuery::limit`] of them.
    pub commits: Vec<String>,
    /// How many commits the revset matched (see [`COUNT_CAP`]).
    pub count: usize,
    pub count_exact: bool,
}

impl RevsetMatch {
    /// "1 revision", "3 revisions", "100000+ revisions".
    pub fn count_label(&self) -> String {
        let plus = if self.count_exact { "" } else { "+" };
        let noun = if self.count == 1 && self.count_exact {
            "revision"
        } else {
            "revisions"
        };
        format!("{}{plus} {noun}", self.count)
    }
}

/// Why a revset didn't parse or resolve, in jj's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevsetError {
    /// One line, jj's wording ("Function `foo` doesn't exist").
    pub message: String,
    /// jj's hint, when it prints one ("Revision 4c1b138ffcc6 would need to
    /// be in the set.").
    pub hint: Option<String>,
    /// Byte range into the revset as given, when the error points somewhere:
    /// parse errors always do, an unknown symbol does when it can be found.
    pub span: Option<Range<usize>>,
}

impl RevsetError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: None,
            span: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for RevsetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RevsetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_labels() {
        let label = |count, count_exact| {
            RevsetMatch {
                commits: Vec::new(),
                count,
                count_exact,
            }
            .count_label()
        };
        assert_eq!(label(1, true), "1 revision");
        assert_eq!(label(3, true), "3 revisions");
        assert_eq!(label(100_000, false), "100000+ revisions");
    }
}
