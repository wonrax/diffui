//! AI provider plugins as the app uses them: which there are and whether
//! each can run without asking (for the automatic pick and Settings → AI),
//! a completion streamed into the app, and what a provider's page asks it.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use diffui_core::repo::CancelFlag;
use diffui_plugins::protocol::{AiAccount, AiCall, AiModel, AiRequest, PluginCall};
use diffui_plugins::{ConfigKind, PluginError, secrets};
use futures::channel::mpsc;

use super::{FromPlugin, Origin, State};
use crate::Diffui;

/// One AI provider plugin, as Settings and the automatic pick see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) bundled: bool,
    /// On, allowed, and loading or loaded.
    pub(crate) usable: bool,
    /// Its `ready` setting says it can run: the program is there, the key
    /// is set.
    pub(crate) ready: bool,
    /// It runs a program rather than calling an API with a key.
    pub(crate) program: Option<String>,
    /// What Settings says under its name.
    pub(crate) detail: String,
}

/// What a completion sends the app.
#[derive(Debug)]
pub(crate) enum Streamed {
    Text(String),
    Ended(Ended),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    Done,
    Cancelled,
    /// `kind` is the `AiError`'s, when the provider threw one.
    Failed {
        kind: Option<String>,
        message: String,
    },
}

/// Whether `program` names something there: a path that exists, or a name
/// on the `PATH`.
pub(crate) fn program_found(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    let path = std::path::Path::new(program);
    if program.contains('/') {
        let expanded = match program.strip_prefix("~/") {
            Some(rest) => crate::app_files::home()
                .map(|home| home.join(rest))
                .unwrap_or_else(|| path.to_path_buf()),
            None => path.to_path_buf(),
        };
        return expanded.exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

impl Diffui {
    /// The AI provider plugins: the bundled ones in the automatic pick's
    /// order, then the rest by name. `stored` is the secret
    /// store's accounts (listing them never prompts; reading one could).
    pub(crate) fn ai_providers(&self, stored: &HashSet<String>) -> Vec<AiProvider> {
        let mut providers: Vec<AiProvider> = self
            .plugins
            .found
            .iter()
            .filter_map(|found| {
                let manifest = found.manifest.as_ref()?;
                let claim = manifest.ai.as_ref()?;
                let config = manifest.resolve_config(&self.plugin_config(&found.id)).0;
                let field = claim
                    .ready
                    .as_ref()
                    .and_then(|key| manifest.config.iter().find(|field| &field.key == key));
                let (ready, program, detail) = match field {
                    Some(field) if field.kind == ConfigKind::Path => {
                        let program = config
                            .get(&field.key)
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_owned();
                        let found = program_found(&program);
                        let detail = if found {
                            format!("Runs {program}")
                        } else {
                            format!("{program} isn't installed")
                        };
                        (found, Some(program), detail)
                    }
                    Some(field) if field.kind == ConfigKind::String => {
                        let value = config
                            .get(&field.key)
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .trim()
                            .to_owned();
                        let detail = if value.is_empty() {
                            format!("No {}", field.title().to_lowercase())
                        } else {
                            format!("{}: {value}", field.title())
                        };
                        (!value.is_empty(), None, detail)
                    }
                    Some(field) => {
                        let saved = stored.contains(&secrets::account(&found.id, &field.key, None));
                        let from_env = field.secret.as_ref().and_then(|secret| {
                            secret
                                .env_for("")
                                .iter()
                                .find(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()))
                                .cloned()
                        });
                        let detail = match (saved, &from_env) {
                            (true, _) => format!("{} saved", field.title()),
                            (false, Some(var)) => format!("{} from {var}", field.title()),
                            (false, None) => format!("No {}", field.title()),
                        };
                        (saved || from_env.is_some(), None, detail)
                    }
                    None => (true, None, String::new()),
                };
                let detail = match &found.state {
                    State::Disabled => "Turned off in Plugins".to_owned(),
                    State::NotAllowed => "Not allowed to run".to_owned(),
                    State::Failed(_) => "Didn't load".to_owned(),
                    State::Loading | State::Running(_) => detail,
                };
                // What the user calls it, when the provider lets them.
                let name = claim
                    .label
                    .as_ref()
                    .and_then(|key| config.get(key)?.as_str())
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&found.name)
                    .to_owned();
                Some(AiProvider {
                    id: found.id.clone(),
                    name,
                    bundled: found.origin != Origin::Folder,
                    usable: matches!(found.state, State::Loading | State::Running(_)),
                    ready,
                    program,
                    detail,
                })
            })
            .collect();
        providers.sort_by(|a, b| {
            let place = |provider: &AiProvider| {
                let auto = crate::ai::AUTO_ORDER
                    .iter()
                    .position(|id| *id == provider.id)
                    .filter(|_| provider.bundled);
                (auto.is_none(), auto, provider.name.to_lowercase())
            };
            place(a).cmp(&place(b))
        });
        providers
    }

    /// Whether provider `id` registered `method`.
    pub(crate) fn ai_answers(&self, id: &str, method: &str) -> bool {
        matches!(
            self.plugins.get(id).map(|found| &found.state),
            Some(State::Running(registration))
                if registration.ai.as_ref().is_some_and(|methods| methods.iter().any(|m| m == method))
        )
    }

    /// Ask provider `id` to complete `request`. What it writes, then how it
    /// ended, arrive on the receiver; raising `cancel` stops it (its fetch
    /// is dropped, its program killed).
    pub(crate) fn ai_complete(
        &self,
        id: &str,
        request: AiRequest,
        cancel: CancelFlag,
    ) -> Result<mpsc::UnboundedReceiver<Streamed>, String> {
        let Some(plugin) = self.plugins.plugin(id) else {
            let name = self.plugins.get(id).map_or(id, |found| found.name.as_str());
            return Err(format!(
                "{name} isn't running: turn it on in Settings → Plugins"
            ));
        };
        let (to, from) = mpsc::unbounded();
        let done = Arc::new(AtomicBool::new(false));
        let parts = to.clone();
        let call = plugin.call_streaming::<serde_json::Value>(
            PluginCall::Ai {
                call: AiCall::Complete { request },
            },
            move |sent, reply| {
                for part in sent {
                    if let serde_json::Value::String(text) = part {
                        let _ = parts.unbounded_send(Streamed::Text(text));
                    }
                }
                reply.ok(serde_json::Value::Null);
            },
            {
                let done = done.clone();
                move |result| {
                    done.store(true, Ordering::Relaxed);
                    let ended = match result {
                        Ok(_) => Ended::Done,
                        Err(PluginError::Gone) => Ended::Cancelled,
                        Err(PluginError::Threw(error)) => Ended::Failed {
                            kind: error.ai.clone(),
                            message: error.message,
                        },
                        Err(other) => Ended::Failed {
                            kind: None,
                            message: other.to_string(),
                        },
                    };
                    let _ = to.unbounded_send(Streamed::Ended(ended));
                }
            },
        );
        // Stop is a flag; the call is stopped from here.
        crate::spawn::runtime().spawn(async move {
            while !done.load(Ordering::Relaxed) {
                if cancel.is_cancelled() {
                    plugin.abort(call);
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });
        Ok(from)
    }

    /// Ask provider `id` for its models, for its page's picker.
    pub(super) fn ask_ai_models(&self, id: &str) {
        let Some(plugin) = self.plugins.plugin(id) else {
            return;
        };
        let (to, plugin_id) = (self.plugins.to_app.clone(), id.to_owned());
        plugin.call::<Vec<AiModel>>(
            PluginCall::Ai {
                call: AiCall::Models,
            },
            move |result| {
                let _ = to.unbounded_send(FromPlugin::AiModels {
                    plugin: plugin_id,
                    result: result.map_err(message),
                });
            },
        );
    }

    /// Ask provider `id` what it runs as, for its page.
    pub(super) fn ask_ai_account(&self, id: &str) {
        let Some(plugin) = self.plugins.plugin(id) else {
            return;
        };
        let (to, plugin_id) = (self.plugins.to_app.clone(), id.to_owned());
        plugin.call::<AiAccount>(
            PluginCall::Ai {
                call: AiCall::Account,
            },
            move |result| {
                let _ = to.unbounded_send(FromPlugin::AiAccount {
                    plugin: plugin_id,
                    result: result.map(|account| account.label).map_err(message),
                });
            },
        );
    }
}

/// A failed call in the provider's own words.
fn message(error: PluginError) -> String {
    match error {
        PluginError::Threw(thrown) => thrown.message,
        other => other.to_string(),
    }
}
