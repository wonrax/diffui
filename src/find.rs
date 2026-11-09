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

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};
use regex::{Regex, RegexBuilder};
use std::time::Duration;

use crate::diff_view::FindMatch;
use crate::theme::{self, Font, ThemeSpec, Weight, text_size};
use crate::{Action, Cx, Diffui, icons};
use diffui_core::DiffFile;

pub const FIND_INPUT_ID: &str = "find-input";

/// Debounce window per keystroke. 50ms is short enough that the bar feels
/// instant, long enough that regex re-runs don't pile up while the user is
/// burst-typing.
pub const DEBOUNCE: Duration = Duration::from_millis(50);

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

/// Compute matches for `state.query` across every line of `files` (the diff
/// document's files, or the source browser's single loaded file).
/// Pure function — caller writes the result back onto `state.matches`. We
/// avoid borrowing `state` mutably so the caller can decide how to merge
/// (e.g. preserve `active` when widths stay stable; reset to 0 when the
/// hit list changes).
pub fn compute_matches(state: &FindState, files: &[DiffFile]) -> (Vec<FindMatch>, Option<String>) {
    if state.query.is_empty() {
        return (Vec::new(), None);
    }

    let matcher = match build_matcher(&state.query, state.case_sensitive, state.regex) {
        Ok(m) => m,
        Err(err) => return (Vec::new(), Some(err)),
    };

    let mut out = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            for (line_index, line) in hunk.lines.iter().enumerate() {
                for m in matcher.find_iter(&line.content) {
                    // A zero-width pattern (`x*`, `^`, `\b`) matches at every
                    // position in the line, which highlights nothing, reports a
                    // count in the millions, and makes the painter's per-row
                    // match scan O(rows × bytes). The iterator already steps
                    // past an empty match, so dropping them is enough.
                    if m.end == m.start {
                        continue;
                    }
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

/// Find-bar control height.
const CONTROL: crate::ui::Size = crate::ui::Size::Compact;
/// Fits "999 of 999" without the arrows beside it moving.
const COUNT_WIDTH: f32 = 56.0;
const CARD_MAX_WIDTH: f32 = 460.0;

/// The find bar pinned to the top-right of the diff viewport. Empty when `find` is
/// `None`, so it can be unconditionally stacked into the diff panel.
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(state) = ui.active().find() else {
        return div().into_any_element();
    };

    let count_label = if state.matches.is_empty() {
        if state.query.is_empty() {
            String::new()
        } else if state.error.is_some() {
            "\u{2014}".to_owned()
        } else {
            "0 of 0".to_owned()
        }
    } else {
        let active = state.active.map(|i| i + 1).unwrap_or(0);
        format!("{} of {}", active, state.matches.len())
    };

    // Intentionally no `on_submit`: that would route Enter to FindNext unconditionally and
    // steal Shift+Enter from the keymap, which is what handles forward/backward.
    let input = div()
        .flex_1()
        .min_w_0()
        .h(px(CONTROL.height()))
        .child(crate::field::filter_field(
            theme,
            ui.config.ui_font,
            crate::field::FilterField {
                id: FIND_INPUT_ID,
                leading_icon: Some(icons::SEARCH),
                placeholder: "Find in diff",
                value: &state.query,
                on_input: |ui, query, cx| ui.find_on_query_changed(query, cx),
                on_submit: None,
                caret: None,
            },
            window,
            cx,
        ));

    let case_button = toggle_button(
        "find-case",
        "Aa",
        state.case_sensitive,
        theme,
        ui.config.ui_font,
        cx,
    )
    .on_click(Diffui::listen(
        cx,
        |this, _: &gpui_kit::ClickEvent, _, _| {
            this.find_on_toggle_case();
        },
    ));
    let regex_button = toggle_button(
        "find-regex",
        ".*",
        state.regex,
        theme,
        ui.config.mono_font,
        cx,
    )
    .on_click(Diffui::listen(
        cx,
        |this, _: &gpui_kit::ClickEvent, _, _| {
            this.find_on_toggle_regex();
        },
    ));

    let nav = |id: &'static str, glyph, action, cx: &mut Cx| {
        crate::ui::icon_button(id, glyph, theme.muted_text, CONTROL, theme, cx)
            .on_click(Diffui::on_action(action, cx))
    };
    let prev_button = nav("find-prev", icons::CHEVRON_UP, Action::FindPrevious, cx);
    let next_button = nav("find-next", icons::CHEVRON_DOWN, Action::FindNext, cx);
    let close_button = nav("find-close", icons::CLOSE, Action::CloseFind, cx);
    let cluster = || {
        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(theme::space::XXS))
    };

    // Every control is one step tall, so the row centers them without help; the count gets
    // a fixed slot so stepping through matches doesn't nudge the arrows sideways as its
    // digits change.
    let bar = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(CONTROL.height()))
        .gap(px(theme::space::SM))
        .child(input)
        .child(cluster().child(case_button).child(regex_button))
        .child(
            div()
                .w(px(COUNT_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .child(crate::typography::centered(
                    crate::typography::line(
                        count_label,
                        ui.config.ui_font,
                        text_size::CAPTION,
                        theme.subtle_text,
                    ),
                    ui.config.ui_font,
                    text_size::CAPTION,
                )),
        )
        .child(
            cluster()
                .child(prev_button)
                .child(next_button)
                .child(close_button),
        );

    // Floating rounded card hovering over the diff's top-right corner: shares the popover
    // elevation language so it reads as a transient control, not as pane chrome. A regex
    // error joins inside the card.
    let mut card = theme::popover(div(), theme)
        .id("find-bar")
        .flex()
        .flex_col()
        .gap(px(theme::space::XS))
        // The field flexes, so the card narrows with the pane instead of clipping its
        // trailing buttons.
        .w_full()
        .max_w(px(CARD_MAX_WIDTH))
        .p(px(theme::space::SM))
        .occlude()
        .child(bar);
    if let Some(err) = &state.error {
        card = card.child(crate::typography::line(
            format!("Regex error: {err}"),
            ui.config.ui_font,
            text_size::CAPTION,
            theme.removed_text,
        ));
    }

    // Inset from the pane's top-right corner like the diff's own headers, which also keeps
    // the card clear of the overlay scrollbar.
    div()
        .absolute()
        .top(px(theme::space::MD))
        .left(px(crate::diff_view::HEADER_HORIZONTAL_PADDING))
        .right(px(crate::diff_view::HEADER_HORIZONTAL_PADDING))
        .flex()
        .justify_end()
        .child(card)
        .into_any_element()
}

/// A match-option toggle: the toolbar toggles' look, quiet at rest, an accent wash and
/// accent label while on.
fn toggle_button(
    id: &'static str,
    label: &'static str,
    on: bool,
    theme: ThemeSpec,
    font: Font,
    cx: &mut Cx,
) -> crate::ui::Button {
    let side = px(CONTROL.height());
    let font = theme::emphasis_font(font, Weight::MEDIUM);
    let mut button = crate::ui::button(id, crate::ui::Variant::Ghost, CONTROL, theme, cx)
        .size(side)
        .p(px(0.0));
    if on {
        button = button.resting(theme::chip_background(theme.accent), None);
    }
    button.child(crate::typography::line(
        label,
        font,
        text_size::CAPTION,
        if on { theme.accent } else { theme.muted_text },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diffui_core::{DiffFileStatus, DiffHunkView, DiffLine, DiffLineKind};

    fn file(lines: &[&str]) -> DiffFile {
        DiffFile {
            path: "f.rs".to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: vec![DiffHunkView {
                header: "@@".to_owned(),
                lines: lines
                    .iter()
                    .map(|content| DiffLine {
                        kind: DiffLineKind::Context,
                        old_line: None,
                        new_line: None,
                        content: (*content).to_owned(),
                        syntax: Vec::new(),
                        emphasis: Vec::new(),
                    })
                    .collect(),
            }],
            additions: 0,
            deletions: 0,
            collapsed: None,
            blob: None,
            mode_change: None,
        }
    }

    fn matches(query: &str, regex: bool, files: &[DiffFile]) -> Vec<(usize, usize)> {
        let state = FindState {
            query: query.to_owned(),
            regex,
            ..Default::default()
        };
        let (found, error) = compute_matches(&state, files);
        assert_eq!(error, None);
        found.iter().map(|m| (m.byte_start, m.byte_end)).collect()
    }

    #[test]
    fn zero_width_regex_matches_are_dropped() {
        let files = vec![file(&["alpha", "beta"])];
        // `x*`, `^` and `\b` all match at positions where they consume
        // nothing: keeping those would report a hit per byte, highlight none
        // of them, and drag the painter down with the count.
        assert!(matches("x*", true, &files).is_empty());
        assert!(matches("^", true, &files).is_empty());
        assert!(matches(r"\b", true, &files).is_empty());
        // A pattern that can match empty still reports the non-empty hits.
        assert_eq!(matches("a*", true, &files), vec![(0, 1), (4, 5), (3, 4)]);
    }

    #[test]
    fn literal_and_regex_matches_are_found() {
        let files = vec![file(&["alpha alpha", "beta"])];
        assert_eq!(matches("alpha", false, &files), vec![(0, 5), (6, 11)]);
        assert_eq!(matches("a.pha", true, &files), vec![(0, 5), (6, 11)]);
    }
}
