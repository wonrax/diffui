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

/// Resolve a title-bar double-click for the caller to act on: the window's
/// current frame, its screen's visible frame (both AppKit screen coords
/// `[x, y, w, h]`), and the configured action read from the same
/// `AppleActionOnDoubleClick` user-default AppKit consults — `0` = zoom (the
/// unset default), `1` = minimize, `2` = none. The caller drives the zoom
/// animation itself (see [`set_window_frame`]); we only *read* here. Returns a
/// zero-size visible frame + action `2` off macOS / non-AppKit handles so the
/// caller no-ops. Must run on the main thread (`window::run` guarantees it).
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn read_double_click_plan(
    handle: raw_window_handle::RawWindowHandle,
) -> ([f64; 4], [f64; 4], u8, f64) {
    #[cfg(target_os = "macos")]
    {
        if let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle {
            // SAFETY: `ns_view` is a live `NSView*` and this runs on the main
            // thread. We only send well-known AppKit messages and read a default.
            return unsafe { read_double_click_plan_impl(appkit.ns_view.as_ptr()) };
        }
    }
    ([0.0; 4], [0.0; 4], 2, 0.0)
}

/// Set the window's frame instantly (no AppKit animation), wrapped in a
/// `CATransaction` with implicit actions disabled so the GPU layer doesn't
/// interpolate (scale) its contents between sizes. Called once per animation
/// frame by the zoom tick, so the resize routes through the same live-repaint
/// path an edge-drag uses. `frame` is AppKit screen coords `[x, y, w, h]`. No-op
/// off macOS / non-AppKit handles. Must run on the main thread.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn set_window_frame(handle: raw_window_handle::RawWindowHandle, frame: [f64; 4]) {
    #[cfg(target_os = "macos")]
    {
        if let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle {
            // SAFETY: `ns_view` is a live `NSView*` and this runs on the main
            // thread. We only send well-known AppKit messages.
            unsafe { set_window_frame_impl(appkit.ns_view.as_ptr(), frame) };
        }
    }
}

/// The current OS appearance (light/dark), read live from the shared
/// application's `effectiveAppearance`. We never set `NSApp`'s own appearance,
/// so this still tracks the system even while iced has pinned the *window's*
/// appearance to our resolved theme — which is exactly what makes winit stop
/// reporting appearance changes (its observer ignores them once the window has
/// an explicit appearance). Polling this is how the strip keeps following the OS
/// without a restart. `Mode::None` means "couldn't tell" (off macOS, or AppKit
/// returned nothing); callers treat that as "leave the current resolution be".
/// Must run on the main thread — the iced update loop, our only caller, does.
pub fn system_appearance() -> iced::theme::Mode {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: reads well-known AppKit appearance APIs on the main thread.
        // `NSApplication` is process-global and these calls don't mutate state.
        unsafe { read_system_appearance() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        iced::theme::Mode::None
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

/// Stop AppKit from moving the window when the user presses-and-drags inside our
/// title-bar strip. With `fullSizeContentView` the content view reaches up into
/// the title-bar region, where NSView's default `mouseDownCanMoveWindow` (YES for
/// a non-opaque, non-control view) hands any press-drag there straight to AppKit's
/// window move — even right on top of a tab, before our iced handlers see the
/// event, so iced-level click capturing can't stop it. We override the method to
/// NO, leaving the strip movable only through our explicit `TitleBarDrag` (the
/// empty-area `mouse_area`). The window stays `movable`, so the
/// `performWindowDragWithEvent:` drag behind `TitleBarDrag` still works. Run once
/// at startup. No-op off macOS / non-AppKit handles.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn configure_custom_titlebar(handle: raw_window_handle::RawWindowHandle) {
    #[cfg(target_os = "macos")]
    {
        let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle else {
            return;
        };
        // SAFETY: `ns_view` is a live `NSView*` and this runs on the main thread
        // (via `window::run`). Overriding `mouseDownCanMoveWindow` on its class
        // only changes that one method's return value.
        unsafe { override_mouse_down_can_move_window(appkit.ns_view.as_ptr()) };
    }
}

/// Replace `-mouseDownCanMoveWindow` on the content view's class with a constant
/// `NO`. The view's concrete class (winit's) doesn't define the method itself —
/// it inherits NSView's — so this adds an override on that class only, leaving
/// `NSView` and every other view untouched. There's a single window for the
/// process's life, so doing it once is enough.
#[cfg(target_os = "macos")]
unsafe fn override_mouse_down_can_move_window(ns_view: *mut std::ffi::c_void) {
    use objc2::ffi::class_replaceMethod;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::{msg_send, sel};

    let view = ns_view as *mut AnyObject;
    if view.is_null() {
        return;
    }
    let class: *mut AnyClass = msg_send![view, class];
    if class.is_null() {
        return;
    }

    extern "C-unwind" fn no_move(_this: *mut AnyObject, _cmd: Sel) -> Bool {
        Bool::NO
    }

    // SAFETY: `no_move` has exactly the `(self, _cmd) -> BOOL` shape AppKit
    // invokes `mouseDownCanMoveWindow` with; transmuting to the type-erased
    // `Imp` only reinterprets the function pointer. "B@:" is that signature's
    // encoding (BOOL return, self, selector), and `class` is a live class.
    let imp: Imp = unsafe {
        std::mem::transmute(no_move as extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool)
    };
    let _ =
        unsafe { class_replaceMethod(class, sel!(mouseDownCanMoveWindow), imp, c"B@:".as_ptr()) };
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

/// Read the window frame, its screen's visible frame, and the configured
/// double-click action behind [`read_double_click_plan`]. Action: `1` =
/// "Minimize", `2` = "None", `0` = zoom (anything else, including the unset
/// default — `stringForKey:` returns nil then).
#[cfg(target_os = "macos")]
unsafe fn read_double_click_plan_impl(
    ns_view: *mut std::ffi::c_void,
) -> ([f64; 4], [f64; 4], u8, f64) {
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use objc2_foundation::{NSRect, NSString};

    let none = ([0.0; 4], [0.0; 4], 2u8, 0.0);
    let ns_view = ns_view as *mut AnyObject;
    if ns_view.is_null() {
        return none;
    }
    let ns_window: *mut AnyObject = msg_send![ns_view, window];
    if ns_window.is_null() {
        return none;
    }

    let window_frame: NSRect = msg_send![ns_window, frame];
    // The window's own screen (it follows the title bar across displays); fall
    // back to the main screen if AppKit hands us nil (e.g. fully off-screen).
    let mut screen: *mut AnyObject = msg_send![ns_window, screen];
    if screen.is_null() {
        screen = msg_send![class!(NSScreen), mainScreen];
    }
    if screen.is_null() {
        return none;
    }
    let visible_frame: NSRect = msg_send![screen, visibleFrame];
    // AppKit's own duration for animating *this* window to the visible frame —
    // the value a native zoom would use. Scales with the resize distance, so we
    // query rather than hardcode. (For the symmetric un-zoom the window already
    // sits at the visible frame, so this reads ~0; the caller reuses the saved
    // zoom-in duration there instead.)
    let duration: f64 = msg_send![ns_window, animationResizeTime: visible_frame];

    let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
    let key = NSString::from_str("AppleActionOnDoubleClick");
    let action_str: *mut AnyObject = msg_send![defaults, stringForKey: &*key];
    let action = if action_str.is_null() {
        0
    } else {
        let minimize = NSString::from_str("Minimize");
        let is_minimize: Bool = msg_send![action_str, isEqualToString: &*minimize];
        let none_str = NSString::from_str("None");
        let is_none: Bool = msg_send![action_str, isEqualToString: &*none_str];
        if is_minimize.as_bool() {
            1
        } else if is_none.as_bool() {
            2
        } else {
            0
        }
    };

    let rect = |r: NSRect| [r.origin.x, r.origin.y, r.size.width, r.size.height];
    (rect(window_frame), rect(visible_frame), action, duration)
}

/// Apply `frame` to the window with no animation, inside a `CATransaction` that
/// disables implicit layer actions — see [`set_window_frame`].
#[cfg(target_os = "macos")]
unsafe fn set_window_frame_impl(ns_view: *mut std::ffi::c_void, frame: [f64; 4]) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let ns_view = ns_view as *mut AnyObject;
    if ns_view.is_null() {
        return;
    }
    let ns_window: *mut AnyObject = msg_send![ns_view, window];
    if ns_window.is_null() {
        return;
    }

    let rect = NSRect {
        origin: NSPoint {
            x: frame[0],
            y: frame[1],
        },
        size: NSSize {
            width: frame[2],
            height: frame[3],
        },
    };
    let _: () = msg_send![class!(CATransaction), begin];
    let _: () = msg_send![class!(CATransaction), setDisableActions: true];
    let _: () = msg_send![ns_window, setFrame: rect, display: true, animate: false];
    let _: () = msg_send![class!(CATransaction), commit];
}

/// Read `NSApp.effectiveAppearance` and collapse it to light/dark by asking
/// AppKit which of the two standard appearance names it best matches — the same
/// determination winit makes, so our answer agrees with what iced would resolve.
#[cfg(target_os = "macos")]
unsafe fn read_system_appearance() -> iced::theme::Mode {
    use iced::theme::Mode;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    if app.is_null() {
        return Mode::None;
    }
    let appearance: *mut AnyObject = msg_send![app, effectiveAppearance];
    if appearance.is_null() {
        return Mode::None;
    }

    // bestMatchFromAppearancesWithNames:[Aqua, DarkAqua] returns whichever name
    // the effective appearance resolves to (the high-contrast variants collapse
    // onto their base light/dark name here).
    let aqua = NSString::from_str("NSAppearanceNameAqua");
    let dark = NSString::from_str("NSAppearanceNameDarkAqua");
    let names: *mut AnyObject = msg_send![class!(NSMutableArray), array];
    if names.is_null() {
        return Mode::None;
    }
    let _: () = msg_send![names, addObject: &*aqua];
    let _: () = msg_send![names, addObject: &*dark];

    let best: *mut AnyObject = msg_send![appearance, bestMatchFromAppearancesWithNames: names];
    if best.is_null() {
        return Mode::None;
    }
    let is_dark: Bool = msg_send![best, isEqualToString: &*dark];
    if is_dark.as_bool() {
        Mode::Dark
    } else {
        Mode::Light
    }
}
