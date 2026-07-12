//! Source-browsing backend: list every file reachable at a revision and read
//! one file's contents as a displayable document. VCS dispatch mirrors
//! [`crate::source`] — jj paths run on a blocking thread (jj-lib is `!Send`);
//! git shells out.
//!
//! A commit lists its tree (everything `Tracked`). The working copy instead
//! walks the real directory — like browsing the repo on disk — so untracked
//! and ignored files show up too, each classified so a frontend can style
//! them. Ignored *directories* with no tracked files inside are emitted as a
//! single unenumerated entry rather than walked (a `target/` or
//! `node_modules/` would otherwise dominate the listing).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::{DiffFile, DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind};
use crate::repository::{Repository, Vcs};
use crate::{RevisionSelection, syntax};

/// How a listed path relates to the revision's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEntryStatus {
    /// In the revision's tree.
    Tracked,
    /// On disk but not in the tree, and not matched by any ignore rule
    /// (working copy only; rare under jj's auto-tracking).
    Untracked,
    /// On disk but matched by a gitignore rule (working copy only).
    Ignored,
}

/// One path in a revision's source listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// Repo-relative, `/`-separated.
    pub path: String,
    /// `true` for an ignored directory whose contents were *not* enumerated —
    /// the entry stands in for its subtree until it's lazily listed (see
    /// [`list_ignored_dir`]).
    pub is_dir: bool,
    pub status: SourceEntryStatus,
    /// Working-copy listings only: how the (tracked) file changed relative to
    /// the revision's parents — the diff-status chip (`A`/`M`/`C`). `None`
    /// for unchanged files, non-wc listings, and untracked/ignored entries.
    pub change: Option<DiffFileStatus>,
}

impl SourceEntry {
    pub(crate) fn new(path: String, is_dir: bool, status: SourceEntryStatus) -> Self {
        Self {
            path,
            is_dir,
            status,
            change: None,
        }
    }
}

/// A file read for the source browser, ready to render: the content as a
/// single all-context "diff" file (so a diff-oriented frontend can reuse its
/// code view), plus what made it unreadable when `file` is empty.
#[derive(Debug, Clone)]
pub struct SourceFileLoad {
    /// One hunk of context lines with `new_line` numbering and syntax spans
    /// applied. Empty hunks when the file is binary/oversized.
    pub file: DiffFile,
    pub line_count: usize,
    pub binary: bool,
    pub too_large: bool,
    /// File size in bytes as read (compressed sizes are not resolved).
    pub byte_len: usize,
}

/// Raw per-VCS read result, before synthesis into a [`SourceFileLoad`].
#[derive(Debug, Clone, Default)]
pub struct SourceFileData {
    /// `None` when binary or oversized.
    pub content: Option<String>,
    pub binary: bool,
    pub too_large: bool,
    pub byte_len: usize,
}

/// Files larger than this aren't loaded into the browser at all.
pub const MAX_SOURCE_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Files larger than this load but skip tree-sitter highlighting — the parse
/// would cost more than the colors are worth.
const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;

/// List every path at `revision`. Commits list their tree; the working copy
/// walks the directory (see module docs). Entries are sorted by path.
pub async fn list_source_tree(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<Vec<SourceEntry>, String> {
    let result = match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::list_jj_source_tree(repository, revision))
            })
            .await
            .map_err(|error| format!("jj source-tree task failed: {error}"))?
        }
        Vcs::Git => crate::git::list_git_source_tree(&repository, &revision).await,
    };
    result
        .map(|mut entries| {
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            entries
        })
        .map_err(|error| format!("{error:#}"))
}

/// Read `path` at `revision` and synthesize the displayable document —
/// including the (potentially seconds-long, hence off-thread) syntax parse.
pub async fn load_source_file(
    repository: Repository,
    revision: RevisionSelection,
    path: String,
) -> Result<SourceFileLoad, String> {
    let data = match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            let repository = repository.clone();
            let revision = revision.clone();
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                handle.block_on(crate::jj::read_jj_source_file(repository, revision, path))
            })
            .await
            .map_err(|error| format!("jj source read task failed: {error}"))?
        }
        Vcs::Git => crate::git::read_git_source_file(&repository, &revision, &path).await,
    }
    .map_err(|error| format!("{error:#}"))?;

    let byte_len = data.byte_len;
    let (binary, too_large) = (data.binary, data.too_large);
    let Some(content) = data.content else {
        return Ok(SourceFileLoad {
            file: empty_source_file(&path),
            line_count: 0,
            binary,
            too_large,
            byte_len,
        });
    };

    // Synthesis + tree-sitter run off-thread: a parse of a large file is
    // seconds of CPU, exactly like the diff highlighter's rationale.
    let joined = tokio::task::spawn_blocking(move || {
        let mut file = synthesize_source_file(&path, &content);
        if content.len() <= MAX_HIGHLIGHT_BYTES {
            syntax::apply_syntax_highlighting_with_sources(&mut file, None, Some(&content));
        }
        file
    })
    .await
    .map_err(|error| format!("source synthesis task failed: {error}"))?;

    let line_count = joined
        .hunks
        .first()
        .map(|hunk| hunk.lines.len())
        .unwrap_or(0);
    Ok(SourceFileLoad {
        file: joined,
        line_count,
        binary: false,
        too_large: false,
        byte_len,
    })
}

/// Wrap full file contents as a single-hunk, all-context [`DiffFile`] with
/// 1-based `new_line` numbering (and no `old_line`, so the old-side syntax
/// reconstruction is a no-op). This is what lets the diff view render plain
/// source without a parallel document model.
pub fn synthesize_source_file(path: &str, content: &str) -> DiffFile {
    let lines: Vec<DiffLine> = content
        .lines()
        .enumerate()
        .map(|(index, line)| DiffLine {
            kind: DiffLineKind::Context,
            old_line: None,
            new_line: Some(index + 1),
            content: line.to_owned(),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        })
        .collect();
    DiffFile {
        path: path.to_owned(),
        old_path: None,
        // The status is never displayed for source documents; `Modified` is
        // just the least-marked placeholder.
        status: DiffFileStatus::Modified,
        hunks: if lines.is_empty() {
            Vec::new()
        } else {
            vec![DiffHunkView {
                header: String::new(),
                lines,
            }]
        },
        additions: 0,
        deletions: 0,
    }
}

fn empty_source_file(path: &str) -> DiffFile {
    DiffFile {
        path: path.to_owned(),
        old_path: None,
        status: DiffFileStatus::Modified,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
    }
}

/// One display row of the source file tree — the source-browser counterpart
/// of [`crate::model::FileTreeRow`], with entry statuses carried through so
/// ignored/untracked rows can style differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceTreeRow {
    Dir {
        /// Display name: the path component, or a compacted `a/b/c` chain.
        label: String,
        /// Full path prefix from the root — the stable expand key.
        path: String,
        depth: usize,
        collapsed: bool,
        /// The directory is gitignored — renders dimmed. Set for the
        /// unenumerated marker rows and for their lazily-listed children.
        ignored: bool,
        /// An ignored directory whose contents haven't been listed yet.
        /// Expanding it triggers a lazy listing instead of a plain toggle.
        unlisted: bool,
        /// Some descendant carries a VCS status (a diff change or an
        /// untracked file) — lets a collapsed dir signal "changes inside".
        has_changes: bool,
    },
    File {
        /// Index into the `entries` slice the rows were built from.
        entry_index: usize,
        label: String,
        depth: usize,
        status: SourceEntryStatus,
        /// Diff-status chip for changed tracked files at the working copy.
        change: Option<DiffFileStatus>,
    },
}

/// Flatten `entries` into display rows: directories first (alphabetical,
/// single-child chains compacted), then files, recursively. Directories are
/// **collapsed by default** — only those in `expanded` show their contents.
/// An unenumerated ignored dir ([`SourceEntry::is_dir`]) with no listed
/// children renders as `Dir { unlisted: true }`; once its children have been
/// lazily merged into `entries` it becomes a normal (still dimmed) directory.
pub fn source_tree_rows(entries: &[SourceEntry], expanded: &HashSet<String>) -> Vec<SourceTreeRow> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        files: BTreeMap<String, usize>,
        /// This directory is an ignored-dir entry itself (the unenumerated
        /// marker, or a lazily-listed subtree root).
        ignored: bool,
        /// Some file beneath carries a VCS status (change chip / untracked).
        has_changes: bool,
    }

    let mut root = Node::default();
    for (index, entry) in entries.iter().enumerate() {
        // A changed or untracked file marks every ancestor dir, so collapsed
        // dirs can still signal the changes inside them.
        let signals_change = !entry.is_dir
            && (entry.change.is_some() || entry.status == SourceEntryStatus::Untracked);
        let mut node = &mut root;
        let mut components = entry.path.split('/').peekable();
        while let Some(component) = components.next() {
            node.has_changes |= signals_change;
            if components.peek().is_none() {
                if entry.is_dir {
                    node.dirs.entry(component.to_owned()).or_default().ignored = true;
                } else {
                    node.files.insert(component.to_owned(), index);
                }
            } else {
                node = node.dirs.entry(component.to_owned()).or_default();
            }
        }
    }

    fn emit(
        node: &Node,
        entries: &[SourceEntry],
        prefix: &str,
        depth: usize,
        expanded: &HashSet<String>,
        inherited_ignored: bool,
        rows: &mut Vec<SourceTreeRow>,
    ) {
        for (name, child) in &node.dirs {
            let mut path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let ignored = inherited_ignored || child.ignored;
            let empty = child.dirs.is_empty() && child.files.is_empty();
            // An ignored marker with nothing beneath it hasn't been listed
            // yet; expanding it is a lazy load, not a toggle.
            if ignored && empty {
                rows.push(SourceTreeRow::Dir {
                    label: name.clone(),
                    path,
                    depth,
                    collapsed: true,
                    ignored: true,
                    unlisted: true,
                    has_changes: false,
                });
                continue;
            }
            // Compact single-child directory chains into one row — but never
            // into or through an ignored dir: its expand key must stay stable
            // as lazy listings fill it in.
            let mut label = name.clone();
            let mut child = child;
            while !child.ignored
                && child.files.is_empty()
                && child.dirs.len() == 1
                && !child.dirs.values().next().is_some_and(|next| next.ignored)
            {
                let Some((next_name, next_child)) = child.dirs.iter().next() else {
                    break;
                };
                label.push('/');
                label.push_str(next_name);
                path.push('/');
                path.push_str(next_name);
                child = next_child;
            }
            let is_collapsed = !expanded.contains(&path);
            rows.push(SourceTreeRow::Dir {
                label,
                path: path.clone(),
                depth,
                collapsed: is_collapsed,
                ignored,
                unlisted: false,
                has_changes: child.has_changes,
            });
            if !is_collapsed {
                emit(child, entries, &path, depth + 1, expanded, ignored, rows);
            }
        }
        for (name, &entry_index) in &node.files {
            rows.push(SourceTreeRow::File {
                entry_index,
                label: name.clone(),
                depth,
                status: entries[entry_index].status,
                change: entries[entry_index].change,
            });
        }
    }

    let mut rows = Vec::with_capacity(entries.len());
    emit(&root, entries, "", 0, expanded, false, &mut rows);
    rows
}

/// How many children one lazy ignored-dir listing returns at most. A level of
/// a `node_modules/` can hold thousands of entries; past the cap the listing
/// is truncated (the browser is for peeking, not for auditing).
pub const MAX_IGNORED_DIR_ENTRIES: usize = 2_000;

/// Lazily list **one level** of an ignored directory straight off the disk
/// (both VCS backends read ignored content from the filesystem — it exists
/// nowhere else). Children come back as `Ignored`: the parent only got its
/// unenumerated marker because nothing tracked lives beneath it, so
/// everything inside is ignored by inheritance. Subdirectories arrive as
/// unenumerated markers themselves, expanding level by level.
pub async fn list_ignored_dir(
    repository: Repository,
    dir: String,
) -> Result<Vec<SourceEntry>, String> {
    list_ignored_dir_inner(&repository, &dir).map_err(|error| format!("{error:#}"))
}

fn list_ignored_dir_inner(repository: &Repository, dir: &str) -> Result<Vec<SourceEntry>> {
    if dir.is_empty()
        || Path::new(dir)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("invalid directory {dir}");
    }
    let disk_dir = repository.root.join(dir);
    let read = std::fs::read_dir(&disk_dir)
        .with_context(|| format!("failed to read directory {}", disk_dir.display()))?;
    let mut entries = Vec::new();
    for item in read {
        let Ok(item) = item else { continue };
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".jj" || name == ".git" {
            continue;
        }
        let is_dir = file_type.is_dir() && !file_type.is_symlink();
        entries.push(SourceEntry::new(
            format!("{dir}/{name}"),
            is_dir,
            SourceEntryStatus::Ignored,
        ));
        if entries.len() >= MAX_IGNORED_DIR_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Classify on-disk bytes: binary sniff over the first 8 KiB (NUL byte), and
/// lossy UTF-8 for everything else. Shared by the jj and git working-copy
/// readers.
pub(crate) fn classify_disk_bytes(bytes: Vec<u8>) -> SourceFileData {
    let byte_len = bytes.len();
    if byte_len > MAX_SOURCE_FILE_BYTES {
        return SourceFileData {
            content: None,
            binary: false,
            too_large: true,
            byte_len,
        };
    }
    let sniff = &bytes[..bytes.len().min(8 * 1024)];
    if sniff.contains(&0) {
        return SourceFileData {
            content: None,
            binary: true,
            too_large: false,
            byte_len,
        };
    }
    SourceFileData {
        content: Some(String::from_utf8_lossy(&bytes).into_owned()),
        binary: false,
        too_large: false,
        byte_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, status: SourceEntryStatus) -> SourceEntry {
        SourceEntry::new(path.to_owned(), is_dir, status)
    }

    #[test]
    fn synthesized_file_numbers_lines_on_the_new_side() {
        let file = synthesize_source_file("src/main.rs", "fn main() {\n}\n");
        assert_eq!(file.hunks.len(), 1);
        let lines = &file.hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].new_line, Some(1));
        assert_eq!(lines[0].old_line, None);
        assert_eq!(lines[1].new_line, Some(2));
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Context));
    }

    #[test]
    fn synthesized_empty_file_has_no_hunks() {
        let file = synthesize_source_file("empty.txt", "");
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn tree_rows_collapse_by_default_and_mark_unlisted_ignored_dirs() {
        let entries = vec![
            entry("src/main.rs", false, SourceEntryStatus::Tracked),
            entry("target", true, SourceEntryStatus::Ignored),
            entry("scratch.txt", false, SourceEntryStatus::Untracked),
        ];
        // Nothing expanded: dirs render collapsed, their contents hidden.
        let rows = source_tree_rows(&entries, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                SourceTreeRow::Dir {
                    label: "src".to_owned(),
                    path: "src".to_owned(),
                    depth: 0,
                    collapsed: true,
                    ignored: false,
                    unlisted: false,
                    has_changes: false,
                },
                SourceTreeRow::Dir {
                    label: "target".to_owned(),
                    path: "target".to_owned(),
                    depth: 0,
                    collapsed: true,
                    ignored: true,
                    unlisted: true,
                    has_changes: false,
                },
                SourceTreeRow::File {
                    entry_index: 2,
                    label: "scratch.txt".to_owned(),
                    depth: 0,
                    status: SourceEntryStatus::Untracked,
                    change: None,
                },
            ]
        );

        // Expanding `src` reveals its file.
        let expanded: HashSet<String> = ["src".to_owned()].into();
        let rows = source_tree_rows(&entries, &expanded);
        assert!(rows.iter().any(|row| matches!(
            row,
            SourceTreeRow::File {
                entry_index: 0,
                depth: 1,
                ..
            }
        )));
    }

    #[test]
    fn lazily_listed_ignored_dir_becomes_expandable_and_children_inherit_dim() {
        let entries = vec![
            entry("target", true, SourceEntryStatus::Ignored),
            entry("target/debug", true, SourceEntryStatus::Ignored),
            entry("target/CACHEDIR.TAG", false, SourceEntryStatus::Ignored),
        ];
        let expanded: HashSet<String> = ["target".to_owned()].into();
        let rows = source_tree_rows(&entries, &expanded);
        assert_eq!(
            rows,
            vec![
                // Children exist now, so `target` is a real (dimmed) dir.
                SourceTreeRow::Dir {
                    label: "target".to_owned(),
                    path: "target".to_owned(),
                    depth: 0,
                    collapsed: false,
                    ignored: true,
                    unlisted: false,
                    has_changes: false,
                },
                // The nested marker is still unenumerated.
                SourceTreeRow::Dir {
                    label: "debug".to_owned(),
                    path: "target/debug".to_owned(),
                    depth: 1,
                    collapsed: true,
                    ignored: true,
                    unlisted: true,
                    has_changes: false,
                },
                SourceTreeRow::File {
                    entry_index: 2,
                    label: "CACHEDIR.TAG".to_owned(),
                    depth: 1,
                    status: SourceEntryStatus::Ignored,
                    change: None,
                },
            ]
        );
    }

    #[test]
    fn expanded_dirs_show_contents_and_chains_compact() {
        let entries = vec![
            entry("src/deep/only/child.rs", false, SourceEntryStatus::Tracked),
            entry("src/main.rs", false, SourceEntryStatus::Tracked),
        ];
        let expanded: HashSet<String> = ["src".to_owned(), "src/deep/only".to_owned()].into();
        let rows = source_tree_rows(&entries, &expanded);
        // `deep/only` compacts into one dir row under `src`, and both files
        // are visible under their expanded parents.
        assert!(rows.iter().any(|row| matches!(
            row,
            SourceTreeRow::Dir { label, path, .. }
                if label == "deep/only" && path == "src/deep/only"
        )));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, SourceTreeRow::File { .. }))
                .count(),
            2
        );

        let rows = source_tree_rows(&entries, &HashSet::new());
        assert_eq!(rows.len(), 1, "collapsed root dir hides everything");
    }

    #[test]
    fn change_status_rides_on_file_rows_and_marks_ancestor_dirs() {
        let mut changed = entry("src/deep/main.rs", false, SourceEntryStatus::Tracked);
        changed.change = Some(DiffFileStatus::Modified);
        let entries = vec![
            changed,
            entry("docs/readme.md", false, SourceEntryStatus::Tracked),
        ];
        let expanded: HashSet<String> = ["src/deep".to_owned()].into();
        let rows = source_tree_rows(&entries, &expanded);
        // Every ancestor of the changed file signals it (the compacted
        // `src/deep` chain row here) — even while collapsed elsewhere; an
        // untouched sibling dir doesn't.
        assert!(rows.iter().any(|row| matches!(
            row,
            SourceTreeRow::Dir { path, has_changes: true, .. } if path == "src/deep"
        )));
        assert!(rows.iter().any(|row| matches!(
            row,
            SourceTreeRow::Dir { path, has_changes: false, .. } if path == "docs"
        )));
        assert!(matches!(
            rows.last(),
            Some(SourceTreeRow::File {
                change: Some(DiffFileStatus::Modified),
                ..
            })
        ));

        // An untracked file marks its ancestors too.
        let entries = vec![entry("notes/todo.txt", false, SourceEntryStatus::Untracked)];
        let rows = source_tree_rows(&entries, &HashSet::new());
        assert!(matches!(
            rows.first(),
            Some(SourceTreeRow::Dir {
                has_changes: true,
                ..
            })
        ));
    }

    #[test]
    fn classify_detects_binary_and_size() {
        let text = classify_disk_bytes(b"hello\nworld\n".to_vec());
        assert_eq!(text.content.as_deref(), Some("hello\nworld\n"));
        let binary = classify_disk_bytes(vec![0u8, 159, 146, 150]);
        assert!(binary.binary && binary.content.is_none());
        let huge = classify_disk_bytes(vec![b'a'; MAX_SOURCE_FILE_BYTES + 1]);
        assert!(huge.too_large && huge.content.is_none());
    }
}
