//! `plugin.toml`: what a plugin is, what it may do and what it adds.
//!
//! Everything the app has to know before any script runs is here, so menus,
//! keys and the permission prompt never wait on (or trust) plugin code.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

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

/// The user-config key that adds hosts to a forge's claim
/// (`[plugins.<id>] hosts = ["git.acme.org"]`). A schema can't declare it.
pub const HOSTS: &str = "hosts";

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
    /// The plugin's directory. Relative imports resolve inside it. A bundled
    /// plugin's names it for messages only: its files are [`Self::files`].
    pub dir: PathBuf,
    /// The files of a plugin built into diffui, by path relative to its
    /// directory. `None`: they are on disk under `dir`.
    pub files: Option<Arc<BTreeMap<String, String>>>,
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
    /// Argv prefixes `exec` may run. A word `{config.<key>}` stands for
    /// that string setting, so the user can point the plugin at a program.
    #[serde(default)]
    pub exec: Vec<Vec<String>>,
    /// Environment variables `exec` may set for the programs it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
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
///
/// A forge's claim ([`HostPattern::parse_claim`]) may put `*` in place of any
/// label, standing for one label or more: `github.*` covers `github.com` and
/// `github.acme.com`, `*.github.*` covers `ci.github.acme.com`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostPattern {
    /// Lowercase labels; `*` is one label or more.
    labels: Vec<String>,
    port: Option<u16>,
}

impl HostPattern {
    /// A pattern a permission names: a host, maybe with `*.` in front.
    pub fn parse(text: &str) -> Result<Self, String> {
        let pattern = Self::parse_claim(text)?;
        let inner = pattern.labels.iter().skip(1).any(|label| label == "*");
        if inner {
            return Err(not_a_host(text));
        }
        Ok(pattern)
    }

    /// A pattern a forge claims remotes with: [`Self::parse`], where `*` may
    /// stand in for any label.
    pub fn parse_claim(text: &str) -> Result<Self, String> {
        let lower = text.trim().to_ascii_lowercase();
        // A colon outside a bracketed IPv6 literal starts the port.
        let (host, port) = match lower.rfind(':').filter(|at| !lower[*at..].contains(']')) {
            Some(at) => (&lower[..at], Some(lower[at + 1..].parse::<u16>().ok())),
            None => (lower.as_str(), None),
        };
        let bracketed = host.len() > 2 && host.starts_with('[') && host.ends_with(']');
        let labels: Vec<String> = if bracketed {
            vec![host.to_string()]
        } else {
            host.split('.').map(str::to_string).collect()
        };
        let plain = bracketed
            || labels.iter().all(|label| {
                label == "*"
                    || !label.is_empty()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        let named = labels.iter().any(|label| label != "*");
        if host.is_empty() || !plain || !named || port == Some(None) {
            return Err(not_a_host(text));
        }
        Ok(Self {
            labels,
            port: port.flatten(),
        })
    }

    /// Whether a request to `host` (as in the URL, lowercase or not) on
    /// `port` (the URL's, or its scheme's default) is covered.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        let labels: Vec<&str> = if host.starts_with('[') {
            vec![host.as_str()]
        } else {
            host.split('.').collect()
        };
        glob(&self.labels, &labels) && self.port.is_none_or(|p| p == port)
    }
}

fn not_a_host(text: &str) -> String {
    format!(
        "\"{text}\" is not a host; write one like \"codeberg.org\", \"*.example.com\" or \"localhost:3000\""
    )
}

/// Whether `labels` spell `pattern`, where a `*` takes one label or more.
fn glob(pattern: &[String], labels: &[&str]) -> bool {
    match pattern.split_first() {
        None => labels.is_empty(),
        Some((first, rest)) if first == "*" => {
            (1..=labels.len()).any(|taken| glob(rest, &labels[taken..]))
        }
        Some((first, rest)) => labels
            .split_first()
            .is_some_and(|(label, others)| first == label && glob(rest, others)),
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.labels.join("."))?;
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
    /// The remotes it answers for, by host ([`HostPattern::parse_claim`]).
    pub hosts: Vec<HostPattern>,
    /// What the forge calls a pull request: `term = "merge request"` makes
    /// the app say that in its menus, dialogs and badges.
    pub term: ForgeTerm,
    /// What prefixes a number in the forge's own references: `#` for a
    /// pull request, `!` for a merge request unless the manifest says.
    pub sigil: char,
    /// The abbreviation: `PR`, `MR`.
    pub short: String,
    /// The host a reference without one means (`github.com`).
    pub default_host: Option<String>,
    /// An environment variable that, when set, names that host instead
    /// (`GH_HOST`).
    pub host_env: Option<String>,
    /// The command-line program the forge's errors tell the user to install
    /// or sign in with (`gh`).
    pub cli: Option<String>,
    /// How the path of a pull request's page reads after the host, with
    /// `{owner}`, `{name}` and `{number}`: `{owner}/{name}/pull/{number}`.
    /// With it the app opens pasted links and `owner/name#12` references.
    pub pull_request_path: Option<String>,
    /// A project's owner may be several segments (GitLab's groups).
    pub subgroups: bool,
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
    /// Where the first character of `when` is, 1-based, when the clause is
    /// written on one line without escapes: then a place in the clause is a
    /// place in the file, and a problem in it can point at the character.
    pub when_at: Option<(usize, usize)>,
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
    #[serde(default)]
    env: Vec<Spanned<String>>,
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
    sigil: Option<Spanned<String>>,
    short: Option<Spanned<String>>,
    default_host: Option<Spanned<String>>,
    host_env: Option<Spanned<String>>,
    cli: Option<Spanned<String>>,
    pull_request_path: Option<Spanned<String>>,
    #[serde(default)]
    subgroups: bool,
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
                "main = \"{}\": no such file beside {MANIFEST_FILE}",
                manifest.main
            )));
        }
        Ok(manifest)
    }

    /// The manifest of a plugin whose files are in memory (one built into
    /// diffui): `files` holds `plugin.toml` and every module, by path
    /// relative to the plugin's directory. `dir` names it in messages.
    pub fn bundled(
        dir: &Path,
        files: BTreeMap<String, String>,
    ) -> Result<Manifest, Vec<Diagnostic>> {
        let whole_file = |message: String| {
            vec![Diagnostic {
                path: dir.join(MANIFEST_FILE),
                line: 1,
                col: 1,
                message,
            }]
        };
        let text = files
            .get(MANIFEST_FILE)
            .ok_or_else(|| whole_file("no manifest".into()))?;
        let mut manifest = Manifest::parse(dir, text)?;
        if !files.contains_key(&manifest.main) {
            return Err(whole_file(format!(
                "main = \"{}\": no such file beside {MANIFEST_FILE}",
                manifest.main
            )));
        }
        manifest.files = Some(Arc::new(files));
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
            let claims = forge
                .hosts
                .get_ref()
                .iter()
                .filter_map(|host| match HostPattern::parse_claim(host.get_ref()) {
                    Ok(pattern) => Some(pattern),
                    Err(why) => {
                        report.at(host.span(), why);
                        None
                    }
                })
                .collect();
            let merge = term == ForgeTerm::MergeRequest;
            let sigil = match &forge.sigil {
                None => if merge { '!' } else { '#' },
                Some(sigil) => {
                    let mut chars = sigil.get_ref().chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if c.is_ascii_punctuation() && !"/.:-_".contains(c) => c,
                        _ => {
                            report.at(sigil.span(), "forge.sigil is one punctuation character, like \"#\" or \"!\"");
                            '#'
                        }
                    }
                }
            };
            let short = match &forge.short {
                None => (if merge { "MR" } else { "PR" }).to_string(),
                Some(short) => {
                    let text = short.get_ref().trim();
                    if text.is_empty() || text.chars().count() > 4 {
                        report.at(short.span(), "forge.short is an abbreviation of one to four characters, like \"PR\"");
                    }
                    text.to_string()
                }
            };
            let default_host = forge.default_host.map(|host| {
                if let Err(why) = HostPattern::parse(host.get_ref()).and_then(|pattern| {
                    if pattern.labels.iter().any(|label| label == "*") {
                        Err(not_a_host(host.get_ref()))
                    } else {
                        Ok(())
                    }
                }) {
                    report.at(host.span(), format!("forge.default_host: {why}"));
                }
                host.into_inner().to_ascii_lowercase()
            });
            let host_env = forge.host_env.map(|var| {
                if !is_env_name(var.get_ref()) {
                    report.at(var.span(), "forge.host_env is an environment variable's name, like \"GH_HOST\"");
                }
                var.into_inner()
            });
            let cli = forge.cli.map(|cli| {
                let ok = !cli.get_ref().is_empty()
                    && cli.get_ref().bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
                if !ok {
                    report.at(cli.span(), "forge.cli is a program's name, like \"gh\"");
                }
                cli.into_inner()
            });
            let pull_request_path = forge.pull_request_path.map(|path| {
                if let Err(why) = check_pull_request_path(path.get_ref()) {
                    report.at(path.span(), format!("forge.pull_request_path: {why}"));
                }
                path.into_inner()
            });
            ForgeClaim {
                hosts: claims,
                term,
                sigil,
                short,
                default_host,
                host_env,
                cli,
                pull_request_path,
                subgroups: forge.subgroups,
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
        let mut env = Vec::new();
        for var in &raw.permissions.env {
            let name = var.get_ref();
            if !is_env_name(name) {
                report.at(
                    var.span(),
                    format!("env \"{name}\" is not an environment variable's name"),
                );
            } else if name.starts_with("DYLD_") || name.starts_with("LD_") {
                report.at(
                    var.span(),
                    format!("env \"{name}\" is not allowed: it changes how every program loads"),
                );
            } else {
                env.push(name.clone());
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
            let when_at = row.when.as_ref().and_then(|when| {
                // Between the quotes, as written.
                let span = when.span();
                let written = text.get(span.start + 1..span.end.checked_sub(1)?)?;
                (written == when.get_ref() && !written.contains('\n'))
                    .then(|| line_col(text, span.start + 1))
            });
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
                when_at,
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
            if name == HOSTS {
                report.at(
                    key.span(),
                    format!(
                        "config key \"{HOSTS}\" is reserved: diffui reads it for the hosts a forge claims"
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

        for argv in &raw.permissions.exec {
            for word in argv.get_ref() {
                let Some(key) = config_word(word) else {
                    continue;
                };
                let string = config
                    .iter()
                    .any(|field| field.key == key && field.kind == ConfigKind::String);
                if !string {
                    report.at(
                        argv.span(),
                        format!("exec {word}: [config.schema] must declare {key} = {{ type = \"string\" }}"),
                    );
                }
            }
        }

        if !report.problems.is_empty() {
            return Err(report.problems);
        }
        Ok(Manifest {
            dir: dir.to_path_buf(),
            files: None,
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
                env,
                jj,
            },
            forge,
            commands,
            config,
        })
    }

    /// Where `exec` runs a program unless the script says: the plugin's
    /// directory, or the home directory for a plugin with none on disk
    /// (a bundled one).
    pub fn cwd(&self) -> &Path {
        if self.files.is_none() && self.dir.is_dir() {
            &self.dir
        } else {
            home_dir()
        }
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

fn home_dir() -> &'static Path {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_dir())
            .unwrap_or_else(std::env::temp_dir)
    })
}

/// The setting an exec permission's word `{config.<key>}` stands for.
pub(crate) fn config_word(word: &str) -> Option<&str> {
    word.strip_prefix("{config.")?.strip_suffix('}')
}

/// The exec permissions with every `{config.<key>}` word replaced by that
/// setting. A prefix whose setting isn't a non-empty string allows nothing.
pub(crate) fn resolve_exec(
    prefixes: &[Vec<String>],
    config: &serde_json::Map<String, serde_json::Value>,
) -> Vec<Vec<String>> {
    prefixes
        .iter()
        .filter_map(|prefix| {
            prefix
                .iter()
                .map(|word| match config_word(word) {
                    None => Some(word.clone()),
                    Some(key) => config
                        .get(key)
                        .and_then(|value| value.as_str())
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                })
                .collect()
        })
        .collect()
}

/// An environment variable's name: letters, digits and underscores, not
/// starting with a digit.
fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A pull request page's path pattern: segments, each one literal or one of
/// `{owner}`, `{name}`, `{number}`, all three present once.
fn check_pull_request_path(path: &str) -> Result<(), String> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err("write segments joined by /, like \"{owner}/{name}/pull/{number}\"".into());
    }
    for field in ["{owner}", "{name}", "{number}"] {
        if segments.iter().filter(|segment| **segment == field).count() != 1 {
            return Err(format!("{field} must be a whole segment, once"));
        }
    }
    if let Some(odd) = segments.iter().find(|segment| {
        segment.contains(['{', '}']) && !["{owner}", "{name}", "{number}"].contains(segment)
    }) {
        return Err(format!("{odd} is not {{owner}}, {{name}} or {{number}}"));
    }
    Ok(())
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
        assert_eq!(command.when_at, Some((19, 9)));
        assert_eq!(manifest.config.len(), 2);
    }

    /// A clause's characters are the file's only when it is written as it
    /// reads: one line, no escapes.
    #[test]
    fn a_when_clause_is_placed_only_when_written_plainly() {
        let when_at = |written: &str| {
            parse(&with(
                "when = \"bookmark.pr\"",
                &format!("when = {written}"),
            ))
            .unwrap()
            .commands[0]
                .when_at
        };
        assert_eq!(when_at(r#"'repo.kind == "jj"'"#), Some((19, 9)));
        assert_eq!(when_at(r#""repo.kind == \"jj\"""#), None);
        assert_eq!(when_at(r#""""bookmark.pr""""#), None);
        assert_eq!(when_at("'''\nbookmark.pr'''"), None);
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

    #[test]
    fn a_forge_claims_hosts_by_label_and_names_itself() {
        let claim = |text: &str| HostPattern::parse_claim(text).unwrap();
        let named = claim("github.*");
        assert!(named.matches("github.com", 443) && named.matches("github.acme.com", 443));
        assert!(!named.matches("github", 443) && !named.matches("ci.github.com", 443));
        let inner = claim("*.github.*");
        assert!(inner.matches("ci.github.acme.com", 443) && !inner.matches("github.com", 443));
        // A permission can't be that broad.
        assert!(HostPattern::parse("github.*").is_err());
        assert!(HostPattern::parse_claim("*.*").is_err());

        let manifest = parse(&with(
            "forge = { hosts = [\"codeberg.org\"] }",
            r#"forge = { hosts = ["gitlab.com", "gitlab.*"], term = "merge request", default_host = "GitLab.com", host_env = "GITLAB_HOST", cli = "glab", pull_request_path = "{owner}/{name}/-/merge_requests/{number}", subgroups = true }"#,
        ))
        .unwrap();
        let forge = manifest.forge.unwrap();
        assert_eq!((forge.sigil, forge.short.as_str()), ('!', "MR"));
        assert_eq!(forge.default_host.as_deref(), Some("gitlab.com"));
        assert_eq!(forge.host_env.as_deref(), Some("GITLAB_HOST"));
        assert_eq!(forge.cli.as_deref(), Some("glab"));
        assert!(forge.subgroups && forge.hosts[1].matches("gitlab.acme.org", 443));

        let problems = parse(&with(
            "forge = { hosts = [\"codeberg.org\"] }",
            r#"forge = { hosts = ["codeberg.org"], sigil = "%%", host_env = "1X", cli = "a b", pull_request_path = "{owner}/pulls/{number}" }"#,
        ))
        .unwrap_err();
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(
            problems[3].contains("{name} must be a whole segment, once"),
            "{problems:#?}"
        );
    }

    #[test]
    fn exec_may_name_a_setting_and_env_names_variables() {
        let manifest = parse(
            &with(
                "exec = [[\"git\", \"credential\"]]",
                "exec = [[\"{config.program}\", \"api\"]]\nenv = [\"GH_HOST\"]",
            )
            .replace(
                "limit = {",
                "program = { type = \"string\", default = \"gh\" }\nlimit = {",
            ),
        )
        .unwrap();
        assert_eq!(manifest.permissions.env, ["GH_HOST"]);
        let (config, _) =
            manifest.resolve_config(&toml::from_str("program = \"/opt/gh\"").unwrap());
        assert_eq!(
            resolve_exec(&manifest.permissions.exec, &config),
            [["/opt/gh", "api"]]
        );
        let problems = parse(&with(
            "exec = [[\"git\", \"credential\"]]",
            "exec = [[\"{config.limit}\"]]\nenv = [\"DYLD_INSERT_LIBRARIES\", \"A-B\"]",
        ))
        .unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:#?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains("changes how every program loads"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("must declare limit = { type = \"string\" }"))
        );
    }
}
