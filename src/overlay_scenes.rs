//! Scenes and keyboard flows of the transient surfaces, through the headless
//! harness: every scene id is one from `shots/iced/scenes.md`.

use diffui_core::RevisionSelection;
use diffui_core::askpass::{AskpassEvent, Prompt, PromptRequest};
use gpui_kit::div;
use gpui_kit::prelude::*;

use crate::Action;
use crate::activity::{ActivityId, ActivityStatus};
use crate::bookmarks::BookmarkAction;
use crate::changes::ChangesMsg;
use crate::conflicts::ConflictMsg;
use crate::field::{FilterField, filter_field};
use crate::harness::{Harness, at, fixture_repo};
use crate::prose::Prose;
use crate::theme::ThemePreference;
use crate::ui::Look;

const LANE: &str = "overlays";

fn app() -> Harness {
    let repo = fixture_repo();
    let mut h = Harness::open_sized(Some(&repo), (1440.0, 900.0));
    h.wait_for_graph();
    h
}

fn palette(query: &str, scene: &str) {
    let mut h = app();
    h.press("cmd-k");
    if !query.is_empty() {
        h.type_text(query);
    }
    h.shot(LANE, scene);
}

#[test]
fn palette_empty() {
    palette("", "palette-empty");
}

#[test]
fn palette_mixed_query() {
    palette("render", "palette-mixed-query");
}

#[test]
fn palette_commands() {
    palette(">", "palette-commands");
}

#[test]
fn palette_files() {
    palette("@", "palette-files");
}

#[test]
fn palette_no_results() {
    palette("zzzzqqqq", "palette-no-results");
}

#[test]
fn palette_dark() {
    let mut h = app();
    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.press("cmd-k");
    h.type_text(">");
    h.shot(LANE, "palette-dark");
}

#[test]
fn palette_actions_column() {
    let mut h = app();
    h.press("cmd-k");
    h.type_text("pipeline");
    h.settle();
    h.press("tab");
    h.settle();
    assert_eq!(h.read(|ui| ui.palette().unwrap().stack.len()), 2);
    // Past the slide.
    std::thread::sleep(std::time::Duration::from_millis(350));
    h.act(|_, cx| cx.notify());
    h.shot(LANE, "palette-actions-column");
    // Esc pops the column and hands the keyboard back to the one under it.
    h.press("escape");
    h.settle();
    assert_eq!(h.read(|ui| ui.palette().unwrap().stack.len()), 1);
    let before = h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone());
    h.type_text("x");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone()),
        format!("{before}x")
    );
}

fn cmdline(line: &str, scene: &str) -> Harness {
    let mut h = app();
    h.press(":");
    h.settle();
    assert!(h.read(|ui| ui.command_line().is_some()), "the command line");
    if !line.is_empty() {
        h.type_text(line);
    }
    h.shot(LANE, scene);
    h
}

#[test]
fn cmdline_empty() {
    cmdline("", "cmdline-empty");
}

#[test]
fn cmdline_typed_rebase() {
    cmdline("rebase -s @ -d main", "cmdline-typed-rebase");
}

#[test]
fn cmdline_revset_error() {
    let mut h = cmdline("rebase -s foo( -d main", "cmdline-revset-error");
    // What's typed is what's there: the editor underneath closes no brackets.
    assert_eq!(
        h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone()),
        "jj rebase -s foo( -d main"
    );
}

#[test]
fn cmdline_completion() {
    let mut h = cmdline("git pu", "cmdline-completion");
    assert!(h.read(|ui| !ui.command_line().unwrap().completions.is_empty()));
}

#[test]
fn cmdline_unknown() {
    cmdline("frobnicate", "cmdline-unknown");
}

#[test]
fn cli_output_panel() {
    let mut h = cmdline("status", "cli-output-typed");
    h.press("enter");
    h.wait_until("the run to end", |ui| {
        ui.shown_cli_run().is_some_and(|run| run.exit.is_some())
    });
    // The run is this binary started as jj, which under test is the test runner:
    // its lines are put in by hand, one of each stream.
    h.act(|ui, _| {
        let line = |stream, text: &str| diffui_core::cli::CliLine {
            stream,
            text: text.to_owned(),
        };
        use diffui_core::cli::CliStream::{Stderr, Stdout};
        let run = ui.active_mut().cmdline.runs.last_mut().unwrap();
        run.lines = vec![
            line(Stdout, "Working copy changes:"),
            line(Stdout, "A assets/working-copy.png"),
            line(Stdout, "D docs/empty-file.txt"),
            line(Stdout, "M src/main.rs"),
            line(Stdout, "Working copy  (@) : lvtsvnkk fa52156f (no description set)"),
            line(Stdout, "Parent commit (@-): wutnkqnv 5e5d5d79 feature/render-pipeline* release/conflicted?? release/conflicted@git | fix(render): clamp the sample count"),
            line(Stderr, "Warning: These bookmarks have conflicts:"),
            line(Stderr, "  release/conflicted"),
            line(Stderr, "Hint: Use `jj bookmark list` to see details. Use `jj bookmark set <name> -r <rev>` to resolve."),
        ];
    });
    h.shot(LANE, "cli-output-panel");
    h.press("escape");
    assert!(h.read(|ui| ui.shown_cli_run().is_none()));
}

fn prompt(h: &mut Harness, text: &str, hint: &str) {
    let event = AskpassEvent::Prompt(PromptRequest {
        id: 1,
        prompt: Prompt::parse(text, hint),
    });
    h.act(|ui, cx| ui.on_askpass_event(event, cx));
}

#[test]
fn credential_password() {
    let mut h = app();
    prompt(&mut h, "Password for 'https://example.com': ", "");
    h.type_text("hunter2");
    h.shot(LANE, "credential-password");
}

#[test]
fn credential_username() {
    let mut h = app();
    prompt(&mut h, "Username for 'https://example.com': ", "");
    h.type_text("wonrax");
    h.shot(LANE, "credential-username");
}

const HOST_KEY: &str = "The authenticity of host 'github.com (140.82.112.3)' can't be established.\n\
ED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\n\
This key is not known by any other names.\n\
Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

#[test]
fn credential_host_key() {
    let mut h = app();
    prompt(&mut h, HOST_KEY, "");
    h.shot(LANE, "credential-host-key");
}

#[test]
fn credential_passphrase() {
    let mut h = app();
    prompt(
        &mut h,
        "Enter passphrase for key '/Users/me/.ssh/id_ed25519': ",
        "",
    );
    h.shot(LANE, "credential-passphrase");
}

#[test]
fn credential_notice() {
    let mut h = app();
    prompt(
        &mut h,
        "Confirm user presence for key ED25519-SK SHA256:abc",
        "none",
    );
    h.shot(LANE, "credential-notice");
}

#[test]
fn credential_dark() {
    let mut h = app();
    h.perform(Action::SetTheme(ThemePreference::Dark));
    prompt(&mut h, "Password for 'https://example.com': ", "");
    h.shot(LANE, "credential-dark");
}

fn name_prompt(text: &str, scene: &str) {
    let mut h = app();
    h.perform(Action::Bookmark(BookmarkAction::Create {
        at: RevisionSelection::WorkingCopy,
    }));
    h.settle();
    if !text.is_empty() {
        h.type_text(text);
    }
    h.shot(LANE, scene);
}

#[test]
fn name_prompt_create() {
    name_prompt("", "name-prompt-create");
}

#[test]
fn name_prompt_invalid() {
    name_prompt("bad name", "name-prompt-invalid");
}

#[test]
fn name_prompt_exists() {
    name_prompt("main", "name-prompt-exists");
}

#[test]
fn name_prompt_rename() {
    let mut h = app();
    h.perform(Action::Bookmark(BookmarkAction::Rename {
        name: "main".to_owned(),
    }));
    h.shot(LANE, "name-prompt-rename");
}

/// Two finished rows, one with output, and one that failed.
fn logged(h: &mut Harness) {
    h.act(|ui, _| {
        let log = &mut ui.active_mut().activities;
        log.start(ActivityId(101), "Refresh", false);
        log.finish(ActivityId(101), ActivityStatus::Done, None);
        log.start(
            ActivityId(102),
            Prose::from("Fetch ").remote("origin"),
            false,
        );
        log.finish(
            ActivityId(102),
            ActivityStatus::Done,
            Some("Nothing new".into()),
        );
        log.start(
            ActivityId(103),
            Prose::from("Push ").bookmark("feature/render"),
            false,
        );
        log.set_command(
            ActivityId(103),
            Some("jj git push -b feature/render --remote origin".to_owned()),
        );
        log.extend_output(
            ActivityId(103),
            [
                "Changes to push to origin:".to_owned(),
                "  Add bookmark feature/render to 9f3c1a2b4d5e".to_owned(),
                "remote: Create a pull request on https://example.com/pull/new/feature/render"
                    .to_owned(),
            ],
        );
        log.finish(ActivityId(103), ActivityStatus::Done, None);
    });
}

fn failed(h: &mut Harness) {
    h.act(|ui, _| {
        let log = &mut ui.active_mut().activities;
        log.start(ActivityId(104), Prose::from("Fetch ").remote("origin"), false);
        log.finish(
            ActivityId(104),
            ActivityStatus::Error,
            Some("Couldn't reach the remote: failed to connect to example.com port 443 after 75 seconds, operation timed out".to_owned()),
        );
    });
}

#[test]
fn activity_popover() {
    let mut h = app();
    logged(&mut h);
    h.perform(Action::ToggleActivityPopover);
    h.shot(LANE, "activity-popover");
}

/// A press outside the popover closes it and still lands, as a macOS
/// popover's does; one on the card keeps it open.
#[test]
fn a_press_outside_the_activity_popover_closes_it() {
    let mut h = app();
    logged(&mut h);
    h.perform(Action::ToggleActivityPopover);
    h.settle();
    h.click_point(at(180.0, 240.0));
    h.settle();
    assert!(!h.read(|ui| ui.activity_popover_open()));
}

#[test]
fn activity_popover_dark() {
    let mut h = app();
    h.perform(Action::SetTheme(ThemePreference::Dark));
    logged(&mut h);
    h.perform(Action::ToggleActivityPopover);
    h.shot(LANE, "activity-popover-dark");
}

#[test]
fn activity_popover_expanded() {
    let mut h = app();
    logged(&mut h);
    h.act(|ui, _| ui.on_activity_expand(ActivityId(103)));
    h.perform(Action::ToggleActivityPopover);
    h.shot(LANE, "activity-popover-expanded");
}

#[test]
fn activity_popover_failed() {
    let mut h = app();
    failed(&mut h);
    h.perform(Action::ToggleActivityPopover);
    h.shot(LANE, "activity-popover-failed");
    // A line cut short is what makes the row open, and open it's whole.
    h.act(|ui, _| ui.on_activity_expand(ActivityId(104)));
    h.shot(LANE, "activity-popover-failed-open");
}

#[test]
fn toast_operation_failed() {
    let mut h = app();
    h.act(|ui, _| {
        ui.push_error_toast(
            Prose::from("Fetch ").remote("origin").text(" failed"),
            "Couldn't reach the remote: failed to connect to example.com port 443 after 75 seconds",
        );
    });
    h.shot(LANE, "toast-operation-failed");
}

#[test]
fn toast_config_error() {
    let mut h = app();
    h.act(|ui, _| {
        ui.push_error_toast(
            "Couldn't read config.toml",
            "TOML parse error at line 3, column 8: invalid table header",
        );
    });
    h.shot(LANE, "toast-config-error");
}

/// The toolbar is the chrome lane's: the indicator and the line, on their own.
#[test]
fn toolbar_running_activity() {
    let mut h = app();
    h.act(|ui, _| {
        let log = &mut ui.active_mut().activities;
        log.start(
            ActivityId(201),
            Prose::from("Fetch ").remote("origin"),
            false,
        );
        log.start(ActivityId(202), "Refresh", false);
    });
    // Past the delay that keeps short work from flashing.
    std::thread::sleep(std::time::Duration::from_millis(200));
    h.show(|ui, theme, window, cx| {
        let look = ui.look(theme);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_end()
            .p_4()
            .child(crate::activity::activity_indicator(ui, look, window, cx))
            .child(crate::activity::activity_progress_line(
                ui, look, window, cx,
            ))
            .into_any_element()
    });
    h.shot(LANE, "toolbar-running-activity");
}

#[test]
fn conflict_marker_editor() {
    let mut h = app();
    // The default revset may not list it.
    h.perform(Action::SetRevset("all()".to_owned()));
    h.wait_until("the bookmark", |ui| {
        ui.active()
            .session
            .commits
            .find_by_bookmark("conflict/settings")
            .is_some()
    });
    conflict_on(h);
}

fn conflict_on(mut h: Harness) {
    let hex = h.read(|ui| {
        ui.active()
            .session
            .commits
            .find_by_bookmark("conflict/settings")
            .map(|row| row.commit_id().to_owned())
            .expect("the bookmark")
    });
    h.perform(Action::SelectRevision(RevisionSelection::Commit(hex)));
    h.wait_until("the conflicted diff", |ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .any(|file| file.path == "src/config.rs")
    });
    h.perform(Action::Changes(ChangesMsg::Conflict(
        ConflictMsg::EditMarkers("src/config.rs".to_owned()),
    )));
    h.wait_until("the editor", |ui| {
        crate::conflicts::editor_shown(ui).is_some()
    });
    h.show(|ui, theme, window, cx| {
        let look = ui.look(theme);
        crate::conflicts::marker_editor(ui, look, window, cx)
            .unwrap_or_else(|| div().into_any_element())
    });
    h.shot(LANE, "conflict-marker-editor");
}

/// The revset field alone: the panels lane places it in the sidebar.
fn field_scene(h: &mut Harness) {
    h.show(|ui, theme, window, cx| {
        let look = ui.look(theme);
        let field = filter_field(
            Look {
                font: ui.config.mono_font,
                ..look
            },
            FilterField {
                id: crate::sidebar::REVSET_INPUT_ID,
                leading_icon: Some(crate::icons::SEARCH),
                placeholder: "revset",
                value: &ui.active().session.revset,
                on_input: |this, value, cx| this.revset_field_edited(value, cx),
                on_submit: Some(|this, cx| this.perform(Action::SubmitRevset, cx)),
                caret: None,
            },
            window,
            cx,
        );
        div()
            .w(gpui_kit::px(ui.sidebar_width))
            .flex()
            .flex_col()
            .child(div().p_1().pl_3().child(field))
            .children(crate::revset_assist::error_strip(ui, look, window, cx))
            .children(crate::revset_assist::dropdown(ui, look, window, cx))
            .into_any_element()
    });
}

fn focused_field() -> Harness {
    let mut h = app();
    field_scene(&mut h);
    h.act(|ui, cx| {
        ui.active_mut().session.revset = String::new();
        crate::input::focus(crate::sidebar::REVSET_INPUT_ID, cx);
    });
    h.settle();
    h
}

#[test]
fn revset_completion() {
    let mut h = focused_field();
    h.type_text("bookm");
    h.wait_until("completions", |ui| ui.active().cmdline.field.open);
    h.shot(LANE, "revset-completion");
}

#[test]
fn revset_completion_args() {
    let mut h = focused_field();
    h.type_text("ancestors(feat");
    h.wait_until("completions", |ui| ui.active().cmdline.field.open);
    h.shot(LANE, "revset-completion-args");
}

#[test]
fn revset_error_strip() {
    let mut h = focused_field();
    h.type_text("foo(");
    h.press("enter");
    h.wait_until("the check", |ui| ui.active().cmdline.field.error.is_some());
    h.shot(LANE, "revset-error-strip");
}

// ── Keyboard flows ──────────────────────────────────────────────────────

/// ↑/↓ in a one-line field that has no list open go to the file list behind it,
/// the way they did: the field has no use for them.
#[test]
fn arrows_in_a_field_walk_the_list_behind_it() {
    let mut h = focused_field();
    h.perform(Action::Refresh);
    h.wait_until("a diff with files", |ui| {
        ui.active().session.document.files.len() > 2
    });
    let first = h.read(|ui| ui.active().selected_file);
    h.press("down");
    h.settle();
    let moved = h.read(|ui| ui.active().selected_file);
    assert_ne!(moved, first, "down moved the file");
    h.press("up");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.active().selected_file),
        first,
        "up moved back"
    );
    assert!(
        h.with_window(|window, _| crate::input::input_has_focus(window)),
        "the field keeps the keyboard"
    );
    // A typed j is the field's, not the list's.
    h.type_text("j");
    h.press("j");
    h.settle();
    assert_eq!(h.read(|ui| ui.active().selected_file), first);
    assert_eq!(h.read(|ui| ui.active().session.revset.clone()), "jj");
}

#[test]
fn arrows_move_the_dropdown_and_keep_its_row_in_sight() {
    let mut h = focused_field();
    // Every function and bookmark is a candidate for an empty word in a call.
    h.type_text("ancestors(");
    h.wait_until("completions", |ui| {
        ui.active().cmdline.field.completions.len() > 10
    });
    let file = h.read(|ui| ui.active().selected_file);
    for _ in 0..10 {
        h.press("down");
    }
    h.settle();
    assert_eq!(h.read(|ui| ui.active().cmdline.field.selected), 10);
    assert_eq!(
        h.read(|ui| ui.active().selected_file),
        file,
        "the list stays"
    );
    h.shot(LANE, "revset-completion-scrolled");
    // ⌃N/⌃P walk it too.
    h.press("ctrl-p");
    h.settle();
    assert_eq!(h.read(|ui| ui.active().cmdline.field.selected), 9);
    // Tab takes the row and keeps the keyboard.
    h.press("tab");
    h.settle();
    assert!(
        !h.read(|ui| ui.active().session.revset == "ancestors("),
        "completed"
    );
}

#[test]
fn esc_closes_the_dropdown_and_lets_go_of_the_field() {
    let mut h = focused_field();
    h.type_text("bookm");
    h.wait_until("completions", |ui| ui.active().cmdline.field.open);
    h.press("escape");
    h.settle();
    assert!(!h.read(|ui| ui.active().cmdline.field.open));
    assert!(!h.with_window(|window, _| crate::input::input_has_focus(window)));
    // The next plain key is the keymap's.
    let file = h.read(|ui| ui.active().selected_file);
    h.press("j");
    h.settle();
    assert_eq!(h.read(|ui| ui.active().session.revset.clone()), "bookm");
    assert_ne!(h.read(|ui| ui.active().selected_file), file);
}

#[test]
fn enter_in_a_field_submits_it() {
    let mut h = focused_field();
    h.type_text("all()");
    h.settle();
    h.press("enter");
    h.wait_until("the revset applied", |ui| {
        ui.active().session.commits.len() > 5
    });
    assert!(!h.read(|ui| ui.active().cmdline.field.open));
}

#[test]
fn tab_in_a_field_goes_nowhere() {
    let mut h = focused_field();
    h.press("tab");
    h.settle();
    assert!(h.with_window(|window, _| crate::input::input_has_focus(window)));
    assert_eq!(h.read(|ui| ui.active().session.revset.clone()), "");
}

#[test]
fn palette_keys() {
    let mut h = app();
    h.press("cmd-k");
    h.type_text(">");
    h.settle();
    let selected = |h: &mut Harness| h.read(|ui| ui.palette().unwrap().top().unwrap().selected);
    assert_eq!(selected(&mut h), 0);
    h.press("down");
    assert_eq!(selected(&mut h), 1);
    h.press("ctrl-n");
    assert_eq!(selected(&mut h), 2);
    h.press("up");
    assert_eq!(selected(&mut h), 1);
    h.press("ctrl-p");
    assert_eq!(selected(&mut h), 0);
    // Past the fold, the highlight stays in sight.
    for _ in 0..30 {
        h.press("down");
    }
    h.settle();
    h.shot(LANE, "palette-scrolled");
    // The list scrolled with it: the row under the list's top is past the
    // first screenful.
    let top = crate::toolbar::modal_top() + crate::palette::list_top() + 20.0;
    h.hover_point(at(1440.0 / 2.0, top));
    h.settle();
    let row = selected(&mut h);
    assert!(row > 15, "the list stayed at its top: {row}");
    // The keys didn't reach what's behind.
    assert_eq!(
        h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone()),
        ">"
    );
    h.press("escape");
    assert!(h.read(|ui| ui.palette().is_none()));
    assert!(!h.with_window(|window, _| crate::input::input_has_focus(window)));
}

#[test]
fn hovering_a_row_highlights_it() {
    let mut h = app();
    h.press("cmd-k");
    h.settle();
    let top = crate::toolbar::modal_top() + crate::palette::list_top() + 20.0;
    let x = 1440.0 / 2.0;
    h.hover_point(at(x, top + 3.0 * crate::palette::ROW_HEIGHT));
    h.settle();
    let row = h.read(|ui| ui.palette().unwrap().top().unwrap().selected);
    assert!(row >= 2, "hover moved the highlight: {row}");
}

#[test]
fn command_line_keys() {
    let mut h = app();
    h.press(":");
    h.settle();
    h.type_text("git pu");
    h.wait_until("completions", |ui| {
        ui.command_line()
            .is_some_and(|command| !command.completions.is_empty())
    });
    let line = |h: &mut Harness| h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone());
    assert_eq!(line(&mut h), "jj git pu");
    // The scope tabs keep their widths with their counts hidden.
    h.shot(LANE, "palette-jj-command");
    // Tab takes the candidate, into the text and its caret.
    h.press("tab");
    h.settle();
    assert!(line(&mut h).starts_with("jj git pu"));
    assert!(line(&mut h).len() > "jj git pu".len());
    h.type_text("x");
    h.settle();
    assert!(line(&mut h).ends_with('x'), "{}", line(&mut h));
    // Emptied, the line is still a command line: the `jj` chip stays.
    h.press("cmd-backspace");
    h.settle();
    assert_eq!(line(&mut h), "jj ");
    assert!(h.read(|ui| ui.command_line().is_some()));
    // Backspace on the empty line goes back to the search, whose input takes
    // over, caret and all.
    h.press("backspace");
    h.settle();
    assert!(h.read(|ui| ui.command_line().is_none()));
    h.type_text("abc");
    h.settle();
    assert_eq!(line(&mut h), "abc");
    h.press("escape");
    assert!(h.read(|ui| ui.palette().is_none()));
}

#[test]
fn name_prompt_keys() {
    let mut h = app();
    h.perform(Action::Bookmark(BookmarkAction::Create {
        at: RevisionSelection::WorkingCopy,
    }));
    h.settle();
    h.type_text("fresh-name");
    h.settle();
    h.press("backspace");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.name_prompt().unwrap().value.clone()),
        "fresh-nam"
    );
    h.press("escape");
    assert!(h.read(|ui| ui.name_prompt().is_none()));
}

#[test]
fn credential_keys() {
    let mut h = app();
    prompt(&mut h, "Password for 'https://example.com': ", "");
    h.type_text("s3cret");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.credential_dialog().unwrap().value.clone()),
        "s3cret"
    );
    // A stray click on the scrim must not cancel a push.
    h.click_point(at(20.0, 800.0));
    h.settle();
    assert!(h.read(|ui| ui.credential_dialog().is_some()));
    h.press("escape");
    assert!(h.read(|ui| ui.credential_dialog().is_none()));
}
