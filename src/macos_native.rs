//! Native macOS integration points used from the iced app.
//!
//! The popup-menu bridge: iced widgets can ask AppKit to show a real `NSMenu`
//! at the cursor — now a *tree* of items with nested submenus — and report
//! back which leaf the user picked. A previous attempt to inject an
//! `NSVisualEffectView` for a liquid glass sidebar was reverted because
//! iced/winit owns the window's `contentView` directly — wrapping it triggers a
//! panic in winit's view subclass (`tried to access uninitialized instance
//! variable`), and adding the effect view at the window's frame-view level
//! doesn't composite behind the contentView. Getting that working requires
//! either patching winit to support hosting iced inside a foreign `NSView`, or
//! rebuilding the app shell against a SwiftUI-hosted `MTKView` à la the
//! egui+SwiftUI article.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEvent, NSMenu, NSMenuItem};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    MainThreadMarker, NSNumber, NSObject, NSObjectProtocol, NSString, ns_string,
};
use objc2_quartz_core::{CABasicAnimation, CALayer, CAMediaTiming, CATransaction};

/// A node in a native popup menu. Leaves carry a caller-chosen `id` that
/// `popup_menu` returns when that leaf is picked (across any submenu depth).
pub enum MenuItem {
    /// A clickable leaf. `id` is what `popup_menu` returns when chosen.
    Entry {
        label: String,
        id: u32,
        enabled: bool,
    },
    /// A nested submenu. Empty `items` renders as a single disabled row.
    Submenu { label: String, items: Vec<MenuItem> },
    /// A horizontal separator line.
    Separator,
}

impl MenuItem {
    pub fn entry(label: impl Into<String>, id: u32) -> Self {
        Self::Entry {
            label: label.into(),
            id,
            enabled: true,
        }
    }

    pub fn submenu(label: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self::Submenu {
            label: label.into(),
            items,
        }
    }
}

/// A row rectangle to glow while the menu is open, in iced window-content
/// logical points (top-left origin) — exactly what the sidebar widget hands
/// back with the right-click. Converted to the window's content-layer space in
/// [`install_glow`].
#[derive(Clone, Copy)]
pub struct GlowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Ivars for the menu's target object: the tag of the chosen item, or `-1` if
/// the menu was dismissed without a selection.
struct TargetIvars {
    selected: Cell<i64>,
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `MenuTarget` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct MenuTarget;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        // The action fired when any leaf item is clicked. Selecting an item
        // (even inside a submenu) ends the modal tracking loop and sends this,
        // so reading the sender's tag afterward identifies the choice.
        //
        // SAFETY: The selector takes a single object argument (the sender).
        #[unsafe(method(menuItemSelected:))]
        fn menu_item_selected(&self, sender: &NSMenuItem) {
            self.ivars().selected.set(sender.tag() as i64);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            selected: Cell::new(-1),
        });
        // SAFETY: `NSObject`'s `init` is a correct designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Recursively build an `NSMenu` from `items`, wiring each leaf to `target` so
/// its action fires (and records its tag) when picked.
fn build_menu(mtm: MainThreadMarker, target: &AnyObject, items: &[MenuItem]) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    // We manage enabled state ourselves; AppKit's auto-enabling would disable
    // items lacking a validated target/action in the responder chain.
    menu.setAutoenablesItems(false);
    for item in items {
        match item {
            MenuItem::Separator => {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            MenuItem::Entry { label, id, enabled } => {
                let menu_item = NSMenuItem::new(mtm);
                menu_item.setTitle(&NSString::from_str(label));
                menu_item.setEnabled(*enabled);
                menu_item.setTag(*id as isize);
                if *enabled {
                    // SAFETY: `target` outlives the modal popup (held on the
                    // stack in `popup_menu`), and the selector exists on it.
                    unsafe {
                        menu_item.setTarget(Some(target));
                        menu_item.setAction(Some(sel!(menuItemSelected:)));
                    }
                }
                menu.addItem(&menu_item);
            }
            MenuItem::Submenu { label, items } => {
                let parent = NSMenuItem::new(mtm);
                parent.setTitle(&NSString::from_str(label));
                if items.is_empty() {
                    parent.setEnabled(false);
                } else {
                    parent.setEnabled(true);
                    let submenu = build_menu(mtm, target, items);
                    parent.setSubmenu(Some(&submenu));
                }
                menu.addItem(&parent);
            }
        }
    }
    menu
}

/// Show a native `NSMenu` (with nested submenus) at the current cursor location
/// and block until the user picks a leaf or dismisses. Returns the chosen
/// leaf's `id`, or `None` if dismissed.
///
/// `glow`, when set, installs a pulsing accent layer over that row for the
/// duration of the menu. Because the layer animates on the window server, it
/// keeps pulsing even though this call blocks the main thread in the modal menu
/// loop.
pub fn popup_menu(items: &[MenuItem], glow: Option<GlowRect>) -> Option<u32> {
    let mtm = MainThreadMarker::new()?;
    let target = MenuTarget::new(mtm);
    let target_ref: &AnyObject = &target;
    let menu = build_menu(mtm, target_ref, items);

    let glow_layer = glow.and_then(|rect| install_glow(mtm, rect));

    let location = NSEvent::mouseLocation();
    let positioned = menu.popUpMenuPositioningItem_atLocation_inView(None, location, None);

    if let Some(layer) = glow_layer {
        layer.removeFromSuperlayer();
    }
    if !positioned {
        return None;
    }
    let tag = target.ivars().selected.get();
    if tag < 0 { None } else { Some(tag as u32) }
}

/// Add a pulsing accent `CALayer` over `rect` on the key window's content
/// layer, and return it so the caller can remove it when the menu closes.
/// `None` if there's no key window (nothing to anchor to).
fn install_glow(mtm: MainThreadMarker, rect: GlowRect) -> Option<Retained<CALayer>> {
    let app = NSApplication::sharedApplication(mtm);
    let window = app.keyWindow().or_else(|| app.mainWindow())?;
    let content_view = window.contentView()?;
    let parent = content_view.layer()?;

    // iced reports the row in top-left-origin points. Map into the content
    // layer's space: if it isn't geometry-flipped, flip Y against its height.
    let parent_height = parent.bounds().size.height;
    let h = f64::from(rect.height);
    let y_top = f64::from(rect.y);
    let y = if parent.isGeometryFlipped() {
        y_top
    } else {
        parent_height - (y_top + h)
    };
    let frame = CGRect::new(
        CGPoint::new(f64::from(rect.x), y),
        CGSize::new(f64::from(rect.width), h),
    );

    // Accent orange (matches the sidebar's working-copy accent). Translucent
    // fill so the row text stays readable; a brighter border + soft shadow read
    // as a "glow".
    let accent = CGColor::new_srgb(0.96, 0.55, 0.22, 1.0);
    let fill = CGColor::new_srgb(0.96, 0.55, 0.22, 0.22);

    let glow = CALayer::new();
    glow.setFrame(frame);
    glow.setCornerRadius(6.0);
    glow.setBackgroundColor(Some(&fill));
    glow.setBorderColor(Some(&accent));
    glow.setBorderWidth(1.5);
    glow.setShadowColor(Some(&accent));
    glow.setShadowRadius(8.0);
    glow.setShadowOffset(CGSize::new(0.0, 0.0));
    glow.setShadowOpacity(0.9);

    // Add it without an implicit fade-in (we drive our own pulse below).
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    parent.addSublayer(&glow);
    CATransaction::commit();

    // Opacity pulse: autoreversing, infinite. Runs on the render server, so it
    // animates under the modal menu.
    let anim = CABasicAnimation::animationWithKeyPath(Some(ns_string!("opacity")));
    let from = NSNumber::numberWithDouble(0.45);
    let to = NSNumber::numberWithDouble(1.0);
    let from_ref: &AnyObject = &from;
    let to_ref: &AnyObject = &to;
    // SAFETY: opacity takes plain NSNumber values.
    unsafe {
        anim.setFromValue(Some(from_ref));
        anim.setToValue(Some(to_ref));
    }
    anim.setDuration(0.6);
    anim.setAutoreverses(true);
    anim.setRepeatCount(f32::INFINITY);
    glow.addAnimation_forKey(&anim, Some(ns_string!("pulse")));

    // Commit to the render server now, before we block in the modal menu loop.
    CATransaction::flush();

    Some(glow)
}
