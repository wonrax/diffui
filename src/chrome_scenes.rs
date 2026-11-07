//! The chrome's scenes (`shots/iced/scenes.md`) and what the real event path does
//! to it: tab drags, the strip's controls, the overlay menu.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use diffui_core::RevisionSelection;

use crate::commands::CommandArg;
use crate::geometry::{Point, Rectangle, Size};
use crate::harness::{Harness, at, fixture_repo};
use crate::menu::{AnchorSpec, MenuEntry, OverlayMenu};
use crate::modes::{Mode, ModeKind};
use crate::theme::ThemePreference;
use crate::{Action, MainView, TabId, activity};

const LANE: &str = "chrome";
const BASELINE: (f32, f32) = (1440.0, 900.0);

/// The scratch directory the repositories under `acme/` live in.
fn acme() -> PathBuf {
    fixture_repo()
        .parent()
        .expect("the fixture has a parent")
        .join("acme")
}

/// A copy of the fixture at `acme/<name>`, so the tab reads `acme/<name>`.
fn repo_named(name: &str) -> PathBuf {
    // Tests name their repositories before they take the harness's turn.
    static COPYING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _copying = COPYING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dest = acme().join(name);
    if !dest.exists() {
        std::fs::create_dir_all(acme()).expect("create acme");
        let status = Command::new("cp")
            .arg("-R")
            .arg(fixture_repo())
            .arg(&dest)
            .status()
            .expect("run cp");
        assert!(status.success());
    }
    dest
}

fn render_toolkit() -> PathBuf {
    repo_named("render-toolkit")
}

/// A plain git repository with one commit.
fn plain_git() -> &'static Path {
    static REPO: OnceLock<PathBuf> = OnceLock::new();
    REPO.get_or_init(|| {
        let repo = acme().join("plain-git");
        std::fs::create_dir_all(&repo).expect("create the git repo");
        std::fs::write(repo.join("README.md"), "plain git\n").expect("write a file");
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=Harness",
                "-c",
                "user.email=harness@example.com",
                "commit",
                "-qm",
                "first commit",
            ],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("run git");
            assert!(status.success());
        }
        repo
    })
}

fn open_fixture() -> Harness {
    let mut h = Harness::open_sized(Some(&render_toolkit()), BASELINE);
    h.wait_for_graph();
    quiet_toasts(&mut h);
    h
}

/// The owner's jj CLI differs from the library, which raises a sticky toast, and
/// the toast layer is still a stub that blocks the pointer and covers the shot.
fn quiet_toasts(h: &mut Harness) {
    h.act(|ui, _| ui.toasts.clear());
}

fn open_second(h: &mut Harness, path: &Path) {
    let before = h.read(|ui| ui.tabs.len());
    h.perform(Action::OpenRepo(path.to_string_lossy().into_owned()));
    h.wait_until("the new tab", |ui| ui.tabs.len() == before + 1);
    h.wait_for_graph();
    quiet_toasts(h);
}

fn commit_with(h: &mut Harness, description: &str) -> RevisionSelection {
    let description = description.to_owned();
    let id = h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .find(|row| row.description().starts_with(&description))
            .map(|row| row.commit_id().to_owned())
    });
    RevisionSelection::Commit(id.unwrap_or_else(|| panic!("no revision {description}")))
}

fn commit_on(h: &mut Harness, bookmark: &str) -> RevisionSelection {
    let bookmark = bookmark.to_owned();
    let id = h.read(|ui| {
        ui.active()
            .session
            .bookmarks
            .bookmarks
            .iter()
            .find(|entry| entry.name == bookmark)
            .and_then(|entry| entry.local_target().map(str::to_owned))
    });
    RevisionSelection::Commit(id.unwrap_or_else(|| panic!("no bookmark {bookmark}")))
}

fn select(h: &mut Harness, revision: RevisionSelection) {
    h.perform(Action::SelectRevision(revision));
    h.settle();
}

/// Waits for a confirm dialog and shoots it.
fn confirm_scene(h: &mut Harness, scene: &str) {
    h.wait_until(scene, |ui| ui.confirm().is_some());
    h.shot(LANE, scene);
    h.perform(Action::ConfirmCancel);
}

fn run(h: &mut Harness, command: crate::commands::CommandId, arg: CommandArg) {
    h.act(|ui, cx| ui.run_command(command, arg, cx));
}

fn bookmark_arg(name: &str, remote: Option<&str>) -> CommandArg {
    CommandArg::Bookmark {
        name: name.to_owned(),
        remote: remote.map(str::to_owned),
        to: None,
    }
}

// ── Welcome ─────────────────────────────────────────────────────────────────

#[test]
fn welcome_scenes() {
    let mut h = Harness::open_sized(None, BASELINE);
    h.act(|ui, _| ui.recent_repos.clear());
    h.shot(LANE, "welcome-empty");

    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.shot(LANE, "welcome-dark");
    h.perform(Action::SetTheme(ThemePreference::Light));

    h.act(|ui, _| {
        ui.recent_repos = vec![
            render_toolkit().to_string_lossy().into_owned(),
            "/Users/ada/code/some/deeply/nested/place/that/runs/on/and/on/and/on/forever/project"
                .to_owned(),
        ];
    });
    h.shot(LANE, "welcome-recents");

    h.click("welcome-shortcuts");
    assert!(h.read(|ui| ui.mode(ModeKind::Shortcuts).is_some()));
    h.shot(LANE, "shortcuts-welcome");
    h.perform(Action::CloseShortcuts);

    let typed = format!("{}/", acme().display());
    render_toolkit();
    h.act(|ui, cx| ui.on_open_repo_path_changed(typed, cx));
    h.wait_until("the folder listing", |ui| {
        ui.open_repo_dialog()
            .and_then(|dialog| dialog.suggestions())
            .is_some_and(|(_, rows)| !rows.is_empty())
    });
    h.shot(LANE, "welcome-typed-path");

    h.act(|ui, cx| ui.on_open_repo_path_changed("/nonexistent/place".to_owned(), cx));
    h.perform(Action::SubmitRepoDialog);
    h.wait_until("the open error", |ui| {
        ui.open_repo_dialog()
            .is_some_and(|dialog| dialog.error.is_some())
    });
    h.shot(LANE, "welcome-open-error");
}

// ── Shell ───────────────────────────────────────────────────────────────────

#[test]
fn shell_scenes() {
    let mut h = open_fixture();
    h.perform(Action::SetTheme(ThemePreference::Light));
    h.shot(LANE, "base-working-copy");
    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.shot(LANE, "base-dark");
    h.perform(Action::SetTheme(ThemePreference::HighContrast));
    h.shot(LANE, "base-contrast");
    h.perform(Action::SetTheme(ThemePreference::Light));

    let tab = h.read(|ui| ui.active_tab_id()).expect("a tab");
    h.act(|ui, _| {
        ui.begin_activity(tab, "Fetching from origin", false);
        ui.begin_activity(tab, "Refresh", false);
    });
    h.shot_now(LANE, "toolbar-running-activity");
    h.act(|ui, _| ui.active_mut().activities = activity::ActivityLog::default());

    h.perform(Action::SetMainView(MainView::Source));
    h.shot(LANE, "toolbar-source-view");

    // Through a click: the segment is a real control.
    h.click("segment-Diff");
    assert_eq!(h.read(|ui| ui.active().main_view), MainView::Diff);
    h.click("segment-History");
    assert_eq!(h.read(|ui| ui.active().main_view), MainView::History);
    h.shot(LANE, "view-switcher-main-view");
}

#[test]
fn a_plain_git_toolbar_has_no_jj_actions() {
    let mut h = Harness::open_sized(Some(plain_git()), BASELINE);
    h.wait_for_graph();
    quiet_toasts(&mut h);
    h.shot(LANE, "toolbar-plain-git");
}

#[test]
fn a_cold_load_is_shot_mid_flight() {
    let mut h = Harness::open_sized(Some(&render_toolkit()), BASELINE);
    h.shot_now(LANE, "load-cold");
}

#[test]
fn tab_scenes() {
    let mut h = open_fixture();
    open_second(&mut h, &repo_named("second-repo"));
    h.shot(LANE, "tabs-two-repos");
    for name in ["one", "two", "three", "four"] {
        open_second(
            &mut h,
            &repo_named(&format!("a-rather-long-repository-name-{name}")),
        );
    }
    h.shot(LANE, "tabs-many-long");
    assert_eq!(h.read(|ui| ui.tabs.len()), 6);
    // Squeezed, the strip still ends before the palette control.
    let last = h.read(|ui| ui.tab_strip.borrow().last_slot_end());
    assert!(last < BASELINE.0 - 40.0, "tabs run to {last}");
}

// ── The strip's behaviour ───────────────────────────────────────────────────

fn order(h: &mut Harness) -> Vec<TabId> {
    h.read(|ui| ui.tabs.iter().map(|tab| tab.id).collect())
}

/// The left and right of tab `index`'s slot, and the strip's middle height.
fn slot_span(h: &mut Harness, index: usize) -> (f32, f32) {
    h.read(|ui| ui.tab_strip.borrow().slot_span(index))
}

const STRIP_Y: f32 = 18.0;

#[test]
fn dragging_a_tab_reorders_and_esc_puts_it_back() {
    let mut h = open_fixture();
    open_second(&mut h, &repo_named("second-repo"));
    let start = order(&mut h);
    let first = slot_span(&mut h, 0);
    let second = slot_span(&mut h, 1);

    // A press selects without any travel; less than the threshold stays a click.
    let on_first = first.0 + 30.0;
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        window.pointer_down(at(on_first, STRIP_Y), gpui_kit::MouseButton::Left, cx);
        window.pointer_move(
            at(on_first + 3.0, STRIP_Y),
            Some(gpui_kit::MouseButton::Left),
            cx,
        );
        window.pointer_up(at(on_first + 3.0, STRIP_Y), gpui_kit::MouseButton::Left, cx);
    });
    assert_eq!(order(&mut h), start, "a wobble doesn't reorder");
    assert_eq!(h.read(|ui| ui.active), 0, "the press selected the tab");

    // Past the neighbour's centre, the two trade places while still in the air.
    let past = second.1 + 10.0;
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        let held = Some(gpui_kit::MouseButton::Left);
        window.pointer_down(at(on_first, STRIP_Y), gpui_kit::MouseButton::Left, cx);
        // The drag takes hold of the tab where the threshold is crossed.
        window.pointer_move(at(on_first + 8.0, STRIP_Y), held, cx);
        window.pointer_move(at(past, STRIP_Y), held, cx);
    });
    let moved = order(&mut h);
    assert_eq!(moved, vec![start[1], start[0]], "the drag reordered");
    assert_eq!(h.read(|ui| ui.active), 1, "the dragged tab stays active");

    // Esc abandons it.
    h.press("escape");
    assert_eq!(order(&mut h), start, "esc put it back");
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        window.pointer_up(at(past, STRIP_Y), gpui_kit::MouseButton::Left, cx);
    });

    h.wait_until("the slides to run out", |ui| {
        ui.tab_strip.borrow().at_rest()
    });

    // And a whole drag lands.
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        let held = Some(gpui_kit::MouseButton::Left);
        window.pointer_down(at(on_first, STRIP_Y), gpui_kit::MouseButton::Left, cx);
        window.pointer_move(at(on_first + 8.0, STRIP_Y), held, cx);
        window.pointer_move(at(past, STRIP_Y), held, cx);
        window.pointer_up(at(past, STRIP_Y), gpui_kit::MouseButton::Left, cx);
    });
    h.settle();
    assert_eq!(order(&mut h), vec![start[1], start[0]], "the drop stuck");
    h.wait_until("the drop to settle", |ui| ui.tab_strip.borrow().at_rest());
}

#[test]
fn the_strips_controls_do_what_they_say() {
    let mut h = open_fixture();
    h.click("palette-hint");
    assert!(h.read(|ui| ui.palette().is_some()));
    h.perform(Action::ClosePalette);

    h.click("tab-add");
    assert!(h.read(|ui| ui.mode(ModeKind::OpenRepo).is_some()));
    h.press("escape");
    assert!(h.read(|ui| ui.mode(ModeKind::OpenRepo).is_none()));

    // The `×` closes without first selecting: the press stays on the button.
    open_second(&mut h, &repo_named("second-repo"));
    assert_eq!(h.read(|ui| ui.active), 1);
    let doomed = order(&mut h)[0];
    let on_doomed = slot_span(&mut h, 0).0 + 30.0;
    h.hover_point(at(on_doomed, STRIP_Y));
    h.settle();
    let close = ("tab-close", doomed.0 as usize);
    h.with_window(|window, cx| {
        use gpui_kit::test::TestWindowExt as _;
        window.click(close, cx);
    });
    h.settle();
    assert_eq!(h.read(|ui| ui.tabs.len()), 1);
}

#[test]
fn the_fetch_caret_pops_its_menu() {
    use crate::menus::tests::{SCRIPT, Script};
    let mut h = open_fixture();
    SCRIPT.with_borrow_mut(|script| *script = Script::default());
    h.click("toolbar-fetch-caret");
    h.settle();
    let shown = SCRIPT.with_borrow(|script| script.shown.clone());
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0[0], "Fetch");
    assert_eq!(shown[0].0[1], "Fetch all remotes");
}

// ── Dialogs ─────────────────────────────────────────────────────────────────

#[test]
fn confirm_scenes() {
    let mut h = open_fixture();
    let plain = commit_with(&mut h, "fix(render): clamp");
    select(&mut h, plain.clone());
    h.press("a");
    confirm_scene(&mut h, "confirm-abandon");

    h.press("a");
    h.wait_until("the dialog", |ui| ui.confirm().is_some());
    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.shot(LANE, "confirm-dark");
    h.perform(Action::ConfirmCancel);
    h.perform(Action::SetTheme(ThemePreference::Light));

    // A button on the dialog goes through the same path.
    h.press("a");
    h.wait_until("the dialog", |ui| ui.confirm().is_some());
    h.click("confirm-cancel");
    assert!(h.read(|ui| ui.confirm().is_none()));

    let other = commit_with(&mut h, "perf(render)");
    let ids = [plain.clone(), other].map(|revision| match revision {
        RevisionSelection::Commit(id) => id,
        RevisionSelection::WorkingCopy => unreachable!(),
    });
    h.act(|ui, _| {
        ui.active_mut().revision_multi_selection = ids.to_vec();
    });
    h.press("a");
    confirm_scene(&mut h, "confirm-abandon-many");
    h.perform(Action::ClearMultiSelection);

    let main = commit_on(&mut h, "main");
    select(&mut h, main);
    h.press("a");
    confirm_scene(&mut h, "confirm-immutable");
    select(&mut h, RevisionSelection::WorkingCopy);

    h.press("shift-p");
    confirm_scene(&mut h, "confirm-push-tracked");

    run(
        &mut h,
        "revision.push-change",
        CommandArg::Revision(RevisionSelection::WorkingCopy),
    );
    confirm_scene(&mut h, "confirm-push-working-copy");

    let note = commit_with(&mut h, "docs(changelog)");
    run(
        &mut h,
        "revision.rebase.onto-trunk",
        CommandArg::Revision(note),
    );
    confirm_scene(&mut h, "confirm-rebase-trunk-conflicts");

    run(
        &mut h,
        "bookmark.push",
        bookmark_arg("conflict/settings", Some("origin")),
    );
    confirm_scene(&mut h, "confirm-push-refused");

    // A bookmark that goes out as asked has nothing to confirm: this push simply
    // runs. The iced baseline shows the same, despite the scene's name.
    h.perform(Action::Bookmark(crate::bookmarks::BookmarkAction::Push {
        name: "fix/changelog-note".to_owned(),
        remote: None,
    }));
    h.shot(LANE, "confirm-push-new");
    assert!(h.read(|ui| ui.confirm().is_none()));

    run(
        &mut h,
        "bookmark.delete-everywhere",
        bookmark_arg("feature/auth", None),
    );
    // Like the push above: picked by name, so it runs without asking.
    h.shot(LANE, "confirm-delete-bookmark");
    assert!(h.read(|ui| ui.confirm().is_none()));

    run(
        &mut h,
        "file.restore",
        CommandArg::Path("src/main.rs".to_owned()),
    );
    confirm_scene(&mut h, "confirm-restore-file");

    h.perform(Action::SetMainView(MainView::History));
    h.wait_until("the operations", |ui| {
        crate::history_panel::selected_operation(ui).is_some()
    });
    run(&mut h, "history.restore-operation", CommandArg::None);
    confirm_scene(&mut h, "confirm-restore-operation");
}

#[test]
fn a_confirm_fits_a_narrow_window() {
    let mut h = Harness::open_sized(Some(&render_toolkit()), (640.0, 480.0));
    h.wait_for_graph();
    quiet_toasts(&mut h);
    h.press("a");
    confirm_scene(&mut h, "narrow-confirm");
}

#[test]
fn open_repo_scenes() {
    let mut h = open_fixture();
    let recent = repo_named("second-repo");
    h.act(|ui, _| {
        ui.recent_repos = vec![
            recent.to_string_lossy().into_owned(),
            "/Users/ada/code/acme/another-project".to_owned(),
        ];
    });
    h.press("cmd-o");
    h.shot(LANE, "open-repo-dialog");

    let typed = format!("{}/", acme().display());
    h.act(|ui, cx| ui.on_open_repo_path_changed(typed, cx));
    h.wait_until("the folder listing", |ui| {
        ui.open_repo_dialog()
            .and_then(|dialog| dialog.suggestions())
            .is_some_and(|(_, rows)| !rows.is_empty())
    });
    h.perform(Action::OpenRepoMove(1));
    h.shot(LANE, "open-repo-typed");

    h.act(|ui, cx| ui.on_open_repo_path_changed("/tmp".to_owned(), cx));
    h.perform(Action::SubmitRepoDialog);
    h.wait_until("the open error", |ui| {
        ui.open_repo_dialog()
            .is_some_and(|dialog| dialog.error.is_some())
    });
    h.shot(LANE, "open-repo-not-repo");
}

#[test]
fn shortcut_scenes() {
    let mut h = open_fixture();
    h.press("?");
    assert!(h.read(|ui| ui.mode(ModeKind::Shortcuts).is_some()));
    h.shot(LANE, "shortcuts-sheet");
    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.shot(LANE, "shortcuts-dark");
}

// ── Menus ───────────────────────────────────────────────────────────────────

/// The overlay is what off-mac platforms get; macOS never opens it on its own, so
/// the scenes and these tests raise it directly.
fn raise(h: &mut Harness, tree: Vec<MenuEntry>, anchor: AnchorSpec, glow: Option<Rectangle>) {
    h.act(|ui, _| {
        let mut menu = OverlayMenu::new(tree, anchor, None);
        menu.glow = glow;
        ui.push_mode(Mode::Menu(menu));
    });
    h.settle();
}

fn row_rect(y: f32) -> Rectangle {
    Rectangle::new(Point::new(4.0, y), Size::new(330.0, 44.0))
}

fn cursor(y: f32) -> AnchorSpec {
    AnchorSpec::At(Point::new(180.0, y))
}

#[test]
fn menu_scenes() {
    let mut h = open_fixture();
    let selection = commit_with(&mut h, "fix(render): clamp");
    let tree = h.read(|ui| ui.revision_menu_tree(&selection));
    raise(&mut h, tree.clone(), cursor(190.0), Some(row_rect(160.0)));
    h.shot(LANE, "menu-revision");

    // Hovering a submenu row opens its flyout.
    let rebase = tree
        .iter()
        .position(|entry| matches!(entry, MenuEntry::Submenu { label, .. } if label == "Rebase"))
        .expect("the rebase submenu");
    h.act(|ui, cx| ui.menu_on_hover(vec![rebase], cx));
    h.settle();
    h.shot(LANE, "menu-revision-submenu");
    h.perform(Action::DismissMenu);

    let main = commit_on(&mut h, "main");
    let tree = h.read(|ui| ui.revision_menu_tree(&main));
    raise(&mut h, tree, cursor(420.0), Some(row_rect(400.0)));
    h.shot(LANE, "menu-revision-immutable");
    h.perform(Action::DismissMenu);

    let other = commit_with(&mut h, "perf(render)");
    let ids = [selection.clone(), other].map(|revision| match revision {
        RevisionSelection::Commit(id) => id,
        RevisionSelection::WorkingCopy => unreachable!(),
    });
    h.act(|ui, _| {
        ui.active_mut().revision_multi_selection = ids.to_vec();
    });
    let tree = h.read(|ui| ui.revision_menu_tree(&selection));
    raise(&mut h, tree, cursor(190.0), Some(row_rect(160.0)));
    h.shot(LANE, "menu-revision-marked");
    h.perform(Action::DismissMenu);
    h.perform(Action::ClearMultiSelection);

    let tree = h.read(|ui| ui.file_context_menu_tree(1));
    raise(&mut h, tree, AnchorSpec::At(Point::new(420.0, 150.0)), None);
    h.shot(LANE, "menu-file");
    h.perform(Action::DismissMenu);

    let tree = h.read(|ui| ui.fetch_menu_entries());
    let fetch = Rectangle::new(Point::new(300.0, 37.0), Size::new(120.0, 28.0));
    raise(&mut h, tree, AnchorSpec::Below(fetch), None);
    h.shot(LANE, "menu-fetch");
    h.perform(Action::DismissMenu);

    let tree = h.read(|ui| ui.revset_menu_entry_tree());
    let caret = Rectangle::new(Point::new(8.0, 78.0), Size::new(300.0, 28.0));
    raise(&mut h, tree, AnchorSpec::Below(caret), None);
    h.shot(LANE, "menu-revset-presets");
}

fn one_row_menu(h: &mut Harness, opened_by: Option<gpui_kit::MouseButton>) {
    let tree = vec![MenuEntry::Item {
        label: "Wrap".to_owned(),
        detail: None,
        emphasized: false,
        command: "view.toggle-wrap",
        arg: CommandArg::None,
    }];
    h.act(|ui, _| {
        ui.push_mode(Mode::Menu(OverlayMenu::new(
            tree,
            AnchorSpec::At(Point::new(400.0, 300.0)),
            opened_by,
        )));
    });
    h.settle();
}

/// The opening press's release picks nothing, the next one does; a press outside
/// dismisses. Through real pointer events, on the path other platforms use.
#[test]
fn the_overlay_menu_swallows_only_the_opening_release() {
    use gpui_kit::MouseButton::{Left, Right};
    use gpui_kit::test::TestEventExt as _;
    let mut h = open_fixture();
    let on_row = at(420.0, 318.0);
    let wrapped = h.read(|ui| ui.diff_wrap);

    one_row_menu(&mut h, Some(Right));
    h.with_window(|window, cx| window.pointer_up(on_row, Right, cx));
    assert!(h.read(|ui| ui.menu().is_some()), "the opening release");

    h.with_window(|window, cx| {
        window.pointer_down(on_row, Left, cx);
        window.pointer_up(on_row, Left, cx);
    });
    assert!(h.read(|ui| ui.menu().is_none()), "the row picked");
    assert_ne!(h.read(|ui| ui.diff_wrap), wrapped, "and ran its command");

    one_row_menu(&mut h, None);
    h.with_window(|window, cx| window.pointer_down(at(1000.0, 700.0), Left, cx));
    assert!(
        h.read(|ui| ui.menu().is_none()),
        "a press outside dismisses"
    );
    h.with_window(|window, cx| window.pointer_up(at(1000.0, 700.0), Left, cx));
}
