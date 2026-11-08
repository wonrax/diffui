//! The status bar: a quiet strip along the window's bottom edge, under every
//! pane, so what it says stays on screen whichever panels are hidden.
//!
//! Left to right: the bookmark `@` builds on, with how far it is ahead of and
//! behind its upstream (or "in sync"), then how many revisions the log holds.
//! A right-hand group is laid out for whatever joins them later.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::repository::Vcs;
use crate::theme::{self, Color, ThemeSpec, Weight, emphasis_font};
use crate::typography::{self, Aligned};
use crate::{Cx, Diffui, icons, ui};

/// The bar's height, without the hairline over it.
pub const HEIGHT: f32 = 26.0;

/// What the bar takes from the window's height: the bar and its hairline.
pub const TOTAL_HEIGHT: f32 = HEIGHT + 1.0;

const TEXT_SIZE: f32 = 11.5;
const ICON_SIZE: f32 = 13.0;
/// Between two items.
const ITEM_GAP: f32 = 16.0;

/// Group an integer with `,` thousands separators, e.g. 1410 -> "1,410".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub(crate) fn build_status_bar(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    if ui.tabs.is_empty() {
        return div().into_any_element();
    }
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let dim = theme.muted_text;
    let text = |value: String, font, color: Color| {
        Aligned::text(
            typography::line(value, font, TEXT_SIZE, color),
            font,
            TEXT_SIZE,
        )
    };
    let glyph = |glyph: &'static str| Aligned::glyph(icons::icon(glyph, ICON_SIZE, dim), TEXT_SIZE);
    let vcs = ui.active().repository.as_ref().map(|r| r.vcs);

    let mut left: Vec<Aligned> = Vec::new();
    if let Some(status) = &ui.active().session.branch_status {
        let mut bookmark = vec![
            glyph(if vcs == Some(Vcs::Git) {
                icons::GIT_BRANCH
            } else {
                icons::BOOKMARK
            }),
            text(status.branch.clone(), mono, dim),
        ];
        // Ahead/behind only mean something against a tracked upstream.
        if status.upstream.is_some() {
            let counts = emphasis_font(font, Weight::SEMIBOLD);
            if status.ahead == 0 && status.behind == 0 {
                bookmark.push(text("in sync".to_owned(), font, theme.subtle_text));
            }
            if status.ahead > 0 {
                bookmark.push(text(format!("↑{}", status.ahead), counts, theme.accent));
            }
            if status.behind > 0 {
                bookmark.push(text(
                    format!("↓{}", status.behind),
                    counts,
                    theme.modified_token,
                ));
            }
        }
        let bookmark = typography::group(bookmark, 5.0);
        left.push(match &status.upstream {
            // The upstream it's measured against rides along as a tooltip.
            Some(upstream) => {
                let upstream = format!("Compared with {upstream}");
                bookmark.map(move |bookmark| {
                    div()
                        .id("status-bookmark")
                        .tooltip(ui::tooltip(upstream, font, theme))
                        .child(bookmark)
                        .into_any_element()
                })
            }
            None => bookmark,
        });
    }
    // jj counts changes; git has only commits.
    let noun = match vcs {
        Some(Vcs::Git) => "commits",
        _ => "changes",
    };
    left.push(text(
        format!("{} {noun}", thousands(ui.active().session.commits.len())),
        font,
        dim,
    ));

    div()
        .flex()
        .flex_col()
        .w_full()
        .h(px(TOTAL_HEIGHT))
        .flex_none()
        .bg(theme.background)
        .child(crate::root::horizontal_divider(theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(ITEM_GAP))
                .h(px(HEIGHT))
                .px(px(theme::space::LG))
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(typography::baseline_row(left, ITEM_GAP)),
                )
                // Pinned right, for what joins the bar later.
                .child(div().flex_none()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn counts_group_their_thousands() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(1410), "1,410");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
