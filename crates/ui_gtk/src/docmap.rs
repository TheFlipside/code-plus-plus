//! The Document Map — a right-side miniature of the active buffer with a
//! translucent orange box marking the visible viewport, ported from the
//! Win32 backend (`ui_win32`'s `docmap_*` functions).
//!
//! # Shape
//!
//! A **second Scintilla widget** is the miniature. It shares the active
//! tab's document through `SCI_SETDOCPOINTER` (rebound on tab switch by
//! [`sync_to_active_tab`]) exactly as the main view does, so no buffer
//! text is duplicated. It is created once at startup and never destroyed
//! or reassigned — the same lifetime discipline the main view keeps, and
//! what lets the single-view source invariant in `lib.rs` allow a second
//! permanent view without dangling any `Copy` `EditorHandle`.
//!
//! # The orange box
//!
//! Rather than Win32's buffered `WM_PAINT` overlay, GTK stacks a
//! transparent [`gtk::DrawingArea`] on top of the miniature inside a
//! [`gtk::Overlay`]. The drawing area's `draw` handler fills one flat
//! rectangle — full panel width, spanning the visible line range — with
//! the identical colour and transparency Win32 uses:
//!
//! * colour `#FFA500` (Notepad++'s Document Map orange — the same
//!   `DOCMAP_VIEWPORT_COLOR = 0x0000_A5FF` the Win32 side names), and
//! * alpha `60/255 ≈ 24 %` (`DOCMAP_VIEWPORT_FILL_ALPHA` on Win32).
//!
//! The same drawing area also intercepts the mouse (button press +
//! button-1 drag), the GTK stand-in for Win32's Scintilla subclass proc:
//! the read-only miniature underneath never starts a selection drag, and
//! a click/drag scrolls the *main* editor to the corresponding line.

use std::cell::Cell;
use std::rc::Rc;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    CARETSTYLE_INVISIBLE, SCI_DOCLINEFROMVISIBLE, SCI_GETDOCPOINTER, SCI_GETFIRSTVISIBLELINE,
    SCI_GETLINECOUNT, SCI_LINESCROLL, SCI_LINESONSCREEN, SCI_POINTYFROMPOSITION,
    SCI_POSITIONFROMLINE, SCI_SETCARETSTYLE, SCI_SETDOCPOINTER, SCI_SETHSCROLLBAR,
    SCI_SETMARGINWIDTHN, SCI_SETVSCROLLBAR, SCI_SETZOOM, SCI_TEXTHEIGHT, SCI_VISIBLEFROMDOCLINE,
};
use gtk::glib;
use gtk::prelude::*;

use crate::state::with_state;

/// Panel title, matching the Win32 header label.
const PANEL_TITLE: &str = "Document Map";
/// Initial panel width the first time the map is opened. Mirrors Win32's
/// `DEFAULT_DOCMAP_WIDTH_PX`.
const DEFAULT_WIDTH_PX: i32 = 160;
/// Width floor — below this the miniature collapses into unreadable
/// blocks. Mirrors Win32's `MIN_DOCMAP_WIDTH_PX`.
const MIN_WIDTH_PX: i32 = 80;
/// Miniature zoom: `-10` shrinks the font to the smallest legible
/// block-shape that still hints at text density. Mirrors Win32's
/// `SCI_SETZOOM(-10)` and Notepad++'s default map font size.
const MINIATURE_ZOOM: isize = -10;

/// The `#FFA500` viewport orange, as cairo colour components (R, G, B).
/// The exact tone Win32's `DOCMAP_VIEWPORT_COLOR = 0x0000_A5FF` encodes.
const VIEWPORT_RGB: (f64, f64, f64) = (1.0, 165.0 / 255.0, 0.0);
/// Fill alpha of the viewport wash, `60/255 ≈ 24 %` — the same value as
/// Win32's `DOCMAP_VIEWPORT_FILL_ALPHA`. A soft tint that leaves the
/// miniature text underneath legible.
const VIEWPORT_ALPHA: f64 = 60.0 / 255.0;

/// Everything the Document Map owns for the window's lifetime.
pub struct DocMapPanel {
    /// Horizontal splitter: the editor content is pack1, this panel pack2.
    paned: gtk::Paned,
    /// The panel column (header + overlay). Shown/hidden as a unit.
    container: gtk::Box,
    /// The transparent overlay that paints the orange box and catches the
    /// mouse. Held so [`update_indicator`] can `queue_draw` it.
    overlay_area: gtk::DrawingArea,
    /// One-shot map-width to apply on the next paned `size-allocate`. The
    /// map is the RIGHT (pack2) pane, so its width is `total - position`
    /// and the total isn't known until the paned is allocated — which, on
    /// a session restore, is after `set_shown` runs. The size-allocate
    /// handler set up in [`Self::build`] reads this, sets the position
    /// once, and clears it; a `None` leaves user drags untouched.
    pending_map_width: Rc<Cell<Option<i32>>>,
    /// Whether the panel is currently shown.
    visible: bool,
    /// Remembered panel width across show/hide cycles and sessions.
    width: i32,
}

impl DocMapPanel {
    /// Build the panel, wrapping `editor_content` in a horizontal paned
    /// whose right pane is the map. `miniature` is the second Scintilla
    /// widget (created in `run`); it becomes the overlay's base child.
    pub fn build(editor_content: &gtk::Widget, miniature: &gtk::Widget) -> Self {
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_size_request(MIN_WIDTH_PX, -1);

        // Title row: "Document Map … ✕", mirroring the workspace panel's
        // header shape so the two docked panels read alike.
        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        title_row.set_margin_top(2);
        title_row.set_margin_bottom(2);
        title_row.set_margin_start(6);
        title_row.set_margin_end(2);
        let title = gtk::Label::new(Some(PANEL_TITLE));
        title.set_xalign(0.0);
        title_row.pack_start(&title, true, true, 0);
        let close_btn = gtk::Button::with_label("✕");
        close_btn.set_relief(gtk::ReliefStyle::None);
        WidgetExt::set_tooltip_text(&close_btn, Some("Close Document Map"));
        close_btn.connect_clicked(|_| set_visible(false));
        title_row.pack_end(&close_btn, false, false, 0);
        container.pack_start(&title_row, false, false, 0);

        container.pack_start(
            &gtk::Separator::new(gtk::Orientation::Horizontal),
            false,
            false,
            0,
        );

        // Exclude the miniature from all input. This is the *edit* guard,
        // not `SCI_SETREADONLY`: read-only is a document-level flag, so it
        // stops protecting the moment `SCI_SETDOCPOINTER` binds the
        // miniature onto the main view's shared, editable document. The
        // real guarantee is that no input ever reaches the miniature —
        // matching Win32, which omits `WS_TABSTOP` and intercepts the
        // mouse. `set_can_focus(false)` takes it out of the Tab-focus
        // chain (so keystrokes can never land here), and a key-press
        // swallow is belt-and-suspenders against a stray `grab_focus`.
        miniature.set_can_focus(false);
        miniature.connect_key_press_event(|_, _| glib::Propagation::Stop);

        // The miniature sits under a transparent drawing area. GtkOverlay
        // composites the overlay child over the base, so where the drawing
        // area paints nothing the miniature text shows through.
        let overlay = gtk::Overlay::new();
        overlay.add(miniature);
        let overlay_area = gtk::DrawingArea::new();
        // The overlay area catches the mouse so the miniature underneath
        // never starts a selection drag (or a middle-click primary-selection
        // paste) — the GTK analogue of Win32's subclass proc. Button press
        // + button-1 motion drive scroll-to-line; the wheel is forwarded to
        // the MAIN editor so the map tracks rather than scrolling its own
        // independent viewport (matching Win32's `WM_MOUSEWHEEL` arm).
        overlay_area.add_events(
            gtk::gdk::EventMask::BUTTON_PRESS_MASK
                | gtk::gdk::EventMask::BUTTON1_MOTION_MASK
                | gtk::gdk::EventMask::SCROLL_MASK,
        );
        overlay_area.connect_draw(|area, cr| {
            draw_overlay(area, cr);
            glib::Propagation::Proceed
        });
        overlay_area.connect_button_press_event(|_, ev| {
            scroll_to_event_y(ev.position().1);
            glib::Propagation::Stop
        });
        overlay_area.connect_motion_notify_event(|_, ev| {
            scroll_to_event_y(ev.position().1);
            glib::Propagation::Stop
        });
        overlay_area.connect_scroll_event(|_, ev| {
            scroll_main_by_wheel(ev);
            glib::Propagation::Stop
        });
        // Re-centre + repaint whenever the map area is (re)sized: on first
        // show (the area gets its allocation), on window resize, and on a
        // splitter drag. This is the GTK analogue of Win32 updating the
        // indicator from the parent's `WM_SIZE`, and it seeds a correct box
        // on the first paint after a session restore with no interaction.
        overlay_area.connect_size_allocate(|_, _| refresh());
        overlay.add_overlay(&overlay_area);
        container.pack_start(&overlay, true, true, 0);

        paned.pack1(editor_content, true, true);
        paned.pack2(&container, false, false);

        // Seed the initial splitter position for a restored width, once
        // the paned knows its total width. The map is the right pane, so
        // position = total - width. One-shot: cleared after the first
        // apply so subsequent user drags (which GTK preserves for a
        // non-resize pack2 across window resizes) are never overridden.
        let pending_map_width: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
        let pending = pending_map_width.clone();
        paned.connect_size_allocate(move |p, alloc| {
            if let Some(target) = pending.get() {
                let pos = (alloc.width() - target.max(MIN_WIDTH_PX)).max(0);
                if p.position() != pos {
                    p.set_position(pos);
                }
                pending.set(None);
            }
        });

        // Realize the panel's children, then keep the column collapsed
        // until the user opens the map — same opt-out of the toplevel
        // `show_all` the workspace panel and FIF dock use.
        container.show_all();
        container.hide();
        container.set_no_show_all(true);

        Self {
            paned,
            container,
            overlay_area,
            pending_map_width,
            visible: false,
            width: DEFAULT_WIDTH_PX,
        }
    }

    /// The horizontal paned, so the caller can pack it into the layout.
    pub fn paned(&self) -> &gtk::Paned {
        &self.paned
    }

    /// Show or hide the column, snapshotting the width on hide and
    /// requesting the remembered width on show (applied by the paned's
    /// `size-allocate` handler once the total width is known).
    fn set_shown(&mut self, visible: bool) {
        if visible {
            self.pending_map_width
                .set(Some(self.width.max(MIN_WIDTH_PX)));
            self.container.show();
            // If the paned is already allocated (a mid-session toggle,
            // not a cold-start restore), the handler won't fire on its
            // own — nudge a re-allocation so the pending width applies.
            self.paned.queue_resize();
        } else {
            if self.visible {
                self.width = self.current_width();
            }
            self.container.hide();
        }
        self.visible = visible;
    }

    /// The width to persist: the live map width while visible, else the
    /// last remembered value. The map is the right pane, so its live
    /// width is its own container allocation.
    fn current_width(&self) -> i32 {
        if self.visible {
            let w = self.container.allocated_width();
            if w > 0 {
                return w;
            }
        }
        self.width
    }
}

// --- Registration + indicator sync -----------------------------------

thread_local! {
    /// The View-menu "Document Map" check item, once built.
    static MENU_CHECK: std::cell::RefCell<Option<gtk::CheckMenuItem>> =
        const { std::cell::RefCell::new(None) };
    /// The toolbar toggle, once built.
    static TB_TOGGLE: std::cell::RefCell<Option<gtk::ToggleToolButton>> =
        const { std::cell::RefCell::new(None) };
    /// True while [`sync_indicators`] owns the check/toggle, so their
    /// handlers don't loop back into [`set_visible`].
    static SYNCING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Register the View-menu check item so [`sync_indicators`] can keep it
/// in step with the panel's real visibility. Called by [`crate::menu`].
pub(crate) fn register_menu_check(item: gtk::CheckMenuItem) {
    MENU_CHECK.with(|c| *c.borrow_mut() = Some(item));
}

/// Register the toolbar toggle button. Called by [`crate::toolbar`].
pub(crate) fn register_toolbar_toggle(button: gtk::ToggleToolButton) {
    TB_TOGGLE.with(|t| *t.borrow_mut() = Some(button));
}

/// True while [`sync_indicators`] owns the check/toggle. The menu and
/// toolbar handlers bail on this so a programmatic `set_active` never
/// loops back into [`set_visible`]. Mirrors `workspace::syncing`.
pub(crate) fn syncing() -> bool {
    SYNCING.with(std::cell::Cell::get)
}

/// Drive both indicators to `visible` without re-firing their handlers.
fn sync_indicators(visible: bool) {
    SYNCING.with(|s| s.set(true));
    MENU_CHECK.with(|c| {
        if let Some(item) = &*c.borrow() {
            item.set_active(visible);
        }
    });
    TB_TOGGLE.with(|t| {
        if let Some(button) = &*t.borrow() {
            button.set_active(visible);
        }
    });
    SYNCING.with(|s| s.set(false));
}

// --- Public entry points ----------------------------------------------

/// Show or hide the panel. The single funnel behind the View toggle, the
/// toolbar button and the header close button, so all three agree.
pub(crate) fn set_visible(visible: bool) {
    with_state(|st| st.docmap.set_shown(visible));
    if visible {
        // Bind the map to the active buffer and paint the box for the
        // current viewport — the panel may have been hidden across
        // several scrolls/tab switches.
        sync_to_active_tab();
    }
    sync_indicators(visible);
    sync_to_shell();
}

/// Rebind the miniature to the active tab's document, then repaint. Port
/// of Win32's `sync_docmap_to_active_tab`. A no-op when the map is hidden
/// (nothing to paint) or the tab has no document yet.
pub(crate) fn sync_to_active_tab() {
    with_state(|st| {
        if !st.docmap.visible {
            return;
        }
        let doc = st.shell.active().map_or(0, |t| t.scintilla_doc);
        if doc == 0 {
            return;
        }
        // Skip a redundant re-point: `SCI_SETDOCPOINTER` is not free and
        // rebinding resets the miniature's scroll, which `update_indicator`
        // would then have to correct.
        if doc != st.docmap_editor.send(SCI_GETDOCPOINTER, 0, 0) {
            st.docmap_editor.send(SCI_SETDOCPOINTER, 0, doc);
        }
        update_indicator(st);
    });
}

/// Recompute the viewport range and repaint the box. Cheap enough to run
/// on every `sci-notify`; a no-op when the map is hidden. Called from the
/// main editor's `sci-notify` handler (scroll/edit) and after tab switch.
pub(crate) fn refresh() {
    with_state(|st| {
        if st.docmap.visible {
            update_indicator(st);
        }
    });
}

/// Apply the saved session: width, and open the panel if it was open.
/// Mirrors `workspace::apply_saved`. Runs at cold start.
pub(crate) fn apply_saved() {
    let Some(Some(saved)) = with_state(|st| st.shell.saved_docmap_session()) else {
        return;
    };
    if let Some(width) = saved.width {
        if width >= MIN_WIDTH_PX {
            with_state(|st| st.docmap.width = width);
        }
    }
    if saved.visible {
        set_visible(true);
    }
}

/// Snapshot the live panel state into the shell so the next
/// `save_session` persists it. Called from the autosave / shutdown path.
pub(crate) fn sync_to_shell() {
    with_state(|st| {
        let dm = &st.docmap;
        let session = codepp_core::session::DocMapSession {
            visible: dm.visible,
            width: Some(dm.current_width()),
        };
        st.shell.set_docmap_session(Some(session));
    });
}

// --- Miniature configuration ------------------------------------------

/// Apply the once-only Scintilla view settings that make the second view a
/// miniature overview. Every setting here is **view-level** (per-`ViewStyle`)
/// and so survives the `SCI_SETDOCPOINTER` swaps that rebind the miniature
/// onto each tab's document — which is why it runs once at creation. Port of
/// the Win32 block at the docmap Scintilla's creation.
///
/// Note what is *not* here: `SCI_SETREADONLY` is deliberately omitted. That
/// flag is a **document** property, so setting it on the miniature would set
/// it on the shared document once bound — freezing the *main* editor too —
/// and setting it only on the miniature's throwaway initial document would
/// be superseded on the first bind anyway. Edits are prevented instead by
/// keeping all input off the miniature (`set_can_focus(false)` + mouse/key
/// interception in [`DocMapPanel::build`]), which is also how the Win32
/// original does it (no `WS_TABSTOP`).
pub(crate) fn configure_miniature(handle: EditorHandle) {
    // Invisible caret hides the blink; both scrollbars off (fixed overview,
    // not scrollable); every margin collapses so the miniature uses its full
    // width; zoom shrinks the font to a density-hinting block shape.
    handle.send(SCI_SETCARETSTYLE, CARETSTYLE_INVISIBLE, 0);
    handle.send(SCI_SETHSCROLLBAR, 0, 0);
    handle.send(SCI_SETVSCROLLBAR, 0, 0);
    handle.send(SCI_SETMARGINWIDTHN, 0, 0);
    handle.send(SCI_SETMARGINWIDTHN, 1, 0);
    handle.send(SCI_SETMARGINWIDTHN, 2, 0);
    handle.send(SCI_SETZOOM, MINIATURE_ZOOM as usize, 0);
}

// --- Geometry + painting ----------------------------------------------

/// The visible document-line range of the main editor, as a half-open
/// `[first, last)` in document-line space (folding-aware). `None` for an
/// empty buffer or a pre-first-layout editor. Shared by the painter and
/// the centering step so they never disagree.
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

/// Recompute the viewport range, then scroll the miniature so the
/// highlighted band stays centred (a long file won't fit the panel at any
/// zoom) and `queue_draw` the overlay so the box repaints against the
/// fresh scroll position. Port of Win32's
/// `update_docmap_viewport_indicator`. Cheap: a handful of direct-calls
/// plus one deferred redraw, well inside the §8 budget.
fn update_indicator(st: &crate::state::GtkUiState) {
    let (main, dm) = (st.editor, st.docmap_editor);
    let Some((first_doc, last_doc)) = visible_doc_range(main) else {
        // Empty buffer: clear any leftover box from a previous tab.
        st.docmap.overlay_area.queue_draw();
        return;
    };
    // Centre the highlight in the miniature's own viewport.
    let map_lines = dm.send(SCI_LINESONSCREEN, 0, 0);
    let span = last_doc - first_doc;
    let target_first = (first_doc + span / 2 - map_lines / 2).max(0);
    let delta = target_first - dm.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
    if delta != 0 {
        dm.send(SCI_LINESCROLL, 0, delta);
    }
    st.docmap.overlay_area.queue_draw();
}

/// Paint the flat translucent-orange viewport box onto `cr`. Full panel
/// width, vertical extent = top of the first visible doc line → bottom of
/// the last, computed with `SCI_POINTYFROMPOSITION` on the miniature —
/// the same geometry as Win32's `paint_docmap_viewport_overlay`. Paints
/// nothing on an empty buffer or degenerate coordinates.
fn draw_overlay(area: &gtk::DrawingArea, cr: &gtk::cairo::Context) {
    with_state(|st| {
        let (main, dm) = (st.editor, st.docmap_editor);
        let Some((first_doc, last_doc)) = visible_doc_range(main) else {
            return;
        };
        let first_pos = dm.send(SCI_POSITIONFROMLINE, first_doc as usize, 0);
        let last_pos = dm.send(SCI_POSITIONFROMLINE, last_doc as usize, 0);
        if first_pos < 0 || last_pos < 0 {
            return;
        }
        let top_y = dm.send(SCI_POINTYFROMPOSITION, 0, first_pos);
        let mut bottom_y = dm.send(SCI_POINTYFROMPOSITION, 0, last_pos);
        if bottom_y <= top_y {
            // Single visible line: give the box one line's height so it
            // never collapses to nothing.
            bottom_y = top_y + dm.send(SCI_TEXTHEIGHT, first_doc as usize, 0);
        }
        let height = (bottom_y - top_y).max(0);
        let width = area.allocated_width();
        if height <= 0 || width <= 0 {
            return;
        }
        cr.set_source_rgba(
            VIEWPORT_RGB.0,
            VIEWPORT_RGB.1,
            VIEWPORT_RGB.2,
            VIEWPORT_ALPHA,
        );
        // Pixel coordinates are small; narrow to i32 (crate-allowed
        // truncation) before the exact `f64::from`, sidestepping the
        // isize→f64 precision-loss lint.
        cr.rectangle(
            0.0,
            f64::from(top_y as i32),
            f64::from(width),
            f64::from(height as i32),
        );
        // `fill` can only fail if the cairo context is in an error state,
        // which for a widget-supplied context it is not; log rather than
        // unwrap so a theme oddity never takes the process down.
        if let Err(err) = cr.fill() {
            tracing::warn!(?err, "docmap: overlay fill failed");
        }
    });
}

// --- Drag-to-scroll ----------------------------------------------------

/// Map a y coordinate inside the miniature to a document line and scroll
/// the MAIN editor so that line is centred. Called on button press and
/// button-1 drag over the overlay. Port of Win32's `scroll_main_to_doc_line`
/// driven from `docmap_scintilla_subclass_proc`.
fn scroll_to_event_y(y: f64) {
    with_state(|st| {
        let (main, dm) = (st.editor, st.docmap_editor);
        // Map the click y to a miniature visible line via the row height,
        // then to a document line. Computing from y/height (rather than a
        // hit-test message) clamps naturally for a click below the last
        // line and needs no special "outside any character" handling.
        let row_h = dm.send(SCI_TEXTHEIGHT, 0, 0).max(1);
        let map_first = dm.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
        let visible_line = (map_first + y as isize / row_h).max(0);
        let target_doc = dm.send(SCI_DOCLINEFROMVISIBLE, visible_line as usize, 0);
        let target_visible = main.send(SCI_VISIBLEFROMDOCLINE, target_doc as usize, 0);
        let main_lines = main.send(SCI_LINESONSCREEN, 0, 0);
        let desired_first = (target_visible - main_lines / 2).max(0);
        let delta = desired_first - main.send(SCI_GETFIRSTVISIBLELINE, 0, 0);
        if delta != 0 {
            main.send(SCI_LINESCROLL, 0, delta);
        }
        // The main-editor scroll fires its own `sci-notify`, but repaint
        // the box now so drag tracking feels immediate.
        update_indicator(st);
    });
}

/// Lines to scroll the MAIN editor per wheel notch over the map — a
/// conventional 3-line step; the map then re-centres to follow.
const WHEEL_LINES: isize = 3;

/// Forward a wheel event over the map to the MAIN editor rather than
/// letting the miniature scroll its own independent viewport (which would
/// desync the orange box). Port of Win32's `WM_MOUSEWHEEL` arm, which
/// forwards the delta to the main editor and consumes it on the map.
fn scroll_main_by_wheel(ev: &gtk::gdk::EventScroll) {
    let lines: isize = match ev.direction() {
        gtk::gdk::ScrollDirection::Up => -WHEEL_LINES,
        gtk::gdk::ScrollDirection::Down => WHEEL_LINES,
        // Smooth-scroll devices (when GTK synthesises a smooth event):
        // scale the vertical delta by the notch step.
        gtk::gdk::ScrollDirection::Smooth => {
            let (_, dy) = ev.delta();
            (dy * f64::from(WHEEL_LINES as i32)).round() as isize
        }
        // Horizontal wheel and any future direction: ignore.
        _ => 0,
    };
    if lines == 0 {
        return;
    }
    with_state(|st| {
        st.editor.send(SCI_LINESCROLL, 0, lines);
        update_indicator(st);
    });
}
