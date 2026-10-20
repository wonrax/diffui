//! Menu construction for [`Diffui`]: the revision context-menu tree, the
//! toolbar fetch/revset dropdowns, running a picked command, and lowering to
//! the macOS native `NSMenu`. Split out of `update.rs`; these read only
//! already-loaded `Session` state, so menus open instantly with no repo I/O.
//!
//! A menu row names a registry command and the argument to run it with. The
//! label and the chord hint come from the registry unless the row is one of a
//! generated set (a bookmark name, a remote branch) that the registry cannot
//! know. Nothing here decides *what* a pick does — [`Diffui::run_command`]
//! re-checks the command's `enabled` at pick time and performs its
//! [`crate::Action`].

use super::*;
use crate::commands::{self, CommandId};
use crate::prose::Prose;

impl Diffui {
    /// A menu row for `command`, labelled and chord-hinted from the registry.
    pub(crate) fn menu_item(&self, command: CommandId, arg: CommandArg) -> menu::MenuEntry {
        let entry = commands::command(command);
        if let Some(Err(reason)) = entry.map(|entry| (entry.enabled)(self, &arg)) {
            return menu::MenuEntry::Disabled {
                label: entry.map(|c| c.label.to_owned()).unwrap_or_default(),
                reason: Some(reason.to_owned()),
            };
        }
        menu::MenuEntry::Item {
            label: entry.map(|c| c.label.to_owned()).unwrap_or_default(),
            detail: self
                .keymap
                .chord_for(Context::Base, command)
                .map(Chord::display),
            emphasized: false,
            command,
            arg,
        }
    }

    /// A menu row for `command` under a label the registry can't supply — one
    /// of a generated set (a bookmark, a remote branch, a preset).
    pub(crate) fn menu_row(
        &self,
        label: impl Into<String>,
        command: CommandId,
        arg: CommandArg,
    ) -> menu::MenuEntry {
        if let Some(Err(reason)) =
            commands::command(command).map(|entry| (entry.enabled)(self, &arg))
        {
            return menu::MenuEntry::Disabled {
                label: label.into(),
                reason: Some(reason.to_owned()),
            };
        }
        menu::MenuEntry::Item {
            label: label.into(),
            detail: None,
            emphasized: false,
            command,
            arg,
        }
    }

    /// Build the revision context menu's entry tree from the already-loaded
    /// bookmarks / commit graph (so it opens instantly, no repo read).
    /// Shared by the macOS native popup and the iced overlay; author/committer/
    /// description copies carry only an in-memory fallback, with the live value
    /// read on demand when picked.
    pub(crate) fn revision_menu_tree(&self, selection: &RevisionSelection) -> Vec<menu::MenuEntry> {
        use menu::MenuEntry;

        // A live multi-selection (several marked rows, the clicked one among
        // them) swaps the per-revision menu for the batch menu — marking rows
        // only exists to act on all of them at once. Marks whose rows left
        // the loaded graph (reload, revset change) are ignored rather than
        // silently acted on.
        let marked: Vec<&str> = self
            .active()
            .revision_multi_selection
            .iter()
            .map(String::as_str)
            .filter(|id| {
                self.active()
                    .session
                    .commits
                    .find_by_commit_id(id)
                    .is_some()
            })
            .collect();
        let clicked_id = match selection {
            RevisionSelection::WorkingCopy => self
                .active()
                .session
                .commits
                .working_copy()
                .map(|row| row.commit_id().to_owned()),
            RevisionSelection::Commit(hex) => Some(hex.clone()),
        };
        if marked.len() > 1 && clicked_id.as_deref().is_some_and(|id| marked.contains(&id)) {
            return self.batch_menu_tree(crate::revops::marked_selections(self));
        }

        let on = || CommandArg::Revision(selection.clone());
        let mut top = vec![
            self.menu_item("revision.edit-description", on()),
            MenuEntry::Separator,
            self.menu_item("revision.new-child", on()),
            self.menu_item("revision.edit", on()),
        ];
        top.extend(self.revops_menu_rows(selection));
        top.extend([
            MenuEntry::Separator,
            // History surgery, grouped: multi-variant ops fold into submenus;
            // the "…" leaves enter target mode (pick the destination on the
            // graph, or drag a row directly — same machinery).
            MenuEntry::Submenu {
                label: "Rebase".to_owned(),
                items: vec![
                    self.menu_item("revision.rebase.start", on()),
                    self.menu_item("revision.rebase.descendants.start", on()),
                    self.menu_item("revision.rebase.branch.start", on()),
                    self.menu_item("revision.rebase.onto-trunk", on()),
                ],
            },
            MenuEntry::Submenu {
                label: "Squash".to_owned(),
                items: vec![
                    self.menu_item("revision.squash-into-parent", on()),
                    self.menu_item("revision.squash.start", on()),
                ],
            },
            self.menu_item("revision.merge.start", on()),
            self.menu_item("revision.parallelize.start", on()),
        ]);
        top.extend(self.revops_surgery_rows(selection));
        top.extend([
            self.menu_item("revision.absorb", on()),
            self.menu_item("revision.abandon", on()),
            MenuEntry::Separator,
            self.menu_item(
                "revision.browse-source",
                CommandArg::Browse {
                    revision: selection.clone(),
                    path: None,
                },
            ),
            MenuEntry::Submenu {
                label: "Compare".to_owned(),
                items: vec![
                    self.menu_item("compare.with", on()),
                    self.menu_item("compare.with-working-copy", on()),
                    self.menu_item("diff.since-trunk", on()),
                ],
            },
        ]);
        if crate::history_panel::available(self) {
            top.push(self.menu_item("history.evolog", on()));
        }

        // Copy revision metadata — values come from the loaded graph row.
        let copy_fields = {
            let row = match selection {
                RevisionSelection::WorkingCopy => self.active().session.commits.working_copy(),
                RevisionSelection::Commit(hex) => {
                    self.active().session.commits.find_by_commit_id(hex)
                }
            };
            row.map(|row| {
                (
                    row.change_id().to_owned(),
                    row.commit_id().to_owned(),
                    row.description().to_owned(),
                    row.author().to_owned(),
                    row.bookmarks().to_vec(),
                )
            })
        };
        if let Some((change_id, commit_id, description, author, bookmarks)) = copy_fields {
            let copy_text = |label: &str, value: String| {
                self.menu_row(label, "revision.copy-text", CommandArg::Text(value))
            };
            let copy_detail = |label: &str, field: DetailField, fallback: String| {
                self.menu_row(
                    label,
                    "revision.copy-detail",
                    CommandArg::Detail {
                        revision: selection.clone(),
                        field,
                        fallback,
                    },
                )
            };
            let mut copy_items = vec![
                copy_text("Revision ID", change_id),
                copy_text("Commit hash", commit_id),
            ];
            match bookmarks.len() {
                0 => {}
                1 => copy_items.push(copy_text("Bookmark", bookmarks[0].clone())),
                _ => {
                    let subs = bookmarks
                        .iter()
                        .map(|name| copy_text(name, name.clone()))
                        .collect();
                    copy_items.push(MenuEntry::Submenu {
                        label: "Bookmark".to_owned(),
                        items: subs,
                    });
                }
            }
            if !description.is_empty() {
                copy_items.push(copy_detail(
                    "Description",
                    DetailField::Description,
                    description,
                ));
            }
            copy_items.push(copy_detail("Author", DetailField::Author, author.clone()));
            copy_items.push(copy_detail("Committer", DetailField::Committer, author));
            top.push(MenuEntry::Separator);
            top.push(MenuEntry::Submenu {
                label: "Copy".to_owned(),
                items: copy_items,
            });
        }

        // Move a local bookmark onto this revision, nearest-first. A
        // conflicted bookmark stays offered — `jj bookmark set` onto a
        // revision is exactly how a conflict resolves — wearing its `??` so
        // the pick doubles as the resolution it is.
        let mut moves: Vec<(String, String, bool)> = self
            .active()
            .session
            .bookmarks
            .bookmarks
            .iter()
            .filter_map(|b| {
                b.local_target()
                    .map(|t| (b.name.clone(), t.to_owned(), b.is_conflicted()))
            })
            .collect();
        moves.sort();
        let move_reference = match selection {
            RevisionSelection::Commit(hex) => Some(hex.clone()),
            RevisionSelection::WorkingCopy => {
                self.active().session.bookmarks.working_copy_commit.clone()
            }
        };
        self.sort_by_proximity(&mut moves, move_reference.as_deref(), |(_, t, _)| {
            t.as_str()
        });
        let move_items: Vec<MenuEntry> = moves
            .iter()
            .map(|(name, _target, conflicted)| {
                let label = if *conflicted {
                    format!("{name}??")
                } else {
                    name.clone()
                };
                self.menu_row(
                    label,
                    "bookmark.move",
                    CommandArg::Bookmark {
                        name: name.clone(),
                        remote: None,
                        to: Some(selection.clone()),
                    },
                )
            })
            .collect();
        // Same list, but landing the move also pushes the bookmark to its
        // tracked remote — "advance main and publish it" as one pick. Kept a
        // sibling submenu rather than a per-bookmark verb submenu so the
        // common plain move stays one level deep. A bookmark no remote tracks
        // yet goes to the default push remote, which creates it there; the
        // submenu hides entirely when none qualify instead of sitting disabled
        // in every menu of a remote-less repo. Conflicted bookmarks are omitted too:
        // the move is a conflict *resolution* — publishing it in the same
        // gesture, typically right after a force-push surprise, deserves a
        // deliberate separate push once the resolved state looks right.
        let move_push_items: Vec<MenuEntry> = moves
            .iter()
            .filter(|(_, _, conflicted)| !conflicted)
            .filter_map(|(name, _target, _)| {
                let (remote, new) = self.move_push_remote(name)?;
                // The remote rides in the detail column rather than an arrow
                // glyph in the label: `\u{2192}` renders as a fallback-font
                // blob in plenty of UI fonts.
                Some(menu::MenuEntry::Item {
                    label: name.clone(),
                    detail: Some(if new {
                        format!("{remote} (new)")
                    } else {
                        remote.clone()
                    }),
                    emphasized: false,
                    command: "bookmark.move",
                    arg: CommandArg::Bookmark {
                        name: name.clone(),
                        remote: Some(remote),
                        to: Some(selection.clone()),
                    },
                })
            })
            .collect();
        top.push(MenuEntry::Separator);
        top.extend(self.revision_bookmark_rows(selection));
        top.push(if move_items.is_empty() {
            MenuEntry::Disabled {
                label: "Move bookmark here".to_owned(),
                reason: None,
            }
        } else {
            MenuEntry::Submenu {
                label: "Move bookmark here".to_owned(),
                items: move_items,
            }
        });
        if !move_push_items.is_empty() {
            top.push(MenuEntry::Submenu {
                label: "Move bookmark here & push".to_owned(),
                items: move_push_items,
            });
        }

        // Per-bookmark actions for bookmarks sitting on this revision.
        let target_hex: Option<&str> = match selection {
            RevisionSelection::Commit(hex) => Some(hex.as_str()),
            RevisionSelection::WorkingCopy => self
                .active()
                .session
                .bookmarks
                .working_copy_commit
                .as_deref(),
        };
        let mut bookmark_items: Vec<MenuEntry> = Vec::new();
        if let Some(hex) = target_hex {
            for entry in &self.active().session.bookmarks.bookmarks {
                // Any side of a conflicted bookmark counts as sitting here —
                // its `??` chip shows on every side, so the menu must too.
                if entry.local_targets.iter().any(|t| t == hex) {
                    let mut sub = Vec::new();
                    if entry.is_conflicted() {
                        // jj refuses to push a conflicted bookmark; what it
                        // wants is a resolution — `jj bookmark set` onto one
                        // side — so that's the action offered in its place.
                        sub.push(self.menu_row(
                            "Set here (resolve conflict)",
                            "bookmark.move",
                            CommandArg::Bookmark {
                                name: entry.name.clone(),
                                remote: None,
                                to: Some(RevisionSelection::Commit(hex.to_owned())),
                            },
                        ));
                    }
                    sub.extend(self.local_bookmark_rows(entry));
                    let label = if entry.is_conflicted() {
                        format!("{}??", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    bookmark_items.push(MenuEntry::Submenu { label, items: sub });
                }
                for remote_ref in &entry.remotes {
                    // Untracked remote bookmarks can be tracked; a tracked
                    // one whose local bookmark was deleted can have that
                    // deletion pushed. A tracked one with a live local
                    // bookmark is handled through the local bookmark.
                    let actionable = !remote_ref.tracked || entry.local_targets.is_empty();
                    if remote_ref.target.as_str() == hex && actionable {
                        bookmark_items.push(MenuEntry::Submenu {
                            label: format!("{}@{}", entry.name, remote_ref.remote),
                            items: self.remote_bookmark_rows(entry, remote_ref),
                        });
                    }
                }
            }
        }
        if !bookmark_items.is_empty() {
            top.push(MenuEntry::Separator);
            top.append(&mut bookmark_items);
        }

        top
    }

    /// The toolbar fetch menu's entries: "Fetch all remotes" + one row per known
    /// remote branch (`name@remote`), nearest-first. Non-macOS only — the iced
    /// overlay's builder; macOS builds its fetch menu natively in
    /// `open_toolbar_menu`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn fetch_menu_entries(&self) -> Vec<menu::MenuEntry> {
        use menu::MenuEntry;
        let mut items = vec![
            MenuEntry::Item {
                label: "Fetch".to_owned(),
                detail: None,
                emphasized: true,
                command: "repo.fetch",
                arg: CommandArg::Fetch(FetchTarget::Default),
            },
            MenuEntry::Item {
                label: "Fetch all remotes".to_owned(),
                detail: None,
                emphasized: false,
                command: "repo.fetch",
                arg: CommandArg::Fetch(FetchTarget::AllRemotes),
            },
        ];
        let branches = self.remote_branches_by_proximity();
        if !branches.is_empty() {
            items.push(MenuEntry::Separator);
            for (branch, remote) in branches {
                items.push(self.menu_row(
                    format!("{branch}@{remote}"),
                    "repo.fetch",
                    CommandArg::Fetch(FetchTarget::RemoteBranch { remote, branch }),
                ));
            }
        }
        items
    }

    /// The toolbar revset menu's entries: each `label` with its `expr` shown as
    /// the right-aligned detail. Non-macOS only — the iced overlay's builder;
    /// macOS builds its revset menu natively in `open_toolbar_menu`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn revset_menu_entry_tree(&self) -> Vec<menu::MenuEntry> {
        self.revset_menu_entries()
            .into_iter()
            .map(|(label, expr)| menu::MenuEntry::Item {
                label,
                detail: Some(expr.clone()),
                emphasized: false,
                command: "repo.set-revset",
                arg: CommandArg::Revset(expr),
            })
            .collect()
    }

    /// Run the registry command `id` with `arg`.
    ///
    /// The single funnel every non-keyboard producer goes through. `enabled` is
    /// evaluated *here*, not when the surface was built, so a menu that has been
    /// open across a graph reload can no longer act on a row that reload hid.
    pub(crate) fn run_command(&mut self, id: CommandId, arg: CommandArg) -> Task<Message> {
        let Some(command) = commands::command(id) else {
            return Task::none();
        };
        if (command.enabled)(self, &arg).is_err() {
            return Task::none();
        }
        match (command.build)(self, arg) {
            Some(action) => self.perform(action),
            None => Task::none(),
        }
    }

    /// Wrap `op` in an activity and send it to the repository actor. Every
    /// mutation entry point — context menu, target-mode confirm, drag & drop —
    /// funnels through here so labels and guards can't drift apart.
    pub(crate) fn start_mutation_op(&mut self, op: mutations::MutationOp) -> Task<Message> {
        use mutations::MutationOp;
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        // Surface the mutation as an activity (push captures its remote output).
        let label: Prose = match &op {
            MutationOp::New { .. } => "New change".into(),
            MutationOp::Edit { .. } => "Edit".into(),
            MutationOp::Abandon { targets } => {
                if targets.len() == 1 {
                    "Abandon".into()
                } else {
                    format!("Abandon {} revisions", targets.len()).into()
                }
            }
            MutationOp::Describe { .. } => "Update description".into(),
            MutationOp::Rebase { sources, .. } => {
                if sources.len() == 1 {
                    "Rebase".into()
                } else {
                    format!("Rebase {} revisions", sources.len()).into()
                }
            }
            MutationOp::Squash { .. } => "Squash".into(),
            MutationOp::Merge { .. } => "New merge".into(),
            MutationOp::Duplicate { .. } => "Duplicate".into(),
            MutationOp::Absorb { .. } => "Absorb".into(),
            MutationOp::MoveBookmark {
                name,
                push_remote: Some(remote),
                ..
            } => Prose::from("Move ")
                .bookmark(name)
                .text(" + push to ")
                .remote(remote),
            MutationOp::MoveBookmark { name, .. } => Prose::from("Move bookmark ").bookmark(name),
            MutationOp::DeleteBookmark { name } => Prose::from("Delete bookmark ").bookmark(name),
            MutationOp::TrackBookmark { name, remote } => {
                Prose::from("Track ").bookmark(format!("{name}@{remote}"))
            }
            MutationOp::PushBookmark { name, remote } => Prose::from("Push ")
                .bookmark(name)
                .text(" to ")
                .remote(remote),
            MutationOp::Undo { operation_id: None } => "Undo".into(),
            MutationOp::Undo {
                operation_id: Some(_),
            } => "Revert operation".into(),
            MutationOp::Redo => "Redo".into(),
            MutationOp::CreateBookmark { name, .. } => {
                Prose::from("Create bookmark ").bookmark(name)
            }
            MutationOp::RenameBookmark { old, new } => Prose::from("Rename ")
                .bookmark(old)
                .text(" to ")
                .bookmark(new),
            MutationOp::ForgetBookmark { name } => Prose::from("Forget bookmark ").bookmark(name),
            MutationOp::UntrackBookmark { name, remote } => {
                Prose::from("Untrack ").bookmark(format!("{name}@{remote}"))
            }
            MutationOp::Push(request) => crate::bookmarks::push_label(request),
            MutationOp::Commit { .. } => "Commit".into(),
            MutationOp::NewAt { .. } => "New change".into(),
            MutationOp::Parallelize { targets } => {
                format!("Parallelize {} revisions", targets.len()).into()
            }
            MutationOp::DuplicateTo { targets, .. } => match targets.len() {
                1 => "Duplicate".into(),
                n => format!("Duplicate {n} revisions").into(),
            },
            MutationOp::KeepDivergent { .. } => "Resolve divergent change".into(),
            MutationOp::Metaedit { .. } => "Update author".into(),
            MutationOp::Split { selection, .. } => {
                Prose::from("Split off ").then(crate::changes::selection_prose(selection))
            }
            MutationOp::SquashChanges { selection, .. } => {
                Prose::from("Move ").then(crate::changes::selection_prose(selection))
            }
            MutationOp::RestoreChanges { selection, .. } => {
                Prose::from("Discard ").then(crate::changes::selection_prose(selection))
            }
            MutationOp::ResolveConflict { path, .. } => Prose::from("Resolve ").path(path),
            MutationOp::RestoreOperation { .. } => "Restore operation".into(),
            MutationOp::RevertOperation { .. } => "Revert operation".into(),
        };
        // Only pushes report real progress (git transfer); the rest are quick
        // local ops, so they stay indeterminate.
        let determinate = matches!(
            op,
            MutationOp::PushBookmark { .. }
                | MutationOp::MoveBookmark {
                    push_remote: Some(_),
                    ..
                }
                | MutationOp::Push(_)
        );
        // A batch abandon consumes the marked rows — the marks (and their
        // wash) mustn't outlive the pick.
        if crate::revops::consumes_marks(self, &op) {
            self.active_mut().revision_multi_selection.clear();
        }
        let (activity_id, _) = self.begin_activity(tab_id, label, determinate);
        let pending = PendingMutation {
            op,
            tab_id,
            activity_id,
            allow_immutable: false,
        };
        // jj CLI parity: `jj bookmark set` refuses a backwards/sideways move
        // without `--allow-backwards`. Check ancestry first — the result either
        // runs the move directly (fast-forward) or raises a confirmation
        // dialog. The activity sits Queued meanwhile so the
        // action stays visible (and is resolved on cancel).
        if let MutationOp::MoveBookmark { name, to, .. } = &pending.op {
            if let Some(log) = self.activity_log_for(pending.tab_id) {
                log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
            }
            let (name, to) = (name.clone(), to.clone());
            let Some(state) = self.tab_mut(tab_id) else {
                return Task::none();
            };
            let (job, effects) = state.session.bookmark_check(name, to);
            state.jobs.bookmark_check = Some((job, pending));
            return self.run_effects(tab_id, effects);
        }
        self.run_mutation_checking_wc_push(pending)
    }

    /// Raise a confirmation for `op` and run it on accept. Only abandon asks
    /// today (reached from a single key or the palette); anything else runs
    /// straight away. The activity row is opened now and sits queued, so a
    /// cancel resolves it the same way the other confirmations do.
    pub(crate) fn confirm_mutation_op(&mut self, op: mutations::MutationOp) -> Task<Message> {
        use mutations::MutationOp;
        let MutationOp::Abandon { targets } = &op else {
            return self.start_mutation_op(op);
        };
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        if !self.active().session.capabilities.mutate || targets.is_empty() {
            return Task::none();
        }
        let (title, label, body) = match targets.as_slice() {
            [target] => (
                Prose::from("Abandon ")
                    .then(self.revision_prose(target))
                    .text("?"),
                "Abandon".to_owned(),
                "Its changes are dropped and its descendants move onto its parent. Undo \
                 brings it back.",
            ),
            _ => (
                format!("Abandon {} revisions?", targets.len()).into(),
                format!("Abandon {} revisions", targets.len()),
                "Their changes are dropped and their descendants move onto their parents. \
                 Undo brings them back.",
            ),
        };
        let body = Prose::from(body);
        let (activity_id, _) = self.begin_activity(tab_id, label, false);
        if let Some(log) = self.activity_log_for(tab_id) {
            log.set_status(activity_id, activity::ActivityStatus::Queued);
        }
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title,
            body,
            confirm_label: "Abandon".to_owned(),
            pending: PendingMutation {
                op,
                tab_id,
                activity_id,
                allow_immutable: false,
            },
        }));
        Task::none()
    }

    /// Context-menu tree for a file-tree row (`display_index` into the
    /// flattened tree of whichever sidebar is showing). In the diff view a
    /// file offers "Browse source at this revision" (jumped to it); both
    /// views offer path copies. Empty when the row vanished under the click.
    pub(crate) fn file_context_menu_tree(&self, display_index: usize) -> Vec<menu::MenuEntry> {
        use menu::MenuEntry;

        // Resolve the clicked row to a repo-relative path (+ whether it's a
        // file, i.e. browseable).
        let (path, is_file) = match self.active().main_view {
            MainView::Diff => {
                let rows = diffui_core::file_tree_rows(
                    &self.active().session.document.files,
                    &self.active().collapsed_dirs,
                );
                match rows.get(display_index) {
                    Some(diffui_core::FileTreeRow::File { file_index, .. }) => (
                        self.active()
                            .session
                            .document
                            .files
                            .get(*file_index)
                            .map(|file| file.path.clone()),
                        true,
                    ),
                    Some(diffui_core::FileTreeRow::Dir { path, .. }) => (Some(path.clone()), false),
                    None => (None, false),
                }
            }
            MainView::Source => {
                let (entries, rows) = self.source_entries_and_rows();
                match rows.get(display_index) {
                    Some(diffui_core::SourceTreeRow::File { entry_index, .. }) => (
                        entries.get(*entry_index).map(|entry| entry.path.clone()),
                        true,
                    ),
                    Some(diffui_core::SourceTreeRow::Dir { path, .. }) => {
                        (Some(path.clone()), false)
                    }
                    None => (None, false),
                }
            }
            MainView::History => (None, false),
        };
        let Some(path) = path else {
            return Vec::new();
        };

        let mut items = Vec::new();
        if is_file
            && self.active().main_view == MainView::Diff
            && self.active().repository.is_some()
        {
            items.push(self.menu_row(
                "Browse source at this revision",
                "revision.browse-source",
                CommandArg::Browse {
                    revision: self.active().session.selected_revision.clone(),
                    path: Some(path.clone()),
                },
            ));
            items.push(MenuEntry::Separator);
        }
        if is_file
            && self.active().main_view == MainView::Diff
            && crate::changes::command_file(self, &CommandArg::Path(path.clone())).is_some()
        {
            items.extend(self.file_change_menu_items(&path));
            items.push(MenuEntry::Separator);
        }
        if is_file && crate::history_panel::available(self) {
            let revision = match self.active().main_view {
                MainView::Source => crate::source_panel::browsed_revision(&self.active().source),
                _ => self.active().session.selected_revision.clone(),
            };
            items.push(self.menu_item(
                "file.history",
                CommandArg::Browse {
                    revision,
                    path: Some(path.clone()),
                },
            ));
            items.push(MenuEntry::Separator);
        }
        items.push(self.menu_row(
            "Copy path",
            "file.copy-path",
            CommandArg::Path(path.clone()),
        ));
        if self.active().repository.is_some() {
            items.push(self.menu_item("file.copy-absolute-path", CommandArg::Path(path)));
        }
        items
    }

    /// The diff-view file menu's partial-change rows: resolve actions for a
    /// conflicted file, then restore / move / split / select.
    fn file_change_menu_items(&self, path: &str) -> Vec<menu::MenuEntry> {
        let arg = || CommandArg::Path(path.to_owned());
        let mut items = Vec::new();
        if let Some(conflict) = crate::conflicts::conflict_at(self, path) {
            if conflict.pickable {
                for (index, id) in [(0, "conflict.take-side-1"), (1, "conflict.take-side-2")] {
                    if let Some(side) = conflict.sides.get(index) {
                        items.push(self.menu_row(
                            format!("Resolve: take side {} ({})", index + 1, side.label),
                            id,
                            arg(),
                        ));
                    }
                }
                if !conflict.bases.is_empty() {
                    items.push(self.menu_item("conflict.take-base", arg()));
                }
            }
            if conflict.pickable {
                items.push(self.menu_item("conflict.edit-markers", arg()));
            }
            if conflict.merge_tool_blocker.is_none() {
                items.push(self.menu_item("conflict.merge-tool", arg()));
            }
            items.push(menu::MenuEntry::Separator);
        }
        items.extend([
            self.menu_item("file.restore", arg()),
            self.menu_item("file.move-into", arg()),
            self.menu_item("file.split", arg()),
            self.menu_item("changes.toggle-file", arg()),
        ]);
        items
    }

    /// macOS: pop the file context menu natively (blocking, glowing over the
    /// row) and dispatch the pick. Mirrors `open_revision_context_menu`.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_file_context_menu(
        &mut self,
        display_index: usize,
        row_rect: iced::Rectangle,
        _cursor: iced::Point,
    ) -> Task<Message> {
        let tree = self.file_context_menu_tree(display_index);
        if tree.is_empty() {
            return Task::none();
        }
        let mut picks: Vec<(CommandId, CommandArg)> = Vec::new();
        let items = lower_menu_to_native(&tree, &mut picks);
        let glow = macos_native::GlowRect {
            x: row_rect.x,
            y: row_rect.y,
            width: row_rect.width,
            height: row_rect.height,
            radius: crate::revision_list::ROW_CARD_RADIUS,
        };
        let Some(chosen) = macos_native::popup_menu(&items, Some(glow)) else {
            return Task::none();
        };
        let Some((command, arg)) = picks.get(chosen as usize).cloned() else {
            return Task::none();
        };
        self.run_command(command, arg)
    }

    /// Non-macOS: open the file context menu as the iced overlay at the
    /// cursor, pulsing the row.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_file_context_menu(
        &mut self,
        display_index: usize,
        row_rect: iced::Rectangle,
        cursor: iced::Point,
    ) -> Task<Message> {
        let tree = self.file_context_menu_tree(display_index);
        if tree.is_empty() {
            return Task::none();
        }
        let mut overlay = menu::OverlayMenu::new(
            tree,
            menu::AnchorSpec::At(cursor),
            Some(iced::mouse::Button::Right),
        );
        overlay.glow = Some(row_rect);
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::Menu(overlay));
        Task::none()
    }

    /// macOS: lower the shared tree to a native `NSMenu`, pop it (blocking, with
    /// a pulsing glow over `row_rect`), and dispatch the chosen action.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_revision_context_menu(
        &mut self,
        _repository: Repository,
        selection: RevisionSelection,
        row_rect: iced::Rectangle,
        _cursor: iced::Point,
    ) -> Task<Message> {
        let tree = self.revision_menu_tree(&selection);
        let mut picks: Vec<(CommandId, CommandArg)> = Vec::new();
        let items = lower_menu_to_native(&tree, &mut picks);
        let glow = macos_native::GlowRect {
            x: row_rect.x,
            y: row_rect.y,
            width: row_rect.width,
            height: row_rect.height,
            radius: crate::revision_list::ROW_CARD_RADIUS,
        };
        let Some(chosen) = macos_native::popup_menu(&items, Some(glow)) else {
            return Task::none();
        };
        let Some((command, arg)) = picks.get(chosen as usize).cloned() else {
            return Task::none();
        };
        self.run_command(command, arg)
    }

    /// Non-macOS: open the iced overlay menu at the cursor, pulsing `row_rect`.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_revision_context_menu(
        &mut self,
        _repository: Repository,
        selection: RevisionSelection,
        row_rect: iced::Rectangle,
        cursor: iced::Point,
    ) -> Task<Message> {
        let tree = self.revision_menu_tree(&selection);
        // Opened by the right button: its release is the one that opened the
        // menu, so it neither picks a row nor dismisses (see
        // `OverlayMenu::opening_release`).
        let mut overlay = menu::OverlayMenu::new(
            tree,
            menu::AnchorSpec::At(cursor),
            Some(iced::mouse::Button::Right),
        );
        overlay.glow = Some(row_rect);
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::Menu(overlay));
        Task::none()
    }

    /// Open a toolbar dropdown (fetch branches / revset presets) as a native
    /// `NSMenu` at the cursor — it auto-sizes to the longest label and never
    /// word-wraps, unlike the iced overlay (kept as the non-macOS fallback).
    /// The menu is modal/blocking like the revision context menu, so the chosen
    /// action is dispatched directly on return.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_toolbar_menu(
        &mut self,
        which: ToolbarMenu,
        _anchor: iced::Rectangle,
    ) -> Task<Message> {
        use macos_native::MenuItem;

        match which {
            ToolbarMenu::FetchBranches => {
                // id 0 = `jj git fetch`, 1 = all remotes; each known
                // `name@remote` follows, ordered by proximity to the working
                // copy.
                let mut targets = vec![FetchTarget::Default, FetchTarget::AllRemotes];
                let mut items = vec![
                    MenuItem::entry("Fetch", 0),
                    MenuItem::entry("Fetch all remotes", 1),
                ];
                let branches = self.remote_branches_by_proximity();
                if !branches.is_empty() {
                    items.push(MenuItem::Separator);
                    for (branch, remote) in branches {
                        let id = targets.len() as u32;
                        items.push(MenuItem::entry(format!("{branch}@{remote}"), id));
                        targets.push(FetchTarget::RemoteBranch { remote, branch });
                    }
                }
                let Some(chosen) = macos_native::popup_menu(&items, None) else {
                    return Task::none();
                };
                let Some(target) = targets.get(chosen as usize).cloned() else {
                    return Task::none();
                };
                self.run_command("repo.fetch", CommandArg::Fetch(target))
            }
            ToolbarMenu::RevsetPresets => {
                let entries = self.revset_menu_entries();
                let items: Vec<MenuItem> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, (label, expr))| {
                        MenuItem::entry(format!("{label}  \u{b7}  {expr}"), index as u32)
                    })
                    .collect();
                let Some(chosen) = macos_native::popup_menu(&items, None) else {
                    return Task::none();
                };
                let Some((_, expr)) = entries.get(chosen as usize) else {
                    return Task::none();
                };
                self.run_command("repo.set-revset", CommandArg::Revset(expr.clone()))
            }
        }
    }

    /// Non-macOS: open the iced overlay dropdown, anchored edge-to-edge below
    /// the trigger's reported rect.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_toolbar_menu(
        &mut self,
        which: ToolbarMenu,
        anchor: iced::Rectangle,
    ) -> Task<Message> {
        let root = match which {
            ToolbarMenu::FetchBranches => self.fetch_menu_entries(),
            ToolbarMenu::RevsetPresets => self.revset_menu_entry_tree(),
        };
        // `AnchorArea` fires on a left press, so that press's release is the
        // opening one — swallowed rather than treated as a pick/dismiss.
        self.push_mode(Mode::Menu(menu::OverlayMenu::new(
            root,
            menu::AnchorSpec::Below(anchor),
            Some(iced::mouse::Button::Left),
        )));
        Task::none()
    }
}
