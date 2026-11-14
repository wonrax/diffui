//! The built-in forge providers against fake `glab` programs
//! (`tests/fixtures/fake-forge-cli.sh`): what they run, and what they make of
//! the answers. Nothing here reaches a real forge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diffui_core::forge::gitlab::GitLab;
use diffui_core::forge::{
    CheckState, CreatePr, ForgeError, ForgeKind, ForgePr, ForgeProvider, ForgeRemote, PrState,
    UpdatePr,
};
use diffui_core::repo::CancelFlag;

/// A fake CLI in its own directory: rules in, recorded calls out.
struct Fake {
    dir: PathBuf,
}

impl Fake {
    /// `name` is unique per test: the tests run concurrently.
    fn new(name: &str, rules: &[(&str, &str, i32)]) -> Self {
        let dir = std::env::temp_dir().join(format!("diffui-forge-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-forge-cli.sh");
        std::fs::copy(script, dir.join("cli")).unwrap();
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
        self.dir.join("cli")
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

fn has(call: &[String], args: &[&str]) -> bool {
    call.windows(args.len())
        .any(|window| window.iter().zip(args).all(|(a, b)| a == b))
}

fn gitlab_remote() -> ForgeRemote {
    ForgeRemote::new(ForgeKind::GitLab, "gitlab.acme.org", "group/sub", "app")
}

#[tokio::test]
async fn a_missing_or_signed_out_cli_is_a_typed_error() {
    let fake = Fake::new("glab-signed-out", &[("*", "ERROR: 401 Unauthorized\n", 1)]);
    let error = GitLab::new(fake.program())
        .pull_requests(gitlab_remote(), vec!["main".into()])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ForgeError::NotAuthenticated {
            kind: ForgeKind::GitLab,
            ..
        }
    ));
    assert!(
        error
            .to_string()
            .contains("glab auth login --hostname gitlab.acme.org")
    );
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
   "state": "opened", "source_project_id": 9, "target_project_id": 5}
]"#;

const GITLAB_MR: &str = r#"{"iid": 13, "web_url": "https://gitlab.acme.org/group/sub/app/-/merge_requests/13",
  "title": "Draft: New", "source_branch": "new", "target_branch": "tidy", "sha": "n13",
  "state": "opened", "draft": true, "source_project_id": 5, "target_project_id": 5,
  "author": {"username": "me"}, "changes_count": "2"}"#;

#[tokio::test]
async fn gitlab_lists_creates_and_reads_pipelines() {
    let fake = Fake::new(
        "glab-all",
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
                "api --hostname gitlab.acme.org projects/group%2Fsub%2Fapp/repository/commits/*",
                r#"{"id": "x", "title": "x"}"#,
                0,
            ),
        ],
    );
    let gitlab = GitLab::new(fake.program());
    let prs = gitlab
        .pull_requests(gitlab_remote(), vec!["tidy".into(), "ship".into()])
        .await
        .expect("listed");
    let summary: Vec<(u64, PrState)> = prs.iter().map(|pr| (pr.number, pr.state)).collect();
    assert_eq!(summary, [(12, PrState::Draft), (11, PrState::Merged)]);

    let created = gitlab
        .create_pull_request(
            gitlab_remote(),
            CreatePr {
                head: "new".into(),
                base: "tidy".into(),
                title: "New".into(),
                body: "Body".into(),
                draft: true,
            },
        )
        .await
        .expect("created");
    assert_eq!((created.number, created.state), (13, PrState::Draft));
    gitlab
        .update_pull_request(
            gitlab_remote(),
            13,
            UpdatePr {
                base: Some("main".into()),
                ..UpdatePr::default()
            },
        )
        .await
        .expect("updated");

    let running = gitlab
        .checks(gitlab_remote(), "t12".into())
        .await
        .expect("checks");
    assert_eq!(running.state, CheckState::Pending);
    assert_eq!(running.url.as_deref(), Some("https://gitlab.acme.org/p/1"));
    let none = gitlab
        .checks(gitlab_remote(), "other".into())
        .await
        .expect("checks");
    assert_eq!(none.state, CheckState::None);

    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .all(|call| has(call, &["HOST=gitlab.acme.org"]))
    );
    assert!(
        has(&calls[1], &["-f", "title=Draft: New"]),
        "a draft is a title prefix"
    );
    assert!(has(&calls[1], &["-f", "source_branch=new"]));
    assert!(has(&calls[2], &["-f", "target_branch=main"]));
}

#[tokio::test]
async fn pull_request_tabs_read_through_the_provider() {
    let mr_diffs = r#"[{"old_path": "a", "new_path": "a", "diff": "@@ -1 +1 @@\n-x\n+y\n"}]"#;
    let fake = Fake::new(
        "glab-tab",
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
    let gitlab: Arc<dyn ForgeProvider> = Arc::new(GitLab::new(fake.program()));
    let mr = ForgePr::parse("group/app!3").expect("an MR");
    let info = gitlab.view(mr.clone()).await.expect("viewed");
    assert_eq!((info.state.as_str(), info.changed_files), ("draft", 2));
    let mut files = Vec::new();
    gitlab
        .stream_diff(
            mr,
            CancelFlag::default(),
            Box::new(|file| files.push(file.path)),
        )
        .await
        .expect("streamed");
    assert_eq!(files, ["a"]);
}
