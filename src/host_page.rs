//! Hosts in Settings: the group that lists them, and a host's page (the
//! pencil on its row).
//!
//! What the page changes goes to `settings.toml` as a `[hosts.<name>]` table
//! and applies to the next connection. A key config.toml's table sets is
//! shown locked. A host a transport found on its own (a WSL distribution, a
//! `Host` of `~/.ssh/config`) has no table until something about it is
//! changed.
//!
//! Text is kept as typed until Return, or until the page is left or the
//! host is connected to: a half-typed command is never the one that is run.
//!
//! The page's Connection row is the app's own hold on the host (see
//! `crate::connections`): connecting from here, like from anywhere else,
//! starts a connection that outlives the page.

use std::collections::HashMap;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use diffui_core::cli::tokenize::{shell_quote, tokenize};
use diffui_core::remote::{HostId, Transcript};
use diffui_plugins::{ConfigField, ConfigKind};

use crate::config::HostTables;
use crate::connections::HostState;
use crate::modes::ModeKind;
use crate::overlay_kit::{self, DIALOG_PAD};
use crate::plugins::page::{
    CARD_WIDTH, DIALOG_HEAD, FIELD_WIDTH, GROUP_HEAD, ROW_HEIGHT, caption, field_id, group, label,
    row_shell,
};
use crate::prose::Prose;
use crate::remote::{DEFAULT_SERVER, Transport, lock};
use crate::settings::HINT_ROW_HEIGHT;
use crate::theme::{self, text_size};
use crate::typography::{self, Aligned};
use crate::ui::{self, Look, Segment, Segmented, Tip as _};
use crate::{Action, Cx, Diffui, icons, input};

const TRANSPORT: &str = "transport";
const COMMAND: &str = "command";
const SERVER: &str = "server";
const ENV: &str = "env";

/// The field for a new host's name, in the Settings list.
pub(crate) const NEW_HOST: &str = "settings-new-host";

/// The open page.
#[derive(Debug)]
pub(crate) struct HostPage {
    pub(crate) name: String,
    /// What is typed in each field, by the table's key.
    drafts: HashMap<String, String>,
    /// "Command" is picked, whether or not one is saved yet.
    by_command: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostAction {
    /// Open `name`'s page.
    Open(String),
    /// Back to the Settings list.
    Back,
    /// The text in the field for `key` changed.
    Draft { key: String, text: String },
    /// The text in the new host's name field changed.
    NewName(String),
    /// Save the text of `key`'s field.
    Commit(String),
    /// Set `key` to `value`; `None` removes it.
    Set {
        key: String,
        value: Option<toml::Value>,
    },
    /// Reach the host over this transport, or (`None`) by a command.
    Reach(Option<String>),
    /// Make the host the name field names and open its page.
    Add,
    /// Take the host's table out of settings.toml.
    Remove,
    /// Commit what is typed and connect to the host.
    Connect,
    /// Hang up on the host, or stop reaching for it.
    Disconnect,
    /// Copy what was run to reach the host and what it wrote.
    CopyOutput,
}

/// The host tables in force: settings.toml's, with config.toml's over them a
/// key at a time.
pub(crate) fn merged(settings: &HostTables, config: &HostTables) -> HostTables {
    let mut merged = settings.clone();
    for (name, over) in config {
        let table = merged.entry(name.clone()).or_default();
        // How a host is reached is one choice: config.toml's replaces
        // Settings', and the settings that went with it.
        if over.contains_key(TRANSPORT) || over.contains_key(COMMAND) {
            table.retain(|key, _| key == SERVER || key == ENV);
        }
        table.extend(over.clone());
    }
    merged
}

/// A `server` or `command` value as its words.
fn words_of(value: Option<&toml::Value>) -> Vec<String> {
    match value {
        Some(toml::Value::String(word)) => vec![word.clone()],
        Some(toml::Value::Array(words)) => words
            .iter()
            .filter_map(|word| word.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Words as one line a field shows, quoted so [`split`] reads them back.
fn line_of(words: &[String]) -> String {
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn split(text: &str) -> Vec<String> {
    tokenize(text).into_iter().map(|word| word.value).collect()
}

/// An `env` table as `NAME=value` words.
fn env_line(value: Option<&toml::Value>) -> String {
    let Some(toml::Value::Table(env)) = value else {
        return String::new();
    };
    let words: Vec<String> = env
        .iter()
        .filter_map(|(name, value)| Some(format!("{name}={}", value.as_str()?)))
        .collect();
    line_of(&words)
}

fn parse_env(text: &str) -> Result<toml::Table, String> {
    let mut env = toml::Table::new();
    for word in split(text) {
        match word.split_once('=') {
            Some((name, value)) if !name.is_empty() => {
                env.insert(name.to_owned(), toml::Value::String(value.to_owned()));
            }
            _ => return Err(format!("{word} isn't NAME=value")),
        }
    }
    Ok(env)
}

/// Why `name` can't name a host, if it can't: `host:/path` has to read as one.
fn name_problem(name: &str) -> Option<&'static str> {
    let fits = name.chars().count() >= 2
        && !name.contains(['/', '\\', ':'])
        && !name.contains(char::is_whitespace);
    (!fits).then_some("A host's name is two or more characters, without spaces, slashes or colons")
}

fn draft_id(name: &str, key: &str) -> &'static str {
    field_id(format!("host-page-{key}@{name}"))
}

/// A setting's value as its field shows it.
fn json_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl Diffui {
    /// The `[hosts.<name>]` tables in force, from both files.
    pub(crate) fn host_tables(&self) -> HostTables {
        merged(&self.config_files.hosts, &self.user_config.hosts)
    }

    /// Leave the open host's page, keeping what was typed on it.
    pub(crate) fn close_host_page(&mut self) {
        self.commit_host_drafts();
        self.remote.page = None;
    }

    pub(crate) fn update_host_page(&mut self, action: HostAction, cx: &mut Cx) {
        match action {
            HostAction::Open(name) => self.open_host_page(name),
            HostAction::Back => self.close_host_page(),
            HostAction::Draft { key, text } => {
                if let Some(page) = self.remote.page.as_mut() {
                    page.drafts.insert(key, text);
                }
            }
            HostAction::NewName(text) => self.remote.new_host = text,
            HostAction::Commit(key) => self.commit_host_draft(&key),
            HostAction::Set { key, value } => self.set_host_value(&key, value),
            HostAction::Reach(transport) => self.set_host_reach(transport),
            HostAction::Add => self.add_host(),
            HostAction::Remove => self.remove_host(),
            HostAction::Connect => self.connect_host(cx),
            HostAction::Disconnect => {
                if let Some(page) = self.remote.page.as_ref() {
                    self.disconnect_host(&HostId(page.name.clone()));
                }
            }
            HostAction::CopyOutput => {
                let transcript = self
                    .remote
                    .page
                    .as_ref()
                    .and_then(|page| self.host_transcript(&HostId(page.name.clone())));
                if let Some(transcript) = transcript {
                    self.perform(Action::Copy(transcript_text(&transcript)), cx);
                }
            }
        }
    }

    fn open_host_page(&mut self, name: String) {
        if self.mode(ModeKind::Settings).is_none() {
            self.open_settings();
        }
        self.close_host_page();
        self.plugins.page = None;
        let by_command = self
            .host_tables()
            .get(&name)
            .is_some_and(|table| table.contains_key(COMMAND));
        self.remote.page = Some(HostPage {
            name,
            drafts: HashMap::new(),
            by_command,
        });
    }

    fn commit_host_drafts(&mut self) {
        let Some(page) = self.remote.page.as_ref() else {
            return;
        };
        let mut keys: Vec<String> = page.drafts.keys().cloned().collect();
        keys.sort();
        for key in keys {
            self.commit_host_draft(&key);
        }
    }

    /// Save what is typed in `key`'s field. Text that can't be the setting
    /// stays in the field.
    fn commit_host_draft(&mut self, key: &str) {
        let Some(page) = self.remote.page.as_mut() else {
            return;
        };
        let Some(typed) = page.drafts.remove(key) else {
            return;
        };
        let name = page.name.clone();
        let text = typed.trim();
        let words = |text: &str| {
            let words = split(text);
            (!words.is_empty())
                .then(|| toml::Value::Array(words.into_iter().map(toml::Value::String).collect()))
        };
        let value = match key {
            SERVER | COMMAND => Ok(words(text)),
            ENV => parse_env(text).map(|env| (!env.is_empty()).then_some(toml::Value::Table(env))),
            _ if text.is_empty() => Ok(None),
            _ => match self.host_field_kind(&name, key) {
                Some(ConfigKind::Number) => text
                    .parse::<i64>()
                    .map(|whole| Some(toml::Value::Integer(whole)))
                    .map_err(|_| format!("{text} isn't a number")),
                _ => Ok(Some(toml::Value::String(text.to_owned()))),
            },
        };
        match value {
            Ok(value) => {
                let saved = self
                    .config_files
                    .hosts
                    .get(&name)
                    .and_then(|table| table.get(key));
                if saved != value.as_ref() {
                    self.set_host_value(key, value);
                }
            }
            Err(why) => {
                if let Some(page) = self.remote.page.as_mut() {
                    page.drafts.insert(key.to_owned(), typed);
                }
                self.push_error_toast(Prose::from("Not saved"), &why);
            }
        }
    }

    /// What kind of setting `key` is to the transport `name` is reached
    /// over.
    fn host_field_kind(&self, name: &str, key: &str) -> Option<ConfigKind> {
        let directory = lock(&self.remote.directory);
        let id = directory.transport_id(name)?;
        let transport = directory.transports.get(&id)?;
        transport
            .manifest
            .config
            .iter()
            .find(|field| field.per_host && field.key == key)
            .map(|field| field.kind)
    }

    fn set_host_value(&mut self, key: &str, value: Option<toml::Value>) {
        let Some(name) = self.remote.page.as_ref().map(|page| page.name.clone()) else {
            return;
        };
        let mut table = self
            .config_files
            .hosts
            .get(&name)
            .cloned()
            .unwrap_or_default();
        match value {
            Some(value) => {
                table.insert(key.to_owned(), value);
            }
            None => {
                table.remove(key);
            }
        }
        self.save_host(&name, table);
    }

    /// Write `name`'s table to settings.toml and read the hosts again. An
    /// empty table is kept only where it is what makes the host one.
    fn save_host(&mut self, name: &str, table: toml::Table) {
        let elsewhere = self.user_config.hosts.contains_key(name)
            || lock(&self.remote.directory).found_by(name).is_some();
        let table = (!table.is_empty() || !elsewhere).then_some(table);
        if let Err(error) = self.config_files.write_host(name, table) {
            self.push_error_toast("Couldn't save settings", &error);
            return;
        }
        self.refresh_hosts();
        // What is connected was reached the old way.
        self.forget_host_connection(&HostId(name.to_owned()));
    }

    /// Reach the open host another way. The settings that went with the old
    /// way go: they are another transport's.
    fn set_host_reach(&mut self, transport: Option<String>) {
        self.commit_host_drafts();
        let Some(page) = self.remote.page.as_mut() else {
            return;
        };
        let name = page.name.clone();
        page.by_command = transport.is_none();
        page.drafts.clear();
        let mut table = self
            .config_files
            .hosts
            .get(&name)
            .cloned()
            .unwrap_or_default();
        table.retain(|key, _| key == SERVER || key == ENV);
        let found_by = lock(&self.remote.directory)
            .found_by(&name)
            .map(|(id, _)| id.to_owned());
        // The transport that found it needs no saying.
        if let Some(id) = transport.filter(|id| found_by.as_deref() != Some(id)) {
            table.insert(TRANSPORT.to_owned(), toml::Value::String(id));
        }
        self.save_host(&name, table);
    }

    fn add_host(&mut self) {
        let name = self.remote.new_host.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if let Some(problem) = name_problem(&name) {
            return self.push_error_toast(Prose::from("Not added"), problem);
        }
        self.remote.new_host.clear();
        if !lock(&self.remote.directory).knows(&name) {
            let mut table = toml::Table::new();
            // ssh is what a host with only a name is most likely reached
            // over; the page says so and offers the rest.
            if lock(&self.remote.directory).transports.contains_key("ssh") {
                table.insert(TRANSPORT.to_owned(), toml::Value::String("ssh".to_owned()));
            }
            if let Err(error) = self.config_files.write_host(&name, Some(table)) {
                return self.push_error_toast("Couldn't save settings", &error);
            }
            self.refresh_hosts();
        }
        self.open_host_page(name);
    }

    /// Take the open host's table out of settings.toml. A host that is one
    /// without it (config.toml's, or one a transport found) stays, as it
    /// was before Settings touched it.
    fn remove_host(&mut self) {
        let Some(page) = self.remote.page.as_mut() else {
            return;
        };
        page.drafts.clear();
        let name = page.name.clone();
        if let Err(error) = self.config_files.write_host(&name, None) {
            return self.push_error_toast("Couldn't save settings", &error);
        }
        self.refresh_hosts();
        self.forget_host_connection(&HostId(name.clone()));
        if lock(&self.remote.directory).knows(&name) {
            let by_command = self
                .host_tables()
                .get(&name)
                .is_some_and(|table| table.contains_key(COMMAND));
            if let Some(page) = self.remote.page.as_mut() {
                page.by_command = by_command;
            }
        } else {
            self.remote.page = None;
        }
    }

    /// Reach the open host as it is now set. The app keeps the connection:
    /// the page shows how it stands, and leaving the page doesn't end it.
    fn connect_host(&mut self, cx: &mut Cx) {
        self.commit_host_drafts();
        let Some(name) = self.remote.page.as_ref().map(|page| page.name.clone()) else {
            return;
        };
        let host = HostId(name);
        // A try that failed is tried anew; one that is up or coming stays.
        if matches!(self.host_state(&host), Some(HostState::Failed(_))) {
            self.disconnect_host(&host);
        }
        self.host_link(&host, cx);
    }
}

fn send(
    action: HostAction,
    cx: &mut Cx,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(Action::Host(action), cx)
}

/// A one-line field: what is typed goes to `on_input`, Return to
/// `on_submit`.
#[allow(clippy::too_many_arguments)]
fn text_field(
    look: Look,
    id: &'static str,
    value: &str,
    placeholder: &str,
    on_input: impl Fn(String) -> HostAction + 'static,
    on_submit: HostAction,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let typed = Diffui::listen(cx, move |this, text: &String, _, cx| {
        this.perform(Action::Host(on_input(text.clone())), cx);
    });
    let submit = Diffui::on_action::<()>(Action::Host(on_submit), cx);
    let spec = input::FieldSpec::new(id, value)
        .placeholder(placeholder)
        .on_input(move |text, window, cx| typed(&text, window, cx))
        .on_submit(move |window, cx| submit(&(), window, cx));
    div()
        .w(px(FIELD_WIDTH))
        .child(crate::field::dialog_input(spec, look, window, cx))
        .into_any_element()
}

/// What the open page draws from: the host's tables and how it is reached.
struct Shown {
    name: String,
    /// The table in force, from both files.
    table: toml::Table,
    /// config.toml's table: its keys are locked.
    user: toml::Table,
    /// The transport it is reached over, when that one is running.
    transport: Option<(String, Transport)>,
    /// The settings the transport found the host with.
    found: serde_json::Map<String, serde_json::Value>,
}

impl Shown {
    fn locked(&self, key: &str) -> bool {
        self.user.contains_key(key)
    }

    /// A transport setting's value: the host's own, else what the transport
    /// found it with, else the plugin's.
    fn option(&self, key: &str) -> Option<serde_json::Value> {
        if let Some(own) = self.table.get(key) {
            return serde_json::to_value(own).ok();
        }
        self.found.get(key).cloned().or_else(|| {
            let (_, transport) = self.transport.as_ref()?;
            transport.settings.get(key).cloned()
        })
    }
}

/// The page, in place of the Settings list, while one is open.
pub(crate) fn page_overlay(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let Look { theme, .. } = look;
    let page = ui.remote.page.as_ref()?;
    let name = page.name.clone();
    let directory = lock(&ui.remote.directory);
    let transports: Vec<(String, Transport)> = directory
        .transports
        .iter()
        .map(|(id, transport)| (id.clone(), transport.clone()))
        .collect();
    let transport_id = directory.transport_id(&name);
    let found = directory.found_by(&name);
    let elsewhere = found.is_some() || ui.user_config.hosts.contains_key(&name);
    let shown = Shown {
        table: ui.host_tables().remove(&name).unwrap_or_default(),
        user: ui.user_config.hosts.get(&name).cloned().unwrap_or_default(),
        transport: transport_id.as_ref().and_then(|id| {
            transports
                .iter()
                .find(|(known, _)| known == id)
                .map(|(id, transport)| (id.clone(), transport.clone()))
        }),
        found: found
            .map(|(_, options)| options.clone())
            .unwrap_or_default(),
        name: name.clone(),
    };
    let problem = directory.problem(&name);
    drop(directory);

    let mut body = div().flex().flex_col().w_full().pr(px(DIALOG_PAD));
    let mut height = 0.0;

    // How it is reached.
    let reach_locked = shown.locked(TRANSPORT) || shown.locked(COMMAND);
    let mut choices: Vec<(Option<String>, String)> = transports
        .iter()
        .map(|(id, transport)| (Some(id.clone()), transport.name.clone()))
        .collect();
    choices.push((None, "Command".to_owned()));
    let reach = ui::segmented(theme)
        .children(choices.into_iter().enumerate().map(|(index, (id, name))| {
            let active = match &id {
                Some(id) => !page.by_command && transport_id.as_ref() == Some(id),
                None => page.by_command,
            };
            ui::segment(
                ("host-page-reach", index),
                Segment {
                    icon: None,
                    label: name.into(),
                    count: None,
                    active,
                    enabled: !reach_locked,
                },
                Segmented::Compact,
                look,
            )
            .when(!reach_locked, |segment| {
                segment.on_click(send(HostAction::Reach(id), cx))
            })
        }))
        .into_any_element();
    let mut rows = vec![row_shell(
        ROW_HEIGHT,
        label(look, "Reached over", reach_locked, None),
        reach,
    )];
    height += ROW_HEIGHT;
    if page.by_command {
        rows.push(words_row(
            look,
            page,
            &shown,
            COMMAND,
            "Command",
            "A program whose input and output reach the host. {server} in it is where the server goes.",
            "docker exec -i dev",
            window,
            cx,
        ));
        height += HINT_ROW_HEIGHT;
    } else if let Some((_, transport)) = &shown.transport {
        for field in transport
            .manifest
            .config
            .iter()
            .filter(|field| field.per_host && field.kind != ConfigKind::Secret)
        {
            rows.push(option_row(look, page, &shown, field, window, cx));
            height += HINT_ROW_HEIGHT;
        }
    }
    let host = HostId(name.clone());
    let state = ui.host_state(&host);
    rows.push(connection_row(look, state.as_ref(), problem, cx));
    height += HINT_ROW_HEIGHT;
    body = body.child(group(look, "Connection", false, rows));
    height += GROUP_HEAD;

    // What reaching it ran and wrote: live while it comes up, and what is
    // left of a try that failed.
    if let Some(transcript) = ui.host_transcript(&host) {
        height += output_height(&transcript) + GROUP_HEAD;
        body = body.child(group(
            look,
            "Output",
            true,
            vec![output_row(look, &transcript, cx)],
        ));
    }

    // What runs there.
    let rows = vec![
        words_row(
            look,
            page,
            &shown,
            SERVER,
            "Command",
            "The command that is diffui-serve there. diffui installs nothing.",
            DEFAULT_SERVER,
            window,
            cx,
        ),
        words_row(
            look,
            page,
            &shown,
            ENV,
            "Environment",
            "Set for the server.",
            "NAME=value",
            window,
            cx,
        ),
    ];
    height += 2.0 * HINT_ROW_HEIGHT + GROUP_HEAD;
    body = body.child(group(look, "Server", true, rows));

    let height = (ui.window_size.height - crate::toolbar::modal_top() - theme::space::XXL)
        .min(height + DIALOG_HEAD)
        .max(240.0);
    let saved = ui.config_files.hosts.contains_key(&name);
    // A host that is one without Settings' table goes back to how it was.
    let remove = look
        .button(
            "host-page-remove",
            if elsewhere { "Reset" } else { "Remove" },
        )
        .disabled(!saved)
        .when(saved, |button| {
            button.on_click(send(HostAction::Remove, cx))
        });
    let back = look
        .button("host-page-back", "Back")
        .on_click(send(HostAction::Back, cx));
    let card = overlay_kit::Dialog::new(&Prose::new().path(name), look)
        .width(CARD_WIDTH)
        .height(height)
        .child(div().size_full().child(ui::scrollable("host-page", body)))
        .bound_hint(
            ui,
            crate::commands::Context::Settings,
            &["settings.close"],
            "Close",
        )
        .buttons([remove.into_any_element(), back.into_any_element()])
        .build();
    let dismiss = Diffui::on_action::<()>(Action::CloseSettings, cx);
    Some(
        ui::modal(
            "settings-dialog",
            card,
            crate::toolbar::modal_top(),
            move |window, cx| dismiss(&(), window, cx),
        )
        .into_any_element(),
    )
}

/// A locked setting's value: read, not typed.
fn locked_value(look: Look, text: String) -> AnyElement {
    let Look { theme, mono, .. } = look;
    div()
        .max_w(px(FIELD_WIDTH))
        .child(
            typography::line(text, mono, text_size::BODY, theme.muted_text)
                .overflow_hidden()
                .text_ellipsis(),
        )
        .into_any_element()
}

/// `server`, `command` or `env`: words on one line.
#[allow(clippy::too_many_arguments)]
fn words_row(
    look: Look,
    page: &HostPage,
    shown: &Shown,
    key: &'static str,
    title: &str,
    hint: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let _ = look;
    let saved = match key {
        ENV => env_line(shown.table.get(key)),
        _ => line_of(&words_of(shown.table.get(key))),
    };
    let locked = shown.locked(key);
    let control = if locked {
        locked_value(look, saved)
    } else {
        let text = page.drafts.get(key).cloned().unwrap_or(saved);
        text_field(
            look,
            draft_id(&shown.name, key),
            &text,
            placeholder,
            move |text| HostAction::Draft {
                key: key.to_owned(),
                text,
            },
            HostAction::Commit(key.to_owned()),
            window,
            cx,
        )
    };
    row_shell(
        HINT_ROW_HEIGHT,
        label(look, title, locked, Some(hint_lines(look, hint))),
        control,
    )
}

/// One of the transport's per-host settings, with the control its type
/// gets.
fn option_row(
    look: Look,
    page: &HostPage,
    shown: &Shown,
    field: &ConfigField,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let locked = shown.locked(&field.key);
    let current = shown.option(&field.key);
    let choice = |index: usize, text: String, active: bool, value: toml::Value, cx: &mut Cx| {
        ui::segment(
            (
                gpui_kit::SharedString::from(format!("host-page-{}", field.key)),
                index,
            ),
            Segment {
                icon: None,
                label: text.into(),
                count: None,
                active,
                enabled: !locked,
            },
            Segmented::Compact,
            look,
        )
        .when(!locked, |segment| {
            segment.on_click(send(
                HostAction::Set {
                    key: field.key.clone(),
                    value: Some(value),
                },
                cx,
            ))
        })
    };
    let control = match field.kind {
        ConfigKind::Boolean => {
            let on = current.as_ref().and_then(serde_json::Value::as_bool) == Some(true);
            ui::segmented(theme)
                .children(
                    [(true, "On"), (false, "Off")]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (value, text))| {
                            choice(index, text.to_owned(), on == value, value.into(), cx)
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        }
        ConfigKind::Enum => {
            let picked = current.as_ref().and_then(serde_json::Value::as_str);
            ui::segmented(theme)
                .children(
                    field
                        .values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            let active = picked == Some(value.as_str());
                            choice(index, value.clone(), active, value.clone().into(), cx)
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        }
        _ => {
            let saved = current.as_ref().map(json_text).unwrap_or_default();
            if locked {
                locked_value(look, saved)
            } else {
                let key = field.key.clone();
                let text = page.drafts.get(&key).cloned().unwrap_or(saved);
                text_field(
                    look,
                    draft_id(&shown.name, &key),
                    &text,
                    "",
                    {
                        let key = key.clone();
                        move |text| HostAction::Draft {
                            key: key.clone(),
                            text,
                        }
                    },
                    HostAction::Commit(key),
                    window,
                    cx,
                )
            }
        }
    };
    let hint = field.hint.as_deref().map(|hint| hint_lines(look, hint));
    row_shell(
        HINT_ROW_HEIGHT,
        label(look, field.title(), locked, hint),
        control,
    )
}

/// A transcript as text to copy: the command, then what it wrote.
fn transcript_text(transcript: &Transcript) -> String {
    match &transcript.command {
        Some(command) => format!("$ {command}\n{}", transcript.output),
        None => transcript.output.clone(),
    }
}

/// How the host stands: what is wrong with its settings, or how its
/// connection is doing, and the button that starts or ends it.
fn connection_row(
    look: Look,
    state: Option<&HostState>,
    problem: Option<String>,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let (text, ink) = match (state, problem) {
        // What the program that reaches it is saying, as it says it: a
        // server being fetched or built can take minutes.
        (Some(state @ HostState::Connecting { said, .. }), _) => (
            match said {
                Some(said) => format!("{}: {said}", state.word()),
                None => format!("{}\u{2026}", state.word()),
            },
            theme.muted_text,
        ),
        (Some(HostState::Up(info)), _) => (
            match &info.home {
                Some(home) => format!("Connected: {}, home {home}", info.os),
                None => format!("Connected: {}", info.os),
            },
            theme.added_text,
        ),
        (Some(HostState::Failed(why)), _) => (why.clone(), theme.removed_text),
        (None, Some(problem)) => (problem, theme.removed_text),
        (None, None) => ("Not connected.".to_owned(), theme.subtle_text),
    };
    // One line, cut to fit; the whole of it is a hover away.
    let under = div()
        .id("host-page-connection-said")
        .w_full()
        .min_w_0()
        .tip(text.clone(), look)
        .child(caption(look, text, ink))
        .into_any_element();
    let (word, action) = match state {
        Some(HostState::Connecting { .. }) => ("Stop", HostAction::Disconnect),
        Some(HostState::Up(_)) => ("Disconnect", HostAction::Disconnect),
        Some(HostState::Failed(_)) => ("Try again", HostAction::Connect),
        None => ("Connect", HostAction::Connect),
    };
    let button = look
        .button("host-page-connect", word)
        .compact()
        .on_click(send(action, cx));
    row_shell(
        HINT_ROW_HEIGHT,
        label(look, "Connection", false, Some(under)),
        button,
    )
}

/// How many of the output's last lines the page shows. All of it is a Copy
/// away.
const OUTPUT_LINES: usize = 10;
const OUTPUT_LINE: f32 = 18.0;

/// How tall [`output_row`] is for `transcript`.
fn output_height(transcript: &Transcript) -> f32 {
    let lines = transcript.output.lines().count().min(OUTPUT_LINES)
        + usize::from(transcript.command.is_some());
    lines.max(1) as f32 * OUTPUT_LINE + 2.0 * theme::space::LG
}

/// What was run to reach the host and the end of what it wrote, in mono:
/// what there is to go on when it doesn't come up.
fn output_row(look: Look, transcript: &Transcript, cx: &mut Cx) -> AnyElement {
    let Look { theme, mono, .. } = look;
    let line = |text: String, ink| {
        div()
            .h(px(OUTPUT_LINE))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .child(
                typography::line(text, mono, text_size::CAPTION, ink)
                    .overflow_hidden()
                    .text_ellipsis(),
            )
    };
    let wrote: Vec<&str> = transcript.output.lines().collect();
    let shown = &wrote[wrote.len().saturating_sub(OUTPUT_LINES)..];
    let mut lines = div().flex().flex_col().flex_1().min_w_0().overflow_hidden();
    if let Some(command) = &transcript.command {
        lines = lines.child(line(format!("$ {command}"), theme.text));
    }
    if shown.is_empty() && transcript.command.is_none() {
        lines = lines.child(line("It wrote nothing.".to_owned(), theme.subtle_text));
    }
    for wrote in shown {
        lines = lines.child(line((*wrote).to_owned(), theme.muted_text));
    }
    let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
    let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
    let copy = look
        .icon_button("host-page-copy-output", icons::COPY, theme.muted_text)
        .size(px(side))
        .corner_radius(face)
        .tip("Copy all of it")
        .on_click(send(HostAction::CopyOutput, cx));
    div()
        .flex()
        .flex_row()
        .items_start()
        .w_full()
        .h(px(output_height(transcript)))
        .px(px(crate::settings::ROW_INSET))
        .py(px(theme::space::LG))
        .gap(px(theme::space::MD))
        .child(lines)
        .child(div().flex_none().child(ui::segmented(theme).child(copy)))
        .into_any_element()
}

/// The settings dialog's Hosts group: a row per host, then the field that
/// adds one, and how tall the rows are together.
pub(crate) fn settings_group(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> (Vec<AnyElement>, f32) {
    let Look {
        theme, font, mono, ..
    } = look;
    let hosts = lock(&ui.remote.directory).rows();
    let tables = ui.host_tables();
    let mut height = 0.0;
    let mut rows: Vec<AnyElement> = Vec::new();
    for (index, host) in hosts.iter().enumerate() {
        let mut name = vec![Aligned::text(
            typography::line(host.name.clone(), mono, text_size::BODY, theme.text),
            mono,
            text_size::BODY,
        )];
        if !host.via.is_empty() {
            name.push(ui::tag(&host.via, theme.muted_text, ui::TagSize::Small, font).aligned());
        }
        let state = ui.host_state(&HostId(host.name.clone()));
        if let Some(state) = &state {
            let ink = match state {
                HostState::Connecting { .. } => theme.muted_text,
                HostState::Up(_) => theme.added_text,
                HostState::Failed(_) => theme.removed_text,
            };
            name.push(ui::tag(state.word(), ink, ui::TagSize::Small, font).aligned());
        }
        let server = words_of(tables.get(&host.name).and_then(|table| table.get(SERVER)));
        let (said, ink, said_font) = match (&host.problem, &state) {
            (Some(problem), _) => (problem.clone(), theme.removed_text, font),
            (None, Some(HostState::Failed(why))) => (why.clone(), theme.removed_text, font),
            (
                None,
                Some(HostState::Connecting {
                    said: Some(said), ..
                }),
            ) => (said.clone(), theme.muted_text, mono),
            _ if server.is_empty() => (
                format!("Runs {DEFAULT_SERVER} from its PATH"),
                theme.muted_text,
                font,
            ),
            _ => (line_of(&server), theme.muted_text, mono),
        };
        let label = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .gap(px(theme::space::XXS))
            .child(typography::baseline_row(name, theme::space::MD))
            .child(
                div()
                    .id(("settings-host-said", index))
                    .w_full()
                    .min_w_0()
                    .tip(said.clone(), look)
                    .child(
                        typography::line(said, said_font, text_size::CAPTION, ink)
                            .overflow_hidden()
                            .text_ellipsis(),
                    ),
            );
        let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
        let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
        let edit = look
            .icon_button(
                ("settings-host-edit", index),
                icons::PENCIL,
                theme.muted_text,
            )
            .size(px(side))
            .corner_radius(face)
            .tip("Settings")
            .on_click(send(HostAction::Open(host.name.clone()), cx));
        rows.push(row_shell(
            HINT_ROW_HEIGHT,
            label,
            ui::segmented(theme).child(edit),
        ));
        height += HINT_ROW_HEIGHT;
    }

    let typed = ui.remote.new_host.clone();
    let add = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .child(text_field(
            look,
            NEW_HOST,
            &typed,
            "devbox",
            HostAction::NewName,
            HostAction::Add,
            window,
            cx,
        ))
        .child(
            look.button("settings-add-host", "Add")
                .compact()
                .disabled(typed.trim().is_empty())
                .on_click(send(HostAction::Add, cx)),
        );
    rows.push(row_shell(
        HINT_ROW_HEIGHT,
        label_with_hint(
            look,
            "Another host",
            "A machine with repositories to open: its name here, then how to reach it",
        ),
        add,
    ));
    height += HINT_ROW_HEIGHT;
    (rows, height)
}

fn label_with_hint(look: Look, name: &str, hint: &str) -> gpui_kit::Div {
    label(look, name, false, Some(hint_lines(look, hint)))
}

/// A line of explanation under a setting's name, wrapped to two lines.
fn hint_lines(look: Look, text: &str) -> AnyElement {
    let Look { theme, font, .. } = look;
    div()
        .font(font.gpui())
        .text_size(px(text_size::CAPTION))
        .text_color(theme.subtle_text)
        .child(text.to_owned())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn config_toml_goes_over_settings_a_key_at_a_time() {
        let settings = HostTables::from([
            (
                "box".to_owned(),
                table("transport = \"ssh\"\nport = 2222\nserver = [\"a\"]\nenv = { A = \"1\" }"),
            ),
            ("mine".to_owned(), table("transport = \"ssh\"")),
        ]);
        let config = HostTables::from([
            ("box".to_owned(), table("server = [\"b\"]")),
            ("theirs".to_owned(), table("command = [\"x\"]")),
        ]);
        let merged = merged(&settings, &config);
        assert_eq!(
            merged["box"],
            table("transport = \"ssh\"\nport = 2222\nserver = [\"b\"]\nenv = { A = \"1\" }")
        );
        assert_eq!(merged["mine"], table("transport = \"ssh\""));
        assert_eq!(merged["theirs"], table("command = [\"x\"]"));
    }

    #[test]
    fn config_tomls_way_to_reach_a_host_replaces_settings_and_its_settings() {
        let settings = HostTables::from([(
            "box".to_owned(),
            table("transport = \"ssh\"\nport = 2222\nserver = [\"a\"]"),
        )]);
        let config = HostTables::from([("box".to_owned(), table("command = [\"docker\"]"))]);
        assert_eq!(
            merged(&settings, &config)["box"],
            table("command = [\"docker\"]\nserver = [\"a\"]")
        );
    }

    #[test]
    fn words_read_back_as_they_were_typed() {
        let words = split("nix run 'github:me/diffui/{rev}#diffui-serve' -- \"a b\"");
        assert_eq!(
            words,
            [
                "nix",
                "run",
                "github:me/diffui/{rev}#diffui-serve",
                "--",
                "a b"
            ]
        );
        assert_eq!(split(&line_of(&words)), words);
    }

    #[test]
    fn an_environment_is_name_value_words() {
        let env = parse_env("JJ_CONFIG=/home/me/jj.toml NAME='two words' EMPTY=").unwrap();
        assert_eq!(
            env,
            table("JJ_CONFIG = \"/home/me/jj.toml\"\nNAME = \"two words\"\nEMPTY = \"\"")
        );
        let value = toml::Value::Table(env.clone());
        assert_eq!(parse_env(&env_line(Some(&value))).unwrap(), env);
        assert_eq!(parse_env("oops").unwrap_err(), "oops isn't NAME=value");
        assert_eq!(parse_env("=x").unwrap_err(), "=x isn't NAME=value");
    }

    #[test]
    fn a_hosts_name_has_to_read_in_host_path() {
        for name in ["box", "Ubuntu-24.04", "me@box"] {
            assert_eq!(name_problem(name), None, "{name}");
        }
        for name in ["", "c", "a b", "a/b", "a\\b", "a:b"] {
            assert!(name_problem(name).is_some(), "{name}");
        }
    }
}
