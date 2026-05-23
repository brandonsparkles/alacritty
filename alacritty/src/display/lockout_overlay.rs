//! Lockout overlay — a fullscreen opaque view that covers each alacritty
//! window's content area when the daily-usage budget block is in effect.
//! macOS only.
//!
//! Rendered via AppKit (NSView + NSTextField + NSButton) rather than the
//! existing GL renderer because:
//!
//!   * The overlay must absorb keystrokes that would otherwise hit the
//!     terminal — NSView's first-responder semantics handle this for free.
//!   * The optional courtesy button must be a real clickable HUD element,
//!     not a synthesized PTY interaction.
//!   * NSView lives outside the GL surface, so re-rendering doesn't fight
//!     with the terminal grid renderer's frame loop.
//!
//! ### Lifecycle
//!
//! * [`install_or_update`] is called every budget tick when the block is
//!   active. First call creates the overlay subview; subsequent calls
//!   update the countdown label in place. Idempotent.
//! * [`remove`] is called when the block lifts (next tick after
//!   `block_status() == None`). Drops the overlay from the contentView.
//! * The courtesy button dispatches `EventType::GrantCourtesy` to the
//!   alacritty event loop via a global `EventLoopProxy` registered at
//!   startup (see [`register_event_proxy`]).
//!
//! ### Identification
//!
//! AppKit lookup is deliberately avoided for the overlay subviews. Plain
//! `NSView` does not support UIKit-style `tag` lookup, and an Objective-C
//! exception here aborts the Rust process. Instead, retained overlay views
//! live in a main-thread Rust registry keyed by the `NSWindow` pointer.

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, ClassType, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSButton, NSColor, NSFont, NSTextAlignment, NSTextField, NSView,
    NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};

thread_local! {
    static OVERLAYS: RefCell<HashMap<usize, OverlayViews>> = RefCell::new(HashMap::new());
}

struct OverlayViews {
    root: Retained<NSView>,
    countdown: Retained<NSTextField>,
    button: Retained<NSButton>,
}

/// Single shared event proxy for dispatching `GrantCourtesy` from the
/// button-click target. Set once at process startup by `Processor::new`.
static EVENT_PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();

/// Singleton target object that owns the button-click action. Created
/// lazily on first overlay install; kept alive for the lifetime of the
/// process so the button's `target` reference stays valid.
static COURTESY_TARGET: OnceLock<Retained<CourtesyTarget>> = OnceLock::new();

/// Register the alacritty event-loop proxy with the overlay module.
/// Must be called once at startup before any overlay is installed.
pub fn register_event_proxy(proxy: EventLoopProxy<Event>) {
    let _ = EVENT_PROXY.set(proxy);
}

// ---------------------------------------------------------------------------
// Courtesy button target — an Obj-C class with a method the NSButton can
// invoke via target/action.
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AlacrittyCourtesyTarget"]
    #[derive(Debug)]
    struct CourtesyTarget;

    impl CourtesyTarget {
        /// Selector: `grantCourtesy:`. Dispatches a `GrantCourtesy` event
        /// to the alacritty main loop. If the proxy isn't registered yet
        /// (shouldn't happen in normal startup), the click is silently
        /// dropped.
        #[unsafe(method(grantCourtesy:))]
        fn grant_courtesy(&self, _sender: *mut NSObject) {
            if let Some(proxy) = EVENT_PROXY.get() {
                let _ = proxy.send_event(Event::new(EventType::GrantCourtesy, None));
            }
        }
    }

    unsafe impl NSObjectProtocol for CourtesyTarget {}
);

fn courtesy_target(_mtm: MainThreadMarker) -> &'static Retained<CourtesyTarget> {
    COURTESY_TARGET.get_or_init(|| {
        // `CourtesyTarget::class()` registers the `define_class!` class before
        // returning the Obj-C class pointer.
        unsafe { Retained::from_raw(msg_send![CourtesyTarget::class(), new]) }.unwrap()
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Install the overlay if absent, or refresh its countdown text if it's
/// already there. Safe to call every tick.
pub fn install_or_update(window: &NSWindow, unlock_seconds: u64, courtesy_available: bool) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(content) = window.contentView() else { return };
    let bounds = content.bounds();
    let window_key = window as *const NSWindow as usize;

    // Existing overlay? Just update the countdown.
    if OVERLAYS.with_borrow(|overlays| overlays.contains_key(&window_key)) {
        OVERLAYS.with_borrow(|overlays| {
            let Some(overlay) = overlays.get(&window_key) else { return };
            let cd_obj: &AnyObject =
                unsafe { &*(&*overlay.countdown as *const NSTextField as *const AnyObject) };
            let s = NSString::from_str(&format_countdown(unlock_seconds));
            unsafe {
                let _: () = msg_send![cd_obj, setStringValue: &*s];
                let _: () = msg_send![&*overlay.button, setHidden: !courtesy_available];
            }
        });
        return;
    }

    // Build a fresh overlay from scratch.
    let overlay: Retained<NSView> =
        unsafe { NSView::initWithFrame(NSView::alloc(mtm), bounds) };
    unsafe {
        overlay.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let _: () = msg_send![&*overlay, setWantsLayer: true];
        // Pull the CALayer out and set its backgroundColor to opaque black.
        let layer: *mut AnyObject = msg_send![&*overlay, layer];
        if !layer.is_null() {
            let black = NSColor::blackColor();
            let cg: *mut AnyObject = msg_send![&*black, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: cg];
        }
    }

    // ── Title: 🔒 LIMIT REACHED
    let title = make_label(
        mtm,
        "🔒 LIMIT REACHED",
        48.0,
        true, // bold
        NSRect {
            origin: NSPoint { x: 0.0, y: bounds.size.height * 0.55 },
            size: NSSize { width: bounds.size.width, height: 80.0 },
        },
    );
    unsafe {
        title.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        overlay.addSubview(&title);
    }

    // ── Countdown
    let countdown = make_label(
        mtm,
        &format_countdown(unlock_seconds),
        22.0,
        false,
        NSRect {
            origin: NSPoint { x: 0.0, y: bounds.size.height * 0.45 },
            size: NSSize { width: bounds.size.width, height: 40.0 },
        },
    );
    unsafe {
        let cd_view: &NSView = std::mem::transmute(&*countdown);
        cd_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        overlay.addSubview(cd_view);
    }

    // ── Courtesy button (centered, ~180×40, below the countdown)
    let btn_width = 220.0;
    let btn_height = 40.0;
    let btn_x = (bounds.size.width - btn_width) / 2.0;
    let btn_y = bounds.size.height * 0.32;
    let button: Retained<NSButton> = unsafe {
        NSButton::initWithFrame(
            NSButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: btn_x, y: btn_y },
                size: NSSize { width: btn_width, height: btn_height },
            },
        )
    };
    let target = courtesy_target(mtm);
    unsafe {
        button.setTitle(&NSString::from_str("Use 5-min courtesy"));
        let btn_view: &NSView = std::mem::transmute(&*button);
        btn_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        let target_obj: &NSObject = std::mem::transmute(&**target);
        let _: () = msg_send![&*button, setTarget: target_obj];
        let _: () = msg_send![&*button, setAction: sel!(grantCourtesy:)];
        if !courtesy_available {
            let _: () = msg_send![&*button, setHidden: true];
        }
        overlay.addSubview(btn_view);
    }

    unsafe {
        content.addSubview(&overlay);
    }

    OVERLAYS.with_borrow_mut(|overlays| {
        overlays.insert(window_key, OverlayViews { root: overlay, countdown, button });
    });
}

/// Remove the overlay if installed. No-op otherwise.
pub fn remove(window: &NSWindow) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    let window_key = window as *const NSWindow as usize;
    if let Some(overlay) = OVERLAYS.with_borrow_mut(|overlays| overlays.remove(&window_key)) {
        unsafe { overlay.root.removeFromSuperview() };
    };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_countdown(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("Unlocks in {h}h {m}m")
    } else if m > 0 {
        format!("Unlocks in {m}m {s:02}s")
    } else {
        format!("Unlocks in {s}s")
    }
}

fn make_label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    bold: bool,
    frame: NSRect,
) -> Retained<NSTextField> {
    let field: Retained<NSTextField> =
        unsafe { NSTextField::initWithFrame(NSTextField::alloc(mtm), frame) };
    unsafe {
        field.setStringValue(&NSString::from_str(text));
        field.setEditable(false);
        field.setSelectable(false);
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setBordered(false);
        field.setAlignment(NSTextAlignment::Center);
        let font = if bold {
            NSFont::boldSystemFontOfSize(size)
        } else {
            NSFont::systemFontOfSize(size)
        };
        field.setFont(Some(&font));
        let _: () = msg_send![&*field, setTextColor: &*NSColor::whiteColor()];
    }
    field
}
