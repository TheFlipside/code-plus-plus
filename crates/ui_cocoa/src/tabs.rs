//! The tab strip.
//!
//! # It is a selector, not a container
//!
//! The one Scintilla view is the strip's **sibling**, not its child.
//! Clicking a tab does not swap views — it rebinds the single view to
//! that tab's document with `SCI_SETDOCPOINTER`. That is the same model
//! Win32 and GTK use, and on this backend it is load-bearing rather than
//! merely consistent: `EditorHandle` is `Copy` with no lifetime, so a
//! per-tab `NSView` would reintroduce exactly the dangling-pointer
//! problem DESIGN.md §7.4 names when it says "a `ui_cocoa` that reaches
//! for one `NSView` per tab inherits the problem the single-view model
//! avoids".
//!
//! # Why `NSButton`s rather than custom drawing
//!
//! `NSTabView` is unsuitable (it owns per-tab content views, which is
//! precisely the model above rules out) and a fully custom-drawn strip
//! means hand-rolling hit-testing, hover states, text truncation and
//! accessibility. Composing each tab from controls is what `ui_gtk` does
//! too — its strip is a `GtkNotebook` carrying label widgets and close
//! buttons, not a drawing surface — so this stays parallel to the
//! backend it is ported from and inherits native look, keyboard
//! accessibility and hover feedback for free.
//!
//! # Why the tab body overrides `mouseDown:` instead of using an action
//!
//! m3a wired the body button with an ordinary target/action pair, which
//! is enough to *select* a tab but leaves no room for a drag: AppKit
//! sends the action on mouse-**up**, by which point the gesture is over.
//! `NSButton` has no reorder affordance of its own — unlike
//! `GtkNotebook`, which gets drag-reorder from `set_tab_reorderable` —
//! so [`TabButton`] takes the event over and runs the tracking loop
//! itself, deciding between "click" and "drag" from what the mouse
//! actually did (see [`TabButton::track`]).
//!
//! A consequence worth knowing: because the loop never calls
//! `super mouseDown:`, the cell's own push-on/push-off toggle never
//! runs. Selection is therefore always painted from the model in
//! [`TabStrip::sync`] and never from AppKit's internal button state,
//! which is the same "never infer selection from a signal" rule
//! `ui_gtk`'s strip documents.
//!
//! # Why the per-tab controls key on buffer id
//!
//! Each tab's body, pin and close all carry the buffer's **`Tab.id` in
//! their `tag`**, never a vector index. Indices go stale: a plugin
//! `NPPM_*` call, a `⌘W`, or a drag can reorder or shrink `Shell.tabs`
//! between the control being built and it being clicked, and an index
//! captured beforehand would then address a different buffer — "clicked
//! X, closed Y". DESIGN.md §7.4 tracks exactly that bug on the Win32
//! strip. Ids are allocated monotonically without reuse, so a stale one
//! resolves to "gone" rather than to somebody else's buffer.

use std::cell::Cell;

use codepp_shell::Tab;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBezelStyle, NSBezierPath, NSBitmapImageRep,
    NSButton, NSButtonType, NSCellImagePosition, NSColor, NSEvent, NSEventMask,
    NSEventTrackingRunLoopMode, NSEventType, NSFont, NSImage, NSImageRep, NSLineBreakMode, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSData, NSDate, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::menu::Actions;

/// Height of the strip in points.
pub const TAB_STRIP_HEIGHT: f64 = 26.0;

/// Width budget for one tab, including its pin and close controls.
const TAB_WIDTH: f64 = 150.0;
/// Width of the close button at the trailing edge of each tab.
const CLOSE_WIDTH: f64 = 18.0;
/// Width of the pin glyph, immediately left of the close button.
const PIN_WIDTH: f64 = 16.0;

/// How far the mouse must travel before a press becomes a drag rather
/// than a click. Below this a small hand tremor during a click would
/// start a reorder the user never asked for; much above it and a
/// deliberate short drag between two adjacent tabs stops registering.
/// AppKit has no published constant for this; 4 points is what the
/// system's own drag-detection uses in practice.
const DRAG_THRESHOLD: f64 = 4.0;

/// Tab icons, embedded rather than read from disk at runtime — the same
/// PNGs `ui_win32` blits and `ui_gtk` decodes, so all three backends
/// show the identical glyph for the identical state and the binary stays
/// self-contained per DESIGN.md §9.1.
const PNG_TAB_SAVE: &[u8] = include_bytes!("../../../assets/icons/tab-save.png");
const PNG_TAB_SAVE_2X: &[u8] = include_bytes!("../../../assets/icons/tab-save@2x.png");
const PNG_TAB_SAVE_DIRTY: &[u8] = include_bytes!("../../../assets/icons/tab-save-dirty.png");
const PNG_TAB_SAVE_DIRTY_2X: &[u8] = include_bytes!("../../../assets/icons/tab-save-dirty@2x.png");

/// Logical size the tab icon occupies, at either display scale.
const ICON_LOGICAL_PX: f64 = 16.0;

/// The pin glyph's design canvas, matching `ui_gtk` and `ui_win32`.
const PIN_CANVAS: f64 = 12.0;
/// Pinned-state thumbtack (upright) — verbatim from Win32's
/// `TAB_PIN_POLYGON_PINNED`, via `ui_gtk`'s copy, so the three backends
/// draw one shape.
const PIN_POLY_PINNED: &[(f64, f64)] = &[
    (3.0, 2.0),
    (9.0, 2.0),
    (9.0, 4.0),
    (7.0, 4.0),
    (7.0, 8.0),
    (6.0, 11.0),
    (5.0, 8.0),
    (5.0, 4.0),
    (3.0, 4.0),
];
/// Unpinned-state thumbtack (the same shape rotated 45° CCW) — Win32's
/// `TAB_PIN_POLYGON_UNPINNED`. The tilt distinguishes it from the
/// pinned tack.
const PIN_POLY_UNPINNED: &[(f64, f64)] = &[
    (1.0, 5.0),
    (5.0, 1.0),
    (7.0, 2.0),
    (5.0, 4.0),
    (8.0, 7.0),
    (10.0, 10.0),
    (7.0, 8.0),
    (4.0, 5.0),
    (2.0, 7.0),
];
/// Pinned fill — Material Blue 500 (#2196F3), Win32's
/// `TAB_PIN_FILL_PINNED`.
const PIN_RGB_PINNED: (f64, f64, f64) = (33.0 / 255.0, 150.0 / 255.0, 243.0 / 255.0);
/// Unpinned outline — grey (#808080), Win32's `TAB_PIN_OUTLINE_UNPINNED`.
const PIN_RGB_UNPINNED: (f64, f64, f64) = (0.5, 0.5, 0.5);
/// The unpinned glyph draws at this fraction of the pinned one's size so
/// "not pinned" reads as a quiet affordance rather than competing with
/// the filled tack. Same factor `ui_gtk` uses.
const PIN_UNPINNED_SHRINK: f64 = 0.7;

/// Per-instance state for [`TabButton`].
///
/// Only the buffer id, and it is duplicated in the control's `tag`
/// because AppKit itself reads the tag for `sender.tag()` dispatch. The
/// ivar exists so the tracking loop can read it without a message send
/// on every mouse-moved event.
pub struct TabButtonIvars {
    id: Cell<i32>,
}

define_class!(
    // SAFETY: an `NSButton` subclass that overrides only mouse-event
    // entry points, adding no state AppKit reads. Main-thread-only
    // because every override is an AppKit callback and the handlers
    // reach the main-thread `thread_local` window state.
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppTabButton"]
    #[ivars = TabButtonIvars]
    pub struct TabButton;

    unsafe impl NSObjectProtocol for TabButton {}

    impl TabButton {
        /// Take the whole press-drag-release gesture over from `NSButton`.
        ///
        /// Deliberately does **not** chain to `super`: `NSButton`'s own
        /// implementation runs a tracking loop that does not return until
        /// mouse-up, so there is no point at which a drag could still be
        /// detected. See the module docs.
        ///
        /// The cost is that `NSButtonCell`'s press-highlight animation
        /// never runs, so a click gives no "pressed" flash before the
        /// strip resyncs into the new selection. Accepted: the resync is
        /// immediate and is itself the feedback, and recovering the
        /// highlight would mean driving the cell's tracking manually —
        /// which is the machinery being bypassed on purpose.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("TabButton::mouseDown:", (), || self.track(event));
        }

        /// Middle-click closes the tab, matching `ui_gtk` and Notepad++.
        ///
        /// `otherMouseDown:` covers every button past left and right, so
        /// the button number is checked rather than assumed — a mouse
        /// with forward/back buttons must not close tabs.
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("TabButton::otherMouseDown:", (), || {
                if event.buttonNumber() == MIDDLE_BUTTON {
                    crate::close_tab_by_id(self.ivars().id.get());
                }
            });
        }
    }
);

/// AppKit's button number for the middle button.
const MIDDLE_BUTTON: isize = 2;

impl TabButton {
    fn new(frame: NSRect, id: i32, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TabButtonIvars { id: Cell::new(id) });
        // SAFETY: `NSButton`'s designated initialiser, on a freshly
        // allocated instance whose ivars are already set.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Run the press-drag-release loop and commit whichever gesture it
    /// turned out to be.
    ///
    /// Pulls events straight off the queue with
    /// `nextEventMatchingMask:` rather than relying on further AppKit
    /// callbacks. That is the standard Cocoa idiom for a self-contained
    /// drag, and it is what makes the click/drag decision possible at
    /// all: the choice is made *after* seeing whether the mouse moved,
    /// not committed at press time.
    ///
    /// The button's frame follows the mouse during a drag so the gesture
    /// has feedback — and its pin and close travel with it, because they
    /// are its subviews rather than its siblings. The strip is rebuilt
    /// wholesale afterwards either way, so a rejected reorder snaps
    /// straight back to model order with no extra bookkeeping — the same
    /// repair-by-resync `ui_gtk` relies on.
    ///
    /// A nested run loop implies a [`crate::DrainFreeze`]; see the guard
    /// inside for why this one needs it as much as a modal does.
    fn track(&self, event: &NSEvent) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let id = self.ivars().id.get();
        let mut outcome: Option<Gesture> = None;
        // SAFETY: reading the superview of a live view on the main
        // thread. The strip's container is what added this button, and it
        // outlives it — `sync` removes buttons from the container, never
        // the other way round.
        let Some(container) = (unsafe { self.superview() }) else {
            return;
        };
        // A pinned tab is fixed in place. The loop below still runs —
        // it has to, so the press can still resolve to a *selection* —
        // but this flag stops it ever promoting to a drag, so the tab
        // never moves and no `move_tab` is attempted. `Shell::move_tab`
        // would reject one anyway; this is what stops the user dragging
        // a tab that can never land, which is the effect `ui_gtk` gets
        // by clearing `set_tab_reorderable`.
        let pinned = crate::tab_is_pinned(id);

        // Held for the whole gesture. This loop pumps
        // `NSEventTrackingRunLoopMode` by hand, and GCD's main-queue
        // source is serviced there — so the §5.4 worker wake can fire
        // mid-drag and reach `refresh_tab_chrome` → `TabStrip::sync`,
        // which removes every control in the strip *including this
        // button*, whose `mouseDown:` is still on the stack below us. It
        // could also pop a modal alert while the mouse is still held.
        // Nothing is lost: the freeze defers, and `reorder_tab_by_id`'s
        // `refresh_tab_chrome` runs after it is released.
        // Named without an underscore: it is dropped explicitly before
        // the model mutation at the end of this function.
        let freeze = crate::DrainFreeze::new();

        let app = NSApplication::sharedApplication(mtm);
        let start = container.convertPoint_fromView(event.locationInWindow(), None);
        let origin = self.frame().origin;
        let mut dragging = false;

        loop {
            let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp;
            // SAFETY: the standard Cocoa modal-tracking call. Running the
            // queue in `NSEventTrackingRunLoopMode` is what the mode
            // exists for; `dequeue: true` consumes the event we handle.
            let next = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    mask,
                    Some(&NSDate::distantFuture()),
                    NSEventTrackingRunLoopMode,
                    true,
                )
            };
            let Some(next) = next else {
                break;
            };
            let kind = next.r#type();
            if kind == NSEventType::LeftMouseUp {
                // Decide here, act after the loop. **`self` must not be
                // touched again once the model is mutated** — see below.
                outcome = if dragging {
                    Some(Gesture::Reorder(drop_target_index(self.frame().origin.x)))
                } else {
                    Some(Gesture::Select)
                };
                break;
            }
            if kind != NSEventType::LeftMouseDragged || pinned {
                continue;
            }
            let here = container.convertPoint_fromView(next.locationInWindow(), None);
            let dx = here.x - start.x;
            if !dragging && dx.abs() >= DRAG_THRESHOLD {
                dragging = true;
            }
            if dragging {
                self.setFrameOrigin(NSPoint::new(origin.x + dx, origin.y));
            }
        }

        // ---- Past this point `self` is not touched again, deliberately.
        //
        // Both arms below end in `refresh_tab_chrome`, which rebuilds the
        // strip and sends `removeFromSuperview` to every control in it —
        // including this button, whose `mouseDown:` is still on the stack
        // underneath us. The superview holds the only strong reference,
        // so that release can deallocate the receiver while this frame is
        // live. Objective-C message dispatch does not retain the
        // receiver, so a `self.` after the mutation is a use-after-free,
        // not merely a stale read. Deciding inside the loop and acting
        // outside it is what keeps the last use of `self` strictly before
        // the mutation.
        //
        // The freeze goes first so the resync it guards is not itself
        // deferred, and the flush after replaces the wake that the freeze
        // swallowed: a worker that finished mid-drag dispatched into this
        // run loop, found the freeze up, and returned without queuing a
        // retry — the same unconditional flush `action_close_tab` does on
        // release, for the same reason.
        drop(freeze);
        match outcome {
            Some(Gesture::Select) => crate::select_tab_by_id(id),
            Some(Gesture::Reorder(target)) => crate::reorder_tab_by_id(id, target),
            // The queue closed under us without a mouse-up. Nothing to
            // commit; the strip is already in model order.
            None => return,
        }
        crate::drain_shell();
    }
}

/// What a finished press turned out to be.
///
/// Exists so [`TabButton::track`] can decide inside its event loop and
/// act outside it — see the comment at the end of that loop for why the
/// separation is a safety requirement rather than a tidiness one.
enum Gesture {
    Select,
    Reorder(usize),
}

/// Which slot a tab dropped at `origin_x` lands in.
///
/// Split out from [`commit_reorder`] so the arithmetic is testable
/// without a window server, an `NSButton` or a live `Shell` — the same
/// pure-helper discipline `ui_win32`'s `resolve_tab_arm_commit` follows,
/// and for the same reason: the interesting failures here are
/// off-by-ones at the edges, which are exactly what a hands-on demo is
/// worst at catching.
///
/// The upper end is deliberately *not* clamped here: this function has
/// no idea how many tabs exist. `crate::reorder_tab_by_id` clamps
/// against the live tab count, which is the only place that knows it.
fn drop_target_index(origin_x: f64) -> usize {
    let slot = ((origin_x + TAB_WIDTH / 2.0) / TAB_WIDTH).floor();
    if slot <= 0.0 {
        // A drag past the left edge yields a negative slot.
        return 0;
    }
    // `slot` is positive and finite here, and the strip holds far fewer
    // tabs than `usize::MAX`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        slot as usize
    }
}

/// Per-instance state for [`PinView`].
pub struct PinViewIvars {
    id: Cell<i32>,
    pinned: Cell<bool>,
}

define_class!(
    // SAFETY: an `NSView` subclass overriding only `drawRect:`,
    // `isFlipped` and `mouseDown:`. Main-thread-only for the same reason
    // as every other view here.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppPinView"]
    #[ivars = PinViewIvars]
    pub struct PinView;

    unsafe impl NSObjectProtocol for PinView {}

    impl PinView {
        /// Top-left origin, so the polygon tables above — which are
        /// authored against Win32's top-left design canvas — can be used
        /// verbatim rather than y-flipped at every vertex.
        ///
        /// The only AppKit entry point on this backend deliberately left
        /// without a [`crate::at_callback_boundary`] guard: it returns a
        /// literal and has no reachable panic, and AppKit calls it on
        /// every layout pass, so a guard here would be pure cost.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("PinView::drawRect:", (), || {
                let bounds = self.bounds();
                draw_pin_glyph(
                    self.ivars().pinned.get(),
                    bounds.size.width,
                    bounds.size.height,
                );
            });
        }

        /// Consumes its own click, so pinning a background tab does not
        /// also switch to it — same as the close button, and same as
        /// `ui_gtk`, where the pin lives in a button that swallows the
        /// press before the notebook sees it.
        ///
        /// `toggle_pin_by_id` resyncs the strip, which deallocates this
        /// view mid-call for the reason spelled out at the end of
        /// [`TabButton::track`]. Sound only because it is the last
        /// statement: nothing here touches `self` afterwards. Keep it
        /// that way.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            crate::at_callback_boundary("PinView::mouseDown:", (), || {
                crate::toggle_pin_by_id(self.ivars().id.get());
            });
        }
    }
);

impl PinView {
    fn new(frame: NSRect, id: i32, pinned: bool, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PinViewIvars {
            id: Cell::new(id),
            pinned: Cell::new(pinned),
        });
        // SAFETY: `NSView`'s designated initialiser on a freshly
        // allocated instance whose ivars are already set.
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        view.setToolTip(Some(&NSString::from_str(if pinned {
            "Unpin"
        } else {
            "Pin"
        })));
        view
    }
}

/// Stroke or fill the thumbtack centred in a `width`×`height` box.
///
/// Runs inside `drawRect:`, so the current graphics context is already
/// the view's — which is why nothing here takes a context argument, the
/// AppKit equivalent of cairo's `cr` being handed to `ui_gtk`'s
/// `draw_pin_glyph`.
fn draw_pin_glyph(pinned: bool, width: f64, height: f64) {
    // Shrink the unpinned outline, then recompute the centring offsets
    // from the smaller canvas so it stays centred.
    let mut scale = width.min(height) / PIN_CANVAS;
    if !pinned {
        scale *= PIN_UNPINNED_SHRINK;
    }
    if scale <= 0.0 {
        return;
    }
    let off_x = (width - PIN_CANVAS * scale) / 2.0;
    let off_y = (height - PIN_CANVAS * scale) / 2.0;
    let verts = if pinned {
        PIN_POLY_PINNED
    } else {
        PIN_POLY_UNPINNED
    };

    let path = NSBezierPath::bezierPath();
    for (i, &(x, y)) in verts.iter().enumerate() {
        let point = NSPoint::new(off_x + x * scale, off_y + y * scale);
        if i == 0 {
            path.moveToPoint(point);
        } else {
            path.lineToPoint(point);
        }
    }
    path.closePath();

    let (r, g, b) = if pinned {
        PIN_RGB_PINNED
    } else {
        PIN_RGB_UNPINNED
    };
    // These make a colour current in the live `drawRect:` graphics
    // context, which is the only place they are meaningful.
    let colour = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
    if pinned {
        colour.setFill();
        path.fill();
    } else {
        colour.setStroke();
        path.setLineWidth(scale.max(1.0));
        path.stroke();
    }
}

/// Handle to the strip.
///
/// `Clone` is a refcount bump on the container — this is stored on the
/// window state and read on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct TabStrip {
    pub container: Retained<NSView>,
}

impl TabStrip {
    pub fn new(width: f64, mtm: MainThreadMarker) -> Self {
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, TAB_STRIP_HEIGHT)),
        );
        container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        Self { container }
    }

    /// Rebuild the strip from the shell's tab list.
    ///
    /// Rebuilding wholesale rather than diffing is deliberate: the strip
    /// is at most a few dozen small controls, it is refreshed only on
    /// real model changes (open, close, save, reorder, tab switch) and
    /// never per keystroke, and a diffing implementation would need its
    /// own identity bookkeeping — which is the exact thing that has gone
    /// wrong on this project before (DESIGN.md §7.4's tab arm/commit
    /// race). Cheap and obviously-correct beats clever here.
    ///
    /// It is also what repairs a rejected drag: the reorder handler does
    /// not move anything itself, it asks the shell and then re-syncs, so
    /// a `move_tab` that declined leaves the strip back in model order
    /// with no special case.
    pub fn sync(
        &self,
        tabs: &[Tab],
        active: Option<usize>,
        actions: &Actions,
        mtm: MainThreadMarker,
    ) {
        // Drop the previous generation of controls.
        for view in self.container.subviews() {
            view.removeFromSuperview();
        }

        let active_id = active.and_then(|i| tabs.get(i)).map(|t| t.id);
        let mut x = 0.0;
        for tab in tabs {
            let selected = Some(tab.id) == active_id;
            let button = make_tab_button(tab, selected, x, mtm);

            // **Children of the body, not siblings of it.** They have to
            // travel with the tab during a drag, and parenting them is
            // what makes that automatic — the alternative is repositioning
            // three frames per mouse-moved event and getting it wrong the
            // first time the layout changes. Their frames are therefore in
            // the body's coordinate space, which is why the leading `x`
            // that positions the body does not appear below.
            //
            // Being subviews also keeps the click behaviour the sibling
            // layout had: AppKit hit-tests subviews before their parent,
            // so pin and close still consume their own presses and a click
            // on either does not reach the body's `mouseDown:`.
            let pin = PinView::new(
                NSRect::new(
                    NSPoint::new(
                        TAB_WIDTH - CLOSE_WIDTH - PIN_WIDTH,
                        (TAB_STRIP_HEIGHT - PIN_WIDTH) / 2.0,
                    ),
                    NSSize::new(PIN_WIDTH, PIN_WIDTH),
                ),
                tab.id,
                tab.pinned,
                mtm,
            );
            button.addSubview(&pin);

            let close = make_close_button(tab.id, TAB_WIDTH - CLOSE_WIDTH, actions, mtm);
            button.addSubview(&close);

            self.container.addSubview(&button);
            x += TAB_WIDTH;
        }
    }

    /// Whole-strip visibility, for `NPPM_HIDETABBAR` and the View menu.
    pub fn set_hidden(&self, hidden: bool) {
        self.container.setHidden(hidden);
    }

    pub fn is_hidden(&self) -> bool {
        self.container.isHidden()
    }
}

/// Decode one of the embedded PNGs into an `NSImage` carrying **both**
/// scale representations.
///
/// Adding the 1× and 2× bitmaps to one image and then pinning the
/// image's logical size to 16×16 lets AppKit pick the right bitmap per
/// display, which is the Cocoa equivalent of `ui_gtk` selecting an asset
/// from `scale_factor()` — except it also keeps working when the window
/// is dragged between a Retina and a non-Retina display mid-session,
/// which the GTK path re-evaluates only on the next sync.
///
/// Returns `None` rather than panicking if decoding fails: a tab without
/// its icon is a cosmetic loss, and taking the process down because a
/// bundled asset would not parse is not proportionate. Both other
/// backends degrade the same way.
fn tab_icon(dirty: bool) -> Option<Retained<NSImage>> {
    let sources: [&[u8]; 2] = if dirty {
        [PNG_TAB_SAVE_DIRTY, PNG_TAB_SAVE_DIRTY_2X]
    } else {
        [PNG_TAB_SAVE, PNG_TAB_SAVE_2X]
    };
    let image = NSImage::initWithSize(
        NSImage::alloc(),
        NSSize::new(ICON_LOGICAL_PX, ICON_LOGICAL_PX),
    );
    let mut added = 0usize;
    for bytes in sources {
        let data = NSData::with_bytes(bytes);
        // Returns nil rather than raising when the bytes are not a
        // decodable image, which is why this can be a plain `else`.
        let Some(rep) = NSBitmapImageRep::imageRepWithData(&data) else {
            continue;
        };
        let rep: Retained<NSImageRep> = Retained::into_super(rep);
        image.addRepresentation(&rep);
        added += 1;
    }
    if added == 0 {
        tracing::warn!(dirty, "tab icon decode failed; tab renders without it");
        return None;
    }
    Some(image)
}

/// One tab body button.
fn make_tab_button(
    tab: &Tab,
    selected: bool,
    x: f64,
    mtm: MainThreadMarker,
) -> Retained<TabButton> {
    // `tab_display_name` is the shared, already-sanitized display name —
    // the same helper the window title uses, so a filename carrying
    // bidi-control or other hostile characters cannot reach the strip
    // raw. DESIGN.md §7.4 records two prior incidents of exactly that
    // class (the Win32 workspace tree, `NPPM_SETSTATUSBAR`).
    let name = codepp_shell::tab_display_name(tab);

    let frame = NSRect::new(
        NSPoint::new(x, 0.0),
        NSSize::new(TAB_WIDTH, TAB_STRIP_HEIGHT),
    );
    let button = TabButton::new(frame, tab.id, mtm);
    button.setTitle(&NSString::from_str(&name));
    // The full name stays reachable even when the title is truncated,
    // matching the tooltip `ui_gtk` puts on every label.
    button.setToolTip(Some(&NSString::from_str(&name)));
    // `AccessoryBar` is the non-deprecated spelling of what used to
    // be `Recessed`: a flat, tinted-when-on bezel, which is the right
    // look for a tab.
    button.setBezelStyle(NSBezelStyle::AccessoryBar);
    button.setButtonType(NSButtonType::PushOnPushOff);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    button.setTag(tab.id as isize);
    if let Some(icon) = tab_icon(tab.dirty) {
        button.setImage(Some(&icon));
        button.setImagePosition(NSCellImagePosition::ImageLeft);
    }
    if let Some(cell) = button.cell() {
        cell.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    }
    // No target/action: [`TabButton`] handles the whole gesture itself so
    // it can tell a click from a drag. See the module docs.
    //
    // `NSControlStateValueOn` is 1, `Off` is 0.
    button.setState(isize::from(selected));
    button
}

/// The close button for one tab.
fn make_close_button(
    id: i32,
    x: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let frame = NSRect::new(
        NSPoint::new(x, 4.0),
        NSSize::new(CLOSE_WIDTH, TAB_STRIP_HEIGHT - 8.0),
    );
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str("×"));
    button.setToolTip(Some(&NSString::from_str("Close")));
    // Ordinary AppKit setters. The selector is a compile-time `sel!`
    // literal that `Actions` implements, and the target is a weak
    // reference AppKit zeroes if it ever outlives the receiver — which it
    // cannot here, since the window state owns `Actions` for the process
    // lifetime.
    button.setBordered(false);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    button.setTag(id as isize);
    // SAFETY: `setTarget:`/`setAction:` are unsafe because AppKit will
    // later send `action` to `target` unchecked. Both are sound here:
    // `Actions` implements `codeppCloseTab:` (a compile-time `sel!`
    // literal declared in its `define_class!` block, so it cannot go
    // stale), and the target is a weak reference to an object the window
    // state owns for the process lifetime.
    unsafe {
        let target: &AnyObject = actions;
        button.setTarget(Some(target));
        button.setAction(Some(sel!(codeppCloseTab:)));
    }
    button
}

#[cfg(test)]
mod tests {
    use super::{drop_target_index, TAB_WIDTH};

    #[test]
    fn a_tab_left_where_it_started_maps_to_its_own_slot() {
        assert_eq!(drop_target_index(0.0), 0);
        assert_eq!(drop_target_index(TAB_WIDTH), 1);
        assert_eq!(drop_target_index(2.0 * TAB_WIDTH), 2);
    }

    #[test]
    fn a_half_tab_of_travel_is_what_flips_the_slot() {
        // Just short of half a tab: still slot 0. Exactly half: slot 1.
        // This boundary is the whole behaviour of a drag-reorder — a
        // tab swaps with its neighbour once it covers more of the
        // neighbour's cell than its own.
        assert_eq!(drop_target_index(TAB_WIDTH / 2.0 - 1.0), 0);
        assert_eq!(drop_target_index(TAB_WIDTH / 2.0), 1);
    }

    #[test]
    fn dragging_off_the_left_edge_clamps_to_the_first_slot() {
        assert_eq!(drop_target_index(-1.0), 0);
        assert_eq!(drop_target_index(-10.0 * TAB_WIDTH), 0);
    }

    #[test]
    fn dragging_past_the_last_tab_is_left_for_the_caller_to_clamp() {
        // Deliberately unclamped: this helper does not know the tab
        // count. `reorder_tab_by_id` clamps against the live list, and a
        // test that asserted a clamp here would be pinning the wrong
        // contract.
        assert_eq!(drop_target_index(99.0 * TAB_WIDTH), 99);
    }
}
