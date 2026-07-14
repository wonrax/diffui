use iced::{
    Background, Border, Color, Element, Font, Length, alignment,
    widget::{Space, column, container, row, text, tooltip},
};

use crate::chip::{self, Chip};
use crate::config::AppConfig;
use crate::graph_layout::GraphLayout;
use crate::graph_view::{self, RevisionGraphStyle};
use crate::icons;
use crate::measure;
use crate::repository::Vcs;
use crate::revision_list::{
    self, FileRowView, RevisionList, RevisionListStyle, RevisionRowView, RowSelectionKey,
};
use crate::theme::{
    self, ThemeSpec, chip_background, emphasis_font, file_status_color, sidebar_panel_style,
};
use crate::{Diffui, HoverTarget, LoadStatus, Message, ToolbarMenu};
use diffui_core::{CommitStore, DiffFile, FileTreeRow, RevisionSelection, RowView, file_tree_rows};
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
    let id_width =
        |content: &str| measure::line_width(content, CAPTION_TEXT_SIZE, config.mono_font);

    let change_id_w = id_width(&"a".repeat(REVISION_ID_CHARS));
    let commit_id_w = id_width(&"a".repeat(COMMIT_ID_CHARS));
    let at_w = id_width("@");
    let author_w = measure::line_width("Author Name", CAPTION_TEXT_SIZE, config.ui_font);
    let plus_n_w = chip::width("+9", None, config.ui_font);
    let conflict_w = chip::width("divergent", None, config.mono_font);

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

/// Mirrors `revision_list::CONTENT_PADDING` — the right-edge padding
/// inside a revision row. Kept here so `min_width` can budget the row
/// without exposing the constant through `revision_list`.
const REVISION_CONTENT_RIGHT_PAD: f32 = 12.0;

const CAPTION_TEXT_SIZE: f32 = theme::text_size::BODY;
pub(crate) const REVISION_ID_CHARS: usize = 12;
pub(crate) const COMMIT_ID_CHARS: usize = 12;

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
    // soft alert card above the list rather than bare red text.
    if let LoadStatus::Failed(error) = &ui.session.status {
        body = body.push(
            container(
                container(
                    text(format!("Failed: {error}"))
                        .size(CAPTION_TEXT_SIZE)
                        .font(ui.config.ui_font)
                        .color(theme.removed_text),
                )
                .width(Length::Fill)
                .padding([6, 10])
                .style(move |_| container::Style {
                    background: Some(Background::Color(chip_background(theme.removed_text))),
                    border: Border {
                        width: 0.0,
                        color: Color::TRANSPARENT,
                        radius: theme::radius::CONTROL.into(),
                    },
                    ..container::Style::default()
                }),
            )
            .width(Length::Fill)
            .padding([6, 8]),
        );
    }
    body = body.push(revision_list);
    // Target mode: the op bar floats between the list and the footer while a
    // rebase/squash draft is picking its destination.
    if ui.op_draft.is_some() {
        body = body.push(build_op_bar(ui, theme));
    }
    body = body.push(build_footer(ui, theme));

    let draft_active = ui.op_draft.is_some();
    container(body)
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| {
            if draft_active {
                container::Style::default().background(draft_panel_background(theme))
            } else {
                sidebar_panel_style(theme)
            }
        })
        .into()
}

/// The sidebar's whole-pane wash while target mode is on: a light accent
/// tint over the panel, so "a draft is in progress" reads from anywhere in
/// the pane — not just the op bar. Also fed to the revision list's own
/// background fill (the widget paints over the container).
fn draft_panel_background(theme: ThemeSpec) -> Color {
    theme::mix(theme.panel_background, theme.accent, 0.05)
}

/// The target-mode strip: what's being moved, the placement toggle
/// (rebase only), the live preview line, and the key hints.
fn build_op_bar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    use diffui_core::{DraftKind, PlacementKind};
    let Some(draft_ui) = ui.op_draft.as_ref() else {
        return Space::new().into();
    };
    let draft = &draft_ui.draft;
    let is_rebase = matches!(draft.kind, DraftKind::Rebase { .. });
    // Rebase and merge both run the destination simulation; squash doesn't.
    let has_preview = !matches!(draft.kind, DraftKind::Squash);

    // The live destination readout: what a drop/confirm at the current
    // position would do, updated instantly on every drag-spot or candidate
    // change (the debounced simulation below trails it). `bool` = valid.
    let short_id = |index: usize| -> Option<String> {
        (index < ui.session.commits.len()).then(|| {
            ui.session
                .commits
                .row(index)
                .change_id()
                .chars()
                .take(8)
                .collect::<String>()
        })
    };
    let is_source_index = |index: usize| -> bool {
        index < ui.session.commits.len()
            && draft.is_source(ui.session.commits.row(index).commit_id())
    };
    let on_row_label = |id: String| match draft.kind {
        DraftKind::Rebase { .. } => format!("onto {id}"),
        DraftKind::Squash => format!("into {id}"),
        DraftKind::Merge => format!("with {id}"),
    };
    const INVALID_TARGET: &str = "can't target the revision being moved";
    let destination_line: Option<(String, bool)> = match draft_ui.hover_spot {
        Some(revision_list::DropSpot::OnRow(index)) => {
            if is_source_index(index) {
                Some((INVALID_TARGET.to_owned(), false))
            } else {
                short_id(index).map(|id| (on_row_label(id), true))
            }
        }
        Some(revision_list::DropSpot::Gap { above, below }) => {
            if is_source_index(above) || is_source_index(below) {
                Some((INVALID_TARGET.to_owned(), false))
            } else {
                match (short_id(below), short_id(above)) {
                    (Some(parent), Some(child)) => Some((
                        match draft.kind {
                            DraftKind::Rebase { .. } => {
                                format!("between {parent} and {child}")
                            }
                            // Gap drops resolve to the parent side for these.
                            DraftKind::Squash => format!("into {parent}"),
                            DraftKind::Merge => format!("with {parent}"),
                        },
                        true,
                    )),
                    _ => None,
                }
            }
        }
        None => draft.candidate.and_then(|index| {
            // Hover can arm a source row as candidate (keyboard nav skips
            // them) — surface the same invalid-target hint as a drag would.
            if is_source_index(index) {
                return Some((INVALID_TARGET.to_owned(), false));
            }
            short_id(index).map(|id| {
                (
                    match draft.kind {
                        DraftKind::Rebase { .. } => {
                            let place = match draft.placement {
                                PlacementKind::Onto => "onto",
                                PlacementKind::After => "after",
                                PlacementKind::Before => "before",
                            };
                            format!("{place} {id}")
                        }
                        DraftKind::Squash => format!("into {id}"),
                        DraftKind::Merge => format!("with {id}"),
                    },
                    true,
                )
            })
        }),
    };
    let hint: Element<'_, Message> = match &destination_line {
        Some((label, true)) => text(format!("→ {label}"))
            .size(theme::text_size::BODY)
            .font(emphasis_font(ui.config.ui_font, iced::font::Weight::Medium))
            .color(theme.accent)
            .into(),
        Some((label, false)) => text(label.clone())
            .size(theme::text_size::BODY)
            .font(ui.config.ui_font)
            .color(theme.removed_text)
            .into(),
        None => text(match draft.kind {
            DraftKind::Rebase { .. } => "— pick a destination",
            DraftKind::Squash => "— pick the revision to fold into",
            DraftKind::Merge => "— pick the other parent",
        })
        .size(theme::text_size::BODY)
        .font(ui.config.ui_font)
        .color(theme.muted_text)
        .into(),
    };

    let headline = row![
        icons::icon(
            if is_rebase {
                icons::GIT_BRANCH
            } else {
                icons::FILE_DIFF
            },
            15.0,
            theme.accent
        ),
        text(draft.headline())
            .size(theme::text_size::BODY)
            .font(emphasis_font(ui.config.ui_font, iced::font::Weight::Medium))
            .color(theme.text),
        hint,
        Space::new().width(Length::Fill),
        iced::widget::button(icons::icon(icons::CLOSE, 14.0, theme.muted_text))
            .padding([2, 6])
            .on_press(Message::DraftCancel)
            .style(move |_, status| theme::ghost_button_style(theme, status)),
    ]
    .spacing(7)
    .align_y(alignment::Vertical::Center);

    let mut body = column![headline].spacing(6);

    if is_rebase {
        // While a drag is live the toggle mirrors what a drop would do —
        // a row means onto, a gap means between (none of the three) — and
        // reverts to the sticky keyboard placement when the drag ends.
        let live_placement: Option<PlacementKind> = match draft_ui.hover_spot {
            Some(revision_list::DropSpot::OnRow(_)) => Some(PlacementKind::Onto),
            Some(revision_list::DropSpot::Gap { .. }) => None,
            None => Some(draft.placement),
        };
        let segment = |label: &'static str, key: &'static str, placement: PlacementKind| {
            let active = live_placement == Some(placement);
            let label_text = row![
                text(label)
                    .size(theme::text_size::UI)
                    .font(ui.config.ui_font)
                    .color(if active { theme.background } else { theme.text }),
                text(key)
                    .size(theme::text_size::CAPTION)
                    .font(ui.config.mono_font)
                    .color(if active {
                        theme.background
                    } else {
                        theme.subtle_text
                    }),
            ]
            .spacing(4)
            .align_y(alignment::Vertical::Center);
            iced::widget::button(label_text)
                .padding([3, 9])
                .on_press(Message::DraftPlacement(placement))
                .style(move |_, status| {
                    if active {
                        theme::primary_button_style(theme)
                    } else {
                        theme::ghost_button_style(theme, status)
                    }
                })
        };
        let mut segments = row![
            segment("Onto", "o", PlacementKind::Onto),
            segment("After", "a", PlacementKind::After),
            segment("Before", "b", PlacementKind::Before),
        ]
        .spacing(4)
        .align_y(alignment::Vertical::Center);
        // Gap hover: the drop means "between" — surfaced as a fourth, live
        // indicator chip since it isn't one of the sticky placements.
        if matches!(
            draft_ui.hover_spot,
            Some(revision_list::DropSpot::Gap { .. })
        ) {
            segments = segments.push(
                container(
                    text("Between")
                        .size(theme::text_size::UI)
                        .font(ui.config.ui_font)
                        .color(theme.background),
                )
                .padding([3, 9])
                .style(move |_| container::Style {
                    background: Some(Background::Color(theme.accent)),
                    border: Border {
                        width: 0.0,
                        color: Color::TRANSPARENT,
                        radius: theme::radius::CONTROL.into(),
                    },
                    ..container::Style::default()
                }),
            );
        }
        body = body.push(segments);
    }

    // Preview line (rebase + merge): live simulation results for the current
    // candidate — rebase counts + predicted new conflicts, or the merge's
    // conflicted paths.
    if has_preview {
        let caption = |content: String, color: Color| -> Element<'_, Message> {
            text(content)
                .size(theme::text_size::UI)
                .font(ui.config.ui_font)
                .color(color)
                .into()
        };
        let preview: Element<'_, Message> = match &draft_ui.preview {
            // Branch mode's fork-point roots depend on the destination, so
            // there's nothing to resolve until a candidate exists — say so.
            crate::DraftPreviewState::Idle => caption(
                if matches!(
                    draft.kind,
                    DraftKind::Rebase {
                        mode: diffui_core::RebaseSourceMode::Branch,
                    }
                ) {
                    "hover or ↑↓ a destination to resolve the branch".to_owned()
                } else {
                    "hover or ↑↓ to preview the result".to_owned()
                },
                theme.subtle_text,
            ),
            crate::DraftPreviewState::Loading => {
                caption("simulating…".to_owned(), theme.subtle_text)
            }
            crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Rebase(preview)) => {
                // Branch mode: name the resolved fork-point root(s) — the
                // picked revision is just a member; this is the branch that
                // actually moves (the washed rows in the list).
                let branch_mode = matches!(
                    draft.kind,
                    DraftKind::Rebase {
                        mode: diffui_core::RebaseSourceMode::Branch,
                    }
                );
                // The candidate sits inside the branch itself (a descendant
                // of the source): the branch has nothing outside it to move.
                if branch_mode && preview.simulated && preview.moved == 0 {
                    caption(
                        "nothing to move — this destination already contains the branch".to_owned(),
                        theme.muted_text,
                    )
                } else {
                    let mut summary = if branch_mode && !preview.entry_points.is_empty() {
                        format!(
                            "will move the branch from {} · {} revision{}",
                            preview.entry_points.join(", "),
                            preview.moved,
                            if preview.moved == 1 { "" } else { "s" },
                        )
                    } else {
                        format!("will move {}", preview.moved)
                    };
                    if preview.descendants > 0 {
                        summary.push_str(&format!(" + {} descendant", preview.descendants));
                        if preview.descendants > 1 {
                            summary.push('s');
                        }
                    }
                    if preview.abandoned_empty > 0 {
                        summary.push_str(&format!(" · {} emptied", preview.abandoned_empty));
                    }
                    if !preview.simulated {
                        summary.push_str(" · too large to simulate conflicts");
                        caption(summary, theme.muted_text)
                    } else if preview.new_conflicts.is_empty() {
                        summary.push_str(" · no new conflicts");
                        caption(summary, theme.added_text)
                    } else {
                        summary.push_str(&format!(
                            " · conflicts in {}",
                            preview.new_conflicts.join(", ")
                        ));
                        caption(summary, theme.conflict_marker)
                    }
                }
            }
            crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Merge(preview)) => {
                if preview.conflicts.is_empty() {
                    caption("clean merge · no conflicts".to_owned(), theme.added_text)
                } else {
                    let mut summary = format!("will conflict in {}", preview.conflicts.join(", "));
                    if preview.truncated {
                        summary.push_str(", …");
                    }
                    caption(summary, theme.conflict_marker)
                }
            }
            crate::DraftPreviewState::Failed(error) => {
                caption(format!("preview failed: {error}"), theme.removed_text)
            }
        };
        body = body.push(preview);
    }

    // Key hints, pinned as the bar's own last row so a widening segments row
    // (the transient "Between" chip) can't wrap or reflow them.
    body = body.push(
        text(match draft.kind {
            DraftKind::Rebase { .. } => {
                "↑↓ target · o/a/b place · space/⌘click toggles a source · ↵ apply · esc cancels"
            }
            DraftKind::Merge => {
                "click / ↵ = final parent · space/⌘click toggles a parent · esc cancels"
            }
            DraftKind::Squash => {
                "click / ↑↓ + ↵ = fold into · space/⌘click toggles a source · esc cancels"
            }
        })
        .size(theme::text_size::CAPTION)
        .font(ui.config.ui_font)
        .color(theme.subtle_text),
    );

    // Hairline on top so the bar reads as its own surface pinned above the
    // footer. The card itself sits on the *elevated* surface — the accent
    // mood now lives in the whole sidebar's target-mode wash, so the op bar
    // pops as the neutral control panel on top of it.
    let hairline = container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.border)),
            ..container::Style::default()
        });
    column![
        hairline,
        container(body)
            .width(Length::Fill)
            .padding([10, 12])
            .style(move |_| container::Style {
                background: Some(Background::Color(theme.panel_background_elevated)),
                ..container::Style::default()
            }),
    ]
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
    crate::field::sidebar_filter_field(
        theme,
        ui.config.mono_font,
        crate::field::FilterField {
            id: REVSET_INPUT_ID,
            placeholder,
            value: &ui.session.revset,
            on_input: Message::RevsetChanged,
            on_submit: Some(Message::RevsetSubmit),
            caret: Some(crate::field::FilterCaret {
                hovered: ui.hovered == Some(HoverTarget::RevsetCaret),
                target: HoverTarget::RevsetCaret,
                menu: ToolbarMenu::RevsetPresets,
            }),
        },
    )
}

const FOOTER_TEXT_SIZE: f32 = theme::text_size::UI;

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
                    .padding([3, 9])
                    .style(move |_| theme::tooltip_style(theme)),
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
        (Rc::new(Vec::new()), 0.0, 0.0, 0.0)
    };

    let file_count = tree_rows.len();
    // The widget wants the flat index where the file block *starts*: right
    // after the expanded commit's own row.
    let expanded = expanded_index
        .filter(|_| file_count > 0)
        .map(|index| (index + 1, file_count));

    // Flat sidebar row of the selected file, for the keyboard-nav reveal: the
    // expanded commit's row, then its file rows in tree-display order. `None`
    // when the file list is closed or the file isn't currently shown, which
    // tells the widget to schedule no scroll.
    let reveal_file_flat = expanded.and_then(|(files_start, _)| {
        tree_rows
            .iter()
            .position(|row| {
                matches!(row, FileTreeRow::File { file_index, .. } if *file_index == ui.selected_file)
            })
            .map(|display| files_start + display)
    });
    // Target mode rides the same explicit-row reveal: candidate moves bump
    // the file-reveal token, and the target row wins over the file row while
    // a draft is active.
    let reveal_file_flat = ui
        .op_draft
        .as_ref()
        .and_then(|draft| draft.draft.candidate)
        .map(|candidate| match expanded {
            Some((files_start, files)) if candidate >= files_start => candidate + files,
            _ => candidate,
        })
        .or(reveal_file_flat);

    // The per-row lane fold + prefix lengths are precomputed once and held in
    // `Diffui`; the closures below build a single visible row's view from them
    // on demand, so the widget never materializes all ~N rows.
    let graph = &ui.session.graph;
    let prefix_lens = &ui.session.sidebar_prefix_lens;
    let commits = &ui.session.commits;
    let selected = &ui.session.selected_revision;
    let file_list_expanded = ui.file_list_expanded;
    let config = ui.config;
    let draft = ui.op_draft.as_ref();
    let build_revision = Box::new(move |index: usize| {
        build_revision_row(
            commits,
            graph,
            prefix_lens,
            theme,
            config,
            &graph_style,
            selected,
            file_list_expanded,
            draft,
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

    // The widget fills its own background, so the target-mode wash has to
    // reach it too — a tinted container alone would be painted over.
    let mut list_style = revision_list_style(theme, ui.config, file_badge_width);
    if ui.op_draft.is_some() {
        list_style.background = draft_panel_background(theme);
    }
    let mut list = RevisionList::new(
        ui.session.commits.len(),
        expanded,
        build_revision,
        build_file,
        selected_row,
        Some(ui.selected_file),
        ui.session.selected_commit_index,
        list_style,
        Message::SelectRowKey,
        Message::SidebarFileRow,
    )
    .width(Length::Fill)
    .reveal_selected(ui.revision_reveal_token)
    .reveal_file(ui.sidebar_file_reveal_token, reveal_file_flat)
    .on_scroll(Message::SidebarScrolled)
    .restore_scroll(ui.sidebar_scroll_offset, ui.scroll_restore_token)
    .on_context_menu(Message::RevisionContextMenu)
    .on_file_context_menu(Message::SidebarFileContextMenu);
    // Drag-to-rebase, for mutable (local jj) repos only.
    if ui
        .session
        .repository
        .as_ref()
        .is_some_and(|repo| matches!(repo.vcs, Vcs::Jj))
    {
        list = list
            .on_drag(revision_list::DragHooks {
                start: Message::RevisionDragStart,
                hover: Message::RevisionDragHover,
                drop: Message::RevisionDragDrop,
            })
            .gap_edges(Box::new(|index| {
                ui.session.commits.row(index).next_row_is_parent()
            }));
        if ui.op_draft.is_some() {
            list = list.on_target_hover(Message::DraftHoverCandidate);
        }
    }
    list.into()
}

/// Build the display view for one revision row from the precomputed fold +
/// store. Called only for on-screen rows.
#[allow(clippy::too_many_arguments)]
fn build_revision_row(
    commits: &CommitStore,
    graph: &GraphLayout,
    prefix_lens: &[usize],
    theme: ThemeSpec,
    config: AppConfig,
    graph_style: &RevisionGraphStyle,
    selected: &RevisionSelection,
    file_list_expanded: bool,
    draft: Option<&crate::DraftUi>,
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
    let bookmark_chips = bookmark_chips_for(bookmarks, lane_color, theme, config);
    let status_chips = status_chips_for(commit, theme, config);

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

    // Target-mode decorations: the draft's sources wear the lifted-off wash —
    // and so does every row the last simulation resolved into the moved set,
    // so a branch-mode draft shows the whole branch that would move. The
    // keyboard candidate wears its destination marker (the widget draws live
    // *drag* indicators itself, so they're suppressed here mid-drag).
    let draft_source = draft.is_some_and(|ui| {
        ui.draft.is_source(commit.commit_id()) || ui.moved_highlight.contains(commit.commit_id())
    });
    let draft_marker = draft.and_then(|ui| {
        use diffui_core::{DraftKind, PlacementKind};
        use revision_list::DraftMarker;
        if ui.hover_spot.is_some() {
            return None;
        }
        // Hover can arm any row as candidate, including a source (keyboard
        // nav skips them) — an invalid target gets the red op-bar hint, not
        // a destination decoration.
        if !ui.draft.target_valid(commit.commit_id()) {
            return None;
        }
        let candidate = ui.draft.candidate?;
        (candidate == index).then_some(match ui.draft.kind {
            DraftKind::Squash | DraftKind::Merge => DraftMarker::Ring,
            DraftKind::Rebase { .. } => match ui.draft.placement {
                PlacementKind::Onto => DraftMarker::Ring,
                // After = between the target and its children ⇒ lands just
                // above the row; Before ⇒ just below it.
                PlacementKind::After => DraftMarker::LineAbove,
                PlacementKind::Before => DraftMarker::LineBelow,
            },
        })
    });

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
        draft_source,
        draft_marker,
    }
}

/// Bookmark chips for a revision row, wearing `lane_color`. Shared with the
/// source browser's revision-header row so both rails style identically.
///
/// Core authors the label shapes and jj forbids `@` inside bookmark and
/// remote names, so a trailing `@` can only be another workspace's working
/// copy (`name@`) and an interior `@` a remote bookmark (`main@origin`).
/// Workspace chips get a folder glyph (another working-copy *directory*) and
/// the working-copy accent — the lane palette is deliberately decoupled from
/// it — so they read as a different kind of thing than the bookmark pills
/// sharing the rail. Remotes render outlined (transparent fill + 1px
/// lane-color border) so they read as "tracking" rather than "live".
pub(crate) fn bookmark_chips_for(
    bookmarks: &[String],
    lane_color: Color,
    theme: ThemeSpec,
    config: AppConfig,
) -> Vec<Chip> {
    let mut chips = Vec::with_capacity(bookmarks.len());
    for bookmark in bookmarks {
        let is_workspace = bookmark.ends_with('@');
        let is_remote = !is_workspace && bookmark.contains('@');
        let chip_color = if is_workspace {
            theme.accent
        } else {
            lane_color
        };
        chips.push(Chip {
            label: bookmark.clone(),
            font: config.ui_font,
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
    chips
}

/// Status chips (`empty`, `conflict`, `hidden`/`divergent`) for a commit row.
/// Shared with the source browser's revision-header row.
pub(crate) fn status_chips_for(commit: RowView, theme: ThemeSpec, config: AppConfig) -> Vec<Chip> {
    let mut status_chips = Vec::new();
    if commit.is_empty() == Some(true) {
        status_chips.push(Chip {
            label: "empty".to_owned(),
            font: config.mono_font,
            background: Color::TRANSPARENT,
            text_color: theme.subtle_text,
            border_color: Some(theme.subtle_text),
            border_dashed: true,
            icon: None,
        });
    }
    if commit.has_conflict() {
        status_chips.push(Chip {
            label: "conflict".to_owned(),
            font: config.mono_font,
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
        status_chips.push(Chip {
            label: "hidden".to_owned(),
            font: config.mono_font,
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
        status_chips.push(Chip {
            label: "divergent".to_owned(),
            font: config.mono_font,
            background: chip_background(theme.modified_token),
            text_color: theme.modified_token,
            border_color: None,
            border_dashed: false,
            icon: None,
        });
    }
    status_chips
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
        icon_color: theme.subtle_text,
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
    let max_additions = files.iter().map(|file| file.additions).max().unwrap_or(0);
    let max_deletions = files.iter().map(|file| file.deletions).max().unwrap_or(0);
    FileStatWidths {
        additions: file_stat_width(&format!("+{max_additions}"), config.ui_font),
        deletions: file_stat_width(&format!("-{max_deletions}"), config.ui_font),
        badge: file_badge_width(files, config.mono_font),
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

pub(crate) fn revision_list_style(
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

pub(crate) fn revision_id_display_len(unique_len: usize, revision_id: &str) -> usize {
    REVISION_ID_CHARS
        .max(unique_len)
        .min(revision_id.chars().count())
}

pub(crate) fn truncate_end(value: &str, max_chars: usize) -> String {
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

fn file_stat_width(text: &str, ui_font: Font) -> f32 {
    (measure::line_width(text, CAPTION_TEXT_SIZE, ui_font) + FILE_STAT_HORIZONTAL_PADDING * 2.0)
        .max(FILE_STAT_MIN_WIDTH)
}

/// Width of the status badge column ("M", "A", "D", "R", …). A diff has only
/// a handful of distinct status labels, so we shape each distinct one once
/// rather than re-shaping every file's (the scan itself stays O(files), but
/// the expensive `cosmic_text` measure runs at most a few times).
fn file_badge_width(files: &[DiffFile], mono_font: iced::Font) -> f32 {
    let mut seen: Vec<&str> = Vec::new();
    let mut widest = 0.0_f32;
    for file in files {
        let label = file.status.short_label();
        if !seen.contains(&label) {
            seen.push(label);
            widest = widest.max(chip::width(label, None, mono_font));
        }
    }
    widest
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
            parent_ids: Vec::new(),
        }
    }

    fn store(commits: Vec<CommitSummary>) -> CommitStore {
        let mut builder = CommitStoreBuilder::with_capacity(commits.len());
        for commit in commits {
            builder.push(commit);
        }
        builder.finish()
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
}
