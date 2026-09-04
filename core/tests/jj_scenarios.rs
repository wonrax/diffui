//! End-to-end jj scenarios against scratch repos built with jj-lib.
//!
//! Part of the ordinary `cargo test --workspace` pass. The fixtures come from
//! the shared harness: jj-lib for the shapes the protocol deliberately cannot
//! express (a commit `@` does not move onto, two concurrent operations), the
//! repository actor's own commands for everything else. So there is no `jj` on
//! PATH to find, and no way for the config of whoever runs the tests to reach
//! a scratch repo.
//!
//! Each test rebuilds its repo from scratch, so reruns are deterministic; the
//! repos are left behind afterwards for inspection.

mod harness;

use std::path::{Path, PathBuf};

use diffui_core::jj::read_jj_op_head;
use diffui_core::repo::{Command, JobId, OpenSpec, Payload, RepoError, SettingsSource};
use diffui_core::{
    BookmarksInfo, BranchStatus, CommitStore, Destination, DiffDocument, DiffFileStatus,
    DiffLineKind, LoadProgress, MutationOp, MutationOutcome, PreviewRequest, RebaseSourceMode,
    Repository, RepositorySnapshot, RevisionDetails, RevisionSelection, SourceEntry,
    SourceFileLoad, SquashTarget, Vcs, list_ignored_dir,
};
use diffui_core::{SourceEntryStatus, mutations};
use futures::StreamExt;
use harness::{OpBase, block_on, scratch_repo, write};

/// Run one command against a freshly-opened actor for `root` and return every
/// event of its job, terminal one last.
///
/// A new actor per call is deliberate: each scenario reads the repository from
/// scratch, exactly as reopening the tab would, so nothing a previous command
/// cached can mask a bug.
async fn run_one(root: &Path, make: impl FnOnce(JobId) -> Command) -> Vec<Payload> {
    let mut events = Box::pin(diffui_core::repo::open_with(
        OpenSpec::Local {
            root: root.to_owned(),
            scope: PathBuf::new(),
        },
        // The fixture's settings, never the developer's — see
        // `harness::test_settings_for`.
        SettingsSource::Fixed(Box::new(harness::test_settings_for(root))),
    ));
    let handle = match events.next().await.map(|event| event.payload) {
        Some(Payload::Ready { handle, .. }) => handle,
        other => panic!("expected the actor's Ready event, got {other:?}"),
    };
    let job = JobId(1);
    handle.send(make(job));
    let mut collected = Vec::new();
    while let Some(event) = events.next().await {
        if event.job() != Some(job) {
            continue;
        }
        let terminal = event.payload.is_terminal();
        collected.push(event.payload);
        if terminal {
            break;
        }
    }
    collected
}

/// Walk the graph and fold it into the compact store the sidebar renders —
/// what the projection does with the actor's batches.
async fn load_jj_commits(
    root: PathBuf,
    revset: String,
    _progress: LoadProgress,
) -> anyhow::Result<(
    CommitStore,
    diffui_core::graph_layout::GraphLayout,
    Option<BranchStatus>,
    BookmarksInfo,
)> {
    let events = run_one(&root, |job| Command::LoadGraph { job, revset }).await;
    let mut store = CommitStore::default();
    let mut graph = diffui_core::graph_layout::GraphLayout::default();
    let mut cursor = diffui_core::ColdCursor::default();
    let mut tail = None;
    for payload in events {
        match payload {
            Payload::Batch { rows, .. } => {
                diffui_core::fold_cold_batch(&mut store, &mut graph, &mut cursor, rows, false);
            }
            Payload::GraphLoaded { tail: loaded, .. } => tail = Some(loaded),
            Payload::Progress { .. } => {}
            Payload::Failed { error, .. } => anyhow::bail!("{error}"),
            other => panic!("unexpected event during a graph load: {other:?}"),
        }
    }
    let tail = tail.expect("the walk must end with GraphLoaded");
    for (index, empty) in tail.empty_updates {
        store.set_is_empty(index, empty);
    }
    Ok((store, graph, tail.branch_status, tail.bookmarks))
}

async fn load_jj_diff(
    repository: Repository,
    revision: RevisionSelection,
) -> anyhow::Result<(DiffDocument, Option<RevisionDetails>)> {
    let events = run_one(&repository.root, |job| Command::LoadDiff { job, revision }).await;
    match events.into_iter().next_back() {
        Some(Payload::DiffLoaded {
            document, details, ..
        }) => Ok((document, details)),
        Some(Payload::Failed { error, .. }) => anyhow::bail!("{error}"),
        other => panic!("expected DiffLoaded, got {other:?}"),
    }
}

async fn load_jj_repository_snapshot(repository: Repository) -> anyhow::Result<RepositorySnapshot> {
    let events = run_one(&repository.root, |job| Command::Snapshot {
        job,
        origin: diffui_core::RefreshOrigin::Focus,
    })
    .await;
    match events.into_iter().next_back() {
        Some(Payload::SnapshotDone { snapshot, .. }) => Ok(snapshot),
        Some(Payload::Failed { error, .. }) => anyhow::bail!("{error}"),
        other => panic!("expected SnapshotDone, got {other:?}"),
    }
}

async fn list_source_tree(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<Vec<SourceEntry>, String> {
    let events = run_one(&repository.root, |job| Command::ListTree { job, revision }).await;
    match events.into_iter().next_back() {
        Some(Payload::TreeListed { entries, .. }) => Ok(entries),
        Some(Payload::Failed { error, .. }) => Err(error.to_string()),
        other => panic!("expected TreeListed, got {other:?}"),
    }
}

async fn load_source_file(
    repository: Repository,
    revision: RevisionSelection,
    path: String,
) -> Result<SourceFileLoad, String> {
    let events = run_one(&repository.root, |job| Command::ReadFile {
        job,
        revision,
        path,
    })
    .await;
    match events.into_iter().next_back() {
        Some(Payload::FileRead { file, .. }) => Ok(file),
        Some(Payload::Failed { error, .. }) => Err(error.to_string()),
        other => panic!("expected FileRead, got {other:?}"),
    }
}

async fn run_mutation(
    repository: Repository,
    op: MutationOp,
    _progress: LoadProgress,
    allow_immutable: bool,
) -> Result<MutationOutcome, RepoError> {
    let events = run_one(&repository.root, |job| Command::Mutate {
        job,
        op,
        allow_immutable,
    })
    .await;
    match events.into_iter().next_back() {
        Some(Payload::MutationDone { outcome, .. }) => Ok(outcome),
        Some(Payload::Failed { error, .. }) => Err(error),
        other => panic!("expected a mutation result, got {other:?}"),
    }
}

async fn run_rebase_preview(
    repository: Repository,
    mode: RebaseSourceMode,
    sources: Vec<RevisionSelection>,
    destination: Destination,
) -> Result<diffui_core::RebasePreview, String> {
    let draft = PreviewRequest::Rebase {
        mode,
        sources,
        destination,
    };
    match preview(&repository.root, draft).await? {
        diffui_core::DraftSimulation::Rebase(preview) => Ok(preview),
        other => panic!("expected a rebase preview, got {other:?}"),
    }
}

async fn run_merge_preview(
    repository: Repository,
    parents: Vec<RevisionSelection>,
) -> Result<diffui_core::MergePreview, String> {
    match preview(&repository.root, PreviewRequest::Merge { parents }).await? {
        diffui_core::DraftSimulation::Merge(preview) => Ok(preview),
        other => panic!("expected a merge preview, got {other:?}"),
    }
}

async fn preview(
    root: &Path,
    draft: PreviewRequest,
) -> Result<diffui_core::DraftSimulation, String> {
    let events = run_one(root, |job| Command::Preview { job, draft }).await;
    match events.into_iter().next_back() {
        Some(Payload::PreviewDone { simulation, .. }) => Ok(simulation),
        Some(Payload::Failed { error, .. }) => Err(error.to_string()),
        other => panic!("expected PreviewDone, got {other:?}"),
    }
}

fn repository(root: &Path) -> Repository {
    Repository {
        root: root.to_owned(),
        vcs: Vcs::Jj,
        scope: PathBuf::new(),
    }
}

// ── The fixture verbs, spelled the way the jj CLI spells them ──────────────
//
// Descriptions are stored with the trailing newline `jj -m` writes, so the
// `description(exact:"…\n")` revsets below address commits exactly as `jj log`
// does.

/// `jj commit -m <message>`: fold the disk into `@`, describe it, start a new
/// child. Both mutations go down the actor's protocol, which snapshots before
/// it mutates — the path the app itself takes.
fn commit(root: &Path, message: &str) {
    run(
        root,
        MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: format!("{message}\n"),
        },
    );
    run(
        root,
        MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
    );
}

/// `jj new -r <parents…> -m <message>`: a child of `parents` with `@` moved
/// onto it.
fn new_edit(root: &Path, parents: &[String], message: &str) {
    let op = match parents {
        // No parent named is `jj new` off `@`, the CLI's own default.
        [] => MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
        [parent] => MutationOp::New {
            parent: RevisionSelection::Commit(parent.clone()),
        },
        many => MutationOp::Merge {
            parents: many
                .iter()
                .cloned()
                .map(RevisionSelection::Commit)
                .collect(),
        },
    };
    run(root, op);
    run(
        root,
        MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: format!("{message}\n"),
        },
    );
}

/// `jj new -r <parents…> -m <message> --no-edit`: a leaf `@` stays off.
fn new_leaf(root: &Path, parents: &[String], message: &str) -> String {
    harness::new_commit(root, parents, &format!("{message}\n"))
}

/// `jj status`: fold the disk into `@` without mutating anything else.
fn snapshot(root: &Path) {
    block_on(load_jj_repository_snapshot(repository(root))).expect("snapshot the working copy");
}

/// Commit ids of `revset`, in `jj log` order.
fn commit_ids(root: &Path, revset: &str) -> Vec<String> {
    let (store, ..) = block_on(load_jj_commits(
        root.to_owned(),
        revset.to_owned(),
        LoadProgress::default(),
    ))
    .unwrap_or_else(|err| panic!("load revset {revset}: {err:?}"));
    store.iter().map(|row| row.commit_id().to_owned()).collect()
}

fn commit_id(root: &Path, revset: &str) -> String {
    let mut ids = commit_ids(root, revset);
    assert_eq!(ids.len(), 1, "{revset} must name one revision: {ids:?}");
    ids.remove(0)
}

fn change_id(root: &Path, revset: &str) -> String {
    let (store, ..) = block_on(load_jj_commits(
        root.to_owned(),
        revset.to_owned(),
        LoadProgress::default(),
    ))
    .unwrap_or_else(|err| panic!("load revset {revset}: {err:?}"));
    assert_eq!(store.len(), 1, "{revset} must name one revision");
    store.row(0).change_id().to_owned()
}

/// Commit ids of `revset`'s parents, unordered.
fn parent_ids(root: &Path, revset: &str) -> Vec<String> {
    commit_ids(root, &format!("parents({revset})"))
}

/// Whether `revset` still names a visible revision — `jj log -r present(x)`.
fn is_present(root: &Path, revset: &str) -> bool {
    !commit_ids(root, &format!("present({revset})")).is_empty()
}

/// The paths `revision` changes against its parents — `jj diff --summary`.
fn diff_paths(root: &Path, revision: RevisionSelection) -> Vec<String> {
    let (document, _details) =
        block_on(load_jj_diff(repository(root), revision)).expect("load the revision's diff");
    document.files.into_iter().map(|file| file.path).collect()
}

/// Whether any revision in `revset` is flagged divergent.
fn any_divergent(root: &Path, revset: &str) -> bool {
    let (store, ..) = block_on(load_jj_commits(
        root.to_owned(),
        revset.to_owned(),
        LoadProgress::default(),
    ))
    .expect("load commits");
    store.iter().any(|row| row.is_divergent())
}

fn run(root: &Path, op: MutationOp) -> mutations::MutationOutcome {
    block_on(run_mutation(
        repository(root),
        op,
        LoadProgress::default(),
        false,
    ))
    .expect("mutation succeeds")
}

/// base → sideA / sideB (both rewrite the same line) → `@` = conflicted merge.
fn build_conflicted_merge(root: &Path) {
    write(root, "file.txt", "line1\nline2\nline3\n");
    commit(root, "base");
    let base = commit_id(root, "@-");
    write(root, "file.txt", "line1\nSIDE-A\nline3\n");
    commit(root, "sideA");
    let side_a = commit_id(root, "@-");
    run(
        root,
        MutationOp::New {
            parent: RevisionSelection::Commit(base),
        },
    );
    write(root, "file.txt", "line1\nSIDE-B\nline3\n");
    commit(root, "sideB");
    let side_b = commit_id(root, "@-");
    new_edit(root, &[side_a, side_b], "merge with conflict");
}

/// A conflicted merge's tree equals the merge of its parents, so the
/// parent-tree diff streams nothing — the loader must synthesize `Conflicted`
/// entries with the materialized conflict hunks instead of showing an empty
/// file list.
#[test]
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
fn divergent_change_is_flagged_on_all_its_commits() {
    let root = scratch_repo("divergent");
    write(&root, "file.txt", "hello\n");
    commit(&root, "base");
    // A childless leaf: describing a commit with descendants would rebase
    // them on both sides of the concurrent ops, making their changes
    // (correctly) divergent too and muddying the assertion below.
    let leaf_commit = new_leaf(&root, &[commit_id(&root, "@-")], "leaf");
    let leaf = change_id(&root, "description(glob:\"leaf*\")");
    harness::describe(&root, OpBase::Head, &leaf_commit, "leaf d1\n");
    harness::describe(&root, OpBase::ParentOfHead, &leaf_commit, "leaf d2\n");
    // The next load reconciles the concurrent ops into divergence.

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
fn hidden_copy_is_flagged_with_its_change_offset() {
    let root = scratch_repo("hidden-offset");
    write(&root, "file.txt", "hello\n");
    commit(&root, "one");
    let change = change_id(&root, "@-");
    let old_commit = commit_id(&root, "@-");
    // Rewrite the commit: the change id keeps pointing at the new commit,
    // the old one becomes hidden.
    run(
        &root,
        MutationOp::Describe {
            target: RevisionSelection::Commit(old_commit.clone()),
            description: "one v2\n".to_owned(),
        },
    );
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
fn describe_mutation_replaces_the_full_message_without_moving_working_copy() {
    let root = scratch_repo("describe-mutation");
    write(&root, "file.txt", "hello\n");
    commit(&root, "old description");
    let target = commit_id(&root, "@-");
    let working_copy_change = change_id(&root, "@");
    let description = "subject\n\nmultiline body";

    let outcome = block_on(run_mutation(
        repository(&root),
        MutationOp::Describe {
            target: RevisionSelection::Commit(target.clone()),
            description: description.to_owned(),
        },
        LoadProgress::default(),
        false,
    ))
    .expect("describe mutation");

    assert!(!outcome.moved_working_copy);
    assert_eq!(change_id(&root, "@"), working_copy_change);
    let rewritten = outcome.rewritten_commit.expect("rewritten commit id");
    assert_ne!(rewritten, target, "describe rewrites the commit");
    assert_eq!(harness::full_description(&root, &rewritten), description);
}

/// Opening a secondary workspace (`jj workspace add`) must resolve `@` to
/// *that workspace's* working copy, read op heads through the `.jj/repo`
/// pointer file, and label the other workspace's working copy `name@` in the
/// primary view.
#[test]
fn secondary_workspace_resolves_and_labels() {
    let root = scratch_repo("workspace");
    write(&root, "file.txt", "hello\n");
    commit(&root, "base");
    let second = harness::add_workspace(&root, "second");

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
        ws_row.bookmarks().iter().any(|label| label == "second@"),
        "expected workspace chip, got {:?}",
        ws_row.bookmarks()
    );
}

/// Two workspaces, the default one holding a mega merge of the other's working
/// copy, with the default checkout left *stale*: the side workspace's snapshot
/// rebases the merge, so the default workspace's disk still holds the old tree.
/// Returns the two workspace roots.
fn stale_workspace_pair(test: &str) -> (PathBuf, PathBuf) {
    let root = scratch_repo(test);
    write(&root, "shared.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(glob:\"base*\")");
    let side = harness::add_workspace(&root, "side");
    write(&side, "side.txt", "side-work\n");
    run(
        &side,
        MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: "side work\n".to_owned(),
        },
    );
    let side_work = commit_id(&root, "description(glob:\"side work*\")");
    new_edit(&root, &[side_work, base], "mega merge");
    // Edit in the side workspace; its snapshot rebases the merge and
    // strands the default workspace's checkout on the old tree.
    write(&side, "side.txt", "side-work\nside-more\n");
    snapshot(&side);
    (root, side)
}

/// Two workspaces with the default one holding a mega merge of the other's
/// working copy — the setup where snapshotting used to corrupt history.
/// Editing in the side workspace amends its `@`, auto-rebasing the merge and
/// leaving the default workspace's checkout *stale*; the old snapshot path
/// then amended the rebased merge with the old on-disk tree, silently
/// reverting the side workspace's changes inside it (the jj CLI refuses this
/// state outright: "The working copy is stale"). The snapshot must instead
/// recover like `jj workspace update-stale`: materialize the rebased merge
/// onto disk, keep the merge free of smuggled changes, and preserve any
/// unsnapshotted local edits without data loss.
#[test]
fn stale_workspace_snapshot_recovers_instead_of_reverting_the_rebase() {
    // Clean default workspace: recovery is seamless — the merge follows the
    // rebase, stays empty, and the disk materializes the side edit.
    let (root, _side) = stale_workspace_pair("stale-ws-clean");
    block_on(load_jj_repository_snapshot(repository(&root))).expect("snapshot recovers");
    let merge = commit_id(&root, "description(glob:\"mega merge*\")");
    assert!(
        diff_paths(&root, RevisionSelection::Commit(merge)).is_empty(),
        "the merge must not absorb (or revert) the side workspace's edit"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("side.txt")).expect("side.txt materialized"),
        "side-work\nside-more\n",
        "the default workspace's disk follows the rebase"
    );
    assert!(
        !any_divergent(&root, "all()"),
        "a clean recovery must not diverge anything"
    );

    // Unsnapshotted local edits in the stale workspace: jj-CLI recovery
    // parity — the edit survives in the op graph (as a divergent copy of
    // the merge change) rather than being clobbered or smuggled.
    let (root, _side) = stale_workspace_pair("stale-ws-dirty");
    write(&root, "shared.txt", "base\nlocal-edit\n");
    block_on(load_jj_repository_snapshot(repository(&root))).expect("snapshot recovers");
    assert!(
        diff_paths(&root, RevisionSelection::WorkingCopy).is_empty(),
        "@ lands on the rebased merge, still free of smuggled changes"
    );
    let preserved: Vec<String> = commit_ids(&root, "all()")
        .iter()
        .filter(|id| harness::file_at_or_absent(&root, id, "shared.txt") == "base\nlocal-edit\n")
        .cloned()
        .collect();
    assert!(
        !preserved.is_empty(),
        "the local edit must survive somewhere visible"
    );
}

/// The same stale checkout, reached through a *mutation* instead of a refresh.
/// Snapshots are deferred while a mutation runs, so a mutation can be the first
/// thing to touch a stale workspace. Folding the disk into `@` without the
/// freshness check rewrites the rebased merge with the pre-rebase tree, which
/// reverts the side workspace's edit inside it; the mutation has to recover
/// like `jj workspace update-stale` first and apply on top of that.
#[test]
fn stale_workspace_mutation_recovers_instead_of_reverting_the_rebase() {
    let (root, _side) = stale_workspace_pair("stale-ws-mutation");

    run(
        &root,
        MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: "described merge".to_owned(),
        },
    );

    let working_copy = commit_id(&root, "@");
    assert_eq!(
        harness::file_at(&root, &working_copy, "side.txt"),
        "side-work\nside-more\n",
        "the merge keeps the side workspace's edit instead of reverting it"
    );
    assert!(
        diff_paths(&root, RevisionSelection::WorkingCopy).is_empty(),
        "@ lands on the rebased merge, still free of smuggled changes"
    );
    assert_eq!(
        harness::full_description(&root, &working_copy),
        "described merge",
        "the mutation applies after the recovery, not instead of it"
    );
    assert!(
        !any_divergent(&root, "all()"),
        "a clean recovery must not diverge anything"
    );
}

/// The source browser's two backends against a real repo: the working copy
/// lists the on-disk directory — tracked files plus classified untracked /
/// ignored ones, with ignored dirs collapsed unenumerated — while a commit
/// lists exactly its tree; reads come from the right side (disk vs tree).
#[test]
fn source_browser_lists_and_reads_working_copy_and_commits() {
    let root = scratch_repo("source-browse");
    write(&root, ".gitignore", "/target/\n*.log\n");
    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    write(&root, "src/main.rs", "fn main() {}\n");
    write(&root, "README.md", "hello\n");
    commit(&root, "base");
    let base_commit = commit_id(&root, "@-");

    // Rewrite a tracked file, then lay down ignored + untracked content
    // *after* the last jj op so nothing snapshots them into the tree.
    write(&root, "src/main.rs", "fn main() { println!(\"v2\"); }\n");
    snapshot(&root); // snapshots the edit into @
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
    let children = list_ignored_dir(&repository(&root), "target").expect("list ignored dir");
    assert_eq!(children.len(), 1, "one level only: {children:#?}");
    assert_eq!(children[0].path, "target/debug");
    assert!(children[0].is_dir);
    assert_eq!(children[0].status, SourceEntryStatus::Ignored);
    let nested =
        list_ignored_dir(&repository(&root), "target/debug").expect("list nested ignored dir");
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
fn rebase_revision_moves_a_leaf_onto_the_destination() {
    let root = scratch_repo("rebase-onto");
    write(&root, "file.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "@-");
    write(&root, "file.txt", "base\nx\n");
    commit(&root, "x");
    let x = commit_id(&root, "description(exact:\"x\\n\")");
    new_leaf(&root, std::slice::from_ref(&base), "y");
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
fn rebase_between_inserts_into_the_gap() {
    let root = scratch_repo("rebase-between");
    write(&root, "file.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\nmid\n");
    commit(&root, "mid");
    let mid_change = change_id(&root, "description(exact:\"mid\\n\")");
    new_leaf(&root, std::slice::from_ref(&base), "leaf");
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
fn rebase_with_descendants_moves_the_subtree() {
    let root = scratch_repo("rebase-descendants");
    write(&root, "file.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\ns1\n");
    commit(&root, "s1");
    write(&root, "file.txt", "base\ns1\ns2\n");
    commit(&root, "s2");
    let s1 = commit_id(&root, "description(exact:\"s1\\n\")");
    let s1_change = change_id(&root, "description(exact:\"s1\\n\")");
    let s2_change = change_id(&root, "description(exact:\"s2\\n\")");
    new_leaf(&root, std::slice::from_ref(&base), "dest");
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
fn rebase_branch_moves_the_whole_branch_from_its_fork_point() {
    let root = scratch_repo("rebase-branch");
    write(&root, "file.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "base\nmain1\n");
    commit(&root, "main1");
    let main1 = commit_id(&root, "description(exact:\"main1\\n\")");
    // Feature branch forked at base: f1 → f2.
    new_edit(&root, std::slice::from_ref(&base), "f1");
    write(&root, "feature.txt", "f1\n");
    new_edit(&root, &[], "f2");
    write(&root, "feature.txt", "f1\nf2\n");
    new_edit(&root, &[], "wc off branch");
    let f1_change = change_id(&root, "description(exact:\"f1\\n\")");
    let f1 = commit_id(&root, "description(exact:\"f1\\n\")");
    let f2 = commit_id(&root, "description(exact:\"f2\\n\")");
    let f2_change = change_id(&root, "description(exact:\"f2\\n\")");

    // The preview resolves and names the branch: entry point = the fork
    // root (f1), moved set = the whole branch — even though the *head* was
    // picked. That's what the op bar shows and the sidebar washes.
    let preview = block_on(run_rebase_preview(
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
    let empty = block_on(run_rebase_preview(
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
fn squash_into_parent_folds_changes_and_descriptions() {
    let root = scratch_repo("squash-parent");
    write(&root, "file.txt", "one\n");
    commit(&root, "base message");
    let base_change = change_id(&root, "description(glob:\"base*\")");
    write(&root, "file.txt", "one\ntwo\n");
    commit(&root, "child message");
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
        harness::file_at(&root, &commit_id(&root, &base_change), "file.txt"),
        "one\ntwo\n"
    );
    // ...and the joined descriptions.
    assert_eq!(
        harness::full_description(&root, &commit_id(&root, &base_change)),
        "base message\n\nchild message\n"
    );
    // The emptied source is gone from the visible set.
    assert!(!is_present(&root, &child_change));
    // Selection follows the rewritten destination.
    assert_eq!(
        outcome.rewritten_commit.expect("squash rewrites the dest"),
        commit_id(&root, &base_change)
    );
}

/// Squash into an arbitrary (non-parent) revision on a sibling branch.
#[test]
fn squash_into_arbitrary_revision() {
    let root = scratch_repo("squash-into");
    write(&root, "a.txt", "a\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "b.txt", "b\n");
    commit(&root, "source");
    let source = commit_id(&root, "description(exact:\"source\\n\")");
    new_leaf(&root, std::slice::from_ref(&base), "dest");
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
        harness::file_at(&root, &commit_id(&root, &dest_change), "b.txt"),
        "b\n"
    );
}

/// `jj squash --from a --from b --into c`: the draft's add-source path folds
/// several revisions into one destination in a single op; a parent-target
/// squash with several sources is refused (whose parent?).
#[test]
fn squash_multiple_sources_into_one_destination() {
    let root = scratch_repo("squash-multi");
    write(&root, "base.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "a.txt", "a\n");
    commit(&root, "src a");
    let src_a = commit_id(&root, "description(glob:\"src a*\")");
    run(
        &root,
        MutationOp::New {
            parent: RevisionSelection::Commit(base.clone()),
        },
    );
    write(&root, "b.txt", "b\n");
    commit(&root, "src b");
    let src_b = commit_id(&root, "description(glob:\"src b*\")");
    new_leaf(&root, std::slice::from_ref(&base), "dest");
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
        harness::file_at(&root, &commit_id(&root, &dest_change), "a.txt"),
        "a\n"
    );
    assert_eq!(
        harness::file_at(&root, &commit_id(&root, &dest_change), "b.txt"),
        "b\n"
    );
    // …with all three descriptions joined.
    assert_eq!(
        harness::full_description(&root, &commit_id(&root, &dest_change)),
        "dest\n\nsrc a\n\nsrc b\n"
    );

    // Parent target + several sources is ambiguous and refused.
    let result = block_on(run_mutation(
        repository(&root),
        MutationOp::Squash {
            from: vec![
                RevisionSelection::Commit(src_a),
                RevisionSelection::Commit(src_b),
            ],
            into: SquashTarget::Parent,
        },
        LoadProgress::default(),
        false,
    ));
    let error = result.expect_err("multi-source parent squash must fail");
    assert!(
        error.to_string().contains("explicit destination"),
        "got: {error}"
    );
}

/// `jj new A B`: the merge draft's confirm creates a child of both picked
/// revisions and moves `@` onto it.
#[test]
fn merge_creates_a_child_of_both_parents() {
    let root = scratch_repo("merge-two");
    write(&root, "base.txt", "base\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "x.txt", "x\n");
    commit(&root, "x");
    let x = commit_id(&root, "description(exact:\"x\\n\")");
    new_leaf(&root, std::slice::from_ref(&base), "y");
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
    assert_eq!(
        harness::file_at(&root, &commit_id(&root, "@"), "x.txt"),
        "x\n"
    );
    assert_eq!(
        harness::file_at(&root, &commit_id(&root, "@"), "base.txt"),
        "base\n"
    );

    // A merge with one distinct parent is refused.
    let result = block_on(run_mutation(
        repository(&root),
        MutationOp::Merge {
            parents: vec![
                RevisionSelection::Commit(base.clone()),
                RevisionSelection::Commit(base.clone()),
            ],
        },
        LoadProgress::default(),
        false,
    ));
    let error = result.expect_err("self-merge must fail");
    assert!(
        error.to_string().contains("two distinct parents"),
        "got: {error}"
    );

    // Octopus: the draft's add-parent path sends all stacked parents in one
    // op — three distinct parents make a three-way merge commit.
    new_leaf(&root, std::slice::from_ref(&base), "z");
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
fn merge_preview_lists_conflicting_paths() {
    let root = scratch_repo("merge-preview");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "line1\nSIDE-A\nline3\n");
    commit(&root, "sideA");
    let side_a = commit_id(&root, "description(exact:\"sideA\\n\")");
    run(
        &root,
        MutationOp::New {
            parent: RevisionSelection::Commit(base.clone()),
        },
    );
    write(&root, "file.txt", "line1\nSIDE-B\nline3\n");
    commit(&root, "sideB");
    let side_b = commit_id(&root, "description(exact:\"sideB\\n\")");

    let head_before = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head before preview");

    let preview = block_on(run_merge_preview(
        repository(&root),
        vec![
            RevisionSelection::Commit(side_a),
            RevisionSelection::Commit(side_b.clone()),
        ],
    ))
    .expect("conflicting preview succeeds");
    assert_eq!(preview.conflicts, vec!["file.txt".to_owned()]);
    assert!(!preview.truncated);

    let clean = block_on(run_merge_preview(
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
fn duplicate_creates_a_sibling_copy() {
    let root = scratch_repo("duplicate");
    write(&root, "file.txt", "one\n");
    commit(&root, "base");
    write(&root, "file.txt", "one\ntwo\n");
    commit(&root, "orig");
    let orig = commit_id(&root, "description(exact:\"orig\\n\")");

    let outcome = run(
        &root,
        MutationOp::Duplicate {
            target: RevisionSelection::Commit(orig.clone()),
        },
    );

    let copies = commit_ids(&root, "description(exact:\"orig\\n\")");
    let copies: Vec<&str> = copies.iter().map(String::as_str).collect();
    assert_eq!(copies.len(), 2, "original + duplicate: {copies:?}");
    let duplicate = outcome.rewritten_commit.expect("duplicate id reported");
    assert_ne!(duplicate, orig);
    assert!(copies.contains(&duplicate.as_str()));
    assert!(copies.contains(&orig.as_str()));
    // Same parents, same tree.
    assert_eq!(parent_ids(&root, &duplicate), parent_ids(&root, &orig));
    assert_eq!(
        harness::file_at(&root, &commit_id(&root, &duplicate), "file.txt"),
        "one\ntwo\n"
    );
}

/// `jj absorb`: the working copy's hunk lands in the ancestor that last
/// touched those lines, and the emptied working copy is discarded.
#[test]
fn absorb_moves_hunks_into_the_touching_ancestor() {
    let root = scratch_repo("absorb");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    commit(&root, "base");
    let base_change = change_id(&root, "description(exact:\"base\\n\")");
    // Edit an existing line in the working copy — annotate attributes it to
    // the base commit, so absorb folds it there.
    write(&root, "file.txt", "line1\nline2 edited\nline3\n");
    snapshot(&root);

    let outcome = run(
        &root,
        MutationOp::Absorb {
            from: RevisionSelection::WorkingCopy,
        },
    );

    assert_eq!(
        harness::file_at(&root, &commit_id(&root, &base_change), "file.txt"),
        "line1\nline2 edited\nline3\n"
    );
    // Emptiness needs the parent's tree in the same load, so walk the graph
    // rather than `@` alone.
    let (store, ..) = block_on(load_jj_commits(
        root.clone(),
        "all()".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load commits");
    assert_eq!(
        store.working_copy().and_then(|row| row.is_empty()),
        Some(true),
        "the source is emptied"
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
fn undo_operation_reverts_a_specific_mutation() {
    let root = scratch_repo("undo-op");
    write(&root, "file.txt", "one\n");
    commit(&root, "victim");
    let victim_change = change_id(&root, "description(exact:\"victim\\n\")");
    let victim = commit_id(&root, "description(exact:\"victim\\n\")");

    let outcome = run(
        &root,
        MutationOp::Abandon {
            targets: vec![RevisionSelection::Commit(victim)],
        },
    );
    assert!(
        !is_present(&root, &victim_change),
        "the abandon must hide the commit first"
    );

    let op_id = outcome.operation_id.expect("mutations record their op id");
    run(
        &root,
        MutationOp::Undo {
            operation_id: Some(op_id),
        },
    );

    assert!(
        is_present(&root, &victim_change),
        "undoing the abandon brings the commit back"
    );
}

/// Undo with unsnapshotted on-disk edits must not lose them: the pipeline
/// folds the working copy into `@` (as a standalone snapshot op) before
/// reverting, so edits survive on disk when `@` stays put — and stay
/// reachable through the op log when the undo moves `@` out from under them.
#[test]
fn undo_preserves_unsnapshotted_working_copy_edits() {
    let root = scratch_repo("undo-dirty-wc");
    write(&root, "file.txt", "base\n");
    commit(&root, "base");

    // Undoing a describe leaves `@` in place: the dirty file must be
    // untouched on disk and folded into `@`.
    let outcome = run(
        &root,
        MutationOp::Describe {
            target: RevisionSelection::WorkingCopy,
            description: "victim description".to_owned(),
        },
    );
    let op_id = outcome.operation_id.expect("mutations record their op id");
    write(&root, "file.txt", "precious unsnapshotted edit\n");
    run(
        &root,
        MutationOp::Undo {
            operation_id: Some(op_id),
        },
    );

    let on_disk = std::fs::read_to_string(root.join("file.txt")).expect("file still readable");
    assert_eq!(
        on_disk, "precious unsnapshotted edit\n",
        "undo must not overwrite unsnapshotted edits"
    );
    let diff = diff_paths(&root, RevisionSelection::WorkingCopy);
    assert!(
        diff.iter().any(|path| path == "file.txt"),
        "the pre-undo snapshot folds the edit into @: {diff:?}"
    );

    // Undoing a `new` moves `@` back, so the checkout rewrites the file on
    // disk — but the doomed edit must remain reachable via the snapshot op.
    let outcome = run(
        &root,
        MutationOp::New {
            parent: RevisionSelection::WorkingCopy,
        },
    );
    let new_op = outcome.operation_id.expect("mutations record their op id");
    write(&root, "file.txt", "edit made on the doomed working copy\n");
    run(
        &root,
        MutationOp::Undo {
            operation_id: Some(new_op),
        },
    );

    let (_, snapshot_op) = harness::operation_log(&root)
        .into_iter()
        .find(|(description, _)| description == "snapshot working copy")
        .expect("the undo committed its fold as a snapshot op");
    let at_snapshot = harness::file_at_operation(&root, &snapshot_op, "file.txt");
    assert_eq!(
        at_snapshot, "edit made on the doomed working copy\n",
        "the doomed edit stays reachable through the op log"
    );
}

/// Rewrites of immutable commits are refused, honoring the repo's configured
/// `immutable_heads()` override.
#[test]
fn immutable_commits_refuse_rebase() {
    let root = scratch_repo("immutable-guard");
    write(&root, "file.txt", "one\n");
    commit(&root, "protected base");
    // The repo-level config layer, which `harness::test_settings_for` folds in
    // on top of the fixture settings — the same rank jj gives it.
    std::fs::write(
        root.join(".jj/repo/config.toml"),
        "[revset-aliases]\n'immutable_heads()' = 'description(glob:\"protected*\")'\n",
    )
    .expect("write repo config");
    let protected = commit_id(&root, "description(glob:\"protected*\")");
    let wc = commit_id(&root, "@");

    let result = block_on(run_mutation(
        repository(&root),
        MutationOp::Rebase {
            mode: RebaseSourceMode::Revisions,
            sources: vec![RevisionSelection::Commit(protected)],
            destination: Destination::Onto(RevisionSelection::Commit(wc)),
        },
        LoadProgress::default(),
        false,
    ));

    let error = result.expect_err("rebasing an immutable commit must fail");
    assert!(
        matches!(&error, RepoError::Immutable { .. }),
        "error names immutability: {error}"
    );
    // The rejection is typed, naming the refused commit — that's what lets
    // the frontend raise its confirm-and-rerun dialog instead of a dead end.
    let RepoError::Immutable { short_id } = error else {
        unreachable!("just asserted");
    };
    assert!(!short_id.is_empty());
}

/// Describe and abandon refuse immutable targets like the CLI does (they used
/// to rewrite them silently), and `allow_immutable` — the confirm dialog's
/// accept — overrides the guard like `jj --ignore-immutable`.
#[test]
fn immutable_guard_covers_describe_and_honors_override() {
    let root = scratch_repo("immutable-describe");
    write(&root, "a.txt", "a\n");
    commit(&root, "protected base");
    std::fs::write(
        root.join(".jj/repo/config.toml"),
        "[revset-aliases]\n'immutable_heads()' = 'description(glob:\"protected*\")'\n",
    )
    .expect("write repo config");
    let protected = commit_id(&root, "description(glob:\"protected*\")");

    let describe = |allow: bool| {
        block_on(run_mutation(
            repository(&root),
            MutationOp::Describe {
                target: RevisionSelection::Commit(protected.clone()),
                description: "renamed anyway".to_owned(),
            },
            LoadProgress::default(),
            allow,
        ))
    };

    let error = describe(false).expect_err("describing an immutable commit must fail");
    assert!(
        matches!(&error, RepoError::Immutable { .. }),
        "typed rejection expected: {error}"
    );
    // Nothing was rewritten by the refused attempt.
    assert_eq!(
        harness::full_description(&root, &commit_id(&root, "description(glob:\"protected*\")")),
        "protected base\n"
    );

    let abandon = block_on(run_mutation(
        repository(&root),
        MutationOp::Abandon {
            targets: vec![RevisionSelection::Commit(protected.clone())],
        },
        LoadProgress::default(),
        false,
    ));
    assert!(matches!(
        abandon.expect_err("abandoning an immutable commit must fail"),
        RepoError::Immutable { .. }
    ));

    describe(true).expect("override rewrites the immutable commit");
    assert_eq!(
        harness::full_description(&root, &commit_id(&root, "description(glob:\"renamed*\")")),
        "renamed anyway"
    );
}

/// A conflicted bookmark (concurrent moves — the same state a force-pushed
/// origin leaves behind): every side's row wears the `name??` chip, lookups
/// by that label resolve deterministically to the first displayed side, the
/// context-menu table reports the conflict, and using the bare name as a
/// revset fails with a hint naming the escape hatches instead of a dead end.
#[test]
fn conflicted_bookmark_is_marked_and_revset_error_hints() {
    let root = scratch_repo("conflicted-bookmark");
    write(&root, "f.txt", "a\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(glob:\"base*\")");
    let side_a = new_leaf(&root, &[commit_id(&root, "@-")], "sideA");
    let side_b = new_leaf(&root, &[commit_id(&root, "@-")], "sideB");
    harness::set_bookmark(&root, OpBase::Head, "development", &side_a);
    harness::set_bookmark(&root, OpBase::Head, "development", &side_b);
    // Concurrent with the move to sideB, so reconciling the two leaves the
    // bookmark conflicted — the state a force-pushed origin produces.
    harness::set_bookmark(&root, OpBase::ParentOfHead, "development", &base);

    let (store, _graph, _branch, bookmarks) = block_on(load_jj_commits(
        root.clone(),
        "all()".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load commits");

    let marked: Vec<&str> = store
        .iter()
        .filter(|row| row.bookmarks().iter().any(|b| b == "development??"))
        .map(|row| row.description())
        .collect();
    assert_eq!(
        marked,
        vec!["sideB", "base"],
        "every conflict side wears the ?? chip, in display order"
    );
    assert_eq!(
        store
            .find_by_bookmark("development??")
            .map(|row| row.description()),
        Some("sideB"),
        "label lookups resolve to the first displayed side"
    );

    let entry = bookmarks
        .bookmarks
        .iter()
        .find(|b| b.name == "development")
        .expect("menu table lists the bookmark");
    assert!(entry.is_conflicted());
    assert_eq!(entry.local_targets.len(), 2, "both sides are carried");

    // The bare name as a revset is a real jj error (a conflicted name isn't
    // one revision) — but it must arrive with its ways forward attached.
    let error = format!(
        "{:#}",
        block_on(load_jj_commits(
            root.clone(),
            "development".to_owned(),
            LoadProgress::default(),
        ))
        .expect_err("conflicted name as revset must fail")
    );
    assert!(
        error.contains("is conflicted"),
        "names the problem: {error}"
    );
    assert!(
        error.contains("bookmarks(exact:\"development\")"),
        "hints the select-all escape hatch: {error}"
    );
}

/// The log walk flags rows in `immutable()` (honoring an `immutable_heads()`
/// override), which is what the frontend's pre-flight dialog reads.
#[test]
fn log_rows_carry_the_immutable_flag() {
    let root = scratch_repo("immutable-log-flag");
    write(&root, "a.txt", "a\n");
    commit(&root, "protected base");
    write(&root, "a.txt", "b\n");
    commit(&root, "mutable child");
    std::fs::write(
        root.join(".jj/repo/config.toml"),
        "[revset-aliases]\n'immutable_heads()' = 'description(glob:\"protected*\")'\n",
    )
    .expect("write repo config");

    let (store, _graph, _branch, _bookmarks) = block_on(load_jj_commits(
        root.clone(),
        "all()".to_owned(),
        LoadProgress::default(),
    ))
    .expect("load commits");

    let immutable: Vec<&str> = store
        .iter()
        .filter(|row| row.is_immutable())
        .map(|row| row.description())
        .collect();
    // The protected head and the root commit (immutable() is ancestor-closed
    // and includes root()) — the mutable child and `@` stay unflagged.
    assert!(
        immutable.contains(&"protected base"),
        "protected head flagged, got {immutable:?}"
    );
    assert!(
        !immutable.contains(&"mutable child"),
        "mutable child unflagged, got {immutable:?}"
    );
    assert!(
        store
            .working_copy()
            .is_some_and(|working_copy| !working_copy.is_immutable()),
        "@ stays mutable"
    );
}

/// The rebase preview predicts conflicts without touching the repo's visible
/// state: the op log head must not move, and the conflicting change is named.
#[test]
fn rebase_preview_predicts_conflicts_without_mutating() {
    let root = scratch_repo("rebase-preview");
    write(&root, "file.txt", "line1\nline2\nline3\n");
    commit(&root, "base");
    let base = commit_id(&root, "description(exact:\"base\\n\")");
    write(&root, "file.txt", "line1\nSIDE-A\nline3\n");
    commit(&root, "sideA");
    let side_a = commit_id(&root, "description(exact:\"sideA\\n\")");
    run(
        &root,
        MutationOp::New {
            parent: RevisionSelection::Commit(base.clone()),
        },
    );
    write(&root, "file.txt", "line1\nSIDE-B\nline3\n");
    commit(&root, "sideB");
    let side_b = commit_id(&root, "description(exact:\"sideB\\n\")");
    let side_b_change = change_id(&root, "description(exact:\"sideB\\n\")");

    let head_before = block_on(diffui_core::jj::read_jj_op_head(repository(&root)))
        .expect("read op head before preview");

    let preview = block_on(run_rebase_preview(
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
    let clean = block_on(run_rebase_preview(
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
fn snapshot_parent_fingerprint_detects_external_ops() {
    let root = scratch_repo("external-op");
    write(&root, "file.txt", "hello\n");
    commit(&root, "base");

    // Quiet tree: the snapshot writes no op and is its own base.
    let first = block_on(load_jj_repository_snapshot(repository(&root))).expect("first snapshot");
    assert_eq!(
        first.parent_fingerprint.as_deref(),
        Some(first.fingerprint.as_str()),
        "no-op snapshot is its own parent"
    );

    // An external op (CLI `jj new`) plus a worktree edit in the same window —
    // the case that used to be swallowed as a diff-only refresh.
    new_edit(&root, &[], "external op");
    write(&root, "file.txt", "hello edited\n");

    let second = block_on(load_jj_repository_snapshot(repository(&root))).expect("second snapshot");
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

/// Batch abandon: several picked revisions vanish in a single mutation (the
/// context menu's multi-select "Abandon N revisions"), and the working copy
/// re-parents across the hole.
#[test]
fn abandon_discards_multiple_revisions_at_once() {
    let root = scratch_repo("abandon-multi");
    write(&root, "a.txt", "a\n");
    commit(&root, "keep");
    write(&root, "b.txt", "b\n");
    commit(&root, "victim1");
    write(&root, "c.txt", "c\n");
    commit(&root, "victim2");
    let keep = commit_id(&root, "description(exact:\"keep\\n\")");
    let victim1 = commit_id(&root, "description(exact:\"victim1\\n\")");
    let victim1_change = change_id(&root, "description(exact:\"victim1\\n\")");
    let victim2 = commit_id(&root, "description(exact:\"victim2\\n\")");
    let victim2_change = change_id(&root, "description(exact:\"victim2\\n\")");

    let outcome = run(
        &root,
        MutationOp::Abandon {
            targets: vec![
                RevisionSelection::Commit(victim1),
                RevisionSelection::Commit(victim2),
            ],
        },
    );
    assert_eq!(outcome.message, "Abandoned 2 revisions");

    for change in [&victim1_change, &victim2_change] {
        assert!(
            !is_present(&root, change),
            "an abandoned revision must be hidden"
        );
    }
    assert_eq!(
        parent_ids(&root, "@"),
        vec![keep],
        "@ re-parents onto the survivor"
    );
}

/// Move-and-push in one mutation (the context menu's "Move bookmark here &
/// push"): the local bookmark, its remote-tracking ref, and the remote itself
/// all land on the target.
#[test]
fn move_bookmark_with_push_updates_the_remote() {
    // jj-lib's push shells out to `git push`, so this one scenario needs the
    // git CLI — and skips without it, like the git suite does.
    if !harness::git_available() {
        eprintln!("skipping: the git CLI is not on PATH");
        return;
    }
    let root = scratch_repo("move-push");
    // A bare git repo on disk is a perfectly good `jj git push` remote.
    let remote = std::env::temp_dir().join("diffui-actor-move-push-remote.git");
    harness::init_git_remote(&root, "origin", &remote);

    write(&root, "file.txt", "one\n");
    commit(&root, "first");
    let first = commit_id(&root, "@-");
    run(
        &root,
        MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::Commit(first),
            push_remote: None,
        },
    );

    write(&root, "file.txt", "two\n");
    commit(&root, "second");
    let target = commit_id(&root, "description(exact:\"second\\n\")");

    let outcome = run(
        &root,
        MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::Commit(target.clone()),
            push_remote: Some("origin".to_owned()),
        },
    );
    assert!(
        outcome.message.contains("Pushed main to origin"),
        "message should report the push: {}",
        outcome.message
    );

    assert_eq!(commit_id(&root, "main"), target, "local bookmark moved");
    assert_eq!(
        commit_id(&root, "main@origin"),
        target,
        "remote-tracking ref follows the push"
    );
    assert_eq!(
        harness::run_git(&remote, &["rev-parse", "refs/heads/main"]).trim(),
        target,
        "the remote itself received the new position"
    );
}
