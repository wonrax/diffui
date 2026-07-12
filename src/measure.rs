//! Headless one-line text measurement, shared by chip sizing, sidebar path
//! truncation, menu layout, and the diff view's column metrics.
//!
//! We previously approximated text width with `chars * 7px`-style heuristics,
//! which silently misbehaved for any glyph wider or narrower than the assumed
//! average — `@` clipped into `…` in revision IDs, abbreviated paths over- or
//! under-shot the available room, and badges would clip if the user ever
//! switched to a larger font. Going through real `cosmic_text` shaping fixes
//! the entire class of bug because it's the same engine the wgpu renderer
//! draws with, so measurements match painted pixels exactly.
//!
//! Why headless `iced::advanced::graphics::text::Paragraph` rather than the
//! renderer's `R::Paragraph`: much of this measurement runs in `view()`, well
//! before any `draw()` call, where the renderer's `Paragraph` type isn't
//! reachable without threading renderer generics through `main.rs`. The wgpu
//! renderer is built on top of `iced_graphics`, so the headless `Paragraph`
//! shapes text identically.

use iced::advanced::graphics::text::Paragraph;
use iced::advanced::text::{self, Paragraph as _, Shaping, Text};
use iced::{Font, Pixels, Size, alignment};

/// Line box used by single-line UI text throughout the app.
pub const LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Rendered width of one line of `content`, shaped like UI text
/// (`Shaping::Advanced`).
pub fn line_width(content: &str, size: f32, font: Font) -> f32 {
    line_width_shaped(content, size, font, Shaping::Advanced)
}

/// Rendered extent (width × line box) of one line of `content`, for callers
/// sizing a box around the text — e.g. the sidebar's hover tooltip.
pub fn line_bounds(content: &str, size: f32, font: Font) -> Size {
    one_line(content, size, font, Shaping::Advanced).min_bounds()
}

/// [`line_width`] with an explicit shaping strategy, for callers that must
/// match a renderer path that draws with `Shaping::Basic` (the diff grid).
pub fn line_width_shaped(content: &str, size: f32, font: Font, shaping: Shaping) -> f32 {
    if content.is_empty() {
        return 0.0;
    }
    one_line(content, size, font, shaping).min_width()
}

fn one_line(content: &str, size: f32, font: Font, shaping: Shaping) -> Paragraph {
    let line_height = (size * LINE_HEIGHT_MULTIPLIER).max(1.0);
    Paragraph::with_text(Text {
        content,
        bounds: Size::new(f32::INFINITY, line_height),
        size: Pixels(size),
        line_height: text::LineHeight::Absolute(Pixels(line_height)),
        font,
        align_x: text::Alignment::Left,
        align_y: alignment::Vertical::Top,
        shaping,
        wrapping: text::Wrapping::None,
        ellipsis: text::Ellipsis::None,
        hint_factor: None,
    })
}
