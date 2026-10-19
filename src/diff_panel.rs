use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    widget::{Space, column, container, mouse_area, row, stack, text},
};

use crate::diff_view::{self, DiffFileView, DiffView};
use crate::find;
use crate::icons;
use crate::revision_list::{FileRowView, RevisionList};
use crate::theme::{
    ThemeSpec, centered_control_content, chip_background, diff_palette, diff_panel_style,
    file_status_color, text_size,
};
use crate::ui;
use crate::{Action, Diffui, LoadStatus, Message, UiEvent};
use diffui_core::FileTreeRow;
use jj_lib::graph::GraphEdgeType;
use std::rc::Rc;

const EMPTY_STATE_TEXT_SIZE: f32 = text_size::BODY_LG;
pub(crate) const DESCRIPTION_EDITOR_ID: &str = "revision-description-editor";
const DESCRIPTION_EDITOR_GAP: f32 = 8.0;
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

    let mut trailing = row![].align_y(alignment::Vertical::Center);
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
        // A section's count rides in its title, as in "Changed commits (2)".
        text(format!("Changed files ({})", files.len()))
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
            let mut actions = row![]
                .spacing(ui::BUTTON_GAP)
                .align_y(alignment::Vertical::Center);
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
            // No diff to carry the commit's header, so a header-only diff
            // view stands in above the empty state, selectable the same way.
            let header = ui
                .active()
                .session
                .revision_details
                .as_ref()
                .map(|details| {
                    crate::commit_info::header_lines(ui, details, theme, true, diff_pane_width(ui))
                })
                .filter(|lines| !lines.is_empty())
                .map(|lines| {
                    let height = diff_view::header_block_height(&lines, ui.config.code_type);
                    container(
                        DiffView::new(
                            Vec::new(),
                            0,
                            format!("header:{}", ui.active().session.document_id),
                            diff_palette(theme),
                            ui.config.mono_font,
                            ui.config.code_type,
                            ui.config.multi_click_ms,
                            |index| Message::Action(Action::SelectFile(index)),
                        )
                        .on_copy(|text| Message::Action(Action::Copy(text)))
                        .on_header_action(crate::commit_info::header_action)
                        .content_version(ui.document_version)
                        .with_header(lines),
                    )
                    .height(Length::Fixed(height))
                });
            let mut stack = column![].push(banner).push(summary);
            if let Some(header) = header {
                stack = stack.push(header);
            }
            return container(stack.push(empty))
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
        .side_by_side(split_in_effect(ui))
        .on_header_action(crate::commit_info::header_action);
        if let Some(details) = ui.active().session.revision_details.as_ref() {
            dv = dv.with_header(crate::commit_info::header_lines(
                ui,
                details,
                theme,
                true,
                diff_pane_width(ui),
            ));
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
        let layers = stack![diff_view, find_overlay].clip(true);
        // The selection strip docks below the diff rather than floating over
        // it, so the last lines stay readable while something is picked.
        let diff_with_find: Element<'a, Message> = match crate::changes::selection_bar(ui, theme) {
            Some(bar) => column![layers, bar].into(),
            None => layers.into(),
        };

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
        let on_action = |action| Message::Ui(UiEvent::DescriptionAction(action));
        let input = crate::input::multi_line(
            &editor.content,
            crate::input::MultiLine {
                id: Some(DESCRIPTION_EDITOR_ID),
                placeholder: if editor.commit {
                    "Describe the change you are committing…"
                } else {
                    "Describe this revision…"
                },
                font: ui.config.mono_font,
                size: ui.config.code_type.size,
                height: input_height,
            },
            theme,
            // Read-only while saving.
            (!saving).then_some(on_action),
        );

        let cancel = ui::cancel_button(ui::Size::Compact, ui.config.ui_font, theme)
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
        .spacing(ui::BUTTON_GAP)
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
pub(crate) fn diff_pane_width(ui: &Diffui) -> f32 {
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
    let width = (ui.window_size.width
        - history_width
        - files_width
        - 1.0
        - diff_view::HEADER_HORIZONTAL_PADDING * 2.0)
        .max(1.0);
    crate::input::fit_height(
        &editor.text(),
        ui.config.mono_font,
        ui.config.code_type.size,
        width,
        88.0,
        crate::input::editor_max_height(ui.window_size.height),
    )
}

/// The commit info block above the diff (see [`crate::commit_info`]), or
/// the description editor while it is open for this revision.
fn build_revision_summary<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let session = &ui.active().session;
    if ui
        .active()
        .description_editor()
        .is_some_and(|editor| editor.target == session.selected_revision)
    {
        return build_description_editor(ui, theme);
    }
    crate::commit_info::view(ui, theme)
}

#[cfg(test)]
mod tests {
    use iced::Font;

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
