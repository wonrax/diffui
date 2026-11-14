//! Plugins in the app: the directories under `~/.config/diffui/plugins`, each
//! a `plugin.toml` and a script the [`diffui_plugins`] crate runs on a thread
//! of its own.
//!
//! This module is the app's half of that crate's [`Host`] trait and
//! everything around it:
//!
//! - finding plugins, asking the user before one runs ([`trust`]), starting
//!   and reloading them ([`watch`]);
//! - their `[[commands]]` as registry rows ([`PluginCommand`]);
//! - answering what a script asks for: repository reads, jj commands and the
//!   UI slots ([`api`], [`query`]);
//! - a plugin's forge as a [`diffui_core::forge::ForgeProvider`] ([`forge`]);
//! - telling plugins what happened ([`events`]);
//! - the permission dialog and the Settings group ([`view`]), and
//!   `diffui plugin …` ([`cli`]).
//!
//! A plugin's thread never touches app state. What it asks for arrives here
//! as a [`FromPlugin`] on one channel, which [`start`] folds into the app the
//! way the repository loop folds an actor's events. Every repository call
//! names the tab it is about with a handle the app minted ([`RepoHandle`]),
//! and a handle that no longer matches what is on screen is refused.

mod api;
mod cli;
mod events;
mod forge;
mod query;
#[cfg(test)]
mod scenarios;
mod trust;
mod view;
mod watch;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diffui_core::RepoId;
use diffui_plugins::protocol::{CommandContext, HostCall, PluginCall, RepoHandle, RepoRef};
use diffui_plugins::{
    Host, Limits, LogLine, MANIFEST_FILE, Manifest, MenuPlace, Options, Plugin, PluginError,
    PluginEvent, Registration, Reply,
};
use futures::channel::mpsc;

pub(crate) use cli::client_main;
pub(crate) use events::TabPlugins;
#[cfg(test)]
pub(crate) use query::READ_ONLY;
#[cfg(test)]
pub(crate) use scenarios::PluginDir;
pub(crate) use trust::Record;
pub(crate) use view::{PermissionDialog, build_permission_dialog, settings_group, status_items};

use crate::app_files::{AppDir, app_file};
use crate::commands::{CommandArg, Disabled, reason};
use crate::config::display_path;
use crate::keymap::Chord;
use crate::modes::{Mode, ModeKind};
use crate::user_commands::{Places, Var};
use crate::when::{AppKeys, When};
use crate::{Cx, Diffui, TabId};

/// Where plugins live: `~/.config/diffui/plugins`.
pub(crate) fn plugins_dir() -> Option<PathBuf> {
    app_file(AppDir::Config, "plugins")
}

/// Where a plugin's `kv` store is kept. Outside its own directory, so a
/// write there isn't an edit to reload for.
fn data_dir() -> PathBuf {
    app_file(AppDir::Data, "plugins").unwrap_or_else(|| std::env::temp_dir().join("diffui-plugins"))
}

/// What reaches the app from a plugin's thread, or from the folder watcher.
pub(crate) enum FromPlugin {
    /// The script asked for something only the app can do.
    Call {
        plugin: String,
        instance: u64,
        call: HostCall,
        reply: Reply,
    },
    /// The plugin loaded, or stopped.
    Event {
        plugin: String,
        instance: u64,
        event: PluginEvent,
    },
    /// A command's handler settled.
    Ran {
        label: String,
        result: Result<(), PluginError>,
    },
    /// Files changed under these directories of the plugins folder.
    Changed(Vec<String>),
}

/// One running script's line to the app. Each start of a plugin gets its
/// own, so what a replaced script still says can be told from its successor.
struct Bridge {
    plugin: String,
    instance: u64,
    to: mpsc::UnboundedSender<FromPlugin>,
}

impl Host for Bridge {
    fn call(&self, _plugin: &str, call: HostCall, reply: Reply) {
        // An app that is gone drops the reply, which rejects the promise.
        let _ = self.to.unbounded_send(FromPlugin::Call {
            plugin: self.plugin.clone(),
            instance: self.instance,
            call,
            reply,
        });
    }

    fn event(&self, _plugin: &str, event: PluginEvent) {
        let _ = self.to.unbounded_send(FromPlugin::Event {
            plugin: self.plugin.clone(),
            instance: self.instance,
            event,
        });
    }
}

/// Where a plugin is in its life, as Settings and its commands show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// Turned off in Settings.
    Disabled,
    /// The user hasn't allowed its permissions.
    NotAllowed,
    Loading,
    Running(Registration),
    /// Its manifest didn't read, its script threw while loading, or it ran
    /// past a limit.
    Failed(String),
}

/// One directory of the plugins folder.
pub(crate) struct Found {
    /// The manifest's id, or the directory's name when it didn't read.
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) dir: PathBuf,
    /// `None`: `plugin.toml` has problems (the state says which).
    pub(crate) manifest: Option<Arc<Manifest>>,
    pub(crate) state: State,
    /// The manifest's commands as registry rows.
    rows: Vec<Arc<PluginCommand>>,
    /// What was wrong with its rows, each `path:line:col: …`.
    problems: Vec<String>,
    running: Option<Running>,
    /// Say so when it next loads: the user (or an edit) asked for a reload.
    announce: bool,
}

struct Running {
    plugin: Arc<Plugin>,
    instance: u64,
}

impl Found {
    /// What the plugin logged, for "Show log".
    pub(crate) fn log(&self) -> Vec<LogLine> {
        self.running
            .as_ref()
            .map(|running| running.plugin.log())
            .unwrap_or_default()
    }

    fn read(dir: &Path, records: &crate::config::PluginRecords) -> Self {
        let folder = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match Manifest::load(dir) {
            Ok(manifest) => {
                let (rows, problems) = commands_of(&manifest);
                let off = records.get(&manifest.id).is_some_and(|r| !r.enabled);
                Self {
                    id: manifest.id.clone(),
                    name: manifest.name.clone(),
                    version: manifest.version.clone(),
                    dir: dir.to_path_buf(),
                    state: if off {
                        State::Disabled
                    } else {
                        State::NotAllowed
                    },
                    manifest: Some(Arc::new(manifest)),
                    rows,
                    problems,
                    running: None,
                    announce: false,
                }
            }
            Err(problems) => {
                let problems: Vec<String> = problems.iter().map(placed).collect();
                Self {
                    id: folder.clone(),
                    name: folder,
                    version: String::new(),
                    dir: dir.to_path_buf(),
                    manifest: None,
                    state: State::Failed(
                        problems
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("{MANIFEST_FILE} didn't read")),
                    ),
                    rows: Vec::new(),
                    problems,
                    running: None,
                    announce: false,
                }
            }
        }
    }
}

/// The plugin directories under `dir`, by name: each one that holds a
/// `plugin.toml`.
fn plugin_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(MANIFEST_FILE).is_file())
        .collect();
    dirs.sort();
    dirs
}

/// The app's plugins.
pub(crate) struct Plugins {
    pub(crate) dir: Option<PathBuf>,
    /// Every plugin directory found, by directory name.
    pub(crate) found: Vec<Found>,
    /// Plugins the permission dialog has yet to ask about, next first.
    reviews: VecDeque<String>,
    to_app: mpsc::UnboundedSender<FromPlugin>,
    /// Taken by [`start`].
    incoming: Option<mpsc::UnboundedReceiver<FromPlugin>>,
    instances: u64,
    /// The repositories handles were minted for. A handle's id is its place
    /// here.
    minted: Vec<(TabId, RepoId)>,
    /// What scripts were handed for forge calls: see [`forge::Handles`].
    forge_handles: forge::Handles,
    /// The activity rows scripts started and haven't finished, by the number
    /// each was given: whose it is, and where it shows.
    activities: HashMap<u64, (String, TabId, crate::activity::ActivityId)>,
    next_activity: u64,
    /// Each plugin's status-bar item, by id.
    status: BTreeMap<String, String>,
    /// The prompt a plugin has in front of the user.
    asking: Option<api::Asking>,
    selection_changed: crate::spawn::Debounce,
    watcher: Option<watch::Watcher>,
}

impl Default for Plugins {
    fn default() -> Self {
        let (to_app, incoming) = mpsc::unbounded();
        Self {
            dir: None,
            found: Vec::new(),
            reviews: VecDeque::new(),
            to_app,
            incoming: Some(incoming),
            instances: 0,
            minted: Vec::new(),
            forge_handles: Default::default(),
            activities: HashMap::new(),
            next_activity: 0,
            status: BTreeMap::new(),
            asking: None,
            selection_changed: Default::default(),
            watcher: None,
        }
    }
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.found.iter().map(|found| (&found.id, &found.state)))
            .finish()
    }
}

impl Plugins {
    /// The plugins folder as it is on disk, nothing started.
    pub(crate) fn scan(records: &crate::config::PluginRecords) -> Self {
        let mut plugins = Self {
            dir: plugins_dir(),
            ..Self::default()
        };
        plugins.rescan(records);
        plugins
    }

    fn rescan(&mut self, records: &crate::config::PluginRecords) {
        self.found.clear();
        self.reviews.clear();
        self.status.clear();
        let dirs = self.dir.as_deref().map(plugin_dirs).unwrap_or_default();
        for dir in dirs {
            let mut found = Found::read(&dir, records);
            if let Some(first) = self.found.iter().find(|seen| seen.id == found.id) {
                found.state = State::Failed(format!(
                    "{} already uses the id {:?}",
                    display_path(&first.dir),
                    found.id
                ));
                found.manifest = None;
                found.rows.clear();
            }
            self.found.push(found);
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Found> {
        self.found.iter().find(|found| found.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Found> {
        self.found.iter_mut().find(|found| found.id == id)
    }

    /// The running script of `id`, if it is the one `instance` names.
    fn is_current(&self, id: &str, instance: u64) -> bool {
        self.get(id)
            .and_then(|found| found.running.as_ref())
            .is_some_and(|running| running.instance == instance)
    }

    fn plugin(&self, id: &str) -> Option<Arc<Plugin>> {
        self.get(id)?
            .running
            .as_ref()
            .map(|running| running.plugin.clone())
    }

    /// The commands of every plugin that isn't turned off, for the registry.
    pub(crate) fn rows(&self) -> Vec<Arc<PluginCommand>> {
        self.found
            .iter()
            .filter(|found| found.state != State::Disabled)
            .flat_map(|found| found.rows.iter().cloned())
            .collect()
    }

    /// What was wrong with the plugins' manifests and rows.
    pub(crate) fn problems(&self) -> Vec<String> {
        self.found
            .iter()
            .flat_map(|found| found.problems.iter().cloned())
            .collect()
    }

    /// The handle id for `tab` on `repo`.
    fn mint(&mut self, tab: TabId, repo: RepoId) -> String {
        let at = self
            .minted
            .iter()
            .position(|minted| *minted == (tab, repo.clone()))
            .unwrap_or_else(|| {
                self.minted.push((tab, repo));
                self.minted.len() - 1
            });
        at.to_string()
    }
}

/// One command a plugin's manifest declares, as the registry lists it.
#[derive(Debug)]
pub(crate) struct PluginCommand {
    /// The plugin's id.
    pub(crate) plugin: String,
    /// The plugin's name, for messages.
    plugin_name: String,
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) hint: String,
    /// The palette heading it lists under: the one it named, else the
    /// plugin's name.
    pub(crate) group: String,
    /// The clause as written, and parsed.
    pub(crate) when: Option<(String, When)>,
    pub(crate) places: Places,
    /// Chords, bound in the base context.
    pub(crate) keys: Vec<String>,
    /// Where the manifest declares it, `path:line:col`.
    pub(crate) source: String,
}

/// A manifest problem as the app and `diffui plugin` print it: its place
/// first, the home directory folded.
fn placed(problem: &diffui_plugins::Diagnostic) -> String {
    format!(
        "{}:{}:{}: {}",
        display_path(&problem.path),
        problem.line,
        problem.col,
        problem.message
    )
}

/// `manifest`'s `[[commands]]` as registry rows, and what was wrong with
/// them. A row whose when-clause doesn't parse is left out; a key that isn't
/// a chord is skipped.
fn commands_of(manifest: &Manifest) -> (Vec<Arc<PluginCommand>>, Vec<String>) {
    let file = display_path(&manifest.dir.join(MANIFEST_FILE));
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for decl in &manifest.commands {
        let source = format!("{file}:{}:{}", decl.line, decl.col);
        let mut say = |message: String| {
            problems.push(format!("{source}: [[commands]] {:?}: {message}", decl.id))
        };
        let when = match decl.when.as_deref().map(|text| (text, When::parse(text))) {
            None => None,
            Some((text, Ok(when))) => {
                for key in when.unknown_keys() {
                    say(format!("no key `{key}`; it reads as false"));
                }
                Some((text.to_owned(), when))
            }
            Some((_, Err(error))) => {
                say(format!("when: {error}"));
                continue;
            }
        };
        let mut keys = Vec::new();
        for key in &decl.keys {
            if Chord::parse(key).is_some() {
                keys.push(key.clone());
            } else {
                say(format!("{key:?} is not a chord"));
            }
        }
        let group = match &decl.group {
            None => manifest.name.clone(),
            Some(group) => match crate::commands::GROUPS
                .iter()
                .find(|known| known.eq_ignore_ascii_case(group))
            {
                Some(known) => (*known).to_owned(),
                None => {
                    say(format!(
                        "no group {group:?}; it lists under {}",
                        manifest.name
                    ));
                    manifest.name.clone()
                }
            },
        };
        let mut places = Places::default();
        for place in &decl.menu {
            match place {
                MenuPlace::Palette => places.palette = true,
                MenuPlace::Revision => places.revision = true,
                MenuPlace::Bookmark => places.bookmark = true,
                MenuPlace::File => places.file = true,
            }
        }
        if decl.menu.is_empty() {
            places.palette = true;
        }
        rows.push(Arc::new(PluginCommand {
            plugin: manifest.id.clone(),
            plugin_name: manifest.name.clone(),
            id: decl.id.clone(),
            label: decl.label.clone(),
            hint: decl.hint.clone().unwrap_or_default(),
            group,
            when,
            places,
            keys,
            source,
        }));
    }
    (rows, problems)
}

impl PluginCommand {
    /// `Err` with why it can't run on `arg` now: its plugin isn't running,
    /// there is no repository to hand it, or its when-clause doesn't hold.
    pub(crate) fn availability(&self, ui: &Diffui, arg: &CommandArg) -> Result<(), Disabled> {
        match ui.plugins.get(&self.plugin).map(|found| &found.state) {
            Some(State::Running(registration)) => crate::commands::require(
                registration.commands.contains(&self.id),
                "The plugin registered no handler",
            )?,
            Some(State::Loading) => return Err(Cow::Borrowed("The plugin is loading")),
            Some(State::NotAllowed) => return Err(Cow::Borrowed("Not allowed")),
            Some(State::Failed(_)) => {
                return Err(Cow::Owned(format!("{} failed", self.plugin_name)));
            }
            Some(State::Disabled) | None => return Err(Cow::Borrowed("The plugin is off")),
        }
        crate::commands::require(ui.active().repository.is_some(), reason::NO_REPOSITORY)?;
        if let Some((text, when)) = &self.when
            && !when.eval(&AppKeys { ui, arg })
        {
            return Err(Cow::Owned(format!("Only when {text}")));
        }
        Ok(())
    }
}

/// Fold what plugins say into the app, for as long as it runs.
pub(crate) fn start(app: &mut Diffui, cx: &mut Cx) {
    use futures::{FutureExt as _, StreamExt as _};
    let Some(mut incoming) = app.plugins.incoming.take() else {
        return;
    };
    cx.spawn(async move |this, cx| {
        while let Some(first) = incoming.next().await {
            let mut batch = vec![first];
            while let Some(Some(next)) = incoming.next().now_or_never() {
                batch.push(next);
            }
            let updated = this.update(cx, |this, cx| {
                this.act(cx, |this, cx| {
                    for message in batch {
                        this.on_plugin_message(message, cx);
                    }
                });
            });
            if updated.is_err() {
                return;
            }
        }
    })
    .detach();
}

impl Diffui {
    /// Start the plugins the user allowed and queue the permission dialog for
    /// the ones nobody answered for. The folder was read by [`Plugins::scan`].
    pub(crate) fn start_plugins(&mut self) {
        let ids: Vec<String> = self
            .plugins
            .found
            .iter()
            .map(|found| found.id.clone())
            .collect();
        for id in ids {
            self.settle_plugin(&id);
        }
        self.watch_plugins();
    }

    /// Watch the plugins folder, once there is one to watch.
    fn watch_plugins(&mut self) {
        if self.plugins.watcher.is_none() {
            self.plugins.watcher = self
                .plugins
                .dir
                .as_deref()
                .and_then(|dir| watch::Watcher::start(dir, self.plugins.to_app.clone()));
        }
    }

    /// Decide what becomes of the plugin `id` from what the user answered:
    /// run it, leave it refused, or ask.
    fn settle_plugin(&mut self, id: &str) {
        let Some(found) = self.plugins.get(id) else {
            return;
        };
        let Some(manifest) = found.manifest.clone() else {
            return;
        };
        let record = self.config_files.plugins.get(id);
        if record.is_some_and(|record| !record.enabled) {
            return self.stop_plugin(id, State::Disabled);
        }
        match trust::verdict(record, &manifest.permissions) {
            trust::Verdict::Allowed => self.spawn_plugin(id),
            trust::Verdict::Refused => self.stop_plugin(id, State::NotAllowed),
            trust::Verdict::Ask { .. } => {
                self.stop_plugin(id, State::NotAllowed);
                if !self.plugins.reviews.iter().any(|waiting| waiting == id) {
                    self.plugins.reviews.push_back(id.to_owned());
                }
            }
        }
    }

    /// Start `id`'s script afresh. Only for a plugin the user allowed.
    fn spawn_plugin(&mut self, id: &str) {
        let config = self
            .user_config
            .plugins
            .get(id)
            .cloned()
            .unwrap_or_default();
        self.plugins.instances += 1;
        let instance = self.plugins.instances;
        let to = self.plugins.to_app.clone();
        let Some(found) = self.plugins.get_mut(id) else {
            return;
        };
        let Some(manifest) = found.manifest.as_deref().cloned() else {
            return;
        };
        let host = Arc::new(Bridge {
            plugin: id.to_owned(),
            instance,
            to,
        });
        let options = Options {
            config,
            data_dir: data_dir(),
            limits: Limits::default(),
        };
        found.state = State::Loading;
        found.running = Some(Running {
            plugin: Arc::new(Plugin::spawn(manifest, options, host)),
            instance,
        });
        self.plugins.status.remove(id);
    }

    /// Stop `id`'s script, if one runs, and leave the plugin in `state`.
    fn stop_plugin(&mut self, id: &str, state: State) {
        let Some(found) = self.plugins.get_mut(id) else {
            return;
        };
        let was_forge =
            matches!(&found.state, State::Running(registration) if registration.forge.is_some());
        found.running = None;
        found.state = state;
        self.plugins.status.remove(id);
        self.end_plugin_activities(id);
        if was_forge {
            self.refresh_script_forges();
        }
    }

    /// Read the plugins folder again and start over: every script stops, and
    /// the allowed ones start afresh with the config as it is now. What
    /// "Reload keys and commands" and `diffui ctl reload` do for plugins.
    pub(crate) fn reload_plugins(&mut self) -> Vec<String> {
        let running: Vec<String> = self.plugins.found.iter().map(|f| f.id.clone()).collect();
        for id in running {
            self.end_plugin_activities(&id);
        }
        self.plugins.rescan(&self.config_files.plugins);
        self.refresh_script_forges();
        let problems = self.rederive_commands(None);
        let ids: Vec<String> = self
            .plugins
            .found
            .iter()
            .map(|found| found.id.clone())
            .collect();
        for id in ids {
            self.settle_plugin(&id);
            if let Some(found) = self.plugins.get_mut(&id) {
                found.announce = found.running.is_some();
            }
        }
        self.watch_plugins();
        problems
    }

    /// The registry and keymap again, from the config as read and the
    /// plugins as found. Returns what was wrong with any of it, having
    /// reported what is `about` one plugin directory: the rest was reported
    /// when it arose.
    fn rederive_commands(&mut self, about: Option<&Path>) -> Vec<String> {
        let (registry, keymap, problems) = Self::derive_commands(&self.user_config, &self.plugins);
        self.registry = registry;
        self.keymap = keymap;
        crate::app_menu::install(self);
        if let Some(dir) = about {
            let prefix = display_path(dir);
            let ours: Vec<String> = problems
                .iter()
                .filter(|problem| problem.starts_with(&prefix))
                .cloned()
                .collect();
            self.report_command_problems(&ours);
        }
        problems
    }

    /// The directories `names` of the plugins folder changed on disk: read
    /// each again and restart or reload its plugin.
    fn plugins_changed_on_disk(&mut self, names: Vec<String>) {
        let Some(root) = self.plugins.dir.clone() else {
            return;
        };
        for name in names {
            let dir = root.join(&name);
            let known = self.plugins.found.iter().position(|found| found.dir == dir);
            if !dir.join(MANIFEST_FILE).is_file() {
                // The directory, or its manifest, went away.
                if let Some(index) = known {
                    let id = self.plugins.found[index].id.clone();
                    self.stop_plugin(&id, State::Disabled);
                    self.plugins.found.remove(index);
                    self.rederive_commands(None);
                }
                continue;
            }
            let fresh = Found::read(&dir, &self.config_files.plugins);
            let same_manifest = known.is_some_and(|index| {
                let old = &self.plugins.found[index];
                old.manifest.is_some() && old.manifest.as_deref() == fresh.manifest.as_deref()
            });
            if let (true, Some(index)) = (same_manifest, known) {
                // Only the script changed: the same plugin, loaded again.
                let found = &mut self.plugins.found[index];
                if let Some(running) = &found.running {
                    found.announce = true;
                    found.state = State::Loading;
                    running.plugin.reload();
                    let id = found.id.clone();
                    self.end_plugin_activities(&id);
                }
                continue;
            }
            // A new plugin, or a manifest that changed: what it may do is
            // looked at again before anything runs.
            let id = fresh.id.clone();
            if let Some(index) = known {
                let old = self.plugins.found[index].id.clone();
                self.stop_plugin(&old, State::Disabled);
                self.plugins.found.remove(index);
            }
            let clash = self.plugins.get(&id).map(|other| other.dir.clone());
            let mut fresh = fresh;
            if let Some(other) = clash {
                fresh.state = State::Failed(format!(
                    "{} already uses the id {id:?}",
                    display_path(&other)
                ));
                fresh.manifest = None;
                fresh.rows.clear();
            }
            let failed = match &fresh.state {
                State::Failed(why) => Some(why.clone()),
                _ => None,
            };
            let at = self
                .plugins
                .found
                .iter()
                .position(|found| found.dir > dir)
                .unwrap_or(self.plugins.found.len());
            self.plugins.found.insert(at, fresh);
            self.rederive_commands(Some(&dir));
            match failed {
                Some(why) => self.push_error_toast(format!("{name} didn't load"), &why),
                None => {
                    self.settle_plugin(&id);
                    if let Some(found) = self.plugins.get_mut(&id) {
                        found.announce = found.running.is_some();
                    }
                }
            }
        }
    }

    pub(crate) fn on_plugin_message(&mut self, message: FromPlugin, cx: &mut Cx) {
        match message {
            FromPlugin::Call {
                plugin,
                instance,
                call,
                reply,
            } => {
                // A script that was replaced gets no answer: dropping the
                // reply rejects its promise.
                if self.plugins.is_current(&plugin, instance) {
                    self.answer_plugin(&plugin, call, reply, cx);
                }
            }
            FromPlugin::Event {
                plugin,
                instance,
                event,
            } => {
                if self.plugins.is_current(&plugin, instance) {
                    self.on_plugin_event(&plugin, event);
                }
            }
            FromPlugin::Ran { label, result } => match result {
                // A reload or a stop says so itself.
                Ok(()) | Err(PluginError::Gone | PluginError::Failed(_)) => {}
                Err(error) => self.push_error_toast(format!("{label} failed"), &error.to_string()),
            },
            FromPlugin::Changed(names) => self.plugins_changed_on_disk(names),
        }
    }

    fn on_plugin_event(&mut self, id: &str, event: PluginEvent) {
        let Some(found) = self.plugins.get_mut(id) else {
            return;
        };
        let name = found.name.clone();
        let main = found
            .manifest
            .as_ref()
            .map(|manifest| manifest.main.clone());
        let announce = std::mem::take(&mut found.announce);
        let forge = found.manifest.as_ref().is_some_and(|m| m.forge.is_some());
        match event {
            PluginEvent::Loaded(registration) => {
                found.state = State::Running(registration);
                if announce {
                    self.push_info_toast(format!("Reloaded {name}"), "");
                }
                self.tell_open_repositories(id);
            }
            PluginEvent::Failed(why) => {
                // A load error leads with where it is (`main.js:3:7: …`).
                let why = main
                    .and_then(|main| why.strip_prefix(&format!("{main} did not load: ")))
                    .map_or(why.clone(), str::to_owned);
                found.state = State::Failed(why.clone());
                self.plugins.status.remove(id);
                self.end_plugin_activities(id);
                self.push_error_toast(format!("{name} failed"), &why);
            }
        }
        if forge {
            self.refresh_script_forges();
        }
    }

    /// Show the permission dialog for the next plugin waiting on one, once
    /// nothing else has the keyboard.
    fn raise_permission_dialog(&mut self) {
        if !self.modes.is_empty() {
            return;
        }
        while let Some(id) = self.plugins.reviews.pop_front() {
            if self.plugins.get(&id).is_some_and(|f| f.manifest.is_some()) {
                self.push_mode(Mode::PluginPermission(PermissionDialog { plugin: id }));
                return;
            }
        }
    }

    pub(crate) fn update_plugins(&mut self, action: PluginAction, cx: &mut Cx) {
        match action {
            PluginAction::Answer(allowed) => {
                let Some(Mode::PluginPermission(dialog)) =
                    self.pop_mode(ModeKind::PluginPermission)
                else {
                    return;
                };
                self.answer_permission(&dialog.plugin, allowed);
            }
            PluginAction::Review(id) => {
                self.plugins.reviews.retain(|waiting| *waiting != id);
                if self.plugins.get(&id).is_some_and(|f| f.manifest.is_some()) {
                    self.push_mode(Mode::PluginPermission(PermissionDialog { plugin: id }));
                }
            }
            PluginAction::Enable(id, enabled) => {
                let off = self
                    .plugins
                    .get(&id)
                    .is_some_and(|found| found.state == State::Disabled);
                if enabled != off {
                    return;
                }
                let mut record = self
                    .config_files
                    .plugins
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                record.enabled = enabled;
                if let Err(error) = self.config_files.write_plugin(&id, record) {
                    self.push_error_toast("Couldn't save settings", &error);
                }
                if enabled {
                    if let Some(found) = self.plugins.get_mut(&id)
                        && found.manifest.is_some()
                    {
                        found.state = State::NotAllowed;
                    }
                    self.rederive_commands(None);
                    self.settle_plugin(&id);
                } else {
                    self.plugins.reviews.retain(|waiting| *waiting != id);
                    self.stop_plugin(&id, State::Disabled);
                    self.rederive_commands(None);
                }
            }
            PluginAction::Reveal(id) => {
                let dir = match &id {
                    Some(id) => self.plugins.get(id).map(|found| found.dir.clone()),
                    None => self.plugins.dir.clone(),
                };
                if let Some(dir) = dir {
                    // The folder is made on demand: "Plugins folder" is how a
                    // user finds where the first one goes.
                    let _ = std::fs::create_dir_all(&dir);
                    self.watch_plugins();
                    crate::settings::reveal_path(&dir);
                }
            }
            PluginAction::ShowLog(id) => self.show_plugin_log(&id, cx),
        }
    }

    /// The user answered the permission dialog for `id`.
    fn answer_permission(&mut self, id: &str, allowed: bool) {
        let Some(manifest) = self.plugins.get(id).and_then(|f| f.manifest.clone()) else {
            return;
        };
        let record = Record {
            enabled: true,
            allowed: Some(allowed),
            permissions: Some(manifest.permissions.clone()),
        };
        if let Err(error) = self.config_files.write_plugin(id, record) {
            self.push_error_toast("Couldn't save settings", &error);
        }
        if self
            .plugins
            .get(id)
            .is_some_and(|found| found.state == State::Disabled)
        {
            self.rederive_commands(None);
        }
        if allowed {
            // Allowed again as it is: a running script keeps running.
            if self.plugins.plugin(id).is_none() {
                self.spawn_plugin(id);
            }
        } else {
            self.stop_plugin(id, State::NotAllowed);
        }
    }

    /// Run `command`'s handler for `arg`, on `tab` (the active tab when the
    /// action was built).
    pub(crate) fn run_plugin_command(
        &mut self,
        command: Arc<PluginCommand>,
        tab: TabId,
        arg: CommandArg,
    ) {
        if !self.is_active(tab) {
            return;
        }
        let Some(plugin) = self.plugins.plugin(&command.plugin) else {
            return;
        };
        let Some(repo) = self.repo_handle(tab, &command.plugin) else {
            return;
        };
        let value = |var: Var| crate::user_commands::value(self, &arg, &var).ok().flatten();
        let context = CommandContext {
            repo,
            change_id: value(Var::ChangeId),
            commit_id: value(Var::CommitId),
            bookmark: value(Var::Bookmark),
            path: value(Var::Path),
            line: value(Var::Line).and_then(|line| line.parse().ok()),
        };
        let to = self.plugins.to_app.clone();
        let label = command.label.clone();
        plugin.call::<serde_json::Value>(
            PluginCall::Command {
                id: command.id.clone(),
                context,
            },
            move |result| {
                let _ = to.unbounded_send(FromPlugin::Ran {
                    label,
                    result: result.map(|_| ()),
                });
            },
        );
    }

    /// `tab`'s repository as the plugin `id` is handed it now, or `None` when
    /// the tab shows no local repository.
    pub(crate) fn repo_handle(&mut self, tab: TabId, id: &str) -> Option<RepoHandle> {
        let entry = self.tabs.iter().find(|entry| entry.id == tab)?;
        let repository = entry.state.repository.as_ref()?;
        let repo = entry.repo_id()?;
        let handle = RepoHandle {
            id: String::new(),
            op: entry
                .state
                .session
                .graph_operation()
                .unwrap_or_default()
                .to_owned(),
            root: repository.root.display().to_string(),
            forge_remote: entry
                .state
                .forge
                .home
                .as_ref()
                .and_then(|(_, remote)| forge::remote_of(remote, id)),
        };
        Some(RepoHandle {
            id: self.plugins.mint(tab, repo),
            ..handle
        })
    }

    /// The tab a handle names, when the handle is current: the tab is on
    /// screen, still shows that repository, and is at that operation.
    fn resolve_handle(&self, handle: &RepoRef) -> Option<TabId> {
        let (tab, repo) = self.plugins.minted.get(handle.id.parse::<usize>().ok()?)?;
        let entry = self.tabs.iter().find(|entry| entry.id == *tab)?;
        let current = self.is_active(*tab)
            && entry.repo_id().as_ref() == Some(repo)
            && entry.state.session.graph_operation().unwrap_or_default() == handle.op;
        current.then_some(*tab)
    }
}

/// What the permission dialog and the Settings group ask for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PluginAction {
    /// The permission dialog's answer: allow, or don't.
    Answer(bool),
    /// Open the permission dialog for this plugin.
    Review(String),
    Enable(String, bool),
    /// Show this plugin's directory, or the plugins folder.
    Reveal(Option<String>),
    ShowLog(String),
}
