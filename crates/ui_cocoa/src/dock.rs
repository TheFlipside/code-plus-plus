//! Cocoa mechanism for the plugin-panel docking subsystem.
//!
//! All policy lives in `codepp_core::dock` (`DockLayout`,
//! `compute_frame`, `resolve_drop`) — this module only supplies what
//! AppKit must: the group containers (caption bar + panel content +
//! bottom tab bar), the four side splitters, the translucent drop-hint
//! panel, the caption/tab drag gestures, and the reconciler that makes
//! the view tree match the model after every mutation. It is the Cocoa
//! counterpart of `ui_win32::dock_panels` and `ui_gtk::dock`, and the
//! three are kept the same shape on purpose so the one runner each has
//! can stand in for the others when reading.
//!
//! # Layout: a flipped dock area, laid out by hand
//!
//! The dock area — everything between the toolbar and the status bar —
//! is one plain `NSView` ([`DockArea`]) whose children are positioned
//! from the rects [`compute_frame`] carves: the editor cell, each
//! docked group's frame, and the splitter of every occupied side. It
//! answers `isFlipped` **YES**, and that is what makes it the right
//! container rather than a stylistic choice: the model carves top-down
//! (a `Top` band sits at `y == 0`), so a flipped view lets every rect
//! the model produces be handed to `setFrame:` verbatim, with no
//! per-child conversion that a Bottom band would get wrong. Autoresizing
//! is not used inside the area at all — the same window-resize hook that
//! re-lays the tab strip ([`crate::relayout_chrome_bands`]) re-carves
//! the area on every resize step, so nothing needs a mask to keep up.
//!
//! The editor cell (tab strip + editor + FIF dock) is an ordinary,
//! unflipped `NSView`: its children keep the springs-and-struts they
//! always had, in the cell's own coordinates, and only the *cell* moves.
//! The one Scintilla view this module ever reparents is the Document
//! Map miniature, riding inside its panel container — which AppKit
//! handles as an ordinary `viewDidMoveToWindow` cycle.
//!
//! # Groups are rehosted, never rebuilt
//!
//! A group's outermost view ([`GroupWidget::frame`]) moves between the
//! dock area and a floating [`FloatWindow`] on dock ↔ float transitions;
//! the same frame, caption and slot survive. Win32 recreates its group
//! *window* on that transition because toggling `WS_CHILD` on a live
//! HWND is fragile; an AppKit reparent is `removeFromSuperview` +
//! `addSubview:`, so there is nothing to gain from a rebuild.
//!
//! # Nothing here is ever destroyed
//!
//! Every view and window this module creates is held by a `Retained`
//! for the window's lifetime. Group frames leave a host through
//! `removeFromSuperview` after their panels have been evacuated to the
//! hidden parking view, and floating windows are `orderOut`ed and pooled
//! for reuse rather than closed. The panel *content* views are created
//! once in their own modules and only ever move — the Document Map's
//! container in particular, because the miniature inside it is one of
//! the two permanent Scintilla views the source scan in `lib.rs` pins.
//!
//! # Floating groups are borderless panels
//!
//! A [`FloatWindow`] is an `NSPanel` with no title bar: the caption is
//! the group's own, so a drag on it can re-dock, exactly as on the other
//! two backends. `Resizable` on a borderless window is what gives it
//! AppKit's edge-resize behaviour, so no hand-rolled resize border is
//! needed. It floats above the main window, hides when the application
//! deactivates (the utility-panel default the Find panel also keeps),
//! and becomes key only when a view inside it asks — so ordering one
//! front on session restore never steals the editor's focus. It carries
//! the same `makeFirstResponder:` refusal as the main window, because a
//! floating Document Map would otherwise be a second route to typing
//! into the shared document with no visible caret.
//!
//! # Coordinate space
//!
//! Every drag computation runs in **flipped screen coordinates** —
//! top-down, origin at the top-left of the primary screen — the same
//! choice Win32 and GTK make in root coordinates, and for the same
//! reason: floating groups are screen-positioned windows, and
//! `resolve_drop` has to compare their rects with the dock area's in one
//! space. AppKit's screen space is bottom-up, so [`flip_rect`] and
//! [`unflip_rect`] are the two conversions, both against the primary
//! screen's height. `compute_frame` runs in the dock area's own flipped
//! bounds, which is what its children are placed in.
//!
//! # Borrow discipline
//!
//! The model and every view it drives live in a module thread-local
//! ([`Ui`]) rather than on `CocoaUiState`, reached through [`with_dock`].
//! The reason is the layout pass: `CocoaUi::relayout_chrome` runs inside
//! a `with_state` borrow, and if the dock state lived there a relayout
//! would be declined re-entrantly on exactly the paths that most need it
//! — the blank-panel failure the results dock documents for
//! `with_state`. Keeping the dock state separate means a live
//! `with_state` borrow never blocks a carve. The rule that keeps the two
//! `RefCell`s from deadlocking on each other: code holding the dock
//! borrow **never** calls `with_state` (every shell read happens before
//! the dock borrow is taken, every shell write after it is dropped),
//! while a `with_state` closure may take a brief dock borrow
//! ([`is_visible`], [`legacy_band_width`], [`layout_area`]).
//!
//! The second half reaches further than it looks: an AppKit call made
//! under the dock borrow can run a *delegate method* synchronously —
//! `setFrame:display:` on a floating panel posts `windowDidResize:`
//! before it returns — and if that method reads the dock it is declined.
//! That is why [`apply_layout`] reconciles the view tree under the
//! borrow and positions the floating windows only after it drops. A
//! decline degrades (a skipped mirror of a rect the model already
//! holds), never corrupts, but it is logged at `debug` so a new instance
//! is findable.

use std::cell::{Cell, RefCell};

use codepp_core::dock::{
    compute_frame, resolve_drop, DockGroup, DockLayout, DockLocation, DockPanel, DockRect,
    DockSide, DragSubject, DropTarget, DropZones, MIN_FLOAT_H, MIN_FLOAT_W,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezelStyle, NSBitmapImageRep,
    NSButton, NSColor, NSCursor, NSEvent, NSEventMask, NSEventTrackingRunLoopMode, NSEventType,
    NSFloatingWindowLevel, NSFont, NSImage, NSImageRep, NSImageScaling, NSImageView,
    NSLineBreakMode, NSPanel, NSRectFill, NSResponder, NSScreen, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};

use crate::menu::Actions;
use crate::state::with_state;

/// Height of a group's caption bar (the drag bar), in points. Same
/// value as Win32's and GTK's `DOCK_CAPTION_H`, so the three backends
/// carve identically.
pub(crate) const DOCK_CAPTION_H: f64 = 22.0;
/// Height of the bottom tab bar, shown only when a group holds two or
/// more panels. Same value as the other two backends'.
pub(crate) const DOCK_TAB_BAR_H: f64 = 26.0;
/// Icon display size on a tab, in points. The `@2x` representation is
/// in the same `NSImage`, so AppKit keeps the glyph sharp on Retina.
const DOCK_TAB_ICON_PX: f64 = 16.0;
/// Horizontal padding inside a tab, either side of its content.
const DOCK_TAB_PAD: f64 = 8.0;
/// Gap between a tab's icon and its label (active tabs only).
const DOCK_TAB_ICON_GAP: f64 = 5.0;
/// Width of the caption's close ✕.
const DOCK_CLOSE_W: f64 = 22.0;
/// Points of pointer travel before a pressed caption/tab becomes a
/// drag rather than a click. Same 4-pt convention as the tab strip.
const DOCK_DRAG_THRESHOLD: i32 = 4;
/// Alpha of the drop-hint panel (`110/255`, the value Win32's layered
/// hint and GTK's composited popup both use).
const DOCK_HINT_ALPHA: f64 = 110.0 / 255.0;
/// Editor-cell minimums handed to [`compute_frame`] — the same floors
/// `ui_win32` and `ui_gtk` reserve.
const MIN_EDITOR_W: i32 = 200;
const MIN_EDITOR_H: i32 = 60;
/// AppKit's virtual key code for Escape, which cancels a live drag.
const ESCAPE_KEY_CODE: u16 = 53;
/// `NSWindowLevel` one above the floating groups, so the hint paints
/// over a float it is previewing a drop onto.
const HINT_LEVEL_ABOVE_FLOATS: isize = 1;

// --- state ---------------------------------------------------------------------

/// One live group container: the model group's id and its views. The id
/// is the identity (§7.4's key-on-ids rule); the views are rehosted,
/// never remade, on dock ↔ float transitions.
struct GroupWidget {
    id: u32,
    /// The outermost view — the one moved between the dock area and a
    /// floating window's content view.
    frame: Retained<GroupFrame>,
    /// The caption's title label (the active panel's title).
    title: Retained<NSTextField>,
    /// Holds every panel of the group; only the active one is unhidden.
    slot: Retained<NSView>,
    /// The bottom tab bar, populated only for two or more panels.
    tab_bar: Retained<NSView>,
    /// The floating window hosting `frame`, when floating.
    float: Option<Retained<FloatWindow>>,
}

/// Everything the dock mechanism owns for the window's lifetime.
struct Ui {
    /// The model. Synced to the shell's session cache by
    /// [`sync_to_shell`] after every mutation.
    layout: DockLayout,
    main_window: Retained<NSWindow>,
    /// The dock area; see the module docs for why it is flipped.
    area: Retained<DockArea>,
    /// Tab strip + editor + FIF dock, placed at `DockFrame::editor`.
    editor_cell: Retained<NSView>,
    /// Hidden home for panel content that is not currently shown, so a
    /// hidden panel's views stay under the main window and a dismantled
    /// group can never take them with it.
    parking: Retained<NSView>,
    /// The two panels' content views, created once in their own
    /// modules. Never destroyed; only moved between `parking` and a
    /// group's slot.
    workspace_content: Retained<NSView>,
    docmap_content: Retained<NSView>,
    groups: Vec<GroupWidget>,
    /// Side splitters, indexed by [`side_index`].
    splitters: [Retained<SplitterView>; 4],
    /// The translucent grey drop preview.
    hint: Retained<NSPanel>,
    /// Hidden floating windows awaiting reuse. See the module docs for
    /// why they are pooled rather than closed.
    float_pool: Vec<Retained<FloatWindow>>,
    /// The delegate every floating window reports moves and resizes
    /// to. Held here because `NSWindow.delegate` is weak.
    float_delegate: Retained<FloatDelegate>,
    /// The action target the caption ✕ buttons fire at. Held because
    /// `NSControl.target` is weak, and so the strip can rebuild against
    /// it.
    actions: Retained<Actions>,
    /// The dock area's last laid-out size — the `mid` rect every
    /// layout pass carves.
    area_size: (i32, i32),
}

impl Ui {
    /// The content view for `panel`. A field per panel rather than a
    /// map, so adding a `DockPanel` variant is a compile error here —
    /// which is what makes "every panel is hosted" hold by construction
    /// rather than by a test remembering to check. (The
    /// `every_panel_is_handed_to_the_dock` scan in `lib.rs` pins the
    /// hand-off from `build_content` to [`install`], which is the half
    /// the compiler cannot see.)
    /// `None` only for a plugin panel, which this backend has no
    /// content view for: `NPPM_DMMREGASDCKDLG` is Win32-only
    /// (DESIGN.md §7.4), so nothing here ever supplies one.
    /// [`DockLayout::drop_plugin_panels`] is called at restore for
    /// exactly that reason, so a layout reaching this function cannot
    /// name one — `None` is the fail-safe, not the expected path, and
    /// every caller skips rather than substituting a placeholder the
    /// user could neither use nor close.
    fn panel_content(&self, panel: DockPanel) -> Option<Retained<NSView>> {
        match panel {
            DockPanel::Workspace => Some(self.workspace_content.clone()),
            DockPanel::DocMap => Some(self.docmap_content.clone()),
            DockPanel::Plugin(_) => None,
        }
    }

    fn group_index(&self, id: u32) -> Option<usize> {
        self.groups.iter().position(|g| g.id == id)
    }

    /// The group hosted by `window`, if any, by window identity.
    fn group_of_window(&self, window: &NSWindow) -> Option<u32> {
        self.groups
            .iter()
            .find(|g| {
                g.float.as_ref().is_some_and(|f| {
                    let hosted: &NSWindow = f;
                    std::ptr::eq(hosted, window)
                })
            })
            .map(|g| g.id)
    }
}

thread_local! {
    /// Installed once on the main thread by [`install`].
    static DOCK: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Run `f` against the dock state if it is installed and not already
/// borrowed. `None` in either case — a re-entrant call is logged, and
/// every caller degrades rather than corrupts (see the module docs).
fn with_dock<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    DOCK.with(|d| {
        let Ok(mut guard) = d.try_borrow_mut() else {
            tracing::debug!("with_dock skipped: re-entrant call while an outer borrow was live");
            return None;
        };
        guard.as_mut().map(f)
    })
}

/// Index into the per-side arrays, in [`DockSide::ALL`] order.
pub(crate) fn side_index(side: DockSide) -> usize {
    match side {
        DockSide::Left => 0,
        DockSide::Right => 1,
        DockSide::Top => 2,
        DockSide::Bottom => 3,
    }
}

// --- pure geometry (unit-tested at the bottom) ---------------------------------

/// New band size for a splitter drag: the delta's sign depends on
/// which side the band hangs off (dragging a Right band's splitter
/// left *grows* the band). Same arithmetic as the other two backends'.
#[must_use]
fn side_drag_size(side: DockSide, size_at_start: i32, delta: (i32, i32)) -> i32 {
    match side {
        DockSide::Left => size_at_start + delta.0,
        DockSide::Right => size_at_start - delta.0,
        DockSide::Top => size_at_start + delta.1,
        DockSide::Bottom => size_at_start - delta.1,
    }
}

/// The size a panel or group opens at when it is torn off into a
/// float: a third of the main window's current width and height,
/// floored at the model's minimum — the GTK choice, for the reason it
/// records (a full-height side band makes an awkward window nobody
/// wants and has to resize anyway).
#[must_use]
fn tear_off_size(main: (i32, i32)) -> (i32, i32) {
    ((main.0 / 3).max(MIN_FLOAT_W), (main.1 / 3).max(MIN_FLOAT_H))
}

/// Where the pointer sits inside a torn-off float's caption: the same
/// *fraction* along the caption as it had along the source (so a grab
/// near the right end stays near the right end), clamped inside the
/// new width, and vertically mid-caption.
#[must_use]
fn tear_off_grab(cursor_dx: i32, source_w: i32, new_w: i32) -> (i32, i32) {
    let fraction = f64::from(cursor_dx.clamp(0, source_w.max(1))) / f64::from(source_w.max(1));
    let x = (fraction * f64::from(new_w)).round() as i32;
    (x.clamp(0, new_w.max(0)), (DOCK_CAPTION_H / 2.0) as i32)
}

/// An AppKit (bottom-up) screen rect as a top-down [`DockRect`], given
/// the primary screen's height.
#[must_use]
fn flip_rect(r: NSRect, primary_h: f64) -> DockRect {
    DockRect::new(
        r.origin.x.round() as i32,
        (primary_h - r.origin.y - r.size.height).round() as i32,
        r.size.width.round() as i32,
        r.size.height.round() as i32,
    )
}

/// Inverse of [`flip_rect`].
#[must_use]
fn unflip_rect(r: DockRect, primary_h: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(f64::from(r.x), primary_h - f64::from(r.y) - f64::from(r.h)),
        NSSize::new(f64::from(r.w.max(1)), f64::from(r.h.max(1))),
    )
}

/// A model rect as an `NSRect` in the flipped dock area's coordinates
/// — no conversion beyond the number type, which is the point of the
/// area being flipped.
#[must_use]
fn area_rect(r: DockRect) -> NSRect {
    NSRect::new(
        NSPoint::new(f64::from(r.x), f64::from(r.y)),
        NSSize::new(f64::from(r.w.max(0)), f64::from(r.h.max(0))),
    )
}

/// The three child rects of a group frame of size `w`×`h` holding
/// `panels` tabs, in the frame's flipped coordinates: caption, slot,
/// tab bar (zero-height when a single panel needs no bar). Pure so the
/// boundaries are testable without a window server.
#[must_use]
fn group_child_rects(w: f64, h: f64, panels: usize) -> (NSRect, NSRect, NSRect) {
    let bar_h = if panels >= 2 { DOCK_TAB_BAR_H } else { 0.0 };
    let caption = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, DOCK_CAPTION_H));
    let slot = NSRect::new(
        NSPoint::new(0.0, DOCK_CAPTION_H),
        NSSize::new(w, (h - DOCK_CAPTION_H - bar_h).max(0.0)),
    );
    let bar = NSRect::new(
        NSPoint::new(0.0, (h - bar_h).max(0.0)),
        NSSize::new(w, bar_h),
    );
    (caption, slot, bar)
}

/// A `WindowGeometry` as a top-down rect, when every field is present.
/// The persisted origin is AppKit's bottom-left one, so it is flipped
/// like any other screen rect.
#[must_use]
fn geometry_rect(
    geometry: Option<codepp_core::WindowGeometry>,
    primary_h: f64,
) -> Option<DockRect> {
    let g = geometry?;
    let r = NSRect::new(
        NSPoint::new(f64::from(g.x?), f64::from(g.y?)),
        NSSize::new(f64::from(g.width?), f64::from(g.height?)),
    );
    Some(flip_rect(r, primary_h))
}

// --- screen-coordinate helpers -------------------------------------------------

/// The primary screen's height — the flip axis for every screen rect.
/// Zero with no screens (headless), which degrades every drag to a
/// no-op rather than panicking.
fn primary_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    if screens.count() == 0 {
        return 0.0;
    }
    screens.objectAtIndex(0).frame().size.height
}

/// `view`'s on-screen rect in flipped screen coordinates, or `None`
/// while it is not inside a window.
fn screen_rect(view: &NSView, primary_h: f64) -> Option<DockRect> {
    let window = view.window()?;
    let in_window = view.convertRect_toView(view.bounds(), None);
    Some(flip_rect(window.convertRectToScreen(in_window), primary_h))
}

/// The pointer position of `event` in flipped screen coordinates.
fn event_screen_point(event: &NSEvent, mtm: MainThreadMarker, primary_h: f64) -> (i32, i32) {
    let local = event.locationInWindow();
    // `locationInWindow` is already in screen space for an event with
    // no window; otherwise convert through the window.
    let screen = match event.window(mtm) {
        Some(window) => {
            window
                .convertRectToScreen(NSRect::new(local, NSSize::new(0.0, 0.0)))
                .origin
        }
        None => local,
    };
    (
        screen.x.round() as i32,
        (primary_h - screen.y).round() as i32,
    )
}

/// The `i`th subview of `view`, if it has one.
fn nth_subview(view: &NSView, i: usize) -> Option<Retained<NSView>> {
    let subviews = view.subviews();
    (i < subviews.count()).then(|| subviews.objectAtIndex(i))
}

/// True iff `view`'s superview is `container`.
fn is_child_of(view: &NSView, container: &NSView) -> bool {
    // SAFETY: a plain accessor on a live view, on the main thread — every
    // caller is a main-thread AppKit path.
    unsafe { view.superview() }.is_some_and(|p| std::ptr::eq(&raw const *p, container))
}

// --- view classes --------------------------------------------------------------

define_class!(
    // SAFETY: an `NSView` subclass overriding only the flip flag.
    // Main-thread-only, as every view is.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockArea"]
    pub struct DockArea;

    unsafe impl NSObjectProtocol for DockArea {}

    impl DockArea {
        /// Top-down, so the rects `compute_frame` carves place children
        /// verbatim. A deliberate exception to the `at_callback_boundary`
        /// rule, on the same reasoning as `PinView::isFlipped`: it
        /// returns a literal, cannot panic, and AppKit calls it on every
        /// layout pass. The other literal-returning overrides in this
        /// module (`isFlipped`, `acceptsFirstMouse:`,
        /// `canBecomeKeyWindow`) take the same exception and say so.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }
    }
);

impl DockArea {
    pub(crate) fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser
        // and this subclass adds no ivars needing other initialisation.
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

/// Per-instance state for [`GroupFrame`]: how many panels it holds,
/// which decides whether the tab bar takes height. Kept on the view so
/// a floating window's resize can re-lay the frame's children without
/// a dock borrow.
pub struct GroupFrameIvars {
    panels: Cell<usize>,
}

define_class!(
    // SAFETY: an `NSView` subclass overriding drawing and the flip flag.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockGroupFrame"]
    #[ivars = GroupFrameIvars]
    pub struct GroupFrame;

    unsafe impl NSObjectProtocol for GroupFrame {}

    impl GroupFrame {
        /// Flipped for the same reason as the area: caption at the
        /// top, tab bar at the bottom, with no mirroring. A literal,
        /// so no callback boundary — same exception as `DockArea`'s.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }

        /// An opaque body, so a floating group never shows what is
        /// behind it through the gaps between its controls.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("dock:frame:drawRect", (), || {
                NSColor::windowBackgroundColor().setFill();
                NSRectFill(self.bounds());
            });
        }
    }
);

impl GroupFrame {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(GroupFrameIvars {
            panels: Cell::new(1),
        });
        // SAFETY: `NSView`'s designated initialiser, on a freshly
        // allocated instance whose ivars are already set.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

/// Per-instance state for [`CaptionView`]: the group it drags.
pub struct CaptionIvars {
    id: Cell<u32>,
}

define_class!(
    // SAFETY: an `NSView` subclass overriding drawing and mouse entry
    // points, adding no state AppKit reads.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockCaption"]
    #[ivars = CaptionIvars]
    pub struct CaptionView;

    unsafe impl NSObjectProtocol for CaptionView {}

    impl CaptionView {
        /// Flipped like the frame it sits in, so [`paint_bar`]'s
        /// "bottom" is the edge that meets the slot below. A literal —
        /// same callback-boundary exception as `DockArea::isFlipped`.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("dock:caption:drawRect", (), || {
                paint_bar(self.bounds(), true);
            });
        }

        /// The first click on a non-key floating window must start the
        /// drag rather than only bring the window forward — otherwise
        /// every re-dock of a float takes two presses. A literal, so no
        /// callback boundary — same exception as `isFlipped`.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> Bool {
            Bool::YES
        }

        /// The whole caption is the drag bar — except its ✕. AppKit's
        /// default resolves a press on the title label to the label,
        /// and an `NSTextField` keeps a mouse-down it does not use, so
        /// the drag would only start from the gaps between the title
        /// and the button. The ✕ is a real control and keeps its hit.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            crate::at_callback_boundary("dock:caption:hitTest", None, || {
                claim_hit(self, point, true)
            })
        }

        /// Take the whole press-drag-release gesture. See
        /// [`track_caption`].
        ///
        /// `me` is an owned reference held until the gesture — and the
        /// reconcile it can trigger — has finished. A caption drag
        /// merged into another group dismantles this view's whole
        /// frame, and Objective-C dispatch does not retain the
        /// receiver, so without it the last strong reference could go
        /// while this `mouseDown:` is still on the stack. The tracker
        /// also never touches the view after the model mutation; both
        /// halves of the tab strip's rule, for the reason it records.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("dock:caption:mouseDown", (), || {
                let me = self.retain();
                track_caption(me.ivars().id.get(), event);
            });
        }
    }
);

/// `hitTest:` for a view whose every point is its own: the whole tab
/// is a press target, and so is the whole caption bar. Without this a
/// press on the tab's `NSImageView` — which is most of an inactive tab
/// — never reached the tab, because an image view takes the mouse-down
/// for its own drag-out behaviour and does nothing with it. Caught by
/// driving the app: a synthetic click at a tab's centre switched
/// nothing. With `keep_buttons`, a press that resolves to an
/// `NSButton` child (the caption ✕) stays with the button.
fn claim_hit(view: &NSView, point: NSPoint, keep_buttons: bool) -> Option<Retained<NSView>> {
    // `point` is in the superview's coordinate system, so test against
    // the frame rather than the bounds.
    let f = view.frame();
    let inside = point.x >= f.origin.x
        && point.x < f.origin.x + f.size.width
        && point.y >= f.origin.y
        && point.y < f.origin.y + f.size.height;
    if !inside || view.isHidden() {
        return None;
    }
    if keep_buttons {
        // SAFETY: a plain accessor on a live view, on the main thread —
        // `hitTest:` is only ever sent by AppKit's event dispatch there.
        let superview = unsafe { view.superview() };
        let local = view.convertPoint_fromView(point, superview.as_deref());
        for child in &view.subviews() {
            let cf = child.frame();
            let on_child = local.x >= cf.origin.x
                && local.x < cf.origin.x + cf.size.width
                && local.y >= cf.origin.y
                && local.y < cf.origin.y + cf.size.height;
            if on_child && child.downcast_ref::<NSButton>().is_some() {
                return Some(child);
            }
        }
    }
    Some(view.retain())
}

impl CaptionView {
    fn new(id: u32, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CaptionIvars { id: Cell::new(id) });
        // SAFETY: as for `GroupFrame::new`.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

/// Per-instance state for [`TabView`]: which tab of which group, and
/// the panel it stands for.
pub struct TabIvars {
    group: Cell<u32>,
    index: Cell<usize>,
    panel: Cell<DockPanel>,
    active: Cell<bool>,
}

define_class!(
    // SAFETY: an `NSView` subclass overriding drawing and mouse entry
    // points, adding no state AppKit reads.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockTab"]
    #[ivars = TabIvars]
    pub struct TabView;

    unsafe impl NSObjectProtocol for TabView {}

    impl TabView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("dock:tab:drawRect", (), || {
                if self.ivars().active.get() {
                    NSColor::controlBackgroundColor().setFill();
                    NSRectFill(self.bounds());
                }
            });
        }

        /// A literal — see `CaptionView`'s for why this one and its
        /// siblings skip the callback boundary.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> Bool {
            Bool::YES
        }

        /// Every point of the tab is the tab's — see [`claim_hit`].
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            crate::at_callback_boundary("dock:tab:hitTest", None, || {
                claim_hit(self, point, false)
            })
        }

        /// A press that ends without travel switches tabs; one that
        /// crosses the threshold drags the panel out. See [`track_tab`].
        ///
        /// `me` is an owned reference held until the gesture has
        /// finished: an ordinary tab switch reconciles, and the
        /// reconcile rebuilds this tab bar, removing *this* view while
        /// its `mouseDown:` is still on the stack. See `CaptionView`'s.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("dock:tab:mouseDown", (), || {
                let me = self.retain();
                let iv = me.ivars();
                track_tab(iv.group.get(), iv.index.get(), iv.panel.get(), event);
            });
        }
    }
);

impl TabView {
    fn new(
        frame: NSRect,
        group: u32,
        index: usize,
        panel: DockPanel,
        active: bool,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TabIvars {
            group: Cell::new(group),
            index: Cell::new(index),
            panel: Cell::new(panel),
            active: Cell::new(active),
        });
        // SAFETY: as for `GroupFrame::new`.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// Per-instance state for [`SplitterView`]: which side it resizes.
pub struct SplitterIvars {
    side: Cell<DockSide>,
}

define_class!(
    // SAFETY: an `NSView` subclass overriding drawing, cursor rects and
    // mouse entry points.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockSplitter"]
    #[ivars = SplitterIvars]
    pub struct SplitterView;

    unsafe impl NSObjectProtocol for SplitterView {}

    impl SplitterView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("dock:splitter:drawRect", (), || {
                NSColor::separatorColor().setFill();
                NSRectFill(self.bounds());
            });
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            crate::at_callback_boundary("dock:splitter:resetCursorRects", (), || {
                // Both are deprecated in favour of the directional
                // macOS 15+ variants; Code++ has no such deployment
                // floor. Same call the FIF divider makes.
                #[allow(deprecated)]
                let cursor = match self.ivars().side.get() {
                    DockSide::Left | DockSide::Right => NSCursor::resizeLeftRightCursor(),
                    DockSide::Top | DockSide::Bottom => NSCursor::resizeUpDownCursor(),
                };
                self.addCursorRect_cursor(self.bounds(), &cursor);
            });
        }

        /// A literal — see `CaptionView`'s for why it skips the
        /// callback boundary.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> Bool {
            Bool::YES
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            crate::at_callback_boundary("dock:splitter:mouseDown", (), || {
                track_splitter(self.ivars().side.get(), event);
            });
        }
    }
);

impl SplitterView {
    fn new(side: DockSide, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SplitterIvars {
            side: Cell::new(side),
        });
        // SAFETY: as for `GroupFrame::new`.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

/// Fill a caption or tab bar: a slightly recessed background with a
/// hairline along the edge that meets the panel content. `bounds` are
/// a **flipped** view's, so "bottom" is the largest `y` — both callers
/// answer `isFlipped` YES.
fn paint_bar(bounds: NSRect, line_at_bottom: bool) {
    NSColor::underPageBackgroundColor().setFill();
    NSRectFill(bounds);
    NSColor::separatorColor().setFill();
    let y = if line_at_bottom {
        bounds.size.height - 1.0
    } else {
        0.0
    };
    NSRectFill(NSRect::new(
        NSPoint::new(0.0, y.max(0.0)),
        NSSize::new(bounds.size.width, 1.0),
    ));
}

define_class!(
    // SAFETY: an `NSView` subclass overriding only drawing.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockTabBar"]
    /// The bottom tab bar's background: painted by a tiny subclass so
    /// the bar, like the caption, is opaque in a float.
    pub struct TabBarView;

    unsafe impl NSObjectProtocol for TabBarView {}

    impl TabBarView {
        /// Flipped, so [`paint_bar`]'s "top" is the edge that meets the
        /// slot above. A literal — same exception as `isFlipped` elsewhere.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            crate::at_callback_boundary("dock:tabbar:drawRect", (), || {
                paint_bar(self.bounds(), false);
            });
        }
    }
);

impl TabBarView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: as for `DockArea::new`.
        unsafe { msg_send![this, initWithFrame: NSRect::ZERO] }
    }
}

// --- the floating window ---------------------------------------------------------

define_class!(
    // SAFETY: an `NSPanel` subclass overriding two methods, each of
    // which chains to `super` in every case but the one it exists to
    // change. Main-thread-only, as every window is.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockFloat"]
    pub struct FloatWindow;

    unsafe impl NSObjectProtocol for FloatWindow {}

    impl FloatWindow {
        /// A borderless window refuses key status by default, which
        /// would leave the workspace tree inside it unable to take a
        /// selection. Saying yes here is what `becomesKeyOnlyIfNeeded`
        /// then moderates: key only when a view asks for it.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> Bool {
            Bool::YES
        }

        /// The same refusal `crate::window::MainWindow` makes, for the
        /// same reason: a floating Document Map is still bound to the
        /// editable document, and Tab-cycling inside this window would
        /// otherwise reach its miniature.
        #[unsafe(method(makeFirstResponder:))]
        fn make_first_responder(&self, responder: Option<&NSResponder>) -> Bool {
            crate::at_callback_boundary("dock:float:makeFirstResponder", Bool::NO, || {
                if let Some(view) = responder.and_then(|r| r.downcast_ref::<NSView>()) {
                    if crate::docmap::owns_view(view) {
                        return Bool::NO;
                    }
                }
                // SAFETY: plain chain to `NSPanel`'s implementation with
                // the arguments it was given.
                unsafe { msg_send![super(self), makeFirstResponder: responder] }
            })
        }
    }
);

impl FloatWindow {
    /// Build one pooled floating window. See the module docs for the
    /// choices; each is also noted inline.
    fn new(delegate: &FloatDelegate, mtm: MainThreadMarker) -> Retained<Self> {
        // `Resizable` on an otherwise borderless mask is what gives the
        // window AppKit's edge-resize behaviour without a title bar.
        let style = NSWindowStyleMask::Resizable;
        // SAFETY: `NSWindow`'s designated initialiser on a freshly
        // allocated instance of our own subclass, which adds no ivars.
        // `defer: false` creates the window-server resources now.
        let win: Retained<Self> = unsafe {
            msg_send![
                Self::alloc(mtm),
                initWithContentRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(f64::from(MIN_FLOAT_W), f64::from(MIN_FLOAT_H))),
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };
        // Pooled, never closed — and a `Retained` owns it regardless.
        // SAFETY: turning the release *off* is the safe direction.
        unsafe { win.setReleasedWhenClosed(false) };
        win.setTitle(&NSString::from_str("Code++"));
        // Above the main window, hidden while the app is inactive — the
        // utility-panel behaviour the Find panel keeps for the same
        // reason (a floating-level window that does not hide follows the
        // user into other applications).
        win.setFloatingPanel(true);
        win.setHidesOnDeactivate(true);
        // Key only when a view inside asks. A caption press therefore
        // raises the float without taking focus off the editor, which is
        // what "never focused on map" buys GTK.
        win.setBecomesKeyOnlyIfNeeded(true);
        win.setHasShadow(true);
        win.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
        win.setContentMinSize(NSSize::new(f64::from(MIN_FLOAT_W), f64::from(MIN_FLOAT_H)));
        // The caption is the move affordance and it re-docks; AppKit's
        // own move-by-background would move the window without ever
        // consulting the drop resolver.
        win.setMovableByWindowBackground(false);
        // SAFETY: the delegate is held by the dock state for the
        // process lifetime; `NSWindow.delegate` is weak.
        win.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        win
    }
}

define_class!(
    // SAFETY: plain `NSObject` subclass with no ivars; every method is
    // invoked by AppKit on the main thread, which is also where the
    // dock thread-local lives.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppDockFloatDelegate"]
    pub struct FloatDelegate;

    unsafe impl NSObjectProtocol for FloatDelegate {}

    unsafe impl NSWindowDelegate for FloatDelegate {
        /// A floating window moved (by us, or by AppKit clamping it):
        /// mirror the live rect into the model.
        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, notification: &NSNotification) {
            crate::at_callback_boundary("dock:float:windowDidMove", (), || {
                on_float_configured(notification);
            });
        }

        /// A floating window resized (an edge drag, or us): mirror the
        /// rect and re-lay the group's children to the new size.
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, notification: &NSNotification) {
            crate::at_callback_boundary("dock:float:windowDidResize", (), || {
                on_float_configured(notification);
            });
        }
    }
);

impl FloatDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own
        // class, which adds no ivars.
        unsafe { msg_send![this, init] }
    }
}

// --- installation ------------------------------------------------------------------

/// The views `build_content` hands the dock at [`install`]: the area
/// it carves, the editor cell it carves around, and the two panels'
/// content it hosts. One field per `DockPanel`, resolved by
/// [`Ui::panel_content`]'s exhaustive match — so a new panel variant is
/// a compile error here rather than a panel that builds and never
/// draws.
pub(crate) struct DockHosts {
    pub area: Retained<DockArea>,
    pub editor_cell: Retained<NSView>,
    pub workspace: Retained<NSView>,
    pub docmap: Retained<NSView>,
}

/// Build the dock chrome around `hosts.area` and publish the state.
/// Called once from `build_content`, after the two panel content views
/// exist and before the window is shown. `hosts.editor_cell` must
/// already be a child of the area; `content` is the window's content
/// view, which the hidden parking view joins.
pub(crate) fn install(
    main_window: &NSWindow,
    content: &NSView,
    hosts: DockHosts,
    actions: &Retained<Actions>,
    mtm: MainThreadMarker,
) {
    let DockHosts {
        area,
        editor_cell,
        workspace,
        docmap,
    } = hosts;
    let parking = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    parking.setHidden(true);
    content.addSubview(&parking);
    parking.addSubview(&workspace);
    parking.addSubview(&docmap);

    let splitters = DockSide::ALL.map(|side| {
        let splitter = SplitterView::new(side, mtm);
        splitter.setHidden(true);
        area.addSubview(&splitter);
        splitter
    });
    let hint = build_hint(mtm);
    let float_delegate = FloatDelegate::new(mtm);

    let ui = Ui {
        layout: DockLayout::new(),
        main_window: main_window.retain(),
        area,
        editor_cell,
        parking,
        workspace_content: workspace,
        docmap_content: docmap,
        groups: Vec::new(),
        splitters,
        hint,
        float_pool: Vec::new(),
        float_delegate,
        actions: actions.clone(),
        area_size: (0, 0),
    };
    DOCK.with(|d| *d.borrow_mut() = Some(ui));
}

/// The drop-hint panel: borderless, non-activating, mouse-transparent,
/// painted flat translucent grey. Shown/moved by [`show_hint`] while a
/// drag is live; never key.
fn build_hint(mtm: MainThreadMarker) -> Retained<NSPanel> {
    let hint = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
        NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the safe direction, as everywhere else.
    unsafe { hint.setReleasedWhenClosed(false) };
    hint.setOpaque(false);
    hint.setHasShadow(false);
    hint.setIgnoresMouseEvents(true);
    hint.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.5, DOCK_HINT_ALPHA)));
    hint.setLevel(NSFloatingWindowLevel + HINT_LEVEL_ABOVE_FLOATS);
    hint
}

fn show_hint(hint: &NSPanel, rect: DockRect, primary_h: f64) {
    hint.setFrame_display(unflip_rect(rect, primary_h), true);
    if !hint.isVisible() {
        hint.orderFront(None);
    }
}

fn hide_hint(hint: &NSPanel) {
    if hint.isVisible() {
        hint.orderOut(None);
    }
}

// --- group chrome ----------------------------------------------------------------------

/// Build a group container: frame, caption (title + ✕), slot, tab bar.
fn build_group(id: u32, actions: &Actions, mtm: MainThreadMarker) -> GroupWidget {
    let frame = GroupFrame::new(mtm);

    let caption = CaptionView::new(id, mtm);
    let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    title.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    title.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    caption.addSubview(&title);
    // The ✕ is a real button, so a press on it never reaches the
    // caption's drag loop: AppKit hit-tests subviews first.
    let close = NSButton::initWithFrame(NSButton::alloc(mtm), NSRect::ZERO);
    close.setTitle(&NSString::from_str("\u{2715}"));
    close.setBezelStyle(NSBezelStyle::AccessoryBarAction);
    close.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    close.setToolTip(Some(&NSString::from_str("Close panel")));
    close.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
    // The group id rides in the tag, which is how `codeppDockClose:`
    // knows whose active panel to close.
    close.setTag(id as isize);
    // SAFETY: the target is a weak reference to an object the window
    // state owns for the process lifetime, and the selector is a
    // compile-time `sel!` literal `Actions` implements.
    unsafe {
        close.setTarget(Some(actions));
        close.setAction(Some(sel!(codeppDockClose:)));
    }
    caption.addSubview(&close);
    frame.addSubview(&caption);

    let slot = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    frame.addSubview(&slot);

    let tab_bar = TabBarView::new(mtm);
    tab_bar.setHidden(true);
    frame.addSubview(&tab_bar);

    GroupWidget {
        id,
        frame,
        title,
        slot,
        tab_bar: Retained::into_super(tab_bar),
        float: None,
    }
}

/// Place a group frame's children for its current bounds. Reads the
/// panel count off the frame's own ivars, so a floating window's
/// resize delegate can call it with no dock borrow.
fn layout_group_frame(frame: &GroupFrame) {
    let size = frame.bounds().size;
    let (caption_rc, slot_rc, bar_rc) =
        group_child_rects(size.width, size.height, frame.ivars().panels.get());
    // Built in a fixed order by `build_group`: caption, slot, tab bar.
    // Positional, with the classes checked in debug builds so an edit
    // that inserts a child ahead of them fails loudly rather than
    // mis-positioning controls.
    if let Some(caption) = nth_subview(frame, 0) {
        debug_assert!(caption.downcast_ref::<CaptionView>().is_some());
        caption.setFrame(caption_rc);
        layout_caption(&caption, caption_rc.size.width);
    }
    if let Some(slot) = nth_subview(frame, 1) {
        slot.setFrame(slot_rc);
        for content in &slot.subviews() {
            content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), slot_rc.size));
        }
    }
    if let Some(bar) = nth_subview(frame, 2) {
        debug_assert!(bar.downcast_ref::<TabBarView>().is_some());
        bar.setFrame(bar_rc);
        bar.setHidden(bar_rc.size.height <= 0.0);
    }
}

/// The caption's own two children: title on the left, ✕ on the right.
fn layout_caption(caption: &NSView, width: f64) {
    if let Some(title) = nth_subview(caption, 0) {
        title.setFrame(NSRect::new(
            NSPoint::new(6.0, 2.0),
            NSSize::new((width - DOCK_CLOSE_W - 10.0).max(0.0), DOCK_CAPTION_H - 4.0),
        ));
    }
    if let Some(close) = nth_subview(caption, 1) {
        debug_assert!(close.downcast_ref::<NSButton>().is_some());
        close.setFrame(NSRect::new(
            NSPoint::new((width - DOCK_CLOSE_W - 1.0).max(0.0), 0.0),
            NSSize::new(DOCK_CLOSE_W, DOCK_CAPTION_H),
        ));
    }
}

/// The tab-bar icon for `panel` — the same quick-action-bar art the
/// other two backends use, both scales in one image. `None` on a decode
/// failure (cosmetic; the tab keeps its tooltip and, when active, its
/// label).
fn panel_icon(panel: DockPanel) -> Option<Retained<NSImage>> {
    let (at_1x, at_2x): (&[u8], &[u8]) = match panel {
        DockPanel::Workspace => (
            include_bytes!("../../../assets/icons/folder-workspace.png"),
            include_bytes!("../../../assets/icons/folder-workspace@2x.png"),
        ),
        DockPanel::DocMap => (
            include_bytes!("../../../assets/icons/document-map.png"),
            include_bytes!("../../../assets/icons/document-map@2x.png"),
        ),
        // No artwork for a panel this backend cannot host. A tab
        // without an icon keeps its label, which is the same
        // degradation a decode failure takes.
        DockPanel::Plugin(_) => return None,
    };
    let image = NSImage::initWithSize(
        NSImage::alloc(),
        NSSize::new(DOCK_TAB_ICON_PX, DOCK_TAB_ICON_PX),
    );
    let mut added = 0usize;
    for bytes in [at_1x, at_2x] {
        let data = objc2_foundation::NSData::with_bytes(bytes);
        let Some(rep) = NSBitmapImageRep::imageRepWithData(&data) else {
            continue;
        };
        let rep: Retained<NSImageRep> = Retained::into_super(rep);
        image.addRepresentation(&rep);
        added += 1;
    }
    if added == 0 {
        tracing::warn!(?panel, "dock: tab icon decode failed");
        return None;
    }
    Some(image)
}

/// Build one tab of a multi-panel group: icon, plus the label when
/// active. Returns the tab and its width.
fn build_tab(
    x: f64,
    group: u32,
    index: usize,
    panel: DockPanel,
    active: bool,
    mtm: MainThreadMarker,
) -> (Retained<TabView>, f64) {
    let icon = panel_icon(panel);
    let mut width = DOCK_TAB_PAD * 2.0 + DOCK_TAB_ICON_PX;
    let label = active.then(|| {
        let label = NSTextField::labelWithString(&NSString::from_str(panel.title()), mtm);
        label.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        label.sizeToFit();
        width += DOCK_TAB_ICON_GAP + label.frame().size.width;
        label
    });
    let tab = TabView::new(
        NSRect::new(NSPoint::new(x, 0.0), NSSize::new(width, DOCK_TAB_BAR_H)),
        group,
        index,
        panel,
        active,
        mtm,
    );
    tab.setToolTip(Some(&NSString::from_str(panel.title())));
    let icon_y = ((DOCK_TAB_BAR_H - DOCK_TAB_ICON_PX) / 2.0).max(0.0);
    if let Some(icon) = icon {
        let view = NSImageView::imageViewWithImage(&icon, mtm);
        view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        view.setFrame(NSRect::new(
            NSPoint::new(DOCK_TAB_PAD, icon_y),
            NSSize::new(DOCK_TAB_ICON_PX, DOCK_TAB_ICON_PX),
        ));
        tab.addSubview(&view);
    }
    if let Some(label) = label {
        let size = label.frame().size;
        label.setFrame(NSRect::new(
            NSPoint::new(
                DOCK_TAB_PAD + DOCK_TAB_ICON_PX + DOCK_TAB_ICON_GAP,
                ((DOCK_TAB_BAR_H - size.height) / 2.0).max(0.0),
            ),
            size,
        ));
        tab.addSubview(&label);
    }
    (tab, width)
}

// --- reconciler ----------------------------------------------------------------------

/// Make the view tree match the model, relayout, and refresh every
/// indicator. The single funnel every mutation goes through —
/// show/hide, drops, tab switches, restore.
///
/// Two phases: the first holds the dock borrow and touches only the
/// view tree; the second, with that borrow dropped, positions the
/// floating windows (whose delegates read the dock), re-lays the chrome
/// (which needs `with_state`) and does the shell-side work — see the
/// module docs for why the two never nest in that direction.
pub(crate) fn apply_layout() {
    let docmap_visible = with_dock(|d| {
        reconcile(d);
        d.layout.is_visible(DockPanel::DocMap)
    });
    sync_floats();
    crate::relayout_chrome_bands();
    // A freshly shown Document Map needs its doc binding + the
    // viewport box caught up (it skips both while hidden).
    if docmap_visible == Some(true) {
        crate::docmap::sync_to_active_tab();
    }
    sync_indicators();
    sync_to_shell();
}

/// Phase one of [`apply_layout`]: the view tree only.
fn reconcile(d: &mut Ui) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let layout = d.layout.clone();

    // 1. Groups gone from the model lose their views — panels are
    //    evacuated to parking first, so nothing of ours is ever inside
    //    a container on its way out.
    let mut i = 0;
    while i < d.groups.len() {
        if layout.group(d.groups[i].id).is_some() {
            i += 1;
        } else {
            let gone = d.groups.remove(i);
            dismantle_group(d, gone);
        }
    }

    // 2. New model groups get views.
    for group in layout.groups() {
        if d.group_index(group.id).is_none() {
            let actions = d.actions.clone();
            d.groups.push(build_group(group.id, &actions, mtm));
        }
    }

    // 3. Host each group where the model says, then fill it.
    for group in layout.groups() {
        let Some(gi) = d.group_index(group.id) else {
            continue;
        };
        rehost_group(d, gi, group.location, mtm);
        fill_group(d, gi, group, mtm);
    }

    // 4. Hidden panels go to parking.
    for panel in DockPanel::BUILT_IN {
        if !layout.is_visible(panel) {
            park(d, panel);
        }
    }

    // 5. Splitters follow their side's occupancy.
    for side in DockSide::ALL {
        let occupied = layout.groups_on(side).next().is_some();
        d.splitters[side_index(side)].setHidden(!occupied);
    }
}

/// Retire a group's views: panels back to parking, the frame out of
/// wherever it is hosted, a floating window back to the pool. The
/// `GroupWidget` drops at the end holding nothing of ours.
fn dismantle_group(d: &mut Ui, mut g: GroupWidget) {
    for child in &g.slot.subviews() {
        child.removeFromSuperview();
        d.parking.addSubview(&child);
    }
    g.frame.removeFromSuperview();
    if let Some(win) = g.float.take() {
        win.orderOut(None);
        d.float_pool.push(win);
    }
}

/// Put group `gi`'s frame in the dock area or a floating window to
/// match `location`. Positioning a float happens in [`sync_floats`],
/// outside the dock borrow.
fn rehost_group(d: &mut Ui, gi: usize, location: DockLocation, mtm: MainThreadMarker) {
    match location {
        DockLocation::Side(_) => {
            let g = &mut d.groups[gi];
            if let Some(win) = g.float.take() {
                win.orderOut(None);
                d.float_pool.push(win);
            }
            let g = &d.groups[gi];
            if !is_child_of(&g.frame, &d.area) {
                // Out of the float's content view (a no-op when the
                // frame has no superview), then into the area.
                g.frame.removeFromSuperview();
                // Placed by the layout pass; no mask, since the area
                // lays every child out by hand.
                g.frame
                    .setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);
                d.area.addSubview(&g.frame);
            }
        }
        DockLocation::Floating(_) => {
            if d.groups[gi].float.is_some() {
                return;
            }
            let win = d
                .float_pool
                .pop()
                .unwrap_or_else(|| FloatWindow::new(&d.float_delegate, mtm));
            let g = &mut d.groups[gi];
            g.frame.removeFromSuperview();
            if let Some(content) = win.contentView() {
                g.frame.setFrame(content.bounds());
                // Inside a window the frame follows the content view,
                // so an edge resize by AppKit reaches it before the
                // delegate re-lays its children.
                g.frame.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable
                        | NSAutoresizingMaskOptions::ViewHeightSizable,
                );
                content.addSubview(&g.frame);
            }
            g.float = Some(win);
        }
    }
}

/// Move `group`'s panels into its slot, show the active one, set the
/// caption, rebuild the tab bar.
fn fill_group(d: &mut Ui, gi: usize, group: &DockGroup, mtm: MainThreadMarker) {
    let contents: Vec<Option<Retained<NSView>>> =
        group.panels.iter().map(|p| d.panel_content(*p)).collect();
    let g = &mut d.groups[gi];
    for (i, content) in contents.iter().enumerate() {
        let Some(content) = content else {
            continue;
        };
        if !is_child_of(content, &g.slot) {
            content.removeFromSuperview();
            content.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            g.slot.addSubview(content);
        }
        content.setHidden(i != group.active);
    }
    g.title
        .setStringValue(&NSString::from_str(group.active_panel().title()));
    g.frame.ivars().panels.set(group.panels.len());
    rebuild_tab_bar(g, group, mtm);
    layout_group_frame(&g.frame);
}

/// Rebuild a group's tab bar from scratch — a handful of small views,
/// far cheaper than diffing, and it runs only on a dock mutation.
fn rebuild_tab_bar(g: &mut GroupWidget, group: &DockGroup, mtm: MainThreadMarker) {
    for child in &g.tab_bar.subviews() {
        child.removeFromSuperview();
    }
    if group.panels.len() < 2 {
        return;
    }
    let mut x = 0.0;
    for (i, panel) in group.panels.iter().enumerate() {
        let (tab, width) = build_tab(x, g.id, i, *panel, i == group.active, mtm);
        g.tab_bar.addSubview(&tab);
        x += width;
    }
}

/// Send a hidden panel's content to parking.
fn park(d: &mut Ui, panel: DockPanel) {
    let Some(content) = d.panel_content(panel) else {
        return;
    };
    if !is_child_of(&content, &d.parking) {
        content.removeFromSuperview();
        d.parking.addSubview(&content);
    }
}

/// Carve the dock area at `(w, h)` and place its children: the editor
/// cell, each docked group's frame and the splitter of every occupied
/// side. Called from `CocoaUi::relayout_chrome` on every layout pass —
/// which is inside a `with_state` borrow, so this must take only the
/// dock borrow and call nothing that needs the shell.
pub(crate) fn layout_area(w: f64, h: f64) {
    with_dock(|d| {
        d.area_size = (w.round() as i32, h.round() as i32);
        place_children(d);
    });
}

/// The layout pass proper, under the dock borrow.
fn place_children(d: &Ui) {
    let (w, h) = d.area_size;
    if w <= 0 || h <= 0 {
        return;
    }
    let frame = compute_frame(
        DockRect::new(0, 0, w, h),
        &d.layout,
        MIN_EDITOR_W,
        MIN_EDITOR_H,
    );
    d.editor_cell.setFrame(area_rect(frame.editor));
    for band in &frame.bands {
        d.splitters[side_index(band.side)].setFrame(area_rect(band.splitter));
        for (gid, rect) in &band.groups {
            if let Some(g) = d.groups.iter().find(|g| g.id == *gid && g.float.is_none()) {
                g.frame.setFrame(area_rect(*rect));
                layout_group_frame(&g.frame);
            }
        }
    }
}

/// Phase two of [`apply_layout`]: position and show every floating
/// window from the model. Outside the dock borrow, because
/// `setFrame:display:` posts `windowDidResize:` synchronously and that
/// delegate mirrors the rect back through [`with_dock`].
fn sync_floats() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let primary_h = primary_height(mtm);
    let floats: Vec<(Retained<FloatWindow>, DockRect)> = with_dock(|d| {
        d.groups
            .iter()
            .filter_map(|g| {
                let win = g.float.clone()?;
                match d.layout.group(g.id)?.location {
                    DockLocation::Floating(rect) => Some((win, rect)),
                    DockLocation::Side(_) => None,
                }
            })
            .collect()
    })
    .unwrap_or_default();
    for (win, rect) in floats {
        win.setFrame_display(unflip_rect(rect, primary_h), true);
        if let Some(frame) = win.contentView().and_then(|c| nth_subview(&c, 0)) {
            if let Some(frame) = frame.downcast_ref::<GroupFrame>() {
                layout_group_frame(frame);
            }
        }
        if !win.isVisible() {
            win.orderFront(None);
        }
    }
}

// --- public entry points -------------------------------------------------------------

/// Cold-start restore of the whole arrangement: the model from
/// `Shell::restored_dock_layout` (persisted `<dock>`, else the legacy
/// migration — the precedence lives on the shell, shared by all three
/// backends), gated on the workspace panel actually having a root,
/// floats clamped back into reach of the main window, then one
/// reconcile. Runs from `run()` after the session is loaded and the
/// window geometry restored, before the window is shown, so the first
/// paint carries the arrangement.
pub(crate) fn apply_saved() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some((mut layout, saved_root, geometry)) = with_state(|st| {
        (
            st.shell.restored_dock_layout(),
            st.shell.saved_workspace_session().and_then(|w| w.root),
            st.shell.saved_window_geometry(),
        )
    }) else {
        return;
    };
    // Only reopen a root that still exists; and a layout claiming the
    // workspace visible with no root degrades to "hidden, remembered
    // where it was" rather than presenting an empty husk.
    let root = saved_root.filter(|r| r.is_dir());
    if root.is_none() && layout.is_visible(DockPanel::Workspace) {
        layout.hide(DockPanel::Workspace);
    }
    // `session.xml` is portable but plugin dock panels are not: only
    // the Win32 host accepts `NPPM_DMMREGASDCKDLG` (DESIGN.md §7.4),
    // so a layout written there names panels this backend can never
    // supply a content widget for. Dropping them here is what lets
    // `Ui::panel_content`'s `None` arm stay unreachable.
    layout.drop_plugin_panels();
    // A float rect persisted on a bigger display (or hand-edited to the
    // moon) must stay retrievable. The window has its restored frame
    // by now even though it is not yet on screen; failing that, the
    // saved geometry, then the primary screen.
    let primary_h = primary_height(mtm);
    // The window frame is always there by this point; the chain past
    // it is for a headless session, and it ends in a fixed default so
    // the clamp is unconditional — a persisted position is the one
    // float field `core::dock` leaves for the backend to bound.
    let area = with_dock(|d| flip_rect(d.main_window.frame(), primary_h))
        .filter(|r| r.w > 0 && r.h > 0)
        .or_else(|| geometry_rect(geometry, primary_h))
        .or_else(|| NSScreen::mainScreen(mtm).map(|s| flip_rect(s.frame(), primary_h)))
        .unwrap_or_else(|| DockRect::new(0, 0, 1024, 768));
    layout.clamp_floating_to_area(area);
    let workspace_visible = layout.is_visible(DockPanel::Workspace);
    with_dock(|d| d.layout = layout);
    if let Some(root) = root {
        crate::workspace::restore_root(&root, workspace_visible);
    }
    apply_layout();
}

/// Show or hide `panel` through the model and reconcile. A show on an
/// already-open panel makes it the active tab of its group. The
/// per-panel modules call this after their own preparation (the
/// workspace populates its tree first; a hide cancels its walk), and
/// **never from inside a `with_state` closure** — the reconcile that
/// follows re-lays the chrome through one of its own.
pub(crate) fn set_panel_visible(panel: DockPanel, visible: bool) {
    let changed = with_dock(|d| {
        if visible {
            d.layout.show(panel);
            true
        } else {
            let was = d.layout.is_visible(panel);
            if was {
                d.layout.hide(panel);
            }
            was
        }
    });
    if changed == Some(true) {
        apply_layout();
    }
}

/// Whether `panel` is open (docked, tabbed or floating).
pub(crate) fn is_visible(panel: DockPanel) -> bool {
    with_dock(|d| d.layout.is_visible(panel)).unwrap_or(false)
}

/// The band width to mirror into the legacy `<workspace>` /
/// `<docmap>` session fields: the size of the side the panel is
/// docked on, else its default side's stored size. Downgrade
/// tolerance only — a dock-aware build restores from `<dock>`.
pub(crate) fn legacy_band_width(panel: DockPanel) -> i32 {
    with_dock(|d| {
        let side = match d.layout.group_of(panel).map(|g| g.location) {
            Some(DockLocation::Side(s)) => s,
            _ => panel.default_side(),
        };
        d.layout.side_size(side)
    })
    .unwrap_or(0)
}

/// Push the model into the shell's session cache so the next
/// `save_session` persists it. Called after every mutation and from
/// the autosave / shutdown path.
pub(crate) fn sync_to_shell() {
    if let Some(session) = with_dock(|d| d.layout.to_session()) {
        with_state(|st| st.shell.set_dock_session(Some(session)));
    }
}

/// Drive the toolbar toggles from the model. The View menu resolves
/// its own marks in `validateMenuItem:` on open, so it needs no push.
fn sync_indicators() {
    with_state(|st| {
        st.toolbar.refresh_workspace_toggle();
        st.toolbar.refresh_docmap_toggle();
    });
}

/// Re-order every visible floating window front. Called when the
/// application becomes active: a float ordered front while the app was
/// still inactive at startup is hidden by `hidesOnDeactivate`, and this
/// is the one moment AppKit may not bring it back on its own.
pub(crate) fn order_floats_front() {
    let floats: Vec<Retained<FloatWindow>> =
        with_dock(|d| d.groups.iter().filter_map(|g| g.float.clone()).collect())
            .unwrap_or_default();
    for win in floats {
        win.orderFront(None);
    }
}

/// Close a panel from its group's caption ✕. Routes to the per-panel
/// hide path rather than mutating the model directly — the ✕ must
/// behave exactly like the View toggle's hide half (the workspace one
/// cancels an in-flight Unfold All).
fn close_panel(panel: DockPanel) {
    match panel {
        DockPanel::Workspace => crate::workspace::set_visible(false),
        DockPanel::DocMap => crate::docmap::set_visible(false),
        // Unreachable in practice: this backend never hosts one
        // (see `Ui::panel_content`), and `drop_plugin_panels` keeps
        // a restored layout from naming one. Hiding through the same
        // funnel the two built-ins reach is the safe degradation — it
        // reconciles, so a ✕ that does nothing is not left behind on
        // an empty group.
        DockPanel::Plugin(_) => set_panel_visible(panel, false),
    }
}

/// The caption ✕ of group `id`: close its active panel.
pub(crate) fn close_active_panel(id: u32) {
    let panel = with_dock(|d| d.layout.group(id).map(DockGroup::active_panel)).flatten();
    if let Some(panel) = panel {
        close_panel(panel);
    }
}

// --- gesture handlers ------------------------------------------------------------------

/// What the drag loop is tracking.
struct Drag {
    subject: DragSubject,
    group_id: u32,
    /// Flipped-screen point of the button-down.
    start: (i32, i32),
    /// Pointer offset into the float preview, so the grab point stays
    /// under the pointer while floating.
    grab: (i32, i32),
    /// Size of the float preview.
    float_size: (i32, i32),
    /// Armed tab index — a press on a tab that ends without crossing
    /// the drag threshold is a tab *switch*.
    armed_tab: Option<usize>,
}

/// What a finished gesture asks for, decided inside the loop and acted
/// on after it — see [`run_drag_loop`].
enum Outcome {
    /// Commit the drop at this flipped-screen point.
    Drop((i32, i32)),
    /// A tab press released without travel.
    Click((i32, i32)),
    /// Escape, or the queue closed under us.
    Cancelled,
}

/// Caption press: a whole-group drag. On an already-floating group the
/// grab offset keeps the pointer where the user pressed, relative to
/// the frame's own origin, so the window moves "in hand"; on a docked
/// group the float opens at [`tear_off_size`] with the pointer at the
/// same fraction along the caption ([`tear_off_grab`]). A press on a
/// floating group also raises it above its floating siblings, without
/// taking focus.
fn track_caption(id: u32, event: &NSEvent) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let primary_h = primary_height(mtm);
    let start = event_screen_point(event, mtm, primary_h);
    let Some(Some(drag)) = with_dock(|d| {
        let gi = d.group_index(id)?;
        let g = &d.groups[gi];
        if let Some(win) = &g.float {
            win.orderFront(None);
        }
        let outer = screen_rect(&g.frame, primary_h).unwrap_or_default();
        let (grab, float_size) = if g.float.is_some() {
            (
                (start.0 - outer.x, start.1 - outer.y),
                (outer.w.max(MIN_FLOAT_W), outer.h.max(MIN_FLOAT_H)),
            )
        } else {
            let main = d.main_window.frame().size;
            let size = tear_off_size((main.width as i32, main.height as i32));
            (tear_off_grab(start.0 - outer.x, outer.w, size.0), size)
        };
        Some(Drag {
            subject: DragSubject::Group(id),
            group_id: id,
            start,
            grab,
            float_size,
            armed_tab: None,
        })
    }) else {
        return;
    };
    run_drag_loop(&drag, mtm, primary_h);
}

/// Tab press: arm a tab switch that becomes a single-panel drag if the
/// pointer travels. A torn-off tab floats at the tear-off size with
/// the grab point mid-caption.
fn track_tab(group: u32, index: usize, panel: DockPanel, event: &NSEvent) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let primary_h = primary_height(mtm);
    let start = event_screen_point(event, mtm, primary_h);
    let Some(drag) = with_dock(|d| {
        let main = d.main_window.frame().size;
        let size = tear_off_size((main.width as i32, main.height as i32));
        Drag {
            subject: DragSubject::Panel(panel),
            group_id: group,
            start,
            grab: (size.0 / 2, (DOCK_CAPTION_H / 2.0) as i32),
            float_size: size,
            armed_tab: Some(index),
        }
    }) else {
        return;
    };
    run_drag_loop(&drag, mtm, primary_h);
}

/// The press-drag-release loop shared by captions and tabs.
///
/// Pulls events straight off the queue with `nextEventMatchingMask:`,
/// the standard Cocoa idiom for a self-contained drag and the same one
/// the tab strip uses: the click/drag decision is made *after* seeing
/// whether the mouse moved. Escape is read from the same queue, so a
/// cancel needs no key monitor. The hint follows the pointer; the model
/// is mutated only on release, at the release point.
///
/// A nested run loop implies a [`crate::DrainFreeze`]: GCD's main-queue
/// source is serviced in `NSEventTrackingRunLoopMode`, so a worker
/// finishing mid-drag would otherwise reach `drain_shell` and could pop
/// a modal while the mouse is held. And the decision is acted on
/// **after** the loop rather than inside it, for the reason the tab
/// strip records: the reconcile that a drop triggers rebuilds the tab
/// bar and sends `removeFromSuperview` to the very tab whose
/// `mouseDown:` is still on the stack. Nothing here touches that view
/// after the mutation.
fn run_drag_loop(drag: &Drag, mtm: MainThreadMarker, primary_h: f64) {
    let Some(hint) = with_dock(|d| d.hint.clone()) else {
        return;
    };
    let freeze = crate::DrainFreeze::new();
    let app = NSApplication::sharedApplication(mtm);
    let mut started = false;
    let mut cancelled = false;
    let mut outcome = Outcome::Cancelled;
    loop {
        let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp | NSEventMask::KeyDown;
        // SAFETY: the standard Cocoa modal-tracking call; `dequeue: true`
        // consumes the event we handle.
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
        match next.r#type() {
            NSEventType::LeftMouseUp => {
                let cursor = event_screen_point(&next, mtm, primary_h);
                outcome = if cancelled {
                    Outcome::Cancelled
                } else if started {
                    Outcome::Drop(cursor)
                } else {
                    Outcome::Click(cursor)
                };
                break;
            }
            // Esc cancels without committing; the loop still runs to
            // the mouse-up so the release is consumed here rather than
            // reaching whatever is under the pointer.
            NSEventType::KeyDown if next.keyCode() == ESCAPE_KEY_CODE => {
                cancelled = true;
                hide_hint(&hint);
            }
            NSEventType::LeftMouseDragged => {
                if cancelled {
                    continue;
                }
                let cursor = event_screen_point(&next, mtm, primary_h);
                if !started {
                    let dx = (cursor.0 - drag.start.0).abs();
                    let dy = (cursor.1 - drag.start.1).abs();
                    if dx < DOCK_DRAG_THRESHOLD && dy < DOCK_DRAG_THRESHOLD {
                        continue;
                    }
                    started = true;
                }
                let preview = float_preview(drag, cursor);
                let rect = with_dock(|d| {
                    resolve_current_drop(d, drag.subject, cursor, preview, primary_h)
                        .map(|(_, r)| r)
                })
                .flatten();
                if let Some(rect) = rect {
                    show_hint(&hint, rect, primary_h);
                }
            }
            _ => {}
        }
    }
    hide_hint(&hint);
    // The freeze goes first so the reconcile it guards is not itself
    // deferred; the flush after replaces the wake the freeze swallowed.
    drop(freeze);
    let mutated = match outcome {
        Outcome::Drop(cursor) => {
            let preview = float_preview(drag, cursor);
            with_dock(|d| {
                let (target, _) =
                    resolve_current_drop(d, drag.subject, cursor, preview, primary_h)?;
                match drag.subject {
                    DragSubject::Panel(panel) => d.layout.move_panel(panel, target),
                    DragSubject::Group(id) => d.layout.move_group(id, target),
                }
                Some(())
            })
            .flatten()
            .is_some()
        }
        Outcome::Click(cursor) => drag.armed_tab.is_some_and(|tab| {
            with_dock(|d| {
                if tab_contains(d, drag.group_id, tab, cursor, primary_h) {
                    d.layout.set_active_index(drag.group_id, tab);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
        }),
        Outcome::Cancelled => false,
    };
    if mutated {
        apply_layout();
    }
    crate::drain_shell();
}

/// The cursor-anchored float candidate for `drag` at `cursor`.
fn float_preview(drag: &Drag, cursor: (i32, i32)) -> DockRect {
    DockRect::new(
        cursor.0 - drag.grab.0,
        cursor.1 - drag.grab.1,
        drag.float_size.0,
        drag.float_size.1,
    )
}

/// Whether the flipped-screen point is over tab `index` of group `id`
/// — the "release still on what was pressed" check for a tab switch.
fn tab_contains(d: &Ui, id: u32, index: usize, cursor: (i32, i32), primary_h: f64) -> bool {
    d.group_index(id)
        .and_then(|gi| nth_subview(&d.groups[gi].tab_bar, index))
        .and_then(|tab| screen_rect(&tab, primary_h))
        .is_some_and(|r| r.contains(cursor.0, cursor.1))
}

/// Resolve the drop target + hint rect for the current pointer
/// position. Builds the `DropZones` from live view rects (floating
/// groups first — they sit above the docked ones) and delegates the
/// decision to the pure core resolver.
fn resolve_current_drop(
    d: &Ui,
    subject: DragSubject,
    cursor: (i32, i32),
    preview: DockRect,
    primary_h: f64,
) -> Option<(DropTarget, DockRect)> {
    let mid = screen_rect(&d.area, primary_h)?;
    let mut groups: Vec<(u32, DockRect)> = Vec::new();
    for g in d.groups.iter().filter(|g| g.float.is_some()) {
        if let Some(r) = screen_rect(&g.frame, primary_h) {
            groups.push((g.id, r));
        }
    }
    for g in d.groups.iter().filter(|g| g.float.is_none()) {
        if let Some(r) = screen_rect(&g.frame, primary_h) {
            groups.push((g.id, r));
        }
    }
    let zones = DropZones { mid, groups };
    Some(resolve_drop(&zones, &d.layout, subject, cursor, preview))
}

/// A floating window moved or resized: mirror the live rect into the
/// model and re-lay the group's children. Declined re-entrantly while
/// [`sync_floats`]' caller still holds the borrow — impossible by
/// construction, since that runs outside it — and otherwise idempotent.
fn on_float_configured(notification: &NSNotification) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(window) = notification
        .object()
        .and_then(|o: Retained<AnyObject>| o.downcast::<NSWindow>().ok())
    else {
        return;
    };
    let primary_h = primary_height(mtm);
    let rect = flip_rect(window.frame(), primary_h);
    with_dock(|d| {
        if let Some(id) = d.group_of_window(&window) {
            d.layout.set_floating_rect(id, rect);
        }
    });
    if let Some(frame) = window.contentView().and_then(|c| nth_subview(&c, 0)) {
        if let Some(frame) = frame.downcast_ref::<GroupFrame>() {
            layout_group_frame(frame);
        }
    }
}

// --- side splitters ------------------------------------------------------------------

/// Splitter press: a drag that resizes the band live. Computes the
/// *visually clamped* result via the same authority the layout uses,
/// then stores that — so the model's size always matches what renders
/// and a drag past the limit has no dead zone on the way back. Escape
/// puts the size back where the press found it. The new size is
/// written through on release.
fn track_splitter(side: DockSide, event: &NSEvent) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let primary_h = primary_height(mtm);
    let start = event_screen_point(event, mtm, primary_h);
    let Some(size_at_start) = with_dock(|d| d.layout.side_size(side)) else {
        return;
    };
    // Named rather than `_freeze`: dropped explicitly before the flush
    // at the end of this function.
    let freeze = crate::DrainFreeze::new();
    let app = NSApplication::sharedApplication(mtm);
    let mut cancelled = false;
    loop {
        let mask = NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp | NSEventMask::KeyDown;
        // SAFETY: as in `run_drag_loop`.
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
        match next.r#type() {
            NSEventType::LeftMouseUp => break,
            NSEventType::KeyDown if next.keyCode() == ESCAPE_KEY_CODE && !cancelled => {
                cancelled = true;
                with_dock(|d| d.layout.set_side_size(side, size_at_start));
                crate::relayout_chrome_bands();
            }
            NSEventType::LeftMouseDragged => {
                if cancelled {
                    continue;
                }
                let cursor = event_screen_point(&next, mtm, primary_h);
                let proposed = side_drag_size(
                    side,
                    size_at_start,
                    (cursor.0 - start.0, cursor.1 - start.1),
                );
                let changed = with_dock(|d| apply_splitter_size(d, side, proposed));
                if changed == Some(true) {
                    crate::relayout_chrome_bands();
                }
            }
            _ => {}
        }
    }
    if !cancelled {
        // Release writes the new size through; a plain click re-writes
        // the unchanged size, which is idempotent.
        sync_to_shell();
    }
    // The freeze goes first so the flush is not itself deferred; the
    // flush replaces the wake a worker may have made into this loop —
    // the same pair `run_drag_loop` ends with.
    drop(freeze);
    crate::drain_shell();
}

/// Store the rendered size a proposed band size would produce. Returns
/// whether the model changed.
fn apply_splitter_size(d: &mut Ui, side: DockSide, proposed: i32) -> bool {
    let (w, h) = d.area_size;
    let mut probe = d.layout.clone();
    probe.set_side_size(side, proposed);
    let frame = compute_frame(
        DockRect::new(0, 0, w, h),
        &probe,
        MIN_EDITOR_W,
        MIN_EDITOR_H,
    );
    let rendered = frame
        .bands
        .iter()
        .find(|b| b.side == side)
        .map(|b| match side {
            DockSide::Left | DockSide::Right => b.rect.w,
            DockSide::Top | DockSide::Bottom => b.rect.h,
        });
    match rendered {
        Some(rendered) if rendered != d.layout.side_size(side) => {
            d.layout.set_side_size(side, rendered);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        flip_rect, geometry_rect, group_child_rects, side_drag_size, tear_off_grab, tear_off_size,
        unflip_rect, DOCK_CAPTION_H, DOCK_TAB_BAR_H,
    };
    use codepp_core::dock::{DockRect, DockSide, MIN_FLOAT_H, MIN_FLOAT_W};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    #[test]
    fn side_drag_sign_conventions() {
        // Dragging right grows a Left band and shrinks a Right one.
        assert_eq!(side_drag_size(DockSide::Left, 200, (30, 0)), 230);
        assert_eq!(side_drag_size(DockSide::Right, 200, (30, 0)), 170);
        // Dragging down grows a Top band and shrinks a Bottom one — in
        // the flipped space every drag runs in, "down" is +y.
        assert_eq!(side_drag_size(DockSide::Top, 200, (0, 30)), 230);
        assert_eq!(side_drag_size(DockSide::Bottom, 200, (0, 30)), 170);
        // The off-axis component is ignored.
        assert_eq!(side_drag_size(DockSide::Left, 200, (0, 99)), 200);
    }

    #[test]
    fn tear_off_size_is_a_third_of_the_window_floored_at_the_minimum() {
        assert_eq!(tear_off_size((1200, 900)), (400, 300));
        assert_eq!(tear_off_size((300, 240)), (MIN_FLOAT_W, MIN_FLOAT_H));
        assert_eq!(tear_off_size((0, 0)), (MIN_FLOAT_W, MIN_FLOAT_H));
    }

    #[test]
    fn tear_off_grab_keeps_the_pointer_fraction_along_the_caption() {
        assert_eq!(
            tear_off_grab(100, 400, 200),
            (50, (DOCK_CAPTION_H / 2.0) as i32)
        );
        assert_eq!(tear_off_grab(-30, 400, 200).0, 0);
        assert_eq!(tear_off_grab(900, 400, 200).0, 200);
        assert_eq!(tear_off_grab(10, 0, 200).0, 200);
    }

    /// The flip is its own inverse, and a rect at the top of the
    /// screen in AppKit's space (high `y`) lands at `y == 0` — which is
    /// the whole reason the model can carve top-down.
    #[test]
    fn screen_flip_round_trips_and_puts_the_top_at_zero() {
        let primary_h = 900.0;
        let at_top = NSRect::new(NSPoint::new(10.0, 800.0), NSSize::new(300.0, 100.0));
        assert_eq!(flip_rect(at_top, primary_h), DockRect::new(10, 0, 300, 100));
        let back = unflip_rect(DockRect::new(10, 0, 300, 100), primary_h);
        assert!((back.origin.x - 10.0).abs() < f64::EPSILON);
        assert!((back.origin.y - 800.0).abs() < f64::EPSILON);
        assert!((back.size.height - 100.0).abs() < f64::EPSILON);
        // A rect below the primary screen's bottom (a second display
        // hanging beneath it) keeps going positive rather than wrapping.
        let below = NSRect::new(NSPoint::new(0.0, -200.0), NSSize::new(50.0, 100.0));
        assert_eq!(flip_rect(below, primary_h).y, 1000);
        // A degenerate size never produces an `NSRect` AppKit refuses.
        assert!(unflip_rect(DockRect::new(0, 0, 0, 0), primary_h).size.width >= 1.0);
    }

    /// Caption at the top, slot in the middle, tab bar at the bottom
    /// only for two or more panels — in flipped frame coordinates.
    #[test]
    fn group_children_stack_top_down_and_the_bar_needs_two_panels() {
        let (caption, slot, bar) = group_child_rects(300.0, 400.0, 1);
        assert!((caption.origin.y).abs() < f64::EPSILON);
        assert!((caption.size.height - DOCK_CAPTION_H).abs() < f64::EPSILON);
        assert!((slot.origin.y - DOCK_CAPTION_H).abs() < f64::EPSILON);
        assert!((slot.size.height - (400.0 - DOCK_CAPTION_H)).abs() < f64::EPSILON);
        assert!(bar.size.height.abs() < f64::EPSILON);

        let (_, slot, bar) = group_child_rects(300.0, 400.0, 2);
        assert!((bar.size.height - DOCK_TAB_BAR_H).abs() < f64::EPSILON);
        assert!((bar.origin.y - (400.0 - DOCK_TAB_BAR_H)).abs() < f64::EPSILON);
        assert!(
            (slot.size.height - (400.0 - DOCK_CAPTION_H - DOCK_TAB_BAR_H)).abs() < f64::EPSILON
        );
        // A frame shorter than its own chrome floors at zero rather
        // than asking `setFrame:` for a negative height.
        let (_, slot, _) = group_child_rects(300.0, 10.0, 2);
        assert!(slot.size.height.abs() < f64::EPSILON);
    }

    #[test]
    fn geometry_rect_needs_every_field_and_flips_the_origin() {
        let full = codepp_core::WindowGeometry {
            width: Some(800),
            height: Some(600),
            x: Some(10),
            y: Some(20),
            maximized: false,
        };
        // AppKit origin (10, 20) with height 600 on a 1000-high screen:
        // the top edge is at 620 from the bottom, i.e. 380 from the top.
        assert_eq!(
            geometry_rect(Some(full), 1000.0),
            Some(DockRect::new(10, 380, 800, 600))
        );
        let no_pos = codepp_core::WindowGeometry { x: None, ..full };
        assert_eq!(geometry_rect(Some(no_pos), 1000.0), None);
        assert_eq!(geometry_rect(None, 1000.0), None);
    }
}
