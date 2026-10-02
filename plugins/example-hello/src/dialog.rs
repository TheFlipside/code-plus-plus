//! A modeless dialog, registered with the host through
//! `NPPM_MODELESSDIALOG` — the demo for that message.
//!
//! On Windows a plugin registers its modeless dialog so the host's message
//! pump calls `IsDialogMessage` for it; without that, Tab does not move
//! between the dialog's controls. AppKit and GTK do that for every window,
//! so the hosts there check the handle and answer it. On macOS registering
//! changes nothing else; on Linux it makes the dialog transient for the
//! host's main window — what a Windows plugin gets by creating its dialog
//! with the npp handle as owner, which a GTK plugin cannot, since it is
//! never given the main window. This dialog shows that: "Show Modeless
//! Dialog" opens it, Tab moves between its two fields, on Linux it stays
//! above the editor, centred on it, and the status bar reports what the
//! host answered. It is unregistered at `NPPN_SHUTDOWN`, as the ABI asks:
//! removal comes before a dialog is released.
//!
//! On Linux the dialog also has an "Add a Note" button, whose GTK
//! `clicked` handler asks the host for a Scintilla widget in the dialog
//! (`NPPM_CREATESCINTILLAHANDLE`). Nothing of the host's is calling the
//! plugin when a button is clicked, and the widget is still the plugin's:
//! the request reached the host by the plugin's own route, so every edit
//! in the note arrives at `messageProc` as `SCN_MODIFIED`, and the status
//! bar reports the note's length.
//!
//! Linux and macOS. On Windows the command says so on the status bar.

#[cfg(target_os = "macos")]
pub use appkit::{show, shutdown};
#[cfg(target_os = "windows")]
pub use elsewhere::{show, shutdown};
#[cfg(target_os = "linux")]
pub use gtk_window::{note_notification, show, shutdown};

/// The dialog on Linux: a `GtkWindow` with two entries, made on first use
/// and kept for the process — closing it hides it, so the pointer stays
/// valid for the next "Show Modeless Dialog" and for the removal at
/// `NPPN_SHUTDOWN`. Registered before it is first shown, so the host's
/// transient-for is in place when the window maps and it opens centred on
/// the editor. The GTK calls come from [`crate::gtk`].
#[cfg(target_os = "linux")]
mod gtk_window {
    use codepp_plugin_sdk as sdk;
    use core::ffi::{c_int, c_void, CStr};
    use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    use crate::gtk::{
        g_signal_connect_data, gtk_box_new, gtk_button_new_with_label, gtk_container_add,
        gtk_container_set_border_width, gtk_entry_new, gtk_entry_set_placeholder_text,
        gtk_widget_hide_on_delete, gtk_widget_set_sensitive, gtk_widget_set_size_request,
        gtk_widget_show_all, gtk_window_new, gtk_window_present, gtk_window_set_default_size,
        gtk_window_set_position, gtk_window_set_title, ready, VERTICAL, WINDOW_TOPLEVEL,
        WIN_POS_CENTER_ON_PARENT,
    };

    /// The dialog's width and its border, in pixels — the macOS dialog's
    /// measures — and the gap between its fields.
    const WIDTH: c_int = 300;
    const INSET: u32 = 16;
    const GAP: c_int = 8;
    /// The note's height, in pixels: the dialog has no height of its own
    /// to share out, so the note asks for one.
    const NOTE_HEIGHT: c_int = 120;
    /// What the note starts out saying.
    const NOTE_TEXT: &CStr = c"A note, made by a click on the dialog's own button. Each edit \
reaches the plugin as SCN_MODIFIED, though no call of the host's was under way when it was asked \
for.";

    /// The dialog once made. GTK holds a toplevel window's reference
    /// itself, and closing only hides this one, so the pointer stays
    /// valid for the process. Nothing destroys the window; code that
    /// did would have to clear this first.
    static DIALOG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    /// Whether the host accepted the dialog's registration.
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    /// The note once made: the Scintilla widget the "Add a Note" button
    /// asked the host for, which names itself in its notifications'
    /// `nmhdr.hwndFrom`. Null until then, and for good where the host
    /// makes none. The host keeps the widget for the process.
    static NOTE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    /// Open the dialog, registering it with the host the first time.
    pub fn show() {
        if !ready() {
            return;
        }
        let dialog = dialog();
        if dialog.is_null() {
            sdk::set_status("Example Hello: could not make the modeless dialog");
            return;
        }
        if !REGISTERED.load(Ordering::Acquire) {
            // SAFETY: the npp handle and a live window of the plugin's
            // own; the host reads the pointer during the call.
            let answer = unsafe {
                sdk::SendMessageW(
                    sdk::npp_handle(),
                    sdk::NPPM_MODELESSDIALOG,
                    sdk::MODELESSDIALOGADD,
                    dialog as isize,
                )
            };
            if answer == dialog as isize {
                REGISTERED.store(true, Ordering::Release);
                sdk::set_status(
                    "Example Hello: modeless dialog registered (the host answered its handle)",
                );
            } else {
                sdk::set_status("Example Hello: the host refused the modeless dialog");
            }
        }
        // SAFETY: the live window, on the UI thread.
        unsafe {
            gtk_widget_show_all(dialog);
            gtk_window_present(dialog);
        }
    }

    /// Unregister the dialog, before the process lets it go —
    /// `NPPN_SHUTDOWN`.
    pub fn shutdown() {
        if REGISTERED.swap(false, Ordering::AcqRel) {
            // SAFETY: the npp handle and the window registered above,
            // still live.
            unsafe {
                sdk::SendMessageW(
                    sdk::npp_handle(),
                    sdk::NPPM_MODELESSDIALOG,
                    sdk::MODELESSDIALOGREMOVE,
                    DIALOG.load(Ordering::Acquire) as isize,
                );
            }
        }
    }

    /// The dialog, made on first use. Null if GTK could not make it.
    fn dialog() -> *mut c_void {
        let existing = DIALOG.load(Ordering::Acquire);
        if !existing.is_null() {
            return existing;
        }
        let made = build();
        DIALOG.store(made, Ordering::Release);
        made
    }

    /// A window titled "Example Hello Dialog" with two entries, which
    /// closing hides rather than destroys.
    fn build() -> *mut c_void {
        // SAFETY: plain GTK calls on the UI thread, with widgets just made
        // and NUL-terminated static strings. `gtk_widget_hide_on_delete`
        // is GTK's own handler for `delete-event`, documented for exactly
        // this connection: it takes the window and ignores the event and
        // data the signal also passes.
        unsafe {
            let window = gtk_window_new(WINDOW_TOPLEVEL);
            if window.is_null() {
                return window;
            }
            gtk_window_set_title(window, c"Example Hello Dialog".as_ptr());
            gtk_window_set_default_size(window, WIDTH, -1);
            gtk_window_set_position(window, WIN_POS_CENTER_ON_PARENT);
            gtk_container_set_border_width(window, INSET);
            // Closing hides the window rather than destroying it, so the
            // pointer kept above stays good.
            g_signal_connect_data(
                window,
                c"delete-event".as_ptr(),
                gtk_widget_hide_on_delete as *const c_void,
                core::ptr::null_mut(),
                core::ptr::null(),
                0,
            );
            let column = gtk_box_new(VERTICAL, GAP);
            if !column.is_null() {
                add_entry(column, c"Type here, then press Tab");
                add_entry(column, c"Tab brings you here");
                add_note_button(column);
                gtk_container_add(window, column);
            }
            window
        }
    }

    /// An "Add a Note" button in `column`, whose `clicked` handler makes
    /// the note there ([`add_note`]).
    fn add_note_button(column: *mut c_void) {
        // SAFETY: a live box and a NUL-terminated static string; the
        // handler has the `clicked` signal's C signature, and the box it
        // is given lives as long as the window it is in, which is never
        // destroyed.
        unsafe {
            let button = gtk_button_new_with_label(c"Add a Note".as_ptr());
            if button.is_null() {
                return;
            }
            g_signal_connect_data(
                button,
                c"clicked".as_ptr(),
                add_note as *const c_void,
                column,
                core::ptr::null(),
                0,
            );
            gtk_container_add(column, button);
        }
    }

    /// The button's `clicked` handler: ask the host for a Scintilla widget
    /// in `column`, the dialog's box. GTK calls this from its main loop,
    /// with no call of the host's into the plugin under way — and the host
    /// still knows the widget is this plugin's, because the request reaches
    /// it by the plugin's own route, so its notifications arrive at
    /// `messageProc` ([`note_notification`]). One note, kept: every widget
    /// the host makes counts against the plugin's allowance for the rest of
    /// the process, so the button goes insensitive once it has made it.
    extern "C" fn add_note(button: *mut c_void, column: *mut c_void) {
        if !NOTE.load(Ordering::Acquire).is_null() {
            return;
        }
        let note = crate::dock::scintilla_in(column, NOTE_TEXT);
        if note.is_null() {
            sdk::set_status("Example Hello: the host made no note");
            return;
        }
        NOTE.store(note, Ordering::Release);
        // SAFETY: the widget the host just made and the button clicked,
        // both live, on the UI thread.
        unsafe {
            gtk_widget_set_size_request(note, -1, NOTE_HEIGHT);
            gtk_widget_set_sensitive(button, 0);
        }
        sdk::set_status("Example Hello: a note from the dialog's own button — type in it");
    }

    /// Whether `lparam` is a notification from the note, reporting its
    /// length on the status bar after each edit.
    pub fn note_notification(lparam: isize) -> bool {
        crate::dock::scintilla_notification(lparam, NOTE.load(Ordering::Acquire), |length| {
            sdk::set_status(&format!(
                "Example Hello: the dialog's note is {length} bytes (SCN_MODIFIED from a widget \
                 its own button asked for)"
            ));
        })
    }

    /// An entry with `placeholder` in it, in `column`.
    fn add_entry(column: *mut c_void, placeholder: &CStr) {
        // SAFETY: a live box; a NUL-terminated static string.
        unsafe {
            let entry = gtk_entry_new();
            if !entry.is_null() {
                gtk_entry_set_placeholder_text(entry, placeholder.as_ptr());
                gtk_container_add(column, entry);
            }
        }
    }
}

/// The dialog: an `NSPanel` with two text fields, made on first use and
/// kept for the process. Built on the Objective-C runtime directly — see
/// [`crate::objc`].
#[cfg(target_os = "macos")]
mod appkit {
    use codepp_plugin_sdk as sdk;
    use core::ffi::{c_void, CStr};
    use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    use crate::objc::{
        class, ready, rect, sel, send, string, with_pool, Id, ObjcBool, Rect, Sel, NO,
    };

    /// `NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
    /// NSWindowStyleMaskUtilityWindow`: a small floating panel with a
    /// close button.
    const STYLE: usize = 1 | 2 | 16;
    /// `NSBackingStoreBuffered`.
    const BUFFERED: usize = 2;
    /// The dialog's content size, and the fields' margin and height, in
    /// points.
    const WIDTH: f64 = 300.0;
    const HEIGHT: f64 = 96.0;
    const INSET: f64 = 16.0;
    const FIELD_HEIGHT: f64 = 24.0;

    /// The dialog once made. The plugin holds the reference
    /// `alloc`/`init` returned for the process, and the window is not
    /// released when closed, so the pointer stays valid.
    static DIALOG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    /// Whether the host accepted the dialog's registration.
    static REGISTERED: AtomicBool = AtomicBool::new(false);

    /// Open the dialog, registering it with the host the first time.
    pub fn show() {
        if !ready() {
            return;
        }
        let dialog = dialog();
        if dialog.is_null() {
            sdk::set_status("Example Hello: could not make the modeless dialog");
            return;
        }
        if !REGISTERED.load(Ordering::Acquire) {
            // SAFETY: the npp handle and a live window of the plugin's
            // own; the host reads the pointer during the call.
            let answer = unsafe {
                sdk::SendMessageW(
                    sdk::npp_handle(),
                    sdk::NPPM_MODELESSDIALOG,
                    sdk::MODELESSDIALOGADD,
                    dialog as isize,
                )
            };
            if answer == dialog as isize {
                REGISTERED.store(true, Ordering::Release);
                sdk::set_status(
                    "Example Hello: modeless dialog registered (the host answered its handle)",
                );
            } else {
                sdk::set_status("Example Hello: the host refused the modeless dialog");
            }
        }
        // SAFETY: `makeKeyAndOrderFront:`'s own prototype, on the main
        // thread, to the live window.
        unsafe {
            let front: unsafe extern "C" fn(Id, Sel, Id) = send();
            front(dialog, sel(c"makeKeyAndOrderFront:"), core::ptr::null_mut());
        }
    }

    /// Unregister the dialog, before the process lets it go —
    /// `NPPN_SHUTDOWN`.
    pub fn shutdown() {
        if REGISTERED.swap(false, Ordering::AcqRel) {
            // SAFETY: the npp handle and the window registered above,
            // still live.
            unsafe {
                sdk::SendMessageW(
                    sdk::npp_handle(),
                    sdk::NPPM_MODELESSDIALOG,
                    sdk::MODELESSDIALOGREMOVE,
                    DIALOG.load(Ordering::Acquire) as isize,
                );
            }
        }
    }

    /// The dialog, made on first use. Null if AppKit could not make it.
    fn dialog() -> Id {
        let existing = DIALOG.load(Ordering::Acquire);
        if !existing.is_null() {
            return existing;
        }
        let made = build();
        DIALOG.store(made, Ordering::Release);
        made
    }

    /// An `NSPanel` titled "Example Hello Dialog" with two fields, not
    /// released when closed.
    fn build() -> Id {
        let panel_class = class(c"NSPanel");
        if panel_class.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: each message sent through its method's own prototype, on
        // the main thread (`ready` checked it), to the class looked up
        // above and the panel just made.
        unsafe {
            let alloc: unsafe extern "C" fn(Id, Sel) -> Id = send();
            let init: unsafe extern "C" fn(Id, Sel, Rect, usize, usize, ObjcBool) -> Id = send();
            let set_released: unsafe extern "C" fn(Id, Sel, ObjcBool) = send();
            let panel = init(
                alloc(panel_class, sel(c"alloc")),
                sel(c"initWithContentRect:styleMask:backing:defer:"),
                rect(0.0, 0.0, WIDTH, HEIGHT),
                STYLE,
                BUFFERED,
                NO,
            );
            if panel.is_null() {
                return panel;
            }
            // Closing hides it; the pointer above stays good for the next
            // "Show Modeless Dialog" and for the removal at shutdown.
            set_released(panel, sel(c"setReleasedWhenClosed:"), NO);
            with_pool(|| fill(panel));
            panel
        }
    }

    /// The title and the two fields.
    ///
    /// # Safety
    ///
    /// `panel` is a live `NSPanel`, and this runs on the main thread
    /// inside an autorelease pool.
    unsafe fn fill(panel: Id) {
        // SAFETY: the caller's contract; each message through its own
        // prototype.
        unsafe {
            let set_title: unsafe extern "C" fn(Id, Sel, Id) = send();
            let content_view: unsafe extern "C" fn(Id, Sel) -> Id = send();
            let center: unsafe extern "C" fn(Id, Sel) = send();
            set_title(panel, sel(c"setTitle:"), string(c"Example Hello Dialog"));
            center(panel, sel(c"center"));
            let content = content_view(panel, sel(c"contentView"));
            if content.is_null() {
                return;
            }
            let top = HEIGHT - INSET - FIELD_HEIGHT;
            add_field(content, top, c"Type here, then press Tab");
            add_field(content, INSET, c"Tab brings you here");
        }
    }

    /// An editable text field across `content` at `y`, showing
    /// `placeholder` while empty.
    ///
    /// # Safety
    ///
    /// As [`fill`], for `content`.
    unsafe fn add_field(content: Id, y: f64, placeholder: &CStr) {
        let field_class = class(c"NSTextField");
        if field_class.is_null() {
            return;
        }
        // SAFETY: the caller's contract; each message through its own
        // prototype. The field comes back autoreleased and the content
        // view keeps it once it is added.
        unsafe {
            let with_string: unsafe extern "C" fn(Id, Sel, Id) -> Id = send();
            let set_placeholder: unsafe extern "C" fn(Id, Sel, Id) = send();
            let set_frame: unsafe extern "C" fn(Id, Sel, Rect) = send();
            let add_subview: unsafe extern "C" fn(Id, Sel, Id) = send();
            let field = with_string(field_class, sel(c"textFieldWithString:"), string(c""));
            if field.is_null() {
                return;
            }
            set_placeholder(field, sel(c"setPlaceholderString:"), string(placeholder));
            set_frame(
                field,
                sel(c"setFrame:"),
                rect(INSET, y, WIDTH - 2.0 * INSET, FIELD_HEIGHT),
            );
            add_subview(content, sel(c"addSubview:"), field);
        }
    }
}

/// Windows: the command says where the demo is.
#[cfg(target_os = "windows")]
mod elsewhere {
    use codepp_plugin_sdk as sdk;

    pub fn show() {
        sdk::set_status("Example Hello: the modeless dialog is demonstrated on Linux and macOS");
    }

    /// Nothing was registered.
    pub fn shutdown() {}
}
