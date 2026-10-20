//! GUI → command: every [`MutationOp`] as the jj command that does the same.
//!
//! Revisions are named by short change id, through a [`RevisionNames`] the
//! caller supplies: core sees only commit ids, the frontend holds the loaded
//! graph that maps them to change ids. `@` is always `@`; a commit the namer
//! doesn't know (or whose change is divergent, where the change id names
//! several commits) falls back to a 12-digit commit id prefix, which jj
//! accepts anywhere a revision goes.

use std::fmt;

use super::tokenize::shell_quote;
use crate::model::RevisionSelection;
use crate::mutations::{
    Destination, DraftKind, MutationOp, OpDraft, PlacementKind, RebaseSourceMode, SquashTarget,
};
use crate::partial::{DiffSelection, HunkPick};
use crate::push::PushTarget;
use crate::resolve::ConflictResolution;

/// How many hex digits of a commit or operation id the rendering keeps:
/// jj's own short form.
const SHORT_HEX: usize = 12;

/// Maps commit ids to the change ids the command names them by.
pub trait RevisionNames {
    /// The shortest unique change id of the commit `commit_id` (hex), or
    /// `None` when it isn't known.
    fn change_id(&self, commit_id: &str) -> Option<String>;

    /// Whether the commit's change is divergent, so its change id alone
    /// doesn't name it.
    fn is_divergent(&self, _commit_id: &str) -> bool {
        false
    }
}

impl<F: Fn(&str) -> Option<String>> RevisionNames for F {
    fn change_id(&self, commit_id: &str) -> Option<String> {
        self(commit_id)
    }
}

/// Names every revision by commit id prefix.
pub struct CommitIds;

impl RevisionNames for CommitIds {
    fn change_id(&self, _commit_id: &str) -> Option<String> {
        None
    }
}

/// The jj command(s) equivalent to an op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliEquivalent {
    /// Each invocation's arguments, without the leading `jj`. One, except
    /// for the ops that do two things at once (move a bookmark, then push
    /// it).
    pub commands: Vec<Vec<String>>,
    /// Whether parsing and lowering [`to_shell`](Self::to_shell) gives back
    /// this very op. `false` where the command is jj's nearest spelling of
    /// it rather than the same thing: a hunk-level selection becomes `-i`, a
    /// push that moves its bookmark becomes two commands, and an op the CLI
    /// spells like another (`jj new X` for a new placed onto X) comes back as
    /// that other op.
    pub exact: bool,
}

impl CliEquivalent {
    fn one(args: Vec<String>) -> Self {
        Self {
            commands: vec![args],
            exact: true,
        }
    }

    fn approximate(mut self) -> Self {
        self.exact = false;
        self
    }

    /// What a user types: `jj rebase -s abc -o 'trunk()'`, commands joined
    /// with `&&`. Quoted for a POSIX shell, which is also what the command
    /// line's tokeniser reads.
    pub fn to_shell(&self) -> String {
        self.commands
            .iter()
            .map(|args| {
                std::iter::once("jj".to_owned())
                    .chain(args.iter().map(|arg| shell_quote(arg)))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" && ")
    }

    /// The first command as jj-cli records it in an operation's `args` tag,
    /// byte for byte (jj's own escaping, which differs from a shell's for
    /// `'`).
    pub fn op_args_tag(&self) -> String {
        jj_args_tag(self.commands.first().map_or(&[][..], Vec::as_slice))
    }
}

impl fmt::Display for CliEquivalent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_shell())
    }
}

/// `args` as jj-cli 0.45.1 writes an operation's `args` tag: `jj`, then each
/// argument bare or in single quotes with `'` escaped as `\'`.
pub fn jj_args_tag(args: &[String]) -> String {
    let escape = |arg: &String| {
        if arg.bytes().all(|b| {
            matches!(b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b',' | b'-' | b'.' | b'/' | b':' | b'@' | b'_')
        }) {
            arg.clone()
        } else {
            format!("'{}'", arg.replace('\'', "\\'"))
        }
    };
    std::iter::once("jj".to_owned())
        .chain(args.iter().map(escape))
        .collect::<Vec<_>>()
        .join(" ")
}

struct Renderer<'a>(&'a dyn RevisionNames);

impl Renderer<'_> {
    fn rev(&self, revision: &RevisionSelection) -> String {
        match revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(commit_id) => self
                .0
                .change_id(commit_id)
                .filter(|_| !self.0.is_divergent(commit_id))
                .unwrap_or_else(|| short_hex(commit_id)),
        }
    }

    fn revs(&self, revisions: &[RevisionSelection]) -> Vec<String> {
        revisions.iter().map(|r| self.rev(r)).collect()
    }

    /// Positional revisions that default to `@`: nothing for just `@`.
    fn revs_or_default(&self, revisions: &[RevisionSelection]) -> Vec<String> {
        match revisions {
            [RevisionSelection::WorkingCopy] => Vec::new(),
            _ => self.revs(revisions),
        }
    }

    fn flagged(&self, flag: &str, revisions: &[RevisionSelection]) -> Vec<String> {
        revisions
            .iter()
            .flat_map(|r| [flag.to_owned(), self.rev(r)])
            .collect()
    }

    fn destination(&self, destination: &Destination) -> Vec<String> {
        match destination {
            Destination::Onto(target) => vec!["-o".into(), self.rev(target)],
            Destination::After(target) => vec!["-A".into(), self.rev(target)],
            Destination::Before(target) => vec!["-B".into(), self.rev(target)],
            Destination::Between { parent, child } => {
                vec!["-A".into(), self.rev(parent), "-B".into(), self.rev(child)]
            }
        }
    }

    /// `-r X` unless X is `@`, the default.
    fn revision_flag(&self, revision: &RevisionSelection) -> Vec<String> {
        match revision {
            RevisionSelection::WorkingCopy => Vec::new(),
            _ => vec!["-r".into(), self.rev(revision)],
        }
    }
}

fn short_hex(hex: &str) -> String {
    hex.chars().take(SHORT_HEX).collect()
}

fn strings<const N: usize>(words: [&str; N]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// A bookmark name as a string pattern: plain unless it has glob
/// characters, which `exact:` neutralises.
fn bookmark_pattern(name: &str) -> String {
    if name.contains(['*', '?', '[', ']']) || name.contains(':') {
        format!("exact:{name}")
    } else {
        name.to_owned()
    }
}

/// A repo path as a fileset: bare when it parses as one, else `file:"…"`.
fn fileset(path: &str) -> String {
    let bare = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-' | b'_'));
    if bare {
        path.to_owned()
    } else {
        format!(
            "file:\"{}\"",
            path.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

/// The selection's paths, and whether it picks whole files only (else the
/// CLI needs `-i` to pick hunks).
fn selection_paths(selection: &DiffSelection) -> (Vec<String>, bool) {
    let whole = selection
        .files
        .iter()
        .all(|file| matches!(file.hunks, HunkPick::All));
    (
        selection.files.iter().map(|f| fileset(&f.path)).collect(),
        whole,
    )
}

impl MutationOp {
    /// The jj command that does what this op does. See [`RevisionNames`]
    /// for how revisions are named.
    pub fn cli_equivalent(&self, names: &dyn RevisionNames) -> CliEquivalent {
        let r = Renderer(names);
        match self {
            Self::New { parent } => {
                let mut args = strings(["new"]);
                args.extend(r.revs_or_default(std::slice::from_ref(parent)));
                CliEquivalent::one(args)
            }
            Self::Edit { target } => CliEquivalent::one(vec!["edit".into(), r.rev(target)]),
            Self::Abandon { targets } => {
                let mut args = strings(["abandon"]);
                args.extend(r.revs_or_default(targets));
                CliEquivalent::one(args)
            }
            Self::Describe {
                target,
                description,
            } => {
                let mut args = strings(["describe"]);
                args.extend(r.revs_or_default(std::slice::from_ref(target)));
                args.extend(["-m".into(), description.clone()]);
                CliEquivalent::one(args)
            }
            Self::Rebase {
                mode,
                sources,
                destination,
            } => {
                let mut args = strings(["rebase"]);
                match mode {
                    RebaseSourceMode::Revisions => args.extend(r.flagged("-r", sources)),
                    RebaseSourceMode::WithDescendants => args.extend(r.flagged("-s", sources)),
                    RebaseSourceMode::Branch => {
                        if sources.as_slice() != [RevisionSelection::WorkingCopy] {
                            args.extend(r.flagged("-b", sources));
                        }
                    }
                }
                args.extend(r.destination(destination));
                CliEquivalent::one(args)
            }
            Self::Squash {
                from,
                into: SquashTarget::Parent,
            } => match from.as_slice() {
                [source] => {
                    let mut args = strings(["squash"]);
                    args.extend(r.revision_flag(source));
                    CliEquivalent::one(args)
                }
                _ => {
                    let mut args = strings(["squash"]);
                    args.extend(r.flagged("--from", from));
                    CliEquivalent::one(args).approximate()
                }
            },
            Self::Squash {
                from,
                into: SquashTarget::Revision(into),
            } => {
                let mut args = strings(["squash"]);
                if from.as_slice() != [RevisionSelection::WorkingCopy] {
                    args.extend(r.flagged("--from", from));
                }
                args.extend(["--into".into(), r.rev(into)]);
                CliEquivalent::one(args)
            }
            Self::Merge { parents } => {
                let mut args = strings(["new"]);
                args.extend(r.revs(parents));
                CliEquivalent::one(args)
            }
            Self::Duplicate { target } => {
                let mut args = strings(["duplicate"]);
                args.extend(r.revs_or_default(std::slice::from_ref(target)));
                CliEquivalent::one(args).approximate()
            }
            Self::Absorb { from } => {
                let mut args = strings(["absorb"]);
                if *from != RevisionSelection::WorkingCopy {
                    args.extend(["--from".into(), r.rev(from)]);
                }
                CliEquivalent::one(args)
            }
            Self::MoveBookmark {
                name,
                to,
                push_remote,
            } => {
                let set = vec![
                    "bookmark".into(),
                    "set".into(),
                    bookmark_pattern(name),
                    "-r".into(),
                    r.rev(to),
                ];
                match push_remote {
                    None => CliEquivalent::one(set),
                    Some(remote) => CliEquivalent {
                        commands: vec![
                            set,
                            vec![
                                "git".into(),
                                "push".into(),
                                "--remote".into(),
                                remote.clone(),
                                "-b".into(),
                                bookmark_pattern(name),
                            ],
                        ],
                        exact: false,
                    },
                }
            }
            Self::DeleteBookmark { name } => CliEquivalent::one(vec![
                "bookmark".into(),
                "delete".into(),
                bookmark_pattern(name),
            ]),
            Self::TrackBookmark { name, remote } => CliEquivalent::one(vec![
                "bookmark".into(),
                "track".into(),
                format!("{name}@{remote}"),
            ]),
            Self::UntrackBookmark { name, remote } => CliEquivalent::one(vec![
                "bookmark".into(),
                "untrack".into(),
                format!("{name}@{remote}"),
            ]),
            Self::PushBookmark { name, remote } => CliEquivalent::one(vec![
                "git".into(),
                "push".into(),
                "--remote".into(),
                remote.clone(),
                "-b".into(),
                bookmark_pattern(name),
            ])
            .approximate(),
            Self::Undo { operation_id: None } => CliEquivalent::one(strings(["undo"])),
            Self::Undo {
                operation_id: Some(operation),
            } => CliEquivalent::one(vec!["op".into(), "revert".into(), short_hex(operation)])
                .approximate(),
            Self::Redo => CliEquivalent::one(strings(["redo"])),
            Self::CreateBookmark { name, to } => CliEquivalent::one(vec![
                "bookmark".into(),
                "create".into(),
                bookmark_pattern(name),
                "-r".into(),
                r.rev(to),
            ]),
            Self::RenameBookmark { old, new } => CliEquivalent::one(vec![
                "bookmark".into(),
                "rename".into(),
                old.clone(),
                new.clone(),
            ]),
            Self::ForgetBookmark { name } => CliEquivalent::one(vec![
                "bookmark".into(),
                "forget".into(),
                bookmark_pattern(name),
            ]),
            Self::Push(request) => {
                let mut before = Vec::new();
                let mut push = vec![
                    "git".into(),
                    "push".into(),
                    "--remote".into(),
                    request.remote.clone(),
                ];
                let mut exact = request.confirm == request.targets.contains(&PushTarget::Tracked);
                for target in &request.targets {
                    match target {
                        PushTarget::Bookmark(name) => {
                            push.extend(["-b".into(), bookmark_pattern(name)])
                        }
                        PushTarget::Change(revision) => push.extend(["-c".into(), r.rev(revision)]),
                        PushTarget::Tracked => {}
                        PushTarget::SetBookmark { name, to } => {
                            before.push(vec![
                                "bookmark".into(),
                                "set".into(),
                                bookmark_pattern(name),
                                "-r".into(),
                                r.rev(to),
                            ]);
                            push.extend(["-b".into(), bookmark_pattern(name)]);
                            exact = false;
                        }
                        PushTarget::DeleteBookmark(name) => {
                            before.push(vec![
                                "bookmark".into(),
                                "delete".into(),
                                bookmark_pattern(name),
                            ]);
                            push.extend(["-b".into(), bookmark_pattern(name)]);
                            exact = false;
                        }
                    }
                }
                let allow = request.allow;
                for (on, flag) in [
                    (allow.empty_description, "--allow-empty-description"),
                    (allow.private, "--allow-private"),
                    (allow.conflicts, "--allow-conflicts"),
                ] {
                    if on {
                        push.push(flag.into());
                    }
                }
                before.push(push);
                CliEquivalent {
                    commands: before,
                    exact,
                }
            }
            Self::Commit { description } => {
                CliEquivalent::one(vec!["commit".into(), "-m".into(), description.clone()])
            }
            Self::NewAt { destination } => match destination {
                Destination::Onto(target) => {
                    CliEquivalent::one(vec!["new".into(), r.rev(target)]).approximate()
                }
                _ => {
                    let mut args = strings(["new"]);
                    args.extend(r.destination(destination));
                    CliEquivalent::one(args)
                }
            },
            Self::Parallelize { targets } => {
                let mut args = strings(["parallelize"]);
                args.extend(r.revs(targets));
                CliEquivalent::one(args)
            }
            Self::DuplicateTo {
                targets,
                destination,
            } => {
                let mut args = strings(["duplicate"]);
                args.extend(r.revs_or_default(targets));
                if let Some(destination) = destination {
                    args.extend(r.destination(destination));
                }
                CliEquivalent::one(args)
            }
            Self::KeepDivergent { keep } => {
                let commit = match keep {
                    RevisionSelection::Commit(id) => short_hex(id),
                    RevisionSelection::WorkingCopy => "@".to_owned(),
                };
                // An unknown change id renders the commit prefix inside
                // `change_id()`, which jj rejects rather than abandoning
                // something else.
                let change = match keep {
                    RevisionSelection::Commit(id) => {
                        names.change_id(id).unwrap_or_else(|| commit.clone())
                    }
                    RevisionSelection::WorkingCopy => commit.clone(),
                };
                CliEquivalent::one(vec![
                    "abandon".into(),
                    format!("change_id({change}) ~ {commit}"),
                ])
                .approximate()
            }
            Self::Metaedit {
                targets,
                update_author,
                update_author_timestamp,
            } => {
                let mut args = strings(["metaedit"]);
                args.extend(r.revs_or_default(targets));
                if *update_author {
                    args.push("--update-author".into());
                }
                if *update_author_timestamp {
                    args.push("--update-author-timestamp".into());
                }
                let equivalent = CliEquivalent::one(args);
                if *update_author || *update_author_timestamp {
                    equivalent
                } else {
                    equivalent.approximate()
                }
            }
            Self::Split {
                target,
                selection,
                first_description,
                ..
            } => {
                let mut args = strings(["split"]);
                args.extend(r.revision_flag(target));
                let (paths, whole) = selection_paths(selection);
                if !whole {
                    args.push("-i".into());
                }
                args.extend(paths);
                if let Some(description) = first_description {
                    args.extend(["-m".into(), description.clone()]);
                }
                CliEquivalent::one(args).approximate()
            }
            Self::SquashChanges {
                from,
                selection,
                into,
            } => {
                let mut args = strings(["squash"]);
                match into {
                    SquashTarget::Parent => args.extend(r.revision_flag(from)),
                    SquashTarget::Revision(into) => {
                        if *from != RevisionSelection::WorkingCopy {
                            args.extend(["--from".into(), r.rev(from)]);
                        }
                        args.extend(["--into".into(), r.rev(into)]);
                    }
                }
                let (paths, whole) = selection_paths(selection);
                if !whole {
                    args.push("-i".into());
                }
                args.extend(paths);
                CliEquivalent::one(args).approximate()
            }
            Self::RestoreChanges { target, selection } => {
                let mut args = strings(["restore", "--changes-in"]);
                args.push(r.rev(target));
                let (paths, whole) = selection_paths(selection);
                if !whole {
                    args.push("-i".into());
                }
                args.extend(paths);
                CliEquivalent::one(args).approximate()
            }
            Self::ResolveConflict {
                target,
                path,
                resolution,
            } => {
                let mut args = strings(["resolve"]);
                args.extend(r.revision_flag(target));
                match resolution {
                    ConflictResolution::Side(0) => args.extend(strings(["--tool", ":ours"])),
                    ConflictResolution::Side(1) => args.extend(strings(["--tool", ":theirs"])),
                    _ => {}
                }
                args.push(fileset(path));
                CliEquivalent::one(args).approximate()
            }
            Self::RestoreOperation { operation_id } => {
                CliEquivalent::one(vec!["op".into(), "restore".into(), short_hex(operation_id)])
            }
            Self::RevertOperation { operation_id } => {
                CliEquivalent::one(vec!["op".into(), "revert".into(), short_hex(operation_id)])
            }
        }
    }
}

impl OpDraft {
    /// The draft as a command line that still lacks its destination, for
    /// opening the command line on it: `jj rebase -s abc -o `, ending where
    /// the destination revset goes. Drafts complete from their sources alone
    /// (parallelize) render whole.
    pub fn cli_prefix(&self, names: &dyn RevisionNames) -> String {
        let r = Renderer(names);
        let sources: Vec<RevisionSelection> =
            self.sources.iter().map(|s| s.selection.clone()).collect();
        let placement = match self.placement {
            PlacementKind::Onto => "-o",
            PlacementKind::After => "-A",
            PlacementKind::Before => "-B",
        };
        let mut args: Vec<String> = Vec::new();
        match self.kind {
            DraftKind::Rebase { mode } => {
                args.push("rebase".into());
                let flag = match mode {
                    RebaseSourceMode::Revisions => "-r",
                    RebaseSourceMode::WithDescendants => "-s",
                    RebaseSourceMode::Branch => "-b",
                };
                args.extend(r.flagged(flag, &sources));
                args.push(placement.into());
            }
            DraftKind::Squash => {
                args.push("squash".into());
                args.extend(r.flagged("--from", &sources));
                if let Some(selection) = &self.selection {
                    let (paths, whole) = selection_paths(selection);
                    if !whole {
                        args.push("-i".into());
                    }
                    args.extend(paths);
                }
                args.push("--into".into());
            }
            DraftKind::Merge => {
                args.push("new".into());
                args.extend(r.revs(&sources));
            }
            DraftKind::New => {
                args.push("new".into());
                if self.placement != PlacementKind::Onto {
                    args.push(placement.into());
                }
            }
            DraftKind::Duplicate => {
                args.push("duplicate".into());
                args.extend(r.revs(&sources));
                args.push(placement.into());
            }
            DraftKind::Parallelize => {
                args.push("parallelize".into());
                args.extend(r.revs(&sources));
                return CliEquivalent::one(args).to_shell();
            }
        }
        CliEquivalent::one(args).to_shell() + " "
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::cli::{CliCommand, CliConfig, LowerContext, NativeAction, lower};
    use crate::mutations::PlacementKind;
    use crate::partial::{FileSelection, HunkRef};
    use crate::push::{PushAllow, PushRequest};

    /// Four commits, named `kkkk`… by change id; `mmmm` is divergent.
    struct Repo {
        names: HashMap<String, String>,
    }

    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    fn c(n: char) -> RevisionSelection {
        RevisionSelection::Commit(hex(n))
    }

    const OP: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\
                      0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    impl Repo {
        fn new() -> Self {
            let names = [('a', "kkkk"), ('b', "llll"), ('c', "mmmm"), ('d', "nnnn")]
                .into_iter()
                .map(|(commit, change)| (hex(commit), change.to_owned()))
                .collect();
            Self { names }
        }
    }

    impl RevisionNames for Repo {
        fn change_id(&self, commit_id: &str) -> Option<String> {
            self.names.get(commit_id).cloned()
        }

        fn is_divergent(&self, commit_id: &str) -> bool {
            self.names.get(commit_id).is_some_and(|name| name == "mmmm")
        }
    }

    impl LowerContext for Repo {
        fn resolve(&self, revset: &str) -> Result<Vec<RevisionSelection>, String> {
            if revset == "@" {
                return Ok(vec![RevisionSelection::WorkingCopy]);
            }
            for (commit, change) in &self.names {
                if change == revset || commit.starts_with(revset) {
                    return Ok(vec![RevisionSelection::Commit(commit.clone())]);
                }
            }
            Err(format!("Revision `{revset}` doesn't exist"))
        }

        fn default_push_remote(&self) -> Option<String> {
            Some("origin".to_owned())
        }

        fn operation_id(&self, operation: &str) -> Result<String, String> {
            OP.starts_with(operation)
                .then(|| OP.to_owned())
                .ok_or_else(|| format!("No operation `{operation}`"))
        }
    }

    /// Render, parse and lower `op`.
    fn round_trip(op: &MutationOp) -> (String, MutationOp) {
        let repo = Repo::new();
        let line = op.cli_equivalent(&repo).to_shell();
        let parsed = crate::cli::parse::parse(&CliConfig::builtin(), &line)
            .unwrap_or_else(|e| panic!("{line}: {e:?}"));
        let CliCommand::Mutation(command) = parsed.command else {
            panic!("{line}: not a mutation: {:?}", parsed.command);
        };
        match lower(&command, &repo) {
            Ok(NativeAction::Mutate(back)) => (line, back),
            other => panic!("{line}: {other:?}"),
        }
    }

    fn whole(paths: &[&str]) -> DiffSelection {
        DiffSelection {
            files: paths
                .iter()
                .map(|path| FileSelection {
                    path: (*path).to_owned(),
                    hunks: HunkPick::All,
                })
                .collect(),
        }
    }

    fn exact_ops() -> Vec<MutationOp> {
        use RebaseSourceMode::*;
        let wc = RevisionSelection::WorkingCopy;
        let mut ops = vec![
            MutationOp::New { parent: wc.clone() },
            MutationOp::New { parent: c('a') },
            MutationOp::Edit { target: c('b') },
            MutationOp::Abandon {
                targets: vec![wc.clone()],
            },
            MutationOp::Abandon {
                targets: vec![c('a'), c('b')],
            },
            MutationOp::Describe {
                target: wc.clone(),
                description: "hello world".to_owned(),
            },
            MutationOp::Describe {
                target: c('a'),
                description: "it's\n\nmulti-line".to_owned(),
            },
            MutationOp::Squash {
                from: vec![wc.clone()],
                into: SquashTarget::Parent,
            },
            MutationOp::Squash {
                from: vec![c('a')],
                into: SquashTarget::Parent,
            },
            MutationOp::Squash {
                from: vec![wc.clone()],
                into: SquashTarget::Revision(c('a')),
            },
            MutationOp::Squash {
                from: vec![c('a'), c('b')],
                into: SquashTarget::Revision(c('d')),
            },
            MutationOp::Merge {
                parents: vec![c('a'), c('b'), wc.clone()],
            },
            MutationOp::Absorb { from: wc.clone() },
            MutationOp::Absorb { from: c('b') },
            MutationOp::MoveBookmark {
                name: "main".to_owned(),
                to: c('a'),
                push_remote: None,
            },
            MutationOp::DeleteBookmark {
                name: "main".to_owned(),
            },
            MutationOp::DeleteBookmark {
                name: "feat*".to_owned(),
            },
            MutationOp::TrackBookmark {
                name: "main".to_owned(),
                remote: "origin".to_owned(),
            },
            MutationOp::UntrackBookmark {
                name: "main".to_owned(),
                remote: "upstream".to_owned(),
            },
            MutationOp::Undo { operation_id: None },
            MutationOp::Redo,
            MutationOp::CreateBookmark {
                name: "topic".to_owned(),
                to: wc.clone(),
            },
            MutationOp::RenameBookmark {
                old: "a".to_owned(),
                new: "b".to_owned(),
            },
            MutationOp::ForgetBookmark {
                name: "old".to_owned(),
            },
            MutationOp::Push(PushRequest {
                remote: "upstream".to_owned(),
                targets: vec![
                    PushTarget::Bookmark("main".to_owned()),
                    PushTarget::Change(c('b')),
                ],
                allow: PushAllow {
                    empty_description: true,
                    private: false,
                    conflicts: true,
                },
                confirm: false,
            }),
            MutationOp::Push(PushRequest {
                remote: "origin".to_owned(),
                targets: vec![PushTarget::Tracked],
                allow: PushAllow::default(),
                confirm: true,
            }),
            MutationOp::Commit {
                description: "feat: a thing".to_owned(),
            },
            MutationOp::NewAt {
                destination: Destination::After(c('a')),
            },
            MutationOp::NewAt {
                destination: Destination::Before(wc.clone()),
            },
            MutationOp::NewAt {
                destination: Destination::Between {
                    parent: c('a'),
                    child: c('b'),
                },
            },
            MutationOp::Parallelize {
                targets: vec![c('a'), c('b')],
            },
            MutationOp::DuplicateTo {
                targets: vec![c('a')],
                destination: None,
            },
            MutationOp::DuplicateTo {
                targets: vec![c('a'), c('b')],
                destination: Some(Destination::Onto(c('d'))),
            },
            MutationOp::Metaedit {
                targets: vec![wc.clone()],
                update_author: true,
                update_author_timestamp: true,
            },
            MutationOp::RestoreOperation {
                operation_id: OP.to_owned(),
            },
            MutationOp::RevertOperation {
                operation_id: OP.to_owned(),
            },
        ];
        for mode in [Revisions, WithDescendants, Branch] {
            for destination in [
                Destination::Onto(c('d')),
                Destination::After(c('d')),
                Destination::Before(c('d')),
                Destination::Between {
                    parent: c('d'),
                    child: wc.clone(),
                },
            ] {
                ops.push(MutationOp::Rebase {
                    mode,
                    sources: vec![c('a'), c('b')],
                    destination,
                });
            }
        }
        ops.push(MutationOp::Rebase {
            mode: Branch,
            sources: vec![wc],
            destination: Destination::Onto(c('c')),
        });
        ops
    }

    #[test]
    fn exact_ops_round_trip() {
        let repo = Repo::new();
        for op in exact_ops() {
            assert!(op.cli_equivalent(&repo).exact, "{op:?}");
            let (line, back) = round_trip(&op);
            assert_eq!(back, op, "{line}");
        }
    }

    #[test]
    fn renders_like_a_person_would_type_it() {
        let repo = Repo::new();
        let render = |op: MutationOp| op.cli_equivalent(&repo).to_shell();
        assert_eq!(
            render(MutationOp::Rebase {
                mode: RebaseSourceMode::WithDescendants,
                sources: vec![c('a')],
                destination: Destination::Onto(c('b')),
            }),
            "jj rebase -s kkkk -o llll"
        );
        assert_eq!(
            render(MutationOp::Describe {
                target: RevisionSelection::WorkingCopy,
                description: "it's done".to_owned(),
            }),
            r"jj describe -m 'it'\''s done'"
        );
        // A divergent change id names several commits: use the commit.
        assert_eq!(
            render(MutationOp::Edit { target: c('c') }),
            "jj edit cccccccccccc"
        );
        assert_eq!(
            render(MutationOp::Edit { target: c('e') }),
            "jj edit eeeeeeeeeeee"
        );
        assert_eq!(
            render(MutationOp::Squash {
                from: vec![RevisionSelection::WorkingCopy],
                into: SquashTarget::Parent,
            }),
            "jj squash"
        );
        assert_eq!(
            render(MutationOp::RevertOperation {
                operation_id: OP.to_owned()
            }),
            "jj op revert 0123456789ab"
        );
    }

    #[test]
    fn op_args_tag_matches_jj() {
        let equivalent = MutationOp::Describe {
            target: c('a'),
            description: "it's done".to_owned(),
        }
        .cli_equivalent(&Repo::new());
        assert_eq!(
            equivalent.op_args_tag(),
            r"jj describe kkkk -m 'it\'s done'"
        );
    }

    #[test]
    fn approximate_ops_still_render_runnable_commands() {
        let repo = Repo::new();
        let partial = DiffSelection {
            files: vec![FileSelection {
                path: "src/a b.rs".to_owned(),
                hunks: HunkPick::Hunks(vec![HunkRef {
                    index: 0,
                    header: "@@ -1 +1 @@".to_owned(),
                    lines: None,
                }]),
            }],
        };
        let ops = [
            MutationOp::Duplicate { target: c('a') },
            MutationOp::PushBookmark {
                name: "main".to_owned(),
                remote: "origin".to_owned(),
            },
            MutationOp::Undo {
                operation_id: Some(OP.to_owned()),
            },
            MutationOp::NewAt {
                destination: Destination::Onto(c('a')),
            },
            MutationOp::KeepDivergent { keep: c('c') },
            MutationOp::Split {
                target: c('a'),
                selection: whole(&["a.rs"]),
                first_description: Some("first".to_owned()),
                second_description: "second".to_owned(),
            },
            MutationOp::SquashChanges {
                from: c('a'),
                selection: partial.clone(),
                into: SquashTarget::Revision(c('b')),
            },
            MutationOp::RestoreChanges {
                target: RevisionSelection::WorkingCopy,
                selection: whole(&["x.rs"]),
            },
            MutationOp::ResolveConflict {
                target: c('a'),
                path: "a.rs".to_owned(),
                resolution: ConflictResolution::Side(1),
            },
        ];
        for op in ops {
            let equivalent = op.cli_equivalent(&repo);
            assert!(!equivalent.exact, "{op:?}");
            let line = equivalent.to_shell();
            crate::cli::parse::parse(&CliConfig::builtin(), &line)
                .unwrap_or_else(|e| panic!("{line}: {e:?}"));
        }
        assert_eq!(
            MutationOp::KeepDivergent { keep: c('c') }
                .cli_equivalent(&repo)
                .to_shell(),
            "jj abandon 'change_id(mmmm) ~ cccccccccccc'"
        );
        assert_eq!(
            MutationOp::SquashChanges {
                from: c('a'),
                selection: partial,
                into: SquashTarget::Revision(c('b')),
            }
            .cli_equivalent(&repo)
            .to_shell(),
            r#"jj squash --from kkkk --into llll -i 'file:"src/a b.rs"'"#
        );
        let push_after_move = MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: c('a'),
            push_remote: Some("origin".to_owned()),
        }
        .cli_equivalent(&repo);
        assert_eq!(
            push_after_move.to_shell(),
            "jj bookmark set main -r kkkk && jj git push --remote origin -b main"
        );
    }

    #[test]
    fn drafts_prefill_up_to_their_destination() {
        use crate::mutations::{DraftSource, RebaseSourceMode};
        let source = |n: char| DraftSource {
            selection: c(n),
            commit_id: hex(n),
            label: String::new(),
        };
        let repo = Repo::new();
        let mut rebase = OpDraft::rebase(RebaseSourceMode::WithDescendants, vec![source('a')]);
        assert_eq!(rebase.cli_prefix(&repo), "jj rebase -s kkkk -o ");
        rebase.placement = PlacementKind::Before;
        assert_eq!(rebase.cli_prefix(&repo), "jj rebase -s kkkk -B ");
        let squash = OpDraft::squash_changes(source('b'), whole(&["a.rs"]));
        assert_eq!(
            squash.cli_prefix(&repo),
            "jj squash --from llll a.rs --into "
        );
        let merge = OpDraft::merge(source('a'));
        assert_eq!(merge.cli_prefix(&repo), "jj new kkkk ");
        let new = OpDraft::with_sources(DraftKind::New, Vec::new());
        assert_eq!(new.cli_prefix(&repo), "jj new ");
        let parallel =
            OpDraft::with_sources(DraftKind::Parallelize, vec![source('a'), source('b')]);
        assert_eq!(parallel.cli_prefix(&repo), "jj parallelize kkkk llll");
        // What the prefix plus a destination parses to is the draft's op.
        let line = rebase.cli_prefix(&repo) + "llll";
        let parsed = crate::cli::parse::parse(&CliConfig::builtin(), &line).unwrap();
        let CliCommand::Mutation(command) = parsed.command else {
            panic!("{line}")
        };
        assert_eq!(
            lower(&command, &repo),
            Ok(NativeAction::Mutate(
                rebase
                    .op_for(c('b'), PlacementKind::Before)
                    .expect("a valid target")
            ))
        );
    }

    #[test]
    fn typed_commands_know_their_draft() {
        let parsed =
            crate::cli::parse::parse(&CliConfig::builtin(), "jj rebase -s a -A b").unwrap();
        let CliCommand::Mutation(command) = parsed.command else {
            panic!()
        };
        assert_eq!(
            command.draft_kind(),
            Some((
                crate::mutations::DraftKind::Rebase {
                    mode: RebaseSourceMode::WithDescendants
                },
                PlacementKind::After
            ))
        );
    }
}
