//! The GitHub pull-request actor.
//!
//! A PR has no checkout, no operation log and nothing to mutate, so this is a
//! plain tokio task over the `gh` CLI. It speaks the protocol so a PR tab and
//! a repository tab are the same thing to the frontend: the diff streams back
//! as `Batch`-shaped work, and the commit list arrives as graph rows.

use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, GraphTail, JobId, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands};
use crate::github::{self, PrSpec};
use crate::model::{DiffDocument, DiffFile, RevisionSelection};

/// Flush the pending file batch once it holds this many diff lines. Small
/// enough that the first screenful paints quickly, large enough that a
/// million-line PR doesn't flood the channel with per-file messages.
const BATCH_LINE_LIMIT: usize = 4_096;

pub(super) fn spawn(
    id: RepoId,
    spec: PrSpec,
    commands: mpsc::UnboundedReceiver<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    // The reader runs as its own task so a cancel reaches a download that is
    // still streaming, which is the whole point of cancelling one.
    let jobs: Jobs = Jobs::default();
    let (work_tx, mut work) = mpsc::unbounded_channel();
    tokio::spawn(route_commands(commands, work_tx, jobs.clone()));
    tokio::spawn(async move {
        while let Some(envelope) = work.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                // Nothing on the local filesystem describes a pull request.
                Envelope::Watch(_) => {}
                Envelope::Run { command, .. } => {
                    let Some(job) = command.job() else { continue };
                    let cancel = match jobs.lock() {
                        Ok(mut jobs) => jobs.begin(job),
                        Err(_) => CancelFlag::default(),
                    };
                    let span = tracing::info_span!("pr_command", repo = %id, job = job.0);
                    let _entered = span.enter();
                    run(&id, &spec, command, &cancel, &events).await;
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

async fn run(
    id: &RepoId,
    spec: &PrSpec,
    command: Command,
    cancel: &CancelFlag,
    events: &mpsc::UnboundedSender<Event>,
) {
    let emit = |payload: Payload| {
        let _ = events.send(Event::new(id.clone(), payload));
    };
    match command {
        // The PR's commit list is its graph: an "All changes" row on top, then
        // one row per commit. It arrives whole, in a single batch.
        Command::LoadGraph { job, .. } => match github::fetch_pr_commits(spec).await {
            Ok(commits) => {
                emit(Payload::Batch {
                    job,
                    rows: github::pr_commit_rows(&commits),
                });
                emit(Payload::GraphLoaded {
                    job,
                    tail: GraphTail::default(),
                });
            }
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(format!("{error:#}")),
            }),
        },
        Command::LoadDiff { job, revision } => {
            stream_diff(spec, job, revision, cancel, &emit).await;
        }
        Command::RevisionDetails { job, revision } => match github::fetch_pr_info(spec).await {
            Ok(info) => emit(Payload::DetailsLoaded {
                job,
                revision,
                details: github::pr_revision_details(&info),
            }),
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(format!("{error:#}")),
            }),
        },
        // A PR carries no working copy, no operations and no local tree, and
        // `Capabilities` says so — a frontend that asks anyway gets told.
        Command::Snapshot { job, .. }
        | Command::Mutate { job, .. }
        | Command::Preview { job, .. }
        | Command::Fetch { job, .. }
        | Command::EmptyStatus { job, .. }
        | Command::ListTree { job, .. }
        | Command::ReadFile { job, .. }
        | Command::FilePair { job, .. }
        | Command::BookmarkCheck { job, .. } => emit(Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }),
        Command::Cancel { .. } => unreachable!("cancels are handled before dispatch"),
    }
}

/// Stream the whole-PR diff (or one commit's) back as batches.
///
/// The `gh` child process is spawned with `kill_on_drop`, so cancelling here
/// actually stops the download instead of leaving it to run to completion
/// against a tab the user has already navigated away from.
async fn stream_diff(
    spec: &PrSpec,
    job: JobId,
    revision: RevisionSelection,
    cancel: &CancelFlag,
    emit: &impl Fn(Payload),
) {
    if let RevisionSelection::Commit(oid) = &revision {
        match github::load_pr_commit_diff(spec, oid).await {
            Ok((document, details)) => emit(Payload::DiffLoaded {
                job,
                revision,
                document,
                details,
            }),
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(error),
            }),
        }
        return;
    }

    // The PR header's own counts, read first: the files-API fallback zeroes
    // per-file counts for oversized blobs, so summing the parsed files
    // undercounts (react#36173: 73k summed against 123k real).
    let header = github::fetch_pr_info(spec).await.ok();
    let mut batch: Vec<DiffFile> = Vec::new();
    let mut batch_lines = 0usize;
    let mut totals = header
        .as_ref()
        .map(|info| (info.additions, info.deletions))
        .unwrap_or((0, 0));
    let summed = header.is_none();
    let mut cancelled = false;
    let result = github::stream_pr_diff(spec, |file| {
        // One flag check per file: a PR download that outlives its tab used to
        // run to completion, and a big one takes minutes.
        if cancelled || cancel.is_cancelled() {
            cancelled = true;
            return;
        }
        if summed {
            totals.0 += file.additions;
            totals.1 += file.deletions;
        }
        batch_lines += file
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len())
            .sum::<usize>();
        batch.push(file);
        if batch_lines >= BATCH_LINE_LIMIT {
            emit(Payload::DiffLoaded {
                job,
                revision: RevisionSelection::WorkingCopy,
                document: DiffDocument {
                    files: std::mem::take(&mut batch),
                    total_additions: totals.0,
                    total_deletions: totals.1,
                },
                details: None,
            });
            batch_lines = 0;
        }
    })
    .await;

    if cancelled {
        emit(Payload::Cancelled { job });
        return;
    }
    match result {
        Ok(()) => emit(Payload::DiffLoaded {
            job,
            revision,
            document: DiffDocument {
                files: batch,
                total_additions: totals.0,
                total_deletions: totals.1,
            },
            details: None,
        }),
        Err(error) => emit(Payload::Failed {
            job,
            error: RepoError::Other(format!("{error:#}")),
        }),
    }
}
