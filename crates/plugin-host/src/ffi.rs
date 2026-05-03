//! Rust mirrors of the C ABI types declared in
//! `plugins/nppcompat-headers/PluginInterface.h`.
//!
//! These structs and function pointers are the wire format Code++
//! uses to talk to plugin DLLs. Layout, field order, and field
//! types must match the header verbatim. Any divergence is an ABI
//! break — verified by the static asserts in the header's
//! companion compile-test (and re-checked here in `cfg(test)`).

#![cfg(target_os = "windows")]

use core::ffi::c_void;

/// Win32 handle type. Mirrors `HWND` from `windows.h`. Held as a
/// raw pointer because Rust's FFI doesn't know about Win32's
/// pointer-sized handle namespace.
pub type Hwnd = *mut c_void;

/// Mirror of `NppData` from PluginInterface.h. **Must** stay
/// `#[repr(C)]` and 3 × pointer-sized; reordered or padded
/// differently and a real plugin DLL parses garbage.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NppData {
    pub npp_handle: Hwnd,
    pub scintilla_main_handle: Hwnd,
    pub scintilla_second_handle: Hwnd,
}

/// Mirror of `ShortcutKey`. Field types are 1-byte `u8` (Win32
/// `bool`) plus 1-byte `u8` (Win32 `UCHAR`), total 4 bytes —
/// matching the public ABI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShortcutKey {
    pub is_ctrl: u8,
    pub is_alt: u8,
    pub is_shift: u8,
    pub key: u8,
}

/// Maximum FuncItem name length in TCHARs (UTF-16 code units on
/// Windows). Matches `MENU_TITLE_LENGTH` from PluginInterface.h.
pub const MENU_TITLE_LENGTH: usize = 64;

/// Mirror of `FuncItem`. The plugin owns the memory; the host reads
/// it after `getFuncsArray` and never writes back (except for
/// `_cmdID`, which the host populates with the assigned menu ID).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FuncItem {
    /// UTF-16 menu label. Null-terminated; trailing bytes after
    /// the null are not read.
    pub item_name: [u16; MENU_TITLE_LENGTH],
    /// Invoked when the user clicks the menu item.
    pub p_func: Option<PluginCmd>,
    /// Set by the host at load time to the menu-command id.
    pub cmd_id: i32,
    /// Initial check-state of the menu item (0 = unchecked, 1 = checked).
    pub init2_check: i32,
    /// Optional accelerator. Heap-allocated by the plugin; ownership
    /// stays with the plugin and survives until shutdown.
    pub p_sh_key: *mut ShortcutKey,
}

/// Plugin's per-menu-item callback. Plain `extern "C"` no-arg, no-return.
pub type PluginCmd = unsafe extern "C" fn();

// --- Six entry points exported by every plugin DLL -------------------

/// `setInfo(NppData)`
pub type SetInfoFn = unsafe extern "C" fn(NppData);

/// `getName() -> const TCHAR*`. Returns a wide-char pointer that
/// remains valid for the lifetime of the plugin (typically a static
/// string).
pub type GetNameFn = unsafe extern "C" fn() -> *const u16;

/// `getFuncsArray(int* nbF) -> FuncItem*`. Plugin populates `*nbF`
/// with the count and returns a pointer to its menu-item array.
pub type GetFuncsArrayFn = unsafe extern "C" fn(*mut i32) -> *mut FuncItem;

/// `beNotified(SCNotification*)`. Notification dispatch.
pub type BeNotifiedFn = unsafe extern "C" fn(*const c_void);

/// `messageProc(UINT, WPARAM, LPARAM) -> LRESULT`. Host-to-plugin
/// custom messages.
pub type MessageProcFn = unsafe extern "C" fn(u32, usize, isize) -> isize;

/// `isUnicode() -> BOOL`. Win32 `BOOL` is a 4-byte int.
pub type IsUnicodeFn = unsafe extern "C" fn() -> i32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nppdata_layout_is_three_pointers() {
        assert_eq!(
            core::mem::size_of::<NppData>(),
            3 * core::mem::size_of::<Hwnd>(),
            "NppData must be exactly three pointer-sized fields"
        );
    }

    #[test]
    fn shortcutkey_is_four_bytes() {
        assert_eq!(
            core::mem::size_of::<ShortcutKey>(),
            4,
            "ShortcutKey must match the public ABI's 4-byte layout"
        );
    }

    #[test]
    fn funcitem_name_buffer_size() {
        let f = FuncItem {
            item_name: [0; MENU_TITLE_LENGTH],
            p_func: None,
            cmd_id: 0,
            init2_check: 0,
            p_sh_key: core::ptr::null_mut(),
        };
        assert_eq!(f.item_name.len(), MENU_TITLE_LENGTH);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn funcitem_total_size_x64() {
        // 128 (item_name: [u16; 64])
        // +   8 (Option<PluginCmd>: pointer-sized, null-pointer-optimized)
        // +   4 (cmd_id)
        // +   4 (init2_check)
        // +   8 (p_sh_key: pointer-sized)
        // = 152 bytes. Catches any future field-type/order regression
        //   that would break ABI compat with plugins compiled against
        //   the C header.
        assert_eq!(
            core::mem::size_of::<FuncItem>(),
            152,
            "FuncItem layout regressed; plugins compiled against the C header would parse garbage"
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn funcitem_total_size_x86() {
        // 128 (item_name) + 4 (fn ptr) + 4 (cmd_id) + 4 (init2) + 4 (sh_key ptr) = 144
        assert_eq!(core::mem::size_of::<FuncItem>(), 144);
    }
}
