//! Output handed over a piece at a time: `exec.stream`, a streamed
//! `http.fetch`, and a forge's diff sent to the host part by part, with the
//! host's pace and its cancel reaching the program underneath.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diffui_plugins::protocol::{ForgeCall, PluginCall, PrRef};
use diffui_plugins::{PluginError, Reply};
use serde_json::{Value, json};

use crate::support::{PATIENCE, Response, Server, Setup, probe, repo, scratch, threw};

#[test]
fn a_programs_output_comes_in_pieces_and_then_its_exit() {
    let plugin = probe(
        "[permissions]\nexec = [[\"sh\"], [\"no-such-program-here\"]]\nenv = [\"GH_HOST\"]\n",
        r#"
  big: async () => {
    const out = await exec.stream(["sh", "-c", "i=0; while [ $i -lt 30000 ]; do echo line $i; i=$((i+1)); done; echo oops >&2; exit 3"]);
    let lines = 0, pieces = 0, longest = 0;
    for await (const piece of out) {
      pieces++;
      lines += piece.split("\n").length - 1;
      longest = Math.max(longest, piece.length);
      if (!piece.endsWith("\n")) throw new Error("a piece cut a line");
    }
    const end = await out.ended;
    return { lines, pieces, longest, ...end };
  },
  env: () => exec(["sh", "-c", "printf %s \"$GH_HOST\""], { env: { GH_HOST: "git.acme.test" } }),
  refusedEnv: () => exec(["sh", "-c", "true"], { env: { PATH: "/tmp" } }),
  missing: () => exec.stream(["no-such-program-here"]),
  early: async () => {
    const out = await exec.stream(["sh", "-c", "yes"]);
    for await (const piece of out) break;
    try { await out.ended; return "ended"; } catch (error) { return error.name; }
  },
"#,
    )
    .spawn();
    plugin.loaded();
    let big = plugin.case("big").unwrap();
    assert_eq!(big["lines"], 30000);
    assert!(big["pieces"].as_u64().unwrap() >= 3, "{big}");
    assert!(big["longest"].as_u64().unwrap() <= 64 * 1024, "{big}");
    assert_eq!(
        (&big["code"], &big["ok"], &big["stderr"]),
        (&json!(3), &json!(false), &json!("oops\n"))
    );
    assert_eq!(plugin.case("env").unwrap()["stdout"], "git.acme.test");
    let refused = threw(plugin.case("refusedEnv"));
    assert_eq!(refused.name, "PermissionError");
    assert!(
        refused.message.contains("can't set PATH"),
        "{}",
        refused.message
    );
    assert_eq!(threw(plugin.case("missing")).name, "NotFoundError");
    assert_eq!(plugin.case("early").unwrap(), json!("AbortError"));
}

#[test]
fn an_exec_permission_can_name_the_program_the_user_set() {
    let plugin = probe(
        "[permissions]\nexec = [[\"{config.program}\", \"-c\"]]\n[config.schema]\nprogram = { type = \"string\", default = \"gh\" }\n",
        r#"
  run: () => exec(["sh", "-c", "echo hi"]),
  other: () => exec(["gh", "-c", "echo hi"]),
"#,
    )
    .config("program = \"sh\"")
    .spawn();
    plugin.loaded();
    assert_eq!(plugin.case("run").unwrap()["stdout"], "hi\n");
    assert_eq!(threw(plugin.case("other")).name, "PermissionError");
}

#[test]
fn a_streamed_response_is_read_in_pieces() {
    let body = "0123456789abcdef\n".repeat(20_000);
    let server = Server::start({
        let body = body.clone();
        move |_| Response::text(200, &body)
    });
    let plugin = probe(
        &format!("[permissions]\nhttp = [\"{}\"]\n", server.host),
        &format!(
            r#"
  stream: async () => {{
    const response = await http.fetch("http://{host}/big", {{ stream: true }});
    let length = 0, pieces = 0;
    for await (const piece of response.body) {{ pieces++; length += piece.length; }}
    return {{ status: response.status, length, pieces }};
  }},
  text: async () => (await (await http.fetch("http://{host}/big", {{ stream: true }})).text()).length,
  plain: async () => (await http.fetch("http://{host}/big")).body,
"#,
            host = server.host
        ),
    )
    .spawn();
    plugin.loaded();
    let seen = plugin.case("stream").unwrap();
    assert_eq!(seen["status"], 200);
    assert_eq!(seen["length"], body.len());
    assert!(seen["pieces"].as_u64().unwrap() >= 3, "{seen}");
    assert_eq!(plugin.case("text").unwrap(), json!(body.len()));
    assert_eq!(plugin.case("plain").unwrap(), Value::Null);
}

#[test]
fn a_forge_error_carries_its_kind() {
    let plugin = probe(
        "",
        r#"
  auth: () => { throw new ForgeError("auth", "token refused", { host: "git.acme.test" }); },
  bad: () => new ForgeError("nope"),
"#,
    )
    .spawn();
    plugin.loaded();
    let error = threw(plugin.case("auth"));
    assert_eq!(
        (error.name.as_str(), error.message.as_str()),
        ("ForgeError", "token refused")
    );
    let forge = error.forge.expect("a forge error");
    assert_eq!(
        (forge.kind.as_str(), forge.host.as_deref()),
        ("auth", Some("git.acme.test"))
    );
    assert!(
        threw(plugin.case("bad"))
            .message
            .contains("is not one of missing-cli")
    );
}

fn pr() -> PrRef {
    PrRef {
        host: "forge.test".into(),
        owner: "team".into(),
        name: "app".into(),
        number: 7,
        web_url: "https://forge.test/team/app".into(),
    }
}

/// A forge whose diff is a program that writes forever, and whose commit
/// diff is one file.
fn endless(pid_file: &std::path::Path) -> Setup<'static> {
    Setup::new(
        "[permissions]\nexec = [[\"sh\"]]\n",
        &format!(
            r#"import {{ commands, exec, forge }} from "diffui";
commands.register("t.run", async () => {{}});
forge.register({{
  async pullRequests() {{ return []; }},
  async *diff(pr, {{ signal }}) {{
    const out = await exec.stream(["sh", "-c", `echo $$ > '{pid}'; i=0; while true; do echo "+line $i of ${{pr.number}}"; i=$((i+1)); done`], {{ signal }});
    yield `diff --git a/x b/x\n`;
    yield* out;
  }},
  async commitDiff(pr, oid) {{
    return {{ diff: [{{ path: "a.rs", status: "added", patch: "@@ -0,0 +1 @@\n+x", additions: 1 }}], details: {{ message: `msg of ${{oid}}` }} }};
  }},
}});
"#,
            pid = pid_file.display()
        ),
    )
}

#[test]
fn a_diff_streams_at_the_hosts_pace_and_a_cancel_reaches_the_program() {
    let pid_file = scratch("pid").join("pid");
    let plugin = endless(&pid_file).spawn();
    let registration = plugin.loaded();
    assert_eq!(
        registration.forge.unwrap(),
        ["pullRequests", "commitDiff", "diff"]
    );

    // Take three parts, then sit on the fourth.
    let held: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (done, finished) = mpsc::channel();
    let id = plugin.plugin.call_streaming::<Value>(
        PluginCall::Forge {
            repo: repo(),
            call: ForgeCall::Diff { pr: pr() },
        },
        {
            let (held, seen) = (held.clone(), seen.clone());
            move |part, reply| {
                let mut seen = seen.lock().unwrap();
                seen.push(part);
                if seen.len() < 4 {
                    reply.ok(Value::Null);
                } else {
                    held.lock().unwrap().push(reply);
                }
            }
        },
        move |result| {
            let _ = done.send(result);
        },
    );
    let started = Instant::now();
    while seen.lock().unwrap().len() < 4 {
        assert!(started.elapsed() < PATIENCE, "the parts never came");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "the script didn't wait for the host");
        assert_eq!(seen[0], "diff --git a/x b/x\n");
        assert!(seen[1].as_str().unwrap().starts_with("+line 0 of 7\n"));
    }
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&pid));

    plugin.plugin.abort(id);
    assert_eq!(
        finished.recv_timeout(PATIENCE).unwrap(),
        Err(PluginError::Gone)
    );
    let started = Instant::now();
    while alive(&pid) {
        assert!(
            started.elapsed() < PATIENCE,
            "the program outlived the cancel"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(held);

    // The plugin is fine afterwards, and a commit diff sends its file and
    // then its details.
    let parts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (done, finished) = mpsc::channel();
    plugin.plugin.call_streaming::<Value>(
        PluginCall::Forge {
            repo: repo(),
            call: ForgeCall::CommitDiff {
                pr: pr(),
                oid: "c0ffee".into(),
            },
        },
        {
            let parts = parts.clone();
            move |part, reply| {
                parts.lock().unwrap().push(part);
                reply.ok(Value::Null);
            }
        },
        move |result| {
            let _ = done.send(result);
        },
    );
    let result = finished.recv_timeout(PATIENCE).unwrap().unwrap();
    assert_eq!(result["details"]["message"], "msg of c0ffee");
    assert_eq!(parts.lock().unwrap()[0]["path"], "a.rs");
}
