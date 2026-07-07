use iced::{
    Background, Border, Color, Element, Font, Length, Padding, alignment, mouse,
    widget::{Space, column, container, mouse_area, row, text, text_input, tooltip},
};

use crate::config::AppConfig;
use crate::graph_layout::GraphLayout;
use crate::graph_view::{self, RevisionGraphStyle};
use crate::icons;
use crate::repository::Vcs;
use crate::revision_list::{
    self, FileRowView, IndicatorChip, RevisionList, RevisionListStyle, RevisionRowView,
    RowSelectionKey,
};
use crate::theme::{self, ThemeSpec, chip_background, sidebar_panel_style};
use crate::{Diffui, HoverTarget, LoadStatus, Message, ToolbarMenu};
use diffui_core::{
    CommitStore, DiffFile, DiffFileStatus, FileTreeRow, RevisionSelection, RowView, file_tree_rows,
};
use jj_lib::graph::GraphEdgeType;
use std::collections::HashSet;
use std::rc::Rc;

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
/// (`divergent`, 9 chars). Below this width the chip rail starts dropping
/// rails, so the user loses the conflict/divergence signal.
pub fn min_width(config: AppConfig) -> f32 {
    let id_metrics = TextMetrics::iced(config.mono_font, CAPTION_TEXT_SIZE);
    let ui_metrics = TextMetrics::iced(config.ui_font, CAPTION_TEXT_SIZE);

    let change_id_w = id_metrics.measure(&"a".repeat(REVISION_ID_CHARS));
    let commit_id_w = id_metrics.measure(&"a".repeat(COMMIT_ID_CHARS));
    let at_w = id_metrics.measure("@");
    let author_w = ui_metrics.measure("Author Name");
    let plus_n_w = chip_width("+9", &ui_metrics);
    let conflict_w = chip_width("divergent", &ui_metrics);

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
        // Shared Lucide chevron (see `toolbar::caret_glyph`), centered in a box
        // whose height = the input's size-12 line box; with the same vertical
        // padding (5), the hover fill lines up with the input box top-to-bottom
        // instead of hugging the small glyph.
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
                icons::icon(icons::GIT_BRANCH, FOOTER_TEXT_SIZE, dim),
                text(&status.branch)
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
                        text(upstream)
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
                            row![
                                icons::icon(icons::ARROW_UP, FOOTER_TEXT_SIZE, theme.added_text),
                                text(status.ahead.to_string())
                                    .size(FOOTER_TEXT_SIZE)
                                    .font(font)
                                    .color(theme.added_text),
                            ]
                            .spacing(1)
                            .align_y(alignment::Vertical::Center),
                        );
                    }
                    if status.behind > 0 {
                        group = group.push(
                            row![
                                icons::icon(
                                    icons::ARROW_DOWN,
                                    FOOTER_TEXT_SIZE,
                                    theme.removed_text
                                ),
                                text(status.behind.to_string())
                                    .size(FOOTER_TEXT_SIZE)
                                    .font(font)
                                    .color(theme.removed_text),
                            ]
                            .spacing(1)
                            .align_y(alignment::Vertical::Center),
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

    let bar = row![left, Space::new().width(Length::Fill), right]
        .align_y(alignment::Vertical::Center)
        .spacing(8);

    // iced's `Border` paints all four edges; draw the hairline top rule as a
    // 1px line above the bar instead.
    let hairline = container(Space::new())
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
    let graph_style = RevisionGraphStyle {
        lane_base_color: theme.lane_base,
        missing_color: theme.subtle_text,
    };

    // Files show under the selected commit, only while its inline list is open.
    let expanded_index = ui
        .session
        .selected_commit_index
        .filter(|_| ui.file_list_expanded);

    // Stat-column widths and the flattened file tree are derived purely from the
    // document (and, for the tree, the collapse set) — never from the scroll
    // position. But `view()` re-runs on every diff-scroll file-boundary crossing
    // (the diff view publishes `SelectFile`, the only thing that forces an iced
    // tree rebuild), so recomputing them here re-shaped ~5·N strings per crossing
    // and tanked the frame rate on large PRs. Memoize on document identity
    // (`document_id` + count — the count also catches streaming `extend`s that
    // grow the set under a stable id) so a rebuild that didn't change the file
    // set is free; templates themselves are built lazily per visible row below.
    let (tree_rows, additions_w, deletions_w, file_badge_width) = if expanded_index.is_some()
        && matches!(ui.session.status, LoadStatus::Loaded)
        && !ui.session.document.files.is_empty()
    {
        let files = &ui.session.document.files;
        let document_id = ui.session.document_id;
        let count = files.len();
        let mut cache = ui.sidebar_file_cache.borrow_mut();
        let widths = cache.stat_widths(document_id, count, ui.config, files);
        let tree_rows = cache.tree_rows(document_id, count, &ui.collapsed_dirs, files);
        (tree_rows, widths.additions, widths.deletions, widths.badge)
    } else {
        (Rc::new(Vec::new()), 0.0, 0.0, FILE_BADGE_MIN_WIDTH)
    };

    let file_count = tree_rows.len();
    let expanded = expanded_index
        .filter(|_| file_count > 0)
        .map(|index| (index, file_count));

    // Flat sidebar row of the selected file, for the keyboard-nav reveal: the
    // expanded commit's row, then its file rows in tree-display order. `None`
    // when the file list is closed or the file isn't currently shown, which
    // tells the widget to schedule no scroll.
    let reveal_file_flat = expanded.and_then(|(commit, _)| {
        tree_rows
            .iter()
            .position(|row| {
                matches!(row, FileTreeRow::File { file_index, .. } if *file_index == ui.selected_file)
            })
            .map(|display| commit + 1 + display)
    });

    // The per-row lane fold + prefix lengths are precomputed once and held in
    // `Diffui`; the closures below build a single visible row's view from them
    // on demand, so the widget never materializes all ~N rows.
    let graph = &ui.session.graph;
    let prefix_lens = &ui.session.sidebar_prefix_lens;
    let commits = &ui.session.commits;
    let selected = &ui.session.selected_revision;
    let file_list_expanded = ui.file_list_expanded;
    let build_revision = Box::new(move |index: usize| {
        build_revision_row(
            commits,
            graph,
            prefix_lens,
            theme,
            &graph_style,
            selected,
            file_list_expanded,
            index,
        )
    });

    // File rows render under the expanded commit, so they share its
    // continuation lane state (the post-trim snapshot of that row's fold).
    let (continuation, continuation_columns) = expanded_index
        .map(|index| {
            let frame = ui.session.graph.frame(index, usize::MAX);
            let columns = frame.display_columns();
            (frame.after, columns)
        })
        .unwrap_or_default();
    let (continuation_labels, continuation_segments) = expanded_index
        .map(|index| {
            let lane = ui.session.graph.fold(index, usize::MAX);
            (lane.continuation_labels, lane.continuation_segments)
        })
        .unwrap_or_default();
    // Every file row shares the parent's continuation state, so build it into
    // `Rc`s once and hand each row a cheap refcount clone instead of deep-copying
    // four Vecs per row on every rebuild.
    let continuation: Rc<[Option<GraphEdgeType>]> = continuation.into();
    let continuation_columns: Rc<[Option<usize>]> = continuation_columns.into();
    let continuation_labels: Rc<[Vec<String>]> = continuation_labels.into();
    let continuation_segments: Rc<[Option<usize>]> = continuation_segments.into();
    let files = &ui.session.document.files;
    let build_file = Box::new(move |row_index: usize| {
        let template = file_row_template(
            &tree_rows[row_index],
            files,
            additions_w,
            deletions_w,
            theme,
        );
        build_file_row(
            template,
            continuation.clone(),
            continuation_columns.clone(),
            continuation_labels.clone(),
            continuation_segments.clone(),
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
        Message::SidebarFileRow,
    )
    .width(Length::Fill)
    .reveal_selected(ui.revision_reveal_token)
    .reveal_file(ui.sidebar_file_reveal_token, reveal_file_flat)
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
    // Warped display columns for this row and the one above (row 0 has no
    // transition, so its own packing stands in).
    let columns = lane_frame.display_columns();
    let prev_columns = if index > 0 {
        graph.frame(index - 1, usize::MAX).display_columns()
    } else {
        columns.clone()
    };
    let bookmarks = commit.bookmarks();
    let unique_len = prefix_lens.get(index).copied().unwrap_or(REVISION_ID_CHARS);
    let label_len = revision_id_display_len(unique_len, change_id);
    let id_prefix: String = change_id.chars().take(unique_len).collect();
    let mut id_suffix: String = change_id
        .chars()
        .skip(unique_len)
        .take(label_len.saturating_sub(unique_len))
        .collect();
    // jj log prints divergent / hidden copies as `changeid/N`; the suffix is
    // how a revset addresses one copy (`-r xyz/1`), so render the same form.
    if let Some(offset) = commit.change_offset() {
        id_suffix.push('/');
        id_suffix.push_str(&offset.to_string());
    }
    let commit_id_short = truncate_end(commit.commit_id(), COMMIT_ID_CHARS);

    let lane_color = graph_style.lane_color(lane_frame.node_lane);
    let mut bookmark_chips = Vec::with_capacity(bookmarks.len());
    for bookmark in bookmarks {
        // Core authors the label shapes and jj forbids `@` inside bookmark
        // and remote names, so a trailing `@` can only be another workspace's
        // working copy (`name@`) and an interior `@` a remote bookmark
        // (`main@origin`). Workspace chips get a folder glyph (another
        // working-copy *directory*) and the working-copy accent — the lane
        // palette is deliberately decoupled from it — so they read as a
        // different kind of thing than the bookmark pills sharing the rail.
        // Remotes render outlined (transparent fill + 1px lane-color border)
        // so they read as "tracking" rather than "live" bookmarks.
        let is_workspace = bookmark.ends_with('@');
        let is_remote = !is_workspace && bookmark.contains('@');
        let chip_color = if is_workspace { theme.accent } else { lane_color };
        bookmark_chips.push(IndicatorChip {
            label: bookmark.clone(),
            background: if is_remote {
                Color::TRANSPARENT
            } else {
                chip_background(chip_color)
            },
            text_color: chip_color,
            border_color: is_remote.then_some(lane_color),
            border_dashed: false,
            icon: is_workspace.then_some(icons::FOLDER),
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
            icon: None,
        });
    }
    if commit.has_conflict() {
        status_chips.push(IndicatorChip {
            label: "conflict".to_owned(),
            background: chip_background(theme.conflict_marker),
            text_color: theme.conflict_marker,
            border_color: None,
            border_dashed: false,
            icon: None,
        });
    }
    if commit.is_hidden() {
        // jj log's "(hidden)": rewritten/abandoned, still shown because a ref
        // (e.g. a stale remote bookmark) pins it into the revset. Takes
        // precedence over the divergent chip, matching jj's log template.
        status_chips.push(IndicatorChip {
            label: "hidden".to_owned(),
            background: Color::TRANSPARENT,
            text_color: theme.subtle_text,
            border_color: Some(theme.subtle_text),
            border_dashed: false,
            icon: None,
        });
    } else if commit.is_divergent() {
        // jj log flags these with a change-offset suffix: the change id maps
        // to several visible commits. Amber (not conflict-red) — it's a
        // warning about identity, not about tree state.
        status_chips.push(IndicatorChip {
            label: "divergent".to_owned(),
            background: chip_background(theme.modified_token),
            text_color: theme.modified_token,
            border_color: None,
            border_dashed: false,
            icon: None,
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
        columns,
        prev_columns,
        lane_labels: lane.labels,
        lane_segments_before: lane.segments_before,
        lane_segments_after: lane.segments_after,
        // The collapse/expand chevron shows only on the selected row.
        collapse_chevron: is_selected.then_some(is_expanded),
    }
}

/// Build the display view for one file row under the expanded commit.
fn build_file_row(
    template: FileRowTemplate,
    continuation: Rc<[Option<GraphEdgeType>]>,
    continuation_columns: Rc<[Option<usize>]>,
    continuation_labels: Rc<[Vec<String>]>,
    continuation_segments: Rc<[Option<usize>]>,
    theme: ThemeSpec,
) -> FileRowView {
    FileRowView {
        primary: template.label,
        raw_path: template.raw_path,
        status_label: template.status_label,
        status_background: chip_background(template.status_color),
        status_text: template.status_color,
        additions: template.additions,
        deletions: template.deletions,
        additions_text: theme.added_text,
        deletions_text: theme.removed_text,
        continuation,
        columns: continuation_columns,
        additions_width: template.additions_width,
        deletions_width: template.deletions_width,
        primary_color: theme.text,
        indent: template.indent,
        chevron: template.chevron,
        file_index: template.file_index,
        lane_labels: continuation_labels,
        lane_segments: continuation_segments,
    }
}

struct FileRowTemplate {
    label: String,
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
    indent: f32,
    chevron: Option<bool>,
}

/// View-time memo for the file list's document-derived layout: the stat-column
/// widths and the flattened file tree. The sidebar is rebuilt on every `view()`,
/// and `view()` re-runs on every diff-scroll *file-boundary crossing* (the diff
/// view publishes `SelectFile`, the only thing that forces an iced widget-tree
/// rebuild — a plain redraw doesn't). Recomputing these there re-shaped ~5·N
/// strings through `cosmic_text` per crossing, which tanked the frame rate when
/// the list was expanded over a large PR. Neither value depends on the scroll
/// position, so we key them on document identity and reuse across rebuilds that
/// didn't change the file set.
#[derive(Debug, Clone, Default)]
pub(crate) struct SidebarFileCache {
    widths: Option<(DocKey, FileStatWidths)>,
    tree: Option<(TreeKey, Rc<Vec<FileTreeRow>>)>,
}

/// Identity of the file set a cached value was computed against. `document_id`
/// is stamped fresh on every document *replacement*, but a streaming PR load
/// `extend`s the existing document in place (same id, growing length), so the
/// count is part of the key too. `font` invalidates on a config font change,
/// which would re-shape the stat strings to a different width.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DocKey {
    document_id: u64,
    file_count: usize,
    font: Font,
}

#[derive(Debug, Clone, Copy)]
struct FileStatWidths {
    additions: f32,
    deletions: f32,
    badge: f32,
}

/// The tree folds in the collapse set (it prunes collapsed dirs) but, unlike the
/// widths, is independent of font and theme — so it gets its own key.
#[derive(Debug, Clone, PartialEq)]
struct TreeKey {
    document_id: u64,
    file_count: usize,
    collapsed: HashSet<String>,
}

impl SidebarFileCache {
    fn stat_widths(
        &mut self,
        document_id: u64,
        file_count: usize,
        config: AppConfig,
        files: &[DiffFile],
    ) -> FileStatWidths {
        let key = DocKey {
            document_id,
            file_count,
            font: config.ui_font,
        };
        if let Some((cached_key, widths)) = &self.widths
            && *cached_key == key
        {
            return *widths;
        }
        let widths = compute_stat_widths(files, config);
        self.widths = Some((key, widths));
        widths
    }

    fn tree_rows(
        &mut self,
        document_id: u64,
        file_count: usize,
        collapsed: &HashSet<String>,
        files: &[DiffFile],
    ) -> Rc<Vec<FileTreeRow>> {
        if let Some((key, rows)) = &self.tree
            && key.document_id == document_id
            && key.file_count == file_count
            && &key.collapsed == collapsed
        {
            return Rc::clone(rows);
        }
        let rows = Rc::new(file_tree_rows(files, collapsed));
        self.tree = Some((
            TreeKey {
                document_id,
                file_count,
                collapsed: collapsed.clone(),
            },
            Rc::clone(&rows),
        ));
        rows
    }
}

/// Stat-column widths for the file list, measured once per document. The widest
/// rendered `+N` / `−N` is the file with the most additions / deletions — more
/// digits ⇒ a wider string in the UI font's near-tabular figures — so we shape
/// one string per column. The old form measured every file's stat through a
/// `max_by` whose comparator shaped *both* sides, i.e. ~2·N `cosmic_text` shapes
/// per column.
fn compute_stat_widths(files: &[DiffFile], config: AppConfig) -> FileStatWidths {
    let metrics = sidebar_text_metrics(config);
    let badge_metrics = badge_text_metrics(config);
    let max_additions = files.iter().map(|file| file.additions).max().unwrap_or(0);
    let max_deletions = files.iter().map(|file| file.deletions).max().unwrap_or(0);
    FileStatWidths {
        additions: file_stat_width(&format!("+{max_additions}"), &metrics),
        deletions: file_stat_width(&format!("-{max_deletions}"), &metrics),
        badge: file_badge_width(files, &badge_metrics),
    }
}

/// Build one file-list row template from a flattened tree row. Called lazily,
/// per *visible* row, by the `RevisionList` virtualization closure — the full
/// set is never materialized (only ~a screenful exist at once).
fn file_row_template(
    row: &FileTreeRow,
    files: &[DiffFile],
    additions_width: f32,
    deletions_width: f32,
    theme: ThemeSpec,
) -> FileRowTemplate {
    match row {
        FileTreeRow::Dir {
            label,
            path,
            depth,
            collapsed,
        } => FileRowTemplate {
            label: label.clone(),
            raw_path: path.clone(),
            status_label: String::new(),
            status_color: theme.subtle_text,
            additions: 0,
            deletions: 0,
            file_index: usize::MAX,
            additions_width,
            deletions_width,
            indent: *depth as f32 * revision_list::FILE_TREE_INDENT,
            chevron: Some(*collapsed),
        },
        FileTreeRow::File {
            file_index,
            label,
            depth,
        } => {
            let file = &files[*file_index];
            FileRowTemplate {
                label: label.clone(),
                raw_path: file.path.clone(),
                status_label: file.status.short_label().to_owned(),
                status_color: file_status_color(file.status, theme),
                additions: file.additions,
                deletions: file.deletions,
                file_index: *file_index,
                additions_width,
                deletions_width,
                indent: *depth as f32 * revision_list::FILE_TREE_INDENT,
                chevron: None,
            }
        }
    }
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
        DiffFileStatus::Conflicted => theme.conflict_marker,
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
#[derive(Clone)]
pub enum TextMetrics {
    Iced { font: Font, size: f32 },
}

impl TextMetrics {
    fn iced(font: Font, size: f32) -> Self {
        Self::Iced { font, size }
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
        }
    }
}

fn sidebar_text_metrics(config: AppConfig) -> TextMetrics {
    TextMetrics::iced(config.ui_font, CAPTION_TEXT_SIZE)
}

fn badge_text_metrics(config: AppConfig) -> TextMetrics {
    TextMetrics::iced(config.ui_font, FILE_BADGE_TEXT_SIZE)
}

fn file_stat_width(text: &str, metrics: &TextMetrics) -> f32 {
    (metrics.measure(text) + FILE_STAT_HORIZONTAL_PADDING * 2.0).max(FILE_STAT_MIN_WIDTH)
}

/// Width of the status badge column ("M", "A", "D", "R", …). We add padding so
/// two-letter labels like "MM" still fit comfortably. A diff has only a handful
/// of distinct status labels, so we shape each distinct one once rather than
/// re-shaping every file's (the scan itself stays O(files), but the expensive
/// `cosmic_text` measure runs at most a few times).
fn file_badge_width(files: &[DiffFile], metrics: &TextMetrics) -> f32 {
    let mut seen: Vec<&str> = Vec::new();
    let mut widest = 0.0_f32;
    for file in files {
        let label = file.status.short_label();
        if !seen.contains(&label) {
            seen.push(label);
            widest = widest.max(metrics.measure(label));
        }
    }
    (widest + FILE_BADGE_HORIZONTAL_PADDING * 2.0).max(FILE_BADGE_MIN_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diffui_core::{CommitStoreBuilder, CommitSummary};

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
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
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
}
