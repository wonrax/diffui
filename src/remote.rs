//! Hosts: the machines repositories are opened on, and how each is reached.
//!
//! A host is a `[hosts.<name>]` table (config.toml's, or the one Settings
//! saves: see `crate::host_page`), a machine a transport plugin found on its
//! own (a WSL distribution, a `Host` of `~/.ssh/config`), or both: a table
//! named like a found host adds to it, which is how a distribution gets its
//! `server`.
//!
//! diffui never puts anything on a host. `server` is the command that is
//! `diffui-serve` over there, as words: a name on the PATH, a path, or
//! whatever fetches and runs it (`nix run …`). `{rev}` and `{version}` in it
//! are this build's, so a command that can fetch a version fetches the
//! matching one.
//!
//! The [`Directory`] is what the core's links ask through [`HostConnector`]:
//! it turns a host's name into the program whose stdio is the stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use diffui_core::remote::{Connector, Exit, HostId, Launch};
use diffui_plugins::protocol::{
    FoundHost, PluginCall, ServerCommand, TransportCall, TransportExit, TransportHost,
};
use diffui_plugins::{Manifest, Plugin, PluginError};
use futures::future::BoxFuture;

use crate::config::HostTables;

/// The app's hosts and its links to them.
pub(crate) struct Remote {
    /// One link per host, shared by its tabs and the open prompt.
    pub(crate) hosts: diffui_core::remote::Hosts,
    pub(crate) directory: Arc<Mutex<Directory>>,
    /// What was wrong with the `[hosts]` tables as last read.
    pub(crate) problems: Vec<String>,
    /// The host whose page is open in Settings.
    pub(crate) page: Option<crate::host_page::HostPage>,
    /// What is typed in Settings' field for a new host's name.
    pub(crate) new_host: String,
    /// The links the app holds, and what became of the ones that failed.
    pub(crate) connections: crate::connections::Connections,
}

impl Default for Remote {
    fn default() -> Self {
        let directory = Arc::<Mutex<Directory>>::default();
        Self {
            hosts: diffui_core::remote::Hosts::new(Arc::new(HostConnector(directory.clone()))),
            directory,
            problems: Vec::new(),
            page: None,
            new_host: String::new(),
            connections: Default::default(),
        }
    }
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").finish_non_exhaustive()
    }
}

/// A tab's connection to its repository's host.
#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteTab {
    pub(crate) host: HostId,
    /// The last thing the transport's program said while the link comes up.
    pub(crate) connecting: Option<String>,
    /// Why the repository can't be reached. Reconnecting clears it.
    pub(crate) lost: Option<diffui_core::remote::ConnectError>,
    /// No revset was saved for the tab: it starts on the host's default.
    pub(crate) wants_default_revset: bool,
    /// The repository's git remotes, as the server read them.
    pub(crate) remotes: Vec<(String, String)>,
}

/// The server when a host names none: `diffui-serve` on the PATH there.
pub(crate) const DEFAULT_SERVER: &str = "diffui-serve";

/// One `[hosts.<name>]` table.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct HostConfig {
    pub(crate) name: String,
    /// The transport plugin that reaches it, by id.
    pub(crate) transport: Option<String>,
    /// Instead of a transport: the command whose stdio is the stream, with
    /// the server appended (or put where a word says `{server}`).
    pub(crate) command: Option<Vec<String>>,
    /// The server over there. Empty: [`DEFAULT_SERVER`].
    pub(crate) server: Vec<String>,
    /// The server's environment.
    pub(crate) env: BTreeMap<String, String>,
    /// The transport's per-host settings, as written: every key of the
    /// table that isn't one of diffui's own.
    pub(crate) options: toml::Table,
}

/// Read the host tables: the hosts by name, and what was wrong
/// with them.
pub(crate) fn read_hosts(tables: &HostTables) -> (Vec<HostConfig>, Vec<String>) {
    let mut problems = Vec::new();
    let hosts = tables
        .iter()
        .map(|(name, table)| read_host(name, table.clone(), &mut problems))
        .collect();
    (hosts, problems)
}

fn read_host(name: &str, mut table: toml::Table, problems: &mut Vec<String>) -> HostConfig {
    let mut problem = |what: &str| problems.push(format!("[hosts.{name}] {what}"));
    let words = |value: toml::Value| -> Option<Vec<String>> {
        match value {
            toml::Value::String(word) => Some(vec![word]),
            toml::Value::Array(words) => words
                .into_iter()
                .map(|word| word.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    };
    let transport = match table.remove("transport") {
        Some(toml::Value::String(id)) => Some(id),
        Some(_) => {
            problem("transport must be a plugin's id, like \"ssh\"");
            None
        }
        None => None,
    };
    let command = table.remove("command").and_then(|value| {
        let command = words(value).filter(|command| !command.is_empty());
        if command.is_none() {
            problem(
                "command must be a list of words, like [\"docker\", \"exec\", \"-i\", \"dev\"]",
            );
        }
        command
    });
    if transport.is_some() && command.is_some() {
        problem("has both transport and command: command is used");
    }
    let server = table
        .remove("server")
        .map(|value| {
            words(value).unwrap_or_else(|| {
                problem("server must be a word or a list of words, like [\"nix\", \"run\", \"…\", \"--\"]");
                Vec::new()
            })
        })
        .unwrap_or_default();
    let env = match table.remove("env") {
        None => BTreeMap::new(),
        Some(toml::Value::Table(env)) => env
            .into_iter()
            .filter_map(|(key, value)| match value {
                toml::Value::String(value) => Some((key, value)),
                _ => {
                    problem(&format!("env.{key} must be a string"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            problem("env must be a table of strings");
            BTreeMap::new()
        }
    };
    HostConfig {
        name: name.to_owned(),
        transport,
        command,
        server,
        env,
        options: table,
    }
}

/// A transport plugin that is running or loading.
#[derive(Clone)]
pub(crate) struct Transport {
    /// The plugin's name, for a host's "over SSH".
    pub(crate) name: String,
    pub(crate) plugin: Arc<Plugin>,
    pub(crate) manifest: Arc<Manifest>,
    /// The plugin's settings as resolved: what a host's own setting falls
    /// back to.
    pub(crate) settings: serde_json::Map<String, serde_json::Value>,
}

/// One host, for the open prompt and Settings to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostRow {
    pub(crate) name: String,
    /// How it is reached: the transport's name, or "command".
    pub(crate) via: String,
    /// It has a `[hosts.<name>]` table.
    pub(crate) configured: bool,
    /// Why it can't be reached as it stands.
    pub(crate) problem: Option<String>,
}

/// Every host the app knows, and the transports that reach them. Shared
/// with the connector, which reads it from the core's threads.
#[derive(Default)]
pub(crate) struct Directory {
    pub(crate) hosts: Vec<HostConfig>,
    /// What each transport's `hosts()` answered, by plugin id.
    pub(crate) found: BTreeMap<String, Vec<FoundHost>>,
    pub(crate) transports: BTreeMap<String, Transport>,
}

/// A host with everything about it settled but the plugin's own answer.
struct Resolved {
    name: String,
    reach: Reach,
    server: Vec<String>,
    env: BTreeMap<String, String>,
}

enum Reach {
    Command(Vec<String>),
    Transport {
        transport: Transport,
        options: serde_json::Map<String, serde_json::Value>,
    },
}

impl Directory {
    fn config(&self, name: &str) -> Option<&HostConfig> {
        self.hosts.iter().find(|host| host.name == name)
    }

    /// The transport that found a host named `name`, and what it said.
    fn found(&self, name: &str) -> Option<(&str, &FoundHost)> {
        self.found.iter().find_map(|(transport, hosts)| {
            let host = hosts.iter().find(|host| host.name == name)?;
            Some((transport.as_str(), host))
        })
    }

    /// The id of the transport that found `name` on its own, and the
    /// settings it found it with.
    pub(crate) fn found_by(
        &self,
        name: &str,
    ) -> Option<(&str, &serde_json::Map<String, serde_json::Value>)> {
        self.found(name)
            .map(|(transport, host)| (transport, &host.options))
    }

    /// The id of the transport `name` is reached over: its table's, else
    /// the one that found it. `None` for a host reached by a command.
    pub(crate) fn transport_id(&self, name: &str) -> Option<String> {
        let config = self.config(name);
        if config.is_some_and(|host| host.command.is_some()) {
            return None;
        }
        config
            .and_then(|host| host.transport.clone())
            .or_else(|| self.found(name).map(|(transport, _)| transport.to_owned()))
    }

    /// Why `name` can't be reached as it stands.
    pub(crate) fn problem(&self, name: &str) -> Option<String> {
        self.resolve(name).err()
    }

    /// Whether `name` is a host: configured, or found by a transport.
    pub(crate) fn knows(&self, name: &str) -> bool {
        self.config(name).is_some() || self.found(name).is_some()
    }

    /// Remember a host a transport named outside its `hosts()` (the one a
    /// located path is on), so it can be reached by name.
    pub(crate) fn note_found(&mut self, transport: &str, host: FoundHost) {
        let found = self.found.entry(transport.to_owned()).or_default();
        if !found.iter().any(|known| known.name == host.name) {
            found.push(host);
        }
    }

    /// The hosts, configured ones first, each by name.
    pub(crate) fn rows(&self) -> Vec<HostRow> {
        let mut rows: Vec<HostRow> = self
            .hosts
            .iter()
            .map(|host| HostRow {
                name: host.name.clone(),
                via: self.via(&host.name),
                configured: true,
                problem: self.resolve(&host.name).err(),
            })
            .collect();
        let mut found: Vec<HostRow> = self
            .found
            .values()
            .flatten()
            .filter(|host| self.config(&host.name).is_none())
            .map(|host| HostRow {
                name: host.name.clone(),
                via: self.via(&host.name),
                configured: false,
                problem: None,
            })
            .collect();
        found.sort_by_key(|host| host.name.to_lowercase());
        found.dedup_by(|a, b| a.name == b.name);
        rows.extend(found);
        rows
    }

    /// How `name` is reached, in a word.
    pub(crate) fn via(&self, name: &str) -> String {
        let config = self.config(name);
        if config.is_some_and(|host| host.command.is_some()) {
            return "command".to_owned();
        }
        let id = config
            .and_then(|host| host.transport.as_deref())
            .or_else(|| self.found(name).map(|(transport, _)| transport));
        match id {
            Some(id) => self
                .transports
                .get(id)
                .map_or_else(|| id.to_owned(), |transport| transport.name.clone()),
            None => String::new(),
        }
    }

    fn resolve(&self, name: &str) -> Result<Resolved, String> {
        let config = self.config(name);
        let found = self.found(name);
        if config.is_none() && found.is_none() {
            return Err(format!(
                "No host named {name}. Add it in Settings, or as [hosts.{name}] in config.toml."
            ));
        }
        let server = config.map(|host| host.server.clone()).unwrap_or_default();
        let server = if server.is_empty() {
            vec![DEFAULT_SERVER.to_owned()]
        } else {
            server
        };
        let env = config.map(|host| host.env.clone()).unwrap_or_default();
        if let Some(command) = config.and_then(|host| host.command.clone()) {
            return Ok(Resolved {
                name: name.to_owned(),
                reach: Reach::Command(command),
                server,
                env,
            });
        }
        // A table without a transport of its own adds to the found host of
        // its name.
        let id = config
            .and_then(|host| host.transport.as_deref())
            .or(found.map(|(transport, _)| transport))
            .ok_or_else(|| format!("{name} needs a transport to reach it over, or a command"))?;
        let transport = self.transports.get(id).cloned().ok_or_else(|| {
            format!(
                "{name} is reached over {id}, which isn't running: turn it on in Settings → Plugins"
            )
        })?;
        let mut options = serde_json::Map::new();
        for field in transport
            .manifest
            .config
            .iter()
            .filter(|field| field.per_host)
        {
            if let Some(value) = transport.settings.get(&field.key) {
                options.insert(field.key.clone(), value.clone());
            }
        }
        if let Some((found_by, host)) = found
            && found_by == id
        {
            options.extend(host.options.clone());
        }
        for (key, value) in config.map(|host| &host.options).into_iter().flatten() {
            let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
            let field = transport
                .manifest
                .config
                .iter()
                .find(|field| field.per_host && &field.key == key);
            match field {
                Some(field) if field.admits(&value) => {
                    options.insert(key.clone(), value);
                }
                Some(field) => {
                    return Err(format!(
                        "[hosts.{name}] {key} isn't what {}'s \"{}\" takes",
                        transport.name,
                        field.title()
                    ));
                }
                None => {
                    return Err(format!(
                        "[hosts.{name}] {key} isn't a setting of {} hosts",
                        transport.name
                    ));
                }
            }
        }
        Ok(Resolved {
            name: name.to_owned(),
            reach: Reach::Transport { transport, options },
            server,
            env,
        })
    }
}

/// `server` with this build's `{rev}` and `{version}` filled in.
fn fill_server(host: &str, server: &[String]) -> Result<Vec<String>, String> {
    let version = env!("CARGO_PKG_VERSION");
    let rev = diffui_core::remote::build_rev();
    server
        .iter()
        .map(|word| {
            let word = word.replace("{version}", version);
            match (word.contains("{rev}"), rev) {
                (false, _) => Ok(word),
                (true, Some(rev)) => Ok(word.replace("{rev}", rev)),
                (true, None) => Err(format!(
                    "{host}'s server names {{rev}}, but this diffui wasn't built knowing its commit. \
                     Name a path or a version there instead."
                )),
            }
        })
        .collect()
}

/// One word for a POSIX shell. A leading `~/` stays the home folder.
fn shell_quote(word: &str) -> String {
    if let Some(rest) = word.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", shell_quote(rest));
    }
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_/.:=@%+,-".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// A `command` host's whole command line: the command, then `env K=V …` and
/// the server as further words. A command that goes through a shell says
/// where the server goes with `{server}`, and gets it as one quoted line.
fn command_argv(
    command: &[String],
    server: &[String],
    env: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    if !env.is_empty() {
        words.push("env".to_owned());
        words.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
    }
    words.extend(server.iter().cloned());
    if command.iter().any(|word| word.contains("{server}")) {
        let line = words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        command
            .iter()
            .map(|word| word.replace("{server}", &line))
            .collect()
    } else {
        command.iter().cloned().chain(words).collect()
    }
}

/// The reading of an exit every transport shares: the far side had no such
/// program.
fn missing_server(host: &str, server: &[String], exit: &Exit) -> Option<String> {
    let said = exit.stderr.to_lowercase();
    let missing = exit.code == Some(127)
        || said.contains("command not found")
        || said.contains(": not found")
        || said.contains("no such file or directory");
    missing.then(|| {
        format!(
            "{host} has no {} to run. Install diffui-serve there, or set this host's server to where it is.",
            server.first().map_or(DEFAULT_SERVER, String::as_str)
        )
    })
}

pub(crate) fn lock(directory: &Mutex<Directory>) -> MutexGuard<'_, Directory> {
    directory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The app's hosts, as the core's links ask about them.
pub(crate) struct HostConnector(pub(crate) Arc<Mutex<Directory>>);

/// Ask `plugin`'s transport and wait for the answer off its thread.
async fn ask<T: serde::de::DeserializeOwned + Send + 'static>(
    plugin: &Plugin,
    call: TransportCall,
) -> Result<T, PluginError> {
    let (answer, answered) = futures::channel::oneshot::channel();
    plugin.call::<T>(PluginCall::Transport { call }, move |result| {
        let _ = answer.send(result);
    });
    answered.await.unwrap_or(Err(PluginError::Gone))
}

fn transport_host(
    resolved: &Resolved,
    options: &serde_json::Map<String, serde_json::Value>,
    server: Vec<String>,
) -> TransportHost {
    TransportHost {
        name: resolved.name.clone(),
        options: options.clone(),
        server: ServerCommand {
            argv: server,
            env: resolved.env.clone(),
        },
    }
}

/// Resolve `name`, giving the transports a moment to say which hosts they
/// find: a tab restored at launch asks before a transport that is still
/// loading has listed its distributions.
async fn settled(directory: &Mutex<Directory>, name: &str) -> Result<Resolved, String> {
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        {
            let directory = lock(directory);
            let all_heard = directory
                .transports
                .keys()
                .all(|id| directory.found.contains_key(id));
            if directory.knows(name) || all_heard || tokio::time::Instant::now() >= deadline {
                return directory.resolve(name);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

impl Connector for HostConnector {
    fn launch(&self, host: &HostId) -> BoxFuture<'static, Result<Launch, String>> {
        let (directory, name) = (self.0.clone(), host.0.clone());
        Box::pin(async move {
            let resolved = settled(&directory, &name).await?;
            let server = fill_server(&resolved.name, &resolved.server)?;
            match &resolved.reach {
                #[cfg(test)]
                Reach::Command(command) if command == &[in_process::COMMAND] => {
                    Ok(in_process::launch())
                }
                Reach::Command(command) => Ok(Launch::Program {
                    argv: command_argv(command, &server, &resolved.env),
                    env: Vec::new(),
                }),
                Reach::Transport { transport, options } => {
                    let host = transport_host(&resolved, options, server);
                    let launch: diffui_plugins::protocol::Launch =
                        ask(&transport.plugin, TransportCall::Connect { host })
                            .await
                            .map_err(|error| match error {
                                // What the script threw is for the user.
                                PluginError::Threw(thrown) => thrown.message,
                                other => format!("{}: {other}", transport.name),
                            })?;
                    Ok(Launch::Program {
                        argv: launch.argv,
                        env: launch.env.into_iter().collect(),
                    })
                }
            }
        })
    }

    fn explain(&self, host: &HostId, exit: &Exit) -> BoxFuture<'static, Option<String>> {
        let resolved = lock(&self.0).resolve(&host.0);
        let exit = exit.clone();
        Box::pin(async move {
            let resolved = resolved.ok()?;
            let server = fill_server(&resolved.name, &resolved.server).ok()?;
            if let Reach::Transport { transport, options } = &resolved.reach {
                let call = TransportCall::Explain {
                    host: transport_host(&resolved, options, server.clone()),
                    exit: TransportExit {
                        code: exit.code,
                        stderr: exit.stderr.clone(),
                    },
                };
                // A transport without `explain`, or one that failed to, leaves
                // it to the shared reading below.
                if let Ok(Some(explained)) = ask::<Option<String>>(&transport.plugin, call).await {
                    return Some(explained);
                }
            }
            missing_server(&resolved.name, &server, &exit)
        })
    }
}

impl crate::Diffui {
    /// The hosts the open prompt offers, by name.
    pub(crate) fn open_repo_host_rows(&self) -> Vec<HostRow> {
        lock(&self.remote.directory).rows()
    }

    /// Point the open prompt at `host` (`None`: this machine) and start
    /// reaching it. The path starts over: one machine's says nothing about
    /// another's.
    pub(crate) fn set_open_repo_host(&mut self, host: Option<String>, cx: &mut crate::Cx) {
        let host = host.map(HostId);
        let Some(dialog) = self.open_repo_dialog() else {
            return;
        };
        let changed = dialog.host != host;
        // Picking the host a failed link was to is how it is tried again.
        let down = dialog.link.as_ref().is_some_and(|link| {
            matches!(
                &*link.state().borrow(),
                diffui_core::remote::LinkState::Down(_)
            )
        });
        let link = match &host {
            Some(host) if changed || down => Some(self.host_link(host, cx)),
            _ => None,
        };
        let Some(dialog) = self.open_repo_dialog_mut() else {
            return;
        };
        dialog.picking_host = false;
        dialog.selected = None;
        dialog.error = None;
        if changed || down {
            dialog.host = host;
            dialog.link = link.clone();
            dialog.listing = None;
            dialog.listed_query = None;
            dialog.listing_pending = None;
            dialog.probe = None;
            let path = match (changed, dialog.host.is_some()) {
                (false, _) => dialog.path.clone(),
                (true, true) => "~/".to_owned(),
                (true, false) => String::new(),
            };
            crate::input::set_caret(crate::tab_bar::OPEN_REPO_INPUT_ID, path.len(), cx);
            self.set_open_repo_path(path, cx);
        }
        crate::input::focus(crate::tab_bar::OPEN_REPO_INPUT_ID, cx);
        if let Some(link) = link {
            self.watch_open_repo_link(link, cx);
        }
    }

    /// Redraw the prompt as its host's link comes up: each thing its program
    /// says, then the answer.
    fn watch_open_repo_link(&mut self, link: diffui_core::remote::HostLink, cx: &mut crate::Cx) {
        let mut state = link.state();
        Self::run_async(
            async move {
                let connecting = matches!(
                    &*state.borrow_and_update(),
                    diffui_core::remote::LinkState::Connecting { .. }
                );
                if connecting {
                    let _ = state.changed().await;
                }
                link
            },
            cx,
            |this, link, cx| {
                let current = this
                    .open_repo_dialog()
                    .and_then(|dialog| dialog.link.as_ref())
                    .is_some_and(|held| held.host() == link.host());
                if !current {
                    return;
                }
                let connecting = matches!(
                    &*link.state().borrow(),
                    diffui_core::remote::LinkState::Connecting { .. }
                );
                if connecting {
                    this.watch_open_repo_link(link, cx);
                }
            },
        );
    }

    /// Open what the recent list calls `key`: a root here, or `host:root`.
    /// Whatever machine the prompt is pointed at doesn't matter to it.
    pub(crate) fn open_recent(&mut self, key: &str, cx: &mut crate::Cx) {
        match diffui_core::remote::RemoteTarget::parse(key)
            .filter(|_| !std::path::Path::new(key).exists())
        {
            Some(target) => self.open_remote(target, cx),
            None => self.open_local_repository(key, cx),
        }
    }

    /// Open the repository at or above `target` as a tab, or focus the tab
    /// that has it. The host is asked first where the repository's root is:
    /// a tab is known by it, as a local one is.
    pub(crate) fn open_remote(
        &mut self,
        target: diffui_core::remote::RemoteTarget,
        cx: &mut crate::Cx,
    ) {
        if let Some(open) = self
            .tabs
            .iter()
            .find(|tab| tab.remote_target().as_ref() == Some(&target))
        {
            let id = open.id;
            self.pop_mode(crate::ModeKind::OpenRepo);
            return self.activate_tab(id, cx);
        }
        let link = self.host_link(&target.host, cx);
        Self::run_async(
            async move {
                let probed = match link.ready().await {
                    Ok(_) => link.probe(&target.path).await,
                    Err(error) => Err(error.to_string()),
                };
                (target, probed)
            },
            cx,
            |this, (target, probed), cx| this.on_remote_probed(target, probed, cx),
        );
    }

    fn on_remote_probed(
        &mut self,
        target: diffui_core::remote::RemoteTarget,
        probed: Result<diffui_core::remote::Probed, String>,
        cx: &mut crate::Cx,
    ) {
        let found = match probed.map(|probed| probed.repository) {
            Ok(Some(found)) => found,
            Ok(None) => {
                return self.remote_open_failed(
                    &target,
                    format!(
                        "No jj or git repository at {} on {}.",
                        target.path, target.host
                    ),
                );
            }
            Err(error) => return self.remote_open_failed(&target, error),
        };
        self.pop_mode(crate::ModeKind::OpenRepo);
        self.mark_geometry_dirty();
        let rooted = diffui_core::remote::RemoteTarget {
            host: target.host.clone(),
            path: found.root.clone(),
        };
        self.push_recent_key(rooted.to_string());
        if let Some(open) = self
            .tabs
            .iter()
            .find(|tab| tab.remote_target().as_ref() == Some(&rooted))
        {
            let id = open.id;
            return self.activate_tab(id, cx);
        }
        let repository = crate::Repository {
            root: std::path::PathBuf::from(&found.root),
            vcs: found.vcs,
            scope: std::path::PathBuf::from(&found.scope),
        };
        let (_, name) = crate::repo_label(&repository.root);
        let source = crate::TabSource::Repo {
            vcs: repository.vcs,
            root: repository.root.clone(),
            host: Some(target.host.clone()),
        };
        let state = self.repo_layouts.remote_tab_state(
            &rooted.to_string(),
            repository,
            target.host.clone(),
        );
        self.push_tab(target.host.0, name, source, state, cx);
    }

    /// Say why `target` didn't open: in the prompt when it is up, which
    /// stays open on it, else as a toast (a path named on the command line).
    fn remote_open_failed(&mut self, target: &diffui_core::remote::RemoteTarget, error: String) {
        match self.open_repo_dialog_mut() {
            Some(dialog) => dialog.error = Some(error),
            None => self.push_error_toast(format!("Couldn't open {target}"), &error),
        }
    }

    /// Fold a link's own events into the tabs on its repository: what its
    /// program says while it comes up, and that it is gone. `false` for any
    /// other event.
    pub(crate) fn on_link_event(&mut self, event: &diffui_core::Event) -> bool {
        use diffui_core::Payload;
        let (connecting, lost) = match &event.payload {
            Payload::Connecting { detail } => (detail.clone(), None),
            Payload::Disconnected { error } => (None, Some(error.clone())),
            _ => return false,
        };
        for tab in &mut self.tabs {
            if tab.repo_id().as_ref() != Some(&event.repo) {
                continue;
            }
            let state = &mut tab.state;
            let Some(remote) = state.remote.as_mut() else {
                continue;
            };
            remote.connecting = connecting.clone();
            let Some(error) = &lost else { continue };
            remote.lost = Some(error.clone());
            // Nothing was ever shown: say so where the graph would be. The
            // reason, and the way back, are the strip's. With a graph on
            // screen it stays, under the strip.
            if !matches!(state.session.status, diffui_core::LoadStatus::Loaded) {
                state.session.status =
                    diffui_core::LoadStatus::Failed(format!("Not connected to {}", remote.host));
                state.session.loading_since = None;
            }
        }
        true
    }

    /// Reach the active tab's host again: every tab the lost link took down
    /// opens its repository afresh, on one new link.
    pub(crate) fn reconnect_host(&mut self) {
        let Some(host) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.state.remote.as_ref())
            .map(|remote| remote.host.clone())
        else {
            return;
        };
        let mut specs = Vec::new();
        for tab in &mut self.tabs {
            let spec = tab.open_spec();
            let state = &mut tab.state;
            let Some(remote) = state.remote.as_mut().filter(|remote| remote.host == host) else {
                continue;
            };
            if remote.lost.take().is_none() {
                continue;
            }
            // Its actor answers nothing now; the new one's `Ready` brings a
            // handle that does.
            state.handle = None;
            if !matches!(state.session.status, diffui_core::LoadStatus::Loaded) {
                state.session.status = diffui_core::LoadStatus::Loading;
            }
            specs.push(spec);
        }
        self.background.restart(&specs);
    }
}

/// The server answered for `tab`'s repository: take what only it knows.
pub(crate) fn answered(tab: &mut crate::Tab, opened: &diffui_core::remote::Opened) {
    let state = &mut tab.state;
    let Some(remote) = state.remote.as_mut() else {
        return;
    };
    remote.connecting = None;
    remote.lost = None;
    remote.remotes = opened.remotes.clone();
    let default = opened.default_revset.clone().unwrap_or_default();
    if std::mem::take(&mut remote.wants_default_revset) {
        state.session.revset = default.clone();
    }
    state.default_revset = Some(default);
    // A tab restored from the session guessed the kind.
    if let Some(repository) = state.repository.as_mut() {
        repository.vcs = opened.vcs;
    }
    if let crate::TabSource::Repo { vcs, .. } = &mut tab.source {
        *vcs = opened.vcs;
    }
}

/// The strip atop a remote tab's diff while its host is being reached or
/// can't be: what is wrong, and the way back.
pub(crate) fn banners(
    ui: &crate::Diffui,
    look: crate::ui::Look,
    cx: &mut crate::Cx,
) -> Option<gpui_kit::AnyElement> {
    use gpui_kit::div;
    use gpui_kit::prelude::*;

    use crate::{Action, icons, ui as kit};
    let theme = look.theme;
    let remote = ui.active().remote.as_ref()?;
    let host = remote.host.0.clone();
    let Some(error) = &remote.lost else {
        // Up, or coming up with nothing to say yet.
        let detail = remote.connecting.as_ref()?;
        let words = crate::prose::Prose::from("Connecting to ")
            .code(host)
            .text(format!(": {detail}"));
        return Some(
            kit::Banner::new(kit::Tone::Muted)
                .shape(kit::BannerShape::Strip)
                .icon(icons::REFRESH)
                .child(words.view(look, theme.muted_text))
                .build(look)
                .into_any_element(),
        );
    };
    let (tone, button) = if error.is_transient() {
        (kit::Tone::Warn, "Reconnect")
    } else {
        (kit::Tone::Danger, "Try again")
    };
    Some(
        kit::Banner::new(tone)
            .shape(kit::BannerShape::Strip)
            .icon(icons::ALERT_TRIANGLE)
            .child(div().text_color(theme.text).child(error.to_string()))
            .end(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(gpui_kit::px(crate::theme::space::SM))
                    .child(
                        look.button("remote-details", "Details")
                            .ghost()
                            .compact()
                            .on_click(crate::Diffui::on_action(
                                Action::Host(crate::host_page::HostAction::Open(host.clone())),
                                cx,
                            ))
                            .tip("What was run to reach it, and what that wrote"),
                    )
                    .child(
                        look.button("remote-reconnect", button)
                            .ghost()
                            .compact()
                            .on_click(crate::Diffui::on_action(Action::ReconnectHost, cx))
                            .tip(format!("Start the connection to {host} again")),
                    ),
            )
            .build(look)
            .into_any_element(),
    )
}

/// A host for the app's own tests: its server runs in this process, on the
/// two ends of an in-memory pipe, so a remote tab is driven end to end
/// without an ssh or a second binary.
#[cfg(test)]
pub(crate) mod in_process {
    use std::sync::Mutex;

    use diffui_core::remote::Launch;

    /// The `command` of a `[hosts.<name>]` table that is served here.
    pub(crate) const COMMAND: &str = "<in this process>";

    static SERVERS: Mutex<Vec<tokio::task::JoinHandle<std::io::Result<()>>>> =
        Mutex::new(Vec::new());

    pub(super) fn launch() -> Launch {
        let (here, there) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(here);
        let (input, output) = tokio::io::split(there);
        let server = tokio::spawn(diffui_core::remote::serve::serve(
            input,
            output,
            std::path::PathBuf::from("jj"),
        ));
        SERVERS.lock().unwrap().push(server);
        Launch::Stream {
            reader: Box::new(reader),
            writer: Box::new(writer),
            name: "a server in this process".to_owned(),
        }
    }

    /// End every server started so far, as a dropped connection would.
    pub(crate) fn stop() {
        for server in SERVERS.lock().unwrap().drain(..) {
            server.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(text: &str) -> HostTables {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            hosts: HostTables,
        }
        toml::from_str::<File>(text).unwrap().hosts
    }

    #[test]
    fn a_host_table_reads_its_own_keys_and_keeps_the_rest_for_the_transport() {
        let config = tables(
            r#"
            [hosts.work]
            transport = "wsl"
            distribution = "Ubuntu"
            server = ["nix", "run", "github:wonrax/diffui/{rev}#diffui-serve", "--"]
            env = { JJ_CONFIG = "/home/me/work.toml" }

            [hosts.dev]
            command = ["docker", "exec", "-i", "dev"]
            server = "/opt/diffui-serve"
            "#,
        );
        let (hosts, problems) = read_hosts(&config);
        assert_eq!(problems, Vec::<String>::new());
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["dev", "work"]);

        let work = &hosts[1];
        assert_eq!(work.transport.as_deref(), Some("wsl"));
        assert_eq!(work.server.len(), 4);
        assert_eq!(work.env["JJ_CONFIG"], "/home/me/work.toml");
        // What isn't diffui's own is the transport's.
        assert_eq!(work.options.len(), 1);
        assert_eq!(work.options["distribution"].as_str(), Some("Ubuntu"));

        let dev = &hosts[0];
        assert_eq!(dev.command.as_deref().unwrap().len(), 4);
        assert_eq!(dev.server, ["/opt/diffui-serve"]);
    }

    #[test]
    fn what_is_wrong_with_a_host_table_is_said_by_name() {
        let config = tables(
            "[hosts.a]\ntransport = 3\nserver = { x = 1 }\nenv = { K = 2 }\n[hosts.b]\ntransport = \"ssh\"\ncommand = [\"x\"]\n",
        );
        let (_, problems) = read_hosts(&config);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(
            problems[0].starts_with("[hosts.a] transport must be"),
            "{problems:?}"
        );
        assert!(
            problems[3].starts_with("[hosts.b] has both"),
            "{problems:?}"
        );
    }

    #[test]
    fn a_command_host_gets_the_server_as_words_or_as_one_quoted_line() {
        let env = BTreeMap::from([("K".to_owned(), "a b".to_owned())]);
        let server = ["~/bin/diffui-serve".to_owned(), "it's".to_owned()];
        let exec = ["docker", "exec", "-i", "dev"].map(str::to_owned);
        assert_eq!(
            command_argv(&exec, &server, &BTreeMap::new()),
            ["docker", "exec", "-i", "dev", "~/bin/diffui-serve", "it's"]
        );
        assert_eq!(
            command_argv(&exec, &server, &env)[4..6],
            ["env".to_owned(), "K=a b".to_owned()]
        );
        let shell = ["ssh", "box", "exec {server}"].map(str::to_owned);
        assert_eq!(
            command_argv(&shell, &server, &env),
            [
                "ssh",
                "box",
                "exec env 'K=a b' \"$HOME\"/bin/diffui-serve 'it'\\''s'"
            ]
        );
    }

    #[test]
    fn the_server_names_this_build() {
        let server = [
            "nix".to_owned(),
            "run".to_owned(),
            "x/v{version}#serve".to_owned(),
        ];
        let filled = fill_server("box", &server).unwrap();
        assert_eq!(filled[2], format!("x/v{}#serve", env!("CARGO_PKG_VERSION")));
        let pinned = fill_server("box", &["x/{rev}".to_owned()]);
        match diffui_core::remote::build_rev() {
            Some(rev) => assert_eq!(pinned.unwrap(), [format!("x/{rev}")]),
            None => assert!(
                pinned
                    .unwrap_err()
                    .contains("wasn't built knowing its commit")
            ),
        }
    }

    #[test]
    fn a_host_nobody_knows_and_a_table_without_a_way_say_so() {
        let mut directory = Directory::default();
        assert!(
            directory
                .resolve("box")
                .err()
                .unwrap()
                .starts_with("No host named box.")
        );
        directory.hosts = read_hosts(&tables("[hosts.box]\nserver = \"x\"\n")).0;
        assert!(
            directory
                .resolve("box")
                .err()
                .unwrap()
                .contains("needs a transport")
        );
        directory.hosts = read_hosts(&tables("[hosts.box]\ntransport = \"ssh\"\n")).0;
        assert!(
            directory
                .resolve("box")
                .err()
                .unwrap()
                .contains("isn't running")
        );
        assert!(directory.knows("box") && !directory.knows("other"));
        assert_eq!(directory.rows()[0].via, "ssh");
    }

    #[test]
    fn an_exit_that_means_no_server_reads_the_same_for_every_transport() {
        let exit = |code, stderr: &str| Exit {
            code,
            stderr: stderr.to_owned(),
        };
        let server = ["/opt/serve".to_owned()];
        assert!(
            missing_server("box", &server, &exit(Some(127), ""))
                .unwrap()
                .starts_with("box has no /opt/serve to run.")
        );
        assert!(
            missing_server(
                "box",
                &server,
                &exit(Some(1), "sh: 1: /opt/serve: not found")
            )
            .is_some()
        );
        assert_eq!(missing_server("box", &server, &exit(Some(1), "boom")), None);
    }
}
