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
//! 1. **"Show Dock Panel"** creates a plain child window (as a real
//!    Notepad++ plugin does, except a real one usually builds it
//!    from a dialog template), hands it to the host with
//!    `NPPM_DMMREGASDCKDLG`, and shows it with `NPPM_DMMSHOW`. The
//!    host wraps it in a frame of its own; this window is only ever
//!    a child of that frame.
//! 2. **"Rename Dock Panel"** re-points the *same* `tTbData`'s
//!    `psz_name` at a different static string and sends
//!    `NPPM_DMMUPDATEDISPINFO`. The frame caption changes, which is
//!    observable and is the whole content of that message.
//! 3. **Closing the panel** with the frame's ✕ makes the host send
//!    `DMN_CLOSE`, which arrives here as an ordinary `WM_NOTIFY` on
//!    this window — *not* through `beNotified` — and the panel
//!    reports it on the status bar.
//!
//! Windows-only: `h_client` is an HWND, and `register_dock_dialog`
//! is defaulted (and so declines) on the GTK and Cocoa backends. The
//! menu items exist on every platform and say so rather than
//! disappearing, which keeps one `FuncItem` array across all three.

#[cfg(target_os = "windows")]
pub use win::{register_panels, rename_panel, show_panel, show_second_panel, view_other_tab};

#[cfg(not(target_os = "windows"))]
pub use stub::{register_panels, rename_panel, show_panel, show_second_panel, view_other_tab};

/// The non-Windows arm. `NPPM_DMMREGASDCKDLG` carries an HWND, so
/// there is nothing to send; DESIGN.md §7.4 tracks what a
/// plugin-owned panel should even mean on those backends.
#[cfg(not(target_os = "windows"))]
mod stub {
    use codepp_plugin_sdk as sdk;

    pub fn show_panel() {
        sdk::set_status("Example Hello: docking panels are Windows-only in this build");
    }

    pub fn rename_panel() {
        show_panel();
    }

    pub fn show_second_panel() {
        show_panel();
    }

    pub fn view_other_tab() {
        show_panel();
    }

    pub fn register_panels() {}
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
    const MODULE_NAME: [u16; sdk::MENU_TITLE_LENGTH] = sdk::menu_label(b"example_hello");
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
        dlg_id: 0,
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
        dlg_id: 1,
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
                // DMN_CLOSE arrives here, as an ordinary WM_NOTIFY
                // on this window, because that is how Notepad++
                // delivers it — a plugin that waited for it in
                // `beNotified` would wait forever. `hwndFrom` is the
                // host's frame and `idFrom` is 0, so the code is the
                // only field worth reading.
                let nmhdr = lparam as *const SciNotifyHeader;
                // SAFETY: per the WM_NOTIFY contract, `lparam` is a
                // pointer to at least an NMHDR, live for this call.
                if !nmhdr.is_null() && unsafe { (*nmhdr).code } == sdk::DMN_CLOSE {
                    sdk::set_status("Example Hello: panel closed (DMN_CLOSE received)");
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

    /// Register both panels with the docking manager, showing
    /// neither. Called from `NPPN_TBMODIFICATION`, which is the
    /// moment the ABI sets aside for it.
    ///
    /// The timing is the point rather than an implementation detail.
    /// The host restores its dock arrangement from `session.xml`
    /// before any plugin loads, so a panel the user had docked last
    /// session already has a group waiting and is missing only its
    /// content window; registering here fills it the moment the
    /// plugin loads. Registering on the menu click instead — which an
    /// earlier version of this file did — leaves that group visibly
    /// empty until the user clicks an item they have no reason to
    /// connect with it.
    pub fn register_panels() {
        let a = register_one(
            &PANEL,
            LABEL_TEXT.as_ptr(),
            &TB_DATA,
            TITLE_A.as_ptr(),
            &REGISTERED,
        );
        let b = register_one(
            &PANEL_2,
            LABEL_TEXT_2.as_ptr(),
            &TB_DATA_2,
            TITLE_2.as_ptr(),
            &REGISTERED_2,
        );
        if a.is_null() || b.is_null() {
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
            sdk::set_status("Example Hello: switched to the other panel");
        }
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
        sdk::set_status("Example Hello: dock panel title updated");
    }
}
