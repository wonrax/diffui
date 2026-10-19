//! The identity line under a revision's or an operation's title: its ids,
//! status badges, who made it and when. The diff pane's commit header and
//! the History view's operation and evolog headers all draw it through
//! [`view`], so ids, people and times read the same everywhere: ids in mono
//! with the graph's colours, then a person and a relative time, each behind
//! a small icon and set apart by space rather than by separator glyphs.

use diffui_core::history::{now_millis, relative_age};
use iced::{
    Background, Border, Color, Element, Length, alignment,
    widget::{Row, container, row, text},
};

use crate::theme::{ThemeSpec, chip_background, space, text_size};
use crate::{Diffui, Message, icons, ui};

/// Widest a name gets before it ellipsizes, so a long one can't push the
/// time off the line.
const WHO_MAX_WIDTH: f32 = 240.0;
const ICON_SIZE: f32 = 12.0;

/// An id in mono: the first `bright` characters in `bright_color` (jj's
/// shortest unique prefix, as the graph draws it), the rest in `color`.
pub(crate) struct Id {
    pub(crate) text: String,
    pub(crate) bright: usize,
    pub(crate) bright_color: Color,
    pub(crate) color: Color,
}

impl Id {
    pub(crate) fn plain(text: impl Into<String>, color: Color) -> Self {
        Self {
            text: text.into(),
            bright: 0,
            bright_color: color,
            color,
        }
    }
}

/// A status badge after the ids: `current`, `hidden`, `divergent`, ….
pub(crate) struct Badge {
    pub(crate) label: &'static str,
    pub(crate) color: Color,
}

#[derive(Default)]
pub(crate) struct Meta {
    pub(crate) ids: Vec<Id>,
    pub(crate) badges: Vec<Badge>,
    /// Who: a name, and what hovering it adds (an email).
    pub(crate) who: Option<(String, Option<String>)>,
    /// When: epoch millis when known, and the absolute time as the backend
    /// wrote it. Shown relative, absolute on hover; absolute when unknown.
    pub(crate) when: Option<(Option<i64>, String)>,
    /// The operation behind it (an evolog version's): its short id and
    /// description, last and ellipsized.
    pub(crate) operation: Option<(String, String)>,
}

pub(crate) fn view<'a>(meta: Meta, ui: &Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let mut line = Row::new()
        .spacing(space::LG)
        .align_y(alignment::Vertical::Center);
    if !meta.ids.is_empty() || !meta.badges.is_empty() {
        let mut ids = Row::new()
            .spacing(space::SM)
            .align_y(alignment::Vertical::Center);
        for id in meta.ids {
            ids = ids.push(id_text(id, ui));
        }
        for badge_spec in meta.badges {
            ids = ids.push(badge(badge_spec.label, badge_spec.color, ui));
        }
        line = line.push(ids);
    }
    if let Some((name, detail)) = meta.who {
        let label = container(labelled(icons::USER, name, ui, theme))
            .width(Length::Shrink.max(WHO_MAX_WIDTH));
        line = line.push(hover(label, detail, ui, theme));
    }
    if let Some((millis, absolute)) = meta.when {
        let (label, detail) = match millis {
            Some(millis) => (age_phrase(millis, now_millis()), Some(absolute)),
            None => (absolute, None),
        };
        line = line.push(hover(
            labelled(icons::CLOCK, label, ui, theme),
            detail,
            ui,
            theme,
        ));
    }
    if let Some((id, operation)) = meta.operation {
        line = line.push(
            row![
                icons::icon(icons::HISTORY, ICON_SIZE, theme.subtle_text),
                id_text(Id::plain(id, theme.subtle_text), ui),
                text(shorten_ids(&operation))
                    .size(text_size::UI)
                    .font(ui.config.ui_font)
                    .color(theme.muted_text)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End)
                    .width(Length::Fill),
            ]
            .spacing(space::XS)
            .align_y(alignment::Vertical::Center),
        );
    }
    line.into()
}

/// "5m ago", or "just now".
pub(crate) fn age_phrase(then: i64, now: i64) -> String {
    match relative_age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

/// A small filled badge, the look of the History view's `hidden` / `empty`
/// markers.
pub(crate) fn badge<'a>(label: &'a str, color: Color, ui: &Diffui) -> Element<'a, Message> {
    container(
        text(label)
            .size(text_size::BADGE)
            .font(ui.config.ui_font)
            .color(color),
    )
    .padding(iced::Padding::from([0, 5]))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(color))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: crate::chip::RADIUS.into(),
        },
        ..container::Style::default()
    })
    .into()
}

fn id_text<'a>(id: Id, ui: &Diffui) -> Element<'a, Message> {
    let split = id
        .text
        .char_indices()
        .nth(id.bright)
        .map_or(id.text.len(), |(at, _)| at);
    let (bright, rest) = id.text.split_at(split);
    let part = |value: &str, color: Color| {
        text(value.to_owned())
            .size(text_size::UI)
            .font(ui.config.mono_font)
            .color(color)
            .wrapping(text::Wrapping::None)
    };
    row![part(bright, id.bright_color), part(rest, id.color)].into()
}

fn labelled<'a>(
    glyph: &'static str,
    label: String,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    row![
        icons::icon(glyph, ICON_SIZE, theme.subtle_text),
        text(label)
            .size(text_size::UI)
            .font(ui.config.ui_font)
            .color(theme.muted_text)
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End),
    ]
    .spacing(space::XS)
    .align_y(alignment::Vertical::Center)
    .into()
}

fn hover<'a>(
    content: impl Into<Element<'a, Message>>,
    detail: Option<String>,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    match detail.filter(|detail| !detail.is_empty()) {
        Some(detail) => ui::tooltip(
            content,
            detail,
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ),
        None => content.into(),
    }
}

/// A run of an operation description: prose, or an id written out in full.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Prose(String),
    Id(String),
}

/// Split a jj operation description ("abandon commit 4fb31ffd8c…") into
/// prose and ids, each id cut to the length the app shows it at: 8 for a
/// commit, 12 for an operation.
pub(crate) fn split_ids(description: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut prose = String::new();
    for word in description.split_whitespace() {
        let is_id = word.len() >= 32 && word.chars().all(|c| c.is_ascii_hexdigit());
        if is_id {
            if !prose.is_empty() {
                parts.push(Part::Prose(std::mem::take(&mut prose)));
            }
            let short = if word.len() > 40 { 12 } else { 8 };
            parts.push(Part::Id(word[..short].to_owned()));
        } else {
            if !prose.is_empty() {
                prose.push(' ');
            }
            prose.push_str(word);
        }
    }
    if !prose.is_empty() {
        parts.push(Part::Prose(prose));
    }
    parts
}

/// [`split_ids`] flattened back into one string, for places that can only
/// take plain text.
pub(crate) fn shorten_ids(description: &str) -> String {
    split_ids(description)
        .into_iter()
        .map(|part| match part {
            Part::Prose(text) | Part::Id(text) => text,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// An operation description with its ids short and in mono, on one line
/// that ellipsizes its last run of prose.
pub(crate) fn description<'a>(
    description: &str,
    size: f32,
    font: iced::Font,
    color: Color,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let parts = split_ids(description);
    let last = parts.len().saturating_sub(1);
    // One space's width between runs, which are separate text widgets.
    let mut line = Row::new()
        .spacing((size * 0.28).round())
        .align_y(alignment::Vertical::Center)
        .width(Length::Fill)
        .clip(true);
    for (index, part) in parts.into_iter().enumerate() {
        let run = match part {
            Part::Prose(prose) => text(prose).font(font).color(color),
            Part::Id(id) => text(id).font(ui.config.mono_font).color(theme.muted_text),
        }
        .size(size)
        .wrapping(text::Wrapping::None);
        line = line.push(if index == last {
            run.ellipsis(text::Ellipsis::End).width(Length::Fill)
        } else {
            run
        });
    }
    line.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_descriptions_shorten_their_ids() {
        let commit = "4fb31ffd8ccf33acf4ebb0239b64ed2d8d8864d0";
        let operation = "a".repeat(128);
        assert_eq!(
            split_ids(&format!("abandon commit {commit}")),
            [
                Part::Prose("abandon commit".to_owned()),
                Part::Id("4fb31ffd".to_owned()),
            ]
        );
        assert_eq!(
            split_ids(&format!("revert operation {operation} and more")),
            [
                Part::Prose("revert operation".to_owned()),
                Part::Id("aaaaaaaaaaaa".to_owned()),
                Part::Prose("and more".to_owned()),
            ]
        );
        // Short hex-looking words are prose: "add", "deadbeef".
        assert_eq!(
            shorten_ids("snapshot working copy"),
            "snapshot working copy"
        );
        assert_eq!(shorten_ids("add deadbeef"), "add deadbeef");
    }
}
