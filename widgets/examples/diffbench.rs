//! Times the real diff element through the offscreen renderer: scroll steps, jumps
//! and width changes, in wrap and split, with paint counts to confirm one real paint
//! per frame.
//!
//!   cargo run -p diffui-widgets --example diffbench
//!
//! `cx.notify()` flushes a draw when `cx.update` returns, so a frame is timed around
//! that update. Lines are distinct: GPUI caches layouts by text, and a document of
//! repeats would measure the cache.

use std::sync::Arc;
use std::time::{Duration, Instant};

use diffui_core::{DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind, SyntaxKind, SyntaxSpan};
use diffui_widgets::diff_view::{self, DiffFileView, DiffProps, DiffView, DocSource};
use diffui_widgets::theme::{Font, ResolvedTheme, ThemeSpec, diff_palette};
use diffui_widgets::typography::CodeTypography;
use gpui_kit::prelude::*;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext, Context, Entity, HeadlessAppContext, Render, Window, div, px, size,
};

/// The document, in an entity, as the app holds it.
struct Doc {
    files: Vec<Vec<DiffHunkView>>,
}

struct Stage {
    doc: Entity<Doc>,
    wrap: bool,
    split: bool,
    width: Option<f32>,
    view: Option<Entity<DiffView>>,
    theme: ThemeSpec,
}

impl Render for Stage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.doc.read(cx).files.len();
        let files = (0..count)
            .map(|i| DiffFileView {
                title: format!("src/generated/module_{i}.rs"),
                status: DiffFileStatus::Modified,
                status_color: self.theme.added_text,
                status_fill: diffui_widgets::theme::chip_background(self.theme.added_text),
                additions: 3,
                deletions: 2,
            })
            .collect();
        let props = DiffProps::new(
            files,
            DocSource::new(self.doc.clone(), |doc: &Doc, file| {
                doc.files.get(file).map_or(&[], Vec::as_slice)
            }),
            0,
            "bench",
            diff_palette(self.theme),
            Font::MONOSPACE,
            CodeTypography::default(),
            500,
        )
        .wrap(self.wrap)
        .side_by_side(self.split);
        let element = diff_view::entity("bench", props, window, cx);
        self.view = Some(element.clone());
        let mut pane = div().h_full();
        pane = match self.width {
            Some(width) => pane.w(px(width)),
            None => pane.w_full(),
        };
        div().size_full().child(pane.child(element))
    }
}

struct Bench {
    cx: HeadlessAppContext,
    stage: Entity<Stage>,
    window: AnyWindowHandle,
}

impl Bench {
    fn new(files: Vec<Vec<DiffHunkView>>, wrap: bool, split: bool) -> Self {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(gpui_kit::assets::Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.update(gpui_kit::init);
        let mut stage = None;
        let handle = cx
            .open_window(size(px(1600.0), px(1000.0)), |_, cx| {
                let doc = cx.new(|_| Doc { files });
                let entity = cx.new(|_| Stage {
                    doc,
                    wrap,
                    split,
                    width: None,
                    view: None,
                    theme: ResolvedTheme::Light.spec(),
                });
                stage = Some(entity.clone());
                entity
            })
            .expect("open a window");
        Self {
            cx,
            stage: stage.expect("built"),
            window: handle.into(),
        }
    }

    fn view(&mut self) -> Entity<DiffView> {
        let stage = self.stage.clone();
        self.cx
            .update(|cx| stage.read(cx).view.clone().expect("rendered"))
    }

    fn redraw(&mut self) -> Duration {
        let started = Instant::now();
        self.cx
            .update_window(self.window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        started.elapsed()
    }

    fn scroll_to(&mut self, offset: f32) -> Duration {
        let view = self.view();
        let started = Instant::now();
        self.cx
            .update(|cx| view.update(cx, |view, cx| view.scroll_to(offset, cx)));
        started.elapsed()
    }

    fn set_width(&mut self, width: f32) -> Duration {
        let stage = self.stage.clone();
        let started = Instant::now();
        self.cx.update(|cx| {
            stage.update(cx, |stage, cx| {
                stage.width = Some(width);
                cx.notify();
            });
        });
        started.elapsed()
    }

    fn stats(&mut self) -> diff_view::FrameStats {
        let view = self.view();
        self.cx.update(|cx| view.read(cx).frame_stats())
    }
}

fn line(kind: DiffLineKind, number: usize, content: String) -> DiffLine {
    let mut syntax = Vec::new();
    let mut start = 0;
    for (index, ch) in content.char_indices() {
        if ch == ' ' && index > start {
            let kind = match syntax.len() % 4 {
                0 => SyntaxKind::Keyword,
                1 => SyntaxKind::Function,
                2 => SyntaxKind::String,
                _ => SyntaxKind::Type,
            };
            syntax.push(SyntaxSpan {
                start,
                end: index,
                kind,
            });
            start = index + 1;
        }
    }
    let emphasis = if kind == DiffLineKind::Context || content.len() < 24 {
        Vec::new()
    } else {
        let at = |byte: usize| {
            (byte..content.len())
                .find(|&it| content.is_char_boundary(it))
                .unwrap_or(content.len())
        };
        vec![(at(4), at(10)), (at(18), at(24))]
    };
    DiffLine {
        kind,
        old_line: (kind != DiffLineKind::Addition).then_some(number),
        new_line: (kind != DiffLineKind::Deletion).then_some(number),
        content,
        syntax,
        emphasis,
    }
}

/// Splits `lines` into files of hunks, 100 rows a hunk and 1000 a file.
fn into_files(lines: Vec<DiffLine>) -> Vec<Vec<DiffHunkView>> {
    let hunks: Vec<DiffHunkView> = lines
        .chunks(100)
        .enumerate()
        .map(|(i, lines)| DiffHunkView {
            header: format!("@@ -{0},100 +{0},100 @@ fn section_{i}()", i * 100 + 1),
            lines: lines.to_vec(),
        })
        .collect();
    hunks.chunks(10).map(<[_]>::to_vec).collect()
}

fn kind_of(index: usize) -> DiffLineKind {
    match (index % 7 == 3, index % 11 == 5) {
        (true, _) => DiffLineKind::Addition,
        (_, true) => DiffLineKind::Deletion,
        _ => DiffLineKind::Context,
    }
}

/// `rows` rows of real source, made distinct.
fn realistic(rows: usize, tabs: bool) -> Vec<Vec<DiffHunkView>> {
    let source = include_str!("../src/diff_view/layout.rs");
    let lines: Vec<&str> = source.lines().collect();
    into_files(
        (0..rows)
            .map(|index| {
                let raw = lines[index % lines.len()];
                let mut text = format!("{raw} // {index}");
                if tabs {
                    text = text.replace("    ", "\t");
                }
                line(kind_of(index), index + 1, text)
            })
            .collect(),
    )
}

/// Rows of `columns`-long lines, each distinct: the worst case for wrapping.
fn long_lines(rows: usize, columns: usize) -> Vec<Vec<DiffHunkView>> {
    let unit = "let value = compute(input, &mut state); ";
    into_files(
        (0..rows)
            .map(|index| {
                let mut text = format!("{index} ");
                while text.len() < columns {
                    text.push_str(unit);
                }
                text.truncate(columns);
                line(kind_of(index), index + 1, text)
            })
            .collect(),
    )
}

/// Non-Latin long lines: the path that has to ask the engine.
fn cjk_lines(rows: usize, columns: usize) -> Vec<Vec<DiffHunkView>> {
    let unit = "日本語のテキストが折り返される ";
    into_files(
        (0..rows)
            .map(|index| {
                let mut text = format!("{index} ");
                while text.chars().count() < columns {
                    text.push_str(unit);
                }
                line(kind_of(index), index + 1, text)
            })
            .collect(),
    )
}

fn report(name: &str, mut samples: Vec<Duration>, paints: u64) {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1000.0;
    println!(
        "  {name:<26} n={:<4} paints={paints:<4} p50={:>8.2}ms p95={:>8.2}ms p99={:>8.2}ms max={:>8.2}ms",
        samples.len(),
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0),
    );
}

fn run(name: &str, files: Vec<Vec<DiffHunkView>>, wrap: bool, split: bool, resize: bool) {
    let rows: usize = files.iter().flatten().map(|hunk| hunk.lines.len()).sum();
    let mut bench = Bench::new(files, wrap, split);
    let cold = bench.redraw();
    let stats = bench.stats();
    println!(
        "\n== {name}: {rows} rows, wrap={wrap}, split={split}, {:.0}px tall",
        stats.total_height
    );
    println!(
        "  first build of the height index {:.1}ms",
        stats.last_index_build.as_secs_f64() * 1e3,
    );
    let _ = cold;
    let total = stats.total_height;

    let before = bench.stats().frames;
    let mut steps = Vec::new();
    let mut scroll = 0.0;
    for _ in 0..600 {
        scroll += 30.0;
        steps.push(bench.scroll_to(scroll));
    }
    let paints = bench.stats().frames - before;
    report("scroll +30px", steps, paints);

    let before = bench.stats().frames;
    let mut jumps = Vec::new();
    let mut state = 0x2545_f491_u64;
    for _ in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        jumps.push(bench.scroll_to((state % total.max(1.0) as u64) as f32));
    }
    let paints = bench.stats().frames - before;
    report("jump to random offset", jumps, paints);

    if resize {
        let before = bench.stats().frames;
        let mut widths = Vec::new();
        let mut index_builds = Vec::new();
        for step in 0..24 {
            widths.push(bench.set_width(1500.0 - step as f32 * 7.0));
            index_builds.push(bench.stats().last_index_build);
        }
        let paints = bench.stats().frames - before;
        report("width change", widths, paints);
        report("  of it the index", index_builds, 0);
    }
}

fn main() {
    run("realistic", realistic(200_000, false), true, false, true);
    run("realistic", realistic(200_000, false), false, false, true);
    run("realistic", realistic(200_000, false), true, true, true);
    run("realistic", realistic(200_000, false), false, true, false);
    run("tabs", realistic(200_000, true), true, false, true);
    run("long lines", long_lines(20_000, 600), true, false, true);
    run("long lines", long_lines(20_000, 600), true, true, true);
    run("long lines", long_lines(20_000, 600), false, false, false);
    run("cjk long lines", cjk_lines(5_000, 300), true, false, true);
}
