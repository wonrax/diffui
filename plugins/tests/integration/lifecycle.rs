//! Loading, failing, limits, reload and isolation.

use std::time::{Duration, Instant};

use diffui_plugins::protocol::{Event, PluginCall};
use diffui_plugins::{Limits, PluginError, PluginState};
use serde_json::{Value, json};

use crate::support::{PATIENCE, Setup, command, probe, repo, threw};

#[test]
fn a_plugin_loads_and_says_what_it_registered() {
    let plugin = Setup::new(
        "",
        r#"import { commands, events, forge } from "diffui";
commands.register("t.run", async (ctx) => { console.log("ran on", ctx.bookmark, ctx.repo.root); });
events.on("pushed", () => {});
forge.register({ async pullRequests() { return []; }, async checks() { return { state: "none" }; } });
"#,
    )
    .spawn();
    let registration = plugin.loaded();
    assert_eq!(registration.commands, ["t.run"]);
    assert_eq!(
        registration.forge.as_deref(),
        Some(&["pullRequests".to_string(), "checks".to_string()][..])
    );
    assert_eq!(registration.events, ["pushed"]);
    assert_eq!(plugin.plugin.state(), PluginState::Ready(registration));

    assert_eq!(plugin.run_command().unwrap(), Value::Null);
    assert!(
        plugin.log_text().contains("Info ran on feature /work/app"),
        "{}",
        plugin.log_text()
    );

    // A method the provider doesn't have is refused without entering the script.
    let create = PluginCall::Forge {
        repo: repo(),
        call: diffui_plugins::protocol::ForgeCall::UpdatePullRequest {
            number: 1,
            patch: Default::default(),
        },
    };
    assert_eq!(
        plugin.call::<Value>(create).unwrap_err(),
        PluginError::NotRegistered("forge method updatePullRequest".into())
    );
    assert_eq!(
        plugin
            .call::<Value>(command("t.other"))
            .unwrap_err()
            .to_string(),
        "the plugin registered no command \"t.other\""
    );
}

#[test]
fn calls_made_while_loading_wait_for_it() {
    let plugin = Setup::new(
        "",
        r#"import { commands, forge } from "diffui";
await new Promise((resolve) => setTimeout(resolve, 150));
commands.register("t.run", async () => {});
forge.register({ async pullRequests() { return ["late"]; } });
"#,
    )
    .spawn();
    let early = plugin.start::<Value>(PluginCall::Forge {
        repo: repo(),
        call: diffui_plugins::protocol::ForgeCall::PullRequests { branches: vec![] },
    });
    assert_eq!(plugin.plugin.state(), PluginState::Loading);
    plugin.loaded();
    assert_eq!(
        early.recv_timeout(PATIENCE).unwrap().unwrap(),
        json!(["late"])
    );
}

#[test]
fn registration_must_match_the_manifest() {
    let undeclared = Setup::new(
        "",
        "import { commands } from \"diffui\";\ncommands.register(\"t.surprise\", () => {});\n",
    )
    .spawn();
    let why = undeclared.failed();
    assert_eq!(
        why,
        "main.js did not load: main.js:2:10: Error: Command \"t.surprise\" is not declared in plugin.toml. Add a [[commands]] row for it. Declared: \"t.run\"."
    );
    assert_eq!(undeclared.plugin.state(), PluginState::Failed(why.clone()));
    assert_eq!(
        undeclared.run_command().unwrap_err(),
        PluginError::Failed(why)
    );

    let missing = Setup::new("", "import { forge } from \"diffui\";\nforge.register({ async pullRequests() { return []; } });\n").spawn();
    assert_eq!(
        missing.failed(),
        "plugin.toml declares a command but main.js never called commands.register for t.run"
    );

    let no_forge = Setup::new(
        "",
        "import { commands } from \"diffui\";\ncommands.register(\"t.run\", () => {});\n",
    )
    .spawn();
    assert_eq!(
        no_forge.failed(),
        "plugin.toml provides a forge but main.js never called forge.register"
    );

    let late = probe(
        "",
        "late: () => { commands.register(\"t.run\", () => {}); },",
    )
    .spawn();
    late.loaded();
    assert!(
        threw(late.case("late"))
            .message
            .starts_with("commands.register must run while the plugin loads")
    );
}

#[test]
fn a_bare_import_says_what_can_be_imported() {
    let plugin = Setup::new("", "import fs from \"node:fs\";\n").spawn();
    assert_eq!(
        plugin.failed(),
        "main.js did not load: ReferenceError: Cannot import \"node:fs\" (from main.js). A plugin can import \"diffui\" (auth, commands, config, events, exec, forge, http, kv, ui) and its own files by relative path (\"./util.js\"). There is no Node, npm or require here."
    );
}

#[test]
fn relative_imports_stay_inside_the_plugin() {
    let plugin = Setup::new(
        "",
        r#"import { commands, forge } from "diffui";
import { helper } from "./lib/util.js";
commands.register("t.run", () => {});
forge.register({ async pullRequests() { return []; }, async checks() { return helper(); } });
"#,
    )
    .file(
        "lib/util.js",
        "import { base } from \"../shared.js\";\nexport const helper = () => base + 1;\n",
    )
    .file("shared.js", "export const base = 41;\n")
    .spawn();
    plugin.loaded();
    assert_eq!(plugin.case("anything").unwrap(), json!(42));

    let outside = Setup::new("", "import \"../other/main.js\";\n").spawn();
    assert_eq!(
        outside.failed(),
        "main.js did not load: ReferenceError: Cannot import \"../other/main.js\" (from main.js): it is outside the plugin's directory"
    );

    let absent = Setup::new("", "import \"./utils.js\";\n")
        .file("util.js", "")
        .spawn();
    assert_eq!(
        absent.failed(),
        "main.js did not load: ReferenceError: Cannot import \"./utils.js\" (from main.js): there is no utils.js in the plugin's directory. It has: main.js, util.js."
    );
}

#[test]
fn an_exception_carries_its_file_line_and_stack() {
    let plugin = Setup::new(
        "",
        r#"import { commands, forge } from "diffui";
import { explode } from "./lib/boom.js";
commands.register("t.run", async () => {
  explode();
});
forge.register({ async pullRequests() { return []; } });
"#,
    )
    .file(
        "lib/boom.js",
        "export function explode() {\n  const nothing = undefined;\n  return nothing.field;\n}\n",
    )
    .spawn();
    plugin.loaded();
    let error = threw(plugin.run_command());
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.location(), Some("lib/boom.js:3:10"));
    assert_eq!(
        error.to_string(),
        "lib/boom.js:3:10: TypeError: cannot read property 'field' of undefined"
    );
    assert!(
        error.stack.contains("at explode (lib/boom.js:3:10)"),
        "{}",
        error.stack
    );
    assert!(error.stack.contains("(main.js:4:3)"), "{}", error.stack);
    // The plugin is still there, and the failure is in its log.
    assert!(matches!(plugin.plugin.state(), PluginState::Ready(_)));
    assert!(
        plugin
            .log_text()
            .contains("Error command \"t.run\" failed: lib/boom.js:3:10: TypeError")
    );

    let syntax = Setup::new("", "const fine = 1;\nconst broken = ;\n").spawn();
    assert_eq!(
        syntax.failed(),
        "main.js did not load: main.js:2:16: SyntaxError: unexpected token in expression: ';'"
    );

    let thrown = Setup::new(
        "",
        "const config = {};\nthrow new Error(`no host in ${JSON.stringify(config)}`);\n",
    )
    .spawn();
    assert_eq!(
        thrown.failed(),
        "main.js did not load: main.js:2:46: Error: no host in {}"
    );
}

#[test]
fn a_busy_loop_is_stopped_within_a_second_and_a_reload_recovers() {
    let plugin = probe(
        "",
        "spin: () => { while (true) {} },\n  ok: () => \"fine\",",
    )
    .spawn();
    plugin.loaded();
    let started = Instant::now();
    let error = plugin.case("spin").unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "took {:?}",
        started.elapsed()
    );
    let why = "it ran for more than 500 ms without yielding and was stopped";
    assert_eq!(error, PluginError::Failed(why.into()));
    assert_eq!(plugin.failed(), why);
    // Failed means failed: nothing runs until a reload.
    assert_eq!(
        plugin.case("ok").unwrap_err(),
        PluginError::Failed(why.into())
    );

    plugin.plugin.reload();
    plugin.loaded();
    assert_eq!(plugin.case("ok").unwrap(), json!("fine"));
}

#[test]
fn a_chain_of_microtasks_is_a_busy_loop_too() {
    let plugin = probe("", "spin: async () => { while (true) await null; },").spawn();
    plugin.loaded();
    let started = Instant::now();
    assert!(matches!(plugin.case("spin"), Err(PluginError::Failed(_))));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_busy_top_level_fails_the_load() {
    let plugin = Setup::new("", "while (true) {}\n").spawn();
    assert_eq!(
        plugin.failed(),
        "it ran for more than 500 ms without yielding and was stopped"
    );

    let limits = Limits {
        load: Duration::from_millis(200),
        ..Limits::default()
    };
    let stuck = Setup::new("", "await new Promise(() => {});\n")
        .limits(limits)
        .spawn();
    assert_eq!(
        stuck.failed(),
        "main.js did not finish loading within 0 s: a top-level await never settled"
    );
}

#[test]
fn the_memory_limit_fails_the_plugin_not_the_host() {
    let limits = Limits {
        memory: 8 * 1024 * 1024,
        turn: Duration::from_secs(20),
        ..Limits::default()
    };
    let plugin = probe(
        "",
        "hoard: () => { const kept = []; while (true) kept.push(new Array(10000).fill(\"x\")); },",
    )
    .limits(limits)
    .spawn();
    plugin.loaded();
    let why = "it ran out of memory (the limit is 8 MB)";
    assert_eq!(
        plugin.case("hoard").unwrap_err(),
        PluginError::Failed(why.into())
    );
    assert_eq!(plugin.failed(), why);
}

#[test]
fn flooding_the_host_counts_against_the_memory_limit() {
    let limits = Limits {
        memory: 8 * 1024 * 1024,
        turn: Duration::from_secs(20),
        ..Limits::default()
    };
    let plugin = probe(
        "",
        "flood: () => { const line = \"x\".repeat(64 * 1024); while (true) console.log(line); },\n  shout: () => console.log(\"é\".repeat(20000)),",
    )
    .limits(limits)
    .spawn();
    plugin.loaded();
    // One long line is cut, not kept whole.
    plugin.case("shout").unwrap();
    let shouted = plugin.plugin.log().pop().unwrap().message;
    assert!(
        shouted.len() < 17 * 1024 && shouted.ends_with("é …"),
        "{}",
        shouted.len()
    );

    let why = "it ran out of memory (the limit is 8 MB)";
    assert_eq!(
        plugin.case("flood").unwrap_err(),
        PluginError::Failed(why.into())
    );
}

#[test]
fn deep_recursion_is_an_error_the_script_can_catch() {
    let plugin = probe(
        "",
        "deep: () => { const down = (n) => down(n + 1) + 1; try { return down(0); } catch (error) { return error.name + \": \" + error.message; } },",
    )
    .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("deep").unwrap(),
        json!("RangeError: Maximum call stack size exceeded")
    );
}

#[test]
fn reload_reads_the_files_again_and_re_registers() {
    let plugin = Setup::new(
        "",
        r#"import { commands, events, forge } from "diffui";
commands.register("t.run", () => {});
events.on("fetched", () => {});
forge.register({ async pullRequests() { return ["one"]; }, async checks() { return new Promise(() => {}); } });
"#,
    )
    .spawn();
    let first = plugin.loaded();
    assert_eq!(first.events, ["fetched"]);
    let list = || {
        plugin.call::<Value>(PluginCall::Forge {
            repo: repo(),
            call: diffui_plugins::protocol::ForgeCall::PullRequests { branches: vec![] },
        })
    };
    assert_eq!(list().unwrap(), json!(["one"]));
    let never = plugin.start::<Value>(crate::support::case("x", repo()));

    std::fs::write(
        plugin.dir.join("main.js"),
        r#"import { commands, events, forge } from "diffui";
commands.register("t.run", () => {});
events.on("pushed", () => {});
forge.register({ async pullRequests() { return ["two"]; } });
"#,
    )
    .unwrap();
    plugin.plugin.reload();
    let second = plugin.loaded();
    assert_eq!(second.events, ["pushed"]);
    assert_eq!(
        second.forge.as_deref(),
        Some(&["pullRequests".to_string()][..])
    );
    assert_eq!(list().unwrap(), json!(["two"]));
    // The call that was out when the script went away is answered.
    assert_eq!(
        never.recv_timeout(PATIENCE).unwrap().unwrap_err(),
        PluginError::Gone
    );
}

#[test]
fn two_plugins_run_without_blocking_each_other() {
    let limits = Limits {
        turn: Duration::from_secs(5),
        ..Limits::default()
    };
    let busy = probe(
        "",
        "spin: () => { const until = Date.now() + 1500; while (Date.now() < until) {} return \"spun\"; },",
    )
    .limits(limits)
    .spawn();
    let quick = probe("", "ping: () => \"pong\",").spawn();
    busy.loaded();
    quick.loaded();

    let spinning = busy.start::<Value>(crate::support::case("spin", repo()));
    std::thread::sleep(Duration::from_millis(50));
    for _ in 0..5 {
        let asked = Instant::now();
        assert_eq!(quick.case("ping").unwrap(), json!("pong"));
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "the quick plugin waited {:?}",
            asked.elapsed()
        );
    }
    // The busy one was busy the whole time, and still finishes.
    assert!(spinning.try_recv().is_err());
    assert_eq!(
        spinning.recv_timeout(PATIENCE).unwrap().unwrap(),
        json!("spun")
    );
}

#[test]
fn one_plugin_answers_while_an_earlier_handler_awaits() {
    let plugin = probe(
        "",
        "slow: () => new Promise((resolve) => setTimeout(() => resolve(\"slow\"), 300)),\n  fast: () => \"fast\",",
    )
    .spawn();
    plugin.loaded();
    let slow = plugin.start::<Value>(crate::support::case("slow", repo()));
    assert_eq!(plugin.case("fast").unwrap(), json!("fast"));
    assert!(slow.try_recv().is_err());
    assert_eq!(slow.recv_timeout(PATIENCE).unwrap().unwrap(), json!("slow"));
}

#[test]
fn a_forge_method_that_never_settles_times_out() {
    let limits = Limits {
        forge_call: Duration::from_millis(150),
        ..Limits::default()
    };
    let plugin = probe("", "hang: () => new Promise(() => {}),")
        .limits(limits)
        .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("hang").unwrap_err().to_string(),
        "forge.checks did not finish in time"
    );
}

#[test]
fn a_handler_returning_the_wrong_shape_is_a_bad_reply() {
    let plugin = probe("", "odd: () => ({ state: \"sideways\" }),").spawn();
    plugin.loaded();
    let error = plugin
        .call::<diffui_plugins::protocol::Checks>(crate::support::case("odd", repo()))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "checks returned the wrong shape: unknown variant `sideways`, expected one of `pending`, `success`, `failure`, `none`"
    );
}

#[test]
fn events_reach_their_listeners_and_a_throwing_one_is_logged() {
    let plugin = Setup::new(
        "",
        r#"import { commands, events, forge, kv } from "diffui";
commands.register("t.run", () => {});
forge.register({ async pullRequests() { return []; }, async checks() { return kv.get("heard"); } });
events.on("pushed", async ({ repo, bookmarks }) => {
  await kv.set("heard", { root: repo.root, bookmarks });
});
events.on("pushed", () => {
  throw new Error("listener broke");
});
"#,
    )
    .spawn();
    plugin.loaded();
    let pushed = PluginCall::Event {
        event: Event::Pushed {
            repo: repo(),
            bookmarks: vec!["feature".into()],
        },
    };
    assert_eq!(plugin.call::<Value>(pushed).unwrap(), Value::Null);
    // Nobody listens for this one; it is answered without entering the script.
    let fetched = PluginCall::Event {
        event: Event::Fetched { repo: repo() },
    };
    assert_eq!(plugin.call::<Value>(fetched).unwrap(), Value::Null);

    let deadline = Instant::now() + PATIENCE;
    while plugin.case("heard").unwrap() == Value::Null {
        assert!(Instant::now() < deadline, "the listener never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        plugin.case("heard").unwrap(),
        json!({ "root": "/work/app", "bookmarks": ["feature"] })
    );
    assert!(
        plugin
            .log_text()
            .contains("Error Uncaught in pushed listener: main.js:8:13: Error: listener broke"),
        "{}",
        plugin.log_text()
    );
}

#[test]
fn a_floating_rejection_is_logged_with_its_place() {
    let plugin = probe(
        "",
        "float: () => { Promise.reject(new Error(\"nobody awaited me\")); return \"done\"; },\n  handled: async () => { const failing = Promise.reject(new Error(\"caught later\")); try { await failing; } catch { return \"caught\"; } },",
    )
    .spawn();
    plugin.loaded();
    assert_eq!(plugin.case("handled").unwrap(), json!("caught"));
    assert!(
        !plugin.log_text().contains("caught later"),
        "{}",
        plugin.log_text()
    );
    assert_eq!(plugin.case("float").unwrap(), json!("done"));
    assert!(
        plugin
            .log_text()
            .contains("Error Uncaught main.js:5:35: Error: nobody awaited me"),
        "{}",
        plugin.log_text()
    );
}
