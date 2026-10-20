//! What an operation will do, said two ways: in words ("Rebase xyz and 2
//! descendants onto main") and as the jj command that does the same
//! (`jj rebase -s xyz -o abc`).
//!
//! The command comes from [`MutationOp::cli_equivalent`]; this module names
//! the revisions in it the way the rest of diffui does, and builds the
//! `args` tag diffui records on its own operations so `jj op log` in a
//! terminal shows what it ran.

use crate::cli::{CliEquivalent, RevisionNames, jj_args_tag};
use crate::model::{CommitStore, RevisionSelection};
use crate::mutations::{Destination, MutationOp, RebaseSourceMode, SquashTarget};
use crate::repository::FetchTarget;

/// How many characters of a change id a command spells out: jj's short
/// form, the width diffui shows ids at everywhere else.
pub const NAME_CHARS: usize = 8;

/// A change id as a command names it: [`NAME_CHARS`] long, or longer when
/// that many aren't unique (`shortest` is the shortest unique prefix).
pub fn change_id_name(change_id: &str, shortest: Option<usize>) -> String {
    let len = shortest.unwrap_or(0).max(NAME_CHARS);
    change_id.chars().take(len).collect()
}

/// The loaded graph names its rows by change id. A row whose change id
/// doesn't single it out (a divergent change, a hidden commit) isn't named,
/// so the command falls back to its commit id.
impl RevisionNames for CommitStore {
    fn change_id(&self, commit_id: &str) -> Option<String> {
        let row = self.find_by_commit_id(commit_id)?;
        (!row.is_hidden() && !row.is_divergent())
            .then(|| change_id_name(row.change_id(), row.shortest_change_id_len()))
    }

    fn is_divergent(&self, commit_id: &str) -> bool {
        self.find_by_commit_id(commit_id)
            .is_some_and(|row| row.is_divergent())
    }
}

/// `equivalent` run with the immutable guards off: jj's
/// `--ignore-immutable` on each command.
pub fn ignoring_immutable(mut equivalent: CliEquivalent) -> CliEquivalent {
    for command in &mut equivalent.commands {
        command.push("--ignore-immutable".to_owned());
    }
    equivalent
}

/// The `args` tag for an operation that ran `equivalent`: each command as
/// jj-cli records its own, joined with `&&` when the op did two things at
/// once (move a bookmark, then push it).
pub fn args_tag(equivalent: &CliEquivalent) -> String {
    equivalent
        .commands
        .iter()
        .map(|args| jj_args_tag(args))
        .collect::<Vec<_>>()
        .join(" && ")
}

/// `jj git fetch` for a fetch target.
pub fn fetch_command(target: &FetchTarget) -> CliEquivalent {
    let args = match target {
        FetchTarget::AllRemotes => vec!["git", "fetch", "--all-remotes"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        FetchTarget::RemoteBranch { remote, branch } => vec![
            "git".to_owned(),
            "fetch".to_owned(),
            "--remote".to_owned(),
            remote.clone(),
            "--branch".to_owned(),
            branch.clone(),
        ],
    };
    CliEquivalent {
        commands: vec![args],
        exact: true,
    }
}

/// A run of a plan sentence: prose, or a name that renders as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phrase {
    Text(String),
    /// A change id, commit id or `@`.
    Revision(String),
    Bookmark(String),
}

/// How the loaded graph names a revision in words: by its first bookmark
/// when it has one ("onto main"), else by change id, else by commit id.
pub fn graph_phrase(store: &CommitStore, revision: &RevisionSelection) -> Phrase {
    let hex = match revision {
        RevisionSelection::WorkingCopy => return Phrase::Revision("@".to_owned()),
        RevisionSelection::Commit(hex) => hex,
    };
    match store.find_by_commit_id(hex) {
        Some(row) => match row.bookmarks().first() {
            Some(bookmark) => Phrase::Bookmark(bookmark.clone()),
            None => Phrase::Revision(
                store
                    .change_id(hex)
                    .unwrap_or_else(|| hex.chars().take(12).collect()),
            ),
        },
        None => Phrase::Revision(hex.chars().take(12).collect()),
    }
}

/// The draft's op in words, e.g. "Rebase xyz and 2 descendants onto main".
/// `descendants` is how many descendants a rebase brings along, once a
/// simulation has counted them. `None` for an op target mode never builds.
pub fn sentence(
    op: &MutationOp,
    name: &dyn Fn(&RevisionSelection) -> Phrase,
    descendants: Option<usize>,
) -> Option<Vec<Phrase>> {
    let mut out = Sentence {
        phrases: Vec::new(),
        name,
    };
    match op {
        MutationOp::Rebase {
            mode,
            sources,
            destination,
        } => {
            match mode {
                RebaseSourceMode::Revisions => {
                    out.text("Rebase ");
                    out.list(sources);
                }
                RebaseSourceMode::WithDescendants => {
                    out.text("Rebase ");
                    out.list(sources);
                    match descendants {
                        Some(0) => {}
                        Some(1) => out.text(" and 1 descendant"),
                        Some(count) => out.text(&format!(" and {count} descendants")),
                        None => out.text(" and descendants"),
                    }
                }
                RebaseSourceMode::Branch => {
                    out.text("Rebase the branch of ");
                    out.list(sources);
                }
            }
            out.destination(destination, " onto ");
        }
        MutationOp::Squash { from, into } => {
            out.text("Squash ");
            out.list(from);
            out.squash_target(into);
        }
        MutationOp::SquashChanges { from, into, .. } => {
            out.text("Move the picked changes from ");
            out.list(std::slice::from_ref(from));
            out.squash_target(into);
        }
        MutationOp::Merge { parents } => {
            out.text("Create a merge of ");
            out.list(parents);
        }
        MutationOp::New { parent } => {
            out.text("Create a change on top of ");
            out.list(std::slice::from_ref(parent));
        }
        MutationOp::NewAt { destination } => {
            out.text("Create a change");
            out.destination(destination, " on top of ");
        }
        MutationOp::Duplicate { target } => {
            out.text("Duplicate ");
            out.list(std::slice::from_ref(target));
        }
        MutationOp::DuplicateTo {
            targets,
            destination,
        } => {
            out.text("Duplicate ");
            out.list(targets);
            if let Some(destination) = destination {
                out.destination(destination, " onto ");
            }
        }
        MutationOp::Parallelize { targets } => {
            out.text("Make ");
            out.list(targets);
            out.text(" siblings");
        }
        _ => return None,
    }
    Some(out.phrases)
}

struct Sentence<'a> {
    phrases: Vec<Phrase>,
    name: &'a dyn Fn(&RevisionSelection) -> Phrase,
}

impl Sentence<'_> {
    fn text(&mut self, text: &str) {
        match self.phrases.last_mut() {
            Some(Phrase::Text(run)) => run.push_str(text),
            _ => self.phrases.push(Phrase::Text(text.to_owned())),
        }
    }

    fn revision(&mut self, revision: &RevisionSelection) {
        let phrase = (self.name)(revision);
        self.phrases.push(phrase);
    }

    /// `a`, `a and b`, `a, b and c`.
    fn list(&mut self, revisions: &[RevisionSelection]) {
        for (index, revision) in revisions.iter().enumerate() {
            if index > 0 {
                self.text(if index + 1 == revisions.len() {
                    " and "
                } else {
                    ", "
                });
            }
            self.revision(revision);
        }
    }

    fn destination(&mut self, destination: &Destination, onto: &str) {
        match destination {
            Destination::Onto(target) => {
                self.text(onto);
                self.revision(target);
            }
            Destination::After(target) => {
                self.text(" after ");
                self.revision(target);
            }
            Destination::Before(target) => {
                self.text(" before ");
                self.revision(target);
            }
            Destination::Between { parent, child } => {
                self.text(" between ");
                self.revision(parent);
                self.text(" and ");
                self.revision(child);
            }
        }
    }

    fn squash_target(&mut self, into: &SquashTarget) {
        match into {
            SquashTarget::Parent => self.text(" into its parent"),
            SquashTarget::Revision(target) => {
                self.text(" into ");
                self.revision(target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::JjCli;

    fn commit(hex: &str) -> RevisionSelection {
        RevisionSelection::Commit(hex.to_owned())
    }

    /// Names `aaa…` as `xyz`, `bbb…` as `main` (a bookmark) and anything
    /// else by commit id.
    fn name(revision: &RevisionSelection) -> Phrase {
        match revision {
            RevisionSelection::WorkingCopy => Phrase::Revision("@".to_owned()),
            RevisionSelection::Commit(hex) if hex.starts_with('a') => {
                Phrase::Revision("xyz".to_owned())
            }
            RevisionSelection::Commit(hex) if hex.starts_with('b') => {
                Phrase::Bookmark("main".to_owned())
            }
            RevisionSelection::Commit(hex) => Phrase::Revision(hex.clone()),
        }
    }

    fn words(op: &MutationOp, descendants: Option<usize>) -> String {
        sentence(op, &name, descendants)
            .expect("a draft op")
            .into_iter()
            .map(|phrase| match phrase {
                Phrase::Text(text) => text,
                Phrase::Revision(id) => format!("<{id}>"),
                Phrase::Bookmark(name) => format!("[{name}]"),
            })
            .collect()
    }

    #[test]
    fn a_rebase_reads_as_its_plan() {
        let rebase = |mode, destination| MutationOp::Rebase {
            mode,
            sources: vec![commit("aaa")],
            destination,
        };
        assert_eq!(
            words(
                &rebase(
                    RebaseSourceMode::WithDescendants,
                    Destination::Onto(commit("bbb"))
                ),
                Some(2)
            ),
            "Rebase <xyz> and 2 descendants onto [main]"
        );
        assert_eq!(
            words(
                &rebase(
                    RebaseSourceMode::WithDescendants,
                    Destination::Onto(commit("bbb"))
                ),
                None
            ),
            "Rebase <xyz> and descendants onto [main]"
        );
        assert_eq!(
            words(
                &rebase(
                    RebaseSourceMode::Revisions,
                    Destination::After(RevisionSelection::WorkingCopy)
                ),
                Some(3)
            ),
            "Rebase <xyz> after <@>"
        );
        assert_eq!(
            words(
                &rebase(
                    RebaseSourceMode::Branch,
                    Destination::Between {
                        parent: commit("bbb"),
                        child: commit("ccc"),
                    }
                ),
                None
            ),
            "Rebase the branch of <xyz> between [main] and <ccc>"
        );
    }

    #[test]
    fn lists_and_the_other_draft_kinds_read_naturally() {
        let three = vec![commit("aaa"), commit("bbb"), commit("ccc")];
        assert_eq!(
            words(
                &MutationOp::Merge {
                    parents: three.clone()
                },
                None
            ),
            "Create a merge of <xyz>, [main] and <ccc>"
        );
        assert_eq!(
            words(
                &MutationOp::Squash {
                    from: vec![commit("aaa"), commit("ccc")],
                    into: SquashTarget::Revision(commit("bbb")),
                },
                None
            ),
            "Squash <xyz> and <ccc> into [main]"
        );
        assert_eq!(
            words(
                &MutationOp::NewAt {
                    destination: Destination::Onto(commit("bbb"))
                },
                None
            ),
            "Create a change on top of [main]"
        );
        assert_eq!(
            words(&MutationOp::Parallelize { targets: three }, None),
            "Make <xyz>, [main] and <ccc> siblings"
        );
        assert!(sentence(&MutationOp::Redo, &name, None).is_none());
    }

    #[test]
    fn change_ids_are_short_unless_that_is_ambiguous() {
        assert_eq!(change_id_name("kmnopqrstuvw", None), "kmnopqrs");
        assert_eq!(change_id_name("kmnopqrstuvw", Some(3)), "kmnopqrs");
        assert_eq!(change_id_name("kmnopqrstuvw", Some(10)), "kmnopqrstu");
    }

    #[test]
    fn the_args_tag_is_jj_clis_and_parses_back() {
        let op = MutationOp::Describe {
            target: commit("0123456789abcdef"),
            description: "it's done".to_owned(),
        };
        let equivalent = ignoring_immutable(op.cli_equivalent(&crate::cli::CommitIds));
        let tag = args_tag(&equivalent);
        assert_eq!(
            tag,
            "jj describe 0123456789ab -m 'it\\'s done' --ignore-immutable"
        );
        let parsed = JjCli::default()
            .parse(&equivalent.to_shell())
            .expect("parses");
        assert!(parsed.ignore_immutable);

        let fetch = fetch_command(&FetchTarget::RemoteBranch {
            remote: "origin".to_owned(),
            branch: "main".to_owned(),
        });
        assert_eq!(
            args_tag(&fetch),
            "jj git fetch --remote origin --branch main"
        );
        assert!(JjCli::default().parse(&fetch.to_shell()).is_ok());
        assert!(
            JjCli::default()
                .parse(&fetch_command(&FetchTarget::AllRemotes).to_shell())
                .is_ok()
        );
    }

    #[test]
    fn two_commands_join_in_the_tag() {
        let op = MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::WorkingCopy,
            push_remote: Some("origin".to_owned()),
        };
        let tag = args_tag(&op.cli_equivalent(&crate::cli::CommitIds));
        assert!(tag.starts_with("jj bookmark "), "{tag}");
        assert!(tag.contains(" && jj git push "), "{tag}");
    }
}
