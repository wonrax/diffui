//! The panels' baseline scenes: `shots/panels/<scene-id>.png`, each compared
//! against `shots/iced/<scene-id>.png`, and the panels' controls driven by real
//! clicks.

use diffui_core::{DraftSimulation, RevisionSelection};

use crate::changes::{ChangesMsg, LineMsg};
use crate::commands::CommandArg;
use crate::compare::CompareAction;
use crate::diff_view::HeaderLine;
use crate::harness::{Harness, at, fixture_repo};
use crate::range::RangeAction;
use crate::revision_list::RowSelectionKey;
use crate::theme::ThemePreference;
use crate::{Action, MainView};

const LANE: &str = "panels";
const BASELINE: (f32, f32) = (1440.0, 900.0);
const CLAMP: &str = "fix(render): clamp the sample count";
const REWORK: &str = "feat(render): rework the pipeline";

fn open() -> Harness {
    let mut h = Harness::open_sized(Some(&fixture_repo()), BASELINE);
    h.wait_for_graph();
    h.settle();
    h
}

fn shot(h: &mut Harness, scene: &str) {
    h.settle();
    h.shot(LANE, scene);
}

fn commit_with(h: &mut Harness, description: &str) -> String {
    let description = description.to_owned();
    h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .find(|row| row.description().starts_with(&description))
            .map(|row| row.commit_id().to_owned())
    })
    .unwrap_or_else(|| panic!("no revision {description}"))
}

fn loaded(h: &mut Harness) {
    h.wait_until("the diff", |ui| !ui.active().session.diff_in_flight());
    h.settle();
}

fn select(h: &mut Harness, description: &str) -> String {
    let id = commit_with(h, description);
    let key = RowSelectionKey::Commit(id.clone());
    h.act(|ui, cx| ui.on_select_row_key(key, cx));
    loaded(h);
    id
}

fn run(h: &mut Harness, command: &'static str) {
    h.act(|ui, cx| ui.run_command(command, CommandArg::None, cx));
    h.settle();
}

fn run_line(h: &mut Harness, line: &str) {
    let line = line.to_owned();
    h.act(|ui, cx| {
        ui.open_command_line(line, cx);
        ui.run_command_line(cx);
    });
    loaded(h);
}

#[test]
fn panels_collapse_and_peek() {
    let mut h = open();
    select(&mut h, CLAMP);
    h.perform(Action::ToggleHistoryPanel);
    assert!(h.read(|ui| ui.active().history_panel_collapsed));
    shot(&mut h, "panel-history-collapsed");
    h.perform(Action::ToggleFilesPanel);
    shot(&mut h, "panel-both-collapsed");
    h.perform(Action::ToggleHistoryPanel);
    shot(&mut h, "panel-files-collapsed");
    h.act(|ui, _| ui.on_changed_files_trigger_entered());
    assert!(h.read(|ui| ui.changed_files_peek_open));
    shot(&mut h, "panel-files-peek");
}

#[test]
fn the_commit_block() {
    let mut h = open();
    select(&mut h, REWORK);
    shot(&mut h, "diff-kitchen-sink");

    // The fold control, by a real click.
    h.click("commit-fold");
    assert!(h.read(|ui| ui.commit_details_collapsed));
    shot(&mut h, "diff-details-collapsed");
    h.hover_point(at(900.0, 96.0));
    shot(&mut h, "longtext-header-hover");
    h.click("commit-fold");
    assert!(!h.read(|ui| ui.commit_details_collapsed));

    // The pencil starts the description editor.
    h.click("commit-edit");
    h.settle();
    assert!(h.read(|ui| ui.active().description_editor().is_some()));
}

#[test]
fn commit_header_actions_reach_the_clipboard_and_the_graph() {
    let mut h = open();
    let id = select(&mut h, CLAMP);
    let lines = h.read(|ui| {
        let theme = ui.resolved_theme().spec();
        let details = ui
            .active()
            .session
            .revision_details
            .clone()
            .expect("details");
        crate::commit_info::header_lines(ui, &details, theme, true, 900.0)
    });
    let copy = lines.iter().find_map(|line| match line {
        HeaderLine::Rich(field) if field.label == "Commit" => Some(field.buttons[0].1.clone()),
        _ => None,
    });
    let action = crate::commit_info::header_action(copy.expect("a commit line"));
    assert!(matches!(&action, Action::Copy(value) if *value == id));
    h.perform(action);
    let clip = h.with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(clip.as_deref(), Some(id.as_str()));
}

#[test]
fn revisions_without_a_description() {
    let mut h = open();
    // The working copy of the fixture has nothing written yet.
    h.act(|ui, cx| ui.on_select_row_key(RowSelectionKey::WorkingCopy, cx));
    loaded(&mut h);
    shot(&mut h, "base-working-copy");
    let bare = h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .find(|row| !row.has_description() && !row.is_working_copy())
            .map(|row| row.commit_id().to_owned())
    });
    let key = RowSelectionKey::Commit(bare.expect("a bare revision"));
    h.act(|ui, cx| ui.on_select_row_key(key, cx));
    loaded(&mut h);
    shot(&mut h, "diff-no-description");

    // The revset moves on and leaves the selection behind.
    select(&mut h, "chore(modules): generate 240 module stubs");
    h.perform(Action::SetRevset("@".to_owned()));
    h.wait_for_graph();
    assert!(h.read(crate::empty_states::selection_outside_view));
    shot(&mut h, "selection-outside-revset");
}

#[test]
fn the_revset_field_states() {
    let mut h = open();
    h.perform(Action::SetRevset("none()".to_owned()));
    h.wait_until("the empty list", |ui| {
        let session = &ui.active().session;
        session.commits.is_empty() && matches!(session.status, crate::LoadStatus::Loaded)
    });
    shot(&mut h, "revset-empty-result");
    h.click("reset-revset");
    h.wait_for_graph();
    assert_ne!(h.read(|ui| ui.active().session.revset.clone()), "none()");

    h.perform(Action::SetRevset("nosuchsymbol(".to_owned()));
    h.settle();
    shot(&mut h, "revset-error");
}

fn draft(command: &'static str, keys: &[&str], scene: &str) -> Harness {
    let mut h = open();
    select(&mut h, CLAMP);
    run(&mut h, command);
    for key in keys {
        h.press(key);
        h.settle();
    }
    assert!(
        h.read(|ui| ui.active().op_draft().is_some()),
        "{scene} is in target mode"
    );
    shot(&mut h, scene);
    h
}

#[test]
fn op_bar_scenes() {
    for (scene, command, keys) in [
        ("draft-rebase-start", "revision.rebase.start", &[][..]),
        (
            "draft-rebase-candidate",
            "revision.rebase.start",
            &["j", "j"],
        ),
        // With a candidate armed these keys apply at once, so they come first.
        ("draft-rebase-after", "revision.rebase.start", &["a", "j"]),
        ("draft-rebase-before", "revision.rebase.start", &["b", "j"]),
        (
            "draft-rebase-descendants",
            "revision.rebase.descendants.start",
            &[],
        ),
        ("draft-rebase-branch", "revision.rebase.branch.start", &[]),
        ("draft-squash", "revision.squash.start", &["j"]),
        ("draft-merge", "revision.merge.start", &["j", "space"]),
        ("draft-new", "revision.new.start", &[]),
        ("draft-duplicate", "revision.duplicate.start", &[]),
        ("draft-parallelize", "revision.parallelize.start", &[]),
    ] {
        draft(command, keys, scene);
    }
    let mut h = draft(
        "revision.rebase.start",
        &["j", "j"],
        "draft-rebase-candidate",
    );
    h.perform(Action::SetTheme(ThemePreference::Dark));
    shot(&mut h, "draft-dark");
}

#[test]
fn the_op_bar_placement_rows_and_cancel_are_real_buttons() {
    let mut h = draft(
        "revision.rebase.start",
        &["j", "j"],
        "draft-rebase-candidate",
    );
    h.click("op-place-after");
    h.settle();
    let placement = h.read(|ui| ui.active().op_draft().expect("a draft").draft.placement);
    assert_eq!(placement, diffui_core::PlacementKind::After);
    h.click("op-bar-cancel");
    h.settle();
    assert!(h.read(|ui| ui.active().op_draft().is_none()));
}

#[test]
fn apply_is_inert_until_a_target_is_armed() {
    let mut h = draft("revision.rebase.start", &[], "draft-rebase-start");
    let before = h.read(|ui| ui.active().session.commits.len());
    h.click("op-bar-apply");
    h.settle();
    assert!(h.read(|ui| ui.active().op_draft().is_some()));
    assert_eq!(h.read(|ui| ui.active().session.commits.len()), before);
}

#[test]
fn a_verdict_that_forecasts_conflicts() {
    let mut h = open();
    let first = commit_with(&mut h, "feat(render): rework the pipeline");
    let second = commit_with(&mut h, "perf(render): cache resolved bindings");
    let target = commit_with(&mut h, "docs(changelog): note the breaking change");
    h.act(|ui, _| ui.active_mut().revision_multi_selection = vec![first, second]);
    run(&mut h, "selection.rebase.start");
    let row = h.read(|ui| {
        let tab = ui.active();
        let index = tab
            .session
            .commits
            .iter()
            .position(|row| row.commit_id() == target);
        index.and_then(|index| tab.list_row(index))
    });
    h.act(|ui, cx| ui.on_draft_hover_candidate(row, cx));
    h.wait_until("the forecast", |ui| {
        matches!(
            ui.active().op_draft().map(|draft| &draft.preview),
            Some(crate::DraftPreviewState::Ready(DraftSimulation::Rebase(preview)))
                if !preview.new_conflicts.is_empty()
        )
    });
    shot(&mut h, "draft-rebase-conflict-verdict");
}

#[test]
fn drafts_from_marks_and_from_changes() {
    let mut h = open();
    let first = commit_with(&mut h, CLAMP);
    let second = commit_with(&mut h, "perf(render): cache resolved bindings");
    h.act(|ui, _| ui.active_mut().revision_multi_selection = vec![first, second]);
    run(&mut h, "selection.rebase.start");
    assert!(h.read(|ui| ui.active().op_draft().is_some()));
    h.press("j");
    shot(&mut h, "draft-batch-sources");
    drop(h);

    let mut h = open();
    select(&mut h, CLAMP);
    h.perform(Action::Changes(ChangesMsg::ToggleFile(0)));
    h.perform(Action::Changes(ChangesMsg::MoveInto));
    h.settle();
    let carries = h.read(|ui| {
        ui.active()
            .op_draft()
            .is_some_and(|draft| draft.draft.selection.is_some())
    });
    assert!(carries);
    shot(&mut h, "draft-move-changes");
    drop(h);

    // A drag is a rebase draft with a live spot under the cursor.
    let mut h = open();
    let row = h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .position(|row| row.description().starts_with(CLAMP))
    });
    let row = row.expect("the row");
    h.act(|ui, cx| {
        ui.on_revision_drag_start(row, cx);
        ui.on_revision_drag_hover(Some(crate::revision_list::DropSpot::OnRow(row + 3)), cx);
    });
    shot(&mut h, "draft-drag");
}

#[test]
fn picks_and_line_selection() {
    let mut h = open();
    loaded(&mut h);
    h.perform(Action::Changes(ChangesMsg::ToggleFile(0)));
    shot(&mut h, "picks-files-hunks");
    h.perform(Action::Changes(ChangesMsg::Lines(LineMsg::Toggle)));
    shot(&mut h, "lines-select-mode");
    for key in ["j", "space", "shift-j", "shift-j", "space"] {
        h.press(key);
    }
    shot(&mut h, "lines-picked");
    h.perform(Action::ToggleSplit);
    shot(&mut h, "lines-split");
    h.perform(Action::ToggleSplit);
    // Done leaves the mode; the pick stays.
    h.click("lines-done");
    h.settle();
    assert!(h.read(|ui| crate::changes::live_line_select(ui.active()).is_none()));
    assert!(h.read(crate::changes::has_selection));
    h.click("picks-clear");
    h.settle();
    assert!(!h.read(crate::changes::has_selection));
}

#[test]
fn the_selection_strip_wraps_in_a_narrow_pane() {
    let mut h = Harness::open_sized(Some(&fixture_repo()), (900.0, 620.0));
    h.wait_for_graph();
    select(&mut h, CLAMP);
    h.perform(Action::Changes(ChangesMsg::ToggleFile(0)));
    h.perform(Action::Changes(ChangesMsg::Lines(LineMsg::Toggle)));
    shot(&mut h, "narrow-selection-strip");
    h.perform(Action::Changes(ChangesMsg::Lines(LineMsg::Exit)));
    h.perform(Action::Changes(ChangesMsg::Clear));

    // A revset longer than the pane: it ellipsises, the controls stay.
    run_line(
        &mut h,
        "jj diff -r 'main..feature/render-pipeline | main..fix/changelog-note | main..data/regenerate | main..feature/auth'",
    );
    assert!(h.read(|ui| crate::range::mode(ui).is_some()));
    shot(&mut h, "narrow-range-banner");
}

#[test]
fn the_split_modal() {
    let mut h = open();
    loaded(&mut h);
    h.perform(Action::Changes(ChangesMsg::ToggleFile(0)));
    h.settle();
    h.click("picks-split");
    h.settle();
    assert!(h.read(crate::changes::split_shown));
    shot(&mut h, "split-modal");

    // The first editor has the keyboard and edits reach the prompt.
    h.press("cmd-a");
    h.type_text("stays here");
    h.settle();
    let first = h.read(|ui| ui.active().changes.split_first().map(str::to_owned));
    assert_eq!(first.as_deref(), Some("stays here"));

    // A stray click on the scrim keeps what was typed; Cancel doesn't.
    h.click_point(at(20.0, 880.0));
    h.settle();
    assert!(h.read(crate::changes::split_shown));
    h.click("split-cancel");
    h.settle();
    assert!(!h.read(crate::changes::split_shown));
}

#[test]
fn discard_flows() {
    let mut h = open();
    loaded(&mut h);
    let path = h.read(|ui| ui.active().session.document.files[0].path.clone());
    h.perform(Action::Changes(ChangesMsg::ToggleFile(0)));
    h.settle();
    h.click("picks-discard");
    h.settle();
    let dialog = h.read(|ui| {
        ui.confirm()
            .map(|dialog| (format!("{:?}", dialog.title), dialog.confirm_label.clone()))
    });
    let (title, label) = dialog.expect("a confirm dialog");
    assert!(title.contains(&path), "{title}");
    assert_eq!(label, "Discard");
    shot(&mut h, "discard-confirm");
    h.perform(Action::ConfirmCancel);
    h.settle();
    assert!(h.read(|ui| ui.confirm().is_none()));
    assert!(
        h.read(crate::changes::has_selection),
        "the pick survives Cancel"
    );

    // A file row's Restore from parent is the same flow.
    h.perform(Action::Changes(ChangesMsg::RestoreFile(path)));
    h.settle();
    assert!(h.read(|ui| ui.confirm().is_some()));
    shot(&mut h, "confirm-restore-file");
}

#[test]
fn compare_scenes() {
    let mut h = open();
    let id = select(&mut h, CLAMP);
    h.perform(Action::Compare(CompareAction::WithWorkingCopy(
        RevisionSelection::Commit(id.clone()),
    )));
    loaded(&mut h);
    shot(&mut h, "compare-with-wc");

    h.click("compare-mode");
    loaded(&mut h);
    assert!(h.read(|ui| matches!(
        ui.active().session.document_base,
        diffui_core::DiffBase::Interdiff(_)
    )));
    shot(&mut h, "compare-interdiff");
    h.click("compare-mode");
    loaded(&mut h);

    let before = h.read(|ui| ui.active().session.selected_revision.clone());
    h.click("compare-swap");
    loaded(&mut h);
    assert_ne!(
        h.read(|ui| ui.active().session.selected_revision.clone()),
        before
    );

    h.click("compare-exit");
    loaded(&mut h);
    assert!(!h.read(crate::compare::comparing));

    select(&mut h, CLAMP);
    h.perform(Action::Compare(CompareAction::PickWith(
        RevisionSelection::Commit(id),
    )));
    loaded(&mut h);
    shot(&mut h, "compare-picking");
    h.perform(Action::Compare(CompareAction::Exit));
    loaded(&mut h);

    let older = commit_with(&mut h, "perf(render): cache resolved bindings");
    let newer = commit_with(&mut h, CLAMP);
    h.act(|ui, _| ui.active_mut().revision_multi_selection = vec![newer, older]);
    h.perform(Action::Compare(CompareAction::Marked));
    loaded(&mut h);
    assert!(h.read(crate::compare::comparing));
    shot(&mut h, "compare-marked");
}

fn since_trunk(h: &mut Harness, target: RevisionSelection) {
    h.perform(Action::Range(RangeAction::SinceTrunk {
        target,
        symbol: None,
    }));
    loaded(h);
}

#[test]
fn range_scenes() {
    let mut h = open();
    let id = select(&mut h, CLAMP);
    since_trunk(&mut h, RevisionSelection::Commit(id));
    assert!(h.read(|ui| crate::range::shown_range(ui).is_some()));
    shot(&mut h, "range-since-trunk");
    h.click("range-exit");
    loaded(&mut h);
    assert!(h.read(|ui| crate::range::mode(ui).is_none()));

    run_line(&mut h, "jj diff -r 'main..feature/render-pipeline'");
    assert!(h.read(|ui| crate::range::shown_range(ui).is_some()));
    shot(&mut h, "range-typed");
    h.perform(Action::Compare(CompareAction::Exit));
    loaded(&mut h);

    let trunk = h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .find(|row| row.is_immutable())
            .map(|row| row.commit_id().to_owned())
    });
    let trunk = trunk.expect("a revision in trunk");
    since_trunk(&mut h, RevisionSelection::Commit(trunk));
    assert_eq!(
        h.read(|ui| crate::range::shown_range(ui).map(|range| range.count)),
        Some(0)
    );
    shot(&mut h, "range-empty");
    h.perform(Action::Compare(CompareAction::Exit));
    loaded(&mut h);

    // Two connected rows, as a ⇧-click marks them.
    let older = commit_with(&mut h, "perf(render): cache resolved bindings");
    let newer = commit_with(&mut h, CLAMP);
    h.act(|ui, _| {
        ui.active_mut().revision_multi_selection = vec![newer, older];
        ui.sync_span(true);
    });
    loaded(&mut h);
    assert!(h.read(|ui| crate::range::refused_span(ui).is_none()));
    shot(&mut h, "range-span-marked");

    // Leaving a span leaves the marks it showed.
    h.click("range-exit");
    loaded(&mut h);
    assert!(h.read(|ui| ui.active().revision_multi_selection.is_empty()));

    // Rows with a revision missing between them can't be one diff.
    let one = commit_with(&mut h, CLAMP);
    let other = commit_with(&mut h, "feat(render): rework the pipeline");
    h.act(|ui, _| {
        ui.active_mut().revision_multi_selection = vec![one, other];
        ui.sync_span(true);
    });
    h.wait_until("the refusal", |ui| crate::range::refused_span(ui).is_some());
    shot(&mut h, "range-disconnected");
}

fn open_source(h: &mut Harness, path: &str) {
    let path = path.to_owned();
    h.act(|ui, cx| {
        let revision = crate::source_panel::browsed_revision(&ui.active().source);
        ui.perform(
            Action::BrowseSource {
                revision,
                path: Some(path),
            },
            cx,
        );
    });
    source_loaded(h);
}

fn source_loaded(h: &mut Harness) {
    h.wait_until("the source pane", |ui| {
        let source = &ui.active().source;
        source.tree.is_some() && source.loading.is_none()
    });
    h.settle();
}

#[test]
fn source_scenes() {
    let mut h = open();
    h.perform(Action::SetMainView(MainView::Source));
    source_loaded(&mut h);
    shot(&mut h, "source-opened");

    open_source(&mut h, "src/renderer/frame_loop.rs");
    assert!(h.read(|ui| ui.active().source.file.is_some()));
    shot(&mut h, "source-file");

    for (scene, path) in [
        ("source-long-lines", "src/generated/minified.js"),
        ("source-image", "assets/hero.png"),
        ("source-binary", "vendor/blob.bin"),
        ("source-cjk", "i18n/ja.txt"),
    ] {
        open_source(&mut h, path);
        shot(&mut h, scene);
    }

    open_source(&mut h, "src/renderer/frame_loop.rs");
    h.perform(Action::SetTheme(ThemePreference::Dark));
    shot(&mut h, "source-dark");
    h.perform(Action::SetTheme(ThemePreference::Light));

    run(&mut h, "source.toggle-annotate");
    source_loaded(&mut h);
    shot(&mut h, "source-annotate");
    run(&mut h, "source.toggle-annotate");

    run(&mut h, "find.open");
    shot(&mut h, "source-find");
}

#[test]
fn the_source_filter_ranks_and_opens_the_best_match() {
    let mut h = open();
    h.perform(Action::SetMainView(MainView::Source));
    source_loaded(&mut h);
    h.act(|ui, _| ui.on_source_filter_changed("frameloop".to_owned()));
    h.settle();
    shot(&mut h, "source-filter");
    h.act(|ui, _| ui.on_source_filter_submit());
    source_loaded(&mut h);
    let selected = h.read(|ui| ui.active().source.selected.clone());
    assert_eq!(selected.as_deref(), Some("src/renderer/frame_loop.rs"));
}

#[test]
fn the_source_pane_with_nothing_to_open() {
    let mut h = open();
    let empty = h.read(|ui| {
        ui.active()
            .session
            .commits
            .iter()
            .find(|row| row.is_empty() == Some(true))
            .map(|row| row.commit_id().to_owned())
    });
    let key = RowSelectionKey::Commit(empty.expect("an empty revision"));
    h.act(|ui, cx| ui.on_select_row_key(key, cx));
    loaded(&mut h);
    h.perform(Action::SetMainView(MainView::Source));
    source_loaded(&mut h);
    let selected = h.read(|ui| ui.active().source.selected.clone());
    if selected.is_none() {
        assert!(h.read(|ui| ui.active().source.file.is_none()));
    }
    shot(&mut h, "source-no-file");
}
