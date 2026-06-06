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
//!     bar with nothing above it; nothing special is needed today. A floating,
//!     undecorated WM is the natural next opt-in for the drag handle.
//!   * **Windows** — keeps its native title bar above the strip for now.
//!
//! Centralizing the policy here keeps `main` / `tab_bar` platform-agnostic, so
//! extending to full custom chrome on Windows/Linux is a change in one place
//! rather than `cfg!`s scattered across the UI.

use iced::window;

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

/// Whether the strip's empty area should act as a window-drag handle. True on
/// platforms where we've hidden/replaced the native title bar so the OS no
/// longer offers a drag surface — macOS today; an undecorated Linux WM is the
/// natural next opt-in.
pub fn drag_region() -> bool {
    cfg!(target_os = "macos")
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

/// Apply platform window-chrome settings to the window the app opens. On macOS
/// this hides the title bar and lets our content fill behind the (still-shown)
/// traffic lights; elsewhere the window defaults already give us a top-anchored
/// strip, so this is a no-op.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn apply_window_settings(settings: &mut window::Settings) {
    #[cfg(target_os = "macos")]
    {
        settings.platform_specific.title_hidden = true;
        settings.platform_specific.titlebar_transparent = true;
        settings.platform_specific.fullsize_content_view = true;
    }
}

/// Set the macOS dock / app-switcher icon for an **unbundled** run only.
///
/// The nix package wraps the binary in a real `Diffui.app` whose compiled
/// asset catalog (from `actool`) drives the icon — including the Tahoe dynamic
/// light/dark/tinted + Liquid Glass treatment — so a *packaged* launch (and the
/// `bin/diffui` symlink into the bundle) gets the right icon from the OS with no
/// code involved. We must NOT touch it there: `setApplicationIconImage:` takes a
/// *static* `NSImage` and would flatten that dynamic icon. So this only fires
/// for an unbundled binary (`cargo run`, a bare `target/debug/diffui`), where
/// the dock would otherwise show the generic terminal icon — winit/iced's window
/// icon is a no-op on macOS, so AppKit is the only lever. We detect "bundled" by
/// asking whether the main bundle has an identifier (only true with a real
/// `Info.plist`). No-op off macOS; must run on the main thread (the iced update
/// loop, which calls this, is main-thread).
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use std::ffi::c_void;

    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: runs on the main thread (via the iced update loop). We only send
    // well-known AppKit messages, copy the bytes into an autoreleased `NSData`,
    // and null-check each result before using it.
    unsafe {
        // Inside a real `.app`, the main bundle has a `CFBundleIdentifier`; a
        // bare CLI binary's main bundle returns nil. Bail when bundled so the
        // OS-rendered (dynamic) icon stands.
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        if !bundle.is_null() {
            let identifier: *mut AnyObject = msg_send![bundle, bundleIdentifier];
            if !identifier.is_null() {
                return;
            }
        }

        // A 512px padded render of the macOS icon (the dock shows it as-is, so
        // it carries the ~80% gutter a packaged icon would otherwise get from
        // the OS). Kept small so the unbundled binary doesn't balloon.
        const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

        let data: *mut AnyObject = msg_send![
            class!(NSData),
            dataWithBytes: ICON_PNG.as_ptr() as *const c_void,
            length: ICON_PNG.len(),
        ];
        if data.is_null() {
            return;
        }
        let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![image, initWithData: data];
        if image.is_null() {
            return;
        }
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

/// Reposition the OS-drawn window controls to the vertical center of a
/// `bar_height`-tall title bar, so they line up with the centered tabs. macOS
/// pins the traffic lights to the center of the *native* ~28pt title bar and
/// won't move them through any iced/winit setting, so we reach the `NSWindow`
/// through the raw handle and nudge them ourselves. A no-op on other platforms
/// (and whenever the handle isn't an AppKit one). Must run on the main thread —
/// `iced::window::run` guarantees that.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn position_window_controls(handle: raw_window_handle::RawWindowHandle, bar_height: f32) {
    #[cfg(target_os = "macos")]
    {
        let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle else {
            return;
        };
        // SAFETY: `ns_view` is a live `NSView*` for our window, and this runs on
        // the main thread (via `window::run`). We only send well-known AppKit
        // messages and immediately consume the autoreleased results.
        unsafe { position_traffic_lights(appkit.ns_view.as_ptr(), bar_height) };
    }
}

/// Install a native observer that re-centers the traffic lights on every window
/// resize, **synchronously within AppKit's own resize handling**. Reacting to
/// winit's `Resized` through the iced message loop (see
/// `position_window_controls`) runs a frame *after* AppKit has already snapped
/// the buttons back to their default top position — that one stale frame, drawn
/// repeatedly while dragging an edge, is the up/down "jump". Re-centering inside
/// `windowDidResize:` happens before the frame is flushed, so the buttons never
/// visibly land at the default position. Install once; the observer lives for
/// the app's lifetime. No-op off macOS / non-AppKit handles.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn install_window_resize_observer(handle: raw_window_handle::RawWindowHandle, bar_height: f32) {
    #[cfg(target_os = "macos")]
    {
        let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle else {
            return;
        };
        resize_observer::install(appkit.ns_view.as_ptr(), bar_height);
    }
}

/// The `NSWindowDidResizeNotification` observer backing
/// [`install_window_resize_observer`]. A tiny `NSObject` subclass whose
/// `windowDidResize:` re-runs the same traffic-light centring as the iced path,
/// but on AppKit's timeline.
#[cfg(target_os = "macos")]
mod resize_observer {
    use std::ffi::c_void;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{DefinedClass, MainThreadOnly, class, define_class, msg_send, sel};
    use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString};

    pub(super) struct Ivars {
        bar_height: f32,
    }

    define_class!(
        // SAFETY:
        // - The superclass NSObject has no subclassing requirements.
        // - `ResizeObserver` does not implement `Drop`.
        #[unsafe(super = NSObject)]
        #[thread_kind = MainThreadOnly]
        #[ivars = Ivars]
        struct ResizeObserver;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for ResizeObserver {}

        impl ResizeObserver {
            // Fired by AppKit (on the main thread, synchronously during the
            // resize) for each `NSWindowDidResizeNotification` on our window.
            //
            // SAFETY: the selector takes one object argument — the notification,
            // whose `object` is the resized `NSWindow`.
            #[unsafe(method(windowDidResize:))]
            fn window_did_resize(&self, notification: &NSObject) {
                let window: *mut AnyObject = unsafe { msg_send![notification, object] };
                if window.is_null() {
                    return;
                }
                let content_view: *mut AnyObject = unsafe { msg_send![window, contentView] };
                if content_view.is_null() {
                    return;
                }
                // SAFETY: `content_view` is a live `NSView*`; we're on the main
                // thread and only send well-known AppKit messages.
                unsafe {
                    super::position_traffic_lights(
                        content_view as *mut c_void,
                        self.ivars().bar_height,
                    );
                }
            }
        }
    );

    impl ResizeObserver {
        fn new(mtm: MainThreadMarker, bar_height: f32) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { bar_height });
            // SAFETY: `NSObject`'s `init` is a correct designated initializer.
            unsafe { msg_send![super(this), init] }
        }
    }

    pub(super) fn install(ns_view: *mut c_void, bar_height: f32) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let ns_view = ns_view as *mut AnyObject;
        if ns_view.is_null() {
            return;
        }
        // SAFETY: `ns_view` is a live `NSView*`; `-window` returns its window.
        let window: *mut AnyObject = unsafe { msg_send![ns_view, window] };
        if window.is_null() {
            return;
        }

        let observer = ResizeObserver::new(mtm, bar_height);
        let name = NSString::from_str("NSWindowDidResizeNotification");
        let observer_ref: &AnyObject = &observer;
        // SAFETY: registering a well-formed observer + selector with the default
        // notification center; `name` and `window` are valid for the call.
        unsafe {
            let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
            let _: () = msg_send![
                center,
                addObserver: observer_ref,
                selector: sel!(windowDidResize:),
                name: &*name,
                object: window,
            ];
        }
        // The observer must outlive this call. There's exactly one window and it
        // lives until the process exits, so leak the observer rather than track
        // ownership we'd never reclaim.
        std::mem::forget(observer);
    }
}

/// macOS traffic-light repositioning. We resize the private
/// `NSTitlebarContainerView` (the buttons' grandparent) to span the bar and pin
/// it to the window top — the Tauri/Electron technique — then *measure* where
/// AppKit actually placed the close button and rigidly translate the container
/// so the button centers land exactly on `bar_height/2`, the line the centered
/// tabs sit on. Measuring beats assuming how AppKit anchors the buttons inside
/// the container: that's the detail I got wrong twice (too high, then too low),
/// and it isn't contractual across macOS versions anyway.
#[cfg(target_os = "macos")]
unsafe fn position_traffic_lights(ns_view: *mut std::ffi::c_void, bar_height: f32) {
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_foundation::{NSPoint, NSRect};

    let ns_view = ns_view as *mut AnyObject;
    if ns_view.is_null() {
        return;
    }
    let ns_window: *mut AnyObject = msg_send![ns_view, window];
    if ns_window.is_null() {
        return;
    }
    // `NSWindowButton::CloseButton` == 0.
    let close: *mut AnyObject = msg_send![ns_window, standardWindowButton: 0usize];
    if close.is_null() {
        return;
    }
    // close → NSTitlebarView → NSTitlebarContainerView.
    let titlebar: *mut AnyObject = msg_send![close, superview];
    if titlebar.is_null() {
        return;
    }
    let container: *mut AnyObject = msg_send![titlebar, superview];
    if container.is_null() {
        return;
    }

    let close_frame: NSRect = msg_send![close, frame];
    let button_height = close_frame.size.height;
    let window_frame: NSRect = msg_send![ns_window, frame];
    let window_height = window_frame.size.height;

    // Step 1: make the container span the bar and pin it to the window top, then
    // let AppKit re-lay-out the buttons inside it. The exact height only needs to
    // be tall enough to hold the buttons — the centering comes from step 2.
    let container_height = (bar_height as f64).max(button_height);
    let mut frame: NSRect = msg_send![container, frame];
    frame.origin.y = window_height - container_height;
    frame.size.height = container_height;
    let _: () = msg_send![container, setFrame: frame];
    let _: () = msg_send![container, layoutSubtreeIfNeeded];

    // Step 2: read where the close button actually landed (in window base
    // coords) and translate the container so the button's vertical center sits
    // on the bar's center. A pure origin shift moves the button rigidly, so this
    // is exact regardless of how AppKit anchored it inside the container.
    let bounds = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: close_frame.size,
    };
    let null_view: *mut AnyObject = std::ptr::null_mut();
    let in_window: NSRect = msg_send![close, convertRect: bounds, toView: null_view];
    let actual_center_y = in_window.origin.y + in_window.size.height / 2.0;
    let desired_center_y = window_height - bar_height as f64 / 2.0;

    let mut frame: NSRect = msg_send![container, frame];
    frame.origin.y += desired_center_y - actual_center_y;
    let _: () = msg_send![container, setFrame: frame];
}
