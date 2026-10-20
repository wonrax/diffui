//! The commit info block: what a revision is, above its diff.
//!
//! Above the diff sits the title, with the change's size and the edit and
//! fold controls. The rest scrolls with the diff, drawn by the diff view as
//! its header ([`header_lines`]) so it stays selectable text: the
//! description's body in mono, then one column of fields: author, date,
//! change and commit ids in the graph's colours with copy buttons, parents
//! as links, and the refs as the graph draws them. The History view's diffs
//! carry the same header. Folded, the block is the title and the shared
//! [`crate::meta_line`] line, the line the History view's headers use.

use diffui_core::history::{human_timestamp, now_millis, parse_timestamp_millis};
use diffui_core::{RevisionDetails, SignatureInfo};
use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    widget::{Column, Row, Space, container, row, text},
};

use crate::diff_view::{HeaderAction, HeaderField, HeaderLine};
use crate::meta_line::{self, Badge, Id, Meta};
use crate::sidebar::{COMMIT_ID_CHARS, REVISION_ID_CHARS, revision_id_display_len};
use crate::theme::{ThemeSpec, chip_background, emphasis_font, space, text_size};
use crate::{Action, Diffui, Message, icons, ui};

/// The title leads the pane at the dialog-title step, one above the graph
/// row's description (`BODY_LG`).
const TITLE_SIZE: f32 = text_size::TITLE;
/// Height of the additions/deletions bar under the counts.
const STAT_BAR_HEIGHT: f32 = 3.0;
/// The smallest share of the bar a non-zero side gets, so one deletion
/// against thousands of additions still shows.
const STAT_BAR_MIN_SHARE: f32 = 0.08;

pub(crate) fn view<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let session = &ui.active().session;
    let stats = (!session.document.files.is_empty()).then(|| diff_stats(ui, theme));
    // A range has no one revision to describe: its title counts the
    // commits, which the list under it names.
    if let Some(range) = crate::range::shown_range(ui) {
        let title_font = emphasis_font(ui.config.ui_font, iced::font::Weight::Semibold);
        let mut title_row = row![crate::typography::centered(
            text(crate::range::title(range))
                .size(TITLE_SIZE)
                .font(title_font)
                .color(theme.text)
                .wrapping(text::Wrapping::None)
                .width(Length::Fill),
            title_font,
            TITLE_SIZE,
        )]
        .spacing(space::SM)
        .align_y(alignment::Vertical::Center);
        if let Some(stats) = stats {
            title_row = title_row.push(container(stats).padding(Padding {
                right: space::SM,
                ..Padding::ZERO
            }));
        }
        return container(title_row)
            .width(Length::Fill)
            .padding(Padding {
                top: space::MD,
                right: space::LG,
                bottom: space::MD,
                left: space::XL,
            })
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into();
    }
    let Some(details) = session.revision_details.as_ref() else {
        return container(stats.unwrap_or_else(|| Space::new().into()))
            .width(Length::Fill)
            .height(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .padding([0.0, space::XL])
            .center_y(Length::Fixed(crate::field::PANEL_HEADER_HEIGHT))
            .style(move |_| container::Style::default().background(theme.panel_background))
            .into();
    };

    let title = details
        .description
        .lines()
        .find(|line| !line.trim().is_empty());

    let title_font = emphasis_font(ui.config.ui_font, iced::font::Weight::Semibold);
    // Centred on its caps against the buttons beside it.
    let mut title_row = row![crate::typography::centered(
        text(title.unwrap_or("No description").to_owned())
            .size(TITLE_SIZE)
            .font(title_font)
            .color(if title.is_some() {
                theme.text
            } else {
                theme.subtle_text
            })
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
        title_font,
        TITLE_SIZE,
    )]
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

    let mut block = Column::new().push(title_row).spacing(space::XXS);
    if folded {
        block = block.push(revision_meta(ui, details, theme));
    }
    container(block)
        .width(Length::Fill)
        .padding(Padding {
            top: space::MD,
            right: space::LG,
            bottom: space::MD,
            left: space::XL,
        })
        .style(move |_| container::Style::default().background(theme.panel_background))
        .into()
}

/// `+N −M` on one small line with a bar under it, as wide as the line,
/// split between them. The file count and exact totals are on hover.
fn diff_stats<'a>(ui: &Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let document = &ui.active().session.document;
    let (additions, deletions) = (document.total_additions, document.total_deletions);
    let font = ui.config.ui_font;
    let count =
        |value: String, color: Color| text(value).size(text_size::CAPTION).font(font).color(color);
    let counts = row![
        count(
            format!("+{}", compact_count(additions)),
            crate::diff_panel::stat_color(additions, theme.added_text, theme)
        ),
        count(
            format!("\u{2212}{}", compact_count(deletions)),
            crate::diff_panel::stat_color(deletions, theme.removed_text, theme)
        ),
    ]
    .spacing(space::SM);

    let segment = |portion: u16, color: Color| {
        container(Space::new())
            .width(Length::FillPortion(portion))
            .height(Length::Fixed(STAT_BAR_HEIGHT))
            .style(move |_| container::Style {
                background: Some(Background::Color(color)),
                border: Border {
                    radius: (STAT_BAR_HEIGHT / 2.0).into(),
                    ..Border::default()
                },
                ..container::Style::default()
            })
    };
    let (added, removed) = bar_portions(additions, deletions);
    let mut bar = Row::new().spacing(1).width(Length::Fill);
    if added > 0 {
        bar = bar.push(segment(added, theme.added_text));
    }
    if removed > 0 {
        bar = bar.push(segment(removed, theme.removed_text));
    }
    if added == 0 && removed == 0 {
        bar = bar.push(segment(1, chip_background(theme.muted_text)));
    }

    let files = document.files.len();
    let summary = format!(
        "{files} {}, {additions} added, {deletions} removed",
        if files == 1 { "file" } else { "files" }
    );
    ui::tooltip(
        Column::new()
            .push(counts)
            .push(bar)
            .spacing(space::XXS)
            .width(Length::Shrink),
        summary,
        iced::widget::tooltip::Position::Bottom,
        font,
        theme,
    )
}

/// A count short enough for the stats line: thousands abbreviated past
/// four digits.
fn compact_count(count: usize) -> String {
    match count {
        0..10_000 => count.to_string(),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{}M", count / 1_000_000),
    }
}

/// The bar's two shares out of 1000, each non-zero side getting at least
/// [`STAT_BAR_MIN_SHARE`] of it.
fn bar_portions(additions: usize, deletions: usize) -> (u16, u16) {
    let total = additions + deletions;
    if total == 0 {
        return (0, 0);
    }
    if additions == 0 || deletions == 0 {
        return (
            u16::from(additions > 0) * 1000,
            u16::from(deletions > 0) * 1000,
        );
    }
    let share =
        (additions as f32 / total as f32).clamp(STAT_BAR_MIN_SHARE, 1.0 - STAT_BAR_MIN_SHARE);
    let added = (share * 1000.0).round() as u16;
    (added, 1000 - added)
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

/// The diff view's header for `details`: the description's body, then the
/// field column. Empty when the block is folded, if `foldable` (the diff
/// pane; the History view's headers already say what the revision is, so
/// its diffs always carry the details).
///
/// `pane_width` is the width of the diff view it heads: with line wrap on,
/// the description wraps to it, since a header line is one row.
pub(crate) fn header_lines(
    ui: &Diffui,
    details: &RevisionDetails,
    theme: ThemeSpec,
    foldable: bool,
    pane_width: f32,
) -> Vec<HeaderLine> {
    if foldable && ui.commit_details_collapsed {
        return Vec::new();
    }
    let text = details
        .description
        .trim()
        .split_once('\n')
        .map_or("", |(_, body)| body)
        .trim();
    let columns = ui.diff_wrap.then(|| {
        let size = ui.config.code_type.size;
        let char_width = crate::measure::line_width("0000000000", size, ui.config.mono_font) / 10.0;
        let text_width = pane_width
            - 2.0 * crate::diff_view::HEADER_HORIZONTAL_PADDING
            - crate::scrollbar::GUTTER;
        (text_width / char_width.max(1.0)).floor().max(20.0) as usize
    });
    let mut lines: Vec<HeaderLine> = text
        .lines()
        .flat_map(|line| match columns {
            Some(columns) => wrap_columns(line, columns),
            None => vec![line.to_owned()],
        })
        .map(|line| HeaderLine::description(&line))
        .collect();
    if !lines.is_empty() {
        lines.push(HeaderLine::blank());
    }

    lines.push(rich(signature_field("Author", &details.author, theme)));
    if let Some(timestamp) = details
        .author
        .timestamp
        .as_deref()
        .filter(|timestamp| !timestamp.is_empty())
    {
        let exact = human_timestamp(timestamp).unwrap_or_else(|| timestamp.to_owned());
        let mut date = HeaderField::new("Date", exact.clone());
        if let Some(millis) = parse_timestamp_millis(timestamp) {
            date.value = format!("{exact}  {}", meta_line::age_phrase(millis, now_millis()));
            date.runs = vec![(0, theme.text), (exact.chars().count(), theme.subtle_text)];
        }
        lines.push(rich(date));
    }
    // Only when someone else committed it: the same person again, a few
    // minutes later, is noise.
    if let Some(committer) = details.committer.as_ref().filter(|committer| {
        committer.name != details.author.name || committer.email != details.author.email
    }) {
        lines.push(rich(signature_field("Committer", committer, theme)));
    }
    if let Some(signature) = &details.signature {
        lines.push(rich(HeaderField::new("Signature", signature.clone())));
    }

    let session = &ui.active().session;
    let row_of = session.commits.find_by_commit_id(&details.commit_id);
    if let Some(change_id) = details.change_id.as_deref() {
        let unique = unique_prefix(ui, row_of);
        let mut value = change_id.to_owned();
        // How jj log addresses one copy of a divergent or hidden change.
        if let Some(offset) = row_of.and_then(|row| row.change_offset()) {
            value.push_str(&format!("/{offset}"));
        }
        let mut change = HeaderField::new("Change", value);
        change.runs = vec![(0, theme.accent), (unique, theme.subtle_text)];
        if let Some(row) = row_of {
            change.chips = crate::sidebar::status_chips_for(row, theme, ui.config);
        }
        change.buttons = vec![(icons::COPY, HeaderAction::Copy(change_id.to_owned()))];
        lines.push(rich(change));
    }
    let pull_request = details.commit_id.starts_with('#');
    let mut commit = HeaderField::new(
        if pull_request {
            "Pull request"
        } else {
            "Commit"
        },
        details.commit_id.clone(),
    );
    commit.runs = if details.change_id.is_some() {
        // jj: the short commit id stands out, as the graph shows it.
        vec![(0, theme.muted_text), (COMMIT_ID_CHARS, theme.subtle_text)]
    } else if pull_request {
        vec![(0, theme.muted_text)]
    } else {
        // Git: the hash is the revision's one id, coloured as the graph
        // colours it.
        vec![
            (0, theme.accent),
            (unique_prefix(ui, row_of), theme.subtle_text),
        ]
    };
    commit.buttons = vec![(icons::COPY, HeaderAction::Copy(details.commit_id.clone()))];
    lines.push(rich(commit));

    for (index, parent) in details.parents.iter().enumerate() {
        let parent_row = session.commits.find_by_commit_id(&parent.commit_id);
        let unique = unique_prefix(ui, parent_row);
        let mut id: String = match parent.change_id.as_deref() {
            Some(change_id) => change_id
                .chars()
                .take(revision_id_display_len(unique, change_id))
                .collect(),
            None => parent.commit_id.chars().take(COMMIT_ID_CHARS).collect(),
        };
        if let Some(offset) = parent_row.and_then(|row| row.change_offset()) {
            id.push_str(&format!("/{offset}"));
        }
        let root = !parent.commit_id.is_empty() && parent.commit_id.chars().all(|c| c == '0');
        let subject = if root {
            "root()".to_owned()
        } else if !parent.summary.is_empty() {
            parent.summary.clone()
        } else {
            parent_row
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
        let id_chars = id.chars().count();
        let label = match (index, details.parents.len()) {
            (0, 1) => "Parent",
            (0, _) => "Parents",
            _ => "",
        };
        let mut field = HeaderField::new(label, format!("{id}  {subject}"));
        field.runs = vec![
            (0, theme.accent),
            (unique.min(id_chars), theme.subtle_text),
            (id_chars, theme.muted_text),
        ];
        field.link = Some((id_chars, HeaderAction::Open(parent.commit_id.clone())));
        lines.push(rich(field));
    }
    // The refs as the graph draws them on this revision's row, lane colour
    // and all; a revision off the graph shows its bookmarks in the accent.
    let chips = match row_of {
        Some(row) => crate::sidebar::graph_ref_chips(session, row.index(), theme, ui.config),
        None => {
            crate::sidebar::bookmark_chips_for(&details.bookmarks, theme.accent, theme, ui.config)
        }
    };
    if !chips.is_empty() {
        lines.push(HeaderLine::Bookmarks {
            label: if details.change_id.is_some() {
                "Bookmarks"
            } else {
                "Refs"
            }
            .to_owned(),
            chips,
        });
    }
    lines
}

/// What the diff view's header links and buttons do.
pub(crate) fn header_action(action: HeaderAction) -> Message {
    match action {
        HeaderAction::Copy(value) => Message::Action(Action::Copy(value)),
        HeaderAction::Open(commit_id) => Message::Action(Action::History(
            crate::history_panel::HistoryAction::OpenRevision(commit_id),
        )),
    }
}

/// `line` broken at spaces into rows of at most `columns` characters; a
/// word longer than a row is cut. Continuation rows keep the line's indent,
/// so a wrapped list item still hangs under its text.
fn wrap_columns(line: &str, columns: usize) -> Vec<String> {
    let line = line.trim_end();
    if line.chars().count() <= columns {
        return vec![line.to_owned()];
    }
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let hang = if indent.len() * 2 < columns {
        indent.as_str()
    } else {
        ""
    };
    let mut rows = Vec::new();
    let mut row = indent.clone();
    for word in line.split(' ').filter(|word| !word.is_empty()) {
        let mut word = word.to_owned();
        loop {
            let used = row.chars().count();
            let fresh = row.trim().is_empty();
            let needed = word.chars().count() + usize::from(!fresh);
            if used + needed <= columns {
                if !fresh {
                    row.push(' ');
                }
                row.push_str(&word);
                break;
            }
            if !fresh {
                rows.push(std::mem::replace(&mut row, hang.to_owned()));
                continue;
            }
            // A word wider than a whole row: cut it.
            let room = columns.saturating_sub(used).max(1);
            let head: String = word.chars().take(room).collect();
            word = word.chars().skip(room).collect();
            row.push_str(&head);
            rows.push(std::mem::replace(&mut row, hang.to_owned()));
            if word.is_empty() {
                break;
            }
        }
    }
    if !row.trim().is_empty() {
        rows.push(row);
    }
    rows
}

fn rich(field: HeaderField) -> HeaderLine {
    HeaderLine::Rich(Box::new(field))
}

/// "Name <email>", the address quieter.
fn signature_field(label: &str, signature: &SignatureInfo, theme: ThemeSpec) -> HeaderField {
    let name = if signature.name.is_empty() {
        "Unknown"
    } else {
        signature.name.as_str()
    };
    let mut field = HeaderField::new(label, name);
    if !signature.email.is_empty() {
        field.value = format!("{name} <{}>", signature.email);
        field.runs = vec![(0, theme.text), (name.chars().count(), theme.subtle_text)];
    }
    field
}

#[cfg(test)]
mod tests {
    use diffui_core::{ParentBrief, RevisionDetails, SignatureInfo};

    use super::{bar_portions, compact_count, header_lines, wrap_columns};

    #[test]
    fn the_stat_bar_keeps_both_sides_visible_and_counts_stay_short() {
        assert_eq!(bar_portions(0, 0), (0, 0));
        assert_eq!(bar_portions(5, 0), (1000, 0));
        assert_eq!(bar_portions(0, 5), (0, 1000));
        assert_eq!(bar_portions(1, 1), (500, 500));
        // One deletion against thousands of additions still gets a sliver.
        assert_eq!(bar_portions(10_000, 1), (920, 80));
        assert_eq!(compact_count(9_999), "9999");
        assert_eq!(compact_count(12_345), "12k");
        assert_eq!(compact_count(3_400_000), "3M");
    }

    #[test]
    fn description_lines_wrap_at_spaces_and_keep_their_indent() {
        assert_eq!(wrap_columns("short line", 20), ["short line"]);
        assert_eq!(
            wrap_columns("one two three four five", 10),
            ["one two", "three four", "five"]
        );
        assert_eq!(
            wrap_columns("  - a list item that wraps", 12),
            ["  - a list", "  item that", "  wraps"]
        );
        assert_eq!(wrap_columns("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
    }
    use crate::diff_view::{HeaderAction, HeaderLine};

    fn render(lines: &[HeaderLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                HeaderLine::Description(value) => format!("| {value}"),
                HeaderLine::Blank => String::new(),
                HeaderLine::Rich(field) => format!("{}: {}", field.label, field.value),
                HeaderLine::Bookmarks { label, chips } => format!(
                    "{label}: {}",
                    chips
                        .iter()
                        .map(|chip| chip.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                other => panic!("unexpected header line: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_header_is_the_description_then_one_column_of_fields() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/diffui-commit-info-test");
        let theme = ui.resolved_theme().spec();
        let details = RevisionDetails {
            commit_id: "abcdef0123456789abcdef0123456789abcdef01".to_owned(),
            change_id: Some("zyxwvutsrqponmlkzyxwvutsrqponmlk".to_owned()),
            bookmarks: vec!["main".to_owned()],
            author: SignatureInfo {
                name: "Ada Lovelace".to_owned(),
                email: "ada@example.com".to_owned(),
                timestamp: Some("2026-09-07 10:00:00 +0000".to_owned()),
            },
            committer: None,
            signature: None,
            description: "subject\n\nbody one\nbody two\n".to_owned(),
            parents: vec![ParentBrief {
                commit_id: "1111111111111111111111111111111111111111".to_owned(),
                change_id: Some("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk".to_owned()),
                summary: "the parent".to_owned(),
            }],
        };

        let lines = header_lines(&ui, &details, theme, true, 2000.0);
        let rendered = render(&lines);
        assert_eq!(
            rendered[..4],
            [
                "| body one",
                "| body two",
                "",
                "Author: Ada Lovelace <ada@example.com>"
            ]
        );
        assert!(rendered[4].starts_with("Date: Sep 7, 2026, 10:00  "));
        assert_eq!(
            rendered[5..],
            [
                "Change: zyxwvutsrqponmlkzyxwvutsrqponmlk",
                "Commit: abcdef0123456789abcdef0123456789abcdef01",
                "Parent: kkkkkkkk  the parent",
                "Bookmarks: main",
            ]
        );
        // The copy buttons copy whole ids; the parent's id opens it.
        let HeaderLine::Rich(commit) = &lines[6] else {
            panic!("the commit line is a field");
        };
        assert_eq!(
            commit.buttons[0].1,
            HeaderAction::Copy(details.commit_id.clone())
        );
        let HeaderLine::Rich(parent) = &lines[7] else {
            panic!("the parent line is a field");
        };
        assert_eq!(
            parent.link,
            Some((8, HeaderAction::Open(details.parents[0].commit_id.clone())))
        );

        // Folded, the diff pane's header is gone; the History view's stays.
        ui.commit_details_collapsed = true;
        assert!(header_lines(&ui, &details, theme, true, 2000.0).is_empty());
        assert_eq!(
            header_lines(&ui, &details, theme, false, 2000.0).len(),
            lines.len()
        );
    }
}
