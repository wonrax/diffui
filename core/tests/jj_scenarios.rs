//! End-to-end jj scenarios against scratch repos built with the `jj` CLI.
//!
//! Ignored by default: they shell out to `jj` (must be on PATH) and build
//! repos under the system temp dir. Run with:
//!
//! ```sh
//! cargo test -p diffui-core --test jj_scenarios -- --ignored
//! ```
//!
//! Each test rebuilds its repo from scratch, so reruns are deterministic; the
//! repos are left behind afterwards for inspection.

use std::path::{Path, PathBuf};
use std::process::Command;

use diffui_core::jj::{
    load_jj_commits, load_jj_diff, load_jj_repository_snapshot, read_jj_op_head,
};
use diffui_core::{DiffFileStatus, DiffLineKind, LoadProgress, Repository, RevisionSelection, Vcs};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(future)
}

/// Run `jj` in `dir`, panicking (with stderr) on failure; returns stdout.
fn jj(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .current_dir(dir)
        .args(args)
        .env("JJ_USER", "Scenario Test")
        .env("JJ_EMAIL", "scenario@example.com")
        .output()
        .expect("jj CLI must be on PATH for these scenarios");
    assert!(
        output.status.success(),
        "jj {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write scratch file");
}

/// A fresh scratch jj repo at a deterministic per-test path.
fn scratch_repo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("diffui-core-scenario-{test}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch dir");
    jj(&root, &["git", "init"]);
    root
}

fn repository(root: &Path) -> Repository {
    Repository {
        root: root.to_owned(),
        vcs: Vcs::Jj,
        scope: PathBuf::new(),
    }
}

fn change_id(dir: &Path, revset: &str) -> String {
    jj(dir, &["log", "--no-graph", "-r", revset, "-T", "change_id"])
}

fn commit_id(dir: &Path, revset: &str) -> String {
    jj(dir, &["log", "--no-graph", "-r", revset, "-T", "commit_id"])
}

/// base → sideA / sideB (both rewrite the same line) → `@` = conflicted merge.
fn build_conflicted_merge(root: &Path) {
    write(root, "file.txt", "line1\nline2\nline3\n");
    jj(root, &["commit", "-m", "base"]);
    let base = change_id(root, "@-");
    write(root, "file.txt", "line1\nSIDE-A\nline3\n");
    jj(root, &["commit", "-m", "sideA"]);
    let side_a = change_id(root, "@-");
    jj(root, &["new", &base]);
    write(root, "file.txt", "line1\nSIDE-B\nline3\n");
    jj(root, &["commit", "-m", "sideB"]);
    let side_b = change_id(root, "@-");
    jj(
        root,
        &["new", &side_a, &side_b, "-m", "merge with conflict"],
    );
}

/// A conflicted merge's tree equals the merge of its parents, so the
/// parent-tree diff streams nothing — the loader must synthesize `Conflicted`
/// entries with the materialized conflict hunks instead of showing an empty
/// file list.
#[test]
#[ignore = "shells out to the jj CLI"]
fn conflicted_merge_diff_lists_conflict_files() {
    let root = scratch_repo("conflicted-merge");
    build_conflicted_merge(&root);

    let (document, _details) = block_on(load_jj_diff(
        repository(&root),
        RevisionSelection::WorkingCopy,
    ))
    .expect("load conflicted merge diff");

    assert_eq!(
        document.files.len(),
        1,
        "the conflicted path must be listed"
    );
    let file = &document.files[0];
    assert_eq!(file.path, "file.txt");
    assert_eq!(file.status, DiffFileStatus::Conflicted);
    assert!(!file.hunks.is_empty(), "conflict content must be shown");
    let lines = &file.hunks[0].lines;
    assert!(
        lines.iter().any(|l| l.kind == DiffLineKind::Conflict),
        "marker lines render as Conflict: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.content.contains("SIDE-B")),
        "a conflict side's content is visible"
    );
}

/// Two concurrent `describe`s of the same leaf change make it divergent (two
/// visible commits, one change id) — both rows must carry the flag.
#[test]
#[ignore = "shells out to the jj CLI"]
fn divergent_change_is_flagged_on_all_its_commits() {
    let root = scratch_repo("divergent");
    write(&root, "file.txt", "hello\n");
    jj(&root, &["commit", "-m", "base"]);
    // A childless leaf: describing a commit with descendants would rebase
    // them on both sides of the concurrent ops, making their changes
    // (correctly) divergent too and muddying the assertion below.
    jj(&root, &["new", "-r", "@-", "-m", "leaf", "--no-edit"]);
    let leaf = change_id(&root, "description(glob:\"leaf*\")");
    jj(&root, &["describe", "-r", &leaf, "-m", "leaf d1"]);
    jj(
        &root,
        &["--at-op", "@-", "describe", "-r", &leaf, "-m", "leaf d2"],
    );
    // Any subsequent command reconciles the concurrent ops into divergence.
    jj(&root, &["log", "-r", "all()"]);

    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        root.clone(),
        "all()".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load commits");

    let divergent: Vec<_> = store.iter().filter(|row| row.change_id() == leaf).collect();
    assert_eq!(divergent.len(), 2, "both sides of the divergence are shown");
    assert!(
        divergent.iter().all(|row| row.is_divergent()),
        "divergent rows must be flagged"
    );
    assert!(
        store
            .iter()
            .filter(|row| row.change_id() != leaf)
            .all(|row| !row.is_divergent()),
        "non-divergent rows must not be flagged"
    );

    // Every visible copy carries its change offset (jj log's `xyz/N` suffix);
    // the untouched rows carry none.
    let mut offsets: Vec<_> = divergent
        .iter()
        .map(|row| row.change_offset().expect("divergent copies have offsets"))
        .collect();
    offsets.sort_unstable();
    assert_eq!(offsets, vec![0, 1], "copies are addressed as xyz/0, xyz/1");
    assert!(
        store
            .iter()
            .filter(|row| row.change_id() != leaf)
            .all(|row| row.change_offset().is_none()),
        "single-copy changes carry no offset"
    );

    // The suffix is a real revset symbol: `changeid/N` loads exactly the copy
    // the offset was reported for.
    for row in &divergent {
        let symbol = format!("{leaf}/{}", row.change_offset().expect("offset"));
        let expected = row.commit_id().to_owned();
        let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
            root.clone(),
            symbol.clone(),
            LoadProgress::default(),
        ))
        .unwrap_or_else(|err| panic!("load revset {symbol}: {err:?}"));
        assert_eq!(store.len(), 1, "{symbol} selects a single revision");
        assert_eq!(store.row(0).commit_id(), expected, "{symbol} picks its copy");
    }
}

/// A rewritten commit stays loadable when a ref (here: an explicit commit id
/// in the revset, like a stale remote bookmark would) pins it into the graph.
/// The hidden copy must be flagged and carry the `changeid/N` offset jj log
/// shows — while the surviving visible copy stays a plain, suffix-less id.
#[test]
#[ignore = "shells out to the jj CLI"]
fn hidden_copy_is_flagged_with_its_change_offset() {
    let root = scratch_repo("hidden-offset");
    write(&root, "file.txt", "hello\n");
    jj(&root, &["commit", "-m", "one"]);
    let change = change_id(&root, "@-");
    let old_commit = commit_id(&root, "@-");
    // Rewrite the commit: the change id keeps pointing at the new commit,
    // the old one becomes hidden.
    jj(&root, &["describe", "-r", &change, "-m", "one v2"]);
    let new_commit = commit_id(&root, "@-");
    assert_ne!(old_commit, new_commit, "describe must rewrite the commit");

    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        root.clone(),
        format!("{old_commit} | all()"),
        LoadProgress::default(),
    ))
    .expect("load commits with the hidden copy pinned");

    let hidden = store
        .iter()
        .find(|row| row.commit_id() == old_commit)
        .expect("hidden copy is in the graph");
    assert!(hidden.is_hidden(), "the old copy must be flagged hidden");
    assert!(
        !hidden.is_divergent(),
        "one visible copy ⇒ the change is not divergent"
    );
    let offset = hidden.change_offset().expect("hidden copy has an offset");
    assert!(offset > 0, "the visible copy owns offset 0");

    let visible = store
        .iter()
        .find(|row| row.commit_id() == new_commit)
        .expect("visible copy is in the graph");
    assert!(!visible.is_hidden() && !visible.is_divergent());
    assert_eq!(
        visible.change_offset(),
        None,
        "a lone visible copy renders without a suffix, like jj log"
    );

    // The displayed suffix addresses the hidden copy in a revset.
    let symbol = format!("{change}/{offset}");
    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        root.clone(),
        symbol.clone(),
        LoadProgress::default(),
    ))
    .unwrap_or_else(|err| panic!("load revset {symbol}: {err:?}"));
    assert_eq!(store.len(), 1);
    assert_eq!(store.row(0).commit_id(), old_commit);
    assert!(store.row(0).is_hidden());
}

/// Opening a secondary workspace (`jj workspace add`) must resolve `@` to
/// *that workspace's* working copy, read op heads through the `.jj/repo`
/// pointer file, and label the other workspace's working copy `name@` in the
/// primary view.
#[test]
#[ignore = "shells out to the jj CLI"]
fn secondary_workspace_resolves_and_labels() {
    let root = scratch_repo("workspace");
    write(&root, "file.txt", "hello\n");
    jj(&root, &["commit", "-m", "base"]);
    jj(
        &root,
        &[
            "workspace",
            "add",
            "../diffui-core-scenario-workspace-second",
        ],
    );
    let second = root
        .parent()
        .expect("scratch parent")
        .join("diffui-core-scenario-workspace-second");

    // The op-head read used to require `.jj/repo` to be a directory; in a
    // secondary workspace it's a pointer file.
    let head = block_on(read_jj_op_head(repository(&second))).expect("op head via pointer file");
    assert!(!head.is_empty());

    // `@` in the secondary workspace is its own working copy, not the
    // default workspace's.
    let ws_wc = commit_id(&second, "@");
    let default_wc = commit_id(&root, "@");
    assert_ne!(ws_wc, default_wc, "scenario needs distinct working copies");
    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        second.clone(),
        "@".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load @ in secondary workspace");
    assert_eq!(store.len(), 1);
    assert_eq!(store.row(0).commit_id(), ws_wc);
    assert!(store.row(0).is_working_copy());

    // From the primary workspace, the secondary's working copy carries a
    // `name@` chip (and is not marked as *the* working copy).
    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        root.clone(),
        "all()".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load all() in primary workspace");
    let ws_row = store
        .iter()
        .find(|row| row.commit_id() == ws_wc)
        .expect("secondary workspace's wc is in the graph");
    assert!(!ws_row.is_working_copy());
    assert!(
        ws_row
            .bookmarks()
            .iter()
            .any(|label| label == "diffui-core-scenario-workspace-second@"),
        "expected workspace chip, got {:?}",
        ws_row.bookmarks()
    );
}

/// The snapshot must expose the op it was based on, so a frontend can tell
/// "our own snapshot advanced the head" from "a CLI op landed in between" —
/// the latter escalates a diff-only watcher refresh to a full graph reload.
#[test]
#[ignore = "shells out to the jj CLI"]
fn snapshot_parent_fingerprint_detects_external_ops() {
    let root = scratch_repo("external-op");
    write(&root, "file.txt", "hello\n");
    jj(&root, &["commit", "-m", "base"]);

    // Quiet tree: the snapshot writes no op and is its own base.
    let (first, _repo, _wc, _ws) =
        block_on(load_jj_repository_snapshot(repository(&root))).expect("first snapshot");
    assert_eq!(
        first.parent_fingerprint.as_deref(),
        Some(first.fingerprint.as_str()),
        "no-op snapshot is its own parent"
    );

    // An external op (CLI `jj new`) plus a worktree edit in the same window —
    // the case that used to be swallowed as a diff-only refresh.
    jj(&root, &["new", "-m", "external op"]);
    write(&root, "file.txt", "hello edited\n");

    let (second, _repo, _wc, _ws) =
        block_on(load_jj_repository_snapshot(repository(&root))).expect("second snapshot");
    assert_ne!(
        second.parent_fingerprint.as_deref(),
        Some(first.fingerprint.as_str()),
        "the CLI op must show up as an unexpected parent (escalation trigger)"
    );
    assert_ne!(
        second.parent_fingerprint.as_deref(),
        Some(second.fingerprint.as_str()),
        "the edit forces a snapshot op, so the parent is the CLI op"
    );

    // And the on-disk head is exactly what the snapshot recorded, so the
    // op-log dedup comparison holds.
    let head = block_on(read_jj_op_head(repository(&root))).expect("read op head");
    assert_eq!(head, second.fingerprint);
}
