//! A plugin directory on disk, a host that records what it is asked, and a
//! tiny HTTP server.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diffui_plugins::protocol::{
    Bookmark, Diff, ForgeCall, ForgeRemote, HostCall, JjRun, PluginCall, RepoCall, RepoHandle, Row,
    Selection, UiCall,
};
use diffui_plugins::{
    Host, Limits, LogLine, Manifest, Options, Plugin, PluginError, PluginEvent, Registration, Reply,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Long enough for a loaded machine, short enough that a hang fails the run.
pub const PATIENCE: Duration = Duration::from_secs(20);

/// The operation the fake host considers current; a handle with another is
/// stale.
pub const CURRENT_OP: &str = "op-current";

pub struct FakeHost {
    events: Mutex<Sender<PluginEvent>>,
    /// Every call that reached the host, in order.
    pub calls: Mutex<Vec<HostCall>>,
    /// Replies held back by a `promptInput` labelled "hold", for a test to
    /// answer or drop.
    pub held: Mutex<Vec<Reply>>,
}

impl Host for FakeHost {
    fn call(&self, _plugin: &str, call: HostCall, reply: Reply) {
        self.calls.lock().unwrap().push(call.clone());
        match call {
            HostCall::Repo { repo, .. } if repo.op != CURRENT_OP => reply.stale(),
            HostCall::Repo { call, .. } => match call {
                RepoCall::Bookmarks => reply.ok(vec![Bookmark {
                    name: "main".into(),
                    targets: vec!["c0ffee".into()],
                    remotes: vec![],
                }]),
                RepoCall::Remotes => reply
                    .ok(json!([{ "name": "origin", "url": "https://codeberg.org/team/app.git" }])),
                RepoCall::Selection => reply.ok(Selection::default()),
                RepoCall::Rows { revset, limit } => reply.ok(vec![Row {
                    change_id: "kkmpptxz".into(),
                    commit_id: "c0ffee".into(),
                    description: format!("rows of {revset} (limit {limit})"),
                    author: "Ada".into(),
                    timestamp: Some(1_700_000_000_000),
                    bookmarks: vec!["main".into()],
                    working_copy: true,
                    immutable: false,
                    empty: false,
                    conflict: false,
                }]),
                RepoCall::Diff {
                    revset,
                    format,
                    max_bytes,
                } => reply.ok(Diff {
                    text: format!("{format:?} diff of {revset} up to {max_bytes}"),
                    truncated: false,
                }),
                RepoCall::ReadFile { path, .. } if path == "missing" => reply.ok(Value::Null),
                RepoCall::ReadFile { rev, path } => reply.ok(format!("{path} at {rev}")),
                RepoCall::JjQuery { args } => reply.ok(format!("jj {}", args.join(" "))),
                RepoCall::JjRun { args } => reply.ok(JjRun {
                    summary: format!("Ran jj {}", args.join(" ")),
                }),
            },
            HostCall::Ui(ui) => match ui {
                UiCall::ActivityStart { .. } => reply.ok(7),
                UiCall::PromptInput { label, .. } if label == "hold" => {
                    self.held.lock().unwrap().push(reply)
                }
                UiCall::PromptInput { value, .. } => {
                    reply.ok(value.map(|value| format!("{value}!")))
                }
                UiCall::PromptConfirm { destructive, .. } => reply.ok(!destructive),
                UiCall::PromptChoose { choices, .. } => {
                    reply.ok(choices.last().map(|choice| choice.value.clone()))
                }
                _ => reply.ok(Value::Null),
            },
        }
    }

    fn event(&self, _plugin: &str, event: PluginEvent) {
        let _ = self.events.lock().unwrap().send(event);
    }
}

/// A plugin directory written for one test and removed with it.
pub struct TestPlugin {
    pub plugin: Plugin,
    pub host: Arc<FakeHost>,
    pub dir: PathBuf,
    events: Receiver<PluginEvent>,
}

impl Drop for TestPlugin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

/// A fresh directory under cargo's per-target scratch space.
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "plugins-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The manifest most tests start from: plugin `t`, one command, a forge.
pub const MANIFEST: &str = r#"
id = "t"
name = "Test"
version = "0.0.1"
api = 0
main = "main.js"

[provides]
forge = { hosts = ["forge.test"] }

[[commands]]
id = "t.run"
label = "Run"
"#;

pub struct Setup<'a> {
    pub manifest: String,
    pub files: Vec<(&'a str, String)>,
    pub config: String,
    pub limits: Limits,
}

impl<'a> Setup<'a> {
    /// [`MANIFEST`] plus `extra` manifest tables, with `main.js` as given.
    pub fn new(extra: &str, main: &str) -> Self {
        Self::with_manifest(format!("{MANIFEST}\n{extra}\n"), main)
    }

    pub fn with_manifest(manifest: String, main: &str) -> Self {
        Setup {
            manifest,
            files: vec![("main.js", main.to_string())],
            config: String::new(),
            limits: Limits::default(),
        }
    }

    pub fn file(mut self, name: &'a str, text: &str) -> Self {
        self.files.push((name, text.to_string()));
        self
    }

    pub fn config(mut self, toml: &str) -> Self {
        self.config = toml.to_string();
        self
    }

    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn spawn(self) -> TestPlugin {
        let root = scratch("plugin");
        let dir = root.join("plugin");
        for (name, text) in
            std::iter::once(("plugin.toml", self.manifest.clone())).chain(self.files)
        {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let manifest = Manifest::load(&dir).unwrap_or_else(|problems| {
            panic!(
                "the test manifest is wrong:\n{}",
                problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
        let (events, heard) = mpsc::channel();
        let host = Arc::new(FakeHost {
            events: Mutex::new(events),
            calls: Mutex::default(),
            held: Mutex::default(),
        });
        let options = Options {
            config: toml::from_str(&self.config).unwrap(),
            data_dir: root.join("data"),
            limits: self.limits,
        };
        TestPlugin {
            plugin: Plugin::spawn(manifest, options, host.clone()),
            host,
            dir,
            events: heard,
        }
    }
}

/// A plugin whose `checks(repo, name)` runs the test case `name` from
/// `cases` and returns whatever it returns: a way to evaluate script in the
/// plugin and read the result.
pub fn probe(extra_manifest: &str, cases: &str) -> Setup<'static> {
    Setup::new(
        extra_manifest,
        &format!(
            r#"import * as diffui from "diffui";
const {{ auth, commands, config, events, exec, forge, http, kv, ui }} = diffui;
commands.register("t.run", async () => {{}});
const cases = {{
{cases}
}};
forge.register({{
  async pullRequests() {{ return []; }},
  async checks(repo, name) {{ return (await cases[name](repo)) ?? null; }},
}});
"#
        ),
    )
}

/// The repository handle the fake host calls current.
pub fn repo() -> RepoHandle {
    RepoHandle {
        id: "tab-1".into(),
        op: CURRENT_OP.into(),
        root: "/work/app".into(),
        forge_remote: Some(ForgeRemote {
            host: "forge.test".into(),
            owner: "team".into(),
            name: "app".into(),
            web_url: "https://forge.test/team/app".into(),
        }),
    }
}

impl TestPlugin {
    pub fn next_event(&self) -> PluginEvent {
        self.events
            .recv_timeout(PATIENCE)
            .expect("the plugin said nothing")
    }

    /// Wait for the load to end well.
    pub fn loaded(&self) -> Registration {
        match self.next_event() {
            PluginEvent::Loaded(registration) => registration,
            PluginEvent::Failed(why) => panic!("the plugin failed to load: {why}"),
        }
    }

    /// Wait for the plugin to fail and return why.
    pub fn failed(&self) -> String {
        match self.next_event() {
            PluginEvent::Failed(why) => why,
            PluginEvent::Loaded(registration) => panic!("the plugin loaded: {registration:?}"),
        }
    }

    /// Call and wait for the reply.
    pub fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        call: PluginCall,
    ) -> Result<T, PluginError> {
        self.start(call)
            .recv_timeout(PATIENCE)
            .expect("the call never settled")
    }

    /// Call without waiting.
    pub fn start<T: DeserializeOwned + Send + 'static>(
        &self,
        call: PluginCall,
    ) -> Receiver<Result<T, PluginError>> {
        let (reply, result) = mpsc::channel();
        self.plugin.call(call, move |outcome| {
            let _ = reply.send(outcome);
        });
        result
    }

    /// Run the probe's test case `name`.
    pub fn case(&self, name: &str) -> Result<Value, PluginError> {
        self.call(case(name, repo()))
    }

    pub fn run_command(&self) -> Result<Value, PluginError> {
        self.call(command("t.run"))
    }

    pub fn log_text(&self) -> String {
        self.plugin
            .log()
            .iter()
            .map(|LogLine { level, message }| format!("{level:?} {message}\n"))
            .collect()
    }
}

pub fn case(name: &str, repo: RepoHandle) -> PluginCall {
    PluginCall::Forge {
        repo,
        call: ForgeCall::Checks { sha: name.into() },
    }
}

pub fn command(id: &str) -> PluginCall {
    PluginCall::Command {
        id: id.into(),
        context: diffui_plugins::protocol::CommandContext {
            repo: repo(),
            change_id: Some("kkmpptxz".into()),
            commit_id: Some("c0ffee".into()),
            bookmark: Some("feature".into()),
            path: None,
            line: None,
        },
    }
}

/// The message of a handler's exception, or a panic if the call succeeded.
pub fn threw(result: Result<Value, PluginError>) -> diffui_plugins::JsError {
    match result {
        Err(PluginError::Threw(error)) => error,
        other => panic!("expected the handler to throw, got {other:?}"),
    }
}

// The HTTP server.

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, body: Value) -> Self {
        Response {
            status,
            headers: vec![("content-type", "application/json".into())],
            body: body.to_string(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response {
            status,
            headers: vec![("content-type", "text/plain".into())],
            body: body.into(),
        }
    }
}

/// An HTTP/1.1 server on a loopback port, one thread per connection, alive
/// for the rest of the test process.
pub struct Server {
    /// `127.0.0.1:<port>`.
    pub host: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    pub fn start(respond: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let respond = Arc::new(respond);
        std::thread::spawn({
            let requests = requests.clone();
            move || {
                for stream in listener.incoming().flatten() {
                    let (requests, respond) = (requests.clone(), respond.clone());
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let Some(request) = read_request(&mut reader) else {
                            return;
                        };
                        requests.lock().unwrap().push(request.clone());
                        let response = respond(&request);
                        let mut head = format!(
                            "HTTP/1.1 {} Status\r\ncontent-length: {}\r\nconnection: close\r\n",
                            response.status,
                            response.body.len()
                        );
                        for (name, value) in &response.headers {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                        let mut stream = stream;
                        let _ = stream.write_all(format!("{head}\r\n{}", response.body).as_bytes());
                    });
                }
            }
        });
        Server { host, requests }
    }

    pub fn seen(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut words = line.split_whitespace();
    let (method, path) = (words.next()?.to_string(), words.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}
