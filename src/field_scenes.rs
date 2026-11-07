//! When a text field holds the keyboard and what its keys do, through the real
//! root view and raw platform events.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::AnyInputState;

use crate::MainView;
use crate::harness::{Harness, at, fixture_repo};

/// Blank toolbar, past the last button on the left: not a list, not the diff.
const BLANK_TOOLBAR: (f32, f32) = (900.0, 37.0);
/// Inside the revset box.
const IN_REVSET_BOX: (f32, f32) = (100.0, 66.0);
/// Over the palette's results.
const IN_PALETTE_RESULTS: (f32, f32) = (720.0, 300.0);

fn app() -> Harness {
    let repo = fixture_repo();
    let mut h = Harness::open_sized(Some(&repo), (1440.0, 900.0));
    h.wait_for_graph();
    h
}

fn focused_revset_box() -> Harness {
    let mut h = app();
    h.act(|ui, cx| {
        ui.active_mut().session.revset = String::new();
        crate::input::focus(crate::sidebar::REVSET_INPUT_ID, cx);
    });
    h.settle();
    assert!(field_focused(&mut h), "the revset box took focus");
    h
}

fn field_focused(h: &mut Harness) -> bool {
    h.with_window(|window, _| crate::input::input_has_focus(window))
}

fn field_text(h: &mut Harness) -> String {
    h.with_window(|window, cx| match window.focused_input(cx) {
        Some(AnyInputState::Input(state)) => state.read(cx).value().to_string(),
        Some(AnyInputState::Textarea(state)) => state.read(cx).value().to_string(),
        Some(AnyInputState::Editor(state)) => state.read(cx).value().to_string(),
        _ => panic!("no text field has focus"),
    })
}

/// How many pixels differ between two frames.
fn changed_pixels(first: &image::RgbaImage, second: &image::RgbaImage) -> usize {
    first
        .pixels()
        .zip(second.pixels())
        .filter(|(a, b)| a != b)
        .count()
}

#[test]
fn a_press_elsewhere_takes_the_keyboard_from_a_field() {
    let mut h = focused_revset_box();
    h.click_point(at(BLANK_TOOLBAR.0, BLANK_TOOLBAR.1));
    h.settle();
    assert!(!field_focused(&mut h), "the field let go");

    // So the next plain key is the keymap's.
    let before = h.read(|ui| ui.active().session.revset.clone());
    h.press("j");
    h.settle();
    assert_eq!(h.read(|ui| ui.active().session.revset.clone()), before);
}

#[test]
fn a_press_inside_a_field_keeps_the_keyboard() {
    let mut h = focused_revset_box();
    h.type_text("abc");
    h.settle();
    h.click_point(at(IN_REVSET_BOX.0, IN_REVSET_BOX.1));
    h.settle();
    assert!(field_focused(&mut h), "still the field's");
    assert_eq!(field_text(&mut h), "abc");
}

#[test]
fn a_right_press_elsewhere_leaves_the_field_alone() {
    let mut h = focused_revset_box();
    let point = at(BLANK_TOOLBAR.0, BLANK_TOOLBAR.1);
    h.pointer_down(point, gpui_kit::MouseButton::Right);
    h.pointer_up(point, gpui_kit::MouseButton::Right);
    h.settle();
    assert!(field_focused(&mut h));
}

#[test]
fn a_completion_in_the_middle_keeps_the_caret_there() {
    let mut h = app();
    h.press(":");
    h.settle();
    h.type_text("git pu origin");
    h.settle();
    for _ in 0.." origin".len() {
        h.press("left");
    }
    h.settle();
    assert!(
        h.read(|ui| !ui.command_line().unwrap().completions.is_empty()),
        "something to complete under the caret"
    );
    h.press("tab");
    h.settle();
    let (line, cursor) = h.read(|ui| {
        let column = ui.palette().unwrap().top().unwrap();
        (column.query.clone(), ui.command_line().unwrap().cursor)
    });
    assert!(line.ends_with("origin"), "{line}");
    assert!(cursor < line.len() - "origin".len() + 1, "{cursor} in {line}");

    // Typing lands at the caret too, not at the end of the line.
    h.type_text("X");
    h.settle();
    let typed = h.read(|ui| ui.palette().unwrap().top().unwrap().query.clone());
    assert!(typed.ends_with("origin"), "{typed}");
    assert!(typed.contains('X'), "{typed}");
}

#[cfg(target_os = "macos")]
#[test]
fn the_cocoa_chords_edit_a_field() {
    let mut h = focused_revset_box();
    h.type_text("abc");
    h.settle();

    h.press("ctrl-b");
    h.settle();
    assert_eq!(h.read(|ui| ui.active().cmdline.field.cursor), 2, "⌃B");
    h.type_text("X");
    h.settle();
    assert_eq!(field_text(&mut h), "abXc", "⌃B moved the real caret");

    h.press("ctrl-f");
    h.press("ctrl-h");
    h.settle();
    assert_eq!(field_text(&mut h), "abX", "⌃F then ⌃H");

    h.press("ctrl-a");
    h.press("ctrl-d");
    h.settle();
    assert_eq!(field_text(&mut h), "bX", "⌃A then ⌃D");

    h.press("ctrl-k");
    h.settle();
    assert_eq!(field_text(&mut h), "", "⌃K");
}

#[cfg(target_os = "macos")]
#[test]
fn shift_extends_the_cocoa_motions() {
    let mut h = focused_revset_box();
    h.type_text("abc");
    h.settle();
    h.press("ctrl-shift-b");
    h.press("ctrl-h");
    h.settle();
    assert_eq!(field_text(&mut h), "ab", "⌃⇧B selected what ⌃H took");
}

#[test]
fn escape_in_a_field_only_lets_go_of_it() {
    let mut h = app();
    h.press("space");
    h.settle();
    let marked = h.read(|ui| ui.active().revision_multi_selection.len());
    assert!(marked > 0, "a mark to keep");
    h.act(|_, cx| crate::input::focus(crate::sidebar::REVSET_INPUT_ID, cx));
    h.settle();
    assert!(field_focused(&mut h));

    h.press("escape");
    h.settle();
    assert!(!field_focused(&mut h), "the first Esc lets go");
    assert_eq!(
        h.read(|ui| ui.active().revision_multi_selection.len()),
        marked,
        "and leaves the marks"
    );

    h.press("escape");
    h.settle();
    assert_eq!(
        h.read(|ui| ui.active().revision_multi_selection.len()),
        0,
        "the second is the keymap's"
    );
}

#[test]
fn a_new_query_returns_a_wheeled_palette_to_the_top() {
    let mut h = app();
    h.press("cmd-k");
    h.settle();
    h.frames();
    let top = h.screenshot();

    h.wheel(at(IN_PALETTE_RESULTS.0, IN_PALETTE_RESULTS.1), -300.0);
    h.settle();
    h.frames();
    let scrolled = h.screenshot();
    assert!(changed_pixels(&top, &scrolled) > 1000, "the wheel moved it");

    // A query and back: the same rows, so the same frame if it went to the top.
    h.type_text("a");
    h.settle();
    h.press("backspace");
    h.settle();
    h.frames();
    let back = h.screenshot();
    let moved = changed_pixels(&top, &back);
    assert!(moved < 200, "{moved} pixels off the top of the list");
}

#[test]
fn source_and_history_keep_their_width_with_the_panel_collapsed() {
    let mut h = app();
    h.act(|ui, _| ui.active_mut().history_panel_collapsed = true);
    h.settle();
    let collapsed = crate::theme::COLLAPSED_PANEL_WIDTH;
    assert_eq!(h.read(|ui| ui.left_pane_width()), collapsed, "the diff's");
    for view in [MainView::Source, MainView::History] {
        h.act(|ui, _| ui.active_mut().main_view = view);
        h.settle();
        let (width, sidebar) = h.read(|ui| (ui.left_pane_width(), ui.sidebar_width));
        assert_eq!(width, sidebar, "{view:?}");
    }
}
