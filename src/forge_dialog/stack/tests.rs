use super::*;

/// A preview on `tab` still reading its stack, as Submit stack opens it,
/// without asking the repository: its serial.
pub(crate) fn open_preview(ui: &mut Diffui, tab: TabId) -> u64 {
    let github = crate::plugins::bundled_kind("github");
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    ui.push_mode(Mode::StackSubmit(StackDialog {
        tab,
        serial,
        remote_name: "origin".to_owned(),
        remote: ForgeRemote::new(github, "github.com", "team", "app"),
        revision: RevisionSelection::WorkingCopy,
        rows: Vec::new(),
        expanded: None,
        focused: Cell::new(None),
        reading: Reading {
            stack: true,
            ..Reading::default()
        },
        problem: None,
        run: None,
        outcome: None,
        carried: HashMap::new(),
    }));
    serial
}

#[test]
fn stat_totals_read_the_summary_line() {
    assert_eq!(
        stat_totals("a.txt | 3 ++-\n1 file changed, 2 insertions(+), 1 deletion(-)\n"),
        Some((2, 1))
    );
    assert_eq!(
        stat_totals("0 files changed, 0 insertions(+), 0 deletions(-)\n"),
        Some((0, 0))
    );
    assert_eq!(stat_totals(""), None);
}

// ── Against the fixture, a fake `gh` and the bundled GitHub plugin ──────────

use super::super::tests::{FakeCli, finished, has, listed, rest_pr, stacked_repo};
use crate::activity::ActivityStatus;
use crate::harness::Harness;

fn stack(action: StackAction) -> Action {
    Action::Forge(super::super::ForgeAction::Stack(action))
}

/// Open the preview of the stack ending at `top` and wait until it's read.
fn preview(h: &mut Harness, top: &str) {
    h.perform(Action::Forge(super::super::ForgeAction::SubmitStack(
        CommandArg::Revision(RevisionSelection::Commit(top.to_owned())),
    )));
    h.wait_until("the stack read", |ui| {
        ui.stack_dialog()
            .is_some_and(|dialog| dialog.reading.done())
    });
    h.settle();
}

/// The preview's rows as `(bookmark, base, tag)`, and why Confirm can't run.
fn rows(h: &mut Harness) -> (Vec<(String, String, Plan)>, Option<&'static str>) {
    h.read(|ui| {
        let dialog = ui.stack_dialog().expect("the preview");
        (
            dialog
                .rows
                .iter()
                .map(|row| (row.bookmark.clone(), row.base.clone(), row.plan()))
                .collect(),
            dialog.blocked(),
        )
    })
}

/// Confirm, then wait for the submission to end.
fn confirm(h: &mut Harness) -> Option<ActivityStatus> {
    h.perform(stack(StackAction::Confirm));
    let activity = h
        .read(|ui| ui.stack_dialog().and_then(|dialog| dialog.run))
        .expect("a submission");
    finished(h, activity)
}

/// The bookmarks the fixture's bare remote has.
fn remote_refs(h: &mut Harness) -> String {
    let repo = h.read(|ui| {
        ui.active()
            .repository
            .as_ref()
            .expect("a repository")
            .root
            .clone()
    });
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo.join(".jj/fixture-remote.git"))
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .expect("run git");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn writes(calls: &[Vec<String>]) -> Vec<&Vec<String>> {
    calls
        .iter()
        .filter(|call| has(call, &["POST"]) || has(call, &["PATCH"]))
        .collect()
}

/// Submit stack opens the preview and only reads: the forge is asked which
/// pull requests exist, nothing is pushed, created or updated. Cancel
/// leaves it all as it was; Esc too.
#[test]
fn the_preview_sends_nothing_until_confirm() {
    let fake = FakeCli::new(
        "stack-preview",
        &[("pr list *", "[]"), ("api graphql *", "SUCCESS\n")],
    );
    let (mut h, top) = stacked_repo(&fake, "");
    let before = remote_refs(&mut h);
    fake.answer(&[("pr list *", "[]"), ("api graphql *", "SUCCESS\n")]);
    preview(&mut h, &top);

    let (found, blocked) = rows(&mut h);
    assert_eq!(
        found,
        [
            ("stack-1".to_owned(), "main".to_owned(), Plan::Create),
            ("stack-2".to_owned(), "stack-1".to_owned(), Plan::Create),
        ]
    );
    assert_eq!(blocked, None, "ready to confirm");
    h.read(|ui| {
        let dialog = ui.stack_dialog().unwrap();
        let first = &dialog.rows[0];
        assert_eq!(first.title, "Stack one");
        assert_eq!(first.body, "The first body.");
        assert_eq!(first.push, Push::New);
        assert_eq!(
            first.stats,
            Some(Stats {
                commits: 1,
                added: 1,
                removed: 0
            })
        );
        assert_eq!(dialog.pushes().0, ["stack-1", "stack-2"]);
        assert!(ui.active().forge.submission.is_none());
    });
    let calls = fake.calls();
    assert!(writes(&calls).is_empty(), "{calls:?}");
    assert!(calls.iter().any(|call| has(call, &["pr", "list"])));
    assert_eq!(remote_refs(&mut h), before, "nothing pushed");

    h.perform(stack(StackAction::Close));
    h.settle();
    assert!(h.read(|ui| ui.stack_dialog().is_none()));
    // Esc from the keyboard does the same.
    preview(&mut h, &top);
    h.press("escape");
    h.settle();
    assert!(h.read(|ui| ui.stack_dialog().is_none()));
    let calls = fake.calls();
    assert!(writes(&calls).is_empty(), "{calls:?}");
    assert_eq!(remote_refs(&mut h), before, "nothing pushed");
    h.read(|ui| {
        assert!(ui.active().forge.submission.is_none());
        let info = &ui.active().session.bookmarks;
        assert!(!super::super::in_sync(info, "stack-1", "origin"));
    });
}

/// ⌘⏎ confirms: both bookmarks go in one push and a pull request opens for
/// each, the second based on the first, the rows saying so. Read again,
/// both are up to date and there's nothing to confirm; a base moved on the
/// forge is moved back, and nothing else is sent.
#[test]
fn submit_stack_chains_bases_and_is_idempotent() {
    let fake = FakeCli::new(
        "submit-stack",
        &[
            ("pr list *", "[]"),
            (
                "api -X POST repos/team/app/pulls *head=stack-1 *",
                &rest_pr(1, "stack-1", "main"),
            ),
            (
                "api -X POST repos/team/app/pulls *head=stack-2 *",
                &rest_pr(2, "stack-2", "stack-1"),
            ),
            ("api graphql *", "SUCCESS\n"),
        ],
    );
    let (mut h, top) = stacked_repo(&fake, "");
    preview(&mut h, &top);
    h.press("cmd-enter");
    let activity = h
        .read(|ui| ui.stack_dialog().and_then(|dialog| dialog.run))
        .expect("⌘⏎ confirmed");
    assert_eq!(finished(&mut h, activity), Some(ActivityStatus::Done));
    let calls = fake.calls();
    let posts: Vec<&Vec<String>> = calls.iter().filter(|call| has(call, &["POST"])).collect();
    assert_eq!(posts.len(), 2, "{calls:?}");
    assert!(
        has(posts[0], &["-f", "head=stack-1"]) && has(posts[0], &["-f", "base=main"]),
        "{calls:?}"
    );
    assert!(has(posts[0], &["-f", "title=Stack one"]));
    assert!(has(posts[0], &["-f", "body=The first body."]));
    assert!(has(posts[1], &["-f", "head=stack-2"]) && has(posts[1], &["-f", "base=stack-1"]));
    h.read(|ui| {
        let dialog = ui
            .stack_dialog()
            .expect("the dialog stays to show how it went");
        let progress: Vec<&Progress> = dialog.rows.iter().map(|row| &row.progress).collect();
        assert_eq!(
            progress,
            [
                &Progress::Done("Opened #1".to_owned()),
                &Progress::Done("Opened #2".to_owned())
            ]
        );
        assert_eq!(
            dialog.outcome,
            Some(Ok("Opened 2 pull requests".to_owned()))
        );
        let info = &ui.active().session.bookmarks;
        assert!(super::super::in_sync(info, "stack-1", "origin"));
        assert!(super::super::in_sync(info, "stack-2", "origin"));
    });
    h.perform(stack(StackAction::Close));

    // Again, with both open as asked: nothing to push, nothing to change.
    fake.answer(&[
        (
            "pr list *",
            &listed(&[(1, "stack-1", "main"), (2, "stack-2", "stack-1")]),
        ),
        ("api graphql *", "SUCCESS\n"),
    ]);
    preview(&mut h, &top);
    let (found, blocked) = rows(&mut h);
    assert!(
        found.iter().all(|(_, _, plan)| *plan == Plan::UpToDate),
        "{found:?}"
    );
    assert_eq!(blocked, Some("Everything is up to date"));
    h.press("cmd-enter");
    h.settle();
    assert!(h.read(|ui| ui.stack_dialog().is_some_and(|dialog| dialog.run.is_none())));
    h.perform(stack(StackAction::Close));

    // Someone retargeted the second onto main: only its base moves back.
    fake.answer(&[
        (
            "pr list *",
            &listed(&[(1, "stack-1", "main"), (2, "stack-2", "main")]),
        ),
        (
            "api -X PATCH repos/team/app/pulls/2 *",
            &rest_pr(2, "stack-2", "stack-1"),
        ),
        ("api graphql *", "SUCCESS\n"),
    ]);
    preview(&mut h, &top);
    let (found, _) = rows(&mut h);
    assert_eq!(found[0].2, Plan::UpToDate);
    assert_eq!(
        found[1].2,
        Plan::Update {
            base: true,
            commits: false,
            text: false
        }
    );
    assert_eq!(confirm(&mut h), Some(ActivityStatus::Done));
    let patches: Vec<Vec<String>> = fake
        .calls()
        .into_iter()
        .filter(|call| has(call, &["PATCH"]))
        .collect();
    assert_eq!(patches.len(), 1);
    assert!(has(&patches[0], &["-f", "base=stack-1"]));
    assert!(
        !patches[0]
            .iter()
            .any(|arg| arg.starts_with("body=") || arg.starts_with("title=")),
        "unchanged fields aren't sent: {:?}",
        patches[0]
    );
    h.read(|ui| {
        let progress: Vec<Progress> = ui
            .stack_dialog()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.progress.clone())
            .collect();
        assert_eq!(
            progress,
            [Progress::Skipped, Progress::Done("Updated #2".to_owned())]
        );
    });
}

/// A title and description edited in the preview reach the forge: an
/// update for the open pull request (its unchanged base left out), a create
/// for the new one. Tab walks the fields, opening the next row.
#[test]
fn edited_titles_and_descriptions_reach_create_and_update() {
    let fake = FakeCli::new(
        "stack-edits",
        &[
            ("pr list *", &listed(&[(1, "stack-1", "main")])),
            (
                "api -X PATCH repos/team/app/pulls/1 *",
                &rest_pr(1, "stack-1", "main"),
            ),
            (
                "api -X POST repos/team/app/pulls *head=stack-2 *",
                &rest_pr(2, "stack-2", "stack-1"),
            ),
            ("api graphql *", "SUCCESS\n"),
        ],
    );
    let (mut h, top) = stacked_repo(&fake, "");
    preview(&mut h, &top);
    h.read(|ui| {
        let dialog = ui.stack_dialog().unwrap();
        // An open pull request keeps its own title, and its description
        // isn't sent unless one is written.
        assert_eq!(dialog.rows[0].title, "t");
        assert_eq!(dialog.rows[0].body, "");
        // Its bookmark isn't on the remote yet: it gets commits.
        assert_eq!(
            dialog.rows[0].plan(),
            Plan::Update {
                base: false,
                commits: true,
                text: false
            }
        );
    });

    // Tab opens the first row at its title, then its description, then the
    // next row's title.
    h.press("tab");
    h.settle();
    assert_eq!(h.read(|ui| ui.stack_dialog().unwrap().expanded), Some(0));
    assert_eq!(
        h.read(|ui| ui.stack_dialog().unwrap().focused.get()),
        Some(Field::Title)
    );
    h.press("tab");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.stack_dialog().unwrap().focused.get()),
        Some(Field::Body)
    );
    h.press("tab");
    h.settle();
    assert_eq!(h.read(|ui| ui.stack_dialog().unwrap().expanded), Some(1));
    h.press("shift-tab");
    h.settle();
    assert_eq!(h.read(|ui| ui.stack_dialog().unwrap().expanded), Some(0));

    h.perform(stack(StackAction::Title(
        "Teach the stack a title".to_owned(),
    )));
    h.perform(stack(StackAction::Body("Written here.".to_owned())));
    h.perform(stack(StackAction::Expand(1)));
    h.perform(stack(StackAction::Title("The second, edited".to_owned())));
    h.perform(stack(StackAction::Body("Its own words.".to_owned())));
    h.perform(stack(StackAction::Draft(1, true)));
    assert_eq!(confirm(&mut h), Some(ActivityStatus::Done));

    let calls = fake.calls();
    let patch = calls
        .iter()
        .find(|call| has(call, &["PATCH"]))
        .expect("an update");
    assert!(
        has(patch, &["-f", "title=Teach the stack a title"]),
        "{calls:?}"
    );
    assert!(has(patch, &["-f", "body=Written here."]));
    assert!(
        !patch.iter().any(|arg| arg.starts_with("base=")),
        "the base didn't change: {patch:?}"
    );
    let post = calls
        .iter()
        .find(|call| has(call, &["POST"]))
        .expect("a create");
    assert!(has(post, &["-f", "title=The second, edited"]));
    assert!(has(post, &["-f", "body=Its own words."]));
    assert!(has(post, &["-F", "draft=true"]));
}

/// The second create fails: the first stays made, the failed row says why,
/// and nothing after it is sent. Try again reads the stack again, finds the
/// first open and up to date, keeps the failed row's edited title, and
/// creates only the second.
#[test]
fn a_failure_midway_stops_and_a_retry_picks_up_where_it_stopped() {
    let fake = FakeCli::new("stack-failure", &[]);
    fake.answer_or_fail(&[
        ("pr list *", "[]", 0),
        (
            "api -X POST repos/team/app/pulls *head=stack-1 *",
            &rest_pr(1, "stack-1", "main"),
            0,
        ),
        (
            "api -X POST repos/team/app/pulls *head=stack-2 *",
            "HTTP 422: Validation Failed\n",
            1,
        ),
        ("api graphql *", "SUCCESS\n", 0),
    ]);
    let (mut h, top) = stacked_repo(&fake, "");
    preview(&mut h, &top);
    h.perform(stack(StackAction::Expand(1)));
    h.perform(stack(StackAction::Title(
        "Kept across the retry".to_owned(),
    )));
    assert_eq!(confirm(&mut h), Some(ActivityStatus::Error));
    h.read(|ui| {
        let dialog = ui.stack_dialog().unwrap();
        assert_eq!(
            dialog.rows[0].progress,
            Progress::Done("Opened #1".to_owned())
        );
        assert!(
            matches!(&dialog.rows[1].progress, Progress::Failed(why) if why.contains("stack-2")),
            "{:?}",
            dialog.rows[1].progress
        );
        assert!(matches!(dialog.outcome, Some(Err(_))));
    });

    fake.answer(&[
        ("pr list *", &listed(&[(1, "stack-1", "main")])),
        (
            "api -X POST repos/team/app/pulls *head=stack-2 *",
            &rest_pr(2, "stack-2", "stack-1"),
        ),
        ("api graphql *", "SUCCESS\n"),
    ]);
    h.perform(stack(StackAction::Retry));
    h.wait_until("the stack read again", |ui| {
        ui.stack_dialog()
            .is_some_and(|dialog| dialog.run.is_none() && dialog.reading.done())
    });
    h.settle();
    h.read(|ui| {
        let dialog = ui.stack_dialog().unwrap();
        assert_eq!(dialog.rows[0].plan(), Plan::UpToDate);
        assert_eq!(dialog.rows[1].title, "Kept across the retry");
    });
    assert_eq!(confirm(&mut h), Some(ActivityStatus::Done));
    let calls = fake.calls();
    let posts: Vec<&Vec<String>> = calls.iter().filter(|call| has(call, &["POST"])).collect();
    assert_eq!(posts.len(), 1, "{calls:?}");
    assert!(has(posts[0], &["-f", "head=stack-2"]));
    assert!(has(posts[0], &["-f", "title=Kept across the retry"]));
    assert!(!calls.iter().any(|call| has(call, &["PATCH"])), "{calls:?}");
}

/// Generate on a row runs `ai.pr-description` for that row's commit
/// through the chosen provider (a stand-in `claude`) and fills that row
/// alone.
#[test]
fn generate_fills_a_row_through_the_provider() {
    let fake = FakeCli::new(
        "stack-generate",
        &[("pr list *", "[]"), ("api graphql *", "SUCCESS\n")],
    );
    let claude = crate::ai_scenarios::fake_claude();
    let (mut h, top) = stacked_repo(
        &fake,
        &format!(
            "\n[plugins.claude-code]\nprogram = {:?}\n\n[ai]\nprovider = \"claude-code\"\n",
            claude.display().to_string()
        ),
    );
    h.wait_until("the provider", |ui| {
        ui.ai_answers("claude-code", "complete")
    });
    preview(&mut h, &top);
    h.perform(stack(StackAction::Expand(1)));
    h.settle();
    h.act(|ui, cx| {
        ui.run_command(
            "ai.pr-description",
            CommandArg::Revision(RevisionSelection::Commit(top.clone())),
            cx,
        )
    });
    h.wait_until("the row written", |ui| {
        ui.stack_dialog().is_some_and(|dialog| {
            dialog.rows[1]
                .generating
                .as_ref()
                .is_some_and(|generation| !generation.running)
        })
    });
    h.read(|ui| {
        assert!(ui.pull_request_dialog().is_none(), "not the single dialog");
        let dialog = ui.stack_dialog().unwrap();
        assert_eq!(dialog.rows[1].title, "Teach the parser empty files");
        assert_eq!(
            dialog.rows[1].body,
            "The parser no longer panics.\n\n- Adds a test"
        );
        assert_eq!(
            dialog.rows[0].title, "Stack one",
            "the other row is left alone"
        );
    });
    let stdin = std::fs::read_to_string(claude.with_extension("stdin")).unwrap();
    assert!(stdin.contains("Stack two"), "{stdin}");
    assert!(writes(&fake.calls()).is_empty());
}

/// After a submission, rewording the bottom commit rewrites both bookmarks'
/// history: the preview says it force-pushes them, and calls them updates
/// with new commits.
#[test]
fn a_rewritten_stack_is_previewed_as_force_pushed() {
    let fake = FakeCli::new(
        "stack-force",
        &[
            ("pr list *", "[]"),
            (
                "api -X POST repos/team/app/pulls *head=stack-1 *",
                &rest_pr(1, "stack-1", "main"),
            ),
            (
                "api -X POST repos/team/app/pulls *head=stack-2 *",
                &rest_pr(2, "stack-2", "stack-1"),
            ),
            ("api graphql *", "SUCCESS\n"),
        ],
    );
    let (mut h, top) = stacked_repo(&fake, "");
    preview(&mut h, &top);
    assert_eq!(confirm(&mut h), Some(ActivityStatus::Done));
    h.perform(stack(StackAction::Close));

    let repo = h.read(|ui| ui.active().repository.as_ref().unwrap().root.clone());
    let status = std::process::Command::new(crate::harness::fixture_jj())
        .arg("-R")
        .arg(&repo)
        .args(["describe", "stack-1", "-m", "Stack one, reworded"])
        .status()
        .expect("run jj");
    assert!(status.success());
    h.perform(Action::Refresh);
    h.wait_until("the rewritten stack", |ui| {
        !super::super::in_sync(&ui.active().session.bookmarks, "stack-1", "origin")
    });
    let top = h.read(|ui| {
        ui.active()
            .session
            .bookmarks
            .entry("stack-2")
            .and_then(|entry| entry.local_target())
            .unwrap()
            .to_owned()
    });
    fake.answer(&[
        (
            "pr list *",
            &listed(&[(1, "stack-1", "main"), (2, "stack-2", "stack-1")]),
        ),
        ("api graphql *", "SUCCESS\n"),
    ]);
    preview(&mut h, &top);
    h.read(|ui| {
        let dialog = ui.stack_dialog().unwrap();
        assert!(
            dialog.rows.iter().all(|row| row.push == Push::Force),
            "{:?}",
            dialog.rows.iter().map(|row| row.push).collect::<Vec<_>>()
        );
        assert_eq!(dialog.pushes().1, ["stack-1", "stack-2"]);
        assert_eq!(
            dialog.rows[0].plan(),
            Plan::Update {
                base: false,
                commits: true,
                text: false
            }
        );
    });
    assert!(writes(&fake.calls()).is_empty());
}

/// What a scene shows of the preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scene {
    /// Read: a new row, an update and an up-to-date one.
    Preview,
    /// The update's row open for editing.
    Editing,
    /// Confirmed: the first opened, the second under way, the third skipped.
    Running,
}

/// The preview on the active tab as `scene` shows it, built rather than
/// read, so the scene doesn't depend on the forge or the jj CLI.
pub(crate) fn show(ui: &mut Diffui, scene: Scene) {
    use diffui_core::forge::PrState;
    let tab = ui.active_tab_id().expect("a tab");
    let (remote_name, remote) = ui.active().forge.home.clone().expect("a forge");
    let pr = |number, head: &str, base: &str, title: &str| PullRequest {
        number,
        url: format!("{}/pull/{number}", remote.web_url),
        title: title.to_owned(),
        head: head.to_owned(),
        base: base.to_owned(),
        head_sha: String::new(),
        state: PrState::Open,
        review: None,
    };
    let row = |bookmark: &str, base: &str, title: &str, body: &str| StackRow {
        bookmark: bookmark.to_owned(),
        commit: String::new(),
        base: base.to_owned(),
        title: title.to_owned(),
        body: body.to_owned(),
        draft: false,
        existing: None,
        push: Push::New,
        stats: None,
        progress: Progress::Idle,
        generating: None,
        described: None,
    };
    let long = "feature/render-pipeline-with-a-bookmark-name-long-enough-to-ellipsise";
    let mut rows = vec![
        StackRow {
            stats: Some(Stats {
                commits: 3,
                added: 182,
                removed: 41,
            }),
            ..row(
                "fix/parser-crash",
                "main",
                "Teach the parser empty files",
                "The parser no longer panics on an empty file.",
            )
        },
        StackRow {
            existing: Some(pr(41, long, "main", "Rework the render pipeline")),
            push: Push::Force,
            stats: Some(Stats {
                commits: 2,
                added: 64,
                removed: 12,
            }),
            ..row(long, "fix/parser-crash", "Rework the render pipeline", "")
        },
        StackRow {
            existing: Some(pr(
                42,
                "fix/changelog-note",
                long,
                "Note the release in the changelog",
            )),
            push: Push::InSync,
            stats: Some(Stats {
                commits: 1,
                added: 3,
                removed: 0,
            }),
            ..row(
                "fix/changelog-note",
                long,
                "Note the release in the changelog",
                "",
            )
        },
    ];
    let mut expanded = None;
    let mut run = None;
    match scene {
        Scene::Preview => rows[0].draft = true,
        Scene::Editing => {
            expanded = Some(1);
            rows[1].title = "Rework the render pipeline around passes".to_owned();
            rows[1].body = "Each pass owns its targets now.\n\n- The blur pass reads the scene \
                            once\n- Resizes rebuild only what changed"
                .to_owned();
        }
        Scene::Running => {
            let (activity, _) = ui.begin_activity(tab, "Submit stack", false);
            let mut submission =
                Submission::new(activity, remote_name.clone(), remote.clone(), Vec::new());
            // Under way, so nothing here moves it on.
            submission.running = true;
            ui.active_mut().forge.submission = Some(submission);
            run = Some(activity);
            rows[0].progress = Progress::Done("Opened #43".to_owned());
            rows[1].progress = Progress::Running;
            rows[2].progress = Progress::Skipped;
        }
    }
    ui.push_mode(Mode::StackSubmit(StackDialog {
        tab,
        serial: SERIAL.fetch_add(1, Ordering::Relaxed),
        remote_name,
        remote,
        revision: RevisionSelection::WorkingCopy,
        rows,
        expanded,
        focused: Cell::new(None),
        reading: Reading::default(),
        problem: None,
        run,
        outcome: None,
        carried: HashMap::new(),
    }));
}
