//! Per-platform window-chrome policy: how the in-app tab strip coexists with
//! (or stands in for) the OS title bar.
//!
//! The tab strip is the app's top bar and is ALWAYS visible — it does not
//! depend on the OS drawing a title bar (e.g. on a Linux compositor like niri
//! that draws no decorations, the strip simply *is* the top of the window).
//! What differs per platform is only how it coexists with native chrome:
//!
//!   * **macOS** — we make the OS title bar transparent and let our content
//!     fill behind it, so the strip sits inline with the (still-drawn) traffic
//!     lights. We reserve a leading inset for those lights and turn the strip's
//!     empty area into a window-drag handle, since the hidden title bar no
//!     longer provides one.
//!   * **Linux** — undecorated compositors already render the strip as the top
//!     bar with nothing above it, so the strip itself needs nothing; the only
//!     platform setting is the window's `app_id`, which ties it to our desktop
//!     entry. A floating, undecorated WM is the natural next opt-in for the drag
//!     handle.
//!   * **Windows** — keeps its native title bar above the strip for now.
//!
//! Centralizing the policy here keeps `main` / `tab_bar` platform-agnostic, so
//! extending to full custom chrome on Windows/Linux is a change in one place
//! rather than `cfg!`s scattered across the UI.

use gpui_kit::{Pixels, Point, TitlebarOptions, point, px};

/// Width reserved at the leading edge of the strip for OS-drawn window controls
/// that overlap our content (macOS traffic lights). Zero where they don't.
pub fn leading_inset() -> f32 {
    if cfg!(target_os = "macos") {
        // Clears the three traffic lights plus their left margin.
        78.0
    } else {
        0.0
    }
}

/// How much of the content's outer edge the OS window border covers. macOS
/// draws its 1pt hairline over our pixels, so an inset measured from the
/// window edge reads that much smaller than the same inset against one of
/// our own dividers.
pub fn window_border() -> f32 {
    if cfg!(target_os = "macos") { 1.0 } else { 0.0 }
}

/// Whether the strip's empty area should act as a window-drag handle. True on
/// platforms where we've hidden/replaced the native title bar so the OS no
/// longer offers a drag surface — macOS today; an undecorated Linux WM is the
/// natural next opt-in.
pub fn drag_region() -> bool {
    cfg!(target_os = "macos")
}

/// Platform spelling of the primary command modifier for UI hints — `⌘` on
/// macOS, `Ctrl` elsewhere, matching what the keyboard handlers actually
/// bind (`modifiers.command()` maps to Ctrl off-mac).
pub fn cmd_label(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("\u{2318}{key}")
    } else {
        format!("Ctrl {key}")
    }
}

/// Fixed height for the strip when it stands in for the OS title bar — the tabs
/// center in it, and `position_window_controls` repositions the native traffic
/// lights to that same center so the two line up. Kept comfortably taller than
/// the native ~28pt title bar so the tabs aren't cramped. `None` lets the strip
/// size to its own content (it sits below a native title bar, or is the sole
/// top bar with no controls to match).
pub fn title_bar_height() -> Option<f32> {
    if cfg!(target_os = "macos") {
        // ~4px of breathing room above/below the ~25px-tall tabs.
        Some(33.0)
    } else {
        None
    }
}

/// The window's title bar. On macOS the bar is hidden and our content fills
/// behind the (still-shown) traffic lights, which sit on the vertical center of
/// the [`title_bar_height`] strip so they line up with the centered tabs.
pub fn titlebar_options() -> TitlebarOptions {
    TitlebarOptions {
        title: Some("diffui".into()),
        appears_transparent: cfg!(target_os = "macos"),
        traffic_light_position: traffic_light_position(),
    }
}

fn traffic_light_position() -> Option<Point<Pixels>> {
    /// A traffic light's diameter.
    const LIGHT: f32 = 14.0;
    let bar = title_bar_height()?;
    Some(point(px(12.0), px((bar - LIGHT) / 2.0)))
}
