//! The `+` at the right of the menu bar, drawn as GTK draws it.
//!
//! The menu bar ends in three entries, `+ ▼ X` (new tab, open-files
//! switcher, close tab). The `+` used to be U+FF0B FULLWIDTH PLUS SIGN, so
//! that it stood as tall as the `X` beside it. Segoe UI, the Windows menu
//! font, has no such glyph (`GetGlyphIndicesW` answers 0xFFFF for it), so
//! Windows drew it from a fallback font. GTK dropped the same glyph for the
//! same reason, and draws an ASCII `+` at 120% of the menu font instead
//! (`ui_gtk::menu::build_right_shortcuts`). This module does that here.
//!
//! A Win32 menu bar cannot give one text item another font, so the item is
//! owner-drawn (`MF_OWNERDRAW`), and the main window answers its
//! `WM_MEASUREITEM` and `WM_DRAWITEM` through [`measure`] and [`draw`].
//! Measured on Windows 11, in a throwaway window:
//!
//! * Windows pads an owner-drawn bar item exactly as it pads a text item,
//!   14 px at 96 DPI, and ignores the height it is given. So the item asks
//!   for the glyph's width and nothing more.
//! * The rectangle `WM_DRAWITEM` hands over is the padded one, so centring
//!   the glyph in it lines it up as the text items are.
//! * The bar is drawn as a flat menu bar, not from the theme; see
//!   [`colours`].
//! * Windows measures the item once. Setting its type again, `ModifyMenuW`
//!   and `SetMenu` do not measure it again; taking it out and putting it
//!   back does. [`remeasure`] does that after a DPI or menu-font change.
//!
//! The process is per-monitor DPI aware, so the font is the menu font at
//! the window's own DPI. `GetDpiForWindow` and `SystemParametersInfoForDpi`
//! came with Windows 10 1607, so they are looked up at run time, and an
//! older Windows gets the menu font at the system DPI.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;

use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, FillRect, GetDC, GetSysColor, GetSysColorBrush,
    GetTextExtentPoint32W, GetTextMetricsW, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    BACKGROUND_MODE, COLOR_GRAYTEXT, COLOR_HIGHLIGHTTEXT, COLOR_MENUBAR, COLOR_MENUHILIGHT,
    COLOR_MENUTEXT, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ,
    LOGFONTW, SYS_COLOR_INDEX, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_FLAGS, ODS_GRAYED, ODS_HOTLIGHT,
    ODS_SELECTED, ODT_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, GetMenuItemCount, GetMenuItemInfoW, InsertMenuW, RemoveMenu,
    SystemParametersInfoW, HMENU, MENUITEMINFOW, MENU_ITEM_FLAGS, MFT_OWNERDRAW, MF_BYPOSITION,
    MF_OWNERDRAW, MF_RIGHTJUSTIFY, MIIM_DATA, MIIM_FTYPE, MIIM_ID, NONCLIENTMETRICSW,
    SPI_GETNONCLIENTMETRICS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// The item data that marks the `+`. `WM_MEASUREITEM` and `WM_DRAWITEM`
/// name an item by its command id, and the `+` shares `ID_FILE_NEW` with
/// File → New, so the data is what tells it apart. ASCII "C++" and `+`.
pub(crate) const PLUS_ITEM_DATA: usize = 0x432B_2B2B;

/// The glyph drawn: an ASCII plus, which Segoe UI has.
const PLUS_GLYPH: &str = "+";

/// How large the glyph is drawn, in percent of the menu font: GTK's 120%.
pub(crate) const PLUS_SCALE_PERCENT: i32 = 120;

/// The flags the `+` is appended and re-inserted with: owner-drawn, and
/// right-justified, which pushes it and every item after it to the bar's
/// right edge.
const PLUS_FLAGS: MENU_ITEM_FLAGS = MENU_ITEM_FLAGS(MF_OWNERDRAW.0 | MF_RIGHTJUSTIFY.0);

/// The bar the `+` was last appended to: the main window's, which the
/// process builds once. [`remeasure_recorded`] works on it, so a re-measure
/// needs neither the window's state, which a nested message loop may have
/// borrowed, nor the window's menu, which is none while the bar is hidden.
static BAR: AtomicIsize = AtomicIsize::new(0);

/// Appends the `+` to `bar`, owner-drawn and right-justified, running
/// `command` when chosen, and records `bar` for [`remeasure_recorded`].
///
/// # Errors
///
/// Whatever `AppendMenuW` reports.
pub(crate) fn append(bar: HMENU, command: u16) -> windows::core::Result<()> {
    // SAFETY: `bar` is a live menu of the caller's. For an owner-drawn
    // item the string parameter is the item data, which Windows stores
    // and never reads through.
    unsafe {
        AppendMenuW(
            bar,
            PLUS_FLAGS,
            usize::from(command),
            windows::core::PCWSTR(PLUS_ITEM_DATA as *const u16),
        )?;
    }
    BAR.store(bar.0 as isize, Ordering::Relaxed);
    Ok(())
}

/// Whether `WM_MEASUREITEM` or `WM_DRAWITEM` can be about a menu item:
/// Windows sends both with `wParam` 0 for one. Any other value is a
/// control's id, or a forged message, and is left alone, so the host never
/// writes `itemWidth` through a pointer a control message carries.
pub(crate) fn is_menu_message(wparam: usize) -> bool {
    wparam == 0
}

/// `lf_height` scaled to [`PLUS_SCALE_PERCENT`], rounded half away from
/// zero. The sign is kept: a negative height is a character height, a
/// positive one a cell height (`LOGFONTW`).
pub(crate) fn scaled_height(lf_height: i32) -> i32 {
    let scaled = i64::from(lf_height) * i64::from(PLUS_SCALE_PERCENT);
    let rounded = if scaled < 0 {
        (scaled - 50) / 100
    } else {
        (scaled + 50) / 100
    };
    i32::try_from(rounded).unwrap_or(lf_height)
}

/// The system colours the item is drawn in, fill and text, for the state
/// `WM_DRAWITEM` hands over. They are the ones Windows draws the bar's text
/// items in: the flat menu bar's colour, and a solid highlight with white
/// text while the item is hot or selected. Measured on Windows 11: the bar
/// is `COLOR_MENUBAR`, not the theme's bar background, which is white, and
/// a selected text item is a `COLOR_MENUHILIGHT` fill. Text items do not go
/// grey in an inactive window, so `ODS_INACTIVE` changes nothing.
pub(crate) fn colours(state: ODS_FLAGS) -> (SYS_COLOR_INDEX, SYS_COLOR_INDEX) {
    let highlighted = state.0 & (ODS_HOTLIGHT.0 | ODS_SELECTED.0) != 0;
    let grayed = state.0 & (ODS_GRAYED.0 | ODS_DISABLED.0) != 0;
    if highlighted {
        (COLOR_MENUHILIGHT, COLOR_HIGHLIGHTTEXT)
    } else if grayed {
        (COLOR_MENUBAR, COLOR_GRAYTEXT)
    } else {
        (COLOR_MENUBAR, COLOR_MENUTEXT)
    }
}

type GetDpiForWindowFn = unsafe extern "system" fn(HWND) -> u32;
type SystemParametersInfoForDpiFn =
    unsafe extern "system" fn(u32, u32, *mut core::ffi::c_void, u32, u32) -> BOOL;

/// `GetDpiForWindow` and `SystemParametersInfoForDpi`, where Windows has
/// them (Windows 10 1607 and later).
fn dpi_functions() -> Option<(GetDpiForWindowFn, SystemParametersInfoForDpiFn)> {
    static FUNCTIONS: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let (get_dpi, spi) = (*FUNCTIONS.get_or_init(|| {
        // SAFETY: user32 is loaded in every process that has a window,
        // and the names are NUL-terminated literals.
        unsafe {
            let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
            let get_dpi = GetProcAddress(user32, s!("GetDpiForWindow"))?;
            let spi = GetProcAddress(user32, s!("SystemParametersInfoForDpi"))?;
            Some((get_dpi as usize, spi as usize))
        }
    }))?;
    // SAFETY: the addresses are user32's exports of these names, whose
    // signatures are the two types above.
    unsafe {
        Some((
            std::mem::transmute::<usize, GetDpiForWindowFn>(get_dpi),
            std::mem::transmute::<usize, SystemParametersInfoForDpiFn>(spi),
        ))
    }
}

/// The menu font's `LOGFONTW` at `hwnd`'s DPI.
fn menu_logfont(hwnd: HWND) -> Option<LOGFONTW> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: u32::try_from(std::mem::size_of::<NONCLIENTMETRICSW>()).ok()?,
        ..Default::default()
    };
    let pv = (&raw mut ncm).cast::<core::ffi::c_void>();
    let mut found = false;
    if let Some((get_dpi, spi_for_dpi)) = dpi_functions() {
        // SAFETY: `hwnd` is the caller's window; `ncm` is sized as
        // `cbSize` says, and outlives the call.
        unsafe {
            let dpi = get_dpi(hwnd);
            found = dpi != 0
                && spi_for_dpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, pv, 0, dpi).as_bool();
        }
    }
    if !found {
        // SAFETY: as above.
        unsafe {
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(pv),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .ok()?;
        }
    }
    Some(ncm.lfMenuFont)
}

/// A font made from `lf`.
fn font_from(lf: &LOGFONTW) -> Option<OwnedFont> {
    // SAFETY: `lf` is a filled-in LOGFONTW.
    let font = unsafe { CreateFontIndirectW(lf) };
    (!font.is_invalid()).then_some(OwnedFont(font))
}

/// Where the top of the `+`'s line goes so that its baseline is the
/// baseline of a text item's line: the menu bar centres a text item's line
/// in the item (`normal_height`, `normal_ascent`), and the larger line
/// (`plus_ascent`) is hung from that baseline. Centred on its own, the
/// larger line would sit the `+` about two pixels below the `X`.
pub(crate) fn plus_line_top(
    rect_top: i32,
    rect_height: i32,
    normal_height: i32,
    normal_ascent: i32,
    plus_ascent: i32,
) -> i32 {
    let baseline = rect_top + (rect_height - normal_height) / 2 + normal_ascent;
    baseline - plus_ascent
}

/// The ascent and height of the font selected into `hdc`.
///
/// # Safety
///
/// `hdc` must be a live DC.
unsafe fn line_metrics(hdc: HDC) -> Option<(i32, i32)> {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: the caller's contract; `tm` outlives the call.
    unsafe { GetTextMetricsW(hdc, &raw mut tm) }
        .as_bool()
        .then_some((tm.tmAscent, tm.tmHeight))
}

/// A font this module made, deleted when dropped.
struct OwnedFont(HFONT);

impl Drop for OwnedFont {
    fn drop(&mut self) {
        // SAFETY: the font is this guard's, and is no longer selected into
        // a DC by the time the guard drops (`Selected` drops first).
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0 .0));
        }
    }
}

/// A font selected into a DC, put back when dropped.
struct Selected {
    hdc: HDC,
    previous: HGDIOBJ,
}

impl Selected {
    /// # Safety
    ///
    /// `hdc` must be a live DC for as long as the guard lives.
    unsafe fn new(hdc: HDC, font: &OwnedFont) -> Self {
        // SAFETY: the caller's contract; the font is live.
        let previous = unsafe { SelectObject(hdc, HGDIOBJ(font.0 .0)) };
        Self { hdc, previous }
    }
}

impl Drop for Selected {
    fn drop(&mut self) {
        // SAFETY: `new`'s contract.
        unsafe {
            SelectObject(self.hdc, self.previous);
        }
    }
}

/// Whether a `WM_MEASUREITEM` or `WM_DRAWITEM` is the `+`'s.
fn is_plus(ctl_type: u32, item_data: usize) -> bool {
    ctl_type == ODT_MENU.0 && item_data == PLUS_ITEM_DATA
}

/// Answers `WM_MEASUREITEM` for the `+`: the glyph's width at 120% of the
/// menu font. Returns whether the message was the `+`'s.
///
/// # Safety
///
/// `mis` must be the structure `WM_MEASUREITEM` points at.
pub(crate) unsafe fn measure(hwnd: HWND, mis: &mut MEASUREITEMSTRUCT) -> bool {
    if !is_plus(mis.CtlType.0, mis.itemData) {
        return false;
    }
    let Some(mut lf) = menu_logfont(hwnd) else {
        return false;
    };
    lf.lfHeight = scaled_height(lf.lfHeight);
    let Some(font) = font_from(&lf) else {
        return false;
    };
    // SAFETY: `hwnd` is the caller's window; the DC is released below.
    let hdc = unsafe { GetDC(Some(hwnd)) };
    if hdc.is_invalid() {
        return false;
    }
    let glyph: Vec<u16> = PLUS_GLYPH.encode_utf16().collect();
    let mut size = SIZE::default();
    let measured = {
        // SAFETY: `hdc` is live until the `ReleaseDC` below.
        let _selected = unsafe { Selected::new(hdc, &font) };
        // SAFETY: as above; `size` outlives the call.
        unsafe { GetTextExtentPoint32W(hdc, &glyph, &raw mut size) }.as_bool()
    };
    // SAFETY: the DC `GetDC` handed out, released once.
    unsafe {
        ReleaseDC(Some(hwnd), hdc);
    }
    if !measured {
        return false;
    }
    // Windows adds its own padding, as for a text item, and ignores the
    // height on a menu bar, so only the width is set.
    mis.itemWidth = u32::try_from(size.cx).unwrap_or(0);
    true
}

/// Paints the `+` for `WM_DRAWITEM`, in the colours of [`colours`].
/// Returns whether it did: not when the message is not the `+`'s, or no
/// font could be made.
///
/// # Safety
///
/// `dis` must be the structure `WM_DRAWITEM` points at.
pub(crate) unsafe fn draw(hwnd: HWND, dis: &DRAWITEMSTRUCT) -> bool {
    if !is_plus(dis.CtlType.0, dis.itemData) {
        return false;
    }
    let Some(normal_lf) = menu_logfont(hwnd) else {
        tracing::debug!("no menu font for the menu bar's +; leaving it to Windows");
        return false;
    };
    let mut plus_lf = normal_lf;
    plus_lf.lfHeight = scaled_height(normal_lf.lfHeight);
    let (Some(normal), Some(font)) = (font_from(&normal_lf), font_from(&plus_lf)) else {
        tracing::debug!("no font for the menu bar's +; leaving it to Windows");
        return false;
    };
    let hdc = dis.hDC;
    let mut rect: RECT = dis.rcItem;
    let (fill, text) = colours(dis.itemState);
    // SAFETY: `hdc` and `rect` are the ones `WM_DRAWITEM` handed over,
    // valid for the message. The fill also clears a highlight the item
    // had before.
    unsafe {
        FillRect(hdc, &raw const rect, GetSysColorBrush(fill));
    }
    // SAFETY: reads a system colour; no pointers.
    let text = COLORREF(unsafe { GetSysColor(text) });
    let mut glyph: Vec<u16> = PLUS_GLYPH.encode_utf16().collect();
    // SAFETY: `hdc` is live for the message; the previous font, text
    // colour and background mode are put back.
    unsafe {
        let normal_metrics = {
            let _selected = Selected::new(hdc, &normal);
            line_metrics(hdc)
        };
        let _selected = Selected::new(hdc, &font);
        let mut format = DT_CENTER | DT_SINGLELINE | DT_NOPREFIX;
        if let (Some((normal_ascent, normal_height)), Some((plus_ascent, plus_height))) =
            (normal_metrics, line_metrics(hdc))
        {
            rect.top = plus_line_top(
                dis.rcItem.top,
                dis.rcItem.bottom - dis.rcItem.top,
                normal_height,
                normal_ascent,
                plus_ascent,
            );
            rect.bottom = rect.top + plus_height;
        } else {
            format |= DT_VCENTER;
        }
        let old_mode = SetBkMode(hdc, TRANSPARENT);
        let old_colour = SetTextColor(hdc, text);
        DrawTextW(hdc, glyph.as_mut_slice(), &raw mut rect, format);
        SetTextColor(hdc, old_colour);
        SetBkMode(
            hdc,
            BACKGROUND_MODE(u32::try_from(old_mode).unwrap_or(TRANSPARENT.0)),
        );
    }
    true
}

/// The `+`'s position in `bar`, found by its item data.
pub(crate) fn position(bar: HMENU) -> Option<u32> {
    // SAFETY: `bar` is a live menu; `info` outlives each call.
    unsafe {
        let count = u32::try_from(GetMenuItemCount(Some(bar))).ok()?;
        (0..count).find(|&pos| {
            let mut info = MENUITEMINFOW {
                cbSize: u32::try_from(std::mem::size_of::<MENUITEMINFOW>()).unwrap_or(0),
                fMask: MIIM_DATA | MIIM_FTYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(bar, pos, true, &raw mut info).is_ok()
                && info.fType.0 & MFT_OWNERDRAW.0 != 0
                && info.dwItemData == PLUS_ITEM_DATA
        })
    }
}

/// Has Windows measure the `+` again, after the window's DPI or the menu
/// font changed, by putting a new `+` where it is and taking the old one
/// out. The new one goes in first, so a failure leaves the old one. The
/// caller redraws the bar.
pub(crate) fn remeasure(bar: HMENU) {
    let Some(pos) = position(bar) else {
        return;
    };
    // SAFETY: `bar` is a live menu and `pos` one of its items.
    unsafe {
        let mut info = MENUITEMINFOW {
            cbSize: u32::try_from(std::mem::size_of::<MENUITEMINFOW>()).unwrap_or(0),
            fMask: MIIM_ID,
            ..Default::default()
        };
        if GetMenuItemInfoW(bar, pos, true, &raw mut info).is_err() {
            return;
        }
        if let Err(e) = InsertMenuW(
            bar,
            pos,
            MF_BYPOSITION | PLUS_FLAGS,
            info.wID as usize,
            windows::core::PCWSTR(PLUS_ITEM_DATA as *const u16),
        ) {
            tracing::warn!(error = ?e, "re-measuring the menu bar's + failed");
            return;
        }
        // The old `+` is now one place on.
        if let Err(e) = RemoveMenu(bar, pos + 1, MF_BYPOSITION) {
            tracing::warn!(error = ?e, "taking the old menu bar's + out failed");
        }
    }
}

/// [`remeasure`] on the bar [`append`] recorded, if any.
pub(crate) fn remeasure_recorded() {
    let bar = HMENU(BAR.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    if !bar.is_invalid() {
        remeasure(bar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateMenu, DestroyMenu, GetMenuItemID, MF_STRING,
    };

    #[test]
    fn the_height_is_scaled_to_120_percent_keeping_its_sign() {
        assert_eq!(scaled_height(-12), -14);
        assert_eq!(scaled_height(-15), -18);
        assert_eq!(scaled_height(-16), -19);
        assert_eq!(scaled_height(-13), -16);
        assert_eq!(scaled_height(12), 14);
        assert_eq!(scaled_height(0), 0);
        assert_eq!(scaled_height(i32::MIN), i32::MIN);
    }

    #[test]
    fn the_colours_follow_the_item_state() {
        let plain = (COLOR_MENUBAR, COLOR_MENUTEXT);
        let highlighted = (COLOR_MENUHILIGHT, COLOR_HIGHLIGHTTEXT);
        assert_eq!(colours(ODS_FLAGS(0)), plain);
        assert_eq!(colours(ODS_HOTLIGHT), highlighted);
        assert_eq!(colours(ODS_SELECTED), highlighted);
        assert_eq!(colours(ODS_GRAYED), (COLOR_MENUBAR, COLOR_GRAYTEXT));
        assert_eq!(
            colours(windows::Win32::UI::Controls::ODS_INACTIVE),
            plain,
            "an inactive window's text items do not go grey"
        );
        assert_eq!(colours(windows::Win32::UI::Controls::ODS_NOACCEL), plain);
    }

    #[test]
    fn the_plus_shares_the_text_items_baseline() {
        // Segoe UI at 96 DPI: a 16 px line with a 13 px ascent at the menu
        // font's size, and a 19 px line with a 15 px ascent at 120%, in a
        // 19 px item. A text item's baseline is at 1 + 13 = 14.
        let top = plus_line_top(31, 19, 16, 13, 15);
        assert_eq!(top + 15, 31 + 14, "the larger line's baseline is the same");
        assert_eq!(top, 30, "so its line starts a pixel above the item");
        // A line no larger than the menu font's sits as a text item's does.
        assert_eq!(plus_line_top(0, 19, 16, 13, 13), 1);
    }

    #[test]
    fn only_a_menu_message_is_answered() {
        assert!(is_menu_message(0));
        assert!(!is_menu_message(1));
        assert!(!is_menu_message(usize::MAX));
    }

    #[test]
    fn only_the_plus_is_answered() {
        assert!(is_plus(ODT_MENU.0, PLUS_ITEM_DATA));
        assert!(!is_plus(ODT_MENU.0, 0));
        assert!(!is_plus(
            windows::Win32::UI::Controls::ODT_TAB.0,
            PLUS_ITEM_DATA
        ));
    }

    #[test]
    fn the_plus_is_found_and_put_back_where_it_was() {
        // SAFETY: the menu is this test's, destroyed at its end.
        unsafe {
            let bar = CreateMenu().expect("a menu");
            AppendMenuW(bar, MF_STRING, 1, w!("&File")).unwrap();
            append(bar, 2).unwrap();
            AppendMenuW(bar, MF_STRING, 3, w!("X")).unwrap();
            assert_eq!(position(bar), Some(1));
            remeasure(bar);
            remeasure(bar);
            assert_eq!(position(bar), Some(1), "back at its position");
            assert_eq!(GetMenuItemCount(Some(bar)), 3);
            assert_eq!(GetMenuItemID(bar, 1), 2, "with its command");
            let mut info = MENUITEMINFOW {
                cbSize: u32::try_from(std::mem::size_of::<MENUITEMINFOW>()).unwrap(),
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(bar, 1, true, &raw mut info).unwrap();
            assert_eq!(
                info.fType.0 & PLUS_FLAGS.0,
                PLUS_FLAGS.0,
                "still owner-drawn and right-justified"
            );
            let _ = DestroyMenu(bar);
        }
    }
}
