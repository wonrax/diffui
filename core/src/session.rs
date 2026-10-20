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
    BookmarksInfo, BranchStatus, CommitStore, DiffBase, DiffDocument, DiffRequest, LoadProgress,
    RevisionDetails, RevisionSelection, StreamRow,
};
use crate::mutations::{MutationOp, MutationOutcome};
use crate::repo::{
    Capabilities, Command, Event, JobId, Payload, PreviewRequest, RepoError, RepoId,
};
use crate::repository::{FetchTarget, RepositorySnapshot};
use crate::revset::{
    RevsetCompleter, RevsetError, RevsetExpect, RevsetMatch, RevsetQuery, RevsetSymbols,
};

/// A parked diff document for a source that flips between several documents —
/// a PR's "all changes" view vs its per-commit diffs — so flipping back is an
/// in-memory move instead of a re-download. Keyed in [`Session::pr_diffs`]
/// by commit id (`""` = the whole-PR diff).
#[derive(Debug, Clone, Default)]
pub struct CachedDiff {
    pub document: DiffDocument,
    pub details: Option<RevisionDetails>,
}

/// Files with more diff rows than this are left uncoloured.
pub const HIGHLIGHT_MAX_ROWS: usize = 10_000;

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
    /// The "from" side the next diff load uses. Sticky: selecting another
    /// revision while comparing compares that one against the same base.
    pub diff_base: DiffBase,
    /// The base `document` was actually loaded against — what the diff
    /// pane's header describes and what full-context reads must match.
    pub document_base: DiffBase,
    /// "Diff since": while set (`trunk()`), browsing a revision X shows the
    /// range `base..X`, so the mode follows the graph's cursor the way a
    /// comparison's base does. See [`Self::diff_since`].
    pub diff_since: Option<String>,
    /// A span of graph rows shown as one diff (`jj diff -r` over them). See
    /// [`Self::diff_span`].
    pub span: Option<DiffSpan>,
    /// Files of the shown diff the user expanded past the size caps. Sent
    /// with every reload of the same diff so a refresh doesn't fold them
    /// back; dropped when the revision or the base changes.
    pub expanded_paths: Vec<String>,
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
    /// Revset aliases and workspace names from the last graph load, for
    /// revset completion (see [`Session::revset_completer`]).
    pub revset_symbols: RevsetSymbols,
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
    /// The diff switch in flight: the revision and the base it was asked for.
    diff_job: Option<(JobId, RevisionSelection, DiffBase)>,
    snapshot_job: Option<(JobId, RefreshOrigin)>,
    /// The sweep in flight, with the graph job whose rows it addresses.
    empty_status_job: Option<(JobId, JobId)>,
    /// The job of the graph currently on screen — the identity a background
    /// per-row result is checked against.
    graph_version: Option<JobId>,
    mutation_job: Option<JobId>,
    /// The operation the rows on screen reflect: the op the last graph walk
    /// read, advanced by a watcher snapshot that patched `@` in place. Kept
    /// apart from [`Session::repository_snapshot`], which a snapshot advances
    /// whether or not the rows followed — comparing against that is how a
    /// stale graph used to answer a manual refresh with "already up to date".
    /// `None` until a walk reports one (and always for git).
    graph_fingerprint: Option<String>,
    /// Collapsed files being fetched in full, by path.
    expand_jobs: Vec<(JobId, String)>,
    /// A typed `jj diff` being resolved into a revision and base.
    diff_resolve_job: Option<JobId>,
    /// The check that a [`DiffSpan`] has no gaps.
    span_job: Option<JobId>,
}

/// Several graph rows picked to be shown as one diff, the way `jj diff -r`
/// shows a set of commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    /// The rows as a revset: their union.
    pub revset: String,
    /// How many rows were picked.
    pub count: usize,
    /// Why they can't be one diff: jj's refusal of a set with gaps, once the
    /// check has answered. The pane then says so instead of guessing.
    pub refused: Option<RevsetError>,
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
            || self.diff_job.as_ref().is_some_and(|(id, ..)| *id == job)
            || self.snapshot_job.as_ref().is_some_and(|(id, _)| *id == job)
            || self.empty_status_job.is_some_and(|(id, _)| id == job)
            || self.mutation_job == Some(job)
            || self.expand_jobs.iter().any(|(id, _)| *id == job)
            || self.diff_resolve_job == Some(job)
            || self.span_job == Some(job)
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
        self.diff_job.as_ref().map(|(_, revision, _)| revision)
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
        if let Some((job, ..)) = self.diff_job.take() {
            effects.push(Effect::Send(Command::Cancel { job }));
        }
        let job = self.next_job();
        let base = self.diff_base.clone();
        if revision != self.selected_revision || base != self.document_base {
            self.expanded_paths.clear();
        }
        self.diff_job = Some((job, revision.clone(), base.clone()));
        self.loading_since = Some(Instant::now());
        effects.push(Effect::Send(Command::LoadDiff {
            job,
            revision,
            base,
            expand: self.expanded_paths.clone(),
        }));
        effects
    }

    /// Fetch a file the backend collapsed for size in full. The path also
    /// joins [`Self::expanded_paths`], so a reload landing meanwhile already
    /// carries it expanded.
    pub fn expand_file(&mut self, path: &str) -> Vec<Effect> {
        let collapsed = self
            .document
            .files
            .iter()
            .any(|file| file.path == path && file.collapsed.is_some());
        if !self.capabilities.compare
            || !collapsed
            || self.expand_jobs.iter().any(|(_, pending)| pending == path)
        {
            return Vec::new();
        }
        let job = self.next_job();
        self.expand_jobs.push((job, path.to_owned()));
        if !self.expanded_paths.iter().any(|expanded| expanded == path) {
            self.expanded_paths.push(path.to_owned());
        }
        vec![Effect::Send(Command::ExpandFile {
            job,
            revision: self.selected_revision.clone(),
            base: self.document_base.clone(),
            path: path.to_owned(),
        })]
    }

    fn on_file_expanded(
        &mut self,
        job: JobId,
        path: String,
        file: Option<crate::model::DiffFile>,
    ) -> Vec<Effect> {
        let Some(index) = self.expand_jobs.iter().position(|(id, _)| *id == job) else {
            return Vec::new();
        };
        self.expand_jobs.remove(index);
        let (Some(file), Some(slot)) = (
            file,
            self.document
                .files
                .iter_mut()
                .find(|shown| shown.path == path && shown.collapsed.is_some()),
        ) else {
            return Vec::new();
        };
        *slot = file;
        vec![Effect::RepaintDocument]
    }

    // ── comparing two revisions ─────────────────────────────────────────

    /// Show `to`'s diff against `base` (`jj diff --from` / `jj interdiff
    /// --from`). The base sticks: selecting another revision afterwards
    /// compares that one against the same base, until [`Self::exit_compare`].
    pub fn compare(&mut self, base: DiffBase, to: RevisionSelection) -> Vec<Effect> {
        self.diff_since = None;
        self.drop_span();
        self.load_against(base, to)
    }

    /// [`Self::compare`] without leaving "diff since" or a span.
    fn load_against(&mut self, base: DiffBase, to: RevisionSelection) -> Vec<Effect> {
        if !self.capabilities.compare && base.is_compare()
            || !self.capabilities.range_diff && base.range().is_some()
        {
            return Vec::new();
        }
        self.diff_base = base;
        self.load_diff(to)
    }

    /// Back to the selected revision's own diff.
    pub fn exit_compare(&mut self) -> Vec<Effect> {
        let had_span = self.span.is_some();
        self.diff_since = None;
        self.drop_span();
        if !self.diff_base.is_compare() && !self.document_base.is_compare() {
            // A refused span showed no diff of its own; leaving it is enough.
            return if had_span {
                vec![Effect::RepaintDocument]
            } else {
                Vec::new()
            };
        }
        self.diff_base = DiffBase::Parents;
        let revision = self.selected_revision.clone();
        self.load_diff(revision)
    }

    /// Swap the two sides: the base becomes the selected revision and the
    /// selected revision the base.
    pub fn swap_compare(&mut self) -> Vec<Effect> {
        let to = self.selected_revision.clone();
        let (base, from) = match &self.diff_base {
            DiffBase::Parents | DiffBase::Range(_) => return Vec::new(),
            DiffBase::Revision(from) => (DiffBase::Revision(to), from.clone()),
            DiffBase::Interdiff(from) => (DiffBase::Interdiff(to), from.clone()),
        };
        self.compare(base, from)
    }

    /// Flip a comparison between a plain tree diff and an interdiff of the
    /// same two revisions.
    pub fn set_interdiff(&mut self, interdiff: bool) -> Vec<Effect> {
        let base = match (&self.diff_base, interdiff) {
            (DiffBase::Revision(from), true) => DiffBase::Interdiff(from.clone()),
            (DiffBase::Interdiff(from), false) => DiffBase::Revision(from.clone()),
            _ => return Vec::new(),
        };
        let to = self.selected_revision.clone();
        self.compare(base, to)
    }

    // ── ranges that follow the graph ────────────────────────────────────

    /// How a revset names `revision`: `@`, its change id as the graph
    /// shows it, or a commit id prefix where the change id doesn't single
    /// it out (a divergent change, a hidden commit, a row not loaded).
    pub fn revision_symbol(&self, revision: &RevisionSelection) -> String {
        use crate::cli::RevisionNames as _;
        match revision {
            RevisionSelection::WorkingCopy => "@".to_owned(),
            RevisionSelection::Commit(hex) => self
                .commits
                .change_id(hex)
                .unwrap_or_else(|| hex.chars().take(12).collect()),
        }
    }

    /// Show `base..X` for `target` X ("Diff since trunk"), and keep doing so
    /// for whatever revision is browsed next (see [`Self::browse`]), until
    /// [`Self::exit_compare`]. `symbol` names X in the revset when the user
    /// picked it by name (a bookmark), so the range reads as they'd type it.
    /// A revision already in `base` gives the empty range.
    pub fn diff_since(
        &mut self,
        base: impl Into<String>,
        target: RevisionSelection,
        symbol: Option<String>,
    ) -> Vec<Effect> {
        if !self.capabilities.range_diff {
            return Vec::new();
        }
        let base = base.into();
        self.drop_span();
        let symbol = symbol.unwrap_or_else(|| self.revision_symbol(&target));
        let revset = format!("{base}..{symbol}");
        self.diff_since = Some(base);
        self.load_against(DiffBase::Range(revset), target)
    }

    /// Browse `revision` (a click on its row, a keyboard move): show its
    /// diff, as the diff pane's mode has it. A comparison compares it with
    /// the same base; "diff since" re-targets its range at it; a fixed range
    /// or a span has nothing to say about it, so it leaves them.
    pub fn browse(&mut self, revision: RevisionSelection) -> Vec<Effect> {
        if let Some(base) = self.diff_since.clone() {
            let revset = format!("{base}..{}", self.revision_symbol(&revision));
            self.diff_base = DiffBase::Range(revset);
        } else if self.span.take().is_some() || matches!(self.diff_base, DiffBase::Range(_)) {
            self.span_job = None;
            self.diff_base = DiffBase::Parents;
        }
        self.load_diff(revision)
    }

    /// Show `revisions` (rows picked on the graph) as one diff: `jj diff -r`
    /// over their union, once the actor confirms the set has no gaps. A set
    /// with gaps keeps the pane as it was and records jj's refusal in
    /// [`Self::span`]. Fewer than two rows is no span: it leaves one.
    pub fn diff_span(&mut self, revisions: &[RevisionSelection]) -> Vec<Effect> {
        if !self.capabilities.range_diff {
            return Vec::new();
        }
        if revisions.len() < 2 {
            return if self.span.is_some() {
                self.exit_compare()
            } else {
                Vec::new()
            };
        }
        let revset = revisions
            .iter()
            .map(|revision| self.revision_symbol(revision))
            .collect::<Vec<_>>()
            .join(" | ");
        let mut effects = Vec::new();
        if let Some(job) = self.span_job.take() {
            effects.push(Effect::Send(Command::Cancel { job }));
        }
        let job = self.next_job();
        self.span_job = Some(job);
        self.span = Some(DiffSpan {
            revset: revset.clone(),
            count: revisions.len(),
            refused: None,
        });
        let mut query = RevsetQuery::new(revset).expect(RevsetExpect::Contiguous);
        query.limit = 0;
        effects.push(Effect::Send(Command::ResolveRevset { job, query }));
        effects
    }

    fn on_span_checked(
        &mut self,
        job: JobId,
        result: Result<RevsetMatch, RevsetError>,
    ) -> Vec<Effect> {
        if self.span_job != Some(job) {
            return Vec::new();
        }
        self.span_job = None;
        let Some(span) = self.span.as_mut() else {
            return Vec::new();
        };
        match result {
            Ok(_) => {
                self.diff_since = None;
                let revset = span.revset.clone();
                let to = self.selected_revision.clone();
                self.load_against(DiffBase::Range(revset), to)
            }
            Err(error) => {
                span.refused = Some(error);
                vec![Effect::RepaintDocument]
            }
        }
    }

    /// Forget the span, and the check still running for it.
    fn drop_span(&mut self) {
        self.span = None;
        self.span_job = None;
    }

    // ── typed revsets ───────────────────────────────────────────────────

    /// Show the combined diff of the commits `revset` matches (`jj diff -r`
    /// with several), as a sticky base like a comparison: it stays until
    /// [`Self::exit_compare`], and is re-resolved on every reload, so a range
    /// ending at `@` follows the working copy. A set with gaps fails the load
    /// with jj's message and falls back to the selected revision's own diff.
    /// [`DiffDocument::range`] says what the range held once it lands.
    pub fn diff_range(&mut self, revset: impl Into<String>) -> Vec<Effect> {
        let to = self.selected_revision.clone();
        self.compare(DiffBase::Range(revset.into()), to)
    }

    /// Show what `jj diff` with these arguments would: one commit from `-r`
    /// is selected and shown as itself, several are a [`Self::diff_range`],
    /// `--from`/`--to` a comparison. Revsets are resolved by the actor with
    /// jj's rules first; one jj would refuse (a gap, a `--from` matching two
    /// commits) ends in a toast with jj's message, leaving the pane as it was.
    pub fn diff_revsets(&mut self, request: DiffRequest) -> Vec<Effect> {
        if !self.capabilities.range_diff {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some(job) = self.diff_resolve_job.take() {
            effects.push(Effect::Send(Command::Cancel { job }));
        }
        let job = self.next_job();
        self.diff_resolve_job = Some(job);
        effects.push(Effect::Send(Command::ResolveDiff { job, request }));
        effects
    }

    fn on_diff_resolved(
        &mut self,
        job: JobId,
        revision: RevisionSelection,
        base: DiffBase,
    ) -> Vec<Effect> {
        if self.diff_resolve_job != Some(job) {
            return Vec::new();
        }
        self.diff_resolve_job = None;
        self.compare(base, revision)
    }

    /// Parse and resolve a revset against the repository, for live feedback
    /// as it is typed: a count, the rows to tint, or jj's error with its span.
    /// The answer is a [`Payload::RevsetResolved`] the frontend routes by the
    /// returned job, like a preview; superseding a query is the caller's
    /// `Command::Cancel` for the old job.
    pub fn resolve_revset(&mut self, query: RevsetQuery) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        (
            job,
            vec![Effect::Send(Command::ResolveRevset { job, query })],
        )
    }

    /// Revset completion over this session's graph, bookmarks and aliases.
    /// `history` is the caller's previously used revsets, newest first.
    pub fn revset_completer<'a>(&'a self, history: &'a [String]) -> RevsetCompleter<'a> {
        RevsetCompleter {
            symbols: &self.revset_symbols,
            bookmarks: &self.bookmarks,
            commits: &self.commits,
            history,
        }
    }

    /// Whether the diff on screen (or on its way) changes when the working
    /// copy does: `@` is on one side of it, or in the range it covers.
    fn diff_follows_working_copy(&self) -> bool {
        matches!(self.selected_revision, RevisionSelection::WorkingCopy)
            && !matches!(self.diff_base, DiffBase::Range(_))
            || self.diff_base.involves_working_copy()
            || self.diff_base.range().is_some()
                && self
                    .document
                    .range
                    .as_ref()
                    .is_none_or(|range| range.includes_working_copy)
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
                seen_op: self.graph_fingerprint.clone(),
            })],
        )
    }

    pub fn fetch(&mut self, target: FetchTarget) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        (job, vec![Effect::Send(Command::Fetch { job, target })])
    }

    /// Simulate `draft`, asking for the graph it would leave over the loaded
    /// revset too.
    pub fn preview(&mut self, draft: PreviewRequest) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        let graph = Some(self.revset.clone());
        (
            job,
            vec![Effect::Send(Command::Preview {
                job,
                draft,
                graph,
                seen_op: self.graph_fingerprint.clone(),
            })],
        )
    }

    /// Ask whether moving bookmark `name` to `to` is backwards or sideways,
    /// resolving `to` the way the move itself will.
    pub fn bookmark_check(&mut self, name: String, to: RevisionSelection) -> (JobId, Vec<Effect>) {
        let job = self.next_job();
        (
            job,
            vec![Effect::Send(Command::BookmarkCheck {
                job,
                name,
                to,
                seen_op: self.graph_fingerprint.clone(),
            })],
        )
    }

    /// The job of the graph on screen: the identity a result laid over the
    /// loaded rows is checked against.
    pub fn graph_version(&self) -> Option<JobId> {
        self.graph_version
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
            Payload::FileExpanded { job, path, file } => self.on_file_expanded(job, path, file),
            Payload::EmptyStatus { job, updates } => self.on_empty_status(job, updates),
            Payload::Progress { loaded, total, .. } => {
                self.commit_progress.set_total(total);
                self.commit_progress.set_loaded(loaded);
                Vec::new()
            }
            Payload::WorkingCopyChanged => self.snapshot(RefreshOrigin::Watcher),
            Payload::StaleWorkingCopyRecovered => {
                const NOTICE: &str = "Working copy was stale (changed by another process); \
                                      updated to the latest operation";
                vec![
                    Effect::Activity(Activity::Toast {
                        repo,
                        title: "Working copy updated".to_owned(),
                        body: NOTICE.to_owned(),
                    }),
                    Effect::Activity(Activity::Note(NOTICE.to_owned())),
                ]
            }
            Payload::RepositoryChanged => self.snapshot(RefreshOrigin::Focus),
            Payload::OpHeadChanged { fingerprint } => self.on_op_head(&fingerprint),
            Payload::DiffResolved {
                job,
                revision,
                base,
            } => self.on_diff_resolved(job, revision, base),
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
            | Payload::BookmarkChecked { .. }
            | Payload::ConflictInfoLoaded { .. }
            | Payload::MergeToolPrepared { .. }
            | Payload::ConflictTextLoaded { .. }
            | Payload::OpLogBatch { .. }
            | Payload::History { .. }
            | Payload::CliOutput { .. }
            | Payload::CliExited { .. } => Vec::new(),
            Payload::RevsetResolved { job, result, .. } => self.on_span_checked(job, result),
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
        // The change the selected row stands for, read off the store about to
        // be replaced: a rewrite gives the commit a new id, and the selection
        // should follow the change rather than fall off the graph.
        let selected_change = match &self.selected_revision {
            RevisionSelection::Commit(id) if swapped => self
                .commits
                .find_by_commit_id(id)
                .map(|row| row.change_id().to_owned()),
            _ => None,
        };
        if let Some((commits, graph, prefixes)) = load.staging {
            self.commits = commits;
            self.graph = graph;
            self.sidebar_prefix_lens = prefixes;
        }
        self.graph_version = Some(job);
        self.graph_fingerprint = tail.op_id;
        self.branch_status = tail.branch_status;
        self.bookmarks = tail.bookmarks;
        self.revset_symbols = tail.symbols;
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
        let followed = self.selected_commit_index.is_none()
            && selected_change.is_some_and(|change| self.follow_selected_change(&change));
        self.repaint_version = self.repaint_version.wrapping_add(1);

        let mut effects = vec![Effect::Activity(Activity::LoadFinished {
            ok: true,
            detail: None,
        })];
        if swapped {
            effects.push(Effect::GraphReplaced);
        }
        if followed && !self.diff_in_flight() {
            let revision = self.selected_revision.clone();
            effects.extend(self.load_diff(revision));
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
        let Some((_, _, base)) = self.diff_job.take_if(|(id, ..)| *id == job) else {
            return Vec::new();
        };
        self.loading_since = None;

        // A working-copy diff is the definitive emptiness signal for `@` (files
        // present ⇒ not empty), so an edit flips the chip without a re-walk.
        // Synthesized conflict entries don't count: jj calls a conflicted merge
        // whose tree is exactly its parents' merge "empty", and the chip should
        // agree with the graph walk. A comparison says nothing about `@` alone.
        let own_diff = !base.is_compare();
        let wc_empty =
            (own_diff && matches!(revision, RevisionSelection::WorkingCopy)).then(|| {
                document
                    .files
                    .iter()
                    .all(|file| file.status == crate::model::DiffFileStatus::Conflicted)
            });

        if !self.capabilities.graph {
            self.park_displayed_document();
        }
        // Every backend's document lands here, so this is the one place that
        // makes file order mean the same thing to the diff view, the tree and
        // next/previous file.
        let mut document = document;
        document.sort_in_tree_order();
        self.selected_revision = revision;
        self.document_base = base;
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
        // The rows on screen are behind this snapshot when the walk that built
        // them read another op — even if an earlier snapshot already recorded
        // this one. Comparing only against the last snapshot is what let a
        // manual refresh call a stale graph "already up to date" forever.
        let graph_behind = self
            .graph_fingerprint
            .as_deref()
            .is_some_and(|op| op != snapshot.fingerprint);
        let changed = graph_behind
            || reflected.map(|s| s.fingerprint.as_str()) != Some(snapshot.fingerprint.as_str());
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
        let base = self
            .graph_fingerprint
            .as_deref()
            .or(reflected.map(|s| s.fingerprint.as_str()));
        let external_op = match (base, snapshot.parent_fingerprint.as_deref()) {
            (Some(base), Some(parent)) => base != parent,
            _ => false,
        };
        // A snapshot under an edited mid-stack commit rebased its children:
        // every one of them has a new id, and the rows still name the old
        // ones, which no mutation can act on any more.
        let origin = if external_op || snapshot.rewrote_descendants {
            RefreshOrigin::Focus
        } else {
            origin
        };
        self.repository_snapshot = Some(snapshot.clone());

        match origin {
            RefreshOrigin::Watcher => {
                // A working-tree edit moved `@`'s tree but not the topology, so
                // skip the (up to ~1M-commit) re-walk. The snapshot did give
                // `@` a new commit id, though, and nothing else changed, so
                // patching that one row brings the graph to the snapshot's op.
                // Keep `@`'s sidebar "empty" chip live even when the diff pane
                // shows another revision: the snapshot just rewrote `@`'s tree.
                self.apply_working_copy_commit_id(snapshot.working_copy_commit_id.as_deref());
                if self.graph_fingerprint.is_some() {
                    self.graph_fingerprint = Some(snapshot.fingerprint.clone());
                }
                self.apply_working_copy_empty(snapshot.working_copy_empty);
                if self.diff_follows_working_copy() {
                    let revision = self.selected_revision.clone();
                    effects.extend(self.load_diff(revision));
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
        // A comparison whose base no longer resolves (rewritten or abandoned
        // since it was picked) falls back to the plain diff rather than
        // leaving the pane on a comparison it can't refresh.
        let failed_compare = self
            .diff_job
            .as_ref()
            .is_some_and(|(id, _, base)| *id == job && base.is_compare());
        let failed_range = self
            .diff_job
            .as_ref()
            .is_some_and(|(id, _, base)| *id == job && base.range().is_some());
        let failed_resolve = self.diff_resolve_job == Some(job);
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
                title: if failed_range {
                    "Range diff ended".to_owned()
                } else if failed_compare {
                    "Comparison ended".to_owned()
                } else if failed_resolve {
                    "Can't diff that".to_owned()
                } else {
                    "Operation failed".to_owned()
                },
                body: message,
            }));
        }
        if failed_compare && self.diff_base.is_compare() {
            self.diff_base = DiffBase::Parents;
            self.diff_since = None;
            self.drop_span();
            let revision = self.selected_revision.clone();
            effects.extend(self.load_diff(revision));
            return effects;
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
        if self.diff_job.as_ref().is_some_and(|(id, ..)| *id == job) {
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
        if self.diff_resolve_job == Some(job) {
            self.diff_resolve_job = None;
        }
        if self.span_job == Some(job) {
            self.span_job = None;
        }
        self.expand_jobs.retain(|(id, _)| *id != job);
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

    /// Point the working-copy row at `@`'s post-snapshot commit id. A no-op
    /// when the id is unknown (git) or `@` isn't in the loaded graph.
    fn apply_working_copy_commit_id(&mut self, commit_id: Option<&str>) {
        let (Some(commit_id), Some(index)) = (commit_id, self.commits.working_copy_index()) else {
            return;
        };
        if self.commits.row(index).commit_id() != commit_id {
            self.commits.set_commit_id(index, commit_id);
            self.repaint_version = self.repaint_version.wrapping_add(1);
        }
    }

    /// Move a commit selection the reload dropped onto the row now carrying
    /// its change, when exactly one does. Returns whether it moved.
    fn follow_selected_change(&mut self, change_id: &str) -> bool {
        let Some(row) = self.commits.find_by_change_id(change_id) else {
            return false;
        };
        if row.is_divergent() {
            return false;
        }
        let commit_id = row.commit_id().to_owned();
        self.selected_revision = self.canonical_selection(&commit_id);
        self.selected_commit_index = self.find_selected_commit_index();
        self.selected_commit_index.is_some()
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
            // A collapsed file has no rows to colour, and past a size the
            // full-source parse costs more than the colour is worth.
            let rows: usize = file.hunks.iter().map(|hunk| hunk.lines.len()).sum();
            if file.collapsed.is_some() || rows > HIGHLIGHT_MAX_ROWS {
                continue;
            }
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
                on_trunk: false,
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

    /// The diff view walks `document.files` and the changed-files tree lists
    /// directories first, so a document left in the backend's path order reads
    /// differently in the two panes — and "next file" skips around the tree.
    #[test]
    fn a_landed_diff_is_in_the_order_the_file_tree_lists_it() {
        let file = |path: &str| crate::model::DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: crate::model::DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 1,
            deletions: 0,
            collapsed: None,
            blob: None,
        };
        let mut session = Session::unloaded(String::new());
        let job = session
            .load_diff(RevisionSelection::WorkingCopy)
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send(Command::LoadDiff { job, .. }) => Some(job),
                _ => None,
            })
            .expect("a diff load");

        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/ordering".into(),
            scope: Default::default(),
        });
        session.apply(Event::new(
            repo,
            Payload::DiffLoaded {
                job,
                revision: RevisionSelection::WorkingCopy,
                document: DiffDocument {
                    files: vec![
                        file("README.md"),
                        file("src/main.rs"),
                        file("src/nested/deep.rs"),
                    ],
                    total_additions: 3,
                    total_deletions: 0,
                    range: None,
                },
                details: None,
            },
        ));

        let paths: Vec<&str> = session
            .document
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["src/nested/deep.rs", "src/main.rs", "README.md"]);
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

    fn sent_job(effects: &[Effect], pick: fn(&Command) -> Option<JobId>) -> Option<JobId> {
        effects.iter().find_map(|effect| match effect {
            Effect::Send(command) => pick(command),
            _ => None,
        })
    }

    /// The rows on screen were walked at op `a`; a snapshot has since recorded
    /// op `b` without them following. A refresh must reload instead of calling
    /// that "already up to date" because the *snapshot* is current.
    #[test]
    fn a_refresh_reloads_rows_walked_at_an_older_operation() {
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/graph-behind".into(),
            scope: Default::default(),
        });
        let mut session = Session::unloaded(String::new());
        session.capabilities.graph = true;
        let load = session.load_graph(false);
        let job = sent_job(&load, |command| match command {
            Command::LoadGraph { job, .. } => Some(*job),
            _ => None,
        })
        .expect("a graph load");
        session.apply(Event::new(
            repo.clone(),
            Payload::GraphLoaded {
                job,
                tail: crate::repo::GraphTail {
                    op_id: Some("a".to_owned()),
                    ..Default::default()
                },
            },
        ));
        let at_b = RepositorySnapshot {
            fingerprint: "b".to_owned(),
            parent_fingerprint: Some("b".to_owned()),
            ..Default::default()
        };
        session.repository_snapshot = Some(at_b.clone());

        let refresh = session.snapshot(RefreshOrigin::Focus);
        let job = sent_job(&refresh, |command| match command {
            Command::Snapshot { job, .. } => Some(*job),
            _ => None,
        })
        .expect("a snapshot");
        let effects = session.apply(Event::new(
            repo,
            Payload::SnapshotDone {
                job,
                origin: RefreshOrigin::Focus,
                snapshot: at_b,
                warnings: Vec::new(),
            },
        ));
        assert!(
            sent_job(&effects, |command| match command {
                Command::LoadGraph { job, .. } => Some(*job),
                _ => None,
            })
            .is_some(),
            "the stale rows must be reloaded: {effects:?}"
        );
    }

    /// A save under a childless `@` patches that one row's commit id rather
    /// than re-walking, and the rows then count as current at the new op.
    #[test]
    fn a_watcher_snapshot_patches_the_working_copy_row() {
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/patch-wc".into(),
            scope: Default::default(),
        });
        let mut session = Session::unloaded(String::new());
        session.capabilities.graph = true;
        let load = session.load_graph(true);
        let job = sent_job(&load, |command| match command {
            Command::LoadGraph { job, .. } => Some(*job),
            _ => None,
        })
        .expect("a graph load");
        session.apply(Event::new(
            repo.clone(),
            Payload::Batch {
                job,
                rows: (0..3).map(row).collect(),
            },
        ));
        session.apply(Event::new(
            repo.clone(),
            Payload::GraphLoaded {
                job,
                tail: crate::repo::GraphTail {
                    op_id: Some("a".to_owned()),
                    ..Default::default()
                },
            },
        ));
        session.repository_snapshot = Some(RepositorySnapshot {
            fingerprint: "a".to_owned(),
            ..Default::default()
        });

        let refresh = session.snapshot(RefreshOrigin::Watcher);
        let job = sent_job(&refresh, |command| match command {
            Command::Snapshot { job, .. } => Some(*job),
            _ => None,
        })
        .expect("a snapshot");
        let effects = session.apply(Event::new(
            repo,
            Payload::SnapshotDone {
                job,
                origin: RefreshOrigin::Watcher,
                snapshot: RepositorySnapshot {
                    fingerprint: "b".to_owned(),
                    parent_fingerprint: Some("a".to_owned()),
                    working_copy_commit_id: Some("rewritten".to_owned()),
                    ..Default::default()
                },
                warnings: Vec::new(),
            },
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Send(Command::LoadGraph { .. }))),
            "no re-walk for a plain save: {effects:?}"
        );
        assert_eq!(
            session.commits.working_copy().map(|row| row.commit_id()),
            Some("rewritten")
        );
        assert!(session.commits.find_by_commit_id("rewritten").is_some());
        assert_eq!(session.graph_fingerprint.as_deref(), Some("b"));
    }

    /// Every command that resolves revision targets tells the actor which op
    /// the rows reflect, so a hidden commit picked off them resolves exactly
    /// and a stale one follows its change.
    #[test]
    fn commands_that_resolve_targets_carry_the_rows_op() {
        let mut session = Session::unloaded(String::new());
        session.graph_fingerprint = Some("seen".to_owned());
        let target = RevisionSelection::Commit("abc".to_owned());
        let (_, mutate) = session.mutate(
            MutationOp::Edit {
                target: target.clone(),
            },
            false,
        );
        let (_, preview) = session.preview(PreviewRequest::Merge {
            parents: vec![target.clone()],
        });
        let (_, check) = session.bookmark_check("main".to_owned(), target);
        for effects in [mutate, preview, check] {
            let seen = effects.iter().find_map(|effect| match effect {
                Effect::Send(
                    Command::Mutate { seen_op, .. }
                    | Command::Preview { seen_op, .. }
                    | Command::BookmarkCheck { seen_op, .. },
                ) => Some(seen_op.clone()),
                _ => None,
            });
            assert_eq!(seen, Some(Some("seen".to_owned())), "{effects:?}");
        }
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

    // ── comparing ───────────────────────────────────────────────────────

    fn compare_session() -> Session {
        let mut session = Session::unloaded(String::new());
        session.capabilities.compare = true;
        session.capabilities.graph = true;
        session
    }

    fn sent_diff(effects: &[Effect]) -> Option<(JobId, RevisionSelection, DiffBase)> {
        effects.iter().find_map(|effect| match effect {
            Effect::Send(Command::LoadDiff {
                job,
                revision,
                base,
                ..
            }) => Some((*job, revision.clone(), base.clone())),
            _ => None,
        })
    }

    fn land(session: &mut Session, job: JobId, revision: RevisionSelection) -> Vec<Effect> {
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/compare".into(),
            scope: Default::default(),
        });
        session.apply(Event::new(
            repo,
            Payload::DiffLoaded {
                job,
                revision,
                document: DiffDocument::default(),
                details: None,
            },
        ))
    }

    fn commit(id: &str) -> RevisionSelection {
        RevisionSelection::Commit(id.to_owned())
    }

    /// The base sticks across selections until the comparison is exited, and
    /// the displayed base only moves when a load lands.
    #[test]
    fn a_comparison_base_sticks_until_exit() {
        let mut session = compare_session();
        let effects = session.compare(DiffBase::Revision(commit("aaa")), commit("bbb"));
        let (job, revision, base) = sent_diff(&effects).expect("a diff load");
        assert_eq!(revision, commit("bbb"));
        assert_eq!(base, DiffBase::Revision(commit("aaa")));
        assert_eq!(session.document_base, DiffBase::Parents, "not landed yet");
        land(&mut session, job, revision);
        assert_eq!(session.document_base, DiffBase::Revision(commit("aaa")));

        // Picking another revision compares it against the same base.
        let effects = session.load_diff(commit("ccc"));
        let (job, revision, base) = sent_diff(&effects).expect("a diff load");
        assert_eq!(base, DiffBase::Revision(commit("aaa")));
        land(&mut session, job, revision);

        let effects = session.exit_compare();
        let (job, revision, base) = sent_diff(&effects).expect("a diff load");
        assert_eq!((revision.clone(), base), (commit("ccc"), DiffBase::Parents));
        land(&mut session, job, revision);
        assert_eq!(session.document_base, DiffBase::Parents);
        assert!(session.exit_compare().is_empty(), "nothing left to exit");
    }

    #[test]
    fn swapping_exchanges_the_two_sides_and_keeps_the_mode() {
        let mut session = compare_session();
        let effects = session.compare(DiffBase::Interdiff(commit("aaa")), commit("bbb"));
        let (job, revision, _) = sent_diff(&effects).expect("a diff load");
        land(&mut session, job, revision);

        let effects = session.swap_compare();
        let (_, revision, base) = sent_diff(&effects).expect("a diff load");
        assert_eq!(revision, commit("aaa"));
        assert_eq!(base, DiffBase::Interdiff(commit("bbb")));

        let effects = session.set_interdiff(false);
        let (_, _, base) = sent_diff(&effects).expect("a diff load");
        assert_eq!(base, DiffBase::Revision(commit("bbb")));
    }

    /// A backend without comparisons (git, a PR) never sends one.
    #[test]
    fn comparing_is_gated_on_the_capability() {
        let mut session = Session::unloaded(String::new());
        assert!(
            session
                .compare(DiffBase::Revision(commit("aaa")), commit("bbb"))
                .is_empty()
        );
        assert_eq!(session.diff_base, DiffBase::Parents);
    }

    /// A comparison against `@` reloads on a working-tree edit even while a
    /// commit is the selected side.
    #[test]
    fn a_working_copy_base_reloads_on_a_watcher_refresh() {
        let mut session = compare_session();
        let effects = session.compare(
            DiffBase::Revision(RevisionSelection::WorkingCopy),
            commit("bbb"),
        );
        let (job, revision, _) = sent_diff(&effects).expect("a diff load");
        land(&mut session, job, revision);
        session.repository_snapshot = Some(RepositorySnapshot {
            fingerprint: "op1".to_owned(),
            working_copy_empty: None,
            parent_fingerprint: None,
            ..Default::default()
        });

        let job = match session.snapshot(RefreshOrigin::Watcher).as_slice() {
            [Effect::Send(Command::Snapshot { job, .. })] => *job,
            other => panic!("expected a snapshot, got {other:?}"),
        };
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/compare".into(),
            scope: Default::default(),
        });
        let effects = session.apply(Event::new(
            repo,
            Payload::SnapshotDone {
                job,
                origin: RefreshOrigin::Watcher,
                snapshot: RepositorySnapshot {
                    fingerprint: "op2".to_owned(),
                    working_copy_empty: None,
                    parent_fingerprint: Some("op1".to_owned()),
                    ..Default::default()
                },
                warnings: Vec::new(),
            },
        ));
        let (_, revision, base) = sent_diff(&effects).expect("the comparison reloads");
        assert_eq!(revision, commit("bbb"));
        assert_eq!(base, DiffBase::Revision(RevisionSelection::WorkingCopy));
    }

    /// A comparison whose base stopped resolving falls back to the plain diff.
    #[test]
    fn a_failed_comparison_falls_back_to_the_plain_diff() {
        let mut session = compare_session();
        let effects = session.compare(DiffBase::Revision(commit("gone")), commit("bbb"));
        let (job, ..) = sent_diff(&effects).expect("a diff load");
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/compare".into(),
            scope: Default::default(),
        });
        let effects = session.apply(Event::new(
            repo,
            Payload::Failed {
                job,
                error: RepoError::Other("failed to load jj commit gone".to_owned()),
            },
        ));
        let (_, _, base) = sent_diff(&effects).expect("the plain diff reloads");
        assert_eq!(base, DiffBase::Parents);
        assert_eq!(session.diff_base, DiffBase::Parents);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Activity(Activity::Toast { title, .. }) if title == "Comparison ended"
        )));
    }

    // ── typed revsets ───────────────────────────────────────────────────

    fn range_session() -> Session {
        let mut session = compare_session();
        session.capabilities.range_diff = true;
        session
    }

    fn test_repo() -> RepoId {
        crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/range".into(),
            scope: Default::default(),
        })
    }

    fn land_range(session: &mut Session, job: JobId, includes_working_copy: bool) {
        session.apply(Event::new(
            test_repo(),
            Payload::DiffLoaded {
                job,
                revision: commit("bbb"),
                document: DiffDocument {
                    range: Some(Box::new(crate::model::DiffRange {
                        revset: "aaa|bbb".to_owned(),
                        includes_working_copy,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
                details: None,
            },
        ));
    }

    /// Run a watcher snapshot that moved the op and return its effects.
    fn watcher_snapshot(session: &mut Session) -> Vec<Effect> {
        session.repository_snapshot = Some(RepositorySnapshot {
            fingerprint: "op1".to_owned(),
            ..Default::default()
        });
        let job = match session.snapshot(RefreshOrigin::Watcher).as_slice() {
            [Effect::Send(Command::Snapshot { job, .. })] => *job,
            other => panic!("expected a snapshot, got {other:?}"),
        };
        session.apply(Event::new(
            test_repo(),
            Payload::SnapshotDone {
                job,
                origin: RefreshOrigin::Watcher,
                snapshot: RepositorySnapshot {
                    fingerprint: "op2".to_owned(),
                    parent_fingerprint: Some("op1".to_owned()),
                    ..Default::default()
                },
                warnings: Vec::new(),
            },
        ))
    }

    /// A range is a sticky base: it survives selecting another revision,
    /// can't be swapped or turned into an interdiff, and exits like a
    /// comparison. Partial picks read it as one (`is_compare`).
    #[test]
    fn a_range_sticks_until_exit() {
        let mut session = range_session();
        session.selected_revision = commit("bbb");
        let effects = session.diff_range("aaa|bbb");
        let (job, revision, base) = sent_diff(&effects).expect("a range load");
        assert_eq!(revision, commit("bbb"));
        assert_eq!(base, DiffBase::Range("aaa|bbb".to_owned()));
        assert!(session.diff_base.is_compare());
        land_range(&mut session, job, false);
        assert_eq!(session.document_base, DiffBase::Range("aaa|bbb".to_owned()));
        assert_eq!(session.document.range.as_ref().unwrap().revset, "aaa|bbb");

        assert!(session.swap_compare().is_empty());
        assert!(session.set_interdiff(true).is_empty());
        // Another row moves the cursor; the diff stays the range.
        let (job, revision, base) = sent_diff(&session.load_diff(commit("ccc"))).expect("a reload");
        assert_eq!(base, DiffBase::Range("aaa|bbb".to_owned()));
        land(&mut session, job, revision);
        assert_eq!(session.selected_revision, commit("ccc"));

        let (_, revision, base) = sent_diff(&session.exit_compare()).expect("the plain diff");
        assert_eq!((revision, base), (commit("ccc"), DiffBase::Parents));
    }

    #[test]
    fn ranges_are_gated_on_the_capability() {
        let mut session = compare_session();
        assert!(session.diff_range("aaa|bbb").is_empty());
        assert!(
            session
                .diff_revsets(DiffRequest::Revisions(vec!["x".to_owned()]))
                .is_empty()
        );
        assert_eq!(session.diff_base, DiffBase::Parents);
    }

    /// A range ending at `@` reloads on a working-tree edit; one that
    /// doesn't, doesn't.
    #[test]
    fn a_range_reloads_on_a_watcher_refresh_only_when_it_holds_the_working_copy() {
        let mut session = range_session();
        session.selected_revision = RevisionSelection::WorkingCopy;
        let (job, ..) = sent_diff(&session.diff_range("aaa|bbb")).expect("a range load");
        land_range(&mut session, job, false);
        assert!(
            sent_diff(&watcher_snapshot(&mut session)).is_none(),
            "the range doesn't hold @, even though @ is selected"
        );

        let (job, ..) = sent_diff(&session.diff_range("aaa::")).expect("a range load");
        land_range(&mut session, job, true);
        let (_, _, base) = sent_diff(&watcher_snapshot(&mut session)).expect("the range reloads");
        assert_eq!(base, DiffBase::Range("aaa::".to_owned()));
    }

    #[test]
    fn a_failed_range_falls_back_to_the_plain_diff() {
        let mut session = range_session();
        let (job, ..) = sent_diff(&session.diff_range("aaa|ccc")).expect("a range load");
        let effects = session.apply(Event::new(
            test_repo(),
            Payload::Failed {
                job,
                error: RepoError::Revset("Cannot diff revsets with gaps in.".to_owned()),
            },
        ));
        let (_, _, base) = sent_diff(&effects).expect("the plain diff reloads");
        assert_eq!(base, DiffBase::Parents);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Activity(Activity::Toast { title, body, .. })
                if title == "Range diff ended" && body.starts_with("Cannot diff")
        )));
    }

    /// `jj diff` arguments resolve on the actor first; only the newest
    /// request's answer loads anything.
    #[test]
    fn typed_diff_arguments_resolve_then_load() {
        let mut session = range_session();
        let resolve_job = |effects: &[Effect]| {
            effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::Send(Command::ResolveDiff { job, .. }) => Some(*job),
                    _ => None,
                })
                .expect("a resolve")
        };
        let first =
            resolve_job(&session.diff_revsets(DiffRequest::Revisions(vec!["aaa".to_owned()])));
        assert!(session.owns_job(first));
        let effects = session.diff_revsets(DiffRequest::Revisions(vec!["aaa|bbb".to_owned()]));
        assert!(
            effects.iter().any(
                |effect| matches!(effect, Effect::Send(Command::Cancel { job }) if *job == first)
            ),
            "the superseded resolve is cancelled"
        );
        let second = resolve_job(&effects);

        let stale = session.apply(Event::new(
            test_repo(),
            Payload::DiffResolved {
                job: first,
                revision: commit("aaa"),
                base: DiffBase::Parents,
            },
        ));
        assert!(stale.is_empty());

        let effects = session.apply(Event::new(
            test_repo(),
            Payload::DiffResolved {
                job: second,
                revision: commit("bbb"),
                base: DiffBase::Range("aaa|bbb".to_owned()),
            },
        ));
        let (_, revision, base) = sent_diff(&effects).expect("the range loads");
        assert_eq!(revision, commit("bbb"));
        assert_eq!(base, DiffBase::Range("aaa|bbb".to_owned()));
        assert!(!session.owns_job(second));

        // A refusal is the user's to read.
        let job = resolve_job(&session.diff_revsets(DiffRequest::FromTo {
            from: Some("all()".to_owned()),
            to: None,
            interdiff: false,
        }));
        let effects = session.apply(Event::new(
            test_repo(),
            Payload::Failed {
                job,
                error: RepoError::Revset(
                    "Revset `all()` resolved to more than one revision".to_owned(),
                ),
            },
        ));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Activity(Activity::Toast { title, .. }) if title == "Can't diff that"
        )));
        assert!(sent_diff(&effects).is_none(), "the pane stays as it was");
    }

    #[test]
    fn the_completer_reads_the_last_graph_load() {
        let mut session = range_session();
        session
            .revset_symbols
            .aliases
            .push(crate::revset::RevsetAlias {
                name: "wip".to_owned(),
                params: None,
                definition: "description(wip)".to_owned(),
            });
        let history = vec!["wip & mine()".to_owned()];
        let found = session.revset_completer(&history).complete_revset("wi", 2);
        let inserts: Vec<&str> = found.iter().map(|c| c.insert.as_str()).collect();
        assert_eq!(inserts, ["wip & mine()", "wip"]);
    }

    // ── size caps ───────────────────────────────────────────────────────

    fn collapsed_file(path: &str) -> crate::model::DiffFile {
        crate::model::DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: crate::model::DiffFileStatus::Added,
            hunks: vec![crate::model::DiffHunkView {
                header: "Large diff".to_owned(),
                lines: Vec::new(),
            }],
            additions: 5_000,
            deletions: 0,
            collapsed: Some(crate::model::DiffCollapse::LargeFile),
            blob: None,
        }
    }

    /// Expanding swaps the placeholder for the full file in place, and the
    /// path rides along on later reloads of the same diff — but not onto
    /// another revision's.
    #[test]
    fn an_expanded_file_replaces_its_placeholder_and_survives_reloads() {
        let mut session = compare_session();
        let effects = session.load_diff(commit("aaa"));
        let (job, ..) = sent_diff(&effects).expect("a diff load");
        let repo = crate::repo::repo_id(&crate::repo::OpenSpec::Local {
            root: "/tmp/caps".into(),
            scope: Default::default(),
        });
        session.apply(Event::new(
            repo.clone(),
            Payload::DiffLoaded {
                job,
                revision: commit("aaa"),
                document: DiffDocument {
                    files: vec![collapsed_file("huge.txt")],
                    total_additions: 5_000,
                    total_deletions: 0,
                    range: None,
                },
                details: None,
            },
        ));

        let effects = session.expand_file("huge.txt");
        let job = match effects.as_slice() {
            [Effect::Send(Command::ExpandFile { job, path, .. })] if path == "huge.txt" => *job,
            other => panic!("expected an ExpandFile, got {other:?}"),
        };
        assert!(
            session.expand_file("huge.txt").is_empty(),
            "already in flight"
        );

        let mut full = collapsed_file("huge.txt");
        full.collapsed = None;
        full.hunks[0].header = "@@ -0,0 +1,5000 @@".to_owned();
        let effects = session.apply(Event::new(
            repo,
            Payload::FileExpanded {
                job,
                path: "huge.txt".to_owned(),
                file: Some(full),
            },
        ));
        assert!(matches!(effects.as_slice(), [Effect::RepaintDocument]));
        assert_eq!(session.document.files[0].collapsed, None);

        let effects = session.load_diff(commit("aaa"));
        let expand = effects.iter().find_map(|effect| match effect {
            Effect::Send(Command::LoadDiff { expand, .. }) => Some(expand.clone()),
            _ => None,
        });
        assert_eq!(expand, Some(vec!["huge.txt".to_owned()]));

        let effects = session.load_diff(commit("bbb"));
        let expand = effects.iter().find_map(|effect| match effect {
            Effect::Send(Command::LoadDiff { expand, .. }) => Some(expand.clone()),
            _ => None,
        });
        assert_eq!(
            expand,
            Some(Vec::new()),
            "another revision starts collapsed"
        );
    }

    #[test]
    fn collapsed_and_oversized_files_are_not_highlighted() {
        let mut session = Session::unloaded(String::new());
        let row = crate::model::DiffLine {
            kind: crate::model::DiffLineKind::Addition,
            old_line: None,
            new_line: Some(1),
            content: "x".to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        };
        let mut small = collapsed_file("small.rs");
        small.collapsed = None;
        small.hunks[0].lines = vec![row.clone()];
        let mut big = collapsed_file("big.rs");
        big.collapsed = None;
        big.hunks[0].lines = vec![row; HIGHLIGHT_MAX_ROWS + 1];
        session.document.files = vec![collapsed_file("huge.rs"), small, big];
        session.reset_highlights(1);
        assert_eq!(Vec::from(session.highlight_pending.clone()), vec![1]);
    }

    // ── ranges that follow the graph ────────────────────────────────────

    fn sent_resolve(effects: &[Effect]) -> Option<(JobId, RevsetQuery)> {
        effects.iter().find_map(|effect| match effect {
            Effect::Send(Command::ResolveRevset { job, query }) => Some((*job, query.clone())),
            _ => None,
        })
    }

    /// "Diff since trunk" shows `trunk()..X` and re-targets as the cursor
    /// moves, like a comparison's sticky base; exit ends it.
    #[test]
    fn diff_since_follows_the_browsed_revision() {
        let mut session = range_session();
        let effects = session.diff_since("trunk()", commit("aaa"), Some("alpha".to_owned()));
        let (_, revision, base) = sent_diff(&effects).expect("a range load");
        assert_eq!(revision, commit("aaa"));
        assert_eq!(base, DiffBase::Range("trunk()..alpha".to_owned()));

        let effects = session.browse(RevisionSelection::WorkingCopy);
        let (_, revision, base) = sent_diff(&effects).expect("re-targeted");
        assert_eq!(revision, RevisionSelection::WorkingCopy);
        assert_eq!(base, DiffBase::Range("trunk()..@".to_owned()));

        // A row the graph doesn't hold is named by commit id.
        let effects = session.browse(commit("0123456789abcdef"));
        let (.., base) = sent_diff(&effects).expect("re-targeted");
        assert_eq!(base, DiffBase::Range("trunk()..0123456789ab".to_owned()));

        // Picking a comparison leaves the mode.
        session.compare(DiffBase::Revision(commit("aaa")), commit("bbb"));
        assert_eq!(session.diff_since, None);

        session.diff_since("trunk()", commit("aaa"), None);
        let effects = session.exit_compare();
        let (.., base) = sent_diff(&effects).expect("the plain diff");
        assert_eq!(base, DiffBase::Parents);
        assert_eq!(session.diff_since, None);
    }

    /// Browsing away from a fixed range shows the browsed revision itself;
    /// a comparison keeps its base.
    #[test]
    fn browsing_leaves_a_fixed_range_but_not_a_comparison() {
        let mut session = range_session();
        session.diff_range("aaa|bbb");
        let effects = session.browse(commit("ccc"));
        let (.., base) = sent_diff(&effects).expect("a load");
        assert_eq!(base, DiffBase::Parents);

        session.compare(DiffBase::Revision(commit("aaa")), commit("bbb"));
        let effects = session.browse(commit("ccc"));
        let (.., base) = sent_diff(&effects).expect("a load");
        assert_eq!(base, DiffBase::Revision(commit("aaa")));
    }

    /// A span is checked for gaps first: a connected one becomes a range
    /// diff, one with gaps is refused in jj's words and the pane stays put.
    #[test]
    fn a_span_shows_as_one_diff_only_without_gaps() {
        let mut session = range_session();
        let effects = session.diff_span(&[commit("aaa"), commit("bbb")]);
        assert!(
            sent_diff(&effects).is_none(),
            "nothing loads before the check"
        );
        let (job, query) = sent_resolve(&effects).expect("a gap check");
        assert_eq!(query.expect, RevsetExpect::Contiguous);
        assert_eq!(query.revset, "aaa | bbb");
        assert!(session.owns_job(job));

        let effects = session.apply(Event::new(
            test_repo(),
            Payload::RevsetResolved {
                job,
                revset: query.revset.clone(),
                result: Ok(RevsetMatch::default()),
            },
        ));
        let (.., base) = sent_diff(&effects).expect("the span loads");
        assert_eq!(base, DiffBase::Range("aaa | bbb".to_owned()));
        assert!(session.span.is_some(), "the span stays while it is shown");

        let effects = session.diff_span(&[commit("aaa"), commit("ccc")]);
        let (job, query) = sent_resolve(&effects).expect("a gap check");
        let effects = session.apply(Event::new(
            test_repo(),
            Payload::RevsetResolved {
                job,
                revset: query.revset,
                result: Err(RevsetError::new("Cannot diff revsets with gaps in.")),
            },
        ));
        assert!(sent_diff(&effects).is_none(), "the pane stays as it was");
        let refused = session.span.as_ref().and_then(|span| span.refused.as_ref());
        assert_eq!(
            refused.map(|error| error.message.as_str()),
            Some("Cannot diff revsets with gaps in.")
        );

        // A plain click leaves the span for the clicked revision.
        let effects = session.browse(commit("ccc"));
        let (.., base) = sent_diff(&effects).expect("a load");
        assert_eq!(base, DiffBase::Parents);
        assert_eq!(session.span, None);
    }

    /// A newer span supersedes the check of an older one, whose answer is
    /// then ignored.
    #[test]
    fn a_stale_span_check_is_ignored() {
        let mut session = range_session();
        let first = sent_resolve(&session.diff_span(&[commit("aaa"), commit("bbb")]))
            .expect("a check")
            .0;
        let effects = session.diff_span(&[commit("aaa"), commit("ccc")]);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Send(Command::Cancel { job }) if *job == first
        )));
        let effects = session.apply(Event::new(
            test_repo(),
            Payload::RevsetResolved {
                job: first,
                revset: "aaa | bbb".to_owned(),
                result: Ok(RevsetMatch::default()),
            },
        ));
        assert!(effects.is_empty());
    }
}
