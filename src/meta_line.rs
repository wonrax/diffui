//! The identity line under a revision's or an operation's title: its ids,
//! status badges, who made it and when. The diff pane's commit header and
//! the History view's operation and evolog headers all draw it through
//! [`view`], so ids, people and times read the same everywhere: ids in mono
//! with the graph's colours, then a person and a relative time, each behind
//! a small icon and set apart by space rather than by separator glyphs.

use diffui_core::history::{human_timestamp, now_millis, relative_age};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, ElementId, Window, div, px};

use crate::ellipsis::{self, Span};
use crate::theme::{Color, space, text_size};
use crate::typography::{self, Aligned};
use crate::ui::Look;
use crate::ui::Tip as _;
use crate::{Cx, Diffui, icons};

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

/// "5m ago", or "just now".
pub(crate) fn age_phrase(then: i64, now: i64) -> String {
    match relative_age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
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
    color: Color,
    look: Look,
) -> ellipsis::Line<'static> {
    let Look { theme, font, .. } = look;
    let mono = look.mono;
    // One rich text, so the mono ids sit on the prose's baseline. GPUI sizes a line as
    // a whole, so ids stay at the line's size rather than matching its cap height.
    let mut spans = Vec::new();
    for (index, part) in split_ids(description).into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::new(" ".to_owned()));
        }
        spans.push(match part {
            Part::Prose(prose) => Span::new(prose),
            Part::Id(id) => Span::new(id).font(mono).color(theme.muted_text),
        });
    }
    ellipsis::Line::new(spans, font, size, color)
}

pub(crate) fn view(
    meta: Meta,
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    // Mono ids, sans names and small badges share one baseline (see
    // `crate::typography`), not one box centre.
    let mut line: Vec<Aligned> = Vec::new();
    if !meta.ids.is_empty() || !meta.badges.is_empty() {
        let mut ids: Vec<Aligned> = Vec::new();
        for id in meta.ids {
            ids.push(id_text(id, ui));
        }
        for spec in meta.badges {
            ids.push(badge(spec.label, spec.color, ui, window, cx));
        }
        line.push(typography::group(ids, space::SM));
    }
    if let Some((name, detail)) = meta.who {
        let label = labelled(icons::USER, name, look);
        line.push(
            label
                .map(|label| {
                    div()
                        .flex()
                        .max_w(px(WHO_MAX_WIDTH))
                        .child(label)
                        .into_any_element()
                })
                .map(|label| hover("meta-who", label, detail, look)),
        );
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
            labelled(icons::CLOCK, label, look)
                .map(|label| hover("meta-when", label, detail, look)),
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
                    typography::line(
                        shorten_ids(&operation),
                        look.font,
                        text_size::BODY,
                        theme.muted_text,
                    )
                    .text_ellipsis(),
                    look.font,
                    text_size::BODY,
                )
                .shrink(),
            ],
            space::XS,
        ));
    }
    typography::baseline_row(line, space::LG).into_any_element()
}

/// A small filled badge, the look of the History view's `hidden` / `empty`
/// markers, ready to sit on a line's baseline.
pub(crate) fn badge(
    label: &str,
    color: Color,
    ui: &Diffui,
    _window: &mut Window,
    _cx: &mut Cx,
) -> Aligned {
    crate::ui::tag(
        label.to_owned(),
        color,
        crate::ui::TagSize::Small,
        ui.config.ui_font,
    )
    .aligned()
}

/// An id in mono, its bright prefix and the rest as one rich text, so the two
/// colours can't drift apart.
pub(crate) fn id_text(id: Id, ui: &Diffui) -> Aligned {
    let mono = ui.config.mono_font;
    let split = id
        .text
        .char_indices()
        .nth(id.bright)
        .map_or(id.text.len(), |(at, _)| at);
    let (bright, rest) = id.text.split_at(split);
    let spans = [
        Span::new(bright.to_owned()).color(id.bright_color),
        Span::new(rest.to_owned()).color(id.color),
    ];
    Aligned::text(
        div()
            .text_size(px(text_size::BODY))
            .whitespace_nowrap()
            .child(ellipsis::rich_text(&spans, mono, id.color)),
        mono,
        text_size::BODY,
    )
}

/// An icon and its label, centred on the label's caps.
fn labelled(glyph: &'static str, label: String, look: Look) -> Aligned {
    let Look { theme, .. } = look;
    let font = look.font;
    typography::group(
        [
            Aligned::glyph(icons::icon(glyph, ICON_SIZE, theme.subtle_text), ICON_SIZE),
            Aligned::text(
                typography::line(label, font, text_size::BODY, theme.muted_text).text_ellipsis(),
                font,
                text_size::BODY,
            )
            .shrink(),
        ],
        space::XS,
    )
}

fn hover(id: &'static str, content: AnyElement, detail: Option<String>, look: Look) -> AnyElement {
    match detail.filter(|detail| !detail.is_empty()) {
        Some(detail) => div()
            .id(ElementId::Name(id.into()))
            .flex()
            .min_w_0()
            .tip(detail, look)
            .child(content)
            .into_any_element(),
        None => content,
    }
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
