//! The server end: `diffui-serve`, and the app's own `diffui serve`.
//!
//! It answers on stdin and stdout for as long as stdin stays open, opening
//! each repository it is asked for with the same actor the app runs in
//! process. Nothing here is told where it is: one server answers for
//! whatever paths the app names, as the user who started it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use tokio::io::{AsyncWriteExt as _, BufReader, BufWriter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::wire::{self, Channel, Down, Hello, HostInfo, Opened, Up};
use super::{Hosts, dirs};
use crate::repo::{self, Command, OpenSpec, Payload, RepoHandle, SettingsSource};
use crate::repository::{Vcs, prepare_repository};

/// What `--version` prints: enough to tell whether an app can talk to this.
pub fn version() -> String {
    Hello::ours(None).describe()
}

/// Serve this machine's repositories on stdin and stdout until stdin ends.
///
/// `cli` is the program that runs as the jj CLI for a typed jj command (the
/// running binary: see [`crate::cli::passthrough`]).
pub fn serve_stdio(cli: PathBuf) -> ExitCode {
    // Nobody is at this end to answer git: fail instead of waiting.
    //
    // SAFETY: nothing else runs yet; the runtime and its threads start below.
    unsafe { std::env::set_var("GIT_TERMINAL_PROMPT", "0") };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("diffui-serve: {error}");
            return ExitCode::FAILURE;
        }
    };
    let served = runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout(), cli));
    // An actor mid-snapshot shouldn't keep the process alive once the app is
    // gone.
    runtime.shutdown_background();
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diffui-serve: {error}");
            ExitCode::FAILURE
        }
    }
}

/// One open repository: how to send it commands, and the task relaying its
/// events. Dropping the task's stream stops the actor.
struct Served {
    handle: Option<RepoHandle>,
    pump: JoinHandle<()>,
}

type Repositories = Arc<Mutex<HashMap<Channel, Served>>>;

/// Serve on `input` and `output` until `input` ends.
pub async fn serve<R, W>(input: R, output: W, cli: PathBuf) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    serve_with(input, output, cli, SettingsSource::Layered).await
}

/// [`serve`], with the jj configuration its repositories open with named
/// rather than discovered. Only a test needs this; see [`SettingsSource`].
pub async fn serve_with<R, W>(
    input: R,
    output: W,
    cli: PathBuf,
    settings: SettingsSource,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut input = BufReader::new(input);
    let mut output = BufWriter::new(output);
    wire::write_magic(&mut output).await?;
    let host = HostInfo {
        os: std::env::consts::OS.to_owned(),
        home: dirs::home(),
    };
    wire::write_frame(&mut output, &Hello::ours(Some(host))).await?;
    output.flush().await?;

    let Some(theirs) = wire::read_frame::<_, Hello>(&mut input).await? else {
        return Ok(());
    };
    if theirs.protocol != wire::PROTOCOL {
        // The app reads our hello and says so to the user; this is for
        // whoever ran the server by hand.
        return Err(std::io::Error::other(format!(
            "diffui {} speaks another protocol than this diffui-serve {}",
            theirs.describe(),
            version()
        )));
    }

    let (down, mut outbox) = mpsc::unbounded_channel::<Down>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outbox.recv().await {
            wire::write_frame(&mut output, &message).await?;
            // Whatever is ready goes out in one write.
            while let Ok(message) = outbox.try_recv() {
                wire::write_frame(&mut output, &message).await?;
            }
            output.flush().await?;
        }
        std::io::Result::Ok(())
    });
    // However this ends (stdin closing, or the future dropped), the actors
    // stop and the output is let go: what was queued for the app has nowhere
    // to go.
    let serving = Serving {
        repositories: Repositories::default(),
        writer,
    };

    while let Some(message) = wire::read_frame::<_, Up>(&mut input).await? {
        handle(message, &serving.repositories, &down, &cli, &settings);
    }
    Ok(())
}

/// What a serve loop has running, stopped when it is dropped.
struct Serving {
    repositories: Repositories,
    writer: JoinHandle<std::io::Result<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        for (_, served) in self.repositories.lock().unwrap().drain() {
            served.pump.abort();
        }
        self.writer.abort();
    }
}

fn handle(
    message: Up,
    repositories: &Repositories,
    down: &mpsc::UnboundedSender<Down>,
    cli: &std::path::Path,
    settings: &SettingsSource,
) {
    match message {
        Up::ListDir { request, path } => {
            let down = down.clone();
            tokio::task::spawn_blocking(move || {
                let (entries, error) = match dirs::list(&path) {
                    Ok(entries) => (entries, None),
                    Err(error) => (Vec::new(), Some(error)),
                };
                let _ = down.send(Down::DirListed {
                    request,
                    entries,
                    error,
                });
            });
        }
        Up::Probe { request, path } => {
            let down = down.clone();
            tokio::task::spawn_blocking(move || {
                let _ = down.send(Down::Probed {
                    request,
                    probe: dirs::probe(&path),
                });
            });
        }
        Up::Open { channel, path } => {
            // Locked across the spawn, so the task finds its entry.
            let mut served = repositories.lock().unwrap();
            let pump = tokio::spawn(open(
                channel,
                path,
                repositories.clone(),
                down.clone(),
                settings.clone(),
            ));
            let replaced = served.insert(channel, Served { handle: None, pump });
            drop(served);
            if let Some(replaced) = replaced {
                replaced.pump.abort();
            }
        }
        Up::Run {
            channel,
            scope,
            mut command,
        } => {
            if let Command::RunCli { invocation, .. } = command.as_mut() {
                // The app named its own binary and its own credential
                // prompts, neither of which exists here.
                invocation.program = cli.to_owned();
                invocation.env.clear();
            }
            let handle = repositories
                .lock()
                .unwrap()
                .get(&channel)
                .and_then(|served| served.handle.clone());
            match handle {
                Some(handle) => handle.with_scope(PathBuf::from(scope)).send(*command),
                None => tracing::warn!(channel, "a command for a channel that isn't open"),
            }
        }
        Up::Close { channel } => {
            if let Some(served) = repositories.lock().unwrap().remove(&channel) {
                served.pump.abort();
            }
        }
    }
}

/// Open the repository at `path` and relay its events until the channel is
/// closed.
async fn open(
    channel: Channel,
    path: String,
    repositories: Repositories,
    down: mpsc::UnboundedSender<Down>,
    settings: SettingsSource,
) {
    let target = dirs::expand(&path);
    let prepared = tokio::task::spawn_blocking(move || {
        let repository = prepare_repository(&target).map_err(|error| format!("{error:#}"))?;
        let default_revset =
            (repository.vcs == Vcs::Jj).then(|| crate::jj::jj_log_revset(&repository.root));
        let remotes = crate::forge::remotes(&repository.root);
        Ok::<_, String>((repository, default_revset, remotes))
    })
    .await;
    let (repository, default_revset, remotes) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(message)) => {
            let _ = down.send(Down::Refused { channel, message });
            return;
        }
        Err(error) => {
            let _ = down.send(Down::Refused {
                channel,
                message: error.to_string(),
            });
            return;
        }
    };

    let mut events = repo::open_with(
        OpenSpec::Local {
            root: repository.root.clone(),
            scope: repository.scope.clone(),
        },
        settings,
        &crate::forge::Forges::default(),
        &Hosts::none(),
    );
    while let Some(event) = events.next().await {
        let message = match event.payload {
            Payload::Ready {
                handle,
                capabilities,
            } => {
                match repositories.lock().unwrap().get_mut(&channel) {
                    Some(served) => served.handle = Some(handle),
                    // Closed while it was opening.
                    None => return,
                }
                Down::Opened {
                    channel,
                    repository: Opened {
                        root: repository.root.to_string_lossy().into_owned(),
                        vcs: repository.vcs,
                        scope: repository.scope.to_string_lossy().into_owned(),
                        capabilities,
                        default_revset: default_revset.clone(),
                        remotes: remotes.clone(),
                    },
                }
            }
            payload => Down::Event {
                channel,
                payload: Box::new(payload),
            },
        };
        if down.send(message).is_err() {
            return;
        }
    }
}
