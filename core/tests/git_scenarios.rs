//! End-to-end git scenarios against scratch repos built with the `git` CLI.
//!
//! Ignored by default: they shell out to `git` and build repos under the
//! system temp dir. Run with:
//!
//! ```sh
//! cargo test -p diffui-core --test git_scenarios -- --ignored
//! ```
//!
//! Where the jj scenarios panic when their CLI is missing, these skip: git is
//! optional for a jj-only checkout. Each test rebuilds its repo from scratch,
//! so reruns are deterministic; the repos are left behind for inspection.

use std::path::{Path, PathBuf};
use std::process::Command;

use diffui_core::git::{list_git_source_tree, load_git_diff};
use diffui_core::{DiffFileStatus, DiffLineKind, Repository, RevisionSelection, Vcs};

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(future)
}

/// Whether `git` can run at all. These scenarios cover the git backend, so
/// without the CLI there is nothing to exercise and the fixtures in
/// `diff_parse` carry the coverage instead.
fn git_present() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Run `git` in `dir`, panicking (with stderr) on failure; returns stdout.
/// The user's own config is cut out of the setup commands so a scratch repo
/// starts from git's defaults and the scenarios can set the hostile values
/// themselves.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Scenario Test")
        .env("GIT_AUTHOR_EMAIL", "scenario@example.com")
        .env("GIT_COMMITTER_NAME", "Scenario Test")
        .env("GIT_COMMITTER_EMAIL", "scenario@example.com")
        .output()
        .expect("git CLI must be on PATH");
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// A fresh scratch git repo at a deterministic per-test path, with signing
/// off: a user config with commit signing would otherwise prompt (or hang)
/// on every scratch commit.
fn scratch_repo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("diffui-core-git-scenario-{test}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch dir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Scenario Test"]);
    git(&root, &["config", "user.email", "scenario@example.com"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    root
}

/// The config that reshapes diff output out from under the parser. Set on the
/// repo, so it reaches every invocation diffui makes, exactly as a user's own
/// `~/.gitconfig` would.
fn set_hostile_diff_config(root: &Path) {
    for (key, value) in [
        ("core.quotePath", "true"),
        ("diff.noprefix", "true"),
        ("diff.mnemonicPrefix", "true"),
        ("diff.suppressBlankEmpty", "true"),
        ("color.ui", "always"),
    ] {
        git(root, &["config", key, value]);
    }
}

fn repository(root: &Path) -> Repository {
    Repository {
        root: root.to_owned(),
        vcs: Vcs::Git,
        scope: PathBuf::new(),
    }
}

fn write(dir: &Path, name: &str, contents: &[u8]) {
    std::fs::write(dir.join(name), contents).expect("write scratch file");
}

/// Non-UTF-8 file content used to fail the whole load: `git diff` output was
/// decoded strictly, so one latin-1 line took the diff down with it.
#[test]
#[ignore]
fn latin1_content_still_diffs() {
    if !git_present() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let root = scratch_repo("latin1-content");
    write(&root, "notes.txt", b"caf\xe9\n");
    commit(&root, "add notes");
    write(&root, "notes.txt", b"caf\xe9!\n");

    let (document, _) = block_on(load_git_diff(
        &repository(&root),
        &RevisionSelection::WorkingCopy,
    ))
    .expect("diff a latin-1 file");

    assert_eq!(document.files.len(), 1);
    assert_eq!(document.files[0].path, "notes.txt");
    assert_eq!(document.total_additions, 1);
    assert_eq!(document.total_deletions, 1);
}

/// A filename that isn't valid UTF-8 used to take the whole diff and the
/// whole source-tree listing down with it. The diff now shows the file under
/// its lossily decoded name; the listing keeps its other entries. That name
/// no longer resolves on disk, so the browser's tracked walk skips it — the
/// alternative is offering a row that can't be opened.
#[cfg(unix)]
#[test]
#[ignore]
fn non_utf8_filename_does_not_take_the_listing_down() {
    if !git_present() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    use std::os::unix::ffi::OsStrExt;

    let root = scratch_repo("non-utf8-filename");
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    // Not every filesystem accepts a name that isn't valid UTF-8 — APFS
    // answers EILSEQ — and there is nothing to test where the name can't
    // exist. Skip rather than fail on the developer's machine.
    if std::fs::write(root.join(name), b"one\n").is_err() {
        eprintln!("skipping: the filesystem refuses a non-UTF-8 filename");
        return;
    }
    write(&root, "plain.txt", b"plain\n");
    commit(&root, "add a latin-1 name");
    std::fs::write(root.join(name), b"two\n").expect("write scratch file");

    let repository = repository(&root);
    let (document, _) = block_on(load_git_diff(&repository, &RevisionSelection::WorkingCopy))
        .expect("diff a latin-1 filename");
    assert_eq!(document.files.len(), 1);
    assert!(
        document.files[0].path.starts_with("caf") && document.files[0].path.ends_with(".txt"),
        "path: {:?}",
        document.files[0].path
    );

    let entries = block_on(list_git_source_tree(
        &repository,
        &RevisionSelection::WorkingCopy,
    ))
    .expect("list a tree holding a latin-1 filename");
    assert!(
        entries.iter().any(|entry| entry.path == "plain.txt"),
        "entries: {entries:?}"
    );
}

/// The user's `core.quotePath`, `diff.noprefix`, `diff.mnemonicPrefix`,
/// `diff.suppressBlankEmpty` and `color.ui` all reach the parser unless the
/// invocation pins them.
#[test]
#[ignore]
fn hostile_diff_config_does_not_reach_the_parser() {
    if !git_present() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let root = scratch_repo("hostile-config");
    write(&root, "café note.txt", "one\n\ntwo\n".as_bytes());
    write(&root, "mode only.sh", b"#!/bin/sh\n");
    commit(&root, "add files");
    set_hostile_diff_config(&root);
    git(&root, &["update-index", "--chmod=+x", "mode only.sh"]);
    git(&root, &["commit", "-q", "-m", "make it executable"]);
    write(&root, "café note.txt", "one\n\nthree\n".as_bytes());

    let (document, _) = block_on(load_git_diff(
        &repository(&root),
        &RevisionSelection::WorkingCopy,
    ))
    .expect("diff under a hostile config");

    let café = document
        .files
        .iter()
        .find(|file| file.path.contains("note"))
        .expect("the edited file");
    assert_eq!(café.path, "café note.txt");
    assert_eq!(café.status, DiffFileStatus::Modified);
    // The blank line keeps its numbering, so the change below it lands on
    // line 3 rather than drifting.
    let lines = &café.hunks[0].lines;
    assert_eq!(lines[1].kind, DiffLineKind::Context);
    assert_eq!(lines[1].new_line, Some(2));
    assert_eq!(lines[2].kind, DiffLineKind::Deletion);
    assert_eq!(lines[2].old_line, Some(3));

    // The mode change is its own commit: with no `---`/`+++` lines to correct
    // the header, a spaced path there used to read as a rename.
    let (document, _) = block_on(load_git_diff(
        &repository(&root),
        &RevisionSelection::Commit(head(&root)),
    ))
    .expect("show a mode-only commit under a hostile config");

    assert_eq!(document.files.len(), 1);
    let mode = &document.files[0];
    assert_eq!(mode.path, "mode only.sh");
    assert_eq!(mode.old_path.as_deref(), Some("mode only.sh"));
    assert_eq!(mode.status, DiffFileStatus::Modified);
}

/// `git show` defaults to a `--cc` combined diff on a merge, which carries no
/// `diff --git` headers — the commit rendered empty.
#[test]
#[ignore]
fn merge_commit_diffs_against_its_first_parent() {
    if !git_present() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let root = scratch_repo("merge-first-parent");
    write(&root, "base.txt", b"one\ntwo\n");
    commit(&root, "base");
    git(&root, &["checkout", "-q", "-b", "side"]);
    write(&root, "base.txt", b"one\nside\n");
    commit(&root, "side");
    git(&root, &["checkout", "-q", "main"]);
    write(&root, "base.txt", b"one\nmain\n");
    commit(&root, "main");
    // Conflicting on purpose: a clean merge has an empty first-parent diff.
    let merged = Command::new("git")
        .current_dir(&root)
        .args(["merge", "--no-ff", "-m", "merge"])
        .arg("side")
        .output()
        .expect("run git merge");
    assert!(!merged.status.success(), "the merge should conflict");
    write(&root, "base.txt", b"one\nmerged\n");
    commit(&root, "merge");

    let (document, _) = block_on(load_git_diff(
        &repository(&root),
        &RevisionSelection::Commit(head(&root)),
    ))
    .expect("diff a merge commit");

    assert_eq!(document.files.len(), 1);
    assert_eq!(document.files[0].path, "base.txt");
    let lines = &document.files[0].hunks[0].lines;
    // Against the first parent (`main`), not the side branch.
    assert!(
        lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Deletion && line.content == "main"),
        "lines: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Addition && line.content == "merged"),
        "lines: {lines:?}"
    );
}

/// `Binary files … differ` was dropped, leaving an empty Modified row.
#[test]
#[ignore]
fn binary_file_carries_the_binary_hunk() {
    if !git_present() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let root = scratch_repo("binary-file");
    write(&root, "blob.bin", b"\x00\x01\x02one\n");
    commit(&root, "add a blob");
    write(&root, "blob.bin", b"\x00\x01\x03two\n");

    let (document, _) = block_on(load_git_diff(
        &repository(&root),
        &RevisionSelection::WorkingCopy,
    ))
    .expect("diff a binary file");

    assert_eq!(document.files.len(), 1);
    assert_eq!(document.files[0].hunks.len(), 1);
    assert_eq!(document.files[0].hunks[0].header, "binary files differ");
}
