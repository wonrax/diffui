//! History views over a jj repository: the operation log (`jj op log` /
//! `jj op show`), a change's evolution (`jj evolog`), and per-line blame
//! (`jj file annotate`).
//!
//! Plain owned data, like the rest of the protocol: the jj-lib side lives in
//! [`crate::jj::history`] and runs on the repository actor.

use crate::model::{DiffDocument, RevisionDetails, RevisionSelection};

/// Work a history view asks of the repository actor. Rides in
/// [`Command::History`](crate::repo::Command::History) so the whole family
/// costs the protocol one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRequest {
    /// Stream up to `limit` operations, newest first, as
    /// [`Payload::OpLogBatch`](crate::repo::Payload::OpLogBatch) events.
    OpLog { limit: usize },
    /// What one operation changed, compared with its parent (`jj op show`).
    ShowOperation { operation_id: String },
    /// The previous versions of `revision`'s change (`jj evolog`).
    Evolog { revision: RevisionSelection },
    /// A diff of a commit that may no longer be visible — an evolog entry, or
    /// a commit an operation rewrote or abandoned.
    Diff { target: HistoryDiffTarget },
    /// Which change last touched each line of `path` at `revision`
    /// (`jj file annotate`).
    Annotate {
        revision: RevisionSelection,
        path: String,
    },
}

/// What a [`HistoryRequest::Diff`] compares. Commit ids are full hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HistoryDiffTarget {
    /// The commit against its parents, like `jj show`.
    Commit { commit_id: String },
    /// `to` against its predecessors `from`, with the predecessors first
    /// rebased onto `to`'s parents so a rebase doesn't show up as content
    /// (`jj evolog -p`, `jj op show -p`).
    InterDiff { from: Vec<String>, to: String },
}

/// The terminal answer to a [`HistoryRequest`].
#[derive(Debug, Clone)]
pub enum HistoryResponse {
    /// The op-log stream finished. `truncated` when it stopped at the limit
    /// with older operations left unread.
    OpLogDone {
        truncated: bool,
    },
    Operation(OperationChanges),
    Evolog(Vec<EvologEntry>),
    Diff {
        target: HistoryDiffTarget,
        document: DiffDocument,
        details: Option<RevisionDetails>,
    },
    Annotation(FileAnnotation),
}

/// One operation in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationEntry {
    /// Full hex id.
    pub id: String,
    pub parent_ids: Vec<String>,
    /// The whole description; the first line is the summary.
    pub description: String,
    /// `user@host`, as `jj op log` shows it.
    pub user: String,
    /// When the operation finished, in milliseconds since the epoch.
    pub time_millis: i64,
    /// The same instant in the operation's own offset, for a tooltip/header.
    pub time: String,
    /// A pure working-copy snapshot. These dominate the log of a repository
    /// the app keeps open, so the view folds them away.
    pub is_snapshot: bool,
    /// The operation the repository is currently at.
    pub is_current: bool,
    /// The command line that wrote it, when jj recorded one (`args` tag).
    pub args: Option<String>,
}

impl OperationEntry {
    /// The id prefix `jj op log` prints.
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }

    pub fn summary(&self) -> &str {
        self.description.lines().next().unwrap_or("")
    }
}

/// A commit as a history view lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitBrief {
    pub commit_id: String,
    pub change_id: String,
    /// First line of the description; empty when there is none.
    pub summary: String,
    pub author: String,
    /// Committer timestamp, milliseconds since the epoch.
    pub time_millis: i64,
    pub empty: bool,
    /// Whether the commit is still visible at the repository's current head.
    /// Only a visible commit can be selected in the graph.
    pub visible: bool,
}

impl CommitBrief {
    pub fn short_commit_id(&self) -> &str {
        &self.commit_id[..self.commit_id.len().min(8)]
    }

    pub fn short_change_id(&self) -> &str {
        &self.change_id[..self.change_id.len().min(8)]
    }
}

/// How an operation touched one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitChangeKind {
    /// New, with no predecessor.
    Created,
    /// A new version of an existing change.
    Rewritten,
    /// Hidden, with no successor.
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedCommit {
    pub kind: CommitChangeKind,
    /// The new version, or the abandoned one.
    pub commit: CommitBrief,
    /// What it replaced, first predecessor first. Empty unless `Rewritten`.
    pub predecessors: Vec<CommitBrief>,
    /// A rewrite that only moved the commit onto new parents — the
    /// descendants an edit drags along — rather than one the operation
    /// was about.
    pub rebased: bool,
}

impl ChangedCommit {
    /// The diff that best explains this change: the interdiff against the
    /// first predecessor for a rewrite (`jj op show -p` does the same), the
    /// commit's own patch otherwise.
    pub fn diff_target(&self) -> HistoryDiffTarget {
        match self.predecessors.first() {
            Some(previous) => HistoryDiffTarget::InterDiff {
                from: vec![previous.commit_id.clone()],
                to: self.commit.commit_id.clone(),
            },
            None => HistoryDiffTarget::Commit {
                commit_id: self.commit.commit_id.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    WorkingCopy,
    Bookmark,
    RemoteBookmark,
    Tag,
}

/// A named pointer an operation moved. `from`/`to` list the commits on each
/// side — empty means absent, more than one means conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    pub kind: RefKind,
    /// `name`, `name@remote`, or `workspace@` for a working copy.
    pub name: String,
    pub from: Vec<CommitBrief>,
    pub to: Vec<CommitBrief>,
}

/// What [`HistoryRequest::ShowOperation`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationChanges {
    pub operation: OperationEntry,
    /// Newest first, as `jj op show` lists them.
    pub commits: Vec<ChangedCommit>,
    /// More commits changed than are listed.
    pub commits_truncated: bool,
    pub refs: Vec<RefChange>,
    /// A merge of concurrent operations: like the CLI, no diff is computed
    /// against several parents.
    pub is_merge: bool,
}

/// One version of a change, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvologEntry {
    pub commit: CommitBrief,
    /// The operation that wrote this version, when the op log still records it.
    pub operation: Option<OperationRef>,
    /// The versions it replaced (full hex). More than one after a squash.
    pub predecessor_ids: Vec<String>,
}

impl EvologEntry {
    pub fn interdiff_target(&self) -> Option<HistoryDiffTarget> {
        (!self.predecessor_ids.is_empty()).then(|| HistoryDiffTarget::InterDiff {
            from: self.predecessor_ids.clone(),
            to: self.commit.commit_id.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRef {
    pub id: String,
    pub summary: String,
    pub time_millis: i64,
}

impl OperationRef {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}

/// Per-line origins of a file, `jj file annotate`-style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAnnotation {
    pub revision: RevisionSelection,
    pub path: String,
    /// Index into `origins` for every line of the file at `revision`.
    pub lines: Vec<u32>,
    /// The distinct commits lines came from.
    pub origins: Vec<CommitBrief>,
}

/// A coarse "how long ago" for a timestamp, the way a log column shows it:
/// `now`, `5m`, `3h`, `2d`, `6w`, `4mo`, `2y`.
pub fn relative_age(then_millis: i64, now_millis: i64) -> String {
    let seconds = (now_millis - then_millis).max(0) / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "now".to_owned()
    } else if hours < 1 {
        format!("{minutes}m")
    } else if days < 1 {
        format!("{hours}h")
    } else if days < 14 {
        format!("{days}d")
    } else if days < 60 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// Milliseconds since the epoch of a signature timestamp as the backends
/// render it: `2026-10-18 23:29:51 +0700` (jj, git) or RFC 3339 (GitHub).
pub fn parse_timestamp_millis(timestamp: &str) -> Option<i64> {
    let timestamp = timestamp.trim();
    chrono::DateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S %z")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(timestamp))
        .ok()
        .map(|time| time.timestamp_millis())
}

/// A signature timestamp for people, in the offset it was made in:
/// `Oct 19, 2026, 15:48`. `None` when it doesn't parse.
pub fn human_timestamp(timestamp: &str) -> Option<String> {
    let timestamp = timestamp.trim();
    chrono::DateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S %z")
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(timestamp))
        .ok()
        .map(|time| time.format("%b %-d, %Y, %H:%M").to_string())
}

/// Milliseconds since the epoch, for [`relative_age`].
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The revset for "revisions that touched `path`, up to `revision`": `jj log
/// <path>` scoped to the revision's ancestry, so a file's history reads as of
/// the commit it was opened from. `revision_symbol` is `@` or a commit id.
pub fn file_history_revset(revision_symbol: &str, path: &str) -> String {
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for ch in path.chars() {
        match ch {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    format!("::{revision_symbol} & files(root:{quoted})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_timestamps_parse_from_every_backend() {
        // 2026-10-18 16:29:51 UTC.
        let utc = 1_792_340_991_000;
        assert_eq!(
            parse_timestamp_millis("2026-10-18 23:29:51 +0700"),
            Some(utc)
        );
        assert_eq!(parse_timestamp_millis("2026-10-18T16:29:51Z"), Some(utc));
        assert_eq!(
            parse_timestamp_millis("2026-10-18T09:29:51-07:00"),
            Some(utc)
        );
        assert_eq!(parse_timestamp_millis("yesterday"), None);
        assert_eq!(
            human_timestamp("2026-10-08 23:29:51 +0700").as_deref(),
            Some("Oct 8, 2026, 23:29")
        );
        assert_eq!(
            human_timestamp("2026-10-18T16:29:51Z").as_deref(),
            Some("Oct 18, 2026, 16:29")
        );
        assert_eq!(human_timestamp("yesterday"), None);
    }

    #[test]
    fn ages_round_down_to_the_coarsest_unit() {
        let now = 10_000_000_000;
        assert_eq!(relative_age(now - 30_000, now), "now");
        assert_eq!(relative_age(now - 5 * 60_000, now), "5m");
        assert_eq!(relative_age(now - 3 * 3_600_000, now), "3h");
        assert_eq!(relative_age(now - 2 * 86_400_000, now), "2d");
        assert_eq!(relative_age(now - 21 * 86_400_000, now), "3w");
        assert_eq!(relative_age(now - 120 * 86_400_000, now), "4mo");
        assert_eq!(relative_age(now - 800 * 86_400_000, now), "2y");
        // A clock skewed into the future reads as "now", not a negative age.
        assert_eq!(relative_age(now + 60_000, now), "now");
    }

    #[test]
    fn file_history_quotes_the_path() {
        assert_eq!(
            file_history_revset("@", "src/main.rs"),
            r#"::@ & files(root:"src/main.rs")"#
        );
        assert_eq!(
            file_history_revset("abc", r#"we"ird\name"#),
            r#"::abc & files(root:"we\"ird\\name")"#
        );
    }
}
