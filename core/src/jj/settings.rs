//! jj's config, read through jj-cli's own loader: its shipped defaults
//! (`revsets.log`, the revset aliases, the merge tools,
//! `templates.git_push_bookmark`, `snapshot.*`), its environment layers, its
//! system and user file lookup, `--when` scopes and config migrations.
//!
//! Ported, because jj's version writes or needs a command context. Check each
//! against jj-cli on a bump:
//! - `secure_config_file` ports jj-lib's `SecureConfig::maybe_load_config`
//!   (`lib/src/secure_config.rs`), read-only. jj's mints config ids, rewrites
//!   metadata and migrates legacy files, which a GUI opening a repo shouldn't.
//! - `snapshot_base_ignores` ports `WorkspaceCommandHelper::base_ignores`
//!   (`cli/src/cli_util.rs`).
//! - `snapshot_auto_track_matcher` and `snapshot_max_new_file_size` port
//!   `WorkspaceCommandHelper::auto_tracking_matcher` and
//!   `snapshot_options_with_start_tracking_matcher` (`cli/src/cli_util.rs`).

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use jj_cli::{
    command_error::CommandError,
    config::{
        ConfigEnv, config_from_environment, default_config_layers, default_config_migrations,
    },
    ui::Ui,
};
use jj_lib::{
    config::ConfigSource,
    file_util::path_from_bytes,
    fileset::{FilesetDiagnostics, FilesetExpression, FilesetParseContext, parse as parse_fileset},
    gitignore::GitIgnoreFile,
    matchers::Matcher,
    repo_path::RepoPath,
    settings::{HumanByteSize, UserSettings},
    ui_path::RepoPathUiConverter,
};

/// A jj-cli error as an `anyhow` one. jj-cli's own errors carry no `Display`
/// of their own; the message is the wrapped error and its sources.
pub(super) fn cli_error(error: CommandError) -> anyhow::Error {
    let mut message = error.error.to_string();
    let mut source = error.error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    anyhow!(message)
}

/// The slice of the process environment the git-ignore lookup reads. Held as
/// a value rather than read from `std::env` at each use so the tests can drive
/// it directly — `set_var` is `unsafe` and would race every other test in the
/// binary.
#[derive(Clone, Debug, Default)]
pub(super) struct JjEnv {
    pub(super) vars: HashMap<String, String>,
    home_dir: Option<PathBuf>,
}

impl JjEnv {
    fn from_process() -> Self {
        Self {
            // Non-Unicode variables are dropped rather than panicked on, the
            // way jj-cli's `ConfigEnv::from_environment` does it.
            vars: env::vars_os()
                .filter_map(|(name, value)| {
                    Some((name.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
            home_dir: env::home_dir(),
        }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }
}

/// The settings jj-cli would run a command with in the workspace at
/// `repo_root`, short of the command's own `--config` flags. jj-cli's
/// environment layers set `ui.editor`, `ui.color` and `ui.pager` too; nothing
/// in diffui reads them, so they ride along unused.
pub(crate) fn jj_settings(repo_root: &Path) -> Result<UserSettings> {
    let mut env = ConfigEnv::from_environment();
    let mut config = config_from_environment(default_config_layers());
    env.reload_system_config(&mut config)
        .context("failed to load the system jj config")?;
    env.reload_user_config(&mut config)
        .context("failed to load the user jj config")?;

    let files = JjConfigFiles::locate(repo_root, &env);
    if let Some(path) = &files.repo {
        config
            .as_mut()
            .load_file(ConfigSource::Repo, path.clone())
            .with_context(|| format!("failed to load jj repo config {}", path.display()))?;
    }
    if let Some(path) = &files.workspace {
        config
            .as_mut()
            .load_file(ConfigSource::Workspace, path.clone())
            .with_context(|| format!("failed to load jj workspace config {}", path.display()))?;
    }

    // What `--when.repositories` and `--when.workspaces` match. There is no
    // subcommand, so `--when.commands` scopes never match, the same as the CLI
    // before it has parsed one.
    if let Some(repo_dir) = &files.repo_dir {
        env.reset_repo_path(repo_dir);
    }
    env.reset_workspace_path(repo_root);
    let mut config = env
        .resolve_config(&config)
        .context("failed to resolve conditional jj config")?;
    jj_lib::config::migrate(&mut config, &default_config_migrations())
        .context("failed to migrate deprecated jj config")?;

    UserSettings::from_config(config).context("failed to build jj settings")
}

/// Every config file jj-cli would read for the workspace at `repo_root`,
/// found without writing anything: no config-id is minted and no legacy file
/// migrated, since that's the CLI's job and a GUI opening a repo shouldn't
/// rearrange the user's config directory.
#[derive(Debug, Default)]
pub(super) struct JjConfigFiles {
    system: Vec<PathBuf>,
    user: Vec<PathBuf>,
    repo: Option<PathBuf>,
    workspace: Option<PathBuf>,
    /// `<main workspace>/.jj/repo`, the path `--when.repositories` matches.
    repo_dir: Option<PathBuf>,
    /// Pointer files whose appearance or edit moves the repo/workspace config
    /// elsewhere, watched alongside the configs themselves.
    pointers: Vec<PathBuf>,
}

impl JjConfigFiles {
    pub(super) fn locate(repo_root: &Path, env: &ConfigEnv) -> Self {
        let mut files = JjConfigFiles {
            system: env
                .existing_system_config_paths()
                .map(Path::to_path_buf)
                .collect(),
            user: env.user_config_paths().map(Path::to_path_buf).collect(),
            ..JjConfigFiles::default()
        };
        // `<config dir>/jj`, which holds the `repos/` and `workspaces/` trees.
        let root_config_dir = env
            .repo_configs_root_dir()
            .and_then(|repos| Some(repos.parent()?.to_path_buf()));
        // Through the `.jj/repo` pointer so a secondary workspace picks up the
        // primary repo's config. Best-effort: an unresolvable pointer just
        // means no repo-level config layer (Workspace::load will surface the
        // breakage).
        if let Ok(repo_dir) = crate::repository::resolve_jj_repo_dir(repo_root) {
            files.repo = secure_config_file(
                root_config_dir.as_deref(),
                &repo_dir,
                "config-id",
                "config.toml",
                "repos",
                &mut files.pointers,
            );
            files.repo_dir = Some(repo_dir);
        }
        files.workspace = secure_config_file(
            root_config_dir.as_deref(),
            &repo_root.join(".jj"),
            "workspace-config-id",
            "workspace-config.toml",
            "workspaces",
            &mut files.pointers,
        );
        files
    }

    /// Every path whose change should reload the settings: the config files
    /// and directories (and each file inside a directory), plus the pointers.
    fn watched(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::new();
        for path in self.system.iter().chain(&self.user) {
            paths.push(path.clone());
            if path.is_dir()
                && let Ok(entries) = std::fs::read_dir(path)
            {
                let mut inside: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .collect();
                inside.sort();
                paths.extend(inside);
            }
        }
        paths.extend(self.repo.iter().cloned());
        paths.extend(self.workspace.iter().cloned());
        paths.extend(self.pointers.iter().cloned());
        paths
    }
}

/// jj 0.45's `SecureConfig` lookup, read-only: a `<id file>` in `dir` names
/// `<config dir>/jj/<root>/<id>/config.toml`; without one, the legacy file in
/// `dir` itself is read (jj ≤ 0.40 kept it there, and jj 0.41+ leaves a symlink
/// behind when it migrates). `None` when neither exists.
fn secure_config_file(
    root_config_dir: Option<&Path>,
    dir: &Path,
    id_name: &str,
    legacy_name: &str,
    root_name: &str,
    pointers: &mut Vec<PathBuf>,
) -> Option<PathBuf> {
    const CONFIG_ID_LEN: usize = 20;
    let id_path = dir.join(id_name);
    let legacy = dir.join(legacy_name);
    pointers.push(id_path.clone());
    pointers.push(legacy.clone());
    if let Ok(id) = std::fs::read_to_string(&id_path) {
        // jj rejects anything but a 20-hex-digit id; a bad pointer means no
        // config rather than a path built from junk.
        if id.len() == CONFIG_ID_LEN && id.chars().all(|c| c.is_ascii_hexdigit()) {
            let candidate = root_config_dir?
                .join(root_name)
                .join(id)
                .join("config.toml");
            pointers.push(candidate.clone());
            return candidate.is_file().then_some(candidate);
        }
        return None;
    }
    legacy.is_file().then_some(legacy)
}

/// A cheap fingerprint of every jj config file the settings were built from:
/// path, size and mtime, plus the pointer files that decide which files those
/// are. Comparing two stamps is a handful of `stat`s, cheap enough to pay
/// before every mutation and refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JjConfigStamp(Vec<(PathBuf, Option<(u64, std::time::SystemTime)>)>);

pub(crate) fn jj_config_stamp(repo_root: &Path) -> JjConfigStamp {
    JjConfigStamp(
        JjConfigFiles::locate(repo_root, &ConfigEnv::from_environment())
            .watched()
            .into_iter()
            .map(|path| {
                let meta = std::fs::metadata(&path)
                    .ok()
                    .and_then(|meta| Some((meta.len(), meta.modified().ok()?)));
                (path, meta)
            })
            .collect(),
    )
}

/// `snapshot.max-new-file-size`, where 0 means no limit, as in jj-cli.
pub(super) fn snapshot_max_new_file_size(settings: &UserSettings) -> Result<u64> {
    let HumanByteSize(size) = settings
        .get_value_with("snapshot.max-new-file-size", HumanByteSize::try_from)
        .context("invalid snapshot.max-new-file-size")?;
    Ok(if size == 0 { u64::MAX } else { size })
}

pub(super) fn snapshot_auto_track_matcher(
    settings: &UserSettings,
    repo_root: &Path,
) -> Result<Box<dyn Matcher>> {
    let raw = settings
        .get_string("snapshot.auto-track")
        .context("invalid snapshot.auto-track")?;
    let aliases = jj_cli::cli_util::load_fileset_aliases(&Ui::null(), settings.config())
        .map_err(cli_error)?;
    let path_converter = RepoPathUiConverter::Fs {
        cwd: repo_root.to_path_buf(),
        base: repo_root.to_path_buf(),
    };
    let context = FilesetParseContext {
        aliases_map: &aliases,
        path_converter: &path_converter,
    };
    let mut diagnostics = FilesetDiagnostics::new();
    let expr: FilesetExpression = parse_fileset(&mut diagnostics, &raw, &context)
        .with_context(|| format!("failed to parse snapshot.auto-track {raw:?}"))?;
    Ok(expr.to_matcher())
}

// `LocalWorkingCopy` walks the repo tree and reads in-tree `.gitignore` files
// itself, so we only need to provide the *out-of-tree* ignores — the same two
// jj-cli's `base_ignores` collects: whatever `core.excludesFile` points at
// (git's default location when unset) and `info/exclude` from the git dir
// behind the repo.
pub(crate) fn snapshot_base_ignores(repo_root: &Path) -> Result<Arc<GitIgnoreFile>> {
    snapshot_base_ignores_with_env(repo_root, &JjEnv::from_process())
}

pub(super) fn snapshot_base_ignores_with_env(
    repo_root: &Path,
    env: &JjEnv,
) -> Result<Arc<GitIgnoreFile>> {
    let mut ignores = GitIgnoreFile::empty();
    let git_dir = backing_git_dir(repo_root);

    if let Some(excludes) = git_excludes_file(repo_root, git_dir.as_deref(), env) {
        ignores = ignores
            .chain_with_file(RepoPath::root(), excludes.clone())
            .with_context(|| format!("failed to read git excludes {}", excludes.display()))?;
    }

    if let Some(git_dir) = &git_dir {
        let info_exclude = git_dir.join("info").join("exclude");
        ignores = ignores
            .chain_with_file(RepoPath::root(), info_exclude.clone())
            .with_context(|| format!("failed to read {}", info_exclude.display()))?;
    }

    Ok(ignores)
}

/// Whether `path` is ignored, either itself or through an ignored ancestor.
///
/// jj-lib's `matches_file`/`matches_dir` test the exact path only and leave
/// parent recursion to the caller, because its snapshotter never descends into
/// an ignored directory. Callers here hold arbitrary paths — a watcher event
/// from deep inside `target/`, an entry of an ignored directory that still has
/// tracked files — so the ancestors are walked for them.
pub(crate) fn is_ignored(ignores: &GitIgnoreFile, path: &RepoPath, is_dir: bool) -> bool {
    let matched = if is_dir {
        ignores.matches_dir(path)
    } else {
        ignores.matches_file(path)
    };
    matched || path.ancestors().skip(1).any(|dir| ignores.matches_dir(dir))
}

/// The git dir behind a jj repo. `.jj/repo/store/git_target` holds the path
/// `GitBackend` opens, relative to the store dir: `git` for a repo jj owns,
/// `../../../.git` for a colocated one. Reading it is what gets `info/exclude`
/// right for both — `<root>/.git` exists only when colocated, so a
/// non-colocated repo's exclude file was previously never read. `None` for a
/// repo with no git backend, which has no git dir to consult.
pub(crate) fn backing_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let store = crate::repository::resolve_jj_repo_dir(repo_root)
        .ok()?
        .join("store");
    let target = std::fs::read(store.join("git_target")).ok()?;
    store
        .join(path_from_bytes(&target).ok()?)
        .canonicalize()
        .ok()
}

/// The path git would read ignore patterns from outside the tree:
/// `core.excludesFile` if any config file sets it, else git's default
/// `$XDG_CONFIG_HOME/git/ignore`. The old code always assumed the default, so
/// a user who pointed `core.excludesFile` elsewhere had those patterns
/// dropped — and with `snapshot.auto-track = all()` diffui then tracked, on
/// every tick, files jj itself ignores.
pub(super) fn git_excludes_file(
    work_dir: &Path,
    git_dir: Option<&Path>,
    env: &JjEnv,
) -> Option<PathBuf> {
    let configured = git_config_files(git_dir, env)
        .iter()
        .rev()
        .find_map(|path| git_config_excludes_file(&std::fs::read_to_string(path).ok()?));
    match configured {
        // git reads a relative excludes path from the work tree; `join` on an
        // absolute one keeps it as-is.
        Some(value) => Some(work_dir.join(expand_home_dir(env, &value))),
        None => xdg_config_home(env).map(|dir| dir.join("git").join("ignore")),
    }
}

/// The git config files that can carry `core.excludesFile`, lowest precedence
/// first: system, then global, then the repo's own. A later assignment wins,
/// which is why the caller searches this list back to front.
pub(super) fn git_config_files(git_dir: Option<&Path>, env: &JjEnv) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !env.var("GIT_CONFIG_NOSYSTEM").is_some_and(git_config_bool) {
        files.push(
            env.var("GIT_CONFIG_SYSTEM")
                .map_or_else(|| PathBuf::from("/etc/gitconfig"), PathBuf::from),
        );
    }
    match env.var("GIT_CONFIG_GLOBAL") {
        Some(path) => files.push(PathBuf::from(path)),
        None => {
            files.extend(xdg_config_home(env).map(|dir| dir.join("git").join("config")));
            files.extend(env.home_dir.as_ref().map(|home| home.join(".gitconfig")));
        }
    }
    files.extend(git_dir.map(|dir| dir.join("config")));
    files
}

/// git's own truthiness for the `GIT_CONFIG_*` switches.
pub(super) fn git_config_bool(value: &str) -> bool {
    ["1", "true", "yes", "on"]
        .iter()
        .any(|truthy| value.eq_ignore_ascii_case(truthy))
}

/// `$XDG_CONFIG_HOME`, or `~/.config`. jj-cli's `base_ignores` accepts a
/// relative value here, unlike the etcetera-backed config dir behind
/// [`JjEnv::config_dir`], so this deliberately isn't the same lookup.
pub(super) fn xdg_config_home(env: &JjEnv) -> Option<PathBuf> {
    if let Some(dir) = env.var("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    env.home_dir.as_ref().map(|home| home.join(".config"))
}

/// `jj_lib::file_util::expand_home_path` against our captured environment
/// rather than the process's, so the tests stay off the real home directory.
pub(super) fn expand_home_dir(env: &JjEnv, value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = &env.home_dir
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

/// The last `core.excludesFile` assigned in one git config file's text.
///
/// Deliberately only as much of git's syntax as a path assignment needs —
/// section headers, quoted values, escapes, trailing comments. `include.path`
/// and conditional includes are not followed; a value reachable only through
/// one leaves us on git's default excludes path, which is where every repo
/// was before this lookup existed.
pub(super) fn git_config_excludes_file(text: &str) -> Option<String> {
    let mut value = None;
    let mut in_core = false;
    for line in text.lines() {
        let mut rest = line.trim();
        if let Some(end) = git_config_section_end(rest) {
            in_core = rest[1..end].trim().eq_ignore_ascii_case("core");
            rest = rest[end + 1..].trim_start();
        }
        if !in_core || rest.starts_with(['#', ';']) {
            continue;
        }
        let Some((key, raw)) = rest.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("excludesfile") {
            value = Some(git_config_value(raw.trim_start()));
        }
    }
    value.filter(|value| !value.is_empty())
}

/// Byte offset of the `]` closing a `[section]` header, quoted subsection
/// names included. `None` when the line doesn't open one.
pub(super) fn git_config_section_end(line: &str) -> Option<usize> {
    let mut chars = line.char_indices();
    if chars.next()?.1 != '[' {
        return None;
    }
    let mut quoted = false;
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' if quoted => {
                chars.next();
            }
            '"' => quoted = !quoted,
            ']' if !quoted => return Some(index),
            _ => {}
        }
    }
    None
}

/// Unescape one git config value: quotes drop out, `\n`/`\t`/`\b` and escaped
/// literals resolve, and an unquoted `#`/`;` starts a comment. Whitespace
/// after the last quoted or non-blank character is trailing.
pub(super) fn git_config_value(raw: &str) -> String {
    let mut value = String::new();
    let mut end = 0;
    let mut quoted = false;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some('n') => value.push('\n'),
                Some('t') => value.push('\t'),
                Some('b') => value.push('\u{8}'),
                Some(escaped) => value.push(escaped),
                None => break,
            },
            '#' | ';' if !quoted => break,
            _ => value.push(c),
        }
        if quoted || !c.is_whitespace() {
            end = value.len();
        }
    }
    value.truncate(end);
    value
}

#[cfg(test)]
mod config_layering_tests {
    use std::path::PathBuf;
    use std::process::Command;

    use jj_lib::config::StackedConfig;

    use super::*;

    /// A scratch directory wiped on entry, so a rerun starts from nothing.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("diffui-core-jj-config-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_file(path: PathBuf, contents: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dir");
        std::fs::write(&path, contents).expect("write scratch file");
        path
    }

    fn repo_path(value: &str) -> &RepoPath {
        RepoPath::from_internal_string(value).expect("valid repo path")
    }

    fn utf8(path: &Path) -> &str {
        path.to_str().expect("utf-8 scratch path")
    }

    /// An environment holding only what the test puts in it: no home and
    /// none of the variables the test runner happens to carry.
    fn test_env(vars: &[(&str, &str)]) -> JjEnv {
        JjEnv {
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            ..JjEnv::default()
        }
    }

    const PROBE_ROOT: &str = "DIFFUI_SETTINGS_PROBE_ROOT";
    const PROBE_KEYS: &str = "DIFFUI_SETTINGS_PROBE_KEYS";
    const PROBE_MARK: &str = "diffui-settings-probe:";

    /// What [`settings_probe`] read in its child process: each asked-for
    /// config value that exists, plus the config stamp under `stamp`.
    struct Probe(HashMap<String, String>);

    impl Probe {
        fn get(&self, key: &str) -> &str {
            self.0
                .get(key)
                .unwrap_or_else(|| panic!("{key} is unset: {:?}", self.0))
        }
    }

    /// Load the settings for `repo_root` in a child copy of this test binary
    /// whose environment is exactly `vars`. jj-cli reads `$HOME`,
    /// `$XDG_CONFIG_HOME`, `$JJ_CONFIG` and the `JJ_*` overrides straight from
    /// the process environment, and `set_var` would race every other test.
    fn probe(repo_root: &Path, vars: &[(&str, &str)], keys: &[&str]) -> Probe {
        let path = concat!(module_path!(), "::settings_probe");
        let test = path.split_once("::").map_or(path, |(_, test)| test);
        let output = Command::new(env::current_exe().expect("the test binary"))
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env_clear()
            .envs(vars.iter().copied())
            .env(PROBE_ROOT, repo_root)
            .env(PROBE_KEYS, keys.join(","))
            .output()
            .expect("run the settings probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the settings probe failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Probe(
            stdout
                .lines()
                .filter_map(|line| line.split_once(PROBE_MARK)?.1.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
    }

    /// The child half of [`probe`]; does nothing in an ordinary test run.
    #[test]
    fn settings_probe() {
        let Some(root) = env::var_os(PROBE_ROOT) else {
            return;
        };
        let root = PathBuf::from(root);
        let keys = env::var(PROBE_KEYS).unwrap_or_default();
        let settings = jj_settings(&root).expect("jj settings");
        for key in keys.split(',').filter(|key| !key.is_empty()) {
            let name: jj_lib::config::ConfigNamePathBuf = key.parse().expect("a config name");
            if let Ok(value) = settings.get_value(&name) {
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                println!("{PROBE_MARK}{key}={value}");
            }
        }
        println!("{PROBE_MARK}stamp={:?}", jj_config_stamp(&root));
    }

    /// jj-cli reads exactly `config.toml` and `conf.d/*.toml` from its config
    /// dir, lowest precedence first: `~/.jjconfig.toml`, then `config.toml`,
    /// then `conf.d`. Everything else in that directory is none of jj's
    /// business — `junk.toml` here isn't even valid TOML, and loading the whole
    /// directory (what we used to do) made one such stray file fail every
    /// operation diffui ran.
    #[test]
    fn user_config_layers_match_the_cli() {
        let root = scratch_dir("user-layers");
        let home = root.join("home");
        let config_dir = home.join(".config").join("jj");
        write_file(
            home.join(".jjconfig.toml"),
            "[user]\nname = \"home\"\nemail = \"home@example.com\"\n",
        );
        write_file(
            config_dir.join("config.toml"),
            "[user]\nname = \"config\"\n",
        );
        write_file(
            config_dir.join("conf.d").join("10-name.toml"),
            "[user]\nname = \"conf.d\"\n",
        );
        write_file(config_dir.join("junk.toml"), "this is = = not toml\n");

        let settings = probe(
            &root,
            &[("HOME", utf8(&home))],
            &["user.name", "user.email"],
        );

        assert_eq!(
            settings.get("user.name"),
            "conf.d",
            "conf.d outranks config.toml"
        );
        assert_eq!(
            settings.get("user.email"),
            "home@example.com",
            "~/.jjconfig.toml still supplies what no higher layer sets"
        );
    }

    /// The two environment layers jj-cli brackets its config files with: the
    /// base one below (`operation.hostname`/`operation.username` defaults a
    /// config file may override) and the override one above (`JJ_*`). Without
    /// them diffui's operations land in `jj op log` with a blank host and user,
    /// and `JJ_USER`/`JJ_EMAIL` are ignored where the CLI would honour them.
    #[test]
    fn env_layers_bracket_the_user_config() {
        let root = scratch_dir("env-layers");
        let home = root.join("home");
        write_file(
            home.join(".config").join("jj").join("config.toml"),
            "[user]\nname = \"file\"\nemail = \"file@example.com\"\n\
             [operation]\nhostname = \"file-host\"\n",
        );
        let keys = [
            "user.name",
            "user.email",
            "operation.hostname",
            "operation.username",
        ];
        let mut vars = vec![
            ("HOME", utf8(&home)),
            ("JJ_USER", "Env User"),
            ("JJ_EMAIL", "env@example.com"),
        ];

        let settings = probe(&root, &vars, &keys);
        assert_eq!(
            settings.get("user.name"),
            "Env User",
            "JJ_USER beats the file"
        );
        assert_eq!(settings.get("user.email"), "env@example.com");
        assert_eq!(
            settings.get("operation.hostname"),
            "file-host",
            "the file beats the base env layer"
        );
        assert!(
            !settings.get("operation.username").is_empty(),
            "the login name is the default when nothing else names one"
        );

        vars.extend([("JJ_OP_HOSTNAME", "op-host"), ("JJ_OP_USERNAME", "op-user")]);
        let settings = probe(&root, &vars, &keys);
        assert_eq!(settings.get("operation.hostname"), "op-host");
        assert_eq!(settings.get("operation.username"), "op-user");
    }

    /// A `[[--scope]]` table applies only where its `--when` holds: here a
    /// work identity for every repo under `work/`. diffui used to load the
    /// file and never resolve the scopes, so every repo committed — and
    /// pushed — under the default identity.
    #[test]
    fn when_repositories_scopes_apply_to_matching_repos_only() {
        let root = scratch_dir("when-repositories");
        let home = root.join("home");
        let work_repo = root.join("work").join("project");
        let personal_repo = root.join("personal").join("project");
        for repo in [&work_repo, &personal_repo] {
            std::fs::create_dir_all(repo.join(".jj").join("repo")).expect("create repo dir");
        }
        let vars = [("HOME", utf8(&home))];
        // `--when.hostnames` matches the machine's own name.
        let hostname = probe(&root, &vars, &["operation.hostname"])
            .get("operation.hostname")
            .to_owned();
        write_file(
            home.join(".config").join("jj").join("config.toml"),
            &format!(
                "[user]\nname = \"Me\"\nemail = \"me@personal.example\"\n\n\
                 [[--scope]]\n--when.repositories = [\"{}\"]\n\
                 [--scope.user]\nemail = \"me@work.example\"\n\n\
                 [[--scope]]\n--when.hostnames = [\"{hostname}\"]\n\
                 [--scope.operation]\nusername = \"ci\"\n",
                root.join("work").display()
            ),
        );
        let keys = ["user.name", "user.email", "operation.username"];

        let work = probe(&work_repo, &vars, &keys);
        assert_eq!(work.get("user.email"), "me@work.example");
        assert_eq!(work.get("user.name"), "Me", "unscoped keys still apply");
        assert_eq!(
            work.get("operation.username"),
            "ci",
            "--when.hostnames matches"
        );

        let personal = probe(&personal_repo, &vars, &keys);
        assert_eq!(personal.get("user.email"), "me@personal.example");
    }

    /// jj 0.45 keeps repo and workspace config outside the repo, behind
    /// `config-id` / `workspace-config-id` pointers, and the workspace layer
    /// outranks the repo one. Neither pointer is created or migrated here.
    #[test]
    fn repo_and_workspace_configs_follow_their_config_ids() {
        let root = scratch_dir("secure-config");
        let home = root.join("home");
        let repo = root.join("repo");
        let repo_dir = repo.join(".jj").join("repo");
        let configs = home.join(".config").join("jj");
        write_file(repo_dir.join("config-id"), "0123456789abcdef0123");
        write_file(
            repo.join(".jj").join("workspace-config-id"),
            "fedcba9876543210fedc",
        );
        write_file(
            configs.join("repos/0123456789abcdef0123/config.toml"),
            "[user]\nname = \"repo\"\nemail = \"repo@example.com\"\n",
        );
        write_file(
            configs.join("workspaces/fedcba9876543210fedc/config.toml"),
            "[user]\nname = \"workspace\"\n",
        );

        let settings = probe(
            &repo,
            &[("HOME", utf8(&home))],
            &["user.name", "user.email"],
        );
        assert_eq!(settings.get("user.name"), "workspace");
        assert_eq!(settings.get("user.email"), "repo@example.com");
        assert!(
            !repo.join(".jj").join("workspace-config.toml").exists(),
            "loading the settings writes nothing"
        );
    }

    /// The stamp moves when a config file the settings came from is edited,
    /// and when one appears where jj would look for it.
    #[test]
    fn the_config_stamp_notices_edits_and_new_files() {
        let root = scratch_dir("config-stamp");
        let home = root.join("home");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".jj").join("repo")).expect("create repo dir");
        let user = write_file(
            home.join(".config").join("jj").join("config.toml"),
            "[user]\nname = \"one\"\n",
        );
        let vars = [("HOME", utf8(&home))];
        let stamp = || probe(&repo, &vars, &[]).get("stamp").to_owned();

        let first = stamp();
        assert_eq!(first, stamp(), "stable");
        write_file(user, "[user]\nname = \"two, longer\"\n");
        let second = stamp();
        assert_ne!(first, second, "an edit moves the stamp");
        write_file(
            home.join(".config")
                .join("jj")
                .join("conf.d")
                .join("x.toml"),
            "[ui]\n",
        );
        assert_ne!(second, stamp(), "a new conf.d file moves the stamp");
    }

    /// The defaults are jj-cli's shipped config, not copies of it: what a
    /// repo with no config of its own reads is exactly what jj-cli ships.
    #[test]
    fn defaults_are_jj_clis_shipped_config() {
        let root = scratch_dir("cli-defaults");
        let home = root.join("home");
        let keys = [
            "revsets.log",
            "templates.git_push_bookmark",
            "snapshot.max-new-file-size",
            "snapshot.auto-track",
            "git.private-commits",
            "merge-tools.vscode.program",
        ];
        let settings = probe(&root, &[("HOME", utf8(&home))], &keys);

        let mut shipped = StackedConfig::empty();
        shipped.extend_layers(default_config_layers());
        for key in keys {
            let name: jj_lib::config::ConfigNamePathBuf = key.parse().expect("a config name");
            let value = shipped.get_value(&name).expect("jj-cli ships the key");
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
            assert_eq!(settings.get(key), value, "{key}");
        }
        assert_eq!(settings.get("revsets.log"), "builtin_log()");
    }

    /// `$JJ_CONFIG` replaces the user files (and the system ones) with the
    /// paths it lists, as in the CLI.
    #[test]
    fn jj_config_replaces_the_user_config_files() {
        let root = scratch_dir("jj-config-env");
        let home = root.join("home");
        write_file(
            home.join(".config").join("jj").join("config.toml"),
            "[user]\nname = \"default location\"\n",
        );
        let elsewhere = write_file(
            root.join("elsewhere.toml"),
            "[user]\nname = \"elsewhere\"\n",
        );

        let settings = probe(
            &root,
            &[("HOME", utf8(&home)), ("JJ_CONFIG", utf8(&elsewhere))],
            &["user.name"],
        );
        assert_eq!(settings.get("user.name"), "elsewhere");
    }

    #[test]
    fn a_zero_max_new_file_size_means_no_limit() {
        let settings = |size: &str| {
            let mut config = StackedConfig::with_defaults();
            config.extend_layers(default_config_layers());
            config.add_layer(
                jj_lib::config::ConfigLayer::parse(
                    ConfigSource::User,
                    &format!("snapshot.max-new-file-size = {size}\n"),
                )
                .expect("parse"),
            );
            UserSettings::from_config(config).expect("settings")
        };
        assert_eq!(
            snapshot_max_new_file_size(&settings("0")).unwrap(),
            u64::MAX
        );
        assert_eq!(
            snapshot_max_new_file_size(&settings("\"2KiB\"")).unwrap(),
            2048
        );
    }

    /// `core.excludesFile` decides where git — and therefore jj — reads
    /// out-of-tree ignore patterns from. We used to assume git's default
    /// location unconditionally, so a repo whose config moved the file had
    /// every pattern in it ignored.
    #[test]
    fn base_ignores_follow_core_excludes_file() {
        let root = scratch_dir("excludes-file");
        let home = root.join("home");
        write_file(home.join("my-ignores"), "*.tmp\n");
        write_file(
            home.join(".gitconfig"),
            "[core]\n\texcludesFile = ~/my-ignores # where the patterns live\n",
        );

        let env = JjEnv {
            home_dir: Some(home),
            ..test_env(&[(
                "GIT_CONFIG_SYSTEM",
                root.join("absent").to_str().expect("utf-8 scratch path"),
            )])
        };
        let ignores = snapshot_base_ignores_with_env(&root, &env).expect("base ignores");

        assert!(
            ignores.matches_file(repo_path("build.tmp")),
            "excludesFile patterns apply"
        );
        assert!(!ignores.matches_file(repo_path("build.rs")));
    }

    /// With `core.excludesFile` unset, git falls back to
    /// `$XDG_CONFIG_HOME/git/ignore` — and so must we.
    #[test]
    fn base_ignores_fall_back_to_gits_default_excludes_path() {
        let root = scratch_dir("excludes-default");
        let xdg = root.join("xdg");
        write_file(xdg.join("git").join("ignore"), "*.log\n");

        let env = test_env(&[
            ("XDG_CONFIG_HOME", xdg.to_str().expect("utf-8 scratch path")),
            (
                "GIT_CONFIG_SYSTEM",
                root.join("absent").to_str().expect("utf-8 scratch path"),
            ),
        ]);
        let ignores = snapshot_base_ignores_with_env(&root, &env).expect("base ignores");

        assert!(ignores.matches_file(repo_path("run.log")));
        assert!(!ignores.matches_file(repo_path("run.rs")));
    }

    /// `info/exclude` has to come from the git dir `store/git_target` names,
    /// not from `<root>/.git`: the latter exists only in a colocated repo, so
    /// a repo jj owns outright never had its exclude file read at all.
    #[test]
    fn base_ignores_read_info_exclude_from_either_git_dir_layout() {
        let env = test_env(&[(
            "GIT_CONFIG_SYSTEM",
            std::env::temp_dir()
                .join("diffui-core-absent-gitconfig")
                .to_str()
                .expect("utf-8 temp path"),
        )]);

        for (name, git_target, git_dir) in [
            ("internal", "git", ".jj/repo/store/git"),
            ("colocated", "../../../.git", ".git"),
        ] {
            let root = scratch_dir(&format!("info-exclude-{name}"));
            write_file(root.join(".jj/repo/store/git_target"), git_target);
            write_file(
                root.join(git_dir).join("info").join("exclude"),
                "*.secret\n",
            );

            let ignores = snapshot_base_ignores_with_env(&root, &env).expect("base ignores");
            assert!(
                ignores.matches_file(repo_path("token.secret")),
                "{name} layout"
            );
            assert!(
                !ignores.matches_file(repo_path("token.rs")),
                "{name} layout"
            );
        }
    }

    #[test]
    fn git_config_excludes_file_reads_gits_syntax() {
        let lookup = |text: &str| super::git_config_excludes_file(text);

        assert_eq!(
            lookup("[core]\n\texcludesfile = ~/.gitignore\n").as_deref(),
            Some("~/.gitignore")
        );
        assert_eq!(
            lookup("[CORE] excludesFile = \"/a b/ignore\" ; note\n").as_deref(),
            Some("/a b/ignore"),
            "section and key are case-insensitive, quotes and comments strip"
        );
        assert_eq!(
            lookup(
                "[core]\nexcludesfile = /first\n[user]\nname = x\n[core]\nexcludesfile = /last\n"
            )
            .as_deref(),
            Some("/last"),
            "the last assignment wins"
        );
        assert_eq!(
            lookup("[core \"sub\"]\nexcludesfile = /sub\n"),
            None,
            "a subsection is not the `core` section"
        );
        assert_eq!(lookup("[core]\n# excludesfile = /commented\n"), None);
    }
}
