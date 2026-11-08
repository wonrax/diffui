//! The one chip component: small rounded-rect label used for bookmark /
//! workspace / status indicators in the sidebar's commit rows, the file list's
//! A/M/D badges, and the diff view's revision-header bookmarks and file-header
//! status. Every chip goes through [`view`] so they share geometry, text
//! treatment, and border styles by construction.

use gpui_kit::prelude::*;
use gpui_kit::{App, BorderStyle, Bounds, Div, TextAlign, Window, div, point, px, quad, size};

use crate::icons;
use crate::theme::{Color, Font};
use crate::typography::TextBox;
use crate::width_memo::WidthMemo;

pub const TEXT_SIZE: f32 = crate::theme::text_size::BODY;
pub const PAD_X: f32 = 7.0;
/// The app's smallest radius step, shared with keycaps and badges.
pub const RADIUS: f32 = crate::theme::radius::XS;
/// Chip icon glyph size — a step under the label size so the glyph's optical
/// weight matches the text next to it.
pub const ICON_SIZE: f32 = 11.0;
/// Gap between a chip's icon and its label.
pub const ICON_GAP: f32 = 3.0;

/// Enough room around the label that the chip reads as a box of its own,
/// short of dwarfing the text it sits beside.
pub fn height() -> f32 {
    (TEXT_SIZE + 5.0).round()
}

#[derive(Debug, Clone)]
pub struct Chip {
    pub label: String,
    /// Typeface for the label. Name-like chips (bookmarks) read best in the
    /// UI's sans-serif; tag-like chips (file status letters, `empty`,
    /// `conflict`, …) use the mono font.
    pub font: Font,
    pub background: Color,
    pub text_color: Color,
    /// Optional 1px chip border. When `border_dashed` is `true`, the
    /// stroke is dashed (used by the `empty` chip in the design
    /// system). When `false`, it's a solid 1px line (used by remote
    /// bookmark chips — outlined, lane-colored).
    pub border_color: Option<Color>,
    pub border_dashed: bool,
    /// Optional Lucide glyph drawn ahead of the label. Workspace chips
    /// carry one so a `name@` working-copy marker reads as a different kind
    /// of thing than the bookmark pills around it.
    pub icon: Option<&'static str>,
    /// The glyph's own tint, when it differs from the label's: a neutral
    /// bookmark chip shows its graph lane through the icon alone.
    pub icon_color: Option<Color>,
    /// A trailing run in its own colour, right after the label: a remote
    /// bookmark's `@origin`, or the `*` of a bookmark that differs from its
    /// remote.
    pub suffix: Option<(String, Color)>,
}

/// Full chip width (icon + label + horizontal padding) for the given label,
/// rendered with `font` at [`TEXT_SIZE`].
pub fn width(label: &str, icon: Option<&str>, font: Font) -> f32 {
    width_memoized(&WidthMemo::default(), label, icon, font)
}

/// Full width of `chip`, its suffix included.
pub fn full_width(chip: &Chip) -> f32 {
    chip_width(&WidthMemo::default(), chip)
}

/// [`full_width`], shaping through `memo`.
pub fn chip_width(memo: &WidthMemo, chip: &Chip) -> f32 {
    width_memoized(memo, &chip.label, chip.icon, chip.font)
        + chip
            .suffix
            .as_ref()
            .map_or(0.0, |(suffix, _)| memo.width(suffix, TEXT_SIZE, chip.font))
}

/// [`width`], shaping through `memo`.
///
/// A rail lays its chips out and then paints them, so every chip is measured at
/// least twice per frame, and a row's chips are the same strings frame after
/// frame. Callers that have a memo in reach (the sidebar's row painter) pass it
/// in; the rest go through [`width`], which is the same call against an empty
/// one.
pub fn width_memoized(memo: &WidthMemo, label: &str, icon: Option<&str>, font: Font) -> f32 {
    let label_w = memo.width(label, TEXT_SIZE, font);
    let icon_w = icon.map_or(0.0, |glyph| {
        memo.width(glyph, ICON_SIZE, icons::ICON_FONT) + ICON_GAP
    });
    label_w + icon_w + PAD_X * 2.0
}

/// The `center_y` to draw `chip` at so its label's baseline lands on
/// `baseline_y` — for a chip sitting in a line of text.
pub fn center_for_baseline(chip: &Chip, baseline_y: f32) -> f32 {
    TextBox::new(chip.font, TEXT_SIZE, height()).center_for_baseline(baseline_y)
}

/// Paints `chip` with its left edge at `x`, centred on `center_y`: the chip of
/// [`view`], for a painter that places its rows by hand.
pub fn paint(
    chip: &Chip,
    memo: &WidthMemo,
    x: f32,
    center_y: f32,
    window: &mut Window,
    cx: &mut App,
) {
    let chip_h = height();
    let width = chip_width(memo, chip);
    let top = (center_y - chip_h / 2.0).round();
    let bounds = Bounds::new(point(px(x), px(top)), size(px(width), px(chip_h)));
    let background = if chip.background.a > f32::EPSILON {
        chip.background
    } else {
        Color {
            a: 0.0,
            ..chip.text_color
        }
    };
    let (border_width, border_color) = chip
        .border_color
        .map_or((0.0, background), |color| (1.0, color));
    window.paint_quad(quad(
        bounds,
        px(RADIUS),
        background,
        px(border_width),
        border_color,
        if chip.border_dashed {
            BorderStyle::Dashed
        } else {
            BorderStyle::Solid
        },
    ));
    let mut text_x = x + PAD_X;
    if let Some(glyph) = chip.icon {
        crate::typography::paint_line(
            glyph,
            text_x,
            top + (chip_h - ICON_SIZE) / 2.0,
            ICON_SIZE,
            ICON_SIZE,
            icons::ICON_FONT,
            chip.icon_color.unwrap_or(chip.text_color),
            TextAlign::Left,
            window,
            cx,
        );
        text_x += memo.width(glyph, ICON_SIZE, icons::ICON_FONT) + ICON_GAP;
    }
    // Same box as `view`: the line is the chip's height, shifted so the caps
    // sit on the mid-line.
    let shift = TextBox::new(chip.font, TEXT_SIZE, chip_h).trim_shift();
    crate::typography::paint_line(
        &chip.label,
        text_x,
        top + shift,
        TEXT_SIZE,
        chip_h,
        chip.font,
        chip.text_color,
        TextAlign::Left,
        window,
        cx,
    );
    if let Some((suffix, color)) = &chip.suffix {
        crate::typography::paint_line(
            suffix,
            text_x + memo.width(&chip.label, TEXT_SIZE, chip.font),
            top + shift,
            TEXT_SIZE,
            chip_h,
            chip.font,
            *color,
            TextAlign::Left,
            window,
            cx,
        );
    }
}

/// `chip` as an element, [`height`] tall. A painter placing chips by hand (a
/// row's rail) takes its width from [`width_memoized`].
pub fn view(chip: &Chip) -> Div {
    let chip_h = height();
    // Line box pinned to the chip's own height: a taller box distributes its
    // extra leading around the glyphs, and any asymmetry in that distribution
    // shows up as the label riding high or low inside a box this tight. The
    // shift then puts the label's caps, not its line box, on the mid-line.
    let shift = TextBox::new(chip.font, TEXT_SIZE, chip_h).trim_shift();
    let mut element = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .h(px(chip_h))
        .px(px(PAD_X))
        .gap(px(ICON_GAP))
        .rounded(px(RADIUS));
    // Skip the fill when the chip is meant to read as outlined-only.
    if chip.background.a > f32::EPSILON {
        element = element.bg(chip.background);
    }
    if let Some(color) = chip.border_color {
        element = element.border_1().border_color(color);
        if chip.border_dashed {
            element = element.border_dashed();
        }
    }
    if let Some(glyph) = chip.icon {
        element = element.child(icons::icon(
            glyph,
            ICON_SIZE,
            chip.icon_color.unwrap_or(chip.text_color),
        ));
    }
    let text = |value: String, color: Color| {
        div()
            .relative()
            .top(px(shift))
            .font(chip.font.gpui())
            .text_size(px(TEXT_SIZE))
            .line_height(px(chip_h))
            .text_color(color)
            .whitespace_nowrap()
            .child(value)
    };
    element = element.child(text(chip.label.clone(), chip.text_color));
    if let Some((suffix, color)) = &chip.suffix {
        // Flush against the label: the row's gap is for the icon.
        element = element.child(text(suffix.clone(), *color).ml(px(-ICON_GAP)));
    }
    element
}
