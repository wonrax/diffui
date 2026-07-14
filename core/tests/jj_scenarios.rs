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
use diffui_core::{
    Destination, DiffFileStatus, DiffLineKind, LoadProgress, MutationOp, RebaseSourceMode,
    Repository, RevisionSelection, SquashTarget, Vcs, list_ignored_dir, list_source_tree,
    load_source_file,
};
use diffui_core::{SourceEntryStatus, mutations};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(future)
}

/// Run `jj` in `dir`, panicking (with stderr) on failure; returns stdout.
/// Signing is forced off: a user config with e.g. 1Password SSH signing
/// would otherwise prompt (or hang) on every scratch-repo commit.
fn jj(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .current_dir(dir)
        .args(["--config", "signing.behavior=keep"])
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

/// A fresh scratch jj repo at a deterministic per-test path. The repo-level
/// config turns signing off so diffui-core's *in-process* commits (which
/// load the user's config, signing backend included) stay hermetic too.
fn scratch_repo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("diffui-core-scenario-{test}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch dir");
    jj(&root, &["git", "init"]);
    std::fs::write(
        root.join(".jj/repo/config.toml"),
        "[signing]\nbehavior = \"keep\"\n",
    )
    .expect("write scratch repo config");
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

/// Commit ids of `revset`'s parents, one per line, unordered.
fn parent_ids(dir: &Path, revset: &str) -> Vec<String> {
    jj(
        dir,
        &[
            "log",
            "--no-graph",
            "-r",
            &format!("parents({revset})"),
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

fn run(root: &Path, op: MutationOp) -> mutations::MutationOutcome {
    block_on(mutations::run_mutation(
        repository(root),
        op,
        LoadProgress::default(),
    ))
    .expect("mutation succeeds")
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
        assert_eq!(
            store.row(0).commit_id(),
            expected,
            "{symbol} picks its copy"
        );
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

#[test]
#[ignore = "shells out to the jj CLI"]
fn describe_mutation_replaces_the_full_message_without_moving_working_copy() {
    let root = scratch_repo("describe-mutation");
    write(&root, "file.txt", "hello\n");
    jj(&root, &["commit", "-m", "old description"]);
    let target = commit_id(&root, "@-");
    let working_copy_change = change_id(&root, "@");
    let description = "subject\n\nmultiline body";

    let outcome = block_on(diffui_core::mutations::run_mutation(
        repository(&root),
        MutationOp::Describe {
            target: RevisionSelection::Commit(target.clone()),
            description: description.to_owned(),
        },
        LoadProgress::default(),
    ))
    .expect("describe mutation");

    assert!(!outcome.moved_working_copy);
    assert_eq!(change_id(&root, "@"), working_copy_change);
    let rewritten = outcome.rewritten_commit.expect("rewritten commit id");
    assert_ne!(rewritten, target, "describe rewrites the commit");
    assert_eq!(
        jj(
            &root,
            &["log", "--no-graph", "-r", &rewritten, "-T", "description",]
        ),
        description
    );
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

/// The source browser's two backends against a real repo: the working copy
/// lists the on-disk directory — tracked files plus classified untracked /
/// ignored ones, with ignored dirs collapsed unenumerated — while a commit
/// lists exactly its tree; reads come from the right side (disk vs tree).
#[test]
#[ignore = "shells out to the jj CLI"]
fn source_browser_lists_and_reads_working_copy_and_commits() {
    let root = scratch_repo("source-browse");
    write(&root, ".gitignore", "/target/\n*.log\n");
    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    write(&root, "src/main.rs", "fn main() {}\n");
    write(&root, "README.md", "hello\n");
    jj(&root, &["commit", "-m", "base"]);
    let base_commit = commit_id(&root, "@-");

    // Rewrite a tracked file, then lay down ignored + untracked content
    // *after* the last jj op so nothing snapshots them into the tree.
    write(&root, "src/main.rs", "fn main() { println!(\"v2\"); }\n");
    jj(&root, &["status"]); // snapshots the edit into @
    std::fs::create_dir_all(root.join("target/debug")).expect("mkdir target");
    write(&root, "target/debug/junk.bin", "junk");
    write(&root, "debug.log", "log line\n");
    write(&root, "notes.txt", "untracked note\n");

    // ── Working copy: mirrors the directory ────────────────────────────
    let entries = block_on(list_source_tree(
        repository(&root),
        RevisionSelection::WorkingCopy,
    ))
    .expect("list working copy");
    let find = |path: &str| {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} missing from {entries:#?}"))
    };
    assert_eq!(find("src/main.rs").status, SourceEntryStatus::Tracked);
    assert_eq!(find("README.md").status, SourceEntryStatus::Tracked);
    assert_eq!(find("notes.txt").status, SourceEntryStatus::Untracked);
    assert_eq!(find("debug.log").status, SourceEntryStatus::Ignored);
    let target = find("target");
    assert!(
        target.is_dir && target.status == SourceEntryStatus::Ignored,
        "ignored dir arrives collapsed: {target:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.path.starts_with("target/")),
        "ignored dir contents must not be enumerated"
    );

    // Diff-status chips: the edited file reads Modified; untouched tracked
    // files carry no chip; untracked/ignored never do.
    assert_eq!(
        find("src/main.rs").change,
        Some(DiffFileStatus::Modified),
        "the wc edit must chip as modified"
    );
    assert_eq!(find("README.md").change, None);
    assert_eq!(find("notes.txt").change, None);
    assert_eq!(find("debug.log").change, None);

    // Lazily listing the ignored dir returns one level: a nested dir marker
    // (expandable in turn) — its contents stay unenumerated.
    let children = block_on(list_ignored_dir(repository(&root), "target".to_owned()))
        .expect("list ignored dir");
    assert_eq!(children.len(), 1, "one level only: {children:#?}");
    assert_eq!(children[0].path, "target/debug");
    assert!(children[0].is_dir);
    assert_eq!(children[0].status, SourceEntryStatus::Ignored);
    let nested = block_on(list_ignored_dir(
        repository(&root),
        "target/debug".to_owned(),
    ))
    .expect("list nested ignored dir");
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].path, "target/debug/junk.bin");
    assert!(!nested[0].is_dir);

    // ── A commit: exactly its tree ──────────────────────────────────────
    let entries = block_on(list_source_tree(
        repository(&root),
        RevisionSelection::Commit(base_commit.clone()),
    ))
    .expect("list base commit");
    let mut paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec![".gitignore", "README.md", "src/main.rs"]);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == SourceEntryStatus::Tracked && !entry.is_dir)
    );

    // ── Reads: tree side vs disk side ──────────────────────────────────
    let at_base = block_on(load_source_file(
        repository(&root),
        RevisionSelection::Commit(base_commit.clone()),
        "src/main.rs".to_owned(),
    ))
    .expect("read src/main.rs at base");
    let text = |load: &diffui_core::SourceFileLoad| {
        load.file.hunks[0]
            .lines
            .iter()
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(text(&at_base), "fn main() {}");
    assert!(
        at_base.file.hunks[0]
            .lines
            .iter()
            .any(|line| !line.syntax.is_empty()),
        "a .rs file gets syntax spans"
    );
    assert_eq!(at_base.line_count, 1);

    let at_wc = block_on(load_source_file(
        repository(&root),
        RevisionSelection::WorkingCopy,
        "src/main.rs".to_owned(),
    ))
    .expect("read src/main.rs in wc");
    assert_eq!(text(&at_wc), "fn main() { println!(\"v2\"); }");

    // Ignored files exist only on disk — the working-copy read serves them.
    let ignored = block_on(load_source_file(
        repository(&root),
        RevisionSelection::WorkingCopy,
        "debug.log".to_owned(),
    ))
    .expect("read ignored file");
    assert_eq!(text(&ignored), "log line");

    // Absent path at a commit errors instead of coming back empty.
    let missing = block_on(load_source_file(
        repository(&root),
        RevisionSelection::Commit(base_commit),
        "notes.txt".to_owned(),
    ));
    assert!(missing.is_err(), "missing paths must error: {missing:?}");
}

/// `jj rebase -r`: a leaf moves onto a new destination; the outcome tracks
/// the rewritten commit so the frontend's selection can follow it.
#[test]
#[ignore = "shells out to the jj CLI"]
fn rebase_revision_moves_a_leaf_onto_the_destination() {
    let root = scratch_repo("rebase-onto");
    write(&root, "file.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "@-");
    write(&root, "file.txt", "base\nx\n");
    jj(&root, &["commit", "-m", "x"]);
    let x = commit_id(&root, "description(exact:\"x\\n\")");
    jj(&root, &["new", "-r", &base, "-m", "y", "--no-edit"]);
    let y = commit_id(&root, "description(exact:\"y\\n\")");
    let y_change = change_id(&root, "description(exact:\"y\\n\")");

    let outcome = run(
        &root,
        MutationOp::Rebase {
            mode: RebaseSourceMode::Revisions,
            sources: vec![RevisionSelection::Commit(y.clone())],
            destination: Destination::Onto(RevisionSelection::Commit(x.clone())),
        },
    );

    assert_eq!(parent_ids(&root, &y_change), vec![x.clone()]);
    let rewritten = outcome.rewritten_commit.expect("rebase rewrote the leaf");
    assert_ne!(rewritten, y, "a moved commit gets a new commit id");
    assert_eq!(commit_id(&root, &y_change), rewritten);
    assert!(!outcome.moved_working_copy);
    assert!(outcome.operation_id.is_some());
}

/// `jj rebase -A a -B b` (the gap-drop gesture): the moved leaf lands exactly
/// between the two revisions — child re-parented onto it, it onto the parent.
#[test]
#[ignore = "shells out to the jj CLI"]
fn rebase_between_inserts_into_the_gap() {
    let root = scratch_repo("rebase-between");
    write(&root, "file.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\nmid\n");
    jj(&root, &["commit", "-m", "mid"]);
    let mid_change = change_id(&root, "description(exact:\"mid\\n\")");
    jj(&root, &["new", "-r", &base, "-m", "leaf", "--no-edit"]);
    let leaf = commit_id(&root, "description(exact:\"leaf\\n\")");
    let leaf_change = change_id(&root, "description(exact:\"leaf\\n\")");
    let mid = commit_id(&root, "description(exact:\"mid\\n\")");

    run(
        &root,
        MutationOp::Rebase {
            mode: RebaseSourceMode::Revisions,
            sources: vec![RevisionSelection::Commit(leaf.clone())],
            destination: Destination::Between {
                parent: RevisionSelection::Commit(base.clone()),
                child: RevisionSelection::Commit(mid),
            },
        },
    );

    // leaf keeps base as its parent; mid is re-parented onto leaf.
    assert_eq!(parent_ids(&root, &leaf_change), vec![base]);
    assert_eq!(
        parent_ids(&root, &mid_change),
        vec![commit_id(&root, &leaf_change)]
    );
}

/// `jj rebase -s`: the picked revision moves together with its descendants.
#[test]
#[ignore = "shells out to the jj CLI"]
fn rebase_with_descendants_moves_the_subtree() {
    let root = scratch_repo("rebase-descendants");
    write(&root, "file.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\ns1\n");
    jj(&root, &["commit", "-m", "s1"]);
    write(&root, "file.txt", "base\ns1\ns2\n");
    jj(&root, &["commit", "-m", "s2"]);
    let s1 = commit_id(&root, "description(exact:\"s1\\n\")");
    let s1_change = change_id(&root, "description(exact:\"s1\\n\")");
    let s2_change = change_id(&root, "description(exact:\"s2\\n\")");
    jj(&root, &["new", "-r", &base, "-m", "dest", "--no-edit"]);
    let dest = commit_id(&root, "description(exact:\"dest\\n\")");

    run(
        &root,
        MutationOp::Rebase {
            mode: RebaseSourceMode::WithDescendants,
            sources: vec![RevisionSelection::Commit(s1)],
            destination: Destination::Onto(RevisionSelection::Commit(dest.clone())),
        },
    );

    assert_eq!(parent_ids(&root, &s1_change), vec![dest]);
    assert_eq!(
        parent_ids(&root, &s2_change),
        vec![commit_id(&root, &s1_change)]
    );
}

/// `jj rebase -b`: pointing at the branch *head* moves the whole branch from
/// its fork-point root — no hunting for the first commit — and a branch
/// that's already an ancestor of the destination refuses cleanly.
#[test]
#[ignore = "shells out to the jj CLI"]
fn rebase_branch_moves_the_whole_branch_from_its_fork_point() {
    let root = scratch_repo("rebase-branch");
    write(&root, "file.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\nmain1\n");
    jj(&root, &["commit", "-m", "main1"]);
    let main1 = commit_id(&root, "description(exact:\"main1\\n\")");
    // Feature branch forked at base: f1 → f2.
    jj(&root, &["new", &base, "-m", "f1"]);
    write(&root, "feature.txt", "f1\n");
    jj(&root, &["new", "-m", "f2"]);
    write(&root, "feature.txt", "f1\nf2\n");
    jj(&root, &["new", "@", "-m", "wc off branch"]);
    let f1_change = change_id(&root, "description(exact:\"f1\\n\")");
    let f1 = commit_id(&root, "description(exact:\"f1\\n\")");
    let f2 = commit_id(&root, "description(exact:\"f2\\n\")");
    let f2_change = change_id(&root, "description(exact:\"f2\\n\")");

    // The preview resolves and names the branch: entry point = the fork
    // root (f1), moved set = the whole branch — even though the *head* was
    // picked. That's what the op bar shows and the sidebar washes.
    let preview = block_on(mutations::run_rebase_preview(
        repository(&root),
        RebaseSourceMode::Branch,
        vec![RevisionSelection::Commit(f2.clone())],
        Destination::Onto(RevisionSelection::Commit(main1.clone())),
    ))
    .expect("branch preview succeeds");
    let f1_short: String = f1_change.chars().take(8).collect();
    assert_eq!(preview.entry_points, vec![f1_short]);
    assert_eq!(preview.moved, 3, "f1 + f2 + the wc child all move");
    assert!(preview.moved_commit_ids.contains(&f1));
    assert!(preview.moved_commit_ids.contains(&f2));

    // Point at the branch *head*; the fork-point root (f1) is what moves.
    run(
        &root,
        MutationOp::Rebase {
            mode: RebaseSourceMode::Branch,
            sources: vec![RevisionSelection::Commit(f2)],
            destination: Destination::Onto(RevisionSelection::Commit(main1.clone())),
        },
    );

    assert_eq!(parent_ids(&root, &f1_change), vec![main1]);
    assert_eq!(
        parent_ids(&root, &f2_change),
        vec![commit_id(&root, &f1_change)]
    );

    // A destination inside the branch (a descendant of the source) leaves
    // nothing outside it to move: a benign no-op like the CLI's "Nothing
    // changed" — both in the preview (so j/k walking the branch's own rows
    // reads calmly, not as a failure)…
    let base_sel = commit_id(&root, "description(exact:\"base\\n\")");
    let dest = commit_id(&root, &f2_change);
    let empty = block_on(mutations::run_rebase_preview(
        repository(&root),
        RebaseSourceMode::Branch,
        vec![RevisionSelection::Commit(base_sel.clone())],
        Destination::Onto(RevisionSelection::Commit(dest.clone())),
    ))
    .expect("in-branch preview is a clean empty result, not an error");
    assert!(empty.simulated);
    assert_eq!(empty.moved, 0);
    assert!(empty.entry_points.is_empty());
    // …and in the executed op.
    let outcome = run(
        &root,
        MutationOp::Rebase {
            mode: RebaseSourceMode::Branch,
            sources: vec![RevisionSelection::Commit(base_sel)],
            destination: Destination::Onto(RevisionSelection::Commit(dest)),
        },
    );
    assert!(
        outcome.message.contains("Nothing to rebase"),
        "got: {}",
        outcome.message
    );
}

/// Squash into the parent: the source's tree change lands in the parent, the
/// source is abandoned, and both descriptions survive joined by a blank line.
#[test]
#[ignore = "shells out to the jj CLI"]
fn squash_into_parent_folds_changes_and_descriptions() {
    let root = scratch_repo("squash-parent");
    write(&root, "file.txt", "one\n");
    jj(&root, &["commit", "-m", "base message"]);
    let base_change = change_id(&root, "description(glob:\"base*\")");
    write(&root, "file.txt", "one\ntwo\n");
    jj(&root, &["commit", "-m", "child message"]);
    let child = commit_id(&root, "description(glob:\"child*\")");
    let child_change = change_id(&root, "description(glob:\"child*\")");

    let outcome = run(
        &root,
        MutationOp::Squash {
            from: vec![RevisionSelection::Commit(child)],
            into: SquashTarget::Parent,
        },
    );

    // The squashed-into parent now carries the child's tree...
    assert_eq!(
        jj(&root, &["file", "show", "-r", &base_change, "file.txt"]),
        "one\ntwo\n"
    );
    // ...and the joined descriptions.
    assert_eq!(
        jj(
            &root,
            &["log", "--no-graph", "-r", &base_change, "-T", "description"]
        ),
        "base message\n\nchild message\n"
    );
    // The emptied source is gone from the visible set.
    assert_eq!(
        jj(
            &root,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({child_change})"),
                "-T",
                "commit_id",
            ]
        ),
        ""
    );
    // Selection follows the rewritten destination.
    assert_eq!(
        outcome.rewritten_commit.expect("squash rewrites the dest"),
        commit_id(&root, &base_change)
    );
}

/// Squash into an arbitrary (non-parent) revision on a sibling branch.
#[test]
#[ignore = "shells out to the jj CLI"]
fn squash_into_arbitrary_revision() {
    let root = scratch_repo("squash-into");
    write(&root, "a.txt", "a\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "b.txt", "b\n");
    jj(&root, &["commit", "-m", "source"]);
    let source = commit_id(&root, "description(exact:\"source\\n\")");
    jj(&root, &["new", "-r", &base, "-m", "dest", "--no-edit"]);
    let dest_change = change_id(&root, "description(exact:\"dest\\n\")");

    run(
        &root,
        MutationOp::Squash {
            from: vec![RevisionSelection::Commit(source)],
            into: SquashTarget::Revision(RevisionSelection::Commit(commit_id(
                &root,
                "description(exact:\"dest\\n\")",
            ))),
        },
    );

    assert_eq!(
        jj(&root, &["file", "show", "-r", &dest_change, "b.txt"]),
        "b\n"
    );
}

/// `jj squash --from a --from b --into c`: the draft's add-source path folds
/// several revisions into one destination in a single op; a parent-target
/// squash with several sources is refused (whose parent?).
#[test]
#[ignore = "shells out to the jj CLI"]
fn squash_multiple_sources_into_one_destination() {
    let root = scratch_repo("squash-multi");
    write(&root, "base.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "a.txt", "a\n");
    jj(&root, &["commit", "-m", "src a"]);
    let src_a = commit_id(&root, "description(glob:\"src a*\")");
    jj(&root, &["new", "-r", &base]);
    write(&root, "b.txt", "b\n");
    jj(&root, &["commit", "-m", "src b"]);
    let src_b = commit_id(&root, "description(glob:\"src b*\")");
    jj(&root, &["new", "-r", &base, "-m", "dest", "--no-edit"]);
    let dest = commit_id(&root, "description(exact:\"dest\\n\")");
    let dest_change = change_id(&root, "description(exact:\"dest\\n\")");

    run(
        &root,
        MutationOp::Squash {
            from: vec![
                RevisionSelection::Commit(src_a.clone()),
                RevisionSelection::Commit(src_b.clone()),
            ],
            into: SquashTarget::Revision(RevisionSelection::Commit(dest)),
        },
    );

    // Both sources' trees landed in the destination…
    assert_eq!(
        jj(&root, &["file", "show", "-r", &dest_change, "a.txt"]),
        "a\n"
    );
    assert_eq!(
        jj(&root, &["file", "show", "-r", &dest_change, "b.txt"]),
        "b\n"
    );
    // …with all three descriptions joined.
    assert_eq!(
        jj(
            &root,
            &["log", "--no-graph", "-r", &dest_change, "-T", "description"]
        ),
        "dest\n\nsrc a\n\nsrc b\n"
    );

    // Parent target + several sources is ambiguous and refused.
    let result = block_on(mutations::run_mutation(
        repository(&root),
        MutationOp::Squash {
            from: vec![
                RevisionSelection::Commit(src_a),
                RevisionSelection::Commit(src_b),
            ],
            into: SquashTarget::Parent,
        },
        LoadProgress::default(),
    ));
    let error = result.expect_err("multi-source parent squash must fail");
    assert!(error.contains("explicit destination"), "got: {error}");
}

/// `jj new A B`: the merge draft's confirm creates a child of both picked
/// revisions and moves `@` onto it.
#[test]
#[ignore = "shells out to the jj CLI"]
fn merge_creates_a_child_of_both_parents() {
    let root = scratch_repo("merge-two");
    write(&root, "base.txt", "base\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "x.txt", "x\n");
    jj(&root, &["commit", "-m", "x"]);
    let x = commit_id(&root, "description(exact:\"x\\n\")");
    jj(&root, &["new", "-r", &base, "-m", "y", "--no-edit"]);
    let y = commit_id(&root, "description(exact:\"y\\n\")");

    let outcome = run(
        &root,
        MutationOp::Merge {
            parents: vec![
                RevisionSelection::Commit(x.clone()),
                RevisionSelection::Commit(y.clone()),
            ],
        },
    );

    assert!(outcome.moved_working_copy, "@ moves onto the merge");
    let mut parents = parent_ids(&root, "@");
    parents.sort_unstable();
    let mut expected = vec![x, y];
    expected.sort_unstable();
    assert_eq!(parents, expected);
    // The merged tree carries both sides.
    assert_eq!(jj(&root, &["file", "show", "-r", "@", "x.txt"]), "x\n");
    assert_eq!(
        jj(&root, &["file", "show", "-r", "@", "base.txt"]),
        "base\n"
    );

    // A merge with one distinct parent is refused.
    let result = block_on(mutations::run_mutation(
        repository(&root),
        MutationOp::Merge {
            parents: vec![
                RevisionSelection::Commit(base.clone()),
                RevisionSelection::Commit(base.clone()),
            ],
        },
        LoadProgress::default(),
    ));
    let error = result.expect_err("self-merge must fail");
    assert!(error.contains("two distinct parents"), "got: {error}");

    // Octopus: the draft's add-parent path sends all stacked parents in one
    // op — three distinct parents make a three-way merge commit.
    jj(&root, &["new", "-r", &base, "-m", "z", "--no-edit"]);
    let z = commit_id(&root, "description(exact:\"z\\n\")");
    let x = commit_id(&root, "description(exact:\"x\\n\")");
    let y = commit_id(&root, "description(exact:\"y\\n\")");
    run(
        &root,
        MutationOp::Merge {
            parents: vec![
                RevisionSelection::Commit(x.clone()),
                RevisionSelection::Commit(y.clone()),
                RevisionSelection::Commit(z.clone()),
            ],
        },
    );
    let mut parents = parent_ids(&root, "@");
    parents.sort_unstable();
    let mut expected = vec![x, y, z];
    expected.sort_unstable();
    assert_eq!(parents, expected, "three-parent octopus merge");
}

/// The merge preview names the paths that would conflict, without writing an
/// operation.
#[test]
#[ignore = "shells out to the jj CLI"]
fn merge_preview_lists_conflicting_paths() {
    let root = scratch_repo("merge-preview");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "line1\nSIDE-A\nline3\n");
    jj(&root, &["commit", "-m", "sideA"]);
    let side_a = commit_id(&root, "description(exact:\"sideA\\n\")");
    jj(&root, &["new", "-r", &base]);
    write(&root, "file.txt", "line1\nSIDE-B\nline3\n");
    jj(&root, &["commit", "-m", "sideB"]);
    let side_b = commit_id(&root, "description(exact:\"sideB\\n\")");

    let head_before = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head before preview");

    let preview = block_on(mutations::run_merge_preview(
        repository(&root),
        vec![
            RevisionSelection::Commit(side_a),
            RevisionSelection::Commit(side_b.clone()),
        ],
    ))
    .expect("conflicting preview succeeds");
    assert_eq!(preview.conflicts, vec!["file.txt".to_owned()]);
    assert!(!preview.truncated);

    let clean = block_on(mutations::run_merge_preview(
        repository(&root),
        vec![
            RevisionSelection::Commit(base),
            RevisionSelection::Commit(side_b),
        ],
    ))
    .expect("clean preview succeeds");
    assert!(clean.conflicts.is_empty());

    let head_after = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head after preview");
    assert_eq!(
        head_before, head_after,
        "previews must not write operations"
    );
}

/// `jj duplicate`: a sibling copy with the same tree and parents appears; the
/// original stays put.
#[test]
#[ignore = "shells out to the jj CLI"]
fn duplicate_creates_a_sibling_copy() {
    let root = scratch_repo("duplicate");
    write(&root, "file.txt", "one\n");
    jj(&root, &["commit", "-m", "base"]);
    write(&root, "file.txt", "one\ntwo\n");
    jj(&root, &["commit", "-m", "orig"]);
    let orig = commit_id(&root, "description(exact:\"orig\\n\")");

    let outcome = run(
        &root,
        MutationOp::Duplicate {
            target: RevisionSelection::Commit(orig.clone()),
        },
    );

    let copies = jj(
        &root,
        &[
            "log",
            "--no-graph",
            "-r",
            "description(exact:\"orig\\n\")",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    let copies: Vec<&str> = copies.lines().collect();
    assert_eq!(copies.len(), 2, "original + duplicate: {copies:?}");
    let duplicate = outcome.rewritten_commit.expect("duplicate id reported");
    assert_ne!(duplicate, orig);
    assert!(copies.contains(&duplicate.as_str()));
    assert!(copies.contains(&orig.as_str()));
    // Same parents, same tree.
    assert_eq!(parent_ids(&root, &duplicate), parent_ids(&root, &orig));
    assert_eq!(
        jj(&root, &["file", "show", "-r", &duplicate, "file.txt"]),
        "one\ntwo\n"
    );
}

/// `jj absorb`: the working copy's hunk lands in the ancestor that last
/// touched those lines, and the emptied working copy is discarded.
#[test]
#[ignore = "shells out to the jj CLI"]
fn absorb_moves_hunks_into_the_touching_ancestor() {
    let root = scratch_repo("absorb");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    jj(&root, &["commit", "-m", "base"]);
    let base_change = change_id(&root, "description(exact:\"base\\n\")");
    // Edit an existing line in the working copy — annotate attributes it to
    // the base commit, so absorb folds it there.
    write(&root, "file.txt", "line1\nline2 edited\nline3\n");
    jj(&root, &["status"]);

    let outcome = run(
        &root,
        MutationOp::Absorb {
            from: RevisionSelection::WorkingCopy,
        },
    );

    assert_eq!(
        jj(&root, &["file", "show", "-r", &base_change, "file.txt"]),
        "line1\nline2 edited\nline3\n"
    );
    assert_eq!(
        jj(
            &root,
            &[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "if(empty, \"empty\", \"nonempty\")"
            ]
        ),
        "empty"
    );
    assert!(
        outcome
            .output
            .iter()
            .any(|line| line.starts_with("absorbed into")),
        "absorb reports its destinations: {:?}",
        outcome.output
    );
}

/// Per-activity undo: reverting one specific operation by id brings the
/// abandoned commit back.
#[test]
#[ignore = "shells out to the jj CLI"]
fn undo_operation_reverts_a_specific_mutation() {
    let root = scratch_repo("undo-op");
    write(&root, "file.txt", "one\n");
    jj(&root, &["commit", "-m", "victim"]);
    let victim_change = change_id(&root, "description(exact:\"victim\\n\")");
    let victim = commit_id(&root, "description(exact:\"victim\\n\")");

    let outcome = run(
        &root,
        MutationOp::Abandon {
            target: RevisionSelection::Commit(victim),
        },
    );
    assert_eq!(
        jj(
            &root,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({victim_change})"),
                "-T",
                "commit_id",
            ]
        ),
        "",
        "the abandon must hide the commit first"
    );

    let op_id = outcome.operation_id.expect("mutations record their op id");
    block_on(mutations::run_undo_operation(repository(&root), op_id)).expect("undo the abandon");

    assert!(
        !jj(
            &root,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({victim_change})"),
                "-T",
                "commit_id",
            ]
        )
        .is_empty(),
        "undoing the abandon brings the commit back"
    );
}

/// Rewrites of immutable commits are refused, honoring the repo's configured
/// `immutable_heads()` override.
#[test]
#[ignore = "shells out to the jj CLI"]
fn immutable_commits_refuse_rebase() {
    let root = scratch_repo("immutable-guard");
    write(&root, "file.txt", "one\n");
    jj(&root, &["commit", "-m", "protected base"]);
    // Written straight into the repo dir (the pre-0.41 layout diffui-core
    // reads) rather than via `jj config set --repo`, which since jj 0.41
    // writes into the *user's* `~/.config/jj/repos/<id>/` — a test must not
    // leave artifacts there. Keeps the scratch repo's signing-off table.
    std::fs::write(
        root.join(".jj/repo/config.toml"),
        "[signing]\nbehavior = \"keep\"\n\n\
         [revset-aliases]\n'immutable_heads()' = 'description(glob:\"protected*\")'\n",
    )
    .expect("write repo config");
    let protected = commit_id(&root, "description(glob:\"protected*\")");
    let wc = commit_id(&root, "@");

    let result = block_on(mutations::run_mutation(
        repository(&root),
        MutationOp::Rebase {
            mode: RebaseSourceMode::Revisions,
            sources: vec![RevisionSelection::Commit(protected)],
            destination: Destination::Onto(RevisionSelection::Commit(wc)),
        },
        LoadProgress::default(),
    ));

    let error = result.expect_err("rebasing an immutable commit must fail");
    assert!(
        error.contains("immutable"),
        "error names immutability: {error}"
    );
}

/// The rebase preview predicts conflicts without touching the repo's visible
/// state: the op log head must not move, and the conflicting change is named.
#[test]
#[ignore = "shells out to the jj CLI"]
fn rebase_preview_predicts_conflicts_without_mutating() {
    let root = scratch_repo("rebase-preview");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    jj(&root, &["commit", "-m", "base"]);
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "line1\nSIDE-A\nline3\n");
    jj(&root, &["commit", "-m", "sideA"]);
    let side_a = commit_id(&root, "description(exact:\"sideA\\n\")");
    jj(&root, &["new", "-r", &base]);
    write(&root, "file.txt", "line1\nSIDE-B\nline3\n");
    jj(&root, &["commit", "-m", "sideB"]);
    let side_b = commit_id(&root, "description(exact:\"sideB\\n\")");
    let side_b_change = change_id(&root, "description(exact:\"sideB\\n\")");

    let head_before = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head before preview");

    let preview = block_on(mutations::run_rebase_preview(
        repository(&root),
        RebaseSourceMode::Revisions,
        vec![RevisionSelection::Commit(side_b.clone())],
        Destination::Onto(RevisionSelection::Commit(side_a)),
    ))
    .expect("preview succeeds");

    assert!(preview.simulated);
    assert_eq!(preview.moved, 1);
    let expected_short: String = side_b_change.chars().take(8).collect();
    assert_eq!(preview.new_conflicts, vec![expected_short]);

    // A non-conflicting placement predicts none.
    let clean = block_on(mutations::run_rebase_preview(
        repository(&root),
        RebaseSourceMode::Revisions,
        vec![RevisionSelection::Commit(side_b)],
        Destination::Onto(RevisionSelection::Commit(base)),
    ))
    .expect("clean preview succeeds");
    assert!(clean.simulated);
    assert!(clean.new_conflicts.is_empty());

    let head_after = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head after preview");
    assert_eq!(
        head_before, head_after,
        "previews must not write operations"
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
