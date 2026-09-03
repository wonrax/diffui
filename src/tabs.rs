//! Multi-repo tab lifecycle for [`Diffui`]: the accessors that resolve a tab's
//! state, activating and closing tabs, and opening a repository as a new tab.
//! Split out of `update.rs` to keep that module focused on the message-handling
//! core.

use super::*;

impl Diffui {
    /// The active tab's state, or [`Diffui::no_tab`] when none is open.
    pub(crate) fn active(&self) -> &TabState {
        match self.tabs.get(self.active) {
            Some(tab) => &tab.state,
            None => &self.no_tab,
        }
    }

    pub(crate) fn active_mut(&mut self) -> &mut TabState {
        match self.tabs.get_mut(self.active) {
            Some(tab) => &mut tab.state,
            None => &mut self.no_tab,
        }
    }

    /// The state owned by `tab`, or `None` once that tab has closed — the
    /// single lookup every routed async completion goes through, so a result
    /// that outlives its tab is dropped instead of landing on a stranger.
    pub(crate) fn tab_mut(&mut self, tab: TabId) -> Option<&mut TabState> {
        self.tabs
            .iter_mut()
            .find(|candidate| candidate.id == tab)
            .map(|candidate| &mut candidate.state)
    }

    /// Id of the active tab, or `None` when no tabs are open. Per-tab async
    /// completions carry this so a result that lands after a tab switch is
    /// applied to *its* tab rather than to whichever one is active by then.
    pub(crate) fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.get(self.active).map(|tab| tab.id)
    }

    /// Whether `tab` is the one on screen — the gate on every active-only
    /// follow-up (the paint-version bump, the empty-status spawn, the
    /// coalesced refresh), which a backgrounded tab runs on its next
    /// activation instead.
    pub(crate) fn is_active(&self, tab: TabId) -> bool {
        self.active_tab_id() == Some(tab)
    }

    /// The tab whose live streaming cursor is `version`, with its state.
    /// Streaming results are routed by version rather than by tab id (the
    /// worker is spawned before anything addresses a tab), so a cold walk / PR
    /// stream keeps its progress while its tab is backgrounded. `None` means
    /// the load was superseded — its tab re-kicked, or closed — and the result
    /// must be discarded. The id comes back with the state so the caller can
    /// gate its active-only follow-ups without a second lookup.
    pub(crate) fn load_target_mut(&mut self, version: u64) -> Option<(TabId, &mut TabState)> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.state.session.load.as_ref().map(|c| c.version) == Some(version))
            .map(|tab| (tab.id, &mut tab.state))
    }

    /// The tab displaying document `id`, with its state. Background per-file
    /// work (syntax highlighting) routes its results through this; `None` means
    /// the document is gone and the result must be dropped.
    pub(crate) fn document_target_mut(&mut self, id: u64) -> Option<(TabId, &mut TabState)> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.state.session.document_id == id)
            .map(|tab| (tab.id, &mut tab.state))
    }

    /// Per-switch cleanup for the tab being left. Everything the user was doing
    /// *inside* a tab (a draft, marks, a find, a half-typed description) is
    /// owned by that tab and simply stays there; what's left is the window-level
    /// popup menu, and a description editor whose block on the switch the caller
    /// has already cleared.
    fn leave_active_tab(&mut self) {
        self.menu = None;
        self.active_mut().description_editor = None;
    }

    /// Whether the active tab's description editor refuses to let go — an
    /// unsaved edit or an in-flight save. Flags itself in the UI and blocks the
    /// switch/close so the text isn't silently dropped.
    fn description_editor_blocks_switch(&mut self) -> bool {
        match self.active_mut().description_editor.as_mut() {
            Some(editor) if editor.is_dirty() || editor.saving_activity.is_some() => {
                editor.switch_blocked = true;
                true
            }
            _ => false,
        }
    }

    /// Switch to the tab `id`: leave the current one, make `id` active, push its
    /// saved scroll back into the shared widget state, and kick a load if it
    /// hasn't loaded yet (or its load was abandoned while backgrounded). A
    /// fully-loaded tab is switched to instantly and losslessly — its state was
    /// never moved out from under it.
    pub(crate) fn activate_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(target) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        if target == self.active {
            return Task::none();
        }
        if self.description_editor_blocks_switch() {
            return Task::none();
        }
        self.leave_active_tab();
        // Persist the new active tab so it's re-focused next launch.
        self.mark_geometry_dirty();
        self.active = target;
        self.on_active_tab_changed();
        self.ensure_active_loaded()
    }

    /// Shared tail of every path that changes which tab is on screen. The
    /// sidebar/diff widgets' scroll offsets and shaped-paragraph caches are
    /// shared across tabs, so push the new tab's saved positions back in and
    /// drop the cache the previous tab populated — its `(file, hunk, line)` keys
    /// map to that tab's text, not this one's.
    fn on_active_tab_changed(&mut self) {
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        self.document_version = self.document_version.wrapping_add(1);
    }

    /// Kick a load for the active tab when activating it. A tab that has never
    /// loaded — or whose load was abandoned while backgrounded — has
    /// `status != Loaded`, so activating it (re)starts the streaming load. An
    /// already-loaded tab instead gets a cheap freshness re-check: the fs/op-log
    /// watcher follows only the *active* repo, so external ops/edits to a
    /// backgrounded tab go unseen until we return to it. A `Focus` snapshot
    /// reconciles that — its op-fingerprint dedup in `RepositorySnapshotLoaded`
    /// makes it a no-op (no graph re-walk) when nothing changed, and a full
    /// reload only when an external op actually landed.
    pub(crate) fn ensure_active_loaded(&mut self) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        // A streaming load that kept running while this tab was backgrounded
        // (batches route by version) is still the live load — re-kicking would
        // double it and discard its progress.
        let streaming = self.active().session.load.is_some();
        // A GitHub-PR tab has no local repository: (re)stream it when it
        // hasn't loaded and isn't mid-stream; a loaded one has no
        // watcher/snapshot machinery to re-arm.
        if let Some(spec) = self.active_pr_spec().cloned() {
            return if streaming || matches!(self.active().session.status, LoadStatus::Loaded) {
                Task::none()
            } else {
                self.kick_pr_load(spec)
            };
        }
        if self.active().session.repository.is_none() {
            return Task::none();
        }
        if matches!(self.active().session.status, LoadStatus::Loaded) {
            // The Focus snapshot subsumes any refresh coalesced while the tab
            // was backgrounded; clear it so it can't fire a redundant one
            // later. The empty-status pass re-runs here because a load that
            // finished off-screen skipped it (its results are active-only).
            self.active_mut().session.pending_refresh = None;
            Task::batch([
                self.start_repository_snapshot(tab, RefreshOrigin::Focus),
                self.resolve_empty_status(tab),
            ])
        } else if streaming {
            Task::none()
        } else {
            self.kick_initial_load()
        }
    }

    /// The active tab's PR spec, or `None` when it views a local repository
    /// (or no tab is open).
    pub(crate) fn active_pr_spec(&self) -> Option<&github::PrSpec> {
        match &self.tabs.get(self.active)?.source {
            TabSource::GitHubPr(spec) => Some(spec),
            TabSource::Repo { .. } => None,
        }
    }

    /// Close the tab `id`. Closing an inactive tab just drops it — and with it
    /// everything it owned, so a draft or a set of marks can never outlive the
    /// rows they name. Closing the active tab activates a neighbour (previous,
    /// else next), or falls back to the empty state when it was the last tab.
    pub(crate) fn close_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        let closing_active = index == self.active;
        if closing_active {
            if self.description_editor_blocks_switch() {
                return Task::none();
            }
            self.leave_active_tab();
        }
        // The open-tab set is changing — re-persist the session.
        self.mark_geometry_dirty();

        if !closing_active {
            self.tabs.remove(index);
            if index < self.active {
                self.active -= 1;
            }
            return Task::none();
        }

        // Prefer the previous neighbour, matching the design's close behaviour.
        let neighbour = (index > 0).then(|| self.tabs[index - 1].id);
        self.tabs.remove(index);
        self.active = neighbour
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        if self.tabs.is_empty() {
            // Back to the welcome screen. Reset the no-tab state so a prior
            // stint there (a failed `--path`, say) doesn't resurface behind it.
            self.no_tab = TabState::empty();
        }
        self.on_active_tab_changed();
        self.ensure_active_loaded()
    }

    /// Resolve `raw` to a repository — or a GitHub PR reference — and open it
    /// as a tab (or focus it if it's already open). On failure the dialog
    /// stays open with the reason shown.
    pub(crate) fn open_repository(&mut self, raw: &str) -> Task<Message> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Task::none();
        }
        // A PR URL / `owner/repo#123` opens a PR tab; anything else is a path.
        if let Some(spec) = github::PrSpec::parse(trimmed) {
            return self.open_github_pr(spec);
        }
        match prepare_repository(&expand_user_path(trimmed)) {
            Ok(repository) => {
                self.open_repo_dialog = None;
                self.push_recent_repo(&repository.root);
                // Re-persist the session with the newly-opened repo.
                self.mark_geometry_dirty();
                if let Some(existing) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.root() == Some(repository.root.as_path()))
                {
                    let id = existing.id;
                    return self.activate_tab(id);
                }
                let (owner, name) = repo_label(&repository.root);
                let source = TabSource::Repo {
                    vcs: repository.vcs,
                    root: repository.root.clone(),
                };
                self.push_tab(
                    owner,
                    name,
                    source,
                    TabState::unloaded(Some(repository), None),
                )
            }
            Err(error) => {
                let message = format!("{error:#}");
                if let Some(dialog) = self.open_repo_dialog.as_mut() {
                    dialog.error = Some(message);
                }
                Task::none()
            }
        }
    }

    /// Open `spec` as a GitHub-PR tab (or focus it if already open). The diff
    /// streams from the `gh` CLI; the tab has no local repository, so the
    /// graph/watcher/mutation machinery stays disabled (`session.repository`
    /// is `None`) and only the streamed document renders.
    pub(crate) fn open_github_pr(&mut self, spec: github::PrSpec) -> Task<Message> {
        self.open_repo_dialog = None;
        self.mark_geometry_dirty();
        let source = TabSource::GitHubPr(spec.clone());
        if let Some(existing) = self.tabs.iter().find(|tab| tab.source == source) {
            let id = existing.id;
            return self.activate_tab(id);
        }
        let owner = spec.owner.clone();
        let name = spec.label();
        let state = TabState::unloaded_pr(&spec);
        self.push_tab(owner, name, source, state)
    }

    /// Append a new tab and make it active, kicking its load. Shared tail of
    /// the repo and PR open paths.
    fn push_tab(
        &mut self,
        owner: String,
        name: String,
        source: TabSource,
        state: TabState,
    ) -> Task<Message> {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let was_empty = self.tabs.is_empty();
        self.tabs.push(Tab {
            id,
            owner,
            name,
            source,
            state,
        });
        if was_empty {
            // No active tab to switch from — the new one is simply it.
            self.active = 0;
            self.on_active_tab_changed();
            self.ensure_active_loaded()
        } else {
            self.activate_tab(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, CodeTypography};
    use crate::theme::ThemePreference;
    use diffui_core::DiffDocument;

    /// A blank app with no tabs. Built field-by-field rather than through
    /// [`Diffui::new`], which reads the config file and the saved session and
    /// measures fonts through a renderer this headless run doesn't have.
    fn app() -> Diffui {
        Diffui {
            app_focused: true,
            selected_theme: ThemePreference::System,
            system_theme: iced_theme::Mode::None,
            sidebar_width: sidebar::DEFAULT_WIDTH,
            diff_wrap: true,
            diff_split: false,
            sidebar_file_cache: Default::default(),
            sidebar_min_width: 0.0,
            window_size: Size::new(1280.0, 800.0),
            window_position: None,
            geometry_dirty_since: None,
            zoom_anim: None,
            zoom_restore: None,
            config: AppConfig {
                ui_font: iced::Font::DEFAULT,
                mono_font: iced::Font::MONOSPACE,
                multi_click_ms: 350,
                theme: ThemePreference::System,
                code_type: CodeTypography::default(),
            },
            palette: None,
            recents: Recents::default(),
            sidebar_file_reveal_token: 0,
            scroll_restore_token: 0,
            document_version: 0,
            tabs: Vec::new(),
            active: 0,
            no_tab: TabState::empty(),
            next_tab_id: 0,
            next_load_version: 0,
            next_document_id: 0,
            open_repo_dialog: None,
            recent_repos: Vec::new(),
            next_activity_id: 0,
            mutation_queue: Default::default(),
            menu: None,
            confirm: None,
            activity_popover_open: false,
            hovered: None,
            toasts: Vec::new(),
            next_toast_id: 0,
            modifiers: keyboard::Modifiers::default(),
        }
    }

    /// A git repository at `root` — git so `TabState::unloaded` takes its
    /// no-I/O default-revset path instead of reading jj's config.
    fn repository(root: &str) -> Repository {
        Repository {
            root: PathBuf::from(root),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        }
    }

    /// Append a loaded-enough tab for `root` and return its id. Bypasses
    /// `push_tab` so no load is kicked and the caller controls which tab ends
    /// up active.
    fn push_tab(ui: &mut Diffui, root: &str) -> TabId {
        let repository = repository(root);
        let id = TabId(ui.next_tab_id);
        ui.next_tab_id += 1;
        ui.tabs.push(Tab {
            id,
            owner: String::new(),
            name: root.to_owned(),
            source: TabSource::Repo {
                vcs: repository.vcs,
                root: repository.root.clone(),
            },
            state: TabState::unloaded(Some(repository), Some(String::new())),
        });
        id
    }

    fn document(path: &str) -> DiffDocument {
        DiffDocument {
            files: vec![diffui_core::DiffFile {
                path: path.to_owned(),
                old_path: None,
                status: diffui_core::DiffFileStatus::Modified,
                additions: 1,
                deletions: 0,
                hunks: Vec::new(),
            }],
            total_additions: 1,
            total_deletions: 0,
        }
    }

    #[test]
    fn diff_loaded_for_a_background_tab_leaves_the_active_one_alone() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/first");
        let second = push_tab(&mut ui, "/tmp/second");
        ui.active = 1;
        assert_eq!(ui.active_tab_id(), Some(second));

        // The backgrounded tab is mid-switch to a commit; its diff lands while
        // the other tab is on screen.
        let revision = RevisionSelection::Commit("abc".to_owned());
        ui.tab_mut(first).unwrap().session.pending_revision = Some(revision.clone());
        let _ = ui.update(Message::DiffLoaded(
            first,
            revision.clone(),
            Box::new(Ok((document("a.rs"), None))),
        ));

        let background = ui.tab_mut(first).unwrap();
        assert_eq!(background.session.selected_revision, revision);
        assert_eq!(background.session.document.files.len(), 1);
        assert_eq!(background.session.document.files[0].path, "a.rs");
        // The tab on screen never saw it.
        assert_eq!(
            ui.active().session.selected_revision,
            RevisionSelection::WorkingCopy
        );
        assert!(ui.active().session.document.files.is_empty());
    }

    #[test]
    fn closing_the_active_tab_takes_its_draft_with_it() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/first");
        let second = push_tab(&mut ui, "/tmp/second");
        ui.active = 1;

        let draft = diffui_core::OpDraft::squash(diffui_core::DraftSource {
            selection: RevisionSelection::Commit("abc".to_owned()),
            commit_id: "abc".to_owned(),
            label: "abc".to_owned(),
        });
        let state = ui.tab_mut(second).unwrap();
        state.op_draft = Some(DraftUi::new(draft));
        state.revision_multi_selection = vec!["abc".to_owned()];

        let _ = ui.close_tab(second);

        // The draft named rows of the closed tab; the neighbour inherits none
        // of it — there is no shared slot for it to be left in.
        assert_eq!(ui.active_tab_id(), Some(first));
        assert!(ui.active().op_draft.is_none());
        assert!(ui.active().revision_multi_selection.is_empty());
    }

    #[test]
    fn mutation_completing_for_a_background_tab_writes_that_tabs_selection() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/first");
        let second = push_tab(&mut ui, "/tmp/second");
        ui.active = 1;

        let on_a_commit = RevisionSelection::Commit("abc".to_owned());
        ui.tab_mut(first).unwrap().session.selected_revision = on_a_commit.clone();
        ui.tab_mut(second).unwrap().session.selected_revision = on_a_commit.clone();

        let (activity_id, progress) = ui.begin_activity(first, "Abandon", false);
        let pending = PendingMutation {
            repository: repository("/tmp/first"),
            op: mutations::MutationOp::Abandon {
                targets: vec![on_a_commit],
            },
            tab_id: first,
            activity_id,
            progress,
            allow_immutable: false,
        };
        let _ = ui.update(Message::MutationCompleted(
            Box::new(pending),
            Box::new(Ok(mutations::MutationOutcome {
                message: "Abandoned".to_owned(),
                moved_working_copy: true,
                rewritten_commit: None,
                output: Vec::new(),
                operation_id: None,
            })),
        ));

        assert_eq!(
            ui.tab_mut(first).unwrap().session.selected_revision,
            RevisionSelection::WorkingCopy
        );
        // The tab on screen keeps the revision it was browsing.
        assert!(matches!(
            ui.active().session.selected_revision,
            RevisionSelection::Commit(_)
        ));
    }
}
