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
use objc2_app_kit::{NSApplication, NSEvent, NSMenu, NSMenuItem, NSModalResponseOK, NSOpenPanel};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::{
    MainThreadMarker, NSNumber, NSObject, NSObjectProtocol, NSString, ns_string,
};
use objc2_quartz_core::{CABasicAnimation, CALayer, CAMediaTiming, CATransaction};

/// Show the system directory picker and return the selected working-copy path.
pub fn choose_repository_folder() -> Option<String> {
    let mtm = MainThreadMarker::new()?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseDirectories(true);
    panel.setCanChooseFiles(false);
    panel.setAllowsMultipleSelection(false);
    panel.setResolvesAliases(true);
    panel.setPrompt(Some(ns_string!("Open repository")));
    panel.setMessage(Some(ns_string!(
        "Choose a folder containing a jj or Git working copy."
    )));

    if panel.runModal() != NSModalResponseOK {
        return None;
    }
    panel
        .URLs()
        .firstObject()?
        .path()
        .map(|path| path.to_string())
}

/// The usable size (logical points, menu bar and Dock excluded) of the
/// primary display, for sizing a first-launch window. Safe to call before
/// the app has finished launching.
pub fn primary_screen_visible_size() -> Option<(f32, f32)> {
    use objc2::class;
    use objc2_foundation::NSRect;
    MainThreadMarker::new()?;
    // SAFETY: `NSScreen` class methods and `visibleFrame` take no arguments
    // and return an object / a plain struct; a nil screen is checked first.
    unsafe {
        let mut screen: *mut AnyObject = msg_send![class!(NSScreen), mainScreen];
        if screen.is_null() {
            let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
            if !screens.is_null() {
                screen = msg_send![screens, firstObject];
            }
        }
        if screen.is_null() {
            return None;
        }
        let frame: NSRect = msg_send![screen, visibleFrame];
        Some((frame.size.width as f32, frame.size.height as f32))
    }
}

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

// ── The menu bar ────────────────────────────────────────────────────────────
//
// winit installs a bare app menu (About / Hide / Quit). [`install_menu_bar`]
// replaces it with the app's own, built from the command registry by
// `crate::app_menu`. Two rules keep it from fighting the keymap:
//
//   * A command item doesn't fire on its key equivalent. The command menus
//     are a `KeymapMenu`, whose `performKeyEquivalent:` declines every key,
//     so the keystroke falls through to the window and the keymap resolves
//     it against the mode stack as before (and honours `[keys]`); the menu
//     only *shows* the chord. A click sends the item's tag down
//     [`menu_bar_picks`]. (Disabling the item during the key match doesn't
//     work: AppKit swallows a key that matches a disabled item.) The app and
//     Window menus stay plain `NSMenu`s, so ⌘, ⌘H ⌘Q ⌘M work natively.
//   * Edit's copy / paste / select-all re-post their own key to the window
//     when clicked, so the focused iced text field handles them exactly as
//     it handles the keystroke.
//
// Like the popup menus, the bar blocks the main thread while it is open;
// picks are queued on a channel and read by an iced subscription once it
// closes.

/// A key equivalent as AppKit spells it: the key (lower-case for a plain
/// letter, upper-case implies shift) and the modifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEquivalent {
    pub key: String,
    pub command: bool,
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
}

impl KeyEquivalent {
    fn mask(&self) -> objc2_app_kit::NSEventModifierFlags {
        use objc2_app_kit::NSEventModifierFlags as F;
        let mut mask = F::empty();
        for (held, flag) in [
            (self.command, F::Command),
            (self.alt, F::Option),
            (self.control, F::Control),
            (self.shift, F::Shift),
        ] {
            if held {
                mask |= flag;
            }
        }
        mask
    }
}

/// One menu-bar item.
pub enum BarItem {
    /// Sends `tag` down [`menu_bar_picks`] when clicked.
    Command {
        title: String,
        tag: u32,
        key: Option<KeyEquivalent>,
    },
    /// A standard AppKit action sent down the responder chain (`hide:`,
    /// `terminate:`, `performMiniaturize:`), handled by AppKit itself.
    Standard {
        title: String,
        action: objc2::runtime::Sel,
        key: Option<KeyEquivalent>,
    },
    /// Re-posts `key` to the key window when clicked (Edit's copy/paste).
    EditKey {
        title: String,
        key: KeyEquivalent,
    },
    Submenu {
        title: String,
        items: Vec<BarItem>,
    },
    Separator,
}

/// What AppKit should treat a top-level menu as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarRole {
    /// The application menu (always first; its title is the app's name).
    App,
    Plain,
    /// The Window menu: AppKit appends the window list.
    Windows,
    /// The Help menu: AppKit adds its search field.
    Help,
}

pub struct BarMenu {
    pub title: String,
    pub role: BarRole,
    pub items: Vec<BarItem>,
}

static BAR_TX: std::sync::OnceLock<futures::channel::mpsc::UnboundedSender<u32>> =
    std::sync::OnceLock::new();
static BAR_RX: std::sync::Mutex<Option<futures::channel::mpsc::UnboundedReceiver<u32>>> =
    std::sync::Mutex::new(None);

/// The tags of clicked [`BarItem::Command`]s. Subscribed once; a second call
/// gets a stream that never yields.
pub fn menu_bar_picks() -> impl futures::Stream<Item = u32> {
    bar_channel();
    BAR_RX
        .lock()
        .ok()
        .and_then(|mut rx| rx.take())
        .unwrap_or_else(|| futures::channel::mpsc::unbounded().1)
}

fn bar_channel() -> &'static futures::channel::mpsc::UnboundedSender<u32> {
    BAR_TX.get_or_init(|| {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        if let Ok(mut slot) = BAR_RX.lock() {
            *slot = Some(rx);
        }
        tx
    })
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `BarTarget` does not implement `Drop`.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct BarTarget;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for BarTarget {}

    impl BarTarget {
        // SAFETY: The selector takes a single object argument (the sender).
        #[unsafe(method(barItemSelected:))]
        fn bar_item_selected(&self, sender: &NSMenuItem) {
            let _ = bar_channel().unbounded_send(sender.tag() as u32);
        }

        // SAFETY: The selector takes a single object argument (the sender).
        #[unsafe(method(editKeySelected:))]
        fn edit_key_selected(&self, sender: &NSMenuItem) {
            let key = sender.keyEquivalent().to_string();
            let mask = sender.keyEquivalentModifierMask();
            repost_key(&key, mask);
        }

    }
);

define_class!(
    // SAFETY:
    // - Subclassing NSMenu is supported; only `performKeyEquivalent:` is
    //   overridden, with its documented signature.
    // - `KeymapMenu` does not implement `Drop`.
    #[unsafe(super(NSMenu))]
    #[thread_kind = MainThreadOnly]
    struct KeymapMenu;

    impl KeymapMenu {
        // Decline every key equivalent: the keymap owns these keys.
        //
        // SAFETY: Matches `-performKeyEquivalent:`.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, _event: &NSEvent) -> objc2::runtime::Bool {
            objc2::runtime::Bool::NO
        }
    }
);

impl BarTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `NSObject`'s `init` is a correct designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Post `key` (an AppKit key equivalent) with `mask` to the key window as a
/// key down/up pair, as if typed.
fn repost_key(key: &str, mask: objc2_app_kit::NSEventModifierFlags) {
    use objc2::class;
    use objc2_foundation::NSPoint;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.keyWindow() else {
        return;
    };
    // Virtual key codes (US layout) for the keys the Edit menu reposts;
    // winit reads the physical key from it and the logical key from the
    // characters.
    let code: u16 = match key {
        "a" => 0,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "z" => 6,
        _ => 0,
    };
    let characters = NSString::from_str(key);
    // SAFETY: `keyEventWithType:…` takes plain values and two strings, and
    // returns an autoreleased event or nil; `postEvent:atStart:` takes it.
    unsafe {
        let uptime: f64 = {
            let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
            msg_send![info, systemUptime]
        };
        for kind in [10usize, 11usize] {
            let event: Option<Retained<NSEvent>> = msg_send![
                class!(NSEvent),
                keyEventWithType: kind,
                location: NSPoint::new(0.0, 0.0),
                modifierFlags: mask,
                timestamp: uptime,
                windowNumber: window.windowNumber(),
                context: std::ptr::null_mut::<AnyObject>(),
                characters: &*characters,
                charactersIgnoringModifiers: &*characters,
                isARepeat: false,
                keyCode: code
            ];
            if let Some(event) = event {
                app.postEvent_atStart(&event, false);
            }
        }
    }
}

/// Replace the menu bar with `menus`. Call on the main thread once the app
/// has finished launching (winit installs its default menu then).
pub fn install_menu_bar(menus: &[BarMenu]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    bar_channel();
    let target = BarTarget::new(mtm);
    let app = NSApplication::sharedApplication(mtm);
    let bar = NSMenu::new(mtm);
    for spec in menus {
        let keymap_keys = matches!(spec.role, BarRole::Plain | BarRole::Help);
        let menu = build_bar_menu(mtm, &target, &spec.title, &spec.items, keymap_keys);
        let holder = NSMenuItem::new(mtm);
        holder.setTitle(&NSString::from_str(&spec.title));
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
        match spec.role {
            BarRole::Windows => app.setWindowsMenu(Some(&menu)),
            BarRole::Help => app.setHelpMenu(Some(&menu)),
            BarRole::App | BarRole::Plain => {}
        }
    }
    app.setMainMenu(Some(&bar));
    // Menu items hold their target weakly; it has to live as long as the
    // menu bar, which is the life of the process.
    std::mem::forget(target);
}

/// Build one menu of the bar. `keymap_keys`: its items' key equivalents are
/// shown only, never performed (see [`KeymapMenu`]).
fn build_bar_menu(
    mtm: MainThreadMarker,
    target: &BarTarget,
    title: &str,
    items: &[BarItem],
    keymap_keys: bool,
) -> Retained<NSMenu> {
    let title = NSString::from_str(title);
    let menu: Retained<NSMenu> = if keymap_keys {
        // SAFETY: `initWithTitle:` is NSMenu's designated initializer.
        let menu: Retained<KeymapMenu> =
            unsafe { msg_send![KeymapMenu::alloc(mtm), initWithTitle: &*title] };
        Retained::into_super(menu)
    } else {
        let menu = NSMenu::new(mtm);
        menu.setTitle(&title);
        menu
    };
    // Whether a command can run is checked when its pick runs; AppKit's
    // auto-enabling would grey out every item without a responder for it.
    // The standard items (Hide, Quit, Minimize) are always available.
    menu.setAutoenablesItems(false);
    let target_ref: &AnyObject = target;
    for item in items {
        let built = match item {
            BarItem::Separator => NSMenuItem::separatorItem(mtm),
            BarItem::Command { title, tag, key } => {
                let entry = bar_entry(mtm, title, sel!(barItemSelected:), key.as_ref());
                entry.setTag(*tag as isize);
                // SAFETY: `target` outlives the menu (see `install_menu_bar`).
                unsafe { entry.setTarget(Some(target_ref)) };
                entry
            }
            BarItem::Standard { title, action, key } => {
                bar_entry(mtm, title, *action, key.as_ref())
            }
            BarItem::EditKey { title, key } => {
                let entry = bar_entry(mtm, title, sel!(editKeySelected:), Some(key));
                // SAFETY: as above.
                unsafe { entry.setTarget(Some(target_ref)) };
                entry
            }
            BarItem::Submenu { title, items } => {
                let entry = NSMenuItem::new(mtm);
                entry.setTitle(&NSString::from_str(title));
                let submenu = build_bar_menu(mtm, target, title, items, keymap_keys);
                entry.setSubmenu(Some(&submenu));
                entry
            }
        };
        menu.addItem(&built);
    }
    menu
}

fn bar_entry(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    key: Option<&KeyEquivalent>,
) -> Retained<NSMenuItem> {
    let key_string = NSString::from_str(key.map_or("", |key| key.key.as_str()));
    // SAFETY: `action` is a selector the item's target (ours, or the
    // responder chain for the standard ones) implements.
    let entry = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &key_string,
        )
    };
    if let Some(key) = key {
        entry.setKeyEquivalentModifierMask(key.mask());
    }
    entry.setEnabled(true);
    entry
}
