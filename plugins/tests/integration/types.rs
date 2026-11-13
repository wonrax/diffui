//! `diffui.d.ts` against what the runtime installs, so the two can't drift:
//! the module's exports, each namespace's members, the handle and result
//! objects, the globals, and the fields of the protocol's data types.

use std::collections::BTreeSet;

use diffui_plugins::protocol::{
    Bookmark, CheckState, Checks, Choice, CreatePullRequest, Diff, EVENT_NAMES, FORGE_METHODS,
    JjRun, PrState, PullRequest, Remote, RemoteBookmark, RevisionRef, Row, Selection,
    UpdatePullRequest,
};
use serde_json::{Value, json};

use crate::support::{Response, Server, Setup, command, repo};

const TYPES: &str = diffui_plugins::TYPES;
const HOST_MODULE: &str = include_str!("../../src/js/diffui.js");

/// The globals that exist only to say what to use instead.
const STUBS: [&str; 2] = ["fetch", "require"];

fn set<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    names.into_iter().map(str::to_string).collect()
}

/// The lines between `opening` and the brace that closes it.
fn block(opening: &str) -> Vec<&'static str> {
    let mut lines = TYPES
        .lines()
        .skip_while(|line| line.trim() != opening)
        .skip(1)
        .peekable();
    assert!(lines.peek().is_some(), "diffui.d.ts has no `{opening}`");
    let mut depth = 1;
    let mut inside = Vec::new();
    for line in lines {
        let code = line.trim();
        let comment = code.starts_with("/**") || code.starts_with('*') || code.starts_with("//");
        if !comment {
            if depth == 1 && !code.starts_with('}') && !code.is_empty() {
                inside.push(code);
            }
            depth += code.matches('{').count();
            depth -= code.matches('}').count();
            if depth == 0 {
                break;
            }
        }
    }
    inside
}

/// The instance members a block declares: properties and methods by name,
/// without statics, constructors and symbol-keyed members.
fn members(opening: &str) -> BTreeSet<String> {
    block(opening)
        .into_iter()
        .filter(|line| {
            !line.starts_with("static ")
                && !line.starts_with("constructor(")
                && !line.starts_with('[')
        })
        .map(|line| {
            let line = line.strip_prefix("readonly ").unwrap_or(line);
            let end = line
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(line.len());
            line[..end].to_string()
        })
        .collect()
}

fn interface(name: &str) -> BTreeSet<String> {
    members(&format!("export interface {name} {{"))
}

/// What a top-level declaration introduces: `export const x`, `declare class X`.
fn declared(keyword: &str, lines: impl IntoIterator<Item = &'static str>) -> BTreeSet<String> {
    lines
        .into_iter()
        .filter_map(|line| {
            let rest = line.strip_prefix(keyword)?.trim_start();
            let rest = ["const ", "function ", "class "]
                .iter()
                .find_map(|kind| rest.strip_prefix(kind))?;
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        })
        .collect()
}

fn names(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("not a list of names: {value}"))
        .iter()
        .map(|name| name.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn the_type_definitions_declare_exactly_what_the_runtime_installs() {
    let globals = declared(
        "declare",
        TYPES.lines().filter(|line| !line.starts_with(' ')),
    );
    let server = Server::start(|_| Response::text(200, "ok"));
    let plugin = Setup::new(
        &format!("[permissions]\nhttp = [\"{}\"]\nexec = [[\"true\"]]\n", server.host),
        &format!(
            r#"import * as diffui from "diffui";
const {{ auth, commands, config, events, exec, forge, http, kv, ui }} = diffui;
const own = (object) => Object.keys(object);
const inherited = (object) => Object.getOwnPropertyNames(Object.getPrototypeOf(object)).filter((name) => name !== "constructor");
const all = (object) => [...own(object), ...inherited(object)];
let context;
commands.register("t.run", (ctx) => {{ context = own(ctx); }});
forge.register({{
  async pullRequests() {{ return []; }},
  async checks(repo) {{
    const response = await http.fetch("http://{host}/");
    const controller = new AbortController();
    return {{
      module: own(diffui),
      commands: own(commands), forge: own(forge), events: own(events), http: own(http), auth: own(auth), kv: own(kv),
      ui: own(ui), prompt: own(ui.prompt), clipboard: own(ui.clipboard), status: own(ui.status),
      activity: own(await ui.activity("x")),
      context, repo: all(repo), jj: own(repo.jj), forgeRemote: own(repo.forgeRemote),
      response: all(response), headers: inherited(response.headers), execResult: own(await exec(["true"])),
      console: own(console),
      url: inherited(new URL("https://x.test")), params: inherited(new URLSearchParams()),
      encoder: inherited(new TextEncoder()), decoder: inherited(new TextDecoder()),
      controller: all(controller), signal: all(controller.signal),
      globals: {globals:?}.filter((name) => typeof globalThis[name] !== "undefined"),
    }};
  }},
}});
"#,
            host = server.host,
            globals = globals.iter().collect::<Vec<_>>(),
        ),
    )
    .spawn();
    plugin.loaded();
    plugin.call::<Value>(command("t.run")).unwrap();
    let seen = plugin.case("surface").unwrap();

    // The module's exports.
    let exports = declared("export", block("declare module \"diffui\" {"));
    assert_eq!(names(&seen["module"]), exports);
    assert!(plugin_exports_are_listed_in_the_import_error(&exports));

    // Each namespace and object, member by member.
    for (key, interface_name) in [
        ("commands", "Commands"),
        ("forge", "Forge"),
        ("events", "Events"),
        ("http", "Http"),
        ("auth", "Auth"),
        ("kv", "Kv"),
        ("ui", "Ui"),
        ("prompt", "Prompt"),
        ("clipboard", "Clipboard"),
        ("status", "Status"),
        ("activity", "Activity"),
        ("context", "CommandContext"),
        ("repo", "Repo"),
        ("jj", "Jj"),
        ("forgeRemote", "ForgeRemote"),
        ("response", "Response"),
        ("headers", "Headers"),
        ("execResult", "ExecResult"),
    ] {
        assert_eq!(
            names(&seen[key]),
            interface(interface_name),
            "interface {interface_name}"
        );
    }
    assert_eq!(interface("EventMap"), set(EVENT_NAMES));
    assert_eq!(interface("ForgeProvider"), set(FORGE_METHODS));

    // The globals: declared ones exist, and the host module adds no others.
    assert_eq!(names(&seen["globals"]), globals);
    let installed: BTreeSet<String> = HOST_MODULE
        .lines()
        .filter_map(|line| line.strip_prefix("globalThis.")?.split([' ', '=']).next())
        .filter(|name| !STUBS.contains(name))
        .map(str::to_string)
        .collect();
    assert_eq!(installed, globals);
    for (key, class) in [
        ("url", "URL"),
        ("params", "URLSearchParams"),
        ("encoder", "TextEncoder"),
        ("decoder", "TextDecoder"),
        ("controller", "AbortController"),
        ("signal", "AbortSignal"),
    ] {
        assert_eq!(
            names(&seen[key]),
            members(&format!("declare class {class} {{")),
            "class {class}"
        );
    }
    assert_eq!(names(&seen["console"]), members("declare const console: {"));
}

/// The bare-import error names the exports; it reads the same list.
fn plugin_exports_are_listed_in_the_import_error(exports: &BTreeSet<String>) -> bool {
    let plugin = Setup::new("", "import \"left-pad\";\n").spawn();
    let listed = exports.iter().cloned().collect::<Vec<_>>().join(", ");
    plugin.failed().contains(&format!("\"diffui\" ({listed})"))
}

#[test]
fn the_data_types_have_the_fields_the_protocol_sends() {
    fn fields(value: impl serde::Serialize) -> BTreeSet<String> {
        match serde_json::to_value(value).unwrap() {
            Value::Object(map) => map.keys().cloned().collect(),
            other => panic!("not an object: {other}"),
        }
    }
    let text = String::new;
    assert_eq!(
        fields(repo().forge_remote.unwrap()),
        interface("ForgeRemote")
    );
    assert_eq!(
        fields(Bookmark {
            name: text(),
            targets: vec![],
            remotes: vec![]
        }),
        interface("Bookmark")
    );
    assert_eq!(
        fields(RemoteBookmark {
            remote: text(),
            target: text(),
            tracked: true
        }),
        interface("RemoteBookmark")
    );
    assert_eq!(
        fields(Remote {
            name: text(),
            url: None
        }),
        interface("Remote")
    );
    assert_eq!(fields(Selection::default()), interface("Selection"));
    assert_eq!(
        fields(RevisionRef {
            change_id: text(),
            commit_id: text()
        }),
        interface("RevisionRef")
    );
    assert_eq!(
        fields(Row {
            change_id: text(),
            commit_id: text(),
            description: text(),
            author: text(),
            timestamp: None,
            bookmarks: vec![],
            working_copy: false,
            immutable: false,
            empty: false,
            conflict: false,
        }),
        interface("Row")
    );
    assert_eq!(
        fields(Diff {
            text: text(),
            truncated: false
        }),
        interface("Diff")
    );
    assert_eq!(fields(JjRun { summary: text() }), interface("JjRun"));
    assert_eq!(
        fields(PullRequest {
            number: 1,
            url: text(),
            title: text(),
            head: text(),
            base: text(),
            head_sha: text(),
            state: PrState::Open,
            review: None,
        }),
        interface("PullRequest")
    );
    assert_eq!(
        fields(Checks {
            state: CheckState::None,
            url: None
        }),
        interface("Checks")
    );
    assert_eq!(
        fields(CreatePullRequest {
            head: text(),
            base: text(),
            title: text(),
            body: text(),
            draft: false,
        }),
        interface("CreatePullRequest")
    );
    assert_eq!(
        fields(UpdatePullRequest {
            title: Some(text()),
            body: Some(text()),
            base: Some(text()),
        }),
        interface("UpdatePullRequest")
    );
    assert_eq!(
        fields(Choice {
            label: text(),
            hint: None,
            value: text()
        }),
        interface("Choice")
    );
    // The unions are spelled the way serde spells the enums.
    for (spelled, values) in [
        (
            "state: \"open\" | \"draft\" | \"closed\" | \"merged\";",
            json!([
                PrState::Open,
                PrState::Draft,
                PrState::Closed,
                PrState::Merged
            ]),
        ),
        (
            "state: \"pending\" | \"success\" | \"failure\" | \"none\";",
            json!([
                CheckState::Pending,
                CheckState::Success,
                CheckState::Failure,
                CheckState::None
            ]),
        ),
    ] {
        let union = values
            .as_array()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            spelled.contains(&union) && TYPES.contains(spelled),
            "{spelled}"
        );
    }
}
