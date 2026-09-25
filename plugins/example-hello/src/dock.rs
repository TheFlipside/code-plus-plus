//! A real docking panel, so the host's `NPPM_DMM*` surface has
//! something in-tree to be demonstrated against.
//!
//! Nothing else in the workspace registers one — the four bundled
//! plugins are all buffer-text transforms — so before this existed
//! the docking messages could only be exercised by downloading a
//! third-party Notepad++ plugin. Under DESIGN.md §7.1 that is not a
//! demo anyone can re-run on a clean machine.
//!
//! What it demonstrates, in the order a user would click:
//!
//! 1. **"Show Dock Panel"** creates the panel's content — a plain
//!    child window on Windows (as a real Notepad++ plugin does, except
//!    a real one usually builds it from a dialog template), a GTK
//!    widget on Linux, an `NSView` on macOS — hands it to the host with
//!    `NPPM_DMMREGASDCKDLG`, and shows it with `NPPM_DMMSHOW`. The
//!    host hosts it as a dock panel; this content is only ever a child
//!    of the host's container. The menu item is ticked
//!    (`NPPM_SETMENUITEMCHECK`) while the panel is open.
//! 2. **"Rename Dock Panel"** re-points the *same* `tTbData`'s
//!    `psz_name` at a different static string and sends
//!    `NPPM_DMMUPDATEDISPINFO`. The host takes the new name as the
//!    panel's lookup key — "Switch To Other Dock Panel" then finds the
//!    panel by it — while the caption keeps the identity the panel was
//!    registered under, which is what its saved position is keyed on.
//! 3. **Closing the panel** with the ✕ on its caption makes the host
//!    send `DMN_CLOSE` — *not* through `beNotified` — and the panel
//!    reports it on the status bar and unticks its menu item. On
//!    Windows it arrives as an ordinary `WM_NOTIFY` at this panel's
//!    window procedure; on Linux and macOS, where a widget or a view
//!    has no window procedure, as the same `WM_NOTIFY` at this plugin's
//!    own `messageProc`, `wParam` naming the panel.
//! 4. **Quitting with a panel open and starting again** brings it
//!    back, and the two panels come back two different ways. The
//!    first is registered from `NPPN_TBMODIFICATION`, so its content
//!    is there before anything else happens. The second is only ever
//!    registered from its own menu command, the way `NppExec`'s
//!    console is — and it still comes back, because the host runs
//!    that command at startup: `tTbData.dlgID` names it, and
//!    Notepad++ restores every panel that way.
//!
//! Linux and macOS share everything but how the content is built —
//! [`hosted`] registers, shows, renames and hears the `DMN_*`, and a
//! small module per toolkit builds the widget or the view. Windows has a
//! module of its own, since there the content is a window with its own
//! procedure.

#[cfg(target_os = "windows")]
pub use win::{
    message, register_panels, rename_panel, show_panel, show_second_panel, view_other_tab,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use hosted::{
    message, register_panels, rename_panel, show_panel, show_second_panel, view_other_tab,
};

/// Linux and macOS: the panels are toolkit objects — a `GtkWidget*` or
/// an `NSView*` — which the host adopts as a panel's content where a
/// Windows host adopts a window, taking its own reference and moving the
/// object between its dock containers; this module never frees it.
///
/// Everything here is the same on both, because the host's contract is
/// the same on both: registration by `NPPM_DMMREGASDCKDLG`, the `DMN_*`
/// at `messageProc`. What differs — building the content, and whether
/// the toolkit is up to build it with — is in [`view`], one module per
/// toolkit.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod hosted {
    use codepp_plugin_sdk::{self as sdk, Hwnd, SciNotifyHeader, SyncCell, TbData, TbRect};
    use core::ffi::{c_void, CStr};
    use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    #[cfg(target_os = "macos")]
    use super::cocoa_view as view;
    #[cfg(target_os = "linux")]
    use super::gtk_view as view;

    // ---- Static payloads ----------------------------------------
    //
    // `menu_label` NUL-pads to a fixed width, which makes each of
    // these a valid null-terminated wide string as long as the text is
    // shorter than the array — so they double as `psz_*` buffers.

    const TITLE_A: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"Example Hello Panel");
    const TITLE_B: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"Example Hello Panel (renamed)");
    /// The second panel exists so `NPPM_DMMVIEWOTHERTAB` has something
    /// to switch *to*: the message means "bring that panel to the front
    /// of the container it shares", which needs two panels to be
    /// observable at all.
    const TITLE_2: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"Example Hello Notes");
    const LABEL_TEXT: &CStr = c"Close me to fire DMN_CLOSE.";
    const LABEL_TEXT_2: &CStr = c"Drag my tab onto the other panel.";

    /// The registration payload. It lives in a `static` because the
    /// host keeps the pointer: `NPPM_DMMUPDATEDISPINFO` re-reads this
    /// struct, so a stack temporary would leave the host holding a
    /// dangling pointer the moment the menu handler returned.
    static TB_DATA: SyncCell<TbData> = SyncCell::new(TbData {
        h_client: core::ptr::null_mut(),
        psz_name: core::ptr::null(),
        // The command that opens this panel.
        dlg_id: crate::imp::CMD_SHOW_DOCK_PANEL,
        // Both demo panels ask for the *bottom container*: two panels
        // naming the same one become two tabs of a single dock group,
        // which is the arrangement `NPPM_DMMVIEWOTHERTAB` switches
        // between.
        u_mask: sdk::DWS_DF_CONT_BOTTOM,
        h_icon_tab: core::ptr::null_mut(),
        psz_add_info: core::ptr::null(),
        rc_float: TbRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        i_prev_cont: -1,
        psz_module_name: core::ptr::null(),
    });

    /// This plugin's panel content — the `hClient` the host knows it by.
    /// Null until it is first needed. From then on the plugin holds a
    /// reference of its own for the rest of the process, so the pointer
    /// here names a live object whatever else happens to it.
    static PANEL: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    /// Whether `NPPM_DMMREGASDCKDLG` has been accepted. Registering the
    /// same content twice is refused, so this keeps a second "Show Dock
    /// Panel" to a plain `NPPM_DMMSHOW`.
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    /// Which of the two titles `psz_name` currently points at.
    static RENAMED: AtomicBool = AtomicBool::new(false);

    /// The second panel's payload, content and registration — same
    /// lifetime rules as the first.
    static TB_DATA_2: SyncCell<TbData> = SyncCell::new(TbData {
        h_client: core::ptr::null_mut(),
        psz_name: core::ptr::null(),
        dlg_id: crate::imp::CMD_SHOW_SECOND_DOCK_PANEL,
        u_mask: sdk::DWS_DF_CONT_BOTTOM,
        h_icon_tab: core::ptr::null_mut(),
        psz_add_info: core::ptr::null(),
        rc_float: TbRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        i_prev_cont: -1,
        psz_module_name: core::ptr::null(),
    });
    static PANEL_2: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    static REGISTERED_2: AtomicBool = AtomicBool::new(false);

    /// Build a panel's content on first use. Returns it, or null if the
    /// toolkit is not up or could not make it.
    fn create_in(slot: &'static AtomicPtr<c_void>, text: &CStr) -> Hwnd {
        let existing = slot.load(Ordering::Acquire);
        if !existing.is_null() {
            return existing;
        }
        if !view::ready() {
            return core::ptr::null_mut();
        }
        let panel = view::build(text);
        if !panel.is_null() {
            slot.store(panel, Ordering::Release);
        }
        panel
    }

    /// Create and register one panel, without showing it. Idempotent
    /// through `registered`, because the host refuses a second
    /// registration of the same content. Returns the content, or null if
    /// it could not be created or the host refused it.
    fn register_one(
        slot: &'static AtomicPtr<c_void>,
        text: &CStr,
        tb_data: &'static SyncCell<TbData>,
        title: *const u16,
        registered: &'static AtomicBool,
    ) -> Hwnd {
        let panel = create_in(slot, text);
        if panel.is_null() || registered.load(Ordering::Acquire) {
            return panel;
        }
        // SAFETY: single-threaded — notifications and menu commands
        // both run on the host's UI thread, and this is the only writer.
        unsafe {
            let tb = tb_data.get();
            (*tb).h_client = panel;
            (*tb).psz_name = title;
            (*tb).psz_module_name = view::MODULE_NAME.as_ptr();
        }
        // SAFETY: the `tTbData` is a live `static` for the process's
        // whole life, which is exactly the lifetime the host's
        // `DockDialogParams::tb_data` contract asks for.
        let ok = unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMREGASDCKDLG,
                0,
                tb_data.get().cast_const() as isize,
            )
        };
        if ok == 0 {
            return core::ptr::null_mut();
        }
        registered.store(true, Ordering::Release);
        panel
    }

    /// Register the first panel with the docking manager, without
    /// showing it. Called from `NPPN_TBMODIFICATION`, which is the
    /// moment the ABI sets aside for it. The second panel is registered
    /// only by its own menu command — see the Windows arm, which keeps
    /// the same split for the same reason.
    pub fn register_panels() {
        // Silently, with no toolkit to build with: that is a host with
        // nowhere to put a panel, not a refusal worth reporting.
        if !view::ready() {
            return;
        }
        let a = register_one(&PANEL, LABEL_TEXT, &TB_DATA, TITLE_A.as_ptr(), &REGISTERED);
        if a.is_null() {
            sdk::set_status("Example Hello: the host refused a dock registration");
        }
    }

    /// Show a registered panel and tick the menu item that opens it.
    fn show(panel: Hwnd, item: i32, done: &str) {
        // SAFETY: `panel` is the registered `hClient`; this message takes
        // it by value, not by pointer.
        unsafe {
            sdk::SendMessageW(sdk::npp_handle(), sdk::NPPM_DMMSHOW, 0, panel as isize);
        }
        crate::imp::set_item_check(item, true);
        sdk::set_status(done);
    }

    pub fn show_panel() {
        let panel = register_one(&PANEL, LABEL_TEXT, &TB_DATA, TITLE_A.as_ptr(), &REGISTERED);
        if panel.is_null() {
            sdk::set_status("Example Hello: could not open the dock panel");
            return;
        }
        show(
            panel,
            crate::imp::CMD_SHOW_DOCK_PANEL,
            "Example Hello: dock panel shown",
        );
    }

    /// Create, register and show the second panel — a near-copy of
    /// [`show_panel`] on purpose, since what it demonstrates is that a
    /// plugin may register *several* panels and the host keeps them
    /// distinct.
    pub fn show_second_panel() {
        let panel = register_one(
            &PANEL_2,
            LABEL_TEXT_2,
            &TB_DATA_2,
            TITLE_2.as_ptr(),
            &REGISTERED_2,
        );
        if panel.is_null() {
            sdk::set_status("Example Hello: could not open the second panel");
            return;
        }
        show(
            panel,
            crate::imp::CMD_SHOW_SECOND_DOCK_PANEL,
            "Example Hello: second dock panel shown",
        );
    }

    /// Ask the host to bring the *first* panel to the front of whatever
    /// container it is in — `NPPM_DMMVIEWOTHERTAB`, by the name the
    /// panel currently has.
    pub fn view_other_tab() {
        let name = if RENAMED.load(Ordering::Acquire) {
            TITLE_B.as_ptr()
        } else {
            TITLE_A.as_ptr()
        };
        // SAFETY: the title arrays are `static` and NUL-terminated; the
        // host reads the string during the call and does not retain it.
        let shown = unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMVIEWOTHERTAB,
                0,
                name as isize,
            )
        };
        if shown == 0 {
            sdk::set_status("Example Hello: the host knows no panel by that name");
        } else {
            // The host shows the panel if it was closed, so its menu
            // item is ticked here exactly as after "Show Dock Panel".
            crate::imp::set_item_check(crate::imp::CMD_SHOW_DOCK_PANEL, true);
            sdk::set_status("Example Hello: switched to the other panel");
        }
    }

    /// Re-point `psz_name` at the other title and ask the host to
    /// notice — exactly what `NPPM_DMMUPDATEDISPINFO` exists for.
    pub fn rename_panel() {
        if !REGISTERED.load(Ordering::Acquire) {
            sdk::set_status("Example Hello: show the dock panel first");
            return;
        }
        let renamed = !RENAMED.load(Ordering::Acquire);
        // SAFETY: single-threaded, as in `register_one`.
        unsafe {
            (*TB_DATA.get()).psz_name = if renamed {
                TITLE_B.as_ptr()
            } else {
                TITLE_A.as_ptr()
            };
        }
        RENAMED.store(renamed, Ordering::Release);
        let panel = PANEL.load(Ordering::Acquire);
        // SAFETY: `panel` is the registered `hClient`.
        unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMUPDATEDISPINFO,
                0,
                panel as isize,
            );
        }
        sdk::set_status("Example Hello: dock panel renamed");
    }

    /// The host's `DMN_*` about this plugin's panels, which on these
    /// platforms arrive at `messageProc` as `WM_NOTIFY` with the panel's
    /// content in `wParam` — the host has no window procedure to send
    /// them to. `lParam` is the same `NMHDR` a Windows plugin gets.
    pub fn message(msg: u32, wparam: usize, lparam: isize) -> isize {
        // A zero `wParam` names no panel: the host sends registered
        // content, never null — and a panel not created yet reads as null
        // below, so this also keeps one from matching it.
        if msg != sdk::WM_NOTIFY || lparam == 0 || wparam == 0 {
            return 0;
        }
        let item = if wparam == PANEL.load(Ordering::Acquire) as usize {
            crate::imp::CMD_SHOW_DOCK_PANEL
        } else if wparam == PANEL_2.load(Ordering::Acquire) as usize {
            crate::imp::CMD_SHOW_SECOND_DOCK_PANEL
        } else {
            return 0;
        };
        // SAFETY: by the host's contract `lParam` points at an `NMHDR`
        // that stays live for this call.
        let (from, code) = unsafe {
            let nmhdr = lparam as *const SciNotifyHeader;
            ((*nmhdr).hwnd_from, (*nmhdr).code)
        };
        // Accepted only from the host's main handle, as Notepad++'s
        // docking-dialog template accepts them — which makes the demo a
        // test of the field, not only of the code.
        if from != sdk::npp_handle() {
            return 0;
        }
        // Switch on the low word: DMN_DOCK and DMN_FLOAT carry their
        // container number in the high one.
        match code & 0xFFFF {
            sdk::DMN_CLOSE => {
                crate::imp::set_item_check(item, false);
                sdk::set_status("Example Hello: panel closed (DMN_CLOSE received)");
            }
            sdk::DMN_DOCK => sdk::set_status(&format!(
                "Example Hello: panel docked (DMN_DOCK, container {})",
                code >> 16
            )),
            sdk::DMN_FLOAT => sdk::set_status(&format!(
                "Example Hello: panel floating (DMN_FLOAT, container {})",
                code >> 16
            )),
            _ => {}
        }
        0
    }
}

/// The Linux content: a GTK widget.
///
/// A recompiled plugin hands the GTK host a `GtkWidget*` as `hClient`
/// where a Windows one hands it a dialog `HWND` — unparented, and not a
/// window of its own. The widget's children are shown here
/// (`gtk_widget_show_all`); the widget itself is the host's to show and
/// hide.
///
/// The few GTK calls are declared here rather than taken from a binding
/// crate, as the Windows arm does for `user32`: a demo panel is a
/// handful of functions, and a plugin that a third party might copy as
/// a starting point is better off showing the dependency-free shape.
/// `libgtk-3` is the library the host has already loaded, so linking it
/// adds nothing to the process.
#[cfg(target_os = "linux")]
mod gtk_view {
    use codepp_plugin_sdk::{self as sdk, Hwnd};
    use core::ffi::{c_char, c_int, c_uint, c_void, CStr};

    #[link(name = "gdk-3")]
    extern "C" {
        fn gdk_display_get_default() -> *mut c_void;
    }

    #[link(name = "gobject-2.0")]
    extern "C" {
        fn g_object_ref_sink(object: *mut c_void) -> *mut c_void;
    }

    #[link(name = "gtk-3")]
    extern "C" {
        fn gtk_box_new(orientation: c_int, spacing: c_int) -> *mut c_void;
        fn gtk_container_add(container: *mut c_void, widget: *mut c_void);
        fn gtk_container_set_border_width(container: *mut c_void, width: c_uint);
        fn gtk_label_new(text: *const c_char) -> *mut c_void;
        fn gtk_label_set_line_wrap(label: *mut c_void, wrap: c_int);
        fn gtk_label_set_xalign(label: *mut c_void, xalign: f32);
        fn gtk_widget_show_all(widget: *mut c_void);
    }

    /// `GTK_ORIENTATION_VERTICAL`.
    const VERTICAL: c_int = 1;
    /// Inner margin around the label, in pixels.
    const LABEL_INSET: c_uint = 8;

    /// `tTbData.pszModuleName`: the plugin's own file name, extension
    /// included — the convention Notepad++ keeps with `.dll`, and what
    /// the host matches a restored panel to its plugin by. No `lib`
    /// prefix, although Cargo builds `libexample_hello.so`: staging
    /// drops it, so the installed file is `example_hello/example_hello.so`,
    /// the layout discovery requires. Both spellings read as one plugin
    /// anyway (`codepp_core::shortcuts::module_key`), so adding the prefix
    /// here would change nothing but the name persisted.
    pub(super) const MODULE_NAME: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"example_hello.so");

    /// Whether GTK is up to build widgets with: a default display is
    /// open. Always so inside the GTK host; a host that loads plugins
    /// without a display — a headless test harness — would otherwise
    /// have GTK abort the process at the first widget.
    pub(super) fn ready() -> bool {
        // SAFETY: takes nothing; answers null until a display is open.
        !unsafe { gdk_display_get_default() }.is_null()
    }

    /// A box with a label in it, held by a reference of the plugin's own
    /// — never released. Null if GTK could not make it.
    pub(super) fn build(text: &CStr) -> Hwnd {
        // SAFETY: plain GTK calls on the UI thread — a plugin menu
        // command or notification runs there by the ABI's contract, and
        // the host has initialised GTK. `text` is a NUL-terminated
        // static string; every widget passed on is one just created.
        unsafe {
            let panel = gtk_box_new(VERTICAL, 0);
            if panel.is_null() {
                return core::ptr::null_mut();
            }
            // Sink the floating reference, making it the plugin's own —
            // never released. The host takes a reference of its own when
            // it adopts the widget.
            g_object_ref_sink(panel);
            gtk_container_set_border_width(panel, LABEL_INSET);
            let label = gtk_label_new(text.as_ptr());
            if !label.is_null() {
                gtk_label_set_xalign(label, 0.0);
                gtk_label_set_line_wrap(label, 1);
                gtk_container_add(panel, label);
            }
            // Shown, children and all: the host shows and hides the
            // container it puts the panel in, never the panel.
            gtk_widget_show_all(panel);
            panel
        }
    }
}

/// The macOS content: an `NSView` holding a wrapping label.
///
/// A recompiled plugin hands the Cocoa host an `NSView*` as `hClient` —
/// in no superview, and not a window's. The host takes its own
/// reference, sizes the view to fill the panel through its autoresizing
/// mask, and shows it; it never releases the plugin's reference, which
/// this module holds for the process.
///
/// Built on the Objective-C runtime directly — `objc_getClass`,
/// `sel_registerName`, `objc_msgSend` — for the reason the GTK arm
/// declares its handful of GTK calls: a demo panel is a handful of
/// messages, and a plugin a third party might copy is better off
/// showing the dependency-free shape. Only `libobjc` is linked. The
/// AppKit classes are looked up by name, so a process that has not
/// loaded AppKit — a test harness — finds none and builds nothing, where
/// linking AppKit would have loaded it.
#[cfg(target_os = "macos")]
mod cocoa_view {
    use codepp_plugin_sdk::{self as sdk, Hwnd};
    use core::ffi::{c_char, c_int, c_void, CStr};

    type Id = *mut c_void;
    type Sel = *const c_void;

    /// `CGPoint` / `CGSize` / `CGRect`, which `NSRect` is.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        /// Never called as declared: each use is cast to the prototype
        /// of the method it sends — see [`send`].
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    extern "C" {
        /// `libSystem`: nonzero on the process's main thread.
        fn pthread_main_np() -> c_int;
    }

    /// `tTbData.pszModuleName`: the plugin's own file name, extension
    /// included — the convention Notepad++ keeps with `.dll`, and what
    /// the host matches a restored panel to its plugin by. No `lib`
    /// prefix, although Cargo builds `libexample_hello.dylib`: staging
    /// drops it, so the installed file is `example_hello/example_hello.dylib`,
    /// the layout discovery requires. Both spellings read as one plugin
    /// anyway (`codepp_core::shortcuts::module_key`), so adding the prefix
    /// here would change nothing but the name persisted.
    pub(super) const MODULE_NAME: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"example_hello.dylib");

    /// `NSViewWidthSizable | NSViewHeightSizable`.
    const WIDTH_AND_HEIGHT_SIZABLE: usize = 2 | 16;
    /// The panel's starting size, in points; the host resizes it.
    const PANEL_WIDTH: f64 = 240.0;
    const PANEL_HEIGHT: f64 = 120.0;
    /// Inner margin around the label, in points.
    const LABEL_INSET: f64 = 8.0;

    /// `objc_msgSend` as the prototype `F` of the method being sent.
    ///
    /// On arm64 the untyped symbol cannot be called as declared: a
    /// variadic call passes its arguments differently from the method's
    /// own prototype. So every send goes through an exact function type,
    /// and none of them returns a structure — which on `x86_64` would
    /// need `objc_msgSend_stret` instead.
    ///
    /// # Safety
    ///
    /// `F` must be an `unsafe extern "C" fn(Id, Sel, …)` type matching
    /// the method it will be called with, argument for argument.
    unsafe fn send<F: Copy>() -> F {
        // Catches an `F` that is not a function pointer at all — a value
        // type passed by mistake. It cannot catch the wrong prototype:
        // every function pointer is one word, so that stays the caller's
        // contract above.
        debug_assert_eq!(
            core::mem::size_of::<F>(),
            core::mem::size_of::<unsafe extern "C" fn()>(),
            "`send` casts a function pointer to a function pointer, nothing else"
        );
        // SAFETY: a function pointer, reinterpreted as another function
        // pointer of the same size — the caller's contract makes the
        // signature the method's own.
        unsafe { core::mem::transmute_copy::<unsafe extern "C" fn(), F>(&(objc_msgSend as _)) }
    }

    /// The selector named `name`.
    fn sel(name: &CStr) -> Sel {
        // SAFETY: registers or finds a selector by its NUL-terminated
        // name; never fails.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// Whether AppKit is up to build views with: this is the main thread,
    /// and the process has loaded AppKit. Always so inside the Cocoa host;
    /// a host that loads plugins elsewhere — a test harness on a worker
    /// thread — gets no view built, since AppKit's views are main-thread
    /// only.
    pub(super) fn ready() -> bool {
        // SAFETY: both read process state and take nothing but a static
        // NUL-terminated class name.
        unsafe { pthread_main_np() != 0 && !objc_getClass(c"NSView".as_ptr()).is_null() }
    }

    /// An `NSView` with a wrapping label filling it, less an inset, held
    /// by the reference `alloc`/`init` returns — the plugin's own, never
    /// released. Null if AppKit could not make it.
    pub(super) fn build(text: &CStr) -> Hwnd {
        // SAFETY: plain AppKit messages on the main thread (`ready`
        // checked it), each sent through the exact prototype of its
        // method, to classes looked up by name and checked for null, and
        // to objects just created. The autorelease pool bounds the two
        // autoreleased objects — the string and the label — to this call;
        // the view holds the label past it.
        unsafe {
            let view_class = objc_getClass(c"NSView".as_ptr());
            let label_class = objc_getClass(c"NSTextField".as_ptr());
            let string_class = objc_getClass(c"NSString".as_ptr());
            if view_class.is_null() || label_class.is_null() || string_class.is_null() {
                return core::ptr::null_mut();
            }
            let alloc: unsafe extern "C" fn(Id, Sel) -> Id = send();
            let init_with_frame: unsafe extern "C" fn(Id, Sel, Rect) -> Id = send();
            let with_utf8: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id = send();
            let with_string: unsafe extern "C" fn(Id, Sel, Id) -> Id = send();
            let set_frame: unsafe extern "C" fn(Id, Sel, Rect) = send();
            let set_mask: unsafe extern "C" fn(Id, Sel, usize) = send();
            let add_subview: unsafe extern "C" fn(Id, Sel, Id) = send();

            let pool = objc_autoreleasePoolPush();
            let panel = init_with_frame(
                alloc(view_class, sel(c"alloc")),
                sel(c"initWithFrame:"),
                rect(0.0, 0.0, PANEL_WIDTH, PANEL_HEIGHT),
            );
            if !panel.is_null() {
                let string = with_utf8(string_class, sel(c"stringWithUTF8String:"), text.as_ptr());
                let label = with_string(label_class, sel(c"wrappingLabelWithString:"), string);
                if !label.is_null() {
                    set_frame(
                        label,
                        sel(c"setFrame:"),
                        rect(
                            LABEL_INSET,
                            LABEL_INSET,
                            PANEL_WIDTH - 2.0 * LABEL_INSET,
                            PANEL_HEIGHT - 2.0 * LABEL_INSET,
                        ),
                    );
                    set_mask(
                        label,
                        sel(c"setAutoresizingMask:"),
                        WIDTH_AND_HEIGHT_SIZABLE,
                    );
                    add_subview(panel, sel(c"addSubview:"), label);
                }
            }
            objc_autoreleasePoolPop(pool);
            panel
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use codepp_plugin_sdk::{self as sdk, Hwnd, SciNotifyHeader, SyncCell, TbData, TbRect};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

    // ---- The slice of Win32 this panel needs ---------------------
    //
    // Declared here rather than by depending on the `windows` crate:
    // a demo panel is four API calls, the SDK already establishes
    // this idiom for `SendMessageW`, and a plugin that a third party
    // might copy as a starting point is better off showing the
    // dependency-free shape. `user32` is already linked into this
    // cdylib through the SDK's own import.

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        instance: *mut c_void,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassExW(class: *const WndClassExW) -> u16;
        #[allow(clippy::too_many_arguments)]
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: Hwnd,
            menu: *mut c_void,
            instance: *mut c_void,
            param: *mut c_void,
        ) -> Hwnd;
        fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn MoveWindow(hwnd: Hwnd, x: i32, y: i32, w: i32, h: i32, repaint: i32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        /// Second parameter is `LPCWSTR lpModuleName` in the general
        /// case, but with `FROM_ADDRESS` it is an address inside the
        /// wanted module — which is the only way this is called, so
        /// it is declared as the pointer it actually receives.
        fn GetModuleHandleExW(flags: u32, address: *const c_void, module: *mut *mut c_void) -> i32;
    }

    /// `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` — resolve the module
    /// that *contains* the given address rather than the one that
    /// started the process.
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const SS_LEFT: u32 = 0x0000_0000;
    const WM_SIZE: u32 = 0x0005;
    /// Inner margin for the child label, in pixels.
    const LABEL_INSET: i32 = 8;

    // ---- Static payloads ----------------------------------------
    //
    // `menu_label` NUL-pads to a fixed width, which makes each of
    // these a valid null-terminated wide string as long as the text
    // is shorter than the array — so they double as `psz_*` buffers.

    const CLASS_NAME: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"CodeppExampleHelloPanel");
    /// `tTbData.pszModuleName`: the plugin's own DLL file name,
    /// extension included, which is the Notepad++ convention and what
    /// the host matches a restored panel to its plugin by.
    const MODULE_NAME: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"example_hello.dll");
    const TITLE_A: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"Example Hello Panel");
    const TITLE_B: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"Example Hello Panel (renamed)");
    const LABEL_TEXT: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"Close me to fire DMN_CLOSE.");
    /// The second panel exists so `NPPM_DMMVIEWOTHERTAB` has
    /// something to switch *to*: the message means "bring that panel
    /// to the front of the container it shares", which needs two
    /// panels to be observable at all.
    const TITLE_2: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"Example Hello Notes");
    const LABEL_TEXT_2: [u16; sdk::MENU_TITLE_LENGTH] =
        sdk::menu_label(b"Drag my tab onto the other panel.");

    /// The registration payload. It lives in a `static` because the
    /// host keeps the pointer: `NPPM_DMMUPDATEDISPINFO` re-reads this
    /// struct, so a stack temporary would leave the host holding a
    /// dangling pointer the moment the menu handler returned. Real
    /// Notepad++ plugins keep theirs as a member of the dialog
    /// object for the same reason.
    static TB_DATA: SyncCell<TbData> = SyncCell::new(TbData {
        h_client: core::ptr::null_mut(),
        psz_name: core::ptr::null(),
        // The command that opens this panel — see the constant.
        dlg_id: crate::imp::CMD_SHOW_DOCK_PANEL,
        // Both demo panels ask for the *bottom container*, which is
        // what `DWS_DF_CONT_*` names (see the SDK's re-export). Two
        // panels naming the same one become two tabs of a single dock
        // group, which is the arrangement `NPPM_DMMVIEWOTHERTAB`
        // exists to switch between.
        u_mask: sdk::DWS_DF_CONT_BOTTOM,
        h_icon_tab: core::ptr::null_mut(),
        psz_add_info: core::ptr::null(),
        rc_float: TbRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        i_prev_cont: -1,
        psz_module_name: core::ptr::null(),
    });

    /// This plugin's panel HWND — the `h_client` the host knows it
    /// by. Null until "Show Dock Panel" is first clicked; the panel
    /// is never destroyed after that (the host's frame outlives it
    /// and the process exits with both).
    static PANEL: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    /// The child label inside the panel, resized on `WM_SIZE`.
    static LABEL: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    /// Whether the window class has been registered yet.
    static CLASS_READY: AtomicBool = AtomicBool::new(false);
    /// Whether `NPPM_DMMREGASDCKDLG` has been accepted. Registering
    /// the same `h_client` twice is rejected by the host, so this
    /// keeps the second "Show Dock Panel" click to a plain
    /// `NPPM_DMMSHOW`.
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    /// Which of the two titles `psz_name` currently points at.
    static RENAMED: AtomicBool = AtomicBool::new(false);

    /// The second panel's registration payload, its window, and
    /// whether the host has accepted it. Same lifetime rules as the
    /// first — `TB_DATA_2` is `static` because the host keeps the
    /// pointer.
    static TB_DATA_2: SyncCell<TbData> = SyncCell::new(TbData {
        h_client: core::ptr::null_mut(),
        psz_name: core::ptr::null(),
        dlg_id: crate::imp::CMD_SHOW_SECOND_DOCK_PANEL,
        // The same container as the first panel — see there.
        u_mask: sdk::DWS_DF_CONT_BOTTOM,
        h_icon_tab: core::ptr::null_mut(),
        psz_add_info: core::ptr::null(),
        rc_float: TbRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        i_prev_cont: -1,
        psz_module_name: core::ptr::null(),
    });
    static PANEL_2: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
    static REGISTERED_2: AtomicBool = AtomicBool::new(false);

    /// Window procedure for the panel.
    ///
    /// Wrapped in `catch_unwind` because it is a foreign entry point
    /// and `set_status` allocates: an unwind out of an
    /// `extern "system"` function is at best a defined abort, and
    /// here it would abort the *host*, not just the plugin.
    unsafe extern "system" fn panel_wnd_proc(
        hwnd: Hwnd,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
            WM_SIZE => {
                let label = LABEL.load(Ordering::Acquire);
                if !label.is_null() {
                    let mut rc = Rect::default();
                    // SAFETY: `hwnd` is this live window; `rc` is a
                    // valid out-parameter for the duration.
                    if unsafe { GetClientRect(hwnd, &raw mut rc) } != 0 {
                        // SAFETY: `label` is a live child window of
                        // `hwnd` — only ever written by `create`.
                        unsafe {
                            MoveWindow(
                                label,
                                LABEL_INSET,
                                LABEL_INSET,
                                (rc.right - LABEL_INSET * 2).max(0),
                                (rc.bottom - LABEL_INSET * 2).max(0),
                                1,
                            );
                        }
                    }
                }
                Some(0)
            }
            sdk::WM_NOTIFY => {
                // The DMN_* codes arrive here, as ordinary WM_NOTIFYs
                // on this window, because that is how Notepad++
                // delivers them — a plugin that waited for them in
                // `beNotified` would wait forever.
                let nmhdr = lparam as *const SciNotifyHeader;
                if !nmhdr.is_null() {
                    // SAFETY: per the WM_NOTIFY contract, `lparam` is
                    // a pointer to at least an NMHDR, live for this
                    // call.
                    let (from, code) = unsafe { ((*nmhdr).hwnd_from, (*nmhdr).code) };
                    // All three come from the host's *main* window,
                    // and are accepted only from there — the way
                    // Notepad++'s docking-dialog template checks it,
                    // which is what makes this demo a test of the field
                    // and not only of the code: a host sending these
                    // from anywhere else reaches the template-built
                    // plugins that make up most of the ecosystem not at
                    // all.
                    if from != sdk::npp_handle() {
                        return Some(0);
                    }
                    // Switch on the low word: DMN_DOCK and DMN_FLOAT
                    // carry their container number in the high one.
                    match code & 0xFFFF {
                        sdk::DMN_CLOSE => {
                            let item = if hwnd == PANEL_2.load(Ordering::Acquire) {
                                crate::imp::CMD_SHOW_SECOND_DOCK_PANEL
                            } else {
                                crate::imp::CMD_SHOW_DOCK_PANEL
                            };
                            crate::imp::set_item_check(item, false);
                            sdk::set_status("Example Hello: panel closed (DMN_CLOSE received)");
                        }
                        sdk::DMN_DOCK => {
                            sdk::set_status(&format!(
                                "Example Hello: panel docked (DMN_DOCK, container {})",
                                code >> 16
                            ));
                        }
                        sdk::DMN_FLOAT => {
                            sdk::set_status(&format!(
                                "Example Hello: panel floating (DMN_FLOAT, container {})",
                                code >> 16
                            ));
                        }
                        _ => {}
                    }
                }
                Some(0)
            }
            _ => None,
        }));
        match handled {
            Ok(Some(result)) => result,
            // A panic lands here as `Err` and falls through to
            // `DefWindowProcW`, which is the correct default for
            // every message this proc does not claim.
            Ok(None) | Err(_) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// Create the panel window on first use. Returns its HWND, or
    /// null if any step failed.
    fn create_in(slot: &'static AtomicPtr<c_void>, label: *const u16) -> Hwnd {
        let existing = slot.load(Ordering::Acquire);
        if !existing.is_null() {
            return existing;
        }
        let npp = sdk::npp_handle();
        if npp.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: every call below is an ordinary Win32 window
        // creation on the UI thread — a plugin menu command runs on
        // the host's UI thread by the ABI's own contract. The
        // pointers handed over all point at `static` data.
        unsafe {
            // This plugin's own module, resolved from the address of
            // the window procedure. `GetModuleHandleW(NULL)` would
            // hand back the *host executable* — which is the usual
            // mistake here, and a real one: Windows ties a window
            // class's lifetime to the module it was registered
            // against, and the procedure it points at lives in this
            // DLL. Registering under the exe would mean the class
            // outlives an unloaded plugin and its second
            // registration then fails.
            //
            // Deliberately without `UNCHANGED_REFCOUNT`: the
            // reference this takes pins the DLL for as long as the
            // window class exists, which is exactly the guarantee a
            // live `wnd_proc` needs.
            let mut instance: *mut c_void = core::ptr::null_mut();
            if GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                panel_wnd_proc as *const c_void,
                &raw mut instance,
            ) == 0
            {
                return core::ptr::null_mut();
            }
            if !CLASS_READY.swap(true, Ordering::AcqRel) {
                let class = WndClassExW {
                    cb_size: core::mem::size_of::<WndClassExW>() as u32,
                    style: 0,
                    wnd_proc: Some(panel_wnd_proc),
                    cb_cls_extra: 0,
                    cb_wnd_extra: 0,
                    instance,
                    icon: core::ptr::null_mut(),
                    cursor: core::ptr::null_mut(),
                    // `COLOR_BTNFACE + 1` as an HBRUSH — the
                    // documented way to ask for a system-colour
                    // background without creating a GDI object.
                    background: (15 + 1) as *mut c_void,
                    menu_name: core::ptr::null(),
                    class_name: CLASS_NAME.as_ptr(),
                    icon_sm: core::ptr::null_mut(),
                };
                if RegisterClassExW(&raw const class) == 0 {
                    // Clear the latch again: unlike the window-
                    // creation failure below, nothing was
                    // registered, so a retry must re-attempt it
                    // rather than create against a missing class.
                    CLASS_READY.store(false, Ordering::Release);
                    // Distinguishable from the other failure paths:
                    // without this the user only ever sees the
                    // generic "could not create" message below.
                    sdk::set_status("Example Hello: dock panel class registration failed");
                    return core::ptr::null_mut();
                }
            }
            // Created as a child of the host window, invisible: the
            // host re-parents it into its own frame at registration
            // and shows it there. Its size does not matter — the
            // frame resizes it to fill the client area.
            let panel = CreateWindowExW(
                0,
                CLASS_NAME.as_ptr(),
                core::ptr::null(),
                WS_CHILD,
                0,
                0,
                200,
                120,
                npp,
                core::ptr::null_mut(),
                instance,
                core::ptr::null_mut(),
            );
            if panel.is_null() {
                // Leave `CLASS_READY` set: the class registration
                // itself succeeded and re-registering would fail.
                return core::ptr::null_mut();
            }
            let label_hwnd = CreateWindowExW(
                0,
                sdk::menu_label(b"STATIC").as_ptr(),
                label,
                WS_CHILD | WS_VISIBLE | SS_LEFT,
                LABEL_INSET,
                LABEL_INSET,
                180,
                100,
                panel,
                core::ptr::null_mut(),
                instance,
                core::ptr::null_mut(),
            );
            LABEL.store(label_hwnd, Ordering::Release);
            slot.store(panel, Ordering::Release);
            panel
        }
    }

    /// Create and register one panel, without showing it. Idempotent
    /// through `registered`, because the host rejects a second
    /// registration of the same `h_client`.
    ///
    /// Returns the panel window, or null if it could not be created
    /// or the host refused the registration.
    fn register_one(
        slot: &'static AtomicPtr<c_void>,
        label: *const u16,
        tb_data: &'static SyncCell<TbData>,
        title: *const u16,
        registered: &'static AtomicBool,
    ) -> Hwnd {
        let panel = create_in(slot, label);
        if panel.is_null() || registered.load(Ordering::Acquire) {
            return panel;
        }
        // Fill in the parts of the registration that are only known
        // now. The `tTbData` outlives this call by being `static`,
        // which is what lets the host re-read it on
        // `NPPM_DMMUPDATEDISPINFO`.
        //
        // SAFETY: single-threaded — notifications and menu commands
        // both run on the host's UI thread, and this is the only
        // writer.
        unsafe {
            let tb = tb_data.get();
            (*tb).h_client = panel;
            (*tb).psz_name = title;
            (*tb).psz_module_name = MODULE_NAME.as_ptr();
        }
        // SAFETY: the `tTbData` is a live `static` for the process's
        // whole life, which is exactly the lifetime the host's
        // `DockDialogParams::tb_data` contract asks for.
        let ok = unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMREGASDCKDLG,
                0,
                tb_data.get().cast_const() as isize,
            )
        };
        if ok == 0 {
            return core::ptr::null_mut();
        }
        registered.store(true, Ordering::Release);
        panel
    }

    /// Register the first panel with the docking manager, without
    /// showing it. Called from `NPPN_TBMODIFICATION`, which is the
    /// moment the ABI sets aside for it.
    ///
    /// The second panel is deliberately *not* registered here: it is
    /// registered only by its own menu command, which is how many real
    /// plugins do it (`NppExec`'s console among them). The two together
    /// are the demo for how the host brings a panel back. Both come
    /// back after a restart — the first because its window exists by
    /// the time the host looks, the second because the host runs
    /// `FuncItem[dlgID]` for every panel that was open, which is what
    /// Notepad++ does and what a lazily-registering plugin relies on.
    /// Either way the host runs the command, so the plugin's own state
    /// ends up the same as if the user had clicked.
    pub fn register_panels() {
        let a = register_one(
            &PANEL,
            LABEL_TEXT.as_ptr(),
            &TB_DATA,
            TITLE_A.as_ptr(),
            &REGISTERED,
        );
        if a.is_null() {
            sdk::set_status("Example Hello: the host refused a dock registration");
        }
    }

    pub fn show_panel() {
        let panel = register_one(
            &PANEL,
            LABEL_TEXT.as_ptr(),
            &TB_DATA,
            TITLE_A.as_ptr(),
            &REGISTERED,
        );
        if panel.is_null() {
            sdk::set_status("Example Hello: could not open the dock panel");
            return;
        }
        // SAFETY: `panel` is the registered `h_client`; this message
        // takes it by value, not by pointer.
        unsafe {
            sdk::SendMessageW(sdk::npp_handle(), sdk::NPPM_DMMSHOW, 0, panel as isize);
        }
        crate::imp::set_item_check(crate::imp::CMD_SHOW_DOCK_PANEL, true);
        sdk::set_status("Example Hello: dock panel shown");
    }

    /// Create, register and show the second panel.
    ///
    /// Identical to [`show_panel`] but for its own `tTbData`, window
    /// and title — deliberately a near-copy rather than a shared
    /// helper, because what it demonstrates is that a plugin may
    /// register *several* panels and the host keeps them distinct.
    pub fn show_second_panel() {
        let panel = register_one(
            &PANEL_2,
            LABEL_TEXT_2.as_ptr(),
            &TB_DATA_2,
            TITLE_2.as_ptr(),
            &REGISTERED_2,
        );
        if panel.is_null() {
            sdk::set_status("Example Hello: could not open the second panel");
            return;
        }
        // SAFETY: `panel` is the registered `h_client`.
        unsafe {
            sdk::SendMessageW(sdk::npp_handle(), sdk::NPPM_DMMSHOW, 0, panel as isize);
        }
        crate::imp::set_item_check(crate::imp::CMD_SHOW_SECOND_DOCK_PANEL, true);
        sdk::set_status("Example Hello: second dock panel shown");
    }

    /// Ask the host to bring the *first* panel to the front of
    /// whatever container it is in — `NPPM_DMMVIEWOTHERTAB`.
    ///
    /// Drag one panel's tab onto the other first and the two share a
    /// container; this then switches the visible tab, which is what
    /// the message is for. With them in separate containers it still
    /// does the useful half — brings the named panel into view.
    pub fn view_other_tab() {
        // The name is the plugin's own `pszName`, which is how the
        // host indexes the panel.
        let name = if RENAMED.load(Ordering::Acquire) {
            TITLE_B.as_ptr()
        } else {
            TITLE_A.as_ptr()
        };
        // SAFETY: the title arrays are `static` and NUL-terminated;
        // the host reads the string during the call and does not
        // retain it.
        let shown = unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMVIEWOTHERTAB,
                0,
                name as isize,
            )
        };
        if shown == 0 {
            sdk::set_status("Example Hello: the host knows no panel by that name");
        } else {
            // The host shows the panel if it was closed, so its menu
            // item is ticked here exactly as after "Show Dock Panel".
            crate::imp::set_item_check(crate::imp::CMD_SHOW_DOCK_PANEL, true);
            sdk::set_status("Example Hello: switched to the other panel");
        }
    }

    /// Nothing reaches `messageProc` for the panels here: on Windows the
    /// `DMN_*` arrive at the panel's own window procedure,
    /// `panel_wnd_proc`.
    pub fn message(_msg: u32, _wparam: usize, _lparam: isize) -> isize {
        0
    }

    /// Re-point `psz_name` at the other title and ask the host to
    /// notice. The registration is untouched — this is exactly what
    /// `NPPM_DMMUPDATEDISPINFO` exists for.
    pub fn rename_panel() {
        if !REGISTERED.load(Ordering::Acquire) {
            sdk::set_status("Example Hello: show the dock panel first");
            return;
        }
        let renamed = !RENAMED.load(Ordering::Acquire);
        // SAFETY: single-threaded, as in `show_panel`.
        unsafe {
            (*TB_DATA.get()).psz_name = if renamed {
                TITLE_B.as_ptr()
            } else {
                TITLE_A.as_ptr()
            };
        }
        RENAMED.store(renamed, Ordering::Release);
        let panel = PANEL.load(Ordering::Acquire);
        // SAFETY: `panel` is the registered `h_client`.
        unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_DMMUPDATEDISPINFO,
                0,
                panel as isize,
            );
        }
        sdk::set_status("Example Hello: dock panel renamed");
    }
}
