//! A repository on another machine, end to end: the relaying actor, the
//! host's link and the serve loop, over in-memory pipes instead of an ssh
//! or wsl process. What only a real program can show (its exit code and
//! stderr when no server answers) starts a real `sh`.

use std::sync::Arc;
use std::time::Duration;

use diffui_core::remote::{ConnectError, Connector, Exit, HostId, Hosts, Launch, LinkState, wire};
use diffui_core::repo::{self, Command, OpenSpec, Payload, RepoError};
use diffui_core::{MutationOp, RevisionSelection, Vcs};
use futures::StreamExt as _;
use futures::future::BoxFuture;

use crate::harness::{self, TestRepo};

fn host() -> HostId {
    HostId("box".to_owned())
}

/// A host reached by whatever `launch` makes.
struct Launcher<F>(F, Option<&'static str>);

impl<F> Connector for Launcher<F>
where
    F: Fn() -> Result<Launch, String> + Send + Sync + 'static,
{
    fn launch(&self, _host: &HostId) -> BoxFuture<'static, Result<Launch, String>> {
        let launch = (self.0)();
        Box::pin(async move { launch })
    }

    fn explain(&self, _host: &HostId, exit: &Exit) -> BoxFuture<'static, Option<String>> {
        let explained = self.1.filter(|_| exit.code == Some(127)).map(str::to_owned);
        Box::pin(async move { explained })
    }
}

/// Open `path` on a host reached through `connector` and return what the
/// tab is told instead of `Ready`.
fn refused(connector: impl Connector, path: &str) -> ConnectError {
    harness::block_on(async {
        let hosts = Hosts::new(Arc::new(connector));
        let mut events = Box::pin(repo::open(
            OpenSpec::Remote {
                host: host(),
                path: path.to_owned(),
            },
            &Default::default(),
            &hosts,
        ));
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), events.next())
                .await
                .expect("the open should have been answered by now")
                .expect("the actor's event stream ended early");
            match event.payload {
                Payload::Connecting { .. } => {}
                Payload::Disconnected { error } => return error,
                other => panic!("expected Disconnected, got {other:?}"),
            }
        }
    })
}

#[test]
fn a_remote_repository_loads_writes_and_undoes() {
    let root = harness::scratch_repo("remote-loads-writes");
    let (mut repo, _host) = TestRepo::remote(&root);

    let remote = repo.handle.remote().expect("the handle says it is remote");
    assert_eq!(remote.host, host());
    assert_eq!(
        remote.opened.root,
        root.canonicalize().unwrap().to_string_lossy()
    );
    assert_eq!(remote.opened.vcs, Vcs::Jj);
    assert!(remote.opened.capabilities.mutate);
    assert!(
        remote
            .opened
            .default_revset
            .as_deref()
            .is_some_and(|revset| !revset.is_empty()),
        "jj's default revset comes with the answer"
    );
    assert_eq!(remote.info.os, std::env::consts::OS);

    harness::commit_file(&mut repo, "a.txt", "one\ntwo\n", "first");
    let rows = repo.graph("all()");
    let descriptions: Vec<&str> = rows
        .iter()
        .map(|row| row.summary.description.as_str())
        .collect();
    assert!(descriptions.contains(&"first"), "{descriptions:?}");

    let first = rows
        .iter()
        .find(|row| row.summary.description == "first")
        .expect("the described commit is a row");
    let document = repo.diff(RevisionSelection::Commit(first.summary.commit_id.clone()));
    assert_eq!(document.files.len(), 1);
    assert_eq!(document.files[0].path, "a.txt");

    // `commit_file` ended on a `jj new`: the undo takes that change back.
    repo.mutate(MutationOp::Undo { operation_id: None })
        .expect("undo over the wire");
    assert_eq!(repo.graph("all()").len(), rows.len() - 1);
}

#[test]
fn a_file_saved_over_there_reaches_the_app() {
    let root = harness::scratch_repo("remote-watch");
    let (mut repo, _host) = TestRepo::remote(&root);
    // Make sure the actor over there is up (its watch starts with it).
    repo.snapshot();

    // The watch starts with the server's actor, a moment after it answers:
    // keep saving until it reports rather than guess how long that is.
    let mut seen = false;
    for round in 0..40 {
        harness::write(&root, "saved.txt", &format!("round {round}\n"));
        if repo
            .try_wait_for(Duration::from_millis(500), |payload| {
                matches!(payload, Payload::WorkingCopyChanged)
            })
            .is_some()
        {
            seen = true;
            break;
        }
    }
    assert!(seen, "the save over there should have been reported");
}

#[test]
fn a_dropped_link_fails_what_is_out_and_says_so_once() {
    let root = harness::scratch_repo("remote-dropped");
    let (mut repo, host) = TestRepo::remote(&root);
    repo.snapshot();

    host.stop();
    let lost = repo
        .try_wait_for(Duration::from_secs(10), |payload| {
            matches!(payload, Payload::Disconnected { .. })
        })
        .expect("the tab is told the link went");
    let Payload::Disconnected { error: lost } = lost else {
        unreachable!()
    };
    assert!(matches!(lost, ConnectError::Lost { .. }), "{lost:?}");
    assert!(lost.is_transient());
    assert!(lost.to_string().contains("box"), "{lost}");

    // Whatever is sent now fails at once instead of waiting on a dead pipe.
    let events = repo.run(|job| Command::LoadGraph {
        job,
        revset: "all()".to_owned(),
    });
    assert!(
        matches!(
            events.as_slice(),
            [Payload::Failed {
                error: RepoError::Disconnected,
                ..
            }]
        ),
        "{events:?}"
    );
}

#[test]
fn a_path_without_a_repository_is_refused_in_the_servers_words() {
    let dir = std::env::temp_dir().join("diffui-actor-remote-no-repo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let server = harness::InProcessHost::new(Default::default());
    let error = refused(server, &dir.to_string_lossy());
    let ConnectError::Refused(message) = &error else {
        panic!("expected Refused, got {error:?}");
    };
    assert!(
        message.contains("could not find a jj or git repository"),
        "{message}"
    );
    assert!(!error.is_transient());
}

#[test]
fn a_server_of_another_protocol_is_a_mismatch_naming_both() {
    let connector = Launcher(
        || {
            let (here, there) = tokio::io::duplex(4096);
            let (reader, writer) = tokio::io::split(here);
            tokio::spawn(async move {
                let (_input, mut output) = tokio::io::split(there);
                let theirs = wire::Hello {
                    protocol: wire::PROTOCOL + 1,
                    version: "9.9.9".to_owned(),
                    rev: None,
                    host: None,
                };
                wire::write_magic(&mut output).await.unwrap();
                wire::write_frame(&mut output, &theirs).await.unwrap();
                tokio::io::AsyncWriteExt::flush(&mut output).await.unwrap();
                // Stay up: the app decides, not a closed pipe.
                std::future::pending::<()>().await;
            });
            Ok(Launch::Stream {
                reader: Box::new(reader),
                writer: Box::new(writer),
                name: "a newer server".to_owned(),
            })
        },
        None,
    );
    let error = refused(connector, "/anywhere");
    let ConnectError::Mismatch { ours, theirs } = &error else {
        panic!("expected Mismatch, got {error:?}");
    };
    assert_eq!(ours.protocol, wire::PROTOCOL);
    assert_eq!(theirs.version, "9.9.9");
    assert!(error.to_string().contains("9.9.9"), "{error}");
}

#[test]
fn a_host_with_no_command_says_why() {
    let error = refused(
        Launcher(|| Err("No host named box".to_owned()), None),
        "/anywhere",
    );
    assert_eq!(error, ConnectError::Launch("No host named box".to_owned()));
}

#[cfg(unix)]
#[test]
fn a_program_that_starts_no_server_reports_its_exit_and_last_words() {
    let script = "echo 'Welcome to box'; echo 'sh: diffui-serve: not found' >&2; exit 127";
    let program = || {
        Ok(Launch::Program {
            argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            env: Vec::new(),
        })
    };

    let error = refused(Launcher(program, None), "/anywhere");
    let ConnectError::NoServer {
        exit, timed_out, ..
    } = &error
    else {
        panic!("expected NoServer, got {error:?}");
    };
    assert_eq!(exit.code, Some(127));
    assert!(!timed_out);
    assert!(exit.stderr.contains("diffui-serve: not found"), "{exit:?}");
    assert!(
        error.to_string().ends_with("sh: diffui-serve: not found"),
        "{error}"
    );

    // The transport's reading of the exit wins when it has one.
    let explained = refused(
        Launcher(program, Some("box has no diffui-serve to run")),
        "/anywhere",
    );
    assert_eq!(explained.to_string(), "box has no diffui-serve to run");
}

#[cfg(unix)]
#[test]
fn a_program_that_cannot_be_started_is_a_spawn_error() {
    let error = refused(
        Launcher(
            || {
                Ok(Launch::Program {
                    argv: vec!["/nonexistent/diffui-transport".to_owned()],
                    env: Vec::new(),
                })
            },
            None,
        ),
        "/anywhere",
    );
    assert!(matches!(error, ConnectError::Spawn { .. }), "{error:?}");
}

#[test]
fn a_host_is_browsed_and_probed_over_its_link() {
    let root = harness::scratch_repo("remote-browse");
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    let parent = root.parent().unwrap().to_string_lossy().into_owned();
    let name = root.file_name().unwrap().to_string_lossy().into_owned();

    harness::block_on(async {
        let hosts = Hosts::new(Arc::new(harness::InProcessHost::new(Default::default())));
        let link = hosts.link(&host());
        assert!(matches!(
            &*link.state().borrow(),
            LinkState::Connecting { .. } | LinkState::Up(_)
        ));
        let info = link.ready().await.expect("the server answers");
        assert_eq!(info.os, std::env::consts::OS);

        let listing = link.list_dir(&format!("{parent}/")).await.unwrap();
        let entry = listing
            .iter()
            .find(|entry| entry.name == name)
            .expect("the repository is listed");
        assert_eq!(entry.vcs, Some(Vcs::Jj));
        assert!(link.list_dir(&format!("{parent}/nope/")).await.is_err());

        let inside = link
            .probe(&format!("{}/src/deep", root.display()))
            .await
            .unwrap();
        let found = inside.repository.expect("a repository above the path");
        assert!(!found.at_root);
        assert_eq!(found.vcs, Vcs::Jj);

        // One link per host, shared; a second ask doesn't start a second.
        let again = hosts.link(&host());
        assert!(again.ready().await.is_ok());
        assert!(hosts.existing(&host()).is_some());
        drop((link, again));
        assert!(hosts.existing(&host()).is_none(), "the last holder ends it");
    });
}

/// The shapes of everything on the wire, as serde sees them, against the
/// golden file. A wire type changing shape fails this: decide whether old
/// and new builds can still talk (see [`wire::PROTOCOL`]), raise the version
/// if not, then rewrite the file with `DIFFUI_BLESS=1`.
#[test]
fn the_wire_is_the_golden_file() {
    use serde_reflection::{Tracer, TracerConfig};

    let mut tracer = Tracer::new(TracerConfig::default());
    tracer
        .trace_simple_type::<wire::Hello>()
        .expect("trace Hello");
    tracer.trace_simple_type::<wire::Up>().expect("trace Up");
    tracer
        .trace_simple_type::<wire::Down>()
        .expect("trace Down");
    // An enum reached only through another container shows one variant per
    // pass; tracing it by name walks them all. A new one on the wire fails
    // the `registry()` below by name until it is added here.
    macro_rules! trace {
        ($($ty:ty),* $(,)?) => {$(
            tracer
                .trace_simple_type::<$ty>()
                .unwrap_or_else(|error| panic!("trace {}: {error}", stringify!($ty)));
        )*};
    }
    use diffui_core::{cli::passthrough, forge, history, model, mutations, preview_graph};
    use diffui_core::{repository, revset, session, source_browse};
    trace!(
        Command,
        Payload,
        Vcs,
        Result<diffui_core::RevsetMatch, diffui_core::RevsetError>,
        RepoError,
        repo::DiskSync,
        repo::PreviewRequest,
        passthrough::CliStream,
        forge::PrState,
        history::HistoryRequest,
        history::HistoryResponse,
        model::BlobKind,
        model::DiffBase,
        model::DiffCollapse,
        model::DiffFileStatus,
        model::DiffLineKind,
        model::DiffRequest,
        model::RevisionSelection,
        model::Subject,
        model::SyntaxKind,
        mutations::Destination,
        mutations::DraftSimulation,
        mutations::MutationOp,
        mutations::RebaseSourceMode,
        preview_graph::PostOpRole,
        repository::FetchTarget,
        revset::RevsetExpect,
        session::RefreshMode,
        session::RefreshOrigin,
        source_browse::SourceEntryStatus,
        history::CommitChangeKind,
        history::HistoryDiffTarget,
        history::RefKind,
        mutations::SquashTarget,
        diffui_core::partial::HunkPick,
        diffui_core::partial::LineRef,
        diffui_core::push::MoveKind,
        diffui_core::push::PushCheck,
        diffui_core::push::PushTarget,
        diffui_core::resolve::ConflictResolution,
        diffui_core::resolve::ConflictPick,
    );
    let registry = tracer.registry().expect("every wire type traced in full");
    // Written straight from the registry, not through a `serde_json::Value`:
    // a `Value`'s key order depends on serde_json's `preserve_order`, which
    // another crate of the workspace may or may not have turned on.
    #[derive(serde::Serialize)]
    struct Golden<'a> {
        protocol: u32,
        types: &'a serde_reflection::Registry,
    }
    let traced = serde_json::to_string_pretty(&Golden {
        protocol: wire::PROTOCOL,
        types: &registry,
    })
    .unwrap()
        + "\n";

    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire.json");
    if std::env::var_os("DIFFUI_BLESS").is_some() {
        std::fs::write(&golden, &traced).expect("write the golden file");
        return;
    }
    let expected = std::fs::read_to_string(&golden).unwrap_or_default();
    assert!(
        expected == traced,
        "The wire's shapes moved (or its version did). If a build with this change can't \
         talk to one without it, raise `remote::wire::PROTOCOL`. Then run this test with \
         DIFFUI_BLESS=1 to rewrite {}.",
        golden.display()
    );
}
