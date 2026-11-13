//! The app with real plugin directories under the test home: a plugin is
//! found, asked about, started, called, reloaded and stopped the way a
//! launch would do it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use diffui_core::RevisionSelection;
use diffui_core::forge::{CheckState, ForgeAbility, ForgeKind, PrState};

use super::{PluginAction, State};
use crate::commands::{CommandArg, PaletteScope};
use crate::harness::{Harness, fixture_repo};
use crate::modes::ModeKind;
use crate::{Action, Diffui};

const SIZE: (f32, f32) = (1280.0, 800.0);

/// A plugin directory to write under the test home before the app starts.
pub(crate) struct PluginDir {
    pub(crate) id: &'static str,
    pub(crate) manifest: String,
    pub(crate) main: String,
}

impl PluginDir {
    /// A plugin `id` with `permissions` (the body of `[permissions]`),
    /// `commands` (command names, each `<id>.<name>`, offered in the palette)
    /// and `main` as its script.
    pub(crate) fn new(id: &'static str, permissions: &str, commands: &[&str], main: &str) -> Self {
        let name = super::cli::name_of(id);
        let mut manifest = format!(
            "id = \"{id}\"\nname = \"{name}\"\nversion = \"1.2.0\"\napi = 0\nmain = \"main.js\"\n\n[permissions]\n{permissions}\n"
        );
        for command in commands {
            manifest.push_str(&format!(
                "\n[[commands]]\nid = \"{id}.{command}\"\nlabel = \"{name}: {command}\"\n"
            ));
        }
        Self {
            id,
            manifest,
            main: main.to_owned(),
        }
    }

    fn dir(&self, home: &Path) -> PathBuf {
        home.join(".config/diffui/plugins").join(self.id)
    }

    pub(crate) fn write(&self, home: &Path) {
        let dir = self.dir(home);
        std::fs::create_dir_all(&dir).expect("make the plugin's folder");
        std::fs::write(dir.join("plugin.toml"), &self.manifest).expect("write plugin.toml");
        std::fs::write(dir.join("main.js"), &self.main).expect("write main.js");
    }
}

/// `settings.toml` with the user having allowed `id` these permissions.
pub(crate) fn allowed(id: &str, permissions: &str) -> String {
    format!("[plugins.{id}]\nallowed = true\n\n[plugins.{id}.permissions]\n{permissions}\n")
}

/// The app on `repo` with `plugins` installed, `settings` as its
/// settings.toml and `config` as its config.toml, its graph loaded.
fn launch_on(repo: &Path, plugins: &[PluginDir], config: &str, settings: &str) -> Harness {
    let mut h = Harness::open_prepared(Some(repo), SIZE, config, settings, |home| {
        for plugin in plugins {
            plugin.write(home);
        }
    });
    h.wait_for_graph();
    h
}

fn launch(plugins: &[PluginDir], settings: &str) -> (Harness, PathBuf) {
    let repo = fixture_repo();
    let h = launch_on(&repo, plugins, "", settings);
    (h, repo)
}

fn state(ui: &Diffui, id: &str) -> State {
    ui.plugins.get(id).expect("the plugin").state.clone()
}

pub(crate) fn wait_running(h: &mut Harness, id: &'static str) {
    h.wait_until("the plugin to load", |ui| {
        assert!(
            !matches!(state(ui, id), State::Failed(_)),
            "{:?}",
            state(ui, id)
        );
        matches!(state(ui, id), State::Running(_))
    });
}

/// What the plugin `id` wrote with `console.log`, a line each.
fn logged(ui: &Diffui, id: &str) -> Vec<String> {
    ui.plugins
        .get(id)
        .expect("the plugin")
        .log()
        .into_iter()
        .map(|line| line.message)
        .collect()
}

/// Run `command` and wait for the plugin `id` to log a line `done` accepts;
/// returns that line.
fn run_and_read(
    h: &mut Harness,
    id: &'static str,
    command: &str,
    done: impl Fn(&str) -> bool,
) -> String {
    let before = h.read(|ui| logged(ui, id).len());
    let enabled = h.read(|ui| {
        ui.registry
            .get(command)
            .expect("the command")
            .enabled(ui, &CommandArg::None)
    });
    assert_eq!(enabled, Ok(()), "{command}");
    h.act(|ui, cx| ui.run_command(command, CommandArg::None, cx));
    let mut found = None;
    h.wait_until(&format!("{command} to log"), |ui| {
        found = logged(ui, id)
            .into_iter()
            .skip(before)
            .find(|line| done(line));
        found.is_some()
    });
    found.expect("a line")
}

fn toasts(ui: &Diffui) -> Vec<(String, String, bool)> {
    ui.toasts
        .iter()
        .map(|toast| {
            (
                toast.title.to_string(),
                toast.detail.to_string(),
                toast.info,
            )
        })
        .collect()
}

fn settings_on_disk() -> String {
    std::fs::read_to_string(crate::harness::scratch_home().join(".config/diffui/settings.toml"))
        .expect("settings.toml")
}

const GREETER: &str = r#"
import { commands, ui } from "diffui";
commands.register("greeter.greet", async ({ repo, changeId, commitId }) => {
  const rows = await repo.rows(commitId, { limit: 1 });
  const selection = await repo.selection();
  const bookmarks = await repo.bookmarks();
  const readme = await repo.readFile("@", "README.md");
  const missing = await repo.readFile("@", "no/such/file");
  const diff = await repo.diff(commitId, { format: "summary" });
  console.log(JSON.stringify({
    row: rows[0].changeId === changeId && rows[0].commitId === commitId,
    selected: selection.revisions[0].changeId === changeId,
    main: bookmarks.some((bookmark) => bookmark.name === "main" && bookmark.targets.length === 1),
    readme: typeof readme, missing, diff: typeof diff.text, root: repo.root,
  }));
  await ui.toast(`Hello from ${rows[0].changeId.slice(0, 8)}`);
});
"#;

/// A plugin nobody answered for doesn't run: the dialog comes up, its
/// command is there but off. Allowing it starts it, keeps the answer, and
/// the command runs its handler, which reads the repository and toasts.
#[test]
fn a_command_plugin_asks_then_registers_runs_and_toasts() {
    let greeter = PluginDir::new("greeter", "jj = \"read\"", &["greet"], GREETER);
    let (mut h, _) = launch(&[greeter], "");
    let root = h.read(|ui| ui.active().repository.as_ref().unwrap().root.clone());

    h.wait_until("the permission dialog", |ui| {
        ui.mode(ModeKind::PluginPermission).is_some()
    });
    h.read(|ui| {
        assert_eq!(state(ui, "greeter"), State::NotAllowed);
        assert!(ui.plugins.get("greeter").unwrap().log().is_empty());
        let entry = ui.registry.get("greeter.greet").expect("its row");
        assert_eq!(entry.group, "Greeter");
        assert_eq!(
            entry.enabled(ui, &CommandArg::None).unwrap_err(),
            "Not allowed"
        );
        // Listed in the palette, greyed.
        let listed = ui
            .registry
            .palette_commands(ui, PaletteScope::Root, &CommandArg::None);
        assert!(
            listed
                .iter()
                .any(|(entry, verdict)| entry.id == "greeter.greet" && verdict.is_err())
        );
    });
    // A disabled command does nothing.
    h.act(|ui, cx| ui.run_command("greeter.greet", CommandArg::None, cx));

    h.perform(Action::Plugin(PluginAction::Answer(true)));
    wait_running(&mut h, "greeter");
    let settings = settings_on_disk();
    assert!(
        settings.contains("[plugins.greeter]\nallowed = true"),
        "{settings}"
    );
    assert!(settings.contains("jj = \"read\""), "{settings}");

    let line = run_and_read(&mut h, "greeter", "greeter.greet", |line| {
        line.starts_with('{')
    });
    let said: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(
        said,
        serde_json::json!({
            "row": true, "selected": true, "main": true, "readme": "string",
            "missing": null, "diff": "string", "root": root.display().to_string(),
        })
    );
    h.wait_until("the toast", |ui| {
        toasts(ui).iter().any(|(title, detail, info)| {
            title == "Greeter" && detail.starts_with("Hello from ") && *info
        })
    });
}

/// The script of a plugin the user refused never runs, at this launch or
/// the next, and the dialog doesn't come back for the same permissions.
#[test]
fn a_plugin_that_is_not_allowed_does_not_run() {
    let main = "import { kv } from \"diffui\";\nawait kv.set(\"ran\", true);\n";
    let store = crate::harness::scratch_home().join(".local/share/diffui/plugins/quiet.json");
    let (mut h, _) = launch(&[PluginDir::new("quiet", "", &[], main)], "");
    h.wait_until("the permission dialog", |ui| {
        ui.mode(ModeKind::PluginPermission).is_some()
    });
    h.perform(Action::Plugin(PluginAction::Answer(false)));
    h.settle();
    h.read(|ui| {
        assert_eq!(state(ui, "quiet"), State::NotAllowed);
        assert!(ui.mode(ModeKind::PluginPermission).is_none());
    });
    let settings = settings_on_disk();
    assert!(
        settings.contains("[plugins.quiet]\nallowed = false"),
        "{settings}"
    );
    drop(h);

    let (mut h, _) = launch(&[PluginDir::new("quiet", "", &[], main)], &settings);
    h.settle();
    h.read(|ui| {
        assert_eq!(state(ui, "quiet"), State::NotAllowed);
        assert!(ui.mode(ModeKind::PluginPermission).is_none());
    });
    assert!(!store.exists(), "the script ran");
}

/// A manifest that asks for more than the user allowed is asked about again
/// and doesn't run meanwhile; one that asks for less runs on the old answer.
#[test]
fn a_plugin_that_asks_for_more_is_asked_about_again() {
    let main = "import { commands } from \"diffui\";\ncommands.register(\"grow.go\", () => {});\n";
    let settings = allowed(
        "grow",
        "http = [\"codeberg.org\"]\nexec = []\njj = \"read\"",
    );

    let fewer = PluginDir::new("grow", "jj = \"read\"", &["go"], main);
    let (mut h, _) = launch(&[fewer], &settings);
    wait_running(&mut h, "grow");
    h.read(|ui| assert!(ui.mode(ModeKind::PluginPermission).is_none()));
    drop(h);

    let more = PluginDir::new(
        "grow",
        "http = [\"codeberg.org\", \"example.com\"]\njj = \"write\"",
        &["go"],
        main,
    );
    let (mut h, _) = launch(&[more], &settings);
    h.wait_until("the permission dialog", |ui| {
        ui.mode(ModeKind::PluginPermission).is_some()
    });
    h.read(|ui| {
        assert_eq!(state(ui, "grow"), State::NotAllowed);
        assert!(ui.plugins.get("grow").unwrap().log().is_empty());
        let manifest = ui.plugins.get("grow").unwrap().manifest.clone().unwrap();
        let record = ui.config_files.plugins.get("grow");
        let super::trust::Verdict::Ask { before } =
            super::trust::verdict(record, &manifest.permissions)
        else {
            panic!("it asks");
        };
        let new: Vec<String> = super::trust::lines(&manifest, before.as_ref())
            .into_iter()
            .filter(|line| line.new)
            .map(|line| line.code.unwrap_or(line.text))
            .collect();
        assert_eq!(
            new,
            [
                "example.com",
                "Read the open repository and change it with jj commands"
            ]
        );
    });
    h.perform(Action::Plugin(PluginAction::Answer(true)));
    wait_running(&mut h, "grow");
}

/// Saving the script reloads the plugin: the next run is the new code, and
/// a toast says so. A syntax error in the edit says where.
#[test]
fn an_edit_reloads_the_plugin() {
    let main = |word: &str| {
        format!(
            "import {{ commands }} from \"diffui\";\ncommands.register(\"edit.say\", () => console.log(\"{word}\"));\n"
        )
    };
    let plugin = PluginDir::new("edit", "", &["say"], &main("one"));
    let dir = plugin.dir(crate::harness::scratch_home());
    let (mut h, _) = launch(
        &[plugin],
        &allowed("edit", "http = []\nexec = []\njj = \"none\""),
    );
    wait_running(&mut h, "edit");
    assert_eq!(run_and_read(&mut h, "edit", "edit.say", |_| true), "one");

    std::fs::write(dir.join("main.js"), main("two")).expect("edit main.js");
    h.wait_until("the reload", |ui| {
        toasts(ui)
            .iter()
            .any(|(title, ..)| title == "Reloaded Edit")
    });
    wait_running(&mut h, "edit");
    assert_eq!(run_and_read(&mut h, "edit", "edit.say", |_| true), "two");

    std::fs::write(dir.join("main.js"), "import \"diffui\";\nlet = ;\n").expect("break main.js");
    h.wait_until("the failure", |ui| {
        matches!(state(ui, "edit"), State::Failed(_))
    });
    h.read(|ui| {
        let State::Failed(why) = state(ui, "edit") else {
            unreachable!()
        };
        assert!(why.starts_with("main.js:2:"), "{why}");
        assert!(
            toasts(ui).iter().any(
                |(title, detail, _)| title == "Edit failed" && detail.starts_with("main.js:2:")
            )
        );
        assert_eq!(
            ui.registry
                .get("edit.say")
                .unwrap()
                .enabled(ui, &CommandArg::None)
                .unwrap_err(),
            "Edit failed"
        );
    });

    // A manifest that now asks for something is looked at again first.
    std::fs::write(dir.join("main.js"), main("three")).expect("fix main.js");
    let manifest = std::fs::read_to_string(dir.join("plugin.toml")).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        manifest.replace("[permissions]\n", "[permissions]\njj = \"read\"\n"),
    )
    .expect("edit plugin.toml");
    h.wait_until("the permission dialog", |ui| {
        ui.mode(ModeKind::PluginPermission).is_some()
    });
    h.read(|ui| assert_eq!(state(ui, "edit"), State::NotAllowed));
}

/// A plugin that spins, and one whose script doesn't load, fail by
/// themselves: the app says which, and the plugin beside them keeps
/// answering.
#[test]
fn a_crashing_plugin_fails_alone() {
    let spin = PluginDir::new(
        "spin",
        "",
        &["forever"],
        "import { commands } from \"diffui\";\ncommands.register(\"spin.forever\", () => { for (;;) {} });\n",
    );
    let broken = PluginDir::new("broken", "", &[], "import { nothing } from \"node:fs\";\n");
    let steady = PluginDir::new(
        "steady",
        "",
        &["ping"],
        "import { commands } from \"diffui\";\ncommands.register(\"steady.ping\", () => console.log(\"pong\"));\n",
    );
    let none = "http = []\nexec = []\njj = \"none\"";
    let settings = format!(
        "{}\n{}\n{}",
        allowed("spin", none),
        allowed("broken", none),
        allowed("steady", none)
    );
    let (mut h, _) = launch(&[spin, broken, steady], &settings);
    wait_running(&mut h, "steady");
    wait_running(&mut h, "spin");
    h.wait_until("the broken one to fail", |ui| {
        matches!(state(ui, "broken"), State::Failed(_))
    });
    h.read(|ui| {
        let State::Failed(why) = state(ui, "broken") else {
            unreachable!()
        };
        assert!(why.contains("Cannot import \"node:fs\""), "{why}");
    });

    h.act(|ui, cx| ui.run_command("spin.forever", CommandArg::None, cx));
    h.wait_until("the spinning one to be stopped", |ui| {
        matches!(state(ui, "spin"), State::Failed(_))
    });
    h.read(|ui| {
        assert!(toasts(ui).iter().any(|(title, ..)| title == "Spin failed"));
        assert!(matches!(state(ui, "steady"), State::Running(_)));
    });
    assert_eq!(
        run_and_read(&mut h, "steady", "steady.ping", |_| true),
        "pong"
    );
}

const PROBE: &str = r#"
import { commands, events, ui } from "diffui";
let kept;
const tell = async (work) => {
  try { console.log(`ok ${await work()}`); } catch (error) { console.log(`${error.name}: ${error.message}`); }
};
commands.register("probe.keep", ({ repo }) => { kept = repo; console.log("kept"); });
commands.register("probe.use", () => tell(async () => (await kept.bookmarks()).length > 0));
commands.register("probe.write-query", ({ repo }) => tell(() => repo.jj.query(["describe", "-m", "from a plugin"])));
commands.register("probe.elsewhere", ({ repo }) => tell(() => repo.jj.query(["log", "--at-op", "@-"])));
commands.register("probe.head", ({ repo }) => tell(() => repo.jj.query(["op", "log", "--no-graph", "-n", "1", "-T", "id.short(16)"])));
commands.register("probe.new", ({ repo }) => tell(async () => (await repo.jj.run(["new"])).summary.length > 0));
commands.register("probe.ui", () => tell(async () => {
  await ui.status.set("3 open");
  await ui.clipboard.write("copied by a plugin");
  await ui.openUrl("https://example.com/pulls/7");
  const refused = await ui.openUrl("file:///etc/passwd").then(() => "opened", (error) => error.message);
  const activity = await ui.activity("Probing");
  await activity.append("one\ntwo");
  await activity.finish();
  return refused;
}));
commands.register("probe.ask", () => tell(async () => {
  const name = await ui.prompt.input({ label: "Name it", value: "draft" });
  const sure = await ui.prompt.confirm({ title: "Go ahead?" });
  const which = await ui.prompt.choose({ label: "Which", choices: ["a", { label: "Bee", value: "b" }] });
  return JSON.stringify([name, sure, which]);
}));
for (const name of ["repoOpened", "refsChanged", "selectionChanged", "fetched", "commandInvoked"]) {
  events.on(name, (event) => console.log(`event ${name}${event.id ? ` ${event.id}` : ""}`));
}
"#;

const PROBE_COMMANDS: &[&str] = &[
    "keep",
    "use",
    "write-query",
    "elsewhere",
    "head",
    "new",
    "ui",
    "ask",
];

fn launch_probe() -> (Harness, PathBuf) {
    let probe = PluginDir::new("probe", "jj = \"write\"", PROBE_COMMANDS, PROBE);
    let (mut h, repo) = launch(
        &[probe],
        &allowed("probe", "http = []\nexec = []\njj = \"write\""),
    );
    wait_running(&mut h, "probe");
    (h, repo)
}

fn answer(h: &mut Harness, command: &str) -> String {
    run_and_read(h, "probe", command, |line| {
        line.starts_with("ok ") || line.contains("Error: ")
    })
}

/// `repo.jj.query` runs only what reads, where and when the handle says.
#[test]
fn a_query_refuses_to_write_and_reads_at_the_handles_operation() {
    let (mut h, _) = launch_probe();
    let refused = answer(&mut h, "probe.write-query");
    assert!(
        refused.starts_with("HostError: repo.jj.query runs read-only commands only"),
        "{refused}"
    );
    assert_eq!(
        answer(&mut h, "probe.elsewhere"),
        "HostError: repo.jj.query doesn't take --at-op"
    );
    // `jj op log` run at an operation starts from it.
    let on_screen = h.read(|ui| {
        ui.active()
            .session
            .graph_operation()
            .expect("an operation")
            .to_owned()
    });
    assert_eq!(
        answer(&mut h, "probe.head"),
        format!("ok {}", &on_screen[..16])
    );
    h.read(|ui| {
        let described = ui
            .active()
            .session
            .commits
            .iter()
            .any(|row| row.description().contains("from a plugin"));
        assert!(!described);
    });
}

/// A handle answers while its tab is on screen at the operation it was made
/// at, and is refused once either moved: while another tab is in front, and
/// after the repository changed.
#[test]
fn a_stale_handle_is_refused() {
    let (mut h, _) = launch_probe();
    run_and_read(&mut h, "probe", "probe.keep", |line| line == "kept");
    assert_eq!(answer(&mut h, "probe.use"), "ok true");

    // Another tab in front: the handle's tab isn't what the user sees.
    let first = h.read(|ui| ui.active_tab_id().expect("a tab"));
    let other = fixture_repo();
    h.act(|ui, cx| ui.open_repository(&other.display().to_string(), cx));
    h.wait_until("the second tab", |ui| {
        ui.active_tab_id() != Some(first) && ui.active().session.graph_operation().is_some()
    });
    assert!(answer(&mut h, "probe.use").starts_with("StaleError: "));
    h.act(|ui, cx| ui.activate_tab(first, cx));
    h.settle();
    assert_eq!(answer(&mut h, "probe.use"), "ok true");

    // The repository moves on: `jj new` through the plugin's own jj.run.
    let before = h.read(|ui| ui.active().session.graph_operation().map(str::to_owned));
    assert_eq!(answer(&mut h, "probe.new"), "ok true");
    h.wait_until("the new operation", |ui| {
        let session = &ui.active().session;
        session.graph_operation().map(str::to_owned) != before && !session.busy()
    });
    assert!(answer(&mut h, "probe.use").starts_with("StaleError: "));
    // A handle made now is good again.
    run_and_read(&mut h, "probe", "probe.keep", |line| line == "kept");
    assert_eq!(answer(&mut h, "probe.use"), "ok true");
}

/// The UI slots: the status item, the clipboard, links (http and https
/// only), an activity row, and the three prompts through the app's own
/// dialogs.
#[test]
fn a_plugin_fills_the_ui_slots_and_asks_through_the_apps_dialogs() {
    use crate::bookmarks::BookmarkAction;
    let (mut h, _) = launch_probe();
    crate::harness::OPENED_URLS.lock().unwrap().clear();
    assert_eq!(
        answer(&mut h, "probe.ui"),
        "ok ui.openUrl opens http and https links only"
    );
    assert_eq!(
        *crate::harness::OPENED_URLS.lock().unwrap(),
        ["https://example.com/pulls/7"]
    );
    h.read(|ui| {
        assert_eq!(
            ui.plugins.status.get("probe").map(String::as_str),
            Some("3 open")
        );
        let row = ui
            .active()
            .activities
            .recent(10)
            .find(|activity| activity.label.to_string() == "Probing")
            .expect("the activity row");
        assert_eq!(row.status, crate::activity::ActivityStatus::Done);
        assert_eq!(row.detail, ["one", "two"]);
    });

    let answers = |h: &mut Harness, before: usize| {
        let mut said = None;
        h.wait_until("the answers", |ui| {
            said = logged(ui, "probe")
                .into_iter()
                .skip(before)
                .find(|line| line.starts_with("ok "));
            said.is_some()
        });
        said.expect("answers")
    };
    let before = h.read(|ui| logged(ui, "probe").len());
    h.act(|ui, cx| ui.run_command("probe.ask", CommandArg::None, cx));
    h.wait_until("the input prompt", |ui| ui.name_prompt().is_some());
    h.read(|ui| assert_eq!(ui.name_prompt().unwrap().value, "draft"));
    h.perform(Action::Bookmark(BookmarkAction::PromptInput(
        "final".to_owned(),
    )));
    h.perform(Action::Bookmark(BookmarkAction::PromptSubmit));
    h.wait_until("the confirm", |ui| ui.confirm().is_some());
    h.perform(Action::ConfirmAccept);
    h.wait_until("the choices", |ui| {
        ui.palette().is_some_and(|palette| {
            palette
                .top()
                .is_some_and(|column| column.matches.len() == 2)
        })
    });
    h.press("down");
    h.press("enter");
    assert_eq!(answers(&mut h, before), r#"ok ["final",true,"b"]"#);

    // A prompt closed without an answer resolves, rather than hanging.
    let before = h.read(|ui| logged(ui, "probe").len());
    h.act(|ui, cx| ui.run_command("probe.ask", CommandArg::None, cx));
    h.wait_until("the input prompt", |ui| ui.name_prompt().is_some());
    h.perform(Action::Bookmark(BookmarkAction::PromptCancel));
    h.wait_until("the confirm", |ui| ui.confirm().is_some());
    h.perform(Action::ConfirmCancel);
    h.wait_until("the choices", |ui| ui.palette().is_some());
    h.act(|ui, _| {
        ui.pop_mode(ModeKind::Palette);
    });
    assert_eq!(answers(&mut h, before), "ok [null,false,null]");
}

/// `diffui ctl plugins` says what each plugin is doing, `ctl reload` starts
/// them afresh, and Settings' Show log puts a plugin's log in the output
/// panel.
#[test]
fn ctl_lists_and_reloads_plugins_and_the_log_opens() {
    use crate::ctl::Request;
    let ask = |h: &mut Harness, request: Request| {
        let (reply, mut answer) = futures::channel::oneshot::channel();
        h.act(|ui, cx| ui.on_ctl_request(request, reply, cx));
        let mut value = None;
        h.wait_until("the answer", |_| {
            if value.is_none() {
                value = answer.try_recv().ok().flatten();
            }
            value.is_some()
        });
        value.expect("answered")
    };
    let chatty = PluginDir::new(
        "chatty",
        "",
        &["hi"],
        "import { commands, events } from \"diffui\";\nconsole.log(\"loaded\");\nconsole.warn(\"careful\");\ncommands.register(\"chatty.hi\", () => {});\nevents.on(\"fetched\", () => {});\n",
    );
    let broken = PluginDir::new("broken", "", &[], "throw new Error(\"no good\");\n");
    let none = "http = []\nexec = []\njj = \"none\"";
    let settings = format!("{}\n{}", allowed("chatty", none), allowed("broken", none));
    let (mut h, _) = launch(&[broken, chatty], &settings);
    wait_running(&mut h, "chatty");
    h.wait_until("the broken one", |ui| {
        matches!(state(ui, "broken"), State::Failed(_))
    });

    let listed = ask(&mut h, Request::Plugins);
    let plugins = listed["plugins"].as_array().expect("plugins");
    assert_eq!(plugins.len(), 2, "{listed}");
    assert_eq!(plugins[0]["id"], "broken");
    assert_eq!(plugins[0]["state"], "failed");
    assert_eq!(plugins[0]["error"], "main.js:1:11: Error: no good");
    assert_eq!(plugins[1]["id"], "chatty");
    assert_eq!(plugins[1]["state"], "running");
    assert_eq!(plugins[1]["commands"], serde_json::json!(["chatty.hi"]));
    assert_eq!(plugins[1]["events"], serde_json::json!(["fetched"]));
    assert_eq!(plugins[1]["log"], serde_json::json!(["loaded", "careful"]));

    h.perform(Action::Plugin(PluginAction::ShowLog("chatty".to_owned())));
    h.read(|ui| {
        let run = ui.shown_cli_run().expect("the log in the output panel");
        assert_eq!(run.program, "Chatty log");
        let lines: Vec<&str> = run.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(lines, ["loaded", "careful"]);
        assert_eq!(run.lines[1].stream, diffui_core::cli::CliStream::Stderr);
    });
    h.perform(Action::CommandLine(crate::cmdline::CmdAction::Output(
        crate::cli_output::OutputAction::Close,
    )));

    // A reload starts every plugin again and says so.
    let reloaded = ask(&mut h, Request::Reload);
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    h.wait_until("the reload", |ui| {
        toasts(ui)
            .iter()
            .any(|(title, ..)| title == "Reloaded Chatty")
    });
    wait_running(&mut h, "chatty");

    // Turned off, its command is gone; turned on, it is back.
    h.perform(Action::Plugin(PluginAction::Enable(
        "chatty".to_owned(),
        false,
    )));
    h.read(|ui| {
        assert_eq!(state(ui, "chatty"), State::Disabled);
        assert!(ui.registry.get("chatty.hi").is_none());
    });
    assert!(settings_on_disk().contains("enabled = false"));
    h.perform(Action::Plugin(PluginAction::Enable(
        "chatty".to_owned(),
        true,
    )));
    wait_running(&mut h, "chatty");
    h.read(|ui| assert!(ui.registry.get("chatty.hi").is_some()));
    assert!(!settings_on_disk().contains("enabled = false"));
}

/// Plugins hear what happens, each event once it is true.
#[test]
fn a_plugin_hears_the_events_it_subscribed_to() {
    let (mut h, repo) = launch_probe();
    let heard = |h: &mut Harness, event: &'static str| {
        h.wait_until(event, |ui| {
            logged(ui, "probe").iter().any(|line| line == event)
        });
    };
    // The repository was open before the plugin loaded, or opened after:
    // either way the plugin hears of it once.
    heard(&mut h, "event repoOpened");

    let revision = RevisionSelection::Commit(commit_of(&repo, "fix/parser-crash"));
    h.perform(Action::SelectRevision(revision));
    heard(&mut h, "event selectionChanged");

    h.act(|ui, cx| ui.run_command("repo.refresh", CommandArg::None, cx));
    heard(&mut h, "event commandInvoked repo.refresh");

    h.act(|ui, cx| ui.run_command("repo.fetch", CommandArg::None, cx));
    heard(&mut h, "event fetched");

    h.act(|ui, cx| {
        let tab = ui.active_tab_id().expect("a tab");
        ui.run_jj_line(tab, "bookmark create heard -r @", None, cx)
            .expect("the line runs");
    });
    heard(&mut h, "event refsChanged");
    h.read(|ui| {
        let lines = logged(ui, "probe");
        let count = |event: &str| lines.iter().filter(|line| *line == event).count();
        assert_eq!(count("event repoOpened"), 1, "{lines:?}");
    });
}

fn commit_of(repo: &Path, name: &str) -> String {
    let manifest =
        std::fs::read_to_string(repo.join(".jj/fixture-manifest")).expect("the manifest");
    manifest
        .lines()
        .find_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?;
            let commit = parts.nth(1)?;
            names
                .split(',')
                .any(|it| it == name)
                .then(|| commit.to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} in the manifest"))
}

/// What a [`Server`] was asked: the method, the path and the
/// `authorization` header.
type Asked = (String, String, Option<String>);

/// A loopback HTTP server that answers each request with what `respond`
/// makes of its method and path, and keeps what it was asked.
struct Server {
    /// `127.0.0.1:<port>`.
    host: String,
    seen: Arc<Mutex<Vec<Asked>>>,
}

impl Server {
    fn start(respond: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let host = listener.local_addr().expect("an address").to_string();
        let seen: Arc<Mutex<Vec<Asked>>> = Arc::new(Mutex::new(Vec::new()));
        let respond = Arc::new(respond);
        std::thread::spawn({
            let seen = seen.clone();
            move || {
                for stream in listener.incoming().flatten() {
                    let (seen, respond) = (seen.clone(), respond.clone());
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() {
                            return;
                        }
                        let mut words = line.split_whitespace();
                        let (Some(method), Some(path)) = (words.next(), words.next()) else {
                            return;
                        };
                        let (mut length, mut authorization) = (0usize, None);
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':') {
                                match name.to_ascii_lowercase().as_str() {
                                    "content-length" => length = value.trim().parse().unwrap_or(0),
                                    "authorization" => {
                                        authorization = Some(value.trim().to_owned())
                                    }
                                    _ => {}
                                }
                            }
                        }
                        let mut body = vec![0; length];
                        let _ = reader.read_exact(&mut body);
                        seen.lock().unwrap().push((
                            method.to_owned(),
                            path.to_owned(),
                            authorization,
                        ));
                        let (status, body) = respond(method, path);
                        let mut stream = stream;
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    });
                }
            }
        });
        Self { host, seen }
    }
}

/// The reference Forgejo plugin, as shipped, is the forge of a repository
/// whose remote is on a host it claims: its pull requests and checks land
/// in the same store the built-in forges fill, and the app offers what the
/// plugin implements.
#[test]
fn the_forgejo_plugin_badges_bookmarks_like_a_built_in_forge() {
    let repo = fixture_repo();
    let head = commit_of(&repo, "fix/parser-crash");
    let server = Server::start({
        let head = head.clone();
        move |_, path| {
            if path.starts_with("/api/v1/repos/team/app/pulls") {
                let pull = |number: u64, branch: &str, sha: &str, repo_id: u64| {
                    serde_json::json!({
                        "number": number, "title": format!("Pull {number}"),
                        "html_url": format!("https://forge.test/team/app/pulls/{number}"),
                        "state": "open", "merged": false, "draft": false,
                        "head": { "ref": branch, "sha": sha, "repo_id": repo_id },
                        "base": { "ref": "main", "repo_id": 1 },
                    })
                };
                let pulls = serde_json::json!([
                    pull(7, "fix/parser-crash", &head, 1),
                    // A fork's branch of the same name isn't ours.
                    pull(8, "feature/auth", "ffff", 2),
                    pull(9, "not-a-local-bookmark", "eeee", 1),
                ]);
                (200, pulls.to_string())
            } else if path == format!("/api/v1/repos/team/app/commits/{head}/status") {
                let status = serde_json::json!({
                    "state": "pending", "total_count": 1,
                    "statuses": [{ "target_url": "https://ci.test/run/1" }],
                });
                (200, status.to_string())
            } else {
                (404, "{\"message\":\"not found\"}".to_owned())
            }
        }
    });
    let host = server.host.clone();

    // The repository's remote is on the server's host.
    let git_config = repo.join(".git/config");
    let text = std::fs::read_to_string(&git_config).expect("the git config");
    let pointed: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("url = ") {
                format!("\turl = http://{host}/team/app.git\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    std::fs::write(&git_config, pointed).expect("point origin at the server");

    // The example's own files, with the server's host for codeberg.org.
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/examples/forgejo");
    let forgejo = PluginDir {
        id: "forgejo",
        manifest: std::fs::read_to_string(example.join("plugin.toml"))
            .expect("the example's manifest")
            .replace("codeberg.org", &host),
        main: std::fs::read_to_string(example.join("main.js")).expect("the example's script"),
    };
    let config = "[plugins.forgejo]\ntoken_cmd = [\"echo\", \"token-for-{host}\"]\n";
    let settings = allowed(
        "forgejo",
        &format!("http = [\"{host}\"]\nexec = []\njj = \"none\""),
    );
    let mut h = launch_on(&repo, &[forgejo], config, &settings);
    wait_running(&mut h, "forgejo");

    h.wait_until("the badge", |ui| {
        ui.active()
            .forge
            .of("fix/parser-crash")
            .is_some_and(|decoration| decoration.checks.is_some())
    });
    h.read(|ui| {
        let forge = &ui.active().forge;
        let Some(ForgeKind::Script(script)) = forge.kind() else {
            panic!("the forge is the plugin's: {:?}", forge.kind());
        };
        assert_eq!((script.id, script.name), ("forgejo", "Forgejo"));
        assert_eq!(forge.decorations.len(), 1, "{:?}", forge.decorations);
        let decoration = forge.of("fix/parser-crash").unwrap();
        assert_eq!(decoration.pr.number, 7);
        assert_eq!(decoration.pr.state, PrState::Open);
        assert_eq!(decoration.remote.web_url, format!("http://{host}/team/app"));
        let checks = decoration.checks.as_ref().unwrap();
        assert_eq!(checks.state, CheckState::Pending);
        assert_eq!(checks.url.as_deref(), Some("https://ci.test/run/1"));
        assert_eq!(decoration.tip(), "#7 Pull 7 \u{b7} checks running");

        // What the plugin implements is offered; the tab it can't serve isn't.
        assert!(crate::forge::can(ui, ForgeAbility::Create));
        assert!(crate::forge::can(ui, ForgeAbility::Update));
        assert!(!crate::forge::can(ui, ForgeAbility::Tab));
        let bookmark = CommandArg::Bookmark {
            name: "fix/parser-crash".to_owned(),
            remote: None,
            to: None,
        };
        let enabled = |id: &str| ui.registry.get(id).unwrap().enabled(ui, &bookmark);
        assert_eq!(enabled("forge.open"), Ok(()));
        assert_eq!(enabled("forge.update"), Ok(()));
        assert!(enabled("forge.open-tab").is_err());
        // The plugin's own command reads the same store for its when-clause.
        assert_eq!(enabled("forgejo.open-in-browser"), Ok(()));
        assert!(
            crate::when::When::parse("repo.forge == 'forgejo' && bookmark.pr == 'open'")
                .unwrap()
                .eval(&crate::when::AppKeys { ui, arg: &bookmark })
        );
    });
    let seen = server.seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|(_, _, authorization)| {
            authorization.as_deref() == Some(&format!("token token-for-{host}"))
        }),
        "{seen:?}"
    );
}
