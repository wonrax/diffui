//! The identity line under a revision's or an operation's title: its ids,
//! status badges, who made it and when. The diff pane's commit header and
//! the History view's operation and evolog headers all draw it through
//! [`view`], so ids, people and times read the same everywhere: ids in mono
//! with the graph's colours, then a person and a relative time, each behind
//! a small icon and set apart by space rather than by separator glyphs.

use diffui_core::history::{human_timestamp, now_millis, relative_age};
use iced::{
    Background, Border, Color, Element, Length, Padding,
    widget::{container, text},
};

use crate::ellipsis;
use crate::theme::{ThemeSpec, chip_background, space, text_size};
use crate::typography::{self, Aligned, TextBox};
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
    // Mono ids, sans names and small badges share one baseline (see
    // `crate::typography`), not one box centre.
    let mut line: Vec<Aligned<'a, Message>> = Vec::new();
    if !meta.ids.is_empty() || !meta.badges.is_empty() {
        let mut ids: Vec<Aligned<'a, Message>> = Vec::new();
        for id in meta.ids {
            ids.push(id_text(id, ui));
        }
        for badge_spec in meta.badges {
            ids.push(badge(badge_spec.label, badge_spec.color, ui));
        }
        line.push(typography::group(ids, space::SM));
    }
    if let Some((name, detail)) = meta.who {
        let label = labelled(icons::USER, name, ui, theme).map(|label| {
            container(label)
                .width(Length::Shrink.max(WHO_MAX_WIDTH))
                .into()
        });
        line.push(label.map(|label| hover(label, detail, ui, theme)));
    }
    if let Some((millis, absolute)) = meta.when {
        let (label, detail) = match millis {
            Some(millis) => (
                age_phrase(millis, now_millis()),
                Some(human_timestamp(&absolute).unwrap_or(absolute)),
            ),
            None => (absolute, None),
        };
        line.push(
            labelled(icons::CLOCK, label, ui, theme).map(|label| hover(label, detail, ui, theme)),
        );
    }
    if let Some((id, operation)) = meta.operation {
        line.push(typography::group(
            [
                Aligned::glyph(
                    icons::icon(icons::HISTORY, ICON_SIZE, theme.subtle_text),
                    ICON_SIZE,
                ),
                id_text(Id::plain(id, theme.subtle_text), ui),
                Aligned::text(
                    text(shorten_ids(&operation))
                        .size(text_size::UI)
                        .font(ui.config.ui_font)
                        .color(theme.muted_text)
                        .wrapping(text::Wrapping::None)
                        .ellipsis(text::Ellipsis::End),
                    ui.config.ui_font,
                    text_size::UI,
                ),
            ],
            space::XS,
        ));
    }
    typography::baseline_row(line, space::LG)
}

/// "5m ago", or "just now".
pub(crate) fn age_phrase(then: i64, now: i64) -> String {
    match relative_age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

/// A small filled badge, the look of the History view's `hidden` / `empty`
/// markers, ready to sit on a line's baseline.
pub(crate) fn badge<'a>(label: &'a str, color: Color, ui: &Diffui) -> Aligned<'a, Message> {
    let padding =
        typography::trimmed_padding(Padding::from([0, 5]), ui.config.ui_font, text_size::BADGE);
    Aligned::boxed(
        container(
            text(label)
                .size(text_size::BADGE)
                .font(ui.config.ui_font)
                .color(color),
        )
        .padding(padding)
        .style(move |_| container::Style {
            background: Some(Background::Color(chip_background(color))),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: crate::chip::RADIUS.into(),
            },
            ..container::Style::default()
        }),
        padding,
        TextBox::default_line(ui.config.ui_font, text_size::BADGE),
    )
}

/// An id in mono, its bright prefix and the rest as one rich text, so the
/// two colours can't drift apart.
pub(crate) fn id_text<'a>(id: Id, ui: &Diffui) -> Aligned<'a, Message> {
    let split = id
        .text
        .char_indices()
        .nth(id.bright)
        .map_or(id.text.len(), |(at, _)| at);
    let (bright, rest) = id.text.split_at(split);
    let spans = vec![
        iced::widget::span(bright.to_owned()).color(id.bright_color),
        iced::widget::span(rest.to_owned()).color(id.color),
    ];
    Aligned::text(
        iced::widget::rich_text::<(), Message, iced::Theme>(spans)
            .size(text_size::UI)
            .font(ui.config.mono_font)
            .wrapping(text::Wrapping::None),
        ui.config.mono_font,
        text_size::UI,
    )
}

/// An icon and its label, centred on the label's caps.
fn labelled<'a>(
    glyph: &'static str,
    label: String,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Aligned<'a, Message> {
    typography::group(
        [
            Aligned::glyph(icons::icon(glyph, ICON_SIZE, theme.subtle_text), ICON_SIZE),
            Aligned::text(
                text(label)
                    .size(text_size::UI)
                    .font(ui.config.ui_font)
                    .color(theme.muted_text)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End),
                ui.config.ui_font,
                text_size::UI,
            ),
        ],
        space::XS,
    )
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

/// An operation description with its ids short and in mono, as a line the
/// caller cuts to fit or wraps. The ids are sized so their caps stand as
/// tall as the prose's: a mono face at the same size runs taller.
pub(crate) fn description(
    description: &str,
    size: f32,
    font: iced::Font,
    color: Color,
    ui: &Diffui,
    theme: ThemeSpec,
) -> ellipsis::Line<'static> {
    let mono = ui.config.mono_font;
    let id_size = (size * typography::FontMetrics::of(font).cap_height
        / typography::FontMetrics::of(mono).cap_height)
        .min(size);
    // One rich text, so the mono ids sit on the prose's baseline.
    let mut spans = Vec::new();
    for (index, part) in split_ids(description).into_iter().enumerate() {
        if index > 0 {
            spans.push(iced::widget::span(" ".to_owned()));
        }
        spans.push(match part {
            Part::Prose(prose) => iced::widget::span(prose),
            Part::Id(id) => iced::widget::span(id)
                .font(mono)
                .size(id_size)
                .color(theme.muted_text),
        });
    }
    ellipsis::Line::new(spans, font, size, color)
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
