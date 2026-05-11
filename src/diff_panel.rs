use iced::{
    Element, Length,
    widget::{container, text},
};

use crate::backend::{RevisionDetails, SignatureInfo};
use crate::diff_view::{self, DiffFileView, DiffView};
use crate::theme::{ThemeSpec, diff_palette, diff_panel_style};
use crate::{Diffui, LoadStatus, Message};

const CODE_TEXT_SIZE: f32 = 13.0;
const EMPTY_STATE_TEXT_SIZE: f32 = 15.0;

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

            let header_lines = ui
                .revision_details
                .as_ref()
                .map(build_header_lines)
                .unwrap_or_default();

            DiffView::new(
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
            .into()
        };

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

/// Format a `RevisionDetails` value into the line-by-line layout the diff
/// view renders at the top of its scroll area. Mirrors `jj show`'s
/// formatting: labels padded to 9 chars, blank line between the metadata
/// block and the indented description.
fn build_header_lines(details: &RevisionDetails) -> Vec<diff_view::HeaderLine> {
    use diff_view::HeaderLine;
    let mut lines: Vec<HeaderLine> = Vec::new();

    lines.push(HeaderLine::field("Commit ID", &details.commit_id));
    if let Some(change_id) = &details.change_id {
        lines.push(HeaderLine::field("Change ID", change_id));
    }
    if !details.bookmarks.is_empty() {
        lines.push(HeaderLine::field("Bookmarks", &details.bookmarks.join(" ")));
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
