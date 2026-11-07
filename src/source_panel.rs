//! The source-browser view: a GitHub-style "browse the repo at this revision"
//! lens over the active tab. The sidebar is the full file tree (the browsed
//! revision itself is named in the toolbar and the pane's info bar); the main
//! pane renders the selected file through the shared
//! [`DiffView`] widget in its plain mode (single line-number column, no
//! file/hunk header strips), so selection, copy, find, and wrap all behave
//! exactly like the diff view.

use crate::Cx;
use gpui_kit::{AnyElement, Window};
use jj_lib::graph::GraphEdgeType;
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::collections::HashSet;
use std::rc::Rc;

use crate::revision_list::{FileRowView, RevisionRowView, RowSelectionKey};
use crate::theme::{ThemeSpec, chip_background, text_size};
use crate::{Diffui, SourceState};
use diffui_core::{
    RevisionSelection, SourceEntry, SourceEntryStatus, SourceTreeRow, source_tree_rows,
};

// Matches the diff panel's code size — both panes render "code".
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

/// Stub: the source_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_source_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("source_panel::build_source_sidebar")
}

/// Stub: the source_panel view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_source_panel(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("source_panel::build_source_panel")
}
