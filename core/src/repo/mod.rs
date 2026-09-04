//! One owner per open repository.
//!
//! Every backend operation used to be a free function over a path that loaded
//! the settings, loaded the workspace, loaded the repo at head, did its thing
//! and dropped all three. Nothing owned the repository, so everything that
//! *needs* an owner — serializing mutations, deduping external operations,
//! cancelling superseded work — lived in the frontend.
//!
//! A [`RepoActor`] owns it instead: the workspaces, the repo at head, and (for
//! jj) a dedicated thread with a current-thread runtime, because jj-lib's types
//! are not `Send`. It takes [`Command`]s and emits [`Event`]s, and it is the
//! only thing that ever takes the working-copy lock. Mutations serialize
//! because it is single-threaded; a fetch can no longer race one.
//!
//! [`open`] spawns the actor and hands back its event stream; the actor's life
//! is that stream's life, so the frontend can key it on a subscription and let
//! the last tab closing take the actor with it.

mod cancel;
mod git_actor;
mod jj_actor;
mod pr_actor;
pub mod protocol;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use futures::Stream;
use tokio::sync::mpsc;

pub use cancel::CancelFlag;
use cancel::CancelRegistry;
pub use jj_actor::{canonical_selection, rewritten_targets};
pub use protocol::{
    Capabilities, Command, Event, GraphTail, JobId, OpenSpec, Payload, PreviewRequest, RepoError,
    RepoId,
};

use crate::repository::{Repository, Vcs};
use crate::watcher::WatchBatch;

/// What travels down the command channel. `Watch` and `Shutdown` are the
/// actor's own control traffic and never leave this module — the protocol
/// proper is [`Command`].
pub(crate) enum Envelope {
    Run {
        repository: Repository,
        command: Command,
    },
    Watch(WatchBatch),
    Shutdown,
}

/// The live cancel flags of one actor, shared between the future reading its
/// command channel and the one running its commands.
pub(crate) type Jobs = Arc<Mutex<CancelRegistry>>;

/// Read the command channel while the actor is busy running something.
///
/// A `Cancel` names a job that is, by definition, already running or already
/// queued. On one FIFO channel it would only be dequeued once that job had
/// finished, which is no cancellation at all — so cancels are applied here, as
/// they arrive, and everything else is forwarded in order to the dispatch loop.
///
/// Returns when the command channel closes or the dispatch loop is gone, so
/// the two always end together.
pub(crate) async fn route_commands(
    mut commands: mpsc::UnboundedReceiver<Envelope>,
    work: mpsc::UnboundedSender<Envelope>,
    jobs: Jobs,
) {
    while let Some(envelope) = commands.recv().await {
        if let Envelope::Run {
            command: Command::Cancel { job },
            ..
        } = &envelope
        {
            if let Ok(mut jobs) = jobs.lock() {
                jobs.cancel(*job);
            }
            continue;
        }
        let shutdown = matches!(envelope, Envelope::Shutdown);
        if work.send(envelope).is_err() || shutdown {
            return;
        }
    }
}

/// The command sender for one open repository. Cheap to clone; a tab holds one
/// for as long as it is open.
#[derive(Clone)]
pub struct RepoHandle {
    id: RepoId,
    repository: Repository,
    capabilities: Capabilities,
    tx: mpsc::UnboundedSender<Envelope>,
}

impl std::fmt::Debug for RepoHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoHandle")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl RepoHandle {
    pub fn id(&self) -> &RepoId {
        &self.id
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    /// The same actor, addressed with a different path scope. Two tabs can
    /// open one repository narrowed to different subdirectories; the scope
    /// rides on each command rather than being baked into the actor.
    pub fn with_scope(&self, scope: PathBuf) -> Self {
        let mut handle = self.clone();
        handle.repository.scope = scope;
        handle
    }

    /// Queue `command`. A send to a stopped actor is dropped: the actor only
    /// stops when its event stream is gone, so there is no one left to tell.
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(Envelope::Run {
            repository: self.repository.clone(),
            command,
        });
    }
}

/// The event stream of one open repository. Dropping it shuts the actor down —
/// which is the point of spawning the actor *inside* the frontend's
/// subscription: the last tab on a repository closing ends the subscription,
/// which ends the stream, which ends the actor.
pub struct RepoEvents {
    rx: mpsc::UnboundedReceiver<Event>,
    shutdown: mpsc::UnboundedSender<Envelope>,
}

impl Stream for RepoEvents {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Event>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for RepoEvents {
    fn drop(&mut self) {
        let _ = self.shutdown.send(Envelope::Shutdown);
    }
}

/// The identity of one actor: the workspace root for a local repository, the
/// pull-request reference for a PR.
///
/// Per *workspace*, not per `.jj/repo`, so two workspaces of one repository get
/// an actor each. Sharing one would need the frontend's subscription keyed on
/// the repository rather than on what it opened, and a watcher per workspace
/// root instead of the one the actor starts with. They contend on nothing but
/// the operation store in the meantime, since jj's working-copy lock is per
/// workspace, so this costs a thread rather than correctness.
pub fn repo_id(spec: &OpenSpec) -> RepoId {
    match spec {
        OpenSpec::Local { root, .. } => RepoId(root.to_string_lossy().into_owned()),
        OpenSpec::GitHubPr(pr) => RepoId(format!("{}/{}#{}", pr.owner, pr.repo, pr.number)),
    }
}

/// The capabilities of a repository opened as `spec`, decided once here so no
/// call site has to ask "does this source support X" by downcasting.
fn capabilities_for(spec: &OpenSpec) -> Capabilities {
    match spec {
        OpenSpec::Local { root, .. } if is_jj(root) => Capabilities {
            graph: true,
            mutate: true,
            fetch: true,
            browse: true,
            details: true,
        },
        OpenSpec::Local { .. } => Capabilities {
            graph: true,
            fetch: true,
            ..Capabilities::default()
        },
        OpenSpec::GitHubPr(_) => Capabilities::default(),
    }
}

fn is_jj(root: &Path) -> bool {
    root.join(".jj").is_dir()
}

/// Where a jj actor's configuration comes from.
///
/// The actor loads jj's layered config on its own thread, from the process
/// environment. That is what the app wants and what a test must not get: a
/// developer with `signing.behavior = "own"` and an SSH agent had every commit
/// a fixture wrote through the actor block on a signing prompt, because the
/// fixture's own settings stopped at the jj-lib calls it made directly and
/// never reached the actor. A test hands its settings over instead.
#[derive(Clone, Default)]
pub enum SettingsSource {
    /// jj's real config: defaults, the user's files, the repo's, `JJ_*`.
    #[default]
    Layered,
    /// Exactly these settings; nothing is read from the environment.
    Fixed(Box<jj_lib::settings::UserSettings>),
}

impl std::fmt::Debug for SettingsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layered => f.write_str("Layered"),
            Self::Fixed(_) => f.write_str("Fixed(..)"),
        }
    }
}

/// Spawn the actor for `spec` and return its event stream. The first event is
/// [`Payload::Ready`], which carries the command sender — everything else the
/// frontend needs travels as protocol data.
pub fn open(spec: OpenSpec) -> RepoEvents {
    open_with(spec, SettingsSource::Layered)
}

/// [`open`], with the jj configuration named rather than discovered. Only a
/// test needs this; see [`SettingsSource`].
pub fn open_with(spec: OpenSpec, settings: SettingsSource) -> RepoEvents {
    let id = repo_id(&spec);
    let capabilities = capabilities_for(&spec);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Envelope>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    let repository = match &spec {
        OpenSpec::Local { root, scope } => Repository {
            root: root.clone(),
            vcs: if is_jj(root) { Vcs::Jj } else { Vcs::Git },
            scope: scope.clone(),
        },
        // A PR has no local checkout; the placeholder keeps `RepoHandle` one
        // shape across every kind of source.
        OpenSpec::GitHubPr(_) => Repository {
            root: PathBuf::new(),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        },
    };

    let handle = RepoHandle {
        id: id.clone(),
        repository,
        capabilities,
        tx: cmd_tx.clone(),
    };
    let _ = event_tx.send(Event::new(
        id.clone(),
        Payload::Ready {
            handle,
            capabilities,
        },
    ));

    match spec {
        OpenSpec::Local { root, .. } if is_jj(&root) => {
            jj_actor::spawn(id, root, settings, cmd_rx, cmd_tx.clone(), event_tx);
        }
        OpenSpec::Local { root, .. } => git_actor::spawn(id, root, cmd_rx, event_tx),
        OpenSpec::GitHubPr(pr) => pr_actor::spawn(id, pr, cmd_rx, event_tx),
    }

    RepoEvents {
        rx: event_rx,
        shutdown: cmd_tx,
    }
}
