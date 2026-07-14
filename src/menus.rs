//! Menu construction for [`Diffui`]: the revision context-menu tree, the
//! toolbar fetch/revset dropdowns, menu-action dispatch, and lowering to the
//! macOS native `NSMenu`. Split out of `update.rs`; these read only
//! already-loaded `Session` state, so menus open instantly with no repo I/O.

use super::*;

impl Diffui {
    /// Build the revision context menu's entry tree from the already-loaded
    /// `self.session.bookmarks` / `self.session.commits` (so it opens instantly, no repo read).
    /// Shared by the macOS native popup and the iced overlay; author/committer/
    /// description copies carry only an in-memory fallback, with the live value
    /// read on demand when picked.
    pub(crate) fn revision_menu_tree(&self, selection: &RevisionSelection) -> Vec<menu::MenuEntry> {
        use menu::MenuEntry;
        use mutations::MutationOp;

        // A live multi-selection (several marked rows, the clicked one among
        // them) swaps the per-revision menu for the batch menu — marking rows
        // only exists to act on all of them at once. Marks whose rows left
        // the loaded graph (reload, revset change) are ignored rather than
        // silently acted on.
        let marked: Vec<&str> = self
            .revision_multi_selection
            .iter()
            .map(String::as_str)
            .filter(|id| self.session.commits.find_by_commit_id(id).is_some())
            .collect();
        let clicked_id = match selection {
            RevisionSelection::WorkingCopy => self
                .session
                .commits
                .working_copy()
                .map(|row| row.commit_id().to_owned()),
            RevisionSelection::Commit(hex) => Some(hex.clone()),
        };
        if marked.len() > 1 && clicked_id.as_deref().is_some_and(|id| marked.contains(&id)) {
            return vec![
                MenuEntry::item(
                    format!("Abandon {} revisions", marked.len()),
                    MenuAction::Mutate(MutationOp::Abandon {
                        targets: marked
                            .iter()
                            .map(|id| RevisionSelection::Commit((*id).to_owned()))
                            .collect(),
                    }),
                ),
                MenuEntry::Separator,
                MenuEntry::item("Clear selection", MenuAction::ClearMultiSelection),
            ];
        }

        let mut top = vec![
            MenuEntry::item(
                "Edit description…",
                MenuAction::EditDescription {
                    target: selection.clone(),
                },
            ),
            MenuEntry::Separator,
            MenuEntry::item(
                "New child",
                MenuAction::Mutate(MutationOp::New {
                    parent: selection.clone(),
                }),
            ),
            MenuEntry::item(
                "Edit",
                MenuAction::Mutate(MutationOp::Edit {
                    target: selection.clone(),
                }),
            ),
            MenuEntry::Separator,
            // History surgery, grouped: multi-variant ops fold into submenus;
            // the "…" leaves enter target mode (pick the destination on the
            // graph, or drag a row directly — same machinery).
            MenuEntry::Submenu {
                label: "Rebase".to_owned(),
                items: vec![
                    MenuEntry::item(
                        "Onto\u{2026}",
                        MenuAction::StartDraft {
                            kind: mutations::DraftKind::Rebase {
                                mode: mutations::RebaseSourceMode::Revisions,
                            },
                            source: selection.clone(),
                        },
                    ),
                    MenuEntry::item(
                        "With descendants onto\u{2026}",
                        MenuAction::StartDraft {
                            kind: mutations::DraftKind::Rebase {
                                mode: mutations::RebaseSourceMode::WithDescendants,
                            },
                            source: selection.clone(),
                        },
                    ),
                    MenuEntry::item(
                        "Whole branch onto\u{2026}",
                        MenuAction::StartDraft {
                            kind: mutations::DraftKind::Rebase {
                                mode: mutations::RebaseSourceMode::Branch,
                            },
                            source: selection.clone(),
                        },
                    ),
                ],
            },
            MenuEntry::Submenu {
                label: "Squash".to_owned(),
                items: vec![
                    MenuEntry::item(
                        "Into parent",
                        MenuAction::Mutate(MutationOp::Squash {
                            from: vec![selection.clone()],
                            into: mutations::SquashTarget::Parent,
                        }),
                    ),
                    MenuEntry::item(
                        "Into\u{2026}",
                        MenuAction::StartDraft {
                            kind: mutations::DraftKind::Squash,
                            source: selection.clone(),
                        },
                    ),
                ],
            },
            MenuEntry::item(
                "Merge with\u{2026}",
                MenuAction::StartDraft {
                    kind: mutations::DraftKind::Merge,
                    source: selection.clone(),
                },
            ),
            MenuEntry::item(
                "Duplicate",
                MenuAction::Mutate(MutationOp::Duplicate {
                    target: selection.clone(),
                }),
            ),
            MenuEntry::item(
                "Absorb into ancestors",
                MenuAction::Mutate(MutationOp::Absorb {
                    from: selection.clone(),
                }),
            ),
            MenuEntry::item(
                "Abandon",
                MenuAction::Mutate(MutationOp::Abandon {
                    targets: vec![selection.clone()],
                }),
            ),
            MenuEntry::Separator,
            MenuEntry::item(
                "Browse source",
                MenuAction::BrowseSource {
                    revision: selection.clone(),
                    path: None,
                },
            ),
        ];

        // Copy revision metadata — values come from the loaded graph row.
        let copy_fields = {
            let row = match selection {
                RevisionSelection::WorkingCopy => self.session.commits.working_copy(),
                RevisionSelection::Commit(hex) => self.session.commits.find_by_commit_id(hex),
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
            let mut copy_items = vec![
                MenuEntry::item("Revision ID", MenuAction::CopyText(change_id)),
                MenuEntry::item("Commit hash", MenuAction::CopyText(commit_id)),
            ];
            match bookmarks.len() {
                0 => {}
                1 => copy_items.push(MenuEntry::item(
                    "Bookmark",
                    MenuAction::CopyText(bookmarks[0].clone()),
                )),
                _ => {
                    let subs = bookmarks
                        .iter()
                        .map(|name| {
                            MenuEntry::item(name.clone(), MenuAction::CopyText(name.clone()))
                        })
                        .collect();
                    copy_items.push(MenuEntry::Submenu {
                        label: "Bookmark".to_owned(),
                        items: subs,
                    });
                }
            }
            if !description.is_empty() {
                copy_items.push(MenuEntry::item(
                    "Description",
                    MenuAction::CopyDetail {
                        field: DetailField::Description,
                        fallback: description,
                    },
                ));
            }
            copy_items.push(MenuEntry::item(
                "Author",
                MenuAction::CopyDetail {
                    field: DetailField::Author,
                    fallback: author.clone(),
                },
            ));
            copy_items.push(MenuEntry::item(
                "Committer",
                MenuAction::CopyDetail {
                    field: DetailField::Committer,
                    fallback: author,
                },
            ));
            top.push(MenuEntry::Separator);
            top.push(MenuEntry::Submenu {
                label: "Copy".to_owned(),
                items: copy_items,
            });
        }

        // Move a local bookmark onto this revision, nearest-first.
        let mut moves: Vec<(String, String)> = self
            .session
            .bookmarks
            .bookmarks
            .iter()
            .filter_map(|b| b.local_target.as_ref().map(|t| (b.name.clone(), t.clone())))
            .collect();
        moves.sort();
        let move_reference = match selection {
            RevisionSelection::Commit(hex) => Some(hex.clone()),
            RevisionSelection::WorkingCopy => self.session.bookmarks.working_copy_commit.clone(),
        };
        self.sort_by_proximity(&mut moves, move_reference.as_deref(), |(_, t)| t.as_str());
        let move_items: Vec<MenuEntry> = moves
            .iter()
            .map(|(name, _target)| {
                MenuEntry::item(
                    name.clone(),
                    MenuAction::Mutate(MutationOp::MoveBookmark {
                        name: name.clone(),
                        to: selection.clone(),
                        push_remote: None,
                    }),
                )
            })
            .collect();
        // Same list, but landing the move also pushes the bookmark to its
        // tracked remote — "advance main and publish it" as one pick. Kept a
        // sibling submenu rather than a per-bookmark verb submenu so the
        // common plain move stays one level deep. Bookmarks without a
        // tracked remote are omitted (nowhere to push); the submenu hides
        // entirely when none qualify instead of sitting disabled in every
        // menu of a remote-less repo.
        let move_push_items: Vec<MenuEntry> = moves
            .iter()
            .filter_map(|(name, _target)| {
                let entry = self
                    .session
                    .bookmarks
                    .bookmarks
                    .iter()
                    .find(|b| b.name == *name)?;
                let remote = entry.tracked_remote()?;
                Some(MenuEntry::item(
                    format!("{name} \u{2192} {remote}"),
                    MenuAction::Mutate(MutationOp::MoveBookmark {
                        name: name.clone(),
                        to: selection.clone(),
                        push_remote: Some(remote.to_owned()),
                    }),
                ))
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
            RevisionSelection::WorkingCopy => self.session.bookmarks.working_copy_commit.as_deref(),
        };
        let mut bookmark_items: Vec<MenuEntry> = Vec::new();
        if let Some(hex) = target_hex {
            for entry in &self.session.bookmarks.bookmarks {
                if entry.local_target.as_deref() == Some(hex) {
                    let mut sub = Vec::new();
                    if let Some(remote) = entry.tracked_remote() {
                        sub.push(MenuEntry::item(
                            format!("Push to {remote}"),
                            MenuAction::Mutate(MutationOp::PushBookmark {
                                name: entry.name.clone(),
                                remote: remote.to_owned(),
                            }),
                        ));
                    }
                    sub.push(MenuEntry::item(
                        "Delete",
                        MenuAction::Mutate(MutationOp::DeleteBookmark {
                            name: entry.name.clone(),
                        }),
                    ));
                    bookmark_items.push(MenuEntry::Submenu {
                        label: entry.name.clone(),
                        items: sub,
                    });
                }
                for remote_ref in &entry.remotes {
                    if remote_ref.target.as_str() == hex && !remote_ref.tracked {
                        bookmark_items.push(MenuEntry::Submenu {
                            label: format!("{}@{}", entry.name, remote_ref.remote),
                            items: vec![MenuEntry::item(
                                "Track",
                                MenuAction::Mutate(MutationOp::TrackBookmark {
                                    name: entry.name.clone(),
                                    remote: remote_ref.remote.clone(),
                                }),
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
            action: MenuAction::Fetch(FetchTarget::AllRemotes),
        }];
        let branches = self.remote_branches_by_proximity();
        if !branches.is_empty() {
            items.push(MenuEntry::Separator);
            for (branch, remote) in branches {
                items.push(MenuEntry::item(
                    format!("{branch}@{remote}"),
                    MenuAction::Fetch(FetchTarget::RemoteBranch { remote, branch }),
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
                action: MenuAction::SetRevset(expr),
            })
            .collect()
    }

    /// Run a picked menu action. `selection` is the right-clicked revision (for
    /// the on-demand author/committer/description reads), `None` for the toolbar
    /// menus. Shared by the native and iced menus.
    pub(crate) fn dispatch_menu_action(
        &mut self,
        action: MenuAction,
        selection: Option<RevisionSelection>,
    ) -> Task<Message> {
        let op = match action {
            MenuAction::Fetch(target) => return self.start_fetch(target),
            MenuAction::SetRevset(expr) => {
                self.session.revset = expr;
                return self.evaluate_revset();
            }
            // Ready-to-paste values write to the clipboard immediately.
            MenuAction::CopyText(text) => return iced::clipboard::write(text).discard(),
            MenuAction::BrowseSource { revision, path } => {
                return self.open_source_browser(revision, path);
            }
            MenuAction::EditDescription { target } => {
                if self.session.selected_revision == target {
                    return Task::done(Message::DescriptionEdit);
                }
                if let Some(editor) = self.description_editor.as_mut()
                    && (editor.is_dirty() || editor.saving_activity.is_some())
                {
                    editor.switch_blocked = true;
                    return Task::none();
                }
                self.pending_description_edit = Some(target.clone());
                return self.update(Message::SelectRowKey(selection_key(&target)));
            }
            MenuAction::StartDraft { kind, source } => {
                return self.update(Message::DraftStart(kind, source));
            }
            // Author / committer / full description aren't kept in the graph, so
            // read the revision off-thread, format the field, and copy — falling
            // back to the in-memory value on failure.
            MenuAction::CopyDetail { field, fallback } => {
                let (Some(source), Some(selection)) = (self.session.source.clone(), selection)
                else {
                    return Task::none();
                };
                return Task::perform(source.details(selection), move |result| {
                    let text = result
                        .ok()
                        .and_then(|details| format_detail(&details, field))
                        .unwrap_or(fallback);
                    Message::CopyToClipboard(text)
                });
            }
            MenuAction::ClearMultiSelection => {
                self.revision_multi_selection.clear();
                return Task::none();
            }
            MenuAction::Mutate(op) => op,
        };
        // A batch abandon consumes the marked rows — the marks (and their
        // wash) mustn't outlive the pick.
        if matches!(&op, mutations::MutationOp::Abandon { targets } if targets.len() > 1) {
            self.revision_multi_selection.clear();
        }
        self.start_mutation_op(op)
    }

    /// Wrap `op` in an activity and run it through the serial mutation queue.
    /// Every mutation entry point — context menu, target-mode confirm, drag &
    /// drop — funnels through here so labels and guards can't drift apart.
    pub(crate) fn start_mutation_op(&mut self, op: mutations::MutationOp) -> Task<Message> {
        use mutations::MutationOp;
        let Some(repository) = self.session.repository.clone() else {
            return Task::none();
        };
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
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
        let (activity_id, progress) = self.begin_activity(label, determinate);
        let pending = PendingMutation {
            repository,
            op,
            tab_id,
            activity_id,
            progress,
        };
        // jj CLI parity: `jj bookmark set` refuses a backwards/sideways move
        // without `--allow-backwards`. Check ancestry off-thread first — the
        // result either runs the move directly (fast-forward) or raises a
        // confirmation dialog. The activity sits Queued meanwhile so the
        // action stays visible (and is resolved on cancel).
        if let MutationOp::MoveBookmark { name, to, .. } = &pending.op {
            if let Some(log) = self.activity_log_for(pending.tab_id) {
                log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
            }
            let repository = pending.repository.clone();
            let (name, to) = (name.clone(), to.clone());
            let pending = Box::new(pending);
            return Task::perform(
                crate::jj::bookmark_move_is_backwards(repository, name, to),
                move |result| Message::BookmarkMoveChecked(pending, Box::new(result)),
            );
        }
        self.enqueue_or_run_mutation(pending)
    }

    /// Context-menu tree for a file-tree row (`display_index` into the
    /// flattened tree of whichever sidebar is showing). In the diff view a
    /// file offers "Browse source at this revision" (jumped to it); both
    /// views offer path copies. Empty when the row vanished under the click.
    pub(crate) fn file_context_menu_tree(&self, display_index: usize) -> Vec<menu::MenuEntry> {
        use menu::MenuEntry;

        // Resolve the clicked row to a repo-relative path (+ whether it's a
        // file, i.e. browseable).
        let (path, is_file) = match self.main_view {
            MainView::Diff => {
                let rows =
                    diffui_core::file_tree_rows(&self.session.document.files, &self.collapsed_dirs);
                match rows.get(display_index) {
                    Some(diffui_core::FileTreeRow::File { file_index, .. }) => (
                        self.session
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
        if is_file && self.main_view == MainView::Diff && self.session.repository.is_some() {
            items.push(MenuEntry::item(
                "Browse source at this revision",
                MenuAction::BrowseSource {
                    revision: self.session.selected_revision.clone(),
                    path: Some(path.clone()),
                },
            ));
            items.push(MenuEntry::Separator);
        }
        items.push(MenuEntry::item(
            "Copy path",
            MenuAction::CopyText(path.clone()),
        ));
        if let Some(repository) = &self.session.repository {
            items.push(MenuEntry::item(
                "Copy absolute path",
                MenuAction::CopyText(repository.root.join(&path).display().to_string()),
            ));
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
        let mut actions: Vec<MenuAction> = Vec::new();
        let items = lower_menu_to_native(&tree, &mut actions);
        let glow = macos_native::GlowRect {
            x: row_rect.x,
            y: row_rect.y,
            width: row_rect.width,
            height: row_rect.height,
        };
        let Some(chosen) = macos_native::popup_menu(&items, Some(glow)) else {
            return Task::none();
        };
        let Some(action) = actions.get(chosen as usize).cloned() else {
            return Task::none();
        };
        self.dispatch_menu_action(action, None)
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
        let mut overlay = menu::OverlayMenu::new(tree, menu::AnchorSpec::At(cursor), false);
        overlay.glow = Some(row_rect);
        self.activity_popover_open = false;
        self.menu = Some(overlay);
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
        let mut actions: Vec<MenuAction> = Vec::new();
        let items = lower_menu_to_native(&tree, &mut actions);
        let glow = macos_native::GlowRect {
            x: row_rect.x,
            y: row_rect.y,
            width: row_rect.width,
            height: row_rect.height,
        };
        let Some(chosen) = macos_native::popup_menu(&items, Some(glow)) else {
            return Task::none();
        };
        let Some(action) = actions.get(chosen as usize).cloned() else {
            return Task::none();
        };
        self.dispatch_menu_action(action, Some(selection))
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
        // `armed: false` so the right-button release that opened the menu is
        // swallowed (kept open) rather than treated as a pick/dismiss — the
        // cursor sits in the card's corner padding, not on a row, at open.
        let mut overlay = menu::OverlayMenu::new(tree, menu::AnchorSpec::At(cursor), false);
        overlay.selection = Some(selection);
        overlay.glow = Some(row_rect);
        self.activity_popover_open = false;
        self.menu = Some(overlay);
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
                self.start_fetch(target)
            }
            ToolbarMenu::RevsetPresets => {
                let entries = self.revset_menu_entries();
                let items: Vec<MenuItem> = entries
                    .iter()
                    .enumerate()
                    .map(|(index, (label, expr))| {
                        MenuItem::entry(format!("{label}  ·  {expr}"), index as u32)
                    })
                    .collect();
                let Some(chosen) = macos_native::popup_menu(&items, None) else {
                    return Task::none();
                };
                let Some((_, expr)) = entries.get(chosen as usize) else {
                    return Task::none();
                };
                self.session.revset = expr.clone();
                self.evaluate_revset()
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
        self.menu = Some(menu::OverlayMenu::new(
            root,
            menu::AnchorSpec::Below(anchor),
            false,
        ));
        Task::none()
    }
}
