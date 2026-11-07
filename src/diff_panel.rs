use crate::changes::ChangesMsg;
use crate::diff_view::{
    self, DiffFileView, DiffHunkView, DiffProps, DocSource, HeaderAction, HeaderLine, Slice,
};
use crate::geometry::{Point, Rectangle};
use crate::revision_list::{self, FileRowView, RevisionListProps};
use crate::theme::{
    Color, Font, ThemeSpec, Weight, chip_background, diff_palette, file_status_color, text_size,
};
use crate::{Action, Cx, Diffui, LoadStatus, icons, ui};
use diffui_core::FileTreeRow;
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};
use jj_lib::graph::GraphEdgeType;
use std::rc::Rc;

const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
pub(crate) const DESCRIPTION_EDITOR_ID: &str = "revision-description-editor";
const DESCRIPTION_EDITOR_GAP: f32 = 8.0;
// 12 over iced's 236: CoreText sets 13pt SF as AppKit does, about 12% wider than
// iced's shaper did, and the stat columns measure wider with it.
pub const DEFAULT_FILE_NAV_WIDTH: f32 = 248.0;
pub const MIN_FILE_NAV_WIDTH: f32 = 168.0;
pub const FILE_NAV_PEEK_WIDTH: f32 = 284.0;

/// A `+N` / `−N` count wears its add/del color only when it counts
/// something: a `+0` beside a `−5` reads as noise, not as a signal.
pub(crate) fn stat_color(count: usize, color: Color, theme: ThemeSpec) -> Color {
    if count == 0 { theme.subtle_text } else { color }
}

/// A stat column exactly as wide as its widest count. The counts are drawn
/// right-aligned in it, so the even gaps between the columns come from the
/// row's own gap, not from padding baked in here.
fn changed_file_stat_width(sign: char, value: usize, font: Font) -> f32 {
    crate::measure::line_width(&format!("{sign}{value}"), text_size::CAPTION, font).ceil()
}

#[derive(Default)]
struct EmptyFileTreeLanes {
    continuation: Rc<[Option<GraphEdgeType>]>,
    columns: Rc<[Option<usize>]>,
    labels: Rc<[Vec<String>]>,
    segments: Rc<[Option<usize>]>,
}

fn changed_file_row(
    row: &FileTreeRow,
    files: &[diffui_core::DiffFile],
    theme: ThemeSpec,
    additions_width: f32,
    deletions_width: f32,
    lanes: &EmptyFileTreeLanes,
) -> FileRowView {
    let (
        primary,
        raw_path,
        indent,
        chevron,
        file_index,
        status_label,
        status_color,
        additions,
        deletions,
    ) = match row {
        FileTreeRow::Dir {
            label,
            path,
            depth,
            collapsed,
        } => (
            label.clone(),
            path.clone(),
            *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
            Some(*collapsed),
            usize::MAX,
            String::new(),
            theme.subtle_text,
            0,
            0,
        ),
        FileTreeRow::File {
            file_index,
            label,
            depth,
        } => {
            // Read at paint against live state: an event can land between a new
            // document and the render that replaces these rows.
            let file = files.get(*file_index);
            (
                label.clone(),
                file.map_or_else(String::new, |file| file.path.clone()),
                *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
                None,
                *file_index,
                file.map_or("", |file| file.status.short_label()).to_owned(),
                file.map_or(theme.subtle_text, |file| {
                    file_status_color(file.status, theme)
                }),
                file.map_or(0, |file| file.additions),
                file.map_or(0, |file| file.deletions),
            )
        }
    };
    FileRowView {
        primary,
        raw_path,
        status_label,
        status_background: chip_background(status_color),
        status_text: status_color,
        additions,
        deletions,
        additions_text: stat_color(additions, theme.added_text, theme),
        deletions_text: stat_color(deletions, theme.removed_text, theme),
        continuation: Rc::clone(&lanes.continuation),
        columns: Rc::clone(&lanes.columns),
        additions_width,
        deletions_width,
        primary_color: theme.text,
        icon_color: status_color,
        indent,
        chevron,
        file_index,
        lane_labels: Rc::clone(&lanes.labels),
        lane_segments: Rc::clone(&lanes.segments),
        check: None,
    }
}

/// Code columns a side-by-side half has to hold before the split is worth
/// it. Narrower than this and both halves soft-wrap mid-token on ordinary
/// lines, which reads worse than one unified column.
const MIN_SPLIT_COLUMN_CHARS: f32 = 56.0;

/// The diff pane's width: the window minus the history and file panes and
/// their dividers.
pub(crate) fn diff_pane_width(ui: &Diffui) -> f32 {
    let history_width = if ui.active().history_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH
    } else {
        ui.sidebar_width
    };
    let files_width = if ui.active().session.document.files.is_empty() {
        0.0
    } else if ui.active().files_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH + 1.0
    } else {
        ui.file_nav_width + 1.0
    };
    (ui.window_size.width - history_width - files_width - 1.0).max(0.0)
}

/// Whether a side-by-side diff has room for both halves. The toggle is the
/// user's preference; this only decides whether it can be honoured now, so
/// widening the window brings the split back.
pub(crate) fn split_fits(ui: &Diffui) -> bool {
    let size = ui.config.code_type.size;
    let char_width = crate::measure::line_width("0000000000", size, ui.config.mono_font) / 10.0;
    // Per half: a line-number gutter (one number, ~4 digits + padding) plus
    // the code columns.
    let half = 4.0 * char_width + 16.0 + MIN_SPLIT_COLUMN_CHARS * char_width;
    diff_pane_width(ui) >= 2.0 * half
}

/// Side-by-side is on and fits.
pub(crate) fn split_in_effect(ui: &Diffui) -> bool {
    ui.diff_split && split_fits(ui)
}

fn description_editor_input_height(ui: &Diffui) -> f32 {
    let Some(editor) = ui.active().description_editor() else {
        return 0.0;
    };
    let history_width = if ui.active().history_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH
    } else {
        ui.sidebar_width
    };
    let files_width = if ui.active().session.document.files.is_empty() {
        0.0
    } else if ui.active().files_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH + 1.0
    } else {
        ui.file_nav_width + 1.0
    };
    let width = (ui.window_size.width
        - history_width
        - files_width
        - 1.0
        - diff_view::HEADER_HORIZONTAL_PADDING * 2.0)
        .max(1.0);
    crate::input::fit_height(
        &editor.text(),
        ui.config.mono_font,
        ui.config.code_type.size,
        width,
        88.0,
        crate::input::editor_max_height(ui.window_size.height),
    )
}

/// Stable changed-file navigation beside the review. Keeping this separate
/// from the revision graph means browsing a change never reflows history.
pub(crate) fn build_file_navigator(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    build_file_navigator_surface(ui, theme, ui.file_nav_width, true, window, cx)
}

/// The same navigator used by the permanent panel, sized as a floating quick
/// picker when the panel is collapsed. Keeping one tree builder preserves file
/// selection, directory folding, scrolling, and the file context menu.
pub(crate) fn build_file_navigator_peek(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let tree = build_file_navigator_surface(ui, theme, FILE_NAV_PEEK_WIDTH, false, window, cx);
    let hover = Diffui::listen(cx, |this, entered: &bool, _, cx| {
        if *entered {
            this.on_changed_files_popup_entered();
        } else {
            this.on_changed_files_popup_exited(cx);
        }
    });
    crate::theme::popover(div().id("file-navigator-peek"), theme)
        .w(px(FILE_NAV_PEEK_WIDTH))
        .h_full()
        .overflow_hidden()
        .occlude()
        .on_hover(hover)
        .child(tree)
        .into_any_element()
}

fn build_file_navigator_surface(
    ui: &Diffui,
    theme: ThemeSpec,
    width: f32,
    show_collapse: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let files = &ui.active().session.document.files;
    if files.is_empty() {
        return div().into_any_element();
    }
    let title_font = crate::theme::emphasis_font(ui.config.ui_font, Weight::MEDIUM);
    // A section's count rides in its title, as in "Changed commits (2)".
    let title = crate::typography::centered(
        crate::typography::line(
            format!("Changed files ({})", files.len()),
            title_font,
            text_size::UI,
            theme.subtle_text,
        ),
        title_font,
        text_size::UI,
    );
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        // Toggle-row height even without the toggle (the peek), so the label sits on
        // the same line as the other panels' headers.
        .h(px(crate::theme::control::COMPACT))
        .child(title)
        .child(div().flex_1());
    if show_collapse {
        header = header.child(
            div()
                .flex_none()
                .size(px(crate::theme::control::COMPACT))
                .overflow_hidden()
                .child(crate::field::panel_toggle_button(
                    ui.config.ui_font,
                    theme,
                    icons::MINUS,
                    "Hide changed files",
                    Action::ToggleFilesPanel,
                    window,
                    cx,
                )),
        );
    }
    div()
        .flex()
        .flex_col()
        .w(px(width))
        .h_full()
        .bg(theme.panel_background)
        .child(
            div()
                .flex_none()
                .pt(px(crate::field::PANEL_TOGGLE_INSET))
                .pb(px(crate::field::PANEL_TOGGLE_INSET))
                .pl(px(crate::theme::space::LG))
                // The toggle keeps its square inset; a bare count (the peek) mirrors
                // the label's inset instead.
                .pr(px(if show_collapse {
                    crate::field::PANEL_TOGGLE_INSET
                } else {
                    crate::theme::space::LG
                }))
                .child(header),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(file_tree(ui, theme, window, cx)),
        )
        .into_any_element()
}

/// The changed files on the shared list: folding, stat columns, checkboxes, scroll
/// restore and reveal. Rows are built at paint, for on-screen rows only.
fn file_tree(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let tab = ui.active();
    let files = &tab.session.document.files;
    let tree_rows = ui.sidebar_file_cache.borrow_mut().tree_rows(
        tab.session.document_id,
        files.len(),
        &tab.collapsed_dirs,
        files,
    );
    let row_count = tree_rows.len();
    let selected_display = tree_rows.iter().position(|row| {
        matches!(row, FileTreeRow::File { file_index, .. } if *file_index == tab.selected_file)
    });
    let max_additions = files.iter().map(|file| file.additions).max().unwrap_or(0);
    let max_deletions = files.iter().map(|file| file.deletions).max().unwrap_or(0);
    let additions_width = changed_file_stat_width('+', max_additions, ui.config.ui_font);
    let deletions_width = changed_file_stat_width('\u{2212}', max_deletions, ui.config.ui_font);
    let badge_width = crate::chip::width("M", None, ui.config.mono_font);
    let lanes = EmptyFileTreeLanes::default();
    let this = cx.entity();
    let build_file = Rc::new(move |display: usize, cx: &gpui_kit::App| {
        let ui = this.read(cx);
        let row = &tree_rows[display];
        let mut view = changed_file_row(
            row,
            &ui.active().session.document.files,
            theme,
            additions_width,
            deletions_width,
            &lanes,
        );
        view.check = match row {
            FileTreeRow::File { file_index, .. } => {
                crate::changes::tree_check(ui, Some(*file_index), None)
            }
            FileTreeRow::Dir { path, .. } => crate::changes::tree_check(
                ui,
                None,
                Some(&format!("{}/", path.trim_end_matches('/'))),
            ),
        };
        view
    });
    let props = RevisionListProps::new(
        0,
        Some((0, row_count)),
        crate::sidebar::revision_list_style(theme, ui.config, badge_width),
    )
    .files(
        build_file,
        Some(tab.selected_file),
        Diffui::listen(cx, |this, row: &usize, _, _| {
            this.on_sidebar_file_row(*row);
        }),
    )
    .reveal_file(ui.sidebar_file_reveal_token, selected_display)
    // Every scroll tick: stored, with no reconcile and no redraw.
    .on_scroll(cx.listener(|this: &mut Diffui, offset: &f64, _, _| {
        this.on_changed_files_scrolled(*offset);
    }))
    .restore_scroll(tab.file_tree_scroll_offset, ui.scroll_restore_token)
    .on_file_context_menu(Diffui::listen(
        cx,
        |this, (row, rect, point): &(usize, Rectangle, Point), _, cx| {
            this.on_sidebar_file_context_menu(*row, *rect, *point, cx);
        },
    ))
    .on_file_check(Diffui::listen(cx, |this, row: &usize, _, cx| {
        this.perform(Action::Changes(ChangesMsg::ToggleTreeRow(*row)), cx);
    }));
    revision_list::view("file-navigator", props, window, cx)
}

pub(crate) fn build_collapsed_file_navigator(
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    // Only the icon opens the peek: the rail runs the full height of the window, so
    // as a trigger it would fire on every pass between the history and the diff. It
    // still holds a peek that is already open, because a diagonal move from the icon
    // to a file far down the tree spends longer crossing the rail than the close
    // delay allows.
    let toggle = crate::field::panel_toggle_button_bare(
        theme,
        icons::FILE_DIFF,
        Action::ToggleFilesPanel,
        window,
        cx,
    );
    let trigger_hover = Diffui::listen(cx, |this, entered: &bool, _, cx| {
        if *entered {
            this.on_changed_files_trigger_entered();
        } else {
            this.on_changed_files_trigger_exited(cx);
        }
    });
    let rail_hover = Diffui::listen(cx, |this, entered: &bool, _, cx| {
        if *entered {
            this.on_changed_files_rail_entered();
        } else {
            this.on_changed_files_rail_exited(cx);
        }
    });
    div()
        .id("collapsed-file-navigator")
        .w(px(crate::theme::COLLAPSED_PANEL_WIDTH))
        .h_full()
        .p(px(crate::field::COLLAPSED_RAIL_PADDING))
        .bg(theme.panel_background)
        .on_hover(rail_hover)
        .child(
            div()
                .id("collapsed-file-navigator-trigger")
                .size(px(crate::theme::control::COMPACT))
                .on_hover(trigger_hover)
                .child(toggle),
        )
        .into_any_element()
}

pub(crate) fn build_diff_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let body = build_diff_panel_body(ui, theme, window, cx);
    match crate::empty_states::outside_view_banner(ui, theme) {
        Some(banner) => div()
            .flex()
            .flex_col()
            .size_full()
            .child(banner)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element(),
        None => body,
    }
}

/// A file as the diff view shows it.
pub(crate) fn file_views(files: &[diffui_core::DiffFile], theme: ThemeSpec) -> Vec<DiffFileView> {
    files
        .iter()
        .map(|file| {
            let status_color = file_status_color(file.status, theme);
            DiffFileView {
                title: match &file.old_path {
                    Some(old_path) if old_path != &file.path => {
                        format!("{old_path} -> {}", file.path)
                    }
                    _ => file.path.clone(),
                },
                status: file.status,
                status_color,
                status_fill: chip_background(status_color),
                additions: file.additions,
                deletions: file.deletions,
            }
        })
        .collect()
}

/// The hunks of the active tab's diff, by file.
fn document_hunks(ui: &Diffui, file: usize) -> &[DiffHunkView] {
    ui.active()
        .session
        .document
        .files
        .get(file)
        .map_or(&[], |file| file.hunks.as_slice())
}

/// The find matches of the active tab.
pub(crate) fn find_matches(ui: &Diffui) -> &[diff_view::FindMatch] {
    ui.active()
        .find()
        .map_or(&[], |find| find.matches.as_slice())
}

/// The diff view's find overlay, when the find bar is open. All matches wear a soft
/// wash; the *active* one is the strong fill.
pub(crate) fn find_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Option<diff_view::FindOverlay> {
    let find = ui.active().find()?;
    Some(diff_view::FindOverlay {
        matches: Slice::new(cx.entity(), find_matches),
        active: find.active,
        scroll_token: find.scroll_token,
        highlight: Color {
            a: 0.20,
            ..theme.accent
        },
        active_highlight: Color {
            a: 0.50,
            ..theme.accent
        },
    })
}

fn on_header_action(
    cx: &mut Cx,
) -> impl Fn(&HeaderAction, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::listen(cx, |this, action: &HeaderAction, _, cx| {
        this.perform(crate::commit_info::header_action(action.clone()), cx);
    })
}

fn on_copy(cx: &mut Cx) -> impl Fn(&String, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::listen(cx, |this, text: &String, _, cx| {
        this.perform(Action::Copy(text.clone()), cx);
    })
}

/// A diff view holding only `lines`, standing in where there's no diff to carry them,
/// selectable the same way.
fn header_only_view(
    ui: &Diffui,
    theme: ThemeSpec,
    lines: Vec<HeaderLine>,
    window: &mut Window,
    cx: &mut Cx,
) -> gpui_kit::Div {
    let height = diff_view::header_block_height(&lines, ui.config.code_type);
    let props = DiffProps::new(
        Vec::new(),
        DocSource::empty(),
        0,
        format!("header:{}", ui.active().session.document_id),
        diff_palette(theme),
        ui.config.mono_font,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .on_copy(on_copy(cx))
    .on_header_action(on_header_action(cx))
    .content_version(ui.document_version)
    .with_header(lines);
    div()
        .flex_none()
        .h(px(height))
        .child(diff_view::view("diff-header", props, window, cx))
}

/// A column that fills its parent, with the panel's ground.
fn panel_column(theme: ThemeSpec) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(theme.panel_background)
}

fn centered_fill() -> gpui_kit::Div {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .items_center()
        .justify_center()
}

fn build_diff_panel_body(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let tab = ui.active();
    let session = &tab.session;
    let range = crate::range::shown_range(ui);
    let compare = crate::compare::build_compare_banner(ui, theme, window, cx);
    let mut panel = panel_column(theme).children(compare);

    if let Some(error) = crate::range::refused_span(ui) {
        // The marked rows aren't one diff; say so rather than show the browsed
        // revision's as if it were theirs.
        return panel
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(crate::range::refused_state(ui, theme, error, window, cx)),
            )
            .children(crate::mode_hints::strip(ui, theme, window, cx))
            .into_any_element();
    }
    if matches!(session.status, LoadStatus::Loading) && session.document.files.is_empty() {
        return panel.into_any_element();
    }
    if session.document.files.is_empty() && session.revision_details.is_none() && range.is_none() {
        let message = match &session.status {
            LoadStatus::Failed(_) => "Failed to load changes",
            _ => "No file changes in this revision",
        };
        return panel
            .child(centered_fill().child(crate::typography::line(
                message,
                ui.config.ui_font,
                EMPTY_STATE_TEXT_SIZE,
                theme.subtle_text,
            )))
            .into_any_element();
    }

    let summary = build_revision_summary(ui, theme, window, cx);
    let title_font = crate::theme::emphasis_font(ui.config.ui_font, Weight::MEDIUM);

    if session.document.files.is_empty() {
        if let Some(range) = range {
            // An empty range has nothing to count; the message says it all.
            if range.count > 0 {
                panel = panel.child(div().flex_none().w_full().child(summary));
            }
            let lines = crate::range::header_lines(ui, range, theme);
            if !lines.is_empty() {
                panel = panel.child(header_only_view(ui, theme, lines, window, cx));
            }
            return panel
                .child(centered_fill().p(px(crate::theme::space::XXL)).child(
                    crate::typography::line(
                        crate::range::empty_message(ui, range),
                        title_font,
                        text_size::TITLE,
                        theme.muted_text,
                    ),
                ))
                .children(crate::mode_hints::strip(ui, theme, window, cx))
                .into_any_element();
        }

        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(ui::BUTTON_GAP));
        if tab.repository.is_some() {
            actions = actions.child(
                ui::text_button(
                    "empty-browse-source",
                    Some((icons::CODE, theme.muted_text)),
                    "Browse source",
                    ui::Variant::Raised,
                    ui::Size::Standard,
                    ui.config.ui_font,
                    theme,
                    cx,
                )
                .on_click(Diffui::on_action(
                    Action::SetMainView(crate::MainView::Source),
                    cx,
                )),
            );
        }
        // Offered only while there is no description, and not while the editor above is
        // already open for it.
        let describable = session.capabilities.mutate
            && tab.description_editor().is_none()
            && session
                .revision_details
                .as_ref()
                .is_some_and(|details| details.description.trim().is_empty());
        if describable {
            actions = actions.child(
                ui::text_button(
                    "empty-add-description",
                    None,
                    "Add description",
                    ui::Variant::Raised,
                    ui::Size::Standard,
                    ui.config.ui_font,
                    theme,
                    cx,
                )
                .on_click(Diffui::on_action(
                    Action::EditDescription { target: None },
                    cx,
                )),
            );
        }
        let empty = centered_fill().p(px(crate::theme::space::XXL)).child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(crate::theme::space::LG))
                .max_w(px(440.0))
                .child(
                    div()
                        .size(px(42.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(crate::theme::radius::SURFACE))
                        .bg(chip_background(theme.added_text))
                        .child(icons::icon(icons::CHECK, 22.0, theme.added_text)),
                )
                .child(crate::typography::line(
                    "This revision has no changes",
                    title_font,
                    text_size::TITLE,
                    theme.text,
                ))
                .child(div().h(px(crate::theme::space::XS)))
                .child(actions),
        );
        // No diff to carry the commit's header, so a header-only diff view stands in above
        // the empty state, selectable the same way.
        let header = session
            .revision_details
            .as_ref()
            .map(|details| {
                crate::commit_info::header_lines(ui, details, theme, true, diff_pane_width(ui))
            })
            .filter(|lines| !lines.is_empty());
        panel = panel.child(div().flex_none().w_full().child(summary));
        if let Some(lines) = header {
            panel = panel.child(header_only_view(ui, theme, lines, window, cx));
        }
        return panel.child(empty).into_any_element();
    }

    // The marker editor stands in for the diff while it is open.
    let diff_view: AnyElement = match crate::conflicts::marker_editor(ui, theme, window, cx) {
        Some(editor) => editor,
        None => {
            let props = diff_props(ui, theme, range, cx);
            diff_view::view("diff", props, window, cx)
        }
    };

    // The find bar sits on top of the diff view, pinned to the upper-right of the panel.
    let layers = div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .child(diff_view)
        .child(crate::find::build_overlay(ui, theme, window, cx));
    // The selection strip docks below the diff rather than floating over it, so the last
    // lines stay readable while something is picked; the active mode's key hints dock
    // under it.
    panel
        .child(div().flex_none().w_full().child(summary))
        .child(layers)
        .children(crate::changes::selection_bar(ui, theme, window, cx))
        .children(crate::mode_hints::strip(ui, theme, window, cx))
        .into_any_element()
}

fn diff_props(
    ui: &Diffui,
    theme: ThemeSpec,
    range: Option<&diffui_core::DiffRange>,
    cx: &mut Cx,
) -> DiffProps {
    let tab = ui.active();
    let session = &tab.session;
    let mut props = DiffProps::new(
        file_views(&session.document.files, theme),
        DocSource::new(cx.entity(), document_hunks),
        tab.selected_file,
        session.selected_revision.view_key() + &session.document_base.view_key(),
        diff_palette(theme),
        ui.config.mono_font,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .on_selected_file_changed(Diffui::listen(cx, |this, index: &usize, _, cx| {
        this.perform(Action::SelectFile(*index), cx);
    }))
    .on_copy(on_copy(cx))
    // Every scroll tick: store the offset, and leave the app out of it.
    .on_scroll(cx.listener(|this: &mut Diffui, offset: &f32, _, _| {
        this.on_diff_scrolled(*offset);
    }))
    .restore_scroll(tab.diff_scroll_offset, ui.scroll_restore_token)
    .content_version(ui.document_version)
    .layout_version(session.document_id)
    .wrap(ui.diff_wrap)
    .side_by_side(split_in_effect(ui))
    .on_header_action(on_header_action(cx));
    if let Some(range) = range {
        props = props.with_header(crate::range::header_lines(ui, range, theme));
    } else if let Some(details) = session.revision_details.as_ref() {
        props = props.with_header(crate::commit_info::header_lines(
            ui,
            details,
            theme,
            true,
            diff_pane_width(ui),
        ));
    }

    let collapsed: Vec<bool> = session
        .document
        .files
        .iter()
        .map(|file| file.collapsed.is_some())
        .collect();
    if collapsed.contains(&true) {
        props = props.on_expand_file(
            collapsed,
            Diffui::listen(cx, |this, index: &usize, _, cx| {
                this.perform(Action::ExpandFile(*index), cx);
            }),
        );
    }
    if let Some((media, key)) = crate::media::document_media(ui, &session.document) {
        props = props.media(media, key);
    }

    // Per-file "browse source" affordance: repo tabs only (a PR tab has no local tree to
    // browse).
    if tab.repository.is_some() {
        props = props.on_browse_file(Diffui::listen(cx, |this, index: &usize, _, _| {
            this.on_browse_file_from_diff(*index);
        }));
    }

    if let Some(marks) = crate::changes::diff_marks(ui, theme, cx) {
        props = props.change_marks(marks);
    }
    if let Some(actions) = crate::conflicts::hunk_actions(ui, theme, cx) {
        props = props.hunk_actions(actions);
    }
    if let Some(find) = find_overlay(ui, theme, cx) {
        props = props.with_find(find);
    }
    props
}

fn build_description_editor(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(editor) = ui
        .active()
        .description_editor()
        .filter(|editor| editor.target == ui.active().session.selected_revision)
    else {
        return div().into_any_element();
    };
    let saving = editor.saving_activity.is_some();
    let content = editor.text();
    let mut spec = crate::input::FieldSpec::new(DESCRIPTION_EDITOR_ID, &content).placeholder(
        if editor.commit {
            "Describe the change you are committing\u{2026}"
        } else {
            "Describe this revision\u{2026}"
        },
    );
    // Read-only while saving.
    if !saving {
        let on_edit = Diffui::listen(cx, |this, text: &String, _, _| {
            this.on_description_edited(text.clone());
        });
        spec = spec.on_input(move |text, window, cx| on_edit(&text, window, cx));
    }
    let input = div()
        .h(px(description_editor_input_height(ui)))
        .font(ui.config.mono_font.gpui())
        .text_size(px(ui.config.code_type.size))
        .child(
            crate::input::multi_line(spec, ui.config.mono_font, window, cx)
                .h(px(description_editor_input_height(ui))),
        );

    let cancel = ui::cancel_button(
        "description-cancel",
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
        cx,
    )
    .disabled(saving)
    .on_click(Diffui::on_action(Action::CancelDescription, cx));

    // A commit may keep the description as it stands.
    let save_enabled = !saving && (editor.is_dirty() || editor.commit);
    let save_label = match (saving, editor.commit) {
        (true, true) => "Committing\u{2026}",
        (true, false) => "Saving\u{2026}",
        (false, true) => "Commit",
        (false, false) => "Save",
    };
    let save = ui::primary_button(
        "description-save",
        save_label,
        save_enabled,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
        cx,
    )
    .on_click(Diffui::on_action(Action::SaveDescription, cx));

    // The keys live in the buttons' tooltips; the strip only speaks up when the editor is
    // holding a revision switch back.
    let hint_text = |value: &'static str, color: Color| {
        crate::typography::centered(
            crate::typography::line(value, ui.config.ui_font, text_size::UI, color),
            ui.config.ui_font,
            text_size::UI,
        )
        .into_any_element()
    };
    let hint = if editor.switch_blocked {
        Some(hint_text(
            "Save or cancel before switching revisions",
            theme.note_text,
        ))
    } else if editor.commit {
        Some(hint_text(
            "Then starts a new change on top",
            theme.subtle_text,
        ))
    } else {
        None
    };
    let keyed =
        |id: &'static str, button: gpui_kit::component::button::Button, key: &'static str| {
            div()
                .id(id)
                .child(button)
                .tooltip(ui::tooltip(key, ui.config.ui_font, theme))
                .into_any_element()
        };
    let actions = ui::button_row(
        hint,
        [
            keyed("description-cancel-tip", cancel, "Esc"),
            keyed("description-save-tip", save, "\u{2318}\u{21b5}"),
        ],
    );
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(DESCRIPTION_EDITOR_GAP))
        .w_full()
        .py(px(10.0))
        .px(px(crate::theme::space::XL))
        .bg(theme.panel_background)
        .child(input)
        .child(actions)
        .into_any_element()
}

/// The commit info block above the diff (see [`crate::commit_info`]), or the description
/// editor while it is open for this revision.
fn build_revision_summary(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let session = &ui.active().session;
    if ui
        .active()
        .description_editor()
        .is_some_and(|editor| editor.target == session.selected_revision)
    {
        return build_description_editor(ui, theme, window, cx);
    }
    crate::commit_info::view(ui, theme, window, cx)
}

#[cfg(test)]
#[path = "diff_scenes.rs"]
mod scenes;

#[cfg(test)]
mod tests {

    #[test]
    fn description_height_counts_newlines_and_wrapping() {
        let size = 12.0;
        let line_height = size * crate::measure::LINE_HEIGHT_MULTIPLIER;
        let count = |content: &str, width: f32| {
            crate::measure::wrapped_line_count(
                content,
                size,
                crate::theme::Font::MONOSPACE,
                line_height,
                width,
            )
        };
        let wide = 10_000.0;
        assert_eq!(count("", wide), 1);
        assert_eq!(count("one\ntwo", wide), 2);
        assert_eq!(count("short", wide), 1);
        // A line well past the width must wrap into several visual lines.
        let char_width =
            crate::measure::line_width("M", size, crate::theme::Font::MONOSPACE).max(1.0);
        assert!(count(&"word ".repeat(40), char_width * 12.0) > 2);
    }
}
