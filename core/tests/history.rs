//! The history views against a live repository actor: op log, op show,
//! restore/revert, evolog, interdiffs and annotate, on jj-lib-built fixtures.

mod harness;

use diffui_core::history::{
    CommitChangeKind, EvologEntry, FileAnnotation, HistoryDiffTarget, HistoryRequest,
    HistoryResponse, OperationChanges, OperationEntry, RefKind,
};
use diffui_core::repo::{Command, Payload};
use diffui_core::{DiffDocument, MutationOp, RevisionSelection};
use harness::{TestRepo, commit_file, commit_ids, file_at, scratch_repo, write};

fn history(repo: &mut TestRepo, request: HistoryRequest) -> (Vec<Payload>, HistoryResponse) {
    let mut events = repo.run(|job| Command::History { job, request });
    match events.pop() {
        Some(Payload::History { response, .. }) => (events, response),
        other => panic!("expected a history response, got {other:?}"),
    }
}

fn op_log(repo: &mut TestRepo, limit: usize) -> (Vec<OperationEntry>, bool) {
    let (events, response) = history(repo, HistoryRequest::OpLog { limit });
    let entries = events
        .into_iter()
        .flat_map(|payload| match payload {
            Payload::OpLogBatch { entries, .. } => entries,
            other => panic!("unexpected event in the op log stream: {other:?}"),
        })
        .collect();
    match response {
        HistoryResponse::OpLogDone { truncated } => (entries, truncated),
        other => panic!("expected OpLogDone, got {other:?}"),
    }
}

fn show(repo: &mut TestRepo, operation_id: &str) -> OperationChanges {
    match history(
        repo,
        HistoryRequest::ShowOperation {
            operation_id: operation_id.to_owned(),
        },
    )
    .1
    {
        HistoryResponse::Operation(changes) => changes,
        other => panic!("expected an operation, got {other:?}"),
    }
}

fn evolog(repo: &mut TestRepo, revision: RevisionSelection) -> Vec<EvologEntry> {
    match history(repo, HistoryRequest::Evolog { revision }).1 {
        HistoryResponse::Evolog(entries) => entries,
        other => panic!("expected an evolog, got {other:?}"),
    }
}

fn history_diff(repo: &mut TestRepo, target: HistoryDiffTarget) -> DiffDocument {
    match history(repo, HistoryRequest::Diff { target }).1 {
        HistoryResponse::Diff { document, .. } => document,
        other => panic!("expected a diff, got {other:?}"),
    }
}

fn annotate(repo: &mut TestRepo, revision: RevisionSelection, path: &str) -> FileAnnotation {
    match history(
        repo,
        HistoryRequest::Annotate {
            revision,
            path: path.to_owned(),
        },
    )
    .1
    {
        HistoryResponse::Annotation(annotation) => annotation,
        other => panic!("expected an annotation, got {other:?}"),
    }
}

fn added_lines(document: &DiffDocument) -> Vec<String> {
    document
        .files
        .iter()
        .flat_map(|file| &file.hunks)
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == diffui_core::DiffLineKind::Addition)
        .map(|line| line.content.trim_end().to_owned())
        .collect()
}

fn describe_wc(repo: &mut TestRepo, description: &str) -> String {
    repo.mutate(MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: description.to_owned(),
    })
    .expect("describe @")
    .operation_id
    .expect("a mutation reports its operation")
}

#[test]
fn the_op_log_streams_newest_first_and_flags_snapshots() {
    let root = scratch_repo("history-op-log");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    let described = describe_wc(&mut repo, "second");

    let (entries, truncated) = op_log(&mut repo, 1000);
    assert!(!truncated);
    let expected = harness::operation_log(&root);
    assert_eq!(
        entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        expected
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<Vec<_>>(),
        "the op log follows `jj op log` order"
    );
    assert_eq!(entries[0].id, described);
    assert!(entries[0].is_current);
    assert!(entries.iter().skip(1).all(|entry| !entry.is_current));
    assert!(
        entries.iter().any(|entry| entry.is_snapshot),
        "commit_file snapshots the new file: {entries:?}"
    );
    assert!(entries[0].user.contains('@'));
    assert_eq!(entries[0].short_id().len(), 12);

    let (head, truncated) = op_log(&mut repo, 2);
    assert!(
        truncated,
        "a limit below the log's length reports truncation"
    );
    assert_eq!(head.len(), 2);
    assert_eq!(head[0].id, entries[0].id);
}

#[test]
fn op_show_reports_rewrites_creations_abandons_and_bookmarks() {
    let root = scratch_repo("history-op-show");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    let first = commit_ids(&root)[1].clone();

    // A describe rewrites `@`: one rewritten commit whose predecessor is the
    // old `@`, plus the working-copy pointer moving.
    let before = repo.graph("@")[0].summary.commit_id.clone();
    let describe_op = describe_wc(&mut repo, "described");
    let changes = show(&mut repo, &describe_op);
    assert!(!changes.is_merge);
    assert_eq!(changes.commits.len(), 1, "{changes:?}");
    let rewritten = &changes.commits[0];
    assert_eq!(rewritten.kind, CommitChangeKind::Rewritten);
    assert_eq!(rewritten.commit.summary, "described");
    assert!(rewritten.commit.visible);
    assert_eq!(rewritten.predecessors.len(), 1);
    assert_eq!(rewritten.predecessors[0].commit_id, before);
    assert!(!rewritten.predecessors[0].visible);
    assert!(!rewritten.rebased, "a describe is the op's own rewrite");
    assert!(
        changes
            .refs
            .iter()
            .any(|change| change.kind == RefKind::WorkingCopy),
        "{:?}",
        changes.refs
    );

    // `jj new` creates a commit.
    let new_op = repo
        .mutate(MutationOp::New {
            parent: RevisionSelection::Commit(first.clone()),
        })
        .unwrap()
        .operation_id
        .unwrap();
    let changes = show(&mut repo, &new_op);
    assert!(
        changes
            .commits
            .iter()
            .any(|change| change.kind == CommitChangeKind::Created),
        "{changes:?}"
    );

    // A bookmark move is a ref change and nothing else.
    let bookmark_op = repo
        .mutate(MutationOp::MoveBookmark {
            name: "feature".to_owned(),
            to: RevisionSelection::Commit(first.clone()),
            push_remote: None,
        })
        .unwrap()
        .operation_id
        .unwrap();
    let changes = show(&mut repo, &bookmark_op);
    assert!(changes.commits.is_empty(), "{changes:?}");
    let bookmark = changes
        .refs
        .iter()
        .find(|change| change.kind == RefKind::Bookmark)
        .expect("the bookmark change is listed");
    assert_eq!(bookmark.name, "feature");
    assert!(bookmark.from.is_empty());
    assert_eq!(bookmark.to[0].commit_id, first);

    // Abandoning the described commit.
    let described = repo
        .graph("all()")
        .into_iter()
        .find(|row| row.summary.description == "described")
        .expect("the described commit is visible")
        .summary
        .commit_id;
    let abandon_op = repo
        .mutate(MutationOp::Abandon {
            targets: vec![RevisionSelection::Commit(described.clone())],
        })
        .unwrap()
        .operation_id
        .unwrap();
    let changes = show(&mut repo, &abandon_op);
    let abandoned = changes
        .commits
        .iter()
        .find(|change| change.kind == CommitChangeKind::Abandoned)
        .expect("the abandoned commit is listed");
    assert_eq!(abandoned.commit.commit_id, described);
    assert!(!abandoned.commit.visible);
}

#[test]
fn op_show_tells_a_rewrite_from_the_descendants_it_rebased() {
    let root = scratch_repo("history-op-show-rebased");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    let first = commit_ids(&root)[1].clone();
    let op = repo
        .mutate(MutationOp::Describe {
            target: RevisionSelection::Commit(first),
            description: "first, reworded".to_owned(),
        })
        .unwrap()
        .operation_id
        .unwrap();
    let changes = show(&mut repo, &op);
    let reworded = changes
        .commits
        .iter()
        .find(|change| change.commit.summary == "first, reworded")
        .expect("the described commit is listed");
    assert!(!reworded.rebased);
    let child = changes
        .commits
        .iter()
        .find(|change| change.commit.summary.is_empty())
        .expect("`@` was rebased onto it");
    assert!(child.rebased);
}

#[test]
fn restoring_an_operation_brings_back_its_commits_and_files() {
    let root = scratch_repo("history-op-restore");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    let (entries, _) = op_log(&mut repo, 100);
    let checkpoint = entries[0].id.clone();
    let graph_before: Vec<String> = repo
        .graph("all()")
        .into_iter()
        .map(|row| row.summary.commit_id)
        .collect();

    commit_file(&mut repo, "b.txt", "two\n", "second");
    describe_wc(&mut repo, "scratch");
    assert!(root.join("b.txt").exists());

    // Uncommitted work at restore time is snapshotted first, so it stays
    // recoverable from the op log rather than being thrown away.
    write(&root, "c.txt", "dirty\n");
    let outcome = repo
        .mutate(MutationOp::RestoreOperation {
            operation_id: checkpoint.clone(),
        })
        .expect("restore");
    assert!(outcome.message.starts_with("Restored to operation"));

    let graph_after: Vec<String> = repo
        .graph("all()")
        .into_iter()
        .map(|row| row.summary.commit_id)
        .collect();
    assert_eq!(graph_after, graph_before);
    assert!(
        !root.join("b.txt").exists(),
        "the checkout follows the restored `@`"
    );
    let log = harness::operation_log(&root);
    assert!(
        log.iter()
            .any(|(description, _)| description == "snapshot working copy"),
        "the dirty file was snapshotted before the restore: {log:?}"
    );

    // An operation that predates the workspace can't be restored to.
    let root_op = log.last().unwrap().1.clone();
    let error = repo
        .mutate(MutationOp::RestoreOperation {
            operation_id: root_op,
        })
        .expect_err("the root operation has no working copy");
    assert!(error.to_string().contains("workspace"), "{error}");
}

#[test]
fn reverting_an_operation_keeps_later_work() {
    let root = scratch_repo("history-op-revert");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    let first = commit_ids(&root)[1].clone();
    let bookmark_op = repo
        .mutate(MutationOp::MoveBookmark {
            name: "keep".to_owned(),
            to: RevisionSelection::Commit(first.clone()),
            push_remote: None,
        })
        .unwrap()
        .operation_id
        .unwrap();
    // Later work the revert must not touch.
    describe_wc(&mut repo, "later");

    let outcome = repo
        .mutate(MutationOp::RevertOperation {
            operation_id: bookmark_op.clone(),
        })
        .expect("revert");
    assert!(
        outcome.message.starts_with("Reverted"),
        "{}",
        outcome.message
    );
    // The op-log view's revert and the activity row's are one `jj op revert`
    // of exactly that operation, described the way the CLI describes it.
    let (description, _) = &harness::operation_log(&root)[0];
    assert_eq!(description, &format!("revert operation {bookmark_op}"));
    let rows = repo.graph("all()");
    assert!(
        rows.iter().all(|row| row.summary.bookmarks.is_empty()),
        "the bookmark the reverted op set is gone"
    );
    assert!(
        rows.iter().any(|row| row.summary.description == "later"),
        "the describe that came after survives"
    );
}

#[test]
fn the_evolog_lists_versions_and_their_interdiffs() {
    let root = scratch_repo("history-evolog");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    write(&root, "a.txt", "one\n");
    repo.snapshot();
    describe_wc(&mut repo, "v1");
    write(&root, "a.txt", "one\ntwo\n");
    repo.snapshot();
    describe_wc(&mut repo, "v2");

    let entries = evolog(&mut repo, RevisionSelection::WorkingCopy);
    assert!(entries.len() >= 4, "{entries:?}");
    assert_eq!(entries[0].commit.summary, "v2");
    assert!(entries[0].commit.visible);
    assert!(entries.iter().skip(1).all(|entry| !entry.commit.visible));
    let change = &entries[0].commit.change_id;
    assert!(
        entries
            .iter()
            .all(|entry| &entry.commit.change_id == change)
    );
    assert!(entries[0].operation.is_some());

    // The snapshot that added "two": its interdiff against the version
    // before it is exactly that line.
    let snapshot = entries
        .iter()
        .find(|entry| {
            entry.commit.summary == "v1"
                && entry
                    .operation
                    .as_ref()
                    .is_some_and(|op| op.summary == "snapshot working copy")
        })
        .expect("the second snapshot is a version");
    let document = history_diff(&mut repo, snapshot.interdiff_target().unwrap());
    assert_eq!(added_lines(&document), vec!["two".to_owned()]);

    // The same version against its parents shows the whole file.
    let document = history_diff(
        &mut repo,
        HistoryDiffTarget::Commit {
            commit_id: snapshot.commit.commit_id.clone(),
        },
    );
    assert_eq!(
        added_lines(&document),
        vec!["one".to_owned(), "two".to_owned()]
    );
}

#[test]
fn annotate_attributes_each_line_to_the_commit_that_wrote_it() {
    let root = scratch_repo("history-annotate");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\ntwo\n", "first");
    commit_file(&mut repo, "a.txt", "one\nchanged\ntwo\n", "second");
    let ids = commit_ids(&root);
    let (second, first) = (ids[1].clone(), ids[2].clone());
    assert_eq!(file_at(&root, &second, "a.txt"), "one\nchanged\ntwo\n");

    let annotation = annotate(
        &mut repo,
        RevisionSelection::Commit(second.clone()),
        "a.txt",
    );
    assert_eq!(annotation.lines.len(), 3);
    let origin = |line: usize| &annotation.origins[annotation.lines[line] as usize];
    assert_eq!(origin(0).commit_id, first);
    assert_eq!(origin(1).commit_id, second);
    assert_eq!(origin(2).commit_id, first);
    assert_eq!(origin(1).summary, "second");
    assert_eq!(annotation.origins.len(), 2, "origins are deduplicated");

    let events = repo.run(|job| Command::History {
        job,
        request: HistoryRequest::Annotate {
            revision: RevisionSelection::Commit(second.clone()),
            path: "missing.txt".to_owned(),
        },
    });
    assert!(
        matches!(events.last(), Some(Payload::Failed { .. })),
        "{events:?}"
    );
}
