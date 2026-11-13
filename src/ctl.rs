//! `diffui ctl`: drive the running app from a terminal or an agent.
//!
//! The app listens on a unix socket in a private directory (mode 0700, the
//! socket 0600), the way the credential relay does (see
//! [`diffui_core::askpass`]), and writes the socket's path to the state file
//! `ctl-socket` so a client finds it. A request is one line of JSON naming a
//! `verb`; the answer is one line of JSON with `"ok"` and what the verb
//! returns, or `"ok": false` and an `"error"`.
//!
//! `diffui ctl <verb> …` is the client; `--json` prints the answer as it
//! came. `diffui skill` prints the agent guide (`assets/agent/SKILL.md`).

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use futures::channel::{mpsc, oneshot};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app_files::{AppDir, app_file, write_atomic};
use crate::commands::CommandArg;
use crate::{Cx, Diffui, TabId};

/// The agent guide `diffui skill` prints.
pub(crate) const SKILL: &str = include_str!("../assets/agent/SKILL.md");

/// One request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Request {
    /// The open repositories, the active one, its selected revision and
    /// revset, and its latest activity.
    State,
    /// Open a repository (or a GitHub PR reference) as a tab.
    Open { path: String },
    /// Show the revision `revset` names, the way `jj show` would.
    Select {
        revset: String,
        #[serde(default)]
        repo: Option<String>,
    },
    /// Every command id, with whether it can run now.
    Commands {
        #[serde(default)]
        repo: Option<String>,
    },
    /// Run a command by id, as its key would.
    Run {
        id: String,
        #[serde(default)]
        repo: Option<String>,
    },
    /// Run a jj command line in the app, as the palette would.
    Jj {
        args: Vec<String>,
        #[serde(default)]
        repo: Option<String>,
    },
    /// Show a toast.
    Toast { text: String },
    /// Read config.toml's keys and commands again.
    Reload,
}

/// Where an answer goes.
pub(crate) type Reply = oneshot::Sender<Value>;

type Incoming = (Request, Reply);

static INCOMING: Mutex<Option<mpsc::UnboundedReceiver<Incoming>>> = Mutex::new(None);

/// The file that names the running app's socket.
fn socket_file() -> Option<PathBuf> {
    app_file(AppDir::State, "ctl-socket")
}

fn failure(error: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": error.to_string() })
}

/// Start listening. Call once at startup; a failure leaves `diffui ctl`
/// saying the app isn't reachable.
#[cfg(unix)]
pub(crate) fn listen() -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = std::env::temp_dir().join(format!("diffui-ctl-{}-{:x}", std::process::id(), nonce()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let socket = dir.join("s");
    let (sender, receiver) = mpsc::unbounded();
    serve(&socket, sender)?;
    *INCOMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(receiver);
    if let Some(file) = socket_file() {
        write_atomic(&file, socket.as_os_str().as_encoded_bytes())?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn listen() -> std::io::Result<()> {
    Err(std::io::Error::other("diffui ctl needs a unix platform"))
}

#[cfg(unix)]
fn nonce() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(std::process::id().into());
    hasher.finish()
}

/// Bind `socket` (0600) and hand each request to `sender`, answering with
/// what comes back.
#[cfg(unix)]
fn serve(socket: &Path, sender: mpsc::UnboundedSender<Incoming>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("ctl".to_owned())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let sender = sender.clone();
                let _ = std::thread::Builder::new()
                    .name("ctl-request".to_owned())
                    .spawn(move || {
                        if let Err(error) = answer(stream, &sender) {
                            tracing::warn!("ctl connection failed: {error}");
                        }
                    });
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn answer(
    stream: std::os::unix::net::UnixStream,
    sender: &mpsc::UnboundedSender<Incoming>,
) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let response = match serde_json::from_str::<Request>(&line) {
        Err(error) => failure(format!("bad request: {error}")),
        Ok(request) => {
            let (reply, answer) = oneshot::channel();
            if sender.unbounded_send((request, reply)).is_err() {
                failure("the app is shutting down")
            } else {
                futures::executor::block_on(answer)
                    .unwrap_or_else(|_| failure("the app dropped the request"))
            }
        }
    };
    let mut stream = stream;
    writeln!(stream, "{response}")?;
    stream.flush()
}

/// Starts the loop that carries requests into the app.
pub(crate) fn start(cx: &mut Cx) {
    use futures::StreamExt as _;
    let Some(mut incoming) = INCOMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    else {
        return;
    };
    cx.spawn(async move |this, cx| {
        while let Some((request, reply)) = incoming.next().await {
            let updated = this.update(cx, |this, cx| {
                this.act(cx, |this, cx| this.on_ctl_request(request, reply, cx));
            });
            if updated.is_err() {
                return;
            }
        }
    })
    .detach();
}

impl Diffui {
    /// Carry out one `ctl` request; `reply` hears the answer, now or once a
    /// jj line has planned.
    pub(crate) fn on_ctl_request(&mut self, request: Request, reply: Reply, cx: &mut Cx) {
        let answer = match request {
            Request::State => Ok(self.ctl_state()),
            Request::Open { path } => {
                self.open_repository(&path, cx);
                Ok(self.ctl_state())
            }
            Request::Toast { text } => {
                self.push_info_toast(text, "");
                Ok(json!({}))
            }
            Request::Reload => self
                .reload_commands()
                .map(|problems| json!({ "problems": problems })),
            Request::Commands { repo } => self
                .ctl_tab(repo.as_deref(), cx)
                .map(|_| self.ctl_commands()),
            Request::Run { id, repo } => self
                .ctl_tab(repo.as_deref(), cx)
                .and_then(|_| self.ctl_run(&id, cx)),
            Request::Select { revset, repo } => {
                return self.ctl_line(repo.as_deref(), vec!["show".to_owned(), revset], reply, cx);
            }
            Request::Jj { args, repo } => return self.ctl_line(repo.as_deref(), args, reply, cx),
        };
        let _ = reply.send(match answer {
            Ok(Value::Object(mut fields)) => {
                fields.insert("ok".to_owned(), Value::Bool(true));
                Value::Object(fields)
            }
            Ok(other) => json!({ "ok": true, "value": other }),
            Err(error) => failure(error),
        });
    }

    /// The tab a request names by its repository's path (any path inside
    /// it), brought forward; the active tab when it names none.
    fn ctl_tab(&mut self, repo: Option<&str>, cx: &mut Cx) -> Result<TabId, String> {
        let Some(repo) = repo else {
            return self
                .active_tab_id()
                .ok_or_else(|| "No repository is open".to_owned());
        };
        let wanted = std::fs::canonicalize(repo).unwrap_or_else(|_| PathBuf::from(repo));
        let tab = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let root = &tab.state.repository.as_ref()?.root;
                let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                wanted
                    .starts_with(&root)
                    .then_some((tab.id, root.as_os_str().len()))
            })
            .max_by_key(|(_, depth)| *depth)
            .map(|(id, _)| id)
            .ok_or_else(|| format!("No open repository holds {repo}"))?;
        self.activate_tab(tab, cx);
        Ok(tab)
    }

    fn ctl_state(&self) -> Value {
        let repos: Vec<Value> = self
            .tabs
            .iter()
            .map(|tab| {
                let state = &tab.state;
                json!({
                    "tab": tab.id.0,
                    "root": state.repository.as_ref().map(|repository| repository.root.display().to_string()),
                    "kind": match &state.repository {
                        Some(repository) if repository.vcs == crate::Vcs::Jj => "jj",
                        Some(_) => "git",
                        None => "pr",
                    },
                    "active": self.is_active(tab.id),
                })
            })
            .collect();
        let session = &self.active().session;
        let selected = session.commits.find(&session.selected_revision).map(|row| {
            json!({
                "change_id": row.change_id(),
                "commit_id": row.commit_id(),
                "description": row.description().lines().next().unwrap_or(""),
                "bookmarks": row.bookmarks(),
                "working_copy": row.is_working_copy(),
            })
        });
        let activity: Vec<Value> = self
            .active()
            .activities
            .recent(10)
            .map(|activity| {
                json!({
                    "label": activity.label.to_string(),
                    "status": format!("{:?}", activity.status).to_lowercase(),
                    "result": activity.result,
                    "command": activity.command,
                })
            })
            .collect();
        json!({
            "repos": repos,
            "active": self.active_tab_id().map(|tab| tab.0),
            "selected": selected,
            "revset": session.revset,
            "activity": activity,
        })
    }

    fn ctl_commands(&self) -> Value {
        let commands: Vec<Value> = self
            .registry
            .iter()
            .map(|command| {
                let verdict = command.enabled(self, &CommandArg::None);
                json!({
                    "id": command.id.as_str(),
                    "label": command.palette_label,
                    "declared": command.declared().is_some(),
                    "available": verdict.is_ok(),
                    "reason": verdict.err(),
                })
            })
            .collect();
        json!({ "commands": commands })
    }

    fn ctl_run(&mut self, id: &str, cx: &mut Cx) -> Result<Value, String> {
        let registry = self.registry.clone();
        let command = registry.get(id).ok_or_else(|| format!("No command {id}"))?;
        command
            .enabled(self, &CommandArg::None)
            .map_err(|reason| format!("{id} can't run: {reason}"))?;
        self.run_command(id, CommandArg::None, cx);
        Ok(json!({}))
    }

    /// Run `args` as a jj line on the tab `repo` names, answering once it
    /// has planned: what it does, or why it won't.
    fn ctl_line(&mut self, repo: Option<&str>, args: Vec<String>, reply: Reply, cx: &mut Cx) {
        let tab = match self.ctl_tab(repo, cx) {
            Ok(tab) => tab,
            Err(error) => {
                let _ = reply.send(failure(error));
                return;
            }
        };
        let line = crate::user_commands::jj_line(&args);
        let reply = std::cell::Cell::new(Some(reply));
        let answer = move |outcome: Result<String, String>| {
            if let Some(reply) = reply.take() {
                let _ = reply.send(match outcome {
                    Ok(did) => json!({ "ok": true, "did": did }),
                    Err(error) => failure(error),
                });
            }
        };
        let answer = std::rc::Rc::new(answer);
        let on_done = answer.clone();
        let started = self.run_jj_line(
            tab,
            &line,
            Some(Box::new(move |_, outcome| {
                on_done(outcome.map_err(|error| error.0));
            })),
            cx,
        );
        if let Err(error) = started {
            answer(Err(error.0));
        }
    }
}

// ── The client ──────────────────────────────────────────────────────────────

/// `diffui ctl …` / `diffui skill`: the exit code, or `None` for a normal
/// launch.
pub(crate) fn client_main() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    match args.next()?.to_str()? {
        "skill" => {
            print!("{SKILL}");
            Some(0)
        }
        "ctl" => Some(ctl_main()),
        _ => None,
    }
}

#[derive(Debug, clap::Parser)]
#[command(
    name = "diffui ctl",
    about = "Drive the running diffui (see `diffui skill`)"
)]
struct CtlCli {
    /// Print the app's answer as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// The repository to act on (any path inside it); the active tab when
    /// left out.
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<String>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Debug, clap::Subcommand)]
enum Verb {
    /// Open repositories, the active one, its selection, revset and activity.
    State,
    /// Open a repository (or a GitHub PR reference) as a tab.
    Open { path: String },
    /// Show the revision a revset names.
    Select { revset: String },
    /// Every command id, and whether it can run now.
    Commands,
    /// Run a command by id.
    Run { id: String },
    /// Run a jj command in the app: its activity row, queue and confirms.
    Jj {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Show a toast.
    Toast { text: String },
    /// Read config.toml's keys and commands again.
    Reload,
}

fn ctl_main() -> i32 {
    use clap::Parser as _;
    let cli = CtlCli::parse_from(
        std::iter::once(std::ffi::OsString::from("diffui ctl")).chain(std::env::args_os().skip(2)),
    );
    let repo = cli.repo.clone().map(|repo| {
        std::fs::canonicalize(&repo)
            .map(|path| path.display().to_string())
            .unwrap_or(repo)
    });
    let request = match cli.verb {
        Verb::State => Request::State,
        Verb::Open { path } => Request::Open {
            path: std::fs::canonicalize(&path)
                .map(|path| path.display().to_string())
                .unwrap_or(path),
        },
        Verb::Select { revset } => Request::Select { revset, repo },
        Verb::Commands => Request::Commands { repo },
        Verb::Run { id } => Request::Run { id, repo },
        Verb::Jj { args } => Request::Jj { args, repo },
        Verb::Toast { text } => Request::Toast { text },
        Verb::Reload => Request::Reload,
    };
    let response = socket_file()
        .ok_or_else(|| "no home directory".to_owned())
        .and_then(|file| {
            std::fs::read(&file)
                .map_err(|_| "diffui isn't running (start it, then try again)".to_owned())
        })
        .and_then(|socket| {
            let socket = PathBuf::from(String::from_utf8_lossy(&socket).into_owned());
            call(&socket, &request)
        });
    let response = match response {
        Ok(response) => response,
        Err(error) => failure(error),
    };
    if cli.json {
        println!("{response}");
    } else {
        print_human(&request, &response);
    }
    if response["ok"] == Value::Bool(true) {
        0
    } else {
        1
    }
}

/// Send one request to the app at `socket` and read its answer.
#[cfg(unix)]
fn call(socket: &Path, request: &Request) -> Result<Value, String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket)
        .map_err(|_| "diffui isn't running (start it, then try again)".to_owned())?;
    // A jj line answers once its revsets resolve; a repository that never
    // loads would otherwise hold the client forever.
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(120)))
        .map_err(|error| error.to_string())?;
    let line = serde_json::to_string(request).map_err(|error| error.to_string())?;
    writeln!(stream, "{line}").map_err(|error| error.to_string())?;
    let mut answer = String::new();
    BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&answer).map_err(|error| format!("bad answer: {error}"))
}

#[cfg(not(unix))]
fn call(_socket: &Path, _request: &Request) -> Result<Value, String> {
    Err("diffui ctl needs a unix platform".to_owned())
}

fn print_human(request: &Request, response: &Value) {
    if response["ok"] != Value::Bool(true) {
        eprintln!(
            "diffui ctl: {}",
            response["error"].as_str().unwrap_or("it failed")
        );
        return;
    }
    match request {
        Request::State | Request::Open { .. } => {
            for repo in response["repos"].as_array().into_iter().flatten() {
                let mark = if repo["active"] == Value::Bool(true) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{mark} {} ({})",
                    repo["root"].as_str().unwrap_or("(pull request)"),
                    repo["kind"].as_str().unwrap_or("")
                );
            }
            if let Some(selected) = response["selected"].as_object() {
                println!(
                    "selected: {} {}",
                    selected["change_id"].as_str().unwrap_or(""),
                    selected["description"].as_str().unwrap_or("")
                );
            }
            println!("revset: {}", response["revset"].as_str().unwrap_or(""));
        }
        Request::Commands { .. } => {
            for command in response["commands"].as_array().into_iter().flatten() {
                let id = command["id"].as_str().unwrap_or("");
                match command["reason"].as_str() {
                    Some(reason) => println!("{id}\t(off: {reason})"),
                    None => println!("{id}"),
                }
            }
        }
        Request::Jj { .. } | Request::Select { .. } => {
            if let Some(did) = response["did"].as_str() {
                println!("{did}");
            }
        }
        Request::Reload => {
            for problem in response["problems"].as_array().into_iter().flatten() {
                println!("{}", problem.as_str().unwrap_or(""));
            }
        }
        Request::Run { .. } | Request::Toast { .. } => {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn requests_read_the_way_the_skill_writes_them() {
        let read = |line: &str| serde_json::from_str::<Request>(line).expect(line);
        assert_eq!(read(r#"{"verb":"state"}"#), Request::State);
        assert_eq!(
            read(r#"{"verb":"jj","args":["log","-r","@"]}"#),
            Request::Jj {
                args: vec!["log".to_owned(), "-r".to_owned(), "@".to_owned()],
                repo: None
            }
        );
        assert_eq!(
            read(r#"{"verb":"run","id":"repo.refresh","repo":"/r"}"#),
            Request::Run {
                id: "repo.refresh".to_owned(),
                repo: Some("/r".to_owned())
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"verb":"nope"}"#).is_err());
    }

    /// The guide names every when-clause key and template variable the app
    /// has, so an agent reading it isn't missing one.
    #[test]
    fn the_skill_names_every_key_and_variable() {
        for key in crate::when::KEYS {
            assert!(SKILL.contains(&format!("`{}`", key.name)), "{}", key.name);
        }
        for (name, _) in crate::user_commands::VARS {
            assert!(SKILL.contains(&format!("`{{{name}}}`")), "{name}");
        }
        for verb in [
            "state", "open", "select", "commands", "run", "jj", "toast", "reload",
        ] {
            assert!(SKILL.contains(&format!("`diffui ctl {verb}")), "{verb}");
        }
    }

    /// A request goes over the socket, through whatever answers it, and back.
    #[test]
    fn a_request_round_trips_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("diffui-ctl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let socket = dir.join("s");
        let (sender, mut receiver) = mpsc::unbounded::<Incoming>();
        serve(&socket, sender).expect("binds");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&socket)
                .expect("the socket")
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        let app = std::thread::spawn(move || {
            let (request, reply) =
                futures::executor::block_on(futures::StreamExt::next(&mut receiver))
                    .expect("a request");
            assert_eq!(
                request,
                Request::Toast {
                    text: "hi".to_owned()
                }
            );
            reply
                .send(json!({ "ok": true, "echo": "hi" }))
                .expect("sent");
        });
        let answer = call(
            &socket,
            &Request::Toast {
                text: "hi".to_owned(),
            },
        )
        .expect("answered");
        app.join().expect("the app side");
        assert_eq!(answer["echo"], "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
