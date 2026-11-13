//! Multi-repo tab lifecycle for [`Diffui`]: the accessors that resolve a tab's
//! state, activating and closing tabs, and opening a repository as a new tab.
//! Split out of `update.rs` to keep that module focused on handling state
//! changes.

use super::*;
use crate::Cx;

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

    /// The state owned by `tab`, or `None` once that tab has closed. Work
    /// addressed to a tab reads it through this, never through
    /// [`Self::active`], so a result that outlives its tab or lands after a
    /// switch can't act on a stranger.
    pub(crate) fn tab(&self, tab: TabId) -> Option<&TabState> {
        self.tabs
            .iter()
            .find(|candidate| candidate.id == tab)
            .map(|candidate| &candidate.state)
    }

    /// [`Self::tab`], mutably: the lookup every routed async completion goes
    /// through.
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
        self.pop_mode(ModeKind::Menu);
        self.active_mut().pop_mode(ModeKind::Description);
    }

    /// Whether the active tab's description editor refuses to let go — an
    /// unsaved edit or an in-flight save. Flags itself in the UI and blocks the
    /// switch/close so the text isn't silently dropped.
    fn description_editor_blocks_switch(&mut self) -> bool {
        match self.active_mut().description_editor_mut() {
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
    pub(crate) fn activate_tab(&mut self, id: TabId, cx: &mut Cx) {
        let Some(target) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        if target == self.active {
            return;
        }
        if self.description_editor_blocks_switch() {
            return;
        }
        self.leave_active_tab();
        // Persist the new active tab so it's re-focused next launch.
        self.mark_geometry_dirty();
        self.active = target;
        {
            self.on_active_tab_changed(cx);
            self.ensure_active_loaded();
        }
    }

    /// Shared tail of every path that changes which tab is on screen. The
    /// sidebar/diff widgets' scroll offsets and shaped-paragraph caches are
    /// shared across tabs, so push the new tab's saved positions back in and
    /// drop the cache the previous tab populated — its `(file, hunk, line)` keys
    /// map to that tab's text, not this one's.
    ///
    /// It is also where a description edit deferred while its tab was off
    /// screen finally opens: the diff it was waiting for has already landed,
    /// so the editor has details to show and the keyboard is now this tab's.
    fn on_active_tab_changed(&mut self, cx: &mut Cx) {
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        self.document_version = self.document_version.wrapping_add(1);
        if let Some(tab) = self.active_tab_id() {
            self.open_pending_description_edit(tab, cx)
        }
    }

    /// Bring the active tab up to date on activation.
    ///
    /// A tab that has never loaded — or whose load was abandoned while
    /// backgrounded — restarts it. An already-loaded tab gets a freshness
    /// re-check instead: its actor keeps watching while the tab is off screen,
    /// but a `Focus` snapshot reconciles anything the projection coalesced
    /// while nothing was rendering. Its op-fingerprint dedup makes that a
    /// no-op when nothing changed.
    pub(crate) fn ensure_active_loaded(&mut self) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        // The actor hands the handle over on its first event; until then there
        // is nothing to send to, and that event kicks the load itself.
        if self.active().handle.is_none() {
            return;
        }
        self.active_mut().session.pending_refresh = None;
        self.start_tab_load(tab)
    }

    /// Close the tab `id`. Closing an inactive tab just drops it — and with it
    /// everything it owned, so a draft or a set of marks can never outlive the
    /// rows they name. Closing the active tab activates a neighbour (previous,
    /// else next), or falls back to the empty state when it was the last tab.
    pub(crate) fn close_tab(&mut self, id: TabId, cx: &mut Cx) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let closing_active = index == self.active;
        if closing_active {
            if self.description_editor_blocks_switch() {
                return;
            }
            self.leave_active_tab();
        }
        // The open-tab set is changing — re-persist the session.
        self.mark_geometry_dirty();
        self.repo_layouts.remember(&self.tabs[index]);

        if !closing_active {
            self.tabs.remove(index);
            if index < self.active {
                self.active -= 1;
            }
            return;
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
        {
            self.on_active_tab_changed(cx);
            self.ensure_active_loaded();
        }
    }

    /// Move the tab `id` to position `to` (clamped to the strip), shifting the
    /// tabs between to fill its old slot. Purely an ordering change: the same
    /// tab stays active, and nothing a tab owns moves with respect to it —
    /// tabs are addressed by [`TabId`] everywhere but `active`, which is
    /// re-resolved here.
    pub(crate) fn move_tab(&mut self, id: TabId, to: usize) {
        let Some(from) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let to = to.min(self.tabs.len() - 1);
        if from == to {
            return;
        }
        let active = self.active_tab_id();
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(index) = self.tabs.iter().position(|tab| Some(tab.id) == active) {
            self.active = index;
        }
        // The open-tab order is part of the persisted session.
        self.mark_geometry_dirty();
    }

    /// Resolve `raw` to a repository — or a pull request reference — and open it
    /// as a tab (or focus it if it's already open). On failure the dialog
    /// stays open with the reason shown.
    pub(crate) fn open_repository(&mut self, raw: &str, cx: &mut Cx) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        // A PR or MR URL, `owner/repo#123` or `group/repo!7` opens a PR tab;
        // anything else is a path.
        if let Some(spec) = diffui_core::forge::ForgePr::parse(trimmed) {
            // A bare `owner/repo#123` typed from a repository whose remote
            // lives on a company host means that host's PR.
            let remotes = self
                .tabs
                .get(self.active)
                .and_then(|tab| tab.state.repository.as_ref())
                .map(|repository| diffui_core::forge::remotes(&repository.root))
                .unwrap_or_default();
            let spec = spec.with_host_from_remotes(remotes.iter().map(|(_, url)| url.as_str()));
            return self.open_pull_request(spec, cx);
        }
        match prepare_repository(&expand_user_path(trimmed)) {
            Ok(repository) => {
                self.pop_mode(ModeKind::OpenRepo);
                self.push_recent_repo(&repository.root);
                // Re-persist the session with the newly-opened repo.
                self.mark_geometry_dirty();
                if let Some(existing) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.root() == Some(repository.root.as_path()))
                {
                    let id = existing.id;
                    return self.activate_tab(id, cx);
                }
                let (owner, name) = repo_label(&repository.root);
                let source = TabSource::Repo {
                    vcs: repository.vcs,
                    root: repository.root.clone(),
                };
                let state = self.repo_layouts.tab_state(repository);
                self.push_tab(owner, name, source, state, cx)
            }
            Err(error) => {
                let message = format!("{error:#}");
                if let Some(dialog) = self.open_repo_dialog_mut() {
                    dialog.error = Some(message);
                }
            }
        }
    }

    /// The open-repo prompt's path changed: drop the stale error and
    /// highlight, and read the folder it now points into unless that listing
    /// is already here or on its way.
    pub(crate) fn set_open_repo_path(&mut self, path: String, cx: &mut Cx) {
        let Some(dialog) = self.open_repo_dialog_mut() else {
            return;
        };
        dialog.path = path;
        dialog.error = None;
        dialog.selected = None;
        let Some(query) = path_browse::Query::parse(&dialog.path) else {
            return;
        };
        self.probe_open_repo_path(cx);
        let Some(dialog) = self.open_repo_dialog_mut() else {
            return;
        };
        if dialog
            .listing
            .as_ref()
            .is_some_and(|listing| listing.dir == query.dir)
        {
            dialog.listed_query = Some(query);
            return;
        }
        let dir = query.dir;
        if dialog.listing_pending.as_ref() == Some(&dir) {
            return;
        }
        dialog.listing_pending = Some(dir.clone());
        // A folder can be slow to read (a network mount, a huge directory),
        // and each entry is stat'ed to spot repositories.
        Self::run_async(
            async move {
                let fallback = dir.clone();
                tokio::task::spawn_blocking(move || path_browse::list(dir))
                    .await
                    .unwrap_or_else(|_| path_browse::Listing {
                        dir: fallback,
                        entries: Vec::new(),
                    })
            },
            cx,
            |this, listing, _| this.on_open_repo_listed(listing),
        );
    }

    /// Replace the prompt's path from outside the field (a completion, a
    /// clicked folder), keeping the field focused with the caret at the end.
    pub(crate) fn browse_open_repo_path(&mut self, path: String, cx: &mut Cx) {
        self.set_open_repo_path(path, cx);
        crate::input::focus(tab_bar::OPEN_REPO_INPUT_ID, cx);
    }

    /// Act on folder suggestion `row`: open it when it is a repository,
    /// otherwise browse into it.
    pub(crate) fn pick_open_repo_suggestion(&mut self, row: usize, cx: &mut Cx) {
        let picked = self.open_repo_dialog().and_then(|dialog| {
            let (query, rows) = dialog.suggestions()?;
            let entry = rows.get(row)?;
            Some(match entry.vcs {
                Some(_) => Ok(query.path_of(entry)),
                None => Err(query.complete(entry)),
            })
        });
        match picked {
            Some(Ok(repository)) => self.open_repository(&repository, cx),
            Some(Err(folder)) => self.browse_open_repo_path(folder, cx),
            None => {}
        }
    }

    /// Open `spec` as a pull-request tab (or focus it if already open). The
    /// diff streams from its forge; the tab has no local repository, so the
    /// graph/watcher/mutation machinery stays disabled (`repository`
    /// is `None`) and only the streamed document renders.
    pub(crate) fn open_pull_request(&mut self, spec: diffui_core::forge::ForgePr, cx: &mut Cx) {
        self.pop_mode(ModeKind::OpenRepo);
        self.mark_geometry_dirty();
        let source = TabSource::ForgePr(spec.clone());
        if let Some(existing) = self.tabs.iter().find(|tab| tab.source == source) {
            let id = existing.id;
            return self.activate_tab(id, cx);
        }
        let owner = spec.owner.clone();
        let name = spec.label();
        let state = TabState::unloaded_pr();
        self.push_tab(owner, name, source, state, cx)
    }

    /// Append a new tab and make it active, kicking its load. Shared tail of
    /// the repo and PR open paths.
    fn push_tab(
        &mut self,
        owner: String,
        name: String,
        source: TabSource,
        state: TabState,
        cx: &mut Cx,
    ) {
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
            {
                self.on_active_tab_changed(cx);
                self.ensure_active_loaded();
            }
        } else {
            self.activate_tab(id, cx)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::theme::ThemePreference;
    use crate::typography::CodeTypography;
    use diffui_core::DiffDocument;

    /// A blank app with no tabs. Built field-by-field rather than through
    /// [`Diffui::new`], which reads the config file and the saved session and
    /// measures fonts through a renderer this headless run doesn't have.
    /// Runs `f` with a [`Cx`] on a headless app, for tests of update code: most of
    /// it takes one, even where a test never lets a spawned task run. The context's
    /// entity is not the test's own `Diffui`, so nothing reaches back into it.
    pub(crate) fn with_cx<R>(f: impl FnOnce(&mut crate::Cx) -> R) -> R {
        use gpui_kit::{AppContext as _, TestAppContext};
        let mut cx = TestAppContext::single();
        let entity = cx.update(|cx| cx.new(|_| app()));
        entity.update(&mut cx, |_, cx| f(cx))
    }

    pub(crate) fn app() -> Diffui {
        Diffui {
            app_focused: true,
            selected_theme: ThemePreference::System,
            system_theme: crate::theme::SystemAppearance::Dark,
            sidebar_width: sidebar::DEFAULT_WIDTH,
            file_nav_width: diff_panel::DEFAULT_FILE_NAV_WIDTH,
            diff_wrap: true,
            diff_split: false,
            commit_details_collapsed: false,
            svg_source: false,
            media: Default::default(),
            sidebar_file_cache: Default::default(),
            sidebar_min_width: 0.0,
            window_size: Size::new(1280.0, 800.0),
            window_position: None,
            window_display: None,
            geometry_dirty_since: None,
            config: AppConfig {
                ui_font: crate::theme::Font::DEFAULT,
                mono_font: crate::theme::Font::MONOSPACE,
                multi_click_ms: 350,
                theme: ThemePreference::System,
                code_type: CodeTypography::default(),
                refresh: diffui_core::RefreshMode::Auto,
                locked: Default::default(),
            },
            config_files: Default::default(),
            registry: Default::default(),
            user_config: Default::default(),
            user_runs: Default::default(),
            keymap: Keymap::build(&crate::commands::Registry::builtin(), &Default::default()).0,
            modes: Vec::new(),
            focus_before_modes: None,
            recents: Recents::default(),
            sidebar_file_reveal_token: 0,
            scroll_restore_token: 0,
            document_version: 0,
            tabs: Vec::new(),
            active: 0,
            no_tab: TabState::empty(),
            next_tab_id: 0,
            next_document_id: 0,
            recent_repos: Vec::new(),
            repo_layouts: Default::default(),
            next_activity_id: 0,
            hovered: None,
            panel_peek: None,
            tab_strip: Default::default(),
            toasts: Vec::new(),
            next_toast_id: 0,
            window: None,
            background: Default::default(),
            palette_query: Default::default(),
            command_line_settle: Default::default(),
            panes: None,
            forge: Default::default(),
            plugins: Default::default(),
            #[cfg(test)]
            renders: 0,
            modifiers: crate::keys::Modifiers::default(),
        }
    }

    /// A git repository at `root` — git so `TabState::unloaded` takes its
    /// no-I/O default-revset path instead of reading jj's config.
    pub(crate) fn repository(root: &str) -> Repository {
        Repository {
            root: PathBuf::from(root),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        }
    }

    /// Append a loaded-enough tab for `root` and return its id. Bypasses
    /// `push_tab` so no load is kicked and the caller controls which tab ends
    /// up active.
    pub(crate) fn push_tab(ui: &mut Diffui, root: &str) -> TabId {
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
                collapsed: None,
                blob: None,
                mode_change: None,
            }],
            total_additions: 1,
            total_deletions: 0,
            range: None,
        }
    }

    #[test]
    fn a_diff_landing_for_a_background_tab_leaves_the_active_one_alone() {
        crate::tabs::tests::with_cx(|cx| {
            let mut ui = app();
            let first = push_tab(&mut ui, "/tmp/first");
            let second = push_tab(&mut ui, "/tmp/second");
            ui.active = 1;
            assert_eq!(ui.active_tab_id(), Some(second));

            // The backgrounded tab is mid-switch to a commit; its diff lands while
            // the other tab is on screen. Routing is by job, and the job belongs to
            // that tab's slot — two tabs both pending on `@` can no longer swap
            // each other's contents.
            let revision = RevisionSelection::Commit("abc".to_owned());
            let effects = ui
                .tab_mut(first)
                .unwrap()
                .session
                .load_diff(revision.clone());
            let job = match effects.first() {
                Some(diffui_core::Effect::Send(diffui_core::Command::LoadDiff { job, .. })) => *job,
                other => panic!("expected a diff load, got {other:?}"),
            };
            let repo = ui.tabs[0].repo_id().expect("a repo tab has an id");
            ui.act(cx, |ui, cx| {
                ui.on_repo_event(
                    diffui_core::Event::new(
                        repo,
                        diffui_core::Payload::DiffLoaded {
                            job,
                            revision: revision.clone(),
                            document: document("a.rs"),
                            details: None,
                        },
                    ),
                    cx,
                )
            });

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
        });
    }

    fn details(description: &str) -> diffui_core::RevisionDetails {
        diffui_core::RevisionDetails {
            commit_id: "abc".to_owned(),
            change_id: None,
            bookmarks: Vec::new(),
            author: Default::default(),
            committer: None,
            signature: None,
            description: description.to_owned(),
            parents: Vec::new(),
        }
    }

    /// Land the diff `tab` is waiting on for `revision`, the way the actor
    /// reports it.
    fn land_diff(ui: &mut Diffui, tab: TabId, revision: &RevisionSelection, cx: &mut crate::Cx) {
        let effects = ui.tab_mut(tab).unwrap().session.load_diff(revision.clone());
        let job = effects
            .iter()
            .find_map(|effect| match effect {
                diffui_core::Effect::Send(diffui_core::Command::LoadDiff { job, .. }) => Some(*job),
                _ => None,
            })
            .expect("a diff load");
        let index = ui.tabs.iter().position(|candidate| candidate.id == tab);
        let repo = ui.tabs[index.expect("the tab is open")]
            .repo_id()
            .expect("a repo tab has an id");
        ui.act(cx, |ui, cx| {
            ui.on_repo_event(
                diffui_core::Event::new(
                    repo,
                    diffui_core::Payload::DiffLoaded {
                        job,
                        revision: revision.clone(),
                        document: document("a.rs"),
                        details: Some(details("first line")),
                    },
                ),
                cx,
            )
        });
    }

    /// "Edit description" on a row that isn't selected browses to it first and
    /// only then opens the editor. Nothing else re-reads that pending target,
    /// so if the diff-landing path drops it the menu command silently does
    /// nothing.
    #[test]
    fn editing_the_description_of_another_revision_opens_once_its_diff_lands() {
        crate::tabs::tests::with_cx(|cx| {
            let mut ui = app();
            let tab = push_tab(&mut ui, "/tmp/only");
            ui.tab_mut(tab).unwrap().session.capabilities.mutate = true;

            let revision = RevisionSelection::Commit("abc".to_owned());
            ui.perform(
                Action::EditDescription {
                    target: Some(revision.clone()),
                },
                cx,
            );
            assert_eq!(
                ui.active().pending_description_edit.as_ref(),
                Some(&revision),
                "the edit waits for the revision it named"
            );
            assert!(ui.active().description_editor().is_none());

            land_diff(&mut ui, tab, &revision, cx);

            let state = ui.active();
            assert_eq!(state.pending_description_edit, None);
            let editor = state
                .description_editor()
                .expect("the editor opened over the revision that landed");
            assert_eq!(editor.target, revision);
            assert_eq!(editor.text(), "first line");
        });
    }

    /// The same edit, but its diff lands while another tab is on screen. The
    /// editor is a focused control, so it must not open under the visible tab
    /// — and the pending edit must survive to open on activation instead.
    #[test]
    fn a_deferred_description_edit_opens_when_its_tab_comes_back() {
        crate::tabs::tests::with_cx(|cx| {
            let mut ui = app();
            let first = push_tab(&mut ui, "/tmp/first");
            let _second = push_tab(&mut ui, "/tmp/second");
            ui.tab_mut(first).unwrap().session.capabilities.mutate = true;

            let revision = RevisionSelection::Commit("abc".to_owned());
            ui.tab_mut(first).unwrap().pending_description_edit = Some(revision.clone());
            ui.active = 1;
            land_diff(&mut ui, first, &revision, cx);

            assert!(
                ui.active().description_editor().is_none(),
                "the tab on screen keeps the keyboard"
            );
            assert_eq!(
                ui.tab_mut(first).unwrap().pending_description_edit.as_ref(),
                Some(&revision),
                "the edit is held, not dropped"
            );

            ui.activate_tab(first, cx);

            assert_eq!(ui.active().pending_description_edit, None);
            assert_eq!(
                ui.active()
                    .description_editor()
                    .expect("the editor opened on activation")
                    .target,
                revision
            );
        });
    }

    #[test]
    fn moving_a_tab_reorders_the_strip_without_changing_which_is_active() {
        crate::tabs::tests::with_cx(|_| {
            let mut ui = app();
            let first = push_tab(&mut ui, "/tmp/first");
            let second = push_tab(&mut ui, "/tmp/second");
            let third = push_tab(&mut ui, "/tmp/third");
            ui.active = 1;
            let order = |ui: &Diffui| ui.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>();

            // The active tab dragged to the end of the strip.
            ui.move_tab(second, 2);
            assert_eq!(order(&ui), [first, third, second]);
            assert_eq!(ui.active_tab_id(), Some(second));

            // A background tab crossing the active one: `active` is an index, so
            // it has to follow the tab it named.
            ui.move_tab(first, 2);
            assert_eq!(order(&ui), [third, second, first]);
            assert_eq!(ui.active_tab_id(), Some(second));

            // Past the end clamps to the last slot.
            ui.move_tab(third, 9);
            assert_eq!(order(&ui), [second, first, third]);
            assert_eq!(ui.active_tab_id(), Some(second));

            // The next launch restores the strip in this order.
            assert!(ui.geometry_dirty_since.is_some());
            assert_eq!(
                ui.current_window_state().open_repos,
                ["/tmp/second", "/tmp/first", "/tmp/third"]
            );
        });
    }

    #[test]
    fn moving_a_tab_onto_its_own_slot_changes_nothing() {
        crate::tabs::tests::with_cx(|_| {
            let mut ui = app();
            let first = push_tab(&mut ui, "/tmp/first");
            let _second = push_tab(&mut ui, "/tmp/second");

            ui.move_tab(first, 0);
            ui.move_tab(TabId(99), 1);

            assert_eq!(ui.tabs[0].id, first);
            assert!(
                ui.geometry_dirty_since.is_none(),
                "nothing moved, so there is nothing to persist"
            );
        });
    }

    #[test]
    fn closing_the_active_tab_takes_its_draft_with_it() {
        crate::tabs::tests::with_cx(|cx| {
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
            state.push_mode(TabMode::Draft(DraftUi::new(draft)));
            state.revision_multi_selection = vec!["abc".to_owned()];

            ui.close_tab(second, cx);

            // The draft named rows of the closed tab; the neighbour inherits none
            // of it — there is no shared slot for it to be left in.
            assert_eq!(ui.active_tab_id(), Some(first));
            assert!(ui.active().op_draft().is_none());
            assert!(ui.active().revision_multi_selection.is_empty());
        });
    }

    /// Two mutations from one tab: the second waits rather than overwriting
    /// the first's slot, because an overwritten slot leaves the first
    /// completion with no owner — its activity row spins forever and the
    /// reload that should follow it never fires.
    #[test]
    fn a_second_mutation_waits_instead_of_displacing_the_first() {
        crate::tabs::tests::with_cx(|cx| {
            let mut ui = app();
            let tab = push_tab(&mut ui, "/tmp/only");

            let describe = |ui: &mut Diffui, text: &str, _cx: &mut crate::Cx| {
                let (activity_id, _) = ui.begin_activity(tab, "Describe", false);
                ui.run_mutation(PendingMutation {
                    op: mutations::MutationOp::Describe {
                        target: RevisionSelection::WorkingCopy,
                        description: text.to_owned(),
                    },
                    tab_id: tab,
                    activity_id,
                    allow_immutable: false,
                })
            };
            describe(&mut ui, "first", cx);
            let first_job = ui.tab_mut(tab).unwrap().jobs.mutation().unwrap();
            describe(&mut ui, "second", cx);

            let state = ui.tab_mut(tab).unwrap();
            assert_eq!(
                state.jobs.mutation(),
                Some(first_job),
                "the first mutation still owns the slot"
            );
            assert_eq!(state.queued_mutations.len(), 1);

            // The first reports; the second takes the slot it just freed.
            let repo = ui.tabs[0].repo_id().expect("a repo tab has an id");
            ui.act(cx, |ui, cx| {
                ui.on_repo_event(
                    diffui_core::Event::new(
                        repo,
                        diffui_core::Payload::MutationDone {
                            job: first_job,
                            outcome: mutations::MutationOutcome {
                                message: "Described".to_owned(),
                                moved_working_copy: false,
                                rewritten_commit: None,
                                output: Vec::new(),
                                operation_id: None,
                                bookmarks: Vec::new(),
                            },
                        },
                    ),
                    cx,
                )
            });

            let state = ui.tab_mut(tab).unwrap();
            assert!(state.queued_mutations.is_empty(), "the queue drained");
            assert!(
                state.jobs.mutation().is_some_and(|job| job != first_job),
                "the second mutation is now in flight"
            );
        });
    }

    #[test]
    fn a_mutation_completing_for_a_background_tab_writes_that_tabs_selection() {
        crate::tabs::tests::with_cx(|cx| {
            let mut ui = app();
            let first = push_tab(&mut ui, "/tmp/first");
            let second = push_tab(&mut ui, "/tmp/second");
            ui.active = 1;

            let on_a_commit = RevisionSelection::Commit("abc".to_owned());
            ui.tab_mut(first).unwrap().session.selected_revision = on_a_commit.clone();
            ui.tab_mut(second).unwrap().session.selected_revision = on_a_commit;

            let (job, _effects) = ui.tab_mut(first).unwrap().session.mutate(
                mutations::MutationOp::Abandon {
                    targets: vec![RevisionSelection::Commit("abc".to_owned())],
                },
                false,
            );
            let repo = ui.tabs[0].repo_id().expect("a repo tab has an id");
            ui.act(cx, |ui, cx| {
                ui.on_repo_event(
                    diffui_core::Event::new(
                        repo,
                        diffui_core::Payload::MutationDone {
                            job,
                            outcome: mutations::MutationOutcome {
                                message: "Abandoned".to_owned(),
                                moved_working_copy: true,
                                rewritten_commit: None,
                                output: Vec::new(),
                                operation_id: None,
                                bookmarks: Vec::new(),
                            },
                        },
                    ),
                    cx,
                )
            });

            assert_eq!(
                ui.tab_mut(first).unwrap().session.selected_revision,
                RevisionSelection::WorkingCopy
            );
            // The tab on screen keeps the revision it was browsing.
            assert!(matches!(
                ui.active().session.selected_revision,
                RevisionSelection::Commit(_)
            ));
        });
    }
}
