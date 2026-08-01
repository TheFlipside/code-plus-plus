//! The Document Map — a right-side miniature of the active buffer with a
//! translucent orange box marking the visible viewport. Port of
//! `ui_gtk::docmap`, which is itself a port of the Win32 `docmap_*`
//! functions, so all three backends draw the same thing the same colour.
//!
//! # Shape
//!
//! A **second Scintilla view** is the miniature. It shares the active
//! tab's document through `SCI_SETDOCPOINTER` (rebound on tab switch by
//! [`sync_to_active_tab`]) exactly as the main view does, so no buffer
//! text is duplicated. Like the main view it is created once at startup
//! and never destroyed, removed or reassigned — which is what keeps the
//! `Copy`, lifetime-free [`EditorHandle`] it hands out sound. DESIGN.md
//! §7.4 makes that discipline binding for this backend; the source scan
//! in `lib.rs` pins the count at two.
//!
//! # The orange box
//!
//! Win32 paints it in a buffered `WM_PAINT` overlay and GTK stacks a
//! transparent `DrawingArea` inside a `GtkOverlay`. Cocoa's analogue is
//! simply a **sibling view added after the miniature**: AppKit paints
//! subviews in array order and hit-tests them in reverse, so a later
//! sibling is both on top and first to the mouse, with whatever it does
//! not paint left transparent. No container type is needed for it.
//!
//! The colour is the one the other two backends use — `#FFA500`
//! (Notepad++'s Document Map orange) at `60/255 ≈ 24 %` alpha.
//!
//! # Why the overlay owns the mouse
//!
//! The miniature is bound to the *same, editable* document as the main
//! view, so it must never receive input. `SCI_SETREADONLY` cannot express
//! that: it is a **document** property, so setting it would freeze the
//! main editor too, and setting it on the miniature's throwaway initial
//! document is superseded by the first `SCI_SETDOCPOINTER`. The guarantee
//! is instead that no input reaches the miniature at all — the overlay
//! covers it completely and consumes every mouse event, and a plain
//! `NSView` returns `NO` from `acceptsFirstResponder`, so clicking the
//! overlay does not even move focus off the main editor. Win32 does the
//! same thing (no `WS_TABSTOP`, plus a subclass proc that eats the
//! mouse).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSButton, NSColor, NSCursor, NSEvent, NSFont,
    NSLineBreakMode, NSRectFill, NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    CARETSTYLE_INVISIBLE, SCI_DOCLINEFROMVISIBLE, SCI_GETDOCPOINTER, SCI_GETFIRSTVISIBLELINE,
    SCI_GETLINECOUNT, SCI_LINESCROLL, SCI_LINESONSCREEN, SCI_POINTYFROMPOSITION,
    SCI_POSITIONFROMLINE, SCI_SETCARETSTYLE, SCI_SETDOCPOINTER, SCI_SETHSCROLLBAR,
    SCI_SETMARGINWIDTHN, SCI_SETVSCROLLBAR, SCI_SETZOOM, SCI_TEXTHEIGHT, SCI_VISIBLEFROMDOCLINE,
};

use crate::menu::Actions;
use crate::state::with_state;

/// Panel title, matching the other two backends' header label.
const PANEL_TITLE: &str = "Document Map";
/// Width the first time the map is opened, in points. Mirrors Win32's
/// `DEFAULT_DOCMAP_WIDTH_PX` and GTK's `DEFAULT_WIDTH_PX`.
const DEFAULT_WIDTH: f64 = 160.0;
/// Width floor — below this the miniature is unreadable blocks.
const MIN_WIDTH: f64 = 80.0;
/// The editor never shrinks below this, however far the divider is
/// dragged. Same role as the FIF dock's `EDITOR_MIN_HEIGHT`.
const EDITOR_MIN_WIDTH: f64 = 200.0;
/// Ceiling on a width restored from `session.xml` — comfortably past the
/// widest display this could run on, so it rejects only corruption and
/// never a width a user actually dragged. See [`apply_saved`].
const MAX_RESTORED_WIDTH: f64 = 10_000.0;
/// The panel's own header row (title + close button).
const HEADER_HEIGHT: f64 = 24.0;
/// The drag handle down the panel's left edge.
const DIVIDER_WIDTH: f64 = 5.0;
/// Miniature zoom: `-10` shrinks the font to the smallest block shape
/// that still hints at text density. Same value as the other backends.
const MINIATURE_ZOOM: isize = -10;

/// The `#FFA500` viewport orange as `NSColor` components.
const VIEWPORT_RGB: (f64, f64, f64) = (1.0, 165.0 / 255.0, 0.0);
/// Fill alpha of the viewport wash, `60/255 ≈ 24 %` — the same value as
/// Win32's `DOCMAP_VIEWPORT_FILL_ALPHA`. A soft tint that leaves the
/// miniature text underneath legible.
const VIEWPORT_ALPHA: f64 = 60.0 / 255.0;

/// Lines to scroll the MAIN editor per wheel notch over the map.
const WHEEL_LINES: f64 = 3.0;
/// Smallest divider movement worth acting on, in points. See
/// [`drag_divider_to`].
const DRAG_EPSILON: f64 = 0.5;

thread_local! {
    /// Whether the panel is open, and how wide it is.
    ///
    /// `thread_local`s rather than fields on [`DocMapPanel`] for the same
    /// reason `FifDock`'s height is one: the panel is `Clone` and handed
    /// out by `CocoaUiState::split` on every drain, so a plain field
    /// would give each copy its own value and a divider drag would be
    /// forgotten on the next wake.
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static WIDTH: Cell<f64> = const { Cell::new(DEFAULT_WIDTH) };

    /// The panel's root view, so [`owns_view`] can answer without a
    /// `with_state` borrow. See it for why that matters — the borrow
    /// version failed open on exactly the path the guard protects.
    static PANEL_VIEW: RefCell<Option<Retained<NSView>>> = const { RefCell::new(None) };
}

// --- The overlay ------------------------------------------------------

define_class!(
    // SAFETY: an `NSView` subclass overriding only drawing, mouse
    // handling and the flip flag. Main-thread-only because every one of
    // those callbacks arrives there.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDocMapOverlay"]
    pub struct Overlay;

    unsafe impl NSObjectProtocol for Overlay {}

    impl Overlay {
        /// Top-down coordinates, so the y values Scintilla reports for
        /// the miniature can be used directly instead of being mirrored
        /// against a height that changes with every window resize.
        ///
        /// The one deliberate exception to the `at_callback_boundary`
        /// rule, on the same reasoning as `PinView::isFlipped`: it
        /// returns a literal, cannot panic, and AppKit calls it on every
        /// layout pass.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }

        /// Paint the translucent viewport box. Everything not painted
        /// stays transparent, so the miniature shows through.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("docmap:drawRect", (), || {
                self.paint_viewport_box();
            });
        }

        /// Consume the click and scroll the main editor to the line under
        /// it. Consuming is the point: the miniature underneath must
        /// never start a selection drag in the shared document.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("docmap:mouseDown", (), || {
                self.scroll_main_to_event(event);
            });
        }

        /// AppKit routes drags to whichever view took the `mouseDown:`,
        /// so no tracking loop is needed here — unlike the tab strip,
        /// where `NSButton`'s own `mouseDown:` swallows the gesture.
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            crate::at_callback_boundary("docmap:mouseDragged", (), || {
                self.scroll_main_to_event(event);
            });
        }

        /// Forward the wheel to the MAIN editor rather than letting the
        /// miniature scroll its own independent viewport, which would
        /// desync the orange box. Port of Win32's `WM_MOUSEWHEEL` arm.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            crate::at_callback_boundary("docmap:scrollWheel", (), || {
                scroll_main_by_wheel(event);
            });
        }
    }
);

impl Overlay {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser
        // and this subclass adds no ivars needing other initialisation.
        unsafe { msg_send![this, initWithFrame: frame] }
    }

    /// Map an event to a point in this view and scroll the main editor.
    fn scroll_main_to_event(&self, event: &NSEvent) {
        let local = self.convertPoint_fromView(event.locationInWindow(), None);
        scroll_main_to_y(local.y);
    }

    /// Fill the viewport band. Split out of `drawRect:` so the callback
    /// boundary stays one line and this stays ordinary Rust.
    fn paint_viewport_box(&self) {
        let Some(Some(rect)) = with_state(|st| viewport_rect(st, self.bounds().size.width)) else {
            return;
        };
        // Both of these require a focused graphics context, which is why
        // this is only ever reached from `drawRect:`.
        NSColor::colorWithSRGBRed_green_blue_alpha(
            VIEWPORT_RGB.0,
            VIEWPORT_RGB.1,
            VIEWPORT_RGB.2,
            VIEWPORT_ALPHA,
        )
        .setFill();
        NSRectFill(rect);
    }
}

// --- The panel --------------------------------------------------------

/// Everything the Document Map owns for the window's lifetime.
///
/// `Clone` is a refcount bump per field — it is handed out by
/// `CocoaUiState::split` on every drain, so it must stay cheap.
#[derive(Clone)]
pub struct DocMapPanel {
    /// The panel's root. The caller positions it; see
    /// `CocoaUi::relayout_chrome`.
    pub container: Retained<NSView>,
    /// The orange-box view, held so a refresh can invalidate it.
    overlay: Retained<Overlay>,
    /// Direct-call handle for the miniature.
    pub editor: EditorHandle,
    /// The miniature Scintilla view.
    ///
    /// **`dead_code` is allowed deliberately: not being read is the
    /// point.** The field's whole job is to hold the second permanent
    /// view alive, exactly as `CocoaUiState::sci_view` does for the
    /// first. A future reader who removes the "unused" field would drop
    /// the last strong reference and leave [`Self::editor`]'s raw
    /// pointers dangling.
    #[allow(dead_code)]
    miniature: Retained<NSView>,
}

impl DocMapPanel {
    /// Build the panel around an already-created miniature view. Starts
    /// hidden, so a session that never opens the map pays only this.
    pub fn new(
        miniature: Retained<NSView>,
        editor: EditorHandle,
        height: f64,
        actions: &Actions,
        mtm: MainThreadMarker,
    ) -> Self {
        let width = DEFAULT_WIDTH;
        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
        );
        // Keeps its width and its distance from the right edge as the
        // window grows; the editor beside it absorbs the slack.
        container.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable
                | NSAutoresizingMaskOptions::ViewMinXMargin,
        );
        container.setHidden(true);

        // --- the drag handle, down the left edge --------------------
        let divider = Divider::new(
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DIVIDER_WIDTH, height)),
            mtm,
        );
        divider.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable
                | NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        container.addSubview(&divider);

        // --- header: title + close ----------------------------------
        //
        // `ViewMinYMargin` on both is load-bearing rather than
        // decoration: the header has to stay pinned to the panel's top
        // edge as the window makes the panel taller. The FIF dock's own
        // header shipped without it and buried itself under the results
        // table the first time anyone dragged it.
        let header_y = height - HEADER_HEIGHT;
        let close = NSButton::initWithFrame(
            NSButton::alloc(mtm),
            NSRect::new(
                NSPoint::new(width - 24.0, header_y),
                NSSize::new(22.0, HEADER_HEIGHT),
            ),
        );
        close.setTitle(&NSString::from_str("✕"));
        close.setBezelStyle(NSBezelStyle::AccessoryBarAction);
        close.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        close.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        // SAFETY: the target is a weak reference to an object the window
        // state owns for the process lifetime, and the selector is a
        // compile-time `sel!` literal `Actions` implements.
        unsafe {
            close.setTarget(Some(&**actions));
            close.setAction(Some(sel!(codeppDocMapClose:)));
        }

        let title = NSTextField::labelWithString(&NSString::from_str(PANEL_TITLE), mtm);
        title.setFrame(NSRect::new(
            NSPoint::new(DIVIDER_WIDTH + 4.0, header_y),
            NSSize::new((width - DIVIDER_WIDTH - 32.0).max(0.0), HEADER_HEIGHT),
        ));
        title.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        title.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        container.addSubview(&title);
        container.addSubview(&close);

        // --- the miniature, filling what is left, and its overlay ---
        let body = NSRect::new(
            NSPoint::new(DIVIDER_WIDTH, 0.0),
            NSSize::new(
                (width - DIVIDER_WIDTH).max(0.0),
                (height - HEADER_HEIGHT).max(0.0),
            ),
        );
        let flexible = NSAutoresizingMaskOptions::ViewWidthSizable
            | NSAutoresizingMaskOptions::ViewHeightSizable;
        miniature.setFrame(body);
        miniature.setAutoresizingMask(flexible);
        let overlay = Overlay::new(body, mtm);
        overlay.setAutoresizingMask(flexible);
        // Order matters twice over: AppKit paints subviews in array order
        // and hit-tests them in reverse, so adding the overlay *after*
        // the miniature is what puts the orange box on top and what puts
        // the overlay first in line for the mouse.
        container.addSubview(&miniature);
        container.addSubview(&overlay);

        configure_miniature(editor);
        // Publish the panel root for `owns_view`, the keyboard half of
        // the no-input guarantee. Set here rather than derived from the
        // window state on demand — see `owns_view`.
        PANEL_VIEW.with(|p| *p.borrow_mut() = Some(container.clone()));

        Self {
            container,
            overlay,
            editor,
            miniature,
        }
    }

    /// Invalidate the overlay so the box repaints on the next frame.
    fn redraw(&self) {
        self.overlay.setNeedsDisplay(true);
    }
}

/// The width the layout should give the panel: zero while closed,
/// otherwise clamped to what this window width can spare.
///
/// **The clamp belongs here, not only in the divider drag**, for the same
/// reason the FIF dock's does: the persisted width outlives the window
/// size that justified it, and a plain window resize is handled entirely
/// by autoresizing masks — where the editor is the only flexible view and
/// so absorbs the whole delta. Without a clamp on every layout,
/// narrowing the window under an open map starves the editor to nothing
/// while the map keeps its width.
///
/// A free function rather than a method on [`DocMapPanel`] because both
/// values it reads live in this module's thread-locals; see them for why
/// they are not fields.
pub(crate) fn width_for_layout(content_width: f64) -> f64 {
    if !VISIBLE.with(Cell::get) {
        return 0.0;
    }
    clamp_map_width(WIDTH.with(Cell::get), content_width)
}

/// Clamp a wanted map width against what the window can spare.
///
/// Pure so the boundaries — which is what a divider drag gets wrong —
/// are testable without a window server.
fn clamp_map_width(wanted: f64, content_width: f64) -> f64 {
    // A window too narrow to satisfy both floors: the editor wins, and
    // the map takes whatever is left rather than pushing the editor
    // off-screen. Never negative.
    let ceiling = (content_width - EDITOR_MIN_WIDTH).max(0.0);
    if ceiling <= MIN_WIDTH {
        return ceiling;
    }
    wanted.clamp(MIN_WIDTH, ceiling)
}

// --- The divider ------------------------------------------------------

define_class!(
    // SAFETY: an `NSView` subclass overriding only drawing, cursor rects
    // and mouse tracking. Main-thread-only for the usual reason.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDocMapDivider"]
    pub struct Divider;

    unsafe impl NSObjectProtocol for Divider {}

    impl Divider {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("docmap:dividerDrawRect", (), || {
                NSColor::separatorColor().setFill();
                NSRectFill(self.bounds());
            });
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            crate::at_callback_boundary("docmap:resetCursorRects", (), || {
                // `resizeLeftRightCursor` is deprecated in favour of
                // `columnResizeCursorInDirections:`, which is macOS 15+.
                // Code++ has no such deployment floor. Same call the FIF
                // divider makes for its own axis.
                #[allow(deprecated)]
                let cursor = NSCursor::resizeLeftRightCursor();
                self.addCursorRect_cursor(self.bounds(), &cursor);
            });
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            crate::at_callback_boundary("docmap:dividerMouseDown", (), || {});
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            crate::at_callback_boundary("docmap:dividerMouseDragged", (), || {
                drag_divider_to(event.locationInWindow().x);
            });
        }
    }
);

impl Divider {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser
        // and this subclass adds no ivars needing other initialisation.
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

/// Move the divider so the map's left edge sits at `window_x`.
fn drag_divider_to(window_x: f64) {
    let Some((wanted, content_w)) = with_state(|st| {
        let (_, ui) = st.split();
        let content_w = ui
            .window
            .contentView()
            .map_or(0.0, |c| c.bounds().size.width);
        (content_w - window_x, content_w)
    }) else {
        return;
    };
    let width = clamp_map_width(wanted, content_w);
    // Half a point, matching the FIF dock's divider rather than a
    // bit-exact compare: a drag delivers a `mouseDragged:` per pointer
    // move, and sub-point deltas are invisible but would still relayout
    // the whole chrome, repaint the map and touch the session on every
    // one of them.
    if (WIDTH.with(Cell::get) - width).abs() < DRAG_EPSILON {
        return;
    }
    WIDTH.with(|w| w.set(width));
    crate::relayout_chrome_bands();
    refresh();
    sync_to_shell();
}

// --- Public entry points ----------------------------------------------

/// Whether the panel is open. Read by the View menu's check mark and the
/// toolbar toggle's repaint, so both are painted from one truth.
pub(crate) fn is_visible() -> bool {
    VISIBLE.with(Cell::get)
}

/// Is `view` the miniature, or inside the panel that holds it?
///
/// The predicate behind [`crate::window::MainWindow`]'s focus refusal —
/// the keyboard half of the "no input reaches the miniature" guarantee
/// this module's docs state.
///
/// **It reads a `thread_local`, deliberately, and not `with_state`.**
/// The first version went through the window state and was silently
/// useless: `makeFirstResponder:` is reachable from inside an outer
/// borrow — any `UiPlatform` method that shows or focuses something —
/// and a declined borrow reads as "not the map", which *allows* the
/// focus the guard exists to refuse. That is a security guard whose
/// failure mode is to fail open, on a path this backend already knows
/// re-enters. Caught by driving the app: with the state-reading version
/// installed, Tab still reached the miniature and typing still moved
/// the document length. Holding the panel view directly removes the
/// borrow from the question entirely.
pub(crate) fn owns_view(view: &NSView) -> bool {
    PANEL_VIEW.with(|p| {
        p.borrow()
            .as_ref()
            .is_some_and(|panel| crate::window::is_descendant_of(view, panel))
    })
}

/// Show or hide the panel. The single funnel behind the View toggle, the
/// toolbar button and the header close button, so all three agree.
pub(crate) fn set_visible(visible: bool) {
    VISIBLE.with(|v| v.set(visible));
    with_state(|st| {
        st.docmap.container.setHidden(!visible);
        let (_, ui) = st.split();
        ui.relayout_chrome();
    });
    if visible {
        // The map may have been closed across several scrolls and tab
        // switches, so rebind and repaint rather than waiting for the
        // next notification.
        sync_to_active_tab();
    }
    // The toolbar's toggle is a `PushOnPushOff` button whose state
    // AppKit has already flipped when *it* is the source, but not when
    // the menu or the close button is — so it is repainted from the
    // model here, the same rule the three View toggles follow. The menu
    // resolves its own mark in `validateMenuItem:` on open.
    with_state(|st| st.toolbar.refresh_docmap_toggle());
    sync_to_shell();
}

/// Rebind the miniature to the active tab's document, then repaint. A
/// no-op when the map is hidden or the tab has no document yet.
pub(crate) fn sync_to_active_tab() {
    if !VISIBLE.with(Cell::get) {
        return;
    }
    with_state(|st| {
        let doc = st.shell.active().map_or(0, |t| t.scintilla_doc);
        if doc == 0 {
            return;
        }
        // Skip a redundant re-point: `SCI_SETDOCPOINTER` is not free, and
        // rebinding resets the miniature's scroll, which the indicator
        // update below would then have to correct.
        if doc != st.docmap.editor.send(SCI_GETDOCPOINTER, 0, 0) {
            st.docmap.editor.send(SCI_SETDOCPOINTER, 0, doc);
        }
        update_indicator(st);
    });
}

/// Recompute the viewport range and repaint the box. A no-op when the map
/// is hidden. Driven from the main editor's notification handler, so it
/// is on the keystroke path — a handful of direct-calls plus one deferred
/// redraw, well inside DESIGN.md §8's budget.
pub(crate) fn refresh() {
    if !VISIBLE.with(Cell::get) {
        return;
    }
    with_state(update_indicator);
}

/// Apply the saved session: width, and open the panel if it was open.
/// Runs at cold start, after the session is in the shell.
///
/// **The restored width is bounded here, which is the one place it is
/// not otherwise.** `session.xml` is a file on disk that a user can hand
/// edit and that a crash can truncate, so this is the only path that
/// writes [`WIDTH`] from a value the process did not compute. Nothing
/// downstream is at risk — [`width_for_layout`] clamps against the live
/// window before any rectangle is built, and `f64 as i32` saturates
/// rather than wrapping — so this is hygiene, not a defence: without it
/// a nonsense width sits in the thread-local and gets written straight
/// back out on the next save, surviving every restart until someone
/// drags the divider.
///
/// The bound is deliberately absolute rather than window-relative. The
/// layout clamp is where the window has a say, and it is *not* written
/// back on purpose: a map temporarily squeezed by a narrow window must
/// come back to the width the user chose when the window grows again —
/// the same call `ui_win32` makes for the results dock's height.
pub(crate) fn apply_saved() {
    let Some(Some(saved)) = with_state(|st| st.shell.saved_docmap_session()) else {
        return;
    };
    if let Some(width) = saved.width.and_then(restorable_width) {
        WIDTH.with(|w| w.set(width));
    }
    if saved.visible {
        set_visible(true);
    }
}

/// A persisted width, if it is one this process could have written.
/// Pure so the boundaries are testable without a session file.
fn restorable_width(saved: i32) -> Option<f64> {
    let width = f64::from(saved);
    (MIN_WIDTH..=MAX_RESTORED_WIDTH)
        .contains(&width)
        .then_some(width)
}

/// Snapshot the live panel state into the shell so the next
/// `save_session` persists it. Called from the autosave / shutdown path.
pub(crate) fn sync_to_shell() {
    let width = WIDTH.with(Cell::get);
    with_state(|st| {
        st.shell
            .set_docmap_session(Some(codepp_core::session::DocMapSession {
                visible: VISIBLE.with(Cell::get),
                // Points, not pixels — the same unit every other geometry
                // this backend persists uses, so a Retina display does not
                // write a session a non-Retina one reads as twice as wide.
                width: Some(width.round() as i32),
            }));
    });
}

// --- Miniature configuration ------------------------------------------

/// Apply the once-only settings that make the second view a miniature.
///
/// Every setting here is **view-level** (per-`ViewStyle`) and so survives
/// the `SCI_SETDOCPOINTER` swaps that rebind the miniature onto each
/// tab's document — which is why it runs once, at creation.
///
/// Note what is *not* here: `SCI_SETREADONLY` is deliberately omitted.
/// See the module docs for why it cannot express what it looks like it
/// expresses, and what guarantees no edit instead.
fn configure_miniature(handle: EditorHandle) {
    // Invisible caret hides the blink; both scrollbars off (a fixed
    // overview, not something to scroll); every margin collapses so the
    // miniature uses its full width; zoom shrinks the font to a
    // density-hinting block shape.
    handle.send(SCI_SETCARETSTYLE, CARETSTYLE_INVISIBLE, 0);
    handle.send(SCI_SETHSCROLLBAR, 0, 0);
    handle.send(SCI_SETVSCROLLBAR, 0, 0);
    handle.send(SCI_SETMARGINWIDTHN, 0, 0);
    handle.send(SCI_SETMARGINWIDTHN, 1, 0);
    handle.send(SCI_SETMARGINWIDTHN, 2, 0);
    handle.send(SCI_SETZOOM, MINIATURE_ZOOM as usize, 0);
}

// --- Geometry ---------------------------------------------------------

/// The visible document-line range of the main editor, as a half-open
/// `[first, last)` in document-line space (folding-aware). `None` for an
/// empty buffer or a pre-first-layout editor. Shared by the painter and
/// the centring step so the two can never disagree.
fn visible_doc_range(main: EditorHandle) -> Option<(isize, isize)> {
    let first_visible = main.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
    let lines_on_screen = main.send(SCI_LINESONSCREEN, 0, 0);
    let line_count = main.send(SCI_GETLINECOUNT, 0, 0).max(0);
    if first_visible < 0 || lines_on_screen <= 0 || line_count == 0 {
        return None;
    }
    let last_visible_exclusive = first_visible + lines_on_screen;
    let first_doc = main.send(SCI_DOCLINEFROMVISIBLE, first_visible as usize, 0);
    let last_doc = main
        .send(SCI_DOCLINEFROMVISIBLE, last_visible_exclusive as usize, 0)
        .min(line_count);
    if first_doc < 0 || last_doc <= first_doc {
        return None;
    }
    Some((first_doc, last_doc))
}

/// The orange box's rectangle in the overlay's own (flipped) coordinates,
/// or `None` when there is nothing to paint.
///
/// Vertical extent = top of the first visible doc line → bottom of the
/// last, measured with `SCI_POINTYFROMPOSITION` **on the miniature** —
/// the same geometry Win32's `paint_docmap_viewport_overlay` computes.
fn viewport_rect(st: &crate::state::CocoaUiState, width: f64) -> Option<NSRect> {
    let (main, dm) = (st.editor, st.docmap.editor);
    let (first_doc, last_doc) = visible_doc_range(main)?;
    let first_pos = dm.send(SCI_POSITIONFROMLINE, first_doc as usize, 0);
    let last_pos = dm.send(SCI_POSITIONFROMLINE, last_doc as usize, 0);
    if first_pos < 0 || last_pos < 0 {
        return None;
    }
    let top_y = dm.send(SCI_POINTYFROMPOSITION, 0, first_pos);
    let mut bottom_y = dm.send(SCI_POINTYFROMPOSITION, 0, last_pos);
    if bottom_y <= top_y {
        // A single visible line: give the box one line's height so it
        // never collapses to nothing.
        bottom_y = top_y + dm.send(SCI_TEXTHEIGHT, first_doc as usize, 0);
    }
    let height = (bottom_y - top_y).max(0);
    if height <= 0 || width <= 0.0 {
        return None;
    }
    // Coordinates are view-sized, so narrowing to i32 before the exact
    // `f64::from` is lossless here and sidesteps the isize→f64
    // precision-loss lint.
    Some(NSRect::new(
        NSPoint::new(0.0, f64::from(top_y as i32)),
        NSSize::new(width, f64::from(height as i32)),
    ))
}

/// Recompute the viewport range, scroll the miniature so the highlighted
/// band stays centred (a long file will not fit the panel at any zoom),
/// and invalidate the overlay so the box repaints against the fresh
/// scroll position.
fn update_indicator(st: &mut crate::state::CocoaUiState) {
    let (main, dm) = (st.editor, st.docmap.editor);
    let Some((first_doc, last_doc)) = visible_doc_range(main) else {
        // Empty buffer: clear any box left over from a previous tab.
        st.docmap.redraw();
        return;
    };
    let map_lines = dm.send(SCI_LINESONSCREEN, 0, 0);
    let span = last_doc - first_doc;
    let target_first = (first_doc + span / 2 - map_lines / 2).max(0);
    let delta = target_first - dm.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
    if delta != 0 {
        dm.send(SCI_LINESCROLL, 0, delta);
    }
    st.docmap.redraw();
}

/// Map a y coordinate inside the overlay to a document line and scroll
/// the MAIN editor so that line is centred.
///
/// Computed from `y / row_height` rather than from a hit-test message so
/// a click below the last line clamps naturally and needs no special
/// "outside any character" handling.
///
/// **On the `with_state` re-entrancy contract**, which this backend has
/// been caught by three times: scrolling the main editor from inside a
/// borrow can re-enter `on_sci_notify`, whose own `with_state` is then
/// declined. Nothing goes stale here, and that is a fact about *what a
/// scroll changes* rather than luck — the status bar shows caret,
/// selection and length, none of which a scroll moves, and the map's own
/// indicator is updated below rather than left to the declined
/// notification. A future edit that makes this path touch buffer text
/// would inherit the obligation to refresh the chrome after the borrow
/// returns.
fn scroll_main_to_y(y: f64) {
    with_state(|st| {
        let (main, dm) = (st.editor, st.docmap.editor);
        let row_h = dm.send(SCI_TEXTHEIGHT, 0, 0).max(1);
        let map_first = dm.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
        let visible_line = (map_first + (y.max(0.0) as isize) / row_h).max(0);
        let target_doc = dm.send(SCI_DOCLINEFROMVISIBLE, visible_line as usize, 0);
        let target_visible = main.send(SCI_VISIBLEFROMDOCLINE, target_doc as usize, 0);
        let main_lines = main.send(SCI_LINESONSCREEN, 0, 0);
        let desired_first = (target_visible - main_lines / 2).max(0);
        let delta = desired_first - main.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
        if delta != 0 {
            main.send(SCI_LINESCROLL, 0, delta);
        }
        // The main-editor scroll fires its own notification, but repaint
        // the box now so drag tracking feels immediate.
        update_indicator(st);
    });
}

/// `SCI_LINESCROLL` lines for a wheel/trackpad delta over the map.
///
/// **The sign is the whole content of this function**, so it is pure and
/// tested rather than asserted in a comment. AppKit reports
/// `scrollingDeltaY` **positive for an upward gesture**, and scrolling up
/// means moving *towards* line zero, which `SCI_LINESCROLL` expresses as
/// a negative delta — hence the negation.
///
/// That convention is taken from the vendored backend rather than from a
/// synthetic event: `ScintillaCocoa::MouseWheel`
/// (`cocoa/ScintillaCocoa.mm:2528-2537`) derives `dY` from `deltaY` with
/// the sign preserved and then treats `dY > 0.5` as `Message::ZoomIn`,
/// i.e. positive delta is the "up" gesture. Its *scrolling* branch is
/// empty — the Cocoa backend leaves that to `NSScrollView` — which is
/// exactly why the map has to do it by hand here.
///
/// A trackpad reports fine-grained deltas and a wheel reports notches;
/// scaling both by the same step keeps a wheel click at three lines and
/// leaves a trackpad proportional.
fn wheel_lines(delta_y: f64) -> isize {
    if !delta_y.is_finite() {
        return 0;
    }
    (-delta_y * WHEEL_LINES).round() as isize
}

/// Forward a wheel event over the map to the MAIN editor.
fn scroll_main_by_wheel(event: &NSEvent) {
    let lines = wheel_lines(event.scrollingDeltaY());
    if lines == 0 {
        return;
    }
    with_state(|st| {
        st.editor.send(SCI_LINESCROLL, 0, lines);
        update_indicator(st);
    });
}

/// Selectors this module owns, for `Actions` to forward.
///
/// Declared here rather than in `menu.rs` so the panel's three entry
/// points (menu item, toolbar button, header close) stay next to what
/// they do.
pub(crate) fn toggle_from_menu() {
    set_visible(!is_visible());
}

/// The toolbar's `PushOnPushOff` button has already flipped its own state
/// by the time its action fires, so the handler *applies* that state
/// rather than inverting the model — inverting would fight the button and
/// land on the wrong value every other click. Same rule the three View
/// toggles follow.
pub(crate) fn set_from_toolbar(on: bool) {
    set_visible(on);
}

/// The header's own close button, and `NPPM`-style programmatic closes.
pub(crate) fn close_from_header() {
    set_visible(false);
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_map_width, restorable_width, wheel_lines, EDITOR_MIN_WIDTH, MAX_RESTORED_WIDTH,
        MIN_WIDTH,
    };

    #[test]
    fn an_ordinary_width_passes_through() {
        assert!((clamp_map_width(160.0, 1024.0) - 160.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_too_narrow_map_is_lifted_to_the_floor() {
        assert!((clamp_map_width(10.0, 1024.0) - MIN_WIDTH).abs() < f64::EPSILON);
        assert!((clamp_map_width(-500.0, 1024.0) - MIN_WIDTH).abs() < f64::EPSILON);
    }

    #[test]
    fn the_editor_keeps_its_floor_however_far_the_divider_is_dragged() {
        let content = 900.0;
        let width = clamp_map_width(10_000.0, content);
        assert!(
            content - width >= EDITOR_MIN_WIDTH,
            "editor left with {} pt",
            content - width
        );
    }

    #[test]
    fn a_window_too_narrow_for_both_floors_gives_the_editor_priority() {
        // 250 pt window: the editor's 200 pt floor leaves 50, which is
        // under the map's own 80 pt floor. The map yields rather than
        // pushing the editor off-screen.
        let width = clamp_map_width(160.0, 250.0);
        assert!((width - 50.0).abs() < f64::EPSILON, "got {width}");
        // And a window narrower than the editor floor gives it nothing at
        // all rather than a negative width.
        assert!(clamp_map_width(160.0, 100.0) >= 0.0);
    }

    #[test]
    fn an_upward_gesture_scrolls_towards_line_zero() {
        // The sign, which is the only thing this function decides. A
        // wrong one scrolls the editor backwards from the map — see the
        // function for where the AppKit convention comes from.
        assert!(
            wheel_lines(1.0) < 0,
            "positive scrollingDeltaY is an upward gesture and must move towards line 0"
        );
        assert!(wheel_lines(-1.0) > 0);
        assert_eq!(wheel_lines(0.0), 0);
        // A wheel notch is one line-unit; three lines is the step every
        // backend uses.
        assert_eq!(wheel_lines(-1.0), 3);
        // Sub-notch trackpad deltas round rather than accumulating error,
        // and a delta too small to move a line is simply dropped.
        assert_eq!(wheel_lines(-0.1), 0);
        // Nothing degenerate reaches the `as isize` cast.
        assert_eq!(wheel_lines(f64::NAN), 0);
        assert_eq!(wheel_lines(f64::INFINITY), 0);
    }

    #[test]
    fn a_hand_edited_session_width_is_refused_rather_than_persisted_again() {
        // The one path that writes WIDTH from a value this process did
        // not compute. A refusal leaves the default in place; accepting
        // it would round-trip nonsense back into session.xml on the next
        // save and survive every restart.
        assert_eq!(restorable_width(160), Some(160.0));
        assert_eq!(restorable_width(MIN_WIDTH as i32), Some(MIN_WIDTH));
        assert_eq!(
            restorable_width(MAX_RESTORED_WIDTH as i32),
            Some(MAX_RESTORED_WIDTH)
        );
        for hostile in [0, -1, i32::MIN, i32::MAX, MIN_WIDTH as i32 - 1] {
            assert_eq!(restorable_width(hostile), None, "accepted {hostile}");
        }
    }

    #[test]
    fn a_restored_width_survives_a_window_wide_enough_for_it() {
        // The persisted width is only ever reduced by the clamp, never
        // silently grown — a user who dragged the map narrow keeps it.
        assert!((clamp_map_width(90.0, 1600.0) - 90.0).abs() < f64::EPSILON);
    }
}
