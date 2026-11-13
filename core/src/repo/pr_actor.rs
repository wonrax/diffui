//! The GitHub pull-request actor.
//!
//! A PR has no checkout, no operation log and nothing to mutate, so this is a
//! plain tokio task over the `gh` CLI. It speaks the protocol so a PR tab and
//! a repository tab are the same thing to the frontend: the diff streams back
//! as `Batch`-shaped work, and the commit list arrives as graph rows.

use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, GraphTail, JobId, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands, run_job};
use crate::github::{self, PrSpec};
use crate::model::{DiffBase, DiffDocument, DiffFile, RevisionSelection};

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
        // The PR header, read once: the details pane and the diff's totals
        // both start from it. A graph load (opening or refreshing the tab)
        // reads it afresh.
        let mut info: Option<github::PrInfo> = None;
        while let Some(envelope) = work.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                // Nothing on the local filesystem describes a pull request.
                Envelope::Watch(_) => {}
                Envelope::Run { command, .. } => {
                    let Some(job) = command.job() else { continue };
                    let span = tracing::info_span!("pr_command", repo = %id, job = job.0);
                    let emit = |payload| {
                        let _ = events.send(Event::new(id.clone(), payload));
                    };
                    let (id, spec, events, info) = (&id, &spec, &events, &mut info);
                    run_job(&jobs, job, span, emit, |ticket| async move {
                        run(id, spec, info, *command, ticket.flag(), events).await;
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

async fn run(
    id: &RepoId,
    spec: &PrSpec,
    info: &mut Option<github::PrInfo>,
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
                *info = None;
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
        Command::LoadDiff {
            job,
            revision,
            base: DiffBase::Parents,
            ..
        } => {
            stream_diff(spec, info, job, revision, cancel, &emit).await;
        }
        // A comparison needs two local trees; `Capabilities::compare` is off.
        Command::LoadDiff { job, .. } => emit(Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }),
        Command::RevisionDetails { job, revision } => match pr_info(spec, info).await {
            Ok(info) => emit(Payload::DetailsLoaded {
                job,
                revision,
                details: github::pr_revision_details(info),
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
        | Command::BookmarkCheck { job, .. }
        | Command::ConflictInfo { job, .. }
        | Command::PrepareMergeTool { job, .. }
        | Command::ConflictText { job, .. }
        | Command::History { job, .. }
        | Command::ExpandFile { job, .. }
        | Command::ResolveRevset { job, .. }
        | Command::ResolveDiff { job, .. }
        | Command::RunCli { job, .. } => emit(Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }),
        Command::Cancel { .. } | Command::SetRefreshMode { .. } => {
            unreachable!("jobless commands are handled before dispatch")
        }
    }
}

/// The PR header, read once and kept in `info`.
async fn pr_info<'a>(
    spec: &PrSpec,
    info: &'a mut Option<github::PrInfo>,
) -> anyhow::Result<&'a github::PrInfo> {
    if info.is_none() {
        *info = Some(github::fetch_pr_info(spec).await?);
    }
    Ok(info.as_ref().expect("just filled"))
}

/// Stream the whole-PR diff (or one commit's) back as batches. A cancel kills
/// the `gh` download rather than leaving it to run to completion against a
/// tab the user has already navigated away from.
async fn stream_diff(
    spec: &PrSpec,
    info: &mut Option<github::PrInfo>,
    job: JobId,
    revision: RevisionSelection,
    cancel: &CancelFlag,
    emit: &impl Fn(Payload),
) {
    if let RevisionSelection::Commit(oid) = &revision {
        match github::load_pr_commit_diff(spec, oid, cancel).await {
            _ if cancel.is_cancelled() => emit(Payload::Cancelled { job }),
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
    let header = pr_info(spec, info)
        .await
        .ok()
        .map(|info| (info.additions, info.deletions));
    let mut batch: Vec<DiffFile> = Vec::new();
    let mut batch_lines = 0usize;
    let summed = header.is_none();
    let mut totals = header.unwrap_or((0, 0));
    let result = github::stream_pr_diff(spec, cancel, |file| {
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
                    range: None,
                },
                details: None,
            });
            batch_lines = 0;
        }
    })
    .await;

    match result {
        _ if cancel.is_cancelled() => emit(Payload::Cancelled { job }),
        Ok(()) => emit(Payload::DiffLoaded {
            job,
            revision,
            document: DiffDocument {
                files: batch,
                total_additions: totals.0,
                total_deletions: totals.1,
                range: None,
            },
            details: None,
        }),
        Err(error) => emit(Payload::Failed {
            job,
            error: RepoError::Other(format!("{error:#}")),
        }),
    }
}
