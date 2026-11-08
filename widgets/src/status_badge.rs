//! A file's status as one square badge: its letter (A/M/D/R/C) in the
//! status colour on a tint of it, no border. The changed-files tree and the
//! diff's file headers both draw it, from this geometry, so the two read as
//! the same mark.

use gpui_kit::{App, TextAlign, Window, fill, px};

use crate::geometry::Rectangle;
use crate::theme::{Color, Font, Weight, chip_background, emphasis_font};
use crate::typography::{self, TextBox};

/// Side of the badge.
pub const SIZE: f32 = 16.0;
/// The letter's size: well under the names beside it, so the badge stays a
/// mark rather than a word.
pub const TEXT_SIZE: f32 = 10.0;
pub const RADIUS: f32 = crate::theme::radius::XS;

/// What a badge shows. `ink` is the status colour; the fill is derived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusBadge<'a> {
    pub letter: &'a str,
    pub ink: Color,
}

/// The letter's face: the mono font at semibold.
pub fn font(mono: Font) -> Font {
    emphasis_font(mono, Weight::SEMIBOLD)
}

/// The badge's box with its left edge at `x`, centred on `center_y`.
pub fn rect(x: f32, center_y: f32) -> Rectangle {
    Rectangle {
        x,
        y: (center_y - SIZE / 2.0).round(),
        width: SIZE,
        height: SIZE,
    }
}

/// The `center_y` that puts the letter's baseline on `baseline_y`, for a badge
/// sitting in a line of text. The letter's caps are centred in the box, so this
/// is also where the box goes.
pub fn center_for_baseline(mono: Font, baseline_y: f32) -> f32 {
    TextBox::new(font(mono), TEXT_SIZE, SIZE).center_for_baseline(baseline_y)
}

/// Where the letter's line box goes in `rect`: its top, with the caps centred.
pub fn letter_top(mono: Font, rect: Rectangle) -> f32 {
    let line = TextBox::new(font(mono), TEXT_SIZE, SIZE);
    line.baseline_for_center(rect.center_y()) - line.baseline()
}

/// The badge's tint.
pub fn fill_color(badge: &StatusBadge) -> Color {
    chip_background(badge.ink)
}

/// Paints `badge` in `rect` now.
pub fn paint(badge: &StatusBadge, mono: Font, rect: Rectangle, window: &mut Window, cx: &mut App) {
    window.paint_quad(fill(rect.bounds(), fill_color(badge)).corner_radii(px(RADIUS)));
    typography::paint_line(
        badge.letter,
        rect.center_x(),
        letter_top(mono, rect),
        TEXT_SIZE,
        SIZE,
        font(mono),
        badge.ink,
        TextAlign::Center,
        window,
        cx,
    );
}
