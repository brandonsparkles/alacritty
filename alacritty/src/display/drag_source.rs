//! macOS-only: minimal `NSDraggingSource` conformer used to initiate
//! drag-out of selected terminal text (iTerm-style).
//!
//! The dragged data is published to a *transient* drag pasteboard owned by
//! the drag session — it does NOT touch the user's system clipboard.

#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting,
    NSView,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

define_class!(
    /// Empty marker class implementing `NSDraggingSource` so AppKit accepts
    /// us as a drag source. The conforming method just reports that we
    /// support copy operations.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AlacrittyDragSource"]
    pub struct AlacrittyDragSource;

    unsafe impl NSObjectProtocol for AlacrittyDragSource {}

    unsafe impl NSDraggingSource for AlacrittyDragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn dragging_session_source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }
    }
);

impl AlacrittyDragSource {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>();
        unsafe { msg_send![this, init] }
    }
}

/// Begin a system drag operation publishing `text` on the drag pasteboard.
///
/// `ns_view` is the view that owns the drag (typically the window's content
/// view). `origin_pt` is the press point in view coordinates — AppKit uses it
/// as the upper-left corner of the drag image.
///
/// Returns `true` if AppKit accepted the drag (we had a current event to
/// anchor it to), `false` otherwise. A `false` return means the caller
/// should fall through to whatever normal click handling would have done.
pub fn begin_text_drag(ns_view: &NSView, origin_pt: NSPoint, text: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };

    // Anchor the drag to the most-recent NSEvent currently being processed.
    let app = NSApplication::sharedApplication(mtm);
    let Some(event) = app.currentEvent() else {
        return false;
    };
    if !valid_drag_anchor_event(&event) {
        return false;
    }

    // Build a pasteboard item carrying the text.
    let item = NSPasteboardItem::new();
    let ns_string = NSString::from_str(text);
    let ok = unsafe { item.setString_forType(&ns_string, NSPasteboardTypeString) };
    if !ok {
        return false;
    }
    let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);

    // Wrap it in a dragging item and place it at the press point.
    let dragging_item =
        NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
    let frame = NSRect::new(origin_pt, NSSize::new(0.0, 0.0));
    dragging_item.setDraggingFrame(frame);

    let items: Retained<NSArray<NSDraggingItem>> = NSArray::from_retained_slice(&[dragging_item]);

    let source = AlacrittyDragSource::new(mtm);
    let source_proto: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*source);

    let _ = ns_view.beginDraggingSessionWithItems_event_source(&items, &event, source_proto);
    true
}

/// AppKit's `beginDraggingSession` insists on either a mouseDown or mouseDragged
/// NSEvent. If `NSApp.currentEvent` is anything else (keyDown, applicationDefined,
/// scroll wheel) the call will throw `NSInvalidArgumentException`. Filter here.
fn valid_drag_anchor_event(event: &NSEvent) -> bool {
    use objc2_app_kit::NSEventType;
    let ty = event.r#type();
    matches!(
        ty,
        NSEventType::LeftMouseDown
            | NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDown
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDown
            | NSEventType::OtherMouseDragged
    )
}
