//! One wheel speed for every scroll view.
//!
//! A trackpad's deltas already arrive in points, so content follows the finger
//! 1:1 at any scale factor. A mouse wheel's notches arrive as lines, and each
//! scroll view would otherwise pick its own distance for one: GPUI's `list`
//! steps 20pt, its `div` one text line.

use gpui_kit::{Pixels, Point, ScrollDelta, px};

/// Points one wheel line scrolls. macOS reports a slow notch as one line and
/// accelerates faster spins into several, so this is the slow-notch step:
/// WebKit's and Chromium's per-line distance on macOS, about two diff rows.
/// Elsewhere a notch is one unaccelerated line, and Chromium's 100 (three
/// lines) applies.
pub const POINTS_PER_LINE: f32 = if cfg!(target_os = "macos") {
    40.0
} else {
    100.0
};

/// `delta` in points, lines at [`POINTS_PER_LINE`] each.
pub fn points(delta: ScrollDelta) -> Point<Pixels> {
    delta.pixel_delta(px(POINTS_PER_LINE))
}
