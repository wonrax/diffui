//! A pull request tab end to end, through the bundled GitHub plugin and the
//! harness's fake `gh`: the outline in place of a graph, the scope and its
//! steps, the header, and the layout of a tab with no steps.

use diffui_core::RevisionSelection;

use super::{ReviewAction, Scope, Steps, TextKind};
use crate::Action;
use crate::harness::{Harness, answer_forge};

/// The pull request's description: what the header renders.
pub(crate) const BODY: &str = "Frames now reach the screen through one pipeline instead of three, so a resize no longer drops the frame in flight.\n\n## What changes\n\n- `Pipeline::next` hands out frames in order.\n- The **compositor** waits on the pipeline, not on the window.\n- Old entry points stay as thin wrappers for a release.\n\n```rust\nlet frame = pipeline.next()?;\ncompositor.present(frame);\n```\n\nSee [the design notes](https://example.invalid/render-notes) for the measurements.";

/// The pull request's commits, oldest first: `(oid, author, message,
/// added, removed)`.
const COMMITS: [(&str, &str, &str, u64, u64); 3] = [
    (
        "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
        "ada",
        "Add the frame pipeline\n\nOne queue the renderer pushes to and the compositor\npulls from. Nothing uses it yet.",
        24,
        0,
    ),
    (
        "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
        "ada",
        "Route the compositor through the pipeline",
        9,
        6,
    ),
    (
        "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
        "lin",
        "Keep the old entry points as wrappers",
        5,
        2,
    ),
];

const FILES: [(&str, &str); 3] = [
    (
        "src/pipeline.rs",
        "@@ -0,0 +1,6 @@\n+pub struct Pipeline {\n+    queue: VecDeque<Frame>,\n+}\n+\n+impl Pipeline {\n+    pub fn next(&mut self) -> Option<Frame> { self.queue.pop_front() }\n",
    ),
    (
        "src/compositor.rs",
        "@@ -1,4 +1,4 @@\n use crate::window::Window;\n-pub fn present(window: &Window) {\n-    window.swap();\n+pub fn present(frame: Frame) {\n+    frame.swap();\n }\n",
    ),
    (
        "src/lib.rs",
        "@@ -1,2 +1,3 @@\n pub mod compositor;\n+pub mod pipeline;\n pub mod window;\n",
    ),
];

/// A unified diff of `files`.
fn unified(files: &[(&str, &str)]) -> String {
    files
        .iter()
        .map(|(path, patch)| {
            format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{patch}")
        })
        .collect()
}

/// What `gh` answers about acme/render-toolkit#41: `body` as its
/// description, its first `commits` commits (each one's diff touching one
/// file).
pub(crate) fn answer_pull_request(body: &str, commits: usize) {
    let view = format!(
        "41\u{1f}Rework the render pipeline\u{1f}ada\u{1f}OPEN\u{1f}main\u{1f}feature/render-pipeline\u{1f}38\u{1f}8\u{1f}3\u{1f}https://github.com/acme/render-toolkit/pull/41\u{1f}{body}\n"
    );
    let listed: Vec<crate::plugins::github_scenarios::GhCommit<'_>> = COMMITS[..commits]
        .iter()
        .map(|(oid, author, message, added, removed)| {
            (
                *oid,
                *author,
                *message,
                Some("2026-11-12T09:30:00Z"),
                *added,
                *removed,
            )
        })
        .collect();
    let listed = crate::plugins::github_scenarios::gh_commits(&listed);
    let commit_diffs: Vec<(String, String)> = COMMITS
        .iter()
        .zip(FILES)
        .map(|((oid, author, message, added, removed), (path, patch))| {
            let answer = serde_json::json!({
                "files": [{ "filename": path, "status": "modified", "additions": added,
                            "deletions": removed, "patch": patch }],
                "commit": { "message": message,
                            "author": { "name": author, "email": format!("{author}@example.com"),
                                        "date": "2026-11-12T09:30:00Z" } },
                "stats": { "additions": added, "deletions": removed },
            });
            (
                format!("api repos/acme/render-toolkit/commits/{oid}*"),
                answer.to_string(),
            )
        })
        .collect();
    let diff = unified(&FILES);
    let mut rules: Vec<(&str, &str)> = vec![
        ("pr view 41 *", &view),
        ("api graphql *commits(first:100*", &listed),
        ("pr diff 41 *", &diff),
        ("api graphql *", "{\"data\": null}"),
    ];
    rules.extend(
        commit_diffs
            .iter()
            .map(|(pattern, answer)| (pattern.as_str(), answer.as_str())),
    );
    answer_forge(&rules);
}

/// Opens the pull request in a tab and waits for its outline and diff.
pub(crate) fn open_pull_request(h: &mut Harness) {
    h.wait_until("GitHub's plugin", |ui| {
        ui.plugins
            .get("github")
            .is_some_and(|found| matches!(found.state, crate::plugins::State::Running(_)))
    });
    let pr = h
        .read(|ui| {
            ui.forge
                .forges
                .parse_pr("https://github.com/acme/render-toolkit/pull/41")
        })
        .expect("a pull request link");
    h.act(|ui, cx| ui.open_pull_request(pr, cx));
    h.wait_until("the pull request's outline and diff", |ui| {
        let session = &ui.active().session;
        session.outline.is_some() && !session.diff_in_flight() && !session.document.files.is_empty()
    });
    h.settle();
}

fn paths(h: &mut Harness) -> Vec<String> {
    h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect()
    })
}

/// Scopes to `scope` and waits for its diff.
fn scope_to(h: &mut Harness, action: ReviewAction, scope: Scope) {
    h.perform(Action::Review(action));
    h.wait_until("the scope's diff", |ui| {
        ui.active().scope() == Some(scope) && !ui.active().session.diff_in_flight()
    });
    h.settle();
}

#[test]
fn a_pull_request_opens_on_all_changes_and_its_steps_scope_the_diff() {
    // After the harness takes its turn: the fake `gh` is shared.
    let mut h = Harness::open_fresh(None, (1280.0, 800.0), "", "");
    answer_pull_request(BODY, 3);
    open_pull_request(&mut h);

    // No graph: the steps, oldest first, under the whole, which is selected.
    h.read(|ui| {
        let tab = &ui.tabs[ui.active];
        assert!(matches!(tab.steps(), Steps::List(names) if names.whole == "All changes"));
        assert_eq!(tab.state.session.commits.len(), 0);
        let titles: Vec<&str> = tab
            .state
            .outline_steps()
            .iter()
            .map(|step| step.title.as_str())
            .collect();
        assert_eq!(
            titles,
            [
                "Add the frame pipeline",
                "Route the compositor through the pipeline",
                "Keep the old entry points as wrappers"
            ]
        );
        assert_eq!(tab.state.outline_steps()[1].counts, Some((9, 6)));
        assert_eq!(tab.state.scope(), Some(Scope::All));
        assert!(ui.left_pane_width() > 0.0);
    });
    assert_eq!(
        paths(&mut h),
        ["src/compositor.rs", "src/lib.rs", "src/pipeline.rs"]
    );

    // The header: the pull request's title, facts and Markdown description.
    h.read(|ui| {
        let look = ui.look(ui.resolved_theme().spec());
        let header = crate::commit_info::header(ui, look).expect("a header");
        assert_eq!(header.title.as_deref(), Some("Rework the render pipeline"));
        assert_eq!(header.description, BODY);
        assert_eq!(header.kind, TextKind::Markdown);
    });

    // A step scopes the files and the diff to itself, and heads it.
    scope_to(&mut h, ReviewAction::Scope(Scope::Step(1)), Scope::Step(1));
    assert_eq!(paths(&mut h), ["src/compositor.rs"]);
    h.read(|ui| {
        assert_eq!(
            ui.active().session.selected_revision,
            RevisionSelection::Commit(COMMITS[1].0.to_owned())
        );
        let look = ui.look(ui.resolved_theme().spec());
        let header = crate::commit_info::header(ui, look).expect("a header");
        assert_eq!(
            header.title.as_deref(),
            Some("Route the compositor through the pipeline")
        );
        assert_eq!(header.kind, TextKind::Message);
    });

    // Previous and next walk the steps, and stop at the ends.
    scope_to(&mut h, ReviewAction::Step { by: 1 }, Scope::Step(2));
    assert_eq!(paths(&mut h), ["src/lib.rs"]);
    h.perform(Action::Review(ReviewAction::Step { by: 1 }));
    assert_eq!(h.read(|ui| ui.active().scope()), Some(Scope::Step(2)));
    scope_to(&mut h, ReviewAction::Step { by: -2 }, Scope::Step(0));
    h.read(|ui| {
        let look = ui.look(ui.resolved_theme().spec());
        let header = crate::commit_info::header(ui, look).expect("a header");
        // The title, then the rest of the message.
        assert_eq!(header.title.as_deref(), Some("Add the frame pipeline"));
        assert_eq!(
            header.description,
            "One queue the renderer pushes to and the compositor\npulls from. Nothing uses it yet."
        );
    });

    // The scope item brings everything back.
    scope_to(&mut h, ReviewAction::Scope(Scope::All), Scope::All);
    assert_eq!(
        paths(&mut h),
        ["src/compositor.rs", "src/lib.rs", "src/pipeline.rs"]
    );
}

#[test]
fn the_rendered_or_raw_choice_is_remembered_per_kind_of_text() {
    let mut ui = crate::tabs::tests::app();
    assert!(ui.text_views.rendered(TextKind::Markdown));
    assert!(!ui.text_views.rendered(TextKind::Message));
    crate::tabs::tests::with_cx(|cx| {
        ui.perform_review(ReviewAction::Rendered(TextKind::Markdown, false), cx);
        ui.perform_review(ReviewAction::Rendered(TextKind::Message, true), cx);
    });
    assert!(!ui.text_views.rendered(TextKind::Markdown));
    assert!(ui.text_views.rendered(TextKind::Message));
    let saved = ui.current_window_state();
    assert_eq!(
        (saved.markdown_raw, saved.messages_rendered),
        (Some(true), Some(true))
    );
    // And read back on the next launch.
    let text = toml::to_string(&saved).expect("serialized");
    let back: crate::window_state::WindowState = toml::from_str(&text).expect("parsed");
    assert_eq!(
        (back.markdown_raw, back.messages_rendered),
        (Some(true), Some(true))
    );
}

#[test]
fn a_long_description_shows_its_first_lines_until_show_all() {
    let long: String = (1..=30).map(|n| format!("Line {n}\n\n")).collect();
    // After the harness takes its turn: the fake `gh` is shared.
    let mut h = Harness::open_fresh(None, (1280.0, 800.0), "", "");
    answer_pull_request(&long, 3);
    open_pull_request(&mut h);
    let lines = |h: &mut Harness| {
        h.read(|ui| {
            let look = ui.look(ui.resolved_theme().spec());
            let header = crate::commit_info::header(ui, look).expect("a header");
            crate::commit_info::header_lines(ui, &header, look, true, 1000.0, true)
                .iter()
                .filter(|line| matches!(line, crate::diff_view::HeaderLine::Description(_)))
                .count()
        })
    };
    h.perform(Action::Review(ReviewAction::Rendered(
        TextKind::Markdown,
        false,
    )));
    assert_eq!(lines(&mut h), crate::commit_info::FOLD_LINES);
    h.perform(Action::Review(ReviewAction::ShowAll(true)));
    assert_eq!(lines(&mut h), long.trim().lines().count());
    h.perform(Action::Review(ReviewAction::ShowAll(false)));
    assert_eq!(lines(&mut h), crate::commit_info::FOLD_LINES);
}

#[test]
fn a_source_with_no_steps_has_no_panel_and_no_toggle() {
    // After the harness takes its turn: the fake `gh` is shared.
    let mut h = Harness::open_fresh(None, (1280.0, 800.0), "", "");
    answer_pull_request("", 0);
    open_pull_request(&mut h);
    h.read(|ui| {
        assert_eq!(ui.active_steps(), Steps::None);
        assert_eq!(ui.left_pane_width(), 0.0);
        // Nothing to toggle, from the toolbar or the palette.
        let toggle = ui
            .registry
            .get("view.toggle-revisions")
            .expect("the command");
        assert!(
            toggle
                .enabled(ui, &crate::commands::CommandArg::None)
                .is_err()
        );
    });
    // Even when the panel was left shown, and when it was hidden.
    h.perform(Action::ToggleHistoryPanel);
    assert_eq!(h.read(|ui| ui.left_pane_width()), 0.0);
    // The header has no description, so no Rendered/Raw toggle and no inset.
    h.read(|ui| {
        let look = ui.look(ui.resolved_theme().spec());
        let header = crate::commit_info::header(ui, look).expect("a header");
        assert!(header.description.is_empty());
        let lines = crate::commit_info::header_lines(ui, &header, look, true, 1000.0, true);
        assert!(
            !lines
                .iter()
                .any(|line| matches!(line, crate::diff_view::HeaderLine::Inset))
        );
    });
}
