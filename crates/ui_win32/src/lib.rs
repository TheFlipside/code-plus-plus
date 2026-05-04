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

use codepp_core::{Encoding, Eol};
use codepp_editor::EditorHandle;
use codepp_plugin_host::{Notification, NppData, NPPMSG, NPPMSG_RANGE, PLUGIN_CMD_ID_BASE};
use codepp_scintilla_sys::{
    ScintillaDirectFunction, Scintilla_RegisterClasses, SCI_CREATEDOCUMENT, SCI_EMPTYUNDOBUFFER,
    SCI_GETCURRENTPOS, SCI_GETDIRECTFUNCTION, SCI_GETDIRECTPOINTER, SCI_GETLENGTH, SCI_GETTEXT,
    SCI_GOTOPOS, SCI_SETDOCPOINTER, SCI_SETSAVEPOINT, SCI_SETTEXT, SC_DOCUMENTOPTION_DEFAULT,
};
use codepp_shell::{HostHandles, PendingDialog, Shell, Tab, UiPlatform};
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_TAB_CLASSES, INITCOMMONCONTROLSEX, NMHDR, TCITEMW,
    TCM_GETCURSEL, TCM_INSERTITEMW, TCM_SETCURSEL, TCN_SELCHANGE, WC_TABCONTROL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    DrawMenuBar, GetClientRect, GetMessageW, GetWindowLongPtrW, LoadCursorW, MessageBoxW,
    MoveWindow, PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    GWLP_USERDATA, HMENU, IDC_ARROW, IDYES, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO,
    MF_POPUP, MF_STRING, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_DROPFILES, WM_INITMENUPOPUP, WM_NCCREATE, WM_NOTIFY, WM_SETFOCUS, WM_SIZE, WNDCLASSEXW,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const ID_FILE_SAVE: u16 = 1000;
const ID_FILE_EXIT: u16 = 1001;

/// Our cross-thread wake-up message. Producer threads `PostMessage`
/// this to drag the UI thread out of its `GetMessageW` idle and into
/// the `Shell::drain` path.
const WM_APP_WAKE: u32 = WM_APP + 1;

const MAIN_CLASS: PCWSTR = w!("CodePlusPlusMainWindow");
const SCINTILLA_CLASS: PCWSTR = w!("Scintilla");
const STATUSBAR_CLASS: PCWSTR = w!("msctls_statusbar32");

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

    // confirm_reload and show_error were intentionally removed from
    // UiPlatform: each runs `MessageBoxW` whose nested message pump
    // can re-enter the wnd_proc and produce aliasing UB on the
    // GWLP_USERDATA-borrowed WindowState. Modal dialogs are deferred
    // — `Shell::drain` returns `Vec<PendingDialog>` that the wnd_proc
    // shows after the borrow is dropped (see `WM_APP_WAKE`).
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
    let (mut doc, text_to_populate, encoding, eol, byte_len) = {
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

        // File menu: Save (Ctrl+S not wired to an accelerator yet —
        // Phase 3 brings the accelerator table) and Exit.
        let menubar = CreateMenu()?;
        let file_menu = CreateMenu()?;
        AppendMenuW(file_menu, MF_STRING, ID_FILE_SAVE as usize, w!("&Save"))?;
        AppendMenuW(file_menu, MF_STRING, ID_FILE_EXIT as usize, w!("E&xit"))?;
        AppendMenuW(menubar, MF_POPUP, file_menu.0 as usize, w!("&File"))?;

        // Plugins submenu placeholder. Empty until milestone 5 wires
        // the lazy-load + getFuncsArray flow that populates it. The
        // HMENU exists from startup so plugins that query
        // `NPPM_GETMENUHANDLE(NPPPLUGINMENU)` get a real handle to
        // append into rather than NULL — the submenu just shows up
        // empty in the UI until a plugin contributes items.
        let plugin_menu = CreateMenu()?;
        AppendMenuW(menubar, MF_POPUP, plugin_menu.0 as usize, w!("&Plugins"))?;

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
            editor,
            shell,
        });

        // Resolve the initial file:
        //   1. Explicit CLI argument wins (manual override / test path).
        //   2. Otherwise, restore the previously-open tab from session.xml.
        // Either way, queue a single open through the loader.
        {
            let path_to_open = initial_path.or_else(|| state.shell.load_session());
            if let Some(path) = path_to_open {
                state.shell.open_file(path);
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

        // Standard message loop.
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            match ret.0 {
                0 => break,
                -1 => return Err(windows::core::Error::from_thread()),
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
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
                    ID_FILE_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {
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
