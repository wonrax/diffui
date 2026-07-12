//! Multi-repo tab lifecycle for [`Diffui`]: stashing/restoring the active
//! tab's per-repo state (the `Session` swaps atomically), activating and
//! closing tabs, and opening a repository as a new tab. Split out of
//! `update.rs` to keep that module focused on the message-handling core.

use super::*;

impl Diffui {
    /// Move the active tab's inline view state out into a `RepoState`, leaving
    /// the inline fields at cheap placeholders. Always paired with an
    /// immediate `restore_active_state` of the incoming tab. The `Session` swaps
    /// atomically, so there's no field-by-field list to keep in sync.
    pub(crate) fn stash_active_state(&mut self) -> RepoState {
        // The whole domain + orchestration bundle swaps as one unit — no
        // field-by-field list to keep in sync.
        let mut session = std::mem::take(&mut self.session);
        // In-flight loads survive backgrounding: streaming batches are routed
        // into this stash by version (`load_target_mut`) and one-shot
        // completions (diff switches, graph reloads, revset evals) by tab id
        // (`tab_target_mut`), so a half-done load keeps its progress — and a
        // revset eval that lands off-screen still swaps the stashed graph.
        // Only the snapshot is abandoned: its completion is dropped while
        // backgrounded (activation re-runs a Focus snapshot anyway), so the
        // flag must clear or the stash would sit "busy" forever.
        session.snapshot_pending = false;
        RepoState {
            session,
            file_list_expanded: self.file_list_expanded,
            collapsed_dirs: std::mem::take(&mut self.collapsed_dirs),
            selected_file: self.selected_file,
            revision_reveal_token: self.revision_reveal_token,
            pending_revision_reveal: self.pending_revision_reveal,
            sidebar_scroll_offset: self.sidebar_scroll_offset,
            diff_scroll_offset: self.diff_scroll_offset,
            activities: std::mem::take(&mut self.activities),
            pending_load_activity: self.pending_load_activity.take(),
            main_view: self.main_view,
            source: std::mem::take(&mut self.source),
        }
    }

    /// Move a stashed `RepoState` into the inline fields, making it the active
    /// view. The previous inline state is overwritten (its caller has already
    /// stashed it, or is intentionally discarding it).
    pub(crate) fn restore_active_state(&mut self, state: RepoState) {
        self.session = state.session;
        self.default_revset = self
            .session
            .repository
            .as_ref()
            .map(default_revset)
            .unwrap_or_default();
        // The document moved in with the session; bump `document_version` so the
        // diff view drops the cache the previously-active tab populated — its
        // `(file, hunk, line)` keys map to that tab's text, not this one's.
        self.document_version = self.document_version.wrapping_add(1);
        self.file_list_expanded = state.file_list_expanded;
        self.collapsed_dirs = state.collapsed_dirs;
        self.selected_file = state.selected_file;
        self.revision_reveal_token = state.revision_reveal_token;
        self.pending_revision_reveal = state.pending_revision_reveal;
        self.sidebar_scroll_offset = state.sidebar_scroll_offset;
        self.diff_scroll_offset = state.diff_scroll_offset;
        self.activities = state.activities;
        self.pending_load_activity = state.pending_load_activity;
        self.main_view = state.main_view;
        self.source = state.source;
    }

    /// Switch to the tab `id`: stash the current active tab, restore the
    /// target's state, scroll its selection back into view, and kick a load if
    /// it hasn't loaded yet (or its load was abandoned while backgrounded). A
    /// fully-loaded tab is restored instantly and losslessly.
    pub(crate) fn activate_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(target) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        if target == self.active_tab {
            return Task::none();
        }
        // Persist the new active tab so it's re-focused next launch.
        self.mark_geometry_dirty();

        let current = self.stash_active_state();
        self.tabs[self.active_tab].stash = Some(current);
        self.active_tab = target;
        // Inactive tabs always carry a stash; the fallback only guards against
        // an impossible invariant break.
        let restored = self.tabs[target]
            .stash
            .take()
            .unwrap_or_else(RepoState::empty);
        self.restore_active_state(restored);
        // The sidebar/diff widgets' scroll offsets are shared across tabs, so
        // push this tab's saved positions back in (the restored fields above
        // hold them) over whatever the previous tab left in the widget state.
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        self.ensure_active_loaded()
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
        // A streaming load that kept running while this tab was backgrounded
        // (batches route into the stash) is still the live load — re-kicking
        // would double it and discard its progress.
        let streaming = self.session.load.is_some();
        // A GitHub-PR tab has no local repository: (re)stream it when it
        // hasn't loaded and isn't mid-stream; a loaded one has no
        // watcher/snapshot machinery to re-arm.
        if let Some(spec) = self.active_pr_spec().cloned() {
            return if streaming || matches!(self.session.status, LoadStatus::Loaded) {
                Task::none()
            } else {
                self.kick_pr_load(spec)
            };
        }
        if self.session.repository.is_none() {
            return Task::none();
        }
        if matches!(self.session.status, LoadStatus::Loaded) {
            // The Focus snapshot subsumes any refresh coalesced while the tab
            // was backgrounded; clear it so it can't fire a redundant one
            // later. The empty-status pass re-runs here because a load that
            // finished off-screen skipped it (its results are active-only).
            self.session.pending_refresh = None;
            Task::batch([
                self.start_repository_snapshot(RefreshOrigin::Focus),
                self.resolve_empty_status(),
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
        match &self.tabs.get(self.active_tab)?.source {
            TabSource::GitHubPr(spec) => Some(spec),
            TabSource::Repo { .. } => None,
        }
    }

    /// Close the tab `id`. Closing an inactive tab just drops it; closing the
    /// active tab activates a neighbour (previous, else next), or falls back to
    /// the empty state when it was the last tab.
    pub(crate) fn close_tab(&mut self, id: TabId) -> Task<Message> {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return Task::none();
        };
        // The open-tab set is changing — re-persist the session.
        self.mark_geometry_dirty();

        if index != self.active_tab {
            self.tabs.remove(index);
            if index < self.active_tab {
                self.active_tab -= 1;
            }
            return Task::none();
        }

        if self.tabs.len() == 1 {
            self.tabs.clear();
            self.active_tab = 0;
            self.restore_active_state(RepoState::empty());
            return Task::none();
        }

        // Prefer the previous neighbour, matching the design's close behaviour.
        let neighbour = if index > 0 { index - 1 } else { 1 };
        let neighbour_id = self.tabs[neighbour].id;
        self.tabs.remove(index);
        self.active_tab = self
            .tabs
            .iter()
            .position(|tab| tab.id == neighbour_id)
            .unwrap_or(0);
        // Overwriting the inline fields here discards the closed tab's state.
        let restored = self.tabs[self.active_tab]
            .stash
            .take()
            .unwrap_or_else(RepoState::empty);
        self.restore_active_state(restored);
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
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
                let revset = default_revset(&repository);
                let source = TabSource::Repo {
                    vcs: repository.vcs,
                    root: repository.root.clone(),
                };
                self.push_tab(
                    owner,
                    name,
                    source,
                    RepoState::unloaded(Some(repository), revset),
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
        let state = RepoState::unloaded_pr(&spec);
        self.push_tab(owner, name, source, state)
    }

    /// Append a new tab and make it active, kicking its load. Shared tail of
    /// the repo and PR open paths.
    fn push_tab(
        &mut self,
        owner: String,
        name: String,
        source: TabSource,
        state: RepoState,
    ) -> Task<Message> {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let was_empty = self.tabs.is_empty();
        self.tabs.push(Tab {
            id,
            owner,
            name,
            source,
            stash: Some(state),
        });
        if was_empty {
            // No active tab to switch from — check the new one out directly.
            self.active_tab = 0;
            if let Some(state) = self.tabs[0].stash.take() {
                self.restore_active_state(state);
            }
            self.ensure_active_loaded()
        } else {
            self.activate_tab(id)
        }
    }
}
