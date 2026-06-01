use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result};
use arborium::{
    GrammarStore,
    advanced::{CompiledGrammar, ParseContext},
};

pub use crate::diff_view::{DiffHunkView, DiffLine, DiffLineKind, SyntaxKind, SyntaxSpan};
use crate::graph_layout::GraphLayout;
use crate::repository::{Repository, RepositorySnapshot, Vcs};

/// Shared, lock-free progress for a commit-graph load. The loader bumps these
/// from its worker thread; the UI reads them each frame to render a progress
/// indicator. `total` is 0 until the revset has been walked.
#[derive(Debug, Clone, Default)]
pub struct LoadProgress {
    loaded: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}

impl LoadProgress {
    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn increment(&self) {
        self.loaded.fetch_add(1, Ordering::Relaxed);
    }

    /// `(loaded, total)` so far. `total == 0` means the count isn't known yet.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.loaded.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionSelection {
    WorkingCopy,
    Commit(String),
}

impl RevisionSelection {
    pub fn view_key(&self) -> String {
        match self {
            Self::WorkingCopy => "working-copy".to_owned(),
            Self::Commit(change_id) => format!("commit:{change_id}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub total_additions: usize,
    pub total_deletions: usize,
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: DiffFileStatus,
    pub hunks: Vec<DiffHunkView>,
    pub additions: usize,
    pub deletions: usize,
}

/// One commit's data as produced by a backend loader. This is a transient
/// builder input — it's pushed into a [`CommitStore`], which keeps the data
/// compactly (interned authors, an arena for ids/descriptions, packed flags)
/// so a million-commit history fits in memory. Read it back via [`RowView`].
#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub change_id: String,
    pub commit_id: String,
    pub shortest_change_id_len: Option<usize>,
    pub description: String,
    pub author: String,
    pub has_description: bool,
    pub is_empty: Option<bool>,
    /// Whether the commit's tree is in a conflicted state (jj only — git
    /// commits can't carry an unresolved conflict, so this stays `false`
    /// for the git backend).
    pub has_conflict: bool,
    pub is_working_copy: bool,
    /// Bookmarks pointing at this commit. Local bookmarks are bare
    /// names; remote-tracking ones are `name@remote`. Order matches
    /// `jj show`'s "Bookmarks:" line — local first, then remotes.
    pub bookmarks: Vec<String>,
}

mod commit_flags {
    pub const HAS_DESCRIPTION: u8 = 1;
    pub const IS_EMPTY_KNOWN: u8 = 1 << 1;
    pub const IS_EMPTY: u8 = 1 << 2;
    pub const HAS_CONFLICT: u8 = 1 << 3;
    pub const IS_WORKING_COPY: u8 = 1 << 4;
}

/// Byte range `[start, start+len)` into a [`CommitStore`]'s text arena.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: u32,
    len: u32,
}

#[derive(Debug, Clone, Copy)]
struct CommitSpans {
    change_id: Span,
    commit_id: Span,
    description: Span,
}

/// Compact, indexable storage for a commit graph. Strings live in one shared
/// arena (no per-commit `String` headers/allocations); authors are interned
/// (a repo has far fewer authors than commits); flags are packed into a byte;
/// bookmarks are stored sparsely since most commits have none. Read rows with
/// [`CommitStore::row`] / [`CommitStore::iter`].
#[derive(Debug, Clone, Default)]
pub struct CommitStore {
    text: String,
    spans: Vec<CommitSpans>,
    authors: Vec<Arc<str>>,
    author_idx: Vec<u32>,
    shortest_change_id_len: Vec<u32>,
    flags: Vec<u8>,
    bookmarks: HashMap<usize, Vec<String>>,
}

impl CommitStore {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    // The `len`'s companion (kept for the clippy lint and API completeness);
    // no caller needs it yet.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn row(&self, index: usize) -> RowView<'_> {
        RowView { store: self, index }
    }

    pub fn iter(&self) -> impl Iterator<Item = RowView<'_>> {
        (0..self.len()).map(move |index| RowView { store: self, index })
    }

    pub fn find_by_change_id(&self, change_id: &str) -> Option<RowView<'_>> {
        self.iter().find(|row| row.change_id() == change_id)
    }

    pub fn working_copy(&self) -> Option<RowView<'_>> {
        self.iter().find(|row| row.is_working_copy())
    }

    /// Set a row's empty status, used to fill in the merge/root commits left
    /// "unknown" by the loader once they've been computed in the background.
    pub fn set_is_empty(&mut self, index: usize, empty: bool) {
        if let Some(flags) = self.flags.get_mut(index) {
            *flags |= commit_flags::IS_EMPTY_KNOWN;
            if empty {
                *flags |= commit_flags::IS_EMPTY;
            } else {
                *flags &= !commit_flags::IS_EMPTY;
            }
        }
    }

    fn slice(&self, span: Span) -> &str {
        &self.text[span.start as usize..(span.start + span.len) as usize]
    }

    /// Approximate retained heap of the store in bytes, for the `track-alloc`
    /// memory profiler. Sums the backing capacity of every arena/vec plus each
    /// row's lane allocations, so it can be compared against the allocator's
    /// live/peak counters to see how much of RSS is live data vs transient.
    #[cfg(feature = "track-alloc")]
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let mut total = self.text.capacity();
        total += self.spans.capacity() * size_of::<CommitSpans>();
        total += self.authors.capacity() * size_of::<Arc<str>>();
        total += self.authors.iter().map(|name| name.len()).sum::<usize>();
        total += self.author_idx.capacity() * size_of::<u32>();
        total += self.shortest_change_id_len.capacity() * size_of::<u32>();
        total += self.flags.capacity();
        for names in self.bookmarks.values() {
            total += names.capacity() * size_of::<String>();
            total += names.iter().map(|name| name.capacity()).sum::<usize>();
        }
        total += self.bookmarks.capacity() * (size_of::<usize>() + size_of::<Vec<String>>());
        total
    }
}

/// Borrowed view of a single commit in a [`CommitStore`]. Field accessors
/// mirror the old `&CommitSummary` field reads.
#[derive(Clone, Copy)]
pub struct RowView<'a> {
    store: &'a CommitStore,
    index: usize,
}

impl<'a> RowView<'a> {
    pub fn change_id(&self) -> &'a str {
        self.store.slice(self.store.spans[self.index].change_id)
    }

    pub fn commit_id(&self) -> &'a str {
        self.store.slice(self.store.spans[self.index].commit_id)
    }

    pub fn description(&self) -> &'a str {
        self.store.slice(self.store.spans[self.index].description)
    }

    pub fn author(&self) -> &'a str {
        &self.store.authors[self.store.author_idx[self.index] as usize]
    }

    pub fn shortest_change_id_len(&self) -> Option<usize> {
        match self.store.shortest_change_id_len[self.index] {
            0 => None,
            n => Some(n as usize),
        }
    }

    fn flags(&self) -> u8 {
        self.store.flags[self.index]
    }

    pub fn has_description(&self) -> bool {
        self.flags() & commit_flags::HAS_DESCRIPTION != 0
    }

    pub fn is_empty(&self) -> Option<bool> {
        if self.flags() & commit_flags::IS_EMPTY_KNOWN == 0 {
            None
        } else {
            Some(self.flags() & commit_flags::IS_EMPTY != 0)
        }
    }

    pub fn has_conflict(&self) -> bool {
        self.flags() & commit_flags::HAS_CONFLICT != 0
    }

    pub fn is_working_copy(&self) -> bool {
        self.flags() & commit_flags::IS_WORKING_COPY != 0
    }

    pub fn bookmarks(&self) -> &'a [String] {
        self.store
            .bookmarks
            .get(&self.index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Accumulates [`CommitSummary`]s into a [`CommitStore`], interning as it goes.
#[derive(Default)]
pub struct CommitStoreBuilder {
    store: CommitStore,
    author_interner: HashMap<String, u32>,
}

impl CommitStoreBuilder {
    pub fn with_capacity(commits: usize) -> Self {
        let mut store = CommitStore::default();
        store.spans.reserve(commits);
        store.author_idx.reserve(commits);
        store.shortest_change_id_len.reserve(commits);
        store.flags.reserve(commits);
        Self {
            store,
            author_interner: HashMap::new(),
        }
    }

    pub fn push(&mut self, commit: CommitSummary) {
        let index = self.store.spans.len();
        let change_id = self.intern_text(&commit.change_id);
        let commit_id = self.intern_text(&commit.commit_id);
        let description = self.intern_text(&commit.description);
        self.store.spans.push(CommitSpans {
            change_id,
            commit_id,
            description,
        });

        let author = self.intern_author(&commit.author);
        self.store.author_idx.push(author);

        self.store
            .shortest_change_id_len
            .push(commit.shortest_change_id_len.unwrap_or(0) as u32);

        let mut flags = 0u8;
        if commit.has_description {
            flags |= commit_flags::HAS_DESCRIPTION;
        }
        if let Some(empty) = commit.is_empty {
            flags |= commit_flags::IS_EMPTY_KNOWN;
            if empty {
                flags |= commit_flags::IS_EMPTY;
            }
        }
        if commit.has_conflict {
            flags |= commit_flags::HAS_CONFLICT;
        }
        if commit.is_working_copy {
            flags |= commit_flags::IS_WORKING_COPY;
        }
        self.store.flags.push(flags);

        if !commit.bookmarks.is_empty() {
            self.store.bookmarks.insert(index, commit.bookmarks);
        }
    }

    fn intern_text(&mut self, text: &str) -> Span {
        let start = self.store.text.len() as u32;
        self.store.text.push_str(text);
        Span {
            start,
            len: text.len() as u32,
        }
    }

    fn intern_author(&mut self, author: &str) -> u32 {
        if let Some(&idx) = self.author_interner.get(author) {
            return idx;
        }
        let idx = self.store.authors.len() as u32;
        self.store.authors.push(Arc::from(author));
        self.author_interner.insert(author.to_owned(), idx);
        idx
    }

    pub fn finish(self) -> CommitStore {
        self.store
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

impl DiffFileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Modified => "Modified",
            Self::Renamed => "Renamed",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendOutput {
    pub document: DiffDocument,
    pub commits: CommitStore,
    pub graph: GraphLayout,
    pub snapshot: RepositorySnapshot,
    pub details: Option<RevisionDetails>,
}

/// `jj show`-style summary of a single revision, used to render the header
/// strip above the diff view.
#[derive(Debug, Clone, Default)]
pub struct RevisionDetails {
    pub commit_id: String,
    pub change_id: Option<String>,
    pub bookmarks: Vec<String>,
    pub author: SignatureInfo,
    pub committer: Option<SignatureInfo>,
    pub signature: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Default)]
pub struct SignatureInfo {
    pub name: String,
    pub email: String,
    pub timestamp: Option<String>,
}

pub async fn load_backend(
    repository: Repository,
    revision: RevisionSelection,
    progress: LoadProgress,
) -> Result<BackendOutput, String> {
    run_backend(repository, revision, progress)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_backend(
    repository: Repository,
    revision: RevisionSelection,
    progress: LoadProgress,
) -> Result<BackendOutput> {
    // Snapshot the working copy *before* reading the graph and diff, so both
    // reflect the on-disk state. Loading first showed a stale, pre-snapshot
    // working-copy commit — e.g. `@` flagged "empty" while its diff actually
    // had changes — until the next refresh re-read it.
    let snapshot = run_repository_snapshot(repository.clone()).await?;
    let (commits, graph) = load_commits(&repository, &progress).await?;
    let (document, details) = run_diff(&repository, &revision).await?;

    Ok(BackendOutput {
        document,
        commits,
        graph,
        snapshot,
        details,
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
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::compute_jj_empty_status(root, targets))
            })
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
        }
        Vcs::Git => Vec::new(),
    }
}

async fn run_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_repository_snapshot(repository))
            })
            .await
            .context("jj repository snapshot task failed")?
        }
        Vcs::Git => crate::git::load_git_repository_snapshot(&repository.root).await,
    }
}

async fn load_commits(
    repository: &Repository,
    progress: &LoadProgress,
) -> Result<(CommitStore, GraphLayout)> {
    match repository.vcs {
        Vcs::Jj => {
            let root = repository.root.clone();
            let progress = progress.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::load_jj_commits(root, progress))
            })
            .await
            .context("jj commit loader task failed")?
        }
        Vcs::Git => {
            // The git loader parses `git log` in one shot rather than
            // per-commit, so surface the count once it's known, then fold the
            // summaries into the compact store.
            let (commits, graph) = crate::git::load_git_commits(repository).await?;
            progress.set_total(commits.len());
            let mut builder = CommitStoreBuilder::with_capacity(commits.len());
            for commit in commits {
                builder.push(commit);
            }
            Ok((builder.finish(), graph))
        }
    }
}

#[derive(Debug, Clone)]
struct PendingHunk {
    header: String,
    rows: Vec<DiffLine>,
    next_old_line: usize,
    next_new_line: usize,
}

pub fn format_hunk_header(
    old_range: &std::ops::Range<usize>,
    new_range: &std::ops::Range<usize>,
) -> String {
    format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_range),
        format_hunk_range(new_range)
    )
}

fn format_hunk_range(range: &std::ops::Range<usize>) -> String {
    let len = range.end.saturating_sub(range.start);
    let start = if len == 0 {
        range.start
    } else {
        range.start + 1
    };

    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

/// Parse `git diff` / `git show` style unified diff output into a
/// `DiffDocument`. Used by the git backend; the jj backend builds files
/// directly from materialized trees and only feeds individual hunks through
/// `format_hunk_header` + line construction.
pub fn parse_unified_diff(output: &str) -> DiffDocument {
    let mut files = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<PendingHunk> = None;

    for line in output.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            flush_current_file(&mut files, &mut current_file, &mut current_hunk);

            let (old_path, path) = parse_diff_git_paths(paths);
            current_file = Some(DiffFile {
                path,
                old_path,
                status: DiffFileStatus::Modified,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("@@") {
            flush_current_hunk(file, &mut current_hunk);
            let (next_old_line, next_new_line) = parse_hunk_header(line);
            current_hunk = Some(PendingHunk {
                header: line.to_owned(),
                rows: Vec::new(),
                next_old_line,
                next_new_line,
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            push_hunk_row(file, hunk, line);
        } else {
            update_file_metadata(file, line);
        }
    }

    flush_current_file(&mut files, &mut current_file, &mut current_hunk);

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    DiffDocument {
        files,
        total_additions,
        total_deletions,
    }
}

fn flush_current_file(
    files: &mut Vec<DiffFile>,
    current_file: &mut Option<DiffFile>,
    current_hunk: &mut Option<PendingHunk>,
) {
    if let Some(file) = current_file.as_mut() {
        flush_current_hunk(file, current_hunk);
    }

    if let Some(mut file) = current_file.take() {
        apply_syntax_highlighting(&mut file);
        files.push(file);
    }
}

fn flush_current_hunk(file: &mut DiffFile, current_hunk: &mut Option<PendingHunk>) {
    if let Some(hunk) = current_hunk.take() {
        file.hunks.push(DiffHunkView {
            header: hunk.header,
            lines: hunk.rows,
        });
    }
}

fn update_file_metadata(file: &mut DiffFile, line: &str) {
    if let Some(path) = line.strip_prefix("rename from ") {
        file.old_path = Some(path.to_owned());
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.path = path.to_owned();
        file.status = DiffFileStatus::Renamed;
    } else if line.starts_with("new file mode ") || line == "--- /dev/null" {
        file.status = DiffFileStatus::Added;
    } else if line.starts_with("deleted file mode ") || line == "+++ /dev/null" {
        file.status = DiffFileStatus::Deleted;
    } else if let Some(path) = line.strip_prefix("--- a/") {
        file.old_path = Some(path.to_owned());
    } else if let Some(path) = line.strip_prefix("+++ b/") {
        file.path = path.to_owned();
    }
}

fn push_hunk_row(file: &mut DiffFile, hunk: &mut PendingHunk, line: &str) {
    match line.chars().next() {
        Some('+') => {
            file.additions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_new_line += 1;
        }
        Some('-') => {
            file.deletions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Deletion,
                old_line: Some(hunk.next_old_line),
                new_line: None,
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_old_line += 1;
        }
        Some(' ') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(hunk.next_old_line),
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_old_line += 1;
            hunk.next_new_line += 1;
        }
        Some('\\') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Note,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
            });
        }
        _ => {
            let kind = if is_conflict_marker(line) {
                DiffLineKind::Conflict
            } else {
                DiffLineKind::Note
            };

            hunk.rows.push(DiffLine {
                kind,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
            });
        }
    }
}

fn is_conflict_marker(line: &str) -> bool {
    line.starts_with("<<<<<<<")
        || line.starts_with("|||||||")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
}

fn parse_diff_git_paths(paths: &str) -> (Option<String>, String) {
    let mut parts = paths.split_whitespace();
    let old_path = parts.next().map(clean_git_diff_path);
    let path = parts
        .next()
        .map(clean_git_diff_path)
        .or_else(|| old_path.clone())
        .unwrap_or_else(|| "<unknown>".to_owned());

    (old_path, path)
}

fn clean_git_diff_path(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

fn parse_hunk_header(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    let _marker = parts.next();
    let old = parts.next().unwrap_or_default();
    let new = parts.next().unwrap_or_default();

    (
        parse_hunk_range(old, '-').unwrap_or(0),
        parse_hunk_range(new, '+').unwrap_or(0),
    )
}

fn parse_hunk_range(part: &str, prefix: char) -> Option<usize> {
    part.strip_prefix(prefix)
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<usize>().ok())
}

/// Apply syntax highlighting to all visible diff lines for `file`.
///
/// We previously fed each line to tree-sitter individually, which was
/// fundamentally wrong: tree-sitter expects a complete document, so a line
/// like `fn foo(` parses as an error, `}` on its own gets no captures, and
/// every multi-line construct (string literals, function bodies, doc
/// comments, raw strings) is invisible to the parser. The result was that
/// keywords mid-block went un-highlighted and noisy single-character lines
/// would silently fall back to plain text.
///
/// The fix: reconstruct each "side" (old and new) of the file as a single
/// contiguous document, parse it once, and map the resulting spans back to
/// individual lines. Blank lines fill the gaps between hunks so each
/// surviving line still sits at its original line number — the parser
/// won't see the surrounding code, but tree-sitter is reasonably tolerant
/// of missing top-level constructs and will still recover local syntax
/// (literals, identifiers, comments, keywords) correctly within each hunk.
///
/// Context lines are highlighted from the new side (they're identical on
/// both sides, but we only need to look them up once); deletions come from
/// the old side; additions from the new side. Note/Conflict lines are
/// rendered as plain text — they aren't real source content.
pub fn apply_syntax_highlighting(file: &mut DiffFile) {
    static GRAMMAR_STORE: OnceLock<GrammarStore> = OnceLock::new();

    let Some(language) = arborium::detect_language(&file.path) else {
        return;
    };
    let store = GRAMMAR_STORE.get_or_init(GrammarStore::new);
    let Some(grammar) = store.get(language) else {
        return;
    };

    let new_spans = parse_side(&grammar, file, Side::New);
    let old_spans = parse_side(&grammar, file, Side::Old);

    for (hunk_index, hunk) in file.hunks.iter_mut().enumerate() {
        for (line_index, line) in hunk.lines.iter_mut().enumerate() {
            if matches!(line.kind, DiffLineKind::Note | DiffLineKind::Conflict) {
                continue;
            }

            let key = (hunk_index, line_index);
            let spans = match line.kind {
                DiffLineKind::Deletion => old_spans.get(&key),
                DiffLineKind::Addition | DiffLineKind::Context => new_spans.get(&key),
                DiffLineKind::Note | DiffLineKind::Conflict => None,
            };

            if let Some(spans) = spans {
                line.syntax = spans.clone();
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// Reconstruct one side of `file` as a single document, parse it, and slice
/// the resulting captures into per-line span lists keyed by
/// `(hunk_index, line_index)`.
fn parse_side(
    grammar: &Arc<CompiledGrammar>,
    file: &DiffFile,
    side: Side,
) -> HashMap<(usize, usize), Vec<SyntaxSpan>> {
    // For each line we keep on this side, record its byte range in the
    // reconstructed buffer so we can map captures back later.
    struct LineRange {
        hunk_index: usize,
        line_index: usize,
        start: usize,
        end: usize,
    }

    let mut buf = String::new();
    let mut ranges: Vec<LineRange> = Vec::new();
    // 1-indexed cursor over the source-file line numbers we've reached so far.
    let mut current_source_line: usize = 1;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let included = matches!(
                (side, line.kind),
                (Side::Old, DiffLineKind::Context | DiffLineKind::Deletion)
                    | (Side::New, DiffLineKind::Context | DiffLineKind::Addition)
            );
            if !included {
                continue;
            }

            let source_line = match side {
                Side::Old => line.old_line,
                Side::New => line.new_line,
            };
            let Some(target) = source_line else {
                continue;
            };

            // Pad blank lines so this content sits at its true line number.
            // Tree-sitter will see structurally-meaningless gaps but its
            // error-recovery handles that cleanly for most languages.
            while current_source_line < target {
                buf.push('\n');
                current_source_line += 1;
            }

            let start = buf.len();
            buf.push_str(&line.content);
            let end = buf.len();
            buf.push('\n');
            current_source_line += 1;

            ranges.push(LineRange {
                hunk_index,
                line_index,
                start,
                end,
            });
        }
    }

    if buf.trim().is_empty() || ranges.is_empty() {
        return HashMap::new();
    }

    let Ok(mut context) = ParseContext::for_grammar(grammar) else {
        return HashMap::new();
    };
    let result = grammar.parse(&mut context, &buf);

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();

    for span in result.spans {
        let Some(kind) = syntax_kind_for_capture(&span.capture) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start >= end || end > buf.len() {
            continue;
        }
        if !buf.is_char_boundary(start) || !buf.is_char_boundary(end) {
            continue;
        }

        // Walk every line that the span overlaps. Most spans live entirely
        // within one line so this loop usually fires once, but multi-line
        // constructs (block comments, raw strings) need to highlight every
        // covered line.
        for range in &ranges {
            if range.end <= start || range.start >= end {
                continue;
            }
            let local_start = start.saturating_sub(range.start);
            let local_end = (end - range.start).min(range.end - range.start);
            if local_start >= local_end {
                continue;
            }
            per_line
                .entry((range.hunk_index, range.line_index))
                .or_default()
                .push(SyntaxSpan {
                    start: local_start,
                    end: local_end,
                    kind,
                });
        }
    }

    for spans in per_line.values_mut() {
        *spans = normalize_syntax_spans(std::mem::take(spans));
    }
    per_line
}

fn syntax_kind_for_capture(capture: &str) -> Option<SyntaxKind> {
    if capture.starts_with("comment") {
        Some(SyntaxKind::Comment)
    } else if capture.starts_with("string") || capture == "character" {
        Some(SyntaxKind::String)
    } else if capture.starts_with("number")
        || capture.starts_with("constant")
        || capture == "boolean"
    {
        Some(SyntaxKind::Number)
    } else if capture.starts_with("keyword")
        || capture == "operator"
        || capture == "include"
        || capture == "storageclass"
    {
        Some(SyntaxKind::Keyword)
    } else if capture.starts_with("function") || capture == "constructor" || capture == "method" {
        Some(SyntaxKind::Function)
    } else if capture.starts_with("type") || capture == "variable.builtin" {
        Some(SyntaxKind::Type)
    } else if capture.starts_with("property")
        || capture == "variable.parameter"
        || capture == "field"
        || capture == "attribute"
        || capture == "tag"
    {
        Some(SyntaxKind::Property)
    } else if capture.starts_with("punctuation") {
        Some(SyntaxKind::Punctuation)
    } else {
        None
    }
}

fn normalize_syntax_spans(mut spans: Vec<SyntaxSpan>) -> Vec<SyntaxSpan> {
    spans.sort_by_key(|span| (span.start, span.end));

    let mut normalized: Vec<SyntaxSpan> = Vec::with_capacity(spans.len());
    for mut span in spans {
        if let Some(previous) = normalized.last()
            && span.start < previous.end
        {
            span.start = previous.end;
        }

        if span.start < span.end {
            normalized.push(span);
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_diff_into_hunks_and_rows() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,2 +10,3 @@ fn demo()\n let old_value = 0;\n-let old_value = 1;\n+let new_value = 1;\n+let second_line = 2;\n",
        );

        assert_eq!(document.files.len(), 1);
        assert_eq!(document.total_additions, 2);
        assert_eq!(document.total_deletions, 1);

        let file = &document.files[0];
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(file.hunks[0].lines[1].old_line, Some(11));
        assert_eq!(file.hunks[0].lines[1].new_line, None);
        assert_eq!(file.hunks[0].lines[2].kind, DiffLineKind::Addition);
        assert_eq!(file.hunks[0].lines[2].old_line, None);
        assert_eq!(file.hunks[0].lines[2].new_line, Some(11));
        assert!(!file.hunks[0].lines[2].syntax.is_empty());
    }

    #[test]
    fn parses_conflict_markers() {
        let document = parse_unified_diff(
            "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n<<<<<<< mine\n",
        );

        assert_eq!(
            document.files[0].hunks[0].lines[0].kind,
            DiffLineKind::Conflict
        );
    }

    #[test]
    fn parses_rename_metadata() {
        let document = parse_unified_diff(
            "diff --git a/src/old.rs b/src/new.rs\nsimilarity index 100%\nrename from src/old.rs\nrename to src/new.rs\n",
        );

        let file = &document.files[0];
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.path, "src/new.rs");
    }

    #[test]
    fn empty_diff_yields_no_files() {
        let document = parse_unified_diff("");

        assert!(document.files.is_empty());
        assert_eq!(document.total_additions, 0);
        assert_eq!(document.total_deletions, 0);
    }
}
