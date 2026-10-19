//! The commit info block above the diff.
//!
//! One block says what the revision is: the title with the change's size and
//! the edit and fold controls; the description body at a reading measure,
//! clamped when long; and a field grid in two columns, who and when on the
//! left, which ids on the right, each id with a copy button and the parents
//! as links. Folded, the block is the title and the shared
//! [`crate::meta_line`] line, the same line the History view's headers use.

use diffui_core::history::{human_timestamp, now_millis, parse_timestamp_millis};
use diffui_core::{ParentBrief, RevisionDetails};
use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    widget::{Column, Row, Space, button, column, container, row, text},
};

use crate::meta_line::{self, Badge, Id, Meta};
use crate::sidebar::{COMMIT_ID_CHARS, REVISION_ID_CHARS, revision_id_display_len};
use crate::theme::{ThemeSpec, chip_background, emphasis_font, space, text_size};
use crate::{Action, Diffui, Message, icons, ui};

/// Lines of description body shown before the rest folds behind "Show more".
const BODY_CLAMP_LINES: usize = 4;
/// Widest the description body runs: a comfortable reading measure.
const BODY_MAX_WIDTH: f32 = 720.0;
/// Width of the field grid's label column.
const FIELD_LABEL_WIDTH: f32 = 64.0;
/// One field row: tall enough for the copy button beside an id.
const FIELD_ROW_HEIGHT: f32 = 22.0;
/// Side of the copy and parent-link hit targets inside a field row.
const FIELD_BUTTON: f32 = 20.0;
/// Side of the author's initials badge.
const MONOGRAM_SIZE: f32 = 18.0;
/// The title leads the pane, a step above dialog titles.
const TITLE_SIZE: f32 = 17.0;
/// Widest a name, email or parent subject runs before it ellipsizes.
const FIELD_TEXT_MAX: f32 = 240.0;
/// Width of the additions/deletions bar beside the counts.
const STAT_BAR_WIDTH: f32 = 36.0;

pub(crate) fn view<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let session = &ui.active().session;
    let stats = (!session.document.files.is_empty()).then(|| diff_stats(ui, theme));
    let Some(details) = session.revision_details.as_ref() else {
        return container(stats.unwrap_or_else(|| Space::new().into()))
            .width(Length::Fill)
            .height(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .padding([0.0, space::XL])
            .center_y(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into();
    };

    let description = details.description.trim();
    let title = description.lines().find(|line| !line.trim().is_empty());
    let body = description
        .split_once('\n')
        .map_or("", |(_, body)| body)
        .trim();

    let mut title_row = row![
        text(title.unwrap_or("No description").to_owned())
            .size(TITLE_SIZE)
            .font(emphasis_font(
                ui.config.ui_font,
                iced::font::Weight::Semibold
            ))
            .color(if title.is_some() {
                theme.text
            } else {
                theme.subtle_text
            })
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
    ]
    .spacing(space::SM)
    .align_y(alignment::Vertical::Center);
    if let Some(stats) = stats {
        title_row = title_row.push(container(stats).padding(Padding {
            right: space::SM,
            ..Padding::ZERO
        }));
    }
    if session.capabilities.mutate {
        title_row = title_row.push(ui::tooltip(
            ui::icon_button(icons::PENCIL, theme.muted_text, ui::Size::Compact, theme)
                .on_press(Message::Action(Action::EditDescription { target: None })),
            "Edit description",
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ));
    }
    let folded = ui.commit_details_collapsed;
    title_row = title_row.push(ui::tooltip(
        ui::icon_button(
            if folded {
                icons::UNFOLD_VERTICAL
            } else {
                icons::FOLD_VERTICAL
            },
            theme.muted_text,
            ui::Size::Compact,
            theme,
        )
        .on_press(Message::Action(Action::ToggleCommitDetails)),
        if folded {
            "Show the description and details"
        } else {
            "Show just the title"
        },
        iced::widget::tooltip::Position::Bottom,
        ui.config.ui_font,
        theme,
    ));

    let mut block = Column::new().push(title_row);
    if folded {
        block = block
            .push(revision_meta(ui, details, theme))
            .spacing(space::XXS);
    } else {
        block = block.spacing(space::SM);
        if !body.is_empty() {
            block = block.push(description_body(ui, body, theme));
        }
        block = block.push(container(field_grid(ui, details, theme)).padding(Padding {
            top: space::XS,
            ..Padding::ZERO
        }));
    }
    container(block)
        .width(Length::Fill)
        .padding(Padding {
            top: space::MD,
            right: space::LG,
            bottom: if folded { space::MD } else { space::LG },
            left: space::XL,
        })
        .style(move |_| container::Style::default().background(theme.panel_background))
        .into()
}

/// `+N −M`, a bar splitting the change between them, and the file count.
fn diff_stats<'a>(ui: &Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let document = &ui.active().session.document;
    let (additions, deletions) = (document.total_additions, document.total_deletions);
    let count = |value: String, color: Color| {
        text(value)
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(color)
    };
    let mut stats = row![
        count(
            format!("+{additions}"),
            crate::diff_panel::stat_color(additions, theme.added_text, theme)
        ),
        count(
            format!("\u{2212}{deletions}"),
            crate::diff_panel::stat_color(deletions, theme.removed_text, theme)
        ),
    ]
    .spacing(space::SM)
    .align_y(alignment::Vertical::Center);
    let total = additions + deletions;
    if total > 0 {
        let added = (STAT_BAR_WIDTH * additions as f32 / total as f32).round();
        let segment = |width: f32, color: Color| {
            container(Space::new())
                .width(Length::Fixed(width))
                .height(Length::Fixed(4.0))
                .style(move |_| container::Style {
                    background: Some(Background::Color(color)),
                    border: Border {
                        radius: 2.0.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                })
        };
        let mut bar = Row::new().spacing(1);
        if added > 0.0 {
            bar = bar.push(segment(added, theme.added_text));
        }
        if added < STAT_BAR_WIDTH {
            bar = bar.push(segment(STAT_BAR_WIDTH - added, theme.removed_text));
        }
        stats = stats.push(bar);
    }
    let files = document.files.len();
    stats
        .push(count(
            if files == 1 {
                "1 file".to_owned()
            } else {
                format!("{files} files")
            },
            theme.subtle_text,
        ))
        .into()
}

/// The description body at a reading measure. Past [`BODY_CLAMP_LINES`] it
/// clamps behind "Show more"; unfolded, it scrolls past 40% of the window so
/// the diff keeps a foothold.
fn description_body<'a>(ui: &Diffui, body: &str, theme: ThemeSpec) -> Element<'a, Message> {
    let size = text_size::BODY;
    let line_height = (size * crate::measure::LINE_HEIGHT_MULTIPLIER).round();
    let width =
        (crate::diff_panel::diff_pane_width(ui) - space::XL - space::LG).clamp(1.0, BODY_MAX_WIDTH);
    let measure = |content: &str| {
        crate::measure::wrapped_line_count(content, size, ui.config.ui_font, line_height, width)
    };
    let lines = measure(body);
    let paragraphs: Vec<usize> = body.split("\n\n").map(measure).collect();
    let paragraph = text(body.to_owned())
        .size(size)
        .font(ui.config.ui_font)
        .line_height(iced::Pixels(line_height))
        .color(theme.text)
        .width(Length::Fixed(width));
    let Some(clamp) = clamped_lines(&paragraphs, lines) else {
        return paragraph.into();
    };
    let expanded = ui.commit_body_expanded;
    let shown: Element<'a, Message> = if expanded {
        let natural = lines as f32 * line_height;
        let cap = (ui.window_size.height * 0.4).max(BODY_CLAMP_LINES as f32 * line_height);
        ui::scrollable(
            container(paragraph).padding(Padding::ZERO.right(crate::scrollbar::GUTTER)),
            theme,
        )
        .height(Length::Fixed(natural.min(cap)))
        .into()
    } else {
        container(paragraph)
            .height(Length::Fixed(clamp as f32 * line_height))
            .clip(true)
            .into()
    };
    let toggle = button(
        row![
            text(if expanded { "Show less" } else { "Show more" })
                .size(text_size::UI)
                .font(ui.config.ui_font),
            icons::icon(
                if expanded {
                    icons::CHEVRON_UP
                } else {
                    icons::CHEVRON_DOWN
                },
                12.0,
                theme.accent,
            ),
        ]
        .spacing(space::XXS)
        .align_y(alignment::Vertical::Center),
    )
    .padding(0)
    .on_press(Message::Action(Action::ToggleCommitBody))
    .style(move |_, _| button::Style {
        text_color: theme.accent,
        ..button::Style::default()
    });
    column![shown, toggle].spacing(space::XS).into()
}

/// How many lines a folded body shows, or `None` when it shows whole: as
/// many whole paragraphs as fit in [`BODY_CLAMP_LINES`] (so the fold never
/// lands on the blank line between two), or the clamp when even the first
/// is longer. One line past the clamp isn't worth a fold.
fn clamped_lines(paragraphs: &[usize], total: usize) -> Option<usize> {
    if total <= BODY_CLAMP_LINES + 1 {
        return None;
    }
    let mut shown = 0;
    for (index, &lines) in paragraphs.iter().enumerate() {
        let with = shown + usize::from(index > 0) + lines;
        if with > BODY_CLAMP_LINES {
            break;
        }
        shown = with;
    }
    Some(if shown == 0 { BODY_CLAMP_LINES } else { shown })
}

/// Who and when on the left; the change, commit, parents and refs on the
/// right. The columns wrap onto each other when the pane is narrow.
fn field_grid<'a>(
    ui: &'a Diffui,
    details: &'a RevisionDetails,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let mut people = Column::new();
    people = people.push(field(
        "author",
        signature(ui, &details.author, true, theme),
        ui,
        theme,
    ));
    if let Some(timestamp) = details
        .author
        .timestamp
        .as_deref()
        .filter(|timestamp| !timestamp.is_empty())
    {
        people = people.push(field("date", date(ui, timestamp, theme), ui, theme));
    }
    // Only when someone else committed it: the same person again, a few
    // minutes later, is noise.
    if let Some(committer) = details.committer.as_ref().filter(|committer| {
        committer.name != details.author.name || committer.email != details.author.email
    }) {
        people = people.push(field(
            "committer",
            signature(ui, committer, false, theme),
            ui,
            theme,
        ));
    }
    if let Some(signature) = &details.signature {
        people = people.push(field(
            "signature",
            plain(signature.clone(), theme.muted_text, ui),
            ui,
            theme,
        ));
    }

    let row_of = ui
        .active()
        .session
        .commits
        .find_by_commit_id(&details.commit_id);
    let mut ids = Column::new();
    if let Some(change_id) = details.change_id.as_deref() {
        let unique = unique_prefix(ui, row_of);
        let mut shown = change_id.to_owned();
        // How jj log addresses one copy of a divergent or hidden change.
        if let Some(offset) = row_of.and_then(|row| row.change_offset()) {
            shown.push_str(&format!("/{offset}"));
        }
        let mut value = Row::new()
            .spacing(space::SM)
            .align_y(alignment::Vertical::Center)
            .push(meta_line::id_text(
                Id {
                    text: shown,
                    bright: unique,
                    bright_color: theme.accent,
                    color: theme.subtle_text,
                },
                ui,
            ));
        for badge in status_badges(row_of, theme) {
            value = value.push(meta_line::badge(badge.label, badge.color, ui));
        }
        value = value.push(copy_button(change_id, "Copy the change id", ui, theme));
        ids = ids.push(field("change", inset(value), ui, theme));
    }
    let pull_request = details.commit_id.starts_with('#');
    let commit = Row::new()
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center)
        .push(meta_line::id_text(
            if details.change_id.is_some() {
                // jj: the short commit id bright, as the graph shows it.
                Id {
                    text: details.commit_id.clone(),
                    bright: COMMIT_ID_CHARS,
                    bright_color: theme.muted_text,
                    color: theme.subtle_text,
                }
            } else if pull_request {
                Id::plain(details.commit_id.clone(), theme.muted_text)
            } else {
                // Git: the hash is the revision's one id, coloured like the
                // graph colours it.
                Id {
                    text: details.commit_id.clone(),
                    bright: unique_prefix(ui, row_of),
                    bright_color: theme.accent,
                    color: theme.subtle_text,
                }
            },
            ui,
        ))
        .push(copy_button(
            &details.commit_id,
            if pull_request {
                "Copy the pull request number"
            } else {
                "Copy the commit id"
            },
            ui,
            theme,
        ));
    ids = ids.push(field(
        if pull_request { "pull" } else { "commit" },
        inset(commit),
        ui,
        theme,
    ));
    if !details.parents.is_empty() {
        let mut parents = Row::new()
            .spacing(space::XS)
            .align_y(alignment::Vertical::Center);
        for parent in &details.parents {
            parents = parents.push(parent_link(ui, parent, theme));
        }
        ids = ids.push(field(
            if details.parents.len() == 1 {
                "parent"
            } else {
                "parents"
            },
            parents,
            ui,
            theme,
        ));
    }
    if !details.bookmarks.is_empty() {
        let mut refs = Row::new()
            .spacing(space::XS)
            .align_y(alignment::Vertical::Center);
        for name in &details.bookmarks {
            refs = refs.push(meta_line::badge(name, theme.accent, ui));
        }
        ids = ids.push(field(
            if details.change_id.is_some() {
                "bookmarks"
            } else {
                "refs"
            },
            inset(refs),
            ui,
            theme,
        ));
    }

    row![people, ids]
        .spacing(space::XXL)
        .wrap()
        .vertical_spacing(space::XS)
        .into()
}

/// Lines a plain value up with the text inside a parent link, which carries
/// the link's hover padding.
fn inset<'a>(value: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(value)
        .padding(Padding {
            left: space::XS,
            ..Padding::ZERO
        })
        .into()
}

/// One row of the grid: a quiet lowercase label, then its value.
fn field<'a>(
    label: &'static str,
    value: impl Into<Element<'a, Message>>,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    row![
        container(
            text(label)
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.subtle_text)
        )
        .width(Length::Fixed(FIELD_LABEL_WIDTH)),
        value.into(),
    ]
    .height(Length::Fixed(FIELD_ROW_HEIGHT))
    .align_y(alignment::Vertical::Center)
    .into()
}

fn plain<'a>(value: String, color: Color, ui: &Diffui) -> text::Text<'a> {
    text(value)
        .size(text_size::UI)
        .font(ui.config.ui_font)
        .color(color)
        .wrapping(text::Wrapping::None)
        .ellipsis(text::Ellipsis::End)
}

/// A name with its email after it, dimmer; the author leads with initials.
fn signature<'a>(
    ui: &Diffui,
    signature: &diffui_core::SignatureInfo,
    monogram_first: bool,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let mut value = Row::new()
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center);
    if monogram_first {
        value = value.push(monogram(&signature.name, &signature.email, ui, theme));
    }
    let name = if signature.name.is_empty() {
        "Unknown"
    } else {
        signature.name.as_str()
    };
    value = value.push(
        container(plain(name.to_owned(), theme.text, ui)).width(Length::Shrink.max(FIELD_TEXT_MAX)),
    );
    if !signature.email.is_empty() {
        value = value.push(
            container(plain(signature.email.clone(), theme.subtle_text, ui))
                .width(Length::Shrink.max(FIELD_TEXT_MAX)),
        );
    }
    value.into()
}

/// Initials in a tinted disc: who it is at a glance, with no avatar fetched
/// from anywhere.
fn monogram<'a>(name: &str, email: &str, ui: &Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    container(
        text(initials(name, email))
            .size(9.0)
            .font(emphasis_font(
                ui.config.ui_font,
                iced::font::Weight::Semibold,
            ))
            .color(theme.accent),
    )
    .center(Length::Fixed(MONOGRAM_SIZE))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(theme.accent))),
        border: Border {
            radius: (MONOGRAM_SIZE / 2.0).into(),
            ..Border::default()
        },
        ..container::Style::default()
    })
    .into()
}

/// Up to two initials from a name ("Ada Lovelace" → "AL"), else the first
/// letter of the email, else "?".
pub(crate) fn initials(name: &str, email: &str) -> String {
    let from_name: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if !from_name.is_empty() {
        return from_name;
    }
    email
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

/// "Oct 19, 2026, 15:48" with "51m ago" after it.
fn date<'a>(ui: &Diffui, timestamp: &str, theme: ThemeSpec) -> Element<'a, Message> {
    let mut value = Row::new()
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center)
        .push(plain(
            human_timestamp(timestamp).unwrap_or_else(|| timestamp.to_owned()),
            theme.text,
            ui,
        ));
    if let Some(millis) = parse_timestamp_millis(timestamp) {
        value = value.push(plain(
            meta_line::age_phrase(millis, now_millis()),
            theme.subtle_text,
            ui,
        ));
    }
    value.into()
}

fn status_badges(row: Option<diffui_core::RowView<'_>>, theme: ThemeSpec) -> Vec<Badge> {
    match row {
        Some(row) if row.is_hidden() => vec![Badge {
            label: "hidden",
            color: theme.subtle_text,
        }],
        Some(row) if row.is_divergent() => vec![Badge {
            label: "divergent",
            color: theme.modified_token,
        }],
        _ => Vec::new(),
    }
}

/// How much of a row's id is its shortest unique prefix: jj's own count,
/// else the one the graph computed (git hashes), else the default id width.
fn unique_prefix(ui: &Diffui, row: Option<diffui_core::RowView<'_>>) -> usize {
    row.and_then(|row| {
        row.shortest_change_id_len().or_else(|| {
            ui.active()
                .session
                .sidebar_prefix_lens
                .get(row.index())
                .copied()
        })
    })
    .filter(|&len| len > 0)
    .unwrap_or(REVISION_ID_CHARS)
}

/// A small ghost button copying the whole of `value`.
fn copy_button<'a>(
    value: &str,
    tip: &'static str,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    ui::tooltip(
        button(
            container(icons::icon(icons::COPY, 12.0, theme.subtle_text))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(FIELD_BUTTON))
        .height(Length::Fixed(FIELD_BUTTON))
        .padding(0)
        .on_press(Message::Action(Action::Copy(value.to_owned())))
        .style(move |_, status| crate::theme::ghost_button_style(theme, status)),
        tip,
        iced::widget::tooltip::Position::Bottom,
        ui.config.ui_font,
        theme,
    )
}

/// A parent as a link: its id as the graph draws it and its subject. Opens
/// it in the diff view.
fn parent_link<'a>(
    ui: &'a Diffui,
    parent: &'a ParentBrief,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let session = &ui.active().session;
    let row_of = session.commits.find_by_commit_id(&parent.commit_id);
    let id = match parent.change_id.as_deref() {
        Some(change_id) => {
            let unique = unique_prefix(ui, row_of);
            let mut shown: String = change_id
                .chars()
                .take(revision_id_display_len(unique, change_id))
                .collect();
            if let Some(offset) = row_of.and_then(|row| row.change_offset()) {
                shown.push_str(&format!("/{offset}"));
            }
            Id {
                text: shown,
                bright: unique,
                bright_color: theme.accent,
                color: theme.subtle_text,
            }
        }
        None => Id {
            text: parent
                .commit_id
                .chars()
                .take(COMMIT_ID_CHARS)
                .collect::<String>(),
            bright: unique_prefix(ui, row_of),
            bright_color: theme.accent,
            color: theme.subtle_text,
        },
    };
    let root = !parent.commit_id.is_empty() && parent.commit_id.chars().all(|c| c == '0');
    let subject = if root {
        "root()".to_owned()
    } else if !parent.summary.is_empty() {
        parent.summary.clone()
    } else {
        row_of
            .map(|row| {
                row.description()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| "(no description set)".to_owned())
    };
    let selection = session.canonical_selection(&parent.commit_id);
    button(
        row![
            meta_line::id_text(id, ui),
            container(plain(subject, theme.muted_text, ui))
                .width(Length::Shrink.max(FIELD_TEXT_MAX)),
        ]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center),
    )
    .height(Length::Fixed(FIELD_BUTTON))
    .padding(Padding::from([0.0, space::XS]))
    .on_press(Message::Action(Action::SelectRevision(selection)))
    .style(move |_, status| crate::theme::ghost_button_style(theme, status))
    .into()
}

/// The folded block's identity line: the change id as the graph draws it
/// (the shortest unique prefix bright), the commit id dimmed, then the
/// author and a relative date with the exact one on hover.
fn revision_meta<'a>(
    ui: &'a Diffui,
    details: &'a RevisionDetails,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let row_of = ui
        .active()
        .session
        .commits
        .find_by_commit_id(&details.commit_id);
    let mut ids = Vec::new();
    match details.change_id.as_deref() {
        Some(change_id) => {
            let unique = unique_prefix(ui, row_of);
            let mut id: String = change_id
                .chars()
                .take(revision_id_display_len(unique, change_id))
                .collect();
            // How jj log addresses one copy of a divergent or hidden change.
            if let Some(offset) = row_of.and_then(|row| row.change_offset()) {
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
        None if !details.commit_id.starts_with('#') => ids.push(Id {
            text: details
                .commit_id
                .chars()
                .take(COMMIT_ID_CHARS)
                .collect::<String>(),
            bright: unique_prefix(ui, row_of),
            bright_color: theme.accent,
            color: theme.subtle_text,
        }),
        // A PR's one id is its number.
        None => ids.push(Id::plain(
            details
                .commit_id
                .chars()
                .take(COMMIT_ID_CHARS)
                .collect::<String>(),
            theme.muted_text,
        )),
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
        .map(|timestamp| (parse_timestamp_millis(timestamp), timestamp.clone()));
    meta_line::view(
        Meta {
            ids,
            badges: status_badges(row_of, theme),
            who,
            when,
            operation: None,
        },
        ui,
        theme,
    )
}

#[cfg(test)]
mod tests {
    use super::{BODY_CLAMP_LINES, clamped_lines, initials};

    #[test]
    fn a_long_body_folds_on_a_paragraph_boundary() {
        // Short enough to show whole.
        assert_eq!(clamped_lines(&[5], 5), None);
        // Three lines, a blank, then more: the fold lands after the three.
        assert_eq!(clamped_lines(&[3, 4, 2], 11), Some(3));
        // Two short paragraphs fit with the blank between them.
        assert_eq!(clamped_lines(&[1, 2, 6], 11), Some(4));
        // A first paragraph longer than the clamp is cut at the clamp.
        assert_eq!(clamped_lines(&[9], 9), Some(BODY_CLAMP_LINES));
    }

    #[test]
    fn initials_come_from_the_name_then_the_email() {
        assert_eq!(initials("Ada Lovelace", "ada@example.com"), "AL");
        assert_eq!(
            initials("Bartholomew Alexander Featherstonehaugh", ""),
            "BA"
        );
        assert_eq!(initials("wonrax", "x@y.z"), "W");
        assert_eq!(initials("", "hai@example.com"), "H");
        assert_eq!(initials("", ""), "?");
    }
}
