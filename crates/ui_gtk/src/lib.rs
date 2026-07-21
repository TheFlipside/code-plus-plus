//! GTK 3 UI backend for Code++.
//!
//! Phase 5 m1 scope: bring the Linux target to parity with what Phase 1
//! demonstrated on Win32 — a real window hosting a real Scintilla
//! control, with typing, undo/redo, select-all and Scintilla's built-in
//! context menu all working against the statically-linked vendored
//! engine. `Shell`, tabs, the status bar, dialogs and the plugin host
//! arrive in later milestones; this crate deliberately does not
//! implement `codepp_shell::UiPlatform` yet. (Plain code spans, not
//! intra-doc links, for the cross-crate references in this file —
//! `ui_gtk` does not depend on `shell` or `ui_win32` in m1, so links
//! to them would be unresolvable and would warn on `cargo doc`.)
//!
//! # Why GTK 3
//!
//! Scintilla ships no GTK 4 backend — see
//! `crates/scintilla-sys/build.rs::build_scintilla_gtk` for the evidence
//! and DESIGN.md §4.1 for the amended decision. GTK 3.24 is the final,
//! API-frozen GTK 3 series, so this is a stable target.
//!
//! # Why no `gtk::Application`
//!
//! [`gtk::Application`] wraps `GApplication`, which registers on the
//! session D-Bus at startup and performs single-instance arbitration.
//! Code++'s cold-start budget is 80 ms (DESIGN.md §8) and none of that
//! machinery is on the critical path to the first frame, so this
//! backend uses the lower-level `gtk::init` + `gtk::main` pair instead.
//! Same reasoning as `ui_win32` calling `CreateWindowExW` directly
//! rather than adopting a framework.

#![cfg(target_os = "linux")]

use std::fmt;
use std::path::PathBuf;

use gtk::glib::translate::FromGlibPtrNone;
use gtk::prelude::*;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::scintilla_new;

/// Initial window size, in logical pixels. GTK scales this for `HiDPI`.
const DEFAULT_WIDTH: i32 = 1024;
/// See [`DEFAULT_WIDTH`].
const DEFAULT_HEIGHT: i32 = 768;

/// Width of the line-number gutter, in pixels. Wide enough for five
/// digits at the 11 pt default; m2 replaces this with the same
/// content-measured sizing `ui_win32` does.
const LINE_NUMBER_MARGIN_PX: i32 = 44;

/// Everything that can go wrong bringing the GTK backend up.
///
/// Deliberately small: each variant is a hard setup failure with no
/// recovery path, so the binary reports it and exits non-zero.
#[derive(Debug)]
pub enum GtkUiError {
    /// `gtk_init_check` failed — almost always "no display available"
    /// (running headless, or `DISPLAY`/`WAYLAND_DISPLAY` unset).
    GtkInit,
    /// `scintilla_new()` returned null. Means the vendored engine
    /// failed to construct its widget; not a user-recoverable state.
    ScintillaCreate,
    /// The widget was created but would not surrender its direct-call
    /// `(fn_ptr, instance_ptr)` pair. Continuing would mean routing
    /// every keystroke through a slower fallback path that DESIGN.md
    /// §4.2 forbids, so this is fatal rather than degraded.
    DirectCallCapture,
}

impl fmt::Display for GtkUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GtkInit => write!(
                f,
                "failed to initialise GTK — is a display available? \
                 (DISPLAY / WAYLAND_DISPLAY unset when running headless)"
            ),
            Self::ScintillaCreate => write!(f, "scintilla_new() returned null"),
            Self::DirectCallCapture => write!(
                f,
                "Scintilla did not return a direct-call function/instance pair"
            ),
        }
    }
}

impl std::error::Error for GtkUiError {}

/// Build the window, wire the Scintilla control, and run the GTK main
/// loop until the user closes the window.
///
/// `initial_path` is accepted so the entry point's signature already
/// matches `codepp_ui_win32::run`, but it is **not** opened yet:
/// loading a file goes through `core::file::Loader` and the §5.4
/// cross-thread marshaling pattern, both of which live behind `Shell`.
/// That wiring is the next milestone. Rather than drop the argument
/// silently, this logs a warning so a user passing a path gets told
/// why nothing opened.
///
/// # Errors
///
/// Returns [`GtkUiError`] if GTK will not initialise, if Scintilla will
/// not construct its widget, or if the direct-call pair cannot be
/// captured. All three are fatal setup failures.
pub fn run(initial_path: Option<PathBuf>) -> Result<(), GtkUiError> {
    // Log the underlying `BoolError` before collapsing it: the
    // `Display` impl on `GtkUiError::GtkInit` names the overwhelmingly
    // likely cause (no display), which would misreport any other
    // failure mode if the real message were discarded entirely.
    gtk::init().map_err(|err| {
        tracing::error!(%err, "gtk::init failed");
        GtkUiError::GtkInit
    })?;

    if let Some(path) = initial_path {
        tracing::warn!(
            path = %path.display(),
            "opening a file from the command line is not wired on GTK yet \
             (needs Shell; Phase 5 m2) — starting with an empty buffer"
        );
    }

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("Code++");
    window.set_default_size(DEFAULT_WIDTH, DEFAULT_HEIGHT);

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    window.add(&layout);

    // Menu bar. The full Notepad++ menu set lands with `Shell`; m1
    // carries File → Exit only, mirroring the Phase 0/1 Win32 demo.
    layout.pack_start(&build_menu_bar(), false, false, 0);

    // The Scintilla widget itself.
    //
    // SAFETY: `gtk::init` succeeded above, which is `scintilla_new`'s
    // only precondition.
    let sci_ptr = unsafe { scintilla_new() };
    if sci_ptr.is_null() {
        return Err(GtkUiError::ScintillaCreate);
    }

    // Adopt the raw `GtkWidget*` into gtk-rs. `from_glib_none` is the
    // correct transfer mode for a `*_new()` constructor: the pointer
    // carries a floating reference, and this is the same call every
    // gtk-rs widget constructor makes when wrapping its own C
    // `gtk_*_new()`. Packing it below sinks the float.
    //
    // SAFETY: `sci_ptr` is a non-null widget that `scintilla_new` just
    // returned and nothing has unreffed since.
    let sci_widget = unsafe { gtk::Widget::from_glib_none(sci_ptr.cast::<gtk::ffi::GtkWidget>()) };
    // `expand`/`fill` true: the editor takes all vertical space the
    // menu bar leaves.
    layout.pack_start(&sci_widget, true, true, 0);

    // Capture the direct-call pair once, here, per DESIGN.md §4.2 —
    // every hot-path operation from this point on bypasses GTK's
    // signal machinery entirely.
    //
    // SAFETY: `sci_ptr` is the live widget just packed into `layout`,
    // which holds a reference to it for the rest of the process.
    let editor =
        unsafe { EditorHandle::from_gtk_widget(sci_ptr) }.ok_or(GtkUiError::DirectCallCapture)?;
    apply_baseline_style(&editor);
    tracing::debug!("captured Scintilla direct-call pair");

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });

    window.show_all();
    // Focus the editor so the demo's first keystroke lands in the
    // buffer rather than on the menu bar.
    sci_widget.grab_focus();

    gtk::main();

    Ok(())
}

/// Seed the editor's appearance so the m1 demo opens on a monospace
/// buffer with a line-number margin, rather than GTK's proportional
/// default.
///
/// This is intentionally *not* the full styling path — `styles.xml`,
/// the Style Configurator and the per-language themes all arrive with
/// `Shell` in m2, via `codepp_shell::UiPlatform::apply_default_style`.
/// What it does do is exercise the direct-call pointer immediately
/// after capture: if the `(fn_ptr, instance_ptr)` pair were wrong, the
/// window would fail here at startup rather than on the user's first
/// keystroke.
fn apply_baseline_style(editor: &EditorHandle) {
    use codepp_scintilla_sys::{SCI_STYLECLEARALL, SC_MARGIN_NUMBER, STYLE_DEFAULT};

    editor.style_set_font(STYLE_DEFAULT, "monospace");
    editor.style_set_size(STYLE_DEFAULT, 11);
    // Propagate the default to every other style index — the same
    // sequencing `ui_win32` uses, and a hard requirement: styles set
    // before `SCI_STYLECLEARALL` survive, styles set after do not.
    editor.send(SCI_STYLECLEARALL, 0, 0);

    // Margin 0 as a line-number gutter, matching Notepad++'s default.
    editor.set_margin_type(0, SC_MARGIN_NUMBER);
    editor.set_margin_width(0, LINE_NUMBER_MARGIN_PX);
}

/// File → Exit, the one menu m1 needs. Split out so the m2 expansion to
/// the full Notepad++ menu set has an obvious seam.
fn build_menu_bar() -> gtk::MenuBar {
    let bar = gtk::MenuBar::new();

    let file_menu = gtk::Menu::new();
    let exit_item = gtk::MenuItem::with_mnemonic("E_xit");
    exit_item.connect_activate(|_| gtk::main_quit());
    file_menu.append(&exit_item);

    let file_root = gtk::MenuItem::with_mnemonic("_File");
    file_root.set_submenu(Some(&file_menu));
    bar.append(&file_root);

    bar
}
