//! The source-browser view: a GitHub-style "browse the repo at this revision"
//! lens over the active tab. The sidebar is the full file tree (the browsed
//! revision itself is named in the toolbar and the pane's info bar); the main
//! pane renders the selected file through the shared
//! [`DiffView`] widget in its plain mode (single line-number column, no
//! file/hunk header strips), so selection, copy, find, and wrap all behave
//! exactly like the diff view.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Window, div, px};
use jj_lib::graph::GraphEdgeType;
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::collections::HashSet;
use std::rc::Rc;

use crate::geometry::{Point, Rectangle};
use crate::revision_list::{
    self, FileRowView, RevisionListProps, RevisionRowView, RowSelectionKey,
};
use crate::theme::{Color, Padding, ThemeSpec, chip_background, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Cx, Diffui, SourceState, ellipsis, icons};
use diffui_core::{
    RevisionSelection, SourceEntry, SourceEntryStatus, SourceTreeRow, source_tree_rows,
};

// Matches the diff panel's code size — both panes render "code".
const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
const BAR_TEXT_SIZE: f32 = text_size::BODY;

/// View-time memo for the source tree, mirroring
/// [`sidebar::SidebarFileCache`]'s rationale: the pane renders on every
/// change it shows, and re-composing + re-flattening a whole-repo listing (tens of
/// thousands of entries) each time would dominate frame cost. Two layers:
/// the *composed entries* (the base listing + every lazily-listed ignored
/// dir's children) rebuild only when the tree data changes; the flattened
/// *rows* also rebuild on expand/collapse.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceTreeCache {
    /// `(version, tree_epoch)` the composed entries were built from.
    entries_key: Option<(u64, u64)>,
    entries: Rc<Vec<SourceEntry>>,
    /// `(version, tree_epoch, expanded)` the rows were built from.
    rows_key: Option<(u64, u64, HashSet<String>)>,
    rows: Rc<Vec<SourceTreeRow>>,
    /// `(version, tree_epoch, filter)` the fuzzy match list was built from.
    filtered_key: Option<(u64, u64, String)>,
    filtered: Rc<Vec<SourceTreeRow>>,
}

impl SourceTreeCache {
    /// The composed entry list + its flattened display rows — the tree, or
    /// (while the sidebar's search box holds a query) the flat fuzzy-ranked
    /// match list. Row `entry_index`es point into the returned entries, so
    /// the two must always be consumed as a pair; every consumer (view,
    /// clicks, keyboard nav, context menus) goes through here, which is what
    /// keeps them agreeing on what a display index means while filtering.
    pub(crate) fn entries_and_rows(
        &mut self,
        source: &SourceState,
    ) -> (Rc<Vec<SourceEntry>>, Rc<Vec<SourceTreeRow>>) {
        let entries_key = (source.version, source.tree_epoch);
        if self.entries_key != Some(entries_key) {
            self.entries = Rc::new(compose_entries(
                source.tree.as_deref().unwrap_or(&[]),
                &source.dir_children,
            ));
            self.entries_key = Some(entries_key);
            self.rows_key = None;
            self.filtered_key = None;
        }
        let query = source.filter.trim();
        if !query.is_empty() {
            let filtered_key = (source.version, source.tree_epoch, query.to_owned());
            if self.filtered_key.as_ref() != Some(&filtered_key) {
                self.filtered = Rc::new(fuzzy_file_rows(&self.entries, query));
                self.filtered_key = Some(filtered_key);
            }
            return (Rc::clone(&self.entries), Rc::clone(&self.filtered));
        }
        let rows_key = (source.version, source.tree_epoch, source.expanded.clone());
        if self.rows_key.as_ref() != Some(&rows_key) {
            self.rows = Rc::new(source_tree_rows(&self.entries, &source.expanded));
            self.rows_key = Some(rows_key);
        }
        (Rc::clone(&self.entries), Rc::clone(&self.rows))
    }
}

/// Fuzzy-rank every file entry against `query` (nucleo, path-tuned config —
/// the same matcher the palette uses) into a flat list of full-path rows,
/// best first, alphabetical among ties. Directories don't match — the search
/// finds files to open. Capped: past a few hundred rows nobody is scanning
/// the tail, they're typing more query.
fn fuzzy_file_rows(entries: &[SourceEntry], query: &str) -> Vec<SourceTreeRow> {
    const MAX_RESULTS: usize = 400;
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let needle = Utf32String::from(query);
    let mut scored: Vec<(u16, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.is_dir)
        .filter_map(|(index, entry)| {
            let haystack = Utf32String::from(entry.path.as_str());
            matcher
                .fuzzy_match(haystack.slice(..), needle.slice(..))
                .map(|score| (score, index))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| entries[a.1].path.cmp(&entries[b.1].path))
    });
    scored.truncate(MAX_RESULTS);
    scored
        .into_iter()
        .map(|(_, entry_index)| SourceTreeRow::File {
            entry_index,
            // Full path as the label: ranked results are context-free
            // without their tree around them.
            label: entries[entry_index].path.clone(),
            depth: 0,
            status: entries[entry_index].status,
            change: entries[entry_index].change,
        })
        .collect()
}

/// Base listing + lazily-listed ignored-dir children, as one entry list.
/// A dir's children only join while its unenumerated marker is still present
/// (walking parents before children), so children saved from a previous
/// listing can't orphan themselves under a re-listed tree where the dir is
/// no longer ignored. Order doesn't matter — the row builder sorts.
fn compose_entries(
    tree: &[SourceEntry],
    dir_children: &std::collections::HashMap<String, Vec<SourceEntry>>,
) -> Vec<SourceEntry> {
    if dir_children.is_empty() {
        return tree.to_vec();
    }
    let mut out = tree.to_vec();
    let mut markers: HashSet<&str> = tree
        .iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.path.as_str())
        .collect();
    let mut dirs: Vec<&String> = dir_children.keys().collect();
    dirs.sort();
    for dir in dirs {
        if !markers.contains(dir.as_str()) {
            continue;
        }
        for child in &dir_children[dir] {
            if child.is_dir {
                markers.insert(child.path.as_str());
            }
            out.push(child.clone());
        }
    }
    out
}

/// The revision the browser shows (it defaults to the working copy until an
/// explicit browse pins one).
pub(crate) fn browsed_revision(source: &SourceState) -> RevisionSelection {
    source.revision.clone().unwrap_or_default()
}

/// Focus target id for the sidebar's file-search input.
pub const SOURCE_FILTER_INPUT_ID: &str = "source-filter-input";

/// A blank [`RevisionRowView`] for the files-only source list's revision
/// builder slot — never rendered (the list has zero revision rows).
pub(crate) fn empty_revision_row(theme: ThemeSpec) -> RevisionRowView {
    let frame = diffui_core::graph::LaneFrame::solo();
    let columns = frame.display_columns();
    RevisionRowView {
        selection_key: RowSelectionKey::WorkingCopy,
        change_id_prefix: String::new(),
        change_id_suffix: String::new(),
        commit_id_short: String::new(),
        author: String::new(),
        age: String::new(),
        description: String::new(),
        description_color: theme.text,
        bookmark_chips: Vec::new(),
        status_chips: Vec::new(),
        lane_color: theme.lane_base,
        prev_columns: columns.clone(),
        columns,
        frame,
        lane_labels: Vec::new(),
        lane_segments_before: Vec::new(),
        lane_segments_after: Vec::new(),
        collapse_chevron: None,
        draft_source: false,
        multi_selected: false,
        draft_marker: None,
        inert: false,
        ghost: false,
        node: crate::graph_view::NodeKind::Mutable,
    }
}

/// One display row of the source tree, lowered to the shared file-row view.
/// Changed tracked files chip their diff status (`A`/`M`/`C`) and untracked
/// files a green `U`, like `jj status`; ignored files and directories render
/// dimmed.
fn source_file_row(
    row: &SourceTreeRow,
    entries: &[SourceEntry],
    theme: ThemeSpec,
    continuation: Rc<[Option<GraphEdgeType>]>,
    columns: Rc<[Option<usize>]>,
    lane_labels: Rc<[Vec<String>]>,
    lane_segments: Rc<[Option<usize>]>,
) -> FileRowView {
    let (primary, raw_path, indent, chevron, file_index, primary_color, icon_color, chip) =
        match row {
            SourceTreeRow::Dir {
                label,
                path,
                depth,
                collapsed,
                ignored,
                has_changes,
                ..
            } => (
                label.clone(),
                path.clone(),
                *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
                Some(*collapsed),
                usize::MAX,
                if *ignored {
                    theme.subtle_text
                // "Changes somewhere inside" — the design system's amber
                // (VSCode's dirty-folder gold), so a collapsed dir still
                // points at where the work is.
                } else if *has_changes {
                    theme.modified_token
                } else {
                    theme.text
                },
                theme.subtle_text,
                None,
            ),
            SourceTreeRow::File {
                entry_index,
                label,
                depth,
                status,
                change,
            } => {
                let chip = match (status, change) {
                    (SourceEntryStatus::Untracked, _) => Some(("U", theme.added_text)),
                    (_, Some(change)) => Some((
                        change.short_label(),
                        crate::theme::file_status_color(*change, theme),
                    )),
                    _ => None,
                };
                // A chipped row's name (and icon) carries the chip's color,
                // so a changed file reads as one colored piece rather than a
                // plain name with a colored tag hanging off it.
                let name_color = match (chip, status) {
                    (Some((_, color)), _) => color,
                    (None, SourceEntryStatus::Ignored) => theme.subtle_text,
                    (None, _) => theme.text,
                };
                (
                    label.clone(),
                    // Full path for the hover tooltip.
                    entries
                        .get(*entry_index)
                        .map(|entry| entry.path.clone())
                        .unwrap_or_default(),
                    *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
                    None,
                    *entry_index,
                    name_color,
                    if chip.is_some() {
                        name_color
                    } else {
                        theme.subtle_text
                    },
                    chip,
                )
            }
        };
    FileRowView {
        primary,
        raw_path,
        status_label: chip.map(|(label, _)| label.to_owned()).unwrap_or_default(),
        status_background: chip
            .map(|(_, color)| chip_background(color))
            .unwrap_or(crate::theme::TRANSPARENT),
        status_text: chip.map(|(_, color)| color).unwrap_or(theme.text),
        additions: 0,
        deletions: 0,
        additions_text: theme.added_text,
        deletions_text: theme.removed_text,
        continuation,
        columns,
        additions_width: 0.0,
        deletions_width: 0.0,
        primary_color,
        icon_color,
        indent,
        chevron,
        file_index,
        lane_labels,
        lane_segments,
        check: None,
        tag: None,
        struck: false,
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_lines(count: usize) -> String {
    if count == 1 {
        "1 line".to_owned()
    } else {
        format!("{count} lines")
    }
}

/// The fuzzy file-search box at the top of the source sidebar — the source
/// view's counterpart of the diff sidebar's revset input, built from the
/// same [`crate::field::filter_field`] (caretless variant) so both sidebars'
/// top bars match. Typing filters the tree into a ranked match list;
/// Enter opens the best match.
fn build_source_filter(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::field::sidebar_filter_field(
        theme,
        ui.config.mono_font,
        crate::field::FilterField {
            id: SOURCE_FILTER_INPUT_ID,
            leading_icon: Some(icons::SEARCH),
            placeholder: "Search files",
            value: &ui.active().source.filter,
            on_input: |this, query, _| this.on_source_filter_changed(query),
            on_submit: Some(|this, _| this.on_source_filter_submit()),
            caret: None,
        },
        window,
        cx,
    )
}

/// Sidebar for the source browser: the fuzzy file-search box, then the full
/// file tree — no revision rows, no revset filter, no branch footer.
pub(crate) fn build_source_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let source = &ui.active().source;
    let mut body = div()
        .flex()
        .flex_col()
        .size_full()
        .bg(theme.panel_background)
        .child(
            div()
                .w_full()
                .flex_none()
                .min_h(px(crate::field::PANEL_HEADER_HEIGHT))
                .child(build_source_filter(ui, theme, window, cx)),
        );

    if let Some(error) = &source.tree_error {
        body = body.child(
            div()
                .w_full()
                .flex_none()
                .py(px(8.0))
                .px(px(12.0))
                .font(ui.config.ui_font.gpui())
                .text_size(px(text_size::BODY))
                .text_color(theme.removed_text)
                .child(format!("Failed to list files: {error}")),
        );
    }

    let (entries, rows) = ui
        .active()
        .source_tree_cache
        .borrow_mut()
        .entries_and_rows(source);
    let row_count = rows.len();

    let selected_entry = source.selected.as_deref().and_then(|path| {
        entries
            .iter()
            .position(|entry| !entry.is_dir && entry.path == path)
    });
    // Flat sidebar row of the selected file, for the jump-into-browser
    // reveal — file rows fill the whole list, so display index == flat index.
    let reveal_file_flat = selected_entry.and_then(|entry_index| {
        rows.iter().position(
            |row| matches!(row, SourceTreeRow::File { entry_index: e, .. } if *e == entry_index),
        )
    });

    // Status chips only exist at the working copy; reserve the badge column
    // there so names align whether or not a given row carries one.
    let badge_width = if matches!(browsed_revision(source), RevisionSelection::WorkingCopy) {
        crate::chip::width("M", None, ui.config.mono_font)
    } else {
        0.0
    };

    let empty_continuation: Rc<[Option<GraphEdgeType>]> = Vec::new().into();
    let empty_columns: Rc<[Option<usize>]> = Vec::new().into();
    let empty_labels: Rc<[Vec<String>]> = Vec::new().into();
    let empty_segments: Rc<[Option<usize>]> = Vec::new().into();
    // Rows are built at paint, for on-screen rows only. The cache hands out the
    // pair the list was sized from, so these can't disagree with each other.
    let build_file = Rc::new(move |display: usize, _: &App| {
        source_file_row(
            &rows[display],
            &entries,
            theme,
            empty_continuation.clone(),
            empty_columns.clone(),
            empty_labels.clone(),
            empty_segments.clone(),
        )
    });

    let props = RevisionListProps::new(
        0,
        (row_count > 0).then_some((0, row_count)),
        crate::sidebar::revision_list_style(theme, ui.config, badge_width),
    )
    .files(
        build_file,
        selected_entry,
        Diffui::listen(cx, |this, row: &usize, _, cx| {
            this.on_source_sidebar_row(*row, cx);
        }),
    )
    .at_window_edge()
    .reveal_file(source.reveal_token, reveal_file_flat)
    // Every scroll tick: stored, with no reconcile and no redraw.
    .on_scroll(cx.listener(|this: &mut Diffui, offset: &f64, _, _| {
        this.on_source_tree_scrolled(*offset);
    }))
    .top_fade(theme.panel_background)
    .restore_scroll(source.tree_scroll_offset, ui.scroll_restore_token)
    .on_file_context_menu(Diffui::listen(
        cx,
        |this, (row, rect, point): &(usize, Rectangle, Point), _, cx| {
            this.open_file_context_menu(*row, *rect, *point, cx);
        },
    ));

    body = body.child(div().flex_1().min_h_0().w_full().child(revision_list::view(
        "source-tree",
        props,
        window,
        cx,
    )));

    // A thin status line while the listing loads (the list above shows just
    // the header row meanwhile).
    if source.tree.is_none() && source.tree_error.is_none() {
        body = body.child(div().w_full().flex_none().py(px(6.0)).px(px(12.0)).child(
            typography::line(
                "Listing files…",
                ui.config.ui_font,
                text_size::BODY,
                theme.subtle_text,
            ),
        ));
    }

    body.into_any_element()
}

/// The main source pane: an info bar (revision · path · counts) above the
/// plain code view, with the find bar overlaid like the diff panel's.
pub(crate) fn build_source_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let source = &ui.active().source;
    let info_bar = build_info_bar(ui, theme, window, cx);

    let body: AnyElement = if let Some(error) = &source.file_error {
        centered_note(
            ui,
            format!("Couldn't load file: {error}"),
            theme.removed_text,
        )
    } else if let Some(view) = &source.file {
        if view.binary {
            centered_note(
                ui,
                format!("Binary file ({}), not shown", format_bytes(view.byte_len)),
                theme.subtle_text,
            )
        } else if view.too_large {
            centered_note(
                ui,
                format!(
                    "File is too large to display ({})",
                    format_bytes(view.byte_len)
                ),
                theme.subtle_text,
            )
        } else {
            source_code(ui, view, theme, window, cx)
        }
    } else if source.loading.is_some() {
        // Content on its way; a blank pane beats a flash of placeholder text.
        div().into_any_element()
    } else {
        centered_note(
            ui,
            "Select a file to view its source".to_owned(),
            theme.subtle_text,
        )
    };

    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(theme.panel_background)
        .child(info_bar)
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(body)
                .child(crate::find::build_overlay(ui, theme, window, cx)),
        )
        .into_any_element()
}

/// The file through the shared diff view, in its plain mode.
fn source_code(
    ui: &Diffui,
    view: &crate::SourceFileView,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    use crate::diff_view::{self, DiffFileView, DiffProps, DocSource};
    let source = &ui.active().source;
    let files = vec![DiffFileView {
        title: view.file.path.clone(),
        status: diffui_core::DiffFileStatus::Modified,
        status_color: theme.info,
        status_fill: chip_background(theme.info),
        additions: 0,
        deletions: 0,
        tag: None,
        note: None,
    }];
    let revision_key = format!(
        "source:{}:{}",
        browsed_revision(source).view_key(),
        view.file.path
    );
    // The source document is a single file; the diff view's scroll-driven file
    // tracking has nothing to report, so `on_selected_file_changed` stays unset.
    let doc = DocSource::new(cx.entity(), |ui: &Diffui, _| {
        ui.active()
            .source
            .file
            .as_ref()
            .map_or(&[][..], |view| &view.file.hunks)
    });
    let mut props = DiffProps::new(
        files,
        doc,
        0,
        revision_key,
        crate::theme::diff_palette(theme),
        ui.config.mono_font,
        ui.config.code_type,
        ui.config.multi_click_ms,
    )
    .plain(true)
    .wrap(ui.diff_wrap)
    .on_copy(Diffui::listen(cx, |this, text: &String, _, cx| {
        this.perform(Action::Copy(text.clone()), cx);
    }))
    .on_scroll(cx.listener(|this: &mut Diffui, offset: &f32, _, _| {
        this.on_source_scrolled(*offset);
    }))
    .restore_scroll(source.scroll_offset, ui.scroll_restore_token)
    .content_version(ui.document_version)
    .layout_version(view.doc_id);
    if let Some(gutter) = crate::history_panel::source_annotations(ui, cx) {
        props = props.annotations(gutter);
    }
    // Soft wash for every match, strong fill for the active one: the diff
    // panel's find colours.
    if let Some(find) = crate::diff_panel::find_overlay(ui, theme, cx) {
        props = props.with_find(find);
    }
    diff_view::view("source-code", props, window, cx)
}

fn centered_note(ui: &Diffui, message: String, color: Color) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(typography::line(
            message,
            ui.config.ui_font,
            EMPTY_STATE_TEXT_SIZE,
            color,
        ))
        .into_any_element()
}

/// The strip above the code: the selected path on the left (directory
/// dimmed, like a diff file header), line/byte counts and an
/// ignored/untracked marker on the right. The browsed revision is already
/// in the toolbar, so it isn't repeated here.
fn build_info_bar(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let source = &ui.active().source;
    let mono = ui.config.mono_font;

    // The path (mono), the size and status captions (sans) and the status
    // chip sit on the caps' centre line (see `crate::typography`).
    let font = ui.config.ui_font;
    let caption = |value: String, color: Color| {
        Aligned::text(
            typography::line(value, font, text_size::BODY, color),
            font,
            text_size::BODY,
        )
    };

    // Right side: the selected entry's status (when notable) + size counts.
    let mut right: Vec<Aligned> = Vec::new();
    let status = source.selected.as_deref().and_then(|path| {
        let (entries, _) = ui
            .active()
            .source_tree_cache
            .borrow_mut()
            .entries_and_rows(source);
        entries
            .iter()
            .find(|entry| !entry.is_dir && entry.path == path)
            .map(|entry| entry.status)
    });
    match status {
        Some(SourceEntryStatus::Ignored) => {
            right.push(status_chip_text("ignored", theme.subtle_text, ui));
        }
        Some(SourceEntryStatus::Untracked) => {
            right.push(status_chip_text("untracked", theme.added_text, ui));
        }
        _ => {}
    }
    if let Some((status, failed)) = crate::history_panel::annotation_status(ui) {
        right.push(caption(
            status,
            if failed {
                theme.removed_text
            } else {
                theme.subtle_text
            },
        ));
    }
    if let Some(view) = &source.file {
        let detail = if view.binary || view.too_large {
            format_bytes(view.byte_len)
        } else {
            format!(
                "{} · {}",
                format_lines(view.line_count),
                format_bytes(view.byte_len)
            )
        };
        right.push(caption(detail, theme.subtle_text));
    } else if source.loading.is_some() {
        right.push(caption("Loading…".to_owned(), theme.subtle_text));
    }

    let mut bar = div()
        .flex_none()
        .w_full()
        .h(px(crate::field::PANEL_HEADER_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(crate::diff_view::HEADER_HORIZONTAL_PADDING))
        .bg(theme.panel_background);
    match &source.selected {
        Some(path) => {
            let (dir, name) = match path.rfind('/') {
                Some(slash) => path.split_at(slash + 1),
                None => ("", path.as_str()),
            };
            bar = bar
                .child(icons::icon(icons::FILE, BAR_TEXT_SIZE, theme.subtle_text))
                .child(
                    typography::centered(
                        ellipsis::Line::new(
                            vec![
                                ellipsis::Span::new(dir.to_owned()).color(theme.subtle_text),
                                ellipsis::Span::new(name.to_owned()),
                            ],
                            mono,
                            BAR_TEXT_SIZE,
                            theme.text,
                        )
                        .view(),
                        mono,
                        BAR_TEXT_SIZE,
                    )
                    .flex_1()
                    .min_w_0(),
                );
        }
        None => bar = bar.child(div().flex_1()),
    }
    bar = bar.child(
        div()
            .flex_none()
            .child(typography::baseline_row(right, 8.0)),
    );
    if crate::history_panel::can_annotate(ui) {
        bar = bar.child(crate::history_panel::source_bar_actions(
            ui, theme, window, cx,
        ));
    }
    bar.into_any_element()
}

fn status_chip_text(label: &'static str, color: Color, ui: &Diffui) -> Aligned {
    let mono = ui.config.mono_font;
    let size = typography::optical_size(mono, text_size::CAPTION);
    Aligned::chip(
        typography::line(label, mono, size, color),
        mono,
        size,
        Padding::from([1.0, 6.0]),
        move |chip| {
            chip.bg(chip_background(color))
                .rounded(px(crate::chip::RADIUS))
        },
    )
}
