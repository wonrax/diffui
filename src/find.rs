//! In-diff find. ⌘F / Ctrl+F opens a thin bar pinned to the top-right of
//! the diff viewport with a query field, case/regex toggles, match count,
//! and prev/next chevrons. Matches cover every line of every file in the
//! current revision's diff — the file list in the sidebar is purely a
//! "scroll to" affordance, so search needs to find hits the user couldn't
//! reach by selecting a single file.
//!
//! Recomputation is debounced 50 ms per keystroke via a monotonically
//! incrementing `query_version` — the scheduled `FindRecompute(version)`
//! checks the live version against its captured one and bails if the user
//! has typed past it. This keeps regex over large diffs (50k+ lines) from
//! re-running on every char.

use std::time::Duration;

use iced::{
    Background, Border, Color, Element, Length, Padding, Shadow, alignment,
    font::Weight,
    widget::{Space, button, column, container, row, text, text_input},
};
use regex::{Regex, RegexBuilder};

use crate::theme::{ThemeSpec, ghost_button_style, input_style, text_size};
use crate::{Diffui, Message};
use diffui_core::DiffDocument;

pub const FIND_INPUT_ID: &str = "find-input";

/// Debounce window per keystroke. 50ms is short enough that the bar feels
/// instant, long enough that regex re-runs don't pile up while the user is
/// burst-typing.
pub const DEBOUNCE: Duration = Duration::from_millis(50);

/// Messages from the in-diff find bar, nested under [`Message::Find`].
#[derive(Debug, Clone)]
pub enum FindMessage {
    /// Open the find bar (⌘F / Ctrl+F).
    Open,
    Close,
    QueryChanged(String),
    /// Fired after the debounce delay; the version cookie drops stale results.
    Recompute(u64),
    ToggleCase,
    ToggleRegex,
    /// Enter: advance to the next match (wraps around).
    Next,
    /// Shift+Enter: advance to the previous match.
    Prev,
}

#[derive(Debug, Clone, Default)]
pub struct FindState {
    pub query: String,
    pub case_sensitive: bool,
    pub regex: bool,
    /// Matches in document order (file 0 first → file N last). Empty when
    /// the query yields no hits or hasn't been computed yet.
    pub matches: Vec<FindMatch>,
    /// Index of the active match, or `None` when there are no matches.
    pub active: Option<usize>,
    /// Bumped each time `active` changes so `DiffView` knows to scroll the
    /// active match into view on the next render.
    pub scroll_token: u64,
    /// Compile error for the current regex (only set when `regex` is on).
    pub error: Option<String>,
    /// Monotonic counter. Bumped on every query/toggle change; the
    /// debounced recompute carries a snapshot and drops itself if the
    /// version has moved past it.
    pub query_version: u64,
}

#[derive(Debug, Clone)]
pub struct FindMatch {
    pub file_index: usize,
    pub hunk_index: usize,
    pub line_index: usize,
    /// Byte range within `DiffLine::content`. Inclusive-start, exclusive-end.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Compute matches for `state.query` across every line in `document`.
/// Pure function — caller writes the result back onto `state.matches`. We
/// avoid borrowing `state` mutably so the caller can decide how to merge
/// (e.g. preserve `active` when widths stay stable; reset to 0 when the
/// hit list changes).
pub fn compute_matches(
    state: &FindState,
    document: &DiffDocument,
) -> (Vec<FindMatch>, Option<String>) {
    if state.query.is_empty() {
        return (Vec::new(), None);
    }

    let matcher = match build_matcher(&state.query, state.case_sensitive, state.regex) {
        Ok(m) => m,
        Err(err) => return (Vec::new(), Some(err)),
    };

    let mut out = Vec::new();
    for (file_index, file) in document.files.iter().enumerate() {
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            for (line_index, line) in hunk.lines.iter().enumerate() {
                for m in matcher.find_iter(&line.content) {
                    out.push(FindMatch {
                        file_index,
                        hunk_index,
                        line_index,
                        byte_start: m.start,
                        byte_end: m.end,
                    });
                }
            }
        }
    }

    (out, None)
}

enum Matcher {
    Regex(Regex),
    Literal {
        needle: String,
        case_sensitive: bool,
    },
}

impl Matcher {
    fn find_iter<'a>(&'a self, hay: &'a str) -> Box<dyn Iterator<Item = MatchSpan> + 'a> {
        match self {
            Self::Regex(re) => Box::new(re.find_iter(hay).map(|m| MatchSpan {
                start: m.start(),
                end: m.end(),
            })),
            Self::Literal {
                needle,
                case_sensitive,
            } => Box::new(LiteralIter::new(hay, needle, *case_sensitive)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MatchSpan {
    start: usize,
    end: usize,
}

struct LiteralIter<'a> {
    hay: &'a str,
    needle: &'a str,
    case_sensitive: bool,
    pos: usize,
    // Pre-lowered copies live here when case-insensitive so the scan
    // operates over `&str` byte ranges that still align with the original.
    hay_lower: Option<String>,
    needle_lower: Option<String>,
}

impl<'a> LiteralIter<'a> {
    fn new(hay: &'a str, needle: &'a str, case_sensitive: bool) -> Self {
        let (hay_lower, needle_lower) = if case_sensitive {
            (None, None)
        } else {
            // Lowercasing changes byte lengths only for non-ASCII chars
            // (e.g. 'İ' → "i\u{307}"). Since we'd then have a different
            // byte alignment, we restrict literal mode to a simple scan
            // over `make_ascii_lowercase` clones — guaranteed to preserve
            // byte indices. For non-ASCII case folding, the user can flip
            // on regex mode with `(?i)`.
            let mut h = hay.to_owned();
            h.make_ascii_lowercase();
            let mut n = needle.to_owned();
            n.make_ascii_lowercase();
            (Some(h), Some(n))
        };
        Self {
            hay,
            needle,
            case_sensitive,
            pos: 0,
            hay_lower,
            needle_lower,
        }
    }
}

impl Iterator for LiteralIter<'_> {
    type Item = MatchSpan;

    fn next(&mut self) -> Option<Self::Item> {
        if self.needle.is_empty() {
            return None;
        }
        let (h, n) = if self.case_sensitive {
            (self.hay, self.needle)
        } else {
            (
                self.hay_lower.as_deref().unwrap_or(self.hay),
                self.needle_lower.as_deref().unwrap_or(self.needle),
            )
        };
        if self.pos >= h.len() {
            return None;
        }
        let slice = &h[self.pos..];
        let idx = slice.find(n)?;
        let start = self.pos + idx;
        let end = start + n.len();
        // Advance past this match. Use `max(start + 1)` so a zero-width
        // needle (shouldn't happen with our guard above, but defensive)
        // doesn't loop.
        self.pos = end.max(start + 1);
        Some(MatchSpan { start, end })
    }
}

fn build_matcher(query: &str, case_sensitive: bool, regex: bool) -> Result<Matcher, String> {
    if regex {
        let re = RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| format!("{}", e))?;
        Ok(Matcher::Regex(re))
    } else {
        Ok(Matcher::Literal {
            needle: query.to_owned(),
            case_sensitive,
        })
    }
}

/// Build the find bar pinned to the top-right of the diff viewport.
/// Returns an empty placeholder when `find` is `None` so it can be
/// unconditionally stacked into the diff panel.
pub fn build_overlay<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Element<'a, Message> {
    let Some(state) = &ui.find else {
        return Space::new().into();
    };

    let count_label = if state.matches.is_empty() {
        if state.query.is_empty() {
            String::new()
        } else if state.error.is_some() {
            "—".to_owned()
        } else {
            "0 of 0".to_owned()
        }
    } else {
        let active = state.active.map(|i| i + 1).unwrap_or(0);
        format!("{} of {}", active, state.matches.len())
    };

    let input = text_input("Find in diff", &state.query)
        .id(FIND_INPUT_ID)
        .padding(Padding::from([4, 8]))
        .size(text_size::BODY)
        .font(ui.config.ui_font)
        .on_input(|q| Message::Find(FindMessage::QueryChanged(q)))
        // Intentionally no `on_submit` — that would route Enter to FindNext
        // unconditionally and steal Shift+Enter from the keyboard
        // subscription, which is what handles forward/backward.
        .width(Length::Fixed(220.0))
        .style(move |_, _| input_style(theme));

    let case_button = toggle_button("Aa", state.case_sensitive, theme, ui.config.ui_font)
        .on_press(Message::Find(FindMessage::ToggleCase));
    let regex_button = toggle_button(".*", state.regex, theme, ui.config.mono_font)
        .on_press(Message::Find(FindMessage::ToggleRegex));

    let prev_button =
        nav_button("‹", theme, ui.config.ui_font).on_press(Message::Find(FindMessage::Prev));
    let next_button =
        nav_button("›", theme, ui.config.ui_font).on_press(Message::Find(FindMessage::Next));
    let close_button =
        nav_button("✕", theme, ui.config.ui_font).on_press(Message::Find(FindMessage::Close));

    // Each row item is wrapped in a container that asks for
    // `Length::Shrink` height + centered Y. iced's `Row::align_y` only
    // applies to children that *don't* fight back with their own height,
    // and `text_input` defaults to Length::Fill height, which pulls the
    // row to its full available height and leaves the buttons stuck at
    // the top. Wrapping forces a known, content-sized cell.
    let bar = row![
        centered_cell(input.into()),
        Space::new().width(Length::Fixed(6.0)),
        centered_cell(case_button.into()),
        centered_cell(regex_button.into()),
        Space::new().width(Length::Fixed(10.0)),
        centered_cell(
            text(count_label)
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(ui.config.ui_font)
                .into(),
        ),
        Space::new().width(Length::Fill),
        centered_cell(prev_button.into()),
        centered_cell(next_button.into()),
        Space::new().width(Length::Fixed(4.0)),
        centered_cell(close_button.into()),
    ]
    .spacing(4)
    .align_y(alignment::Vertical::Center);

    // Square, shadowless card flush against the bottom of the stats bar.
    // The diff panel already provides a `+N -N · files` strip above, so
    // the find bar visually reads as an extension of that strip rather
    // than as a floating popover.
    let bar_card = container(bar)
        .padding(Padding::from([6, 10]))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        });

    let error_line: Element<'_, Message> = if let Some(err) = &state.error {
        container(
            text(format!("Regex error: {err}"))
                .size(text_size::CAPTION)
                .color(theme.removed_text)
                .font(ui.config.ui_font),
        )
        .padding(Padding::from([4, 10]))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 0.0.into(),
            },
            ..container::Style::default()
        })
        .into()
    } else {
        Space::new().into()
    };

    let stack = column![bar_card, error_line]
        .align_x(alignment::Horizontal::Right)
        .spacing(0);

    // Flush with the diff viewport's top edge (no padding) and pulled to
    // the right so it sits under the stats bar's right edge.
    container(stack)
        .width(Length::Fill)
        .align_x(alignment::Horizontal::Right)
        .into()
}

fn centered_cell<'a>(element: Element<'a, Message>) -> Element<'a, Message> {
    container(element)
        .align_y(alignment::Vertical::Center)
        .height(Length::Shrink)
        .into()
}

fn toggle_button<'a>(
    label: &'a str,
    on: bool,
    theme: ThemeSpec,
    font: iced::Font,
) -> button::Button<'a, Message> {
    let bg = if on {
        Color {
            a: 0.18,
            ..theme.accent
        }
    } else {
        theme.panel_background
    };
    let txt_color = if on { theme.accent } else { theme.muted_text };
    button(
        container(
            text(label)
                .size(text_size::CAPTION)
                // See `emphasis_font` doc — Medium is only applied on
                // explicitly-named families so the generic default
                // doesn't end up rendering tofu on macOS.
                .font(crate::theme::emphasis_font(font, Weight::Medium))
                .color(txt_color),
        )
        .padding(Padding::from([3, 6])),
    )
    .padding(0)
    .style(move |_, _| button::Style {
        background: Some(Background::Color(bg)),
        text_color: txt_color,
        border: Border {
            width: 1.0,
            color: if on { theme.accent } else { theme.border },
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    })
}

fn nav_button<'a>(
    label: &'a str,
    theme: ThemeSpec,
    font: iced::Font,
) -> button::Button<'a, Message> {
    button(
        container(
            text(label)
                .size(text_size::BODY)
                .font(font)
                .color(theme.muted_text),
        )
        .padding(Padding::from([2, 6])),
    )
    .padding(0)
    .style(move |_, status| ghost_button_style(theme, status))
}
