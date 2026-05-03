//! Entry-point implementations for the example-hello plugin.
//!
//! Six C-ABI exports per DESIGN.md §6.2 / `PluginInterface.h`:
//!   `setInfo`, `getName`, `getFuncsArray`, `beNotified`,
//!   `messageProc`, `isUnicode`.
//!
//! Symbols are exported with `#[no_mangle]` + C linkage so the host's
//! `GetProcAddress` lookups resolve them. The function bodies are
//! plain `extern "C"` (safe-from-Rust) — the host imports them as
//! `unsafe extern "C"` matching the FFI type aliases in
//! `codepp_plugin_host::ffi`. Either form produces the same symbol;
//! we pick the safe form so the body's `unsafe` blocks point at
//! exactly the FFI calls that need them, not the whole function.

#![cfg(target_os = "windows")]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use codepp_plugin_host::ffi::{FuncItem, NppData, SCNotification, MENU_TITLE_LENGTH};

/// Win32 handle alias matching the host's `Hwnd` type.
type Hwnd = *mut c_void;

/// `WM_USER + 1000` — the NPPM_* base. Inlined to avoid a runtime
/// dependency on the host crate's constants from inside FFI bodies.
const NPPMSG: u32 = 0x0400 + 1000;
const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
const SCI_INSERTTEXT: u32 = 2003;

#[link(name = "user32")]
extern "system" {
    fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
}

/// Snapshot of the three handles `setInfo` delivers. Stored as
/// atomics so the menu callback (which may run on the UI thread
/// while another thread is in the middle of setInfo, in theory)
/// reads consistent values without taking a lock.
static NPP_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_MAIN: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static SCINTILLA_SECONDARY: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Plugin name returned by `getName`. Wide-char, null-terminated,
/// stable for the lifetime of the loaded DLL. Notepad++'s convention
/// is that the pointer remains valid until shutdown; a static array
/// is the simplest way to honour that.
const PLUGIN_NAME: [u16; 14] = make_plugin_name();

const fn make_plugin_name() -> [u16; 14] {
    // "Example Hello\0" — 13 ASCII chars + NUL. ASCII codepoints map
    // 1:1 to UTF-16 units, so byte == u16.
    let mut buf = [0u16; 14];
    let bytes = b"Example Hello";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Build the menu-item label as a fixed-size null-padded UTF-16
/// array. Const-fn so the FuncItem array can be constructed at
/// compile time and stored in static memory.
const fn make_menu_label() -> [u16; MENU_TITLE_LENGTH] {
    let mut buf = [0u16; MENU_TITLE_LENGTH];
    let bytes = b"Insert Hello";
    let mut i = 0;
    while i < bytes.len() {
        buf[i] = bytes[i] as u16;
        i += 1;
    }
    buf
}

/// Wrapper providing `Sync` for `UnsafeCell<T>`. The host writes
/// `cmd_id` into our FuncItem array at load time — that's the
/// inherent "shared memory mutated by foreign code" pattern
/// `UnsafeCell` exists for. The plugin itself never reads back
/// `cmd_id`; we just hand the host a pointer it owns.
///
/// We deliberately do **not** bound `T: Send`: `FuncItem` carries a
/// `*mut ShortcutKey` raw pointer that prevents auto-derivation of
/// `Send`, but we know the host's access is single-threaded (the
/// UI thread only) and the only field it mutates is `cmd_id`,
/// which is plain `i32`. The bound would refuse a perfectly safe
/// pattern.
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

/// The plugin's contributed menu items. One entry: "Insert Hello"
/// bound to [`plugin_cmd_insert_hello`]. `cmd_id` is written by the
/// host during load; we leave it 0 in the static initializer per
/// the ABI contract.
static FUNCS: SyncCell<[FuncItem; 1]> = SyncCell::new([FuncItem {
    item_name: make_menu_label(),
    p_func: Some(plugin_cmd_insert_hello),
    cmd_id: 0,
    init2_check: 0,
    p_sh_key: core::ptr::null_mut(),
}]);

#[no_mangle]
pub extern "C" fn setInfo(data: NppData) {
    NPP_HANDLE.store(data.npp_handle, Ordering::Release);
    SCINTILLA_MAIN.store(data.scintilla_main_handle, Ordering::Release);
    SCINTILLA_SECONDARY.store(data.scintilla_second_handle, Ordering::Release);
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
        unsafe { *nb = 1 };
    }
    FUNCS.get().cast::<FuncItem>()
}

#[no_mangle]
pub extern "C" fn beNotified(_notification: *const SCNotification) {
    // No-op for example-hello. A real plugin would inspect
    // `(*notification).nmhdr.code` to dispatch on `NPPN_*` /
    // `SCN_*` events.
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
    let npp = NPP_HANDLE.load(Ordering::Acquire);
    if npp.is_null() {
        // setInfo hasn't run yet; nothing to do. (In practice the
        // host calls setInfo before installing menu items, so this
        // is defense in depth.)
        return;
    }

    // Ask the host which Scintilla view is currently active. The
    // dispatcher writes 0 (main) or 1 (secondary) through `&mut
    // which`; we then look up the matching HWND from setInfo.
    let mut which: i32 = 0;
    // SAFETY: `&mut which` is a valid `int*` for the duration of
    // the SendMessage call. NPPM_GETCURRENTSCINTILLA is documented
    // to write through it; the host's dispatcher implements that.
    unsafe {
        SendMessageW(
            npp,
            NPPM_GETCURRENTSCINTILLA,
            0,
            &mut which as *mut i32 as isize,
        );
    }

    let sci = if which == 0 {
        SCINTILLA_MAIN.load(Ordering::Acquire)
    } else {
        SCINTILLA_SECONDARY.load(Ordering::Acquire)
    };
    if sci.is_null() {
        return;
    }

    // SCI_INSERTTEXT(pos, text). `pos == -1` means the current caret;
    // wparam is `Sci_Position` (signed `intptr_t`) — we pass the
    // 2's-complement bit pattern of -1 as `usize`. The lparam is a
    // null-terminated UTF-8 byte string.
    const HELLO: &[u8] = b"Hello from plugin\0";
    let neg_one = (-1isize) as usize;
    // SAFETY: `HELLO` is static and outlives the SendMessage call;
    // SCI_INSERTTEXT does not retain the pointer past the call.
    unsafe {
        SendMessageW(sci, SCI_INSERTTEXT, neg_one, HELLO.as_ptr() as isize);
    }
}
