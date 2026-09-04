//! The filesystem watcher against a real directory.
//!
//! The unit tests in `watcher.rs` cover classification and debounce timing in
//! isolation; this is the part only a real tree can show — that the watches
//! actually land where the classification says they should.
//!
//! Both watch shapes run here. The per-directory one is what inotify gets; the
//! single recursive one is what FSEvents and ReadDirectoryChangesW get, and it
//! works over inotify too, so it can be exercised somewhere tests run rather
//! than only on the machine it ships to.

mod harness;

use std::time::Duration;

use diffui_core::watcher::RepoWatcher;

/// Longest a batch may take to arrive. Generous: the assertion is about
/// whether the watch reports at all, never about how quickly.
const PATIENCE: Duration = Duration::from_secs(5);

/// Long enough for a batch to have arrived if one was ever coming — the
/// debounce window plus room for the notify thread.
const QUIET: Duration = Duration::from_millis(1500);

/// `(label, per_directory)` for the two shapes [`RepoWatcher::start_with`]
/// takes.
const SHAPES: [(&str, bool); 2] = [("per-directory", true), ("recursive", false)];

async fn next_batch(watcher: &mut RepoWatcher, shape: &str) -> diffui_core::watcher::WatchBatch {
    tokio::time::timeout(PATIENCE, watcher.next_batch())
        .await
        .unwrap_or_else(|_| panic!("{shape}: the watch should have reported by now"))
        .unwrap_or_else(|| panic!("{shape}: the watch ended early"))
}

/// A build writing into an ignored directory must not wake the watch: each
/// batch it produces costs a working-copy snapshot under the repository lock,
/// for a tree jj would not have looked at.
#[test]
fn an_ignored_tree_is_silent_while_the_worktree_reports() {
    for (shape, per_directory) in SHAPES {
        let root = harness::scratch_repo(&format!("watch-ignored-{shape}"));
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::create_dir_all(root.join("target/debug")).expect("mkdir target");
        harness::write(&root, ".gitignore", "/target/\n");

        harness::block_on(async {
            let mut watcher =
                RepoWatcher::start_with(&root, per_directory).expect("start the watch");

            std::fs::write(root.join("target/debug/x.o"), "junk").expect("write into target");
            let quiet = tokio::time::timeout(QUIET, watcher.next_batch()).await;
            assert!(
                quiet.is_err(),
                "{shape}: an ignored write must not produce a batch, got {quiet:?}"
            );

            harness::write(&root, "src/main.rs", "fn main() {}\n");
            assert!(next_batch(&mut watcher, shape).await.worktree);
        });
    }
}

/// A directory created after the watch started has to be picked up — under a
/// per-directory watch that means a watch of its own, or every file ever added
/// under a new module goes unnoticed for the life of the session.
#[test]
fn a_directory_created_after_the_watch_starts_is_picked_up() {
    for (shape, per_directory) in SHAPES {
        let root = harness::scratch_repo(&format!("watch-new-dir-{shape}"));
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");

        harness::block_on(async {
            let mut watcher =
                RepoWatcher::start_with(&root, per_directory).expect("start the watch");

            std::fs::create_dir_all(root.join("src/deep")).expect("mkdir src/deep");
            assert!(next_batch(&mut watcher, shape).await.worktree);

            harness::write(&root, "src/deep/new.rs", "fn new() {}\n");
            assert!(next_batch(&mut watcher, shape).await.worktree);
        });
    }
}

/// An operation landing is the signal that matters for anything done through
/// jj, and under a recursive root watch it arrives through the root's own
/// stream rather than a second registration.
#[test]
fn an_operation_reports_as_an_op_log_write() {
    for (shape, per_directory) in SHAPES {
        let root = harness::scratch_repo(&format!("watch-op-log-{shape}"));

        let head = harness::commit_ids(&root)
            .first()
            .expect("the repo has a commit")
            .clone();

        harness::block_on(async {
            let mut watcher =
                RepoWatcher::start_with(&root, per_directory).expect("start the watch");

            // A bookmark move writes an operation and touches nothing on disk,
            // so the batch it produces can only have come from `op_heads`. On
            // its own thread because the harness drives jj-lib with a runtime
            // of its own, which cannot be started from inside this one.
            let write_root = root.clone();
            std::thread::spawn(move || {
                harness::set_bookmark(&write_root, harness::OpBase::Head, "watched", &head);
            })
            .join()
            .expect("the bookmark write finished");

            let batch = next_batch(&mut watcher, shape).await;
            assert!(batch.op_log, "{shape}: {batch:?}");
        });
    }
}
