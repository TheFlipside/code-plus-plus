//! File → Print Preview — modal window that renders each page onto a
//! scaled-down memory bitmap so the user can page through their
//! document before committing to the printer.
//!
//! High-level flow, matching Notepad++'s File → Print Preview:
//!
//! 1. Query the default printer via `PrintDlgW(PD_RETURNDEFAULT |
//!    PD_RETURNDC)` — same API `handle_print` uses on user click, but
//!    with `PD_RETURNDEFAULT` so no dialog UI shows. Returns the
//!    default printer's HDC (`PrintJob` in the [`crate::print`]
//!    module owns its lifetime via `Drop`).
//! 2. Apply Scintilla's print-mode settings via
//!    [`crate::print::configure_scintilla_for_print`] so the on-
//!    screen preview uses the same paint discipline as the actual
//!    print (colour-on-white paper, wrap-mode word).
//! 3. Measure the page-break table against the printer HDC via
//!    [`crate::print::measure_page_breaks`] — same two-pass discipline
//!    as `handle_print` — so navigation shows the real page count.
//! 4. Register a `WNDCLASSEXW` (`PRINT_PREVIEW_CLASS`) on first
//!    invocation and create a modal `WS_POPUP` window sized to a
//!    generous fraction of the owner window. Toolbar strip at top
//!    (First / Prev / Page indicator / Next / Last / Print / Close);
//!    scaled preview of the current page fills the rest of the client
//!    area.
//! 5. Own a nested `GetMessageW` pump — the pattern the About dialog
//!    uses — with an `IsWindow` termination check and RAII guards for
//!    owner re-enable + dialog destroy.
//! 6. `WM_PAINT` renders the current page: create a screen-compatible
//!    mem DC, set up an anisotropic mapping (window ext = printer
//!    page pixels, viewport ext = preview area pixels), fill white,
//!    draw the header via [`crate::print::draw_page_header`], call
//!    `SCI_FORMATRANGEFULL` in draw mode with `hdc = mem DC` and
//!    `hdc_target = printer HDC`, then blit the mem DC to the window.
//!
//! Cross-platform note: Win32-only for now (Phase 5 wires GTK / Cocoa
//! preview windows alongside the rest of the print backend — the
//! scale-and-render math is Scintilla-agnostic and ports directly).

use core::ffi::c_void;
use std::sync::OnceLock;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::SCI_GETLENGTH;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EndPaint, FillRect, GetDC, GetStockObject, InvalidateRect, ReleaseDC, RestoreDC, SaveDC,
    SelectObject, SetMapMode, SetViewportExtEx, SetViewportOrgEx, SetWindowExtEx, HBRUSH, HDC,
    HGDIOBJ, MM_ANISOTROPIC, PAINTSTRUCT, SRCCOPY, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, SetFocus, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, IsDialogMessageW, IsWindow, LoadCursorW,
    PostMessageW, RegisterClassExW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateMessage, BN_CLICKED, BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, HMENU, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE,
    SWP_NOOWNERZORDER, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_NCCREATE, WM_PAINT, WM_QUIT, WM_SIZE,
    WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME,
    WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_THICKFRAME, WS_VISIBLE,
};

use crate::print::{
    configure_scintilla_for_print, default_printer_dc, draw_page_header, format_today,
    measure_page_breaks, release_format_cache, render_one_page, PaperMetrics,
};
use crate::{dialog_bg_brush, show_error_dialog, DlgDestroyGuard, OwnerEnableGuard};

// -------------------------------------------------------------------
// Constants
// -------------------------------------------------------------------

const PRINT_PREVIEW_CLASS: PCWSTR = w!("CodePP_PrintPreview");

// Toolbar button command IDs (child window IDs, WM_COMMAND). Chosen
// outside the main-menu ID space so a stray dispatch never confuses
// the main wnd_proc.
const IDC_PREVIEW_FIRST: u16 = 4001;
const IDC_PREVIEW_PREV: u16 = 4002;
const IDC_PREVIEW_NEXT: u16 = 4003;
const IDC_PREVIEW_LAST: u16 = 4004;
const IDC_PREVIEW_PRINT: u16 = 4005;
const IDC_PREVIEW_CLOSE: u16 = 4006;

// Layout constants — all in device-independent pixels at 96 DPI.
const TOOLBAR_HEIGHT: i32 = 40;
const BUTTON_H: i32 = 28;
const BUTTON_W: i32 = 80;
const BUTTON_PAD: i32 = 6;
const CONTENT_PAD: i32 = 12;

/// Sizes for the small nav buttons (First / Prev / Next / Last).
const NAV_BUTTON_W: i32 = 32;

/// Portion of the owner window's inner dimensions the preview takes
/// on open. Users can drag the resize handle to change it afterward
/// — the window is `WS_THICKFRAME`.
const OWNER_FRACTION_W: i32 = 8; // 80% -> 8/10
const OWNER_FRACTION_H: i32 = 8;

// -------------------------------------------------------------------
// State
// -------------------------------------------------------------------

/// All state a preview window needs to render + navigate. Heap-owned
/// so the raw pointer we stash in `GWLP_USERDATA` stays valid across
/// the modal pump's lifetime.
struct PreviewState {
    /// Printer DC obtained via `PrintDlgW(PD_RETURNDEFAULT |
    /// PD_RETURNDC)`. Used as `hdc_target` for `SCI_FORMATRANGEFULL`
    /// so font metrics match what the real print will produce.
    /// Owned; `DeleteDC` on close via `Drop`.
    printer_hdc: HDC,
    /// Cached printer-paper geometry (page + text + header rects,
    /// vertical DPI). Computed once at open time.
    paper: PaperMetrics,
    /// Page-break table — one `(cp_min, cp_max)` per page. Length
    /// = total pages. Computed once via measure pass.
    page_breaks: Vec<(isize, isize)>,
    /// Current page index (0-based) shown in the preview.
    current_page: usize,
    /// Copy of the active editor handle so we can call
    /// `SCI_FORMATRANGEFULL` for on-demand re-renders.
    editor: EditorHandle,
    /// Display name of the document — passed to
    /// [`draw_page_header`] for every page.
    doc_name: String,
    /// Cached "YYYY-MM-DD" string for header rendering. Snapped
    /// once at open time so all pages carry the same date even
    /// if the clock rolls midnight mid-print.
    today: String,
    /// Child HWNDs for the toolbar buttons — kept so a resize can
    /// reposition them. Zero-init'd; populated during `WM_CREATE`.
    btn_first: HWND,
    btn_prev: HWND,
    btn_next: HWND,
    btn_last: HWND,
    btn_print: HWND,
    btn_close: HWND,
    /// Text label showing "Page X of M". Also repositioned on
    /// resize.
    lbl_page: HWND,
    /// Was the user's action to open the OS print dialog after
    /// closing the preview? Preview → Print button sets this so
    /// the outer `show_print_preview` caller can chain into the
    /// normal print flow without racing against the modal pump.
    print_after_close: bool,
}

impl Drop for PreviewState {
    fn drop(&mut self) {
        // Release Scintilla's per-print cache and clean up the
        // printer DC we own. Symmetric with the real print path in
        // `handle_print` — the same discipline keeps Scintilla's
        // internal surface caches from leaking across successive
        // preview-open sessions.
        release_format_cache(&self.editor);
        if !self.printer_hdc.is_invalid() {
            // SAFETY: `printer_hdc` was returned by our own
            // `default_printer_dc` call (which wraps `PrintDlgW`
            // with `PD_RETURNDC`); we own its lifetime uniquely.
            unsafe {
                let _ = DeleteDC(self.printer_hdc);
            }
        }
    }
}

// -------------------------------------------------------------------
// Public entry
// -------------------------------------------------------------------

/// Show the Print Preview modal against `owner`. Snapshots the
/// active editor's document via a printer-DC measure pass, then
/// pumps its own message loop until the user closes the window (or
/// clicks Print, which closes the preview and re-enters the normal
/// [`crate::print::print_active_document`] pipeline against `owner`).
///
/// No-op if:
///   * The active document is empty (nothing to preview).
///   * The default printer DC can't be created (usually "no
///     printer installed" — surfaces an error dialog and returns).
///   * `SCI_FORMATRANGEFULL`'s measure pass yields zero pages
///     (extremely defensive).
///
/// Same `&mut WindowState`-lifetime discipline as the print handler:
/// the caller (`handle_print_preview` in `lib.rs`) snapshots the
/// display name + `EditorHandle` copy under a brief `&mut
/// WindowState` borrow and drops it before invoking us, because the
/// nested pump inside can dispatch arbitrary window messages back
/// through the main `wnd_proc`.
pub(crate) fn show_print_preview(owner: HWND, doc_display_name: &str, editor: EditorHandle) {
    let text_length = editor.send(SCI_GETLENGTH, 0, 0);
    if text_length <= 0 {
        return;
    }

    // 1. Default printer DC.
    let Some(printer_hdc) = default_printer_dc() else {
        show_error_dialog(
            owner,
            "Print Preview",
            "No printer is installed, or the default printer could not be opened.",
        );
        return;
    };

    // Scoped auto-cleanup for the printer DC in the pre-window
    // failure branches below — before ownership transfers to
    // `PreviewState`. Once the state is boxed, its own `Drop` takes
    // over.
    struct PrinterDcGuard(HDC);
    impl Drop for PrinterDcGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = DeleteDC(self.0);
                }
            }
        }
    }
    let mut printer_guard = PrinterDcGuard(printer_hdc);

    // 2. Scintilla print settings + paper metrics + measure pass.
    configure_scintilla_for_print(&editor);
    let paper = PaperMetrics::from_hdc(printer_hdc);
    let (page_breaks, _truncation) = measure_page_breaks(&editor, &paper, text_length, printer_hdc);
    if page_breaks.is_empty() {
        // No pages measurable — release the DC and bail.
        return;
    }

    // 3. Build the state box. Transfer HDC ownership from the
    //    stack guard to the state.
    let mut state = Box::new(PreviewState {
        printer_hdc,
        paper,
        page_breaks,
        current_page: 0,
        editor,
        doc_name: doc_display_name.to_string(),
        today: format_today(),
        btn_first: HWND::default(),
        btn_prev: HWND::default(),
        btn_next: HWND::default(),
        btn_last: HWND::default(),
        btn_print: HWND::default(),
        btn_close: HWND::default(),
        lbl_page: HWND::default(),
        print_after_close: false,
    });
    // Prevent the stack guard from double-freeing the DC — state
    // now owns it.
    printer_guard.0 = HDC::default();
    drop(printer_guard);

    // 4. Create + pump the modal window.
    //
    // Pass the state as a raw pointer, not `&mut`, to avoid
    // holding a `&mut PreviewState` alive across the pump — see
    // `run_modal`'s doc for why (Stacked-Borrows aliasing with the
    // wnd_proc's independently-materialised `&mut`).
    let state_ptr: *mut PreviewState = std::ptr::from_mut::<PreviewState>(&mut state);
    // SAFETY: `state` is the local `Box` from step 3; `state_ptr`
    // is valid for the whole call. We do NOT touch `state` between
    // constructing `state_ptr` and `run_modal` returning.
    let print_after_close = unsafe { run_modal(owner, state_ptr) };

    // 5. If the user clicked Print (not Close / Esc), re-enter the
    //    normal print pipeline against the owner window. The
    //    editor handle + doc name are borrowed from `state`, which
    //    is still alive here. We also thread the measured page
    //    count as a hint into `PrintDlgW`'s spinner bounds so the
    //    user can pick a page range against real numbers instead
    //    of the fallback 1..65535 window.
    if print_after_close {
        let editor_copy = state.editor;
        let doc = state.doc_name.clone();
        let total_pages = state.page_breaks.len();
        // Drop the preview state (deletes the printer DC + releases
        // Scintilla's format cache) BEFORE
        // `print_active_document_with_page_hint` reacquires its own
        // printer DC via the user-facing `PrintDlgW`. Not required
        // for correctness — the two DCs could coexist — but keeps
        // the resource ownership picture straightforward.
        drop(state);
        crate::print::print_active_document_with_page_hint(owner, &doc, editor_copy, total_pages);
    }
}

// `default_printer_dc` moved to `crate::print` so it can be shared
// with the "Print Now" no-dialog print path. Re-imported at the top
// of this file.

// -------------------------------------------------------------------
// Modal window creation + pump
// -------------------------------------------------------------------

/// Register the window class (once), create the popup, run its
/// nested `GetMessageW` pump. Returns `true` iff the user clicked
/// the Print button (so the caller can chain into the real print
/// pipeline); `false` on any other exit (Close, Esc, X, `WM_QUIT`).
///
/// Takes the state as a raw `*mut PreviewState` (not `&mut`) — the
/// pump dispatches messages that let `preview_wnd_proc` reach the
/// same allocation independently via `GWLP_USERDATA` and materialise
/// its own `&mut PreviewState` inside `state_from_hwnd`. Holding a
/// `&mut PreviewState` here across the pump would give two live
/// aliasing `&mut` references to the same allocation — a Stacked-
/// Borrows violation even though the accesses never overlap in
/// practice. Sticking to a raw pointer through the pump and only
/// re-deriving a `&mut` once, after the pump exits, avoids the UB.
///
/// # Safety
///
/// * `state` must be a valid, uniquely-owned `*mut PreviewState` for
///   the duration of the modal (typically pointing at a `Box` in the
///   caller's stack frame).
/// * The caller must NOT dereference or otherwise access `*state`
///   for the whole modal lifetime — the `wnd_proc` has exclusive
///   access via `GWLP_USERDATA`.
unsafe fn run_modal(owner: HWND, state: *mut PreviewState) -> bool {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return false;
        };
        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(preview_wnd_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: dialog_bg_brush(),
                lpszClassName: PRINT_PREVIEW_CLASS,
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
        });

        // Sizing: 80% of the owner's outer rect, clamped so the
        // window still fits on the primary monitor's work area.
        let mut owner_rect = RECT::default();
        let _ = GetWindowRect(owner, &raw mut owner_rect);
        let owner_w = owner_rect.right - owner_rect.left;
        let owner_h = owner_rect.bottom - owner_rect.top;
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let mut win_w = (owner_w * OWNER_FRACTION_W / 10).max(720).min(screen_w);
        let mut win_h = (owner_h * OWNER_FRACTION_H / 10).max(600).min(screen_h);
        // Clamp to screen work area (below title bar / above taskbar
        // is an approximation via SM_CY* — good enough for the
        // opening frame; user can resize).
        if win_w > screen_w {
            win_w = screen_w;
        }
        if win_h > screen_h {
            win_h = screen_h;
        }
        let win_x = owner_rect.left + (owner_w - win_w) / 2;
        let win_y = owner_rect.top + (owner_h - win_h) / 2;

        let style =
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_THICKFRAME | WS_CLIPCHILDREN;

        let dlg = match CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PRINT_PREVIEW_CLASS,
            w!("Print Preview"),
            style,
            win_x,
            win_y,
            win_w,
            win_h,
            Some(owner),
            None,
            Some(instance.into()),
            Some(state.cast::<c_void>()),
        ) {
            Ok(h) => h,
            Err(_) => return false,
        };
        let _dlg_guard = DlgDestroyGuard(dlg);

        // Disable the owner window for the modal lifetime — user
        // can't switch focus back to the main window until they
        // dismiss the preview. `OwnerEnableGuard` re-enables on
        // every exit path (including panic).
        let _owner_guard = OwnerEnableGuard(owner);
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(dlg));

        let mut msg = MSG::default();
        loop {
            if !IsWindow(Some(dlg)).as_bool() {
                break;
            }
            let ret = GetMessageW(&raw mut msg, None, 0, 0);
            match ret.0 {
                0 => {
                    // WM_QUIT — repost so the outer message pump
                    // sees it, then bail.
                    let _ = PostMessageW(None, WM_QUIT, msg.wParam, msg.lParam);
                    break;
                }
                -1 => break,
                _ => {
                    if !IsDialogMessageW(dlg, &raw const msg).as_bool() {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }
            }
        }
        // Read the print-after-close flag through a *fresh* &mut
        // materialized only after the pump has exited (so no
        // aliasing with any &mut derived inside the wnd_proc via
        // `state_from_hwnd` — the wnd_proc for this window is
        // dead the moment `IsWindow(dlg) == false`).
        //
        // SAFETY: `state` is still valid and uniquely owned by
        // the caller (the wnd_proc's access rights ended when the
        // pump broke out on `!IsWindow(dlg)`, and `WM_DESTROY`
        // already cleared `GWLP_USERDATA` so no further wnd_proc
        // callback can reach the state).
        (*state).print_after_close
    }
}

// -------------------------------------------------------------------
// WndProc
// -------------------------------------------------------------------

/// State-pointer accessor. Returns `None` before `WM_NCCREATE`
/// stashes the pointer, so early messages fall through to
/// `DefWindowProcW`.
unsafe fn state_from_hwnd<'a>(hwnd: HWND) -> Option<&'a mut PreviewState> {
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewState;
        raw.as_mut()
    }
}

unsafe extern "system" fn preview_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                // Store the state-box pointer we passed as
                // `CreateWindowExW`'s `lpparam` into `GWLP_USERDATA`
                // so subsequent messages can reach it.
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CREATE => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    create_child_controls(hwnd, state);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    layout_children(hwnd, state);
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                // Skip default erase — we paint every pixel in
                // `paint_preview` via a mem-DC-and-BitBlt double
                // buffer, so a redundant erase would flicker.
                LRESULT(1)
            }
            WM_PAINT => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    paint_preview(hwnd, state);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    handle_key(hwnd, state, u32::try_from(wparam.0).unwrap_or(0));
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if let Some(state) = state_from_hwnd(hwnd) {
                    let cmd_id = (wparam.0 & 0xffff) as u16;
                    let notify = ((wparam.0 >> 16) & 0xffff) as u16;
                    if notify == BN_CLICKED as u16 {
                        handle_command(hwnd, state, cmd_id);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Detach the state pointer so a stray late message
                // doesn't UAF. The Box itself is freed by the outer
                // caller's stack frame.
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// -------------------------------------------------------------------
// Child controls (toolbar buttons + page label)
// -------------------------------------------------------------------

unsafe fn create_child_controls(hwnd: HWND, state: &mut PreviewState) {
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return;
        };
        let btn_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32);
        let label_style = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(crate::SS_CENTER);

        let mk = |id: u16, label: PCWSTR, style: WINDOW_STYLE, class: PCWSTR| -> HWND {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                label,
                style,
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(id as usize as *mut c_void)),
                Some(instance.into()),
                None,
            )
            .unwrap_or_default()
        };

        state.btn_first = mk(IDC_PREVIEW_FIRST, w!("|<"), btn_style, w!("BUTTON"));
        state.btn_prev = mk(IDC_PREVIEW_PREV, w!("<"), btn_style, w!("BUTTON"));
        state.lbl_page = mk(0, w!("Page 1 of 1"), label_style, w!("STATIC"));
        state.btn_next = mk(IDC_PREVIEW_NEXT, w!(">"), btn_style, w!("BUTTON"));
        state.btn_last = mk(IDC_PREVIEW_LAST, w!(">|"), btn_style, w!("BUTTON"));
        state.btn_print = mk(IDC_PREVIEW_PRINT, w!("Print..."), btn_style, w!("BUTTON"));
        state.btn_close = mk(IDC_PREVIEW_CLOSE, w!("Close"), btn_style, w!("BUTTON"));

        layout_children(hwnd, state);
        update_page_label(state);
    }
}

/// Position every child control based on the window's current
/// client area. Called from `WM_CREATE` and every `WM_SIZE`.
unsafe fn layout_children(hwnd: HWND, state: &PreviewState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &raw mut rc);
        let w = rc.right - rc.left;

        // Left cluster: |< | < | label | > | >|
        let mut x = CONTENT_PAD;
        let y = (TOOLBAR_HEIGHT - BUTTON_H) / 2;
        move_child(state.btn_first, x, y, NAV_BUTTON_W, BUTTON_H);
        x += NAV_BUTTON_W + BUTTON_PAD;
        move_child(state.btn_prev, x, y, NAV_BUTTON_W, BUTTON_H);
        x += NAV_BUTTON_W + BUTTON_PAD;
        // Page label: fixed width, allow ~120 dip.
        let label_w = 120;
        move_child(state.lbl_page, x, y + 4, label_w, BUTTON_H - 8);
        x += label_w + BUTTON_PAD;
        move_child(state.btn_next, x, y, NAV_BUTTON_W, BUTTON_H);
        x += NAV_BUTTON_W + BUTTON_PAD;
        move_child(state.btn_last, x, y, NAV_BUTTON_W, BUTTON_H);

        // Right cluster: [Close] [Print...]
        let mut xr = w - CONTENT_PAD - BUTTON_W;
        move_child(state.btn_close, xr, y, BUTTON_W, BUTTON_H);
        xr -= BUTTON_W + BUTTON_PAD;
        move_child(state.btn_print, xr, y, BUTTON_W, BUTTON_H);
    }
}

unsafe fn move_child(child: HWND, x: i32, y: i32, w: i32, h: i32) {
    if child.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            child,
            None,
            x,
            y,
            w,
            h,
            SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
        );
    }
}

// -------------------------------------------------------------------
// Navigation
// -------------------------------------------------------------------

unsafe fn handle_command(hwnd: HWND, state: &mut PreviewState, id: u16) {
    let total = state.page_breaks.len();
    let mut changed = false;
    match id {
        IDC_PREVIEW_FIRST if state.current_page != 0 => {
            state.current_page = 0;
            changed = true;
        }
        IDC_PREVIEW_PREV if state.current_page > 0 => {
            state.current_page -= 1;
            changed = true;
        }
        IDC_PREVIEW_NEXT if state.current_page + 1 < total => {
            state.current_page += 1;
            changed = true;
        }
        IDC_PREVIEW_LAST if state.current_page + 1 < total => {
            state.current_page = total.saturating_sub(1);
            changed = true;
        }
        IDC_PREVIEW_PRINT => {
            state.print_after_close = true;
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }
        IDC_PREVIEW_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }
        _ => {}
    }
    if changed {
        // SAFETY: `state.lbl_page` is a valid HWND we created in
        // `create_child_controls` and haven't destroyed. `InvalidateRect`
        // on the preview HWND is a plain refresh request.
        unsafe {
            update_page_label(state);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

unsafe fn handle_key(hwnd: HWND, state: &mut PreviewState, vk: u32) {
    let id = match vk {
        v if v == VK_ESCAPE.0 as u32 => IDC_PREVIEW_CLOSE,
        v if v == VK_RETURN.0 as u32 => IDC_PREVIEW_PRINT,
        v if v == VK_LEFT.0 as u32 || v == VK_PRIOR.0 as u32 => IDC_PREVIEW_PREV,
        v if v == VK_RIGHT.0 as u32 || v == VK_NEXT.0 as u32 => IDC_PREVIEW_NEXT,
        v if v == VK_HOME.0 as u32 => IDC_PREVIEW_FIRST,
        v if v == VK_END.0 as u32 => IDC_PREVIEW_LAST,
        _ => return,
    };
    unsafe {
        handle_command(hwnd, state, id);
    }
}

unsafe fn update_page_label(state: &PreviewState) {
    if state.lbl_page.is_invalid() {
        return;
    }
    let text = format!(
        "Page {} of {}",
        state.current_page + 1,
        state.page_breaks.len()
    );
    let wide: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
    unsafe {
        let _ = SetWindowTextW(state.lbl_page, PCWSTR(wide.as_ptr()));
    }
}

// -------------------------------------------------------------------
// Paint — render current page onto the preview area
// -------------------------------------------------------------------

/// Compute the largest rectangle inside `content_rect` that
/// preserves the page's aspect ratio. Pure function so the layout
/// math is unit-testable in isolation.
///
/// Returns `(x, y, w, h)` in device pixels relative to
/// `content_rect`'s top-left. If either input dimension is zero,
/// returns a zero-sized rectangle at the origin — nothing crashes,
/// but `paint_preview` skips the render.
#[must_use]
pub(crate) fn preview_page_rect(
    content_w: i32,
    content_h: i32,
    page_w: i32,
    page_h: i32,
) -> (i32, i32, i32, i32) {
    if content_w <= 0 || content_h <= 0 || page_w <= 0 || page_h <= 0 {
        return (0, 0, 0, 0);
    }
    // Fit page into content, aspect-ratio preserved.
    let page_ratio = f64::from(page_w) / f64::from(page_h);
    let content_ratio = f64::from(content_w) / f64::from(content_h);
    let (fit_w, fit_h) = if content_ratio > page_ratio {
        // Content is wider than the page — height-bound.
        let h = content_h;
        let w = (f64::from(h) * page_ratio).round() as i32;
        (w, h)
    } else {
        // Content is narrower or equal — width-bound.
        let w = content_w;
        let h = (f64::from(w) / page_ratio).round() as i32;
        (w, h)
    };
    let x = (content_w - fit_w) / 2;
    let y = (content_h - fit_h) / 2;
    (x, y, fit_w, fit_h)
}

unsafe fn paint_preview(hwnd: HWND, state: &PreviewState) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dst_dc = BeginPaint(hwnd, &raw mut ps);
        if dst_dc.is_invalid() {
            return;
        }
        // Fill the whole client area with the dialog background
        // so the toolbar strip + margins pick up the chrome
        // colour. `dialog_bg_brush` is the same brush every other
        // modal uses.
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &raw mut client);
        let _ = FillRect(dst_dc, &raw const client, dialog_bg_brush());

        // Content area: below the toolbar strip, with padding.
        let content_left = CONTENT_PAD;
        let content_top = TOOLBAR_HEIGHT + CONTENT_PAD;
        let content_right = (client.right - CONTENT_PAD).max(content_left);
        let content_bottom = (client.bottom - CONTENT_PAD).max(content_top);
        let content_w = content_right - content_left;
        let content_h = content_bottom - content_top;

        // Aspect-ratio-preserving preview page rect.
        let (px, py, pw, ph) = preview_page_rect(
            content_w,
            content_h,
            state.paper.page_rect.right,
            state.paper.page_rect.bottom,
        );
        if pw <= 0 || ph <= 0 {
            let _ = EndPaint(hwnd, &raw const ps);
            return;
        }

        // Render the current page into a screen-compatible mem DC
        // sized to `(pw, ph)`. Anisotropic mapping scales printer
        // coordinates down to preview pixels.
        let screen_dc = GetDC(Some(hwnd));
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let mem_bmp = CreateCompatibleBitmap(screen_dc, pw, ph);
        let _ = ReleaseDC(Some(hwnd), screen_dc);
        if mem_dc.is_invalid() || mem_bmp.is_invalid() {
            if !mem_bmp.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
            }
            if !mem_dc.is_invalid() {
                let _ = DeleteDC(mem_dc);
            }
            let _ = EndPaint(hwnd, &raw const ps);
            return;
        }
        let old_bmp = SelectObject(mem_dc, HGDIOBJ(mem_bmp.0));

        // White paper background for the whole preview page.
        let white_rc = RECT {
            left: 0,
            top: 0,
            right: pw,
            bottom: ph,
        };
        let white_brush = GetStockObject(WHITE_BRUSH);
        let _ = FillRect(mem_dc, &raw const white_rc, HBRUSH(white_brush.0));

        // Set anisotropic mapping so Scintilla / draw_page_header
        // can address the printer's full page rectangle and get
        // scaled into the (pw, ph) preview.
        let saved = SaveDC(mem_dc);
        let _ = SetMapMode(mem_dc, MM_ANISOTROPIC);
        // Window extent = printer page (logical), viewport =
        // preview bitmap (device). No SetWindowOrgEx / SetViewportOrgEx
        // needed — both origins stay at (0, 0).
        let _ = SetWindowExtEx(
            mem_dc,
            state.paper.page_rect.right,
            state.paper.page_rect.bottom,
            None,
        );
        let _ = SetViewportExtEx(mem_dc, pw, ph, None);
        let _ = SetViewportOrgEx(mem_dc, 0, 0, None);

        // Draw the header, then the page's text range.
        let (cp_min, cp_max) = state.page_breaks[state.current_page];
        draw_page_header(
            mem_dc,
            &state.paper,
            &state.doc_name,
            &state.today,
            state.current_page + 1,
            state.page_breaks.len(),
        );
        // Preview: draw into the screen-compatible mem DC (so we
        // can BitBlt it onto the window), but measure font metrics
        // against the printer HDC — otherwise Scintilla would use
        // screen line-heights (~15 px at 96 dpi) inside a rect
        // sized in printer units (~5700 units tall for A4), so
        // each page would fill only the top ~10% of the preview.
        // See `render_one_page`'s doc for the full mechanism.
        render_one_page(
            &state.editor,
            mem_dc,
            state.printer_hdc,
            &state.paper,
            cp_min,
            cp_max,
        );

        // Restore the DC's mapping mode before blit.
        if saved != 0 {
            let _ = RestoreDC(mem_dc, saved);
        }

        // Blit the mem DC onto the target — no stretch (mem DC
        // already sized to the preview page).
        let _ = BitBlt(
            dst_dc,
            content_left + px,
            content_top + py,
            pw,
            ph,
            Some(mem_dc),
            0,
            0,
            SRCCOPY,
        );

        // Cleanup.
        let _ = SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
        let _ = DeleteDC(mem_dc);
        let _ = EndPaint(hwnd, &raw const ps);
    }
}

// -------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_page_rect_letter_portrait_into_wide_content() {
        // Content is wide enough that the fit is height-bound.
        // 8.5 x 11 page, content 800 x 400.
        let (x, y, w, h) = preview_page_rect(800, 400, 5100, 6600);
        // Height-bound: h = 400, w = 400 * (5100/6600) ≈ 309.
        assert_eq!(h, 400);
        assert!(w > 300 && w < 320);
        // Centred horizontally.
        assert!(x > 0);
        assert_eq!(y, 0);
    }

    #[test]
    fn preview_page_rect_letter_portrait_into_tall_content() {
        // Content narrower relative to page — width-bound.
        // 8.5 x 11 page, content 300 x 800.
        let (x, y, w, h) = preview_page_rect(300, 800, 5100, 6600);
        // Width-bound: w = 300, h = 300 * (6600/5100) ≈ 388.
        assert_eq!(w, 300);
        assert!(h > 380 && h < 400);
        assert_eq!(x, 0);
        assert!(y > 0);
    }

    #[test]
    fn preview_page_rect_zero_dims_yield_empty_rect() {
        assert_eq!(preview_page_rect(0, 100, 5100, 6600), (0, 0, 0, 0));
        assert_eq!(preview_page_rect(100, 0, 5100, 6600), (0, 0, 0, 0));
        assert_eq!(preview_page_rect(100, 100, 0, 6600), (0, 0, 0, 0));
        assert_eq!(preview_page_rect(100, 100, 5100, 0), (0, 0, 0, 0));
    }

    #[test]
    fn preview_page_rect_square_content_and_page_covers_exactly() {
        let (x, y, w, h) = preview_page_rect(500, 500, 1000, 1000);
        assert_eq!((x, y, w, h), (0, 0, 500, 500));
    }
}
