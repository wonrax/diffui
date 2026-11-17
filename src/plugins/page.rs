//! A plugin's page in Settings (the gear on its row): its `[config.schema]`
//! as controls, and for a forge whose schema has a per-host `token` secret,
//! an Account area with a row per host.
//!
//! A setting is saved to `settings.toml` under `[plugins.<id>.config]` and
//! the plugin restarts with it; a key config.toml's `[plugins.<id>]` sets is
//! shown locked. A secret goes to the secret store (the Keychain), never to
//! either file; a token is checked with the forge's `account` before it is
//! kept, and it is never shown back, logged or put in a toast.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use diffui_plugins::protocol::{Account, AccountMethod, AiModel, ForgeCall, PluginCall, Secret};
use diffui_plugins::secrets;
use diffui_plugins::{ConfigField, ConfigKind, Manifest, PER_HOST};

use super::ai::program_found;
use super::{FromPlugin, PluginAction, State};
use crate::modes::ModeKind;
use crate::overlay_kit;
use crate::prose::Prose;
use crate::theme::{self, text_size};
use crate::typography;
use crate::ui::{self, Look, Segment, Segmented, Tip as _};
use crate::{Action, Cx, Diffui, icons, input};

/// The open page.
pub(crate) struct PluginPage {
    pub(crate) id: String,
    /// Hosts shown beyond the ones the forge knows: the one an error named.
    extra: Vec<String>,
    /// What is typed in each field, by the field's id.
    drafts: HashMap<&'static str, String>,
    /// What each host's account check said.
    accounts: HashMap<String, Check>,
    /// The accounts the store has a secret for, read when the page opens and
    /// after each change: asking per frame could prompt.
    stored: HashSet<String>,
    /// Tokens being checked, by host, and why one was refused.
    validating: HashSet<String>,
    rejected: HashMap<String, String>,
    /// The host whose removal waits on a yes, because it deletes a token.
    removing: Option<String>,
    /// An AI provider's `account()` answer; `None` while it is asked.
    ai_account: Option<Result<String, String>>,
    /// An AI provider's models, once asked for: `None` while they come.
    models: Option<Option<Result<Vec<AiModel>, String>>>,
    /// The model list is open under the model setting.
    picking: bool,
}

impl PluginPage {
    /// Nothing is being asked: every account check and token check has
    /// answered.
    #[cfg(test)]
    pub(crate) fn settled(&self) -> bool {
        self.validating.is_empty() && !self.accounts.values().any(|check| *check == Check::Asking)
    }

    /// The host whose removal is waiting on a yes.
    #[cfg(test)]
    pub(crate) fn removing(&self) -> Option<&str> {
        self.removing.as_deref()
    }

    /// The models the list offers, once they came.
    #[cfg(all(test, unix))]
    pub(crate) fn models(&self) -> Option<Vec<String>> {
        match self.models.as_ref()? {
            Some(Ok(models)) => Some(models.iter().map(|model| model.id.clone()).collect()),
            _ => None,
        }
    }

    /// Show `account` and the open model list with `models`, for a scene.
    #[cfg(test)]
    pub(crate) fn show_ai(&mut self, account: &str, models: Vec<AiModel>) {
        self.ai_account = Some(Ok(account.to_owned()));
        self.models = Some(Some(Ok(models)));
        self.picking = true;
    }

    /// Who `host`'s check said the user is.
    #[cfg(test)]
    pub(crate) fn signed_in(&self, host: &str) -> Option<String> {
        match self.accounts.get(host)? {
            Check::Known(account) => account.login.clone(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Check {
    Asking,
    Known(Account),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PageAction {
    /// Open `id`'s page, with `host` among its accounts.
    Open { id: String, host: Option<String> },
    /// Back to the list of plugins.
    Back,
    /// Set `key` (for `host`, if per host) to `value`; `None` removes it.
    Set {
        key: String,
        host: Option<String>,
        value: Option<toml::Value>,
    },
    /// The text in a field changed.
    Draft { field: &'static str, text: String },
    /// Save the text of `key`'s field.
    Commit { key: String, host: Option<String> },
    /// Pick `key`'s path with the system's file dialog.
    Browse { key: String },
    /// Check and keep the typed secret.
    SaveSecret { key: String, host: Option<String> },
    /// Remove a stored secret: "Sign out" for a token.
    ClearSecret { key: String, host: Option<String> },
    /// Open the forge's page for making a token for `host`.
    TokenPage { host: String },
    /// Add the typed host to the forge's `hosts` setting.
    AddHost,
    /// Give the typed name (`work`, or `git.acme.com/work`) a row of its
    /// own: another account on the host, with its own token.
    AddAccount,
    /// Remove a host: from `hosts`, its per-host settings and its secrets.
    /// Unconfirmed, a host with a stored secret asks first.
    RemoveHost { host: String, confirmed: bool },
    /// Keep the host whose removal was asked about.
    KeepHost,
    /// Open or close an AI provider's model list, asking for it the first
    /// time.
    PickModels,
    /// Set the model setting to this one and close the list.
    UseModel(String),
}

/// A field's id: the input widget's ids are static, so each distinct name
/// is kept once for the life of the process.
pub(crate) fn field_id(name: String) -> &'static str {
    static IDS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut ids = IDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(known) = ids.iter().find(|known| **known == name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    ids.push(leaked);
    leaked
}

pub(super) fn draft_id(key: &str, host: Option<&str>) -> &'static str {
    match host {
        Some(host) => field_id(format!("plugin-page-{key}@{host}")),
        None => field_id(format!("plugin-page-{key}")),
    }
}

pub(super) const NEW_HOST: &str = "plugin-page-new-host";
pub(crate) const NEW_ACCOUNT: &str = "plugin-page-new-account";

/// `host`'s table under `per_host` in a `[plugins.<id>]` table.
fn lookup_host<'a>(table: &'a toml::Table, host: &str) -> Option<&'a toml::Table> {
    table
        .get(PER_HOST)?
        .as_table()?
        .get(&host.to_ascii_lowercase())?
        .as_table()
}

/// The store's accounts for `host`'s per-host secrets of `id`.
fn host_secrets(manifest: &Manifest, id: &str, host: &str) -> Vec<String> {
    manifest
        .config
        .iter()
        .filter(|field| field.kind == ConfigKind::Secret && field.per_host)
        .map(|field| secrets::account(id, &field.key, Some(host)))
        .collect()
}

/// Where a setting's value lives in a `[plugins.<id>]` table.
fn lookup<'a>(table: &'a toml::Table, key: &str, host: Option<&str>) -> Option<&'a toml::Value> {
    match host {
        None => table.get(key),
        Some(host) => lookup_host(table, host)?.get(key),
    }
}

/// `over` laid over `under`, a key at a time, per host for the per-host table.
pub(super) fn overlay(under: &mut toml::Table, over: &toml::Table) {
    for (key, value) in over {
        match (under.get_mut(key), value) {
            (Some(toml::Value::Table(hosts)), toml::Value::Table(more)) if key == PER_HOST => {
                for (host, values) in more {
                    match (hosts.get_mut(host), values) {
                        (Some(toml::Value::Table(held)), toml::Value::Table(values)) => {
                            held.extend(values.clone());
                        }
                        _ => {
                            hosts.insert(host.clone(), values.clone());
                        }
                    }
                }
            }
            _ => {
                under.insert(key.clone(), value.clone());
            }
        }
    }
}

/// A setting's value as its field shows it.
fn as_text(value: Option<&toml::Value>) -> String {
    match value {
        Some(toml::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// What an enum's value is called on its segment.
fn value_label(manifest: &Manifest, value: &str) -> String {
    let cli = manifest.forge.as_ref().and_then(|forge| forge.cli.clone());
    match (value, cli) {
        ("cli", Some(cli)) => cli,
        ("cli", None) => "CLI".to_owned(),
        _ => {
            let mut chars = value.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

/// The per-host `token` secret, when the plugin is a forge that has one.
fn token_field(manifest: &Manifest) -> Option<&ConfigField> {
    manifest.forge.as_ref()?;
    manifest.secret("token").filter(|field| field.per_host)
}

impl Diffui {
    /// `[plugins.<id>]` as the plugin sees it: what Settings saved, under
    /// config.toml's table, which wins a key at a time.
    pub(crate) fn plugin_config(&self, id: &str) -> toml::Table {
        let mut config = self
            .config_files
            .plugins
            .get(id)
            .map(|record| record.config.clone())
            .unwrap_or_default();
        if let Some(user) = self.user_config.plugins.get(id) {
            overlay(&mut config, user);
        }
        config
    }

    /// The hosts `id`'s page has a row for: the forge's default host, the
    /// concrete hosts it claims beyond its patterns, the ones with a token
    /// or per-host settings, and any the page was opened for.
    pub(super) fn page_hosts(&self, id: &str) -> Vec<String> {
        let Some(found) = self.plugins.get(id) else {
            return Vec::new();
        };
        let Some(forge) = found.manifest.as_ref().and_then(|m| m.forge.as_ref()) else {
            return Vec::new();
        };
        let config = self.plugin_config(id);
        let mut hosts: Vec<String> = found
            .extra_hosts
            .iter()
            .chain(&super::forge::user_hosts(Some(&config)))
            .map(ToString::to_string)
            .filter(|host| !host.contains('*'))
            .collect();
        // Another account on a host is a row of its own, under its host's.
        let scopes = secrets::token_scopes(&*self.plugins.secrets, id);
        hosts.extend(
            scopes
                .iter()
                .map(|scope| secrets::split_scope(scope).0.to_owned()),
        );
        hosts.extend(scopes);
        if let Some(per_host) = config.get(PER_HOST).and_then(toml::Value::as_table) {
            hosts.extend(per_host.keys().cloned());
        }
        if let Some(page) = self.plugins.page.as_ref().filter(|page| page.id == id) {
            hosts.extend(page.extra.iter().cloned());
        }
        // An account someone is adding has its host's row too.
        let of_extra: Vec<String> = hosts
            .iter()
            .map(|scope| secrets::split_scope(scope).0.to_owned())
            .collect();
        hosts.extend(of_extra);
        if let Some(default) = &forge.default_host {
            hosts.push(default.clone());
        }
        // The forge's own host first, each host followed by its other
        // accounts.
        let default = forge.default_host.as_deref();
        hosts.sort_by(|a, b| {
            let key = |scope: &str| {
                let (host, other) = secrets::split_scope(scope);
                (
                    Some(host) != default,
                    host.to_owned(),
                    other.map(str::to_owned),
                )
            };
            key(a).cmp(&key(b))
        });
        hosts.dedup();
        hosts
    }

    pub(super) fn update_plugin_page(&mut self, action: PageAction, cx: &mut Cx) {
        match action {
            PageAction::Open { id, host } => {
                if self.mode(ModeKind::Settings).is_none() {
                    self.open_settings();
                }
                self.close_host_page();
                // Opened for a host, it is about signing in there.
                if host.is_some() {
                    self.settings_section = crate::settings::Section::Accounts;
                }
                let stored = self.plugins.secrets.accounts().unwrap_or_default();
                self.plugins.page = Some(PluginPage {
                    id: id.clone(),
                    extra: host.into_iter().collect(),
                    drafts: HashMap::new(),
                    accounts: HashMap::new(),
                    stored: stored.into_iter().collect(),
                    validating: HashSet::new(),
                    rejected: HashMap::new(),
                    removing: None,
                    ai_account: None,
                    models: None,
                    picking: false,
                });
                self.check_page_accounts(&id);
            }
            PageAction::Back => self.plugins.page = None,
            PageAction::Draft { field, text } => {
                if let Some(page) = self.plugins.page.as_mut() {
                    page.drafts.insert(field, text);
                }
            }
            PageAction::Set { key, host, value } => self.set_plugin_setting(&key, host, value),
            PageAction::Commit { key, host } => {
                let Some((page, manifest)) = self.page_manifest() else {
                    return;
                };
                let Some(field) = manifest.config.iter().find(|field| field.key == key) else {
                    return;
                };
                let Some(text) = page.drafts.get(draft_id(&key, host.as_deref())).cloned() else {
                    return;
                };
                let text = text.trim();
                let value = match field.kind {
                    _ if text.is_empty() => None,
                    ConfigKind::Number => match text.parse::<i64>() {
                        Ok(whole) => Some(toml::Value::Integer(whole)),
                        Err(_) => match text.parse::<f64>() {
                            Ok(number) => Some(toml::Value::Float(number)),
                            Err(_) => return,
                        },
                    },
                    _ => Some(toml::Value::String(text.to_owned())),
                };
                if let Some(page) = self.plugins.page.as_mut() {
                    page.drafts.remove(draft_id(&key, host.as_deref()));
                }
                self.set_plugin_setting(&key, host, value);
            }
            PageAction::Browse { key } => {
                let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("Choose".into()),
                });
                cx.spawn(async move |this, cx| {
                    let Ok(Ok(Some(paths))) = picked.await else {
                        return;
                    };
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.act(cx, |this, cx| {
                            this.perform(
                                Action::Plugin(PluginAction::Page(PageAction::Set {
                                    key,
                                    host: None,
                                    value: Some(toml::Value::String(path.display().to_string())),
                                })),
                                cx,
                            );
                        });
                    });
                })
                .detach();
            }
            PageAction::SaveSecret { key, host } => self.save_page_secret(&key, host),
            PageAction::ClearSecret { key, host } => {
                let Some(page) = self.plugins.page.as_ref() else {
                    return;
                };
                let id = page.id.clone();
                let account = secrets::account(&id, &key, host.as_deref());
                if let Err(error) = self.plugins.secrets.delete(&account) {
                    self.push_error_toast("Couldn't remove it", &error);
                }
                self.secrets_changed(&id);
            }
            PageAction::TokenPage { host } => {
                let Some((_, manifest)) = self.page_manifest() else {
                    return;
                };
                if let Some(url) = token_field(&manifest)
                    .and_then(|field| field.secret.as_ref())
                    .and_then(|secret| secret.page_for(secrets::split_scope(&host).0))
                {
                    crate::open_url(&url);
                }
            }
            PageAction::AddHost => {
                let Some(page) = self.plugins.page.as_mut() else {
                    return;
                };
                let typed = page.drafts.remove(NEW_HOST).unwrap_or_default();
                let host = typed.trim().trim_end_matches('/').to_ascii_lowercase();
                let host = host
                    .strip_prefix("https://")
                    .or_else(|| host.strip_prefix("http://"))
                    .unwrap_or(&host)
                    .to_owned();
                if diffui_plugins::HostPattern::parse(&host).is_err() || host.contains('*') {
                    return;
                }
                let id = page.id.clone();
                let mut hosts: Vec<toml::Value> = self
                    .config_files
                    .plugins
                    .get(&id)
                    .and_then(|record| record.config.get(diffui_plugins::HOSTS))
                    .and_then(toml::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !hosts.iter().any(|known| known.as_str() == Some(&host)) {
                    hosts.push(toml::Value::String(host));
                }
                self.set_plugin_setting(diffui_plugins::HOSTS, None, Some(hosts.into()));
            }
            PageAction::AddAccount => {
                let Some((page, manifest)) = self.page_manifest() else {
                    return;
                };
                let typed = page.drafts.get(NEW_ACCOUNT).cloned().unwrap_or_default();
                let typed = typed.trim().trim_matches('/');
                // A name alone is an account on the forge's own host.
                let (host, name) = match typed.split_once('/') {
                    Some((host, name)) => (Some(host.to_ascii_lowercase()), name),
                    None => (
                        manifest
                            .forge
                            .as_ref()
                            .and_then(|forge| forge.default_host.clone()),
                        typed,
                    ),
                };
                let (Some(host), false) = (host, name.trim().is_empty()) else {
                    return self.push_error_toast(
                        Prose::from("Not added"),
                        "Say which host too, as host/name: git.acme.com/work",
                    );
                };
                let scope = secrets::scope(&host, Some(name));
                if let Some(page) = self.plugins.page.as_mut() {
                    page.drafts.remove(NEW_ACCOUNT);
                    if !page.extra.contains(&scope) {
                        page.extra.push(scope);
                    }
                }
            }
            PageAction::RemoveHost { host, confirmed } => {
                let Some((page, manifest)) = self.page_manifest() else {
                    return;
                };
                let id = page.id.clone();
                let stored = host_secrets(&manifest, &id, &host)
                    .into_iter()
                    .any(|account| page.stored.contains(&account));
                if stored && !confirmed {
                    if let Some(page) = self.plugins.page.as_mut() {
                        page.removing = Some(host);
                    }
                    return;
                }
                self.remove_page_host(&id, &manifest, &host);
            }
            PageAction::KeepHost => {
                if let Some(page) = self.plugins.page.as_mut() {
                    page.removing = None;
                }
            }
            PageAction::PickModels => {
                let Some(page) = self.plugins.page.as_mut() else {
                    return;
                };
                page.picking = !page.picking;
                let id = page.id.clone();
                if page.picking && !matches!(page.models, Some(Some(Ok(_)))) {
                    page.models = Some(None);
                    self.ask_ai_models(&id);
                }
            }
            PageAction::UseModel(model) => {
                let Some((_, manifest)) = self.page_manifest() else {
                    return;
                };
                let Some(key) = manifest.ai.as_ref().and_then(|ai| ai.model.clone()) else {
                    return;
                };
                if let Some(page) = self.plugins.page.as_mut() {
                    page.picking = false;
                    page.drafts.remove(draft_id(&key, None));
                }
                self.set_plugin_setting(&key, None, Some(toml::Value::String(model)));
            }
        }
    }

    pub(super) fn page_models_answered(&mut self, id: &str, result: Result<Vec<AiModel>, String>) {
        if let Some(page) = self.plugins.page.as_mut().filter(|page| page.id == id) {
            page.models = Some(Some(result));
        }
    }

    pub(super) fn page_ai_account_answered(&mut self, id: &str, result: Result<String, String>) {
        if let Some(page) = self.plugins.page.as_mut().filter(|page| page.id == id) {
            page.ai_account = Some(result);
        }
    }

    /// Take `host` off `id`'s page: out of settings.toml's `hosts` and
    /// `per_host`, its secrets out of the store. What config.toml or the
    /// forge's own CLI lists stays, and a toast says why.
    fn remove_page_host(&mut self, id: &str, manifest: &Manifest, host: &str) {
        let lower = host.to_ascii_lowercase();
        let mut record = self
            .config_files
            .plugins
            .get(id)
            .cloned()
            .unwrap_or_default();
        let mut changed = false;
        if let Some(hosts) = record
            .config
            .get_mut(diffui_plugins::HOSTS)
            .and_then(toml::Value::as_array_mut)
        {
            let before = hosts.len();
            hosts.retain(|known| {
                !known
                    .as_str()
                    .is_some_and(|known| known.eq_ignore_ascii_case(host))
            });
            changed |= hosts.len() != before;
            if hosts.is_empty() {
                record.config.remove(diffui_plugins::HOSTS);
            }
        }
        if let Some(per_host) = record
            .config
            .get_mut(PER_HOST)
            .and_then(toml::Value::as_table_mut)
        {
            changed |= per_host.remove(&lower).is_some();
            if per_host.is_empty() {
                record.config.remove(PER_HOST);
            }
        }
        if changed && let Err(error) = self.config_files.write_plugin(id, record) {
            self.push_error_toast("Couldn't save settings", &error);
            return;
        }
        for account in host_secrets(manifest, id, host) {
            if let Err(error) = self.plugins.secrets.delete(&account) {
                self.push_error_toast("Couldn't remove the token", &error);
            }
        }
        if let Some(page) = self.plugins.page.as_mut() {
            page.extra.retain(|known| !known.eq_ignore_ascii_case(host));
            page.accounts.remove(host);
            page.rejected.remove(host);
            page.removing = None;
        }
        self.secrets_changed(id);
        if let Some(why) = self.host_kept_by(id, manifest, host) {
            self.push_info_toast(Prose::new().path(host).text(" stays"), &why);
        }
    }

    /// Why `host` is on `id`'s page beyond anything Settings can remove:
    /// config.toml lists it, it is the forge's own, or its CLI signs in there.
    pub(super) fn host_kept_by(&self, id: &str, manifest: &Manifest, host: &str) -> Option<String> {
        let forge = manifest.forge.as_ref()?;
        if forge
            .default_host
            .as_deref()
            .is_some_and(|default| default.eq_ignore_ascii_case(host))
        {
            return Some("The plugin's own host".to_owned());
        }
        if let Some(user) = self.user_config.plugins.get(id) {
            let listed = super::forge::user_hosts(Some(user))
                .iter()
                .any(|known| known.to_string().eq_ignore_ascii_case(host));
            let per_host = lookup_host(user, host).is_some();
            if listed || per_host {
                return Some("Set in config.toml".to_owned());
            }
        }
        let stored = secrets::token_hosts(&*self.plugins.secrets, id)
            .iter()
            .any(|known| known.eq_ignore_ascii_case(host));
        let answered = self.plugins.get(id).is_some_and(|found| {
            found
                .extra_hosts
                .iter()
                .any(|known| known.to_string().eq_ignore_ascii_case(host))
        });
        (answered && !stored).then(|| match &forge.cli {
            Some(cli) => format!("{cli} signs in to it"),
            None => "The plugin lists it".to_owned(),
        })
    }

    /// Whether removing `host` would take anything away: a `hosts` entry or
    /// per-host settings in settings.toml, a stored secret, or the page
    /// having been opened for it.
    pub(super) fn host_removable(
        &self,
        page: &PluginPage,
        manifest: &Manifest,
        host: &str,
    ) -> bool {
        let saved = self
            .config_files
            .plugins
            .get(&page.id)
            .is_some_and(|record| {
                super::forge::user_hosts(Some(&record.config))
                    .iter()
                    .any(|known| known.to_string().eq_ignore_ascii_case(host))
                    || lookup_host(&record.config, host).is_some()
            });
        saved
            || page
                .extra
                .iter()
                .any(|known| known.eq_ignore_ascii_case(host))
            || host_secrets(manifest, &page.id, host)
                .iter()
                .any(|account| page.stored.contains(account))
    }

    fn page_manifest(&self) -> Option<(&PluginPage, std::sync::Arc<Manifest>)> {
        let page = self.plugins.page.as_ref()?;
        let manifest = self.plugins.get(&page.id)?.manifest.clone()?;
        Some((page, manifest))
    }

    /// Save one setting of the open page's plugin to `settings.toml` and
    /// restart the plugin with it.
    fn set_plugin_setting(&mut self, key: &str, host: Option<String>, value: Option<toml::Value>) {
        let Some(id) = self.plugins.page.as_ref().map(|page| page.id.clone()) else {
            return;
        };
        let mut record = self
            .config_files
            .plugins
            .get(&id)
            .cloned()
            .unwrap_or_default();
        match &host {
            None => match value {
                Some(value) => {
                    record.config.insert(key.to_owned(), value);
                }
                None => {
                    record.config.remove(key);
                }
            },
            Some(host) => {
                let host = host.to_ascii_lowercase();
                let hosts = record
                    .config
                    .entry(PER_HOST)
                    .or_insert_with(|| toml::Table::new().into());
                if let Some(hosts) = hosts.as_table_mut() {
                    let values = hosts
                        .entry(host.clone())
                        .or_insert_with(|| toml::Table::new().into());
                    if let Some(values) = values.as_table_mut() {
                        match value {
                            Some(value) => {
                                values.insert(key.to_owned(), value);
                            }
                            None => {
                                values.remove(key);
                            }
                        }
                        if values.is_empty() {
                            hosts.remove(&host);
                        }
                    }
                    if hosts.is_empty() {
                        record.config.remove(PER_HOST);
                    }
                }
            }
        }
        if let Err(error) = self.config_files.write_plugin(&id, record) {
            self.push_error_toast("Couldn't save settings", &error);
        }
        self.restart_plugin(&id);
    }

    /// Start `id` again with its settings as they are now, if it runs.
    fn restart_plugin(&mut self, id: &str) {
        let running = self
            .plugins
            .get(id)
            .is_some_and(|found| matches!(found.state, State::Running(_) | State::Loading));
        if running {
            self.spawn_plugin(id);
            self.refresh_script_forges();
        }
        if let Some(page) = self.plugins.page.as_mut().filter(|page| page.id == id) {
            page.accounts.clear();
            page.ai_account = None;
        }
    }

    fn secrets_changed(&mut self, id: &str) {
        self.refresh_ai_keys();
        let stored = self.plugins.secrets.accounts().unwrap_or_default();
        if let Some(page) = self.plugins.page.as_mut() {
            page.stored = stored.into_iter().collect();
        }
        self.restart_plugin(id);
    }

    /// Keep what is typed in the secret `key`'s field: a forge's token after
    /// the forge says whose it is, any other at once.
    fn save_page_secret(&mut self, key: &str, host: Option<String>) {
        let Some((page, manifest)) = self.page_manifest() else {
            return;
        };
        let id = page.id.clone();
        let Some(typed) = page
            .drafts
            .get(draft_id(key, host.as_deref()))
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let checked_by_forge = key == "token" && token_field(&manifest).is_some();
        match (host, checked_by_forge) {
            (Some(host), true) => {
                let Some(plugin) = self.plugins.plugin(&id) else {
                    if let Some(page) = self.plugins.page.as_mut() {
                        page.rejected
                            .insert(host, "Turn the plugin on to check the token.".into());
                    }
                    return;
                };
                if let Some(page) = self.plugins.page.as_mut() {
                    page.validating.insert(host.clone());
                    page.rejected.remove(&host);
                }
                self.ask_account(&id, &plugin, host, Some(typed));
            }
            (host, _) => {
                let account = secrets::account(&id, key, host.as_deref());
                if let Err(error) = self.plugins.secrets.set(&account, &typed) {
                    self.push_error_toast("Couldn't keep it", &error);
                    return;
                }
                if let Some(page) = self.plugins.page.as_mut() {
                    page.drafts.remove(draft_id(key, host.as_deref()));
                }
                self.secrets_changed(&id);
            }
        }
    }

    /// Ask `id`'s forge about `host`'s account, or about `token`.
    fn ask_account(
        &mut self,
        id: &str,
        plugin: &std::sync::Arc<diffui_plugins::Plugin>,
        host: String,
        token: Option<String>,
    ) {
        let to = self.plugins.to_app.clone();
        let checked = token.is_some();
        let (plugin_id, answered) = (id.to_owned(), host.clone());
        // `host` is what the row is for: a host, or another account on it.
        let (on, other) = secrets::split_scope(&host);
        plugin.call::<Account>(
            PluginCall::Forge {
                repo: super::forge::no_repository(None),
                call: ForgeCall::Account {
                    host: on.to_owned(),
                    account: other.map(str::to_owned),
                    token: token.map(Secret),
                },
            },
            move |result| {
                let result = result.map_err(|error| match error {
                    diffui_plugins::PluginError::Threw(thrown) => thrown.message,
                    other => other.to_string(),
                });
                let _ = to.unbounded_send(FromPlugin::Account {
                    plugin: plugin_id,
                    host: answered,
                    checked,
                    result,
                });
            },
        );
    }

    /// Ask the forge of the open page about each of its hosts, when the
    /// page is `id`'s and the plugin is up.
    pub(super) fn check_page_accounts(&mut self, id: &str) {
        if self.plugins.page.as_ref().is_none_or(|page| page.id != id) {
            return;
        }
        if self.ai_answers(id, "account") {
            self.ask_ai_account(id);
        }
        let Some(found) = self.plugins.get(id) else {
            return;
        };
        let answers = matches!(&found.state, State::Running(registration)
            if registration.forge.as_ref().is_some_and(|methods| methods.iter().any(|m| m == "account")));
        let Some(plugin) = self.plugins.plugin(id).filter(|_| answers) else {
            return;
        };
        for host in self.page_hosts(id) {
            if let Some(page) = self.plugins.page.as_mut() {
                page.accounts.insert(host.clone(), Check::Asking);
            }
            self.ask_account(id, &plugin, host, None);
        }
    }

    pub(super) fn plugin_account_answered(
        &mut self,
        id: &str,
        host: String,
        checked: bool,
        result: Result<Account, String>,
    ) {
        let Some(page) = self.plugins.page.as_mut().filter(|page| page.id == id) else {
            return;
        };
        if !checked {
            let check = match result {
                Ok(account) => Check::Known(account),
                Err(why) => Check::Failed(why),
            };
            page.accounts.insert(host, check);
            return;
        }
        if !page.validating.remove(&host) {
            return;
        }
        let field = draft_id("token", Some(&host));
        match result {
            Ok(_) => {
                let Some(token) = page.drafts.remove(field) else {
                    return;
                };
                let account = secrets::account(id, "token", Some(&host));
                if let Err(error) = self.plugins.secrets.set(&account, token.trim()) {
                    self.push_error_toast("Couldn't keep the token", &error);
                    return;
                }
                self.secrets_changed(id);
            }
            Err(why) => {
                page.rejected.insert(host, why);
            }
        }
    }
}

pub(crate) const FIELD_WIDTH: f32 = 240.0;

/// The settings dialog's Accounts section: a row per forge that signs in,
/// naming the hosts and accounts it signs in to, with the pencil that
/// opens them.
pub(crate) fn accounts_group(ui: &Diffui, look: Look, cx: &mut Cx) -> Vec<AnyElement> {
    let Look { theme, .. } = look;
    ui.plugins
        .found
        .iter()
        .enumerate()
        .filter_map(|(index, found)| {
            token_field(found.manifest.as_ref()?)?;
            let scopes: Vec<String> = ui
                .page_hosts(&found.id)
                .iter()
                .map(|scope| match secrets::split_scope(scope) {
                    (host, Some(name)) => format!("{name} on {host}"),
                    (host, None) => host.to_owned(),
                })
                .collect();
            let said = if scopes.is_empty() {
                "Turn the plugin on to sign in".to_owned()
            } else {
                scopes.join(" \u{b7} ")
            };
            let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
            let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
            let open = look
                .icon_button(
                    ("settings-accounts-open", index),
                    icons::PENCIL,
                    theme.muted_text,
                )
                .size(px(side))
                .corner_radius(face)
                .tip("Accounts")
                .on_click(send(
                    PageAction::Open {
                        id: found.id.clone(),
                        host: None,
                    },
                    cx,
                ));
            Some(row_shell(
                HINT_ROW_HEIGHT,
                label(
                    look,
                    &found.name,
                    false,
                    Some(caption(look, said, theme.muted_text)),
                ),
                ui::segmented(theme).child(open),
            ))
        })
        .collect()
}

/// The open page, for the settings dialog's pane.
pub(crate) fn page_pane(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<crate::settings::Pane> {
    let page = ui.plugins.page.as_ref()?;
    let found = ui.plugins.get(&page.id)?;
    let manifest = found.manifest.clone()?;
    let config = ui.plugin_config(&page.id);
    let user = ui
        .user_config
        .plugins
        .get(&page.id)
        .cloned()
        .unwrap_or_default();
    let resolved = manifest.resolve_config(&config).0;

    let mut body = crate::settings::pane_body();
    let mut first = true;

    if let Some(token) = token_field(&manifest) {
        let mut rows = Vec::new();
        for host in ui.page_hosts(&page.id) {
            let row = account_row(ui, look, page, &manifest, token, &host, &user, window, cx);
            rows.push(row);
        }
        rows.push(add_host_row(look, page, window, cx));
        rows.push(add_account_row(look, page, window, cx));
        body = body.child(group(look, "Accounts", !first, rows));
        first = false;
    }

    if manifest.ai.is_some() {
        body = body.child(group(
            look,
            "Provider",
            !first,
            vec![provider_row(ui, look, page, cx)],
        ));
        first = false;
    }

    let fields: Vec<&ConfigField> = manifest
        .config
        .iter()
        .filter(|field| field.kind != ConfigKind::Secret || !field.per_host)
        .filter(|field| !field.per_host || field.default.is_some())
        .collect();
    if !fields.is_empty() {
        let model_key = manifest.ai.as_ref().and_then(|ai| ai.model.as_deref());
        let mut rows: Vec<AnyElement> = Vec::new();
        for field in &fields {
            rows.push(setting_row(
                ui, look, page, &manifest, field, &resolved, &user, window, cx,
            ));
            if page.picking && model_key == Some(field.key.as_str()) {
                let list = model_list(look, page, &field.key, cx);
                rows.push(list);
            }
        }
        body = body.child(group(look, "Settings", !first, rows));
    }

    Some(crate::settings::Pane {
        page: Some((
            Prose::from(found.name.clone()),
            Action::Plugin(PluginAction::Page(PageAction::Back)),
        )),
        actions: Vec::new(),
        scroll: "plugin-page",
        body,
    })
}

pub(crate) const ROW_HEIGHT: f32 = 44.0;
const HINT_ROW_HEIGHT: f32 = crate::settings::HINT_ROW_HEIGHT;
const ROW_INSET: f32 = crate::settings::ROW_INSET;

fn send(
    action: PageAction,
    cx: &mut Cx,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(Action::Plugin(PluginAction::Page(action)), cx)
}

/// A titled card of rows split by inset hairlines, as Settings' groups.
pub(crate) fn group(
    look: Look,
    title: &'static str,
    after: bool,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let Look { theme, .. } = look;
    let count = rows.len();
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .border_1()
        .border_color(theme::TRANSPARENT);
    for (index, row) in rows.into_iter().enumerate() {
        card = card.child(row);
        if index + 1 < count {
            card = card.child(
                div()
                    .mx(px(ROW_INSET))
                    .h(px(theme::HAIRLINE))
                    .bg(theme.border),
            );
        }
    }
    let card = theme::squircle(card, theme::radius::CARD, None, Some(theme.border));
    div()
        .flex()
        .flex_col()
        .w_full()
        .when(after, |group| group.pt(px(theme::space::XXL)))
        .child(
            div()
                .pb(px(crate::settings::HEADING_GAP))
                .child(overlay_kit::group_heading(title, look)),
        )
        .child(card)
        .into_any_element()
}

/// A setting's name, a lock when config.toml sets it, and a line under it.
pub(crate) fn label(
    look: Look,
    name: &str,
    locked: bool,
    under: Option<AnyElement>,
) -> gpui_kit::Div {
    let Look { theme, font, .. } = look;
    let mut title = div()
        .flex()
        .flex_row()
        .items_center()
        .min_w_0()
        .gap(px(theme::space::SM))
        .child(
            typography::line(
                name.to_owned(),
                theme::emphasis_font(font, gpui_kit::FontWeight::MEDIUM),
                text_size::BODY,
                theme.text,
            )
            .text_ellipsis(),
        );
    if locked {
        title = title.child(
            div()
                .id(gpui_kit::ElementId::Name(
                    format!("plugin-page-locked-{name}").into(),
                ))
                .flex_none()
                .tip("Set in config.toml", look)
                .child(icons::icon(
                    overlay_kit::glyph::LOCK,
                    theme::icon::SM,
                    theme.subtle_text,
                )),
        );
    }
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .gap(px(theme::space::XXS))
        .child(title)
        .children(under)
}

pub(crate) fn caption(look: Look, text: impl Into<String>, ink: theme::Color) -> AnyElement {
    let Look { font, .. } = look;
    typography::line(text.into(), font, text_size::CAPTION, ink)
        .overflow_hidden()
        .text_ellipsis()
        .into_any_element()
}

pub(crate) fn row_shell(
    height: f32,
    label: impl IntoElement,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(height))
        .px(px(ROW_INSET))
        .gap(px(theme::space::XL))
        .child(label)
        .child(div().flex().flex_none().items_center().child(control))
        .into_any_element()
}

/// A one-line field of the page, its text the draft when there is one.
#[allow(clippy::too_many_arguments)]
fn text_field(
    look: Look,
    id: &'static str,
    value: &str,
    placeholder: &str,
    masked: bool,
    on_submit: PageAction,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let on_input = Diffui::listen(cx, move |this, text: &String, _, cx| {
        this.perform(
            Action::Plugin(PluginAction::Page(PageAction::Draft {
                field: id,
                text: text.clone(),
            })),
            cx,
        );
    });
    let on_submit = Diffui::on_action::<()>(Action::Plugin(PluginAction::Page(on_submit)), cx);
    let spec = input::FieldSpec::new(id, value)
        .placeholder(placeholder)
        .masked(masked)
        .on_input(move |text, window, cx| on_input(&text, window, cx))
        .on_submit(move |window, cx| on_submit(&(), window, cx));
    div()
        .w(px(FIELD_WIDTH))
        .child(crate::field::dialog_input(spec, look, window, cx))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn segmented_enum(
    look: Look,
    manifest: &Manifest,
    field: &ConfigField,
    host: Option<&str>,
    current: Option<&str>,
    fallback: Option<&str>,
    enabled: bool,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let id_base = match host {
        Some(host) => format!("plugin-page-{}@{host}", field.key),
        None => format!("plugin-page-{}", field.key),
    };
    let shown = current.or(fallback);
    ui::segmented(theme)
        .children(field.values.iter().enumerate().map(|(index, value)| {
            // A host's choice that is the plugin's own is no choice of its own.
            let stored = match (host, fallback) {
                (Some(_), Some(fallback)) if fallback == value => None,
                _ => Some(toml::Value::String(value.clone())),
            };
            ui::segment(
                (gpui_kit::SharedString::from(id_base.clone()), index),
                Segment {
                    icon: None,
                    label: value_label(manifest, value).into(),
                    count: None,
                    active: shown == Some(value.as_str()),
                    enabled,
                },
                Segmented::Compact,
                look,
            )
            .when(enabled, |segment| {
                segment.on_click(send(
                    PageAction::Set {
                        key: field.key.clone(),
                        host: host.map(str::to_owned),
                        value: stored,
                    },
                    cx,
                ))
            })
        }))
        .into_any_element()
}

/// One host of a forge: who the user is there and through what, how it
/// signs in, and its token: a paste field, or Sign out.
#[allow(clippy::too_many_arguments)]
fn account_row(
    ui: &Diffui,
    look: Look,
    page: &PluginPage,
    manifest: &Manifest,
    token: &ConfigField,
    host: &str,
    user: &toml::Table,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, mono, .. } = look;
    let config = ui.plugin_config(&page.id);
    let cli = manifest
        .forge
        .as_ref()
        .and_then(|forge| forge.cli.clone())
        .unwrap_or_else(|| "its CLI".to_owned());
    let (status, ink) = if page.validating.contains(host) {
        ("Checking the token…".to_owned(), theme.muted_text)
    } else if let Some(why) = page.rejected.get(host) {
        (
            why.lines().next().unwrap_or_default().to_owned(),
            theme.removed_text,
        )
    } else {
        match page.accounts.get(host) {
            None if ui.plugins.plugin(&page.id).is_none() => {
                ("Turn the plugin on to sign in".to_owned(), theme.muted_text)
            }
            // An account just added: nothing to check until it has a token.
            None if secrets::split_scope(host).1.is_some()
                && !page
                    .stored
                    .contains(&secrets::account(&page.id, &token.key, Some(host))) =>
            {
                (
                    "Tried for repositories the host's sign-in can't see".to_owned(),
                    theme.muted_text,
                )
            }
            None | Some(Check::Asking) => ("Checking…".to_owned(), theme.muted_text),
            Some(Check::Known(Account {
                method: Some(method),
                login,
            })) => {
                let through = match method {
                    AccountMethod::Cli => cli.clone(),
                    AccountMethod::Token => "a token".to_owned(),
                };
                match login {
                    Some(login) => (
                        format!("Signed in as {login} with {through}"),
                        theme.added_text,
                    ),
                    None => (format!("Signed in with {through}"), theme.added_text),
                }
            }
            Some(Check::Known(_)) => ("Not signed in".to_owned(), theme.muted_text),
            Some(Check::Failed(why)) => (
                format!("Couldn't check: {}", why.lines().next().unwrap_or_default()),
                theme.removed_text,
            ),
        }
    };
    // A row for another account on a host: its token is all there is to
    // it.
    let (on, other) = secrets::split_scope(host);
    let shown = match other {
        Some(name) => format!("{name} on {on}"),
        None => host.to_owned(),
    };
    let name = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .gap(px(theme::space::XXS))
        .child(
            typography::line(shown, mono, text_size::BODY, theme.text)
                .overflow_hidden()
                .text_ellipsis(),
        )
        .child(caption(look, status, ink));
    let methods = manifest
        .config
        .iter()
        .filter(|_| other.is_none())
        .find(|field| field.per_host && field.kind == ConfigKind::Enum)
        .map(|field| {
            let locked = lookup(user, &field.key, Some(host)).is_some();
            let current = lookup(&config, &field.key, Some(host)).and_then(toml::Value::as_str);
            let fallback = config
                .get(&field.key)
                .and_then(toml::Value::as_str)
                .or_else(|| field.default.as_ref().and_then(|value| value.as_str()));
            segmented_enum(
                look,
                manifest,
                field,
                Some(host),
                current,
                fallback,
                !locked,
                cx,
            )
        });
    let own = manifest
        .forge
        .as_ref()
        .and_then(|forge| forge.default_host.as_deref())
        .is_some_and(|default| default.eq_ignore_ascii_case(host));
    let remove = (!own).then(|| {
        let removable = ui.host_removable(page, manifest, host);
        let button = look
            .close_button(
                gpui_kit::SharedString::from(format!("plugin-page-remove-{host}")),
                theme.muted_text,
            )
            .disabled(!removable);
        match ui.host_kept_by(&page.id, manifest, host) {
            Some(why) if !removable => button.tip(why),
            _ => button.tip("Remove").on_click(send(
                PageAction::RemoveHost {
                    host: host.to_owned(),
                    confirmed: false,
                },
                cx,
            )),
        }
    });
    let top = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(theme::space::XL))
        .child(name)
        .children(methods.map(|methods| div().flex_none().child(methods)))
        .children(remove);

    let account = secrets::account(&page.id, &token.key, Some(host));
    let secret = token.secret.as_ref();
    let bottom = if page.removing.as_deref() == Some(host) {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(theme::space::MD))
            .child(div().flex_1().min_w_0().child(caption(
                look,
                if other.is_some() {
                    "Remove this account and its saved token?"
                } else {
                    "Remove this host and its saved token?"
                },
                theme.text,
            )))
            .child(
                look.cancel_button(gpui_kit::SharedString::from(format!(
                    "plugin-page-keep-{host}"
                )))
                .compact()
                .on_click(send(PageAction::KeepHost, cx)),
            )
            .child(
                look.button(
                    gpui_kit::SharedString::from(format!("plugin-page-remove-yes-{host}")),
                    "Remove",
                )
                .destructive()
                .compact()
                .on_click(send(
                    PageAction::RemoveHost {
                        host: host.to_owned(),
                        confirmed: true,
                    },
                    cx,
                )),
            )
    } else if page.stored.contains(&account) {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(theme::space::MD))
            .child(div().flex_1().min_w_0().child(caption(
                look,
                if other.is_some() {
                    "A token is saved for this account"
                } else {
                    "A token is saved for this host"
                },
                theme.muted_text,
            )))
            .child(
                look.button(
                    gpui_kit::SharedString::from(format!("plugin-page-sign-out-{host}")),
                    "Sign out",
                )
                .compact()
                .on_click(send(
                    PageAction::ClearSecret {
                        key: token.key.clone(),
                        host: Some(host.to_owned()),
                    },
                    cx,
                )),
            )
    } else {
        let id = draft_id(&token.key, Some(host));
        let typed = page.drafts.get(id).cloned().unwrap_or_default();
        let save = PageAction::SaveSecret {
            key: token.key.clone(),
            host: Some(host.to_owned()),
        };
        let scopes = secret
            .filter(|secret| !secret.scopes.is_empty())
            .map(|secret| format!("Needs {}", secret.scopes.join(", ")));
        let get = secret.and_then(|secret| secret.page.as_ref()).map(|_| {
            look.button(
                gpui_kit::SharedString::from(format!("plugin-page-token-page-{host}")),
                "Get a token",
            )
            .compact()
            .tip(
                scopes
                    .clone()
                    .unwrap_or_else(|| "Opens the forge's token page".to_owned()),
            )
            .on_click(send(
                PageAction::TokenPage {
                    host: host.to_owned(),
                },
                cx,
            ))
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(theme::space::MD))
            .child(text_field(
                look,
                id,
                &typed,
                "Paste a token",
                true,
                save.clone(),
                window,
                cx,
            ))
            .child(
                look.button(
                    gpui_kit::SharedString::from(format!("plugin-page-token-save-{host}")),
                    "Add",
                )
                .compact()
                .disabled(typed.trim().is_empty() || page.validating.contains(host))
                .on_click(send(save, cx)),
            )
            .children(get)
    };
    div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(ROW_INSET))
        .py(px(theme::space::MD))
        .gap(px(theme::space::SM))
        .child(top)
        .child(bottom)
        .into_any_element()
}

/// A field to name another account on a host.
fn add_account_row(look: Look, page: &PluginPage, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let typed = page.drafts.get(NEW_ACCOUNT).cloned().unwrap_or_default();
    let label = label(
        look,
        "Another account",
        false,
        Some(caption(
            look,
            "Tried for repositories the host's sign-in can't see",
            look.theme.subtle_text,
        )),
    );
    let control = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .child(text_field(
            look,
            NEW_ACCOUNT,
            &typed,
            "work",
            false,
            PageAction::AddAccount,
            window,
            cx,
        ))
        .child(
            look.button("plugin-page-add-account", "Add")
                .compact()
                .disabled(typed.trim().is_empty())
                .on_click(send(PageAction::AddAccount, cx)),
        );
    row_shell(ROW_HEIGHT + theme::space::MD, label, control)
}

/// A field to add a host the forge doesn't know yet.
fn add_host_row(look: Look, page: &PluginPage, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let typed = page.drafts.get(NEW_HOST).cloned().unwrap_or_default();
    let label = label(
        look,
        "Another host",
        false,
        Some(caption(
            look,
            "A company server this plugin should answer for",
            look.theme.subtle_text,
        )),
    );
    let control = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .child(text_field(
            look,
            NEW_HOST,
            &typed,
            "git.example.com",
            false,
            PageAction::AddHost,
            window,
            cx,
        ))
        .child(
            look.button("plugin-page-add-host", "Add")
                .compact()
                .disabled(typed.trim().is_empty())
                .on_click(send(PageAction::AddHost, cx)),
        );
    row_shell(ROW_HEIGHT + theme::space::MD, label, control)
}

/// One setting of the schema, with the control its type gets.
#[allow(clippy::too_many_arguments)]
fn setting_row(
    ui: &Diffui,
    look: Look,
    page: &PluginPage,
    manifest: &Manifest,
    field: &ConfigField,
    resolved: &serde_json::Map<String, serde_json::Value>,
    user: &toml::Table,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let locked = user.contains_key(&field.key);
    let hint = |text: Option<String>, ink| text.map(|text| caption(look, text, ink));
    let plain_hint = hint(field.hint.clone(), theme.subtle_text);
    let (under, control): (Option<AnyElement>, AnyElement) = match field.kind {
        ConfigKind::Enum => {
            let current = resolved.get(&field.key).and_then(|value| value.as_str());
            (
                plain_hint,
                segmented_enum(look, manifest, field, None, current, None, !locked, cx),
            )
        }
        ConfigKind::Boolean => {
            let on = resolved
                .get(&field.key)
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let control = ui::segmented(theme)
                .children([(true, "On"), (false, "Off")].map(|(value, label)| {
                    ui::segment(
                        (
                            gpui_kit::SharedString::from(format!("plugin-page-{}", field.key)),
                            usize::from(value),
                        ),
                        Segment {
                            icon: None,
                            label: label.into(),
                            count: None,
                            active: on == value,
                            enabled: !locked,
                        },
                        Segmented::Compact,
                        look,
                    )
                    .on_click(send(
                        PageAction::Set {
                            key: field.key.clone(),
                            host: None,
                            value: Some(toml::Value::Boolean(value)),
                        },
                        cx,
                    ))
                }))
                .into_any_element();
            (plain_hint, control)
        }
        ConfigKind::Secret => {
            let account = secrets::account(&page.id, &field.key, None);
            if page.stored.contains(&account) {
                let control = look
                    .button(
                        gpui_kit::SharedString::from(format!("plugin-page-clear-{}", field.key)),
                        "Remove",
                    )
                    .compact()
                    .on_click(send(
                        PageAction::ClearSecret {
                            key: field.key.clone(),
                            host: None,
                        },
                        cx,
                    ))
                    .into_any_element();
                (hint(Some("Saved".into()), theme.muted_text), control)
            } else {
                let id = draft_id(&field.key, None);
                let typed = page.drafts.get(id).cloned().unwrap_or_default();
                let save = PageAction::SaveSecret {
                    key: field.key.clone(),
                    host: None,
                };
                (
                    plain_hint,
                    text_field(look, id, &typed, "Not set", true, save, window, cx),
                )
            }
        }
        ConfigKind::String | ConfigKind::Number | ConfigKind::Path => {
            let id = draft_id(&field.key, None);
            let current = resolved.get(&field.key).map(|value| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            });
            let shown = page
                .drafts
                .get(id)
                .cloned()
                .or_else(|| current.clone())
                .unwrap_or_default();
            let commit = PageAction::Commit {
                key: field.key.clone(),
                host: None,
            };
            let placeholder = as_text(field.default.as_ref().map(json_to_toml).as_ref());
            let field_element =
                text_field(look, id, &shown, &placeholder, false, commit, window, cx);
            if field.kind == ConfigKind::Path {
                let missing = current
                    .as_deref()
                    .is_some_and(|program| !program_found(program));
                let under = if missing {
                    hint(Some("Not found on this machine".into()), theme.removed_text)
                } else {
                    plain_hint
                };
                let browse = look
                    .button(
                        gpui_kit::SharedString::from(format!("plugin-page-browse-{}", field.key)),
                        "Browse…",
                    )
                    .compact()
                    .disabled(locked)
                    .on_click(send(
                        PageAction::Browse {
                            key: field.key.clone(),
                        },
                        cx,
                    ));
                let control = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(theme::space::MD))
                    .child(field_element)
                    .child(browse)
                    .into_any_element();
                (under, control)
            } else if manifest.ai.as_ref().and_then(|ai| ai.model.as_deref()) == Some(&field.key)
                && ui.ai_answers(&page.id, "models")
            {
                let choose = look
                    .button(
                        "plugin-page-models",
                        if page.picking {
                            "Close"
                        } else {
                            "Choose\u{2026}"
                        },
                    )
                    .compact()
                    .disabled(locked)
                    .on_click(send(PageAction::PickModels, cx));
                let control = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(theme::space::MD))
                    .child(field_element)
                    .child(choose)
                    .into_any_element();
                (plain_hint, control)
            } else {
                (plain_hint, field_element)
            }
        }
    };
    let _ = ui;
    row_shell(
        HINT_ROW_HEIGHT,
        label(look, field.title(), locked, under),
        control,
    )
}

/// An AI provider's state: what `account()` said, or a Test's result, and
/// the Test button.
fn provider_row(ui: &Diffui, look: Look, page: &PluginPage, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    let under = match (ui.ai.tests.get(&page.id), &page.ai_account) {
        (Some(test), _) => crate::ai::test_caption(look, test),
        (None, Some(Ok(label))) => caption(look, label.clone(), theme.muted_text),
        (None, Some(Err(why))) => caption(look, why.clone(), theme.removed_text),
        (None, None) if ui.ai_answers(&page.id, "account") => {
            caption(look, "Checking\u{2026}", theme.muted_text)
        }
        (None, None) => caption(look, "Test it to see that it answers", theme.subtle_text),
    };
    let testing = ui.ai.tests.get(&page.id) == Some(&crate::ai::Test::Running);
    let test = look
        .button("plugin-page-ai-test", "Test")
        .compact()
        .disabled(testing || ui.plugins.plugin(&page.id).is_none())
        .tip("Send a short prompt and show the reply")
        .on_click(Diffui::on_action(
            Action::Ai(crate::ai::AiAction::Test(page.id.clone())),
            cx,
        ));
    row_shell(
        HINT_ROW_HEIGHT,
        label(look, "Status", false, Some(under)),
        test,
    )
}

/// The most models the list shows; typing in the field narrows it.
const MODELS_SHOWN: usize = 12;
const MODEL_ROW: f32 = 28.0;

/// The provider's models under its model setting, narrowed by what is
/// typed there: each one sets it.
fn model_list(look: Look, page: &PluginPage, key: &str, cx: &mut Cx) -> AnyElement {
    let Look {
        theme, font, mono, ..
    } = look;
    let typed = page
        .drafts
        .get(draft_id(key, None))
        .map(|text| text.trim().to_lowercase())
        .unwrap_or_default();
    let line = |text: String, ink| {
        div()
            .h(px(MODEL_ROW))
            .flex()
            .items_center()
            .child(caption(look, text, ink))
            .into_any_element()
    };
    let mut lines: Vec<AnyElement> = Vec::new();
    match &page.models {
        None | Some(None) => lines.push(line(
            "Asking for the models\u{2026}".into(),
            theme.muted_text,
        )),
        Some(Some(Err(why))) => lines.push(line(why.clone(), theme.removed_text)),
        Some(Some(Ok(models))) => {
            let matching: Vec<&AiModel> = models
                .iter()
                .filter(|model| {
                    typed.is_empty()
                        || model.id.to_lowercase().contains(&typed)
                        || model
                            .name
                            .as_ref()
                            .is_some_and(|name| name.to_lowercase().contains(&typed))
                })
                .collect();
            if matching.is_empty() {
                lines.push(line("No model matches".into(), theme.muted_text));
            }
            for (index, model) in matching.iter().take(MODELS_SHOWN).enumerate() {
                let mut row = div()
                    .id(("plugin-page-model", index))
                    .h(px(MODEL_ROW))
                    .flex()
                    .flex_row()
                    .items_center()
                    .min_w_0()
                    .gap(px(theme::space::MD))
                    .px(px(theme::space::SM))
                    .rounded(px(theme::radius::CONTROL))
                    .cursor_pointer()
                    .hover(|row| row.bg(theme::hover_wash(theme)))
                    .on_click(send(PageAction::UseModel(model.id.clone()), cx))
                    .child(
                        typography::line(model.id.clone(), mono, text_size::CAPTION, theme.text)
                            .flex_none(),
                    );
                if let Some(name) = &model.name {
                    row = row.child(
                        typography::line(name.clone(), font, text_size::CAPTION, theme.muted_text)
                            .overflow_hidden()
                            .text_ellipsis(),
                    );
                }
                lines.push(row.into_any_element());
            }
            if matching.len() > MODELS_SHOWN {
                lines.push(line(
                    format!(
                        "{} more: type in the field to narrow",
                        matching.len() - MODELS_SHOWN
                    ),
                    theme.subtle_text,
                ));
            }
        }
    }
    div()
        .flex()
        .flex_col()
        .w_full()
        .py(px(theme::space::SM))
        .px(px(ROW_INSET))
        .children(lines)
        .into_any_element()
}

fn json_to_toml(value: &serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::Bool(flag) => toml::Value::Boolean(*flag),
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(toml::Value::Integer)
            .unwrap_or_else(|| toml::Value::Float(number.as_f64().unwrap_or_default())),
        serde_json::Value::String(text) => toml::Value::String(text.clone()),
        other => toml::Value::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_lie_under_config_toml_a_key_at_a_time() {
        let mut saved: toml::Table = toml::from_str(
            "auth = \"token\"\nprogram = \"/opt/gh\"\n[per_host.\"a.test\"]\nauth = \"cli\"\napi_url = \"x\"\n",
        )
        .unwrap();
        let user: toml::Table =
            toml::from_str("program = \"/usr/bin/gh\"\n[per_host.\"a.test\"]\nauth = \"token\"\n[per_host.\"b.test\"]\nauth = \"cli\"\n").unwrap();
        overlay(&mut saved, &user);
        assert_eq!(saved["auth"].as_str(), Some("token"));
        assert_eq!(saved["program"].as_str(), Some("/usr/bin/gh"));
        assert_eq!(
            lookup(&saved, "auth", Some("A.test")).and_then(|v| v.as_str()),
            Some("token")
        );
        assert_eq!(
            lookup(&saved, "api_url", Some("a.test")).and_then(|v| v.as_str()),
            Some("x")
        );
        assert_eq!(
            lookup(&saved, "auth", Some("b.test")).and_then(|v| v.as_str()),
            Some("cli")
        );
    }

    #[test]
    fn a_program_is_found_by_path_or_on_the_path() {
        assert!(program_found("/bin/sh"));
        assert!(program_found("sh"));
        assert!(!program_found("/nonexistent/diffui-test/gh"));
        assert!(!program_found("diffui-no-such-program"));
        assert!(!program_found(""));
    }
}
