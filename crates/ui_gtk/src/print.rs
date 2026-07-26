//! Printing the active buffer via `GtkPrintOperation` + Scintilla's
//! `SCI_FORMATRANGEFULL`.
//!
//! This is the GTK counterpart of `ui_win32`'s `print.rs`, but far smaller:
//! `GtkPrintOperation` owns the printer picker, the page-range / copies
//! selection, and the per-page loop natively, so this module only has to
//! feed Scintilla the right cairo surface for each page and paint a header.
//!
//! # How Scintilla renders onto a printer
//!
//! `SCI_FORMATRANGEFULL(draw, &Sci_RangeToFormatFull)` renders a character
//! range into a rectangle on a *surface* and returns the position of the
//! next character that did not fit — i.e. the start of the next page. On GTK
//! 3.x that surface is a `cairo_t *` (the `hdc`/`hdc_target` fields hold the
//! cairo pointer, not a Win32 `HDC`), obtained from the print context with
//! [`gtk::PrintContext::cairo_context`]. See
//! `vendor/scintilla/doc/ScintillaDoc.html` §Printing.
//!
//! Two passes, both against the print context's own cairo context (which is
//! valid in `begin-print` as well as `draw-page` — verified empirically):
//!
//! 1. **`begin-print`** — paginate: loop `draw = 0` from the document start,
//!    recording each page's first character position, until the whole
//!    document is consumed. `set_n_pages` with the count.
//! 2. **`draw-page`** — paint the header, then `draw = 1` for the recorded
//!    range of the requested page. GTK only asks for the pages the user
//!    selected and handles copies itself.
//! 3. **`end-print`** — `SCI_FORMATRANGEFULL(0, NULL)` releases Scintilla's
//!    format cache built up across the passes.
//!
//! Print settings mirror `ui_win32`: colours preserved but default-coloured
//! backgrounds forced to white (`SC_PRINT_COLOURONWHITEDEFAULTBG`), no
//! magnification, word-wrap on so long lines are not clipped at the margin.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    sptr_t, Sci_CharacterRangeFull, Sci_RangeToFormatFull, Sci_Rectangle, SCI_FORMATRANGEFULL,
    SCI_GETLENGTH, SCI_SETPRINTCOLOURMODE, SCI_SETPRINTMAGNIFICATION, SCI_SETPRINTWRAPMODE,
    SC_PRINT_COLOURONWHITEDEFAULTBG, SC_WRAP_WORD,
};
use gtk::prelude::*;
use gtk::PrintOperationAction;

use crate::state::with_state;
use crate::DrainFreeze;

/// Page margin, in cairo points at 72 dpi (~15 mm — matching `ui_win32`'s
/// deliberate inset). `GtkPrintContext` already excludes the non-printable
/// area, but printing text flush to that edge reads badly, so the text
/// column is inset by this on all four sides. The header sits in the top
/// margin band, above the text.
const PAGE_MARGIN_PT: f64 = 43.0;
/// Baseline of the header text, measured from the physical page top (inside
/// the top margin band, above the text column).
const HEADER_BASELINE_PT: f64 = 10.0;
/// Y of the thin rule under the header text.
const HEADER_RULE_Y_PT: f64 = 16.0;
/// Hard cap on the page count so a pathological document (huge file, tiny
/// effective page capacity) can never spin `begin-print` unboundedly on the
/// single UI thread. Matches `ui_win32`'s `MAX_PAGES`.
const MAX_PAGES: usize = 100_000;

/// Print the active buffer: show the native print dialog and, on accept,
/// render every requested page. The shared entry point behind File → Print,
/// its Ctrl+P accelerator, and the toolbar Print button.
pub(crate) fn show() {
    run_operation(PrintOperationAction::PrintDialog, "Print failed", false);
}

/// File → Print Now — push the active document straight to the default
/// printer with no dialog, no page-range selection, no copies control, no
/// confirmation. All pages, single copy. Mirrors `ui_win32`'s
/// `print_active_document_now`.
///
/// `PrintOperationAction::Print` runs the same render pipeline as [`show`]
/// but skips the dialog, using the operation's print settings; with none set,
/// GTK sends the job to the system default printer. An empty buffer is a
/// silent no-op (matching Win32 — nothing to print, so no blank page), and a
/// failure to reach a printer surfaces the same error dialog as [`show`].
pub(crate) fn print_now() {
    run_operation(PrintOperationAction::Print, "Print Now failed", true);
}

/// Shared driver behind [`show`] and [`print_now`]: snapshot the active
/// editor + display name, build the operation, and `run` it under a
/// [`DrainFreeze`] with the requested action.
///
/// `no_dialog` is set only by [`print_now`] (the `Print` action). It changes
/// two things relative to the dialog path:
/// 1. An empty buffer is a silent no-op — there is no dialog to cancel out of,
///    so bail before spooling a blank page (matching Win32).
/// 2. A `Cancel` *result* (not an `Err`) is surfaced as an error. With the
///    dialog suppressed, `GtkPrintOperation` returns `Cancel` when it cannot
///    resolve a printer — i.e. no system default printer is configured — with
///    no `GError`. There is no user cancellation to respect in that case, so
///    it maps to Win32's "No printer is installed…" dialog rather than a
///    silent no-op. On the dialog path a `Cancel` is the user dismissing the
///    dialog and stays silent.
fn run_operation(action: PrintOperationAction, error_title: &str, no_dialog: bool) {
    // Capture everything the operation needs up front, under one short
    // borrow that is dropped before `run` spins its nested main loop.
    // `EditorHandle` is `Copy` and outlives the process (the single view is
    // never destroyed), so it can move into the `'static` signal closures
    // without a `with_state` hop on the hot path — which also means the
    // closures never re-enter `with_state` and cannot be declined.
    let Some((editor, doc_name, window)) = with_state(|st| {
        let name = st
            .shell
            .active()
            .map_or_else(|| "Untitled".to_owned(), codepp_shell::tab_display_name);
        (st.editor, name, st.window.clone())
    }) else {
        return;
    };

    // Print Now on an empty buffer does nothing — no dialog to cancel out of,
    // so bail before building the operation rather than spooling a blank page.
    if no_dialog && editor.send(SCI_GETLENGTH, 0, 0) <= 0 {
        return;
    }

    let op = build_print_operation(editor, &doc_name);

    // Freeze background drains for the span of `run`: it spins a nested main
    // loop (dialog + the print callbacks), during which a worker wake could
    // otherwise `drain_shell` and rebind the single view to a different
    // document mid-print — sliding the wrong buffer under `SCI_FORMATRANGEFULL`
    // between pages. The same guard the close-confirm modal uses; deferred
    // wakes flush once the freeze lifts. (A *synchronous* GTK signal reaching
    // a handler during the nested loop is a separate, accepted-scope concern
    // — see DESIGN.md §7.4's close-modal note; the print dialog is modal for
    // the main window, so that path should not fire here.)
    let result = {
        let _freeze = DrainFreeze::new();
        op.run(action, Some(&window))
    };
    match result {
        Err(err) => {
            crate::message_dialog(
                gtk::MessageType::Error,
                gtk::ButtonsType::Ok,
                error_title,
                &codepp_shell::sanitize_str_for_display(&err.to_string()),
            );
        }
        // No-dialog path: `Cancel` means no printer could be resolved (no
        // default configured), not a user cancellation — surface it. See the
        // `no_dialog` contract on this fn. The message is a static literal, so
        // no sanitization is needed. `PrintOperationResult` is non-exhaustive,
        // hence the catch-all.
        Ok(gtk::PrintOperationResult::Cancel) if no_dialog => {
            crate::message_dialog(
                gtk::MessageType::Error,
                gtk::ButtonsType::Ok,
                error_title,
                "No printer is installed, or the default printer could not be opened.",
            );
        }
        Ok(_) => {}
    }
}

/// Build and fully wire a `GtkPrintOperation` for `editor`'s current
/// document, ready to `run`. Extracted from [`show`] so the render path can
/// be exercised by an EXPORT run in tests, without the interactive dialog.
///
/// Sets the printer render modes (matching `ui_win32`; they affect only the
/// `SCI_FORMATRANGEFULL` output, never the on-screen view), snapshots the
/// document length, and installs the three handlers: `begin-print`
/// paginates, `draw-page` paints the header + one page, `end-print` frees
/// Scintilla's format cache.
fn build_print_operation(editor: EditorHandle, doc_name: &str) -> gtk::PrintOperation {
    let doc_len = editor.send(SCI_GETLENGTH, 0, 0);
    editor.send(
        SCI_SETPRINTCOLOURMODE,
        SC_PRINT_COLOURONWHITEDEFAULTBG as usize,
        0,
    );
    editor.send(SCI_SETPRINTMAGNIFICATION, 0, 0);
    editor.send(SCI_SETPRINTWRAPMODE, SC_WRAP_WORD, 0);

    // Page-start positions, filled by `begin-print` and read by `draw-page`.
    let page_starts: Rc<RefCell<Vec<sptr_t>>> = Rc::new(RefCell::new(Vec::new()));

    let op = gtk::PrintOperation::new();
    op.set_job_name(doc_name);

    // --- begin-print: paginate ---------------------------------------
    let starts_for_begin = Rc::clone(&page_starts);
    op.connect_begin_print(move |op, ctx| {
        let Some(cr) = ctx.cairo_context() else {
            // No surface to measure against; fall back to a single page so
            // the operation still completes rather than dividing by nothing.
            op.set_n_pages(1);
            *starts_for_begin.borrow_mut() = vec![0];
            return;
        };
        let (text_rc, page_rc) = page_rects(ctx);
        let hdc = cr.to_raw_none().cast::<c_void>();
        let starts = paginate(doc_len, |cp| {
            format_range(editor, hdc, false, text_rc, page_rc, cp, doc_len)
        });
        op.set_n_pages(i32::try_from(starts.len()).unwrap_or(i32::MAX));
        *starts_for_begin.borrow_mut() = starts;
    });

    // --- draw-page: header + one page of text ------------------------
    let starts_for_draw = Rc::clone(&page_starts);
    let name_for_draw = doc_name.to_owned();
    op.connect_draw_page(move |_op, ctx, page_nr| {
        let Some(cr) = ctx.cairo_context() else {
            return;
        };
        let (text_rc, page_rc) = page_rects(ctx);
        let starts = starts_for_draw.borrow();
        let total = starts.len();
        let idx = usize::try_from(page_nr).unwrap_or(0);
        let cp_min = starts.get(idx).copied().unwrap_or_else(|| {
            // Unreachable under normal `GtkPrintOperation` semantics (it only
            // asks for pages `begin-print` declared); log rather than silently
            // mis-render if that ever changes.
            tracing::warn!(
                page = idx,
                total,
                "print: draw-page out of range; starting at 0"
            );
            0
        });

        draw_header(&cr, ctx, &name_for_draw, idx + 1, total);

        let hdc = cr.to_raw_none().cast::<c_void>();
        format_range(editor, hdc, true, text_rc, page_rc, cp_min, doc_len);
    });

    // --- end-print: release Scintilla's format cache -----------------
    op.connect_end_print(move |_op, _ctx| {
        // wparam = 0, lparam = NULL: free the pagination/format cache.
        editor.send(SCI_FORMATRANGEFULL, 0, 0);
    });

    op
}

/// The whole-page and text-area rectangles for the current print context,
/// in cairo points. `page_rc` is the full printable area (`GtkPrintContext`
/// already excludes the non-printable area); `text_rc` insets it by
/// [`PAGE_MARGIN_PT`] on all four sides, leaving the top margin band free for
/// the header.
fn page_rects(ctx: &gtk::PrintContext) -> (Sci_Rectangle, Sci_Rectangle) {
    let w = ctx.width();
    let h = ctx.height();
    let m = PAGE_MARGIN_PT;
    let page = Sci_Rectangle {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    };
    let text = Sci_Rectangle {
        left: m as i32,
        top: m as i32,
        right: (w - m) as i32,
        bottom: (h - m) as i32,
    };
    (text, page)
}

/// Walk the document into page-start character positions. `measure(cp)`
/// returns the first character that did *not* fit when rendering from `cp`
/// — Scintilla's `SCI_FORMATRANGEFULL(draw = 0)` contract. Always returns at
/// least one page (an empty document still prints a header-only page), and is
/// bounded on both axes: a non-advancing measurer breaks the loop, and
/// [`MAX_PAGES`] caps the count so an extreme document cannot spin the UI
/// thread unboundedly.
///
/// Pure and measurer-agnostic so the pagination edge cases are unit-testable
/// without a display or a real Scintilla surface.
fn paginate(doc_len: sptr_t, mut measure: impl FnMut(sptr_t) -> sptr_t) -> Vec<sptr_t> {
    let mut starts = Vec::new();
    let mut cp: sptr_t = 0;
    while cp < doc_len && starts.len() < MAX_PAGES {
        starts.push(cp);
        let next = measure(cp);
        // Non-advancing measure (e.g. a degenerate zero-height text rect):
        // stop rather than loop forever.
        if next <= cp {
            break;
        }
        cp = next;
    }
    if cp < doc_len && starts.len() >= MAX_PAGES {
        tracing::warn!(cap = MAX_PAGES, "print: page cap reached; tail not printed");
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}

/// Issue one `SCI_FORMATRANGEFULL`. `draw` false paginates (measures only),
/// true renders. Returns the position of the next character after the range
/// that fit — the start of the next page.
///
/// `hdc` is the cairo pointer for both the draw and the measurement surface;
/// on GTK the two are always the same context. The `Sci_RangeToFormatFull`
/// is a stack local live across the synchronous `send`, and Scintilla does
/// not retain the pointer past the call.
fn format_range(
    editor: EditorHandle,
    hdc: *mut c_void,
    draw: bool,
    text_rc: Sci_Rectangle,
    page_rc: Sci_Rectangle,
    cp_min: sptr_t,
    cp_max: sptr_t,
) -> sptr_t {
    let mut fr = Sci_RangeToFormatFull {
        hdc,
        hdc_target: hdc,
        rc: text_rc,
        rc_page: page_rc,
        chrg: Sci_CharacterRangeFull { cp_min, cp_max },
    };
    editor.send(
        SCI_FORMATRANGEFULL,
        usize::from(draw),
        &raw mut fr as sptr_t,
    )
}

/// Paint the page header: filename (left), date (centre), "Page N of M"
/// (right), and a thin rule beneath. Cairo's toy text API is enough for a
/// single line and avoids a pango dependency. A cairo error only aborts the
/// header — the document body still prints — so it is logged, not fatal.
fn draw_header(
    cr: &gtk::cairo::Context,
    ctx: &gtk::PrintContext,
    name: &str,
    page: usize,
    total: usize,
) {
    if let Err(err) = draw_header_inner(cr, ctx, name, page, total) {
        tracing::warn!(?err, "print: header draw failed");
    }
}

fn draw_header_inner(
    cr: &gtk::cairo::Context,
    ctx: &gtk::PrintContext,
    name: &str,
    page: usize,
    total: usize,
) -> Result<(), gtk::cairo::Error> {
    use gtk::cairo::{FontSlant, FontWeight};

    // Header spans the same left..right column as the body text, so it aligns
    // with the margins rather than running to the paper edge.
    let left = PAGE_MARGIN_PT;
    let right = ctx.width() - PAGE_MARGIN_PT;
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
    cr.set_font_size(9.0);

    // Filename, left-aligned.
    cr.move_to(left, HEADER_BASELINE_PT);
    cr.show_text(name)?;

    // Local date, centred in the text column. Best-effort — an unavailable
    // clock just omits it.
    if let Some(date) = gtk::glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%x").ok())
    {
        let ext = cr.text_extents(&date)?;
        cr.move_to(
            left + ((right - left) - ext.width()) / 2.0,
            HEADER_BASELINE_PT,
        );
        cr.show_text(&date)?;
    }

    // "Page N of M", right-aligned.
    let page_text = format!("Page {page} of {total}");
    let ext = cr.text_extents(&page_text)?;
    cr.move_to(right - ext.width(), HEADER_BASELINE_PT);
    cr.show_text(&page_text)?;

    // Rule beneath the header.
    cr.set_line_width(0.5);
    cr.move_to(left, HEADER_RULE_Y_PT);
    cr.line_to(right, HEADER_RULE_Y_PT);
    cr.stroke()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_print_operation, paginate, sptr_t, EditorHandle, MAX_PAGES};
    use codepp_scintilla_sys::{scintilla_new, SCI_INSERTTEXT};
    use gtk::glib::translate::FromGlibPtrNone;
    use gtk::prelude::*;
    use std::ffi::CString;

    // --- pagination logic (pure, no display) -------------------------

    #[test]
    fn paginate_records_each_page_start() {
        // A measurer that fits 10 characters per page over a 35-char doc.
        let starts = paginate(35, |cp| cp + 10);
        assert_eq!(starts, vec![0, 10, 20, 30]);
    }

    #[test]
    fn paginate_empty_document_is_one_page() {
        let starts = paginate(0, |cp| cp + 10);
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn paginate_breaks_on_a_non_advancing_measure() {
        // A measurer that never advances must not loop forever — it yields a
        // single page instead.
        let starts = paginate(100, |cp| cp);
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn paginate_is_capped_at_max_pages() {
        // One character per page over an effectively unbounded document stops
        // at the cap rather than allocating without limit.
        let starts = paginate(sptr_t::MAX, |cp| cp + 1);
        assert_eq!(starts.len(), MAX_PAGES);
    }

    /// End-to-end render check, `#[ignore]`d because it needs a display.
    /// Builds the print operation over a real Scintilla widget holding
    /// multi-page text and EXPORTs it to a PDF — no dialog, no printer, no
    /// mapped window — then asserts a real, multi-byte PDF came out. This
    /// exercises the actual `SCI_FORMATRANGEFULL` + cairo path this module
    /// wires, the one part that a headless unit test could otherwise not
    /// reach.
    ///
    /// Like the other GTK display-gated tests it must run single-threaded so
    /// GDK is never touched from two threads at once:
    /// `cargo test -p codepp-ui-gtk -- --ignored --test-threads=1`.
    #[test]
    #[ignore = "needs a display; run with -- --ignored --test-threads=1"]
    fn build_print_operation_exports_a_pdf() {
        gtk::init().expect("gtk::init failed — no display?");
        // SAFETY: GTK is initialised, `scintilla_new`'s only precondition.
        let ptr = unsafe { scintilla_new() };
        assert!(!ptr.is_null(), "scintilla_new returned null");
        // Leak a reference for the test's duration — the `EditorHandle`
        // below carries raw pointers into this widget, exactly as the real
        // program's `GtkUiState.sci_widget` holds one.
        let widget: gtk::Widget =
            unsafe { gtk::Widget::from_glib_none(ptr.cast::<gtk::ffi::GtkWidget>()) };
        std::mem::forget(widget);
        // SAFETY: `ptr` is the live widget just leaked.
        let editor = unsafe { EditorHandle::from_gtk_widget(ptr) }.expect("no direct-call pair");

        // Enough lines to force several pages.
        let mut body = String::new();
        for n in 1..=300 {
            use std::fmt::Write as _;
            let _ = writeln!(
                body,
                "Line {n}: the quick brown fox jumps over the lazy dog"
            );
        }
        let c = CString::new(body).expect("no interior NUL");
        editor.send(SCI_INSERTTEXT, 0, c.as_ptr() as sptr_t);

        let op = build_print_operation(editor, "print-test.txt");
        let mut path = std::env::temp_dir();
        path.push("codepp-print-export-test.pdf");
        let _ = std::fs::remove_file(&path);
        op.set_export_filename(&path);
        let res = op.run(gtk::PrintOperationAction::Export, None::<&gtk::Window>);
        assert!(res.is_ok(), "export run failed: {res:?}");

        let bytes = std::fs::read(&path).expect("no PDF written");
        assert!(bytes.starts_with(b"%PDF"), "output is not a PDF");
        assert!(
            bytes.len() > 2000,
            "PDF is suspiciously small ({} bytes) — text may not have rendered",
            bytes.len()
        );
        let _ = std::fs::remove_file(&path);
    }
}
