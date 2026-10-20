//! Completion candidates shared by every text field that speaks jj: the
//! command line (`jj rebase -s …`) and the revset field. The command-line
//! grammar decides *what kind* of value sits under the cursor; a
//! [`ValueCompleter`] supplies the candidates for that kind.

use std::ops::Range;

/// What a candidate is, for its icon and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    Subcommand,
    Flag,
    /// One of a flag's fixed choices (`--color never`).
    Value,
    /// A revset function such as `trunk()` or `ancestors(x, depth)`.
    Function,
    /// A `[revset-aliases]` entry.
    Alias,
    Bookmark,
    RemoteBookmark,
    Tag,
    Remote,
    /// A change id (or commit id) prefix.
    Revision,
    Path,
    /// A previously run command or revset.
    History,
}

/// One candidate. Accepting it replaces `replace` (byte offsets into the
/// whole input) with `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub replace: Range<usize>,
    pub insert: String,
    /// What the list shows; usually `insert`, but e.g. `ancestors(x, depth)`
    /// for a function whose `insert` is `ancestors(`.
    pub label: String,
    /// The muted second column: a flag's help, a revision's title.
    pub detail: String,
    pub kind: CompletionKind,
}

/// What a command-line argument expects, as far as completion cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Revset,
    Bookmark,
    Remote,
    Path,
    /// Free text (`-m`): no candidates.
    Text,
    Other,
}

/// Supplies candidates for a value of `kind`. `text` is the whole value
/// token as typed and `cursor` a byte offset into it; the returned
/// [`Completion::replace`] ranges are relative to `text`, and the caller
/// shifts them into the full input.
pub trait ValueCompleter {
    fn complete(&self, kind: ValueKind, text: &str, cursor: usize) -> Vec<Completion>;
}

/// A completer with nothing to offer: flags and subcommands still complete.
pub struct NoValues;

impl ValueCompleter for NoValues {
    fn complete(&self, _kind: ValueKind, _text: &str, _cursor: usize) -> Vec<Completion> {
        Vec::new()
    }
}
