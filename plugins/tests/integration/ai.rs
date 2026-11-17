//! The AI providers diffui ships, from memory as the app runs them, against
//! local servers standing in for Anthropic and OpenAI and scripts standing in
//! for `claude` and `codex`. Nothing here reaches a real API or program.

#[cfg(unix)]
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

use diffui_plugins::protocol::{AiCall, AiRequest, PluginCall};
use diffui_plugins::secrets::{self, FileStore, SecretStore};
use diffui_plugins::{BUNDLED, PluginError};
use serde_json::{Value, json};

#[cfg(unix)]
use crate::support::read_request;
use crate::support::{PATIENCE, Request, Response, Server, Setup, TestPlugin, scratch};

/// Keys in the shell would stand in for the stored ones.
fn without_keys() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: before any of this module's plugins start, and nothing in
        // this process reads these variables otherwise.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }
    });
}

/// The bundled plugin `id` with `config` (a `[plugins.<id>]` body) and a
/// store holding `key` as its `api_key`.
fn provider(id: &str, config: &str, key: Option<&str>) -> TestPlugin {
    without_keys();
    let (_, files) = BUNDLED
        .iter()
        .find(|(bundled, _)| *bundled == id)
        .expect("a bundled provider");
    let manifest = files
        .iter()
        .find(|(name, _)| *name == "plugin.toml")
        .map(|(_, text)| text.to_string())
        .unwrap();
    let mut setup = Setup::with_manifest(manifest, "");
    setup.files = files
        .iter()
        .filter(|(name, _)| *name != "plugin.toml")
        .map(|(name, text)| (*name, text.to_string()))
        .collect();
    let store = Arc::new(FileStore::new(scratch("secrets").join("secrets.json")));
    if let Some(key) = key {
        store
            .set(&secrets::account(id, "api_key", None), key)
            .unwrap();
    }
    let plugin = setup.config(config).secrets(store).spawn_bundled();
    let registration = plugin.loaded();
    assert!(registration.ai.is_some(), "{registration:?}");
    plugin
}

fn request(prompt: &str, input: Option<&str>) -> AiRequest {
    AiRequest {
        prompt: prompt.into(),
        input: input.map(Into::into),
        ..Default::default()
    }
}

/// Run `complete`: the text it sent, and how it ended.
fn complete(plugin: &TestPlugin, request: AiRequest) -> (String, Result<Value, PluginError>) {
    let text = Arc::new(Mutex::new(String::new()));
    let (done, finished) = mpsc::channel();
    plugin.plugin.call_streaming::<Value>(
        PluginCall::Ai {
            call: AiCall::Complete { request },
        },
        {
            let text = text.clone();
            move |parts, reply| {
                for part in parts {
                    text.lock().unwrap().push_str(part.as_str().expect("text"));
                }
                reply.ok(Value::Null);
            }
        },
        move |result| {
            let _ = done.send(result);
        },
    );
    let result = finished.recv_timeout(PATIENCE).expect("it settled");
    let text = text.lock().unwrap().clone();
    (text, result)
}

/// The kind of the `AiError` a call ended with, and its message.
fn ai_error(result: Result<Value, PluginError>) -> (String, String) {
    match result {
        Err(PluginError::Threw(error)) => (
            error
                .ai
                .clone()
                .unwrap_or_else(|| panic!("not an AiError: {error}")),
            error.message,
        ),
        other => panic!("expected an AiError, got {other:?}"),
    }
}

fn sse(events: &[(&str, Value)]) -> Response {
    let body: String = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect();
    Response {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        body,
    }
}

fn anthropic_reply(texts: &[&str], stop: &str) -> Response {
    let mut events = vec![(
        "message_start",
        json!({ "type": "message_start", "message": { "id": "msg_1" } }),
    )];
    events.push((
        "content_block_start",
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
    ));
    for text in texts {
        events.push((
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        ));
    }
    events.push((
        "message_delta",
        json!({ "type": "message_delta", "delta": { "stop_reason": stop } }),
    ));
    events.push(("message_stop", json!({ "type": "message_stop" })));
    sse(&events)
}

fn anthropic_on(server: &Server) -> String {
    format!("base_url = \"http://{}\"\n", server.host)
}

#[test]
fn anthropic_streams_the_reply_from_the_messages_api() {
    let server = Server::start(|_| anthropic_reply(&["Fix the ", "parser crash"], "end_turn"));
    let plugin = provider("anthropic", &anthropic_on(&server), Some("sk-test"));
    let (text, result) = complete(&plugin, request("Describe it.", Some("diff --git a b")));
    result.unwrap();
    assert_eq!(text, "Fix the parser crash");

    let seen = server.seen();
    let asked = &seen[0];
    assert_eq!(
        (asked.method.as_str(), asked.path.as_str()),
        ("POST", "/v1/messages")
    );
    assert_eq!(asked.header("x-api-key"), Some("sk-test"));
    assert_eq!(asked.header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(asked.header("content-type"), Some("application/json"));
    let body: Value = serde_json::from_str(&asked.body).unwrap();
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 64000);
    for refused in ["temperature", "top_p", "top_k"] {
        assert!(body.get(refused).is_none(), "{body}");
    }
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["text"], "diff --git a b");
    assert_eq!(content[1]["text"], "Describe it.");
}

#[test]
fn anthropic_failures_are_typed() {
    let answer = |request: &Request| -> Response {
        let body: Value = serde_json::from_str(&request.body).unwrap_or_default();
        let prompt = body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        let error = |status, kind: &str| {
            Response::json(
                status,
                json!({ "type": "error", "error": { "type": kind, "message": "nope" } }),
            )
        };
        match prompt {
            "401" => error(401, "authentication_error"),
            "429" => error(429, "rate_limit_error"),
            "refuse" => anthropic_reply(&["I can"], "refusal"),
            "long" => anthropic_reply(&["and so"], "max_tokens"),
            "mid" => sse(&[
                (
                    "content_block_delta",
                    json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Half" } }),
                ),
                (
                    "error",
                    json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
                ),
            ]),
            "cut" => sse(&[(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Half" } }),
            )]),
            _ => error(500, "api_error"),
        }
    };
    let server = Server::start(answer);
    let plugin = provider("anthropic", &anthropic_on(&server), Some("sk-test"));
    let kind = |prompt: &str| ai_error(complete(&plugin, request(prompt, None)).1);
    assert_eq!(kind("401").0, "auth");
    assert_eq!(kind("429").0, "rate-limited");
    let (refused, why) = kind("refuse");
    assert_eq!(refused, "failed");
    assert!(why.contains("declined"), "{why}");
    let (long, why) = kind("long");
    assert_eq!(long, "failed");
    assert!(why.contains("64000 tokens"), "{why}");
    let (mid, why) = kind("mid");
    assert_eq!(mid, "rate-limited");
    assert!(why.contains("Overloaded"), "{why}");
    assert_eq!(kind("cut").0, "unreachable");
    assert_eq!(kind("500").0, "failed");

    let keyless = provider("anthropic", &anthropic_on(&server), None);
    let (kind, why) = ai_error(complete(&keyless, request("hi", None)).1);
    assert_eq!(kind, "auth");
    assert!(why.contains("no Anthropic API key"), "{why}");

    let unreachable = provider(
        "anthropic",
        "base_url = \"http://127.0.0.1:9\"\n",
        Some("k"),
    );
    assert_eq!(
        ai_error(complete(&unreachable, request("hi", None)).1).0,
        "unreachable"
    );
}

#[test]
fn anthropic_lists_models_over_pages() {
    let server = Server::start(|request: &Request| {
        if request.path.contains("after_id=m2") {
            Response::json(
                200,
                json!({ "data": [{ "id": "m3", "display_name": "Three" }], "has_more": false, "last_id": "m3" }),
            )
        } else {
            Response::json(
                200,
                json!({ "data": [{ "id": "m1", "display_name": "One" }, { "id": "m2" }], "has_more": true, "last_id": "m2" }),
            )
        }
    });
    let plugin = provider("anthropic", &anthropic_on(&server), Some("k"));
    let models: Value = plugin
        .call(PluginCall::Ai {
            call: AiCall::Models,
        })
        .unwrap();
    let ids: Vec<&str> = models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["m1", "m2", "m3"]);
    assert_eq!(models[0]["name"], "One");
    let account: Value = plugin
        .call(PluginCall::Ai {
            call: AiCall::Account,
        })
        .unwrap();
    assert!(
        account["label"].as_str().unwrap().contains("works"),
        "{account}"
    );
}

fn openai_chunk(content: &str, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({ "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": finish }] })
    )
}

#[test]
fn openai_compatible_streams_and_needs_a_model() {
    let server = Server::start(|request: &Request| {
        if request.path == "/v1/models" {
            return Response::json(
                200,
                json!({ "data": [{ "id": "zeta" }, { "id": "alpha" }] }),
            );
        }
        let body: Value = serde_json::from_str(&request.body).unwrap();
        let said = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        let text = match said.rsplit("\n\n").next().unwrap() {
            "401" => return Response::json(401, json!({ "error": { "message": "bad key" } })),
            "429" => return Response::json(429, json!({ "error": { "message": "slow down" } })),
            "mid" => format!(
                "{}data: {}\n\n",
                openai_chunk("Half", None),
                json!({ "error": { "message": "upstream died" } })
            ),
            "long" => openai_chunk("and so", Some("length")),
            _ => format!(
                "{}{}data: [DONE]\n\n",
                openai_chunk("Fix the ", None),
                openai_chunk("parser crash", Some("stop"))
            ),
        };
        Response {
            status: 200,
            headers: vec![("content-type", "text/event-stream".into())],
            body: text,
        }
    });
    let base = format!("base_url = \"http://{}/v1\"\n", server.host);

    let unpicked = provider("openai", &base, None);
    let (kind, why) = ai_error(complete(&unpicked, request("hi", None)).1);
    assert_eq!(kind, "failed");
    assert!(why.contains("No model"), "{why}");
    let models: Value = unpicked
        .call(PluginCall::Ai {
            call: AiCall::Models,
        })
        .unwrap();
    assert_eq!(
        models,
        json!([{ "id": "alpha", "name": null }, { "id": "zeta", "name": null }])
    );
    // A local server needs no key, and gets none.
    assert!(
        server
            .seen()
            .iter()
            .all(|r| r.header("authorization").is_none())
    );

    let plugin = provider(
        "openai",
        &format!("{base}model = \"alpha\"\n"),
        Some("sk-o"),
    );
    let (text, result) = complete(&plugin, request("Describe it.", Some("diff")));
    result.unwrap();
    assert_eq!(text, "Fix the parser crash");
    let asked = server.seen().into_iter().last().unwrap();
    assert_eq!(asked.path, "/v1/chat/completions");
    assert_eq!(asked.header("authorization"), Some("Bearer sk-o"));
    let body: Value = serde_json::from_str(&asked.body).unwrap();
    assert_eq!(
        (body["model"].as_str(), body["stream"].as_bool()),
        (Some("alpha"), Some(true))
    );
    assert_eq!(body["messages"][0]["content"], "diff\n\nDescribe it.");

    let kind = |prompt: &str| ai_error(complete(&plugin, request(prompt, None)).1);
    assert_eq!(kind("401").0, "auth");
    assert_eq!(kind("429").0, "rate-limited");
    let (mid, why) = kind("mid");
    assert_eq!(mid, "failed");
    assert!(why.contains("upstream died"), "{why}");
    assert_eq!(kind("long").0, "failed");
}

/// A `stream_event` line with a text delta, as `claude -p --output-format
/// stream-json --include-partial-messages` prints one.
#[cfg(unix)]
fn delta_line(text: &str) -> String {
    json!({
        "type": "stream_event",
        "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }
    })
    .to_string()
}

/// A script body printing `lines`, a line each.
#[cfg(unix)]
fn printing(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("printf '%s\\n' '{}'\n", line.replace('\'', r"'\''")))
        .collect()
}

/// A stand-in program: keeps its arguments, stdin and directory beside
/// itself, then runs `body`.
#[cfg(unix)]
fn fake_program(dir: &Path, name: &str, body: &str) -> PathBuf {
    let script = dir.join(name);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\ncat > \"$0.stdin\"\npwd > \"$0.cwd\"\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
#[cfg(unix)]
fn claude_code_and_codex_run_their_programs() {
    let dir = scratch("cli");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let claude_says = [
        delta_line("Fix the "),
        delta_line("parser crash\n"),
        r#"{"type":"assistant","message":{"content":[]}}"#.to_owned(),
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Fix the parser crash\n"}"#
            .to_owned(),
    ];
    let codex_says = [
        r#"{"type":"thread.started","thread_id":"t"}"#.to_owned(),
        r#"{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"thinking"}}"#
            .to_owned(),
        r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Fix the parser crash\n"}}"#
            .to_owned(),
        r#"{"type":"turn.completed","usage":{}}"#.to_owned(),
    ];
    for (id, name, args, says) in [
        (
            "claude-code",
            "claude",
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--model",
                "opus",
                "Describe it.",
            ],
            &claude_says,
        ),
        (
            "codex",
            "codex",
            vec![
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--model",
                "opus",
                "Describe it.",
            ],
            &codex_says,
        ),
    ] {
        let program = fake_program(&dir, name, &printing(says));
        let plugin = provider(
            id,
            &format!(
                "program = {:?}\nmodel = \"opus\"\n",
                program.display().to_string()
            ),
            None,
        );
        let mut asked = request("Describe it.", Some("diff --git a b"));
        asked.cwd = Some(repo.display().to_string());
        let (text, result) = complete(&plugin, asked);
        result.unwrap();
        assert_eq!(text, "Fix the parser crash\n");
        let read = |ext: &str| std::fs::read_to_string(program.with_extension(ext)).unwrap();
        assert_eq!(read("args").lines().collect::<Vec<_>>(), args, "{id}");
        assert_eq!(read("stdin"), "diff --git a b");
        assert_eq!(read("cwd").trim(), repo.display().to_string());

        let account: Value = plugin
            .call(PluginCall::Ai {
                call: AiCall::Account,
            })
            .unwrap();
        assert!(account["label"].as_str().is_some(), "{account}");
    }

    let signed_out = fake_program(
        &dir,
        "claude-out",
        &printing(&[
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#
                .to_owned(),
        ]),
    );
    let plugin = provider(
        "claude-code",
        &format!("program = {:?}\n", signed_out.display().to_string()),
        None,
    );
    let (kind, why) = ai_error(complete(&plugin, request("hi", None)).1);
    assert_eq!(kind, "auth");
    assert!(why.contains("/login"), "{why}");

    let missing = provider(
        "claude-code",
        "program = \"/nonexistent/diffui-test/claude\"\n",
        None,
    );
    assert_eq!(
        ai_error(complete(&missing, request("hi", None)).1).0,
        "missing-cli"
    );
}

/// A server that sends one SSE event every `gap`, forever, and says when
/// the connection went away.
#[cfg(unix)]
fn trickling_server(gap: Duration) -> (String, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let (closed, heard) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let closed = closed.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if read_request(&mut reader).is_none() {
                    return;
                }
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                loop {
                    let event = format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "word " } })
                    );
                    if stream
                        .write_all(event.as_bytes())
                        .and_then(|()| stream.flush())
                        .is_err()
                    {
                        let _ = closed.send(());
                        return;
                    }
                    std::thread::sleep(gap);
                }
            });
        }
    });
    (host, heard)
}

#[test]
#[cfg(unix)]
fn a_cancel_mid_stream_aborts_the_request_and_kills_the_program() {
    let (host, closed) = trickling_server(Duration::from_millis(50));
    let plugin = provider(
        "anthropic",
        &format!("base_url = \"http://{host}\"\n"),
        Some("k"),
    );
    let (sender, parts) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let id = plugin.plugin.call_streaming::<Value>(
        PluginCall::Ai {
            call: AiCall::Complete {
                request: request("hi", None),
            },
        },
        move |sent, reply| {
            let _ = sender.send(sent);
            reply.ok(Value::Null);
        },
        move |result| {
            let _ = done.send(result);
        },
    );
    parts.recv_timeout(PATIENCE).expect("text came");
    plugin.plugin.abort(id);
    assert!(matches!(
        finished.recv_timeout(PATIENCE).unwrap(),
        Err(PluginError::Gone)
    ));
    closed
        .recv_timeout(PATIENCE)
        .expect("the request was dropped");

    // A program is killed: it would write its mark after a second.
    let dir = scratch("cancel");
    let program = fake_program(
        &dir,
        "claude",
        &format!(
            "{}sleep 1\ntouch \"$0.finished\"",
            printing(&[delta_line("Fix\n")])
        ),
    );
    let plugin = provider(
        "claude-code",
        &format!("program = {:?}\n", program.display().to_string()),
        None,
    );
    let (sender, parts) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let id = plugin.plugin.call_streaming::<Value>(
        PluginCall::Ai {
            call: AiCall::Complete {
                request: request("hi", None),
            },
        },
        move |sent, reply| {
            let _ = sender.send(sent);
            reply.ok(Value::Null);
        },
        move |result| {
            let _ = done.send(result);
        },
    );
    parts.recv_timeout(PATIENCE).expect("text came");
    let stopped = Instant::now();
    plugin.plugin.abort(id);
    assert!(matches!(
        finished.recv_timeout(PATIENCE).unwrap(),
        Err(PluginError::Gone)
    ));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !program.with_extension("finished").exists(),
        "the program ran on after {:?}",
        stopped.elapsed()
    );
}

#[test]
#[cfg(unix)]
fn an_old_claude_without_streaming_answers_in_plain_text() {
    let dir = scratch("old-claude");
    let program = fake_program(
        &dir,
        "claude",
        "case \"$*\" in\n  *--output-format*) echo \"error: unknown option '--output-format'\" >&2 ; exit 1 ;;\n  --help) echo 'Usage: claude [options]' ;;\n  --version) echo '1.0.0 (Claude Code)' ;;\n  *) printf 'Fix the parser crash\\n' ;;\nesac",
    );
    let plugin = provider(
        "claude-code",
        &format!("program = {:?}\n", program.display().to_string()),
        None,
    );
    let (text, result) = complete(&plugin, request("Describe it.", None));
    result.unwrap();
    assert_eq!(text, "Fix the parser crash\n");
    let account: Value = plugin
        .call(PluginCall::Ai {
            call: AiCall::Account,
        })
        .unwrap();
    assert_eq!(
        account["label"],
        "claude 1.0.0 (Claude Code): streaming needs a newer claude"
    );
}
