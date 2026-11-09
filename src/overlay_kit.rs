//! Pieces the overlays share, so the palette, the open prompt, the activity
//! popover and the confirm dialogs read as one family: keycaps, the footer
//! strip of key hints, group headings, count chips and the warning callout.

use gpui_kit::prelude::*;
use gpui_kit::{Div, SharedString, div, px};

use crate::icons;
use crate::prose::Prose;
use crate::theme::{self, Color, Font, ThemeSpec, Weight, radius, text_size};
use crate::typography;

/// Lucide glyphs the overlays use that the shared catalog doesn't carry, from
/// the same pinned font (see `widgets/build.rs`).
pub(crate) mod glyph {
    pub(crate) const CHECK_CIRCLE: &str = "\u{e07c}"; // check-circle
    pub(crate) const X_CIRCLE: &str = "\u{e084}"; // circle-x
    pub(crate) const SLASH_CIRCLE: &str = "\u{e402}"; // circle-slash
    pub(crate) const PULL_REQUEST: &str = "\u{e0e5}"; // git-pull-request
    pub(crate) const EXTERNAL_LINK: &str = "\u{e0b9}"; // external-link
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
    footer_strip_rounded(theme, radius::XL - 1.0)
}

/// [`footer_strip`] for a card rounded by `radius + 1`.
pub(crate) fn footer_strip_rounded(theme: ThemeSpec, radius: f32) -> Div {
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
        .child(card_foot(theme, radius))
}

/// The inset fill of a modal card's footer, its bottom corners following the
/// card's squircle inside its 1px edge. The footer's first child.
fn card_foot(theme: ThemeSpec, radius: f32) -> impl IntoElement {
    crate::squircle::background(
        crate::squircle::Fill::new(crate::squircle::Corners::bottom(radius))
            .background(inset_fill(theme)),
        0.0,
    )
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

/// How a [`count_chip`] is tinted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Accent,
    Neutral,
    Danger,
}

/// A small tinted chip with a count or a state in it ("1 running"), placed on
/// its text's baseline.
pub(crate) fn count_chip(
    label: impl Into<SharedString>,
    tone: Tone,
    font: Font,
    theme: ThemeSpec,
) -> typography::Aligned {
    let font = theme::emphasis_font(font, Weight::MEDIUM);
    let (fill, ink, outline) = match tone {
        Tone::Accent => (theme::chip_background(theme.accent), theme.accent, None),
        Tone::Danger => (
            theme::chip_background(theme.removed_text),
            theme.removed_text,
            None,
        ),
        Tone::Neutral => (
            theme::TRANSPARENT,
            theme.subtle_text,
            Some(firm_border(theme)),
        ),
    };
    typography::Aligned::chip(
        typography::line(label, font, text_size::CAPTION, ink),
        font,
        text_size::CAPTION,
        [3.0, 7.0].into(),
        move |chip| {
            let chip = chip.bg(fill).rounded(px(radius::XS));
            match outline {
                Some(color) => chip.shadow(vec![theme::inset_ring(color)]),
                None => chip,
            }
        },
    )
}

/// A warning-tinted box with an alert glyph: something the reader should know
/// before they confirm, that doesn't stop them. `lead` takes the warning's
/// ink, `detail` (the consequence, in plain words) the body's.
pub(crate) fn callout(
    lead: &Prose,
    detail: Option<&Prose>,
    font: Font,
    mono: Font,
    theme: ThemeSpec,
) -> Div {
    let line = typography::TextBox::default_line(font, text_size::UI);
    // The glyph centres on the first line's caps.
    let glyph_top = ((line.cap_top() + line.baseline()) / 2.0 - CALLOUT_ICON / 2.0).max(0.0);
    let mut text = div()
        .flex_1()
        .min_w_0()
        .font(font.gpui())
        .text_size(px(text_size::UI))
        .line_height(px(text_size::UI * typography::DEFAULT_LINE_HEIGHT))
        .child(lead.view(font, mono, theme.note_text, theme));
    if let Some(detail) = detail {
        text = text.child(detail.view(font, mono, theme.muted_text, theme));
    }
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.0))
        .px(px(theme::space::LG))
        .py(px(10.0))
        .map(|callout| theme::squircle(callout, radius::MD, Some(theme.note_background), None))
        .child(div().flex_none().pt(px(glyph_top)).child(icons::icon(
            icons::ALERT_TRIANGLE,
            CALLOUT_ICON,
            theme.note_text,
        )))
        .child(text)
}

const CALLOUT_ICON: f32 = 14.0;

/// Width of a dialog that asks one question.
pub(crate) const DIALOG_WIDTH: f32 = 480.0;
/// A dialog's inset around its words, fields and rows.
pub(crate) const DIALOG_PAD: f32 = 20.0;
/// Gap between a dialog's title and the line under it.
const DIALOG_HEAD_GAP: f32 = 6.0;
/// Gap between the things in a dialog's body.
const DIALOG_BODY_GAP: f32 = 10.0;

/// Every modal dialog's anatomy: the title with a line of plain words under
/// it, a body of fields and rows, and a footer strip a step off the card with
/// the key hints on the left and the buttons on the right.
pub(crate) struct Dialog {
    width: f32,
    /// A fixed height, the body taking what the head and footer leave and
    /// scrolling out to the card's right edge.
    height: Option<f32>,
    head: Div,
    body: Vec<gpui_kit::AnyElement>,
    hints: Vec<gpui_kit::AnyElement>,
    buttons: Vec<gpui_kit::AnyElement>,
    font: Font,
    theme: ThemeSpec,
}

impl Dialog {
    pub(crate) fn new(title: &Prose, font: Font, mono: Font, theme: ThemeSpec) -> Self {
        let title_font = theme::emphasis_font(font, Weight::SEMIBOLD);
        Self {
            width: DIALOG_WIDTH,
            height: None,
            head: div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(DIALOG_HEAD_GAP))
                .child(
                    div()
                        .w_full()
                        .text_size(px(text_size::TITLE))
                        .child(title.view(title_font, mono, theme.text, theme)),
                ),
            body: Vec::new(),
            hints: Vec::new(),
            buttons: Vec::new(),
            font,
            theme,
        }
    }

    pub(crate) fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Fixes the card's height: the body fills the rest and is expected to
    /// scroll, so its right inset moves inside the scroll view, where the bar
    /// sits at the card's edge.
    pub(crate) fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }

    /// The plain words under the title: what happens, in a sentence.
    pub(crate) fn lede(mut self, text: &Prose, mono: Font) -> Self {
        if !text.is_empty() {
            self.head = self.head.child(
                div()
                    .w_full()
                    .text_size(px(text_size::BODY))
                    .line_height(px(text_size::BODY * 1.5))
                    .child(text.view(self.font, mono, self.theme.muted_text, self.theme)),
            );
        }
        self
    }

    /// Something in the body: a field, a row, a callout.
    pub(crate) fn child(mut self, child: impl IntoElement) -> Self {
        self.body.push(child.into_any_element());
        self
    }

    /// A key hint in the footer's left end.
    pub(crate) fn hint(mut self, keys: &[&str], label: impl Into<SharedString>) -> Self {
        self.hints
            .push(key_hint(keys, label, self.font, self.theme).into_any_element());
        self
    }

    /// Quiet words in the footer's left end, after the hints.
    pub(crate) fn note(mut self, note: impl IntoElement) -> Self {
        self.hints.push(note.into_any_element());
        self
    }

    /// The footer's buttons, Cancel first and the call to action last.
    pub(crate) fn buttons(
        mut self,
        buttons: impl IntoIterator<Item = gpui_kit::AnyElement>,
    ) -> Self {
        self.buttons.extend(buttons);
        self
    }

    pub(crate) fn build(self) -> Div {
        let theme = self.theme;
        let has_body = !self.body.is_empty();
        let head = self
            .head
            .pt(px(DIALOG_PAD))
            .px(px(DIALOG_PAD))
            .when(!has_body, |head| head.pb(px(DIALOG_PAD)));
        let fixed = self.height.is_some();
        let body = has_body.then(|| {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(DIALOG_BODY_GAP))
                .pl(px(DIALOG_PAD))
                .when(!fixed, |body| body.pr(px(DIALOG_PAD)))
                .when(fixed, |body| body.flex_1().min_h_0())
                .pt(px(theme::space::XL))
                .pb(px(DIALOG_PAD))
                .children(self.body)
        });
        let footer = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(crate::ui::BUTTON_GAP))
            .px(px(theme::space::XL))
            .py(px(theme::space::LG))
            .border_t_1()
            .border_color(theme.border)
            .child(card_foot(theme, radius::XL - 1.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(theme::space::LG))
                    .overflow_hidden()
                    .children(self.hints),
            )
            .children(self.buttons)
            .flex_none();
        theme::modal_card(div(), theme)
            .w(px(self.width))
            .when_some(self.height, |card, height| card.h(px(height)))
            .flex()
            .flex_col()
            .child(head.flex_none())
            .children(body)
            .child(footer)
    }
}

/// The footer strip inside an editor's well: key hints and notes on the left,
/// the buttons on the right, a step off the field above it.
pub(crate) fn well_footer(
    hints: Vec<gpui_kit::AnyElement>,
    buttons: impl IntoIterator<Item = gpui_kit::AnyElement>,
    theme: ThemeSpec,
) -> Div {
    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::ui::BUTTON_GAP))
        .pl(px(theme::space::LG))
        .pr(px(theme::space::MD))
        .py(px(theme::space::SM))
        .border_t_1()
        .border_color(theme.border)
        .child(crate::squircle::background(
            crate::squircle::Fill::new(crate::squircle::Corners::bottom(radius::CONTROL - 1.0))
                .background(inset_fill(theme)),
            0.0,
        ))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::LG))
                .overflow_hidden()
                .children(hints),
        )
        .children(buttons)
}

/// A quiet note among a footer's hints, in `color`.
pub(crate) fn hint_note(
    text: impl Into<SharedString>,
    color: Color,
    font: Font,
) -> gpui_kit::AnyElement {
    typography::centered(
        typography::line(text, font, text_size::CAPTION, color),
        font,
        text_size::CAPTION,
    )
    .into_any_element()
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
