//! Native macOS integration points used from the iced app.
//!
//! Right now this is just the popup-menu bridge: iced widgets can ask AppKit
//! to show a real `NSMenu` at the cursor and report back which item the user
//! picked. A previous attempt to inject an `NSVisualEffectView` for a liquid
//! glass sidebar was reverted because iced/winit owns the window's
//! `contentView` directly — wrapping it triggers a panic in winit's view
//! subclass (`tried to access uninitialized instance variable`), and adding
//! the effect view at the window's frame-view level doesn't composite behind
//! the contentView. Getting that working requires either patching winit to
//! support hosting iced inside a foreign `NSView`, or rebuilding the app
//! shell against a SwiftUI-hosted `MTKView` à la the egui+SwiftUI article.

use objc2_app_kit::{NSEvent, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSString};

/// Show a native `NSMenu` at the current cursor location and block until the
/// user picks an item or dismisses.
///
/// Each item's tag is set to its index in `items`, so the returned `usize`
/// matches what the caller passed in.
pub fn popup_menu(items: &[&str]) -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for (i, label) in items.iter().enumerate() {
        let title = NSString::from_str(label);
        let item = NSMenuItem::new(mtm);
        item.setTitle(&title);
        item.setEnabled(true);
        item.setTag(i as isize);
        menu.addItem(&item);
    }
    let location = NSEvent::mouseLocation();
    let positioned =
        menu.popUpMenuPositioningItem_atLocation_inView(None, location, None);
    if !positioned {
        return None;
    }
    let highlighted = menu.highlightedItem()?;
    let tag = highlighted.tag();
    if tag < 0 { None } else { Some(tag as usize) }
}
