//! File → Print — the Win32-native print pipeline.
//!
//! High-level flow, matching Notepad++:
//!
//! 1. Show the standard `PrintDlgW` (printer picker, copies, page range).
//! 2. Configure Scintilla's print-side settings on the active editor:
//!    colour-on-white paper, wrap-mode word, magnification zero.
//! 3. Measure — call [`SCI_FORMATRANGEFULL`] in wparam=0 mode once per
//!    page against the printer's HDC to build the page-break table.
//!    Two-pass rendering is what lets the header show a real "Page N of
//!    M" figure and lets the user's page-range choice select a subset
//!    without hunting through the whole document twice.
//! 4. Draw — iterate the requested page subset, call
//!    `StartDoc`/`StartPage`, paint the header on top of the page, then
//!    call [`SCI_FORMATRANGEFULL`] in wparam=1 mode so Scintilla renders
//!    that page's text into the printer HDC.
//! 5. Release the format cache via one final `SCI_FORMATRANGEFULL(0, NULL)`
//!    call; end the document; `DeleteDC`.
//!
//! Cross-platform note (DESIGN.md §7.4): this is Win32-only for now.
//! The GTK + Cocoa print backends land in Phase 5 alongside the rest of
//! the dialog-primitive abstraction; the two-pass measure/draw shape
//! and the `SCI_FORMATRANGEFULL` message are Scintilla-agnostic, so
//! most of the layout math ports directly.
//!
//! **Windows 11 "app doesn't support print preview" notice.** The
//! modern Win11 print dialog surfaces an integrated preview panel
//! only for apps that opt in via `PrintDlgExW` + `IPrintDialogCallback`
//! implementing the shell's preview-rendering callback protocol.
//! Code++ uses legacy `PrintDlgW`, which the Win11 dialog flags with
//! that notice. Migrating to `PrintDlgExW` + the callback is a Phase 5
//! polish item (a substantial addition — a new COM interface impl and
//! a second render path against the dialog's own DC). The standalone
//! `File → Print Preview` modal in `crate::print_preview` covers the
//! same UX in the meantime and works on every Windows version we
//! target, not just 11.

use core::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::time::SystemTime;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    Sci_CharacterRangeFull, Sci_RangeToFormatFull, Sci_Rectangle, SCI_FORMATRANGEFULL,
    SCI_GETLENGTH, SCI_SETPRINTCOLOURMODE, SCI_SETPRINTMAGNIFICATION, SCI_SETPRINTWRAPMODE,
    SC_PRINT_COLOURONWHITEDEFAULTBG,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteDC, DeleteObject, DrawTextW, GetDeviceCaps, LineTo, MoveToEx,
    RestoreDC, SaveDC, SelectObject, SetBkMode, SetTextAlign, SetTextColor, DEFAULT_CHARSET,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP, FW_NORMAL, HDC, HFONT, HGDIOBJ, HORZRES,
    LOGFONTW, LOGPIXELSX, LOGPIXELSY, TA_LEFT, TA_TOP, TEXT_ALIGN_OPTIONS, TRANSPARENT, VERTRES,
};
use windows::Win32::Storage::Xps::{EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::UI::Controls::Dialogs::{
    PrintDlgW, PD_ALLPAGES, PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PD_RETURNDEFAULT,
    PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW,
};

/// User-facing entry point wired to the `ID_FILE_PRINT` menu / toolbar /
/// Ctrl+P dispatch. Snapshots the necessary editor state, shows the OS
/// print dialog, then runs the render loop. Silent no-op if there's no
/// active document to print.
///
/// `owner` is the main window HWND so `PrintDlgW` opens modal-relative
/// to it. `doc_display_name` is what gets shown in the printer spooler
/// list and printed in the page header (basename for saved files,
/// `new N` for untitled). `editor` is a copy of the active
/// [`EditorHandle`] — its direct-call pointers are already captured
/// once per Scintilla control, so the caller (the `WM_COMMAND` arm) can
/// grab a copy under a brief `&mut WindowState` borrow and drop it
/// before the dialog spins its own message pump.
pub(crate) fn print_active_document(owner: HWND, doc_display_name: &str, editor: EditorHandle) {
    print_active_document_impl(owner, doc_display_name, editor, None);
}

/// Same as [`print_active_document`], but the caller already knows
/// the document's page count (Print Preview → Print handoff) so we
/// can pre-populate `PrintDlgW`'s `nMaxPage` / `nToPage` fields with
/// the real bounds. Without the hint the OS dialog shows a spinner
/// that goes up to 65535 and defaults `nToPage = 1`, which forces the
/// user to memorise page numbers from the preview before switching
/// to the dialog.
pub(crate) fn print_active_document_with_page_hint(
    owner: HWND,
    doc_display_name: &str,
    editor: EditorHandle,
    total_pages: usize,
) {
    print_active_document_impl(owner, doc_display_name, editor, Some(total_pages));
}

/// File → Print Now — pushes the document straight to the default
/// printer with no dialog, no page range selection, no
/// confirmation. All pages, single copy. If there's no default
/// printer, surface an error dialog and bail. Matches the "print
/// without any prompts, right now" contract users expect from a
/// menu item literally named "Print Now".
pub(crate) fn print_active_document_now(owner: HWND, doc_display_name: &str, editor: EditorHandle) {
    let text_length = editor.send(SCI_GETLENGTH, 0, 0);
    if text_length <= 0 {
        return;
    }
    let Some(job) = default_printer_job() else {
        crate::show_error_dialog(
            owner,
            "Print Now",
            "No printer is installed, or the default printer could not be opened.",
        );
        return;
    };
    execute_print_job(owner, doc_display_name, &editor, job);
}

fn print_active_document_impl(
    owner: HWND,
    doc_display_name: &str,
    editor: EditorHandle,
    known_total_pages: Option<usize>,
) {
    // Fast path: nothing to print if the document is empty. Skips
    // showing the dialog for what would obviously produce a blank
    // page; matches N++'s "print of an empty buffer does nothing"
    // behaviour.
    let text_length = editor.send(SCI_GETLENGTH, 0, 0);
    if text_length <= 0 {
        return;
    }

    // Show `PrintDlgW` — user picks printer + range + copies.
    let Some(job) = show_print_dialog(owner, known_total_pages) else {
        return;
    };
    execute_print_job(owner, doc_display_name, &editor, job);
}

/// Shared render pipeline for both the dialog-driven print
/// (`print_active_document`) and the no-dialog Print Now
/// (`print_active_document_now`). Given a fully-acquired `PrintJob`
/// (HDC + page range), runs the two-pass measure/draw against the
/// editor, handles the "Page X of M" header + truncation dialog,
/// and cleans up on every exit.
fn execute_print_job(owner: HWND, doc_display_name: &str, editor: &EditorHandle, job: PrintJob) {
    let text_length = editor.send(SCI_GETLENGTH, 0, 0);
    // The caller has already guarded against empty documents,
    // but re-check inside the shared helper so a future caller
    // that forgets doesn't silently print a blank page.
    if text_length <= 0 {
        return;
    }

    // --- 2. Configure Scintilla print-side settings. ---------------
    configure_scintilla_for_print(editor);

    // --- 3. Compute paper metrics. ----------------------------------
    let paper = PaperMetrics::from_hdc(job.hdc);

    // --- 4. Two-pass render. ----------------------------------------
    // Measure every page break in the document so the header can
    // display an accurate "Page X of M" without an extra pass per
    // page.
    let (page_breaks, truncation) = measure_page_breaks(editor, &paper, text_length, job.hdc);
    let total_pages = page_breaks.len();
    if total_pages == 0 {
        // Nothing to draw (extremely defensive — measure always
        // yields at least one break for a non-empty document).
        release_format_cache(editor);
        return;
    }
    // Surface truncation up front — a silent partial print would
    // look like the file itself was cut off, which is a worse UX
    // than an explicit "your document was too long / could not be
    // paginated" heads-up. The user still gets whatever pages we
    // *did* successfully measure, so this is a warning, not an
    // abort.
    if let Some(reason) = truncation {
        let body = match reason {
            MeasureTruncation::NoProgressAt(byte_offset) => format!(
                "Only {total_pages} page(s) could be laid out before the printer refused to \
                 advance around byte offset {byte_offset}. The rest of the document will not \
                 be printed. Try a smaller font size or a different page size."
            ),
            MeasureTruncation::HitPageCap { cap } => format!(
                "Only the first {cap} pages can be printed in a single job. \
                 The rest of the document will not be printed."
            ),
        };
        crate::show_error_dialog(owner, "Print — document truncated", &body);
    }

    // Clamp the user's page-range choice against reality. `PrintDlgW`
    // reports 1-based inclusive page numbers; the render loop works
    // in 0-based indices into `page_breaks`. If the user picked "All"
    // (`PageRange::All`), print every page; otherwise trim to the
    // legal window and skip if the range is empty after clamping.
    let (start_idx, end_idx_exclusive) = job.page_range.resolve(total_pages);
    if start_idx >= end_idx_exclusive {
        release_format_cache(editor);
        return;
    }

    // Start the print job with the OS spooler.
    let mut doc_info_wide: Vec<u16> = doc_display_name
        .encode_utf16()
        .chain(core::iter::once(0))
        .collect();
    let doc_info = DOCINFOW {
        cbSize: core::mem::size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(doc_info_wide.as_mut_ptr()),
        lpszOutput: PCWSTR::null(),
        lpszDatatype: PCWSTR::null(),
        fwType: 0,
    };
    // SAFETY: `job.hdc` is the printer DC returned by `PrintDlgW`
    // and still valid (we hold the only reference). `doc_info` is
    // fully initialised; the wide-string buffer outlives the call.
    let start_result = unsafe { StartDocW(job.hdc, &raw const doc_info) };
    if start_result <= 0 {
        tracing::warn!(
            code = start_result,
            "Print: StartDocW failed; job aborted before any page rendered"
        );
        release_format_cache(editor);
        return;
    }

    // Page loop. Each iteration renders exactly one page.
    let today = format_today();
    for (offset, &(cp_min, cp_max)) in page_breaks[start_idx..end_idx_exclusive].iter().enumerate()
    {
        let page_idx = start_idx + offset;
        // SAFETY: `job.hdc` still valid after StartDocW.
        let sp = unsafe { StartPage(job.hdc) };
        if sp <= 0 {
            tracing::warn!(
                code = sp,
                page = page_idx + 1,
                "Print: StartPage failed; aborting remaining pages"
            );
            break;
        }
        draw_page_header(
            job.hdc,
            &paper,
            doc_display_name,
            &today,
            page_idx + 1,
            total_pages,
        );
        // Real print: draw surface and metrics surface are the
        // same printer HDC. Preview swaps a screen mem DC in for
        // the draw side but keeps the printer HDC as the metric
        // target — see `render_one_page`'s doc.
        render_one_page(editor, job.hdc, job.hdc, &paper, cp_min, cp_max);
        // SAFETY: paired with the preceding StartPage.
        let ep = unsafe { EndPage(job.hdc) };
        if ep <= 0 {
            tracing::warn!(
                code = ep,
                page = page_idx + 1,
                "Print: EndPage failed; aborting remaining pages"
            );
            break;
        }
    }

    // SAFETY: paired with the earlier StartDocW.
    let end_result = unsafe { EndDoc(job.hdc) };
    if end_result <= 0 {
        // Nothing actionable — the spooler already accepted every
        // completed page — but log for diagnosability so a broken
        // driver / offline printer surfaces in the trace instead
        // of failing silently.
        tracing::warn!(
            code = end_result,
            "Print: EndDoc failed after all pages rendered"
        );
    }
    release_format_cache(editor);
    // `PrintJob::drop` will `DeleteDC(job.hdc)` next.
}

// -------------------------------------------------------------------
// Print dialog
// -------------------------------------------------------------------

/// Result of a successful `PrintDlgW` interaction.
///
/// Owns the printer HDC; drops call `DeleteDC` so a Cancel or an early
/// return from the render loop doesn't leak a printer context.
struct PrintJob {
    hdc: HDC,
    page_range: PageRange,
}

impl Drop for PrintJob {
    fn drop(&mut self) {
        if !self.hdc.is_invalid() {
            // SAFETY: `hdc` is the printer DC obtained from
            // `PrintDlgW` with `PD_RETURNDC` set — we own its
            // lifetime. `DeleteDC` on a valid DC returned by
            // PrintDlg is the standard release path.
            unsafe {
                let _ = DeleteDC(self.hdc);
            }
        }
    }
}

/// Which pages the user asked to print. Resolved against the total
/// page count at render time in [`Self::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageRange {
    All,
    /// 1-based inclusive `[start, end]` — the raw values `PrintDlgW`
    /// deposits into `PRINTDLGW.nFromPage` and `nToPage` when the
    /// user picks "Pages".
    Pages {
        start: u32,
        end: u32,
    },
}

impl PageRange {
    /// Convert to 0-based half-open `[start_idx, end_idx_exclusive)`
    /// against a page-break table of length `total`. Clamps
    /// out-of-range user input silently (the OS dialog already
    /// validates `nFromPage <= nToPage`, so this only catches the
    /// case where the user picked pages beyond the actual document
    /// length).
    fn resolve(self, total: usize) -> (usize, usize) {
        match self {
            PageRange::All => (0, total),
            PageRange::Pages { start, end } => {
                let s = (start as usize).saturating_sub(1).min(total);
                let e = (end as usize).min(total);
                if e < s {
                    (0, 0)
                } else {
                    (s, e)
                }
            }
        }
    }
}

/// Wrap `PrintDlgW`. Returns `None` on Cancel or dialog failure. Sets
/// `PD_RETURNDC` so the returned printer DC is ours to use (and
/// `Drop`-ed on the returned `PrintJob`), `PD_ALLPAGES` so the
/// default range selection is "All", `PD_NOSELECTION` so the
/// (unimplemented) selection radio stays greyed, and
/// `PD_USEDEVMODECOPIESANDCOLLATE` so the driver's copies/collate
/// controls take effect on the returned DC.
///
/// `known_total_pages` is a hint from a caller that already knows
/// how many pages the document would produce (i.e. Print Preview
/// chaining into Print — it did its own measure pass and can pass
/// the count so the dialog's `nMaxPage` spinner shows the real
/// upper bound). `None` on the initial print (we haven't measured
/// yet). `nToPage` defaults to the total so a user picking "Pages"
/// gets the full range pre-selected instead of just page 1.
fn show_print_dialog(owner: HWND, known_total_pages: Option<usize>) -> Option<PrintJob> {
    // Cap the OS spinner at a sensible upper bound. With a known
    // page count from the caller (Preview), use it. Without, fall
    // back to `u16::MAX` — the render loop clamps the returned
    // range against the real page count anyway
    // ([`PageRange::resolve`]), so this ceiling is a dialog-input
    // UX cap, not a security boundary.
    let n_max_page = known_total_pages
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(65535);
    let n_to_page = if known_total_pages.is_some() {
        n_max_page
    } else {
        1
    };
    // Only preload defaults; PrintDlgW overwrites `Flags`,
    // `hDevMode`, `hDevNames`, `hDC` on OK.
    let mut pd = PRINTDLGW {
        lStructSize: core::mem::size_of::<PRINTDLGW>() as u32,
        hwndOwner: owner,
        Flags: PD_RETURNDC | PD_ALLPAGES | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        nCopies: 1,
        nFromPage: 1,
        nToPage: n_to_page,
        nMinPage: 1,
        nMaxPage: n_max_page,
        ..Default::default()
    };
    // SAFETY: `&mut pd` points at a fully-initialised `PRINTDLGW`
    // struct. `PrintDlgW` spins its own modal message loop; no
    // `&mut WindowState` borrow is alive here (the caller drops it
    // before dispatching to this module). On OK, Windows fills
    // `pd.hDC` with a printer DC we own (`PD_RETURNDC`).
    let ok = unsafe { PrintDlgW(&raw mut pd).as_bool() };
    if !ok {
        return None;
    }
    if pd.hDC.is_invalid() {
        // `PD_RETURNDC` guarantees `hDC` is set on success; treat a
        // null here as a driver misbehaviour and skip printing.
        tracing::warn!("PrintDlgW returned success but hDC is null; aborting print");
        return None;
    }
    let page_range = if (pd.Flags & PD_PAGENUMS) == PD_PAGENUMS {
        PageRange::Pages {
            start: u32::from(pd.nFromPage),
            end: u32::from(pd.nToPage),
        }
    } else {
        PageRange::All
    };
    Some(PrintJob {
        hdc: pd.hDC,
        page_range,
    })
}

/// Obtain a printer HDC for the user's default printer without
/// showing any dialog UI. Uses `PrintDlgW(PD_RETURNDEFAULT |
/// PD_RETURNDC)` — same API `show_print_dialog` uses on user
/// interaction, minus the dialog display. Returns `None` when
/// there's no default printer configured, or the driver refused
/// to hand out a DC.
///
/// The caller owns the returned HDC and MUST `DeleteDC` it. When
/// wrapped in [`default_printer_job`] the `PrintJob`'s `Drop` handles
/// that; the standalone form is used by
/// [`crate::print_preview::show_print_preview`] where the HDC lives
/// on `PreviewState` and drops with it.
pub(crate) fn default_printer_dc() -> Option<HDC> {
    let mut pd = PRINTDLGW {
        lStructSize: core::mem::size_of::<PRINTDLGW>() as u32,
        Flags: PD_RETURNDEFAULT | PD_RETURNDC | PD_ALLPAGES | PD_NOSELECTION,
        nCopies: 1,
        nMinPage: 1,
        nMaxPage: 65535,
        ..Default::default()
    };
    // SAFETY: `&mut pd` is a fully-initialised `PRINTDLGW`.
    // `PD_RETURNDEFAULT` suppresses the dialog UI — no nested
    // message pump runs — so this call is a plain query.
    let ok = unsafe { PrintDlgW(&raw mut pd).as_bool() };
    if !ok || pd.hDC.is_invalid() {
        return None;
    }
    Some(pd.hDC)
}

/// [`default_printer_dc`] wrapped as a full `PrintJob` with
/// [`PageRange::All`]. Used by the "Print Now" command that
/// bypasses the OS `PrintDlgW` entirely — no printer picker, no
/// copies control, no range selection. All pages, single copy,
/// straight to the default printer.
fn default_printer_job() -> Option<PrintJob> {
    Some(PrintJob {
        hdc: default_printer_dc()?,
        page_range: PageRange::All,
    })
}

// -------------------------------------------------------------------
// Shared Scintilla print-mode setup
// -------------------------------------------------------------------

/// Configure the Scintilla view's print-side settings that both the
/// real print pipeline and the on-screen Print Preview need.
///
/// * `SC_PRINT_COLOURONWHITEDEFAULTBG` — lexer colours preserved,
///   but every "default background" style gets forced to white so
///   we don't burn the user's ink budget rendering a dark-theme
///   backdrop. Matches N++'s default.
/// * Magnification zero — use the same point-size the on-screen
///   editor uses. Scintilla scales font metrics to the target DC
///   via `hdc_target` automatically.
/// * Word wrap on — long lines fold at the right margin instead of
///   running off the paper (or off the preview page).
///
/// Idempotent. Safe to call once per print-or-preview invocation.
pub(crate) fn configure_scintilla_for_print(editor: &EditorHandle) {
    editor.send(
        SCI_SETPRINTCOLOURMODE,
        SC_PRINT_COLOURONWHITEDEFAULTBG as _,
        0,
    );
    editor.send(SCI_SETPRINTMAGNIFICATION, 0, 0);
    editor.send(SCI_SETPRINTWRAPMODE, 1 /* SC_WRAP_WORD */, 0);
}

// -------------------------------------------------------------------
// Paper metrics
// -------------------------------------------------------------------

/// Cached device-pixel measurements for a single print job.
///
/// **Coordinate system.** All rectangles are in printer-DC pixels
/// with the origin at the top-left of the **printable area**
/// (`HORZRES × VERTRES`), NOT the top-left of the physical page.
/// That's the invariant a Win32 printer HDC in the default
/// `MM_TEXT` mapping mode guarantees — the driver's DDI layer
/// enforces it, so it's consistent across vendors: `(0, 0)` on
/// the HDC lands at the mechanically-printable top-left, and
/// drawing above `y = 0` (or left of `x = 0`) is clipped by the
/// driver. `PHYSICALOFFSETX/Y` exist so an app can *locate* the
/// printable area on the physical sheet (for edge-to-edge / crop-
/// mark workflows), not to be added a second time on top of an
/// already-origin-adjusted HDC.
///
/// An earlier version of this code did exactly that — positioned
/// the header at `y = PHYSICALOFFSETY + margin_y`, effectively
/// double-counting the mechanical top margin. The header ended
/// up ~5mm (the typical `PHYSICALOFFSETY` value) farther from
/// the paper edge than the intended 15mm. That's the classic
/// GDI "double margin" bug. The user-reported "header cut in
/// half from top down" symptom after the first Print landing is
/// the visible artifact — dropping the redundant `PHYSICALOFFSET*`
/// addition is the correct fix regardless of whether the exact
/// mechanism matches the original diagnosis.
///
/// Notably, `Sci_RangeToFormatFull::rc_page` is **not** read by
/// Scintilla's `Editor::FormatRange` / `EditView::FormatRange`
/// implementation (`vendor/scintilla/src/Editor.cxx:1926-1934`
/// and `vendor/scintilla/src/EditView.cxx:2679-2863` — verified
/// during the fix review). We still populate `rc_page` with the
/// printable-area extents both for wire-shape consistency and
/// as a defensible value for any future consumer that starts
/// reading it.
pub(crate) struct PaperMetrics {
    /// Full printable page rectangle — `(0, 0, HORZRES, VERTRES)`.
    /// Populated for `Sci_RangeToFormatFull::rc_page`; not read
    /// by the current Scintilla `FormatRange` code path (see the
    /// `PaperMetrics` doc comment for the vendored-source
    /// citation).
    pub(crate) page_rect: Sci_Rectangle,
    /// The text-area rectangle — printable area minus header strip
    /// minus user-margin insets on all four sides. This is what
    /// Scintilla uses as `rc`.
    pub(crate) text_rect: Sci_Rectangle,
    /// Header strip rectangle — sits above `text_rect`, contains the
    /// filename / date / page-N-of-M line and the divider rule.
    pub(crate) header_rect: Sci_Rectangle,
    /// Vertical DPI — used to size the header font consistently
    /// across printers. Horizontal DPI is consumed at construction
    /// time to compute `text_rect`'s left/right margins and isn't
    /// stored separately (the ratio only matters for font metrics,
    /// which use the Y axis).
    pub(crate) dpi_y: i32,
}

impl PaperMetrics {
    pub(crate) fn from_hdc(hdc: HDC) -> Self {
        // SAFETY: `GetDeviceCaps` on a valid HDC is a pure query, no
        // side effects, no ownership issues. Every index below is a
        // documented value from `wingdi.h`. `HORZRES`/`VERTRES` are
        // the printable area dimensions; the HDC origin already
        // lands at the top-left of that area.
        let (horz_res, vert_res, dpi_x, dpi_y) = unsafe {
            (
                GetDeviceCaps(Some(hdc), HORZRES),
                GetDeviceCaps(Some(hdc), VERTRES),
                GetDeviceCaps(Some(hdc), LOGPIXELSX),
                GetDeviceCaps(Some(hdc), LOGPIXELSY),
            )
        };

        // Standard 15mm text-area margins on all four sides —
        // matches N++'s default and typical Word/browser output.
        // 15mm ≈ 0.591 inch; convert via `dpi_y * 15 / 254` (10x mm
        // ÷ inch-per-mm = 254/10, kept integer to dodge the float
        // conversion round-trip).
        //
        // Note: this margin is measured from the **printable area**
        // edge, not the physical paper edge. On a typical printer
        // with a ~5mm mechanical margin, the effective margin from
        // paper edge is ~20mm. Users don't perceive the difference
        // — what matters is that content is legibly inset — and
        // trying to account for `PHYSICALOFFSET*` would only
        // subtract from the drawable region.
        let margin_x = mm_to_device_pixels(15, dpi_x);
        let margin_y = mm_to_device_pixels(15, dpi_y);
        // Header strip below the top margin: two lines of body-
        // font-sized space. Body font hasn't been chosen at this
        // point (it's the editor's own font, applied later inside
        // `SCI_FORMATRANGEFULL`), so use a conservative 10pt:
        // `10/72 inch = 10 * dpi / 72`.
        let header_h = pt_to_device_pixels(10, dpi_y) * 2;

        Self {
            page_rect: Sci_Rectangle {
                left: 0,
                top: 0,
                right: horz_res,
                bottom: vert_res,
            },
            text_rect: Sci_Rectangle {
                left: margin_x,
                top: margin_y + header_h,
                right: horz_res - margin_x,
                bottom: vert_res - margin_y,
            },
            header_rect: Sci_Rectangle {
                left: margin_x,
                top: margin_y,
                right: horz_res - margin_x,
                bottom: margin_y + header_h,
            },
            dpi_y,
        }
    }
}

/// Convert millimetres to device pixels at `dpi` (pixels-per-inch).
///
/// One inch = 25.4 mm. We do the arithmetic in integers with a fixed
/// scale-by-10 factor to avoid a float round-trip: `mm * 10 * dpi / 254`
/// rounds to nearest-integer as the printer expects and is easy to
/// prove correct at a glance.
///
/// Saturates on overflow — a `GetDeviceCaps(LOGPIXELSY)` implausibly
/// large (a rogue / mis-programmed driver) would otherwise panic in
/// debug or wrap in release; saturation caps the margin at a fixed
/// bound so the print flow silently narrows the text area instead of
/// aborting.
fn mm_to_device_pixels(mm: i32, dpi: i32) -> i32 {
    mm.saturating_mul(dpi)
        .saturating_mul(10)
        .checked_div(254)
        .unwrap_or(0)
}

/// Convert points (1/72 inch) to device pixels at `dpi`. `1 pt = dpi /
/// 72` device pixels; integer division rounds down, which is fine at
/// print resolutions (600 dpi × 10 pt = 83 px, exact).
///
/// Same overflow-saturation discipline as [`mm_to_device_pixels`].
fn pt_to_device_pixels(pt: i32, dpi: i32) -> i32 {
    pt.saturating_mul(dpi).checked_div(72).unwrap_or(0)
}

// -------------------------------------------------------------------
// Two-pass render — measure then draw
// -------------------------------------------------------------------

/// Reason the measure pass stopped short of `text_length`. Non-`None`
/// values are surfaced to the user via [`show_error_dialog`] before
/// the (partial) render begins — a silent truncated printout would
/// be worse UX than an explicit "your document was too long to
/// paginate cleanly" heads-up.
#[derive(Clone, Copy, Debug)]
pub(crate) enum MeasureTruncation {
    /// Scintilla failed to advance `cp_min` on a page (`next <= cp_min`).
    /// Rare — implies a corrupt view state or a font/page-size
    /// combination Scintilla can't lay out. Byte offset where the
    /// stall happened is captured for the diagnostic.
    NoProgressAt(isize),
    /// The measure loop hit the [`MAX_PAGES`] safety cap. Only
    /// reachable with tens of thousands of pages, i.e. the user
    /// tried to print a document that dwarfs any realistic
    /// paginate-and-print workflow.
    HitPageCap { cap: usize },
}

/// Hard ceiling on the page count. A real document rarely exceeds a
/// few thousand pages; 100k is well beyond any plausible user input
/// but short-circuits an infinite loop / DoS-shape stall.
const MAX_PAGES: usize = 100_000;

/// First pass: walk `SCI_FORMATRANGEFULL` in wparam=0 (measure-only)
/// mode across the full document and record the `(cp_min, cp_max)` of
/// every page. Returns one tuple per page, in reading order. Uses the
/// printer's own HDC so the metrics match what the second-pass draw
/// will produce.
///
/// Also returns any truncation the loop observed so the caller can
/// tell the user their document was too long / could not be
/// paginated. Returning the reason (rather than surfacing it inline)
/// keeps this helper free of Win32-dialog dependencies, so it stays
/// unit-testable if a future pass extracts the measure logic behind
/// a pure trait.
///
/// Termination: the loop advances by whatever `SCI_FORMATRANGEFULL`
/// returns for each page. Scintilla guarantees strict monotonic
/// progress on a well-formed non-empty document (the returned position
/// is always `> cp_min` until the whole range is consumed). A
/// pathological return that fails to advance would loop forever, so
/// we cap iterations at [`MAX_PAGES`] as defence-in-depth against a
/// misconfigured / corrupted view.
pub(crate) fn measure_page_breaks(
    editor: &EditorHandle,
    paper: &PaperMetrics,
    text_length: isize,
    hdc: HDC,
) -> (Vec<(isize, isize)>, Option<MeasureTruncation>) {
    let mut breaks: Vec<(isize, isize)> = Vec::new();
    let mut cp_min = 0isize;
    let mut truncation: Option<MeasureTruncation> = None;
    while cp_min < text_length && breaks.len() < MAX_PAGES {
        let mut fr = Sci_RangeToFormatFull {
            hdc: hdc.0,
            hdc_target: hdc.0,
            rc: paper.text_rect,
            rc_page: paper.page_rect,
            chrg: Sci_CharacterRangeFull {
                cp_min,
                cp_max: text_length,
            },
        };
        // SAFETY: `SCI_FORMATRANGEFULL` reads `fr` by pointer and
        // does not retain it after return. `hdc` is a valid
        // printer DC (owned by our `PrintJob`), so the measure
        // pass touches only Scintilla's internal state — no
        // pixels flow to the printer for wparam=0.
        let next = editor.send(
            SCI_FORMATRANGEFULL,
            0, /* wparam=0 => measure only */
            &raw mut fr as isize,
        );
        // Defensive: if Scintilla fails to advance for any reason
        // (extreme font size vs page, corrupt view state), stop
        // rather than spin.
        if next <= cp_min {
            tracing::warn!(
                text_length,
                cp_min,
                returned = next,
                "Print measure pass: no progress; stopping loop"
            );
            truncation = Some(MeasureTruncation::NoProgressAt(cp_min));
            break;
        }
        breaks.push((cp_min, next));
        cp_min = next;
    }
    if breaks.len() >= MAX_PAGES {
        tracing::warn!(
            cap = MAX_PAGES,
            "Print measure pass hit MAX_PAGES cap; document truncated"
        );
        truncation = Some(MeasureTruncation::HitPageCap { cap: MAX_PAGES });
    }
    (breaks, truncation)
}

/// Second pass: render one page's `(cp_min, cp_max)` into the target
/// HDC. Same struct shape as `measure_page_breaks`, but wparam=1 so
/// Scintilla writes glyphs into `hdc`.
///
/// `hdc` is the surface glyphs land on. `hdc_target` is the surface
/// Scintilla measures font metrics against — Scintilla's
/// `SurfaceGDI::Init` reads `LOGPIXELSY` from it (via `AutoSurface
/// surfaceMeasure(pfr->hdcTarget, ...)` in `Editor::FormatRange`),
/// so line heights and glyph sizes come out at whatever resolution
/// `hdc_target`'s DPI reports. For a real print both are the printer
/// HDC. For the on-screen Print Preview, `hdc` is a screen-compatible
/// mem DC (so we can `BitBlt` it onto the preview window) while
/// `hdc_target` stays the printer HDC — that way Scintilla lays out
/// text at printer line-heights (typically ~100 device units per
/// line at 600 dpi) instead of screen line-heights (~15 at 96 dpi),
/// and the caller's anisotropic mapping on `hdc` scales the whole
/// page down for display. Without the separation, the preview
/// rendered text at ~15 units per line into a rectangle sized in
/// printer coordinates (~5700 units tall), so each page filled only
/// the top ~10% of the preview — the bug the "each page contains
/// only the editor's visible content" user report identified.
pub(crate) fn render_one_page(
    editor: &EditorHandle,
    hdc: HDC,
    hdc_target: HDC,
    paper: &PaperMetrics,
    cp_min: isize,
    cp_max: isize,
) {
    let mut fr = Sci_RangeToFormatFull {
        hdc: hdc.0,
        hdc_target: hdc_target.0,
        rc: paper.text_rect,
        rc_page: paper.page_rect,
        chrg: Sci_CharacterRangeFull { cp_min, cp_max },
    };
    // SAFETY: fr fully init'd; `SCI_FORMATRANGEFULL` doesn't retain
    // the pointer; `hdc` remains valid until the surrounding
    // `PrintJob` drops.
    let _ = editor.send(
        SCI_FORMATRANGEFULL,
        1, /* wparam=1 => draw */
        &raw mut fr as isize,
    );
}

/// Free Scintilla's per-print format cache. Called once at the end of
/// a print job — matches the "final `SCI_FORMATRANGE(0, 0)` call"
/// contract in the Scintilla docs so cached surfaces don't leak for
/// the remaining Scintilla-instance lifetime.
pub(crate) fn release_format_cache(editor: &EditorHandle) {
    editor.send(SCI_FORMATRANGEFULL, 0, 0);
}

// -------------------------------------------------------------------
// Header rendering
// -------------------------------------------------------------------

/// Paint one page's header strip: filename left, date centre, "Page N
/// of M" right, plus a hairline divider along the header's bottom
/// edge. Font is 10pt Arial, drawn with `DrawTextW` on the printer
/// HDC directly (Scintilla's format-range doesn't own the header
/// strip — we're outside its `rc`).
pub(crate) fn draw_page_header(
    hdc: HDC,
    paper: &PaperMetrics,
    doc_name: &str,
    date_text: &str,
    page: usize,
    total: usize,
) {
    // Snapshot the DC's full state before touching anything.
    //
    // Scintilla's `SurfaceGDI::Init` calls
    // `SetTextAlign(hdc, TA_BASELINE)` — inherited from the
    // per-page render pass AND from the measure pass that runs
    // *before* the first `StartPage`. `SurfaceGDI`'s destructor
    // restores pen/brush/font/bitmap but does NOT restore text
    // alignment or (on some drivers) other minor DC attributes.
    // `DrawTextW` internally dispatches through `ExtTextOut`,
    // which honours `SetTextAlign`; with `TA_BASELINE` the text
    // baseline gets pinned to the rect's top edge instead of the
    // top of the glyphs, so header text ends up drawn *above* my
    // header rectangle and gets clipped by the page margin. The
    // observed symptom of the second Print-fix attempt (`48ad2f4`,
    // "only a few pixels of the bottom are printed") matches
    // exactly.
    //
    // `SaveDC` + `RestoreDC(-1)` is the belt-and-braces fix:
    // snapshots every DC attribute Scintilla may have touched
    // (text align, colours, background mode, pen, font, clip,
    // transform, mapping mode, …) and restores them at the end
    // regardless of which specific attribute is causing trouble
    // on a given driver. The explicit `SetTextAlign(TA_TOP |
    // TA_LEFT)` below is also-defensive — pinning the alignment
    // we actually need so a future refactor that drops the
    // SaveDC discipline doesn't silently regress the symptom.
    //
    // SAFETY: `hdc` is the caller-owned printer DC (still valid
    // for the duration of this function). `SaveDC` returns 0 on
    // failure — we tolerate that by drawing without a rollback,
    // matching the "rare failure, degrade gracefully" pattern
    // used for `CreateFontIndirectW` below.
    let saved_dc_state = unsafe { SaveDC(hdc) };
    // SAFETY: pure attribute writes on the valid HDC.
    unsafe {
        let _ = SetTextAlign(hdc, TEXT_ALIGN_OPTIONS(TA_TOP.0 | TA_LEFT.0));
    }

    // Font: 10pt Arial, weight normal. Height in device pixels is
    // negative to select the em-height convention `DrawTextW` uses.
    let mut lf = LOGFONTW {
        lfHeight: -pt_to_device_pixels(10, paper.dpi_y),
        lfWidth: 0,
        lfWeight: FW_NORMAL.0 as _,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let face = "Arial\0";
    for (i, c) in face.encode_utf16().enumerate() {
        if i >= lf.lfFaceName.len() {
            break;
        }
        lf.lfFaceName[i] = c;
    }

    // SAFETY: `lf` fully init'd. `CreateFontIndirectW` returns a
    // font handle we own; we `DeleteObject` it before return so
    // the printer DC doesn't hold a reference beyond this scope.
    let hfont: HFONT = unsafe { CreateFontIndirectW(&raw const lf) };
    if hfont.is_invalid() {
        // Rare failure — fall back to rendering with the DC's
        // current font. Header will still appear, just in the
        // driver default.
        tracing::warn!("Print header: CreateFontIndirectW failed; using default font");
    }
    // SAFETY: swap the font in; restore before we delete the
    // custom one so the DC's font-tracking stays balanced.
    let old_font: HGDIOBJ = unsafe {
        if hfont.is_invalid() {
            HGDIOBJ::default()
        } else {
            SelectObject(hdc, HGDIOBJ(hfont.0))
        }
    };

    // Text: black, transparent background so we don't blot the
    // divider line under the text.
    // SAFETY: colour + mode setters on a valid HDC are pure.
    unsafe {
        let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0));
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    let header = paper.header_rect;
    let mut rc_left = RECT {
        left: header.left,
        top: header.top,
        right: header.right,
        bottom: header.bottom,
    };
    let mut rc_right = rc_left;
    let mut rc_centre = rc_left;

    // Filename (left-aligned).
    let mut name_w: Vec<u16> = doc_name.encode_utf16().collect();
    // SAFETY: `DrawTextW` reads (name_w.as_ptr(), name_w.len()) and
    // treats the rect as in/out — we own both.
    unsafe {
        DrawTextW(
            hdc,
            &mut name_w,
            &raw mut rc_left,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    // Date (horizontally centre-aligned, top-anchored to match
    // the filename and page-of-M items so all three text baselines
    // line up. An earlier version used `DT_VCENTER` here, which
    // centred the date halfway down the header strip while the
    // other two hugged the top — visually the date read as a
    // separate lower line, contributing to the "header looks split
    // in half" report.).
    let mut date_w: Vec<u16> = date_text.encode_utf16().collect();
    unsafe {
        DrawTextW(
            hdc,
            &mut date_w,
            &raw mut rc_centre,
            DT_TOP | DT_SINGLELINE | DT_NOPREFIX | windows::Win32::Graphics::Gdi::DT_CENTER,
        );
    }
    // "Page N of M" (right-aligned).
    let page_text = format!("Page {page} of {total}");
    let mut page_w: Vec<u16> = page_text.encode_utf16().collect();
    unsafe {
        DrawTextW(
            hdc,
            &mut page_w,
            &raw mut rc_right,
            DT_RIGHT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    // Hairline divider between the header and the text area — one
    // device pixel down from the header's bottom edge so the text
    // area's first line isn't clipped.
    // SAFETY: MoveToEx + LineTo on a valid HDC.
    unsafe {
        let _ = MoveToEx(hdc, header.left, header.bottom - 1, None);
        let _ = LineTo(hdc, header.right, header.bottom - 1);
    }

    // Restore the previous font selection and free the header
    // font. The `RestoreDC` below would also restore the font
    // selection (that's what `SaveDC` snapshots), but it does NOT
    // free the `hfont` we `CreateFontIndirectW`-created — GDI
    // objects have their own owner-tracked lifetime independent
    // of DC state. So we still `DeleteObject(hfont)` explicitly.
    // The `SelectObject(hdc, old_font)` restore is technically
    // redundant with the pending `RestoreDC`, but we keep it so
    // the ownership handoff is legible independent of the DC-
    // level rollback below.
    if !hfont.is_invalid() {
        // SAFETY: paired with the SelectObject / CreateFontIndirectW
        // pair above; both handles were created inside this
        // function so no other code holds them.
        unsafe {
            if !old_font.is_invalid() {
                let _ = SelectObject(hdc, old_font);
            }
            let _ = DeleteObject(HGDIOBJ(hfont.0));
        }
    }

    // Roll back every attribute the SaveDC snapshotted at the top.
    // Only if the snapshot succeeded (`saved_dc_state != 0`) —
    // passing 0 to `RestoreDC` is a no-op per Win32 docs, but
    // guarding it makes the intent explicit.
    if saved_dc_state != 0 {
        // SAFETY: `saved_dc_state` came from the matching `SaveDC`
        // on the same HDC; no other code has pushed / popped DC
        // state in between (the header render is straight-line).
        unsafe {
            let _ = RestoreDC(hdc, saved_dc_state);
        }
    }
}

// -------------------------------------------------------------------
// Header date formatting
// -------------------------------------------------------------------

/// Today's date rendered as `YYYY-MM-DD`. Uses UTC via the standard
/// `SystemTime::now()` primitive — matches the file-header widths
/// most printers expect and stays deterministic across locales.
///
/// Kept as a small dedicated helper because it's the only piece of
/// non-Win32 logic in this module and factors cleanly for testing.
pub(crate) fn format_today() -> String {
    let epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_ymd_from_epoch_seconds(epoch)
}

/// Pure `epoch_seconds → "YYYY-MM-DD"` conversion. Broken out so the
/// date formatting is unit-testable without touching the system
/// clock. Handles UTC only; the header doesn't try to localise.
fn format_ymd_from_epoch_seconds(epoch: u64) -> String {
    // Standard Julian-day-shifted calendar arithmetic. Reproduces
    // the "civil_from_days" algorithm published by Howard Hinnant
    // (public-domain / MIT-alike, but reimplemented from the
    // description so no source is copied). Valid for any date from
    // year 1 through year 9999.
    let days = (epoch / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}")
}

/// Display name for a document — basename for saved files, `new N` /
/// custom name for untitled. Broken out so the `WM_COMMAND` arm can
/// derive it cheaply from a `codepp_shell::Tab` snapshot.
pub(crate) fn display_name_for(
    path: Option<&Path>,
    untitled_seq: Option<u32>,
    custom: Option<&str>,
) -> String {
    if let Some(name) = custom {
        return name.to_string();
    }
    if let Some(p) = path {
        return p.file_name().map_or_else(
            || p.to_string_lossy().into_owned(),
            |s| s.to_string_lossy().into_owned(),
        );
    }
    if let Some(n) = untitled_seq {
        return format!("new {n}");
    }
    "untitled".to_string()
}

// Silence "unused" while wparam/lparam names are aligned with the
// Scintilla docs — `_lparam` is intentionally the field being read via
// pointer arithmetic in the FFI struct.
#[allow(dead_code)]
const _: fn() = || {
    let _ = ptr::null::<c_void>();
};

// -------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_conversion_at_600_dpi() {
        // 15 mm at 600 dpi = 15 * 600 / 25.4 ≈ 354.33 device pixels
        // (our integer formula rounds to 354).
        assert_eq!(mm_to_device_pixels(15, 600), 354);
        assert_eq!(mm_to_device_pixels(25, 600), 590);
        assert_eq!(mm_to_device_pixels(0, 600), 0);
    }

    #[test]
    fn mm_conversion_at_96_dpi() {
        // 15 mm at 96 dpi = 15 * 96 / 25.4 ≈ 56.69, integer-floor 56.
        assert_eq!(mm_to_device_pixels(15, 96), 56);
    }

    #[test]
    fn pt_conversion_at_600_dpi() {
        // 10 pt at 600 dpi = 10 * 600 / 72 ≈ 83.33 → 83.
        assert_eq!(pt_to_device_pixels(10, 600), 83);
        assert_eq!(pt_to_device_pixels(12, 96), 16);
    }

    /// A rogue printer driver reporting an implausible DPI must not
    /// overflow the mm-conversion — saturating multiplication caps
    /// the result at `i32::MAX` instead of panicking (debug) /
    /// wrapping (release). Pin the behaviour so a well-meaning
    /// future refactor back to plain `*` gets caught.
    #[test]
    fn mm_conversion_saturates_on_absurd_dpi() {
        let result = mm_to_device_pixels(15, i32::MAX);
        // saturating_mul(15, MAX) → MAX; MAX * 10 → MAX;
        // MAX / 254 → about 8.45e6. Just check we didn't panic
        // and got something finite.
        assert!(result > 0 && result < i32::MAX);
        // Negative-DPI defence: `saturating_mul` handles the sign
        // correctly, so a garbage negative DPI produces a small
        // non-positive integer (integer division of a small
        // negative product by 254 truncates to zero) rather than
        // an aborted process. Concretely: 15 * -1 * 10 = -150,
        // then / 254 = 0.
        assert_eq!(mm_to_device_pixels(15, -1), 0);
        assert_eq!(mm_to_device_pixels(15, i32::MIN), i32::MIN / 254);
    }

    #[test]
    fn pt_conversion_saturates_on_absurd_dpi() {
        let result = pt_to_device_pixels(10, i32::MAX);
        assert!(result > 0 && result < i32::MAX);
    }

    #[test]
    fn page_range_all_resolves_to_full_span() {
        assert_eq!(PageRange::All.resolve(10), (0, 10));
        assert_eq!(PageRange::All.resolve(0), (0, 0));
    }

    #[test]
    fn page_range_pages_clamps_to_document_bounds() {
        // User picks pages 3–5 out of 10 → 0-based [2, 5).
        assert_eq!(PageRange::Pages { start: 3, end: 5 }.resolve(10), (2, 5));
        // Pages beyond total truncate.
        assert_eq!(PageRange::Pages { start: 8, end: 12 }.resolve(10), (7, 10));
        // Single page.
        assert_eq!(PageRange::Pages { start: 4, end: 4 }.resolve(10), (3, 4));
    }

    #[test]
    fn page_range_pages_below_one_saturates() {
        // `start=0` would underflow `start - 1` on a naive
        // implementation — the saturating_sub keeps it at 0.
        assert_eq!(PageRange::Pages { start: 0, end: 3 }.resolve(10), (0, 3));
    }

    #[test]
    fn page_range_pages_inverted_yields_empty_span() {
        // Shouldn't happen (PrintDlg validates), but be robust.
        assert_eq!(PageRange::Pages { start: 7, end: 2 }.resolve(10), (0, 0));
    }

    #[test]
    fn format_ymd_epoch_zero_is_1970() {
        assert_eq!(format_ymd_from_epoch_seconds(0), "1970-01-01");
    }

    #[test]
    fn format_ymd_known_dates() {
        // 2024-01-01 UTC = epoch 1704067200.
        assert_eq!(format_ymd_from_epoch_seconds(1_704_067_200), "2024-01-01");
        // 2000-02-29 UTC = epoch 951782400 (leap year).
        assert_eq!(format_ymd_from_epoch_seconds(951_782_400), "2000-02-29");
        // 2100-03-01 UTC = epoch 4107542400 (2100 NOT a leap year:
        // divisible by 100 but not by 400). The test pins that we
        // correctly skip the Feb-29 that a naive `year % 4 == 0`
        // check would have inserted.
        assert_eq!(format_ymd_from_epoch_seconds(4_107_542_400), "2100-03-01");
    }

    #[test]
    fn display_name_prefers_custom_then_path_then_untitled() {
        assert_eq!(
            display_name_for(None, Some(3), Some("release notes")),
            "release notes"
        );
        assert_eq!(
            display_name_for(Some(Path::new(r"C:\src\foo.rs")), None, None),
            "foo.rs"
        );
        assert_eq!(display_name_for(None, Some(2), None), "new 2");
        assert_eq!(display_name_for(None, None, None), "untitled");
    }
}
