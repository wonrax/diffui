//! The identity line under a revision's or an operation's title: its ids,
//! status badges, who made it and when. The diff pane's commit header and
//! the History view's operation and evolog headers all draw it through
//! [`view`], so ids, people and times read the same everywhere: ids in mono
//! with the graph's colours, then a person and a relative time, each behind
//! a small icon and set apart by space rather than by separator glyphs.

use crate::Cx;
use crate::ellipsis::Span;
use crate::theme::{Color, Font};
use diffui_core::history::relative_age;
use gpui_kit::{AnyElement, Window};

use crate::Diffui;
use crate::ellipsis;
use crate::theme::ThemeSpec;
use crate::typography::{self, Aligned};

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
    font: Font,
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
            spans.push(Span::new(" ".to_owned()));
        }
        spans.push(match part {
            Part::Prose(prose) => Span::new(prose),
            Part::Id(id) => Span::new(id).font(mono).color(theme.muted_text),
        });
    }
    ellipsis::Line::new(spans, font, size, color)
}

/// Stub: the meta_line view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn view(
    meta: Meta,
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("meta_line::view")
}

/// Stub: the meta_line view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn badge(
    label: &str,
    color: Color,
    ui: &Diffui,
    window: &mut Window,
    cx: &mut Cx,
) -> Aligned {
    crate::stub::aligned("meta_line::badge")
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
