//! In-memory `DLGTEMPLATEEX` construction plus the modal / modeless
//! runners that go with it.
//!
//! # Why this module exists
//!
//! Every dialog in this backend used to be a `WS_POPUP | WS_CAPTION`
//! window of our own registered class, with a hand-rolled nested
//! `GetMessageW` pump for the modal ones. That works, but it opts out
//! of everything `#32770` — the standard dialog class — does for
//! free, and the visible cost was a colour mismatch: our class brush
//! painted `#F9F9F9` while every *themed* child control (checkbox,
//! radio, groupbox, static) painted its own background at
//! `COLOR_3DFACE` (`#F0F0F0` in the default Win11 light theme). The
//! workaround was a spray of `disable_visual_style` calls stripping
//! Win11 theming off those controls so they would fall back to the
//! classic paint path, which honours `WM_CTLCOLORBTN` and so blends.
//!
//! Building on `#32770` removes the cause rather than the symptom.
//! `DefDlgProc` answers `WM_CTLCOLORDLG` with the system dialog
//! brush, so the dialog and its themed children agree by
//! construction, on whatever theme is active — no hardcoded constant
//! to drift, and the controls keep their Win11 look.
//!
//! It also hands us, for free, the behaviour each custom class had to
//! reimplement or live without: `IDOK` on Enter honouring the default
//! button, `IDCANCEL` on Escape, arrow-key navigation within a radio
//! group, initial-focus assignment, and — for modal dialogs —
//! owner-disable plus the nested message pump.
//!
//! # What the templates carry
//!
//! Nothing but the frame. `DLGTEMPLATEEX` can describe child controls
//! too, but this backend's dialogs compute their layout in pixels at
//! runtime (several of them from measured text extents), whereas
//! template items are fixed dialog units. So every template here
//! declares `cDlgItems == 0` and the existing `CreateWindowExW` child
//! construction moves verbatim into `WM_INITDIALOG`.
//!
//! For the same reason the template's own `cx` / `cy` are placeholders
//! — [`size_client_and_center`] resizes the dialog to an exact client
//! pixel size during `WM_INITDIALOG`, before the first paint.

use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateDialogIndirectParamW, DialogBoxIndirectParamW, GetWindowLongPtrW,
    GetWindowRect, SetWindowPos, DLGPROC, DS_MODALFRAME, DS_SETFONT, GWL_EXSTYLE, GWL_STYLE,
    HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION,
    WS_POPUP, WS_SYSMENU,
};

/// Point size the dialog font is declared at in the template.
///
/// Matches the shell dialog font Windows itself uses for `#32770`
/// dialogs. Note this governs the *dialog*'s font and hence its
/// dialog-unit basis; the child controls this backend creates in
/// `WM_INITDIALOG` are still explicitly given `DEFAULT_GUI_FONT` by
/// `apply_dialog_font`, exactly as before the migration.
const DLG_FONT_POINT_SIZE: u16 = 9;

/// `FW_NORMAL`. Declared inline — windows-rs exposes the font-weight
/// constants only through the GDI `FW_*` newtype, and reaching for
/// that here just to spell `400` is more import noise than the
/// literal.
const DLG_FONT_WEIGHT: u16 = 400;

/// `DEFAULT_CHARSET`, for the same reason as [`DLG_FONT_WEIGHT`].
const DLG_FONT_CHARSET: u8 = 1;

/// Typeface declared in the template. Segoe UI is the Win11 shell
/// dialog font; if it is ever absent the dialog manager falls back to
/// the system font rather than failing, so this needs no probe.
const DLG_FONT_FACE: &str = "Segoe UI";

/// The window style every dialog in this backend is built with.
///
/// `DS_SETFONT` is what makes the trailing font block in the template
/// meaningful — without it the dialog manager stops parsing after the
/// title and the font fields are read as garbage. `DS_MODALFRAME`
/// gives the dialog its raised modal border (the same look the old
/// `WS_EX_DLGMODALFRAME` extended style produced).
///
/// Deliberately **not** `DS_CENTER`: this backend centres on the
/// *owner window* rather than on the screen, which is what
/// [`size_client_and_center`] does.
pub(crate) fn dialog_style() -> u32 {
    DS_SETFONT as u32 | DS_MODALFRAME as u32 | WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0
}

/// Builder for a controls-less `DLGTEMPLATEEX` byte stream.
///
/// The layout is fixed by Win32 and documented under `DLGTEMPLATEEX`:
/// a `WORD`-granular header, then variable-length `sz_Or_Ord` fields
/// for the menu, window class and title, then — because we always set
/// `DS_SETFONT` — the font block. The whole structure must be
/// `DWORD`-aligned, which [`Self::finish`] enforces.
pub(crate) struct DialogTemplate {
    /// The template as 16-bit words. `DLGTEMPLATEEX` is defined in
    /// `WORD` terms with `DWORD` alignment at the start, so building
    /// it as a `Vec<u16>` keeps the field pushes readable and gives
    /// the required 2-byte alignment for free.
    words: Vec<u16>,
}

impl DialogTemplate {
    /// Start a template for the standard dialog class with the given
    /// caption.
    ///
    /// `cx` / `cy` are in dialog units and are placeholders for every
    /// caller in this backend — see the module docs. They still have
    /// to be *something* sane, because the dialog manager creates the
    /// window at that size before `WM_INITDIALOG` runs, and a zero
    /// there makes the pre-resize frame degenerate.
    pub(crate) fn new(title: &str, style: u32, ex_style: u32, cx: u16, cy: u16) -> Self {
        let mut words: Vec<u16> = Vec::new();
        // --- DLGTEMPLATEEX header ---
        words.push(1); // dlgVer: 1
        words.push(0xFFFF); // signature: 0xFFFF marks the EX form
        push_u32(&mut words, 0); // helpID
        push_u32(&mut words, ex_style);
        push_u32(&mut words, style);
        words.push(0); // cDlgItems — see module docs
        words.push(0); // x  (overridden by size_client_and_center)
        words.push(0); // y
        words.push(cx);
        words.push(cy);
        // `sz_Or_Ord` menu: a single 0x0000 word means "no menu".
        words.push(0);
        // `sz_Or_Ord` windowClass: a single 0x0000 word means the
        // predefined dialog class, i.e. `#32770`. This is the whole
        // point of the module — see the docs above.
        words.push(0);
        push_utf16z(&mut words, title);
        // --- font block, present because dialog_style sets DS_SETFONT ---
        debug_assert!(
            style & DS_SETFONT as u32 != 0,
            "the font block below is only parsed when DS_SETFONT is set",
        );
        words.push(DLG_FONT_POINT_SIZE);
        words.push(DLG_FONT_WEIGHT);
        // `italic` and `charset` are adjacent BYTEs, so they occupy
        // one WORD together: italic in the low byte, charset in the
        // high byte.
        words.push(u16::from(DLG_FONT_CHARSET) << 8);
        push_utf16z(&mut words, DLG_FONT_FACE);
        Self { words }
    }

    /// Finish the template and hand back the word buffer.
    ///
    /// The caller must keep the returned `Vec` alive for the whole
    /// `DialogBoxIndirectParamW` / `CreateDialogIndirectParamW` call —
    /// the dialog manager reads the template during creation only, but
    /// it reads it from *our* memory.
    pub(crate) fn finish(mut self) -> Vec<u16> {
        // `DLGTEMPLATEEX` must be DWORD-aligned overall. With no
        // control items following there is nothing after us that
        // depends on the tail alignment, but the dialog manager is
        // documented to require it, so pad rather than rely on the
        // absence of items.
        if !self.words.len().is_multiple_of(2) {
            self.words.push(0);
        }
        self.words
    }
}

/// Append a `u32` as two little-endian words.
fn push_u32(words: &mut Vec<u16>, value: u32) {
    words.push((value & 0xFFFF) as u16);
    words.push((value >> 16) as u16);
}

/// Append a NUL-terminated UTF-16 string.
fn push_utf16z(words: &mut Vec<u16>, s: &str) {
    words.extend(s.encode_utf16());
    words.push(0);
}

/// Run a modal dialog and return the value its dialog proc passed to
/// `EndDialog`.
///
/// Replaces the hand-rolled `EnableWindow(owner, false)` + nested
/// `GetMessageW` / `IsDialogMessageW` pump each modal dialog used to
/// carry: the dialog manager does both, and it also restores
/// activation to the owner on the way out, which the hand-rolled
/// version had to get right by ordering an RAII guard against
/// `DestroyWindow`.
///
/// `param` is delivered to the proc as `WM_INITDIALOG`'s `lparam`;
/// this backend uses it for the `*mut State` pointer that the custom
/// classes used to receive through `CREATESTRUCTW.lpCreateParams`.
///
/// # Safety
///
/// `param` must be valid for the lifetime of the dialog, and
/// `dlg_proc` must be a real dialog procedure — it returns `BOOL`
/// semantics (nonzero = handled), *not* the `LRESULT` a window
/// procedure returns.
pub(crate) unsafe fn run_modal(
    instance: HMODULE,
    template: &[u16],
    owner: HWND,
    dlg_proc: DLGPROC,
    param: isize,
) -> isize {
    unsafe {
        DialogBoxIndirectParamW(
            Some(instance.into()),
            template.as_ptr().cast(),
            Some(owner),
            dlg_proc,
            LPARAM(param),
        )
    }
}

/// Create a modeless dialog. The caller owns the returned handle and
/// is responsible for `DestroyWindow`.
///
/// The template must not carry `WS_VISIBLE`: `CreateDialogIndirectParamW`
/// honours its absence, which lets `WM_INITDIALOG` finish laying the
/// dialog out before it is shown.
///
/// # Safety
///
/// Same contract as [`run_modal`].
pub(crate) unsafe fn create_modeless(
    instance: HMODULE,
    template: &[u16],
    owner: HWND,
    dlg_proc: DLGPROC,
    param: isize,
) -> Option<HWND> {
    unsafe {
        CreateDialogIndirectParamW(
            Some(instance.into()),
            template.as_ptr().cast(),
            Some(owner),
            dlg_proc,
            LPARAM(param),
        )
        .ok()
    }
}

/// Resize `hwnd` so its **client area** is exactly `client_w` x
/// `client_h` pixels, then centre it on `owner`.
///
/// Call from `WM_INITDIALOG`. The dialog manager has already created
/// the window at the template's dialog-unit size by then, but has not
/// shown it, so this runs before the first paint.
///
/// Sizing in client pixels rather than dialog units is what lets the
/// per-dialog layout constants survive the migration unchanged: they
/// were all written against `AdjustWindowRectEx`-derived client
/// rectangles, and this reproduces that derivation against the
/// dialog's real style bits instead of the hardcoded
/// `WS_POPUP | WS_CAPTION | WS_SYSMENU` triple each call site used to
/// repeat.
///
/// # Safety
///
/// `hwnd` must be a live window; `owner` may be null, in which case
/// the dialog is only resized and left where the dialog manager put
/// it.
pub(crate) unsafe fn size_client_and_center(hwnd: HWND, owner: HWND, client_w: i32, client_h: i32) {
    unsafe {
        let (w, h) = window_size_for_client(hwnd, client_w, client_h);
        match center_on(owner, w, h) {
            Some((x, y)) => {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    x,
                    y,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            None => {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    0,
                    0,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                );
            }
        }
    }
}

/// Resize `hwnd` so its **outer window** is exactly `window_w` x
/// `window_h` pixels, then centre it on `owner`.
///
/// The client-sized [`size_client_and_center`] is the one to reach
/// for. This variant exists for dialogs whose layout constants were
/// authored against the outer window size — the FIF progress dialog
/// passed `W` / `H` straight to `CreateWindowExW` and then subtracted
/// a hand-tuned constant for the non-client gap. Sizing those in
/// client pixels would silently shift every control, so they keep the
/// dimensions they were tuned with.
///
/// # Safety
///
/// Same contract as [`size_client_and_center`].
pub(crate) unsafe fn size_window_and_center(hwnd: HWND, owner: HWND, window_w: i32, window_h: i32) {
    unsafe {
        match center_on(owner, window_w, window_h) {
            Some((x, y)) => {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    x,
                    y,
                    window_w,
                    window_h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            None => {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    0,
                    0,
                    window_w,
                    window_h,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                );
            }
        }
    }
}

/// Outer window size that yields a `client_w` x `client_h` client
/// area for the window's current style bits.
///
/// # Safety
///
/// `hwnd` must be a live window.
pub(crate) unsafe fn window_size_for_client(
    hwnd: HWND,
    client_w: i32,
    client_h: i32,
) -> (i32, i32) {
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let ex_style = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        // `false` for the menu argument: none of this backend's
        // dialogs carries a menu bar, and the templates declare
        // `sz_Or_Ord menu == 0` to match.
        let _ = AdjustWindowRectEx(&raw mut rect, style, false, ex_style);
        (rect.right - rect.left, rect.bottom - rect.top)
    }
}

/// Top-left corner that centres a `w` x `h` window on `owner`, or
/// `None` when `owner` has no readable rectangle.
///
/// Returning `None` rather than `(0, 0)` keeps a parentless dialog
/// (there are none today, but [`create_modeless`] permits one) from
/// being flung to the top-left of the desktop — the caller leaves it
/// where the dialog manager placed it instead.
unsafe fn center_on(owner: HWND, w: i32, h: i32) -> Option<(i32, i32)> {
    unsafe {
        if owner.is_invalid() {
            return None;
        }
        let mut owner_rect = RECT::default();
        GetWindowRect(owner, &raw mut owner_rect).ok()?;
        let owner_w = owner_rect.right - owner_rect.left;
        let owner_h = owner_rect.bottom - owner_rect.top;
        Some((
            owner_rect.left + (owner_w - w) / 2,
            owner_rect.top + (owner_h - h) / 2,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word offsets into the fixed-size part of `DLGTEMPLATEEX`.
    const OFF_DLGVER: usize = 0;
    const OFF_SIGNATURE: usize = 1;
    const OFF_EXSTYLE: usize = 4;
    const OFF_STYLE: usize = 6;
    const OFF_CDLGITEMS: usize = 8;
    const OFF_CX: usize = 11;
    const OFF_CY: usize = 12;
    const OFF_MENU: usize = 13;
    const OFF_CLASS: usize = 14;
    const OFF_TITLE: usize = 15;

    fn read_u32(words: &[u16], at: usize) -> u32 {
        u32::from(words[at]) | (u32::from(words[at + 1]) << 16)
    }

    fn read_utf16z(words: &[u16], at: usize) -> (String, usize) {
        let end = at + words[at..].iter().position(|&w| w == 0).expect("NUL");
        (String::from_utf16_lossy(&words[at..end]), end + 1)
    }

    #[test]
    fn header_declares_the_ex_form_and_the_standard_dialog_class() {
        let t = DialogTemplate::new("Go To...", dialog_style(), 0, 200, 120).finish();
        assert_eq!(t[OFF_DLGVER], 1, "dlgVer must be 1 for DLGTEMPLATEEX");
        assert_eq!(
            t[OFF_SIGNATURE], 0xFFFF,
            "signature 0xFFFF is what distinguishes DLGTEMPLATEEX from DLGTEMPLATE",
        );
        assert_eq!(
            t[OFF_CDLGITEMS], 0,
            "controls are created in WM_INITDIALOG, not declared in the template",
        );
        assert_eq!(t[OFF_MENU], 0, "no menu");
        assert_eq!(
            t[OFF_CLASS], 0,
            "a zero class ordinal is what selects #32770 — the whole point of the module",
        );
    }

    #[test]
    fn style_and_ex_style_round_trip_through_the_header() {
        let ex = 0x0001_0004_u32;
        let t = DialogTemplate::new("x", dialog_style(), ex, 10, 10).finish();
        assert_eq!(read_u32(&t, OFF_STYLE), dialog_style());
        assert_eq!(read_u32(&t, OFF_EXSTYLE), ex);
    }

    #[test]
    fn dialog_style_sets_the_bits_the_font_block_and_frame_depend_on() {
        let s = dialog_style();
        assert_ne!(
            s & DS_SETFONT as u32,
            0,
            "without DS_SETFONT the dialog manager never parses the trailing font block",
        );
        assert_ne!(s & DS_MODALFRAME as u32, 0);
        assert_ne!(s & WS_POPUP.0, 0);
        assert_ne!(s & WS_CAPTION.0, 0);
        assert_ne!(s & WS_SYSMENU.0, 0);
    }

    #[test]
    fn dialog_style_omits_ws_visible_so_wm_initdialog_can_lay_out_first() {
        // `create_modeless` relies on this: the layout done in
        // WM_INITDIALOG must land before the window is shown, and a
        // WS_VISIBLE template would show it at the placeholder
        // dialog-unit size first.
        assert_eq!(
            dialog_style() & windows::Win32::UI::WindowsAndMessaging::WS_VISIBLE.0,
            0,
        );
    }

    #[test]
    fn title_and_font_face_are_nul_terminated_utf16() {
        let t = DialogTemplate::new("Find & Replace", dialog_style(), 0, 1, 1).finish();
        let (title, after_title) = read_utf16z(&t, OFF_TITLE);
        assert_eq!(title, "Find & Replace");
        assert_eq!(t[after_title], DLG_FONT_POINT_SIZE);
        assert_eq!(t[after_title + 1], DLG_FONT_WEIGHT);
        assert_eq!(
            t[after_title + 2] >> 8,
            u16::from(DLG_FONT_CHARSET),
            "charset occupies the high byte of the italic/charset word",
        );
        assert_eq!(
            t[after_title + 2] & 0xFF,
            0,
            "italic occupies the low byte and is off",
        );
        let (face, _) = read_utf16z(&t, after_title + 3);
        assert_eq!(face, DLG_FONT_FACE);
    }

    #[test]
    fn an_empty_title_still_terminates_and_keeps_the_font_block_findable() {
        // A dialog with no caption text is legal (nothing uses one
        // today, but the encoding must not collapse the field).
        let t = DialogTemplate::new("", dialog_style(), 0, 1, 1).finish();
        assert_eq!(
            t[OFF_TITLE], 0,
            "empty title is a bare NUL, not an absent field"
        );
        assert_eq!(t[OFF_TITLE + 1], DLG_FONT_POINT_SIZE);
    }

    #[test]
    fn placeholder_extent_round_trips() {
        let t = DialogTemplate::new("x", dialog_style(), 0, 321, 123).finish();
        assert_eq!(t[OFF_CX], 321);
        assert_eq!(t[OFF_CY], 123);
    }

    #[test]
    fn the_finished_template_is_dword_aligned() {
        // The dialog manager requires DWORD alignment. Sweep title
        // lengths so both parities of the pre-pad length are covered.
        for n in 0..8 {
            let title: String = "x".repeat(n);
            let t = DialogTemplate::new(&title, dialog_style(), 0, 1, 1).finish();
            assert_eq!(
                t.len() % 2,
                0,
                "template of {n}-char title is not DWORD-aligned ({} words)",
                t.len(),
            );
        }
    }

    #[test]
    fn non_ascii_titles_encode_as_utf16_not_bytes() {
        let t = DialogTemplate::new("Åäö…", dialog_style(), 0, 1, 1).finish();
        let (title, _) = read_utf16z(&t, OFF_TITLE);
        assert_eq!(title, "Åäö…");
    }
}
