use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::{
    Background, Border, Color, Element, Event, Length, Padding, Rectangle, Size, Theme, Vector,
    alignment,
    widget::{Space, button, column, container, row, stack, text, text_editor},
};

use crate::chip::Chip;
use crate::diff_view::{self, DiffFileView, DiffView};
use crate::find;
use crate::theme::{
    ThemeSpec, chip_background, diff_palette, diff_panel_style, file_status_color,
    primary_button_style, raised_button_style, text_size,
};
use crate::{Diffui, LoadStatus, Message};
use diffui_core::{RevisionDetails, SignatureInfo};

const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
const STATS_TEXT_SIZE: f32 = text_size::BODY;
pub(crate) const DESCRIPTION_EDITOR_ID: &str = "revision-description-editor";
const DESCRIPTION_EDITOR_ACTIONS_HEIGHT: f32 = 28.0;
const DESCRIPTION_EDITOR_GAP: f32 = 8.0;
const DESCRIPTION_EDITOR_PADDING_Y: f32 = 10.0;

pub fn build_diff_panel<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let body: Element<'a, Message> = if matches!(ui.session.status, LoadStatus::Loading)
        && ui.session.document.files.is_empty()
    {
        container(text(""))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    } else if ui.session.document.files.is_empty() && ui.session.revision_details.is_none() {
        let message = match &ui.session.status {
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
            .session
            .document
            .files
            .iter()
            .map(|file| {
                let status_color = file_status_color(file.status, theme);
                DiffFileView {
                    title: match &file.old_path {
                        Some(old_path) if old_path != &file.path => {
                            format!("{old_path} -> {}", file.path)
                        }
                        _ => file.path.clone(),
                    },
                    status: file.status,
                    status_color,
                    status_fill: chip_background(status_color),
                    hunks: &file.hunks,
                    additions: file.additions,
                    deletions: file.deletions,
                }
            })
            .collect::<Vec<_>>();

        let bookmark_color = selected_lane_color(ui, theme);
        let editable_description = ui
            .session
            .repository
            .as_ref()
            .is_some_and(|repo| matches!(repo.vcs, crate::repository::Vcs::Jj));
        let editing_description = ui
            .description_editor
            .as_ref()
            .is_some_and(|editor| editor.target == ui.session.selected_revision);
        let description_editor_height = if editing_description {
            description_editor_height(ui)
        } else {
            0.0
        };
        let header_lines = ui
            .session
            .revision_details
            .as_ref()
            .map(|details| {
                build_header_lines(
                    details,
                    bookmark_color,
                    ui.config.ui_font,
                    editable_description,
                    editing_description,
                    description_editor_height,
                )
            })
            .unwrap_or_default();

        let stats_bar = build_stats_bar(ui, theme);

        let mut dv = DiffView::new(
            files,
            ui.selected_file,
            ui.session.selected_revision.view_key(),
            diff_palette(theme),
            ui.config.mono_font,
            ui.config.code_type,
            ui.config.multi_click_ms,
            Message::SelectFile,
        )
        .with_header(header_lines)
        .on_copy(Message::CopyToClipboard)
        .on_scroll(Message::DiffScrolled)
        .restore_scroll(ui.diff_scroll_offset, ui.scroll_restore_token)
        .content_version(ui.document_version)
        .layout_version(ui.session.document_id)
        .wrap(ui.diff_wrap)
        .side_by_side(ui.diff_split);

        if editable_description && !editing_description {
            dv = dv.on_edit_description(|| Message::DescriptionEdit);
        }

        // Per-file "browse source" affordance — repo tabs only (a PR tab has
        // no local tree to browse).
        if ui.session.repository.is_some() {
            dv = dv.on_browse_file(Message::BrowseFileFromDiff);
        }

        if let Some(find_state) = &ui.find {
            dv = dv.with_find(diff_view::FindOverlay {
                matches: &find_state.matches,
                active: find_state.active,
                scroll_token: find_state.scroll_token,
                // All matches wear a soft wash; the *active* one is the
                // strong fill (the previous full-opacity inactive /
                // translucent active read backwards and drowned the text).
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

        let diff_view: Element<'a, Message> = dv.into();

        // The find bar sits on top of the diff view, pinned to the
        // upper-right of the panel. `stack` overlays without taking
        // the diff view out of the column flow.
        let find_overlay = find::build_overlay(ui, theme);
        let description_editor = build_description_editor(ui, theme, description_editor_height);
        let diff_with_find: Element<'a, Message> =
            stack![diff_view, description_editor, find_overlay]
                .clip(true)
                .into();

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

fn build_description_editor<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    block_height: f32,
) -> Element<'a, Message> {
    if let Some(editor) = ui.description_editor.as_ref()
        && editor.target == ui.session.selected_revision
    {
        let saving = editor.saving_activity.is_some();
        let input_height =
            block_height - DESCRIPTION_EDITOR_ACTIONS_HEIGHT - DESCRIPTION_EDITOR_GAP;
        let input_padding = Padding::from([DESCRIPTION_EDITOR_PADDING_Y, 12.0]);
        let mut input = text_editor(&editor.content)
            .id(iced::widget::Id::new(DESCRIPTION_EDITOR_ID))
            .placeholder("Describe this revision…")
            .size(ui.config.code_type.size)
            .font(ui.config.mono_font)
            .line_height(text::LineHeight::Relative(
                crate::measure::LINE_HEIGHT_MULTIPLIER,
            ))
            .height(Length::Fixed(input_height))
            .padding(input_padding)
            .wrapping(text::Wrapping::Glyph)
            .style(move |_, _| text_editor::Style {
                background: Background::Color(theme.background),
                border: Border {
                    width: 1.0,
                    color: theme.border,
                    radius: crate::theme::radius::CONTROL.into(),
                },
                placeholder: theme.subtle_text,
                value: theme.text,
                selection: Color {
                    a: 0.28,
                    ..theme.accent
                },
            });
        if !saving {
            input = input.on_action(Message::DescriptionAction);
        }
        // Wrapped so double/triple-click drags extend by word/line. Bare while
        // saving — the editor drops actions then, and the wrapper must too.
        let input: Element<'_, Message> = if saving {
            input.into()
        } else {
            crate::editor_drag::editor_drag_area(input, input_padding, Message::DescriptionAction)
                .into()
        };

        let cancel = button(text("Cancel").size(text_size::UI).font(ui.config.ui_font))
            .padding(Padding::from([6, 12]))
            .on_press_maybe((!saving).then_some(Message::DescriptionCancel))
            .style(move |_, status| raised_button_style(theme, status));

        let save_enabled = !saving && editor.is_dirty();
        let save_label = if saving { "Saving…" } else { "Save" };
        let save = button(text(save_label).size(text_size::UI).font(ui.config.ui_font))
            .padding(Padding::from([6, 14]))
            .on_press_maybe(save_enabled.then_some(Message::DescriptionSave))
            .style(move |_, _| primary_button_style(theme));

        let hint = if editor.switch_blocked {
            "save or cancel before switching revisions"
        } else {
            "⌘↵ save · esc cancel"
        };
        let hint_color = if editor.switch_blocked {
            theme.note_text
        } else {
            theme.subtle_text
        };
        let actions = row![
            text(hint)
                .size(text_size::CAPTION)
                .font(ui.config.mono_font)
                .color(hint_color),
            Space::new().width(Length::Fill),
            cancel,
            save,
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center);

        let body = container(column![input, actions].spacing(8))
            .width(Length::Fill)
            .height(Length::Fixed(block_height));
        let positioned = container(body)
            .width(Length::Fill)
            .height(Length::Fixed(
                diff_view::HEADER_VERTICAL_PADDING + block_height,
            ))
            .padding(Padding {
                top: diff_view::HEADER_VERTICAL_PADDING,
                right: diff_view::HEADER_HORIZONTAL_PADDING,
                bottom: 0.0,
                left: diff_view::HEADER_HORIZONTAL_PADDING,
            });
        let offset = ui.diff_scroll_offset;
        return ScrollTranslate::new(positioned.into(), Vector::new(0.0, -offset)).into();
    }
    Space::new().height(0).into()
}

fn description_editor_height(ui: &Diffui) -> f32 {
    let Some(editor) = ui.description_editor.as_ref() else {
        return 0.0;
    };
    let available_width = (ui.window_size.width
        - ui.sidebar_width
        - 1.0
        - diff_view::HEADER_HORIZONTAL_PADDING * 2.0
        - 24.0)
        .max(1.0);
    let char_width =
        crate::measure::line_width("M", ui.config.code_type.size, ui.config.mono_font).max(1.0);
    let chars_per_line = (available_width / char_width).floor().max(1.0) as usize;
    let visual_lines = description_visual_line_count(&editor.text(), chars_per_line);
    let input_height =
        (visual_lines as f32 * ui.config.code_type.size * crate::measure::LINE_HEIGHT_MULTIPLIER)
            .ceil()
            + DESCRIPTION_EDITOR_PADDING_Y * 2.0;
    input_height + DESCRIPTION_EDITOR_GAP + DESCRIPTION_EDITOR_ACTIONS_HEIGHT
}

fn description_visual_line_count(content: &str, chars_per_line: usize) -> usize {
    content
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(chars_per_line.max(1)))
        .sum()
}

/// Moves the editor with the custom diff view's scroll offset while keeping it
/// in the stack's normal clipped layer. `float` cannot be used here: iced
/// promotes translated floats to a window-level overlay, letting the editor
/// paint over the toolbar and tab strip after it scrolls above the diff pane.
struct ScrollTranslate<'a> {
    content: Element<'a, Message>,
    translation: Vector,
}

impl<'a> ScrollTranslate<'a> {
    fn new(content: Element<'a, Message>, translation: Vector) -> Self {
        Self {
            content,
            translation,
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for ScrollTranslate<'_> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(tree, renderer, limits)
            .translate(self.translation)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget_mut()
            .update(tree, event, layout, cursor, renderer, shell, viewport);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }
}

impl<'a> From<ScrollTranslate<'a>> for Element<'a, Message> {
    fn from(translated: ScrollTranslate<'a>) -> Self {
        Element::new(translated)
    }
}

/// Compact stats line shown above the diff scroll area: saturated
/// `+N` / `−M` glyphs followed by a quiet `· N files` tail. Mirrors the
/// pattern the sidebar header used to carry — moved here so the sidebar
/// stays focused on the revision list and the totals sit next to the
/// content they describe.
fn build_stats_bar<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    // Prefer source-reported totals (a PR's header counts) over the summed
    // files — the GitHub files API zeroes counts on oversized blobs, so the
    // sum can undercount what the PR page shows.
    let (additions, deletions) = ui.session.authoritative_totals.unwrap_or((
        ui.session.document.total_additions,
        ui.session.document.total_deletions,
    ));
    let bar = row![
        text(format!("+{additions}"))
            .size(STATS_TEXT_SIZE)
            .font(ui.config.mono_font)
            .color(theme.added_text),
        text(format!("−{deletions}"))
            .size(STATS_TEXT_SIZE)
            .font(ui.config.mono_font)
            .color(theme.removed_text),
        text(format!(
            "· {}",
            format_file_count(ui.session.document.files.len())
        ))
        .size(STATS_TEXT_SIZE)
        .color(theme.subtle_text),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    container(bar)
        .width(Length::Fill)
        .padding([9, 16])
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(theme.panel_background)),
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
    ui.session
        .selected_commit_index
        .filter(|&index| index < ui.session.commits.len())
        .map(|index| style.lane_color(ui.session.graph.frame(index, usize::MAX).node_lane))
        .unwrap_or(theme.accent)
}

/// Format a `RevisionDetails` value into the line-by-line layout the diff
/// view renders at the top of its scroll area. The description leads as the
/// header's title, followed by a compact metadata block — lowercase muted
/// labels padded to a column, change id first (it's the primary handle in a
/// jj-first tool; the sidebar leads with it too).
fn build_header_lines(
    details: &RevisionDetails,
    bookmark_color: Color,
    bookmark_font: iced::Font,
    editable_description: bool,
    editing_description: bool,
    description_editor_height: f32,
) -> Vec<diff_view::HeaderLine> {
    use diff_view::HeaderLine;
    let mut lines: Vec<HeaderLine> = Vec::new();

    if editing_description {
        lines.push(HeaderLine::spacer(description_editor_height));
    } else if !details.description.is_empty() {
        for line in details.description.lines() {
            lines.push(HeaderLine::description(line));
        }
        lines.push(HeaderLine::blank());
    } else if editable_description {
        lines.push(HeaderLine::description("(no description)"));
        lines.push(HeaderLine::blank());
    }

    if let Some(change_id) = &details.change_id {
        lines.push(HeaderLine::field("change", change_id));
    }
    lines.push(HeaderLine::field("commit", &details.commit_id));
    if !details.bookmarks.is_empty() {
        // Chips match the sidebar: a tint of the commit's lane color for local
        // bookmarks, outlined for remote (`name@remote`) ones.
        let chips = details
            .bookmarks
            .iter()
            .map(|bookmark| {
                let is_remote = bookmark.contains('@');
                Chip {
                    label: bookmark.clone(),
                    font: bookmark_font,
                    background: if is_remote {
                        Color::TRANSPARENT
                    } else {
                        chip_background(bookmark_color)
                    },
                    text_color: bookmark_color,
                    border_color: is_remote.then_some(bookmark_color),
                    border_dashed: false,
                    icon: None,
                }
            })
            .collect();
        lines.push(HeaderLine::bookmarks("bookmarks", chips));
    }
    lines.push(HeaderLine::field(
        "author",
        &format_signature_line(&details.author),
    ));
    if let Some(committer) = &details.committer {
        lines.push(HeaderLine::field(
            "committer",
            &format_signature_line(committer),
        ));
    }
    if let Some(sig) = &details.signature {
        lines.push(HeaderLine::field("signature", sig));
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

#[cfg(test)]
mod tests {
    use super::description_visual_line_count;

    #[test]
    fn description_height_counts_newlines_wrapping_and_contraction() {
        assert_eq!(description_visual_line_count("", 10), 1);
        assert_eq!(description_visual_line_count("one\ntwo\n", 10), 3);
        assert_eq!(description_visual_line_count("12345678901", 10), 2);
        assert_eq!(description_visual_line_count("short", 10), 1);
    }
}
