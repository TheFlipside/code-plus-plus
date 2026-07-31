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
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBezelStyle, NSBezierPath, NSBitmapImageRep,
    NSButton, NSButtonType, NSCellImagePosition, NSColor, NSEvent, NSEventMask,
    NSEventTrackingRunLoopMode, NSEventType, NSFont, NSImage, NSImageRep, NSLineBreakMode, NSView,
    NSWindowOrderingMode,
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
/// Width of one overflow arrow, and so of the pair's cell each.
const ARROW_WIDTH: f64 = 22.0;
/// Total width the arrow pair reserves at the strip's trailing edge
/// while the tabs overflow.
const ARROWS_WIDTH: f64 = 2.0 * ARROW_WIDTH;

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

thread_local! {
    /// How far the strip is scrolled, in points, when the tabs overflow
    /// its width.
    ///
    /// A `thread_local` rather than a field on [`TabStrip`] because the
    /// strip is `Clone` — it is handed out by `CocoaUiState::split` on
    /// every drain — so a plain field would give each copy its own
    /// offset. There is exactly one strip per process and every toucher
    /// is on the main thread, which is the same reasoning [`DRAGGING`]
    /// below is written against.
    static SCROLL_OFFSET: Cell<f64> = const { Cell::new(0.0) };
    /// True while a tab-strip drag is in flight. See [`DragGuard`].
    ///
    /// A flag rather than the depth counter [`crate::DrainFreeze`] uses,
    /// because AppKit's tracking loop is synchronous and owns the mouse
    /// for its duration: a second drag cannot begin while one is
    /// running, so there is nothing to nest. If that ever stops being
    /// true — a gesture recogniser, a second pointing device — this must
    /// become a counter, since an inner guard's `Drop` would otherwise
    /// clear the outer one's flag early and unfreeze the strip mid-drag.
    static DRAGGING: Cell<bool> = const { Cell::new(false) };
}

/// Suppresses [`TabStrip::sync`] for the span of a drag.
///
/// **Why the strip must not be rebuilt mid-drag.** `TabButton::track`
/// snapshots its neighbours once when the gesture starts and then moves
/// them by frame; a `sync` in between detaches every one of those views
/// and builds fresh ones, so the drag would go on shuffling orphans
/// while the visible strip sat frozen in model order — and the drop
/// index would be read from a button no longer in the strip.
///
/// A [`crate::DrainFreeze`] does not cover this. It stops the *worker
/// wake* path, which is where the danger was thought to be, but
/// `on_sci_notify` calls `refresh_tab_chrome` directly and is not
/// gated by it. No current notification can fire mid-drag — the arms
/// that rebuild the strip all require an edit or a save, and neither can
/// happen while a mouse button is held on a tab — so this is closing a
/// latent path rather than a live bug. It is worth closing anyway: the
/// reasoning that makes it unreachable is about what a *user* can do,
/// which is exactly the kind of premise that stops holding when a plugin
/// or a timer gains the ability to touch a buffer.
///
/// Nothing is lost by suppressing: `track` ends by calling
/// `select_tab_by_id` or `reorder_tab_by_id`, both of which resync after
/// the guard is dropped.
struct DragGuard;

impl DragGuard {
    fn new() -> Self {
        DRAGGING.with(|d| d.set(true));
        Self
    }
}

impl Drop for DragGuard {
    /// On drop, not on the normal exit path only: an early return or a
    /// panic must not leave the strip permanently unable to redraw,
    /// which would look like every tab operation silently failing.
    fn drop(&mut self) {
        DRAGGING.with(|d| d.set(false));
    }
}

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
        // thread. It is the strip's **lane** — the clipped child view the
        // tabs live in, not `TabStrip::container`, which additionally
        // holds the overflow arrows — and it outlives this button, since
        // `sync` removes buttons from the lane and never the reverse.
        let Some(lane) = (unsafe { self.superview() }) else {
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

        // An owned reference for the whole gesture. Two jobs: it is what
        // `addSubview:positioned:relativeTo:` needs to re-order this
        // button safely (that call removes and re-inserts the subview,
        // which without a reference of our own would drop the container's
        // only one), and it makes the receiver outlive the resync at the
        // end of this function outright. See the note there — the
        // decide-inside/act-outside ordering is kept as well, but this is
        // now the primary guarantee rather than the only one.
        let me = self.retain();

        let app = NSApplication::sharedApplication(mtm);
        let start = lane.convertPoint_fromView(event.locationInWindow(), None);
        let origin = self.frame().origin;
        let mut dragging = false;
        // The other tabs, and where each sat when the drag began.
        // Captured at the moment a press becomes a drag rather than at
        // the top, so an ordinary click walks no subview arrays.
        let mut peers: Vec<(Retained<NSView>, usize)> = Vec::new();
        let mut from_slot = 0usize;
        let mut drag_guard: Option<DragGuard> = None;
        let mut backdrop: Option<Retained<TabBackdrop>> = None;

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
                    Some(Gesture::Reorder(drop_target_index(
                        self.frame().origin.x + TabStrip::scroll_offset(),
                    )))
                } else {
                    Some(Gesture::Select)
                };
                break;
            }
            if kind != NSEventType::LeftMouseDragged || pinned {
                continue;
            }
            let here = lane.convertPoint_fromView(next.locationInWindow(), None);
            let dx = here.x - start.x;
            if !dragging && dx.abs() >= DRAG_THRESHOLD {
                dragging = true;
                // Freeze the strip's rebuild for the rest of the
                // gesture; see [`DragGuard`]. Taken here rather than at
                // the top so an ordinary click never suppresses anything.
                drag_guard = Some(DragGuard::new());
                from_slot = slot_of(origin.x + TabStrip::scroll_offset());
                peers = peer_tabs(&lane, id);
                // **Raise it above the others before it starts moving.**
                // Subview order is paint order, so a button left at its
                // original index in the array is painted *under* every
                // tab that comes after it — which reads as the dragged
                // tab merging into its neighbours rather than being
                // picked up. Re-adding an existing subview is AppKit's
                // documented way to restack it, and it is focus-neutral:
                // measured either side of a drag, the window's first
                // responder stays the Scintilla content view, so the
                // restack does not steal the caret.
                lane.addSubview_positioned_relativeTo(&me, NSWindowOrderingMode::Above, None);
                // Raising it is not enough on its own — see
                // [`TabBackdrop`] for why an unselected tab is otherwise
                // see-through for the whole gesture.
                //
                // Only when unselected. The selected tab already draws a
                // filled bezel, so it occludes on its own — which is why
                // the user who reported this saw the *active* tab drag
                // correctly and every other tab drag wrong. And since the
                // backdrop is a *subview*, it paints after the button's
                // own drawing, so installing it on a selected tab would
                // cover the very bezel that makes it look selected.
                // Gating on the state says the same thing either way:
                // "this tab has no background of its own".
                if me.state() == 0 {
                    backdrop = Some(TabBackdrop::install(&me, mtm));
                }
            }
            if dragging {
                self.setFrameOrigin(NSPoint::new(origin.x + dx, origin.y));
                // Open a gap under the dragged tab by shuffling the
                // others live, so nothing ever sits behind it. Raising
                // the z-order alone would leave the dragged tab visibly
                // covering its neighbours; together the two give the
                // "picked up, and the rest step aside" feel `GtkNotebook`
                // provides for free.
                let total = peers.len() + 1;
                let target = drop_target_index(origin.x + dx + TabStrip::scroll_offset())
                    .min(total.saturating_sub(1));
                for (peer, slot) in &peers {
                    // A strip holds tens of tabs, not 2^53 of them, so
                    // the widening is exact in every reachable case.
                    #[allow(clippy::cast_precision_loss)]
                    let want = slot_after_shift(*slot, from_slot, target) as f64 * TAB_WIDTH
                        - TabStrip::scroll_offset();
                    let at = peer.frame().origin;
                    // Only write when it actually moved: `setFrameOrigin`
                    // invalidates, and this runs per mouse-moved event.
                    if (at.x - want).abs() > 0.5 {
                        peer.setFrameOrigin(NSPoint::new(want, at.y));
                    }
                }
            }
        }

        // Take the drag's opacity backing away again. Done explicitly
        // rather than left to the resync so it is gone even on the paths
        // that do not resync, and done here — before the mutation —
        // because afterwards the button it belongs to may already have
        // been detached.
        if let Some(backdrop) = backdrop {
            backdrop.removeFromSuperview();
        }

        // ---- Past this point `self` is not touched again, deliberately.
        //
        // Both arms below end in `refresh_tab_chrome`, which rebuilds the
        // strip and sends `removeFromSuperview` to every control in it —
        // including this button, whose `mouseDown:` is still on the stack
        // underneath us. Objective-C message dispatch does not retain the
        // receiver, so with the container holding the only reference that
        // release would deallocate it while this frame is live, and a
        // `self.` after the mutation would be a use-after-free rather
        // than a stale read.
        //
        // `me` above now holds a reference of our own for exactly as long
        // as this function runs, so the object survives the resync
        // regardless. The ordering discipline is kept anyway, and pinned
        // by a source-scan test: it costs nothing, it does not depend on
        // a reader noticing that `me` is load-bearing rather than merely
        // convenient, and it keeps the *values* read after the mutation
        // honest — a frame read back from a button the resync has already
        // detached would be stale even where it is safe.
        //
        // The freeze goes first so the resync it guards is not itself
        // deferred, and the flush after replaces the wake that the freeze
        // swallowed: a worker that finished mid-drag dispatched into this
        // run loop, found the freeze up, and returned without queuing a
        // retry — the same unconditional flush `action_close_tab` does on
        // release, for the same reason.
        drop(freeze);
        // Released before the resync below, which is the one that has to
        // land: it is what puts the strip back in agreement with the
        // model after a drag.
        drop(drag_guard);
        match outcome {
            Some(Gesture::Select) => crate::select_tab_by_id(id),
            Some(Gesture::Reorder(target)) => crate::reorder_tab_by_id(id, target),
            // The queue closed under us without a mouse-up — the run
            // loop tearing down mid-gesture, i.e. the app quitting.
            // Nothing to commit, but the strip is *not* necessarily in
            // model order: if the drag had progressed far enough to
            // shuffle the peers, they are sitting in uncommitted
            // positions. Resync rather than returning, so an abnormal end
            // leaves the same state a normal one does. (An earlier
            // version returned here on the reasoning that there was
            // nothing to commit, which is only true when the press never
            // became a drag.)
            None => {
                if dragging {
                    crate::refresh_tab_chrome();
                }
                return;
            }
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

/// Total width `count` tabs occupy.
fn tab_span(count: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        count as f64 * TAB_WIDTH
    }
}

/// How much width the tabs get: the whole strip, or the whole strip
/// less the arrows when they are needed.
///
/// One function rather than the same conditional at three call sites,
/// because `sync`, `scroll_into_view` and `scroll_by_one` must agree —
/// a lane that differed between them would clamp the offset to one
/// bound and scroll against another.
/// Floors at zero here rather than at each call site, so every caller
/// gets the same guarantee: a window narrower than the arrows themselves
/// would otherwise hand a negative width to the offset arithmetic.
fn lane_width_for(count: usize, full_width: f64) -> f64 {
    if tab_span(count) > full_width {
        (full_width - ARROWS_WIDTH).max(0.0)
    } else {
        full_width.max(0.0)
    }
}

/// Hold `offset` inside the range the strip can actually scroll.
///
/// Pure, and tested: the upper bound is the one that goes wrong, because
/// it has to account for the arrows' lane and must floor at zero when
/// everything fits — otherwise a strip with two tabs in a wide window
/// could be scrolled into empty space.
fn clamp_offset(offset: f64, count: usize, lane: f64) -> f64 {
    let max = (tab_span(count) - lane).max(0.0);
    offset.clamp(0.0, max)
}

/// The smallest change to `offset` that brings tab `index` fully into a
/// `lane`-wide view.
///
/// Returns `offset` unchanged when the tab is already visible, so a
/// caller can compare and skip. Brings the near edge into view rather
/// than centring: centring moves the strip on every tab switch, which
/// makes the other tabs feel like they slide around underneath.
fn offset_showing(index: usize, offset: f64, lane: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let left = index as f64 * TAB_WIDTH;
    let right = left + TAB_WIDTH;
    if left < offset {
        left
    } else if right > offset + lane {
        right - lane
    } else {
        offset
    }
}

/// One overflow arrow.
///
/// SF Symbols rather than a drawn glyph: they are the system's own
/// chevrons, so they match every other overflow control on the platform
/// and follow the appearance into dark mode. A missing symbol degrades
/// to a text chevron rather than an empty button.
fn make_arrow(
    forward: bool,
    x: f64,
    actions: &Actions,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    let frame = NSRect::new(
        NSPoint::new(x, 0.0),
        NSSize::new(ARROW_WIDTH, TAB_STRIP_HEIGHT),
    );
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    let symbol = if forward {
        "chevron.right"
    } else {
        "chevron.left"
    };
    match NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        None,
    ) {
        Some(image) => {
            button.setTitle(&NSString::from_str(""));
            button.setImage(Some(&image));
        }
        None => {
            button.setTitle(&NSString::from_str(if forward {
                "\u{203a}"
            } else {
                "\u{2039}"
            }));
        }
    }
    button.setBordered(false);
    button.setToolTip(Some(&NSString::from_str(if forward {
        "Scroll tabs right"
    } else {
        "Scroll tabs left"
    })));
    // SAFETY: a compile-time `sel!` literal `Actions` implements, and a
    // weak target the window state owns for the process lifetime.
    unsafe {
        button.setTarget(Some(actions));
        button.setAction(Some(if forward {
            sel!(codeppScrollTabsRight:)
        } else {
            sel!(codeppScrollTabsLeft:)
        }));
    }
    button
}

/// The slot a tab currently at `origin_x` occupies.
///
/// Rounds rather than truncates: at rest every button sits exactly at
/// `slot * TAB_WIDTH`, but the value comes back through Core Graphics
/// floats, and truncating would turn a 149.9999 into slot 0.
fn slot_of(origin_x: f64) -> usize {
    let slot = (origin_x / TAB_WIDTH).round();
    if slot <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        slot as usize
    }
}

/// Where a *non-dragged* tab sits while a drag from `from` hovers over
/// `target`.
///
/// The dragged tab is conceptually lifted out of the row, so everything
/// between its origin and its current target closes up behind it — which
/// is what leaves an empty slot underneath it. Dragging right shifts the
/// tabs it has passed one slot left; dragging left shifts them one
/// right; everything outside that span stays put.
fn slot_after_shift(slot: usize, from: usize, target: usize) -> usize {
    if target > from && slot > from && slot <= target {
        slot - 1
    } else if target < from && slot >= target && slot < from {
        slot + 1
    } else {
        slot
    }
}

/// Every tab body in the strip's `lane` except the one with `id`,
/// paired with the slot it currently occupies.
///
/// Reads the slot back out of each button's frame rather than from the
/// model: the two agree (the strip is laid out from the model), and
/// reading the view keeps this a pure view-layer operation needing no
/// borrow of `Shell` — which matters because it runs inside a
/// `DrainFreeze`d tracking loop.
fn peer_tabs(lane: &NSView, id: i32) -> Vec<(Retained<NSView>, usize)> {
    lane.subviews()
        .iter()
        .filter(|view| view.downcast_ref::<TabButton>().is_some() && view.tag() != id as isize)
        .map(|view| {
            (
                view.clone(),
                slot_of(view.frame().origin.x + TabStrip::scroll_offset()),
            )
        })
        .collect()
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

define_class!(
    // SAFETY: an `NSView` subclass that overrides only `drawRect:` and
    // adds no state AppKit reads. Main-thread-only like every other view
    // here.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppTabBackdrop"]
    pub struct TabBackdrop;

    unsafe impl NSObjectProtocol for TabBackdrop {}

    impl TabBackdrop {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("TabBackdrop::drawRect:", (), || {
                // The window background, so a dragged tab reads as a
                // solid card cut from the chrome rather than as a new
                // colour. Resolves per appearance, so dark mode is free.
                NSColor::windowBackgroundColor().setFill();
                NSBezierPath::fillRect(self.bounds());
            });
        }
    }
);

impl TabBackdrop {
    /// Make a tab opaque for the duration of a drag.
    ///
    /// **Why a dragged tab needs this at all.** Raising the z-order
    /// stopped the dragged tab being painted *under* its neighbours, but
    /// an unselected `AccessoryBar` button draws no background — so the
    /// tab on top was see-through, and the tabs beneath showed through
    /// it wherever it straddled two slots, which it does throughout the
    /// gesture. The symptom was that dragging the *active* tab looked
    /// right (its selected bezel is filled, so it occludes) while
    /// dragging any inactive one still looked merged. Same defect the
    /// z-order fix addressed, one layer down.
    ///
    /// Inserted **below** the button's other subviews — including the
    /// `_NSCoreHostingView` AppKit uses to render the title and image —
    /// so it backs the content rather than covering it.
    ///
    /// **That last point rests on an undocumented implementation
    /// detail, and is recorded here rather than assumed.** An
    /// `AccessoryBar` button renders its title and image through a
    /// lazily-created private subview, so a backdrop placed at index 0
    /// paints behind them. Verified on the current OS by dumping a
    /// configured button's subview tree; subview paint order itself is
    /// documented, but the fact that the label lives in a subview at all
    /// is not. If a future macOS moves that drawing back into the
    /// button's own `drawRect:`, this backdrop would paint *over* the
    /// dragged tab's label — visible immediately to anyone dragging a
    /// tab, but nothing here would fail. The migration then is to drop
    /// the subview and override `TabButton::drawRect:` to fill the
    /// background before chaining to `super`. Same discipline as the
    /// note on the deprecated `registerNotifyCallback:` in
    /// `ScintillaCocoaShim.mm`: name the private thing being relied on,
    /// and say what to do when it goes.
    ///
    /// Not given a `hitTest:` override to make it click-through, because
    /// it does not need one: it exists only between the start of a drag
    /// and [`TabButton::track`] removing it, and for that whole span the
    /// tracking loop is consuming the mouse itself.
    fn install(on: &NSButton, mtm: MainThreadMarker) -> Retained<Self> {
        let bounds = on.bounds();
        let this = Self::alloc(mtm);
        // SAFETY: `NSView`'s designated initialiser on a fresh instance
        // of a subclass that adds no ivars.
        //
        // Sent to `this` rather than `super(this)`, unlike [`PinView`]
        // and [`TabButton`] just above. Not a style slip: the `super(..)`
        // form is only available once `set_ivars` has produced a
        // `PartialInit`, and this class declares no ivars, so `alloc`
        // yields a plain `Allocated<Self>` and `super(..)` does not
        // typecheck. `dropview::ContentView`, the crate's other
        // ivar-less subclass, is written the same way.
        let view: Retained<Self> = unsafe { msg_send![this, initWithFrame: bounds] };
        on.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, None);
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
    /// The whole strip: the tab lane plus, while overflowing, the
    /// arrows.
    pub container: Retained<NSView>,
    /// The tabs' own view, sized to the space left over after the arrows
    /// and **clipped to it**.
    ///
    /// A child view rather than laying the tabs out directly in
    /// `container`, because reserving width for the arrows is not the
    /// same as stopping the tabs being *drawn* there. The first version
    /// reserved a lane — which bounded how far the strip could scroll —
    /// but AppKit does not clip subviews, so the tabs still painted
    /// under the borderless arrows and showed through them. Clipping is
    /// what makes the arrows sit *beside* the tabs, which is where the
    /// other two backends put them.
    lane: Retained<NSView>,
}

impl TabStrip {
    pub fn new(width: f64, mtm: MainThreadMarker) -> Self {
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, TAB_STRIP_HEIGHT)),
        );
        container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let lane = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, TAB_STRIP_HEIGHT)),
        );
        lane.setClipsToBounds(true);
        container.addSubview(&lane);
        Self { container, lane }
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
        if DRAGGING.with(Cell::get) {
            // A drag owns the strip's layout right now. Rebuilding would
            // detach the views it is moving; the drag resyncs when it
            // ends. See [`DragGuard`].
            return;
        }
        // Drop the previous generation of controls — the tabs from the
        // lane, and any arrows from the strip. The lane itself stays.
        for view in self.lane.subviews() {
            view.removeFromSuperview();
        }
        for view in self.container.subviews() {
            // Pointer identity is the right test and is sound: an
            // Objective-C object does not relocate, and `subviews()`
            // hands back references to the live views rather than copies,
            // so two independently-obtained `Retained` handles to the
            // lane compare equal by address.
            if !std::ptr::eq(std::ptr::from_ref(&*view), std::ptr::from_ref(&*self.lane)) {
                view.removeFromSuperview();
            }
        }

        let active_id = active.and_then(|i| tabs.get(i)).map(|t| t.id);
        // Reserve the trailing arrows' space *before* deciding how far the
        // strip can scroll, so the last tab is not left permanently under
        // them.
        let full_width = self.container.frame().size.width;
        let overflowing = tab_span(tabs.len()) > full_width;
        let lane_width = lane_width_for(tabs.len(), full_width);
        let offset = clamp_offset(SCROLL_OFFSET.with(Cell::get), tabs.len(), lane_width);
        SCROLL_OFFSET.with(|c| c.set(offset));
        // Resize the lane before filling it: it is what clips the tabs,
        // so it has to be the right width *first*.
        self.lane.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(lane_width.max(0.0), TAB_STRIP_HEIGHT),
        ));

        let mut x = -offset;
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

            self.lane.addSubview(&button);
            x += TAB_WIDTH;
        }

        if overflowing {
            // Siblings of the lane, not of the tabs — so they sit
            // *beside* the tabs rather than over them, and the lane's
            // clipping means nothing can reach under them, a dragged tab
            // included.
            //
            // Disabled at the extremes, so an arrow that cannot move the
            // strip says so rather than silently doing nothing.
            let left = make_arrow(false, full_width - ARROWS_WIDTH, actions, mtm);
            left.setEnabled(offset > 0.5);
            self.container.addSubview(&left);
            let right = make_arrow(true, full_width - ARROW_WIDTH, actions, mtm);
            right.setEnabled(offset < clamp_offset(f64::MAX, tabs.len(), lane_width) - 0.5);
            self.container.addSubview(&right);
        }
    }

    /// Scroll `index` fully into view, if it is not already.
    ///
    /// Without this the arrows solve only half the problem: switching
    /// tabs with the Window menu, `⌘W`, or a plugin can select something
    /// that is scrolled off, and the strip would go on showing a
    /// selection the user cannot see. Called on every chrome refresh, so
    /// it also covers a tab being closed under the scrolled region.
    pub fn scroll_into_view(&self, index: usize, count: usize) {
        let full_width = self.container.frame().size.width;
        let lane = lane_width_for(count, full_width);
        let offset = SCROLL_OFFSET.with(Cell::get);
        let wanted = offset_showing(index, offset, lane);
        if (wanted - offset).abs() > 0.5 {
            SCROLL_OFFSET.with(|c| c.set(clamp_offset(wanted, count, lane)));
        }
    }

    /// Step the strip one tab left or right — the arrow buttons.
    pub fn scroll_by_one(&self, forward: bool, count: usize) {
        let full_width = self.container.frame().size.width;
        let lane = lane_width_for(count, full_width);
        let delta = if forward { TAB_WIDTH } else { -TAB_WIDTH };
        let next = clamp_offset(SCROLL_OFFSET.with(Cell::get) + delta, count, lane);
        SCROLL_OFFSET.with(|c| c.set(next));
    }

    /// The strip's scroll offset, for translating a button's frame into
    /// the slot space the drag arithmetic works in.
    pub fn scroll_offset() -> f64 {
        SCROLL_OFFSET.with(Cell::get)
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
    use super::{drop_target_index, slot_after_shift, slot_of, TAB_WIDTH};

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

    #[test]
    fn a_tab_at_rest_reports_its_own_slot() {
        assert_eq!(slot_of(0.0), 0);
        assert_eq!(slot_of(TAB_WIDTH), 1);
        // Core Graphics hands frames back as floats, so the rounding is
        // load-bearing: truncating would call this slot 0.
        assert_eq!(slot_of(TAB_WIDTH - 0.0001), 1);
        assert_eq!(slot_of(-5.0), 0);
    }

    #[test]
    fn dragging_right_closes_the_gap_behind_the_dragged_tab() {
        // Tab 0 dragged onto slot 2: tabs 1 and 2 shift left, 3 stays.
        assert_eq!(slot_after_shift(1, 0, 2), 0);
        assert_eq!(slot_after_shift(2, 0, 2), 1);
        assert_eq!(slot_after_shift(3, 0, 2), 3);
    }

    #[test]
    fn dragging_left_closes_the_gap_the_other_way() {
        // Tab 3 dragged onto slot 1: tabs 1 and 2 shift right, 0 stays.
        assert_eq!(slot_after_shift(0, 3, 1), 0);
        assert_eq!(slot_after_shift(1, 3, 1), 2);
        assert_eq!(slot_after_shift(2, 3, 1), 3);
    }

    #[test]
    fn a_drag_that_has_not_left_its_own_slot_moves_nothing() {
        for slot in 0..4 {
            assert_eq!(slot_after_shift(slot, 2, 2), slot);
        }
    }

    #[test]
    fn the_shift_is_a_permutation_so_no_two_tabs_share_a_slot() {
        // The property that actually matters visually: whatever the
        // drag, the tabs that stay behind must occupy distinct slots,
        // and must leave exactly one free for the dragged tab.
        const N: usize = 5;
        for from in 0..N {
            for target in 0..N {
                let mut seen: Vec<usize> = (0..N)
                    .filter(|s| *s != from)
                    .map(|s| slot_after_shift(s, from, target))
                    .collect();
                seen.sort_unstable();
                let before = seen.len();
                seen.dedup();
                assert_eq!(
                    seen.len(),
                    before,
                    "from={from} target={target}: two tabs collided"
                );
                assert!(
                    !seen.contains(&target),
                    "from={from} target={target}: nothing left a gap for the dragged tab"
                );
            }
        }
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::{clamp_offset, lane_width_for, offset_showing, tab_span, ARROWS_WIDTH, TAB_WIDTH};

    /// Points are compared with a tolerance rather than exactly: these
    /// are layout coordinates, and an exact float comparison would be
    /// pinning the arithmetic's rounding rather than its behaviour.
    #[track_caller]
    fn assert_points(have: f64, want: f64) {
        assert!(
            (have - want).abs() < 0.5,
            "expected {want} points, got {have}"
        );
    }

    #[test]
    fn the_arrows_take_width_only_when_the_tabs_overflow() {
        // Three tabs in a 1000pt strip: everything fits, so no arrows and
        // the tabs get the lot.
        assert_points(lane_width_for(3, 1000.0), 1000.0);
        // Ten tabs in the same strip overflow, so the arrows take theirs.
        assert_points(lane_width_for(10, 1000.0), 1000.0 - ARROWS_WIDTH);
    }

    #[test]
    fn the_boundary_is_exactly_fits_versus_overflows() {
        // Exactly filling the strip is *not* overflow — reserving arrow
        // space there would push the last tab out and create the overflow
        // it was reserving for.
        let exact = tab_span(4);
        assert_points(lane_width_for(4, exact), exact);
        assert_points(lane_width_for(4, exact - 1.0), exact - 1.0 - ARROWS_WIDTH);
    }

    #[test]
    fn a_window_narrower_than_the_arrows_floors_at_zero() {
        assert_points(lane_width_for(10, 20.0), 0.0);
        assert_points(lane_width_for(0, -5.0), 0.0);
    }

    #[test]
    fn nothing_scrolls_while_everything_fits() {
        // Three tabs in a lane wide enough for four: the strip must stay
        // pinned at zero however hard a caller pushes it, or it could be
        // scrolled into empty space.
        let lane = 4.0 * TAB_WIDTH;
        assert_points(clamp_offset(0.0, 3, lane), 0.0);
        assert_points(clamp_offset(500.0, 3, lane), 0.0);
        assert_points(clamp_offset(-500.0, 3, lane), 0.0);
    }

    #[test]
    fn the_last_tab_can_reach_the_trailing_edge_but_no_further() {
        // Ten tabs in a lane three wide: the furthest useful offset puts
        // the tenth tab's right edge on the lane's right edge.
        let lane = 3.0 * TAB_WIDTH;
        let max = tab_span(10) - lane;
        assert_points(clamp_offset(max, 10, lane), max);
        assert_points(clamp_offset(max + 1000.0, 10, lane), max);
        assert_points(clamp_offset(-1.0, 10, lane), 0.0);
    }

    #[test]
    fn an_already_visible_tab_does_not_move_the_strip() {
        // Tabs 2, 3 and 4 are on screen at this offset; asking for any of
        // them must be a no-op, or every tab switch would jitter the
        // strip.
        let lane = 3.0 * TAB_WIDTH;
        let offset = 2.0 * TAB_WIDTH;
        for index in 2..=4 {
            assert_points(offset_showing(index, offset, lane), offset);
        }
    }

    #[test]
    fn a_tab_off_the_left_scrolls_to_its_own_left_edge() {
        let lane = 3.0 * TAB_WIDTH;
        assert_points(offset_showing(1, 5.0 * TAB_WIDTH, lane), TAB_WIDTH);
    }

    #[test]
    fn a_tab_off_the_right_scrolls_only_far_enough_to_show_it() {
        // Tab 7 with a three-wide lane: its right edge is at 8 tabs, so
        // the offset is 8 - 3 = 5. Scrolling further would push earlier
        // tabs off for no reason.
        let lane = 3.0 * TAB_WIDTH;
        assert_points(offset_showing(7, 0.0, lane), 5.0 * TAB_WIDTH);
    }

    #[test]
    fn the_first_and_last_tabs_are_both_reachable() {
        let lane = 3.0 * TAB_WIDTH;
        assert_points(offset_showing(0, 4.0 * TAB_WIDTH, lane), 0.0);
        let last = offset_showing(9, 0.0, lane);
        assert_points(clamp_offset(last, 10, lane), tab_span(10) - lane);
    }
}
