//! What the cached panes promise: a scroll or hover leaves the other regions alone, and
//! no change of state leaves one showing something stale. Plus the frame benchmark.
//!
//! Everything here dispatches raw platform events (see `Harness::event`): kit's test
//! helpers redraw the whole window around each one, which would hide a stale pane.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::InputEvent as _;
use gpui_kit::{Entity, MouseMoveEvent, ScrollDelta, ScrollEvent, point, px};

use crate::harness::{Harness, at, fixture_repo};
use crate::revision_rows::scenes::large_graph;
use crate::root::Pane;
use crate::theme::ThemePreference;
use crate::{Action, MainView, diff_view, revision_list};
use diffui_core::RevisionSelection;

const SIZE: (f32, f32) = (1440.0, 900.0);
/// Over the diff and the revision list, at the default widths.
const IN_DIFF: (f32, f32) = (1000.0, 500.0);
const IN_LIST: (f32, f32) = (150.0, 400.0);
const LIST_FIRST_ROW_Y: f32 = 130.0;

fn commit_of(repo: &Path, name: &str) -> String {
    std::fs::read_to_string(repo.join(".jj/fixture-manifest"))
        .expect("the manifest")
        .lines()
        .find_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?;
            parts.next()?;
            let commit = parts.next()?;
            names
                .split(',')
                .any(|it| it == name)
                .then(|| commit.to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} in the manifest"))
}

fn open() -> (Harness, std::path::PathBuf) {
    let repo = fixture_repo();
    let mut h = Harness::open_sized(Some(&repo), SIZE);
    h.wait_for_graph();
    (h, repo)
}

fn select(h: &mut Harness, repo: &Path, name: &str) {
    let selection = RevisionSelection::Commit(commit_of(repo, name));
    h.perform(Action::SelectRevision(selection.clone()));
    h.wait_until(name, |ui| {
        let session = &ui.active().session;
        session.selected_revision == selection && !session.diff_in_flight()
    });
    h.settle();
    h.frames();
}

/// How many times each region and the root have rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Renders {
    root: usize,
    tab_bar: usize,
    toolbar: usize,
    left: usize,
    files: usize,
    main: usize,
    floats: usize,
}

impl std::ops::Sub for Renders {
    type Output = Renders;
    fn sub(self, before: Renders) -> Renders {
        Renders {
            root: self.root - before.root,
            tab_bar: self.tab_bar - before.tab_bar,
            toolbar: self.toolbar - before.toolbar,
            left: self.left - before.left,
            files: self.files - before.files,
            main: self.main - before.main,
            floats: self.floats - before.floats,
        }
    }
}

fn renders(h: &mut Harness) -> Renders {
    let (root, panes) = h.read(|ui| (ui.renders, ui.panes.clone().expect("the panes")));
    h.with_window(|_, cx| {
        let count = |pane: &Entity<Pane>| pane.read(cx).renders;
        Renders {
            root,
            tab_bar: count(&panes.tab_bar),
            toolbar: count(&panes.toolbar),
            left: count(&panes.left),
            files: count(&panes.files),
            main: count(&panes.main),
            floats: count(&panes.floats),
        }
    })
}

/// The regions that rendered.
fn names(renders: Renders) -> Vec<&'static str> {
    [
        ("tab_bar", renders.tab_bar),
        ("toolbar", renders.toolbar),
        ("left", renders.left),
        ("files", renders.files),
        ("main", renders.main),
        ("floats", renders.floats),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(name, _)| name)
    .collect()
}

/// A wheel tick over the diff draws the diff, and no other region runs.
#[test]
fn a_diff_scroll_leaves_the_other_panes_alone() {
    let (mut h, repo) = open();
    // One long file: crossing into another is a change of state, and acts.
    select(&mut h, &repo, "large-diff");
    h.perform(Action::ExpandFile(0));
    h.wait_until("the full diff", |ui| {
        let session = &ui.active().session;
        !session.diff_in_flight() && session.document.files[0].collapsed.is_none()
    });
    h.settle();
    h.frames();
    let (paints, before) = (diff_view::PAINTS.load(Ordering::Relaxed), renders(&mut h));
    for _ in 0..20 {
        h.wheel(at(IN_DIFF.0, IN_DIFF.1), -30.0);
    }
    let delta = renders(&mut h) - before;
    assert_eq!(names(delta), ["main"], "{delta:?}");
    assert_eq!(delta.main, 20, "one render a frame, no echoes: {delta:?}");
    assert_eq!(diff_view::PAINTS.load(Ordering::Relaxed) - paints, 20);
    assert!(h.read(|ui| ui.active().diff_scroll_offset) > 500.0);
}

/// A wheel tick or a hover over the list draws the list's pane alone, with no echo on
/// the next frame even though that pane holds a retained view whose props it just set.
#[test]
fn a_list_scroll_and_hover_leave_the_other_panes_alone() {
    let (mut h, _repo) = open();
    h.perform(Action::SetRevset("all()".to_owned()));
    h.wait_for_graph();
    h.frames();
    let before = renders(&mut h);
    for _ in 0..10 {
        h.wheel(at(IN_LIST.0, IN_LIST.1), -30.0);
    }
    let delta = renders(&mut h) - before;
    assert_eq!(names(delta), ["left"], "{delta:?}");
    assert_eq!(delta.left, 10, "{delta:?}");

    let before = renders(&mut h);
    for row in 0..8 {
        h.hover_point(at(IN_LIST.0, LIST_FIRST_ROW_Y + row as f32 * 64.0));
    }
    let delta = renders(&mut h) - before;
    assert_eq!(names(delta), ["left"], "{delta:?}");

    // After all that, a frame somewhere else still leaves the list's pane cached.
    let before = renders(&mut h);
    h.wheel(at(IN_DIFF.0, IN_DIFF.1), -30.0);
    h.wheel(at(IN_DIFF.0, IN_DIFF.1), -30.0);
    assert_eq!(names(renders(&mut h) - before), ["main"]);
}

/// Nothing happening runs nothing.
#[test]
fn an_idle_window_renders_nothing() {
    let (mut h, _repo) = open();
    h.settle();
    let before = renders(&mut h);
    for _ in 0..10 {
        h.with_window(|window, cx| {
            let _ = window.draw(cx);
        });
    }
    let delta = renders(&mut h) - before;
    assert!(names(delta).is_empty(), "{delta:?}");
}

/// The check the rest of this file leans on: state changed behind the app's back shows
/// up as a difference.
#[test]
fn the_stale_check_sees_a_change_that_notified_nobody() {
    let (mut h, repo) = open();
    select(&mut h, &repo, "kitchen-sink");
    assert_eq!(h.same_after_a_refresh(), Ok(()));
    h.with_app(|ui| ui.active_mut().main_view = MainView::Source);
    assert!(
        h.same_after_a_refresh().is_err(),
        "the view switched with no notify, so the frames can't agree"
    );
}

/// Each change reaches the regions that read it, through every way state changes.
#[test]
fn no_change_leaves_a_stale_pane() {
    let (mut h, repo) = open();
    let check = |h: &mut Harness, what: &str| {
        h.settle();
        h.frames();
        h.same_after_a_refresh()
            .unwrap_or_else(|diff| panic!("{what}: {diff}"));
    };

    // The list, and what follows from its selection.
    select(&mut h, &repo, "kitchen-sink");
    check(&mut h, "selecting a revision");
    h.perform(Action::SetRevset("all()".to_owned()));
    h.wait_for_graph();
    check(&mut h, "a revset the actor answers");

    // The file tree, by key and by click, and its fold.
    h.press("j");
    check(&mut h, "the next file by key");
    h.press("x");
    check(&mut h, "a file's check");
    h.click_point(at(380.0, 150.0));
    check(&mut h, "a click in the tree");

    // The diff's own settings, from the keyboard and the toolbar's actions.
    h.perform(Action::ToggleWrap);
    check(&mut h, "wrap");
    h.perform(Action::ToggleSplit);
    check(&mut h, "split");
    h.perform(Action::ToggleCommitDetails);
    check(&mut h, "folding the details");

    // Things float, and go away again.
    h.press("cmd-k");
    check(&mut h, "the palette");
    h.type_text("render");
    check(&mut h, "typing in it");
    h.press("escape");
    check(&mut h, "closing it");
    h.press("?");
    check(&mut h, "the shortcuts");
    h.press("escape");
    h.press("cmd-f");
    check(&mut h, "the find bar");
    h.type_text("render");
    check(&mut h, "finding");
    h.press("escape");
    check(&mut h, "closing find");

    // Panes come and go and change size.
    h.perform(Action::ToggleFilesPanel);
    check(&mut h, "folding the files");
    h.perform(Action::ToggleFilesPanel);
    h.perform(Action::ToggleHistoryPanel);
    check(&mut h, "folding the history");
    h.perform(Action::ToggleHistoryPanel);
    h.act(|ui, _| ui.on_sidebar_width_changed(500.0));
    check(&mut h, "a wider sidebar");

    // Views and themes reach every region.
    for view in [MainView::Source, MainView::History, MainView::Diff] {
        h.perform(Action::SetMainView(view));
        check(&mut h, "another view");
    }
    h.perform(Action::SetTheme(ThemePreference::Dark));
    check(&mut h, "the theme");

    // Hover, on a region that's otherwise replaying, then away from it.
    h.hover_point(at(IN_LIST.0, IN_LIST.1));
    check(&mut h, "a hover on the list");
    h.hover_point(at(100.0, 74.0));
    check(&mut h, "a hover on the toolbar");
    h.hover_point(at(IN_DIFF.0, IN_DIFF.1));
    check(&mut h, "the hover leaving");

    // Another tab: every region shows other state, nothing was notified but the root.
    let second = fixture_repo();
    h.perform(Action::OpenRepo(second.to_string_lossy().into_owned()));
    h.wait_for_graph();
    check(&mut h, "a second tab");
    h.press("cmd-w");
    check(&mut h, "closing it");

    // A background result: the watcher's or a refresh's events land in an `act`.
    h.perform(Action::Refresh);
    h.wait_for_graph();
    check(&mut h, "a refresh");
}

/// A scene's retained scroll position comes back through a cached pane: the offset is
/// in the props only when a tab switch rebuilt them.
#[test]
fn a_scroll_survives_another_tab_and_back() {
    let (mut h, repo) = open();
    select(&mut h, &repo, "many-files");
    for _ in 0..10 {
        h.wheel(at(IN_DIFF.0, IN_DIFF.1), -50.0);
    }
    let offset = h.read(|ui| ui.active().diff_scroll_offset);
    assert!(offset > 0.0);
    let second = fixture_repo();
    h.perform(Action::OpenRepo(second.to_string_lossy().into_owned()));
    h.wait_for_graph();
    h.perform(Action::SelectTabIndex(0));
    h.settle();
    h.frames();
    assert_eq!(h.read(|ui| ui.active().diff_scroll_offset), offset);
    assert_eq!(h.same_after_a_refresh(), Ok(()));
}

fn percentile(sorted: &[Duration], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e3
}

fn report(name: &str, mut samples: Vec<Duration>, paints: usize, delta: Renders) {
    samples.sort();
    println!(
        "{name:<34} n={:<4} paints={paints:<4} root={:<4} main={:<4} left={:<4} others={:<4} p50={:>6.2}ms p95={:>6.2}ms p99={:>6.2}ms max={:>6.2}ms",
        samples.len(),
        delta.root,
        delta.main,
        delta.left,
        delta.tab_bar + delta.toolbar + delta.files + delta.floats,
        percentile(&samples, 0.5),
        percentile(&samples, 0.95),
        percentile(&samples, 0.99),
        percentile(&samples, 1.0),
    );
}

fn timed_wheel(h: &mut Harness, position: (f32, f32), by: f32) -> Duration {
    let position = at(position.0, position.1);
    h.timed(|window, cx| {
        window.dispatch_event(
            ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.0), px(by))),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
}

fn timed_hover(h: &mut Harness, position: gpui_kit::Point<gpui_kit::Pixels>) -> Duration {
    h.timed(|window, cx| {
        window.dispatch_event(
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
}

fn run(
    h: &mut Harness,
    name: &str,
    counter: &std::sync::atomic::AtomicUsize,
    steps: impl FnOnce(&mut Harness) -> Vec<Duration>,
) {
    let (paints, before) = (counter.load(Ordering::Relaxed), renders(h));
    let samples = steps(h);
    let delta = renders(h) - before;
    report(
        name,
        samples,
        counter.load(Ordering::Relaxed) - paints,
        delta,
    );
}

/// What a frame costs through the whole app, every panel real, on a large document and
/// a large graph, timed around the update that flushes the draw:
///   cargo test --profile fastrel --bin diffui frames_through_the_app -- --ignored --nocapture
/// `DIFFUI_UNCACHED_PANES=1` draws the panes uncached, which is the app before them.
#[test]
#[ignore = "a benchmark: run on demand, optimized"]
fn frames_through_the_app() {
    let scrolls = 400;
    let (mut h, repo) = open();
    select(&mut h, &repo, "large-diff");
    h.perform(Action::ExpandFile(0));
    h.wait_until("the full diff", |ui| {
        let session = &ui.active().session;
        !session.diff_in_flight()
            && session.document.files[0]
                .hunks
                .iter()
                .map(|hunk| hunk.lines.len())
                .sum::<usize>()
                > 30_000
    });
    // The fixture's rewrite, five times over: the 200k rows of the widget's bench.
    h.act(|ui, _| {
        let session = &mut ui.active_mut().session;
        let file = &mut session.document.files[0];
        let hunk = file.hunks[0].clone();
        for _ in 0..4 {
            file.hunks[0].lines.extend(hunk.lines.iter().cloned());
        }
        session.document_id += 1000;
        ui.document_version += 1;
    });
    h.settle();
    h.frames();
    let rows = h.read(|ui| {
        ui.active().session.document.files[0]
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len())
            .sum::<usize>()
    });
    println!("\n== a document of {rows} rows");
    run(&mut h, "diff: scroll -30px", &diff_view::PAINTS, |h| {
        (0..scrolls)
            .map(|_| timed_wheel(h, IN_DIFF, -30.0))
            .collect()
    });
    run(&mut h, "diff: jump -40000px", &diff_view::PAINTS, |h| {
        (0..100)
            .map(|_| timed_wheel(h, IN_DIFF, -40_000.0))
            .collect()
    });
    run(&mut h, "diff: hover rows", &diff_view::PAINTS, |h| {
        (0..300)
            .map(|step| timed_hover(h, at(IN_DIFF.0, 200.0 + (step % 30) as f32 * 20.0)))
            .collect()
    });
    let app_notify = |h: &mut Harness, count: usize| -> Vec<Duration> {
        (0..count)
            .map(|_| h.timed_app(|ui, cx| ui.act(cx, |_, _| {})))
            .collect()
    };
    run(&mut h, "a change of state (act)", &diff_view::PAINTS, |h| {
        app_notify(h, 100)
    });
    drop(h);

    let (mut h, _repo) = open();
    let (store, layout) = large_graph(200_000);
    h.act(|ui, _| {
        let session = &mut ui.active_mut().session;
        session.commits = store;
        session.graph = layout;
        session.sidebar_prefix_lens = Vec::new();
    });
    h.settle();
    h.frames();
    println!("\n== a graph of 200000 rows");
    run(&mut h, "list: scroll -30px", &revision_list::PAINTS, |h| {
        (0..scrolls)
            .map(|_| timed_wheel(h, IN_LIST, -30.0))
            .collect()
    });
    run(&mut h, "list: jump -40000px", &revision_list::PAINTS, |h| {
        (0..100)
            .map(|_| timed_wheel(h, IN_LIST, -40_000.0))
            .collect()
    });
    run(&mut h, "list: hover rows", &revision_list::PAINTS, |h| {
        (0..300)
            .map(|step| {
                timed_hover(
                    h,
                    at(IN_LIST.0, LIST_FIRST_ROW_Y + (step % 12) as f32 * 64.0),
                )
            })
            .collect()
    });
    run(
        &mut h,
        "a change of state (act)",
        &revision_list::PAINTS,
        |h| app_notify(h, 100),
    );
}

/// A press on a row and a move past the threshold starts a draft, through raw events
/// and with the list's pane cached until the press.
#[test]
fn a_row_drag_starts_a_draft() {
    let (mut h, _repo) = open();
    h.frames();
    let from = at(IN_LIST.0, 200.0);
    h.hover_point(from);
    h.pointer_down(from, gpui_kit::MouseButton::Left);
    for y in [210.0, 240.0, 330.0] {
        h.pointer_move(at(IN_LIST.0, y), Some(gpui_kit::MouseButton::Left));
    }
    assert!(h.read(|ui| ui.active().op_draft().is_some()), "no draft");
    h.pointer_up(at(IN_LIST.0, 330.0), gpui_kit::MouseButton::Left);
}

/// The changed files on the real list: a press on a row's box picks the file and leaves
/// the selection, a press on its name selects, and a selection far down is revealed.
#[test]
fn the_file_tree_checks_selects_and_reveals() {
    let (mut h, repo) = open();
    select(&mut h, &repo, "kitchen-sink");
    let selected = h.read(|ui| ui.active().selected_file);
    h.click_point(at(416.0, 151.0));
    h.settle();
    assert!(h.read(crate::changes::has_selection), "the box picked it");
    assert_eq!(h.read(|ui| ui.active().selected_file), selected);
    h.click_point(at(460.0, 177.0));
    h.settle();
    assert_ne!(h.read(|ui| ui.active().selected_file), selected);

    select(&mut h, &repo, "many-files");
    assert_eq!(h.read(|ui| ui.active().file_tree_scroll_offset), 0.0);
    h.perform(Action::SelectFile(200));
    h.settle();
    h.frames();
    assert!(
        h.read(|ui| ui.active().file_tree_scroll_offset) > 0.0,
        "the row came into view"
    );
}

/// The source tree on the real list, and the real code pane under it.
#[test]
fn the_source_view_is_the_real_one() {
    let (mut h, _repo) = open();
    let revision = h.read(|ui| ui.active().session.selected_revision.clone());
    h.perform(Action::BrowseSource {
        revision,
        path: Some("src/generated/minified.js".to_owned()),
    });
    h.wait_until("the file", |ui| ui.active().source.file.is_some());
    h.settle();
    h.frames();
    let before = diff_view::PAINTS.load(Ordering::Relaxed);
    h.wheel(at(IN_DIFF.0, IN_DIFF.1), -60.0);
    assert_eq!(diff_view::PAINTS.load(Ordering::Relaxed) - before, 1);
    assert!(h.read(|ui| ui.active().source.scroll_offset) > 0.0);
    h.wheel(at(IN_LIST.0, IN_LIST.1), -60.0);
    assert_eq!(h.same_after_a_refresh(), Ok(()));
}
