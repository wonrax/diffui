//! Filesystem watcher for a repository checkout.
//!
//! Classifies raw `notify` events into working-tree edits vs `.jj/repo/op_heads`
//! writes, debounces bursts, and hands the frontend coalesced [`WatchBatch`]es to
//! turn into refreshes. The rest of `.git`/`.jj` is ignored: watching it would
//! feed our own snapshot writes back as a refresh loop and bury real edits under
//! VCS churn. `op_heads` is the one exception, and an op-id dedup (the
//! frontend's job) is what keeps it loop-free.
//!
//! Gated behind the `watcher` feature so a headless / `--no-default-features`
//! build — or a frontend that supplies its own change source — drops the
//! `notify` dependency entirely.

use std::path::Path;
use std::time::Duration;

use notify::Watcher as _;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// How long the tree must go quiet before a burst of fs events is flushed as one
/// batch. Collapses an op's create+remove `op_heads` pair, and editor
/// save-storms, into a single refresh.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// One kind of change seen in a debounce window. A single raw event is only ever
/// one or the other: an op-head write lives entirely under `.jj`, so it never
/// also looks like a worktree edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchSignal {
    Worktree,
    OpLog,
}

/// The kinds of change that arrived during one debounce window. Both flags can
/// be set when a worktree edit and an operation land together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchBatch {
    /// A working-tree file changed (anything outside `.git`/`.jj`): the frontend
    /// should snapshot the working copy and reload @'s diff.
    pub worktree: bool,
    /// A write under `.jj/repo/op_heads` — an operation landed. The frontend
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

/// A live recursive watch over a repository root. Holds the `notify` watcher for
/// its lifetime — drop it to stop watching. Pull coalesced changes with
/// [`RepoWatcher::next_batch`].
pub struct RepoWatcher {
    // Held only to keep the watch alive; the handler talks over `rx`.
    _watcher: notify::RecommendedWatcher,
    rx: UnboundedReceiver<WatchSignal>,
}

impl RepoWatcher {
    /// Begin watching `root` recursively. `notify`'s handler runs on its own
    /// thread and bridges classified signals over an unbounded channel so it
    /// never blocks. Returns the `notify` error if the platform backend can't
    /// initialize or the path can't be watched.
    pub fn start(root: &Path) -> notify::Result<Self> {
        let (tx, rx) = unbounded_channel::<WatchSignal>();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result
                    && !matches!(event.kind, notify::EventKind::Access(_))
                    && let Some(signal) = classify_event(&event)
                {
                    // The receiver only goes away when the stream is dropped, at
                    // which point we no longer care about the send.
                    let _ = tx.send(signal);
                }
            })?;
        watcher.watch(root, notify::RecursiveMode::Recursive)?;
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

/// Classify a raw `notify` event: a worktree edit (any path outside `.git`/
/// `.jj`), an `.jj/repo/op_heads` write, or `None` (other VCS-internal churn we
/// ignore). Worktree is checked first; an op-head write is entirely under `.jj`,
/// so the two are mutually exclusive for a single event.
fn classify_event(event: &notify::Event) -> Option<WatchSignal> {
    if event_touches_worktree(event) {
        Some(WatchSignal::Worktree)
    } else if event_touches_op_log(event) {
        Some(WatchSignal::OpLog)
    } else {
        None
    }
}

/// Whether any path in `event` lies outside `.git` / `.jj` — i.e. it's a
/// working-tree change we should refresh on, rather than VCS-internal churn.
fn event_touches_worktree(event: &notify::Event) -> bool {
    event.paths.iter().any(|path| {
        !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Normal(name) if name == ".git" || name == ".jj"
            )
        })
    })
}

/// Whether any path in `event` is under `.jj/repo/op_heads` — i.e. an operation
/// landed (a head file was added/removed). Matches the three dir names as a
/// consecutive run so a stray `op_heads` component elsewhere can't trip it.
fn event_touches_op_log(event: &notify::Event) -> bool {
    event.paths.iter().any(|path| {
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
    use std::path::PathBuf;

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
            classify_event(&event("/repo/src/main.rs")),
            Some(WatchSignal::Worktree)
        );
    }

    #[test]
    fn op_head_write_classifies_as_op_log() {
        assert_eq!(
            classify_event(&event("/repo/.jj/repo/op_heads/heads/abc123")),
            Some(WatchSignal::OpLog)
        );
    }

    #[test]
    fn other_jj_internal_churn_is_ignored() {
        // A write under `.jj` that isn't an op-head (e.g. the working-copy state
        // file) is neither a worktree edit nor an op landing.
        assert_eq!(
            classify_event(&event("/repo/.jj/working_copy/tree_state")),
            None
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
