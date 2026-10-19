use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    widget::{Space, column, container, mouse_area, row, stack, text, text_editor},
};

use crate::diff_view::{self, DiffFileView, DiffView, HeaderLine};
use crate::find;
use crate::icons;
use crate::revision_list::{FileRowView, RevisionList};
use crate::theme::{
    ThemeSpec, centered_control_content, chip_background, diff_palette, diff_panel_style,
    file_status_color, text_size,
};
use crate::ui;
use crate::{Action, Diffui, LoadStatus, Message, UiEvent};
use diffui_core::{FileTreeRow, SignatureInfo};
use jj_lib::graph::GraphEdgeType;
use std::rc::Rc;

const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
pub(crate) const DESCRIPTION_EDITOR_ID: &str = "revision-description-editor";
const DESCRIPTION_EDITOR_GAP: f32 = 8.0;
const DESCRIPTION_EDITOR_PADDING_Y: f32 = 10.0;
pub const DEFAULT_FILE_NAV_WIDTH: f32 = 236.0;
pub const MIN_FILE_NAV_WIDTH: f32 = 168.0;
pub const FILE_NAV_PEEK_WIDTH: f32 = 284.0;

/// Stable changed-file navigation beside the review. Keeping this separate
/// from the revision graph means browsing a change never reflows history.
pub fn build_file_navigator<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    build_file_navigator_surface(ui, theme, ui.file_nav_width, true)
}

/// The same navigator used by the permanent panel, sized as a floating quick
/// picker when the panel is collapsed. Keeping one tree builder preserves file
/// selection, directory folding, scrolling, and the file context menu.
pub fn build_file_navigator_peek<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let tree = build_file_navigator_surface(ui, theme, FILE_NAV_PEEK_WIDTH, false);
    mouse_area(
        container(tree)
            .width(Length::Fixed(FILE_NAV_PEEK_WIDTH))
            .height(Length::Fill)
            .style(move |_| crate::theme::popover_style(theme)),
    )
    .on_enter(Message::Ui(UiEvent::ChangedFilesPopupEntered))
    .on_exit(Message::Ui(UiEvent::ChangedFilesPopupExited))
    .into()
}

fn build_file_navigator_surface<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    width: f32,
    show_collapse: bool,
) -> Element<'a, Message> {
    let files = &ui.active().session.document.files;
    if files.is_empty() {
        return Space::new().width(0).into();
    }

    let mut trailing = row![
        text(files.len().to_string())
            .size(text_size::CAPTION)
            .font(ui.config.mono_font)
            .color(theme.subtle_text)
    ]
    .spacing(crate::field::PANEL_TOGGLE_INSET)
    .align_y(alignment::Vertical::Center);
    if show_collapse {
        trailing = trailing.push(crate::field::panel_toggle_button(
            ui.config.ui_font,
            theme,
            icons::MINUS,
            "Hide changed files",
            Action::ToggleFilesPanel,
        ));
    }
    let header = row![
        text("Changed files")
            .size(text_size::UI)
            .font(crate::theme::emphasis_font(
                ui.config.ui_font,
                iced::font::Weight::Medium,
            ))
            .color(theme.subtle_text),
        Space::new().width(Length::Fill),
        trailing,
    ]
    // Toggle-row height even without the toggle (the peek), so the label
    // sits on the same line as the other panels' headers.
    .height(Length::Fixed(crate::theme::control::COMPACT))
    .align_y(alignment::Vertical::Center);

    let tree_rows = ui.sidebar_file_cache.borrow_mut().tree_rows(
        ui.active().session.document_id,
        files.len(),
        &ui.active().collapsed_dirs,
        files,
    );
    let row_count = tree_rows.len();
    let selected_display = tree_rows.iter().position(|row| {
        matches!(row, FileTreeRow::File { file_index, .. } if *file_index == ui.active().selected_file)
    });
    let max_additions = files.iter().map(|file| file.additions).max().unwrap_or(0);
    let max_deletions = files.iter().map(|file| file.deletions).max().unwrap_or(0);
    let additions_width = changed_file_stat_width('+', max_additions, ui.config.ui_font);
    let deletions_width = changed_file_stat_width('\u{2212}', max_deletions, ui.config.ui_font);
    let badge_width = crate::chip::width("M", None, ui.config.mono_font);
    let rows_for_files = Rc::clone(&tree_rows);
    let lanes = EmptyFileTreeLanes::default();
    let build_file = Box::new(move |display: usize| {
        let row = &rows_for_files[display];
        let mut view =
            changed_file_row(row, files, theme, additions_width, deletions_width, &lanes);
        view.check = match row {
            FileTreeRow::File { file_index, .. } => {
                crate::changes::tree_check(ui, Some(*file_index), None)
            }
            FileTreeRow::Dir { path, .. } => crate::changes::tree_check(
                ui,
                None,
                Some(&format!("{}/", path.trim_end_matches('/'))),
            ),
        };
        view
    });
    let build_revision = Box::new(move |_| crate::source_panel::empty_revision_row(theme));
    let list = RevisionList::new(
        0,
        Some((0, row_count)),
        build_revision,
        build_file,
        None,
        Some(ui.active().selected_file),
        None,
        crate::sidebar::revision_list_style(theme, ui.config, badge_width),
        |_| Message::Ui(UiEvent::SourceHeaderClicked),
        |row| Message::Ui(UiEvent::SidebarFileRow(row)),
    )
    .source_tree_slot()
    .width(Length::Fill)
    .reveal_file(ui.sidebar_file_reveal_token, selected_display)
    .on_scroll(|offset| Message::Ui(UiEvent::ChangedFilesScrolled(offset)))
    .restore_scroll(ui.active().file_tree_scroll_offset, ui.scroll_restore_token)
    .on_file_context_menu(|row, rect, point| {
        Message::Ui(UiEvent::SidebarFileContextMenu(row, rect, point))
    })
    .on_file_check(|row| Message::Changes(crate::changes::ChangesMsg::ToggleTreeRow(row)));
    container(
        column![
            container(header).padding(Padding {
                top: crate::field::PANEL_TOGGLE_INSET,
                // The toggle keeps its square inset; a bare count (the peek)
                // mirrors the label's inset instead.
                right: if show_collapse {
                    crate::field::PANEL_TOGGLE_INSET
                } else {
                    crate::theme::space::LG
                },
                bottom: crate::field::PANEL_TOGGLE_INSET,
                left: crate::theme::space::LG,
            }),
            list
        ]
        .spacing(0),
    )
    .width(Length::Fixed(width))
    .height(Length::Fill)
    .style(move |_| container::Style::default().background(theme.panel_background))
    .into()
}

/// A `+N` / `−N` count wears its add/del color only when it counts
/// something: a `+0` beside a `−5` reads as noise, not as a signal.
pub(crate) fn stat_color(count: usize, color: Color, theme: ThemeSpec) -> Color {
    if count == 0 { theme.subtle_text } else { color }
}

/// A stat column exactly as wide as its widest count. The counts are drawn
/// right-aligned in it, so the even gaps between the columns come from the
/// row's own gap, not from padding baked in here.
fn changed_file_stat_width(sign: char, value: usize, font: iced::Font) -> f32 {
    crate::measure::line_width(&format!("{sign}{value}"), text_size::CAPTION, font).ceil()
}

#[derive(Default)]
struct EmptyFileTreeLanes {
    continuation: Rc<[Option<GraphEdgeType>]>,
    columns: Rc<[Option<usize>]>,
    labels: Rc<[Vec<String>]>,
    segments: Rc<[Option<usize>]>,
}

fn changed_file_row(
    row: &FileTreeRow,
    files: &[diffui_core::DiffFile],
    theme: ThemeSpec,
    additions_width: f32,
    deletions_width: f32,
    lanes: &EmptyFileTreeLanes,
) -> FileRowView {
    let (
        primary,
        raw_path,
        indent,
        chevron,
        file_index,
        status_label,
        status_color,
        additions,
        deletions,
    ) = match row {
        FileTreeRow::Dir {
            label,
            path,
            depth,
            collapsed,
        } => (
            label.clone(),
            path.clone(),
            *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
            Some(*collapsed),
            usize::MAX,
            String::new(),
            theme.subtle_text,
            0,
            0,
        ),
        FileTreeRow::File {
            file_index,
            label,
            depth,
        } => {
            let file = &files[*file_index];
            (
                label.clone(),
                file.path.clone(),
                *depth as f32 * crate::revision_list::FILE_TREE_INDENT,
                None,
                *file_index,
                file.status.short_label().to_owned(),
                file_status_color(file.status, theme),
                file.additions,
                file.deletions,
            )
        }
    };
    FileRowView {
        primary,
        raw_path,
        status_label,
        status_background: chip_background(status_color),
        status_text: status_color,
        additions,
        deletions,
        additions_text: stat_color(additions, theme.added_text, theme),
        deletions_text: stat_color(deletions, theme.removed_text, theme),
        continuation: Rc::clone(&lanes.continuation),
        columns: Rc::clone(&lanes.columns),
        additions_width,
        deletions_width,
        primary_color: theme.text,
        icon_color: status_color,
        indent,
        chevron,
        file_index,
        lane_labels: Rc::clone(&lanes.labels),
        lane_segments: Rc::clone(&lanes.segments),
        check: None,
    }
}

pub fn build_collapsed_file_navigator(theme: ThemeSpec) -> Element<'static, Message> {
    // Only the icon opens the peek: the rail runs the full height of the
    // window, so as a trigger it would fire on every pass between the history
    // and the diff. It still holds a peek that is already open, because a
    // diagonal move from the icon to a file far down the tree spends longer
    // crossing the rail than the close delay allows.
    let trigger = mouse_area(crate::field::panel_toggle_button_bare(
        theme,
        icons::FILE_DIFF,
        Action::ToggleFilesPanel,
    ))
    .on_enter(Message::Ui(UiEvent::ChangedFilesTriggerEntered))
    .on_exit(Message::Ui(UiEvent::ChangedFilesTriggerExited));
    mouse_area(
        container(trigger)
            .width(Length::Fixed(crate::theme::COLLAPSED_PANEL_WIDTH))
            .height(Length::Fill)
            .padding(crate::field::COLLAPSED_RAIL_PADDING)
            .style(move |_| container::Style::default().background(theme.panel_background)),
    )
    .on_enter(Message::Ui(UiEvent::ChangedFilesRailEntered))
    .on_exit(Message::Ui(UiEvent::ChangedFilesRailExited))
    .into()
}

pub fn build_diff_panel<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let body = build_diff_panel_body(ui, theme);
    match crate::empty_states::outside_view_banner(ui, theme) {
        Some(banner) => column![banner, body]
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        None => body,
    }
}

fn build_diff_panel_body<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let body: Element<'a, Message> = if matches!(ui.active().session.status, LoadStatus::Loading)
        && ui.active().session.document.files.is_empty()
    {
        container(text(""))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    } else if ui.active().session.document.files.is_empty()
        && ui.active().session.revision_details.is_none()
    {
        let message = match &ui.active().session.status {
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
            .active()
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

        let editable_description = ui.active().session.capabilities.mutate;
        let summary = build_revision_summary(ui, theme);

        if files.is_empty() {
            let mut actions = row![].spacing(8).align_y(alignment::Vertical::Center);
            if ui.active().repository.is_some() {
                actions = actions.push(
                    ui::text_button(
                        Some((icons::CODE, theme.muted_text)),
                        "Browse source",
                        ui::Variant::Raised,
                        ui::Size::Standard,
                        ui.config.ui_font,
                        theme,
                    )
                    .on_press(Message::Action(Action::SetMainView(
                        crate::MainView::Source,
                    ))),
                );
            }
            // Offered only while there is no description, and not while the
            // editor above is already open for it.
            let describable = editable_description
                && ui.active().description_editor().is_none()
                && ui
                    .active()
                    .session
                    .revision_details
                    .as_ref()
                    .is_some_and(|details| details.description.trim().is_empty());
            if describable {
                actions = actions.push(
                    ui::text_button(
                        None,
                        "Add description",
                        ui::Variant::Raised,
                        ui::Size::Standard,
                        ui.config.ui_font,
                        theme,
                    )
                    .on_press(Message::Action(Action::EditDescription { target: None })),
                );
            }
            let empty = container(
                column![
                    container(centered_control_content(icons::icon(
                        icons::CHECK,
                        22.0,
                        theme.added_text,
                    )))
                    .width(Length::Fixed(42.0))
                    .height(Length::Fixed(42.0))
                    .align_x(alignment::Horizontal::Center)
                    .align_y(alignment::Vertical::Center)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(chip_background(theme.added_text))),
                        border: Border {
                            radius: crate::theme::radius::SURFACE.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                    text("This revision has no changes")
                        .size(text_size::TITLE)
                        .font(crate::theme::emphasis_font(
                            ui.config.ui_font,
                            iced::font::Weight::Medium,
                        ))
                        .color(theme.text),
                    Space::new().height(Length::Fixed(crate::theme::space::XS)),
                    actions,
                ]
                .spacing(crate::theme::space::LG)
                .align_x(alignment::Horizontal::Center)
                .width(Length::Fit.max(440)),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(crate::theme::space::XXL)
            .center(Length::Fill);
            let banner = crate::compare::build_compare_banner(ui, theme);
            return container(column![].push(banner).push(summary).push(empty))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_| diff_panel_style(theme))
                .into();
        }

        let mut dv = DiffView::new(
            files,
            ui.active().selected_file,
            ui.active().session.selected_revision.view_key()
                + &ui.active().session.document_base.view_key(),
            diff_palette(theme),
            ui.config.mono_font,
            ui.config.code_type,
            ui.config.multi_click_ms,
            |index| Message::Action(Action::SelectFile(index)),
        )
        .on_copy(|text| Message::Action(Action::Copy(text)))
        .on_scroll(|offset| Message::Ui(UiEvent::DiffScrolled(offset)))
        .restore_scroll(ui.active().diff_scroll_offset, ui.scroll_restore_token)
        .content_version(ui.document_version)
        .layout_version(ui.active().session.document_id)
        .wrap(ui.diff_wrap)
        .side_by_side(split_in_effect(ui));

        if let Some(details) = ui.active().session.revision_details.as_ref() {
            dv = dv.with_header(revision_header_lines(details));
        }

        let collapsed: Vec<bool> = ui
            .active()
            .session
            .document
            .files
            .iter()
            .map(|file| file.collapsed.is_some())
            .collect();
        if collapsed.contains(&true) {
            dv = dv.on_expand_file(collapsed, |index| {
                Message::Action(Action::ExpandFile(index))
            });
        }

        // Per-file "browse source" affordance — repo tabs only (a PR tab has
        // no local tree to browse).
        if ui.active().repository.is_some() {
            dv = dv.on_browse_file(|index| Message::Ui(UiEvent::BrowseFileFromDiff(index)));
        }

        if let Some(marks) = crate::changes::diff_marks(ui, theme) {
            dv = dv.change_marks(marks);
        }

        if let Some(find_state) = ui.active().find() {
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
        let mut layers = stack![diff_view, find_overlay].clip(true);
        if let Some(bar) = crate::changes::selection_bar(ui, theme) {
            layers = layers.push(bar);
        }
        let diff_with_find: Element<'a, Message> = layers.into();

        match crate::conflicts::conflict_banner(ui, theme) {
            Some(banner) => column![summary, banner, diff_with_find].spacing(0).into(),
            None => column![summary, diff_with_find].spacing(0).into(),
        }
    };
    let body: Element<'a, Message> = match crate::compare::build_compare_banner(ui, theme) {
        Some(banner) => column![banner, body].into(),
        None => body,
    };

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

fn build_description_editor<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    if let Some(editor) = ui.active().description_editor()
        && editor.target == ui.active().session.selected_revision
    {
        let saving = editor.saving_activity.is_some();
        let input_height = description_editor_input_height(ui);
        let input_padding = Padding::from([DESCRIPTION_EDITOR_PADDING_Y, 12.0]);
        let mut input = text_editor(&editor.content)
            .id(iced::widget::Id::new(DESCRIPTION_EDITOR_ID))
            .key_binding(crate::ui::editor_key_binding)
            .placeholder(if editor.commit {
                "Describe the change you are committing…"
            } else {
                "Describe this revision…"
            })
            .size(ui.config.code_type.size)
            .font(ui.config.mono_font)
            .line_height(text::LineHeight::Relative(
                crate::measure::LINE_HEIGHT_MULTIPLIER,
            ))
            .height(Length::Fixed(input_height))
            .padding(input_padding)
            .wrapping(text::Wrapping::WordOrGlyph)
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
            input = input.on_action(|action| Message::Ui(UiEvent::DescriptionAction(action)));
        }
        // Wrapped so double/triple-click drags extend by word/line. Bare while
        // saving — the editor drops actions then, and the wrapper must too.
        let input: Element<'_, Message> = if saving {
            input.into()
        } else {
            crate::editor_drag::editor_drag_area(input, input_padding, |action| {
                Message::Ui(UiEvent::DescriptionAction(action))
            })
            .into()
        };

        let cancel = ui::text_button(
            None,
            "Cancel",
            ui::Variant::Raised,
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
        )
        .on_press_maybe((!saving).then_some(Message::Action(Action::CancelDescription)));

        // A commit may keep the description as it stands.
        let save_enabled = !saving && (editor.is_dirty() || editor.commit);
        let save_label = match (saving, editor.commit) {
            (true, true) => "Committing…",
            (true, false) => "Saving…",
            (false, true) => "Commit",
            (false, false) => "Save",
        };
        let save = ui::text_button(
            None,
            save_label,
            ui::Variant::Primary,
            ui::Size::Compact,
            ui.config.ui_font,
            theme,
        )
        .on_press_maybe(save_enabled.then_some(Message::Action(Action::SaveDescription)));

        // The keys live in the buttons' tooltips; the strip only speaks up
        // when the editor is holding a revision switch back.
        let hint: Element<'_, Message> = if editor.switch_blocked {
            text("Save or cancel before switching revisions")
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.note_text)
                .into()
        } else if editor.commit {
            text("Then starts a new change on top")
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.subtle_text)
                .into()
        } else {
            Space::new().into()
        };
        let keyed = |button, key: &'static str| {
            ui::tooltip(
                button,
                key,
                iced::widget::tooltip::Position::Bottom,
                ui.config.ui_font,
                theme,
            )
        };
        let actions = row![
            hint,
            Space::new().width(Length::Fill),
            keyed(cancel, "Esc"),
            keyed(save, "\u{2318}\u{21b5}"),
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center);

        return container(column![input, actions].spacing(DESCRIPTION_EDITOR_GAP))
            .width(Length::Fill)
            .padding([10.0, crate::theme::space::XL])
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into();
    }
    Space::new().height(0).into()
}

/// Code columns a side-by-side half has to hold before the split is worth
/// it. Narrower than this and both halves soft-wrap mid-token on ordinary
/// lines, which reads worse than one unified column.
const MIN_SPLIT_COLUMN_CHARS: f32 = 56.0;

/// The diff pane's width: the window minus the history and file panes and
/// their dividers.
fn diff_pane_width(ui: &Diffui) -> f32 {
    let history_width = if ui.active().history_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH
    } else {
        ui.sidebar_width
    };
    let files_width = if ui.active().session.document.files.is_empty() {
        0.0
    } else if ui.active().files_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH + 1.0
    } else {
        ui.file_nav_width + 1.0
    };
    (ui.window_size.width - history_width - files_width - 1.0).max(0.0)
}

/// Whether a side-by-side diff has room for both halves. The toggle is the
/// user's preference; this only decides whether it can be honoured now, so
/// widening the window brings the split back.
pub(crate) fn split_fits(ui: &Diffui) -> bool {
    let size = ui.config.code_type.size;
    let char_width = crate::measure::line_width("0000000000", size, ui.config.mono_font) / 10.0;
    // Per half: a line-number gutter (one number, ~4 digits + padding) plus
    // the code columns.
    let half = 4.0 * char_width + 16.0 + MIN_SPLIT_COLUMN_CHARS * char_width;
    diff_pane_width(ui) >= 2.0 * half
}

/// Side-by-side is on and fits.
pub(crate) fn split_in_effect(ui: &Diffui) -> bool {
    ui.diff_split && split_fits(ui)
}

fn description_editor_input_height(ui: &Diffui) -> f32 {
    let Some(editor) = ui.active().description_editor() else {
        return 0.0;
    };
    let history_width = if ui.active().history_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH
    } else {
        ui.sidebar_width
    };
    let files_width = if ui.active().session.document.files.is_empty() {
        0.0
    } else if ui.active().files_panel_collapsed {
        crate::theme::COLLAPSED_PANEL_WIDTH + 1.0
    } else {
        ui.file_nav_width + 1.0
    };
    let available_width = (ui.window_size.width
        - history_width
        - files_width
        - 1.0
        - diff_view::HEADER_HORIZONTAL_PADDING * 2.0
        - 24.0)
        .max(1.0);
    // Sized by the same engine that lays the editor out (word-first wrap),
    // so the box grows exactly with the wrapped text instead of estimating
    // breaks from a chars-per-line division.
    let line_height = (ui.config.code_type.size * crate::measure::LINE_HEIGHT_MULTIPLIER).max(1.0);
    let visual_lines = crate::measure::wrapped_line_count(
        &editor.text(),
        ui.config.code_type.size,
        ui.config.mono_font,
        line_height,
        available_width,
    );
    let natural = (visual_lines as f32 * line_height).ceil() + DESCRIPTION_EDITOR_PADDING_Y * 2.0;
    let maximum = (ui.window_size.height * 0.35).clamp(120.0, 280.0);
    natural.clamp(88.0, maximum)
}

/// Compact stats line shown above the diff scroll area: saturated
/// `+N` / `−M` glyphs followed by a quiet `· N files` tail. Mirrors the
/// pattern the sidebar header used to carry — moved here so the sidebar
/// stays focused on the revision list and the totals sit next to the
/// content they describe.
fn build_revision_summary<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let session = &ui.active().session;
    if ui
        .active()
        .description_editor()
        .is_some_and(|editor| editor.target == session.selected_revision)
    {
        return build_description_editor(ui, theme);
    }
    let (additions, deletions) = (
        session.document.total_additions,
        session.document.total_deletions,
    );
    let stats = row![
        text(format!("+{additions}"))
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(stat_color(additions, theme.added_text, theme)),
        text(format!("−{deletions}"))
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(stat_color(deletions, theme.removed_text, theme)),
        text(format_file_count(session.document.files.len()))
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(theme.subtle_text),
    ]
    .spacing(7)
    .align_y(alignment::Vertical::Center);

    let Some(details) = session.revision_details.as_ref() else {
        return container(stats)
            .width(Length::Fill)
            .height(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .padding([0.0, crate::theme::space::XL])
            .center_y(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into();
    };

    let description = details.description.trim();
    let title = description
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("No description");
    let meta = revision_meta(ui, details, theme);

    // Counts of nothing say nothing; the empty state below says it better.
    let stats: Element<'_, Message> = if session.document.files.is_empty() {
        Space::new().into()
    } else {
        stats.into()
    };
    let title_stack = stack![
        column![
            stats,
            text(title.to_owned())
                .size(text_size::TITLE)
                .font(crate::theme::emphasis_font(
                    ui.config.ui_font,
                    iced::font::Weight::Medium,
                ))
                .color(if description.is_empty() {
                    theme.subtle_text
                } else {
                    theme.text
                })
                .wrapping(text::Wrapping::None),
            meta,
        ]
        .spacing(crate::theme::space::XS)
        .width(Length::Fill)
    ]
    .clip(true)
    .width(Length::Fill);

    let mut actions = row![].spacing(6).align_y(alignment::Vertical::Center);
    if session.capabilities.mutate {
        actions = actions.push(ui::tooltip(
            ui::icon_button(icons::PENCIL, theme.muted_text, ui::Size::Compact, theme)
                .on_press(Message::Action(Action::EditDescription { target: None })),
            "Edit description",
            iced::widget::tooltip::Position::Left,
            ui.config.ui_font,
            theme,
        ));
    }

    let content = column![
        row![title_stack, actions]
            .spacing(12)
            .align_y(alignment::Vertical::Center)
    ]
    .spacing(10);

    container(content)
        .width(Length::Fill)
        .padding([10.0, crate::theme::space::XL])
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// The header's identity line: the change id as the graph draws it (the
/// shortest unique prefix bright), the commit id dimmed, then the author and
/// a relative date with the exact one on hover.
fn revision_meta<'a>(
    ui: &'a Diffui,
    details: &'a diffui_core::RevisionDetails,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    use crate::meta_line::{Badge, Id, Meta};
    use crate::sidebar::{COMMIT_ID_CHARS, REVISION_ID_CHARS, revision_id_display_len};

    let row = ui
        .active()
        .session
        .commits
        .find_by_commit_id(&details.commit_id);
    let mut ids = Vec::new();
    match details.change_id.as_deref() {
        Some(change_id) => {
            let unique = row
                .and_then(|row| row.shortest_change_id_len())
                .unwrap_or(REVISION_ID_CHARS);
            let mut id: String = change_id
                .chars()
                .take(revision_id_display_len(unique, change_id))
                .collect();
            // How jj log addresses one copy of a divergent or hidden change.
            if let Some(offset) = row.and_then(|row| row.change_offset()) {
                id.push_str(&format!("/{offset}"));
            }
            ids.push(Id {
                text: id,
                bright: unique,
                bright_color: theme.accent,
                color: theme.subtle_text,
            });
            ids.push(Id::plain(
                details
                    .commit_id
                    .chars()
                    .take(COMMIT_ID_CHARS)
                    .collect::<String>(),
                theme.muted_text,
            ));
        }
        // Git commits and PRs have one id: a hash, or the PR number.
        None => ids.push(Id::plain(
            details
                .commit_id
                .chars()
                .take(COMMIT_ID_CHARS)
                .collect::<String>(),
            theme.muted_text,
        )),
    }
    let mut badges = Vec::new();
    if row.is_some_and(|row| row.is_hidden()) {
        badges.push(Badge {
            label: "hidden",
            color: theme.subtle_text,
        });
    } else if row.is_some_and(|row| row.is_divergent()) {
        badges.push(Badge {
            label: "divergent",
            color: theme.modified_token,
        });
    }
    let author = &details.author;
    let who = if !author.name.is_empty() {
        Some((author.name.clone(), Some(author.email.clone())))
    } else if !author.email.is_empty() {
        Some((author.email.clone(), None))
    } else {
        None
    };
    let when = author
        .timestamp
        .as_ref()
        .filter(|timestamp| !timestamp.is_empty())
        .map(|timestamp| {
            (
                diffui_core::history::parse_timestamp_millis(timestamp),
                timestamp.clone(),
            )
        });
    crate::meta_line::view(
        Meta {
            ids,
            badges,
            who,
            when,
            operation: None,
        },
        ui,
        theme,
    )
}

/// The selectable block atop the diff: the description's body and the
/// identity fields. The subject is left out — the header above already
/// shows it as the title — along with the blank lines that set it apart.
pub(crate) fn revision_header_lines(details: &diffui_core::RevisionDetails) -> Vec<HeaderLine> {
    let description = details.description.trim();
    let body = description
        .split_once('\n')
        .map_or("", |(_, body)| body)
        .trim();
    let mut lines = body
        .lines()
        .map(HeaderLine::description)
        .collect::<Vec<_>>();
    if !body.is_empty() {
        lines.push(HeaderLine::blank());
    }
    if let Some(change_id) = &details.change_id {
        lines.push(HeaderLine::field("change", change_id));
    }
    lines.push(HeaderLine::field("commit", &details.commit_id));
    if !details.bookmarks.is_empty() {
        lines.push(HeaderLine::field(
            "bookmarks",
            &details.bookmarks.join(", "),
        ));
    }
    lines.push(HeaderLine::field(
        "author",
        &format_signature_line(&details.author),
    ));
    // Only when someone else committed it: the same person again, a few
    // minutes later, is noise.
    if let Some(committer) = details.committer.as_ref().filter(|committer| {
        committer.name != details.author.name || committer.email != details.author.email
    }) {
        lines.push(HeaderLine::field(
            "committer",
            &format_signature_line(committer),
        ));
    }
    if let Some(signature) = &details.signature {
        lines.push(HeaderLine::field("signature", signature));
    }
    lines
}

fn format_file_count(count: usize) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
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
    use iced::Font;

    use super::{HeaderLine, revision_header_lines};

    #[test]
    fn selectable_details_preserve_description_and_identity() {
        let details = diffui_core::RevisionDetails {
            commit_id: "abcdef012345".to_owned(),
            change_id: Some("zyxwvut98765".to_owned()),
            bookmarks: vec!["main".to_owned()],
            author: diffui_core::SignatureInfo {
                name: "Ada".to_owned(),
                email: "ada@example.com".to_owned(),
                timestamp: Some("2026-09-07".to_owned()),
            },
            committer: None,
            signature: None,
            description: "subject\n\nbody".to_owned(),
        };

        let rendered = revision_header_lines(&details)
            .into_iter()
            .map(|line| match line {
                HeaderLine::Description(value) => format!("description:{value}"),
                HeaderLine::Blank => "blank".to_owned(),
                HeaderLine::Field { label, value } => format!("{label}{value}"),
                other => panic!("unexpected header line: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            [
                "description:body",
                "blank",
                "change   zyxwvut98765",
                "commit   abcdef012345",
                "bookmarksmain",
                "author   Ada <ada@example.com> (2026-09-07)",
            ]
        );
    }

    #[test]
    fn description_height_counts_newlines_and_wrapping() {
        let size = 12.0;
        let line_height = size * crate::measure::LINE_HEIGHT_MULTIPLIER;
        let count = |content: &str, width: f32| {
            crate::measure::wrapped_line_count(content, size, Font::MONOSPACE, line_height, width)
        };
        let wide = 10_000.0;
        assert_eq!(count("", wide), 1);
        assert_eq!(count("one\ntwo", wide), 2);
        assert_eq!(count("short", wide), 1);
        // A line well past the width must wrap into several visual lines.
        let char_width = crate::measure::line_width("M", size, Font::MONOSPACE).max(1.0);
        assert!(count(&"word ".repeat(40), char_width * 12.0) > 2);
    }
}
