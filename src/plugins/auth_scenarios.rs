//! The bundled GitHub and GitLab plugins signed in with a token: each call
//! a CLI path makes, made against a local server standing in for the API,
//! and how `auto` picks between the CLI and a token. Nothing here reaches a
//! real forge or the real Keychain (the secrets are a file).

use std::sync::Arc;

use diffui_core::forge::{
    CheckState, CreatePr, ForgeError, ForgePr, ForgeProvider, ForgeRemote, PrState, ReviewDecision,
    UpdatePr,
};
use diffui_core::repo::CancelFlag;
use diffui_plugins::protocol::{Account, AccountMethod, ForgeCall, PluginCall, Secret};
use diffui_plugins::secrets::{FileStore, SecretStore};
use serde_json::json;

use super::forge::ScriptForge;
use super::forge::tests::{block_on, bundled_with, scratch_dir};
use super::scenarios::{Answer, Request, Server};
use crate::forge_dialog::tests::FakeCli;

const TOKEN: &str = "tok-123";

/// A secret store in a scratch file holding `tokens`, by account.
fn store(name: &str, tokens: &[(&str, &str)]) -> Arc<dyn SecretStore> {
    let store = FileStore::new(scratch_dir(name).join("secrets.json"));
    for (account, token) in tokens {
        store.set(account, token).unwrap();
    }
    Arc::new(store)
}

/// `[plugins.<id>]` for a forge on `server`: its API there over http, `auth`
/// as given (none: auto), and `program` a CLI that isn't there unless one is
/// given.
fn config(server: &Server, api: &str, auth: Option<&str>, program: Option<&str>) -> toml::Table {
    let host = &server.host;
    let mut text = format!(
        "program = {:?}\n",
        program.unwrap_or("/nonexistent/diffui-test/cli")
    );
    if let Some(auth) = auth {
        text.push_str(&format!("auth = \"{auth}\"\n"));
    }
    text.push_str(&format!(
        "[per_host.\"{host}\"]\napi_url = \"http://{host}{api}\"\n"
    ));
    toml::from_str(&text).unwrap()
}

fn remote(forge: &ScriptForge, server: &Server) -> ForgeRemote {
    forge
        .claims(&format!("http://{}/team/app.git", server.host))
        .expect("claimed")
}

fn pr(forge: &ScriptForge, server: &Server, project: &str, number: u64) -> ForgePr {
    forge
        .claims(&format!("http://{}/team/{project}.git", server.host))
        .expect("claimed")
        .pull_request(number)
}

/// The forge's `account(host)`, or with `token`, whose that token is.
fn account(forge: &ScriptForge, host: &str, token: Option<&str>) -> Result<Account, String> {
    let (tell, told) = std::sync::mpsc::channel();
    forge.plugin().call::<Account>(
        PluginCall::Forge {
            repo: super::forge::no_repository(None),
            call: ForgeCall::Account {
                host: host.to_owned(),
                account: None,
                token: token.map(|token| Secret(token.to_owned())),
            },
        },
        move |result| {
            let _ = tell.send(result.map_err(|error| error.to_string()));
        },
    );
    told.recv_timeout(std::time::Duration::from_secs(30))
        .expect("an answer")
}

fn authorized(request: &Request, how: &str) -> bool {
    request.header("authorization") == Some(&format!("Bearer {TOKEN}"))
        || request.header("private-token") == Some(TOKEN) && how == "gitlab"
}

const DIFF: &str =
    "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n";

fn rest_pull(number: u64, state: &str, draft: bool) -> serde_json::Value {
    json!({
        "number": number, "html_url": format!("https://example.test/team/app/pull/{number}"),
        "title": "Add the thing", "state": state, "draft": draft, "merged_at": null,
        "user": { "login": "ada" }, "additions": 3, "deletions": 1, "changed_files": 2,
        "head": { "ref": "thing", "sha": "abc" }, "base": { "ref": "main", "sha": "def" },
        "body": "Adds **the** thing.\n\n- one\n- two",
    })
}

fn listed(number: u64, head: &str, state: &str, fork: bool) -> serde_json::Value {
    json!({
        "number": number, "url": format!("https://example.test/team/app/pull/{number}"),
        "title": format!("PR {number}"), "headRefName": head, "baseRefName": "main",
        "headRefOid": format!("sha{number}"), "state": state, "isDraft": false,
        "reviewDecision": "APPROVED", "isCrossRepository": fork,
    })
}

/// GitHub's REST and GraphQL APIs as GitHub Enterprise serves them under
/// `/api/v3` and `/api/graphql`.
fn github_server() -> Server {
    Server::start_with(|request| {
        if !authorized(request, "github") {
            return Answer::json(401, json!({ "message": "Bad credentials" }));
        }
        let path = request.path.as_str();
        let wants_diff = request.header("accept") == Some("application/vnd.github.diff");
        match (request.method.as_str(), path) {
            ("POST", "/api/graphql") if request.body.contains("pullRequests") => {
                let second = request.body.contains("\"after\":\"c1\"");
                let (nodes, more) = if second {
                    (vec![listed(2, "feature-b", "MERGED", false)], false)
                } else {
                    (
                        vec![
                            listed(3, "feature-a", "OPEN", false),
                            listed(4, "feature-a", "OPEN", true),
                        ],
                        true,
                    )
                };
                Answer::json(
                    200,
                    json!({ "data": { "repository": { "pullRequests": {
                        "nodes": nodes, "pageInfo": { "hasNextPage": more, "endCursor": "c1" } } } } }),
                )
            }
            ("POST", "/api/graphql") if request.body.contains("commits(first") => {
                let (commits, more) = if request.body.contains("\"after\":\"c1\"") {
                    (vec![("bbb2", "", "Second", None, 0, 2)], false)
                } else {
                    (
                        vec![(
                            "aaa1",
                            "ada",
                            "First\n\nbody",
                            Some("2026-11-14T09:30:00Z"),
                            3,
                            1,
                        )],
                        true,
                    )
                };
                let mut page: serde_json::Value =
                    serde_json::from_str(&super::github_scenarios::gh_commits(&commits)).unwrap();
                page["data"]["repository"]["pullRequest"]["commits"]["pageInfo"] =
                    json!({ "hasNextPage": more, "endCursor": "c1" });
                Answer::json(200, page)
            }
            ("POST", "/api/graphql") if request.body.contains("reviewThreads") => {
                let page = if request.body.contains("\"after\":\"t1\"") {
                    2
                } else {
                    1
                };
                Answer::json(200, super::github_scenarios::review_threads(page))
            }
            ("POST", "/api/graphql") if request.body.contains("statusCheckRollup") => {
                let state = if request.body.contains("\"sha\":\"good\"") {
                    json!({ "state": "SUCCESS" })
                } else {
                    json!(null)
                };
                Answer::json(
                    200,
                    json!({ "data": { "repository": { "object": { "statusCheckRollup": state } } } }),
                )
            }
            ("POST", "/api/v3/repos/team/app/pulls") => {
                Answer::json(201, rest_pull(7, "open", true))
            }
            ("PATCH", "/api/v3/repos/team/app/pulls/7") => {
                Answer::json(200, rest_pull(7, "open", false))
            }
            ("GET", "/api/v3/repos/team/app/pulls/5") if wants_diff => Answer {
                status: 200,
                headers: vec![("content-type".into(), "text/plain".into())],
                body: DIFF.to_owned(),
            },
            ("GET", "/api/v3/repos/team/app/pulls/5") => {
                Answer::json(200, rest_pull(5, "open", false))
            }
            ("GET", "/api/v3/repos/team/app/pulls/6") if wants_diff => Answer::json(
                406,
                json!({ "message": "Sorry, the diff exceeded the maximum number of files (300)." }),
            ),
            ("GET", "/api/v3/repos/team/app/pulls/6/files?per_page=100&page=1") => Answer::json(
                200,
                json!([{ "filename": "big.rs", "status": "added", "additions": 1, "deletions": 0,
                         "patch": "@@ -0,0 +1 @@\n+x" }]),
            ),
            ("GET", "/api/v3/user") => Answer::json(200, json!({ "login": "octo" })),
            ("GET", "/api/v3/repos/team/busy/pulls/1") => {
                Answer::json(403, json!({ "message": "API rate limit exceeded" }))
                    .header("x-ratelimit-remaining", "0")
            }
            ("GET", "/api/v3/repos/team/sso/pulls/1") => Answer::json(
                403,
                json!({ "message": "Resource protected by organization SAML enforcement" }),
            ),
            _ => Answer::json(404, json!({ "message": "Not Found" })),
        }
    })
}

fn github(server: &Server, store: Arc<dyn SecretStore>, auth: Option<&str>) -> ScriptForge {
    bundled_with(
        "github",
        config(server, "/api/v3", auth, None),
        store,
        &[&server.host],
        &scratch_dir("github-token"),
    )
}

fn github_token(server: &Server) -> ScriptForge {
    let account = format!("github@{}", server.host);
    github(
        server,
        store("gh-token", &[(&account, TOKEN)]),
        Some("token"),
    )
}

#[test]
fn github_badges_and_checks_with_a_token() {
    let server = github_server();
    let github = github_token(&server);
    let prs = block_on(github.pull_requests(
        remote(&github, &server),
        vec!["feature-a".into(), "feature-b".into()],
    ))
    .expect("listed");
    let summary: Vec<(u64, PrState)> = prs.iter().map(|pr| (pr.number, pr.state)).collect();
    assert_eq!(
        summary,
        [(3, PrState::Open), (2, PrState::Merged)],
        "both GraphQL pages, without the fork's"
    );
    assert_eq!(prs[0].review, Some(ReviewDecision::Approved));
    let checks = |sha: &str| {
        block_on(github.checks(remote(&github, &server), sha.to_owned())).expect("checks")
    };
    assert_eq!(checks("good").state, CheckState::Success);
    assert_eq!(checks("other").state, CheckState::None);
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|request| authorized(request, "github")));
}

#[test]
fn github_creates_and_updates_with_a_token() {
    let server = github_server();
    let github = github_token(&server);
    let created = block_on(github.create_pull_request(
        remote(&github, &server),
        CreatePr {
            head: "thing".into(),
            base: "main".into(),
            title: "Add the thing".into(),
            body: "Line one\nline two".into(),
            draft: true,
        },
    ))
    .expect("created");
    assert_eq!((created.number, created.state), (7, PrState::Draft));
    block_on(github.update_pull_request(
        remote(&github, &server),
        7,
        UpdatePr {
            base: Some("feature-a".into()),
            ..UpdatePr::default()
        },
    ))
    .expect("updated");
    let requests = server.requests();
    let sent: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        sent,
        json!({ "head": "thing", "base": "main", "title": "Add the thing", "body": "Line one\nline two", "draft": true })
    );
    let patched: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(
        patched,
        json!({ "base": "feature-a" }),
        "an update sends only what changes"
    );
}

#[test]
fn github_pull_request_tab_with_a_token() {
    let server = github_server();
    let github = github_token(&server);
    let tab = pr(&github, &server, "app", 5);
    let info = block_on(github.view(tab.clone())).expect("viewed");
    assert_eq!(
        (
            info.title.as_str(),
            info.author.as_str(),
            info.state.label()
        ),
        ("Add the thing", "ada", "open")
    );
    assert_eq!((info.totals, info.changed_files), (Some((3, 1)), 2));
    assert_eq!(info.body, "Adds **the** thing.\n\n- one\n- two");
    let commits = block_on(github.commits(tab.clone())).expect("commits");
    assert_eq!(commits[0].message, "First\n\nbody");
    assert_eq!(commits[0].counts, Some((3, 1)));
    let read: Vec<(&str, &str, &str)> = commits
        .iter()
        .map(|commit| {
            (
                commit.oid.as_str(),
                commit.author.as_str(),
                commit.headline.as_str(),
            )
        })
        .collect();
    assert_eq!(
        read,
        [("aaa1", "ada", "First"), ("bbb2", "", "Second")],
        "both pages, by cursor"
    );

    let mut files = Vec::new();
    block_on(github.stream_diff(
        tab,
        CancelFlag::default(),
        Box::new(|file| files.push(file.path)),
    ))
    .expect("streamed");
    assert_eq!(files, ["src/a.rs"]);
    // Too large for the diff media type: the files API, paged.
    let mut files = Vec::new();
    block_on(github.stream_diff(
        pr(&github, &server, "app", 6),
        CancelFlag::default(),
        Box::new(|file| files.push(file.path)),
    ))
    .expect("streamed");
    assert_eq!(files, ["big.rs"]);
    let asked_diff = server
        .requests()
        .into_iter()
        .filter(|request| request.header("accept") == Some("application/vnd.github.diff"))
        .count();
    assert_eq!(asked_diff, 2);
}

#[test]
fn github_review_threads_with_a_token() {
    let server = github_server();
    let github = github_token(&server);
    let threads = block_on(github.review_comments(pr(&github, &server, "app", 5))).expect("read");
    super::github_scenarios::assert_review_threads(&threads);
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "two pages");
    let variables: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(
        variables["variables"],
        json!({ "owner": "team", "name": "app", "number": 5, "after": null })
    );
}

#[test]
fn github_token_failures_are_typed() {
    let server = github_server();
    let forge = github_token(&server);
    let kind = forge.kind();
    let host = server.host.clone();
    let view = |project: &str| block_on(forge.view(pr(&forge, &server, project, 1))).unwrap_err();
    assert_eq!(
        view("busy"),
        ForgeError::RateLimited {
            kind,
            host: host.clone()
        }
    );
    assert_eq!(
        view("sso"),
        ForgeError::NotAuthenticated {
            kind,
            host: host.clone()
        }
    );
    assert!(matches!(view("gone"), ForgeError::NotFound { .. }));

    // A token the server refuses, and none at all.
    let refused = github(
        &server,
        store("gh-refused", &[(&format!("github@{host}"), "wrong")]),
        Some("token"),
    );
    let error = block_on(refused.view(pr(&refused, &server, "app", 5))).unwrap_err();
    assert_eq!(
        error,
        ForgeError::NotAuthenticated {
            kind,
            host: host.clone()
        }
    );
    assert!(
        error.to_string().contains("Add a token in Settings"),
        "{error}"
    );
    let none = github(&server, store("gh-none", &[]), Some("token"));
    let error = block_on(none.view(pr(&none, &server, "app", 5))).unwrap_err();
    assert_eq!(error, ForgeError::NotAuthenticated { kind, host });
}

/// `auto`: a stored token when gh isn't signed in to the host, gh when no
/// token is stored.
#[test]
fn github_auto_takes_a_token_only_when_there_is_one() {
    let server = github_server();
    let host = server.host.clone();
    let with_token = github(
        &server,
        store("gh-auto", &[(&format!("github@{host}"), TOKEN)]),
        None,
    );
    block_on(with_token.view(pr(&with_token, &server, "app", 5))).expect("viewed with the token");
    assert_eq!(server.requests().len(), 1);

    let fake = FakeCli::new("gh-auto-cli", &[]);
    fake.answer_or_fail(&[(
        "pr view 5 *",
        "5\u{1f}Title\u{1f}me\u{1f}OPEN\u{1f}main\u{1f}topic\u{1f}1\u{1f}1\u{1f}1\u{1f}https://x/5\n",
        0,
    )]);
    let program = fake.program().display().to_string();
    let cli = bundled_with(
        "github",
        config(&server, "/api/v3", None, Some(&program)),
        store("gh-auto-none", &[]),
        &[&host],
        &scratch_dir("github-auto"),
    );
    let info = block_on(cli.view(pr(&cli, &server, "app", 5))).expect("viewed with gh");
    assert_eq!(info.title, "Title");
    // A gh too old to print a body, or a pull request without one: empty.
    assert_eq!(info.body, "");
    assert_eq!(fake.calls().len(), 1);
    assert_eq!(server.requests().len(), 1, "the API wasn't asked");
}

/// With more than one account on a host, a repository uses the one that can
/// see it: the host's own sign-in when it can, else whichever other account
/// can. Whose the repository is doesn't come into it.
#[test]
fn github_finds_the_account_that_can_see_a_repository() {
    let server = Server::start_with(|request| {
        let token = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned();
        let pull = || Answer::json(200, rest_pull(5, "open", false));
        match (
            request.method.as_str(),
            request.path.as_str(),
            token.as_str(),
        ) {
            ("GET", "/api/v3/user", "personal") => Answer::json(200, json!({ "login": "octo" })),
            ("GET", "/api/v3/user", "work") => {
                Answer::json(200, json!({ "login": "octo-at-work" }))
            }
            // `mine/app`: only the personal account sees it. `client/app`,
            // a repository neither account owns: only the work one.
            ("GET", "/api/v3/repos/mine/app", "personal") => Answer::json(200, json!({ "id": 1 })),
            ("GET", "/api/v3/repos/mine/app/pulls/5", "personal") => pull(),
            ("GET", "/api/v3/repos/client/app", "work") => Answer::json(200, json!({ "id": 2 })),
            ("GET", "/api/v3/repos/client/app/pulls/5", "work") => pull(),
            _ => Answer::json(404, json!({ "message": "Not Found" })),
        }
    });
    let host = server.host.clone();
    let github = github(
        &server,
        store(
            "gh-accounts",
            &[
                (&format!("github@{host}"), "personal"),
                (&format!("github@{host}/work"), "work"),
            ],
        ),
        Some("token"),
    );
    let view = |owner: &str| {
        let pr = github
            .claims(&format!("http://{host}/{owner}/app.git"))
            .expect("claimed")
            .pull_request(5);
        block_on(github.view(pr))
    };
    view("mine").expect("the host's own sign-in sees it");
    view("client").expect("the work account sees it");
    // A repository nobody sees is the host's sign-in's to report.
    assert!(matches!(
        view("nobody").unwrap_err(),
        ForgeError::NotFound { .. }
    ));
    // Asked once per repository, then the same account without asking.
    view("client").expect("again");
    let probes = |path: &str| {
        server
            .requests()
            .iter()
            .filter(|request| request.path == path)
            .count()
    };
    assert_eq!(probes("/api/v3/repos/mine/app"), 1);
    assert_eq!(probes("/api/v3/repos/client/app"), 2, "personal, then work");

    // Each row of the page asks about its own account.
    let asked = |account: Option<&str>| {
        let (tell, told) = std::sync::mpsc::channel();
        github.plugin().call::<Account>(
            PluginCall::Forge {
                repo: super::forge::no_repository(None),
                call: ForgeCall::Account {
                    host: host.clone(),
                    account: account.map(str::to_owned),
                    token: None,
                },
            },
            move |result| {
                let _ = tell.send(result.map_err(|error| error.to_string()));
            },
        );
        told.recv().expect("answered").expect("asked")
    };
    assert_eq!(asked(None).login.as_deref(), Some("octo"));
    assert_eq!(asked(Some("Work")).login.as_deref(), Some("octo-at-work"));
    assert_eq!(
        asked(Some("nobody")),
        Account {
            method: None,
            login: None
        }
    );
}

#[test]
fn github_account_says_whose_token_it_is() {
    let server = github_server();
    let host = server.host.clone();
    let github = github(&server, store("gh-account", &[]), None);
    assert_eq!(
        account(&github, &host, Some(TOKEN)).expect("valid"),
        Account {
            method: Some(AccountMethod::Token),
            login: Some("octo".into())
        }
    );
    let refused = account(&github, &host, Some("wrong")).unwrap_err();
    assert!(refused.contains("Bad credentials"), "{refused}");
    // No gh there, no token: not signed in, rather than an error.
    assert_eq!(
        account(&github, &host, None).expect("asked"),
        Account {
            method: None,
            login: None
        }
    );
    let signed_in = github_token(&server);
    assert_eq!(
        account(&signed_in, &host, None).expect("asked"),
        Account {
            method: Some(AccountMethod::Token),
            login: Some("octo".into())
        }
    );
}

/// GitLab's REST API under `/api/v4`.
fn gitlab_server() -> Server {
    Server::start_with(|request| {
        if !authorized(request, "gitlab") {
            return Answer::json(401, json!({ "message": "401 Unauthorized" }));
        }
        let mr = |iid: u64, branch: &str, state: &str| {
            json!({ "iid": iid, "web_url": format!("https://example.test/team/app/-/merge_requests/{iid}"),
                    "title": format!("MR {iid}"), "source_branch": branch, "target_branch": "main",
                    "sha": format!("sha{iid}"), "state": state, "draft": false,
                    "source_project_id": 1, "target_project_id": 1,
                    "author": { "username": "ada" }, "changes_count": "2" })
        };
        let diff = |path: &str| json!({ "old_path": path, "new_path": path, "diff": "@@ -1 +1 @@\n-a\n+b\n" });
        match (request.method.as_str(), request.path.as_str()) {
            (
                "GET",
                "/api/v4/projects/team%2Fapp/merge_requests?state=all&order_by=updated_at&per_page=100",
            ) => Answer::json(
                200,
                json!([mr(4, "feature-a", "opened"), mr(2, "feature-b", "merged")]),
            ),
            ("GET", "/api/v4/projects/team%2Fapp/repository/commits/sha4") => Answer::json(
                200,
                json!({ "id": "sha4", "last_pipeline": { "status": "running", "web_url": "https://ci/1" } }),
            ),
            ("POST", "/api/v4/projects/team%2Fapp/merge_requests") => {
                Answer::json(201, mr(9, "thing", "opened"))
            }
            ("GET", "/api/v4/projects/team%2Fapp/merge_requests/9") => {
                Answer::json(200, mr(9, "thing", "opened"))
            }
            ("GET", "/api/v4/projects/team%2Fapp/merge_requests/9/commits?per_page=100&page=1") => {
                Answer::json(
                    200,
                    json!([{ "id": "c2", "title": "Second", "message": "Second\n\nIts body", "author_name": "Ada" },
                       { "id": "c1", "title": "First", "author_name": "Ada" }]),
                )
            }
            ("GET", "/api/v4/projects/team%2Fapp/merge_requests/9/diffs?per_page=100&page=1") => {
                Answer::json(
                    200,
                    json!(
                        (0..100)
                            .map(|n| diff(&format!("f{n}.rs")))
                            .collect::<Vec<_>>()
                    ),
                )
            }
            ("GET", "/api/v4/projects/team%2Fapp/merge_requests/9/diffs?per_page=100&page=2") => {
                Answer::json(200, json!([diff("last.rs")]))
            }
            ("GET", "/api/v4/user") => Answer::json(200, json!({ "username": "ada" })),
            ("GET", "/api/v4/projects/team%2Fbusy/merge_requests/1") => {
                Answer::json(429, json!({ "message": "Retry later" }))
                    .header("ratelimit-remaining", "0")
            }
            _ => Answer::json(404, json!({ "message": "404 Project Not Found" })),
        }
    })
}

fn gitlab(server: &Server, store: Arc<dyn SecretStore>, auth: Option<&str>) -> ScriptForge {
    bundled_with(
        "gitlab",
        config(server, "/api/v4", auth, None),
        store,
        &[&server.host],
        &scratch_dir("gitlab-token"),
    )
}

#[test]
fn gitlab_does_everything_with_a_token() {
    let server = gitlab_server();
    let host = server.host.clone();
    // auto, with a token and no glab config: the token.
    let gitlab = gitlab(
        &server,
        store("gl-token", &[(&format!("gitlab@{host}"), TOKEN)]),
        None,
    );
    let prs = block_on(gitlab.pull_requests(
        remote(&gitlab, &server),
        vec!["feature-a".into(), "feature-b".into()],
    ))
    .expect("listed");
    assert_eq!(prs.iter().map(|pr| pr.number).collect::<Vec<_>>(), [4, 2]);
    let checks = block_on(gitlab.checks(remote(&gitlab, &server), "sha4".into())).expect("checks");
    assert_eq!(checks.state, CheckState::Pending);
    let created = block_on(gitlab.create_pull_request(
        remote(&gitlab, &server),
        CreatePr {
            head: "thing".into(),
            base: "main".into(),
            title: "Thing".into(),
            body: "Body".into(),
            draft: true,
        },
    ))
    .expect("created");
    assert_eq!(created.number, 9);
    let tab = pr(&gitlab, &server, "app", 9);
    let info = block_on(gitlab.view(tab.clone())).expect("viewed");
    assert_eq!((info.title.as_str(), info.changed_files), ("MR 9", 2));
    // A merge request without a description.
    assert_eq!(info.body, "");
    let commits = block_on(gitlab.commits(tab.clone())).expect("commits");
    assert_eq!(
        commits
            .iter()
            .map(|c| c.headline.as_str())
            .collect::<Vec<_>>(),
        ["First", "Second"]
    );
    // The whole message when the API has it; the title alone when not.
    assert_eq!(
        commits
            .iter()
            .map(|c| c.message.as_str())
            .collect::<Vec<_>>(),
        ["First", "Second\n\nIts body"]
    );
    let mut files = Vec::new();
    block_on(gitlab.stream_diff(
        tab,
        CancelFlag::default(),
        Box::new(|file| files.push(file.path)),
    ))
    .expect("streamed");
    assert_eq!(files.len(), 101, "both pages");
    assert_eq!(files.last().map(String::as_str), Some("last.rs"));

    let requests = server.requests();
    assert!(requests.iter().all(|request| authorized(request, "gitlab")));
    let sent: serde_json::Value = serde_json::from_str(&requests[2].body).unwrap();
    assert_eq!(
        sent,
        json!({ "source_branch": "thing", "target_branch": "main", "title": "Draft: Thing", "description": "Body" })
    );
    assert_eq!(
        account(&gitlab, &host, None).expect("asked"),
        Account {
            method: Some(AccountMethod::Token),
            login: Some("ada".into())
        }
    );
}

#[test]
fn gitlab_token_failures_are_typed() {
    let server = gitlab_server();
    let host = server.host.clone();
    let forge = gitlab(
        &server,
        store("gl-fail", &[(&format!("gitlab@{host}"), TOKEN)]),
        Some("token"),
    );
    let kind = forge.kind();
    let view = |project: &str| block_on(forge.view(pr(&forge, &server, project, 1))).unwrap_err();
    assert_eq!(
        view("busy"),
        ForgeError::RateLimited {
            kind,
            host: host.clone()
        }
    );
    assert!(matches!(view("gone"), ForgeError::NotFound { .. }));
    let refused = account(&forge, &host, Some("wrong")).unwrap_err();
    assert!(refused.contains("401"), "{refused}");
    let wrong = gitlab(
        &server,
        store("gl-wrong", &[(&format!("gitlab@{host}"), "wrong")]),
        Some("token"),
    );
    let error = block_on(wrong.view(pr(&wrong, &server, "app", 9))).unwrap_err();
    assert_eq!(error, ForgeError::NotAuthenticated { kind, host });
}

/// The whole way in the app: a repository on a company GitHub, gh not
/// there, so no badges; the token pasted on GitHub's page in Settings is
/// checked against the server, kept in the store (not in settings.toml),
/// and the bookmark gets its badge. Sign out removes it.
#[test]
fn a_token_pasted_in_settings_gets_badges() {
    use super::PluginAction;
    use super::page::{PageAction, draft_id};
    use super::scenarios::{launch_on, point_origin_at, wait_running};
    use crate::Action;

    let repo = crate::harness::fixture_repo();
    let head = super::scenarios::commit_of(&repo, "fix/parser-crash");
    let server = Server::start_with(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            _ if !authorized(request, "github") => {
                Answer::json(401, json!({ "message": "Bad credentials" }))
            }
            ("GET", "/api/v3/user") => Answer::json(200, json!({ "login": "octo" })),
            ("POST", "/api/graphql") if request.body.contains("pullRequests") => {
                let mut pull = listed(7, "fix/parser-crash", "OPEN", false);
                pull["headRefOid"] = json!(head);
                Answer::json(
                    200,
                    json!({ "data": { "repository": { "pullRequests": {
                    "nodes": [pull], "pageInfo": { "hasNextPage": false, "endCursor": null } } } } }),
                )
            }
            ("POST", "/api/graphql") => Answer::json(
                200,
                json!({ "data": { "repository": { "object": { "statusCheckRollup": { "state": "SUCCESS" } } } } }),
            ),
            _ => Answer::json(404, json!({ "message": "Not Found" })),
        }
    });
    let host = server.host.clone();
    point_origin_at(&repo, &format!("http://{host}/team/app.git"));
    let config = format!(
        "[plugins.github]\nhosts = [\"{host}\"]\nprogram = \"/nonexistent/diffui-test/gh\"\n\n[plugins.github.per_host.\"{host}\"]\napi_url = \"http://{host}/api/v3\"\n"
    );
    let mut h = launch_on(&repo, &[], &config, "");
    wait_running(&mut h, "github");
    let page = |action| Action::Plugin(PluginAction::Page(action));
    h.perform(page(PageAction::Open {
        id: "github".into(),
        host: None,
    }));
    h.wait_until("the account check", |ui| {
        ui.plugins.page.as_ref().is_some_and(|page| page.settled())
    });
    h.read(|ui| {
        assert!(
            ui.active().forge.of("fix/parser-crash").is_none(),
            "no badge without a token"
        )
    });

    // A wrong one is refused and not kept.
    h.perform(page(PageAction::Draft {
        field: draft_id("token", Some(&host)),
        text: "wrong".into(),
    }));
    h.perform(page(PageAction::SaveSecret {
        key: "token".into(),
        host: Some(host.clone()),
    }));
    h.wait_until("the refusal", |ui| {
        ui.plugins.page.as_ref().is_some_and(|page| page.settled())
    });
    h.read(|ui| {
        assert!(
            ui.plugins
                .secrets
                .get(&format!("github@{host}"))
                .unwrap()
                .is_none()
        );
    });

    h.perform(page(PageAction::Draft {
        field: draft_id("token", Some(&host)),
        text: format!(" {TOKEN}\n"),
    }));
    h.perform(page(PageAction::SaveSecret {
        key: "token".into(),
        host: Some(host.clone()),
    }));
    h.wait_until("the badge", |ui| {
        ui.active()
            .forge
            .of("fix/parser-crash")
            .is_some_and(|decoration| decoration.checks.is_some())
    });
    h.read(|ui| {
        assert_eq!(
            ui.plugins
                .secrets
                .get(&format!("github@{host}"))
                .unwrap()
                .as_deref(),
            Some(TOKEN)
        );
        let decoration = ui.active().forge.of("fix/parser-crash").unwrap();
        assert_eq!(decoration.pr.number, 7);
        assert_eq!(
            decoration.checks.as_ref().unwrap().state,
            CheckState::Success
        );
        assert!(
            ui.toasts
                .iter()
                .all(|toast| !format!("{:?}", toast.detail).contains(TOKEN)),
            "a token is never in a toast"
        );
    });
    let settings = std::fs::read_to_string(
        crate::harness::scratch_home().join(".config/diffui/settings.toml"),
    )
    .unwrap_or_default();
    assert!(!settings.contains(TOKEN), "{settings}");
    h.wait_until("the account says the token", |ui| {
        ui.plugins
            .page
            .as_ref()
            .is_some_and(|page| page.settled() && page.signed_in(&host).as_deref() == Some("octo"))
    });

    h.perform(page(PageAction::ClearSecret {
        key: "token".into(),
        host: Some(host.clone()),
    }));
    h.read(|ui| {
        assert!(
            ui.plugins
                .secrets
                .get(&format!("github@{host}"))
                .unwrap()
                .is_none()
        );
    });
}

/// The page's state of `host`: listed, removable, and why it stays.
/// "Another account" gives a host a second row with a token of its own,
/// kept apart from the host's and removed on its own.
#[test]
fn another_account_gets_a_row_and_a_token_of_its_own_on_the_page() {
    use super::PluginAction;
    use super::page::{NEW_ACCOUNT, PageAction, draft_id};
    use super::scenarios::{launch_on, wait_running};
    use crate::Action;

    let repo = crate::harness::fixture_repo();
    let server =
        Server::start_with(
            move |request| match (request.method.as_str(), request.path.as_str()) {
                _ if !authorized(request, "github") => {
                    Answer::json(401, json!({ "message": "Bad credentials" }))
                }
                ("GET", "/api/v3/user") => Answer::json(200, json!({ "login": "octo-at-work" })),
                _ => Answer::json(404, json!({ "message": "Not Found" })),
            },
        );
    let host = server.host.clone();
    let config = format!(
        "[plugins.github]\nhosts = [\"{host}\"]\nprogram = \"/nonexistent/diffui-test/gh\"\n\n[plugins.github.per_host.\"{host}\"]\napi_url = \"http://{host}/api/v3\"\n"
    );
    let mut h = launch_on(&repo, &[], &config, "");
    wait_running(&mut h, "github");
    let page = |action| Action::Plugin(PluginAction::Page(action));
    h.perform(page(PageAction::Open {
        id: "github".into(),
        host: None,
    }));
    let scope = format!("{host}/work");
    h.perform(page(PageAction::Draft {
        field: NEW_ACCOUNT,
        text: format!(" {host}/Work "),
    }));
    h.perform(page(PageAction::AddAccount));
    h.read(|ui| {
        let rows = ui.page_hosts("github");
        let at = rows.iter().position(|row| *row == host).expect("the host");
        assert_eq!(rows.get(at + 1), Some(&scope), "under its host: {rows:?}");
    });

    h.perform(page(PageAction::Draft {
        field: draft_id("token", Some(&scope)),
        text: TOKEN.into(),
    }));
    h.perform(page(PageAction::SaveSecret {
        key: "token".into(),
        host: Some(scope.clone()),
    }));
    h.wait_until("the account's token to be kept", |ui| {
        ui.plugins
            .secrets
            .get(&format!("github@{scope}"))
            .is_ok_and(|kept| kept.as_deref() == Some(TOKEN))
    });
    h.wait_until("who the account is", |ui| {
        ui.plugins
            .page
            .as_ref()
            .is_some_and(|page| page.signed_in(&scope).as_deref() == Some("octo-at-work"))
    });
    h.read(|ui| {
        assert!(
            ui.plugins
                .secrets
                .get(&format!("github@{host}"))
                .unwrap()
                .is_none(),
            "the host's own account is untouched"
        );
    });

    // Removing it asks, since it deletes a token, then leaves the host.
    h.perform(page(PageAction::RemoveHost {
        host: scope.clone(),
        confirmed: false,
    }));
    h.read(|ui| {
        assert_eq!(
            ui.plugins.page.as_ref().unwrap().removing(),
            Some(scope.as_str())
        )
    });
    h.perform(page(PageAction::RemoveHost {
        host: scope.clone(),
        confirmed: true,
    }));
    h.read(|ui| {
        assert!(
            ui.plugins
                .secrets
                .get(&format!("github@{scope}"))
                .unwrap()
                .is_none()
        );
        let rows = ui.page_hosts("github");
        assert!(rows.contains(&host) && !rows.contains(&scope), "{rows:?}");
    });
}

fn host_on_page(ui: &crate::Diffui, host: &str) -> (bool, bool, Option<String>) {
    let page = ui.plugins.page.as_ref().expect("the page");
    let manifest = ui
        .plugins
        .get("github")
        .and_then(|found| found.manifest.clone())
        .expect("github's manifest");
    (
        ui.page_hosts("github").iter().any(|known| known == host),
        ui.host_removable(page, &manifest, host),
        ui.host_kept_by("github", &manifest, host),
    )
}

fn config_files() -> (String, String) {
    let dir = crate::harness::scratch_home().join(".config/diffui");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    (read("config.toml"), read("settings.toml"))
}

/// GitHub's page, open, on the fixture with gh not there.
fn github_page(config: &str, settings: &str) -> crate::harness::Harness {
    use super::PluginAction;
    use super::page::PageAction;
    use super::scenarios::{launch_on, wait_running};

    let repo = crate::harness::fixture_repo();
    let config = format!("[plugins.github]\nprogram = \"/nonexistent/diffui-test/gh\"\n{config}");
    let mut h = launch_on(&repo, &[], &config, settings);
    wait_running(&mut h, "github");
    h.perform(crate::Action::Plugin(PluginAction::Page(
        PageAction::Open {
            id: "github".into(),
            host: None,
        },
    )));
    h
}

fn page(action: super::page::PageAction) -> crate::Action {
    crate::Action::Plugin(super::PluginAction::Page(action))
}

/// A host added on the page, then given a sign-in method of its own, goes
/// with both when it is removed: settings.toml is back to nothing.
#[test]
fn a_host_added_on_the_page_is_removed_with_its_settings() {
    use super::page::{NEW_HOST, PageAction};

    let host = "git.acme.test";
    let mut h = github_page("", "");
    h.perform(page(PageAction::Draft {
        field: NEW_HOST,
        text: format!("https://{host}/"),
    }));
    h.perform(page(PageAction::AddHost));
    h.perform(page(PageAction::Set {
        key: "auth".into(),
        host: Some(host.into()),
        value: Some("cli".into()),
    }));
    let (config, settings) = config_files();
    assert!(
        settings.contains(&format!("hosts = [\"{host}\"]")),
        "{settings}"
    );
    assert!(
        settings.contains(&format!("[plugins.github.config.per_host.\"{host}\"]")),
        "{settings}"
    );
    h.read(|ui| assert_eq!(host_on_page(ui, host), (true, true, None)));

    h.perform(page(PageAction::RemoveHost {
        host: host.into(),
        confirmed: false,
    }));
    let (after, settings) = config_files();
    assert_eq!(after, config, "config.toml isn't written");
    assert!(!settings.contains(host), "{settings}");
    h.read(|ui| {
        assert_eq!(ui.plugins.page.as_ref().unwrap().removing(), None);
        assert!(!host_on_page(ui, host).0);
    });
}

/// A host with a saved token asks first; Cancel keeps it, Remove takes the
/// token out of the store and the host out of settings.toml.
#[test]
fn removing_a_host_with_a_token_asks_then_deletes_the_token() {
    use super::page::PageAction;

    let host = "git.acme.test";
    let account = format!("github@{host}");
    let mut h = github_page(
        "",
        &format!(
            "[plugins.github.config]\nhosts = [\"{host}\"]\n\n[plugins.github.config.per_host.\"{host}\"]\nauth = \"cli\"\n"
        ),
    );
    h.read(|ui| ui.plugins.secrets.set(&account, TOKEN).unwrap());
    // The page reads the store when it opens.
    h.perform(page(PageAction::Open {
        id: "github".into(),
        host: None,
    }));
    let ask = || {
        page(PageAction::RemoveHost {
            host: host.into(),
            confirmed: false,
        })
    };

    h.perform(ask());
    h.read(|ui| {
        assert_eq!(ui.plugins.page.as_ref().unwrap().removing(), Some(host));
        assert_eq!(
            ui.plugins.secrets.get(&account).unwrap().as_deref(),
            Some(TOKEN)
        );
    });
    h.perform(page(PageAction::KeepHost));
    h.read(|ui| {
        assert_eq!(ui.plugins.page.as_ref().unwrap().removing(), None);
        assert_eq!(host_on_page(ui, host), (true, true, None));
    });
    assert!(config_files().1.contains(host));

    h.perform(ask());
    h.perform(page(PageAction::RemoveHost {
        host: host.into(),
        confirmed: true,
    }));
    h.read(|ui| {
        assert_eq!(ui.plugins.secrets.get(&account).unwrap(), None);
        assert!(
            !ui.plugins
                .secrets
                .accounts()
                .unwrap()
                .iter()
                .any(|known| known.contains(host))
        );
        assert_eq!(ui.plugins.page.as_ref().unwrap().removing(), None);
        assert!(!host_on_page(ui, host).0);
    });
    let (config, settings) = config_files();
    assert!(!settings.contains(host), "{settings}");
    assert!(!config.contains(host), "{config}");
    let secrets =
        std::fs::read_to_string(crate::harness::scratch_home().join(".diffui-secrets.json"))
            .unwrap_or_default();
    assert!(!secrets.contains(host), "{secrets}");
}

/// A host config.toml lists can't be removed from the page: what Settings
/// saved for it goes, config.toml is left alone, the host stays and the page
/// and a toast say why.
#[test]
fn a_host_config_toml_lists_stays_and_says_why() {
    use super::page::PageAction;

    let host = "git.corp.test";
    let mut h = github_page(
        &format!("hosts = [\"{host}\"]\n"),
        &format!("[plugins.github.config.per_host.\"{host}\"]\nauth = \"cli\"\n"),
    );
    let (config, _) = config_files();
    h.read(|ui| {
        assert_eq!(
            host_on_page(ui, host),
            (true, true, Some("Set in config.toml".to_owned()))
        )
    });

    h.perform(page(PageAction::RemoveHost {
        host: host.into(),
        confirmed: false,
    }));
    let (after, settings) = config_files();
    assert_eq!(after, config);
    assert!(!settings.contains(host), "{settings}");
    h.read(|ui| {
        assert_eq!(
            host_on_page(ui, host),
            (true, false, Some("Set in config.toml".to_owned()))
        );
        assert!(
            ui.toasts
                .iter()
                .any(|toast| format!("{:?}", toast.detail).contains("Set in config.toml")),
            "a toast says why it stays"
        );
    });
}
