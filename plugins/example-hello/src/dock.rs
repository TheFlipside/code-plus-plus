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
//! 5. **On macOS, the second panel — "Example Hello Notes" — is filled by
//!    a Scintilla view the host made for the plugin**
//!    (`NPPM_CREATESCINTILLAHANDLE`, with the panel as its parent). Typing
//!    in it reaches the plugin as `SCN_MODIFIED` at `messageProc`, and the
//!    status bar reports the notes' length. Where the host makes no view,
//!    the panel carries a label instead.
//! 6. **On macOS, "Show Dock Panel" has a toolbar button**
//!    (`NPPM_ADDTOOLBARICON`), added at `NPPN_TBMODIFICATION`. It runs
//!    the command and shows its check mark as pressed.
//! 7. **On macOS, switching between the two panels' tabs**, dragging a
//!    panel out to float or resizing its band reports what the host
//!    sends on the status bar: `DMN_SWITCHIN` for the panel coming in,
//!    `DMN_SWITCHOFF` for the one going behind it, and
//!    `DMN_FLOATDROPPED` for each panel laid out somewhere new — once
//!    a drag has ended, not at every step of it.
//!
//! Linux and macOS share everything but how the content is built —
//! [`hosted`] registers, shows, renames and hears the `DMN_*`, and a
//! small module per toolkit builds the widget or the view. Windows has a
//! module of its own, since there the content is a window with its own
//! procedure.

#[cfg(target_os = "windows")]
pub use win::{
    add_toolbar_button, message, register_panels, rename_panel, show_panel, show_second_panel,
    view_other_tab,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use hosted::{
    add_toolbar_button, message, register_panels, rename_panel, show_panel, show_second_panel,
    view_other_tab,
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
    use codepp_plugin_sdk::{
        self as sdk, Hwnd, SCNotification, SciNotifyHeader, SyncCell, TbData, TbRect,
    };
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
    /// What the Notes panel's Scintilla view starts out saying, where the
    /// host makes one (see [`view::build_notes`]).
    const NOTES_TEXT: &CStr = c"Notes: type here. This is a Scintilla view the host made for \
example-hello (NPPM_CREATESCINTILLAHANDLE), and every edit reaches the plugin as SCN_MODIFIED. \
Drag my tab onto the other panel.";

    /// `SCN_MODIFIED`, the edits `SC_MOD_INSERTTEXT | SC_MOD_DELETETEXT`
    /// among its kinds, and `SCI_GETLENGTH`: how the Notes view's edits
    /// are heard and measured.
    const SCN_MODIFIED: u32 = 2008;
    const SC_MOD_TEXT: i32 = 0x1 | 0x2;
    const SCI_GETLENGTH: u32 = 2006;

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
    /// The Scintilla view the host made for the Notes panel, whose
    /// notifications arrive at `messageProc`. Null until then, and for
    /// good on a host that makes none.
    static NOTES_SCI: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    /// Build the first panel's content on first use. Returns it, or null
    /// if the toolkit is not up or could not make it.
    fn create_panel() -> Hwnd {
        let existing = PANEL.load(Ordering::Acquire);
        if !existing.is_null() || !view::ready() {
            return existing;
        }
        let panel = view::build(LABEL_TEXT);
        PANEL.store(panel, Ordering::Release);
        panel
    }

    /// Build the Notes panel's content on first use — with a Scintilla
    /// view in it where the host makes one. Returns it, or null.
    fn create_notes() -> Hwnd {
        let existing = PANEL_2.load(Ordering::Acquire);
        if !existing.is_null() || !view::ready() {
            return existing;
        }
        let (panel, sci) = view::build_notes(LABEL_TEXT_2, NOTES_TEXT);
        NOTES_SCI.store(sci, Ordering::Release);
        PANEL_2.store(panel, Ordering::Release);
        panel
    }

    /// Register one panel's content, without showing it. Idempotent
    /// through `registered`, because the host refuses a second
    /// registration of the same content. Returns the content, or null if
    /// there is none or the host refused it.
    fn register_one(
        panel: Hwnd,
        tb_data: &'static SyncCell<TbData>,
        title: *const u16,
        registered: &'static AtomicBool,
    ) -> Hwnd {
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
        let a = register_one(create_panel(), &TB_DATA, TITLE_A.as_ptr(), &REGISTERED);
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

    /// Give "Show Dock Panel" a toolbar button — `NPPM_ADDTOOLBARICON`.
    /// Called from `NPPN_TBMODIFICATION`, the moment the ABI sets aside for
    /// it. On macOS; the GTK host takes no plugin toolbar buttons yet.
    pub fn add_toolbar_button() {
        if !view::ready() {
            return;
        }
        if let Some(cmd_id) = crate::imp::command_id(crate::imp::CMD_SHOW_DOCK_PANEL) {
            view::add_toolbar_button(cmd_id);
        }
    }

    pub fn show_panel() {
        let panel = register_one(create_panel(), &TB_DATA, TITLE_A.as_ptr(), &REGISTERED);
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
        let panel = register_one(create_notes(), &TB_DATA_2, TITLE_2.as_ptr(), &REGISTERED_2);
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

    /// What arrives at `messageProc` on these platforms, where a widget
    /// or a view has no window procedure for it to go to: `WM_NOTIFY`
    /// from the Notes panel's Scintilla view, and the host's `DMN_*`
    /// about this plugin's panels. `lParam` is the same `NMHDR` — or
    /// `SCNotification` — a Windows plugin gets.
    pub fn message(msg: u32, wparam: usize, lparam: isize) -> isize {
        if msg != sdk::WM_NOTIFY || lparam == 0 {
            return 0;
        }
        if notes_notification(lparam) {
            return 0;
        }
        dock_notification(wparam, lparam)
    }

    /// Whether `lparam` is a notification from the Notes view — which
    /// names itself in `nmhdr.hwndFrom`, as a Win32 Scintilla child does —
    /// reporting its length on the status bar after each edit.
    ///
    /// Only the `NMHDR` is read until `hwndFrom` has said so: a `DMN_*`
    /// from the host is an `NMHDR` and nothing more, so the wider
    /// `SCNotification` may be read only once the sender is known to be
    /// the Scintilla view, which sends the whole structure.
    fn notes_notification(lparam: isize) -> bool {
        let notes = NOTES_SCI.load(Ordering::Acquire);
        if notes.is_null() {
            return false;
        }
        // SAFETY: by the host's contract every `WM_NOTIFY` `lParam` points
        // at an `NMHDR`, live for this call.
        let from = unsafe { (*(lparam as *const SciNotifyHeader)).hwnd_from };
        if from != notes {
            return false;
        }
        // SAFETY: the sender is the Notes view, whose notifications carry a
        // whole `SCNotification`, live for this call.
        let scn = unsafe { &*(lparam as *const SCNotification) };
        if scn.nmhdr.code == SCN_MODIFIED && scn.modification_type & SC_MOD_TEXT != 0 {
            // SAFETY: the view the host made, which it keeps for the
            // process; `SCI_GETLENGTH` takes nothing.
            let length = unsafe { sdk::SendMessageW(notes, SCI_GETLENGTH, 0, 0) };
            sdk::set_status(&format!(
                "Example Hello: notes are {length} bytes (SCN_MODIFIED from the plugin's own Scintilla)"
            ));
        }
        true
    }

    /// The host's `DMN_*` about this plugin's panels, with the panel's
    /// content in `wParam`.
    fn dock_notification(wparam: usize, lparam: isize) -> isize {
        // A zero `wParam` names no panel: the host sends registered
        // content, never null — and a panel not created yet reads as null
        // below, so this also keeps one from matching it.
        if wparam == 0 {
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
        // The switch and relayout notices name a panel, and with two
        // panels in one group a tab switch is about both of them.
        let which = if item == crate::imp::CMD_SHOW_DOCK_PANEL {
            "panel"
        } else {
            "notes panel"
        };
        // Switch on the low word: DMN_DOCK and DMN_FLOAT carry their
        // container number in the high one. (The other three carry
        // nothing there, so the whole code would do as well.)
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
            sdk::DMN_SWITCHIN => sdk::set_status(&format!(
                "Example Hello: {which} came on screen (DMN_SWITCHIN)"
            )),
            sdk::DMN_SWITCHOFF => sdk::set_status(&format!(
                "Example Hello: {which} went behind another tab (DMN_SWITCHOFF)"
            )),
            sdk::DMN_FLOATDROPPED => sdk::set_status(&format!(
                "Example Hello: {which} laid out anew (DMN_FLOATDROPPED)"
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

    /// The Notes panel: the same label panel as [`build`]. The GTK host
    /// makes no Scintilla views for plugins yet — `NPPM_CREATESCINTILLAHANDLE`
    /// answers null there (DESIGN.md §7.4) — so there is none to ask for.
    pub(super) fn build_notes(text: &CStr, _notes: &CStr) -> (Hwnd, Hwnd) {
        (build(text), core::ptr::null_mut())
    }

    /// Nothing: the GTK host takes no plugin toolbar buttons yet.
    pub(super) fn add_toolbar_button(_cmd_id: i32) {}
}

/// The macOS content: an `NSView` holding a wrapping label — or, for the
/// Notes panel, a Scintilla view the host made for the plugin.
///
/// A recompiled plugin hands the Cocoa host an `NSView*` as `hClient` —
/// in no superview, and not a window's. The host takes its own
/// reference, sizes the view to fill the panel through its autoresizing
/// mask, and shows it; it never releases the plugin's reference, which
/// this module holds for the process. Built on the Objective-C runtime
/// directly — see [`crate::objc`].
#[cfg(target_os = "macos")]
mod cocoa_view {
    use codepp_plugin_sdk::{self as sdk, Hwnd, ToolbarIcons};
    use core::ffi::CStr;

    use crate::objc::{class, rect, sel, send, string, with_pool, Id, ObjcBool, Rect, Sel, NO};

    pub(super) use crate::objc::ready;

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

    /// `SCI_SETTEXT` and `SCI_SETWRAPMODE` / `SC_WRAP_WORD`: the Notes
    /// view's starting text, wrapped to the panel's width.
    const SCI_SETTEXT: u32 = 2181;
    const SCI_SETWRAPMODE: u32 = 2268;
    const SC_WRAP_WORD: usize = 1;

    /// The SF Symbol on the toolbar button, and what a screen reader
    /// reads for it: a window with a panel along its bottom, which is where "Show
    /// Dock Panel" puts the panel.
    const TOOLBAR_SYMBOL: &CStr = c"rectangle.bottomhalf.inset.filled";
    const TOOLBAR_SYMBOL_DESCRIPTION: &CStr = c"Example Hello: Show Dock Panel";

    /// A panel view with nothing in it yet, held by the reference
    /// `alloc`/`init` returns — the plugin's own, never released. Null if
    /// AppKit could not make it.
    fn panel() -> Hwnd {
        let view_class = class(c"NSView");
        if view_class.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `alloc` and `initWithFrame:`'s own prototypes, on the
        // main thread (`ready` checked it), to the class looked up above.
        unsafe {
            let alloc: unsafe extern "C" fn(Id, Sel) -> Id = send();
            let init_with_frame: unsafe extern "C" fn(Id, Sel, Rect) -> Id = send();
            init_with_frame(
                alloc(view_class, sel(c"alloc")),
                sel(c"initWithFrame:"),
                rect(0.0, 0.0, PANEL_WIDTH, PANEL_HEIGHT),
            )
        }
    }

    /// Put `child` in `panel` at `frame`, resizing with it.
    fn fill(panel: Id, child: Id, frame: Rect) {
        // SAFETY: `setFrame:`, `setAutoresizingMask:` and `addSubview:`'s
        // own prototypes, on the main thread, to live views.
        unsafe {
            let set_frame: unsafe extern "C" fn(Id, Sel, Rect) = send();
            let set_mask: unsafe extern "C" fn(Id, Sel, usize) = send();
            let add_subview: unsafe extern "C" fn(Id, Sel, Id) = send();
            set_frame(child, sel(c"setFrame:"), frame);
            set_mask(
                child,
                sel(c"setAutoresizingMask:"),
                WIDTH_AND_HEIGHT_SIZABLE,
            );
            add_subview(panel, sel(c"addSubview:"), child);
        }
    }

    /// Put a wrapping label reading `text` in `panel`, filling it less an
    /// inset.
    fn add_label(panel: Id, text: &CStr) {
        let label_class = class(c"NSTextField");
        if label_class.is_null() {
            return;
        }
        with_pool(|| {
            // SAFETY: `wrappingLabelWithString:`'s own prototype, on the
            // main thread, with a string just made. The label comes back
            // autoreleased; the panel keeps it once it is added.
            let label = unsafe {
                let with_string: unsafe extern "C" fn(Id, Sel, Id) -> Id = send();
                with_string(label_class, sel(c"wrappingLabelWithString:"), string(text))
            };
            if !label.is_null() {
                fill(
                    panel,
                    label,
                    rect(
                        LABEL_INSET,
                        LABEL_INSET,
                        PANEL_WIDTH - 2.0 * LABEL_INSET,
                        PANEL_HEIGHT - 2.0 * LABEL_INSET,
                    ),
                );
            }
        });
    }

    /// An `NSView` with a wrapping label filling it, less an inset. Null
    /// if AppKit could not make it.
    pub(super) fn build(text: &CStr) -> Hwnd {
        let panel = panel();
        if !panel.is_null() {
            add_label(panel, text);
        }
        panel
    }

    /// The Notes panel: a panel view filled by a Scintilla view the host
    /// makes for the plugin — `NPPM_CREATESCINTILLAHANDLE`, with the panel
    /// as its parent — reading `notes`. Returns the panel and the
    /// Scintilla view, whose notifications then reach this plugin's
    /// `messageProc`. A host that makes no view answers null, and the
    /// panel carries the label `text` instead, as on the other platforms.
    pub(super) fn build_notes(text: &CStr, notes: &CStr) -> (Hwnd, Hwnd) {
        let panel = panel();
        if panel.is_null() {
            return (panel, core::ptr::null_mut());
        }
        // SAFETY: the npp handle and a live view of the plugin's own; the
        // host reads the view and keeps its answer for the process.
        let sci = unsafe {
            sdk::SendMessageW(
                sdk::npp_handle(),
                sdk::NPPM_CREATESCINTILLAHANDLE,
                0,
                panel as isize,
            )
        } as Hwnd;
        if sci.is_null() {
            add_label(panel, text);
            return (panel, sci);
        }
        // The host puts the view in the panel hidden and zero-sized, for
        // the plugin to size and show — here, the whole panel.
        fill(panel, sci, rect(0.0, 0.0, PANEL_WIDTH, PANEL_HEIGHT));
        // SAFETY: `setHidden:`'s own prototype, on the main thread; and
        // two `SCI_*` to the view the host just made, through the SDK's
        // routing like any other — `notes` is NUL-terminated and read
        // during the call.
        unsafe {
            let set_hidden: unsafe extern "C" fn(Id, Sel, ObjcBool) = send();
            set_hidden(sci, sel(c"setHidden:"), NO);
            sdk::SendMessageW(sci, SCI_SETWRAPMODE, SC_WRAP_WORD, 0);
            sdk::SendMessageW(sci, SCI_SETTEXT, 0, notes.as_ptr() as isize);
        }
        (panel, sci)
    }

    /// Ask the host for a toolbar button that runs command `cmd_id` —
    /// `NPPM_ADDTOOLBARICON` — showing an SF Symbol. The image comes back
    /// autoreleased; the host retains it, so it outlives the pool.
    pub(super) fn add_toolbar_button(cmd_id: i32) {
        let image_class = class(c"NSImage");
        if image_class.is_null() {
            return;
        }
        let added = with_pool(|| {
            // SAFETY: `imageWithSystemSymbolName:accessibilityDescription:`'s
            // own prototype, on the main thread, with two strings just
            // made. It answers nil for a name this system does not know.
            let image = unsafe {
                let symbol: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id = send();
                symbol(
                    image_class,
                    sel(c"imageWithSystemSymbolName:accessibilityDescription:"),
                    string(TOOLBAR_SYMBOL),
                    string(TOOLBAR_SYMBOL_DESCRIPTION),
                )
            };
            if image.is_null() {
                return false;
            }
            let icons = ToolbarIcons {
                h_toolbar_bmp: core::ptr::null_mut(),
                h_toolbar_icon: image,
            };
            // SAFETY: `icons` outlives the call, which reads it and
            // retains the image.
            unsafe {
                sdk::SendMessageW(
                    sdk::npp_handle(),
                    sdk::NPPM_ADDTOOLBARICON,
                    cmd_id as usize,
                    (&raw const icons) as isize,
                ) != 0
            }
        });
        if !added {
            sdk::set_status("Example Hello: the host added no toolbar button");
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

    /// Nothing: the toolbar button is demonstrated on macOS, where it is
    /// an `NSImage`. The Win32 host takes one too, as an `HICON`; this
    /// plugin does not ask for it there.
    pub fn add_toolbar_button() {}

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
