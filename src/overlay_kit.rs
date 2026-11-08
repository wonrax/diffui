//! Pieces the overlays share, so the palette, the open prompt, the activity
//! popover and the confirm dialogs read as one family: keycaps, the footer
//! strip of key hints, group headings, count chips and the warning callout.

use gpui_kit::prelude::*;
use gpui_kit::{Div, SharedString, div, px};

use crate::theme::{self, Color, Font, ThemeSpec, Weight, radius, text_size};
use crate::typography;

/// Lucide glyphs the overlays use that the shared catalog doesn't carry, from
/// the same pinned font (see `widgets/build.rs`).
pub(crate) mod glyph {
    pub(crate) const CHECK_CIRCLE: &str = "\u{e07c}"; // check-circle
    pub(crate) const PULL_REQUEST: &str = "\u{e0e5}"; // git-pull-request
    pub(crate) const BOOKMARK: &str = "\u{e060}"; // bookmark
    pub(crate) const SUN: &str = "\u{e178}"; // sun
    pub(crate) const MOON: &str = "\u{e11e}"; // moon
    pub(crate) const MONITOR: &str = "\u{e11d}"; // monitor
    pub(crate) const EYE: &str = "\u{e0ba}"; // eye
    pub(crate) const SETTINGS: &str = "\u{e154}"; // settings
    pub(crate) const KEYBOARD: &str = "\u{e284}"; // keyboard
    pub(crate) const FILE_TEXT: &str = "\u{e0cc}"; // file-text
    pub(crate) const GIT_COMMIT: &str = "\u{e0e3}"; // git-commit-horizontal
}

/// Side of a keycap: the box a single key sits in.
pub(crate) const KEYCAP: f32 = 18.0;
/// Gap between the keys of one chord.
const KEYCAP_GAP: f32 = 3.0;
/// Height of a card's footer strip of key hints.
pub(crate) const FOOTER_HEIGHT: f32 = 36.0;
/// Height of a group heading in a list, its label sitting on the bottom.
pub(crate) const GROUP_HEIGHT: f32 = 28.0;

/// The hairline a keycap and a raised control draw: a step firmer than the
/// panes' own borders, so a key reads as a thing to press.
pub(crate) fn firm_border(theme: ThemeSpec) -> Color {
    theme::mix(theme.border, theme.subtle_text, 0.18)
}

/// The fill of a card's footer strip and of quiet wells inside a card: a step
/// off the card itself.
pub(crate) fn inset_fill(theme: ThemeSpec) -> Color {
    theme.file_header
}

/// An empty one-line text box in `font` at `size` and `color`, for a rich
/// text child (runs of other weights or colours) to fill: what
/// [`typography::line`] is for plain text.
pub(crate) fn rich_line(font: Font, size: f32, color: Color) -> Div {
    div()
        .font(font.gpui())
        .text_size(px(size))
        .line_height(px(size * typography::DEFAULT_LINE_HEIGHT))
        .text_color(color)
        .whitespace_nowrap()
}

/// One key, in its own small box.
pub(crate) fn keycap(key: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Div {
    let font = theme::emphasis_font(font, Weight::MEDIUM);
    div()
        .flex_none()
        .h(px(KEYCAP))
        .min_w(px(KEYCAP))
        .px(px(5.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::XS))
        .bg(theme.panel_background_elevated)
        .border_1()
        .border_color(firm_border(theme))
        .child(typography::centered(
            typography::line(key, font, text_size::BADGE, theme.subtle_text),
            font,
            text_size::BADGE,
        ))
}

/// A chord as one keycap per key.
pub(crate) fn keycaps<S: Into<SharedString>>(
    keys: impl IntoIterator<Item = S>,
    font: Font,
    theme: ThemeSpec,
) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(KEYCAP_GAP));
    for key in keys {
        row = row.child(keycap(key, font, theme));
    }
    row
}

/// The keys of a chord as the keymap spells it (`⌘⇧K`, `Ctrl+Shift+K`), one
/// per cap.
pub(crate) fn chord_keys(display: &str) -> Vec<String> {
    const MODIFIERS: [char; 4] = ['\u{2303}', '\u{2325}', '\u{21e7}', '\u{2318}'];
    let mut keys = Vec::new();
    let mut rest = display;
    while let Some(c) = rest.chars().next().filter(|c| MODIFIERS.contains(c)) {
        keys.push(c.to_string());
        rest = &rest[c.len_utf8()..];
    }
    if rest.is_empty() {
        return keys;
    }
    if keys.is_empty() && rest.chars().count() > 1 && rest.contains('+') {
        // `Ctrl++`: the last `+` is the key itself.
        let (head, key) = match rest.strip_suffix("++") {
            Some(head) => (head, "+"),
            None => rest.rsplit_once('+').unwrap_or(("", rest)),
        };
        keys.extend(
            head.split('+')
                .filter(|part| !part.is_empty())
                .map(str::to_owned),
        );
        keys.push(key.to_owned());
        return keys;
    }
    keys.push(rest.to_owned());
    keys
}

/// A footer hint: the keys, then what they do.
pub(crate) fn key_hint(
    keys: &[&str],
    label: impl Into<SharedString>,
    font: Font,
    theme: ThemeSpec,
) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::SM))
        .child(keycaps(keys.iter().copied(), font, theme))
        .child(typography::centered(
            typography::line(label, font, text_size::CAPTION, theme.subtle_text),
            font,
            text_size::CAPTION,
        ))
}

/// Quiet text in a footer, without keys (the far end's note).
pub(crate) fn footer_note(text: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Div {
    typography::centered(
        typography::line(text, font, text_size::CAPTION, theme.subtle_text),
        font,
        text_size::CAPTION,
    )
}

/// The strip of key hints along a modal card's bottom: a step off the card,
/// rounded into its corners. Put the hints in with `.child`; a child with
/// `ml_auto` sits at the far end.
pub(crate) fn footer_strip(theme: ThemeSpec) -> Div {
    div()
        .w_full()
        .h(px(FOOTER_HEIGHT))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XL))
        .px(px(14.0))
        .overflow_hidden()
        .bg(inset_fill(theme))
        .rounded_b(px(radius::XL - 1.0))
}

/// A list's group heading: sentence case, semibold, on the bottom of its
/// [`GROUP_HEIGHT`] band so it hugs the rows it heads.
pub(crate) fn group_heading(label: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Div {
    let font = theme::emphasis_font(font, Weight::SEMIBOLD);
    div()
        .w_full()
        .h(px(GROUP_HEIGHT))
        .flex_none()
        .flex()
        .flex_col()
        .justify_end()
        .pb(px(theme::space::XS))
        .child(typography::line(
            label,
            font,
            text_size::UI,
            theme.muted_text,
        ))
}

#[cfg(test)]
mod tests {
    use super::chord_keys;

    #[test]
    fn a_chord_splits_into_one_cap_per_key() {
        assert_eq!(chord_keys("\u{2318}K"), ["\u{2318}", "K"]);
        assert_eq!(
            chord_keys("\u{2325}\u{21e7}\u{2318}Z"),
            ["\u{2325}", "\u{21e7}", "\u{2318}", "Z"]
        );
        assert_eq!(chord_keys("Ctrl+Shift+K"), ["Ctrl", "Shift", "K"]);
        assert_eq!(chord_keys("Ctrl++"), ["Ctrl", "+"]);
        assert_eq!(chord_keys("+"), ["+"]);
        assert_eq!(chord_keys("esc"), ["esc"]);
        assert_eq!(chord_keys("r"), ["r"]);
    }
}
