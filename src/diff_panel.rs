use crate::Cx;
use crate::Diffui;
use crate::diff_view::{self};
use crate::revision_list::FileRowView;
use crate::theme::{Color, Font};
use crate::theme::{ThemeSpec, chip_background, file_status_color, text_size};
use diffui_core::FileTreeRow;
use gpui_kit::{AnyElement, Window};
use jj_lib::graph::GraphEdgeType;
use std::rc::Rc;

const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
pub(crate) const DESCRIPTION_EDITOR_ID: &str = "revision-description-editor";
const DESCRIPTION_EDITOR_GAP: f32 = 8.0;
pub const DEFAULT_FILE_NAV_WIDTH: f32 = 236.0;
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
            let file = &files[*file_index];
            (
                label.clone(),
                file.path.clone(),
                *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
                None,
                *file_index,
                file.status.short_label().to_owned(),
                file_status_color(file.status, theme),
                file.additions,
                file.deletions,
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

/// Stub: the diff_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_file_navigator(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("diff_panel::build_file_navigator")
}

/// Stub: the diff_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_file_navigator_peek(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("diff_panel::build_file_navigator_peek")
}

/// Stub: the diff_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_collapsed_file_navigator(
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("diff_panel::build_collapsed_file_navigator")
}

/// Stub: the diff_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_diff_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("diff_panel::build_diff_panel")
}
