//! Revset intelligence for every field that takes one: completion over a
//! cheap symbol snapshot, and the request and answer shapes for resolving a
//! revset through the repository actor.
//!
//! Completion is pure and synchronous. It reads what the session already
//! holds (the loaded graph, the bookmark table, the alias list the last graph
//! load reported), so a keystroke never walks the repository. Resolution goes
//! through the actor ([`crate::repo::Command::ResolveRevset`]), which parses
//! with the same aliases and workspace context as the graph walk.

use std::fmt;
use std::ops::Range;

use crate::completion::{Completion, CompletionKind, ValueCompleter, ValueKind};
use crate::model::{BookmarksInfo, CommitStore};

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

// ── the symbol snapshot ─────────────────────────────────────────────────

/// A `[revset-aliases]` entry, jj's defaults included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevsetAlias {
    pub name: String,
    /// `None` for a symbol alias, the parameter names for a function alias
    /// (empty for `trunk()`).
    pub params: Option<Vec<String>>,
    pub definition: String,
}

impl RevsetAlias {
    /// `trunk()`, `ancestors_of(x, n)`, `wip`.
    pub fn signature(&self) -> String {
        match &self.params {
            None => self.name.clone(),
            Some(params) => format!("{}({})", self.name, params.join(", ")),
        }
    }
}

/// What revset completion knows beyond the graph and the bookmark table,
/// reported by each graph load (see [`crate::repo::GraphTail`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevsetSymbols {
    /// Sorted by name, then arity.
    pub aliases: Vec<RevsetAlias>,
    /// Every workspace's name, this one's included.
    pub workspaces: Vec<String>,
}

// ── built-in functions ──────────────────────────────────────────────────

/// What a function argument holds, as far as completion cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg {
    Revset,
    /// A string pattern naming bookmarks.
    BookmarkName,
    TagName,
    RemoteName,
    ChangeId,
    CommitId,
    /// A number, a date, a description pattern, a fileset: nothing to offer.
    Free,
}

struct Function {
    name: &'static str,
    /// The parameter list as jj's docs write it.
    params: &'static str,
    doc: &'static str,
    /// Positional argument kinds; the last repeats for variadic functions.
    args: &'static [Arg],
    /// The kind of a `remote=` keyword argument, for the ref functions.
    remote_keyword: bool,
}

const REF_ARGS: &[Arg] = &[Arg::BookmarkName, Arg::RemoteName];
const TAG_REF_ARGS: &[Arg] = &[Arg::TagName, Arg::RemoteName];
const REVSET_DEPTH: &[Arg] = &[Arg::Revset, Arg::Free];
const ONE_REVSET: &[Arg] = &[Arg::Revset];
const ONE_FREE: &[Arg] = &[Arg::Free];
const NONE: &[Arg] = &[];

/// jj-lib 0.45.1's built-in revset functions (`BUILTIN_FUNCTION_MAP`), with
/// the one-line gist of jj's docs. Keep in sync when bumping jj-lib.
const FUNCTIONS: &[Function] = &[
    f("parents", "x, [depth]", "Parents of x", REVSET_DEPTH),
    f("children", "x, [depth]", "Children of x", REVSET_DEPTH),
    f(
        "ancestors",
        "x, [depth]",
        "Ancestors of x, x included",
        REVSET_DEPTH,
    ),
    f(
        "descendants",
        "x, [depth]",
        "Descendants of x, x included",
        REVSET_DEPTH,
    ),
    f(
        "first_parent",
        "x, [depth]",
        "First parent of x",
        REVSET_DEPTH,
    ),
    f(
        "first_ancestors",
        "x, [depth]",
        "Ancestors along first parents",
        REVSET_DEPTH,
    ),
    f(
        "reachable",
        "srcs, domain",
        "Commits in domain reachable from srcs",
        &[Arg::Revset, Arg::Revset],
    ),
    f("connected", "x", "x::x, filling the gaps", ONE_REVSET),
    f("all", "", "All visible commits", NONE),
    f("none", "", "No commits", NONE),
    f(
        "change_id",
        "prefix",
        "Commits with this change id prefix",
        &[Arg::ChangeId],
    ),
    f(
        "commit_id",
        "prefix",
        "The commit with this commit id prefix",
        &[Arg::CommitId],
    ),
    f(
        "bookmarks",
        "[pattern]",
        "Local bookmark targets",
        &[Arg::BookmarkName],
    ),
    r(
        "remote_bookmarks",
        "[name], [[remote=]remote]",
        "Remote bookmark targets",
        REF_ARGS,
    ),
    r(
        "tracked_remote_bookmarks",
        "[name], [[remote=]remote]",
        "Tracked remote bookmark targets",
        REF_ARGS,
    ),
    r(
        "untracked_remote_bookmarks",
        "[name], [[remote=]remote]",
        "Untracked remote bookmark targets",
        REF_ARGS,
    ),
    f("tags", "[pattern]", "Tag targets", &[Arg::TagName]),
    r(
        "remote_tags",
        "[name], [[remote=]remote]",
        "Remote tag targets",
        TAG_REF_ARGS,
    ),
    r(
        "tracked_remote_tags",
        "[name], [[remote=]remote]",
        "Tracked remote tag targets",
        TAG_REF_ARGS,
    ),
    r(
        "untracked_remote_tags",
        "[name], [[remote=]remote]",
        "Untracked remote tag targets",
        TAG_REF_ARGS,
    ),
    f("visible_heads", "", "All visible heads", NONE),
    f("root", "", "The virtual root commit", NONE),
    f(
        "heads",
        "x",
        "Commits in x with no descendants in x",
        ONE_REVSET,
    ),
    f(
        "roots",
        "x",
        "Commits in x with no ancestors in x",
        ONE_REVSET,
    ),
    f(
        "latest",
        "x, [count]",
        "Newest commits in x by committer date",
        REVSET_DEPTH,
    ),
    f(
        "fork_point",
        "x",
        "Where the commits in x forked",
        ONE_REVSET,
    ),
    f(
        "merge_point",
        "x",
        "Where the commits in x merge",
        ONE_REVSET,
    ),
    f(
        "bisect",
        "x",
        "Commits splitting x about in half",
        ONE_REVSET,
    ),
    f(
        "exactly",
        "x, count",
        "x, or an error unless it has count commits",
        REVSET_DEPTH,
    ),
    f("merges", "", "Merge commits", NONE),
    f("forks", "", "Commits with more than one child", NONE),
    f("description", "pattern", "Description matches", ONE_FREE),
    f(
        "subject",
        "pattern",
        "First line of the description matches",
        ONE_FREE,
    ),
    f(
        "author",
        "pattern",
        "Author name or email matches",
        ONE_FREE,
    ),
    f("author_name", "pattern", "Author name matches", ONE_FREE),
    f("author_email", "pattern", "Author email matches", ONE_FREE),
    f("author_date", "pattern", "Author date matches", ONE_FREE),
    f("mine", "", "Commits you authored", NONE),
    f(
        "committer",
        "pattern",
        "Committer name or email matches",
        ONE_FREE,
    ),
    f(
        "committer_name",
        "pattern",
        "Committer name matches",
        ONE_FREE,
    ),
    f(
        "committer_email",
        "pattern",
        "Committer email matches",
        ONE_FREE,
    ),
    f(
        "committer_date",
        "pattern",
        "Committer date matches",
        ONE_FREE,
    ),
    f("signed", "", "Signed commits", NONE),
    f("empty", "", "Commits changing no files", NONE),
    f("files", "fileset", "Commits touching these paths", ONE_FREE),
    f(
        "diff_lines",
        "text, [files]",
        "Diffs with matching lines",
        &[Arg::Free, Arg::Free],
    ),
    f(
        "diff_lines_added",
        "text, [files]",
        "Diffs adding matching lines",
        &[Arg::Free, Arg::Free],
    ),
    f(
        "diff_lines_removed",
        "text, [files]",
        "Diffs removing matching lines",
        &[Arg::Free, Arg::Free],
    ),
    f(
        "diff_contains",
        "text, [files]",
        "Diffs with matching lines",
        &[Arg::Free, Arg::Free],
    ),
    f("conflicts", "", "Commits with conflicts", NONE),
    f("divergent", "", "Divergent commits", NONE),
    f(
        "present",
        "x",
        "x, or nothing if a symbol in it is missing",
        ONE_REVSET,
    ),
    f("working_copies", "", "Every workspace's working copy", NONE),
    f(
        "at_operation",
        "op, x",
        "x as of operation op",
        &[Arg::Free, Arg::Revset],
    ),
    f(
        "coalesce",
        "revsets...",
        "The first non-empty revset",
        ONE_REVSET,
    ),
];

const fn f(
    name: &'static str,
    params: &'static str,
    doc: &'static str,
    args: &'static [Arg],
) -> Function {
    Function {
        name,
        params,
        doc,
        args,
        remote_keyword: false,
    }
}

const fn r(
    name: &'static str,
    params: &'static str,
    doc: &'static str,
    args: &'static [Arg],
) -> Function {
    Function {
        name,
        params,
        doc,
        args,
        remote_keyword: true,
    }
}

fn builtin(name: &str) -> Option<&'static Function> {
    FUNCTIONS.iter().find(|function| function.name == name)
}

/// What jj's docs say about the aliases jj ships, which read better than
/// their definitions.
fn builtin_alias_doc(alias: &RevsetAlias) -> Option<&'static str> {
    if alias
        .params
        .as_ref()
        .is_none_or(|params| !params.is_empty())
    {
        return None;
    }
    Some(match alias.name.as_str() {
        "trunk" => "The default bookmark's head on origin or upstream",
        "builtin_log" => "What jj log shows by default",
        "builtin_immutable_heads" => "jj's default immutable_heads()",
        "immutable_heads" => "Heads of the immutable commits",
        "immutable" => "Commits jj won't rewrite",
        "mutable" => "Commits jj may rewrite",
        "visible" => "Every visible commit",
        "hidden" => "Every hidden commit",
        _ => return None,
    })
}

// ── completion ──────────────────────────────────────────────────────────

/// How many candidates one completion returns at most.
const MAX_CANDIDATES: usize = 60;
/// How many change ids, of those matching, make the list.
const MAX_REVISIONS: usize = 20;
/// With nothing typed yet, how many of the newest rows to offer.
const RECENT_REVISIONS: usize = 8;

/// Completes revsets from what the session already holds. Cheap to build;
/// make one per keystroke with [`crate::Session::revset_completer`].
pub struct RevsetCompleter<'a> {
    pub symbols: &'a RevsetSymbols,
    pub bookmarks: &'a BookmarksInfo,
    pub commits: &'a CommitStore,
    /// Previously used revsets (or commands, for the command line), newest
    /// first. Offered when they extend what is typed.
    pub history: &'a [String],
}

/// What sits under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    /// A revset expression: functions, aliases, refs, change ids.
    Expression,
    /// A function argument of a known kind.
    Argument(Arg),
    /// Somewhere a name can't go (inside a string, after a complete term).
    Nothing,
}

#[derive(Debug)]
struct Frame {
    function: Option<String>,
    index: usize,
    keyword: Option<String>,
}

impl RevsetCompleter<'_> {
    /// Candidates for the revset `text` with the cursor at byte `cursor`.
    /// [`Completion::replace`] ranges are byte offsets into `text`.
    pub fn complete_revset(&self, text: &str, cursor: usize) -> Vec<Completion> {
        let cursor = floor_char_boundary(text, cursor.min(text.len()));
        let mut out = Candidates::new();

        // History first: a whole revset typed before, extending this one.
        let typed = &text[..cursor];
        if cursor == text.len() {
            for entry in self.history {
                if entry != text && entry.starts_with(typed) {
                    out.push(Completion {
                        replace: 0..text.len(),
                        insert: entry.clone(),
                        label: entry.clone(),
                        detail: "Recent".to_owned(),
                        kind: CompletionKind::History,
                    });
                }
            }
            out.truncate(5);
        }

        let (slot, token) = analyze(text, cursor);
        let prefix = &text[token.start..cursor];
        match slot {
            Slot::Nothing => {}
            Slot::Expression => self.expression(&mut out, token, prefix),
            Slot::Argument(arg) => self.argument(&mut out, arg, token, prefix),
        }
        out.finish()
    }

    fn expression(&self, out: &mut Candidates, token: Range<usize>, prefix: &str) {
        // `name@rem…`: a remote bookmark.
        if let Some((name, remote)) = prefix.split_once('@')
            && !name.is_empty()
        {
            for (bookmark, remote_name, title) in self.remote_bookmarks() {
                if bookmark == unquote(name) && remote_name.starts_with(remote) {
                    let insert =
                        format!("{}@{}", quote_symbol(bookmark), quote_symbol(remote_name));
                    out.push_match(token.clone(), insert, title, CompletionKind::RemoteBookmark);
                }
            }
            return;
        }

        // In this order: aliases (the user's first), bookmarks, functions,
        // tags, workspaces, remote bookmarks, then change ids.
        let mut add = |insert: String, label: String, detail: String, kind| {
            out.push(Completion {
                replace: token.clone(),
                insert,
                label,
                detail,
                kind,
            });
        };

        for alias in &self.symbols.aliases {
            if !alias.name.starts_with(prefix) {
                continue;
            }
            let insert = match &alias.params {
                None => alias.name.clone(),
                Some(params) if params.is_empty() => format!("{}()", alias.name),
                Some(_) => format!("{}(", alias.name),
            };
            let detail = builtin_alias_doc(alias)
                .map(str::to_owned)
                .unwrap_or_else(|| one_line(&alias.definition));
            add(insert, alias.signature(), detail, CompletionKind::Alias);
        }
        for (name, title) in self.local_bookmarks() {
            if name.starts_with(prefix) {
                add(
                    quote_symbol(name),
                    name.to_owned(),
                    title,
                    CompletionKind::Bookmark,
                );
            }
        }
        let mut functions: Vec<&Function> = FUNCTIONS
            .iter()
            .filter(|function| function.name.starts_with(prefix) && !prefix.is_empty())
            .collect();
        functions.sort_by_key(|function| function.name);
        for function in functions {
            let insert = if function.args.is_empty() {
                format!("{}()", function.name)
            } else {
                format!("{}(", function.name)
            };
            add(
                insert,
                format!("{}({})", function.name, function.params),
                function.doc.to_owned(),
                CompletionKind::Function,
            );
        }
        for (name, title) in self.tags() {
            if name.starts_with(prefix) {
                add(
                    quote_symbol(&name),
                    name.clone(),
                    title,
                    CompletionKind::Tag,
                );
            }
        }
        if self.symbols.workspaces.len() > 1 {
            for workspace in &self.symbols.workspaces {
                let symbol = format!("{}@", quote_symbol(workspace));
                if symbol.starts_with(prefix) && !prefix.is_empty() {
                    add(
                        symbol.clone(),
                        symbol,
                        format!("Working copy of workspace {workspace}"),
                        CompletionKind::Revision,
                    );
                }
            }
        }
        if !prefix.is_empty() {
            for (bookmark, remote, title) in self.remote_bookmarks() {
                let symbol = format!("{}@{}", quote_symbol(bookmark), quote_symbol(remote));
                if symbol.starts_with(prefix) {
                    add(
                        symbol.clone(),
                        symbol,
                        title,
                        CompletionKind::RemoteBookmark,
                    );
                }
            }
        }
        self.revisions(out, token.clone(), prefix, IdKind::Change);
        if is_hex(prefix) {
            self.revisions(out, token, prefix, IdKind::Commit);
        }
    }

    fn argument(&self, out: &mut Candidates, arg: Arg, token: Range<usize>, prefix: &str) {
        let name = unquote(prefix);
        match arg {
            Arg::Revset => self.expression(out, token, prefix),
            Arg::BookmarkName => {
                let mut names: Vec<(String, String)> = self
                    .local_bookmarks()
                    .map(|(name, title)| (name.to_owned(), title))
                    .collect();
                for (bookmark, _, title) in self.remote_bookmarks() {
                    if !names.iter().any(|(known, _)| known == bookmark) {
                        names.push((bookmark.to_owned(), title));
                    }
                }
                names.sort();
                for (bookmark, title) in names {
                    if bookmark.starts_with(name) {
                        out.push_match(
                            token.clone(),
                            quote_symbol(&bookmark),
                            title,
                            CompletionKind::Bookmark,
                        );
                    }
                }
            }
            Arg::TagName => {
                for (tag, title) in self.tags() {
                    if tag.starts_with(name) {
                        out.push_match(
                            token.clone(),
                            quote_symbol(&tag),
                            title,
                            CompletionKind::Tag,
                        );
                    }
                }
            }
            Arg::RemoteName => {
                for remote in &self.bookmarks.remotes {
                    if remote.starts_with(name) {
                        out.push_match(
                            token.clone(),
                            quote_symbol(remote),
                            "Remote".to_owned(),
                            CompletionKind::Remote,
                        );
                    }
                }
            }
            Arg::ChangeId => self.revisions(out, token, prefix, IdKind::Change),
            Arg::CommitId => self.revisions(out, token, prefix, IdKind::Commit),
            Arg::Free => {}
        }
    }

    /// Change ids (or commit ids) of loaded rows starting with `prefix`, the
    /// newest first. With nothing typed, the newest few rows.
    fn revisions(&self, out: &mut Candidates, token: Range<usize>, prefix: &str, kind: IdKind) {
        let alphabet_ok = match kind {
            IdKind::Change => prefix.bytes().all(|b| (b'k'..=b'z').contains(&b)),
            IdKind::Commit => is_hex(prefix) && !prefix.is_empty(),
        };
        if !alphabet_ok {
            return;
        }
        let cap = if prefix.is_empty() {
            RECENT_REVISIONS
        } else {
            MAX_REVISIONS
        };
        let mut taken = 0;
        for row in self.commits.iter() {
            if taken == cap {
                break;
            }
            let id = match kind {
                IdKind::Change => row.change_id(),
                IdKind::Commit => row.commit_id(),
            };
            if !id.starts_with(prefix) {
                continue;
            }
            let shortest = match kind {
                IdKind::Change => row.shortest_change_id_len().unwrap_or(8),
                IdKind::Commit => 12,
            };
            let len = shortest.max(prefix.len()).min(id.len());
            let mut insert = id[..floor_char_boundary(id, len)].to_owned();
            if kind == IdKind::Change
                && let Some(offset) = row.change_offset()
            {
                insert.push_str(&format!("/{offset}"));
            }
            let mut detail = row.description().to_owned();
            if row.is_working_copy() {
                detail = format!("@ {detail}");
            }
            out.push(Completion {
                replace: token.clone(),
                label: insert.clone(),
                insert,
                detail,
                kind: CompletionKind::Revision,
            });
            taken += 1;
        }
    }

    fn title_of(&self, commit_id: &str) -> String {
        self.commits
            .find_by_commit_id(commit_id)
            .map(|row| row.description().to_owned())
            .unwrap_or_default()
    }

    fn local_bookmarks(&self) -> impl Iterator<Item = (&str, String)> {
        self.bookmarks.bookmarks.iter().filter_map(|entry| {
            let target = entry.local_targets.first()?;
            Some((entry.name.as_str(), self.title_of(target)))
        })
    }

    fn remote_bookmarks(&self) -> impl Iterator<Item = (&str, &str, String)> {
        self.bookmarks.bookmarks.iter().flat_map(move |entry| {
            entry.remotes.iter().map(move |remote| {
                (
                    entry.name.as_str(),
                    remote.remote.as_str(),
                    self.title_of(&remote.target),
                )
            })
        })
    }

    /// Tag names (local ones; `v1@origin` labels name the same tag), sorted.
    fn tags(&self) -> Vec<(String, String)> {
        let mut tags: Vec<(String, String)> = Vec::new();
        for (commit, labels) in &self.bookmarks.tags {
            for label in labels {
                let name = label.trim_end_matches("??");
                if name.contains('@') || tags.iter().any(|(known, _)| known == name) {
                    continue;
                }
                tags.push((name.to_owned(), self.title_of(commit)));
            }
        }
        tags.sort();
        tags
    }

    /// Bookmark names for a command-line value: local ones, then names that
    /// only exist on a remote.
    fn bookmark_values(&self, text: &str, cursor: usize) -> Vec<Completion> {
        let prefix = &text[..cursor];
        let mut out = Candidates::new();
        let mut seen: Vec<&str> = Vec::new();
        for entry in &self.bookmarks.bookmarks {
            if entry.name.starts_with(prefix) && !seen.contains(&entry.name.as_str()) {
                seen.push(&entry.name);
                let target = entry
                    .local_targets
                    .first()
                    .or_else(|| entry.remotes.first().map(|remote| &remote.target));
                out.push(Completion {
                    replace: 0..text.len(),
                    insert: entry.name.clone(),
                    label: entry.name.clone(),
                    detail: target.map(|id| self.title_of(id)).unwrap_or_default(),
                    kind: CompletionKind::Bookmark,
                });
            }
        }
        out.finish()
    }
}

impl ValueCompleter for RevsetCompleter<'_> {
    fn complete(&self, kind: ValueKind, text: &str, cursor: usize) -> Vec<Completion> {
        let cursor = floor_char_boundary(text, cursor.min(text.len()));
        match kind {
            ValueKind::Revset => {
                // The command line hands over the token as typed, shell quote
                // and all: `'trunk()..ma`. The revset starts after it.
                let quote = text
                    .chars()
                    .next()
                    .filter(|c| matches!(c, '\'' | '"'))
                    .map_or(0, char::len_utf8);
                if cursor < quote {
                    return Vec::new();
                }
                let inner_end = if quote > 0 && text.len() > quote && text.ends_with(&text[..quote])
                {
                    text.len() - quote
                } else {
                    text.len()
                };
                if cursor > inner_end {
                    return Vec::new();
                }
                let inner = &text[quote..inner_end];
                self.complete_revset(inner, cursor - quote)
                    .into_iter()
                    .map(|mut completion| {
                        completion.replace =
                            completion.replace.start + quote..completion.replace.end + quote;
                        completion
                    })
                    .collect()
            }
            ValueKind::Bookmark => self.bookmark_values(text, cursor),
            ValueKind::Remote => {
                let prefix = &text[..cursor];
                self.bookmarks
                    .remotes
                    .iter()
                    .filter(|remote| remote.starts_with(prefix))
                    .map(|remote| Completion {
                        replace: 0..text.len(),
                        insert: remote.clone(),
                        label: remote.clone(),
                        detail: "Remote".to_owned(),
                        kind: CompletionKind::Remote,
                    })
                    .collect()
            }
            ValueKind::Path | ValueKind::Text | ValueKind::Other => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdKind {
    Change,
    Commit,
}

/// Collects candidates in order, dropping repeats and stopping at the cap.
struct Candidates {
    list: Vec<Completion>,
}

impl Candidates {
    fn new() -> Self {
        Self { list: Vec::new() }
    }

    fn push(&mut self, completion: Completion) {
        if self.list.len() < MAX_CANDIDATES
            && !self.list.iter().any(|known| {
                known.insert == completion.insert && known.replace == completion.replace
            })
        {
            self.list.push(completion);
        }
    }

    /// A name candidate inserted for the whole token.
    fn push_match(
        &mut self,
        replace: Range<usize>,
        insert: String,
        detail: String,
        kind: CompletionKind,
    ) {
        let label = insert.clone();
        self.push(Completion {
            replace,
            insert,
            label,
            detail,
            kind,
        });
    }

    fn truncate(&mut self, len: usize) {
        self.list.truncate(len);
    }

    fn finish(self) -> Vec<Completion> {
        self.list
    }
}

// ── reading the text around the cursor ──────────────────────────────────

/// Characters a revset symbol is made of, `@` included so `name@remote` is
/// one token. `.` and `-`/`+` sit inside names (`v1.2`, `feat-x`) and also
/// spell operators; the scan below tells the two apart.
fn is_symbol_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '*' | '/' | '.' | '-' | '+' | '@')
}

/// The slot the cursor is in and the byte range of the token under it (from
/// its start through any symbol characters after the cursor).
fn analyze(text: &str, cursor: usize) -> (Slot, Range<usize>) {
    let before = &text[..cursor];
    let mut frames: Vec<Frame> = Vec::new();
    let mut last_ident: Option<Range<usize>> = None;
    let mut chars = before.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => {
                let mut closed = false;
                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !closed {
                    return (Slot::Nothing, cursor..cursor);
                }
                last_ident = None;
            }
            '\'' => {
                if !chars.by_ref().any(|(_, c)| c == '\'') {
                    return (Slot::Nothing, cursor..cursor);
                }
                last_ident = None;
            }
            '(' => {
                let function = last_ident
                    .take()
                    .filter(|ident| ident.end == at)
                    .map(|ident| before[ident].to_owned());
                frames.push(Frame {
                    function,
                    index: 0,
                    keyword: None,
                });
            }
            ')' => {
                frames.pop();
                last_ident = None;
            }
            ',' => {
                if let Some(frame) = frames.last_mut() {
                    frame.index += 1;
                    frame.keyword = None;
                }
                last_ident = None;
            }
            '=' => {
                if let (Some(frame), Some(ident)) = (frames.last_mut(), last_ident.take()) {
                    frame.keyword = Some(before[ident].to_owned());
                }
            }
            c if is_symbol_char(c) => {
                let mut end = at + c.len_utf8();
                while let Some(&(next, c)) = chars.peek() {
                    if !is_symbol_char(c) {
                        break;
                    }
                    end = next + c.len_utf8();
                    chars.next();
                }
                last_ident = Some(at..end);
            }
            c if c.is_whitespace() => {}
            _ => last_ident = None,
        }
    }

    let start = token_start(before);
    let end = cursor
        + text[cursor..]
            .char_indices()
            .find(|&(_, c)| !is_symbol_char(c))
            .map_or(text.len() - cursor, |(at, _)| at);
    let token = start..end;

    // What comes before the token decides whether a name can go here.
    let lead = before[..start].trim_end();
    // `exact:ma`: a pattern's value, not the `::` operator.
    let pattern_value = lead.ends_with(':')
        && !lead.ends_with("::")
        && lead[..lead.len() - 1]
            .chars()
            .next_back()
            .is_some_and(is_symbol_char);
    let can_start_term = lead.is_empty()
        || pattern_value
        || lead.ends_with(['(', ',', '|', '&', '~', '='])
        || lead.ends_with("..")
        || lead.ends_with("::");
    if !can_start_term {
        return (Slot::Nothing, token);
    }

    let slot = match frames.last() {
        Some(Frame {
            function: Some(name),
            index,
            keyword,
        }) => match builtin(name) {
            Some(function) => {
                let arg = if keyword.as_deref() == Some("remote") && function.remote_keyword {
                    Arg::RemoteName
                } else if keyword.is_some() || function.args.is_empty() {
                    Arg::Free
                } else {
                    function.args[(*index).min(function.args.len() - 1)]
                };
                Slot::Argument(arg)
            }
            // A function alias's parameters are revsets as far as we know.
            None => Slot::Argument(Arg::Revset),
        },
        _ if pattern_value => Slot::Nothing,
        _ => Slot::Expression,
    };
    (slot, token)
}

/// Where the symbol ending at the end of `before` starts. A `..` inside the
/// run is the range operator, so `trunk()..ma` yields `ma`.
fn token_start(before: &str) -> usize {
    let run = before
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_symbol_char(c))
        .last()
        .map_or(before.len(), |(at, _)| at);
    match before[run..].rfind("..") {
        Some(range) => run + range + 2,
        None => run,
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn is_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `name` as a revset symbol: bare when jj's grammar reads it as one
/// identifier, a string literal otherwise.
pub fn quote_symbol(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        let mut quoted = String::with_capacity(name.len() + 2);
        quoted.push('"');
        for c in name.chars() {
            match c {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                c => quoted.push(c),
            }
        }
        quoted.push('"');
        quoted
    }
}

/// jj's `identifier` rule: parts of `XID_CONTINUE | _ | * | /`, joined by
/// `.`, runs of `-`, or `+`.
fn is_identifier(name: &str) -> bool {
    let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | '*' | '/');
    let mut previous_part = false;
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if part(c) {
            previous_part = true;
        } else if matches!(c, '.' | '-' | '+') && previous_part {
            if c == '-' {
                while chars.peek() == Some(&'-') {
                    chars.next();
                }
            }
            if !chars.peek().copied().is_some_and(part) {
                return false;
            }
            previous_part = false;
        } else {
            return false;
        }
    }
    previous_part
}

/// A typed symbol without the string quotes it may have been started with.
fn unquote(symbol: &str) -> &str {
    symbol.trim_start_matches(['"', '\''])
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 80;
    if joined.chars().count() > MAX {
        let cut: String = joined.chars().take(MAX - 1).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookmarkEntry, CommitStoreBuilder, CommitSummary, RemoteBookmarkRef};

    fn summary(change: &str, commit: &str, title: &str, wc: bool) -> CommitSummary {
        CommitSummary {
            change_id: change.to_owned(),
            commit_id: commit.to_owned(),
            shortest_change_id_len: Some(2),
            description: title.to_owned(),
            author: "a".to_owned(),
            has_description: true,
            is_empty: None,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: wc,
            is_immutable: false,
            bookmarks: Vec::new(),
            parent_ids: Vec::new(),
        }
    }

    struct Fixture {
        symbols: RevsetSymbols,
        bookmarks: BookmarksInfo,
        commits: CommitStore,
        history: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            let mut builder = CommitStoreBuilder::with_capacity(3);
            builder.push(summary("kxqpmnop", "aa11", "Working on it", true));
            builder.push(summary("ksabcdef", "bb22", "Add the parser", false));
            builder.push(summary("zzzzzzzz", "0000", "", false));
            let bookmarks = BookmarksInfo {
                bookmarks: vec![
                    BookmarkEntry {
                        name: "main".to_owned(),
                        local_targets: vec!["bb22".to_owned()],
                        remotes: vec![RemoteBookmarkRef {
                            remote: "origin".to_owned(),
                            target: "bb22".to_owned(),
                            tracked: true,
                        }],
                    },
                    BookmarkEntry {
                        name: "feat x".to_owned(),
                        local_targets: vec!["aa11".to_owned()],
                        remotes: Vec::new(),
                    },
                    BookmarkEntry {
                        name: "mine-only-remote".to_owned(),
                        local_targets: Vec::new(),
                        remotes: vec![RemoteBookmarkRef {
                            remote: "upstream".to_owned(),
                            target: "aa11".to_owned(),
                            tracked: false,
                        }],
                    },
                ],
                remotes: vec!["origin".to_owned(), "upstream".to_owned()],
                tags: [("bb22".to_owned(), vec!["v1.0".to_owned()])]
                    .into_iter()
                    .collect(),
                ..BookmarksInfo::default()
            };
            Self {
                symbols: RevsetSymbols {
                    aliases: vec![
                        RevsetAlias {
                            name: "trunk".to_owned(),
                            params: Some(Vec::new()),
                            definition: "latest(\n  remote_bookmarks(...)\n)".to_owned(),
                        },
                        RevsetAlias {
                            name: "stack".to_owned(),
                            params: Some(vec!["x".to_owned(), "n".to_owned()]),
                            definition: "ancestors(x, n) & mutable()".to_owned(),
                        },
                        RevsetAlias {
                            name: "wip".to_owned(),
                            params: None,
                            definition: "description(wip)".to_owned(),
                        },
                    ],
                    workspaces: vec!["default".to_owned(), "side".to_owned()],
                },
                bookmarks,
                commits: builder.finish(),
                history: vec!["trunk()..@".to_owned(), "mine() & ::@".to_owned()],
            }
        }

        fn completer(&self) -> RevsetCompleter<'_> {
            RevsetCompleter {
                symbols: &self.symbols,
                bookmarks: &self.bookmarks,
                commits: &self.commits,
                history: &self.history,
            }
        }

        fn inserts(&self, text: &str, cursor: usize) -> Vec<String> {
            self.completer()
                .complete_revset(text, cursor)
                .into_iter()
                .map(|completion| completion.insert)
                .collect()
        }
    }

    fn at_end(fixture: &Fixture, text: &str) -> Vec<String> {
        fixture.inserts(text, text.len())
    }

    #[test]
    fn functions_carry_their_signature() {
        let fixture = Fixture::new();
        let found = fixture.completer().complete_revset("anc", 3);
        let ancestors = found
            .iter()
            .find(|completion| completion.insert == "ancestors(")
            .expect("ancestors completes");
        assert_eq!(ancestors.label, "ancestors(x, [depth])");
        assert_eq!(ancestors.kind, CompletionKind::Function);
        assert_eq!(ancestors.replace, 0..3);
        let all = fixture.completer().complete_revset("al", 2);
        assert!(all.iter().any(|completion| completion.insert == "all()"));
    }

    #[test]
    fn aliases_complete_with_parameters() {
        let fixture = Fixture::new();
        let found = fixture.completer().complete_revset("st", 2);
        let stack = found
            .iter()
            .find(|completion| completion.kind == CompletionKind::Alias)
            .expect("the function alias completes");
        assert_eq!(stack.insert, "stack(");
        assert_eq!(stack.label, "stack(x, n)");
        assert_eq!(stack.detail, "ancestors(x, n) & mutable()");
        let found = fixture.completer().complete_revset("tru", 3);
        let trunk = found
            .iter()
            .find(|completion| completion.kind == CompletionKind::Alias)
            .expect("trunk() completes");
        assert_eq!(trunk.insert, "trunk()");
        assert_eq!(
            trunk.detail,
            "The default bookmark's head on origin or upstream"
        );
        assert_eq!(at_end(&fixture, "wi"), ["wip"]);
    }

    #[test]
    fn symbols_complete_after_operators() {
        let fixture = Fixture::new();
        for text in [
            "trunk()..ma",
            "@ | ma",
            "x & ma",
            "~ma",
            "::ma",
            "ma",
            "(ma",
        ] {
            let found = fixture.completer().complete_revset(text, text.len());
            let main = found
                .iter()
                .find(|completion| completion.insert == "main")
                .unwrap_or_else(|| panic!("`main` completes in {text:?}: {found:?}"));
            assert_eq!(main.replace, text.len() - 2..text.len(), "{text:?}");
            assert_eq!(main.detail, "Add the parser");
        }
    }

    #[test]
    fn nothing_inside_strings_or_after_a_term() {
        let fixture = Fixture::new();
        assert!(at_end(&fixture, "description(\"ma").is_empty());
        assert!(at_end(&fixture, "'ma").is_empty());
        assert!(at_end(&fixture, "trunk() ma").is_empty());
    }

    #[test]
    fn quoted_strings_before_the_cursor_are_skipped() {
        let fixture = Fixture::new();
        let found = at_end(&fixture, "description(\"a,b(\") | ma");
        assert!(found.contains(&"main".to_owned()), "{found:?}");
    }

    #[test]
    fn function_arguments_complete_by_kind() {
        let fixture = Fixture::new();
        assert_eq!(at_end(&fixture, "bookmarks(ma"), ["main"]);
        assert_eq!(at_end(&fixture, "bookmarks(exact:ma"), ["main"]);
        assert_eq!(at_end(&fixture, "bookmarks(fe"), ["\"feat x\""]);
        assert_eq!(
            at_end(&fixture, "remote_bookmarks(main, "),
            ["origin", "upstream"]
        );
        assert_eq!(at_end(&fixture, "remote_bookmarks(remote=up"), ["upstream"]);
        assert_eq!(at_end(&fixture, "tags(v"), ["v1.0"]);
        assert!(at_end(&fixture, "author(ma").is_empty());
        assert!(at_end(&fixture, "ancestors(@, ").is_empty());
        assert!(at_end(&fixture, "ancestors(ma").contains(&"main".to_owned()));
        assert!(at_end(&fixture, "stack(ma").contains(&"main".to_owned()));
        assert_eq!(at_end(&fixture, "commit_id(b"), ["bb22"]);
    }

    #[test]
    fn remote_bookmarks_and_workspaces() {
        let fixture = Fixture::new();
        assert_eq!(at_end(&fixture, "main@o"), ["main@origin"]);
        let found = at_end(&fixture, "mine-o");
        assert!(
            found.contains(&"mine-only-remote@upstream".to_owned()),
            "{found:?}"
        );
        assert!(at_end(&fixture, "si").contains(&"side@".to_owned()));
    }

    #[test]
    fn change_ids_complete_with_titles() {
        let fixture = Fixture::new();
        let found = fixture.completer().complete_revset("trunk()..ks", 11);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].insert, "ks");
        assert_eq!(found[0].detail, "Add the parser");
        assert_eq!(found[0].kind, CompletionKind::Revision);
        let working_copy = fixture.completer().complete_revset("kxq", 3);
        assert_eq!(working_copy[0].insert, "kxq");
        assert_eq!(working_copy[0].detail, "@ Working on it");
    }

    #[test]
    fn history_extends_what_is_typed() {
        let fixture = Fixture::new();
        let found = fixture.completer().complete_revset("tr", 2);
        assert_eq!(found[0].insert, "trunk()..@");
        assert_eq!(found[0].replace, 0..2);
        assert_eq!(found[0].kind, CompletionKind::History);
        let empty = fixture.completer().complete_revset("", 0);
        assert_eq!(empty[0].insert, "trunk()..@");
        assert_eq!(empty[1].insert, "mine() & ::@");
    }

    #[test]
    fn token_under_the_cursor_is_replaced_whole() {
        let fixture = Fixture::new();
        let found = fixture.completer().complete_revset("ma | @", 1);
        let main = found
            .iter()
            .find(|completion| completion.insert == "main")
            .expect("main completes mid-token");
        assert_eq!(main.replace, 0..2);
    }

    #[test]
    fn command_line_values_strip_the_shell_quote() {
        let fixture = Fixture::new();
        let completer = fixture.completer();
        let found = completer.complete(ValueKind::Revset, "'trunk()..ma", 12);
        let main = found
            .iter()
            .find(|completion| completion.insert == "main")
            .expect("main completes inside a shell quote");
        assert_eq!(main.replace, 10..12);
        let closed = completer.complete(ValueKind::Revset, "'ma'", 3);
        assert!(closed.iter().any(|completion| completion.replace == (1..3)));
        assert_eq!(
            completer
                .complete(ValueKind::Bookmark, "mi", 2)
                .into_iter()
                .map(|completion| completion.insert)
                .collect::<Vec<_>>(),
            ["mine-only-remote"]
        );
        assert_eq!(
            completer
                .complete(ValueKind::Remote, "o", 1)
                .into_iter()
                .map(|completion| completion.insert)
                .collect::<Vec<_>>(),
            ["origin"]
        );
    }

    #[test]
    fn symbols_quote_when_the_grammar_needs_it() {
        assert_eq!(quote_symbol("main"), "main");
        assert_eq!(quote_symbol("feat/x-1.2"), "feat/x-1.2");
        assert_eq!(quote_symbol("a--b"), "a--b");
        assert_eq!(quote_symbol("feat x"), "\"feat x\"");
        assert_eq!(quote_symbol("trailing-"), "\"trailing-\"");
        assert_eq!(quote_symbol("a..b"), "\"a..b\"");
        assert_eq!(quote_symbol("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

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
