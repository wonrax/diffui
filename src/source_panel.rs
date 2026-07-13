//! The source-browser view: a GitHub-style "browse the repo at this revision"
//! lens over the active tab. The sidebar is the full file tree (the browsed
//! revision itself is named in the toolbar and the pane's info bar); the main
//! pane renders the selected file through the shared
//! [`DiffView`] widget in its plain mode (single line-number column, no
//! file/hunk header strips), so selection, copy, find, and wrap all behave
//! exactly like the diff view.

use std::collections::HashSet;
use std::rc::Rc;

use iced::{
    Background, Color, Element, Length, Padding, alignment,
    widget::{Space, column, container, row, stack, text},
};
use jj_lib::graph::GraphEdgeType;
use nucleo_matcher::{Config, Matcher, Utf32String};

use crate::diff_view::{DiffFileView, DiffView};
use crate::find;
use crate::icons;
use crate::revision_list::{FileRowView, RevisionList, RevisionRowView, RowSelectionKey};
use crate::sidebar;
use crate::theme::{
    ThemeSpec, chip_background, diff_palette, diff_panel_style, sidebar_panel_style, text_size,
};
use crate::{Diffui, Message, SourceState};
use diffui_core::{
    DiffFileStatus, RevisionSelection, SourceEntry, SourceEntryStatus, SourceTreeRow,
    source_tree_rows,
};

// Matches the diff panel's code size — both panes render "code".
const CODE_TEXT_SIZE: f32 = 12.0;
const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
const BAR_TEXT_SIZE: f32 = text_size::BODY;

/// View-time memo for the source tree, mirroring
/// [`sidebar::SidebarFileCache`]'s rationale: `view()` rebuilds on every
/// message, and re-composing + re-flattening a whole-repo listing (tens of
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

fn header_clicked(_key: RowSelectionKey) -> Message {
    Message::SourceHeaderClicked
}

fn source_view_file_changed(_index: usize) -> Message {
    // The source document is a single file; the diff view's scroll-driven
    // file tracking has nothing to report.
    Message::SourceHeaderClicked
}

/// Focus target id for the sidebar's file-search input.
pub const SOURCE_FILTER_INPUT_ID: &str = "source-filter-input";

/// The fuzzy file-search box at the top of the source sidebar — the source
/// view's counterpart of the diff sidebar's revset input, built from the
/// same [`crate::field::filter_field`] (caretless variant) so both sidebars'
/// top bars match. Typing filters the tree into a ranked match list;
/// Enter opens the best match.
fn build_source_filter(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    crate::field::sidebar_filter_field(
        theme,
        ui.config.mono_font,
        crate::field::FilterField {
            id: SOURCE_FILTER_INPUT_ID,
            placeholder: "search files — fuzzy",
            value: &ui.source.filter,
            on_input: Message::SourceFilterChanged,
            on_submit: Some(Message::SourceFilterSubmit),
            caret: None,
        },
    )
}

/// Sidebar for the source browser: the fuzzy file-search box, then the full
/// file tree — no revision rows, no revset filter, no branch footer.
pub fn build_source_sidebar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let source = &ui.source;
    let mut body = column![].spacing(0);
    body = body.push(build_source_filter(ui, theme));

    if let Some(error) = &source.tree_error {
        body = body.push(
            container(
                text(format!("Failed to list files: {error}"))
                    .size(text_size::BODY)
                    .font(ui.config.ui_font)
                    .color(theme.removed_text),
            )
            .width(Length::Fill)
            .padding([8, 12]),
        );
    }

    let (entries, rows) = ui.source_tree_cache.borrow_mut().entries_and_rows(source);
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

    let revision = browsed_revision(source);
    // Never called — with zero revision rows the widget only materializes
    // file rows — but the builder slot still wants a value.
    let build_revision = Box::new(move |_index: usize| empty_revision_row(theme));

    // Status chips only exist at the working copy; reserve the badge column
    // there so names align whether or not a given row carries one.
    let badge_width = if matches!(revision, RevisionSelection::WorkingCopy) {
        crate::chip::width("M", None, ui.config.mono_font)
    } else {
        0.0
    };

    let rows_for_files = Rc::clone(&rows);
    let entries_for_files = Rc::clone(&entries);
    let empty_continuation: Rc<[Option<GraphEdgeType>]> = Vec::new().into();
    let empty_columns: Rc<[Option<usize>]> = Vec::new().into();
    let empty_labels: Rc<[Vec<String>]> = Vec::new().into();
    let empty_segments: Rc<[Option<usize>]> = Vec::new().into();
    let build_file = Box::new(move |display: usize| {
        source_file_row(
            &rows_for_files[display],
            &entries_for_files,
            theme,
            empty_continuation.clone(),
            empty_columns.clone(),
            empty_labels.clone(),
            empty_segments.clone(),
        )
    });

    let list = RevisionList::new(
        0,
        (row_count > 0).then_some((0, row_count)),
        build_revision,
        build_file,
        None,
        selected_entry,
        None,
        sidebar::revision_list_style(theme, ui.config, badge_width),
        header_clicked,
        Message::SourceSidebarRow,
    )
    .width(Length::Fill)
    .reveal_file(source.reveal_token, reveal_file_flat)
    .on_scroll(Message::SourceTreeScrolled)
    .restore_scroll(source.tree_scroll_offset, ui.scroll_restore_token)
    .on_context_menu(Message::RevisionContextMenu)
    .on_file_context_menu(Message::SidebarFileContextMenu);

    body = body.push(list);

    // A thin status line while the listing loads (the list above shows just
    // the header row meanwhile).
    if source.tree.is_none() && source.tree_error.is_none() {
        body = body.push(
            container(
                text("Listing files…")
                    .size(text_size::UI)
                    .font(ui.config.ui_font)
                    .color(theme.subtle_text),
            )
            .width(Length::Fill)
            .padding([6, 12]),
        );
    }

    container(body)
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| sidebar_panel_style(theme))
        .into()
}

/// A blank [`RevisionRowView`] for the files-only source list's revision
/// builder slot — never rendered (the list has zero revision rows).
fn empty_revision_row(theme: ThemeSpec) -> RevisionRowView {
    let frame = diffui_core::graph::LaneFrame::solo();
    let columns = frame.display_columns();
    RevisionRowView {
        selection_key: RowSelectionKey::WorkingCopy,
        change_id_prefix: String::new(),
        change_id_suffix: String::new(),
        commit_id_short: String::new(),
        author: String::new(),
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
                    match status {
                        SourceEntryStatus::Tracked => theme.text,
                        SourceEntryStatus::Untracked => theme.added_text,
                        SourceEntryStatus::Ignored => theme.subtle_text,
                    },
                    // The icon follows the row's tone so untracked/ignored
                    // read as one piece with their name.
                    match status {
                        SourceEntryStatus::Tracked => theme.subtle_text,
                        SourceEntryStatus::Untracked => theme.added_text,
                        SourceEntryStatus::Ignored => theme.subtle_text,
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
            .unwrap_or(Color::TRANSPARENT),
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
    }
}

/// The main source pane: an info bar (revision · path · counts) above the
/// plain code view, with the find bar overlaid like the diff panel's.
pub fn build_source_panel<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let source = &ui.source;
    let info_bar = build_info_bar(ui, theme);

    let body: Element<'a, Message> = if let Some(error) = &source.file_error {
        centered_note(format!("Couldn't load file: {error}"), theme.removed_text)
    } else if let Some(view) = &source.file {
        if view.binary {
            centered_note(
                format!("Binary file ({}) — not shown", format_bytes(view.byte_len)),
                theme.subtle_text,
            )
        } else if view.too_large {
            centered_note(
                format!(
                    "File is too large to display ({})",
                    format_bytes(view.byte_len)
                ),
                theme.subtle_text,
            )
        } else {
            let files = vec![DiffFileView {
                title: view.file.path.clone(),
                status: DiffFileStatus::Modified,
                status_color: theme.info,
                status_fill: chip_background(theme.info),
                hunks: &view.file.hunks,
                additions: 0,
                deletions: 0,
            }];
            let revision_key = format!(
                "source:{}:{}",
                browsed_revision(source).view_key(),
                view.file.path
            );
            let mut code = DiffView::new(
                files,
                0,
                revision_key,
                diff_palette(theme),
                ui.config.mono_font,
                CODE_TEXT_SIZE,
                ui.config.multi_click_ms,
                source_view_file_changed,
            )
            .plain(true)
            .wrap(ui.diff_wrap)
            .on_copy(Message::CopyToClipboard)
            .on_scroll(Message::SourceScrolled)
            .restore_scroll(source.scroll_offset, ui.scroll_restore_token)
            .content_version(ui.document_version)
            .layout_version(view.doc_id);

            if let Some(find_state) = &ui.find {
                code = code.with_find(crate::diff_view::FindOverlay {
                    matches: &find_state.matches,
                    active: find_state.active,
                    scroll_token: find_state.scroll_token,
                    // Soft wash for every match, strong fill for the active
                    // one — mirrors `diff_panel`'s find colors.
                    highlight: Color {
                        a: 0.20,
                        ..theme.accent
                    },
                    active_highlight: Color {
                        a: 0.50,
                        ..theme.accent
                    },
                });
            }
            code.into()
        }
    } else if source.loading.is_some() {
        // Content on its way; a blank pane beats a flash of placeholder text.
        Space::new().into()
    } else {
        centered_note(
            "Select a file to view its source".to_owned(),
            theme.subtle_text,
        )
    };

    let find_overlay = find::build_overlay(ui, theme);
    let body_with_find: Element<'a, Message> = stack![body, find_overlay].into();

    container(column![info_bar, body_with_find].spacing(0))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

fn centered_note<'a>(message: String, color: Color) -> Element<'a, Message> {
    container(text(message).size(EMPTY_STATE_TEXT_SIZE).color(color))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

/// The strip above the code: browse glyph + revision label + selected path on
/// the left, line/byte counts (and an ignored/untracked marker) on the right.
fn build_info_bar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let source = &ui.source;
    let mono = ui.config.mono_font;

    let revision_label = match browsed_revision(source) {
        RevisionSelection::WorkingCopy => "@ working copy".to_owned(),
        RevisionSelection::Commit(hex) => sidebar::truncate_end(&hex, sidebar::COMMIT_ID_CHARS),
    };

    let mut bar = row![
        icons::icon(icons::CODE, BAR_TEXT_SIZE, theme.accent),
        text(revision_label)
            .size(BAR_TEXT_SIZE)
            .font(mono)
            .color(theme.muted_text),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    if let Some(path) = &source.selected {
        bar = bar.push(
            text(path.clone())
                .size(BAR_TEXT_SIZE)
                .font(mono)
                .color(theme.text),
        );
    }

    bar = bar.push(Space::new().width(Length::Fill));

    // Right side: the selected entry's status (when notable) + size counts.
    let status = source.selected.as_deref().and_then(|path| {
        let (entries, _) = ui.source_tree_cache.borrow_mut().entries_and_rows(source);
        entries
            .iter()
            .find(|entry| !entry.is_dir && entry.path == path)
            .map(|entry| entry.status)
    });
    match status {
        Some(SourceEntryStatus::Ignored) => {
            bar = bar.push(status_chip_text("ignored", theme.subtle_text, ui));
        }
        Some(SourceEntryStatus::Untracked) => {
            bar = bar.push(status_chip_text("untracked", theme.added_text, ui));
        }
        _ => {}
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
        bar = bar.push(
            text(detail)
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        );
    } else if source.loading.is_some() {
        bar = bar.push(
            text("Loading…")
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        );
    }

    container(bar)
        .width(Length::Fill)
        .padding([9, 16])
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background)),
            ..container::Style::default()
        })
        .into()
}

fn status_chip_text<'a>(label: &'a str, color: Color, ui: &Diffui) -> Element<'a, Message> {
    container(
        text(label)
            .size(text_size::CAPTION)
            .font(ui.config.mono_font)
            .color(color),
    )
    .padding(Padding::from([1, 6]))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(color))),
        border: iced::Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: crate::chip::RADIUS.into(),
        },
        ..container::Style::default()
    })
    .into()
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
