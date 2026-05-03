//! Win32 UI backend for Code++.
//!
//! Phase 1: a real Scintilla child window inside the main window. The
//! direct-call function pointer is captured via `SCI_GETDIRECTFUNCTION` /
//! `SCI_GETDIRECTPOINTER` immediately after creation and stored on the
//! `EditorHandle`. Scintilla itself handles every keystroke, the
//! undo/redo history, mouse selection, and the right-click context menu;
//! the main window's wnd_proc only handles menu commands plus
//! WM_SIZE / WM_SETFOCUS so the child fills the client area and keeps
//! input focus.
//!
//! See DESIGN.md §7.2 (Phase 1) and §4.2 (direct-call API).

#![cfg(target_os = "windows")]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    ScintillaDirectFunction, Scintilla_RegisterClasses, SCI_GETDIRECTFUNCTION,
    SCI_GETDIRECTPOINTER, SCI_GETLENGTH,
};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, LoadCursorW, MoveWindow, PostQuitMessage, RegisterClassExW,
    SendMessageW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW,
    MF_POPUP, MF_STRING, MSG, SW_SHOW, WINDOW_EX_STYLE, WM_COMMAND, WM_DESTROY, WM_SETFOCUS,
    WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const ID_FILE_EXIT: u16 = 1001;
const MAIN_CLASS: PCWSTR = w!("CodePlusPlusMainWindow");
const SCINTILLA_CLASS: PCWSTR = w!("Scintilla");

// The Scintilla child HWND is stashed in a process-wide atomic so the main
// window's wnd_proc can find it for WM_SIZE/WM_SETFOCUS forwarding. This
// is fine for Phase 1 (single window, single Scintilla control). Phase 3
// switches to per-window state via SetWindowLongPtr(GWLP_USERDATA) when
// multi-tab and potentially multi-window come online.
static SCINTILLA_HWND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Run the Code++ Win32 event loop. Blocks until the user exits.
pub fn run() -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;

        // Register Scintilla's window classes once for this process. Static-
        // linked Scintilla exposes Scintilla_RegisterClasses(HINSTANCE) for
        // exactly this purpose.
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

        // File → Exit menu.
        let menubar = CreateMenu()?;
        let file_menu = CreateMenu()?;
        AppendMenuW(file_menu, MF_STRING, ID_FILE_EXIT as usize, w!("E&xit"))?;
        AppendMenuW(menubar, MF_POPUP, file_menu.0 as usize, w!("&File"))?;

        // Create the main top-level window.
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

        // Create the Scintilla child window inside the main window's client
        // area. Initial size is 0,0 — WM_SIZE arrives during ShowWindow and
        // will resize it to fill the client area.
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
        SCINTILLA_HWND.store(scintilla_hwnd.0, Ordering::Release);

        // Capture the direct-call (fn_ptr, instance_ptr) pair right after
        // construction. Every per-keystroke operation will go through this
        // pair instead of round-tripping the Win32 message queue.
        let direct_fn_lr = SendMessageW(scintilla_hwnd, SCI_GETDIRECTFUNCTION, None, None);
        let direct_ptr_lr = SendMessageW(scintilla_hwnd, SCI_GETDIRECTPOINTER, None, None);
        // Scintilla documents both messages as never returning null on a
        // valid Scintilla control. A zero here means our window-class
        // registration silently produced something that isn't a real
        // Scintilla — bail before transmuting nonsense to a fn pointer.
        if direct_fn_lr.0 == 0 || direct_ptr_lr.0 == 0 {
            return Err(windows::core::Error::new(
                E_FAIL,
                "Scintilla returned null SCI_GETDIRECTFUNCTION/SCI_GETDIRECTPOINTER",
            ));
        }
        let direct_fn: ScintillaDirectFunction = std::mem::transmute(direct_fn_lr.0);
        let direct_ptr = direct_ptr_lr.0 as *mut c_void;
        let editor = EditorHandle::new(scintilla_hwnd.0, direct_fn, direct_ptr);
        // Exercise the direct-call path once during init so the Phase 1 demo
        // proves end-to-end that the (fn_ptr, instance_ptr) pair we captured
        // actually reaches Scintilla. A freshly-created control must report
        // length 0; anything else means we wired the wrong control.
        debug_assert_eq!(
            editor.send(SCI_GETLENGTH, 0, 0),
            0,
            "fresh Scintilla control reports non-zero length via direct-call"
        );
        // Phase 2 stashes `editor` on per-tab state and drives editor
        // operations through it. Phase 1's user-visible demo (typing,
        // undo/redo, select-all, context menu) is handled by Scintilla
        // internally; the handle goes out of scope here on purpose.
        let _ = editor;

        // Show the main window and force the Scintilla child to fill it.
        let _ = ShowWindow(main_hwnd, SW_SHOW);
        let mut rect = RECT::default();
        GetClientRect(main_hwnd, &mut rect)?;
        let _ = MoveWindow(scintilla_hwnd, 0, 0, rect.right, rect.bottom, true);
        let _ = SetFocus(Some(scintilla_hwnd));

        // Standard Win32 message loop. GetMessageW returns 0 on WM_QUIT,
        // -1 on error, positive on a normal message.
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

extern "system" fn main_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let cmd = (wparam.0 & 0xFFFF) as u16;
                if cmd == ID_FILE_EXIT {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let scintilla = SCINTILLA_HWND.load(Ordering::Acquire);
                if !scintilla.is_null() {
                    // lparam packs (LOWORD = client width, HIWORD = height).
                    let width = (lparam.0 & 0xFFFF) as i32;
                    let height = ((lparam.0 >> 16) & 0xFFFF) as i32;
                    let _ = MoveWindow(HWND(scintilla), 0, 0, width, height, true);
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                // Forward focus to the Scintilla child so keystrokes go to
                // the editor immediately.
                let scintilla = SCINTILLA_HWND.load(Ordering::Acquire);
                if !scintilla.is_null() {
                    let _ = SetFocus(Some(HWND(scintilla)));
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // Win32 destroys child windows before the parent receives
                // WM_DESTROY, so the HWND in SCINTILLA_HWND is dead by now.
                // Null it so any late message (or a future tab-lifecycle
                // path that destroys the child without exiting) cannot fire
                // MoveWindow/SetFocus on a freed handle.
                SCINTILLA_HWND.store(std::ptr::null_mut(), Ordering::Release);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
