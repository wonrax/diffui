use std::collections::HashMap;

use iced::{
    Background, Border, Color, Element, Font, Length, alignment,
    font::Weight,
    widget::{button, column, container, row, text},
};

use crate::backend::{CommitSummary, DiffFile, DiffFileStatus, RevisionSelection};
use crate::config::AppConfig;
use crate::graph::LaneFrame;
use crate::graph_view::{self, RevisionGraphStyle};
use crate::revision_list::{
    self, FileRowView, IndicatorChip, Item as RevisionListItem, RevisionList, RevisionListStyle,
    RevisionRowView, RowSelectionKey,
};
use crate::theme::{
    self, ThemePreference, ThemeSpec, chip_background, sidebar_header_style, sidebar_panel_style,
    theme_switcher_button_style,
};
use crate::{Diffui, LoadStatus, Message};

// Public sidebar layout knobs — used by main.rs to clamp the resize handle.
// `DEFAULT_WIDTH` is a starting point; the actual floor is derived from the
// configured UI font at runtime by `min_width(...)` so it scales when the
// user picks a larger font in the future. There's no upper cap — users can
// drag the sidebar as wide as the window allows.
pub const DEFAULT_WIDTH: f32 = 380.0;
pub const RESIZE_HIT_PADDING: f32 = 2.0;

/// Minimum sidebar width for the current font config. Picked so the top
/// row of a revision can always fit its load-bearing columns at this font:
/// change-id (12 chars) + commit-id (12 chars) + an abbreviated author +
/// the `+N` bookmark-overflow chip + the loudest status chip
/// (`conflict`, 8 chars). Below this width the chip rail starts dropping
/// rails, so the user loses the conflict signal.
pub fn min_width(config: AppConfig) -> f32 {
    let id_metrics = TextMetrics::iced(config.mono_font, CAPTION_TEXT_SIZE);
    let ui_metrics = TextMetrics::iced(config.ui_font, CAPTION_TEXT_SIZE);

    let change_id_w = id_metrics.measure(&"a".repeat(REVISION_ID_CHARS));
    let commit_id_w = id_metrics.measure(&"a".repeat(COMMIT_ID_CHARS));
    let at_w = id_metrics.measure("@");
    let author_w = ui_metrics.measure("Author Name");
    let plus_n_w = chip_width("+9", &ui_metrics);
    let conflict_w = chip_width("conflict", &ui_metrics);

    let at_gap = 4.0;
    let id_gap = 8.0;
    let chip_gap = 6.0;
    // Gutter for a typical 1-lane row: `lane_strip_width(1) = LANE_WIDTH`.
    let gutter =
        revision_list::GUTTER_LEFT_PADDING + graph_view::LANE_WIDTH + revision_list::GUTTER_PADDING;

    let row_content = at_w
        + at_gap
        + change_id_w
        + id_gap
        + commit_id_w
        + id_gap
        + author_w
        + chip_gap
        + plus_n_w
        + chip_gap
        + conflict_w;
    gutter + row_content + REVISION_CONTENT_RIGHT_PAD
}

/// Width budget for a single chip (label + horizontal padding), matching
/// `RevisionList::measure_chip_width`'s `pad_x = 5.0` on each side.
fn chip_width(label: &str, metrics: &TextMetrics) -> f32 {
    metrics.measure(label) + 10.0
}

/// Mirrors `revision_list::CONTENT_PADDING` — the right-edge padding
/// inside a revision row. Kept here so `min_width` can budget the row
/// without exposing the constant through `revision_list`.
const REVISION_CONTENT_RIGHT_PAD: f32 = 12.0;

const CAPTION_TEXT_SIZE: f32 = 13.0;
const COUNT_CHIP_TEXT_SIZE: f32 = 11.0;
const REVISION_ID_CHARS: usize = 12;
const COMMIT_ID_CHARS: usize = 12;

// Floor for the badge column. We measure the actual status labels to size the
// column, but a single-character label like "M" can render thinner than the
// chip looks tasteful at, so we keep a small visual minimum.
const FILE_BADGE_MIN_WIDTH: f32 = 13.0;
const FILE_BADGE_HORIZONTAL_PADDING: f32 = 4.0;
/// Mirrors `revision_list::FILE_BADGE_TEXT_SIZE`. The badge column width is
/// computed in `view()`, so we measure label widths at the same point size
/// the renderer actually paints them at.
const FILE_BADGE_TEXT_SIZE: f32 = 10.0;
// Horizontal padding flanking the `+N` / `-N` numeric columns.
const FILE_STAT_HORIZONTAL_PADDING: f32 = 4.0;
const FILE_STAT_MIN_WIDTH: f32 = 24.0;

pub fn build_sidebar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let title_row = row![
        text("Changes")
            .size(15)
            .font(Font {
                weight: Weight::Normal,
                ..ui.config.ui_font
            })
            .color(theme.subtle_text),
        build_count_chip(ui.commits.len(), theme, ui.config.ui_font),
        container(text("")).width(Length::Fill),
        build_theme_switcher(ui.selected_theme, theme, ui.config.ui_font),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    let mut header_content = column![title_row].spacing(7);

    if let LoadStatus::Failed(error) = &ui.status {
        header_content = header_content.push(
            text(format!("Failed: {error}"))
                .size(CAPTION_TEXT_SIZE)
                .color(theme.removed_text),
        );
    }

    let sidebar_header = container(header_content)
        .padding([12, 12])
        .style(move |_| sidebar_header_style(theme));

    let revision_list = build_revision_list(ui, theme);

    let body = column![sidebar_header, revision_list].spacing(0);

    container(body)
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| sidebar_panel_style(theme))
        .into()
}

fn build_revision_list<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let mut items: Vec<RevisionListItem> = Vec::with_capacity(ui.commits.len());
    let metrics = sidebar_text_metrics(ui.config);
    let graph_style = RevisionGraphStyle {
        lane_base_color: theme.lane_base,
        missing_color: theme.subtle_text,
    };

    let (file_widgets, file_badge_width): (Option<Vec<FileRowTemplate>>, f32) =
        if matches!(ui.status, LoadStatus::Loaded) && !ui.document.files.is_empty() {
            let widest_addition = ui
                .document
                .files
                .iter()
                .map(|file| format!("+{}", file.additions))
                .max_by(|a, b| {
                    metrics
                        .measure(a)
                        .partial_cmp(&metrics.measure(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or_else(|| "+0".to_owned());
            let widest_deletion = ui
                .document
                .files
                .iter()
                .map(|file| format!("-{}", file.deletions))
                .max_by(|a, b| {
                    metrics
                        .measure(a)
                        .partial_cmp(&metrics.measure(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or_else(|| "-0".to_owned());
            let additions_w = file_stat_width(&widest_addition, &metrics);
            let deletions_w = file_stat_width(&widest_deletion, &metrics);
            let badge_metrics = badge_text_metrics(ui.config);
            let badge_w = file_badge_width(&ui.document.files, &badge_metrics);

            // Mirror `draw_file`'s layout exactly so truncation kicks in at
            // the same threshold the renderer clips at:
            //   [gutter] [badge] gap [path] gap [+N] gap [-N] right_pad
            // Previously this used 4 gaps and the full horizontal padding,
            // and ignored the graph gutter entirely — so paths bled past
            // the +N/-N columns whenever the expanded commit had any lanes.
            let expanded_lane_count = ui
                .commits
                .iter()
                .find(|commit| {
                    is_expanded_commit(&ui.selected_revision, ui.file_list_expanded, commit)
                })
                .map(|commit| commit.lane_frame.after.len())
                .unwrap_or(0);
            let gutter_total = revision_list::GUTTER_LEFT_PADDING
                + expanded_lane_count as f32 * graph_view::LANE_WIDTH
                + revision_list::GUTTER_PADDING;
            let reserved = gutter_total
                + badge_w
                + additions_w
                + deletions_w
                + revision_list::FILE_ROW_GAP * 3.0
                + revision_list::FILE_ROW_RIGHT_PAD;
            let display_width = (ui.sidebar_width - reserved).max(0.0);
            let display_models = file_display_models(&ui.document.files, display_width, &metrics);
            (
                Some(
                    ui.document
                        .files
                        .iter()
                        .enumerate()
                        .map(|(idx, file)| FileRowTemplate {
                            primary: display_models[idx].primary.clone(),
                            secondary: display_models[idx].secondary.clone(),
                            raw_path: display_models[idx].raw_path.clone(),
                            status_label: file.status.short_label().to_owned(),
                            status_color: file_status_color(file.status, theme),
                            additions: file.additions,
                            deletions: file.deletions,
                            file_index: idx,
                            additions_width: additions_w,
                            deletions_width: deletions_w,
                        })
                        .collect(),
                ),
                badge_w,
            )
        } else {
            (None, FILE_BADGE_MIN_WIDTH)
        };

    // Per-lane bookmark labels propagating top-down through the visible
    // graph. When a commit carries bookmarks, they latch onto its
    // node lane and stay until that lane terminates (drops out of
    // `after`). The pre-trim snapshot is shown on the commit row
    // itself; the post-trim snapshot is shown on any file rows below
    // it, so terminated lanes don't keep a stale tooltip.
    let mut current_labels: Vec<Vec<String>> = Vec::new();
    // Per-lane segment id: a fresh id is allocated whenever a lane goes
    // from dead → alive, and cleared when the lane drops out of `after`.
    // Lets the revision list emphasize a whole lane segment when one of
    // its rows is hovered, instead of only the strokes inside that row.
    let mut current_segments: Vec<Option<usize>> = Vec::new();
    let mut next_segment_id: usize = 0;

    for commit in &ui.commits {
        let unique_len = shortest_unique_prefix_len(&commit.change_id, &ui.commits);
        let label_len = revision_id_display_len(unique_len, &commit.change_id);
        let id_prefix: String = commit.change_id.chars().take(unique_len).collect();
        let id_suffix: String = commit
            .change_id
            .chars()
            .skip(unique_len)
            .take(label_len.saturating_sub(unique_len))
            .collect();
        let commit_id_short = truncate_end(&commit.commit_id, COMMIT_ID_CHARS);

        // Working-copy `@` is drawn inline before the change-id by the
        // revision-list widget itself — the design system puts the
        // marker *at* the change, not on the right-edge chip rail.
        //
        // Two chip groups: `bookmark_chips` can overflow into a `+N`
        // chip when there's not enough room, and `status_chips` always
        // shows.
        let lane_color = graph_style.lane_color(commit.lane_frame.node_lane);
        let mut bookmark_chips = Vec::with_capacity(commit.bookmarks.len());
        for bookmark in &commit.bookmarks {
            // Remote/untracked bookmarks contain `@` (e.g. `main@origin`).
            // They render outlined (transparent fill + 1px lane-color
            // border) so they read as "tracking" rather than "live"
            // bookmarks, which use the filled-tint pattern.
            let is_remote = bookmark.contains('@');
            bookmark_chips.push(IndicatorChip {
                label: bookmark.clone(),
                background: if is_remote {
                    Color::TRANSPARENT
                } else {
                    chip_background(lane_color)
                },
                text_color: lane_color,
                border_color: if is_remote { Some(lane_color) } else { None },
                border_dashed: false,
            });
        }

        let mut status_chips = Vec::new();
        if commit.is_empty == Some(true) {
            // Outlined-transparent chip, mirroring the design's
            // `.tag-empty` (dashed border, ink-400 text, transparent
            // fill).
            status_chips.push(IndicatorChip {
                label: "empty".to_owned(),
                background: Color::TRANSPARENT,
                text_color: theme.subtle_text,
                border_color: Some(theme.subtle_text),
                border_dashed: true,
            });
        }
        if commit.has_conflict {
            // Filled-tint chip (mirrors the "soft" tag pattern used by
            // file-status badges) using the design system's conflict red
            // so it screams across the sidebar at a glance.
            status_chips.push(IndicatorChip {
                label: "conflict".to_owned(),
                background: chip_background(theme.conflict_marker),
                text_color: theme.conflict_marker,
                border_color: None,
                border_dashed: false,
            });
        }

        let selection_key = if commit.is_working_copy {
            RowSelectionKey::WorkingCopy
        } else {
            RowSelectionKey::Commit(commit.revision_id.clone())
        };

        let is_expanded = is_expanded_commit(&ui.selected_revision, ui.file_list_expanded, commit);

        // Latch this commit's bookmarks onto its lane, allocate segment
        // ids for any lane that just came alive, then snapshot pre-trim
        // for the revision row.
        let lane_count = commit.lane_frame.lane_count();
        if current_labels.len() < lane_count {
            current_labels.resize(lane_count, Vec::new());
        }
        if current_segments.len() < lane_count {
            current_segments.resize(lane_count, None);
        }
        if !commit.bookmarks.is_empty() {
            current_labels[commit.lane_frame.node_lane] = commit.bookmarks.clone();
        }
        // Capture the incoming-half segment ids BEFORE allocation, so a
        // merged-in branch on a soon-to-be-reused lane keeps its old
        // id in `before` while the outgoing half gets a fresh id in
        // `after`. Hover emphasis then picks one half cleanly.
        let revision_lane_segments_before = current_segments.clone();
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &commit.lane_frame);
        // Snapshot labels BEFORE clearing split-lane labels — the merge
        // row itself still needs the merged-in branch's tooltip to fire
        // (hover detection gates on labels being non-empty), even
        // though those labels shouldn't propagate to the new branch's
        // strokes below.
        let revision_lane_labels = current_labels.clone();
        let revision_lane_segments_after = current_segments.clone();
        clear_split_lane_labels(&mut current_labels, &commit.lane_frame);
        trim_lane_state(&mut current_segments, &mut current_labels, &commit.lane_frame);
        let continuation_lane_labels = current_labels.clone();
        let continuation_lane_segments = current_segments.clone();

        let is_selected = match &ui.selected_revision {
            RevisionSelection::WorkingCopy => commit.is_working_copy,
            RevisionSelection::Commit(id) => !commit.is_working_copy && id == &commit.revision_id,
        };

        items.push(RevisionListItem::Revision(RevisionRowView {
            selection_key,
            change_id_prefix: id_prefix,
            change_id_suffix: id_suffix,
            commit_id_short,
            author: commit.author.clone(),
            description: commit.description.clone(),
            description_color: commit_description_color(commit, theme),
            bookmark_chips,
            status_chips,
            lane_color,
            frame: commit.lane_frame.clone(),
            lane_labels: revision_lane_labels,
            lane_segments_before: revision_lane_segments_before,
            lane_segments_after: revision_lane_segments_after,
            // Show the collapse/expand chevron only on the selected row, so
            // non-selected rows don't suggest they have a hidden file list.
            collapse_chevron: is_selected.then_some(is_expanded),
        }));

        if is_expanded && let Some(files) = &file_widgets {
            let continuation = commit.lane_frame.after.clone();
            for file in files {
                items.push(RevisionListItem::File(FileRowView {
                    primary: file.primary.clone(),
                    secondary: file.secondary.clone(),
                    raw_path: file.raw_path.clone(),
                    status_label: file.status_label.clone(),
                    // Design system badge style: a soft tint of the
                    // status color as the chip background, with the
                    // saturated color used for the glyph. Replaces the
                    // earlier "filled chip + bg-color text" pattern.
                    status_background: chip_background(file.status_color),
                    status_text: file.status_color,
                    additions: file.additions,
                    deletions: file.deletions,
                    additions_text: theme.added_text,
                    deletions_text: theme.removed_text,
                    continuation: continuation.clone(),
                    additions_width: file.additions_width,
                    deletions_width: file.deletions_width,
                    primary_color: theme.text,
                    secondary_color: theme.muted_text,
                    file_index: file.file_index,
                    lane_labels: continuation_lane_labels.clone(),
                    lane_segments: continuation_lane_segments.clone(),
                }));
            }
        }
    }

    let selected_row = match &ui.selected_revision {
        RevisionSelection::WorkingCopy => Some(RowSelectionKey::WorkingCopy),
        RevisionSelection::Commit(id) => Some(RowSelectionKey::Commit(id.clone())),
    };

    RevisionList::new(
        items,
        selected_row,
        Some(ui.selected_file),
        revision_list_style(theme, ui.config, file_badge_width),
        Message::SelectRowKey,
        Message::SelectFile,
    )
    .width(Length::Fill)
    .into()
}

/// Advance per-lane segment ids for one revision row, allocating fresh
/// ids for newly-alive lanes and for lanes whose index is reused by a
/// brand-new outgoing branch at this merge.
///
/// The second case (split lanes) is what stops the hover emphasis from
/// leaking across two unrelated branches that share a lane index: at a
/// merge commit `allocate_lane` will pick the leftmost-empty slot for any
/// extra parent, which is often the slot just freed by the merged-in
/// branch. Without resetting the id here, both the merged-in branch above
/// and the brand-new branch below would share one segment id.
///
/// Labels are not touched here — callers separately clear split-lane
/// labels *after* snapshotting the revision row, so the merged-in
/// branch's tooltip still fires when the cursor is over the merge row
/// itself (hover only triggers on lanes that have labels).
fn advance_lane_segments(
    current_segments: &mut Vec<Option<usize>>,
    next_segment_id: &mut usize,
    lane_frame: &LaneFrame,
) {
    let lane_count = lane_frame.lane_count();
    if current_segments.len() < lane_count {
        current_segments.resize(lane_count, None);
    }
    for (lane, slot) in current_segments.iter_mut().enumerate().take(lane_count) {
        let before = lane_frame.before.get(lane).copied().flatten();
        let after = lane_frame.after.get(lane).copied().flatten();
        let alive = before.is_some() || after.is_some() || lane == lane_frame.node_lane;
        if alive && slot.is_none() {
            *slot = Some(*next_segment_id);
            *next_segment_id += 1;
        }
    }
    // Split-lane case: a non-node lane in `merging_lanes` terminates its
    // incoming edge at the disc; if it also has an outgoing edge, that's
    // a new branch reusing the freed slot. Allocate a fresh id for the
    // outgoing half so emphasis at and below the merge follows the new
    // branch, not the merged-in one.
    for &lane in &lane_frame.merging_lanes {
        if lane == lane_frame.node_lane {
            continue;
        }
        if lane_frame.after.get(lane).copied().flatten().is_none() {
            continue;
        }
        if let Some(slot) = current_segments.get_mut(lane) {
            *slot = Some(*next_segment_id);
            *next_segment_id += 1;
        }
    }
}

/// Clear the merged-in branch's labels on split lanes, so its name
/// doesn't carry onto the new branch's strokes below the merge. Called
/// *after* the revision row's label snapshot so the merge row itself
/// still shows the merged-in branch's tooltip when the user hovers it.
fn clear_split_lane_labels(current_labels: &mut [Vec<String>], lane_frame: &LaneFrame) {
    for &lane in &lane_frame.merging_lanes {
        if lane == lane_frame.node_lane {
            continue;
        }
        if lane_frame.after.get(lane).copied().flatten().is_none() {
            continue;
        }
        if let Some(labels) = current_labels.get_mut(lane) {
            labels.clear();
        }
    }
}

/// Drop labels and segment ids for lanes that don't survive into the
/// row's `after` snapshot. The trimmed state is what file rows below
/// inherit so a terminated lane doesn't keep a stale tooltip or
/// emphasis-id.
fn trim_lane_state(
    current_segments: &mut [Option<usize>],
    current_labels: &mut [Vec<String>],
    lane_frame: &LaneFrame,
) {
    for (lane, slot) in current_segments.iter_mut().enumerate() {
        let alive = lane_frame.after.get(lane).copied().flatten().is_some();
        if !alive {
            *slot = None;
        }
    }
    for (lane, labels) in current_labels.iter_mut().enumerate() {
        let alive = lane_frame.after.get(lane).copied().flatten().is_some();
        if !alive {
            labels.clear();
        }
    }
}

struct FileRowTemplate {
    primary: String,
    secondary: String,
    raw_path: String,
    status_label: String,
    /// Saturated color for the file's status (e.g., green for Added).
    /// The chip background is derived from this via `chip_background`
    /// at draw-row construction time so the chip reads as a tint of
    /// the same hue as its glyph — matching the design system's
    /// "soft tint + colored text" badge pattern.
    status_color: Color,
    additions: usize,
    deletions: usize,
    file_index: usize,
    additions_width: f32,
    deletions_width: f32,
}

fn revision_list_style(
    theme: ThemeSpec,
    config: AppConfig,
    file_badge_width: f32,
) -> RevisionListStyle {
    RevisionListStyle {
        graph: RevisionGraphStyle {
            // Lane 0 (the trunk) wears `theme.lane_base` — violet by
            // design — and subsequent lanes derive from it via HSL
            // rotation in `RevisionGraphStyle::lane_color`. Kept
            // separate from `theme.accent` so the coral accent stays
            // reserved for working-copy / selection signalling and
            // doesn't compete with the diff add/del greens and reds.
            lane_base_color: theme.lane_base,
            missing_color: theme.subtle_text,
        },
        background: theme.panel_background,
        selected_background: theme.selected_file,
        accent: theme.accent,
        border: theme.border,
        muted_text: theme.muted_text,
        subtle_text: theme.subtle_text,
        accent_text: theme.accent,
        primary_font: config.ui_font,
        mono_font: config.mono_font,
        file_badge_width,
        tooltip_background: theme.panel_background_elevated,
        tooltip_text: theme.text,
        tooltip_border: theme.border,
        scrollbar: theme::scrollbar_style(theme),
    }
}

/// Small rounded badge showing the total number of revisions in view.
/// Matches the design's `.pane-hd .ct` token: bg-elev background, line
/// border, ink-400 mono numeral. Sits next to the `CHANGES` label.
fn build_count_chip(count: usize, theme: ThemeSpec, font: Font) -> Element<'static, Message> {
    container(
        text(count.to_string())
            .size(COUNT_CHIP_TEXT_SIZE)
            .font(font)
            .color(theme.subtle_text),
    )
    .padding([1, 7])
    .style(move |_| container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        text_color: Some(theme.subtle_text),
        border: Border {
            color: theme.border,
            width: 1.0,
            radius: 999.0.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn build_theme_switcher(
    selected_theme: ThemePreference,
    theme: ThemeSpec,
    font: Font,
) -> Element<'static, Message> {
    let mut controls = row![].spacing(3);

    for candidate in ThemePreference::ALL {
        let selected = candidate == selected_theme;
        controls = controls.push(
            button(text(candidate.label()).size(CAPTION_TEXT_SIZE).font(font))
                .padding([5, 7])
                .style(move |_, status| theme_switcher_button_style(status, selected, theme))
                .on_press(Message::SelectTheme(candidate)),
        );
    }

    controls.into()
}

/// Saturated color associated with a file's diff status, used to tint
/// the status chip in the revision list. Mapping follows the design
/// system: A→green (added_text), M→blue (info), D→red (removed_text),
/// R→amber (modified_token). The chip's background is derived from
/// this color via `chip_background` so the glyph and the tint share
/// a hue.
fn file_status_color(status: DiffFileStatus, theme: ThemeSpec) -> Color {
    match status {
        DiffFileStatus::Added => theme.added_text,
        DiffFileStatus::Deleted => theme.removed_text,
        DiffFileStatus::Modified => theme.info,
        DiffFileStatus::Renamed => theme.modified_token,
    }
}

/// True when `commit` is the selected revision and the user hasn't
/// collapsed the inline file list. The collapse/expand preference is
/// global rather than per-revision (see `Diffui::file_list_expanded`),
/// so flipping it once carries across whatever revision the user picks
/// next.
fn is_expanded_commit(
    selected: &RevisionSelection,
    expanded: bool,
    commit: &CommitSummary,
) -> bool {
    if !expanded {
        return false;
    }
    match selected {
        RevisionSelection::WorkingCopy => commit.is_working_copy,
        RevisionSelection::Commit(id) => !commit.is_working_copy && id == &commit.revision_id,
    }
}

fn revision_id_display_len(unique_len: usize, revision_id: &str) -> usize {
    REVISION_ID_CHARS
        .max(unique_len)
        .min(revision_id.chars().count())
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn commit_description_color(commit: &CommitSummary, theme: ThemeSpec) -> Color {
    if commit.has_description {
        return theme.text;
    }

    match commit.is_empty {
        Some(true) => theme.added_text,
        Some(false) => theme.note_text,
        None => theme.note_text,
    }
}

fn shortest_unique_prefix_len(change_id: &str, commits: &[CommitSummary]) -> usize {
    if let Some(prefix_len) = commits
        .iter()
        .find(|commit| commit.change_id == change_id)
        .and_then(|commit| commit.shortest_change_id_len)
    {
        return prefix_len.min(change_id.chars().count());
    }

    let total_len = change_id.chars().count();

    (1..=total_len)
        .find(|prefix_len| {
            let prefix = change_id.chars().take(*prefix_len).collect::<String>();
            commits
                .iter()
                .filter(|commit| commit.change_id.starts_with(&prefix))
                .count()
                == 1
        })
        .unwrap_or(total_len)
}

/// Pixel-accurate text width measurement for layout decisions made outside
/// the renderer (path truncation, badge column sizing, etc.).
///
/// We previously approximated text width with `chars * 7px` heuristics, which
/// silently misbehaved for any glyph wider or narrower than the assumed
/// average — `@` clipped into `…` in revision IDs, abbreviated paths over- or
/// under-shot the available room, and badges would clip if the user ever
/// switched to a larger font. Going through real `cosmic_text` shaping fixes
/// the entire class of bug because we use the same engine the wgpu renderer
/// uses, so the measurements match what gets drawn.
///
/// Why headless `iced::advanced::graphics::text::Paragraph` rather than the
/// renderer's `R::Paragraph`: path-truncation runs in `view()` (in
/// `build_revision_list`), well before any `draw()` call, and we need exact
/// widths to decide *which* string to hand to the widget. The renderer's
/// `Paragraph` type isn't reachable from here without threading renderer
/// generics through `main.rs`. The wgpu renderer is built on top of
/// `iced_graphics`, so the headless `Paragraph` shapes text identically.
///
/// Tests use `Self::fixed_per_char` to stay deterministic regardless of which
/// system fonts happen to be installed on the host.
#[derive(Clone)]
pub enum TextMetrics {
    Iced {
        font: Font,
        size: f32,
    },
    #[cfg(test)]
    FixedPerChar {
        width: f32,
    },
}

impl TextMetrics {
    fn iced(font: Font, size: f32) -> Self {
        Self::Iced { font, size }
    }

    #[cfg(test)]
    fn fixed_per_char(width: f32) -> Self {
        Self::FixedPerChar { width }
    }

    fn measure(&self, content: &str) -> f32 {
        if content.is_empty() {
            return 0.0;
        }
        match self {
            Self::Iced { font, size } => {
                use iced::advanced::graphics::text::Paragraph;
                use iced::advanced::text::{LineHeight, Paragraph as _, Shaping, Text, Wrapping};
                use iced::{Pixels, Size};

                let line_height = (*size * 1.4).max(1.0);
                let paragraph = Paragraph::with_text(Text {
                    content,
                    bounds: Size::new(f32::INFINITY, line_height),
                    size: Pixels(*size),
                    line_height: LineHeight::Absolute(Pixels(line_height)),
                    font: *font,
                    align_x: iced::advanced::text::Alignment::Left,
                    align_y: alignment::Vertical::Top,
                    shaping: Shaping::Advanced,
                    wrapping: Wrapping::None,
                    ellipsis: iced::advanced::text::Ellipsis::None,
                    hint_factor: None,
                });
                paragraph.min_width()
            }
            #[cfg(test)]
            Self::FixedPerChar { width } => content.chars().count() as f32 * width,
        }
    }
}

fn sidebar_text_metrics(config: AppConfig) -> TextMetrics {
    TextMetrics::iced(config.ui_font, CAPTION_TEXT_SIZE)
}

fn badge_text_metrics(config: AppConfig) -> TextMetrics {
    TextMetrics::iced(config.ui_font, FILE_BADGE_TEXT_SIZE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarFileDisplay {
    primary: String,
    secondary: String,
    raw_path: String,
}

fn file_display_models(
    files: &[DiffFile],
    available_width: f32,
    metrics: &TextMetrics,
) -> Vec<SidebarFileDisplay> {
    let mut basename_counts = HashMap::<&str, usize>::new();
    let split_paths = files
        .iter()
        .map(|file| split_display_path(&file.path))
        .inspect(|(_, basename)| {
            *basename_counts.entry(*basename).or_default() += 1;
        })
        .collect::<Vec<_>>();

    files
        .iter()
        .zip(split_paths.iter())
        .map(|(file, (directories, basename))| {
            let (primary, secondary) = if basename_counts.get(basename).copied() == Some(1) {
                (
                    (*basename).to_owned(),
                    secondary_display_path(directories, available_width, metrics),
                )
            } else {
                let group = split_paths
                    .iter()
                    .filter(|(_, other_basename)| other_basename == basename)
                    .map(|(other_directories, _)| other_directories.as_slice())
                    .collect::<Vec<_>>();
                let suffix_len = collision_directory_suffix_len(directories, &group);
                let split_at = directories.len().saturating_sub(suffix_len);
                let primary_segments = directories[split_at..]
                    .iter()
                    .copied()
                    .chain(std::iter::once(*basename))
                    .collect::<Vec<_>>();

                (
                    primary_segments.join("/"),
                    secondary_display_path(
                        &common_directory_prefix(&group),
                        available_width,
                        metrics,
                    ),
                )
            };

            SidebarFileDisplay {
                primary: truncate_primary_display(&primary, basename, available_width, metrics),
                secondary,
                raw_path: file.path.clone(),
            }
        })
        .collect()
}

fn split_display_path(path: &str) -> (Vec<&str>, &str) {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    match segments.pop() {
        Some(basename) => (segments, basename),
        None => (Vec::new(), path),
    }
}

fn collision_directory_suffix_len(directories: &[&str], group: &[&[&str]]) -> usize {
    let max_depth = group
        .iter()
        .map(|other_directories| other_directories.len())
        .max()
        .unwrap_or(0);

    for depth in 1..=max_depth {
        let mut segments = group.iter().map(|other_directories| {
            other_directories
                .len()
                .checked_sub(depth)
                .and_then(|index| other_directories.get(index).copied())
        });
        let Some(first) = segments.next() else {
            return 0;
        };

        if segments.any(|segment| segment != first) {
            return directories.len().min(depth);
        }
    }

    directories.len()
}

fn common_directory_prefix<'a>(group: &[&[&'a str]]) -> Vec<&'a str> {
    let Some(first) = group.first() else {
        return Vec::new();
    };

    first
        .iter()
        .enumerate()
        .take_while(|(index, segment)| {
            group
                .iter()
                .all(|directories| directories.get(*index) == Some(segment))
        })
        .map(|(_, segment)| *segment)
        .collect()
}

fn secondary_display_path(
    segments: &[&str],
    available_width: f32,
    metrics: &TextMetrics,
) -> String {
    let path = segments.join("/");
    if metrics.measure(&path) <= available_width {
        path
    } else {
        abbreviate_secondary_path(segments)
    }
}

fn abbreviate_secondary_path(segments: &[&str]) -> String {
    match segments {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, rest @ .., last] => {
            let mut abbreviated = Vec::with_capacity(segments.len());
            abbreviated.push((*first).to_owned());
            abbreviated.extend(
                rest.iter()
                    .filter_map(|segment| segment.chars().next())
                    .map(|character| character.to_string()),
            );
            abbreviated.push((*last).to_owned());
            abbreviated.join("/")
        }
    }
}

fn truncate_primary_display(
    primary: &str,
    basename: &str,
    available_width: f32,
    metrics: &TextMetrics,
) -> String {
    if metrics.measure(primary) <= available_width || primary == basename {
        return primary.to_owned();
    }

    let Some(prefix) = primary.strip_suffix(basename) else {
        return primary.to_owned();
    };
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return primary.to_owned();
    }

    // Always preserve the basename — the user needs to know what file this is.
    // The prefix is what gets squeezed.
    let suffix = format!("/{basename}");
    let suffix_w = metrics.measure(&suffix);
    if suffix_w >= available_width {
        return basename.to_owned();
    }
    let prefix_budget = available_width - suffix_w;

    let truncated_prefix = middle_truncate_to_width(prefix, prefix_budget, metrics);
    if truncated_prefix.is_empty() {
        return basename.to_owned();
    }
    format!("{truncated_prefix}{suffix}")
}

/// Middle-truncate `value` so it fits in `max_width` pixels under `metrics`.
///
/// Reads char-by-char from each side and stops as soon as the rendered width
/// of `head + "…" + tail` exceeds the budget. Linear in chars; fine since
/// these strings are short path segments and we run this once per file.
fn middle_truncate_to_width(value: &str, max_width: f32, metrics: &TextMetrics) -> String {
    if metrics.measure(value) <= max_width {
        return value.to_owned();
    }
    let ellipsis_w = metrics.measure("…");
    if ellipsis_w > max_width {
        return String::new();
    }

    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    let mut head = String::new();
    let mut tail = String::new();
    let mut head_len = 0;
    let mut tail_len = 0;
    // Bias: take from head first when budget allows odd counts.
    let mut take_head = true;

    while head_len + tail_len < chars.len() {
        let next_char = if take_head {
            chars[head_len]
        } else {
            chars[chars.len() - 1 - tail_len]
        };

        let mut candidate_head = head.clone();
        let mut candidate_tail = tail.clone();
        if take_head {
            candidate_head.push(next_char);
        } else {
            candidate_tail.insert(0, next_char);
        }

        let candidate = format!("{candidate_head}…{candidate_tail}");
        if metrics.measure(&candidate) > max_width {
            break;
        }

        head = candidate_head;
        tail = candidate_tail;
        if take_head {
            head_len += 1;
        } else {
            tail_len += 1;
        }
        take_head = !take_head;
    }

    if head_len == 0 && tail_len == 0 {
        // We can fit the ellipsis but not even one neighbouring character.
        return "…".to_owned();
    }
    format!("{head}…{tail}")
}

fn file_stat_width(text: &str, metrics: &TextMetrics) -> f32 {
    (metrics.measure(text) + FILE_STAT_HORIZONTAL_PADDING * 2.0).max(FILE_STAT_MIN_WIDTH)
}

/// Width of the status badge column ("M", "A", "D", "R", …). We measure the
/// widest label in the document and add padding so two-letter labels like
/// "MM" still fit comfortably.
fn file_badge_width(files: &[DiffFile], metrics: &TextMetrics) -> f32 {
    let widest = files
        .iter()
        .map(|file| metrics.measure(file.status.short_label()))
        .fold(0.0_f32, f32::max);
    (widest + FILE_BADGE_HORIZONTAL_PADDING * 2.0).max(FILE_BADGE_MIN_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DiffFileStatus;
    use crate::graph::LaneFrame;
    use jj_lib::graph::GraphEdgeType;

    fn diff_file(path: &str) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }

    fn commit_summary(change_id: &str) -> CommitSummary {
        CommitSummary {
            change_id: change_id.to_owned(),
            commit_id: change_id.to_owned(),
            revision_id: change_id.to_owned(),
            shortest_change_id_len: None,
            description: String::new(),
            author: String::new(),
            has_description: false,
            is_empty: None,
            has_conflict: false,
            lane_frame: LaneFrame {
                before: Vec::new(),
                after: Vec::new(),
                node_lane: 0,
                merging_lanes: Vec::new(),
                missing_parents: 0,
            },
            is_working_copy: false,
            bookmarks: Vec::new(),
        }
    }

    /// Deterministic metrics for tests: each character is 7px wide, matching
    /// the old `SIDEBAR_FILE_TEXT_CHAR_WIDTH` heuristic so the existing
    /// fixture widths still trigger truncation at the same boundaries. We
    /// don't go through real cosmic_text in tests because system font
    /// availability differs across hosts and would make the assertions
    /// flaky in CI.
    fn test_metrics() -> TextMetrics {
        TextMetrics::fixed_per_char(7.0)
    }

    #[test]
    fn revision_id_prefix_uses_shortest_unique_change_id() {
        let commits = vec![
            commit_summary("abc"),
            commit_summary("abd"),
            commit_summary("z"),
        ];

        assert_eq!(shortest_unique_prefix_len("abc", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("abd", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("z", &commits), 1);
    }

    #[test]
    fn revision_id_display_len_keeps_shortest_unique_prefix() {
        let long_id = "abcdefghijklmnopqrstuvwxyz";

        assert_eq!(revision_id_display_len(3, long_id), REVISION_ID_CHARS);
        assert_eq!(revision_id_display_len(16, long_id), 16);
        assert_eq!(revision_id_display_len(99, long_id), long_id.len());
    }

    #[test]
    fn sidebar_display_keeps_unique_basename_primary_and_full_secondary_when_it_fits() {
        let files = vec![diff_file("packages/frontend/src/components/Button.rs")];

        let display = file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(
            display,
            [SidebarFileDisplay {
                primary: "Button.rs".to_owned(),
                secondary: "packages/frontend/src/components".to_owned(),
                raw_path: "packages/frontend/src/components/Button.rs".to_owned(),
            }]
        );
    }

    #[test]
    fn sidebar_display_abbreviates_secondary_only_when_width_is_tight() {
        let files = vec![diff_file("packages/frontend/src/components/Button.rs")];

        // Width tight enough that "packages/frontend/src/components" (32 chars
        // at 7px under the test metrics → 224px) doesn't fit, forcing the
        // abbreviation path.
        let display = file_display_models(&files, 7.0 * 16.0, &test_metrics());

        assert_eq!(display[0].primary, "Button.rs");
        assert_eq!(display[0].secondary, "packages/f/s/components");
    }

    #[test]
    fn sidebar_display_uses_shortest_unique_suffix_for_colliding_basenames() {
        let files = vec![
            diff_file("crates/ui/src/main.rs"),
            diff_file("crates/cli/src/main.rs"),
            diff_file("crates/worker/src/lib.rs"),
        ];

        let display = file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "ui/src/main.rs");
        assert_eq!(display[0].secondary, "crates");
        assert_eq!(display[1].primary, "cli/src/main.rs");
        assert_eq!(display[1].secondary, "crates");
        assert_eq!(display[2].primary, "lib.rs");
        assert_eq!(display[2].secondary, "crates/worker/src");
    }

    #[test]
    fn sidebar_display_collision_secondary_is_common_root_only() {
        let files = vec![
            diff_file("workspace/package-a/src/Button.rs"),
            diff_file("workspace/package-b/test/Button.rs"),
        ];

        let display = file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "src/Button.rs");
        assert_eq!(display[1].primary, "test/Button.rs");
        assert_eq!(display[0].secondary, "workspace");
        assert_eq!(display[1].secondary, "workspace");
    }

    #[test]
    fn sidebar_display_handles_collision_at_repository_root() {
        let files = vec![diff_file("src/main.rs"), diff_file("tests/main.rs")];

        let display = file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "src/main.rs");
        assert_eq!(display[0].secondary, "");
        assert_eq!(display[1].primary, "tests/main.rs");
        assert_eq!(display[1].secondary, "");
    }

    #[test]
    fn sidebar_display_root_file_has_empty_secondary() {
        let files = vec![diff_file("Cargo.lock")];

        let display = file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "Cargo.lock");
        assert_eq!(display[0].secondary, "");
    }

    #[test]
    fn sidebar_display_preserves_root_and_leaf_secondary_segments() {
        assert_eq!(
            abbreviate_secondary_path(&["workspace", "packages", "frontend", "src"]),
            "workspace/p/f/src"
        );
        assert_eq!(abbreviate_secondary_path(&["src"]), "src");
        assert_eq!(abbreviate_secondary_path(&[]), "");
    }

    #[test]
    fn sidebar_display_middle_truncates_only_prepended_primary_directories() {
        // Budget = 24 chars × 7px under test metrics = 168px.
        let primary = truncate_primary_display(
            "very/long/generated/module/path/component/Button.rs",
            "Button.rs",
            7.0 * 24.0,
            &test_metrics(),
        );

        assert_eq!(primary, "very/lo…ponent/Button.rs");
        assert_eq!(primary.chars().count(), 24);
        assert!(primary.ends_with("/Button.rs"));
    }

    #[test]
    fn sidebar_display_protects_basename_when_width_is_tiny() {
        // Even 6 chars of budget is too tight for "/Button.rs" (10 chars), so
        // the truncator should bail out and just hand back the basename.
        assert_eq!(
            truncate_primary_display(
                "deeply/nested/source/Button.rs",
                "Button.rs",
                7.0 * 6.0,
                &test_metrics(),
            ),
            "Button.rs"
        );
    }

    #[test]
    fn lane_reuse_at_merge_starts_a_new_segment() {
        // Two children of a merge commit M share the lane, then M brings
        // in a feature on a reused index. The merged-in branch (above M)
        // and the brand-new feature branch (below M) must end up with
        // different segment ids — otherwise hovering one would emphasize
        // both.
        //
        // Layout (top is newest):
        //
        //   *  A0 (lane 0, child of M)
        //   | * B0 (lane 1, child of M)
        //   |/
        //   *  M  (lane 0, parents [P (lane 0), F (lane 1)])
        //   ...
        let mut current_segments: Vec<Option<usize>> = Vec::new();
        let mut current_labels: Vec<Vec<String>> = Vec::new();
        let mut next_segment_id: usize = 0;

        let direct = Some(GraphEdgeType::Direct);

        // Row A0: lane 0 node, awaits M downward.
        let frame_a0 = LaneFrame {
            before: vec![],
            after: vec![direct],
            node_lane: 0,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_a0);
        let a0_seg = current_segments[0];
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_a0);

        // Row B0: lane 1 node, awaits M downward; lane 0 still pending M.
        let frame_b0 = LaneFrame {
            before: vec![direct, None],
            after: vec![direct, direct],
            node_lane: 1,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        if current_labels.len() < frame_b0.lane_count() {
            current_labels.resize(frame_b0.lane_count(), Vec::new());
        }
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_b0);
        let b0_seg = current_segments[1];
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_b0);

        // Row M: lanes 0 and 1 collapse into lane 0 (node), and lane 1
        // is reused for the feature parent (F) opening a brand-new
        // outgoing edge.
        let frame_m = LaneFrame {
            before: vec![direct, direct],
            after: vec![direct, direct],
            node_lane: 0,
            merging_lanes: vec![0, 1],
            missing_parents: 0,
        };
        // Snapshot before/after halves exactly the way `build_revision_list`
        // does — drawing emphasis hinges on the split.
        let m_before = current_segments.clone();
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_m);
        let m_after = current_segments.clone();
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_m);

        // The before-half on the reused lane carries the merged-in
        // branch (B0)'s segment, while the after-half is a fresh id —
        // so hovering either branch only emphasizes its own half at M.
        assert_eq!(m_before[1], b0_seg);
        assert_ne!(m_after[1], b0_seg);
        // Lane 0 continues across the merge — same id from A0 onward
        // in both halves.
        assert_eq!(m_before[0], a0_seg);
        assert_eq!(m_after[0], a0_seg);
    }

    #[test]
    fn pass_through_lane_keeps_its_segment_id() {
        // Sanity check: an unrelated lane that just passes through a
        // commit shouldn't get a new segment id.
        let mut current_segments: Vec<Option<usize>> = Vec::new();
        let mut current_labels: Vec<Vec<String>> = Vec::new();
        let mut next_segment_id: usize = 0;

        let direct = Some(GraphEdgeType::Direct);

        let frame_a = LaneFrame {
            before: vec![direct, direct],
            after: vec![direct, direct],
            node_lane: 0,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        // Seed lane 1's segment via a previous row.
        current_segments.extend([Some(10), Some(11)]);
        let lane_1_before = current_segments[1];
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_a);
        assert_eq!(current_segments[1], lane_1_before);
    }
}
