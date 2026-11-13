//! Script plugins for diffui: everything about running one except app state.
//!
//! A plugin is a directory with a `plugin.toml` ([`Manifest`]) and an ES
//! module, JavaScript or TypeScript (types are erased in place, nothing is
//! compiled). [`Plugin::spawn`] gives it a thread of its own with a QuickJS
//! context that has no ambient authority: the script reaches the world only
//! through the `"diffui"` module, and each function there is checked against
//! the manifest's permissions.
//!
//! - `http`, `exec`, `auth`, `kv`, `console` and `config` are served here.
//! - Repository reads, jj commands and UI slots are [`protocol::HostCall`]s,
//!   forwarded to the app's [`Host`].
//! - The app drives a plugin with [`protocol::PluginCall`]s through
//!   [`Plugin::call`]: run a command, ask the forge provider, deliver an
//!   event.
//!
//! Plugin code never runs on the caller's thread and nothing here blocks it:
//! every result arrives through a callback or a [`Host`] method.
//!
//! The script-side contract is `assets/diffui.d.ts` ([`TYPES`]).

mod actor;
mod builtin;
mod host;
mod js;
mod manifest;
mod memory;
pub mod protocol;
mod typescript;

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;

use actor::{Actor, Status, ToActor, lock};
pub use host::{
    Host, LogLevel, LogLine, PluginError, PluginEvent, PluginState, Registration, Reply,
};
pub use js::JsError;
pub use manifest::{
    API_VERSION, CommandDecl, ConfigField, ConfigKind, Diagnostic, ForgeClaim, HostPattern,
    JjAccess, MANIFEST_FILE, Manifest, MenuPlace, Permissions,
};
use protocol::PluginCall;

/// The type definitions of the plugin API, for `diffui plugin init` to write
/// next to a new plugin and for the agent docs.
pub const TYPES: &str = include_str!("../assets/diffui.d.ts");

/// What a plugin is started with, beyond its manifest.
#[derive(Debug, Clone)]
pub struct Options {
    /// The user's `[plugins.<id>]` table from config.toml. The script sees it
    /// as `config`, over the manifest's schema defaults; its `token_cmd`, an
    /// argv, answers `auth.token`.
    pub config: toml::Table,
    /// Where `kv` keeps this plugin's `<id>.json`.
    pub data_dir: PathBuf,
    pub limits: Limits,
}

/// The bounds a plugin runs inside.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Heap the script may hold. Passing it fails the plugin.
    pub memory: usize,
    /// Script stack depth, in bytes. Passing it is a `RangeError` the script
    /// can catch.
    pub stack: usize,
    /// How long the script may run without returning to its event loop
    /// (awaiting counts as returning). Passing it fails the plugin.
    pub turn: Duration,
    /// How long the main module's top level may take, awaits included.
    pub load: Duration,
    /// How long a forge method may take to settle.
    pub forge_call: Duration,
    /// `http.fetch`'s timeout unless the script passes `timeoutMs`.
    pub http_timeout: Duration,
    /// `exec`'s timeout unless the script passes `timeoutMs`.
    pub exec_timeout: Duration,
    /// The largest response body, or output of one `exec` stream, in bytes.
    pub max_response: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: 64 * 1024 * 1024,
            stack: 512 * 1024,
            turn: Duration::from_millis(500),
            load: Duration::from_secs(10),
            forge_call: Duration::from_secs(60),
            http_timeout: Duration::from_secs(60),
            exec_timeout: Duration::from_secs(300),
            max_response: 8 * 1024 * 1024,
        }
    }
}

/// A running plugin. Dropping it stops the script.
pub struct Plugin {
    manifest: Arc<Manifest>,
    status: Arc<Mutex<Status>>,
    to: Sender<ToActor>,
}

impl Plugin {
    /// Start the plugin on its own thread and begin loading its main module.
    /// Returns at once; [`PluginEvent::Loaded`] or [`PluginEvent::Failed`]
    /// reaches `host` when loading ends. Calls made before then wait for it.
    pub fn spawn(manifest: Manifest, options: Options, host: Arc<dyn Host>) -> Plugin {
        let manifest = Arc::new(manifest);
        let status = Arc::new(Mutex::new(Status {
            state: PluginState::Loading,
            log: Default::default(),
        }));
        let (to, inbox) = mpsc::channel();
        // Room for the script's stack limit plus the engine's own frames.
        let stack = options.limits.stack + 1024 * 1024;
        let actor = {
            let (manifest, status, own) = (manifest.clone(), status.clone(), to.clone());
            move || Actor::new(manifest, options, host, status, inbox, own).run()
        };
        let spawned = std::thread::Builder::new()
            .name(format!("plugin-{}", manifest.id))
            .stack_size(stack)
            .spawn(actor);
        if let Err(error) = spawned {
            lock(&status).state = PluginState::Failed(format!("no thread for the plugin: {error}"));
        }
        Plugin {
            manifest,
            status,
            to,
        }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn state(&self) -> PluginState {
        lock(&self.status).state.clone()
    }

    /// What the plugin logged, oldest first: its `console` output, uncaught
    /// errors with their stacks, and why it failed. The last few hundred
    /// lines are kept; all of it also goes to `tracing`.
    pub fn log(&self) -> Vec<LogLine> {
        lock(&self.status).log.iter().cloned().collect()
    }

    /// Call into the script. `reply` runs on the plugin's thread when the
    /// handler settles, with its JSON result read as `T` (the reply type
    /// named on the [`PluginCall`] variant, or `serde_json::Value`).
    pub fn call<T: DeserializeOwned>(
        &self,
        call: PluginCall,
        reply: impl FnOnce(Result<T, PluginError>) + Send + 'static,
    ) {
        let what = match &call {
            PluginCall::Forge { call, .. } => call.method(),
            PluginCall::Command { .. } | PluginCall::Event { .. } => "the handler",
        };
        let typed = move |result: Result<serde_json::Value, PluginError>| {
            reply(result.and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    PluginError::BadReply(format!("{what} returned the wrong shape: {error}"))
                })
            }));
        };
        let sent = self.to.send(ToActor::Call {
            call: Box::new(call),
            reply: Box::new(typed),
        });
        if let Err(mpsc::SendError(ToActor::Call { reply, .. })) = sent {
            reply(Err(PluginError::Failed("its thread is gone".into())));
        }
    }

    /// Throw the script's context away and load the plugin's files again,
    /// under the same manifest and options. Calls that were out fail with
    /// [`PluginError::Gone`]; a failed plugin gets a fresh start. A changed
    /// `plugin.toml` needs a new [`Plugin`] instead, because the app has to
    /// look at its permissions first.
    pub fn reload(&self) {
        let _ = self.to.send(ToActor::Reload);
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        let _ = self.to.send(ToActor::Shutdown);
    }
}
