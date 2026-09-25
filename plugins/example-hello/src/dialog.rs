//! A modeless dialog, registered with the host through
//! `NPPM_MODELESSDIALOG` — the demo for that message.
//!
//! On Windows a plugin registers its modeless dialog so the host's message
//! pump calls `IsDialogMessage` for it; without that, Tab does not move
//! between the dialog's controls. AppKit does that for every window, so on
//! macOS the host checks the handle and answers it, and registering
//! changes nothing — which this dialog shows: "Show Modeless Dialog"
//! opens it, Tab moves between its two fields, and the status bar reports
//! what the host answered. It is unregistered at `NPPN_SHUTDOWN`, as the
//! ABI asks: removal comes before a dialog is released.
//!
//! macOS only. Elsewhere the command says so on the status bar.

#[cfg(target_os = "macos")]
pub use appkit::{show, shutdown};
#[cfg(not(target_os = "macos"))]
pub use elsewhere::{show, shutdown};

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

/// Everywhere but macOS: the command says where the demo is.
#[cfg(not(target_os = "macos"))]
mod elsewhere {
    use codepp_plugin_sdk as sdk;

    pub fn show() {
        sdk::set_status("Example Hello: the modeless dialog is demonstrated on macOS");
    }

    /// Nothing was registered.
    pub fn shutdown() {}
}
