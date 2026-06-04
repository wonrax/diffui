use iced::{
    Color, Element, Length, alignment,
    widget::{column, container, row, stack, text},
};

use crate::backend::{RevisionDetails, SignatureInfo};
use crate::diff_view::{self, DiffFileView, DiffView};
use crate::find;
use crate::theme::{ThemeSpec, chip_background, diff_palette, diff_panel_style};
use crate::{Diffui, LoadStatus, Message};

const CODE_TEXT_SIZE: f32 = 12.0;
const EMPTY_STATE_TEXT_SIZE: f32 = 14.0;
const STATS_TEXT_SIZE: f32 = 13.0;

pub fn build_diff_panel<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let body: Element<'a, Message> =
        if matches!(ui.status, LoadStatus::Loading) && ui.document.files.is_empty() {
            container(text(""))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        } else if ui.document.files.is_empty() && ui.revision_details.is_none() {
            let message = match &ui.status {
                LoadStatus::Failed(_) => "Failed to load changes",
                _ => "No file changes in this revision",
            };
            container(
                text(message)
                    .size(EMPTY_STATE_TEXT_SIZE)
                    .color(theme.subtle_text),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        } else {
            let files = ui
                .document
                .files
                .iter()
                .map(|file| DiffFileView {
                    title: match &file.old_path {
                        Some(old_path) if old_path != &file.path => {
                            format!("{old_path} -> {}", file.path)
                        }
                        _ => file.path.clone(),
                    },
                    status: file.status.label(),
                    hunks: &file.hunks,
                    additions: file.additions,
                    deletions: file.deletions,
                })
                .collect::<Vec<_>>();

            let bookmark_color = selected_lane_color(ui, theme);
            let header_lines = ui
                .revision_details
                .as_ref()
                .map(|details| build_header_lines(details, bookmark_color))
                .unwrap_or_default();

            let stats_bar = build_stats_bar(ui, theme);

            let mut dv = DiffView::new(
                files,
                ui.selected_file,
                ui.selected_revision.view_key(),
                diff_palette(theme),
                ui.config.mono_font,
                CODE_TEXT_SIZE,
                ui.config.multi_click_ms,
                Message::SelectFile,
            )
            .with_header(header_lines)
            .on_copy(Message::CopyToClipboard)
            .on_scroll(Message::DiffScrolled)
            .restore_scroll(ui.diff_scroll_offset, ui.scroll_restore_token)
            .content_version(ui.document_version);

            if let Some(find_state) = &ui.find {
                dv = dv.with_find(diff_view::FindOverlay {
                    matches: &find_state.matches,
                    active: find_state.active,
                    scroll_token: find_state.scroll_token,
                    highlight: theme.accent,
                    active_highlight: Color {
                        a: 0.45,
                        ..theme.accent
                    },
                });
            }

            let diff_view: Element<'a, Message> = dv.into();

            // The find bar sits on top of the diff view, pinned to the
            // upper-right of the panel. `stack` overlays without taking
            // the diff view out of the column flow.
            let find_overlay = find::build_overlay(ui, theme);
            let diff_with_find: Element<'a, Message> = stack![diff_view, find_overlay].into();

            column![stats_bar, diff_with_find].spacing(0).into()
        };

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

/// Compact stats line shown above the diff scroll area: saturated
/// `+N` / `−M` glyphs followed by a quiet `· N files` tail. Mirrors the
/// pattern the sidebar header used to carry — moved here so the sidebar
/// stays focused on the revision list and the totals sit next to the
/// content they describe.
fn build_stats_bar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let bar = row![
        text(format!("+{}", ui.document.total_additions))
            .size(STATS_TEXT_SIZE)
            .font(ui.config.mono_font)
            .color(theme.added_text),
        text(format!("−{}", ui.document.total_deletions))
            .size(STATS_TEXT_SIZE)
            .font(ui.config.mono_font)
            .color(theme.removed_text),
        text(format!("· {}", format_file_count(ui.document.files.len())))
            .size(STATS_TEXT_SIZE)
            .color(theme.subtle_text),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    container(bar)
        .width(Length::Fill)
        .padding([8, 14])
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(theme.panel_background)),
            border: iced::Border {
                color: theme.border,
                width: 0.0,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn format_file_count(count: usize) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
}

/// The lane color of the currently-selected commit — the same color the
/// sidebar paints that commit's bookmark chips, so the diff-view chips match.
/// Falls back to the accent when there's no selected row yet.
fn selected_lane_color(ui: &Diffui, theme: ThemeSpec) -> Color {
    let style = crate::graph_view::RevisionGraphStyle {
        lane_base_color: theme.lane_base,
        missing_color: theme.subtle_text,
    };
    ui.selected_commit_index
        .filter(|&index| index < ui.commits.len())
        .map(|index| style.lane_color(ui.graph.frame(index, usize::MAX).node_lane))
        .unwrap_or(theme.accent)
}

/// Format a `RevisionDetails` value into the line-by-line layout the diff
/// view renders at the top of its scroll area. Mirrors `jj show`'s
/// formatting: labels padded to 9 chars, blank line between the metadata
/// block and the indented description.
fn build_header_lines(
    details: &RevisionDetails,
    bookmark_color: Color,
) -> Vec<diff_view::HeaderLine> {
    use diff_view::{HeaderChip, HeaderLine};
    let mut lines: Vec<HeaderLine> = Vec::new();

    lines.push(HeaderLine::field("Commit ID", &details.commit_id));
    if let Some(change_id) = &details.change_id {
        lines.push(HeaderLine::field("Change ID", change_id));
    }
    if !details.bookmarks.is_empty() {
        // Chips match the sidebar: a tint of the commit's lane color for local
        // bookmarks, outlined for remote (`name@remote`) ones.
        let chips = details
            .bookmarks
            .iter()
            .map(|bookmark| {
                let is_remote = bookmark.contains('@');
                HeaderChip {
                    label: bookmark.clone(),
                    fill: if is_remote {
                        Color::TRANSPARENT
                    } else {
                        chip_background(bookmark_color)
                    },
                    text: bookmark_color,
                    border: is_remote.then_some(bookmark_color),
                }
            })
            .collect();
        lines.push(HeaderLine::bookmarks("Bookmarks", chips));
    }
    lines.push(HeaderLine::field(
        "Author",
        &format_signature_line(&details.author),
    ));
    if let Some(committer) = &details.committer {
        lines.push(HeaderLine::field(
            "Committer",
            &format_signature_line(committer),
        ));
    }
    if let Some(sig) = &details.signature {
        lines.push(HeaderLine::field("Signature", sig));
    }

    if !details.description.is_empty() {
        lines.push(HeaderLine::blank());
        for line in details.description.lines() {
            lines.push(HeaderLine::description(line));
        }
    }

    lines
}

fn format_signature_line(sig: &SignatureInfo) -> String {
    let mut parts = String::new();
    if !sig.name.is_empty() {
        parts.push_str(&sig.name);
    }
    if !sig.email.is_empty() {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push('<');
        parts.push_str(&sig.email);
        parts.push('>');
    }
    if let Some(ts) = &sig.timestamp
        && !ts.is_empty()
    {
        parts.push_str(" (");
        parts.push_str(ts);
        parts.push(')');
    }
    parts
}
