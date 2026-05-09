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

/// Mirror of `Sci_NotifyHeader` from Scintilla.h. ABI-compatible with
/// Win32 `NMHDR`: a `void*` window handle, a pointer-sized identifier,
/// and a 32-bit code that carries the `NPPN_*` or `SCN_*` discriminant.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SciNotifyHeader {
    pub hwnd_from: Hwnd,
    pub id_from: usize,
    pub code: u32,
}

impl Default for SciNotifyHeader {
    fn default() -> Self {
        Self {
            hwnd_from: core::ptr::null_mut(),
            id_from: 0,
            code: 0,
        }
    }
}

/// Mirror of `SCNotification` from Scintilla.h. **Must** stay
/// `#[repr(C)]` and field-for-field identical to the upstream
/// definition: plugins read this struct in their `beNotified` body
/// using the public `SCNotification` layout, so any field reorder
/// here parses as garbage on the plugin side.
///
/// `Sci_Position` is `ptrdiff_t` upstream — `isize` in Rust. `sptr_t`
/// is `intptr_t` (also `isize`); `uptr_t` is `uintptr_t` (`usize`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SCNotification {
    pub nmhdr: SciNotifyHeader,
    pub position: isize,
    pub ch: i32,
    pub modifiers: i32,
    pub modification_type: i32,
    pub text: *const u8,
    pub length: isize,
    pub lines_added: isize,
    pub message: i32,
    pub w_param: usize,
    pub l_param: isize,
    pub line: isize,
    pub fold_level_now: i32,
    pub fold_level_prev: i32,
    pub margin: i32,
    pub list_type: i32,
    pub x: i32,
    pub y: i32,
    pub token: i32,
    pub annotation_lines_added: isize,
    pub updated: i32,
    pub list_completion_method: i32,
    pub character_source: i32,
}

impl Default for SCNotification {
    fn default() -> Self {
        Self {
            nmhdr: SciNotifyHeader::default(),
            position: 0,
            ch: 0,
            modifiers: 0,
            modification_type: 0,
            text: core::ptr::null(),
            length: 0,
            lines_added: 0,
            message: 0,
            w_param: 0,
            l_param: 0,
            line: 0,
            fold_level_now: 0,
            fold_level_prev: 0,
            margin: 0,
            list_type: 0,
            x: 0,
            y: 0,
            token: 0,
            annotation_lines_added: 0,
            updated: 0,
            list_completion_method: 0,
            character_source: 0,
        }
    }
}

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
pub type BeNotifiedFn = unsafe extern "C" fn(*const SCNotification);

/// `messageProc(UINT, WPARAM, LPARAM) -> LRESULT`. Host-to-plugin
/// custom messages.
pub type MessageProcFn = unsafe extern "C" fn(u32, usize, isize) -> isize;

/// `isUnicode() -> BOOL`. Win32 `BOOL` is a 4-byte int.
pub type IsUnicodeFn = unsafe extern "C" fn() -> i32;

/// Mirror of Notepad++'s `toolbarIcons` struct used by
/// `NPPM_ADDTOOLBARICON`. The plugin populates the two icon
/// handles and passes a pointer to this struct in `lParam`.
///
/// Layout matches the upstream C declaration:
///   `HBITMAP hToolbarBmp; HICON hToolbarIcon;`
/// — two pointer-sized handles. `#[repr(C)]` keeps Rust from
/// reordering, so a plugin compiled against
/// `Notepad_plus_msgs.h` reads/writes the same bytes Code++
/// does.
///
/// **Code++ uses `h_toolbar_icon` only.** The legacy
/// `h_toolbar_bmp` (16×16 16-color bitmap, kept for old
/// Win9x-era N++) is logged-and-ignored: modern plugins ship
/// the 32-bpp HICON, and the imagelist Code++ attaches to its
/// toolbar is sized for icon — not bitmap — input. A plugin
/// that supplied only `h_toolbar_bmp` (NULL HICON) gets a
/// `false` return from the dispatcher.
#[repr(C)]
pub struct ToolbarIcons {
    /// Legacy 16-color bitmap. **Ignored by Code++.** Kept in
    /// the struct for ABI compatibility with plugins compiled
    /// against the upstream header.
    pub h_toolbar_bmp: *mut c_void,
    /// 32-bpp icon Code++ adds to the toolbar's HIMAGELIST via
    /// `ImageList_ReplaceIcon`. Must be non-null; the
    /// dispatcher rejects with `false` otherwise.
    pub h_toolbar_icon: *mut c_void,
}

/// Mirror of Notepad++'s `CommunicationInfo` struct used by
/// `NPPM_MSGTOPLUGIN` for inter-plugin messaging. The source
/// plugin populates this and passes a pointer to it in `lParam`;
/// the host forwards it to the target plugin's `messageProc`.
///
/// Layout matches the upstream C struct verbatim:
///   `long internalMsg; const TCHAR* srcModuleName; void* info;`
///
/// `long` is `i32` in the Win32 LLP64 ABI on both x86 and x64;
/// the pointer fields are pointer-sized, so the struct's overall
/// size differs across architectures (12 bytes on x86, 24 bytes on
/// x64 due to alignment padding after the 4-byte `internal_msg`).
///
/// The host doesn't dereference `src_module_name` or `info` — they
/// are forwarded verbatim through the target plugin's `messageProc`
/// in the `wParam` slot. Only `internal_msg` is read host-side, to
/// pick the message number the target's `messageProc` receives.
#[repr(C)]
pub struct CommunicationInfo {
    /// Custom message code the source plugin chose for this
    /// communication. The host calls
    /// `target.messageProc(internal_msg, wparam = info_ptr, 0)`.
    pub internal_msg: i32,
    /// Wide-char name of the source plugin. Forwarded to the
    /// target plugin via the `info_ptr` (which points at this
    /// struct) — the host does not dereference it.
    pub src_module_name: *const u16,
    /// Opaque payload pointer the source plugin chose. Same
    /// "host doesn't dereference" rule applies.
    pub info: *mut c_void,
}

/// Mirror of Notepad++'s `sessionInfo` struct used by
/// `NPPM_SAVESESSION`. The plugin populates the three fields and
/// passes a pointer to this struct in `lParam`.
///
/// Layout follows the upstream C declaration:
///   `TCHAR* sessionFilePathName; int nbFile; TCHAR** files;`
/// — pointer, int, pointer. `#[repr(C)]` keeps Rust from reordering
/// fields so the Rust struct read by the dispatcher matches what
/// a plugin compiled against `Notepad_plus_msgs.h` writes.
///
/// Pointers are read with `read_unaligned` on the dispatch side
/// because plugin allocations may not be aligned to the
/// pointer-width boundary Rust would otherwise require for an
/// aligned dereference.
#[repr(C)]
pub struct SessionInfo {
    /// Wide-char path of the session-XML file to write. Must be
    /// non-null and null-terminated.
    pub session_file_path_name: *mut u16,
    /// Number of valid entries in `files`. Negative values are
    /// rejected by the dispatcher; a hard cap (`MAX_SESSION_FILES`)
    /// also bounds it against unreasonably large allocations.
    pub nb_file: i32,
    /// Array of `nb_file` wide-char pointers; each points at a
    /// null-terminated path. Null entries are skipped (defensive
    /// behaviour against partially-populated arrays).
    pub files: *mut *mut u16,
}

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

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn session_info_total_size_x64() {
        // 8 (session_file_path_name: *mut u16)
        // + 4 (nb_file: i32)
        // + 4 (padding to align the next pointer)
        // + 8 (files: *mut *mut u16)
        // = 24 bytes. Mirror the upstream C `sessionInfo` layout
        // exactly so plugin allocations parse correctly.
        assert_eq!(
            core::mem::size_of::<SessionInfo>(),
            24,
            "SessionInfo layout regressed; plugins reading the upstream sessionInfo struct would parse garbage",
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn session_info_total_size_x86() {
        // 4 (path) + 4 (nb_file) + 4 (files) = 12 bytes; no
        // alignment padding needed when pointers are 4 bytes.
        assert_eq!(core::mem::size_of::<SessionInfo>(), 12);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn communication_info_total_size_x64() {
        // 4 (internal_msg: i32)
        // + 4 (padding to align the next pointer)
        // + 8 (src_module_name: *const u16)
        // + 8 (info: *mut c_void)
        // = 24 bytes. Mirror the upstream C `CommunicationInfo`
        // layout exactly so plugin allocations parse correctly.
        assert_eq!(
            core::mem::size_of::<CommunicationInfo>(),
            24,
            "CommunicationInfo layout regressed; plugins reading the upstream struct would parse garbage",
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn communication_info_total_size_x86() {
        // 4 (internal_msg) + 4 (src_module_name) + 4 (info) = 12.
        assert_eq!(core::mem::size_of::<CommunicationInfo>(), 12);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn toolbar_icons_total_size_x64() {
        // Two pointer-sized handles back-to-back; no padding.
        // x64: 8 + 8 = 16. Catches a future field-type
        // regression that would break ABI compat with the
        // upstream `toolbarIcons` struct.
        assert_eq!(
            core::mem::size_of::<ToolbarIcons>(),
            16,
            "ToolbarIcons layout regressed; plugins reading the upstream toolbarIcons struct would parse garbage",
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn toolbar_icons_total_size_x86() {
        // 4 + 4 = 8 bytes; pointer-sized handles, no padding.
        assert_eq!(core::mem::size_of::<ToolbarIcons>(), 8);
    }
}
