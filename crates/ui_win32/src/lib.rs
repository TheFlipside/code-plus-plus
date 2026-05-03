//! Win32 UI backend for Code++.
//!
//! Phase 0: opens an empty top-level window with a `File → Exit` menu.
//! No Scintilla yet — that lands in Phase 1. See DESIGN.md §7.2.

#![cfg(target_os = "windows")]

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, LoadCursorW, PostQuitMessage, RegisterClassExW, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, MF_POPUP, MF_STRING, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WM_COMMAND, WM_DESTROY, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

const ID_FILE_EXIT: u16 = 1001;
const CLASS_NAME: PCWSTR = w!("CodePlusPlusMainWindow");

/// Run the Code++ Win32 event loop. Blocks until the user exits.
pub fn run() -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut core::ffi::c_void),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        let atom = RegisterClassExW(&wnd_class);
        if atom == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let menubar = CreateMenu()?;
        let file_menu = CreateMenu()?;
        AppendMenuW(file_menu, MF_STRING, ID_FILE_EXIT as usize, w!("E&xit"))?;
        AppendMenuW(menubar, MF_POPUP, file_menu.0 as usize, w!("&File"))?;

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS_NAME,
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

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            match ret.0 {
                0 => break,                                            // WM_QUIT
                -1 => return Err(windows::core::Error::from_thread()), // error
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        Ok(())
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let cmd = (wparam.0 & 0xFFFF) as u16;
                if cmd == ID_FILE_EXIT {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
