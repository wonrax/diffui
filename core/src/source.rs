//! VCS-dispatch loaders: snapshot + graph + diff for jj/git, plus fetch/undo
//! and the off-load-path empty-status resolver. jj paths run on a blocking
//! thread (jj-lib is `!Send`); git shells out.

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::graph_layout::GraphLayout;
use crate::model::{
    BackendOutput, BookmarksInfo, BranchStatus, CommitStore, CommitStoreBuilder, DiffDocument,
    DiffFile, LoadProgress, RevisionDetails, RevisionSelection,
};
use crate::mutations::{MutationOp, MutationOutcome};
use crate::repository::{FetchTarget, Repository, RepositorySnapshot, Vcs};

pub async fn load_backend(
    repository: Repository,
    revision: RevisionSelection,
    revset: String,
    progress: LoadProgress,
) -> Result<BackendOutput, String> {
    run_backend(repository, revision, revset, progress)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_backend(
    repository: Repository,
    revision: RevisionSelection,
    revset: String,
    progress: LoadProgress,
) -> Result<BackendOutput> {
    // Snapshot the working copy *before* reading the graph and diff, so both
    // reflect the on-disk state. Loading first showed a stale, pre-snapshot
    // working-copy commit — e.g. `@` flagged "empty" while its diff actually
    // had changes — until the next refresh re-read it.
    let snapshot = run_repository_snapshot(repository.clone()).await?;
    let (commits, graph, branch_status, bookmarks) =
        load_commits(&repository, &revset, &progress).await?;
    let (document, details) = run_diff(&repository, &revision).await?;

    Ok(BackendOutput {
        document,
        commits,
        graph,
        snapshot,
        details,
        branch_status,
        bookmarks,
    })
}

/// Load just the diff (and revision-header details) for `revision`, without
/// re-walking the commit graph or snapshotting the working copy.
///
/// Switching which revision's diff is shown leaves the graph and repo state
/// untouched, so the full `load_backend` would waste ~all of its time
/// re-running `load_commits` (whose per-commit `is_empty` check dominates
/// load time on large repos — tens of seconds on a 40k-commit repo).
pub async fn load_diff(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>), String> {
    run_diff(&repository, &revision)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_diff(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    match repository.vcs {
        Vcs::Jj => {
            let repository = repository.clone();
            let revision = revision.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_diff(repository, revision))
            })
            .await
            .context("jj diff loader task failed")?
        }
        Vcs::Git => crate::git::load_git_diff(repository, revision).await,
    }
}

pub async fn load_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    run_repository_snapshot(repository).await
}

/// Read the jj op-log head fingerprint (cheap, lock-free) for the fs-watcher's
/// op-id dedup. Git has no operation log, so it yields `Ok(None)` and the caller
/// treats the op-log signal as a no-op. Mirrors the off-thread `block_on` idiom
/// the other jj backend calls use.
pub async fn read_op_head(repository: Repository) -> Result<Option<String>, String> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            let joined = tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::read_jj_op_head(repository))
            })
            .await;
            match joined {
                Ok(result) => result.map(Some).map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("jj op-head read task failed: {error}")),
            }
        }
        Vcs::Git => Ok(None),
    }
}

/// Load just the `jj show`-style header (ids, bookmarks, author/committer
/// signatures, description) for a single revision — no diff. Used by the
/// revision context menu's "Copy → Author / Committer", which needs the dates
/// the in-memory graph doesn't keep. jj-only (the context menu itself is
/// jj-only); the off-thread `block_on` mirrors [`run_diff`]'s jj path.
pub async fn load_revision_details(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<RevisionDetails, String> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            let joined = tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_revision_details(repository, revision))
            })
            .await;
            match joined {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("jj details loader task failed: {error}")),
            }
        }
        Vcs::Git => Err("revision details are jj-only".to_owned()),
    }
}

/// Resolve the empty status of `targets` (row index + hex commit-id) off the
/// load path. Best-effort: any failure yields no update for that commit, since
/// the "empty" marker is purely cosmetic. Git has no equivalent here (its
/// loader doesn't populate empty status), so it returns nothing.
pub async fn compute_empty_status(
    repository: Repository,
    targets: Vec<(usize, String)>,
) -> Vec<(usize, bool)> {
    if targets.is_empty() {
        return Vec::new();
    }
    match repository.vcs {
        Vcs::Jj => {
            let root = repository.root.clone();
            let handle = tokio::runtime::Handle::current();
            let joined = tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::compute_jj_empty_status(root, targets))
            })
            .await;
            // Best-effort: the empty marker is cosmetic, so a failure yields no
            // update — but log it rather than swallow it silently, so a jj crash
            // or panicking task is visible during debugging.
            match joined {
                Ok(Ok(updates)) => updates,
                Ok(Err(error)) => {
                    eprintln!("diffui: failed to compute empty status: {error:#}");
                    Vec::new()
                }
                Err(error) => {
                    eprintln!("diffui: empty-status task failed: {error}");
                    Vec::new()
                }
            }
        }
        Vcs::Git => Vec::new(),
    }
}

/// Background syntax highlighting for one file of a displayed document.
///
/// When `repository` is given, both sides' **full contents** are read first
/// (jj: materialized from the trees; git: `git show` / the working tree) so
/// tree-sitter parses the real documents — constructs that span hunk
/// boundaries (multi-line strings, block comments, the enclosing class)
/// highlight correctly, which the diff-only reconstruction gets wrong.
/// Sourceless inputs (PR tabs, unreadable sides) fall back to that
/// reconstruction, so the result is never worse than the old inline pass.
///
/// Returns the per-line spans sparsely as `(hunk, line, spans)`; empty when
/// the language is unknown. The parse runs on a blocking thread — this is
/// seconds of CPU for huge files, which is exactly why it left the load path.
pub async fn highlight_file(
    repository: Option<Repository>,
    revision: RevisionSelection,
    file: DiffFile,
) -> Vec<(usize, usize, Vec<crate::SyntaxSpan>)> {
    let sources = match &repository {
        Some(repository) => match repository.vcs {
            Vcs::Jj => crate::jj::read_jj_file_pair(
                repository.clone(),
                revision,
                file.path.clone(),
                file.old_path.clone(),
            )
            .await
            .unwrap_or((None, None)),
            Vcs::Git => {
                crate::git::read_git_file_pair(
                    repository,
                    &revision,
                    &file.path,
                    file.old_path.as_deref(),
                )
                .await
            }
        },
        None => (None, None),
    };

    let joined = tokio::task::spawn_blocking(move || {
        let (old_source, new_source) = sources;
        let mut file = file;
        crate::syntax::apply_syntax_highlighting_with_sources(
            &mut file,
            old_source.as_deref(),
            new_source.as_deref(),
        );
        file.hunks
            .into_iter()
            .enumerate()
            .flat_map(|(hunk_index, hunk)| {
                hunk.lines
                    .into_iter()
                    .enumerate()
                    .filter(|(_, line)| !line.syntax.is_empty())
                    .map(move |(line_index, line)| (hunk_index, line_index, line.syntax))
            })
            .collect()
    })
    .await;
    joined.unwrap_or_default()
}

/// Fetch from the remote(s) and return the captured remote/sideband output for
/// the activity log. jj runs in-process (jj-lib spawns git internally) on a
/// blocking thread since jj-lib state is `!Send`; git shells out.
pub async fn fetch(
    repository: Repository,
    target: crate::FetchTarget,
    progress: LoadProgress,
) -> Result<Vec<String>, String> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::fetch_jj(repository, target, progress))
            })
            .await
            .map_err(|error| format!("fetch task panicked: {error}"))?
            .map_err(|error| format!("{error:#}"))
        }
        Vcs::Git => crate::git::fetch_git(&repository, &target)
            .await
            .map_err(|error| format!("{error:#}")),
    }
}

/// Undo the latest jj operation, returning a one-line summary for the activity
/// log. jj-only — git has no operation log.
pub async fn undo(repository: Repository) -> Result<Vec<String>, String> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(crate::jj::undo_jj(repository)))
                .await
                .map_err(|error| format!("undo task panicked: {error}"))?
                .map_err(|error| format!("{error:#}"))
        }
        Vcs::Git => Err("Undo is only available for jj repositories".to_owned()),
    }
}

async fn run_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                // Drop the returned repo + wc id; the refresh path only needs
                // the fingerprint (the cold path keeps the repo for reuse).
                handle
                    .block_on(crate::jj::load_jj_repository_snapshot(repository))
                    .map(|(snapshot, _repo, _wc_commit_id)| snapshot)
            })
            .await
            .context("jj repository snapshot task failed")?
        }
        Vcs::Git => crate::git::load_git_repository_snapshot(&repository.root).await,
    }
}

async fn load_commits(
    repository: &Repository,
    revset: &str,
    progress: &LoadProgress,
) -> Result<(
    CommitStore,
    GraphLayout,
    Option<BranchStatus>,
    BookmarksInfo,
)> {
    match repository.vcs {
        Vcs::Jj => {
            let root = repository.root.clone();
            let revset = revset.to_owned();
            let progress = progress.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_commits(root, revset, progress))
            })
            .await
            .context("jj commit loader task failed")?
        }
        Vcs::Git => {
            // The git loader parses `git log` in one shot rather than
            // per-commit, so surface the count once it's known, then fold the
            // summaries into the compact store.
            let (commits, graph) = crate::git::load_git_commits(repository, revset).await?;
            progress.set_total(commits.len());
            let mut builder = CommitStoreBuilder::with_capacity(commits.len());
            for commit in commits {
                builder.push(commit);
            }
            // Git ahead/behind isn't wired yet — the footer falls back to the
            // change count for git repos, and the context menu's bookmark
            // actions are jj-only.
            Ok((builder.finish(), graph, None, BookmarksInfo::default()))
        }
    }
}

// ---- The `DiffSource` capability abstraction ----
//
// `DiffSource` is the one capability every source has: produce a diff for a
// target. Repo-backed sources additionally expose `RevisionGraph` (commit
// graph, bookmarks, branch status, fetch) and — for jj — `Mutable` (history
// edits). A future GitHub-PR or two-local-files source implements only
// `DiffSource`. The traits are object-safe (a frontend can hold an
// `Arc<dyn DiffSource>` chosen at runtime); `async-trait` boxes the futures,
// and each impl offloads the `!Send` jj-lib work onto `spawn_blocking`
// internally so the futures stay `Send`.

/// What a [`DiffSource`] should diff. Repo sources interpret `Revision`;
/// non-repo sources (a PR, two files) add their own variants later. One arm
/// today keeps the seam without speculating a shape we can't yet validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    Revision(RevisionSelection),
}

impl From<RevisionSelection> for DiffTarget {
    fn from(revision: RevisionSelection) -> Self {
        Self::Revision(revision)
    }
}

/// The universal capability: produce a diff document for a target.
#[async_trait]
pub trait DiffSource: Send + Sync {
    /// Human-facing label for the *source* (a repo path, "PR #123", …), or
    /// `None` when there's no canonical label and the frontend should name it.
    /// (The diffed revision's own message rides in `RevisionDetails`.)
    fn describe(&self) -> Option<String>;

    /// Produce the diff (and optional revision-header details) for `target`.
    async fn load_diff(
        &self,
        target: &DiffTarget,
    ) -> Result<(DiffDocument, Option<RevisionDetails>), String>;

    /// Downcast to the revision-graph capability, or `None` if unsupported.
    fn as_revision_graph(&self) -> Option<&dyn RevisionGraph> {
        None
    }

    /// Downcast to the mutation capability, or `None` if unsupported.
    fn as_mutable(&self) -> Option<&dyn Mutable> {
        None
    }
}

/// A source backed by a commit graph: it can load the whole graph + working
/// copy, snapshot it, and fetch. jj and git repos implement this; a PR or
/// file-pair source does not.
#[async_trait]
pub trait RevisionGraph: Send + Sync {
    /// Atomic full load — snapshot + commit store + graph + initial diff +
    /// details + branch/bookmark state. (The jj streaming cold-load is a
    /// separate fast-path; see [`RevisionGraph::supports_streaming_cold_load`].)
    async fn load(
        &self,
        revision: &RevisionSelection,
        revset: &str,
        progress: LoadProgress,
    ) -> Result<BackendOutput, String>;

    /// Re-read only the working-copy fingerprint (fs-watcher dedup).
    async fn snapshot(&self) -> Result<RepositorySnapshot, String>;

    /// Op-log head fingerprint, or `None` for sources without one (git).
    async fn op_head(&self) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// `jj show`-style header for one revision. Default: unsupported.
    async fn revision_details(
        &self,
        _revision: &RevisionSelection,
    ) -> Result<RevisionDetails, String> {
        Err("revision details are not supported by this source".to_owned())
    }

    /// Resolve deferred empty-status for merges/roots. Default: nothing.
    async fn compute_empty_status(&self, _targets: Vec<(usize, String)>) -> Vec<(usize, bool)> {
        Vec::new()
    }

    /// Fetch from remote(s); returns captured output lines for an activity log.
    async fn fetch(
        &self,
        target: FetchTarget,
        progress: LoadProgress,
    ) -> Result<Vec<String>, String>;

    /// Whether this source supports the jj streaming cold-load fast-path
    /// ([`crate::jj::load_jj_cold`]). Lets a frontend pick streaming vs the
    /// atomic [`RevisionGraph::load`] without matching on the VCS.
    fn supports_streaming_cold_load(&self) -> bool {
        false
    }
}

/// Sources that support history mutation (jj only today). git/PR/file sources
/// don't implement it, so a frontend capability-gates mutating actions via
/// [`DiffSource::as_mutable`] instead of catching "unsupported" errors.
#[async_trait]
pub trait Mutable: Send + Sync {
    async fn mutate(
        &self,
        op: MutationOp,
        progress: LoadProgress,
    ) -> Result<MutationOutcome, String>;

    /// Undo the latest operation (jj op-log); returns an activity-log summary.
    async fn undo(&self) -> Result<Vec<String>, String>;
}

/// Cloneable, frontend-facing handle to a [`DiffSource`]. A newtype so state
/// structs (e.g. [`crate::session::Session`]) keep `#[derive(Debug)]` — trait
/// objects aren't `Debug`, so this prints the source's label instead.
///
/// The async helpers take `self` by value (a cheap `Arc` clone) so the
/// returned futures are `'static` and hand straight to a runtime
/// (`Task::perform`, `tokio::spawn`) without borrowing the app. Capability
/// helpers (`load`, `fetch`, `undo`, …) resolve the downcast internally and
/// surface "unsupported" as a plain `Err`, so call sites stay one-liners.
#[derive(Clone)]
pub struct SourceHandle(pub std::sync::Arc<dyn DiffSource>);

impl SourceHandle {
    pub fn new(source: impl DiffSource + 'static) -> Self {
        Self(std::sync::Arc::new(source))
    }

    /// The diff for `revision`, via the universal capability.
    pub async fn diff(
        self,
        revision: RevisionSelection,
    ) -> Result<(DiffDocument, Option<RevisionDetails>), String> {
        self.0.load_diff(&DiffTarget::Revision(revision)).await
    }

    /// Atomic full load via the revision-graph capability.
    pub async fn load(
        self,
        revision: RevisionSelection,
        revset: String,
        progress: LoadProgress,
    ) -> Result<BackendOutput, String> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.load(&revision, &revset, progress).await,
            None => Err("this source has no revision graph to load".to_owned()),
        }
    }

    pub async fn snapshot(self) -> Result<RepositorySnapshot, String> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.snapshot().await,
            None => Err("this source has no working copy to snapshot".to_owned()),
        }
    }

    /// Op-log head fingerprint; `Ok(None)` for sources without one.
    pub async fn op_head(self) -> Result<Option<String>, String> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.op_head().await,
            None => Ok(None),
        }
    }

    pub async fn details(self, revision: RevisionSelection) -> Result<RevisionDetails, String> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.revision_details(&revision).await,
            None => Err("revision details are not supported by this source".to_owned()),
        }
    }

    /// Deferred empty-status resolution; nothing for graph-less sources
    /// (the marker is cosmetic, so "unsupported" is just an empty result).
    pub async fn empty_status(self, targets: Vec<(usize, String)>) -> Vec<(usize, bool)> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.compute_empty_status(targets).await,
            None => Vec::new(),
        }
    }

    pub async fn fetch(
        self,
        target: FetchTarget,
        progress: LoadProgress,
    ) -> Result<Vec<String>, String> {
        match self.0.as_revision_graph() {
            Some(graph) => graph.fetch(target, progress).await,
            None => Err("this source cannot fetch".to_owned()),
        }
    }

    pub async fn undo(self) -> Result<Vec<String>, String> {
        match self.0.as_mutable() {
            Some(mutable) => mutable.undo().await,
            None => Err("this source does not support undo".to_owned()),
        }
    }
}

impl std::fmt::Debug for SourceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SourceHandle")
            .field(&self.0.describe())
            .finish()
    }
}

impl std::ops::Deref for SourceHandle {
    type Target = dyn DiffSource;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// A [`DiffSource`] backed by a local jj/git [`Repository`] — the first concrete
/// source. The internal `match vcs` keeps today's dispatch; splitting into
/// `JjSource`/`GitSource` later is invisible to callers behind the traits.
pub struct RepoSource {
    repo: Repository,
}

impl RepoSource {
    pub fn new(repo: Repository) -> Self {
        Self { repo }
    }

    pub fn repository(&self) -> &Repository {
        &self.repo
    }
}

#[async_trait]
impl DiffSource for RepoSource {
    fn describe(&self) -> Option<String> {
        Some(self.repo.root.display().to_string())
    }

    async fn load_diff(
        &self,
        target: &DiffTarget,
    ) -> Result<(DiffDocument, Option<RevisionDetails>), String> {
        match target {
            DiffTarget::Revision(revision) => load_diff(self.repo.clone(), revision.clone()).await,
        }
    }

    fn as_revision_graph(&self) -> Option<&dyn RevisionGraph> {
        Some(self)
    }

    fn as_mutable(&self) -> Option<&dyn Mutable> {
        matches!(self.repo.vcs, Vcs::Jj).then_some(self as &dyn Mutable)
    }
}

#[async_trait]
impl RevisionGraph for RepoSource {
    async fn load(
        &self,
        revision: &RevisionSelection,
        revset: &str,
        progress: LoadProgress,
    ) -> Result<BackendOutput, String> {
        load_backend(
            self.repo.clone(),
            revision.clone(),
            revset.to_owned(),
            progress,
        )
        .await
    }

    async fn snapshot(&self) -> Result<RepositorySnapshot, String> {
        load_repository_snapshot(self.repo.clone())
            .await
            .map_err(|error| format!("{error:#}"))
    }

    async fn op_head(&self) -> Result<Option<String>, String> {
        read_op_head(self.repo.clone()).await
    }

    async fn revision_details(
        &self,
        revision: &RevisionSelection,
    ) -> Result<RevisionDetails, String> {
        load_revision_details(self.repo.clone(), revision.clone()).await
    }

    async fn compute_empty_status(&self, targets: Vec<(usize, String)>) -> Vec<(usize, bool)> {
        compute_empty_status(self.repo.clone(), targets).await
    }

    async fn fetch(
        &self,
        target: FetchTarget,
        progress: LoadProgress,
    ) -> Result<Vec<String>, String> {
        fetch(self.repo.clone(), target, progress).await
    }

    fn supports_streaming_cold_load(&self) -> bool {
        matches!(self.repo.vcs, Vcs::Jj)
    }
}

#[async_trait]
impl Mutable for RepoSource {
    async fn mutate(
        &self,
        op: MutationOp,
        progress: LoadProgress,
    ) -> Result<MutationOutcome, String> {
        crate::mutations::run_mutation(self.repo.clone(), op, progress)
            .await
            .map_err(|error| format!("{error:#}"))
    }

    async fn undo(&self) -> Result<Vec<String>, String> {
        undo(self.repo.clone()).await
    }
}
