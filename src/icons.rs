//! The app's icon set.
//!
//! UI glyphs come from the [Lucide](https://lucide.dev) icon font, fetched at
//! build time (see `build.rs`) and embedded here. Rendering them as **text**
//! (rather than rasterized SVG) is deliberate: glyphs go through the same
//! cosmic-text rasterizer + glyph-atlas cache as the rest of our text, so they
//! stay crisp at every size, tint for free via `.color()`, and — crucially —
//! carry **identical metrics on every OS**. The platform UI font differs across
//! macOS/Windows/Linux, so glyphs drawn from it (the old `text("\u{2193}")`
//! approach) shifted vertically per platform; a bundled font we ship removes
//! that variance entirely.
//!
//! One-off icons that aren't in Lucide are imported as monochrome SVG via
//! [`svg_icon`] and recolored to the theme.

// This module is a catalog: the codepoint constants and `svg_icon` helper are a
// shared palette consumed incrementally as call sites migrate off the old
// text-glyph icons, so some entries are intentionally ahead of their first use.
#![allow(dead_code)]

use iced::{
    Color, Element, Length,
    widget::{container, svg, text, text::LineHeight},
};

/// The Lucide TTF, fetched + checksummed by `build.rs` into `OUT_DIR`. Embedded
/// so there's no runtime file dependency. Registered with iced in `main` via
/// `.font(icons::ICON_FONT_BYTES)`.
pub const ICON_FONT_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lucide.ttf"));

/// The family name inside the Lucide TTF (its `name` table). Must match exactly
/// or cosmic-text falls back to `.notdef` tofu boxes.
pub const ICON_FONT: iced::Font = iced::Font::new("lucide");

/// A Lucide glyph as a themeable icon, centered inside a `size`×`size` square.
///
/// The fixed box — not the glyph's font-specific line metrics — is what drives
/// alignment next to text, so an icon + label row centers the same way on every
/// platform. Collapsing the default 1.3× line height keeps the glyph's own box
/// ≈ `size` so the container can center it tightly.
pub fn icon<'a, Message: 'a>(glyph: &'a str, size: f32, color: Color) -> Element<'a, Message> {
    container(
        text(glyph)
            .font(ICON_FONT)
            .size(size)
            .color(color)
            .line_height(LineHeight::Relative(1.0)),
    )
    .center_x(Length::Fixed(size))
    .center_y(Length::Fixed(size))
    .into()
}

/// A custom icon imported from an embedded, monochrome SVG and tinted to
/// `color` (the tint overrides every fill in the SVG). Use the escape hatch for
/// marks Lucide doesn't have:
///
/// ```ignore
/// icons::svg_icon(include_bytes!("../assets/icons/my-mark.svg"), 16.0, theme.text)
/// ```
pub fn svg_icon<'a, Message: 'a>(
    bytes: &'static [u8],
    size: f32,
    color: Color,
) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(bytes))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into()
}

// --- Lucide codepoints ------------------------------------------------------
// Pinned to lucide-static 1.21.0 (see build.rs). Regenerate from that version's
// font/info.json if the pin changes. Names mirror the Lucide icon they map to.

pub const REFRESH: &str = "\u{e145}"; // refresh-cw
pub const UNDO: &str = "\u{e2a1}"; // undo-2
pub const WRAP: &str = "\u{e248}"; // wrap-text
pub const SPLIT: &str = "\u{e098}"; // columns-2
pub const FETCH: &str = "\u{e0b2}"; // download
pub const ARROW_DOWN: &str = "\u{e042}"; // arrow-down
pub const ARROW_UP: &str = "\u{e04a}"; // arrow-up
pub const CLOSE: &str = "\u{e1b2}"; // x
pub const GIT_BRANCH: &str = "\u{e0e2}"; // git-branch
pub const CHEVRON_DOWN: &str = "\u{e06d}"; // chevron-down
pub const CHEVRON_RIGHT: &str = "\u{e06f}"; // chevron-right
pub const CHEVRON_UP: &str = "\u{e070}"; // chevron-up
pub const CHECK: &str = "\u{e06c}"; // check
pub const CIRCLE: &str = "\u{e076}"; // circle
pub const HASH: &str = "\u{e0ef}"; // hash
pub const SEARCH: &str = "\u{e151}"; // search
pub const PLUS: &str = "\u{e13d}"; // plus
pub const MINUS: &str = "\u{e11c}"; // minus
