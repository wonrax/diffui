//! `plugin.toml`: what a plugin is, what it may do and what it adds.
//!
//! Everything the app has to know before any script runs is here, so menus,
//! keys and the permission prompt never wait on (or trust) plugin code.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Spanned;

/// The plugin API this build speaks. A manifest's `api` must be this or the
/// one before it; `0` is unstable through alpha.
pub const API_VERSION: u32 = 0;

/// The file a plugin directory must hold.
pub const MANIFEST_FILE: &str = "plugin.toml";

/// The user-config key `auth.token` reads its command from. A schema can't
/// declare it.
pub(crate) const TOKEN_CMD: &str = "token_cmd";

/// A problem in a manifest, placed. Displays as `path:line:col: message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub col: usize,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.path.display(),
            self.line,
            self.col,
            self.message
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// The plugin's directory. Relative imports resolve inside it.
    pub dir: PathBuf,
    pub id: String,
    pub name: String,
    pub version: String,
    pub api: u32,
    /// The entry module, relative to `dir`.
    pub main: String,
    pub permissions: Permissions,
    /// `[provides] forge`: the hosts whose remotes this plugin answers for.
    pub forge: Option<ForgeClaim>,
    pub commands: Vec<CommandDecl>,
    /// `[config.schema]`, by key.
    pub config: Vec<ConfigField>,
}

/// What the manifest asks for. The app shows this before the first load and
/// stores what the user granted, so it compares and serialises.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Hosts `http.fetch` may reach and `auth.token` may be asked for.
    #[serde(default)]
    pub http: Vec<HostPattern>,
    /// Argv prefixes `exec` may run.
    #[serde(default)]
    pub exec: Vec<Vec<String>>,
    #[serde(default)]
    pub jj: JjAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JjAccess {
    /// Only the handle's own fields (`root`, `forgeRemote`).
    #[default]
    None,
    Read,
    Write,
}

/// A host a plugin may talk to: `codeberg.org`, `*.example.com` (any
/// subdomain, not the bare domain) or `127.0.0.1:3000` (that port only).
/// Without a port, any port matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPattern {
    wildcard: bool,
    host: String,
    port: Option<u16>,
}

impl HostPattern {
    pub fn parse(text: &str) -> Result<Self, String> {
        let lower = text.trim().to_ascii_lowercase();
        let (wildcard, rest) = match lower.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, lower.as_str()),
        };
        // A colon outside a bracketed IPv6 literal starts the port.
        let (host, port) = match rest.rfind(':').filter(|at| !rest[*at..].contains(']')) {
            Some(at) => (&rest[..at], Some(rest[at + 1..].parse::<u16>().ok())),
            None => (rest, None),
        };
        let bracketed = host.len() > 2 && host.starts_with('[') && host.ends_with(']');
        let plain = !host.is_empty()
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !(plain || bracketed && !wildcard) || port == Some(None) {
            return Err(format!(
                "\"{text}\" is not a host; write one like \"codeberg.org\", \"*.example.com\" or \"localhost:3000\""
            ));
        }
        Ok(Self {
            wildcard,
            host: host.to_string(),
            port: port.flatten(),
        })
    }

    /// Whether a request to `host` (as in the URL, lowercase or not) on
    /// `port` (the URL's, or its scheme's default) is covered.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        let host_ok = if self.wildcard {
            host.strip_suffix(&self.host)
                .is_some_and(|front| front.len() > 1 && front.ends_with('.'))
        } else {
            host == self.host
        };
        host_ok && self.port.is_none_or(|p| p == port)
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.wildcard {
            f.write_str("*.")?;
        }
        f.write_str(&self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for HostPattern {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        Self::parse(&text)
    }
}

impl From<HostPattern> for String {
    fn from(pattern: HostPattern) -> String {
        pattern.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeClaim {
    pub hosts: Vec<HostPattern>,
    /// What the forge calls a pull request: `term = "merge request"` makes
    /// the app say that in its menus, dialogs and badges.
    pub term: ForgeTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForgeTerm {
    #[default]
    PullRequest,
    MergeRequest,
}

/// A `[[commands]]` row: the shape of a command declared in config.toml,
/// without `exec` / `run`, because the handler is the script's
/// `commands.register(id, …)`.
///
/// `when`, `keys` and `group` are kept as written: the app owns their
/// grammars and reports problems at this row's position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDecl {
    pub id: String,
    pub label: String,
    pub hint: Option<String>,
    pub group: Option<String>,
    pub when: Option<String>,
    pub menu: Vec<MenuPlace>,
    pub keys: Vec<String>,
    /// Where the row's `id` is in the manifest, 1-based.
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuPlace {
    Palette,
    Revision,
    Bookmark,
    File,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigField {
    pub key: String,
    pub kind: ConfigKind,
    pub default: Option<serde_json::Value>,
    pub label: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigKind {
    String,
    Boolean,
    Number,
}

impl ConfigKind {
    fn admits(self, value: &serde_json::Value) -> bool {
        match self {
            ConfigKind::String => value.is_string(),
            ConfigKind::Boolean => value.is_boolean(),
            ConfigKind::Number => value.is_number(),
        }
    }

    fn noun(self) -> &'static str {
        match self {
            ConfigKind::String => "a string",
            ConfigKind::Boolean => "true or false",
            ConfigKind::Number => "a number",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    id: Spanned<String>,
    name: Spanned<String>,
    version: Spanned<String>,
    api: Spanned<u32>,
    main: Spanned<String>,
    #[serde(default)]
    permissions: RawPermissions,
    #[serde(default)]
    provides: RawProvides,
    #[serde(default)]
    commands: Vec<RawCommand>,
    #[serde(default)]
    config: RawConfig,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPermissions {
    #[serde(default)]
    http: Vec<Spanned<String>>,
    #[serde(default)]
    exec: Vec<Spanned<Vec<String>>>,
    jj: Option<Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProvides {
    forge: Option<RawForge>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForge {
    hosts: Spanned<Vec<Spanned<String>>>,
    term: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: Spanned<String>,
    label: Spanned<String>,
    hint: Option<String>,
    group: Option<String>,
    when: Option<Spanned<String>>,
    #[serde(default)]
    menu: Vec<MenuPlace>,
    #[serde(default)]
    keys: Vec<Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    schema: BTreeMap<Spanned<String>, Spanned<RawField>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    #[serde(rename = "type")]
    kind: ConfigKind,
    default: Option<toml::Value>,
    label: Option<String>,
    hint: Option<String>,
}

/// Collects problems against one file's text.
struct Report<'a> {
    path: &'a Path,
    text: &'a str,
    problems: Vec<Diagnostic>,
}

impl Report<'_> {
    fn at(&mut self, span: Range<usize>, message: impl Into<String>) {
        let (line, col) = line_col(self.text, span.start);
        self.problems.push(Diagnostic {
            path: self.path.to_path_buf(),
            line,
            col,
            message: message.into(),
        });
    }
}

/// 1-based line and character column of a byte offset.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

impl Manifest {
    /// Read and check the manifest in `dir`, including that its `main` is
    /// there.
    pub fn load(dir: &Path) -> Result<Manifest, Vec<Diagnostic>> {
        let path = dir.join(MANIFEST_FILE);
        let whole_file = |message: String| {
            vec![Diagnostic {
                path: path.clone(),
                line: 1,
                col: 1,
                message,
            }]
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| whole_file(format!("can't read the manifest: {error}")))?;
        let manifest = Manifest::parse(dir, &text)?;
        if !dir.join(&manifest.main).is_file() {
            return Err(whole_file(format!(
                "main = \"{}\": no such file in {}",
                manifest.main,
                dir.display()
            )));
        }
        Ok(manifest)
    }

    /// Check a manifest's text. Touches no files; `dir` only names the plugin
    /// directory in the result and in diagnostics.
    pub fn parse(dir: &Path, text: &str) -> Result<Manifest, Vec<Diagnostic>> {
        let path = dir.join(MANIFEST_FILE);
        let mut report = Report {
            path: &path,
            text,
            problems: Vec::new(),
        };
        let raw: Raw = match toml::from_str(text) {
            Ok(raw) => raw,
            Err(error) => {
                report.at(error.span().unwrap_or(0..0), error.message());
                return Err(report.problems);
            }
        };

        let id = raw.id.get_ref().clone();
        let id_ok = id.len() <= 32
            && id.starts_with(|c: char| c.is_ascii_lowercase())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !id_ok {
            report.at(
                raw.id.span(),
                format!("id \"{id}\" must be lowercase letters, digits and dashes, starting with a letter, at most 32 long"),
            );
        }
        if raw.name.get_ref().trim().is_empty() {
            report.at(raw.name.span(), "name is empty");
        }
        if raw.version.get_ref().trim().is_empty() {
            report.at(raw.version.span(), "version is empty");
        }
        let api = *raw.api.get_ref();
        // The host speaks the current api and the one before it.
        if !(API_VERSION.saturating_sub(1)..=API_VERSION).contains(&api) {
            let fix = if api > API_VERSION {
                "update diffui"
            } else {
                "update the plugin"
            };
            report.at(
                raw.api.span(),
                format!(
                    "api = {api} is not supported: this diffui speaks api {API_VERSION}; {fix}"
                ),
            );
        }
        let main = raw.main.get_ref().clone();
        if let Err(why) = check_main(&main) {
            report.at(raw.main.span(), format!("main = \"{main}\": {why}"));
        }

        let hosts = |list: &[Spanned<String>], report: &mut Report| {
            list.iter()
                .filter_map(|host| match HostPattern::parse(host.get_ref()) {
                    Ok(pattern) => Some(pattern),
                    Err(why) => {
                        report.at(host.span(), why);
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let http = hosts(&raw.permissions.http, &mut report);
        let forge = raw.provides.forge.map(|forge| {
            if forge.hosts.get_ref().is_empty() {
                report.at(
                    forge.hosts.span(),
                    "forge.hosts is empty: name the hosts this plugin answers for",
                );
            }
            let term = match forge.term.as_ref().map(|term| (term, term.get_ref().as_str())) {
                None | Some((_, "pull request")) => ForgeTerm::PullRequest,
                Some((_, "merge request")) => ForgeTerm::MergeRequest,
                Some((term, other)) => {
                    report.at(
                        term.span(),
                        format!(
                            "forge.term {other:?} is not known: write \"pull request\" or \"merge request\""
                        ),
                    );
                    ForgeTerm::PullRequest
                }
            };
            ForgeClaim {
                hosts: hosts(forge.hosts.get_ref(), &mut report),
                term,
            }
        });
        for argv in &raw.permissions.exec {
            if argv.get_ref().is_empty() || argv.get_ref().iter().any(String::is_empty) {
                report.at(
                    argv.span(),
                    "an exec permission is an argv prefix like [\"git\", \"credential\"], with no empty words",
                );
            }
        }
        let jj = match &raw.permissions.jj {
            None => JjAccess::None,
            Some(jj) => match jj.get_ref().as_str() {
                "read" => JjAccess::Read,
                "write" => JjAccess::Write,
                other => {
                    report.at(
                        jj.span(),
                        format!("jj = \"{other}\": expected \"read\" or \"write\""),
                    );
                    JjAccess::None
                }
            },
        };

        let mut commands: Vec<CommandDecl> = Vec::new();
        for row in raw.commands {
            let command = row.id.get_ref().clone();
            let prefix = format!("{id}.");
            let rest = command.strip_prefix(&prefix).unwrap_or("");
            let rest_ok = !rest.is_empty()
                && rest.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                });
            if !rest_ok {
                report.at(
                    row.id.span(),
                    format!("[[commands]] \"{command}\": a command id is the plugin id, a dot, then lowercase letters, digits and dashes (\"{id}.do-thing\")"),
                );
            }
            if commands.iter().any(|c| c.id == command) {
                report.at(
                    row.id.span(),
                    format!("[[commands]] \"{command}\": declared twice"),
                );
            }
            if row.label.get_ref().trim().is_empty() {
                report.at(
                    row.label.span(),
                    format!("[[commands]] \"{command}\": label is empty"),
                );
            }
            if let Some(when) = &row.when
                && when.get_ref().trim().is_empty()
            {
                report.at(
                    when.span(),
                    format!(
                        "[[commands]] \"{command}\": when is empty; leave it out to always enable"
                    ),
                );
            }
            for key in &row.keys {
                if key.get_ref().trim().is_empty() {
                    report.at(
                        key.span(),
                        format!("[[commands]] \"{command}\": an empty key"),
                    );
                }
            }
            let (line, col) = line_col(text, row.id.span().start);
            commands.push(CommandDecl {
                id: command,
                label: row.label.into_inner(),
                hint: row.hint,
                group: row.group,
                when: row.when.map(Spanned::into_inner),
                menu: row.menu,
                keys: row.keys.into_iter().map(Spanned::into_inner).collect(),
                line,
                col,
            });
        }

        let mut config = Vec::new();
        for (key, field) in raw.config.schema {
            let span = field.span();
            let field = field.into_inner();
            let name = key.get_ref().clone();
            if name == TOKEN_CMD {
                report.at(
                    key.span(),
                    format!(
                        "config key \"{TOKEN_CMD}\" is reserved: diffui reads it for auth.token"
                    ),
                );
            }
            let default = field.default.map(toml_to_json);
            if let Some(default) = &default
                && !field.kind.admits(default)
            {
                report.at(
                    span,
                    format!(
                        "config \"{name}\": the default must be {}",
                        field.kind.noun()
                    ),
                );
            }
            config.push(ConfigField {
                key: name,
                kind: field.kind,
                default,
                label: field.label,
                hint: field.hint,
            });
        }

        if !report.problems.is_empty() {
            return Err(report.problems);
        }
        Ok(Manifest {
            dir: dir.to_path_buf(),
            id,
            name: raw.name.into_inner(),
            version: raw.version.into_inner(),
            api,
            main,
            permissions: Permissions {
                http,
                exec: raw
                    .permissions
                    .exec
                    .into_iter()
                    .map(Spanned::into_inner)
                    .collect(),
                jj,
            },
            forge,
            commands,
            config,
        })
    }

    /// The table a script sees as `config`: the schema's defaults under the
    /// user's `[plugins.<id>]` table. A value of the wrong type keeps the
    /// default and is reported; keys outside the schema pass through.
    pub fn resolve_config(
        &self,
        user: &toml::Table,
    ) -> (serde_json::Map<String, serde_json::Value>, Vec<String>) {
        let mut config = serde_json::Map::new();
        let mut problems = Vec::new();
        for field in &self.config {
            if let Some(default) = &field.default {
                config.insert(field.key.clone(), default.clone());
            }
        }
        for (key, value) in user {
            let value = toml_to_json(value.clone());
            match self.config.iter().find(|field| &field.key == key) {
                Some(field) if !field.kind.admits(&value) => problems.push(format!(
                    "[plugins.{}] {key} must be {}",
                    self.id,
                    field.kind.noun()
                )),
                _ => {
                    config.insert(key.clone(), value);
                }
            }
        }
        (config, problems)
    }
}

/// `main` names a JavaScript or TypeScript module inside the plugin
/// directory.
fn check_main(main: &str) -> Result<(), &'static str> {
    let path = Path::new(main);
    let inside = path
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if main.is_empty() || !inside {
        return Err("must be a path inside the plugin directory");
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "mjs" | "ts" | "mts") => Ok(()),
        _ => Err("must be a .js, .mjs, .ts or .mts module"),
    }
}

pub(crate) fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => text.into(),
        toml::Value::Integer(number) => number.into(),
        toml::Value::Float(number) => number.into(),
        toml::Value::Boolean(flag) => flag.into(),
        toml::Value::Datetime(when) => when.to_string().into(),
        toml::Value::Array(items) => items.into_iter().map(toml_to_json).collect(),
        toml::Value::Table(table) => table
            .into_iter()
            .map(|(key, value)| (key, toml_to_json(value)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
id = "forgejo"
name = "Forgejo"
version = "0.1.0"
api = 0
main = "main.js"

[permissions]
http = ["codeberg.org", "*.forgejo.example", "localhost:3000"]
exec = [["git", "credential"]]
jj = "read"

[provides]
forge = { hosts = ["codeberg.org"] }

[[commands]]
id = "forgejo.open-in-browser"
label = "Open pull request in browser"
when = "bookmark.pr"
menu = ["bookmark", "palette"]
keys = ["cmd+shift+o"]

[config.schema]
host = { type = "string", default = "codeberg.org", label = "Host" }
limit = { type = "number", default = 50 }
"#;

    fn parse(text: &str) -> Result<Manifest, Vec<String>> {
        Manifest::parse(Path::new("/plugins/forgejo"), text)
            .map_err(|problems| problems.iter().map(ToString::to_string).collect())
    }

    /// `FULL` with one line swapped, so a test names only what it breaks.
    fn with(from: &str, to: &str) -> String {
        assert!(FULL.contains(from), "{from} is not in the fixture");
        FULL.replace(from, to)
    }

    #[test]
    fn a_full_manifest_reads() {
        let manifest = parse(FULL).unwrap();
        assert_eq!(manifest.id, "forgejo");
        assert_eq!(manifest.main, "main.js");
        assert_eq!(manifest.permissions.jj, JjAccess::Read);
        assert_eq!(manifest.permissions.exec, [["git", "credential"]]);
        assert_eq!(manifest.permissions.http.len(), 3);
        assert_eq!(manifest.forge.unwrap().hosts[0].to_string(), "codeberg.org");
        let command = &manifest.commands[0];
        assert_eq!(command.id, "forgejo.open-in-browser");
        assert_eq!(command.when.as_deref(), Some("bookmark.pr"));
        assert_eq!(command.menu, [MenuPlace::Bookmark, MenuPlace::Palette]);
        assert_eq!(command.keys, ["cmd+shift+o"]);
        assert_eq!((command.line, command.col), (17, 6));
        assert_eq!(manifest.config.len(), 2);
    }

    #[test]
    fn a_minimal_manifest_asks_for_nothing() {
        let manifest =
            parse("id = \"a\"\nname = \"A\"\nversion = \"1\"\napi = 0\nmain = \"a.js\"\n").unwrap();
        assert_eq!(manifest.permissions, Permissions::default());
        assert!(manifest.forge.is_none() && manifest.commands.is_empty());
    }

    #[test]
    fn problems_are_placed_and_all_reported() {
        let text = with("id = \"forgejo\"\n", "id = \"Forgejo!\"\n")
            .replace("jj = \"read\"", "jj = \"admin\"")
            .replace("\"*.forgejo.example\"", "\"https://x\"");
        let problems = parse(&text).unwrap_err();
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(
            problems[0].starts_with("/plugins/forgejo/plugin.toml:2:6: id \"Forgejo!\" must be")
        );
        assert!(
            problems[1]
                .starts_with("/plugins/forgejo/plugin.toml:9:25: \"https://x\" is not a host")
        );
        assert!(problems[2].contains(":11:6: jj = \"admin\": expected \"read\" or \"write\""));
        // The command id no longer starts with the (bad) plugin id.
        assert!(problems[3].contains(":17:6: [[commands]] \"forgejo.open-in-browser\""));
    }

    #[test]
    fn the_api_gate_names_what_this_build_speaks() {
        let problems = parse(&with("api = 0", "api = 7")).unwrap_err();
        assert_eq!(
            problems,
            [
                "/plugins/forgejo/plugin.toml:5:7: api = 7 is not supported: this diffui speaks api 0; update diffui"
            ]
        );
    }

    #[test]
    fn main_stays_inside_the_directory_and_is_a_script() {
        for (main, why) in [
            ("../evil.js", "must be a path inside the plugin directory"),
            (
                "/etc/passwd.js",
                "must be a path inside the plugin directory",
            ),
            ("main.tsx", "must be a .js, .mjs, .ts or .mts module"),
            ("main.py", "must be a .js, .mjs, .ts or .mts module"),
        ] {
            let problems =
                parse(&with("main = \"main.js\"", &format!("main = \"{main}\""))).unwrap_err();
            assert!(problems[0].contains(why), "{main}: {problems:?}");
        }
        assert!(parse(&with("main = \"main.js\"", "main = \"src/main.mjs\"")).is_ok());
        assert!(parse(&with("main = \"main.js\"", "main = \"main.ts\"")).is_ok());
    }

    #[test]
    fn a_tier_one_row_is_refused_by_its_field() {
        let problems =
            parse(&with("when = \"bookmark.pr\"", "run = \"jj git fetch\"")).unwrap_err();
        assert!(
            problems[0].contains(":19:1: unknown field `run`"),
            "{problems:?}"
        );
    }

    #[test]
    fn commands_are_namespaced_and_unique() {
        let problems =
            parse(&with("id = \"forgejo.open-in-browser\"", "id = \"open\"")).unwrap_err();
        assert!(
            problems[0].contains("a command id is the plugin id, a dot"),
            "{problems:?}"
        );
        let twice =
            format!("{FULL}\n[[commands]]\nid = \"forgejo.open-in-browser\"\nlabel = \"Again\"\n");
        // The second row lands after [config.schema], so toml reads it as a
        // top-level array again only when it comes first; put it before.
        let twice = twice.replace(
            "[config.schema]",
            "[[commands]]\nid = \"forgejo.open-in-browser\"\nlabel = \"Again\"\n\n[config.schema]",
        );
        let problems = parse(&twice).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains("declared twice")),
            "{problems:?}"
        );
    }

    #[test]
    fn exec_prefixes_and_forge_hosts_are_checked() {
        let problems =
            parse(&with("exec = [[\"git\", \"credential\"]]", "exec = [[]]")).unwrap_err();
        assert!(
            problems[0].contains("an exec permission is an argv prefix"),
            "{problems:?}"
        );
        let problems = parse(&with("hosts = [\"codeberg.org\"]", "hosts = []")).unwrap_err();
        assert!(problems[0].contains("forge.hosts is empty"), "{problems:?}");
        let hosts = "hosts = [\"codeberg.org\"]";
        let merge = parse(&with(hosts, &format!("{hosts}, term = \"merge request\""))).unwrap();
        assert_eq!(merge.forge.unwrap().term, ForgeTerm::MergeRequest);
        let problems = parse(&with(hosts, &format!("{hosts}, term = \"patch\""))).unwrap_err();
        assert!(
            problems[0].contains("forge.term \"patch\" is not known"),
            "{problems:?}"
        );
        let problems = parse(&with("\"localhost:3000\"", "\"*\"")).unwrap_err();
        assert!(problems[0].contains("\"*\" is not a host"), "{problems:?}");
    }

    #[test]
    fn schema_defaults_match_their_type_and_token_cmd_is_reserved() {
        let problems = parse(&with("default = 50", "default = \"fifty\"")).unwrap_err();
        assert!(
            problems[0].contains("config \"limit\": the default must be a number"),
            "{problems:?}"
        );
        let problems = parse(&with(
            "limit = { type = \"number\", default = 50 }",
            "token_cmd = { type = \"string\" }",
        ))
        .unwrap_err();
        assert!(
            problems[0].contains("\"token_cmd\" is reserved"),
            "{problems:?}"
        );
    }

    #[test]
    fn host_patterns_match_hosts_and_ports() {
        let exact = HostPattern::parse("Codeberg.org").unwrap();
        assert!(exact.matches("codeberg.org", 443) && exact.matches("CODEBERG.ORG", 8443));
        assert!(
            !exact.matches("evil-codeberg.org", 443) && !exact.matches("codeberg.org.evil", 443)
        );
        let wild = HostPattern::parse("*.example.com").unwrap();
        assert!(wild.matches("git.example.com", 443) && wild.matches("a.b.example.com", 443));
        assert!(!wild.matches("example.com", 443) && !wild.matches("notexample.com", 443));
        let port = HostPattern::parse("127.0.0.1:3000").unwrap();
        assert!(port.matches("127.0.0.1", 3000) && !port.matches("127.0.0.1", 3001));
        let v6 = HostPattern::parse("[::1]:8080").unwrap();
        assert!(v6.matches("[::1]", 8080) && !v6.matches("[::1]", 80));
        assert_eq!(v6.to_string(), "[::1]:8080");
        for bad in ["", "*", "a b", "http://x", "x/y", "*.", "host:port"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn config_is_defaults_under_the_users_table() {
        let manifest = parse(FULL).unwrap();
        let user: toml::Table =
            toml::from_str("host = \"git.example\"\nlimit = \"many\"\nextra = [1, 2]\n").unwrap();
        let (config, problems) = manifest.resolve_config(&user);
        assert_eq!(config["host"], "git.example");
        assert_eq!(config["limit"], 50);
        assert_eq!(config["extra"], serde_json::json!([1, 2]));
        assert_eq!(problems, ["[plugins.forgejo] limit must be a number"]);
    }

    #[test]
    fn permissions_round_trip_for_the_settings_file() {
        let manifest = parse(FULL).unwrap();
        let text = toml::to_string(&manifest.permissions).unwrap();
        assert_eq!(
            toml::from_str::<Permissions>(&text).unwrap(),
            manifest.permissions
        );
    }

    #[test]
    fn loading_checks_the_entry_module_exists() {
        let dir = std::env::temp_dir().join(format!("diffui-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), FULL).unwrap();
        let problems = Manifest::load(&dir).unwrap_err();
        assert!(
            problems[0]
                .message
                .contains("main = \"main.js\": no such file"),
            "{problems:?}"
        );
        std::fs::write(dir.join("main.js"), "").unwrap();
        assert_eq!(Manifest::load(&dir).unwrap().dir, dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
