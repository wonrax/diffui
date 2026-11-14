//! The bundled GitHub plugin (`plugins/bundled/github`) against a fake `gh`
//! (`core/tests/fixtures/fake-forge-cli.sh`): what it runs, what it makes of
//! the answers, and how the app claims and opens through it. Nothing here
//! reaches a real forge.

use std::time::{Duration, Instant};

use diffui_core::forge::{
    CheckState, CreatePr, ForgeAbility, ForgeError, ForgePr, ForgeProvider, ForgeRemote, PrState,
    ReviewDecision, UpdatePr,
};
use diffui_core::model::DiffFileStatus;
use diffui_core::repo::CancelFlag;

use super::forge::ScriptForge;
use super::forge::tests::{block_on, bundled_github, scratch_dir};
use crate::forge_dialog::tests::FakeCli;
use crate::harness::{Harness, answer_forge, fixture_jj, fixture_repo};

/// The plugin, its `gh` a fake answering `rules` (each with its exit code).
fn github(fake: &FakeCli) -> ScriptForge {
    bundled_github(&fake.program(), &scratch_dir("github"))
}

fn fake(name: &str, rules: &[(&str, &str, i32)]) -> FakeCli {
    let fake = FakeCli::new(name, &[]);
    fake.answer_or_fail(rules);
    fake
}

fn has(call: &[String], args: &[&str]) -> bool {
    call.windows(args.len())
        .any(|window| window.iter().zip(args).all(|(a, b)| a == b))
}

fn remote(github: &ScriptForge) -> ForgeRemote {
    github
        .claims("git@github.com:team/app.git")
        .expect("GitHub's")
}

/// Pull request `number` of `team/app` on `host`.
fn pr(github: &ScriptForge, host: &str, number: u64) -> ForgePr {
    github
        .claims(&format!("git@{host}:team/app.git"))
        .expect("GitHub's")
        .pull_request(number)
}

const GH_LIST: &str = r#"[
  {"number": 3, "url": "https://github.com/team/app/pull/3", "title": "Feature A, again",
   "headRefName": "feature-a", "baseRefName": "main", "headRefOid": "aaa3",
   "state": "OPEN", "isDraft": true, "reviewDecision": "", "isCrossRepository": false},
  {"number": 1, "url": "https://github.com/team/app/pull/1", "title": "Feature A",
   "headRefName": "feature-a", "baseRefName": "main", "headRefOid": "aaa1",
   "state": "CLOSED", "isDraft": false, "reviewDecision": "", "isCrossRepository": false},
  {"number": 2, "url": "https://github.com/team/app/pull/2", "title": "Feature B",
   "headRefName": "feature-b", "baseRefName": "feature-a", "headRefOid": "bbb2",
   "state": "MERGED", "isDraft": false, "reviewDecision": "APPROVED", "isCrossRepository": false},
  {"number": 4, "url": "https://github.com/team/app/pull/4", "title": "Someone's fork",
   "headRefName": "main", "baseRefName": "main", "headRefOid": "fff4",
   "state": "OPEN", "isDraft": false, "reviewDecision": "", "isCrossRepository": true}
]"#;

const GH_REST_PR: &str = r#"{"number": 7, "html_url": "https://github.com/team/app/pull/7",
  "title": "Add the thing", "state": "open", "draft": true, "merged_at": null,
  "head": {"ref": "thing", "sha": "abc"}, "base": {"ref": "main", "sha": "def"}}"#;

#[test]
fn github_claims_its_hosts_only() {
    let fake = fake("gh-claims", &[]);
    let github = github(&fake);
    let public = remote(&github);
    assert_eq!(public.kind.id(), "github");
    assert_eq!(public.kind.name(), "GitHub");
    assert_eq!(public.web_url, "https://github.com/team/app");
    assert!(github.claims("https://github.acme.com/team/app").is_some());
    let enterprise = github
        .claims("ssh://git@github.acme.com:2222/team/app")
        .expect("claimed");
    assert_eq!(enterprise.host, "github.acme.com");
    assert!(github.claims("git@gitlab.com:team/app.git").is_none());
    // GitHub projects have no subgroups.
    assert!(github.claims("git@github.com:team/sub/app.git").is_none());
}

#[test]
fn github_lists_one_pull_request_per_branch_from_its_own_repo() {
    let fake = fake("gh-list", &[("pr list *", GH_LIST, 0)]);
    let github = github(&fake);
    let prs = block_on(github.pull_requests(
        remote(&github),
        vec!["feature-a".into(), "feature-b".into(), "main".into()],
    ))
    .expect("listed");
    let summary: Vec<(String, u64, PrState)> = prs
        .iter()
        .map(|pr| (pr.head.clone(), pr.number, pr.state))
        .collect();
    assert_eq!(
        summary,
        [
            ("feature-a".to_owned(), 3, PrState::Draft),
            ("feature-b".to_owned(), 2, PrState::Merged),
        ],
        "the open PR beats an older closed one, and a fork's `main` isn't ours"
    );
    assert_eq!(prs[1].review, Some(ReviewDecision::Approved));
    assert_eq!(prs[1].base, "feature-a");
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "one call for every branch");
    assert!(has(&calls[0], &["HOST=github.com"]));
    assert!(has(&calls[0], &["--repo", "github.com/team/app"]));
    assert!(has(&calls[0], &["--state", "all"]));
}

#[test]
fn github_reads_the_check_rollup() {
    let fake = fake(
        "gh-checks",
        &[
            ("api graphql *sha=good*", "SUCCESS\n", 0),
            ("api graphql *sha=bad*", "ERROR\n", 0),
            ("api graphql *sha=busy*", "EXPECTED\n", 0),
            ("api graphql *", "null\n", 0),
        ],
    );
    let github = github(&fake);
    let state =
        |sha: &str| block_on(github.checks(remote(&github), sha.to_owned())).expect("checks");
    let good = state("good");
    assert_eq!(good.state, CheckState::Success);
    assert_eq!(
        good.url.as_deref(),
        Some("https://github.com/team/app/commit/good")
    );
    assert_eq!(state("bad").state, CheckState::Failure);
    assert_eq!(state("busy").state, CheckState::Pending);
    let none = state("unknown");
    assert_eq!((none.state, none.url), (CheckState::None, None));
    assert!(
        fake.calls()
            .iter()
            .all(|call| has(call, &["-f", "owner=team"]))
    );
}

#[test]
fn github_creates_and_updates_through_the_rest_api() {
    let fake = fake(
        "gh-write",
        &[
            ("api -X POST repos/team/app/pulls *", GH_REST_PR, 0),
            ("api -X PATCH repos/team/app/pulls/7 *", GH_REST_PR, 0),
        ],
    );
    let github = github(&fake);
    assert!(github.can(ForgeAbility::Create) && github.can(ForgeAbility::Update));
    let created = block_on(github.create_pull_request(
        remote(&github),
        CreatePr {
            head: "thing".into(),
            base: "main".into(),
            title: "Add the thing".into(),
            body: "Line one\nline two".into(),
            draft: true,
        },
    ))
    .expect("created");
    assert_eq!(created.number, 7);
    assert_eq!(created.state, PrState::Draft);
    assert_eq!(created.head_sha, "abc");
    block_on(github.update_pull_request(
        remote(&github),
        7,
        UpdatePr {
            base: Some("feature-a".into()),
            ..UpdatePr::default()
        },
    ))
    .expect("updated");
    let calls = fake.calls();
    assert!(has(&calls[0], &["-f", "head=thing"]));
    assert!(has(&calls[0], &["-f", "body=Line one\nline two"]));
    assert!(has(&calls[0], &["-F", "draft=true"]));
    assert!(has(&calls[1], &["-f", "base=feature-a"]));
    assert!(
        !calls[1].iter().any(|arg| arg.starts_with("title=")),
        "an update sends only what changes"
    );
}

#[test]
fn a_missing_or_signed_out_gh_is_a_typed_error() {
    let missing = bundled_github(
        std::path::Path::new("/nonexistent/diffui-test/gh"),
        &scratch_dir("gh-missing"),
    );
    let error = block_on(missing.pull_requests(remote(&missing), vec!["main".into()])).unwrap_err();
    assert_eq!(
        error,
        ForgeError::MissingCli {
            kind: missing.kind()
        }
    );
    assert!(error.to_string().contains("brew install gh"), "{error}");

    let fake = fake(
        "gh-signed-out",
        &[(
            "*",
            "To get started with GitHub CLI, please run:  gh auth login\n",
            4,
        )],
    );
    let github = github(&fake);
    let error = block_on(github.checks(remote(&github), "abc".into())).unwrap_err();
    assert_eq!(
        error,
        ForgeError::NotAuthenticated {
            kind: github.kind(),
            host: "github.com".into()
        }
    );
    assert!(
        error
            .to_string()
            .contains("gh auth login --hostname github.com")
    );
}

#[test]
fn a_pull_request_tab_reads_through_the_plugin() {
    let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    let fake = fake(
        "gh-tab",
        &[
            (
                "pr view 5 --repo github.acme.com/team/app --json number,title,*",
                "5\u{1f}Title\u{1f}me\u{1f}OPEN\u{1f}main\u{1f}topic\u{1f}1\u{1f}1\u{1f}1\u{1f}https://github.acme.com/team/app/pull/5\n",
                0,
            ),
            (
                "pr view 5 --repo github.acme.com/team/app --json commits *",
                "aaa1\u{1f}ada\u{1f}First\u{1f}2026-11-14T09:30:00Z\nbbb2\u{1f}\u{1f}Second\u{1f}\n",
                0,
            ),
            ("pr diff 5 *", diff, 0),
        ],
    );
    let github = github(&fake);
    assert!(github.can(ForgeAbility::Tab));
    let pr = pr(&github, "github.acme.com", 5);
    let info = block_on(github.view(pr.clone())).expect("viewed");
    assert_eq!(
        (
            info.title.as_str(),
            info.state.as_str(),
            info.author.as_str()
        ),
        ("Title", "open", "me")
    );
    assert_eq!(info.totals, Some((1, 1)));
    assert_eq!(info.url, "https://github.acme.com/team/app/pull/5");
    let commits = block_on(github.commits(pr.clone())).expect("the commits");
    let read: Vec<(&str, &str, &str, bool)> = commits
        .iter()
        .map(|commit| {
            (
                commit.oid.as_str(),
                commit.author.as_str(),
                commit.headline.as_str(),
                commit.authored_millis.is_some(),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            ("aaa1", "ada", "First", true),
            ("bbb2", "", "Second", false)
        ]
    );
    let mut files = Vec::new();
    block_on(github.stream_diff(
        pr,
        CancelFlag::default(),
        Box::new(|file| files.push(file.path)),
    ))
    .expect("streamed");
    assert_eq!(files, ["src/a.rs"]);
    assert!(
        fake.calls()
            .iter()
            .all(|call| has(call, &["HOST=github.acme.com"]))
    );
}

/// The commits API's files, as the files API's: a rename with spaces in its
/// paths keeps them, and a file without a patch keeps its row and counts.
#[test]
fn a_commits_files_lower_like_the_files_apis() {
    let commit = r#"{"stats": {"additions": 40, "deletions": 1},
      "commit": {"message": "Rename it\n\nWhy", "author": {"name": "Ada", "email": "ada@example.com", "date": "2026-11-14T09:30:00Z"}},
      "files": [
        {"filename": "src/new name.rs", "previous_filename": "src/old name.rs", "status": "renamed",
         "additions": 1, "deletions": 1, "patch": "@@ -10,2 +10,2 @@\n context\n-old line\n+new line"},
        {"filename": "big.bin", "status": "added", "additions": 0, "deletions": 0}
      ]}"#;
    let fake = fake(
        "gh-commit",
        &[(
            "api repos/team/app/commits/abc?per_page=100&page=1",
            commit,
            0,
        )],
    );
    let github = github(&fake);
    let (document, details) = block_on(github.commit_diff(
        pr(&github, "github.com", 5),
        "abc".into(),
        CancelFlag::default(),
    ))
    .expect("the commit");
    let file = &document.files[0];
    // Paths with spaces survive (no `diff --git` header round-trip).
    assert_eq!(file.path, "src/new name.rs");
    assert_eq!(file.old_path.as_deref(), Some("src/old name.rs"));
    assert_eq!(file.status, DiffFileStatus::Renamed);
    assert_eq!(file.hunks.len(), 1);
    assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
    assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
    assert_eq!((file.additions, file.deletions), (1, 1));
    // No patch (binary / oversized): the row survives with its counts.
    let binary = &document.files[1];
    assert_eq!(binary.status, DiffFileStatus::Added);
    assert_eq!(binary.hunks.len(), 1);
    assert!(binary.hunks[0].header.contains("diff unavailable"));
    // The commit's own stats, not the files' sum.
    assert_eq!(
        (document.total_additions, document.total_deletions),
        (40, 1)
    );
    let details = details.expect("its details");
    assert_eq!(details.description, "Rename it\n\nWhy");
    assert_eq!(details.author.email, "ada@example.com");
    assert_eq!(details.commit_id, "abc");
}

/// GitHub refuses the unified diff of a pull request with too many files:
/// the files API serves it instead, page by page. Any other failure is
/// the pull request's.
#[test]
fn a_diff_too_large_for_gh_comes_from_the_files_api() {
    let file = |n: usize| {
        format!(
            r#"{{"filename": "f{n:03}.rs", "status": "modified", "additions": 1, "deletions": 0, "patch": "@@ -1 +1,2 @@\n a\n+b"}}"#
        )
    };
    let page = |range: std::ops::Range<usize>| {
        format!("[{}]", range.map(file).collect::<Vec<_>>().join(","))
    };
    let (first, second) = (page(0..100), page(100..101));
    let fake = fake(
        "gh-too-large",
        &[
            (
                "pr diff 7 *",
                "HTTP 406: Sorry, the diff exceeded the maximum number of files (300).\n",
                1,
            ),
            (
                "api repos/team/app/pulls/7/files?per_page=100&page=1",
                &first,
                0,
            ),
            (
                "api repos/team/app/pulls/7/files?per_page=100&page=2",
                &second,
                0,
            ),
            ("pr diff 8 *", "HTTP 404: Not Found\n", 1),
        ],
    );
    let github = github(&fake);
    let mut files = Vec::new();
    block_on(github.stream_diff(
        pr(&github, "github.com", 7),
        CancelFlag::default(),
        Box::new(|file| files.push(file.path)),
    ))
    .expect("the files");
    assert_eq!(files.len(), 101);
    assert_eq!(
        (files[0].as_str(), files[100].as_str()),
        ("f000.rs", "f100.rs")
    );

    let error = block_on(github.stream_diff(
        pr(&github, "github.com", 8),
        CancelFlag::default(),
        Box::new(|_| {}),
    ))
    .unwrap_err();
    assert_eq!(
        error,
        ForgeError::NotFound {
            kind: github.kind(),
            host: "github.com".into(),
            what: "team/app#8".into()
        }
    );
    assert!(
        !fake.calls().iter().any(|call| has(
            call,
            &["api", "repos/team/app/pulls/8/files?per_page=100&page=1"]
        )),
        "only the too-large refusal reroutes"
    );
}

/// In the app: GitHub runs without asking, claims its hosts and the ones
/// gh's `hosts.yml` lists (read without running gh), and opens its links.
#[test]
fn the_bundled_github_claims_its_hosts() {
    let fake = FakeCli::new("gh-app", &[]);
    let config = format!(
        "[plugins.github]\nprogram = \"{}\"\n",
        fake.program().display()
    );
    let hosts =
        "github.com:\n    user: ada\n    git_protocol: ssh\n\"git.corp.example\":\n    user: ada\n";
    let mut h = Harness::open_prepared(None, (1280.0, 800.0), &config, "", |home| {
        let dir = home.join(".config/gh");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hosts.yml"), hosts).unwrap();
    });
    h.wait_until("gh's hosts", |ui| {
        ui.forge
            .forges
            .claim("git@git.corp.example:team/app.git")
            .is_some()
    });
    h.read(|ui| {
        let found = ui.plugins.get("github").expect("GitHub");
        assert_eq!(found.origin, super::Origin::Bundled);
        assert!(ui.plugins.reviews.is_empty(), "nothing asked");
        let claimed = |url: &str| ui.forge.forges.claim(url).map(|remote| remote.kind.id());
        for url in [
            "git@github.com:team/app.git",
            "https://github.acme.com/team/app",
            "git@git.corp.example:team/app.git",
        ] {
            assert_eq!(claimed(url), Some("github"), "{url}");
        }
        assert_ne!(claimed("git@gitlab.com:team/app.git"), Some("github"));
        let pr = ui
            .forge
            .forges
            .parse_pr("https://github.com/team/app/pull/3/files")
            .expect("a link");
        assert_eq!(
            (pr.kind.id(), pr.reference()),
            ("github", "team/app#3".into())
        );
        let pr = ui.forge.forges.parse_pr("team/app#4").expect("a reference");
        assert_eq!(pr.kind.id(), "github");
    });
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());

    // `diffui plugin eject github`: the TypeScript and what checks it.
    let folder = h
        .read(|ui| ui.plugins.dir.clone())
        .expect("a plugins folder");
    super::bundled::eject("github", &folder).expect("ejected");
    for file in ["plugin.toml", "main.ts", "diffui.d.ts", "tsconfig.json"] {
        assert!(folder.join("github").join(file).is_file(), "{file}");
    }
    drop(h);
    // Other tests share the home.
    let _ = std::fs::remove_dir_all(crate::harness::scratch_home().join(".config/gh"));
}

/// `repo` with `count` local bookmarks, `origin` on a GitHub project.
fn bookmarked_repo(count: usize) -> std::path::PathBuf {
    let repo = fixture_repo();
    let jj = |args: &[&str]| {
        let output = std::process::Command::new(fixture_jj())
            .arg("-R")
            .arg(&repo)
            .args(args)
            .output()
            .expect("run jj");
        assert!(
            output.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let names: Vec<String> = (1..=count).map(|n| format!("measure-{n:02}")).collect();
    let mut args = vec!["bookmark", "create", "-r", "@-"];
    args.extend(names.iter().map(String::as_str));
    jj(&args);
    let status = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo.join(".git"))
        .args([
            "config",
            "remote.origin.url",
            "https://github.invalid/team/app.git",
        ])
        .status()
        .expect("run git");
    assert!(status.success());
    repo
}

/// A unified diff of `files` files of `lines` added lines each.
fn big_diff(files: usize, lines: usize) -> String {
    let mut diff = String::new();
    for file in 0..files {
        let path = format!("src/file{file:04}.rs");
        diff.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n"
        ));
        for line in 0..lines {
            diff.push_str(&format!("+let value_{line} = {line};\n"));
        }
    }
    diff
}

/// Not a check: the numbers in the forge-github report. A refresh of 20
/// bookmarks' badges (one `pr list`, 20 check rollups), and a pull request
/// tab's 200-file, 20 000-line diff, through whatever answers for GitHub,
/// against the harness's fake `gh`. Run alone: it sets the fake's rules for
/// every harness.
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_github_badges_and_a_pull_requests_diff() {
    const BOOKMARKS: usize = 20;
    const RUNS: usize = 5;
    let rows: Vec<String> = (1..=BOOKMARKS)
        .map(|n| {
            format!(
                r#"{{"number": {n}, "url": "https://github.invalid/team/app/pull/{n}",
                "title": "t{n}", "headRefName": "measure-{n:02}", "baseRefName": "main",
                "headRefOid": "{n:040}", "state": "OPEN", "isDraft": false,
                "reviewDecision": "", "isCrossRepository": false}}"#
            )
        })
        .collect();
    let list = format!("[{}]", rows.join(","));
    let view = "1\u{1f}Big\u{1f}me\u{1f}OPEN\u{1f}main\u{1f}topic\u{1f}20000\u{1f}0\u{1f}200\u{1f}https://github.invalid/team/app/pull/1\n";
    let commits = format!(
        "{}\u{1f}me\u{1f}Big\u{1f}2026-11-14T09:30:00Z\n",
        "a".repeat(40)
    );
    let diff = big_diff(200, 100);
    answer_forge(&[
        ("pr list *", &list),
        ("api graphql *", "SUCCESS\n"),
        ("pr view 1 * --json commits *", &commits),
        ("pr view 1 *", view),
        ("pr diff 1 *", &diff),
    ]);

    let repo = bookmarked_repo(BOOKMARKS);
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    // Whatever loads at launch (a plugin) has loaded.
    h.settle();
    let mut badges = Vec::new();
    for _ in 0..RUNS {
        let started = Instant::now();
        h.act(|ui, cx| {
            let tab = ui.active_tab_id().expect("a tab");
            ui.active_mut().forge.decorations.clear();
            ui.refresh_pull_requests(tab, true, cx);
        });
        h.wait_until("the badges", |ui| {
            let forge = &ui.active().forge;
            forge.decorations.len() == BOOKMARKS
                && forge
                    .decorations
                    .values()
                    .all(|decoration| decoration.checks.is_some())
        });
        badges.push(started.elapsed());
    }

    let mut tabs = Vec::new();
    for _ in 0..RUNS {
        let pr = h
            .read(|ui| {
                ui.forge
                    .forges
                    .parse_pr("https://github.invalid/team/app/pull/1")
            })
            .expect("a pull request link");
        let started = Instant::now();
        h.act(|ui, cx| ui.open_pull_request(pr, cx));
        h.wait_until("the pull request's diff", |ui| {
            let session = &ui.active().session;
            !session.diff_in_flight() && session.document.files.len() == 200
        });
        tabs.push(started.elapsed());
        h.act(|ui, cx| {
            let tab = ui.active_tab_id().expect("the tab");
            ui.close_tab(tab, cx);
        });
        h.settle();
    }
    let show = |times: &mut Vec<Duration>| {
        times.sort();
        format!(
            "median {:?} (min {:?}, max {:?})",
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        )
    };
    eprintln!(
        "MEASURE badges for {BOOKMARKS} bookmarks: {}; pull request tab diff: {}",
        show(&mut badges),
        show(&mut tabs)
    );
}
