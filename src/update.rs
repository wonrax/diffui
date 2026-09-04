//! `impl Diffui` — the application reducer (`update`), lifecycle (`new`,
//! `view`, `subscription`, `theme`), and all the orchestration/menu/tab helper
//! methods. Split out of `main.rs`; the type defs + free fns it calls stay at
//! the crate root and are reached via `super::*` / `crate::`.

use super::*;
// `super::*` is a glob, which loses to the prelude for the `column!` macro name
// — import it directly so `column![…]` in `view` resolves. The other widget
// builders (`row!`/`stack!`/`container`) aren't shadowed and come via the glob.
use crate::find::FindMessage;
use crate::menu::MenuMessage;
use crate::palette::PaletteMessage;
use iced::widget::column;

impl Diffui {
    pub(crate) fn new(cli: Cli, saved: WindowState) -> (Self, Task<Message>) {
        let (config, raw_keys) = AppConfig::load();
        // A keymap problem is never fatal: a config written against another
        // build still opens the app, having said once what it had to skip.
        let (keymap, problems) = Keymap::build(&raw_keys);
        for problem in problems {
            eprintln!("diffui: {problem}");
        }
        let sidebar_min_width = sidebar::min_width(config);
        // Restore the persisted sidebar split and window geometry. The window
        // size/position seed the in-memory tracking; the compositor's `Opened`
        // event overwrites them with the real values a frame later, but seeding
        // keeps them correct in between. The sidebar is clamped to that size, so
        // a stale width — from a narrower font config, or from the wider display
        // the file was written on — can't reopen with an unusable split.
        let window_size = saved
            .size()
            .map(|(w, h)| Size::new(w, h))
            .unwrap_or_else(|| window::Settings::default().size);
        let sidebar_width = resize_handle::clamp_width(
            saved
                .sidebar_width
                .filter(|w| w.is_finite() && *w > 0.0)
                .unwrap_or(sidebar::DEFAULT_WIDTH),
            sidebar_min_width,
            window_size.width,
        );
        let window_position = saved.position().map(|(x, y)| Point::new(x, y));

        // Launch precedence: explicit `--path` args win; else restore last
        // session; else fall back to the current directory *only if it's a
        // repo* (the `cd repo && diffui` terminal flow). A `--path` arg is a
        // GitHub PR reference (URL / `owner/repo#123`) or a repository path;
        // session and cwd entries are always local repos (PR tabs are
        // session-only and never persisted). Only `--path` failures surface
        // an error — the user named those this launch. A vanished session
        // repo or a non-repo cwd (e.g. `/` from the Spotlight/Dock launcher)
        // is silently skipped, leaving zero tabs and the welcome screen
        // rather than a "not inside a repository" error.
        enum BootTarget {
            Repo(Repository),
            Pr(github::PrSpec),
        }
        let mut targets: Vec<BootTarget> = Vec::new();
        let mut first_error = None;
        // Two `--path`s (or two saved entries) can resolve to the same root —
        // a plain repeat, or two spellings of one path. Opening it twice would
        // give the repo two tabs racing the same working copy, so keep the
        // first and drop the rest, exactly as `open_repository` does later.
        fn push_repo(targets: &mut Vec<BootTarget>, repository: Repository) {
            let duplicate = targets.iter().any(|target| match target {
                BootTarget::Repo(open) => open.root == repository.root,
                BootTarget::Pr(_) => false,
            });
            if !duplicate {
                targets.push(BootTarget::Repo(repository));
            }
        }
        if !cli.paths.is_empty() {
            for path in &cli.paths {
                if let Some(spec) = github::PrSpec::parse(&path.to_string_lossy()) {
                    if !targets
                        .iter()
                        .any(|target| matches!(target, BootTarget::Pr(open) if *open == spec))
                    {
                        targets.push(BootTarget::Pr(spec));
                    }
                    continue;
                }
                match prepare_repository(path) {
                    Ok(repository) => push_repo(&mut targets, repository),
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(format!("{error:#}"));
                        }
                    }
                }
            }
        } else {
            for path in &saved.open_repos {
                if let Ok(repository) = prepare_repository(Path::new(path)) {
                    push_repo(&mut targets, repository);
                }
            }
            if targets.is_empty()
                && let Ok(cwd) = std::env::current_dir()
                && let Ok(repository) = prepare_repository(&cwd)
            {
                push_repo(&mut targets, repository);
            }
        }

        // Re-focus the tab that was active last session (matched by repo root),
        // falling back to the first.
        let active_index = saved
            .active_repo
            .as_deref()
            .and_then(|active| {
                targets.iter().position(|target| match target {
                    BootTarget::Repo(repository) => repository.root.to_string_lossy() == active,
                    BootTarget::Pr(_) => false,
                })
            })
            .unwrap_or(0);

        // A repo's persisted revset (keyed by root); `None` starts it on the
        // repo's own default, which `TabState::unloaded` reads once.
        let revset_for = |repository: &Repository| -> Option<String> {
            saved
                .revsets
                .get(&repository.root.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
                .cloned()
        };

        let mut next_tab_id = 0u64;
        let mut tabs = Vec::with_capacity(targets.len());
        for target in &targets {
            let (owner, name, source, state) = match target {
                BootTarget::Repo(repository) => {
                    let (owner, name) = repo_label(&repository.root);
                    let source = TabSource::Repo {
                        vcs: repository.vcs,
                        root: repository.root.clone(),
                    };
                    let state =
                        TabState::unloaded(Some(repository.clone()), revset_for(repository));
                    (owner, name, source, state)
                }
                BootTarget::Pr(spec) => (
                    spec.owner.clone(),
                    spec.label(),
                    TabSource::GitHubPr(spec.clone()),
                    TabState::unloaded(None, Some(String::new())),
                ),
            };
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            tabs.push(Tab {
                id,
                owner,
                name,
                source,
                state,
            });
        }

        let active = if targets.is_empty() { 0 } else { active_index };
        // Only an explicit `--path` that failed to resolve surfaces an error,
        // and only when it left us with nothing open — the welcome screen is
        // where it's shown.
        let mut no_tab = TabState::empty();
        if tabs.is_empty()
            && let Some(error) = &first_error
        {
            no_tab.session.status = LoadStatus::Failed(error.clone());
        }

        // Recent-repos MRU: prior history from disk, with the repos opening this
        // session promoted to the front (newest first) so they're remembered
        // even after they're later closed. PR tabs aren't paths, so they stay out.
        let mut recent_repos = saved.recent_repos.clone();
        for target in targets.iter().rev() {
            let BootTarget::Repo(repository) = target else {
                continue;
            };
            let key = repository.root.to_string_lossy().into_owned();
            recent_repos.retain(|root| root != &key);
            recent_repos.insert(0, key);
        }
        recent_repos.truncate(RECENT_REPOS_MAX);

        // Every tab starts as a blank `unloaded` shell; the load below fills the
        // active one in (and streams the rest). Inactive tabs load on first
        // activation.
        let mut app = Self {
            app_focused: true,
            selected_theme: config.theme,
            system_theme: iced_theme::Mode::None,
            sidebar_width,
            diff_wrap: saved.diff_wrap.unwrap_or(true),
            diff_split: saved.diff_split.unwrap_or(false),
            sidebar_min_width,
            window_size,
            window_position,
            geometry_dirty_since: None,
            zoom_anim: None,
            zoom_restore: None,
            config,
            keymap,
            modes: Vec::new(),
            recents: Recents::load(),
            sidebar_file_reveal_token: 0,
            scroll_restore_token: 0,
            document_version: 0,
            sidebar_file_cache: Default::default(),
            tabs,
            active,
            no_tab,
            next_tab_id,
            next_document_id: 0,
            recent_repos,
            next_activity_id: 0,
            hovered: None,
            modifiers: keyboard::Modifiers::default(),
            toasts: Vec::new(),
            next_toast_id: 0,
        };

        let theme_task =
            system::theme().map(|mode| Message::Window(WindowEvent::SystemThemeChanged(mode)));
        // Kicks the streaming load for whatever the active tab is — a repo
        // walk, a PR stream, or nothing when no tab opened.
        let load_task = app.ensure_active_loaded();
        (app, Task::batch([load_task, theme_task]))
    }

    /// Fold one repository event into the tab that asked for it.
    ///
    /// Routing is by job: a job belongs to exactly one tab's slot, so a result
    /// that outlives a tab switch still lands on its own tab, and one that
    /// outlives its tab entirely is dropped. The jobless events (the actor's
    /// handshake, an operation head moving, a working-tree edit) go to every
    /// tab on that repository.
    fn on_repo_event(&mut self, event: diffui_core::Event) -> Task<Message> {
        use diffui_core::Payload;

        if let Payload::Ready {
            handle,
            capabilities,
        } = event.payload
        {
            let mut tasks = Vec::new();
            for index in 0..self.tabs.len() {
                if self.tabs[index].repo_id().as_ref() != Some(&event.repo) {
                    continue;
                }
                let id = self.tabs[index].id;
                let state = &mut self.tabs[index].state;
                // The scope is per tab (a repo opened at a subdirectory), so
                // narrow the shared handle rather than using it verbatim.
                let scope = state
                    .repository
                    .as_ref()
                    .map(|repository| repository.scope.clone())
                    .unwrap_or_default();
                state.handle = Some(handle.with_scope(scope));
                state.session.repo = Some(event.repo.clone());
                state.session.capabilities = capabilities;
                // Only the tab on screen loads now; the rest load when they are
                // first activated, as they always have. Their actors are up
                // either way, so the switch is instant.
                if self.is_active(id) {
                    tasks.push(self.start_tab_load(id));
                }
            }
            return Task::batch(tasks);
        }

        let Some(tab) = self.route_event(&event) else {
            return Task::none();
        };
        // A job can be both the frontend's and the projection's — a mutation
        // owns an activity row *and* a slot — so the two run in order rather
        // than one short-circuiting the other.
        let own = self.take_own_job(tab, &event).unwrap_or_else(Task::none);
        // Read the draft's destination off the store this event is about to
        // replace, so it can be re-found by commit id afterwards.
        let candidate = self.draft_candidate_commit(tab);
        let Some(target) = self.tab_mut(tab) else {
            return own;
        };
        let effects = target.session.apply(event);
        let replaced = effects
            .iter()
            .any(|effect| matches!(effect, diffui_core::Effect::GraphReplaced));
        let folded = self.run_effects(tab, effects);
        if replaced {
            self.rearm_draft_candidate(tab, candidate);
        }
        Task::batch([own, folded, self.drain_queued_mutation(tab)])
    }

    /// The commit id under `tab`'s draft destination candidate, if it has one.
    /// Read before a graph swap so the candidate can survive it.
    fn draft_candidate_commit(&self, tab: TabId) -> Option<String> {
        let state = self.tabs.iter().find(|candidate| candidate.id == tab)?;
        let index = state.state.op_draft()?.draft.candidate?;
        Some(
            state
                .state
                .session
                .commits
                .row(index)
                .commit_id()
                .to_owned(),
        )
    }

    /// Re-find the draft's destination in the replaced graph. A candidate whose
    /// commit is no longer in the loaded set stays dropped: the draft is still
    /// live, but its destination has to be picked again rather than silently
    /// becoming whatever row now sits at that index.
    fn rearm_draft_candidate(&mut self, tab: TabId, commit_id: Option<String>) {
        let Some(commit_id) = commit_id else { return };
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(index) = state
            .session
            .commits
            .iter()
            .position(|row| row.commit_id() == commit_id)
        else {
            return;
        };
        if let Some(draft) = state.op_draft_mut() {
            draft.draft.candidate = Some(index);
        }
    }

    /// Which tab an event belongs to. A job names one tab; a jobless event
    /// belongs to whichever tab views that repository (the first, since the
    /// open path keeps one tab per root).
    fn route_event(&self, event: &diffui_core::Event) -> Option<TabId> {
        match event.job() {
            Some(job) => self
                .tabs
                .iter()
                .find(|tab| tab.state.session.owns_job(job) || tab.state.jobs.owns(job))
                .map(|tab| tab.id),
            None => self
                .tabs
                .iter()
                .find(|tab| tab.repo_id().as_ref() == Some(&event.repo))
                .map(|tab| tab.id),
        }
    }

    /// Handle the results the projection has no state to fold: previews, the
    /// source browser, fetches, mutations and file pairs. Returns `None` when
    /// the event isn't one of those, so the caller hands it to `Session::apply`.
    fn take_own_job(&mut self, tab: TabId, event: &diffui_core::Event) -> Option<Task<Message>> {
        use diffui_core::Payload;

        let job = event.job()?;
        let target = self.tab_mut(tab)?;
        let owns = target.jobs.owns(job)
            || target
                .pending_detail_copy
                .as_ref()
                .is_some_and(|(id, ..)| *id == job);
        if !owns {
            return None;
        }
        Some(match &event.payload {
            Payload::PreviewDone { simulation, .. } => {
                self.tab_mut(tab)?.jobs.preview = None;
                self.apply_draft_preview(tab, Ok(simulation.clone()))
            }
            Payload::TreeListed {
                revision, entries, ..
            } => self.apply_source_tree(tab, revision.clone(), Ok(entries.clone())),
            Payload::FileRead { path, file, .. } => {
                self.apply_source_file(tab, path.clone(), Ok(file.clone()))
            }
            Payload::FetchDone { output, .. } => self.finish_fetch(tab, Ok(output.clone())),
            Payload::MutationDone { outcome, .. } => self.finish_mutation(tab, Ok(outcome.clone())),
            Payload::BookmarkChecked { backwards, .. } => {
                self.apply_bookmark_check(tab, Ok(*backwards))
            }
            Payload::FilePairRead { old, new, .. } => {
                self.apply_file_pair(tab, job, old.clone(), new.clone())
            }
            Payload::DetailsLoaded { details, .. } => {
                let field = self
                    .tab_mut(tab)
                    .and_then(|state| state.pending_detail_copy.take());
                match field {
                    Some((_, field, fallback)) => {
                        iced::clipboard::write(format_detail(details, field).unwrap_or(fallback))
                            .discard()
                    }
                    None => Task::none(),
                }
            }
            Payload::Failed { error, .. } => self.fail_own_job(tab, job, error.clone()),
            Payload::Cancelled { .. } => {
                // Resolve whatever row was showing the job before releasing
                // its slot: a cancelled fetch or mutation left "Running" spins
                // in the log forever, and calling it "Done" would claim work
                // that never happened.
                let activity = self.tab_mut(tab).and_then(|state| {
                    let jobs = &state.jobs;
                    jobs.fetch
                        .as_ref()
                        .filter(|(id, ..)| *id == job)
                        .map(|(_, activity, _)| *activity)
                        .or_else(|| {
                            jobs.mutation
                                .as_ref()
                                .filter(|(id, _)| *id == job)
                                .map(|(_, pending)| pending.activity_id)
                        })
                        .or_else(|| {
                            jobs.bookmark_check
                                .as_ref()
                                .filter(|(id, _)| *id == job)
                                .map(|(_, pending)| pending.activity_id)
                        })
                });
                if let Some(activity_id) = activity
                    && let Some(log) = self.activity_log_for(tab)
                {
                    log.finish(
                        activity_id,
                        activity::ActivityStatus::Cancelled,
                        Some("Canceled".to_owned()),
                    );
                }
                self.tab_mut(tab)?.jobs.clear(job);
                Task::none()
            }
            // A job this tab owns can't produce anything else.
            _ => Task::none(),
        })
    }

    /// A frontend-routed job failed. Each kind has its own recovery — an
    /// immutable rejection re-raises the confirmation, a preview just vanishes
    /// — so they can't share the projection's generic failure path.
    fn fail_own_job(
        &mut self,
        tab: TabId,
        job: diffui_core::JobId,
        error: diffui_core::RepoError,
    ) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        let jobs = &mut target.jobs;
        if jobs.preview == Some(job) {
            jobs.preview = None;
            return self.apply_draft_preview(tab, Err(error.to_string()));
        }
        if jobs.tree.as_ref().is_some_and(|(id, _)| *id == job) {
            let revision = jobs.tree.take().map(|(_, revision)| revision);
            return match revision {
                Some(revision) => self.apply_source_tree(tab, revision, Err(error.to_string())),
                None => Task::none(),
            };
        }
        if jobs.file.as_ref().is_some_and(|(id, _)| *id == job) {
            let path = jobs.file.take().map(|(_, path)| path);
            return match path {
                Some(path) => self.apply_source_file(tab, path, Err(error.to_string())),
                None => Task::none(),
            };
        }
        if jobs.fetch.as_ref().is_some_and(|(id, ..)| *id == job) {
            return self.finish_fetch(tab, Err(error.to_string()));
        }
        if jobs
            .bookmark_check
            .as_ref()
            .is_some_and(|(id, _)| *id == job)
        {
            // The ancestry check failed, so we can't tell a backwards move from
            // a fast-forward. Run it: this is where the guard did not exist.
            return self.apply_bookmark_check(tab, Err(error.to_string()));
        }
        if jobs.mutation.as_ref().is_some_and(|(id, _)| *id == job) {
            return self.finish_mutation(tab, Err(error));
        }
        // A file-pair read that failed leaves its file unhighlighted, but the
        // queue must still advance — otherwise two failures wedge it at the
        // concurrency limit and nothing else is ever highlighted.
        if let Some((document_id, _)) = jobs.file_pairs.remove(&job) {
            target.session.highlight_in_flight =
                target.session.highlight_in_flight.saturating_sub(1);
            return self.spawn_highlights(document_id);
        }
        Task::none()
    }

    /// Carry out what the projection asked for. Commands go straight down the
    /// actor's channel — the answer comes back through the subscription — so
    /// most effects produce no `Task` at all.
    fn run_effects(&mut self, tab: TabId, effects: Vec<diffui_core::Effect>) -> Task<Message> {
        use diffui_core::{Effect, session::Activity};

        let is_active = self.is_active(tab);
        let mut tasks = Vec::new();
        for effect in effects {
            match effect {
                Effect::Send(command) => {
                    if let Some(handle) = self.tab_mut(tab).and_then(|t| t.handle.clone()) {
                        handle.send(command);
                    }
                }
                Effect::RepaintDocument => {
                    let document_id = self.allocate_document_id();
                    if let Some(target) = self.tab_mut(tab) {
                        target.session.reset_highlights(document_id);
                        target.selected_file = target
                            .selected_file
                            .min(target.session.document.files.len().saturating_sub(1));
                    }
                    if is_active {
                        self.document_version = self.document_version.wrapping_add(1);
                    }
                    tasks.push(self.spawn_highlights(document_id));
                }
                Effect::GraphReplaced => {
                    // Every row index is stale. Drop the draft's destination
                    // candidate; the event path re-finds it by commit id when
                    // the commit is still in the new graph.
                    if let Some(draft) = self.tab_mut(tab).and_then(|t| t.op_draft_mut()) {
                        draft.draft.candidate = None;
                        draft.hover_spot = None;
                        draft.preview = DraftPreviewState::Idle;
                        draft.preview_request = None;
                    }
                }
                Effect::RevealSelection => {
                    if let Some(target) = self.tab_mut(tab) {
                        target.revision_reveal_token = target.revision_reveal_token.wrapping_add(1);
                    }
                }
                Effect::Activity(Activity::LoadFinished { ok, detail }) => {
                    let status = if ok {
                        activity::ActivityStatus::Done
                    } else {
                        activity::ActivityStatus::Error
                    };
                    if let Some(target) = self.tab_mut(tab) {
                        target.finish_load_activity(status, detail);
                    }
                }
                Effect::Activity(Activity::Toast { title, body, .. }) => {
                    self.push_error_toast(title, &body);
                }
                Effect::Activity(Activity::Note(note)) => {
                    if let Some(log) = self.activity_log_for(tab) {
                        log.note(note);
                    }
                }
            }
        }
        Task::batch(tasks)
    }

    /// The reducer. Five kinds of message, five shapes of handling: a user
    /// intent, a repository actor reporting, widget plumbing, the platform, and
    /// each overlay's own chatter.
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        let task = match message {
            Message::Action(action) => self.perform(action),
            Message::Repo(event) => self.on_repo_event(*event),
            Message::Ui(event) => self.on_ui_event(event),
            Message::Window(event) => self.on_window_event(event),
            Message::Palette(message) => self.on_palette_message(message),
            Message::Find(message) => self.on_find_message(message),
            Message::Menu(message) => self.on_menu_message(message),
        };
        // A mode that isn't on screen must not keep the keyboard.
        self.prune_stale_modes();
        // Chokepoint: if the active tab coalesced a refresh while busy and it's
        // now idle, run it. A no-op when nothing's pending (the common case).
        let refresh = match self.active_tab_id() {
            Some(tab) => {
                let effects = match self.tab_mut(tab) {
                    Some(target) => target.session.take_pending_refresh(),
                    None => Vec::new(),
                };
                self.run_effects(tab, effects)
            }
            None => Task::none(),
        };
        Task::batch([task, refresh])
    }

    /// Carry out one user intent, whichever producer resolved it.
    ///
    /// This is the only place an intent is executed. Nothing in here re-enters
    /// `update` or re-emits a `Message` at itself: a handler that needs another
    /// intent's work calls the helper both of them share, so a fix lands once.
    pub(crate) fn perform(&mut self, action: Action) -> Task<Message> {
        match action {
            // ── Revisions ───────────────────────────────────────────────
            Action::SelectRevision(selection) => self.select_revision(selection),
            Action::JumpToResult(item) => {
                if let Some(change_id) = change_id_for_recents(&item, self) {
                    self.recents.push_revision(change_id);
                    self.recents.save();
                }
                self.jump_to_revision_ref(&item)
            }
            Action::ClearMultiSelection => {
                self.active_mut().revision_multi_selection.clear();
                Task::none()
            }
            Action::Mutate(op) => self.start_mutation_op(op),
            Action::StartDraft { kind, source } => self.start_draft(kind, source),
            Action::EditDescription { target } => {
                let target =
                    target.unwrap_or_else(|| self.active().session.selected_revision.clone());
                self.edit_description(target)
            }
            Action::SaveDescription => self.save_description(),
            Action::CancelDescription => {
                if self
                    .active()
                    .description_editor()
                    .is_some_and(|editor| editor.saving_activity.is_none())
                {
                    self.active_mut().pop_mode(ModeKind::Description);
                    self.active_mut().pending_description_edit = None;
                }
                Task::none()
            }
            Action::BrowseSource { revision, path } => self.open_source_browser(revision, path),

            // ── Files ───────────────────────────────────────────────────
            Action::SelectFile(index) => {
                if index < self.active().session.document.files.len() {
                    self.active_mut().selected_file = index;
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
                Task::none()
            }
            Action::SelectNextFile => {
                if self.active().main_view == MainView::Source {
                    return self.source_select_neighbor(1);
                }
                let target = self.active_mut();
                if !target.session.document.files.is_empty() {
                    target.selected_file = (target.selected_file + 1)
                        .min(target.session.document.files.len().saturating_sub(1));
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
                Task::none()
            }
            Action::SelectPreviousFile => {
                if self.active().main_view == MainView::Source {
                    return self.source_select_neighbor(-1);
                }
                let target = self.active_mut();
                let previous = target.selected_file.saturating_sub(1);
                if previous != target.selected_file {
                    target.selected_file = previous;
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
                Task::none()
            }
            Action::JumpToFile(path) => {
                self.jump_to_file_path(&path);
                Task::none()
            }

            // ── Target mode ─────────────────────────────────────────────
            Action::DraftCandidate(delta) => self.move_draft_candidate(delta),
            Action::DraftPlacement(placement) => self.set_draft_placement(placement),
            Action::DraftApplyPlacement(placement) => {
                // With a keyboard candidate armed, `o`/`a`/`b` applies right
                // away (jjui's flow); before that they just flip the toggle.
                let candidate = self.active().op_draft().and_then(|ui| ui.draft.candidate);
                match candidate.and_then(|index| self.selection_at_index(index)) {
                    Some(target) => self.confirm_draft_on(target, Some(placement)),
                    None => self.set_draft_placement(placement),
                }
            }
            Action::DraftConfirm => {
                if let Some(op) = self
                    .active()
                    .op_draft()
                    .and_then(|ui| ui.draft.op_from_sources())
                {
                    self.active_mut().pop_mode(ModeKind::Draft);
                    return self.start_mutation_op(op);
                }
                let Some(target) = self
                    .active()
                    .op_draft()
                    .and_then(|ui| ui.draft.candidate)
                    .and_then(|index| self.selection_at_index(index))
                else {
                    return Task::none();
                };
                self.confirm_draft_on(target, None)
            }
            Action::DraftCancel => {
                self.active_mut().pop_mode(ModeKind::Draft);
                Task::none()
            }
            Action::DraftToggleSource => {
                let Some(target) = self
                    .active()
                    .op_draft()
                    .and_then(|ui| ui.draft.candidate)
                    .and_then(|index| self.selection_at_index(index))
                else {
                    return Task::none();
                };
                self.draft_toggle_source(target)
            }

            // ── Overlays ────────────────────────────────────────────────
            Action::OpenPalette => self.open_palette(),
            Action::ClosePalette => {
                self.pop_mode(ModeKind::Palette);
                Task::none()
            }
            Action::TogglePalette => {
                if self.palette().is_some() {
                    self.pop_mode(ModeKind::Palette);
                    Task::none()
                } else {
                    self.open_palette()
                }
            }
            Action::PalettePop => {
                let popped = self.palette_mut().map(|state| state.pop());
                match popped {
                    Some(true) => {
                        widget::operation::focus(iced::widget::Id::new(palette::PALETTE_INPUT_ID))
                    }
                    Some(false) => {
                        self.pop_mode(ModeKind::Palette);
                        Task::none()
                    }
                    None => Task::none(),
                }
            }
            Action::PaletteMove(delta) => self.palette_move(delta),
            Action::PaletteActions => {
                let Some(mut state) = self.take_palette() else {
                    return Task::none();
                };
                let pushed = state.push_actions(self);
                self.push_mode(Mode::Palette(state));
                if pushed {
                    return widget::operation::focus(palette::PALETTE_INPUT_ID);
                }
                Task::none()
            }
            Action::OpenFind => self.open_find(),
            Action::CloseFind => {
                self.active_mut().pop_mode(ModeKind::Find);
                Task::none()
            }
            Action::FindNext => {
                self.find_advance(1);
                Task::none()
            }
            Action::FindPrevious => {
                self.find_advance(-1);
                Task::none()
            }
            Action::OpenRepoDialog => {
                // Mutually exclusive with the other overlays: they share one
                // keyboard, and stacking them makes each look broken.
                self.pop_mode(ModeKind::Palette);
                self.active_mut().pop_mode(ModeKind::Find);
                self.push_mode(Mode::OpenRepo(OpenRepoDialog::default()));
                widget::operation::focus(tab_bar::OPEN_REPO_INPUT_ID)
            }
            Action::CloseRepoDialog => {
                self.pop_mode(ModeKind::OpenRepo);
                Task::none()
            }
            Action::SubmitRepoDialog => {
                let path = self
                    .open_repo_dialog()
                    .map(|dialog| dialog.path.clone())
                    .unwrap_or_default();
                self.open_repository(&path)
            }
            Action::OpenRepo(path) => self.open_repository(&path),
            Action::ToggleActivityPopover => {
                if self.activity_popover_open() {
                    self.pop_mode(ModeKind::ActivityPopover);
                } else {
                    self.pop_mode(ModeKind::Menu);
                    self.push_mode(Mode::ActivityPopover);
                }
                Task::none()
            }
            Action::CloseActivityPopover => {
                self.pop_mode(ModeKind::ActivityPopover);
                Task::none()
            }
            Action::ClearActivities => {
                self.active_mut().activities.clear_finished();
                Task::none()
            }
            Action::DismissMenu => {
                self.pop_mode(ModeKind::Menu);
                Task::none()
            }
            Action::ConfirmAccept => {
                let Some(Mode::Confirm(dialog)) = self.pop_mode(ModeKind::Confirm) else {
                    return Task::none();
                };
                self.run_mutation(dialog.pending)
            }
            Action::ConfirmCancel => {
                let Some(Mode::Confirm(dialog)) = self.pop_mode(ModeKind::Confirm) else {
                    return Task::none();
                };
                // Resolve the held activity so it doesn't sit queued forever,
                // and as *cancelled* — a row that reads "Done" for something
                // that never ran is a lie the log can't be trusted after.
                if let Some(log) = self.activity_log_for(dialog.pending.tab_id) {
                    log.finish(
                        dialog.pending.activity_id,
                        activity::ActivityStatus::Cancelled,
                        Some("Canceled".to_owned()),
                    );
                }
                // A canceled describe-on-immutable leaves its editor open
                // and editable again (it was parked as "Saving…").
                if let Some(editor) = self.active_mut().description_editor_mut()
                    && editor.saving_activity == Some(dialog.pending.activity_id)
                {
                    editor.saving_activity = None;
                }
                Task::none()
            }

            // ── Repository ──────────────────────────────────────────────
            Action::Refresh => {
                let Some(tab) = self.active_tab_id() else {
                    return Task::none();
                };
                if !self.active().session.capabilities.graph {
                    return Task::none();
                }
                // A manual refresh is a full reload: the user may have run an
                // external jj op since the last load. An up-to-date refresh
                // that finds nothing changed is silent.
                self.start_repository_snapshot(tab, RefreshOrigin::Focus)
            }
            Action::Fetch(target) => self.start_fetch(target),
            Action::Undo => self.start_undo(),
            Action::UndoOperation(activity_id, operation_id) => {
                let Some(tab_id) = self.active_tab_id() else {
                    return Task::none();
                };
                // One-shot: clear the button so a second click can't
                // double-revert the same operation.
                if let Some(log) = self.activity_log_for(tab_id) {
                    log.clear_undo_op(activity_id);
                }
                self.start_mutation_op(mutations::MutationOp::Undo {
                    operation_id: Some(operation_id),
                })
            }
            Action::SetRevset(expr) => {
                self.active_mut().session.revset = expr;
                self.evaluate_revset()
            }
            Action::SubmitRevset => self.evaluate_revset(),

            // ── View ────────────────────────────────────────────────────
            Action::ToggleWrap => {
                self.diff_wrap = !self.diff_wrap;
                // Wrap changes shaping, so drop the paragraph cache; the
                // height index re-keys off the flag itself. Persist with the
                // usual geometry debounce.
                self.document_version = self.document_version.wrapping_add(1);
                self.mark_geometry_dirty();
                Task::none()
            }
            Action::ToggleSplit => {
                self.diff_split = !self.diff_split;
                // Same cache discipline as the wrap toggle: column widths
                // change shaping, the height index re-keys off the flag.
                self.document_version = self.document_version.wrapping_add(1);
                self.mark_geometry_dirty();
                Task::none()
            }
            Action::SetTheme(theme) => {
                self.selected_theme = theme;
                // Switching to System: sync to the live OS appearance now rather
                // than render one stale frame until the first poll tick lands.
                if theme == ThemePreference::System {
                    let current = chrome::system_appearance();
                    if current != iced_theme::Mode::None {
                        self.system_theme = current;
                    }
                }
                Task::none()
            }
            Action::SetMainView(mode) => self.set_main_view(mode),

            // ── Tabs ────────────────────────────────────────────────────
            Action::SelectTab(id) => self.activate_tab(id),
            Action::SelectTabIndex(index) => match self.tabs.get(index) {
                Some(tab) => {
                    let id = tab.id;
                    self.activate_tab(id)
                }
                None => Task::none(),
            },
            Action::CloseTab(id) => self.close_tab(id),
            Action::CloseActiveTab => match self.tabs.get(self.active) {
                Some(tab) => {
                    let id = tab.id;
                    self.close_tab(id)
                }
                None => Task::none(),
            },

            // ── Clipboard ───────────────────────────────────────────────
            Action::Copy(text) => iced::clipboard::write(text).discard(),
            Action::CopyDetail {
                revision,
                field,
                fallback,
            } => {
                // Author / committer / full description aren't kept in the
                // graph, so ask the actor for the revision's header and copy
                // from that, falling back to the in-memory value.
                let Some(tab) = self.active_tab_id() else {
                    return Task::none();
                };
                let Some(state) = self.tab_mut(tab) else {
                    return Task::none();
                };
                if !state.session.capabilities.details {
                    return iced::clipboard::write(fallback).discard();
                }
                let job = state.session.next_job();
                state.pending_detail_copy = Some((job, field, fallback));
                self.send(tab, diffui_core::Command::RevisionDetails { job, revision })
            }
            Action::CopyFileDiff => match current_file_diff_text(self) {
                Some(text) => iced::clipboard::write(text).discard(),
                None => Task::none(),
            },
            Action::OpenUrl(url) => {
                open_url(&url);
                Task::none()
            }
        }
    }

    /// Widget plumbing: positions, hovers, ticks, and the clicks whose meaning
    /// depends on state the widget can't see.
    fn on_ui_event(&mut self, event: UiEvent) -> Task<Message> {
        match event {
            UiEvent::SelectRowKey(key) => return self.on_revision_row_click(key),
            UiEvent::SidebarFileRow(display_index) => {
                let target = self.active_mut();
                let rows = diffui_core::file_tree_rows(
                    &target.session.document.files,
                    &target.collapsed_dirs,
                );
                match rows.get(display_index) {
                    Some(diffui_core::FileTreeRow::File { file_index, .. }) => {
                        if *file_index < target.session.document.files.len() {
                            target.selected_file = *file_index;
                        }
                    }
                    Some(diffui_core::FileTreeRow::Dir { path, .. }) => {
                        if !target.collapsed_dirs.remove(path) {
                            target.collapsed_dirs.insert(path.clone());
                        }
                    }
                    None => {}
                }
            }
            UiEvent::SidebarScrolled(offset) => {
                self.active_mut().sidebar_scroll_offset = offset;
            }
            UiEvent::DiffScrolled(offset) => {
                self.active_mut().diff_scroll_offset = offset;
            }
            UiEvent::SourceScrolled(offset) => {
                self.active_mut().source.scroll_offset = offset;
            }
            UiEvent::SourceTreeScrolled(offset) => {
                self.active_mut().source.tree_scroll_offset = offset;
            }
            UiEvent::SetHover(target) => self.hovered = target,
            UiEvent::ToastDismiss(id) => self.toasts.retain(|toast| toast.id != id),
            UiEvent::ToastTick => self.toasts.retain(|toast| toast.born.elapsed() < TOAST_TTL),
            UiEvent::LoadingTick => {}
            UiEvent::RevisionContextMenu(key, row_rect, cursor) => {
                return self.open_revision_menu(key, row_rect, cursor);
            }
            UiEvent::SidebarFileContextMenu(display_index, row_rect, cursor) => {
                return self.open_file_context_menu(display_index, row_rect, cursor);
            }
            UiEvent::OpenToolbarMenu(which, anchor) => {
                self.pop_mode(ModeKind::ActivityPopover);
                return self.open_toolbar_menu(which, anchor);
            }
            UiEvent::DescriptionAction(action) => {
                if let Some(editor) = self.active_mut().description_editor_mut()
                    && editor.saving_activity.is_none()
                {
                    editor.content.perform(action);
                    editor.switch_blocked = false;
                }
            }
            UiEvent::ActivityExpand(id) => self.active_mut().activities.toggle_expand(id),
            UiEvent::ActivityDetailAction(id, action) => {
                // Selection/caret/scroll only — the log drops edit actions,
                // keeping the output buffer read-only.
                self.active_mut()
                    .activities
                    .perform_detail_action(id, action);
            }
            UiEvent::ActivityNoOp | UiEvent::ConfirmNoOp | UiEvent::OpenRepoNoOp => {}
            UiEvent::OpenRepoPathChanged(path) => {
                if let Some(dialog) = self.open_repo_dialog_mut() {
                    dialog.path = path;
                    // Clear a stale error as soon as the user edits the path.
                    dialog.error = None;
                }
            }
            UiEvent::RevsetChanged(value) => self.active_mut().session.revset = value,
            UiEvent::SidebarWidthChanged(width) => {
                let clamped = resize_handle::clamp_width(
                    width,
                    self.sidebar_min_width,
                    self.window_size.width,
                );
                if clamped != self.sidebar_width {
                    self.sidebar_width = clamped;
                    self.mark_geometry_dirty();
                }
            }
            UiEvent::SourceSidebarRow(display_index) => {
                let (entries, rows) = self.source_entries_and_rows();
                match rows.get(display_index).cloned() {
                    Some(diffui_core::SourceTreeRow::Dir { path, unlisted, .. }) => {
                        if unlisted {
                            // Unenumerated ignored dir: expanding it is a
                            // lazy disk listing. Mark it expanded now so the
                            // arriving children render straight away.
                            let Some(tab) = self.active_tab_id() else {
                                return Task::none();
                            };
                            self.active_mut().source.expanded.insert(path.clone());
                            return self.list_ignored_dir(tab, path);
                        }
                        let expanded = &mut self.active_mut().source.expanded;
                        if !expanded.remove(&path) {
                            expanded.insert(path);
                        }
                    }
                    Some(diffui_core::SourceTreeRow::File { entry_index, .. }) => {
                        let path = entries.get(entry_index).map(|entry| entry.path.clone());
                        if let Some(path) = path {
                            return self.select_source_file(path);
                        }
                    }
                    None => {}
                }
            }
            UiEvent::SourceHeaderClicked => {}
            UiEvent::SourceFilterChanged(query) => {
                self.active_mut().source.filter = query;
                // New result set — jump the list back to the top. The code
                // pane's restore re-applies its own live offset, so only the
                // sidebar actually moves.
                self.active_mut().source.tree_scroll_offset = 0.0;
                self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
            }
            UiEvent::SourceFilterSubmit => {
                // Open the best match (the ranked list's first row). A no-op
                // when the box is empty — the first tree row is a directory.
                let (entries, rows) = self.source_entries_and_rows();
                if let Some(diffui_core::SourceTreeRow::File { entry_index, .. }) = rows.first()
                    && let Some(path) = entries.get(*entry_index).map(|entry| entry.path.clone())
                {
                    return self.select_source_file(path);
                }
            }
            UiEvent::BrowseFileFromDiff(file_index) => {
                let target = self.active();
                let Some(file) = target.session.document.files.get(file_index) else {
                    return Task::none();
                };
                let revision = target.session.selected_revision.clone();
                let path = file.path.clone();
                return self.open_source_browser(revision, Some(path));
            }
            UiEvent::RevisionDragStart(index) => return self.on_drag_start(index),
            UiEvent::RevisionDragHover(spot) => {
                let Some(draft) = self.active_mut().op_draft_mut() else {
                    return Task::none();
                };
                if draft.hover_spot == spot {
                    return Task::none();
                }
                draft.hover_spot = spot;
                draft.draft.candidate = match spot {
                    Some(revision_list::DropSpot::OnRow(index)) => Some(index),
                    _ => None,
                };
                return self.kick_draft_preview();
            }
            UiEvent::RevisionDragDrop(spot) => return self.on_drag_drop(spot),
            UiEvent::DraftHoverCandidate(index) => {
                // Mouse-hover targeting: arm the hovered row as the candidate,
                // exactly like j/k. Leaving the rows (`None`) keeps the last
                // candidate armed — same persistence the keyboard gets.
                let Some(index) = index else {
                    return Task::none();
                };
                let target = self.active_mut();
                let commits = target.session.commits.len();
                let Some(draft) = target.op_draft_mut() else {
                    return Task::none();
                };
                if draft.draft.candidate == Some(index) || index >= commits {
                    return Task::none();
                }
                draft.draft.candidate = Some(index);
                return self.kick_draft_preview();
            }
            UiEvent::DraftPreviewKick(version) => return self.run_parked_preview(version),
            UiEvent::FileHighlighted(document_id, file_index, spans) => {
                let Some((tab, target)) = self.document_target_mut(document_id) else {
                    // The document was replaced; this result highlighted a
                    // dead snapshot.
                    return Task::none();
                };
                let session = &mut target.session;
                session.highlight_in_flight = session.highlight_in_flight.saturating_sub(1);
                let mut applied = false;
                if let Some(file) = session.document.files.get_mut(file_index) {
                    for (hunk_index, line_index, line_spans) in spans {
                        if let Some(line) = file
                            .hunks
                            .get_mut(hunk_index)
                            .and_then(|hunk| hunk.lines.get_mut(line_index))
                        {
                            line.syntax = line_spans;
                            applied = true;
                        }
                    }
                }
                // Repaint (re-shape) the rows now carrying spans; the layout
                // is untouched, so the height index survives — that's the
                // whole point of the paint/layout version split. A backgrounded
                // tab repaints when it's next activated.
                if applied && self.is_active(tab) {
                    self.document_version = self.document_version.wrapping_add(1);
                }
                return self.spawn_highlights(document_id);
            }
        }
        Task::none()
    }

    /// The platform half: window geometry and lifecycle, modifiers, the OS
    /// appearance, and the raw key presses the keymap resolves.
    fn on_window_event(&mut self, event: WindowEvent) -> Task<Message> {
        match event {
            WindowEvent::KeyPressed {
                key,
                modifiers,
                consumed,
            } => return self.on_key_pressed(&key, modifiers, consumed),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers,
            WindowEvent::SystemThemeChanged(theme) => self.system_theme = theme,
            WindowEvent::PollSystemTheme => {
                // iced pins the window's NSAppearance to our resolved theme,
                // which makes winit stop reporting OS appearance changes (it
                // ignores them once a window has an explicit appearance). So
                // while following the OS we read the live application appearance
                // ourselves and re-resolve on a change. `Mode::None` means
                // "undeterminable" — leave the last known value untouched.
                let current = chrome::system_appearance();
                if current != iced_theme::Mode::None && current != self.system_theme {
                    self.system_theme = current;
                }
            }
            WindowEvent::FocusChanged(focused) => {
                let gained_focus = focused && !self.app_focused;
                let lost_focus = !focused && self.app_focused;
                self.app_focused = focused;

                // Flush pending geometry immediately on focus loss. App-switch
                // and quit almost always blur the window first, so this closes
                // the gap between a resize and the debounce timer firing.
                if lost_focus && self.geometry_dirty_since.is_some() {
                    self.geometry_dirty_since = None;
                    self.current_window_state().save();
                }

                if gained_focus && let Some(tab) = self.active_tab_id() {
                    return self.start_repository_snapshot(tab, RefreshOrigin::Focus);
                }
            }
            WindowEvent::Opened(position, size) => {
                // Seed tracking from the real window without marking dirty: the
                // geometry we'd persist already matches what's on disk.
                self.window_size = size;
                if position.is_some() {
                    self.window_position = position;
                }
                // Center the native window controls on the tab strip, and arm
                // the native resize observer that keeps them centered without a
                // frame of lag while the window is dragged (see
                // `chrome::install_window_resize_observer`).
                return Task::batch([
                    self.reposition_window_controls(),
                    self.install_resize_observer(),
                    self.configure_custom_titlebar(),
                ]);
            }
            WindowEvent::Resized(size) => {
                if self.window_size != size {
                    self.window_size = size;
                    // Shrinking the window can push the split past the diff
                    // pane's minimum, so re-clamp rather than persist a width
                    // the new size can't show.
                    self.sidebar_width = resize_handle::clamp_width(
                        self.sidebar_width,
                        self.sidebar_min_width,
                        size.width,
                    );
                    self.mark_geometry_dirty();
                }
                // The native resize observer (armed on open) re-centers the
                // traffic lights in step with AppKit's layout. This message-loop
                // reposition stays as a harmless fallback — it runs a frame
                // later and just re-applies the same position the observer
                // already set, so it can't reintroduce the jump.
                return self.reposition_window_controls();
            }
            WindowEvent::Moved(position) => {
                if self.window_position != Some(position) {
                    self.window_position = Some(position);
                    self.mark_geometry_dirty();
                }
            }
            WindowEvent::PersistState => {
                // Only write once the changes have settled — a drag keeps
                // bumping `geometry_dirty_since`, so the elapsed check holds the
                // write back until the burst stops.
                if let Some(since) = self.geometry_dirty_since
                    && since.elapsed() >= WINDOW_STATE_DEBOUNCE
                {
                    self.geometry_dirty_since = None;
                    self.current_window_state().save();
                }
            }
            WindowEvent::CloseRequested => {
                // The app owns the close (`exit_on_close_request(false)`) so
                // the debounced write can't be cut off by the process going
                // away — ⌘Q and the close button raise no `Unfocused`, so
                // without this the last resize, tab or revset is lost.
                self.geometry_dirty_since = None;
                self.current_window_state().save();
                return iced::exit();
            }
            WindowEvent::TitleBarDrag => {
                // Resolve the (single) window and begin an interactive drag.
                // No-op if the window id isn't available yet.
                return window::latest().then(|id| id.map_or_else(Task::none, window::drag));
            }
            WindowEvent::TitleBarDoubleClick => {
                // A native title bar runs the system double-click action for free;
                // our custom strip has to resolve it. Read the window frame, its
                // screen's visible frame, and the configured action on the main
                // thread, then act on the result in `TitleBarDoubleClickPlan`.
                return window::latest()
                    .then(|maybe_id| {
                        maybe_id.map_or_else(Task::none, |id| {
                            window::run(id, |window| {
                                window
                                    .window_handle()
                                    .ok()
                                    .map(|h| chrome::read_double_click_plan(h.as_raw()))
                                    .unwrap_or(([0.0; 4], [0.0; 4], 2, 0.0))
                            })
                        })
                    })
                    .map(|(current, visible, action, duration)| {
                        Message::Window(WindowEvent::TitleBarDoubleClickPlan {
                            current,
                            visible,
                            action,
                            duration,
                        })
                    });
            }
            WindowEvent::TitleBarDoubleClickPlan {
                current,
                visible,
                action,
                duration,
            } => {
                match action {
                    // Minimize: let iced/winit miniaturize the window.
                    1 => {
                        return window::latest().then(|id| {
                            id.map_or_else(Task::none, |id| window::minimize(id, true))
                        });
                    }
                    // None: the user asked for nothing on double-click.
                    2 => {}
                    // Zoom (the default): toggle between the visible frame and the
                    // saved restore frame, driven by our own animation. Ignore a
                    // re-trigger mid-flight, and bail if the read came back empty.
                    _ => {
                        if self.zoom_anim.is_some() || visible[2] <= 0.0 {
                            return Task::none();
                        }
                        let (to, dur) = if frames_approx_eq(current, visible) {
                            // Un-zoom: restore the saved frame at the same duration
                            // the zoom-in used (the resize is symmetric).
                            self.zoom_restore.take().unwrap_or_else(|| {
                                (zoom_default_restore(visible), ZOOM_FALLBACK_SECS)
                            })
                        } else {
                            // Zoom in: remember where to come back to, and the
                            // native duration to come back at.
                            self.zoom_restore = Some((current, duration));
                            (visible, duration)
                        };
                        self.zoom_anim = Some(ZoomAnim {
                            start: Instant::now(),
                            from: current,
                            to,
                            duration: dur,
                        });
                    }
                }
            }
            WindowEvent::ZoomAnimTick => {
                let Some(anim) = self.zoom_anim else {
                    return Task::none();
                };
                // Normalized progress over the native-matched duration (guarded
                // against a zero duration). Snap to the target on the final frame
                // so we land exactly.
                let t =
                    (anim.start.elapsed().as_secs_f64() / anim.duration.max(0.001)).clamp(0.0, 1.0);
                let done = t >= 1.0;
                let frame = if done {
                    self.zoom_anim = None;
                    anim.to
                } else {
                    // Sinusoidal ease-in-out — slow at both ends, matching the
                    // feel of AppKit's native window-resize curve.
                    let e = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
                    let mut f = [0.0; 4];
                    for (i, slot) in f.iter_mut().enumerate() {
                        *slot = anim.from[i] + (anim.to[i] - anim.from[i]) * e;
                    }
                    f
                };
                return window::latest()
                    .then(move |maybe_id| {
                        maybe_id.map_or_else(Task::none, move |id| {
                            window::run(id, move |window| {
                                if let Ok(handle) = window.window_handle() {
                                    chrome::set_window_frame(handle.as_raw(), frame);
                                }
                            })
                        })
                    })
                    .discard();
            }
        }
        Task::none()
    }

    /// Resolve a key press through the keymap and run whatever it names.
    ///
    /// `consumed` is iced's "a focused widget already took this key". The
    /// revset box is an inline text input with no mode of its own, so without
    /// that check its typing would also drive file navigation.
    fn on_key_pressed(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
        consumed: bool,
    ) -> Task<Message> {
        let Some(chord) = Chord::from_event(key, modifiers) else {
            return Task::none();
        };
        let context = self.key_context();
        let Some(id) = self.resolve_chord(context, &chord, consumed) else {
            return Task::none();
        };
        self.run_command(id, CommandArg::None)
    }

    /// The command a chord names in `context`, falling back to the base context
    /// only where the mode allows it (see [`crate::modes::passes_through`]).
    fn resolve_chord(
        &self,
        context: Context,
        chord: &Chord,
        consumed: bool,
    ) -> Option<commands::CommandId> {
        let claim = |context: Context| {
            if consumed && modes::needs_unconsumed(context, chord) {
                return None;
            }
            self.keymap.resolve(context, chord)
        };
        claim(context).or_else(|| {
            modes::passes_through(context, chord)
                .then(|| claim(Context::Base))
                .flatten()
        })
    }

    fn on_palette_message(&mut self, message: PaletteMessage) -> Task<Message> {
        match message {
            PaletteMessage::QueryChanged(query) => {
                // Take the palette out of `self` so the matcher can borrow
                // `&self` (commits / files / recents) directly. Previously
                // this cloned the entire app per keystroke; on a 40k-commit
                // repo that deep clone was the bulk of the typing latency.
                let Some(mut state) = self.take_palette() else {
                    return Task::none();
                };
                let depth = state.stack.len().saturating_sub(1);
                let mut task = Task::none();
                if let Some(column) = state.top_mut() {
                    column.query = query;
                    column.dirty = true;
                    // Editing resets `:` commit-search back to its "press ⏎"
                    // prompt (the prior results are for a stale query).
                    column.searched = false;
                    column.query_version = column.query_version.wrapping_add(1);
                    let version = column.query_version;
                    // Debounce: the matcher scans every commit, so coalesce
                    // fast typing rather than re-matching on each keystroke.
                    task = Task::perform(
                        async move {
                            tokio::time::sleep(PALETTE_QUERY_DEBOUNCE).await;
                            (depth, version)
                        },
                        |(depth, version)| {
                            Message::Palette(PaletteMessage::Recompute(depth, version))
                        },
                    );
                }
                self.push_mode(Mode::Palette(state));
                task
            }
            PaletteMessage::Recompute(depth, version) => {
                let Some(mut state) = self.take_palette() else {
                    return Task::none();
                };
                let mut task = Task::none();
                if let Some(column) = state.stack.get_mut(depth)
                    && column.query_version == version
                {
                    column.selected = 0;
                    // Re-running the matcher invalidates row positions; jump
                    // the scroll back to the top so the first row is visible.
                    column.scroll_y = 0.0;
                    column.dirty = false;
                    palette::recompute_matches(column, self, false);
                    task = widget::operation::scroll_to(
                        palette::results_scrollable_id(depth),
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
                self.push_mode(Mode::Palette(state));
                task
            }
            PaletteMessage::SelectIndex(depth, index) => {
                if let Some(state) = self.palette_mut()
                    && state.stack.len() == depth + 1
                    && let Some(column) = state.stack.get_mut(depth)
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
                Task::none()
            }
            PaletteMessage::Accept => self.palette_submit(),
            PaletteMessage::AcceptIndex(depth, index) => {
                // The click carries the column it was drawn in: a push or pop
                // between the press and its delivery must not run whatever row
                // now sits at that index in a different column.
                let addressed = self
                    .palette_mut()
                    .is_some_and(|state| state.stack.len() == depth + 1);
                if !addressed {
                    return Task::none();
                }
                if let Some(state) = self.palette_mut()
                    && let Some(column) = state.stack.get_mut(depth)
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
                self.palette_accept_current()
            }
            PaletteMessage::NoOp | PaletteMessage::Tick => Task::none(),
        }
    }

    fn on_find_message(&mut self, message: FindMessage) -> Task<Message> {
        match message {
            FindMessage::QueryChanged(query) => {
                if let Some(state) = self.active_mut().find_mut() {
                    state.query = query;
                    state.error = None;
                    state.query_version = state.query_version.wrapping_add(1);
                    let version = state.query_version;
                    return Task::perform(
                        async move {
                            tokio::time::sleep(find::DEBOUNCE).await;
                            version
                        },
                        |version| Message::Find(FindMessage::Recompute(version)),
                    );
                }
            }
            FindMessage::Recompute(version) => {
                let target = self.active_mut();
                if let Some(state) = target.find()
                    && state.query_version == version
                {
                    let (matches, error) = find::compute_matches(state, target.find_files());
                    if let Some(state) = self.active_mut().find_mut() {
                        state.matches = matches;
                        state.error = error;
                        state.active = if state.matches.is_empty() {
                            None
                        } else {
                            Some(0)
                        };
                        state.scroll_token = state.scroll_token.wrapping_add(1);
                    }
                }
            }
            FindMessage::ToggleCase => {
                if let Some(state) = self.active_mut().find_mut() {
                    state.case_sensitive = !state.case_sensitive;
                    return self.refind_now();
                }
            }
            FindMessage::ToggleRegex => {
                if let Some(state) = self.active_mut().find_mut() {
                    state.regex = !state.regex;
                    return self.refind_now();
                }
            }
        }
        Task::none()
    }

    fn on_menu_message(&mut self, message: MenuMessage) -> Task<Message> {
        match message {
            MenuMessage::Hover(path) => {
                // Hover is `on_enter`-driven (geometry-free, can't mis-hit). On
                // the open branch (or with no flyout open) it commits at once.
                // An off-branch row while a flyout is open is held as *pending*
                // only when the pointer is actually sweeping — a slow crawl onto
                // a row is aim, not transit, and commits directly. A pending row
                // commits once the cursor veers out of the trajectory wedge
                // (MenuMouseMoved) or the sweep stalls (MenuTick).
                let Some(m) = self.menu_mut() else {
                    return Task::none();
                };
                m.entered = true;
                if m.open_path.is_empty() || m.on_open_branch(&path) || m.moving_slowly() {
                    m.activate(path);
                } else {
                    m.pending_row = Some(path);
                }
            }
            MenuMessage::MouseMoved(pos) => {
                if self.menu().is_none() {
                    return Task::none();
                }
                // Off-branch sweep pending: commit the row the moment the cursor
                // leaves the triangle aimed at the flyout (a veer). The apex
                // itself is eased toward the cursor on the menu tick (see
                // `MenuTick`), frozen while a row is pending — so it sits upstream
                // and the wedge has room during a sweep. No distance ceiling: a
                // big menu can take as long as the sweep keeps moving (stalls are
                // the tick's job). A not-yet-set apex (submenu only just opened)
                // holds the row pending rather than stealing it.
                let commit = {
                    let m = self.menu().expect("just checked");
                    m.pending_row.as_ref().map(|_| {
                        match (m.flyout_origin, menu::flyout_rect(self, m)) {
                            (Some(apex), Some(fly)) => !menu::heading_to_flyout(apex, pos, fly),
                            _ => false,
                        }
                    })
                };
                let m = self.menu_mut().expect("just checked");
                m.note_cursor_move(pos);
                m.entered = true;
                if commit == Some(true)
                    && let Some(path) = m.pending_row.take()
                {
                    m.activate(path);
                }
            }
            MenuMessage::Select(path, button) => {
                let Some(open) = self.menu_mut() else {
                    return Task::none();
                };
                // The opening press's own release picks nothing: the menu was
                // drawn under a cursor that never pressed anything inside it, so
                // running the row it happens to cover would fire an action the
                // user only asked to *see*. A later press-release picks.
                // Only a leaf picks; a release on a submenu/disabled/separator
                // row leaves the (already hover-opened) menu as it is.
                if !open.opening_release(button)
                    && let Some(menu::MenuEntry::Item { command, arg, .. }) = open.entry_at(&path)
                {
                    let (command, arg) = (*command, arg.clone());
                    self.pop_mode(ModeKind::Menu);
                    return self.run_command(command, arg);
                }
            }
            MenuMessage::CapturePress => {}
            MenuMessage::CardScrolled(depth, offset) => {
                if let Some(m) = self.menu_mut() {
                    if m.scrolls.len() <= depth {
                        m.scrolls.resize(depth + 1, 0.0);
                    }
                    m.scrolls[depth] = offset;
                }
            }
            MenuMessage::Dismiss => {
                self.pop_mode(ModeKind::Menu);
            }
            MenuMessage::ScrimRelease(button) => {
                if let Some(menu) = self.menu_mut() {
                    // Same gate the row `Select` uses, on the other half of the
                    // window: the opening press's release lands here when the
                    // cursor sits outside the cards — swallow it and keep the
                    // menu open. A later release — or one after the cursor has
                    // dragged into the menu — dismisses.
                    if !menu.opening_release(button) || menu.entered {
                        self.pop_mode(ModeKind::Menu);
                    }
                }
            }
            // Drives three time-based effects while a menu is open: the
            // right-click glow pulse (re-running `view`), easing the trajectory
            // apex toward the cursor, and the sweep-stall commit. Easing on this
            // fixed clock — not on raw moves — is what makes the apex lag by a
            // velocity-proportional amount during a sweep yet catch up when the
            // cursor idles. Frozen while a row is pending so the wedge keeps a
            // stable upstream origin mid-sweep.
            MenuMessage::Tick => {
                if let Some(m) = self.menu_mut() {
                    if m.pending_row.is_none() {
                        if let Some(cursor) = m.cursor {
                            m.flyout_origin = Some(menu::ease_apex(m.flyout_origin, cursor));
                        }
                    } else if m.sweep_stalled()
                        && let Some(path) = m.pending_row.take()
                    {
                        // The wedge only protects an *ongoing* sweep. Idle or
                        // crawling for a beat means the user settled on the row
                        // they're over — the hold ends and that row wins, instead
                        // of the guard pinning the old flyout open forever.
                        m.activate(path);
                    }
                }
            }
        }
        Task::none()
    }

    /// Open the palette. It shares one keyboard with the find bar and the
    /// open-repo dialog, so those close rather than sit behind the modal.
    fn open_palette(&mut self) -> Task<Message> {
        self.active_mut().pop_mode(ModeKind::Find);
        self.pop_mode(ModeKind::OpenRepo);
        let state = PaletteState::open(self);
        self.push_mode(Mode::Palette(state));
        widget::operation::focus(palette::PALETTE_INPUT_ID)
    }

    /// Lift the palette out of the mode stack so the matcher can borrow `&self`
    /// — the commits, files and recents it ranks against. The caller must push
    /// it back.
    fn take_palette(&mut self) -> Option<PaletteState> {
        match self.pop_mode(ModeKind::Palette) {
            Some(Mode::Palette(state)) => Some(state),
            _ => None,
        }
    }

    /// Move the palette's highlighted row, scrolling it back into view when the
    /// move pushed it out.
    fn palette_move(&mut self, delta: i32) -> Task<Message> {
        let Some(state) = self.palette_mut() else {
            return Task::none();
        };
        let depth = state.stack.len().saturating_sub(1);
        let Some(column) = state.top_mut() else {
            return Task::none();
        };
        if column.matches.is_empty() {
            return Task::none();
        }
        let len = column.matches.len() as i32;
        column.selected = (column.selected as i32 + delta).rem_euclid(len) as usize;
        if column.ensure_selected_visible() {
            let y = column.scroll_y;
            return widget::operation::scroll_to(
                palette::results_scrollable_id(depth),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
            );
        }
        Task::none()
    }

    /// Open the find bar, closing the overlays that share its keyboard.
    fn open_find(&mut self) -> Task<Message> {
        self.pop_mode(ModeKind::Palette);
        self.pop_mode(ModeKind::OpenRepo);
        if self.active().find().is_none() {
            self.active_mut()
                .push_mode(TabMode::Find(FindState::default()));
        }
        widget::operation::focus(find::FIND_INPUT_ID)
    }

    /// Browse `selection`: the plain-click / palette-jump path. Re-selecting
    /// the revision already shown toggles its inline file list instead of
    /// re-running the backend.
    fn select_revision(&mut self, selection: RevisionSelection) -> Task<Message> {
        let target = self.active_mut();
        target.revision_multi_selection.clear();
        if target.session.selected_revision != selection {
            if let Some(editor) = target.description_editor_mut()
                && editor.is_dirty()
            {
                editor.switch_blocked = true;
                return Task::none();
            }
            target.pop_mode(ModeKind::Description);
        }
        // Re-clicking the already-selected revision toggles its file
        // list without re-running the backend or changing the diff.
        // The toggled value persists across revision switches, so
        // collapsing once stays collapsed wherever the user moves
        // next.
        if target.session.selected_revision == selection {
            target.file_list_expanded = !target.file_list_expanded;
        } else if target.session.diff_pending() == Some(&selection) {
            // Already loading this revision — let it land.
        } else if let Some(tab) = self.active_tab_id() {
            let effects = self.active_mut().session.load_diff(selection);
            return self.run_effects(tab, effects);
        }
        Task::none()
    }

    /// A click on a revision row. What it means depends on the modifiers held
    /// and on whether a draft is picking a destination, neither of which the
    /// widget can see — so the row publishes the click and this resolves it.
    fn on_revision_row_click(&mut self, key: revision_list::RowSelectionKey) -> Task<Message> {
        let selection = selection_from_key(&key);
        // Target mode: a plain click *arms* the row as the destination
        // candidate (same as hover / j-k); the Apply button, ↵, or a
        // drop executes. One-click execution was too easy to fire by
        // accident once hover started arming rows. ⌘-click toggles the
        // row as a draft *source* and stays in the mode — stack extra
        // merge parents / revisions to rebase / squash sources, or
        // un-stack them.
        if self.active().op_draft().is_some() {
            if self.modifiers.command() {
                return self.draft_toggle_source(selection);
            }
            let index = match &selection {
                RevisionSelection::WorkingCopy => {
                    self.active().session.commits.working_copy_index()
                }
                RevisionSelection::Commit(id) => self
                    .active()
                    .session
                    .commits
                    .iter()
                    .position(|row| row.commit_id() == id),
            };
            let Some(index) = index else {
                return Task::none();
            };
            let Some(draft) = self.active_mut().op_draft_mut() else {
                return Task::none();
            };
            if draft.draft.candidate == Some(index) {
                return Task::none();
            }
            draft.draft.candidate = Some(index);
            draft.hover_spot = None;
            return self.kick_draft_preview();
        }
        // Outside target mode, ⌘-click marks/unmarks the row for a
        // batch action and ⇧-click marks the visible range between
        // the browsed row and the clicked one. Neither navigates —
        // the marked set is a separate axis from the browsed
        // revision, consumed by the context menu's batch items.
        // jj-gated like every mutation entry point.
        if (self.modifiers.command() || self.modifiers.shift())
            && self.active().session.capabilities.mutate
        {
            let commit_id = match &selection {
                RevisionSelection::WorkingCopy => self
                    .active()
                    .session
                    .commits
                    .working_copy()
                    .map(|row| row.commit_id().to_owned()),
                RevisionSelection::Commit(id) => Some(id.clone()),
            };
            let Some(commit_id) = commit_id else {
                return Task::none();
            };
            if self.modifiers.shift() {
                let clicked = self
                    .active()
                    .session
                    .commits
                    .iter()
                    .position(|row| row.commit_id() == commit_id);
                // Anchor on the browsed row, Finder-style; replaces
                // any previous marks.
                if let (Some(clicked), Some(anchor)) =
                    (clicked, self.active().session.selected_commit_index)
                {
                    let (lo, hi) = (clicked.min(anchor), clicked.max(anchor));
                    self.active_mut().revision_multi_selection = (lo..=hi)
                        .map(|index| {
                            self.active()
                                .session
                                .commits
                                .row(index)
                                .commit_id()
                                .to_owned()
                        })
                        .collect();
                    return Task::none();
                }
                // No anchor to span from — fall through to a toggle.
            }
            if let Some(found) = self
                .active()
                .revision_multi_selection
                .iter()
                .position(|id| *id == commit_id)
            {
                self.active_mut().revision_multi_selection.remove(found);
            } else {
                self.active_mut().revision_multi_selection.push(commit_id);
            }
            return Task::none();
        }
        self.select_revision(selection)
    }

    /// Right-click on a revision row: retarget the marks if needed, then pop
    /// the context menu.
    fn open_revision_menu(
        &mut self,
        key: revision_list::RowSelectionKey,
        row_rect: iced::Rectangle,
        cursor: iced::Point,
    ) -> Task<Message> {
        // Right-clicking during target mode reads as "do something
        // else instead" — drop the draft rather than nesting modes.
        self.active_mut().pop_mode(ModeKind::Draft);
        let Some(repository) = self.active().repository.clone() else {
            return Task::none();
        };
        // jj-only for now — the mutations are jj-lib transactions.
        if !matches!(repository.vcs, Vcs::Jj) {
            return Task::none();
        }
        // Right-click inside the marked set keeps it (the menu shows
        // the batch actions); outside it re-targets the clicked row
        // alone, Finder-style.
        if !self.active().revision_multi_selection.is_empty() {
            let clicked_id = match &key {
                revision_list::RowSelectionKey::WorkingCopy => self
                    .active()
                    .session
                    .commits
                    .working_copy()
                    .map(|row| row.commit_id().to_owned()),
                revision_list::RowSelectionKey::Commit(id) => Some(id.clone()),
            };
            let inside =
                clicked_id.is_some_and(|id| self.active().revision_multi_selection.contains(&id));
            if !inside {
                self.active_mut().revision_multi_selection.clear();
            }
        }
        // macOS pops the native menu (blocking) with a pulsing glow over
        // `row_rect`; every other platform opens the iced overlay at the
        // cursor. Either way the chosen command dispatches the same way.
        self.open_revision_context_menu(repository, selection_from_key(&key), row_rect, cursor)
    }

    /// Turn the selected revision's description strip into its inline editor.
    fn open_description_editor(&mut self) -> Task<Message> {
        let target = self.active_mut();
        if target
            .description_editor()
            .is_some_and(|editor| editor.target == target.session.selected_revision)
        {
            return iced::widget::operation::focus(iced::widget::Id::new(
                diff_panel::DESCRIPTION_EDITOR_ID,
            ));
        }
        if !target.session.capabilities.mutate {
            return Task::none();
        }
        let Some(description) = target
            .session
            .revision_details
            .as_ref()
            .map(|details| details.description.clone())
        else {
            return Task::none();
        };
        // The editor occupies the description's slot in the scrollable
        // revision header. Reveal it before focusing when editing was
        // triggered from farther down a diff.
        target.diff_scroll_offset = 0.0;
        target.pop_mode(ModeKind::Find);
        let editor = DescriptionEditor {
            target: target.session.selected_revision.clone(),
            original: description.clone(),
            content: widget::text_editor::Content::with_text(&description),
            saving_activity: None,
            switch_blocked: false,
        };
        target.push_mode(TabMode::Description(editor));
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        iced::widget::operation::focus(iced::widget::Id::new(diff_panel::DESCRIPTION_EDITOR_ID))
    }

    /// Open the description editor on `target`, browsing to it first when it
    /// isn't the revision on screen.
    fn edit_description(&mut self, target: RevisionSelection) -> Task<Message> {
        if self.active().session.selected_revision == target {
            return self.open_description_editor();
        }
        if let Some(editor) = self.active_mut().description_editor_mut()
            && (editor.is_dirty() || editor.saving_activity.is_some())
        {
            editor.switch_blocked = true;
            return Task::none();
        }
        self.active_mut().pending_description_edit = Some(target.clone());
        self.select_revision(target)
    }

    fn save_description(&mut self) -> Task<Message> {
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        let Some(editor) = self.active().description_editor() else {
            return Task::none();
        };
        if editor.saving_activity.is_some() || !editor.is_dirty() {
            return Task::none();
        }
        let op = mutations::MutationOp::Describe {
            target: editor.target.clone(),
            description: editor.text().trim_end().to_owned(),
        };
        let (activity_id, _) = self.begin_activity(tab_id, "Update description", false);
        if let Some(editor) = self.active_mut().description_editor_mut() {
            editor.saving_activity = Some(activity_id);
            editor.switch_blocked = false;
        }
        self.run_mutation(PendingMutation {
            op,
            tab_id,
            activity_id,
            allow_immutable: false,
        })
    }

    /// Enter target mode with `source` as the draft's first source.
    pub(crate) fn start_draft(
        &mut self,
        kind: mutations::DraftKind,
        source: RevisionSelection,
    ) -> Task<Message> {
        // jj-only, like every mutation.
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        let Some(draft_source) = self.draft_source_for(&source) else {
            return Task::none();
        };
        // Target mode's plain keys and a half-written description can't both
        // own the keyboard. An unsaved edit wins and says so, the same way it
        // blocks a revision or tab switch; a clean editor just closes.
        if let Some(editor) = self.active_mut().description_editor_mut() {
            if editor.is_dirty() || editor.saving_activity.is_some() {
                editor.switch_blocked = true;
                return Task::none();
            }
            self.active_mut().pop_mode(ModeKind::Description);
        }
        let draft = match kind {
            mutations::DraftKind::Rebase { mode } => {
                diffui_core::OpDraft::rebase(mode, vec![draft_source])
            }
            mutations::DraftKind::Squash => diffui_core::OpDraft::squash(draft_source),
            mutations::DraftKind::Merge => diffui_core::OpDraft::merge(draft_source),
        };
        // Two marked-row languages at once would be unreadable — the
        // draft's source wash takes over from the multi-select marks.
        let target = self.active_mut();
        target.revision_multi_selection.clear();
        target.push_mode(TabMode::Draft(DraftUi::new(draft)));
        self.pop_mode(ModeKind::ActivityPopover);
        self.pop_mode(ModeKind::Menu);
        Task::none()
    }

    /// Arm a placement on the active draft, re-previewing the result.
    fn set_draft_placement(&mut self, placement: mutations::PlacementKind) -> Task<Message> {
        if let Some(draft) = self.active_mut().op_draft_mut() {
            draft.draft.placement = placement;
            draft.hover_spot = None;
        }
        self.kick_draft_preview()
    }

    /// Walk the destination candidate `delta` rows, skipping draft sources.
    fn move_draft_candidate(&mut self, delta: i32) -> Task<Message> {
        let target = self.active();
        let len = target.session.commits.len();
        let Some(draft) = target.op_draft() else {
            return Task::none();
        };
        if len == 0 {
            return Task::none();
        }
        // Walk from the current candidate in `delta`'s direction to
        // the next non-source row. The first move anchors on the
        // *draft source's* row (the revision being moved) — a
        // context-menu draft can start on a row that isn't the
        // selection, and starting from the stale selection made the
        // first j/k land somewhere unrelated. One O(n) scan, only on
        // a draft's first nav.
        let session = &target.session;
        let start = draft
            .draft
            .candidate
            .or_else(|| {
                session
                    .commits
                    .iter()
                    .position(|row| draft.draft.is_source(row.commit_id()))
            })
            .or(session.selected_commit_index)
            .map(|index| index as i64 + delta as i64)
            .unwrap_or(if delta >= 0 { 0 } else { len as i64 - 1 });
        let step = if delta >= 0 { 1 } else { -1 };
        let mut next = start;
        let found = loop {
            if next < 0 || next >= len as i64 {
                break None;
            }
            let row = session.commits.row(next as usize);
            if draft.draft.target_valid(row.commit_id()) {
                break Some(next as usize);
            }
            next += step;
        };
        let Some(found) = found else {
            return Task::none();
        };
        if let Some(draft) = self.active_mut().op_draft_mut() {
            draft.draft.candidate = Some(found);
            draft.hover_spot = None;
        }
        // Reveal the candidate row (the sidebar routes the file-reveal
        // token at a draft candidate while target mode is active).
        self.sidebar_file_reveal_token = self.sidebar_file_reveal_token.wrapping_add(1);
        self.kick_draft_preview()
    }

    /// A drag crossed the activation threshold on a commit row.
    fn on_drag_start(&mut self, index: usize) -> Task<Message> {
        // A drag that activates right after a confirm-on-press (the
        // press ran the draft, the move crossed the threshold) must
        // not spawn a phantom draft on the mutation's target.
        if self.mutation_busy() {
            return Task::none();
        }
        // Dragging a row that's already a source of the active draft
        // *continues* that draft — kind, mode, and stacked sources
        // intact. Without this, starting "Whole branch onto…" (or a
        // squash/merge) and then dragging the row to its target
        // silently downgraded the draft to a plain single-revision
        // rebase, so the panel never previewed the resolved branch.
        let target = self.active();
        if let Some(draft) = target.op_draft()
            && index < target.session.commits.len()
            && draft
                .draft
                .is_source(target.session.commits.row(index).commit_id())
        {
            return Task::none();
        }
        let Some(source) = self.selection_at_index(index) else {
            return Task::none();
        };
        // ⌥ at drag start opts into moving the whole subtree.
        let mode = if self.modifiers.alt() {
            mutations::RebaseSourceMode::WithDescendants
        } else {
            mutations::RebaseSourceMode::Revisions
        };
        self.start_draft(mutations::DraftKind::Rebase { mode }, source)
    }

    /// A drag released: execute the draft on the spot, or keep target mode.
    fn on_drag_drop(&mut self, spot: Option<revision_list::DropSpot>) -> Task<Message> {
        let Some(spot) = spot else {
            // Released outside any spot: stay in target mode so the
            // op bar keeps offering click / keyboard picking.
            if let Some(draft) = self.active_mut().op_draft_mut() {
                draft.hover_spot = None;
            }
            return Task::none();
        };
        let Some(ui) = self.active().op_draft() else {
            return Task::none();
        };
        let mut draft = ui.draft.clone();
        // ⌥ held at drop upgrades the move to "with descendants".
        if self.modifiers.alt()
            && let mutations::DraftKind::Rebase { mode } = &mut draft.kind
        {
            *mode = mutations::RebaseSourceMode::WithDescendants;
        }
        // Targets resolve through the graph before the lowering's own
        // check: `op_for`'s guard only sees the `Commit` variant, so
        // a `WorkingCopy` selection naming a source commit would slip
        // through it.
        let op = match spot {
            revision_list::DropSpot::OnRow(index) => self
                .selection_at_index(index)
                .filter(|target| !self.draft_blocks_target(target))
                .and_then(|target| draft.op_for(target, mutations::PlacementKind::Onto)),
            revision_list::DropSpot::Gap { above, below } => {
                match (
                    self.selection_at_index(below),
                    self.selection_at_index(above),
                ) {
                    (Some(parent), Some(child))
                        if !self.draft_blocks_target(&parent)
                            && !self.draft_blocks_target(&child) =>
                    {
                        draft.op_for_gap(parent, child)
                    }
                    _ => None,
                }
            }
        };
        match op {
            Some(op) => {
                self.active_mut().pop_mode(ModeKind::Draft);
                self.start_mutation_op(op)
            }
            // Dropped on a source / vanished row: keep target mode.
            None => {
                if let Some(draft) = self.active_mut().op_draft_mut() {
                    draft.hover_spot = None;
                }
                Task::none()
            }
        }
    }

    /// The preview debounce timer fired: run the simulation that was parked at
    /// kick time, unless a newer kick (or an Idle transition, which clears the
    /// request) superseded it.
    fn run_parked_preview(&mut self, version: u64) -> Task<Message> {
        let Some(draft) = self.active_mut().op_draft_mut() else {
            return Task::none();
        };
        if draft.preview_version != version {
            return Task::none();
        }
        let Some(request) = draft.preview_request.take() else {
            return Task::none();
        };
        let draft = match request.kind {
            mutations::DraftKind::Rebase { mode } => diffui_core::PreviewRequest::Rebase {
                mode,
                sources: request.sources,
                destination: request.destination,
            },
            mutations::DraftKind::Merge => {
                // The merge's second parent is the destination's anchor
                // (gap drops resolve to the parent side, like the
                // confirm path).
                let anchor = request.destination.anchor().clone();
                diffui_core::PreviewRequest::Merge {
                    parents: request.sources.into_iter().chain([anchor]).collect(),
                }
            }
            // Filtered out before the request was parked.
            mutations::DraftKind::Squash => return Task::none(),
        };
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        // A superseded simulation is cancelled rather than left to
        // finish against a candidate the user has already left.
        let mut effects = Vec::new();
        if let Some(previous) = state.jobs.preview.take() {
            effects.push(diffui_core::Effect::Send(diffui_core::Command::Cancel {
                job: previous,
            }));
        }
        let (job, preview) = state.session.preview(draft);
        state.jobs.preview = Some(job);
        effects.extend(preview);
        self.run_effects(tab, effects)
    }

    /// Toolbar view switcher: show the diff or the source browser.
    fn set_main_view(&mut self, mode: MainView) -> Task<Message> {
        if mode == self.active().main_view {
            return Task::none();
        }
        match mode {
            MainView::Diff => {
                self.active_mut().main_view = MainView::Diff;
                // The shared code widget swaps documents: restore
                // this view's saved scroll and drop the other
                // view's shaped-paragraph cache.
                self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
                self.document_version = self.document_version.wrapping_add(1);
                Task::none()
            }
            MainView::Source => {
                // First open browses the selected revision jumped
                // to the diff's selected file; later toggles
                // return to whatever was being browsed.
                let tab = self.active();
                let revision = tab
                    .source
                    .revision
                    .clone()
                    .unwrap_or_else(|| tab.session.selected_revision.clone());
                let jump = tab
                    .source
                    .revision
                    .is_none()
                    .then(|| {
                        tab.session
                            .document
                            .files
                            .get(tab.selected_file)
                            .map(|file| file.path.clone())
                    })
                    .flatten();
                self.open_source_browser(revision, jump)
            }
        }
    }

    /// Drop any mode the window is no longer showing.
    ///
    /// The description editor is the one that bites: it renders inside the
    /// revision header of the diff view, so a view switch, a palette jump, or
    /// any other move off its target leaves it invisible — and, before it was a
    /// mode, still swallowing every keystroke including Esc. Popping it here
    /// means "not on screen" and "not holding the keyboard" cannot come apart.
    fn prune_stale_modes(&mut self) {
        // With no repository open, `view` renders the welcome screen and the
        // open-repo dialog and nothing else — so every other mode would be
        // holding the keyboard from behind a surface that never draws it.
        if self.tabs.is_empty() {
            self.modes.retain(|mode| mode.kind() == ModeKind::OpenRepo);
            self.no_tab.modes.clear();
            return;
        }
        let stale = {
            let tab = self.active();
            tab.description_editor().is_some_and(|editor| {
                tab.main_view != MainView::Diff || editor.target != tab.session.selected_revision
            })
        };
        if stale {
            self.active_mut().pop_mode(ModeKind::Description);
        }
    }

    /// Open (or re-focus) the source browser at `revision`, optionally jumped
    /// to `jump` — the entry point behind the toolbar switcher, the revision
    /// context menu, file-tree right-clicks, and the diff view's per-file
    /// browse button. Repo tabs only; a PR tab has no tree to browse.
    /// Fold a finished draft simulation into the active draft.
    fn apply_draft_preview(
        &mut self,
        tab: TabId,
        result: Result<mutations::DraftSimulation, String>,
    ) -> Task<Message> {
        let Some(ui) = self.tab_mut(tab).and_then(|target| target.op_draft_mut()) else {
            return Task::none();
        };
        ui.preview = match result {
            Ok(preview) => {
                // Refresh the whole-moved-set wash (branch mode's "which
                // branch is this?" answer); merge simulations and failures
                // drop it.
                ui.moved_highlight = match &preview {
                    diffui_core::DraftSimulation::Rebase(rebase) => {
                        rebase.moved_commit_ids.iter().cloned().collect()
                    }
                    diffui_core::DraftSimulation::Merge(_) => HashSet::new(),
                };
                DraftPreviewState::Ready(preview)
            }
            Err(error) => {
                ui.moved_highlight = HashSet::new();
                DraftPreviewState::Failed(error)
            }
        };
        Task::none()
    }

    /// Fold a source-browser listing in. A failed re-list leaves the previous
    /// tree usable under the error banner rather than blanking the pane.
    fn apply_source_tree(
        &mut self,
        tab: TabId,
        revision: RevisionSelection,
        result: Result<Vec<diffui_core::SourceEntry>, String>,
    ) -> Task<Message> {
        let is_active_tab = self.is_active(tab);
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        // A listing for a revision the browser has since left is stale.
        if source_panel::browsed_revision(&target.source) != revision {
            target.jobs.tree = None;
            return Task::none();
        }
        target.jobs.tree = None;
        let state = &mut target.source;
        match result {
            Ok(entries) => {
                state.tree = Some(entries);
                state.tree_epoch = state.tree_epoch.wrapping_add(1);
                state.tree_error = None;
                // A jump scheduled before the listing arrived had no row to
                // reveal yet — re-arm it now that the selected path has one.
                // One-shot, so periodic wc re-lists don't yank the scroll back.
                let rearm = state.reveal_pending && state.selected.is_some();
                state.reveal_pending = false;
                if rearm {
                    state.reveal_token = state.reveal_token.wrapping_add(1);
                    if is_active_tab {
                        self.aim_tree_scroll_at_selected();
                    }
                }
            }
            Err(error) => state.tree_error = Some(error),
        }
        Task::none()
    }

    fn apply_source_file(
        &mut self,
        tab: TabId,
        path: String,
        result: Result<diffui_core::SourceFileLoad, String>,
    ) -> Task<Message> {
        // Allocated up front — the lookup below borrows self.
        let doc_id = self.allocate_document_id();
        let is_active_tab = self.is_active(tab);
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        target.jobs.file = None;
        let state = &mut target.source;
        // A selection that has since moved on drops the result.
        if state.selected.as_deref() != Some(path.as_str()) {
            return Task::none();
        }
        state.loading = None;
        match result {
            Ok(load) => {
                state.file = Some(SourceFileView {
                    file: load.file,
                    line_count: load.line_count,
                    byte_len: load.byte_len,
                    binary: load.binary,
                    too_large: load.too_large,
                    doc_id,
                });
                state.file_error = None;
            }
            Err(error) => {
                state.file = None;
                state.file_error = Some(error);
            }
        }
        // Repaint the (shared) code widget with the new document.
        if is_active_tab {
            self.document_version = self.document_version.wrapping_add(1);
        }
        Task::none()
    }

    /// Full file sources for one background highlight job: hand them to the
    /// tree-sitter parse, which is seconds of CPU on a large file and so stays
    /// off the actor's thread.
    fn apply_file_pair(
        &mut self,
        tab: TabId,
        job: diffui_core::JobId,
        old: Option<String>,
        new: Option<String>,
    ) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        let Some((document_id, file_index)) = target.jobs.file_pairs.remove(&job) else {
            return Task::none();
        };
        if target.session.document_id != document_id {
            return Task::none();
        }
        let Some(file) = target.session.document.files.get(file_index).cloned() else {
            return Task::none();
        };
        // The parse is seconds of CPU on a large file, which is exactly why it
        // isn't on the UI thread — and why it isn't on the actor's either.
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    diffui_core::syntax::highlight_file(file, old.as_deref(), new.as_deref())
                })
                .await
                .unwrap_or_default()
            },
            move |spans| Message::Ui(UiEvent::FileHighlighted(document_id, file_index, spans)),
        )
    }

    /// A fetch finished. "Up to date" when the remote sent nothing back;
    /// otherwise name the fetched target (the activity title may be
    /// ellipsized, the subtitle shows it in full).
    fn finish_fetch(&mut self, tab: TabId, result: Result<Vec<String>, String>) -> Task<Message> {
        let Some((_, activity_id, target)) =
            self.tab_mut(tab).and_then(|state| state.jobs.fetch.take())
        else {
            return Task::none();
        };
        let summary = match result.as_ref() {
            Err(_) => None,
            Ok(lines) if lines.is_empty() => Some("Fetched \u{b7} up to date".to_owned()),
            Ok(_) => Some(match &target {
                FetchTarget::AllRemotes => "Fetched all remotes".to_owned(),
                FetchTarget::RemoteBranch { remote, branch } => {
                    format!("Fetched {remote}/{branch}")
                }
            }),
        };
        let ok = result.is_ok();
        let mut tasks = vec![self.finish_activity_row(tab, activity_id, result, summary)];
        if ok {
            let effects = match self.tab_mut(tab) {
                Some(state) => state.session.snapshot(RefreshOrigin::Focus),
                None => Vec::new(),
            };
            tasks.push(self.run_effects(tab, effects));
        }
        Task::batch(tasks)
    }

    /// A mutation finished. Success resolves its activity and arms the row's
    /// one-click undo; an immutable rejection re-raises the confirmation with
    /// the op intact and the override armed, because the pre-flight can only
    /// see rows the UI directly addressed — a computed-set member (a rebase's
    /// descendant, an insert-after target's child) surfaces only here.
    fn finish_mutation(
        &mut self,
        tab: TabId,
        result: Result<mutations::MutationOutcome, diffui_core::RepoError>,
    ) -> Task<Message> {
        let Some((_, pending)) = self
            .tab_mut(tab)
            .and_then(|state| state.jobs.mutation.take())
        else {
            return Task::none();
        };
        let activity_id = pending.activity_id;
        let description_save = self.tab_mut(tab).is_some_and(|target| {
            target
                .description_editor()
                .as_ref()
                .is_some_and(|editor| editor.saving_activity == Some(activity_id))
        });
        match result {
            Ok(outcome) => {
                if let Some(log) = self.activity_log_for(tab) {
                    if !outcome.output.is_empty() {
                        log.extend_output(activity_id, outcome.output.clone());
                    }
                    log.finish(
                        activity_id,
                        activity::ActivityStatus::Done,
                        Some(outcome.message.clone()),
                    );
                    if let Some(operation_id) = outcome.operation_id.clone() {
                        log.set_undo_op(activity_id, operation_id);
                    }
                }
                if description_save && let Some(target) = self.tab_mut(tab) {
                    target.pop_mode(ModeKind::Description);
                }
                Task::none()
            }
            Err(diffui_core::RepoError::Immutable { short_id }) => {
                self.confirm_immutable(pending, &[short_id], true)
            }
            Err(error) => {
                let error = error.to_string();
                if description_save
                    && let Some(editor) = self
                        .tab_mut(tab)
                        .and_then(|target| target.description_editor_mut())
                {
                    editor.saving_activity = None;
                }
                self.finish_activity_row(tab, activity_id, Err(error), None)
            }
        }
    }

    /// Send the next mutation `tab` is holding, once the one before it has
    /// reported. Called after the projection has folded the completion in, so
    /// its slot is genuinely free by then.
    fn drain_queued_mutation(&mut self, tab: TabId) -> Task<Message> {
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        if state.jobs.mutation.is_some() || state.session.mutation_in_flight() {
            return Task::none();
        }
        match state.queued_mutations.pop_front() {
            Some(next) => self.dispatch_mutation(next),
            None => Task::none(),
        }
    }

    /// The backwards-bookmark-move check resolved: run the move, or raise the
    /// confirmation the jj CLI's `--allow-backwards` stands for.
    fn apply_bookmark_check(&mut self, tab: TabId, result: Result<bool, String>) -> Task<Message> {
        let Some((_, pending)) = self
            .tab_mut(tab)
            .and_then(|state| state.jobs.bookmark_check.take())
        else {
            return Task::none();
        };
        // A check that failed can't tell a backwards move from a fast-forward;
        // run it, as it ran before the guard existed. The mutation path still
        // surfaces any real failure.
        let backwards = result.unwrap_or(false);
        let mutations::MutationOp::MoveBookmark {
            name,
            to,
            push_remote,
        } = &pending.op
        else {
            return self.run_mutation(pending);
        };
        if !backwards {
            return self.run_mutation(pending);
        }
        let target = match to {
            RevisionSelection::WorkingCopy => "The working copy".to_owned(),
            RevisionSelection::Commit(hex) => {
                let _ = hex;
                self.revision_short_label(to)
            }
        };
        // A conflicted bookmark also lands here (it has no single commit to be
        // "a descendant of"), but there the move *is* the fix — the dialog
        // explains the resolution rather than warning about a backwards move
        // that isn't the story.
        let conflicted_sides = self
            .active()
            .session
            .bookmarks
            .bookmarks
            .iter()
            .find(|b| b.name == *name)
            .filter(|b| b.is_conflicted())
            .map(|b| b.local_targets.len());
        let (title, mut body) = match conflicted_sides {
            Some(sides) => (
                format!("Resolve conflicted bookmark \u{201c}{name}\u{201d}?"),
                format!(
                    "\u{201c}{name}\u{201d} is conflicted — it points at {sides} \
                     commits at once (concurrent moves, or a force-pushed \
                     remote). Moving it to {target} picks that side and \
                     resolves the conflict."
                ),
            ),
            None => (
                format!("Move bookmark \u{201c}{name}\u{201d} backwards?"),
                format!(
                    "{target} is not a descendant of the commit \u{201c}{name}\u{201d} \
                     points at, so this is a backwards or sideways move — the jj CLI \
                     refuses it without --allow-backwards."
                ),
            ),
        };
        // A move that also pushes rewrites the remote branch — say so before
        // the user commits to it.
        let confirm_label = match (push_remote, conflicted_sides) {
            (Some(remote), None) => {
                body.push_str(&format!(
                    " The bookmark is then pushed, moving it backwards on \
                     \u{201c}{remote}\u{201d} too."
                ));
                "Move & push anyway".to_owned()
            }
            (Some(remote), Some(_)) => {
                body.push_str(&format!(
                    " The bookmark is then pushed to \u{201c}{remote}\u{201d}."
                ));
                "Move, resolve & push".to_owned()
            }
            (None, Some(_)) => "Move & resolve".to_owned(),
            (None, None) => "Move anyway".to_owned(),
        };
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title,
            body,
            confirm_label,
            pending,
        }));
        Task::none()
    }

    /// Record an operation's result on `tab`'s log, toasting a failure so it is
    /// impossible to miss. The projection reports what happened to the *view*;
    /// the activity row belongs to whoever minted its id.
    fn finish_activity_row(
        &mut self,
        tab: TabId,
        activity_id: activity::ActivityId,
        result: Result<Vec<String>, String>,
        summary: Option<String>,
    ) -> Task<Message> {
        let mut failure: Option<(String, String)> = None;
        if let Some(log) = self.activity_log_for(tab) {
            match result {
                Ok(lines) => {
                    log.extend_output(activity_id, lines);
                    log.finish(activity_id, activity::ActivityStatus::Done, summary);
                }
                Err(error) => {
                    let title = log
                        .label(activity_id)
                        .map(|label| format!("{label} failed"))
                        .unwrap_or_else(|| "Operation failed".to_owned());
                    failure = Some((title, error.clone()));
                    log.append_output(activity_id, error.clone());
                    log.finish(activity_id, activity::ActivityStatus::Error, Some(error));
                }
            }
        }
        if let Some((title, error)) = failure {
            self.push_error_toast(title, &error);
        }
        Task::none()
    }

    pub(crate) fn open_source_browser(
        &mut self,
        revision: RevisionSelection,
        jump: Option<String>,
    ) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let target = self.active_mut();
        if target.repository.is_none() {
            return Task::none();
        }
        // The description editor lives in the diff view's revision header, so
        // leaving that view closes it. An unsaved edit blocks the move instead,
        // the same way it blocks a revision or tab switch.
        if let Some(editor) = target.description_editor_mut() {
            if editor.is_dirty() || editor.saving_activity.is_some() {
                editor.switch_blocked = true;
                return Task::none();
            }
            self.active_mut().pop_mode(ModeKind::Description);
        }
        let target = self.active_mut();
        target.main_view = MainView::Source;

        let changed = target.source.revision.as_ref() != Some(&revision);
        let is_working_copy = matches!(revision, RevisionSelection::WorkingCopy);
        // Commits are immutable — their listing is reusable as-is. The
        // working copy re-lists on every open so the browser mirrors the
        // disk right now.
        let need_tree = changed
            || is_working_copy
            || target.source.tree.is_none()
            || target.source.tree_error.is_some();

        if changed {
            target.source.revision = Some(revision.clone());
            // The old revision's listing is wrong for the new one; clear so
            // the sidebar shows the loading state instead of stale rows.
            // Expansion state, lazily-listed ignored dirs, and the selected
            // path survive — most paths exist across revisions (and the
            // ignored-dir markers vanish from non-wc listings, hiding the
            // saved children automatically), which is what makes "browse
            // this file at another revision" feel continuous.
            target.source.tree = None;
            target.source.tree_epoch = target.source.tree_epoch.wrapping_add(1);
            target.source.tree_error = None;
            target.source.file_error = None;
        }
        if need_tree {
            target.source.version = target.source.version.wrapping_add(1);
            target.source.tree_error = None;
        }

        let jumped = jump.is_some();
        if let Some(path) = jump {
            // Expand every ancestor so the reveal target row exists (chain-
            // compacted dir rows key on full prefixes, which this covers).
            let mut prefix = String::new();
            for component in path.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                target.source.expanded.insert(prefix.clone());
            }
            if target.source.selected.as_deref() != Some(path.as_str()) {
                target.source.file = None;
                target.source.scroll_offset = 0.0;
            }
            target.source.selected = Some(path);
            target.source.file_error = None;
            target.source.reveal_token = target.source.reveal_token.wrapping_add(1);
            // A jump also bumps the scroll-restore token (the switched-in
            // widget must drop the other view's offset), and a restore
            // scheduled in the same render pass as a reveal wins over it —
            // so the reveal alone can't be trusted to land. When the target
            // row is computable now, aim the restore offset at it; when the
            // listing is still in flight, its arrival re-fires the reveal
            // (which then runs in a restore-free pass).
            target.source.reveal_pending = need_tree;
        }

        let need_file = target.source.selected.is_some()
            && (changed
                || jumped
                || is_working_copy
                || (target.source.file.is_none() && target.source.file_error.is_none()));

        // The shared code widget swaps documents: restore this view's saved
        // scroll and drop the other view's shaped-paragraph cache.
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        self.document_version = self.document_version.wrapping_add(1);
        if jumped && !need_tree {
            self.aim_tree_scroll_at_selected();
        }

        let mut tasks = Vec::new();
        if need_tree {
            tasks.push(self.kick_source_tree_load(tab));
        }
        if need_file {
            tasks.push(self.kick_source_file_load(tab));
        }
        Task::batch(tasks)
    }

    /// Point the source sidebar's saved scroll at the selected file's row
    /// (a bit of context above it), so the next scroll *restore* lands
    /// showing it. Used on jumps, where the same-pass restore outranks the
    /// reveal (see `open_source_browser`).
    fn aim_tree_scroll_at_selected(&mut self) {
        const CONTEXT_ABOVE: f64 = 200.0;
        let Some(selected) = self.active().source.selected.clone() else {
            return;
        };
        let (entries, rows) = self.source_entries_and_rows();
        let Some(entry_index) = entries
            .iter()
            .position(|entry| !entry.is_dir && entry.path == selected)
        else {
            return;
        };
        let Some(display) = rows.iter().position(|row| {
            matches!(row, diffui_core::SourceTreeRow::File { entry_index: e, .. } if *e == entry_index)
        }) else {
            return;
        };
        let row_top = revision_list::REVISION_ROW_HEIGHT as f64
            + display as f64 * revision_list::FILE_ROW_HEIGHT as f64;
        self.active_mut().source.tree_scroll_offset = (row_top - CONTEXT_ABOVE).max(0.0);
    }

    /// Keyboard file navigation (j/k) in the source browser: move the
    /// selection to the next/previous *file* row among the currently visible
    /// tree rows (collapsed dirs stay skipped) and load it.
    fn source_select_neighbor(&mut self, delta: i32) -> Task<Message> {
        let (entries, rows) = self.source_entries_and_rows();
        let file_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(display, row)| {
                matches!(row, diffui_core::SourceTreeRow::File { .. }).then_some(display)
            })
            .collect();
        if file_rows.is_empty() {
            return Task::none();
        }
        let selected_entry = self.active().source.selected.as_deref().and_then(|path| {
            entries
                .iter()
                .position(|entry| !entry.is_dir && entry.path == path)
        });
        let current_pos = selected_entry.and_then(|entry_index| {
            file_rows.iter().position(|&display| {
                matches!(
                    rows[display],
                    diffui_core::SourceTreeRow::File { entry_index: e, .. } if e == entry_index
                )
            })
        });
        let next_pos = match current_pos {
            Some(pos) => (pos as i32 + delta).clamp(0, file_rows.len() as i32 - 1) as usize,
            None => 0,
        };
        let diffui_core::SourceTreeRow::File { entry_index, .. } = rows[file_rows[next_pos]] else {
            return Task::none();
        };
        let Some(path) = entries.get(entry_index).map(|entry| entry.path.clone()) else {
            return Task::none();
        };
        let target = self.active_mut();
        target.source.reveal_token = target.source.reveal_token.wrapping_add(1);
        self.select_source_file(path)
    }

    /// Select `path` in the source browser and load its contents (a sidebar
    /// click or keyboard nav). The previous file stays visible until the new
    /// one lands, mirroring how a revision switch keeps the prior diff.
    fn select_source_file(&mut self, path: String) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let target = self.active_mut();
        let reselect = target.source.selected.as_deref() == Some(path.as_str());
        if reselect && (target.source.file.is_some() || target.source.loading.is_some()) {
            return Task::none();
        }
        if !reselect {
            target.source.scroll_offset = 0.0;
        }
        target.source.selected = Some(path);
        target.source.file_error = None;
        self.kick_source_file_load(tab)
    }

    /// Ask for the tree listing of `tab`'s browsed revision.
    fn kick_source_tree_load(&mut self, tab: TabId) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        if !target.session.capabilities.browse {
            return Task::none();
        }
        let revision = source_panel::browsed_revision(&target.source);
        let job = target.session.next_job();
        target.jobs.tree = Some((job, revision.clone()));
        self.send(tab, diffui_core::Command::ListTree { job, revision })
    }

    /// Ask for the contents of `tab`'s browser selection.
    fn kick_source_file_load(&mut self, tab: TabId) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        if !target.session.capabilities.browse {
            return Task::none();
        }
        let Some(path) = target.source.selected.clone() else {
            return Task::none();
        };
        target.source.loading = Some(path.clone());
        let revision = source_panel::browsed_revision(&target.source);
        let job = target.session.next_job();
        target.jobs.file = Some((job, path.clone()));
        self.send(
            tab,
            diffui_core::Command::ReadFile {
                job,
                revision,
                path,
            },
        )
    }

    /// Re-list + re-read `tab`'s browser when it's showing the working copy —
    /// called wherever the repo refreshes (watcher edits, focus regain, the
    /// post-mutation reload), so the browsed "directory" tracks the disk.
    fn refresh_source_if_working_copy(&mut self, tab: TabId) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        if target.main_view != MainView::Source
            || !matches!(target.source.revision, Some(RevisionSelection::WorkingCopy))
        {
            return Task::none();
        }
        let reload_file = target.source.selected.is_some();
        // Previously-expanded ignored dirs re-list too, so an open `target/`
        // exploration tracks the disk like the rest of the tree.
        let dirs: Vec<String> = target.source.dir_children.keys().cloned().collect();
        let mut tasks = vec![self.kick_source_tree_load(tab)];
        if reload_file {
            tasks.push(self.kick_source_file_load(tab));
        }
        for dir in dirs {
            tasks.push(self.list_ignored_dir(tab, dir));
        }
        Task::batch(tasks)
    }

    /// Hand one command to `tab`'s actor. Returns a `Task` only so call sites
    /// read like the other kickers; commands travel down the channel, not
    /// through the runtime.
    pub(crate) fn send(&mut self, tab: TabId, command: diffui_core::Command) -> Task<Message> {
        if let Some(handle) = self.tab_mut(tab).and_then(|target| target.handle.clone()) {
            handle.send(command);
        }
        Task::none()
    }

    /// The composed source entries + their flattened display rows for the
    /// active tab (memoized — see [`source_panel::SourceTreeCache`]). Row
    /// indices point into the returned entries, so consumers take them as a
    /// pair.
    pub(crate) fn source_entries_and_rows(
        &self,
    ) -> (
        std::rc::Rc<Vec<diffui_core::SourceEntry>>,
        std::rc::Rc<Vec<diffui_core::SourceTreeRow>>,
    ) {
        let tab = self.active();
        tab.source_tree_cache
            .borrow_mut()
            .entries_and_rows(&tab.source)
    }

    /// List one level of an unenumerated ignored directory. Ignored content
    /// exists only on disk, so this reads the filesystem directly rather than
    /// going through the actor's tree.
    fn list_ignored_dir(&mut self, tab: TabId, dir: String) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        let Some(repository) = target.repository.clone() else {
            return Task::none();
        };
        match diffui_core::list_ignored_dir(&repository, &dir) {
            Ok(entries) => {
                target.source.dir_children.insert(dir, entries);
                target.source.tree_epoch = target.source.tree_epoch.wrapping_add(1);
            }
            // Surface a failed readdir in the tree banner; the row stays
            // unlisted so the click can retry.
            Err(error) => {
                target.source.expanded.remove(&dir);
                target.source.tree_error = Some(format!("{error:#}"));
            }
        }
        Task::none()
    }

    /// Recompute find matches immediately (no debounce). Used by toggle
    /// presses where the user's intent is immediate.
    pub(crate) fn refind_now(&mut self) -> Task<Message> {
        let target = self.active_mut();
        if let Some(state) = target.find_mut() {
            state.query_version = state.query_version.wrapping_add(1);
        }
        let Some(state) = target.find() else {
            return Task::none();
        };
        let (matches, error) = find::compute_matches(state, target.find_files());
        if let Some(state) = self.active_mut().find_mut() {
            state.matches = matches;
            state.error = error;
            state.active = if state.matches.is_empty() {
                None
            } else {
                Some(0)
            };
            state.scroll_token = state.scroll_token.wrapping_add(1);
        }
        Task::none()
    }

    pub(crate) fn find_advance(&mut self, delta: i32) {
        let Some(state) = self.active_mut().find_mut() else {
            return;
        };
        if state.matches.is_empty() {
            return;
        }
        let len = state.matches.len() as i32;
        let current = state.active.map(|i| i as i32).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        state.active = Some(next as usize);
        state.scroll_token = state.scroll_token.wrapping_add(1);
    }

    /// Handle ⏎ in the palette. In `:` commit-search mode the all-commits scan
    /// is deferred to here (too slow to run per keystroke on a 1M-commit repo):
    /// the first ⏎ runs the scan and shows results; once searched, ⏎ accepts the
    /// highlighted row like any other mode.
    pub(crate) fn palette_submit(&mut self) -> Task<Message> {
        let Some(mut state) = self.take_palette() else {
            return Task::none();
        };
        let trigger_search = state.top().is_some_and(|column| {
            matches!(column.source, ColumnSource::Root)
                && !column.searched
                && palette::revision_mode_needle(&column.query)
                    .is_some_and(|needle| !needle.trim().is_empty())
        });
        if trigger_search {
            let depth = state.stack.len().saturating_sub(1);
            if let Some(column) = state.top_mut() {
                column.searched = true;
                column.dirty = false;
                column.selected = 0;
                column.scroll_y = 0.0;
                // Invalidate the pending debounced recompute so it can't wipe
                // the results we're about to compute.
                column.query_version = column.query_version.wrapping_add(1);
                // The palette is out of the mode stack here (taken above), so
                // this borrows `self` cleanly while mutating the detached
                // column.
                palette::recompute_matches(column, self, true);
            }
            self.push_mode(Mode::Palette(state));
            return widget::operation::scroll_to(
                palette::results_scrollable_id(depth),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
            );
        }
        self.push_mode(Mode::Palette(state));
        self.palette_accept_current()
    }

    /// Re-run the matcher now if the top column's rows are for an older query.
    ///
    /// Matching is debounced, so between the last keystroke and its recompute
    /// the visible rows belong to a query the user has already typed past.
    /// Accepting then runs whatever happened to be highlighted — for a stale
    /// list that is a different command from the one on screen. Recomputing
    /// synchronously costs one match pass on the accept keystroke and makes the
    /// row that runs the row the user is looking at.
    fn palette_recompute_if_dirty(&mut self) {
        let dirty = self
            .palette()
            .and_then(|state| state.top())
            .is_some_and(|column| column.dirty);
        if !dirty {
            return;
        }
        let Some(mut state) = self.take_palette() else {
            return;
        };
        if let Some(column) = state.top_mut() {
            column.dirty = false;
            column.selected = 0;
            column.scroll_y = 0.0;
            // Drop the debounced recompute still in flight; it would land on
            // a closed palette or re-sort what we just settled.
            column.query_version = column.query_version.wrapping_add(1);
            palette::recompute_matches(column, self, false);
        }
        self.push_mode(Mode::Palette(state));
    }

    /// Execute the highlighted result in the rightmost column. Returns the
    /// `Task` chain that performs the corresponding action plus any
    /// followup state (closing the palette, focusing input, etc.).
    pub(crate) fn palette_accept_current(&mut self) -> Task<Message> {
        self.palette_recompute_if_dirty();
        let Some(state) = self.palette() else {
            return Task::none();
        };
        let Some(top) = state.top() else {
            return Task::none();
        };
        let Some(selected) = top.matches.get(top.selected) else {
            return Task::none();
        };
        let item = selected.item.clone();
        let target = match &top.source {
            ColumnSource::Root => None,
            ColumnSource::Actions(target) => Some(target.clone()),
        };

        match (target, &item) {
            // Top-level: command rows run directly; revision/file rows
            // primary-action without going through the Actions column.
            (None, ResultRef::Command(id)) => {
                let id = *id;
                self.recents.push_command(id);
                self.recents.save();
                self.pop_mode(ModeKind::Palette);
                self.run_command(id, CommandArg::None)
            }
            (None, ResultRef::WorkingCopy | ResultRef::Commit { .. } | ResultRef::Bookmark(_)) => {
                self.pop_mode(ModeKind::Palette);
                self.perform(Action::JumpToResult(item))
            }
            (None, ResultRef::File(path)) => {
                let path = path.clone();
                self.pop_mode(ModeKind::Palette);
                self.perform(Action::JumpToFile(path))
            }
            // Actions column: the row is always a Command — run it against
            // the column's target.
            (Some(target), ResultRef::Command(id)) => {
                let id = *id;
                self.recents.push_command(id);
                self.recents.save();
                self.pop_mode(ModeKind::Palette);
                self.run_command(id, CommandArg::Result(target))
            }
            _ => Task::none(),
        }
    }

    pub(crate) fn jump_to_revision_ref(&mut self, target: &ResultRef) -> Task<Message> {
        let Some(selection) = revision_selection(target, self) else {
            return Task::none();
        };

        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let state = self.active_mut();
        // An unsaved description edit blocks a jump exactly as it blocks a row
        // click: the editor is bound to the revision on screen, so moving off
        // it would either drop the text or leave the editor open over someone
        // else's diff.
        if state.session.selected_revision != selection
            && let Some(editor) = state.description_editor_mut()
            && (editor.is_dirty() || editor.saving_activity.is_some())
        {
            editor.switch_blocked = true;
            return Task::none();
        }
        let state = self.active_mut();

        // Already current — no load, no async wait, bump the token now so
        // the next render scrolls the sidebar row into view.
        if state.session.selected_revision == selection {
            state.revision_reveal_token = state.revision_reveal_token.wrapping_add(1);
            return Task::none();
        }

        // A load is already in flight for the same revision; piggyback so the
        // eventual `DiffLoaded` bumps the token for us.
        if state.session.diff_pending() == Some(&selection) {
            state.pending_revision_reveal = true;
            return Task::none();
        }
        // Deferred bump — see the comment on `pending_revision_reveal`.
        state.pending_revision_reveal = true;
        let effects = state.session.load_diff(selection);
        self.run_effects(tab, effects)
    }

    pub(crate) fn jump_to_file_path(&mut self, path: &str) {
        if let Some(index) = self
            .active_mut()
            .session
            .document
            .files
            .iter()
            .position(|f| f.path == path)
        {
            self.active_mut().selected_file = index;
            self.reveal_selected_file_in_tree();
        }
    }

    /// Expand any collapsed ancestors of the selected file so its tree row
    /// is visible (selection moved by j/k, the palette, or the diff scroll
    /// spy may land inside a collapsed directory).
    pub(crate) fn reveal_selected_file_in_tree(&mut self) {
        let target = self.active_mut();
        if target.collapsed_dirs.is_empty() {
            return;
        }
        let Some(file) = target.session.document.files.get(target.selected_file) else {
            return;
        };
        let path = file.path.clone();
        target
            .collapsed_dirs
            .retain(|dir| !path.starts_with(&format!("{dir}/")));
    }

    /// Start a working-copy snapshot for `tab`. Every refresh path funnels
    /// through here (watcher edits, focus regain, tab activation, the
    /// post-mutation reload), so it's also where a working-copy source browse
    /// re-syncs with the disk. Those loads are read-only and version-guarded,
    /// so they ride alongside the snapshot without contending for jj's wc lock.
    /// Fold `tab`'s working copy into `@`. The projection decides whether to
    /// run now or coalesce behind work already in flight; the browser re-lists
    /// alongside it so a browsed working copy tracks the disk.
    pub(crate) fn start_repository_snapshot(
        &mut self,
        tab: TabId,
        origin: RefreshOrigin,
    ) -> Task<Message> {
        let source_refresh = self.refresh_source_if_working_copy(tab);
        let effects = match self.tab_mut(tab) {
            Some(target) => target.session.snapshot(origin),
            None => Vec::new(),
        };
        Task::batch([source_refresh, self.run_effects(tab, effects)])
    }

    pub(crate) fn sort_by_proximity<T>(
        &self,
        items: &mut [T],
        reference: Option<&str>,
        target_of: impl Fn(&T) -> &str,
    ) {
        let index_of = self
            .active()
            .session
            .commit_indices(items.iter().map(&target_of).chain(reference));
        let reference_index = reference.and_then(|r| index_of.get(r).copied());
        items.sort_by_key(|item| proximity_key(&index_of, reference_index, target_of(item)));
    }

    /// Every known remote-tracking bookmark as `(branch, remote)`, ordered
    /// nearest-first to the working copy (alphabetical tiebreak). Shared by the
    /// native fetch menu and the iced fallback so they list identically.
    pub(crate) fn remote_branches_by_proximity(&self) -> Vec<(String, String)> {
        // (branch, remote, target-commit-hex) per known remote bookmark.
        let mut branches: Vec<(String, String, String)> = self
            .active()
            .session
            .bookmarks
            .bookmarks
            .iter()
            .flat_map(|entry| {
                entry
                    .remotes
                    .iter()
                    .map(move |r| (entry.name.clone(), r.remote.clone(), r.target.clone()))
            })
            .collect();
        branches.sort(); // alphabetical baseline (stable tiebreak below)
        branches.dedup();
        let reference = self.active().session.bookmarks.working_copy_commit.clone();
        self.sort_by_proximity(&mut branches, reference.as_deref(), |(_, _, t)| t.as_str());
        branches
            .into_iter()
            .map(|(branch, remote, _)| (branch, remote))
            .collect()
    }

    /// (Re)start the initial load for the active tab's repository — a streaming
    /// cold load for jj, a one-shot load for git. Resets the per-repo view
    /// fields first, so a re-kick (after returning to a tab whose load was
    /// abandoned while it sat in the background) starts from a clean slate.
    /// Hand out the next document identity (monotonic across every tab).
    pub(crate) fn allocate_document_id(&mut self) -> u64 {
        self.next_document_id = self.next_document_id.wrapping_add(1);
        self.next_document_id
    }

    /// Drain the highlight queue of document `id`, keeping a couple of reads in
    /// flight. Each asks the actor for the file's full old/new sources — one
    /// command on a repository that is already open, where every highlight used
    /// to open a workspace of its own.
    pub(crate) fn spawn_highlights(&mut self, document_id: u64) -> Task<Message> {
        const HIGHLIGHT_CONCURRENCY: usize = 2;

        let Some((tab, _)) = self.document_target_mut(document_id) else {
            return Task::none();
        };
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        let Some(handle) = target.handle.clone() else {
            return Task::none();
        };
        let revision = target.session.selected_revision.clone();
        let mut commands = Vec::new();
        while target.session.highlight_in_flight < HIGHLIGHT_CONCURRENCY {
            let Some(file_index) = target.session.highlight_pending.pop_front() else {
                break;
            };
            let Some(file) = target.session.document.files.get(file_index) else {
                continue;
            };
            let (path, old_path) = (file.path.clone(), file.old_path.clone());
            target.session.highlight_in_flight += 1;
            let job = target.session.next_job();
            target
                .jobs
                .file_pairs
                .insert(job, (document_id, file_index));
            commands.push(diffui_core::Command::FilePair {
                job,
                revision: revision.clone(),
                path,
                old_path,
            });
        }
        for command in commands {
            handle.send(command);
        }
        Task::none()
    }

    /// Start (or restart) the load for `tab` once its actor is up.
    ///
    /// A never-loaded tab streams its graph progressively — there is nothing on
    /// screen to preserve. A tab that already has one gets a snapshot instead,
    /// whose op-fingerprint dedup makes it free when nothing changed and a full
    /// reload when an external operation actually landed.
    pub(crate) fn start_tab_load(&mut self, tab: TabId) -> Task<Message> {
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        if matches!(target.session.status, LoadStatus::Loaded) {
            return self.start_repository_snapshot(tab, RefreshOrigin::Focus);
        }
        if target.session.streaming() {
            return Task::none();
        }
        let label = target
            .repository
            .as_ref()
            .map(|repository| format!("Load {}", repo_label(&repository.root).1))
            .unwrap_or_else(|| "Load pull request".to_owned());
        let (activity_id, progress) = self.begin_activity(tab, label, true);
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        target.finish_load_activity(activity::ActivityStatus::Done, None);
        target.pending_load_activity = Some(activity_id);
        target.session.commit_progress = progress;
        target.session.selected_revision = RevisionSelection::WorkingCopy;
        target.selected_file = 0;
        // The cold load clears the graph + diff, so both views belong at the
        // top; the widgets restore from these on the next activation.
        target.sidebar_scroll_offset = 0.0;
        target.diff_scroll_offset = 0.0;
        target.session.pending_refresh = None;

        // The actor answers these in order on its own thread, so the snapshot
        // lands before the walk reads the repo — the ordering the cold load
        // used to get by calling them in sequence.
        let mut effects = target.session.snapshot(RefreshOrigin::Focus);
        effects.extend(target.session.load_diff(RevisionSelection::WorkingCopy));
        effects.extend(target.session.load_graph(true));
        self.run_effects(tab, effects)
    }

    /// Toolbar "Refresh" and the revset input's Enter: re-walk the graph
    /// without clearing what's on screen, so the switch never flashes empty.
    pub(crate) fn reload_graph(&mut self, tab: TabId, label: String) -> Task<Message> {
        let (activity_id, progress) = self.begin_activity(tab, label, true);
        let Some(target) = self.tab_mut(tab) else {
            return Task::none();
        };
        target.finish_load_activity(activity::ActivityStatus::Done, None);
        target.pending_load_activity = Some(activity_id);
        target.session.commit_progress = progress;
        let effects = target.session.load_graph(false);
        self.run_effects(tab, effects)
    }

    pub(crate) fn allocate_activity_id(&mut self) -> activity::ActivityId {
        let id = activity::ActivityId(self.next_activity_id);
        self.next_activity_id = self.next_activity_id.wrapping_add(1);
        id
    }

    /// Start an activity on `tab`'s log, returning its id and the progress
    /// handle the worker reports through. A tab that closed between the caller
    /// resolving it and here still gets a live handle — the work is already
    /// committed, it simply has no log left to show up in.
    pub(crate) fn begin_activity(
        &mut self,
        tab: TabId,
        label: impl Into<String>,
        determinate: bool,
    ) -> (activity::ActivityId, LoadProgress) {
        let id = self.allocate_activity_id();
        let progress = match self.tab_mut(tab) {
            Some(target) => target.activities.start(id, label, determinate),
            None => LoadProgress::default(),
        };
        (id, progress)
    }

    /// Whether a mutation of ours is running. The actor serializes them on its
    /// own thread, so this only decides what the UI shows, not what may start.
    pub(crate) fn mutation_busy(&self) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.state.session.mutation_in_flight())
    }

    /// Whether `target` resolves to one of the active draft's source commits.
    /// Resolved through the loaded graph so a [`RevisionSelection::WorkingCopy`]
    /// naming a source commit is caught too — the core-side `op_for` guard
    /// only inspects the `Commit` variant.
    pub(crate) fn draft_blocks_target(&self, target: &RevisionSelection) -> bool {
        let Some(ui) = self.active().op_draft() else {
            return false;
        };
        self.draft_source_for(target)
            .is_some_and(|source| ui.draft.is_source(&source.commit_id))
    }

    /// Resolve a selection to a draft source against the loaded graph: its
    /// commit id (for validity checks) and a short change-id label for the op
    /// bar. `None` when the row isn't in the loaded graph.
    pub(crate) fn draft_source_for(
        &self,
        selection: &RevisionSelection,
    ) -> Option<diffui_core::DraftSource> {
        let commits = &self.active().session.commits;
        let row = match selection {
            RevisionSelection::WorkingCopy => commits.working_copy(),
            RevisionSelection::Commit(id) => commits.find_by_commit_id(id),
        }?;
        Some(diffui_core::DraftSource {
            selection: selection.clone(),
            commit_id: row.commit_id().to_owned(),
            label: row.change_id().chars().take(8).collect(),
        })
    }

    /// The selection for a commit-store row index (`@` stays `WorkingCopy` so
    /// it never leaks a stale commit id into a mutation).
    pub(crate) fn selection_at_index(&self, index: usize) -> Option<RevisionSelection> {
        if index >= self.active().session.commits.len() {
            return None;
        }
        let row = self.active().session.commits.row(index);
        Some(if row.is_working_copy() {
            RevisionSelection::WorkingCopy
        } else {
            RevisionSelection::Commit(row.commit_id().to_owned())
        })
    }

    /// Execute the active draft on `target` (click / Enter / `o`-`a`-`b`),
    /// leaving target mode. Picking a draft source is a no-op that *keeps*
    /// the draft, so a stray click can't silently do nothing-and-vanish.
    pub(crate) fn confirm_draft_on(
        &mut self,
        target: RevisionSelection,
        placement_override: Option<mutations::PlacementKind>,
    ) -> Task<Message> {
        if self.draft_blocks_target(&target) {
            return Task::none();
        }
        let Some(ui) = self.active().op_draft() else {
            return Task::none();
        };
        let placement = placement_override.unwrap_or(ui.draft.placement);
        let Some(op) = ui.draft.op_for(target, placement) else {
            return Task::none();
        };
        self.active_mut().pop_mode(ModeKind::Draft);
        self.start_mutation_op(op)
    }

    /// Toggle a source on the active draft (space / ⌘-click) without leaving
    /// target mode: stack an extra merge parent / revision to rebase / squash
    /// source, or un-stack one that's already in. The last source can't be
    /// removed — a sourceless draft means nothing; esc is the way out. The
    /// destination still arrives via the normal confirm.
    pub(crate) fn draft_toggle_source(&mut self, selection: RevisionSelection) -> Task<Message> {
        let Some(source) = self.draft_source_for(&selection) else {
            return Task::none();
        };
        // If the keyboard candidate just became a source it's no longer a
        // valid destination — drop it rather than leaving the markers on an
        // invalid row.
        let candidate_now_source = self
            .active_mut()
            .op_draft()
            .as_ref()
            .and_then(|ui| ui.draft.candidate)
            .and_then(|index| self.selection_at_index(index))
            .is_some_and(|candidate| candidate == selection);
        let Some(ui) = self.active_mut().op_draft_mut() else {
            return Task::none();
        };
        if let Some(position) = ui
            .draft
            .sources
            .iter()
            .position(|s| s.commit_id == source.commit_id)
        {
            if ui.draft.sources.len() == 1 {
                return Task::none();
            }
            ui.draft.sources.remove(position);
        } else {
            ui.draft.sources.push(source);
            if candidate_now_source {
                ui.draft.candidate = None;
            }
        }
        // The simulated moved set is definitionally stale once the sources
        // change — clear the wash now rather than showing the old branch
        // until the next simulation lands.
        ui.moved_highlight.clear();
        self.kick_draft_preview()
    }

    /// Float an error toast (bottom-right) for a failed operation. The
    /// activity log keeps the full record; this only makes the failure
    /// impossible to miss. Newest last, capped so a burst can't wall the UI.
    pub(crate) fn push_error_toast(&mut self, title: String, error: &str) {
        const MAX_TOASTS: usize = 4;
        const MAX_DETAIL: usize = 160;
        let mut detail: String = error
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(MAX_DETAIL)
            .collect();
        if error.len() > detail.len() {
            detail.push('…');
        }
        self.next_toast_id = self.next_toast_id.wrapping_add(1);
        self.toasts.push(Toast {
            id: self.next_toast_id,
            title,
            detail,
            born: Instant::now(),
        });
        if self.toasts.len() > MAX_TOASTS {
            let excess = self.toasts.len() - MAX_TOASTS;
            self.toasts.drain(..excess);
        }
    }

    /// Kick the debounced rebase simulation for the draft's current
    /// destination (keyboard candidate + placement, or the live drag spot).
    /// Version-guarded: a newer candidate supersedes the in-flight preview.
    pub(crate) fn kick_draft_preview(&mut self) -> Task<Message> {
        if self.active_mut().repository.is_none() {
            return Task::none();
        }
        // Destination from the drag spot when one is live, else the keyboard
        // candidate + placement. Resolved before borrowing the draft mutably.
        let (spot, candidate, placement, selected_merge_target) = match self.active().op_draft() {
            Some(ui) => (
                ui.hover_spot,
                ui.draft.candidate,
                ui.draft.placement,
                matches!(ui.draft.kind, mutations::DraftKind::Merge)
                    .then(|| ui.draft.sources.last())
                    .flatten()
                    .filter(|_| ui.draft.sources.len() >= 2)
                    .map(|source| source.selection.clone()),
            ),
            None => return Task::none(),
        };
        // A merge with enough selected parents previews that exact set. Feed
        // its final source through the request's destination slot so the
        // shared debounce request stays destination-shaped for rebases.
        let destination = if let Some(target) = selected_merge_target.clone() {
            Some(mutations::Destination::Onto(target))
        } else {
            match spot {
                Some(revision_list::DropSpot::OnRow(index)) => self
                    .selection_at_index(index)
                    .map(mutations::Destination::Onto),
                Some(revision_list::DropSpot::Gap { above, below }) => match (
                    self.selection_at_index(below),
                    self.selection_at_index(above),
                ) {
                    (Some(parent), Some(child)) => {
                        Some(mutations::Destination::Between { parent, child })
                    }
                    _ => None,
                },
                None => candidate
                    .and_then(|index| self.selection_at_index(index))
                    .map(|target| match placement {
                        mutations::PlacementKind::Onto => mutations::Destination::Onto(target),
                        mutations::PlacementKind::After => mutations::Destination::After(target),
                        mutations::PlacementKind::Before => mutations::Destination::Before(target),
                    }),
            }
        };
        // A destination anchored on a source is invalid — resolved through
        // the graph (so a WorkingCopy anchor naming a source is caught) and
        // before the mutable draft borrow below.
        let anchor_blocked = selected_merge_target.is_none()
            && destination
                .as_ref()
                .is_some_and(|destination| self.draft_blocks_target(destination.anchor()));
        let Some(ui) = self.active_mut().op_draft_mut() else {
            return Task::none();
        };
        // The Idle paths must also drop any parked request: they don't bump
        // the version, so a still-running debounce timer would otherwise
        // pick the stale request up and revive a destination the user left.
        if matches!(ui.draft.kind, mutations::DraftKind::Squash) {
            // Squash has no simulation (it rarely conflicts and the op bar
            // already names the fold target).
            ui.preview = DraftPreviewState::Idle;
            ui.preview_request = None;
            return Task::none();
        }
        let Some(destination) = destination else {
            ui.preview = DraftPreviewState::Idle;
            ui.preview_request = None;
            return Task::none();
        };
        if anchor_blocked {
            ui.preview = DraftPreviewState::Idle;
            ui.preview_request = None;
            return Task::none();
        }
        let mut sources: Vec<RevisionSelection> = ui
            .draft
            .sources
            .iter()
            .map(|s| s.selection.clone())
            .collect();
        if selected_merge_target.is_some() {
            let _ = sources.pop();
        }
        ui.preview_version = ui.preview_version.wrapping_add(1);
        let version = ui.preview_version;
        ui.preview = DraftPreviewState::Loading;
        ui.preview_request = Some(crate::PreviewRequest {
            kind: ui.draft.kind,
            sources,
            destination,
        });
        // Debounce: only the version crosses the timer. `DraftPreviewKick`
        // spawns the parked simulation iff the version is still current, so
        // rapid j/j/j candidate hops expire without touching the backend.
        Task::perform(tokio::time::sleep(Duration::from_millis(250)), move |()| {
            Message::Ui(UiEvent::DraftPreviewKick(version))
        })
    }

    /// Send `pending` to the repository actor.
    ///
    /// The actor is single-threaded, so mutations serialize there — the
    /// frontend queue that used to keep them from contending on jj's
    /// working-copy lock is gone. What is left here is the immutable
    /// pre-flight: an op whose known targets are marked immutable in the
    /// loaded graph raises the confirmation dialog instead of dispatching
    /// (unless the dialog's accept already armed the override).
    pub(crate) fn run_mutation(&mut self, pending: PendingMutation) -> Task<Message> {
        if !pending.allow_immutable {
            let immutable = self.immutable_op_targets(&pending.op);
            if !immutable.is_empty() {
                return self.confirm_immutable(pending, &immutable, false);
            }
        }
        let tab = pending.tab_id;
        // One mutation slot per tab, so the second has to wait for the first to
        // report rather than overwrite it — an overwritten slot means nobody
        // owns the first completion, and its activity row spins forever. The
        // actor serializes the work either way; this is what keeps the queued
        // one visible, which is what the activity log shows.
        if self
            .tab_mut(tab)
            .is_some_and(|state| state.jobs.mutation.is_some())
        {
            if let Some(log) = self.activity_log_for(tab) {
                log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
            }
            if let Some(state) = self.tab_mut(tab) {
                state.queued_mutations.push_back(pending);
            }
            return Task::none();
        }
        self.dispatch_mutation(pending)
    }

    /// Send `pending` now. The caller has already established that the tab's
    /// mutation slot is free.
    fn dispatch_mutation(&mut self, pending: PendingMutation) -> Task<Message> {
        let tab = pending.tab_id;
        if let Some(log) = self.activity_log_for(tab) {
            log.set_status(pending.activity_id, activity::ActivityStatus::Running);
        }
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        let (job, effects) = state
            .session
            .mutate(pending.op.clone(), pending.allow_immutable);
        // The projection owns the job; the frontend keeps its twin so the
        // activity row and an immutable retry can find their way back to the op
        // that started them.
        state.jobs.mutation = Some((job, pending));
        self.run_effects(tab, effects)
    }

    /// The short display labels of `op`'s rewrite targets that the loaded graph
    /// marks immutable.
    ///
    /// The target set comes from [`diffui_core::rewritten_targets`], the same
    /// list the actor's guard reads, so the dialog can't name a different set
    /// from the one that will actually be refused. Only rows the UI directly
    /// addresses are caught here — computed sets (a rebase's descendants, an
    /// insert-after target's other children, a parent-squash's destination)
    /// stay the backend guard's job, whose typed rejection re-raises the same
    /// dialog after the fact.
    fn immutable_op_targets(&self, op: &mutations::MutationOp) -> Vec<String> {
        let session = &self.active().session;
        let targets = diffui_core::rewritten_targets(
            op,
            &session.selected_revision,
            session.root_commit_id.as_deref(),
        );
        let mut labels: Vec<String> = Vec::new();
        for selection in &targets {
            let row = match selection {
                RevisionSelection::WorkingCopy => session.commits.working_copy(),
                RevisionSelection::Commit(hex) => session.commits.find_by_commit_id(hex.as_str()),
            };
            if let Some(row) = row
                && row.is_immutable()
            {
                let label = self.revision_short_label(selection);
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        labels
    }

    /// Park `pending` behind the immutable-rewrite confirmation dialog, its
    /// override armed for the accept. `after_rejection` marks the fallback
    /// path — the backend already refused once (a computed-set member was
    /// immutable), so the wording says what *would also* be rewritten.
    fn confirm_immutable(
        &mut self,
        mut pending: PendingMutation,
        targets: &[String],
        after_rejection: bool,
    ) -> Task<Message> {
        use mutations::MutationOp;
        // Hold the activity visibly while the dialog decides; cancel resolves
        // it (ConfirmCancel), accept re-dispatches it.
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            log.set_status(pending.activity_id, activity::ActivityStatus::Queued);
        }
        pending.allow_immutable = true;

        let (verb, confirm_label) = match &pending.op {
            MutationOp::Describe { .. } => ("Describe", "Rewrite anyway"),
            MutationOp::Abandon { .. } => ("Abandon", "Abandon anyway"),
            MutationOp::Edit { .. } => ("Edit", "Edit anyway"),
            MutationOp::Squash { .. } => ("Squash", "Rewrite anyway"),
            MutationOp::Absorb { .. } => ("Absorb from", "Rewrite anyway"),
            _ => ("Rewrite", "Rewrite anyway"),
        };
        let (noun, verb_be) = if targets.len() == 1 {
            ("revision", "is")
        } else {
            ("revisions", "are")
        };
        let names = targets.join(", ");
        let mut body = if after_rejection {
            format!("This operation would also rewrite {names}, which {verb_be} immutable")
        } else {
            format!("{names} {verb_be} immutable")
        };
        body.push_str(
            " — reachable from immutable_heads(), which usually means pushed or \
             otherwise shared history. The jj CLI refuses this without \
             --ignore-immutable.",
        );
        if matches!(pending.op, MutationOp::Edit { .. }) {
            body.push_str(" Editing it makes further working-copy changes amend it in place.");
        }
        // Stacking rather than replacing: a second guarded mutation raised
        // while a dialog is open (a background tab's op being refused) waits
        // under it and surfaces when that one resolves, instead of silently
        // taking over the buttons the user was about to press.
        self.push_mode(Mode::Confirm(ConfirmDialog {
            title: format!("{verb} immutable {noun}?"),
            body,
            confirm_label: confirm_label.to_owned(),
            pending,
        }));
        Task::none()
    }

    /// Short display name for a revision, matching the sidebar: unique
    /// change-id prefix (min 8 chars), `/N`-suffixed for divergent/hidden
    /// copies, falling back to a commit-id prefix for rows not in the graph.
    fn revision_short_label(&self, selection: &RevisionSelection) -> String {
        let commits = &self.active().session.commits;
        let row = match selection {
            RevisionSelection::WorkingCopy => commits.working_copy(),
            RevisionSelection::Commit(hex) => commits.find_by_commit_id(hex),
        };
        match (row, selection) {
            (Some(row), _) => {
                let len = row.shortest_change_id_len().unwrap_or(8).max(8);
                let mut id: String = row.change_id().chars().take(len).collect();
                if let Some(offset) = row.change_offset() {
                    id.push('/');
                    id.push_str(&offset.to_string());
                }
                id
            }
            (None, RevisionSelection::WorkingCopy) => "the working copy".to_owned(),
            (None, RevisionSelection::Commit(hex)) => hex.chars().take(12).collect(),
        }
    }

    /// The activity log for `tab_id`, or `None` if the tab has since closed.
    pub(crate) fn activity_log_for(&mut self, tab_id: TabId) -> Option<&mut activity::ActivityLog> {
        self.tab_mut(tab_id).map(|target| &mut target.activities)
    }

    /// Toolbar "Fetch": fetch the given target (all remotes / one branch),
    /// surfaced as an activity whose expanded output shows the remote messages.
    /// The actor runs it inside the same working-copy lock every other write
    /// takes, so it can no longer overlap a mutation.
    pub(crate) fn start_fetch(&mut self, target: FetchTarget) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        if !self.active().session.capabilities.fetch {
            return Task::none();
        }
        self.pop_mode(ModeKind::Menu);
        let label = match &target {
            FetchTarget::AllRemotes => "Fetching all remotes".to_owned(),
            FetchTarget::RemoteBranch { remote, branch } => format!("Fetching {remote}/{branch}"),
        };
        let (activity_id, _) = self.begin_activity(tab, label, true);
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        let (job, effects) = state.session.fetch(target.clone());
        state.jobs.fetch = Some((job, activity_id, target));
        self.run_effects(tab, effects)
    }

    /// Toolbar "Undo": revert the latest meaningful operation. Routed through
    /// the mutation path like everything else, so it gets the
    /// snapshot-before-mutate discipline and the same activity treatment.
    pub(crate) fn start_undo(&mut self) -> Task<Message> {
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        self.start_mutation_op(mutations::MutationOp::Undo { operation_id: None })
    }

    /// The revset preset menu entries as `(label, expression)`: the active
    /// repo's "Default" (its `revsets.log`) first when there is one, then the
    /// built-in presets. The default is read from the [`TabState::default_revset`]
    /// cache, so building the menu never touches the config files.
    pub(crate) fn revset_menu_entries(&self) -> Vec<(String, String)> {
        let tab = self.active();
        let presets = revset_presets(tab.repository.as_ref().map(|r| r.vcs));
        let mut entries = Vec::with_capacity(presets.len() + 1);
        if !tab.default_revset.is_empty() {
            entries.push(("Default".to_owned(), tab.default_revset.clone()));
        }
        entries.extend(
            presets
                .iter()
                .map(|(label, expr)| ((*label).to_owned(), (*expr).to_owned())),
        );
        entries
    }

    /// Re-evaluate the log against the tab's current revset (Enter in the
    /// revset input, or a preset pick), persisting the filter for this repo.
    ///
    /// Double-buffered: the current graph stays on screen the whole time and is
    /// replaced in one shot when the new walk is ready, so switching revsets
    /// doesn't flash an empty sidebar. The selection is kept (it just won't be
    /// highlighted if it falls outside the new set).
    pub(crate) fn evaluate_revset(&mut self) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        if !self.active().session.capabilities.graph {
            return Task::none();
        }
        self.pop_mode(ModeKind::Menu);
        // Persist the new filter (debounced) for this repo.
        self.mark_geometry_dirty();
        let shown = self.active().session.revset.trim();
        let label = if shown.is_empty() {
            "Evaluate revset: all()".to_owned()
        } else {
            format!("Evaluate revset: {shown}")
        };
        self.reload_graph(tab, label)
    }

    pub(crate) fn view(&self) -> Element<'_, Message> {
        let theme = self.resolved_theme().spec();

        // No repositories open: the empty state owns the whole window. The
        // open-repo dialog still has to be stacked on top here — it's the only
        // way in from this state (⌘O and the empty-state button both summon it),
        // and the main `stack!` that normally carries the overlay is below the
        // early return. Without this the dialog would never render with no tabs
        // open, so ⌘O would look dead and there'd be no visible path forward.
        if self.tabs.is_empty() {
            let content = stack![
                empty_state(self, theme),
                tab_bar::build_open_repo_dialog(self, theme),
            ]
            .width(Length::Fill)
            .height(Length::Fill);
            return container(content)
                .height(Length::Fill)
                .width(Length::Fill)
                .style(move |_| app_shell_style(theme))
                .into();
        }

        let tab_bar = tab_bar::build_tab_bar(self, theme);
        let toolbar = toolbar::build_toolbar(self, theme);

        // Body is always the sidebar + diff panes. All loading feedback lives in
        // the toolbar now (progress line + activity indicator) — there's no
        // full-window cold-load takeover and no diff-pane spinner. On a cold
        // load the sidebar simply grows from empty as batches arrive; a revision
        // switch keeps the prior diff until `DiffLoaded` replaces it.
        // The source browser swaps in its own pair: a file-tree-only sidebar
        // and the plain code pane.
        let (sidebar, diff_pane) = match self.active().main_view {
            MainView::Diff => (
                sidebar::build_sidebar(self, theme),
                diff_panel::build_diff_panel(self, theme),
            ),
            MainView::Source => (
                source_panel::build_source_sidebar(self, theme),
                source_panel::build_source_panel(self, theme),
            ),
        };
        let panels = row![sidebar, vertical_divider(theme), diff_pane]
            .spacing(0)
            .height(Length::Fill);
        let resize_overlay = ResizeHandle::new(
            self.sidebar_width,
            self.sidebar_min_width,
            sidebar::RESIZE_HIT_PADDING,
            |width| Message::Ui(UiEvent::SidebarWidthChanged(width)),
        );
        let palette_overlay = palette::build_overlay(self, theme);
        let body: Element<'_, Message> = stack![panels, resize_overlay, palette_overlay]
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let shell = column![tab_bar, toolbar, horizontal_divider(theme), body]
            .width(Length::Fill)
            .height(Length::Fill);

        // Overlays float above the whole shell. Each returns an empty `Space`
        // when inactive, so they can always be stacked.
        let content: Element<'_, Message> = stack![
            shell,
            activity::activity_popover(self, theme),
            menu::build_overlay(self, theme),
            tab_bar::build_open_repo_dialog(self, theme),
            tab_bar::build_confirm_dialog(self, theme),
            activity::toast_layer(self, theme),
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into();

        container(content)
            .padding(0)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(move |_| app_shell_style(theme))
            .into()
    }

    pub(crate) fn theme(&self) -> Theme {
        self.resolved_theme().iced_theme()
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        // Key presses are published raw and resolved in `update`, against the
        // mode stack and the keymap. Resolving here instead would mean handing
        // the whole keymap and the open modes through `Subscription::with` —
        // and that value becomes part of the subscription's identity, so every
        // mode change would tear the listener down and build a new one.
        //
        // We *must* see Esc even when an iced text_input is focused —
        // text_input captures Esc to clear focus, and `keyboard::listen()`
        // only fires for `Status::Ignored` events, so we'd lose Esc to the
        // input and force the user to press Esc twice (once to unfocus, once
        // to close). `event::listen_with` ignores the capture status and gives
        // us every event; the `consumed` flag it carries is what keeps the
        // revset box's typing out of the plain-key bindings.
        let keyboard = event::listen_with(|event, status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some(Message::Window(WindowEvent::KeyPressed {
                    key,
                    modifiers,
                    consumed: !matches!(status, event::Status::Ignored),
                }))
            }
            _ => None,
        });

        // Modifier tracking for pointer gestures (⌥-drop = move with
        // descendants). Separate from the key listener: `ModifiersChanged`
        // is its own event kind.
        let modifier_events = event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                Some(Message::Window(WindowEvent::ModifiersChanged(modifiers)))
            }
            _ => None,
        });

        let window_events = event::listen()
            .filter_map(|event| match event {
                Event::Window(window::Event::Focused) => Some(WindowEvent::FocusChanged(true)),
                Event::Window(window::Event::Unfocused) => Some(WindowEvent::FocusChanged(false)),
                Event::Window(window::Event::Opened { position, size, .. }) => {
                    Some(WindowEvent::Opened(position, size))
                }
                Event::Window(window::Event::CloseRequested) => Some(WindowEvent::CloseRequested),
                Event::Window(window::Event::Resized(size)) => Some(WindowEvent::Resized(size)),
                Event::Window(window::Event::Moved(position)) => Some(WindowEvent::Moved(position)),
                _ => None,
            })
            .map(Message::Window);
        // Watch the working tree for changes instead of polling. The
        // subscription identity is keyed on the repo root, so the watcher
        // One actor per open repository. Spawning it inside the subscription
        // ties its life to the tab set: iced keeps the subscription while a tab
        // names it and drops it when the last one closes, which shuts the actor
        // — its thread, its workspace, its watcher — down with it.
        let repositories = Subscription::batch(
            self.tabs
                .iter()
                .map(|tab| Subscription::run_with(tab.open_spec(), open_repository)),
        );

        // Per-frame ticks during a palette push/pop animation. iced's
        // `Animation::interpolate_with` is read-only — to actually drive
        // the interpolation forward in time we need iced to keep
        // re-rendering. Subscribing to a 60-Hz timer while the animation
        // is in progress keeps the view function re-running; the handler
        // is a no-op, the side effect is the render itself.
        let palette_animating = self
            .palette()
            .map(|p| p.is_animating(std::time::Instant::now()))
            .unwrap_or(false);
        let palette_tick = if palette_animating {
            time::every(Duration::from_millis(16)).map(|_| Message::Palette(PaletteMessage::Tick))
        } else {
            Subscription::none()
        };

        // While anything is in flight — a load, a diff switch, or any running
        // activity (fetch/undo/push) — tick so the toolbar progress line +
        // spinner animate and reflect live progress.
        let active = self.active();
        let work_in_flight = active.session.loading_since.is_some()
            || active.session.diff_in_flight()
            || active.activities.any_running();
        let loading_tick = if work_in_flight {
            time::every(Duration::from_millis(120)).map(|_| Message::Ui(UiEvent::LoadingTick))
        } else {
            Subscription::none()
        };

        // Drives the debounced geometry save. Active only while a change is
        // pending; the handler writes once the changes settle, then clears the
        // dirty flag, which tears this subscription back down.
        let window_state_tick = if self.geometry_dirty_since.is_some() {
            time::every(WINDOW_STATE_DEBOUNCE).map(|_| Message::Window(WindowEvent::PersistState))
        } else {
            Subscription::none()
        };

        // Per-frame ticks while a right-click menu's row glow is up (so its pulse
        // animates — the render is the effect; only the iced overlay glows, macOS
        // animates natively) or while a submenu is open (so the trajectory apex
        // eases toward the cursor / catches up when it idles).
        let menu_ticking = self
            .menu()
            .is_some_and(|m| m.glow.is_some() || !m.open_path.is_empty());
        let menu_tick = if menu_ticking {
            time::every(Duration::from_millis(16)).map(|_| Message::Menu(MenuMessage::Tick))
        } else {
            Subscription::none()
        };

        // macOS only: once iced applies our resolved theme it pins the window's
        // NSAppearance, and winit then ignores OS appearance changes — so
        // `theme_changes()` below goes silent on macOS after the first frame.
        // While we're following the OS, poll the live application appearance so a
        // system light/dark switch is picked up without a restart. Runs only in
        // System mode; explicit themes don't care what the OS does.
        let system_theme_poll =
            if cfg!(target_os = "macos") && self.selected_theme == ThemePreference::System {
                time::every(Duration::from_secs(1))
                    .map(|_| Message::Window(WindowEvent::PollSystemTheme))
            } else {
                Subscription::none()
            };

        // Drives the custom double-click zoom: while an animation is in flight,
        // tick at ~60fps so each frame steps the window toward its target. Tears
        // itself down the moment the animation completes.
        let zoom_tick = if self.zoom_anim.is_some() {
            time::every(Duration::from_millis(16))
                .map(|_| Message::Window(WindowEvent::ZoomAnimTick))
        } else {
            Subscription::none()
        };

        // Prunes expired error toasts; subscribed only while any are up.
        let toast_tick = if self.toasts.is_empty() {
            Subscription::none()
        } else {
            time::every(Duration::from_millis(500)).map(|_| Message::Ui(UiEvent::ToastTick))
        };

        Subscription::batch([
            keyboard,
            modifier_events,
            window_events,
            repositories,
            palette_tick,
            loading_tick,
            menu_tick,
            window_state_tick,
            system_theme_poll,
            zoom_tick,
            toast_tick,
            system::theme_changes()
                .map(|mode| Message::Window(WindowEvent::SystemThemeChanged(mode))),
        ])
    }

    /// Re-center the native OS window controls (macOS traffic lights) on the tab
    /// strip. macOS pins them to the native title bar, so we reach the window on
    /// the main thread via `window::run` and nudge them through `chrome`. A
    /// no-op where the strip doesn't stand in for the title bar.
    pub(crate) fn reposition_window_controls(&self) -> Task<Message> {
        let Some(bar_height) = chrome::title_bar_height() else {
            return Task::none();
        };
        window::latest()
            .then(move |maybe_id| {
                maybe_id.map_or_else(Task::none, move |id| {
                    window::run(id, move |window| {
                        if let Ok(handle) = window.window_handle() {
                            chrome::position_window_controls(handle.as_raw(), bar_height);
                        }
                    })
                })
            })
            .discard()
    }

    /// Arm the native resize observer once the window exists, so the traffic
    /// lights track resizes on AppKit's timeline rather than a frame behind via
    /// the message loop. Called once on `WindowOpened`.
    pub(crate) fn install_resize_observer(&self) -> Task<Message> {
        let Some(bar_height) = chrome::title_bar_height() else {
            return Task::none();
        };
        window::latest()
            .then(move |maybe_id| {
                maybe_id.map_or_else(Task::none, move |id| {
                    window::run(id, move |window| {
                        if let Ok(handle) = window.window_handle() {
                            chrome::install_window_resize_observer(handle.as_raw(), bar_height);
                        }
                    })
                })
            })
            .discard()
    }

    /// Adjust the native window to cooperate with our custom title-bar strip:
    /// stop AppKit auto-dragging it from the strip (tabs included) and make the
    /// double-click zoom snap instead of morph. Runs once on `WindowOpened`; a
    /// no-op where the strip isn't the title bar. See
    /// [`chrome::configure_custom_titlebar`].
    pub(crate) fn configure_custom_titlebar(&self) -> Task<Message> {
        if !chrome::drag_region() {
            return Task::none();
        }
        window::latest()
            .then(|maybe_id| {
                maybe_id.map_or_else(Task::none, |id| {
                    window::run(id, |window| {
                        if let Ok(handle) = window.window_handle() {
                            chrome::configure_custom_titlebar(handle.as_raw());
                        }
                    })
                })
            })
            .discard()
    }

    /// Mark the persisted session (window geometry, sidebar width, open tabs)
    /// as changed, arming the debounce timer the subscription runs while a save
    /// is pending.
    pub(crate) fn mark_geometry_dirty(&mut self) {
        self.geometry_dirty_since = Some(Instant::now());
    }

    /// Record `root` as the most-recently-opened repository (deduped, newest
    /// first, capped). Surfaced by the open dialog's quick-pick list.
    pub(crate) fn push_recent_repo(&mut self, root: &std::path::Path) {
        let key = root.to_string_lossy().into_owned();
        self.recent_repos.retain(|existing| existing != &key);
        self.recent_repos.insert(0, key);
        self.recent_repos.truncate(RECENT_REPOS_MAX);
    }

    /// Snapshot the current geometry + sidebar width into the persisted form.
    pub(crate) fn current_window_state(&self) -> WindowState {
        WindowState {
            width: Some(self.window_size.width),
            height: Some(self.window_size.height),
            x: self.window_position.map(|p| p.x),
            y: self.window_position.map(|p| p.y),
            sidebar_width: Some(self.sidebar_width),
            diff_wrap: Some(self.diff_wrap),
            diff_split: Some(self.diff_split),
            // GitHub-PR tabs are session-only (no local root to restore from),
            // so they drop out of the persisted set here.
            open_repos: self
                .tabs
                .iter()
                .filter_map(|tab| Some(tab.root()?.to_string_lossy().into_owned()))
                .collect(),
            active_repo: self
                .tabs
                .get(self.active)
                .and_then(|tab| Some(tab.root()?.to_string_lossy().into_owned())),
            revsets: self.collect_revsets(),
            recent_repos: self.recent_repos.clone(),
        }
    }

    /// Gather each open tab's revset, keyed by repo root, dropping empties so
    /// the persisted map stays tidy.
    pub(crate) fn collect_revsets(&self) -> BTreeMap<String, String> {
        let mut revsets = BTreeMap::new();
        for tab in &self.tabs {
            let Some(root) = tab.root() else {
                continue;
            };
            if !tab.state.session.revset.is_empty() {
                revsets.insert(
                    root.to_string_lossy().into_owned(),
                    tab.state.session.revset.clone(),
                );
            }
        }
        revsets
    }

    pub(crate) fn resolved_theme(&self) -> ResolvedTheme {
        self.selected_theme.active(self.system_theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Context;
    use crate::modes::{Mode, ModeKind};
    use crate::palette::PaletteMessage;
    use crate::tabs::tests::{app, push_tab};

    fn press(ui: &mut Diffui, chord: &str) {
        let chord = Chord::parse(chord).expect("a parsable chord");
        let id = ui
            .resolve_chord(ui.key_context(), &chord, false)
            .unwrap_or_else(|| panic!("{chord:?} is bound to nothing"));
        let _ = ui.run_command(id, CommandArg::None);
    }

    /// Matching is debounced, so between a keystroke and its recompute the
    /// visible rows belong to a query the user has already typed past.
    /// Accepting has to run what the query names, not what the stale list left
    /// highlighted.
    #[test]
    fn accepting_a_dirty_palette_runs_what_the_query_names() {
        let mut ui = app();
        // The palette only renders with a repository open, so it only stays
        // open with one — see `prune_stale_modes`.
        push_tab(&mut ui, "/tmp/repo");
        let _ = ui.perform(Action::OpenPalette);
        let stale = ui
            .palette()
            .and_then(|state| state.top())
            .and_then(|column| column.matches.first())
            .map(|m| m.item.clone())
            .expect("the empty query lists the root commands");
        assert!(
            !matches!(&stale, ResultRef::Command(id) if *id == "theme.contrast"),
            "the stale highlight must differ from the answer, or the test proves nothing"
        );

        let _ = ui.update(Message::Palette(PaletteMessage::QueryChanged(
            ">high contrast".to_owned(),
        )));
        assert!(
            ui.palette()
                .and_then(|state| state.top())
                .is_some_and(|column| column.dirty),
            "the debounced recompute has not run yet"
        );

        let _ = ui.palette_accept_current();
        assert_eq!(ui.selected_theme, ThemePreference::HighContrast);
        assert!(ui.palette().is_none(), "accepting closes the palette");
    }

    #[test]
    fn a_confirmation_swallows_every_key_but_its_own() {
        let mut ui = app();
        ui.push_mode(Mode::Confirm(ConfirmDialog {
            title: "Rewrite?".to_owned(),
            body: String::new(),
            confirm_label: "Rewrite anyway".to_owned(),
            pending: PendingMutation {
                op: mutations::MutationOp::Undo { operation_id: None },
                tab_id: TabId(0),
                activity_id: activity::ActivityId(0),
                allow_immutable: false,
            },
        }));
        let context = ui.key_context();
        assert_eq!(context, Context::Confirm);
        for chord in ["cmd+k", "cmd+w", "j", "alt+z"] {
            let chord = Chord::parse(chord).expect("a parsable chord");
            assert_eq!(
                ui.resolve_chord(context, &chord, false),
                None,
                "{chord:?} reached past the dialog"
            );
        }
        let esc = Chord::parse("esc").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(context, &esc, false),
            Some("confirm.cancel")
        );
    }

    /// The base context's plain keys belong to whatever has focus — the revset
    /// box types `j` rather than moving the file selection — but its chords
    /// with a modifier are the window's regardless.
    #[test]
    fn a_focused_input_keeps_the_plain_keys_it_is_typing() {
        let ui = app();
        let plain = Chord::parse("j").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(Context::Base, &plain, false),
            Some("file.next")
        );
        assert_eq!(ui.resolve_chord(Context::Base, &plain, true), None);
        let combo = Chord::parse("cmd+k").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(Context::Base, &combo, true),
            Some("palette.toggle")
        );
    }

    /// Target mode owns the plain keys and hands the modifier combos back, so
    /// `j` picks a destination while ⌘K still opens the palette.
    #[test]
    fn target_mode_keeps_the_plain_keys_and_passes_the_combos_down() {
        let ui = app();
        let j = Chord::parse("j").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(Context::Draft, &j, false),
            Some("draft.candidate.next")
        );
        let combo = Chord::parse("cmd+k").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(Context::Draft, &combo, false),
            Some("palette.toggle")
        );
        // Nothing in target mode binds ⌘W, so the base context answers it.
        let close = Chord::parse("cmd+w").expect("a parsable chord");
        assert_eq!(
            ui.resolve_chord(Context::Draft, &close, false),
            Some("tab.close")
        );
    }

    #[test]
    fn the_palette_chord_toggles_the_palette_from_inside_it() {
        let mut ui = app();
        push_tab(&mut ui, "/tmp/repo");
        press(&mut ui, "cmd+k");
        assert!(ui.palette().is_some());
        assert_eq!(ui.key_context(), Context::Palette);
        press(&mut ui, "cmd+k");
        assert!(ui.palette().is_none());
        assert_eq!(ui.key_context(), Context::Base);
    }

    /// A menu built before a graph reload names rows by commit id. `enabled`
    /// is re-checked when the row is picked, so one the reload dropped can no
    /// longer run its mutation.
    #[test]
    fn a_stale_menu_row_cannot_act_on_a_commit_the_graph_no_longer_has() {
        let mut ui = app();
        ui.push_mode(Mode::ActivityPopover);
        let gone = RevisionSelection::Commit("deadbeef".to_owned());
        let _ = ui.run_command("revision.abandon", CommandArg::Revision(gone));
        // Nothing was queued and nothing was logged: the command refused
        // before it could mint an activity.
        assert!(ui.active().activities.is_empty());
        assert!(ui.active().queued_mutations.is_empty());
        // The popover is still up — the pick was a no-op, not a dismissal.
        assert!(ui.mode(ModeKind::ActivityPopover).is_some());
    }
}
