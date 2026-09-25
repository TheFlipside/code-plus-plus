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
//! dropped), while a `with_state` closure may take a brief dock borrow:
//! a read such as [`is_visible`] or [`legacy_band_width`], or one of the
//! model-only writes the `NPPM_DMM*` handlers make from inside the NPPM
//! dispatch's borrow (see the plugin-panel section).
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

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;

use codepp_core::dock::{
    compute_frame, resolve_drop, DockContainer, DockGroup, DockLayout, DockLocation, DockPanel,
    DockRect, DockSide, DragSubject, DropTarget, DropZones, MIN_FLOAT_H, MIN_FLOAT_W,
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

/// One plugin's docking dialog, adopted as a dock panel's content: the
/// GTK counterpart of `ui_win32`'s `DockEntry`.
///
/// What gets here has already been checked by
/// `crate::plugin::register_dock_dialog` — a live, free-standing,
/// non-toplevel `GtkWidget` the plugin made. From then on it is an
/// ordinary panel: it docks, floats, tabs with the host's own panels and
/// persists in `session.xml`, all through the model and the reconciler
/// the built-in panels use.
struct PluginPanel {
    /// The panel this registration owns — interned from the sanitized
    /// module and title (`codepp_shell::intern_plugin_dock_panel`), so
    /// stable across runs and what `session.xml` keys its position on.
    /// The panel is the identity; everything else here is the plugin's
    /// current view of it.
    panel: DockPanel,
    /// The host's own reference to the plugin's widget, held for as long
    /// as the registration stands and never used to destroy it. It is
    /// what keeps [`Self::handle`] naming this object: registrations are
    /// found by the widget's address, and an object finalized while its
    /// registration stood — pulled out of [`Self::content`] by the
    /// plugin, or destroyed before [`forget_destroyed_plugin_panels`]
    /// runs — would free that address for a new widget to reuse and be
    /// mistaken for. A plugin that destroys the widget sets
    /// [`Self::gone`], and the registration is then dropped.
    _widget: gtk::Widget,
    /// What the dock shows and moves for this panel: the host's scrolled
    /// container around [`Self::widget`] — see [`scrolled_content`]. The
    /// widget stays inside it for the registration's lifetime.
    content: gtk::Widget,
    /// `tTbData.hClient` as the plugin sent it: the handle it addresses
    /// the panel by in `NPPM_DMMSHOW` / `NPPM_DMMHIDE` /
    /// `NPPM_DMMUPDATEDISPINFO`, gets back from
    /// `NPPM_DMMGETPLUGINHWNDBYNAME`, and finds in the `wParam` of every
    /// `DMN_*` about the panel. Compared, never dereferenced.
    handle: *mut std::ffi::c_void,
    /// The plugin's `tTbData`, retained so `NPPM_DMMUPDATEDISPINFO` has
    /// something to re-read. Stored here, read only by
    /// `crate::plugin::update_dock_disp_info`; see
    /// `codepp_plugin_host::DockDialogParams::tb_data` for the lifetime
    /// contract the plugin owes it.
    tb_data: *const codepp_plugin_host::TbData,
    /// `pszName`, raw — the lookup key `NPPM_DMMVIEWOTHERTAB` and
    /// `NPPM_DMMGETPLUGINHWNDBYNAME` match, because a plugin knows only
    /// what it registered. The caption shows the sanitized form the
    /// panel identity carries.
    name: String,
    /// `pszModuleName`, raw — the optional disambiguator for
    /// `NPPM_DMMGETPLUGINHWNDBYNAME`.
    module_name: String,
    /// Registry index of the plugin that registered the panel, if the
    /// registration arrived while the host was calling a plugin — where
    /// its `DMN_*` go first (`Shell::plugin_panel_message_target`).
    caller: Option<usize>,
    /// The plugin's own tab icon (`tTbData.hIconTab`, a `GdkPixbuf`
    /// here, with `DWS_ICONTAB`), or `None` for the generic plugin
    /// glyph.
    icon: Option<Pixbuf>,
    /// The container this plugin was last told its panel is in, through
    /// `DMN_DOCK` / `DMN_FLOAT`. `None` until the first reconcile after
    /// registration, which is what makes that reconcile send the
    /// registration-time notification. Written before the notification
    /// goes out — see [`container_notices`].
    told: Option<DockContainer>,
    /// Set from the widget's `destroy` handler when the plugin destroys
    /// its own panel. Checked by everything that would use the widget, so
    /// a disposed widget is never put back into a container, until
    /// [`forget_destroyed_plugin_panels`] drops the registration.
    gone: Rc<Cell<bool>>,
}

impl PluginPanel {
    /// Whether this registration still stands for a live widget.
    fn live(&self) -> bool {
        !self.gone.get()
    }
}

/// What `crate::plugin::register_dock_dialog` hands the dock once a
/// plugin's widget has passed its checks. See [`PluginPanel`] for the
/// fields.
pub(crate) struct PluginPanelSpec {
    pub panel: DockPanel,
    pub handle: *mut std::ffi::c_void,
    pub tb_data: *const codepp_plugin_host::TbData,
    pub name: String,
    pub module_name: String,
    pub caller: Option<usize>,
    pub icon: Option<Pixbuf>,
    /// `tTbData.uMask`'s `DWS_DF_CONT_*` preference, decoded — where the
    /// panel first opens.
    pub initial_side: Option<DockSide>,
    pub gone: Rc<Cell<bool>>,
}

/// One `DMN_DOCK` / `DMN_FLOAT` to send: the notification code for the
/// container `panel` is in now, and whom to tell. Built by
/// [`container_notices`], sent by `crate::plugin::deliver_dock_notices`.
pub(crate) struct DockNotice {
    pub panel: DockPanel,
    pub handle: *mut std::ffi::c_void,
    pub caller: Option<usize>,
    pub code: u32,
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
    /// Every plugin docking dialog registered this session, in
    /// registration order. See [`PluginPanel`].
    plugin_panels: Vec<PluginPanel>,
    /// The model has changed under a `NPPM_DMM*` handler and the widget
    /// tree has not caught up. Those handlers run inside the dispatch's
    /// state borrow, where [`apply_layout`] cannot run — it syncs the
    /// session through `with_state` — so they only mark, and the NPPM
    /// dispatch reconciles once its borrow has ended ([`take_dirty`]).
    /// The same shape as Win32's `dock_dirty`.
    dirty: bool,
}

impl Ui {
    /// The content widget for `panel`. A field per built-in panel rather
    /// than a map, so adding a `DockPanel` variant is a compile error
    /// here — which is what makes "every panel is hosted" hold by
    /// construction rather than by a test remembering to check.
    ///
    /// A plugin panel's content is its registered widget, and is `None`
    /// until the plugin registers it — the normal case for a panel a
    /// restored layout names before its plugin has loaded — and once the
    /// plugin has destroyed it. Every caller skips a `None` rather than
    /// substituting a placeholder the user could neither use nor close;
    /// a panel no plugin will supply this session is parked instead
    /// (`crate::plugin`'s load pass).
    fn panel_content(&self, panel: DockPanel) -> Option<gtk::Widget> {
        match panel {
            DockPanel::Workspace => Some(self.workspace_content.clone()),
            DockPanel::DocMap => Some(self.docmap_content.clone()),
            DockPanel::Plugin(_) => self
                .plugin_panels
                .iter()
                .find(|p| p.panel == panel && p.live())
                .map(|p| p.content.clone()),
        }
    }

    /// The icon a plugin panel's tab shows: its plugin's own, if it gave
    /// one.
    fn plugin_icon(&self, panel: DockPanel) -> Option<Pixbuf> {
        self.plugin_panels
            .iter()
            .find(|p| p.panel == panel && p.live())
            .and_then(|p| p.icon.clone())
    }

    /// The live registration for the widget a plugin addresses by
    /// `handle`.
    fn plugin_panel_by_handle(&self, handle: *mut std::ffi::c_void) -> Option<&PluginPanel> {
        if handle.is_null() {
            return None;
        }
        self.plugin_panels
            .iter()
            .find(|p| std::ptr::eq(p.handle, handle) && p.live())
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

/// The size a panel or group opens at when it is torn off into a
/// float: a third of the main window's current width and height,
/// floored at the model's minimum. The docked size is *not* used —
/// a full-height side band makes an awkward window nobody wants and
/// has to resize anyway (a user's report); a third of the window is a
/// working size on any display, and it scales with the window rather
/// than being a fixed constant.
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
    (x.clamp(0, new_w.max(0)), DOCK_CAPTION_H / 2)
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
        plugin_panels: Vec::new(),
        dirty: false,
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
fn build_tab(
    id: u32,
    index: usize,
    panel: DockPanel,
    own_icon: Option<&Pixbuf>,
    active: bool,
    scale: i32,
) -> gtk::EventBox {
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
    if let Some(icon) = panel_icon(panel, own_icon, scale) {
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
/// high-DPI screens. A plugin panel shows its plugin's own icon when it
/// gave one (`own`), else a generic plugin glyph — never the Document
/// Map's, which left two plugin panels in one group indistinguishable
/// from each other and from the map. `None` on a decode failure
/// (cosmetic; the tab keeps its label and tooltip).
fn panel_icon(panel: DockPanel, own: Option<&Pixbuf>, scale: i32) -> Option<gtk::Image> {
    let px = DOCK_TAB_ICON_PX * scale.max(1);
    let source = if let Some(own) = own {
        own.clone()
    } else {
        let (at_1x, at_2x): (&[u8], &[u8]) = match panel {
            DockPanel::Workspace => (
                include_bytes!("../../../assets/icons/folder-workspace.png"),
                include_bytes!("../../../assets/icons/folder-workspace@2x.png"),
            ),
            DockPanel::DocMap => (
                include_bytes!("../../../assets/icons/document-map.png"),
                include_bytes!("../../../assets/icons/document-map@2x.png"),
            ),
            DockPanel::Plugin(_) => (
                include_bytes!("../../../assets/icons/plugin-panel.png"),
                include_bytes!("../../../assets/icons/plugin-panel@2x.png"),
            ),
        };
        let bytes = if scale >= 2 { at_2x } else { at_1x };
        match Pixbuf::from_read(Cursor::new(bytes)) {
            Ok(pixbuf) => pixbuf,
            Err(err) => {
                tracing::warn!(?err, ?panel, "dock: tab icon decode failed");
                return None;
            }
        }
    };
    let scaled = source.scale_simple(px, px, gtk::gdk_pixbuf::InterpType::Bilinear)?;
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
        d.dirty = false;
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
    // Last, so a plugin reacting to where its panel is finds the tree and
    // the session already settled — and with no borrow held, so its
    // handler's `NPPM_*` is answered rather than declined.
    let notices = with_dock(container_notices).unwrap_or_default();
    crate::plugin::deliver_dock_notices(notices);
}

/// Record, for every registered plugin panel, the container it is in
/// now, and return a notice for each one whose container differs from
/// what its plugin was last told — including every panel whose plugin has
/// been told nothing yet, which is how a freshly registered panel gets
/// upstream's registration-time notification.
///
/// Recording happens here, under the borrow, before
/// `crate::plugin::deliver_dock_notices` sends anything, and that order
/// is what stops a transition being told twice: a plugin's handler may
/// show or hide a panel, which reconciles again from inside the
/// delivery, and the nested pass must find the transition already
/// recorded. What bounds the round trip when a handler registers a *new*
/// panel — which the nested pass has genuinely not told — is the
/// delivery's queue, not this order. Win32's `container_notices` in
/// `dock_panels.rs` is the same function, and a source scan pins the
/// order on both.
fn container_notices(d: &mut Ui) -> Vec<DockNotice> {
    let mut out = Vec::new();
    for entry in d.plugin_panels.iter_mut().filter(|p| p.live()) {
        let now = d.layout.container_of(entry.panel);
        let told = entry.told.is_some_and(|last| last.is_same(now));
        entry.told = Some(now);
        if !told {
            out.push(DockNotice {
                panel: entry.panel,
                handle: entry.handle,
                caller: entry.caller,
                code: codepp_plugin_host::docking::dock_container_code(&d.layout, now),
            });
        }
    }
    out
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

    // 4. Hidden panels go to parking: the two built-in ones and every
    //    registered plugin panel — a model-parked one included, since
    //    `DockLayout::park` takes it out of every group.
    let hosted: Vec<DockPanel> = DockPanel::BUILT_IN
        .into_iter()
        .chain(d.plugin_panels.iter().filter(|p| p.live()).map(|p| p.panel))
        .collect();
    for panel in hosted {
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
                // Replace the size request the docked layout gave the
                // frame: inside a toplevel that request becomes the
                // window's *minimum*, so a float could be grown by its
                // border but never shrunk below its last band size
                // (reported by a user). The model's floor goes in its
                // place rather than `-1` so the live window can never be
                // dragged smaller than the rect `set_floating_rect` will
                // persist — otherwise the two disagree until the next
                // reconcile happens to resize it back.
                g.frame.set_size_request(MIN_FLOAT_W, MIN_FLOAT_H);
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
    let contents: Vec<Option<gtk::Widget>> =
        group.panels.iter().map(|p| d.panel_content(*p)).collect();
    let icons: Vec<Option<Pixbuf>> = group.panels.iter().map(|p| d.plugin_icon(*p)).collect();
    // Read live rather than cached at install: a float dragged to a
    // monitor with another scale factor gets its tab icons re-decoded
    // at the next reconcile.
    let scale = d.main_window.scale_factor();
    let g = &mut d.groups[gi];
    for (i, content) in contents.iter().enumerate() {
        let Some(content) = content else {
            continue;
        };
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
    rebuild_tab_bar(g, group, &icons, scale);
}

/// Rebuild a group's tab bar from scratch — a handful of small widgets,
/// far cheaper than diffing, and it runs only on a dock mutation.
/// `icons` holds each tab's plugin-supplied icon, aligned with
/// `group.panels`.
fn rebuild_tab_bar(g: &mut GroupWidget, group: &DockGroup, icons: &[Option<Pixbuf>], scale: i32) {
    for child in g.tab_bar.children() {
        g.tab_bar.remove(&child);
    }
    if group.panels.len() < 2 {
        g.tab_bar.hide();
        return;
    }
    for (i, panel) in group.panels.iter().enumerate() {
        let own = icons.get(i).and_then(Option::as_ref);
        let tab = build_tab(g.id, i, *panel, own, i == group.active, scale);
        g.tab_bar.pack_start(&tab, false, false, 0);
    }
    g.tab_bar.show();
}

/// Send a hidden panel's content to parking.
fn park(d: &mut Ui, panel: DockPanel) {
    let Some(content) = d.panel_content(panel) else {
        return;
    };
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
    // Plugin panels are kept: their groups wait for the widgets their
    // plugins register, and the startup load pass
    // (`crate::plugin::restore_panel_plugins`) loads the plugins that
    // supply them, runs each panel's own command, and parks whatever no
    // loaded plugin can supply — one a Windows-written session names for
    // a Windows-only plugin included — so nothing is left on screen
    // empty.
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

thread_local! {
    /// Set by [`freeze_session`] once the quit has captured the layout.
    static SESSION_FROZEN: Cell<bool> = const { Cell::new(false) };
}

/// Push the model into the shell's session cache so the next
/// `save_session` persists it. Called after every mutation and from
/// the autosave / shutdown path. A no-op once [`freeze_session`] has
/// run.
pub(crate) fn sync_to_shell() {
    if SESSION_FROZEN.with(Cell::get) {
        return;
    }
    if let Some(session) = with_dock(|d| d.layout.to_session()) {
        with_state(|st| st.shell.set_dock_session(Some(session)));
    }
}

/// Stop the layout reaching the session from here on. Called by
/// `crate::quit` straight after it captures the layout and before the
/// plugins hear they are shutting down, so the arrangement saved is the
/// one the user left: a plugin that hides or shows its panel from
/// `NPPN_SHUTDOWN` changes the model — and so, without this, the session
/// the next start restores. Win32 gets the same result by declining its
/// plugins' messages during shutdown.
pub(crate) fn freeze_session() {
    SESSION_FROZEN.with(|f| f.set(true));
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
        // Closing a plugin's panel owes the plugin a `DMN_CLOSE`, sent
        // before the panel hides — how a plugin keeps a "Show Console"
        // style menu check in step with what the user can see.
        DockPanel::Plugin(_) => crate::plugin::close_plugin_panel(panel),
    }
}

fn close_active_panel(id: u32) {
    let panel = with_dock(|d| d.layout.group(id).map(DockGroup::active_panel)).flatten();
    if let Some(panel) = panel {
        close_panel(panel);
    }
}

// --- plugin panels -------------------------------------------------------------------
//
// The `NPPM_DMM*` handlers reach these from inside the NPPM dispatch's
// state borrow, so the ones that change the model change *only* the
// model and mark `Ui::dirty`; the dispatch reconciles once its borrow
// has ended ([`take_dirty`]). None of them calls `with_state`. The
// only widgets touched here are ones not in the window's tree:
// registration builds the panel's scrolled container, and
// [`forget_destroyed_plugin_panels`] — run from an idle, not from a
// handler — takes an emptied one out of it.

/// The container a plugin panel's widget lives in, which is what the dock
/// then shows, hides and moves: a scrolled window, so a panel smaller
/// than the widget's minimum size scrolls instead of painting over its
/// neighbour.
///
/// A plugin's widget may ask for any minimum — a label that does not
/// wrap, a row of buttons — and `GtkLayout` allocates a child no smaller
/// than its minimum. Measured with `example-hello`'s panel in an 80 px
/// left band: its label drew across the splitter into the editor. The
/// built-in panels never met this, because their content already
/// scrolls; a plugin's need not. Scrollbars appear only when the panel
/// is smaller than the widget asks for.
///
/// A widget that scrolls by itself (a text view, a tree view) goes in
/// directly; anything else through a viewport with no frame, so the
/// panel looks as it would without the wrapper. The host shows the
/// widget itself once, here, as Notepad++ shows `hClient` — the one
/// property of the plugin's widget it sets. After that, showing and
/// hiding the panel shows and hides this container.
fn scrolled_content(widget: &gtk::Widget) -> gtk::ScrolledWindow {
    let scrolled = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scrolled.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scrolled.set_shadow_type(gtk::ShadowType::None);
    if widget.is::<gtk::Scrollable>() {
        scrolled.add(widget);
    } else {
        let viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        viewport.set_shadow_type(gtk::ShadowType::None);
        viewport.add(widget);
        viewport.show();
        scrolled.add(&viewport);
    }
    widget.show();
    scrolled
}

/// Register a plugin's docking dialog as a dock panel — the commit half
/// of `crate::plugin::register_dock_dialog`, which has checked the widget.
///
/// `adopt` takes the host's own reference to the widget and is called
/// only once every refusal below has been ruled out, under the same
/// borrow as the commit. That order is load-bearing: adopting sinks a
/// floating reference, so a registration refused *after* adopting would
/// drop the only reference and finalize the plugin's widget out from
/// under it. Refused, the widget is never touched.
///
/// Returns the adopted widget, for the caller to watch. It goes into its
/// [`scrolled_content`] here, and that container joins the tree at the
/// reconcile the dispatch runs afterwards.
pub(crate) fn register_plugin_panel(
    spec: PluginPanelSpec,
    adopt: impl FnOnce() -> gtk::Widget,
) -> Result<gtk::Widget, &'static str> {
    with_dock(|d| {
        let live = || d.plugin_panels.iter().filter(|p| p.live());
        if live().any(|p| std::ptr::eq(p.handle, spec.handle)) {
            return Err("that widget is already registered");
        }
        if live().any(|p| p.panel == spec.panel) {
            return Err("that panel is already registered");
        }
        if live().count() >= codepp_core::dock::MAX_PLUGIN_PANELS {
            return Err("the plugin panel cap is reached");
        }
        let widget = adopt();
        let content = scrolled_content(&widget).upcast();
        d.plugin_panels.push(PluginPanel {
            panel: spec.panel,
            _widget: widget.clone(),
            content,
            handle: spec.handle,
            tb_data: spec.tb_data,
            name: spec.name,
            module_name: spec.module_name,
            caller: spec.caller,
            icon: spec.icon,
            told: None,
            gone: spec.gone,
        });
        // Where the panel opens the first time it is shown, from the
        // plugin's own `DWS_DF_CONT_*` preference — never over a
        // position the user or a restored session has since given it.
        if let Some(side) = spec.initial_side {
            d.layout.set_initial_side(spec.panel, side);
        }
        // A panel parked because no loaded plugin could supply it comes
        // back where it was the moment its widget arrives — the way
        // Notepad++ shows a panel it saved open when its plugin registers
        // it.
        d.layout.unpark(&[spec.panel]);
        // A restored layout can already name this panel, its group
        // waiting since startup for the widget only registration
        // supplies. So the tree needs a reconcile even when the model did
        // not change.
        d.dirty = true;
        Ok(widget)
    })
    .unwrap_or(Err("the dock is busy (a re-entrant registration)"))
}

/// Record the command that reopens `panel` at the next start, with the
/// seal the shell made for it — see
/// `UiPlatform::record_panel_open_command`. Model only; persisted by the
/// reconcile that follows the registration.
pub(crate) fn set_open_command(
    panel: DockPanel,
    command: i32,
    seal: Option<codepp_core::dock::CommandSeal>,
) {
    with_dock(|d| d.layout.set_open_command(panel, command, seal));
}

/// `NPPM_DMMSHOW`: show the panel registered for `handle` and bring it to
/// the front of its group. `false` for a handle nothing is registered
/// under. Model only — see [`take_dirty`].
pub(crate) fn show_plugin_panel(handle: *mut std::ffi::c_void) -> bool {
    with_dock(|d| {
        let Some(panel) = d.plugin_panel_by_handle(handle).map(|p| p.panel) else {
            return false;
        };
        d.layout.show(panel);
        d.layout.activate(panel);
        d.dirty = true;
        true
    })
    .unwrap_or(false)
}

/// `NPPM_DMMHIDE`: hide the panel registered for `handle`. The
/// registration survives, and so does the panel's position — a later
/// `NPPM_DMMSHOW` reopens it where it was. No `DMN_CLOSE`: the plugin
/// asked. Model only — see [`take_dirty`].
pub(crate) fn hide_plugin_panel(handle: *mut std::ffi::c_void) -> bool {
    with_dock(|d| {
        let Some(panel) = d.plugin_panel_by_handle(handle).map(|p| p.panel) else {
            return false;
        };
        d.layout.hide(panel);
        d.dirty = true;
        true
    })
    .unwrap_or(false)
}

/// `NPPM_DMMVIEWOTHERTAB`: show the panel registered under `name` — its
/// plugin's raw `pszName`, which is what the plugin knows — and make it
/// the front tab of its group. A hidden panel is shown first, since
/// "view" can only mean "put it in front of the user". Model only.
pub(crate) fn view_plugin_panel(name: &str) -> bool {
    with_dock(|d| {
        let Some(panel) = d
            .plugin_panels
            .iter()
            .find(|p| p.live() && p.name == name)
            .map(|p| p.panel)
        else {
            return false;
        };
        d.layout.show(panel);
        d.layout.activate(panel);
        d.dirty = true;
        true
    })
    .unwrap_or(false)
}

/// `NPPM_DMMGETPLUGINHWNDBYNAME`: the handle registered under the raw
/// `name`, and under `module` too when one is given.
pub(crate) fn plugin_panel_handle(
    name: &str,
    module: Option<&str>,
) -> Option<*mut std::ffi::c_void> {
    with_dock(|d| {
        d.plugin_panels
            .iter()
            .find(|p| p.live() && p.name == name && module.is_none_or(|m| m == p.module_name))
            .map(|p| p.handle)
    })
    .flatten()
}

/// The `tTbData` the panel registered for `handle` was registered with,
/// for `NPPM_DMMUPDATEDISPINFO` to re-read.
pub(crate) fn plugin_panel_tb_data(
    handle: *mut std::ffi::c_void,
) -> Option<*const codepp_plugin_host::TbData> {
    with_dock(|d| d.plugin_panel_by_handle(handle).map(|p| p.tb_data)).flatten()
}

/// Take a re-read `pszName` / `pszModuleName` for the panel registered
/// for `handle`: the keys `NPPM_DMMVIEWOTHERTAB` and
/// `NPPM_DMMGETPLUGINHWNDBYNAME` match, which a plugin addresses by its
/// *current* name.
///
/// The caption does not move, and that is deliberate rather than an
/// omission — the same call Win32 makes. A panel's title is its interned
/// identity, and `session.xml` keys the panel's remembered position on
/// it, so a caption that followed the plugin would either lose the
/// user's layout on every rename or need a second, divergent name to
/// persist under.
pub(crate) fn rename_plugin_panel(
    handle: *mut std::ffi::c_void,
    name: String,
    module_name: String,
) {
    with_dock(|d| {
        if let Some(entry) = d
            .plugin_panels
            .iter_mut()
            .find(|p| p.live() && std::ptr::eq(p.handle, handle))
        {
            entry.name = name;
            entry.module_name = module_name;
        }
    });
}

/// Whether a `NPPM_DMM*` handler changed the model since the last
/// reconcile, clearing the mark. The NPPM dispatch asks once its state
/// borrow has ended and runs [`apply_layout`] if so. A reconcile for any
/// other reason clears it too, since it catches the tree up with
/// everything.
pub(crate) fn take_dirty() -> bool {
    with_dock(|d| std::mem::take(&mut d.dirty)).unwrap_or(false)
}

/// Whom to tell about `panel`: its registered handle and its registrant,
/// or `None` if no live widget is registered for it.
pub(crate) fn plugin_panel_notify_target(
    panel: DockPanel,
) -> Option<(*mut std::ffi::c_void, Option<usize>)> {
    with_dock(|d| {
        d.plugin_panels
            .iter()
            .find(|p| p.live() && p.panel == panel)
            .map(|p| (p.handle, p.caller))
    })
    .flatten()
}

/// Whether a notice raised for `panel` under `handle` still has a live
/// registration to go to. A handler for an earlier notice may have
/// destroyed the widget, and its address may even have been reused by a
/// widget registered since — so both halves are checked.
pub(crate) fn plugin_panel_is_live(panel: DockPanel, handle: *mut std::ffi::c_void) -> bool {
    with_dock(|d| {
        d.plugin_panels
            .iter()
            .any(|p| p.live() && p.panel == panel && std::ptr::eq(p.handle, handle))
    })
    .unwrap_or(false)
}

/// Whether a live widget is registered for plugin `panel`.
pub(crate) fn is_plugin_panel_registered(panel: DockPanel) -> bool {
    with_dock(|d| d.plugin_panels.iter().any(|p| p.live() && p.panel == panel)).unwrap_or(false)
}

/// Drop the registrations whose plugins destroyed their widgets, closing
/// their panels. Runs from an idle scheduled by the widget's `destroy`
/// handler rather than from the handler itself, which can fire inside a
/// reconcile holding the dock borrow.
///
/// Closing is the honest outcome: nothing is left to show, and the plugin
/// registers a fresh widget if it wants the panel back — the dropped
/// registration no longer stands in its way, and the layout still
/// remembers where the panel was.
pub(crate) fn forget_destroyed_plugin_panels() {
    let changed = with_dock(|d| {
        // Once each: a plugin can destroy two widgets registered in turn
        // under one name before this idle runs.
        let mut gone: Vec<DockPanel> = Vec::new();
        for p in d.plugin_panels.iter().filter(|p| !p.live()) {
            if !gone.contains(&p.panel) {
                gone.push(p.panel);
            }
        }
        if gone.is_empty() {
            return false;
        }
        // Each panel's scrolled container is the host's own and stays
        // wherever the dock last put it, now empty — GTK took only the
        // destroyed widget out of it. Taken out too, or it would keep its
        // share of a group that goes on showing other panels. Dropping
        // the last reference to it then finalizes nothing of the
        // plugin's.
        for p in d.plugin_panels.iter().filter(|p| !p.live()) {
            unparent(&p.content);
        }
        d.plugin_panels.retain(PluginPanel::live);
        for panel in gone {
            // A plugin that destroyed its widget and registered a
            // replacement under the same name before this idle ran — a
            // natural way to rebuild a panel — holds the panel again.
            // Closing it now would take away the replacement.
            if d.plugin_panels.iter().any(|p| p.panel == panel) {
                continue;
            }
            tracing::info!(
                panel = panel.persist_key(),
                "a plugin destroyed its dock panel's widget; closing the panel"
            );
            if d.layout.is_visible(panel) {
                d.layout.hide(panel);
            }
        }
        true
    });
    if changed == Some(true) {
        apply_layout();
    }
}

/// A copy of the model, for decisions that also need the shell — which
/// cannot be asked while the dock is borrowed.
pub(crate) fn layout_snapshot() -> Option<DockLayout> {
    with_dock(|d| d.layout.clone())
}

/// Apply a model-only change `f` and report whether it changed anything,
/// without reconciling — the load pass batches its changes and reconciles
/// once. `false` when the dock is busy.
pub(crate) fn update_layout(f: impl FnOnce(&mut DockLayout) -> bool) -> bool {
    with_dock(|d| f(&mut d.layout)).unwrap_or(false)
}

// --- gesture handlers ------------------------------------------------------------------

/// Caption press: arm a whole-group drag. On an already-floating group
/// the grab offset keeps the pointer where the user pressed, relative to
/// the frame's own origin, so the window moves "in hand"; on a docked
/// group the float opens at [`tear_off_size`] with the pointer at the
/// same fraction along the caption ([`tear_off_grab`]). A press on a
/// floating group also raises it above its floating siblings, without
/// taking focus.
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
        // An already-floating group is being *moved*: it keeps its size
        // and the pointer stays where the user pressed. A docked group
        // is being torn off: it opens at the tear-off size.
        let (grab, float_size) = if g.float.is_some() {
            (
                (rx - outer.x, ry - outer.y),
                (outer.w.max(MIN_FLOAT_W), outer.h.max(MIN_FLOAT_H)),
            )
        } else {
            let size = tear_off_size(d.main_window.size());
            (tear_off_grab(rx - outer.x, outer.w, size.0), size)
        };
        d.drag = Some(Drag {
            subject: DragSubject::Group(id),
            group_id: id,
            start: (rx, ry),
            grab,
            float_size,
            armed_tab: None,
            started: false,
            cancelled: false,
        });
    });
    glib::Propagation::Stop
}

/// Tab press: arm a tab switch that becomes a single-panel drag if the
/// pointer travels. A torn-off tab floats at the tear-off size with
/// the grab point mid-caption.
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
        let size = tear_off_size(d.main_window.size());
        d.drag = Some(Drag {
            subject: DragSubject::Panel(panel),
            group_id: id,
            start: (rx, ry),
            grab: (size.0 / 2, DOCK_CAPTION_H / 2),
            float_size: size,
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

/// The plugin panel's scrolled container. Display-gated, because it
/// builds real widgets: driven by `crate::display_tests`, which owns the
/// invocation and explains why these cannot be `#[test]`s of their own.
#[cfg(test)]
pub(crate) mod content_tests {
    use super::scrolled_content;
    use codepp_core::dock::MIN_DOCK_BAND_PX;
    use gtk::prelude::*;

    /// Whatever a plugin's widget asks for, the container the dock
    /// allocates asks for less than the narrowest band — across and down,
    /// since a band on the left or right constrains the width and one on
    /// the top or bottom the height. The dock's `GtkLayout` allocates a
    /// child no smaller than its minimum, so this is what stops a panel
    /// painting over its neighbour.
    pub(crate) fn a_plugin_panel_asks_for_less_than_the_narrowest_band() {
        gtk::init().expect("gtk::init failed — no display?");
        // Wide: a label that cannot wrap, like the one that overflowed an
        // 80 px band. Tall: a column of them.
        let wide: gtk::Widget = gtk::Label::new(Some(
            "one line of text, far wider than the narrowest band a panel can be given",
        ))
        .upcast();
        let tall = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for i in 0..12 {
            tall.pack_start(&gtk::Label::new(Some(&format!("row {i}"))), false, false, 0);
        }
        let tall: gtk::Widget = tall.upcast();
        // Shown as a plugin shows its own: GTK 3 gives a hidden widget no
        // size at all, which would make every assertion below vacuous.
        wide.show();
        tall.show_all();
        assert!(
            wide.preferred_width().0 > MIN_DOCK_BAND_PX,
            "the wide fixture must ask for more than the narrowest band"
        );
        assert!(
            tall.preferred_height().0 > MIN_DOCK_BAND_PX,
            "the tall fixture must ask for more than the narrowest band"
        );

        let across = scrolled_content(&wide);
        let down = scrolled_content(&tall);
        // Shown as the dock shows the active panel's container, for the
        // same reason.
        across.show();
        down.show();
        let (across_min, _) = across.preferred_width();
        let (down_min, _) = down.preferred_height();
        assert!(
            across_min < MIN_DOCK_BAND_PX,
            "a wide widget's container asks for {across_min} px across"
        );
        assert!(
            down_min < MIN_DOCK_BAND_PX,
            "a tall widget's container asks for {down_min} px down"
        );

        // The widget is shown, inside a viewport with no frame, inside
        // the container.
        assert!(wide.is_visible(), "the host shows the plugin's widget");
        let viewport = wide
            .parent()
            .and_then(|p| p.downcast::<gtk::Viewport>().ok())
            .expect("a widget that does not scroll goes in through a viewport");
        assert_eq!(viewport.shadow_type(), gtk::ShadowType::None);
        assert_eq!(
            viewport.parent().as_ref(),
            Some(across.upcast_ref::<gtk::Widget>())
        );

        // One that scrolls by itself goes in directly.
        let text: gtk::Widget = gtk::TextView::new().upcast();
        let direct = scrolled_content(&text);
        assert_eq!(
            text.parent().as_ref(),
            Some(direct.upcast_ref::<gtk::Widget>())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{geometry_rect, resize_edge, side_drag_size, tear_off_grab, tear_off_size};
    use codepp_core::dock::{DockRect, DockSide, MIN_FLOAT_H, MIN_FLOAT_W};
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
    fn tear_off_size_is_a_third_of_the_window_floored_at_the_minimum() {
        assert_eq!(tear_off_size((1200, 900)), (400, 300));
        // A small window floors at the model minimum rather than a sliver.
        assert_eq!(tear_off_size((300, 240)), (MIN_FLOAT_W, MIN_FLOAT_H));
        assert_eq!(tear_off_size((0, 0)), (MIN_FLOAT_W, MIN_FLOAT_H));
    }

    #[test]
    fn tear_off_grab_keeps_the_pointer_fraction_along_the_caption() {
        // Pressed a quarter of the way along a 400-wide band → a quarter
        // of the way along the 200-wide float.
        assert_eq!(
            tear_off_grab(100, 400, 200),
            (50, super::DOCK_CAPTION_H / 2)
        );
        // Beyond either end clamps rather than leaving the float behind.
        assert_eq!(tear_off_grab(-30, 400, 200).0, 0);
        assert_eq!(tear_off_grab(900, 400, 200).0, 200);
        // A degenerate source width does not divide by zero.
        assert_eq!(tear_off_grab(10, 0, 200).0, 200);
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

/// Source guards for orderings no unit test can see: the one that bounds
/// the `DMN_DOCK` / `DMN_FLOAT` round trip, and the one that keeps the
/// dock borrow and `with_state` from deadlocking on each other.
#[cfg(test)]
mod source_guards {
    use crate::source_scan::{code_only, skip_char_literal, strip_test_modules};

    /// The text of the top-level function whose definition starts with
    /// `signature`, up to its closing brace at the start of a line.
    fn body_of(src: &str, signature: &str) -> String {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` not found"));
        let rest = &src[start..];
        rest[..rest.find("\n}").unwrap_or(rest.len())].to_string()
    }

    /// The twin of `ui_win32`'s test of the same name, whose comment
    /// gives the reason. Here the record is written by
    /// `container_notices` under the dock borrow, and `apply_layout`
    /// hands the notices to `crate::plugin::deliver_dock_notices` only
    /// after that borrow has ended.
    #[test]
    fn the_container_is_recorded_before_the_notification_is_sent() {
        let dock = strip_test_modules(&code_only(include_str!("dock.rs")));
        let apply = body_of(&dock, "pub(crate) fn apply_layout()");
        let record = apply
            .find("with_dock(container_notices)")
            .expect("the reconcile no longer records containers under the dock borrow");
        let send = apply
            .find("deliver_dock_notices(notices)")
            .expect("the reconcile no longer sends the notices");
        assert!(record < send, "the send now precedes the record");

        let notices = body_of(&dock, "fn container_notices(");
        assert!(
            notices.contains("entry.told = Some(now);"),
            "container_notices no longer writes the record"
        );
        assert!(
            !notices.contains("deliver_dock_notices") && !notices.contains(".send("),
            "container_notices sends while the dock borrow is live"
        );

        let plugin = strip_test_modules(&code_only(include_str!("plugin.rs")));
        let deliver = body_of(&plugin, "pub(crate) fn deliver_dock_notices(");
        assert!(
            !deliver.contains(".told"),
            "the record moved into the send loop, after the send it must precede"
        );
    }

    /// Code holding the dock borrow never calls `with_state` — the rule
    /// the module docs give for keeping the two `RefCell`s from
    /// deadlocking on each other. Checks every argument handed to
    /// `with_dock`, closure or function name; the body of a function
    /// passed by name is not followed, so it keeps the same rule by hand.
    #[test]
    fn nothing_under_the_dock_borrow_asks_for_the_state() {
        let dock = strip_test_modules(&code_only(include_str!("dock.rs")));
        let bytes = dock.as_bytes();
        let mut checked = 0;
        for (at, _) in dock.match_indices("with_dock(") {
            let open = at + "with_dock".len();
            let mut depth = 0usize;
            let mut i = open;
            while i < bytes.len() {
                match bytes[i] {
                    b'\'' => {
                        i = skip_char_literal(bytes, i).max(i + 1);
                        continue;
                    }
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let argument = &dock[open..i.min(dock.len())];
            assert!(
                !argument.contains("with_state"),
                "a `with_dock` call asks for the state under the dock borrow: {argument}"
            );
            checked += 1;
        }
        assert!(
            checked > 10,
            "found only {checked} `with_dock` calls: the scan is not reading the module"
        );
    }
}
