//! The tab strip.
//!
//! # Why a `GtkNotebook` with empty pages
//!
//! Code++ is single-view: one Scintilla widget serves every tab and
//! `SCI_SETDOCPOINTER` swaps the document underneath it (DESIGN.md
//! §7.2, Phase 3). What this backend needs is therefore a tab *strip* —
//! a selector — not a container that owns per-tab content.
//!
//! `GtkNotebook` is still the right widget, because everything a tab
//! strip has to do beyond selection is already in it: drag reorder,
//! scroll arrows once the tabs overflow, and the desktop theme's own
//! tab rendering. The trick is to give every page a zero-size
//! `gtk::Box` and pack the Scintilla widget as a *sibling* below the
//! notebook rather than inside it. Measured on GTK 3.24: with three
//! tabs in an 800×600 window the notebook allocates 27 px of height and
//! the sibling gets the remaining 573 — the page area collapses to
//! nothing, as needed. That is the same relationship `ui_win32` has
//! between its `SysTabControl32` and its Scintilla child window.
//!
//! The page widgets are therefore interchangeable placeholders. Nothing
//! reads them, and every piece of visible information lives in the tab
//! *labels* — which is what lets [`TabStrip::sync`] repair any
//! divergence just by rewriting labels by index.
//!
//! # Why signals must be suppressed during sync
//!
//! Measured against GTK 3.24, not assumed: `gtk_notebook_append_page`
//! emits `switch-page` for the first page added, and
//! `set_current_page` emits it always. Both run inside
//! [`TabStrip::sync`], which is itself reached from that signal's own
//! handler — so without a guard the strip would re-enter `Shell` and
//! move `active_tab` behind the user's back. [`suppressed`] holds the
//! guard; the handler checks [`is_suppressed`] first.
//!
//! `page-reordered` is guarded the same way but for a different
//! reason: `sync` never calls `reorder_child`, so it cannot provoke
//! that signal. Drag-reorder is `GtkNotebook`'s own drag-and-drop, enabled by
//! `set_tab_reorderable`, and `sync` responds to the result by
//! relabelling rather than by moving pages itself. The check on that
//! handler is therefore defence against a future `sync` that *does*
//! reorder, not a live hazard — worth keeping, worth not mistaking for
//! one.
//!
//! Note also what is *not* emitted: removing a page never fires
//! `switch-page`, even when the selected index shifts, because the
//! selected *widget* did not change. Selection is therefore always
//! driven explicitly from `Shell.active_tab` at the end of `sync` and
//! never inferred from a signal.
//!
//! # Why the per-tab buttons key on buffer id
//!
//! A tab's close button and its middle-click gesture both capture the
//! tab's `id` — the stable buffer id — and re-find the tab by id when
//! they fire, rather than capturing a vector index. Indices go stale:
//! a plugin `NPPM_*` call, a `Ctrl+W`, or a drag can reorder or shrink
//! `Shell.tabs` between the label being built and the button being
//! clicked, and an index captured beforehand would then address a
//! different buffer — "clicked X, closed Y". DESIGN.md §7.4 tracks
//! exactly this bug on the Win32 strip, which arms on index; this
//! backend does not inherit it.

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib;
use gtk::prelude::*;

use codepp_shell::{tab_display_name, Tab};

/// Tab icons, embedded rather than read from disk at runtime. The same
/// PNGs the Win32 owner-draw strip blits (`ui_win32`'s `PNG_TAB_SAVE_*`),
/// so both backends show the identical glyph for the identical state,
/// and the binary stays self-contained per DESIGN.md §9.1. `@2x` is the
/// `HiDPI` variant, picked by the widget's scale factor exactly as the
/// Win32 side picks between its `_LO` and `_HIDPI` constants.
const PNG_TAB_SAVE: &[u8] = include_bytes!("../../../assets/icons/tab-save.png");
const PNG_TAB_SAVE_2X: &[u8] = include_bytes!("../../../assets/icons/tab-save@2x.png");
const PNG_TAB_SAVE_DIRTY: &[u8] = include_bytes!("../../../assets/icons/tab-save-dirty.png");
const PNG_TAB_SAVE_DIRTY_2X: &[u8] = include_bytes!("../../../assets/icons/tab-save-dirty@2x.png");

/// Logical size the tab icon renders at: the 16 px asset at scale 1,
/// the 32 px asset at scale 2, both occupying the same logical square.
const ICON_LOGICAL_PX: i32 = 16;

/// Width bounds for a tab label, in characters.
///
/// Both are load-bearing — see the comment at the call site. The
/// minimum is what stops an ellipsized label from collapsing to a bare
/// "…"; the maximum stops a long filename from pushing the strip wider
/// than the window. The full name stays reachable as a tooltip.
const LABEL_MIN_CHARS: i32 = 10;
/// See [`LABEL_MIN_CHARS`].
const LABEL_MAX_CHARS: i32 = 24;

/// X11/Wayland button number for the middle button.
const MIDDLE_BUTTON: u32 = 2;

thread_local! {
    /// True while [`TabStrip::sync`] is rewriting the notebook. See the
    /// module docs for why every handler has to honour it.
    static SUPPRESS: Cell<bool> = const { Cell::new(false) };
}

/// Clears [`SUPPRESS`] on drop, so an early return or a panic inside a
/// suppressed block cannot leave the strip's handlers permanently
/// disarmed — which would silently stop every tab click from working,
/// with no other symptom.
struct SuppressGuard;

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        SUPPRESS.with(|s| s.set(false));
    }
}

/// Run `f` with the notebook's signal handlers disarmed.
///
/// Not re-entrant by design: a nested call's guard would clear the flag
/// on the inner exit and leave the outer body unguarded. Nothing nests
/// today; the assertion pins that so a future call site cannot
/// introduce it unnoticed.
fn suppressed<R>(f: impl FnOnce() -> R) -> R {
    debug_assert!(
        !is_suppressed(),
        "nested suppressed() would disarm the outer guard on inner exit"
    );
    SUPPRESS.with(|s| s.set(true));
    let _guard = SuppressGuard;
    f()
}

/// True if a notebook signal should be ignored as self-inflicted.
pub fn is_suppressed() -> bool {
    SUPPRESS.with(Cell::get)
}

/// Handle to the tab strip.
///
/// `Clone` is a refcount bump on the notebook plus an `Rc` clone — this
/// is handed out by `GtkUiState::split` on every drain, so it has to
/// stay cheap.
#[derive(Clone)]
pub struct TabStrip {
    /// The notebook, used as a bare strip. See the module docs.
    pub notebook: gtk::Notebook,
    /// Page widgets in the order [`TabStrip::sync`] last wrote them.
    ///
    /// Needed because `page-reordered` reports the *new* index and the
    /// moved child but not where it came from, and `Shell::move_tab`
    /// needs both. Since `sync` is the only writer and a drag is the
    /// only thing that can invalidate it, its contents are exactly the
    /// pre-drag order at the moment the signal fires.
    order: Rc<RefCell<Vec<gtk::Widget>>>,
}

impl TabStrip {
    /// Build an empty strip.
    pub fn new() -> Self {
        let notebook = gtk::Notebook::new();
        // No page content, so the frame around it would render as a
        // stray line under the tabs.
        notebook.set_show_border(false);
        // Arrows once the tabs overflow, rather than squeezing them to
        // illegibility. Notepad++ scrolls too.
        notebook.set_scrollable(true);
        Self {
            notebook,
            order: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Reconcile the strip with a tab list and active index.
    ///
    /// Takes the two fields it needs rather than a `&Shell`, so the
    /// strip has no dependency on how they are stored and can be
    /// exercised from a test without standing up a `Shell` (and its
    /// file watcher and worker threads).
    ///
    /// Idempotent and total: it does not track *what* changed, it makes
    /// the strip match the model. That is deliberate — the incremental
    /// alternative is the shape where one missed call site leaves the
    /// strip showing a tab that no longer exists. Cheap enough to be
    /// unconditional: sessions run well under 100 tabs, and the work is
    /// one label rebuild each.
    ///
    /// Callers should still avoid calling it on every Scintilla
    /// notification: the `sci-notify` handler gates its call on
    /// `crate::refresh_active_dirty` returning `true`, so the strip is
    /// only rebuilt on a transition that actually changes a label.
    pub fn sync(&self, tabs: &[Tab], active: Option<usize>) {
        suppressed(|| {
            let wanted = tabs.len();
            // Grow first, then shrink, so the two loops never both run.
            while (self.notebook.n_pages() as usize) < wanted {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
                // A page must be visible for its tab to be, even though
                // this one has no content and requests 0×0.
                page.show();
                self.notebook.append_page(&page, gtk::Widget::NONE);
                self.notebook.set_tab_reorderable(&page, true);
            }
            while (self.notebook.n_pages() as usize) > wanted {
                self.notebook.remove_page(Some(self.notebook.n_pages() - 1));
            }

            // Relabel every tab by index. This is also what repairs a
            // drag that `Shell::move_tab` rejected: the page widgets
            // are interchangeable, so rewriting labels in model order
            // puts the visible strip back into model order regardless
            // of how GTK permuted the pages.
            let scale = self.notebook.scale_factor();
            for (idx, tab) in tabs.iter().enumerate() {
                let Ok(idx_u32) = u32::try_from(idx) else {
                    break;
                };
                let Some(page) = self.notebook.nth_page(Some(idx_u32)) else {
                    break;
                };
                let label = build_tab_label(tab, scale);
                self.notebook.set_tab_label(&page, Some(&label));
            }

            // Selection is set explicitly, never inferred: removing a
            // page shifts the selected index without emitting
            // `switch-page`, so there is no signal to rely on.
            if let Some(active) = active {
                if let Ok(active) = u32::try_from(active) {
                    if (active as usize) < wanted {
                        self.notebook.set_current_page(Some(active));
                    }
                }
            }

            // Record the post-sync page order for the reorder handler.
            let mut order = self.order.borrow_mut();
            order.clear();
            order.extend(
                (0..self.notebook.n_pages()).filter_map(|i| self.notebook.nth_page(Some(i))),
            );
        });
    }

    /// The index `child` occupied *before* the drag that just moved it.
    ///
    /// `None` if the child is not one this strip knows about, which
    /// should not happen but is not worth panicking across a GTK signal
    /// frame for.
    pub fn index_before_reorder(&self, child: &gtk::Widget) -> Option<usize> {
        self.order.borrow().iter().position(|w| w == child)
    }
}

/// Decode one of the embedded PNGs at `scale`.
///
/// Returns `None` rather than panicking if decoding fails: a tab
/// without its icon is a cosmetic loss, and taking the process down
/// because a bundled asset would not parse is not proportionate. The
/// Win32 side degrades the same way — "tab-save … icon decode failed;
/// tab strip will paint without it".
fn tab_icon(dirty: bool, scale: i32) -> Option<Pixbuf> {
    let bytes = match (dirty, scale >= 2) {
        (false, false) => PNG_TAB_SAVE,
        (false, true) => PNG_TAB_SAVE_2X,
        (true, false) => PNG_TAB_SAVE_DIRTY,
        (true, true) => PNG_TAB_SAVE_DIRTY_2X,
    };
    match Pixbuf::from_read(Cursor::new(bytes)) {
        Ok(pixbuf) => Some(pixbuf),
        Err(err) => {
            tracing::warn!(%err, dirty, "tab icon decode failed; tab renders without it");
            None
        }
    }
}

/// Build the widget shown on one tab.
///
/// Wrapped in a `gtk::EventBox` because a plain `gtk::Label` has no
/// `GdkWindow` and so receives no button events — without it there is
/// nowhere for middle-click-to-close to land. `set_visible_window(false)`
/// keeps the box input-only, so it does not paint over the theme's own
/// tab background.
fn build_tab_label(tab: &Tab, scale: i32) -> gtk::Widget {
    let ebox = gtk::EventBox::new();
    ebox.set_visible_window(false);
    ebox.add_events(gtk::gdk::EventMask::BUTTON_PRESS_MASK);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);

    if let Some(pixbuf) = tab_icon(tab.dirty, scale) {
        let image = gtk::Image::from_pixbuf(Some(&pixbuf));
        // The @2x asset is twice the logical size; pin the logical
        // square so both scales occupy the same space in the strip.
        image.set_pixel_size(ICON_LOGICAL_PX);
        row.pack_start(&image, false, false, 0);
    }

    // `tab_display_name`, not `tab.path` directly: it is the shared
    // resolver both backends render from, it honours `custom_name` and
    // `untitled_seq`, and it sanitizes. Filenames are attacker-
    // influenced — a plugin picks one via `NPPM_DOOPEN` — and an
    // unsanitized one here could break the label across lines or use a
    // bidi override to make the tab name a different file than the one
    // it opens.
    let name = tab_display_name(tab);
    let label = gtk::Label::new(Some(&name));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // Both bounds are required, and only setting the max is a bug that
    // renders every tab as a bare "…". Turning ellipsization on drops a
    // `GtkLabel`'s *minimum* width request to about one ellipsis, so
    // inside a `GtkNotebook` tab — which sizes itself from what its
    // child asks for — the label asks for nothing, gets nothing, and
    // has nothing left to draw but the ellipsis. `set_width_chars`
    // restores a floor; `set_max_width_chars` keeps a long name from
    // pushing the strip wider than the window.
    label.set_width_chars(LABEL_MIN_CHARS);
    label.set_max_width_chars(LABEL_MAX_CHARS);
    label.set_tooltip_text(Some(&name));
    // No `expand`: the label should occupy what it requests between
    // those two bounds, not absorb slack from the notebook.
    row.pack_start(&label, false, false, 0);

    // Both gestures below capture `id`, never an index — see the
    // module docs for why.
    let id = tab.id;

    let close = gtk::Button::new();
    close.set_relief(gtk::ReliefStyle::None);
    close.set_focus_on_click(false);
    close.set_tooltip_text(Some("Close"));
    close.add(&gtk::Image::from_icon_name(
        Some("window-close-symbolic"),
        gtk::IconSize::Menu,
    ));
    close.connect_clicked(move |_| crate::close_tab_by_id(id));
    row.pack_start(&close, false, false, 0);

    ebox.connect_button_press_event(move |_, ev| {
        if ev.button() == MIDDLE_BUTTON {
            crate::close_tab_by_id(id);
            return glib::Propagation::Stop;
        }
        // Anything else falls through to the notebook, which turns a
        // left-click into the `switch-page` that selects this tab.
        glib::Propagation::Proceed
    });

    ebox.add(&row);
    ebox.show_all();
    ebox.upcast()
}
