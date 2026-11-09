use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::WindowTextSystem;

use super::layout::*;
use super::line::*;
use super::paint::{CodeCache, Plan};
use super::*;
use crate::geometry::{Point, Rectangle, Size};
use crate::hunk_actions::{Button, FileStrip, Row, Run, Style, Tool};
use crate::measure;
use crate::theme::{BLACK, TRANSPARENT, WHITE};
use crate::typography::TextBox;

/// The typography config drives the layout metrics: the text's caps are centred in
/// the row whatever its height, and the baseline offset shifts them verbatim.
#[test]
fn layout_metrics_follow_code_typography() {
    let metrics = |typography| LayoutMetrics::new(typography, 3, Font::MONOSPACE, false);

    let default = metrics(CodeTypography::default());
    assert_eq!(default.row_height, 22.0, "12px × 1.85 rounds to 22");
    // The caps sit centred in the row: the shift comes from the font.
    let trim = TextBox::new(Font::MONOSPACE, 12.0, 22.0).trim_shift();
    assert_eq!(default.text_y_pad, trim);

    let airy = metrics(CodeTypography {
        line_height: 2.35,
        ..CodeTypography::default()
    });
    assert_eq!(airy.row_height, 28.0, "12px × 2.35 rounds to 28");
    // Extra leading is centred by the line box itself; the shift that centres the
    // caps doesn't depend on it.
    assert!((airy.text_y_pad - trim).abs() < 1e-4);

    let nudged = metrics(CodeTypography {
        baseline_offset: -1.5,
        ..CodeTypography::default()
    });
    assert_eq!(nudged.row_height, default.row_height);
    assert_eq!(nudged.text_y_pad, trim - 1.5);

    let bigger = metrics(CodeTypography {
        size: 16.0,
        ..CodeTypography::default()
    });
    assert_eq!(bigger.row_height, 30.0, "16px × 1.85 rounds to 30");
    // The shift scales with the size.
    let scaled = TextBox::new(Font::MONOSPACE, 16.0, 30.0).trim_shift();
    assert_eq!(bigger.text_y_pad, scaled);
}

fn test_palette() -> Palette {
    let c = WHITE;
    Palette {
        text: c,
        text_muted: c,
        addition_text: c,
        deletion_text: c,
        modified_token: c,
        conflict_marker: c,
        note_text: c,
        syntax_keyword: c,
        syntax_type: c,
        syntax_function: c,
        syntax_literal: c,
        syntax_property: c,
        panel: c,
        file_header: c,
        hunk_header: c,
        addition_background: c,
        deletion_background: c,
        addition_emphasis: c,
        deletion_emphasis: c,
        note_background: c,
        gutter_background: c,
        border: c,
        selection: c,
        accent: c,
    }
}

fn line(kind: DiffLineKind, content: &str, n: usize) -> DiffLine {
    DiffLine {
        kind,
        old_line: Some(n),
        new_line: Some(n),
        content: content.to_owned(),
        syntax: Vec::new(),
        emphasis: Vec::new(),
    }
}

/// A doc with several files/hunks and one long line that wraps, so the index has
/// non-uniform row heights to account for.
fn test_hunks() -> Vec<Vec<DiffHunkView>> {
    let long = "x".repeat(500);
    vec![
        vec![DiffHunkView {
            header: "@@ -1,3 +1,3 @@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "alpha", 1),
                line(DiffLineKind::Addition, &long, 2),
                line(DiffLineKind::Deletion, "gamma", 3),
            ],
        }],
        vec![
            DiffHunkView {
                header: "@@ -10,2 +10,2 @@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Context, "delta", 10),
                    line(DiffLineKind::Addition, "", 11),
                ],
            },
            DiffHunkView {
                header: "@@ -40,1 +40,1 @@".to_owned(),
                lines: vec![line(DiffLineKind::Context, "epsilon", 40)],
            },
        ],
    ]
}

fn test_props(files: usize) -> DiffProps {
    DiffProps::new(
        (0..files)
            .map(|i| DiffFileView {
                title: format!("file-{i}"),
                status: DiffFileStatus::Modified,
                status_color: WHITE,
                status_fill: TRANSPARENT,
                additions: 1,
                deletions: 1,
                tag: None,
                note: None,
            })
            .collect(),
        DocSource::empty(),
        0,
        "test",
        test_palette(),
        Font::MONOSPACE,
        CodeTypography {
            size: 13.0,
            ..CodeTypography::default()
        },
        500,
    )
}

fn view_of<'a>(props: &'a DiffProps, hunks: &'a [Vec<DiffHunkView>]) -> View<'a> {
    View::new(props, hunks.iter().map(Vec::as_slice).collect(), &[], &[])
}

/// The two caches the view keeps in its state, so a test can drive the layout the
/// way a frame does.
#[derive(Default)]
struct TestCaches {
    index: RefCell<HeightIndex>,
    geometry: RefCell<GeometryMemo>,
}

/// Brute-force row walk mirroring the pre-index geometry, used as the oracle the
/// prefix sums must agree with.
fn brute_force_tops(view: &View<'_>, content_width: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>, f32) {
    let memo = RefCell::new(GeometryMemo::default());
    let wrap_width = view.wrap_width(content_width);
    let mut file_tops = Vec::new();
    let mut row_tops = Vec::new();
    let mut row_heights = Vec::new();
    let mut y = view.header_height();
    for (file_index, hunks) in view.hunks.iter().enumerate() {
        file_tops.push(y);
        y += view.metrics.file_header_height;
        for (hunk_index, hunk) in hunks.iter().enumerate() {
            y += view.metrics.hunk_header_height;
            for (line_index, l) in hunk.lines.iter().enumerate() {
                let key = body_geometry_key(
                    file_index,
                    hunk_index,
                    line_index,
                    wrap_width,
                    view.metrics.char_width,
                );
                let height = view.row_height(&memo, key, l, wrap_width);
                row_tops.push(y);
                row_heights.push(height);
                y += height;
            }
        }
    }
    (file_tops, row_tops, row_heights, y)
}

#[test]
fn height_index_matches_brute_force_walk() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let width = 400.0;
    let content_width = view.content_width(width);

    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, width);
    let index = cells.index.borrow();

    let (file_tops, row_tops, row_heights, total) = brute_force_tops(&view, content_width);
    assert_eq!(&index.file_tops[..file_tops.len()], file_tops.as_slice());
    assert_eq!(index.file_tops.last().copied(), Some(total));
    assert_eq!(index.row_tops, row_tops);
    assert_eq!(index.row_heights, row_heights);
    assert_eq!(index.total_height, total);
    // The long line wraps: its row is taller than one row height, so the following
    // row's top reflects the wrap.
    assert!(index.row_tops[2] - index.row_tops[1] > view.metrics.row_height * 1.5);

    // file_offset / file_at_offset agree with the prefix sums.
    assert_eq!(view.file_offset(&index, 0), file_tops[0]);
    assert_eq!(view.file_offset(&index, 1), file_tops[1]);
    assert_eq!(view.file_at_offset(&index, 0.0), 0);
    assert_eq!(view.file_at_offset(&index, file_tops[1] - 1.0), 0);
    assert_eq!(view.file_at_offset(&index, file_tops[1]), 1);
    assert_eq!(view.file_at_offset(&index, total + 100.0), 1);

    // Row lookup by id and by y agree with the walk.
    for (i, &(f, h, l)) in index.row_ids.iter().enumerate() {
        assert_eq!(
            view.match_target_y(
                &index,
                &cells.geometry,
                width,
                (f as usize, h as usize, l as usize, 0)
            ),
            Some(index.row_tops[i]),
        );
        assert_eq!(index.row_at(index.row_tops[i] + 0.5), Some(i));
    }
    // A y above the first row (in the file-0 header band) has no candidate row.
    assert_eq!(index.row_at(index.row_tops[0] - 1.0), None);
}

/// File 1 conflicted: a strip with one tool and two take buttons.
fn conflict_strip(file: usize) -> HunkActions {
    let run = |text: &str| Run {
        text: text.to_owned(),
        font: Font::DEFAULT,
        color: BLACK,
    };
    HunkActions {
        rows: Vec::new(),
        style: Style {
            size: 12.0,
            fill: WHITE,
            hover_fill: WHITE,
            ghost_hover_fill: WHITE,
            border: BLACK,
            focus: BLACK,
            on_fill: BLACK,
            radius: 4.0,
        },
        on_press: Rc::new(|_, _, _| ()),
        files: vec![FileStrip {
            file,
            label: "file conflict, 2 sides".to_owned(),
            tools: vec![Tool {
                runs: vec![run("Edit markers")],
                enabled: true,
            }],
            row: Row {
                file,
                hunk: 0,
                focused: false,
                lead: Some(run("Take all")),
                buttons: vec![
                    Button {
                        runs: vec![run("one")],
                        tone: crate::hunk_actions::Tone::Raised,
                    },
                    Button {
                        runs: vec![run("two")],
                        tone: crate::hunk_actions::Tone::Raised,
                    },
                ],
                hover: false,
            },
        }],
        on_file_press: Rc::new(|_, _, _| ()),
        reveal: None,
    }
}

#[test]
fn a_file_strip_sits_between_its_header_and_first_hunk() {
    let hunks = test_hunks();
    let width = 800.0;
    let plain_props = test_props(hunks.len());
    let plain = TestCaches::default();
    view_of(&plain_props, &hunks).ensure_height_index(&plain.index, &plain.geometry, width);
    let plain = plain.index.borrow();

    let mut props = test_props(hunks.len());
    props.hunk_actions = Some(conflict_strip(1));
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, width);
    let index = cells.index.borrow();
    let strip = view.metrics.hunk_header_height;

    // File 0 is untouched; file 1's header, then its strip, then its hunks, everything
    // after moved down by one strip.
    assert_eq!(index.file_tops[..2], plain.file_tops[..2]);
    let strip_top = index.file_tops[1] + view.metrics.file_header_height;
    assert_eq!(index.file_strips, vec![(1, strip_top)]);
    let first_hunk = index.hunk_ids.iter().position(|id| id.0 == 1).unwrap();
    assert_eq!(index.hunk_tops[first_hunk], strip_top + strip);
    assert_eq!(index.total_height, plain.total_height + strip);
    for (row, top) in index.row_tops.iter().enumerate() {
        let shift = if index.row_ids[row].0 == 1 {
            strip
        } else {
            0.0
        };
        assert_eq!(*top, plain.row_tops[row] + shift);
    }

    // Its buttons hit-test where they're laid out; the rest of the strip, and file 1's
    // hunk strip (no region buttons), don't.
    let bounds = Rectangle::new(Point::ORIGIN, Size::new(width, 600.0));
    let placed = view
        .file_strip_layout(&index, 0, strip_top, bounds)
        .unwrap();
    let center = |rect: Rectangle| Point::new(rect.center_x(), rect.center_y());
    let hit = |point| view.strip_action_at(&index, bounds, 0.0, point);
    assert_eq!(
        hit(center(placed.tools[0].rect)),
        Some(ActionHit::File(0, 0))
    );
    assert_eq!(
        hit(center(placed.buttons[1].rect)),
        Some(ActionHit::File(0, 2))
    );
    assert!(placed.tools[0].rect.x + placed.tools[0].rect.width < placed.buttons[0].rect.x);
    assert_eq!(
        hit(Point::new(bounds.x + 2.0, strip_top + strip / 2.0)),
        None
    );
    let hunk_y = index.hunk_tops[first_hunk] + 2.0;
    assert_eq!(
        hit(Point::new(placed.buttons[1].rect.center_x(), hunk_y)),
        None
    );

    // Scrolled so file 1's header is pinned over the strip: the header wins.
    let offset = strip_top - 4.0;
    let under_header = Point::new(center(placed.buttons[1].rect).x, 6.0);
    assert_eq!(
        view.strip_action_at(&index, bounds, offset, under_header),
        None
    );
}

#[test]
fn height_index_rebuilds_when_the_file_strips_change() {
    let hunks = test_hunks();
    let mut props = test_props(hunks.len());
    let cells = TestCaches::default();
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 800.0);
    let total = cells.index.borrow().total_height;
    props.hunk_actions = Some(conflict_strip(0));
    let view = view_of(&props, &hunks);
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);
    assert_eq!(
        cells.index.borrow().total_height,
        total + view.metrics.hunk_header_height
    );
    assert_eq!(cells.index.borrow().file_strips[0].0, 0);
    // Plain documents draw no strips.
    props.plain = true;
    assert_eq!(view_of(&props, &hunks).file_strip_height(0), 0.0);
}

#[test]
fn a_plain_document_insets_its_code_alike_from_the_gutter_and_the_top() {
    let hunks = test_hunks();
    let props = test_props(hunks.len()).plain(true);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);
    let index = cells.index.borrow();
    // The first row starts below the inset, and its caps sit PLAIN_INSET below the top,
    // as the code sits PLAIN_INSET right of the gutter.
    assert_eq!(index.row_tops[0], view.metrics.top_inset);
    let caps = TextBox::new(props.font, 13.0, view.metrics.row_height).cap_height();
    let cap_top = view.metrics.top_inset + (view.metrics.row_height - caps) / 2.0;
    assert!((cap_top - PLAIN_INSET).abs() < 1e-3, "{cap_top}");
    assert_eq!(view.metrics.prefix_width + TEXT_X_PADDING, PLAIN_INSET);
    // A diff keeps its prefix column and starts at the top.
    let diff_props = test_props(hunks.len());
    let diff = view_of(&diff_props, &hunks);
    assert_eq!(diff.metrics.prefix_width, PREFIX_WIDTH);
    assert_eq!(diff.metrics.top_inset, 0.0);
}

#[test]
fn a_blame_run_spans_its_commits_lines_and_skips_hidden_ones() {
    let entry = |id: &str, hidden| AnnotationLabel {
        id: id.to_owned(),
        hidden,
        ..AnnotationLabel::default()
    };
    let labels = [
        entry("aaaa", false),
        entry("", false),
        entry("", false),
        entry("bbbb", true),
        entry("", true),
        entry("cccc", false),
    ];
    assert_eq!(annotation_run(&labels, 0), Some((0, 3)));
    assert_eq!(annotation_run(&labels, 2), Some((0, 3)));
    assert_eq!(annotation_run(&labels, 4), None);
    assert_eq!(annotation_run(&labels, 5), Some((5, 6)));
    assert_eq!(annotation_run(&labels, 6), None);
}

#[test]
fn height_index_rebuilds_on_shape_or_width_change() {
    let hunks = test_hunks();
    let mut props = test_props(hunks.len());
    let cells = TestCaches::default();

    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 400.0);
    let narrow_total = cells.index.borrow().total_height;
    let narrow_key = cells.index.borrow().key;

    // Wider viewport: the 500-char line wraps fewer times, so the total shrinks and the
    // key changes.
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 4000.0);
    assert_ne!(cells.index.borrow().key, narrow_key);
    assert!(cells.index.borrow().total_height < narrow_total);

    // Same shape + width: cache hit, key stable.
    let key = cells.index.borrow().key;
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 4000.0);
    assert_eq!(cells.index.borrow().key, key);

    // A replaced document (new layout id) must rebuild.
    props.layout_version = 7;
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 4000.0);
    assert_ne!(cells.index.borrow().key, key);

    // With wrapping off nothing in the layout depends on the width, so dragging the
    // sidebar (a new width every frame) must not rebuild.
    props.wrap = false;
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 4000.0);
    let no_wrap_key = cells.index.borrow().key;
    let no_wrap_total = cells.index.borrow().total_height;
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 900.0);
    assert_eq!(cells.index.borrow().key, no_wrap_key);
    assert_eq!(cells.index.borrow().total_height, no_wrap_total);
}

#[test]
fn header_spacer_reserves_variable_height_and_maps_following_rows() {
    let hunks = test_hunks();
    let props = test_props(hunks.len()).with_header(vec![
        HeaderLine::spacer(150.0),
        HeaderLine::field("commit", "abc"),
    ]);
    let view = view_of(&props, &hunks);

    assert_eq!(
        view.header_height(),
        HEADER_VERTICAL_PADDING * 2.0 + 150.0 + view.metrics.row_height
    );
    assert_eq!(
        view.header_line_at_y(HEADER_VERTICAL_PADDING + 149.0),
        Some(0)
    );
    assert_eq!(
        view.header_line_at_y(HEADER_VERTICAL_PADDING + 150.0),
        Some(1)
    );
}

#[test]
fn side_by_side_pairs_runs_and_pads() {
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Context, "ctx", 1),
            line(DiffLineKind::Deletion, "old a", 2),
            line(DiffLineKind::Deletion, "old b", 3),
            line(DiffLineKind::Addition, "new a", 2),
            line(DiffLineKind::Addition, "new b", 3),
            line(DiffLineKind::Addition, "new c", 4),
            line(DiffLineKind::Context, "tail", 5),
        ],
    }]];
    let props = test_props(1).side_by_side(true);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);

    let index = cells.index.borrow();
    // ctx + 3 pairs (2 del × 3 add) + tail.
    assert_eq!(index.row_tops.len(), 5);
    assert_eq!(
        index.pair_lines,
        vec![(0, 0), (1, 3), (2, 4), (NO_LINE, 5), (6, 6)]
    );
    // Reps stay sorted, and the padded pair is keyed by its right member.
    assert_eq!(
        index.row_ids,
        vec![(0, 0, 0), (0, 0, 1), (0, 0, 2), (0, 0, 5), (0, 0, 6)]
    );
    // Both members of a pair resolve to the same row.
    assert_eq!(index.row_of_line(0, 0, 3), Some(1));
    assert_eq!(index.row_of_line(0, 0, 1), Some(1));
    assert_eq!(index.row_of_line(0, 0, 5), Some(3));
    assert_eq!(index.row_of_line(0, 0, 6), Some(4));
}

#[test]
fn side_by_side_addition_only_runs_pad_left() {
    // Additions with no deletion run in front belong to the right column alone: they
    // must not mirror into the left side.
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Context, "ctx", 1),
            line(DiffLineKind::Addition, "new a", 2),
            line(DiffLineKind::Addition, "new b", 3),
            line(DiffLineKind::Context, "tail", 4),
        ],
    }]];
    let props = test_props(1).side_by_side(true);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);

    let index = cells.index.borrow();
    assert_eq!(
        index.pair_lines,
        vec![(0, 0), (NO_LINE, 1), (NO_LINE, 2), (3, 3)]
    );
}

#[test]
fn side_by_side_single_sided_files_render_full_width() {
    // File 0 is additions-only (a new file): it renders as one full-width column. File 1
    // mixes kinds and stays split.
    let long = "x".repeat(120);
    let hunks = vec![
        vec![DiffHunkView {
            header: "@@".to_owned(),
            lines: vec![
                line(DiffLineKind::Addition, "a", 1),
                line(DiffLineKind::Addition, &long, 2),
            ],
        }],
        vec![DiffHunkView {
            header: "@@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "ctx", 1),
                line(DiffLineKind::Deletion, "old", 2),
                line(DiffLineKind::Addition, "new", 2),
            ],
        }],
    ];
    let props = test_props(2).side_by_side(true);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);

    let index = cells.index.borrow();
    let unified_width = view.content_width(800.0);
    let split_width = view.effective_text_width(800.0);
    assert_eq!(index.full_width_files, vec![true, false]);
    // Full-width rows keep `pair_lines` aligned via identity pairs; the mixed file still
    // pairs its deletion/addition run.
    assert_eq!(index.pair_lines, vec![(0, 0), (1, 1), (0, 0), (1, 2)]);
    // Full-width rows measure against the whole text area, not a column: the long line
    // wraps less (or not at all) compared to a split row.
    assert!(unified_width > split_width);
    let long_height = |wrap_width: f32| {
        let memo = RefCell::new(GeometryMemo::default());
        let key = body_geometry_key(0, 0, 1, wrap_width, view.metrics.char_width);
        view.visual_rows(&memo, key, &long, wrap_width) as f32 * view.metrics.row_height
    };
    assert_eq!(view.index_row_height(&index, 1), long_height(unified_width));
    assert!(long_height(split_width) > view.index_row_height(&index, 1));
}

#[test]
fn split_selection_copies_only_its_column() {
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Context, "ctx", 1),
            line(DiffLineKind::Deletion, "old", 2),
            line(DiffLineKind::Addition, "new", 2),
            line(DiffLineKind::Context, "tail", 3),
        ],
    }]];
    let props = test_props(1).side_by_side(true);
    let view = view_of(&props, &hunks);
    let start = body_position(0, 0, 0, 0);
    let end = body_position(0, 0, 3, 4);
    assert_eq!(
        view.collect_selected_text(start, end, Some(SplitSide::Right)),
        "ctx\nnew\ntail"
    );
    assert_eq!(
        view.collect_selected_text(start, end, Some(SplitSide::Left)),
        "ctx\nold\ntail"
    );
    // No side (unified mode): everything in range, both columns.
    assert_eq!(
        view.collect_selected_text(start, end, None),
        "ctx\nold\nnew\ntail"
    );
}

#[test]
fn no_wrap_makes_rows_uniform_height() {
    let hunks = test_hunks();
    let mut props = test_props(hunks.len());
    let cells = TestCaches::default();

    // Narrow viewport: the 500-char line wraps, so heights are mixed.
    view_of(&props, &hunks).ensure_height_index(&cells.index, &cells.geometry, 400.0);
    let wrapped_key = cells.index.borrow().key;
    let wrapped_total = cells.index.borrow().total_height;

    // Wrap off: same width, new key, every row exactly one line tall.
    props.wrap = false;
    let view = view_of(&props, &hunks);
    view.ensure_height_index(&cells.index, &cells.geometry, 400.0);
    assert_ne!(cells.index.borrow().key, wrapped_key);
    assert!(cells.index.borrow().total_height < wrapped_total);
    let index = cells.index.borrow();
    for i in 1..index.row_tops.len() {
        let delta = index.row_tops[i] - index.row_tops[i - 1];
        // Consecutive rows within one hunk sit exactly one row apart; larger gaps are
        // hunk/file header bands.
        assert!((delta - view.metrics.row_height).abs() < 0.01 || delta > view.metrics.row_height);
    }
    assert!(
        index
            .row_heights
            .iter()
            .all(|height| (height - view.metrics.row_height).abs() < 0.01)
    );
}

/// The break points the geometry hands the painter must be the ones the engine would
/// have picked. ASCII lines don't ask it, so this pins the grid's copy of the rule to the
/// real thing, trailing blanks and oversized words included.
#[test]
fn geometry_starts_match_the_engine() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let memo = RefCell::new(GeometryMemo::default());
    let cw = view.metrics.char_width;
    let long = "x".repeat(200);
    let corpus = [
        "\tif err != nil {",
        "\t\treturn fmt.Errorf(\"read %q: %w\", path, err) // a long tail",
        "日本語のテキストが折り返される場合の折り返し位置を確かめる",
        "мешанина of Cyrillic and ASCII words wrapping somewhere",
        &long,
        "one two three four five six seven eight nine ten eleven twelve",
        "    indented, with punctuation: foo_bar(baz)->quux[0] != <|> /usr/lib/x-y",
        "https://example.com/some/long/path?with=queries&and=params",
        "ends with spaces      ",
        "      leading spaces then words follow here",
        "a  b   c    d     e      f       g",
        "🎉🎊🥳 emoji are wider than a CJK glyph 👩‍👩‍👧‍👦 and they keep going past the edge",
        "混在 mixed テキスト with spaces 　and ideographic ones",
    ];
    for (line_index, content) in corpus.iter().enumerate() {
        for columns in [8, 13, 21, 34, 55] {
            let wrap_width = cw * columns as f32;
            let key = body_geometry_key(0, 0, line_index, wrap_width, cw);
            let geometry = view.geometry(&memo, key, content, wrap_width);
            let text = geometry
                .display
                .as_ref()
                .map_or(*content, |display| display.text.as_str());
            let oracle = measure::wrapped_line_starts(
                text,
                props.typography.size,
                props.font,
                view.metrics.row_height,
                wrap_width,
            );
            assert_eq!(
                geometry.display_starts, oracle,
                "break points disagree for {content:?} at {columns} columns"
            );
        }
    }
}

/// A tab lands on the next eight-column stop, not on CoreText's fixed 28px one: tabs are
/// expanded before the engine sees them, and a highlight or a click maps through the
/// expansion back to the source byte.
#[test]
fn geometry_puts_tabs_on_eight_column_stops() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let memo = RefCell::new(GeometryMemo::default());
    let content = "\tab\tcd\t.";
    let key = body_geometry_key(0, 0, 0, f32::INFINITY, view.metrics.char_width);
    let geometry = view.geometry(&memo, key, content, f32::INFINITY);
    assert_eq!(geometry.visual_lines(), 1);
    // byte:      0=\t 1=a 2=b 3=\t 4=c 5=d 6=\t 7=.
    for (byte, column) in [
        (0, 0.0),
        (1, 8.0),
        (2, 9.0),
        (3, 10.0),
        (4, 16.0),
        (5, 17.0),
        (6, 18.0),
        (7, 24.0),
        (8, 25.0),
    ] {
        assert!(
            (geometry.column_at(0, byte) - column).abs() < 0.01,
            "byte {byte} sits at column {}, expected {column}",
            geometry.column_at(0, byte)
        );
    }
    assert!((geometry.width_columns - 25.0).abs() < 0.01);
    // And back: a click inside a tab's run resolves to the tab itself, one past its stop to
    // the glyph that follows.
    assert_eq!(geometry.byte_at(0, 3.0), 0);
    assert_eq!(geometry.byte_at(0, 8.0), 1);
    assert_eq!(geometry.byte_at(0, 19.0), 6);
}

/// A tab-indented line that wraps has to reserve every visual row it paints. Counting a tab
/// as one column under-reserved the row, and the next row painted over the overflow.
#[test]
fn a_tab_indented_wrapped_row_reserves_its_full_height() {
    let content = "\t\treturn some_function(argument_one, argument_two, argument_three)";
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![line(DiffLineKind::Context, content, 1)],
    }]];
    let props = test_props(1);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    // About seventy columns of text: the line wraps with its tabs at eight
    // columns and fits with them at one.
    let width = view.metrics.gutter_width + 32.0 + 70.0 * view.metrics.char_width + 12.0;
    view.ensure_height_index(&cells.index, &cells.geometry, width);

    let cw = view.metrics.char_width;
    let wrap_width = view.content_width(width);
    let rows = |line_index: usize, content: &str| {
        let key = body_geometry_key(0, 0, line_index, wrap_width, cw);
        view.geometry(&cells.geometry, key, content, wrap_width)
            .visual_lines()
    };
    let wrapped = rows(0, content);
    assert!(
        wrapped > 1,
        "the line has to wrap at this width or the test proves nothing"
    );
    assert_eq!(
        cells.index.borrow().row_heights[0],
        wrapped as f32 * view.metrics.row_height
    );
    assert!(
        rows(1, &content.replace('\t', " ")) < wrapped,
        "a tab occupying one column would have reserved fewer rows"
    );
}

/// The height index walks the whole document, so it asks `column_bound` whether a line fits
/// rather than measuring it. A short tab-indented or CJK line must therefore never reach
/// the text engine during a rebuild: shaping them there cost ~180 ms per rebuild on 20k
/// tab-indented lines, once per frame of a sidebar drag.
#[test]
fn short_wide_lines_are_not_shaped_during_an_index_rebuild() {
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Context, "\t\tif err != nil {", 1),
            line(DiffLineKind::Context, "    // мешанина", 2),
            line(DiffLineKind::Context, "\t日本語のコメント", 3),
            line(DiffLineKind::Context, "    plain ascii", 4),
        ],
    }]];
    let mut props = test_props(1);
    for wrap in [true, false] {
        props.wrap = wrap;
        let view = view_of(&props, &hunks);
        let cells = TestCaches::default();
        view.ensure_height_index(&cells.index, &cells.geometry, 1200.0);
        assert!(
            {
                let memo = cells.geometry.borrow();
                memo.lines.is_empty() && memo.shapes.is_empty()
            },
            "a rebuild shaped a line that fits (wrap: {wrap})"
        );
        assert!(
            cells
                .index
                .borrow()
                .row_heights
                .iter()
                .all(|height| *height == view.metrics.row_height)
        );
    }
}

/// `column_bound` may only ever err high. Under-counting would reserve a row too few and
/// the next row would paint over the overflow: the bug this whole seam exists to prevent.
/// Tabs are the case it has to get exactly right, since a tab-indented line that fits is
/// decided by the bound alone and never reaches the engine.
#[test]
fn column_bound_never_undercounts_the_engine() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let memo = RefCell::new(GeometryMemo::default());
    let corpus = [
        "\tif err != nil {",
        "\t\treturn fmt.Errorf(\"read %q: %w\", path, err)",
        "a\tb\tc\td",
        "\t\t\t\t\t\t\t\tdeep",
        "日本語 mixed with ASCII",
        "\t// мешанина of Cyrillic and ASCII",
        "→ ← ↔ ✓ ✗",
        "🎉🎊🥳 emoji are wider than a CJK glyph 👩‍👩‍👧‍👦",
        "plain ascii, no surprises",
        "",
    ];
    for (line_index, content) in corpus.iter().enumerate() {
        let key = body_geometry_key(0, 0, line_index, f32::INFINITY, view.metrics.char_width);
        let measured = view
            .geometry(&memo, key, content, f32::INFINITY)
            .width_columns;
        let bound = column_bound(content);
        assert!(
            bound >= measured - 0.01,
            "bound {bound} undercounts the engine's {measured} for {content:?}"
        );
        if content.is_ascii() {
            assert!(
                (bound - measured).abs() < 0.01,
                "bound {bound} is not exact for the ASCII line {content:?} ({measured})"
            );
        }
    }
}

/// `column_at` and `byte_at` invert each other on multi-byte text, so a click lands on a
/// char boundary and the highlight painted from it covers the glyph the user pointed at.
#[test]
fn columns_and_bytes_round_trip_on_multibyte_text() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let memo = RefCell::new(GeometryMemo::default());
    for (line_index, content) in ["日本語 mixed with ASCII", "Ελληνικά + \tтабы"]
        .iter()
        .enumerate()
    {
        let key = body_geometry_key(0, 0, line_index, f32::INFINITY, view.metrics.char_width);
        let geometry = view.geometry(&memo, key, content, f32::INFINITY);
        let mut last_column = -1.0_f32;
        for (byte, _) in content
            .char_indices()
            .chain(std::iter::once((content.len(), ' ')))
        {
            let column = geometry.column_at(0, byte);
            assert!(
                column > last_column,
                "columns must advance across {content:?}: byte {byte} at {column}"
            );
            last_column = column;
            assert_eq!(
                geometry.byte_at(0, column),
                byte,
                "column {column} of {content:?} resolves away from byte {byte}"
            );
        }
    }
}

/// The height index and the painter agree on a line's visual lines, in every mode that
/// wraps. They read one table, which this holds them to: a row reserves what the painter
/// shapes, the shaped lines are the whole text in order, and none runs past the width
/// unless the column can't hold a single glyph. Trailing blanks are in the corpus because GPUI doesn't
/// hang them past the width.
#[test]
fn the_painter_shapes_the_lines_the_index_counted() {
    let corpus = [
        "aaa bbb ccc".to_owned(),
        "aaa bbb ccc ".to_owned(),
        "word ".repeat(40),
        "ends with spaces      ".to_owned(),
        "      ".to_owned(),
        " ".repeat(300),
        "\t\treturn some_function(argument_one, argument_two, argument_three)".to_owned(),
        "a\tb\tc\td\te\tf\tg\th\ti\tj\tk\tl\tm\tn\to\tp\tq\tr\ts\tt\tu\tv".to_owned(),
        "日本語のテキストが折り返される場合の折り返し位置を確かめる ".repeat(4),
        "🎉🎊🥳 emoji are wider than a CJK glyph 👩‍👩‍👧‍👦 ".repeat(5),
        "x".repeat(500),
        String::new(),
    ];
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: corpus
            .iter()
            .enumerate()
            .map(|(i, text)| line(DiffLineKind::Context, text, i + 1))
            .collect(),
    }]];
    let text_system = Rc::new(WindowTextSystem::new(measure::text_system()));
    for width in [180.0, 240.0, 321.5, 480.0, 777.0, 1200.0] {
        for side_by_side in [false, true] {
            let props = test_props(1).side_by_side(side_by_side);
            let view = view_of(&props, &hunks);
            let cells = TestCaches::default();
            view.ensure_height_index(&cells.index, &cells.geometry, width);
            let index = cells.index.borrow();
            let wrap = view.wrap_width(view.text_width_for_file(&index, 0, width));
            let plan = Plan::new(text_system.clone(), 0);
            let code = RefCell::new(CodeCache::default());
            for (i, content) in corpus.iter().enumerate() {
                let key = body_geometry_key(0, 0, i, wrap, view.metrics.char_width);
                let geometry = view.geometry(&cells.geometry, key, content, wrap);
                let lines = view.shaped_for_test(&plan, &hunks[0][0].lines[i], &geometry, &code);
                assert_eq!(
                    view.index_row_height(&index, i),
                    lines.len() as f32 * view.metrics.row_height,
                    "{content:?} at {width}"
                );
                let shaped: String = lines
                    .iter()
                    .map(|line| AsRef::<str>::as_ref(&line.text))
                    .collect();
                let text = geometry
                    .display
                    .as_ref()
                    .map_or(&content[..], |display| display.text.as_str());
                assert_eq!(shaped, text, "at {width}");
                for line in lines.iter() {
                    assert!(
                        // A column narrower than one wide glyph has to overflow by it.
                        f32::from(line.width) <= wrap + 0.5 || wrap < 24.0,
                        "{:?} is {} wide in {wrap}",
                        line.text,
                        f32::from(line.width)
                    );
                }
            }
        }
    }
}

/// The columns a tabbed line's highlights use land on the glyphs the painter drew.
#[test]
fn emphasis_columns_land_on_the_painted_glyphs_after_a_tab() {
    let hunks = test_hunks();
    let props = test_props(hunks.len());
    let view = view_of(&props, &hunks);
    let memo = RefCell::new(GeometryMemo::default());
    let cw = view.metrics.char_width;
    let content = "\tlet 日本語 = \"x\";\tdone";
    let key = body_geometry_key(0, 0, 0, f32::INFINITY, cw);
    let geometry = view.geometry(&memo, key, content, f32::INFINITY);
    let display = geometry.display.as_ref().expect("the tabs expand");
    let shaped = measure::shape_wrapped(&display.text, props.typography.size, props.font, None)
        .expect("shapes");
    let glyphs: Vec<(usize, f32)> = shaped
        .runs()
        .iter()
        .flat_map(|run| run.glyphs.iter())
        .map(|glyph| (glyph.index, f32::from(glyph.position.x)))
        .collect();
    for needle in ["let", "日本語", "\"x\"", "done"] {
        let byte = content.find(needle).unwrap();
        let disp = display.disp_of(byte);
        let (_, x) = glyphs
            .iter()
            .find(|(index, _)| *index == disp)
            .expect("a glyph at the expansion's offset");
        assert!(
            (geometry.column_at(0, byte) * cw - x).abs() < 0.5,
            "{needle}: column {} vs glyph at {x}",
            geometry.column_at(0, byte) * cw
        );
    }
}

/// A click in the right column of a split row resolves to that column's line and lane.
#[test]
fn split_row_hit_test_lands_in_the_clicked_column() {
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Deletion, "old text here", 2),
            line(DiffLineKind::Addition, "new text here", 2),
        ],
    }]];
    let props = test_props(1).side_by_side(true);
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    let bounds = Rectangle {
        x: 0.0,
        y: 0.0,
        width: 900.0,
        height: 600.0,
    };
    view.ensure_height_index(&cells.index, &cells.geometry, bounds.width);
    let index = cells.index.borrow();
    let split = view.split_layout(bounds.width);
    let row_top = index.row_tops[0];
    // Four columns into each side's text, on the row's only visual line.
    let probe = |text_x: f32| {
        view.position_at_point(
            &index,
            &cells.geometry,
            Point::new(text_x + 4.0 * view.metrics.char_width, row_top + 1.0),
            bounds,
            0.0,
            0.0,
            None,
        )
    };
    let (left, left_lane) = probe(split.left_text_x).expect("a hit in the left column");
    assert_eq!(left_lane, Some(SplitSide::Left));
    assert_eq!((left.line_index, left.byte), (0, 4));
    let (right, right_lane) = probe(split.right_text_x).expect("a hit in the right column");
    assert_eq!(right_lane, Some(SplitSide::Right));
    assert_eq!((right.line_index, right.byte), (1, 4));
}

#[test]
fn line_select_hits_changed_lines_in_the_clicked_column() {
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![
            line(DiffLineKind::Context, "same", 1),
            line(DiffLineKind::Deletion, "old text here", 2),
            line(DiffLineKind::Addition, "new text here", 2),
        ],
    }]];
    let marks = |lines: bool| ChangeMarks {
        files: vec![FileMark {
            state: CheckState::Off,
            hunks: HunkMarks::Unpicked,
        }],
        control: WHITE,
        on_toggle_file: Rc::new(|_, _, _| ()),
        on_toggle_hunk: Rc::new(|_, _, _| ()),
        lines: lines.then(|| LineMode {
            cursor: None,
            reveal: 0,
            on_press: Rc::new(|_, _, _| ()),
            on_paint: Rc::new(|_, _, _| ()),
        }),
        picking: false,
    };
    let bounds = Rectangle {
        x: 0.0,
        y: 0.0,
        width: 900.0,
        height: 600.0,
    };
    for side_by_side in [false, true] {
        let props = test_props(1)
            .change_marks(marks(true))
            .side_by_side(side_by_side);
        let view = view_of(&props, &hunks);
        let cells = TestCaches::default();
        view.ensure_height_index(&cells.index, &cells.geometry, bounds.width);
        let index = cells.index.borrow();
        let hit = |x: f32, row: usize| {
            let point = Point::new(x, index.row_tops[row] + 1.0);
            view.pickable_line_at(&index, 0.0, bounds, point, None)
                .map(|(line, lane)| (line.line, lane))
        };
        // Context is not pickable, in the gutter or the text.
        assert_eq!(hit(10.0, 0), None);
        assert_eq!(hit(400.0, 0), None);
        if side_by_side {
            // The removal and the addition share one row.
            assert_eq!(hit(10.0, 1), Some((1, Some(SplitSide::Left))));
            assert_eq!(hit(800.0, 1), Some((2, Some(SplitSide::Right))));
            // A drag held to the left column stays there.
            let held = view.pickable_line_at(
                &index,
                0.0,
                bounds,
                Point::new(800.0, index.row_tops[1] + 1.0),
                Some(Some(SplitSide::Left)),
            );
            assert_eq!(held.map(|(line, _)| line.line), Some(1));
        } else {
            assert_eq!(hit(10.0, 1), Some((1, None)));
            assert_eq!(hit(400.0, 2), Some((2, None)));
        }
    }
    // Outside line-select mode nothing is a line target.
    let props = test_props(1).change_marks(marks(false));
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, bounds.width);
    let index = cells.index.borrow();
    let point = Point::new(10.0, index.row_tops[1] + 1.0);
    assert_eq!(
        view.pickable_line_at(&index, 0.0, bounds, point, None),
        None
    );
}

/// `\ No newline at end of file` belongs to the line above it, not to a side: counting it
/// made a new file without a trailing newline render split, with one column entirely
/// hatched padding.
#[test]
fn a_trailing_note_leaves_a_new_file_single_sided() {
    let note = DiffLine {
        kind: DiffLineKind::Note,
        old_line: None,
        new_line: None,
        content: "\\ No newline at end of file".to_owned(),
        syntax: Vec::new(),
        emphasis: Vec::new(),
    };
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![line(DiffLineKind::Addition, "fn main() {}", 1), note],
    }]];
    let props = test_props(1).side_by_side(true);
    let view = view_of(&props, &hunks);
    assert!(file_is_single_sided(view.hunks[0]));
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 800.0);
    assert_eq!(cells.index.borrow().full_width_files, vec![true]);
}

/// Bucketing the document-ordered match list into the visible window must find exactly
/// what a scan over every match would have.
#[test]
fn find_matches_bucket_by_row() {
    let matches: Vec<FindMatch> = [
        (0, 0, 0),
        (0, 0, 3),
        (0, 0, 3),
        (0, 1, 0),
        (1, 0, 2),
        (1, 0, 9),
    ]
    .into_iter()
    .map(|(file_index, hunk_index, line_index)| FindMatch {
        file_index,
        hunk_index,
        line_index,
        byte_start: 0,
        byte_end: 1,
    })
    .collect();

    for first in [(0, 0, 0), (0, 0, 3), (0, 1, 0), (1, 0, 0), (2, 0, 0)] {
        for last in [(0, 0, 2), (0, 0, 3), (0, 1, 5), (1, 0, 9), (9, 9, 9)] {
            if last < first {
                continue;
            }
            let window = matches_between(&matches, first, last);
            let brute: Vec<usize> = matches
                .iter()
                .enumerate()
                .filter(|(_, m)| (first..=last).contains(&match_id(m)))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                window.clone().collect::<Vec<_>>(),
                brute,
                "window {first:?}..={last:?}"
            );
            // And within the window, one row's run is the same run the scan would have
            // found, at the same global indices.
            for line in [(0, 0, 3), (0, 1, 0), (1, 0, 9), (0, 0, 7)] {
                let (offset, hits) = matches_on_line(&matches[window.clone()], line);
                let brute: Vec<usize> = matches
                    .iter()
                    .enumerate()
                    .filter(|(i, m)| window.contains(i) && match_id(m) == line)
                    .map(|(i, _)| i)
                    .collect();
                let found: Vec<usize> =
                    (0..hits.len()).map(|i| window.start + offset + i).collect();
                assert_eq!(found, brute, "line {line:?} in window {first:?}..={last:?}");
            }
        }
    }
}

#[test]
fn gutter_digits_use_hunk_tails() {
    let long = vec![
        DiffHunkView {
            header: String::new(),
            lines: vec![line(DiffLineKind::Context, "a", 5)],
        },
        DiffHunkView {
            header: String::new(),
            lines: vec![
                line(DiffLineKind::Context, "b", 99_950),
                // Trailing note without line numbers: the tail scan must step past it.
                DiffLine {
                    kind: DiffLineKind::Note,
                    old_line: None,
                    new_line: None,
                    content: "\\ No newline at end of file".to_owned(),
                    syntax: Vec::new(),
                    emphasis: Vec::new(),
                },
            ],
        },
    ];
    assert_eq!(compute_gutter_digit_count(&[long.as_slice()]), 5);
    assert_eq!(compute_gutter_digit_count(&[]), 3);
}

/// Height-index scaling profile on a synthetic ~1M-line diff:
///   cargo test -p diffui-widgets profile_height_index -- --ignored --nocapture
#[test]
#[ignore]
fn profile_height_index() {
    let lines: Vec<DiffLine> = (0..1_000_000)
        .map(|i| DiffLine {
            kind: DiffLineKind::Context,
            old_line: Some(i + 1),
            new_line: Some(i + 1),
            content: format!("    let value_{i} = compute({i}) + offset; // padding padding"),
            syntax: Vec::new(),
            emphasis: Vec::new(),
        })
        .collect();
    let hunks: Vec<DiffHunkView> = lines
        .chunks(5_000)
        .map(|chunk| DiffHunkView {
            header: "@@ @@".to_owned(),
            lines: chunk.to_vec(),
        })
        .collect();
    let files: Vec<Vec<DiffHunkView>> = hunks.chunks(20).map(<[_]>::to_vec).collect();
    let props = test_props(files.len());
    let view = view_of(&props, &files);

    let cells = TestCaches::default();
    let t = std::time::Instant::now();
    view.ensure_height_index(&cells.index, &cells.geometry, 1200.0);
    let build = t.elapsed();

    let index = cells.index.borrow();
    let t = std::time::Instant::now();
    let mut acc = 0usize;
    for i in 0..10_000 {
        acc += view.file_at_offset(&index, (i * 137) as f32 % index.total_height);
        acc += index
            .row_at((i * 631) as f32 % index.total_height)
            .unwrap_or(0);
    }
    let queries = t.elapsed();

    eprintln!("\n=== height index profile (1M lines) ===");
    eprintln!("build (once per content/width change): {build:?}");
    eprintln!("20k mixed queries                    : {queries:?}  (sink {acc})");
    eprintln!(
        "per query                            : {:?}",
        queries / 20_000
    );
    eprintln!("=======================================\n");
}

/// A drag that ends on a file or hunk header selects up to the nearest row, in either
/// direction and either layout, while a press on a band and a drag past the end keep
/// their end-of-document snap.
#[test]
fn a_drag_onto_a_header_band_stops_at_the_nearest_row() {
    let hunks = vec![
        vec![
            DiffHunkView {
                header: "@@ one @@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Context, "alpha", 1),
                    line(DiffLineKind::Deletion, "old", 2),
                    line(DiffLineKind::Addition, "new", 2),
                ],
            },
            DiffHunkView {
                header: "@@ two @@".to_owned(),
                lines: vec![line(DiffLineKind::Context, "beta", 40)],
            },
        ],
        vec![DiffHunkView {
            header: "@@ three @@".to_owned(),
            lines: vec![
                line(DiffLineKind::Context, "gamma", 1),
                line(DiffLineKind::Context, "omega", 2),
            ],
        }],
    ];
    let bounds = Rectangle {
        x: 0.0,
        y: 0.0,
        width: 900.0,
        height: 600.0,
    };
    let at_end = |file, hunk, line, text: &str| body_position(file, hunk, line, text.len());
    for side_by_side in [false, true] {
        let props = test_props(2).side_by_side(side_by_side);
        let view = view_of(&props, &hunks);
        let cells = TestCaches::default();
        view.ensure_height_index(&cells.index, &cells.geometry, bounds.width);
        let index = cells.index.borrow();
        let drag_to = |y: f32| {
            view.drag_position_at_point(
                &index,
                &cells.geometry,
                Point::new(40.0, y),
                bounds,
                0.0,
                0.0,
                side_by_side.then_some(SplitSide::Left),
            )
            .expect("a drag always lands somewhere")
            .0
        };
        let press_at = |y: f32| {
            view.position_at_point(
                &index,
                &cells.geometry,
                Point::new(40.0, y),
                bounds,
                0.0,
                0.0,
                None,
            )
            .expect("a press on a band lands at the end")
            .0
        };
        let side = side_by_side.then_some(SplitSide::Left);
        let first = body_position(0, 0, 0, 0);
        let last = at_end(1, 0, 1, "omega");
        let top = |file: usize, hunk: usize, line: usize| {
            let row = index
                .row_of_line(file, hunk, line)
                .expect("the line has a row");
            index.row_tops[row]
        };

        // The second hunk's header: its top half is nearer the row above it, its
        // bottom half the row below.
        let band_end = top(0, 1, 0);
        let hunk_band = band_end - view.metrics.hunk_header_height;
        let (hunk_near_above, hunk_near_below) = (hunk_band + 1.0, band_end - 1.0);
        // The last line of the first hunk in this layout's left column reads "old";
        // unified has "new" after it.
        let above_hunk = if side_by_side {
            body_position(0, 0, 1, "old".len())
        } else {
            at_end(0, 0, 2, "new")
        };
        assert_eq!(drag_to(hunk_near_above), above_hunk);
        assert_eq!(drag_to(hunk_near_below), body_position(0, 1, 0, 0));

        // The second file's header is nearer `beta`; its hunk header nearer `gamma`.
        let file_band = index.file_tops[1];
        assert_eq!(drag_to(file_band + 1.0), at_end(0, 1, 0, "beta"));
        assert_eq!(drag_to(top(1, 0, 0) - 1.0), body_position(1, 0, 0, 0));

        // Downward: everything before the band, nothing after it.
        let down = view.collect_selected_text(first, drag_to(file_band + 1.0), side);
        assert!(down.ends_with("beta"), "{down:?}");
        assert!(!down.contains("gamma"), "{down:?}");
        let down = view.collect_selected_text(first, drag_to(top(1, 0, 0) - 1.0), side);
        assert!(down.contains("beta") && !down.contains("gamma"), "{down:?}");

        // Upward: from the last line, nothing above the band.
        let up = view.collect_selected_text(drag_to(file_band + 1.0), last, side);
        assert_eq!(up.trim_start(), "gamma\nomega");
        let up = view.collect_selected_text(drag_to(top(1, 0, 0) - 1.0), last, side);
        assert_eq!(up, "gamma\nomega");
        let up = view.collect_selected_text(drag_to(hunk_near_below), last, side);
        assert!(up.starts_with("beta"), "{up:?}");
        assert!(!up.contains("alpha"), "{up:?}");

        // Above the first row there's only that row to go to.
        assert_eq!(drag_to(1.0), first);
        assert_eq!(drag_to(-50.0), first);

        // Past the last row a drag still selects to the end.
        let bottom = *index.file_tops.last().expect("a sentinel");
        assert_eq!(drag_to(bottom + 200.0), last);

        // A press on a band is what it was.
        assert_eq!(press_at(file_band + 1.0), last);
        assert_eq!(press_at(hunk_near_below), last);
    }
}

/// No-wrap sideways extent: each line is held to the text area it is laid out in. A
/// one-sided file spans the whole area in split mode, so its long line mustn't let the
/// split files scroll on into nothing, and a line that fits needs no scrolling at all.
#[test]
fn the_sideways_extent_holds_each_line_to_its_own_text_area() {
    let cols = |n: usize| "x".repeat(n);
    let hunks = |full: usize, split: usize| {
        vec![
            vec![DiffHunkView {
                header: "@@".to_owned(),
                lines: vec![line(DiffLineKind::Addition, &cols(full), 1)],
            }],
            vec![DiffHunkView {
                header: "@@".to_owned(),
                lines: vec![
                    line(DiffLineKind::Deletion, &cols(split), 1),
                    line(DiffLineKind::Addition, "new", 1),
                ],
            }],
        ]
    };
    let width = 1600.0;
    let extent = |hunks: &[Vec<DiffHunkView>]| {
        let mut props = test_props(2).side_by_side(true);
        props.wrap = false;
        let view = view_of(&props, hunks);
        let cells = TestCaches::default();
        view.ensure_height_index(&cells.index, &cells.geometry, width);
        let index = cells.index.borrow();
        let cw = view.metrics.char_width;
        let areas = (
            view.content_width(width),
            view.split_layout(width).text_width,
        );
        (view.max_horizontal(&index, width), cw, areas)
    };
    let fits = |area: f32, cw: f32| (area / cw).floor() as usize - 2;

    // The full-width line is wider than a split column but fits the whole area, and the
    // split one fits its column: nothing to scroll to.
    let (_, cw, (full, split)) = extent(&hunks(1, 1));
    assert!(fits(full, cw) > fits(split, cw));
    let (max, ..) = extent(&hunks(fits(full, cw), fits(split, cw)));
    assert_eq!(max, 0.0);

    // Past either area, it scrolls exactly to the line's end (and a breathing column).
    let long = fits(full, cw) + 10;
    let (max, ..) = extent(&hunks(long, 1));
    assert_eq!(max, (long as f32 + 1.0) * cw - full);
    let long = fits(split, cw) + 10;
    let (max, ..) = extent(&hunks(1, long));
    assert_eq!(max, (long as f32 + 1.0) * cw - split);
}

/// A non-ASCII line's sideways extent is its shaped width, worked out after the
/// rebuild, not the three-columns-a-char bound the rebuild works with.
#[test]
fn a_non_ascii_line_reaches_as_far_as_it_is_drawn() {
    let content = "日本語のコメント".repeat(20);
    let hunks = vec![vec![DiffHunkView {
        header: "@@".to_owned(),
        lines: vec![line(DiffLineKind::Addition, &content, 1)],
    }]];
    let mut props = test_props(1);
    props.wrap = false;
    let view = view_of(&props, &hunks);
    let cells = TestCaches::default();
    view.ensure_height_index(&cells.index, &cells.geometry, 1200.0);
    assert!(
        cells.geometry.borrow().shapes.is_empty(),
        "the rebuild shaped"
    );
    view.settle_extent(&cells.index, &cells.geometry);
    let shaped = LineShape::of(&content, view.geometry_params(f32::INFINITY)).width()
        / view.metrics.char_width;
    let index = cells.index.borrow();
    assert_eq!(index.max_columns, shaped);
    assert!(shaped < super::line::column_bound(&content));
}
