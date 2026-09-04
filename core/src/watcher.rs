//! Filesystem watcher for a repository checkout.
//!
//! Classifies raw `notify` events into working-tree edits vs jj op-head
//! writes, debounces bursts, and hands the frontend coalesced [`WatchBatch`]es to
//! turn into refreshes. The rest of `.git`/`.jj` is ignored: watching it would
//! feed our own snapshot writes back as a refresh loop and bury real edits under
//! VCS churn. `op_heads` is the one exception, and an op-id dedup (the
//! frontend's job) is what keeps it loop-free.
//!
//! The watch skips what the repository's ignore rules skip: a write a build
//! makes under `target/` or `node_modules/` would otherwise be a worktree
//! signal, and each of those costs a working-copy snapshot under the repository
//! lock for a tree jj would not have looked at anyway. How the skipping is done
//! depends on the platform backend — see [`WATCH_PER_DIRECTORY`].
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

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jj_lib::gitignore::GitIgnoreFile;
use notify::Watcher as _;
use tokio::sync::mpsc::{Receiver, channel};

/// How long the tree must go quiet before a burst of fs events is flushed as one
/// batch. Collapses an op's create+remove `op_heads` pair, and editor
/// save-storms, into a single refresh.
pub const WATCH_DEBOUNCE: Duration = Duration::from_millis(400);

/// Longest a batch may be held open. A trailing-edge debounce alone never fires
/// while writes keep arriving, so a long `cargo build` — or a formatter
/// sweeping the tree — would leave the sidebar stale for its whole duration.
/// Past this the batch flushes whether or not the tree has gone quiet.
pub const WATCH_MAX_LATENCY: Duration = Duration::from_secs(2);

/// Whether to register one watch per directory, or one recursive watch over the
/// root and let classification do the filtering.
///
/// inotify has no recursive watch of its own: `notify` walks the tree and adds
/// a descriptor per directory either way, so adding them ourselves — minus the
/// ignored ones — is strictly less work, strictly fewer descriptors, and the
/// difference between hitting `max_user_watches` on a big checkout and not.
///
/// FSEvents and ReadDirectoryChangesW are the other way round. They watch a
/// subtree natively, and `notify`'s FSEvents backend rebuilds its entire event
/// stream on every `watch()` call: stop the run loop, join its thread, append
/// the path, create a new stream starting from *now*. One watch per directory
/// on a checkout the size of nixpkgs is then tens of thousands of stream
/// restarts before the window has finished opening, with events lost in each
/// gap — op-head writes included, since they share the stream — and a linear
/// scan of the registered paths on every event after that. There we take the
/// one recursive watch the backend wants and let [`WatchScope::classify`] drop
/// the ignored paths, which it does regardless. The survey still runs on both:
/// the ignore matcher it builds is what classification reads.
const WATCH_PER_DIRECTORY: bool = cfg!(target_os = "linux");

/// Depth of the queue between the `notify` handler thread and the watch task.
/// Bounded on purpose: the handler blocks rather than letting a build storm
/// buffer a million raw events that all fold into the same one-bit batch.
const SIGNAL_QUEUE: usize = 256;

/// One kind of change seen in a debounce window. A single raw event is only ever
/// one or the other: an op-head write lives entirely under the repo dir, so it
/// never also looks like a worktree edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchSignal {
    Worktree,
    OpLog,
}

/// What the `notify` handler forwards to the watch task.
enum WatchEvent {
    Signal(WatchSignal),
    /// A directory appeared, under a [per-directory](WATCH_PER_DIRECTORY)
    /// watch. It needs a watch of its own, and only the task holding the
    /// watcher can add one, which is why this travels down the channel rather
    /// than being done in the handler. Never sent under a recursive watch,
    /// where the backend has already picked the directory up.
    NewDirectory(PathBuf),
}

/// The kinds of change that arrived during one debounce window. Both flags can
/// be set when a worktree edit and an operation land together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchBatch {
    /// A working-tree file changed (anything outside `.git`/`.jj`, outside the
    /// resolved repo dir, and not ignored): the frontend should snapshot the
    /// working copy and reload @'s diff.
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
            // Only when it is really there: `resolve_jj_repo_dir` answers with
            // the conventional path for a repository that has no `.jj` at all
            // (a plain git checkout), and watching that fails outright.
            op_heads_dir: Some(repo_dir.join("op_heads")).filter(|dir| dir.is_dir()),
        }
    }
}

/// Everything classification needs, shared with the `notify` handler thread.
struct WatchScope {
    root: PathBuf,
    targets: WatchTargets,
    /// The repository's ignore rules, composed the way the working-copy
    /// snapshot composes them: jj's out-of-tree base ignores, then every
    /// in-tree `.gitignore` the survey found, each chained at its own
    /// directory prefix. Fixed once at start — a `.gitignore` written later is
    /// not picked up until the repository is reopened, which costs a watch on
    /// a directory we needn't have, never a missed edit.
    ignores: Arc<GitIgnoreFile>,
}

impl WatchScope {
    /// Classify one path from a raw event. Op-log is checked first: it is the
    /// more specific signal, and for an external (primary-repo) op store the
    /// path may not contain a `.jj` component at all.
    fn classify(&self, path: &Path, is_dir: bool) -> Option<WatchSignal> {
        if self.touches_op_log(path) {
            return Some(WatchSignal::OpLog);
        }
        if let Some(repo_dir) = &self.targets.external_repo_dir
            && path.starts_with(repo_dir)
        {
            return None;
        }
        // Matched on the path *relative to the root*: a checkout that itself
        // lives under a directory called `.jj` or `.git` — `~/.jj/scratch`, a
        // worktree beside a bare `…/.git` — would otherwise have every one of
        // its files read as VCS internals and never refresh at all.
        let relative = path.strip_prefix(&self.root).ok()?;
        if relative.components().any(|component| {
            matches!(component, Component::Normal(name) if name == ".git" || name == ".jj")
        }) {
            return None;
        }
        if self.is_ignored(relative, is_dir) {
            return None;
        }
        Some(WatchSignal::Worktree)
    }

    /// Whether `relative` is ignored. Anything jj's snapshot would skip is
    /// noise here for the same reason: the refresh it would trigger reads the
    /// same tree and finds nothing changed.
    fn is_ignored(&self, relative: &Path, is_dir: bool) -> bool {
        let Some(text) = relative.to_str() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        if is_dir {
            self.ignores.matches(&format!("{text}/"))
        } else {
            self.ignores.matches(text)
        }
    }

    /// Whether `path` is under the repo's `op_heads` — i.e. an operation landed
    /// (a head file was added or removed). The resolved-prefix check covers
    /// both the in-root repo and a secondary workspace's primary repo; the
    /// `.jj/repo/op_heads` component window remains as the fallback when
    /// resolution failed, matching the three names as a consecutive run so a
    /// stray `op_heads` component elsewhere can't trip it.
    fn touches_op_log(&self, path: &Path) -> bool {
        if let Some(op_heads) = &self.targets.op_heads_dir
            && path.starts_with(op_heads)
        {
            return true;
        }
        let names: Vec<_> = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        names
            .windows(3)
            .any(|w| w[0] == ".jj" && w[1] == "repo" && w[2] == "op_heads")
    }

    /// Whether `dir` is a directory the watch should descend into: inside the
    /// root, not VCS-internal, not ignored.
    fn is_watchable_dir(&self, dir: &Path) -> bool {
        matches!(self.classify(dir, true), Some(WatchSignal::Worktree))
    }
}

/// A live watch over a repository. Holds the `notify` watcher for its lifetime
/// — drop it to stop watching. Pull coalesced changes with
/// [`RepoWatcher::next_batch`].
pub struct RepoWatcher {
    // Declared before the watcher so it drops first: the handler blocks on a
    // full queue, and closing the receiver is what lets that send fail and the
    // handler thread finish, instead of deadlocking the watcher's own drop.
    rx: Receiver<WatchEvent>,
    watcher: notify::RecommendedWatcher,
    scope: Arc<WatchScope>,
}

impl RepoWatcher {
    /// Begin watching `root`, in whichever shape the platform backend wants
    /// (see [`WATCH_PER_DIRECTORY`]), plus the primary repo's `op_heads` when
    /// that lies outside the root. `notify`'s handler runs on its own thread
    /// and forwards classified signals over a bounded channel, blocking when
    /// the task falls behind.
    ///
    /// Returns the `notify` error only if the platform backend can't
    /// initialize, or the root or op-log watch can't be established. Running
    /// out of inotify watches partway through the tree leaves the watch
    /// *degraded*, not off: operations still refresh, which is the signal that
    /// matters for anything done through jj itself.
    pub fn start(root: &Path) -> notify::Result<Self> {
        Self::start_with(root, WATCH_PER_DIRECTORY)
    }

    /// [`start`], with the watch shape named instead of taken from the
    /// platform. Both shapes work over inotify, so this is how the one macOS
    /// takes gets exercised somewhere tests actually run.
    pub fn start_with(root: &Path, per_directory: bool) -> notify::Result<Self> {
        // Classification strips the root off each event path, and the
        // backends report canonical paths: FSEvents resolves symlinks, so a
        // checkout under `/tmp` or `/var` on macOS comes back as `/private/…`
        // and a root left as given would strip nothing and drop every
        // worktree edit. Resolve it once so the prefix is the one the events
        // carry; the op store is already resolved the same way.
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        let root = canonical.as_path();
        let targets = WatchTargets::resolve(root);
        let (ignores, directories) = survey(root, &targets, per_directory);
        let scope = Arc::new(WatchScope {
            root: root.to_owned(),
            targets,
            ignores,
        });

        let (tx, rx) = channel::<WatchEvent>(SIGNAL_QUEUE);
        let handler_scope = scope.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in &event.paths {
                    let is_dir = path.is_dir();
                    if let Some(signal) = handler_scope.classify(path, is_dir) {
                        // A full queue blocks this thread, which is the point:
                        // the kernel's own buffer then applies the back
                        // pressure, rather than us holding an unbounded copy of
                        // a build's worth of events. The send only fails once
                        // the watch task is gone, and then there is nothing to
                        // report to.
                        if tx.blocking_send(WatchEvent::Signal(signal)).is_err() {
                            return;
                        }
                    }
                    if per_directory
                        && is_dir
                        && handler_scope.is_watchable_dir(path)
                        && tx
                            .blocking_send(WatchEvent::NewDirectory(path.clone()))
                            .is_err()
                    {
                        return;
                    }
                }
            })?;

        // The op-log watch first, so it survives a watch-limit failure in the
        // tree walk below. Under a recursive root watch it is only needed when
        // the repo dir lies outside the root, and registering it twice would
        // cost a second FSEvents stream restart for nothing.
        if let Some(op_heads) = &scope.targets.op_heads_dir
            && (per_directory || !op_heads.starts_with(root))
        {
            watcher.watch(op_heads, notify::RecursiveMode::Recursive)?;
        }
        if per_directory {
            watch_directories(&mut watcher, &scope, directories);
        } else {
            // One call, one stream. Everything the survey would have excluded
            // still arrives here and is dropped by `classify`, so an ignored
            // tree costs an event apiece instead of a snapshot apiece.
            watcher.watch(root, notify::RecursiveMode::Recursive)?;
        }

        Ok(Self { rx, watcher, scope })
    }

    /// Await the next coalesced change. Returns `None` once the watcher handler
    /// has been dropped (the watch ended).
    pub async fn next_batch(&mut self) -> Option<WatchBatch> {
        let Self {
            rx, watcher, scope, ..
        } = self;
        debounce(rx, |batch, event| match event {
            WatchEvent::Signal(signal) => batch.apply(signal),
            // A directory that appeared mid-session (a new module, a freshly
            // cloned submodule) is watched from here on. Its creation is a
            // worktree edit too, but the handler classified it as one before
            // sending this, so there is nothing to fold in here.
            WatchEvent::NewDirectory(path) => watch_subtree(watcher, scope, &path),
        })
        .await
    }
}

/// Fold raw events into one batch: wait for the first, then keep draining until
/// the source has been quiet for [`WATCH_DEBOUNCE`] — or until the batch has
/// been open for [`WATCH_MAX_LATENCY`], whichever comes first.
///
/// Split out from the watcher, and generic over what arrives, so the timing can
/// be tested against a plain channel under a paused clock instead of a real
/// filesystem.
async fn debounce<T>(
    rx: &mut Receiver<T>,
    mut fold: impl FnMut(&mut WatchBatch, T),
) -> Option<WatchBatch> {
    let first = rx.recv().await?;
    let mut batch = WatchBatch::default();
    fold(&mut batch, first);
    let ceiling = tokio::time::sleep(WATCH_MAX_LATENCY);
    tokio::pin!(ceiling);
    loop {
        tokio::select! {
            biased;
            event = rx.recv() => match event {
                Some(event) => fold(&mut batch, event),
                // The watch ended; report what we have rather than dropping it.
                None => return Some(batch),
            },
            () = tokio::time::sleep(WATCH_DEBOUNCE) => return Some(batch),
            () = &mut ceiling => return Some(batch),
        }
    }
}

/// Walk `root` once, building the ignore matcher and (under a per-directory
/// watch) the list of directories worth watching.
///
/// They define each other: a directory's `.gitignore` decides whether the
/// directories under it are visited at all, so the two cannot be computed in
/// separate passes. Breadth-first, because chained rules are read innermost
/// first: a deeper `.gitignore` has to be chained after the one above it to win
/// over it, exactly as it does for the snapshot. A recursive watch needs the
/// matcher but not the list, and on a large checkout that list is tens of
/// thousands of paths.
fn survey(
    root: &Path,
    targets: &WatchTargets,
    collect_directories: bool,
) -> (Arc<GitIgnoreFile>, Vec<PathBuf>) {
    // jj's out-of-tree ignores (`core.excludesFile`, `info/exclude`) are the
    // base layer. Failing to read them leaves the watch noisier than it needs
    // to be, which beats not starting it.
    let mut ignores = crate::jj::settings::snapshot_base_ignores(root).unwrap_or_else(|error| {
        tracing::warn!(
            root = %root.display(),
            %error,
            "failed to read the repository's base ignores; the watch will not skip ignored paths"
        );
        GitIgnoreFile::empty()
    });

    let mut directories = Vec::new();
    let mut level = vec![(root.to_owned(), String::new())];
    while !level.is_empty() {
        let mut next = Vec::new();
        for (dir, prefix) in level {
            match ignores.chain_with_file(&prefix, dir.join(".gitignore")) {
                Ok(chained) => ignores = chained,
                Err(error) => tracing::debug!(
                    dir = %dir.display(),
                    %error,
                    "ignoring an unreadable .gitignore"
                ),
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            if collect_directories {
                directories.push(dir);
            }
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let name = entry.file_name();
                if name == ".git" || name == ".jj" {
                    continue;
                }
                let child_prefix = format!("{prefix}{}/", name.to_string_lossy());
                if ignores.matches(&child_prefix)
                    || targets
                        .external_repo_dir
                        .as_ref()
                        .is_some_and(|repo_dir| entry.path().starts_with(repo_dir))
                {
                    continue;
                }
                next.push((entry.path(), child_prefix));
            }
        }
        level = next;
    }
    (ignores, directories)
}

/// Watch each of `directories`, one non-recursive watch apiece.
///
/// Warns and stops when the platform runs out of watch descriptors (inotify's
/// `ENOSPC`, the usual outcome on a large tree with the default
/// `max_user_watches`): the op-log watch is already in place by then, so
/// anything done through jj still refreshes and only unsnapshotted disk edits
/// go unnoticed. Degraded, not off.
fn watch_directories(
    watcher: &mut notify::RecommendedWatcher,
    scope: &WatchScope,
    directories: Vec<PathBuf>,
) {
    for dir in directories {
        if let Err(error) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            if is_watch_limit(&error) {
                tracing::warn!(
                    root = %scope.root.display(),
                    "out of filesystem watches; auto-refresh is limited to jj operations \
                     (raise fs.inotify.max_user_watches to restore it)"
                );
                return;
            }
            // A directory that vanished between listing and watching, or one we
            // can't read, is not worth abandoning the rest of the tree for.
            tracing::debug!(dir = %dir.display(), %error, "failed to watch a directory");
        }
    }
}

/// Watch a directory that appeared after the survey, and everything under it.
/// Its own `.gitignore` isn't chained — that would mean rebuilding the matcher
/// the handler thread is already reading — so a new directory is judged by the
/// rules that were in force when the watch started.
fn watch_subtree(watcher: &mut notify::RecommendedWatcher, scope: &WatchScope, dir: &Path) {
    let mut directories = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        directories.push(dir);
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && scope.is_watchable_dir(&path) {
                pending.push(path);
            }
        }
    }
    watch_directories(watcher, scope, directories);
}

/// Whether `error` is the platform saying it has no watch descriptors left.
fn is_watch_limit(error: &notify::Error) -> bool {
    match &error.kind {
        notify::ErrorKind::Io(io) => io.raw_os_error() == Some(NO_SPACE),
        notify::ErrorKind::MaxFilesWatch => true,
        _ => false,
    }
}

/// `ENOSPC` — what inotify returns once `max_user_watches` is exhausted.
const NO_SPACE: i32 = 28;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::channel;

    fn scope(root: &str) -> WatchScope {
        WatchScope {
            root: PathBuf::from(root),
            targets: WatchTargets::default(),
            ignores: GitIgnoreFile::empty(),
        }
    }

    #[test]
    fn worktree_edit_classifies_as_worktree() {
        assert_eq!(
            scope("/repo").classify(Path::new("/repo/src/main.rs"), false),
            Some(WatchSignal::Worktree)
        );
    }

    #[test]
    fn op_head_write_classifies_as_op_log() {
        assert_eq!(
            scope("/repo").classify(Path::new("/repo/.jj/repo/op_heads/heads/abc123"), false),
            Some(WatchSignal::OpLog)
        );
    }

    #[test]
    fn other_jj_internal_churn_is_ignored() {
        // A write under `.jj` that isn't an op-head (e.g. the working-copy state
        // file) is neither a worktree edit nor an op landing.
        assert_eq!(
            scope("/repo").classify(Path::new("/repo/.jj/working_copy/tree_state"), false),
            None
        );
    }

    /// The `.git`/`.jj` test is on the path *relative to the root*. A checkout
    /// that lives under a directory called `.jj` used to have every file in it
    /// classified as VCS internals, so the tree never refreshed at all.
    #[test]
    fn a_root_nested_under_a_dot_jj_directory_still_reports_its_tree() {
        let scope = scope("/home/dev/.jj/scratch");
        assert_eq!(
            scope.classify(Path::new("/home/dev/.jj/scratch/src/main.rs"), false),
            Some(WatchSignal::Worktree)
        );
        // Its own `.jj` is still internal.
        assert_eq!(
            scope.classify(
                Path::new("/home/dev/.jj/scratch/.jj/working_copy/tree_state"),
                false
            ),
            None
        );
    }

    #[test]
    fn an_ignored_path_is_not_a_worktree_signal() {
        let ignores = GitIgnoreFile::empty()
            .chain("", Path::new("/repo/.gitignore"), b"/target/\n")
            .expect("parse the test ignore rules");
        let scope = WatchScope {
            root: PathBuf::from("/repo"),
            targets: WatchTargets::default(),
            ignores,
        };
        assert_eq!(scope.classify(Path::new("/repo/target"), true), None);
        assert_eq!(
            scope.classify(Path::new("/repo/target/debug/build.rs"), false),
            None
        );
        assert!(!scope.is_watchable_dir(Path::new("/repo/target/debug")));
        assert_eq!(
            scope.classify(Path::new("/repo/src/main.rs"), false),
            Some(WatchSignal::Worktree)
        );
    }

    #[test]
    fn external_repo_dir_is_not_worktree_and_its_op_heads_signal() {
        // A secondary workspace: the primary repo lives elsewhere, possibly at
        // a path with no `.jj` component. Its op_heads writes must signal, and
        // the rest of its internals must not read as worktree edits.
        let scope = WatchScope {
            root: PathBuf::from("/workspace"),
            targets: WatchTargets {
                external_repo_dir: Some(PathBuf::from("/elsewhere/store")),
                op_heads_dir: Some(PathBuf::from("/elsewhere/store/op_heads")),
            },
            ignores: GitIgnoreFile::empty(),
        };
        assert_eq!(
            scope.classify(Path::new("/elsewhere/store/op_heads/heads/abc"), false),
            Some(WatchSignal::OpLog)
        );
        assert_eq!(
            scope.classify(Path::new("/elsewhere/store/index/segments"), false),
            None
        );
        // The workspace's own tree still classifies as worktree.
        assert_eq!(
            scope.classify(Path::new("/workspace/src/main.rs"), false),
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

    #[tokio::test(start_paused = true)]
    async fn a_quiet_tree_flushes_one_debounce_after_the_last_signal() {
        let (tx, mut rx) = channel(8);
        tokio::spawn(async move {
            for step in 0..3 {
                if step > 0 {
                    tokio::time::sleep(WATCH_DEBOUNCE / 4).await;
                }
                tx.send(WatchSignal::Worktree).await.expect("send");
            }
            // Hold the sender: a closed channel flushes immediately, which
            // would prove nothing about the quiet window.
            tokio::time::sleep(WATCH_MAX_LATENCY * 10).await;
        });

        let start = tokio::time::Instant::now();
        let batch = debounce(&mut rx, WatchBatch::apply)
            .await
            .expect("the burst flushes");
        assert!(batch.worktree && !batch.op_log);
        // Three signals a quarter-window apart fold into one batch that lands a
        // full window after the last of them.
        assert_eq!(start.elapsed(), WATCH_DEBOUNCE / 2 + WATCH_DEBOUNCE);
    }

    /// A build writes without pause for as long as it runs. With only a
    /// trailing-edge debounce the batch is never flushed while that lasts, so
    /// the sidebar stays stale for the whole build; the ceiling is what bounds
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_continuous_storm_still_flushes_at_the_ceiling() {
        let (tx, mut rx) = channel(8);
        tokio::spawn(async move {
            while tx.send(WatchSignal::Worktree).await.is_ok() {
                tokio::time::sleep(WATCH_DEBOUNCE / 4).await;
            }
        });

        let start = tokio::time::Instant::now();
        let batch = debounce(&mut rx, WatchBatch::apply)
            .await
            .expect("the ceiling flushes");
        assert!(batch.worktree);
        assert_eq!(start.elapsed(), WATCH_MAX_LATENCY);
    }
}
