//! Entry-point implementations for cppconverter.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Phase 4 m7 scaffolding: two menu items wired to no-op placeholders.
//! The real ASCII↔HEX transforms land in the follow-up commit.

#![cfg(target_os = "windows")]

use core::cell::UnsafeCell;

use codepp_plugin_host::ffi::{FuncItem, NppData, SCNotification, MENU_TITLE_LENGTH};

const PLUGIN_NAME: [u16; 10] = make_plugin_name();

const fn make_plugin_name() -> [u16; 10] {
    // "Converter\0" — 9 ASCII chars + NUL.
    let mut buf = [0u16; 10];
    let bytes = b"Converter";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

const fn menu_label(bytes: &[u8]) -> [u16; MENU_TITLE_LENGTH] {
    let mut buf = [0u16; MENU_TITLE_LENGTH];
    let mut i = 0;
    while i < bytes.len() && i < MENU_TITLE_LENGTH - 1 {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

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

static FUNCS: SyncCell<[FuncItem; 2]> = SyncCell::new([
    FuncItem {
        item_name: menu_label(b"ASCII -> HEX"),
        p_func: Some(cmd_ascii_to_hex),
        cmd_id: 0,
        init2_check: 0,
        p_sh_key: core::ptr::null_mut(),
    },
    FuncItem {
        item_name: menu_label(b"HEX -> ASCII"),
        p_func: Some(cmd_hex_to_ascii),
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
        unsafe { *nb = 2 };
    }
    FUNCS.get().cast::<FuncItem>()
}

#[no_mangle]
pub extern "C" fn beNotified(_notification: *const SCNotification) {}

#[no_mangle]
pub extern "C" fn messageProc(_msg: u32, _wparam: usize, _lparam: isize) -> isize {
    0
}

#[no_mangle]
pub extern "C" fn isUnicode() -> i32 {
    1
}

extern "C" fn cmd_ascii_to_hex() {}
extern "C" fn cmd_hex_to_ascii() {}
