//! What the panels lane calls from neighbours that aren't in its tree yet. Each
//! piece stands in for a contract in `reports/contracts.md`, just well enough to
//! see and drive the panels around it. `reports/panels.md` says how to reconnect;
//! when that's done this file goes away.

use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Window, div, px, uniform_list};

use crate::theme::{ThemeSpec, chip_background};
use crate::typography;
use crate::{Cx, Diffui};

/// What keeps `main.rs`'s `graph_layout` re-export in use until `revision_rows` is.
type RevisionRowsLayout = crate::graph_layout::GraphLayout;

/// `crate::revision_rows::build_revision_list`, the history lane's.
pub(crate) fn revision_list(
    _ui: &Diffui,
    _theme: ThemeSpec,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("revision_rows::build_revision_list")
}

/// `crate::find::build_overlay`: its stub fills the pane; the real one floats.
pub(crate) fn find_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.active().find().is_none() {
        return div().into_any_element();
    }
    div()
        .absolute()
        .top_0()
        .right_0()
        .w(px(420.0))
        .h(px(40.0))
        .bg(theme.panel_background_elevated)
        .child(crate::find::build_overlay(ui, theme, window, cx))
        .into_any_element()
}

/// `crate::history_panel::source_bar_actions`: its stub fills the bar.
pub(crate) fn source_bar_actions(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    div()
        .flex_none()
        .w(px(180.0))
        .child(crate::history_panel::source_bar_actions(
            ui, theme, window, cx,
        ))
        .into_any_element()
}

/// The plain file view `diff_view::view` draws: line numbers and text, no find,
/// no selection, no blame. For the pane around it.
pub(crate) fn code_lines(
    ui: &Diffui,
    view: &crate::SourceFileView,
    theme: ThemeSpec,
    _cx: &mut Cx,
) -> AnyElement {
    let lines: Rc<Vec<(Option<usize>, String)>> = Rc::new(
        view.file
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .map(|line| (line.new_line, line.content.clone()))
            .collect(),
    );
    let (mono, code) = (ui.config.mono_font, ui.config.code_type);
    let height = code.size * code.line_height;
    let count = lines.len();
    uniform_list("source-code", count, move |range, _, _| {
        range
            .map(|index| {
                let (number, content) = &lines[index];
                div()
                    .h(px(height))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(16.0))
                    .pl(px(16.0))
                    .whitespace_nowrap()
                    .child(div().w(px(36.0)).flex_none().child(typography::line(
                        number.map(|n| n.to_string()).unwrap_or_default(),
                        mono,
                        code.size,
                        theme.subtle_text,
                    )))
                    .child(typography::line(
                        content.replace('\t', "    "),
                        mono,
                        code.size,
                        theme.text,
                    ))
            })
            .collect::<Vec<_>>()
    })
    .size_full()
    .into_any_element()
}

/// `crate::revision_list`'s file-only face, with the contract's signatures. The
/// rows are the real [`crate::revision_list::FileRowView`]s; scrolling isn't
/// reported or restored here.
pub(crate) mod revision_list {
    use crate::geometry::{Point, Rectangle};
    use crate::icons;
    use crate::revision_list::{FILE_ROW_HEIGHT, FileRowView, ROW_CARD_RADIUS, RevisionListStyle};
    use gpui_kit::base::TestSupportExt as _;

    use super::*;

    type Build = Rc<dyn Fn(usize, &App) -> FileRowView>;
    type Callback<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

    pub(crate) struct RevisionListProps {
        rows: usize,
        style: RevisionListStyle,
        build: Option<Build>,
        selected_file: Option<usize>,
        on_select: Option<Callback<usize>>,
    }

    impl RevisionListProps {
        pub(crate) fn new(
            _commit_count: usize,
            expanded: Option<(usize, usize)>,
            style: RevisionListStyle,
        ) -> Self {
            Self {
                rows: expanded.map_or(0, |(_, count)| count),
                style,
                build: None,
                selected_file: None,
                on_select: None,
            }
        }

        pub(crate) fn files(
            mut self,
            build: Build,
            selected_file: Option<usize>,
            on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
        ) -> Self {
            self.build = Some(build);
            self.selected_file = selected_file;
            self.on_select = Some(Rc::new(on_select));
            self
        }

        pub(crate) fn at_window_edge(self) -> Self {
            self
        }

        pub(crate) fn reveal_file(self, _token: u64, _flat: Option<usize>) -> Self {
            self
        }

        pub(crate) fn on_scroll(self, _f: impl Fn(&f64, &mut Window, &mut App) + 'static) -> Self {
            self
        }

        pub(crate) fn restore_scroll(self, _offset: f64, _token: u64) -> Self {
            self
        }

        pub(crate) fn on_file_context_menu(
            self,
            _f: impl Fn(&(usize, Rectangle, Point), &mut Window, &mut App) + 'static,
        ) -> Self {
            self
        }
    }

    pub(crate) fn view(
        id: &'static str,
        props: RevisionListProps,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        let RevisionListProps {
            rows,
            style,
            build,
            selected_file,
            on_select,
        } = props;
        let (Some(build), Some(on_select)) = (build, on_select) else {
            return div().into_any_element();
        };
        let (font, mono) = (style.primary_font, style.mono_font);
        uniform_list(id, rows, move |range, _, cx| {
            range
                .map(|index| {
                    let row = build(index, cx);
                    let on_select = on_select.clone();
                    let selected = row.chevron.is_none() && Some(row.file_index) == selected_file;
                    let glyph = match row.chevron {
                        Some(true) => icons::CHEVRON_RIGHT,
                        Some(false) => icons::CHEVRON_DOWN,
                        None => icons::FILE,
                    };
                    let badge = (style.file_badge_width > 0.0).then(|| {
                        div().w(px(style.file_badge_width)).flex_none().when(
                            !row.status_label.is_empty(),
                            |badge| {
                                badge
                                    .rounded(px(4.0))
                                    .bg(row.status_background)
                                    .flex()
                                    .justify_center()
                                    .child(typography::line(
                                        row.status_label.clone(),
                                        mono,
                                        crate::theme::text_size::CAPTION,
                                        row.status_text,
                                    ))
                            },
                        )
                    });
                    div()
                        .id(("seam-file-row", index))
                        .h(px(FILE_ROW_HEIGHT))
                        .mx(px(crate::revision_list::ROW_CARD_INSET_X))
                        .my(px(0.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(8.0 + row.indent))
                        .pr(px(crate::revision_list::FILE_ROW_RIGHT_PAD))
                        .rounded(px(ROW_CARD_RADIUS))
                        .when(selected, |card| card.bg(style.selected_background))
                        .hover(|card| card.bg(chip_background(style.subtle_text)))
                        .test_support()
                        .on_click(move |_, window, cx| on_select(&index, window, cx))
                        .children(badge)
                        .child(icons::icon(glyph, 13.0, row.icon_color))
                        .child(
                            crate::ellipsis::Line::new(
                                vec![crate::ellipsis::Span::new(row.primary.clone())],
                                font,
                                crate::revision_list::SMALL_TEXT_SIZE,
                                row.primary_color,
                            )
                            .view(),
                        )
                })
                .collect::<Vec<_>>()
        })
        .size_full()
        .bg(style.background)
        .into_any_element()
    }
}
