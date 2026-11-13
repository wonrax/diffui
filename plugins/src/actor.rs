//! One plugin, on its own thread.
//!
//! The thread owns the script engine and nothing else may touch it. Work
//! arrives as [`ToActor`] messages: calls from the app, replies to the
//! script's host calls, a reload. Every entry into the script is one bounded
//! turn, so a busy plugin stalls only itself, and nothing here ever waits on
//! the app: a host call is posted to [`Host`] or to a worker thread and its
//! reply comes back as another message.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::exec::{ExecRequest, Run};
use crate::builtin::http::FetchRequest;
use crate::builtin::kv::Kv;
use crate::builtin::{auth, exec, http};
use crate::host::{
    Host, HostError, LogLevel, LogLine, PluginError, PluginEvent, PluginState, Registration, Reply,
};
use crate::js::{Fault, Js, JsError, MainState, StartError, Turn};
use crate::manifest::{JjAccess, Manifest};
use crate::protocol::{HostCall, PluginCall, RepoCall};
use crate::{Limits, Options};

/// How many `http`, `exec` and `auth` calls of one plugin run at once; more
/// wait their turn.
const MAX_WORKERS: usize = 8;

/// How many log lines a plugin keeps for the app to show, and how long one
/// may be before it is cut.
const LOG_LINES: usize = 500;
const LOG_LINE_BYTES: usize = 16 * 1024;

/// How long a fetched token is reused before the helper is asked again.
const TOKEN_TTL: Duration = Duration::from_secs(300);

/// Lock a mutex whose data stays usable after a panic elsewhere.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) type ReplyFn = Box<dyn FnOnce(Result<Value, PluginError>) + Send>;

pub(crate) enum ToActor {
    Call {
        call: Box<PluginCall>,
        reply: ReplyFn,
    },
    /// The answer to a host call the script made. `io` marks one that held a
    /// worker.
    HostReply {
        generation: u64,
        id: u64,
        result: Result<Value, HostError>,
        io: bool,
    },
    /// A line of a streaming `exec`'s stdout.
    Chunk {
        generation: u64,
        id: u64,
        data: String,
    },
    Reload,
    Shutdown,
}

/// What the app's handle reads without asking the thread.
pub(crate) struct Status {
    pub state: PluginState,
    pub log: VecDeque<LogLine>,
}

/// What the script posts through `send`.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
enum FromScript {
    Call {
        id: u64,
        op: String,
        #[serde(default)]
        args: Value,
    },
    Cancel {
        id: u64,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(default)]
        value: Value,
        error: Option<JsError>,
    },
    Timer {
        id: u64,
        ms: f64,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    Uncaught {
        what: String,
        error: JsError,
    },
}

enum Phase {
    Loading {
        js: Js,
        deadline: Instant,
        /// Calls that arrived before the main module finished.
        waiting: Vec<(PluginCall, ReplyFn)>,
    },
    Ready {
        js: Js,
        registration: Registration,
    },
    Failed(String),
}

struct PendingCall {
    reply: ReplyFn,
    what: String,
    deadline: Option<Instant>,
}

type Job = Box<dyn FnOnce() -> Result<Value, HostError> + Send>;

pub(crate) struct Actor {
    manifest: Arc<Manifest>,
    config: serde_json::Map<String, Value>,
    limits: Limits,
    host: Arc<dyn Host>,
    status: Arc<Mutex<Status>>,
    inbox: Receiver<ToActor>,
    /// For replies from workers and from the host.
    own: Sender<ToActor>,
    phase: Phase,
    /// Bumped on every (re)load, so replies meant for an earlier load of the
    /// script are recognised and dropped.
    generation: u64,
    next_call: u64,
    /// Calls into the script that haven't settled.
    calls: HashMap<u64, PendingCall>,
    /// Host calls the script is waiting on, by its id: their cancel flags.
    host_calls: HashMap<u64, Arc<AtomicBool>>,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    workers: usize,
    queued: VecDeque<(u64, Job)>,
    agent: ureq::Agent,
    kv: Kv,
    /// Tokens `auth.token` fetched, by host, with when. Workers fill it.
    tokens: Arc<Mutex<HashMap<String, (String, Instant)>>>,
}

impl Actor {
    pub(crate) fn new(
        manifest: Arc<Manifest>,
        options: Options,
        host: Arc<dyn Host>,
        status: Arc<Mutex<Status>>,
        inbox: Receiver<ToActor>,
        own: Sender<ToActor>,
    ) -> Self {
        let (config, problems) = manifest.resolve_config(&options.config);
        let actor = Self {
            kv: Kv::new(options.data_dir.join(format!("{}.json", manifest.id))),
            manifest,
            config,
            limits: options.limits,
            host,
            status,
            inbox,
            own,
            phase: Phase::Failed(String::new()),
            generation: 0,
            next_call: 1,
            calls: HashMap::new(),
            host_calls: HashMap::new(),
            timers: BinaryHeap::new(),
            workers: 0,
            queued: VecDeque::new(),
            agent: http::agent(),
            tokens: Arc::default(),
        };
        for problem in problems {
            actor.log(LogLevel::Warn, problem);
        }
        actor
    }

    pub(crate) fn run(mut self) {
        self.guarded(Self::load);
        loop {
            let message = match self.next_wake() {
                Some(wake) => match self
                    .inbox
                    .recv_timeout(wake.saturating_duration_since(Instant::now()))
                {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.inbox.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                },
            };
            if matches!(message, Some(ToActor::Shutdown)) {
                break;
            }
            self.guarded(|actor| {
                if let Some(message) = message {
                    actor.handle(message);
                }
                actor.tick();
            });
        }
        self.stop(PluginError::Gone);
    }

    /// Run one step; a panic in it (ours or the engine's) fails the plugin
    /// instead of taking the thread, and with it every later call, down.
    fn guarded(&mut self, step: impl FnOnce(&mut Self)) {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| step(self))) {
            let what = panic
                .downcast_ref::<&str>()
                .map(|text| text.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            // The engine panicking for want of memory is the memory limit.
            match self.js().and_then(Js::fault) {
                Some(fault) => self.fail(self.fault_reason(fault)),
                None => self.fail(format!("the plugin host panicked: {what}")),
            }
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        let timer = self.timers.peek().map(|Reverse((due, _))| *due);
        let call = self.calls.values().filter_map(|call| call.deadline).min();
        let load = match &self.phase {
            Phase::Loading { deadline, .. } => Some(*deadline),
            _ => None,
        };
        [timer, call, load].into_iter().flatten().min()
    }

    fn js(&self) -> Option<&Js> {
        match &self.phase {
            Phase::Loading { js, .. } | Phase::Ready { js, .. } => Some(js),
            Phase::Failed(_) => None,
        }
    }

    fn log(&self, level: LogLevel, mut message: String) {
        if message.len() > LOG_LINE_BYTES {
            let cut = (0..=LOG_LINE_BYTES)
                .rev()
                .find(|at| message.is_char_boundary(*at))
                .unwrap_or(0);
            message.truncate(cut);
            message.push_str(" …");
        }
        let plugin = self.manifest.id.as_str();
        match level {
            LogLevel::Debug => tracing::debug!(plugin, "{message}"),
            LogLevel::Info => tracing::info!(plugin, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin, "{message}"),
            LogLevel::Error => tracing::error!(plugin, "{message}"),
        }
        let mut status = lock(&self.status);
        if status.log.len() == LOG_LINES {
            status.log.pop_front();
        }
        status.log.push_back(LogLine { level, message });
    }

    fn set_state(&self, state: PluginState) {
        lock(&self.status).state = state;
    }

    // Lifecycle.

    /// Start a fresh engine on the plugin's main module.
    fn load(&mut self) {
        self.generation += 1;
        self.set_state(PluginState::Loading);
        match Js::start(&self.manifest, &self.config, &self.limits) {
            Ok((js, turn)) => {
                self.phase = Phase::Loading {
                    js,
                    deadline: Instant::now() + self.limits.load,
                    waiting: Vec::new(),
                };
                self.after_turn(turn);
            }
            Err(StartError::Fault(fault)) => self.fail(self.fault_reason(fault)),
            Err(StartError::Threw(error)) => {
                self.fail(format!("{} did not load: {error}", self.manifest.main))
            }
        }
    }

    fn fault_reason(&self, fault: Fault) -> String {
        match fault {
            Fault::Budget => format!(
                "it ran for more than {} ms without yielding and was stopped",
                self.limits.turn.as_millis()
            ),
            Fault::Memory => format!(
                "it ran out of memory (the limit is {} MB)",
                self.limits.memory / (1024 * 1024)
            ),
        }
    }

    /// Once the main module has settled, either open for calls or fail.
    fn check_loaded(&mut self) {
        let Phase::Loading { js, deadline, .. } = &self.phase else {
            return;
        };
        let main = &self.manifest.main;
        let registration = match js.main_state() {
            MainState::Pending if Instant::now() < *deadline => return,
            MainState::Pending => Err(format!(
                "{main} did not finish loading within {} s: a top-level await never settled",
                self.limits.load.as_secs()
            )),
            MainState::Failed(error) => Err(format!("{main} did not load: {error}")),
            MainState::Loaded => js
                .finish_load()
                .map_err(|error| format!("{main} did not load: {error}"))
                .and_then(|registration| self.check_registration(registration)),
        };
        match registration {
            Err(why) => self.fail(why),
            Ok(registration) => {
                let Phase::Loading { js, waiting, .. } =
                    std::mem::replace(&mut self.phase, Phase::Failed(String::new()))
                else {
                    unreachable!("checked above");
                };
                self.phase = Phase::Ready {
                    js,
                    registration: registration.clone(),
                };
                self.set_state(PluginState::Ready(registration.clone()));
                self.host
                    .event(&self.manifest.id, PluginEvent::Loaded(registration));
                for (call, reply) in waiting {
                    self.call(call, reply);
                }
            }
        }
    }

    /// The manifest promised commands and maybe a forge; the script must
    /// have delivered them.
    fn check_registration(&self, registration: Registration) -> Result<Registration, String> {
        let main = &self.manifest.main;
        let missing: Vec<&str> = self
            .manifest
            .commands
            .iter()
            .map(|command| command.id.as_str())
            .filter(|id| {
                !registration
                    .commands
                    .iter()
                    .any(|registered| registered == id)
            })
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "plugin.toml declares {} but {main} never called commands.register for {}",
                if missing.len() == 1 {
                    "a command"
                } else {
                    "commands"
                },
                missing.join(", ")
            ));
        }
        if self.manifest.forge.is_some() && registration.forge.is_none() {
            return Err(format!(
                "plugin.toml provides a forge but {main} never called forge.register"
            ));
        }
        Ok(registration)
    }

    /// Stop the script for good (until a reload) and say why.
    fn fail(&mut self, why: String) {
        self.log(LogLevel::Error, why.clone());
        self.stop(PluginError::Failed(why.clone()));
        self.phase = Phase::Failed(why.clone());
        self.set_state(PluginState::Failed(why.clone()));
        self.host.event(&self.manifest.id, PluginEvent::Failed(why));
    }

    /// Drop the engine and answer everything that was waiting on it.
    fn stop(&mut self, why: PluginError) {
        if let Phase::Loading { waiting, .. } =
            std::mem::replace(&mut self.phase, Phase::Failed(String::new()))
        {
            for (_, reply) in waiting {
                reply(Err(why.clone()));
            }
        }
        for (_, call) in self.calls.drain() {
            (call.reply)(Err(why.clone()));
        }
        for (_, cancel) in self.host_calls.drain() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.queued.clear();
        self.timers.clear();
        lock(&self.tokens).clear();
    }

    // Messages.

    fn handle(&mut self, message: ToActor) {
        match message {
            ToActor::Call { call, reply } => self.call(*call, reply),
            ToActor::HostReply {
                generation,
                id,
                result,
                io,
            } => {
                if io {
                    self.workers -= 1;
                    self.start_workers();
                }
                if generation == self.generation && self.host_calls.remove(&id).is_some() {
                    let reply = match result {
                        Ok(value) => json!({ "t": "reply", "id": id, "ok": true, "value": value }),
                        Err(error) => json!({
                            "t": "reply", "id": id, "ok": false,
                            "error": { "name": error.name, "message": error.message },
                        }),
                    };
                    self.enter(&reply);
                }
            }
            ToActor::Chunk {
                generation,
                id,
                data,
            } => {
                if generation == self.generation && self.host_calls.contains_key(&id) {
                    self.enter(&json!({ "t": "chunk", "id": id, "data": data }));
                }
            }
            ToActor::Reload => {
                self.stop(PluginError::Gone);
                self.load();
            }
            ToActor::Shutdown => {}
        }
    }

    /// What time alone brings: due timers, calls past their deadline, a load
    /// that settled or ran out.
    fn tick(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((due, id))) = self.timers.peek().copied() {
            if due > now {
                break;
            }
            self.timers.pop();
            self.enter(&json!({ "t": "timer", "id": id }));
        }
        let late: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| call.deadline.is_some_and(|deadline| deadline <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some(call) = self.calls.remove(&id) {
                (call.reply)(Err(PluginError::TimedOut(call.what)));
            }
        }
        self.check_loaded();
    }

    /// A call from the app into the script.
    fn call(&mut self, call: PluginCall, reply: ReplyFn) {
        let registration = match &mut self.phase {
            Phase::Failed(why) => return reply(Err(PluginError::Failed(why.clone()))),
            Phase::Loading { waiting, .. } => return waiting.push((call, reply)),
            Phase::Ready { registration, .. } => registration,
        };
        let (what, deadline) = match &call {
            PluginCall::Command { id, .. } => {
                if !registration.commands.contains(id) {
                    return reply(Err(PluginError::NotRegistered(format!("command \"{id}\""))));
                }
                // A command may sit in a prompt for as long as the user does.
                (format!("command \"{id}\""), None)
            }
            PluginCall::Forge { call, .. } => {
                let method = call.method();
                let registered = registration
                    .forge
                    .as_ref()
                    .is_some_and(|methods| methods.iter().any(|m| m == method));
                if !registered {
                    return reply(Err(PluginError::NotRegistered(format!(
                        "forge method {method}"
                    ))));
                }
                (
                    format!("forge.{method}"),
                    Some(Instant::now() + self.limits.forge_call),
                )
            }
            PluginCall::Event { event } => {
                if !registration.events.iter().any(|name| name == event.name()) {
                    return reply(Ok(Value::Null));
                }
                (format!("event {}", event.name()), None)
            }
        };
        let id = self.next_call;
        self.next_call += 1;
        self.calls.insert(
            id,
            PendingCall {
                reply,
                what,
                deadline,
            },
        );
        self.enter(&json!({ "t": "call", "id": id, "call": call }));
    }

    /// One turn of the script on `message`, then everything it posted.
    fn enter(&mut self, message: &Value) {
        let Some(js) = self.js() else { return };
        let turn = js.dispatch(&message.to_string());
        self.after_turn(turn);
    }

    fn after_turn(&mut self, turn: Turn) {
        for error in &turn.uncaught {
            self.log(
                LogLevel::Error,
                format!("Uncaught {error}\n{}", error.stack.trim_end()),
            );
        }
        // A fault ends the script before anything it said in that turn
        // counts, so every waiting caller hears the same reason.
        if let Some(fault) = turn.fault {
            return self.fail(self.fault_reason(fault));
        }
        for message in self.js().map(Js::take_outbox).unwrap_or_default() {
            match serde_json::from_str(&message) {
                Ok(message) => self.script_said(message),
                Err(error) => self.log(
                    LogLevel::Error,
                    format!("unreadable message from the script: {error}"),
                ),
            }
        }
        self.check_loaded();
    }

    /// Act on one message from the script.
    fn script_said(&mut self, message: FromScript) {
        match message {
            FromScript::Call { id, op, args } => self.host_call(id, &op, args),
            FromScript::Cancel { id } => {
                // The script already rejected its promise; this stops the work.
                if let Some(cancel) = self.host_calls.remove(&id) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            FromScript::Result {
                id,
                ok,
                value,
                error,
            } => {
                let Some(call) = self.calls.remove(&id) else {
                    return;
                };
                if ok {
                    (call.reply)(Ok(value));
                } else {
                    let error = error.unwrap_or_default();
                    self.log(
                        LogLevel::Error,
                        format!("{} failed: {error}\n{}", call.what, error.stack.trim_end()),
                    );
                    (call.reply)(Err(PluginError::Threw(error)));
                }
            }
            FromScript::Timer { id, ms } => {
                let delay = Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(Duration::MAX);
                // A delay too long to add is one that never fires.
                if let Some(due) = Instant::now().checked_add(delay) {
                    self.timers.push(Reverse((due, id)));
                }
            }
            FromScript::Log { level, message } => self.log(level, message),
            FromScript::Uncaught { what, error } => {
                self.log(
                    LogLevel::Error,
                    format!("Uncaught in {what}: {error}\n{}", error.stack.trim_end()),
                );
            }
        }
    }

    // Host calls.

    fn answer(&self, id: u64, result: Result<Value, HostError>) {
        let _ = self.own.send(ToActor::HostReply {
            generation: self.generation,
            id,
            result,
            io: false,
        });
    }

    fn host_call(&mut self, id: u64, op: &str, args: Value) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.host_calls.insert(id, cancel.clone());
        if let Err(refused) = self.route(id, op, args, cancel) {
            self.answer(id, Err(refused));
        }
    }

    /// Check the call against the manifest and send it where it is answered:
    /// here, a worker, or the app.
    fn route(
        &mut self,
        id: u64,
        op: &str,
        args: Value,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), HostError> {
        fn read<T: serde::de::DeserializeOwned>(op: &str, args: Value) -> Result<T, HostError> {
            serde_json::from_value(args)
                .map_err(|error| HostError::new("TypeError", format!("{op}: {error}")))
        }
        #[derive(Deserialize)]
        struct Key {
            key: String,
            #[serde(default)]
            value: Value,
        }
        #[derive(Deserialize)]
        struct TokenFor {
            host: String,
        }
        let manifest = self.manifest.clone();
        let limits = self.limits;
        match op {
            "http.fetch" => {
                let request: FetchRequest = read(op, args)?;
                http::check(&request.url, &manifest.permissions.http)?;
                let agent = self.agent.clone();
                self.spawn(
                    id,
                    Box::new(move || {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(HostError::new("AbortError", "The operation was aborted"));
                        }
                        http::fetch(
                            &agent,
                            request,
                            &manifest.permissions.http,
                            limits.http_timeout,
                            limits.max_response,
                        )
                    }),
                );
            }
            "exec" => {
                let request: ExecRequest = read(op, args)?;
                exec::check(&request.argv, &manifest.permissions.exec)?;
                let chunks = request.stream.then(|| (self.own.clone(), self.generation));
                self.spawn(
                    id,
                    Box::new(move || {
                        let run = Run {
                            argv: &request.argv,
                            stdin: request.stdin,
                            cwd: request.cwd.as_deref().map_or(manifest.dir.as_path(), std::path::Path::new),
                            env: &[],
                            timeout: request.timeout_ms.map_or(limits.exec_timeout, Duration::from_millis),
                            max_output: limits.max_response as usize,
                        };
                        let output = exec::run(run, &cancel, |line| {
                            if let Some((to, generation)) = &chunks {
                                let _ = to.send(ToActor::Chunk {
                                    generation: *generation,
                                    id,
                                    data: line.to_string(),
                                });
                            }
                        })?;
                        Ok(json!({ "code": output.code, "stdout": output.stdout, "stderr": output.stderr }))
                    }),
                );
            }
            "auth.token" => {
                let TokenFor { host } = read(op, args)?;
                auth::check(&host, &manifest.permissions.http)?;
                let fresh = lock(&self.tokens)
                    .get(&host)
                    .filter(|(_, fetched)| fetched.elapsed() < TOKEN_TTL)
                    .map(|(token, _)| token.clone());
                match fresh {
                    Some(token) => self.answer(id, Ok(json!(token))),
                    None => {
                        let token_cmd = auth::token_cmd(&self.config)?;
                        let tokens = self.tokens.clone();
                        self.spawn(
                            id,
                            Box::new(move || {
                                let token = auth::token(
                                    &host,
                                    token_cmd.as_deref(),
                                    &manifest.dir,
                                    &cancel,
                                )?;
                                lock(&tokens).insert(host, (token.clone(), Instant::now()));
                                Ok(json!(token))
                            }),
                        );
                    }
                }
            }
            "kv.get" => {
                let result = self.kv.get(&read::<Key>(op, args)?.key);
                self.answer(id, result);
            }
            "kv.set" => {
                let Key { key, value } = read(op, args)?;
                let result = self.kv.set(key, value).map(|()| Value::Null);
                self.answer(id, result);
            }
            "kv.delete" => {
                let result = self
                    .kv
                    .delete(&read::<Key>(op, args)?.key)
                    .map(|()| Value::Null);
                self.answer(id, result);
            }
            "repo" | "ui" => {
                let call: HostCall = read(op, json!({ "op": op, "args": args }))?;
                if let HostCall::Repo { call, .. } = &call {
                    let needs = match call {
                        RepoCall::JjRun { .. } => JjAccess::Write,
                        _ => JjAccess::Read,
                    };
                    if manifest.permissions.jj < needs {
                        let (wanted, name) = match needs {
                            JjAccess::Write => ("write", call.name()),
                            _ => ("read", call.name()),
                        };
                        return Err(HostError::denied(format!(
                            "{name} is not allowed: it needs jj = \"{wanted}\" under [permissions] in plugin.toml"
                        )));
                    }
                }
                let reply = Reply {
                    to: Some(self.own.clone()),
                    generation: self.generation,
                    id,
                    cancelled: cancel,
                };
                self.host.call(&manifest.id, call, reply);
            }
            other => {
                return Err(HostError::new(
                    "Error",
                    format!("there is no host function \"{other}\""),
                ));
            }
        }
        Ok(())
    }

    /// Run `job` on a worker thread, or queue it while all are busy.
    fn spawn(&mut self, id: u64, job: Job) {
        self.queued.push_back((id, job));
        self.start_workers();
    }

    fn start_workers(&mut self) {
        while self.workers < MAX_WORKERS {
            let Some((id, job)) = self.queued.pop_front() else {
                break;
            };
            self.workers += 1;
            let own = self.own.clone();
            let generation = self.generation;
            let report = move |result| {
                let _ = own.send(ToActor::HostReply {
                    generation,
                    id,
                    result,
                    io: true,
                });
            };
            let spawned = std::thread::Builder::new()
                .name(format!("plugin-{}-io", self.manifest.id))
                .spawn({
                    let report = report.clone();
                    move || {
                        let result = catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|_| {
                            Err(HostError::new("Error", "the host function panicked"))
                        });
                        report(result);
                    }
                });
            if let Err(error) = spawned {
                report(Err(HostError::new(
                    "Error",
                    format!("no thread for the call: {error}"),
                )));
            }
        }
    }
}
