//! The client-side projection of a repository.
//!
//! A [`Session`] is the fold over the event stream one [`crate::repo`] actor
//! produces: `apply` takes an [`Event`], updates what the view reads, and
//! returns the [`Effect`]s the frontend must carry out — almost always
//! "send this command back".
//!
//! Everything that used to be spread across the frontend's completion handlers
//! lives here instead: which job owns which slot, whether a result is stale,
//! when a load is finished, what a snapshot's fingerprint means. It does no IO
//! and spawns nothing.
//!
//! The view-facing fields stay public because the frontend renders straight
//! from them — `view` borrows `commits`/`graph` every frame, so the store stays
//! resident and rendering is O(visible rows) even at ~1M commits. The
//! orchestration state is private: "is this event stale" is one comparison
//! against its slot, and a terminal event clears the slot automatically.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::graph_layout::{GraphLayout, LaneFoldState};
use crate::model::{
    BookmarksInfo, BranchStatus, CommitStore, DiffDocument, LoadProgress, RevisionDetails,
    RevisionSelection, StreamRow,
};
use crate::mutations::{MutationOp, MutationOutcome};
use crate::repo::{
    Capabilities, Command, Event, JobId, Payload, PreviewRequest, RepoError, RepoId,
};
use crate::repository::{FetchTarget, RepositorySnapshot};

/// A parked diff document for a source that flips between several documents —
/// a PR's "all changes" view vs its per-commit diffs — so flipping back is an
/// in-memory move instead of a re-download. Keyed in [`Session::pr_diffs`]
/// by commit id (`""` = the whole-PR diff).
#[derive(Debug, Clone, Default)]
pub struct CachedDiff {
    pub document: DiffDocument,
    pub details: Option<RevisionDetails>,
}

/// What triggered a repository refresh — decides how much we reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOrigin {
    /// The filesystem watcher (a working-tree file edit). It ignores `.jj`/
    /// `.git`, so the change is always a working-copy tree edit — topology is
    /// unchanged, so the diff reloads but the graph walk is skipped.
    Watcher,
    /// Focus regain, a manual "Refresh repository", or the tail of a mutation.
    /// These can follow an external op (rebase, new, bookmark move) that
    /// changed topology, so they do the full reload.
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

/// How a graph load paints. The actor always streams `Batch`es; this is the
/// projection's decision about what to do with them, and it is the whole of
/// what used to be two separate load pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphMode {
    /// Nothing worth keeping on screen: append each batch straight into the
    /// live store so the sidebar grows as the walk progresses.
    Progressive,
    /// A graph is already on screen: build the replacement off to the side and
    /// swap it in whole, so a refresh or a revset change never flashes empty.
    DoubleBuffer,
}

/// Transient builder state carried across a streaming load's batches: the
/// author interner and the lane fold (both must persist as rows append).
#[derive(Debug, Default, Clone)]
pub struct ColdCursor {
    pub interner: HashMap<String, u32>,
    pub fold: LaneFoldState,
}

/// Result of folding one batch: the per-row shortest-unique-prefix lengths
/// (appended to the sidebar index) and the working-copy row index if it
/// appeared in this batch.
#[derive(Debug, Default)]
pub struct ColdBatchFold {
    pub prefix_lens: Vec<usize>,
    pub working_copy_index: Option<usize>,
}

/// Fold one streaming batch into the growing `commits` + `graph`. Pure CPU —
/// the heart of the load — kept as a free function so it stays testable
/// against the one-shot fold it has to agree with. `selecting_wc` is whether
/// the working copy is the selected revision (so the caller can move the
/// selection onto it when it streams in).
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

/// What the frontend must do after an event was folded in. Almost all of it is
/// "send this command"; the rest is the handful of things only a frontend can
/// do — drop a shaped-text cache, scroll a row into view, write to the
/// activity log.
#[derive(Debug, Clone)]
pub enum Effect {
    Send(Command),
    /// The displayed document was replaced: drop the per-line shaped-paragraph
    /// cache, whose keys map to the old text.
    RepaintDocument,
    /// Scroll the selected revision's row into view — a selection that moved
    /// through something other than the sidebar (a palette jump, a mutation
    /// that moved `@`).
    RevealSelection,
    /// The commit store was replaced wholesale, so every row index the frontend
    /// is holding now addresses a different commit. Anything keyed by index —
    /// a target-mode draft's destination candidate, most of all — has to be
    /// re-resolved by commit id or dropped.
    GraphReplaced,
    Activity(Activity),
}

/// Something worth telling the user about. The session can't mint the
/// frontend's activity ids, so it says what happened and the frontend files it.
#[derive(Debug, Clone)]
pub enum Activity {
    /// The graph load finished. `detail` is the row's result summary.
    LoadFinished { ok: bool, detail: Option<String> },
    /// A failure the user must not miss, on top of the log entry. Only for the
    /// work the projection issues on its own — an operation the frontend
    /// started owns its own activity row and reports there.
    Toast {
        repo: RepoId,
        title: String,
        body: String,
    },
    /// A warning the backend surfaced in passing (a snapshot that skipped an
    /// oversized file).
    Note(String),
}

/// The job currently loading the graph, plus what to do with its batches.
#[derive(Debug, Clone)]
struct GraphJob {
    id: JobId,
    mode: GraphMode,
    cursor: ColdCursor,
    /// The replacement store under construction, for `DoubleBuffer`.
    staging: Option<(CommitStore, GraphLayout, Vec<usize>)>,
}

/// All per-repository domain + orchestration state.
///
/// Orchestration is private: each class of work has one typed slot holding the
/// job that owns it, so "is this event stale" is a single comparison and a
/// terminal event clears the slot. Five ad-hoc completion guards and one
/// `pending_revision` shared between graph reloads and diff switches used to
/// do this job between them.
#[derive(Debug, Default)]
pub struct Session {
    /// Which repository this session views, and what it can do. `None` until
    /// the actor's `Ready` event lands (and for the no-tab empty state).
    pub repo: Option<RepoId>,
    pub capabilities: Capabilities,
    pub status: LoadStatus,
    /// The diff currently shown — the selected revision's, or the working copy's.
    pub document: DiffDocument,
    /// Compact commit store backing the sidebar.
    pub commits: CommitStore,
    pub selected_revision: RevisionSelection,
    /// Op-log fingerprint of the last load, for the "did anything change" dedup
    /// (so our own snapshot writes don't trigger a re-walk).
    pub repository_snapshot: Option<RepositorySnapshot>,
    pub revision_details: Option<RevisionDetails>,
    /// Working-copy branch summary (nearest local bookmark + ahead/behind) for
    /// the sidebar footer. `None` until a load resolves it.
    pub branch_status: Option<BranchStatus>,
    /// Repo-wide bookmark table, loaded with the graph; drives the context
    /// menu's move/track/delete/push. Empty for git.
    pub bookmarks: BookmarksInfo,
    /// The repository's root commit id, once a graph load has reported it.
    /// Rewrite-target lists exclude it (see
    /// [`rewritten_targets`](crate::repo::rewritten_targets)).
    pub root_commit_id: Option<String>,
    /// Bumped on every change the rendered rows must be re-read for, including
    /// ones that leave the row *indices* alone (a chip flipping). A frontend
    /// that memoizes shaped rows keys on this.
    ///
    /// Deliberately not the graph's identity: one counter for both meant a
    /// cosmetic repaint silently invalidated background results that were
    /// still computed against exactly the rows they addressed. The identity is
    /// the graph load's own job id ([`Session::graph_version`]).
    pub repaint_version: u64,
    pub graph: GraphLayout,
    pub sidebar_prefix_lens: Vec<usize>,
    pub selected_commit_index: Option<usize>,
    pub commit_progress: LoadProgress,
    /// When the current load began, or `None` when idle — drives the grace
    /// period before a loading indicator appears.
    pub loading_since: Option<Instant>,
    /// Resolved-once empty status keyed by commit-id; survives reloads (a
    /// commit's emptiness never changes).
    pub empty_cache: HashMap<String, bool>,
    /// The revset / revision-range filtering the log.
    pub revset: String,
    /// Parked documents for sources that flip between several (PR tabs).
    pub pr_diffs: HashMap<String, CachedDiff>,
    /// Identity of the displayed document. Background per-file results carry
    /// it so they route to the session still showing that document.
    pub document_id: u64,
    /// File indices of `document` still waiting for background highlighting.
    pub highlight_pending: VecDeque<usize>,
    /// Highlight jobs currently running for `document`.
    pub highlight_in_flight: usize,
    /// A refresh requested while work was already in flight, held (coalesced —
    /// `Focus` subsumes `Watcher`) so it runs once the current work finishes
    /// rather than racing it (a second wc snapshot thrashes jj's lock).
    pub pending_refresh: Option<RefreshOrigin>,

    // ── inflight slots ──────────────────────────────────────────────────
    graph_job: Option<GraphJob>,
    diff_job: Option<(JobId, RevisionSelection)>,
    snapshot_job: Option<(JobId, RefreshOrigin)>,
    /// The sweep in flight, with the graph job whose rows it addresses.
    empty_status_job: Option<(JobId, JobId)>,
    /// The job of the graph currently on screen — the identity a background
    /// per-row result is checked against.
    graph_version: Option<JobId>,
    mutation_job: Option<JobId>,
}

impl Session {
    /// A never-loaded session. `revset` is the persisted (or default) filter.
    pub fn unloaded(revset: String) -> Self {
        Self {
            status: LoadStatus::Loading,
            selected_revision: RevisionSelection::WorkingCopy,
            revset,
            ..Self::default()
        }
    }

    /// The session shown when no repository is open (the last tab closed).
    /// `Loaded` so the frontend shows its empty state, not a spinner.
    pub fn empty() -> Self {
        Self {
            status: LoadStatus::Loaded,
            ..Self::unloaded(String::new())
        }
    }

    /// Mint the next job id. Callers park it in a slot before sending, which is
    /// what makes every arriving event answerable with one comparison.
    pub fn next_job(&mut self) -> JobId {
        JobId::next()
    }

    /// Whether a graph load, snapshot, diff switch or mutation is in flight —
    /// the gate that makes a refresh coalesce instead of racing the work
    /// already running.
    pub fn busy(&self) -> bool {
        self.graph_job.is_some()
            || self.diff_job.is_some()
            || self.snapshot_job.is_some()
            || self.mutation_job.is_some()
    }

    /// Whether `job` belongs to one of this session's slots — how the
    /// frontend picks which tab an arriving event is for.
    pub fn owns_job(&self, job: JobId) -> bool {
        self.graph_job.as_ref().is_some_and(|load| load.id == job)
            || self.diff_job.as_ref().is_some_and(|(id, _)| *id == job)
            || self.snapshot_job.as_ref().is_some_and(|(id, _)| *id == job)
            || self.empty_status_job.is_some_and(|(id, _)| id == job)
            || self.mutation_job == Some(job)
    }

    /// Whether a mutation is running. Gates the actions a half-applied
    /// operation would confuse (a second mutation is *safe* — the actor
    /// serializes it — but the UI still shows it as queued).
    pub fn mutation_in_flight(&self) -> bool {
        self.mutation_job.is_some()
    }

    /// The revision whose diff is loading, or `None` when no switch is in
    /// flight. The toolbar's progress line reads it; so does the guard that
    /// keeps a cold load's initial `@` diff from overwriting a palette jump.
    pub fn diff_pending(&self) -> Option<&RevisionSelection> {
        self.diff_job.as_ref().map(|(_, revision)| revision)
    }

    pub fn diff_in_flight(&self) -> bool {
        self.diff_job.is_some()
    }

    /// Whether a graph load is streaming into the live store right now. The
    /// PR sidebar locks while this is true: switching the shown document out
    /// from under the batches would scatter its files into a commit diff.
    pub fn streaming(&self) -> bool {
        self.graph_job.is_some()
    }

    // ── issuing work ────────────────────────────────────────────────────

    /// Start a graph load. `progressive` paints batches as they arrive (a cold
    /// open, where there is nothing to preserve); otherwise the replacement is
    /// staged and swapped in whole.
    pub fn load_graph(&mut self, progressive: bool) -> Vec<Effect> {
        let job = self.next_job();
        let mut effects = self.cancel_graph();
        self.loading_since = Some(Instant::now());
        let mode = if progressive {
            GraphMode::Progressive
        } else {
            GraphMode::DoubleBuffer
        };
        if progressive {
            self.status = LoadStatus::Loading;
            self.commits = CommitStore::default();
            self.graph = GraphLayout::default();
            self.sidebar_prefix_lens.clear();
            self.selected_commit_index = None;
            effects.push(Effect::GraphReplaced);
        }
        self.graph_job = Some(GraphJob {
            id: job,
            mode,
            cursor: ColdCursor::default(),
            staging: (!progressive)
                .then(|| (CommitStore::default(), GraphLayout::default(), Vec::new())),
        });
        effects.push(Effect::Send(Command::LoadGraph {
            job,
            revset: self.revset.clone(),
        }));
        effects
    }

    /// Start a diff load for `revision`, superseding any switch already in
    /// flight.
    ///
    /// A source that flips between a fixed set of documents (a PR's "all
    /// changes" view and its per-commit diffs) parks the one it is leaving and
    /// moves the target back in, so flipping back is two moves rather than a
    /// re-download.
    pub fn load_diff(&mut self, revision: RevisionSelection) -> Vec<Effect> {
        if !self.capabilities.graph
            && let Some(cached) = self.pr_diffs.remove(&document_key(&revision))
        {
            self.park_displayed_document();
            self.document = cached.document;
            self.revision_details = cached.details;
            self.selected_revision = revision;
            self.selected_commit_index = self.find_selected_commit_index();
            // An instant swap lands no event, so the follow-ups a `DiffLoaded`
            // would have produced are issued here.
            return vec![Effect::RepaintDocument, Effect::RevealSelection];
        }
        let mut effects = Vec::new();
        if let Some((job, _)) = self.diff_job.take() {
            effects.push(Effect::Send(Command::Cancel { job }));
        }
        let job = self.next_job();
        self.diff_job = Some((job, revision.clone()));
        self.loading_since = Some(Instant::now());
        effects.push(Effect::Send(Command::LoadDiff { job, revision }));
        effects
    }

    /// Fold the working copy into `@`. Coalesces instead of racing when
    /// something is already in flight: a second snapshot thrashes jj's lock,
    /// and the in-flight work's terminal event drains what was held.
    pub fn snapshot(&mut self, origin: RefreshOrigin) -> Vec<Effect> {
        if !self.capabilities.graph {
            return Vec::new();
        }
        if self.busy() {
            self.pending_refresh = Some(coalesce_refresh(self.pending_refresh, origin));
            return Vec::new();
        }
        let job = self.next_job();
        self.snapshot_job = Some((job, origin));
        vec![Effect::Send(Command::Snapshot { job, origin })]
    }

    /// Run `op`. The actor serializes mutations on its own thread, so there is
    /// no queue here — the one slot exists so each completion has an owner, and
    /// a caller with a second mutation to run holds it until this one reports.
    pub fn mutate(&mut self, op: MutationOp, allow_immutable: bool) -> (JobId, Vec<Effect>) {
        debug_assert!(
            self.mutation_job.is_none(),
            "a mutation would overwrite the one already in flight, leaving its completion unowned"
        );
        let job = self.next_job();
        self.mutation_job = Some(job);
        (
            job,
            vec![Effect::Send(Command::Mutate {
                job,
                op,
                allow_immutable,
            })],
        )
    }

    pub fn fetch(&mut self, target: FetchTarget) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        (job, vec![Effect::Send(Command::Fetch { job, target })])
    }

    pub fn preview(&mut self, draft: PreviewRequest) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        (job, vec![Effect::Send(Command::Preview { job, draft })])
    }

    /// Cancel the graph load in flight, if any. A superseded walk that runs to
    /// completion competes for the same disk as the walk that replaced it.
    fn cancel_graph(&mut self) -> Vec<Effect> {
        match self.graph_job.take() {
            Some(job) => vec![Effect::Send(Command::Cancel { job: job.id })],
            None => Vec::new(),
        }
    }

    // ── folding events ──────────────────────────────────────────────────

    /// Fold one event in and report what the frontend must do next.
    ///
    /// An event whose job doesn't match its slot is stale — its work was
    /// superseded or its tab moved on — and is dropped here rather than at
    /// forty call sites.
    pub fn apply(&mut self, event: Event) -> Vec<Effect> {
        let repo = event.repo;
        match event.payload {
            Payload::Ready { .. } => Vec::new(),
            Payload::Batch { job, rows } => self.on_batch(job, rows),
            Payload::GraphLoaded { job, tail } => self.on_graph_loaded(job, tail),
            Payload::DiffLoaded {
                job,
                revision,
                document,
                details,
            } => self.on_diff_loaded(job, revision, document, details),
            Payload::SnapshotDone {
                job,
                origin,
                snapshot,
                warnings,
            } => self.on_snapshot(job, origin, snapshot, warnings),
            Payload::MutationDone { job, outcome } => self.on_mutation(job, outcome),
            Payload::EmptyStatus { job, updates } => self.on_empty_status(job, updates),
            Payload::Progress { loaded, total, .. } => {
                self.commit_progress.set_total(total);
                self.commit_progress.set_loaded(loaded);
                Vec::new()
            }
            Payload::WorkingCopyChanged => self.snapshot(RefreshOrigin::Watcher),
            Payload::OpHeadChanged { fingerprint } => self.on_op_head(&fingerprint),
            Payload::Failed { job, error } => self.on_failed(repo, job, error),
            Payload::Cancelled { job } => {
                self.clear_slot(job);
                self.drain_pending_refresh()
            }
            // Results the frontend routes by job on its own (previews, source
            // browsing, file pairs, details, fetch): the projection has no
            // state to fold them into, so it clears nothing and says nothing.
            Payload::PreviewDone { .. }
            | Payload::FetchDone { .. }
            | Payload::DetailsLoaded { .. }
            | Payload::TreeListed { .. }
            | Payload::FileRead { .. }
            | Payload::FilePairRead { .. }
            | Payload::BookmarkChecked { .. } => Vec::new(),
        }
    }

    fn on_batch(&mut self, job: JobId, rows: Vec<StreamRow>) -> Vec<Effect> {
        let Some(load) = self.graph_job.as_mut().filter(|load| load.id == job) else {
            return Vec::new();
        };
        let selecting_wc = matches!(self.selected_revision, RevisionSelection::WorkingCopy);
        match load.mode {
            GraphMode::Progressive => {
                let fold = fold_cold_batch(
                    &mut self.commits,
                    &mut self.graph,
                    &mut load.cursor,
                    rows,
                    selecting_wc,
                );
                self.sidebar_prefix_lens.extend(fold.prefix_lens);
                if let Some(index) = fold.working_copy_index {
                    self.selected_commit_index = Some(index);
                }
                // First batch on screen: lift the full-window loading indicator
                // and reveal the (still-growing) sidebar.
                if matches!(self.status, LoadStatus::Loading) {
                    self.status = LoadStatus::Loaded;
                    self.loading_since = None;
                }
            }
            GraphMode::DoubleBuffer => {
                let Some((commits, graph, prefixes)) = load.staging.as_mut() else {
                    return Vec::new();
                };
                let fold = fold_cold_batch(commits, graph, &mut load.cursor, rows, selecting_wc);
                prefixes.extend(fold.prefix_lens);
            }
        }
        self.repaint_version = self.repaint_version.wrapping_add(1);
        Vec::new()
    }

    fn on_graph_loaded(&mut self, job: JobId, tail: crate::repo::GraphTail) -> Vec<Effect> {
        let Some(load) = self.graph_job.take().filter(|load| load.id == job) else {
            // Not ours: put back whatever was there.
            return Vec::new();
        };
        let swapped = load.staging.is_some();
        if let Some((commits, graph, prefixes)) = load.staging {
            self.commits = commits;
            self.graph = graph;
            self.sidebar_prefix_lens = prefixes;
        }
        self.graph_version = Some(job);
        self.branch_status = tail.branch_status;
        self.bookmarks = tail.bookmarks;
        if tail.root_commit_id.is_some() {
            self.root_commit_id = tail.root_commit_id;
        }
        for (index, empty) in tail.empty_updates {
            // A shorter store must never be indexed past its end.
            if index >= self.commits.len() {
                continue;
            }
            let commit_id = self.commits.row(index).commit_id().to_owned();
            self.empty_cache.insert(commit_id, empty);
            self.commits.set_is_empty(index, empty);
        }
        // The stream ended: the graph is what it is, even when it held no rows
        // at all. A zero-row revset used to leave the tab spinning forever,
        // because only a batch ever lifted the indicator.
        self.status = LoadStatus::Loaded;
        self.loading_since = None;
        self.selected_commit_index = self.find_selected_commit_index();
        self.repaint_version = self.repaint_version.wrapping_add(1);

        let mut effects = vec![Effect::Activity(Activity::LoadFinished {
            ok: true,
            detail: None,
        })];
        if swapped {
            effects.push(Effect::GraphReplaced);
        }
        effects.extend(self.resolve_empty_status());
        effects.extend(self.drain_pending_refresh());
        effects
    }

    fn on_diff_loaded(
        &mut self,
        job: JobId,
        revision: RevisionSelection,
        document: DiffDocument,
        details: Option<RevisionDetails>,
    ) -> Vec<Effect> {
        if self.diff_job.as_ref().map(|(id, _)| *id) != Some(job) {
            return Vec::new();
        }
        self.diff_job = None;
        self.loading_since = None;

        // A working-copy diff is the definitive emptiness signal for `@` (files
        // present ⇒ not empty), so an edit flips the chip without a re-walk.
        // Synthesized conflict entries don't count: jj calls a conflicted merge
        // whose tree is exactly its parents' merge "empty", and the chip should
        // agree with the graph walk.
        let wc_empty = matches!(revision, RevisionSelection::WorkingCopy).then(|| {
            document
                .files
                .iter()
                .all(|file| file.status == crate::model::DiffFileStatus::Conflicted)
        });

        if !self.capabilities.graph {
            self.park_displayed_document();
        }
        self.selected_revision = revision;
        self.status = LoadStatus::Loaded;
        self.document = document;
        self.revision_details = details;
        self.selected_commit_index = self.find_selected_commit_index();
        if let Some(empty) = wc_empty
            && self.capabilities.graph
            && let Some(index) = self.selected_commit_index
        {
            self.commits.set_is_empty(index, empty);
            self.repaint_version = self.repaint_version.wrapping_add(1);
        }

        let mut effects = vec![Effect::RepaintDocument];
        effects.extend(self.drain_pending_refresh());
        effects
    }

    fn on_snapshot(
        &mut self,
        job: JobId,
        origin: RefreshOrigin,
        snapshot: RepositorySnapshot,
        warnings: Vec<String>,
    ) -> Vec<Effect> {
        if self.snapshot_job.as_ref().map(|(id, _)| *id) != Some(job) {
            return Vec::new();
        }
        self.snapshot_job = None;
        let mut effects: Vec<Effect> = warnings
            .into_iter()
            .map(|note| Effect::Activity(Activity::Note(note)))
            .collect();

        let reflected = self.repository_snapshot.as_ref();
        let changed =
            reflected.map(|s| s.fingerprint.as_str()) != Some(snapshot.fingerprint.as_str());
        if !changed {
            effects.push(Effect::Activity(Activity::LoadFinished {
                ok: true,
                detail: Some("Already up to date".to_owned()),
            }));
            effects.extend(self.drain_pending_refresh());
            return effects;
        }

        // Escalate a watcher (diff-only) refresh to a full reload when ops
        // other than our own snapshot landed since the graph was walked: the
        // snapshot's parent op should be exactly the op the graph reflects. A
        // CLI `jj edit` fires worktree + op-log signals in one debounce window;
        // the worktree signal wins the race and this snapshot advances the
        // fingerprint *past* the external op — without the escalation, that
        // swallowed the topology change and the graph stayed stale.
        let external_op = match (reflected, snapshot.parent_fingerprint.as_deref()) {
            (Some(reflected), Some(parent)) => reflected.fingerprint != parent,
            _ => false,
        };
        let origin = if external_op {
            RefreshOrigin::Focus
        } else {
            origin
        };
        self.repository_snapshot = Some(snapshot.clone());

        match origin {
            RefreshOrigin::Watcher => {
                // A working-tree edit moved `@`'s tree but not the topology, so
                // skip the (up to ~1M-commit) re-walk. Keep `@`'s sidebar
                // "empty" chip live even when the diff pane shows another
                // revision: the snapshot just rewrote `@`'s tree.
                self.apply_working_copy_empty(snapshot.working_copy_empty);
                if matches!(self.selected_revision, RevisionSelection::WorkingCopy) {
                    effects.extend(self.load_diff(RevisionSelection::WorkingCopy));
                }
            }
            RefreshOrigin::Focus => {
                // A real topology change (an external op, a mutation, a fetch).
                // Double-buffered, so the current graph stays up until the
                // replacement is ready.
                effects.extend(self.load_graph(false));
                let revision = self.selected_revision.clone();
                effects.extend(self.load_diff(revision));
            }
        }
        effects
    }

    fn on_mutation(&mut self, job: JobId, outcome: MutationOutcome) -> Vec<Effect> {
        if self.mutation_job != Some(job) {
            return Vec::new();
        }
        self.mutation_job = None;
        // The activity row belongs to whoever started the mutation — it minted
        // the id — so this only folds in what the *view* has to change.
        let mut effects = Vec::new();
        if outcome.moved_working_copy {
            self.selected_revision = RevisionSelection::WorkingCopy;
            effects.push(Effect::RevealSelection);
        } else if let Some(rewritten) = &outcome.rewritten_commit {
            // The commit the caller still addresses by its old id became this
            // one; follow it rather than stranding the selection.
            self.selected_revision = self.canonical_selection(rewritten);
            effects.push(Effect::RevealSelection);
        }
        // The op moved the head; reconcile through the normal snapshot path so
        // the escalation logic is the same one everything else uses.
        effects.extend(self.snapshot(RefreshOrigin::Focus));
        effects
    }

    fn on_empty_status(&mut self, job: JobId, updates: Vec<(usize, bool)>) -> Vec<Effect> {
        let Some((_, graph)) = self.empty_status_job.filter(|(id, _)| *id == job) else {
            return Vec::new();
        };
        self.empty_status_job = None;
        // The graph these row indices address has since been replaced.
        if Some(graph) != self.graph_version || updates.is_empty() {
            return Vec::new();
        }
        for &(index, empty) in &updates {
            if index >= self.commits.len() {
                continue;
            }
            let commit_id = self.commits.row(index).commit_id().to_owned();
            self.empty_cache.insert(commit_id, empty);
            self.commits.set_is_empty(index, empty);
        }
        self.repaint_version = self.repaint_version.wrapping_add(1);
        Vec::new()
    }

    fn on_op_head(&mut self, fingerprint: &str) -> Vec<Effect> {
        let reflected = self
            .repository_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprint.as_str());
        if reflected == Some(fingerprint) {
            return Vec::new();
        }
        self.snapshot(RefreshOrigin::Focus)
    }

    fn on_failed(&mut self, repo: RepoId, job: JobId, error: RepoError) -> Vec<Effect> {
        if !self.owns_job(job) {
            // Somebody else's job — a preview, a source read, a mutation the
            // frontend is tracking. Its owner reports it.
            return Vec::new();
        }
        let owned_graph = self.graph_job.as_ref().is_some_and(|load| load.id == job);
        // A mutation's failure is reported by whoever started it (an immutable
        // rejection turns into an offer, not a toast), so only the slot is
        // cleared here.
        let owned_mutation = self.mutation_job == Some(job);
        self.clear_slot(job);
        self.loading_since = None;
        let message = error.to_string();

        let mut effects = Vec::new();
        if owned_graph {
            // Only a graph load failing means the tab has nothing to show. An
            // operation failing leaves the graph exactly where it was, so it
            // goes to the log and a toast instead of blanking the window.
            self.status = LoadStatus::Failed(message.clone());
            effects.push(Effect::Activity(Activity::LoadFinished {
                ok: false,
                detail: Some(message.clone()),
            }));
        }
        if !owned_mutation {
            effects.push(Effect::Activity(Activity::Toast {
                repo,
                title: "Operation failed".to_owned(),
                body: message,
            }));
        }
        effects.extend(self.drain_pending_refresh());
        effects
    }

    /// Clear whichever slot `job` owns. Called from every terminal event, so a
    /// failure or a cancel can never strand a slot.
    fn clear_slot(&mut self, job: JobId) {
        if self.graph_job.as_ref().is_some_and(|load| load.id == job) {
            self.graph_job = None;
        }
        if self.diff_job.as_ref().is_some_and(|(id, _)| *id == job) {
            self.diff_job = None;
        }
        if self.snapshot_job.as_ref().is_some_and(|(id, _)| *id == job) {
            self.snapshot_job = None;
        }
        if self.empty_status_job.is_some_and(|(id, _)| id == job) {
            self.empty_status_job = None;
        }
        if self.mutation_job == Some(job) {
            self.mutation_job = None;
        }
    }

    /// Run a refresh coalesced while the session was busy, now that it is idle.
    pub fn take_pending_refresh(&mut self) -> Vec<Effect> {
        self.drain_pending_refresh()
    }

    fn drain_pending_refresh(&mut self) -> Vec<Effect> {
        if self.busy() {
            return Vec::new();
        }
        match self.pending_refresh.take() {
            Some(origin) => self.snapshot(origin),
            None => Vec::new(),
        }
    }

    // ── view-facing helpers ─────────────────────────────────────────────

    /// Park the document on screen under the key it was shown for. Commit
    /// diffs are small; the one big entry is the whole-PR document, so past the
    /// cap the commit entries go and it stays.
    fn park_displayed_document(&mut self) {
        const PARKED_DIFF_CAP: usize = 16;

        let key = document_key(&self.selected_revision);
        self.pr_diffs.insert(
            key,
            CachedDiff {
                document: std::mem::take(&mut self.document),
                details: self.revision_details.take(),
            },
        );
        if self.pr_diffs.len() > PARKED_DIFF_CAP {
            self.pr_diffs.retain(|key, _| key.is_empty());
        }
    }

    /// A hex naming the working-copy row is the working copy, not a commit
    /// that happens to be at `@`. Squashing into `@` rewrites it, so an id
    /// captured before the squash points at nothing afterwards; addressing it
    /// as `WorkingCopy` survives.
    pub fn canonical_selection(&self, hex: &str) -> RevisionSelection {
        crate::repo::canonical_selection(
            hex,
            self.commits.working_copy().map(|row| row.commit_id()),
        )
    }

    /// Row index in `commits` of the selected revision, or `None` when it isn't
    /// in the loaded graph (e.g. filtered out by the revset).
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

    /// One-pass `commit-id hex → index` lookup in the loaded log for `wanted`
    /// (early-exit once all are found). Used to order bookmark menus by
    /// proximity to a reference revision.
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

    /// Refresh the working-copy row's "empty" chip from a snapshot, without
    /// touching the diff pane or re-walking. A no-op when the value is unknown
    /// (git), `@` isn't in the loaded graph, or the chip already matches.
    pub fn apply_working_copy_empty(&mut self, empty: Option<bool>) {
        let Some(empty) = empty else { return };
        let Some(index) = self.commits.working_copy_index() else {
            return;
        };
        if self.commits.row(index).is_empty() == Some(empty) {
            return;
        }
        self.commits.set_is_empty(index, empty);
        self.repaint_version = self.repaint_version.wrapping_add(1);
    }

    /// The displayed document was replaced: stamp its new identity and restart
    /// highlight bookkeeping.
    pub fn reset_highlights(&mut self, document_id: u64) {
        self.document_id = document_id;
        self.highlight_in_flight = 0;
        self.highlight_pending.clear();
        self.enqueue_unhighlighted(0);
    }

    /// Queue files from `start` onward that have no syntax spans at all.
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

    /// Apply any cached empty status to the loaded commits and ask the actor to
    /// resolve the rest.
    ///
    /// Each background resolution is an ~8ms parent-tree merge, so a repo with
    /// hundreds of thousands of merge commits would burn tens of minutes of CPU
    /// resolving them all. Cap how many are resolved per load; beyond that,
    /// merges simply keep no "empty" chip. Cached results still apply to every
    /// row, so this only bounds *new* work.
    fn resolve_empty_status(&mut self) -> Vec<Effect> {
        const EMPTY_STATUS_LIMIT: usize = 5_000;

        let mut cached_updates = Vec::new();
        let mut targets = Vec::new();
        for (index, row) in self.commits.iter().enumerate() {
            if row.is_empty().is_some() {
                continue;
            }
            match self.empty_cache.get(row.commit_id()) {
                Some(&empty) => cached_updates.push((index, empty)),
                None if targets.len() < EMPTY_STATUS_LIMIT => {
                    targets.push((index, row.commit_id().to_owned()))
                }
                None => {}
            }
        }
        let had_cached = !cached_updates.is_empty();
        for (index, empty) in cached_updates {
            self.commits.set_is_empty(index, empty);
        }
        if had_cached {
            self.repaint_version = self.repaint_version.wrapping_add(1);
        }
        if targets.is_empty() {
            return Vec::new();
        }
        if let Some((job, _)) = self.empty_status_job.take() {
            return vec![
                Effect::Send(Command::Cancel { job }),
                self.start_empty_status(targets),
            ];
        }
        vec![self.start_empty_status(targets)]
    }

    fn start_empty_status(&mut self, targets: Vec<(usize, String)>) -> Effect {
        let job = self.next_job();
        let graph = self
            .graph_version
            .expect("empty status is only resolved for a graph that finished loading");
        self.empty_status_job = Some((job, graph));
        Effect::Send(Command::EmptyStatus { job, targets })
    }
}

/// The key a parked document is filed under: `""` for the whole-PR diff, the
/// commit id for one of its commits.
fn document_key(revision: &RevisionSelection) -> String {
    match revision {
        RevisionSelection::WorkingCopy => String::new(),
        RevisionSelection::Commit(oid) => oid.clone(),
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::LaneFrame;
    use crate::model::CommitSummary;

    use super::*;

    fn row(index: usize) -> StreamRow {
        StreamRow {
            summary: CommitSummary {
                change_id: format!("change{index:04}"),
                commit_id: format!("commit{index:04}"),
                shortest_change_id_len: Some(8),
                description: format!("row {index}"),
                author: "someone".to_owned(),
                has_description: true,
                is_empty: None,
                has_conflict: false,
                is_divergent: false,
                is_hidden: false,
                change_offset: None,
                is_working_copy: index == 0,
                is_immutable: false,
                bookmarks: Vec::new(),
                parent_ids: if index == 0 {
                    Vec::new()
                } else {
                    vec![format!("commit{:04}", index - 1)]
                },
            },
            frame: LaneFrame::solo(),
        }
    }

    /// Folding a walk one batch at a time must land exactly where folding it in
    /// one shot does. The two used to be separate pipelines — a streaming cold
    /// load and an atomic refresh — and this is the invariant that let them be
    /// unified into one.
    #[test]
    fn batched_and_one_shot_folds_agree() {
        const ROWS: usize = 5_000;
        let all: Vec<StreamRow> = (0..ROWS).map(row).collect();

        let mut expected_commits = CommitStore::default();
        let mut expected_graph = GraphLayout::default();
        let mut expected_cursor = ColdCursor::default();
        let one_shot = fold_cold_batch(
            &mut expected_commits,
            &mut expected_graph,
            &mut expected_cursor,
            all.clone(),
            true,
        );

        for batch_size in [1usize, 256, 4096] {
            let mut commits = CommitStore::default();
            let mut graph = GraphLayout::default();
            let mut cursor = ColdCursor::default();
            let mut prefixes = Vec::new();
            let mut working_copy = None;
            for chunk in all.chunks(batch_size) {
                let fold =
                    fold_cold_batch(&mut commits, &mut graph, &mut cursor, chunk.to_vec(), true);
                prefixes.extend(fold.prefix_lens);
                working_copy = working_copy.or(fold.working_copy_index);
            }
            assert_eq!(commits.len(), expected_commits.len(), "batch {batch_size}");
            assert_eq!(graph.len(), expected_graph.len(), "batch {batch_size}");
            assert_eq!(prefixes, one_shot.prefix_lens, "batch {batch_size}");
            assert_eq!(
                working_copy, one_shot.working_copy_index,
                "batch {batch_size}"
            );
            for index in 0..commits.len() {
                assert_eq!(
                    commits.row(index).commit_id(),
                    expected_commits.row(index).commit_id(),
                    "batch {batch_size}, row {index}"
                );
            }
        }
    }

    /// The graph's identity and the repaint stamp are separate. One counter
    /// for both meant a cosmetic repaint (a chip flipping) invalidated
    /// background results that were still addressing exactly those rows.
    #[test]
    fn a_repaint_does_not_invalidate_the_graph_identity() {
        let mut session = Session::unloaded(String::new());
        let identity = session.graph_version;
        let stamp = session.repaint_version;
        session.apply_working_copy_empty(Some(true));
        session.repaint_version = session.repaint_version.wrapping_add(1);
        assert_eq!(session.graph_version, identity);
        assert_ne!(session.repaint_version, stamp);
    }

    /// A hex that names the working-copy row addresses it as the working copy.
    /// Squashing into `@` rewrites it, so an id captured before the squash
    /// points at nothing afterwards.
    #[test]
    fn a_working_copy_hex_resolves_to_the_working_copy() {
        assert_eq!(
            crate::repo::canonical_selection("abc", Some("abc")),
            RevisionSelection::WorkingCopy
        );
        assert_eq!(
            crate::repo::canonical_selection("abc", Some("def")),
            RevisionSelection::Commit("abc".to_owned())
        );
        assert_eq!(
            crate::repo::canonical_selection("abc", None),
            RevisionSelection::Commit("abc".to_owned())
        );
    }

    /// The pre-flight and the confirmation dialog read one list: re-editing the
    /// commit already checked out rewrites nothing, and the root is never
    /// offered, because the backend refuses it whatever the dialog says.
    #[test]
    fn rewritten_targets_excludes_the_root_and_a_no_op_edit() {
        use crate::mutations::MutationOp;

        let at = RevisionSelection::Commit("abc".to_owned());
        assert!(
            crate::repo::rewritten_targets(&MutationOp::Edit { target: at.clone() }, &at, None)
                .is_empty()
        );
        assert_eq!(
            crate::repo::rewritten_targets(
                &MutationOp::Edit { target: at.clone() },
                &RevisionSelection::WorkingCopy,
                None,
            ),
            vec![at.clone()]
        );
        assert_eq!(
            crate::repo::rewritten_targets(
                &MutationOp::Abandon {
                    targets: vec![RevisionSelection::Commit("root".to_owned()), at.clone()],
                },
                &RevisionSelection::WorkingCopy,
                Some("root"),
            ),
            vec![at]
        );
    }

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
}
