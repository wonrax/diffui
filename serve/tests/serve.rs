//! The real binary over real pipes: what the in-process scenarios in the
//! core can't show. The magic line and hello on a process's stdout, a typed
//! jj command run by the server's own built-in CLI, and the process ending
//! when the app lets go.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use diffui_core::cli::CliInvocation;
use diffui_core::remote::{Connector, Exit, HostId, Hosts, Launch};
use diffui_core::repo::{self, Command, JobId, OpenSpec, Payload};
use futures::StreamExt as _;
use futures::future::BoxFuture;

const SERVE: &str = env!("CARGO_BIN_EXE_diffui-serve");

/// A host that is this machine: the transport is the binary itself.
struct Local {
    jj_config: PathBuf,
}

impl Connector for Local {
    fn launch(&self, _host: &HostId) -> BoxFuture<'static, Result<Launch, String>> {
        let launch = Launch::Program {
            argv: vec![SERVE.to_owned()],
            env: vec![(
                "JJ_CONFIG".to_owned(),
                self.jj_config.to_string_lossy().into_owned(),
            )],
        };
        Box::pin(async move { Ok(launch) })
    }

    fn explain(&self, _host: &HostId, _exit: &Exit) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diffui-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A jj config that never reaches for the developer's: an identity, and no
/// signing.
fn jj_config(dir: &Path) -> PathBuf {
    let path = dir.join("jj-config.toml");
    std::fs::write(
        &path,
        "[user]\nname = \"Serve Test\"\nemail = \"serve@example.com\"\n\
         [signing]\nbehavior = \"drop\"\n",
    )
    .unwrap();
    path
}

#[tokio::test]
async fn a_typed_jj_command_runs_on_the_servers_own_cli() {
    let dir = scratch("cli");
    let config = jj_config(&dir);
    let root = dir.join("repo");

    // The binary is its own jj: make the repository with it.
    let init = tokio::process::Command::new(SERVE)
        .args(["jj", "git", "init"])
        .arg(&root)
        .env("JJ_CONFIG", &config)
        .output()
        .await
        .expect("run diffui-serve jj");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let hosts = Hosts::new(Arc::new(Local { jj_config: config }));
    let host = HostId("here".to_owned());
    let mut events = Box::pin(repo::open(
        OpenSpec::Remote {
            host: host.clone(),
            path: root.to_string_lossy().into_owned(),
        },
        &Default::default(),
        &hosts,
    ));
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(60), events.next())
            .await
            .expect("the server should have answered by now")
            .expect("the event stream ended early")
            .payload
    };
    let handle = loop {
        match next().await {
            Payload::Connecting { .. } => {}
            Payload::Ready { handle, .. } => break handle,
            other => panic!("expected Ready, got {other:?}"),
        }
    };
    assert_eq!(
        handle.remote().expect("remote").opened.root,
        root.to_string_lossy()
    );

    // The app names its own binary; the server must run its own instead.
    let job = JobId::next();
    handle.send(Command::RunCli {
        job,
        invocation: CliInvocation::new(
            "/nonexistent/the-apps-own-binary",
            ["log", "--no-graph", "-r", "@", "-T", "\"served\""]
                .map(str::to_owned)
                .to_vec(),
        ),
    });
    let mut output = String::new();
    let code = loop {
        match next().await {
            Payload::CliOutput { lines, .. } => {
                for line in lines {
                    output.push_str(&line.text);
                    output.push('\n');
                }
            }
            Payload::CliExited { code, .. } => break code,
            Payload::Failed { error, .. } => panic!("the run failed: {error}"),
            _ => {}
        }
    };
    assert_eq!(code, Some(0), "{output}");
    assert!(output.contains("served"), "{output}");

    // Letting go of everything ends the link, and with it the process.
    drop(handle);
    drop(events);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(hosts.existing(&host).is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn run_by_hand_it_says_what_it_is() {
    let version = tokio::process::Command::new(SERVE)
        .arg("--version")
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&version.stdout);
    assert!(
        text.starts_with("diffui-serve ") && text.contains("protocol"),
        "{text}"
    );

    let unknown = tokio::process::Command::new(SERVE)
        .arg("--nope")
        .output()
        .await
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Usage:"));
}
