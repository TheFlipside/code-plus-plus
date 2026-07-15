//! Win32 toolbar — horizontal strip of icon buttons that sits between
//! the menu bar and the tab control.
//!
//! Buttons are bound to the existing menu command IDs declared in
//! `lib.rs` (`ID_FILE_NEW`, `ID_EDIT_UNDO`, …) so a click fires
//! `WM_COMMAND` that the menu's existing handlers already dispatch on
//! — the toolbar adds zero new command-dispatch logic. Buttons whose
//! commands aren't implemented yet (Sync V/H Scroll, Define Language,
//! the macro family, etc.) get a unique ID anyway so the spec layout
//! is complete, but the `WM_COMMAND` falls through the switch with no
//! handler.
//!
//! ## Bitmap pipeline
//!
//! Per-button PNG bytes are baked in via `include_bytes!`, decoded
//! with the `png` crate at toolbar-creation time (one-shot cost
//! during window setup), and turned into 32bpp BGRA top-down DIB
//! sections that go into a single `HIMAGELIST` shared by all
//! buttons. The pipeline picks 24x24 bitmaps for system DPI < 144
//! (i.e. < 150% scaling) and 48x48 above — a coarse threshold that's
//! good enough for the chrome that's still 96-DPI-only elsewhere; a
//! later phase that does proper DPI awareness can reuse the same
//! per-button entries.
//!
//! ## State
//!
//! The toolbar's runtime handle and image list live on
//! `WindowState` (added in `lib.rs`); this module exposes only free
//! functions that take those handles in. The button table itself is
//! a `&'static` array — adding or reordering buttons is one edit
//! here, no per-button scaffolding elsewhere.

// Same FFI / cast rationale as `lib.rs` — this is the Win32
// toolbar control wrapper. PNG decoding and DIB construction
// use short single-character bindings (`r`, `g`, `b`, `a`,
// `w`, `h`) at tight numeric loops where descriptive names
// would clutter; allowed at module scope so the inner loops
// stay readable.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::many_single_char_names
)]

use core::ffi::c_void;
use std::io::Cursor;

use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, GetDC, GetDeviceCaps, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, LOGPIXELSY,
};
use windows::Win32::UI::Controls::{
    ImageList_Add, ImageList_Create, CCS_NODIVIDER, CCS_NOPARENTALIGN, CCS_NORESIZE, HIMAGELIST,
    ILC_COLOR32, NMTBGETINFOTIPW, TBBUTTON, TBSTATE_CHECKED, TBSTATE_ENABLED, TBSTYLE_BUTTON,
    TBSTYLE_CHECK, TBSTYLE_FLAT, TBSTYLE_SEP, TBSTYLE_TOOLTIPS, TB_ADDBUTTONS, TB_AUTOSIZE,
    TB_BUTTONSTRUCTSIZE, TB_GETSTATE, TB_SETIMAGELIST, TB_SETSTATE, TOOLBARCLASSNAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, SendMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CHILD, WS_VISIBLE,
};

use crate::{
    ID_EDIT_COPY, ID_EDIT_CUT, ID_EDIT_PASTE, ID_EDIT_REDO, ID_EDIT_UNDO, ID_FILE_CLOSE,
    ID_FILE_CLOSE_ALL, ID_FILE_NEW, ID_FILE_OPEN, ID_FILE_PRINT, ID_FILE_SAVE, ID_FILE_SAVE_ALL,
    ID_MACRO_PLAY, ID_MACRO_RECORD, ID_MACRO_RUN_MULTIPLE, ID_MACRO_SAVE, ID_MACRO_STOP,
    ID_SEARCH_FIND, ID_SEARCH_REPLACE, ID_TOOLS_DEFINE_LANG, ID_TOOLS_MONITORING, ID_VIEW_DOCLIST,
    ID_VIEW_DOCMAP, ID_VIEW_FOLDER_AS_WORKSPACE, ID_VIEW_FUNCLIST, ID_VIEW_SHOW_ALL_CHARS,
    ID_VIEW_SHOW_INDENT_GUIDE, ID_VIEW_SYNC_H_SCROLL, ID_VIEW_SYNC_V_SCROLL, ID_VIEW_WORDWRAP,
    ID_VIEW_ZOOMIN, ID_VIEW_ZOOMOUT,
};

// --- Constants ----------------------------------------------------------------

/// Bitmap edge length at 96 DPI. Matches the SVG viewBox dimension.
pub const BITMAP_PX_STD: i32 = 24;

/// Bitmap edge length at high DPI. Matches the `@2x` PNG suffix.
pub const BITMAP_PX_HIDPI: i32 = 48;

/// DPI threshold (inclusive) at which we switch from the 24px bitmap
/// set to the 48px set. 144 DPI is 150% scaling. Below that the 24px
/// set scales acceptably; at-or-above we want the higher-resolution
/// source so the toolbar doesn't look soft.
/// DPI breakpoint that flips the toolbar (and tab strip) between
/// their LO and HIDPI bitmap sizes. `pub(crate)` so the tab-strip
/// owner-draw path can pick its own 16/32 vs the toolbar's 24/48
/// from the same threshold.
pub(crate) const HIDPI_DPI_THRESHOLD: i32 = 144;

/// Per-button vertical chrome (top + bottom padding inside the
/// toolbar's window). Combined with the bitmap height to produce
/// the toolbar's overall pixel height.
const TOOLBAR_VERTICAL_CHROME_PX: i32 = 6;

// --- Embedded PNGs ------------------------------------------------------------
//
// `include_bytes!` resolves relative to this source file. From
// `crates/ui_win32/src/toolbar.rs` to `<repo>/assets/icons/` is two
// parents up plus one down — every entry uses the same "../../../..."
// shape so the macro hits the canonical asset location.

macro_rules! icon24 {
    ($name:literal) => {
        include_bytes!(concat!("../../../assets/icons/", $name, ".png"))
    };
}
macro_rules! icon48 {
    ($name:literal) => {
        include_bytes!(concat!("../../../assets/icons/", $name, "@2x.png"))
    };
}

// --- Button table -------------------------------------------------------------

/// One toolbar button (or a separator when `cmd_id == 0`).
///
/// Buttons that flag `is_check = true` use `TBSTYLE_CHECK` so a click
/// toggles the pressed/normal state — Win32 themes those as a
/// permanent highlighted background while checked, which is how the
/// "Word Wrap is on" / "Show All Chars is on" indication will read.
/// M2 leaves the check states off; M3 will sync them with Scintilla
/// `SCI_GET*` queries on `SCN_UPDATEUI`.
struct ButtonDef {
    cmd_id: u16,
    icon24: &'static [u8],
    icon48: &'static [u8],
    tooltip: &'static str,
    /// Initial enabled state. Greyed buttons (`enabled = false`) are
    /// the ones whose underlying feature isn't implemented yet —
    /// Sync V/H Scroll, Monitoring. Clicking them does nothing
    /// because the `WM_COMMAND` falls through, but greyed is the
    /// honest visual cue.
    enabled: bool,
    /// Toggle-style (`TBSTYLE_CHECK`) vs. push-style (`TBSTYLE_BUTTON`).
    is_check: bool,
}

const SEP: ButtonDef = ButtonDef {
    cmd_id: 0,
    icon24: &[],
    icon48: &[],
    tooltip: "",
    enabled: false,
    is_check: false,
};

const fn push(
    cmd_id: u16,
    icon24: &'static [u8],
    icon48: &'static [u8],
    tooltip: &'static str,
) -> ButtonDef {
    ButtonDef {
        cmd_id,
        icon24,
        icon48,
        tooltip,
        enabled: true,
        is_check: false,
    }
}

const fn check(
    cmd_id: u16,
    icon24: &'static [u8],
    icon48: &'static [u8],
    tooltip: &'static str,
) -> ButtonDef {
    ButtonDef {
        cmd_id,
        icon24,
        icon48,
        tooltip,
        enabled: true,
        is_check: true,
    }
}

const fn greyed(
    cmd_id: u16,
    icon24: &'static [u8],
    icon48: &'static [u8],
    tooltip: &'static str,
) -> ButtonDef {
    ButtonDef {
        cmd_id,
        icon24,
        icon48,
        tooltip,
        enabled: false,
        is_check: false,
    }
}

/// The toolbar layout, in left-to-right paint order. Order matches
/// the spec from the m2 issue: 32 buttons across 10 separator-
/// delimited groups (the trailing `SEP` is the divider before the
/// plugin-contributed buttons that future `NPPM_ADDTOOLBARICON`
/// calls will append). Adding / reordering / removing a button is
/// one edit here; the rest of the file iterates this table without
/// per-entry scaffolding.
///
/// `assets/icons/` ships a handful of icons the spec doesn't
/// reference today (`find-next`, `show-whitespace`, `macro-pause`)
/// — those are reserved for follow-up milestones (incremental Find
/// Next as a separate toolbar action; whitespace-only render mode;
/// macro pause/resume) and stay generated-but-unembedded until then.
const BUTTONS: &[ButtonDef] = &[
    // File ops
    push(ID_FILE_NEW, icon24!("new"), icon48!("new"), "New"),
    push(ID_FILE_OPEN, icon24!("open"), icon48!("open"), "Open..."),
    push(ID_FILE_SAVE, icon24!("save"), icon48!("save"), "Save"),
    push(
        ID_FILE_SAVE_ALL,
        icon24!("save-all"),
        icon48!("save-all"),
        "Save All",
    ),
    push(ID_FILE_CLOSE, icon24!("close"), icon48!("close"), "Close"),
    push(
        ID_FILE_CLOSE_ALL,
        icon24!("close-all"),
        icon48!("close-all"),
        "Close All",
    ),
    push(ID_FILE_PRINT, icon24!("print"), icon48!("print"), "Print"),
    SEP,
    // Clipboard
    push(ID_EDIT_CUT, icon24!("cut"), icon48!("cut"), "Cut"),
    push(ID_EDIT_COPY, icon24!("copy"), icon48!("copy"), "Copy"),
    push(ID_EDIT_PASTE, icon24!("paste"), icon48!("paste"), "Paste"),
    SEP,
    // History
    push(ID_EDIT_UNDO, icon24!("undo"), icon48!("undo"), "Undo"),
    push(ID_EDIT_REDO, icon24!("redo"), icon48!("redo"), "Redo"),
    SEP,
    // Search
    push(ID_SEARCH_FIND, icon24!("find"), icon48!("find"), "Find..."),
    push(
        ID_SEARCH_REPLACE,
        icon24!("replace"),
        icon48!("replace"),
        "Replace...",
    ),
    SEP,
    // Zoom
    push(
        ID_VIEW_ZOOMIN,
        icon24!("zoom-in"),
        icon48!("zoom-in"),
        "Zoom In (Ctrl + Mouse Wheel Up)",
    ),
    push(
        ID_VIEW_ZOOMOUT,
        icon24!("zoom-out"),
        icon48!("zoom-out"),
        "Zoom Out (Ctrl + Mouse Wheel Down)",
    ),
    SEP,
    // Sync scrolling — feature not implemented yet, greyed by default;
    // becomes active when the user enables sync mode.
    greyed(
        ID_VIEW_SYNC_V_SCROLL,
        icon24!("sync-scroll-vertical"),
        icon48!("sync-scroll-vertical"),
        "Synchronize Vertical Scrolling",
    ),
    greyed(
        ID_VIEW_SYNC_H_SCROLL,
        icon24!("sync-scroll-horizontal"),
        icon48!("sync-scroll-horizontal"),
        "Synchronize Horizontal Scrolling",
    ),
    SEP,
    // View toggles — TBSTYLE_CHECK so the button stays pressed-look
    // while the option is on. Initial check state is set in M3 by
    // querying Scintilla for the current setting.
    check(
        ID_VIEW_WORDWRAP,
        icon24!("word-wrap"),
        icon48!("word-wrap"),
        "Word Wrap",
    ),
    check(
        ID_VIEW_SHOW_ALL_CHARS,
        icon24!("show-all-chars"),
        icon48!("show-all-chars"),
        "Show All Characters",
    ),
    check(
        ID_VIEW_SHOW_INDENT_GUIDE,
        icon24!("show-indent-guide"),
        icon48!("show-indent-guide"),
        "Show Indent Guide",
    ),
    SEP,
    // Tools — Define Language is push-style (opens a dialog); the
    // other four are toggle-style indicators for panel visibility.
    push(
        ID_TOOLS_DEFINE_LANG,
        icon24!("define-language"),
        icon48!("define-language"),
        "Define your language...",
    ),
    check(
        ID_VIEW_DOCMAP,
        icon24!("document-map"),
        icon48!("document-map"),
        "Document Map",
    ),
    check(
        ID_VIEW_DOCLIST,
        icon24!("document-list"),
        icon48!("document-list"),
        "Document List",
    ),
    check(
        ID_VIEW_FUNCLIST,
        icon24!("function-list"),
        icon48!("function-list"),
        "Function List",
    ),
    check(
        ID_VIEW_FOLDER_AS_WORKSPACE,
        icon24!("folder-workspace"),
        icon48!("folder-workspace"),
        "Folder as Workspace",
    ),
    SEP,
    // tail -f — greyed until the user opts in to monitoring a file.
    greyed(
        ID_TOOLS_MONITORING,
        icon24!("monitoring"),
        icon48!("monitoring"),
        "Monitoring (tail -f)",
    ),
    SEP,
    // Macros
    push(
        ID_MACRO_RECORD,
        icon24!("macro-record"),
        icon48!("macro-record"),
        "Start Recording",
    ),
    push(
        ID_MACRO_STOP,
        icon24!("macro-stop"),
        icon48!("macro-stop"),
        "Stop Recording",
    ),
    push(
        ID_MACRO_PLAY,
        icon24!("macro-play"),
        icon48!("macro-play"),
        "Playback",
    ),
    push(
        ID_MACRO_RUN_MULTIPLE,
        icon24!("run"),
        icon48!("run"),
        "Run a Macro Multiple Times...",
    ),
    push(
        ID_MACRO_SAVE,
        icon24!("save-macro"),
        icon48!("save-macro"),
        "Save Current Recorded Macro...",
    ),
    SEP,
    // Plugin-contributed buttons spawn after this trailing separator
    // — Phase 4 m9 wires NPPM_ADDTOOLBARICON to write into here.
];

// --- DPI / sizing -------------------------------------------------------------

/// Probe the system DPI via the desktop DC. Returns a value in
/// dots-per-inch (typically 96, 120, 144, 192). Falls back to 96 if
/// the DC probe fails — a sane default that picks the 24px bitmap
/// set, the conservative branch.
pub(crate) fn system_dpi_y() -> i32 {
    // SAFETY: GetDC(None)/ReleaseDC is the standard "get a screen-
    // wide DC for one read" pattern; both calls are paired.
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return 96;
        }
        let dpi = GetDeviceCaps(Some(hdc), LOGPIXELSY);
        ReleaseDC(None, hdc);
        if dpi <= 0 {
            96
        } else {
            dpi
        }
    }
}

/// Pixel size to use for one toolbar bitmap edge based on the system
/// DPI. 24 below the threshold, 48 at-or-above.
pub fn pick_bitmap_size() -> i32 {
    if system_dpi_y() >= HIDPI_DPI_THRESHOLD {
        BITMAP_PX_HIDPI
    } else {
        BITMAP_PX_STD
    }
}

/// Toolbar window's pixel height: bitmap edge + chrome.
pub fn toolbar_height_px(bitmap_px: i32) -> i32 {
    bitmap_px + TOOLBAR_VERTICAL_CHROME_PX
}

/// Natural minimum width (in pixels) of the toolbar laid out as a
/// single horizontal strip — the smallest width at which every
/// button and separator is fully visible. Sent via `TB_GETMAXSIZE`,
/// which the common-controls toolbar fills with the rectangle
/// needed to display all `TBBUTTON` entries currently registered;
/// the call has no side effects.
///
/// Used by `Win32Ui::run` as the floor on the main window's inner
/// width so the user never has to manually widen the window to
/// reveal toolbar buttons that were cut off by the default
/// 900-pixel size.
///
/// # Safety
///
/// `toolbar_hwnd` must be a live HWND created by `create_toolbar`,
/// past the `TB_ADDBUTTONS`/`TB_AUTOSIZE` calls that finalise the
/// button layout — caller invariant since `create_toolbar` returns
/// the HWND only after both messages have been sent.
pub fn natural_min_width_px(toolbar_hwnd: HWND) -> i32 {
    use windows::Win32::Foundation::SIZE;
    // `TB_GETMAXSIZE = WM_USER + 53`. windows-rs does not export it
    // (the constant lives behind a feature gate that's not currently
    // wired in for `Win32::UI::Controls`); the numeric value is
    // stable SDK ABI. Fills `lparam` (a `*mut SIZE`) with the
    // minimum width × button-row height required for the current
    // button set. Plain `//` because rustdoc silently drops doc
    // comments on local `const` items inside function bodies.
    const TB_GETMAXSIZE: u32 = 0x400 + 53;
    let mut size = SIZE::default();
    unsafe {
        SendMessageW(
            toolbar_hwnd,
            TB_GETMAXSIZE,
            None,
            Some(LPARAM(&raw mut size as isize)),
        );
    }
    size.cx.max(0)
}

// --- PNG decode -> HBITMAP ---------------------------------------------------

/// Decode `png_bytes` to a `width * height * 4` BGRA buffer with
/// premultiplied alpha. Premultiplication is what
/// `ILC_COLOR32`-flagged image lists expect for `AlphaBlend`
/// rendering during draw — without it, anti-aliased edges of the
/// SVG renders pick up dark fringes when blended over the toolbar
/// background.
fn decode_png_to_bgra(png_bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder.read_info().map_err(map_png_err)?;
    // Reject anything other than 8-bit-per-channel sources up
    // front. The compile-time-embedded icons are always 8-bit
    // RGBA (svglib + PIL convention), but the same `decode_png_*`
    // helper is the natural reuse point for `NPPM_ADDTOOLBARICON`
    // when plugin-supplied icons land — without this guard, a
    // 16-bit RGBA plugin icon would double the per-pixel byte
    // count and overflow the DIB section the caller sized for
    // `w * h * 4`. Belt-and-braces with the size check at copy
    // time below.
    let bit_depth = reader.info().bit_depth;
    if bit_depth != png::BitDepth::Eight {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("toolbar PNG must be 8-bit-per-channel; got {bit_depth:?}"),
        ));
    }
    // `output_buffer_size` returns None only when the colour-type
    // metadata is malformed; a zero-length buffer makes
    // `next_frame` fail loudly with a buffer-too-small error
    // rather than silently misparse — we never reach the buggy
    // code path with a 0-byte allocation.
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(map_png_err)?;
    let (w, h) = (info.width, info.height);

    // Cap dimensions at i32::MAX so the later cast to `biWidth` /
    // `biHeight` (both `i32`) can't wrap. Compile-time icons are
    // 24x24 / 48x48 so this branch is unreachable today; the cap
    // is forward-cover for the plugin-icon reuse path.
    if w > i32::MAX as u32 || h > i32::MAX as u32 {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("toolbar PNG dimensions exceed i32::MAX ({w}x{h})"),
        ));
    }

    // The icon-author pipeline (svglib + PIL post-process) always
    // emits 8-bit RGBA. Defend against future SVG additions that
    // accidentally drop the alpha channel — RGB inputs get a
    // synthesised alpha=255 plane instead of producing a pixel-
    // misaligned BGRA buffer.
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let src = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity((w * h) as usize * 4);
            for px in src.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        other => {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                format!("toolbar PNG has unsupported colour type {other:?}"),
            ));
        }
    };

    // Swap R/B and premultiply alpha. Win32 32bpp DIBs are BGRA
    // (low byte = B); ImageList ILC_COLOR32 wants premultiplied.
    let mut bgra = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        // Premultiply: c' = c * a / 255 — exact division to keep
        // black anti-aliased edges from drifting.
        let pm = |c: u8| -> u8 { ((u32::from(c) * u32::from(a) + 127) / 255) as u8 };
        bgra.extend_from_slice(&[pm(b), pm(g), pm(r), a]);
    }
    Ok((bgra, w, h))
}

fn map_png_err<E: std::fmt::Display>(e: E) -> windows::core::Error {
    windows::core::Error::new(
        windows::Win32::Foundation::E_FAIL,
        format!("toolbar PNG decode: {e}"),
    )
}

/// Wrap a PNG byte slice into a top-down 32bpp DIB section
/// `HBITMAP` with premultiplied BGRA pixels. The caller owns the
/// returned `HBITMAP` and is responsible for `DeleteObject`.
///
/// `pub(crate)` so the tab-strip owner-draw path (in `lib.rs`) can
/// share the same decode logic — both the toolbar and the tab strip
/// load PNGs from `assets/icons/` at startup, and duplicating the
/// 60-line PNG → BGRA → DIB pipeline would drift over time.
///
/// # Safety
///
/// `CreateDIBSection` is the only intrinsically `unsafe` step; the
/// caller takes on the bitmap ownership contract documented above.
pub(crate) unsafe fn png_to_hbitmap(png_bytes: &[u8]) -> Result<HBITMAP> {
    let (bgra, w, h) = decode_png_to_bgra(png_bytes)?;

    // BITMAPINFOHEADER: 32bpp, BI_RGB, NEGATIVE biHeight for top-
    // down (so our row-0-first BGRA buffer maps directly without a
    // vertical flip). Build the whole BITMAPINFO in one initialiser
    // so clippy doesn't complain about Default-then-reassign.
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        ..BITMAPINFO::default()
    };

    // SAFETY: CreateDIBSection is the standard Win32 path for
    // creating an HBITMAP whose pixel data is accessible through
    // a host pointer. With DIB_RGB_COLORS and a 32bpp/BI_RGB
    // header we get a top-down DIB whose `bits` is a tightly-
    // packed BGRA buffer of `w * h * 4` bytes.
    let mut bits: *mut c_void = core::ptr::null_mut();
    // SAFETY: CreateDIBSection's section-handle parameter is None for
    // a standalone (non-shared) bitmap; offset is unused when
    // section is None. The bits pointer it writes through is valid
    // while the bitmap is alive.
    let hbm =
        unsafe { CreateDIBSection(None, &raw const bmi, DIB_RGB_COLORS, &raw mut bits, None, 0)? };
    if hbm.is_invalid() || bits.is_null() {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "CreateDIBSection returned null bits",
        ));
    }

    // Copy the decoded BGRA buffer into the DIB's pixel memory.
    // Hard check (not `debug_assert`) — the unsafe copy below has
    // exactly one safe-input invariant ("`bgra.len() == w * h * 4`")
    // and we want it enforced in release too. The check is O(1) and
    // runs once per bitmap at toolbar creation; cost is negligible.
    let byte_len = bgra.len();
    let expected = (w as usize) * (h as usize) * 4;
    if byte_len != expected {
        // Also free the DIB so a malformed PNG doesn't leak it.
        let _ = unsafe { DeleteObject(hbm.into()) };
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("decoded BGRA length {byte_len} != expected {expected}"),
        ));
    }
    // SAFETY: `bits` points to `byte_len` bytes of writable BGRA
    // storage owned by the DIB section we just created (verified by
    // the explicit length check immediately above; `bgra.as_ptr()`
    // remains valid through the call since the Vec is live until
    // function return).
    unsafe { core::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), byte_len) };

    Ok(hbm)
}

/// Build the `HIMAGELIST` containing every non-separator button's
/// bitmap, in `BUTTONS` order. The image list lives for the
/// lifetime of the window (destroyed in `WM_DESTROY` via
/// `ImageList_Destroy`).
unsafe fn build_image_list(bitmap_px: i32) -> Result<HIMAGELIST> {
    let bitmap_count = BUTTONS.iter().filter(|b| b.cmd_id != 0).count() as i32;
    // SAFETY: ImageList_Create is documented to return a non-null
    // HIMAGELIST on success; the null check below catches OOM /
    // theme-engine failure cases.
    let il = unsafe { ImageList_Create(bitmap_px, bitmap_px, ILC_COLOR32, bitmap_count, 0) };
    if il.0 == 0 {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "ImageList_Create returned null",
        ));
    }
    for (slot, btn) in BUTTONS.iter().filter(|b| b.cmd_id != 0).enumerate() {
        let png = if bitmap_px == BITMAP_PX_HIDPI {
            btn.icon48
        } else {
            btn.icon24
        };
        // SAFETY: png_to_hbitmap returns a freshly-allocated DIB
        // whose pixel format matches what ImageList_Add expects for
        // ILC_COLOR32. The bitmap is deleted right after Add since
        // ImageList copies the bits in.
        let hbm = unsafe { png_to_hbitmap(png) }?;
        let added_index = unsafe { ImageList_Add(il, hbm, None) };
        let _ = unsafe { DeleteObject(hbm.into()) };
        // `ImageList_Add` returns -1 on failure. If a single Add
        // fails partway through, every subsequent button's
        // `iBitmap` index in the TBBUTTON table would be off by
        // one and buttons would render the wrong icon (or
        // garbage). Bail out with an error rather than ship a
        // visually-broken toolbar.
        if added_index < 0 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                format!(
                    "ImageList_Add returned -1 for button slot {slot} ({})",
                    btn.tooltip
                ),
            ));
        }
        debug_assert_eq!(
            added_index, slot as i32,
            "ImageList_Add appended out-of-order — bitmap indices in BUTTONS are stale"
        );
    }
    Ok(il)
}

// --- Toolbar creation ---------------------------------------------------------

/// Result of toolbar setup: window handle plus the image list (so
/// the parent can stash both on `WindowState` and free the image
/// list on shutdown).
pub struct ToolbarHandles {
    pub hwnd: HWND,
    pub image_list: HIMAGELIST,
    pub bitmap_px: i32,
}

/// Create the toolbar control as a child of `parent`, populate it
/// with [`BUTTONS`], wire the image list, and return the handles.
///
/// The toolbar is created with `CCS_NORESIZE | CCS_NOPARENTALIGN |
/// CCS_NODIVIDER` so its position and size are entirely under our
/// control — `layout_children` decides where it sits and how wide
/// it is, just like the tab and status bars. Without `CCS_NORESIZE`
/// the toolbar would auto-anchor to the top of the parent and
/// confuse the layout pass.
pub unsafe fn create_toolbar(parent: HWND, instance: HMODULE) -> Result<ToolbarHandles> {
    let bitmap_px = pick_bitmap_size();

    // SAFETY: standard CreateWindowExW pattern. TOOLBARCLASSNAME is
    // a constant from the windows crate; the parent HWND is owned
    // by the caller and stays alive for the lifetime of this child.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            TOOLBARCLASSNAME,
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WINDOW_STYLE(
                    TBSTYLE_FLAT
                        | TBSTYLE_TOOLTIPS
                        | (CCS_NORESIZE | CCS_NOPARENTALIGN | CCS_NODIVIDER) as u32,
                ),
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            Some(instance.into()),
            None,
        )?
    };

    // Toolbar requires TB_BUTTONSTRUCTSIZE to be sent before any
    // TB_ADDBUTTONS — it tells the control which TBBUTTON shape
    // (32-bit vs. 64-bit) the host is using.
    unsafe {
        SendMessageW(
            hwnd,
            TB_BUTTONSTRUCTSIZE,
            Some(WPARAM(core::mem::size_of::<TBBUTTON>())),
            None,
        );
    }

    // Build the image list and attach it. Index 0 in TB_SETIMAGELIST
    // is the default-state list; pressed/hot/disabled lists could
    // be supplied as well but the system theme handles those
    // transitions for us.
    // SAFETY: build_image_list returns a non-null HIMAGELIST on
    // success (it errors out otherwise); SendMessageW with
    // TB_SETIMAGELIST treats lparam as an HIMAGELIST handle.
    let image_list = unsafe { build_image_list(bitmap_px) }?;
    unsafe {
        SendMessageW(
            hwnd,
            TB_SETIMAGELIST,
            Some(WPARAM(0)),
            Some(LPARAM(image_list.0)),
        );
    }

    // Build the TBBUTTON array. Bitmap index counts up across
    // non-separator entries in the same order as `build_image_list`
    // populated the HIMAGELIST.
    let mut tb_buttons: Vec<TBBUTTON> = Vec::with_capacity(BUTTONS.len());
    let mut bitmap_idx: i32 = 0;
    for btn in BUTTONS {
        if btn.cmd_id == 0 {
            tb_buttons.push(TBBUTTON {
                fsStyle: TBSTYLE_SEP as u8,
                ..TBBUTTON::default()
            });
        } else {
            let mut state: u8 = 0;
            if btn.enabled {
                state |= TBSTATE_ENABLED as u8;
            }
            let style: u8 = if btn.is_check {
                (TBSTYLE_BUTTON | TBSTYLE_CHECK) as u8
            } else {
                TBSTYLE_BUTTON as u8
            };
            tb_buttons.push(TBBUTTON {
                iBitmap: bitmap_idx,
                idCommand: i32::from(btn.cmd_id),
                fsState: state,
                fsStyle: style,
                // iString = 0 → no inline label; tooltip text is
                // supplied via `TBN_GETINFOTIPW`.
                ..TBBUTTON::default()
            });
            bitmap_idx += 1;
        }
    }

    // SAFETY: TB_ADDBUTTONS reads `tb_buttons.len()` TBBUTTON
    // entries from the supplied pointer. The Vec is alive through
    // the call; the toolbar copies its state internally.
    unsafe {
        SendMessageW(
            hwnd,
            TB_ADDBUTTONS,
            Some(WPARAM(tb_buttons.len())),
            Some(LPARAM(tb_buttons.as_ptr() as isize)),
        );
    }

    // Ask the toolbar to size itself based on its content — needed
    // even with CCS_NORESIZE so the *button cells* compute their
    // proper width; the parent still positions the window via
    // MoveWindow elsewhere.
    unsafe {
        SendMessageW(hwnd, TB_AUTOSIZE, None, None);
    }

    Ok(ToolbarHandles {
        hwnd,
        image_list,
        bitmap_px,
    })
}

// --- Tooltip handler ----------------------------------------------------------

/// Fill `pnmtbgit->pszText` with the tooltip for the hovered button.
/// Called from the parent's `WM_NOTIFY` arm when the toolbar fires
/// `TBN_GETINFOTIPW`. No-op if the cmd-id isn't one of ours.
///
/// # Safety
///
/// `pnmtbgit` must point at a valid `NMTBGETINFOTIPW`. The toolbar
/// supplies a buffer of `cchTextMax` UTF-16 units writable through
/// `pszText`; we write up to that bound and NUL-terminate.
pub unsafe fn fill_info_tip(pnmtbgit: *mut NMTBGETINFOTIPW) {
    if pnmtbgit.is_null() {
        return;
    }
    // SAFETY: the caller guarantees this points at a valid struct
    // for the duration of the call.
    let info = unsafe { &mut *pnmtbgit };
    let cmd_id = info.iItem as u16;
    let Some(btn) = BUTTONS.iter().find(|b| b.cmd_id != 0 && b.cmd_id == cmd_id) else {
        return;
    };
    if info.cchTextMax <= 0 || info.pszText.is_null() {
        return;
    }

    let cap = info.cchTextMax as usize;
    let mut wide: Vec<u16> = btn.tooltip.encode_utf16().collect();
    // Reserve one slot for NUL — encode at most `cap - 1` units, so
    // even an unusually long tooltip survives a copy with a
    // terminator inside the supplied buffer.
    if wide.len() > cap.saturating_sub(1) {
        wide.truncate(cap.saturating_sub(1));
    }
    // SAFETY: pszText points at `cchTextMax` writable u16s. We
    // write `wide.len() + 1` units (including the NUL), which fits
    // by construction.
    unsafe {
        core::ptr::copy_nonoverlapping(wide.as_ptr(), info.pszText.0, wide.len());
        *info.pszText.0.add(wide.len()) = 0;
    }
}

// --- State helpers ------------------------------------------------------------

/// Toggle a toolbar button's enabled-vs-greyed state. Used to reflect
/// Scintilla's `SCI_CANUNDO` / `SCI_CANREDO` queries on
/// `SCN_UPDATEUI`. The corresponding `is_check` style is preserved
/// — `TB_SETSTATE` rewrites the `TBSTATE_*` byte, not the `fsStyle`.
pub unsafe fn set_button_enabled(toolbar: HWND, cmd_id: u16, enabled: bool) {
    // SAFETY: `toolbar` is a real toolbar HWND owned by the caller
    // (the ui_win32 main window) and stays alive for the duration
    // of the SendMessageW dance inside the helper.
    unsafe { set_button_state_bit(toolbar, cmd_id, TBSTATE_ENABLED as u8, enabled) };
}

/// Toggle a toolbar button's checked-vs-normal indicator. Used to
/// reflect view-toggle state (Word Wrap, Show All Chars, Show Indent
/// Guide). Theme renders the checked state as a permanent pressed-
/// look highlight; that's the "active option" visual indicator from
/// the spec.
pub unsafe fn set_button_checked(toolbar: HWND, cmd_id: u16, checked: bool) {
    // SAFETY: same as `set_button_enabled`.
    unsafe { set_button_state_bit(toolbar, cmd_id, TBSTATE_CHECKED as u8, checked) };
}

/// Sync every dynamic-state toolbar button against the editor's
/// current Scintilla state. Cheap (a handful of direct-call queries
/// plus at most five `TB_SETSTATE` writes) and idempotent — safe to
/// run on every `SCN_UPDATEUI` and after every view-toggle handler.
///
/// Wired buttons:
///
/// - Undo / Redo: `TBSTATE_ENABLED` flipped from `SCI_CANUNDO` /
///   `SCI_CANREDO`. Per-buffer because Scintilla tracks an undo
///   stack per document and `SCI_SETDOCPOINTER` makes those queries
///   reflect the newly-bound buffer's state.
/// - Word Wrap: `TBSTATE_CHECKED` from `SCI_GETWRAPMODE`.
/// - Show All Chars: `TBSTATE_CHECKED` when *both* `SCI_GETVIEWWS`
///   and `SCI_GETVIEWEOL` are on. The sub-toggles in the View menu
///   can produce a "split" state where only one is set; the toolbar
///   button represents the "all chars are visible" mode
///   specifically and stays unchecked in the split case.
/// - Show Indent Guide: `TBSTATE_CHECKED` from
///   `SCI_GETINDENTATIONGUIDES != SC_IV_NONE`.
///
/// The three still-stub "panel visibility" toggles (Doc List,
/// Function List, Folder as Workspace) stay un-checked here —
/// their underlying features aren't fully wired to `refresh_state`
/// yet. Document Map has its own bit (`state.docmap_visible` in
/// `ui_win32`) that isn't a Scintilla-derived state, so its
/// button-check is pushed explicitly by `show_docmap_panel` /
/// `hide_docmap_panel` rather than being re-derived here on every
/// `SCN_UPDATEUI`.
///
/// # Safety
///
/// `toolbar` must be a live `ToolbarWindow32` HWND owned by the
/// caller (the `ui_win32` main window). `editor`'s direct-call
/// `(fn_ptr, instance_ptr)` pair must currently address the
/// document whose state the caller wants to read; with the multi-
/// tab `SCI_SETDOCPOINTER` model that's whichever doc is bound at
/// call time. Both calls happen on the UI thread — the toolbar
/// `SendMessageW` and Scintilla direct-call dispatch are both
/// synchronous and must not cross threads.
pub unsafe fn refresh_state(toolbar: HWND, editor: &codepp_editor::EditorHandle) {
    use codepp_scintilla_sys::{
        SCI_CANREDO, SCI_CANUNDO, SCI_GETINDENTATIONGUIDES, SCI_GETVIEWEOL, SCI_GETVIEWWS,
        SCI_GETWRAPMODE, SC_IV_NONE,
    };

    let can_undo = editor.send(SCI_CANUNDO, 0, 0) != 0;
    let can_redo = editor.send(SCI_CANREDO, 0, 0) != 0;
    let wrap_on = editor.send(SCI_GETWRAPMODE, 0, 0) != 0;
    let ws_on = editor.send(SCI_GETVIEWWS, 0, 0) != 0;
    let eol_on = editor.send(SCI_GETVIEWEOL, 0, 0) != 0;
    let indent_on = (editor.send(SCI_GETINDENTATIONGUIDES, 0, 0) as usize) != SC_IV_NONE;

    // SAFETY: `toolbar` is a real toolbar HWND from `WindowState`;
    // each `set_button_*` call uses TB_GETSTATE / TB_SETSTATE which
    // are synchronous direct sends.
    unsafe {
        set_button_enabled(toolbar, ID_EDIT_UNDO, can_undo);
        set_button_enabled(toolbar, ID_EDIT_REDO, can_redo);
        set_button_checked(toolbar, ID_VIEW_WORDWRAP, wrap_on);
        set_button_checked(toolbar, ID_VIEW_SHOW_ALL_CHARS, ws_on && eol_on);
        set_button_checked(toolbar, ID_VIEW_SHOW_INDENT_GUIDE, indent_on);
    }
}

/// Read-modify-write helper: get the current state byte, flip one
/// bit, write back. `TB_SETSTATE`'s lparam is a `MAKELONG(state, 0)`
/// where the low word carries the new `TBSTATE_*` byte.
unsafe fn set_button_state_bit(toolbar: HWND, cmd_id: u16, bit: u8, on: bool) {
    // SAFETY: TB_GETSTATE returns the low byte of a TBSTATE_* mask
    // through SendMessageW's return value (LRESULT cast); -1
    // indicates the cmd_id wasn't found.
    let cur = unsafe { SendMessageW(toolbar, TB_GETSTATE, Some(WPARAM(cmd_id as usize)), None) };
    if cur.0 == -1 {
        return;
    }
    let mut state = cur.0 as u8;
    if on {
        state |= bit;
    } else {
        state &= !bit;
    }
    let lparam = LPARAM((state as isize) & 0xFFFF);
    unsafe {
        SendMessageW(
            toolbar,
            TB_SETSTATE,
            Some(WPARAM(cmd_id as usize)),
            Some(lparam),
        );
    }
}
