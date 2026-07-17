//! Settings → Preferences… dialog.
//!
//! N++-style modal Preferences window built on the same custom
//! `WS_POPUP` + `WS_CAPTION` scaffolding as the About / Print
//! Preview / UDL editor dialogs. Layout matches N++'s
//! Preferences box: left-side category listbox + right-side
//! per-category panel + bottom Close button.
//!
//! Today the dialog ships exactly one category — **Recent Files
//! History** — since that's the only pane wired into live UI so
//! far. Every other N++ Preferences pane (General, Toolbar, Tab
//! Bar, Editing 1/2, Dark Mode, Margins/Border/Edge, New
//! Document, Default Directory, File Association, Language,
//! Indentation, Highlighting, Print, Searching, Backup,
//! Auto-Completion, Multi-Instance & Date, Delimiter,
//! Performance, Cloud & Link, Search Engine, MISC.) is a future
//! commit — each just adds a new listbox row and a new
//! per-category panel switched by the listbox selection.
//!
//! **Return value.** [`show_preferences_dialog`] returns
//! `Some(Preferences)` when the user closed via the Close button
//! or Esc *after* changing something (or even without changing;
//! we always emit the read-back), and `None` if the dialog
//! couldn't be created (window-class register failure, alloc
//! failure). The Shell's `set_preferences` handles the persist
//! + side-effects; this module never touches disk directly.

use std::ffi::c_void;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    FillRect, GetStockObject, SetBkMode, DEFAULT_GUI_FONT, HDC, HFONT, NULL_BRUSH, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    IsDialogMessageW, IsWindow, LoadCursorW, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK,
    BN_CLICKED, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_DEFPUSHBUTTON, BS_GROUPBOX, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, GW_OWNER, HCURSOR, HMENU, IDC_ARROW, LBN_SELCHANGE,
    LBS_HASSTRINGS, LBS_NOTIFY, LB_ADDSTRING, LB_SETCURSEL, MSG, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DESTROY,
    WM_ERASEBKGND, WM_NCCREATE, WM_NCDESTROY, WM_QUIT, WM_SETFONT, WM_SETTEXT, WNDCLASSEXW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_GROUP, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use codepp_core::preferences::{
    Preferences, RecentFileDisplayMode, CUSTOM_MAX_LENGTH_LIMIT, MAX_ENTRIES_LIMIT,
};

use crate::{dialog_bg_brush, disable_visual_style, DlgDestroyGuard, OwnerEnableGuard};

const PREFS_CLASS: PCWSTR = windows::core::w!("CodePlusPlusPreferencesDialog");

/// Control ids. Kept in the 900..=999 block so they never
/// collide with the main window's menu ids or the other modal
/// dialogs' control ids (Style Configurator uses 800..=899).
const IDC_PREFS_CATEGORY_LIST: u16 = 900;
const IDC_PREFS_ENABLED: u16 = 901;
const IDC_PREFS_MAX_ENTRIES: u16 = 902;
const IDC_PREFS_IN_SUBMENU: u16 = 903;
const IDC_PREFS_RADIO_ONLY_NAME: u16 = 904;
const IDC_PREFS_RADIO_FULL_PATH: u16 = 905;
const IDC_PREFS_RADIO_CUSTOM_LEN: u16 = 906;
const IDC_PREFS_CUSTOM_LEN_EDIT: u16 = 907;
const IDC_PREFS_CLOSE: u16 = 908;

/// Per-dialog state. Lives on the heap and its raw pointer is
/// parked in `GWLP_USERDATA` on the dialog HWND. Access from
/// `wnd_proc` goes through the pointer, never a fresh `&mut`
/// — same Stacked-Borrows-safe pattern the print preview
/// module uses.
struct PrefsState {
    /// Snapshot the caller handed us; the "as it was when we
    /// opened" baseline. Mutated in place by control callbacks
    /// so the read-back on Close is trivially "just return
    /// prefs".
    prefs: Preferences,
    /// Child HWNDs, cached so control callbacks don't need to
    /// re-query via `GetDlgItem` on every read/write.
    hwnd_category: HWND,
    hwnd_enabled: HWND,
    hwnd_max_entries: HWND,
    hwnd_in_submenu: HWND,
    hwnd_only_name: HWND,
    hwnd_full_path: HWND,
    hwnd_custom_len_radio: HWND,
    hwnd_custom_len_edit: HWND,
    hwnd_close: HWND,
}

/// Show the Preferences dialog modally. On close returns the
/// updated [`Preferences`] snapshot; the caller (`Shell`) is
/// responsible for persistence and any side-effect actions
/// (e.g. clearing the recent-files list when the feature was
/// disabled).
///
/// Returns `None` when the dialog could not be created (class
/// registration failure, `CreateWindowExW` failure, allocation
/// failure). Every other close path — Close button, Esc, or
/// System Menu → Close — returns `Some(prefs)` with whatever
/// values the controls held at close time.
pub fn show_preferences_dialog(main_hwnd: HWND, current: Preferences) -> Option<Preferences> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<()> = OnceLock::new();

    unsafe {
        let instance = GetModuleHandleW(None).ok()?;

        REGISTERED.get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(prefs_wnd_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR(std::ptr::null_mut())),
                hbrBackground: dialog_bg_brush(),
                lpszClassName: PREFS_CLASS,
                ..Default::default()
            };
            let _ = RegisterClassExW(&raw const class);
        });

        // Layout in CLIENT coordinates. Matches N++'s Preferences
        // dialog proportions — left listbox, right per-category
        // panel, bottom Close.
        const CLIENT_W: i32 = 720;
        const CLIENT_H: i32 = 440;

        let mut window_rect = RECT {
            left: 0,
            top: 0,
            right: CLIENT_W,
            bottom: CLIENT_H,
        };
        let _ = AdjustWindowRectEx(
            &raw mut window_rect,
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            false,
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
        );
        let dlg_w = window_rect.right - window_rect.left;
        let dlg_h = window_rect.bottom - window_rect.top;

        let mut owner_rect = RECT::default();
        let _ = GetWindowRect(main_hwnd, &raw mut owner_rect);
        let owner_w = owner_rect.right - owner_rect.left;
        let owner_h = owner_rect.bottom - owner_rect.top;
        let dlg_x = owner_rect.left + (owner_w - dlg_w) / 2;
        let dlg_y = owner_rect.top + (owner_h - dlg_h) / 2;

        // Box up an empty state so the pointer is stable across
        // the WM_NCCREATE window-creation callback. Fields are
        // patched in on WM_CREATE once the child controls exist.
        let state = Box::new(PrefsState {
            prefs: current,
            hwnd_category: HWND(std::ptr::null_mut()),
            hwnd_enabled: HWND(std::ptr::null_mut()),
            hwnd_max_entries: HWND(std::ptr::null_mut()),
            hwnd_in_submenu: HWND(std::ptr::null_mut()),
            hwnd_only_name: HWND(std::ptr::null_mut()),
            hwnd_full_path: HWND(std::ptr::null_mut()),
            hwnd_custom_len_radio: HWND(std::ptr::null_mut()),
            hwnd_custom_len_edit: HWND(std::ptr::null_mut()),
            hwnd_close: HWND(std::ptr::null_mut()),
        });
        let state_ptr = Box::into_raw(state);

        let title = windows::core::HSTRING::from("Preferences");
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PREFS_CLASS,
            &title,
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            dlg_x,
            dlg_y,
            dlg_w,
            dlg_h,
            Some(main_hwnd),
            None,
            Some(instance.into()),
            Some(state_ptr.cast::<c_void>()),
        );
        let dlg = match dlg {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                // Reclaim the Box — CreateWindowExW never got to
                // hand it to WM_NCCREATE, so we own the leak.
                drop(Box::from_raw(state_ptr));
                return None;
            }
        };
        let _dlg_guard = DlgDestroyGuard(dlg);

        // Disable the owner + run the modal pump. RAII guard
        // restores enable + foreground on every exit path.
        let _ = EnableWindow(main_hwnd, false);
        let _owner_guard = OwnerEnableGuard(main_hwnd);
        let _ = ShowWindow(dlg, SW_SHOW);

        // Focus goes to the category listbox so the initial Tab
        // order feels natural (Tab → panel controls, Shift-Tab
        // → Close). Focusing Close would look strange for a
        // dialog the user just opened.
        if let Some(state_ref) = state_ptr.as_ref() {
            let _ = SetFocus(Some(state_ref.hwnd_category));
        }

        run_modal(dlg);

        // Read back the mutated Prefs before dropping the Box.
        let result = state_ptr.as_ref().map(|state_ref| state_ref.prefs.clone());
        // Reclaim + drop the state box. Ownership was never
        // handed off — GWLP_USERDATA is a bare pointer.
        drop(Box::from_raw(state_ptr));
        result
    }
}

/// Modal pump — a private `GetMessageW` loop that owns the
/// thread until `dlg` is destroyed. Mirrors the print-preview
/// module's `run_modal` verbatim so behaviour (Esc handling,
/// Tab navigation, Close routing) matches across every modal
/// dialog in the app.
unsafe fn run_modal(dlg: HWND) {
    unsafe {
        let mut msg = MSG::default();
        loop {
            if !IsWindow(Some(dlg)).as_bool() {
                break;
            }
            let ret = GetMessageW(&raw mut msg, None, 0, 0);
            match ret.0 {
                0 => {
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
    }
}

/// Restore the owner to the foreground before destroying the
/// dialog. Same pattern the print-preview and UDL-editor
/// modules use — destroying an owned popup that currently
/// holds the foreground can leave Win32 with the main app
/// behind another window.
unsafe fn close_prefs(hwnd: HWND) {
    unsafe {
        let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
        if !owner.is_invalid() && IsWindow(Some(owner)).as_bool() {
            let _ = SetForegroundWindow(owner);
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// Cast the raw `GWLP_USERDATA` pointer back to a mutable
/// [`PrefsState`] reference. Returns `None` when the slot
/// isn't populated (either before `WM_NCCREATE` fires or
/// after `WM_NCDESTROY` clears it).
unsafe fn state_from(hwnd: HWND) -> Option<*mut PrefsState> {
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if raw == 0 {
            None
        } else {
            Some(raw as *mut PrefsState)
        }
    }
}

unsafe extern "system" fn prefs_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                // Extract the PrefsState pointer we handed to
                // CreateWindowExW via `lpCreateParams` and park
                // it in GWLP_USERDATA for every subsequent
                // callback.
                let cs = lparam.0 as *const CREATESTRUCTW;
                if !cs.is_null() {
                    let state_ptr = (*cs).lpCreateParams as isize;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CREATE => {
                // Populate the child controls, cache their
                // HWNDs, and prime their values from the
                // current Preferences snapshot.
                populate_controls(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                // Paint the whole client area with the app's
                // chrome brush (`DIALOG_BG`) so the dialog blends
                // with every other modal in Code++. Returning
                // LRESULT(1) alone is not enough — DefWindowProc
                // is what normally uses the class `hbrBackground`,
                // and short-circuiting past it means no paint
                // happens unless we do it here.
                let hdc = HDC(wparam.0 as *mut c_void);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &raw mut rect);
                FillRect(hdc, &raw const rect, dialog_bg_brush());
                LRESULT(1)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                // Static labels, group boxes, checkboxes, and
                // radio buttons ask their parent what colour to
                // paint their text background in. Without this
                // handler Win32 hands back `COLOR_WINDOW` (system
                // white / dark-mode grey), which shows up as
                // small colour patches around each control text
                // that don't match `DIALOG_BG`. Same fix the
                // About / Style Configurator / UDL-editor dialogs
                // apply — `SetBkMode(TRANSPARENT)` + returning
                // `NULL_BRUSH` tells the control to skip its own
                // background fill and let the parent's already-
                // painted chrome show through.
                let hdc = HDC(wparam.0 as *mut c_void);
                let _ = SetBkMode(hdc, TRANSPARENT);
                LRESULT(GetStockObject(NULL_BRUSH).0 as isize)
            }
            WM_COMMAND => {
                // wparam packs (notify_code, cmd_id); lparam is
                // the child HWND (or 0 for menu commands).
                let cmd_id = (wparam.0 & 0xFFFF) as u16;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u16;
                // IsDialogMessageW translates Esc/Enter into
                // IDCANCEL/IDOK — both close the dialog.
                let effective = if cmd_id == 1 || cmd_id == 2 {
                    IDC_PREFS_CLOSE
                } else {
                    cmd_id
                };
                if u32::from(notify) == BN_CLICKED
                    || notify == EN_KILLFOCUS
                    || u32::from(notify) == LBN_SELCHANGE
                    || cmd_id == 1
                    || cmd_id == 2
                {
                    handle_command(hwnd, effective, notify);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                sync_state_from_controls(hwnd);
                close_prefs(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// `EN_KILLFOCUS` — edit-control notify code fired when the
/// user tabs out of an edit box. Not re-exported by the
/// windows-rs `WindowsAndMessaging` prelude, so we spell the
/// constant out.
const EN_KILLFOCUS: u16 = 0x0200;

const WM_CREATE: u32 = 0x0001;

/// Handle a `WM_COMMAND` after normalising the effective id.
/// Runs on the UI thread; safe to mutate the `PrefsState` behind
/// `GWLP_USERDATA` — no other message can be dispatched
/// concurrently.
unsafe fn handle_command(hwnd: HWND, effective: u16, notify: u16) {
    unsafe {
        match effective {
            IDC_PREFS_CLOSE => {
                sync_state_from_controls(hwnd);
                close_prefs(hwnd);
            }
            IDC_PREFS_ENABLED
            | IDC_PREFS_IN_SUBMENU
            | IDC_PREFS_RADIO_ONLY_NAME
            | IDC_PREFS_RADIO_FULL_PATH
            | IDC_PREFS_RADIO_CUSTOM_LEN
                if u32::from(notify) == BN_CLICKED =>
            {
                sync_state_from_controls(hwnd);
            }
            // Edit-control values are re-read on close and on
            // kill-focus. Both paths funnel through
            // `sync_state_from_controls`, which clamps to the
            // valid range so a mid-edit intermediate (empty
            // string, out-of-range digit) never leaks into
            // `PrefsState.prefs`.
            IDC_PREFS_MAX_ENTRIES | IDC_PREFS_CUSTOM_LEN_EDIT if notify == EN_KILLFOCUS => {
                sync_state_from_controls(hwnd);
            }
            _ => {}
        }
    }
}

/// Pull the current values off the child controls and write
/// them back into `PrefsState.prefs`, clamping to the valid
/// range so downstream `set_preferences` never sees invalid
/// input. Called on close and on every meaningful control
/// mutation so read-back is trivial.
unsafe fn sync_state_from_controls(hwnd: HWND) {
    unsafe {
        let Some(state_ptr) = state_from(hwnd) else {
            return;
        };
        let state = &mut *state_ptr;
        let cfg = &mut state.prefs.recent_files_history;

        // Label inversion: the checkbox reads "Don't check at
        // launch time" (N++ wording, negative sense), so
        // BST_CHECKED means the feature is OFF. Storage holds
        // the positive-sense `enabled`, so the render side
        // (see `populate_controls`) writes `!enabled` and this
        // read side inverts back with `!is_checked(...)`. Both
        // halves of the inversion MUST stay in lockstep — a
        // no-op open+close of the dialog is otherwise silently
        // destructive (flips `enabled` → `!enabled`, which
        // then triggers `Shell::set_preferences`'s "user
        // disabled the feature" side-effect and wipes the
        // retained recent-files list).
        cfg.enabled = !is_checked(state.hwnd_enabled);
        cfg.in_submenu = is_checked(state.hwnd_in_submenu);
        cfg.max_entries = read_edit_u32(state.hwnd_max_entries).unwrap_or(cfg.max_entries);
        cfg.custom_max_length =
            read_edit_u32(state.hwnd_custom_len_edit).unwrap_or(cfg.custom_max_length);

        if is_checked(state.hwnd_only_name) {
            cfg.display_mode = RecentFileDisplayMode::OnlyFileName;
        } else if is_checked(state.hwnd_custom_len_radio) {
            cfg.display_mode = RecentFileDisplayMode::CustomMaxLength;
        } else {
            // Default to FullPath — the third radio is the
            // "always fits" fallback.
            cfg.display_mode = RecentFileDisplayMode::FullPath;
        }

        cfg.clamp();
    }
}

/// `Button.IsChecked` in one line. Returns `true` iff the
/// window's `BM_GETCHECK` reply is `BST_CHECKED == 1`.
unsafe fn is_checked(button: HWND) -> bool {
    unsafe { SendMessageW(button, BM_GETCHECK, None, None).0 == 1 }
}

/// Read an edit control's text and parse it as `u32`. Returns
/// `None` when the field is empty or contains non-digit
/// characters; the caller preserves the previous value in
/// that case.
unsafe fn read_edit_u32(edit: HWND) -> Option<u32> {
    unsafe {
        let mut buf = [0u16; 16];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        if n == 0 {
            return None;
        }
        let text = String::from_utf16_lossy(&buf[..n]);
        text.trim().parse::<u32>().ok()
    }
}

/// Build the child controls. Runs once during `WM_CREATE`.
/// Every control uses the same `DEFAULT_GUI_FONT` so the dialog
/// blends with the system theme.
unsafe fn populate_controls(hwnd: HWND) {
    unsafe {
        let Some(state_ptr) = state_from(hwnd) else {
            return;
        };
        let state = &mut *state_ptr;
        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);

        // Layout constants. Matches N++ layout roughly.
        const PAD: i32 = 12;
        const LIST_W: i32 = 180;
        const LIST_Y: i32 = PAD;
        const LIST_H: i32 = 340;
        const PANEL_X: i32 = PAD + LIST_W + PAD;
        const PANEL_Y: i32 = PAD;

        // 1. Category listbox on the left.
        let category = create_child(
            hwnd,
            windows::core::w!("LISTBOX"),
            None,
            WS_CHILD
                | WS_VISIBLE
                | WS_BORDER
                | WS_VSCROLL
                | WS_TABSTOP
                | WINDOW_STYLE(LBS_NOTIFY as u32)
                | WINDOW_STYLE(LBS_HASSTRINGS as u32),
            PAD,
            LIST_Y,
            LIST_W,
            LIST_H,
            IDC_PREFS_CATEGORY_LIST,
        );
        state.hwnd_category = category;
        set_font(category, font);
        // Populate with the single available category. Every
        // future pane just adds another `LB_ADDSTRING` line.
        add_listbox_string(category, "Recent Files History");
        // Select the first (and only) row so the panel is
        // immediately labelled correctly.
        SendMessageW(category, LB_SETCURSEL, Some(WPARAM(0)), None);

        // 2. Right panel: "Recent Files History" group.
        //
        // BS_GROUPBOX + BS_AUTOCHECKBOX + BS_AUTORADIOBUTTON on
        // Win11 all render through the UxTheme layer, which
        // paints its own COLOR_BTNFACE (~#F0F0F0) rectangle
        // behind the control text before consulting
        // WM_CTLCOLORBTN / WM_CTLCOLORSTATIC. That produces a
        // visible darker patch around every label that doesn't
        // match the dialog's #F9F9F9 chrome. `disable_visual_style`
        // opts the control out of theming for its background paint
        // (via `SetWindowTheme(hwnd, "", "")`), pushing it onto
        // the classic-paint path that honours our NULL_BRUSH
        // return — text still uses the system UI font, but its
        // background is transparent so the dialog chrome shows
        // through. Same fix the other Code++ modals apply.
        // Push buttons (BS_DEFPUSHBUTTON, Close below) deliberately
        // keep their visual style so they look right on Win11.
        const GROUP_TOP_H: i32 = 96;
        let group_top = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Recent Files History"),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_GROUPBOX as u32),
            PANEL_X,
            PANEL_Y,
            420,
            GROUP_TOP_H,
            0,
        );
        set_font(group_top, font);
        disable_visual_style(group_top);

        // Enabled checkbox.
        let enabled = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Don't check at launch time"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            PANEL_X + 16,
            PANEL_Y + 24,
            300,
            22,
            IDC_PREFS_ENABLED,
        );
        state.hwnd_enabled = enabled;
        set_font(enabled, font);
        disable_visual_style(enabled);
        // Render inverts because the label is negative-sense:
        // the checkbox reads "Don't check at launch time" (N++
        // wording), so BST_CHECKED means the feature is OFF.
        // `sync_state_from_controls` performs the matching
        // inverse read — the two halves must stay in lockstep,
        // see the same comment there.
        set_checked(enabled, !state.prefs.recent_files_history.enabled);

        // Static "Max. number of entries:" + edit + hint.
        let lbl_max = create_child(
            hwnd,
            windows::core::w!("STATIC"),
            Some("Max. number of entries:"),
            WS_CHILD | WS_VISIBLE,
            PANEL_X + 16,
            PANEL_Y + 56,
            160,
            18,
            0,
        );
        set_font(lbl_max, font);
        let max_edit = create_child(
            hwnd,
            windows::core::w!("EDIT"),
            None,
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP,
            PANEL_X + 180,
            PANEL_Y + 54,
            48,
            22,
            IDC_PREFS_MAX_ENTRIES,
        );
        state.hwnd_max_entries = max_edit;
        set_font(max_edit, font);
        set_edit_text(
            max_edit,
            &state.prefs.recent_files_history.max_entries.to_string(),
        );
        let hint_max = format!("(0 - {MAX_ENTRIES_LIMIT})");
        let hint_max_hwnd = create_child(
            hwnd,
            windows::core::w!("STATIC"),
            Some(&hint_max),
            WS_CHILD | WS_VISIBLE,
            PANEL_X + 236,
            PANEL_Y + 56,
            80,
            18,
            0,
        );
        set_font(hint_max_hwnd, font);

        // 3. Display group.
        const DISPLAY_Y: i32 = PANEL_Y + GROUP_TOP_H + 12;
        const DISPLAY_H: i32 = 200;
        let group_disp = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Display"),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_GROUPBOX as u32),
            PANEL_X,
            DISPLAY_Y,
            420,
            DISPLAY_H,
            0,
        );
        set_font(group_disp, font);
        disable_visual_style(group_disp);

        let in_submenu = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("In Submenu"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            PANEL_X + 16,
            DISPLAY_Y + 24,
            180,
            22,
            IDC_PREFS_IN_SUBMENU,
        );
        state.hwnd_in_submenu = in_submenu;
        set_font(in_submenu, font);
        disable_visual_style(in_submenu);
        set_checked(in_submenu, state.prefs.recent_files_history.in_submenu);

        // Radio group: mark the first radio with WS_GROUP so
        // arrow-key nav treats the three as one unit.
        let only_name = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Only File Name"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_GROUP | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
            PANEL_X + 16,
            DISPLAY_Y + 54,
            240,
            22,
            IDC_PREFS_RADIO_ONLY_NAME,
        );
        state.hwnd_only_name = only_name;
        set_font(only_name, font);
        disable_visual_style(only_name);

        let full_path = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Full File Name Path"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
            PANEL_X + 16,
            DISPLAY_Y + 82,
            240,
            22,
            IDC_PREFS_RADIO_FULL_PATH,
        );
        state.hwnd_full_path = full_path;
        set_font(full_path, font);
        disable_visual_style(full_path);

        let custom_len_radio = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Customize Maximum Length:"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
            PANEL_X + 16,
            DISPLAY_Y + 110,
            240,
            22,
            IDC_PREFS_RADIO_CUSTOM_LEN,
        );
        state.hwnd_custom_len_radio = custom_len_radio;
        set_font(custom_len_radio, font);
        disable_visual_style(custom_len_radio);
        let custom_len_edit = create_child(
            hwnd,
            windows::core::w!("EDIT"),
            None,
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP,
            PANEL_X + 260,
            DISPLAY_Y + 108,
            48,
            22,
            IDC_PREFS_CUSTOM_LEN_EDIT,
        );
        state.hwnd_custom_len_edit = custom_len_edit;
        set_font(custom_len_edit, font);
        set_edit_text(
            custom_len_edit,
            &state
                .prefs
                .recent_files_history
                .custom_max_length
                .to_string(),
        );
        let hint_len = format!("(1 - {CUSTOM_MAX_LENGTH_LIMIT})");
        let hint_len_hwnd = create_child(
            hwnd,
            windows::core::w!("STATIC"),
            Some(&hint_len),
            WS_CHILD | WS_VISIBLE,
            PANEL_X + 316,
            DISPLAY_Y + 110,
            100,
            18,
            0,
        );
        set_font(hint_len_hwnd, font);

        // Set the initial radio selection to match prefs.
        match state.prefs.recent_files_history.display_mode {
            RecentFileDisplayMode::OnlyFileName => set_checked(only_name, true),
            RecentFileDisplayMode::FullPath => set_checked(full_path, true),
            RecentFileDisplayMode::CustomMaxLength => set_checked(custom_len_radio, true),
        }

        // 4. Bottom-centred Close button.
        const CLOSE_W: i32 = 96;
        const CLOSE_H: i32 = 28;
        let close = create_child(
            hwnd,
            windows::core::w!("BUTTON"),
            Some("Close"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            (PAD + LIST_W + PAD + 420 - CLOSE_W) / 2 + PAD + LIST_W / 2,
            PANEL_Y + GROUP_TOP_H + 12 + DISPLAY_H + 20,
            CLOSE_W,
            CLOSE_H,
            IDC_PREFS_CLOSE,
        );
        state.hwnd_close = close;
        set_font(close, font);
    }
}

#[allow(clippy::too_many_arguments)] // Win32 `CreateWindowExW`
                                     // takes 9 positional args; a helper
                                     // wrapper below the natural API
                                     // shape isn't worth the extra
                                     // ceremony for an internal helper.
unsafe fn create_child(
    parent: HWND,
    class: PCWSTR,
    text: Option<&str>,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: u16,
) -> HWND {
    unsafe {
        // Keep the HSTRING alive until CreateWindowExW returns —
        // the PCWSTR is a bare pointer into its buffer.
        let text_hstring = text.map(HSTRING::from);
        let text_pcwstr = text_hstring
            .as_ref()
            .map_or(PCWSTR::null(), |hs| PCWSTR(hs.as_ptr()));
        let instance = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            text_pcwstr,
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            Some(instance.into()),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()))
    }
}

unsafe fn set_font(control: HWND, font: HFONT) {
    unsafe {
        SendMessageW(
            control,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

unsafe fn set_checked(button: HWND, checked: bool) {
    unsafe {
        let wparam = WPARAM(usize::from(checked));
        SendMessageW(button, BM_SETCHECK, Some(wparam), None);
    }
}

unsafe fn set_edit_text(edit: HWND, text: &str) {
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(wide.as_ptr() as isize)));
    }
}

unsafe fn add_listbox_string(listbox: HWND, text: &str) {
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        SendMessageW(
            listbox,
            LB_ADDSTRING,
            None,
            Some(LPARAM(wide.as_ptr() as isize)),
        );
    }
}
