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

/// Width of the NPPM_* numeric range the dispatcher claims. The
/// compat header currently tops out at NPPMSG+102; +200 gives
/// headroom for v3 additions before this guard needs revisiting.
/// Exposed publicly so wnd_proc pre-filters use the same bound as
/// the dispatcher's internal range check — keeping the two in sync
/// is otherwise a footgun when the bound is bumped.
pub const NPPMSG_RANGE: u32 = 200;

// --- v1 NPPM_* set ---------------------------------------------------

pub const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
pub const NPPM_GETCURRENTLANGTYPE: u32 = NPPMSG + 5;
pub const NPPM_SETCURRENTLANGTYPE: u32 = NPPMSG + 6;
pub const NPPM_GETNBOPENFILES: u32 = NPPMSG + 7;
pub const NPPM_GETOPENFILENAMES: u32 = NPPMSG + 8;
pub const NPPM_GETOPENFILENAMESPRIMARY: u32 = NPPMSG + 17;
pub const NPPM_GETOPENFILENAMESSECOND: u32 = NPPMSG + 18;
pub const NPPM_GETCURRENTDOCINDEX: u32 = NPPMSG + 23;
pub const NPPM_SETSTATUSBAR: u32 = NPPMSG + 24;
pub const NPPM_GETMENUHANDLE: u32 = NPPMSG + 25;
pub const NPPM_ENCODESCI: u32 = NPPMSG + 26;
pub const NPPM_DECODESCI: u32 = NPPMSG + 27;
pub const NPPM_ACTIVATEDOC: u32 = NPPMSG + 28;
/// Open the Find in Files dialog, optionally pre-filling the
/// directory (`wparam`, wide string) and filters (`lparam`, wide
/// string). Both pointers may be NULL — N++'s ABI treats them as
/// "use the dialog's current values".
pub const NPPM_LAUNCHFINDINFILESDLG: u32 = NPPMSG + 29;
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
pub const NPPM_RELOADBUFFERID: u32 = NPPMSG + 61;
pub const NPPM_GETBUFFERLANGTYPE: u32 = NPPMSG + 64;
pub const NPPM_SETBUFFERLANGTYPE: u32 = NPPMSG + 65;
pub const NPPM_GETBUFFERENCODING: u32 = NPPMSG + 66;
pub const NPPM_SETBUFFERENCODING: u32 = NPPMSG + 67;
pub const NPPM_GETBUFFERFORMAT: u32 = NPPMSG + 68;
pub const NPPM_SETBUFFERFORMAT: u32 = NPPMSG + 69;
pub const NPPM_DOOPEN: u32 = NPPMSG + 77;
pub const NPPM_GETLANGUAGENAME: u32 = NPPMSG + 83;
pub const NPPM_GETLANGUAGEDESC: u32 = NPPMSG + 84;

/// Selectors for [`NPPM_GETMENUHANDLE`].
pub const NPPPLUGINMENU: i32 = 0;
pub const NPPMAINMENU: i32 = 1;

/// Selectors for [`NPPM_GETNBOPENFILES`] / [`NPPM_GETOPENFILENAMES`]
/// (`wparam`). `ALL_OPEN_FILES` returns the union across views;
/// `PRIMARY_VIEW` and `SECOND_VIEW` request a per-view subset. The
/// dedicated messages [`NPPM_GETOPENFILENAMESPRIMARY`] /
/// [`NPPM_GETOPENFILENAMESSECOND`] are equivalent to passing those
/// selectors via [`NPPM_GETOPENFILENAMES`] but predate the unified
/// form in N++'s ABI; both are still used by plugins.
pub const ALL_OPEN_FILES: i32 = 0;
pub const PRIMARY_VIEW: i32 = 1;
pub const SECOND_VIEW: i32 = 2;

/// Encoding values returned by [`NPPM_GETBUFFERENCODING`]. Numeric
/// values match Notepad++'s public `UniMode` enum so plugins
/// compiled against either header read the same wire format.
/// `UNI_END` is the sentinel — never returned, but documented so
/// plugins doing `>= UNI_END` bounds checks behave the same way.
pub const UNI_8BIT: i32 = 0;
pub const UNI_UTF8: i32 = 1;
pub const UNI_UTF16BE: i32 = 2;
pub const UNI_UTF16LE: i32 = 3;
pub const UNI_COOKIE: i32 = 4;
pub const UNI_7BIT: i32 = 5;
pub const UNI_UTF16BE_NO_BOM: i32 = 6;
pub const UNI_UTF16LE_NO_BOM: i32 = 7;
pub const UNI_END: i32 = 8;

/// EOL format values returned by [`NPPM_GETBUFFERFORMAT`]. Numeric
/// values match Notepad++'s `EolType` so plugins read the same wire
/// codes here as they do in N++.
pub const WIN_FORMAT: i32 = 0;
pub const MAC_FORMAT: i32 = 1;
pub const UNIX_FORMAT: i32 = 2;

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
    /// Short language name for `lang` (`NPPM_GETLANGUAGENAME`). N++
    /// uses the same string the user sees in the Language menu —
    /// "C", "C++", "Rust", "Normal Text". `None` for langs whose
    /// name isn't known to the host (the dispatcher writes a zero-
    /// length wide string in that case so plugins observe "no
    /// name" rather than garbage).
    fn language_name(&self, lang: i32) -> Option<&'static str>;
    /// Long language description (`NPPM_GETLANGUAGEDESC`). Same
    /// `None` semantics as [`Self::language_name`].
    fn language_desc(&self, lang: i32) -> Option<&'static str>;
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
    /// Open the Find in Files dialog (FIF tab), optionally pre-
    /// filling the directory and filter combobox text. Both args
    /// `None` means "open the dialog with whatever the controls
    /// already hold". Used by `NPPM_LAUNCHFINDINFILESDLG` so a
    /// plugin can drive a project-wide search.
    fn launch_find_in_files_dialog(&mut self, directory: Option<PathBuf>, filters: Option<String>);

    /// Paths of files currently open in the requested view selector.
    /// `selector` matches the [`NPPM_GETNBOPENFILES`] /
    /// [`NPPM_GETOPENFILENAMES`] wparam contract:
    /// [`ALL_OPEN_FILES`], [`PRIMARY_VIEW`], or [`SECOND_VIEW`].
    /// Code++ is single-view through Phase 4, so `ALL` and
    /// `PRIMARY` return the same set and `SECOND` returns empty
    /// — the dispatcher relies on the impl honouring the selector
    /// rather than slicing here. Untitled tabs (no on-disk path)
    /// are omitted: N++'s ABI documents this surface as "open
    /// *files*", and a plugin that allocated a TCHAR** array
    /// expects each slot to receive a real path.
    fn open_buffer_paths(&self, selector: i32) -> Vec<PathBuf>;

    /// Index of the active tab in `view` (0 = primary, 1 = secondary)
    /// — same convention as [`Self::scintilla_hwnd_for_view`]. Returns
    /// `-1` when the view has no active tab (the only "no view" the
    /// secondary path produces in single-view Code++).
    fn current_doc_index(&self, view: i32) -> i32;

    /// Encoding (UniMode) of the buffer with id `id`. Return values
    /// match Notepad++'s `UniMode` enum: see [`UNI_8BIT`] /
    /// [`UNI_UTF8`] / [`UNI_UTF16BE`] / [`UNI_UTF16LE`] /
    /// [`UNI_COOKIE`] / [`UNI_7BIT`] / [`UNI_UTF16BE_NO_BOM`] /
    /// [`UNI_UTF16LE_NO_BOM`]. Returns `-1` when `id` is unknown so
    /// plugins can distinguish "unknown buffer" from "valid 8-bit
    /// buffer" (UniMode 0).
    fn buffer_encoding(&self, id: isize) -> i32;

    /// EOL format of the buffer with id `id`. Returns one of
    /// [`WIN_FORMAT`] / [`MAC_FORMAT`] / [`UNIX_FORMAT`], or `-1`
    /// when `id` is unknown. Code++'s internal `Eol::Mixed` (a
    /// per-line preservation mode N++ does not have) maps to
    /// [`UNIX_FORMAT`] — the modern default and the one Code++'s
    /// "Edit → EOL Conversion" picks if the user normalises a
    /// mixed buffer.
    fn buffer_format(&self, id: isize) -> i32;

    /// Reload the buffer identified by `id` from disk, blowing away
    /// any in-memory edits that have not been saved.
    ///
    /// `with_alert == true` means the plugin asked for the
    /// "modified externally — reload?" confirmation prompt to
    /// surface; `false` means a silent reload. **Phase 4
    /// limitation:** Code++ silently reloads in both cases. The
    /// confirmation-prompt path requires routing through the
    /// per-window pending-dialog queue, which the dispatcher
    /// doesn't currently access. Tracked as a follow-up; plugins
    /// passing `with_alert == true` get a `tracing::warn!` so the
    /// gap is visible in the log.
    ///
    /// Returns `true` if the reload was issued (the buffer id was
    /// known and had an associated path), `false` otherwise. Same
    /// "ok / unknown" shape as [`Self::set_buffer_lang_type`].
    fn reload_buffer_id(&mut self, id: isize, with_alert: bool) -> bool;

    /// Set the save-time encoding of the buffer with id `id` from a
    /// [`UNI_8BIT`] / [`UNI_UTF8`] / [`UNI_UTF16BE`] / [`UNI_UTF16LE`]
    /// / [`UNI_COOKIE`] / [`UNI_UTF16BE_NO_BOM`] /
    /// [`UNI_UTF16LE_NO_BOM`] numeric. Mirrors the menu-driven
    /// `Shell::set_buffer_encoding` but works on any open buffer
    /// rather than just the active one — that's the contract
    /// `NPPM_SETBUFFERENCODING` plugins expect.
    ///
    /// [`UNI_7BIT`] is rejected (returns `false`): Code++'s detection
    /// pipeline never produces this value (pure ASCII is reported as
    /// `UNI_COOKIE`), and there is no exact-match `Encoding` variant
    /// for "ASCII". A plugin asking for it likely wants `UNI_COOKIE`
    /// (UTF-8 without BOM) which is the natural superset.
    ///
    /// Returns `false` for unknown buffer id, unknown UniMode value,
    /// or `UNI_7BIT` per the rule above.
    fn set_buffer_encoding(&mut self, id: isize, unimode: i32) -> bool;

    /// Set the EOL format of the buffer with id `id` from a
    /// [`WIN_FORMAT`] / [`MAC_FORMAT`] / [`UNIX_FORMAT`] numeric.
    ///
    /// **Phase 4 limitation:** the change is metadata-only — the
    /// existing line-ending bytes inside the Scintilla document are
    /// NOT rewritten. The next save still encodes the buffer text
    /// through `tab.encoding`, so the file's bytes are correct
    /// only if the buffer's in-memory line endings already match
    /// the new format (which is true for empty buffers and any
    /// buffer the user reloads after the metadata change). N++
    /// additionally issues `SCI_CONVERTEOLS` to rewrite the bytes
    /// in place — that needs a UI-side hook (the doc-pointer-swap
    /// dance to reach a non-active buffer) tracked in DESIGN.md
    /// §7.4.
    ///
    /// Returns `false` for unknown buffer id or unknown EolType.
    fn set_buffer_format(&mut self, id: isize, eoltype: i32) -> bool;

    /// Convert the active buffer of `view` (0 = primary,
    /// 1 = secondary) to UTF-8 (no BOM). The N++ contract for
    /// `NPPM_ENCODESCI`: switch the Scintilla view's bytes to
    /// UTF-8 and report the new encoding. Code++'s Scintilla view
    /// is *always* UTF-8 internally (we set `SC_CP_UTF8` at create
    /// time), so the byte representation needs no work — the only
    /// observable change is `tab.encoding` flipping to
    /// [`codepp_core::Encoding::Utf8`] (UNI_COOKIE), which is
    /// what the next save will produce.
    ///
    /// Returns the new encoding numeric ([`UNI_COOKIE`]) on
    /// success, or `-1` if the requested view has no active
    /// buffer (the only failure mode in single-view Code++ is
    /// `view == 1`, the secondary view, which is empty).
    fn encode_sci(&mut self, view: i32) -> i32;

    /// Inverse of [`Self::encode_sci`]: switch the active buffer
    /// of `view` to single-byte ANSI (the system codepage). N++
    /// uses `SCI_SETCODEPAGE(0)` here; in Code++ the equivalent is
    /// flipping `tab.encoding` to [`codepp_core::Encoding::Other`]
    /// with the system-codepage WHATWG label. The Scintilla view
    /// itself stays in UTF-8 mode — we don't unwind the
    /// `SC_CP_UTF8` setting because Code++'s internal model
    /// requires UTF-8 in the buffer; the on-disk encoding is what
    /// the user actually picks via the metadata.
    ///
    /// Returns the new encoding numeric ([`UNI_8BIT`]) on
    /// success, or `-1` if the view has no active buffer.
    fn decode_sci(&mut self, view: i32) -> i32;
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
    // host's own UI continue to dispatch normally. See `NPPMSG_RANGE`
    // for the bound's rationale and the wnd_proc pre-filter that
    // shares the same constant.
    if !(NPPMSG..NPPMSG + NPPMSG_RANGE).contains(&msg) {
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

        NPPM_GETNBOPENFILES => {
            // wparam: selector ([`ALL_OPEN_FILES`], [`PRIMARY_VIEW`],
            // [`SECOND_VIEW`]). Returns the number of files in the
            // requested set.
            services.open_buffer_paths(wparam as i32).len() as isize
        }

        NPPM_GETOPENFILENAMES | NPPM_GETOPENFILENAMESPRIMARY | NPPM_GETOPENFILENAMESSECOND => {
            // wparam: TCHAR** OUT — array of plugin-allocated wide
            // buffers, each at least MAX_PATH_TCHARS units. **Not a
            // selector** — the plain `GETOPENFILENAMES` form is
            // implicitly `ALL_OPEN_FILES` because wparam is consumed
            // as the pointer here. The -PRIMARY / -SECOND aliases
            // override the selector via the message number.
            //
            // lparam: capacity of the array (in slots, not in TCHARs).
            //
            // We keep the three messages in one arm rather than
            // forking handlers so the truncation / pointer-validity
            // logic lives in exactly one place.
            let selector = match msg {
                NPPM_GETOPENFILENAMESPRIMARY => PRIMARY_VIEW,
                NPPM_GETOPENFILENAMESSECOND => SECOND_VIEW,
                NPPM_GETOPENFILENAMES => ALL_OPEN_FILES,
                // Outer match guarantees we entered through one of
                // the three messages above; this arm is unreachable
                // and exists only to satisfy `match` exhaustiveness
                // when the message list is extended.
                _ => unreachable!("NPPM_GETOPENFILENAMES* outer match guarantees three msgs"),
            };
            if wparam == 0 {
                // NULL out-array — plugin is asking "how many
                // would you write?" without committing storage.
                // Mirrors the NPPM_GETFULLPATHFROMBUFFERID probe
                // contract; works identically for the -PRIMARY /
                // -SECOND aliases (selector is honoured before the
                // probe short-circuit).
                return Some(services.open_buffer_paths(selector).len() as isize);
            }
            let cap = lparam.max(0) as usize;
            let paths = services.open_buffer_paths(selector);
            // `cap` from the plugin can be arbitrarily large, but
            // the host iterates only up to `paths.len()` — bounded
            // by host-controlled state, not by plugin input. The
            // assert documents that invariant for future maintainers
            // who might add lazy iteration / plugin-driven filters.
            let to_write = paths.len().min(cap);
            debug_assert!(to_write <= paths.len());
            // SAFETY: plugin promises wparam is a valid pointer to
            // an array of at least `cap` `*mut u16` slots, each
            // pointing to a writable wide buffer of at least
            // MAX_PATH_TCHARS units. The pointer-array is read with
            // `read_unaligned` per slot to tolerate misaligned
            // input, matching the NPPM_GETCURRENTSCINTILLA pattern.
            let arr = wparam as *const *mut u16;
            // Track slots we *actually* wrote — distinct from
            // `to_write`, which counts slots we attempted. A null
            // slot pointer is a plugin bug (the contract requires
            // every slot to point at a real wide buffer), so we
            // log it and skip rather than crash; the return value
            // reports honestly so the plugin can detect the gap by
            // comparing against `cap`.
            let mut written_slots = 0usize;
            for (i, path) in paths.iter().take(to_write).enumerate() {
                let slot_ptr = unsafe { arr.add(i) };
                let dst = unsafe { core::ptr::read_unaligned(slot_ptr) };
                if dst.is_null() {
                    tracing::warn!(
                        slot = i,
                        "NPPM_GETOPENFILENAMES: plugin slot pointer is null; skipping",
                    );
                    continue;
                }
                let _ = unsafe { write_wide_path(dst, MAX_PATH_TCHARS, path) };
                written_slots += 1;
            }
            written_slots as isize
        }

        NPPM_GETCURRENTDOCINDEX => {
            // wparam: view selector (0 = primary, 1 = secondary).
            // Returns the active tab index in that view, or -1 if
            // the view has no active tab. N++ uses `int` here, so
            // the negative-on-empty signal fits. The `i as i32` cast
            // in `HostBridge::current_doc_index` is safe because
            // `MAX_SESSION_TABS = 512` (tab count cap in `core::session`).
            services.current_doc_index(wparam as i32) as isize
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
                let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
                if decoded.is_empty() {
                    // Bad surrogates → empty; treat as "no path
                    // supplied" rather than reloading the empty path.
                    None
                } else {
                    Some(PathBuf::from(decoded))
                }
            };
            services.reload_file(path);
            1
        }

        NPPM_SWITCHTOFILE => {
            if lparam == 0 {
                0
            } else {
                // SAFETY: plugin promises lparam is a valid wide path.
                let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
                if decoded.is_empty() {
                    return Some(0);
                }
                if services.switch_to_file(PathBuf::from(decoded)) {
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

        NPPM_SETBUFFERLANGTYPE => {
            // wparam: buffer id, lparam: LangType i32 (signed). The
            // host fires NPPN_LANGCHANGED on the *change*, not on
            // every call — a no-op set (same lang already on the
            // buffer) returns success without re-styling, since
            // re-applying the same lexer would visibly flicker the
            // colours and the notification would be a false-positive.
            // The trait impl is responsible for that idempotence.
            if services.set_buffer_lang_type(wparam as isize, lparam as i32) {
                1
            } else {
                0
            }
        }

        NPPM_GETBUFFERENCODING => {
            // wparam: buffer id. Return value is a `UniMode` enum
            // numeric (see UNI_* constants above). `-1` for unknown
            // id — distinct from `UNI_8BIT` (0) so a plugin can tell
            // "no such buffer" from "8-bit buffer".
            services.buffer_encoding(wparam as isize) as isize
        }

        NPPM_GETBUFFERFORMAT => {
            // wparam: buffer id. Return value is an `EolType` numeric
            // (`WIN_FORMAT` / `MAC_FORMAT` / `UNIX_FORMAT`); `-1` for
            // unknown id, same separation rationale as
            // `NPPM_GETBUFFERENCODING`.
            services.buffer_format(wparam as isize) as isize
        }

        NPPM_SETBUFFERENCODING => {
            // wparam: buffer id. lparam: UniMode numeric. Returns
            // 1 on success (id resolved AND value accepted — same
            // value as already set is also success per the N++
            // "buffer is in the requested state" contract), 0 on
            // unknown id / unknown UniMode / UNI_7BIT (no exact
            // `Encoding` variant — see the trait doc-comment for
            // the rationale).
            services.set_buffer_encoding(wparam as isize, lparam as i32) as isize
        }

        NPPM_ENCODESCI => {
            // wparam: view selector (0 = primary, 1 = secondary).
            // Returns the new encoding numeric (UNI_COOKIE) on
            // success, or -1 when the view has no active buffer.
            // The pre-check rejects out-of-range wparams *before*
            // the `usize -> i32` truncation: without it, a plugin
            // passing `0x1_0000_0000` would truncate to 0 and be
            // silently accepted as "primary view". Phase 5
            // split-view will need to widen this to `<= 1`; the
            // explicit bound flags that maintenance point.
            if wparam > 1 {
                return Some(-1);
            }
            services.encode_sci(wparam as i32) as isize
        }

        NPPM_DECODESCI => {
            // Same wparam pre-check rationale as NPPM_ENCODESCI.
            if wparam > 1 {
                return Some(-1);
            }
            services.decode_sci(wparam as i32) as isize
        }

        NPPM_SETBUFFERFORMAT => {
            // wparam: buffer id. lparam: EolType numeric. Returns
            // 1 on success (same "is in the requested state"
            // semantics as SETBUFFERENCODING), 0 on unknown id or
            // unknown EolType. Phase 4 metadata-only — see the
            // trait doc-comment for the SCI_CONVERTEOLS deferral.
            services.set_buffer_format(wparam as isize, lparam as i32) as isize
        }

        NPPM_RELOADBUFFERID => {
            // wparam: buffer id. lparam: BOOL — TRUE asks for the
            // "reload?" confirmation, FALSE for a silent reload.
            // Returns 1 on success (id resolved to a path and
            // reload was issued), 0 if the id is unknown.
            let id = wparam as isize;
            let with_alert = lparam != 0;
            if services.reload_buffer_id(id, with_alert) {
                1
            } else {
                0
            }
        }

        NPPM_DOOPEN => {
            if lparam == 0 {
                0
            } else {
                // SAFETY: plugin promises lparam is a valid wide path.
                let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
                if decoded.is_empty() {
                    return Some(0);
                }
                services.open_file(PathBuf::from(decoded));
                1
            }
        }

        NPPM_LAUNCHFINDINFILESDLG => {
            // wparam: directory (TCHAR*) — optional pre-fill.
            // lparam: filters (TCHAR*) — optional pre-fill.
            // Either / both NULL means "open the dialog with
            // whatever the controls already hold". Empty wide
            // strings are treated as NULL — `wide_ptr_to_string`
            // returns "" on a bad-surrogate decode, and we don't
            // want a single bad UTF-16 unit to trash a good
            // pre-fill on the other arg.
            //
            // Both pointers are read by SAFETY contract from the
            // plugin: each must be either NULL or a valid wide
            // null-terminated buffer.
            let directory = if wparam == 0 {
                None
            } else {
                let s = unsafe { wide_ptr_to_string(wparam as *const u16) };
                if s.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(s))
                }
            };
            let filters = if lparam == 0 {
                None
            } else {
                let s = unsafe { wide_ptr_to_string(lparam as *const u16) };
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            };
            services.launch_find_in_files_dialog(directory, filters);
            1
        }

        NPPM_GETLANGUAGENAME => {
            // wparam: LangType id (i32 widened to usize by Win32's
            // `WPARAM`). lparam: wide char* OUT, or 0 for size-probe.
            // Returns count of TCHARs needed (incl. NUL) on probe /
            // count written on store. 0 on unknown lang — plugins
            // that expect a valid lang already bounded their wparam.
            //
            // `wparam as i32` truncates the upper 32 bits. The N++
            // ABI specifies LangType is a 32-bit signed int (the
            // `LangType_` enum), so a plugin sending a wParam whose
            // high bits are non-zero is malformed; the truncation
            // matches the ABI without an explicit guard.
            write_lang_string_with_probe(services.language_name(wparam as i32), lparam)
        }

        NPPM_GETLANGUAGEDESC => {
            // Same shape as NPPM_GETLANGUAGENAME above; longer
            // human-readable description, same probe contract,
            // same `wparam as i32` ABI truncation.
            write_lang_string_with_probe(services.language_desc(wparam as i32), lparam)
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
/// Returns the empty string if the wide payload contains unpaired
/// surrogates: `from_utf16_lossy` would silently substitute U+FFFD
/// for each bad surrogate, which on a path-typed payload would
/// reroute the open to a *different* valid path that shares the
/// non-surrogate prefix. Empty-string return makes the caller's
/// error path hit (it null-checks the result before forwarding).
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
    let s = String::from_utf16_lossy(&units);
    if s.contains('\u{FFFD}') {
        // Reject rather than mangle: a path with a substituted
        // U+FFFD might resolve to a different file than the plugin
        // intended, and the dispatcher routes path-typed payloads
        // straight into open_file/reload_file/switch_to_file.
        tracing::warn!(
            len = units.len(),
            "plugin-supplied wide string contained invalid surrogates; rejecting",
        );
        return String::new();
    }
    s
}

/// Shared body for `NPPM_GETLANGUAGENAME` / `NPPM_GETLANGUAGEDESC`.
/// Implements the N++ contract: `lparam == 0` means "probe — return
/// the number of TCHARs the host *will write* if a buffer of that
/// size is supplied"; otherwise write up to `MAX_PATH_TCHARS` units
/// into the plugin's buffer and return the count actually written.
/// `None` (unknown lang) reports zero on both probe and write —
/// plugins reading the probe see "no name available" rather than
/// allocating a one-NUL buffer that would silently match an empty
/// real name.
///
/// Probe and write must agree on the same number for the protocol
/// to hold: a plugin that allocates `probe` units expects the write
/// to fill exactly that many. So the probe path applies the same
/// `MAX_PATH_TCHARS` cap the write path applies — for any future
/// language name long enough to be truncated, both paths now report
/// the truncated length.
fn write_lang_string_with_probe(name: Option<&'static str>, lparam: isize) -> isize {
    let Some(name) = name else { return 0 };
    if lparam == 0 {
        // +1 for the trailing NUL the host always writes; cap at
        // MAX_PATH_TCHARS to match the truncation `write_wide_path`
        // applies on the write side.
        let codeunit_count = name.encode_utf16().count().min(MAX_PATH_TCHARS - 1);
        (codeunit_count + 1) as isize
    } else {
        // SAFETY: per the documented ABI, plugin promises `lparam`
        // points to a wide buffer of at least `MAX_PATH_TCHARS`
        // (260) units. `Path::new(&str)` is a zero-cost cast — we
        // reuse `write_wide_path` for the truncation+NUL logic
        // rather than duplicate its bounds-checking code. The
        // helper's name says "path" because that was the first
        // user; the implementation is plain UTF-16 writeout.
        let written = unsafe {
            write_wide_path(
                lparam as *mut u16,
                MAX_PATH_TCHARS,
                std::path::Path::new(name),
            )
        };
        written as isize
    }
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
        /// Open file paths in the primary view, in tab order.
        /// Empty means "no open files". Drives the
        /// `NPPM_GETNBOPENFILES` / `NPPM_GETOPENFILENAMES` tests.
        open_files_primary: Vec<PathBuf>,
        /// Active tab index in the primary view (used for
        /// `NPPM_GETCURRENTDOCINDEX`). `-1` means no active tab.
        active_tab_primary: i32,
        /// Per-buffer encoding (UniMode integer). Looked up by
        /// buffer id; missing entries return `-1` matching the
        /// dispatcher's "unknown id" contract.
        buffer_encodings: Vec<(isize, i32)>,
        /// Per-buffer EOL format (EolType integer). Same lookup
        /// shape as `buffer_encodings`.
        buffer_formats: Vec<(isize, i32)>,
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
        fn language_name(&self, lang: i32) -> Option<&'static str> {
            match lang {
                0 => Some("Normal Text"),
                3 => Some("C++"),
                81 => Some("Rust"),
                _ => None,
            }
        }
        fn language_desc(&self, lang: i32) -> Option<&'static str> {
            match lang {
                0 => Some("Normal text file"),
                3 => Some("C++ source file"),
                81 => Some("Rust source file"),
                _ => None,
            }
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
        fn launch_find_in_files_dialog(
            &mut self,
            directory: Option<PathBuf>,
            filters: Option<String>,
        ) {
            self.record(format!(
                "fif_launch[dir={},filters={}]",
                directory
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".into()),
                filters.unwrap_or_else(|| "<none>".into())
            ));
        }
        fn open_buffer_paths(&self, selector: i32) -> Vec<PathBuf> {
            // Single-view mock: ALL == PRIMARY, SECOND is always
            // empty. Mirrors the production `HostBridge` impl so
            // the dispatcher tests cover the same selector code
            // paths plugins will hit in release.
            match selector {
                ALL_OPEN_FILES | PRIMARY_VIEW => self.open_files_primary.clone(),
                SECOND_VIEW => Vec::new(),
                _ => Vec::new(),
            }
        }
        fn current_doc_index(&self, view: i32) -> i32 {
            match view {
                0 => self.active_tab_primary,
                _ => -1,
            }
        }
        fn buffer_encoding(&self, id: isize) -> i32 {
            self.buffer_encodings
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, e)| *e)
                .unwrap_or(-1)
        }
        fn buffer_format(&self, id: isize) -> i32 {
            self.buffer_formats
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, f)| *f)
                .unwrap_or(-1)
        }
        fn reload_buffer_id(&mut self, id: isize, with_alert: bool) -> bool {
            // Look up the path from the same `buffer_paths` map the
            // production HostBridge uses, so the dispatcher's
            // unknown-id branch is exercised the same way.
            let Some(path) = self
                .buffer_paths
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, p)| p.clone())
            else {
                return false;
            };
            self.record(format!(
                "reload_id[{id}]={} alert={with_alert}",
                path.display()
            ));
            true
        }
        fn set_buffer_encoding(&mut self, id: isize, unimode: i32) -> bool {
            // Match the production HostBridge's "unknown id → false"
            // contract: only buffers we know about (via the
            // `buffer_paths` map, the canonical "is this id real?"
            // signal in the mock) accept a set. UniMode validation
            // mirrors the production mapping below.
            if !self.buffer_paths.iter().any(|(i, _)| *i == id) {
                return false;
            }
            // Reject unknown UniMode and UNI_7BIT (no exact Encoding).
            if !matches!(
                unimode,
                UNI_8BIT
                    | UNI_UTF8
                    | UNI_UTF16BE
                    | UNI_UTF16LE
                    | UNI_COOKIE
                    | UNI_UTF16BE_NO_BOM
                    | UNI_UTF16LE_NO_BOM
            ) {
                return false;
            }
            self.record(format!("set_encoding[{id}]={unimode}"));
            true
        }
        fn set_buffer_format(&mut self, id: isize, eoltype: i32) -> bool {
            if !self.buffer_paths.iter().any(|(i, _)| *i == id) {
                return false;
            }
            if !matches!(eoltype, WIN_FORMAT | MAC_FORMAT | UNIX_FORMAT) {
                return false;
            }
            self.record(format!("set_format[{id}]={eoltype}"));
            true
        }
        fn encode_sci(&mut self, view: i32) -> i32 {
            // Single-view mock: only view 0 has an active buffer
            // (when one is configured). Mirrors the production
            // single-view-through-Phase-4 contract.
            if view != 0 || self.current_buffer == 0 {
                return -1;
            }
            self.record(format!("encode_sci[view={view}]"));
            UNI_COOKIE
        }
        fn decode_sci(&mut self, view: i32) -> i32 {
            if view != 0 || self.current_buffer == 0 {
                return -1;
            }
            self.record(format!("decode_sci[view={view}]"));
            UNI_8BIT
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
    fn fif_launch_decodes_dir_and_filters() {
        let mut s = MockServices::default();
        let dir = make_wide(r"C:\src");
        let filters = make_wide("*.rs *.toml");
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_LAUNCHFINDINFILESDLG,
                dir.as_ptr() as usize,
                filters.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(
            s.calls(),
            vec![r"fif_launch[dir=C:\src,filters=*.rs *.toml]"]
        );
    }

    #[test]
    fn fif_launch_null_args_passes_none() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_LAUNCHFINDINFILESDLG, 0, 0) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["fif_launch[dir=<none>,filters=<none>]"]);
    }

    #[test]
    fn fif_launch_partial_prefill_dir_only() {
        let mut s = MockServices::default();
        let dir = make_wide(r"C:\proj");
        let r =
            unsafe { dispatch_nppm(&mut s, NPPM_LAUNCHFINDINFILESDLG, dir.as_ptr() as usize, 0) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec![r"fif_launch[dir=C:\proj,filters=<none>]"]);
    }

    #[test]
    fn fif_launch_bad_surrogate_dir_falls_back_to_none() {
        // Lone high surrogate followed by NUL: not a valid UTF-16
        // sequence. `wide_ptr_to_string` rejects this with an empty
        // string rather than substituting U+FFFD; the dispatcher
        // arm folds the empty result to `None` so a bad surrogate
        // in `wparam` doesn't trash a good `lparam` prefill (and
        // vice versa).
        let bad: [u16; 2] = [0xD800, 0x0000];
        let filters = make_wide("*.txt");
        let mut s = MockServices::default();
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_LAUNCHFINDINFILESDLG,
                bad.as_ptr() as usize,
                filters.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["fif_launch[dir=<none>,filters=*.txt]"]);
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
        assert_eq!(NPPM_SETBUFFERLANGTYPE, NPPMSG + 65);
        assert_eq!(NPPM_DOOPEN, NPPMSG + 77);
        assert_eq!(NPPM_GETLANGUAGENAME, NPPMSG + 83);
        assert_eq!(NPPM_GETLANGUAGEDESC, NPPMSG + 84);
        assert_eq!(NPPM_GETPLUGINSCONFIGDIR, NPPMSG + 46);
        assert_eq!(NPPM_GETNPPVERSION, NPPMSG + 50);
        assert_eq!(NPPN_FIRST, 1000);
    }

    #[test]
    fn set_buffer_lang_type_dispatches() {
        let mut s = MockServices::default();
        // wparam = buffer id, lparam = LangType id (3 = L_CPP).
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERLANGTYPE, 7, 3) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["set_lang[7]=3"]);
    }

    #[test]
    fn get_language_name_probe_returns_length_with_nul() {
        // wparam = LangType (81 = L_RUST in our compat header), lparam = 0
        // → probe. Expect "Rust".len() + 1 (4 + 1 NUL = 5 TCHARs).
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETLANGUAGENAME, 81, 0) };
        assert_eq!(r, Some(5));
    }

    #[test]
    fn get_language_name_unknown_lang_returns_zero() {
        // Unknown LangType id — host has no name for it. Plugins
        // that read the probe see 0 and back off rather than
        // allocating a one-NUL buffer that would silently match an
        // empty real name.
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETLANGUAGENAME, 9999, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn get_language_name_writes_wide_string() {
        let mut s = MockServices::default();
        let mut buf = [0u16; 16];
        let r =
            unsafe { dispatch_nppm(&mut s, NPPM_GETLANGUAGENAME, 81, buf.as_mut_ptr() as isize) };
        // Wrote 5 TCHARs ("Rust\0").
        assert_eq!(r, Some(5));
        let written: Vec<u16> = buf.iter().take_while(|&&c| c != 0).copied().collect();
        assert_eq!(String::from_utf16(&written).unwrap(), "Rust");
    }

    #[test]
    fn get_language_desc_returns_long_form() {
        // L_CPP description is "C++ source file" (15 chars + NUL = 16).
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETLANGUAGEDESC, 3, 0) };
        assert_eq!(r, Some(16));
    }

    #[test]
    fn nb_open_files_counts_per_selector() {
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")],
            ..Default::default()
        };
        let all = unsafe { dispatch_nppm(&mut s, NPPM_GETNBOPENFILES, ALL_OPEN_FILES as usize, 0) };
        let prim = unsafe { dispatch_nppm(&mut s, NPPM_GETNBOPENFILES, PRIMARY_VIEW as usize, 0) };
        let sec = unsafe { dispatch_nppm(&mut s, NPPM_GETNBOPENFILES, SECOND_VIEW as usize, 0) };
        assert_eq!(all, Some(2));
        assert_eq!(prim, Some(2));
        assert_eq!(sec, Some(0));
    }

    #[test]
    fn open_filenames_writes_each_path_into_caller_slot() {
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("D:/a.txt"), PathBuf::from("D:/b.txt")],
            ..Default::default()
        };
        // Plugin allocates 2 buffers of MAX_PATH_TCHARS each, plus a
        // pointer array of length 2.
        let mut slot_a = vec![0u16; MAX_PATH_TCHARS];
        let mut slot_b = vec![0u16; MAX_PATH_TCHARS];
        let arr: [*mut u16; 2] = [slot_a.as_mut_ptr(), slot_b.as_mut_ptr()];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETOPENFILENAMES,
                arr.as_ptr() as usize,
                arr.len() as isize,
            )
        };
        assert_eq!(r, Some(2));
        let take_until_nul = |buf: &[u16]| {
            let n = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n])
        };
        assert_eq!(take_until_nul(&slot_a), "D:/a.txt");
        assert_eq!(take_until_nul(&slot_b), "D:/b.txt");
    }

    #[test]
    fn open_filenames_truncates_at_caller_capacity() {
        // Three open files but only two slots → write the first two
        // and report 2 so the plugin can detect under-allocation.
        let mut s = MockServices {
            open_files_primary: vec![
                PathBuf::from("/x.txt"),
                PathBuf::from("/y.txt"),
                PathBuf::from("/z.txt"),
            ],
            ..Default::default()
        };
        let mut slot_a = vec![0u16; MAX_PATH_TCHARS];
        let mut slot_b = vec![0u16; MAX_PATH_TCHARS];
        let arr: [*mut u16; 2] = [slot_a.as_mut_ptr(), slot_b.as_mut_ptr()];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETOPENFILENAMES,
                arr.as_ptr() as usize,
                arr.len() as isize,
            )
        };
        assert_eq!(r, Some(2));
    }

    #[test]
    fn open_filenames_null_array_returns_count_for_probe() {
        // wparam == 0 is the probe form: "how many files would you
        // write?", same shape as NPPM_GETFULLPATHFROMBUFFERID's probe.
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETOPENFILENAMES, 0, 0) };
        assert_eq!(r, Some(2));
    }

    #[test]
    fn open_filenames_primary_alias_uses_primary_view() {
        // The -PRIMARY message is selector-fixed: even if the
        // wparam/lparam look like `ALL` in the plain selector form,
        // the dispatcher uses PRIMARY_VIEW for the lookup.
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/p.txt")],
            ..Default::default()
        };
        let mut slot = vec![0u16; MAX_PATH_TCHARS];
        let arr: [*mut u16; 1] = [slot.as_mut_ptr()];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETOPENFILENAMESPRIMARY,
                arr.as_ptr() as usize,
                1,
            )
        };
        assert_eq!(r, Some(1));
    }

    #[test]
    fn open_filenames_second_alias_returns_zero() {
        // Secondary view doesn't exist in single-view Code++; the
        // alias must produce `0` regardless of how many primary files
        // are open, so plugins gating on "is split-view active?" by
        // way of "does the secondary view have files?" get the right
        // answer (no).
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/p.txt")],
            ..Default::default()
        };
        let mut slot = vec![0u16; MAX_PATH_TCHARS];
        let arr: [*mut u16; 1] = [slot.as_mut_ptr()];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETOPENFILENAMESSECOND,
                arr.as_ptr() as usize,
                1,
            )
        };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn open_filenames_primary_probe_returns_view_count() {
        // The probe form (wparam = NULL) must respect the message's
        // selector. PRIMARY → count of primary view's files.
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETOPENFILENAMESPRIMARY, 0, 0) };
        assert_eq!(r, Some(2));
    }

    #[test]
    fn open_filenames_second_probe_returns_zero() {
        // SECOND probe is always 0 in single-view Code++. Without
        // this test a regression in the probe arm could silently
        // return the primary count for SECOND probes, which would
        // mislead plugins gating on split-view presence.
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/a.txt")],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETOPENFILENAMESSECOND, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn open_filenames_skips_null_slot_and_undercounts() {
        // Plugin programming bug: array slot pointer is NULL. Host
        // must not crash, must skip the null slot, and the return
        // value must reflect the actual write count — NOT the
        // attempted count — so the plugin can detect the gap.
        let mut s = MockServices {
            open_files_primary: vec![PathBuf::from("/a.txt"), PathBuf::from("/b.txt")],
            ..Default::default()
        };
        let mut slot_b = vec![0u16; MAX_PATH_TCHARS];
        // First slot pointer is NULL; second is real.
        let arr: [*mut u16; 2] = [core::ptr::null_mut(), slot_b.as_mut_ptr()];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETOPENFILENAMES,
                arr.as_ptr() as usize,
                arr.len() as isize,
            )
        };
        // 2 files attempted, 1 written (the null slot was skipped).
        assert_eq!(r, Some(1));
        // The second slot did receive its path.
        let n = slot_b.iter().position(|&u| u == 0).unwrap_or(slot_b.len());
        assert_eq!(String::from_utf16_lossy(&slot_b[..n]), "/b.txt");
    }

    #[test]
    fn current_doc_index_returns_active_tab_in_primary() {
        let mut s = MockServices {
            active_tab_primary: 2,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETCURRENTDOCINDEX, 0, 0) };
        assert_eq!(r, Some(2));
    }

    #[test]
    fn current_doc_index_for_secondary_returns_minus_one() {
        let mut s = MockServices {
            active_tab_primary: 2,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETCURRENTDOCINDEX, 1, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn current_doc_index_when_no_tabs_returns_minus_one() {
        // Default: active_tab_primary = 0 in MockServices::default(),
        // but with no open files. We deliberately seed -1 here to
        // cover the "no active tab" branch.
        let mut s = MockServices {
            active_tab_primary: -1,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETCURRENTDOCINDEX, 0, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn get_buffer_encoding_returns_unimode() {
        let mut s = MockServices {
            buffer_encodings: vec![(7, UNI_UTF8), (8, UNI_8BIT), (9, UNI_COOKIE)],
            ..Default::default()
        };
        // Cover all three populated ids, including the UNI_8BIT
        // path — this is the value that `Encoding::Other(_)`
        // collapses to in the production HostBridge mapping, and
        // should be observable through the dispatcher unchanged.
        for (id, expected) in [(7, UNI_UTF8), (8, UNI_8BIT), (9, UNI_COOKIE)] {
            let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERENCODING, id, 0) };
            assert_eq!(r, Some(expected as isize), "id={id}");
        }
    }

    #[test]
    fn get_buffer_encoding_unknown_id_returns_minus_one() {
        // Distinct from UNI_8BIT (0) so plugins can tell "no such
        // buffer" from "valid 8-bit buffer".
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERENCODING, 999, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn get_buffer_format_returns_eol_type() {
        let mut s = MockServices {
            buffer_formats: vec![(7, WIN_FORMAT), (8, UNIX_FORMAT), (9, MAC_FORMAT)],
            ..Default::default()
        };
        for (id, expected) in [(7, WIN_FORMAT), (8, UNIX_FORMAT), (9, MAC_FORMAT)] {
            let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERFORMAT, id, 0) };
            assert_eq!(r, Some(expected as isize));
        }
    }

    #[test]
    fn get_buffer_format_unknown_id_returns_minus_one() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERFORMAT, 999, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn reload_buffer_id_known_id_returns_one_and_records_call() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/notes.txt"))],
            ..Default::default()
        };
        // with_alert = 0 (FALSE) — silent reload.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_RELOADBUFFERID, 7, 0) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["reload_id[7]=D:/notes.txt alert=false"]);
    }

    #[test]
    fn reload_buffer_id_with_alert_records_alert_true() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/notes.txt"))],
            ..Default::default()
        };
        // with_alert = 1 (TRUE) — caller wants the "modified
        // externally" prompt. Phase 4 silently reloads either
        // way; this test pins the alert flag is forwarded
        // through the dispatcher boundary so the eventual
        // dialog-routing wiring observes it.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_RELOADBUFFERID, 7, 1) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["reload_id[7]=D:/notes.txt alert=true"]);
    }

    #[test]
    fn reload_buffer_id_unknown_id_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_RELOADBUFFERID, 999, 0) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn set_buffer_encoding_dispatches_unimode_to_services() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/x.txt"))],
            ..Default::default()
        };
        for unimode in [UNI_8BIT, UNI_UTF8, UNI_UTF16LE, UNI_COOKIE] {
            let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERENCODING, 7, unimode as isize) };
            assert_eq!(r, Some(1), "unimode={unimode}");
        }
    }

    #[test]
    fn set_buffer_encoding_rejects_unknown_unimode() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/x.txt"))],
            ..Default::default()
        };
        // 99 is outside the UniMode range; the mock and the
        // production mapping both reject it.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERENCODING, 7, 99) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn set_buffer_encoding_rejects_uni_7bit() {
        // UNI_7BIT has no exact `Encoding` variant — pure ASCII is
        // reported as `UNI_COOKIE` on the way out, and a plugin
        // setting it would imply "save as 7-bit ASCII" which Code++
        // does not model. Reject explicitly so a plugin sees 0
        // rather than a silent fallback.
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/x.txt"))],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERENCODING, 7, UNI_7BIT as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn set_buffer_encoding_unknown_id_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERENCODING, 999, UNI_UTF8 as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn set_buffer_format_dispatches_eoltype_to_services() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/x.txt"))],
            ..Default::default()
        };
        for eol in [WIN_FORMAT, MAC_FORMAT, UNIX_FORMAT] {
            let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERFORMAT, 7, eol as isize) };
            assert_eq!(r, Some(1), "eoltype={eol}");
        }
    }

    #[test]
    fn set_buffer_format_rejects_unknown_eoltype() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/x.txt"))],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERFORMAT, 7, 99) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn set_buffer_format_unknown_id_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETBUFFERFORMAT, 999, WIN_FORMAT as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn encode_sci_primary_view_returns_uni_cookie() {
        let mut s = MockServices {
            current_buffer: 7,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ENCODESCI, 0, 0) };
        assert_eq!(r, Some(UNI_COOKIE as isize));
        assert_eq!(s.calls(), vec!["encode_sci[view=0]"]);
    }

    #[test]
    fn encode_sci_secondary_view_returns_minus_one() {
        // View 1 is the secondary view (split-view), which has no
        // active buffer in single-view Code++. Plugins calling
        // ENCODESCI on it should observe the same "view has no
        // buffer" return value N++ produces (-1).
        let mut s = MockServices {
            current_buffer: 7,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ENCODESCI, 1, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn encode_sci_no_active_buffer_returns_minus_one() {
        // Empty session — no tabs, no active buffer. View 0
        // exists but has nothing to encode.
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ENCODESCI, 0, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn decode_sci_primary_view_returns_uni_8bit() {
        let mut s = MockServices {
            current_buffer: 7,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DECODESCI, 0, 0) };
        assert_eq!(r, Some(UNI_8BIT as isize));
        assert_eq!(s.calls(), vec!["decode_sci[view=0]"]);
    }

    #[test]
    fn decode_sci_secondary_view_returns_minus_one() {
        let mut s = MockServices {
            current_buffer: 7,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DECODESCI, 1, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn decode_sci_no_active_buffer_returns_minus_one() {
        // Symmetric to `encode_sci_no_active_buffer_returns_minus_one`:
        // primary view with no current buffer (empty session)
        // returns -1 from `decode_sci`.
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DECODESCI, 0, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn encode_sci_out_of_range_view_returns_minus_one() {
        // Pre-cast guard: a wparam outside [0, 1] is rejected
        // before the `usize -> i32` truncation. Without the guard,
        // 0x1_0000_0000 would truncate to 0 and be accepted as
        // "primary view".
        let mut s = MockServices {
            current_buffer: 7,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ENCODESCI, 2, 0) };
        assert_eq!(r, Some(-1));
        // Mock should not have been called — the dispatcher arm
        // short-circuited before reaching it.
        assert!(s.calls().is_empty());
    }
}
