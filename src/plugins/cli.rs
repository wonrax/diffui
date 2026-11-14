//! `diffui plugin …`: start a plugin, check one without the app, list what
//! is installed. Written for agents as much as for people: every problem is
//! one line with its `file:line:col`, and `check` exits non-zero on any.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use diffui_core::cli::tokenize::shell_quote;
use diffui_plugins::protocol::{FORGE_METHODS, HostCall};
use diffui_plugins::{
    API_VERSION, ForgeTerm, Host, JjAccess, Limits, LogLevel, MANIFEST_FILE, Manifest, Options,
    Plugin, PluginEvent, Registration, Reply,
};

use super::{commands_of, placed, plugin_dirs, plugins_dir, trust};
use crate::config::display_path;

/// `diffui plugin …`, when that is how the process was started.
pub(crate) fn client_main() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    (args.next()?.to_str()? == "plugin").then(plugin_main)
}

#[derive(Debug, clap::Parser)]
#[command(
    name = "diffui plugin",
    about = "Write and check diffui plugins (see `diffui skill`)"
)]
struct PluginCli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Debug, clap::Subcommand)]
enum Verb {
    /// Start a plugin: plugin.toml, a main module, the API's types and an
    /// editor config, in the plugins folder
    Init {
        /// The plugin's id: lowercase letters, digits and dashes
        id: String,
        /// Write main.ts instead of main.js
        #[arg(long)]
        ts: bool,
        /// Make it a forge for remotes on this host (codeberg.org)
        #[arg(long, value_name = "HOST")]
        forge: Option<String>,
        /// Where to write it instead of the plugins folder
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Load a plugin without the app: its manifest's problems, what its
    /// script registers, and why it doesn't load if it doesn't
    Check {
        /// The plugin's directory, or the id of one in the plugins folder
        /// (the current directory when left out)
        plugin: Option<PathBuf>,
    },
    /// The plugins in the plugins folder, and whether each is allowed
    List,
}

fn plugin_main() -> i32 {
    use clap::Parser as _;
    let cli = PluginCli::parse_from(
        std::iter::once(std::ffi::OsString::from("diffui plugin"))
            .chain(std::env::args_os().skip(2)),
    );
    let done = match cli.verb {
        Verb::Init { id, ts, forge, dir } => {
            let dir = match dir.or_else(|| Some(plugins_dir()?.join(&id))) {
                Some(dir) => dir,
                None => return fail("no home directory to put the plugins folder in"),
            };
            init(&id, ts, forge.as_deref(), &dir)
        }
        Verb::Check { plugin } => plugin_dir(
            &plugin.unwrap_or_else(|| PathBuf::from(".")),
            plugins_dir().as_deref(),
        )
        .and_then(|dir| check(&dir)),
        Verb::List => Ok(list()),
    };
    match done {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(text) => fail(text.trim_end()),
    }
}

fn fail(text: &str) -> i32 {
    eprintln!("{text}");
    1
}

/// A name for `id`: `my-forge` reads "My forge".
pub(super) fn name_of(id: &str) -> String {
    let words = id.replace('-', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => words,
    }
}

/// What `init` writes as `plugin.toml`.
fn manifest_text(id: &str, main: &str, forge: Option<&str>) -> String {
    let name = name_of(id);
    let mut text = format!(
        "id = \"{id}\"\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         api = {API_VERSION}\n\
         main = \"{main}\"\n\
         \n\
         # What the plugin may do. diffui asks the user before the plugin first runs,\n\
         # and again when this asks for more.\n\
         [permissions]\n"
    );
    match forge {
        Some(host) => {
            let _ = writeln!(
                text,
                "http = [\"{host}\"]            # hosts http.fetch and auth.token may use"
            );
        }
        None => text.push_str(
            "# http = [\"example.com\"]        # hosts http.fetch and auth.token may use\n",
        ),
    }
    text.push_str(
        "# exec = [[\"git\", \"config\"]]     # argv prefixes exec may run\n\
         jj = \"read\"                     # \"read\": repo reads and repo.jj.query. \"write\": also repo.jj.run\n",
    );
    if let Some(host) = forge {
        let _ = write!(
            text,
            "\n\
             # Remotes on these hosts are this plugin's: main must call forge.register.\n\
             [provides]\n\
             forge = {{ hosts = [\"{host}\"] }}\n"
        );
    }
    let _ = write!(
        text,
        "\n\
         # A command: main must call commands.register with this id.\n\
         [[commands]]\n\
         id = \"{id}.hello\"\n\
         label = \"{name}: say hello\"\n\
         menu = [\"palette\", \"revision\"]   # palette | revision | bookmark | file\n\
         # when = \"repo.kind == 'jj'\"      # a when-clause; see `diffui skill`\n\
         # keys = [\"cmd+shift+h\"]\n"
    );
    text
}

/// What `init` writes as the main module.
fn main_text(id: &str, ts: bool, forge: bool) -> String {
    let mut imports = vec!["commands", "ui"];
    if forge {
        imports.extend(["auth", "forge", "http"]);
    }
    imports.sort_unstable();
    let mut text = String::new();
    if ts {
        let _ = writeln!(
            text,
            "import {{ {}, type CommandContext }} from \"diffui\";\n",
            imports.join(", ")
        );
    } else {
        let _ = writeln!(
            text,
            "// @ts-check\n/// <reference path=\"./diffui.d.ts\" />\nimport {{ {} }} from \"diffui\";\n",
            imports.join(", ")
        );
    }
    let context = if ts {
        "(context: CommandContext)"
    } else {
        "(context)"
    };
    let _ = write!(
        text,
        "// Runs when the user picks the command. `context.repo` is the repository it\n\
         // was picked in; `changeId`, `bookmark` and `path` say what it was picked on.\n\
         commands.register(\"{id}.hello\", async {context} => {{\n\
         \x20 const rows = await context.repo.rows(context.changeId ?? \"@\", {{ limit: 1 }});\n\
         \x20 await ui.toast(rows.length ? `Hello, ${{rows[0].description || rows[0].changeId}}` : \"Hello\");\n\
         }});\n"
    );
    if forge {
        let _ = write!(
            text,
            "\n\
             // The forge. diffui asks; these answer with data, and diffui draws the badges\n\
             // and dialogs. Leave a method out and diffui hides what needs it.\n\
             forge.register({{\n\
             \x20 // The pull requests whose head is one of `branches`.\n\
             \x20 async pullRequests(repo, {{ branches }}) {{\n\
             \x20   const remote = repo.forgeRemote;\n\
             \x20   if (!remote) return [];\n\
             \x20   const token = await auth.token(remote.host);\n\
             \x20   const res = await http.fetch(`${{new URL(remote.webUrl).origin}}/api/v1/repos/${{remote.owner}}/${{remote.name}}/pulls`, {{\n\
             \x20     headers: {{ accept: \"application/json\", authorization: `token ${{token}}` }},\n\
             \x20   }});\n\
             \x20   if (!res.ok) throw new Error(`${{remote.host}} answered ${{res.status}}`);\n\
             \x20   const pulls = await res.json();\n\
             \x20   return pulls\n\
             \x20     .filter((pull) => branches.includes(pull.head.ref))\n\
             \x20     .map((pull) => ({{\n\
             \x20       number: pull.number,\n\
             \x20       url: pull.html_url,\n\
             \x20       title: pull.title,\n\
             \x20       head: pull.head.ref,\n\
             \x20       base: pull.base.ref,\n\
             \x20       headSha: pull.head.sha,\n\
             \x20       state: pull.merged ? \"merged\" : pull.state === \"closed\" ? \"closed\" : \"open\",\n\
             \x20     }}));\n\
             \x20 }},\n\
             \x20 // The checks on a commit: \"pending\", \"success\", \"failure\" or \"none\".\n\
             \x20 async checks(repo, sha) {{\n\
             \x20   return {{ state: \"none\" }};\n\
             \x20 }},\n\
             }});\n"
        );
    }
    text
}

fn editor_config(ts: bool) -> (&'static str, &'static str) {
    if ts {
        (
            "tsconfig.json",
            "{\n  \"compilerOptions\": {\n    \"lib\": [\"ES2023\"],\n    \"target\": \"ES2023\",\n    \"module\": \"ESNext\",\n    \"moduleResolution\": \"bundler\",\n    \"strict\": true,\n    \"noEmit\": true,\n    \"erasableSyntaxOnly\": true,\n    \"verbatimModuleSyntax\": true,\n    \"allowImportingTsExtensions\": true\n  },\n  \"include\": [\"*.ts\"]\n}\n",
        )
    } else {
        (
            "jsconfig.json",
            "{\n  \"compilerOptions\": {\n    \"lib\": [\"ES2023\"],\n    \"target\": \"ES2023\",\n    \"module\": \"ESNext\",\n    \"moduleResolution\": \"bundler\",\n    \"checkJs\": true,\n    \"strict\": true,\n    \"noEmit\": true\n  },\n  \"include\": [\"*.js\", \"diffui.d.ts\"]\n}\n",
        )
    }
}

/// Write a new plugin into `dir`. Refuses a directory that already holds one.
fn init(id: &str, ts: bool, forge: Option<&str>, dir: &Path) -> Result<String, String> {
    let main = if ts { "main.ts" } else { "main.js" };
    let manifest = manifest_text(id, main, forge);
    // The manifest reader says what is wrong with an id or a host.
    if let Err(problems) = Manifest::parse(dir, &manifest) {
        let first = problems.first().map(|problem| problem.message.clone());
        return Err(first.unwrap_or_else(|| "that doesn't make a plugin".to_owned()));
    }
    if dir.join(MANIFEST_FILE).exists() {
        return Err(format!(
            "{} already holds a plugin; pick another id or --dir",
            display_path(dir)
        ));
    }
    let (config_name, config) = editor_config(ts);
    let files = [
        (MANIFEST_FILE, manifest),
        (main, main_text(id, ts, forge.is_some())),
        ("diffui.d.ts", diffui_plugins::TYPES.to_owned()),
        (config_name, config.to_owned()),
    ];
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", display_path(dir)))?;
    let dir = &std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut said = format!("Created {}\n", display_path(dir));
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|error| format!("{}: {error}", display_path(&path)))?;
        let _ = writeln!(said, "  {name}");
    }
    // The one line meant to be pasted: the id when `check` finds the plugin
    // by it, else the whole path, quoted if a shell would split it.
    let installed = plugins_dir().is_some_and(|folder| folder.join(id) == *dir);
    let target = if installed {
        id.to_owned()
    } else {
        shell_quote(&dir.display().to_string())
    };
    let _ = write!(
        said,
        "Next: edit {main}, then run `diffui plugin check {target}`.\n\
         diffui asks before it runs a new plugin; `diffui ctl reload` makes a running diffui look.\n"
    );
    Ok(said)
}

/// The plugin directory `given` names: that directory, or, for a bare name
/// with no directory of that name here, the plugin of that id in `folder`
/// (the plugins folder). `Err` names every place it looked.
fn plugin_dir(given: &Path, folder: Option<&Path>) -> Result<PathBuf, String> {
    let here = std::path::absolute(given).unwrap_or_else(|_| given.to_path_buf());
    if here.is_dir() {
        return Ok(here);
    }
    let mut tried = vec![display_path(&here)];
    let mut parts = given.components();
    if let (Some(std::path::Component::Normal(id)), None, Some(folder)) =
        (parts.next(), parts.next(), folder)
    {
        let installed = folder.join(id);
        if installed.is_dir() {
            return Ok(installed);
        }
        tried.push(display_path(&installed));
    }
    Err(format!("No plugin directory at {}", tried.join(" or ")))
}

/// The host `check` gives a plugin: it hears the plugin load, and answers
/// nothing, because there is no app behind it.
struct NoApp(std::sync::Mutex<std::sync::mpsc::Sender<PluginEvent>>);

impl Host for NoApp {
    fn call(&self, _plugin: &str, _call: HostCall, reply: Reply) {
        reply.err(
            "diffui plugin check runs without the app: repository and ui calls have nothing to answer them here",
        );
    }

    fn event(&self, _plugin: &str, event: PluginEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .send(event);
    }
}

/// Load the plugin in `dir` the way the app would, without the app, and say
/// what it is. `Err` with every problem, a line each.
fn check(dir: &Path) -> Result<String, String> {
    let manifest = Manifest::load(dir).map_err(|problems| {
        problems
            .iter()
            .map(|problem| format!("{}\n", placed(problem)))
            .collect::<String>()
    })?;
    let shown = display_path(dir);
    let mut problems = commands_of(&manifest).1;
    let builtin = crate::commands::Registry::builtin();
    for command in &manifest.commands {
        if builtin.get(&command.id).is_some() {
            problems.push(format!(
                "{}:{}:{}: [[commands]] {:?} is already a diffui command",
                display_path(&dir.join(MANIFEST_FILE)),
                command.line,
                command.col,
                command.id
            ));
        }
    }

    let (events, loaded) = std::sync::mpsc::channel();
    let data_dir = std::env::temp_dir().join(format!("diffui-plugin-check-{}", std::process::id()));
    let limits = Limits::default();
    let plugin = Plugin::spawn(
        manifest.clone(),
        Options {
            config: toml::Table::new(),
            data_dir: data_dir.clone(),
            limits,
        },
        Arc::new(NoApp(std::sync::Mutex::new(events))),
    );
    let event = loaded.recv_timeout(limits.load + Duration::from_secs(5));
    let log = plugin.log();
    drop(plugin);
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut failure = None;
    let registration = match event {
        Ok(PluginEvent::Loaded(registration)) => Some(registration),
        Ok(PluginEvent::Failed(why)) => {
            // A script's own position is relative to the plugin: name the
            // directory so the line resolves from anywhere.
            problems.push(format!("{shown}: {why}"));
            failure = Some(why);
            None
        }
        Err(_) => {
            problems.push(format!("{shown}: the plugin didn't finish loading"));
            None
        }
    };

    let mut said = String::new();
    let _ = writeln!(
        said,
        "{} {} (api {}) in {shown}",
        manifest.id, manifest.version, manifest.api
    );
    let _ = writeln!(said, "permissions: {}", permissions_line(&manifest));
    if let Some(registration) = &registration {
        said.push_str(&registered(&manifest, registration));
    }
    // The failure is in the log too; it is said once, with the problems.
    for line in log
        .iter()
        .filter(|line| Some(&line.message) != failure.as_ref())
    {
        let level = match line.level {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        };
        let _ = writeln!(said, "log {level}: {}", line.message);
    }
    if problems.is_empty() {
        said.push_str("ok\n");
        Ok(said)
    } else {
        for problem in &problems {
            let _ = writeln!(said, "{problem}");
        }
        Err(said)
    }
}

fn permissions_line(manifest: &Manifest) -> String {
    let permissions = &manifest.permissions;
    let mut parts = Vec::new();
    if !permissions.http.is_empty() {
        let hosts: Vec<String> = permissions.http.iter().map(ToString::to_string).collect();
        parts.push(format!("http {}", hosts.join(", ")));
    }
    for argv in &permissions.exec {
        parts.push(format!("exec {}", argv.join(" ")));
    }
    match permissions.jj {
        JjAccess::None => {}
        JjAccess::Read => parts.push("jj read".to_owned()),
        JjAccess::Write => parts.push("jj write".to_owned()),
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join("; ")
    }
}

/// What the script registered, against what the manifest declares.
fn registered(manifest: &Manifest, registration: &Registration) -> String {
    let mut said = String::new();
    if !manifest.commands.is_empty() {
        said.push_str("commands:\n");
    }
    for command in &manifest.commands {
        let _ = write!(said, "  {}  {}", command.id, command.label);
        if let Some(when) = &command.when {
            let _ = write!(said, "  (when {when})");
        }
        said.push('\n');
    }
    if let (Some(claim), Some(methods)) = (&manifest.forge, &registration.forge) {
        let hosts: Vec<String> = claim.hosts.iter().map(ToString::to_string).collect();
        let _ = writeln!(said, "forge {}: {}", hosts.join(", "), methods.join(", "));
        let request = match claim.term {
            ForgeTerm::PullRequest => "pull request",
            ForgeTerm::MergeRequest => "merge request",
        };
        for method in FORGE_METHODS {
            if methods.iter().any(|registered| registered == method) {
                continue;
            }
            // What `ScriptForge::can` and the refresh make of a method the
            // script left out.
            let without = match method {
                "checks" => format!("a {request} shows no check state"),
                "createPullRequest" => format!("Create {request} and Submit stack are off"),
                "updatePullRequest" => format!("Update {request} and Submit stack are off"),
                _ => continue,
            };
            let _ = writeln!(said, "  no {method}: {without}");
        }
    }
    if !registration.events.is_empty() {
        let _ = writeln!(said, "events: {}", registration.events.join(", "));
    }
    said
}

/// The plugins folder's plugins, a line each.
fn list() -> String {
    let Some(dir) = plugins_dir() else {
        return String::new();
    };
    let records = crate::config::plugin_records();
    let mut said = String::new();
    for dir in plugin_dirs(&dir) {
        match Manifest::load(&dir) {
            Ok(manifest) => {
                let record = records.get(&manifest.id);
                let state = if record.is_some_and(|record| !record.enabled) {
                    "off"
                } else {
                    match trust::verdict(record, &manifest.permissions) {
                        trust::Verdict::Allowed => "allowed",
                        trust::Verdict::Refused => "not allowed",
                        trust::Verdict::Ask { .. } => "not asked yet",
                    }
                };
                let _ = writeln!(
                    said,
                    "{}\t{}\t{state}\t{}",
                    manifest.id,
                    manifest.version,
                    display_path(&dir)
                );
            }
            Err(problems) => {
                let why = problems.first().map(placed).unwrap_or_default();
                let _ = writeln!(said, "?\t\tbroken\t{}\t{why}", display_path(&dir));
            }
        }
    }
    if said.is_empty() {
        let _ = writeln!(said, "No plugins in {}", display_path(&dir));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("diffui-plugin-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// What `init` writes loads as it is, in JavaScript and in TypeScript,
    /// with and without a forge.
    #[test]
    fn a_new_plugin_passes_check_as_written() {
        for (name, ts, forge) in [
            ("js", false, None),
            ("ts", true, None),
            ("forge-js", false, Some("codeberg.org")),
            ("forge-ts", true, Some("git.example.com:3000")),
        ] {
            let dir = scratch(name);
            let said = init("my-plugin", ts, forge, &dir).expect("init");
            assert!(said.contains("plugin.toml"), "{said}");
            // Outside the plugins folder, the next step names the whole path.
            let next = format!("run `diffui plugin check {}`", dir.display());
            assert!(said.contains(&next), "{said}");
            let main = if ts { "main.ts" } else { "main.js" };
            for file in [
                MANIFEST_FILE,
                main,
                "diffui.d.ts",
                if ts { "tsconfig.json" } else { "jsconfig.json" },
            ] {
                assert!(dir.join(file).is_file(), "{name}: {file}");
            }
            assert_eq!(
                std::fs::read_to_string(dir.join("diffui.d.ts")).unwrap(),
                diffui_plugins::TYPES
            );
            let checked = check(&dir).unwrap_or_else(|problems| panic!("{name}: {problems}"));
            assert!(
                checked.starts_with("my-plugin 0.1.0 (api 0) in "),
                "{checked}"
            );
            assert!(
                checked.contains("  my-plugin.hello  My plugin: say hello"),
                "{checked}"
            );
            assert!(checked.ends_with("ok\n"), "{checked}");
            if forge.is_some() {
                assert!(
                    checked.contains(
                        ": pullRequests, checks\n  \
                         no createPullRequest: Create pull request and Submit stack are off\n  \
                         no updatePullRequest: Update pull request and Submit stack are off\n"
                    ),
                    "{checked}"
                );
            }
            // A second init leaves the first alone.
            let again = init("my-plugin", ts, forge, &dir).unwrap_err();
            assert!(again.contains("already holds a plugin"), "{again}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `check` takes a directory or the id of an installed plugin, and says
    /// where it looked when it finds neither.
    #[test]
    fn check_finds_a_plugin_by_directory_or_by_id() {
        let folder = scratch("folder");
        let installed = folder.join("stack-tools");
        std::fs::create_dir_all(&installed).unwrap();
        assert_eq!(
            plugin_dir(Path::new("stack-tools"), Some(&folder)),
            Ok(installed.clone())
        );
        assert_eq!(plugin_dir(&installed, None), Ok(installed));

        let here = std::env::current_dir().unwrap();
        assert_eq!(
            plugin_dir(Path::new("no-such"), Some(&folder)),
            Err(format!(
                "No plugin directory at {} or {}",
                display_path(&here.join("no-such")),
                display_path(&folder.join("no-such"))
            ))
        );
        // Anything but a bare name is a path, looked for where it says.
        assert_eq!(
            plugin_dir(Path::new("./stack-tools"), Some(&folder)),
            Err(format!(
                "No plugin directory at {}",
                display_path(&here.join("stack-tools"))
            ))
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn init_refuses_an_id_or_a_host_the_manifest_would() {
        let dir = scratch("bad");
        let refused = init("My Plugin", false, None, &dir).unwrap_err();
        assert!(refused.contains("id \"My Plugin\" must be"), "{refused}");
        let refused = init("fine", false, Some("https://x"), &dir).unwrap_err();
        assert!(refused.contains("is not a host"), "{refused}");
        assert!(!dir.exists());
    }

    #[test]
    fn check_places_a_syntax_error_and_a_manifest_problem() {
        let dir = scratch("broken");
        init("broken", false, None, &dir).expect("init");
        std::fs::write(
            dir.join("main.js"),
            "import { commands } from \"diffui\";\ncommands.register(\"broken.hello\", async () => {\n  let = ;\n});\n",
        )
        .unwrap();
        let problems = check(&dir).unwrap_err();
        assert!(problems.contains("main.js:3:"), "{problems}");
        assert!(problems.contains("SyntaxError"), "{problems}");
        assert!(!problems.contains("ok\n"), "{problems}");

        // A handler the manifest declares and the script leaves out.
        std::fs::write(dir.join("main.js"), "import \"diffui\";\n").unwrap();
        let problems = check(&dir).unwrap_err();
        assert!(problems.contains("broken.hello"), "{problems}");

        // The manifest's own problems come with their place, before any
        // script runs.
        let manifest = std::fs::read_to_string(dir.join(MANIFEST_FILE)).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest.replace("api = 0", "api = 9"),
        )
        .unwrap();
        let problems = check(&dir).unwrap_err();
        assert!(
            problems.contains("plugin.toml:4:7: api = 9 is not supported"),
            "{problems}"
        );

        // And the app's grammars: a when-clause that doesn't parse.
        std::fs::write(
            dir.join(MANIFEST_FILE),
            manifest.replace(
                "# when = \"repo.kind == 'jj'\"",
                "when = \"repo.kind ==\"     ",
            ),
        )
        .unwrap();
        let problems = check(&dir).unwrap_err();
        assert!(
            problems.contains("[[commands]] \"broken.hello\": when:"),
            "{problems}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
