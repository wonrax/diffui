//! The wire between a repository actor and whoever is driving it.
//!
//! Everything here is plain owned data — `Clone + Send`, no borrows, no
//! trait objects — so a command or an event could be serialized across a
//! process boundary later without the shapes changing. Nothing *does* that
//! today; the point is only that the design doesn't foreclose it.

use std::fmt;
use std::path::PathBuf;

use crate::history::{HistoryRequest, HistoryResponse, OperationEntry};
use crate::model::{
    BookmarksInfo, BranchStatus, DiffBase, DiffDocument, DiffFile, RevisionDetails,
    RevisionSelection, StreamRow,
};
use crate::mutations::{
    Destination, DraftSimulation, MutationOp, MutationOutcome, RebaseSourceMode,
};
use crate::repository::{FetchTarget, RepositorySnapshot};
use crate::resolve::{ConflictFile, MergeToolInvocation};
use crate::revset::{RevsetError, RevsetMatch, RevsetQuery, RevsetSymbols};
use crate::session::RefreshOrigin;
use crate::source_browse::{SourceEntry, SourceFileLoad};

/// Identity of an open repository: the workspace root for a local repository,
/// the `owner/repo#number` reference for a GitHub pull request. One actor per
/// id — see [`repo_id`](super::repo_id) for why this is per workspace rather
/// than per `.jj/repo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(pub String);

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of one unit of work. Minted by the caller *before* the command is
/// sent, so the caller can park it in an inflight slot and match the events
/// that come back against it — that comparison is the whole staleness story.
///
/// Unique process-wide, not per session: two sessions over different
/// repositories would otherwise both be waiting on "job 3", and an event would
/// have no way to say which one it answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl JobId {
    /// The next job id. Monotonic, which the actor also relies on: a cancel for
    /// an id below everything it has started names a job that already ran.
    pub fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// What a repository can be asked to do, decided once when it is opened. This
/// replaces runtime downcasts to per-capability traits: a frontend reads the
/// flags to grey out an action instead of dispatching it and catching
/// [`RepoError::Unsupported`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Has a commit graph and a working copy to snapshot (jj, git).
    pub graph: bool,
    /// Can rewrite history (jj only).
    pub mutate: bool,
    /// Can fetch from git remotes.
    pub fetch: bool,
    /// Can list and read a revision's tree (the source browser).
    pub browse: bool,
    /// Can produce `jj show`-style revision headers.
    pub details: bool,
    /// Can diff against something other than the parents ([`DiffBase`]),
    /// and expand a file its diff collapsed for size.
    pub compare: bool,
}

/// A typed backend failure. `Result<_, String>` stops at the core surface:
/// the frontend needs to tell "the revset doesn't parse" (keep the graph, show
/// the revset error) from "this commit is immutable" (offer the override) from
/// "the walk died" (fail the tab), and prose can't be matched on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    /// The user's revset failed to parse or resolve.
    Revset(String),
    Io(String),
    /// The workspace's checkout is behind the repo and recovery failed.
    StaleWorkspace,
    /// The working-copy lock could not be taken.
    Lock,
    /// The op refused to rewrite an immutable commit; `short_id` names it so
    /// the frontend can offer a rerun with the guards off.
    Immutable {
        short_id: String,
    },
    /// The repository has no such capability (see [`Capabilities`]).
    Unsupported,
    Other(String),
    /// `jj git push`'s commit checks refused the push; the rejection lists
    /// each commit and carries the request for a rerun with allowances.
    PushRejected(Box<crate::push::PushRejection>),
    /// The actor panicked while running the command. It dropped whatever it
    /// had loaded and keeps serving, so the next command starts clean.
    Crashed(String),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revset(message) => write!(f, "{message}"),
            Self::Io(message) => write!(f, "{message}"),
            Self::StaleWorkspace => {
                f.write_str("the working copy is stale and could not be recovered automatically")
            }
            Self::Lock => f.write_str("another process holds the working-copy lock"),
            Self::Immutable { short_id } => write!(f, "{short_id} is immutable"),
            Self::Unsupported => f.write_str("this repository does not support that operation"),
            Self::Other(message) => write!(f, "{message}"),
            Self::PushRejected(rejection) => write!(f, "{rejection}"),
            Self::Crashed(message) => write!(
                f,
                "This repository's worker crashed: {message}. See the log \
                 (\"Reveal log file\") for details; refresh to retry."
            ),
        }
    }
}

impl std::error::Error for RepoError {}

/// What a preview simulates. Separate from [`MutationOp`] because a preview
/// resolves the same inputs without ever writing an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewRequest {
    Rebase {
        mode: RebaseSourceMode,
        sources: Vec<RevisionSelection>,
        destination: Destination,
    },
    Merge {
        parents: Vec<RevisionSelection>,
    },
    /// `jj parallelize` of the targets; answered rebase-shaped.
    Parallelize {
        targets: Vec<RevisionSelection>,
    },
}

/// Work asked of a repository. Every variant that does work carries a
/// [`JobId`]; the actor answers each with exactly one terminal event
/// (`Failed`, `Cancelled`, or the variant's own success).
#[derive(Debug, Clone)]
pub enum Command {
    /// Walk `revset` and stream the rows back as `Batch` events.
    LoadGraph {
        job: JobId,
        revset: String,
    },
    LoadDiff {
        job: JobId,
        revision: RevisionSelection,
        /// The "from" side; [`DiffBase::Parents`] for a plain revision diff.
        base: DiffBase,
        /// Paths to diff in full even past the size caps — files the user
        /// already expanded, kept expanded across a reload.
        expand: Vec<String>,
    },
    /// Fold the on-disk tree into `@`. `origin` rides along untouched so the
    /// projection knows how much to reload when the fingerprint moves.
    Snapshot {
        job: JobId,
        origin: RefreshOrigin,
    },
    Mutate {
        job: JobId,
        op: MutationOp,
        allow_immutable: bool,
    },
    Preview {
        job: JobId,
        draft: PreviewRequest,
    },
    Fetch {
        job: JobId,
        target: FetchTarget,
    },
    RevisionDetails {
        job: JobId,
        revision: RevisionSelection,
    },
    EmptyStatus {
        job: JobId,
        targets: Vec<(usize, String)>,
    },
    ListTree {
        job: JobId,
        revision: RevisionSelection,
    },
    ReadFile {
        job: JobId,
        revision: RevisionSelection,
        path: String,
    },
    /// Both full sides of one file, for full-context syntax highlighting (and,
    /// later, hunk selection). One command per file instead of one workspace
    /// open per file.
    FilePair {
        job: JobId,
        revision: RevisionSelection,
        path: String,
        old_path: Option<String>,
        /// What the old side is read from, matching the displayed diff.
        base: DiffBase,
    },
    /// Whether moving bookmark `name` to `to` is backwards or sideways — the
    /// move the jj CLI refuses without `--allow-backwards`.
    BookmarkCheck {
        job: JobId,
        name: String,
        to: RevisionSelection,
    },
    /// The conflicted paths of `revision`, with jj's labels for their sides
    /// — what the resolve actions are offered from.
    ConflictInfo {
        job: JobId,
        revision: RevisionSelection,
    },
    /// Write the merge tool's input files for `path` at `revision` and build
    /// its command line. Read-only; the frontend runs the tool and applies
    /// the result as a [`MutationOp::ResolveConflict`].
    PrepareMergeTool {
        job: JobId,
        revision: RevisionSelection,
        path: String,
    },
    /// A history view's read: the op log, an operation's changes, an
    /// evolog, an interdiff, or a file's annotation. See
    /// [`HistoryRequest`].
    History {
        job: JobId,
        request: HistoryRequest,
    },
    /// One file of a diff in full, past the size caps that collapsed it
    /// ([`crate::model::DiffFile::collapsed`]).
    ExpandFile {
        job: JobId,
        revision: RevisionSelection,
        base: DiffBase,
        path: String,
    },
    /// Parse and resolve a revset against the repository as it stands, with
    /// the aliases and workspace the graph walk uses. Answered by
    /// [`Payload::RevsetResolved`] even when the revset is bad: that is an
    /// answer, not a failure. Cheap, and meant to be sent on every debounced
    /// keystroke, cancelling the one before.
    ResolveRevset {
        job: JobId,
        query: RevsetQuery,
    },
    /// Ask the actor to stop `job`. A cancelled job reports
    /// [`Payload::Cancelled`] and never its success event, so the caller's
    /// inflight slot is always cleared by exactly one terminal event.
    Cancel {
        job: JobId,
    },
}

impl Command {
    /// The job this command runs under, or `None` for `Cancel` (which names
    /// someone else's job).
    pub fn job(&self) -> Option<JobId> {
        match self {
            Self::LoadGraph { job, .. }
            | Self::LoadDiff { job, .. }
            | Self::Snapshot { job, .. }
            | Self::Mutate { job, .. }
            | Self::Preview { job, .. }
            | Self::Fetch { job, .. }
            | Self::RevisionDetails { job, .. }
            | Self::EmptyStatus { job, .. }
            | Self::ListTree { job, .. }
            | Self::ReadFile { job, .. }
            | Self::FilePair { job, .. }
            | Self::BookmarkCheck { job, .. }
            | Self::ConflictInfo { job, .. }
            | Self::PrepareMergeTool { job, .. }
            | Self::History { job, .. }
            | Self::ExpandFile { job, .. }
            | Self::ResolveRevset { job, .. } => Some(*job),
            Self::Cancel { .. } => None,
        }
    }
}

/// The tail of a graph walk: what only the completed walk knows.
#[derive(Debug, Clone, Default)]
pub struct GraphTail {
    /// `(row index, is_empty)` for the single-parent commits the walk resolved
    /// in its final pass. Merges and roots stay unknown (see
    /// [`Command::EmptyStatus`]).
    pub empty_updates: Vec<(usize, bool)>,
    pub branch_status: Option<BranchStatus>,
    pub bookmarks: BookmarksInfo,
    /// The repository's root commit id, so a caller can exclude it from a set
    /// of rewrite targets before offering the operation. `None` for backends
    /// without one.
    pub root_commit_id: Option<String>,
    /// The operation the walk read (`RepositorySnapshot::fingerprint`'s
    /// form), so the projection knows exactly which op the rows on screen
    /// reflect. `None` for backends without an op log.
    pub op_id: Option<String>,
    /// Revset aliases and workspace names, for revset completion. Empty for
    /// backends without revsets.
    pub symbols: RevsetSymbols,
}

/// One event from a repository actor. `repo` rides on every one of them, so a
/// toast, an activity row or a log line can name the repository even when the
/// job it belongs to has already been forgotten.
#[derive(Debug, Clone)]
pub struct Event {
    pub repo: RepoId,
    pub payload: Payload,
}

impl Event {
    pub fn new(repo: RepoId, payload: Payload) -> Self {
        Self { repo, payload }
    }

    /// The job this event answers, or `None` for the unsolicited ones
    /// (`Ready`, `OpHeadChanged`).
    pub fn job(&self) -> Option<JobId> {
        self.payload.job()
    }
}

#[derive(Debug, Clone)]
pub enum Payload {
    /// The actor is up. The only variant that isn't plain data: `handle` is a
    /// channel sender, which is how the in-process bootstrap works. Across a
    /// process boundary this is where a connect handshake would go.
    Ready {
        handle: super::RepoHandle,
        capabilities: Capabilities,
    },
    Batch {
        job: JobId,
        rows: Vec<StreamRow>,
    },
    GraphLoaded {
        job: JobId,
        tail: GraphTail,
    },
    DiffLoaded {
        job: JobId,
        revision: RevisionSelection,
        document: DiffDocument,
        details: Option<RevisionDetails>,
    },
    SnapshotDone {
        job: JobId,
        origin: RefreshOrigin,
        snapshot: RepositorySnapshot,
        /// The skipped-large-file warning, when the snapshot hit one. Surfaced
        /// once here because the snapshot prologue is single now.
        warnings: Vec<String>,
    },
    MutationDone {
        job: JobId,
        outcome: MutationOutcome,
    },
    PreviewDone {
        job: JobId,
        simulation: DraftSimulation,
    },
    FetchDone {
        job: JobId,
        output: Vec<String>,
    },
    DetailsLoaded {
        job: JobId,
        revision: RevisionSelection,
        details: RevisionDetails,
    },
    EmptyStatus {
        job: JobId,
        updates: Vec<(usize, bool)>,
    },
    TreeListed {
        job: JobId,
        revision: RevisionSelection,
        entries: Vec<SourceEntry>,
    },
    FileRead {
        job: JobId,
        revision: RevisionSelection,
        path: String,
        file: SourceFileLoad,
    },
    FilePairRead {
        job: JobId,
        path: String,
        old: Option<String>,
        new: Option<String>,
    },
    BookmarkChecked {
        job: JobId,
        backwards: bool,
    },
    /// The full diff of one collapsed file; `None` when the path is no longer
    /// part of the diff.
    FileExpanded {
        job: JobId,
        path: String,
        file: Option<DiffFile>,
    },
    Progress {
        job: JobId,
        loaded: usize,
        total: usize,
    },
    /// A slice of the operation log, newest first. Not terminal: the stream
    /// ends with a [`HistoryResponse::OpLogDone`].
    OpLogBatch {
        job: JobId,
        entries: Vec<OperationEntry>,
    },
    /// The answer to a [`Command::History`].
    History {
        job: JobId,
        response: HistoryResponse,
    },
    /// The repository's operation head moved — ours or someone else's. The
    /// actor dedups on the fingerprint, so this only fires on a real change.
    OpHeadChanged {
        fingerprint: String,
    },
    /// A file under the working tree changed. Topology is untouched, so the
    /// projection asks for a snapshot rather than a walk. Separate from
    /// `OpHeadChanged` because a plain edit writes no operation at all.
    WorkingCopyChanged,
    /// Git refs, worktree HEADs, or working-copy status may have changed.
    /// Git's synthetic working-copy row requires a graph refresh too.
    RepositoryChanged,
    /// The jj checkout was stale — another process (a CLI command, another
    /// workspace's snapshot) rewrote `@` — so the actor updated the files on
    /// disk to the latest operation instead of snapshotting over it, like
    /// `jj workspace update-stale`. Edits on disk were snapshotted first.
    /// Unsolicited: it rides alongside the snapshot or mutation that hit it.
    StaleWorkingCopyRecovered,
    ConflictInfoLoaded {
        job: JobId,
        revision: RevisionSelection,
        files: Vec<ConflictFile>,
    },
    MergeToolPrepared {
        job: JobId,
        invocation: MergeToolInvocation,
    },
    /// The answer to a [`Command::ResolveRevset`]: what `revset` matched, or
    /// why it doesn't parse or resolve.
    RevsetResolved {
        job: JobId,
        revset: String,
        result: Result<RevsetMatch, RevsetError>,
    },
    Failed {
        job: JobId,
        error: RepoError,
    },
    Cancelled {
        job: JobId,
    },
}

impl Payload {
    pub fn job(&self) -> Option<JobId> {
        match self {
            Self::Batch { job, .. }
            | Self::GraphLoaded { job, .. }
            | Self::DiffLoaded { job, .. }
            | Self::SnapshotDone { job, .. }
            | Self::MutationDone { job, .. }
            | Self::PreviewDone { job, .. }
            | Self::FetchDone { job, .. }
            | Self::DetailsLoaded { job, .. }
            | Self::EmptyStatus { job, .. }
            | Self::TreeListed { job, .. }
            | Self::FileRead { job, .. }
            | Self::FilePairRead { job, .. }
            | Self::BookmarkChecked { job, .. }
            | Self::FileExpanded { job, .. }
            | Self::Progress { job, .. }
            | Self::ConflictInfoLoaded { job, .. }
            | Self::MergeToolPrepared { job, .. }
            | Self::OpLogBatch { job, .. }
            | Self::History { job, .. }
            | Self::RevsetResolved { job, .. }
            | Self::Failed { job, .. }
            | Self::Cancelled { job, .. } => Some(*job),
            Self::Ready { .. }
            | Self::OpHeadChanged { .. }
            | Self::WorkingCopyChanged
            | Self::RepositoryChanged
            | Self::StaleWorkingCopyRecovered => None,
        }
    }

    /// Whether this event ends its job. Exactly one terminal event arrives per
    /// job, which is what lets the projection clear an inflight slot without
    /// tracking anything else.
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::Batch { .. }
                | Self::Progress { .. }
                | Self::OpLogBatch { .. }
                | Self::Ready { .. }
                | Self::OpHeadChanged { .. }
                | Self::WorkingCopyChanged
                | Self::RepositoryChanged
                | Self::StaleWorkingCopyRecovered
        )
    }
}

/// How the actor was asked to open a repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OpenSpec {
    /// A local jj or git repository at this workspace root.
    Local { root: PathBuf, scope: PathBuf },
    /// A GitHub pull request, streamed through the `gh` CLI.
    GitHubPr(crate::github::PrSpec),
}
