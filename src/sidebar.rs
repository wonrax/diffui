use std::collections::HashMap;

use iced::{
    Background, Border, Color, Element, Font, Length, Padding, alignment, mouse,
    widget::{Space, column, container, mouse_area, row, text, text_input, tooltip},
};

use crate::config::AppConfig;
use crate::graph_layout::GraphLayout;
use crate::graph_view::{self, RevisionGraphStyle};
use crate::repository::Vcs;
use crate::revision_list::{
    self, FileRowView, IndicatorChip, RevisionList, RevisionListStyle, RevisionRowView,
    RowSelectionKey,
};
use crate::theme::{self, ThemeSpec, chip_background, sidebar_panel_style};
use crate::{Diffui, HoverTarget, LoadStatus, Message, ToolbarMenu};
use diffui_core::{CommitStore, DiffFile, DiffFileStatus, RevisionSelection, RowView};
use jj_lib::graph::GraphEdgeType;

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

pub fn build_sidebar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    // The "Changes" header is gone; the list starts flush at the top of the
    // pane. Each row carries its own internal vertical padding, so the first
    // row's content isn't cramped against the top edge — adding a panel-colored
    // gap above it instead just reads as a stray strip above the selected row.
    let revision_list = build_revision_list(ui, theme);

    let mut body = column![].spacing(0);
    // The revset / revision-range filter sits at the very top of the pane.
    body = body.push(build_revset_filter(ui, theme));
    // Load failures no longer have a header to live under — surface them as a
    // slim banner above the list.
    if let LoadStatus::Failed(error) = &ui.session.status {
        body = body.push(
            container(
                text(format!("Failed: {error}"))
                    .size(CAPTION_TEXT_SIZE)
                    .font(ui.config.ui_font)
                    .color(theme.removed_text),
            )
            .width(Length::Fill)
            .padding([8, 12]),
        );
    }
    body = body.push(revision_list);
    body = body.push(build_footer(ui, theme));

    container(body)
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| sidebar_panel_style(theme))
        .into()
}

/// Focus target id for the revset input.
pub const REVSET_INPUT_ID: &str = "revset-input";

/// The monospace revset (jj) / revision-range (git) input at the top of the
/// sidebar, with a caret that opens the presets menu. Submitting (Enter) or
/// picking a preset re-evaluates the log.
fn build_revset_filter(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let placeholder = match ui.session.repository.as_ref().map(|r| r.vcs) {
        Some(Vcs::Git) => "revision range — e.g. --all, main..@",
        _ => "revset — e.g. all(), mine()",
    };

    let input = text_input(placeholder, &ui.session.revset)
        .id(REVSET_INPUT_ID)
        .padding(Padding::from([5, 8]))
        .size(12)
        .font(ui.config.mono_font)
        .width(Length::Fill)
        .on_input(Message::RevsetChanged)
        .on_submit(Message::RevsetSubmit)
        .style(move |_, _| text_input::Style {
            background: Background::Color(theme.background),
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: 6.0.into(),
            },
            icon: theme.muted_text,
            placeholder: theme.subtle_text,
            value: theme.text,
            selection: Color {
                a: 0.25,
                ..theme.accent
            },
        });

    // `mouse_area` (not `button`) so the presets menu opens on mouse-*down*
    // while held — required for the native NSMenu's press-drag-release select.
    // Hover is tracked manually (mouse_area has no built-in hover style).
    let caret_hovered = ui.hovered == Some(HoverTarget::RevsetCaret);
    // No press handler — the press falls through to the wrapping `AnchorArea`
    // (the `text_input` captures its own), which reports the whole bar's rect so
    // the presets menu anchors edge-to-edge below it.
    let caret = mouse_area(
        // Drawn triangle (see `toolbar::caret_glyph`) so it centers exactly.
        // Box height = the input's size-12 line box and the same vertical
        // padding (5), so the hover fill lines up with the input box top-to-
        // bottom instead of hugging the small glyph.
        container(crate::toolbar::caret_glyph(theme.muted_text, 12.0 * 1.3))
            .padding(Padding::from([5, 7]))
            .align_y(alignment::Vertical::Center)
            .style(move |_| crate::toolbar::caret_hover_style(theme, caret_hovered)),
    )
    .on_enter(Message::SetHover(Some(HoverTarget::RevsetCaret)))
    .on_exit(Message::SetHover(None))
    .interaction(mouse::Interaction::Pointer);

    let bar = crate::menu::anchor_area(
        row![input, caret]
            .spacing(4)
            .align_y(alignment::Vertical::Center),
        |rect| Message::OpenToolbarMenu(ToolbarMenu::RevsetPresets, rect),
    );

    let hairline = container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        });

    column![
        container(bar)
            .width(Length::Fill)
            .padding(Padding::from([6, 8])),
        hairline,
    ]
    .into()
}

const FOOTER_TEXT_SIZE: f32 = 12.0;

/// Thin status bar pinned to the bottom of the commit-log pane: the working
/// copy's branch + ahead/behind on the left, the total change count on the
/// right. Monospace, dim, with a hairline top border. The "uncommitted files"
/// count common to git tools is deliberately omitted — jj's `@` is always a
/// commit, so it would be semantically wrong here.
fn build_footer(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let font = ui.config.mono_font;
    let dim = theme.subtle_text;

    let left: Element<'_, Message> = match &ui.session.branch_status {
        Some(status) => {
            // Branch glyph + name; the tracked upstream rides along as a tooltip.
            let name = row![
                text("\u{2387}")
                    .size(FOOTER_TEXT_SIZE)
                    .font(font)
                    .color(dim),
                text(status.branch.clone())
                    .size(FOOTER_TEXT_SIZE)
                    .font(font)
                    .color(dim),
            ]
            .spacing(5)
            .align_y(alignment::Vertical::Center);
            let name: Element<'_, Message> = match &status.upstream {
                Some(upstream) => tooltip(
                    name,
                    container(
                        text(upstream.clone())
                            .size(FOOTER_TEXT_SIZE)
                            .font(font)
                            .color(theme.text),
                    )
                    .padding([3, 7])
                    .style(move |_| footer_tooltip_style(theme)),
                    tooltip::Position::Top,
                )
                .gap(4)
                .into(),
                None => name.into(),
            };

            let mut group = row![name].spacing(8).align_y(alignment::Vertical::Center);
            // Ahead/behind only mean something against a tracked upstream.
            if status.upstream.is_some() {
                if status.ahead == 0 && status.behind == 0 {
                    group =
                        group.push(text("in sync").size(FOOTER_TEXT_SIZE).font(font).color(dim));
                } else {
                    if status.ahead > 0 {
                        group = group.push(
                            text(format!("\u{2191}{}", status.ahead))
                                .size(FOOTER_TEXT_SIZE)
                                .font(font)
                                .color(theme.added_text),
                        );
                    }
                    if status.behind > 0 {
                        group = group.push(
                            text(format!("\u{2193}{}", status.behind))
                                .size(FOOTER_TEXT_SIZE)
                                .font(font)
                                .color(theme.removed_text),
                        );
                    }
                }
            }
            group.into()
        }
        None => row![].into(),
    };

    let right = text(format!("{} changes", thousands(ui.session.commits.len())))
        .size(FOOTER_TEXT_SIZE)
        .font(font)
        .color(dim);

    let bar = row![left, container(text("")).width(Length::Fill), right]
        .align_y(alignment::Vertical::Center)
        .spacing(8);

    // iced's `Border` paints all four edges; draw the hairline top rule as a
    // 1px line above the bar instead.
    let hairline = container(text(""))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        });

    column![
        hairline,
        container(bar)
            .width(Length::Fill)
            .padding([6, 12])
            .style(move |_| container::Style::default().background(theme.panel_background)),
    ]
    .into()
}

fn footer_tooltip_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        text_color: Some(theme.text),
        border: Border {
            color: theme.border,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    }
}

/// Group an integer with `,` thousands separators, e.g. 1410 -> "1,410".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn build_revision_list<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let metrics = sidebar_text_metrics(ui.config);
    let graph_style = RevisionGraphStyle {
        lane_base_color: theme.lane_base,
        missing_color: theme.subtle_text,
    };

    // Files show under the selected commit, only while its inline list is open.
    let expanded_index = ui
        .session
        .selected_commit_index
        .filter(|_| ui.file_list_expanded);

    let (file_widgets, file_badge_width): (Vec<FileRowTemplate>, f32) = if let Some(expanded) =
        expanded_index
        && matches!(ui.session.status, LoadStatus::Loaded)
        && !ui.session.document.files.is_empty()
    {
        let widest_addition = ui
            .session
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
            .session
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
        let badge_w = file_badge_width(&ui.session.document.files, &badge_metrics);

        // Mirror `draw_file`'s layout exactly so truncation kicks in at the
        // same threshold the renderer clips at:
        //   [gutter] [badge] gap [path] gap [+N] gap [-N] right_pad
        let expanded_lane_count = ui.session.graph.frame(expanded, usize::MAX).after.len();
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
        let display_models =
            file_display_models(&ui.session.document.files, display_width, &metrics);
        let templates = ui
            .session
            .document
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
            .collect();
        (templates, badge_w)
    } else {
        (Vec::new(), FILE_BADGE_MIN_WIDTH)
    };

    let file_count = file_widgets.len();
    let expanded = expanded_index
        .filter(|_| file_count > 0)
        .map(|index| (index, file_count));

    // The per-row lane fold + prefix lengths are precomputed once and held in
    // `Diffui`; the closures below build a single visible row's view from them
    // on demand, so the widget never materializes all ~N rows.
    let graph = &ui.session.graph;
    let prefix_lens = &ui.session.sidebar_prefix_lens;
    let commits = &ui.session.commits;
    let selected = ui.session.selected_revision.clone();
    let file_list_expanded = ui.file_list_expanded;
    let build_revision = Box::new(move |index: usize| {
        build_revision_row(
            commits,
            graph,
            prefix_lens,
            theme,
            &graph_style,
            &selected,
            file_list_expanded,
            index,
        )
    });

    // File rows render under the expanded commit, so they share its
    // continuation lane state (the post-trim snapshot of that row's fold).
    let continuation = expanded_index
        .map(|index| ui.session.graph.frame(index, usize::MAX).after)
        .unwrap_or_default();
    let (continuation_labels, continuation_segments) = expanded_index
        .map(|index| {
            let lane = ui.session.graph.fold(index, usize::MAX);
            (lane.continuation_labels, lane.continuation_segments)
        })
        .unwrap_or_default();
    let build_file = Box::new(move |file_index: usize| {
        build_file_row(
            &file_widgets[file_index],
            &continuation,
            &continuation_labels,
            &continuation_segments,
            theme,
        )
    });

    let selected_row = match &ui.session.selected_revision {
        RevisionSelection::WorkingCopy => Some(RowSelectionKey::WorkingCopy),
        RevisionSelection::Commit(id) => Some(RowSelectionKey::Commit(id.clone())),
    };

    RevisionList::new(
        ui.session.commits.len(),
        expanded,
        build_revision,
        build_file,
        selected_row,
        Some(ui.selected_file),
        ui.session.selected_commit_index,
        revision_list_style(theme, ui.config, file_badge_width),
        Message::SelectRowKey,
        Message::SelectFile,
    )
    .width(Length::Fill)
    .reveal_selected(ui.revision_reveal_token)
    .on_scroll(Message::SidebarScrolled)
    .restore_scroll(ui.sidebar_scroll_offset, ui.scroll_restore_token)
    .on_context_menu(Message::RevisionContextMenu)
    .into()
}

/// Build the display view for one revision row from the precomputed fold +
/// store. Called only for on-screen rows.
#[allow(clippy::too_many_arguments)]
fn build_revision_row(
    commits: &CommitStore,
    graph: &GraphLayout,
    prefix_lens: &[usize],
    theme: ThemeSpec,
    graph_style: &RevisionGraphStyle,
    selected: &RevisionSelection,
    file_list_expanded: bool,
    index: usize,
) -> RevisionRowView {
    let commit = commits.row(index);
    let change_id = commit.change_id();
    let lane_frame = graph.frame(index, usize::MAX);
    let bookmarks = commit.bookmarks();
    let unique_len = prefix_lens.get(index).copied().unwrap_or(REVISION_ID_CHARS);
    let label_len = revision_id_display_len(unique_len, change_id);
    let id_prefix: String = change_id.chars().take(unique_len).collect();
    let id_suffix: String = change_id
        .chars()
        .skip(unique_len)
        .take(label_len.saturating_sub(unique_len))
        .collect();
    let commit_id_short = truncate_end(commit.commit_id(), COMMIT_ID_CHARS);

    let lane_color = graph_style.lane_color(lane_frame.node_lane);
    let mut bookmark_chips = Vec::with_capacity(bookmarks.len());
    for bookmark in bookmarks {
        // Remote/untracked bookmarks contain `@` (e.g. `main@origin`) and
        // render outlined (transparent fill + 1px lane-color border) so they
        // read as "tracking" rather than "live" bookmarks.
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
    if commit.is_empty() == Some(true) {
        status_chips.push(IndicatorChip {
            label: "empty".to_owned(),
            background: Color::TRANSPARENT,
            text_color: theme.subtle_text,
            border_color: Some(theme.subtle_text),
            border_dashed: true,
        });
    }
    if commit.has_conflict() {
        status_chips.push(IndicatorChip {
            label: "conflict".to_owned(),
            background: chip_background(theme.conflict_marker),
            text_color: theme.conflict_marker,
            border_color: None,
            border_dashed: false,
        });
    }

    let selection_key = if commit.is_working_copy() {
        RowSelectionKey::WorkingCopy
    } else {
        RowSelectionKey::Commit(commit.commit_id().to_owned())
    };
    let is_expanded = is_expanded_commit(selected, file_list_expanded, commit);
    let is_selected = match selected {
        RevisionSelection::WorkingCopy => commit.is_working_copy(),
        RevisionSelection::Commit(id) => !commit.is_working_copy() && id == commit.commit_id(),
    };

    let lane = graph.fold(index, usize::MAX);
    RevisionRowView {
        selection_key,
        change_id_prefix: id_prefix,
        change_id_suffix: id_suffix,
        commit_id_short,
        author: commit.author().to_owned(),
        description: commit.description().to_owned(),
        description_color: commit_description_color(commit, theme),
        bookmark_chips,
        status_chips,
        lane_color,
        frame: lane_frame,
        lane_labels: lane.labels,
        lane_segments_before: lane.segments_before,
        lane_segments_after: lane.segments_after,
        // The collapse/expand chevron shows only on the selected row.
        collapse_chevron: is_selected.then_some(is_expanded),
    }
}

/// Build the display view for one file row under the expanded commit.
fn build_file_row(
    template: &FileRowTemplate,
    continuation: &[Option<GraphEdgeType>],
    continuation_labels: &[Vec<String>],
    continuation_segments: &[Option<usize>],
    theme: ThemeSpec,
) -> FileRowView {
    FileRowView {
        primary: template.primary.clone(),
        secondary: template.secondary.clone(),
        raw_path: template.raw_path.clone(),
        status_label: template.status_label.clone(),
        status_background: chip_background(template.status_color),
        status_text: template.status_color,
        additions: template.additions,
        deletions: template.deletions,
        additions_text: theme.added_text,
        deletions_text: theme.removed_text,
        continuation: continuation.to_vec(),
        additions_width: template.additions_width,
        deletions_width: template.deletions_width,
        primary_color: theme.text,
        secondary_color: theme.muted_text,
        file_index: template.file_index,
        lane_labels: continuation_labels.to_vec(),
        lane_segments: continuation_segments.to_vec(),
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
fn is_expanded_commit(selected: &RevisionSelection, expanded: bool, commit: RowView) -> bool {
    if !expanded {
        return false;
    }
    match selected {
        RevisionSelection::WorkingCopy => commit.is_working_copy(),
        RevisionSelection::Commit(id) => !commit.is_working_copy() && id == commit.commit_id(),
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

fn commit_description_color(commit: RowView, theme: ThemeSpec) -> Color {
    if commit.has_description() {
        return theme.text;
    }

    match commit.is_empty() {
        Some(true) => theme.added_text,
        Some(false) => theme.note_text,
        None => theme.note_text,
    }
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
    use diffui_core::{CommitStoreBuilder, CommitSummary, DiffFileStatus};

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
            shortest_change_id_len: None,
            description: String::new(),
            author: String::new(),
            has_description: false,
            is_empty: None,
            has_conflict: false,
            is_working_copy: false,
            bookmarks: Vec::new(),
        }
    }

    fn store(commits: Vec<CommitSummary>) -> CommitStore {
        let mut builder = CommitStoreBuilder::with_capacity(commits.len());
        for commit in commits {
            builder.push(commit);
        }
        builder.finish()
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
        // No precomputed lengths (the git backend leaves them `None`), so this
        // exercises the sorted-neighbor derivation. "abc"/"abd" collide on
        // "ab" and need 3 chars to disambiguate; "z" is unique at 1.
        let commits = vec![
            commit_summary("abc"),
            commit_summary("abd"),
            commit_summary("z"),
        ];

        assert_eq!(store(commits).shortest_unique_prefix_lens(), vec![3, 3, 1]);
    }

    #[test]
    fn revision_id_prefix_prefers_precomputed_len() {
        // When jj supplies `shortest_change_id_len` we trust it over the
        // neighbor derivation, clamped to the id's own length.
        let mut commits = vec![commit_summary("abcdef"), commit_summary("abcxyz")];
        commits[0].shortest_change_id_len = Some(4);
        commits[1].shortest_change_id_len = Some(99);

        assert_eq!(store(commits).shortest_unique_prefix_lens(), vec![4, 6]);
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
}
