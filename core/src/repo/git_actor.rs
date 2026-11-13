//! The git repository actor.
//!
//! git shells out, so there is no `!Send` state to pin to a thread and no
//! working-copy lock to serialize on — this runs as a tokio task. It speaks
//! the same protocol as [`super::jj_actor`] and answers `Unsupported` for the
//! capabilities [`super::Capabilities`] already told the caller it lacks, so a
//! frontend never has to ask which backend it is talking to.

use tokio::sync::mpsc;

/// Rows per `Batch`, as the jj walk streams them, so a long log is folded in
/// steps rather than in one message.
const BATCH_SIZE: usize = 256;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, GraphTail, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands, run_job};
use crate::repository::Repository;

pub(super) fn spawn(
    id: RepoId,
    root: std::path::PathBuf,
    commands: mpsc::UnboundedReceiver<Envelope>,
    poke: mpsc::UnboundedSender<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    super::watch(root, poke);
    // The reader runs as its own task so a cancel is applied while the job it
    // names is still running, rather than after it (see `route_commands`).
    let jobs: Jobs = Jobs::default();
    let (work_tx, mut work) = mpsc::unbounded_channel();
    tokio::spawn(route_commands(commands, work_tx, jobs.clone()));
    tokio::spawn(async move {
        while let Some(envelope) = work.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                // A save in the worktree changes what the working-copy diff
                // shows, not the history, so it asks for a snapshot rather
                // than a reload (`Session` escalates one that finds the
                // working copy turned clean or dirty). Anything under `.git`
                // may have moved refs.
                Envelope::Watch(batch) => {
                    let payload = if batch.git_metadata {
                        Payload::RepositoryChanged
                    } else if batch.worktree {
                        Payload::WorkingCopyChanged
                    } else {
                        continue;
                    };
                    let _ = events.send(Event::new(id.clone(), payload));
                }
                Envelope::Run {
                    repository,
                    command,
                } => {
                    let Some(job) = command.job() else { continue };
                    let span = tracing::info_span!("repo_command", repo = %id, job = job.0);
                    let emit = |payload| {
                        let _ = events.send(Event::new(id.clone(), payload));
                    };
                    run_job(&jobs, job, span, emit, |ticket| async move {
                        #[cfg(feature = "fault-injection")]
                        super::fault::trip(&repository.root);
                        for payload in run(&repository, *command, ticket.flag()).await {
                            emit(payload);
                        }
                        Ok(())
                    })
                    .await;
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
                // The git loader parses `git log` in one shot (lanes need to
                // know which parents the listing contains), so the batches
                // are cut afterwards. Ahead/behind and bookmarks aren't wired
                // for git, so the tail is empty.
                Ok(rows) => {
                    let mut payloads: Vec<Payload> =
                        Vec::with_capacity(rows.len() / BATCH_SIZE + 2);
                    let mut rows = rows.into_iter().peekable();
                    while rows.peek().is_some() {
                        payloads.push(Payload::Batch {
                            job,
                            rows: rows.by_ref().take(BATCH_SIZE).collect(),
                        });
                    }
                    payloads.push(Payload::GraphLoaded {
                        job,
                        tail: GraphTail::default(),
                    });
                    payloads
                }
                Err(error) => failed(job, error),
            }
        }
        // Comparisons and ranges are jj-only for now; `Capabilities::compare`
        // and `range_diff` are off.
        Command::LoadDiff { job, base, .. } if base.is_compare() => {
            vec![Payload::Failed {
                job,
                error: RepoError::Unsupported,
            }]
        }
        Command::LoadDiff {
            job,
            revision,
            expand,
            ..
        } => match crate::git::load_git_diff(repository, &revision, &expand).await {
            Ok((document, details)) => vec![Payload::DiffLoaded {
                job,
                revision,
                document,
                details,
            }],
            Err(error) => failed(job, error),
        },
        Command::ExpandFile { job, base, .. } if base.is_compare() => {
            vec![Payload::Failed {
                job,
                error: RepoError::Unsupported,
            }]
        }
        Command::ExpandFile {
            job,
            revision,
            path,
            ..
        } => match crate::git::load_git_file_diff(repository, &revision, &path).await {
            Ok(file) => vec![Payload::FileExpanded { job, path, file }],
            Err(error) => failed(job, error),
        },
        // No snapshot exists in git: every `DiskSync` reads the worktree.
        Command::Snapshot { job, origin, .. } => {
            match crate::git::load_git_repository_snapshot(&repository.root).await {
                Ok(snapshot) => vec![Payload::SnapshotDone {
                    job,
                    origin,
                    snapshot,
                    warnings: Vec::new(),
                    disk: Default::default(),
                }],
                Err(error) => failed(job, error),
            }
        }
        Command::Fetch { job, target } => match crate::git::fetch_git(repository, &target).await {
            Ok(output) => vec![Payload::FetchDone {
                job,
                outcome: crate::FetchOutcome {
                    remotes: Vec::new(),
                    changed: !output.is_empty(),
                    output,
                    warnings: Vec::new(),
                },
            }],
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
            ..
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
        | Command::BookmarkCheck { job, .. }
        | Command::ConflictInfo { job, .. }
        | Command::PrepareMergeTool { job, .. }
        | Command::ConflictText { job, .. }
        | Command::History { job, .. }
        | Command::ResolveRevset { job, .. }
        | Command::ResolveDiff { job, .. }
        | Command::RunCli { job, .. } => vec![Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }],
        Command::Cancel { .. } | Command::SetRefreshMode { .. } => {
            unreachable!("jobless commands are handled before dispatch")
        }
    }
}
