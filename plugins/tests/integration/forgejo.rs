//! The reference plugin in `examples/forgejo`, end to end: its real files,
//! a fake host, and a local server speaking enough of Forgejo's API.

use diffui_plugins::protocol::{
    CheckState, Checks, CreatePullRequest, ForgeCall, ForgeRemote, HostCall, PluginCall, PrState,
    PullRequest, RepoHandle, UiCall, UpdatePullRequest,
};
use diffui_plugins::{PluginError, Registration};
use serde_json::{Value, json};

use crate::support::{CURRENT_OP, Request, Response, Server, Setup, TestPlugin};

/// A pull request as Forgejo's API returns one, trimmed to what matters.
fn api_pull(number: u64, head: &str, base: &str, state: &str, extra: Value) -> Value {
    let mut pull = json!({
        "number": number,
        "html_url": format!("https://forge.test/team/app/pulls/{number}"),
        "title": format!("Change {number}"),
        "state": state,
        "merged": false,
        "draft": false,
        "head": { "ref": head, "sha": format!("{number:040x}"), "repo_id": 7 },
        "base": { "ref": base, "sha": "b".repeat(40), "repo_id": 7 },
    });
    pull.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    pull
}

fn forgejo(request: &Request) -> Response {
    if request.header("authorization") != Some("token s3cret") {
        return Response::json(401, json!({ "message": "token is required" }));
    }
    let project = "/api/v1/repos/team/app";
    let Some(path) = request.path.strip_prefix(project) else {
        return Response::json(404, json!({ "message": "The target couldn't be found." }));
    };
    match (request.method.as_str(), path) {
        ("GET", "/pulls?state=all&sort=recentupdate&limit=50") => Response::json(
            200,
            json!([
                api_pull(41, "feature", "main", "open", json!({})),
                api_pull(
                    40,
                    "stack-2",
                    "feature",
                    "open",
                    json!({ "draft": true, "title": "WIP: Change 40" })
                ),
                api_pull(39, "old", "main", "closed", json!({ "merged": true })),
                api_pull(38, "dropped", "main", "closed", json!({})),
                api_pull(37, "unrelated", "main", "open", json!({})),
                // A fork's branch with the same name is not ours.
                api_pull(
                    36,
                    "feature",
                    "main",
                    "open",
                    json!({ "head": { "ref": "feature", "sha": "f".repeat(40), "repo_id": 99 } })
                ),
            ]),
        ),
        ("GET", "/commits/aaaa/status") => Response::json(
            200,
            json!({ "state": "pending", "total_count": 2, "statuses": [{ "target_url": "https://ci.test/run/9" }, { "target_url": "https://ci.test/run/8" }] }),
        ),
        ("GET", "/commits/bbbb/status") => Response::json(
            200,
            json!({ "state": "error", "total_count": 1, "statuses": [{}] }),
        ),
        ("GET", "/commits/cccc/status") => Response::json(
            200,
            json!({ "state": "", "total_count": 0, "statuses": null }),
        ),
        ("POST", "/pulls") => {
            let asked: Value = serde_json::from_str(&request.body).unwrap();
            if asked["head"] == "taken" {
                return Response::json(
                    409,
                    json!({ "message": "pull request already exists for these targets" }),
                );
            }
            let title = asked["title"].as_str().unwrap();
            Response::json(
                201,
                api_pull(
                    42,
                    asked["head"].as_str().unwrap(),
                    asked["base"].as_str().unwrap(),
                    "open",
                    json!({ "title": title, "draft": title.starts_with("WIP:") }),
                ),
            )
        }
        ("PATCH", "/pulls/41") => {
            let patch: Value = serde_json::from_str(&request.body).unwrap();
            Response::json(
                201,
                api_pull(
                    41,
                    "feature",
                    patch["base"].as_str().unwrap_or("main"),
                    "open",
                    json!({}),
                ),
            )
        }
        _ => Response::text(500, "<html>upstream broke</html>"),
    }
}

/// The example's own files, pointed at `server` instead of codeberg.org,
/// with a `token_cmd` that prints `token`.
fn example(server: &Server, token: &str) -> TestPlugin {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/forgejo");
    let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
    let manifest = read("plugin.toml").replace("codeberg.org", &server.host);
    Setup::with_manifest(manifest, &read("main.js"))
        .config(&format!("token_cmd = [\"echo\", \"{token}\"]\n"))
        .spawn()
}

fn repo(server: &Server) -> RepoHandle {
    RepoHandle {
        id: "tab-1".into(),
        op: CURRENT_OP.into(),
        root: "/work/app".into(),
        forge_remote: Some(ForgeRemote {
            host: server.host.clone(),
            owner: "team".into(),
            name: "app".into(),
            web_url: format!("http://{}/team/app", server.host),
        }),
    }
}

fn ask(server: &Server, call: ForgeCall) -> PluginCall {
    PluginCall::Forge {
        repo: repo(server),
        call,
    }
}

#[test]
fn the_reference_plugin_lists_checks_creates_and_updates() {
    let server = Server::start(forgejo);
    let plugin = example(&server, "s3cret");
    assert_eq!(
        plugin.loaded(),
        Registration {
            commands: vec!["forgejo.open-in-browser".into()],
            forge: Some(vec![
                "pullRequests".into(),
                "checks".into(),
                "createPullRequest".into(),
                "updatePullRequest".into()
            ]),
            events: vec![],
        }
    );

    // pullRequests: ours only, whatever their state, mapped to diffui's shape.
    let branches = ["feature", "stack-2", "old", "dropped", "no-pr"]
        .map(String::from)
        .to_vec();
    let pulls: Vec<PullRequest> = plugin
        .call(ask(&server, ForgeCall::PullRequests { branches }))
        .unwrap();
    let brief: Vec<_> = pulls
        .iter()
        .map(|pr| (pr.number, pr.head.as_str(), pr.base.as_str(), pr.state))
        .collect();
    assert_eq!(
        brief,
        [
            (41, "feature", "main", PrState::Open),
            (40, "stack-2", "feature", PrState::Draft),
            (39, "old", "main", PrState::Merged),
            (38, "dropped", "main", PrState::Closed),
        ]
    );
    assert_eq!(pulls[0].url, "https://forge.test/team/app/pulls/41");
    assert_eq!(pulls[0].head_sha, format!("{:040x}", 41));
    let listed = &server.seen()[0];
    assert_eq!(listed.header("authorization"), Some("token s3cret"));
    assert_eq!(listed.header("accept"), Some("application/json"));

    // checks: Forgejo's states folded into four, with the first status's link.
    let checks = |sha: &str| {
        plugin
            .call::<Checks>(ask(&server, ForgeCall::Checks { sha: sha.into() }))
            .unwrap()
    };
    assert_eq!(
        checks("aaaa"),
        Checks {
            state: CheckState::Pending,
            url: Some("https://ci.test/run/9".into())
        }
    );
    assert_eq!(checks("bbbb").state, CheckState::Failure);
    assert_eq!(
        checks("cccc"),
        Checks {
            state: CheckState::None,
            url: None
        }
    );

    // createPullRequest, plain and draft.
    let create = |head: &str, draft| {
        plugin.call::<PullRequest>(ask(
            &server,
            ForgeCall::CreatePullRequest {
                request: CreatePullRequest {
                    head: head.into(),
                    base: "main".into(),
                    title: "Add the thing".into(),
                    body: "Because.\n".into(),
                    draft,
                },
            },
        ))
    };
    let created = create("thing", false).unwrap();
    assert_eq!(
        (created.number, created.head.as_str(), created.state),
        (42, "thing", PrState::Open)
    );
    let posted = server
        .seen()
        .into_iter()
        .rfind(|request| request.method == "POST")
        .unwrap();
    assert_eq!(posted.header("content-type"), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<Value>(&posted.body).unwrap(),
        json!({ "head": "thing", "base": "main", "title": "Add the thing", "body": "Because.\n" })
    );
    let draft = create("thing", true).unwrap();
    assert_eq!(
        (draft.title.as_str(), draft.state),
        ("WIP: Add the thing", PrState::Draft)
    );

    // updatePullRequest sends only what changed.
    let patch = UpdatePullRequest {
        base: Some("develop".into()),
        ..Default::default()
    };
    let updated: PullRequest = plugin
        .call(ask(
            &server,
            ForgeCall::UpdatePullRequest { number: 41, patch },
        ))
        .unwrap();
    assert_eq!(updated.base, "develop");
    let patched = server
        .seen()
        .into_iter()
        .rfind(|request| request.method == "PATCH")
        .unwrap();
    assert_eq!(patched.path, "/api/v1/repos/team/app/pulls/41");
    assert_eq!(patched.body, "{\"base\":\"develop\"}");

    // A refusal from the forge is the error the user reads.
    let conflict = create("taken", false).unwrap_err();
    let message = conflict.to_string();
    assert!(
        message.starts_with("main.js:")
            && message
                .ends_with(": Error: Forgejo 409: pull request already exists for these targets"),
        "{message}"
    );
    let broken = plugin
        .call::<Checks>(ask(&server, ForgeCall::Checks { sha: "dddd".into() }))
        .unwrap_err();
    assert!(
        broken
            .to_string()
            .ends_with(": Error: Forgejo 500: <html>upstream broke</html>"),
        "{broken}"
    );
    let PluginError::Threw(error) = broken else {
        panic!("not an exception: {broken:?}");
    };
    assert!(error.stack.contains("at api (main.js:"), "{}", error.stack);
}

#[test]
fn the_reference_plugins_command_opens_the_bookmarks_pull_request() {
    let server = Server::start(forgejo);
    let plugin = example(&server, "s3cret");
    plugin.loaded();
    let open = |bookmark: &str| {
        plugin.call::<Value>(PluginCall::Command {
            id: "forgejo.open-in-browser".into(),
            context: diffui_plugins::protocol::CommandContext {
                repo: repo(&server),
                change_id: None,
                commit_id: None,
                bookmark: Some(bookmark.into()),
                path: None,
                line: None,
            },
        })
    };
    open("feature").unwrap();
    open("no-pr").unwrap();
    let calls = plugin.host.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [
            HostCall::Ui(UiCall::OpenUrl {
                url: "https://forge.test/team/app/pulls/41".into()
            }),
            HostCall::Ui(UiCall::Toast {
                message: "No pull request for no-pr".into(),
                tone: Default::default()
            }),
        ]
    );
}

#[test]
fn a_rejected_token_is_the_forges_answer_not_a_crash() {
    let server = Server::start(forgejo);
    let plugin = example(&server, "expired");
    plugin.loaded();
    let error = plugin
        .call::<Vec<PullRequest>>(ask(&server, ForgeCall::PullRequests { branches: vec![] }))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with(": Error: Forgejo 401: token is required"),
        "{error}"
    );
}
