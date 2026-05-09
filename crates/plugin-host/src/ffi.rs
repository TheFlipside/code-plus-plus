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

/// Mirror of Win32 `RECT` (left/top/right/bottom). Embedded inside
/// [`TbData`] for the floating dialog's preferred position. Plain
/// 4×i32 — `#[repr(C)]` keeps Rust from reordering so the layout
/// matches the upstream `RECT` byte-for-byte.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct TbRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Mirror of upstream Notepad++'s `NppDarkMode::Colors`. The
/// payload struct for `NPPM_GETDARKMODECOLORS`: 12 × Win32
/// `COLORREF` (each `0x00BBGGRR`, packed as `u32`), totalling
/// 48 bytes on every platform regardless of pointer width.
///
/// Field order matches upstream verbatim — plugins compiled
/// against the public ABI parse the fields by offset, so any
/// reorder here is an ABI break.
///
/// The host writes the 12 colours through this struct when
/// dark mode is active. Code++ today returns FALSE from
/// `NPPM_ISDARKMODEENABLED` and `NPPM_GETDARKMODECOLORS` —
/// the host has no dark-mode rendering yet (Phase 5 polish,
/// DESIGN.md §7.4) — so the buffer is never written.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NppDarkModeColors {
    pub background: u32,
    pub ctrl_background: u32,
    pub hot_background: u32,
    pub dlg_background: u32,
    pub error_background: u32,
    pub text: u32,
    pub darker_text: u32,
    pub disabled_text: u32,
    pub link_text: u32,
    pub edge: u32,
    pub hot_edge: u32,
    pub disabled_edge: u32,
}

/// Mirror of Notepad++'s `tTbData` — the registration payload for
/// `NPPM_DMMREGASDCKDLG`. Layout matches the public ABI declared
/// in `plugins/nppcompat-headers/Docking.h`:
///
/// ```text
/// HWND          h_client       (offset  0, 8 bytes on x64 / 4 on x86)
/// const TCHAR*  psz_name       (offset  8 / 4)
/// int           dlg_id         (offset 16 / 8)
/// UINT          u_mask         (offset 20 / 12)
/// HICON         h_icon_tab     (offset 24 / 16)
/// const TCHAR*  psz_add_info   (offset 32 / 20)
/// RECT          rc_float       (offset 40 / 24, 16 bytes)
/// int           i_prev_cont    (offset 56 / 40)
/// const TCHAR*  psz_module_name(offset 64 / 44 — 4 bytes padding on x64)
/// ```
///
/// Total size: 72 bytes (x64) / 48 bytes (x86), enforced by the
/// `cfg(test)` size assertions below.
///
/// Pointers are read with `read_unaligned` on the dispatch side
/// because plugin allocations may not be aligned to the
/// pointer-width boundary Rust would otherwise require for an
/// aligned dereference.
#[repr(C)]
pub struct TbData {
    /// Plugin's docking-dialog HWND. The frame the host creates
    /// owns this HWND as its child for sizing; the plugin retains
    /// the lifetime contract (the plugin destroys the HWND, not
    /// the host).
    pub h_client: *mut c_void,
    /// Wide-char display title. Used for the floating frame's
    /// caption and as the lookup key for
    /// `NPPM_DMMGETPLUGINHWNDBYNAME`. Plugin owns the buffer;
    /// host reads on every UPDATEDISPINFO and on lookup.
    pub psz_name: *const u16,
    /// Carried in `nmhdr.idFrom` for any future `DMN_*`
    /// notification routed back to the plugin.
    pub dlg_id: i32,
    /// Bit-mask of `DWS_*` flags.
    pub u_mask: u32,
    /// Optional title-bar icon. NULL skips icon rendering.
    pub h_icon_tab: *mut c_void,
    /// Optional extra-info wide string shown alongside the title.
    /// NULL skips. Plugin owns the buffer.
    pub psz_add_info: *const u16,
    /// Preferred floating position. `(0,0,0,0)` falls back to a
    /// default offset from the host window.
    pub rc_float: TbRect,
    /// Previous-container id (CONT_LEFT/RIGHT/TOP/BOTTOM = 0..=3).
    /// Stored verbatim; floating-only mode does not act on it.
    pub i_prev_cont: i32,
    /// Plugin DLL filename without extension. Used by
    /// `GETPLUGINHWNDBYNAME`'s second argument (the optional
    /// module-name disambiguator). Plugin owns the buffer.
    pub psz_module_name: *const u16,
}

// --- DWS_* Docking Window Style flags --------------------------------
//
// Mirrors the public ABI in `plugins/nppcompat-headers/Docking.h`.
// Numeric values are not copyrightable; the ABI requires they match.

/// `hIconTab` is shown on the tab strip.
pub const DWS_ICONTAB: u32 = 0x0000_0001;
/// `hIconTab` is shown in the title bar (legacy alias).
pub const DWS_ICONBAR: u32 = 0x0000_0002;
/// `pszAddInfo` is shown in the title bar.
pub const DWS_ADDINFO: u32 = 0x0000_0004;
/// Plugin draws its own dark-mode chrome.
pub const DWS_USEOWNDARKMODE: u32 = 0x0100_0000;

/// Default-container nibble: opens floating.
pub const DWS_DF_FLOATING: u32 = 0x8000_0000;
/// Default-container nibble: dock-left preference.
pub const DWS_DF_CONT_LEFT: u32 = 0x0000_0000;
/// Default-container nibble: dock-right preference.
pub const DWS_DF_CONT_RIGHT: u32 = 0x1000_0000;
/// Default-container nibble: dock-top preference.
pub const DWS_DF_CONT_TOP: u32 = 0x2000_0000;
/// Default-container nibble: dock-bottom preference.
pub const DWS_DF_CONT_BOTTOM: u32 = 0x3000_0000;

// --- DMN_* dock notifications ----------------------------------------
//
// Sent in `SciNotifyHeader.code`. `id_from` is the registered
// `tTbData.dlg_id`; `hwnd_from` is the host frame's HWND.

/// First DMN_* code. Notifications below this floor are reserved.
pub const DMN_FIRST: u32 = 0x1000;
/// User closed the floating dialog (frame hidden, plugin's HWND
/// stays alive).
pub const DMN_CLOSE: u32 = DMN_FIRST + 1;
/// Floating dialog has been docked into a container. Reserved for
/// the Phase-5 docking manager — never sent in floating-only mode.
pub const DMN_DOCK: u32 = DMN_FIRST + 2;
/// Docked dialog has been floated. Reserved for the Phase-5
/// docking manager — never sent in floating-only mode.
pub const DMN_FLOAT: u32 = DMN_FIRST + 3;

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

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn tb_data_total_size_x64() {
        // Mirror the upstream `tTbData` layout exactly:
        //   8 (h_client) + 8 (psz_name) + 4 (dlg_id) + 4 (u_mask)
        // + 8 (h_icon_tab) + 8 (psz_add_info) + 16 (rc_float)
        // + 4 (i_prev_cont) + 4 (padding to align next pointer)
        // + 8 (psz_module_name)
        // = 72 bytes. Catches any future field-type/order regression
        // that would break ABI compat with plugins compiled against
        // `Docking.h`.
        assert_eq!(
            core::mem::size_of::<TbData>(),
            72,
            "TbData layout regressed; plugins reading the upstream tTbData struct would parse garbage",
        );
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn tb_data_total_size_x86() {
        // 4 (h_client) + 4 (psz_name) + 4 (dlg_id) + 4 (u_mask)
        // + 4 (h_icon_tab) + 4 (psz_add_info) + 16 (rc_float)
        // + 4 (i_prev_cont) + 4 (psz_module_name)
        // = 48 bytes; pointers are 4-byte-aligned so no padding.
        assert_eq!(core::mem::size_of::<TbData>(), 48);
    }

    #[test]
    fn tb_rect_is_four_i32() {
        assert_eq!(core::mem::size_of::<TbRect>(), 16);
    }

    #[test]
    fn npp_dark_mode_colors_is_48_bytes() {
        // 12 × COLORREF (each u32, 4 bytes) = 48. The struct's
        // size is the same on x86 and x64 because all fields are
        // primitives, so no per-arch test pair is needed.
        assert_eq!(
            core::mem::size_of::<NppDarkModeColors>(),
            48,
            "NppDarkModeColors layout regressed; plugins reading the upstream struct would parse garbage",
        );
    }
}
