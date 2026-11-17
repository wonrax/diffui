//! The app's end of the stream to one host.
//!
//! A [`HostLink`] starts the transport's program, shakes hands with the
//! server behind it and then carries every tab's commands and events, and
//! the open prompt's questions, over that one stream. It is up for as long
//! as something holds it: the last holder letting go ends the program.
//!
//! A link that failed stays failed. [`Hosts::link`] hands out a fresh one
//! the next time it is asked, which is all a reconnect is.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, BufWriter,
};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch};

use super::wire::{self, Channel, DirEntry, Down, Hello, HostInfo, Opened, Probed, Request, Up};
use super::{ConnectError, Connector, Exit, HostId, Launch};
use crate::repo::Payload;

/// How long a launched program may stay silent before the app gives up on a
/// server answering. Anything it writes to stderr starts the wait over. It
/// is long because a command that is busy getting the server (a `nix run`
/// compiling it) can say nothing for many minutes, and cutting that off
/// throws the work away. A program that fails says so and exits, which ends
/// the wait at once; one that is stuck is dropped with its tab or prompt.
const QUIET_LIMIT: Duration = Duration::from_secs(30 * 60);

/// How much of the program's stderr is kept: for messages, and for whoever
/// wants to read what happened.
const STDERR_TAIL: usize = 32 * 1024;

/// What was run to reach a host and what it has written to stderr so far
/// (the end of it), for showing while it comes up and after it failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    /// The command line, once the transport has said what it is.
    pub command: Option<String>,
    pub output: String,
}

/// Where a link stands.
#[derive(Debug, Clone)]
pub enum LinkState {
    /// The program is starting. `detail` is the last line it wrote to
    /// stderr, when it wrote any.
    Connecting {
        detail: Option<String>,
    },
    Up(Arc<HostInfo>),
    /// Over, and why. A link doesn't come back from this.
    Down(ConnectError),
}

/// What arrives on one channel.
pub(crate) enum ChannelEvent {
    Opened(Opened),
    Refused(String),
    Event(Box<Payload>),
}

/// Who is waiting for what on a stream.
#[derive(Default)]
struct Waiting {
    listings: HashMap<Request, oneshot::Sender<Result<Vec<DirEntry>, String>>>,
    probes: HashMap<Request, oneshot::Sender<Probed>>,
    channels: HashMap<Channel, mpsc::UnboundedSender<ChannelEvent>>,
    /// The stream is over: nothing new can wait on it.
    closed: bool,
}

struct Shared {
    host: HostId,
    state: watch::Sender<LinkState>,
    waiting: Mutex<Waiting>,
    started: std::time::Instant,
    /// The command line that was started, once there is one.
    command: Mutex<Option<String>>,
    stderr: Arc<Mutex<Stderr>>,
}

impl Shared {
    /// End the link: every waiter is let go, and finds the reason in the
    /// state.
    fn fail(&self, error: ConnectError) {
        tracing::warn!(host = %self.host, %error, "host link is down");
        // The state first: a waiter that wakes on its dropped sender reads it.
        self.state.send_replace(LinkState::Down(error));
        let mut waiting = self.waiting.lock().unwrap();
        waiting.closed = true;
        waiting.listings.clear();
        waiting.probes.clear();
        waiting.channels.clear();
    }
}

struct Link {
    shared: Arc<Shared>,
    up: mpsc::UnboundedSender<Up>,
    next_request: AtomicU64,
    next_channel: AtomicU32,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // Ends the program too: the task owns it, `kill_on_drop`.
        self.task.abort();
    }
}

/// A shared hold on the stream to one host.
#[derive(Clone)]
pub struct HostLink(Arc<Link>);

impl std::fmt::Debug for HostLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostLink")
            .field("host", &self.0.shared.host)
            .field("state", &*self.0.shared.state.borrow())
            .finish()
    }
}

impl HostLink {
    fn start(host: HostId, connector: Arc<dyn Connector>) -> Self {
        let (state, _) = watch::channel(LinkState::Connecting { detail: None });
        let shared = Arc::new(Shared {
            host,
            state,
            waiting: Mutex::default(),
            started: std::time::Instant::now(),
            command: Mutex::default(),
            stderr: Arc::default(),
        });
        let (up, outbox) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(shared.clone(), connector, outbox));
        Self(Arc::new(Link {
            shared,
            up,
            next_request: AtomicU64::new(1),
            next_channel: AtomicU32::new(1),
            task,
        }))
    }

    pub fn host(&self) -> &HostId {
        &self.0.shared.host
    }

    /// When the link was started.
    pub fn started(&self) -> std::time::Instant {
        self.0.shared.started
    }

    /// What was run and what it has said, as of now.
    pub fn transcript(&self) -> Transcript {
        Transcript {
            command: self.0.shared.command.lock().unwrap().clone(),
            output: self.0.shared.stderr.lock().unwrap().tail.clone(),
        }
    }

    /// Whether `other` is a hold on this very link, not just one to the
    /// same host.
    pub fn same(&self, other: &HostLink) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// How many hold the link, this one included.
    pub fn holders(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Hang up now, whoever else holds the link: the program ends, and they
    /// find the link down.
    pub fn stop(&self) {
        self.0.task.abort();
        if matches!(&*self.0.shared.state.borrow(), LinkState::Down(_)) {
            return;
        }
        self.0.shared.fail(ConnectError::Lost {
            message: format!("The connection to {} was stopped", self.host()),
        });
    }

    /// The link's state, to read now and to wait on.
    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.0.shared.state.subscribe()
    }

    /// Wait for the handshake: the host, or why it can't be reached.
    pub async fn ready(&self) -> Result<Arc<HostInfo>, ConnectError> {
        let mut state = self.state();
        loop {
            match &*state.borrow_and_update() {
                LinkState::Up(info) => return Ok(info.clone()),
                LinkState::Down(error) => return Err(error.clone()),
                LinkState::Connecting { .. } => {}
            }
            if state.changed().await.is_err() {
                return Err(self.down_reason());
            }
        }
    }

    /// Why the link is down, for a waiter it let go.
    pub(crate) fn down_reason(&self) -> ConnectError {
        match &*self.0.shared.state.borrow() {
            LinkState::Down(error) => error.clone(),
            _ => ConnectError::Lost {
                message: format!("The connection to {} was closed", self.host()),
            },
        }
    }

    /// The folders in `path` on the host.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, String> {
        let request = self.0.next_request.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.ask(
            |waiting| {
                waiting.listings.insert(request, answer);
            },
            Up::ListDir {
                request,
                path: path.to_owned(),
            },
        );
        match answered.await {
            Ok(result) => result,
            Err(_) => Err(self.down_reason().to_string()),
        }
    }

    /// What opening `path` on the host would open.
    pub async fn probe(&self, path: &str) -> Result<Probed, String> {
        let request = self.0.next_request.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.ask(
            |waiting| {
                waiting.probes.insert(request, answer);
            },
            Up::Probe {
                request,
                path: path.to_owned(),
            },
        );
        answered.await.map_err(|_| self.down_reason().to_string())
    }

    /// Open the repository at `path` on a channel of its own. The first
    /// event is the answer; the receiver closing is the link going down.
    pub(crate) fn open(&self, path: String) -> (Channel, mpsc::UnboundedReceiver<ChannelEvent>) {
        let channel = self.0.next_channel.fetch_add(1, Ordering::Relaxed);
        let (events, receiver) = mpsc::unbounded_channel();
        self.ask(
            |waiting| {
                waiting.channels.insert(channel, events);
            },
            Up::Open { channel, path },
        );
        (channel, receiver)
    }

    /// Register a waiter and send its question, unless the stream is over:
    /// then the waiter is dropped here, which is how its owner hears.
    fn ask(&self, register: impl FnOnce(&mut Waiting), message: Up) {
        let mut waiting = self.0.shared.waiting.lock().unwrap();
        if waiting.closed {
            return;
        }
        register(&mut waiting);
        drop(waiting);
        self.send(message);
    }

    /// Queue `message`. Sent before the handshake is done, it goes out right
    /// after; sent to a link that is down, it is dropped.
    pub(crate) fn send(&self, message: Up) {
        let _ = self.0.up.send(message);
    }

    /// Stop hearing about `channel` and tell the server to stop its actor.
    pub(crate) fn close(&self, channel: Channel) {
        self.0
            .shared
            .waiting
            .lock()
            .unwrap()
            .channels
            .remove(&channel);
        self.send(Up::Close { channel });
    }
}

/// The app's links, one per host, shared by everything that talks to it.
#[derive(Clone)]
pub struct Hosts {
    connector: Arc<dyn Connector>,
    links: Arc<Mutex<HashMap<HostId, Weak<Link>>>>,
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts").finish_non_exhaustive()
    }
}

impl Hosts {
    pub fn new(connector: Arc<dyn Connector>) -> Self {
        Self {
            connector,
            links: Arc::default(),
        }
    }

    /// No hosts at all: for a frontend (or a server) that opens local
    /// repositories only.
    pub fn none() -> Self {
        struct NoHosts;
        impl Connector for NoHosts {
            fn launch(&self, host: &HostId) -> BoxFuture<'static, Result<Launch, String>> {
                let message = format!("No host named {host}");
                Box::pin(async move { Err(message) })
            }
            fn explain(&self, _: &HostId, _: &Exit) -> BoxFuture<'static, Option<String>> {
                Box::pin(async { None })
            }
        }
        Self::new(Arc::new(NoHosts))
    }

    /// The link to `host`: the one already up or coming up, else a new one.
    /// Call it inside a tokio runtime.
    pub fn link(&self, host: &HostId) -> HostLink {
        let mut links = self.links.lock().unwrap();
        links.retain(|_, link| link.strong_count() > 0);
        if let Some(link) = links.get(host).and_then(Weak::upgrade)
            && !matches!(&*link.shared.state.borrow(), LinkState::Down(_))
        {
            return HostLink(link);
        }
        let link = HostLink::start(host.clone(), self.connector.clone());
        links.insert(host.clone(), Arc::downgrade(&link.0));
        link
    }

    /// The link to `host` if one is up or coming up, without starting one.
    pub fn existing(&self, host: &HostId) -> Option<HostLink> {
        let links = self.links.lock().unwrap();
        let link = links.get(host).and_then(Weak::upgrade)?;
        let down = matches!(&*link.shared.state.borrow(), LinkState::Down(_));
        (!down).then(|| HostLink(link))
    }
}

/// The end of the program's stderr, shared between the task reading it and
/// the one that reports a failure.
#[derive(Default)]
struct Stderr {
    tail: String,
    /// Bumped on every line, so the handshake's wait can tell the program is
    /// still doing something.
    lines: u64,
}

impl Stderr {
    fn push(&mut self, line: &str) {
        self.lines += 1;
        self.tail.push_str(line);
        self.tail.push('\n');
        if self.tail.len() > STDERR_TAIL {
            let mut cut = self.tail.len() - STDERR_TAIL;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
    }
}

/// The program, set up but not started: stdio piped, ended when dropped,
/// and without a console window of its own (on Windows a GUI app starting
/// `wsl.exe` or `ssh.exe` would otherwise flash one).
fn command(argv: &[String], env: &[(String, String)]) -> Option<tokio::process::Command> {
    let (program, args) = argv.split_first()?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Some(command)
}

async fn run(
    shared: Arc<Shared>,
    connector: Arc<dyn Connector>,
    outbox: mpsc::UnboundedReceiver<Up>,
) {
    let error = match connect(&shared, connector.as_ref()).await {
        Ok(stream) => carry(&shared, stream, outbox).await,
        Err(error) => error,
    };
    shared.fail(error);
}

/// A stream whose server said hello.
struct Stream {
    /// The program behind it, when there is one.
    child: Option<Child>,
    input: BufWriter<Box<dyn AsyncWrite + Send + Unpin>>,
    output: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    stderr: Arc<Mutex<Stderr>>,
}

/// The program's exit code, if it ends within a moment: the pipe closing
/// and the exit are two events.
async fn exit_code(child: &mut Option<Child>) -> Option<i32> {
    let child = child.as_mut()?;
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    // Let the last of stderr arrive behind the exit.
    tokio::time::sleep(Duration::from_millis(50)).await;
    status.ok()?.ok()?.code()
}

async fn connect(shared: &Arc<Shared>, connector: &dyn Connector) -> Result<Stream, ConnectError> {
    let launch = connector
        .launch(&shared.host)
        .await
        .map_err(ConnectError::Launch)?;
    let shown = launch.display();
    tracing::info!(host = %shared.host, command = %shown, "connecting to host");
    *shared.command.lock().unwrap() = Some(shown.clone());
    let stderr = shared.stderr.clone();

    let (mut child, input, output): (
        _,
        Box<dyn AsyncWrite + Send + Unpin>,
        Box<dyn AsyncRead + Send + Unpin>,
    ) = match launch {
        Launch::Stream { reader, writer, .. } => (None, writer, reader),
        Launch::Program { argv, env } => {
            let spawn_error = |message: String| ConnectError::Spawn {
                command: shown.clone(),
                message,
            };
            let mut child = command(&argv, &env)
                .ok_or_else(|| spawn_error("the transport gave no command".to_owned()))?
                .spawn()
                .map_err(|error| spawn_error(error.to_string()))?;
            let (Some(input), Some(output), Some(errors)) =
                (child.stdin.take(), child.stdout.take(), child.stderr.take())
            else {
                return Err(spawn_error("its stdio wasn't piped".to_owned()));
            };
            tokio::spawn(read_stderr(errors, stderr.clone(), Arc::downgrade(shared)));
            (Some(child), Box::new(input), Box::new(output))
        }
    };
    let mut input = BufWriter::new(input);
    let mut output = BufReader::new(output);

    let (said, timed_out) = {
        let handshake = async {
            // Ours goes out without waiting for theirs, so a server that is
            // there but older still learns who called.
            wire::write_frame(&mut input, &Hello::ours(None)).await?;
            input.flush().await?;
            if !wire::read_magic(&mut output).await? {
                return Ok(None);
            }
            wire::read_frame::<_, Hello>(&mut output).await
        };
        tokio::pin!(handshake);
        let mut seen = 0;
        loop {
            tokio::select! {
                said = &mut handshake => break (said.ok().flatten(), false),
                () = tokio::time::sleep(QUIET_LIMIT) => {
                    let lines = stderr.lock().unwrap().lines;
                    if lines == seen {
                        break (None, true);
                    }
                    seen = lines;
                }
            }
        }
    };

    let Some(theirs) = said else {
        let code = if timed_out {
            None
        } else {
            exit_code(&mut child).await
        };
        let exit = Exit {
            code,
            stderr: stderr.lock().unwrap().tail.clone(),
        };
        // The whole of it goes to the log: the message shows one line.
        tracing::warn!(
            host = %shared.host,
            command = %shown,
            code = ?exit.code,
            timed_out,
            "no diffui-serve answered; the program wrote:\n{}",
            exit.stderr
        );
        let explained = connector.explain(&shared.host, &exit).await;
        return Err(ConnectError::NoServer {
            command: shown,
            exit,
            explained,
            timed_out,
        });
    };
    if theirs.protocol != wire::PROTOCOL {
        return Err(ConnectError::Mismatch {
            ours: Box::new(Hello::ours(None)),
            theirs: Box::new(theirs),
        });
    }
    let info = theirs.host.unwrap_or(HostInfo {
        os: String::new(),
        home: None,
    });
    tracing::info!(host = %shared.host, os = %info.os, "host link is up");
    shared.state.send_replace(LinkState::Up(Arc::new(info)));
    Ok(Stream {
        child,
        input,
        output,
        stderr,
    })
}

/// Keep the end of what the program writes to stderr, and show its last
/// line as the progress there is while the link is coming up.
async fn read_stderr(
    errors: tokio::process::ChildStderr,
    stderr: Arc<Mutex<Stderr>>,
    shared: Weak<Shared>,
) {
    let mut lines = BufReader::new(errors).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        tracing::debug!(host = %shared.host, "{line}");
        stderr.lock().unwrap().push(&line);
        shared.state.send_if_modified(|state| match state {
            LinkState::Connecting { detail } => {
                *detail = Some(line.trim().to_owned());
                true
            }
            _ => false,
        });
    }
}

/// Carry messages both ways until the stream ends, and say why it did.
async fn carry(
    shared: &Arc<Shared>,
    stream: Stream,
    mut outbox: mpsc::UnboundedReceiver<Up>,
) -> ConnectError {
    let Stream {
        mut child,
        mut input,
        mut output,
        stderr,
    } = stream;
    let write = async {
        while let Some(message) = outbox.recv().await {
            wire::write_frame(&mut input, &message).await?;
            while let Ok(message) = outbox.try_recv() {
                wire::write_frame(&mut input, &message).await?;
            }
            input.flush().await?;
        }
        // Every holder letting go aborts this task before it gets here.
        std::future::pending::<std::io::Result<()>>().await
    };
    let read = async {
        loop {
            match wire::read_frame::<_, Down>(&mut output).await {
                Ok(Some(message)) => deliver(shared, message),
                Ok(None) => return None,
                Err(error) => return Some(error),
            }
        }
    };
    let broken = tokio::select! {
        written = write => written.err(),
        broken = read => broken,
    };

    let code = exit_code(&mut child).await;
    let last = stderr
        .lock()
        .unwrap()
        .tail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned());
    let host = &shared.host;
    let message = match (broken, last, code) {
        (_, Some(last), _) => format!("The connection to {host} was lost: {last}"),
        (Some(error), None, _) => format!("The connection to {host} was lost: {error}"),
        (None, None, Some(code)) => {
            format!("The connection to {host} was closed (exit code {code})")
        }
        (None, None, None) => format!("The connection to {host} was closed"),
    };
    ConnectError::Lost { message }
}

fn deliver(shared: &Shared, message: Down) {
    let mut waiting = shared.waiting.lock().unwrap();
    match message {
        Down::DirListed {
            request,
            entries,
            error,
        } => {
            if let Some(answer) = waiting.listings.remove(&request) {
                let _ = answer.send(error.map_or(Ok(entries), Err));
            }
        }
        Down::Probed { request, probe } => {
            if let Some(answer) = waiting.probes.remove(&request) {
                let _ = answer.send(probe);
            }
        }
        Down::Opened {
            channel,
            repository,
        } => send(&mut waiting, channel, ChannelEvent::Opened(repository)),
        Down::Refused { channel, message } => {
            send(&mut waiting, channel, ChannelEvent::Refused(message));
            waiting.channels.remove(&channel);
        }
        Down::Event { channel, payload } => {
            send(&mut waiting, channel, ChannelEvent::Event(payload));
        }
    }
}

/// Hand `event` to whoever holds `channel`. One that stopped listening is
/// forgotten; its `Close` is on the way or already sent.
fn send(waiting: &mut Waiting, channel: Channel, event: ChannelEvent) {
    if let Some(events) = waiting.channels.get(&channel)
        && events.send(event).is_err()
    {
        waiting.channels.remove(&channel);
    }
}
