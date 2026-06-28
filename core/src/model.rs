//! Domain data model for diffui: the diff document, the compact commit store,
//! and the per-revision metadata the loaders produce. Pure data — no `iced`.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::graph::LaneFrame;
use crate::graph_layout::GraphLayout;
use crate::repository::RepositorySnapshot;

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

    /// Set the absolute loaded count. For sources that report a running total
    /// rather than per-item ticks (e.g. git's transfer progress), as opposed to
    /// [`increment`](Self::increment).
    pub fn set_loaded(&self, loaded: usize) {
        self.loaded.store(loaded, Ordering::Relaxed);
    }

    /// `(loaded, total)` so far. `total == 0` means the count isn't known yet.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.loaded.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RevisionSelection {
    #[default]
    WorkingCopy,
    /// A specific revision by **commit-id hex** (jj commit id / git sha) —
    /// never a jj change id. The jj backend feeds this straight into
    /// `CommitId::try_from_hex`; a change id (the k–z alphabet) must be
    /// resolved through the commit store first (the palette does this in
    /// `revision_selection`).
    Commit(String),
}

impl RevisionSelection {
    pub fn view_key(&self) -> String {
        match self {
            Self::WorkingCopy => "working-copy".to_owned(),
            Self::Commit(commit_id) => format!("commit:{commit_id}"),
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
    /// Reverse index: bookmark name → owning row. Bookmark names are unique per
    /// ref, so this is 1:1 and lets `find_by_bookmark` resolve in O(1) instead
    /// of scanning every commit (the palette hits this per displayed row).
    bookmark_index: HashMap<String, usize>,
    /// Row of the working-copy (`@`) commit, recorded at push time so
    /// `working_copy`/`working_copy_index` are O(1). The tab bar reads the
    /// working copy's empty status for every tab on every frame.
    working_copy_row: Option<usize>,
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

    /// Iterate only the rows that carry bookmarks, as `(row index, names)`.
    /// Bookmarks are stored sparsely, so this is `O(#bookmarks)` — the palette
    /// uses it to list branch candidates without scanning all ~1M commits per
    /// keystroke. Order is unspecified (the caller sorts when it needs one).
    pub fn bookmarked_rows(&self) -> impl Iterator<Item = (usize, &[String])> {
        self.bookmarks
            .iter()
            .map(|(&index, names)| (index, names.as_slice()))
    }

    pub fn find_by_change_id(&self, change_id: &str) -> Option<RowView<'_>> {
        self.iter().find(|row| row.change_id() == change_id)
    }

    pub fn find_by_commit_id(&self, commit_id: &str) -> Option<RowView<'_>> {
        self.iter().find(|row| row.commit_id() == commit_id)
    }

    /// Row owning bookmark `name`, resolved through the reverse index (O(1))
    /// rather than scanning every commit's bookmark list.
    pub fn find_by_bookmark(&self, name: &str) -> Option<RowView<'_>> {
        self.bookmark_index.get(name).map(|&index| self.row(index))
    }

    pub fn working_copy(&self) -> Option<RowView<'_>> {
        self.working_copy_row.map(|index| self.row(index))
    }

    /// Row index of the working-copy (`@`) commit, if it's in the loaded graph.
    /// Used to refresh @'s "empty" chip from a working-copy snapshot without
    /// re-walking the graph.
    pub fn working_copy_index(&self) -> Option<usize> {
        self.working_copy_row
    }

    /// Shortest unique change-id prefix length for every commit, in store order.
    ///
    /// jj precomputes this per commit (`shortest_change_id_len`) against the repo
    /// index, so when every row carries it we map straight through (O(n)). The
    /// git backend leaves it `None`; there we derive each prefix from sorted
    /// neighbors — a prefix is unique once it's one character longer than the
    /// longest prefix the id shares with either lexicographic neighbor. This is
    /// domain logic over the commit store (not rendering), so it lives in core
    /// where any frontend's sidebar can reuse it.
    pub fn shortest_unique_prefix_lens(&self) -> Vec<usize> {
        /// Number of leading characters two strings share.
        fn common_prefix_len(a: &str, b: &str) -> usize {
            a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
        }

        if (0..self.len()).all(|i| self.row(i).shortest_change_id_len().is_some()) {
            return (0..self.len())
                .map(|i| {
                    let row = self.row(i);
                    row.shortest_change_id_len()
                        .unwrap_or(1)
                        .min(row.change_id().chars().count())
                })
                .collect();
        }

        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by(|&a, &b| self.row(a).change_id().cmp(self.row(b).change_id()));

        let mut lens = vec![0usize; self.len()];
        for (rank, &idx) in order.iter().enumerate() {
            let change_id = self.row(idx).change_id();
            let total = change_id.chars().count();
            lens[idx] = if let Some(precomputed) = self.row(idx).shortest_change_id_len() {
                precomputed.min(total)
            } else {
                let prev = rank
                    .checked_sub(1)
                    .map(|r| common_prefix_len(change_id, self.row(order[r]).change_id()))
                    .unwrap_or(0);
                let next = order
                    .get(rank + 1)
                    .map(|&n| common_prefix_len(change_id, self.row(n).change_id()))
                    .unwrap_or(0);
                (prev.max(next) + 1).min(total)
            };
        }
        lens
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

    /// Append one commit, interning its author through `author_interner` (kept
    /// by the caller so it persists across a streaming load's batches and is
    /// freed when the load ends). Strings land in the shared arena; flags pack
    /// into a byte; bookmarks are stored sparsely. The batch
    /// [`CommitStoreBuilder`] is a thin wrapper over this.
    pub fn push(&mut self, commit: CommitSummary, author_interner: &mut HashMap<String, u32>) {
        let index = self.spans.len();
        let change_id = self.intern_text(&commit.change_id);
        let commit_id = self.intern_text(&commit.commit_id);
        let description = self.intern_text(&commit.description);
        self.spans.push(CommitSpans {
            change_id,
            commit_id,
            description,
        });

        let author = intern_author(&mut self.authors, author_interner, &commit.author);
        self.author_idx.push(author);

        self.shortest_change_id_len
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
            // First-wins, matching the old `iter().find` scan this replaced.
            if self.working_copy_row.is_none() {
                self.working_copy_row = Some(index);
            }
        }
        self.flags.push(flags);

        if !commit.bookmarks.is_empty() {
            for name in &commit.bookmarks {
                self.bookmark_index.insert(name.clone(), index);
            }
            self.bookmarks.insert(index, commit.bookmarks);
        }
    }

    fn intern_text(&mut self, text: &str) -> Span {
        // The arena addresses every interned string with `u32` offsets, so the
        // total interned text must stay under 4 GiB. That ceiling is far beyond
        // any real log/diff payload; the assert turns a silent truncation (and
        // the corrupted slices it would yield) into a loud debug-build failure.
        debug_assert!(
            self.text.len() + text.len() <= u32::MAX as usize,
            "CommitStore text arena exceeded u32 addressing range"
        );
        let start = self.text.len() as u32;
        self.text.push_str(text);
        Span {
            start,
            len: text.len() as u32,
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
        self.store.push(commit, &mut self.author_interner);
    }

    pub fn finish(self) -> CommitStore {
        self.store
    }
}

/// Intern `author` into `authors`, returning its index. The `interner` maps a
/// name to its slot so a repo's handful of distinct authors are stored once
/// even across a million commits. Shared by [`CommitStore::push`] (the lone
/// caller); kept free-standing so it can borrow `authors` and `interner`
/// disjointly from the rest of the store.
fn intern_author(
    authors: &mut Vec<Arc<str>>,
    interner: &mut HashMap<String, u32>,
    author: &str,
) -> u32 {
    if let Some(&idx) = interner.get(author) {
        return idx;
    }
    let idx = authors.len() as u32;
    authors.push(Arc::from(author));
    interner.insert(author.to_owned(), idx);
    idx
}

/// One display row of the changed-files tree: a collapsible directory
/// (possibly a compacted single-child chain, e.g. `src/sub/dir`) or a file
/// leaf pointing back into the `files` slice it was built from. Produced by
/// [`file_tree_rows`]; the frontend renders these flat with `depth`-based
/// indentation, so its row virtualization stays simple arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTreeRow {
    Dir {
        /// Display name: the path component, or a `a/b/c` chain when every
        /// intermediate directory has exactly one child.
        label: String,
        /// Full path prefix from the root — the stable collapse key.
        path: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        file_index: usize,
        /// The basename (the tree shows structure; the full path lives on
        /// the file entry itself).
        label: String,
        depth: usize,
    },
}

/// Flatten `files` into tree display rows: directories first (alphabetical,
/// single-child chains compacted), then files (alphabetical), recursively —
/// skipping the contents of any directory whose path is in `collapsed`.
pub fn file_tree_rows(
    files: &[DiffFile],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<FileTreeRow> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: BTreeMap<String, usize>,
    }

    let mut root = Node::default();
    for (index, file) in files.iter().enumerate() {
        let mut node = &mut root;
        let mut components = file.path.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                // Duplicate basenames within one directory can't happen in
                // one diff; last-wins is harmless if a source ever emits one.
                node.files.insert(component.to_owned(), index);
            } else {
                node = node.dirs.entry(component.to_owned()).or_default();
            }
        }
    }

    fn emit(
        node: &Node,
        prefix: &str,
        depth: usize,
        collapsed: &std::collections::HashSet<String>,
        rows: &mut Vec<FileTreeRow>,
    ) {
        for (name, child) in &node.dirs {
            // Compact single-child directory chains into one row.
            let mut label = name.clone();
            let mut child = child;
            while child.files.is_empty() && child.dirs.len() == 1 {
                let Some((next_name, next_child)) = child.dirs.iter().next() else {
                    break;
                };
                label.push('/');
                label.push_str(next_name);
                child = next_child;
            }
            let path = if prefix.is_empty() {
                label.clone()
            } else {
                format!("{prefix}/{label}")
            };
            let is_collapsed = collapsed.contains(&path);
            rows.push(FileTreeRow::Dir {
                label,
                path: path.clone(),
                depth,
                collapsed: is_collapsed,
            });
            if !is_collapsed {
                emit(child, &path, depth + 1, collapsed, rows);
            }
        }
        for (name, &file_index) in &node.files {
            rows.push(FileTreeRow::File {
                file_index,
                label: name.clone(),
                depth,
            });
        }
    }

    let mut rows = Vec::with_capacity(files.len());
    emit(&root, "", 0, collapsed, &mut rows);
    rows
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
    pub branch_status: Option<BranchStatus>,
    pub bookmarks: BookmarksInfo,
}

/// One commit emitted by the streaming loader: the data for the compact store
/// plus the row's lane frame for the graph fold. The UI appends a batch of
/// these into its live `CommitStore` + `GraphLayout` per `CommitsBatch`
/// message, so the sidebar paints after the first batch instead of waiting for
/// the whole (up to ~1M-row) history to load.
#[derive(Debug, Clone)]
pub struct StreamRow {
    pub summary: CommitSummary,
    pub frame: LaneFrame,
}

/// Tail of a streaming load, delivered once the walk completes: the
/// working-copy snapshot fingerprint (held for refresh comparison) and the
/// `(row index, is_empty)` results for single-parent commits — resolved in one
/// final pass now that every tree-id is known (merges/roots are still left to
/// the background `compute_empty_status` path).
#[derive(Debug, Clone)]
pub struct CommitsTail {
    pub snapshot: RepositorySnapshot,
    pub empty_updates: Vec<(usize, bool)>,
    pub branch_status: Option<BranchStatus>,
    pub bookmarks: BookmarksInfo,
}

/// Working-copy branch summary for the sidebar footer: the nearest local
/// bookmark at or behind `@`, its tracked upstream (if any), and how far `@`
/// sits ahead/behind that upstream. `None` when the working copy has no local
/// bookmark in its ancestry, or for backends without branch tracking (git).
#[derive(Debug, Clone)]
pub struct BranchStatus {
    /// The nearest local bookmark at or behind `@`.
    pub branch: String,
    /// Tracked remote bookmark, e.g. `main@origin`. `None` when the branch has
    /// no tracking remote — the footer then shows just the name.
    pub upstream: Option<String>,
    /// Commits reachable from `@` but not the upstream (your unpushed work).
    pub ahead: usize,
    /// Commits reachable from the upstream but not `@`.
    pub behind: usize,
}

/// Every bookmark in the repo, with the state the revision context menu needs:
/// which commit each points at (to know which bookmarks sit on a right-clicked
/// revision) and per-remote tracking state (to offer push vs. track). Computed
/// once per load alongside [`BranchStatus`]; empty for git repos.
#[derive(Debug, Clone, Default)]
pub struct BookmarksInfo {
    pub bookmarks: Vec<BookmarkEntry>,
    /// `@`'s commit id (hex), so a working-copy right-click can resolve the
    /// bookmarks sitting on it.
    pub working_copy_commit: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BookmarkEntry {
    pub name: String,
    /// Commit id (hex) the local bookmark points at, if it has a local target.
    pub local_target: Option<String>,
    /// Remote-tracking copies of this bookmark.
    pub remotes: Vec<RemoteBookmarkRef>,
}

#[derive(Debug, Clone)]
pub struct RemoteBookmarkRef {
    pub remote: String,
    /// Commit id (hex) this remote ref points at.
    pub target: String,
    pub tracked: bool,
}

impl BookmarkEntry {
    /// The remote this local bookmark tracks, if any (first tracked remote).
    pub fn tracked_remote(&self) -> Option<&str> {
        self.remotes
            .iter()
            .find(|r| r.tracked)
            .map(|r| r.remote.as_str())
    }
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

// ---- Diff line model (moved here from the UI `diff_view` module) ----

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub content: String,
    pub syntax: Vec<SyntaxSpan>,
    /// Byte ranges of the tokens that actually changed within this line
    /// (intra-line/word diff), present on deletion/addition lines that pair
    /// up across a change. Empty for unpaired lines and for lines rewritten
    /// nearly wholesale, where token emphasis would just restate the line
    /// tint. See `diff_parse::mark_intra_line_changes`.
    pub emphasis: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct DiffHunkView {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Conflict,
    Note,
}

#[derive(Debug, Clone)]
pub struct SyntaxSpan {
    pub start: usize,
    pub end: usize,
    pub kind: SyntaxKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Property,
    Punctuation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn file(path: &str) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn file_tree_compacts_chains_and_sorts_dirs_first() {
        let files = vec![
            file("src/deep/only/child.rs"),
            file("src/main.rs"),
            file("README.md"),
            file("src/a.rs"),
        ];
        let rows = file_tree_rows(&files, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                FileTreeRow::Dir {
                    label: "src".to_owned(),
                    path: "src".to_owned(),
                    depth: 0,
                    collapsed: false,
                },
                FileTreeRow::Dir {
                    label: "deep/only".to_owned(),
                    path: "src/deep/only".to_owned(),
                    depth: 1,
                    collapsed: false,
                },
                FileTreeRow::File {
                    file_index: 0,
                    label: "child.rs".to_owned(),
                    depth: 2,
                },
                FileTreeRow::File {
                    file_index: 3,
                    label: "a.rs".to_owned(),
                    depth: 1,
                },
                FileTreeRow::File {
                    file_index: 1,
                    label: "main.rs".to_owned(),
                    depth: 1,
                },
                FileTreeRow::File {
                    file_index: 2,
                    label: "README.md".to_owned(),
                    depth: 0,
                },
            ]
        );
    }

    #[test]
    fn collapsed_dirs_hide_their_contents() {
        let files = vec![file("src/deep/only/child.rs"), file("src/main.rs")];
        let collapsed: HashSet<String> = ["src/deep/only".to_owned()].into();
        let rows = file_tree_rows(&files, &collapsed);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, FileTreeRow::File { .. }))
                .count(),
            1
        );
        assert!(rows.iter().any(|row| matches!(
            row,
            FileTreeRow::Dir {
                collapsed: true,
                ..
            }
        )));

        // Collapsing the root dir hides everything beneath it.
        let collapsed: HashSet<String> = ["src".to_owned()].into();
        let rows = file_tree_rows(&files, &collapsed);
        assert_eq!(rows.len(), 1);
    }
}
