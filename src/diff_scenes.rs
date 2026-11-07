//! The diff surface's parity scenes against `shots/iced/scenes.md`, and what can only
//! be checked with the real element on a real document. Everything goes through the
//! real root on the fixture.

use std::path::Path;

use gpui_kit::{ScrollDelta, ScrollEvent, px};

use crate::harness::{Harness, at, fixture_repo};
use crate::theme::ThemePreference;
use crate::{Action, Diffui};
use diffui_core::RevisionSelection;

const SIZE: (f32, f32) = (1440.0, 900.0);
const LANE: &str = "diff";

fn commit_of(repo: &Path, name: &str) -> String {
    let manifest =
        std::fs::read_to_string(repo.join(".jj/fixture-manifest")).expect("the manifest");
    manifest
        .lines()
        .find_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?;
            let _change = parts.next()?;
            let commit = parts.next()?;
            names
                .split(',')
                .any(|it| it == name)
                .then(|| commit.to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} in the manifest"))
}

fn open_sized(size: (f32, f32)) -> (Harness, std::path::PathBuf) {
    let repo = fixture_repo();
    let mut h = Harness::open_sized(Some(&repo), size);
    h.wait_for_graph();
    (h, repo)
}

fn select(h: &mut Harness, repo: &Path, name: &str) {
    let commit = commit_of(repo, name);
    let selection = RevisionSelection::Commit(commit);
    h.perform(Action::SelectRevision(selection.clone()));
    h.wait_until(name, |ui| {
        let session = &ui.active().session;
        session.selected_revision == selection && !session.diff_in_flight()
    });
    h.settle();
}

fn select_file(h: &mut Harness, path: &str) {
    let index = h
        .read(|ui| {
            ui.active()
                .session
                .document
                .files
                .iter()
                .position(|file| file.path == path)
        })
        .unwrap_or_else(|| panic!("no {path} in the diff"));
    h.perform(Action::SelectFile(index));
    h.settle();
    h.frames();
}

/// One revision, optionally one file, shot under `id`.
fn scene(id: &str, name: &str, file: Option<&str>, setup: impl FnOnce(&mut Harness)) {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, name);
    setup(&mut h);
    if let Some(file) = file {
        select_file(&mut h, file);
    }
    h.frames();
    h.shot(LANE, id);
}

fn wheel(h: &mut Harness, position: (f32, f32), by: f32) {
    let position = at(position.0, position.1);
    h.with_window(|window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::Scroll(ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(gpui_kit::point(px(0.0), px(by))),
                ..Default::default()
            }),
            cx,
        );
    });
}

fn nothing(_: &mut Harness) {}

fn dark(h: &mut Harness) {
    h.perform(Action::SetTheme(ThemePreference::Dark));
}

fn split(h: &mut Harness) {
    h.perform(Action::ToggleSplit);
}

#[test]
fn kitchen_sink_file_kinds() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    h.frames();
    h.shot(LANE, "diff-kitchen-sink");
    for (id, file) in [
        ("diff-rename", "src/renderer/frame_loop.rs"),
        ("diff-deleted", "src/removed.rs"),
        ("diff-binary", "vendor/blob.bin"),
        ("diff-binary-added", "vendor/new-blob.dat"),
        ("diff-image-modified", "assets/logo.png"),
        ("diff-image-added", "assets/hero.png"),
        ("diff-svg-rendered", "assets/logo.svg"),
        ("diff-long-lines-wrap", "src/generated/minified.js"),
        ("diff-tabs", "Makefile"),
        ("diff-cjk-wrap", "i18n/ja.txt"),
        ("diff-mode-change", "scripts/run.sh"),
        ("diff-empty-file", "docs/empty-file.txt"),
    ] {
        select_file(&mut h, file);
        h.shot(LANE, id);
    }
    h.perform(Action::SetSvgSource(true));
    h.settle();
    select_file(&mut h, "assets/logo.svg");
    h.shot(LANE, "diff-svg-markup");
    h.perform(Action::SetSvgSource(false));
    h.settle();
    h.perform(Action::ToggleWrap);
    select_file(&mut h, "src/generated/minified.js");
    h.shot(LANE, "diff-long-lines-nowrap");
    select_file(&mut h, "i18n/zh.txt");
    h.shot(LANE, "diff-cjk-emoji-nowrap");
}

#[test]
fn split_modes() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    h.perform(Action::ToggleSplit);
    h.frames();
    h.shot(LANE, "split-kitchen-sink");
    select_file(&mut h, "src/generated/minified.js");
    h.shot(LANE, "split-wrap-long-lines");
    select_file(&mut h, "i18n/ja.txt");
    h.shot(LANE, "split-cjk");
    select_file(&mut h, "assets/logo.png");
    h.shot(LANE, "split-image");
    h.perform(Action::ToggleWrap);
    select_file(&mut h, "src/generated/minified.js");
    h.shot(LANE, "split-nowrap");
}

#[test]
fn dark_scenes() {
    scene("split-dark", "kitchen-sink", None, |h| {
        dark(h);
        split(h);
    });
    scene(
        "diff-kitchen-sink-dark",
        "kitchen-sink",
        Some("src/renderer/frame_loop.rs"),
        dark,
    );
}

#[test]
fn deep_path() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    let deep = h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .max_by_key(|file| file.path.len())
            .map(|file| file.path.clone())
            .unwrap()
    });
    select_file(&mut h, &deep);
    h.shot(LANE, "diff-deep-path");
}

#[test]
fn large_diff_collapses_then_expands() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "large-diff");
    h.frames();
    assert!(h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .any(|file| file.collapsed.is_some())
    }));
    h.shot(LANE, "diff-large-collapsed");
    let index = h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .position(|file| file.collapsed.is_some())
            .unwrap()
    });
    h.perform(Action::ExpandFile(index));
    h.wait_until("the full diff", |ui| {
        ui.active().session.document.files[index]
            .collapsed
            .is_none()
            && !ui.active().session.diff_in_flight()
    });
    h.settle();
    h.frames();
    wheel(&mut h, (1000.0, 500.0), -220_000.0);
    h.frames();
    h.shot(LANE, "diff-large-expanded");
}

#[test]
fn many_files() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "many-files");
    h.frames();
    h.shot(LANE, "diff-many-files");
    wheel(&mut h, (1000.0, 500.0), -9_000.0);
    h.frames();
    h.shot(LANE, "diff-many-files-scrolled");
}

#[test]
fn revision_oddities() {
    for (id, name) in [
        ("diff-merge-commit", "merge-commit"),
        ("diff-empty-revision", "empty-revision"),
        ("diff-no-description", "no-description"),
        ("diff-long-description", "long-description"),
        ("diff-immutable", "main"),
        ("diff-divergent", "divergent/take-a"),
    ] {
        scene(id, name, None, nothing);
    }
}

#[test]
fn sticky_header_selection_and_hover() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    select_file(&mut h, "src/renderer/frame_loop.rs");
    wheel(&mut h, (1000.0, 500.0), -160.0);
    h.frames();
    h.shot(LANE, "diff-scrolled-sticky");
    // Hover a hunk header, then drag across a few lines of code.
    h.hover_point(at(1000.0, 200.0));
    h.frames();
    h.shot(LANE, "diff-hunk-hover");
    h.drag(at(800.0, 300.0), at(1000.0, 400.0));
    h.frames();
    h.shot(LANE, "diff-text-selection");
}

#[test]
fn find_states() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    h.perform(Action::OpenFind);
    h.frames();
    h.shot(LANE, "find-open");
    h.act(|ui, cx| ui.find_on_query_changed("render".to_owned(), cx));
    h.wait_until("matches", |ui| {
        ui.active()
            .find()
            .is_some_and(|find| !find.matches.is_empty())
    });
    h.perform(Action::FindNext);
    h.frames();
    h.shot(LANE, "find-matches");
    h.act(|ui, cx| ui.find_on_query_changed("zzz-no-such-thing".to_owned(), cx));
    h.wait_until("no matches", |ui| {
        ui.active()
            .find()
            .is_some_and(|find| find.matches.is_empty())
    });
    h.frames();
    h.shot(LANE, "find-no-matches");
    h.act(|ui, cx| {
        ui.find_on_toggle_regex();
        ui.find_on_query_changed("ren(der".to_owned(), cx);
    });
    h.wait_until("the regex error", |ui| {
        ui.active().find().is_some_and(|find| find.error.is_some())
    });
    h.frames();
    h.shot(LANE, "find-regex-invalid");
}

#[test]
fn picks_and_lines() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "working-copy");
    let Some(file) = h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .position(|file| !file.hunks.is_empty())
    }) else {
        panic!("the working copy has a text change");
    };
    use crate::changes::{ChangesMsg, LineMsg};
    h.perform(Action::Changes(ChangesMsg::ToggleFile(file)));
    h.frames();
    h.shot(LANE, "picks-files-hunks");
    h.perform(Action::Changes(ChangesMsg::Lines(LineMsg::Toggle)));
    h.frames();
    h.shot(LANE, "lines-select-mode");
    h.press("j");
    h.press("space");
    h.press("shift-j");
    h.press("shift-j");
    h.frames();
    h.shot(LANE, "lines-picked");
    h.perform(Action::ToggleSplit);
    h.frames();
    h.shot(LANE, "lines-split");
}

#[test]
fn conflicts() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "conflicted");
    h.settle();
    h.frames();
    h.shot(LANE, "conflict-revision");
    select_file(&mut h, "src/config.rs");
    h.shot(LANE, "conflict-config-file");
    select_file(&mut h, "src/removed.rs");
    h.shot(LANE, "conflict-modify-delete");
    h.perform(Action::ToggleSplit);
    select_file(&mut h, "src/config.rs");
    h.shot(LANE, "conflict-split");
    h.perform(Action::ToggleSplit);
    h.perform(Action::SetTheme(ThemePreference::Dark));
    h.settle();
    h.frames();
    h.shot(LANE, "conflict-dark");
    h.perform(Action::SetTheme(ThemePreference::Light));
    select(&mut h, &repo, "conflict/octopus");
    h.frames();
    h.shot(LANE, "conflict-octopus");
}

#[test]
fn narrow_split_falls_back_to_unified() {
    let (mut h, repo) = open_sized((900.0, 620.0));
    select(&mut h, &repo, "kitchen-sink");
    h.perform(Action::ToggleSplit);
    h.frames();
    assert!(h.read(|ui: &Diffui| !crate::diff_panel::split_in_effect(ui)));
    h.shot(LANE, "narrow-split-fallback");
}

fn offset(h: &mut Harness) -> f32 {
    h.read(|ui| ui.active().diff_scroll_offset)
}

/// A wheel tick moves the offset, stores it on the tab, and crossing into another file
/// moves the app's selection, with no jump back when that selection comes round again.
#[test]
fn scrolling_reports_its_offset_and_the_file_under_it() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    let file = h.read(|ui| ui.active().selected_file);
    wheel(&mut h, (1000.0, 500.0), -1500.0);
    h.frames();
    assert_eq!(offset(&mut h), 1500.0);
    let moved = h.read(|ui| ui.active().selected_file);
    assert_ne!(moved, file, "1500px down is in another file");
    h.settle();
    h.frames();
    assert_eq!(offset(&mut h), 1500.0, "the selection didn't snap it back");
    // Selecting a file jumps to it.
    select_file(&mut h, "Makefile");
    assert!(offset(&mut h) > 1500.0);
}

/// Double click selects a word, a third click the line, a drag extends by the unit, and
/// ⌘C copies what is selected.
#[test]
fn multi_click_selects_and_copy_reaches_the_clipboard() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    select_file(&mut h, "src/removed.rs");
    h.frames();
    let clipboard = |h: &mut Harness| {
        h.with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    };
    // `pub fn rasterize(...` is the second row under src/removed.rs's strip.
    let spot = at(800.0, 215.0);
    h.click_point(spot);
    h.click_point(spot);
    h.press("cmd-c");
    h.settle();
    let word = clipboard(&mut h).expect("a double click selected a word");
    assert!(!word.contains(' ') && !word.is_empty(), "{word:?}");
    h.click_point(spot);
    h.press("cmd-c");
    h.settle();
    let line = clipboard(&mut h).expect("a third click selected the line");
    assert!(line.contains(&word) && line.len() > word.len(), "{line:?}");
}

/// A drag past the bottom edge keeps scrolling, frame after frame, with the pointer still.
#[test]
fn a_drag_past_the_edge_scrolls_by_itself() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    select_file(&mut h, "src/renderer/frame_loop.rs");
    h.frames();
    let before = offset(&mut h);
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        window.pointer_down(at(800.0, 400.0), gpui_kit::MouseButton::Left, cx);
        window.pointer_move(at(800.0, 960.0), Some(gpui_kit::MouseButton::Left), cx);
    });
    for _ in 0..4 {
        h.frames();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let after = offset(&mut h);
    assert!(after > before, "{before} -> {after}");
    h.with_window(|window, cx| {
        use gpui_kit::test::TestEventExt as _;
        window.pointer_up(at(800.0, 960.0), gpui_kit::MouseButton::Left, cx);
    });
    h.frames();
    let settled = offset(&mut h);
    h.frames();
    assert_eq!(offset(&mut h), settled, "releasing stops it");
}

/// The browse button in a file header opens the source view.
#[test]
fn the_browse_button_opens_the_source_view() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    select_file(&mut h, "src/renderer/frame_loop.rs");
    h.frames();
    // Left of the summary, on the header parked under the commit title: the `<>` icon.
    h.click_point(at(1295.0, 139.0));
    h.settle();
    assert_eq!(h.read(|ui| ui.active().main_view), crate::MainView::Source);
}

/// What a scroll tick costs through the whole app, root render included, against the
/// widget's own numbers from `widgets/examples/diffbench.rs`:
///   cargo test --bin diffui scroll_frame_through_the_app -- --ignored --nocapture
#[test]
#[ignore]
fn scroll_frame_through_the_app() {
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    h.frames();
    let mut samples = Vec::new();
    for step in 0..400 {
        let by = if step % 200 < 100 { -30.0 } else { 30.0 };
        let started = std::time::Instant::now();
        wheel(&mut h, (1000.0, 500.0), by);
        samples.push(started.elapsed());
    }
    samples.sort();
    eprintln!(
        "app scroll tick: p50 {:?} p95 {:?} max {:?}",
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
    );
}

/// A plain source document with a blame column, through the widget directly: the source
/// panel that hosts it belongs to another lane.
#[test]
fn plain_document_with_blame() {
    use crate::diff_view::{
        self, AnnotationGutter, AnnotationLabel, AnnotationStyle, DiffFileStatus, DiffFileView,
        DiffProps, DocSource, Slice,
    };
    use std::sync::OnceLock;
    static LABELS: OnceLock<Vec<AnnotationLabel>> = OnceLock::new();
    fn labels(_: &Diffui) -> &[AnnotationLabel] {
        LABELS.get_or_init(|| {
            (0..400)
                .map(|line| match line % 7 {
                    0 => AnnotationLabel {
                        id: "lopylyzv".to_owned(),
                        unique: 2,
                        author: "Maximiliana Wilhelmina von Hohenzollern".to_owned(),
                        age: "8mo ago".to_owned(),
                        hidden: line % 14 == 7,
                    },
                    _ => AnnotationLabel::default(),
                })
                .collect()
        })
    }
    fn first_file(ui: &Diffui, _: usize) -> &[crate::diff_view::DiffHunkView] {
        ui.active()
            .session
            .document
            .files
            .iter()
            .find(|file| file.path == "src/renderer/frame_loop.rs")
            .map_or(&[], |file| file.hunks.as_slice())
    }
    let (mut h, repo) = open_sized(SIZE);
    select(&mut h, &repo, "kitchen-sink");
    h.show(|ui, theme, window, cx| {
        let gutter = AnnotationGutter {
            labels: Slice::new(cx.entity(), labels),
            width_chars: 25,
            on_click: std::rc::Rc::new(|_, _, _| ()),
            style: AnnotationStyle {
                font: ui.config.ui_font,
                id: theme.accent,
                id_rest: theme.subtle_text,
                author: theme.muted_text,
                age: theme.subtle_text,
                hidden: theme.subtle_text,
                hover: crate::theme::chip_background(theme.muted_text),
            },
        };
        let props = DiffProps::new(
            vec![DiffFileView {
                title: String::new(),
                status: DiffFileStatus::Modified,
                status_color: theme.text,
                status_fill: theme.text,
                additions: 0,
                deletions: 0,
            }],
            DocSource::new(cx.entity(), first_file),
            0,
            "source:scene",
            crate::theme::diff_palette(theme),
            ui.config.mono_font,
            ui.config.code_type,
            ui.config.multi_click_ms,
        )
        .plain(true)
        .annotations(gutter);
        diff_view::view("source-code", props, window, cx)
    });
    h.frames();
    h.hover_point(at(60.0, 120.0));
    h.frames();
    h.shot(LANE, "plain-document-blame");
}

/// Real columns: at the baselines' 1440 the pane is under `split_fits`, so every `split-*`
/// scene there is the unified fallback.
#[test]
fn split_columns_in_a_wide_window() {
    let (mut h, repo) = open_sized((2400.0, 1000.0));
    select(&mut h, &repo, "kitchen-sink");
    h.perform(Action::ToggleSplit);
    assert!(h.read(crate::diff_panel::split_in_effect));
    select_file(&mut h, "Makefile");
    h.shot(LANE, "split-columns-wrap");
    select_file(&mut h, "i18n/ja.txt");
    h.shot(LANE, "split-columns-cjk");
    h.perform(Action::ToggleWrap);
    select_file(&mut h, "src/generated/minified.js");
    h.shot(LANE, "split-columns-nowrap");
}

/// A selection in a split column stays in that column, from text and from a padded side.
#[test]
fn split_selection_holds_its_column() {
    let (mut h, repo) = open_sized((2400.0, 1000.0));
    select(&mut h, &repo, "kitchen-sink");
    h.perform(Action::ToggleSplit);
    select_file(&mut h, "i18n/ja.txt");
    let clipboard = |h: &mut Harness| {
        h.with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    };
    // From the right column's text, down a few rows.
    h.drag(at(1700.0, 250.0), at(1800.0, 275.0));
    h.frames();
    h.press("cmd-c");
    h.settle();
    let text = clipboard(&mut h).expect("a drag in the right column selected text");
    eprintln!("right column: {text:?}");
    assert!(
        !text.contains("完了しました (混在 mixed 文字列 with ASCII)\n"),
        "{text:?}"
    );
    h.shot(LANE, "split-columns-selection");
    // From the padded left side: the selection takes the populated column.
    h.drag(at(1400.0, 300.0), at(1500.0, 420.0));
    h.frames();
    h.press("cmd-c");
    h.settle();
    let text = clipboard(&mut h).expect("a drag from padding selected text");
    eprintln!("from padding: {text:?}");
    assert!(
        text.contains("error.long") || text.contains("折り返し"),
        "{text:?}"
    );
}
