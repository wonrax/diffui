//! The AI commands in the real app on the fixture repository, through each
//! kind of provider: the bundled Claude Code provider on a stand-in `claude`,
//! the bundled Anthropic provider on a local server speaking its streamed
//! Messages API, and config.toml's `[ai] command`. Nothing here reaches a
//! real API or program.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diffui_core::RevisionSelection;
use diffui_plugins::secrets::{self, FileStore, SecretStore as _};

use crate::Action;
use crate::ai::{AiAction, Test};
use crate::commands::CommandArg;
use crate::harness::{Harness, fixture_repo};
use crate::modes::{Mode, ModeKind};

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

/// What the stand-in `claude` answers, by a word of its prompt.
const REPLIES: &[(&str, &str)] = &[
    (
        "pull request",
        "Teach the parser empty files\n\nThe parser no longer panics.\n\n- Adds a test\n",
    ),
    ("bookmark", "`Fix Parser crash`\n"),
    (
        "Review",
        "src/parser.rs:3: the empty case\nreturns early.\n",
    ),
    ("Explain", "It fixes the parser.\nNothing else changes.\n"),
    ("single word OK", "OK\n"),
    (
        "",
        "Fix the parser crash\n\nIt no longer panics on an empty file.\n\n",
    ),
];

/// A stand-in `claude`: keeps its arguments and stdin beside itself, then
/// answers by what the prompt asks for: as `claude -p --output-format
/// stream-json --include-partial-messages` does, a text delta event, a
/// pause, the rest and the result; else (as `[ai] command`) plain text. It
/// lives in the part of the scratch home a launch keeps.
pub(crate) fn fake_claude() -> PathBuf {
    let dir = crate::harness::scratch_home().join("repos");
    std::fs::create_dir_all(&dir).expect("a bin dir");
    let script = dir.join("fake-claude");
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    let event = |text: &str| {
        serde_json::json!({
            "type": "stream_event",
            "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }
        })
        .to_string()
    };
    let mut body = String::from(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\ncat > \"$0.stdin\"\ncase \"$*\" in\n",
    );
    for (word, reply) in REPLIES {
        let (first, rest) = reply.split_at(reply.len() / 2);
        let result = serde_json::json!({ "type": "result", "subtype": "success", "is_error": false, "result": reply }).to_string();
        body.push_str(&format!(
            "  *{word:?}*)\n    case \"$*\" in\n      *stream-json*) printf '%s\\n' {} ; sleep 1 ; printf '%s\\n' {} {} ;;\n      *) printf '%s' {} ;;\n    esac ;;\n",
            quote(&event(first)),
            quote(&event(rest)),
            quote(&result),
            quote(reply),
        ));
    }
    body.push_str("esac\n");
    std::fs::write(&script, body).expect("write the script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
    script
}

/// The app on the fixture with the Claude Code provider pointed at
/// [`fake_claude`] and picked in Settings.
fn with_claude() -> (Harness, PathBuf, PathBuf) {
    let repo = fixture_repo();
    let claude = fake_claude();
    let mut h = Harness::open_fresh(
        Some(&repo),
        (1280.0, 800.0),
        &format!(
            "[plugins.claude-code]\nprogram = {:?}\n",
            claude.display().to_string()
        ),
        "[ai]\nprovider = \"claude-code\"\n",
    );
    h.wait_for_graph();
    wait_ready(&mut h, "claude-code");
    (h, repo, claude)
}

fn wait_ready(h: &mut Harness, id: &str) {
    h.wait_until("the provider to load", |ui| ui.ai_answers(id, "complete"));
}

fn select(h: &mut Harness, repo: &Path, name: &str) -> RevisionSelection {
    let revision = RevisionSelection::Commit(commit_of(repo, name));
    h.perform(Action::SelectRevision(revision.clone()));
    h.wait_until("the revision", |ui| {
        ui.active().session.selected_revision == revision
    });
    revision
}

fn wait_description(h: &mut Harness) -> String {
    h.wait_until("the description", |ui| {
        ui.active().description_editor().is_some_and(|editor| {
            editor
                .generating
                .as_ref()
                .is_some_and(|generation| !generation.running)
        })
    });
    h.read(|ui| ui.active().description_editor().unwrap().content.clone())
}

#[test]
fn describe_through_the_claude_code_provider() {
    let (mut h, repo, claude) = with_claude();
    let revision = select(&mut h, &repo, "fix/parser-crash");
    let hint = h.read(|ui| ui.agent_destination("ai.describe"));
    assert_eq!(hint.as_deref(), Some("Sends the diff to Claude Code"));
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision), cx));
    // The first piece shows while claude is still writing.
    h.wait_until("the first piece", |ui| {
        ui.active().description_editor().is_some_and(|editor| {
            editor.content.starts_with("Fix the parser")
                && editor
                    .generating
                    .as_ref()
                    .is_some_and(|generation| generation.running)
        })
    });
    assert_eq!(
        wait_description(&mut h),
        "Fix the parser crash\n\nIt no longer panics on an empty file."
    );
    let args = std::fs::read_to_string(claude.with_extension("args")).unwrap();
    assert!(
        args.starts_with(
            "-p\n--output-format\nstream-json\n--verbose\n--include-partial-messages\n"
        ),
        "{args}"
    );
    assert!(args.contains("---"), "recent descriptions: {args}");
    let stdin = std::fs::read_to_string(claude.with_extension("stdin")).unwrap();
    assert!(stdin.starts_with("diff --git"), "{stdin}");
}

#[test]
fn a_custom_command_still_wins_under_automatic() {
    let repo = fixture_repo();
    let claude = fake_claude();
    let mut h = Harness::open_fresh(
        Some(&repo),
        (1280.0, 800.0),
        &format!(
            "[ai]\ncommand = [{:?}, \"-p\"]\n",
            claude.display().to_string()
        ),
        "",
    );
    h.wait_for_graph();
    let revision = select(&mut h, &repo, "fix/parser-crash");
    assert_eq!(
        h.read(|ui| ui.ai_provider().map(|p| p.name().to_owned())),
        Ok("Custom command".to_owned())
    );
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision), cx));
    assert!(wait_description(&mut h).starts_with("Fix the parser crash"));
    assert!(claude.with_extension("stdin").exists());
}

#[test]
fn the_pull_request_dialog_and_the_bookmark_prompt_fill_in() {
    let (mut h, repo, _) = with_claude();
    let revision = select(&mut h, &repo, "fix/parser-crash");
    h.act(|ui, _| {
        let github = crate::plugins::bundled_kind("github");
        let remote = diffui_core::forge::ForgeRemote::new(github, "github.com", "acme", "app");
        ui.active_mut().forge.home = Some(("origin".to_owned(), remote));
    });

    h.act(|ui, cx| {
        ui.run_command(
            "ai.pr-description",
            CommandArg::Revision(revision.clone()),
            cx,
        )
    });
    h.wait_until("the title and body", |ui| {
        ui.pull_request_dialog().is_some_and(|dialog| {
            dialog
                .generating
                .as_ref()
                .is_some_and(|generation| !generation.running)
        })
    });
    let (title, body) = h.read(|ui| {
        let dialog = ui.pull_request_dialog().unwrap();
        (dialog.title.clone(), dialog.body.clone())
    });
    assert_eq!(title, "Teach the parser empty files");
    assert_eq!(body, "The parser no longer panics.\n\n- Adds a test");
    // Generate is the size of the buttons beside it, and none overlaps.
    let [generate, cancel, accept] = [
        "pull-request-generate",
        "pull-request-cancel",
        "pull-request-accept",
    ]
    .map(|id| h.bounds(id));
    assert_eq!(generate.size.height, cancel.size.height);
    assert_eq!(accept.size.height, cancel.size.height);
    assert!(generate.right() <= cancel.left() && cancel.right() <= accept.left());
    h.perform(Action::Forge(crate::forge_dialog::ForgeAction::Cancel));

    h.act(|ui, cx| ui.run_command("ai.bookmark-name", CommandArg::Revision(revision), cx));
    h.wait_until("the name", |ui| {
        ui.name_prompt()
            .is_some_and(|prompt| prompt.generating.is_none() && !prompt.value.is_empty())
    });
    assert_eq!(
        h.read(|ui| ui.name_prompt().unwrap().value.clone()),
        "fix-parser-crash"
    );
    let [suggest, accept] = ["name-prompt-suggest", "name-prompt-accept"].map(|id| h.bounds(id));
    assert_eq!(suggest.size.height, accept.size.height);
}

#[test]
fn explain_and_review_stream_into_the_output_panel() {
    let (mut h, repo, claude) = with_claude();
    let revision = select(&mut h, &repo, "fix/parser-crash");
    for (id, first) in [
        ("ai.explain", "It fixes the parser."),
        ("ai.review", "src/parser.rs:3: the empty case"),
    ] {
        h.act(|ui, cx| ui.run_command(id, CommandArg::Revision(revision.clone()), cx));
        h.wait_until("the panel's text", |ui| {
            let Some(Mode::CliOutput(view)) = ui.mode(ModeKind::CliOutput) else {
                return false;
            };
            ui.tab(view.tab)
                .and_then(|tab| {
                    tab.cmdline
                        .runs
                        .iter()
                        .find(|run| run.activity == view.activity)
                })
                .is_some_and(|run| run.exit.is_some() && run.lines.len() == 2)
        });
        let (heading, lines) = h.read(|ui| {
            let Some(Mode::CliOutput(view)) = ui.mode(ModeKind::CliOutput) else {
                unreachable!()
            };
            let run = ui
                .tab(view.tab)
                .unwrap()
                .cmdline
                .runs
                .iter()
                .find(|run| run.activity == view.activity)
                .unwrap();
            (
                run.heading.clone(),
                run.lines
                    .iter()
                    .map(|line| line.text.clone())
                    .collect::<Vec<_>>(),
            )
        });
        assert!(heading.unwrap().ends_with("\u{b7} Claude Code"));
        assert_eq!(lines[0], first);
        let stdin = std::fs::read_to_string(claude.with_extension("stdin")).unwrap();
        assert!(stdin.contains("diff --git"), "{stdin}");
        h.act(|ui, _| {
            ui.pop_mode(ModeKind::CliOutput);
        });
    }
}

/// A local server that answers each `POST /v1/messages` with the SSE
/// events `answer` makes of its body, one every `gap`, and notes when a
/// client hung up before the end.
struct Sse {
    host: String,
    bodies: Arc<Mutex<Vec<String>>>,
    hung_up: Arc<Mutex<bool>>,
}

fn sse_server(
    gap: Duration,
    answer: impl Fn(&str) -> (u16, Vec<String>) + Send + Sync + 'static,
) -> Sse {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let host = listener.local_addr().unwrap().to_string();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let hung_up = Arc::new(Mutex::new(false));
    let answer = Arc::new(answer);
    std::thread::spawn({
        let (bodies, hung_up) = (bodies.clone(), hung_up.clone());
        move || {
            for stream in listener.incoming().flatten() {
                let (bodies, hung_up, answer) = (bodies.clone(), hung_up.clone(), answer.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    let _ = reader.read_exact(&mut body);
                    let body = String::from_utf8_lossy(&body).into_owned();
                    bodies.lock().unwrap().push(body.clone());
                    let (status, events) = answer(&body);
                    let mut stream = stream;
                    let head = format!(
                        "HTTP/1.1 {status} Status\r\ncontent-type: {}\r\nconnection: close\r\n\r\n",
                        if status == 200 {
                            "text/event-stream"
                        } else {
                            "application/json"
                        }
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    for event in events {
                        if stream
                            .write_all(event.as_bytes())
                            .and_then(|()| stream.flush())
                            .is_err()
                        {
                            *hung_up.lock().unwrap() = true;
                            return;
                        }
                        std::thread::sleep(gap);
                    }
                });
            }
        }
    });
    Sse {
        host,
        bodies,
        hung_up,
    }
}

fn delta(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {}\n\n",
        serde_json::json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } })
    )
}

fn stop(reason: &str) -> Vec<String> {
    vec![
        format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::json!({ "type": "message_delta", "delta": { "stop_reason": reason } })
        ),
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
    ]
}

/// The app with the Anthropic provider on `server`, its key saved.
fn with_anthropic(server: &Sse, key: bool) -> (Harness, PathBuf) {
    let repo = fixture_repo();
    let config = format!(
        "[plugins.anthropic]\nbase_url = \"http://{}\"\n",
        server.host
    );
    let mut h = Harness::open_prepared(
        Some(&repo),
        (1280.0, 800.0),
        &config,
        "[ai]\nprovider = \"anthropic\"\n",
        |home| {
            if key {
                FileStore::new(home.join(".diffui-secrets.json"))
                    .set(&secrets::account("anthropic", "api_key", None), "sk-test")
                    .expect("keep the key");
            }
        },
    );
    h.wait_for_graph();
    wait_ready(&mut h, "anthropic");
    (h, repo)
}

#[test]
fn describe_through_the_anthropic_api_streams_in() {
    let server = sse_server(Duration::ZERO, |_| {
        let mut events = vec![
            delta("Fix the parser "),
            delta("crash\n\nIt no longer panics."),
        ];
        events.extend(stop("end_turn"));
        (200, events)
    });
    let (mut h, repo) = with_anthropic(&server, true);
    let revision = select(&mut h, &repo, "fix/parser-crash");
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision), cx));
    assert_eq!(
        wait_description(&mut h),
        "Fix the parser crash\n\nIt no longer panics."
    );
    let body: serde_json::Value = serde_json::from_str(&server.bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["stream"], true);
    let content = &body["messages"][0]["content"];
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("diff --git")
    );
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .contains("jj change description")
    );
}

#[test]
fn a_refusal_puts_the_description_back_and_says_why() {
    let server = sse_server(Duration::ZERO, |_| {
        let mut events = vec![delta("I can't")];
        events.extend(stop("refusal"));
        (200, events)
    });
    let (mut h, repo) = with_anthropic(&server, true);
    let revision = select(&mut h, &repo, "fix/parser-crash");
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision), cx));
    let content = wait_description(&mut h);
    assert!(!content.contains("I can't"), "{content}");
    let toast = h.read(|ui| ui.toasts.last().map(|toast| format!("{toast:?}")));
    assert!(toast.unwrap().contains("declined"));
}

#[test]
fn stop_mid_stream_drops_the_request_and_keeps_the_old_text() {
    let server = sse_server(Duration::from_millis(50), |_| {
        (200, (0..2000).map(|_| delta("word ")).collect())
    });
    let (mut h, repo) = with_anthropic(&server, true);
    let revision = select(&mut h, &repo, "fix/parser-crash");
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision), cx));
    h.wait_until("some text", |ui| {
        ui.active()
            .description_editor()
            .is_some_and(|editor| editor.content.starts_with("word"))
    });
    h.perform(Action::StopGenerating);
    let content = wait_description(&mut h);
    assert!(!content.contains("word"), "{content}");
    h.wait_until("the server to see the hang-up", |_| {
        *server.hung_up.lock().unwrap()
    });
}

#[test]
fn settings_test_sends_a_short_prompt_and_shows_the_answer() {
    let server = sse_server(Duration::ZERO, |body| {
        if body.contains("single word OK") {
            let mut events = vec![delta("OK")];
            events.extend(stop("end_turn"));
            (200, events)
        } else {
            (500, vec![])
        }
    });
    let (mut h, _) = with_anthropic(&server, true);
    h.perform(Action::Ai(AiAction::Test("anthropic".to_owned())));
    h.wait_until("the reply", |ui| {
        !matches!(ui.ai.tests.get("anthropic"), None | Some(Test::Running))
    });
    assert_eq!(
        h.read(|ui| ui.ai.tests.get("anthropic").cloned()),
        Some(Test::Replied("OK".to_owned()))
    );

    drop(h);

    // Without a key the test says where to add one.
    let refused = sse_server(Duration::ZERO, |_| (401, vec![]));
    let (mut h, _) = with_anthropic(&refused, false);
    h.perform(Action::Ai(AiAction::Test("anthropic".to_owned())));
    h.wait_until("the failure", |ui| {
        !matches!(ui.ai.tests.get("anthropic"), None | Some(Test::Running))
    });
    let Some(Test::Failed(why)) = h.read(|ui| ui.ai.tests.get("anthropic").cloned()) else {
        panic!("it should have failed");
    };
    assert_eq!(
        why,
        "There is no Anthropic API key. Add an API key in Settings → AI → Anthropic."
    );
    assert!(!why.contains("sk-"));
}

#[test]
fn settings_lists_the_providers_and_saves_the_pick() {
    let (mut h, _, _) = with_claude();
    let providers = h.read(|ui| {
        ui.ai_providers(&Default::default())
            .into_iter()
            .map(|provider| provider.id)
            .collect::<Vec<_>>()
    });
    assert_eq!(providers, ["claude-code", "codex", "anthropic", "openai"]);
    h.perform(Action::Ai(AiAction::SetProvider("openai".to_owned())));
    let settings = std::fs::read_to_string(
        crate::harness::scratch_home().join(".config/diffui/settings.toml"),
    )
    .unwrap();
    assert!(
        settings.contains("[ai]\nprovider = \"openai\""),
        "{settings}"
    );
    h.perform(Action::Ai(AiAction::SetProvider(
        crate::ai::AUTO.to_owned(),
    )));
    assert_eq!(h.read(|ui| ui.ai_choice().0.to_owned()), "auto");
}

#[test]
fn a_providers_page_picks_a_model_from_its_list() {
    let server = sse_server(Duration::ZERO, |body| {
        if body.is_empty() {
            let models = serde_json::json!({
                "data": [
                    { "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5" },
                    { "id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5" }
                ],
                "has_more": false,
                "last_id": "claude-haiku-5-5"
            });
            (200, vec![models.to_string()])
        } else {
            (500, vec![])
        }
    });
    let (mut h, _) = with_anthropic(&server, true);
    h.perform(Action::Plugin(crate::plugins::PluginAction::Page(
        crate::plugins::PageAction::Open {
            id: "anthropic".to_owned(),
            host: None,
        },
    )));
    h.perform(Action::Plugin(crate::plugins::PluginAction::Page(
        crate::plugins::PageAction::PickModels,
    )));
    h.wait_until("the models", |ui| {
        ui.plugins
            .page
            .as_ref()
            .and_then(|page| page.models())
            .is_some()
    });
    assert_eq!(
        h.read(|ui| ui.plugins.page.as_ref().unwrap().models()),
        Some(vec![
            "claude-opus-5-5".to_owned(),
            "claude-haiku-5-5".to_owned()
        ])
    );
    h.perform(Action::Plugin(crate::plugins::PluginAction::Page(
        crate::plugins::PageAction::UseModel("claude-haiku-5-5".to_owned()),
    )));
    let settings = std::fs::read_to_string(
        crate::harness::scratch_home().join(".config/diffui/settings.toml"),
    )
    .unwrap();
    assert!(
        settings.contains("[plugins.anthropic.config]\nmodel = \"claude-haiku-5-5\""),
        "{settings}"
    );
}

#[test]
fn the_revision_menu_groups_ai_rows_under_the_provider_and_pr_rows_under_the_forge() {
    let (mut h, repo, _) = with_claude();
    let revision = select(&mut h, &repo, "fix/parser-crash");
    h.act(|ui, _| {
        let github = crate::plugins::bundled_kind("github");
        let remote = diffui_core::forge::ForgeRemote::new(github, "github.com", "acme", "app");
        ui.active_mut().forge.home = Some(("origin".to_owned(), remote));
    });
    let tree = h.read(|ui| ui.revision_menu_tree(&revision));
    let submenu = |label: &str| {
        tree.iter().find_map(|entry| match entry {
            crate::menu::MenuEntry::Submenu { label: l, items } if l == label => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        crate::menu::MenuEntry::Item { label, icon, .. } => {
                            Some((label.clone(), icon.is_some()))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
    };
    let ai = submenu("Claude Code").expect("the provider's group");
    let labels: Vec<&str> = ai.iter().map(|(label, _)| label.as_str()).collect();
    assert!(labels.contains(&"Explain this change"), "{labels:?}");
    assert!(labels.contains(&"Generate description"), "{labels:?}");
    assert!(ai.iter().all(|(_, sparkles)| *sparkles));
    let forge = submenu("GitHub").expect("the forge's group");
    assert!(
        forge
            .iter()
            .any(|(label, _)| label.starts_with("Create pull request")),
        "{forge:?}"
    );
    assert!(submenu("AI").is_none() && submenu("Forge").is_none());
}

#[test]
fn without_a_provider_the_ai_rows_are_off_and_say_why() {
    let repo = fixture_repo();
    let mut h = Harness::open_fresh(
        Some(&repo),
        (1280.0, 800.0),
        "[plugins.claude-code]\nprogram = \"/nonexistent/claude\"\n\n[plugins.codex]\nprogram = \"/nonexistent/codex\"\n",
        "",
    );
    h.wait_for_graph();
    wait_ready(&mut h, "anthropic");
    let revision = select(&mut h, &repo, "fix/parser-crash");
    let arg = CommandArg::Revision(revision.clone());
    let why = h.read(|ui| {
        ui.registry
            .get("ai.explain")
            .unwrap()
            .enabled(ui, &arg)
            .unwrap_err()
            .into_owned()
    });
    assert!(
        why.starts_with("No AI provider") && why.contains("Settings → AI"),
        "{why}"
    );
    let tree = h.read(|ui| ui.revision_menu_tree(&revision));
    let shown = format!("{tree:?}");
    assert!(!shown.contains("Explain this change"), "{shown}");
    assert!(!shown.contains("Generate description"), "{shown}");
}
