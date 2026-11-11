//! The jj repository actor.
//!
//! One thread, one current-thread tokio runtime (jj-lib's async APIs need a
//! runtime; its types are not `Send`, so they cannot cross to a pool), and one
//! owner for the workspaces and the repo at head. Commands arrive on a channel
//! and are answered in order, which is why mutations serialize without a queue
//! and why a fetch can no longer overlap one.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use jj_lib::{
    object_id::ObjectId,
    ref_name::WorkspaceNameBuf,
    repo::{ReadonlyRepo, Repo},
    settings::UserSettings,
    workspace::Workspace,
};
use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::cancel::JobTicket;
use super::protocol::{
    Command, Event, GraphTail, JobId, Payload, PreviewRequest, RepoError, RepoId,
};
use super::{Envelope, Jobs, SettingsSource, catch_panic, route_commands, run_job};
use crate::history::{HistoryRequest, HistoryResponse};
use crate::jj;
use crate::model::{DiffBase, LoadProgress, RevisionSelection, StreamRow};
use crate::mutations::MutationOp;
use crate::repository::Repository;
use crate::watcher::WatchBatch;

/// Rows per streamed batch. Small enough that the first screenful paints
/// quickly, large enough that a million commits don't flood the channel.
const BATCH_SIZE: usize = 256;

/// The workspace this actor owns, opened once on the first command that names
/// it and held for the actor's life.
struct WorkspaceSlot {
    workspace: Workspace,
    settings: UserSettings,
    snapshot: jj::SnapshotContext,
    name: WorkspaceNameBuf,
    /// What the config files looked like when `settings` were read, or `None`
    /// for fixed settings, which never reload.
    config_stamp: Option<jj::settings::JjConfigStamp>,
}

pub(super) fn spawn(
    id: RepoId,
    root: PathBuf,
    settings: SettingsSource,
    commands: mpsc::UnboundedReceiver<Envelope>,
    poke: mpsc::UnboundedSender<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    let watch_root = root.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("diffui-repo-{}", id.0))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "failed to build the repository actor's runtime");
                    return;
                }
            };
            super::watch(watch_root, poke);
            runtime.block_on(async move {
                // Two futures on one thread: one reads the command channel so a
                // cancel lands while its job is still running, the other runs
                // the commands. `join!` polls them in the same task, and the
                // walk yields at each batch, so the reader is never starved.
                let jobs: Jobs = Jobs::default();
                let (work_tx, work_rx) = mpsc::unbounded_channel();
                let router = route_commands(commands, work_tx, jobs.clone());
                let actor = async {
                    Actor {
                        id,
                        events,
                        settings,
                        workspace: None,
                        repo: None,
                        op_head: None,
                        sides: None,
                        jobs,
                        stale_notice: None,
                    }
                    .run(work_rx)
                    .await;
                };
                tokio::join!(router, actor);
            });
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "failed to spawn the repository actor thread");
    }
}

struct Actor {
    id: RepoId,
    events: mpsc::UnboundedSender<Event>,
    settings: SettingsSource,
    workspace: Option<WorkspaceSlot>,
    /// The repo at head, reloaded only when the operation head actually moves.
    repo: Option<Arc<ReadonlyRepo>>,
    op_head: Option<String>,
    /// The trees the last diff compared, for the reads that follow it (each
    /// file's full sources for highlighting, a file expanded in full). Keyed
    /// by operation, so anything written since misses it.
    sides: Option<(SidesKey, jj::DiffSides)>,
    jobs: Jobs,
    /// The stale-checkout notice last reported, so a refresh on every save
    /// doesn't raise it again until it changes or the checkout is updated.
    stale_notice: Option<String>,
}

/// Stops [`Actor::relay_progress`]'s thread when dropped.
struct ProgressRelay(CancelFlag);

impl Drop for ProgressRelay {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// What [`Actor::sides`] was resolved for: an operation id, a revision and a
/// base.
type SidesKey = (String, RevisionSelection, DiffBase);

impl Actor {
    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<Envelope>) {
        while let Some(envelope) = commands.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                Envelope::Watch(batch) => {
                    if let Err(message) = catch_panic(self.on_watch(batch)).await {
                        tracing::error!(repo = %self.id, "the watch handler panicked: {message}");
                        self.forget_loaded_state();
                    }
                }
                Envelope::Run {
                    repository,
                    command,
                } => {
                    // Cancels never reach here; `route_commands` applies them.
                    let Some(job) = command.job() else { continue };
                    // Let the reader run before the job starts. A caller that
                    // supersedes its own work sends the replacement and the
                    // cancel together, and a short job can otherwise finish
                    // without ever yielding — the two futures share a thread.
                    tokio::task::yield_now().await;
                    let span = tracing::info_span!(
                        "repo_command",
                        repo = %self.id,
                        job = job.0,
                        command = command_name(&command),
                    );
                    let (events, id) = (self.events.clone(), self.id.clone());
                    let emit = |payload| {
                        let _ = events.send(Event::new(id.clone(), payload));
                    };
                    let jobs = self.jobs.clone();
                    let actor = &mut *self;
                    let panicked = run_job(&jobs, job, span, emit, move |ticket| async move {
                        #[cfg(feature = "fault-injection")]
                        super::fault::trip(&repository.root);
                        actor
                            .dispatch(&repository, *command, ticket)
                            .await
                            .map_err(classify)
                    })
                    .await;
                    if panicked {
                        self.forget_loaded_state();
                    }
                }
            }
            if self.events.is_closed() {
                return;
            }
        }
    }

    /// A filesystem change. Op-log writes are deduped against the head we last
    /// saw — most of them are our own snapshots — and a working-tree edit is
    /// reported as itself, so the projection asks for a snapshot rather than a
    /// walk.
    async fn on_watch(&mut self, batch: WatchBatch) {
        if batch.worktree {
            self.emit(Payload::WorkingCopyChanged);
        }
        if !batch.op_log {
            return;
        }
        let Some(slot) = self.workspace.as_ref() else {
            return;
        };
        match jj::read_op_heads(slot.workspace.repo_loader()).await {
            Ok(fingerprint) => {
                if self.op_head.as_deref() != Some(fingerprint.as_str()) {
                    self.op_head = Some(fingerprint.clone());
                    self.emit(Payload::OpHeadChanged { fingerprint });
                }
            }
            Err(error) => tracing::warn!(%error, "failed to read the jj op head"),
        }
    }

    /// Drop everything loaded from disk after a panic: the workspace, its
    /// lock-holding state and the cached repo may be half-updated, so the next
    /// command reopens them from scratch.
    fn forget_loaded_state(&mut self) {
        self.workspace = None;
        self.repo = None;
        self.op_head = None;
        self.sides = None;
    }

    fn emit(&self, payload: Payload) {
        let _ = self.events.send(Event::new(self.id.clone(), payload));
    }

    /// A progress handle for `job`'s git transfer (a fetch, a push), relayed
    /// as `Progress` events while the returned guard lives. git runs inside a
    /// blocking jj-lib call that holds this thread until it returns, so the
    /// relay polls from a thread of its own.
    fn relay_progress(&self, job: JobId) -> (LoadProgress, ProgressRelay) {
        const POLL: std::time::Duration = std::time::Duration::from_millis(100);

        let progress = LoadProgress::default();
        let stop = CancelFlag::default();
        let (events, id) = (self.events.clone(), self.id.clone());
        let (reporter, stopped) = (progress.clone(), stop.clone());
        let spawned = std::thread::Builder::new()
            .name("diffui-progress".to_owned())
            .spawn(move || {
                let mut last = (0, 0);
                while !stopped.is_cancelled() {
                    std::thread::sleep(POLL);
                    let (loaded, total) = reporter.snapshot();
                    // Nothing to show until git reports a total.
                    if total > 0 && (loaded, total) != last {
                        last = (loaded, total);
                        let _ = events.send(Event::new(
                            id.clone(),
                            Payload::Progress { job, loaded, total },
                        ));
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to spawn the progress relay");
        }
        (progress, ProgressRelay(stop))
    }

    /// Open (once) the workspace `repository` names.
    fn slot(&mut self, repository: &Repository) -> Result<&mut WorkspaceSlot> {
        if self.workspace.is_none() {
            let root = &repository.root;
            // Stamped before the read, so an edit racing the load shows up as
            // a change next time instead of being missed.
            let (settings, config_stamp) = match &self.settings {
                SettingsSource::Layered => {
                    let stamp = jj::settings::jj_config_stamp(root);
                    (jj::settings::jj_settings(root)?, Some(stamp))
                }
                SettingsSource::Fixed(settings) => ((**settings).clone(), None),
            };
            let workspace = jj::load_workspace(&settings, root)?;
            let name = workspace.workspace_name().to_owned();
            let snapshot = jj::SnapshotContext::load(&settings, root)?;
            self.workspace = Some(WorkspaceSlot {
                workspace,
                settings,
                snapshot,
                name,
                config_stamp,
            });
        }
        Ok(self
            .workspace
            .as_mut()
            .expect("the workspace was just opened"))
    }

    /// [`Self::slot`], about to take the working-copy lock: the repo we hold
    /// goes along, so the lock reuses it instead of loading the one at head.
    fn slot_for_lock(&mut self, repository: &Repository) -> Result<&mut WorkspaceSlot> {
        let head = self.repo.clone();
        let slot = self.slot(repository)?;
        slot.snapshot.head.set(head);
        Ok(slot)
    }

    /// Re-read the jj config when one of its files changed since the
    /// workspace was opened, so an edited identity, signing setup or
    /// `immutable_heads()` applies without reopening the tab. jj-lib bakes the
    /// settings into the workspace's repo loader, so the workspace is reopened
    /// with them (a handful of small file reads). Called before the work that
    /// writes commits or answers a manual refresh; a stat per config file is
    /// all it costs when nothing changed.
    fn reload_settings_if_changed(&mut self, repository: &Repository) {
        let Some(stamp) = self
            .workspace
            .as_ref()
            .and_then(|slot| slot.config_stamp.as_ref())
        else {
            return;
        };
        if *stamp == jj::settings::jj_config_stamp(&repository.root) {
            return;
        }
        tracing::info!("jj config changed on disk; reloading the workspace settings");
        self.forget_loaded_state();
    }

    /// The repo at head, reloading only when the operation head has moved.
    /// Reading the op heads is a readdir through the workspace's loader, so
    /// paying it before every read is far cheaper than re-reading the commit
    /// index would be.
    ///
    /// A head that moved since we last looked is an operation someone else
    /// wrote, so it is announced here as the watcher would have: the watcher's
    /// own signal arrives later and finds the head already seen.
    async fn repo_at_head(&mut self, repository: &Repository) -> Result<Arc<ReadonlyRepo>> {
        let fingerprint = jj::read_op_heads(self.slot(repository)?.workspace.repo_loader())
            .await
            .ok();
        if let Some(repo) = self.repo.clone()
            && fingerprint.is_some()
            && fingerprint == self.op_head
        {
            return Ok(repo);
        }
        let repo = match &self.repo {
            Some(repo) => repo
                .reload_at_head()
                .await
                .context("failed to reload the jj repo at head")?,
            None => self
                .slot(repository)?
                .workspace
                .repo_loader()
                .load_at_head()
                .await
                .context("failed to load the jj repo")?,
        };
        let previous = self.op_head.replace(repo.op_id().hex());
        self.repo = Some(repo.clone());
        if let Some(previous) = previous
            && Some(&previous) != self.op_head.as_ref()
        {
            self.emit(Payload::OpHeadChanged {
                fingerprint: repo.op_id().hex(),
            });
        }
        Ok(repo)
    }

    /// Adopt a repo the working-copy lock already produced, so the next read
    /// doesn't reload what we are holding.
    fn adopt(&mut self, repo: Arc<ReadonlyRepo>) {
        self.op_head = Some(repo.op_id().hex());
        self.repo = Some(repo);
    }

    /// Take note of an operation we just wrote.
    ///
    /// The op head is read afresh so the watcher's next signal dedups against
    /// *our* write. Leaving it stale made every mutation and fetch look like
    /// an external operation, which cost a second lock-and-snapshot that could
    /// only report "already up to date". The cached repo stays only when the
    /// head didn't move (the work committed nothing).
    async fn adopt_written_op(&mut self, repository: &Repository) {
        let Ok(slot) = self.slot(repository) else {
            self.repo = None;
            self.op_head = None;
            return;
        };
        self.op_head = jj::read_op_heads(slot.workspace.repo_loader()).await.ok();
        if self.repo.as_ref().map(|repo| repo.op_id().hex()) != self.op_head {
            self.repo = None;
        }
    }

    /// After a CLI run: it may have written operations (or edited the jj
    /// config), so take the head now rather than waiting on the watcher.
    /// Announcing it here puts the reload right after the run's exit, and
    /// the watcher's later signal for the same head dedups against it.
    async fn notice_cli_writes(&mut self, repository: &Repository) {
        let before = self.op_head.clone();
        self.reload_settings_if_changed(repository);
        self.adopt_written_op(repository).await;
        if self.op_head != before
            && let Some(fingerprint) = self.op_head.clone()
        {
            self.emit(Payload::OpHeadChanged { fingerprint });
        }
    }

    async fn dispatch(
        &mut self,
        repository: &Repository,
        command: Command,
        ticket: JobTicket,
    ) -> Result<()> {
        let cancel = &ticket.flag().clone();
        match command {
            Command::LoadGraph { job, revset } => {
                self.load_graph(repository, job, revset, cancel).await
            }
            Command::LoadDiff {
                job,
                revision,
                base,
                expand,
            } => {
                self.load_diff(repository, job, revision, base, &expand, cancel)
                    .await
            }
            Command::ExpandFile {
                job,
                revision,
                base,
                path,
            } => {
                let (repo, sides) = self.diff_sides(repository, &revision, &base).await?;
                let expand = [path.clone()];
                let options = jj::DiffOptions {
                    limits: jj::large_diff::DiffLimits::unlimited(),
                    expand: &expand,
                    only: Some(&path),
                };
                let document =
                    match jj::diff_jj(repo.as_ref(), &sides, repository, &options, cancel).await {
                        Err(_) if cancel.is_cancelled() => {
                            self.emit(Payload::Cancelled { job });
                            return Ok(());
                        }
                        document => document?,
                    };
                let file = document.files.into_iter().find(|file| file.path == path);
                self.emit(Payload::FileExpanded { job, path, file });
                Ok(())
            }
            Command::Snapshot { job, origin } => {
                // A watcher tick is a file save; only a refresh (or the tail
                // of a mutation, or focus regain) is worth the config check.
                if origin == crate::session::RefreshOrigin::Focus {
                    self.reload_settings_if_changed(repository);
                }
                let slot = self.slot_for_lock(repository)?;
                let outcome = {
                    let WorkspaceSlot {
                        workspace,
                        snapshot,
                        ..
                    } = slot;
                    jj::snapshot_working_copy(workspace, snapshot).await?
                };
                self.adopt(outcome.repo.clone());
                if outcome.recovered_stale {
                    self.emit(Payload::StaleWorkingCopyRecovered);
                }
                if outcome.stale != self.stale_notice {
                    if let Some(message) = &outcome.stale {
                        self.emit(Payload::StaleWorkingCopy {
                            message: message.clone(),
                        });
                    }
                    self.stale_notice = outcome.stale;
                }
                self.emit(Payload::SnapshotDone {
                    job,
                    origin,
                    snapshot: outcome.snapshot,
                    warnings: outcome.warnings,
                });
                Ok(())
            }
            Command::Mutate {
                job,
                op,
                allow_immutable,
                seen_op,
                seen_wc,
            } => {
                self.mutate(
                    repository,
                    job,
                    &op,
                    allow_immutable,
                    seen_op.as_deref(),
                    seen_wc.as_deref(),
                )
                .await
            }
            Command::Preview {
                job,
                draft,
                graph,
                seen_op,
            } => {
                self.preview(repository, job, draft, graph, seen_op, ticket)
                    .await
            }
            Command::Fetch { job, target } => {
                self.reload_settings_if_changed(repository);
                let (progress, _relay) = self.relay_progress(job);
                let slot = self.slot_for_lock(repository)?;
                let outcome = {
                    let WorkspaceSlot {
                        workspace,
                        settings,
                        snapshot,
                        ..
                    } = slot;
                    jj::fetch_jj(workspace, settings, snapshot, &target, &progress).await?
                };
                self.adopt_written_op(repository).await;
                self.emit(Payload::FetchDone { job, outcome });
                Ok(())
            }
            Command::RevisionDetails { job, revision } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let commit_id = jj::resolve_revision(repo.as_ref(), &name, &revision).await?;
                let commit = repo
                    .store()
                    .get_commit_async(&commit_id)
                    .await
                    .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
                let details = jj::jj_revision_details(repo.as_ref(), &commit).await;
                self.emit(Payload::DetailsLoaded {
                    job,
                    revision,
                    details,
                });
                Ok(())
            }
            Command::EmptyStatus { job, targets } => {
                let repo = self.repo_at_head(repository).await?;
                match jj::compute_jj_empty_status(repo.as_ref(), targets, cancel).await {
                    Some(updates) => self.emit(Payload::EmptyStatus { job, updates }),
                    None => self.emit(Payload::Cancelled { job }),
                }
                Ok(())
            }
            Command::ListTree { job, revision } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let entries =
                    jj::list_jj_source_tree(repo.as_ref(), &name, repository, &revision).await?;
                self.emit(Payload::TreeListed {
                    job,
                    revision,
                    entries: crate::source_browse::sort_source_entries(entries),
                });
                Ok(())
            }
            Command::ReadFile {
                job,
                revision,
                path,
            } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let data =
                    jj::read_jj_source_file(repo.as_ref(), &name, repository, &revision, &path)
                        .await?;
                self.emit(Payload::FileRead {
                    job,
                    revision,
                    file: crate::source_browse::build_source_file(&path, data),
                    path,
                });
                Ok(())
            }
            Command::FilePair {
                job,
                revision,
                path,
                old_path,
                base,
            } => {
                let (repo, sides) = self.diff_sides(repository, &revision, &base).await?;
                let (old, new) =
                    jj::read_jj_file_pair(repo.as_ref(), &sides, &path, old_path.as_deref()).await;
                self.emit(Payload::FilePairRead {
                    job,
                    path,
                    old,
                    new,
                });
                Ok(())
            }
            Command::BookmarkCheck {
                job,
                name,
                to,
                seen_op,
            } => {
                let repo = self.repo_at_head(repository).await?;
                let workspace_name = self.slot(repository)?.name.clone();
                let backwards = jj::check_bookmark_move_backwards(
                    repo.as_ref(),
                    &workspace_name,
                    &name,
                    &to,
                    seen_op.as_deref(),
                )
                .await?;
                self.emit(Payload::BookmarkChecked { job, backwards });
                Ok(())
            }
            Command::ConflictInfo { job, revision } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let commit_id = jj::resolve_revision(repo.as_ref(), &name, &revision).await?;
                let commit = repo
                    .store()
                    .get_commit_async(&commit_id)
                    .await
                    .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
                let files = jj::conflict_files(&commit).await?;
                self.emit(Payload::ConflictInfoLoaded {
                    job,
                    revision,
                    files,
                });
                Ok(())
            }
            Command::PrepareMergeTool {
                job,
                revision,
                path,
            } => {
                let repo = self.repo_at_head(repository).await?;
                let (settings, name) = {
                    let slot = self.slot(repository)?;
                    (slot.settings.clone(), slot.name.clone())
                };
                let invocation =
                    jj::prepare_merge_tool(repo.as_ref(), &settings, &name, &revision, &path)
                        .await?;
                self.emit(Payload::MergeToolPrepared { job, invocation });
                Ok(())
            }
            Command::ConflictText {
                job,
                revision,
                path,
            } => {
                let repo = self.repo_at_head(repository).await?;
                let (settings, name) = {
                    let slot = self.slot(repository)?;
                    (slot.settings.clone(), slot.name.clone())
                };
                let (text, marker_len) =
                    jj::conflict_text(repo.as_ref(), &settings, &name, &revision, &path).await?;
                self.emit(Payload::ConflictTextLoaded {
                    job,
                    path,
                    text,
                    marker_len,
                });
                Ok(())
            }
            Command::History { job, request } => {
                self.history(repository, job, request, ticket).await
            }
            Command::ResolveRevset { job, query } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let answer = revset_scope(&repo, &name, repository)
                    .query(&query, cancel)
                    .await;
                match answer {
                    Some(result) if !cancel.is_cancelled() => self.emit(Payload::RevsetResolved {
                        job,
                        revset: query.revset,
                        result,
                    }),
                    _ => self.emit(Payload::Cancelled { job }),
                }
                Ok(())
            }
            Command::ResolveDiff { job, request } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let (revision, base) = revset_scope(&repo, &name, repository)
                    .diff_request(&request)
                    .await?;
                self.emit(Payload::DiffResolved {
                    job,
                    revision,
                    base,
                });
                Ok(())
            }
            Command::RunCli { job, invocation } => {
                crate::cli::passthrough::run(
                    job,
                    &invocation,
                    &repository.root,
                    cancel,
                    |payload| self.emit(payload),
                )
                .await?;
                self.notice_cli_writes(repository).await;
                Ok(())
            }
            Command::Cancel { .. } => Ok(()),
        }
    }

    async fn load_graph(
        &mut self,
        repository: &Repository,
        job: JobId,
        revset: String,
        cancel: &CancelFlag,
    ) -> Result<()> {
        let repo = self.repo_at_head(repository).await?;
        let name = self.slot(repository)?.name.clone();
        let wc_commit_id = repo
            .view()
            .get_wc_commit_id(&name)
            .context("jj workspace has no working-copy commit")?
            .clone();

        // The walk bumps `progress` as it goes; each batch carries the count so
        // far out with it, so the toolbar's bar is determinate without the
        // frontend holding a handle into the walk.
        let progress = LoadProgress::default();
        let events = self.events.clone();
        let id = self.id.clone();
        let reporter = progress.clone();
        let mut emit = |rows: Vec<StreamRow>| {
            let _ = events.send(Event::new(id.clone(), Payload::Batch { job, rows }));
            let (loaded, total) = reporter.snapshot();
            let _ = events.send(Event::new(
                id.clone(),
                Payload::Progress { job, loaded, total },
            ));
        };
        let walked = jj::walk_jj_with_repo(
            repo.as_ref(),
            jj::WorkspaceView {
                wc_commit_id: &wc_commit_id,
                workspace_name: &name,
            },
            &repository.root,
            &revset,
            progress,
            BATCH_SIZE,
            cancel,
            &mut emit,
        )
        .await?;
        match walked {
            Some((empty_updates, branch_status, bookmarks)) => self.emit(Payload::GraphLoaded {
                job,
                tail: GraphTail {
                    empty_updates,
                    branch_status,
                    bookmarks,
                    root_commit_id: Some(repo.store().root_commit_id().hex()),
                    op_id: Some(repo.op_id().hex()),
                    symbols: jj::revset::symbols(&repo, repo.settings()),
                },
            }),
            None => self.emit(Payload::Cancelled { job }),
        }
        Ok(())
    }

    async fn load_diff(
        &mut self,
        repository: &Repository,
        job: JobId,
        revision: RevisionSelection,
        base: DiffBase,
        expand: &[String],
        cancel: &CancelFlag,
    ) -> Result<()> {
        let (repo, sides) = self.diff_sides(repository, &revision, &base).await?;
        let options = jj::DiffOptions {
            expand,
            ..Default::default()
        };
        let document = match jj::diff_jj(repo.as_ref(), &sides, repository, &options, cancel).await
        {
            Err(_) if cancel.is_cancelled() => {
                self.emit(Payload::Cancelled { job });
                return Ok(());
            }
            document => document?,
        };
        // A range's revset names both sides, and the selected revision rides
        // along untouched as the graph's cursor.
        let Some(commit) = &sides.commit else {
            self.emit(Payload::DiffLoaded {
                job,
                revision,
                document,
                details: None,
            });
            return Ok(());
        };
        let details = jj::jj_revision_details(repo.as_ref(), commit).await;
        // A commit rewritten since the caller read its id resolved to its
        // successor; answer under the id that was actually loaded, so the
        // selection follows the change instead of naming a hidden commit.
        let revision = match revision {
            RevisionSelection::Commit(hex) if hex != commit.id().hex() => {
                RevisionSelection::Commit(commit.id().hex())
            }
            revision => revision,
        };
        self.emit(Payload::DiffLoaded {
            job,
            revision,
            document,
            details: Some(details),
        });
        Ok(())
    }

    /// The trees a diff of `revision` against `base` compares, resolved once
    /// per operation and shared by the load and the reads that follow it.
    async fn diff_sides(
        &mut self,
        repository: &Repository,
        revision: &RevisionSelection,
        base: &DiffBase,
    ) -> Result<(Arc<ReadonlyRepo>, jj::DiffSides)> {
        let repo = self.repo_at_head(repository).await?;
        let key = (repo.op_id().hex(), revision.clone(), base.clone());
        if let Some((cached, sides)) = &self.sides
            && *cached == key
        {
            return Ok((repo, sides.clone()));
        }
        let name = self.slot(repository)?.name.clone();
        let sides =
            jj::DiffSides::resolve(repo.as_ref(), &name, &repository.root, revision, base).await?;
        self.sides = Some((key, sides.clone()));
        Ok((repo, sides))
    }

    async fn mutate(
        &mut self,
        repository: &Repository,
        job: JobId,
        op: &MutationOp,
        allow_immutable: bool,
        seen_op: Option<&str>,
        seen_wc: Option<&str>,
    ) -> Result<()> {
        self.reload_settings_if_changed(repository);
        let (progress, _relay) = self.relay_progress(job);
        let slot = self.slot_for_lock(repository)?;
        let (outcome, recovered_stale) = {
            let WorkspaceSlot {
                workspace,
                settings,
                snapshot,
                ..
            } = slot;
            jj::apply_mutation(
                workspace,
                settings,
                snapshot,
                repository,
                op,
                &progress,
                allow_immutable,
                seen_op,
                seen_wc,
            )
            .await?
        };
        self.adopt_written_op(repository).await;
        if recovered_stale {
            self.emit(Payload::StaleWorkingCopyRecovered);
        }
        self.emit(Payload::MutationDone { job, outcome });
        Ok(())
    }

    /// Simulate a draft on a thread of its own, against the operation the
    /// actor is at. A big rebase takes seconds to simulate and jj-lib can't
    /// be interrupted inside one, so on the actor it held up every diff and
    /// load behind it even after the draft was gone; detached, a cancel only
    /// has to stop the thread, and the actor answers the next command at
    /// once. The thread sends the terminal event itself.
    async fn preview(
        &mut self,
        repository: &Repository,
        job: JobId,
        draft: PreviewRequest,
        graph: Option<String>,
        seen_op: Option<String>,
        ticket: JobTicket,
    ) -> Result<()> {
        let operation = self.repo_at_head(repository).await?.op_id().hex();
        let settings = self.slot(repository)?.settings.clone();
        let repository = repository.clone();
        let (events, id) = (self.events.clone(), self.id.clone());
        std::thread::Builder::new()
            .name("diffui-preview".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let result = match runtime {
                    Ok(runtime) => runtime.block_on(jj::preview_at_operation(
                        &settings,
                        &repository,
                        &operation,
                        &draft,
                        graph.as_deref(),
                        seen_op.as_deref(),
                        ticket.flag(),
                    )),
                    Err(error) => Err(error.into()),
                };
                let payload = match result {
                    _ if ticket.flag().is_cancelled() => Payload::Cancelled { job },
                    Ok((simulation, graph)) => Payload::PreviewDone {
                        job,
                        simulation,
                        graph: graph.map(Arc::new),
                    },
                    Err(error) => Payload::Failed {
                        job,
                        error: classify(error),
                    },
                };
                let _ = events.send(Event::new(id, payload));
            })
            .context("failed to spawn the preview thread")?;
        Ok(())
    }
}

impl Actor {
    /// Annotate on a thread of its own. Walking a long file's ancestry takes
    /// seconds on a big repository, and everything else the tab asks for
    /// would queue behind it on the actor. The thread loads its own repo at
    /// the operation the actor is at, and sends the terminal event itself.
    fn annotate_detached(
        &mut self,
        repository: &Repository,
        repo: &ReadonlyRepo,
        job: JobId,
        revision: crate::model::RevisionSelection,
        path: String,
        ticket: JobTicket,
    ) -> Result<()> {
        let settings = self.slot(repository)?.settings.clone();
        let root = repository.root.clone();
        let operation = repo.op_id().hex();
        let (events, id) = (self.events.clone(), self.id.clone());
        std::thread::Builder::new()
            .name("diffui-annotate".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let result = match runtime {
                    Ok(runtime) => runtime.block_on(jj::history::annotate_at_operation(
                        &settings, &root, &operation, &revision, &path,
                    )),
                    Err(error) => Err(error.into()),
                };
                let payload = match result {
                    _ if ticket.flag().is_cancelled() => Payload::Cancelled { job },
                    Ok(annotation) => Payload::History {
                        job,
                        response: HistoryResponse::Annotation(annotation),
                    },
                    Err(error) => Payload::Failed {
                        job,
                        error: classify(error),
                    },
                };
                let _ = events.send(Event::new(id, payload));
            })
            .context("failed to spawn the annotate thread")?;
        Ok(())
    }

    /// The history views' reads. All run against the repo at head; none take
    /// the working-copy lock.
    async fn history(
        &mut self,
        repository: &Repository,
        job: JobId,
        request: HistoryRequest,
        ticket: JobTicket,
    ) -> Result<()> {
        let cancel = ticket.flag();
        let repo = self.repo_at_head(repository).await?;
        let name = self.slot(repository)?.name.clone();
        let response = match request {
            HistoryRequest::OpLog { limit } => {
                let events = self.events.clone();
                let id = self.id.clone();
                let mut emit = |entries| {
                    let _ =
                        events.send(Event::new(id.clone(), Payload::OpLogBatch { job, entries }));
                };
                match jj::history::stream_op_log(&repo, limit, BATCH_SIZE, cancel, &mut emit)
                    .await?
                {
                    Some(truncated) => HistoryResponse::OpLogDone { truncated },
                    None => {
                        self.emit(Payload::Cancelled { job });
                        return Ok(());
                    }
                }
            }
            HistoryRequest::ShowOperation { operation_id } => {
                HistoryResponse::Operation(jj::history::show_operation(&repo, &operation_id).await?)
            }
            HistoryRequest::Evolog { revision } => {
                HistoryResponse::Evolog(jj::history::evolog(&repo, &name, &revision).await?)
            }
            HistoryRequest::Diff { target } => {
                let (document, details) =
                    jj::history::history_diff(&repo, repository, &target).await?;
                HistoryResponse::Diff {
                    target,
                    document,
                    details,
                }
            }
            HistoryRequest::Annotate { revision, path } => {
                self.annotate_detached(repository, &repo, job, revision, path, ticket)?;
                return Ok(());
            }
        };
        if cancel.is_cancelled() {
            self.emit(Payload::Cancelled { job });
        } else {
            self.emit(Payload::History { job, response });
        }
        Ok(())
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::LoadGraph { .. } => "load_graph",
        Command::LoadDiff { .. } => "load_diff",
        Command::Snapshot { .. } => "snapshot",
        Command::Mutate { .. } => "mutate",
        Command::Preview { .. } => "preview",
        Command::Fetch { .. } => "fetch",
        Command::RevisionDetails { .. } => "revision_details",
        Command::EmptyStatus { .. } => "empty_status",
        Command::ListTree { .. } => "list_tree",
        Command::ReadFile { .. } => "read_file",
        Command::FilePair { .. } => "file_pair",
        Command::BookmarkCheck { .. } => "bookmark_check",
        Command::ConflictInfo { .. } => "conflict_info",
        Command::PrepareMergeTool { .. } => "prepare_merge_tool",
        Command::ConflictText { .. } => "conflict_text",
        Command::History { .. } => "history",
        Command::ExpandFile { .. } => "expand_file",
        Command::ResolveRevset { .. } => "resolve_revset",
        Command::ResolveDiff { .. } => "resolve_diff",
        Command::RunCli { .. } => "run_cli",
        Command::Cancel { .. } => "cancel",
    }
}

/// Where the actor parses a revset: the repo at head, this workspace's `@`.
fn revset_scope<'a>(
    repo: &'a ReadonlyRepo,
    name: &'a WorkspaceNameBuf,
    repository: &'a Repository,
) -> jj::revset::RevsetScope<'a> {
    jj::revset::RevsetScope {
        repo,
        workspace_name: name,
        repo_root: &repository.root,
    }
}

/// Give an `anyhow` chain from jj-lib the shape the frontend can act on. The
/// immutable rejection is the one the UI turns into an offer ("rewrite
/// anyway"), so it travels as data rather than prose.
pub(super) fn classify(error: anyhow::Error) -> RepoError {
    if let Some(immutable) = error.downcast_ref::<jj::ImmutableRewriteError>() {
        return RepoError::Immutable {
            short_id: immutable.short_id.clone(),
        };
    }
    if let Some(stale) = error.downcast_ref::<jj::workspace::StaleWorkingCopy>() {
        return RepoError::StaleWorkspace(stale.0.clone());
    }
    if let Some(undo) = error.downcast_ref::<jj::undo::UndoesPush>() {
        return RepoError::UndoesPush {
            summary: undo.summary.clone(),
        };
    }
    if let Some(rejection) = error.downcast_ref::<crate::push::PushRejection>() {
        return RepoError::PushRejected(Box::new(rejection.clone()));
    }
    if let Some(failed) = error.downcast_ref::<jj::push::PushFailed>() {
        return RepoError::Other(failed.to_string());
    }
    if let Some(failed) = error.downcast_ref::<jj::push::SignFailed>() {
        return RepoError::Other(failed.to_string());
    }
    if let Some(revset) = error.downcast_ref::<crate::revset::RevsetError>() {
        return RepoError::Revset(revset.to_string());
    }
    let message = format!("{error:#}");
    if message.contains("failed to parse revset")
        || message.contains("failed to resolve")
        || message.contains("Revision") && message.contains("doesn't exist")
    {
        return RepoError::Revset(message);
    }
    if message.contains("failed to lock jj working copy") {
        return RepoError::Lock;
    }
    if message.contains("stale") {
        return RepoError::StaleWorkspace(message);
    }
    RepoError::Other(message)
}

/// The revisions a mutation would rewrite, named once so the actor's guard and
/// the frontend's confirmation dialog can never disagree about what an
/// operation touches.
///
/// `root` is dropped unconditionally: the root commit is refused by
/// [`ensure_rewritable`](crate::jj::mutate::ensure_rewritable) whatever the
/// caller passes, so listing it in a dialog offering "rewrite anyway" would
/// promise something the backend will not do. An `Edit` of the commit already
/// checked out short-circuits, because moving the working copy to where it
/// already is rewrites nothing.
pub fn rewritten_targets(
    op: &MutationOp,
    current: &RevisionSelection,
    root: Option<&str>,
) -> Vec<RevisionSelection> {
    use crate::mutations::{Destination, SquashTarget};

    let is_root = |selection: &RevisionSelection| match (selection, root) {
        (RevisionSelection::Commit(hex), Some(root)) => hex == root,
        _ => false,
    };
    let mut targets: Vec<RevisionSelection> = Vec::new();
    let mut push = |selection: &RevisionSelection| {
        if !is_root(selection) && !targets.contains(selection) {
            targets.push(selection.clone());
        }
    };
    match op {
        MutationOp::Edit { target } => {
            if target != current {
                push(target);
            }
        }
        MutationOp::Describe { target, .. } => push(target),
        MutationOp::Abandon { targets: picked } => picked.iter().for_each(&mut push),
        MutationOp::Rebase {
            sources,
            destination,
            ..
        } => {
            sources.iter().for_each(&mut push);
            match destination {
                // The target itself gains a parent (is rewritten).
                Destination::Before(target) => push(target),
                // The gap's child side gains a parent.
                Destination::Between { child, .. } => push(child),
                // `Onto` rewrites only the sources; `After` rewrites the
                // target's children, which nobody can enumerate up front.
                Destination::Onto(_) | Destination::After(_) | Destination::Trunk => {}
            }
        }
        MutationOp::Squash { from, into } => {
            from.iter().for_each(&mut push);
            if let SquashTarget::Revision(target) = into {
                push(target);
            }
        }
        MutationOp::Absorb { from, .. } => push(from),
        MutationOp::Commit { .. } => push(&RevisionSelection::WorkingCopy),
        MutationOp::NewAt { destination } => match destination {
            // The insertion's far side gains the new commit as a parent.
            Destination::Before(target) => push(target),
            Destination::Between { child, .. } => push(child),
            Destination::Onto(_) | Destination::After(_) | Destination::Trunk => {}
        },
        MutationOp::Parallelize { targets: picked }
        | MutationOp::Metaedit {
            targets: picked, ..
        } => picked.iter().for_each(&mut push),
        MutationOp::DuplicateTo { destination, .. } => match destination {
            Some(Destination::Before(target)) => push(target),
            Some(Destination::Between { child, .. }) => push(child),
            _ => {}
        },
        // The copies to abandon are resolved in the backend; its guard
        // covers them.
        MutationOp::KeepDivergent { .. } => {}
        MutationOp::Split { target, .. }
        | MutationOp::RestoreChanges { target, .. }
        | MutationOp::ResolveConflict { target, .. } => push(target),
        MutationOp::SquashChanges { from, into, .. } => {
            push(from);
            if let SquashTarget::Revision(target) = into {
                push(target);
            }
        }
        // New/Merge/Duplicate only create commits; bookmark and undo ops
        // rewrite nothing the caller named.
        MutationOp::New { .. }
        | MutationOp::Merge { .. }
        | MutationOp::Duplicate { .. }
        | MutationOp::MoveBookmark { .. }
        | MutationOp::DeleteBookmark { .. }
        | MutationOp::TrackBookmark { .. }
        | MutationOp::PushBookmark { .. }
        | MutationOp::Undo { .. }
        | MutationOp::Redo
        | MutationOp::CreateBookmark { .. }
        | MutationOp::RenameBookmark { .. }
        | MutationOp::ForgetBookmark { .. }
        | MutationOp::UntrackBookmark { .. }
        | MutationOp::Push(_)
        | MutationOp::RestoreOperation { .. }
        | MutationOp::RevertOperation { .. } => {}
    }
    targets
}

/// A hex that names the working-copy row resolves to
/// [`RevisionSelection::WorkingCopy`], not to a commit that happens to be `@`.
///
/// Two things depended on this and got it wrong: squashing into `@` left the
/// selection pointing at the pre-squash commit id, which no longer existed, and
/// the palette addressed `@` as an ordinary commit so its row never highlighted.
pub fn canonical_selection(hex: &str, working_copy: Option<&str>) -> RevisionSelection {
    match working_copy {
        Some(wc) if wc == hex => RevisionSelection::WorkingCopy,
        _ => RevisionSelection::Commit(hex.to_owned()),
    }
}
