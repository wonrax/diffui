//! The app's side of a plugin: the trait it implements and what it hears.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use serde::Serialize;
use serde_json::Value;

use crate::actor::ToActor;
use crate::js::JsError;
use crate::protocol::HostCall;

/// What the app provides to plugins.
///
/// Both methods are called on a plugin's own thread, never the UI thread, and
/// must not block it: hand the work to the app and return. One implementation
/// serves every plugin; `plugin` is the manifest id of the caller.
pub trait Host: Send + Sync + 'static {
    /// A plugin asked for something only the app can do. Answer through
    /// `reply` from any thread, at any later time. Reject a
    /// [`HostCall::Repo`] whose handle is stale with [`Reply::stale`].
    fn call(&self, plugin: &str, call: HostCall, reply: Reply);

    /// The plugin's lifecycle moved.
    fn event(&self, plugin: &str, event: PluginEvent);
}

/// The answer to one [`HostCall`]. Dropping it unanswered rejects the
/// script's promise.
pub struct Reply {
    pub(crate) to: Option<Sender<ToActor>>,
    pub(crate) generation: u64,
    pub(crate) id: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl Reply {
    /// Resolve the script's promise with `value` as JSON.
    pub fn ok(self, value: impl Serialize) {
        let result = serde_json::to_value(value).map_err(|error| {
            HostError::new("HostError", format!("the reply did not serialise: {error}"))
        });
        self.send(result);
    }

    /// Reject the script's promise with an `Error` carrying `message`.
    pub fn err(self, message: impl Into<String>) {
        self.send(Err(HostError::new("HostError", message)));
    }

    /// Reject a repository call whose handle is no longer current. The
    /// script sees an `Error` named "StaleError".
    pub fn stale(self) {
        self.send(Err(HostError::new(
            "StaleError",
            "This repository handle is stale: the repository changed after it was made",
        )));
    }

    /// The plugin stopped waiting: the script aborted the call, or the plugin
    /// was reloaded or failed. Answering is then optional.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn send(mut self, result: Result<Value, HostError>) {
        if let Some(to) = self.to.take() {
            // A plugin that is gone has nobody to tell.
            let _ = to.send(ToActor::HostReply {
                generation: self.generation,
                id: self.id,
                result,
                io: false,
            });
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(to) = self.to.take() {
            let _ = to.send(ToActor::HostReply {
                generation: self.generation,
                id: self.id,
                result: Err(HostError::new(
                    "HostError",
                    "diffui dropped the request without answering",
                )),
                io: false,
            });
        }
    }
}

/// Why a host call was refused or failed, as the script sees it: an `Error`
/// with this `name` and `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostError {
    pub name: &'static str,
    pub message: String,
}

impl HostError {
    pub fn new(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            name,
            message: message.into(),
        }
    }

    /// The manifest doesn't allow it.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("PermissionError", message)
    }
}

/// What a plugin's main module registered while it loaded.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct Registration {
    /// Ids with a handler. Loading fails unless this covers every command
    /// the manifest declares.
    pub commands: Vec<String>,
    /// The forge provider's methods (of [`crate::protocol::FORGE_METHODS`]),
    /// or `None` without a provider.
    pub forge: Option<Vec<String>>,
    /// Event names with at least one listener; the rest need not be sent.
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEvent {
    /// The main module finished loading, at start or after a reload.
    Loaded(Registration),
    /// The plugin stopped: it failed to load, ran past its time or memory
    /// budget, or its thread panicked. Calls fail with
    /// [`PluginError::Failed`] until it is reloaded.
    Failed(String),
}

/// Where a plugin is in its life, for a settings list or an error surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    Loading,
    Ready(Registration),
    Failed(String),
}

/// Why a call into a plugin has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// The handler threw or rejected.
    Threw(JsError),
    /// The plugin is in the failed state; the text says why.
    Failed(String),
    /// Nothing is registered under that command id, forge method or the
    /// plugin has no forge provider.
    NotRegistered(String),
    /// The plugin was reloaded or shut down while the call was out.
    Gone,
    /// The handler didn't settle in time.
    TimedOut(String),
    /// The handler returned something that isn't the expected shape.
    BadReply(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::Threw(error) => error.fmt(f),
            PluginError::Failed(why) => write!(f, "the plugin stopped: {why}"),
            PluginError::NotRegistered(what) => write!(f, "the plugin registered no {what}"),
            PluginError::Gone => f.write_str("the plugin was reloaded or closed"),
            PluginError::TimedOut(what) => write!(f, "{what} did not finish in time"),
            PluginError::BadReply(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for PluginError {}

/// One line a plugin logged through `console`, or that the host logged about
/// it (uncaught errors, refused loads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
