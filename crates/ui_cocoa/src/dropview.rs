//! The window's content view, which also accepts dropped files.
//!
//! Win32 gets this from `DragAcceptFiles` + `WM_DROPFILES` and GTK from
//! `gtk_drag_dest_set` + `drag-data-received`. Cocoa's equivalent is the
//! `NSDraggingDestination` protocol, which is implemented by a *view*
//! rather than configured on a window — hence a custom `NSView` subclass
//! rather than the plain one m2 used.
//!
//! It is the content view specifically, so the whole window area is a
//! drop target: dropping onto the editor, the tab strip or the status
//! bar all work, which is what a user expects and what the other two
//! backends do.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, ClassType, MainThreadOnly};
use objc2_app_kit::{
    NSDraggingDestination, NSDraggingInfo, NSPasteboard, NSPasteboardTypeFileURL, NSView,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSCopying, NSObjectProtocol, NSRect, NSURL};
use std::path::PathBuf;

define_class!(
    // SAFETY: an `NSView` subclass that overrides nothing of `NSView`'s
    // own behaviour — it only adds the dragging-destination protocol
    // methods. Main-thread-only because every callback arrives there.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppContentView"]
    pub struct ContentView;

    unsafe impl NSObjectProtocol for ContentView {}

    unsafe impl NSDraggingDestination for ContentView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> usize {
            // `NSDragOperationCopy` is 1. Advertised only when the drag
            // actually carries filenames, so dragging arbitrary content
            // (a colour swatch, a text selection) shows the "no" cursor
            // rather than promising something we would then ignore.
            //
            // `NSDragOperationNone` (0) is the right thing to fall back
            // to if the guard fires: refusing the drop is the safe answer.
            crate::at_callback_boundary("draggingEntered:", 0, || {
                usize::from(!dragged_paths(sender).is_empty())
            })
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Bool {
            // Falls back to `NO` — "the drop was not handled" — which
            // is what AppKit should be told if we could not complete it.
            crate::at_callback_boundary("performDragOperation:", Bool::NO, || {
                let paths = dragged_paths(sender);
                if paths.is_empty() {
                    return Bool::NO;
                }
                for path in paths {
                    crate::open_dropped_path(path);
                }
                Bool::YES
            })
        }
    }
);

impl ContentView {
    /// Build the content view and register it for filename drags.
    pub fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser
        // and this subclass adds no ivars needing other initialisation.
        let view: Retained<Self> = unsafe { msg_send![this, initWithFrame: frame] };
        // Without this the protocol methods above are never called: a
        // view receives drags only for the types it has registered.
        // `NSPasteboardTypeFileURL` rather than the older
        // `NSFilenamesPboardType`, which AppKit deprecates: the modern
        // type carries one file URL per pasteboard *item*, which is also
        // why the read below asks for objects rather than a property
        // list.
        unsafe {
            let types = NSArray::from_retained_slice(&[NSPasteboardTypeFileURL.copy()]);
            view.registerForDraggedTypes(&types);
        }
        view
    }
}

/// Extract filesystem paths from a drag, or an empty vector if it
/// carries none.
///
/// Reads `NSURL` objects rather than a filename property list: that is
/// the non-deprecated path, and it also means AppKit does the URL
/// decoding, so a filename containing spaces or percent-encodable
/// characters arrives intact.
fn dragged_paths(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let pasteboard: Retained<NSPasteboard> = sender.draggingPasteboard();
    let classes = NSArray::from_slice(&[NSURL::class() as &AnyClass]);
    // SAFETY: `readObjectsForClasses:options:` with a class array and no
    // options is the documented read path; it returns null when nothing
    // on the pasteboard matches, which `Option` models.
    let Some(objects) = (unsafe { pasteboard.readObjectsForClasses_options(&classes, None) })
    else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|item: Retained<AnyObject>| {
            let url = item.downcast::<NSURL>().ok()?;
            // **Scheme check, not an optimisation.** `-[NSURL path]`
            // returns the path *component* of any hierarchical URL, so
            // `http://evil/etc/passwd` yields `/etc/passwd` — it does
            // not return nil for non-file URLs. And
            // `readObjectsForClasses:options:` without
            // `NSPasteboardURLReadingFileURLsOnly` returns every URL on
            // the pasteboard, not only file ones. Without this gate a
            // drag carrying a spoofed payload could hand an arbitrary
            // absolute path to `Shell::open_file` as though the user had
            // picked a local file. `ui_gtk` rejects non-`file:` schemes
            // at the same boundary (its `uri_to_local_path`); this is
            // the Cocoa equivalent.
            if !url.isFileURL() {
                return None;
            }
            url.path().map(|p| PathBuf::from(p.to_string()))
        })
        .collect()
}
