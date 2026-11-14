//! The bundled GitLab plugin (`plugins/bundled/gitlab`) against a fake
//! `glab` (`core/tests/fixtures/fake-forge-cli.sh`): what it runs, and what
//! the app makes of its answers. Nothing here reaches a real forge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diffui_core::forge::{
    CheckState, CreatePr, ForgeAbility, ForgeError, ForgeKind, ForgePr, ForgeProvider, ForgeRemote,
    PrState, UpdatePr,
};
use diffui_core::model::DiffFileStatus;
use diffui_core::repo::CancelFlag;
use diffui_plugins::protocol;
use diffui_plugins::{Manifest, Plugin, Reply};

use super::{Handles, ScriptForge, spec_of};

/// A fake `glab` in its own directory: rules in, recorded calls out.
struct Fake {
    dir: PathBuf,
}

impl Fake {
    /// `name` is unique per test: the tests run concurrently.
    fn new(name: &str, rules: &[(&str, &str, i32)]) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("diffui-gitlab-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("core/tests/fixtures/fake-forge-cli.sh");
        std::fs::copy(script, dir.join("glab")).unwrap();
        std::fs::set_permissions(dir.join("glab"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut table = String::new();
        for (index, (pattern, response, status)) in rules.iter().enumerate() {
            let file = format!("response-{index}");
            std::fs::write(dir.join(&file), response).unwrap();
            table.push_str(&format!("{pattern}\t{file}\t{status}\n"));
        }
        std::fs::write(dir.join("rules"), table).unwrap();
        Self { dir }
    }

    fn program(&self) -> PathBuf {
        self.dir.join("glab")
    }

    /// Each call's arguments, its host variable first as `HOST=…`.
    fn calls(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
        text.split("\u{1}\n")
            .filter(|call| !call.is_empty())
            .map(|call| {
                call.split('\0')
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn has(call: &[String], args: &[&str]) -> bool {
    call.windows(args.len())
        .any(|window| window.iter().zip(args).all(|(a, b)| a == b))
}

struct Quiet;

impl diffui_plugins::Host for Quiet {
    fn call(&self, _plugin: &str, _call: protocol::HostCall, _reply: Reply) {}
    fn event(&self, _plugin: &str, _event: diffui_plugins::PluginEvent) {}
}

/// The bundled plugin's manifest, from the files built into diffui.
fn manifest() -> Manifest {
    let (_, files) = diffui_plugins::BUNDLED
        .iter()
        .find(|(id, _)| *id == "gitlab")
        .expect("GitLab is bundled");
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect();
    Manifest::bundled(Path::new("(bundled)/gitlab"), files).expect("its manifest reads")
}

/// The bundled GitLab plugin running as the app runs it, driving `program`.
fn gitlab(program: &Path) -> ScriptForge {
    let manifest = manifest();
    let mut config = toml::Table::new();
    config.insert("program".into(), program.display().to_string().into());
    let plugin = Plugin::spawn(
        manifest.clone(),
        diffui_plugins::Options {
            config,
            data_dir: std::env::temp_dir().join("diffui-gitlab-data"),
            limits: Default::default(),
        },
        Arc::new(Quiet),
    );
    ScriptForge {
        plugin: Arc::new(plugin),
        kind: ForgeKind::new(spec_of(&manifest).expect("a forge")),
        hosts: manifest.forge.as_ref().unwrap().hosts.clone(),
        methods: None,
        handles: Handles::default(),
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    crate::spawn::runtime().block_on(future)
}

fn remote(gitlab: &ScriptForge) -> ForgeRemote {
    gitlab
        .claims("git@gitlab.acme.org:group/sub/app.git")
        .expect("claimed")
}

/// `group/app!3` on gitlab.com, as a link or a reference names it.
fn merge_request(gitlab: &ScriptForge, reference: &str) -> ForgePr {
    ForgePr::parse_with(reference, &[gitlab.kind]).expect("a merge request")
}

#[test]
fn it_says_what_gitlab_says() {
    let kind = gitlab(Path::new("glab")).kind;
    assert_eq!(
        (kind.id(), kind.name(), kind.cli(), kind.default_host()),
        ("gitlab", "GitLab", "glab", "gitlab.com")
    );
    assert_eq!(
        (kind.noun(), kind.short(), kind.sigil()),
        ("merge request", "MR", '!')
    );
    let mr = merge_request(
        &gitlab(Path::new("glab")),
        "https://gitlab.com/group/sub/app/-/merge_requests/7/diffs",
    );
    assert_eq!(
        (mr.owner.as_str(), mr.name.as_str(), mr.number),
        ("group/sub", "app", 7)
    );
}

#[test]
fn claims_gitlab_hosts_with_subgroups() {
    let gitlab = gitlab(Path::new("glab"));
    let remote = gitlab
        .claims("git@gitlab.com:group/sub/app.git")
        .expect("claimed");
    assert_eq!(remote.slug(), "group/sub/app");
    assert_eq!(remote.web_url, "https://gitlab.com/group/sub/app");
    let https = gitlab
        .claims("https://gitlab.com/group/sub/app.git")
        .expect("claimed");
    assert_eq!(
        (https.owner.as_str(), https.name.as_str()),
        ("group/sub", "app")
    );
    // A host named like a GitLab.
    let company = gitlab
        .claims("git@gitlab.acme.org:infra/tools.git")
        .expect("claimed");
    assert_eq!(company.kind, gitlab.kind);
    assert!(gitlab.claims("git@code.gitlab:team/app.git").is_some());
    assert!(gitlab.claims("git@github.com:team/app.git").is_none());
    assert!(gitlab.claims("git@example.org:team/app.git").is_none());
}

#[test]
fn a_missing_or_signed_out_glab_is_a_typed_error() {
    let missing = gitlab(Path::new("/nonexistent/diffui-test/glab"));
    let error = block_on(missing.pull_requests(remote(&missing), vec!["main".into()])).unwrap_err();
    assert_eq!(error, ForgeError::MissingCli { kind: missing.kind });

    let fake = Fake::new("signed-out", &[("*", "ERROR: 401 Unauthorized\n", 1)]);
    let gitlab = gitlab(&fake.program());
    let error = block_on(gitlab.pull_requests(remote(&gitlab), vec!["main".into()])).unwrap_err();
    assert_eq!(
        error,
        ForgeError::NotAuthenticated {
            kind: gitlab.kind,
            host: "gitlab.acme.org".into()
        }
    );
    assert!(
        error
            .to_string()
            .contains("glab auth login --hostname gitlab.acme.org"),
        "{error}"
    );
}

#[test]
fn failures_get_typed() {
    let fake = Fake::new(
        "failures",
        &[
            ("*/merge_requests/1", "API rate limit exceeded\n", 1),
            ("*/merge_requests/2", "ERROR: 404 Project Not Found\n", 1),
            (
                "*/merge_requests/3",
                "dial tcp: lookup gitlab.com: no such host\n",
                1,
            ),
            (
                "*/merge_requests/4",
                "{\"message\": \"Validation failed\"}\n",
                1,
            ),
            ("*/merge_requests/5", "not json", 0),
        ],
    );
    let gitlab = gitlab(&fake.program());
    let view = |number: u64| {
        block_on(gitlab.view(merge_request(&gitlab, &format!("group/app!{number}")))).unwrap_err()
    };
    assert!(
        matches!(view(1), ForgeError::RateLimited { .. }),
        "{:?}",
        view(1)
    );
    assert_eq!(
        view(2),
        ForgeError::NotFound {
            kind: gitlab.kind,
            host: "gitlab.com".into(),
            what: "group/app!2".into()
        }
    );
    assert_eq!(
        view(3),
        ForgeError::Unreachable {
            host: "gitlab.com".into()
        }
    );
    match view(4) {
        ForgeError::Failed(message) => {
            assert!(message.starts_with("glab failed: "), "{message}");
            assert!(message.contains("Validation failed"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    match view(5) {
        ForgeError::Failed(message) => {
            assert!(
                message.contains("glab returned something diffui couldn't read"),
                "{message}"
            )
        }
        other => panic!("{other:?}"),
    }
}

const GITLAB_LIST: &str = r#"[
  {"iid": 12, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/12",
   "title": "Draft: Tidy", "source_branch": "tidy", "target_branch": "main", "sha": "t12",
   "state": "opened", "draft": true, "source_project_id": 5, "target_project_id": 5},
  {"iid": 11, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/11",
   "title": "Ship", "source_branch": "ship", "target_branch": "main", "sha": "s11",
   "state": "merged", "source_project_id": 5, "target_project_id": 5},
  {"iid": 10, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/10",
   "title": "Fork", "source_branch": "tidy", "target_branch": "main", "sha": "f10",
   "state": "opened", "source_project_id": 9, "target_project_id": 5},
  {"iid": 9, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/9",
   "title": "Ship, first try", "source_branch": "ship", "target_branch": "main", "sha": "s9",
   "state": "closed", "source_project_id": 5, "target_project_id": 5}
]"#;

const GITLAB_MR: &str = r#"{"iid": 13, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/13",
  "title": "Draft: New", "source_branch": "new", "target_branch": "tidy", "sha": "n13",
  "state": "opened", "draft": true, "source_project_id": 5, "target_project_id": 5,
  "author": {"username": "me"}, "changes_count": "2"}"#;

#[test]
fn gitlab_lists_creates_and_reads_pipelines() {
    let fake = Fake::new(
        "all",
        &[
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/merge_requests?state=all*",
                GITLAB_LIST,
                0,
            ),
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/merge_requests -X POST *",
                GITLAB_MR,
                0,
            ),
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/merge_requests/13 -X PUT *",
                GITLAB_MR,
                0,
            ),
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/repository/commits/t12",
                r#"{"id": "t12", "title": "Tidy", "last_pipeline": {"status": "running", "web_url": "https://gitlab.acme.org/p/1"}}"#,
                0,
            ),
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/repository/commits/s11",
                r#"{"id": "s11", "title": "Ship", "last_pipeline": {"status": "canceled"}}"#,
                0,
            ),
            (
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/repository/commits/*",
                r#"{"id": "x", "title": "x"}"#,
                0,
            ),
        ],
    );
    let gitlab = gitlab(&fake.program());
    let remote = || remote(&gitlab);
    let prs = block_on(gitlab.pull_requests(remote(), vec!["tidy".into(), "ship".into()]))
        .expect("listed");
    let summary: Vec<(u64, PrState)> = prs.iter().map(|pr| (pr.number, pr.state)).collect();
    assert_eq!(
        summary,
        [(12, PrState::Draft), (11, PrState::Merged)],
        "the fork's `tidy` isn't ours, and the latest of two closed ones wins"
    );
    assert_eq!((prs[0].head_sha.as_str(), prs[0].review), ("t12", None));

    let created = block_on(gitlab.create_pull_request(
        remote(),
        CreatePr {
            head: "new".into(),
            base: "tidy".into(),
            title: "New".into(),
            body: "Body".into(),
            draft: true,
        },
    ))
    .expect("created");
    assert_eq!((created.number, created.state), (13, PrState::Draft));
    block_on(gitlab.update_pull_request(
        remote(),
        13,
        UpdatePr {
            base: Some("main".into()),
            ..UpdatePr::default()
        },
    ))
    .expect("updated");

    let running = block_on(gitlab.checks(remote(), "t12".into())).expect("checks");
    assert_eq!(running.state, CheckState::Pending);
    assert_eq!(running.url.as_deref(), Some("https://gitlab.acme.org/p/1"));
    let canceled = block_on(gitlab.checks(remote(), "s11".into())).expect("checks");
    assert_eq!((canceled.state, canceled.url), (CheckState::Failure, None));
    let none = block_on(gitlab.checks(remote(), "other".into())).expect("checks");
    assert_eq!(none.state, CheckState::None);

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .all(|call| has(call, &["HOST=gitlab.acme.org"]))
    );
    assert_eq!(
        calls.len(),
        6,
        "one call for every branch, one per write and check"
    );
    assert!(
        has(&calls[1], &["-f", "title=Draft: New"]),
        "a draft is a title prefix"
    );
    assert!(has(&calls[1], &["-f", "source_branch=new"]));
    assert!(has(&calls[1], &["-f", "description=Body"]));
    assert!(has(&calls[2], &["-f", "target_branch=main"]));
    assert!(
        !calls[2].iter().any(|arg| arg.starts_with("title=")),
        "an update sends only what changes"
    );
}

#[test]
fn projects_are_addressed_by_their_encoded_path() {
    let fake = Fake::new(
        "encoded",
        &[(
            "api --hostname gitlab.com projects/group%2Fsub%2Fmy.app/merge_requests/1",
            GITLAB_MR,
            0,
        )],
    );
    let gitlab = gitlab(&fake.program());
    let mr = merge_request(&gitlab, "gitlab.com/group/sub/my.app!1");
    block_on(gitlab.view(mr)).expect("viewed");
}

/// glab names a host without the port an https remote can carry.
#[test]
fn glab_is_asked_about_the_host_without_its_port() {
    let fake = Fake::new(
        "port",
        &[(
            "api --hostname gitlab.acme.org projects/group%2Fapp/repository/commits/abc",
            r#"{"id": "abc", "last_pipeline": {"status": "success"}}"#,
            0,
        )],
    );
    let gitlab = gitlab(&fake.program());
    let remote = gitlab
        .claims("https://gitlab.acme.org:8443/group/app.git")
        .expect("claimed");
    assert_eq!(remote.web_url, "https://gitlab.acme.org:8443/group/app");
    let checks = block_on(gitlab.checks(remote, "abc".into())).expect("checks");
    assert_eq!(checks.state, CheckState::Success);
    assert!(has(&fake.calls()[0], &["HOST=gitlab.acme.org"]));
}

#[test]
fn a_merge_request_tab_reads_through_the_plugin() {
    let mr_diffs = r#"[{"old_path": "a", "new_path": "a", "diff": "@@ -1 +1 @@\n-x\n+y\n"},
        {"old_path": "a.rs", "new_path": "b.rs", "renamed_file": true, "diff": "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n+more\n"},
        {"old_path": "gone", "new_path": "gone", "deleted_file": true, "diff": "@@ -1 +0,0 @@\n-x\n"},
        {"old_path": "big.bin", "new_path": "big.bin", "new_file": true, "diff": ""}]"#;
    let fake = Fake::new(
        "tab",
        &[
            (
                "api --hostname gitlab.com projects/group%2Fapp/merge_requests/3/diffs*",
                mr_diffs,
                0,
            ),
            (
                "api --hostname gitlab.com projects/group%2Fapp/merge_requests/3",
                GITLAB_MR,
                0,
            ),
        ],
    );
    let gitlab = gitlab(&fake.program());
    assert!(gitlab.can(ForgeAbility::Tab));
    let mr = merge_request(&gitlab, "group/app!3");
    let info = block_on(gitlab.view(mr.clone())).expect("viewed");
    assert_eq!((info.state.as_str(), info.changed_files), ("draft", 2));
    assert_eq!(
        (info.number, info.author.as_str(), info.totals),
        (13, "me", None)
    );
    assert_eq!(
        (info.base_ref.as_str(), info.head_ref.as_str()),
        ("tidy", "new")
    );
    let mut files = Vec::new();
    block_on(gitlab.stream_diff(mr, CancelFlag::default(), Box::new(|file| files.push(file))))
        .expect("streamed");
    let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["a", "b.rs", "gone", "big.bin"]);
    let renamed = &files[1];
    assert_eq!(renamed.status, DiffFileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("a.rs"));
    assert_eq!((renamed.additions, renamed.deletions), (2, 1));
    assert_eq!(files[0].old_path, None);
    assert_eq!(files[2].status, DiffFileStatus::Deleted);
    assert_eq!(files[3].status, DiffFileStatus::Added);
    assert!(files[3].hunks[0].header.contains("diff unavailable"));
    let calls = fake.calls();
    assert!(calls.iter().all(|call| has(call, &["HOST=gitlab.com"])));
    assert!(has(
        &calls[1],
        &["projects/group%2Fapp/merge_requests/3/diffs?per_page=100&page=1"]
    ));
}

/// A merge request's commits page by page, oldest first; a commit's diff
/// with its message and its own counts, or the files' when it has none.
#[test]
fn commits_and_a_commits_diff_read_through_the_plugin() {
    // Newest first, as GitLab lists them: c149 down to c0.
    let newest: String = {
        let mut all: Vec<String> = (0..150)
            .rev()
            .map(|n| {
                format!(
                    r#"{{"id": "c{n}", "title": "Commit {n}", "author_name": "Ada", "authored_date": "2026-03-10T10:00:00Z"}}"#
                )
            })
            .collect();
        let second = all.split_off(100);
        format!("[{}]\n[{}]", all.join(","), second.join(","))
    };
    let (first, second) = newest.split_once('\n').unwrap();
    let commit_diff = r#"[{"old_path": "one.txt", "new_path": "one.txt", "new_file": true, "diff": "@@ -0,0 +1,2 @@\n+a\n+b\n"}]"#;
    let fake = Fake::new(
        "commits",
        &[
            ("*/merge_requests/3/commits?per_page=100&page=1", first, 0),
            ("*/merge_requests/3/commits?per_page=100&page=2", second, 0),
            ("*/repository/commits/c1/diff?*", commit_diff, 0),
            ("*/repository/commits/c2/diff?*", commit_diff, 0),
            (
                "*/repository/commits/c1",
                r#"{"id": "c1", "title": "First", "message": "First\n\nIts body", "author_name": "Ada", "author_email": "ada@example.com", "authored_date": "2026-03-10T10:00:00Z", "stats": {"additions": 5, "deletions": 4}}"#,
                0,
            ),
            (
                "*/repository/commits/c2",
                r#"{"id": "c2", "title": "Second"}"#,
                0,
            ),
        ],
    );
    let gitlab = gitlab(&fake.program());
    let mr = merge_request(&gitlab, "group/app!3");
    let commits = block_on(gitlab.commits(mr.clone())).expect("the commits");
    assert_eq!(commits.len(), 150);
    assert_eq!(
        (commits[0].oid.as_str(), commits[149].oid.as_str()),
        ("c0", "c149"),
        "oldest first"
    );
    assert_eq!(
        (commits[0].headline.as_str(), commits[0].author.as_str()),
        ("Commit 0", "Ada")
    );
    assert!(commits[0].authored_millis.is_some());

    let (document, details) =
        block_on(gitlab.commit_diff(mr.clone(), "c1".into(), CancelFlag::default()))
            .expect("a commit's diff");
    assert_eq!(document.files[0].path, "one.txt");
    assert_eq!(
        (document.total_additions, document.total_deletions),
        (5, 4),
        "the commit's own counts"
    );
    let details = details.expect("its details");
    assert_eq!(details.commit_id, "c1");
    assert_eq!(details.description, "First\n\nIts body");
    assert_eq!(
        (details.author.name.as_str(), details.author.email.as_str()),
        ("Ada", "ada@example.com")
    );
    let (document, details) = block_on(gitlab.commit_diff(mr, "c2".into(), CancelFlag::default()))
        .expect("a commit's diff");
    assert_eq!((document.total_additions, document.total_deletions), (2, 0));
    assert_eq!(details.expect("its details").description, "Second");
}

/// The hosts glab is signed in to are GitLab's too, whatever they're named:
/// the plugin's `hosts()` reads them from glab's config file.
#[test]
fn hosts_glab_is_configured_for_are_gitlabs() {
    use crate::harness::Harness;
    let config = "git_protocol: ssh\nhosts:\n  gitlab.com:\n    token: x\n    api_host: gitlab.com\n  \"code.corp.invalid\":\n    token: y\nother: 1\n";
    let mut h = Harness::open_prepared(None, (1280.0, 800.0), "", "", |home| {
        let dir = home.join(".config/glab-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yml"), config).unwrap();
    });
    let claimed =
        |ui: &crate::Diffui, url: &str| ui.forge.forges.claim(url).map(|remote| remote.kind.id());
    h.wait_until("hosts() to answer", |ui| {
        claimed(ui, "git@code.corp.invalid:infra/tools.git") == Some("gitlab")
    });
    h.read(|ui| {
        assert_eq!(claimed(ui, "git@gitlab.com:group/app.git"), Some("gitlab"));
        assert_eq!(claimed(ui, "git@api.corp.invalid:infra/tools.git"), None);
    });
    drop(h);
    // Other tests share the home.
    let _ = std::fs::remove_dir_all(crate::harness::scratch_home().join(".config/glab-cli"));
}

/// Not a check: the numbers in the forge-gitlab report. A badge refresh of
/// 20 bookmarks (one listing, then the checks of the open ones, four at a
/// time, as the app asks) and a merge request tab (header, commits, and a
/// diff of `MEASURE_FILES` files, 300 by default, in pages of 100), against
/// the fake `glab`. The change that added this test also ran it against the
/// built-in provider the plugin replaced (`MEASURE_PATH=native`).
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_badges_and_a_merge_request_tab() {
    use futures::StreamExt as _;
    let files: usize = std::env::var("MEASURE_FILES")
        .ok()
        .and_then(|files| files.parse().ok())
        .unwrap_or(300);
    let branches: Vec<String> = (0..20).map(|n| format!("feature-{n}")).collect();
    let listed: Vec<String> = branches
        .iter()
        .enumerate()
        .map(|(n, branch)| {
            let state = ["opened", "merged", "closed"][n % 3];
            format!(
                r#"{{"iid": {n}, "web_url": "https://gitlab.com/group/app/-/merge_requests/{n}", "title": "Change {n}", "source_branch": "{branch}", "target_branch": "main", "sha": "sha{n}", "state": "{state}", "source_project_id": 1, "target_project_id": 1}}"#
            )
        })
        .collect();
    let listed = format!("[{}]", listed.join(","));
    let page = |from: usize| -> String {
        let entries: Vec<String> = (from..(from + 100).min(files))
            .map(|n| {
                let lines: String = (0..20).map(|l| format!("-old {l}\\n+new {l}\\n")).collect();
                format!(
                    r#"{{"old_path": "src/f{n}.rs", "new_path": "src/f{n}.rs", "diff": "@@ -1,20 +1,20 @@\n{lines}"}}"#
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    };
    let pages: Vec<String> = (0..=files / 100).map(|p| page(p * 100)).collect();
    let page_rules: Vec<String> = (0..pages.len())
        .map(|p| format!("*/merge_requests/1/diffs?per_page=100&page={}", p + 1))
        .collect();
    let mut rules: Vec<(&str, &str, i32)> = vec![
        ("*/merge_requests?state=all*", &listed, 0),
        (
            "*/repository/commits/*",
            r#"{"id": "x", "title": "x", "last_pipeline": {"status": "success"}}"#,
            0,
        ),
        (
            "*/merge_requests/1/commits*",
            r#"[{"id": "c2", "title": "Two"}, {"id": "c1", "title": "One"}]"#,
            0,
        ),
    ];
    for (pattern, page) in page_rules.iter().zip(&pages) {
        rules.push((pattern, page, 0));
    }
    rules.push(("*/merge_requests/1", GITLAB_MR, 0));
    let fake = Fake::new("measure", &rules);
    // A new program's first start pays for the system's check of it.
    let _ = std::process::Command::new(fake.program()).output();
    let provider = gitlab(&fake.program());
    let remote = ForgeRemote::new(provider.kind, "gitlab.com", "group", "app");
    let mr = ForgePr {
        kind: provider.kind,
        host: Some("gitlab.com".into()),
        owner: "group".into(),
        name: "app".into(),
        number: 1,
    };
    // Warm: the plugin has loaded and run glab once.
    block_on(provider.checks(remote.clone(), "warm".into())).expect("warm");
    for run in 0..3 {
        let started = std::time::Instant::now();
        let prs =
            block_on(provider.pull_requests(remote.clone(), branches.clone())).expect("listed");
        let open: Vec<_> = prs.iter().filter(|pr| pr.state.is_open()).collect();
        let checks: Vec<_> = block_on(
            futures::stream::iter(
                open.iter()
                    .map(|pr| provider.checks(remote.clone(), pr.head_sha.clone())),
            )
            .buffered(4)
            .collect::<Vec<_>>(),
        );
        let badges = started.elapsed();
        assert!(checks.iter().all(Result::is_ok));

        let started = std::time::Instant::now();
        block_on(provider.view(mr.clone())).expect("the header");
        block_on(provider.commits(mr.clone())).expect("the commits");
        let header = started.elapsed();
        let first = std::sync::Mutex::new(None);
        let mut count = 0usize;
        block_on(provider.stream_diff(
            mr.clone(),
            CancelFlag::default(),
            Box::new(|_| {
                first.lock().unwrap().get_or_insert(started.elapsed());
                count += 1;
            }),
        ))
        .expect("the diff");
        let all = started.elapsed();
        assert_eq!(count, files);
        eprintln!(
            "MEASURE run {run}: badges for {} prs ({} checks) {badges:?}; tab header+commits {header:?}, first file {:?}, all {files} files {all:?}",
            prs.len(),
            checks.len(),
            first.lock().unwrap().unwrap(),
        );
    }
}
