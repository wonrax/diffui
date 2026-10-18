//! `jj git push` as data: what a push sends ([`PushRequest`]), which of the
//! CLI's commit checks it may skip ([`PushAllow`]), and why a push was refused
//! ([`PushRejection`]). The execution lives in `jj::push`; these types are what
//! a frontend builds and matches on.

use crate::model::RevisionSelection;

/// What one push sends, mirroring `jj git push`'s target flags. Every target
/// lands in the same transaction as the push itself, so a refused or failed
/// push leaves no half-made local bookmark behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushTarget {
    /// `-b <name>`: the local bookmark as it stands. A bookmark that doesn't
    /// track this remote yet is created there (and tracked from then on); a
    /// deleted local bookmark that still tracks the remote is deleted there.
    Bookmark(String),
    /// `jj bookmark set <name> -r <to>` then `-b <name>`.
    SetBookmark { name: String, to: RevisionSelection },
    /// `jj bookmark delete <name>` then `-b <name>`: delete the bookmark here
    /// and on the remote.
    DeleteBookmark(String),
    /// `-c <revision>`: create the bookmark `templates.git_push_bookmark`
    /// names for the revision and push it.
    Change(RevisionSelection),
    /// No target flags at all: the tracked bookmarks whose local target sits
    /// in `remote_bookmarks(remote=<remote>)..@` and differs from the remote.
    Tracked,
}

/// The CLI's escape hatches for the per-commit checks. Missing author or
/// committer identity has none, so neither does this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushAllow {
    /// `--allow-empty-description`
    pub empty_description: bool,
    /// `--allow-private` (commits in `git.private-commits`)
    pub private: bool,
    /// `--allow-conflicts`
    pub conflicts: bool,
}

impl PushAllow {
    /// The CLI flags these allowances stand for, for dialogs and summaries.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if self.empty_description {
            flags.push("--allow-empty-description");
        }
        if self.private {
            flags.push("--allow-private");
        }
        if self.conflicts {
            flags.push("--allow-conflicts");
        }
        flags
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    pub remote: String,
    pub targets: Vec<PushTarget>,
    pub allow: PushAllow,
    /// Stop before pushing and hand back the plan as a [`PushRejection`]
    /// with no offending commits — a confirmation step ("these will be
    /// pushed") for pushes whose targets the user didn't name one by one.
    pub confirm: bool,
}

/// One reason `jj git push` refuses a commit it would publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushCheck {
    EmptyDescription,
    /// Author or committer has no name or email.
    MissingIdentity,
    Conflicts,
    /// Matched by `git.private-commits`.
    Private,
}

impl PushCheck {
    /// The CLI's wording ("Won't push commit X since it has no description").
    pub fn describe(self) -> &'static str {
        match self {
            PushCheck::EmptyDescription => "has no description",
            PushCheck::MissingIdentity => "has no author and/or committer set",
            PushCheck::Conflicts => "has conflicts",
            PushCheck::Private => "is private",
        }
    }
}

/// A commit the push would have made newly reachable on the remote, and why
/// it may not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedCommit {
    /// Short change id, as the graph shows it.
    pub change_id: String,
    /// Short commit hash, as the CLI's message prints it.
    pub commit_id: String,
    /// First line of the description (empty when there is none).
    pub subject: String,
    pub reasons: Vec<PushCheck>,
    /// Whether this is the pushing workspace's working-copy commit.
    pub working_copy: bool,
}

/// Why a push stopped before anything reached the remote: commits jj's checks
/// refuse, or — with no commits listed — a [`PushRequest::confirm`] asking for
/// the go-ahead on `plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRejection {
    pub remote: String,
    /// What the push would do, one line per bookmark, as the CLI prints
    /// "Changes to push to <remote>:" (`bookmark: main [move forward from
    /// … to …]`).
    pub plan: Vec<String>,
    pub commits: Vec<RejectedCommit>,
    /// More offending commits exist than are listed.
    pub truncated: bool,
    /// The configured `git.private-commits`, when a commit was refused for
    /// matching it.
    pub private_commits: Option<String>,
    /// The request that was refused, so the frontend can rerun it with the
    /// allowances the user accepts.
    pub request: PushRequest,
}

impl PushRejection {
    /// Only a confirmation: nothing was refused.
    pub fn is_confirmation(&self) -> bool {
        self.commits.is_empty()
    }

    /// The allowances that would let this push through, or `None` when some
    /// commit fails a check the CLI has no escape hatch for.
    pub fn allowances(&self) -> Option<PushAllow> {
        let mut allow = self.request.allow;
        for commit in &self.commits {
            for reason in &commit.reasons {
                match reason {
                    PushCheck::EmptyDescription => allow.empty_description = true,
                    PushCheck::Private => allow.private = true,
                    PushCheck::Conflicts => allow.conflicts = true,
                    PushCheck::MissingIdentity => return None,
                }
            }
        }
        Some(allow)
    }

    /// One line per offending commit, in the CLI's words.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .commits
            .iter()
            .map(|commit| {
                let reasons: Vec<&str> = commit.reasons.iter().map(|r| r.describe()).collect();
                let subject = if commit.subject.is_empty() {
                    "(no description set)"
                } else {
                    commit.subject.as_str()
                };
                let wc = if commit.working_copy {
                    " (working copy)"
                } else {
                    ""
                };
                format!(
                    "{} {}{wc} {}: {}",
                    commit.change_id,
                    commit.commit_id,
                    subject,
                    reasons.join(" and ")
                )
            })
            .collect();
        if self.truncated {
            lines.push("\u{2026} and more".to_owned());
        }
        if let Some(revset) = &self.private_commits {
            lines.push(format!("Configured git.private-commits: '{revset}'"));
        }
        lines
    }
}

impl std::fmt::Display for PushRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_confirmation() {
            return write!(
                f,
                "push to {} awaits confirmation: {}",
                self.remote,
                self.plan.join("; ")
            );
        }
        write!(
            f,
            "won't push to {}: {}",
            self.remote,
            self.lines().join("; ")
        )
    }
}

impl std::error::Error for PushRejection {}
