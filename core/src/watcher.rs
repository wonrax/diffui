//! Filesystem watcher for a repository checkout.
//!
//! Classifies raw `notify` events into working-tree edits vs jj op-head
//! writes, debounces bursts, and hands the frontend coalesced [`WatchBatch`]es to
//! turn into refreshes. The rest of `.git`/`.jj` is ignored: watching it would
//! feed our own snapshot writes back as a refresh loop and bury real edits under
//! VCS churn. `op_heads` is the one exception, and an op-id dedup (the
//! frontend's job) is what keeps it loop-free.
//!
//! A secondary jj workspace (`jj workspace add`) keeps its op log in the
//! *primary* repo's `.jj`, outside the workspace root — so the watch resolves
//! the `.jj/repo` pointer and additionally watches that directory when it lies
//! elsewhere, and everything under it is classified as VCS-internal rather
//! than worktree edits.
//!
//! Gated behind the `watcher` feature so a headless / `--no-default-features`
//! build — or a frontend that supplies its own change source — drops the
//! `notify` dependency entirely.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// How long the tree must go quiet before a burst of fs events is flushed as one
/// batch. Collapses an op's create+remove `op_heads` pair, and editor
/// save-storms, into a single refresh.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// One kind of change seen in a debounce window. A single raw event is only ever
/// one or the other: an op-head write lives entirely under the repo dir, so it
/// never also looks like a worktree edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchSignal {
    Worktree,
    OpLog,
}

/// The kinds of change that arrived during one debounce window. Both flags can
/// be set when a worktree edit and an operation land together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchBatch {
    /// A working-tree file changed (anything outside `.git`/`.jj` and outside
    /// the resolved repo dir): the frontend should snapshot the working copy
    /// and reload @'s diff.
    pub worktree: bool,
    /// A write under the repo's `op_heads` — an operation landed. The frontend
    /// should read the op head and reload only if it differs from the one it
    /// already reflects (so its own writes don't trigger a redundant walk).
    pub op_log: bool,
}

impl WatchBatch {
    pub fn is_empty(&self) -> bool {
        !self.worktree && !self.op_log
    }

    fn apply(&mut self, signal: WatchSignal) {
        match signal {
            WatchSignal::Worktree => self.worktree = true,
            WatchSignal::OpLog => self.op_log = true,
        }
    }
}

/// Where a repository's watch-relevant pieces live, resolved once at watch
/// start so per-event classification is prefix checks.
#[derive(Debug, Clone, Default)]
struct WatchTargets {
    /// The resolved jj repo dir when it lies *outside* the workspace root — a
    /// secondary workspace's primary repo. Watched in addition to the root,
    /// and excluded from "worktree" classification (its path need not contain
    /// a `.jj` component).
    external_repo_dir: Option<PathBuf>,
    /// `<repo>/op_heads` — writes under it signal "an operation landed".
    op_heads_dir: Option<PathBuf>,
}

impl WatchTargets {
    fn resolve(root: &Path) -> Self {
        // Best-effort: a git repo (or an unreadable pointer) simply keeps the
        // component-based classification below.
        let Ok(repo_dir) = crate::repository::resolve_jj_repo_dir(root) else {
            return Self::default();
        };
        Self {
            external_repo_dir: (!repo_dir.starts_with(root)).then(|| repo_dir.clone()),
            op_heads_dir: Some(repo_dir.join("op_heads")),
        }
    }
}

/// A live recursive watch over a repository root. Holds the `notify` watcher for
/// its lifetime — drop it to stop watching. Pull coalesced changes with
/// [`RepoWatcher::next_batch`].
pub struct RepoWatcher {
    // Held only to keep the watch alive; the handler talks over `rx`.
    _watcher: notify::RecommendedWatcher,
    rx: UnboundedReceiver<WatchSignal>,
}

impl RepoWatcher {
    /// Begin watching `root` recursively — plus the primary repo dir when
    /// `root` is a secondary jj workspace. `notify`'s handler runs on its own
    /// thread and bridges classified signals over an unbounded channel so it
    /// never blocks. Returns the `notify` error if the platform backend can't
    /// initialize or the path can't be watched.
    pub fn start(root: &Path) -> notify::Result<Self> {
        let targets = WatchTargets::resolve(root);
        let external_repo_dir = targets.external_repo_dir.clone();
        let (tx, rx) = unbounded_channel::<WatchSignal>();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result
                    && !matches!(event.kind, notify::EventKind::Access(_))
                    && let Some(signal) = classify_event(&event, &targets)
                {
                    // The receiver only goes away when the stream is dropped, at
                    // which point we no longer care about the send.
                    let _ = tx.send(signal);
                }
            })?;
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
        if let Some(repo_dir) = &external_repo_dir {
            watcher.watch(repo_dir, notify::RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Await the next coalesced change: block for the first raw signal, then keep
    /// draining until the tree is quiet for [`WATCH_DEBOUNCE`], folding every
    /// kind seen into one [`WatchBatch`]. Returns `None` once the watcher handler
    /// has been dropped (the watch ended).
    pub async fn next_batch(&mut self) -> Option<WatchBatch> {
        let first = self.rx.recv().await?;
        let mut batch = WatchBatch::default();
        batch.apply(first);
        while let Ok(signal) = tokio::time::timeout(WATCH_DEBOUNCE, self.rx.recv()).await {
            match signal {
                Some(signal) => batch.apply(signal),
                None => break,
            }
        }
        Some(batch)
    }
}

/// Classify a raw `notify` event: an op-head write (a path under the resolved
/// `op_heads` dir, or the `.jj/repo/op_heads` component pattern), a worktree
/// edit (any path outside `.git`/`.jj` and outside the external repo dir), or
/// `None` (other VCS-internal churn we ignore). Op-log is checked first: it is
/// the more specific signal, and for an external (primary-repo) op store its
/// path may not contain a `.jj` component at all.
fn classify_event(event: &notify::Event, targets: &WatchTargets) -> Option<WatchSignal> {
    if event_touches_op_log(event, targets) {
        Some(WatchSignal::OpLog)
    } else if event_touches_worktree(event, targets) {
        Some(WatchSignal::Worktree)
    } else {
        None
    }
}

/// Whether any path in `event` lies outside `.git` / `.jj` and outside the
/// external repo dir — i.e. it's a working-tree change we should refresh on,
/// rather than VCS-internal churn.
fn event_touches_worktree(event: &notify::Event, targets: &WatchTargets) -> bool {
    event.paths.iter().any(|path| {
        if let Some(repo_dir) = &targets.external_repo_dir
            && path.starts_with(repo_dir)
        {
            return false;
        }
        !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Normal(name) if name == ".git" || name == ".jj"
            )
        })
    })
}

/// Whether any path in `event` is under the repo's `op_heads` — i.e. an
/// operation landed (a head file was added/removed). The resolved-prefix check
/// covers both the in-root repo and a secondary workspace's primary repo; the
/// `.jj/repo/op_heads` component window remains as the fallback when
/// resolution failed, matching the three names as a consecutive run so a stray
/// `op_heads` component elsewhere can't trip it.
fn event_touches_op_log(event: &notify::Event, targets: &WatchTargets) -> bool {
    event.paths.iter().any(|path| {
        if let Some(op_heads) = &targets.op_heads_dir
            && path.starts_with(op_heads)
        {
            return true;
        }
        let names: Vec<_> = path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        names
            .windows(3)
            .any(|w| w[0] == ".jj" && w[1] == "repo" && w[2] == "op_heads")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::{Event, EventKind};

    fn event(path: &str) -> Event {
        Event {
            kind: EventKind::Any,
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        }
    }

    #[test]
    fn worktree_edit_classifies_as_worktree() {
        assert_eq!(
            classify_event(&event("/repo/src/main.rs"), &WatchTargets::default()),
            Some(WatchSignal::Worktree)
        );
    }

    #[test]
    fn op_head_write_classifies_as_op_log() {
        assert_eq!(
            classify_event(
                &event("/repo/.jj/repo/op_heads/heads/abc123"),
                &WatchTargets::default()
            ),
            Some(WatchSignal::OpLog)
        );
    }

    #[test]
    fn other_jj_internal_churn_is_ignored() {
        // A write under `.jj` that isn't an op-head (e.g. the working-copy state
        // file) is neither a worktree edit nor an op landing.
        assert_eq!(
            classify_event(
                &event("/repo/.jj/working_copy/tree_state"),
                &WatchTargets::default()
            ),
            None
        );
    }

    #[test]
    fn external_repo_dir_is_not_worktree_and_its_op_heads_signal() {
        // A secondary workspace: the primary repo lives elsewhere, possibly at
        // a path with no `.jj` component. Its op_heads writes must signal, and
        // the rest of its internals must not read as worktree edits.
        let targets = WatchTargets {
            external_repo_dir: Some(PathBuf::from("/elsewhere/store")),
            op_heads_dir: Some(PathBuf::from("/elsewhere/store/op_heads")),
        };
        assert_eq!(
            classify_event(&event("/elsewhere/store/op_heads/heads/abc"), &targets),
            Some(WatchSignal::OpLog)
        );
        assert_eq!(
            classify_event(&event("/elsewhere/store/index/segments"), &targets),
            None
        );
        // The workspace's own tree still classifies as worktree.
        assert_eq!(
            classify_event(&event("/workspace/src/main.rs"), &targets),
            Some(WatchSignal::Worktree)
        );
    }

    #[test]
    fn batch_folds_both_kinds() {
        let mut batch = WatchBatch::default();
        assert!(batch.is_empty());
        batch.apply(WatchSignal::OpLog);
        batch.apply(WatchSignal::Worktree);
        assert!(!batch.is_empty());
        assert!(batch.worktree && batch.op_log);
    }
}
