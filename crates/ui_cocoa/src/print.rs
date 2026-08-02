//! Printing the active buffer via `NSPrintOperation` + Scintilla's
//! `SCI_FORMATRANGEFULL`.
//!
//! The Cocoa counterpart of `ui_gtk::print` and `ui_win32::print`.
//! `NSPrintOperation` owns the printer picker, the page-range and copies
//! selection and the per-page loop, so this module only has to paginate,
//! hand Scintilla the right graphics context per page, and paint a
//! header.
//!
//! # How Scintilla renders onto a printer
//!
//! `SCI_FORMATRANGEFULL(draw, &Sci_RangeToFormatFull)` renders a
//! character range into a rectangle on a *surface* and returns the
//! position of the next character that did not fit — the start of the
//! next page. The `hdc` / `hdc_target` fields are not Win32 `HDC`s: they
//! are whatever the platform layer treats as a `SurfaceID`, and on Cocoa
//! `SurfaceImpl::Init(SurfaceID sid, …)` casts that straight to a
//! **`CGContextRef`** (`cocoa/PlatCocoa.mm:464`). So this passes the
//! print operation's own `CGContext`.
//!
//! **The print view is flipped**, and that is required rather than
//! stylistic. `Sci_Rectangle` has `top < bottom` with y increasing
//! downward, and Scintilla's Cocoa surface is written against
//! `SCIContentView`, which answers `isFlipped` YES
//! (`cocoa/ScintillaView.mm:395`) and hands its context straight to
//! `ScintillaCocoa::Draw`. An unflipped print view would render the page
//! upside down.
//!
//! # The two passes
//!
//! 1. **`knowsPageRange:`** — paginate: loop `draw = 0` from the document
//!    start, recording each page's first character, until the document is
//!    consumed. AppKit has no graphics context established at that point,
//!    so the measuring pass runs against a scratch bitmap context; the
//!    measurement is CoreText layout and does not depend on the
//!    destination.
//! 2. **`drawRect:`** — paint the header, then `draw = 1` for the page's
//!    recorded range. AppKit asks only for the pages the user selected
//!    and handles copies itself.
//! 3. After the operation — `SCI_FORMATRANGEFULL(0, NULL)` releases the
//!    format cache built across the passes.
//!
//! Print settings mirror both other backends: colours preserved but
//! default-coloured backgrounds forced to white
//! (`SC_PRINT_COLOURONWHITEDEFAULTBG`), no magnification, word wrap on so
//! long lines are not clipped at the margin.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{define_class, msg_send, AnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSBitmapImageRep, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSPrintInfo, NSPrintOperation, NSStringDrawing, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSDictionary, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString,
};

use codepp_editor::EditorHandle;
use codepp_scintilla_sys::{
    sptr_t, Sci_CharacterRangeFull, Sci_RangeToFormatFull, Sci_Rectangle, SCI_FORMATRANGEFULL,
    SCI_GETLENGTH, SCI_SETPRINTCOLOURMODE, SCI_SETPRINTMAGNIFICATION, SCI_SETPRINTWRAPMODE,
    SC_PRINT_COLOURONWHITEDEFAULTBG, SC_WRAP_WORD,
};

use crate::state::with_state;

/// Page margin, in points at 72 dpi (~15 mm) — the same deliberate inset
/// `ui_win32` and `ui_gtk` use. The header sits in the top margin band,
/// above the text column.
const PAGE_MARGIN_PT: f64 = 43.0;
/// Header baseline, measured down from the page top.
const HEADER_BASELINE_PT: f64 = 10.0;
/// Y of the thin rule under the header text.
const HEADER_RULE_Y_PT: f64 = 26.0;
/// Header font size, in points.
const HEADER_FONT_PT: f64 = 9.0;
/// Hard cap on the page count, so a pathological document cannot spin the
/// single UI thread unboundedly. Matches both other backends.
const MAX_PAGES: usize = 100_000;

/// The sheet's printable band, in flipped (y-down) coordinates relative
/// to the paper's top-left — the coordinate space the print view and
/// `Sci_Rectangle` both use.
///
/// **Everything is placed inside this, not inside the paper.** A printer
/// cannot mark the whole sheet, and `NSPrintInfo` reports what it can as
/// `imageablePageBounds` — in *unflipped* page coordinates, measured from
/// the bottom-left. Measured on this machine: a 595×842 A4 sheet whose
/// printable band is 559×783 at origin (18, 41), i.e. 18 pt in from the
/// left and 18 pt down from the top. The header used to sit at a 10 pt
/// baseline measured from the paper edge, which is *outside* that band —
/// it survived a PDF render, where there is no hardware margin, and would
/// have been clipped on a real printer.
///
/// `ui_gtk` gets this for free (`GtkPrintContext` already excludes the
/// non-printable area) and `ui_win32` takes it from `HORZRES`/`VERTRES`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Printable {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

/// The printable band for a sheet of `paper` with `imageable` bounds.
///
/// Pure because it is a coordinate flip, and a flip is exactly the kind
/// of arithmetic that looks right and prints wrong. Degenerate bounds —
/// a driver reporting nothing usable — fall back to the whole sheet,
/// which is what the code did before this existed.
fn printable_band(paper: NSSize, imageable: NSRect) -> Printable {
    if imageable.size.width <= 0.0 || imageable.size.height <= 0.0 {
        return Printable {
            left: 0.0,
            top: 0.0,
            width: paper.width,
            height: paper.height,
        };
    }
    Printable {
        left: imageable.origin.x,
        // `imageable.origin.y` is measured up from the bottom, so the
        // distance from the *top* is what is left over above the band.
        top: paper.height - (imageable.origin.y + imageable.size.height),
        width: imageable.size.width,
        height: imageable.size.height,
    }
}

thread_local! {
    /// The job the running print operation is rendering.
    ///
    /// A `thread_local` rather than an ivar on the view for the same
    /// reason the other panels on this backend use one: `define_class!`
    /// ivars are awkward for non-Objective-C payloads, AppKit calls the
    /// view back on the main thread only, and an `NSPrintOperation` is
    /// modal — there is exactly one job at a time.
    static JOB: RefCell<Option<Job>> = const { RefCell::new(None) };
}

/// What the print view needs to render, captured before the operation
/// runs so no callback has to reach through `with_state` — AppKit drives
/// `drawRect:` from inside its own modal loop, where a nested borrow
/// would be declined and the page would come out blank.
struct Job {
    editor: EditorHandle,
    /// First character of each page, from the pagination pass.
    page_starts: Vec<sptr_t>,
    doc_len: sptr_t,
    /// Paper size in points, as `NSPrintInfo` reports it. The page
    /// *rects* are the full sheet — AppKit centres them 1:1 — while the
    /// content is placed inside [`Self::band`].
    page: NSSize,
    /// The printable band of that sheet. See [`Printable`].
    band: Printable,
    /// Header text — the buffer's display name, already sanitized.
    header: String,
}

/// Print the active buffer: show the native print panel and, on accept,
/// render every requested page. The entry point behind File → Print… and
/// its ⌘P equivalent.
pub(crate) fn show() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // One job at a time — enforced, not merely asserted. `runOperation`
    // spins a modal loop that still services the GCD main-queue source,
    // so a second `show()` dispatched into that window would overwrite
    // `JOB` and then clear it on *its* way out, leaving the outer
    // operation to render its remaining pages against `None` and emit
    // blank paper with nothing reported. Unreachable from the menu today
    // (AppKit disables the bar during the panel) but reachable the moment
    // `dispatch_npp_menu_command` is wired on this backend, which §7.5
    // still lists as outstanding.
    if JOB.with(|j| j.borrow().is_some()) {
        tracing::warn!("print: a job is already running; ignoring the request");
        return;
    }
    let Some((editor, header)) = with_state(|st| {
        let header = st
            .shell
            .active()
            .map_or_else(|| "Untitled".to_string(), codepp_shell::tab_display_name);
        (st.editor, header)
    }) else {
        return;
    };

    let info = NSPrintInfo::sharedPrintInfo();
    let page = info.paperSize();

    let doc_len = editor.send(SCI_GETLENGTH, 0, 0);
    apply_print_settings(&editor);

    let band = printable_band(page, info.imageablePageBounds());
    let page_starts = paginate(doc_len, |cp| {
        measure_page(&editor, cp, doc_len, page, band).unwrap_or(cp)
    });
    let pages = page_starts.len();

    JOB.with(|j| {
        *j.borrow_mut() = Some(Job {
            editor,
            page_starts,
            doc_len,
            page,
            band,
            header: codepp_shell::sanitize_str_for_display(&header),
        });
    });

    let view = PrintView::new(
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            // One tall view, `pages` pages high — the classic AppKit
            // paginated-view model, where `rectForPage:` carves it up.
            NSSize::new(page.width, page.height * page_count_f64(pages)),
        ),
        mtm,
    );

    // The panel runs a nested modal session, which services the GCD
    // main-queue source — the same freeze every other modal here takes.
    let _freeze = crate::DrainFreeze::new();
    let op = NSPrintOperation::printOperationWithView_printInfo(&view, &info);
    let title = JOB.with(|j| {
        j.borrow()
            .as_ref()
            .map_or_else(String::new, |job| job.header.clone())
    });
    op.setJobTitle(Some(&NSString::from_str(&title)));
    op.runOperation();

    // Release Scintilla's format cache and drop the job either way — a
    // cancelled panel still built one.
    release_format_cache(&editor);
    JOB.with(|j| *j.borrow_mut() = None);
}

/// Colour and wrap settings for the printed page, identical on all three
/// backends: keep syntax colours, but force a default-coloured background
/// to white so a dark theme does not flood the page with toner.
fn apply_print_settings(editor: &EditorHandle) {
    editor.send(
        SCI_SETPRINTCOLOURMODE,
        SC_PRINT_COLOURONWHITEDEFAULTBG as usize,
        0,
    );
    editor.send(SCI_SETPRINTMAGNIFICATION, 0, 0);
    editor.send(SCI_SETPRINTWRAPMODE, SC_WRAP_WORD, 0);
}

/// `SCI_FORMATRANGEFULL(0, NULL)` — releases the format cache Scintilla
/// builds across a print run. Documented in `ScintillaDoc.html` §Printing
/// as required after the last page.
fn release_format_cache(editor: &EditorHandle) {
    editor.send(SCI_FORMATRANGEFULL, 0, 0);
}

/// The text column, inset by [`PAGE_MARGIN_PT`] inside the printable
/// band, in flipped coordinates relative to the page's own top-left.
fn text_rect(band: Printable) -> Sci_Rectangle {
    Sci_Rectangle {
        left: to_i32(band.left + PAGE_MARGIN_PT),
        top: to_i32(band.top + PAGE_MARGIN_PT),
        right: to_i32(band.left + band.width - PAGE_MARGIN_PT),
        bottom: to_i32(band.top + band.height - PAGE_MARGIN_PT),
    }
}

/// Where one page's content lands: the sheet, its printable band, and
/// the y offset of this page within the tall print view.
#[derive(Clone, Copy)]
struct PageGeometry {
    page: NSSize,
    band: Printable,
    offset_y: i32,
}

/// Run one `SCI_FORMATRANGEFULL` pass and return the next character
/// position. `draw` selects measure-only (0) or render (1); `offset_y`
/// shifts the target rectangle down for a page in a taller view.
fn format_range(
    editor: &EditorHandle,
    context: *mut c_void,
    range: (sptr_t, sptr_t),
    geometry: PageGeometry,
    draw: usize,
) -> sptr_t {
    let (from, to) = range;
    let PageGeometry {
        page,
        band,
        offset_y,
    } = geometry;
    let mut rc = text_rect(band);
    rc.top += offset_y;
    rc.bottom += offset_y;
    let rc_page = Sci_Rectangle {
        left: 0,
        top: offset_y,
        right: to_i32(page.width),
        bottom: to_i32(page.height) + offset_y,
    };
    let mut range = Sci_RangeToFormatFull {
        hdc: context,
        hdc_target: context,
        rc,
        rc_page,
        chrg: Sci_CharacterRangeFull {
            cp_min: from,
            cp_max: to,
        },
    };
    editor.send(
        SCI_FORMATRANGEFULL,
        draw,
        std::ptr::from_mut(&mut range) as isize,
    )
}

/// Measure how far one page reaches from `cp`, against a scratch bitmap
/// context.
///
/// AppKit establishes no graphics context before `knowsPageRange:`, and
/// the pagination has to happen before the operation can report a page
/// count — so the measuring pass needs a context of its own. Scintilla's
/// measurement is CoreText layout, which does not depend on where the
/// pixels would land, so a small off-screen bitmap is sufficient and the
/// page geometry comes from the rectangle rather than from the context.
fn measure_page(
    editor: &EditorHandle,
    cp: sptr_t,
    doc_len: sptr_t,
    page: NSSize,
    band: Printable,
) -> Option<sptr_t> {
    let mtm = MainThreadMarker::new()?;
    // SAFETY: `NSBitmapImageRep`'s designated initialiser with a null
    // plane pointer, which is the documented "allocate the backing store
    // for me" form. Every other argument is a compile-time constant, and
    // the 8x8 RGBA shape is internally consistent (4 samples, 8 bits
    // each, non-planar, row/pixel strides derived by AppKit from the
    // zeros). Returns `None` rather than a broken object on failure.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            8,
            8,
            8,
            4,
            true,
            false,
            objc2_app_kit::NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let gc = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    let _ = mtm;
    let context = current_cg_context(&gc)?;
    Some(format_range(
        editor,
        context,
        (cp, doc_len),
        PageGeometry {
            page,
            band,
            offset_y: 0,
        },
        0,
    ))
}

/// Page-start positions for a document of `doc_len`, given a measurer
/// that reports where the page starting at `cp` ends.
///
/// [`MAX_PAGES`] caps the count so an extreme document cannot spin the UI
/// thread unboundedly. Always returns at least one page, so an empty
/// buffer still prints a (blank, headed) sheet rather than producing an
/// operation with nothing to render.
///
/// Pure and measurer-agnostic, so the edge cases are unit-testable
/// without a window server or a real Scintilla surface — the same shape
/// `ui_gtk::print::paginate` has.
fn paginate(doc_len: sptr_t, mut measure: impl FnMut(sptr_t) -> sptr_t) -> Vec<sptr_t> {
    let mut starts = Vec::new();
    let mut cp: sptr_t = 0;
    while cp < doc_len && starts.len() < MAX_PAGES {
        starts.push(cp);
        let next = measure(cp);
        // A non-advancing measurer — a degenerate zero-height text rect,
        // or a failed scratch context — would otherwise loop forever.
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

/// A page count or index as `f64`, losslessly.
///
/// Counts are bounded by [`MAX_PAGES`], far inside `f64`'s exact-integer
/// range, so the saturation can never fire for a real job — it is here so
/// the conversion is total rather than a suppressed lint.
fn page_count_f64(pages: usize) -> f64 {
    f64::from(u32::try_from(pages).unwrap_or(u32::MAX))
}

/// A point coordinate as an `i32` for `Sci_Rectangle`, saturating rather
/// than wrapping. Page geometry is small; the saturation is total-ness,
/// not an expected case.
fn to_i32(v: f64) -> i32 {
    if v.is_nan() {
        return 0;
    }
    v.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Which page a draw rectangle belongs to, and the range it covers.
///
/// Pure because the arithmetic — a rect's y divided by the page height,
/// and the end of the last page being the document end rather than the
/// next start — is exactly the kind of off-by-one a hands-on demo is
/// worst at catching.
fn page_for_rect(
    rect_top: f64,
    page_height: f64,
    starts: &[sptr_t],
    doc_len: sptr_t,
) -> Option<(usize, sptr_t, sptr_t)> {
    if page_height <= 0.0 || starts.is_empty() {
        return None;
    }
    let index = to_i32((rect_top / page_height).floor().max(0.0));
    let index = usize::try_from(index).ok()?;
    let from = *starts.get(index)?;
    let to = starts.get(index + 1).copied().unwrap_or(doc_len);
    Some((index, from, to))
}

define_class!(
    // SAFETY: a plain `NSView` subclass with no ivars. AppKit drives it
    // on the main thread for the duration of one modal print operation.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppPrintView"]
    struct PrintView;

    impl PrintView {
        /// Flipped, so y increases downward and the view's coordinates
        /// match `Sci_Rectangle`'s convention. See the module docs — an
        /// unflipped view prints the page upside down.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> Bool {
            Bool::YES
        }

        /// Report the page count AppKit should ask for.
        ///
        /// The pagination itself already happened in [`show`], before the
        /// operation was created — the view's own height depends on the
        /// count, so it cannot be deferred to here.
        #[unsafe(method(knowsPageRange:))]
        fn knows_page_range(&self, range: *mut NSRange) -> Bool {
            crate::at_callback_boundary("print:knowsPageRange", Bool::NO, || {
                let pages = JOB.with(|j| j.borrow().as_ref().map_or(0, |job| job.page_starts.len()));
                if pages == 0 || range.is_null() {
                    return Bool::NO;
                }
                // SAFETY: AppKit passes a valid `NSRange` out-parameter
                // for the duration of this call.
                unsafe {
                    (*range).location = 1;
                    (*range).length = pages;
                }
                Bool::YES
            })
        }

        /// The rectangle for a one-based page number, carved out of the
        /// tall view.
        #[unsafe(method(rectForPage:))]
        fn rect_for_page(&self, page: isize) -> NSRect {
            crate::at_callback_boundary("print:rectForPage", NSRect::ZERO, || {
                JOB.with(|j| {
                    j.borrow().as_ref().map_or(NSRect::ZERO, |job| {
                        let index = page_count_f64(usize::try_from(page.max(1) - 1).unwrap_or(0));
                        NSRect::new(
                            NSPoint::new(0.0, index * job.page.height),
                            NSSize::new(job.page.width, job.page.height),
                        )
                    })
                })
            })
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, rect: NSRect) {
            crate::at_callback_boundary("print:drawRect", (), || draw_page(rect));
        }
    }
);

impl PrintView {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: `NSView`'s designated initialiser on a freshly
        // allocated instance of our own subclass, which adds no ivars.
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

/// The `CGContextRef` behind an `NSGraphicsContext`, or `None` if this
/// context has none to give.
///
/// **Sent unchecked, deliberately.** During a print *preview* AppKit
/// makes a private `NSPrintPreviewGraphicsContext` current, and that
/// class implements `CGContext` by *forwarding* rather than by declaring
/// it — so `respondsToSelector:` answers YES and the send works, while
/// `class_getInstanceMethod` finds nothing. objc2's generated accessor
/// consults the latter under `debug_assertions` and panics
/// ("invalid message send … method not found") before the message is
/// ever sent.
///
/// That panic is caught by [`crate::at_callback_boundary`], so the page
/// draws *nothing* and the preview comes up blank — every page, while
/// pagination still reports the right count, because pagination measures
/// against a bitmap context that has a real `CGContext`. Release builds
/// skip the verification and print correctly, which is why this survived
/// a PDF-based check and a release-build check and was found by a user
/// running `cargo run` — the documented development workflow.
///
/// `respondsToSelector:` is still consulted, so a context that genuinely
/// cannot supply one is declined rather than sent to blindly.
///
/// # The returned pointer is borrowed, not owned
///
/// `CGContext` is a Get-Rule property: the pointer belongs to
/// `context`, and it is valid only while `context` is. **The caller must
/// keep the `NSGraphicsContext` alive until after the pointer's last
/// use** — which for both callers here means the binding must outlive
/// the `SCI_FORMATRANGEFULL` call, not merely the call to this function.
///
/// Passing a temporary — `current_cg_context(&NSGraphicsContext::currentContext()?)`
/// — would compile and produce a dangling pointer, so a source guard
/// bans that shape. Worth noting that the checked accessor this replaced
/// gave no protection here either: `Retained::as_ptr(&x.CGContext())`
/// drops its temporary `Retained` at the end of that statement, so the
/// extra retain was gone before the pointer was ever used. The lifetime
/// has always rested on the owning `NSGraphicsContext`; this only says
/// so out loud.
fn current_cg_context(context: &NSGraphicsContext) -> Option<*mut c_void> {
    let obj: &objc2::runtime::NSObject = context;
    let sel = objc2::runtime::Sel::register(c"CGContext");
    if !obj.respondsToSelector(sel) {
        return None;
    }
    // SAFETY: `CGContext` takes no arguments and returns a `CGContextRef`
    // — a pointer — which is what this signature declares. The selector
    // is checked immediately above, so the receiver does implement it,
    // by forwarding or otherwise.
    let send: unsafe extern "C" fn(
        *const objc2::runtime::AnyObject,
        objc2::runtime::Sel,
    ) -> *mut c_void = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as *const ()) };
    let ptr = unsafe { send(std::ptr::from_ref(obj).cast(), sel) };
    (!ptr.is_null()).then_some(ptr)
}

/// Render one page: the header band, then Scintilla's range.
fn draw_page(rect: NSRect) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(context) = NSGraphicsContext::currentContext() else {
        return;
    };
    let Some(cg) = current_cg_context(&context) else {
        return;
    };

    JOB.with(|j| {
        let borrow = j.borrow();
        let Some(job) = borrow.as_ref() else {
            return;
        };
        let Some((index, from, to)) = page_for_rect(
            rect.origin.y,
            job.page.height,
            &job.page_starts,
            job.doc_len,
        ) else {
            return;
        };
        let offset_y = to_i32(page_count_f64(index) * job.page.height);
        draw_header(job, index, rect, mtm);
        format_range(
            &job.editor,
            cg,
            (from, to),
            PageGeometry {
                page: job.page,
                band: job.band,
                offset_y,
            },
            1,
        );
    });
}

/// The header band: the buffer name at the left, "Page N" at the right,
/// and a hairline under both.
fn draw_header(job: &Job, index: usize, rect: NSRect, mtm: MainThreadMarker) {
    let font = NSFont::systemFontOfSize(HEADER_FONT_PT);
    let keys: Vec<&objc2_foundation::NSString> =
        // SAFETY: reading two AppKit string constants. They are
        // immortal statics owned by the framework; the `unsafe` is
        // objc2's blanket rule for `extern` statics, not a real
        // obligation to discharge here.
        vec![unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }];
    let black = NSColor::blackColor();
    let values: Vec<&objc2::runtime::AnyObject> = vec![&font, &black];
    let attrs = NSDictionary::from_slices(&keys, &values);

    let top = rect.origin.y + job.band.top;
    let name = NSString::from_str(&job.header);
    // SAFETY: `drawAtPoint:withAttributes:` requires a current graphics
    // context, which AppKit has established — this only runs from
    // `drawRect:`. The attributes dictionary holds a font and a colour
    // and outlives the call.
    unsafe {
        name.drawAtPoint_withAttributes(
            NSPoint::new(job.band.left + PAGE_MARGIN_PT, top + HEADER_BASELINE_PT),
            Some(&attrs),
        );
    }
    let page_label = NSString::from_str(&format!("Page {}", index + 1));
    // SAFETY: measurement only, and the same attributes dictionary.
    let size = unsafe { page_label.sizeWithAttributes(Some(&attrs)) };
    // SAFETY: as for the name above.
    unsafe {
        page_label.drawAtPoint_withAttributes(
            NSPoint::new(
                job.band.left + job.band.width - PAGE_MARGIN_PT - size.width,
                top + HEADER_BASELINE_PT,
            ),
            Some(&attrs),
        );
    }

    // The hairline. Drawn with `NSBezierPath` rather than into the raw
    // CGContext so it inherits the same flipped coordinate system the
    // text above just used.
    let rule = NSRect::new(
        NSPoint::new(job.band.left + PAGE_MARGIN_PT, top + HEADER_RULE_Y_PT),
        NSSize::new(job.band.width - 2.0 * PAGE_MARGIN_PT, 0.5),
    );
    NSColor::blackColor().setFill();
    objc2_app_kit::NSBezierPath::fillRect(rule);
    let _ = mtm;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paginate_records_each_page_start() {
        // A measurer that fits 10 characters per page over a 35-char doc.
        let starts = paginate(35, |cp| cp + 10);
        assert_eq!(starts, vec![0, 10, 20, 30]);
    }

    #[test]
    fn an_empty_document_still_yields_one_page() {
        let starts = paginate(0, |cp| cp + 10);
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn a_non_advancing_measurer_terminates() {
        // A degenerate text rect reports no progress. Without the guard
        // this is an infinite loop on the UI thread.
        let starts = paginate(100, |cp| cp);
        assert_eq!(starts, vec![0]);
        // ...and one that goes backwards is treated the same way.
        assert_eq!(paginate(100, |cp| cp - 1), vec![0]);
    }

    #[test]
    fn the_page_cap_bounds_a_pathological_document() {
        let starts = paginate(sptr_t::MAX, |cp| cp + 1);
        assert_eq!(starts.len(), MAX_PAGES);
    }

    #[test]
    fn a_rect_resolves_to_its_page_and_range() {
        let starts = vec![0, 40, 90];
        // First page.
        assert_eq!(page_for_rect(0.0, 100.0, &starts, 130), Some((0, 0, 40)));
        // Second — and the boundary belongs to the page it starts.
        assert_eq!(page_for_rect(100.0, 100.0, &starts, 130), Some((1, 40, 90)));
        assert_eq!(page_for_rect(199.9, 100.0, &starts, 130), Some((1, 40, 90)));
        // The last page runs to the document end, not to a next start.
        assert_eq!(
            page_for_rect(200.0, 100.0, &starts, 130),
            Some((2, 90, 130))
        );
        // Past the end: no page rather than a wrapped index.
        assert_eq!(page_for_rect(300.0, 100.0, &starts, 130), None);
    }

    /// Exact equality is correct here: every value below is either a
    /// literal or a sum of literals that is exactly representable, so an
    /// epsilon would only hide a real arithmetic change.
    #[allow(clippy::float_cmp)]
    #[test]
    fn the_printable_band_flips_the_imageable_bounds() {
        // Measured on this machine: A4 at 595x842, printable 559x783 at
        // (18, 41) from the bottom-left. So 18 pt in from the left, and
        // 842 - (41 + 783) = 18 pt down from the top.
        let band = printable_band(
            NSSize::new(595.0, 842.0),
            NSRect::new(NSPoint::new(18.0, 41.0), NSSize::new(559.0, 783.0)),
        );
        assert_eq!(band.left, 18.0);
        assert_eq!(band.top, 18.0);
        assert_eq!(band.width, 559.0);
        assert_eq!(band.height, 783.0);
        // The band's bottom edge sits 41 pt above the paper's, which is
        // the asymmetry the flip exists to get right.
        assert_eq!(842.0 - (band.top + band.height), 41.0);
    }

    #[allow(clippy::float_cmp)]
    #[test]
    fn a_driver_reporting_no_printable_area_falls_back_to_the_sheet() {
        let paper = NSSize::new(595.0, 842.0);
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        let band = printable_band(paper, zero);
        assert_eq!(
            (band.left, band.top, band.width, band.height),
            (0.0, 0.0, 595.0, 842.0)
        );
    }

    #[test]
    fn the_header_sits_inside_the_printable_band() {
        // The bug this band exists to fix: a 10 pt baseline measured from
        // the *paper* edge is above an 18 pt printable top, so a real
        // printer clips it. Measured from the band, it cannot be.
        let band = printable_band(
            NSSize::new(595.0, 842.0),
            NSRect::new(NSPoint::new(18.0, 41.0), NSSize::new(559.0, 783.0)),
        );
        assert!(band.top + HEADER_BASELINE_PT >= band.top);
        assert!(band.top + HEADER_RULE_Y_PT < band.top + PAGE_MARGIN_PT);
        // ...and the text column starts below the rule, so they cannot
        // collide however the two constants are retuned.
        let text = text_rect(band);
        assert!(f64::from(text.top) > band.top + HEADER_RULE_Y_PT);
        assert!(f64::from(text.bottom) <= band.top + band.height);
        assert!(f64::from(text.right) <= band.left + band.width);
    }

    #[test]
    fn degenerate_page_geometry_resolves_to_nothing() {
        assert_eq!(page_for_rect(0.0, 0.0, &[0], 10), None);
        assert_eq!(page_for_rect(0.0, -5.0, &[0], 10), None);
        assert_eq!(page_for_rect(0.0, 100.0, &[], 10), None);
    }
}
