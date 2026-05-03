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
use std::sync::Arc;

use codepp_core::{Encoding, Eol};
use codepp_editor::EditorHandle;
use codepp_plugin_host::{NPPMSG, NPPMSG_RANGE};
use codepp_scintilla_sys::{
    ScintillaDirectFunction, Scintilla_RegisterClasses, SCI_EMPTYUNDOBUFFER, SCI_GETCURRENTPOS,
    SCI_GETDIRECTFUNCTION, SCI_GETDIRECTPOINTER, SCI_GETLENGTH, SCI_GETTEXT, SCI_GOTOPOS,
    SCI_SETSAVEPOINT, SCI_SETTEXT,
};
use codepp_shell::{HostHandles, PendingDialog, Shell, UiPlatform};
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindowLongPtrW, LoadCursorW, MessageBoxW, MoveWindow,
    PostMessageW, PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, HMENU, IDC_ARROW,
    IDYES, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_YESNO, MF_POPUP, MF_STRING, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_DESTROY, WM_DROPFILES, WM_NCCREATE, WM_SETFOCUS,
    WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
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
    /// HMENU for the main menu bar — the parent of File/Edit/.../Plugins.
    /// Plugins query this via `NPPM_GETMENUHANDLE(NPPMAINMENU)` to
    /// install accelerator-bound items at the top level.
    main_menu: HMENU,
    /// HMENU for the per-plugin submenu under "Plugins". Plugins query
    /// this via `NPPM_GETMENUHANDLE(NPPPLUGINMENU)` to add their menu
    /// items. Empty until milestone 5 wires lazy-load + getFuncsArray
    /// integration; populated then.
    plugin_menu: HMENU,
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
/// `Shell::drain` needs to reach the editor and status bar. The main
/// HWND is intentionally absent: dialogs that need it are deferred
/// (`PendingDialog`) and shown by wnd_proc using its own HWND
/// parameter, so no Win32Ui method needs main_hwnd.
#[derive(Clone, Copy)]
struct Win32Ui {
    status_hwnd: HWND,
    editor: EditorHandle,
}

impl UiPlatform for Win32Ui {
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

        // Common controls — required for the status bar.
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
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

        // Heap-allocate the WindowState and stash its pointer in
        // GWLP_USERDATA. The Box is reclaimed in WM_DESTROY.
        let state = Box::new(WindowState {
            scintilla_hwnd,
            status_hwnd,
            main_menu: menubar,
            plugin_menu,
            editor,
            shell,
        });
        let state_ptr = Box::into_raw(state);
        SetWindowLongPtrW(main_hwnd, GWLP_USERDATA, state_ptr as isize);

        // Resolve the initial file:
        //   1. Explicit CLI argument wins (manual override / test path).
        //   2. Otherwise, restore the previously-open tab from session.xml.
        // Either way, queue a single open through the loader.
        {
            let state_mut = &mut *state_ptr;
            let path_to_open = initial_path.or_else(|| state_mut.shell.load_session());
            if let Some(path) = path_to_open {
                state_mut.shell.open_file(path);
            }
        }

        // Drag-drop: tell Windows our window accepts files.
        DragAcceptFiles(main_hwnd, true);

        // Show + size + focus.
        let _ = ShowWindow(main_hwnd, SW_SHOW);
        let mut rect = RECT::default();
        GetClientRect(main_hwnd, &mut rect)?;
        layout_children(scintilla_hwnd, status_hwnd, rect.right, rect.bottom);
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

/// Layout children: status bar at the bottom (auto-sized to its own
/// height), Scintilla fills the remainder.
///
/// SAFETY: caller must hold valid HWNDs for the three windows.
unsafe fn layout_children(scintilla: HWND, status: HWND, width: i32, height: i32) {
    // SB_GETBORDERS / status-bar height detection is a Phase 3 polish;
    // for now use a fixed 22 px which matches the Win32 default at
    // 96 DPI.
    let status_height = 22_i32;
    unsafe {
        let _ = MoveWindow(
            status,
            0,
            height - status_height,
            width,
            status_height,
            true,
        );
        let _ = MoveWindow(scintilla, 0, 0, width, height - status_height, true);
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
                let cmd = (wparam.0 & 0xFFFF) as u16;
                match cmd {
                    ID_FILE_SAVE => {
                        // Save inside the borrow; if it fails, capture
                        // the error message and show the dialog AFTER
                        // the borrow is dropped (same UB rule as
                        // WM_APP_WAKE).
                        let save_error: Option<String> = {
                            if let Some(state) = state_from_hwnd(hwnd) {
                                let (shell, mut ui) = state.split();
                                match shell.save_current_to_disk(&mut ui) {
                                    Ok(()) => {
                                        state.editor.send(SCI_SETSAVEPOINT, 0, 0);
                                        None
                                    }
                                    Err(e) => Some(e.to_string()),
                                }
                            } else {
                                None
                            }
                        };
                        if let Some(msg) = save_error {
                            show_error_dialog(hwnd, "Save failed", &msg);
                        }
                    }
                    ID_FILE_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let width = (lparam.0 & 0xFFFF) as i32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as i32;
                if let Some(state) = state_from_hwnd(hwnd) {
                    layout_children(state.scintilla_hwnd, state.status_hwnd, width, height);
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
                //
                // **Deferred (milestone 5):** fire `NPPN_SHUTDOWN` to
                // every loaded plugin before reclaiming the
                // WindowState. Outbound notify is held back until the
                // re-entrance design pass that pairs with the
                // example-hello plugin — a plugin's `beNotified` can
                // synchronously `SendMessage(NPPM_*)` back into our
                // wnd_proc, materializing a second `&mut WindowState`
                // from the same raw pointer. Inbound NPPM_* dispatch
                // (this commit) is safe because no foreign code runs
                // while we hold the borrow; outbound notify breaks
                // that invariant and needs separate hardening.
                if let Some(state) = state_from_hwnd(hwnd) {
                    let (shell, mut ui) = state.split();
                    if let Err(e) = shell.save_session(&mut ui) {
                        tracing::warn!(error = %e, "session save failed");
                    }
                }

                // Reclaim the WindowState box.
                let raw = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if raw != 0 {
                    let _ = Box::from_raw(raw as *mut WindowState);
                }
                // TODO(milestone-5): fire NPPN_SHUTDOWN here once the
                // re-entrance design pass lands (see WM_DESTROY block
                // comment above).
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
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// SAFETY: the returned reference borrows from the `Box<WindowState>`
/// stashed in `GWLP_USERDATA`. wnd_proc invocations are serialized
/// per-window (Win32 dispatches one at a time on the owning thread),
/// so concurrent mutable aliasing across messages cannot occur.
unsafe fn state_from_hwnd<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
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
