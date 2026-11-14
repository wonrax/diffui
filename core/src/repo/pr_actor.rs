//! The pull-request actor.
//!
//! A PR has no checkout, no operation log and nothing to mutate, so this is a
//! plain tokio task over its forge's [`ForgeProvider`]. It speaks the protocol so a PR tab and
//! a repository tab are the same thing to the frontend: the diff streams back
//! as `Batch`-shaped work, and the commit list arrives as graph rows.

use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, GraphTail, JobId, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands, run_job};
use crate::forge::{self, ForgePr, ForgeProvider, Forges, PrInfo};
use crate::model::{DiffBase, DiffDocument, DiffFile, RevisionSelection};

/// Flush the pending file batch once it holds this many diff lines. Small
/// enough that the first screenful paints quickly, large enough that a
/// million-line PR doesn't flood the channel with per-file messages.
const BATCH_LINE_LIMIT: usize = 4_096;

pub(super) fn spawn(
    id: RepoId,
    spec: ForgePr,
    commands: mpsc::UnboundedReceiver<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    // The reader runs as its own task so a cancel reaches a download that is
    // still streaming, which is the whole point of cancelling one.
    let jobs: Jobs = Jobs::default();
    let (work_tx, mut work) = mpsc::unbounded_channel();
    tokio::spawn(route_commands(commands, work_tx, jobs.clone()));
    tokio::spawn(async move {
        let Some(provider) = Forges::builtin().provider(spec.kind) else {
            return;
        };
        let provider = provider.as_ref();
        // The PR header, read once: the details pane and the diff's totals
        // both start from it. A graph load (opening or refreshing the tab)
        // reads it afresh.
        let mut info: Option<PrInfo> = None;
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
                        run(id, provider, spec, info, *command, ticket.flag(), events).await;
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
    provider: &dyn ForgeProvider,
    spec: &ForgePr,
    info: &mut Option<PrInfo>,
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
        Command::LoadGraph { job, .. } => match provider.commits(spec.clone()).await {
            Ok(commits) => {
                *info = None;
                emit(Payload::Batch {
                    job,
                    rows: forge::pr_commit_rows(&commits),
                });
                emit(Payload::GraphLoaded {
                    job,
                    tail: GraphTail::default(),
                });
            }
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(error.to_string()),
            }),
        },
        Command::LoadDiff {
            job,
            revision,
            base: DiffBase::Parents,
            ..
        } => {
            stream_diff(id, provider, spec, info, job, revision, cancel, events).await;
        }
        // A comparison needs two local trees; `Capabilities::compare` is off.
        Command::LoadDiff { job, .. } => emit(Payload::Failed {
            job,
            error: RepoError::Unsupported,
        }),
        Command::RevisionDetails { job, revision } => match pr_info(provider, spec, info).await {
            Ok(info) => emit(Payload::DetailsLoaded {
                job,
                revision,
                details: forge::pr_revision_details(spec.kind, info),
            }),
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(error.to_string()),
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
    provider: &dyn ForgeProvider,
    spec: &ForgePr,
    info: &'a mut Option<PrInfo>,
) -> forge::ForgeResult<&'a PrInfo> {
    if info.is_none() {
        *info = Some(provider.view(spec.clone()).await?);
    }
    Ok(info.as_ref().expect("just filled"))
}

/// Stream the whole-PR diff back in parts (or one commit's, whole). A cancel
/// kills the `gh` download rather than leaving it to run to completion
/// against a tab the user has already navigated away from.
#[allow(clippy::too_many_arguments)]
async fn stream_diff(
    id: &RepoId,
    provider: &dyn ForgeProvider,
    spec: &ForgePr,
    info: &mut Option<PrInfo>,
    job: JobId,
    revision: RevisionSelection,
    cancel: &CancelFlag,
    events: &mpsc::UnboundedSender<Event>,
) {
    let emit = |payload: Payload| {
        let _ = events.send(Event::new(id.clone(), payload));
    };
    if let RevisionSelection::Commit(oid) = &revision {
        match provider
            .commit_diff(spec.clone(), oid.clone(), cancel.clone())
            .await
        {
            _ if cancel.is_cancelled() => emit(Payload::Cancelled { job }),
            Ok((document, details)) => emit(Payload::DiffLoaded {
                job,
                revision,
                document,
                details,
            }),
            Err(error) => emit(Payload::Failed {
                job,
                error: RepoError::Other(error.to_string()),
            }),
        }
        return;
    }

    // The PR header's own counts, read first: the files-API fallback zeroes
    // per-file counts for oversized blobs, so summing the parsed files
    // undercounts (react#36173: 73k summed against 123k real).
    let header = pr_info(provider, spec, info)
        .await
        .ok()
        .and_then(|info| info.totals);
    let mut batch: Vec<DiffFile> = Vec::new();
    let mut batch_lines = 0usize;
    let summed = header.is_none();
    let mut totals = header.unwrap_or((0, 0));
    let on_file = Box::new(|file: DiffFile| {
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
            emit(Payload::DiffPart {
                job,
                files: std::mem::take(&mut batch),
                total_additions: totals.0,
                total_deletions: totals.1,
            });
            batch_lines = 0;
        }
    });
    let result = provider
        .stream_diff(spec.clone(), cancel.clone(), on_file)
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
            error: RepoError::Other(error.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{
        Checks, CreatePr, ForgeFuture, ForgeKind, ForgeRemote, OnFile, PullRequest, UpdatePr,
    };
    use crate::model::{DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind};
    use crate::session::Session;

    /// A forge whose pull request is `files`, each `lines` lines long, under
    /// a header that counts `totals`.
    struct Streams {
        files: Vec<&'static str>,
        lines: usize,
        totals: (usize, usize),
    }

    impl ForgeProvider for Streams {
        fn kind(&self) -> ForgeKind {
            ForgeKind::GitHub
        }

        fn claims(&self, _remote_url: &str) -> Option<ForgeRemote> {
            None
        }

        fn pull_requests(
            &self,
            _remote: ForgeRemote,
            _branches: Vec<String>,
        ) -> ForgeFuture<'_, Vec<PullRequest>> {
            unreachable!("a pull-request tab doesn't ask")
        }

        fn checks(&self, _remote: ForgeRemote, _sha: String) -> ForgeFuture<'_, Checks> {
            unreachable!("a pull-request tab doesn't ask")
        }

        fn create_pull_request(
            &self,
            _remote: ForgeRemote,
            _request: CreatePr,
        ) -> ForgeFuture<'_, PullRequest> {
            unreachable!("a pull-request tab doesn't ask")
        }

        fn update_pull_request(
            &self,
            _remote: ForgeRemote,
            _number: u64,
            _patch: UpdatePr,
        ) -> ForgeFuture<'_, PullRequest> {
            unreachable!("a pull-request tab doesn't ask")
        }

        fn view(&self, _pr: ForgePr) -> ForgeFuture<'_, PrInfo> {
            let totals = self.totals;
            let changed_files = self.files.len();
            Box::pin(async move {
                Ok(PrInfo {
                    number: 7,
                    title: "Big".to_owned(),
                    author: "someone".to_owned(),
                    state: "open".to_owned(),
                    base_ref: "main".to_owned(),
                    head_ref: "big".to_owned(),
                    totals: Some(totals),
                    changed_files,
                    url: String::new(),
                })
            })
        }

        fn stream_diff<'a>(
            &'a self,
            _pr: ForgePr,
            _cancel: CancelFlag,
            mut on_file: OnFile<'a>,
        ) -> ForgeFuture<'a, ()> {
            Box::pin(async move {
                for path in &self.files {
                    on_file(file(path, self.lines));
                }
                Ok(())
            })
        }
    }

    fn file(path: &str, lines: usize) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Added,
            hunks: vec![DiffHunkView {
                header: String::new(),
                lines: (0..lines)
                    .map(|line| DiffLine {
                        kind: DiffLineKind::Addition,
                        old_line: None,
                        new_line: Some(line + 1),
                        content: String::new(),
                        syntax: Vec::new(),
                        emphasis: Vec::new(),
                    })
                    .collect(),
            }],
            additions: lines,
            deletions: 0,
            collapsed: None,
            blob: None,
            mode_change: None,
        }
    }

    /// A pull request whose diff is longer than one batch reaches the tab
    /// whole: every file, in the tree's order, under the header's totals.
    #[tokio::test]
    async fn a_diff_longer_than_one_batch_lands_whole() {
        // Git's path order, which is how a forge sends them. Three files fill
        // a batch, so this is three batches and a last file.
        let provider = Streams {
            files: vec![
                "README.md",
                "a.rs",
                "src/lib.rs",
                "src/main.rs",
                "src/nested/a.rs",
                "src/nested/b.rs",
                "src/z.rs",
                "tests/t.rs",
                "zeta/deep/x.rs",
                "zz.rs",
            ],
            lines: BATCH_LINE_LIMIT / 3 + 1,
            totals: (123_000, 45),
        };
        let spec = ForgePr {
            kind: ForgeKind::GitHub,
            host: None,
            owner: "o".to_owned(),
            name: "r".to_owned(),
            number: 7,
        };
        let id = RepoId(spec.key());
        let mut session = Session::unloaded(String::new());
        let job = session
            .load_diff(RevisionSelection::WorkingCopy)
            .iter()
            .find_map(|effect| match effect {
                crate::session::Effect::Send(Command::LoadDiff { job, .. }) => Some(*job),
                _ => None,
            })
            .expect("a diff load");

        let (events, mut sent) = mpsc::unbounded_channel();
        stream_diff(
            &id,
            &provider,
            &spec,
            &mut None,
            job,
            RevisionSelection::WorkingCopy,
            &CancelFlag::default(),
            &events,
        )
        .await;
        drop(events);
        let mut count = 0;
        while let Some(event) = sent.recv().await {
            count += 1;
            session.apply(event);
            // Each batch shows as it arrives, and the load isn't over until
            // the last one.
            assert_eq!(session.diff_in_flight(), count < 4, "after event {count}");
            assert!(!session.document.files.is_empty());
        }
        assert_eq!(count, 4);

        let mut whole = DiffDocument {
            files: provider.files.iter().map(|path| file(path, 1)).collect(),
            ..DiffDocument::default()
        };
        whole.sort_in_tree_order();
        let paths = |document: &DiffDocument| -> Vec<String> {
            document
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect()
        };
        assert_ne!(
            paths(&whole)[0],
            "README.md",
            "the tree's order isn't git's"
        );
        assert_eq!(paths(&session.document), paths(&whole));
        assert_eq!(
            (
                session.document.total_additions,
                session.document.total_deletions
            ),
            provider.totals
        );
        assert_eq!(session.selected_revision, RevisionSelection::WorkingCopy);
    }
}
