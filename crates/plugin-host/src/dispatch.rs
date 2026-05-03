//! Inbound NPPM_* and outbound NPPN_* dispatch.
//!
//! Two halves:
//!
//! **Outbound** ([`notify_all`]): Code++ events become a [`Notification`]
//! and the dispatcher synthesizes an `SCNotification`, then calls every
//! loaded plugin's `beNotified` entry point. Each call is wrapped in
//! `catch_unwind` so a Rust-authored plugin that panics doesn't unwind
//! across the C ABI (see DESIGN.md §6.5).
//!
//! **Inbound** ([`dispatch_nppm`]): plugins call `SendMessage(npp_handle,
//! NPPM_*, wParam, lParam)`. The Win32 wnd_proc routes those messages
//! into this function. The dispatcher pulls live state from the
//! [`HostServices`] trait — implemented by `shell` so the plugin host
//! crate stays free of `Session` / `EditorHandle` knowledge.
//!
//! This file ships the **v1** subset of NPPM_* tagged in
//! `plugins/nppcompat-headers/Notepad_plus_msgs.h` and tracked in
//! `docs/nppm-coverage.md`. Plugins that send an unimplemented message
//! receive `0` and a `tracing::warn!` is logged — that's the
//! documented contract, so plugins always *link* and Code++ surfaces
//! coverage gaps at runtime, not at plugin-build time.

#![cfg(target_os = "windows")]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use crate::ffi::{Hwnd, SCNotification};
use crate::host::PluginHost;

/// `WM_USER` from `<winuser.h>`. Mirrored here so the dispatcher can
/// be unit-tested without pulling the `windows` crate into a test
/// dependency.
const WM_USER: u32 = 0x0400;

/// `NPPMSG` base — every NPPM_* is `NPPMSG + offset`. Matches the
/// public ABI in `Notepad_plus_msgs.h` so plugins compiled against
/// our header (or against Notepad++'s) hit the same numeric range.
pub const NPPMSG: u32 = WM_USER + 1000;

// --- v1 NPPM_* set ---------------------------------------------------

pub const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
pub const NPPM_GETCURRENTLANGTYPE: u32 = NPPMSG + 5;
pub const NPPM_SETCURRENTLANGTYPE: u32 = NPPMSG + 6;
pub const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;
pub const NPPM_GETMENUHANDLE: u32 = NPPMSG + 25;
pub const NPPM_ACTIVATEDOC: u32 = NPPMSG + 28;
pub const NPPM_RELOADFILE: u32 = NPPMSG + 36;
pub const NPPM_SWITCHTOFILE: u32 = NPPMSG + 37;
pub const NPPM_SAVECURRENTFILE: u32 = NPPMSG + 38;
pub const NPPM_SETMENUITEMCHECK: u32 = NPPMSG + 40;
pub const NPPM_GETWINDOWSVERSION: u32 = NPPMSG + 42;
pub const NPPM_MAKECURRENTBUFFERDIRTY: u32 = NPPMSG + 44;
pub const NPPM_GETPLUGINSCONFIGDIR: u32 = NPPMSG + 46;
pub const NPPM_MENUCOMMAND: u32 = NPPMSG + 48;
pub const NPPM_GETNPPVERSION: u32 = NPPMSG + 50;
pub const NPPM_GETFULLPATHFROMBUFFERID: u32 = NPPMSG + 58;
pub const NPPM_GETCURRENTBUFFERID: u32 = NPPMSG + 60;
pub const NPPM_GETBUFFERLANGTYPE: u32 = NPPMSG + 64;
pub const NPPM_DOOPEN: u32 = NPPMSG + 77;

/// Selectors for [`NPPM_GETMENUHANDLE`].
pub const NPPPLUGINMENU: i32 = 0;
pub const NPPMAINMENU: i32 = 1;

// --- v1 NPPN_* set ---------------------------------------------------

pub const NPPN_FIRST: u32 = 1000;

pub const NPPN_READY: u32 = NPPN_FIRST + 1;
pub const NPPN_TBMODIFICATION: u32 = NPPN_FIRST + 2;
pub const NPPN_FILEBEFORECLOSE: u32 = NPPN_FIRST + 3;
pub const NPPN_FILEOPENED: u32 = NPPN_FIRST + 4;
pub const NPPN_FILECLOSED: u32 = NPPN_FIRST + 5;
pub const NPPN_FILESAVED: u32 = NPPN_FIRST + 8;
pub const NPPN_SHUTDOWN: u32 = NPPN_FIRST + 9;
pub const NPPN_BUFFERACTIVATED: u32 = NPPN_FIRST + 10;
pub const NPPN_LANGCHANGED: u32 = NPPN_FIRST + 11;

/// Code++'s self-reported plugin-API version. Matches the encoding
/// plugins expect from `NPPM_GETNPPVERSION`: HIWORD = major, LOWORD =
/// minor. `0x0000_0001` reads as 0.1 — deliberately *below* any real
/// Notepad++ version so plugin gating like
/// `if (NPPM_GETNPPVERSION() >= 0x00080000)` correctly disables N++-
/// version-locked features that Code++ Phase 3 doesn't yet expose.
pub const CODEPP_PLUGIN_API_VERSION: isize = 0x0000_0001;

/// `MAX_PATH` in TCHARs — Win32's documented cap for path-shaped
/// out-buffers passed to NPPM messages. Plugins that opt into
/// `NPPM_ALLOCATESUPPORTED` (Phase 4+) get the unbounded variant; v1
/// caps every wide-string write at this length, NUL-terminated. The
/// limit closes the class of "plugin passed a small buffer, host
/// scribbles past it" bug — undefined behaviour we cannot detect from
/// the call site since the buffer size isn't carried in the message.
pub const MAX_PATH_TCHARS: usize = 260;

// --- Outbound: NPPN_* notifications ----------------------------------

/// High-level event the host wants every loaded plugin to learn about.
/// Each variant maps to one `NPPN_*` code; the dispatcher translates.
#[derive(Debug, Clone)]
pub enum Notification {
    /// Plugin lifecycle: `setInfo` + `getFuncsArray` finished. Fired
    /// once per plugin, immediately after first-touch load completes.
    Ready,
    /// Toolbar-button registration window. Fired after [`Self::Ready`].
    TbModification,
    FileBeforeClose {
        buffer_id: isize,
    },
    FileOpened {
        buffer_id: isize,
    },
    FileClosed {
        buffer_id: isize,
    },
    FileSaved {
        buffer_id: isize,
    },
    BufferActivated {
        buffer_id: isize,
    },
    LangChanged {
        buffer_id: isize,
    },
    /// App is about to exit. Fired before any DLL unload.
    Shutdown,
}

impl Notification {
    fn code(&self) -> u32 {
        match self {
            Self::Ready => NPPN_READY,
            Self::TbModification => NPPN_TBMODIFICATION,
            Self::FileBeforeClose { .. } => NPPN_FILEBEFORECLOSE,
            Self::FileOpened { .. } => NPPN_FILEOPENED,
            Self::FileClosed { .. } => NPPN_FILECLOSED,
            Self::FileSaved { .. } => NPPN_FILESAVED,
            Self::BufferActivated { .. } => NPPN_BUFFERACTIVATED,
            Self::LangChanged { .. } => NPPN_LANGCHANGED,
            Self::Shutdown => NPPN_SHUTDOWN,
        }
    }

    fn buffer_id(&self) -> isize {
        match self {
            Self::FileBeforeClose { buffer_id }
            | Self::FileOpened { buffer_id }
            | Self::FileClosed { buffer_id }
            | Self::FileSaved { buffer_id }
            | Self::BufferActivated { buffer_id }
            | Self::LangChanged { buffer_id } => *buffer_id,
            Self::Ready | Self::TbModification | Self::Shutdown => 0,
        }
    }
}

/// Synthesize an `SCNotification` for `notification` and deliver it to
/// every loaded plugin via `beNotified`. `npp_hwnd` is set as
/// `nmhdr.hwndFrom` so plugins can identify the host window.
///
/// Each plugin call is wrapped in `catch_unwind`; a panic logs a
/// warning but does not abort the iteration — one misbehaving plugin
/// must not block notifications to its peers (parity with Notepad++).
pub fn notify_all(host: &PluginHost, notification: &Notification, npp_hwnd: Hwnd) {
    let sci = SCNotification {
        nmhdr: crate::ffi::SciNotifyHeader {
            hwnd_from: npp_hwnd,
            // `id_from` is `uintptr_t` upstream; we carry the buffer id
            // as `isize` and reinterpret the bits — plugins read it
            // back as a buffer id without sign concerns.
            id_from: notification.buffer_id() as usize,
            code: notification.code(),
        },
        ..SCNotification::default()
    };

    for plugin in host.iter() {
        let Some(be_notified) = plugin.be_notified_fn() else {
            continue;
        };
        let _span = tracing::trace_span!(
            "plugin_notify",
            plugin = %plugin.display_label(),
            code = notification.code(),
        )
        .entered();
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `be_notified` has the C ABI declared in
            // PluginInterface.h; `&sci` points to a valid #[repr(C)]
            // SCNotification and stays live for the duration of the
            // call (no thread spawning, no async).
            unsafe { be_notified(&sci as *const SCNotification) }
        }));
        if result.is_err() {
            tracing::warn!(
                plugin = %plugin.display_label(),
                "plugin panicked in beNotified",
            );
        }
    }
}

// --- Inbound: NPPM_* dispatch ----------------------------------------

/// Side-effecting operations the dispatcher needs from the host. The
/// shell crate implements this trait against the live `Session`,
/// `ActiveBuffer`, and `EditorHandle`s.
///
/// Trait methods are split into *queries* (immutable `&self`) and
/// *commands* (mutating `&mut self`) so most of the dispatcher's
/// branches can take only a shared borrow — useful for the future
/// case where a wnd_proc handler wants to query state without giving
/// the dispatcher the right to mutate.
///
/// **Phase 3 stubs are explicit:** methods that return `0` / `L_TEXT`
/// / `false` in milestone 3 do so deliberately (no lexer or multi-tab
/// yet); milestones 4 and 5 fill them in. The trait shape is fixed by
/// the plugin-ABI freeze at end of Phase 3 — Phase 4 wires up
/// behaviour without changing signatures.
pub trait HostServices {
    /// HWND of the active Scintilla view (the one with focus).
    fn current_scintilla_hwnd(&self) -> Hwnd;
    /// HWND of view 0 (main) or 1 (secondary). Phase 3 has only main;
    /// `view==1` returns NULL until split-view lands.
    fn scintilla_hwnd_for_view(&self, view: i32) -> Hwnd;
    /// Active buffer's id. Phase 3 single-tab returns a stable
    /// nonzero id; multi-tab returns the per-tab id.
    fn current_buffer_id(&self) -> isize;
    /// Path of the buffer with id `id`. None if the id is unknown.
    fn buffer_path(&self, id: isize) -> Option<PathBuf>;
    /// Lang-type for buffer `id`. Phase 3 returns `L_TEXT` (0); Phase
    /// 4 wires this through the lexer registry.
    fn buffer_lang_type(&self, id: isize) -> i32;
    /// Per-install plugin config directory.
    fn plugins_config_dir(&self) -> PathBuf;
    /// HMENU for `which` (NPPPLUGINMENU or NPPMAINMENU). NULL means
    /// the menu isn't available yet (e.g. early in startup).
    fn menu_handle(&self, which: i32) -> Hwnd;

    fn set_status_bar(&mut self, section: usize, text: String);
    fn open_file(&mut self, path: PathBuf);
    fn reload_file(&mut self, path: Option<PathBuf>);
    fn save_current_file(&mut self);
    fn switch_to_file(&mut self, path: PathBuf) -> bool;
    fn menu_command(&mut self, cmd_id: i32);
    fn make_current_buffer_dirty(&mut self);
    fn set_buffer_lang_type(&mut self, id: isize, lang: i32) -> bool;
    fn set_menu_item_check(&mut self, cmd_id: i32, checked: bool);
    /// Activate the buffer at index `pos` in view `view`. Phase 3
    /// single-tab is a no-op success.
    fn activate_doc(&mut self, view: i32, pos: i32) -> bool;
}

/// Dispatch an inbound NPPM_* message. Returns `Some(lresult)` if the
/// message is in the NPPM_* numeric range, or `None` if it should
/// fall through to the host's default wnd_proc.
///
/// Unknown messages **inside** the NPPM_* range return `Some(0)` and
/// log a `tracing::warn!`. That's the documented contract: plugins
/// compiled against a future header version always *link*, and
/// missing coverage surfaces at runtime via the log.
///
/// # Safety
///
/// Several NPPM_* messages carry an out-pointer in `lparam` (e.g.
/// `NPPM_GETPLUGINSCONFIGDIR` writes a wide string). The dispatcher
/// trusts the plugin's pointer to be non-null and to point at a
/// buffer of at least [`MAX_PATH_TCHARS`] units, which is the
/// documented contract from the public ABI. A buggy plugin that
/// passes a smaller buffer or NULL invokes UB on its own behalf —
/// same as Notepad++. We bound writes at `MAX_PATH_TCHARS` to keep
/// the host's blast radius constant.
pub unsafe fn dispatch_nppm<S: HostServices>(
    services: &mut S,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> Option<isize> {
    // Stay inside a generous NPPM_* range; out-of-range falls back to
    // the default wnd_proc so non-plugin WM_USER+N messages from the
    // host's own UI continue to dispatch normally. The compat header
    // currently tops out at NPPMSG+102; +200 is generous headroom for
    // v3 additions before this guard needs revisiting.
    if !(NPPMSG..NPPMSG + 200).contains(&msg) {
        return None;
    }

    Some(match msg {
        NPPM_GETCURRENTSCINTILLA => {
            // lParam is `int*` OUT — the active view index, 0 (main)
            // or 1 (secondary). Phase 3 single-view always reports 0.
            // Plugins read NppData._scintillaMainHandle or
            // _scintillaSecondHandle (set by setInfo) to get the
            // HWND; the LRESULT is the view index, **not** the HWND.
            // Returning the HWND here would break plugins that gate
            // on `lresult == 0 ? main : second` to pick a view.
            if lparam != 0 {
                // SAFETY: plugin promises lparam is a valid `int*` it
                // owns and that lives for the duration of this call.
                // Use `write_unaligned` because a malicious or buggy
                // plugin can pass an unaligned pointer; an aligned
                // `*mut i32` store would be UB (silent corruption on
                // x86, hardware fault on ARM64 Windows).
                unsafe {
                    core::ptr::write_unaligned(lparam as *mut i32, 0);
                }
            }
            0
        }

        NPPM_GETCURRENTLANGTYPE => {
            // lParam is `LangType*` OUT.
            let id = services.current_buffer_id();
            let lang = services.buffer_lang_type(id);
            if lparam != 0 {
                // SAFETY: plugin promises lparam is a valid LangType*
                // (sizeof == int) it owns. write_unaligned for the
                // same reason as NPPM_GETCURRENTSCINTILLA above.
                unsafe {
                    core::ptr::write_unaligned(lparam as *mut i32, lang);
                }
            }
            1 // TRUE
        }

        NPPM_SETCURRENTLANGTYPE => {
            let lang = lparam as i32;
            let id = services.current_buffer_id();
            if services.set_buffer_lang_type(id, lang) {
                1
            } else {
                0
            }
        }

        NPPM_SETSTATUSBAR => {
            // wParam: section index (0 = doc info, 1 = type/encoding,
            // …). lParam: const TCHAR* (null-terminated).
            if lparam == 0 {
                0
            } else {
                // SAFETY: plugin promises lparam is a valid
                // null-terminated wide string.
                let text = unsafe { wide_ptr_to_string(lparam as *const u16) };
                services.set_status_bar(wparam, text);
                1
            }
        }

        NPPM_GETMENUHANDLE => services.menu_handle(wparam as i32) as isize,

        NPPM_ACTIVATEDOC => {
            let view = wparam as i32;
            let pos = lparam as i32;
            if services.activate_doc(view, pos) {
                1
            } else {
                0
            }
        }

        NPPM_RELOADFILE => {
            // wParam is a BOOL: TRUE = alert user before reload (we
            // route through the normal reload path, which prompts).
            // lParam: optional TCHAR* path. NULL = current buffer.
            let path = if lparam == 0 {
                None
            } else {
                // SAFETY: plugin promises lparam is a valid wide path
                // it owns for the duration of this call.
                Some(PathBuf::from(unsafe {
                    wide_ptr_to_string(lparam as *const u16)
                }))
            };
            services.reload_file(path);
            1
        }

        NPPM_SWITCHTOFILE => {
            if lparam == 0 {
                0
            } else {
                // SAFETY: plugin promises lparam is a valid wide path.
                let path = PathBuf::from(unsafe { wide_ptr_to_string(lparam as *const u16) });
                if services.switch_to_file(path) {
                    1
                } else {
                    0
                }
            }
        }

        NPPM_SAVECURRENTFILE => {
            services.save_current_file();
            1
        }

        NPPM_SETMENUITEMCHECK => {
            services.set_menu_item_check(wparam as i32, lparam != 0);
            1
        }

        NPPM_GETWINDOWSVERSION => {
            // Notepad++'s `winVer` enum has `WV_WIN10 = 16`,
            // `WV_WIN11 = 17`. Code++ doesn't yet sniff the OS
            // version; reporting WV_WIN10 keeps plugins that gate on
            // `>= WV_WIN10` happy without claiming features we don't
            // probe. Phase 4 may upgrade this via RtlGetVersion.
            16
        }

        NPPM_MAKECURRENTBUFFERDIRTY => {
            services.make_current_buffer_dirty();
            1
        }

        NPPM_GETPLUGINSCONFIGDIR => {
            // wParam: capacity in TCHARs. lParam: TCHAR* OUT.
            // Reject `wparam == 0` and `lparam == 0` with a 0
            // (failure) return — `Some(1)` claims success but
            // would leave the plugin's buffer unwritten, producing
            // garbage on the read side.
            if lparam == 0 || wparam == 0 {
                0
            } else {
                let dir = services.plugins_config_dir();
                let cap = wparam.min(MAX_PATH_TCHARS);
                // SAFETY: plugin promises lparam points to a wide
                // buffer of at least `wparam` TCHARs (we further cap
                // at MAX_PATH_TCHARS).
                unsafe {
                    write_wide_path(lparam as *mut u16, cap, &dir);
                }
                1
            }
        }

        NPPM_MENUCOMMAND => {
            services.menu_command(lparam as i32);
            1
        }

        NPPM_GETNPPVERSION => CODEPP_PLUGIN_API_VERSION,

        NPPM_GETFULLPATHFROMBUFFERID => {
            // wParam: buffer id. lParam: TCHAR* OUT (caller-allocated).
            // Returns: number of TCHARs written (incl. null) on
            // success, or -1 if the id is unknown.
            let id = wparam as isize;
            let Some(path) = services.buffer_path(id) else {
                return Some(-1);
            };
            if lparam == 0 {
                // Probe: return the documented buffer-size contract
                // for this message (MAX_PATH minimum). Returning the
                // *actual* path length would let a plugin allocate
                // less than MAX_PATH and then take a write up to
                // MAX_PATH_TCHARS units on the second call — a
                // host-side overflow into plugin memory.
                MAX_PATH_TCHARS as isize
            } else {
                // SAFETY: per the ABI, plugin promises lparam points
                // to a wide buffer of at least MAX_PATH_TCHARS.
                let written =
                    unsafe { write_wide_path(lparam as *mut u16, MAX_PATH_TCHARS, &path) };
                written as isize
            }
        }

        NPPM_GETCURRENTBUFFERID => services.current_buffer_id(),

        NPPM_GETBUFFERLANGTYPE => services.buffer_lang_type(wparam as isize) as isize,

        NPPM_DOOPEN => {
            if lparam == 0 {
                0
            } else {
                // SAFETY: plugin promises lparam is a valid wide path.
                let path = PathBuf::from(unsafe { wide_ptr_to_string(lparam as *const u16) });
                services.open_file(path);
                1
            }
        }

        // Known NPPM_* range, but no v1 implementation. Plugins that
        // depend on the real semantics of these messages will see
        // sensible defaults (zero) and a log entry.
        _ => {
            tracing::warn!(msg = msg, "unhandled NPPM_*");
            0
        }
    })
}

/// Decode a null-terminated wide-char string at `p` into an owned
/// `String`. Bounded to `MAX_PATH_TCHARS` units to keep a missing
/// terminator from running off into arbitrary memory; truncation is
/// preferable to a buffer over-read.
///
/// # Safety
///
/// `p` must be either NULL or a valid pointer to a null-terminated
/// (or at-least-`MAX_PATH_TCHARS`-long) sequence of `u16` values.
unsafe fn wide_ptr_to_string(mut p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::with_capacity(64);
    let mut count = 0usize;
    // SAFETY: bounded by MAX_PATH_TCHARS; null-terminator stops the loop.
    unsafe {
        while count < MAX_PATH_TCHARS {
            let c = *p;
            if c == 0 {
                break;
            }
            units.push(c);
            p = p.add(1);
            count += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

/// Write `path` as a null-terminated wide string into the buffer at
/// `dst`, capped at `cap` TCHARs (incl. trailing null). Returns the
/// number of TCHARs written, **including** the trailing null. If
/// `path` is longer than `cap - 1`, the result is truncated; the
/// truncated string is still null-terminated.
///
/// # Safety
///
/// `dst` must point to a writable buffer of at least `cap` `u16`
/// units.
unsafe fn write_wide_path(dst: *mut u16, cap: usize, path: &std::path::Path) -> usize {
    if cap == 0 || dst.is_null() {
        return 0;
    }
    let mut units: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    // Reserve one unit for the null terminator.
    if units.len() > cap - 1 {
        // Capture the pre-truncation length for the log so a
        // future maintainer chasing a truncation warning can see
        // how badly we ran past the cap; logging post-truncation
        // length always prints `cap - 1` and is useless.
        let original_len = units.len();
        units.truncate(cap - 1);
        tracing::warn!(
            cap = cap,
            original_len = original_len,
            "wide path truncated to fit plugin buffer",
        );
    }
    units.push(0);
    // SAFETY: caller promises `dst` has `cap` writable u16 units;
    // we wrote at most `cap` (truncation enforced above).
    unsafe {
        core::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
    }
    units.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;

    /// In-memory `HostServices` that records mutations and returns
    /// configured query values. Lets us exercise every dispatcher
    /// branch without a real shell or window.
    #[derive(Default)]
    struct MockServices {
        active_scintilla: usize,
        secondary_scintilla: usize,
        current_buffer: isize,
        buffer_paths: Vec<(isize, PathBuf)>,
        buffer_lang: i32,
        plugins_dir: PathBuf,
        plugin_menu: usize,
        main_menu: usize,
        // recorded mutations
        log: RefCell<Vec<String>>,
    }

    impl MockServices {
        fn record(&self, s: impl Into<String>) {
            self.log.borrow_mut().push(s.into());
        }
        fn calls(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl HostServices for MockServices {
        fn current_scintilla_hwnd(&self) -> Hwnd {
            self.active_scintilla as Hwnd
        }
        fn scintilla_hwnd_for_view(&self, view: i32) -> Hwnd {
            match view {
                0 => self.active_scintilla as Hwnd,
                1 => self.secondary_scintilla as Hwnd,
                _ => core::ptr::null_mut(),
            }
        }
        fn current_buffer_id(&self) -> isize {
            self.current_buffer
        }
        fn buffer_path(&self, id: isize) -> Option<PathBuf> {
            self.buffer_paths
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, p)| p.clone())
        }
        fn buffer_lang_type(&self, _id: isize) -> i32 {
            self.buffer_lang
        }
        fn plugins_config_dir(&self) -> PathBuf {
            self.plugins_dir.clone()
        }
        fn menu_handle(&self, which: i32) -> Hwnd {
            match which {
                NPPPLUGINMENU => self.plugin_menu as Hwnd,
                NPPMAINMENU => self.main_menu as Hwnd,
                _ => core::ptr::null_mut(),
            }
        }
        fn set_status_bar(&mut self, section: usize, text: String) {
            self.record(format!("status[{section}]={text}"));
        }
        fn open_file(&mut self, path: PathBuf) {
            self.record(format!("open={}", path.display()));
        }
        fn reload_file(&mut self, path: Option<PathBuf>) {
            self.record(format!(
                "reload={}",
                path.map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<current>".into())
            ));
        }
        fn save_current_file(&mut self) {
            self.record("save");
        }
        fn switch_to_file(&mut self, path: PathBuf) -> bool {
            self.record(format!("switch={}", path.display()));
            true
        }
        fn menu_command(&mut self, cmd_id: i32) {
            self.record(format!("menu_cmd={cmd_id}"));
        }
        fn make_current_buffer_dirty(&mut self) {
            self.record("dirty");
        }
        fn set_buffer_lang_type(&mut self, id: isize, lang: i32) -> bool {
            self.record(format!("set_lang[{id}]={lang}"));
            true
        }
        fn set_menu_item_check(&mut self, cmd_id: i32, checked: bool) {
            self.record(format!("check[{cmd_id}]={checked}"));
        }
        fn activate_doc(&mut self, view: i32, pos: i32) -> bool {
            self.record(format!("activate[{view},{pos}]"));
            true
        }
    }

    fn make_wide(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    #[test]
    fn out_of_range_msg_returns_none() {
        let mut s = MockServices::default();
        assert!(unsafe { dispatch_nppm(&mut s, WM_USER + 5, 0, 0) }.is_none());
        assert!(unsafe { dispatch_nppm(&mut s, NPPMSG - 1, 0, 0) }.is_none());
        assert!(unsafe { dispatch_nppm(&mut s, NPPMSG + 1000, 0, 0) }.is_none());
    }

    #[test]
    fn unknown_in_range_returns_zero_and_logs() {
        let mut s = MockServices::default();
        // (NPPMSG + 199) is in the dispatcher's range but unmapped.
        let r = unsafe { dispatch_nppm(&mut s, NPPMSG + 199, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn get_current_scintilla_writes_view_index() {
        let mut s = MockServices {
            active_scintilla: 0xABCD,
            ..Default::default()
        };
        let mut view: i32 = -1;
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETCURRENTSCINTILLA,
                0,
                &mut view as *mut i32 as isize,
            )
        };
        // LRESULT is the view index (0 for Phase 3 single-view), not
        // the HWND — plugins read the HWND from NppData.
        assert_eq!(r, Some(0));
        assert_eq!(view, 0);
    }

    #[test]
    fn get_current_lang_type_writes_out() {
        let mut s = MockServices {
            current_buffer: 7,
            buffer_lang: 5,
            ..Default::default()
        };
        let mut lang: i32 = -1;
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETCURRENTLANGTYPE,
                0,
                &mut lang as *mut i32 as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(lang, 5);
    }

    #[test]
    fn set_status_bar_decodes_and_records() {
        let mut s = MockServices::default();
        let text = make_wide("Ready.");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETSTATUSBAR, 2, text.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["status[2]=Ready."]);
    }

    #[test]
    fn set_status_bar_null_lparam_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETSTATUSBAR, 0, 0) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn save_current_dispatches() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVECURRENTFILE, 0, 0) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save"]);
    }

    #[test]
    fn doopen_decodes_path() {
        let mut s = MockServices::default();
        let p = make_wide("C:/work/foo.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DOOPEN, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["open=C:/work/foo.txt"]);
    }

    #[test]
    fn doopen_null_lparam_is_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DOOPEN, 0, 0) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn get_npp_version_returns_phase3_version() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETNPPVERSION, 0, 0) };
        assert_eq!(r, Some(CODEPP_PLUGIN_API_VERSION));
    }

    #[test]
    fn get_current_buffer_id_returns_active() {
        let mut s = MockServices {
            current_buffer: 42,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETCURRENTBUFFERID, 0, 0) };
        assert_eq!(r, Some(42));
    }

    #[test]
    fn get_full_path_writes_buffer() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/notes.txt"))],
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETFULLPATHFROMBUFFERID,
                7,
                buf.as_mut_ptr() as isize,
            )
        };
        let written = r.unwrap();
        assert!(written > 0);
        // Decode back, drop trailing null.
        let len = (written as usize).saturating_sub(1);
        let s = String::from_utf16_lossy(&buf[..len]);
        assert_eq!(s, "D:/notes.txt");
    }

    #[test]
    fn get_full_path_unknown_id_returns_minus_one() {
        let mut s = MockServices::default();
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETFULLPATHFROMBUFFERID,
                999,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn get_full_path_probe_returns_max_path() {
        // Probe (lParam == 0) must always return MAX_PATH_TCHARS,
        // never the actual path length. Returning the actual length
        // lets a plugin allocate less than MAX_PATH and then take a
        // host-side overflow on the second call.
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/short.txt"))],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETFULLPATHFROMBUFFERID, 7, 0) };
        assert_eq!(r, Some(MAX_PATH_TCHARS as isize));
    }

    #[test]
    fn get_plugins_config_dir_writes_buffer() {
        let mut s = MockServices {
            plugins_dir: PathBuf::from("E:/codepp/plugins/config"),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETPLUGINSCONFIGDIR,
                buf.len(),
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        let nul = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        assert_eq!(
            String::from_utf16_lossy(&buf[..nul]),
            "E:/codepp/plugins/config"
        );
    }

    #[test]
    fn get_plugins_config_dir_zero_capacity_returns_zero() {
        // Plugin passes `wparam == 0` (broken caller, or accidental
        // probe). Returning Some(1) would claim success without
        // having written; we must return Some(0).
        let mut s = MockServices {
            plugins_dir: PathBuf::from("E:/codepp/plugins/config"),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETPLUGINSCONFIGDIR,
                0,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(0));
        // Buffer must be untouched.
        assert!(buf.iter().all(|&u| u == 0));
    }

    #[test]
    fn write_wide_path_truncates_overlong_input() {
        // Construct a longer-than-cap path; write_wide_path must
        // truncate-with-null and not write past `cap`.
        let big = "x".repeat(MAX_PATH_TCHARS + 50);
        let mut buf = vec![0xFFFFu16; MAX_PATH_TCHARS + 4];
        let path = Path::new(&big);
        let written = unsafe { write_wide_path(buf.as_mut_ptr(), MAX_PATH_TCHARS, path) };
        assert_eq!(written, MAX_PATH_TCHARS);
        // The trailing slot we left at 0xFFFF must be untouched —
        // proves we didn't overrun.
        assert_eq!(buf[MAX_PATH_TCHARS], 0xFFFF);
        // And the in-bounds last slot is the NUL terminator.
        assert_eq!(buf[MAX_PATH_TCHARS - 1], 0);
    }

    #[test]
    fn menucommand_routes_cmd_id() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_MENUCOMMAND, 0, 41006) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["menu_cmd=41006"]);
    }

    #[test]
    fn reload_with_path_and_without() {
        let mut s = MockServices::default();
        unsafe { dispatch_nppm(&mut s, NPPM_RELOADFILE, 1, 0) };
        let p = make_wide("C:/x.txt");
        unsafe { dispatch_nppm(&mut s, NPPM_RELOADFILE, 1, p.as_ptr() as isize) };
        assert_eq!(s.calls(), vec!["reload=<current>", "reload=C:/x.txt"]);
    }

    #[test]
    fn switch_to_file_path() {
        let mut s = MockServices::default();
        let p = make_wide("D:/notes.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SWITCHTOFILE, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["switch=D:/notes.txt"]);
    }

    #[test]
    fn notification_codes_match_header() {
        // Sanity check: the NPPN_* constants we expose match the
        // header values plugins compile against.
        assert_eq!(Notification::Ready.code(), 1001);
        assert_eq!(Notification::TbModification.code(), 1002);
        assert_eq!(Notification::FileOpened { buffer_id: 0 }.code(), 1004);
        assert_eq!(Notification::FileClosed { buffer_id: 0 }.code(), 1005);
        assert_eq!(Notification::FileSaved { buffer_id: 0 }.code(), 1008);
        assert_eq!(Notification::Shutdown.code(), 1009);
        assert_eq!(Notification::BufferActivated { buffer_id: 0 }.code(), 1010);
        assert_eq!(Notification::LangChanged { buffer_id: 0 }.code(), 1011);
    }

    #[test]
    fn notify_all_with_no_plugins_is_noop() {
        // PluginHost in default state has zero plugins; notify_all
        // must not panic or attempt to call any function.
        let host = PluginHost::new();
        notify_all(&host, &Notification::Ready, core::ptr::null_mut());
    }

    #[test]
    fn nppmsg_constants_match_header() {
        // Lock the values in: changing any of these silently is an
        // ABI break against existing plugin DLLs.
        assert_eq!(NPPMSG, WM_USER + 1000);
        assert_eq!(NPPM_GETCURRENTSCINTILLA, NPPMSG + 4);
        assert_eq!(NPPM_GETCURRENTBUFFERID, NPPMSG + 60);
        assert_eq!(NPPM_DOOPEN, NPPMSG + 77);
        assert_eq!(NPPM_GETPLUGINSCONFIGDIR, NPPMSG + 46);
        assert_eq!(NPPM_GETNPPVERSION, NPPMSG + 50);
        assert_eq!(NPPN_FIRST, 1000);
    }
}
