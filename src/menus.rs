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

impl Diffui {
    /// A menu row for `command`, labelled and chord-hinted from the registry.
    pub(crate) fn menu_item(&self, command: CommandId, arg: CommandArg) -> menu::MenuEntry {
        let entry = commands::command(command);
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
            return vec![
                self.menu_row(
                    format!("Abandon {} revisions", marked.len()),
                    "revision.abandon",
                    CommandArg::Revisions(
                        marked
                            .iter()
                            .map(|id| RevisionSelection::Commit((*id).to_owned()))
                            .collect(),
                    ),
                ),
                MenuEntry::Separator,
                self.menu_item("selection.clear", CommandArg::None),
            ];
        }

        let on = || CommandArg::Revision(selection.clone());
        let mut top = vec![
            self.menu_item("revision.edit-description", on()),
            MenuEntry::Separator,
            self.menu_item("revision.new-child", on()),
            self.menu_item("revision.edit", on()),
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
            self.menu_item("revision.duplicate", on()),
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
        ];

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
        // common plain move stays one level deep. Bookmarks without a
        // tracked remote are omitted (nowhere to push); the submenu hides
        // entirely when none qualify instead of sitting disabled in every
        // menu of a remote-less repo. Conflicted bookmarks are omitted too:
        // the move is a conflict *resolution* — publishing it in the same
        // gesture, typically right after a force-push surprise, deserves a
        // deliberate separate push once the resolved state looks right.
        let move_push_items: Vec<MenuEntry> = moves
            .iter()
            .filter(|(_, _, conflicted)| !conflicted)
            .filter_map(|(name, _target, _)| {
                let entry = self
                    .active()
                    .session
                    .bookmarks
                    .bookmarks
                    .iter()
                    .find(|b| b.name == *name)?;
                let remote = entry.tracked_remote()?;
                // The remote rides in the detail column rather than an arrow
                // glyph in the label: `\u{2192}` renders as a fallback-font
                // blob in plenty of UI fonts.
                Some(menu::MenuEntry::Item {
                    label: name.clone(),
                    detail: Some(remote.to_owned()),
                    emphasized: false,
                    command: "bookmark.move",
                    arg: CommandArg::Bookmark {
                        name: name.clone(),
                        remote: Some(remote.to_owned()),
                        to: Some(selection.clone()),
                    },
                })
            })
            .collect();
        top.push(MenuEntry::Separator);
        top.push(if move_items.is_empty() {
            MenuEntry::Disabled {
                label: "Move bookmark here".to_owned(),
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
                    } else if let Some(remote) = entry.tracked_remote() {
                        sub.push(self.menu_row(
                            format!("Push to {remote}"),
                            "bookmark.push",
                            CommandArg::Bookmark {
                                name: entry.name.clone(),
                                remote: Some(remote.to_owned()),
                                to: None,
                            },
                        ));
                    }
                    sub.push(self.menu_item(
                        "bookmark.delete",
                        CommandArg::Bookmark {
                            name: entry.name.clone(),
                            remote: None,
                            to: None,
                        },
                    ));
                    let label = if entry.is_conflicted() {
                        format!("{}??", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    bookmark_items.push(MenuEntry::Submenu { label, items: sub });
                }
                for remote_ref in &entry.remotes {
                    if remote_ref.target.as_str() == hex && !remote_ref.tracked {
                        bookmark_items.push(MenuEntry::Submenu {
                            label: format!("{}@{}", entry.name, remote_ref.remote),
                            items: vec![self.menu_item(
                                "bookmark.track",
                                CommandArg::Bookmark {
                                    name: entry.name.clone(),
                                    remote: Some(remote_ref.remote.clone()),
                                    to: None,
                                },
                            )],
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
        let mut items = vec![MenuEntry::Item {
            label: "Fetch all remotes".to_owned(),
            detail: None,
            emphasized: true,
            command: "repo.fetch",
            arg: CommandArg::Fetch(FetchTarget::AllRemotes),
        }];
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
        if !(command.enabled)(self, &arg) {
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
        let label = match &op {
            MutationOp::New { .. } => "New change".to_owned(),
            MutationOp::Edit { .. } => "Edit".to_owned(),
            MutationOp::Abandon { targets } => {
                if targets.len() == 1 {
                    "Abandon".to_owned()
                } else {
                    format!("Abandon {} revisions", targets.len())
                }
            }
            MutationOp::Describe { .. } => "Update description".to_owned(),
            MutationOp::Rebase { sources, .. } => {
                if sources.len() == 1 {
                    "Rebase".to_owned()
                } else {
                    format!("Rebase {} revisions", sources.len())
                }
            }
            MutationOp::Squash { .. } => "Squash".to_owned(),
            MutationOp::Merge { .. } => "New merge".to_owned(),
            MutationOp::Duplicate { .. } => "Duplicate".to_owned(),
            MutationOp::Absorb { .. } => "Absorb".to_owned(),
            MutationOp::MoveBookmark {
                name,
                push_remote: Some(remote),
                ..
            } => format!("Move {name} + push to {remote}"),
            MutationOp::MoveBookmark { name, .. } => format!("Move bookmark {name}"),
            MutationOp::DeleteBookmark { name } => format!("Delete bookmark {name}"),
            MutationOp::TrackBookmark { name, remote } => format!("Track {name}@{remote}"),
            MutationOp::PushBookmark { name, remote } => format!("Push {name} to {remote}"),
            MutationOp::Undo { .. } => "Undo".to_owned(),
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
        );
        // A batch abandon consumes the marked rows — the marks (and their
        // wash) mustn't outlive the pick.
        if matches!(&op, MutationOp::Abandon { targets } if targets.len() > 1) {
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
            let job = state.session.next_job();
            state.jobs.bookmark_check = Some((job, pending));
            return self.send(
                tab_id,
                diffui_core::Command::BookmarkCheck { job, name, to },
            );
        }
        self.run_mutation(pending)
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
                // id 0 = all remotes; each known `name@remote` follows, ordered
                // by proximity to the working copy.
                let mut targets = vec![FetchTarget::AllRemotes];
                let mut items = vec![MenuItem::entry("Fetch all remotes", 0)];
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
