//! The git repository actor.
//!
//! git shells out, so there is no `!Send` state to pin to a thread and no
//! working-copy lock to serialize on — this runs as a tokio task. It speaks
//! the same protocol as [`super::jj_actor`] and answers `Unsupported` for the
//! capabilities [`super::Capabilities`] already told the caller it lacks, so a
//! frontend never has to ask which backend it is talking to.

use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, GraphTail, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands};
use crate::repository::Repository;

pub(super) fn spawn(
    id: RepoId,
    _root: std::path::PathBuf,
    commands: mpsc::UnboundedReceiver<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    // The reader runs as its own task so a cancel is applied while the job it
    // names is still running, rather than after it (see `route_commands`).
    let jobs: Jobs = Jobs::default();
    let (work_tx, mut work) = mpsc::unbounded_channel();
    tokio::spawn(route_commands(commands, work_tx, jobs.clone()));
    tokio::spawn(async move {
        while let Some(envelope) = work.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                // git has no operation log, so there is nothing to dedup and
                // nothing to report but the edit itself.
                Envelope::Watch(batch) => {
                    if batch.worktree {
                        let _ = events.send(Event::new(id.clone(), Payload::WorkingCopyChanged));
                    }
                }
                Envelope::Run {
                    repository,
                    command,
                } => {
                    let Some(job) = command.job() else { continue };
                    let cancel = match jobs.lock() {
                        Ok(mut jobs) => jobs.begin(job),
                        Err(_) => CancelFlag::default(),
                    };
                    let span = tracing::info_span!("repo_command", repo = %id, job = job.0);
                    let _entered = span.enter();
                    for payload in run(&repository, command, &cancel).await {
                        let _ = events.send(Event::new(id.clone(), payload));
                    }
                    if let Ok(mut jobs) = jobs.lock() {
                        jobs.finish(job);
                    }
                }
            }
            if events.is_closed() {
                return;
            }
        }
    });
}

async fn run(repository: &Repository, command: Command, cancel: &CancelFlag) -> Vec<Payload> {
    /// Every failure git reports is prose from a subprocess; there is no
    /// typed rejection to recover, so they all land in one arm.
    fn failed(job: super::JobId, error: anyhow::Error) -> Vec<Payload> {
        vec![Payload::Failed {
            job,
            error: RepoError::Other(format!("{error:#}")),
        }]
    }

    match command {
        Command::LoadGraph { job, revset } => {
            match crate::git::load_git_commits(repository, &revset).await {
                Ok(_) if cancel.is_cancelled() => vec![Payload::Cancelled { job }],
                // The git loader parses `git log` in one shot rather than per
                // commit, so the "stream" is a single batch — which the
                // projection folds exactly like a jj one. Ahead/behind and
                // bookmarks aren't wired for git, so the tail is empty.
                Ok(rows) => vec![
                    Payload::Batch { job, rows },
                    Payload::GraphLoaded {
                        job,
                        tail: GraphTail::default(),
                    },
                ],
                Err(error) => failed(job, error),
            }
        }
        Command::LoadDiff { job, revision } => {
            match crate::git::load_git_diff(repository, &revision).await {
                Ok((document, details)) => vec![Payload::DiffLoaded {
                    job,
                    revision,
                    document,
                    details,
                }],
                Err(error) => failed(job, error),
            }
        }
        Command::Snapshot { job, origin } => {
            match crate::git::load_git_repository_snapshot(&repository.root).await {
                Ok(snapshot) => vec![Payload::SnapshotDone {
                    job,
                    origin,
                    snapshot,
                    warnings: Vec::new(),
                }],
                Err(error) => failed(job, error),
            }
        }
        Command::Fetch { job, target } => match crate::git::fetch_git(repository, &target).await {
            Ok(output) => vec![Payload::FetchDone { job, output }],
            Err(error) => failed(job, error),
        },
        Command::ListTree { job, revision } => {
            match crate::git::list_git_source_tree(repository, &revision).await {
                Ok(entries) => vec![Payload::TreeListed {
                    job,
                    revision,
                    entries: crate::source_browse::sort_source_entries(entries),
                }],
                Err(error) => failed(job, error),
            }
        }
        Command::ReadFile {
            job,
            revision,
            path,
        } => match crate::git::read_git_source_file(repository, &revision, &path).await {
            Ok(data) => vec![Payload::FileRead {
                job,
                revision,
                file: crate::source_browse::build_source_file(&path, data),
                path,
            }],
            Err(error) => failed(job, error),
        },
        Command::FilePair {
            job,
            revision,
            path,
            old_path,
        } => {
            let (old, new) =
                crate::git::read_git_file_pair(repository, &revision, &path, old_path.as_deref())
                    .await;
            vec![Payload::FilePairRead {
                job,
                path,
                old,
                new,
            }]
        }
        // Emptiness is cosmetic and the git loader never populates it, so an
        // empty answer is the honest one rather than an error.
        Command::EmptyStatus { job, .. } => vec![Payload::EmptyStatus {
            job,
            updates: Vec::new(),
        }],
        Command::Mutate { job, .. }
        | Command::Preview { job, .. }
        | Command::RevisionDetails { job, .. }
        | Command::BookmarkCheck { job, .. } => vec![Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }],
        Command::Cancel { .. } => unreachable!("cancels are handled before dispatch"),
    }
}
