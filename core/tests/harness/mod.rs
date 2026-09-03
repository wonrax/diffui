//! Driving a repository actor from a test.
//!
//! The actor is asynchronous by construction — commands in, events out — so a
//! test that wants one answer has to run the stream until the job it asked
//! about reaches a terminal event. [`TestRepo`] is that loop, plus the
//! jj-lib-only fixture builder the scenarios construct their repos with (no
//! `jj` CLI, so these run anywhere the crate builds).

// Shared by both integration-test binaries; each uses a different subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use diffui_core::repo::{self, Command, JobId, OpenSpec, Payload, RepoHandle};
use diffui_core::{Repository, RevisionSelection, Vcs};
use futures::StreamExt;

pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(future)
}

/// A scratch jj repo built through jj-lib alone, at a deterministic per-test
/// path wiped on entry so reruns start from nothing.
pub fn scratch_repo(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("diffui-actor-{test}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch dir");
    let settings = test_settings();
    block_on(async {
        jj_lib::workspace::Workspace::init_internal_git(&settings, &root)
            .await
            .expect("init jj workspace");
    });
    root
}

/// Settings with an identity and signing off, so a commit written here never
/// reaches for the user's real config or a signing agent.
pub fn test_settings() -> jj_lib::settings::UserSettings {
    let mut config = jj_lib::config::StackedConfig::with_defaults();
    let layer = jj_lib::config::ConfigLayer::parse(
        jj_lib::config::ConfigSource::User,
        r#"
[user]
name = "Scenario Test"
email = "scenario@example.com"
[signing]
behavior = "keep"
[operation]
hostname = "test"
username = "test"
"#,
    )
    .expect("parse test config");
    config.add_layer(layer);
    jj_lib::settings::UserSettings::from_config(config).expect("build test settings")
}

pub fn write(dir: &Path, name: &str, contents: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create scratch dir");
    }
    std::fs::write(path, contents).expect("write scratch file");
}

/// Commit ids of the repo's visible heads' ancestry, newest first — enough for
/// a test to name a revision without shelling out to `jj log`.
pub fn commit_ids(root: &Path) -> Vec<String> {
    let settings = test_settings();
    block_on(async {
        let workspace = jj_lib::workspace::Workspace::load(
            &settings,
            root,
            &jj_lib::repo::StoreFactories::default(),
            &jj_lib::workspace::default_working_copy_factories(),
        )
        .expect("load workspace");
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load repo at head");
        let expr = jj_lib::revset::RevsetExpression::all();
        let resolver = jj_lib::revset::SymbolResolver::new(
            repo.as_ref(),
            &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
        );
        let resolved = expr
            .resolve_user_expression(repo.as_ref(), &resolver)
            .expect("resolve all()");
        resolved
            .evaluate(repo.as_ref())
            .expect("evaluate all()")
            .iter()
            .map(|entry| {
                use jj_lib::object_id::ObjectId;
                entry.expect("walk all()").hex()
            })
            .collect()
    })
}

/// One open repository, driven synchronously.
pub struct TestRepo {
    pub root: PathBuf,
    pub handle: RepoHandle,
    events: std::pin::Pin<Box<repo::RepoEvents>>,
    runtime: tokio::runtime::Runtime,
    /// Events that arrived while waiting for some other job.
    parked: Vec<diffui_core::Event>,
}

impl TestRepo {
    pub fn open(root: &Path) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let mut events = Box::pin(repo::open(OpenSpec::Local {
            root: root.to_owned(),
            scope: PathBuf::new(),
        }));
        let handle = runtime.block_on(async {
            match events.next().await.map(|event| event.payload) {
                Some(Payload::Ready { handle, .. }) => handle,
                other => panic!("expected the actor's Ready event, got {other:?}"),
            }
        });
        Self {
            root: root.to_owned(),
            handle,
            events,
            runtime,
            parked: Vec::new(),
        }
    }

    pub fn repository(&self) -> Repository {
        Repository {
            root: self.root.clone(),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        }
    }

    /// Send `command` and collect every event for its job up to and including
    /// the terminal one.
    pub fn run(&mut self, make: impl FnOnce(JobId) -> Command) -> Vec<Payload> {
        let job = self.next_job();
        self.handle.send(make(job));
        self.drain(job)
    }

    /// Send a command without waiting — how a test queues work behind
    /// something already running.
    pub fn send(&mut self, make: impl FnOnce(JobId) -> Command) -> JobId {
        let job = self.next_job();
        self.handle.send(make(job));
        job
    }

    /// Run the stream until `job` reaches its terminal event, parking anything
    /// that belongs to another job.
    pub fn drain(&mut self, job: JobId) -> Vec<Payload> {
        let mut collected = Vec::new();
        let mut remaining = Vec::new();
        for event in std::mem::take(&mut self.parked) {
            if event.job() == Some(job) {
                let terminal = event.payload.is_terminal();
                collected.push(event.payload);
                if terminal {
                    self.parked = remaining;
                    return collected;
                }
            } else {
                remaining.push(event);
            }
        }
        self.parked = remaining;
        loop {
            let event = self
                .runtime
                .block_on(self.events.next())
                .expect("the actor's event stream ended early");
            if event.job() != Some(job) {
                self.parked.push(event);
                continue;
            }
            let terminal = event.payload.is_terminal();
            collected.push(event.payload);
            if terminal {
                return collected;
            }
        }
    }

    /// Every event seen so far that isn't `job`'s — what a test inspects to
    /// prove something *else* happened (a cancellation, an op-head move).
    pub fn parked(&self) -> &[diffui_core::Event] {
        &self.parked
    }

    fn next_job(&mut self) -> JobId {
        // The same source a `Session` mints from. A second counter would hand
        // out ids that interleave with the projection's, and the actor reads
        // them as ordered.
        JobId::next()
    }

    // ── convenience wrappers ────────────────────────────────────────────

    pub fn snapshot(&mut self) -> diffui_core::RepositorySnapshot {
        let events = self.run(|job| Command::Snapshot {
            job,
            origin: diffui_core::RefreshOrigin::Focus,
        });
        match events.into_iter().next_back() {
            Some(Payload::SnapshotDone { snapshot, .. }) => snapshot,
            other => panic!("expected SnapshotDone, got {other:?}"),
        }
    }

    pub fn graph(&mut self, revset: &str) -> Vec<diffui_core::StreamRow> {
        let revset = revset.to_owned();
        let events = self.run(|job| Command::LoadGraph { job, revset });
        let mut rows = Vec::new();
        for payload in events {
            match payload {
                Payload::Batch { rows: batch, .. } => rows.extend(batch),
                Payload::GraphLoaded { .. } | Payload::Progress { .. } => {}
                Payload::Failed { error, .. } => panic!("graph load failed: {error}"),
                other => panic!("unexpected event during a graph load: {other:?}"),
            }
        }
        rows
    }

    pub fn diff(&mut self, revision: RevisionSelection) -> diffui_core::DiffDocument {
        let events = self.run(|job| Command::LoadDiff { job, revision });
        match events.into_iter().next_back() {
            Some(Payload::DiffLoaded { document, .. }) => document,
            other => panic!("expected DiffLoaded, got {other:?}"),
        }
    }

    pub fn mutate(
        &mut self,
        op: diffui_core::MutationOp,
    ) -> Result<diffui_core::MutationOutcome, diffui_core::RepoError> {
        let events = self.run(|job| Command::Mutate {
            job,
            op,
            allow_immutable: false,
        });
        match events.into_iter().next_back() {
            Some(Payload::MutationDone { outcome, .. }) => Ok(outcome),
            Some(Payload::Failed { error, .. }) => Err(error),
            other => panic!("expected a mutation result, got {other:?}"),
        }
    }
}

/// Load the workspace at `root` through jj-lib directly, for the fixtures that
/// have to reach past the protocol to set something up.
pub fn load_workspace(root: &Path) -> jj_lib::workspace::Workspace {
    jj_lib::workspace::Workspace::load(
        &test_settings(),
        root,
        &jj_lib::repo::StoreFactories::default(),
        &jj_lib::workspace::default_working_copy_factories(),
    )
    .expect("load workspace")
}

/// Write two operations from the same base so the op log has a genuine merge
/// operation at its head, and return that operation's id.
///
/// This is what an undo has to refuse: a merge operation has two parents, so
/// there is no single "before" state to revert to. Building it needs jj-lib
/// directly — the protocol has no way to ask for concurrent operations, which
/// is rather the point.
pub fn write_merge_operation(root: &Path) -> String {
    use jj_lib::object_id::ObjectId;
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;

    block_on(async {
        let workspace = load_workspace(root);
        let loader = workspace.repo_loader().clone();
        let base = loader.load_at_head().await.expect("load repo at head");

        // Two transactions from the same base commit into divergent op heads;
        // reloading then merges them, and that merge is what we hand to undo.
        for (index, name) in ["concurrent-a", "concurrent-b"].into_iter().enumerate() {
            let mut tx = base.start_transaction();
            let target = base
                .view()
                .get_wc_commit_id(workspace.workspace_name())
                .expect("a working-copy commit")
                .clone();
            tx.repo_mut()
                .set_local_bookmark_target(RefName::new(name), RefTarget::normal(target));
            tx.commit(format!("concurrent op {index}"))
                .await
                .expect("commit concurrent op");
        }
        let merged = loader
            .load_at_head()
            .await
            .expect("reload after divergence");
        assert_eq!(
            merged
                .operation()
                .parents()
                .await
                .expect("read the merged operation's parents")
                .len(),
            2,
            "the head operation should be a merge of the two concurrent ones"
        );
        merged.op_id().hex()
    })
}

/// Add a second workspace to the repository at `root` and return its root.
///
/// Two workspaces of one repository is what makes a checkout go stale: one
/// snapshots and rebases the other's working-copy commit, leaving the second
/// synced with a commit the repo has moved past.
pub fn add_workspace(root: &Path, name: &str) -> PathBuf {
    let side = root.with_file_name(format!(
        "{}-{name}",
        root.file_name()
            .expect("a named scratch dir")
            .to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&side);
    std::fs::create_dir_all(&side).expect("create the second workspace dir");
    block_on(async {
        let workspace = load_workspace(root);
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load repo at head");
        let factories = jj_lib::workspace::default_working_copy_factories();
        let factory = factories
            .get(jj_lib::working_copy::WorkingCopy::name(
                workspace.working_copy(),
            ))
            .expect("the workspace's own working-copy factory");
        jj_lib::workspace::Workspace::init_workspace_with_existing_repo(
            &side,
            &crate::harness::repo_dir(root),
            &repo,
            factory.as_ref(),
            jj_lib::ref_name::WorkspaceNameBuf::from(name),
        )
        .await
        .expect("add the second workspace");
    });
    side
}

/// The `.jj/repo` directory backing `root`'s workspace.
pub fn repo_dir(root: &Path) -> PathBuf {
    root.join(".jj").join("repo")
}

/// Write `contents` to `name` and fold it into `@` — the fixture equivalent of
/// editing a file and letting the app snapshot it.
pub fn commit_file(repo: &mut TestRepo, name: &str, contents: &str, description: &str) {
    write(&repo.root, name, contents);
    repo.snapshot();
    repo.mutate(diffui_core::MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: description.to_owned(),
    })
    .expect("describe the working copy");
    repo.mutate(diffui_core::MutationOp::New {
        parent: RevisionSelection::WorkingCopy,
    })
    .expect("start a new change");
}
