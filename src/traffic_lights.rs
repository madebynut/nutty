//! Keeps the macOS traffic lights out of sight while the window leaves full screen.
//!
//! The window sets the lights at a custom inset (centred on the 48 px header row).
//! While AppKit animates out of full screen it lays them out at their default spot
//! near the top edge, and GPUI moves them back to the inset only when the
//! animation ends, so they visibly jump. Hiding them for the transition and fading
//! them in at their final spot removes the jump.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::{msg_send, rc::Retained, runtime::AnyObject};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// `NSWindowCloseButton`.
const CLOSE_BUTTON: usize = 0;

/// Hides the window's traffic lights from the start of a full screen exit until it
/// ends (or fails), then fades them in.
pub(crate) fn install(window: &gpui_kit::Window) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: `ns_view` is the live content view of this GPUI window, and this runs
    // on the main thread that owns it.
    let native: Option<Retained<AnyObject>> =
        unsafe { msg_send![handle.ns_view.cast::<AnyObject>().as_ref(), window] };
    let Some(native) = native else {
        return;
    };
    let center = NSNotificationCenter::defaultCenter();
    for (name, visible) in [
        ("NSWindowWillExitFullScreenNotification", false),
        ("NSWindowDidExitFullScreenNotification", true),
        ("NSWindowDidFailToExitFullScreenNotification", true),
    ] {
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: AppKit passes a live notification for the duration of the call.
            if let Some(window) = unsafe { notification.as_ref() }.object() {
                set_visible(&window, visible);
            }
        });
        // The block runs synchronously on the posting (main) thread. Nutty has one
        // window that lives as long as the process, so the observers stay registered;
        // the notification centre keeps them alive.
        // SAFETY: the observed object is this NSWindow and the block only touches
        // AppKit views from the main thread the notifications are posted on.
        let _ = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(&NSString::from_str(name)),
                Some(&native),
                None,
                &block,
            )
        };
    }
}

/// Shows or hides the titlebar container that holds the buttons (the close
/// button's grandparent view, the same view GPUI resizes to place them).
fn set_visible(window: &AnyObject, visible: bool) {
    // SAFETY: `window` is the NSWindow the notification was posted for, and these
    // are plain NSView/NSWindow messages on the main thread. Each step may return
    // nil, which ends the walk.
    unsafe {
        let close: Option<Retained<AnyObject>> =
            msg_send![window, standardWindowButton: CLOSE_BUTTON];
        let Some(close) = close else {
            return;
        };
        let buttons: Option<Retained<AnyObject>> = msg_send![&*close, superview];
        let Some(buttons) = buttons else {
            return;
        };
        let container: Option<Retained<AnyObject>> = msg_send![&*buttons, superview];
        let Some(container) = container else {
            return;
        };
        if visible {
            let _: () = msg_send![&*container, setAlphaValue: 0.0f64];
            let _: () = msg_send![&*container, setHidden: false];
            let animator: Retained<AnyObject> = msg_send![&*container, animator];
            let _: () = msg_send![&*animator, setAlphaValue: 1.0f64];
        } else {
            let _: () = msg_send![&*container, setHidden: true];
        }
    }
}
