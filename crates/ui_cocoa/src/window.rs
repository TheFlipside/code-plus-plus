//! The main window class.
//!
//! An `NSWindow` subclass whose only job is to refuse keyboard focus to
//! views that must never receive it — today, the Document Map's
//! miniature.
//!
//! # Why this exists at all
//!
//! The miniature is a second Scintilla view bound, through
//! `SCI_SETDOCPOINTER`, to the *same editable document* the main editor
//! shows. `crate::docmap` documents why `SCI_SETREADONLY` cannot express
//! "do not edit this" (it is a document property, so it would freeze the
//! main editor too) and closes the mouse half of the guarantee with an
//! overlay that swallows every event. The keyboard half needs this.
//!
//! `ui_gtk` says `miniature.set_can_focus(false)` and is done. AppKit has
//! no per-view equivalent — `acceptsFirstResponder` is a *method on the
//! view*, and the view here is vendored (`ScintillaView.mm:420` returns
//! `YES` unconditionally, and `SCIContentView` does the same). So the
//! guard goes at the only place every route converges:
//! `-[NSWindow makeFirstResponder:]`, which Tab-cycling, a click and any
//! programmatic assignment all go through.
//!
//! # It was reachable, and it was measured
//!
//! Driving the real app: `selectNextKeyView:` — the exact call AppKit's
//! Tab handling makes — **alternates** between the editor's
//! `SCIContentView` and the miniature's, and five synthetic keystrokes
//! after tabbing in moved the document length 12 189 → 12 194. The user
//! sees nothing, because `configure_miniature` sets
//! `CARETSTYLE_INVISIBLE`, so there is no insertion point to notice.
//!
//! Worth recording that the *first* probe reported this as unreachable.
//! Its "is the responder inside the map?" helper took its own
//! `with_state` borrow from inside an outer one, so it was declined
//! re-entrantly and answered `false` for every input — a clean, confident
//! "no" from an instrument that could not say anything else. The rewrite
//! carries a control that types into the main editor first and asserts
//! the length moves, so an instrument that cannot see an edit fails
//! loudly instead of exonerating the code.

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{define_class, msg_send, MainThreadOnly, Message};
use objc2_app_kit::{NSBackingStoreType, NSResponder, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSRect};

define_class!(
    // SAFETY: an `NSWindow` subclass overriding one method, which it
    // chains to `super` in every case but the one it exists to refuse.
    // Main-thread-only, as every window is.
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppMainWindow"]
    pub struct MainWindow;

    unsafe impl NSObjectProtocol for MainWindow {}

    impl MainWindow {
        /// Refuse focus to anything inside a no-focus panel; otherwise
        /// behave exactly as `NSWindow` does.
        ///
        /// Returning `false` is the documented way to decline: AppKit
        /// leaves the previous first responder in place, so Tab is a
        /// no-op rather than dropping focus on the floor. Measured —
        /// see the module docs.
        #[unsafe(method(makeFirstResponder:))]
        fn make_first_responder(&self, responder: Option<&NSResponder>) -> Bool {
            crate::at_callback_boundary("window:makeFirstResponder", Bool::NO, || {
                if let Some(view) = responder.and_then(|r| r.downcast_ref::<NSView>()) {
                    if crate::docmap::owns_view(view) {
                        return Bool::NO;
                    }
                }
                // SAFETY: plain chain to `NSWindow`'s implementation with
                // the arguments it was given.
                unsafe { msg_send![super(self), makeFirstResponder: responder] }
            })
        }
    }
);

impl MainWindow {
    /// Build the main window. Same designated initialiser `run` used
    /// before this class existed, so nothing about geometry or style
    /// changes — only the class.
    pub fn new(
        content_rect: NSRect,
        style: NSWindowStyleMask,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        // SAFETY: `NSWindow`'s designated initialiser on a freshly
        // allocated instance of our own subclass, which adds no ivars
        // needing other initialisation. `defer: false` asks AppKit to
        // create the window-server resources now rather than on first
        // display, which is what the pre-subclass code asked for too.
        unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: content_rect,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        }
    }
}

/// Walk up from `view` looking for `ancestor`.
///
/// Lives here rather than in `docmap` so the traversal has one
/// implementation; the panel supplies the ancestor.
///
/// **This wants object *identity*, and gets it by inheritance rather
/// than by asking for it.** `==` on a `Retained<NSView>` goes through
/// `-isEqual:`, which `NSObject` implements as pointer equality and
/// `NSView` does not override — so today the two coincide. A future
/// `NSView` subclass in this tree that gave `isEqual:` value semantics
/// would silently change what "is inside the panel" means for
/// [`crate::docmap::owns_view`], which is a focus guard. Nothing in the
/// type system says so, hence the note.
pub(crate) fn is_descendant_of(view: &NSView, ancestor: &NSView) -> bool {
    let mut cur: Option<Retained<NSView>> = Some(view.retain());
    while let Some(c) = cur {
        if &*c == ancestor {
            return true;
        }
        // SAFETY: `superview` is a plain accessor; objc2 marks it unsafe
        // only because `NSView` is not `Send`/`Sync`, and this runs on
        // the main thread by construction (every caller is a main-thread
        // AppKit callback).
        cur = unsafe { c.superview() };
    }
    false
}
