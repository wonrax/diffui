//! Headless orchestration primitives for driving a repository view.
//!
//! These are the runtime-agnostic *policies* a frontend would otherwise have to
//! reimplement: a serial mutation queue (jj's working-copy lock allows only one
//! mutation at a time), load-versioning (drop results from a superseded load),
//! refresh coalescing, and the streaming cold-load fold. They do no IO and no
//! rendering — pure logic — so they're unit-testable and shared across any
//! frontend (the iced app, and future electron/web/swiftui ones).
//!
//! The fuller sans-IO `Session` engine (a single object owning all per-repo
//! domain + orchestration state and emitting a command/outcome/event stream) is
//! designed in the project plan and builds on exactly these primitives; this
//! module is the foundation it (and any frontend) reuses today.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::graph_layout::{GraphLayout, LaneFoldState};
use crate::model::{
    BookmarksInfo, BranchStatus, CommitStore, DiffDocument, LoadProgress, RevisionDetails,
    RevisionSelection, StreamRow,
};
use crate::repository::{Repository, RepositorySnapshot};
use crate::source::{RepoSource, SourceHandle};

/// A parked diff document for a source that flips between several documents —
/// a PR's "all changes" view vs its per-commit diffs — so flipping back is an
/// in-memory move instead of a re-download. Keyed in [`Session::pr_diffs`]
/// by commit id (`""` = the whole-PR diff).
#[derive(Debug, Clone, Default)]
pub struct CachedDiff {
    pub document: DiffDocument,
    /// The source-reported totals that go with the document (see
    /// [`Session::authoritative_totals`]).
    pub totals: Option<(usize, usize)>,
    pub details: Option<RevisionDetails>,
}

/// What triggered a repository refresh — decides how much we reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOrigin {
    /// The filesystem watcher (a working-tree file edit). It ignores `.jj`/
    /// `.git`, so the change is always a working-copy tree edit — topology is
    /// unchanged, so the diff reloads but the graph walk is skipped.
    Watcher,
    /// Focus regain or a manual "Refresh repository". These can follow an
    /// external op (rebase, new, bookmark move) that changed topology, so they
    /// do the full reload.
    Focus,
}

/// Merge a newly-requested refresh origin with one already coalesced: a `Focus`
/// full walk subsumes a `Watcher` lightweight @-diff reload.
pub fn coalesce_refresh(pending: Option<RefreshOrigin>, incoming: RefreshOrigin) -> RefreshOrigin {
    if matches!(pending, Some(RefreshOrigin::Focus)) || matches!(incoming, RefreshOrigin::Focus) {
        RefreshOrigin::Focus
    } else {
        RefreshOrigin::Watcher
    }
}

/// A monotonic version stamp for graph loads. Async results carry the version
/// they were started under; [`LoadVersion::is_current`] drops stale ones from a
/// load that's since been superseded (a backgrounded tab, a refresh that
/// replaced the walk in flight).
#[derive(Debug, Default, Clone)]
pub struct LoadVersion {
    next: u64,
    current: u64,
}

impl LoadVersion {
    /// Start a new load: allocate the next version and make it current.
    pub fn bump(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.current = self.next;
        self.current
    }

    pub fn current(&self) -> u64 {
        self.current
    }

    /// Whether `version` belongs to the load currently being applied.
    pub fn is_current(&self, version: u64) -> bool {
        version == self.current
    }
}

/// The decision [`MutationQueue::enqueue`] returns: run the job now, or hold it.
#[derive(Debug, PartialEq, Eq)]
pub enum QueueAction<Job> {
    /// `job` became the in-flight job — run it now.
    Run(Job),
    /// `job` was queued behind the in-flight one — show it as pending.
    Queued,
}

/// A serial job queue: at most one job in flight, the rest FIFO behind it. Pure
/// policy — the frontend runs each job and calls [`MutationQueue::advance`] when
/// it finishes (on success *and* failure, so a failure never strands the queue).
#[derive(Debug, Clone)]
pub struct MutationQueue<Job> {
    in_flight: bool,
    queue: VecDeque<Job>,
}

impl<Job> Default for MutationQueue<Job> {
    fn default() -> Self {
        Self {
            in_flight: false,
            queue: VecDeque::new(),
        }
    }
}

impl<Job> MutationQueue<Job> {
    /// Submit a job: returns [`QueueAction::Run`] if it can start immediately
    /// (and marks the queue busy), else [`QueueAction::Queued`].
    pub fn enqueue(&mut self, job: Job) -> QueueAction<Job> {
        if self.in_flight {
            self.queue.push_back(job);
            QueueAction::Queued
        } else {
            self.in_flight = true;
            QueueAction::Run(job)
        }
    }

    /// The in-flight job finished. Returns the next job to run (still busy), or
    /// `None` when the queue drained — and clears the busy flag, so the frontend
    /// knows to run its post-batch reconcile (e.g. a single reload).
    pub fn advance(&mut self) -> Option<Job> {
        match self.queue.pop_front() {
            Some(next) => Some(next),
            None => {
                self.in_flight = false;
                None
            }
        }
    }

    /// Whether a job is running or waiting — gates background snapshots so a
    /// watcher refresh doesn't fire into the middle of a mutation (both take
    /// jj's working-copy lock).
    pub fn is_busy(&self) -> bool {
        self.in_flight || !self.queue.is_empty()
    }
}

/// Transient builder state carried across a streaming cold load's batches: the
/// author interner and the lane fold (both must persist as rows append), tagged
/// with the load `version` so a stale batch can be dropped. Freed when the load
/// finishes.
#[derive(Debug, Default, Clone)]
pub struct ColdCursor {
    pub version: u64,
    pub interner: HashMap<String, u32>,
    pub fold: LaneFoldState,
}

impl ColdCursor {
    pub fn new(version: u64) -> Self {
        Self {
            version,
            ..Self::default()
        }
    }
}

/// Result of folding one cold-load batch: the per-row shortest-unique-prefix
/// lengths (appended to the sidebar index) and the working-copy row index if it
/// appeared in this batch.
#[derive(Debug, Default)]
pub struct ColdBatchFold {
    pub prefix_lens: Vec<usize>,
    pub working_copy_index: Option<usize>,
}

/// Fold one streaming batch into the growing `commits` + `graph`. Pure CPU — the
/// heart of the cold load — extracted here so it's reusable and testable without
/// a UI. `selecting_wc` is whether the working copy is the selected revision (so
/// the caller can move the selection onto it when it streams in).
pub fn fold_cold_batch(
    commits: &mut CommitStore,
    graph: &mut GraphLayout,
    cursor: &mut ColdCursor,
    rows: Vec<StreamRow>,
    selecting_wc: bool,
) -> ColdBatchFold {
    let mut fold = ColdBatchFold {
        prefix_lens: Vec::with_capacity(rows.len()),
        working_copy_index: None,
    };
    for row in rows {
        let index = commits.len();
        // The graph fold consumes the frame + the row's bookmarks (still owned
        // by the summary), so push it before the summary moves into the store.
        graph.push(&row.frame, &row.summary.bookmarks, &mut cursor.fold);
        // jj precomputes the shortest-unique-prefix length per row, so the
        // sidebar index grows by one O(1) push instead of an O(n) rescan.
        let total = row.summary.change_id.chars().count();
        let prefix = row.summary.shortest_change_id_len.unwrap_or(1).min(total);
        fold.prefix_lens.push(prefix);
        if selecting_wc && row.summary.is_working_copy {
            fold.working_copy_index = Some(index);
        }
        commits.push(row.summary, &mut cursor.interner);
    }
    fold
}

/// Where a repository's load currently stands. The frontend's `view` reads this
/// to choose between a loading indicator, the live graph, or an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LoadStatus {
    #[default]
    Loading,
    Loaded,
    Failed(String),
}

/// All per-repository **domain + orchestration** state, owned in one place so a
/// frontend never reimplements — or hand-syncs — it. This is the headless core's
/// single source of truth for *what a repo view is showing and what work is in
/// flight*; the frontend keeps only its own UI state (scroll offsets, the
/// file-selection cursor, activity widgets) alongside it.
///
/// It does **no IO and spawns nothing**: heavy work runs off-thread in the
/// frontend's runtime and feeds results back in (a future pass routes that
/// through a command/outcome stream). Fields are public because the frontend
/// renders straight from them — `view` borrows `commits`/`graph` every frame, so
/// the store stays resident and rendering is O(visible rows) even at ~1M commits.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// The repository this session views, or `None` for the empty (no-repo)
    /// state and for non-repo sources (a GitHub PR). Repo-*specific* machinery
    /// — the fs-watcher, the jj streaming cold load, mutations — keys off this;
    /// everything load-shaped dispatches through [`Session::source`].
    pub repository: Option<Repository>,
    /// The diff source behind this session: what diff/graph loads, fetch and
    /// undo dispatch through. `None` only for the empty (no-tab) state.
    pub source: Option<SourceHandle>,
    pub status: LoadStatus,
    /// The diff currently shown — the selected revision's, or the working copy's.
    pub document: DiffDocument,
    /// Diff totals reported by the source itself, when it knows better than a
    /// sum over the parsed files — a GitHub PR's header counts: the files-API
    /// fallback zeroes per-file counts for oversized blobs, so the summed
    /// document totals can undercount. Frontends prefer these for the header
    /// totals; `None` for repo-backed sources.
    pub authoritative_totals: Option<(usize, usize)>,
    /// Compact commit store backing the sidebar. A streaming cold load appends to
    /// it per batch; a refresh swaps it wholesale.
    pub commits: CommitStore,
    pub selected_revision: RevisionSelection,
    /// The revision whose diff load is in flight, gating diff-class results so a
    /// superseded switch's result is dropped. Distinct from the graph load's
    /// version guard ([`Session::load`]).
    pub pending_revision: Option<RevisionSelection>,
    /// Op-log fingerprint of the last load, for the watcher's "did anything
    /// change" dedup (so our own snapshot writes don't trigger a re-walk).
    pub repository_snapshot: Option<RepositorySnapshot>,
    pub snapshot_pending: bool,
    pub revision_details: Option<RevisionDetails>,
    /// Working-copy branch summary (nearest local bookmark + ahead/behind) for
    /// the sidebar footer. `None` until a load resolves it.
    pub branch_status: Option<BranchStatus>,
    /// Repo-wide bookmark table, loaded with the graph; drives the context menu's
    /// move/track/delete/push. Empty for git.
    pub bookmarks: BookmarksInfo,
    /// Bumped when `commits` changes; tags background empty-status results so a
    /// result computed against a superseded graph is dropped.
    pub commits_version: u64,
    pub graph: GraphLayout,
    pub sidebar_prefix_lens: Vec<usize>,
    pub selected_commit_index: Option<usize>,
    pub commit_progress: LoadProgress,
    /// When the current load began, or `None` when idle — drives the grace period
    /// before a loading indicator appears (so fast loads don't flash one).
    pub loading_since: Option<Instant>,
    /// Resolved-once empty status keyed by commit-id; survives reloads (a commit's
    /// emptiness never changes), so only newly-seen merges recompute.
    pub empty_cache: HashMap<String, bool>,
    /// Append state for an in-flight streaming cold load (jj only); `None` when no
    /// stream is running.
    pub load: Option<ColdCursor>,
    /// The revset / revision-range filtering the log. Empty or `all()` is the
    /// default; the frontend persists it per repo root.
    pub revset: String,
    /// Parked documents for sources that flip between several (PR tabs:
    /// whole-PR diff ↔ per-commit diffs). The *displayed* document is never
    /// in here — switching moves it in and the target out. Cleared on reload.
    pub pr_diffs: HashMap<String, CachedDiff>,
    /// Identity of the displayed document, stamped from the frontend's global
    /// counter on every replacement. Background per-file results (syntax
    /// highlights) carry it so they route to the session still showing that
    /// document — or are dropped once it's gone. Appends (a PR stream) keep
    /// the id: existing files are unchanged, results stay valid.
    pub document_id: u64,
    /// File indices of `document` still waiting for background highlighting,
    /// drained a few at a time by the frontend.
    pub highlight_pending: VecDeque<usize>,
    /// Highlight jobs currently running for `document`.
    pub highlight_in_flight: usize,
    /// A refresh requested while a load/snapshot was already in flight, held
    /// (coalesced — `Focus` subsumes `Watcher`) so it runs once the current work
    /// finishes rather than racing it (a second wc snapshot thrashes jj's lock).
    pub pending_refresh: Option<RefreshOrigin>,
}

impl Session {
    /// A never-loaded session for `repository`: empty graph, `Loading` status, no
    /// work in flight. `revset` is the persisted (or default) filter. The
    /// session's [`SourceHandle`] is derived — a [`RepoSource`] over the repo.
    pub fn unloaded(repository: Option<Repository>, revset: String) -> Self {
        let source = repository
            .clone()
            .map(|repository| SourceHandle::new(RepoSource::new(repository)));
        Self {
            repository,
            source,
            status: LoadStatus::Loading,
            selected_revision: RevisionSelection::WorkingCopy,
            revset,
            ..Self::default()
        }
    }

    /// A never-loaded session over a non-repo source (e.g. a GitHub PR): no
    /// repository — so the watcher/mutation machinery stays off — and every
    /// load dispatches through `source`.
    pub fn for_source(source: SourceHandle) -> Self {
        Self {
            source: Some(source),
            ..Self::unloaded(None, String::new())
        }
    }

    /// The session shown when no repository is open (the last tab closed).
    /// `Loaded` so the frontend shows its empty state, not a spinner.
    pub fn empty() -> Self {
        Self {
            status: LoadStatus::Loaded,
            ..Self::unloaded(None, String::new())
        }
    }

    /// Whether a graph load, working-copy snapshot, or diff load is in flight —
    /// the gate that makes a refresh coalesce (into [`Session::pending_refresh`])
    /// instead of racing the work already running.
    pub fn busy(&self) -> bool {
        self.snapshot_pending || self.load.is_some() || self.pending_revision.is_some()
    }

    /// Row index in `commits` of the selected revision (`@` or a commit-id), or
    /// `None` when it isn't in the loaded graph (e.g. filtered out by the revset).
    /// Drives the reveal-on-jump scroll and the expanded file-list span.
    pub fn find_selected_commit_index(&self) -> Option<usize> {
        match &self.selected_revision {
            RevisionSelection::WorkingCopy => {
                self.commits.iter().position(|row| row.is_working_copy())
            }
            RevisionSelection::Commit(id) => self
                .commits
                .iter()
                .position(|row| !row.is_working_copy() && id == row.commit_id()),
        }
    }

    /// Recompute the per-row sidebar index (shortest-unique-prefix lengths +
    /// selected-row index) after the commit graph changes. O(n) once per graph
    /// load, so a frontend's per-frame render stays O(visible rows).
    pub fn rebuild_sidebar_index(&mut self) {
        self.sidebar_prefix_lens = self.commits.shortest_unique_prefix_lens();
        self.selected_commit_index = self.find_selected_commit_index();
    }

    /// One-pass `commit-id hex → index` lookup in the loaded log for `wanted`
    /// (early-exit once all are found). Used to order bookmark menus by proximity
    /// to a reference revision — far cheaper than a graph-distance revset, and the
    /// index distance matches the sidebar's visual order.
    pub fn commit_indices<'a>(
        &self,
        wanted: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, usize> {
        let want: HashSet<&str> = wanted.into_iter().collect();
        let mut out: HashMap<String, usize> = HashMap::new();
        if want.is_empty() {
            return out;
        }
        for (index, row) in self.commits.iter().enumerate() {
            let id = row.commit_id();
            if want.contains(id) {
                out.entry(id.to_owned()).or_insert(index);
                if out.len() == want.len() {
                    break;
                }
            }
        }
        out
    }

    /// The displayed document was replaced: stamp its new identity and restart
    /// highlight bookkeeping — queue every file of the new document whose
    /// lines carry no spans yet (a parked PR document keeps the spans it
    /// already earned), and forget in-flight jobs (their results carry the old
    /// id and will be dropped on arrival).
    pub fn reset_highlights(&mut self, document_id: u64) {
        self.document_id = document_id;
        self.highlight_in_flight = 0;
        self.highlight_pending.clear();
        self.enqueue_unhighlighted(0);
    }

    /// Queue files from `start` onward that have no syntax spans at all —
    /// used by [`reset_highlights`] for a fresh document and by streaming
    /// appends for the newly arrived tail.
    pub fn enqueue_unhighlighted(&mut self, start: usize) {
        for (offset, file) in self.document.files[start.min(self.document.files.len())..]
            .iter()
            .enumerate()
        {
            let unhighlighted = file
                .hunks
                .iter()
                .all(|hunk| hunk.lines.iter().all(|line| line.syntax.is_empty()));
            if unhighlighted && !file.hunks.is_empty() {
                self.highlight_pending.push_back(start + offset);
            }
        }
    }

    /// Apply any cached empty-status to the loaded commits in place, and return
    /// the commits still needing async resolution (row index + commit-id hex),
    /// capped at `limit`. Bumps `commits_version` when a cached value was applied
    /// (so the sidebar re-renders); the frontend spawns resolution for the
    /// returned targets and feeds results back as an empty-status outcome.
    pub fn take_empty_status_targets(&mut self, limit: usize) -> Vec<(usize, String)> {
        let mut cached_updates = Vec::new();
        let mut targets = Vec::new();
        for (index, row) in self.commits.iter().enumerate() {
            if row.is_empty().is_some() {
                continue;
            }
            match self.empty_cache.get(row.commit_id()) {
                Some(&empty) => cached_updates.push((index, empty)),
                None if targets.len() < limit => targets.push((index, row.commit_id().to_owned())),
                None => {}
            }
        }
        let had_cached = !cached_updates.is_empty();
        for (index, empty) in cached_updates {
            self.commits.set_is_empty(index, empty);
        }
        if had_cached {
            self.commits_version = self.commits_version.wrapping_add(1);
        }
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_focus_subsumes_watcher() {
        assert_eq!(
            coalesce_refresh(None, RefreshOrigin::Watcher),
            RefreshOrigin::Watcher
        );
        assert_eq!(
            coalesce_refresh(Some(RefreshOrigin::Watcher), RefreshOrigin::Focus),
            RefreshOrigin::Focus
        );
        assert_eq!(
            coalesce_refresh(Some(RefreshOrigin::Focus), RefreshOrigin::Watcher),
            RefreshOrigin::Focus
        );
    }

    #[test]
    fn load_version_is_monotonic_and_drops_stale() {
        let mut v = LoadVersion::default();
        let first = v.bump();
        assert!(v.is_current(first));
        let second = v.bump();
        assert!(v.is_current(second));
        // A result from the first (now superseded) load is stale.
        assert!(!v.is_current(first));
        assert_ne!(first, second);
    }

    #[test]
    fn mutation_queue_serializes_in_fifo_order() {
        let mut q: MutationQueue<&str> = MutationQueue::default();
        assert!(!q.is_busy());

        // First runs immediately.
        assert_eq!(q.enqueue("a"), QueueAction::Run("a"));
        assert!(q.is_busy());
        // Subsequent ones queue behind it, in order.
        assert_eq!(q.enqueue("b"), QueueAction::Queued);
        assert_eq!(q.enqueue("c"), QueueAction::Queued);
        assert!(q.is_busy());

        // Completion drains FIFO, staying busy until empty.
        assert_eq!(q.advance(), Some("b"));
        assert!(q.is_busy());
        assert_eq!(q.advance(), Some("c"));
        assert!(q.is_busy());
        // Drained: no next job, and no longer busy.
        assert_eq!(q.advance(), None);
        assert!(!q.is_busy());

        // Ready to run immediately again.
        assert_eq!(q.enqueue("d"), QueueAction::Run("d"));
    }
}
