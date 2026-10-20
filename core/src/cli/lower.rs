//! Lowering a revset-level [`MutationCommand`] onto the op diffui already
//! runs. Resolving revsets is the caller's job (it is async and goes through
//! the repository actor); hand the answers in through a [`LowerContext`].

use std::fmt;
use std::ops::Range;

use super::command::{MutationCommand, RevsetArg, RevsetDestination, Spanned};
use crate::model::RevisionSelection;
use crate::mutations::{Destination, DraftKind, MutationOp, PlacementKind, SquashTarget};
use crate::push::{PushRequest, PushTarget};
use crate::repository::FetchTarget;

/// What lowering needs from the repository.
pub trait LowerContext {
    /// The revisions `revset` resolves to, in jj's order. Return
    /// [`RevisionSelection::WorkingCopy`] for the working-copy commit so the
    /// op addresses `@` the way the GUI's ops do.
    fn resolve(&self, revset: &str) -> Result<Vec<RevisionSelection>, String>;

    /// `git.push`, or the only remote, or `origin`: where a `jj git push`
    /// without `--remote` goes.
    fn default_push_remote(&self) -> Option<String> {
        None
    }

    /// The full hex id of the operation `operation` names (an id prefix,
    /// `@`, `@-`, …).
    fn operation_id(&self, operation: &str) -> Result<String, String> {
        Err(format!("can't resolve operation `{operation}` here"))
    }
}

/// A lowered native command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeAction {
    /// Send as [`crate::Command::Mutate`].
    Mutate(MutationOp),
    /// Send as [`crate::Command::Fetch`].
    Fetch(FetchTarget),
}

/// Why a native command can't become an op right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// A revset (or operation) didn't resolve.
    Resolve {
        text: String,
        span: Option<Range<usize>>,
        message: String,
    },
    /// An argument that takes one revision resolved to `count`.
    NotSingle {
        revset: String,
        span: Option<Range<usize>>,
        count: usize,
    },
    /// The arguments resolved to something diffui's op can't express (a
    /// merge destination, several revisions to describe at once). Running
    /// the same line through the CLI does exactly what jj does.
    NeedsCli(String),
    /// `jj git push` without `--remote`, and no default to fall back to.
    NoPushRemote,
}

impl LowerError {
    /// What to underline, when the error is about one argument.
    pub fn span(&self) -> Option<Range<usize>> {
        match self {
            Self::Resolve { span, .. } | Self::NotSingle { span, .. } => span.clone(),
            Self::NeedsCli(_) | Self::NoPushRemote => None,
        }
    }
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve { message, .. } => f.write_str(message),
            // jj's wording.
            Self::NotSingle {
                revset, count: 0, ..
            } => write!(f, "Revset `{revset}` didn't resolve to any revisions"),
            Self::NotSingle { revset, .. } => {
                write!(f, "Revset `{revset}` resolved to more than one revision")
            }
            Self::NeedsCli(reason) => f.write_str(reason),
            Self::NoPushRemote => f.write_str("No git remote to push to; pass --remote"),
        }
    }
}

impl std::error::Error for LowerError {}

struct Resolver<'a>(&'a dyn LowerContext);

impl Resolver<'_> {
    fn all(&self, arg: &RevsetArg) -> Result<Vec<RevisionSelection>, LowerError> {
        self.0
            .resolve(&arg.revset)
            .map_err(|message| LowerError::Resolve {
                text: arg.revset.clone(),
                span: arg.span.clone(),
                message,
            })
    }

    /// The union of several revset arguments, first occurrence first.
    fn union(&self, args: &[RevsetArg]) -> Result<Vec<RevisionSelection>, LowerError> {
        let mut out: Vec<RevisionSelection> = Vec::new();
        for arg in args {
            for revision in self.all(arg)? {
                if !out.contains(&revision) {
                    out.push(revision);
                }
            }
        }
        Ok(out)
    }

    fn non_empty(&self, args: &[RevsetArg]) -> Result<Vec<RevisionSelection>, LowerError> {
        let revisions = self.union(args)?;
        if revisions.is_empty() {
            let first = args
                .first()
                .cloned()
                .unwrap_or_else(|| RevsetArg::implicit("@"));
            return Err(LowerError::NotSingle {
                revset: first.revset,
                span: first.span,
                count: 0,
            });
        }
        Ok(revisions)
    }

    fn single(&self, arg: &RevsetArg) -> Result<RevisionSelection, LowerError> {
        let mut revisions = self.all(arg)?;
        if revisions.len() != 1 {
            return Err(LowerError::NotSingle {
                revset: arg.revset.clone(),
                span: arg.span.clone(),
                count: revisions.len(),
            });
        }
        Ok(revisions.remove(0))
    }

    /// A placement anchor: one revision, or the CLI for a merge placement.
    fn anchor(&self, arg: &RevsetArg) -> Result<RevisionSelection, LowerError> {
        let mut revisions = self.all(arg)?;
        match revisions.len() {
            1 => Ok(revisions.remove(0)),
            0 => Err(LowerError::NotSingle {
                revset: arg.revset.clone(),
                span: arg.span.clone(),
                count: 0,
            }),
            _ => Err(LowerError::NeedsCli(format!(
                "`{}` is several revisions; diffui places onto one",
                arg.revset
            ))),
        }
    }

    fn destination(&self, destination: &RevsetDestination) -> Result<Destination, LowerError> {
        Ok(match destination {
            RevsetDestination::Onto(target) => Destination::Onto(self.anchor(target)?),
            RevsetDestination::After(target) => Destination::After(self.anchor(target)?),
            RevsetDestination::Before(target) => Destination::Before(self.anchor(target)?),
            RevsetDestination::Between { after, before } => Destination::Between {
                parent: self.anchor(after)?,
                child: self.anchor(before)?,
            },
        })
    }

    /// `None` is jj's default, `@`.
    fn operation(&self, operation: Option<&Spanned<String>>) -> Result<String, LowerError> {
        let text = operation.map_or("@", |op| op.value.as_str());
        self.0
            .operation_id(text)
            .map_err(|message| LowerError::Resolve {
                text: text.to_owned(),
                span: operation.map(|op| op.span.clone()),
                message,
            })
    }
}

/// Lower `command` onto diffui's op.
pub fn lower(
    command: &MutationCommand,
    context: &dyn LowerContext,
) -> Result<NativeAction, LowerError> {
    let r = Resolver(context);
    let op = match command {
        MutationCommand::New { parents } => {
            let mut parents = r.non_empty(parents)?;
            if parents.len() == 1 {
                MutationOp::New {
                    parent: parents.remove(0),
                }
            } else {
                MutationOp::Merge { parents }
            }
        }
        MutationCommand::NewAt { destination } => MutationOp::NewAt {
            destination: r.destination(destination)?,
        },
        MutationCommand::Edit { target } => MutationOp::Edit {
            target: r.single(target)?,
        },
        MutationCommand::Abandon { targets } => MutationOp::Abandon {
            targets: r.non_empty(targets)?,
        },
        MutationCommand::Describe { target, message } => {
            let mut targets = r.all(target)?;
            match targets.len() {
                1 => MutationOp::Describe {
                    target: targets.remove(0),
                    description: message.clone(),
                },
                0 => {
                    return Err(LowerError::NotSingle {
                        revset: target.revset.clone(),
                        span: target.span.clone(),
                        count: 0,
                    });
                }
                _ => {
                    return Err(LowerError::NeedsCli(
                        "diffui describes one revision at a time".to_owned(),
                    ));
                }
            }
        }
        MutationCommand::Commit { message } => MutationOp::Commit {
            description: message.clone(),
        },
        MutationCommand::Rebase {
            mode,
            sources,
            destination,
        } => MutationOp::Rebase {
            mode: *mode,
            sources: r.non_empty(sources)?,
            destination: r.destination(destination)?,
        },
        MutationCommand::Squash { from, into: None } => {
            let [from] = from.as_slice() else {
                return Err(LowerError::NeedsCli(
                    "squash into the parent of one revision".into(),
                ));
            };
            MutationOp::Squash {
                from: vec![r.single(from)?],
                into: SquashTarget::Parent,
            }
        }
        MutationCommand::Squash {
            from,
            into: Some(into),
        } => MutationOp::Squash {
            from: r.non_empty(from)?,
            into: SquashTarget::Revision(r.single(into)?),
        },
        MutationCommand::Duplicate {
            targets,
            destination,
        } => MutationOp::DuplicateTo {
            targets: r.non_empty(targets)?,
            destination: destination
                .as_ref()
                .map(|destination| r.destination(destination))
                .transpose()?,
        },
        MutationCommand::Parallelize { targets } => MutationOp::Parallelize {
            targets: r.non_empty(targets)?,
        },
        MutationCommand::Absorb { from } => MutationOp::Absorb {
            from: r.single(from)?,
        },
        MutationCommand::Metaedit {
            targets,
            update_author,
            update_author_timestamp,
        } => MutationOp::Metaedit {
            targets: r.non_empty(targets)?,
            update_author: *update_author,
            update_author_timestamp: *update_author_timestamp,
        },
        MutationCommand::BookmarkCreate { name, to } => MutationOp::CreateBookmark {
            name: name.value.clone(),
            to: r.single(to)?,
        },
        MutationCommand::BookmarkSet { name, to, .. } => MutationOp::MoveBookmark {
            name: name.value.clone(),
            to: r.single(to)?,
            push_remote: None,
        },
        MutationCommand::BookmarkDelete { name } => MutationOp::DeleteBookmark {
            name: name.value.clone(),
        },
        MutationCommand::BookmarkForget { name } => MutationOp::ForgetBookmark {
            name: name.value.clone(),
        },
        MutationCommand::BookmarkRename { old, new } => MutationOp::RenameBookmark {
            old: old.value.clone(),
            new: new.value.clone(),
        },
        MutationCommand::BookmarkTrack { name, remote } => MutationOp::TrackBookmark {
            name: name.clone(),
            remote: remote.clone(),
        },
        MutationCommand::BookmarkUntrack { name, remote } => MutationOp::UntrackBookmark {
            name: name.clone(),
            remote: remote.clone(),
        },
        MutationCommand::Push {
            remote,
            bookmarks,
            changes,
            allow,
        } => {
            let remote = remote
                .clone()
                .or_else(|| context.default_push_remote())
                .ok_or(LowerError::NoPushRemote)?;
            let mut targets: Vec<PushTarget> = bookmarks
                .iter()
                .map(|name| PushTarget::Bookmark(name.value.clone()))
                .collect();
            for revision in r.union(changes)? {
                targets.push(PushTarget::Change(revision));
            }
            if targets.is_empty() {
                targets.push(PushTarget::Tracked);
            }
            // As the GUI does: a push nobody picked target by target is
            // confirmed first.
            let confirm = targets.contains(&PushTarget::Tracked);
            MutationOp::Push(PushRequest {
                remote,
                targets,
                allow: *allow,
                confirm,
            })
        }
        MutationCommand::Fetch => return Ok(NativeAction::Fetch(FetchTarget::Default)),
        MutationCommand::FetchAll => return Ok(NativeAction::Fetch(FetchTarget::AllRemotes)),
        MutationCommand::FetchBranch { remote, branch } => {
            return Ok(NativeAction::Fetch(FetchTarget::RemoteBranch {
                remote: remote.clone(),
                branch: branch.clone(),
            }));
        }
        MutationCommand::Undo => MutationOp::Undo { operation_id: None },
        MutationCommand::Redo => MutationOp::Redo,
        MutationCommand::OpRestore { operation } => MutationOp::RestoreOperation {
            operation_id: r.operation(Some(operation))?,
        },
        MutationCommand::OpRevert { operation } => MutationOp::RevertOperation {
            operation_id: r.operation(operation.as_ref())?,
        },
    };
    Ok(NativeAction::Mutate(op))
}

impl MutationCommand {
    /// The target-mode draft this command is, when the GUI builds the same
    /// op by picking a destination: start the draft with the lowered op's
    /// sources and focus its destination, so typing and picking meet in the
    /// same draft (with its conflict forecast).
    pub fn draft_kind(&self) -> Option<(DraftKind, PlacementKind)> {
        let placement = |destination: &RevsetDestination| match destination {
            RevsetDestination::Onto(_) => PlacementKind::Onto,
            RevsetDestination::After(_) | RevsetDestination::Between { .. } => PlacementKind::After,
            RevsetDestination::Before(_) => PlacementKind::Before,
        };
        match self {
            Self::Rebase {
                mode, destination, ..
            } => Some((DraftKind::Rebase { mode: *mode }, placement(destination))),
            Self::Squash { into: Some(_), .. } => Some((DraftKind::Squash, PlacementKind::Onto)),
            Self::NewAt { destination } => Some((DraftKind::New, placement(destination))),
            Self::Duplicate {
                destination: Some(destination),
                ..
            } => Some((DraftKind::Duplicate, placement(destination))),
            Self::Parallelize { .. } => Some((DraftKind::Parallelize, PlacementKind::Onto)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse::parse;
    use crate::cli::{CliCommand, CliConfig};

    fn commit(n: char) -> RevisionSelection {
        RevisionSelection::Commit(std::iter::repeat_n(n, 40).collect())
    }

    #[test]
    fn lowering_reports_what_it_cannot_express() {
        struct Many;
        impl LowerContext for Many {
            fn resolve(&self, revset: &str) -> Result<Vec<RevisionSelection>, String> {
                match revset {
                    "none()" => Ok(Vec::new()),
                    "@" => Ok(vec![RevisionSelection::WorkingCopy]),
                    _ => Ok(vec![commit('a'), commit('b')]),
                }
            }
        }
        let lower_line = |line: &str| {
            let parsed = parse(&CliConfig::builtin(), line).unwrap();
            let CliCommand::Mutation(command) = parsed.command else {
                panic!("{line}");
            };
            lower(&command, &Many)
        };
        assert!(matches!(
            lower_line("jj edit 'x|y'"),
            Err(LowerError::NotSingle { count: 2, .. })
        ));
        assert!(matches!(
            lower_line("jj rebase -s x -o 'x|y'"),
            Err(LowerError::NeedsCli(_))
        ));
        assert!(matches!(
            lower_line("jj abandon 'none()'"),
            Err(LowerError::NotSingle { count: 0, .. })
        ));
        assert!(matches!(
            lower_line("jj git push"),
            Err(LowerError::NoPushRemote)
        ));
        // Several parents make a merge.
        assert!(matches!(
            lower_line("jj new 'x|y'"),
            Ok(NativeAction::Mutate(MutationOp::Merge { parents })) if parents.len() == 2
        ));
        let error = lower_line("jj edit 'x|y'").unwrap_err();
        assert_eq!(error.span(), Some(9..12));
        assert_eq!(
            error.to_string(),
            "Revset `x|y` resolved to more than one revision"
        );
    }
}
