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

/// The per-tab fields a load completion touches, borrowed from the active
/// inline state or from a backgrounded tab's stash. Routing results to their
/// owner (instead of dropping foreign ones) is what lets a cold graph walk /
/// PR stream / revset eval keep its progress while its tab is backgrounded —
/// a tab switch no longer restarts or discards an in-flight load.
pub(crate) struct LoadTargetMut<'a> {
    pub(crate) session: &'a mut Session,
    pub(crate) selected_file: &'a mut usize,
    pub(crate) activities: &'a mut activity::ActivityLog,
    pub(crate) pending_load_activity: &'a mut Option<activity::ActivityId>,
    pub(crate) pending_revision_reveal: &'a mut bool,
    pub(crate) revision_reveal_token: &'a mut u64,
    /// Whether this is the active tab — gates the active-only follow-ups
    /// (the shaped-paragraph cache bump, empty-status spawns, coalesced
    /// refreshes; a backgrounded tab runs those on its next activation).
    pub(crate) is_active: bool,
}

impl<'a> LoadTargetMut<'a> {
    /// Resolve the owning tab's pending load activity (mirrors
    /// `finish_load_activity`, which only knows the active tab).
    fn finish_load_activity(&mut self, status: activity::ActivityStatus, result: Option<String>) {
        if let Some(id) = self.pending_load_activity.take() {
            self.activities.finish(id, status, result);
        }
    }
}

impl Diffui {
    pub(crate) fn new(cli: Cli, saved: WindowState) -> (Self, Task<Message>) {
        let config = AppConfig::load();
        let sidebar_min_width = sidebar::min_width(config);
        // Restore the persisted sidebar split and window geometry. The sidebar
        // is clamped to its min so a stale width from a narrower font config
        // can't leave it unusable. The window size/position seed the in-memory
        // tracking; the compositor's `Opened` event overwrites them with the
        // real values a frame later, but seeding keeps them correct in between.
        let sidebar_width = saved
            .sidebar_width
            .filter(|w| w.is_finite() && *w > 0.0)
            .unwrap_or(sidebar::DEFAULT_WIDTH)
            .max(sidebar_min_width);
        let window_size = saved
            .size()
            .map(|(w, h)| Size::new(w, h))
            .unwrap_or_else(|| window::Settings::default().size);
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
        let mut targets = Vec::new();
        let mut first_error = None;
        if !cli.paths.is_empty() {
            for path in &cli.paths {
                if let Some(spec) = github::PrSpec::parse(&path.to_string_lossy()) {
                    targets.push(BootTarget::Pr(spec));
                    continue;
                }
                match prepare_repository(path) {
                    Ok(repository) => targets.push(BootTarget::Repo(repository)),
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
                    targets.push(BootTarget::Repo(repository));
                }
            }
            if targets.is_empty()
                && let Ok(cwd) = std::env::current_dir()
                && let Ok(repository) = prepare_repository(&cwd)
            {
                targets.push(BootTarget::Repo(repository));
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

        // Resolve a repo's persisted revset (keyed by root), else its default.
        let revset_for = |repository: &Repository| -> String {
            saved
                .revsets
                .get(&repository.root.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| default_revset(repository))
        };

        let mut next_tab_id = 0u64;
        let mut tabs = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let (owner, name, source, state) = match target {
                BootTarget::Repo(repository) => {
                    let (owner, name) = repo_label(&repository.root);
                    let source = TabSource::Repo {
                        vcs: repository.vcs,
                        root: repository.root.clone(),
                    };
                    let state =
                        RepoState::unloaded(Some(repository.clone()), revset_for(repository));
                    (owner, name, source, state)
                }
                BootTarget::Pr(spec) => (
                    spec.owner.clone(),
                    spec.label(),
                    TabSource::GitHubPr(spec.clone()),
                    RepoState::unloaded(None, String::new()),
                ),
            };
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            // The active tab's state lives inline (no stash); the rest start
            // unloaded and load lazily on first activation.
            let stash = (index != active_index).then_some(state);
            tabs.push(Tab {
                id,
                owner,
                name,
                source,
                stash,
            });
        }

        let active_repository = match targets.get(active_index) {
            Some(BootTarget::Repo(repository)) => Some(repository.clone()),
            _ => None,
        };
        let active_revset = active_repository
            .as_ref()
            .map(revset_for)
            .unwrap_or_default();
        let active_tab = if targets.is_empty() { 0 } else { active_index };
        let status = match (targets.get(active_index), &first_error) {
            (Some(_), _) => LoadStatus::Loading,
            (None, Some(error)) => LoadStatus::Failed(error.clone()),
            (None, None) => LoadStatus::Loaded,
        };

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

        // The active tab starts as a blank `unloaded` shell; `kick_initial_load`
        // below fills it in (and streams the rest). Inactive tabs load on
        // first activation.
        let mut app = Self {
            // The active tab starts unloaded; `status` may already be `Loaded`
            // when no repo is open. `kick_initial_load` below fills in the rest.
            session: {
                let mut session = Session::unloaded(active_repository.clone(), active_revset);
                session.status = status;
                session
            },
            file_list_expanded: true,
            collapsed_dirs: Default::default(),
            app_focused: true,
            selected_theme: config.theme,
            system_theme: iced_theme::Mode::None,
            selected_file: 0,
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
            palette: None,
            recents: Recents::load(),
            find: None,
            revision_reveal_token: 0,
            pending_revision_reveal: false,
            sidebar_file_reveal_token: 0,
            sidebar_scroll_offset: 0.0,
            diff_scroll_offset: 0.0,
            scroll_restore_token: 0,
            document_version: 0,
            sidebar_file_cache: Default::default(),
            tabs,
            active_tab,
            next_tab_id,
            next_load_version: 0,
            next_document_id: 0,
            main_view: MainView::default(),
            source: SourceState::default(),
            source_tree_cache: Default::default(),
            open_repo_dialog: None,
            recent_repos,
            default_revset: active_repository
                .as_ref()
                .map(default_revset)
                .unwrap_or_default(),
            activities: activity::ActivityLog::default(),
            pending_load_activity: None,
            next_activity_id: 0,
            mutation_queue: diffui_core::session::MutationQueue::default(),
            menu: None,
            confirm: None,
            activity_popover_open: false,
            hovered: None,
            description_editor: None,
            pending_description_edit: None,
            op_draft: None,
            modifiers: keyboard::Modifiers::default(),
            toasts: Vec::new(),
            next_toast_id: 0,
        };

        let theme_task = system::theme().map(Message::SystemThemeChanged);
        // Kicks the streaming load for whatever the active tab is — a repo
        // walk, a PR stream, or nothing when no tab opened.
        let load_task = app.ensure_active_loaded();
        (app, Task::batch([load_task, theme_task]))
    }

    /// Handle a completed full backend load (graph + diff + snapshot) for `tab`.
    /// See [`Message::BackendLoaded`]. Routed to the owning tab so a load that
    /// finishes while the tab is backgrounded still lands; on a failed targeted
    /// reload it falls back once to the working copy.
    fn on_backend_loaded(
        &mut self,
        tab: TabId,
        revision: RevisionSelection,
        result: Box<Result<diffui_core::BackendOutput, String>>,
    ) -> Task<Message> {
        match *result {
            Ok(output) => {
                // Routed to the owning tab (active or stashed) so a revset
                // eval / git load / focus reload that completes while its
                // tab is backgrounded still lands — without the tab
                // routing, two tabs both pending on `@` would pass the
                // revision check and swap repos' contents.
                let document_id = self.allocate_document_id();
                let Some(mut target) = self.tab_target_mut(tab) else {
                    return Task::none();
                };
                if target.session.pending_revision.as_ref() != Some(&revision) {
                    return Task::none();
                }
                let is_active = target.is_active;

                let revision_changed = target.session.selected_revision != revision;
                target.session.selected_revision = revision;
                target.session.pending_revision = None;
                target.session.loading_since = None;
                target.session.status = LoadStatus::Loaded;
                target.session.document = output.document;
                target.session.commits = output.commits;
                target.session.graph = output.graph;
                // A refresh swaps the graph atomically; if a cold stream was
                // somehow still in flight, supersede it so its late batches
                // (which assume the now-replaced row indices) are dropped.
                target.session.load = None;
                target.session.commits_version = target.session.commits_version.wrapping_add(1);
                target.session.repository_snapshot = Some(output.snapshot);
                target.session.branch_status = output.branch_status;
                target.session.bookmarks = output.bookmarks;
                target.session.revision_details = output.details;
                *target.selected_file = if revision_changed {
                    0
                } else {
                    (*target.selected_file)
                        .min(target.session.document.files.len().saturating_sub(1))
                };
                // If this load was triggered by the palette, the
                // sidebar didn't yet know the new selected_revision
                // when the user accepted; bump the reveal token now
                // that it's been written so the *next* render scrolls
                // the correct row into view.
                if *target.pending_revision_reveal {
                    *target.pending_revision_reveal = false;
                    *target.revision_reveal_token = target.revision_reveal_token.wrapping_add(1);
                }
                // Recompute the on-demand sidebar index (lane fold, prefix
                // lengths, selected-row index) for the new graph. If the
                // selected commit isn't in the new graph, `selected_commit_index`
                // is `None` — but that's *not* a fall-back trigger: the diff
                // loaded, so the commit still exists, it's just outside the
                // current revset (e.g. a palette jump to an off-view commit,
                // or an abandoned-but-not-yet-GC'd commit). We keep showing
                // it rather than yanking the user to `@`. Only a *failed*
                // resolve (the `Err` arm) means the commit is truly gone.
                target.session.rebuild_sidebar_index();
                target.session.reset_highlights(document_id);
                target.finish_load_activity(activity::ActivityStatus::Done, None);
                // The document was replaced in place; drop the active view's
                // shaped-paragraph cache. (A stashed tab gets the bump on
                // restore.) Empty-status + coalesced refreshes are
                // active-only; a backgrounded tab runs them on activation.
                let highlights = self.spawn_highlights(document_id);
                if is_active {
                    self.document_version = self.document_version.wrapping_add(1);
                    let empty = self.resolve_empty_status();
                    return Task::batch([highlights, empty, self.take_pending_refresh()]);
                }
                return highlights;
            }
            Err(error) => {
                let Some(mut target) = self.tab_target_mut(tab) else {
                    return Task::none();
                };
                if target.session.pending_revision.as_ref() != Some(&revision) {
                    return Task::none();
                }
                let is_active = target.is_active;

                target.session.pending_revision = None;
                target.session.loading_since = None;
                // A reload targeting a specific commit that's since vanished
                // (abandoned *and* GC'd) can't resolve it, failing the whole
                // walk. Retry once against the working copy rather than
                // stranding the tab on an error screen. Guarded on the
                // revision being a commit, so a genuine `@` failure (or a
                // failure of this very retry) still surfaces.
                if !matches!(revision, RevisionSelection::WorkingCopy)
                    && let Some(source) = target.session.source.clone()
                {
                    eprintln!(
                        "diffui: reload of {revision:?} failed ({error}); \
                             falling back to the working copy"
                    );
                    let fallback = RevisionSelection::WorkingCopy;
                    target.session.selected_revision = fallback.clone();
                    *target.selected_file = 0;
                    target.session.pending_revision = Some(fallback.clone());
                    *target.pending_revision_reveal = true;
                    target.session.loading_since = Some(Instant::now());
                    let revset = target.session.revset.clone();
                    let progress = target.session.commit_progress.clone();
                    if is_active {
                        // Stashed tabs keep their scroll; a stale offset is
                        // clamped against the new content on restore.
                        self.diff_scroll_offset = 0.0;
                    }
                    return Task::perform(
                        source.load(fallback.clone(), revset, progress),
                        move |result| Message::BackendLoaded(tab, fallback, Box::new(result)),
                    );
                }
                target.session.status = LoadStatus::Failed(error.clone());
                target.finish_load_activity(activity::ActivityStatus::Error, Some(error));
            }
        }
        Task::none()
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(tab, revision, result) => {
                return self.on_backend_loaded(tab, revision, result);
            }
            Message::CommitsBatch(version, rows) => {
                // Route to whichever tab owns this stream — the active one or
                // a backgrounded stash — so a tab switch doesn't discard a
                // half-done walk. Take the cursor out so the appends below
                // borrow the session fields freely; `take_if` re-asserts the
                // version the router already matched.
                let Some(target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                let Some(mut cursor) = target.session.load.take_if(|c| c.version == version) else {
                    return Task::none();
                };
                let selecting_wc = matches!(
                    target.session.selected_revision,
                    RevisionSelection::WorkingCopy
                );
                // Fold the batch into the growing store/graph via the core
                // engine, then apply the UI-side bits it reports back.
                let fold = diffui_core::session::fold_cold_batch(
                    &mut target.session.commits,
                    &mut target.session.graph,
                    &mut cursor,
                    rows,
                    selecting_wc,
                );
                target.session.sidebar_prefix_lens.extend(fold.prefix_lens);
                if let Some(index) = fold.working_copy_index {
                    target.session.selected_commit_index = Some(index);
                }
                target.session.load = Some(cursor);
                // First batch on screen: lift the full-window loading indicator
                // and reveal the (still-growing) sidebar.
                if matches!(target.session.status, LoadStatus::Loading) {
                    target.session.status = LoadStatus::Loaded;
                    target.session.loading_since = None;
                }
            }
            Message::CommitsFinished(version, result) => {
                let Some(mut target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                target.session.load = None;
                let is_active = target.is_active;
                match *result {
                    Ok(tail) => {
                        target.session.repository_snapshot = Some(tail.snapshot);
                        target.session.branch_status = tail.branch_status;
                        target.session.bookmarks = tail.bookmarks;
                        // Apply the single-parent emptiness resolved in the
                        // loader's final pass, caching each so reloads skip it.
                        for (index, empty) in tail.empty_updates {
                            // Defensive: a superseded/shorter store must never
                            // index past its end (`set_is_empty` already guards
                            // with `get_mut`; the row read did not).
                            if index >= target.session.commits.len() {
                                continue;
                            }
                            let commit_id =
                                target.session.commits.row(index).commit_id().to_owned();
                            target.session.empty_cache.insert(commit_id, empty);
                            target.session.commits.set_is_empty(index, empty);
                        }
                        target.session.commits_version =
                            target.session.commits_version.wrapping_add(1);
                        target.session.selected_commit_index =
                            target.session.find_selected_commit_index();
                        target.finish_load_activity(activity::ActivityStatus::Done, None);
                        // Fill in the merges/roots the loader left unknown, then
                        // run any refresh coalesced during the cold load — but
                        // only for the active tab: those results are guarded to
                        // it, so a backgrounded tab runs both on activation
                        // instead (`ensure_active_loaded`).
                        if is_active {
                            let empty = self.resolve_empty_status();
                            return Task::batch([empty, self.take_pending_refresh()]);
                        }
                    }
                    Err(error) => {
                        target.session.status = LoadStatus::Failed(error.clone());
                        target.session.loading_since = None;
                        target.finish_load_activity(activity::ActivityStatus::Error, Some(error));
                    }
                }
            }
            Message::InitialDiff(version, result) => {
                // Routed by stream version like `CommitsBatch`. Apply only
                // while the owning tab is still on the working copy — a
                // palette jump mid-load supersedes this initial @ diff. (A
                // backgrounded tab has `pending_revision == None`: its
                // one-shot switches were abandoned on stash.) Leaves `status`
                // as `Loading` so the sidebar stays empty until the first
                // commit batch; loading feedback is in the toolbar.
                let document_id = self.allocate_document_id();
                let Some(target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                let on_working_copy = matches!(
                    target.session.selected_revision,
                    RevisionSelection::WorkingCopy
                ) && target
                    .session
                    .pending_revision
                    .as_ref()
                    .is_none_or(|pending| matches!(pending, RevisionSelection::WorkingCopy));
                if !on_working_copy {
                    return Task::none();
                }
                target.session.pending_revision = None;
                let bump = target.is_active;
                match *result {
                    Ok((document, details)) => {
                        target.session.document = document;
                        target.session.revision_details = details;
                        *target.selected_file = 0;
                        target.session.reset_highlights(document_id);
                        // The shaped-paragraph cache keys map to the replaced
                        // text — drop it. A stashed tab gets the same bump on
                        // restore, so only the active one needs it here.
                        if bump {
                            self.document_version = self.document_version.wrapping_add(1);
                        }
                        return self.spawn_highlights(document_id);
                    }
                    Err(error) => {
                        eprintln!("diffui: working-copy diff failed during load: {error}");
                    }
                }
            }
            Message::PrMetaLoaded(version, result) => {
                // Routed by stream version like `CommitsBatch` — the PR
                // stream reuses the `session.load` cursor purely as its
                // guard (interner/fold stay unused); globally monotonic
                // versions make it tab-unique.
                let Some(target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                match *result {
                    Ok(info) => {
                        // The PR header's totals are authoritative: the
                        // files-API fallback zeroes per-file counts on
                        // oversized blobs, so summing parsed files can
                        // undercount (react#36173: 73k summed vs 123k real).
                        target.session.authoritative_totals =
                            Some((info.additions, info.deletions));
                        target.session.revision_details = Some(github::pr_revision_details(&info));
                    }
                    Err(error) => {
                        // Header metadata is cosmetic; the diff stream decides
                        // the tab's fate. Log and move on.
                        eprintln!("diffui: gh pr view failed: {error}");
                    }
                }
            }
            Message::PrCommitsLoaded(version, result) => {
                let Some(target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                match *result {
                    Ok(commits) => {
                        let (store, graph) = github::pr_commit_store(&commits);
                        target.session.commits = store;
                        target.session.graph = graph;
                        target.session.commits_version =
                            target.session.commits_version.wrapping_add(1);
                        target.session.rebuild_sidebar_index();
                    }
                    Err(error) => {
                        // The sidebar list is an enhancement; the diff stands
                        // alone without it.
                        eprintln!("diffui: gh pr view --json commits failed: {error}");
                    }
                }
            }
            Message::PrFilesBatch(version, files) => {
                let Some(target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                // Append without bumping `document_version`: the existing
                // (file, hunk, line) cache keys still map to the same text,
                // so visible rows don't re-shape on every batch of a
                // million-line stream.
                let appended_from = target.session.document.files.len();
                for file in &files {
                    target.session.document.total_additions += file.additions;
                    target.session.document.total_deletions += file.deletions;
                }
                target.session.document.files.extend(files);
                // First batch on screen: lift the loading indicator and show
                // the (still-growing) diff.
                if matches!(target.session.status, LoadStatus::Loading) {
                    target.session.status = LoadStatus::Loaded;
                    target.session.loading_since = None;
                }
                // Queue the new tail for background highlighting (appends keep
                // the document id, so results for earlier files stay valid).
                let document_id = target.session.document_id;
                target.session.enqueue_unhighlighted(appended_from);
                return self.spawn_highlights(document_id);
            }
            Message::PrFinished(version, result) => {
                let Some(mut target) = self.load_target_mut(version) else {
                    return Task::none();
                };
                target.session.load = None;
                target.session.loading_since = None;
                match *result {
                    Ok(()) => {
                        // An empty PR never sends a batch — the stream ending
                        // is what lifts the loading screen then.
                        target.session.status = LoadStatus::Loaded;
                        // GitHub's files API stops listing at 3,000 files;
                        // say so rather than looking complete.
                        let (loaded, total) = target.session.commit_progress.snapshot();
                        let note = (total > 0 && loaded < total).then(|| {
                            format!(
                                "Streamed {loaded} of {total} files \
                                 (GitHub's files API caps the listing)"
                            )
                        });
                        target.finish_load_activity(activity::ActivityStatus::Done, note);
                    }
                    Err(error) => {
                        // Keep a partially-streamed diff on screen (the error
                        // lands in the activity log); fail the tab only when
                        // nothing rendered at all.
                        if target.session.document.files.is_empty() {
                            target.session.status = LoadStatus::Failed(error.clone());
                        }
                        target.finish_load_activity(activity::ActivityStatus::Error, Some(error));
                    }
                }
            }
            Message::DiffLoaded(tab, revision, result) => match *result {
                Ok((document, details)) => {
                    // Routed to the owning tab (active or stashed); see
                    // `BackendLoaded`.
                    let document_id = self.allocate_document_id();
                    let Some(target) = self.tab_target_mut(tab) else {
                        return Task::none();
                    };
                    if target.session.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }
                    let is_active = target.is_active;

                    // A working-copy diff is the definitive emptiness signal for
                    // @ (files present ⇒ not empty) — capture it before
                    // `document` moves so a watcher-refresh edit toggles the @
                    // "empty" chip without a graph re-walk. Synthesized
                    // conflict entries don't count: jj calls a conflicted
                    // merge whose tree is exactly its parents' merge "empty",
                    // and the chip should agree with the graph walk.
                    let wc_empty = matches!(revision, RevisionSelection::WorkingCopy).then(|| {
                        document
                            .files
                            .iter()
                            .all(|file| file.status == diffui_core::DiffFileStatus::Conflicted)
                    });

                    // PR tab: park the outgoing document under the key it was
                    // shown for, so flipping back ("All changes" ↔ a commit)
                    // is an in-memory move instead of a re-download.
                    if target.session.repository.is_none() {
                        let outgoing_key = match &target.session.selected_revision {
                            RevisionSelection::WorkingCopy => String::new(),
                            RevisionSelection::Commit(oid) => oid.clone(),
                        };
                        let outgoing = diffui_core::session::CachedDiff {
                            document: std::mem::take(&mut target.session.document),
                            totals: target.session.authoritative_totals.take(),
                            details: target.session.revision_details.take(),
                        };
                        target.session.pr_diffs.insert(outgoing_key, outgoing);
                        // Commit diffs are small; the one big entry is the
                        // whole-PR document. Past the cap, drop the commit
                        // entries but keep it.
                        if target.session.pr_diffs.len() > 16 {
                            target.session.pr_diffs.retain(|key, _| key.is_empty());
                        }
                    }

                    let revision_changed = target.session.selected_revision != revision;
                    target.session.selected_revision = revision.clone();
                    target.session.pending_revision = None;
                    target.session.loading_since = None;
                    target.session.status = LoadStatus::Loaded;
                    target.session.document = document;
                    target.session.revision_details = details;
                    // The graph is unchanged on a diff-only load; just relocate
                    // the selected row.
                    target.session.selected_commit_index =
                        target.session.find_selected_commit_index();
                    if let Some(empty) = wc_empty
                        && target.session.repository.is_some()
                        && let Some(index) = target.session.selected_commit_index
                    {
                        target.session.commits.set_is_empty(index, empty);
                        target.session.commits_version =
                            target.session.commits_version.wrapping_add(1);
                    }
                    *target.selected_file = if revision_changed {
                        0
                    } else {
                        (*target.selected_file)
                            .min(target.session.document.files.len().saturating_sub(1))
                    };
                    if *target.pending_revision_reveal {
                        *target.pending_revision_reveal = false;
                        *target.revision_reveal_token =
                            target.revision_reveal_token.wrapping_add(1);
                    }
                    target.session.reset_highlights(document_id);
                    if is_active {
                        self.document_version = self.document_version.wrapping_add(1);
                    }
                    let open_description_editor =
                        is_active && self.pending_description_edit.as_ref() == Some(&revision);
                    if open_description_editor {
                        self.pending_description_edit = None;
                    }
                    // Drain a refresh coalesced while this diff load was in
                    // flight (e.g. an op-log signal during a watcher reload) —
                    // this arm returns early, so it would otherwise wait for
                    // the next unrelated message to hit the fall-through.
                    let pending = self.take_pending_refresh();
                    return Task::batch([
                        self.spawn_highlights(document_id),
                        pending,
                        if open_description_editor {
                            Task::done(Message::DescriptionEdit)
                        } else {
                            Task::none()
                        },
                    ]);
                }
                Err(error) => {
                    if self.pending_description_edit.as_ref() == Some(&revision) {
                        self.pending_description_edit = None;
                    }
                    let Some(target) = self.tab_target_mut(tab) else {
                        return Task::none();
                    };
                    if target.session.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    target.session.pending_revision = None;
                    target.session.loading_since = None;
                    if target.session.repository.is_none() {
                        // A PR commit fetch failed — keep the current document
                        // on screen rather than failing the whole tab.
                        eprintln!("diffui: PR commit diff failed: {error}");
                    } else {
                        target.session.status = LoadStatus::Failed(error);
                    }
                }
            },
            Message::RepositorySnapshotLoaded(tab, origin, Ok(snapshot)) => {
                if Some(tab) != self.active_tab_id() {
                    return Task::none();
                }
                self.session.snapshot_pending = false;
                let changed = self
                    .session
                    .repository_snapshot
                    .as_ref()
                    .map(|reflected| reflected.fingerprint.as_str())
                    != Some(snapshot.fingerprint.as_str());
                if changed
                    && self.session.pending_revision.is_none()
                    && let Some(source) = self.session.source.clone()
                {
                    // Escalate a watcher (diff-only) refresh to a full reload
                    // when ops other than our own snapshot landed since the
                    // graph was walked: the snapshot's parent op should be
                    // exactly the op the graph reflects. A CLI `jj edit` /
                    // `jj rebase` fires worktree + op-log signals in one
                    // debounce window; the worktree signal wins the race and
                    // this snapshot advances the fingerprint *past* the
                    // external op — without the escalation, that swallowed the
                    // topology change and the graph stayed stale.
                    let external_op = match (
                        self.session.repository_snapshot.as_ref(),
                        snapshot.parent_fingerprint.as_deref(),
                    ) {
                        (Some(reflected), Some(parent)) => reflected.fingerprint != parent,
                        _ => false,
                    };
                    let origin = if external_op {
                        RefreshOrigin::Focus
                    } else {
                        origin
                    };
                    match origin {
                        RefreshOrigin::Watcher => {
                            // A working-tree edit moved @'s tree but not the
                            // graph topology, so skip the (up to ~1M-commit)
                            // re-walk and just reload @'s diff if it's on screen
                            // (the wc snapshot already ran in
                            // `load_repository_snapshot`, so `load_diff` sees the
                            // edit; `DiffLoaded` re-syncs @'s empty chip).
                            // Viewing another commit ⇒ its diff is unchanged.
                            //
                            // Advance `repository_snapshot` to this op: external
                            // ops are now caught live by the op-log watcher, so a
                            // later focus-regain no longer has to conservatively
                            // re-walk just because our own snapshot moved the op
                            // — that's what kept focus expensive on big repos.
                            // (The narrow race where an external op lands in the
                            // same instant as an edit and is absorbed into this
                            // snapshot self-heals on the next op change.)
                            self.session.repository_snapshot = Some(snapshot.clone());
                            // Keep @'s sidebar "empty" chip live even when the
                            // diff pane is showing another revision: the wc
                            // snapshot just rewrote @'s tree, so its empty↔
                            // non-empty state may have flipped. (When @ *is*
                            // selected the reload below refreshes it too —
                            // redundant but consistent.)
                            self.apply_working_copy_empty(snapshot.working_copy_empty);
                            if matches!(
                                self.session.selected_revision,
                                RevisionSelection::WorkingCopy
                            ) {
                                let revision = self.session.selected_revision.clone();
                                self.session.pending_revision = Some(revision.clone());
                                self.session.loading_since = Some(Instant::now());
                                return Task::perform(
                                    source.diff(revision.clone()),
                                    move |result| {
                                        Message::DiffLoaded(tab, revision, Box::new(result))
                                    },
                                );
                            }
                        }
                        RefreshOrigin::Focus => {
                            // A real topology change (an external op caught by the
                            // op-log watcher, a mutation, a fetch): full reload.
                            // Surface the walk as an activity so a multi-second
                            // graph walk on a big repo doesn't look like a freeze;
                            // `BackendLoaded` records the snapshot and finishes it.
                            let revision = self.session.selected_revision.clone();
                            self.session.pending_revision = Some(revision.clone());
                            self.session.loading_since = Some(Instant::now());
                            let progress = if self.pending_load_activity.is_none() {
                                let (id, progress) =
                                    self.begin_activity("Refresh repository", true);
                                self.pending_load_activity = Some(id);
                                progress
                            } else {
                                LoadProgress::default()
                            };
                            self.session.commit_progress = progress.clone();
                            let revset = self.session.revset.clone();
                            return Task::perform(
                                source.load(revision.clone(), revset, progress),
                                move |result| {
                                    Message::BackendLoaded(tab, revision, Box::new(result))
                                },
                            );
                        }
                    }
                }
                // We reach here only when no reload was kicked (snapshot
                // unchanged, or viewing a non-@ revision on a watcher tick). A
                // toolbar Refresh still wants its activity resolved — there was
                // simply nothing to reload.
                self.finish_load_activity(
                    activity::ActivityStatus::Done,
                    Some("Already up to date".to_owned()),
                );
            }
            Message::RepositorySnapshotLoaded(tab, _, Err(error)) => {
                if Some(tab) != self.active_tab_id() {
                    return Task::none();
                }
                self.session.snapshot_pending = false;
                self.finish_load_activity(activity::ActivityStatus::Error, Some(error.clone()));
                self.session.status = LoadStatus::Failed(error);
            }
            Message::EmptyStatusComputed(tab, version, updates) => {
                // Drop results computed against a graph that's since been
                // replaced — their row indices would no longer line up. The
                // version alone isn't unique across tabs (each session counts
                // its own), hence the tab guard.
                if Some(tab) != self.active_tab_id()
                    || version != self.session.commits_version
                    || updates.is_empty()
                {
                    return Task::none();
                }
                for &(index, empty) in &updates {
                    let commit_id = self.session.commits.row(index).commit_id().to_owned();
                    self.session.empty_cache.insert(commit_id, empty);
                    self.session.commits.set_is_empty(index, empty);
                }
                self.session.commits_version = self.session.commits_version.wrapping_add(1);
            }
            Message::SelectFile(index) => {
                if index < self.session.document.files.len() {
                    self.selected_file = index;
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
            }
            Message::SidebarFileRow(display_index) => {
                let rows =
                    diffui_core::file_tree_rows(&self.session.document.files, &self.collapsed_dirs);
                match rows.get(display_index) {
                    Some(diffui_core::FileTreeRow::File { file_index, .. }) => {
                        if *file_index < self.session.document.files.len() {
                            self.selected_file = *file_index;
                        }
                    }
                    Some(diffui_core::FileTreeRow::Dir { path, .. }) => {
                        if !self.collapsed_dirs.remove(path) {
                            self.collapsed_dirs.insert(path.clone());
                        }
                    }
                    None => {}
                }
            }
            Message::SidebarScrolled(offset) => {
                self.sidebar_scroll_offset = offset;
            }
            Message::DiffScrolled(offset) => {
                self.diff_scroll_offset = offset;
            }
            Message::SelectRowKey(key) => {
                let selection = match key {
                    revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
                    revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id),
                };
                // Target mode: a plain click *arms* the row as the destination
                // candidate (same as hover / j-k); the Apply button, ↵, or a
                // drop executes. One-click execution was too easy to fire by
                // accident once hover started arming rows. ⌘-click toggles the
                // row as a draft *source* and stays in the mode — stack extra
                // merge parents / revisions to rebase / squash sources, or
                // un-stack them.
                if self.op_draft.is_some() {
                    if self.modifiers.command() {
                        return self.draft_toggle_source(selection);
                    }
                    let index = match &selection {
                        RevisionSelection::WorkingCopy => self.session.commits.working_copy_index(),
                        RevisionSelection::Commit(id) => self
                            .session
                            .commits
                            .iter()
                            .position(|row| row.commit_id() == id),
                    };
                    let Some(index) = index else {
                        return Task::none();
                    };
                    let Some(ui) = self.op_draft.as_mut() else {
                        return Task::none();
                    };
                    if ui.draft.candidate == Some(index) {
                        return Task::none();
                    }
                    ui.draft.candidate = Some(index);
                    ui.hover_spot = None;
                    return self.kick_draft_preview();
                }
                if self.session.selected_revision != selection {
                    if let Some(editor) = self.description_editor.as_mut()
                        && editor.is_dirty()
                    {
                        editor.switch_blocked = true;
                        return Task::none();
                    }
                    self.description_editor = None;
                }
                // Re-clicking the already-selected revision toggles its file
                // list without re-running the backend or changing the diff.
                // The toggled value persists across revision switches, so
                // collapsing once stays collapsed wherever the user moves
                // next.
                if self.session.selected_revision == selection {
                    self.file_list_expanded = !self.file_list_expanded;
                } else if self.session.pending_revision.as_ref() == Some(&selection) {
                    // Already loading this revision — let it land.
                } else if let Some(tab) = self.active_tab_id() {
                    // Graph-less (PR) sources go through the cache-aware
                    // switcher: parked documents swap back in for free, and a
                    // live stream blocks the switch.
                    if self.active_pr_spec().is_some() {
                        return self.select_pr_revision(tab, selection);
                    }
                    if let Some(source) = self.session.source.clone() {
                        self.session.pending_revision = Some(selection.clone());
                        self.session.loading_since = Some(Instant::now());
                        let revision = selection.clone();
                        return Task::perform(source.diff(selection), move |result| {
                            Message::DiffLoaded(tab, revision, Box::new(result))
                        });
                    }
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
                // Switching to System: sync to the live OS appearance now rather
                // than render one stale frame until the first poll tick lands.
                if theme == ThemePreference::System {
                    let current = chrome::system_appearance();
                    if current != iced_theme::Mode::None {
                        self.system_theme = current;
                    }
                }
            }
            Message::ToggleDiffWrap => {
                self.diff_wrap = !self.diff_wrap;
                // Wrap changes shaping, so drop the paragraph cache; the
                // height index re-keys off the flag itself. Persist with the
                // usual geometry debounce.
                self.document_version = self.document_version.wrapping_add(1);
                self.mark_geometry_dirty();
            }
            Message::ToggleDiffSplit => {
                self.diff_split = !self.diff_split;
                // Same cache discipline as the wrap toggle: column widths
                // change shaping, the height index re-keys off the flag.
                self.document_version = self.document_version.wrapping_add(1);
                self.mark_geometry_dirty();
            }
            Message::SystemThemeChanged(theme) => {
                self.system_theme = theme;
            }
            Message::PollSystemTheme => {
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
            Message::RevisionContextMenu(key, row_rect, cursor) => {
                // Right-clicking during target mode reads as "do something
                // else instead" — drop the draft rather than nesting modes.
                self.op_draft = None;
                let Some(repository) = self.session.repository.clone() else {
                    return Task::none();
                };
                // jj-only for now — the mutations are jj-lib transactions.
                if !matches!(repository.vcs, Vcs::Jj) {
                    return Task::none();
                }
                // macOS pops the native menu (blocking) with a pulsing glow over
                // `row_rect`; every other platform opens the iced overlay at the
                // cursor. Either way the chosen action dispatches the same way.
                return self.open_revision_context_menu(
                    repository,
                    selection_from_key(&key),
                    row_rect,
                    cursor,
                );
            }
            Message::DescriptionEdit => {
                if self
                    .description_editor
                    .as_ref()
                    .is_some_and(|editor| editor.target == self.session.selected_revision)
                {
                    return iced::widget::operation::focus(iced::widget::Id::new(
                        diff_panel::DESCRIPTION_EDITOR_ID,
                    ));
                }
                let Some(details) = self.session.revision_details.as_ref() else {
                    return Task::none();
                };
                let editable = self
                    .session
                    .repository
                    .as_ref()
                    .is_some_and(|repo| matches!(repo.vcs, Vcs::Jj));
                if !editable {
                    return Task::none();
                }
                // The editor occupies the description's slot in the scrollable
                // revision header. Reveal it before focusing when editing was
                // triggered from farther down a diff.
                self.diff_scroll_offset = 0.0;
                self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
                self.find = None;
                let description = details.description.clone();
                self.description_editor = Some(DescriptionEditor {
                    target: self.session.selected_revision.clone(),
                    original: description.clone(),
                    content: widget::text_editor::Content::with_text(&description),
                    saving_activity: None,
                    switch_blocked: false,
                });
                return iced::widget::operation::focus(iced::widget::Id::new(
                    diff_panel::DESCRIPTION_EDITOR_ID,
                ));
            }
            Message::DescriptionAction(action) => {
                if let Some(editor) = self.description_editor.as_mut()
                    && editor.saving_activity.is_none()
                {
                    editor.content.perform(action);
                    editor.switch_blocked = false;
                }
            }
            Message::DescriptionCancel => {
                if self
                    .description_editor
                    .as_ref()
                    .is_some_and(|editor| editor.saving_activity.is_none())
                {
                    self.description_editor = None;
                    self.pending_description_edit = None;
                }
            }
            Message::DescriptionSave => {
                let Some(editor) = self.description_editor.as_ref() else {
                    return Task::none();
                };
                if editor.saving_activity.is_some() || !editor.is_dirty() {
                    return Task::none();
                }
                let Some(repository) = self.session.repository.clone() else {
                    return Task::none();
                };
                let Some(tab_id) = self.active_tab_id() else {
                    return Task::none();
                };
                let op = mutations::MutationOp::Describe {
                    target: editor.target.clone(),
                    description: editor.text().trim_end().to_owned(),
                };
                let (activity_id, progress) = self.begin_activity("Update description", false);
                if let Some(editor) = self.description_editor.as_mut() {
                    editor.saving_activity = Some(activity_id);
                    editor.switch_blocked = false;
                }
                return self.enqueue_or_run_mutation(PendingMutation {
                    repository,
                    op,
                    tab_id,
                    activity_id,
                    progress,
                });
            }
            Message::MutationCompleted(tab_id, id, result) => {
                let description_save = self
                    .description_editor
                    .as_ref()
                    .is_some_and(|editor| editor.saving_activity == Some(id));
                match *result {
                    Ok(outcome) => {
                        if let Some(log) = self.activity_log_for(tab_id) {
                            if !outcome.output.is_empty() {
                                log.extend_output(id, outcome.output);
                            }
                            log.finish(
                                id,
                                activity::ActivityStatus::Done,
                                Some(outcome.message.clone()),
                            );
                            // Arm the row's one-click "Undo" with the op this
                            // mutation committed.
                            if let Some(operation_id) = outcome.operation_id.clone() {
                                log.set_undo_op(id, operation_id);
                            }
                        }
                        // Only snap the selection back to `@` when the op
                        // actually moved it (new/edit/abandon); bookmark ops
                        // leave it put. The reload itself happens once the queue
                        // drains, in `advance_mutation_queue`.
                        if outcome.moved_working_copy {
                            self.session.selected_revision = RevisionSelection::WorkingCopy;
                        } else if let Some(commit_id) = outcome.rewritten_commit {
                            self.session.selected_revision = RevisionSelection::Commit(commit_id);
                        }
                        if description_save {
                            self.description_editor = None;
                        }
                    }
                    Err(error) => {
                        // Surface the failure in the activity log rather than
                        // failing the whole view — a rejected push shouldn't
                        // blank the panes.
                        let mut toast_title = "Operation failed".to_owned();
                        if let Some(log) = self.activity_log_for(tab_id) {
                            if let Some(label) = log.label(id) {
                                toast_title = format!("{label} failed");
                            }
                            log.append_output(id, error.clone());
                            log.finish(id, activity::ActivityStatus::Error, Some(error.clone()));
                        }
                        // The log is durable but easy to miss — float the
                        // failure too.
                        self.push_error_toast(toast_title, &error);
                        if description_save && let Some(editor) = self.description_editor.as_mut() {
                            editor.saving_activity = None;
                        }
                    }
                }
                // Run the next queued mutation, or reload once the batch is
                // done. Unconditional so a failure can't strand the queue.
                return self.advance_mutation_queue();
            }
            Message::BookmarkMoveChecked(pending, result) => {
                let backwards = match *result {
                    Ok(backwards) => backwards,
                    Err(error) => {
                        // Couldn't determine ancestry — run the move like
                        // before the guard existed; the mutation path
                        // surfaces any real failure.
                        eprintln!("diffui: bookmark ancestry check failed: {error}");
                        false
                    }
                };
                let mutations::MutationOp::MoveBookmark { name, to } = &pending.op else {
                    return self.enqueue_or_run_mutation(*pending);
                };
                if !backwards {
                    return self.enqueue_or_run_mutation(*pending);
                }
                let target = match to {
                    RevisionSelection::WorkingCopy => "The working copy".to_owned(),
                    RevisionSelection::Commit(hex) => self
                        .session
                        .commits
                        .find_by_commit_id(hex)
                        .map(|c| {
                            let len = c.shortest_change_id_len().unwrap_or(8).max(8);
                            let mut id: String = c.change_id().chars().take(len).collect();
                            // Divergent/hidden copies read as `changeid/N`,
                            // like the sidebar and jj log.
                            if let Some(offset) = c.change_offset() {
                                id.push('/');
                                id.push_str(&offset.to_string());
                            }
                            id
                        })
                        .unwrap_or_else(|| hex.chars().take(12).collect()),
                };
                self.confirm = Some(ConfirmDialog {
                    title: format!("Move bookmark \u{201c}{name}\u{201d} backwards?"),
                    body: format!(
                        "{target} is not a descendant of the commit \u{201c}{name}\u{201d} \
                         points at, so this is a backwards or sideways move — the jj CLI \
                         refuses it without --allow-backwards."
                    ),
                    confirm_label: "Move anyway".to_owned(),
                    pending: *pending,
                });
            }
            Message::ConfirmAccept => {
                if let Some(dialog) = self.confirm.take() {
                    return self.enqueue_or_run_mutation(dialog.pending);
                }
            }
            Message::ConfirmCancel => {
                if let Some(dialog) = self.confirm.take() {
                    // Resolve the held activity so it doesn't sit queued forever.
                    if let Some(log) = self.activity_log_for(dialog.pending.tab_id) {
                        log.finish(
                            dialog.pending.activity_id,
                            activity::ActivityStatus::Done,
                            Some("Canceled".to_owned()),
                        );
                    }
                }
            }
            Message::ConfirmNoOp => {}
            Message::DraftStart(kind, source) => {
                // jj-only, like every mutation.
                if !self
                    .session
                    .repository
                    .as_ref()
                    .is_some_and(|repo| matches!(repo.vcs, Vcs::Jj))
                {
                    return Task::none();
                }
                let Some(draft_source) = self.draft_source_for(&source) else {
                    return Task::none();
                };
                let draft = match kind {
                    mutations::DraftKind::Rebase { mode } => {
                        diffui_core::OpDraft::rebase(mode, vec![draft_source])
                    }
                    mutations::DraftKind::Squash => diffui_core::OpDraft::squash(draft_source),
                    mutations::DraftKind::Merge => diffui_core::OpDraft::merge(draft_source),
                };
                self.op_draft = Some(DraftUi::new(draft));
                self.activity_popover_open = false;
                self.menu = None;
            }
            Message::DraftStartKey(kind) => {
                let source = self.session.selected_revision.clone();
                return self.update(Message::DraftStart(kind, source));
            }
            Message::DraftPlacement(placement) => {
                if let Some(ui) = self.op_draft.as_mut() {
                    ui.draft.placement = placement;
                    ui.hover_spot = None;
                }
                return self.kick_draft_preview();
            }
            Message::DraftPlacementKey(placement) => {
                // With a keyboard candidate armed, `o`/`a`/`b` applies right
                // away (jjui's flow); before that they just flip the toggle.
                let candidate = self.op_draft.as_ref().and_then(|ui| ui.draft.candidate);
                match candidate.and_then(|index| self.selection_at_index(index)) {
                    Some(target) => return self.confirm_draft_on(target, Some(placement)),
                    None => return self.update(Message::DraftPlacement(placement)),
                }
            }
            Message::DraftCandidate(delta) => {
                let len = self.session.commits.len();
                let Some(ui) = self.op_draft.as_ref() else {
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
                let start = ui
                    .draft
                    .candidate
                    .or_else(|| {
                        self.session
                            .commits
                            .iter()
                            .position(|row| ui.draft.is_source(row.commit_id()))
                    })
                    .or(self.session.selected_commit_index)
                    .map(|index| index as i64 + delta as i64)
                    .unwrap_or(if delta >= 0 { 0 } else { len as i64 - 1 });
                let step = if delta >= 0 { 1 } else { -1 };
                let mut next = start;
                let found = loop {
                    if next < 0 || next >= len as i64 {
                        break None;
                    }
                    let row = self.session.commits.row(next as usize);
                    if ui.draft.target_valid(row.commit_id()) {
                        break Some(next as usize);
                    }
                    next += step;
                };
                let Some(found) = found else {
                    return Task::none();
                };
                if let Some(ui) = self.op_draft.as_mut() {
                    ui.draft.candidate = Some(found);
                    ui.hover_spot = None;
                }
                // Reveal the candidate row (the sidebar routes the file-reveal
                // token at a draft candidate while target mode is active).
                self.sidebar_file_reveal_token = self.sidebar_file_reveal_token.wrapping_add(1);
                return self.kick_draft_preview();
            }
            Message::DraftConfirm => {
                let Some(target) = self
                    .op_draft
                    .as_ref()
                    .and_then(|ui| ui.draft.candidate)
                    .and_then(|index| self.selection_at_index(index))
                else {
                    return Task::none();
                };
                return self.confirm_draft_on(target, None);
            }
            Message::DraftCancel => {
                self.op_draft = None;
            }
            Message::DraftHoverCandidate(index) => {
                // Mouse-hover targeting: arm the hovered row as the candidate,
                // exactly like j/k. Leaving the rows (`None`) keeps the last
                // candidate armed — same persistence the keyboard gets.
                let Some(index) = index else {
                    return Task::none();
                };
                let Some(ui) = self.op_draft.as_mut() else {
                    return Task::none();
                };
                if ui.draft.candidate == Some(index) || index >= self.session.commits.len() {
                    return Task::none();
                }
                ui.draft.candidate = Some(index);
                return self.kick_draft_preview();
            }
            Message::DraftPreviewKick(version) => {
                // The debounce timer fired: run the simulation that was
                // parked at kick time, unless a newer kick (or an Idle
                // transition, which clears the request) superseded it.
                let Some(repository) = self.session.repository.clone() else {
                    return Task::none();
                };
                let Some(ui) = self.op_draft.as_mut() else {
                    return Task::none();
                };
                if ui.preview_version != version {
                    return Task::none();
                }
                let Some(request) = ui.preview_request.take() else {
                    return Task::none();
                };
                return Task::perform(
                    async move {
                        match request.kind {
                            mutations::DraftKind::Rebase { mode } => mutations::run_rebase_preview(
                                repository,
                                mode,
                                request.sources,
                                request.destination,
                            )
                            .await
                            .map(mutations::DraftSimulation::Rebase),
                            mutations::DraftKind::Merge => {
                                // The merge's second parent is the
                                // destination's anchor (gap drops resolve to
                                // the parent side, like the confirm path).
                                let anchor = request.destination.anchor().clone();
                                let parents: Vec<RevisionSelection> =
                                    request.sources.into_iter().chain([anchor]).collect();
                                mutations::run_merge_preview(repository, parents)
                                    .await
                                    .map(mutations::DraftSimulation::Merge)
                            }
                            // Filtered out before the request was parked.
                            mutations::DraftKind::Squash => {
                                Err("squash has no simulation".to_owned())
                            }
                        }
                    },
                    move |result| Message::DraftPreview(version, Box::new(result)),
                );
            }
            Message::DraftPreview(version, result) => {
                if let Some(ui) = self.op_draft.as_mut()
                    && ui.preview_version == version
                {
                    ui.preview = match *result {
                        Ok(preview) => {
                            // Refresh the whole-moved-set wash (branch mode's
                            // "which branch is this?" answer); non-rebase
                            // simulations and failures drop it.
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
                }
            }
            Message::RevisionDragStart(index) => {
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
                if let Some(ui) = self.op_draft.as_ref()
                    && index < self.session.commits.len()
                    && ui
                        .draft
                        .is_source(self.session.commits.row(index).commit_id())
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
                return self.update(Message::DraftStart(
                    mutations::DraftKind::Rebase { mode },
                    source,
                ));
            }
            Message::RevisionDragHover(spot) => {
                let Some(ui) = self.op_draft.as_mut() else {
                    return Task::none();
                };
                if ui.hover_spot == spot {
                    return Task::none();
                }
                ui.hover_spot = spot;
                ui.draft.candidate = match spot {
                    Some(revision_list::DropSpot::OnRow(index)) => Some(index),
                    _ => None,
                };
                return self.kick_draft_preview();
            }
            Message::RevisionDragDrop(spot) => {
                let Some(spot) = spot else {
                    // Released outside any spot: stay in target mode so the
                    // op bar keeps offering click / keyboard picking.
                    if let Some(ui) = self.op_draft.as_mut() {
                        ui.hover_spot = None;
                    }
                    return Task::none();
                };
                let Some(ui) = self.op_draft.as_ref() else {
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
                        self.op_draft = None;
                        return self.start_mutation_op(op);
                    }
                    // Dropped on a source / vanished row: keep target mode.
                    None => {
                        if let Some(ui) = self.op_draft.as_mut() {
                            ui.hover_spot = None;
                        }
                    }
                }
            }
            Message::UndoActivityOp(activity_id, operation_id) => {
                let Some(tab_id) = self.active_tab_id() else {
                    return Task::none();
                };
                // One-shot: clear the button so a second click can't
                // double-revert the same operation.
                if let Some(log) = self.activity_log_for(tab_id) {
                    log.clear_undo_op(activity_id);
                }
                return self.start_mutation_op(mutations::MutationOp::Undo {
                    operation_id: Some(operation_id),
                });
            }
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
            }
            Message::DraftToggleSource => {
                let Some(target) = self
                    .op_draft
                    .as_ref()
                    .and_then(|ui| ui.draft.candidate)
                    .and_then(|index| self.selection_at_index(index))
                else {
                    return Task::none();
                };
                return self.draft_toggle_source(target);
            }
            Message::ToastDismiss(id) => {
                self.toasts.retain(|toast| toast.id != id);
            }
            Message::ToastTick => {
                self.toasts.retain(|toast| toast.born.elapsed() < TOAST_TTL);
            }
            Message::FileHighlighted(document_id, file_index, spans) => {
                let Some((session, is_active)) = self.session_by_document_mut(document_id) else {
                    // The document was replaced; this result highlighted a
                    // dead snapshot.
                    return Task::none();
                };
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
                // whole point of the paint/layout version split. A stashed
                // tab repaints on restore anyway.
                if applied && is_active {
                    self.document_version = self.document_version.wrapping_add(1);
                }
                return self.spawn_highlights(document_id);
            }
            Message::WindowFocusChanged(focused) => {
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

                if gained_focus {
                    return self.start_repository_snapshot(RefreshOrigin::Focus);
                }
            }
            Message::RefreshRepository => {
                // Hold off the (frequent, uncontrolled) watcher snapshot while a
                // mutation is running or queued — both take jj's wc lock, and
                // the post-batch reload re-snapshots, so nothing is lost.
                if self.app_focused && !self.mutation_busy() {
                    return self.start_repository_snapshot(RefreshOrigin::Watcher);
                }
            }
            Message::OpLogChanged => {
                // An op landed. Read the head cheaply (off-thread, no wc lock)
                // and let `OpHeadChecked` decide; while a snapshot/load is
                // already in flight, coalesce instead of dropping — the
                // deferred snapshot's parent-op check catches an external op
                // that landed mid-work (dropping used to lose ops that raced a
                // cold load). Mid-mutation stays a plain skip (the post-batch
                // reload re-snapshots). Gated on focus like the watcher
                // refresh — ops that land while unfocused are caught by the
                // focus-regain reload.
                if self.app_focused && !self.mutation_busy() {
                    if self.session.snapshot_pending
                        || self.session.load.is_some()
                        || self.session.pending_revision.is_some()
                    {
                        self.session.pending_refresh = Some(coalesce_refresh(
                            self.session.pending_refresh,
                            RefreshOrigin::Watcher,
                        ));
                    } else if let Some(source) = self.session.source.clone()
                        && let Some(tab) = self.active_tab_id()
                    {
                        return Task::perform(source.op_head(), move |result| {
                            Message::OpHeadChecked(tab, Box::new(result))
                        });
                    }
                }
            }
            Message::OpHeadChecked(tab, result) => {
                if Some(tab) != self.active_tab_id() {
                    return Task::none();
                }
                // Reload only when the on-disk op differs from the one the graph
                // reflects — i.e. it was an *external* op, not our own wc
                // snapshot / mutation (whose op is already recorded in
                // `repository_snapshot`). That dedup is what keeps our own writes
                // from each triggering a full re-walk. Errors are swallowed: a
                // failed cheap read just means we wait for the next signal.
                if let Ok(Some(on_disk)) = *result {
                    let reflects = self
                        .session
                        .repository_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.fingerprint.as_str());
                    if reflects != Some(on_disk.as_str()) {
                        return self.start_repository_snapshot(RefreshOrigin::Focus);
                    }
                }
            }
            Message::LoadingTick => {}
            Message::SelectNextFile => {
                if self.main_view == MainView::Source {
                    return self.source_select_neighbor(1);
                }
                if !self.session.document.files.is_empty() {
                    self.selected_file = (self.selected_file + 1)
                        .min(self.session.document.files.len().saturating_sub(1));
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
            }
            Message::SelectPreviousFile => {
                if self.main_view == MainView::Source {
                    return self.source_select_neighbor(-1);
                }
                let previous = self.selected_file.saturating_sub(1);
                if previous != self.selected_file {
                    self.selected_file = previous;
                    self.reveal_selected_file_in_tree();
                    return scroll_sidebar_to_file(self);
                }
            }
            Message::CopyToClipboard(text) => {
                return iced::clipboard::write(text).discard();
            }
            Message::SidebarWidthChanged(width) => {
                let clamped = width.max(self.sidebar_min_width);
                if clamped != self.sidebar_width {
                    self.sidebar_width = clamped;
                    self.mark_geometry_dirty();
                }
            }
            Message::WindowOpened(position, size) => {
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
            Message::WindowResized(size) => {
                if self.window_size != size {
                    self.window_size = size;
                    self.mark_geometry_dirty();
                }
                // The native resize observer (armed on open) re-centers the
                // traffic lights in step with AppKit's layout. This message-loop
                // reposition stays as a harmless fallback — it runs a frame
                // later and just re-applies the same position the observer
                // already set, so it can't reintroduce the jump.
                return self.reposition_window_controls();
            }
            Message::WindowMoved(position) => {
                if self.window_position != Some(position) {
                    self.window_position = Some(position);
                    self.mark_geometry_dirty();
                }
            }
            Message::PersistWindowState => {
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
            Message::SelectTab(id) => {
                return self.activate_tab(id);
            }
            Message::SelectTabIndex(index) => {
                if let Some(tab) = self.tabs.get(index) {
                    let id = tab.id;
                    return self.activate_tab(id);
                }
            }
            Message::CloseTab(id) => {
                return self.close_tab(id);
            }
            Message::CloseActiveTab => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    let id = tab.id;
                    return self.close_tab(id);
                }
            }
            Message::OpenRepoDialogOpen => {
                // Mutually exclusive with the other overlays.
                self.palette = None;
                self.find = None;
                self.open_repo_dialog = Some(OpenRepoDialog::default());
                return widget::operation::focus(tab_bar::OPEN_REPO_INPUT_ID);
            }
            Message::OpenRepoDialogClose => {
                self.open_repo_dialog = None;
            }
            Message::OpenRepoPathChanged(path) => {
                if let Some(dialog) = self.open_repo_dialog.as_mut() {
                    dialog.path = path;
                    // Clear a stale error as soon as the user edits the path.
                    dialog.error = None;
                }
            }
            Message::OpenRepoSubmit => {
                let path = self
                    .open_repo_dialog
                    .as_ref()
                    .map(|dialog| dialog.path.clone())
                    .unwrap_or_default();
                return self.open_repository(&path);
            }
            Message::OpenRecentRepo(path) => {
                return self.open_repository(&path);
            }
            Message::OpenRepoNoOp => {}
            Message::TitleBarDrag => {
                // Resolve the (single) window and begin an interactive drag.
                // No-op if the window id isn't available yet.
                return window::latest().then(|id| id.map_or_else(Task::none, window::drag));
            }
            Message::TitleBarDoubleClick => {
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
                        Message::TitleBarDoubleClickPlan {
                            current,
                            visible,
                            action,
                            duration,
                        }
                    });
            }
            Message::TitleBarDoubleClickPlan {
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
            Message::ZoomAnimTick => {
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
            Message::Palette(PaletteMessage::Open) => {
                if self.palette.is_none() {
                    // Mutually exclusive with the find bar / open-repo dialog:
                    // opening the palette pulls keyboard focus and the others
                    // would sit behind the modal anyway.
                    self.find = None;
                    self.open_repo_dialog = None;
                    self.palette = Some(PaletteState::open(self));
                    return widget::operation::focus(palette::PALETTE_INPUT_ID);
                }
            }
            Message::Palette(PaletteMessage::Close) => {
                self.palette = None;
            }
            Message::Palette(PaletteMessage::QueryChanged(query)) => {
                // Take the palette out of `self` so the matcher can borrow
                // `&self` (commits / files / recents) directly. Previously
                // this cloned the entire app per keystroke; on a 40k-commit
                // repo that deep clone was the bulk of the typing latency.
                let Some(mut state) = self.palette.take() else {
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
                self.palette = Some(state);
                return task;
            }
            Message::Palette(PaletteMessage::Recompute(depth, version)) => {
                let Some(mut state) = self.palette.take() else {
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
                self.palette = Some(state);
                return task;
            }
            Message::Palette(PaletteMessage::MoveSelection(delta)) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && !column.matches.is_empty()
                {
                    let len = column.matches.len() as i32;
                    let next = (column.selected as i32 + delta).rem_euclid(len);
                    column.selected = next as usize;
                    let depth = state.stack.len().saturating_sub(1);
                    let column = state.stack.last_mut().expect("top column");
                    if column.ensure_selected_visible() {
                        return widget::operation::scroll_to(
                            palette::results_scrollable_id(depth),
                            iced::widget::scrollable::AbsoluteOffset {
                                x: 0.0,
                                y: column.scroll_y,
                            },
                        );
                    }
                }
            }
            Message::Palette(PaletteMessage::SelectIndex(index)) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
            }
            Message::Palette(PaletteMessage::Accept) => {
                return self.palette_submit();
            }
            Message::Palette(PaletteMessage::AcceptIndex(index)) => {
                if let Some(state) = self.palette.as_mut()
                    && let Some(column) = state.top_mut()
                    && index < column.matches.len()
                {
                    column.selected = index;
                }
                return self.palette_accept_current();
            }
            Message::Palette(PaletteMessage::PushActions) => {
                let Some(mut state) = self.palette.take() else {
                    return Task::none();
                };
                let pushed = state.push_actions(self);
                self.palette = Some(state);
                if pushed {
                    return widget::operation::focus(palette::PALETTE_INPUT_ID);
                }
            }
            Message::Palette(PaletteMessage::NoOp) => {}
            Message::Palette(PaletteMessage::Tick) => {}
            Message::Palette(PaletteMessage::PopColumn) => {
                if let Some(state) = self.palette.as_mut() {
                    if state.pop() {
                        return widget::operation::focus(palette::PALETTE_INPUT_ID);
                    } else {
                        self.palette = None;
                    }
                }
            }
            Message::Find(FindMessage::Open) => {
                // Mutually exclusive with the palette / open-repo dialog: same
                // keyboard focus arbiter, and stacking overlays makes the find
                // bar look broken.
                self.palette = None;
                self.open_repo_dialog = None;
                if self.find.is_none() {
                    self.find = Some(FindState::default());
                }
                return widget::operation::focus(find::FIND_INPUT_ID);
            }
            Message::Find(FindMessage::Close) => {
                self.find = None;
            }
            Message::Find(FindMessage::QueryChanged(query)) => {
                if let Some(state) = self.find.as_mut() {
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
            Message::Find(FindMessage::Recompute(version)) => {
                if let Some(state) = self.find.as_ref()
                    && state.query_version == version
                {
                    let (matches, error) = find::compute_matches(state, self.find_files());
                    if let Some(state) = self.find.as_mut() {
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
            Message::Find(FindMessage::ToggleCase) => {
                if let Some(state) = self.find.as_mut() {
                    state.case_sensitive = !state.case_sensitive;
                    return self.refind_now();
                }
            }
            Message::Find(FindMessage::ToggleRegex) => {
                if let Some(state) = self.find.as_mut() {
                    state.regex = !state.regex;
                    return self.refind_now();
                }
            }
            Message::Find(FindMessage::Next) => {
                self.find_advance(1);
            }
            Message::Find(FindMessage::Prev) => {
                self.find_advance(-1);
            }

            // ── Toolbar / activity / revset ─────────────────────────────
            Message::ToolbarRefresh => {
                return self.toolbar_refresh();
            }
            Message::Fetch(target) => {
                return self.start_fetch(target);
            }
            Message::FetchCompleted(tab_id, id, target, result) => {
                // "Up to date" when the remote sent nothing back; otherwise
                // name the fetched target (the title may be ellipsized, the
                // subtitle shows it in full).
                let summary = match result.as_ref() {
                    Err(_) => None,
                    Ok(lines) if lines.is_empty() => Some("Fetched · up to date".to_owned()),
                    Ok(_) => Some(match &target {
                        FetchTarget::AllRemotes => "Fetched all remotes".to_owned(),
                        FetchTarget::RemoteBranch { remote, branch } => {
                            format!("Fetched {remote}/{branch}")
                        }
                    }),
                };
                return self.finish_remote_op(tab_id, id, *result, summary);
            }
            Message::Undo => {
                return self.start_undo();
            }
            Message::RevsetChanged(value) => {
                self.session.revset = value;
            }
            Message::RevsetSubmit => {
                return self.evaluate_revset();
            }
            Message::OpenToolbarMenu(which, anchor) => {
                self.activity_popover_open = false;
                return self.open_toolbar_menu(which, anchor);
            }
            Message::Menu(MenuMessage::Hover(path)) => {
                // Hover is `on_enter`-driven (geometry-free, can't mis-hit). On
                // the open branch (or with no flyout open) it commits at once.
                // An off-branch row while a flyout is open is held as *pending*;
                // MenuMouseMoved commits it once the cursor veers out of the
                // trajectory wedge, so the row only opens when the sweep ends.
                let Some(m) = self.menu.as_mut() else {
                    return Task::none();
                };
                m.entered = true;
                if m.open_path.is_empty() || m.on_open_branch(&path) {
                    m.activate(path);
                } else {
                    m.pending_row = Some(path);
                }
            }
            Message::Menu(MenuMessage::MouseMoved(pos)) => {
                if self.menu.is_none() {
                    return Task::none();
                }
                // Off-branch sweep pending: commit the row the moment the cursor
                // leaves the triangle aimed at the flyout (a veer). The apex
                // itself is eased toward the cursor on the menu tick (see
                // `MenuTick`), frozen while a row is pending — so it sits upstream
                // and the wedge has room during a sweep. No timeouts: a big menu
                // can take as long as it likes. A not-yet-set apex (submenu only
                // just opened) holds the row pending rather than stealing it.
                let commit = {
                    let m = self.menu.as_ref().unwrap();
                    m.pending_row.as_ref().map(|_| {
                        match (m.flyout_origin, menu::flyout_rect(self, m)) {
                            (Some(apex), Some(fly)) => !menu::heading_to_flyout(apex, pos, fly),
                            _ => false,
                        }
                    })
                };
                let m = self.menu.as_mut().unwrap();
                m.cursor = Some(pos);
                m.entered = true;
                if commit == Some(true)
                    && let Some(path) = m.pending_row.take()
                {
                    m.activate(path);
                }
            }
            Message::Menu(MenuMessage::Select(path)) => {
                let Some(open) = self.menu.as_ref() else {
                    return Task::none();
                };
                // Only a leaf picks; a release on a submenu/disabled/separator
                // row leaves the (already hover-opened) menu as it is.
                if let Some(menu::MenuEntry::Item { action, .. }) = open.entry_at(&path) {
                    let action = action.clone();
                    let selection = open.selection.clone();
                    self.menu = None;
                    return self.dispatch_menu_action(action, selection);
                }
            }
            Message::Menu(MenuMessage::CapturePress) => {}
            Message::Menu(MenuMessage::Dismiss) => {
                self.menu = None;
            }
            Message::Menu(MenuMessage::ScrimRelease) => {
                if let Some(menu) = self.menu.as_mut() {
                    // The opening left-click's release lands here first; swallow
                    // it (arm) and keep the menu open. A later release — or one
                    // after the cursor has dragged into the menu — dismisses.
                    if menu.armed || menu.entered {
                        self.menu = None;
                    } else {
                        menu.armed = true;
                    }
                }
            }
            // Drives two time-based effects while a menu is open: the right-click
            // glow pulse (re-running `view`), and easing the trajectory apex
            // toward the cursor. Easing on this fixed clock — not on raw moves —
            // is what makes the apex lag by a velocity-proportional amount during
            // a sweep yet catch up when the cursor idles. Frozen while a row is
            // pending so the wedge keeps a stable upstream origin mid-sweep.
            Message::Menu(MenuMessage::Tick) => {
                if let Some(m) = self.menu.as_mut()
                    && m.pending_row.is_none()
                    && let Some(cursor) = m.cursor
                {
                    m.flyout_origin = Some(menu::ease_apex(m.flyout_origin, cursor));
                }
            }
            Message::ActivityToggle => {
                self.activity_popover_open = !self.activity_popover_open;
                self.menu = None;
            }
            Message::ActivityExpand(id) => {
                self.activities.toggle_expand(id);
            }
            Message::ActivityDetailAction(id, action) => {
                // Selection/caret/scroll only — the log drops edit actions,
                // keeping the output buffer read-only.
                self.activities.perform_detail_action(id, action);
            }
            Message::ActivityClear => {
                self.activities.clear_finished();
            }
            Message::ActivityNoOp => {}
            Message::OpenUrl(url) => {
                open_url(&url);
            }
            Message::SetHover(target) => {
                self.hovered = target;
            }

            // ── Source browser ──────────────────────────────────────────
            Message::SetMainView(mode) => {
                if mode != self.main_view {
                    match mode {
                        MainView::Diff => {
                            self.main_view = MainView::Diff;
                            // The shared code widget swaps documents: restore
                            // this view's saved scroll and drop the other
                            // view's shaped-paragraph cache.
                            self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
                            self.document_version = self.document_version.wrapping_add(1);
                        }
                        MainView::Source => {
                            // First open browses the selected revision jumped
                            // to the diff's selected file; later toggles
                            // return to whatever was being browsed.
                            let revision = self
                                .source
                                .revision
                                .clone()
                                .unwrap_or_else(|| self.session.selected_revision.clone());
                            let jump = if self.source.revision.is_none() {
                                self.session
                                    .document
                                    .files
                                    .get(self.selected_file)
                                    .map(|file| file.path.clone())
                            } else {
                                None
                            };
                            return self.open_source_browser(revision, jump);
                        }
                    }
                }
            }
            Message::BrowseFileFromDiff(file_index) => {
                let Some(file) = self.session.document.files.get(file_index) else {
                    return Task::none();
                };
                let revision = self.session.selected_revision.clone();
                let path = file.path.clone();
                return self.open_source_browser(revision, Some(path));
            }
            Message::SourceTreeLoaded(tab, version, result) => {
                let is_active_tab = self.active_tab_id() == Some(tab);
                let Some(state) = self.source_state_for(tab) else {
                    return Task::none();
                };
                if state.version != version {
                    return Task::none();
                }
                match *result {
                    Ok(entries) => {
                        state.tree = Some(entries);
                        state.tree_epoch = state.tree_epoch.wrapping_add(1);
                        state.tree_error = None;
                        // A jump scheduled before the listing arrived had no
                        // row to reveal yet — re-arm it now that the selected
                        // path has one. One-shot, so periodic wc re-lists
                        // don't yank the scroll back to the selection.
                        let rearm = state.reveal_pending && state.selected.is_some();
                        state.reveal_pending = false;
                        if rearm {
                            state.reveal_token = state.reveal_token.wrapping_add(1);
                            if is_active_tab {
                                self.aim_tree_scroll_at_selected();
                            }
                        }
                    }
                    // Keep a previously-listed tree usable under the error
                    // banner (a wc re-list that failed shouldn't blank the
                    // pane).
                    Err(error) => state.tree_error = Some(error),
                }
            }
            Message::SourceFileLoaded(tab, version, path, result) => {
                // Allocated up front — `source_state_for` borrows self.
                let doc_id = self.allocate_document_id();
                let is_active_tab = self.active_tab_id() == Some(tab);
                let Some(state) = self.source_state_for(tab) else {
                    return Task::none();
                };
                // Stale guards: a superseded browse (version) or a selection
                // that moved on (path) drops the result.
                if state.version != version || state.selected.as_deref() != Some(path.as_str()) {
                    return Task::none();
                }
                state.loading = None;
                match *result {
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
            }
            Message::SourceSidebarRow(display_index) => {
                let (entries, rows) = self.source_entries_and_rows();
                match rows.get(display_index).cloned() {
                    Some(diffui_core::SourceTreeRow::Dir { path, unlisted, .. }) => {
                        if unlisted {
                            // Unenumerated ignored dir: expanding it is a
                            // lazy disk listing. Mark it expanded now so the
                            // arriving children render straight away.
                            self.source.expanded.insert(path.clone());
                            return self.kick_ignored_dir_load(path);
                        }
                        if !self.source.expanded.remove(&path) {
                            self.source.expanded.insert(path);
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
            Message::SourceDirLoaded(tab, version, dir, result) => {
                let Some(state) = self.source_state_for(tab) else {
                    return Task::none();
                };
                if state.version != version {
                    return Task::none();
                }
                match *result {
                    Ok(entries) => {
                        state.dir_children.insert(dir, entries);
                        state.tree_epoch = state.tree_epoch.wrapping_add(1);
                    }
                    // Surface a failed readdir in the tree banner; the row
                    // stays unlisted so the click can retry.
                    Err(error) => {
                        state.expanded.remove(&dir);
                        state.tree_error = Some(error);
                    }
                }
            }
            Message::SourceHeaderClicked => {}
            Message::SourceFilterChanged(query) => {
                self.source.filter = query;
                // New result set — jump the list back to the top. The code
                // pane's restore re-applies its own live offset, so only the
                // sidebar actually moves.
                self.source.tree_scroll_offset = 0.0;
                self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
            }
            Message::SourceFilterSubmit => {
                // Open the best match (the ranked list's first row). A no-op
                // when the box is empty — the first tree row is a directory.
                let (entries, rows) = self.source_entries_and_rows();
                if let Some(diffui_core::SourceTreeRow::File { entry_index, .. }) = rows.first()
                    && let Some(path) = entries.get(*entry_index).map(|entry| entry.path.clone())
                {
                    return self.select_source_file(path);
                }
            }
            Message::SourceScrolled(offset) => {
                self.source.scroll_offset = offset;
            }
            Message::SourceTreeScrolled(offset) => {
                self.source.tree_scroll_offset = offset;
            }
            Message::SidebarFileContextMenu(display_index, row_rect, cursor) => {
                return self.open_file_context_menu(display_index, row_rect, cursor);
            }
        }

        // Fall-through chokepoint for every arm that didn't return its own task:
        // if a refresh was coalesced while busy and we're now idle, run it. A
        // no-op when nothing's pending (the common case).
        self.take_pending_refresh()
    }

    /// Open (or re-focus) the source browser at `revision`, optionally jumped
    /// to `jump` — the entry point behind the toolbar switcher, the revision
    /// context menu, file-tree right-clicks, and the diff view's per-file
    /// browse button. Repo tabs only; a PR tab has no tree to browse.
    pub(crate) fn open_source_browser(
        &mut self,
        revision: RevisionSelection,
        jump: Option<String>,
    ) -> Task<Message> {
        if self.session.repository.is_none() {
            return Task::none();
        }
        self.main_view = MainView::Source;
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
        self.document_version = self.document_version.wrapping_add(1);

        let changed = self.source.revision.as_ref() != Some(&revision);
        let is_working_copy = matches!(revision, RevisionSelection::WorkingCopy);
        // Commits are immutable — their listing is reusable as-is. The
        // working copy re-lists on every open so the browser mirrors the
        // disk right now.
        let need_tree = changed
            || is_working_copy
            || self.source.tree.is_none()
            || self.source.tree_error.is_some();

        if changed {
            self.source.revision = Some(revision.clone());
            // The old revision's listing is wrong for the new one; clear so
            // the sidebar shows the loading state instead of stale rows.
            // Expansion state, lazily-listed ignored dirs, and the selected
            // path survive — most paths exist across revisions (and the
            // ignored-dir markers vanish from non-wc listings, hiding the
            // saved children automatically), which is what makes "browse
            // this file at another revision" feel continuous.
            self.source.tree = None;
            self.source.tree_epoch = self.source.tree_epoch.wrapping_add(1);
            self.source.tree_error = None;
            self.source.file_error = None;
        }

        let mut tasks = Vec::new();
        if need_tree {
            self.source.version = self.source.version.wrapping_add(1);
            self.source.tree_error = None;
            tasks.push(self.kick_source_tree_load());
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
                self.source.expanded.insert(prefix.clone());
            }
            if self.source.selected.as_deref() != Some(path.as_str()) {
                self.source.file = None;
                self.source.scroll_offset = 0.0;
            }
            self.source.selected = Some(path);
            self.source.file_error = None;
            self.source.reveal_token = self.source.reveal_token.wrapping_add(1);
            // A jump also bumps the scroll-restore token (the switched-in
            // widget must drop the other view's offset), and a restore
            // scheduled in the same render pass as a reveal wins over it —
            // so the reveal alone can't be trusted to land. When the target
            // row is computable now, aim the restore offset at it; when the
            // listing is still in flight, its arrival re-fires the reveal
            // (which then runs in a restore-free pass).
            self.source.reveal_pending = need_tree;
            if !need_tree {
                self.aim_tree_scroll_at_selected();
            }
        }

        let need_file = self.source.selected.is_some()
            && (changed
                || jumped
                || is_working_copy
                || (self.source.file.is_none() && self.source.file_error.is_none()));
        if need_file {
            tasks.push(self.kick_source_file_load());
        }
        Task::batch(tasks)
    }

    /// Point the source sidebar's saved scroll at the selected file's row
    /// (a bit of context above it), so the next scroll *restore* lands
    /// showing it. Used on jumps, where the same-pass restore outranks the
    /// reveal (see `open_source_browser`).
    fn aim_tree_scroll_at_selected(&mut self) {
        const CONTEXT_ABOVE: f64 = 200.0;
        let Some(selected) = self.source.selected.clone() else {
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
        self.source.tree_scroll_offset = (row_top - CONTEXT_ABOVE).max(0.0);
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
        let selected_entry = self.source.selected.as_deref().and_then(|path| {
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
        self.source.reveal_token = self.source.reveal_token.wrapping_add(1);
        self.select_source_file(path)
    }

    /// Select `path` in the source browser and load its contents (a sidebar
    /// click or keyboard nav). The previous file stays visible until the new
    /// one lands, mirroring how a revision switch keeps the prior diff.
    fn select_source_file(&mut self, path: String) -> Task<Message> {
        let reselect = self.source.selected.as_deref() == Some(path.as_str());
        if reselect && (self.source.file.is_some() || self.source.loading.is_some()) {
            return Task::none();
        }
        if !reselect {
            self.source.scroll_offset = 0.0;
        }
        self.source.selected = Some(path);
        self.source.file_error = None;
        self.kick_source_file_load()
    }

    /// Spawn the tree listing for the browsed revision under the current
    /// browse version.
    fn kick_source_tree_load(&mut self) -> Task<Message> {
        let (Some(repository), Some(tab)) = (self.session.repository.clone(), self.active_tab_id())
        else {
            return Task::none();
        };
        let revision = source_panel::browsed_revision(&self.source);
        let version = self.source.version;
        Task::perform(
            diffui_core::list_source_tree(repository, revision),
            move |result| Message::SourceTreeLoaded(tab, version, Box::new(result)),
        )
    }

    /// Spawn the content load for the browser's selected path.
    fn kick_source_file_load(&mut self) -> Task<Message> {
        let (Some(repository), Some(tab)) = (self.session.repository.clone(), self.active_tab_id())
        else {
            return Task::none();
        };
        let Some(path) = self.source.selected.clone() else {
            return Task::none();
        };
        self.source.loading = Some(path.clone());
        let revision = source_panel::browsed_revision(&self.source);
        let version = self.source.version;
        let result_path = path.clone();
        Task::perform(
            diffui_core::load_source_file(repository, revision, path),
            move |result| {
                Message::SourceFileLoaded(tab, version, result_path.clone(), Box::new(result))
            },
        )
    }

    /// Re-list + re-read the browser when it's showing the working copy —
    /// called wherever the repo refreshes (watcher edits, focus regain, the
    /// post-mutation reload), so the browsed "directory" tracks the disk.
    fn refresh_source_if_working_copy(&mut self) -> Task<Message> {
        if self.main_view != MainView::Source
            || !matches!(self.source.revision, Some(RevisionSelection::WorkingCopy))
        {
            return Task::none();
        }
        self.source.version = self.source.version.wrapping_add(1);
        let mut tasks = vec![self.kick_source_tree_load()];
        if self.source.selected.is_some() {
            tasks.push(self.kick_source_file_load());
        }
        // Previously-expanded ignored dirs re-list too, so an open `target/`
        // exploration tracks the disk like the rest of the tree.
        for dir in self.source.dir_children.keys().cloned().collect::<Vec<_>>() {
            tasks.push(self.kick_ignored_dir_load(dir));
        }
        Task::batch(tasks)
    }

    /// The source-browser state owned by `tab` — inline for the active tab,
    /// stashed otherwise. `None` if the tab has since closed.
    fn source_state_for(&mut self, tab: TabId) -> Option<&mut SourceState> {
        if self.active_tab_id() == Some(tab) {
            return Some(&mut self.source);
        }
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab)
            .and_then(|t| t.stash.as_mut())
            .map(|state| &mut state.source)
    }

    /// The composed source entries + their flattened display rows
    /// (memoized — see [`source_panel::SourceTreeCache`]). Row indices point
    /// into the returned entries, so consumers take them as a pair.
    pub(crate) fn source_entries_and_rows(
        &self,
    ) -> (
        std::rc::Rc<Vec<diffui_core::SourceEntry>>,
        std::rc::Rc<Vec<diffui_core::SourceTreeRow>>,
    ) {
        self.source_tree_cache
            .borrow_mut()
            .entries_and_rows(&self.source)
    }

    /// Spawn the lazy one-level listing of an unenumerated ignored dir.
    fn kick_ignored_dir_load(&mut self, dir: String) -> Task<Message> {
        let (Some(repository), Some(tab)) = (self.session.repository.clone(), self.active_tab_id())
        else {
            return Task::none();
        };
        let version = self.source.version;
        let result_dir = dir.clone();
        Task::perform(
            diffui_core::list_ignored_dir(repository, dir),
            move |result| {
                Message::SourceDirLoaded(tab, version, result_dir.clone(), Box::new(result))
            },
        )
    }

    /// The files the find bar searches: the diff document, or the source
    /// browser's loaded file when that view is active.
    pub(crate) fn find_files(&self) -> &[DiffFile] {
        match self.main_view {
            MainView::Diff => &self.session.document.files,
            MainView::Source => self
                .source
                .file
                .as_ref()
                .map(|view| std::slice::from_ref(&view.file))
                .unwrap_or(&[]),
        }
    }

    /// Recompute find matches immediately (no debounce). Used by toggle
    /// presses where the user's intent is immediate.
    pub(crate) fn refind_now(&mut self) -> Task<Message> {
        if let Some(state) = self.find.as_mut() {
            state.query_version = state.query_version.wrapping_add(1);
        }
        let Some(state) = self.find.as_ref() else {
            return Task::none();
        };
        let (matches, error) = find::compute_matches(state, self.find_files());
        if let Some(state) = self.find.as_mut() {
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
        let Some(state) = self.find.as_mut() else {
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
        let Some(mut state) = self.palette.take() else {
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
                // `self.palette` is `None` here (taken above), so this borrows
                // `self` cleanly while mutating the detached column.
                palette::recompute_matches(column, self, true);
            }
            self.palette = Some(state);
            return widget::operation::scroll_to(
                palette::results_scrollable_id(depth),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
            );
        }
        self.palette = Some(state);
        self.palette_accept_current()
    }

    /// Execute the highlighted result in the rightmost column. Returns the
    /// `Task` chain that performs the corresponding action plus any
    /// followup state (closing the palette, focusing input, etc.).
    pub(crate) fn palette_accept_current(&mut self) -> Task<Message> {
        let Some(state) = self.palette.as_ref() else {
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
            ColumnSource::Actions(t) => Some(t.clone()),
        };

        match (&top.source, &item) {
            // Top-level: command rows run directly; revision/file rows
            // primary-action without going through the Actions column.
            (ColumnSource::Root, ResultRef::Command(cmd)) => {
                self.recents.push_command(*cmd);
                self.recents.save();
                self.palette = None;
                self.run_palette_command(*cmd, None)
            }
            (
                ColumnSource::Root,
                ResultRef::WorkingCopy | ResultRef::Commit(_) | ResultRef::Bookmark(_),
            ) => {
                if let Some(change_id) = change_id_for_recents(&item, self) {
                    self.recents.push_revision(change_id);
                    self.recents.save();
                }
                self.palette = None;
                self.jump_to_revision_ref(&item)
            }
            (ColumnSource::Root, ResultRef::File(path)) => {
                self.palette = None;
                self.jump_to_file_path(path);
                Task::none()
            }
            // Actions column: the row is always a Command — run it against
            // the column's target.
            (ColumnSource::Actions(_), ResultRef::Command(cmd)) => {
                self.recents.push_command(*cmd);
                self.recents.save();
                self.palette = None;
                self.run_palette_command(*cmd, target)
            }
            _ => Task::none(),
        }
    }

    pub(crate) fn run_palette_command(
        &mut self,
        cmd: PaletteCommand,
        target: Option<ResultRef>,
    ) -> Task<Message> {
        match cmd {
            PaletteCommand::RefreshRepository => {
                if self.app_focused {
                    // Manual refresh = full reload (the user may have run an
                    // external jj op since the last load).
                    return self.start_repository_snapshot(RefreshOrigin::Focus);
                }
                Task::none()
            }
            PaletteCommand::SelectNextFile => Task::done(Message::SelectNextFile),
            PaletteCommand::SelectPreviousFile => Task::done(Message::SelectPreviousFile),
            PaletteCommand::ThemeSystem => {
                Task::done(Message::SelectTheme(ThemePreference::System))
            }
            PaletteCommand::ThemeLight => Task::done(Message::SelectTheme(ThemePreference::Light)),
            PaletteCommand::ThemeDark => Task::done(Message::SelectTheme(ThemePreference::Dark)),
            PaletteCommand::ThemeHighContrast => {
                Task::done(Message::SelectTheme(ThemePreference::HighContrast))
            }
            PaletteCommand::CopyFileDiff => {
                if let Some(text) = current_file_diff_text(self) {
                    Task::done(Message::CopyToClipboard(text))
                } else {
                    Task::none()
                }
            }
            PaletteCommand::OpenFind => Task::done(Message::Find(FindMessage::Open)),
            PaletteCommand::JumpToRevision => {
                if let Some(t) = target.as_ref() {
                    if let Some(change_id) = change_id_for_recents(t, self) {
                        self.recents.push_revision(change_id);
                        self.recents.save();
                    }
                    self.jump_to_revision_ref(t)
                } else {
                    Task::none()
                }
            }
            PaletteCommand::CopyChangeId => {
                // Resolve through the unified helper so bookmarks /
                // working-copy / explicit commits all surface their
                // change-id consistently.
                let payload = target.and_then(|t| change_id_for_recents(&t, self));
                payload
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::CopyCommitMessage => {
                let payload = target.and_then(|t| commit_for_ref(self, &t)).map(|c| {
                    if c.has_description() {
                        c.description().to_owned()
                    } else {
                        String::new()
                    }
                });
                payload
                    .filter(|s| !s.is_empty())
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::CopyAuthor => {
                let payload = target
                    .and_then(|t| commit_for_ref(self, &t))
                    .map(|c| c.author().to_owned());
                payload
                    .filter(|s| !s.is_empty())
                    .map(|t| Task::done(Message::CopyToClipboard(t)))
                    .unwrap_or_else(Task::none)
            }
            PaletteCommand::OpenFile => {
                if let Some(ResultRef::File(path)) = target.as_ref() {
                    self.jump_to_file_path(path);
                }
                Task::none()
            }
            PaletteCommand::CopyFilePath => {
                if let Some(ResultRef::File(path)) = target.as_ref() {
                    Task::done(Message::CopyToClipboard(path.clone()))
                } else {
                    Task::none()
                }
            }
        }
    }

    pub(crate) fn jump_to_revision_ref(&mut self, target: &ResultRef) -> Task<Message> {
        let Some(selection) = revision_selection(target, self) else {
            return Task::none();
        };

        // Already current — no load, no async wait, bump the token now so
        // the next render scrolls the sidebar row into view.
        if self.session.selected_revision == selection {
            self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
            return Task::none();
        }

        // A load is already in flight for the same revision; piggyback so
        // the eventual `BackendLoaded` bumps the token for us.
        if self.session.pending_revision.as_ref() == Some(&selection) {
            self.pending_revision_reveal = true;
            return Task::none();
        }

        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        // PR tabs switch through the cache-aware switcher (parked documents,
        // live-stream guard) — a palette jump lands on its commits too.
        if self.active_pr_spec().is_some() {
            self.pending_revision_reveal = true;
            return self.select_pr_revision(tab, selection);
        }
        let Some(source) = self.session.source.clone() else {
            return Task::none();
        };
        self.session.pending_revision = Some(selection.clone());
        self.session.loading_since = Some(Instant::now());
        // Deferred bump — see comment on `pending_revision_reveal`.
        self.pending_revision_reveal = true;
        let revision = selection.clone();
        Task::perform(source.diff(selection), move |result| {
            Message::DiffLoaded(tab, revision, Box::new(result))
        })
    }

    pub(crate) fn jump_to_file_path(&mut self, path: &str) {
        if let Some(index) = self
            .session
            .document
            .files
            .iter()
            .position(|f| f.path == path)
        {
            self.selected_file = index;
            self.reveal_selected_file_in_tree();
        }
    }

    /// Expand any collapsed ancestors of the selected file so its tree row
    /// is visible (selection moved by j/k, the palette, or the diff scroll
    /// spy may land inside a collapsed directory).
    pub(crate) fn reveal_selected_file_in_tree(&mut self) {
        if self.collapsed_dirs.is_empty() {
            return;
        }
        let Some(file) = self.session.document.files.get(self.selected_file) else {
            return;
        };
        let path = file.path.clone();
        self.collapsed_dirs
            .retain(|dir| !path.starts_with(&format!("{dir}/")));
    }

    /// Refresh the working-copy (`@`) row's "empty" chip from a snapshot's
    /// `working_copy_empty`, without touching the diff pane or re-walking the
    /// graph. A no-op when the value is unknown (git), @ isn't in the loaded
    /// graph, or the chip already matches — so it won't needlessly bump
    /// `commits_version` (which would invalidate the sidebar's shaped-row cache).
    pub(crate) fn apply_working_copy_empty(&mut self, empty: Option<bool>) {
        let Some(empty) = empty else { return };
        let Some(index) = self.session.commits.working_copy_index() else {
            return;
        };
        if self.session.commits.row(index).is_empty() == Some(empty) {
            return;
        }
        self.session.commits.set_is_empty(index, empty);
        self.session.commits_version = self.session.commits_version.wrapping_add(1);
    }

    pub(crate) fn start_repository_snapshot(&mut self, origin: RefreshOrigin) -> Task<Message> {
        // Every refresh path funnels through here (watcher edits, focus
        // regain, tab activation, the post-mutation reload), so it's also
        // where a working-copy source browse re-syncs with the disk. Its
        // loads are read-only and version-guarded, so they ride alongside
        // the snapshot without contending for jj's wc lock.
        let source_refresh = self.refresh_source_if_working_copy();
        Task::batch([source_refresh, self.start_snapshot_inner(origin)])
    }

    fn start_snapshot_inner(&mut self, origin: RefreshOrigin) -> Task<Message> {
        // A snapshot or reload is already in flight (the snapshot phase, a cold
        // stream still appending, or a graph walk holding `pending_revision`).
        // Coalesce rather than race it: a second snapshot thrashes the wc lock,
        // and a snapshot landing mid-walk is dropped by `RepositorySnapshotLoaded`
        // anyway. The in-flight load's terminal kicks the coalesced refresh via
        // `take_pending_refresh`, so a change that lands mid-reload isn't lost.
        if self.session.snapshot_pending
            || self.session.load.is_some()
            || self.session.pending_revision.is_some()
        {
            self.session.pending_refresh =
                Some(coalesce_refresh(self.session.pending_refresh, origin));
            return Task::none();
        }

        // Only graph-backed sources have a working copy to snapshot; a PR
        // tab's `source` would just error.
        if self.session.repository.is_none() {
            return Task::none();
        }
        let Some(source) = self.session.source.clone() else {
            return Task::none();
        };
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };

        self.session.snapshot_pending = true;
        Task::perform(source.snapshot(), move |result| {
            Message::RepositorySnapshotLoaded(tab, origin, result)
        })
    }

    /// Run a refresh coalesced while the app was busy, now that it's idle. The
    /// idle check mirrors `start_repository_snapshot`'s coalesce condition, and
    /// taking the origin before dispatching keeps a no-op snapshot (op
    /// unchanged) from re-arming itself into a loop.
    pub(crate) fn take_pending_refresh(&mut self) -> Task<Message> {
        if self.session.snapshot_pending
            || self.session.load.is_some()
            || self.session.pending_revision.is_some()
        {
            return Task::none();
        }
        match self.session.pending_refresh.take() {
            Some(origin) => self.start_repository_snapshot(origin),
            None => Task::none(),
        }
    }

    /// Resolve the merge/root commits the loader left with unknown empty
    /// status: apply any cached results immediately, and spawn a background
    /// task for the rest (single-parent commits were already decided cheaply
    /// during load).
    pub(crate) fn resolve_empty_status(&mut self) -> Task<Message> {
        let Some(source) = self.session.source.clone() else {
            return Task::none();
        };

        // Each background resolution is an ~8ms parent-tree merge, so on a repo
        // with hundreds of thousands of merge commits (nixpkgs) computing them
        // all would burn tens of minutes of CPU. Cap how many we resolve per
        // load; beyond that, merges simply keep no "empty" chip. Cached results
        // still apply to every row, so this only bounds *new* work. The gather +
        // cache-apply is core engine logic (`Session::take_empty_status_targets`);
        // here we just spawn the async resolution for what's left.
        const EMPTY_STATUS_LIMIT: usize = 5_000;
        let targets = self.session.take_empty_status_targets(EMPTY_STATUS_LIMIT);
        if targets.is_empty() {
            return Task::none();
        }

        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let version = self.session.commits_version;
        Task::perform(source.empty_status(targets), move |updates| {
            Message::EmptyStatusComputed(tab, version, updates)
        })
    }

    /// Stable-sort `items` in place so each item's target commit (via
    /// `target_of`) sits nearest-first to `reference` in the loaded log. Items
    /// whose target — or the reference — isn't loaded sink to the bottom, where
    /// the caller's prior ordering (e.g. an alphabetical pre-sort) breaks ties.
    pub(crate) fn sort_by_proximity<T>(
        &self,
        items: &mut [T],
        reference: Option<&str>,
        target_of: impl Fn(&T) -> &str,
    ) {
        let index_of = self
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
        let reference = self.session.bookmarks.working_copy_commit.clone();
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
    pub(crate) fn kick_initial_load(&mut self) -> Task<Message> {
        self.kick_load(None)
    }

    /// Replace the displayed diff, bumping [`document_version`](Self::document_version)
    /// so the diff view drops its per-line shaped-paragraph cache, and
    /// stamping a fresh document id (which restarts highlight bookkeeping —
    /// see [`Session::reset_highlights`]). Every write to
    /// `self.session.document` must go through here — a missed one leaves the
    /// diff view rendering another revision's (or repo's) stale highlighted
    /// text. (Routed load completions write their owner's session directly
    /// instead, with the same id + reset discipline; a stashed tab's restore
    /// bumps the paint version.)
    pub(crate) fn set_document(&mut self, document: DiffDocument) {
        self.session.document = document;
        self.document_version = self.document_version.wrapping_add(1);
        let document_id = self.allocate_document_id();
        self.session.reset_highlights(document_id);
    }

    /// Hand out the next document identity (monotonic across every tab).
    pub(crate) fn allocate_document_id(&mut self) -> u64 {
        self.next_document_id = self.next_document_id.wrapping_add(1);
        self.next_document_id
    }

    /// The session displaying document `id` — active or stashed — plus
    /// whether it's the active one. Background per-file results (syntax
    /// highlights) route through this; `None` means the document is gone and
    /// the result must be dropped.
    pub(crate) fn session_by_document_mut(&mut self, id: u64) -> Option<(&mut Session, bool)> {
        if self.session.document_id == id {
            return Some((&mut self.session, true));
        }
        self.tabs.iter_mut().find_map(|tab| {
            let stash = tab.stash.as_mut()?;
            (stash.session.document_id == id).then_some((&mut stash.session, false))
        })
    }

    /// Drain the highlight queue of document `id`, keeping a couple of jobs
    /// in flight. Each job clones one file (memory bounded by the concurrency
    /// window, not the document) and reports back as `FileHighlighted` — for
    /// a backgrounded tab too, so highlighting keeps progressing off-screen.
    pub(crate) fn spawn_highlights(&mut self, document_id: u64) -> Task<Message> {
        const HIGHLIGHT_CONCURRENCY: usize = 2;
        let Some((session, _)) = self.session_by_document_mut(document_id) else {
            return Task::none();
        };
        let repository = session.repository.clone();
        let revision = session.selected_revision.clone();
        let mut tasks = Vec::new();
        while session.highlight_in_flight < HIGHLIGHT_CONCURRENCY {
            let Some(file_index) = session.highlight_pending.pop_front() else {
                break;
            };
            let Some(file) = session.document.files.get(file_index).cloned() else {
                continue;
            };
            session.highlight_in_flight += 1;
            let repository = repository.clone();
            let revision = revision.clone();
            tasks.push(Task::perform(
                diffui_core::highlight_file(repository, revision, file),
                move |spans| Message::FileHighlighted(document_id, file_index, spans),
            ));
        }
        Task::batch(tasks)
    }

    /// Find the tab whose live streaming cursor is `version` — the active one
    /// or a backgrounded stash. Streaming results are routed here instead of
    /// dropped, so a cold walk / PR stream keeps its progress while its tab
    /// is backgrounded. `None` means the load was superseded (its tab
    /// re-kicked, or closed) and the result must be discarded.
    pub(crate) fn load_target_mut(&mut self, version: u64) -> Option<LoadTargetMut<'_>> {
        if self.session.load.as_ref().map(|c| c.version) == Some(version) {
            return Some(LoadTargetMut {
                session: &mut self.session,
                selected_file: &mut self.selected_file,
                activities: &mut self.activities,
                pending_load_activity: &mut self.pending_load_activity,
                pending_revision_reveal: &mut self.pending_revision_reveal,
                revision_reveal_token: &mut self.revision_reveal_token,
                is_active: true,
            });
        }
        self.tabs.iter_mut().find_map(|tab| {
            let stash = tab.stash.as_mut()?;
            if stash.session.load.as_ref().map(|c| c.version) != Some(version) {
                return None;
            }
            Some(LoadTargetMut {
                session: &mut stash.session,
                selected_file: &mut stash.selected_file,
                activities: &mut stash.activities,
                pending_load_activity: &mut stash.pending_load_activity,
                pending_revision_reveal: &mut stash.pending_revision_reveal,
                revision_reveal_token: &mut stash.revision_reveal_token,
                is_active: false,
            })
        })
    }

    /// Like [`load_target_mut`], but resolved by tab id — for one-shot
    /// completions (graph reloads, diff switches), which are tab-addressed
    /// rather than stream-versioned. `None` when the tab has since closed.
    pub(crate) fn tab_target_mut(&mut self, tab: TabId) -> Option<LoadTargetMut<'_>> {
        if self.active_tab_id() == Some(tab) {
            return Some(LoadTargetMut {
                session: &mut self.session,
                selected_file: &mut self.selected_file,
                activities: &mut self.activities,
                pending_load_activity: &mut self.pending_load_activity,
                pending_revision_reveal: &mut self.pending_revision_reveal,
                revision_reveal_token: &mut self.revision_reveal_token,
                is_active: true,
            });
        }
        let stash = self
            .tabs
            .iter_mut()
            .find(|candidate| candidate.id == tab)?
            .stash
            .as_mut()?;
        Some(LoadTargetMut {
            session: &mut stash.session,
            selected_file: &mut stash.selected_file,
            activities: &mut stash.activities,
            pending_load_activity: &mut stash.pending_load_activity,
            pending_revision_reveal: &mut stash.pending_revision_reveal,
            revision_reveal_token: &mut stash.revision_reveal_token,
            is_active: false,
        })
    }

    /// (Re)start the streaming load for a GitHub-PR tab: reset the per-tab
    /// view state, then run `gh pr view` + `gh pr diff` concurrently (see
    /// [`stream_github_pr_load`]). The `session.load` cursor carries the
    /// version guard exactly like a jj cold stream; the commit graph stays
    /// empty (a PR tab has no revision sidebar yet).
    pub(crate) fn kick_pr_load(&mut self, spec: github::PrSpec) -> Task<Message> {
        self.finish_load_activity(activity::ActivityStatus::Done, None);
        self.session.status = LoadStatus::Loading;
        self.session.loading_since = Some(Instant::now());
        self.session.selected_revision = RevisionSelection::WorkingCopy;
        self.session.pending_revision = None;
        self.pending_revision_reveal = false;
        self.selected_file = 0;
        self.sidebar_scroll_offset = 0.0;
        self.diff_scroll_offset = 0.0;
        self.set_document(DiffDocument::default());
        self.session.authoritative_totals = None;
        self.session.commits = CommitStore::default();
        self.session.graph = graph_layout::GraphLayout::default();
        self.session.sidebar_prefix_lens.clear();
        self.session.selected_commit_index = None;
        self.session.revision_details = None;
        self.session.repository_snapshot = None;
        self.session.snapshot_pending = false;
        self.session.pending_refresh = None;
        let label = format!("Load {}/{}", spec.owner, spec.label());
        let (activity_id, progress) = self.begin_activity(label, true);
        self.pending_load_activity = Some(activity_id);
        self.session.commit_progress = progress.clone();
        let version = self.allocate_load_version();
        self.session.commits_version = version;
        self.session.load = Some(diffui_core::session::ColdCursor::new(version));
        self.session.pr_diffs.clear();
        stream_github_pr_load(spec, progress, version)
    }

    /// Switch a PR tab's view between "All changes" (`WorkingCopy`) and one of
    /// its commits. Documents the tab has already shown swap back in from
    /// [`Session::pr_diffs`] without a re-download; unseen commits fetch
    /// through the commits REST endpoint and land as a `DiffLoaded`.
    pub(crate) fn select_pr_revision(
        &mut self,
        tab: TabId,
        selection: RevisionSelection,
    ) -> Task<Message> {
        // A live stream still appends into the displayed document — switching
        // it out from under the batches would scatter PR files into a commit
        // diff. The sidebar unlocks once the stream finishes.
        if self.session.load.is_some() {
            return Task::none();
        }

        let key = match &selection {
            RevisionSelection::WorkingCopy => String::new(),
            RevisionSelection::Commit(oid) => oid.clone(),
        };
        if let Some(cached) = self.session.pr_diffs.remove(&key) {
            // Park the outgoing document, then move the cached one in — two
            // moves, no clone, no network.
            let outgoing_key = match &self.session.selected_revision {
                RevisionSelection::WorkingCopy => String::new(),
                RevisionSelection::Commit(oid) => oid.clone(),
            };
            let outgoing = diffui_core::session::CachedDiff {
                document: std::mem::take(&mut self.session.document),
                totals: self.session.authoritative_totals.take(),
                details: self.session.revision_details.take(),
            };
            self.session.pr_diffs.insert(outgoing_key, outgoing);
            self.set_document(cached.document);
            self.session.authoritative_totals = cached.totals;
            self.session.revision_details = cached.details;
            self.session.selected_revision = selection;
            self.session.selected_commit_index = self.session.find_selected_commit_index();
            self.selected_file = 0;
            // An instant swap never lands a `DiffLoaded`, so honor a deferred
            // reveal (palette jump) here.
            if self.pending_revision_reveal {
                self.pending_revision_reveal = false;
                self.revision_reveal_token = self.revision_reveal_token.wrapping_add(1);
            }
            // The parked document kept the spans it had earned; resume
            // highlighting whatever was still pending when it was parked.
            return self.spawn_highlights(self.session.document_id);
        }

        let Some(source) = self.session.source.clone() else {
            return Task::none();
        };
        self.session.pending_revision = Some(selection.clone());
        self.session.loading_since = Some(Instant::now());
        let revision = selection.clone();
        Task::perform(source.diff(selection), move |result| {
            Message::DiffLoaded(tab, revision, Box::new(result))
        })
    }

    /// As [`kick_initial_load`], with an optional activity label (defaults to
    /// "Load <repo>"). This is the **streaming cold load** — it clears the graph
    /// and regrows it as batches arrive, for the initial open / tab activation
    /// where there's nothing on screen to preserve. A revset switch instead uses
    /// the atomic-swap load in [`evaluate_revset`] to avoid flashing.
    pub(crate) fn kick_load(&mut self, activity_label: Option<String>) -> Task<Message> {
        let Some(repository) = self.session.repository.clone() else {
            return Task::none();
        };
        // A previous load's activity is being superseded by this (re)load —
        // resolve it so it doesn't spin forever.
        self.finish_load_activity(activity::ActivityStatus::Done, None);
        self.session.status = LoadStatus::Loading;
        self.session.loading_since = Some(Instant::now());
        self.session.selected_revision = RevisionSelection::WorkingCopy;
        self.session.pending_revision = Some(RevisionSelection::WorkingCopy);
        self.pending_revision_reveal = false;
        self.selected_file = 0;
        // The cold load clears the graph + diff, so both views belong at the
        // top. Keep the mirrors in step with the cleared content; the widgets
        // restore from these on the next activation.
        self.sidebar_scroll_offset = 0.0;
        self.diff_scroll_offset = 0.0;
        self.set_document(DiffDocument::default());
        self.session.authoritative_totals = None;
        self.session.commits = CommitStore::default();
        self.session.graph = graph_layout::GraphLayout::default();
        self.session.sidebar_prefix_lens.clear();
        self.session.selected_commit_index = None;
        self.session.repository_snapshot = None;
        self.session.snapshot_pending = false;
        // A full cold reload supersedes any coalesced refresh for the old state.
        self.session.pending_refresh = None;
        // Wrap the load in a determinate activity; its progress handle is what
        // the loader bumps, so the toolbar progress line + popover track it.
        let label =
            activity_label.unwrap_or_else(|| format!("Load {}", repo_label(&repository.root).1));
        let (activity_id, progress) = self.begin_activity(label, true);
        self.pending_load_activity = Some(activity_id);
        self.session.commit_progress = progress.clone();
        // Fresh version so a backgrounded load's late batches are dropped.
        let version = self.allocate_load_version();
        self.session.commits_version = version;
        let revset = self.session.revset.clone();
        match repository.vcs {
            Vcs::Jj => {
                self.session.load = Some(diffui_core::session::ColdCursor::new(version));
                stream_jj_initial_load(repository, revset, progress, version)
            }
            Vcs::Git => {
                self.session.load = None;
                let revision = RevisionSelection::WorkingCopy;
                let Some(tab) = self.active_tab_id() else {
                    return Task::none();
                };
                let Some(source) = self.session.source.clone() else {
                    return Task::none();
                };
                Task::perform(
                    source.load(revision.clone(), revset, progress),
                    move |result| Message::BackendLoaded(tab, revision, Box::new(result)),
                )
            }
        }
    }

    /// Hand out the next streaming-load version. Monotonic across every tab
    /// and reload, so a backgrounded load's late batches never collide with
    /// the active tab's cursor.
    pub(crate) fn allocate_load_version(&mut self) -> u64 {
        self.next_load_version = self.next_load_version.wrapping_add(1);
        self.next_load_version
    }

    /// Id of the active tab, or `None` when no tabs are open. Per-tab async
    /// completions carry this so a result that lands after a tab switch is
    /// dropped instead of applying to whichever tab is active by then.
    pub(crate) fn active_tab_id(&self) -> Option<TabId> {
        self.tabs.get(self.active_tab).map(|tab| tab.id)
    }

    /// Hand out the next activity id (monotonic across every tab).
    pub(crate) fn allocate_activity_id(&mut self) -> activity::ActivityId {
        let id = activity::ActivityId(self.next_activity_id);
        self.next_activity_id = self.next_activity_id.wrapping_add(1);
        id
    }

    /// Start an activity on the active tab's log, returning its id and the
    /// progress handle the worker reports through.
    pub(crate) fn begin_activity(
        &mut self,
        label: impl Into<String>,
        determinate: bool,
    ) -> (activity::ActivityId, LoadProgress) {
        let id = self.allocate_activity_id();
        let progress = self.activities.start(id, label, determinate);
        (id, progress)
    }

    /// Whether one of our own mutations is running or waiting to. Background
    /// snapshots check this so the watcher doesn't fire a working-copy snapshot
    /// into the middle of a mutation (both take jj's wc lock); the post-batch
    /// reload in [`advance_mutation_queue`] catches anything missed meanwhile.
    pub(crate) fn mutation_busy(&self) -> bool {
        self.mutation_queue.is_busy()
    }

    /// Whether `target` resolves to one of the active draft's source commits.
    /// Resolved through the loaded graph so a [`RevisionSelection::WorkingCopy`]
    /// naming a source commit is caught too — the core-side `op_for` guard
    /// only inspects the `Commit` variant.
    pub(crate) fn draft_blocks_target(&self, target: &RevisionSelection) -> bool {
        let Some(ui) = self.op_draft.as_ref() else {
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
        let row = match selection {
            RevisionSelection::WorkingCopy => self.session.commits.working_copy(),
            RevisionSelection::Commit(id) => self.session.commits.find_by_commit_id(id),
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
        if index >= self.session.commits.len() {
            return None;
        }
        let row = self.session.commits.row(index);
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
        let Some(ui) = self.op_draft.as_ref() else {
            return Task::none();
        };
        let placement = placement_override.unwrap_or(ui.draft.placement);
        let Some(op) = ui.draft.op_for(target, placement) else {
            return Task::none();
        };
        self.op_draft = None;
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
            .op_draft
            .as_ref()
            .and_then(|ui| ui.draft.candidate)
            .and_then(|index| self.selection_at_index(index))
            .is_some_and(|candidate| candidate == selection);
        let Some(ui) = self.op_draft.as_mut() else {
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
        if self.session.repository.is_none() {
            return Task::none();
        }
        // Destination from the drag spot when one is live, else the keyboard
        // candidate + placement. Resolved before borrowing the draft mutably.
        let (spot, candidate, placement) = match self.op_draft.as_ref() {
            Some(ui) => (ui.hover_spot, ui.draft.candidate, ui.draft.placement),
            None => return Task::none(),
        };
        let destination = match spot {
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
        };
        // A destination anchored on a source is invalid — resolved through
        // the graph (so a WorkingCopy anchor naming a source is caught) and
        // before the mutable draft borrow below.
        let anchor_blocked = destination
            .as_ref()
            .is_some_and(|destination| self.draft_blocks_target(destination.anchor()));
        let Some(ui) = self.op_draft.as_mut() else {
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
        let sources: Vec<RevisionSelection> = ui
            .draft
            .sources
            .iter()
            .map(|s| s.selection.clone())
            .collect();
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
            Message::DraftPreviewKick(version)
        })
    }

    /// Run a mutation now, or queue it behind one already in flight. Two of our
    /// mutations running at once would contend on jj's working-copy lock and
    /// serialize opaquely; queuing keeps them in order and visible (the queued
    /// entry shows in the activity log until it starts).
    pub(crate) fn enqueue_or_run_mutation(&mut self, pending: PendingMutation) -> Task<Message> {
        let (tab_id, activity_id) = (pending.tab_id, pending.activity_id);
        match self.mutation_queue.enqueue(pending) {
            diffui_core::QueueAction::Queued => {
                if let Some(log) = self.activity_log_for(tab_id) {
                    log.set_status(activity_id, activity::ActivityStatus::Queued);
                }
                Task::none()
            }
            diffui_core::QueueAction::Run(pending) => self.run_mutation_now(pending),
        }
    }

    /// Dispatch `pending` off the runtime and mark a mutation in flight. The
    /// status flip to `Running` is idempotent — harmless for the first run,
    /// and what un-queues an entry pulled off the queue by
    /// [`advance_mutation_queue`].
    pub(crate) fn run_mutation_now(&mut self, pending: PendingMutation) -> Task<Message> {
        if let Some(log) = self.activity_log_for(pending.tab_id) {
            log.set_status(pending.activity_id, activity::ActivityStatus::Running);
        }
        let PendingMutation {
            repository,
            op,
            tab_id,
            activity_id,
            progress,
        } = pending;
        Task::perform(
            mutations::run_mutation(repository, op, progress),
            move |result| Message::MutationCompleted(tab_id, activity_id, Box::new(result)),
        )
    }

    /// A mutation finished: start the next queued one, or — when the queue is
    /// empty — clear the in-flight flag and reload once so the graph reflects
    /// the whole batch. Called on success *and* failure, so a failed mutation
    /// never strands the ones queued behind it. (A reload when nothing actually
    /// changed is a no-op: the snapshot fingerprint compares equal.)
    pub(crate) fn advance_mutation_queue(&mut self) -> Task<Message> {
        match self.mutation_queue.advance() {
            Some(next) => self.run_mutation_now(next),
            None => self.start_repository_snapshot(RefreshOrigin::Focus),
        }
    }

    /// The activity log for `tab_id`: the inline one when it's the active tab,
    /// else the matching stash. `None` if the tab has since closed.
    pub(crate) fn activity_log_for(&mut self, tab_id: TabId) -> Option<&mut activity::ActivityLog> {
        if self.active_tab_id() == Some(tab_id) {
            return Some(&mut self.activities);
        }
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.stash.as_mut())
            .map(|state| &mut state.activities)
    }

    /// Finish a remote op's activity (fetch / undo) on its tab, recording the
    /// captured output or error — `success_result` becomes the row's subtitle
    /// summary on success — then, if it's still the active tab, reload so the
    /// new commits/state appear.
    pub(crate) fn finish_remote_op(
        &mut self,
        tab_id: TabId,
        id: activity::ActivityId,
        result: Result<Vec<String>, String>,
        success_result: Option<String>,
    ) -> Task<Message> {
        let ok = result.is_ok();
        let mut failure: Option<(String, String)> = None;
        if let Some(log) = self.activity_log_for(tab_id) {
            match result {
                Ok(lines) => {
                    log.extend_output(id, lines);
                    log.finish(id, activity::ActivityStatus::Done, success_result);
                }
                Err(error) => {
                    let title = log
                        .label(id)
                        .map(|label| format!("{label} failed"))
                        .unwrap_or_else(|| "Operation failed".to_owned());
                    failure = Some((title, error.clone()));
                    log.append_output(id, error.clone());
                    log.finish(id, activity::ActivityStatus::Error, Some(error));
                }
            }
        }
        if let Some((title, error)) = failure {
            self.push_error_toast(title, &error);
        }
        if ok && self.active_tab_id() == Some(tab_id) {
            self.start_repository_snapshot(RefreshOrigin::Focus)
        } else {
            Task::none()
        }
    }

    /// Finish the activity that wraps the in-flight graph (re)load, if one is
    /// tracked for this tab. Called from the terminal load handlers.
    pub(crate) fn finish_load_activity(
        &mut self,
        status: activity::ActivityStatus,
        result: Option<String>,
    ) {
        if let Some(id) = self.pending_load_activity.take() {
            self.activities.finish(id, status, result);
        }
    }

    /// Toolbar "Refresh": a full reload (working-copy snapshot + graph re-walk).
    /// No-op while a load is already in flight. The walk itself is surfaced as an
    /// activity by the `Focus` reload path (so every reload trigger logs it the
    /// same way); an up-to-date refresh that finds nothing changed is silent.
    pub(crate) fn toolbar_refresh(&mut self) -> Task<Message> {
        if self.session.repository.is_none()
            || self.session.snapshot_pending
            || self.session.load.is_some()
        {
            return Task::none();
        }
        self.start_repository_snapshot(RefreshOrigin::Focus)
    }

    /// Toolbar "Fetch": fetch the given target (all remotes / one branch),
    /// surfaced as an activity whose expanded output shows the remote messages.
    /// On success `finish_remote_op` reloads so new commits appear.
    pub(crate) fn start_fetch(&mut self, target: FetchTarget) -> Task<Message> {
        // Capability-gated: graph-less sources (PR tabs) have nothing to
        // fetch from, so the toolbar action stays a silent no-op there.
        let Some(source) = self
            .session
            .source
            .clone()
            .filter(|source| source.as_revision_graph().is_some())
        else {
            return Task::none();
        };
        let Some(tab_id) = self.active_tab_id() else {
            return Task::none();
        };
        self.menu = None;
        let label = match &target {
            FetchTarget::AllRemotes => "Fetching all remotes".to_owned(),
            FetchTarget::RemoteBranch { remote, branch } => format!("Fetching {remote}/{branch}"),
        };
        let (id, progress) = self.begin_activity(label, true);
        let fetched = target.clone();
        Task::perform(source.fetch(target, progress), move |result| {
            Message::FetchCompleted(tab_id, id, fetched.clone(), Box::new(result))
        })
    }

    /// Toolbar "Undo": revert the latest meaningful operation. Capability-gated
    /// (jj repos only today) and routed through the mutation queue like every
    /// other mutation, so it serializes behind in-flight ops and gets the
    /// snapshot-before-mutate discipline.
    pub(crate) fn start_undo(&mut self) -> Task<Message> {
        let Some(source) = self.session.source.clone() else {
            return Task::none();
        };
        if source.as_mutable().is_none() {
            return Task::none();
        }
        self.start_mutation_op(mutations::MutationOp::Undo { operation_id: None })
    }

    /// Re-evaluate the log against the current `self.session.revset` (Enter in the
    /// revset input, or a preset pick), surfaced as an activity, persisting the
    /// filter for this repo.
    ///
    /// Uses the **atomic-swap** load (`load_backend` → `BackendLoaded`) rather
    /// than the streaming cold load: the current graph/diff stay on screen the
    /// whole time and are replaced in one shot when the new walk is ready, so
    /// switching revsets doesn't flash an empty sidebar. The selection is kept
    /// (it just won't be highlighted if it falls outside the new set).
    /// The revset preset menu entries as `(label, expression)`: the active
    /// repo's "Default" (its `revsets.log`) first when there is one, then the
    /// built-in presets. Owned so the dynamic default can sit alongside the
    /// `'static` presets, and shared by the iced and native menus so they stay
    /// in sync. The default is read from the [`Self::default_revset`] cache, so
    /// building the menu never touches the config files.
    pub(crate) fn revset_menu_entries(&self) -> Vec<(String, String)> {
        let presets = revset_presets(self.session.repository.as_ref().map(|r| r.vcs));
        let mut entries = Vec::with_capacity(presets.len() + 1);
        if !self.default_revset.is_empty() {
            entries.push(("Default".to_owned(), self.default_revset.clone()));
        }
        entries.extend(
            presets
                .iter()
                .map(|(label, expr)| ((*label).to_owned(), (*expr).to_owned())),
        );
        entries
    }

    pub(crate) fn evaluate_revset(&mut self) -> Task<Message> {
        // Silent no-op for graph-less sources (a PR tab has no revset),
        // matching the capability rather than erroring through the load.
        let Some(source) = self
            .session
            .source
            .clone()
            .filter(|source| source.as_revision_graph().is_some())
        else {
            return Task::none();
        };
        self.menu = None;
        // Persist the new filter (debounced) for this repo.
        self.mark_geometry_dirty();
        // Supersede any prior in-flight load (cold stream or a previous eval) so
        // its results/activity don't linger.
        self.finish_load_activity(activity::ActivityStatus::Done, None);
        self.session.load = None;

        let shown = self.session.revset.trim();
        let label = if shown.is_empty() {
            "Evaluate revset: all()".to_owned()
        } else {
            format!("Evaluate revset: {shown}")
        };
        let (id, progress) = self.begin_activity(label, true);
        self.pending_load_activity = Some(id);
        self.session.commit_progress = progress.clone();

        // Keep the current view; only `pending_revision` is set, which lights
        // the toolbar progress line. `BackendLoaded` swaps the graph atomically.
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let revision = self.session.selected_revision.clone();
        self.session.pending_revision = Some(revision.clone());
        self.session.loading_since = Some(Instant::now());
        let revset = self.session.revset.clone();
        Task::perform(
            source.load(revision.clone(), revset, progress),
            move |result| Message::BackendLoaded(tab, revision, Box::new(result)),
        )
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
        let (sidebar, diff_pane) = match self.main_view {
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
            Message::SidebarWidthChanged,
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
        // Three-track keyboard handling:
        //   * global: owns ⌘K / ⌘F (overlay entry) and j/k/arrow file nav
        //     when nothing is open
        //   * palette track: ↑/↓/Tab/Esc when the palette is open
        //   * find track: Enter/Shift+Enter/Esc when the find bar is open
        // The text input still consumes character keys when focused, so
        // typing inside an overlay never falls through to file nav.
        // We *must* see Esc even when an iced text_input is focused —
        // text_input captures Esc to clear focus, and `keyboard::listen()`
        // only fires for `Status::Ignored` events, so we'd lose Esc to
        // the input and force the user to press Esc twice (once to
        // unfocus, once to close). `event::listen_with` ignores the
        // capture status and gives us every event, so the palette / find
        // overlays close on the first Esc regardless of focus.
        //
        // Subscription closures must be non-capturing, so we hand the
        // open/closed flags in through `Subscription::with`, which
        // becomes part of the subscription identity and arrives as a
        // tuple alongside each event.
        let flags = (
            self.palette.is_some(),
            self.find.is_some(),
            self.open_repo_dialog.is_some(),
            self.menu.is_some(),
            self.activity_popover_open,
            self.confirm.is_some(),
            self.description_editor.is_some(),
            self.op_draft.is_some(),
        );

        let keyboard = event::listen_with(|event, status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                // `ignored` = no focused widget consumed it. The revset input is
                // inline (not behind an overlay flag), so we use this to keep its
                // keystrokes from leaking into the global j/k file nav.
                Some((key, modifiers, matches!(status, event::Status::Ignored)))
            }
            _ => None,
        })
        .with(flags)
        .filter_map(
            |(
                (
                    palette_open,
                    find_open,
                    dialog_open,
                    menu_open,
                    popover_open,
                    confirm_open,
                    description_editor_open,
                    draft_open,
                ),
                (key, modifiers, ignored),
            )| {
                // A confirmation dialog owns the keyboard: Esc cancels (there
                // is deliberately no Enter-accept — the confirm gates a
                // mutation jj itself refuses), everything else is swallowed.
                if confirm_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::ConfirmCancel)
                        }
                        _ => None,
                    };
                }

                if description_editor_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::DescriptionCancel)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Enter)
                            if modifiers.command() =>
                        {
                            Some(Message::DescriptionSave)
                        }
                        _ => None,
                    };
                }

                // A toolbar dropdown / activity popover is open: Esc dismisses it
                // and other keys are swallowed so they don't reach file nav.
                if menu_open || popover_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => Some(if menu_open {
                            Message::Menu(MenuMessage::Dismiss)
                        } else {
                            Message::ActivityToggle
                        }),
                        _ => None,
                    };
                }

                // Cmd/Ctrl+K opens (or toggles closed) the palette.
                if modifiers.command()
                    && matches!(
                        key.as_ref(),
                        keyboard::Key::Character("k") | keyboard::Key::Character("K")
                    )
                {
                    return Some(if palette_open {
                        Message::Palette(PaletteMessage::Close)
                    } else {
                        Message::Palette(PaletteMessage::Open)
                    });
                }

                // Cmd/Ctrl+F opens the in-diff find bar. No toggle; Esc
                // closes.
                if modifiers.command()
                    && matches!(
                        key.as_ref(),
                        keyboard::Key::Character("f") | keyboard::Key::Character("F")
                    )
                {
                    return Some(Message::Find(FindMessage::Open));
                }

                // Open-repo dialog owns the keyboard: Esc dismisses, everything
                // else falls through to its text input. (Enter is handled by the
                // input's `on_submit`.)
                if dialog_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::OpenRepoDialogClose)
                        }
                        _ => None,
                    };
                }

                if palette_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::Palette(PaletteMessage::PopColumn))
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                            Some(Message::Palette(PaletteMessage::MoveSelection(1)))
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                            Some(Message::Palette(PaletteMessage::MoveSelection(-1)))
                        }
                        keyboard::Key::Named(keyboard::key::Named::Tab) => {
                            Some(Message::Palette(PaletteMessage::PushActions))
                        }
                        _ => None,
                    };
                }

                if find_open {
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::Find(FindMessage::Close))
                        }
                        // Enter / Shift+Enter — handle both here since
                        // text_input intentionally has no on_submit (it
                        // would route every Enter to FindNext and swallow
                        // Shift+Enter on the way).
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(if modifiers.shift() {
                                Message::Find(FindMessage::Prev)
                            } else {
                                Message::Find(FindMessage::Next)
                            })
                        }
                        _ => None,
                    };
                }

                // Target mode owns the plain keys: arrows/j/k move the
                // destination candidate, o/a/b pick a placement, Enter
                // applies, Esc leaves. Unhandled plain keys are swallowed so
                // they can't fall through to file nav; ⌘/⌥/⌃ combos still
                // pass (tab switching, wrap toggle, …).
                if draft_open && !modifiers.command() && !modifiers.alt() && !modifiers.control() {
                    if matches!(
                        key.as_ref(),
                        keyboard::Key::Named(keyboard::key::Named::Escape)
                    ) {
                        return Some(Message::DraftCancel);
                    }
                    // A focused text input (the revset box) owns everything
                    // but Esc — don't hijack typed characters as draft keys.
                    if !ignored {
                        return None;
                    }
                    return match key.as_ref() {
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(Message::DraftConfirm)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                        | keyboard::Key::Character("j") => Some(Message::DraftCandidate(1)),
                        keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                        | keyboard::Key::Character("k") => Some(Message::DraftCandidate(-1)),
                        keyboard::Key::Character("o") => {
                            Some(Message::DraftPlacementKey(diffui_core::PlacementKind::Onto))
                        }
                        keyboard::Key::Character("a") => Some(Message::DraftPlacementKey(
                            diffui_core::PlacementKind::After,
                        )),
                        keyboard::Key::Character("b") => Some(Message::DraftPlacementKey(
                            diffui_core::PlacementKind::Before,
                        )),
                        // Toggle the candidate as a draft source (merge
                        // parent / rebase revision / squash source).
                        keyboard::Key::Named(keyboard::key::Named::Space) => {
                            Some(Message::DraftToggleSource)
                        }
                        _ => None,
                    };
                }

                // Tab management — only with no overlay holding the keyboard, so
                // these never steal keystrokes from a focused text input. ⌘W
                // closes the active tab, ⌘O opens the path dialog, ⌘1–9 jump to a
                // tab by position.
                if modifiers.command() && !modifiers.shift() && !modifiers.alt() {
                    match key.as_ref() {
                        keyboard::Key::Character("w") | keyboard::Key::Character("W") => {
                            return Some(Message::CloseActiveTab);
                        }
                        keyboard::Key::Character("o") | keyboard::Key::Character("O") => {
                            return Some(Message::OpenRepoDialogOpen);
                        }
                        keyboard::Key::Character(c) => {
                            if let Some(digit) = c.chars().next().and_then(|c| c.to_digit(10))
                                && (1..=9).contains(&digit)
                            {
                                return Some(Message::SelectTabIndex((digit - 1) as usize));
                            }
                        }
                        _ => {}
                    }
                }

                // ⌥Z toggles diff line-wrap (the editor-world convention).
                // macOS composes ⌥Z into "Ω", so match that form too.
                if modifiers.alt()
                    && !modifiers.command()
                    && !modifiers.control()
                    && let keyboard::Key::Character(c) = key.as_ref()
                    && matches!(c, "z" | "Z" | "Ω" | "ω")
                {
                    return Some(Message::ToggleDiffWrap);
                }

                // No overlay — global j/k/arrow file shortcuts apply. Only
                // fire when no modifier is held, otherwise ⌘J / ⌘K combos
                // would also trigger file nav.
                if modifiers.command() || modifiers.alt() || modifiers.control() {
                    return None;
                }
                // A focused widget (the revset input) consumed this key — don't also
                // route it to file nav.
                if !ignored {
                    return None;
                }
                match key.as_ref() {
                    keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                    | keyboard::Key::Character("j") => Some(Message::SelectNextFile),
                    keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                    | keyboard::Key::Character("k") => Some(Message::SelectPreviousFile),
                    // Target-mode entry points on the selected revision:
                    // `r` = rebase it, `R` = rebase it with descendants,
                    // `s` = squash it into a picked destination.
                    keyboard::Key::Character("r") => {
                        Some(Message::DraftStartKey(diffui_core::DraftKind::Rebase {
                            mode: diffui_core::RebaseSourceMode::Revisions,
                        }))
                    }
                    keyboard::Key::Character("R") => {
                        Some(Message::DraftStartKey(diffui_core::DraftKind::Rebase {
                            mode: diffui_core::RebaseSourceMode::WithDescendants,
                        }))
                    }
                    keyboard::Key::Character("s") | keyboard::Key::Character("S") => {
                        Some(Message::DraftStartKey(diffui_core::DraftKind::Squash))
                    }
                    // `m` = merge the selected revision with a picked one.
                    keyboard::Key::Character("m") | keyboard::Key::Character("M") => {
                        Some(Message::DraftStartKey(diffui_core::DraftKind::Merge))
                    }
                    // `b` = rebase the whole branch the selected revision is
                    // on (fork-point roots resolve against the destination).
                    keyboard::Key::Character("b") | keyboard::Key::Character("B") => {
                        Some(Message::DraftStartKey(diffui_core::DraftKind::Rebase {
                            mode: diffui_core::RebaseSourceMode::Branch,
                        }))
                    }
                    _ => None,
                }
            },
        );

        // Modifier tracking for pointer gestures (⌥-drop = move with
        // descendants). Separate from the key listener: `ModifiersChanged`
        // is its own event kind.
        let modifier_events = event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                Some(Message::ModifiersChanged(modifiers))
            }
            _ => None,
        });

        let window_events = event::listen().filter_map(|event| match event {
            Event::Window(window::Event::Focused) => Some(Message::WindowFocusChanged(true)),
            Event::Window(window::Event::Unfocused) => Some(Message::WindowFocusChanged(false)),
            Event::Window(window::Event::Opened { position, size, .. }) => {
                Some(Message::WindowOpened(position, size))
            }
            Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized(size)),
            Event::Window(window::Event::Moved(position)) => Some(Message::WindowMoved(position)),
            _ => None,
        });
        // Watch the working tree for changes instead of polling. The
        // subscription identity is keyed on the repo root, so the watcher
        // starts once and persists; `RefreshRepository` itself is gated on
        // focus, so edits made while unfocused are picked up on focus-regain.
        let refresh = match &self.session.repository {
            Some(repository) => Subscription::run_with(repository.root.clone(), watch_repository),
            None => Subscription::none(),
        };

        // Per-frame ticks during a palette push/pop animation. iced's
        // `Animation::interpolate_with` is read-only — to actually drive
        // the interpolation forward in time we need iced to keep
        // re-rendering. Subscribing to a 60-Hz timer while the animation
        // is in progress keeps the view function re-running; the handler
        // is a no-op, the side effect is the render itself.
        let palette_animating = self
            .palette
            .as_ref()
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
        let work_in_flight = self.session.loading_since.is_some()
            || self.session.pending_revision.is_some()
            || self.activities.any_running();
        let loading_tick = if work_in_flight {
            time::every(Duration::from_millis(120)).map(|_| Message::LoadingTick)
        } else {
            Subscription::none()
        };

        // Drives the debounced geometry save. Active only while a change is
        // pending; the handler writes once the changes settle, then clears the
        // dirty flag, which tears this subscription back down.
        let window_state_tick = if self.geometry_dirty_since.is_some() {
            time::every(WINDOW_STATE_DEBOUNCE).map(|_| Message::PersistWindowState)
        } else {
            Subscription::none()
        };

        // Per-frame ticks while a right-click menu's row glow is up (so its pulse
        // animates — the render is the effect; only the iced overlay glows, macOS
        // animates natively) or while a submenu is open (so the trajectory apex
        // eases toward the cursor / catches up when it idles).
        let menu_ticking = self
            .menu
            .as_ref()
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
                time::every(Duration::from_secs(1)).map(|_| Message::PollSystemTheme)
            } else {
                Subscription::none()
            };

        // Drives the custom double-click zoom: while an animation is in flight,
        // tick at ~60fps so each frame steps the window toward its target. Tears
        // itself down the moment the animation completes.
        let zoom_tick = if self.zoom_anim.is_some() {
            time::every(Duration::from_millis(16)).map(|_| Message::ZoomAnimTick)
        } else {
            Subscription::none()
        };

        // Prunes expired error toasts; subscribed only while any are up.
        let toast_tick = if self.toasts.is_empty() {
            Subscription::none()
        } else {
            time::every(Duration::from_millis(500)).map(|_| Message::ToastTick)
        };

        Subscription::batch([
            keyboard,
            modifier_events,
            window_events,
            refresh,
            palette_tick,
            loading_tick,
            menu_tick,
            window_state_tick,
            system_theme_poll,
            zoom_tick,
            toast_tick,
            system::theme_changes().map(Message::SystemThemeChanged),
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
                .get(self.active_tab)
                .and_then(|tab| Some(tab.root()?.to_string_lossy().into_owned())),
            revsets: self.collect_revsets(),
            recent_repos: self.recent_repos.clone(),
        }
    }

    /// Gather each open tab's revset (active inline + stashed), keyed by repo
    /// root, dropping empties so the persisted map stays tidy.
    pub(crate) fn collect_revsets(&self) -> BTreeMap<String, String> {
        let mut revsets = BTreeMap::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let Some(root) = tab.root() else {
                continue;
            };
            let revset = if index == self.active_tab {
                self.session.revset.clone()
            } else {
                tab.stash
                    .as_ref()
                    .map(|state| state.session.revset.clone())
                    .unwrap_or_default()
            };
            if !revset.is_empty() {
                revsets.insert(root.to_string_lossy().into_owned(), revset);
            }
        }
        revsets
    }

    pub(crate) fn resolved_theme(&self) -> ResolvedTheme {
        self.selected_theme.active(self.system_theme)
    }
}
