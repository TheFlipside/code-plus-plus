//! Entry-point implementations for cppmimetools.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Phase 4 m7 scaffolding: the menu items are wired through to
//! placeholder callbacks that no-op silently. The real Base64 / URL /
//! quoted-printable transforms land in the follow-up commit. The
//! scaffold ships now so the workspace builds with the new crate as a
//! member and the integration test that asserts load + menu count
//! has something to assert against.

#![cfg(target_os = "windows")]

use core::cell::UnsafeCell;

use codepp_plugin_host::ffi::{FuncItem, NppData, SCNotification, MENU_TITLE_LENGTH};

/// Plugin display name returned by `getName`. Wide-char, null-terminated,
/// stable for the DLL's lifetime.
const PLUGIN_NAME: [u16; 11] = make_plugin_name();

const fn make_plugin_name() -> [u16; 11] {
    // "MIME Tools\0" — 10 ASCII chars + NUL.
    let mut buf = [0u16; 11];
    let bytes = b"MIME Tools";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Build a fixed-size null-padded UTF-16 menu label from an ASCII
/// byte string. `const fn` so the FuncItem array initialiser is a
/// compile-time constant.
const fn menu_label(bytes: &[u8]) -> [u16; MENU_TITLE_LENGTH] {
    let mut buf = [0u16; MENU_TITLE_LENGTH];
    let mut i = 0;
    while i < bytes.len() && i < MENU_TITLE_LENGTH - 1 {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Wrapper providing `Sync` for `UnsafeCell<T>`. The host writes
/// `cmd_id` into our FuncItem array at load time; the plugin itself
/// never reads back. See `example-hello`'s `imp.rs` for the long-form
/// rationale (single-threaded UI dispatch + only `cmd_id` mutated).
#[repr(transparent)]
struct SyncCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncCell<T> {}
impl<T> SyncCell<T> {
    const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// Six menu items in the Phase 4 m7 scaffold. Bodies are no-op
/// placeholders that the follow-up commit replaces with real
/// transforms.
static FUNCS: SyncCell<[FuncItem; 6]> = SyncCell::new([
    FuncItem {
        item_name: menu_label(b"Base64 Encode"),
        p_func: Some(cmd_base64_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Base64 Decode"),
        p_func: Some(cmd_base64_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"URL Encode"),
        p_func: Some(cmd_url_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"URL Decode"),
        p_func: Some(cmd_url_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Quoted-Printable Encode"),
        p_func: Some(cmd_qp_encode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"Quoted-Printable Decode"),
        p_func: Some(cmd_qp_decode),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
]);

#[no_mangle]
pub extern "C" fn setInfo(_data: NppData) {
    // Scaffold: callbacks are no-ops, so the handle snapshot isn't
    // needed yet. The follow-up commit re-introduces atomic-pointer
    // storage of the three handles when the real callbacks land.
}

#[no_mangle]
pub extern "C" fn getName() -> *const u16 {
    PLUGIN_NAME.as_ptr()
}

#[no_mangle]
pub extern "C" fn getFuncsArray(nb: *mut i32) -> *mut FuncItem {
    if !nb.is_null() {
        // SAFETY: per the ABI, `nb` is a valid out-pointer the host
        // owns for the duration of this call.
        unsafe { *nb = 6 };
    }
    FUNCS.get().cast::<FuncItem>()
}

#[no_mangle]
pub extern "C" fn beNotified(_notification: *const SCNotification) {
    // No-op: cppmimetools doesn't react to NPPN_*/SCN_* events.
}

#[no_mangle]
pub extern "C" fn messageProc(_msg: u32, _wparam: usize, _lparam: isize) -> isize {
    // No-op: no host-to-plugin custom messages.
    0
}

#[no_mangle]
pub extern "C" fn isUnicode() -> i32 {
    1
}

extern "C" fn cmd_base64_encode() {}
extern "C" fn cmd_base64_decode() {}
extern "C" fn cmd_url_encode() {}
extern "C" fn cmd_url_decode() {}
extern "C" fn cmd_qp_encode() {}
extern "C" fn cmd_qp_decode() {}
