//! The app's icon set.
//!
//! UI glyphs come from the [Lucide](https://lucide.dev) icon font, fetched at
//! build time (see `build.rs`) and embedded here. Rendering them as **text**
//! (rather than rasterized SVG) is deliberate: glyphs go through the same
//! rasterizer and glyph atlas as the rest of our text, so they stay crisp at
//! every size, tint for free, and carry **identical metrics on every OS**.

// This module is a catalog: some entries are intentionally ahead of their
// first use.
#![allow(dead_code)]

use gpui_kit::prelude::*;
use gpui_kit::{App, Div, div, px};

use crate::theme::{Color, Font};

/// The Lucide TTF, fetched + checksummed by `build.rs` into `OUT_DIR`. Embedded
/// so there's no runtime file dependency. Registered by [`register`].
pub const ICON_FONT_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lucide.ttf"));

/// The family name inside the Lucide TTF (its `name` table). Must match exactly
/// or the text system falls back to tofu boxes.
pub const ICON_FONT: Font = Font::new("lucide");

/// Adds the icon font to the app's text system. Call once at startup.
pub fn register(cx: &App) {
    if let Err(error) = cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(ICON_FONT_BYTES)])
    {
        tracing::warn!("couldn't load the icon font: {error}");
    }
}

/// A Lucide glyph as a themeable icon, centered inside a `size`×`size` square.
///
/// The fixed box, not the glyph's font-specific line metrics, is what drives
/// alignment next to text, so an icon + label row centers the same way on every
/// platform.
pub fn icon(glyph: &'static str, size: f32, color: Color) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .font(ICON_FONT.gpui())
        .text_size(px(size))
        .line_height(px(size))
        .text_color(color)
        .child(glyph)
}

// --- Lucide codepoints ------------------------------------------------------
// Pinned to lucide-static 1.21.0 (see build.rs). Regenerate from that version's
// font/info.json if the pin changes. Names mirror the Lucide icon they map to.

pub const REFRESH: &str = "\u{e145}"; // refresh-cw
pub const UNDO: &str = "\u{e2a1}"; // undo-2
pub const REDO: &str = "\u{e2a0}"; // redo-2
pub const WRAP: &str = "\u{e248}"; // wrap-text
pub const SPLIT: &str = "\u{e098}"; // columns-2
pub const FETCH: &str = "\u{e0b2}"; // download
pub const ARROW_DOWN: &str = "\u{e042}"; // arrow-down
pub const ARROW_UP: &str = "\u{e04a}"; // arrow-up
pub const CLOSE: &str = "\u{e1b2}"; // x
pub const ALERT_TRIANGLE: &str = "\u{e193}"; // alert-triangle (⚠)
pub const GIT_BRANCH: &str = "\u{e0e2}"; // git-branch
pub const GIT_MERGE: &str = "\u{e0e4}"; // git-merge
pub const GIT_FORK: &str = "\u{e28d}"; // git-fork
pub const FOLD_VERTICAL: &str = "\u{e43c}"; // fold-vertical
pub const ENTER: &str = "\u{e0a1}"; // corner-down-left (↵)
pub const CHEVRON_DOWN: &str = "\u{e06d}"; // chevron-down
pub const CHEVRON_RIGHT: &str = "\u{e06f}"; // chevron-right
pub const CHEVRON_UP: &str = "\u{e070}"; // chevron-up
pub const CHECK: &str = "\u{e06c}"; // check
pub const PENCIL: &str = "\u{e1f9}"; // pencil
pub const FOLDER: &str = "\u{e0d7}"; // folder
pub const CIRCLE: &str = "\u{e076}"; // circle
pub const HASH: &str = "\u{e0ef}"; // hash
pub const SEARCH: &str = "\u{e151}"; // search
pub const PLUS: &str = "\u{e13d}"; // plus
pub const MINUS: &str = "\u{e11c}"; // minus
pub const CODE: &str = "\u{e093}"; // code
pub const CODE_XML: &str = "\u{e206}"; // code-xml
pub const IMAGE: &str = "\u{e0f6}"; // image
pub const FILE: &str = "\u{e0c0}"; // file
pub const FILE_CODE: &str = "\u{e0c3}"; // file-code
pub const FILE_DIFF: &str = "\u{e317}"; // file-diff
pub const FOLDER_OPEN: &str = "\u{e247}"; // folder-open
pub const TAG: &str = "\u{e17f}"; // tag
pub const UPLOAD: &str = "\u{e19e}"; // upload
pub const COPY: &str = "\u{e09e}"; // copy
pub const COLUMNS_3: &str = "\u{e099}"; // columns-3
pub const SQUARE_PLUS: &str = "\u{e173}"; // square-plus
pub const HISTORY: &str = "\u{e1f5}"; // history
pub const ROTATE_CCW: &str = "\u{e148}"; // rotate-ccw
pub const FILE_CLOCK: &str = "\u{e315}"; // file-clock
pub const ARROW_LEFT: &str = "\u{e048}"; // arrow-left
pub const SCROLL_TEXT: &str = "\u{e45f}"; // scroll-text
pub const ARROW_RIGHT: &str = "\u{e049}"; // arrow-right
pub const ARROW_LEFT_RIGHT: &str = "\u{e24a}"; // arrow-left-right
pub const GIT_COMPARE: &str = "\u{e359}"; // git-compare
pub const UNFOLD_VERTICAL: &str = "\u{e43e}"; // unfold-vertical
pub const USER: &str = "\u{e19f}"; // user
pub const CLOCK: &str = "\u{e087}"; // clock
pub const TERMINAL: &str = "\u{e181}"; // terminal
pub const LIST_CHECKS: &str = "\u{e1d0}"; // list-checks
