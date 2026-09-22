//! Entry-point implementations for the example-hello plugin.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Most of the FFI scaffolding lives in `codepp-plugin-sdk`
//! (handle storage, `SyncCell`, the `SendMessageW` link, common
//! NPPM constants). This file keeps only the example-specific
//! bits: the plugin name, the one-item `FUNCS` array, and the
//! menu callback that inserts "Hello from plugin" via
//! `SCI_INSERTTEXT`.

#![cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]

use codepp_plugin_sdk::{self as sdk, FuncItem, NppData, SCNotification, SyncCell};

/// `SCI_INSERTTEXT` isn't part of the SDK's shared message set
/// (only example-hello uses it; the other plugins go through the
/// SDK's selection-replacement helper), so the constant lives
/// locally.
const SCI_INSERTTEXT: u32 = 2003;

/// Plugin name returned by `getName`. Wide-char, null-terminated,
/// stable for the lifetime of the loaded DLL. Notepad++'s
/// convention is that the pointer remains valid until shutdown; a
/// static array is the simplest way to honour that.
const PLUGIN_NAME: [u16; 14] = make_plugin_name();

const fn make_plugin_name() -> [u16; 14] {
    // "Example Hello\0" — 13 ASCII chars + NUL. ASCII codepoints
    // map 1:1 to UTF-16 units, so byte == u16.
    let mut buf = [0u16; 14];
    let bytes = b"Example Hello";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Number of entries in [`FUNCS`]. Single-sourced: it is both the
/// array's length and the count `getFuncsArray` reports, and the
/// host indexes the array with the number we report — so a literal
/// left stale after adding an item is an out-of-bounds read on the
/// host's side.
const FUNCS_COUNT: usize = 3;

/// The plugin's contributed menu items. `cmd_id` is written by the
/// host during load; we leave it 0 in the static initialiser per
/// the ABI contract.
///
/// The two docking entries drive the host's `NPPM_DMM*` surface —
/// see [`crate::dock`] for what each one demonstrates. They are
/// present on every platform and report their own unavailability
/// off Windows rather than changing the array's length per target.
static FUNCS: SyncCell<[FuncItem; FUNCS_COUNT]> = SyncCell::new([
    FuncItem {
        item_name: sdk::menu_label(b"Insert Hello"),
        p_func: Some(plugin_cmd_insert_hello),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Show Dock Panel"),
        p_func: Some(plugin_cmd_show_dock_panel),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: sdk::menu_label(b"Rename Dock Panel"),
        p_func: Some(plugin_cmd_rename_dock_panel),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
]);

#[no_mangle]
pub extern "C" fn setInfo(data: NppData) {
    sdk::store_handles(data);
}

#[no_mangle]
pub extern "C" fn getName() -> *const u16 {
    PLUGIN_NAME.as_ptr()
}

#[no_mangle]
pub extern "C" fn getFuncsArray(nb: *mut i32) -> *mut FuncItem {
    if !nb.is_null() {
        // SAFETY: per the ABI, `nb` is a valid out-pointer the host
        // owns for the duration of this call. Writing one `i32`
        // through it does not exceed its bounds.
        unsafe { *nb = FUNCS_COUNT as i32 };
    }
    FUNCS.get().cast::<FuncItem>()
}

/// `beNotified`: dispatch on `nmhdr.code`.
///
/// The one event example-hello handles is `NPPN_FILEBEFORECLOSE`,
/// and it handles it the way real Notepad++ plugins do — by calling
/// **back into the host from inside the notification** to resolve
/// the closing buffer's path, then reporting it on the status bar
/// (`Closing: C:\…\file.txt`). That round trip is the point: the
/// host has to deliver FILEBEFORECLOSE while the tab is still open
/// *and* answer `NPPM_*` from inside `beNotified`, or the lookup
/// comes back empty. It is the demo for DESIGN.md §7.4's
/// synchronous-notification item, observable in the running app.
#[no_mangle]
pub extern "C" fn beNotified(notification: *const SCNotification) {
    if notification.is_null() {
        return;
    }
    // SAFETY: per the ABI the host hands a valid `SCNotification`
    // that stays live for the duration of this synchronous call.
    let header = unsafe { &(*notification).nmhdr };
    if header.code != sdk::NPPN_FILEBEFORECLOSE {
        return;
    }
    match sdk::buffer_path(header.id_from) {
        Some(path) => sdk::set_status(&format!("Closing: {path}")),
        None => sdk::set_status("Closing: (untitled)"),
    }
}

#[no_mangle]
pub extern "C" fn messageProc(_msg: u32, _wparam: usize, _lparam: isize) -> isize {
    // No-op: example-hello has no host-to-plugin custom messages.
    0
}

#[no_mangle]
pub extern "C" fn isUnicode() -> i32 {
    // TRUE — we operate on UTF-16 strings throughout.
    1
}

/// Menu callback. Invoked by the host (single-threaded UI dispatch)
/// when the user clicks our "Insert Hello" menu item.
extern "C" fn plugin_cmd_insert_hello() {
    // SCI_INSERTTEXT(pos, text). `pos == -1` means the current
    // caret; wparam is `Sci_Position` (signed `intptr_t`) — we pass
    // the 2's-complement bit pattern of -1 as `usize`. The lparam
    // is a null-terminated UTF-8 byte string.
    // Hoisted above the first statement so clippy's
    // `items_after_statements` rule is satisfied.
    const HELLO: &[u8] = b"Hello from plugin\0";

    let sci = sdk::active_scintilla();
    if sci.is_null() {
        return;
    }

    let neg_one = (-1isize) as usize;
    // SAFETY: `HELLO` is static and outlives the SendMessage call;
    // SCI_INSERTTEXT does not retain the pointer past the call.
    unsafe {
        sdk::SendMessageW(sci, SCI_INSERTTEXT, neg_one, HELLO.as_ptr() as isize);
    }
}

/// Menu callback: create-register-show the docking panel.
extern "C" fn plugin_cmd_show_dock_panel() {
    crate::dock::show_panel();
}

/// Menu callback: re-point the panel's `psz_name` and refresh it
/// through `NPPM_DMMUPDATEDISPINFO`.
extern "C" fn plugin_cmd_rename_dock_panel() {
    crate::dock::rename_panel();
}
