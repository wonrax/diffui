//! What Enter will do with a command line, worked out from its parse and
//! the revsets resolved so far, and said in words for the palette's footer.

use std::ops::Range;

use diffui_core::cli::{
    CliCommand, CommandKind, LowerContext, LowerError, MutationCommand, NativeAction,
    RevisionNames, RevsetArg, RevsetDestination, UiCommand, ViewCommand,
};
use diffui_core::revset::union_revsets;
use diffui_core::{DiffRequest, MutationOp, RevisionSelection};

use super::{CommandLine, Resolution};
use crate::Diffui;
use crate::prose::Prose;

impl Diffui {
    // ── the plan ────────────────────────────────────────────────────────

    /// What Enter does with the line as it stands, in words.
    pub(crate) fn plan(&self, command: &CommandLine) -> Plan {
        let parsed = match &command.parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                return Plan {
                    kind: None,
                    words: Prose::new(),
                    state: PlanState::Blocked {
                        reason: sentence(&error.message, error.hint.as_deref()),
                        span: Some(error.span.clone()),
                    },
                };
            }
        };
        let tab = self.active();
        let context = Resolver {
            command,
            working_copy: tab
                .session
                .bookmarks
                .working_copy_commit
                .as_deref()
                .or_else(|| {
                    tab.session
                        .commits
                        .working_copy()
                        .map(|row| row.commit_id())
                }),
            push_remote: tab
                .session
                .bookmarks
                .default_push_remote()
                .map(str::to_owned),
        };
        let words = Words { ui: self, command };
        // A revset that didn't resolve stops everything, whatever the kind:
        // the CLI would refuse it just the same.
        for slot in &parsed.revsets {
            if let Some(Resolution {
                result: Err(error), ..
            }) = command.resolved.get(&slot.revset)
            {
                let span = error
                    .span
                    .clone()
                    .map(|span| {
                        let base = slot.span.start;
                        base + span.start..base + span.end.max(span.start)
                    })
                    .or(Some(slot.span.clone()));
                return Plan {
                    kind: Some(parsed.command.kind()),
                    words: Prose::new(),
                    state: PlanState::Blocked {
                        reason: sentence(&error.message, error.hint.as_deref()),
                        span,
                    },
                };
            }
        }
        let pending = parsed
            .revsets
            .iter()
            .any(|slot| !command.resolved.contains_key(&slot.revset));
        let cli_plan = |reason: Option<String>| {
            let mut words = Prose::from("Runs ")
                .code(format!("jj {}", parsed.passthrough_args.join(" ")))
                .text(" in a subprocess");
            if let Some(reason) = reason {
                words = words.text(format!(" ({reason})"));
            }
            Plan {
                kind: Some(CommandKind::Passthrough),
                words,
                state: if pending {
                    PlanState::Pending
                } else {
                    PlanState::Ready(Run::Cli {
                        args: parsed.passthrough_args.clone(),
                    })
                },
            }
        };
        let kind = Some(parsed.command.kind());
        let blocked = |error: LowerError| Plan {
            kind,
            words: Prose::new(),
            state: PlanState::Blocked {
                span: error.span(),
                reason: error.to_string(),
            },
        };
        let wait_or = |plan: Plan| {
            if pending {
                Plan {
                    state: PlanState::Pending,
                    ..plan
                }
            } else {
                plan
            }
        };
        match &parsed.command {
            CliCommand::Passthrough => cli_plan(None),
            // An operation id needs an op-log lookup diffui doesn't have; the
            // CLI resolves it exactly.
            CliCommand::Mutation(
                MutationCommand::OpRestore { .. } | MutationCommand::OpRevert { .. },
            ) => cli_plan(None),
            CliCommand::Mutation(mutation) => {
                if !tab.session.capabilities.mutate {
                    return cli_plan(None);
                }
                if pending {
                    return Plan {
                        kind,
                        words: words.mutation(mutation, None),
                        state: PlanState::Pending,
                    };
                }
                match diffui_core::cli::lower(mutation, &context) {
                    Ok(action) => Plan {
                        kind,
                        // Once lowered, the same sentence the draft banner
                        // and the activity log say for this op.
                        words: match &action {
                            NativeAction::Mutate(op) => crate::plan::op_sentence(self, op, None),
                            NativeAction::Fetch(_) => None,
                        }
                        .unwrap_or_else(|| words.mutation(mutation, Some(&action))),
                        state: PlanState::Ready(Run::Native {
                            action,
                            command: mutation.clone(),
                        }),
                    },
                    Err(LowerError::NeedsCli(reason)) => cli_plan(Some(reason)),
                    Err(error) => blocked(error),
                }
            }
            CliCommand::View(view) => {
                if pending {
                    return Plan {
                        kind,
                        words: words.view(view),
                        state: PlanState::Pending,
                    };
                }
                match resolve_view(view, &context) {
                    Ok(resolved) => wait_or(Plan {
                        kind,
                        words: words.view(view),
                        state: PlanState::Ready(Run::View(resolved)),
                    }),
                    Err(error) => blocked(error),
                }
            }
            CliCommand::OpensUi(ui_command) => {
                if !tab.session.capabilities.mutate {
                    return cli_plan(None);
                }
                if pending {
                    return Plan {
                        kind,
                        words: words.opens(ui_command),
                        state: PlanState::Pending,
                    };
                }
                let target = match ui_command {
                    UiCommand::Commit => Ok(RevisionSelection::WorkingCopy),
                    UiCommand::Describe { target }
                    | UiCommand::Split { target }
                    | UiCommand::Resolve { target, .. } => context.single(target),
                };
                match target {
                    Ok(target) => Plan {
                        kind,
                        words: words.opens(ui_command),
                        state: PlanState::Ready(Run::Ui {
                            command: ui_command.clone(),
                            target,
                        }),
                    },
                    Err(error) => blocked(error),
                }
            }
        }
    }
}
/// What Enter will do.
pub(crate) struct Plan {
    /// The badge; `None` while the line doesn't parse.
    pub(crate) kind: Option<CommandKind>,
    pub(crate) words: Prose,
    pub(crate) state: PlanState,
}

pub(crate) enum PlanState {
    Ready(Run),
    /// A revset is still resolving.
    Pending,
    /// Enter does nothing, for this reason; `span` is what to underline.
    Blocked {
        reason: String,
        span: Option<Range<usize>>,
    },
}

pub(crate) enum Run {
    View(ResolvedView),
    Native {
        action: NativeAction,
        command: MutationCommand,
    },
    Ui {
        command: UiCommand,
        target: RevisionSelection,
    },
    Cli {
        args: Vec<String>,
    },
}

/// A view command with its single-revision arguments checked.
pub(crate) enum ResolvedView {
    Diff(DiffRequest),
    /// `None`: the configured default log revset.
    Log(Option<String>),
    Evolog(RevisionSelection),
    OpLog,
    Annotate {
        revision: RevisionSelection,
        path: String,
    },
}

fn resolve_view(view: &ViewCommand, context: &Resolver<'_>) -> Result<ResolvedView, LowerError> {
    let text = |args: &[RevsetArg]| args.iter().map(|arg| arg.revset.clone()).collect();
    Ok(match view {
        ViewCommand::Diff { revisions } => {
            ResolvedView::Diff(DiffRequest::Revisions(text(revisions)))
        }
        ViewCommand::DiffRange { from, to } | ViewCommand::Interdiff { from, to } => {
            context.single(from)?;
            context.single(to)?;
            ResolvedView::Diff(DiffRequest::FromTo {
                from: Some(from.revset.clone()),
                to: Some(to.revset.clone()),
                interdiff: matches!(view, ViewCommand::Interdiff { .. }),
            })
        }
        ViewCommand::Show { revision } => {
            context.single(revision)?;
            ResolvedView::Diff(DiffRequest::Revisions(vec![revision.revset.clone()]))
        }
        ViewCommand::Log { revisions } => ResolvedView::Log(
            revisions
                .as_ref()
                .map(|revisions| union_revsets(&text(revisions))),
        ),
        ViewCommand::Evolog { revision } => ResolvedView::Evolog(context.single(revision)?),
        ViewCommand::OpLog { .. } => ResolvedView::OpLog,
        ViewCommand::FileAnnotate { path, revision } => ResolvedView::Annotate {
            revision: context.single(revision)?,
            path: path.value.clone(),
        },
    })
}

/// Answers lowering from the revsets the command line already resolved.
struct Resolver<'a> {
    command: &'a CommandLine,
    working_copy: Option<&'a str>,
    push_remote: Option<String>,
}

impl Resolver<'_> {
    fn selection(&self, hex: &str) -> RevisionSelection {
        if self.working_copy == Some(hex) {
            RevisionSelection::WorkingCopy
        } else {
            RevisionSelection::Commit(hex.to_owned())
        }
    }

    fn single(&self, arg: &RevsetArg) -> Result<RevisionSelection, LowerError> {
        let mut found = self
            .resolve(&arg.revset)
            .map_err(|message| LowerError::Resolve {
                text: arg.revset.clone(),
                span: arg.span.clone(),
                message,
            })?;
        if found.len() != 1 {
            return Err(LowerError::NotSingle {
                revset: arg.revset.clone(),
                span: arg.span.clone(),
                count: found.len(),
            });
        }
        Ok(found.remove(0))
    }
}

impl LowerContext for Resolver<'_> {
    fn resolve(&self, revset: &str) -> Result<Vec<RevisionSelection>, String> {
        if revset.trim() == "@" {
            return Ok(vec![RevisionSelection::WorkingCopy]);
        }
        match self.command.resolved.get(revset).map(|r| &r.result) {
            Some(Ok(found)) if found.commits.len() < found.count => Err(format!(
                "Revset `{revset}` matches {} revisions, more than diffui acts on at once",
                found.count
            )),
            Some(Ok(found)) => Ok(found
                .commits
                .iter()
                .map(|hex| self.selection(hex))
                .collect()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err(format!("Revset `{revset}` is still resolving")),
        }
    }

    fn default_push_remote(&self) -> Option<String> {
        self.push_remote.clone()
    }
}

// ── words ───────────────────────────────────────────────────────────────

/// Builds the plan's sentence, naming revisions the way the graph does.
struct Words<'a> {
    ui: &'a Diffui,
    command: &'a CommandLine,
}

impl Words<'_> {
    /// A revset argument as a noun: a bookmark by name, one revision by its
    /// change id, several as the revset and how many it holds.
    fn arg(&self, arg: &RevsetArg) -> Prose {
        let text = arg.revset.trim();
        if arg.is_implicit() || text == "@" {
            return Prose::new().rev("@");
        }
        let session = &self.ui.active().session;
        if session.bookmarks.entry(text).is_some() {
            return Prose::new().bookmark(text);
        }
        match self.command.resolved.get(&arg.revset).map(|r| &r.result) {
            Some(Ok(found)) if found.count == 1 => {
                let hex = found
                    .commits
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default();
                // `trunk()` reads better as the bookmark it lands on.
                if text.contains('(')
                    && let Some(row) = session.commits.find_by_commit_id(hex)
                    && let [bookmark] = row.bookmarks()
                {
                    return Prose::new().bookmark(bookmark.clone());
                }
                // Named the way every jj command diffui shows names it.
                let name = RevisionNames::change_id(&session.commits, hex);
                Prose::new().rev(name.unwrap_or_else(|| text.to_owned()))
            }
            Some(Ok(found)) => Prose::new()
                .text(found.count_label())
                .text(" (")
                .code(text)
                .text(")"),
            _ => Prose::new().code(text),
        }
    }

    fn args(&self, args: &[RevsetArg]) -> Prose {
        match args {
            [] => Prose::new().rev("@"),
            [one] => self.arg(one),
            several => {
                let mut prose = Prose::new();
                for (i, arg) in several.iter().enumerate() {
                    if i > 0 {
                        prose = prose.text(if i + 1 == several.len() {
                            " and "
                        } else {
                            ", "
                        });
                    }
                    prose = prose.then(self.arg(arg));
                }
                prose
            }
        }
    }

    fn destination(&self, destination: &RevsetDestination) -> Prose {
        match destination {
            RevsetDestination::Onto(target) => Prose::from(" onto ").then(self.arg(target)),
            RevsetDestination::After(target) => Prose::from(" after ").then(self.arg(target)),
            RevsetDestination::Before(target) => Prose::from(" before ").then(self.arg(target)),
            RevsetDestination::Between { after, before } => Prose::from(" between ")
                .then(self.arg(after))
                .text(" and ")
                .then(self.arg(before)),
        }
    }

    fn view(&self, view: &ViewCommand) -> Prose {
        match view {
            ViewCommand::Diff { revisions } => match revisions.as_slice() {
                [] => Prose::from("Shows the diff of ").rev("@"),
                [one] if self.single(one) => Prose::from("Shows the diff of ").then(self.arg(one)),
                _ => Prose::from("Shows the combined diff of ").then(self.args(revisions)),
            },
            ViewCommand::DiffRange { from, to } => Prose::from("Compares ")
                .then(self.arg(from))
                .text(" with ")
                .then(self.arg(to)),
            ViewCommand::Interdiff { from, to } => Prose::from("Shows the interdiff of ")
                .then(self.arg(from))
                .text(" and ")
                .then(self.arg(to)),
            ViewCommand::Show { revision } => Prose::from("Shows ").then(self.arg(revision)),
            ViewCommand::Log {
                revisions: Some(revisions),
            } => Prose::from("Shows ")
                .then(self.args(revisions))
                .text(" in the log"),
            ViewCommand::Log { revisions: None } => Prose::from("Shows the default log"),
            ViewCommand::Evolog { revision } => Prose::from("Shows how ")
                .then(self.arg(revision))
                .text(" evolved"),
            ViewCommand::OpLog { .. } => Prose::from("Shows the operation log"),
            ViewCommand::FileAnnotate { path, revision } => Prose::from("Annotates ")
                .path(path.value.clone())
                .text(" at ")
                .then(self.arg(revision)),
        }
    }

    fn single(&self, arg: &RevsetArg) -> bool {
        arg.is_implicit()
            || matches!(
                self.command.resolved.get(&arg.revset).map(|r| &r.result),
                Some(Ok(found)) if found.count == 1
            )
    }

    fn opens(&self, command: &UiCommand) -> Prose {
        match command {
            UiCommand::Describe { target } => {
                Prose::from("Opens the description editor for ").then(self.arg(target))
            }
            UiCommand::Commit => Prose::from("Opens the commit editor for ").rev("@"),
            UiCommand::Split { target } => Prose::from("Shows ")
                .then(self.arg(target))
                .text(" to pick the changes to split off"),
            UiCommand::Resolve { target, .. } => {
                Prose::from("Shows the conflicts in ").then(self.arg(target))
            }
        }
    }

    fn mutation(&self, command: &MutationCommand, action: Option<&NativeAction>) -> Prose {
        use diffui_core::mutations::RebaseSourceMode;
        let quoted = |name: &str| Prose::new().bookmark(name);
        match command {
            MutationCommand::New { parents } if parents.len() > 1 => {
                Prose::from("New merge of ").then(self.args(parents))
            }
            MutationCommand::New { parents } => {
                Prose::from("New change on ").then(self.args(parents))
            }
            MutationCommand::NewAt { destination } => {
                Prose::from("New change").then(self.destination(destination))
            }
            MutationCommand::Edit { target } => Prose::from("Edit ").then(self.arg(target)),
            MutationCommand::Abandon { targets } => {
                Prose::from("Abandon ").then(self.args(targets))
            }
            MutationCommand::Describe { target, message } => Prose::from("Describe ")
                .then(self.arg(target))
                .text(" as ")
                .code(first_line(message)),
            MutationCommand::Commit { message } => Prose::from("Commit ")
                .rev("@")
                .text(" as ")
                .code(first_line(message)),
            MutationCommand::Rebase {
                mode,
                sources,
                destination,
            } => {
                let what = match mode {
                    RebaseSourceMode::Revisions => self.args(sources),
                    RebaseSourceMode::WithDescendants => {
                        self.args(sources).text(" and descendants")
                    }
                    RebaseSourceMode::Branch => {
                        Prose::from("the branch of ").then(self.args(sources))
                    }
                };
                Prose::from("Rebase ")
                    .then(what)
                    .then(self.destination(destination))
            }
            MutationCommand::Squash { from, into: None } => Prose::from("Squash ")
                .then(self.args(from))
                .text(" into its parent"),
            MutationCommand::Squash {
                from,
                into: Some(into),
            } => Prose::from("Squash ")
                .then(self.args(from))
                .text(" into ")
                .then(self.arg(into)),
            MutationCommand::Duplicate {
                targets,
                destination,
            } => {
                let words = Prose::from("Duplicate ").then(self.args(targets));
                match destination {
                    Some(destination) => words.then(self.destination(destination)),
                    None => words,
                }
            }
            MutationCommand::Parallelize { targets } => Prose::from("Make ")
                .then(self.args(targets))
                .text(" siblings"),
            MutationCommand::Absorb { from } => Prose::from("Absorb ")
                .then(self.arg(from))
                .text(" into the revisions it touches"),
            MutationCommand::Metaedit { targets, .. } => {
                Prose::from("Update the author of ").then(self.args(targets))
            }
            MutationCommand::BookmarkCreate { name, to } => Prose::from("Create ")
                .then(quoted(&name.value))
                .text(" at ")
                .then(self.arg(to)),
            MutationCommand::BookmarkSet { name, to, .. } => Prose::from("Move ")
                .then(quoted(&name.value))
                .text(" to ")
                .then(self.arg(to)),
            MutationCommand::BookmarkDelete { name } => {
                Prose::from("Delete ").then(quoted(&name.value))
            }
            MutationCommand::BookmarkForget { name } => {
                Prose::from("Forget ").then(quoted(&name.value))
            }
            MutationCommand::BookmarkRename { old, new } => Prose::from("Rename ")
                .then(quoted(&old.value))
                .text(" to ")
                .then(quoted(&new.value)),
            MutationCommand::BookmarkTrack { name, remote } => {
                Prose::from("Track ").bookmark(format!("{name}@{remote}"))
            }
            MutationCommand::BookmarkUntrack { name, remote } => {
                Prose::from("Untrack ").bookmark(format!("{name}@{remote}"))
            }
            MutationCommand::Push { .. } => match action {
                Some(NativeAction::Mutate(MutationOp::Push(request))) => {
                    crate::bookmarks::push_label(request).then(Prose::from(if request.confirm {
                        ", after a look at what moves"
                    } else {
                        ""
                    }))
                }
                _ => Prose::from("Push to the remote"),
            },
            MutationCommand::Fetch => Prose::from("Fetch from the configured remotes"),
            MutationCommand::FetchAll => Prose::from("Fetch from every remote"),
            MutationCommand::FetchBranch { remote, branch } => Prose::from("Fetch ")
                .bookmark(branch.clone())
                .text(" from ")
                .remote(remote.clone()),
            MutationCommand::Undo => Prose::from("Undo the last operation"),
            MutationCommand::Redo => Prose::from("Redo the last undone operation"),
            MutationCommand::OpRestore { operation } => {
                Prose::from("Restore to operation ").op(operation.value.clone())
            }
            MutationCommand::OpRevert { .. } => Prose::from("Revert an operation"),
        }
    }
}

/// jj's message and its hint as one line.
pub(crate) fn sentence(message: &str, hint: Option<&str>) -> String {
    match hint {
        Some(hint) if message.ends_with(['.', '?', '!', ':']) => format!("{message} {hint}"),
        Some(hint) => format!("{message}. {hint}"),
        None => message.to_owned(),
    }
}

fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or("");
    if message.lines().count() > 1 {
        format!("{line}\u{2026}")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use diffui_core::cli::JjCli;
    use diffui_core::revset::{RevsetError, RevsetMatch};

    fn plan_for(line: &str, resolved: &[(&str, Result<Vec<&str>, &str>)]) -> (Plan, String) {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/plan");
        ui.active = 0;
        ui.active_mut().session.capabilities.mutate = true;
        let mut command = CommandLine::new(&JjCli::default(), line);
        for (revset, result) in resolved {
            let result = match result {
                Ok(commits) => Ok(RevsetMatch {
                    commits: commits.iter().map(|c| (*c).to_owned()).collect(),
                    count: commits.len(),
                    count_exact: true,
                }),
                Err(message) => Err(RevsetError::new(*message)),
            };
            command.resolved.insert(
                (*revset).to_owned(),
                Resolution {
                    result,
                    set: HashSet::new(),
                },
            );
        }
        let plan = ui.plan(&command);
        let words = plan.words.to_string();
        (plan, words)
    }

    #[test]
    fn the_plan_says_what_enter_does() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        // Still resolving: Enter waits.
        let (plan, _) = plan_for("jj rebase -s x -o y", &[("x", Ok(vec![&a]))]);
        assert!(matches!(plan.state, PlanState::Pending));
        // Resolved: a native rebase, worded.
        let (plan, words) = plan_for(
            "jj rebase -s x -o y",
            &[("x", Ok(vec![&a])), ("y", Ok(vec![&b]))],
        );
        assert_eq!(plan.kind, Some(CommandKind::Mutation));
        assert!(matches!(
            plan.state,
            PlanState::Ready(Run::Native {
                action: NativeAction::Mutate(MutationOp::Rebase { .. }),
                ..
            })
        ));
        assert!(
            words.starts_with("Rebase ") && words.contains(" and descendants onto "),
            "{words}"
        );
        // A revset jj refuses blocks Enter, pointing at it.
        let (plan, _) = plan_for(
            "jj log -r nope",
            &[("nope", Err("Revision `nope` doesn't exist"))],
        );
        assert!(
            matches!(plan.state, PlanState::Blocked { span: Some(ref span), .. } if *span == (10..14))
        );
        // A merge destination diffui can't place runs through the CLI.
        let (plan, _) = plan_for(
            "jj rebase -s x -o y",
            &[("x", Ok(vec![&a])), ("y", Ok(vec![&a, &b]))],
        );
        assert_eq!(plan.kind, Some(CommandKind::Passthrough));
        assert!(matches!(plan.state, PlanState::Ready(Run::Cli { .. })));
        // `describe` without `-m` opens the editor on its one revision.
        let (plan, _) = plan_for("jj describe x", &[("x", Ok(vec![&a]))]);
        assert!(matches!(plan.state, PlanState::Ready(Run::Ui { .. })));
        // Anything else is the CLI's.
        let (plan, words) = plan_for("jj fix", &[]);
        assert!(matches!(plan.state, PlanState::Ready(Run::Cli { ref args }) if args == &["fix"]));
        assert_eq!(words, "Runs jj fix in a subprocess");
    }

    #[test]
    fn jj_hints_join_their_message_as_a_sentence() {
        assert_eq!(
            sentence(
                "Function `trnk` doesn't exist",
                Some("Did you mean `trunk`?")
            ),
            "Function `trnk` doesn't exist. Did you mean `trunk`?"
        );
        assert_eq!(
            sentence("Cannot diff revsets with gaps in.", Some("x")),
            "Cannot diff revsets with gaps in. x"
        );
        assert_eq!(sentence("plain", None), "plain");
    }
}
