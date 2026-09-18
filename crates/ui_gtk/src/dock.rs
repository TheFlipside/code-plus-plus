//! GTK mechanism for the plugin-panel docking subsystem.
//!
//! All policy lives in `codepp_core::dock` (`DockLayout`,
//! `compute_frame`, `resolve_drop`) — this module only supplies what
//! a toolkit must: the group containers (caption bar + panel content +
//! bottom tab bar), the four side splitters, the grey drop-hint
//! popup, the caption/tab drag gestures, and the reconciler that makes
//! the widget tree match the model after every mutation. It is the
//! GTK counterpart of `ui_win32::dock_panels`, and the two are kept
//! the same shape on purpose so the one runner each has can stand in
//! for the other when reading.
//!
//! # Layout: a `GtkLayout`, not a tree of `GtkPaned`s
//!
//! The dock area — everything between the toolbar and the status bar —
//! is one [`gtk::Layout`] whose children are positioned by hand from
//! the rects [`compute_frame`] carves: the editor cell, each docked
//! group's frame, and the four splitters. Two properties of that
//! container make it the right one, and both were checked against
//! the GTK 3.24 source rather than assumed:
//!
//!   * `gtk_layout_allocate_child` allocates every child at its
//!     **minimum** requisition, and `gtk_widget_set_size_request`
//!     raises a widget's minimum to the requested size — so
//!     `move_` + `set_size_request` places a child at exactly the
//!     rect the model computed, which is precisely the `MoveWindow`
//!     Win32 has and `GtkFixed`-style containers otherwise lack.
//!   * `gtk_layout_get_preferred_width/height` report a minimum of
//!     **zero** regardless of the children (it is a scrollable), so
//!     sizing the editor cell to fill the area does not turn that
//!     size into the window's minimum. A `GtkFixed` would: its
//!     minimum is `max(child.x + child.min_width)`, and the window
//!     could never be shrunk below whatever it was last laid out at.
//!
//! A tree of `GtkPaned`s rebuilt per layout was the alternative, and
//! it is worse for the reason that matters most here: every rebuild
//! would reparent — and so unrealize and re-realize — the main
//! Scintilla view. With a `GtkLayout` the editor cell is placed once
//! and only ever *moved*; the one Scintilla widget this module does
//! reparent is the Document Map miniature, riding inside its panel
//! container, which Scintilla's GTK backend is written to survive
//! (`ScintillaGTK::UnRealizeThis` / `RealizeThis` recreate its
//! `GdkWindow`s and IM context).
//!
//! # Groups are rehosted, never rebuilt
//!
//! A group's outermost widget (`GroupWidget::frame`) is reparented
//! between the dock area and a floating toplevel on dock ↔ float
//! transitions; the same frame, caption and slot survive. Win32
//! recreates its group *window* on that transition because toggling
//! `WS_CHILD` on a live HWND is fragile; GTK reparenting is an ordinary
//! `remove` + `add`, so there is nothing to gain from a rebuild.
//!
//! # Nothing here is ever destroyed
//!
//! `gtk_widget_destroy` on a container disposes every descendant, and
//! `ScintillaGTK::Dispose` unparents the view's scrollbars — a
//! disposed miniature is a zombie even while `GtkUiState.docmap_sci`
//! still holds a reference to it. So this module holds **no
//! `.destroy()` call at all**, pinned by a source scan in `lib.rs`:
//! group frames leave the tree through `remove` after their panels
//! have been evacuated to the hidden parking box, and floating
//! toplevels are hidden and pooled for reuse rather than destroyed
//! (GTK keeps a toplevel alive in its own list, so dropping the last
//! Rust reference would not free it anyway). The panel *content*
//! widgets are created once in their own modules and only ever move.
//!
//! # Coordinate space
//!
//! Every drag computation runs in **root (screen) coordinates**, the
//! same choice Win32 makes and for the same reason: floating groups
//! are screen-positioned toplevels, and `resolve_drop` has to compare
//! their rects with the dock area's in one space. [`root_rect`] is the
//! single conversion, and `compute_frame` runs in the dock area's own
//! space (origin `(0, 0)`), which is what its children are placed in.
//!
//! # Borrow discipline
//!
//! The model and every widget it drives live in a module thread-local
//! ([`Ui`]) rather than on `GtkUiState`, reached through [`with_dock`].
//! The reason is the `size-allocate` handler: GTK delivers it during
//! its layout phase, and a relayout declined re-entrantly there would
//! leave the bands wherever the previous pass put them — the exact
//! blank-panel failure the Cocoa results dock documents for
//! `with_state`. Keeping the dock state separate means a live
//! `with_state` borrow never blocks a layout pass. The rule that keeps
//! the two `RefCell`s from deadlocking on each other: code holding the
//! dock borrow **never** calls `with_state` (every shell read happens
//! before the dock borrow is taken, every shell write after it is
//! dropped), while a `with_state` closure may take a brief read-only
//! dock borrow ([`is_visible`], [`legacy_band_width`]).
//!
//! The second half of that rule reaches further than it looks: a GTK
//! call made under the dock borrow can run a *signal handler*
//! synchronously, and if that handler reads the dock it is declined.
//! The one such path found by driving the app — a child's
//! `size_allocate` firing the Document Map overlay's own
//! `size-allocate`, whose refresh asks [`is_visible`] — is why
//! [`on_area_allocated`] snapshots the carve under the borrow and
//! applies it after. A decline degrades (a skipped refresh, a retried
//! layout pass), never corrupts, but "declined" is logged at `debug`
//! so a new instance is findable.

use std::cell::RefCell;
use std::io::Cursor;

use codepp_core::dock::{
    compute_frame, resolve_drop, DockGroup, DockLayout, DockLocation, DockPanel, DockRect,
    DockSide, DragSubject, DropTarget, DropZones, DEFAULT_FLOAT_H, DEFAULT_FLOAT_W, MIN_FLOAT_H,
    MIN_FLOAT_W,
};
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib;
use gtk::prelude::*;

use crate::state::with_state;

/// Height of a group's caption bar (the drag bar). Same value as
/// Win32's `DOCK_CAPTION_H`, so the two backends carve identically.
pub(crate) const DOCK_CAPTION_H: i32 = 22;
/// Height of the bottom tab bar, shown only when a group holds two
/// or more panels. Same value as Win32's `DOCK_TAB_BAR_H`.
pub(crate) const DOCK_TAB_BAR_H: i32 = 26;
/// Icon display size on a tab (logical pixels; the `@2x` art is used
/// on high-DPI screens so the glyph stays sharp).
const DOCK_TAB_ICON_PX: i32 = 16;
/// Horizontal padding inside a tab, either side of its content.
const DOCK_TAB_PAD: i32 = 8;
/// Gap between a tab's icon and its label (active tabs only).
const DOCK_TAB_ICON_GAP: i32 = 5;
/// Pixels of pointer travel before a pressed caption/tab becomes a
/// drag rather than a click. Same 4-px convention as the tab strip.
const DOCK_DRAG_THRESHOLD: i32 = 4;
/// Alpha of the drop-hint popup on a compositing screen (`110/255`,
/// the same value Win32's layered hint uses). Opaque grey without a
/// compositor.
const DOCK_HINT_ALPHA: f64 = 110.0 / 255.0;
/// Width of the resize border around a floating group. Floating
/// toplevels are undecorated (the caption is ours, so a drag on it can
/// re-dock), which means no window-manager frame to resize by; a
/// press inside this margin starts a WM resize drag instead.
const FLOAT_RESIZE_BORDER: i32 = 6;
/// Editor-cell minimums handed to [`compute_frame`] — the same floors
/// `ui_win32` reserves (`MIN_SCINTILLA_WIDTH_PX` / `_HEIGHT_PX`).
const MIN_EDITOR_W: i32 = 200;
const MIN_EDITOR_H: i32 = 60;

/// CSS classes for the dock chrome. Class selectors rather than
/// element selectors so nothing else in the window picks them up.
const CSS_CAPTION: &str = "codepp-dock-caption";
const CSS_TAB_BAR: &str = "codepp-dock-tab-bar";
const CSS_TAB: &str = "codepp-dock-tab";
const CSS_TAB_ACTIVE: &str = "codepp-dock-tab-active";
const CSS_SPLITTER: &str = "codepp-dock-splitter";
const CSS_FLOAT_FRAME: &str = "codepp-dock-float-frame";

/// The chrome's stylesheet. Theme colour names (`@theme_bg_color`,
/// `@borders`, …) are the ones Adwaita and every major GTK 3 theme
/// define; a theme without them fails the parse, which is logged and
/// leaves the chrome unstyled rather than the app unusable.
const DOCK_CSS: &str = "\
.codepp-dock-caption, .codepp-dock-tab-bar, .codepp-dock-float-frame {
    background-color: shade(@theme_bg_color, 0.92);
}
.codepp-dock-tab-active { background-color: @theme_base_color; }
.codepp-dock-splitter { background-color: @borders; }
";

// --- state ---------------------------------------------------------------------

/// One live group container: the model group's id and its widgets.
/// The id is the identity (§7.4's key-on-ids rule); the widgets are
/// rehosted, never remade, on dock ↔ float transitions.
struct GroupWidget {
    id: u32,
    /// The outermost widget — the one reparented between the dock
    /// area and a floating toplevel. An `EventBox` so a press on its
    /// resize border (floating only) can start a WM resize drag.
    frame: gtk::EventBox,
    /// The caption's title label (the active panel's title).
    title: gtk::Label,
    /// Holds every panel of the group; only the active one is shown.
    slot: gtk::Box,
    /// The bottom tab bar, shown only for two or more panels.
    tab_bar: gtk::Box,
    /// The floating toplevel hosting `frame`, when floating.
    float: Option<gtk::Window>,
}

/// In-flight caption/tab gesture. Cleared on release or cancel.
struct Drag {
    /// What a completed drag will move.
    subject: DragSubject,
    /// The group the gesture started on.
    group_id: u32,
    /// Root point of the button-down.
    start: (i32, i32),
    /// Pointer offset into the float preview, so the grab point stays
    /// under the pointer while floating.
    grab: (i32, i32),
    /// Size of the float preview (the group's current outer size, so
    /// "tear off" keeps the panel the size the user knows).
    float_size: (i32, i32),
    /// Armed tab index — a press on a tab that ends without crossing
    /// the drag threshold is a tab *switch*.
    armed_tab: Option<usize>,
    /// Crossed the drag threshold: the hint is up and release commits
    /// a move.
    started: bool,
    /// Esc was pressed mid-drag: release does nothing.
    cancelled: bool,
}

/// In-flight side-splitter drag.
#[derive(Copy, Clone)]
struct SplitterDrag {
    side: DockSide,
    start: (i32, i32),
    size_at_start: i32,
}

/// Everything the dock mechanism owns for the window's lifetime.
struct Ui {
    /// The model. Synced to the shell's session cache by
    /// [`sync_to_shell`] after every mutation.
    layout: DockLayout,
    main_window: gtk::Window,
    /// The dock area; see the module docs for why a `GtkLayout`.
    area: gtk::Layout,
    /// Tab strip + editor + FIF dock, placed at `DockFrame::editor`.
    editor_cell: gtk::Widget,
    /// Hidden home for panel content that is not currently shown, so
    /// a hidden panel's widgets stay under the main window's toplevel
    /// and a dismantled group can never take them with it.
    parking: gtk::Box,
    /// The two panels' content widgets, created once in their own
    /// modules. Never destroyed; only moved between `parking` and a
    /// group's slot.
    workspace_content: gtk::Widget,
    docmap_content: gtk::Widget,
    groups: Vec<GroupWidget>,
    /// Side splitters, indexed by [`side_index`].
    splitters: [gtk::EventBox; 4],
    /// The translucent grey drop preview.
    hint: gtk::Window,
    /// Hidden floating toplevels awaiting reuse. See the module docs
    /// for why they are pooled rather than destroyed.
    float_pool: Vec<gtk::Window>,
    drag: Option<Drag>,
    splitter_drag: Option<SplitterDrag>,
    /// The dock area's last allocation — the `mid` rect every layout
    /// pass carves.
    area_size: (i32, i32),
}

impl Ui {
    /// The content widget for `panel`. A field per panel rather than a
    /// map, so adding a `DockPanel` variant is a compile error here —
    /// which is what makes "every panel is hosted" hold by
    /// construction rather than by a test remembering to check.
    fn panel_content(&self, panel: DockPanel) -> gtk::Widget {
        match panel {
            DockPanel::Workspace => self.workspace_content.clone(),
            DockPanel::DocMap => self.docmap_content.clone(),
        }
    }

    fn group_index(&self, id: u32) -> Option<usize> {
        self.groups.iter().position(|g| g.id == id)
    }
}

thread_local! {
    /// Installed once on the main thread by [`install`].
    static DOCK: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Run `f` against the dock state if it is installed and not already
/// borrowed. `None` in either case — a re-entrant call is logged, and
/// the one caller for which a missed call matters (the area's
/// `size-allocate`) retries from an idle.
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
/// left *grows* the band). Same arithmetic as Win32's.
#[must_use]
fn side_drag_size(side: DockSide, size_at_start: i32, delta: (i32, i32)) -> i32 {
    match side {
        DockSide::Left => size_at_start + delta.0,
        DockSide::Right => size_at_start - delta.0,
        DockSide::Top => size_at_start + delta.1,
        DockSide::Bottom => size_at_start - delta.1,
    }
}

/// Which window edge a press at `(x, y)` inside a `w`×`h` floating
/// frame should resize from, given a `border`-wide margin; `None` for
/// a press inside the body. Corners take priority so a press near
/// one resizes on both axes.
#[must_use]
fn resize_edge(x: i32, y: i32, w: i32, h: i32, border: i32) -> Option<gdk::WindowEdge> {
    let left = x < border;
    let right = x >= w - border;
    let top = y < border;
    let bottom = y >= h - border;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(gdk::WindowEdge::NorthWest),
        (_, true, true, _) => Some(gdk::WindowEdge::NorthEast),
        (true, _, _, true) => Some(gdk::WindowEdge::SouthWest),
        (_, true, _, true) => Some(gdk::WindowEdge::SouthEast),
        (true, _, _, _) => Some(gdk::WindowEdge::West),
        (_, true, _, _) => Some(gdk::WindowEdge::East),
        (_, _, true, _) => Some(gdk::WindowEdge::North),
        (_, _, _, true) => Some(gdk::WindowEdge::South),
        _ => None,
    }
}

/// A `WindowGeometry` as a rect, when every field is present.
#[must_use]
fn geometry_rect(geometry: Option<codepp_core::WindowGeometry>) -> Option<DockRect> {
    let g = geometry?;
    Some(DockRect::new(g.x?, g.y?, g.width?, g.height?))
}

// --- root-coordinate helpers -----------------------------------------------------

/// `widget`'s on-screen rect in root coordinates, or `None` while it
/// is not inside a realized toplevel. `translate_coordinates` handles
/// the windowed/non-windowed ancestor distinction, so this is correct
/// for a child of the dock area and for a floating frame alike.
fn root_rect(widget: &impl IsA<gtk::Widget>) -> Option<DockRect> {
    let top = widget.toplevel()?;
    let (x, y) = widget.translate_coordinates(&top, 0, 0)?;
    let (_, ox, oy) = top.window()?.origin();
    let a = widget.allocation();
    Some(DockRect::new(ox + x, oy + y, a.width(), a.height()))
}

/// A GDK root position as integers.
fn root_i32(root: (f64, f64)) -> (i32, i32) {
    (root.0 as i32, root.1 as i32)
}

/// True iff `w`'s parent is `container`.
fn is_child_of(w: &gtk::Widget, container: &impl IsA<gtk::Widget>) -> bool {
    w.parent()
        .is_some_and(|p| p == *container.upcast_ref::<gtk::Widget>())
}

/// Take `w` out of whatever container holds it (a no-op when it has no
/// parent). **`remove`, never `destroy`** — see the module docs.
fn unparent(w: &gtk::Widget) {
    if let Some(parent) = w.parent() {
        if let Some(container) = parent.downcast_ref::<gtk::Container>() {
            container.remove(w);
        }
    }
}

// --- installation ------------------------------------------------------------------

/// Build the dock chrome around `area` and publish the state. Called
/// once from `run()`, after the two panel content widgets exist and
/// before the main window is shown. `editor_cell` must already be a
/// child of `area`.
pub(crate) fn install(
    main_window: &gtk::Window,
    area: &gtk::Layout,
    editor_cell: &gtk::Widget,
    workspace_content: &gtk::Widget,
    docmap_content: &gtk::Widget,
) {
    install_css(main_window);

    let parking = gtk::Box::new(gtk::Orientation::Vertical, 0);
    parking.set_no_show_all(true);
    parking.hide();
    area.put(&parking, 0, 0);
    parking.add(workspace_content);
    parking.add(docmap_content);

    let splitters = DockSide::ALL.map(|side| build_splitter(area, side));
    let hint = build_hint();

    area.connect_size_allocate(|_, alloc| {
        crate::at_callback_boundary("dock:area:size_allocate", (), || {
            on_area_allocated(alloc.width(), alloc.height());
        });
    });
    // Esc cancels a live caption/tab drag. Key events go to the
    // focused toplevel, which during a drag from the main window is
    // the main window; floating toplevels get the same handler.
    main_window.connect_key_press_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:main:key_press_event",
            glib::Propagation::Proceed,
            || on_key_press(ev),
        )
    });

    let ui = Ui {
        layout: DockLayout::new(),
        main_window: main_window.clone(),
        area: area.clone(),
        editor_cell: editor_cell.clone(),
        parking,
        workspace_content: workspace_content.clone(),
        docmap_content: docmap_content.clone(),
        groups: Vec::new(),
        splitters,
        hint,
        float_pool: Vec::new(),
        drag: None,
        splitter_drag: None,
        area_size: (0, 0),
    };
    DOCK.with(|d| *d.borrow_mut() = Some(ui));
}

/// Attach [`DOCK_CSS`] to the screen. A parse failure is cosmetic and
/// logged, never fatal.
fn install_css(main_window: &gtk::Window) {
    let provider = gtk::CssProvider::new();
    if let Err(err) = provider.load_from_data(DOCK_CSS.as_bytes()) {
        tracing::warn!(?err, "dock: chrome CSS failed to parse");
        return;
    }
    if let Some(screen) = gtk::prelude::GtkWindowExt::screen(main_window) {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// One hidden side splitter, placed in the area and shown only while
/// its side carries a band.
fn build_splitter(area: &gtk::Layout, side: DockSide) -> gtk::EventBox {
    let splitter = gtk::EventBox::new();
    splitter.style_context().add_class(CSS_SPLITTER);
    splitter.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK,
    );
    splitter.connect_realize(move |w| {
        crate::at_callback_boundary("dock:splitter:realize", (), || set_resize_cursor(w, side));
    });
    splitter.connect_button_press_event(move |_, ev| {
        crate::at_callback_boundary(
            "dock:splitter:button_press_event",
            glib::Propagation::Proceed,
            || on_splitter_press(side, ev),
        )
    });
    splitter.connect_motion_notify_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:splitter:motion_notify_event",
            glib::Propagation::Proceed,
            || on_splitter_motion(ev),
        )
    });
    splitter.connect_button_release_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:splitter:button_release_event",
            glib::Propagation::Proceed,
            || on_splitter_release(ev),
        )
    });
    splitter.connect_grab_broken_event(|_, _| {
        crate::at_callback_boundary(
            "dock:splitter:grab_broken_event",
            glib::Propagation::Proceed,
            on_grab_broken,
        )
    });
    splitter.set_no_show_all(true);
    splitter.hide();
    area.put(&splitter, 0, 0);
    splitter
}

/// Give a realized splitter the resize cursor for its axis.
fn set_resize_cursor(w: &gtk::EventBox, side: DockSide) {
    let name = match side {
        DockSide::Left | DockSide::Right => "col-resize",
        DockSide::Top | DockSide::Bottom => "row-resize",
    };
    if let (Some(window), Some(display)) = (w.window(), gdk::Display::default()) {
        window.set_cursor(gdk::Cursor::from_name(&display, name).as_ref());
    }
}

/// The drop-hint popup: an override-redirect window painted flat grey,
/// translucent where the screen composites. Shown/moved by
/// [`show_hint`] while a drag is live; never focused.
fn build_hint() -> gtk::Window {
    let hint = gtk::Window::new(gtk::WindowType::Popup);
    hint.set_app_paintable(true);
    hint.set_accept_focus(false);
    let translucent = gtk::prelude::GtkWindowExt::screen(&hint).is_some_and(|screen| match screen
        .rgba_visual()
    {
        Some(visual) if screen.is_composited() => {
            hint.set_visual(Some(&visual));
            true
        }
        _ => false,
    });
    hint.connect_draw(move |_, cr| {
        crate::at_callback_boundary("dock:hint:draw", glib::Propagation::Proceed, || {
            paint_hint(cr, translucent)
        })
    });
    hint
}

fn paint_hint(cr: &gtk::cairo::Context, translucent: bool) -> glib::Propagation {
    let alpha = if translucent { DOCK_HINT_ALPHA } else { 1.0 };
    cr.set_source_rgba(0.5, 0.5, 0.5, alpha);
    cr.set_operator(gtk::cairo::Operator::Source);
    if let Err(err) = cr.paint() {
        tracing::warn!(?err, "dock: hint paint failed");
    }
    glib::Propagation::Stop
}

fn show_hint(hint: &gtk::Window, rect: DockRect) {
    hint.move_(rect.x, rect.y);
    hint.resize(rect.w.max(1), rect.h.max(1));
    if !hint.is_visible() {
        hint.show();
    }
    hint.queue_draw();
}

// --- group chrome ----------------------------------------------------------------------

/// Build a group container. Its slot and tab bar carry
/// `no_show_all`, because their children's visibility is this
/// module's to manage — a `show_all` on the main window must not
/// reveal an inactive tab's content.
fn build_group(id: u32) -> GroupWidget {
    let frame = gtk::EventBox::new();
    frame.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
    frame.connect_button_press_event(move |f, ev| {
        crate::at_callback_boundary(
            "dock:group:frame:button_press_event",
            glib::Propagation::Proceed,
            || on_frame_press(id, f, ev),
        )
    });

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let (caption, title) = build_caption(id);
    outer.pack_start(&caption, false, false, 0);

    let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    slot.set_no_show_all(true);
    outer.pack_start(&slot, true, true, 0);

    let tab_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_bar.set_size_request(-1, DOCK_TAB_BAR_H);
    tab_bar.style_context().add_class(CSS_TAB_BAR);
    tab_bar.set_no_show_all(true);
    outer.pack_start(&tab_bar, false, false, 0);

    frame.add(&outer);
    frame.show_all();
    slot.show();

    GroupWidget {
        id,
        frame,
        title,
        slot,
        tab_bar,
        float: None,
    }
}

/// The caption bar: title label, close ✕, and the drag gesture.
fn build_caption(id: u32) -> (gtk::EventBox, gtk::Label) {
    let caption = gtk::EventBox::new();
    caption.set_size_request(-1, DOCK_CAPTION_H);
    caption.style_context().add_class(CSS_CAPTION);
    caption.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK,
    );
    caption.connect_button_press_event(move |_, ev| {
        crate::at_callback_boundary(
            "dock:caption:button_press_event",
            glib::Propagation::Proceed,
            || on_caption_press(id, ev),
        )
    });
    caption.connect_motion_notify_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:caption:motion_notify_event",
            glib::Propagation::Proceed,
            || on_drag_motion(ev),
        )
    });
    caption.connect_button_release_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:caption:button_release_event",
            glib::Propagation::Proceed,
            || on_drag_release(ev),
        )
    });
    caption.connect_grab_broken_event(|_, _| {
        crate::at_callback_boundary(
            "dock:caption:grab_broken_event",
            glib::Propagation::Proceed,
            on_grab_broken,
        )
    });

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    row.set_margin_start(6);
    row.set_margin_end(2);
    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.pack_start(&title, true, true, 0);
    // The ✕ is a real button with its own event window, so a press on
    // it never reaches the caption's drag handler.
    let close = gtk::Button::with_label("\u{2715}");
    close.set_relief(gtk::ReliefStyle::None);
    close.set_can_focus(false);
    WidgetExt::set_tooltip_text(&close, Some("Close panel"));
    crate::workspace::apply_compact_button_css(&close);
    close.connect_clicked(move |_| {
        crate::at_callback_boundary("dock:caption:close:clicked", (), || close_active_panel(id));
    });
    row.pack_end(&close, false, false, 0);
    caption.add(&row);
    (caption, title)
}

/// One tab of a multi-panel group: icon, plus the label when active.
fn build_tab(id: u32, index: usize, panel: DockPanel, active: bool, scale: i32) -> gtk::EventBox {
    let tab = gtk::EventBox::new();
    tab.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK,
    );
    tab.style_context().add_class(CSS_TAB);
    if active {
        tab.style_context().add_class(CSS_TAB_ACTIVE);
    }
    WidgetExt::set_tooltip_text(&tab, Some(panel.title()));

    let row = gtk::Box::new(gtk::Orientation::Horizontal, DOCK_TAB_ICON_GAP);
    row.set_margin_start(DOCK_TAB_PAD);
    row.set_margin_end(DOCK_TAB_PAD);
    if let Some(icon) = panel_icon(panel, scale) {
        row.pack_start(&icon, false, false, 0);
    }
    if active {
        // Ellipsized so a narrow band shows "Folder as W…" beside the
        // other tabs' icons rather than pushing them off the bar; the
        // Box hands the label its natural width whenever there is room.
        let label = gtk::Label::new(Some(panel.title()));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        row.pack_start(&label, false, false, 0);
    }
    tab.add(&row);

    tab.connect_button_press_event(move |_, ev| {
        crate::at_callback_boundary(
            "dock:tab:button_press_event",
            glib::Propagation::Proceed,
            || on_tab_press(id, index, panel, ev),
        )
    });
    tab.connect_motion_notify_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:tab:motion_notify_event",
            glib::Propagation::Proceed,
            || on_drag_motion(ev),
        )
    });
    tab.connect_button_release_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:tab:button_release_event",
            glib::Propagation::Proceed,
            || on_drag_release(ev),
        )
    });
    tab.connect_grab_broken_event(|_, _| {
        crate::at_callback_boundary(
            "dock:tab:grab_broken_event",
            glib::Propagation::Proceed,
            on_grab_broken,
        )
    });
    tab.show_all();
    tab
}

/// The tab-bar icon for `panel` — the same quick-action-bar art the
/// Win32 tabs blit, decoded at the window's scale so it stays sharp on
/// high-DPI screens. `None` on a decode failure (cosmetic; the tab keeps its
/// label and tooltip).
fn panel_icon(panel: DockPanel, scale: i32) -> Option<gtk::Image> {
    let (at_1x, at_2x): (&[u8], &[u8]) = match panel {
        DockPanel::Workspace => (
            include_bytes!("../../../assets/icons/folder-workspace.png"),
            include_bytes!("../../../assets/icons/folder-workspace@2x.png"),
        ),
        DockPanel::DocMap => (
            include_bytes!("../../../assets/icons/document-map.png"),
            include_bytes!("../../../assets/icons/document-map@2x.png"),
        ),
    };
    let bytes = if scale >= 2 { at_2x } else { at_1x };
    let pixbuf = match Pixbuf::from_read(Cursor::new(bytes)) {
        Ok(pixbuf) => pixbuf,
        Err(err) => {
            tracing::warn!(?err, ?panel, "dock: tab icon decode failed");
            return None;
        }
    };
    let px = DOCK_TAB_ICON_PX * scale.max(1);
    let scaled = pixbuf.scale_simple(px, px, gtk::gdk_pixbuf::InterpType::Bilinear)?;
    let surface = scaled.create_surface(scale.max(1), None::<&gdk::Window>)?;
    Some(gtk::Image::from_surface(Some(&surface)))
}

/// A floating toplevel for a group. Undecorated — the caption is the
/// group's own, so a drag on it can re-dock — transient for the main
/// window so it stays above it, and never focused on map so a session
/// restore does not steal the editor's focus. Pooled after use.
fn build_float_window(main: &gtk::Window) -> gtk::Window {
    let win = gtk::Window::new(gtk::WindowType::Toplevel);
    win.set_transient_for(Some(main));
    win.set_type_hint(gdk::WindowTypeHint::Utility);
    win.set_decorated(false);
    win.set_skip_taskbar_hint(true);
    win.set_skip_pager_hint(true);
    win.set_focus_on_map(false);
    win.set_title("Code++");
    win.connect_configure_event(|w, _| {
        crate::at_callback_boundary("dock:float:configure_event", false, || {
            on_float_configured(w)
        })
    });
    // A WM close request (Alt+F4 on the float) hides the active panel,
    // exactly like the caption ✕; the toplevel itself is kept (pooled).
    win.connect_delete_event(|w, _| {
        crate::at_callback_boundary("dock:float:delete_event", glib::Propagation::Stop, || {
            on_float_delete(w)
        })
    });
    win.connect_key_press_event(|_, ev| {
        crate::at_callback_boundary(
            "dock:float:key_press_event",
            glib::Propagation::Proceed,
            || on_key_press(ev),
        )
    });
    win
}

// --- reconciler ----------------------------------------------------------------------

/// Make the widget tree match the model, relayout, and refresh every
/// indicator. The single funnel every mutation goes through —
/// show/hide, drops, tab switches, restore.
///
/// Two phases: the first holds the dock borrow and touches only
/// widgets; the second, with that borrow dropped, does the shell-side
/// work (`with_state`) — see the module docs for why the two never
/// nest in that direction.
pub(crate) fn apply_layout() {
    let docmap_visible = with_dock(|d| {
        reconcile(d);
        relayout(d);
        d.layout.is_visible(DockPanel::DocMap)
    });
    // A freshly shown Document Map needs its doc binding + the
    // viewport box caught up (it skips both while hidden).
    if docmap_visible == Some(true) {
        crate::docmap::sync_to_active_tab();
    }
    sync_indicators();
    sync_to_shell();
}

/// Phase one of [`apply_layout`]: widgets only.
fn reconcile(d: &mut Ui) {
    let layout = d.layout.clone();

    // 1. Groups gone from the model lose their widgets — panels are
    //    evacuated to parking first, so nothing of ours is ever
    //    inside a container on its way out.
    let mut i = 0;
    while i < d.groups.len() {
        if layout.group(d.groups[i].id).is_some() {
            i += 1;
        } else {
            let gone = d.groups.remove(i);
            dismantle_group(d, gone);
        }
    }

    // 2. New model groups get widgets.
    for group in layout.groups() {
        if d.group_index(group.id).is_none() {
            d.groups.push(build_group(group.id));
        }
    }

    // 3. Host each group where the model says, then fill it.
    for group in layout.groups() {
        let Some(gi) = d.group_index(group.id) else {
            continue;
        };
        rehost_group(d, gi, group.location);
        fill_group(d, gi, group);
    }

    // 4. Hidden panels go to parking.
    for panel in DockPanel::ALL {
        if !layout.is_visible(panel) {
            park(d, panel);
        }
    }

    // 5. Splitters follow their side's occupancy.
    for side in DockSide::ALL {
        let occupied = layout.groups_on(side).next().is_some();
        d.splitters[side_index(side)].set_visible(occupied);
    }
}

/// Retire a group's widgets: panels back to parking, the frame out of
/// wherever it is hosted, a floating toplevel back to the pool. The
/// `GroupWidget` drops at the end holding nothing of ours.
fn dismantle_group(d: &mut Ui, mut g: GroupWidget) {
    for child in g.slot.children() {
        g.slot.remove(&child);
        d.parking.add(&child);
    }
    if let Some(win) = g.float.take() {
        win.remove(&g.frame);
        win.hide();
        d.float_pool.push(win);
    } else if g.frame.parent().is_some() {
        d.area.remove(&g.frame);
    }
}

/// Put group `gi`'s frame in the dock area or a floating toplevel to
/// match `location`, and position a float.
fn rehost_group(d: &mut Ui, gi: usize, location: DockLocation) {
    let area = d.area.clone();
    let main = d.main_window.clone();
    match location {
        DockLocation::Side(_) => {
            let g = &mut d.groups[gi];
            if let Some(win) = g.float.take() {
                win.remove(&g.frame);
                win.hide();
                d.float_pool.push(win);
                set_float_margin(&g.frame, 0);
                g.frame.style_context().remove_class(CSS_FLOAT_FRAME);
            }
            if g.frame.parent().is_none() {
                area.put(&g.frame, 0, 0);
            }
        }
        DockLocation::Floating(rect) => {
            let pooled = d.float_pool.pop();
            let g = &mut d.groups[gi];
            if g.float.is_none() {
                if g.frame.parent().is_some() {
                    area.remove(&g.frame);
                }
                let win = pooled.unwrap_or_else(|| build_float_window(&main));
                set_float_margin(&g.frame, FLOAT_RESIZE_BORDER);
                g.frame.style_context().add_class(CSS_FLOAT_FRAME);
                win.add(&g.frame);
                g.float = Some(win);
            } else if let Some(unused) = pooled {
                d.float_pool.push(unused);
            }
            if let Some(win) = &d.groups[gi].float {
                win.move_(rect.x, rect.y);
                win.resize(rect.w.max(1), rect.h.max(1));
                if !win.is_visible() {
                    win.show();
                }
            }
        }
    }
}

/// Inset a group's inner box by `px` on every side, leaving that much
/// bare frame around it for the floating resize border.
///
/// Deliberately **not** `GtkContainer::set_border_width` on the frame:
/// a `GtkEventBox` creates its event window *inside* its border, so a
/// press on the border would go to the toplevel behind it and never
/// reach [`on_frame_press`] — measured: the first version used the
/// border width and a drag on the edge did nothing. Margins on the
/// child keep the whole frame under the event box's own window.
fn set_float_margin(frame: &gtk::EventBox, px: i32) {
    if let Some(inner) = frame.child() {
        inner.set_margin_start(px);
        inner.set_margin_end(px);
        inner.set_margin_top(px);
        inner.set_margin_bottom(px);
    }
}

/// Move `group`'s panels into its slot, show the active one, set the
/// caption, rebuild the tab bar.
fn fill_group(d: &mut Ui, gi: usize, group: &DockGroup) {
    let contents: Vec<gtk::Widget> = group.panels.iter().map(|p| d.panel_content(*p)).collect();
    // Read live rather than cached at install: a float dragged to a
    // monitor with another scale factor gets its tab icons re-decoded
    // at the next reconcile.
    let scale = d.main_window.scale_factor();
    let g = &mut d.groups[gi];
    for (i, content) in contents.iter().enumerate() {
        if !is_child_of(content, &g.slot) {
            unparent(content);
            g.slot.pack_start(content, true, true, 0);
        }
        content.set_visible(i == group.active);
    }
    let title = group.active_panel().title();
    g.title.set_text(title);
    if let Some(win) = &g.float {
        win.set_title(title);
    }
    rebuild_tab_bar(g, group, scale);
}

/// Rebuild a group's tab bar from scratch — a handful of small widgets,
/// far cheaper than diffing, and it runs only on a dock mutation.
fn rebuild_tab_bar(g: &mut GroupWidget, group: &DockGroup, scale: i32) {
    for child in g.tab_bar.children() {
        g.tab_bar.remove(&child);
    }
    if group.panels.len() < 2 {
        g.tab_bar.hide();
        return;
    }
    for (i, panel) in group.panels.iter().enumerate() {
        let tab = build_tab(g.id, i, *panel, i == group.active, scale);
        g.tab_bar.pack_start(&tab, false, false, 0);
    }
    g.tab_bar.show();
}

/// Send a hidden panel's content to parking.
fn park(d: &mut Ui, panel: DockPanel) {
    let content = d.panel_content(panel);
    if !is_child_of(&content, &d.parking) {
        unparent(&content);
        d.parking.add(&content);
    }
}

/// The rects every child of the dock area should occupy, from a fresh
/// carve of its current allocation: the editor cell, each docked
/// group's frame, and the splitter of every occupied side. Computed
/// under the dock borrow and *applied* outside it — see
/// [`on_area_allocated`].
fn placements(d: &Ui) -> Vec<(gtk::Widget, DockRect)> {
    let (w, h) = d.area_size;
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let frame = compute_frame(
        DockRect::new(0, 0, w, h),
        &d.layout,
        MIN_EDITOR_W,
        MIN_EDITOR_H,
    );
    let mut out = vec![(d.editor_cell.clone(), frame.editor)];
    for band in &frame.bands {
        out.push((
            d.splitters[side_index(band.side)].clone().upcast(),
            band.splitter,
        ));
        for (gid, rect) in &band.groups {
            if let Some(g) = d.groups.iter().find(|g| g.id == *gid && g.float.is_none()) {
                out.push((g.frame.clone().upcast(), *rect));
            }
        }
    }
    out
}

/// Place one child of the dock area at `r`. Child allocations in a
/// `GtkLayout` are relative to its own `GdkWindow`, so the rect is
/// used as computed.
fn allocate(area: &gtk::Layout, w: &impl IsA<gtk::Widget>, r: DockRect) {
    let (width, height) = (r.w.max(1), r.h.max(1));
    area.move_(w, r.x, r.y);
    w.set_size_request(width, height);
    w.size_allocate(&gtk::Allocation::new(r.x, r.y, width, height));
}

/// Ask for a fresh layout pass over the dock area; the carve happens
/// in [`allocate_children`] when GTK gets there.
fn relayout(d: &Ui) {
    d.area.queue_resize();
}

/// `size-allocate` on the dock area: carve and place the children,
/// from inside the pass GTK is already running.
///
/// **Why it allocates directly rather than only requesting.** A first
/// version did `move_` + `set_size_request` here and relied on GTK to
/// run another pass; measured on GTK 3.24, a resize queued from
/// inside `size-allocate` re-allocates only the *children* that
/// queued it, at the size they already had, and never re-runs
/// `gtk_layout_size_allocate` — so nothing moved and every child sat
/// at its minimum size in the corner. Allocating the children here,
/// with `WidgetExt::size_allocate`, is what `gtk_layout_size_allocate`
/// itself does and needs no second pass. The request is still set so
/// the class handler's own allocation agrees with ours in steady
/// state, which keeps each child at one real allocation per pass.
///
/// **Why the allocation happens outside the dock borrow.** A child's
/// `size_allocate` runs its own handlers synchronously — the Document
/// Map overlay's, which reads the dock through [`is_visible`] — so
/// allocating under the borrow made that read decline and the
/// viewport box skip a re-centre (measured: the declined call's
/// backtrace ran through `allocate` → `size_allocate_trampoline` →
/// `docmap::refresh` → `is_visible`). The carve is snapshotted under
/// the borrow and applied after it drops.
///
/// A declined (re-entrant) carve asks for another pass from an idle
/// rather than being dropped, since a missed pass leaves the bands
/// laid out for the previous size until something else moves.
fn on_area_allocated(w: i32, h: i32) {
    let plan = with_dock(|d| {
        d.area_size = (w, h);
        (d.area.clone(), placements(d))
    });
    match plan {
        Some((area, plan)) => {
            for (widget, rect) in &plan {
                allocate(&area, widget, *rect);
            }
        }
        None => {
            glib::idle_add_local_once(|| {
                crate::at_callback_boundary("dock:area:relayout_retry", (), || {
                    with_dock(|d| relayout(d));
                });
            });
        }
    }
}

// --- public entry points -------------------------------------------------------------

/// Cold-start restore of the whole arrangement: the model from
/// `Shell::restored_dock_layout` (persisted `<dock>`, else the legacy
/// migration — the precedence lives on the shell, shared by all three
/// backends), gated on the workspace panel actually having a root,
/// floats clamped back into reach of the main window, then one
/// reconcile. Runs from `run()` after the session is loaded and before
/// the window is shown, so the first paint carries the arrangement.
pub(crate) fn apply_saved() {
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
    // A float rect persisted on a bigger display (or hand-edited to
    // the moon) must stay retrievable. The main window is not realized
    // yet at this point, so the area is its saved geometry, falling
    // back to the primary monitor.
    let area = with_dock(|d| root_rect(&d.main_window))
        .flatten()
        .or_else(|| geometry_rect(geometry))
        .or_else(primary_monitor_rect);
    if let Some(area) = area {
        layout.clamp_floating_to_area(area);
    }
    let workspace_visible = layout.is_visible(DockPanel::Workspace);
    with_dock(|d| d.layout = layout);
    if let Some(root) = root {
        crate::workspace::restore_root(&root, workspace_visible);
    }
    apply_layout();
}

fn primary_monitor_rect() -> Option<DockRect> {
    let display = gdk::Display::default()?;
    let monitor = display.primary_monitor().or_else(|| display.monitor(0))?;
    let r = monitor.geometry();
    Some(DockRect::new(r.x(), r.y(), r.width(), r.height()))
}

/// Show or hide `panel` through the model and reconcile. A show on an
/// already-open panel makes it the active tab of its group. The
/// per-panel modules call this after their own preparation (the
/// workspace populates its tree first; a hide cancels its walk).
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

/// Drive the View-menu checks and toolbar toggles from the model.
fn sync_indicators() {
    if let Some((workspace, docmap)) = with_dock(|d| {
        (
            d.layout.is_visible(DockPanel::Workspace),
            d.layout.is_visible(DockPanel::DocMap),
        )
    }) {
        crate::workspace::sync_indicators(workspace);
        crate::docmap::sync_indicators(docmap);
    }
}

/// Close a panel from its group's caption ✕ (or a float's WM close).
/// Routes to the per-panel hide path rather than mutating the model
/// directly — the ✕ must behave exactly like the View toggle's hide
/// half (the workspace one cancels an in-flight Unfold All).
fn close_panel(panel: DockPanel) {
    match panel {
        DockPanel::Workspace => crate::workspace::set_visible(false),
        DockPanel::DocMap => crate::docmap::set_visible(false),
    }
}

fn close_active_panel(id: u32) {
    let panel = with_dock(|d| d.layout.group(id).map(DockGroup::active_panel)).flatten();
    if let Some(panel) = panel {
        close_panel(panel);
    }
}

// --- gesture handlers ------------------------------------------------------------------

/// Caption press: arm a whole-group drag. The grab offset keeps the
/// pointer where the user pressed, relative to the frame's own origin,
/// so a float lands "in hand". A press on a floating group also raises
/// it above its floating siblings, without taking focus.
fn on_caption_press(id: u32, ev: &gdk::EventButton) -> glib::Propagation {
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    let (rx, ry) = root_i32(ev.root());
    with_dock(|d| {
        let Some(gi) = d.group_index(id) else {
            return;
        };
        let g = &d.groups[gi];
        if let Some(window) = g.float.as_ref().and_then(gtk::prelude::WidgetExt::window) {
            window.raise();
        }
        let outer = root_rect(&g.frame).unwrap_or_default();
        d.drag = Some(Drag {
            subject: DragSubject::Group(id),
            group_id: id,
            start: (rx, ry),
            grab: (rx - outer.x, ry - outer.y),
            float_size: (outer.w.max(MIN_FLOAT_W), outer.h.max(MIN_FLOAT_H)),
            armed_tab: None,
            started: false,
            cancelled: false,
        });
    });
    glib::Propagation::Stop
}

/// Tab press: arm a tab switch that becomes a single-panel drag if the
/// pointer travels. A torn-off tab floats at the default size with the
/// grab point in its caption.
fn on_tab_press(
    id: u32,
    index: usize,
    panel: DockPanel,
    ev: &gdk::EventButton,
) -> glib::Propagation {
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    let (rx, ry) = root_i32(ev.root());
    with_dock(|d| {
        d.drag = Some(Drag {
            subject: DragSubject::Panel(panel),
            group_id: id,
            start: (rx, ry),
            grab: (DEFAULT_FLOAT_W / 2, DOCK_CAPTION_H / 2),
            float_size: (DEFAULT_FLOAT_W, DEFAULT_FLOAT_H),
            armed_tab: Some(index),
            started: false,
            cancelled: false,
        });
    });
    glib::Propagation::Stop
}

/// Pointer motion with the button held on a caption or tab: cross the
/// threshold, then track the drop preview.
fn on_drag_motion(ev: &gdk::EventMotion) -> glib::Propagation {
    let cursor = root_i32(ev.root());
    with_dock(|d| {
        let Some(drag) = d.drag.as_mut() else {
            return;
        };
        if drag.cancelled {
            return;
        }
        if !drag.started {
            let dx = (cursor.0 - drag.start.0).abs();
            let dy = (cursor.1 - drag.start.1).abs();
            if dx < DOCK_DRAG_THRESHOLD && dy < DOCK_DRAG_THRESHOLD {
                return;
            }
            drag.started = true;
            // Once it is a drag, it is no longer a click.
            drag.armed_tab = None;
        }
        let preview = DockRect::new(
            cursor.0 - drag.grab.0,
            cursor.1 - drag.grab.1,
            drag.float_size.0,
            drag.float_size.1,
        );
        let subject = drag.subject;
        if let Some((_, rect)) = resolve_current_drop(d, subject, cursor, preview) {
            show_hint(&d.hint, rect);
        }
    });
    glib::Propagation::Stop
}

/// What a completed gesture asks for, decided under the dock borrow
/// and acted on after it drops.
enum Release {
    /// The model changed; reconcile.
    Apply,
}

/// Button release: commit a drag at the release point, or resolve a
/// click gesture (tab switch) if the pointer is still on what was
/// pressed.
fn on_drag_release(ev: &gdk::EventButton) -> glib::Propagation {
    // Only the button that started the gesture ends it — see
    // `on_splitter_release`. A secondary-button release mid-drag would
    // otherwise commit the move at wherever the pointer happened to be.
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    let cursor = root_i32(ev.root());
    let outcome = with_dock(|d| {
        let drag = d.drag.take()?;
        d.hint.hide();
        if drag.cancelled {
            return None;
        }
        if drag.started {
            // Recompute at the release point — the hint followed the
            // pointer, but the model mutation must key on where the
            // button actually went up.
            let preview = DockRect::new(
                cursor.0 - drag.grab.0,
                cursor.1 - drag.grab.1,
                drag.float_size.0,
                drag.float_size.1,
            );
            let (target, _) = resolve_current_drop(d, drag.subject, cursor, preview)?;
            match drag.subject {
                DragSubject::Panel(panel) => d.layout.move_panel(panel, target),
                DragSubject::Group(id) => d.layout.move_group(id, target),
            }
            return Some(Release::Apply);
        }
        let tab = drag.armed_tab?;
        if tab_contains(d, drag.group_id, tab, cursor) {
            d.layout.set_active_index(drag.group_id, tab);
            return Some(Release::Apply);
        }
        None
    })
    .flatten();
    if let Some(Release::Apply) = outcome {
        apply_layout();
    }
    glib::Propagation::Stop
}

/// Whether the root point is over tab `index` of group `id` — the
/// "release still on what was pressed" check for a tab switch.
fn tab_contains(d: &Ui, id: u32, index: usize, cursor: (i32, i32)) -> bool {
    d.group_index(id)
        .and_then(|gi| d.groups[gi].tab_bar.children().get(index).cloned())
        .and_then(|tab| root_rect(&tab))
        .is_some_and(|r| r.contains(cursor.0, cursor.1))
}

/// Resolve the drop target + hint rect for the current pointer
/// position. Builds the `DropZones` from live widget rects (floating
/// groups first — they sit above the docked ones) and delegates the
/// decision to the pure core resolver.
fn resolve_current_drop(
    d: &Ui,
    subject: DragSubject,
    cursor: (i32, i32),
    preview: DockRect,
) -> Option<(DropTarget, DockRect)> {
    let mid = root_rect(&d.area)?;
    let mut groups: Vec<(u32, DockRect)> = Vec::new();
    for g in d.groups.iter().filter(|g| g.float.is_some()) {
        if let Some(r) = root_rect(&g.frame) {
            groups.push((g.id, r));
        }
    }
    for g in d.groups.iter().filter(|g| g.float.is_none()) {
        if let Some(r) = root_rect(&g.frame) {
            groups.push((g.id, r));
        }
    }
    let zones = DropZones { mid, groups };
    Some(resolve_drop(&zones, &d.layout, subject, cursor, preview))
}

/// The implicit pointer grab was taken away mid-gesture — a modal
/// appearing, an Alt+Tab, the window iconified. Abandon whatever was in
/// flight without committing, the GTK analogue of Win32's
/// `WM_CAPTURECHANGED` arm: the hint comes down, and a splitter drag's
/// last applied size is written through so the session cache matches
/// what is on screen (the release that would have done it never
/// arrives). Always proceeds.
fn on_grab_broken() -> glib::Propagation {
    let splitter_was_live = with_dock(|d| {
        d.drag = None;
        d.hint.hide();
        d.splitter_drag.take().is_some()
    });
    if splitter_was_live == Some(true) {
        sync_to_shell();
    }
    glib::Propagation::Proceed
}

/// Esc during a live drag cancels it without committing. Always
/// proceeds so the key still reaches whoever else wants it.
fn on_key_press(ev: &gdk::EventKey) -> glib::Propagation {
    if ev.keyval() == gdk::keys::constants::Escape {
        with_dock(|d| {
            if let Some(drag) = d.drag.as_mut() {
                drag.cancelled = true;
                d.hint.hide();
            }
            // A splitter drag has already applied its intermediate sizes
            // to the model (that is what makes it live), so cancelling
            // means putting the size back where the press found it.
            if let Some(drag) = d.splitter_drag.take() {
                d.layout.set_side_size(drag.side, drag.size_at_start);
                relayout(d);
                // No `sync_to_shell` here: `size_at_start` is what the
                // press found, and every path that changes a side size
                // outside a live drag writes through before another
                // press can happen — so the restored value is already
                // the persisted one.
            }
        });
    }
    glib::Propagation::Proceed
}

/// A press on a floating frame's resize border starts a WM resize
/// drag; the WM then owns the gesture and reports the result through
/// `configure-event`.
fn on_frame_press(id: u32, frame: &gtk::EventBox, ev: &gdk::EventButton) -> glib::Propagation {
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    let float =
        with_dock(|d| d.group_index(id).and_then(|gi| d.groups[gi].float.clone())).flatten();
    let Some(win) = float else {
        return glib::Propagation::Proceed;
    };
    let (x, y) = ev.position();
    let a = frame.allocation();
    let Some(edge) = resize_edge(
        x as i32,
        y as i32,
        a.width(),
        a.height(),
        FLOAT_RESIZE_BORDER,
    ) else {
        return glib::Propagation::Proceed;
    };
    let (rx, ry) = root_i32(ev.root());
    win.begin_resize_drag(
        edge,
        i32::try_from(ev.button()).unwrap_or(1),
        rx,
        ry,
        ev.time(),
    );
    glib::Propagation::Stop
}

/// A floating toplevel moved or resized (by us, by a WM resize drag,
/// or by the WM clamping it): mirror the live rect into the model.
/// Returns `false` (not handled) so GTK's own configure handling runs;
/// `configure-event` is one of the `bool`-returning signals.
fn on_float_configured(win: &gtk::Window) -> bool {
    let (x, y) = win.position();
    let (w, h) = win.size();
    with_dock(|d| {
        let hosted = d
            .groups
            .iter()
            .find(|g| g.float.as_ref() == Some(win))
            .map(|g| g.id);
        if let Some(id) = hosted {
            d.layout.set_floating_rect(id, DockRect::new(x, y, w, h));
        }
    });
    false
}

/// WM close on a floating toplevel: hide its active panel, keep the
/// window (never destroyed — pooled by the reconcile that follows).
fn on_float_delete(win: &gtk::Window) -> glib::Propagation {
    let panel = with_dock(|d| {
        d.groups
            .iter()
            .find(|g| g.float.as_ref() == Some(win))
            .and_then(|g| d.layout.group(g.id))
            .map(DockGroup::active_panel)
    })
    .flatten();
    if let Some(panel) = panel {
        close_panel(panel);
    }
    glib::Propagation::Stop
}

// --- side splitters ------------------------------------------------------------------

fn on_splitter_press(side: DockSide, ev: &gdk::EventButton) -> glib::Propagation {
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    let start = root_i32(ev.root());
    with_dock(|d| {
        d.splitter_drag = Some(SplitterDrag {
            side,
            start,
            size_at_start: d.layout.side_size(side),
        });
    });
    glib::Propagation::Stop
}

/// Splitter drag: compute the *visually clamped* result via the same
/// authority the layout uses, then store that — so the model's size
/// always matches what renders and a drag past the limit has no dead
/// zone on the way back.
fn on_splitter_motion(ev: &gdk::EventMotion) -> glib::Propagation {
    let cursor = root_i32(ev.root());
    with_dock(|d| {
        let Some(drag) = d.splitter_drag else {
            return;
        };
        let proposed = side_drag_size(
            drag.side,
            drag.size_at_start,
            (cursor.0 - drag.start.0, cursor.1 - drag.start.1),
        );
        let (w, h) = d.area_size;
        let mut probe = d.layout.clone();
        probe.set_side_size(drag.side, proposed);
        let frame = compute_frame(
            DockRect::new(0, 0, w, h),
            &probe,
            MIN_EDITOR_W,
            MIN_EDITOR_H,
        );
        let rendered = frame
            .bands
            .iter()
            .find(|b| b.side == drag.side)
            .map(|b| match drag.side {
                DockSide::Left | DockSide::Right => b.rect.w,
                DockSide::Top | DockSide::Bottom => b.rect.h,
            });
        if let Some(rendered) = rendered {
            if rendered != d.layout.side_size(drag.side) {
                d.layout.set_side_size(drag.side, rendered);
                relayout(d);
            }
        }
    });
    glib::Propagation::Stop
}

/// Only the button that started the drag ends it: GDK holds an
/// implicit grab per button, so a stray secondary-button release
/// mid-drag arrives here too and must not end the gesture early.
fn on_splitter_release(ev: &gdk::EventButton) -> glib::Propagation {
    if ev.button() != 1 {
        return glib::Propagation::Proceed;
    }
    // Release is a good moment to write the new size through — when the
    // gesture is still live, i.e. Escape or a broken grab has not
    // already consumed it (each of those settles the model itself). A
    // plain click still arrives here and re-writes the unchanged size,
    // which is idempotent.
    if with_dock(|d| d.splitter_drag.take().is_some()) == Some(true) {
        sync_to_shell();
    }
    glib::Propagation::Stop
}

#[cfg(test)]
mod tests {
    use super::{geometry_rect, resize_edge, side_drag_size};
    use codepp_core::dock::{DockRect, DockSide};
    use gtk::gdk::WindowEdge;

    #[test]
    fn side_drag_sign_conventions() {
        // Dragging right grows a Left band and shrinks a Right one.
        assert_eq!(side_drag_size(DockSide::Left, 200, (30, 0)), 230);
        assert_eq!(side_drag_size(DockSide::Right, 200, (30, 0)), 170);
        // Dragging down grows a Top band and shrinks a Bottom one.
        assert_eq!(side_drag_size(DockSide::Top, 200, (0, 30)), 230);
        assert_eq!(side_drag_size(DockSide::Bottom, 200, (0, 30)), 170);
        // The off-axis component is ignored.
        assert_eq!(side_drag_size(DockSide::Left, 200, (0, 99)), 200);
    }

    #[test]
    fn resize_edge_corners_beat_sides_and_the_body_is_none() {
        let (w, h, b) = (300, 200, 6);
        assert_eq!(resize_edge(0, 0, w, h, b), Some(WindowEdge::NorthWest));
        assert_eq!(resize_edge(299, 0, w, h, b), Some(WindowEdge::NorthEast));
        assert_eq!(resize_edge(0, 199, w, h, b), Some(WindowEdge::SouthWest));
        assert_eq!(resize_edge(299, 199, w, h, b), Some(WindowEdge::SouthEast));
        assert_eq!(resize_edge(2, 100, w, h, b), Some(WindowEdge::West));
        assert_eq!(resize_edge(297, 100, w, h, b), Some(WindowEdge::East));
        assert_eq!(resize_edge(150, 3, w, h, b), Some(WindowEdge::North));
        assert_eq!(resize_edge(150, 196, w, h, b), Some(WindowEdge::South));
        // Just inside the border on both axes: the body.
        assert_eq!(resize_edge(6, 6, w, h, b), None);
        assert_eq!(resize_edge(150, 100, w, h, b), None);
    }

    #[test]
    fn geometry_rect_needs_every_field() {
        let full = codepp_core::WindowGeometry {
            width: Some(800),
            height: Some(600),
            x: Some(10),
            y: Some(20),
            maximized: false,
        };
        assert_eq!(
            geometry_rect(Some(full)),
            Some(DockRect::new(10, 20, 800, 600))
        );
        let no_pos = codepp_core::WindowGeometry { x: None, ..full };
        assert_eq!(geometry_rect(Some(no_pos)), None);
        assert_eq!(geometry_rect(None), None);
    }
}
