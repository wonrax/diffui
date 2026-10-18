//! Partial changes and conflict resolution against a live repository actor:
//! `jj split` / `jj squash` / `jj restore` on a file-and-hunk selection, and
//! `jj resolve` by side, base, or merge tool.
//!
//! Selections are built the way the diff pane builds them — from the hunk
//! headers of the revision's loaded diff — so these also pin down that the
//! backend cuts hunks exactly where the pane does.

mod harness;

use diffui_core::repo::{Command, Payload};
use diffui_core::{
    ConflictFile, ConflictResolution, DiffSelection, FileSelection, HunkPick, HunkRef,
    MergeToolInvocation, MutationOp, RepoError, RevisionSelection, SquashTarget,
};
use harness::{TestRepo, file_at, file_at_or_absent, full_description, scratch_repo, write};

/// Twenty numbered lines with `edits` applied — far-apart edits land in
/// separate hunks.
fn numbered(edits: &[(usize, &str)]) -> String {
    (1..=20)
        .map(|n| match edits.iter().find(|(line, _)| *line == n) {
            Some((_, text)) => format!("{text}\n"),
            None => format!("line {n}\n"),
        })
        .collect()
}

/// `(change id, commit id, description)` of the single revision `revset`
/// names.
fn rev(repo: &mut TestRepo, revset: &str) -> (String, String, String) {
    let rows = repo.graph(revset);
    assert_eq!(rows.len(), 1, "{revset} must name one revision");
    let summary = &rows[0].summary;
    (
        summary.change_id.clone(),
        summary.commit_id.clone(),
        summary.description.clone(),
    )
}

fn commit_of(repo: &mut TestRepo, revset: &str) -> String {
    rev(repo, revset).1
}

/// `jj commit -m`: describe `@`, start a new change on it.
fn commit(repo: &mut TestRepo, message: &str) {
    repo.snapshot();
    repo.mutate(MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: format!("{message}\n"),
    })
    .expect("describe");
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::WorkingCopy,
    })
    .expect("new");
}

fn describe_wc(repo: &mut TestRepo, message: &str) {
    repo.snapshot();
    repo.mutate(MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: format!("{message}\n"),
    })
    .expect("describe");
}

/// The selection the diff pane produces for `picks`: `(path, hunk indices)`,
/// an empty index list meaning the whole file.
fn select(
    repo: &mut TestRepo,
    revision: &RevisionSelection,
    picks: &[(&str, &[usize])],
) -> DiffSelection {
    let document = repo.diff(revision.clone());
    DiffSelection {
        files: picks
            .iter()
            .map(|(path, hunks)| {
                let file = document
                    .files
                    .iter()
                    .find(|file| file.path == *path)
                    .unwrap_or_else(|| panic!("{path} is not in the diff"));
                FileSelection {
                    path: (*path).to_owned(),
                    hunks: if hunks.is_empty() {
                        HunkPick::All
                    } else {
                        HunkPick::Hunks(
                            hunks
                                .iter()
                                .map(|&index| HunkRef {
                                    index,
                                    header: file.hunks[index].header.clone(),
                                })
                                .collect(),
                        )
                    },
                }
            })
            .collect(),
    }
}

/// base: a.txt (20 lines) → `@` "both edits": a.txt lines 2 and 18 edited,
/// b.txt added.
fn two_hunk_child(name: &str) -> TestRepo {
    let root = scratch_repo(name);
    let mut repo = TestRepo::open(&root);
    write(&root, "a.txt", &numbered(&[]));
    commit(&mut repo, "base");
    write(&root, "a.txt", &numbered(&[(2, "two"), (18, "eighteen")]));
    write(&root, "b.txt", "bee\n");
    describe_wc(&mut repo, "both edits");
    repo
}

#[test]
fn split_keeps_the_selected_hunk_and_moves_the_rest_to_a_new_child() {
    let mut repo = two_hunk_child("split-hunk");
    let (change, _, _) = rev(&mut repo, "@");
    let selection = select(
        &mut repo,
        &RevisionSelection::WorkingCopy,
        &[("a.txt", &[0])],
    );
    let outcome = repo
        .mutate(MutationOp::Split {
            target: RevisionSelection::WorkingCopy,
            selection,
            first_description: None,
            second_description: "the rest\n".to_owned(),
        })
        .expect("split");
    // `@` was split, so it follows the remaining changes (a new change id).
    assert!(outcome.moved_working_copy);
    let (wc_change, wc, _) = rev(&mut repo, "@");
    assert_ne!(wc_change, change);
    assert_eq!(full_description(&repo.root, &wc), "the rest\n");
    // The selected part keeps the original change id and description.
    let (first_change, first, _) = rev(&mut repo, "@-");
    assert_eq!(first_change, change);
    assert_eq!(full_description(&repo.root, &first), "both edits\n");
    assert_eq!(
        file_at(&repo.root, &first, "a.txt"),
        numbered(&[(2, "two")])
    );
    assert_eq!(file_at_or_absent(&repo.root, &first, "b.txt"), "");
    // The child carries the full original content.
    assert_eq!(
        file_at(&repo.root, &wc, "a.txt"),
        numbered(&[(2, "two"), (18, "eighteen")])
    );
    assert_eq!(file_at(&repo.root, &wc, "b.txt"), "bee\n");
    // And the files on disk still match `@`.
    assert_eq!(
        std::fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        numbered(&[(2, "two"), (18, "eighteen")])
    );
}

#[test]
fn split_of_a_described_commit_rebases_its_descendants_onto_the_remainder() {
    let mut repo = two_hunk_child("split-descendants");
    // Move on: `@` becomes a child of "both edits".
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::WorkingCopy,
    })
    .expect("new");
    let (change, target, _) = rev(&mut repo, "@-");
    let revision = RevisionSelection::Commit(target);
    let selection = select(&mut repo, &revision, &[("b.txt", &[])]);
    let outcome = repo
        .mutate(MutationOp::Split {
            target: revision,
            selection,
            first_description: Some("add b\n".to_owned()),
            second_description: "edit a\n".to_owned(),
        })
        .expect("split");
    assert!(!outcome.moved_working_copy);
    // The selection follows the first commit (same change).
    let (first_change, first, _) = rev(&mut repo, "@--");
    assert_eq!(first_change, change);
    assert_eq!(outcome.rewritten_commit.as_deref(), Some(first.as_str()));
    assert_eq!(full_description(&repo.root, &first), "add b\n");
    assert_eq!(file_at(&repo.root, &first, "a.txt"), numbered(&[]));
    assert_eq!(file_at(&repo.root, &first, "b.txt"), "bee\n");
    // `@`'s parent is now the remainder.
    let second = commit_of(&mut repo, "@-");
    assert_eq!(full_description(&repo.root, &second), "edit a\n");
}

#[test]
fn squashing_one_hunk_into_the_parent_leaves_the_other_behind() {
    let mut repo = two_hunk_child("squash-hunk");
    let (change, _, _) = rev(&mut repo, "@");
    let selection = select(
        &mut repo,
        &RevisionSelection::WorkingCopy,
        &[("a.txt", &[1])],
    );
    repo.mutate(MutationOp::SquashChanges {
        from: RevisionSelection::WorkingCopy,
        selection,
        into: SquashTarget::Parent,
    })
    .expect("squash");
    let (wc_change, wc, _) = rev(&mut repo, "@");
    assert_eq!(wc_change, change, "a partial squash keeps the source");
    assert_eq!(full_description(&repo.root, &wc), "both edits\n");
    let parent = commit_of(&mut repo, "@-");
    assert_eq!(
        full_description(&repo.root, &parent),
        "base\n",
        "the destination keeps its message"
    );
    assert_eq!(
        file_at(&repo.root, &parent, "a.txt"),
        numbered(&[(18, "eighteen")])
    );
    // The source still shows both edits on top of its (new) parent.
    assert_eq!(
        file_at(&repo.root, &wc, "a.txt"),
        numbered(&[(2, "two"), (18, "eighteen")])
    );
    let remaining = repo.diff(RevisionSelection::WorkingCopy);
    let a = remaining
        .files
        .iter()
        .find(|file| file.path == "a.txt")
        .expect("a.txt still changes in @");
    assert_eq!(a.hunks.len(), 1);
}

#[test]
fn squashing_everything_abandons_the_source_and_combines_descriptions() {
    let mut repo = two_hunk_child("squash-all");
    let (change, source, _) = rev(&mut repo, "@");
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::WorkingCopy,
    })
    .expect("new");
    let revision = RevisionSelection::Commit(source);
    let selection = select(&mut repo, &revision, &[("a.txt", &[0, 1]), ("b.txt", &[])]);
    let outcome = repo
        .mutate(MutationOp::SquashChanges {
            from: revision,
            selection,
            into: SquashTarget::Parent,
        })
        .expect("squash");
    assert!(outcome.message.contains("abandoned"), "{}", outcome.message);
    assert!(repo.graph(&format!("present({change})")).is_empty());
    let (_, parent, _) = rev(&mut repo, "@-");
    assert_eq!(
        full_description(&repo.root, &parent),
        "base\n\nboth edits\n"
    );
    assert_eq!(outcome.rewritten_commit.as_deref(), Some(parent.as_str()));
}

#[test]
fn moving_a_file_into_a_sibling_revision() {
    let mut repo = two_hunk_child("move-file");
    let (_, source, _) = rev(&mut repo, "@");
    let base = commit_of(&mut repo, "@-");
    let sibling = harness::new_commit(&repo.root, &[base], "sibling\n");
    let revision = RevisionSelection::Commit(source);
    let selection = select(&mut repo, &revision, &[("b.txt", &[])]);
    repo.mutate(MutationOp::SquashChanges {
        from: revision,
        selection,
        into: SquashTarget::Revision(RevisionSelection::Commit(sibling)),
    })
    .expect("move");
    let (_, moved_to, _) = rev(&mut repo, "description(exact:\"sibling\\n\")");
    assert_eq!(file_at(&repo.root, &moved_to, "b.txt"), "bee\n");
    let wc = commit_of(&mut repo, "@");
    assert_eq!(file_at_or_absent(&repo.root, &wc, "b.txt"), "");
    assert_eq!(
        file_at(&repo.root, &wc, "a.txt"),
        numbered(&[(2, "two"), (18, "eighteen")])
    );
}

#[test]
fn restoring_a_hunk_and_a_file_puts_the_parent_content_back() {
    let mut repo = two_hunk_child("restore");
    let selection = select(
        &mut repo,
        &RevisionSelection::WorkingCopy,
        &[("a.txt", &[0]), ("b.txt", &[])],
    );
    repo.mutate(MutationOp::RestoreChanges {
        target: RevisionSelection::WorkingCopy,
        selection,
    })
    .expect("restore");
    let wc = commit_of(&mut repo, "@");
    assert_eq!(
        file_at(&repo.root, &wc, "a.txt"),
        numbered(&[(18, "eighteen")])
    );
    assert_eq!(file_at_or_absent(&repo.root, &wc, "b.txt"), "");
    // The working copy on disk follows.
    assert!(!repo.root.join("b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        numbered(&[(18, "eighteen")])
    );
}

#[test]
fn line_endings_and_missing_final_newlines_survive_a_partial_restore() {
    let root = scratch_repo("restore-eol");
    let mut repo = TestRepo::open(&root);
    let crlf = |text: &str| text.replace('\n', "\r\n");
    let base = crlf(&numbered(&[]));
    let base = base.trim_end_matches("\r\n").to_owned();
    write(&root, "c.txt", &base);
    commit(&mut repo, "base");
    let edited = crlf(&numbered(&[(1, "one"), (20, "twenty")]));
    write(&root, "c.txt", &edited);
    repo.snapshot();
    let selection = select(
        &mut repo,
        &RevisionSelection::WorkingCopy,
        &[("c.txt", &[1])],
    );
    repo.mutate(MutationOp::RestoreChanges {
        target: RevisionSelection::WorkingCopy,
        selection,
    })
    .expect("restore");
    let wc = commit_of(&mut repo, "@");
    let expected = crlf(&numbered(&[(1, "one")]));
    let expected = expected.trim_end_matches("\r\n");
    assert_eq!(file_at(&root, &wc, "c.txt"), expected);
}

#[test]
fn a_selection_made_against_an_older_diff_is_refused() {
    let mut repo = two_hunk_child("stale-hunks");
    let mut selection = select(
        &mut repo,
        &RevisionSelection::WorkingCopy,
        &[("a.txt", &[1])],
    );
    // The file moves under the selection: a new line on top shifts hunk 1.
    write(
        &repo.root,
        "a.txt",
        &format!("zero\n{}", numbered(&[(2, "two"), (18, "eighteen")])),
    );
    let error = repo
        .mutate(MutationOp::RestoreChanges {
            target: RevisionSelection::WorkingCopy,
            selection: selection.clone(),
        })
        .expect_err("stale headers must be refused");
    assert!(error.to_string().contains("changed since"), "{error}");
    // An index past the end is refused the same way.
    selection.files[0].hunks = HunkPick::Hunks(vec![HunkRef {
        index: 9,
        header: "@@ -1 +1 @@".to_owned(),
    }]);
    assert!(
        repo.mutate(MutationOp::SquashChanges {
            from: RevisionSelection::WorkingCopy,
            selection,
            into: SquashTarget::Parent,
        })
        .is_err()
    );
}

// ── Conflicts ────────────────────────────────────────────────────────────

/// base → sideA / sideB rewriting the same line → `@` = their conflicted
/// merge. Returns the merge's commit id.
fn conflicted_merge(name: &str) -> TestRepo {
    let root = scratch_repo(name);
    let mut repo = TestRepo::open(&root);
    write(&root, "file.txt", "line1\nline2\nline3\n");
    commit(&mut repo, "base");
    let base = commit_of(&mut repo, "@-");
    write(&root, "file.txt", "line1\nSIDE-A\nline3\n");
    commit(&mut repo, "sideA");
    let side_a = commit_of(&mut repo, "@-");
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::Commit(base),
    })
    .expect("new on base");
    write(&root, "file.txt", "line1\nSIDE-B\nline3\n");
    commit(&mut repo, "sideB");
    let side_b = commit_of(&mut repo, "@-");
    repo.mutate(MutationOp::Merge {
        parents: vec![
            RevisionSelection::Commit(side_a),
            RevisionSelection::Commit(side_b),
        ],
    })
    .expect("merge");
    repo
}

fn conflict_info(repo: &mut TestRepo, revision: RevisionSelection) -> Vec<ConflictFile> {
    let events = repo.run(|job| Command::ConflictInfo { job, revision });
    match events.into_iter().next_back() {
        Some(Payload::ConflictInfoLoaded { files, .. }) => files,
        other => panic!("expected ConflictInfoLoaded, got {other:?}"),
    }
}

#[test]
fn conflict_info_labels_both_sides_and_the_base() {
    let mut repo = conflicted_merge("conflict-info");
    let files = conflict_info(&mut repo, RevisionSelection::WorkingCopy);
    assert_eq!(files.len(), 1);
    let file = &files[0];
    assert_eq!(file.path, "file.txt");
    assert_eq!(file.sides.len(), 2);
    assert_eq!(file.bases.len(), 1);
    assert!(file.pickable);
    assert_eq!(file.merge_tool_blocker, None);
    assert!(file.sides[0].label.contains("sideA"), "{:?}", file.sides);
    assert!(file.sides[1].label.contains("sideB"), "{:?}", file.sides);
    // A clean revision has nothing to resolve.
    let side_a = commit_of(&mut repo, "description(exact:\"sideA\\n\")");
    assert!(conflict_info(&mut repo, RevisionSelection::Commit(side_a)).is_empty());
}

#[test]
fn taking_a_side_or_the_base_resolves_the_file() {
    for (resolution, expected) in [
        (ConflictResolution::Side(0), "line1\nSIDE-A\nline3\n"),
        (ConflictResolution::Side(1), "line1\nSIDE-B\nline3\n"),
        (ConflictResolution::Base(0), "line1\nline2\nline3\n"),
    ] {
        let mut repo = conflicted_merge(&format!("take-{resolution:?}").replace(['(', ')'], ""));
        let outcome = repo
            .mutate(MutationOp::ResolveConflict {
                target: RevisionSelection::WorkingCopy,
                path: "file.txt".to_owned(),
                resolution,
            })
            .expect("resolve");
        assert!(outcome.message.contains("Resolved file.txt"));
        let wc = commit_of(&mut repo, "@");
        assert_eq!(file_at(&repo.root, &wc, "file.txt"), expected);
        assert!(conflict_info(&mut repo, RevisionSelection::WorkingCopy).is_empty());
        assert_eq!(
            std::fs::read_to_string(repo.root.join("file.txt")).unwrap(),
            expected
        );
    }
}

#[test]
fn resolving_a_file_that_is_not_conflicted_is_refused() {
    let mut repo = two_hunk_child("resolve-clean");
    let error = repo
        .mutate(MutationOp::ResolveConflict {
            target: RevisionSelection::WorkingCopy,
            path: "a.txt".to_owned(),
            resolution: ConflictResolution::Side(0),
        })
        .expect_err("nothing to resolve");
    assert!(
        matches!(error, RepoError::Other(ref m) if m.contains("not conflicted")),
        "{error}"
    );
}

#[test]
fn hunks_of_a_conflicted_file_cannot_be_picked() {
    let mut repo = conflicted_merge("partial-conflict");
    let selection = DiffSelection {
        files: vec![FileSelection {
            path: "file.txt".to_owned(),
            hunks: HunkPick::Hunks(vec![HunkRef {
                index: 0,
                header: "@@ -1,3 +1,3 @@".to_owned(),
            }]),
        }],
    };
    let error = repo
        .mutate(MutationOp::RestoreChanges {
            target: RevisionSelection::WorkingCopy,
            selection,
        })
        .expect_err("conflicted files are whole-file only");
    assert!(error.to_string().contains("conflicted"), "{error}");
}

fn prepare(repo: &mut TestRepo, path: &str) -> Result<MergeToolInvocation, RepoError> {
    let path = path.to_owned();
    let events = repo.run(|job| Command::PrepareMergeTool {
        job,
        revision: RevisionSelection::WorkingCopy,
        path,
    });
    match events.into_iter().next_back() {
        Some(Payload::MergeToolPrepared { invocation, .. }) => Ok(invocation),
        Some(Payload::Failed { error, .. }) => Err(error),
        other => panic!("expected MergeToolPrepared, got {other:?}"),
    }
}

/// Point the scratch repo's config at a merge "tool" (a shell one-liner).
fn configure_tool(repo: &TestRepo, toml: &str) {
    std::fs::write(harness::repo_dir(&repo.root).join("config.toml"), toml)
        .expect("write repo config");
}

#[test]
fn a_merge_tool_round_trip_writes_its_output_back() {
    let repo = conflicted_merge("merge-tool");
    configure_tool(
        &repo,
        r#"
ui.merge-editor = "fake"
[merge-tools.fake]
program = "sh"
merge-args = ["-c", "printf 'line1\nMERGED\nline3\n' > \"$1\"", "fake", "$output"]
"#,
    );
    // The actor was opened with the old settings; reopen it.
    let mut repo = TestRepo::open(&repo.root.clone());
    let invocation = prepare(&mut repo, "file.txt").expect("prepare");
    for input in ["base", "left", "right"] {
        assert!(
            invocation.dir.join(format!("{input}_file.txt")).exists(),
            "{input} file written"
        );
    }
    let left = std::fs::read_to_string(invocation.dir.join("left_file.txt")).unwrap();
    assert_eq!(left, "line1\nSIDE-A\nline3\n");
    let resolution = invocation.run().expect("tool ran");
    invocation.cleanup();
    assert!(!invocation.dir.exists());
    repo.mutate(MutationOp::ResolveConflict {
        target: invocation.revision.clone(),
        path: invocation.path.clone(),
        resolution,
    })
    .expect("apply the tool's output");
    let wc = commit_of(&mut repo, "@");
    assert_eq!(
        file_at(&repo.root, &wc, "file.txt"),
        "line1\nMERGED\nline3\n"
    );
}

#[test]
fn a_tool_that_edits_markers_can_leave_the_conflict_in_place() {
    let repo = conflicted_merge("merge-tool-markers");
    // "Resolves" nothing but touches the file: the markers parse back into
    // the same conflict, so the file stays conflicted.
    configure_tool(
        &repo,
        r#"
ui.merge-editor = "fake"
[merge-tools.fake]
program = "sh"
merge-args = ["-c", "printf '\n' >> \"$1\"", "fake", "$output"]
merge-tool-edits-conflict-markers = true
"#,
    );
    let mut repo = TestRepo::open(&repo.root.clone());
    let invocation = prepare(&mut repo, "file.txt").expect("prepare");
    assert!(
        String::from_utf8_lossy(&invocation.initial_output).contains("<<<<<<<"),
        "the output starts as the materialized conflict"
    );
    let resolution = invocation.run().expect("tool ran");
    invocation.cleanup();
    assert!(matches!(
        resolution,
        ConflictResolution::Content {
            marker_len: Some(_),
            ..
        }
    ));
    let error = repo.mutate(MutationOp::ResolveConflict {
        target: invocation.revision.clone(),
        path: invocation.path.clone(),
        resolution,
    });
    // The markers parse back into the same conflict: the file stays
    // conflicted, and the message says so.
    let outcome = error.expect("apply");
    assert!(
        outcome.message.contains("still conflicted"),
        "{}",
        outcome.message
    );
    assert_eq!(
        conflict_info(&mut repo, RevisionSelection::WorkingCopy).len(),
        1
    );
}

#[test]
fn an_unconfigured_merge_tool_says_how_to_configure_one() {
    let mut repo = conflicted_merge("merge-tool-missing");
    let error = prepare(&mut repo, "file.txt").expect_err("no tool configured");
    assert!(error.to_string().contains("ui.merge-editor"), "{error}");
}
