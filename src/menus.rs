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
use crate::Cx;
use crate::commands::CommandId;
use crate::prose::Prose;

impl Diffui {
    /// A menu row for `command`, labelled and chord-hinted from the registry.
    pub(crate) fn menu_item(&self, command: &str, arg: CommandArg) -> menu::MenuEntry {
        let Some(entry) = self.registry.get(command) else {
            return menu::MenuEntry::Disabled {
                label: command.to_owned(),
                reason: None,
            };
        };
        if let Err(reason) = entry.enabled(self, &arg) {
            return menu::MenuEntry::Disabled {
                label: entry.label.to_string(),
                reason: Some(reason.into_owned()),
            };
        }
        menu::MenuEntry::Item {
            label: entry.label.to_string(),
            detail: self
                .keymap
                .chord_for(Context::Base, command)
                .map(Chord::display),
            emphasized: false,
            command: entry.id.clone(),
            arg,
        }
    }

    /// A menu row for `command` under a label the registry can't supply — one
    /// of a generated set (a bookmark, a remote branch, a preset).
    pub(crate) fn menu_row(
        &self,
        label: impl Into<String>,
        command: &str,
        arg: CommandArg,
    ) -> menu::MenuEntry {
        if let Some(Err(reason)) = self
            .registry
            .get(command)
            .map(|entry| entry.enabled(self, &arg))
        {
            return menu::MenuEntry::Disabled {
                label: label.into(),
                reason: Some(reason.into_owned()),
            };
        }
        menu::MenuEntry::Item {
            label: label.into(),
            detail: None,
            emphasized: false,
            command: command.into(),
            arg,
        }
    }

    /// Build the revision context menu's entry tree from the already-loaded
    /// bookmarks / commit graph (so it opens instantly, no repo read).
    /// Shared by the macOS native popup and the overlay; author/committer/
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
            return self.batch_menu_tree(crate::revops::marked_selections(self.active()));
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
            self.active().session.commits.find(selection).map(|row| {
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
                    command: "bookmark.move".into(),
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
    /// remote branch (`name@remote`), nearest-first. Non-macOS only — the
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
                command: "repo.fetch".into(),
                arg: CommandArg::Fetch(FetchTarget::Default),
            },
            MenuEntry::Item {
                label: "Fetch all remotes".to_owned(),
                detail: None,
                emphasized: false,
                command: "repo.fetch".into(),
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
    /// the right-aligned detail. Non-macOS only — the overlay's builder;
    /// macOS builds its revset menu natively in `open_toolbar_menu`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn revset_menu_entry_tree(&self) -> Vec<menu::MenuEntry> {
        self.revset_menu_entries()
            .into_iter()
            .map(|(label, expr)| menu::MenuEntry::Item {
                label,
                detail: Some(expr.clone()),
                emphasized: false,
                command: "repo.set-revset".into(),
                arg: CommandArg::Revset(expr),
            })
            .collect()
    }

    /// Run the registry command `id` with `arg`.
    ///
    /// The single funnel every non-keyboard producer goes through. `enabled` is
    /// evaluated *here*, not when the surface was built, so a menu that has been
    /// open across a graph reload can no longer act on a row that reload hid.
    pub(crate) fn run_command(&mut self, id: &str, arg: CommandArg, cx: &mut Cx) {
        let registry = self.registry.clone();
        let Some(command) = registry.get(id) else {
            return;
        };
        if command.enabled(self, &arg).is_err() {
            return;
        }
        if let Some(action) = command.build(self, arg) {
            self.perform(action, cx)
        }
    }

    /// [`Self::start_mutation_on`] the active tab: what a user's gesture
    /// starts.
    pub(crate) fn start_mutation_op(&mut self, op: mutations::MutationOp) {
        if let Some(tab) = self.active_tab_id() {
            self.start_mutation_on(tab, op, false, false);
        }
    }

    /// Wrap `op` in an activity and send it to `tab_id`'s repository actor.
    /// Every mutation entry point — context menu, target-mode confirm, drag &
    /// drop — funnels through here so labels and guards can't drift apart.
    /// `allow_backwards` is a typed `--allow-backwards`: a bookmark move then
    /// runs without asking first. `allow_immutable` is a typed
    /// `--ignore-immutable`: the immutable-commit guard is off.
    pub(crate) fn start_mutation_on(
        &mut self,
        tab_id: TabId,
        op: mutations::MutationOp,
        allow_backwards: bool,
        allow_immutable: bool,
    ) {
        use mutations::MutationOp;
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        if !tab.session.capabilities.mutate {
            return;
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
        if crate::revops::consumes_marks(tab, &op)
            && let Some(tab) = self.tab_mut(tab_id)
        {
            tab.revision_multi_selection.clear();
        }
        let (activity_id, _) = self.begin_activity(tab_id, label, determinate);
        let pending = PendingMutation {
            op,
            tab_id,
            activity_id,
            allow_immutable,
        };
        // jj CLI parity: `jj bookmark set` refuses a backwards/sideways move
        // without `--allow-backwards`. Check ancestry first — the result either
        // runs the move directly (fast-forward) or raises a confirmation
        // dialog. The activity sits Queued meanwhile so the
        // action stays visible (and is resolved on cancel).
        if !allow_backwards && let MutationOp::MoveBookmark { name, to, .. } = &pending.op {
            if let Some(log) = self.activity_log_for(pending.tab_id) {
                log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
            }
            let (name, to) = (name.clone(), to.clone());
            let Some(state) = self.tab_mut(tab_id) else {
                return;
            };
            let (job, effects) = state.session.bookmark_check(name, to);
            state.jobs.issue(job, Pending::BookmarkCheck(pending));
            return self.run_effects(tab_id, effects);
        }
        self.run_mutation_checking_wc_push(pending)
    }

    /// Raise a confirmation for `op` and run it on accept. Only abandon asks
    /// today (reached from a single key or the palette); anything else runs
    /// straight away. The activity row is opened now and sits queued, so a
    /// cancel resolves it the same way the other confirmations do.
    pub(crate) fn confirm_mutation_op(&mut self, op: mutations::MutationOp) {
        use mutations::MutationOp;
        let MutationOp::Abandon { targets } = &op else {
            return self.start_mutation_op(op);
        };
        let Some(tab_id) = self.active_tab_id() else {
            return;
        };
        if !self.active().session.capabilities.mutate || targets.is_empty() {
            return;
        }
        let (title, label, body) = match targets.as_slice() {
            [target] => (
                Prose::from("Abandon ")
                    .then(self.active().revision_prose(target))
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
        let pending = self.begin_mutation(tab_id, label, op);
        self.ask(ConfirmDialog {
            title,
            body: Prose::from(body),
            confirm_label: "Abandon".to_owned(),
            look: Default::default(),
            pending,
        });
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

    /// macOS: shows a native `NSMenu`, then runs `pick` on what was chosen, inside
    /// `act`.
    ///
    /// An `NSMenu` runs a modal tracking loop on the main thread, and while it runs
    /// AppKit keeps servicing the main queue (GPUI's foreground tasks, repository
    /// events) and the display link. Each of those takes the `App` borrow, so the
    /// loop must not start inside one: a listener or an `act` holds that borrow
    /// and the first task to fire would panic with it already borrowed. This runs
    /// it as a foreground task of its own, which holds nothing while it blocks, the
    /// way kit's native menu does. The app stays live under the menu, and a pick
    /// comes back through a fresh update.
    #[cfg(target_os = "macos")]
    fn popup(
        items: Vec<macos_native::MenuItem>,
        glow: Option<macos_native::GlowRect>,
        cx: &mut Cx,
        pick: impl FnOnce(&mut Diffui, u32, &mut Cx) + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let chosen = run_popup(&items, glow, &this, cx);
            let _ = this.update(cx, |this, cx| {
                if let Some(chosen) = chosen {
                    this.act(cx, |this, cx| pick(this, chosen, cx));
                }
                // The tracking loop ate the mouse-up. Without a repaint, a dismissed
                // menu leaves the window idle and deaf to the next right-click.
                this.with_window(cx, |window, _| window.refresh());
            });
        })
        .detach();
    }

    /// macOS: pop `tree` natively and run the picked row's command.
    #[cfg(target_os = "macos")]
    pub(crate) fn popup_tree(
        tree: &[menu::MenuEntry],
        glow: Option<macos_native::GlowRect>,
        cx: &mut Cx,
    ) {
        let mut picks: Vec<(CommandId, CommandArg)> = Vec::new();
        let items = lower_menu_to_native(tree, &mut picks);
        Self::popup(items, glow, cx, move |this, chosen, cx| {
            if let Some((command, arg)) = picks.get(chosen as usize).cloned() {
                this.run_command(&command, arg, cx);
            }
        });
    }

    /// The system folder picker, then opening what was chosen. Its panel runs a
    /// modal loop like a menu's, so it runs outside any update for the same reason
    /// ([`Self::popup`]).
    pub(crate) fn choose_repo_folder(&mut self, cx: &mut Cx) {
        #[cfg(target_os = "macos")]
        cx.spawn(async move |this, cx| {
            let chosen = macos_native::choose_repository_folder();
            let _ = this.update(cx, |this, cx| {
                if let Some(path) = chosen {
                    this.act(cx, |this, cx| this.open_repository(&path, cx));
                }
                this.with_window(cx, |window, _| window.refresh());
            });
        })
        .detach();
        #[cfg(not(target_os = "macos"))]
        let _ = cx;
    }

    /// macOS: pop the file context menu natively, glowing over the row, and
    /// dispatch the pick. Mirrors `open_revision_context_menu`.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_file_context_menu(
        &mut self,
        display_index: usize,
        row_rect: Rectangle,
        _cursor: Point,
        cx: &mut Cx,
    ) {
        let tree = self.file_context_menu_tree(display_index);
        if tree.is_empty() {
            return;
        }
        Self::popup_tree(&tree, Some(row_glow(row_rect)), cx);
    }

    /// Non-macOS: open the file context menu as the overlay at the cursor, pulsing
    /// the row.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_file_context_menu(
        &mut self,
        display_index: usize,
        row_rect: Rectangle,
        cursor: Point,
        cx: &mut Cx,
    ) {
        let tree = self.file_context_menu_tree(display_index);
        if tree.is_empty() {
            return;
        }
        let mut overlay = menu::OverlayMenu::new(
            tree,
            menu::AnchorSpec::At(cursor),
            Some(gpui_kit::MouseButton::Right),
        );
        overlay.glow = Some(row_rect);
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::Menu(overlay));
    }

    /// macOS: lower the shared tree to a native `NSMenu` and pop it, with a pulsing
    /// glow over `row_rect`.
    #[cfg(target_os = "macos")]
    pub(crate) fn open_revision_context_menu(
        &mut self,
        _repository: Repository,
        selection: RevisionSelection,
        row_rect: Rectangle,
        _cursor: Point,
        cx: &mut Cx,
    ) {
        let tree = self.revision_menu_tree(&selection);
        Self::popup_tree(&tree, Some(row_glow(row_rect)), cx);
    }

    /// Non-macOS: open the overlay menu at the cursor, pulsing `row_rect`.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_revision_context_menu(
        &mut self,
        _repository: Repository,
        selection: RevisionSelection,
        row_rect: Rectangle,
        cursor: Point,
        cx: &mut Cx,
    ) {
        let tree = self.revision_menu_tree(&selection);
        // Opened by the right button: its release is the one that opened the
        // menu, so it neither picks a row nor dismisses (see
        // `OverlayMenu::opening_release`).
        let mut overlay = menu::OverlayMenu::new(
            tree,
            menu::AnchorSpec::At(cursor),
            Some(gpui_kit::MouseButton::Right),
        );
        overlay.glow = Some(row_rect);
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::Menu(overlay));
    }

    /// Open a toolbar dropdown (fetch branches / revset presets) as a native
    /// `NSMenu` at the cursor: it auto-sizes to the longest label and never
    /// word-wraps, unlike the overlay (kept as the non-macOS fallback).
    #[cfg(target_os = "macos")]
    pub(crate) fn open_toolbar_menu(
        &mut self,
        which: ToolbarMenu,
        _anchor: Rectangle,
        cx: &mut Cx,
    ) {
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
                Self::popup(items, None, cx, move |this, chosen, cx| {
                    if let Some(target) = targets.get(chosen as usize).cloned() {
                        this.run_command("repo.fetch", CommandArg::Fetch(target), cx);
                    }
                });
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
                Self::popup(items, None, cx, move |this, chosen, cx| {
                    if let Some((_, expr)) = entries.get(chosen as usize) {
                        this.run_command("repo.set-revset", CommandArg::Revset(expr.clone()), cx);
                    }
                });
            }
        }
    }

    /// Non-macOS: open the overlay dropdown, anchored edge-to-edge below the
    /// trigger's rect.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn open_toolbar_menu(&mut self, which: ToolbarMenu, anchor: Rectangle, cx: &mut Cx) {
        let root = match which {
            ToolbarMenu::FetchBranches => self.fetch_menu_entries(),
            ToolbarMenu::RevsetPresets => self.revset_menu_entry_tree(),
        };
        // The caret opens on a left press, so that press's release is the opening
        // one: swallowed rather than treated as a pick/dismiss.
        self.push_mode(Mode::Menu(menu::OverlayMenu::new(
            root,
            menu::AnchorSpec::Below(anchor),
            Some(gpui_kit::MouseButton::Left),
        )));
    }
}

/// A glow tracing the revision or file row at `rect`.
#[cfg(target_os = "macos")]
fn row_glow(rect: Rectangle) -> macos_native::GlowRect {
    macos_native::GlowRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        radius: crate::theme::list::radius(rect.height),
    }
}

/// The tracking loop itself: blocks until the menu closes.
#[cfg(all(target_os = "macos", not(test)))]
fn run_popup(
    items: &[macos_native::MenuItem],
    glow: Option<macos_native::GlowRect>,
    _this: &gpui_kit::WeakEntity<Diffui>,
    _cx: &mut gpui_kit::AsyncApp,
) -> Option<u32> {
    macos_native::popup_menu(items, glow)
}

/// Tests can't track a real menu, so this stands in for the loop and does to the
/// app what AppKit does during one: run updates on the same stack. If the popup
/// were still inside the update that opened it, that would panic with the app
/// already borrowed.
#[cfg(all(target_os = "macos", test))]
fn run_popup(
    items: &[macos_native::MenuItem],
    glow: Option<macos_native::GlowRect>,
    this: &gpui_kit::WeakEntity<Diffui>,
    cx: &mut gpui_kit::AsyncApp,
) -> Option<u32> {
    let _ = this.update(cx, |this, cx| this.act(cx, |_, _| {}));
    tests::SCRIPT.with_borrow_mut(|script| {
        script.shown.push((
            items
                .iter()
                .filter_map(|item| match item {
                    macos_native::MenuItem::Entry { label, .. }
                    | macos_native::MenuItem::Submenu { label, .. } => Some(label.clone()),
                    macos_native::MenuItem::Separator => None,
                })
                .collect(),
            glow.is_some(),
        ));
        script.pick.take()
    })
}

#[cfg(all(target_os = "macos", test))]
pub(crate) mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::harness::{Harness, fixture_repo};

    #[derive(Default)]
    pub(crate) struct Script {
        /// What each popup showed (top-level labels) and whether it glowed.
        pub(crate) shown: Vec<(Vec<String>, bool)>,
        /// What the user "picks" in the next one.
        pub(crate) pick: Option<u32>,
    }

    thread_local! {
        pub(crate) static SCRIPT: RefCell<Script> = RefCell::default();
    }

    fn rect() -> Rectangle {
        Rectangle::new(Point::new(10.0, 10.0), Size::new(100.0, 28.0))
    }

    /// The menu doesn't open inside the update that asks for it, survives app
    /// updates while it's up, and a pick comes back through `act`: a command that
    /// reads state set by the pick sees it, and the mode stack was reconciled.
    #[test]
    fn a_toolbar_menu_opens_outside_the_update_and_its_pick_comes_back() {
        let mut h = Harness::open(Some(&fixture_repo()));
        h.wait_for_graph();
        SCRIPT.with_borrow_mut(|script| *script = Script::default());

        let opened_inline = h.act(|ui, cx| {
            ui.open_toolbar_menu(ToolbarMenu::RevsetPresets, rect(), cx);
            SCRIPT.with_borrow(|script| !script.shown.is_empty())
        });
        assert!(!opened_inline, "the loop must not run inside the update");
        h.settle();
        let shown = SCRIPT.with_borrow(|script| script.shown.clone());
        assert_eq!(shown.len(), 1, "dismissed: shown once");
        assert!(!shown[0].1, "a dropdown has no row glow");
        let before = h.read(|ui| ui.active().session.revset.clone());

        // Dismissed changed nothing; a pick runs the preset's command.
        let presets = h.read(|ui| ui.revset_menu_entries());
        let target = presets
            .iter()
            .position(|(_, expr)| *expr != before)
            .expect("a preset other than the current revset");
        SCRIPT.with_borrow_mut(|script| script.pick = Some(target as u32));
        h.act(|ui, cx| ui.open_toolbar_menu(ToolbarMenu::RevsetPresets, rect(), cx));
        h.settle();
        assert_eq!(
            h.read(|ui| ui.active().session.revset.clone()),
            presets[target].1
        );
    }

    /// The revision menu takes the same path, with the row's glow.
    #[test]
    fn a_revision_menu_pops_with_its_glow() {
        let mut h = Harness::open(Some(&fixture_repo()));
        h.wait_for_graph();
        SCRIPT.with_borrow_mut(|script| *script = Script::default());
        h.act(|ui, cx| {
            let repository = ui.active().repository.clone().expect("a repository");
            ui.open_revision_context_menu(
                repository,
                RevisionSelection::WorkingCopy,
                rect(),
                Point::ORIGIN,
                cx,
            );
        });
        h.settle();
        let shown = SCRIPT.with_borrow(|script| script.shown.clone());
        assert_eq!(shown.len(), 1);
        assert!(shown[0].1, "the row glows while its menu is up");
        assert!(shown[0].0.iter().any(|label| label == "Rebase"));
    }
}
