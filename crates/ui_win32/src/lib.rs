//! Win32 UI backend for Code++.
//!
//! Phase 2 brings the UI online for the full demo (DESIGN.md §7.2):
//! drag-drop file open, status bar (encoding + EOL + cursor), File →
//! Save with the live editor text, external-change reload prompt, and
//! single-tab session restore. The cross-thread marshaling pattern
//! from §5.4 is wired in as `WM_APP_WAKE`: producer threads in
//! `core::file::Loader` and `platform::watch::FileWatcher` post to
//! their channels and PostMessage the main HWND, which on the next
//! GetMessage iteration runs `Shell::drain` — that's where every
//! worker-loaded buffer is pushed into Scintilla via the direct-call
//! API.
//!
//! Per-window state lives in a heap-allocated `WindowState` whose
//! pointer is stashed in the window's `GWLP_USERDATA` slot. That
//! replaces the process-global `AtomicPtr<HWND>` from Phase 1 and is
//! the standard idiom that scales to multi-window in Phase 3+.

#![cfg(target_os = "windows")]

use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use codepp_core::lang::{CPP_KEYWORDS, C_KEYWORDS, L_C, L_CPP, L_RUST, RUST_KEYWORDS};
use codepp_core::{Encoding, Eol, LangType};
use codepp_editor::EditorHandle;
use codepp_plugin_host::ffi::SCNotification;
use codepp_plugin_host::{Notification, NppData, NPPMSG, NPPMSG_RANGE, PLUGIN_CMD_ID_BASE};
use codepp_scintilla_sys::{
    ScintillaDirectFunction, Scintilla_RegisterClasses, SCE_C_CHARACTER, SCE_C_COMMENT,
    SCE_C_COMMENTDOC, SCE_C_COMMENTLINE, SCE_C_COMMENTLINEDOC, SCE_C_NUMBER, SCE_C_OPERATOR,
    SCE_C_PREPROCESSOR, SCE_C_STRING, SCE_C_WORD, SCE_C_WORD2, SCE_RUST_CHARACTER,
    SCE_RUST_COMMENTBLOCK, SCE_RUST_COMMENTBLOCKDOC, SCE_RUST_COMMENTLINE, SCE_RUST_COMMENTLINEDOC,
    SCE_RUST_LIFETIME, SCE_RUST_MACRO, SCE_RUST_NUMBER, SCE_RUST_OPERATOR, SCE_RUST_STRING,
    SCE_RUST_WORD, SCE_RUST_WORD2, SCI_BEGINUNDOACTION, SCI_CLEAR, SCI_COPY, SCI_CREATEDOCUMENT,
    SCI_CUT, SCI_EMPTYUNDOBUFFER, SCI_ENDUNDOACTION, SCI_GETCURRENTPOS, SCI_GETDIRECTFUNCTION,
    SCI_GETDIRECTPOINTER, SCI_GETLENGTH, SCI_GETLINECOUNT, SCI_GETSELECTIONEND,
    SCI_GETSELECTIONSTART, SCI_GETTEXT, SCI_GETVIEWEOL, SCI_GETVIEWWS, SCI_GETWRAPMODE,
    SCI_GOTOLINE, SCI_GOTOPOS, SCI_LINEFROMPOSITION, SCI_PASTE, SCI_REDO, SCI_RELEASEDOCUMENT,
    SCI_SELECTALL, SCI_SETDOCPOINTER, SCI_SETSAVEPOINT, SCI_SETSCROLLWIDTH,
    SCI_SETSCROLLWIDTHTRACKING, SCI_SETSELECTIONEND, SCI_SETSELECTIONSTART, SCI_SETTEXT,
    SCI_SETVIEWEOL, SCI_SETVIEWWS, SCI_SETWRAPMODE, SCI_SETZOOM, SCI_UNDO, SCI_ZOOMIN, SCI_ZOOMOUT,
    SCN_MODIFIED, SC_DOCUMENTOPTION_DEFAULT, SC_MOD_DELETETEXT, SC_MOD_INSERTTEXT, STYLE_DEFAULT,
};
use codepp_shell::{HostHandles, PendingDialog, SearchFlags, Shell, Tab, UiPlatform};
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetStockObject, GetSysColorBrush, SetBkMode, COLOR_WINDOW, DEFAULT_GUI_FONT, HBRUSH, HDC,
    HFONT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, ICC_BAR_CLASSES, ICC_TAB_CLASSES, INITCOMMONCONTROLSEX,
    NMHDR, TCITEMW, TCM_DELETEITEM, TCM_GETCURSEL, TCM_INSERTITEMW, TCM_SETCURSEL, TCM_SETITEMW,
    TCN_SELCHANGE, WC_TABCONTROL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, SetFocus, VK_0, VK_F, VK_G, VK_H, VK_OEM_MINUS, VK_OEM_PLUS, VK_S, VK_W,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CheckMenuRadioItem, CreateAcceleratorTableW, CreateMenu,
    CreateWindowExW, DefWindowProcW, DeleteMenu, DestroyWindow, DispatchMessageW, DrawMenuBar,
    GetClientRect, GetMenuItemCount, GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    IsDialogMessageW, IsWindow, LoadCursorW, MessageBoxW, MoveWindow, PostMessageW,
    PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateAcceleratorW, TranslateMessage, ACCEL, ACCEL_VIRT_FLAGS, BM_SETCHECK, BN_CLICKED,
    BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, ES_AUTOHSCROLL, ES_NUMBER, FCONTROL, FSHIFT, FVIRTKEY, GWLP_USERDATA, HACCEL,
    HMENU, IDCANCEL, IDC_ARROW, IDOK, IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING,
    MB_OK, MB_YESNO, MF_BYCOMMAND, MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DROPFILES, WM_INITMENUPOPUP,
    WM_NCCREATE, WM_NOTIFY, WM_QUIT, WM_SETFOCUS, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_CAPTION,
    WS_CHILD, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_GROUP, WS_OVERLAPPEDWINDOW, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

// --- Built-in menu command ids ----------------------------------------
//
// The host's built-in WM_COMMAND ids live in 1000..=1999. Plugin
// command ids start at PLUGIN_CMD_ID_BASE = 50000, well above any
// host-built-in. Two ranges inside 1000..=1999 are *dynamic* — the
// cmd id encodes a parameter the WM_COMMAND handler decodes back:
//
//   ID_LANGUAGE_BASE..ID_LANGUAGE_END  → LangType id
//   ID_WINDOW_BASE..ID_WINDOW_END      → tab index
//
// The static ids below avoid those two ranges so a click on a static
// item never collides with a dynamic one.
//
// File (1000-1099)
const ID_FILE_SAVE: u16 = 1000;
const ID_FILE_EXIT: u16 = 1001;
const ID_FILE_CLOSE: u16 = 1002;
const ID_FILE_NEW: u16 = 1003;
const ID_FILE_OPEN: u16 = 1004;
const ID_FILE_SAVE_AS: u16 = 1005;
const ID_FILE_SAVE_ALL: u16 = 1006;
const ID_FILE_CLOSE_ALL: u16 = 1007;
const ID_FILE_RELOAD: u16 = 1008;
const ID_FILE_PRINT: u16 = 1009;

// Edit (1100-1199) — most route directly to Scintilla via SCI_*.
const ID_EDIT_UNDO: u16 = 1100;
const ID_EDIT_REDO: u16 = 1101;
const ID_EDIT_CUT: u16 = 1102;
const ID_EDIT_COPY: u16 = 1103;
const ID_EDIT_PASTE: u16 = 1104;
const ID_EDIT_DELETE: u16 = 1105;
const ID_EDIT_SELECTALL: u16 = 1106;

// Search (1200-1299) — disabled stubs until Phase 4 m3 wires
// Find/Replace dialogs.
const ID_SEARCH_FIND: u16 = 1200;
const ID_SEARCH_FINDNEXT: u16 = 1201;
const ID_SEARCH_FINDPREV: u16 = 1202;
const ID_SEARCH_REPLACE: u16 = 1203;
const ID_SEARCH_FINDINFILES: u16 = 1204;
const ID_SEARCH_GOTOLINE: u16 = 1205;

// View (1300-1399).
const ID_VIEW_WORDWRAP: u16 = 1300;
const ID_VIEW_SHOWWS: u16 = 1301;
const ID_VIEW_SHOWEOL: u16 = 1302;
const ID_VIEW_ZOOMIN: u16 = 1303;
const ID_VIEW_ZOOMOUT: u16 = 1304;
const ID_VIEW_ZOOMRESET: u16 = 1305;

// Encoding (1400-1499) — m1 ships the menu shape with disabled
// conversion stubs; the radio marker reflects the active buffer's
// encoding for plugins/users that read the menu state. m5 wires
// the conversions.
//
// Order matters: `CheckMenuRadioItem` takes a [first_id, last_id]
// closed range to clear-then-set. If the range is inverted the
// "clear other items in range" pass becomes a no-op and stale
// radio marks pile up. Keep these strictly ascending.
const ID_ENCODING_ANSI: u16 = 1400;
const ID_ENCODING_UTF8: u16 = 1401;
const ID_ENCODING_UTF8_BOM: u16 = 1402;
const ID_ENCODING_UTF16_LE: u16 = 1403;
const ID_ENCODING_UTF16_BE: u16 = 1404;

// Language (1500-1599) — *dynamic*: cmd id = ID_LANGUAGE_BASE +
// LangType numeric id. LangType ids run 0..=93 in the N++ ABI; we
// reserve a 100-wide window so the full set fits. `_END` is the
// last valid id (inclusive), giving 100 slots [BASE, END].
const ID_LANGUAGE_BASE: u16 = 1500;
const ID_LANGUAGE_END: u16 = 1599;
const ID_LANGUAGE_CAP: usize = (ID_LANGUAGE_END - ID_LANGUAGE_BASE + 1) as usize;

// Settings / Tools / Macro / Run (1600-1699) — disabled stubs.
const ID_SETTINGS_PREFERENCES: u16 = 1600;

// Window (1700-1799) — *dynamic*: cmd id = ID_WINDOW_BASE + tab idx.
// 100-wide window caps the visible-by-shortcut tab list. `_END` is
// inclusive (last valid id); `_CAP` is the count of usable slots.
const ID_WINDOW_BASE: u16 = 1700;
const ID_WINDOW_END: u16 = 1799;
const ID_WINDOW_CAP: usize = (ID_WINDOW_END - ID_WINDOW_BASE + 1) as usize;

// Help (1800-1899).
const ID_HELP_ABOUT: u16 = 1800;

/// Our cross-thread wake-up message. Producer threads `PostMessage`
/// this to drag the UI thread out of its `GetMessageW` idle and into
/// the `Shell::drain` path.
const WM_APP_WAKE: u32 = WM_APP + 1;

const MAIN_CLASS: PCWSTR = w!("CodePlusPlusMainWindow");
const SCINTILLA_CLASS: PCWSTR = w!("Scintilla");
const STATUSBAR_CLASS: PCWSTR = w!("msctls_statusbar32");

/// Window class for the "Go to..." modal popup. Registered once on
/// first `show_goto_dialog`. The dialog is a plain top-level
/// `WS_POPUP`/`WS_CAPTION`/`WS_SYSMENU` window with our own wnd_proc;
/// `IsDialogMessageW` in the modal pump still handles Tab navigation
/// and the IDOK/IDCANCEL keyboard contract because the window has
/// `WS_EX_CONTROLPARENT` and the controls have `WS_TABSTOP`.
const GOTO_CLASS: PCWSTR = w!("CodePlusPlusGotoDialog");

/// "Go to..." dialog control ids. IDOK / IDCANCEL are the standard
/// Win32 button ids and are reused for the dialog's OK and Cancel
/// buttons. The radio pair toggles between Line and Offset mode;
/// HERE / TARGET / MAX are the three labeled boxes.
const IDC_GOTO_RADIO_LINE: u16 = 100;
const IDC_GOTO_RADIO_OFFSET: u16 = 101;
const IDC_GOTO_HERE: u16 = 102;
const IDC_GOTO_TARGET: u16 = 103;
const IDC_GOTO_MAX: u16 = 104;

/// Per-window state. Box-allocated, pointer stashed in
/// `GWLP_USERDATA`. wnd_proc reads it back via
/// `GetWindowLongPtrW(GWLP_USERDATA)` on every message. The main
/// window's own HWND is passed to wnd_proc on every dispatch, so we
/// don't store it here.
struct WindowState {
    scintilla_hwnd: HWND,
    status_hwnd: HWND,
    /// Win32 tab control HWND. Sits between the menu bar and the
    /// Scintilla view; one tab item per `Shell.tabs[i]`. Multi-tab
    /// (Phase 3 milestone 6b) uses `SCI_SETDOCPOINTER` to repoint
    /// the single Scintilla view at the active tab's document on
    /// each click.
    tab_hwnd: HWND,
    /// Number of tab items currently inserted into [`Self::tab_hwnd`].
    /// Compared against `state.shell.tabs.len()` after each drain
    /// so newly-pushed Shell tabs get a tab-control item without
    /// rebuilding the whole strip from scratch.
    synced_tab_count: usize,
    /// HMENU for the main menu bar — the parent of File/Edit/.../Plugins.
    /// Plugins query this via `NPPM_GETMENUHANDLE(NPPMAINMENU)` to
    /// install accelerator-bound items at the top level.
    main_menu: HMENU,
    /// HMENU for the per-plugin submenu under "Plugins". Plugins query
    /// this via `NPPM_GETMENUHANDLE(NPPPLUGINMENU)` to add their menu
    /// items. Populated lazily on the first `WM_INITMENUPOPUP` for
    /// this submenu (DESIGN.md §6.4 lazy-load contract).
    plugin_menu: HMENU,
    /// Set once the lazy-load + menu-population dance has run for
    /// `plugin_menu`. Subsequent `WM_INITMENUPOPUP` for the same
    /// menu skip the work; we never reload plugins after the first
    /// touch, even if the user re-opens the menu.
    plugins_menu_initialized: bool,
    /// HMENUs of the four menus whose contents reflect live state
    /// (encoding radio, language radio, View toggle/check marks,
    /// Window's per-tab list). The popup HMENU on
    /// `WM_INITMENUPOPUP` is matched against these to dispatch the
    /// right refresh routine.
    view_menu: HMENU,
    encoding_menu: HMENU,
    language_menu: HMENU,
    window_menu: HMENU,
    editor: EditorHandle,
    shell: Shell,
}

impl WindowState {
    /// Split into a (shell, ui-platform) pair so we can call
    /// `shell.drain(ui)` without aliasing `&mut self`. `Win32Ui` is
    /// Copy-cheap (two pointer-sized values + EditorHandle) so we just
    /// produce a fresh one per call.
    fn split(&mut self) -> (&mut Shell, Win32Ui) {
        let ui = Win32Ui {
            status_hwnd: self.status_hwnd,
            editor: self.editor,
        };
        (&mut self.shell, ui)
    }

    /// Build the per-call `HostHandles` the plugin dispatcher consumes.
    /// Centralized so the route in `WM_USER+1000..` and any future
    /// notification call site share one definition of "what the host's
    /// handles are right now." `HWND` and `HMENU` in windows-rs 0.62
    /// are both `pub struct(pub *mut c_void)`, so `.0` already has
    /// the `Hwnd = *mut c_void` shape the plugin-host crate expects.
    fn host_handles(&self, npp_hwnd: HWND) -> HostHandles {
        HostHandles {
            npp_hwnd: npp_hwnd.0,
            scintilla_main: self.scintilla_hwnd.0,
            scintilla_secondary: core::ptr::null_mut(),
            plugin_menu: self.plugin_menu.0,
            main_menu: self.main_menu.0,
        }
    }
}

/// `UiPlatform` impl. Lightweight — just carries the HWND values
/// `Shell::drain` needs to reach the editor and status bar. The
/// main HWND is intentionally absent: dialogs that need it are
/// deferred (`PendingDialog`) and shown by wnd_proc using its own
/// HWND parameter, so no Win32Ui method needs main_hwnd. The tab
/// HWND is also absent: `activate_tab` only needs the Scintilla
/// view (via `editor`); the visual tab-strip selection is driven
/// by `sync_tab_strip` after the drain, not by the trait method.
#[derive(Clone, Copy)]
struct Win32Ui {
    status_hwnd: HWND,
    editor: EditorHandle,
}

impl UiPlatform for Win32Ui {
    fn activate_tab(&mut self, _idx: usize, scintilla_doc: isize) -> isize {
        // Resolve the document pointer. A zero `scintilla_doc` is
        // the contract for "no document yet, please create one"
        // — happens the first time a Tab is bound to the view.
        // SCI_CREATEDOCUMENT(wparam = bytes hint, lparam = options)
        // returns the new doc pointer with refcount 1. We then
        // SCI_SETDOCPOINTER to bind it; that bumps the refcount
        // and decrements the previously-bound doc's refcount, but
        // doesn't free either — Scintilla keeps the previous
        // document alive as long as anyone holds a refcount or
        // it's still pointed at.
        let doc = if scintilla_doc != 0 {
            scintilla_doc
        } else {
            self.editor
                .send(SCI_CREATEDOCUMENT, 0, SC_DOCUMENTOPTION_DEFAULT)
        };
        // Bind the resolved document to the single Scintilla view.
        // wparam is unused; lparam is the doc pointer.
        self.editor.send(SCI_SETDOCPOINTER, 0, doc);
        doc
    }

    fn set_buffer_text(&mut self, text: &str, cursor: u64) {
        // SCI_SETTEXT requires a null-terminated UTF-8 string. Build
        // a single buffer; the address is valid for the duration of
        // the synchronous direct-call.
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        self.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
        // Loaded buffers start clean — clear the undo history (Scintilla
        // would otherwise let Ctrl+Z undo the load itself, then mark
        // everything as deleted) and reset the save point so the title
        // bar isn't asterisk'd.
        self.editor.send(SCI_EMPTYUNDOBUFFER, 0, 0);
        self.editor.send(SCI_SETSAVEPOINT, 0, 0);
        self.editor.send(SCI_GOTOPOS, cursor as usize, 0);
    }

    fn get_buffer_text(&mut self) -> String {
        let len = self.editor.send(SCI_GETLENGTH, 0, 0);
        if len <= 0 {
            return String::new();
        }
        // SCI_GETTEXT writes len+1 bytes (null-terminator inclusive)
        // into our buffer; the wparam is the buffer size including
        // the null.
        let cap = len as usize + 1;
        let mut buf = vec![0u8; cap];
        let written = self
            .editor
            .send(SCI_GETTEXT, cap, buf.as_mut_ptr() as isize);
        if written <= 0 {
            return String::new();
        }
        // Drop the trailing null Scintilla wrote.
        buf.truncate(written as usize);
        // Scintilla stores text as the user types — bytes are valid
        // UTF-8 if the buffer is in UTF-8 mode (our default). Use
        // from_utf8_lossy as a defensive measure: if some weird code
        // path inserted invalid bytes, we get U+FFFD rather than
        // panicking. The save path will then surface an encoding
        // error if those bytes can't be re-encoded.
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn get_cursor_pos(&mut self) -> u64 {
        let pos = self.editor.send(SCI_GETCURRENTPOS, 0, 0);
        pos.max(0) as u64
    }

    fn update_status(&mut self, encoding: &Encoding, eol: Eol, byte_len: u64) {
        let text = format!(
            "  {} | {} | {} bytes",
            encoding.label(),
            eol.label(),
            byte_len
        );
        write_status_part(self.status_hwnd, 0, &text);
    }

    fn set_plugin_status(&mut self, section: usize, text: &str) {
        // NPPM_SETSTATUSBAR sections: 0 = doc info, 1 = type, 2 =
        // encoding, etc. The Phase 3 status bar is single-part, so
        // we collapse all plugin-supplied sections onto part 0; the
        // next `update_status` call repaints the standard fields.
        //
        // **Known limitation:** a plugin that calls NPPM_SETSTATUSBAR
        // twice with different sections sees only the last write
        // until milestone 6's multi-part status bar lands. Plugins
        // that depend on per-section persistence will visibly
        // misbehave; this is a documented surface deviation, not a
        // bug to file.
        let _ = section;
        write_status_part(self.status_hwnd, 0, text);
    }

    fn apply_lang(&mut self, lang: LangType) {
        // Look up the Lexilla lexer name in core::lang. None means
        // either L_TEXT (no lexer wanted) or a language whose lexer
        // isn't yet in build.rs's static-link set — both fall through
        // to clear_lexer so the view renders as plain text instead
        // of carrying the previous tab's lexer state.
        let Some(name) = lang.lexer_name() else {
            self.editor.clear_lexer();
            // Reset the styles to a single default — otherwise the
            // previous lexer's per-style colours stay applied to
            // whatever style indices Scintilla picks for unstyled
            // text, producing visual contamination on tab switch.
            apply_default_styles(&self.editor);
            return;
        };
        if !self.editor.set_lexer_by_name(name) {
            // Lexilla returned NULL — the static build doesn't
            // contain this lexer. Trace and fall through to plain
            // rendering.
            tracing::warn!(lexer = name, "CreateLexer returned NULL");
            self.editor.clear_lexer();
            apply_default_styles(&self.editor);
            return;
        }
        // Per-language theming + keywords. The set is small enough
        // that an inline branch is clearer than a per-language
        // table; it grows as Phase 4 adds lexers.
        // Order: keywords → reset-then-restyle. `set_keywords` only
        // mutates the lexer's internal word list (no style state),
        // so it doesn't matter that `apply_default_styles` runs
        // after — the keyword list survives the style reset. Listed
        // first to keep this method's logical order
        // (lexer-data-then-paint) clearer at the call site.
        if lang == L_C || lang == L_CPP {
            self.editor.set_keywords(
                0,
                if lang == L_C {
                    C_KEYWORDS
                } else {
                    CPP_KEYWORDS
                },
            );
            apply_default_styles(&self.editor);
            apply_cpp_theme(&self.editor);
        } else if lang == L_RUST {
            self.editor.set_keywords(0, RUST_KEYWORDS);
            apply_default_styles(&self.editor);
            apply_rust_theme(&self.editor);
        }
    }

    fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
        // Anchor the search at the current selection so Find Next
        // walks from where the user last left off (rather than
        // re-finding the same match each call). search_next takes
        // flags directly via wparam — no sticky-state surprise
        // from a previous Replace All having left SCFIND_REGEX set.
        self.editor.search_anchor();
        match self.editor.search_next(query, flags.bits()) {
            -1 => None,
            pos => Some(pos as u64),
        }
    }

    fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
        self.editor.search_anchor();
        match self.editor.search_prev(query, flags.bits()) {
            -1 => None,
            pos => Some(pos as u64),
        }
    }

    fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool {
        // Defense-in-depth empty-query guard: `Shell::replace_current`
        // also gates this, but a future caller that bypasses the
        // shell-level wrapper would otherwise hit Scintilla's
        // permissive empty-target-match behaviour and replace the
        // selection with `replacement` regardless of content.
        if query.is_empty() {
            return false;
        }
        // Use the target range to verify the current selection
        // matches the search query before replacing — guards
        // against the user dragging a different selection between
        // a Find and the Replace click. SCI_GET{SELECTIONSTART,END}
        // gives the selection range; SCI_SETTARGETRANGE pins it as
        // the target; SCI_SEARCHINTARGET returns nonneg iff the
        // target text matches `query` under `flags`. SEARCHINTARGET
        // (unlike SEARCH{NEXT,PREV}) reads flags from the sticky
        // state set by SCI_SETSEARCHFLAGS, so set them first.
        let sel_start = self.editor.send(SCI_GETSELECTIONSTART, 0, 0).max(0) as u64;
        let sel_end = self.editor.send(SCI_GETSELECTIONEND, 0, 0).max(0) as u64;
        if sel_start == sel_end {
            return false;
        }
        self.editor.set_search_flags(flags.bits());
        self.editor.set_target_range(sel_start, sel_end);
        if self.editor.search_in_target(query) < 0 {
            return false;
        }
        // SEARCHINTARGET narrows the target to the match; replace
        // exactly that. Then re-anchor selection on the inserted
        // text so a subsequent Find Next walks past it.
        self.editor.replace_target(replacement);
        let new_end = self.editor.target_end();
        self.editor
            .send(SCI_SETSELECTIONSTART, sel_start as usize, 0);
        self.editor.send(SCI_SETSELECTIONEND, new_end as usize, 0);
        true
    }

    fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize {
        if query.is_empty() {
            return 0;
        }
        // Wrap the whole iteration in a single Scintilla undo group
        // so Ctrl+Z reverts the entire Replace All in one step.
        // SEARCHINTARGET reads flags from the sticky state set
        // here once before the loop.
        self.editor.set_search_flags(flags.bits());
        self.editor.send(SCI_BEGINUNDOACTION, 0, 0);
        let mut count = 0usize;
        let mut cursor = 0u64;
        loop {
            // Re-read the document length each iteration — the
            // previous replace_target may have grown or shrunk
            // the document, so a precomputed end would miss
            // matches past the size delta or stop short of them.
            let doc_len = self.editor.send(SCI_GETLENGTH, 0, 0).max(0) as u64;
            self.editor.set_target_range(cursor, doc_len);
            let hit = self.editor.search_in_target(query);
            if hit < 0 {
                break;
            }
            self.editor.replace_target(replacement);
            // Resume just past the inserted replacement; without
            // this, replacing "a" with "ab" would re-match the
            // newly-inserted "a" and loop forever.
            cursor = self.editor.target_end();
            count += 1;
        }
        self.editor.send(SCI_ENDUNDOACTION, 0, 0);
        count
    }

    // confirm_reload and show_error were intentionally removed from
    // UiPlatform: each runs `MessageBoxW` whose nested message pump
    // can re-enter the wnd_proc and produce aliasing UB on the
    // GWLP_USERDATA-borrowed WindowState. Modal dialogs are deferred
    // — `Shell::drain` returns `Vec<PendingDialog>` that the wnd_proc
    // shows after the borrow is dropped (see `WM_APP_WAKE`).
}

// --- Phase 4 m1 default theme -------------------------------------------
//
// Scintilla colours are `COLORREF`-encoded `0x00BBGGRR`: the low byte is
// red, middle is green, high is blue. The values below are a single
// hand-picked light-background scheme — black on white, with greenish
// comments, blue keywords, brick-red strings, magenta numbers. Phase 4
// m2 will route the theme through a dedicated module so users can
// switch schemes; the inline approach here is the smallest thing that
// makes the demo gate ("open .cpp, see colour") visible.

const FG_DEFAULT: u32 = 0x00_00_00_00; // black
const BG_DEFAULT: u32 = 0x00_FF_FF_FF; // white
const FG_COMMENT: u32 = 0x00_00_80_00; // green (BBGGRR)
const FG_KEYWORD: u32 = 0x00_FF_00_00; // blue
const FG_KEYWORD2: u32 = 0x00_C0_60_00; // steel blue
const FG_STRING: u32 = 0x00_22_22_99; // brick red (BBGGRR -> R=99 G=22 B=22)
const FG_NUMBER: u32 = 0x00_80_00_80; // magenta
const FG_PREPROC: u32 = 0x00_80_40_80; // purple
const FG_OPERATOR: u32 = 0x00_30_30_30; // dark grey
const FG_LIFETIME: u32 = 0x00_00_60_C0; // amber
const FG_MACRO: u32 = 0x00_80_30_80; // violet

/// Initialise STYLE_DEFAULT then reset every other style to it. This is
/// Scintilla's idiomatic "blank slate before lexer-specific styling"
/// sequence and stops the previous lexer's colours from leaking through
/// on lexer switch. Editor must already be bound to the document the
/// caller wants to style.
fn apply_default_styles(editor: &EditorHandle) {
    editor.style_set_fore(STYLE_DEFAULT, FG_DEFAULT);
    editor.style_set_back(STYLE_DEFAULT, BG_DEFAULT);
    editor.style_clear_all();
}

/// LexCPP per-style overrides. Both C and C++ buffers share this
/// theme; the only thing that varies is the keyword list installed
/// via `SCI_SETKEYWORDS`.
fn apply_cpp_theme(editor: &EditorHandle) {
    editor.style_set_fore(SCE_C_COMMENT, FG_COMMENT);
    editor.style_set_fore(SCE_C_COMMENTLINE, FG_COMMENT);
    editor.style_set_fore(SCE_C_COMMENTDOC, FG_COMMENT);
    editor.style_set_fore(SCE_C_COMMENTLINEDOC, FG_COMMENT);
    editor.style_set_italic(SCE_C_COMMENT, true);
    editor.style_set_italic(SCE_C_COMMENTLINE, true);
    editor.style_set_fore(SCE_C_WORD, FG_KEYWORD);
    editor.style_set_bold(SCE_C_WORD, true);
    editor.style_set_fore(SCE_C_WORD2, FG_KEYWORD2);
    editor.style_set_fore(SCE_C_STRING, FG_STRING);
    editor.style_set_fore(SCE_C_CHARACTER, FG_STRING);
    editor.style_set_fore(SCE_C_NUMBER, FG_NUMBER);
    editor.style_set_fore(SCE_C_PREPROCESSOR, FG_PREPROC);
    editor.style_set_fore(SCE_C_OPERATOR, FG_OPERATOR);
}

/// LexRust per-style overrides. Style indices differ from LexCPP — see
/// `vendor/lexilla/include/SciLexer.h` `SCE_RUST_*`.
fn apply_rust_theme(editor: &EditorHandle) {
    editor.style_set_fore(SCE_RUST_COMMENTBLOCK, FG_COMMENT);
    editor.style_set_fore(SCE_RUST_COMMENTLINE, FG_COMMENT);
    editor.style_set_fore(SCE_RUST_COMMENTBLOCKDOC, FG_COMMENT);
    editor.style_set_fore(SCE_RUST_COMMENTLINEDOC, FG_COMMENT);
    editor.style_set_italic(SCE_RUST_COMMENTBLOCK, true);
    editor.style_set_italic(SCE_RUST_COMMENTLINE, true);
    editor.style_set_fore(SCE_RUST_WORD, FG_KEYWORD);
    editor.style_set_bold(SCE_RUST_WORD, true);
    editor.style_set_fore(SCE_RUST_WORD2, FG_KEYWORD2);
    editor.style_set_fore(SCE_RUST_STRING, FG_STRING);
    editor.style_set_fore(SCE_RUST_CHARACTER, FG_STRING);
    editor.style_set_fore(SCE_RUST_NUMBER, FG_NUMBER);
    editor.style_set_fore(SCE_RUST_OPERATOR, FG_OPERATOR);
    editor.style_set_fore(SCE_RUST_LIFETIME, FG_LIFETIME);
    editor.style_set_fore(SCE_RUST_MACRO, FG_MACRO);
}

/// Fire every queued NPPN_* notification through `Shell::notify_plugins`,
/// each call wrapped in `PluginCallGuard` (re-entrance guard) and
/// `catch_unwind` (host-internal panics must not unwind across the
/// `extern "system"` wnd_proc frame).
///
/// Each notification grabs a fresh `&mut WindowState` borrow, calls
/// notify_plugins (which iterates plugins through `&Shell`), then
/// drops the borrow before the next iteration. A plugin's beNotified
/// that `SendMessage(NPPM_*)`s back hits `state_from_hwnd → None`
/// while the guard is set; the inner wnd_proc returns 0 and the
/// outer borrow stays sound.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn fire_queued_notifications(hwnd: HWND) {
    // Drain the queue under one borrow, then release before
    // calling into plugin code — `take_notifications` only needs
    // `&mut Shell` for the swap, no plugin code runs inside it.
    // SAFETY: caller's contract requires UI-thread invocation;
    // state_from_hwnd's own contract is satisfied there.
    let notifications = if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        state.shell.take_notifications()
    } else {
        Vec::new()
    };
    if notifications.is_empty() {
        return;
    }
    for notification in notifications {
        // SAFETY: same as above; UI-thread call.
        if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
            // The guard is created INSIDE the catch_unwind closure
            // so that a panic from `PluginCallGuard::enter()` (the
            // nesting-detection assert) is caught here rather than
            // unwinding across the extern "system" wnd_proc frame.
            // The guard's Drop runs when the closure exits (panic
            // or normal return), tightly scoping
            // PLUGIN_CALL_ACTIVE to the plugin call.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = PluginCallGuard::enter();
                state.shell.notify_plugins(notification, hwnd.0);
            }));
        }
        // Borrow on `state` ends at the end of each iteration so
        // the next iteration acquires fresh.
    }
}

/// Close the active tab in response to `WM_COMMAND(ID_FILE_CLOSE)`
/// (Ctrl+W accelerator, File → Close menu item, or future
/// right-click → Close). Drives the platform-side cleanup the
/// shell-side `close_active_tab` doesn't know about: removes the
/// item from the tab control, releases the closed document via
/// `SCI_RELEASEDOCUMENT` (drops Scintilla's last refcount on the
/// buffer so memory is freed), and rebinds the view to the new
/// active document if there is one. Closing the last tab leaves
/// the view bound to a fresh empty document until the next
/// `open_file`.
///
/// Notifications (`NPPN_FILECLOSED` + possibly
/// `NPPN_BUFFERACTIVATED`) are queued by the shell-side close;
/// we deliver them via `fire_queued_notifications` after the
/// borrow ends, so plugin `beNotified` callbacks see a fully
/// quiesced state.
///
/// The function body is wrapped in `catch_unwind` so a host-
/// internal panic (allocation failure inside `Vec::push` or
/// `String::clone`, a misbehaving `tracing` subscriber, the
/// `allocate_buffer_id` overflow assert) doesn't unwind across
/// the `extern "system"` wnd_proc frame — that's UB. Plugin
/// code never runs here, so the wrap is purely a defense
/// against host-internal panics.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn handle_close_active_tab(hwnd: HWND) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: caller's UI-thread contract carries.
        unsafe {
            handle_close_active_tab_inner(hwnd);
        }
    }));
}

/// Body of [`handle_close_active_tab`], factored out so the
/// caller can wrap it in a single `catch_unwind`.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn handle_close_active_tab_inner(hwnd: HWND) {
    // Phase 1: ask the shell to do its half. We hold a brief
    // `&mut WindowState` borrow only for the duration of this
    // call — `close_active_tab` is pure data-model work plus an
    // unwatch, no plugin code runs inside it.
    let closed = if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        state.shell.close_active_tab()
    } else {
        return;
    };
    let Some(closed) = closed else {
        return; // nothing was open
    };

    // Defense in depth: a refactor that ever produced
    // `closed.scintilla_doc == closed.new_active_doc` (e.g. a
    // future "reload in place" path that reuses the existing
    // doc) would have us release the view's only ref to the
    // doc before the rebind — UAF. Catch it as an assert in
    // debug builds; release builds rely on the structural
    // guarantee that `SCI_CREATEDOCUMENT` returns unique pointers.
    if closed.scintilla_doc != 0 && closed.new_active_doc != 0 {
        debug_assert_ne!(
            closed.scintilla_doc, closed.new_active_doc,
            "closed and new-active doc pointers must be distinct"
        );
    }

    // Phase 2: platform cleanup. Re-acquire the borrow; no plugin
    // code runs in this phase either (TCM_*, SCI_* are all
    // synchronous and don't re-enter our wnd_proc).
    if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        // Remove the item from the tab control. wparam is the
        // index; lparam is unused for TCM_DELETEITEM.
        unsafe {
            SendMessageW(
                state.tab_hwnd,
                TCM_DELETEITEM,
                Some(WPARAM(closed.closed_idx)),
                Some(LPARAM(0)),
            );
        }
        state.synced_tab_count = state.synced_tab_count.saturating_sub(1);

        // Release the closed tab's Scintilla document. The view
        // is still bound to it at this point, so Scintilla's
        // implicit view-ownership keeps the buffer alive until
        // the rebind below; the SCI_RELEASEDOCUMENT here drops
        // our external reference. Skip when the doc was never
        // materialized (background-loaded tab closed before
        // first activation — no `SCI_CREATEDOCUMENT` was issued).
        if closed.scintilla_doc != 0 {
            state
                .editor
                .send(SCI_RELEASEDOCUMENT, 0, closed.scintilla_doc);
        }

        // Rebind the view to whatever's now active. Three sub-
        // cases:
        //
        //  1. New active tab has a materialized doc — straight
        //     SCI_SETDOCPOINTER. View releases the just-released
        //     `closed.scintilla_doc` (now its final release →
        //     buffer freed) and AddRefs the new doc.
        //  2. New active tab has *no* materialized doc (it was
        //     loaded in the background and never activated).
        //     **Must** lazy-create + populate the doc here, not
        //     defer to a future click: leaving the view bound to
        //     the just-released document would create a use-after-
        //     free window where any keystroke or paint touches a
        //     buffer whose external refcount is zero.
        //  3. No new active tab (closed the last open tab). Bind
        //     the view to a fresh empty placeholder doc so the user
        //     sees an empty editor — not the just-closed file's
        //     stale content. Without this, Scintilla's view-implicit
        //     ref keeps the closed doc alive and visible until the
        //     next open's `activate_tab` rebinds the view, which
        //     produces the visible "closed file's text remained on
        //     screen" bug.
        if state.shell.active_tab.is_none() {
            // Refcount lifecycle:
            //   CREATE         → placeholder.refcount = 1 (our ref)
            //   SETDOCPOINTER  → view AddRefs new + Releases whatever
            //                    was previously bound. If we held a
            //                    materialized closed.scintilla_doc it
            //                    was already explicitly RELEASEDOCUMENT'd
            //                    above (refcount 1 → view's implicit
            //                    ref); SETDOCPOINTER's view-Release
            //                    drops that to 0, freeing it.
            //                    placeholder.refcount becomes 2.
            //   RELEASEDOCUMENT → placeholder.refcount = 1, just the
            //                    view's implicit ref. The next
            //                    SETDOCPOINTER (from a future open's
            //                    activate_tab) drops that final ref
            //                    and the placeholder is freed cleanly.
            //
            // Guard against a NULL CREATEDOCUMENT return (Scintilla
            // returns 0 on allocation failure). Using 0 as the lparam
            // for RELEASEDOCUMENT would invoke release-of-null which
            // isn't part of the public ABI contract; guard the whole
            // block so on OOM we leave the view in whatever state
            // Scintilla's own fallback produces — same observable
            // behaviour as before this fix and not worse.
            let placeholder = state
                .editor
                .send(SCI_CREATEDOCUMENT, 0, SC_DOCUMENTOPTION_DEFAULT);
            if placeholder != 0 {
                state.editor.send(SCI_SETDOCPOINTER, 0, placeholder);
                state.editor.send(SCI_RELEASEDOCUMENT, 0, placeholder);
            }
            // Refresh chrome to the empty state — status bar should
            // reflect the no-buffer state. The title bar gets
            // refreshed unconditionally in Phase 3 below
            // (`update_window_title`), which already handles the
            // `active_tab.is_none()` case by rendering just "Code++".
            let mut win32_ui = Win32Ui {
                status_hwnd: state.status_hwnd,
                editor: state.editor,
            };
            <Win32Ui as UiPlatform>::update_status(
                &mut win32_ui,
                &Encoding::default(),
                Eol::default(),
                0,
            );
        }
        if let Some(active_idx) = state.shell.active_tab {
            if closed.new_active_doc != 0 {
                state
                    .editor
                    .send(SCI_SETDOCPOINTER, 0, closed.new_active_doc);
            } else if let Some(text) = state.shell.tabs.get(active_idx).map(|t| t.text.clone()) {
                // Sub-case 2: lazily materialize the doc from the
                // tab's stored text. `tabs.get(active_idx)` rather
                // than `[active_idx]` so a future refactor that
                // could put `active_tab` out of range fails as a
                // missed-rebind no-op rather than a panic across
                // the `extern "system"` wnd_proc frame. Same
                // pattern as `handle_tab_selchange`'s lazy-create
                // branch.
                let new_doc = state
                    .editor
                    .send(SCI_CREATEDOCUMENT, 0, SC_DOCUMENTOPTION_DEFAULT);
                state.editor.send(SCI_SETDOCPOINTER, 0, new_doc);
                let mut bytes = Vec::with_capacity(text.len() + 1);
                bytes.extend_from_slice(text.as_bytes());
                bytes.push(0);
                state.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
                state.editor.send(SCI_EMPTYUNDOBUFFER, 0, 0);
                state.editor.send(SCI_SETSAVEPOINT, 0, 0);
                if let Some(tab) = state.shell.tabs.get_mut(active_idx) {
                    tab.scintilla_doc = new_doc;
                }
            }

            // Sync the visual selection on the tab strip with
            // the new active index.
            unsafe {
                SendMessageW(
                    state.tab_hwnd,
                    TCM_SETCURSEL,
                    Some(WPARAM(active_idx)),
                    Some(LPARAM(0)),
                );
            }

            // Re-apply the new active tab's lexer/theme AND refresh
            // the status bar. Both fields live on the *view* (lexer
            // attachment) and the chrome (status bar text), neither
            // of which the rebind above touches — without these
            // calls the user sees the closed tab's colours and
            // status text after the close. The two snapshots happen
            // together so we hold the &Tab borrow once. Pulled out
            // of the borrow scope by Copy: status_hwnd is HWND and
            // editor is EditorHandle, both Copy; the Win32Ui
            // methods only touch self.{editor,status_hwnd}, so the
            // outer &mut state borrow stays sound across the calls.
            let snapshot = state
                .shell
                .tabs
                .get(active_idx)
                .map(|t| (t.lang, t.encoding.clone(), t.eol, t.byte_len));
            if let Some((lang, encoding, eol, byte_len)) = snapshot {
                let mut win32_ui = Win32Ui {
                    status_hwnd: state.status_hwnd,
                    editor: state.editor,
                };
                <Win32Ui as UiPlatform>::apply_lang(&mut win32_ui, lang);
                <Win32Ui as UiPlatform>::update_status(&mut win32_ui, &encoding, eol, byte_len);
            }
        }
    }

    // Phase 3: chrome refresh + notification delivery. No state
    // borrow held while plugin code runs.
    unsafe {
        if let Some(state) = state_from_hwnd(hwnd) {
            update_window_title(hwnd, &state.shell);
        }
        fire_queued_notifications(hwnd);
    }
}

/// Handle a tab-strip selection change (`TCN_SELCHANGE`). Reads the
/// freshly-selected index off the control, updates
/// `Shell.active_tab`, binds the Scintilla view to that tab's
/// document, and refreshes the status bar so encoding/EOL/size
/// reflect the new tab.
///
/// If the target tab has no document yet (`scintilla_doc == 0`),
/// it's a tab whose load completed while it wasn't active —
/// `apply_load_result` populated `tab.text` but skipped the
/// Scintilla document entirely. We materialize the document
/// lazily here: create one, push the tab's stored text, store the
/// pointer back on the tab. Subsequent switches short-circuit.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn handle_tab_selchange(hwnd: HWND) {
    let Some(state) = (unsafe { state_from_hwnd(hwnd) }) else {
        return;
    };
    // SAFETY: tab_hwnd is a live HWND created in `run`.
    let new_idx = unsafe { SendMessageW(state.tab_hwnd, TCM_GETCURSEL, None, None).0 as isize };
    if new_idx < 0 {
        return;
    }
    let new_idx = new_idx as usize;
    if new_idx >= state.shell.tabs.len() {
        // The control's selection is out of sync with the data
        // model — defensive only; should never happen because
        // sync_tab_strip keeps them in lockstep.
        return;
    }
    state.shell.active_tab = Some(new_idx);

    // Snapshot what we need from the tab, then drop the borrow
    // before reaching for `state.editor` (a Copy field) so we can
    // call into Scintilla without a live `&mut Tab` borrow.
    let (mut doc, text_to_populate, encoding, eol, byte_len, lang) = {
        let tab = &state.shell.tabs[new_idx];
        (
            tab.scintilla_doc,
            if tab.scintilla_doc == 0 {
                Some(tab.text.clone())
            } else {
                None
            },
            tab.encoding.clone(),
            tab.eol,
            tab.byte_len,
            tab.lang,
        )
    };

    // Lazily populate the doc on first activation of a background-
    // loaded tab. Create the doc, bind it, push the saved text.
    if let Some(text) = text_to_populate {
        doc = state
            .editor
            .send(SCI_CREATEDOCUMENT, 0, SC_DOCUMENTOPTION_DEFAULT);
        state.editor.send(SCI_SETDOCPOINTER, 0, doc);
        let mut bytes = Vec::with_capacity(text.len() + 1);
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        state.editor.send(SCI_SETTEXT, 0, bytes.as_ptr() as isize);
        state.editor.send(SCI_EMPTYUNDOBUFFER, 0, 0);
        state.editor.send(SCI_SETSAVEPOINT, 0, 0);
        state.shell.tabs[new_idx].scintilla_doc = doc;
    } else {
        // Doc already exists — just rebind the view.
        state.editor.send(SCI_SETDOCPOINTER, 0, doc);
    }

    // Refresh the status bar so encoding/EOL/size match the
    // newly-active tab. Without this, the user sees the old tab's
    // stats until the next `WM_APP_WAKE` drain (which has no
    // reason to fire on a click-only switch).
    let mut win32_ui = Win32Ui {
        status_hwnd: state.status_hwnd,
        editor: state.editor,
    };
    // Re-apply the new tab's lexer/theme. Each tab carries its own
    // LangType; without this call the previous tab's lexer stays
    // bound to the single Scintilla view and colours the new
    // buffer with the wrong rules (or, if the previous tab was
    // L_TEXT, leaves a coloured buffer un-styled).
    <Win32Ui as UiPlatform>::apply_lang(&mut win32_ui, lang);
    <Win32Ui as UiPlatform>::update_status(&mut win32_ui, &encoding, eol, byte_len);

    // Reflect the new active tab in the window title.
    // SAFETY: caller's UI-thread contract carries to update_window_title.
    unsafe {
        update_window_title(hwnd, &state.shell);
    }

    // Queue NPPN_BUFFERACTIVATED for plugins that track the
    // active buffer. The borrow on `state` is released by NLL at
    // its last use here (the queue method), before
    // `fire_queued_notifications` re-acquires a fresh borrow.
    state.shell.queue_buffer_activated();
    // SAFETY: caller's UI-thread contract carries.
    unsafe {
        fire_queued_notifications(hwnd);
    }
}

/// Sanitize a filename string for display in chrome (tab labels,
/// window titles): strip embedded NULs, CR/LF/TAB, and cap at
/// `TAB_LABEL_MAX_TCHARS - 1` UTF-16 code units. Without this:
///
///   - An embedded U+0000 (legal on some network filesystems,
///     trivially injectable via `NPPM_DOOPEN` from a plugin)
///     truncates `SetWindowTextW`/SB_SETTEXTW silently — the
///     chrome no longer reflects the real open file, confusing
///     users into acting on the wrong file.
///   - CR/LF/TAB render as glyph noise on tab strips and may
///     produce odd line wrapping in title bars.
///   - Multi-MB paths (legal on some filesystems) produce huge
///     temporary allocations on every chrome refresh.
fn sanitize_filename_for_display(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|&c| !matches!(c, '\0' | '\r' | '\n' | '\t'))
        .collect();
    // Cap by char count rather than code-unit count for simplicity;
    // the difference is small for chrome strings and stays safely
    // under the wide-buffer cap downstream.
    if cleaned.chars().count() > TAB_LABEL_MAX_TCHARS - 1 {
        cleaned.chars().take(TAB_LABEL_MAX_TCHARS - 1).collect()
    } else {
        cleaned
    }
}

/// Set the main window's title to reflect the currently-active tab:
/// `"<filename> - Code++"` when there's an active path,
/// `"Untitled - Code++"` when the active tab has no path yet,
/// `"Code++"` when no tab is open. Called whenever `Shell.active_tab`
/// changes (tab click, sync after open, plugin-driven switch).
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn update_window_title(hwnd: HWND, shell: &Shell) {
    let title = match shell.active() {
        Some(tab) => match tab.path.as_ref().and_then(|p| p.file_name()) {
            Some(name) => {
                let sanitized = sanitize_filename_for_display(&name.to_string_lossy());
                format!("{sanitized} - Code++")
            }
            None => "Untitled - Code++".to_string(),
        },
        None => "Code++".to_string(),
    };
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: hwnd is a valid main-window HWND owned by the caller's
    // thread; `wide` is a null-terminated UTF-16 buffer that lives
    // for the duration of the synchronous SetWindowTextW call.
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
    }
}

/// Bring the Win32 tab strip into sync with `state.shell.tabs`.
/// Inserts a tab item for each new `Shell.tabs[i]` past the count
/// the strip already knows about, using the file's basename (or
/// "Untitled" if the tab has no path yet) as the label. Then
/// snaps the tab control's selection to `state.shell.active_tab`
/// so a click elsewhere or a programmatic switch (NPPM_SWITCHTOFILE)
/// is reflected visually.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `state`'s HWNDs.
unsafe fn sync_tab_strip(state: &mut WindowState) {
    while state.synced_tab_count < state.shell.tabs.len() {
        let idx = state.synced_tab_count;
        let label = tab_label_for(&state.shell.tabs[idx]);
        // TCITEMW.pszText is `*mut u16`; the wide buffer must
        // outlive the SendMessage call. The `label_storage` vec
        // is the buffer, kept alive for the call duration.
        let mut label_storage = label;
        let mut item = TCITEMW {
            mask: windows::Win32::UI::Controls::TCITEMHEADERA_MASK(0x0001), // TCIF_TEXT
            pszText: windows::core::PWSTR(label_storage.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: tab_hwnd is a valid HWND (created in `run`); item
        // points to live wide-char storage that lives across the
        // SendMessage call.
        unsafe {
            SendMessageW(
                state.tab_hwnd,
                TCM_INSERTITEMW,
                Some(WPARAM(idx)),
                Some(LPARAM(&mut item as *mut TCITEMW as isize)),
            );
        }
        state.synced_tab_count += 1;
    }

    // Refresh the labels of tabs already on the strip. Without
    // this, a tab that was inserted while its `path` was still
    // None (synced as "Untitled" right after `open_file`) keeps
    // the placeholder label after `apply_load_result` populates
    // its real path: the insert-only loop above never revisits
    // existing items. TCM_SETITEMW with the same payload shape
    // updates a single TCITEM field in place.
    for idx in 0..state.synced_tab_count {
        // `.get(idx)` rather than `[idx]` so a future refactor
        // that ever lets `synced_tab_count` drift past
        // `shell.tabs.len()` degrades to a missing-update no-op
        // for that one tab rather than panicking across the
        // `extern "system"` wnd_proc frame.
        let Some(tab) = state.shell.tabs.get(idx) else {
            break;
        };
        let label = tab_label_for(tab);
        let mut label_storage = label;
        let mut item = TCITEMW {
            mask: windows::Win32::UI::Controls::TCITEMHEADERA_MASK(0x0001), // TCIF_TEXT
            pszText: windows::core::PWSTR(label_storage.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: `state.tab_hwnd` is a live HWND created in
        // `run`; `&mut item` and `label_storage` both stay live
        // for the duration of the synchronous SendMessageW call.
        // TCM_SETITEMW does not re-enter our wnd_proc.
        unsafe {
            SendMessageW(
                state.tab_hwnd,
                TCM_SETITEMW,
                Some(WPARAM(idx)),
                Some(LPARAM(&mut item as *mut TCITEMW as isize)),
            );
        }
    }

    // Reflect the active tab index on the visual control. Done
    // unconditionally so external switches (NPPM_SWITCHTOFILE,
    // future shortcut routing) keep the tab strip in lockstep.
    if let Some(active_idx) = state.shell.active_tab {
        // SAFETY: tab_hwnd is valid; the result is unused.
        unsafe {
            SendMessageW(
                state.tab_hwnd,
                TCM_SETCURSEL,
                Some(WPARAM(active_idx)),
                Some(LPARAM(0)),
            );
        }
    }
}

/// Build the wide-char tab label for `tab`: the file basename if
/// the tab has a path, else "Untitled". Trailing NUL is appended so
/// the buffer can be passed to `TCITEMW.pszText` directly.
///
/// The output is capped at `TAB_LABEL_MAX_TCHARS - 1` UTF-16 code
/// units (plus the trailing NUL). Without a cap, a path whose
/// `file_name()` is multiple-MB long (legal on some network
/// filesystems) would produce an equally long allocation and a
/// degenerate tab strip. Embedded control characters (`\n`,
/// `\r`, `\t`) are stripped — they're legal on some filesystems
/// but render as glyph noise on the tab strip.
const TAB_LABEL_MAX_TCHARS: usize = 260;

fn tab_label_for(tab: &Tab) -> Vec<u16> {
    let raw = tab
        .path
        .as_ref()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()))
        .unwrap_or("Untitled");
    let cleaned = sanitize_filename_for_display(raw);
    cleaned.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Bundle of HMENUs the wnd_proc and `WindowState` need to keep
/// addressable after `build_main_menu` returns. The bar itself is
/// what `CreateWindowExW` consumes; the four named popups need
/// dynamic refresh on `WM_INITMENUPOPUP` (state-driven check marks
/// or rebuilt-on-open lists). The plugin submenu is exposed
/// separately because the plugin host's `NPPM_GETMENUHANDLE` ABI
/// hands it back as a stable handle, distinct from any other host
/// menu.
struct BuiltMenuBar {
    bar: HMENU,
    plugin_menu: HMENU,
    view_menu: HMENU,
    encoding_menu: HMENU,
    language_menu: HMENU,
    window_menu: HMENU,
}

/// Construct the full N++-shaped main menu: File, Edit, Search,
/// View, Encoding, Language, Settings, Tools, Macro, Run, Plugins,
/// Window, ?. Items that already have a real handler attached
/// (Edit's clipboard ops, View's toggles, File's Save/Close/Exit)
/// are enabled; items whose feature lands later in Phase 4+ are
/// inserted as `MF_GRAYED` placeholders so the structure is
/// observable and the cmd ids are reserved.
///
/// # Safety
///
/// `CreateMenu` / `AppendMenuW` are pure heap operations; they
/// don't re-enter our wnd_proc. The returned handles are owned by
/// the host process for the window's lifetime.
fn build_main_menu() -> windows::core::Result<BuiltMenuBar> {
    unsafe {
        let bar = CreateMenu()?;

        // ----- File -----
        let file_menu = CreateMenu()?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_NEW as usize,
            w!("&New\tCtrl+N"),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_OPEN as usize,
            w!("&Open...\tCtrl+O"),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_RELOAD as usize,
            w!("&Reload from Disk"),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING,
            ID_FILE_SAVE as usize,
            w!("&Save\tCtrl+S"),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_SAVE_AS as usize,
            w!("Save &As..."),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_SAVE_ALL as usize,
            w!("Sav&e All"),
        )?;
        AppendMenuW(file_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            file_menu,
            MF_STRING,
            ID_FILE_CLOSE as usize,
            w!("&Close\tCtrl+W"),
        )?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_CLOSE_ALL as usize,
            w!("Close A&ll"),
        )?;
        AppendMenuW(file_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            file_menu,
            MF_STRING | MF_GRAYED,
            ID_FILE_PRINT as usize,
            w!("&Print...\tCtrl+P"),
        )?;
        AppendMenuW(file_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(file_menu, MF_STRING, ID_FILE_EXIT as usize, w!("E&xit"))?;
        AppendMenuW(bar, MF_POPUP, file_menu.0 as usize, w!("&File"))?;

        // ----- Edit ----- (Scintilla-backed; all enabled.)
        let edit_menu = CreateMenu()?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_UNDO as usize,
            w!("&Undo\tCtrl+Z"),
        )?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_REDO as usize,
            w!("&Redo\tCtrl+Y"),
        )?;
        AppendMenuW(edit_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_CUT as usize,
            w!("Cu&t\tCtrl+X"),
        )?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_COPY as usize,
            w!("&Copy\tCtrl+C"),
        )?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_PASTE as usize,
            w!("&Paste\tCtrl+V"),
        )?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_DELETE as usize,
            w!("&Delete\tDel"),
        )?;
        AppendMenuW(edit_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            edit_menu,
            MF_STRING,
            ID_EDIT_SELECTALL as usize,
            w!("Select &All\tCtrl+A"),
        )?;
        AppendMenuW(bar, MF_POPUP, edit_menu.0 as usize, w!("&Edit"))?;

        // ----- Search ----- (stubs — Phase 4 m3 wires Find/Replace.)
        let search_menu = CreateMenu()?;
        AppendMenuW(
            search_menu,
            MF_STRING | MF_GRAYED,
            ID_SEARCH_FIND as usize,
            w!("&Find...\tCtrl+F"),
        )?;
        AppendMenuW(
            search_menu,
            MF_STRING | MF_GRAYED,
            ID_SEARCH_FINDNEXT as usize,
            w!("Find &Next\tF3"),
        )?;
        AppendMenuW(
            search_menu,
            MF_STRING | MF_GRAYED,
            ID_SEARCH_FINDPREV as usize,
            w!("Find Pre&vious\tShift+F3"),
        )?;
        AppendMenuW(
            search_menu,
            MF_STRING | MF_GRAYED,
            ID_SEARCH_REPLACE as usize,
            w!("&Replace...\tCtrl+H"),
        )?;
        AppendMenuW(search_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            search_menu,
            MF_STRING | MF_GRAYED,
            ID_SEARCH_FINDINFILES as usize,
            w!("Find in Fi&les...\tCtrl+Shift+F"),
        )?;
        AppendMenuW(search_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        // Go to Line — wired in m3b1. The other Search items stay
        // MF_GRAYED until m3b2 lands the Find/Replace dialogs;
        // TranslateAcceleratorW silently discards a WM_COMMAND
        // whose target menu item is disabled, so an enabled item
        // is what makes Ctrl+G actually fire.
        AppendMenuW(
            search_menu,
            MF_STRING,
            ID_SEARCH_GOTOLINE as usize,
            w!("&Go to...\tCtrl+G"),
        )?;
        AppendMenuW(bar, MF_POPUP, search_menu.0 as usize, w!("&Search"))?;

        // ----- View ----- (toggles refreshed in WM_INITMENUPOPUP.)
        let view_menu = CreateMenu()?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_WORDWRAP as usize,
            w!("&Word Wrap"),
        )?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_SHOWWS as usize,
            w!("Show White&space"),
        )?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_SHOWEOL as usize,
            w!("Show End of &Line"),
        )?;
        AppendMenuW(view_menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_ZOOMIN as usize,
            w!("Zoom &In\tCtrl++"),
        )?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_ZOOMOUT as usize,
            w!("Zoom &Out\tCtrl+-"),
        )?;
        AppendMenuW(
            view_menu,
            MF_STRING,
            ID_VIEW_ZOOMRESET as usize,
            w!("&Restore Zoom\tCtrl+0"),
        )?;
        AppendMenuW(bar, MF_POPUP, view_menu.0 as usize, w!("&View"))?;

        // ----- Encoding ----- (radio refreshed in WM_INITMENUPOPUP;
        // conversions wired in m5, grayed for now.)
        let encoding_menu = CreateMenu()?;
        AppendMenuW(
            encoding_menu,
            MF_STRING | MF_GRAYED,
            ID_ENCODING_ANSI as usize,
            w!("&ANSI"),
        )?;
        AppendMenuW(
            encoding_menu,
            MF_STRING | MF_GRAYED,
            ID_ENCODING_UTF8 as usize,
            w!("UTF-&8 (no BOM)"),
        )?;
        AppendMenuW(
            encoding_menu,
            MF_STRING | MF_GRAYED,
            ID_ENCODING_UTF8_BOM as usize,
            w!("UTF-8 with &BOM"),
        )?;
        AppendMenuW(
            encoding_menu,
            MF_STRING | MF_GRAYED,
            ID_ENCODING_UTF16_LE as usize,
            w!("UTF-16 &LE BOM"),
        )?;
        AppendMenuW(
            encoding_menu,
            MF_STRING | MF_GRAYED,
            ID_ENCODING_UTF16_BE as usize,
            w!("UTF-16 B&E BOM"),
        )?;
        AppendMenuW(bar, MF_POPUP, encoding_menu.0 as usize, w!("E&ncoding"))?;

        // ----- Language ----- (one item per known LangType; cmd id
        // = ID_LANGUAGE_BASE + lang.0. Radio refreshed in
        // WM_INITMENUPOPUP based on the active tab's lang.)
        let language_menu = CreateMenu()?;
        for (lang, label) in [
            (codepp_core::lang::L_TEXT, w!("&Normal Text")),
            (codepp_core::lang::L_C, w!("&C")),
            (codepp_core::lang::L_CPP, w!("C&++")),
            (codepp_core::lang::L_RUST, w!("&Rust")),
        ] {
            AppendMenuW(
                language_menu,
                MF_STRING,
                (ID_LANGUAGE_BASE as i32 + lang.as_npp_id()) as usize,
                label,
            )?;
        }
        AppendMenuW(bar, MF_POPUP, language_menu.0 as usize, w!("&Language"))?;

        // ----- Settings ----- (stubs.)
        let settings_menu = CreateMenu()?;
        AppendMenuW(
            settings_menu,
            MF_STRING | MF_GRAYED,
            ID_SETTINGS_PREFERENCES as usize,
            w!("&Preferences..."),
        )?;
        AppendMenuW(bar, MF_POPUP, settings_menu.0 as usize, w!("Se&ttings"))?;

        // ----- Tools / Macro / Run ----- (placeholder submenus —
        // the menu *items* land alongside the features in later
        // phases; for now the top-level entries reserve the slots
        // and show as empty grayed popups.)
        let tools_menu = CreateMenu()?;
        AppendMenuW(
            tools_menu,
            MF_STRING | MF_GRAYED,
            0,
            w!("(no tools wired yet)"),
        )?;
        AppendMenuW(bar, MF_POPUP, tools_menu.0 as usize, w!("&Tools"))?;

        let macro_menu = CreateMenu()?;
        AppendMenuW(
            macro_menu,
            MF_STRING | MF_GRAYED,
            0,
            w!("(macro recording lands later)"),
        )?;
        AppendMenuW(bar, MF_POPUP, macro_menu.0 as usize, w!("&Macro"))?;

        let run_menu = CreateMenu()?;
        AppendMenuW(
            run_menu,
            MF_STRING | MF_GRAYED,
            0,
            w!("(run-command lands later)"),
        )?;
        AppendMenuW(bar, MF_POPUP, run_menu.0 as usize, w!("R&un"))?;

        // ----- Plugins ----- (populated lazily by `populate_plugin_menu`
        // on first WM_INITMENUPOPUP; the HMENU is alive from now on
        // so plugins that query NPPM_GETMENUHANDLE before the popup
        // get a real handle.)
        let plugin_menu = CreateMenu()?;
        AppendMenuW(bar, MF_POPUP, plugin_menu.0 as usize, w!("&Plugins"))?;

        // ----- Window ----- (rebuilt every WM_INITMENUPOPUP from
        // Shell.tabs; cmd id = ID_WINDOW_BASE + tab idx.)
        let window_menu = CreateMenu()?;
        AppendMenuW(
            window_menu,
            MF_STRING | MF_GRAYED,
            0,
            w!("(no open windows)"),
        )?;
        AppendMenuW(bar, MF_POPUP, window_menu.0 as usize, w!("&Window"))?;

        // ----- ? (Help) -----
        let help_menu = CreateMenu()?;
        AppendMenuW(
            help_menu,
            MF_STRING,
            ID_HELP_ABOUT as usize,
            w!("&About Code++"),
        )?;
        AppendMenuW(bar, MF_POPUP, help_menu.0 as usize, w!("&?"))?;

        Ok(BuiltMenuBar {
            bar,
            plugin_menu,
            view_menu,
            encoding_menu,
            language_menu,
            window_menu,
        })
    }
}

/// Refresh check marks on the View menu so they reflect the live
/// Scintilla state (`SCI_GETWRAPMODE`, `SCI_GETVIEWWS`,
/// `SCI_GETVIEWEOL`). Cheap to call on every `WM_INITMENUPOPUP` —
/// three direct-call queries plus three `CheckMenuItem` calls.
///
/// # Safety
///
/// `view_menu` must be a live HMENU (the one built by
/// `build_main_menu`); caller must run on the UI thread.
unsafe fn refresh_view_menu(view_menu: HMENU, editor: &EditorHandle) {
    let wrap_on = editor.send(SCI_GETWRAPMODE, 0, 0) != 0;
    let ws_on = editor.send(SCI_GETVIEWWS, 0, 0) != 0;
    let eol_on = editor.send(SCI_GETVIEWEOL, 0, 0) != 0;
    let mark = |id: u16, on: bool| {
        let flags = MF_BYCOMMAND.0 | if on { MF_CHECKED.0 } else { MF_UNCHECKED.0 };
        // CheckMenuItem returns the previous state on success or
        // -1u32 on failure; we treat both as fire-and-forget — a
        // failure here just means the check mark is wrong on this
        // open, never a process-stability issue.
        unsafe {
            let _ = CheckMenuItem(view_menu, id as u32, flags);
        }
    };
    mark(ID_VIEW_WORDWRAP, wrap_on);
    mark(ID_VIEW_SHOWWS, ws_on);
    mark(ID_VIEW_SHOWEOL, eol_on);
}

/// Radio-mark the menu item matching `lang` in the Language menu.
/// `CheckMenuRadioItem` ensures only one item in the range carries
/// the radio glyph at a time. Phase 4 m1 wires four langs (Normal
/// Text, C, C++, Rust); cmd ids run from `ID_LANGUAGE_BASE` upward.
///
/// # Safety
///
/// `language_menu` must be a live HMENU; caller on the UI thread.
unsafe fn refresh_language_menu(language_menu: HMENU, lang: LangType) {
    // CheckMenuRadioItem(menu, first_id, last_id, check_id, MF_BYCOMMAND).
    // The range is closed [first, last]; we use the full reserved
    // band so future `Lang` additions don't need an off-by-one
    // here. `check_id` outside the range is a no-op which keeps an
    // unrecognised lang from clearing all marks.
    //
    // `lang.as_npp_id()` is whatever i32 the plugin ABI handed us,
    // including i32::MAX from a hostile NPPM_SETBUFFERLANGTYPE.
    // Bound the value before the addition to avoid overflow on the
    // intermediate i32 stage and to keep the resulting target id
    // inside the cmd-id range Win32 will accept.
    let lang_id = lang.as_npp_id();
    if lang_id < 0 || lang_id as usize >= ID_LANGUAGE_CAP {
        return;
    }
    let target = ID_LANGUAGE_BASE as u32 + lang_id as u32;
    unsafe {
        let _ = CheckMenuRadioItem(
            language_menu,
            ID_LANGUAGE_BASE as u32,
            ID_LANGUAGE_END as u32,
            target,
            MF_BYCOMMAND.0,
        );
    }
}

/// Radio-mark the Encoding menu's active item. Encoding values
/// don't yet have a numeric ABI to share with plugins, so the
/// match is on a small inline table — extended alongside the
/// `core::encoding` set as new variants land.
///
/// # Safety
///
/// `encoding_menu` must be a live HMENU; caller on the UI thread.
unsafe fn refresh_encoding_menu(encoding_menu: HMENU, encoding: &Encoding) {
    // The Phase 3 status-bar label is the most stable identity we
    // have for an `Encoding`; matching on it avoids exporting more
    // surface from `core::encoding` than the menu actually needs.
    // Anything not in the table leaves no radio mark — a
    // user-visible "no encoding selected" cue that an unfamiliar
    // encoding is current.
    let target = match encoding.label() {
        "ANSI" => Some(ID_ENCODING_ANSI),
        "UTF-8" => Some(ID_ENCODING_UTF8),
        "UTF-8 BOM" => Some(ID_ENCODING_UTF8_BOM),
        "UTF-16 LE" => Some(ID_ENCODING_UTF16_LE),
        "UTF-16 BE" => Some(ID_ENCODING_UTF16_BE),
        _ => None,
    };
    if let Some(id) = target {
        unsafe {
            let _ = CheckMenuRadioItem(
                encoding_menu,
                ID_ENCODING_ANSI as u32,
                ID_ENCODING_UTF16_BE as u32,
                id as u32,
                MF_BYCOMMAND.0,
            );
        }
    }
}

/// Rebuild the Window menu from the current tab list. Each tab gets
/// one entry (`<idx>: <filename>`); the active tab is check-marked.
/// Cmd id = `ID_WINDOW_BASE + idx`.
///
/// The submenu is fully torn down and rebuilt on every popup — the
/// tab set is small (≤ 100 capped by the cmd-id range), so a per-
/// open rebuild is far simpler than tracking deltas.
///
/// # Safety
///
/// `window_menu` must be a live HMENU; caller on the UI thread.
unsafe fn refresh_window_menu(window_menu: HMENU, shell: &Shell) {
    // Clear by deleting every existing item by position. Iterate
    // backward so deletion of item N doesn't shift item N+1 into
    // the just-cleared slot.
    let count = unsafe { GetMenuItemCount(Some(window_menu)) };
    if count > 0 {
        for i in (0..count).rev() {
            unsafe {
                let _ = DeleteMenu(window_menu, i as u32, MF_BYPOSITION);
            }
        }
    }

    if shell.tabs.is_empty() {
        unsafe {
            let _ = AppendMenuW(
                window_menu,
                MF_STRING | MF_GRAYED,
                0,
                w!("(no open windows)"),
            );
        }
        return;
    }

    for (idx, tab) in shell.tabs.iter().enumerate() {
        if idx >= ID_WINDOW_CAP {
            // Cap the visible list at the cmd-id window. The
            // remainder is reachable through tab-strip clicks; the
            // menu just shows the first ID_WINDOW_CAP entries.
            break;
        }
        // Same filename-extraction rule as the tab strip: prefer
        // `path.file_name()`, fall back to "Untitled" for buffers
        // that haven't been saved yet, sanitize for control chars.
        let raw = tab
            .path
            .as_ref()
            .and_then(|p| p.file_name().and_then(|s| s.to_str()))
            .unwrap_or("Untitled");
        let cleaned = sanitize_filename_for_display(raw);
        // Prefix with "<idx+1> " for keyboard-friendly mnemonic
        // (matches N++ which uses 1-based numbering in this menu).
        let display = format!("{} {}", idx + 1, cleaned);
        let display_w: Vec<u16> = display.encode_utf16().chain(std::iter::once(0)).collect();
        let id = (ID_WINDOW_BASE as usize) + idx;
        unsafe {
            let _ = AppendMenuW(window_menu, MF_STRING, id, PCWSTR(display_w.as_ptr()));
        }
    }

    if let Some(active) = shell.active_tab {
        // Defense in depth: only mark the active tab if it's in the
        // visible-cmd-id window. A future `active_tab` outside the
        // 0..ID_WINDOW_CAP range would silently overflow the u32
        // addition; bounded check keeps the target id well-formed.
        if active < ID_WINDOW_CAP {
            let id = ID_WINDOW_BASE as u32 + active as u32;
            unsafe {
                let _ = CheckMenuItem(window_menu, id, MF_BYCOMMAND.0 | MF_CHECKED.0);
            }
        }
    }
}

/// Append loaded-plugin FuncItems onto the per-plugin submenu after
/// a successful lazy-load round. Each plugin gets its own popup
/// submenu under the top-level "Plugins" entry, with the plugin's
/// own getName output as the label.
///
/// # Safety
///
/// Caller must invoke this on the UI thread that owns `plugin_menu`.
/// `CreateMenu`/`AppendMenuW` do not re-enter our wnd_proc.
unsafe fn populate_plugin_menu(plugin_menu: HMENU, shell: &Shell) {
    for (plugin_name, funcs) in shell.loaded_plugin_funcs() {
        // One popup submenu per plugin so users see "Plugins → MyPlugin
        // → Item". Matches Notepad++'s layout.
        // SAFETY: CreateMenu just allocates a new HMENU; no aliasing
        // concerns.
        let submenu = match unsafe { CreateMenu() } {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(plugin = %plugin_name, error = %e, "CreateMenu failed");
                continue;
            }
        };
        for func in funcs {
            // N++ FuncItem ABI convention: a NULL `_pFunc` marks a menu
            // separator. The item_name for such an entry is a
            // placeholder the plugin does not expect to see rendered
            // (mimeTools, for example, writes a sentinel string there);
            // dispatching MF_STRING with that label is the visible bug.
            if func.p_func.is_none() {
                if let Err(e) = unsafe { AppendMenuW(submenu, MF_SEPARATOR, 0, PCWSTR::null()) } {
                    tracing::warn!(plugin = %plugin_name, error = %e, "AppendMenuW (separator) failed");
                }
                continue;
            }
            // FuncItem.item_name is a fixed-length null-terminated UTF-16
            // array; pass its pointer directly to AppendMenuW.
            let label = PCWSTR(func.item_name.as_ptr());
            // `cmd_id` is i32 (signed) but AppendMenuW expects usize.
            // Plugin cmd_ids are always positive (assigned from
            // PLUGIN_CMD_ID_BASE = 50000), so the cast is value-preserving.
            let id = func.cmd_id as usize;
            // SAFETY: `submenu` is the HMENU we just created; `label`
            // points into a static null-terminated wide string the
            // plugin owns for its lifetime.
            if let Err(e) = unsafe { AppendMenuW(submenu, MF_STRING, id, label) } {
                tracing::warn!(plugin = %plugin_name, error = %e, "AppendMenuW (item) failed");
            }
        }
        // Attach the submenu to the parent "Plugins" popup.
        let plugin_label_w: Vec<u16> = plugin_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `plugin_menu` is the parent HMENU passed in;
        // `plugin_label_w` is a local wide string that lives for the
        // duration of this call (AppendMenuW copies the label).
        if let Err(e) = unsafe {
            AppendMenuW(
                plugin_menu,
                MF_POPUP,
                submenu.0 as usize,
                PCWSTR(plugin_label_w.as_ptr()),
            )
        } {
            tracing::warn!(plugin = %plugin_name, error = %e, "AppendMenuW (popup) failed");
        }
    }
}

/// Write `text` into status-bar part `part_index`. Centralizes the
/// SB_SETTEXTW idiom so the regular status updates (encoding/EOL/size)
/// and the plugin-driven `NPPM_SETSTATUSBAR` overrides share one
/// implementation — the only thing that varies is the source string.
fn write_status_part(status_hwnd: HWND, part_index: usize, text: &str) {
    // Strip embedded NUL characters before building the wide buffer.
    // SB_SETTEXTW reads up to the first U+0000 unit; an embedded NUL
    // in plugin-supplied text would silently truncate the visible
    // string mid-glyph. Stripping puts the visible/encoded length in
    // sync so any future multi-part status logic that compares
    // `vec.len()` with what the control consumed stays consistent.
    let cleaned: String;
    let payload = if text.contains('\0') {
        cleaned = text.replace('\0', "");
        cleaned.as_str()
    } else {
        text
    };
    // Null-terminated UTF-16 buffer — Vec<u16> over HSTRING so the
    // layout is unambiguous; HSTRING has its own refcounted header.
    let wide: Vec<u16> = payload.encode_utf16().chain(std::iter::once(0)).collect();
    // SB_SETTEXTW = 0x040B (wide variant; 0x0401 is the ANSI
    // SB_SETTEXTA, which the SendMessageW call would mismatch).
    // wparam packs `(part_index | drawing_type << 8)`; drawing type 0
    // gives the default sunken inner edge.
    const SB_SETTEXTW: u32 = 0x040B;
    unsafe {
        SendMessageW(
            status_hwnd,
            SB_SETTEXTW,
            Some(WPARAM(part_index)),
            Some(LPARAM(wide.as_ptr() as isize)),
        );
    }
}

/// Show a "file changed externally — reload?" dialog. Standalone so
/// no `&mut WindowState` borrow is held while the modal pump runs.
fn show_reload_dialog(main_hwnd: HWND, path: &Path) -> bool {
    let prompt = HSTRING::from(format!(
        "{}\n\nThis file changed on disk. Reload from disk and discard any unsaved edits?",
        path.display()
    ));
    let title = w!("Code++: file changed externally");
    let response =
        unsafe { MessageBoxW(Some(main_hwnd), &prompt, title, MB_YESNO | MB_ICONQUESTION) };
    response == IDYES
}

/// Show a non-fatal error dialog. Standalone for the same reason as
/// `show_reload_dialog`.
fn show_error_dialog(main_hwnd: HWND, title: &str, message: &str) {
    let title_w = HSTRING::from(title);
    let msg_w = HSTRING::from(message);
    unsafe {
        MessageBoxW(Some(main_hwnd), &msg_w, &title_w, MB_OK | MB_ICONWARNING);
    }
}

/// Show the "About Code++" dialog. Modal MessageBox, so the borrow
/// must already be dropped at the call site (the WM_COMMAND handler
/// calls this through `state_from_hwnd` already, but pulls only
/// HWND values out before showing).
fn show_about_dialog(main_hwnd: HWND) {
    let body = format!(
        "Code++ {}\nA cross-platform Notepad++ clone in Rust on Scintilla.\n\nhttps://git.fiedler.live/tux/code-plus-plus",
        env!("CARGO_PKG_VERSION"),
    );
    let body_w = HSTRING::from(body);
    let title = w!("About Code++");
    unsafe {
        MessageBoxW(Some(main_hwnd), &body_w, title, MB_OK | MB_ICONINFORMATION);
    }
}

// --- "Go to..." modal dialog -----------------------------------------
//
// A small `WS_POPUP` + `WS_CAPTION` + `WS_SYSMENU` window with a
// Line/Offset radio toggle, three labeled boxes ("you are here / want
// to go to / can't go further than"), and OK/Cancel buttons. The
// editable middle box is the only place the user types; the other
// two are read-only edits whose values track the radio state.
//
// The pump is a nested `GetMessageW` loop that runs until the dialog
// HWND is destroyed (via OK/Cancel/X-button); `IsDialogMessageW` in
// the loop handles Tab/Enter/Esc/mnemonic semantics for free because
// the dialog has `WS_EX_CONTROLPARENT` and the controls have
// `WS_TABSTOP`.
//
// While the modal is up the owner is `EnableWindow(false)`'d so the
// user can't reach the main window's menu/accelerators; on destroy
// we re-enable (RAII) and SetFocus back to Scintilla.
//
// Result is plumbed back via a heap-allocated `GotoDialogState`
// whose pointer is stashed in the dialog HWND's `GWLP_USERDATA`.
// The outer caller reads `state.result` after the pump exits.

/// `EM_SETSEL` — declared inline because windows-rs splits its
/// edit-control message constants across modules and reaching one
/// just for this single use is more import noise than the literal.
const EM_SETSEL: u32 = 0x00B1;

/// Which axis the dialog's "want to go to" value applies to.
/// Toggled by the radio pair.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GotoMode {
    Line,
    Offset,
}

/// What the user picked. The caller decodes this and dispatches to
/// `SCI_GOTOLINE` or `SCI_GOTOPOS`.
enum GotoTarget {
    /// 1-based line number.
    Line(u32),
    /// 0-based byte offset.
    Offset(u32),
}

/// Heap-allocated dialog state. The wnd_proc reads/writes through
/// `GWLP_USERDATA`; the outer `show_goto_dialog` owns the
/// `Box<GotoDialogState>` and reads `result` after the modal pump
/// exits (the dialog window is already destroyed at that point so
/// no wnd_proc can race the read).
struct GotoDialogState {
    /// `Some(target)` iff the user clicked OK with valid input.
    /// Stays `None` on Cancel/Esc/X-button close.
    result: Option<GotoTarget>,
    /// Active mode. Mutated by the radio click handler.
    mode: GotoMode,
    /// 1-based current line in the active editor.
    current_line: u32,
    /// 1-based total line count.
    max_line: u32,
    /// 0-based current byte offset of the caret.
    current_offset: u32,
    /// 0-based document length in bytes.
    max_offset: u32,
    /// Control HWNDs, set by `show_goto_dialog` after the children
    /// are created; the wnd_proc reads them on radio click and
    /// IDOK to update the readonly boxes and parse the user's
    /// input.
    here_hwnd: HWND,
    target_hwnd: HWND,
    max_hwnd: HWND,
    /// Set to `true` once `show_goto_dialog` has populated the
    /// three control HWNDs above. The wnd_proc gates on this so a
    /// `WM_COMMAND` delivered between `WM_NCCREATE` and the end of
    /// child setup (e.g. via `SendMessage` from another thread, or
    /// a plugin synthesizing input) doesn't dereference null HWNDs.
    controls_ready: bool,
}

impl GotoDialogState {
    /// Current-axis "you are here" value.
    fn here_value(&self) -> u32 {
        match self.mode {
            GotoMode::Line => self.current_line,
            GotoMode::Offset => self.current_offset,
        }
    }
    /// Current-axis upper bound. `max_line` is clamped to at least 1
    /// so an empty buffer still presents a useful range.
    fn max_value(&self) -> u32 {
        match self.mode {
            GotoMode::Line => self.max_line.max(1),
            GotoMode::Offset => self.max_offset,
        }
    }
}

extern "system" fn goto_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The whole body runs under `catch_unwind` so a panic from
    // String::from_utf16_lossy / SetFocus / SendMessageW /
    // SetWindowTextW cannot unwind across this `extern "system"`
    // frame (UB at the FFI boundary). On a panic we fall back to
    // DefWindowProcW which is what every other branch already does
    // for unhandled msgs.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                if !cs.is_null() {
                    let state_ptr = (*cs).lpCreateParams as isize;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_COMMAND => {
                let cmd = (wparam.0 & 0xFFFF) as i32;
                let notif = ((wparam.0 >> 16) & 0xFFFF) as u32;
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut GotoDialogState;
                // controls_ready guard: a stray WM_COMMAND between
                // WM_NCCREATE and child-setup completion would
                // otherwise dereference null HWNDs (set/focus on
                // null is silent but SetFocus(null) clears
                // foreground focus, an observable misbehaviour).
                let state = if !state_ptr.is_null() && (*state_ptr).controls_ready {
                    Some(&mut *state_ptr)
                } else {
                    None
                };
                if cmd == IDOK.0 {
                    if let Some(state) = state {
                        if let Some(n) = read_target_value(state.target_hwnd, state.max_value()) {
                            let target = match state.mode {
                                // Clamp Line to >= 1 so a stray "0"
                                // doesn't fall off the bottom of the
                                // 1-based axis.
                                GotoMode::Line => GotoTarget::Line(n.max(1)),
                                GotoMode::Offset => GotoTarget::Offset(n),
                            };
                            state.result = Some(target);
                            let _ = DestroyWindow(hwnd);
                        } else {
                            // Empty / unparseable input: leave the
                            // dialog open with the target field
                            // re-focused and selected so a retry is
                            // a single keystroke.
                            let _ = SetFocus(Some(state.target_hwnd));
                            SendMessageW(
                                state.target_hwnd,
                                EM_SETSEL,
                                Some(WPARAM(0)),
                                Some(LPARAM(-1)),
                            );
                        }
                    }
                    LRESULT(0)
                } else if cmd == IDCANCEL.0 {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                } else if (cmd == IDC_GOTO_RADIO_LINE as i32 || cmd == IDC_GOTO_RADIO_OFFSET as i32)
                    && notif == BN_CLICKED
                {
                    if let Some(state) = state {
                        let new_mode = if cmd == IDC_GOTO_RADIO_LINE as i32 {
                            GotoMode::Line
                        } else {
                            GotoMode::Offset
                        };
                        if state.mode != new_mode {
                            state.mode = new_mode;
                            populate_axis_boxes(state);
                            let _ = SetFocus(Some(state.target_hwnd));
                            SendMessageW(
                                state.target_hwnd,
                                EM_SETSEL,
                                Some(WPARAM(0)),
                                Some(LPARAM(-1)),
                            );
                        }
                    }
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Themed STATIC and EDIT controls paint their own
            // background which doesn't match the dialog's hbrBackground
            // (`COLOR_WINDOW`) — they show as a slightly darker grey
            // rectangle behind every label, the readonly value
            // STATICs, and the editable target. Returning the
            // `COLOR_WINDOW` brush here, plus setting the DC bk mode
            // to `TRANSPARENT` so glyphs don't paint their own
            // background rectangle either, makes everything render
            // against the dialog's actual background colour.
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let hdc = HDC(wparam.0 as *mut c_void);
                let _ = SetBkMode(hdc, TRANSPARENT);
                let brush = GetSysColorBrush(COLOR_WINDOW);
                LRESULT(brush.0 as isize)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }));
    match result {
        Ok(lr) => lr,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Populate the three labeled boxes for the current `state.mode`.
/// Called on initial display and on every radio toggle so the
/// "You are here / want to go to / can't go further than" values
/// reflect the active axis. Pure text update — focus and selection
/// are handled by the caller so this can run safely before
/// `ShowWindow`.
unsafe fn populate_axis_boxes(state: &GotoDialogState) {
    unsafe {
        let here = HSTRING::from(state.here_value().to_string());
        let max = HSTRING::from(state.max_value().to_string());
        let _ = SetWindowTextW(state.here_hwnd, &here);
        let _ = SetWindowTextW(state.max_hwnd, &max);
        // Seed the editable target with the current value so the
        // user has a concrete starting point in the active axis.
        let _ = SetWindowTextW(state.target_hwnd, &here);
    }
}

/// Read the editable target box and parse it as a `u32`. Returns
/// `None` only on empty/unparseable input; values above `max` are
/// clamped so the user always lands inside the document. Note that
/// 0 is a valid offset (start of file); the line-vs-offset floor
/// gating happens in the IDOK handler.
unsafe fn read_target_value(edit: HWND, max: u32) -> Option<u32> {
    let mut buf = [0u16; 32];
    let len = unsafe { GetWindowTextW(edit, &mut buf) };
    if len <= 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..len as usize]);
    let n = text.trim().parse::<u32>().ok()?;
    Some(n.min(max))
}

/// Apply the system default GUI font to a freshly-created child
/// control. Without this Win32 falls back to the bitmap "System"
/// font from the Win95 era, which looks broken on every modern DPI.
unsafe fn apply_dialog_font(child: HWND, font: HFONT) {
    unsafe {
        SendMessageW(
            child,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// Show the modal "Go to..." dialog and return the user's choice,
/// or `None` on Cancel. `current_line` / `max_line` are 1-based;
/// `current_offset` / `max_offset` are 0-based byte counts.
///
/// Must be called from the UI thread that owns `owner`.
fn show_goto_dialog(
    owner: HWND,
    current_line: u32,
    max_line: u32,
    current_offset: u32,
    max_offset: u32,
) -> Option<GotoTarget> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<()> = OnceLock::new();

    unsafe {
        let instance = GetModuleHandleW(None).ok()?;

        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(goto_wnd_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void),
                lpszClassName: GOTO_CLASS,
                ..Default::default()
            };
            let _ = RegisterClassExW(&class);
        });

        // Heap-allocate the state so the wnd_proc can mutate it
        // and we can read `result` after DestroyWindow. The raw
        // pointer below remains valid for the lifetime of `state`
        // because the local binding is never moved (`state` is
        // the sole owner; the `Box` stays in this stack frame
        // until the function returns).
        let mut state = Box::new(GotoDialogState {
            result: None,
            mode: GotoMode::Line,
            current_line,
            max_line,
            current_offset,
            max_offset,
            here_hwnd: HWND::default(),
            target_hwnd: HWND::default(),
            max_hwnd: HWND::default(),
            controls_ready: false,
        });
        let state_ptr: *mut GotoDialogState = &mut *state;

        // Center on the owner. GetWindowRect returns screen coords,
        // which is what CreateWindowExW for a top-level WS_POPUP
        // wants.
        const DLG_W: i32 = 480;
        const DLG_H: i32 = 195;
        let mut owner_rect = RECT::default();
        let _ = GetWindowRect(owner, &mut owner_rect);
        let owner_w = owner_rect.right - owner_rect.left;
        let owner_h = owner_rect.bottom - owner_rect.top;
        let dlg_x = owner_rect.left + (owner_w - DLG_W) / 2;
        let dlg_y = owner_rect.top + (owner_h - DLG_H) / 2;

        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            GOTO_CLASS,
            w!("Go To..."),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            dlg_x,
            dlg_y,
            DLG_W,
            DLG_H,
            Some(owner),
            None,
            Some(instance.into()),
            Some(state_ptr as *mut c_void),
        )
        .ok()?;
        let _dlg_guard = DlgDestroyGuard(dlg);

        // Layout (client-area pixels). Three rows of [label] [box]
        // [button], with radios on top. The boxes sit close to the
        // labels — left edge roughly under the Offset radio's text
        // — and the buttons are right-anchored independently so
        // moving the box X doesn't drag them left.
        const X_PAD: i32 = 14;
        const LABEL_X: i32 = X_PAD;
        const LABEL_W: i32 = 155;
        const BOX_X: i32 = 175;
        const BOX_W: i32 = 90;
        const BTN_W: i32 = 160;
        const BTN_X: i32 = DLG_W - X_PAD - BTN_W;
        const BOX_H: i32 = 22;
        const LABEL_H: i32 = 20;
        const BTN_H: i32 = 26;
        const RADIO_Y: i32 = 14;
        const ROW1_Y: i32 = 50;
        const ROW2_Y: i32 = 84;
        const ROW3_Y: i32 = 118;

        // Radio pair. WS_GROUP on the first scopes the auto-radio
        // group; the second is in the same group so picking one
        // unchecks the other automatically.
        let radio_line = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("&Line"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_GROUP | style_bits(BS_AUTORADIOBUTTON),
            X_PAD,
            RADIO_Y,
            90,
            BOX_H,
            Some(dlg),
            Some(HMENU(IDC_GOTO_RADIO_LINE as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;
        let radio_offset = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("&Offset"),
            WS_CHILD | WS_VISIBLE | style_bits(BS_AUTORADIOBUTTON),
            X_PAD + 140,
            RADIO_Y,
            90,
            BOX_H,
            Some(dlg),
            Some(HMENU(IDC_GOTO_RADIO_OFFSET as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;

        // Row 1: "You are here:" + readonly box. No button.
        let label_here = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("You are here:"),
            WS_CHILD | WS_VISIBLE,
            LABEL_X,
            ROW1_Y + 2,
            LABEL_W,
            LABEL_H,
            Some(dlg),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        // Readonly value: STATIC, not EDIT. STATICs render with the
        // dialog's own background brush, so the greyish-edit-field
        // look that ES_READONLY produces is gone. SetWindowTextW
        // still drives the displayed value.
        let here = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            BOX_X,
            ROW1_Y + 2,
            BOX_W,
            LABEL_H,
            Some(dlg),
            Some(HMENU(IDC_GOTO_HERE as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;

        // Row 2: "You want to go to:" + editable box + "Go" button
        // (the dialog's IDOK).
        let label_target = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("You want to go to:"),
            WS_CHILD | WS_VISIBLE,
            LABEL_X,
            ROW2_Y + 2,
            LABEL_W,
            LABEL_H,
            Some(dlg),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let target = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | style_bits(ES_NUMBER | ES_AUTOHSCROLL),
            BOX_X,
            ROW2_Y,
            BOX_W,
            BOX_H,
            Some(dlg),
            Some(HMENU(IDC_GOTO_TARGET as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;
        // Buttons are 4px taller than the boxes they sit beside,
        // so subtract `(BTN_H - BOX_H) / 2 = 2` from the row Y to
        // center the button vertically against its companion box.
        let ok_btn = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("&Go"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | style_bits(BS_DEFPUSHBUTTON),
            BTN_X,
            ROW2_Y - 2,
            BTN_W,
            BTN_H,
            Some(dlg),
            Some(HMENU(IDOK.0 as u16 as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;

        // Row 3: "You can't go further than:" + readonly box +
        // "I'm going nowhere" button (the dialog's IDCANCEL).
        let label_max = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("You can't go further than:"),
            WS_CHILD | WS_VISIBLE,
            LABEL_X,
            ROW3_Y + 2,
            LABEL_W,
            LABEL_H,
            Some(dlg),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let max_box = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            BOX_X,
            ROW3_Y + 2,
            BOX_W,
            LABEL_H,
            Some(dlg),
            Some(HMENU(IDC_GOTO_MAX as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;
        let cancel = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("I'm going &nowhere"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | style_bits(BS_PUSHBUTTON),
            BTN_X,
            ROW3_Y - 2,
            BTN_W,
            BTN_H,
            Some(dlg),
            Some(HMENU(IDCANCEL.0 as u16 as usize as *mut c_void)),
            Some(instance.into()),
            None,
        )
        .ok()?;

        state.here_hwnd = here;
        state.target_hwnd = target;
        state.max_hwnd = max_box;
        // The wnd_proc gates WM_COMMAND on this flag — flip it
        // only after the three HWNDs above are populated so a
        // stray pre-show message can't dereference null handles.
        state.controls_ready = true;

        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        for child in [
            radio_line,
            radio_offset,
            label_here,
            here,
            label_target,
            target,
            label_max,
            max_box,
            ok_btn,
            cancel,
        ] {
            apply_dialog_font(child, font);
        }

        // Initial mode = Line; check the corresponding radio and
        // populate the three boxes BEFORE the dialog is shown so
        // the first frame paints the correct values.
        SendMessageW(
            radio_line,
            BM_SETCHECK,
            Some(WPARAM(BST_CHECKED.0 as usize)),
            Some(LPARAM(0)),
        );
        populate_axis_boxes(&state);

        // Disable owner FIRST, then reveal the dialog and move
        // focus. Doing it in this order means the moment the
        // owner could see "I just lost focus" is also the moment
        // it's disabled, eliminating the brief window where input
        // could still reach the main window.
        let _ = EnableWindow(owner, false);
        let _owner_guard = OwnerEnableGuard(owner);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(target));
        SendMessageW(target, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));

        let mut msg = MSG::default();
        loop {
            if !IsWindow(Some(dlg)).as_bool() {
                break;
            }
            let ret = GetMessageW(&mut msg, None, 0, 0);
            match ret.0 {
                0 => {
                    let _ = PostMessageW(None, WM_QUIT, msg.wParam, msg.lParam);
                    break;
                }
                -1 => break,
                _ => {
                    if !IsDialogMessageW(dlg, &msg).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }

        state.result.take()
    }
}

/// Lift a control-style bitmask (ES_*/BS_*) into the `WINDOW_STYLE`
/// newtype that windows-rs models WS_* flags with. The two are
/// bitwise-compatible by Win32 design, but Rust requires the type
/// match for `|` against `WS_CHILD` etc.
const fn style_bits(bits: i32) -> WINDOW_STYLE {
    WINDOW_STYLE(bits as u32)
}

/// RAII guard that re-enables `owner` on drop. `show_goto_dialog`
/// disables the owner before the modal pump and relies on the guard
/// to re-enable it on every exit path — including a panic between
/// disable and the pump's natural exit. Without the guard a panic
/// there would soft-lock the main window forever.
struct OwnerEnableGuard(HWND);
impl Drop for OwnerEnableGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = EnableWindow(self.0, true);
        }
    }
}

/// RAII guard that destroys a dialog HWND on drop if it's still
/// alive. Pairs with `OwnerEnableGuard` to make every exit path —
/// `?` propagation, panic, WM_QUIT mid-pump, or `GetMessageW` error —
/// correctly tear down both the dialog window and the disabled-owner
/// state. The `IsWindow` check covers the happy path where the user
/// already clicked OK/Cancel: the dialog is already destroyed and
/// `DestroyWindow` on a dead HWND is a silent error we don't care
/// about, but skipping it keeps the trace log clean.
struct DlgDestroyGuard(HWND);
impl Drop for DlgDestroyGuard {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.0)).as_bool() {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

/// Handle a Language-menu click — flip the active tab's lang to the
/// supplied LangType id. Routes through the same `set_buffer_lang_type`
/// the plugin ABI uses, so the code path covers re-applying the lexer
/// for the active tab and queueing `NPPN_LANGCHANGED`.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn handle_language_menu_click(hwnd: HWND, lang_id: i32) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        unsafe { handle_language_menu_click_inner(hwnd, lang_id) };
    }));
}

unsafe fn handle_language_menu_click_inner(hwnd: HWND, lang_id: i32) {
    // Route through `dispatch_plugin_message` with the
    // NPPM_SETBUFFERLANGTYPE wire format so the menu click and a
    // plugin-driven set go through one code path. That code already
    // re-applies the lexer for the active tab and queues
    // NPPN_LANGCHANGED on a real change; mirroring it here means
    // adding/changing language behaviour only happens in one place.
    let buffer_id = if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        match state.shell.active() {
            Some(t) => t.id as usize,
            None => return,
        }
    } else {
        return;
    };
    if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        let handles = state.host_handles(hwnd);
        let (shell, mut ui) = state.split();
        // SAFETY: `dispatch_plugin_message` requires a UI-thread
        // call with valid handles for this window. The wnd_proc
        // contract gives us both.
        unsafe {
            let _ = shell.dispatch_plugin_message(
                &mut ui,
                handles,
                codepp_plugin_host::dispatch::NPPM_SETBUFFERLANGTYPE,
                buffer_id,
                lang_id as isize,
            );
        }
    }
    unsafe {
        fire_queued_notifications(hwnd);
    }
}

/// Handle a Window-menu click — switch the active tab to `tab_idx`.
/// Routes through the same code path as a tab-strip click so the
/// editor view, status bar, language and window title all update
/// together.
///
/// # Safety
///
/// Caller must invoke from the UI thread that owns `hwnd`.
unsafe fn handle_window_menu_click(hwnd: HWND, tab_idx: usize) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        unsafe { handle_window_menu_click_inner(hwnd, tab_idx) };
    }));
}

unsafe fn handle_window_menu_click_inner(hwnd: HWND, tab_idx: usize) {
    // TCM_SETCURSEL doesn't generate TCN_SELCHANGE, so we manually
    // invoke the selchange handler after a successful cursor move
    // to do the rest of the work (doc rebind, status, lexer,
    // notification queueing). The selchange call is gated on the
    // cursor move actually happening — if `state_from_hwnd` returns
    // None (e.g. PLUGIN_CALL_ACTIVE is set because a plugin's
    // beNotified synthesised this WM_COMMAND), no cursor move
    // occurred and firing selchange anyway would queue a spurious
    // BUFFERACTIVATED on the wrong tab.
    let did_move = if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
        if tab_idx >= state.shell.tabs.len() {
            return;
        }
        unsafe {
            SendMessageW(
                state.tab_hwnd,
                TCM_SETCURSEL,
                Some(WPARAM(tab_idx)),
                Some(LPARAM(0)),
            );
        }
        true
    } else {
        false
    };
    if did_move {
        unsafe {
            handle_tab_selchange(hwnd);
        }
    }
}

/// Run the Code++ Win32 event loop. Blocks until the user exits.
///
/// `initial_path` (if `Some`) is queued for opening immediately after
/// the window is shown — same code path as drag-and-drop and as
/// session-restore. Used for Phase 2 demo verification and for
/// `codepp.exe <path>` from the shell.
pub fn run(initial_path: Option<PathBuf>) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;

        // Common controls — status bar (BAR) and tab strip (TAB).
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_TAB_CLASSES,
        };
        InitCommonControlsEx(&icc).ok()?;

        // Register Scintilla's window classes.
        if Scintilla_RegisterClasses(instance.0) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        // Register our main-window class.
        let main_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(main_wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut c_void),
            lpszClassName: MAIN_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&main_class) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        // Full N++-shaped menu bar. `build_main_menu` returns the
        // bar plus the HMENUs of submenus we need later for dynamic
        // refresh (encoding/lang radios, View toggles) or for plugin
        // ABI exposure (the per-plugin submenu).
        let menus = build_main_menu()?;
        let menubar = menus.bar;
        let plugin_menu = menus.plugin_menu;

        // Create the main window without children first; we attach
        // them after the Shell is built and stashed in GWLP_USERDATA.
        let main_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            MAIN_CLASS,
            w!("Code++"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            900,
            600,
            None,
            Some(menubar),
            Some(instance.into()),
            None,
        )?;

        // Status bar — auto-anchored to the bottom by the SB_RESIZE
        // pattern (parent WM_SIZE forwards to the status bar so it
        // stretches to fill width).
        let status_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            STATUSBAR_CLASS,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            Some(main_hwnd),
            None,
            Some(instance.into()),
            None,
        )?;
        // A freshly-created status bar has zero parts. SB_SETTEXTW on a
        // non-existent part silently no-ops (returns success without
        // storing anything), which produces an empty status bar at
        // runtime. Define a single full-width part now; later phases
        // can split into multiple parts (encoding | EOL | cursor | etc).
        {
            const SB_SETPARTS: u32 = 0x0404;
            // -1 in the right-edge array means "extend to right edge".
            let edges: [i32; 1] = [-1];
            SendMessageW(
                status_hwnd,
                SB_SETPARTS,
                Some(WPARAM(edges.len())),
                Some(LPARAM(edges.as_ptr() as isize)),
            );
        }

        // Tab control — sits below the menu bar, above Scintilla.
        // One TCITEMW per `Shell.tabs[i]` is inserted lazily by
        // `sync_tab_strip` after the first drain delivers a load
        // result. WM_NOTIFY (TCN_SELCHANGE) wires click → tab
        // switch via SCI_SETDOCPOINTER.
        let tab_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WC_TABCONTROL,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            Some(main_hwnd),
            None,
            Some(instance.into()),
            None,
        )?;

        // Scintilla child — sized via WM_SIZE relative to the status bar.
        let scintilla_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            SCINTILLA_CLASS,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            Some(main_hwnd),
            None,
            Some(instance.into()),
            None,
        )?;

        // Capture the direct-call pair.
        let direct_fn_lr = SendMessageW(scintilla_hwnd, SCI_GETDIRECTFUNCTION, None, None);
        let direct_ptr_lr = SendMessageW(scintilla_hwnd, SCI_GETDIRECTPOINTER, None, None);
        if direct_fn_lr.0 == 0 || direct_ptr_lr.0 == 0 {
            return Err(windows::core::Error::new(
                E_FAIL,
                "Scintilla returned null SCI_GETDIRECTFUNCTION/SCI_GETDIRECTPOINTER",
            ));
        }
        let direct_fn: ScintillaDirectFunction = std::mem::transmute(direct_fn_lr.0);
        let direct_ptr = direct_ptr_lr.0 as *mut c_void;
        let editor = EditorHandle::new(scintilla_hwnd.0, direct_fn, direct_ptr);

        // Horizontal scroll: by default Scintilla seeds scrollWidth
        // at 2000 px and never shrinks it, so the user can scroll
        // far past the end of any visible line into empty space.
        // Width tracking (SCI_SETSCROLLWIDTHTRACKING(1)) tells
        // Scintilla to recompute scrollWidth as the longest visible
        // line, with `SCI_SETSCROLLWIDTH(1)` seeding the starting
        // value at 1 px so the first paint doesn't carry the 2000
        // default forward. Together: horizontal scrollbar only
        // appears when content actually overflows, and scrolling
        // stops at the real end of the longest line.
        editor.send(SCI_SETSCROLLWIDTH, 1, 0);
        editor.send(SCI_SETSCROLLWIDTHTRACKING, 1, 0);

        // Wake closure: PostMessage ourselves WM_APP_WAKE.
        // PostMessage is thread-safe — it just enqueues a message for
        // the target window's thread, which is what we want.
        let main_hwnd_value = main_hwnd.0 as usize;
        let wake = Arc::new(move || {
            // SAFETY: HWND is just a handle — PostMessageW only
            // enqueues a message for the target window's thread,
            // doesn't dereference. The window may have been destroyed
            // by the time wake fires; PostMessage returns FALSE in
            // that case and we ignore it. The enclosing run() body is
            // already wrapped in an unsafe block, so the closure
            // doesn't need its own.
            let _ = PostMessageW(
                Some(HWND(main_hwnd_value as *mut c_void)),
                WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }) as Arc<dyn Fn() + Send + Sync>;

        let mut shell = Shell::new(wake)
            .map_err(|e| windows::core::Error::new(E_FAIL, format!("shell init: {e}")))?;

        // Plugin discovery: enumerate `*.dll` candidates in the user's
        // plugins directory. **No DLL is mapped here** (DESIGN.md
        // §6.4 mandates lazy load); each candidate stays in the
        // `Pending` state until first menu touch. A non-existent
        // plugins directory is the first-run case and is not an
        // error — `discover_plugins` returns 0 silently.
        if let Some(dir) = codepp_platform::plugins_dir() {
            match shell.discover_plugins(&dir) {
                Ok(count) => {
                    tracing::info!(plugins_dir = ?dir, count = count, "plugin candidates discovered");
                }
                Err(e) => {
                    tracing::warn!(plugins_dir = ?dir, error = %e, "plugin discovery failed");
                }
            }
        }

        // Heap-allocate the WindowState. We resolve and queue the
        // initial-open path while we still own the box (i.e.
        // BEFORE installing it in `GWLP_USERDATA`), so there's
        // never a moment when both a Rust `&mut` reference into
        // the state and the raw pointer in `GWLP_USERDATA` are
        // simultaneously live. `state_from_hwnd` returns `None`
        // until the slot is filled at the bottom of this block.
        let mut state = Box::new(WindowState {
            scintilla_hwnd,
            status_hwnd,
            tab_hwnd,
            synced_tab_count: 0,
            main_menu: menubar,
            plugin_menu,
            plugins_menu_initialized: false,
            view_menu: menus.view_menu,
            encoding_menu: menus.encoding_menu,
            language_menu: menus.language_menu,
            window_menu: menus.window_menu,
            editor,
            shell,
        });

        // Resolve the initial files:
        //   1. Explicit CLI argument wins (manual override / test path).
        //      Single file → single open.
        //   2. Otherwise, restore *every* tab from session.xml in its
        //      stored order, then override `active_tab` to the
        //      session's recorded active index. Without the override
        //      `open_file` would always end with the last-pushed tab
        //      active, regardless of which one the user had focused
        //      before shutdown.
        if let Some(path) = initial_path {
            state.shell.open_file(path);
        } else {
            let paths = state.shell.load_session_paths();
            for p in paths {
                state.shell.open_file(p);
            }
            if let Some(idx) = state.shell.session_active_index() {
                if idx < state.shell.tabs.len() {
                    state.shell.active_tab = Some(idx);
                } else {
                    // Saved active index points outside the
                    // restored tab range — happens if a tab
                    // failed to push (e.g. `loader.open` returned
                    // None for a path) or if session.xml was
                    // tampered. Fall back to the most-recently-
                    // pushed tab and surface the rejection so the
                    // user has a breadcrumb if their previously-
                    // active buffer isn't the one focused.
                    tracing::warn!(
                        session_active = idx,
                        restored = state.shell.tabs.len(),
                        "session.xml active index out of range; using last-restored tab",
                    );
                }
            }
        }

        // Box now finalized. Install the raw pointer in GWLP_USERDATA;
        // the Box is reclaimed in WM_DESTROY.
        let state_ptr = Box::into_raw(state);
        SetWindowLongPtrW(main_hwnd, GWLP_USERDATA, state_ptr as isize);

        // Drag-drop: tell Windows our window accepts files.
        DragAcceptFiles(main_hwnd, true);

        // Show + size + focus.
        let _ = ShowWindow(main_hwnd, SW_SHOW);
        let mut rect = RECT::default();
        GetClientRect(main_hwnd, &mut rect)?;
        layout_children(
            tab_hwnd,
            scintilla_hwnd,
            status_hwnd,
            rect.right,
            rect.bottom,
        );
        let _ = SetFocus(Some(scintilla_hwnd));

        // Accelerator table. We route through `TranslateAcceleratorW`
        // rather than handling `WM_KEYDOWN` in the wnd_proc because
        // Scintilla owns keyboard focus while the user types, so a
        // parent-side WM_KEYDOWN never fires for editor keystrokes.
        // The accelerator table is queried before
        // `TranslateMessage`/`DispatchMessageW`, posting a WM_COMMAND
        // to the main window without depending on focus.
        //
        // **Scintilla-native shortcuts are deliberately absent.**
        // Scintilla's own keyboard table already binds Ctrl+X / C /
        // V / Z / Y / A to its built-in cut/copy/paste/undo/redo/
        // selectall implementations. If we register them as
        // accelerators, TranslateAcceleratorW intercepts the
        // keypress before Scintilla sees it and hands the work back
        // through a WM_COMMAND round-trip — same end result on the
        // happy path, but it's a duplicate code path that surfaces
        // odd glitches under heavy keyboard activity (the
        // user-observed "Ctrl+V sometimes doesn't paste, second
        // attempt works" was traced to this duplication). The menu
        // items still display the shortcut hint (`\tCtrl+X`) and
        // mouse clicks on them fire WM_COMMAND → editor.send(SCI_*),
        // so menu and keyboard each have a single, well-defined
        // path that doesn't fight with the other.
        //
        // The table covers only the commands Scintilla doesn't
        // already bind natively:
        //   - File: Save (Ctrl+S), Close (Ctrl+W).
        //   - Search: Find / Replace / Find-in-Files / Goto Line.
        //   - View: Zoom In / Zoom Out / Restore Zoom.
        let ctrl = ACCEL_VIRT_FLAGS(FCONTROL.0 | FVIRTKEY.0);
        let ctrl_shift = ACCEL_VIRT_FLAGS(FCONTROL.0 | FSHIFT.0 | FVIRTKEY.0);
        let accels = [
            // File
            ACCEL {
                fVirt: ctrl,
                key: VK_S.0,
                cmd: ID_FILE_SAVE,
            },
            ACCEL {
                fVirt: ctrl,
                key: VK_W.0,
                cmd: ID_FILE_CLOSE,
            },
            // Search
            ACCEL {
                fVirt: ctrl,
                key: VK_F.0,
                cmd: ID_SEARCH_FIND,
            },
            ACCEL {
                fVirt: ctrl,
                key: VK_H.0,
                cmd: ID_SEARCH_REPLACE,
            },
            ACCEL {
                fVirt: ctrl_shift,
                key: VK_F.0,
                cmd: ID_SEARCH_FINDINFILES,
            },
            ACCEL {
                fVirt: ctrl,
                key: VK_G.0,
                cmd: ID_SEARCH_GOTOLINE,
            },
            // View
            ACCEL {
                fVirt: ctrl,
                key: VK_OEM_PLUS.0,
                cmd: ID_VIEW_ZOOMIN,
            },
            ACCEL {
                fVirt: ctrl,
                key: VK_OEM_MINUS.0,
                cmd: ID_VIEW_ZOOMOUT,
            },
            ACCEL {
                fVirt: ctrl,
                key: VK_0.0,
                cmd: ID_VIEW_ZOOMRESET,
            },
        ];
        let haccel: HACCEL = CreateAcceleratorTableW(&accels)?;

        // Standard message loop with accelerator translation.
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            match ret.0 {
                0 => break,
                -1 => return Err(windows::core::Error::from_thread()),
                _ => {
                    if TranslateAcceleratorW(main_hwnd, haccel, &msg) == 0 {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Layout children: tab strip across the top, status bar at the
/// bottom, Scintilla fills the remainder.
///
/// SAFETY: caller must hold valid HWNDs for the four windows.
unsafe fn layout_children(tabs: HWND, scintilla: HWND, status: HWND, width: i32, height: i32) {
    // SB_GETBORDERS / TCM_GETITEMRECT could measure these precisely;
    // for now use fixed 22 px each which matches the Win32 default
    // tab/status height at 96 DPI. DPI-aware sizing is a Phase 4
    // polish item.
    let tab_height = 22_i32;
    let status_height = 22_i32;
    unsafe {
        let _ = MoveWindow(tabs, 0, 0, width, tab_height, true);
        let _ = MoveWindow(
            status,
            0,
            height - status_height,
            width,
            status_height,
            true,
        );
        let scintilla_top = tab_height;
        let scintilla_height = (height - status_height - tab_height).max(0);
        let _ = MoveWindow(scintilla, 0, scintilla_top, width, scintilla_height, true);
    }
}

extern "system" fn main_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                // GWLP_USERDATA is set by run() AFTER CreateWindowExW
                // returns, since the state needs the HWND. So there's
                // no CREATESTRUCT.lpCreateParams to harvest here.
                // Fall through to default processing.
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_APP_WAKE => {
                // Drain the shell's task queues. Drain returns any
                // pending modal dialogs; we MUST show them only AFTER
                // the &mut WindowState borrow is dropped, otherwise
                // MessageBoxW's nested message pump can re-enter
                // wnd_proc and produce aliasing UB.
                let pending: Vec<PendingDialog> = {
                    if let Some(state) = state_from_hwnd(hwnd) {
                        let (shell, mut ui) = state.split();
                        shell.drain(&mut ui)
                    } else {
                        Vec::new()
                    }
                };
                // No state borrow held below this point.
                for dialog in pending {
                    match dialog {
                        PendingDialog::ConfirmReload(path) => {
                            let yes = show_reload_dialog(hwnd, &path);
                            if yes {
                                if let Some(state) = state_from_hwnd(hwnd) {
                                    state.shell.confirm_reload(path);
                                }
                            }
                        }
                        PendingDialog::Error { title, message } => {
                            show_error_dialog(hwnd, &title, &message);
                        }
                    }
                }
                // Fire any NPPN_* notifications queued by the drain
                // (NPPN_FILEOPENED on a successful load). Done AFTER
                // dialogs so a plugin that might block in beNotified
                // doesn't delay the user-visible reload prompt.
                fire_queued_notifications(hwnd);
                // Bring the tab strip into sync after any new tabs
                // were pushed during the drain. Done after the
                // dialog/notification cycle so the user sees the
                // new tab appear at the same moment they get the
                // open's "load complete" feedback. The window
                // title is refreshed alongside so the title bar
                // reflects whichever tab the drain just activated.
                if let Some(state) = state_from_hwnd(hwnd) {
                    sync_tab_strip(state);
                    update_window_title(hwnd, &state.shell);
                }
                LRESULT(0)
            }
            WM_DROPFILES => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    let hdrop = HDROP(wparam.0 as *mut c_void);
                    handle_dropped_files(state, hdrop);
                    DragFinish(hdrop);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let cmd_u16 = (wparam.0 & 0xFFFF) as u16;
                let cmd_i32 = cmd_u16 as i32;
                match cmd_u16 {
                    ID_FILE_SAVE => {
                        // Save inside the borrow; if it fails, capture
                        // the error message and show the dialog AFTER
                        // the borrow is dropped (same UB rule as
                        // WM_APP_WAKE).
                        //
                        // The save call is wrapped in catch_unwind so
                        // a host-internal panic — e.g. a `Vec::push`
                        // OOM in the notification queue, or a
                        // tracing-subscriber misbehaviour — doesn't
                        // unwind across the `extern "system"`
                        // wnd_proc frame (UB at the FFI boundary).
                        let save_error: Option<String> = {
                            if let Some(state) = state_from_hwnd(hwnd) {
                                let (shell, mut ui) = state.split();
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        shell.save_current_to_disk(&mut ui)
                                    }));
                                match result {
                                    Ok(Ok(())) => {
                                        state.editor.send(SCI_SETSAVEPOINT, 0, 0);
                                        None
                                    }
                                    Ok(Err(e)) => Some(e.to_string()),
                                    Err(_) => Some("internal panic during save".to_string()),
                                }
                            } else {
                                None
                            }
                        };
                        if let Some(msg) = save_error {
                            show_error_dialog(hwnd, "Save failed", &msg);
                        }
                        // On a successful save, save_current_to_disk
                        // queues NPPN_FILESAVED; on failure the queue
                        // is empty and this is a no-op.
                        fire_queued_notifications(hwnd);
                    }
                    ID_FILE_CLOSE => {
                        handle_close_active_tab(hwnd);
                    }
                    ID_FILE_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    // Edit menu — every entry is a single Scintilla
                    // direct-call. `editor` is `Copy`, so we pull a
                    // local copy out of the brief `&mut state` borrow
                    // and the borrow ends before the SCI call. Plugin
                    // re-entrance isn't a concern (Scintilla direct
                    // calls don't go through any wnd_proc).
                    ID_EDIT_UNDO | ID_EDIT_REDO | ID_EDIT_CUT | ID_EDIT_COPY | ID_EDIT_PASTE
                    | ID_EDIT_DELETE | ID_EDIT_SELECTALL => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            let editor = state.editor;
                            let sci_msg = match cmd_u16 {
                                ID_EDIT_UNDO => SCI_UNDO,
                                ID_EDIT_REDO => SCI_REDO,
                                ID_EDIT_CUT => SCI_CUT,
                                ID_EDIT_COPY => SCI_COPY,
                                ID_EDIT_PASTE => SCI_PASTE,
                                ID_EDIT_DELETE => SCI_CLEAR,
                                ID_EDIT_SELECTALL => SCI_SELECTALL,
                                _ => unreachable!(),
                            };
                            editor.send(sci_msg, 0, 0);
                        }
                    }
                    // View toggles — flip the corresponding Scintilla
                    // setting; the next WM_INITMENUPOPUP refreshes
                    // the check mark from the now-updated state.
                    ID_VIEW_WORDWRAP => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            let editor = state.editor;
                            let on = editor.send(SCI_GETWRAPMODE, 0, 0) != 0;
                            editor.send(SCI_SETWRAPMODE, if on { 0 } else { 1 }, 0);
                        }
                    }
                    ID_VIEW_SHOWWS => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            let editor = state.editor;
                            let on = editor.send(SCI_GETVIEWWS, 0, 0) != 0;
                            // SCWS_INVISIBLE = 0, SCWS_VISIBLEALWAYS = 1.
                            editor.send(SCI_SETVIEWWS, if on { 0 } else { 1 }, 0);
                        }
                    }
                    ID_VIEW_SHOWEOL => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            let editor = state.editor;
                            let on = editor.send(SCI_GETVIEWEOL, 0, 0) != 0;
                            editor.send(SCI_SETVIEWEOL, if on { 0 } else { 1 }, 0);
                        }
                    }
                    ID_VIEW_ZOOMIN => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            state.editor.send(SCI_ZOOMIN, 0, 0);
                        }
                    }
                    ID_VIEW_ZOOMOUT => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            state.editor.send(SCI_ZOOMOUT, 0, 0);
                        }
                    }
                    ID_VIEW_ZOOMRESET => {
                        if let Some(state) = state_from_hwnd(hwnd) {
                            state.editor.send(SCI_SETZOOM, 0, 0);
                        }
                    }
                    // Help → About: simple MessageBox with version
                    // info. Modal pump runs after the borrow drops.
                    ID_HELP_ABOUT => {
                        show_about_dialog(hwnd);
                    }
                    // Go to... (m3b1). Pull the caret's line + offset
                    // and the document's line count + length off the
                    // editor so the dialog can populate both axes
                    // independently. After the user picks, dispatch
                    // to SCI_GOTOLINE (zero-based line) or SCI_GOTOPOS
                    // (zero-based byte offset) per the result variant
                    // and re-focus Scintilla so the user can type
                    // immediately.
                    ID_SEARCH_GOTOLINE => {
                        let seed = if let Some(state) = state_from_hwnd(hwnd) {
                            let editor = state.editor;
                            let pos = editor.send(SCI_GETCURRENTPOS, 0, 0).max(0) as u32;
                            let cur_line =
                                editor.send(SCI_LINEFROMPOSITION, pos as usize, 0).max(0) as u32;
                            let total_lines = editor.send(SCI_GETLINECOUNT, 0, 0).max(0) as u32;
                            let length = editor.send(SCI_GETLENGTH, 0, 0).max(0) as u32;
                            Some((
                                cur_line.saturating_add(1).max(1),
                                total_lines.max(1),
                                pos,
                                length,
                                state.scintilla_hwnd,
                            ))
                        } else {
                            None
                        };
                        if let Some((cur_line, max_line, cur_offset, max_offset, scintilla_hwnd)) =
                            seed
                        {
                            if let Some(target) =
                                show_goto_dialog(hwnd, cur_line, max_line, cur_offset, max_offset)
                            {
                                if let Some(state) = state_from_hwnd(hwnd) {
                                    match target {
                                        GotoTarget::Line(n) => {
                                            state.editor.send(
                                                SCI_GOTOLINE,
                                                n.saturating_sub(1) as usize,
                                                0,
                                            );
                                        }
                                        GotoTarget::Offset(p) => {
                                            state.editor.send(SCI_GOTOPOS, p as usize, 0);
                                        }
                                    }
                                }
                            }
                            let _ = SetFocus(Some(scintilla_hwnd));
                        }
                    }
                    // Find / Replace stubs — Ctrl+F / Ctrl+H
                    // accelerators already arrive here; the dialogs
                    // are wired in m3b2.
                    ID_SEARCH_FIND
                    | ID_SEARCH_FINDNEXT
                    | ID_SEARCH_FINDPREV
                    | ID_SEARCH_REPLACE
                    | ID_SEARCH_FINDINFILES => {
                        tracing::trace!(
                            cmd = cmd_u16,
                            "find/replace command not yet wired (Phase 4 m3b2)",
                        );
                    }
                    _ => {
                        // Dynamic-range commands (Language menu,
                        // Window menu). These reach the `_` arm
                        // because their cmd ids carry a parameter
                        // value (lang-id or tab-idx) rather than a
                        // single fixed constant.
                        if (ID_LANGUAGE_BASE..=ID_LANGUAGE_END).contains(&cmd_u16) {
                            let lang_id = (cmd_u16 - ID_LANGUAGE_BASE) as i32;
                            handle_language_menu_click(hwnd, lang_id);
                            return LRESULT(0);
                        }
                        if (ID_WINDOW_BASE..=ID_WINDOW_END).contains(&cmd_u16) {
                            let tab_idx = (cmd_u16 - ID_WINDOW_BASE) as usize;
                            handle_window_menu_click(hwnd, tab_idx);
                            return LRESULT(0);
                        }
                        // Plugin menu-command dispatch. Plugin cmd-ids
                        // start at PLUGIN_CMD_ID_BASE (50000) — well
                        // above any host built-in. Look up the
                        // callback through the borrow, **then drop
                        // the borrow before invoking**: a plugin's
                        // PluginCmd is allowed to SendMessage(NPPM_*)
                        // back into our wnd_proc, which would
                        // re-enter and materialize a second
                        // &mut WindowState from the same raw pointer
                        // (aliasing UB). Splitting into a lookup
                        // phase and an invoke phase keeps the
                        // re-entrant call sound.
                        if cmd_i32 >= PLUGIN_CMD_ID_BASE {
                            let p_func = if let Some(state) = state_from_hwnd(hwnd) {
                                state.shell.lookup_plugin_command(cmd_i32)
                            } else {
                                None
                            };
                            // Borrow on `state` ends here.
                            if let Some(f) = p_func {
                                // SAFETY: `f` is the C ABI fn ptr the
                                // plugin handed us in FuncItem.p_func.
                                // The plugin's DLL stays loaded for as
                                // long as Shell holds it; the pointer
                                // is valid. catch_unwind so a Rust-
                                // authored plugin's panic doesn't
                                // unwind across the C ABI. The
                                // PluginCallGuard arms the re-entrance
                                // flag in case the plugin
                                // SendMessages NPPM_* back; defense
                                // in depth even though NLL has
                                // already dropped the lookup borrow.
                                // Guard inside the catch_unwind
                                // closure so nested-guard assert is
                                // caught here. Same pattern as
                                // `fire_queued_notifications`.
                                let _ =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        let _guard = PluginCallGuard::enter();
                                        f();
                                    }));
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_INITMENUPOPUP => {
                // First-touch plugin load. wparam carries the HMENU
                // of the popup about to be shown; lparam packs (item
                // index, is_window_menu_flag). We compare the HMENU
                // against our `plugin_menu` and, if it matches and we
                // haven't yet loaded, load every Pending plugin and
                // append their FuncItems as menu entries.
                //
                // The lookup phase holds `state`; the load phase has
                // to drop the borrow first because plugin `setInfo`
                // can synchronously SendMessage(NPPM_*) back into
                // wnd_proc (re-entrance). We therefore: (1) decide
                // whether work is needed under the borrow, (2) drop
                // the borrow, (3) reacquire and run the load, (4)
                // populate the menu under a fresh borrow.
                let popup_hmenu_value = wparam.0;
                let needs_init = if let Some(state) = state_from_hwnd(hwnd) {
                    !state.plugins_menu_initialized
                        && popup_hmenu_value == state.plugin_menu.0 as usize
                } else {
                    false
                };
                if needs_init {
                    // Build NppData under a brief borrow.
                    let npp_data = if let Some(state) = state_from_hwnd(hwnd) {
                        NppData {
                            npp_handle: hwnd.0,
                            scintilla_main_handle: state.scintilla_hwnd.0,
                            scintilla_second_handle: core::ptr::null_mut(),
                        }
                    } else {
                        // Should be unreachable given the needs_init
                        // path above, but stay defensive.
                        return LRESULT(0);
                    };
                    // Mark the menu as initialized **before** running
                    // any plugin code so a nested WM_INITMENUPOPUP
                    // (from any path that re-enters wnd_proc during
                    // load) sees `needs_init == false` and skips
                    // re-running the load. We pay the cost of a
                    // possibly-empty submenu in the rare error case
                    // where load fails entirely — preferable to
                    // double-loading the same plugin.
                    if let Some(state) = state_from_hwnd(hwnd) {
                        state.plugins_menu_initialized = true;
                    }
                    // Trigger lazy load. The PluginCallGuard arms the
                    // PLUGIN_CALL_ACTIVE flag for the duration of the
                    // call so any re-entrant `state_from_hwnd` from a
                    // plugin's `setInfo` returns None — preventing
                    // the second `&mut WindowState` materialization
                    // that would otherwise alias with our outer
                    // borrow. The guard's Drop clears the flag even
                    // on panic.
                    //
                    // The whole call is wrapped in `catch_unwind` so
                    // a host-internal panic (allocation failure,
                    // tracing-subscriber misbehaviour) doesn't
                    // unwind across the `extern "system"` wnd_proc
                    // frame — that's UB. Plugin entry-points are
                    // already individually `catch_unwind`-wrapped
                    // inside `load_inner`; this outer guard catches
                    // panics in our own bookkeeping.
                    if let Some(state) = state_from_hwnd(hwnd) {
                        // Guard inside the catch_unwind closure so
                        // its assert (nested-guard detection) is
                        // caught here rather than unwinding across
                        // extern "system". Same pattern as
                        // `fire_queued_notifications`.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _guard = PluginCallGuard::enter();
                            state.shell.ensure_plugins_loaded(npp_data);
                        }));
                    }
                    // Populate the menu from loaded plugins. We rebuild
                    // the FuncItem list inside a borrow, then call
                    // AppendMenuW for each entry; AppendMenuW does
                    // not re-enter our wnd_proc, so the borrow can
                    // span the population.
                    if let Some(state) = state_from_hwnd(hwnd) {
                        populate_plugin_menu(state.plugin_menu, &state.shell);
                        // Force the menu bar to redraw so the user
                        // sees the populated submenu on this very
                        // open (without a redraw, the items only
                        // appear after the popup re-displays).
                        let _ = DrawMenuBar(hwnd);
                    }
                }
                // Refresh dynamic state on the four state-driven
                // submenus (View toggles, Encoding/Language radios,
                // Window list). Each refresh is read-only against
                // `state` apart from the rebuild on `window_menu`,
                // which only mutates the menu HMENU — not anything
                // a re-entrant call could observe through `state`.
                // None of these helpers re-enter our wnd_proc.
                if let Some(state) = state_from_hwnd(hwnd) {
                    if popup_hmenu_value == state.view_menu.0 as usize {
                        refresh_view_menu(state.view_menu, &state.editor);
                    } else if popup_hmenu_value == state.encoding_menu.0 as usize {
                        if let Some(active) = state.shell.active() {
                            refresh_encoding_menu(state.encoding_menu, &active.encoding);
                        }
                    } else if popup_hmenu_value == state.language_menu.0 as usize {
                        if let Some(active) = state.shell.active() {
                            refresh_language_menu(state.language_menu, active.lang);
                        }
                    } else if popup_hmenu_value == state.window_menu.0 as usize {
                        refresh_window_menu(state.window_menu, &state.shell);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_SIZE => {
                let width = (lparam.0 & 0xFFFF) as i32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as i32;
                if let Some(state) = state_from_hwnd(hwnd) {
                    layout_children(
                        state.tab_hwnd,
                        state.scintilla_hwnd,
                        state.status_hwnd,
                        width,
                        height,
                    );
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    let _ = SetFocus(Some(state.scintilla_hwnd));
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // Persist the session before tearing down. Pull live
                // text/cursor through the editor while it still
                // exists — once we PostQuitMessage the message pump
                // unwinds and the Scintilla window is destroyed.
                if let Some(state) = state_from_hwnd(hwnd) {
                    let (shell, mut ui) = state.split();
                    // catch_unwind for the same reason as
                    // ID_FILE_SAVE: a panic in shell bookkeeping must
                    // not unwind across the extern "system" wnd_proc
                    // frame during teardown.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        shell.save_session(&mut ui)
                    }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "session save failed"),
                        Err(_) => tracing::warn!("session save panicked"),
                    }
                }

                // Drain any leftover NPPN_FILEOPENED/NPPN_FILESAVED
                // notifications that were queued since the last
                // WM_APP_WAKE drain — a file-open completing right
                // before the user closes the app would otherwise be
                // silently dropped, breaking plugins that audit-log
                // file activity. Safe to call here: no borrow is
                // held, and `fire_queued_notifications` arms its
                // own PluginCallGuard around each plugin call.
                fire_queued_notifications(hwnd);

                // Fire NPPN_SHUTDOWN to every loaded plugin while the
                // WindowState (and the PluginHost it owns) still
                // exists. The PluginCallGuard prevents a plugin's
                // beNotified from materializing a second
                // &mut WindowState via re-entrant SendMessage; the
                // catch_unwind keeps a host-internal panic from
                // unwinding across the extern "system" wnd_proc.
                if let Some(state) = state_from_hwnd(hwnd) {
                    // Guard inside the catch_unwind closure so the
                    // nested-guard assert (if it ever fired) is
                    // caught here. Same pattern as
                    // `fire_queued_notifications`.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _guard = PluginCallGuard::enter();
                        state.shell.notify_plugins(Notification::Shutdown, hwnd.0);
                    }));
                }

                // Reclaim the WindowState box. After this point, any
                // re-entrant wnd_proc gets `None` from
                // `state_from_hwnd` (GWLP_USERDATA == 0), so any late
                // plugin SendMessage during teardown is safely
                // dispatched as DefWindowProcW.
                let raw = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if raw != 0 {
                    let _ = Box::from_raw(raw as *mut WindowState);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Plugin host inbound dispatch. Plugins call
            // `SendMessage(npp_hwnd, NPPM_*, …)`; the dispatcher
            // (`plugin_host::dispatch_nppm`) handles every v1 message
            // and returns `Some(LRESULT)`. Out-of-range messages
            // return `None` and we fall through to the default
            // handler.
            //
            // The range guard here matches the dispatcher's own
            // (NPPMSG..NPPMSG+200). Pre-filtering at the wnd_proc
            // layer keeps `dispatch_plugin_message` (and the
            // `state_from_hwnd` traversal it requires) off the hot
            // path for every non-plugin WM_USER message.
            m if (NPPMSG..NPPMSG + NPPMSG_RANGE).contains(&m) => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    let handles = state.host_handles(hwnd);
                    let (shell, mut ui) = state.split();
                    // SAFETY: `(msg, wparam, lparam)` are forwarded
                    // verbatim from a real Win32 wnd_proc dispatch;
                    // `handles` describes the same window. The
                    // plugin sending the message is bound by the
                    // documented NPPM_* ABI in the compat headers.
                    // No nested `unsafe` block needed — the entire
                    // wnd_proc body runs inside one already.
                    let routed =
                        shell.dispatch_plugin_message(&mut ui, handles, m, wparam.0, lparam.0);
                    match routed {
                        Some(lr) => LRESULT(lr),
                        None => DefWindowProcW(hwnd, msg, wparam, lparam),
                    }
                } else {
                    // No state yet (early WM_NCCREATE territory); fall
                    // through to default. Plugins shouldn't be
                    // sending NPPM_* this early — they receive
                    // `npp_hwnd` from `setInfo`, which only runs
                    // once the WindowState exists.
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_NOTIFY => {
                // lParam points to an NMHDR; for tab-control
                // notifications it points to the start of a tab-
                // specific struct whose first field is NMHDR. We
                // only act on TCN_SELCHANGE for our own tab control;
                // other WM_NOTIFY senders fall through to default.
                let nmhdr_ptr = lparam.0 as *const NMHDR;
                if !nmhdr_ptr.is_null() {
                    // SAFETY (covered by the wnd_proc's outer
                    // `unsafe` block): Win32 hands us a valid NMHDR
                    // pointer for the duration of the dispatch. We
                    // read by value into a local; nothing escapes.
                    let nmhdr = *nmhdr_ptr;
                    if nmhdr.code == TCN_SELCHANGE {
                        // Filter on the source HWND so a future
                        // sibling tab control doesn't accidentally
                        // drive our state machine.
                        let owns_source = if let Some(state) = state_from_hwnd(hwnd) {
                            nmhdr.hwndFrom == state.tab_hwnd
                        } else {
                            false
                        };
                        if owns_source {
                            handle_tab_selchange(hwnd);
                        }
                    } else if nmhdr.code == SCN_MODIFIED {
                        // Scintilla's tracking-mode horizontal
                        // scrollWidth is high-water-mark — it grows
                        // when a wider line appears but never
                        // shrinks when long lines are deleted, so
                        // the scrollbar stays after the long line
                        // is gone. SCI_SETSCROLLWIDTH(1) resets
                        // `lineWidthMaxSeen`; the next paint
                        // recomputes from the current visible
                        // content. Filter to text-changing
                        // modifications only (insert / delete) —
                        // style and fold notifications don't
                        // affect line widths.
                        //
                        // SAFETY (outer `unsafe`): Win32 hands us a
                        // pointer to a Scintilla `SCNotification`
                        // valid for the dispatch; nmhdr is just its
                        // first field. We read modification_type
                        // by value into a local; nothing escapes.
                        let owns_source = if let Some(state) = state_from_hwnd(hwnd) {
                            nmhdr.hwndFrom == state.scintilla_hwnd
                        } else {
                            false
                        };
                        if owns_source {
                            let sci = &*(lparam.0 as *const SCNotification);
                            let modtype = sci.modification_type;
                            if (modtype & (SC_MOD_INSERTTEXT | SC_MOD_DELETETEXT)) != 0 {
                                if let Some(state) = state_from_hwnd(hwnd) {
                                    state.editor.send(SCI_SETSCROLLWIDTH, 1, 0);
                                }
                            }
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Set while a plugin entry-point call is in flight from this UI
/// thread. The flag protects [`state_from_hwnd`] against Win32's
/// re-entrant SendMessage: a plugin's `setInfo` (or any synchronous
/// plugin callback) can `SendMessage(npp_handle, NPPM_*, ...)` back
/// into our wnd_proc on the same call stack. Without the flag, the
/// re-entrant wnd_proc would materialize a second `&mut WindowState`
/// from the same raw pointer while the outer borrow was still live —
/// aliasing UB. With the flag, the inner `state_from_hwnd` returns
/// `None` and the inner wnd_proc handles the message with no host
/// state (the dispatcher returns 0, which plugins read as "feature
/// unavailable" — same fallback Notepad++ produces when its own
/// state is mid-mutation).
///
/// Win32 dispatches messages serially on the owning thread, so a
/// process-wide static is sufficient — there's no second thread that
/// could reasonably observe a different value.
static PLUGIN_CALL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard that flips [`PLUGIN_CALL_ACTIVE`] on for the duration
/// of a plugin call. Drop unconditionally clears the flag, including
/// on panic — so a Rust-authored plugin that panics doesn't leave
/// the host wedged with a permanently-set guard.
struct PluginCallGuard;

impl PluginCallGuard {
    fn enter() -> Self {
        // Reject nested guards. The current call sites never nest
        // (each is a leaf of the wnd_proc that holds no borrow
        // above it), but a future change that adds a second `enter`
        // while one is already armed would silently get a guard
        // whose Drop clears the flag too early — re-opening the
        // aliasing window for any plugin code still on the stack.
        //
        // Hard `assert!` (not `debug_assert!`) so release builds
        // catch it too. Every call site invokes `enter()` from
        // inside a `catch_unwind` closure, so the panic is caught
        // there rather than crossing the `extern "system"` boundary.
        assert!(
            !PLUGIN_CALL_ACTIVE.load(Ordering::Acquire),
            "PluginCallGuard nested — Drop ordering would clear the flag too early"
        );
        PLUGIN_CALL_ACTIVE.store(true, Ordering::Release);
        Self
    }
}

impl Drop for PluginCallGuard {
    fn drop(&mut self) {
        PLUGIN_CALL_ACTIVE.store(false, Ordering::Release);
    }
}

/// SAFETY: the returned reference borrows from the `Box<WindowState>`
/// stashed in `GWLP_USERDATA`. wnd_proc invocations are serialized
/// per-window (Win32 dispatches one at a time on the owning thread),
/// so concurrent mutable aliasing across messages cannot occur — but
/// see [`PLUGIN_CALL_ACTIVE`] for re-entrant SendMessage from inside
/// plugin code, which IS a path that can produce nested wnd_proc
/// calls on the same stack. The flag check refuses the inner
/// borrow when one is already in flight.
unsafe fn state_from_hwnd<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
    if PLUGIN_CALL_ACTIVE.load(Ordering::Acquire) {
        // Re-entered while a plugin callback is on the stack.
        // Returning None here is what makes the outer borrow safe:
        // the inner wnd_proc never materializes a second
        // &mut WindowState from the raw pointer.
        return None;
    }
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if raw == 0 {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut WindowState) })
    }
}

/// Maximum plausible path length in TCHARs. Windows long paths can
/// theoretically reach ~32767, but anything longer than that from
/// `DragQueryFileW` is implausible and likely a malformed HDROP. We
/// reject rather than risk an overflow when computing `needed + 1`.
const MAX_PATH_TCHARS: u32 = 32_767;

/// SAFETY: `hdrop` must be a valid HDROP handed to us by `WM_DROPFILES`.
unsafe fn handle_dropped_files(state: &mut WindowState, hdrop: HDROP) {
    // DragQueryFileW with iFile=0xFFFFFFFF returns the count.
    let count = unsafe { DragQueryFileW(hdrop, 0xFFFFFFFF, None) };
    for i in 0..count {
        // First call: required-buffer size (TCHARs, exclusive of null).
        let needed = unsafe { DragQueryFileW(hdrop, i, None) };
        if needed == 0 || needed > MAX_PATH_TCHARS {
            // 0 means "no path here" (defensive), and >32767 is the
            // overflow guard: `needed + 1` on a u32 close to MAX would
            // wrap to 0 and we'd allocate an empty buffer, then write
            // OOB on the second DragQueryFileW.
            continue;
        }
        let mut buf = vec![0u16; needed as usize + 1];
        let copied = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
        if copied == 0 {
            continue;
        }
        // `copied` excludes the trailing null.
        buf.truncate(copied);
        let path = PathBuf::from(String::from_utf16_lossy(&buf));
        state.shell.open_file(path);
    }
}
