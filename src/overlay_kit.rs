//! Pieces the overlays share, so the palette, the open prompt, the activity
//! popover and the confirm dialogs read as one family: keycaps, the footer
//! strip of key hints, group headings, count chips and the warning callout.

use gpui_kit::prelude::*;
use gpui_kit::{Div, SharedString, div, px};

use crate::icons;
use crate::prose::Prose;
use crate::theme::{self, Color, Font, ThemeSpec, Weight, radius, text_size};
use crate::typography;
use crate::ui::{self, Look};

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

/// Height of a card's footer strip of key hints.
pub(crate) const FOOTER_HEIGHT: f32 = 36.0;
/// Height of a group heading in a list, its label sitting on the bottom.
pub(crate) const GROUP_HEIGHT: f32 = 28.0;

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
pub(crate) fn key_hint(keys: &[&str], label: impl Into<SharedString>, look: Look) -> Div {
    let Look { theme, font, .. } = look;
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::SM))
        .child(ui::chord(keys.iter().copied(), look))
        .child(typography::centered(
            typography::line(label, font, text_size::CAPTION, theme.subtle_text),
            font,
            text_size::CAPTION,
        ))
}

/// A footer hint for `commands` in `context`, its keys read from the keymap
/// (each command's first live chord, one cap per key), so a rebinding shows
/// here. `None` when none of them is bound.
pub(crate) fn bound_hint(
    ui: &crate::Diffui,
    context: crate::commands::Context,
    commands: &[crate::commands::CommandId],
    label: impl Into<SharedString>,
    look: Look,
) -> Option<Div> {
    let keys = bound_keys(ui, context, commands);
    if keys.is_empty() {
        return None;
    }
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    Some(key_hint(&keys, label, look))
}

/// The keys of `commands` in `context`: each command's first live chord, one
/// entry per key. Empty when none of them is bound.
pub(crate) fn bound_keys(
    ui: &crate::Diffui,
    context: crate::commands::Context,
    commands: &[crate::commands::CommandId],
) -> Vec<String> {
    commands
        .iter()
        .filter_map(|id| ui.keymap.live_chords(context, id).first().copied())
        .flat_map(|chord| chord_keys(&chord.display()))
        .collect()
}

/// Quiet text in a footer, without keys (the far end's note).
pub(crate) fn footer_note(text: impl Into<SharedString>, look: Look) -> Div {
    let Look { theme, font, .. } = look;
    typography::centered(
        typography::line(text, font, text_size::CAPTION, theme.subtle_text),
        font,
        text_size::CAPTION,
    )
}

/// How much room a [`footer`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Density {
    /// Key hints only, along a list card's bottom: the palette, the open
    /// prompt, the revset dropdown.
    Strip,
    /// A dialog's hints and its buttons.
    Dialog,
    /// Inside an editor's well, under its field.
    Well,
}

/// The footer along a card's bottom: a step off the card, ruled off what's
/// above it, its bottom corners following the card's, `corner` being the
/// card's radius inside its 1px edge. Put key hints in with `.child`; a child
/// with `ml_auto` sits at the far end. [`footer_with_buttons`] adds buttons.
pub(crate) fn footer(density: Density, corner: f32, theme: ThemeSpec) -> Div {
    let footer = div()
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(HINT_GAP))
        .overflow_hidden()
        .border_t_1()
        .border_color(theme.border)
        .child(card_foot(theme, corner));
    match density {
        Density::Strip => footer.h(px(FOOTER_HEIGHT)).px(px(14.0)),
        Density::Dialog => footer.px(px(theme::space::XL)).py(px(theme::space::LG)),
        Density::Well => footer
            .pl(px(theme::space::LG))
            .pr(px(theme::space::MD))
            .py(px(theme::space::SM)),
    }
}

/// A [`footer`] with `hints` on the left and `buttons` on the right.
pub(crate) fn footer_with_buttons(
    density: Density,
    corner: f32,
    hints: Vec<gpui_kit::AnyElement>,
    buttons: impl IntoIterator<Item = gpui_kit::AnyElement>,
    theme: ThemeSpec,
) -> Div {
    footer(density, corner, theme)
        .gap(px(crate::ui::BUTTON_GAP))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(HINT_GAP))
                .overflow_hidden()
                .children(hints),
        )
        .children(buttons)
}

/// Between two key hints in a footer.
const HINT_GAP: f32 = theme::space::LG;

/// The corner a modal card's footer follows: the card's, inside its edge.
pub(crate) const MODAL_FOOT: f32 = radius::inner(radius::MODAL, theme::HAIRLINE);

/// The inset fill of a card's footer, its bottom corners following the
/// card's squircle inside its 1px edge. The footer's first child.
fn card_foot(theme: ThemeSpec, radius: f32) -> impl IntoElement {
    crate::squircle::background(
        crate::squircle::Fill::new(crate::squircle::Corners::bottom(radius))
            .background(inset_fill(theme)),
        0.0,
    )
}

/// A list's group band: its [`crate::ui::section_label`] on the bottom of
/// its [`GROUP_HEIGHT`] so it hugs the rows it heads.
pub(crate) fn group_heading(label: impl Into<SharedString>, look: Look) -> Div {
    div()
        .w_full()
        .h(px(GROUP_HEIGHT))
        .flex_none()
        .flex()
        .flex_col()
        .justify_end()
        .pb(px(theme::space::XS))
        .child(typography::baseline_row(
            [ui::section_label(label, None, look)],
            0.0,
        ))
}

/// A warning card with an alert glyph: something the reader should know
/// before they confirm, that doesn't stop them. `lead` takes the warning's
/// ink, `detail` (the consequence, in plain words) the body's.
pub(crate) fn callout(lead: &Prose, detail: Option<&Prose>, look: Look) -> Div {
    let Look { theme, font, .. } = look;
    let text = |prose: &Prose, color: Color| {
        div()
            .font(font.gpui())
            .line_height(px(text_size::BODY * typography::DEFAULT_LINE_HEIGHT))
            .child(prose.view(look, color))
    };
    let mut banner = crate::ui::Banner::new(crate::ui::Tone::Warn)
        .icon(icons::ALERT_TRIANGLE)
        .child(text(lead, theme.note_text));
    if let Some(detail) = detail {
        banner = banner.child(text(detail, theme.muted_text));
    }
    banner.build(look)
}

/// Width of a dialog that asks one question.
pub(crate) const DIALOG_WIDTH: f32 = 480.0;
/// A dialog's inset around its words, fields and rows.
pub(crate) const DIALOG_PAD: f32 = theme::space::XL_XXL;
/// Gap between a dialog's title and the line under it.
const DIALOG_HEAD_GAP: f32 = 6.0;
/// Gap between the things in a dialog's body.
const DIALOG_BODY_GAP: f32 = theme::space::MD_LG;

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
    look: Look,
}

impl Dialog {
    pub(crate) fn new(title: &Prose, look: Look) -> Self {
        let title_font = theme::emphasis_font(look.font, Weight::SEMIBOLD);
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
                        .child(title.view(
                            Look {
                                font: title_font,
                                ..look
                            },
                            look.theme.text,
                        )),
                ),
            body: Vec::new(),
            hints: Vec::new(),
            buttons: Vec::new(),
            look,
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
    pub(crate) fn lede(mut self, text: &Prose) -> Self {
        if !text.is_empty() {
            self.head = self.head.child(
                div()
                    .w_full()
                    .text_size(px(text_size::BODY))
                    .line_height(px(text_size::BODY * 1.5))
                    .child(text.view(self.look, self.look.theme.muted_text)),
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
            .push(key_hint(keys, label, self.look).into_any_element());
        self
    }

    /// [`bound_hint`] in the footer's left end: nothing when `commands`
    /// aren't bound.
    pub(crate) fn bound_hint(
        mut self,
        ui: &crate::Diffui,
        context: crate::commands::Context,
        commands: &[crate::commands::CommandId],
        label: impl Into<SharedString>,
    ) -> Self {
        self.hints.extend(
            bound_hint(ui, context, commands, label, self.look).map(IntoElement::into_any_element),
        );
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
        let theme = self.look.theme;
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
        let footer =
            footer_with_buttons(Density::Dialog, MODAL_FOOT, self.hints, self.buttons, theme);
        theme::surface(div(), theme::Elevation::Modal, theme)
            .w(px(self.width))
            .when_some(self.height, |card, height| card.h(px(height)))
            .flex()
            .flex_col()
            .child(head.flex_none())
            .children(body)
            .child(footer)
    }
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
