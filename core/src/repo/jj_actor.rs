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
use super::protocol::{
    Command, Event, GraphTail, JobId, Payload, PreviewRequest, RepoError, RepoId,
};
use super::{Envelope, Jobs, SettingsSource, route_commands};
use crate::jj;
use crate::model::{LoadProgress, RevisionSelection, StreamRow};
use crate::mutations::{DraftSimulation, MutationOp};
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
            watch(watch_root, poke);
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
                        jobs,
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

/// Turn the filesystem watch into a poke on the actor's own channel, so the
/// actor decides what a change means (dedup on the op head, never snapshot
/// mid-mutation) rather than the frontend guessing from the outside.
fn watch(root: PathBuf, poke: mpsc::UnboundedSender<Envelope>) {
    let spawned = std::thread::Builder::new()
        .name("diffui-repo-watch".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build()
            else {
                return;
            };
            runtime.block_on(async move {
                let mut watcher = match crate::watcher::RepoWatcher::start(&root) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        tracing::warn!(
                            root = %root.display(),
                            %error,
                            "filesystem watcher unavailable; auto-refresh is off for this repository"
                        );
                        return;
                    }
                };
                loop {
                    tokio::select! {
                        batch = watcher.next_batch() => match batch {
                            Some(batch) => {
                                if poke.send(Envelope::Watch(batch)).is_err() {
                                    return;
                                }
                            }
                            None => return,
                        },
                        // The actor shut down; stop watching with it.
                        () = poke.closed() => return,
                    }
                }
            });
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "failed to spawn the repository watcher thread");
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
    jobs: Jobs,
}

impl Actor {
    /// Start `job`, picking up a cancel that arrived before its turn came.
    fn begin(&self, job: JobId) -> CancelFlag {
        match self.jobs.lock() {
            Ok(mut jobs) => jobs.begin(job),
            Err(_) => CancelFlag::default(),
        }
    }

    fn finish(&self, job: JobId) {
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.finish(job);
        }
    }

    async fn run(&mut self, mut commands: mpsc::UnboundedReceiver<Envelope>) {
        while let Some(envelope) = commands.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                Envelope::Watch(batch) => self.on_watch(batch).await,
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
                    let cancel = self.begin(job);
                    let span = tracing::info_span!(
                        "repo_command",
                        repo = %self.id,
                        job = job.0,
                        command = command_name(&command),
                    );
                    let _entered = span.enter();
                    let result = self.dispatch(&repository, command, &cancel).await;
                    if let Err(error) = result {
                        let error = classify(error);
                        tracing::warn!(%error, "command failed");
                        self.emit(Payload::Failed { job, error });
                    }
                    self.finish(job);
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
        let root = slot.workspace.workspace_root().to_owned();
        match jj::read_op_head_with(&slot.settings, &root).await {
            Ok(fingerprint) => {
                if self.op_head.as_deref() != Some(fingerprint.as_str()) {
                    self.op_head = Some(fingerprint.clone());
                    self.emit(Payload::OpHeadChanged { fingerprint });
                }
            }
            Err(error) => tracing::warn!(%error, "failed to read the jj op head"),
        }
    }

    fn emit(&self, payload: Payload) {
        let _ = self.events.send(Event::new(self.id.clone(), payload));
    }

    /// Open (once) the workspace `repository` names.
    fn slot(&mut self, repository: &Repository) -> Result<&mut WorkspaceSlot> {
        if self.workspace.is_none() {
            let root = &repository.root;
            let settings = match &self.settings {
                SettingsSource::Layered => jj::settings::jj_settings(root)?,
                SettingsSource::Fixed(settings) => (**settings).clone(),
            };
            let workspace = jj::load_workspace(&settings, root)?;
            let name = workspace.workspace_name().to_owned();
            let snapshot = jj::SnapshotContext::load(&settings, root)?;
            self.workspace = Some(WorkspaceSlot {
                workspace,
                settings,
                snapshot,
                name,
            });
        }
        Ok(self
            .workspace
            .as_mut()
            .expect("the workspace was just opened"))
    }

    /// The repo at head, reloading only when the operation head has moved.
    /// The op-heads read is a bare readdir, so paying it before every read is
    /// far cheaper than re-reading the commit index would be.
    async fn repo_at_head(&mut self, repository: &Repository) -> Result<Arc<ReadonlyRepo>> {
        let root = repository.root.clone();
        let settings = self.slot(repository)?.settings.clone();
        // A bare readdir of `.jj/repo/op_heads/heads` — cheap enough to pay
        // before every read, and far cheaper than re-reading the commit index
        // would be.
        let fingerprint = jj::read_op_head_with(&settings, &root).await.ok();
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
        self.op_head = Some(repo.op_id().hex());
        self.repo = Some(repo.clone());
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
    /// The cached repo is a generation behind, so it goes; the op head is read
    /// afresh so the watcher's next signal dedups against *our* write. Leaving
    /// it stale made every mutation and fetch look like an external operation,
    /// which cost a second lock-and-snapshot that could only report "already
    /// up to date".
    async fn adopt_written_op(&mut self, repository: &Repository) {
        self.repo = None;
        let Ok(slot) = self.slot(repository) else {
            self.op_head = None;
            return;
        };
        let (settings, root) = (
            slot.settings.clone(),
            slot.workspace.workspace_root().to_owned(),
        );
        self.op_head = jj::read_op_head_with(&settings, &root).await.ok();
    }

    async fn dispatch(
        &mut self,
        repository: &Repository,
        command: Command,
        cancel: &CancelFlag,
    ) -> Result<()> {
        match command {
            Command::LoadGraph { job, revset } => {
                self.load_graph(repository, job, revset, cancel).await
            }
            Command::LoadDiff { job, revision } => self.load_diff(repository, job, revision).await,
            Command::Snapshot { job, origin } => {
                let slot = self.slot(repository)?;
                let outcome = {
                    let WorkspaceSlot {
                        workspace,
                        snapshot,
                        ..
                    } = slot;
                    jj::snapshot_working_copy(workspace, snapshot).await?
                };
                self.adopt(outcome.repo.clone());
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
            } => self.mutate(repository, job, &op, allow_immutable).await,
            Command::Preview { job, draft } => self.preview(repository, job, draft).await,
            Command::Fetch { job, target } => {
                let progress = LoadProgress::default();
                let slot = self.slot(repository)?;
                let output = {
                    let WorkspaceSlot {
                        workspace,
                        settings,
                        snapshot,
                        ..
                    } = slot;
                    jj::fetch_jj(workspace, settings, snapshot, &target, &progress).await?
                };
                self.adopt_written_op(repository).await;
                self.emit(Payload::FetchDone { job, output });
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
                let details = jj::jj_revision_details(repo.as_ref(), &commit);
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
            } => {
                let repo = self.repo_at_head(repository).await?;
                let name = self.slot(repository)?.name.clone();
                let (old, new) = jj::read_jj_file_pair_inner(
                    repo.as_ref(),
                    &name,
                    &revision,
                    &path,
                    old_path.as_deref(),
                )
                .await?;
                self.emit(Payload::FilePairRead {
                    job,
                    path,
                    old,
                    new,
                });
                Ok(())
            }
            Command::BookmarkCheck { job, name, to } => {
                let repo = self.repo_at_head(repository).await?;
                let workspace_name = self.slot(repository)?.name.clone();
                let backwards =
                    jj::check_bookmark_move_backwards(repo.as_ref(), &workspace_name, &name, &to)
                        .await?;
                self.emit(Payload::BookmarkChecked { job, backwards });
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
    ) -> Result<()> {
        let repo = self.repo_at_head(repository).await?;
        let name = self.slot(repository)?.name.clone();
        let commit_id = jj::resolve_revision(repo.as_ref(), &name, &revision).await?;
        let (document, details) =
            jj::diff_jj_with_repo(repo.as_ref(), &commit_id, repository).await?;
        self.emit(Payload::DiffLoaded {
            job,
            revision,
            document,
            details,
        });
        Ok(())
    }

    async fn mutate(
        &mut self,
        repository: &Repository,
        job: JobId,
        op: &MutationOp,
        allow_immutable: bool,
    ) -> Result<()> {
        let progress = LoadProgress::default();
        let slot = self.slot(repository)?;
        let outcome = {
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
            )
            .await?
        };
        self.adopt_written_op(repository).await;
        self.emit(Payload::MutationDone { job, outcome });
        Ok(())
    }

    async fn preview(
        &mut self,
        repository: &Repository,
        job: JobId,
        draft: PreviewRequest,
    ) -> Result<()> {
        let repo = self.repo_at_head(repository).await?;
        let (settings, name) = {
            let slot = self.slot(repository)?;
            (slot.settings.clone(), slot.name.clone())
        };
        let simulation = match draft {
            PreviewRequest::Merge { parents } => {
                DraftSimulation::Merge(jj::preview_merge(&repo, &name, &parents).await?)
            }
            PreviewRequest::Rebase {
                mode,
                sources,
                destination,
            } => DraftSimulation::Rebase(
                jj::preview_rebase(
                    &repo,
                    &settings,
                    repository,
                    &name,
                    mode,
                    &sources,
                    &destination,
                )
                .await?,
            ),
        };
        self.emit(Payload::PreviewDone { job, simulation });
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
        Command::Cancel { .. } => "cancel",
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
        return RepoError::StaleWorkspace;
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
                Destination::Onto(_) | Destination::After(_) => {}
            }
        }
        MutationOp::Squash { from, into } => {
            from.iter().for_each(&mut push);
            if let SquashTarget::Revision(target) = into {
                push(target);
            }
        }
        MutationOp::Absorb { from } => push(from),
        // New/Merge/Duplicate only create commits; bookmark and undo ops
        // rewrite nothing the caller named.
        MutationOp::New { .. }
        | MutationOp::Merge { .. }
        | MutationOp::Duplicate { .. }
        | MutationOp::MoveBookmark { .. }
        | MutationOp::DeleteBookmark { .. }
        | MutationOp::TrackBookmark { .. }
        | MutationOp::PushBookmark { .. }
        | MutationOp::Undo { .. } => {}
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
