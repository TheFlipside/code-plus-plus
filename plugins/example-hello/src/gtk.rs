//! The GTK 3 calls the Linux side of this plugin makes — the dock panels
//! ([`crate::dock`]) and the modeless dialog ([`crate::dialog`]) — and
//! the test for whether GTK is up to make anything at all.
//!
//! Declared here rather than taken from a binding crate, as the Windows
//! side does for `user32`: a demo plugin needs a handful of functions,
//! and a plugin that a third party might copy as a starting point is
//! better off showing the dependency-free shape. The libraries are the
//! ones the host has already loaded, so linking them adds nothing to the
//! process.

use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

// Each declaration is the C function of the same name, with its C types:
// a `GObject` or widget as `*mut c_void`, `gboolean` and the enums as
// `c_int`, `guint` as `c_uint`, `gsize` as `usize`.

// GDK: whether a display is open.
#[link(name = "gdk-3")]
extern "C" {
    fn gdk_display_get_default() -> *mut c_void;
}

// gdk-pixbuf: the toolbar icon, drawn pixel by pixel.
#[link(name = "gdk_pixbuf-2.0")]
extern "C" {
    pub(crate) fn gdk_pixbuf_new(
        colorspace: c_int,
        has_alpha: c_int,
        bits_per_sample: c_int,
        width: c_int,
        height: c_int,
    ) -> *mut c_void;
    pub(crate) fn gdk_pixbuf_fill(pixbuf: *mut c_void, pixel: u32);
    pub(crate) fn gdk_pixbuf_get_byte_length(pixbuf: *mut c_void) -> usize;
    pub(crate) fn gdk_pixbuf_get_pixels(pixbuf: *mut c_void) -> *mut u8;
    pub(crate) fn gdk_pixbuf_get_rowstride(pixbuf: *mut c_void) -> c_int;
}

// GObject: references, and the one signal connected.
#[link(name = "gobject-2.0")]
extern "C" {
    pub(crate) fn g_object_ref_sink(object: *mut c_void) -> *mut c_void;
    pub(crate) fn g_object_unref(object: *mut c_void);
    pub(crate) fn g_signal_connect_data(
        instance: *mut c_void,
        signal: *const c_char,
        handler: *const c_void,
        data: *mut c_void,
        destroy_data: *const c_void,
        flags: c_uint,
    ) -> c_ulong;
}

// GTK: the panels' boxes and labels, and the dialog.
#[link(name = "gtk-3")]
extern "C" {
    pub(crate) fn gtk_box_new(orientation: c_int, spacing: c_int) -> *mut c_void;
    pub(crate) fn gtk_container_add(container: *mut c_void, widget: *mut c_void);
    pub(crate) fn gtk_container_set_border_width(container: *mut c_void, width: c_uint);
    pub(crate) fn gtk_entry_new() -> *mut c_void;
    pub(crate) fn gtk_entry_set_placeholder_text(entry: *mut c_void, text: *const c_char);
    pub(crate) fn gtk_label_new(text: *const c_char) -> *mut c_void;
    pub(crate) fn gtk_label_set_line_wrap(label: *mut c_void, wrap: c_int);
    pub(crate) fn gtk_label_set_xalign(label: *mut c_void, xalign: f32);
    pub(crate) fn gtk_widget_hide_on_delete(widget: *mut c_void) -> c_int;
    pub(crate) fn gtk_widget_set_hexpand(widget: *mut c_void, expand: c_int);
    pub(crate) fn gtk_widget_set_vexpand(widget: *mut c_void, expand: c_int);
    pub(crate) fn gtk_widget_show(widget: *mut c_void);
    pub(crate) fn gtk_widget_show_all(widget: *mut c_void);
    pub(crate) fn gtk_window_new(kind: c_int) -> *mut c_void;
    pub(crate) fn gtk_window_present(window: *mut c_void);
    pub(crate) fn gtk_window_set_default_size(window: *mut c_void, width: c_int, height: c_int);
    pub(crate) fn gtk_window_set_position(window: *mut c_void, position: c_int);
    pub(crate) fn gtk_window_set_title(window: *mut c_void, title: *const c_char);
}

/// `GTK_ORIENTATION_VERTICAL`.
pub(crate) const VERTICAL: c_int = 1;
/// `GTK_WINDOW_TOPLEVEL`.
pub(crate) const WINDOW_TOPLEVEL: c_int = 0;
/// `GTK_WIN_POS_CENTER_ON_PARENT`.
pub(crate) const WIN_POS_CENTER_ON_PARENT: c_int = 4;
/// `GDK_COLORSPACE_RGB`.
pub(crate) const COLORSPACE_RGB: c_int = 0;

/// Whether GTK is up to build widgets with: a default display is open.
/// Always so inside the GTK host; a host that loads plugins without a
/// display — a headless test harness — would otherwise have GTK abort
/// the process at the first widget.
pub(crate) fn ready() -> bool {
    // SAFETY: takes nothing; answers null until a display is open.
    !unsafe { gdk_display_get_default() }.is_null()
}
