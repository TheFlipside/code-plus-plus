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

/// Notepad++ split a handful of "host-state-as-environment" queries
/// (the application's own directory, the running executable's full
/// path, …) into a separate `RUNCOMMAND_USER` family at
/// `WM_USER + 3000` rather than tucking them inside the main NPPM
/// range. Mirroring the same base value is the only way plugins
/// compiled against the upstream header hit the right wParam → host
/// route.
pub const RUNCOMMAND_USER: u32 = WM_USER + 3000;

/// Width of the RUNCOMMAND_USER range the dispatcher claims.
/// Upstream tops out at +49 today; +100 gives parallel headroom to
/// `NPPMSG_RANGE`. Same wnd_proc-pre-filter / dispatcher-internal
/// constraint applies — keep the two range checks in sync.
pub const RUNCOMMAND_RANGE: u32 = 100;

// --- v1 NPPM_* set ---------------------------------------------------

pub const NPPM_GETCURRENTSCINTILLA: u32 = NPPMSG + 4;
pub const NPPM_GETCURRENTLANGTYPE: u32 = NPPMSG + 5;
pub const NPPM_SETCURRENTLANGTYPE: u32 = NPPMSG + 6;
pub const NPPM_GETNBOPENFILES: u32 = NPPMSG + 7;
pub const NPPM_GETOPENFILENAMES: u32 = NPPMSG + 8;
pub const NPPM_GETNBSESSIONFILES: u32 = NPPMSG + 13;
pub const NPPM_GETSESSIONFILES: u32 = NPPMSG + 14;
pub const NPPM_SAVESESSION: u32 = NPPMSG + 15;
pub const NPPM_SAVECURRENTSESSION: u32 = NPPMSG + 16;
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
pub const NPPM_LOADSESSION: u32 = NPPMSG + 34;
pub const NPPM_RELOADFILE: u32 = NPPMSG + 36;
pub const NPPM_SWITCHTOFILE: u32 = NPPMSG + 37;
pub const NPPM_SAVECURRENTFILE: u32 = NPPMSG + 38;
pub const NPPM_SAVEALLFILES: u32 = NPPMSG + 39;
pub const NPPM_SETMENUITEMCHECK: u32 = NPPMSG + 40;
pub const NPPM_GETWINDOWSVERSION: u32 = NPPMSG + 42;
pub const NPPM_MAKECURRENTBUFFERDIRTY: u32 = NPPMSG + 44;
pub const NPPM_GETPLUGINSCONFIGDIR: u32 = NPPMSG + 46;
pub const NPPM_MSGTOPLUGIN: u32 = NPPMSG + 47;
pub const NPPM_MENUCOMMAND: u32 = NPPMSG + 48;
pub const NPPM_GETNPPVERSION: u32 = NPPMSG + 50;
pub const NPPM_HIDETABBAR: u32 = NPPMSG + 51;
pub const NPPM_ISTABBARHIDDEN: u32 = NPPMSG + 52;
pub const NPPM_GETPOSFROMBUFFERID: u32 = NPPMSG + 57;
pub const NPPM_GETFULLPATHFROMBUFFERID: u32 = NPPMSG + 58;
pub const NPPM_GETBUFFERIDFROMPOS: u32 = NPPMSG + 59;
pub const NPPM_GETCURRENTBUFFERID: u32 = NPPMSG + 60;
pub const NPPM_RELOADBUFFERID: u32 = NPPMSG + 61;
pub const NPPM_GETBUFFERLANGTYPE: u32 = NPPMSG + 64;
pub const NPPM_SETBUFFERLANGTYPE: u32 = NPPMSG + 65;
pub const NPPM_GETBUFFERENCODING: u32 = NPPMSG + 66;
pub const NPPM_SETBUFFERENCODING: u32 = NPPMSG + 67;
pub const NPPM_GETBUFFERFORMAT: u32 = NPPMSG + 68;
pub const NPPM_SETBUFFERFORMAT: u32 = NPPMSG + 69;
pub const NPPM_HIDETOOLBAR: u32 = NPPMSG + 70;
pub const NPPM_ISTOOLBARHIDDEN: u32 = NPPMSG + 71;
pub const NPPM_HIDEMENU: u32 = NPPMSG + 72;
pub const NPPM_ISMENUHIDDEN: u32 = NPPMSG + 73;
pub const NPPM_HIDESTATUSBAR: u32 = NPPMSG + 74;
pub const NPPM_ISSTATUSBARHIDDEN: u32 = NPPMSG + 75;
pub const NPPM_DOOPEN: u32 = NPPMSG + 77;
pub const NPPM_SAVECURRENTFILEAS: u32 = NPPMSG + 78;
pub const NPPM_ALLOCATESUPPORTED: u32 = NPPMSG + 80;
pub const NPPM_GETLANGUAGENAME: u32 = NPPMSG + 83;
pub const NPPM_GETLANGUAGEDESC: u32 = NPPMSG + 84;
pub const NPPM_SHOWDOCSWITCHER: u32 = NPPMSG + 85;
pub const NPPM_ISDOCSWITCHERSHOWN: u32 = NPPMSG + 86;
pub const NPPM_GETAPPDATAPLUGINSALLOWED: u32 = NPPMSG + 87;
pub const NPPM_GETCURRENTVIEW: u32 = NPPMSG + 88;
pub const NPPM_DOCSWITCHERDISABLECOLUMN: u32 = NPPMSG + 89;
pub const NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR: u32 = NPPMSG + 90;
pub const NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR: u32 = NPPMSG + 91;
pub const NPPM_SETSMOOTHFONT: u32 = NPPMSG + 92;
pub const NPPM_SETEDITORBORDEREDGE: u32 = NPPMSG + 93;
pub const NPPM_SAVEFILE: u32 = NPPMSG + 94;
pub const NPPM_DISABLEAUTOUPDATE: u32 = NPPMSG + 95;
pub const NPPM_GETPLUGINHOMEPATH: u32 = NPPMSG + 97;
pub const NPPM_GETSETTINGSCLOUDPATH: u32 = NPPMSG + 98;
pub const NPPM_SETLINENUMBERWIDTHMODE: u32 = NPPMSG + 99;
pub const NPPM_GETLINENUMBERWIDTHMODE: u32 = NPPMSG + 100;
pub const NPPM_GETBOOKMARKID: u32 = NPPMSG + 101;
pub const NPPM_GETZOOMLEVEL: u32 = NPPMSG + 102;

/// `NPPM_SETLINENUMBERWIDTHMODE` / `GETLINENUMBERWIDTHMODE`
/// values. Match the upstream `LineNumberWidthMode` enum so
/// plugins compiled against either header use the same wire
/// codes.
///
/// `LINENUMWIDTH_DYNAMIC` (0) — the margin auto-grows as the line
/// count crosses each decimal-digit boundary. The modern default
/// and Code++'s only currently-implemented mode.
pub const LINENUMWIDTH_DYNAMIC: i32 = 0;
/// `LINENUMWIDTH_CONSTANT` (1) — the margin stays at a fixed
/// width sized for the current line count. Plugins requesting
/// this in Code++ today get the request recorded but the Win32
/// backend does not yet flip Scintilla's mode (Phase 4 polish).
pub const LINENUMWIDTH_CONSTANT: i32 = 1;

/// `RUNCOMMAND_USER` family — see [`RUNCOMMAND_USER`].
///
/// Returns the host's installation directory (the one containing
/// the running executable) into a plugin-allocated wide buffer.
/// `wparam` carries the buffer capacity in TCHARs; `lparam` the
/// `TCHAR*` OUT pointer. Returns 1 on success, 0 on bad arguments
/// or unresolvable executable path.
pub const NPPM_GETNPPDIRECTORY: u32 = RUNCOMMAND_USER + 23;
/// Returns the full path of the running executable (the
/// installation directory plus `code++.exe` filename) into a
/// plugin-allocated wide buffer. Same wparam/lparam contract as
/// `NPPM_GETNPPDIRECTORY`.
pub const NPPM_GETNPPFULLFILEPATH: u32 = RUNCOMMAND_USER + 42;

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
pub const NPPN_FILEBEFOREOPEN: u32 = NPPN_FIRST + 6;
pub const NPPN_FILEBEFORESAVE: u32 = NPPN_FIRST + 7;
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

/// Cap on the file count a single `NPPM_SAVESESSION` call accepts.
/// A malformed plugin claiming `nb_file = i32::MAX` would otherwise
/// force the host into a multi-gigabyte allocation; this cap turns
/// that into a clean rejection. The number itself matches the
/// editor's `MAX_OPEN_TABS` order of magnitude — a session with
/// more than 1024 entries is well outside any reasonable plugin's
/// scope and the plugin contract has no way to signal partial
/// success on truncation.
pub const MAX_SESSION_FILES: usize = 1024;

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
    /// Plugins about to be told a file is being opened. Fired before
    /// `Shell` even attempts disk I/O — useful for plugins wanting to
    /// veto / pre-process based on path. Code++ does not honour a
    /// veto (deferred-queue model means the open is in flight by
    /// the time the plugin runs); the notification is informational
    /// only. Carries a buffer id of 0 because the buffer hasn't
    /// been allocated yet — N++ uses the same convention.
    FileBeforeOpen,
    FileOpened {
        buffer_id: isize,
    },
    FileClosed {
        buffer_id: isize,
    },
    /// Plugins about to be told a file is being saved. Fired right
    /// before the host writes the buffer text to disk — paired with
    /// `FileSaved` for the post-write notification.
    FileBeforeSave {
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
            Self::FileBeforeOpen => NPPN_FILEBEFOREOPEN,
            Self::FileOpened { .. } => NPPN_FILEOPENED,
            Self::FileClosed { .. } => NPPN_FILECLOSED,
            Self::FileBeforeSave { .. } => NPPN_FILEBEFORESAVE,
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
            | Self::FileBeforeSave { buffer_id }
            | Self::FileSaved { buffer_id }
            | Self::BufferActivated { buffer_id }
            | Self::LangChanged { buffer_id } => *buffer_id,
            Self::Ready | Self::TbModification | Self::FileBeforeOpen | Self::Shutdown => 0,
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

    /// `true` if the tab strip is currently hidden, `false` if
    /// visible. Drives [`NPPM_ISTABBARHIDDEN`].
    fn is_tabbar_hidden(&self) -> bool;

    /// Toggle the tab strip's visibility. `hidden == true` hides
    /// it, `false` shows it. Returns the *previous* hidden state
    /// — that's the contract `NPPM_HIDETABBAR` plugins read to
    /// know whether they actually changed anything.
    ///
    /// Same-value calls are still routed to the UI so the editor
    /// view layout refreshes idempotently (a UI-only invariant
    /// guard). The previous-state return is the sole signal to
    /// the plugin.
    fn set_tabbar_hidden(&mut self, hidden: bool) -> bool;

    /// `true` if the toolbar is currently hidden, `false` if
    /// visible. Drives [`NPPM_ISTOOLBARHIDDEN`].
    fn is_toolbar_hidden(&self) -> bool;
    /// Toggle the toolbar's visibility. Same return contract as
    /// [`Self::set_tabbar_hidden`] — returns the *previous*
    /// hidden state. Drives [`NPPM_HIDETOOLBAR`].
    fn set_toolbar_hidden(&mut self, hidden: bool) -> bool;

    /// `true` if the main menu bar is currently hidden, `false`
    /// if visible. Drives [`NPPM_ISMENUHIDDEN`].
    fn is_menu_hidden(&self) -> bool;
    /// Toggle the main menu bar's visibility. `SetMenu(NULL)` to
    /// hide; `SetMenu(main_menu)` + `DrawMenuBar` to show. Same
    /// return contract — previous hidden state. Drives
    /// [`NPPM_HIDEMENU`].
    fn set_menu_hidden(&mut self, hidden: bool) -> bool;

    /// `true` if the status bar is currently hidden, `false` if
    /// visible. Drives [`NPPM_ISSTATUSBARHIDDEN`].
    fn is_statusbar_hidden(&self) -> bool;
    /// Toggle the status bar's visibility. Same return contract
    /// as the other chrome toggles. Drives
    /// [`NPPM_HIDESTATUSBAR`].
    fn set_statusbar_hidden(&mut self, hidden: bool) -> bool;

    /// Save every dirty titled tab to disk in one batch. Drives
    /// [`NPPM_SAVEALLFILES`]; matches `Shell::save_all`'s contract:
    /// untitled tabs are skipped (no on-disk path to save to);
    /// per-tab errors are logged but do not abort the batch. The
    /// dispatcher returns 1 unconditionally — N++'s ABI documents
    /// the return as "always TRUE" because the per-file failure
    /// mode is reported via the live error UI, not the message
    /// return.
    fn save_all_files(&mut self);

    /// Directory containing the host executable — drives
    /// [`NPPM_GETNPPDIRECTORY`]. Returns `None` when the running
    /// binary's path can't be resolved (sandboxed runners,
    /// `/proc/self/exe` denied, …); the dispatcher then reports
    /// failure to the plugin rather than handing back a guessed
    /// path.
    fn program_dir(&self) -> Option<PathBuf>;

    /// Full path of the running executable — drives
    /// [`NPPM_GETNPPFULLFILEPATH`]. Same `None` semantics as
    /// [`Self::program_dir`].
    fn program_path(&self) -> Option<PathBuf>;

    /// Notepad++'s `winVer` enum value for the current OS. Drives
    /// [`NPPM_GETWINDOWSVERSION`]. The dispatcher trusts the impl to
    /// stay within the canonical N++ enum (… `WV_WIN10 = 16`,
    /// `WV_WIN11 = 17`, …) so plugins gating on `>= WV_WIN10`
    /// behave the same as in N++.
    fn windows_version(&self) -> i32;

    /// Tab-strip position of the buffer with id `id`. Returns
    /// `Some((view, idx))` when the buffer exists, `None`
    /// otherwise. Drives [`NPPM_GETPOSFROMBUFFERID`]; the
    /// dispatcher is responsible for the `(idx | view_bit)`
    /// encoding the N++ ABI documents.
    fn buffer_position(&self, id: isize) -> Option<(i32, i32)>;

    /// Buffer id of the tab at index `pos` in `view`. Returns 0
    /// for an out-of-range index, an unknown view, or — in
    /// single-view Code++ — `view == 1`. Drives
    /// [`NPPM_GETBUFFERIDFROMPOS`].
    fn buffer_id_at(&self, view: i32, pos: i32) -> isize;

    /// Save the active buffer to `path`. `as_copy == true` writes a
    /// copy without re-pointing the active tab (the in-memory
    /// buffer continues to track its original on-disk path);
    /// `as_copy == false` is a rename — the tab moves to the new
    /// path and subsequent saves write there. Drives
    /// [`NPPM_SAVECURRENTFILEAS`]. Returns `true` on a successful
    /// write, `false` on any I/O / encoding failure or when there
    /// is no active buffer.
    fn save_current_as(&mut self, path: PathBuf, as_copy: bool) -> bool;

    /// Open every file listed in the session-XML at `path`, in
    /// order. Untitled tabs and the saved active-tab index inside
    /// the session file are honoured exactly as the file-watcher /
    /// menu-driven session restore would. Drives
    /// [`NPPM_LOADSESSION`]. Returns `true` on a successful parse
    /// (even for an empty session — that's "load nothing"),
    /// `false` on I/O / parse failure.
    fn load_session(&mut self, path: PathBuf) -> bool;

    /// Write the currently-open titled tabs to a session-XML at
    /// `path`. Untitled tabs are excluded — they have no
    /// reproducible on-disk path and the foreign-session protocol
    /// has no slot for them. Drives [`NPPM_SAVECURRENTSESSION`].
    /// Returns `true` on a successful write, `false` on I/O
    /// failure.
    fn save_current_session(&self, path: PathBuf) -> bool;

    /// Write a session-XML at `path` containing the supplied
    /// `files`. Used by [`NPPM_SAVESESSION`], where the plugin
    /// supplies the file list rather than the host's current state.
    /// Returns `true` on success, `false` on I/O failure.
    fn save_session_with_files(&self, path: PathBuf, files: Vec<PathBuf>) -> bool;

    /// Read the session-XML at `path` and return its file list.
    /// `None` when the file can't be read or doesn't parse as
    /// Code++'s session schema. Drives both
    /// [`NPPM_GETNBSESSIONFILES`] (count returned) and
    /// [`NPPM_GETSESSIONFILES`] (paths written into the plugin's
    /// TCHAR** array). Untitled tabs (no on-disk path) are filtered
    /// out — the message contracts both expect file paths only.
    fn read_session_file_paths(&self, path: PathBuf) -> Option<Vec<PathBuf>>;

    /// `true` when the host supports `NPPM_ALLOCATECMDID` /
    /// `NPPM_ALLOCATEMARKER` (the plugin-driven id reservation
    /// messages). Drives [`NPPM_ALLOCATESUPPORTED`] — plugins use
    /// this to gate `if (NPPM_ALLOCATESUPPORTED) { … }`. Code++
    /// returns `false` until those messages land in v3.
    fn alloc_supported(&self) -> bool;

    /// `true` when plugins installed under `%APPDATA%\Code++\plugins`
    /// (per-user, no admin rights) are honoured. Drives
    /// [`NPPM_GETAPPDATAPLUGINSALLOWED`]. Code++ always loads from
    /// the per-user dir, so this is unconditionally `true`.
    fn appdata_plugins_allowed(&self) -> bool;

    /// Active view index (0 = primary, 1 = secondary). Drives
    /// [`NPPM_GETCURRENTVIEW`]. Code++ is single-view through
    /// Phase 4 → always 0.
    fn current_view(&self) -> i32;

    /// Per-user plugins directory (the parent of every plugin
    /// subdirectory). Drives [`NPPM_GETPLUGINHOMEPATH`]. `None`
    /// in sandboxed runners with no resolvable config dir, in
    /// which case the dispatcher reports failure.
    fn plugin_home_dir(&self) -> Option<PathBuf>;

    /// Cloud-sync settings directory if the user has opted in.
    /// Drives [`NPPM_GETSETTINGSCLOUDPATH`]. Code++ does not
    /// implement settings cloud-sync, so this is `None` and the
    /// dispatcher writes an empty wide string.
    fn settings_cloud_dir(&self) -> Option<PathBuf>;

    /// Scintilla marker number reserved for bookmarks. Drives
    /// [`NPPM_GETBOOKMARKID`]. Code++ uses N++'s convention of
    /// marker 24 so plugins that install a bookmark via
    /// `SCI_MARKERADD(line, NPPM_GETBOOKMARKID())` work the same
    /// way they would in N++. Note: Code++'s UI does not yet
    /// surface bookmarks as user-visible markers — that's a Phase
    /// 4 polish item — so the marker is set on the buffer but
    /// won't be styled.
    fn bookmark_marker_id(&self) -> i32;

    /// Active editor's zoom level in points (Scintilla
    /// `SCI_GETZOOM`). Drives [`NPPM_GETZOOMLEVEL`]. Range is
    /// approximately `[-10, 20]` (Scintilla's documented bounds).
    fn editor_zoom_level(&self) -> i32;

    /// Forward an inter-plugin message (`NPPM_MSGTOPLUGIN`) to
    /// the target plugin named `target_name`. The host calls
    /// `target.messageProc(internal_msg, info_ptr, 0)` and
    /// returns the LRESULT. Returns 0 if the target plugin is not
    /// loaded (or the name does not match any known plugin) — the
    /// upstream contract.
    ///
    /// `internal_msg` is the value the source plugin set on
    /// `CommunicationInfo.internal_msg` (`long` upstream → `i32`
    /// in the LLP64 ABI). Kept signed in the trait so the
    /// `i32 → u32` cast that hands the value to the FFI
    /// `messageProc(UINT, WPARAM, LPARAM)` happens at exactly one
    /// site (the impl). A plugin using a negative `long` for the
    /// message code sees the same bit pattern it would in N++ —
    /// both hosts perform the same implicit signed-to-unsigned
    /// cast at the messageProc call.
    ///
    /// `info_ptr` is the verbatim CommunicationInfo pointer the
    /// source plugin supplied; the host does not dereference it
    /// past reading `internal_msg`.
    fn forward_plugin_message(
        &mut self,
        target_name: &str,
        internal_msg: i32,
        info_ptr: usize,
    ) -> isize;

    /// Default foreground colour of the active editor. Drives
    /// [`NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR`]. Returns Win32
    /// `COLORREF` (`0x00BBGGRR`) — Scintilla and Win32 share the
    /// layout, so the value passes through verbatim.
    fn editor_default_fg_color(&self) -> i32;

    /// Default background colour of the active editor. Drives
    /// [`NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR`]. Same `COLORREF`
    /// layout as [`Self::editor_default_fg_color`].
    fn editor_default_bg_color(&self) -> i32;

    /// Toggle Scintilla's font-rendering quality between
    /// LCD-optimised (`smooth == true`, the modern Win32
    /// default) and non-antialiased (`smooth == false`). Drives
    /// [`NPPM_SETSMOOTHFONT`]. Returns the *previous* state — same
    /// "did I change anything?" contract as the chrome toggles.
    fn set_smooth_font(&mut self, smooth: bool) -> bool;

    /// Toggle the `WS_EX_CLIENTEDGE` extended style on the
    /// Scintilla view's HWND. Drives [`NPPM_SETEDITORBORDEREDGE`].
    /// Returns the previous state.
    fn set_editor_border_edge(&mut self, enable: bool) -> bool;

    /// Save the buffer matching `path` to disk. Drives
    /// [`NPPM_SAVEFILE`]. Returns `true` on a successful write,
    /// `false` if the path does not match any open tab or the
    /// write itself failed. **Phase 4 limitation:** only the
    /// active tab can be saved through this path — saving a
    /// background tab needs the doc-pointer-swap dance tracked in
    /// DESIGN.md §7.4 alongside the `SCI_CONVERTEOLS` deferral.
    fn save_file(&mut self, path: PathBuf) -> bool;

    /// `true` when the doc-switcher panel is currently shown.
    /// Code++ does not yet implement a doc-switcher, so this is
    /// permanently `false`. Drives [`NPPM_ISDOCSWITCHERSHOWN`].
    fn is_doc_switcher_shown(&self) -> bool;

    /// Show / hide the doc-switcher panel. Drives
    /// [`NPPM_SHOWDOCSWITCHER`]. Code++ has no doc-switcher
    /// panel; the call is a no-op and the return is always
    /// `false` (the previous state, since the panel is never
    /// shown). The honest "we don't have it" answer matches
    /// `NPPM_ALLOCATESUPPORTED`'s pattern — plugins gating on
    /// `IS*SHOWN` already see the right answer.
    fn set_doc_switcher_shown(&mut self, shown: bool) -> bool;

    /// Disable a column in the doc-switcher's listview. Drives
    /// [`NPPM_DOCSWITCHERDISABLECOLUMN`]. Code++'s no-op
    /// equivalent — there's no listview to mutate.
    fn doc_switcher_disable_column(&mut self, column_idx: i32, disable: bool);

    /// Current line-number margin width mode (`DYNAMIC` or
    /// `CONSTANT`). Drives [`NPPM_GETLINENUMBERWIDTHMODE`].
    /// Code++ uses `LINENUMWIDTH_DYNAMIC` everywhere right now.
    fn line_number_width_mode(&self) -> i32;

    /// Set the line-number margin width mode. Drives
    /// [`NPPM_SETLINENUMBERWIDTHMODE`]. Code++ accepts
    /// `LINENUMWIDTH_DYNAMIC` and records `LINENUMWIDTH_CONSTANT`
    /// without rejecting (UI-side mode flip is a Phase 4 polish
    /// item). Returns `false` for unknown values.
    fn set_line_number_width_mode(&mut self, mode: i32) -> bool;
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
    // Stay inside the two ranges N++ owns: NPPMSG_RANGE for the
    // mainline NPPM_* family (NPPMSG..NPPMSG+200) and the
    // RUNCOMMAND_USER family (WM_USER+3000..+3100) for the handful of
    // host-environment queries — `NPPM_GETNPPDIRECTORY`,
    // `NPPM_GETNPPFULLFILEPATH`. Out-of-range falls back to the
    // default wnd_proc so non-plugin WM_USER+N messages from the
    // host's own UI continue to dispatch normally.
    let in_nppm = (NPPMSG..NPPMSG + NPPMSG_RANGE).contains(&msg);
    let in_runcmd = (RUNCOMMAND_USER..RUNCOMMAND_USER + RUNCOMMAND_RANGE).contains(&msg);
    if !in_nppm && !in_runcmd {
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
            // wparam: view selector (0 = primary, 1 = secondary).
            // lparam: tab position in that view.
            //
            // Range-check `wparam` *before* the `usize -> i32`
            // truncation: a plugin sending `wparam = 0x1_0000_0000`
            // would truncate to 0 and silently be accepted as
            // "primary view". Same shape as the pre-cast guard on
            // `NPPM_ENCODESCI` / `NPPM_DECODESCI`.
            if wparam > 1 {
                return Some(0);
            }
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

        NPPM_SAVEALLFILES => {
            // No args. Saves every dirty titled buffer. Untitled
            // tabs (no on-disk path) are skipped; per-tab errors
            // are logged but don't abort the batch — same contract
            // as `Shell::save_all`. Always reports success: per-
            // file failures surface via the live error UI, not the
            // ABI return.
            services.save_all_files();
            1
        }

        NPPM_GETPOSFROMBUFFERID => {
            // wparam: buffer id. lparam: priority view selector
            // (0 = main, 1 = sub) — currently advisory in single-
            // view Code++; the impl always returns the position in
            // the primary view. Returns the tab index, with the
            // 0x40000000 bit set if the buffer lives in the
            // secondary view (always clear in Phase 4 single-view).
            // Returns -1 if the buffer doesn't exist anywhere.
            let id = wparam as isize;
            match services.buffer_position(id) {
                Some((view, idx)) => {
                    // N++'s ABI: bit 0x40000000 means "secondary
                    // view"; lower bits are the tab index. Plugins
                    // mask the view bit then read the index.
                    let view_bit = if view == 1 { 0x4000_0000_isize } else { 0 };
                    (idx as isize) | view_bit
                }
                None => -1,
            }
        }

        NPPM_GETBUFFERIDFROMPOS => {
            // wparam: tab position. lparam: view selector
            // (0 = main, 1 = sub). Returns the buffer id, or 0
            // for an out-of-range index, an unknown view, or
            // (in single-view Code++) `view == 1`.
            //
            // `wparam as i32` truncates the upper bits; positions
            // are bounded by the open-files count, well below
            // i32::MAX, so no real plugin sends a wparam that
            // would suffer from the truncation.
            let view = lparam as i32;
            let pos = wparam as i32;
            services.buffer_id_at(view, pos)
        }

        NPPM_SETMENUITEMCHECK => {
            services.set_menu_item_check(wparam as i32, lparam != 0);
            1
        }

        NPPM_GETWINDOWSVERSION => {
            // Notepad++'s `winVer` enum maps Windows release codes
            // (… `WV_WIN10 = 16`, `WV_WIN11 = 17`). The shell-side
            // impl probes the running kernel via `RtlGetVersion` and
            // returns the matching enum value; failures fall back to
            // `WV_WIN10` (16) so plugins gating on `>= WV_WIN10`
            // continue to work in environments where the version
            // probe is unavailable.
            services.windows_version() as isize
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

        NPPM_MSGTOPLUGIN => {
            // wparam: TCHAR* target plugin name (the one that
            //         appears in the Plugins menu — the value the
            //         target's `getName()` returned).
            // lparam: pointer to a `CommunicationInfo` struct.
            // Returns: the target plugin's `messageProc` return
            // value, or 0 when the target isn't loaded / name
            // doesn't match — the upstream contract.
            //
            // The host reads only `internal_msg` from the struct
            // (to pick the message number the target's messageProc
            // receives); `src_module_name` and `info` are
            // forwarded verbatim through the wParam pointer slot.
            // Reject the obvious-bug pointers up front: NULL on
            // either arg, and a negative `lparam` (which casts to a
            // high-address kernel-mode pointer on x64 Windows
            // reserves bit 47+ for the kernel). A buggy source
            // plugin sending a negative struct address would
            // otherwise pass the null check, and the target's
            // `messageProc` would deref into kernel space and
            // SEGV. N++ inherits the same crash mode; the explicit
            // guard turns it into a clean reject.
            if wparam == 0 || lparam <= 0 {
                return Some(0);
            }
            // SAFETY: plugin promises `wparam` is a valid wide
            // null-terminated string and `lparam` is a valid
            // CommunicationInfo it owns for the duration of the
            // call. The MAX_PATH_TCHARS bound on
            // `wide_ptr_to_string` keeps a missing terminator
            // from running off into arbitrary memory.
            let target_name = unsafe { wide_ptr_to_string(wparam as *const u16) };
            if target_name.is_empty() {
                return Some(0);
            }
            let info = lparam as *const crate::ffi::CommunicationInfo;
            // SAFETY: `info` is the plugin's struct; we read only
            // the `internal_msg` field, which is the first 4 bytes
            // (`i32` per the upstream `long` ABI). `addr_of!` +
            // `read_unaligned` is the explicit form that handles
            // a packed plugin allocation safely.
            let internal_msg =
                unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*info).internal_msg)) };
            services.forward_plugin_message(&target_name, internal_msg, lparam as usize)
        }

        NPPM_MENUCOMMAND => {
            services.menu_command(lparam as i32);
            1
        }

        NPPM_GETNPPVERSION => CODEPP_PLUGIN_API_VERSION,

        NPPM_HIDETABBAR => {
            // wparam: BOOL — TRUE hides the tab strip, FALSE shows
            // it. Returns the *previous* hidden state per the N++
            // contract, so a plugin can detect "I just changed it"
            // (return != hidden) vs. "it was already in this state"
            // (return == hidden).
            let hidden = wparam != 0;
            services.set_tabbar_hidden(hidden) as isize
        }

        NPPM_ISTABBARHIDDEN => {
            // No args. Returns BOOL — current hidden state.
            services.is_tabbar_hidden() as isize
        }

        NPPM_HIDETOOLBAR => {
            // wparam: BOOL — TRUE hides, FALSE shows. Returns the
            // *previous* hidden state. Same contract shape as
            // `NPPM_HIDETABBAR`.
            services.set_toolbar_hidden(wparam != 0) as isize
        }

        NPPM_ISTOOLBARHIDDEN => services.is_toolbar_hidden() as isize,

        NPPM_HIDEMENU => {
            // wparam: BOOL. Returns previous hidden state. The host
            // implementation flips between `SetMenu(main_hwnd, NULL)`
            // and `SetMenu(main_hwnd, main_menu)` + `DrawMenuBar`.
            services.set_menu_hidden(wparam != 0) as isize
        }

        NPPM_ISMENUHIDDEN => services.is_menu_hidden() as isize,

        NPPM_HIDESTATUSBAR => {
            // wparam: BOOL. Returns previous hidden state.
            services.set_statusbar_hidden(wparam != 0) as isize
        }

        NPPM_ISSTATUSBARHIDDEN => services.is_statusbar_hidden() as isize,

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

        NPPM_SAVECURRENTFILEAS => {
            // wparam: BOOL — TRUE means "save a copy" (the active
            // tab keeps tracking its original path), FALSE means
            // rename the active tab to the new path.
            // lparam: TCHAR* target path.
            //
            // Empty wide / bad surrogates rejected the same way
            // NPPM_DOOPEN rejects them: a substituted U+FFFD on a
            // path-typed payload could route the write to a
            // different file than the plugin intended.
            if lparam == 0 {
                return Some(0);
            }
            let as_copy = wparam != 0;
            // SAFETY: plugin promises lparam is a valid wide path.
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            services.save_current_as(PathBuf::from(decoded), as_copy) as isize
        }

        NPPM_LOADSESSION => {
            // wparam: unused.
            // lparam: TCHAR* path to a session-XML file.
            //
            // The session schema is Code++'s `core::session::Session`
            // (not Notepad++'s session.xml format). A session file
            // written by N++ won't load until cross-tool schema
            // support lands as Phase 5 polish.
            if lparam == 0 {
                return Some(0);
            }
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            services.load_session(PathBuf::from(decoded)) as isize
        }

        NPPM_SAVECURRENTSESSION => {
            // wparam: unused.
            // lparam: TCHAR* destination path. Code++ writes its own
            // session schema; foreign-tool readers (N++) will not
            // pick the file up until cross-tool schema support
            // lands.
            if lparam == 0 {
                return Some(0);
            }
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            services.save_current_session(PathBuf::from(decoded)) as isize
        }

        NPPM_GETNBSESSIONFILES => {
            // wparam: unused.
            // lparam: TCHAR* path to a session-XML file.
            // Returns the number of titled files in the session, or
            // 0 on read / parse failure (or for an empty session).
            // Untitled tabs are not counted — the message contract
            // is "files," and untitled has no on-disk file.
            if lparam == 0 {
                return Some(0);
            }
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            services
                .read_session_file_paths(PathBuf::from(decoded))
                .map(|v| v.len() as isize)
                .unwrap_or(0)
        }

        NPPM_GETSESSIONFILES => {
            // wparam: TCHAR** array of plugin-allocated buffers,
            //         each at least MAX_PATH wide chars.
            // lparam: TCHAR* path to the session-XML file.
            // Returns 1 on success, 0 on bad arguments or parse
            // failure.
            //
            // **ABI gap (parity with N++):** the message contract
            // does NOT carry the slot count the plugin allocated.
            // Plugins are expected to call
            // `NPPM_GETNBSESSIONFILES` first and allocate exactly
            // that many slots. The session file the host reads
            // here can in principle have grown between the two
            // calls (race against an external session writer); if
            // it has, the host writes past the plugin's allocation
            // and corrupts plugin memory. This is an upstream-N++
            // hazard plugins inherit; closing it would require an
            // ABI-extension message that carries the count
            // explicitly (`NPPM_GETSESSIONFILES_BOUNDED` or the
            // like). Documented as a known limitation rather than
            // worked around.
            if lparam == 0 || wparam == 0 {
                return Some(0);
            }
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            let Some(mut paths) = services.read_session_file_paths(PathBuf::from(decoded)) else {
                return Some(0);
            };
            // Defence-in-depth: cap the number of slots we write
            // at `MAX_SESSION_FILES`. The plugin sized its array
            // from a prior `NPPM_GETNBSESSIONFILES` call, so the
            // session file's count at that time was already
            // bounded by whatever the host reported. The race
            // window between the count and the write call could
            // see the file change to one with more entries; the
            // cap turns "unbounded plugin-heap overwrite" into
            // "bounded write of at most 1024 slots." Plugins
            // allocating 1024 slots based on the count call would
            // already be carrying a host-side commitment to that
            // size; truncating beyond that is the host's worst-
            // case bound.
            if paths.len() > MAX_SESSION_FILES {
                tracing::warn!(
                    nb = paths.len(),
                    cap = MAX_SESSION_FILES,
                    "NPPM_GETSESSIONFILES: truncating to cap to bound plugin write",
                );
                paths.truncate(MAX_SESSION_FILES);
            }
            // SAFETY: plugin promises wparam is a TCHAR** of at
            // least `paths.len()` slots, each pointing at a buffer
            // of at least MAX_PATH wide chars. The host writes one
            // wide string per slot, capped at MAX_PATH_TCHARS.
            unsafe {
                write_path_array(wparam as *mut *mut u16, &paths);
            }
            1
        }

        NPPM_SAVESESSION => {
            // wparam: unused.
            // lparam: pointer to a `SessionInfo` struct (see
            //         `crate::ffi::SessionInfo`). Plugins fill in
            //         the path + file list before sending; the host
            //         writes that list into a session-XML at the
            //         requested path.
            // Returns: lParam (the session path pointer) on
            // success, 0 on bad arguments or write failure — that's
            // the upstream contract (`saveSession()` returns the
            // input pointer so plugins can chain the call).
            if lparam == 0 {
                return Some(0);
            }
            let info = lparam as *const crate::ffi::SessionInfo;
            // SAFETY: plugin promises `info` points to a valid
            // SessionInfo it owns for the duration of the call.
            // Each pointer field is read via `addr_of!` +
            // `read_unaligned` so we never form an aligned
            // intermediate reference to a potentially-unaligned
            // field — the explicit form removes any ambiguity
            // about an LLVM optimisation pass interpreting
            // `&(*info).field` as a guarantee of natural
            // alignment.
            let (session_path_ptr, nb_file, files_ptr) = unsafe {
                (
                    core::ptr::read_unaligned(core::ptr::addr_of!((*info).session_file_path_name)),
                    core::ptr::read_unaligned(core::ptr::addr_of!((*info).nb_file)),
                    core::ptr::read_unaligned(core::ptr::addr_of!((*info).files)),
                )
            };
            if session_path_ptr.is_null() || nb_file < 0 {
                return Some(0);
            }
            let path_str = unsafe { wide_ptr_to_string(session_path_ptr) };
            if path_str.is_empty() {
                return Some(0);
            }
            let count = nb_file as usize;
            // Bound the per-call allocation. A malformed plugin
            // claiming `nb_file = i32::MAX` would otherwise force
            // the host to attempt a multi-gigabyte alloc.
            // MAX_OPEN_TABS-class bound is appropriate — the file
            // list a session can hold is the same as the editor's
            // open-tab cap.
            if count > MAX_SESSION_FILES {
                tracing::warn!(
                    nb_file = nb_file,
                    cap = MAX_SESSION_FILES,
                    "NPPM_SAVESESSION: rejecting unreasonable file count",
                );
                return Some(0);
            }
            // Reject `nb_file > 0 && files == NULL` before entering
            // the loop. `files_ptr.add(i)` on a null pointer is UB
            // even with `read_unaligned`; the prior `nb_file < 0`
            // guard does not cover this (a plugin can claim a
            // positive count and supply a null array). For
            // `count == 0` the loop body never runs, so a null
            // pointer is harmless — accept it as a degenerate
            // "save an empty session" call.
            if count > 0 && files_ptr.is_null() {
                tracing::warn!(
                    nb_file = nb_file,
                    "NPPM_SAVESESSION: nb_file > 0 but files pointer is NULL",
                );
                return Some(0);
            }
            let mut files: Vec<PathBuf> = Vec::with_capacity(count);
            // SAFETY: plugin promises `files_ptr` points to an
            // array of `count` wide-string pointers. Each entry is
            // read with `read_unaligned`; null entries are skipped
            // (same defensive shape as `NPPM_GETOPENFILENAMES`'s
            // host-side iteration).
            for i in 0..count {
                let p = unsafe { core::ptr::read_unaligned(files_ptr.add(i)) };
                if p.is_null() {
                    continue;
                }
                let s = unsafe { wide_ptr_to_string(p) };
                if !s.is_empty() {
                    files.push(PathBuf::from(s));
                }
            }
            if services.save_session_with_files(PathBuf::from(path_str), files) {
                // Mirror N++: return lParam unchanged on success so
                // the plugin can chain the call.
                lparam
            } else {
                0
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

        NPPM_ALLOCATESUPPORTED => services.alloc_supported() as isize,

        NPPM_GETAPPDATAPLUGINSALLOWED => services.appdata_plugins_allowed() as isize,

        NPPM_GETCURRENTVIEW => services.current_view() as isize,

        NPPM_GETPLUGINHOMEPATH => {
            // wparam: capacity in TCHARs. lparam: TCHAR* OUT.
            // Returns 1 on success, 0 on bad args or unresolvable
            // plugins dir (sandboxed runner with no config_dir).
            // Same out-buffer contract as `NPPM_GETPLUGINSCONFIGDIR`.
            if lparam == 0 || wparam == 0 {
                return Some(0);
            }
            let Some(path) = services.plugin_home_dir() else {
                return Some(0);
            };
            let cap = wparam.min(MAX_PATH_TCHARS);
            // SAFETY: plugin promises lparam points to at least
            // `wparam` TCHARs (capped at MAX_PATH_TCHARS).
            unsafe {
                write_wide_path(lparam as *mut u16, cap, &path);
            }
            1
        }

        NPPM_GETSETTINGSCLOUDPATH => {
            // Same out-buffer contract; an empty path is
            // legitimate ("no cloud sync configured"). The
            // dispatcher writes an empty wide string (just the NUL
            // terminator) so plugins read a length-0 path rather
            // than uninitialised memory.
            if lparam == 0 || wparam == 0 {
                return Some(0);
            }
            let path = services
                .settings_cloud_dir()
                .unwrap_or_else(|| PathBuf::from(""));
            let cap = wparam.min(MAX_PATH_TCHARS);
            unsafe {
                write_wide_path(lparam as *mut u16, cap, &path);
            }
            1
        }

        NPPM_GETBOOKMARKID => services.bookmark_marker_id() as isize,

        NPPM_GETZOOMLEVEL => services.editor_zoom_level() as isize,

        NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR => {
            // Returns COLORREF (0x00BBGGRR) of STYLE_DEFAULT.
            services.editor_default_fg_color() as isize
        }

        NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR => services.editor_default_bg_color() as isize,

        NPPM_SETSMOOTHFONT => {
            // wparam: BOOL — TRUE picks LCD-optimised, FALSE picks
            // non-antialiased. Returns previous state.
            services.set_smooth_font(wparam != 0) as isize
        }

        NPPM_SETEDITORBORDEREDGE => {
            // wparam: BOOL — TRUE adds `WS_EX_CLIENTEDGE`, FALSE
            // removes. Returns previous state.
            services.set_editor_border_edge(wparam != 0) as isize
        }

        NPPM_SAVEFILE => {
            // wparam: unused. lparam: TCHAR* path of buffer to
            // save. Returns 1 on success, 0 on bad args / unknown
            // path / write failure.
            if lparam == 0 {
                return Some(0);
            }
            let decoded = unsafe { wide_ptr_to_string(lparam as *const u16) };
            if decoded.is_empty() {
                return Some(0);
            }
            services.save_file(PathBuf::from(decoded)) as isize
        }

        NPPM_DISABLEAUTOUPDATE => {
            // wparam / lparam: unused. Plugins call this to opt
            // out of the host's auto-update prompt. Code++ has no
            // auto-update, so the call is unconditionally a
            // no-op — but we accept it cleanly so plugins
            // compiled against the latest header link and run
            // without surfacing an "unhandled NPPM_*" warning at
            // every call.
            tracing::trace!("NPPM_DISABLEAUTOUPDATE: no-op (Code++ has no auto-update)");
            0
        }

        NPPM_ISDOCSWITCHERSHOWN => services.is_doc_switcher_shown() as isize,

        NPPM_SHOWDOCSWITCHER => {
            // wparam: BOOL — TRUE shows, FALSE hides. Returns the
            // *previous* shown state. Code++'s impl is a no-op
            // returning `false` (panel never shown).
            services.set_doc_switcher_shown(wparam != 0) as isize
        }

        NPPM_DOCSWITCHERDISABLECOLUMN => {
            // wparam: column index. lparam: BOOL disable flag.
            // Code++ has no doc-switcher columns; the call is a
            // no-op and returns 0 (the column couldn't be
            // disabled because it doesn't exist).
            services.doc_switcher_disable_column(wparam as i32, lparam != 0);
            0
        }

        NPPM_GETLINENUMBERWIDTHMODE => services.line_number_width_mode() as isize,

        NPPM_SETLINENUMBERWIDTHMODE => {
            // wparam: `LINENUMWIDTH_DYNAMIC` (0) or
            // `LINENUMWIDTH_CONSTANT` (1). Returns 1 on success,
            // 0 on unknown mode. The cast to i32 is safe: valid
            // values are tightly bounded.
            services.set_line_number_width_mode(wparam as i32) as isize
        }

        // RUNCOMMAND_USER family. The two host-environment queries
        // share an out-buffer protocol: wparam = capacity in TCHARs,
        // lparam = TCHAR* OUT. Bad pointer / zero capacity returns 0
        // (failure); successful write returns 1.
        NPPM_GETNPPDIRECTORY | NPPM_GETNPPFULLFILEPATH => {
            if lparam == 0 || wparam == 0 {
                return Some(0);
            }
            let resolved = if msg == NPPM_GETNPPDIRECTORY {
                services.program_dir()
            } else {
                services.program_path()
            };
            let Some(path) = resolved else {
                // Couldn't probe `current_exe()` at all (sandboxed
                // runner, denied `/proc/self/exe`, …). Report
                // failure rather than handing back a guessed path.
                return Some(0);
            };
            let cap = wparam.min(MAX_PATH_TCHARS);
            // SAFETY: plugin promises lparam points to a wide buffer
            // of at least `wparam` TCHARs (we further cap at
            // MAX_PATH_TCHARS).
            unsafe {
                write_wide_path(lparam as *mut u16, cap, &path);
            }
            1
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

/// Write each path in `paths` into the corresponding slot of a
/// plugin-allocated TCHAR** array. Each slot is assumed to point at
/// a buffer of at least `MAX_PATH_TCHARS` wide chars — the documented
/// contract for `NPPM_GETSESSIONFILES` (and its peer
/// `NPPM_GETOPENFILENAMES`). Slots whose pointer is null are skipped
/// without aborting the iteration; null is a plugin bug we keep
/// non-fatal because there is no way to signal partial success
/// through this message's return type.
///
/// # Safety
///
/// `array` must point to at least `paths.len()` `*mut u16` slots.
/// Each non-null slot must point to a writable buffer of at least
/// `MAX_PATH_TCHARS` wide chars.
unsafe fn write_path_array(array: *mut *mut u16, paths: &[std::path::PathBuf]) {
    if array.is_null() {
        return;
    }
    for (i, path) in paths.iter().enumerate() {
        // SAFETY: caller promises `array` has at least `paths.len()`
        // slots. `read_unaligned` because the plugin's TCHAR** is
        // not guaranteed to be aligned (rare, but defensible).
        let slot = unsafe { core::ptr::read_unaligned(array.add(i)) };
        if slot.is_null() {
            tracing::warn!(
                slot = i,
                "NPPM_GETSESSIONFILES: plugin slot is null; skipping",
            );
            continue;
        }
        // SAFETY: per the message ABI, each slot points at a wide
        // buffer of at least MAX_PATH_TCHARS units.
        unsafe {
            write_wide_path(slot, MAX_PATH_TCHARS, path);
        }
    }
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
        /// Tab-strip visibility for `NPPM_HIDETABBAR` /
        /// `NPPM_ISTABBARHIDDEN`. `false` (visible) by default,
        /// matching the host's startup state.
        tabbar_hidden: bool,
        /// Toolbar / main-menu / status-bar visibility flags for
        /// `NPPM_HIDETOOLBAR` / `NPPM_HIDEMENU` /
        /// `NPPM_HIDESTATUSBAR` and their `IS*HIDDEN` peers. All
        /// `false` (visible) by default — matches the host's
        /// startup chrome.
        toolbar_hidden: bool,
        menu_hidden: bool,
        statusbar_hidden: bool,
        /// Per-buffer encoding (UniMode integer). Looked up by
        /// buffer id; missing entries return `-1` matching the
        /// dispatcher's "unknown id" contract.
        buffer_encodings: Vec<(isize, i32)>,
        /// Per-buffer EOL format (EolType integer). Same lookup
        /// shape as `buffer_encodings`.
        buffer_formats: Vec<(isize, i32)>,
        /// Where the host program reports its install dir from. Set
        /// to a known path so the dispatcher tests can assert against
        /// the wide-string write without needing a real
        /// `current_exe()` resolution.
        program_dir: Option<PathBuf>,
        program_path: Option<PathBuf>,
        /// `winVer` enum value to report from `windows_version`.
        /// Defaults to `0` so a test that doesn't care about the
        /// upgraded WV path gets a deterministic, distinct-from-
        /// production-default value.
        windows_version: i32,
        /// Plugin-home / cloud-settings dirs returned by the
        /// long-tail accessors. `None` for the unhappy-path tests.
        plugin_home_dir: Option<PathBuf>,
        settings_cloud_dir: Option<PathBuf>,
        /// Reported zoom level for `NPPM_GETZOOMLEVEL`. Real
        /// Scintilla zoom range is approximately `[-10, 20]`;
        /// tests pin specific values.
        zoom_level: i32,
        /// LRESULT the mock returns from `forward_plugin_message`.
        /// Defaults to 0 (the "target plugin not found" sentinel),
        /// matching the upstream contract.
        forward_plugin_return: isize,
        /// Editor default fg/bg colours for the editor-color
        /// queries. `0x00BBGGRR` (Win32 / Scintilla `COLORREF`).
        editor_default_fg_color: i32,
        editor_default_bg_color: i32,
        /// Smooth-font / border-edge / line-number-width-mode
        /// shadow state. The setters report the previous value
        /// and update; getters read the shadow.
        smooth_font: bool,
        editor_border_edge: bool,
        line_number_width_mode: i32,
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
        fn is_tabbar_hidden(&self) -> bool {
            self.tabbar_hidden
        }
        fn set_tabbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.tabbar_hidden;
            self.tabbar_hidden = hidden;
            self.record(format!("set_tabbar_hidden({hidden})"));
            prev
        }
        fn is_toolbar_hidden(&self) -> bool {
            self.toolbar_hidden
        }
        fn set_toolbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.toolbar_hidden;
            self.toolbar_hidden = hidden;
            self.record(format!("set_toolbar_hidden({hidden})"));
            prev
        }
        fn is_menu_hidden(&self) -> bool {
            self.menu_hidden
        }
        fn set_menu_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.menu_hidden;
            self.menu_hidden = hidden;
            self.record(format!("set_menu_hidden({hidden})"));
            prev
        }
        fn is_statusbar_hidden(&self) -> bool {
            self.statusbar_hidden
        }
        fn set_statusbar_hidden(&mut self, hidden: bool) -> bool {
            let prev = self.statusbar_hidden;
            self.statusbar_hidden = hidden;
            self.record(format!("set_statusbar_hidden({hidden})"));
            prev
        }
        fn save_all_files(&mut self) {
            self.record("save_all");
        }
        fn program_dir(&self) -> Option<PathBuf> {
            self.program_dir.clone()
        }
        fn program_path(&self) -> Option<PathBuf> {
            self.program_path.clone()
        }
        fn windows_version(&self) -> i32 {
            self.windows_version
        }
        fn buffer_position(&self, id: isize) -> Option<(i32, i32)> {
            // Mirror the production HostBridge: tab position is the
            // index in `open_files_primary` of the path bound to
            // `id` via `buffer_paths`. Untitled tabs (no path) are
            // therefore not addressable here, same as production.
            let path = self
                .buffer_paths
                .iter()
                .find(|(i, _)| *i == id)
                .map(|(_, p)| p.clone())?;
            self.open_files_primary
                .iter()
                .position(|p| p == &path)
                .map(|idx| (0, idx as i32))
        }
        fn buffer_id_at(&self, view: i32, pos: i32) -> isize {
            // Single-view: only view 0 yields a valid id. Out-of-
            // range index returns 0 (N++'s "no buffer" sentinel).
            if view != 0 || pos < 0 {
                return 0;
            }
            let pos = pos as usize;
            let Some(path) = self.open_files_primary.get(pos) else {
                return 0;
            };
            self.buffer_paths
                .iter()
                .find(|(_, p)| p == path)
                .map(|(i, _)| *i)
                .unwrap_or(0)
        }
        fn save_current_as(&mut self, path: PathBuf, as_copy: bool) -> bool {
            self.record(format!("save_as={} copy={as_copy}", path.display()));
            true
        }
        fn load_session(&mut self, path: PathBuf) -> bool {
            self.record(format!("load_session={}", path.display()));
            true
        }
        fn save_current_session(&self, path: PathBuf) -> bool {
            self.record(format!("save_current_session={}", path.display()));
            true
        }
        fn save_session_with_files(&self, path: PathBuf, files: Vec<PathBuf>) -> bool {
            let files_str = files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("|");
            self.record(format!(
                "save_session_with_files={} files=[{files_str}]",
                path.display()
            ));
            true
        }
        fn read_session_file_paths(&self, path: PathBuf) -> Option<Vec<PathBuf>> {
            // Test surface: the path's filename is read as a
            // semicolon-separated list of "fake" paths the test
            // wants to pretend the session file contains. Lets us
            // exercise the dispatcher's array-write logic without
            // touching the filesystem. An empty filename means
            // "no session file there" and returns None.
            let fname = path.file_name()?.to_string_lossy().to_string();
            if fname.is_empty() || fname == "MISSING" {
                return None;
            }
            Some(
                fname
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect(),
            )
        }
        fn alloc_supported(&self) -> bool {
            false
        }
        fn appdata_plugins_allowed(&self) -> bool {
            true
        }
        fn current_view(&self) -> i32 {
            0
        }
        fn plugin_home_dir(&self) -> Option<PathBuf> {
            self.plugin_home_dir.clone()
        }
        fn settings_cloud_dir(&self) -> Option<PathBuf> {
            self.settings_cloud_dir.clone()
        }
        fn bookmark_marker_id(&self) -> i32 {
            24
        }
        fn editor_zoom_level(&self) -> i32 {
            self.zoom_level
        }
        fn forward_plugin_message(
            &mut self,
            target_name: &str,
            internal_msg: i32,
            info_ptr: usize,
        ) -> isize {
            self.record(format!(
                "forward_plugin_message[name={target_name},msg={internal_msg},info=0x{info_ptr:x}]"
            ));
            self.forward_plugin_return
        }
        fn editor_default_fg_color(&self) -> i32 {
            self.editor_default_fg_color
        }
        fn editor_default_bg_color(&self) -> i32 {
            self.editor_default_bg_color
        }
        fn set_smooth_font(&mut self, smooth: bool) -> bool {
            let prev = self.smooth_font;
            self.smooth_font = smooth;
            self.record(format!("set_smooth_font({smooth})"));
            prev
        }
        fn set_editor_border_edge(&mut self, enable: bool) -> bool {
            let prev = self.editor_border_edge;
            self.editor_border_edge = enable;
            self.record(format!("set_editor_border_edge({enable})"));
            prev
        }
        fn save_file(&mut self, path: PathBuf) -> bool {
            // Mock: a path equal to one of `buffer_paths` saves
            // successfully; anything else fails. Mirrors the
            // production HostBridge's "active-tab-only" rule
            // without modelling the active-vs-background distinction.
            let known = self.buffer_paths.iter().any(|(_, p)| p == &path);
            self.record(format!("save_file[{}={known}]", path.display()));
            known
        }
        fn is_doc_switcher_shown(&self) -> bool {
            false
        }
        fn set_doc_switcher_shown(&mut self, shown: bool) -> bool {
            self.record(format!("set_doc_switcher_shown({shown})"));
            // Always reports "previously not shown".
            false
        }
        fn doc_switcher_disable_column(&mut self, column_idx: i32, disable: bool) {
            self.record(format!(
                "doc_switcher_disable_column({column_idx},{disable})"
            ));
        }
        fn line_number_width_mode(&self) -> i32 {
            self.line_number_width_mode
        }
        fn set_line_number_width_mode(&mut self, mode: i32) -> bool {
            if !matches!(mode, LINENUMWIDTH_DYNAMIC | LINENUMWIDTH_CONSTANT) {
                return false;
            }
            self.line_number_width_mode = mode;
            self.record(format!("set_line_number_width_mode({mode})"));
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

    #[test]
    fn is_tabbar_hidden_reports_current_state() {
        let mut s = MockServices::default();
        // Default: visible.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ISTABBARHIDDEN, 0, 0) };
        assert_eq!(r, Some(0));

        // Flip to hidden.
        s.tabbar_hidden = true;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ISTABBARHIDDEN, 0, 0) };
        assert_eq!(r, Some(1));
    }

    #[test]
    fn hide_tabbar_returns_previous_state_and_flips() {
        let mut s = MockServices::default();
        // Hide for the first time — previous state was visible (0).
        let r = unsafe { dispatch_nppm(&mut s, NPPM_HIDETABBAR, 1, 0) };
        assert_eq!(r, Some(0));
        assert!(s.tabbar_hidden);

        // Hide again — previous state was hidden (1), no real change.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_HIDETABBAR, 1, 0) };
        assert_eq!(r, Some(1));
        assert!(s.tabbar_hidden);

        // Show — previous state was hidden (1), flips to visible.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_HIDETABBAR, 0, 0) };
        assert_eq!(r, Some(1));
        assert!(!s.tabbar_hidden);
    }

    // --- NPPM_SAVEALLFILES, NPPM_GETPOSFROMBUFFERID,
    //     NPPM_GETBUFFERIDFROMPOS, NPPM_GETWINDOWSVERSION,
    //     NPPM_GETNPPDIRECTORY, NPPM_GETNPPFULLFILEPATH ---

    #[test]
    fn save_all_files_dispatches_to_services() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVEALLFILES, 0, 0) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save_all"]);
    }

    #[test]
    fn pos_from_buffer_id_returns_index_when_known() {
        let mut s = MockServices {
            buffer_paths: vec![
                (7, PathBuf::from("D:/a.txt")),
                (8, PathBuf::from("D:/b.txt")),
            ],
            open_files_primary: vec![PathBuf::from("D:/a.txt"), PathBuf::from("D:/b.txt")],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETPOSFROMBUFFERID, 7, 0) };
        assert_eq!(r, Some(0));
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETPOSFROMBUFFERID, 8, 0) };
        assert_eq!(r, Some(1));
    }

    #[test]
    fn pos_from_buffer_id_unknown_returns_minus_one() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETPOSFROMBUFFERID, 999, 0) };
        assert_eq!(r, Some(-1));
    }

    #[test]
    fn buffer_id_from_pos_returns_id_when_in_range() {
        let mut s = MockServices {
            buffer_paths: vec![
                (7, PathBuf::from("D:/a.txt")),
                (8, PathBuf::from("D:/b.txt")),
            ],
            open_files_primary: vec![PathBuf::from("D:/a.txt"), PathBuf::from("D:/b.txt")],
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERIDFROMPOS, 0, 0) };
        assert_eq!(r, Some(7));
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERIDFROMPOS, 1, 0) };
        assert_eq!(r, Some(8));
    }

    #[test]
    fn buffer_id_from_pos_out_of_range_returns_zero() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/a.txt"))],
            open_files_primary: vec![PathBuf::from("D:/a.txt")],
            ..Default::default()
        };
        // pos beyond the open count.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERIDFROMPOS, 99, 0) };
        assert_eq!(r, Some(0));
        // Secondary view in single-view Code++.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBUFFERIDFROMPOS, 0, 1) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn windows_version_passes_through() {
        // 17 is N++'s `WV_WIN11`; the dispatcher returns whatever the
        // impl reports without re-mapping.
        let mut s = MockServices {
            windows_version: 17,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETWINDOWSVERSION, 0, 0) };
        assert_eq!(r, Some(17));
    }

    #[test]
    fn npp_directory_writes_wide_path_and_returns_one() {
        let mut s = MockServices {
            program_dir: Some(PathBuf::from("C:/Program Files/Code++")),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETNPPDIRECTORY,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        // Find the NUL and decode the prefix.
        let nul = buf.iter().position(|&u| u == 0).unwrap();
        let s = String::from_utf16(&buf[..nul]).unwrap();
        assert_eq!(s, "C:/Program Files/Code++");
    }

    #[test]
    fn npp_directory_zero_capacity_returns_zero() {
        let mut s = MockServices {
            program_dir: Some(PathBuf::from("C:/Program Files/Code++")),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r =
            unsafe { dispatch_nppm(&mut s, NPPM_GETNPPDIRECTORY, 0, buf.as_mut_ptr() as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn npp_directory_null_pointer_returns_zero() {
        let mut s = MockServices {
            program_dir: Some(PathBuf::from("C:/Program Files/Code++")),
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETNPPDIRECTORY, MAX_PATH_TCHARS, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn npp_directory_unresolvable_returns_zero() {
        // Sandboxed runner: program_dir is None.
        let mut s = MockServices::default();
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETNPPDIRECTORY,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn npp_full_file_path_writes_wide_path_and_returns_one() {
        let mut s = MockServices {
            program_path: Some(PathBuf::from("C:/Program Files/Code++/codepp.exe")),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETNPPFULLFILEPATH,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        let nul = buf.iter().position(|&u| u == 0).unwrap();
        let s = String::from_utf16(&buf[..nul]).unwrap();
        assert_eq!(s, "C:/Program Files/Code++/codepp.exe");
    }

    #[test]
    fn runcmd_range_falls_through_for_unknown_offset() {
        // RUNCOMMAND_USER + 1 is in-range but unmapped — the
        // dispatcher's match-arm fallthrough hits the catch-all and
        // returns Some(0). (The catch-all branch is shared with the
        // NPPMSG family's "unhandled" arm.)
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, RUNCOMMAND_USER + 1, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn runcmd_out_of_range_returns_none() {
        // Outside both ranges — falls through to the default
        // wnd_proc per the dispatcher's contract.
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, RUNCOMMAND_USER + RUNCOMMAND_RANGE + 1, 0, 0) };
        assert!(r.is_none());
    }

    // --- Session messages + Save As ---

    #[test]
    fn save_current_file_as_routes_path_and_copy_flag() {
        let mut s = MockServices::default();
        let target = make_wide("D:/copy.txt");
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_SAVECURRENTFILEAS,
                1, // asCopy = TRUE
                target.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save_as=D:/copy.txt copy=true"]);
    }

    #[test]
    fn save_current_file_as_rename_clears_copy_flag() {
        let mut s = MockServices::default();
        let target = make_wide("D:/renamed.txt");
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_SAVECURRENTFILEAS,
                0, // asCopy = FALSE — rename
                target.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save_as=D:/renamed.txt copy=false"]);
    }

    #[test]
    fn save_current_file_as_null_lparam_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVECURRENTFILEAS, 0, 0) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn load_session_routes_path_to_services() {
        let mut s = MockServices::default();
        let p = make_wide("D:/sess.xml");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_LOADSESSION, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["load_session=D:/sess.xml"]);
    }

    #[test]
    fn load_session_null_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_LOADSESSION, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn save_current_session_routes_path_to_services() {
        let mut s = MockServices::default();
        let p = make_wide("D:/save.xml");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVECURRENTSESSION, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save_current_session=D:/save.xml"]);
    }

    #[test]
    fn get_nb_session_files_returns_count() {
        let mut s = MockServices::default();
        // The mock parses the path's filename as a `;`-separated
        // file list — see `read_session_file_paths` impl above.
        let p = make_wide("D:/a.txt;b.txt;c.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETNBSESSIONFILES, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(3));
    }

    #[test]
    fn get_nb_session_files_missing_returns_zero() {
        let mut s = MockServices::default();
        let p = make_wide("D:/MISSING");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETNBSESSIONFILES, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn get_session_files_writes_paths_into_array() {
        let mut s = MockServices::default();
        // 2 slots, each MAX_PATH_TCHARS u16 wide.
        let mut slot_a: Vec<u16> = vec![0; MAX_PATH_TCHARS];
        let mut slot_b: Vec<u16> = vec![0; MAX_PATH_TCHARS];
        let mut array: Vec<*mut u16> = vec![slot_a.as_mut_ptr(), slot_b.as_mut_ptr()];
        let session_path = make_wide("D:/x.txt;y.txt");

        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETSESSIONFILES,
                array.as_mut_ptr() as usize,
                session_path.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));

        let nul_a = slot_a.iter().position(|&u| u == 0).unwrap();
        let nul_b = slot_b.iter().position(|&u| u == 0).unwrap();
        assert_eq!(String::from_utf16(&slot_a[..nul_a]).unwrap(), "x.txt");
        assert_eq!(String::from_utf16(&slot_b[..nul_b]).unwrap(), "y.txt");
    }

    #[test]
    fn get_session_files_null_array_returns_zero() {
        let mut s = MockServices::default();
        let p = make_wide("D:/x.txt;y.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETSESSIONFILES, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn save_session_with_struct_writes_file_list() {
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/out.xml");
        let mut f0 = make_wide("D:/foo.txt");
        let mut f1 = make_wide("D:/bar.txt");
        let mut files_arr: Vec<*mut u16> = vec![f0.as_mut_ptr(), f1.as_mut_ptr()];
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: 2,
            files: files_arr.as_mut_ptr(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        // Success returns lParam unchanged so the plugin can chain.
        assert_eq!(r, Some(info_ptr));
        assert_eq!(
            s.calls(),
            vec!["save_session_with_files=D:/out.xml files=[D:/foo.txt|D:/bar.txt]"]
        );
    }

    #[test]
    fn save_session_with_null_lparam_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn save_session_with_negative_count_returns_zero() {
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/out.xml");
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: -1,
            files: core::ptr::null_mut(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn save_session_with_positive_count_and_null_files_returns_zero() {
        // The dispatcher must reject `nb_file > 0 && files == NULL`
        // before entering the loop — `read_unaligned(files.add(i))`
        // is UB on a null pointer regardless of the unaligned
        // form. Without this guard, a plugin sending the malformed
        // pair would crash the host.
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/out.xml");
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: 5,
            files: core::ptr::null_mut(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        assert_eq!(r, Some(0));
        // No `save_session_with_files` call recorded — the
        // dispatcher rejected before reaching the trait method.
        assert!(s.calls().is_empty());
    }

    #[test]
    fn save_session_with_zero_count_and_null_files_succeeds() {
        // `count == 0` makes the loop a no-op, so a null `files`
        // pointer is harmless — accept it as a degenerate "save an
        // empty session" call.
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/empty.xml");
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: 0,
            files: core::ptr::null_mut(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        assert_eq!(r, Some(info_ptr));
        assert_eq!(
            s.calls(),
            vec!["save_session_with_files=D:/empty.xml files=[]"]
        );
    }

    #[test]
    fn save_session_with_count_above_cap_returns_zero() {
        // A plugin claiming `nb_file = i32::MAX` would otherwise
        // force the host into a multi-GB allocation. The cap
        // (`MAX_SESSION_FILES`) turns that into a clean rejection.
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/huge.xml");
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: i32::MAX,
            files: core::ptr::null_mut(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn save_session_skips_null_file_entries() {
        use crate::ffi::SessionInfo;
        let mut s = MockServices::default();
        let mut path = make_wide("D:/out.xml");
        let mut f0 = make_wide("D:/foo.txt");
        // Middle slot is NULL; the dispatcher skips it without
        // aborting iteration.
        let mut files_arr: Vec<*mut u16> = vec![f0.as_mut_ptr(), core::ptr::null_mut()];
        let info = SessionInfo {
            session_file_path_name: path.as_mut_ptr(),
            nb_file: 2,
            files: files_arr.as_mut_ptr(),
        };
        let info_ptr = &info as *const SessionInfo as isize;
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVESESSION, 0, info_ptr) };
        assert_eq!(r, Some(info_ptr));
        // Only the one real path made it into the recorded list.
        assert_eq!(
            s.calls(),
            vec!["save_session_with_files=D:/out.xml files=[D:/foo.txt]"]
        );
    }

    // --- Chrome toggles: HIDETOOLBAR / HIDEMENU / HIDESTATUSBAR ---

    #[test]
    fn is_toolbar_hidden_reports_state() {
        let mut s = MockServices::default();
        // Default visible.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISTOOLBARHIDDEN, 0, 0) },
            Some(0)
        );
        s.toolbar_hidden = true;
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISTOOLBARHIDDEN, 0, 0) },
            Some(1)
        );
    }

    #[test]
    fn hide_toolbar_returns_previous_and_flips() {
        let mut s = MockServices::default();
        // First hide: previous was visible (0). Same return-value
        // contract as `NPPM_HIDETABBAR` — the *previous* state.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDETOOLBAR, 1, 0) },
            Some(0)
        );
        assert!(s.toolbar_hidden);
        // Second hide: previous was hidden (1) — confirms the
        // contract reports prior state, not the new state.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDETOOLBAR, 1, 0) },
            Some(1)
        );
        // Show: prior was hidden (1), flips to visible.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDETOOLBAR, 0, 0) },
            Some(1)
        );
        assert!(!s.toolbar_hidden);
    }

    #[test]
    fn is_menu_hidden_reports_state() {
        let mut s = MockServices::default();
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISMENUHIDDEN, 0, 0) },
            Some(0)
        );
        s.menu_hidden = true;
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISMENUHIDDEN, 0, 0) },
            Some(1)
        );
    }

    #[test]
    fn hide_menu_returns_previous_and_flips() {
        let mut s = MockServices::default();
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDEMENU, 1, 0) },
            Some(0)
        );
        assert!(s.menu_hidden);
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDEMENU, 0, 0) },
            Some(1)
        );
        assert!(!s.menu_hidden);
    }

    #[test]
    fn is_statusbar_hidden_reports_state() {
        let mut s = MockServices::default();
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISSTATUSBARHIDDEN, 0, 0) },
            Some(0)
        );
        s.statusbar_hidden = true;
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_ISSTATUSBARHIDDEN, 0, 0) },
            Some(1)
        );
    }

    #[test]
    fn hide_statusbar_returns_previous_and_flips() {
        let mut s = MockServices::default();
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDESTATUSBAR, 1, 0) },
            Some(0)
        );
        assert!(s.statusbar_hidden);
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_HIDESTATUSBAR, 0, 0) },
            Some(1)
        );
        assert!(!s.statusbar_hidden);
    }

    // --- ACTIVATEDOC: real per-index activation ---

    #[test]
    fn activate_doc_routes_view_and_pos_to_services() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ACTIVATEDOC, 0, 3) };
        // The mock's `activate_doc` always succeeds; the dispatcher
        // forwards (view, pos) verbatim and turns the bool into 1.
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["activate[0,3]"]);
    }

    #[test]
    fn activate_doc_out_of_range_wparam_returns_zero() {
        // Pre-cast guard: a wparam outside [0, 1] is rejected
        // before the `usize -> i32` truncation. Without this, a
        // plugin sending `wparam = 0x1_0000_0000` would truncate
        // to 0 and silently be accepted as "primary view". Same
        // shape as the guard on `NPPM_ENCODESCI`.
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ACTIVATEDOC, 2, 0) };
        assert_eq!(r, Some(0));
        // Mock should not have been called — the dispatcher arm
        // short-circuited before reaching it.
        assert!(s.calls().is_empty());
    }

    // --- Long-tail accessors ---

    #[test]
    fn alloc_supported_passes_through() {
        let mut s = MockServices::default();
        // Mock returns false; dispatcher relays it as 0.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ALLOCATESUPPORTED, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn appdata_plugins_allowed_returns_true() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETAPPDATAPLUGINSALLOWED, 0, 0) };
        // Mock returns true; relayed as 1.
        assert_eq!(r, Some(1));
    }

    #[test]
    fn current_view_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETCURRENTVIEW, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn plugin_home_path_writes_wide_path() {
        let mut s = MockServices {
            plugin_home_dir: Some(PathBuf::from("C:/Users/me/AppData/Roaming/Code++/plugins")),
            ..Default::default()
        };
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETPLUGINHOMEPATH,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        let nul = buf.iter().position(|&u| u == 0).unwrap();
        let s = String::from_utf16(&buf[..nul]).unwrap();
        assert_eq!(s, "C:/Users/me/AppData/Roaming/Code++/plugins");
    }

    #[test]
    fn plugin_home_path_unresolvable_returns_zero() {
        let mut s = MockServices::default();
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETPLUGINHOMEPATH,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn settings_cloud_path_writes_empty_string_when_none() {
        let mut s = MockServices::default();
        let mut buf = vec![0xFFFFu16; MAX_PATH_TCHARS];
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_GETSETTINGSCLOUDPATH,
                MAX_PATH_TCHARS,
                buf.as_mut_ptr() as isize,
            )
        };
        // Always succeeds — empty path is a legitimate "no cloud
        // sync configured" answer. The dispatcher writes a NUL
        // terminator so plugins read length 0.
        assert_eq!(r, Some(1));
        assert_eq!(buf[0], 0, "empty path should be just a NUL terminator");
    }

    #[test]
    fn bookmark_marker_id_returns_24() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETBOOKMARKID, 0, 0) };
        assert_eq!(r, Some(24));
    }

    #[test]
    fn zoom_level_passes_through() {
        let mut s = MockServices {
            zoom_level: 5,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETZOOMLEVEL, 0, 0) };
        assert_eq!(r, Some(5));
    }

    #[test]
    fn zoom_level_negative_passes_through() {
        let mut s = MockServices {
            zoom_level: -3,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETZOOMLEVEL, 0, 0) };
        // The signed cast survives — `i32` widens to `isize` with
        // sign extension, so -3 stays -3 (not 4_294_967_293).
        assert_eq!(r, Some(-3));
    }

    // --- NPPM_MSGTOPLUGIN ---

    #[test]
    fn msgtoplugin_routes_name_and_internal_msg() {
        use crate::ffi::CommunicationInfo;
        let mut s = MockServices {
            forward_plugin_return: 42,
            ..Default::default()
        };
        let target = make_wide("My Plugin");
        let mut src = make_wide("source plugin");
        let info = CommunicationInfo {
            internal_msg: 1234,
            src_module_name: src.as_mut_ptr(),
            info: core::ptr::null_mut(),
        };
        let info_ptr = &info as *const CommunicationInfo as isize;
        let r =
            unsafe { dispatch_nppm(&mut s, NPPM_MSGTOPLUGIN, target.as_ptr() as usize, info_ptr) };
        assert_eq!(r, Some(42));
        // Mock recorded the forward call with the right args.
        assert_eq!(s.calls().len(), 1);
        let call = &s.calls()[0];
        assert!(call.contains("name=My Plugin"));
        assert!(call.contains("msg=1234"));
    }

    #[test]
    fn msgtoplugin_null_args_return_zero() {
        let mut s = MockServices::default();
        // Null wparam (no target name).
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_MSGTOPLUGIN, 0, 1) },
            Some(0)
        );
        // Null lparam (no CommunicationInfo).
        let target = make_wide("anything");
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_MSGTOPLUGIN, target.as_ptr() as usize, 0) },
            Some(0)
        );
        assert!(s.calls().is_empty(), "no forward call should have happened");
    }

    #[test]
    fn msgtoplugin_empty_name_returns_zero() {
        // A legitimate empty wide string (just the NUL terminator)
        // is rejected the same way an out-of-band name would be —
        // the dispatcher treats `"" == empty` as "no target named".
        let mut s = MockServices::default();
        let empty = make_wide("");
        let dummy_info = make_wide("ignored");
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_MSGTOPLUGIN,
                empty.as_ptr() as usize,
                dummy_info.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn msgtoplugin_negative_lparam_returns_zero() {
        // A negative `lparam` casts to a high-address kernel-mode
        // pointer on Win64. The dispatcher rejects up front so a
        // buggy source plugin can't trick the target's messageProc
        // into dereferencing kernel space.
        let mut s = MockServices::default();
        let target = make_wide("anything");
        let r =
            unsafe { dispatch_nppm(&mut s, NPPM_MSGTOPLUGIN, target.as_ptr() as usize, -1isize) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    #[test]
    fn msgtoplugin_bad_surrogate_name_returns_zero() {
        // A lone high surrogate without its low pair is rejected by
        // `wide_ptr_to_string`'s U+FFFD-substitution check (it
        // refuses to silently turn a malformed name into a
        // different valid string). The dispatcher then sees an
        // empty decoded name and rejects.
        let mut s = MockServices::default();
        let bad: Vec<u16> = vec![0xD800, 0]; // lone high surrogate, NUL-terminated
        let dummy_info = make_wide("ignored");
        let r = unsafe {
            dispatch_nppm(
                &mut s,
                NPPM_MSGTOPLUGIN,
                bad.as_ptr() as usize,
                dummy_info.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    // --- Editor colour queries ---

    #[test]
    fn editor_default_fg_color_passes_through() {
        let mut s = MockServices {
            editor_default_fg_color: 0x00112233,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR, 0, 0) };
        assert_eq!(r, Some(0x00112233));
    }

    #[test]
    fn editor_default_bg_color_passes_through() {
        let mut s = MockServices {
            editor_default_bg_color: 0x00FFFFFF,
            ..Default::default()
        };
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR, 0, 0) };
        assert_eq!(r, Some(0x00FFFFFF));
    }

    // --- SETSMOOTHFONT / SETEDITORBORDEREDGE ---

    #[test]
    fn set_smooth_font_returns_previous_and_flips() {
        let mut s = MockServices::default();
        // First SET TRUE: previous was FALSE (default).
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_SETSMOOTHFONT, 1, 0) },
            Some(0)
        );
        assert!(s.smooth_font);
        // Second SET TRUE: previous was TRUE.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_SETSMOOTHFONT, 1, 0) },
            Some(1)
        );
        // SET FALSE: previous was TRUE, flips off.
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_SETSMOOTHFONT, 0, 0) },
            Some(1)
        );
        assert!(!s.smooth_font);
    }

    #[test]
    fn set_editor_border_edge_returns_previous_and_flips() {
        let mut s = MockServices::default();
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_SETEDITORBORDEREDGE, 1, 0) },
            Some(0)
        );
        assert!(s.editor_border_edge);
        assert_eq!(
            unsafe { dispatch_nppm(&mut s, NPPM_SETEDITORBORDEREDGE, 0, 0) },
            Some(1)
        );
        assert!(!s.editor_border_edge);
    }

    // --- SAVEFILE ---

    #[test]
    fn savefile_routes_known_path_to_services() {
        let mut s = MockServices {
            buffer_paths: vec![(7, PathBuf::from("D:/known.txt"))],
            ..Default::default()
        };
        let p = make_wide("D:/known.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVEFILE, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(1));
        assert_eq!(s.calls(), vec!["save_file[D:/known.txt=true]"]);
    }

    #[test]
    fn savefile_unknown_path_returns_zero() {
        let mut s = MockServices::default();
        let p = make_wide("D:/missing.txt");
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVEFILE, 0, p.as_ptr() as isize) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn savefile_null_lparam_returns_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SAVEFILE, 0, 0) };
        assert_eq!(r, Some(0));
        assert!(s.calls().is_empty());
    }

    // --- DISABLEAUTOUPDATE / Doc switcher trio ---

    #[test]
    fn disable_autoupdate_is_noop_returning_zero() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DISABLEAUTOUPDATE, 0, 0) };
        assert_eq!(r, Some(0));
        // No HostServices call recorded — the dispatcher handles
        // the no-op directly.
        assert!(s.calls().is_empty());
    }

    #[test]
    fn is_doc_switcher_shown_returns_false() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_ISDOCSWITCHERSHOWN, 0, 0) };
        assert_eq!(r, Some(0));
    }

    #[test]
    fn show_doc_switcher_is_noop_returning_false() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SHOWDOCSWITCHER, 1, 0) };
        // Mock returns false (panel never shown). Dispatcher
        // converts to 0.
        assert_eq!(r, Some(0));
        assert_eq!(s.calls(), vec!["set_doc_switcher_shown(true)"]);
    }

    #[test]
    fn doc_switcher_disable_column_routes_args() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_DOCSWITCHERDISABLECOLUMN, 2, 1) };
        assert_eq!(r, Some(0));
        assert_eq!(s.calls(), vec!["doc_switcher_disable_column(2,true)"]);
    }

    // --- Line-number width mode ---

    #[test]
    fn get_line_number_width_mode_returns_dynamic_default() {
        let mut s = MockServices::default();
        let r = unsafe { dispatch_nppm(&mut s, NPPM_GETLINENUMBERWIDTHMODE, 0, 0) };
        assert_eq!(r, Some(LINENUMWIDTH_DYNAMIC as isize));
    }

    #[test]
    fn set_line_number_width_mode_accepts_dynamic_and_constant() {
        let mut s = MockServices::default();
        for mode in [LINENUMWIDTH_DYNAMIC, LINENUMWIDTH_CONSTANT] {
            let r = unsafe { dispatch_nppm(&mut s, NPPM_SETLINENUMBERWIDTHMODE, mode as usize, 0) };
            assert_eq!(r, Some(1), "mode={mode}");
        }
    }

    #[test]
    fn set_line_number_width_mode_rejects_unknown_value() {
        let mut s = MockServices::default();
        // 99 is outside the {DYNAMIC, CONSTANT} set; the mock
        // (and the production HostBridge) reject it.
        let r = unsafe { dispatch_nppm(&mut s, NPPM_SETLINENUMBERWIDTHMODE, 99, 0) };
        assert_eq!(r, Some(0));
    }
}
