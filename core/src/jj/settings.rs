use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result};
use jj_lib::{
    config::{ConfigLayer, ConfigSource, ConfigValue, StackedConfig},
    file_util::path_from_bytes,
    fileset::{
        FilesetAliasesMap, FilesetDiagnostics, FilesetExpression, FilesetParseContext,
        parse as parse_fileset,
    },
    gitignore::GitIgnoreFile,
    matchers::Matcher,
    repo_path::RepoPathUiConverter,
    settings::{HumanByteSize, UserSettings},
};

// Fallback used only when the user has not configured `snapshot.max-new-file-size`.
// Matches jj-cli's shipped default (1 MiB).
pub(super) const DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE: u64 = 1024 * 1024;

/// The slice of the process environment jj's config layering reads. Held as a
/// value rather than read from `std::env` at each use so the tests can drive
/// the loader directly — `set_var` is `unsafe` and would race every other test
/// in the binary.
#[derive(Clone, Debug, Default)]
pub(super) struct JjEnv {
    pub(super) vars: HashMap<String, String>,
    home_dir: Option<PathBuf>,
    /// Default for `operation.hostname`; see [`process_hostname`].
    hostname: Option<String>,
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
            hostname: process_hostname(),
        }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// jj-cli resolves its config dir through `etcetera::choose_base_strategy`,
    /// which is the XDG strategy on every unix — macOS included, where jj
    /// deliberately does *not* look in `~/Library/Application Support`.
    /// etcetera ignores a relative `XDG_CONFIG_HOME`, so we do too.
    fn config_dir(&self) -> Option<PathBuf> {
        let xdg = self
            .var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute());
        match xdg {
            Some(dir) => Some(dir),
            None => Some(self.home_dir.as_ref()?.join(".config")),
        }
    }

    /// `<config dir>/jj` — the directory jj 0.41 keeps per-repo config under.
    fn root_config_dir(&self) -> Option<PathBuf> {
        Some(self.config_dir()?.join("jj"))
    }

    /// The user config files and directories, lowest precedence first, exactly
    /// as jj-cli's `UnresolvedConfigEnv::resolve` orders them: the legacy
    /// `~/.jjconfig.toml`, then `<config dir>/jj/config.toml`, then
    /// `<config dir>/jj/conf.d`. Anything else sitting next to `config.toml`
    /// is not jj's to read — we used to load the whole directory, so one
    /// malformed stray `*.toml` failed every operation diffui ran.
    fn user_config_paths(&self) -> Vec<PathBuf> {
        if let Some(paths) = self.var("JJ_CONFIG") {
            return env::split_paths(paths)
                .filter(|path| !path.as_os_str().is_empty())
                .collect();
        }

        let mut paths = Vec::new();
        let home_config = self
            .home_dir
            .as_ref()
            .map(|home| home.join(".jjconfig.toml"));
        let config_dir = self.config_dir();
        let platform_config = config_dir
            .as_ref()
            .map(|dir| dir.join("jj").join("config.toml"));

        // The home file counts only when it exists — unless there is no config
        // dir at all, in which case it is the one place a config could live.
        if let Some(path) = home_config
            && (path.exists() || platform_config.is_none())
        {
            paths.push(path);
        }
        paths.extend(platform_config);
        if let Some(dir) = config_dir {
            let conf_d = dir.join("jj").join("conf.d");
            if conf_d.exists() {
                paths.push(conf_d);
            }
        }
        paths
    }
}

/// The machine's hostname, for `operation.hostname`. jj-cli reads it through
/// `whoami`; diffui-core has no equivalent dependency, so we take Linux's
/// procfs entry and fall back to the `hostname` command (macOS has no such
/// file but always ships `/bin/hostname`). Cached, because the alternative is
/// spawning a process on every snapshot tick — and without it every operation
/// diffui writes shows a blank host in `jj op log` while the CLI's shows one.
pub(super) fn process_hostname() -> Option<String> {
    static HOSTNAME: OnceLock<Option<String>> = OnceLock::new();
    fn non_empty(value: String) -> Option<String> {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    }
    HOSTNAME
        .get_or_init(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .and_then(non_empty)
                .or_else(|| {
                    let output = std::process::Command::new("hostname").output().ok()?;
                    non_empty(String::from_utf8(output.stdout).ok()?)
                })
        })
        .clone()
}

pub(super) const OP_HOSTNAME: &str = "operation.hostname";
pub(super) const OP_USERNAME: &str = "operation.username";

/// `ConfigLayer::set_value` only rejects a malformed config name or a value
/// with no TOML representation; every call below passes a literal name and a
/// plain scalar, so the error is unreachable (jj-cli unwraps it too).
pub(super) fn set_env_config_value(
    layer: &mut ConfigLayer,
    name: &'static str,
    value: impl Into<ConfigValue>,
) {
    layer
        .set_value(name, value)
        .expect("literal config name and scalar value");
}

/// jj-cli's `env_base_layer`: environment-derived values a config file is
/// still allowed to override. Its presentation-only entries (`$NO_COLOR`,
/// `$VISUAL`, `$EDITOR`) are left out — nothing in jj-lib reads `ui.color` or
/// `ui.editor`, they only drive the CLI's own output and editor launching.
pub(super) fn jj_env_base_layer(env: &JjEnv) -> ConfigLayer {
    let mut layer = ConfigLayer::empty(ConfigSource::EnvBase);
    if let Some(hostname) = &env.hostname {
        set_env_config_value(&mut layer, OP_HOSTNAME, hostname.as_str());
    }
    // jj-cli asks `whoami` first and falls back to `$USER`; that fallback is
    // all we have. launchd exports both `USER` and `LOGNAME` to a macOS GUI
    // app, so a bundled diffui still gets a name.
    if let Some(username) = env.var("USER").or_else(|| env.var("LOGNAME")) {
        set_env_config_value(&mut layer, OP_USERNAME, username);
    }
    layer
}

/// jj-cli's `env_overrides_layer`, minus `$JJ_EDITOR` (`ui.editor` again).
/// Without this a diffui commit ignores the `JJ_USER`/`JJ_EMAIL` a wrapper
/// script or a test harness set and signs the commit as whoever the config
/// file names instead.
pub(super) fn jj_env_overrides_layer(env: &JjEnv) -> ConfigLayer {
    let mut layer = ConfigLayer::empty(ConfigSource::EnvOverrides);
    for (name, key) in [
        ("JJ_USER", "user.name"),
        ("JJ_EMAIL", "user.email"),
        ("JJ_TIMESTAMP", "debug.commit-timestamp"),
        ("JJ_OP_TIMESTAMP", "debug.operation-timestamp"),
        ("JJ_OP_HOSTNAME", OP_HOSTNAME),
        ("JJ_OP_USERNAME", OP_USERNAME),
    ] {
        if let Some(value) = env.var(name) {
            set_env_config_value(&mut layer, key, value);
        }
    }
    if let Some(seed) = env
        .var("JJ_RANDOMNESS_SEED")
        .and_then(|value| value.parse::<i64>().ok())
    {
        set_env_config_value(&mut layer, "debug.randomness-seed", seed);
    }
    layer
}

pub(crate) fn jj_settings(repo_root: &Path) -> Result<UserSettings> {
    jj_settings_with_env(repo_root, &JjEnv::from_process())
}

pub(super) fn jj_settings_with_env(repo_root: &Path, env: &JjEnv) -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    // `add_layer` inserts by `ConfigSource` rank, so these sit below and above
    // the file layers regardless of the order we hand them over in.
    config.add_layer(jj_env_base_layer(env));
    config.add_layer(jj_env_overrides_layer(env));

    for path in env.user_config_paths() {
        load_jj_user_config_path(&mut config, &path)?;
    }

    // Through the `.jj/repo` pointer so a secondary workspace picks up the
    // primary repo's config. Best-effort: an unresolvable pointer just means
    // no repo-level config layer (Workspace::load will surface the breakage).
    if let Ok(repo_dir) = crate::repository::resolve_jj_repo_dir(repo_root) {
        // jj ≤ 0.40 kept the repo config inside the repo dir; jj 0.41 moved it
        // to `<user config dir>/repos/<config-id>/config.toml` with a
        // `config-id` pointer file in the repo dir. Honor both, or a repo
        // config written by a newer `jj config set --repo` is silently
        // invisible here (revsets.log, immutable_heads() overrides, …).
        let mut repo_config = repo_dir.join("config.toml");
        if !repo_config.is_file()
            && let Ok(id) = std::fs::read_to_string(repo_dir.join("config-id"))
        {
            let id = id.trim();
            if !id.is_empty()
                && id.chars().all(|c| c.is_ascii_alphanumeric())
                && let Some(root) = env.root_config_dir()
            {
                let candidate = root.join("repos").join(id).join("config.toml");
                if candidate.is_file() {
                    repo_config = candidate;
                }
            }
        }
        if repo_config.is_file() {
            config
                .load_file(ConfigSource::Repo, repo_config.clone())
                .with_context(|| {
                    format!("failed to load jj repo config {}", repo_config.display())
                })?;
        }
    }

    UserSettings::from_config(config).context("failed to build jj settings")
}

pub(super) fn load_jj_user_config_path(config: &mut StackedConfig, path: &Path) -> Result<()> {
    if path.is_dir() {
        config
            .load_dir(ConfigSource::User, path)
            .with_context(|| format!("failed to load jj config dir {}", path.display()))?;
    } else if path.is_file() {
        config
            .load_file(ConfigSource::User, path.to_path_buf())
            .with_context(|| format!("failed to load jj config file {}", path.display()))?;
    }
    Ok(())
}

pub(super) fn snapshot_max_new_file_size(settings: &UserSettings) -> Result<u64> {
    use jj_lib::config::ConfigGetError;
    match settings.get_value_with("snapshot.max-new-file-size", HumanByteSize::try_from) {
        Ok(size) => Ok(size.0),
        Err(ConfigGetError::NotFound { .. }) => Ok(DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE),
        Err(err) => Err(err).context("invalid snapshot.max-new-file-size"),
    }
}

pub(super) fn snapshot_auto_track_matcher(
    settings: &UserSettings,
    repo_root: &Path,
) -> Result<Box<dyn Matcher>> {
    use jj_lib::config::ConfigGetError;
    let raw = match settings.get_string("snapshot.auto-track") {
        Ok(value) => value,
        Err(ConfigGetError::NotFound { .. }) => "all()".to_string(),
        Err(err) => return Err(err).context("invalid snapshot.auto-track"),
    };
    let aliases = FilesetAliasesMap::new();
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
pub(super) fn snapshot_base_ignores(repo_root: &Path) -> Result<Arc<GitIgnoreFile>> {
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
            .chain_with_file("", excludes.clone())
            .with_context(|| format!("failed to read git excludes {}", excludes.display()))?;
    }

    if let Some(git_dir) = &git_dir {
        let info_exclude = git_dir.join("info").join("exclude");
        ignores = ignores
            .chain_with_file("", info_exclude.clone())
            .with_context(|| format!("failed to read {}", info_exclude.display()))?;
    }

    Ok(ignores)
}

/// The git dir behind a jj repo. `.jj/repo/store/git_target` holds the path
/// `GitBackend` opens, relative to the store dir: `git` for a repo jj owns,
/// `../../../.git` for a colocated one. Reading it is what gets `info/exclude`
/// right for both — `<root>/.git` exists only when colocated, so a
/// non-colocated repo's exclude file was previously never read. `None` for a
/// repo with no git backend, which has no git dir to consult.
pub(super) fn backing_git_dir(repo_root: &Path) -> Option<PathBuf> {
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

    /// An environment holding only what the test puts in it: no home, no
    /// hostname, and none of the variables the test runner happens to carry.
    fn test_env(vars: &[(&str, &str)]) -> JjEnv {
        JjEnv {
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            ..JjEnv::default()
        }
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

        let env = JjEnv {
            home_dir: Some(home),
            ..test_env(&[])
        };
        let settings =
            jj_settings_with_env(&root, &env).expect("a stray junk.toml must not fail the load");

        assert_eq!(
            settings.user_name(),
            "conf.d",
            "conf.d outranks config.toml"
        );
        assert_eq!(
            settings.user_email(),
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

        let mut env = JjEnv {
            home_dir: Some(home),
            hostname: Some("base-host".to_owned()),
            ..test_env(&[
                ("USER", "login-name"),
                ("JJ_USER", "Env User"),
                ("JJ_EMAIL", "env@example.com"),
            ])
        };

        let settings = jj_settings_with_env(&root, &env).expect("jj settings");
        assert_eq!(settings.user_name(), "Env User", "JJ_USER beats the file");
        assert_eq!(settings.user_email(), "env@example.com");
        assert_eq!(
            settings.operation_hostname(),
            "file-host",
            "the file beats the base env layer"
        );
        assert_eq!(
            settings.operation_username(),
            "login-name",
            "$USER is the default when nothing else names one"
        );

        env.vars
            .insert("JJ_OP_HOSTNAME".to_owned(), "op-host".to_owned());
        env.vars
            .insert("JJ_OP_USERNAME".to_owned(), "op-user".to_owned());
        let settings = jj_settings_with_env(&root, &env).expect("jj settings");
        assert_eq!(settings.operation_hostname(), "op-host");
        assert_eq!(settings.operation_username(), "op-user");
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

        assert!(ignores.matches("build.tmp"), "excludesFile patterns apply");
        assert!(!ignores.matches("build.rs"));
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

        assert!(ignores.matches("run.log"));
        assert!(!ignores.matches("run.rs"));
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
            assert!(ignores.matches("token.secret"), "{name} layout");
            assert!(!ignores.matches("token.rs"), "{name} layout");
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
