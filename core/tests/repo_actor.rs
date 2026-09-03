//! Scenarios against a live repository actor.
//!
//! The fixtures are built with jj-lib alone (`Workspace::init_internal_git` in
//! a temp dir), so these need no `jj` on PATH and run in the normal test pass.

mod harness;

use diffui_core::repo::{Command, JobId, Payload};
use diffui_core::{
    Effect, Event, MutationOp, RefreshOrigin, RepoError, RepoId, RevisionSelection, Session,
};
use harness::{TestRepo, commit_file, commit_ids, scratch_repo, write};

/// The job of the first command in `effects` that `pick` matches. The
/// projection returns a load's follow-ups in one list, so tests name the
/// command they mean instead of indexing into it.
fn job_of(effects: &[Effect], pick: fn(&Command) -> Option<JobId>) -> JobId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(command) => pick(command),
            _ => None,
        })
        .expect("the projection should have sent that command")
}

fn graph_job(effects: &[Effect]) -> JobId {
    job_of(effects, |command| match command {
        Command::LoadGraph { job, .. } => Some(*job),
        _ => None,
    })
}

fn repo_id(root: &std::path::Path) -> RepoId {
    diffui_core::repo::repo_id(&diffui_core::OpenSpec::Local {
        root: root.to_owned(),
        scope: std::path::PathBuf::new(),
    })
}

/// Drive a session by hand: apply `event`, and run whatever it asks for back
/// into the actor so the next event is the one the projection expects.
fn apply(session: &mut Session, repo: &mut TestRepo, event: Event) -> Vec<Effect> {
    let effects = session.apply(event);
    for effect in &effects {
        if let Effect::Send(command) = effect {
            repo.handle.send(command.clone());
        }
    }
    effects
}

#[test]
fn a_zero_row_graph_still_finishes_loading() {
    let root = scratch_repo("zero-row-load");
    let mut repo = TestRepo::open(&root);
    let mut session = Session::unloaded("none()".to_owned());
    session.capabilities = repo.handle.capabilities();

    let effects = session.load_graph(true);
    let job = graph_job(&effects);
    repo.handle.send(Command::LoadGraph {
        job,
        revset: "none()".to_owned(),
    });

    for payload in repo.drain(job) {
        session.apply(Event::new(repo_id(&root), payload));
    }
    // Only a batch used to lift the indicator, so a revset matching nothing
    // left the tab spinning forever.
    assert_eq!(session.status, diffui_core::LoadStatus::Loaded);
    assert_eq!(session.commits.len(), 0);
}

#[test]
fn a_diff_switch_during_a_graph_reload_survives_it() {
    let root = scratch_repo("diff-during-reload");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");
    commit_file(&mut repo, "b.txt", "two\n", "second");

    let mut session = Session::unloaded(String::new());
    session.capabilities = repo.handle.capabilities();

    // A graph reload and a diff switch are in flight at once. They own
    // different slots, so neither clears the other.
    let graph = session.load_graph(false);
    let target = RevisionSelection::Commit(commit_ids(&root)[1].clone());
    let diff = session.load_diff(target.clone());
    for effect in graph.iter().chain(diff.iter()) {
        if let Effect::Send(command) = effect {
            repo.handle.send(command.clone());
        }
    }
    let diff_job = job_of(&diff, |command| match command {
        Command::LoadDiff { job, .. } => Some(*job),
        _ => None,
    });
    for payload in repo.drain(diff_job) {
        session.apply(Event::new(repo_id(&root), payload));
    }
    assert_eq!(session.selected_revision, target);
    assert!(
        session.streaming(),
        "the graph load must still own its own slot"
    );
}

#[test]
fn a_superseded_walk_is_cancelled() {
    let root = scratch_repo("superseded-walk");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    for index in 0..40 {
        commit_file(&mut repo, &format!("f{index}.txt"), "x\n", "commit");
    }

    // Let the watch go quiet before the race below. Building the fixture wrote
    // forty files, the watch reports them, and an actor woken by one of those
    // pokes picks the load below up between the test's two sends — running a
    // forty-one row walk to completion before the cancel is even sent. The race
    // under test is the actor's, not the fixture's.
    std::thread::sleep(diffui_core::watcher::WATCH_DEBOUNCE * 3);

    let mut session = Session::unloaded(String::new());
    session.capabilities = repo.handle.capabilities();

    let first = session.load_graph(true);
    let first_job = graph_job(&first);
    // Starting a second load supersedes the first.
    let second = session.load_graph(true);
    assert!(
        second.iter().any(
            |effect| matches!(effect, Effect::Send(Command::Cancel { job }) if *job == first_job)
        ),
        "the superseded walk should be cancelled: {second:?}"
    );

    // And the actor acts on it. The cancel rides a channel the actor reads
    // while the walk is still running, which is the only way a flag check
    // inside the walk can ever fire; behind the walk on one queue it would
    // arrive after the thing it cancels had finished.
    repo.handle.send(Command::LoadGraph {
        job: first_job,
        revset: String::new(),
    });
    for effect in &second {
        if let Effect::Send(command) = effect {
            repo.handle.send(command.clone());
        }
    }
    let events = repo.drain(first_job);
    assert!(
        matches!(events.last(), Some(Payload::Cancelled { .. })),
        "the superseded walk should end in Cancelled, got {:?}",
        events.last()
    );
}

/// Two sessions over two repositories, each with a job in flight. Job ids are
/// minted process-wide, so the second session's job can't collide with the
/// first's — which is what used to let an event be folded into a stranger,
/// where it was dropped as stale while its real owner waited forever.
#[test]
fn two_repositories_mint_distinct_jobs() {
    let first = scratch_repo("distinct-jobs-a");
    let second = scratch_repo("distinct-jobs-b");
    let first_repo = TestRepo::open(&first);
    let second_repo = TestRepo::open(&second);

    let mut left = Session::unloaded(String::new());
    let mut right = Session::unloaded(String::new());
    left.capabilities = first_repo.handle.capabilities();
    right.capabilities = second_repo.handle.capabilities();

    let left_job = graph_job(&left.load_graph(true));
    let right_job = graph_job(&right.load_graph(true));
    assert_ne!(left_job, right_job);
    assert!(left.owns_job(left_job) && !left.owns_job(right_job));
    assert!(right.owns_job(right_job) && !right.owns_job(left_job));
}

#[test]
fn a_mutation_lands_on_the_tab_that_asked_for_it() {
    let root = scratch_repo("background-mutation");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");

    // Two sessions over one repository, as two tabs would be. Only the one
    // whose slot owns the job folds the result in.
    let mut foreground = Session::unloaded(String::new());
    let mut background = Session::unloaded(String::new());
    for session in [&mut foreground, &mut background] {
        session.capabilities = repo.handle.capabilities();
        session.selected_revision = RevisionSelection::Commit(commit_ids(&root)[1].clone());
    }

    let (job, _effects) = background.mutate(
        MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
        false,
    );
    repo.handle.send(Command::Mutate {
        job,
        op: MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
        allow_immutable: false,
    });
    for payload in repo.drain(job) {
        let event = Event::new(repo_id(&root), payload);
        foreground.apply(event.clone());
        background.apply(event);
    }
    assert_eq!(
        background.selected_revision,
        RevisionSelection::WorkingCopy,
        "`jj new` moves `@`, so the tab that ran it follows"
    );
    assert!(
        matches!(foreground.selected_revision, RevisionSelection::Commit(_)),
        "the other tab keeps the revision it was browsing"
    );
}

#[test]
fn a_fetch_queues_behind_a_mutation() {
    let root = scratch_repo("fetch-behind-mutation");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");

    // Both take the working-copy lock. The actor is single-threaded, so the
    // fetch cannot start until the mutation has finished — which is what
    // deleted the frontend's mutation queue.
    let mutate = repo.send(|job| Command::Mutate {
        job,
        op: MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
        allow_immutable: false,
    });
    let fetch = repo.send(|job| Command::Fetch {
        job,
        target: diffui_core::FetchTarget::AllRemotes,
    });

    let mutation_events = repo.drain(mutate);
    assert!(
        matches!(mutation_events.last(), Some(Payload::MutationDone { .. })),
        "the mutation runs first: {mutation_events:?}"
    );
    // The repo has no remotes, so the fetch reports that rather than racing
    // the mutation for the lock.
    let fetch_events = repo.drain(fetch);
    assert!(
        matches!(fetch_events.last(), Some(Payload::Failed { .. })),
        "a fetch with no remotes fails cleanly: {fetch_events:?}"
    );
    // The lock survived both.
    assert!(!repo.snapshot().fingerprint.is_empty());
}

#[test]
fn undo_bailing_on_a_merge_operation_leaves_the_lock_finished() {
    let root = scratch_repo("undo-merge-op");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");

    // A genuine merge operation: two parents, so there is no single state to
    // revert to and `resolve_undo_target` refuses it. That refusal happens
    // before the pre-undo working-copy fold is committed, so nothing is
    // written and the lock closes.
    let merge_op = harness::write_merge_operation(&root);
    write(&root, "b.txt", "uncommitted\n");
    let error = repo
        .mutate(MutationOp::Undo {
            operation_id: Some(merge_op),
        })
        .expect_err("a merge operation cannot be undone");
    assert!(
        matches!(&error, RepoError::Other(message) if message.contains("merge operation")),
        "the refusal should name the merge, got {error:?}"
    );

    // The working copy is usable straight afterwards: the lock was abandoned,
    // not left open, and `@` is not divergent.
    let snapshot = repo.snapshot();
    assert!(!snapshot.fingerprint.is_empty());
    let rows = repo.graph("all()");
    let working_copies = rows
        .iter()
        .filter(|row| row.summary.is_working_copy)
        .count();
    assert_eq!(working_copies, 1, "`@` must not have gone divergent");
    assert!(
        rows.iter().all(|row| !row.summary.is_divergent),
        "no change should be divergent after the bail"
    );
}

#[test]
fn a_no_change_snapshot_still_finishes_the_lock() {
    let root = scratch_repo("idle-snapshot");
    let mut repo = TestRepo::open(&root);
    let first = repo.snapshot();
    // Nothing changed on disk, so no operation is written — but the lock is
    // finished all the same, which is what persists the refreshed file states.
    let second = repo.snapshot();
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(
        second.parent_fingerprint.as_deref(),
        Some(second.fingerprint.as_str())
    );
}

#[test]
fn a_view_only_bookmark_move_writes_an_operation_without_touching_the_disk() {
    let root = scratch_repo("bookmark-view-only");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");

    // An uncommitted edit that a working-copy lock would have folded into `@`.
    write(&root, "untracked.txt", "still here\n");
    let outcome = repo
        .mutate(MutationOp::MoveBookmark {
            name: "shelf".to_owned(),
            to: RevisionSelection::Commit(commit_ids(&root)[1].clone()),
            push_remote: None,
        })
        .expect("set a bookmark");
    assert!(!outcome.moved_working_copy);
    assert!(outcome.operation_id.is_some());
    // The bookmark op took no lock, so the edit is still uncommitted.
    let snapshot = repo.snapshot();
    assert_ne!(
        snapshot.parent_fingerprint.as_deref(),
        Some(snapshot.fingerprint.as_str()),
        "the deferred edit lands in the next snapshot, not the bookmark op"
    );
}

#[test]
fn an_empty_status_sweep_survives_a_working_copy_chip_flip() {
    let root = scratch_repo("empty-status-chip");
    let mut repo = TestRepo::open(&root);
    repo.snapshot();
    commit_file(&mut repo, "a.txt", "one\n", "first");

    let mut session = Session::unloaded(String::new());
    session.capabilities = repo.handle.capabilities();
    let effects = session.load_graph(true);
    let job = graph_job(&effects);
    repo.handle.send(Command::LoadGraph {
        job,
        revset: String::new(),
    });
    let mut follow_ups = Vec::new();
    for payload in repo.drain(job) {
        follow_ups.extend(apply(
            &mut session,
            &mut repo,
            Event::new(repo_id(&root), payload),
        ));
    }
    let sweep = follow_ups.iter().find_map(|effect| match effect {
        Effect::Send(Command::EmptyStatus { job, .. }) => Some(*job),
        _ => None,
    });

    // Flipping `@`'s chip repaints the sidebar but leaves the graph identity
    // alone, so a sweep started against it is still valid when it lands.
    let index = session
        .commits
        .working_copy_index()
        .expect("`@` is in the loaded graph");
    let flipped = !session.commits.row(index).is_empty().unwrap_or(false);
    let stamp = session.repaint_version;
    session.apply_working_copy_empty(Some(flipped));
    assert_ne!(
        session.repaint_version, stamp,
        "flipping the chip must repaint the row"
    );

    if let Some(sweep) = sweep {
        let payloads = repo.drain(sweep);
        assert!(
            payloads
                .iter()
                .any(|payload| matches!(payload, Payload::EmptyStatus { .. })),
            "the sweep should complete, not be dropped: {payloads:?}"
        );
        for payload in payloads {
            session.apply(Event::new(repo_id(&root), payload));
        }
    }
}

/// The other workspace rebases this one's working-copy commit, so this
/// checkout is synced with a commit the repo has moved past. A mutation must
/// recover the checkout rather than snapshot the stale tree over the rebased
/// commit, which would silently revert the other workspace's work.
#[test]
fn a_mutation_on_a_stale_workspace_recovers() {
    let root = scratch_repo("stale-workspace");
    let mut main = TestRepo::open(&root);
    main.snapshot();
    commit_file(&mut main, "shared.txt", "base\n", "base");

    let side = harness::add_workspace(&root, "side");
    let mut other = TestRepo::open(&side);
    write(&side, "side.txt", "side-work\n");
    other.snapshot();
    other
        .mutate(MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: "side work".to_owned(),
        })
        .expect("describe the side workspace's commit");

    // Merge both workspaces' heads from the first one, then edit in the second
    // so its snapshot rebases the merge — which is what strands the first.
    let heads: Vec<RevisionSelection> = commit_ids(&root)
        .into_iter()
        .take(2)
        .map(RevisionSelection::Commit)
        .collect();
    main.mutate(MutationOp::Merge { parents: heads })
        .expect("merge the two workspaces' heads");
    write(&side, "side.txt", "side-work\nside-more\n");
    other.snapshot();
    assert_eq!(
        std::fs::read_to_string(root.join("side.txt")).ok(),
        Some("side-work\n".to_owned()),
        "the first workspace's checkout is now behind the rebased merge"
    );

    // The mutation lands on the recovered checkout, not instead of it.
    let outcome = main
        .mutate(MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: "described merge".to_owned(),
        })
        .expect("the mutation recovers the stale checkout and then applies");
    assert!(outcome.operation_id.is_some());
    assert_eq!(
        std::fs::read_to_string(root.join("side.txt")).ok(),
        Some("side-work\nside-more\n".to_owned()),
        "the recovery keeps the other workspace's edit instead of reverting it"
    );
    let rows = main.graph("all()");
    assert!(
        rows.iter().all(|row| !row.summary.is_divergent),
        "a clean recovery must not diverge anything"
    );
    assert!(
        rows.iter()
            .any(|row| row.summary.description.starts_with("described merge")),
        "the description should have been applied after the recovery"
    );
}

#[test]
fn a_cold_load_that_fails_at_snapshot_reports_the_failure() {
    let root = scratch_repo("cold-load-failure");
    let repo = TestRepo::open(&root);
    let mut session = Session::unloaded(String::new());
    session.capabilities = repo.handle.capabilities();

    // A snapshot failure belongs to the snapshot's slot, not the graph's: it
    // must not blank the tab, because the graph is still whatever it was.
    let effects = session.snapshot(RefreshOrigin::Focus);
    let job = job_of(&effects, |command| match command {
        Command::Snapshot { job, .. } => Some(*job),
        _ => None,
    });
    session.apply(Event::new(
        repo_id(&root),
        Payload::Failed {
            job,
            error: RepoError::Lock,
        },
    ));
    assert!(
        !matches!(session.status, diffui_core::LoadStatus::Failed(_)),
        "an operation failure is a toast, not a dead tab"
    );

    // A *graph* failure is the one that does.
    let effects = session.load_graph(true);
    let job = graph_job(&effects);
    session.apply(Event::new(
        repo_id(&root),
        Payload::Failed {
            job,
            error: RepoError::Revset("no such function".to_owned()),
        },
    ));
    assert!(matches!(session.status, diffui_core::LoadStatus::Failed(_)));
}
