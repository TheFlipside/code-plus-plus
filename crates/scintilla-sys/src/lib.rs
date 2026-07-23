//! Raw FFI to vendored Scintilla 5.x and Lexilla 5.x.
//!
//! Phase 1 surface: just enough to register the Scintilla Win32 window class,
//! call into Scintilla via `SendMessage`, and capture the direct-call
//! function pointer. The full message constant set lands progressively in
//! later phases.
//!
//! See DESIGN.md §4.1 (vendoring), §4.2 (direct-call API), §6 (plugin ABI).
//!
//! # Platform layout
//!
//! Everything below the `extern` blocks — the `SCI_*` / `SCE_*` constant
//! set and the `#[repr(C)]` structs — is platform-neutral: those numbers
//! come from `Scintilla.h` and are identical on every backend. Only the
//! *entry points* differ, because Scintilla exposes a different bootstrap
//! per platform:
//!
//! - **Win32** registers a window class (`Scintilla_RegisterClasses`) and
//!   the host creates the control with `CreateWindowExW`.
//! - **GTK** exposes a `GtkWidget` factory (`scintilla_new`) plus a
//!   message entry point (`scintilla_send_message`); there is no window
//!   class to register.
//!
//! The direct-call fast path (DESIGN.md §4.2) is available on both —
//! `Message::GetDirectFunction` / `GetDirectPointer` are handled by
//! `vendor/scintilla/gtk/ScintillaGTK.cxx` exactly as they are by
//! `win32/ScintillaWin.cxx`, so `editor` is backend-agnostic once it
//! holds the `(fn_ptr, instance_ptr)` pair.

#![allow(non_camel_case_types)]

use core::ffi::c_void;

/// Scintilla's signed pointer-sized integer (`Sci_PositionCR`/`sptr_t` in
/// Scintilla.h). Used for return values and `lparam`.
pub type sptr_t = isize;

/// Scintilla's unsigned pointer-sized integer (`uptr_t` in Scintilla.h).
/// Used for `wparam`.
pub type uptr_t = usize;

/// Scintilla's direct-call function signature. Returned by
/// `SCI_GETDIRECTFUNCTION`; must be paired with the instance pointer
/// returned by `SCI_GETDIRECTPOINTER`. Calling this directly bypasses the
/// window message pump and is the speed path Notepad++ uses.
pub type ScintillaDirectFunction =
    unsafe extern "C" fn(ptr: *mut c_void, msg: u32, wparam: uptr_t, lparam: sptr_t) -> sptr_t;

#[cfg(target_os = "windows")]
extern "C" {
    /// Register Scintilla's window classes with the given module handle.
    /// Must be called once before creating any Scintilla controls. Returns
    /// non-zero on success.
    ///
    /// Provided by `vendor/scintilla/win32/ScintillaWin.cxx` when statically
    /// linked.
    pub fn Scintilla_RegisterClasses(h_instance: *mut c_void) -> i32;

    /// Release Scintilla's process-wide resources. Optional; called at
    /// shutdown for clean process exit.
    pub fn Scintilla_ReleaseResources() -> i32;
}

// GTK entry points, mirroring `vendor/scintilla/include/ScintillaWidget.h`.
//
// The GTK backend has no window class to register: `scintilla_new`
// constructs a `GtkWidget*` the host packs into its own container, and
// `scintilla_send_message` is the `SendMessage` equivalent used for
// setup and for capturing the direct-call pair.
//
// Declared here rather than obtained from a GTK binding crate because
// the Scintilla widget is not part of GTK — it is our statically-linked
// vendored C++ — so no binding crate knows about it.
#[cfg(target_os = "linux")]
extern "C" {
    /// Construct a new Scintilla `GtkWidget*`. The returned pointer is a
    /// floating `GObject` reference following standard GTK ownership
    /// rules: the container it is added to sinks the reference.
    ///
    /// In practice this does not return null — `ScintillaGTK.cxx`
    /// builds the widget with `g_object_new`, and a constructor
    /// exception is swallowed by a `catch (...)` that still yields a
    /// non-null widget (with an uninitialised interior). Callers
    /// should still null-check, both because that is the C contract
    /// and because a null here is the clearest signal that linking
    /// went wrong.
    ///
    /// GTK must be initialised (`gtk_init`) before this is called.
    ///
    /// Provided by `vendor/scintilla/gtk/ScintillaGTK.cxx` when
    /// statically linked.
    pub fn scintilla_new() -> *mut c_void;

    /// Send a Scintilla message to the widget returned by
    /// [`scintilla_new`]. Equivalent to Win32's `SendMessage`; reserved
    /// for setup and for the two direct-call capture messages. Hot paths
    /// must use the direct-call pointer instead (DESIGN.md §4.2).
    pub fn scintilla_send_message(
        sci: *mut c_void,
        message: u32,
        wparam: uptr_t,
        lparam: sptr_t,
    ) -> sptr_t;

    /// Release Scintilla's process-wide resources. The GTK sibling of
    /// `Scintilla_ReleaseResources`; note the different return type —
    /// the GTK entry point returns void.
    pub fn scintilla_release_resources();
}

// Lexilla's public C entry points are declared `__stdcall` on Win32
// (`LEXILLA_CALL` in `Lexilla.h`); on x64 Windows that resolves to the
// single Microsoft x64 calling convention so `extern "system"` ==
// `extern "C"`, but `system` is the convention-agnostic spelling and
// stays correct if/when we add an x86 build.
#[cfg(target_os = "windows")]
extern "system" {
    /// Construct an `ILexer5*` for the lexer registered under `name`
    /// (e.g. `b"cpp\0"`, `b"rust\0"`). Returns null if no concrete
    /// `Lex*.cxx` registered that name in `build.rs`. The returned
    /// pointer is owned by the lexer module — Scintilla calls
    /// `ILexer5::Release()` when `SCI_SETILEXER` replaces or detaches
    /// the lexer, so callers must not free it themselves.
    ///
    /// Provided by `vendor/lexilla/src/Lexilla.cxx` when statically
    /// linked together with the concrete `Lex*.cxx` files in
    /// `build.rs`.
    pub fn CreateLexer(name: *const core::ffi::c_char) -> *mut c_void;
}

// Off Windows `LEXILLA_CALL` expands to nothing, so the same entry point
// uses the platform's plain C convention. Spelled as a separate block
// rather than reusing `extern "system"` because the two are only
// interchangeable on x64 Windows — see the note above.
//
// Gated to `linux` specifically, **not** `not(windows)`: `build.rs`
// only compiles Lexilla on the two backends that have a Scintilla
// build, so on macOS the symbol has no definition. Declaring it there
// anyway would compile — an unreferenced extern links fine under
// dead-code elimination — but it would make every caller silently
// dependent on nothing ever calling them, which is not a guarantee
// worth resting on. Narrowing the gate turns a latent link error into
// a compile error at the call site instead. Widen this to include
// `macos` in the same commit that teaches `build.rs` to build the
// Cocoa backend.
#[cfg(target_os = "linux")]
extern "C" {
    /// See the Windows-side declaration above for the ownership contract.
    pub fn CreateLexer(name: *const core::ffi::c_char) -> *mut c_void;
}

/// The Win32 window class name registered by `Scintilla_RegisterClasses`.
/// No GTK equivalent — that backend hands out widgets, not class names.
#[cfg(target_os = "windows")]
pub const SCINTILLA_CLASS_NAME: &str = "Scintilla";

// --- Scintilla message constants (subset used by Phase 1) -----------------
//
// Numbers come from `vendor/scintilla/include/Scintilla.h`. The full set
// is added incrementally as later phases need each message.

pub const SCI_GETDIRECTFUNCTION: u32 = 2184;
pub const SCI_GETDIRECTPOINTER: u32 = 2185;

// Editing — wired in Phase 2+ but the constants live here for completeness.
pub const SCI_INSERTTEXT: u32 = 2003;
pub const SCI_CLEARALL: u32 = 2004;
pub const SCI_GETLENGTH: u32 = 2006;
pub const SCI_GETTEXT: u32 = 2182;
pub const SCI_SETTEXT: u32 = 2181;

// Clipboard / cursor-keyboard ops — Scintilla handles these natively
// when the editor has focus, but the host's Edit menu needs the
// constants too so menu clicks (no key event involved) reach the
// same code path.
pub const SCI_CUT: u32 = 2177;
pub const SCI_COPY: u32 = 2178;
pub const SCI_PASTE: u32 = 2179;
pub const SCI_CLEAR: u32 = 2180;
pub const SCI_GOTOLINE: u32 = 2024;
pub const SCI_GETLINECOUNT: u32 = 2154;
pub const SCI_LINEFROMPOSITION: u32 = 2166;

// --- Printing (`SCI_FORMATRANGEFULL` and friends) --------------------
//
// Scintilla renders a range of the document into a target HDC (screen,
// printer, or memory DC) via `SCI_FORMATRANGEFULL`. The host owns the
// DC lifecycle (StartDoc / StartPage / EndPage / EndDoc for a printer,
// or CreateCompatibleDC for a preview bitmap) and drives the message
// once per page. Return value: the next character position to format
// (== `chrg.cp_max` when the page hasn't consumed the whole remaining
// range, or all-consumed at end of document).
//
// **wparam semantics** — `1` = actually draw into `hdc`; `0` = measure
// only (Scintilla returns the next-page cpMin without touching the DC's
// pixels). Two-pass rendering (measure all pages first to know "page N
// of M", then draw the wanted subset) is the standard use.
//
// **hdc vs hdcTarget** — `hdc` is where pixels go; `hdcTarget` is what
// Scintilla asks for font metrics. For plain printing both point at the
// printer DC. For a screen-based Print Preview against printer metrics,
// `hdcTarget` stays the printer DC while `hdc` swaps to an offscreen
// bitmap DC — that's how the preview shows what the paper will really
// look like without dispatching to the printer.
//
// **Releasing the format cache** — after the last real render call, send
// one final `SCI_FORMATRANGEFULL(0, NULL)` to let Scintilla free the
// per-print caches it built. Without it the caches leak for the
// remaining Scintilla-instance lifetime (small, but observable in
// long-running sessions that print often).
//
// Message codes verified against `vendor/scintilla/include/Scintilla.h`
// (see the `SCI_FORMATRANGE` / `SCI_FORMATRANGEFULL` and
// `SCI_SETPRINTCOLOURMODE` / `SC_PRINT_*` blocks there).
pub const SCI_FORMATRANGE: u32 = 2151;
pub const SCI_FORMATRANGEFULL: u32 = 2777;
pub const SCI_SETPRINTMAGNIFICATION: u32 = 2146;
pub const SCI_GETPRINTMAGNIFICATION: u32 = 2147;
pub const SCI_SETPRINTCOLOURMODE: u32 = 2148;
pub const SCI_GETPRINTCOLOURMODE: u32 = 2149;
pub const SCI_SETPRINTWRAPMODE: u32 = 2406;
pub const SCI_GETPRINTWRAPMODE: u32 = 2407;

/// Print colour modes for `SCI_SETPRINTCOLOURMODE`.
///
/// * `SC_PRINT_NORMAL` — use the same on-screen colours (including any
///   dark background). Ink-hungry on dark themes.
/// * `SC_PRINT_INVERTLIGHT` — swap the lightness component of every
///   colour; keeps hue but flips light↔dark.
/// * `SC_PRINT_BLACKONWHITE` — every glyph prints as pure black.
/// * `SC_PRINT_COLOURONWHITE` — glyphs keep their editor colour, but
///   every background is forced white.
/// * `SC_PRINT_COLOURONWHITEDEFAULTBG` — as above, but styles that use
///   the default background inherit white too (i.e. "syntax colours on
///   plain white paper", matching Notepad++'s default and what nearly
///   every editor does when printing).
/// * `SC_PRINT_SCREENCOLOURS` — same as `SC_PRINT_NORMAL`, kept as a
///   distinct name for API-parity with Scintilla.
pub const SC_PRINT_NORMAL: i32 = 0;
pub const SC_PRINT_INVERTLIGHT: i32 = 1;
pub const SC_PRINT_BLACKONWHITE: i32 = 2;
pub const SC_PRINT_COLOURONWHITE: i32 = 3;
pub const SC_PRINT_COLOURONWHITEDEFAULTBG: i32 = 4;
pub const SC_PRINT_SCREENCOLOURS: i32 = 5;

/// `struct Sci_Rectangle` from `Scintilla.h` — a plain
/// `left`/`top`/`right`/`bottom` rectangle in device pixels of whatever
/// HDC the surrounding [`Sci_RangeToFormatFull`] targets. Field order
/// matches the C ABI exactly; a mismatch would corrupt the render.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sci_Rectangle {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// `struct Sci_CharacterRangeFull` from `Scintilla.h`. The `Full`
/// variant uses `Sci_Position` (== `ptrdiff_t` == `isize`) instead of
/// `long`, so documents larger than 2 GiB address correctly. Same as
/// `sptr_t` in this crate.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sci_CharacterRangeFull {
    pub cp_min: sptr_t,
    pub cp_max: sptr_t,
}

/// `struct Sci_RangeToFormatFull` from `Scintilla.h` — the `lparam`
/// payload for [`SCI_FORMATRANGEFULL`]. Layout is ABI-critical: the
/// vendored `SCI_FORMATRANGEFULL` dispatch reads this struct pointer
/// with fixed field offsets.
///
/// `hdc` and `hdc_target` are `Scintilla::SurfaceID` (== `void *`) in
/// the C header. On Win32 they hold Win32 `HDC` values via that same
/// `void *` typedef.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Sci_RangeToFormatFull {
    pub hdc: *mut c_void,
    pub hdc_target: *mut c_void,
    pub rc: Sci_Rectangle,
    pub rc_page: Sci_Rectangle,
    pub chrg: Sci_CharacterRangeFull,
}

// --- Container-lexer styling (Phase 4.6 m1c-3) --------------------------
//
// When Scintilla is put into container-lexer mode via
// `SCI_SETILEXER(0, 0)` (see `Editor::clear_lexer`), it stops
// running Lexilla's styling passes and instead fires
// `SCN_STYLENEEDED` whenever it needs styling for a byte range.
// The host is expected to compute the styles for that range and
// apply them via `SCI_STARTSTYLING(start)` +
// `SCI_SETSTYLING(length, style)` calls — one per contiguous
// same-style byte run. Numeric values verified against
// `vendor/scintilla/include/Scintilla.h`.

/// `SCI_STARTSTYLING(int start, int unused)` — position the
/// styler at byte offset `start` so subsequent
/// [`SCI_SETSTYLING`] calls paint bytes from there onward. The
/// `unused` argument (historically a style mask) is ignored
/// under the modern styling API; pass 0. Cross-referenced
/// against `Scintilla.h:96`.
pub const SCI_STARTSTYLING: u32 = 2032;

/// `SCI_SETSTYLING(int length, int style)` — paint the next
/// `length` bytes with `style`. Advances the internal styler
/// cursor by `length` bytes; the next call resumes from there.
/// Cross-referenced against `Scintilla.h:97`.
pub const SCI_SETSTYLING: u32 = 2033;

/// `SCI_GETENDSTYLED()` — return the byte offset up to which
/// Scintilla considers styling already applied. Container-mode
/// hosts start their next tokenisation run at (or before, per
/// line-alignment discipline) this position. Cross-referenced
/// against `Scintilla.h:89`.
pub const SCI_GETENDSTYLED: u32 = 2028;

/// `SCI_POSITIONFROMLINE(int line)` — return the byte offset of
/// the first character on `line`. Used by the `SCN_STYLENEEDED`
/// handler to align the tokenisation start to a line boundary
/// (safer restart point than a mid-line byte). Cross-
/// referenced against `Scintilla.h:553`.
pub const SCI_POSITIONFROMLINE: u32 = 2167;

/// `SCI_GETRANGEPOINTER(int start, int length)` — return a
/// pointer into Scintilla's internal buffer covering the
/// requested range. Zero-copy view of the document bytes,
/// valid only until the next buffer modification. The
/// container-mode host reads text through this rather than
/// copying via `SCI_GETTEXTRANGEFULL` to avoid the per-
/// `SCN_STYLENEEDED` allocation cost. Cross-referenced against
/// `Scintilla.h:1000`.
pub const SCI_GETRANGEPOINTER: u32 = 2643;

/// `SCN_STYLENEEDED` — the notification Scintilla sends when a
/// range needs container-side styling. `SCNotification.position`
/// carries the end position; `SCI_GETENDSTYLED` gives the start.
/// Delivered via `WM_NOTIFY` on the parent window (same
/// dispatch surface as `SCN_MODIFIED` / `SCN_UPDATEUI`).
/// Cross-referenced against `Scintilla.h:1288`.
pub const SCN_STYLENEEDED: u32 = 2000;
/// Column (visual / virtual-space-aware) of a byte offset on its
/// line. Used by the status bar to render `Col: N` after the
/// caret moves.
pub const SCI_GETCOLUMN: u32 = 2129;
/// Overtype (insert vs. overwrite) flag — toggled by the Insert
/// key, surfaced in the status bar's `INS`/`OVR` slot.
pub const SCI_GETOVERTYPE: u32 = 2187;
pub const SCI_DOCUMENTSTART: u32 = 2316;
pub const SCI_DOCUMENTEND: u32 = 2318;

// View toggles + zoom — driven by the View menu.
pub const SCI_SETWRAPMODE: u32 = 2268;
pub const SCI_GETWRAPMODE: u32 = 2269;
/// `SC_WRAP_NONE = 0` — no line wrapping (the default).
pub const SC_WRAP_NONE: usize = 0;
/// `SC_WRAP_WORD = 1` — wrap long lines at word boundaries, which is
/// what Notepad++'s View → Word Wrap enables.
pub const SC_WRAP_WORD: usize = 1;
pub const SCI_SETVIEWWS: u32 = 2021;
pub const SCI_GETVIEWWS: u32 = 2020;
/// `SCWS_INVISIBLE = 0` — whitespace not shown (the default).
pub const SCWS_INVISIBLE: usize = 0;
/// `SCWS_VISIBLEALWAYS = 1` — render every space/tab as a visible mark,
/// matching Notepad++'s View → Show Whitespace.
pub const SCWS_VISIBLEALWAYS: usize = 1;
pub const SCI_SETVIEWEOL: u32 = 2356;
pub const SCI_GETVIEWEOL: u32 = 2355;
pub const SCI_SETINDENTATIONGUIDES: u32 = 2132;
pub const SCI_GETINDENTATIONGUIDES: u32 = 2133;
/// `SC_IV_NONE = 0` — indentation-guide mode "off".
pub const SC_IV_NONE: usize = 0;
/// `SC_IV_LOOKBOTH = 3` — render guides at every level the
/// surrounding indented blocks declare, including across blank
/// lines (the most useful general-purpose setting; matches what
/// Notepad++ enables when the user toggles "Show indent guide").
pub const SC_IV_LOOKBOTH: usize = 3;
/// `SCI_SETTABWIDTH(int tabWidth)` — set the visible width of a
/// TAB character in units of one space, and (when
/// [`SCI_SETINDENT`] is left at its default of 0) also the
/// effective indent-level width used by the indent-guide
/// algorithm. Scintilla defaults to 8, which mis-draws guides
/// for the modern 4-space convention (Rust / Python / TS / JSON
/// / YAML) — a 4-space-indented function body sits at column 4,
/// but the first guide column with width=8 is 8, so no guide
/// appears at level 1. Verified against
/// `vendor/scintilla/include/Scintilla.h:100` (2036) and
/// `ScintillaDoc.html` §"Tabs and Indentation Guides."
pub const SCI_SETTABWIDTH: u32 = 2036;
/// `SCI_SETINDENT(int indentSize)` — set the number of spaces
/// used for one level of indentation. A value of 0 (Scintilla
/// default) means "use tab width" per
/// `ScintillaDoc.html` §"Tabs and Indentation Guides", which is
/// what Code++ relies on so a single [`SCI_SETTABWIDTH`] call
/// covers both semantics. Included in the FFI surface for
/// completeness and for future per-language overrides. Verified
/// against `vendor/scintilla/include/Scintilla.h:478` (2122).
pub const SCI_SETINDENT: u32 = 2122;
/// `STYLE_INDENTGUIDE = 37` — the framework-reserved style slot
/// controlling the foreground colour of the vertical dotted
/// lines the indent-guide algorithm draws. Lives inside the
/// 32..=39 predefined range that `SCI_STYLECLEARALL` resets to
/// `STYLE_DEFAULT`, so the per-language theme wiring in
/// `apply_default_styles` must re-apply it after every clear —
/// same discipline as `STYLE_LINENUMBER` (33) and
/// `STYLE_BRACELIGHT` / `STYLE_BRACEBAD` (34/35). Verified
/// against `vendor/scintilla/include/Scintilla.h:218`.
pub const STYLE_INDENTGUIDE: usize = 37;
/// `SCI_SETEOLMODE(int eolMode)` — set the byte sequence
/// Scintilla inserts when the user presses Enter (and the
/// target sequence used by `SCI_CONVERTEOLS`). Per-document
/// Scintilla state (mutates `pdoc->eolMode` per
/// `vendor/scintilla/src/Document.h`), so every fresh document
/// minted via `SCI_CREATEDOCUMENT` starts at Scintilla's
/// built-in default (CRLF on Windows) regardless of the loaded
/// file's actual EOL — requiring re-application per-doc, same
/// discipline as `SCI_SETTABWIDTH`. Verified against
/// `vendor/scintilla/include/Scintilla.h:95` (2031).
pub const SCI_SETEOLMODE: u32 = 2031;
/// `SC_EOL_CRLF = 0` — Windows / DOS / HTTP two-byte `\r\n`.
/// Scintilla's built-in default (see `vendor/scintilla/src/
/// Document.cxx` constructor).
pub const SC_EOL_CRLF: usize = 0;
/// `SC_EOL_CR = 1` — pre-OS-X classic Macintosh one-byte `\r`.
pub const SC_EOL_CR: usize = 1;
/// `SC_EOL_LF = 2` — Unix / modern Linux / modern macOS one-
/// byte `\n`. Verified against
/// `vendor/scintilla/include/Scintilla.h:90-92`.
pub const SC_EOL_LF: usize = 2;
pub const SCI_ZOOMIN: u32 = 2333;
pub const SCI_ZOOMOUT: u32 = 2334;
pub const SCI_SETZOOM: u32 = 2373;
pub const SCI_GETZOOM: u32 = 2374;

/// `SCI_SETREADONLY(readOnly)` — set the read-only flag on the
/// currently-bound Scintilla document. Non-zero blocks all
/// user-initiated edits (typing, delete, paste); scripted edits
/// via `SCI_REPLACETARGET` / `SCI_INSERTTEXT` still work. Used by
/// the Document Map's miniature view — the user browses the map
/// but can't accidentally edit through it.
pub const SCI_SETREADONLY: u32 = 2171;
/// `SCI_SETCARETSTYLE(style)` — style codes are the `CARETSTYLE_*`
/// constants below. The Document Map view uses `CARETSTYLE_INVISIBLE`
/// so no blinking caret appears in the read-only overview.
pub const SCI_SETCARETSTYLE: u32 = 2512;
/// Hide the caret entirely. Pairs with [`SCI_SETREADONLY`] on
/// non-editable views like the Document Map.
pub const CARETSTYLE_INVISIBLE: usize = 0;
/// `SCI_SETHSCROLLBAR(visible)` — non-zero shows the horizontal
/// scrollbar. The Document Map view hides it (0) so the miniature
/// view is chrome-free.
pub const SCI_SETHSCROLLBAR: u32 = 2130;
/// `SCI_SETVSCROLLBAR(visible)` — same shape as
/// [`SCI_SETHSCROLLBAR`] for the vertical scrollbar.
pub const SCI_SETVSCROLLBAR: u32 = 2280;

// -----------------------------------------------------------------
// Indicators. Scintilla supports 32 independent indicator styles per
// document — the Document Map's viewport highlight uses one of them
// (`INDIC_FULLBOX` at slot [`DOCMAP_VIEWPORT_INDICATOR`]) to paint
// the orange rectangle over the range visible in the main editor.
//
// Indicator numbers 0-7 are reserved by Scintilla and Lexilla for
// built-in features (searchresult, diagnostic markers, container-
// managed indicators from lexers, …); user-defined indicators live
// in 8..=31.
// -----------------------------------------------------------------

/// `SCI_INDICSETSTYLE(indicator, style)` — set the visual style of
/// an indicator. The style codes are the `INDIC_*` constants;
/// [`INDIC_STRAIGHTBOX`], [`INDIC_FULLBOX`], and [`INDIC_HIDDEN`]
/// are exported here.
pub const SCI_INDICSETSTYLE: u32 = 2080;
/// `SCI_INDICSETFORE(indicator, colour)` — set the foreground
/// colour of an indicator. `colour` is a Scintilla `sptr_t`
/// encoding a `COLORREF` (0x00BBGGRR — same byte order as
/// Win32's `RGB` macro).
pub const SCI_INDICSETFORE: u32 = 2082;
/// `SCI_INDICSETUNDER(indicator, under)` — non-zero draws the
/// indicator UNDER the text (so text stays legible on top of a
/// filled box). Zero paints on top; for the filled-box styles
/// (`INDIC_STRAIGHTBOX` / `INDIC_FULLBOX` / `INDIC_ROUNDBOX`)
/// that means the fill alpha-blends against the text glyphs,
/// tinting them toward the fill colour.
pub const SCI_INDICSETUNDER: u32 = 2510;
/// `SCI_INDICSETALPHA(indicator, alpha)` — 0..=255 alpha of the
/// fill for filled-box indicators (`INDIC_ROUNDBOX`,
/// `INDIC_STRAIGHTBOX`, `INDIC_FULLBOX`). 0 = fully transparent,
/// 255 = fully opaque.
pub const SCI_INDICSETALPHA: u32 = 2523;
/// `SCI_INDICSETOUTLINEALPHA(indicator, alpha)` — same shape as
/// [`SCI_INDICSETALPHA`] but for the box outline. Paired with
/// [`SCI_INDICSETALPHA`] so a filled box can have a translucent
/// interior and a solid outline (the "cursor rectangle" look on
/// N++'s Document Map).
pub const SCI_INDICSETOUTLINEALPHA: u32 = 2558;
/// `SCI_SETINDICATORCURRENT(indicator)` — select which of the 32
/// indicator slots subsequent
/// [`SCI_INDICATORFILLRANGE`]/[`SCI_INDICATORCLEARRANGE`] calls
/// operate on. Modal — stays selected until changed. Applies per
/// Scintilla view.
pub const SCI_SETINDICATORCURRENT: u32 = 2500;
/// `SCI_INDICATORFILLRANGE(position, fillLength)` — set the
/// currently-selected indicator on `fillLength` bytes starting at
/// `position`. Additive: a second call over the same range doesn't
/// duplicate paint, it just re-asserts the bit.
pub const SCI_INDICATORFILLRANGE: u32 = 2504;
/// `SCI_INDICATORCLEARRANGE(position, clearLength)` — clear the
/// currently-selected indicator over the range. Used to erase the
/// previous viewport highlight before painting the new one.
pub const SCI_INDICATORCLEARRANGE: u32 = 2505;

/// `INDIC_STRAIGHTBOX = 8` — filled rectangle with a solid
/// outline. Verified against `vendor/scintilla/src/Indicator.cxx`
/// and `EditView.cxx`: the box's horizontal extent is derived
/// from the range's actual glyph x-positions (via
/// `ll->XInLine`), so a range that ends mid-line paints only up
/// to where the text ends; blank lines inside the range paint
/// no fill at all. Stacked lines carry a 1-px vertical seam
/// between them because `Indicator.cxx` applies `rcBox.top +=
/// 1` for this style.
pub const INDIC_STRAIGHTBOX: usize = 8;
/// `INDIC_FULLBOX = 16` — same glyph-derived horizontal-extent
/// model as [`INDIC_STRAIGHTBOX`] (both use `XInLine`, so blank
/// lines still don't paint and end-of-text still marks the
/// right edge). The only difference is the 1-px vertical
/// `rcBox.top += 1` inset is dropped, so consecutive filled
/// lines stack without a horizontal seam between them — reads
/// as one continuous vertical band across a multi-line range.
/// That's the property the Document Map's viewport highlight
/// wants (Notepad++'s map paints the "you are here" region as
/// a single band, not a stack of 1-line-tall rectangles).
pub const INDIC_FULLBOX: usize = 16;
/// `INDIC_HIDDEN = 5` — indicator is not painted at all. The
/// Document Map assigns this style to slot
/// [`INDIC_STRAIGHTBOX`]'s number on the **main** editor so the
/// shared-document indicator range (written by the map view for
/// its own highlight) doesn't bleed through as a rendering
/// artefact on the main editor. Indicator STYLE lives on
/// `ViewStyle` (per-view); indicator RANGE lives on `Document`
/// (per-doc, shared). Split the two views' styles accordingly.
pub const INDIC_HIDDEN: usize = 5;

/// `SCI_DOCLINEFROMVISIBLE(visibleLine)` — convert a
/// visible-line index (post-folding / post-wrapping, as returned
/// by [`SCI_GETFIRSTVISIBLELINE`]) into a document-line index
/// (as consumed by [`SCI_POSITIONFROMLINE`]). Identity when no
/// folding is active; catches the case where a scrolled-to
/// visible line sits at a different document line because
/// intervening lines are folded.
pub const SCI_DOCLINEFROMVISIBLE: u32 = 2221;

/// `SCI_VISIBLEFROMDOCLINE(docLine)` — inverse of
/// [`SCI_DOCLINEFROMVISIBLE`]. Converts a document-line index
/// into the visible-line index for the same content. Identity
/// when no folding is active; used by the Document Map's
/// drag-to-scroll to translate a clicked doc line into the
/// visible-line coordinate that [`SCI_LINESCROLL`] operates on.
pub const SCI_VISIBLEFROMDOCLINE: u32 = 2220;

/// `SCI_POSITIONFROMPOINTCLOSE(x, y)` — return the closest byte
/// position to the given client-coord point, or `-1` if the
/// point is not inside a text region (e.g. on the margin, or
/// past the last line). Used by the Document Map's
/// drag-to-scroll: hit-test the click, convert the position to
/// a line, scroll the main editor to that line.
pub const SCI_POSITIONFROMPOINTCLOSE: u32 = 2023;

/// `SCI_INDICSETSTROKEWIDTH(indicator, hundredths)` — width of
/// the indicator's outline stroke, in HUNDREDTHS of a pixel
/// (so `100` = 1 px, `200` = 2 px). The Document Map's viewport
/// highlight uses a thick outline as the primary "you are here"
/// cue, with the fill alpha at 0 so the miniature text under
/// the highlight isn't tinted by an alpha-blended overlay.
///
/// **Renderer caveat.** Only Scintilla's DirectWrite (D2D)
/// surface honours this value — the GDI `Surface` implementation
/// hardcodes a 1 px border regardless (see `SurfaceGDI::AlphaRectangle`
/// in `vendor/scintilla/win32/PlatWin.cxx`). The map view opts
/// into D2D via [`SCI_SETTECHNOLOGY`] +
/// [`SC_TECHNOLOGY_DIRECTWRITE`] to get the thicker outline.
pub const SCI_INDICSETSTROKEWIDTH: u32 = 2751;

/// `SCI_SETTECHNOLOGY(tech)` — pick the low-level rendering
/// surface. Default is GDI (`SC_TECHNOLOGY_DEFAULT`); Direct2D /
/// DirectWrite variants unlock the `AlphaRectangle` stroke-width
/// path plus generally smoother glyph rendering.
pub const SCI_SETTECHNOLOGY: u32 = 2630;
/// GDI-based rendering (default). `AlphaRectangle` outlines are
/// hardcoded at 1 px here.
pub const SC_TECHNOLOGY_DEFAULT: usize = 0;
/// Direct2D + DirectWrite rendering. Honours
/// [`SCI_INDICSETSTROKEWIDTH`] and generally provides smoother
/// glyph rendering.
pub const SC_TECHNOLOGY_DIRECTWRITE: usize = 1;

/// `SCI_POINTYFROMPOSITION(0, pos)` — return the Y coordinate
/// (in the view's client area) of the given byte position.
/// Used by the Document Map's custom-drawn viewport highlight
/// to translate the main editor's visible-line endpoints into
/// map-view pixels for the overlay rectangle.
pub const SCI_POINTYFROMPOSITION: u32 = 2165;
/// `SCI_TEXTHEIGHT(line)` — pixel height of the given line
/// (post-zoom). The Document Map's overlay needs this to extend
/// the highlight rectangle down through the last visible line,
/// since [`SCI_POINTYFROMPOSITION`] gives the TOP of the line
/// only.
pub const SCI_TEXTHEIGHT: u32 = 2279;

// Horizontal-scroll width control. Scintilla defaults `scrollWidth`
// to 2000 px and never auto-shrinks, which produces the visible
// "scroll past the end of any line into empty space" behaviour.
// Setting `SCI_SETSCROLLWIDTHTRACKING(1)` makes Scintilla track the
// actual longest visible line and update `scrollWidth` accordingly,
// so the horizontal scrollbar appears only when content overflows
// and stops at the real end of the longest line. Tracking only
// grows `scrollWidth` (high-water-mark behaviour); to make it
// shrink when long lines are deleted, the host explicitly sets
// `SCI_SETSCROLLWIDTH(1)` on every text-modifying SCN_MODIFIED so
// Scintilla resets `lineWidthMaxSeen` and recomputes from the
// current visible content.
pub const SCI_SETSCROLLWIDTH: u32 = 2274;
pub const SCI_SETSCROLLWIDTHTRACKING: u32 = 2516;

// Search & replace — Phase 4 m3. Two parallel APIs:
//   1. Anchor-based: SCI_SEARCHANCHOR + SCI_SEARCHNEXT/PREV walks
//      the buffer relative to the current selection. Matches the
//      caret to the found text on a hit; returns -1 on miss. The
//      simplest API for "Find / Find Next" with a single query.
//   2. Target-range: SCI_SETTARGETRANGE + SCI_SEARCHINTARGET +
//      SCI_REPLACETARGET drive a stateful "search window" that
//      Replace All iterates without touching the user's selection.
//      Required for Replace All semantics; SCI_SEARCHNEXT can't
//      replace because it leaves the match selected (the next
//      replace would clobber the user's new selection).
pub const SCI_SETSEARCHFLAGS: u32 = 2198;
pub const SCI_SEARCHANCHOR: u32 = 2366;
pub const SCI_SEARCHNEXT: u32 = 2367;
pub const SCI_SEARCHPREV: u32 = 2368;
pub const SCI_SETTARGETSTART: u32 = 2190;
pub const SCI_SETTARGETEND: u32 = 2192;
pub const SCI_SETTARGETRANGE: u32 = 2686;
pub const SCI_GETTARGETSTART: u32 = 2191;
pub const SCI_GETTARGETEND: u32 = 2193;
pub const SCI_SEARCHINTARGET: u32 = 2197;
pub const SCI_REPLACETARGET: u32 = 2194;
/// `SCI_REPLACETARGETRE(length, text)` — like [`SCI_REPLACETARGET`] but
/// expands regex group references in `text` against the most recent
/// regex match in the target. With `SCFIND_CXX11REGEX` the reference
/// syntax is `$1`..`$9` (`std::regex`'s `format` grammar); the older
/// POSIX engine uses `\1`. Only meaningful immediately after a regex
/// `SCI_SEARCHINTARGET`.
pub const SCI_REPLACETARGETRE: u32 = 2195;

// SCFIND_* search flag bits, OR'd into the wparam of
// SCI_SETSEARCHFLAGS. The numeric layout is the public ABI plugins
// use too — don't reshuffle.
pub const SCFIND_NONE: u32 = 0x0;
pub const SCFIND_WHOLEWORD: u32 = 0x2;
pub const SCFIND_MATCHCASE: u32 = 0x4;
pub const SCFIND_WORDSTART: u32 = 0x0010_0000;
pub const SCFIND_REGEXP: u32 = 0x0020_0000;
pub const SCFIND_CXX11REGEX: u32 = 0x0080_0000;

// Undo grouping. Wrap a batch of edits (e.g. Replace All) between
// `SCI_BEGINUNDOACTION` and `SCI_ENDUNDOACTION` and the user can
// Ctrl+Z the whole batch as one step rather than one undo per
// individual edit.
pub const SCI_BEGINUNDOACTION: u32 = 2078;
pub const SCI_ENDUNDOACTION: u32 = 2079;

// Notification codes delivered via WM_NOTIFY's NMHDR.code. Each
// `SCN_*` is paired with the SCNotification fields the Scintilla
// docs document for that code.
//
// Note: `SCN_MODIFIED` (notification, sent *from* Scintilla) and
// `SCI_GETCURRENTPOS` (message, sent *to* Scintilla) are both
// numerically `2008`. Verified against the upstream
// `vendor/scintilla/include/Scintilla.h`. The collision is benign
// because the two value spaces are disjoint at the call site —
// `SCN_*` is only ever read from `NMHDR.code` in WM_NOTIFY, and
// `SCI_*` is only ever written as the `msg` argument of
// `EditorHandle::send`. A future refactor that ever crosses those
// channels would need to disambiguate by source HWND first.
pub const SCN_MODIFIED: u32 = 2008;
/// Scintilla notification fired whenever the caret moves, the
/// selection changes, or any other UI-relevant state shifts. The
/// status bar's cursor / column / pos slots refresh on each
/// `SCN_UPDATEUI` so they track the live caret without needing a
/// separate timer.
pub const SCN_UPDATEUI: u32 = 2007;
/// `struct Sci_NotifyHeader` from `Scintilla.h` — the first member of
/// every `SCNotification`, and the only part the perf instrumentation
/// needs.
///
/// Layout is ABI-critical and matches the C declaration exactly:
/// `void *hwndFrom; uptr_t idFrom; unsigned int code;`. The header
/// comment calls it "compatible with Windows NMHDR", which is why
/// `ui_win32` can read the same bytes through the `windows` crate's
/// `NMHDR`.
///
/// Declared here rather than in `plugin-host` because that crate's
/// `ffi` module is `#![cfg(target_os = "windows")]` — it exists to
/// mirror the plugin ABI — whereas this is a Scintilla type both
/// backends need. GTK reaches it by pulling the boxed `SCNotification`
/// out of the `sci-notify` signal's third `GValue` with
/// `g_value_get_boxed`; the payload is the same struct Win32 receives
/// through `WM_NOTIFY`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Sci_NotifyHeader {
    pub hwnd_from: *mut c_void,
    pub id_from: uptr_t,
    pub code: u32,
}

/// Notification fired after Scintilla has finished painting. The
/// closest thing the Win32 backend has to GTK's `draw` signal, and so
/// what `--perf` uses on that platform to close off a keystroke→redraw
/// measurement (DESIGN.md §8). Carries no payload beyond the standard
/// `SCNotification.nmhdr`.
pub const SCN_PAINTED: u32 = 2013;
/// Notification fired when the bound document transitions back to
/// the save-point state — typically because `SCI_SETSAVEPOINT` was
/// called (after a successful save) or the user undid every edit
/// since the last save. Carries no payload beyond the standard
/// `SCNotification.nmhdr`. Pair: [`SCN_SAVEPOINTLEFT`].
pub const SCN_SAVEPOINTREACHED: u32 = 2002;
/// Notification fired the moment the bound document leaves the
/// save-point state — i.e. on the first user edit after a save (or
/// after document creation, if no save has happened yet). Pair:
/// [`SCN_SAVEPOINTREACHED`]. Together these two are the canonical
/// notifications for tracking "buffer has unsaved changes" without
/// polling `SCI_GETMODIFY` on every keystroke.
pub const SCN_SAVEPOINTLEFT: u32 = 2003;

// `SCNotification.modificationType` flags for SCN_MODIFIED. The
// host filters on the text-changing flags (insert / delete) for
// `scrollWidth` recompute; the rest (style change, fold-level
// change, etc.) don't affect line widths.
pub const SC_MOD_INSERTTEXT: i32 = 0x1;
pub const SC_MOD_DELETETEXT: i32 = 0x2;

// `SCNotification.updated` flags for SCN_UPDATEUI. Used to filter
// the broad-spectrum UPDATEUI firehose down to the events the host
// actually cares about — `SC_UPDATE_V_SCROLL` is the one signalling
// the visible line range moved (so the line-number margin's
// visible-window populate needs to refresh). The full flag set is
// listed for reference even if not all of them have a hook today;
// the values are public Scintilla ABI and must match the upstream
// header.
pub const SC_UPDATE_CONTENT: i32 = 0x1;
pub const SC_UPDATE_SELECTION: i32 = 0x2;
pub const SC_UPDATE_V_SCROLL: i32 = 0x4;
pub const SC_UPDATE_H_SCROLL: i32 = 0x8;

// History
pub const SCI_UNDO: u32 = 2176;
pub const SCI_REDO: u32 = 2011;
pub const SCI_CANUNDO: u32 = 2174;
pub const SCI_CANREDO: u32 = 2016;
pub const SCI_EMPTYUNDOBUFFER: u32 = 2175;
/// Add a plugin-owned container action to the undo stack. `wparam`
/// is the opaque token the plugin receives back through
/// `SCN_MODIFIED` with `SC_MOD_CONTAINER` when the action is
/// undone/redone; `lparam` carries the [`UNDO_MAY_COALESCE`] flag
/// (or 0 to force a standalone action). Adding a container action
/// moves the current undo position past the last save-point, so
/// `SCI_GETMODIFY` starts reporting the document as dirty — that's
/// how `NPPM_MAKECURRENTBUFFERDIRTY` toggles the modified state
/// without touching text.
pub const SCI_ADDUNDOACTION: u32 = 2560;
/// `SCI_ADDUNDOACTION` flag: this container action may coalesce
/// with an adjacent one on the undo stack (e.g. two consecutive
/// same-token actions collapse to one undo step). `0` forces
/// a standalone action.
pub const UNDO_MAY_COALESCE: i32 = 1;

// Caret / cursor position
pub const SCI_GETCURRENTPOS: u32 = 2008;
pub const SCI_GOTOPOS: u32 = 2025;

// Modified state — Scintilla tracks "save point" internally; calling
// SCI_SETSAVEPOINT after a successful save resets the modified flag so
// the title bar doesn't keep its asterisk.
pub const SCI_SETSAVEPOINT: u32 = 2014;
pub const SCI_GETMODIFY: u32 = 2159;

// Selection
pub const SCI_SELECTALL: u32 = 2013;
pub const SCI_GETSELECTIONSTART: u32 = 2143;
pub const SCI_GETSELECTIONEND: u32 = 2145;
pub const SCI_SETSELECTIONSTART: u32 = 2142;
pub const SCI_SETSELECTIONEND: u32 = 2144;
/// Copy the current selection's text into the caller-supplied
/// buffer (lparam = char* out). Returns the byte length written
/// (excluding the trailing NUL Scintilla adds).
pub const SCI_GETSELTEXT: u32 = 2161;
/// Collapse the selection to a single point — wparam = caret pos.
/// Used by the Find dialog to advance past the previous match
/// before re-anchoring (Scintilla's `SCI_SEARCHANCHOR` snaps to
/// `SelectionStart`, so without collapsing forward a Find Next
/// click would re-find the same hit on every press).
pub const SCI_SETEMPTYSELECTION: u32 = 2556;
/// Set selection: `wparam = anchor`, `lparam = caret`. Both are
/// byte positions; the selection runs from `min` to `max` of the
/// pair. Scrolls the caret into view as a side effect, so this
/// suffices for "open file at match" navigation without a
/// follow-up `SCI_SCROLLCARET`.
pub const SCI_SETSEL: u32 = 2160;
/// Selection anchor — the "other" end of the selection (`SCI_GETCURRENTPOS`
/// is the caret end). For a collapsed selection the two are equal.
/// Snapshotted alongside the caret position when swapping Scintilla
/// document pointers via `SCI_SETDOCPOINTER`, so the user's
/// pre-swap selection state can be restored on the swap-back.
pub const SCI_GETANCHOR: u32 = 2009;
/// Horizontal scroll offset in pixels — paired with
/// `SCI_GETFIRSTVISIBLELINE` to fully snapshot the view's scroll
/// position around a doc-pointer swap.
pub const SCI_GETXOFFSET: u32 = 2398;
pub const SCI_SETXOFFSET: u32 = 2397;
/// Wipe every line's margin text in one call. Used when replacing
/// the entire buffer (e.g. `SCI_SETTEXT` during session restore)
/// so per-line annotations from the doc's previous state can't
/// leak through onto the new content.
pub const SCI_MARGINTEXTCLEARALL: u32 = 2536;
/// Scroll the view so the caret is visible. `SCI_SEARCHNEXT/PREV`
/// move the selection but don't bring it into view; the Find
/// dialog issues this after every successful hit.
pub const SCI_SCROLLCARET: u32 = 2169;
/// First currently visible (visual) line — top of the viewport.
pub const SCI_GETFIRSTVISIBLELINE: u32 = 2152;
/// Number of lines that currently fit in the viewport.
pub const SCI_LINESONSCREEN: u32 = 2370;
/// Scroll the view by `(columns, lines)` — wparam=columns,
/// lparam=lines. Used by the Find dialog to centre an
/// out-of-view match without disturbing matches already on
/// screen.
pub const SCI_LINESCROLL: u32 = 2168;
/// Position one character after `pos` (wparam=pos). Honours
/// multi-byte UTF-8 boundaries — using `pos + 1` to advance past
/// a zero-width regex match would land mid-codepoint and skip
/// the next character.
pub const SCI_POSITIONAFTER: u32 = 2418;

// Document handles — Scintilla supports multiple documents attached to
// one view via `SCI_SETDOCPOINTER`. Code++ uses this for multi-tab in
// Phase 3 milestone 6: each tab owns a Scintilla document, and tab
// switch is one `SCI_SETDOCPOINTER` call to repoint the single
// Scintilla view at the active tab's document. Documents are
// reference-counted; create with `SCI_CREATEDOCUMENT`, retain with
// `SCI_ADDREFDOCUMENT`, release with `SCI_RELEASEDOCUMENT`.
pub const SCI_GETDOCPOINTER: u32 = 2357;
pub const SCI_SETDOCPOINTER: u32 = 2358;
pub const SCI_CREATEDOCUMENT: u32 = 2375;
pub const SCI_ADDREFDOCUMENT: u32 = 2376;
pub const SCI_RELEASEDOCUMENT: u32 = 2377;

/// Default document-creation flag. Pass as the **`lparam`** of
/// `SCI_CREATEDOCUMENT(wparam = bytes_hint, lparam = options)`.
/// `0` is the right value for a plain text document; the other
/// `SC_DOCUMENTOPTION_*` values (styles-none, text-large) cover
/// rare cases and aren't yet exposed here.
pub const SC_DOCUMENTOPTION_DEFAULT: isize = 0;

// Lexer attachment — Phase 4. `SCI_SETILEXER(0, ilexer_ptr)` attaches
// the `ILexer5*` returned by Lexilla's `CreateLexer` to the Scintilla
// view. Scintilla takes ownership of the pointer and releases it when
// the lexer is replaced or the document is destroyed.
pub const SCI_SETILEXER: u32 = 4033;
pub const SCI_GETLEXER: u32 = 4002;
/// Force the lexer to (re-)style a byte range. `wparam = start`,
/// `lparam = end` (signed; `-1` means "end of document"). Used
/// after a mid-buffer lexer change so existing text picks up the
/// new lexer's classification — Scintilla doesn't auto-restyle
/// on `SCI_SETILEXER`, only on edit/scroll, so without this call
/// the user has to scroll or type before any new highlighting
/// fires. Causes a redraw as a side effect.
pub const SCI_COLOURISE: u32 = 4003;
/// Wide-form `SCI_GETLEXERLANGUAGE` — out-writes the lexer's name
/// (e.g. `"cpp"`) into the caller's `char*` buffer.
pub const SCI_GETLEXERLANGUAGE: u32 = 4012;

// Per-lexer keyword classes. `SCI_SETKEYWORDS(set_index, words_ptr)`
// installs a space-separated list of keywords for one of the lexer's
// numbered keyword classes (LexCPP defines 5; LexRust defines 7; the
// upper bound is 9 across all lexers in Lexilla 5.x). Without these,
// the lexer recognises tokens but classifies every word as
// SCE_C_IDENTIFIER / SCE_RUST_IDENTIFIER / etc., so nothing renders
// as a keyword.
pub const SCI_SETKEYWORDS: u32 = 4005;

// Style colour controls — set per style-index. Phase 4 m1 uses the
// SetFore/SetBack pair to install a minimal default theme so the
// demo gate ("open a .cpp, see colours") is visible.
/// Set the buffer's codepage for byte-to-character mapping.
/// The only value Code++ uses is `SC_CP_UTF8` — Scintilla treats
/// the buffer as UTF-8, which lets the lexer / display / search
/// machinery handle multi-byte characters correctly. Set on every
/// Scintilla view at creation time, including plugin-owned ones
/// surfaced via `NPPM_CREATESCINTILLAHANDLE`.
pub const SCI_SETCODEPAGE: u32 = 2037;
/// `SCI_SETCODEPAGE` value selecting UTF-8. Numeric value 65001
/// (the same Win32 codepage id Microsoft assigns to UTF-8).
pub const SC_CP_UTF8: u32 = 65001;
pub const SCI_STYLESETFORE: u32 = 2051;
pub const SCI_STYLESETBACK: u32 = 2052;
pub const SCI_STYLESETBOLD: u32 = 2053;
pub const SCI_STYLESETITALIC: u32 = 2054;
/// Set the font point size for a style. `wparam = style index`,
/// `lparam = point size (int)`. Phase 5 may add the fractional
/// variant `SCI_STYLESETSIZEFRACTIONAL` (2061) for sub-point
/// sizing; for now whole-point sizes are fine.
pub const SCI_STYLESETSIZE: u32 = 2055;
/// Set the typeface name for a style. `wparam = style index`,
/// `lparam = const char* (UTF-8)`. Scintilla copies the string
/// internally; the caller can drop the buffer immediately after.
pub const SCI_STYLESETFONT: u32 = 2056;
/// Toggle underline on a style. `wparam = style index`, `lparam =
/// 1 / 0`.
pub const SCI_STYLESETUNDERLINE: u32 = 2059;
/// Toggle caret-line background highlighting. `wparam` is a BOOL
/// (0/1). When enabled, Scintilla paints the line containing the
/// caret with the colour set via `SCI_SETCARETLINEBACK`. The
/// setting lives on the view (not on a style index), so it
/// survives `SCI_STYLECLEARALL` — set it once at editor creation.
pub const SCI_SETCARETLINEVISIBLE: u32 = 2096;
/// Set the background colour Scintilla uses when caret-line
/// highlighting is enabled (see `SCI_SETCARETLINEVISIBLE`).
/// `wparam` is a `COLORREF` (`0x00BBGGRR`) — same encoding as
/// `SCI_STYLESETBACK`.
pub const SCI_SETCARETLINEBACK: u32 = 2098;
/// Read the foreground colour for a Scintilla style. Returns the
/// colour in the same `0x00BBGGRR` Win32 `COLORREF` layout
/// `STYLESETFORE` writes — the bit pattern is symmetric, so a
/// plugin that calls `STYLEGETFORE(STYLE_DEFAULT)` reads back the
/// editor's default text colour without conversion. Drives the
/// host-side `NPPM_GETEDITORDEFAULTFOREGROUNDCOLOR` query.
pub const SCI_STYLEGETFORE: u32 = 2481;
/// Read the background colour for a Scintilla style — peer of
/// `SCI_STYLESETBACK`. Same `COLORREF` return layout as
/// `SCI_STYLEGETFORE`. Drives `NPPM_GETEDITORDEFAULTBACKGROUNDCOLOR`.
pub const SCI_STYLEGETBACK: u32 = 2482;
/// Set the editor's font-rendering quality. Accepts one of the
/// `SC_EFF_QUALITY_*` constants below. Drives the host-side
/// `NPPM_SETSMOOTHFONT` toggle (Code++'s impl flips between
/// `LCD_OPTIMIZED` and `NON_ANTIALIASED` based on the BOOL the
/// plugin supplied).
pub const SCI_SETFONTQUALITY: u32 = 2611;
/// Non-antialiased rendering. Code++'s `NPPM_SETSMOOTHFONT(FALSE)`
/// path uses this so a plugin that "turns smoothing off" gets an
/// observable on-screen change.
pub const SC_EFF_QUALITY_NON_ANTIALIASED: u32 = 1;
/// `ClearType` / LCD-optimised rendering — the modern Windows
/// default and Code++'s `NPPM_SETSMOOTHFONT(TRUE)` choice.
pub const SC_EFF_QUALITY_LCD_OPTIMIZED: u32 = 3;
/// Apply `STYLE_DEFAULT` to all other styles. Useful as the first call
/// after switching lexers so the previous lexer's per-style colours
/// don't bleed through.
///
/// Note: this also clobbers the predefined styles in the 32–39 range
/// (`STYLE_DEFAULT`, `STYLE_LINENUMBER`, etc.) — anything outside
/// `STYLE_DEFAULT` itself must be re-applied after this message.
pub const SCI_STYLECLEARALL: u32 = 2050;
/// `STYLE_DEFAULT = 32` — the style index Scintilla uses as the
/// fallback for any text not classified by a lexer. Setting its
/// fore/back/font here is the way to set the editor's "default"
/// appearance.
pub const STYLE_DEFAULT: usize = 32;
/// `STYLE_LINENUMBER = 33` — the style index used to render line
/// numbers, both in Scintilla's built-in `SC_MARGIN_NUMBER` and in
/// `SC_MARGIN_TEXT` margins whose per-line style is set to this
/// index via `SCI_MARGINSETSTYLE`. Setting its fore/back is how the
/// line-number bar gets its colour scheme. `SCI_STYLECLEARALL`
/// resets this back to `STYLE_DEFAULT`, so any custom colours must
/// be re-applied after the clear.
pub const STYLE_LINENUMBER: usize = 33;

// Margins. Scintilla supports up to `SC_MAX_MARGIN + 1` margins (5
// by default), each addressed by a zero-based **index** (the
// `wparam` of `SCI_SETMARGINTYPEN` / `SCI_SETMARGINWIDTHN`) and
// configured with a **type constant** (`SC_MARGIN_*`, the `lparam`).
// Two distinct numbering systems — don't conflate them:
//
//   - Index convention used by Code++ and Notepad++: `0` = line
//     numbers, `1` = symbols/bookmarks, `2` = fold markers.
//   - Type constants from `Scintilla.h`: `SC_MARGIN_SYMBOL = 0`,
//     `SC_MARGIN_NUMBER = 1`, `SC_MARGIN_BACK = 2`,
//     `SC_MARGIN_FORE = 3`, `SC_MARGIN_TEXT = 4`, etc.
//
// `SCI_SETMARGINWIDTHN(margin, pixels)` controls visibility — width
// `0` hides the margin without clearing its other state, so the
// future "show line numbers" toggle is one width-write away.
//
// `SCI_MARGINSETTEXT(line, char_ptr)` writes per-line text into a
// `SC_MARGIN_TEXT` margin and `SCI_MARGINSETSTYLE(line, style)` sets
// its style. Code++ uses these to render line numbers right-aligned
// within a fixed-width column (1-char left pad + `digits(line_count)`
// chars of right-aligned digits) so `1`, `99`, and `100` all share
// the same rightmost column. Scintilla's built-in `SC_MARGIN_NUMBER`
// also right-aligns, but anchors to the bar's full width — short
// numbers float to the far right of the bar — and exposes no
// alignment control. Managing the text per-line ourselves is what
// gives us the column-width handle. Margin text is per-document
// state in Scintilla (stored on `Document`, not the view), so it
// survives `SCI_SETDOCPOINTER` cycles and only needs (re-)populating
// after document creation and after `SCN_MODIFIED` events that
// change line count.
pub const SCI_SETMARGINTYPEN: u32 = 2240;
pub const SCI_SETMARGINWIDTHN: u32 = 2242;
/// `SCI_TEXTWIDTH(int style, const char *text)` — pixel width of `text`
/// rendered in `style`. Used to size the built-in line-number margin
/// (`SC_MARGIN_NUMBER`) to exactly fit the widest line number.
pub const SCI_TEXTWIDTH: u32 = 2276;
/// Set the marker bitmask for margin `n`. Each margin renders a
/// marker only if the marker's id is set in the margin's mask;
/// without this filter every plugin-installed marker would appear
/// in every margin. Code++'s line-number margin keeps its mask at
/// the default (no markers); the change-history margin's mask
/// includes only the `SC_MARKNUM_HISTORY_*` set so plugin markers
/// from a future bookmark/fold-marker margin can't bleed into the
/// edit-indicator strip.
pub const SCI_SETMARGINMASKN: u32 = 2244;
pub const SCI_MARGINSETTEXT: u32 = 2530;
pub const SCI_MARGINSETSTYLE: u32 = 2532;
/// Configure the symbol drawn for marker number `wparam`. Used to
/// pick from `SC_MARK_*` shape constants — `SC_MARK_FULLRECT`
/// fills the margin column for the line, which (in a 4-px-wide
/// dedicated margin) reads as a vertical bar.
pub const SCI_MARKERDEFINE: u32 = 2040;
/// Configure the background colour drawn for marker number
/// `wparam`. `lparam` is a `COLORREF` (`0x00BBGGRR`).
pub const SCI_MARKERSETBACK: u32 = 2042;
/// Enable Scintilla's built-in change-history tracking on the
/// currently bound document. `wparam` is a bitmask of
/// `SC_CHANGE_HISTORY_*` flags. Per-document setting — must be
/// re-applied after every `SCI_CREATEDOCUMENT`. The matching
/// `SC_MARKNUM_HISTORY_*` markers fire automatically once
/// `SC_CHANGE_HISTORY_MARKERS` is set; the host configures their
/// colour + symbol via `SCI_MARKERDEFINE` / `SCI_MARKERSETBACK`.
pub const SCI_SETCHANGEHISTORY: u32 = 2780;
/// `SC_CHANGE_HISTORY_ENABLED = 1` — turn change tracking on. OR
/// with `SC_CHANGE_HISTORY_MARKERS` to surface modifications as
/// margin markers (the path Code++'s tab strip uses) or
/// `SC_CHANGE_HISTORY_INDICATORS` to surface them as inline text
/// indicators (not used today; the inline path collides visually
/// with selection highlighting).
pub const SC_CHANGE_HISTORY_ENABLED: u32 = 1;
/// `SC_CHANGE_HISTORY_MARKERS = 2` — render history transitions
/// via the `SC_MARKNUM_HISTORY_*` marker family. Combined with
/// `SC_CHANGE_HISTORY_ENABLED` to drive Code++'s
/// "modified-line indicator strip" (DESIGN.md §7.4 follow-up).
pub const SC_CHANGE_HISTORY_MARKERS: u32 = 2;
/// `SC_MARK_FULLRECT = 26` — marker symbol that fills the entire
/// margin-column rectangle for the line. In a dedicated narrow
/// margin this reads as a solid vertical bar; in a wider margin
/// it would conflict with line-number text. Pair with a 4-px
/// margin for the change-history strip.
pub const SC_MARK_FULLRECT: u32 = 26;
/// `SC_MARK_EMPTY = 5` — marker symbol that renders nothing.
/// Used to silence the unused members of the change-history
/// marker family (`SC_MARKNUM_HISTORY_REVERTED_TO_ORIGIN`,
/// `SC_MARKNUM_HISTORY_SAVED`, `SC_MARKNUM_HISTORY_REVERTED_TO_MODIFIED`)
/// when the host only wants to surface modified-since-save
/// (`SC_MARKNUM_HISTORY_MODIFIED`). Without this, Scintilla's
/// default symbol + colour for the auto-applied markers would
/// surface as visible artifacts (e.g. coloured line backgrounds
/// for `SC_MARKNUM_HISTORY_SAVED`).
pub const SC_MARK_EMPTY: u32 = 5;
/// `SC_MARKNUM_HISTORY_REVERTED_TO_ORIGIN = 21` — marker auto-set
/// on lines that were edited then undone back to the original
/// state (pre-first-save). Visualised by Code++ as `SC_MARK_EMPTY`
/// (no glyph) so it doesn't compete with the modified-line strip.
pub const SC_MARKNUM_HISTORY_REVERTED_TO_ORIGIN: u32 = 21;
/// `SC_MARKNUM_HISTORY_SAVED = 22` — marker auto-set on lines that
/// were edited and then made part of a save. Without explicit
/// silencing this renders as a green line-background by default
/// in Scintilla 5.5+, which collides badly with light-theme syntax
/// highlighting; Code++ sets its symbol to `SC_MARK_EMPTY`.
pub const SC_MARKNUM_HISTORY_SAVED: u32 = 22;
/// `SC_MARKNUM_HISTORY_MODIFIED = 23` — marker number Scintilla
/// auto-applies to lines that have unsaved modifications relative
/// to the document's last save-point. Cleared on `SCI_SETSAVEPOINT`
/// (which advances the saved baseline). The only history marker
/// Code++'s strip visualises today.
pub const SC_MARKNUM_HISTORY_MODIFIED: u32 = 23;
/// `SC_MARKNUM_HISTORY_REVERTED_TO_MODIFIED = 24` — marker for
/// lines that were modified, saved, then re-edited back to the
/// post-first-save state. Silenced via `SC_MARK_EMPTY` for the
/// same reasons as the other two siblings.
pub const SC_MARKNUM_HISTORY_REVERTED_TO_MODIFIED: u32 = 24;
/// `SC_MARGIN_TEXT = 4` — the *type constant* that, when passed as
/// the `lparam` of `SCI_SETMARGINTYPEN`, makes the addressed margin
/// render per-line text supplied via `SCI_MARGINSETTEXT`, styled by
/// the index supplied via `SCI_MARGINSETSTYLE`. Used by Code++ to
/// render line numbers right-aligned within a fixed-width column —
/// the host formats each line's text with leading spaces so the
/// rightmost digit lands in the same column for every line.
pub const SC_MARGIN_TEXT: u32 = 4;
/// `SC_MARGIN_NUMBER = 1` — type constant for Scintilla's built-in
/// line-number margin, which formats and paints the numbers itself.
/// `ui_win32` deliberately uses [`SC_MARGIN_TEXT`] instead so it can
/// control right-alignment per line; `ui_gtk` uses this simpler
/// built-in until the GTK backend grows the same margin plumbing.
pub const SC_MARGIN_NUMBER: u32 = 1;
/// `SC_MARGIN_SYMBOL = 0` — type constant for a margin that
/// renders only markers (the `SC_MARKNUM_*` family). Code++ uses
/// this for the change-history strip: a 4-px margin whose only
/// content is the `SC_MARKNUM_HISTORY_MODIFIED` marker, painted
/// as a `SC_MARK_FULLRECT` orange bar.
pub const SC_MARGIN_SYMBOL: u32 = 0;

// --- Brace-match highlight ------------------------------------------
//
// Scintilla ships two reserved style slots for the "cursor is at a
// bracket" visual feedback that N++ shows in red:
//   - `STYLE_BRACELIGHT` (34) — the matched-pair colour (both the
//     caret's bracket and its mate render in this style)
//   - `STYLE_BRACEBAD` (35) — the unmatched-bracket colour (only the
//     caret's bracket, drawn on its own)
// The host activates them by sending `SCI_BRACEHIGHLIGHT(a, b)` to
// paint both `a` and `b` in `STYLE_BRACELIGHT`, or
// `SCI_BRACEBADLIGHT(pos)` to paint one `pos` in `STYLE_BRACEBAD`.
// Passing `-1` (`INVALID_POSITION`) for either clears the highlight.
// `SCI_BRACEMATCH(pos, 0)` returns the paired-bracket position or
// `-1` when unpaired. Values from `vendor/scintilla/include/Scintilla.h`.
pub const SCI_BRACEHIGHLIGHT: u32 = 2351;
pub const SCI_BRACEBADLIGHT: u32 = 2352;
pub const SCI_BRACEMATCH: u32 = 2353;
/// Read a single byte from the document at position `wparam`.
/// Returns the raw byte (0 when the position is past the end).
/// Used by the brace-match dispatch on cursor move to detect
/// whether the caret sits at (or immediately after) a bracket.
pub const SCI_GETCHARAT: u32 = 2007;
/// Reserved style slot for the matched bracket + its pair —
/// N++ default paints these in `RGB(0xFF, 0x00, 0x00)` bold.
pub const STYLE_BRACELIGHT: usize = 34;
/// Reserved style slot for a bracket at the caret whose mate is
/// missing — N++ default paints in `RGB(0x80, 0x00, 0x00)` normal.
pub const STYLE_BRACEBAD: usize = 35;
/// Scintilla's sentinel for "no such position" — returned by
/// `SCI_BRACEMATCH` when the paired bracket is missing, and
/// accepted by `SCI_BRACEHIGHLIGHT` / `SCI_BRACEBADLIGHT` as the
/// "clear highlight" argument.
pub const INVALID_POSITION: isize = -1;

// --- Fold margin + fold markers -------------------------------------
//
// The fold column between the line-number margin and the editing
// area, showing +/- toggles for logical regions the lexer's fold
// classifier has grouped. Enabled by:
//   1. `SCI_SETPROPERTY("fold", "1")` — turns the classifier on for
//      the currently-attached Lexilla lexer. Every lexer with a
//      Fold* function (LexCPP, LexPython, LexBash, LexLisp, LexLua,
//      LexTCL, LexNsis, LexProps) responds. LexBatch and LexMakefile
//      lack fold functions — the property is a silent no-op for them.
//   2. Configuring a symbol margin with `SC_MASK_FOLDERS` so
//      Scintilla renders the `SC_MARKNUM_FOLDER*` family in it.
//   3. Defining the marker shapes (BOXPLUS / BOXMINUS + CONNECTED
//      variants — the N++ default "Box" style).
//   4. `SCI_SETAUTOMATICFOLD` to let Scintilla handle click-to-toggle,
//      auto-expand-on-edit, and marker-visibility toggling internally
//      (no `SCN_MARGINCLICK` handler needed for vanilla behaviour).
/// Set a runtime property on the currently-attached lexer.
/// `wparam` = pointer to a NUL-terminated ASCII name, `lparam` =
/// pointer to a NUL-terminated ASCII value. Both strings are copied
/// by Scintilla; caller's buffers only need to live for the duration
/// of the call. Property is preserved across `SCI_SETILEXER`.
pub const SCI_SETPROPERTY: u32 = 4004;
/// Make margin `wparam` respond to mouse clicks — required for
/// click-to-toggle-fold behaviour (whether via
/// `SCI_SETAUTOMATICFOLD` or a manual `SCN_MARGINCLICK` handler).
pub const SCI_SETMARGINSENSITIVEN: u32 = 2246;
/// Set the foreground colour drawn for marker number `wparam`.
/// `lparam` is a `COLORREF` (`0x00BBGGRR`). Complements
/// `SCI_MARKERSETBACK` (already exported) which sets the fill.
pub const SCI_MARKERSETFORE: u32 = 2041;
/// Set the marker's "selected"/highlight background colour — used
/// when `SCI_MARKERENABLEHIGHLIGHT` is on and the containing fold
/// range brackets the caret. N++ paints selected fold markers in
/// red (matching the brace-highlight colour).
pub const SCI_MARKERSETBACKSELECTED: u32 = 2292;
/// Toggle the marker-highlight feature. When on, markers whose
/// fold-range brackets the caret render with their
/// `SCI_MARKERSETBACKSELECTED` colour instead of the base
/// `SCI_MARKERSETBACK`; provides the "hover the caret over a
/// collapsed region and its `+`/`−` glow" feedback.
pub const SCI_MARKERENABLEHIGHLIGHT: u32 = 2293;
/// Set the fold-margin strip's background colour. `wparam` is a
/// boolean: 1 = use the supplied `lparam` COLORREF, 0 = fall back
/// to the theme default. N++ uses this for the light-grey strip
/// under the fold markers.
pub const SCI_SETFOLDMARGINCOLOUR: u32 = 2290;
/// Set the fold-margin strip's highlight colour — drawn instead of
/// `SCI_SETFOLDMARGINCOLOUR` when the mouse is over the margin.
pub const SCI_SETFOLDMARGINHICOLOUR: u32 = 2291;
/// Bit-mask that a margin's `SCI_SETMARGINMASKN` must include to
/// render the `SC_MARKNUM_FOLDER*` family. Covers bits 25..=31.
pub const SC_MASK_FOLDERS: u32 = 0xFE00_0000;
/// Marker number for the "end tail of a middle segment of a
/// contracted fold" — the `└` corner at the bottom of a collapsed
/// nested region. Shape in N++'s BOX style: `SC_MARK_BOXPLUSCONNECTED`.
pub const SC_MARKNUM_FOLDEREND: u32 = 25;
/// Marker for a mid-region open-fold header — the `−` in the middle
/// of an expanded parent's children. N++ shape: `SC_MARK_BOXMINUSCONNECTED`.
pub const SC_MARKNUM_FOLDEROPENMID: u32 = 26;
/// Marker for the `└` at the bottom-mid of a nested expanded region.
/// N++ shape: `SC_MARK_TCORNER`.
pub const SC_MARKNUM_FOLDERMIDTAIL: u32 = 27;
/// Marker for the `└` at the end of a top-level expanded region.
/// N++ shape: `SC_MARK_LCORNER`.
pub const SC_MARKNUM_FOLDERTAIL: u32 = 28;
/// Marker for the `│` continuation line drawn between the header
/// and tail of an expanded fold range. N++ shape: `SC_MARK_VLINE`.
pub const SC_MARKNUM_FOLDERSUB: u32 = 29;
/// Marker for the collapsed-fold header (`+` glyph). N++ shape:
/// `SC_MARK_BOXPLUS`.
pub const SC_MARKNUM_FOLDER: u32 = 30;
/// Marker for the expanded-fold header (`−` glyph). N++ shape:
/// `SC_MARK_BOXMINUS`.
pub const SC_MARKNUM_FOLDEROPEN: u32 = 31;

// Fold-marker shape constants (subset — used by the fold-margin
// wiring; the full set includes ARROW / CIRCLEPLUS / DOTDOTDOT
// etc. per `vendor/scintilla/include/Scintilla.h:132-150`). The
// BOX family is what N++ ships by default.
/// Vertical line — the `│` continuation stroke between fold
/// header and tail. Paired with `SC_MARKNUM_FOLDERSUB`.
pub const SC_MARK_VLINE: u32 = 9;
/// L-corner — the `└` at the bottom of a top-level expanded fold.
/// Paired with `SC_MARKNUM_FOLDERTAIL`.
pub const SC_MARK_LCORNER: u32 = 10;
/// T-corner — the `├` at the bottom-mid of a nested expanded fold.
/// Paired with `SC_MARKNUM_FOLDERMIDTAIL`.
pub const SC_MARK_TCORNER: u32 = 11;
/// Filled square with `+` inside — the "click to expand" glyph on
/// a collapsed fold header. Paired with `SC_MARKNUM_FOLDER`.
pub const SC_MARK_BOXPLUS: u32 = 12;
/// Same as `SC_MARK_BOXPLUS` but with a continuation line drawn
/// through it — used for a collapsed fold nested inside another
/// expanded fold. Paired with `SC_MARKNUM_FOLDEREND`.
pub const SC_MARK_BOXPLUSCONNECTED: u32 = 13;
/// Filled square with `−` inside — the "click to collapse" glyph
/// on an expanded fold header. Paired with `SC_MARKNUM_FOLDEROPEN`.
pub const SC_MARK_BOXMINUS: u32 = 14;
/// Same as `SC_MARK_BOXMINUS` but with a continuation line — the
/// mid-region expanded-fold header. Paired with `SC_MARKNUM_FOLDEROPENMID`.
pub const SC_MARK_BOXMINUSCONNECTED: u32 = 15;

/// Toggle a single line's fold state (`wparam` = line number).
/// Only needed if `SC_AUTOMATICFOLD_CLICK` is not enabled and the
/// host handles `SCN_MARGINCLICK` manually — Code++ uses automatic
/// fold today so this is exported for a future Shift/Ctrl-click
/// extension (fold-all-children semantics N++ layers on top).
pub const SCI_TOGGLEFOLD: u32 = 2231;
/// Enable Scintilla's built-in fold-margin behaviour. `wparam` is
/// a bitmask of `SC_AUTOMATICFOLD_*` flags. Avoids writing a manual
/// `SCN_MARGINCLICK` handler; Shift/Ctrl-click semantics require
/// the manual path.
pub const SCI_SETAUTOMATICFOLD: u32 = 2663;
/// `SC_AUTOMATICFOLD_SHOW = 1` — automatically show markers when a
/// fold header line is encountered by the lexer.
pub const SC_AUTOMATICFOLD_SHOW: u32 = 1;
/// `SC_AUTOMATICFOLD_CLICK = 2` — turn a click in the fold margin
/// into a toggle without host involvement.
pub const SC_AUTOMATICFOLD_CLICK: u32 = 2;
/// `SC_AUTOMATICFOLD_CHANGE = 4` — auto-expand collapsed folds when
/// an edit lands inside them.
pub const SC_AUTOMATICFOLD_CHANGE: u32 = 4;
/// Set the fold-visualisation flags (`wparam` bitmask of
/// `SC_FOLDFLAG_*`). Controls decorations drawn around
/// contracted/expanded fold ranges independently of the marker
/// shapes.
pub const SCI_SETFOLDFLAGS: u32 = 2233;
/// `SC_FOLDFLAG_LINEAFTER_CONTRACTED = 0x10` — draw a horizontal
/// line below a collapsed fold, matching N++'s
/// "you-collapsed-a-region-here" indicator.
pub const SC_FOLDFLAG_LINEAFTER_CONTRACTED: u32 = 0x10;

/// Fired when the user clicks in a margin whose
/// `SCI_SETMARGINSENSITIVEN` is enabled. Used when the host
/// implements manual fold-toggle (Shift/Ctrl-click extensions) —
/// vanilla click-to-toggle is covered by `SC_AUTOMATICFOLD_CLICK`.
pub const SCN_MARGINCLICK: u32 = 2010;

// LexCPP style indices used by the Phase 4 m1 default theme. The
// full set lives in `vendor/lexilla/include/SciLexer.h`; only those
// the theme actually targets are mirrored here.
pub const SCE_C_DEFAULT: usize = 0;
pub const SCE_C_COMMENT: usize = 1;
pub const SCE_C_COMMENTLINE: usize = 2;
pub const SCE_C_COMMENTDOC: usize = 3;
pub const SCE_C_NUMBER: usize = 4;
pub const SCE_C_WORD: usize = 5;
pub const SCE_C_STRING: usize = 6;
pub const SCE_C_CHARACTER: usize = 7;
pub const SCE_C_PREPROCESSOR: usize = 9;
pub const SCE_C_OPERATOR: usize = 10;
pub const SCE_C_COMMENTLINEDOC: usize = 15;
pub const SCE_C_WORD2: usize = 16;

// LexRust style indices.
pub const SCE_RUST_DEFAULT: usize = 0;
pub const SCE_RUST_COMMENTBLOCK: usize = 1;
pub const SCE_RUST_COMMENTLINE: usize = 2;
pub const SCE_RUST_COMMENTBLOCKDOC: usize = 3;
pub const SCE_RUST_COMMENTLINEDOC: usize = 4;
pub const SCE_RUST_NUMBER: usize = 5;
pub const SCE_RUST_WORD: usize = 6;
pub const SCE_RUST_WORD2: usize = 7;
pub const SCE_RUST_STRING: usize = 13;
pub const SCE_RUST_CHARACTER: usize = 15;
pub const SCE_RUST_OPERATOR: usize = 16;
pub const SCE_RUST_IDENTIFIER: usize = 17;
pub const SCE_RUST_LIFETIME: usize = 18;
pub const SCE_RUST_MACRO: usize = 19;

// LexPython style indices. 21 contiguous slots (0..=20) covering
// the Python lexer's full emission set: `#` line comments, `##`
// block comments (separate state, see banner below), decimal /
// hex / oct / bin / underscore-separated number literals, two
// wordlist classes (`SCE_P_WORD` for reserved words, `SCE_P_WORD2`
// for built-in identifiers), single- and double-quoted strings,
// triple-quoted strings (`'''...'''` and `"""..."""`), the four
// f-string variants (`f"..."` / `f'...'` / `f'''...'''` /
// `f"""..."""`), class / def names (post-keyword identifier
// styles, set automatically by a kwLast state machine),
// operators, identifiers, decorators (`@foo` at line start), and
// the opt-in attribute-access style. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 160-180 and
// `vendor/lexilla/lexers/LexPython.cxx` lines 321-325
// (`pythonWordListDesc`), 258-289 (`IsMatchOrCaseIdentifier`),
// 671 + 694 (case-sensitive `keywords.InList` / `keywords2.InList`
// dispatch), 297 (`stringsF = true` default for f-string
// activation), 305-306 (`identifierAttributes` /
// `decoratorAttributes` defaulting to 0).
//
// **Case-sensitive lexer.** Python language semantics: `True`,
// `False`, `None` are spelled with leading capitals; `match` /
// `case` (soft keywords, Python 3.10+) are lowercase. LexPython
// does NO case folding — `keywords.InList(identifier)` matches
// the byte-exact source token against the installed wordlist.
// Wordlists must store source-canonical casing — see the
// `PYTHON_KEYWORDS` doc comment for the `True`/`False`/`None`
// placement rationale (class 0 because Python 3 makes them
// reserved, unlike Python 2 / N++ where they were builtins).
//
// **Two wordlist classes.** `pythonWordListDesc[]` declares two
// slots: `"Keywords"` (class 0) and `"Highlighted identifiers"`
// (class 1). Class 0 hits emit `SCE_P_WORD` (mapped to Keyword
// bold); class 1 hits emit `SCE_P_WORD2` (Keyword2 steel-blue).
// A token in both classes silently demotes to class 0 (Lexilla
// checks class 0 first at line 671) — wordlists must not
// overlap; `PYTHON_KEYWORDS` / `PYTHON_KEYWORDS_2` enforce this
// structurally via the test suite.
//
// **`match` / `case` soft keywords.** Python 3.10+ PEP 634 makes
// these reserved ONLY in pattern-matching position (`match
// value:` / `case 1:`); elsewhere (`match = 1`, `x.match()`)
// they're regular identifiers. LexPython handles disambiguation
// internally via `IsMatchOrCaseIdentifier` (lines 258-289): if
// the source position is not pattern-matching context, the
// wordlist hit is vetoed and the token falls through to
// `SCE_P_IDENTIFIER`. Installing them in class 0 is correct and
// safe — the lexer does the right thing.
//
// **`SCE_P_CLASSNAME` (8) / `SCE_P_DEFNAME` (9) auto-emission.**
// LexPython's kwLast state machine (lines 673-676): when the
// previous wordlist-class-0 hit was `class` or `def`, the next
// identifier token gets reclassified to CLASSNAME / DEFNAME
// instead of plain IDENTIFIER. No wordlist install needed for
// the class / def NAMES themselves — only that `class` and
// `def` are in the class-0 wordlist (they are).
//
// **`SCE_P_DECORATOR` (15) auto-emission.** LexPython line 916:
// `@` at line start (after `IsFirstNonWhitespace` gate)
// transitions into the DECORATOR state, consuming the
// identifier that follows. Mid-expression `@` (matrix-mul
// operator, Python 3.5+) correctly degrades to `SCE_P_OPERATOR`
// — no wordlist install needed.
//
// **`SCE_P_COMMENTBLOCK` (12) — `##` line-prefix comments.**
// Emitted by LexPython.cxx line 914 when `sc.chNext == '#'`
// (`#` followed by `#`). NOT a separate block-comment syntax —
// Python has no `/* */`-style comments. Pre-themed to Comment
// for safety so users following the `##` heading convention in
// some style guides don't see uncoloured text.
//
// **`SCE_P_STRINGEOL` (13) intentionally unmapped.** Joins the
// deferred-Error-slot migration list (Perl ERROR, VB STRINGEOL,
// and 9 others currently at 12 entries after this addition).
// Synthesising an ad-hoc red here creates palette drift that
// the Error-slot migration would have to clean up — better to
// leave unmapped (falls through to STYLE_DEFAULT) and migrate
// the whole cluster together.
//
// **F-string family (16-19) activation.** `stringsF = true` by
// default in LexPython (line 297). Code++ does not override —
// f-strings highlight automatically. The four variants are
// distinguished by quote shape: `f"..."` → 16 FSTRING,
// `f'...'` → 17 FCHARACTER, `f'''...'''` → 18 FTRIPLE,
// `f"""..."""` → 19 FTRIPLEDOUBLE. All four route to String;
// the `{}` interpolation sub-lexer is internal to Lexilla.
//
// **`SCE_P_ATTRIBUTE` (20) opt-in.** Gated by the
// `lexer.python.identifier.attributes` (default 0) and
// `lexer.python.decorator.attributes` (default 0) properties.
// Code++ never calls `SetProperty` to enable these — the state
// NEVER fires under default configuration. Pre-themed to
// Keyword2 anyway for forward-compat: same pattern as CSS
// EXTENDED_PSEUDOCLASS / EXTENDED_PSEUDOELEMENT pre-theming.
// Costs one table row; gains zero-effort activation if the
// property is ever flipped.
//
// **`SCE_P_DEFAULT` (0) and `SCE_P_IDENTIFIER` (11) intentionally
// unmapped.** Universal omission pattern: bare-identifier and
// background-text styles render at STYLE_DEFAULT (the user's
// chosen foreground) — same precedent as `SCE_C_DEFAULT` /
// `SCE_C_IDENTIFIER`, `SCE_PAS_DEFAULT` / `SCE_PAS_IDENTIFIER`,
// `SCE_PL_DEFAULT` / `SCE_PL_IDENTIFIER`.
pub const SCE_P_DEFAULT: usize = 0;
pub const SCE_P_COMMENTLINE: usize = 1;
pub const SCE_P_NUMBER: usize = 2;
pub const SCE_P_STRING: usize = 3;
pub const SCE_P_CHARACTER: usize = 4;
pub const SCE_P_WORD: usize = 5;
pub const SCE_P_TRIPLE: usize = 6;
pub const SCE_P_TRIPLEDOUBLE: usize = 7;
pub const SCE_P_CLASSNAME: usize = 8;
pub const SCE_P_DEFNAME: usize = 9;
pub const SCE_P_OPERATOR: usize = 10;
pub const SCE_P_IDENTIFIER: usize = 11;
pub const SCE_P_COMMENTBLOCK: usize = 12;
pub const SCE_P_STRINGEOL: usize = 13;
pub const SCE_P_WORD2: usize = 14;
pub const SCE_P_DECORATOR: usize = 15;
pub const SCE_P_FSTRING: usize = 16;
pub const SCE_P_FCHARACTER: usize = 17;
pub const SCE_P_FTRIPLE: usize = 18;
pub const SCE_P_FTRIPLEDOUBLE: usize = 19;
pub const SCE_P_ATTRIBUTE: usize = 20;

// LexJSON style indices. 14 contiguous slots (0..=13) for
// JSON, JSON5, and JSON-LD source. Constants mirror
// `SciLexer.h:1882-1895` verbatim. Dispatches SCLEX_JSON
// (= 120, per `SciLexer.h:136`) via a **two-class wordlist**
// declared at `LexJSON.cxx:40-44`
// (`JSONWordListDesc[]`):
//
//     JSONWordListDesc[] = {
//         "JSON Keywords",   // class 0 → SCE_JSON_KEYWORD
//         "JSON-LD Keywords", // class 1 → SCE_JSON_LDKEYWORD
//         nullptr,
//     };
//
// **Case-SENSITIVE matching.** The identifier check at
// `LexJSON.cxx:191-206` (`IsNextWordInList`) uses
// `styler.SafeGetCharAt` byte-exact, NOT lowered. JSON is
// case-sensitive at the spec level (RFC 8259: literal
// spellings are `true` / `false` / `null` only), so wordlist
// tokens use exact case. Same discipline as
// `D_KEYWORDS` / `R_RESERVED` / `COFFEESCRIPT_KEYWORDS`.
//
// **Two opt-in properties gate three states.**
//
//   - `lexer.json.escape.sequence` (default `0`) — when
//     enabled, `\\`-escapes inside a string enter
//     `SCE_JSON_ESCAPESEQUENCE` (5). Entry logic at
//     `:340-344` and `:377-380`. Invalid escapes get
//     re-classified to `SCE_JSON_ERROR` (13).
//   - `lexer.json.allow.comments` (default `0`) — when
//     enabled, `//`-to-EOL enters
//     `SCE_JSON_LINECOMMENT` (6) at `:416-417` and
//     `/*...*/` enters `SCE_JSON_BLOCKCOMMENT` (7) at
//     `:413-415`. Strict RFC 8259 JSON forbids comments;
//     JSON5 and JSONC (JSON with Comments) permit them.
//
// The host enables both properties in `apply_lang` for
// `L_JSON` and `L_JSON5` (see `extra_fold_properties`) so
// all three states are active.
//
// **Property-name detection is lookahead-driven.** A `"..."`
// string entered from `DEFAULT` is re-classified to
// `SCE_JSON_PROPERTYNAME` (4) at `:407-410` if
// `AtPropertyName` at `:171-189` finds the closing quote
// followed by (up-to-50-spaces of whitespace and then) `:`.
// This distinguishes JSON object keys from string values
// visually without a grammar change.
//
// **URI and JSON-LD sub-styles trigger from inside a
// string.** At `:347-353` seven URI-scheme prefixes
// (`https://`, `http://`, `ssh://`, `git://`, `svn://`,
// `ftp://`, `mailto:`) cause the string to switch to
// `SCE_JSON_URI` (9) mid-token. At `:357-361` an `@` at
// string-position that begins an in-list JSON-LD keyword
// switches to `SCE_JSON_LDKEYWORD` (12). Both states
// return to the pre-switch `SCE_JSON_STRING` /
// `SCE_JSON_PROPERTYNAME` at end-of-URI or
// end-of-LD-keyword per `:367-373`. `SCE_JSON_COMPACTIRI`
// (10) is set at `:329-332` when the closing quote of a
// string is reached AND the accumulated char stream
// contained exactly one `:` with every other char in
// `CompactIRI::setCompactIRI` (alpha + `$_-` per `:59`)
// — JSON-LD compact IRI form (`prefix:suffix`).
//
// **`SCE_JSON_ERROR` (13) catches everything the lexer
// can't classify.** At `:455-457` any non-whitespace char
// that doesn't match a state-entry condition transitions to
// ERROR. This includes bare identifiers not in the keyword
// wordlist (e.g., `Infinity` in a strict-JSON file where
// `Infinity` isn't in the wordlist), unterminated escapes,
// and stray punctuation.
//
// Style semantics (paint-loop citations reference
// LexJSON.cxx):
//
//   - SCE_JSON_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_JSON_NUMBER (1) — numeric literal (integer,
//     decimal, scientific, hex).
//   - SCE_JSON_STRING (2) — `"..."` string value.
//   - SCE_JSON_STRINGEOL (3) — unterminated string that
//     hit end-of-line. Reset at `:298-302`.
//   - SCE_JSON_PROPERTYNAME (4) — string that is followed
//     by a `:` (JSON object key). Detected at `:407-410`
//     via `AtPropertyName` lookahead.
//   - SCE_JSON_ESCAPESEQUENCE (5) — `\\x` / `\\uHHHH`
//     etc. escape inside a string. Emitted only when
//     `lexer.json.escape.sequence` = 1.
//   - SCE_JSON_LINECOMMENT (6) — `//`-to-EOL comment.
//     Emitted only when `lexer.json.allow.comments` = 1.
//   - SCE_JSON_BLOCKCOMMENT (7) — `/* ... */` block
//     comment. Emitted only when
//     `lexer.json.allow.comments` = 1.
//   - SCE_JSON_OPERATOR (8) — structural punctuation per
//     `setOperators` at `:211`: `[`, `]`, `{`, `}`, `:`,
//     `,`.
//   - SCE_JSON_URI (9) — URL substring inside a string
//     starting with one of the recognised URI schemes.
//   - SCE_JSON_COMPACTIRI (10) — JSON-LD compact IRI
//     (`prefix:suffix` inside a string), detected on
//     end-quote at `:329-332`.
//   - SCE_JSON_KEYWORD (11) — wordlist class 0 hit
//     (JSON literals: `true` / `false` / `null`;
//     JSON5 adds `Infinity` / `NaN`).
//   - SCE_JSON_LDKEYWORD (12) — wordlist class 1 hit
//     (JSON-LD `@`-prefixed keywords: `@context`, `@id`,
//     `@type`, etc.).
//   - SCE_JSON_ERROR (13) — catch-all for unclassified
//     non-whitespace. Routes to a distinctive "attention"
//     slot on the theme side so parse errors surface
//     visually.
pub const SCE_JSON_DEFAULT: usize = 0;
pub const SCE_JSON_NUMBER: usize = 1;
pub const SCE_JSON_STRING: usize = 2;
pub const SCE_JSON_STRINGEOL: usize = 3;
pub const SCE_JSON_PROPERTYNAME: usize = 4;
pub const SCE_JSON_ESCAPESEQUENCE: usize = 5;
pub const SCE_JSON_LINECOMMENT: usize = 6;
pub const SCE_JSON_BLOCKCOMMENT: usize = 7;
pub const SCE_JSON_OPERATOR: usize = 8;
pub const SCE_JSON_URI: usize = 9;
pub const SCE_JSON_COMPACTIRI: usize = 10;
pub const SCE_JSON_KEYWORD: usize = 11;
pub const SCE_JSON_LDKEYWORD: usize = 12;
pub const SCE_JSON_ERROR: usize = 13;

// LexFortran style indices. 15 contiguous slots (0..=14) for
// Fortran source in both free-form (`.f90` / `.f95` / `.f2k` /
// `.f03` / `.f08` / `.f15`) and fixed-form (`.f` / `.for` /
// `.f77` / `.ftn`) dialects. Constants mirror `SciLexer.h:764-778`
// verbatim. **One `LexFortran.cxx` provides TWO LexerModules**
// (`:723-724`): `lmFortran(SCLEX_FORTRAN = 36, ..., "fortran")`
// for free-form and `lmF77(SCLEX_F77 = 37, ..., "f77")` for
// fixed-form. Both share `ColouriseFortranDoc` with just an
// `isFixFormat` boolean toggling column-oriented parsing at
// `:92-122`. Same SCE_F_* enum, same three-class wordlist
// descriptor at `:696-701` (`FortranWordLists[]`):
//
//     FortranWordLists[] = {
//         "Primary keywords and identifiers", // class 0 → SCE_F_WORD
//         "Intrinsic functions",              // class 1 → SCE_F_WORD2
//         "Extended and user defined functions", // class 2 → SCE_F_WORD3
//         0,
//     };
//
// **Case-INSENSITIVE matching.** The identifier-classification
// cascade at `LexFortran.cxx:167-179` calls
// `sc.GetCurrentLowered(s, sizeof(s))` — the classifier
// lowercases the token before every `keywords.InList(s)` probe.
// Fortran is case-insensitive at the spec level (per every
// Fortran standard from FORTRAN 66 through Fortran 2023):
// `IF`, `if`, `If`, `iF` are all the same token. Wordlist
// tokens must therefore be all-lowercase — an uppercase entry
// would silently never match. Same discipline as
// `POWERSHELL_KEYWORDS` / `COBOL_KEYWORDS_A`, inverted from
// `D_KEYWORDS` / `R_RESERVED` / `COFFEESCRIPT_KEYWORDS`.
//
// **Fixed-form vs free-form column semantics.** In FORTRAN 77
// / fixed-form (`isFixFormat = true`), the paint loop at
// `:92-122` treats columns specially. `toLineStart` here is
// 0-indexed (position from line-start), which maps to
// 1-indexed FORTRAN 77 columns as `toLineStart = col - 1`:
//   - `toLineStart == 0` (col 1) and char is `c` / `C` / `*`
//     → `SCE_F_COMMENT` runs to end-of-line (`:93-101`).
//   - `toLineStart < 5` (cols 1..=5, the FORTRAN 77 label
//     field) → `SCE_F_LABEL` if digit, else `SCE_F_DEFAULT`
//     (`:107-111`).
//   - `toLineStart == 5` (col 6, the continuation field) →
//     `SCE_F_CONTINUATION` if non-space, non-`0`
//     (`:112-119`). Any single character in column 6
//     (1-indexed) is a continuation-line marker.
//   - `toLineStart >= 72` (col 73+) → `SCE_F_COMMENT`
//     (`:104-106`). FORTRAN 77 is column-limited to 72
//     characters; anything past is a comment.
// Free-form (`isFixFormat = false`) drops the column
// restrictions entirely — `!` anywhere on the line starts a
// comment, and `&` at end-of-line is a continuation
// (`:125-150`).
//
// **`.name.` operator syntax — a Fortran signature.**
// `SCE_F_OPERATOR2` (12) is entered at `:244-245` when a `.`
// is followed by an alphabetic character. Fortran's relational
// and logical operators are written `.eq.` / `.ne.` / `.lt.` /
// `.le.` / `.gt.` / `.ge.` / `.and.` / `.or.` / `.not.` /
// `.eqv.` / `.neqv.`, and the boolean literals `.true.` /
// `.false.` follow the same shape. Distinct visual signal
// from single-char punctuation `SCE_F_OPERATOR` (6) — worth a
// separate colour slot to signal "this dot-form is an
// operator, not a decimal fraction".
//
// **Compiler directives.** `SCE_F_PREPROCESSOR` (11) covers
// three families:
//   - `#`-directives at column 0 (`:153-158`) — C-preprocessor
//     lines like `#include`, `#define` when using CPP wrappers.
//   - Vendor directives `!DEC$` (Intel), `!DIR$` (Cray/PGI),
//     `!MS$` (Microsoft) — detected at `:229-232` in free-form.
//   - Fixed-form equivalents `Cdec$` / `*dec$` / `Cdir$` etc.
//     at column 0 (`:94-97`).
//
// **Two string flavours + STRINGEOL.** `SCE_F_STRING1` (3) is
// `'...'` single-quoted; `SCE_F_STRING2` (4) is `"..."`
// double-quoted. Both support Fortran's doubled-quote escape
// convention (`''` inside `'...'` embeds a `'`; `""` inside
// `"..."` embeds a `"`), handled at `:186-191` and `:202-208`.
// `SCE_F_STRINGEOL` (5) is the transient error state when an
// unterminated string hits end-of-line — set at `:193-195` and
// `:199-201`.
//
// **Number literal quirks.** `SCE_F_NUMBER` (2) covers the
// usual decimal / real / exponent forms, plus Fortran's
// `B"..."` / `O"..."` / `Z"..."` binary/octal/hex boson-quoted
// literals at `:240-243` (`.MODIS style`).
//
// Style semantics (paint-loop citations reference
// LexFortran.cxx):
//
//   - SCE_F_DEFAULT (0) — whitespace / unclassified. Framework
//     convention: leave unmapped.
//   - SCE_F_COMMENT (1) — line comment. Free-form: `!` to EOL.
//     Fixed-form: `c` / `C` / `*` in column 0, `!` anywhere, or
//     content past column 72.
//   - SCE_F_NUMBER (2) — numeric literal, including
//     Fortran-specific `B"..."` / `O"..."` / `Z"..."` forms.
//   - SCE_F_STRING1 (3) — `'...'` single-quoted string.
//   - SCE_F_STRING2 (4) — `"..."` double-quoted string.
//   - SCE_F_STRINGEOL (5) — unterminated string that reached
//     end-of-line.
//   - SCE_F_OPERATOR (6) — punctuation per `isoperator()` at
//     `:252-254`.
//   - SCE_F_IDENTIFIER (7) — bare identifier fallback.
//     Framework convention: leave unmapped so plain identifiers
//     paint at STYLE_DEFAULT.
//   - SCE_F_WORD (8) — wordlist class 0 hit (primary
//     Fortran keywords — `if`, `then`, `do`, `subroutine`,
//     `program`, `module`, `function`, etc.).
//   - SCE_F_WORD2 (9) — wordlist class 1 hit (intrinsic
//     functions — `abs`, `sqrt`, `sin`, `cos`, `mod`,
//     `trim`, etc.).
//   - SCE_F_WORD3 (10) — wordlist class 2 hit (extended and
//     user-defined functions — F95+ additions, MPI /
//     OpenMP intrinsics, etc.).
//   - SCE_F_PREPROCESSOR (11) — CPP directives + vendor
//     compiler directives (`!DEC$` / `!DIR$` / `!MS$`).
//   - SCE_F_OPERATOR2 (12) — `.name.` operator syntax
//     (`.eq.`, `.and.`, `.not.`, `.true.`, `.false.`, etc.).
//   - SCE_F_LABEL (13) — statement label. Fixed-form:
//     columns 1..=4 digits. Free-form: leading digits at
//     line start.
//   - SCE_F_CONTINUATION (14) — line-continuation marker.
//     Fixed-form: column 5 non-space/non-0. Free-form: `&`
//     at end-of-line.
pub const SCE_F_DEFAULT: usize = 0;
pub const SCE_F_COMMENT: usize = 1;
pub const SCE_F_NUMBER: usize = 2;
pub const SCE_F_STRING1: usize = 3;
pub const SCE_F_STRING2: usize = 4;
pub const SCE_F_STRINGEOL: usize = 5;
pub const SCE_F_OPERATOR: usize = 6;
pub const SCE_F_IDENTIFIER: usize = 7;
pub const SCE_F_WORD: usize = 8;
pub const SCE_F_WORD2: usize = 9;
pub const SCE_F_WORD3: usize = 10;
pub const SCE_F_PREPROCESSOR: usize = 11;
pub const SCE_F_OPERATOR2: usize = 12;
pub const SCE_F_LABEL: usize = 13;
pub const SCE_F_CONTINUATION: usize = 14;

// LexCsound style indices. 16 contiguous slots (0..=15) for
// Csound orchestra (`.orc`) and score (`.sco`) source, plus
// unified `.csd` files. Constants mirror `SciLexer.h:1296-1311`
// verbatim. Dispatches SCLEX_CSOUND (= 74, per
// `SciLexer.h:90`) via a **three-class wordlist** declared at
// `vendor\lexilla\lexers\LexCsound.cxx:208-213`
// (`csoundWordListDesc[]`):
//
//     csoundWordListDesc[] = {
//         "Opcodes",           // class 0 → SCE_CSOUND_OPCODE
//         "Header Statements", // class 1 → SCE_CSOUND_HEADERSTMT
//         "User keywords",     // class 2 → SCE_CSOUND_USERKEYWORD
//         0,
//     };
//
// **Case-SENSITIVE matching.** The identifier-classification
// cascade at `LexCsound.cxx:90-113` calls
// `sc.GetCurrent(s, sizeof(s))` (byte-exact), NOT
// `GetCurrentLowered`. Csound is case-sensitive per language
// spec — wordlist tokens use exact source spelling. Since
// Csound convention is all-lowercase opcodes, wordlists are
// all-lowercase. Same discipline as `D_KEYWORDS` /
// `R_RESERVED` / `COFFEESCRIPT_KEYWORDS`, inverted from
// `POWERSHELL_KEYWORDS` / `COBOL_KEYWORDS_A` / `FORTRAN_KEYWORDS`.
//
// **Identifier alphabet with permissive starters.**
// `setWordStart` at `:37-40` accepts alnum + `_` + `.` +
// `%` + `@` + `$` + `?`. `setWord` at `:32-35` narrows to
// alnum + `.` + `_` + `?`. The dollar sign (`$`) is for
// macro-invocation forms like `$MACRO`; percent (`%`) and
// at-sign (`@`) are legacy sigils.
//
// **Rate-prefix auto-classification.** LexCsound's identifier
// classifier at `:101-111` has a unique fallback: if the
// token fails all three wordlist probes, the FIRST CHARACTER
// determines the state:
//   - `p` prefix → `SCE_CSOUND_PARAM` (function parameter
//     references `p1` / `p2` / `p3` / ...).
//   - `a` prefix → `SCE_CSOUND_ARATE_VAR` (audio-rate
//     variable, e.g., `aOut`).
//   - `k` prefix → `SCE_CSOUND_KRATE_VAR` (control-rate
//     variable, e.g., `kEnv`).
//   - `i` prefix → `SCE_CSOUND_IRATE_VAR` (init-rate variable
//     — also covers `i`-statement identifiers per source
//     comment at `:107`).
//   - `g` prefix → `SCE_CSOUND_GLOBAL_VAR` (global variable
//     — `ga...`/`gk...`/`gi...` global naming convention).
// This is Csound's signature — every variable carries its
// evaluation rate in the name.
//
// **No string handling.** LexCsound's paint loop at
// `:68-152` NEVER enters a string state. Quote characters
// `"` / `'` are not in `IsCsoundOperator` at `:42-53` and
// not in `IsAWordStart` at `:37-40`, so they remain in
// `SCE_CSOUND_DEFAULT`. The `SCE_CSOUND_STRINGEOL` (15) slot
// is defined in `SciLexer.h` but ONLY referenced at `:63-64`
// as an `initStyle` guard (`if (initStyle == STRINGEOL)
// initStyle = DEFAULT`) — never `SetState`d. Effectively an
// orphan, same category as `SCE_CSOUND_INSTR` (4) and
// `SCE_CSOUND_COMMENTBLOCK` (9) which are also defined but
// never emitted (no `/*` `*/` block-comment handling either;
// only `;`-to-EOL line comments at `:130-131`).
//
// **Fold classifier.** `FoldCsoundInstruments` at `:154-205`
// folds on `instr` / `endin` opcode boundaries via a positive
// trigger at `:170` — the classifier's guard
// `stylePrev != SCE_CSOUND_OPCODE && style == SCE_CSOUND_OPCODE`
// only enters the `strcmp` check for `instr` / `endin` when
// the token is styled as `SCE_CSOUND_OPCODE`. So wordlist
// class 0 membership of `instr` / `endin` is load-bearing for
// correct folding. The host wordlist places them in class 0
// specifically for this reason; class 1 (HEADERSTMT) holds
// only the user-defined-opcode block markers `opcode` /
// `endop` since the fold classifier doesn't examine those.
//
// Style semantics (paint-loop citations reference
// LexCsound.cxx):
//
//   - SCE_CSOUND_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_CSOUND_COMMENT (1) — `;`-to-EOL line comment.
//     Entry at `:130-131`, exit at `:116-118` on `atLineEnd`.
//   - SCE_CSOUND_NUMBER (2) — numeric literal. Also catches
//     header settings like `0dbfs` that start with a digit.
//   - SCE_CSOUND_OPERATOR (3) — punctuation per
//     `IsCsoundOperator` at `:42-53`:
//     `* / - + ( ) = ^ [ ] < & > , | ~ % :`. `.` deliberately
//     excluded because it's used in numbers.
//   - SCE_CSOUND_INSTR (4) — never entered. Enum slot defined
//     in `SciLexer.h` but no `SetState` / `ChangeState` call
//     targets it anywhere in `ColouriseCsoundDoc`. Legacy
//     Csound conventions used `instr`-marker semantics; the
//     current lexer routes `instr` through the standard
//     class-1 header-statement wordlist path instead.
//   - SCE_CSOUND_IDENTIFIER (5) — bare identifier fallback
//     when no wordlist match and no rate-prefix match.
//     Framework convention: leave unmapped so identifiers
//     paint at STYLE_DEFAULT.
//   - SCE_CSOUND_OPCODE (6) — wordlist class 0 hit
//     (Csound opcodes — signal generators, filters,
//     effects, math intrinsics, I/O).
//   - SCE_CSOUND_HEADERSTMT (7) — wordlist class 1 hit
//     (orchestra header settings, block markers, score
//     statements, preprocessor bare forms).
//   - SCE_CSOUND_USERKEYWORD (8) — wordlist class 2 hit
//     (control-flow: `if`/`then`/`else`/`while`/`goto`
//     family / `loop_*` / `reinit`).
//   - SCE_CSOUND_COMMENTBLOCK (9) — never entered. Enum
//     slot defined but no `/* ... */` handling in the paint
//     loop; only line comments at `:130-131`.
//   - SCE_CSOUND_PARAM (10) — `p`-prefixed function parameter
//     reference. Auto-classified from IDENTIFIER at `:101-102`
//     when the first char is `p` and wordlist probes fail.
//   - SCE_CSOUND_ARATE_VAR (11) — `a`-prefixed audio-rate
//     variable. Auto-classified at `:103-104`.
//   - SCE_CSOUND_KRATE_VAR (12) — `k`-prefixed control-rate
//     variable. Auto-classified at `:105-106`.
//   - SCE_CSOUND_IRATE_VAR (13) — `i`-prefixed init-rate
//     variable / `i`-statement identifier. Auto-classified
//     at `:107-108`.
//   - SCE_CSOUND_GLOBAL_VAR (14) — `g`-prefixed global
//     variable. Auto-classified at `:109-110`.
//   - SCE_CSOUND_STRINGEOL (15) — never entered. Only
//     referenced as `initStyle` guard at `:63-64`; no
//     `SetState` call targets it.
pub const SCE_CSOUND_DEFAULT: usize = 0;
pub const SCE_CSOUND_COMMENT: usize = 1;
pub const SCE_CSOUND_NUMBER: usize = 2;
pub const SCE_CSOUND_OPERATOR: usize = 3;
pub const SCE_CSOUND_INSTR: usize = 4;
pub const SCE_CSOUND_IDENTIFIER: usize = 5;
pub const SCE_CSOUND_OPCODE: usize = 6;
pub const SCE_CSOUND_HEADERSTMT: usize = 7;
pub const SCE_CSOUND_USERKEYWORD: usize = 8;
pub const SCE_CSOUND_COMMENTBLOCK: usize = 9;
pub const SCE_CSOUND_PARAM: usize = 10;
pub const SCE_CSOUND_ARATE_VAR: usize = 11;
pub const SCE_CSOUND_KRATE_VAR: usize = 12;
pub const SCE_CSOUND_IRATE_VAR: usize = 13;
pub const SCE_CSOUND_GLOBAL_VAR: usize = 14;
pub const SCE_CSOUND_STRINGEOL: usize = 15;

// LexErlang style indices. Constants mirror `SciLexer.h:943-968`
// verbatim. The enum is non-contiguous: slots 0..=24 are used, 25..=30
// are skipped, and 31 is the `UNKNOWN` transient state. Dispatches
// SCLEX_ERLANG (= 53, per `SciLexer.h:69`) via a **six-class wordlist**
// declared at `vendor\lexilla\lexers\LexErlang.cxx:616-624`
// (`erlangWordListDesc[]`):
//
//     erlangWordListDesc[] = {
//         "Erlang Reserved words",          // class 0 → KEYWORD
//         "Erlang BIFs",                    // class 1 → BIFS
//         "Erlang Preprocessor",            // class 2 → PREPROC (leading `-`)
//         "Erlang Module Attributes",       // class 3 → MODULES_ATT (leading `-`)
//         "Erlang Documentation",           // class 4 → COMMENT_DOC (leading `@`)
//         "Erlang Documentation Macro",     // class 5 → COMMENT_DOC_MACRO (leading `@`)
//         0,
//     };
//
// **Case-SENSITIVE matching.** The identifier and preprocessor
// classifier at `LexErlang.cxx:212-224` and `:394-406` call
// `sc.GetCurrent(cur, sizeof(cur))` (byte-exact) at `:161`,
// `:201`, `:212`, and `:396` — four distinct capture sites, NOT
// `GetCurrentLowered`. Erlang is case-sensitive per language spec
// — atoms start lowercase, Variables start uppercase or `_`. All
// wordlist tokens are lowercase (Erlang convention). Same discipline
// as `D_KEYWORDS` / `R_RESERVED` / `COFFEESCRIPT_KEYWORDS` /
// `CSOUND_OPCODES`, inverted from `POWERSHELL_KEYWORDS` /
// `COBOL_KEYWORDS_A` / `FORTRAN_KEYWORDS`.
//
// **Sigil-carrying wordlists.** Three wordlists include their
// sigil in each token, because `sc.GetCurrent` returns the buffer
// starting from the last `SetState`, which is the sigil character:
//   - **Class 2 (preprocessor)** and **class 3 (module attributes)**
//     tokens start with `-` (e.g. `-define`, `-module`). Both are
//     probed inside the `PREPROCESSOR` parse state at
//     `LexErlang.cxx:393-407`; the `-` was captured by
//     `SetState(SCE_ERLANG_UNKNOWN)` at `:480-481` and remains at
//     the head of the captured buffer.
//   - **Class 4 (doc)** and **class 5 (doc-macro)** tokens start
//     with `@` (e.g. `@doc`, `@link`). Probed inside the
//     `COMMENT_DOC` / `COMMENT_DOC_MACRO` states at `:157-176`;
//     the `@` is captured because the state ratchets at `:143`
//     via `ForwardSetState(sc.state)` while still holding on
//     the `@` character.
// Consumers writing these wordlists must include the sigil verbatim.
//
// **Multi-level comment ratcheting.** LexErlang implements the
// Erlang convention that comment `%` count encodes documentation
// scope: `%` line-only (COMMENT), `%%` function-doc
// (COMMENT_FUNCTION), `%%%` module-doc (COMMENT_MODULE). The paint
// loop at `:109-153` uses fall-through cases in a `switch` to ratchet
// state upward as consecutive `%` characters are consumed. The
// `to_late_to_comment` flag at `:111,124` prevents downgrading
// mid-line if a non-`%` character intervened. Every ratchet level
// remains subject to embedded edoc `@tag` / `{@macro}` detection at
// `:136-153` for a nested doc emit.
//
// **Fold classifier.** `FoldErlangDoc` at `:531-614` folds on
// keyword transitions from non-KEYWORD → KEYWORD, and specifically
// checks the token spelling for `case` / `fun` / `if` / `query` /
// `receive` (increment) and `end` (decrement) at
// `ClassifyErlangFoldPoint:508-529`. **These six spellings MUST
// live in wordlist class 0** (SCE_ERLANG_KEYWORD). Two guards
// participate: (1) the keyword-start-capture guard at `:558-559`
// (`stylePrev != KEYWORD && style == KEYWORD`) records the token
// boundary, and (2) the symmetrically-inverted classifier-call
// guard at `:564-567` (`stylePrev == KEYWORD && style != KEYWORD
// && style != ATOM`) actually invokes `ClassifyErlangFoldPoint`
// at `:568-570`. If a spelling isn't in class 0, it settles to
// ATOM (or FUNCTION_NAME) instead of KEYWORD; neither guard
// fires, and the block doesn't fold. `fun` has an extra
// negation inside the classifier — the fold only increments
// when the following style isn't `SCE_ERLANG_FUNCTION_NAME`,
// i.e. `fun foo/1` inline function reference doesn't open a
// fold, only `fun () -> ... end` blocks do. `query` is an
// obsolete Erlang keyword (removed at R12B in 2007) but the
// fold classifier still checks it, so we keep it in the KEYWORD
// list too.
//
// **Braced fold on `%{` `%}`.** At `:574-583` the fold also
// increments on `%{` and decrements on `%}` inside any COMMENT
// state. This is an editor-fold convention (comment markers
// carrying explicit fold boundaries), unique to LexErlang.
//
// Style semantics (paint-loop citations reference LexErlang.cxx):
//
//   - SCE_ERLANG_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_ERLANG_COMMENT (1) — `%`-to-EOL line comment. Entry at
//     `:457-460`, ratchets to COMMENT_FUNCTION/MODULE on
//     consecutive `%` at `:112-131`.
//   - SCE_ERLANG_VARIABLE (2) — uppercase-first or `_`-prefixed
//     identifier (Erlang variable convention). Entry at `:492-493`
//     (branch condition at `:492`, `SetState` at `:493`),
//     exit at `:415-418`.
//   - SCE_ERLANG_NUMBER (3) — decimal / base-N integer / float /
//     exponent numeric literal. Multi-state numeric FSA at
//     `:326-389` covers `42`, `16#DEAD`, `3.14`, `1.0e-3`.
//   - SCE_ERLANG_KEYWORD (4) — wordlist class 0 hit (reserved
//     words). Emitted at `:213-214` after ATOM_UNQUOTED settles
//     via `sc.GetCurrent(cur, sizeof(cur))` at `:212`.
//   - SCE_ERLANG_STRING (5) — `"..."` double-quoted string.
//     Entry at `:455`, exit at `:419-422`.
//   - SCE_ERLANG_OPERATOR (6) — punctuation per `isoperator`
//     helper (Scintilla built-in, matches C-style punctuation
//     set). Entry at `:497-500` on any char passing `isoperator`.
//     The `|| sc.ch == '\\'` clause at `:498` extends the
//     DEFAULT-state operator-entry condition to include a bare
//     backslash — so `\` in DEFAULT paints as OPERATOR, not as
//     a character-escape (that's handled inside the CHARACTER
//     state at `:427-433`).
//   - SCE_ERLANG_ATOM (7) — lowercase-first bare identifier
//     (Erlang atom convention). Emitted at `:221` — the atom
//     fallback when no wordlist / function-call context matches.
//     Framework convention: leave unmapped so atoms paint at
//     STYLE_DEFAULT (they're the most common identifier form).
//   - SCE_ERLANG_FUNCTION_NAME (8) — atom followed by `(` or
//     `/` (function-call / function-reference syntax). Emitted
//     at `:218-219`.
//   - SCE_ERLANG_CHARACTER (9) — `$c` character literal, one
//     char after the `$` sigil. Entry at `:456`, exit at
//     `:427-433` after one forward (with `\\` escape allowance).
//   - SCE_ERLANG_MACRO (10) — `?MACRO` macro reference,
//     unquoted form. Entry via `MACRO_START` at `:465-468`,
//     settled at `:308-311`.
//   - SCE_ERLANG_RECORD (11) — `#record` record reference,
//     unquoted form. Entry via `RECORD_START` at `:461-464`,
//     settled at `:279-282`.
//   - SCE_ERLANG_PREPROC (12) — wordlist class 2 hit (`-define`
//     et al). Emitted at `:397-398` inside PREPROCESSOR state.
//   - SCE_ERLANG_NODE_NAME (13) — `atom@host` node-name form,
//     unquoted. Entry via `ATOM_UNQUOTED` → `NODE_NAME_UNQUOTED`
//     at `:190-191`, settled at `:247-250`.
//   - SCE_ERLANG_COMMENT_FUNCTION (14) — `%%` function-doc
//     comment level. Ratcheted at `:112-117`.
//   - SCE_ERLANG_COMMENT_MODULE (15) — `%%%` module-doc comment
//     level. Ratcheted at `:125-130`.
//   - SCE_ERLANG_COMMENT_DOC (16) — edoc `@tag` inside a comment.
//     Emitted at `:168-169` when the token matches the doc
//     wordlist (class 4).
//   - SCE_ERLANG_COMMENT_DOC_MACRO (17) — edoc `{@macro}`
//     inside a comment. Emitted at `:163-166` when the token
//     matches the doc-macro wordlist (class 5); on match the
//     paint loop consumes through the closing `}` at `:166-167`.
//   - SCE_ERLANG_ATOM_QUOTED (18) — `'quoted atom'` form.
//     Entry via `ATOM_QUOTED` at `:469-472`, settled at
//     `:234-238`.
//   - SCE_ERLANG_MACRO_QUOTED (19) — `?'quoted macro'` form.
//     Entry via `MACRO_QUOTED` at `:296-298`, settled at
//     `:315-320`.
//   - SCE_ERLANG_RECORD_QUOTED (20) — `#'quoted record'` form.
//     Entry via `RECORD_QUOTED` at `:267-269`, settled at
//     `:286-291`.
//   - SCE_ERLANG_NODE_NAME_QUOTED (21) — `'quoted'@'quoted'`
//     quoted node-name form. Settled at `:254-262`.
//   - SCE_ERLANG_BIFS (22) — wordlist class 1 hit (built-in
//     functions from the `erlang` module). Emitted at `:215-217`
//     with the `strcmp(cur,"erlang:")` guard to avoid styling
//     the literal `"erlang:"` prefix as a BIF.
//   - SCE_ERLANG_MODULES (23) — atom followed by `:` (module
//     prefix in `module:function()`). Emitted at `:200-203`
//     after ATOM_UNQUOTED sees a `:` and a following alnum or
//     `'`.
//   - SCE_ERLANG_MODULES_ATT (24) — wordlist class 3 hit
//     (`-module`, `-export`, `-behaviour`, ...). Emitted at
//     `:399-400`.
//   - SCE_ERLANG_UNKNOWN (31) — transient parse-in-progress
//     state, set on characters that trigger a parse_state
//     transition at `:463`, `:467`, `:471`, `:478`, `:481`,
//     `:491`, `:496`. The paint loop settles it to a real style
//     before emit. Framework convention: leave unmapped.
pub const SCE_ERLANG_DEFAULT: usize = 0;
pub const SCE_ERLANG_COMMENT: usize = 1;
pub const SCE_ERLANG_VARIABLE: usize = 2;
pub const SCE_ERLANG_NUMBER: usize = 3;
pub const SCE_ERLANG_KEYWORD: usize = 4;
pub const SCE_ERLANG_STRING: usize = 5;
pub const SCE_ERLANG_OPERATOR: usize = 6;
pub const SCE_ERLANG_ATOM: usize = 7;
pub const SCE_ERLANG_FUNCTION_NAME: usize = 8;
pub const SCE_ERLANG_CHARACTER: usize = 9;
pub const SCE_ERLANG_MACRO: usize = 10;
pub const SCE_ERLANG_RECORD: usize = 11;
pub const SCE_ERLANG_PREPROC: usize = 12;
pub const SCE_ERLANG_NODE_NAME: usize = 13;
pub const SCE_ERLANG_COMMENT_FUNCTION: usize = 14;
pub const SCE_ERLANG_COMMENT_MODULE: usize = 15;
pub const SCE_ERLANG_COMMENT_DOC: usize = 16;
pub const SCE_ERLANG_COMMENT_DOC_MACRO: usize = 17;
pub const SCE_ERLANG_ATOM_QUOTED: usize = 18;
pub const SCE_ERLANG_MACRO_QUOTED: usize = 19;
pub const SCE_ERLANG_RECORD_QUOTED: usize = 20;
pub const SCE_ERLANG_NODE_NAME_QUOTED: usize = 21;
pub const SCE_ERLANG_BIFS: usize = 22;
pub const SCE_ERLANG_MODULES: usize = 23;
pub const SCE_ERLANG_MODULES_ATT: usize = 24;
pub const SCE_ERLANG_UNKNOWN: usize = 31;

// LexEScript style indices. 12 contiguous slots (0..=11) for
// ESCRIPT — POL (Penultima Online)'s server-side scripting
// language for Ultima Online emulator scripts, extension `.em`.
// Constants mirror `SciLexer.h:831-842` verbatim. Dispatches
// SCLEX_ESCRIPT (= 41, per `SciLexer.h:57`) via a **three-class
// wordlist** declared at
// `vendor\lexilla\lexers\LexEScript.cxx:270-275`
// (`ESCRIPTWordLists[]`):
//
//     ESCRIPTWordLists[] = {
//         "Primary keywords and identifiers",       // class 0 → WORD
//         "Intrinsic functions",                    // class 1 → WORD2
//         "Extended and user defined functions",    // class 2 → WORD3
//         0,
//     };
//
// **Case-INSENSITIVE by default.** The lexer at
// `LexEScript.cxx:54` reads the `escript.case.sensitive` property
// (default 0) and, when unset, calls `sc.GetCurrentLowered(s,
// sizeof(s))` at `:87` — so wordlist tokens **must be
// all-lowercase**. Same discipline as `PASCAL_KEYWORDS` (LexPascal
// lowercases), inverted from `CSOUND_OPCODES` / `ERLANG_KEYWORDS`
// / `D_KEYWORDS` (byte-exact).
//
// **Load-bearing fold-classifier / class-2 coupling.** The fold
// classifier `classifyFoldPointESCRIPT` at `:152-171` and its
// caller `FoldESCRIPTDoc` at `:232-243` **only fire when
// `style == SCE_ESCRIPT_WORD3`** — the fold-critical control-flow
// tokens (`for` / `foreach` / `program` / `function` / `while` /
// `case` / `if` openers; `endfor` / `endforeach` / `endprogram` /
// `endfunction` / `endwhile` / `endcase` / `endif` closers;
// `else` / `elseif` half-block markers) **must** live in class 2
// (`ESCRIPT_FOLDWORDS`) so the identifier classifier at `:92-97`
// styles them as `SCE_ESCRIPT_WORD3`. The `if (keywords.InList(s))
// ... else if (keywords2.InList(s)) ... else if
// (keywords3.InList(s))` cascade is first-match-wins, so a
// fold-critical token duplicated in class 0 gets `SCE_ESCRIPT_WORD`
// (bold Keyword) instead of `SCE_ESCRIPT_WORD3` — the fold
// classifier never sees it, and blocks won't fold. Class 0 is
// therefore reserved for **non-fold** primary vocabulary
// (declarations `var` / `const` / `struct` / `dictionary`,
// booleans `true` / `false` / `nil`, exit statements `return` /
// `break` / `continue` / `exit`, boolean operators `and` / `or` /
// `not`, loop-body / iteration modifiers `do` / `then` / `to` /
// `downto` / `step` / `in` / `repeat` / `until` / `goto`).
//
// **Fold-classifier prev-word memory.** `classifyFoldPointESCRIPT`
// at `:154` short-circuits if `prevWord == "end"` (a stray `end`
// token guard) and at `:155-156` inverts the level for `elseif`
// alone OR `if` following `else` (bare `else if` — Pascal-style
// with a space between the two words; the current-token check is
// `s == "if"` gated on `prevWord == "else"`, matching source
// order `else` then `if`). Only class-2 tokens contribute to
// `prevWord` (updated at `:241` inside the `style ==
// SCE_ESCRIPT_WORD3` branch), so `if` / `else` need class 2
// membership for this coupling too.
//
// **Braced fold on `//{` `//}` line-comment markers.** At
// `:215-224` inside a `SCE_ESCRIPT_COMMENTLINE` styled range,
// `//{` opens a fold and `//}` closes one — an editor-fold
// convention where line-comment markers carry explicit fold
// boundaries. Same shape as LexErlang's `%{`/`%}` inside
// COMMENT states.
//
// Style semantics (paint-loop citations reference LexEScript.cxx):
//
//   - SCE_ESCRIPT_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_ESCRIPT_COMMENT (1) — `/* ... */` block comment.
//     Entry at `:132-134`, exit at `:102-106`.
//   - SCE_ESCRIPT_COMMENTLINE (2) — `//`-to-EOL line comment.
//     Entry at `:135-136`, exit at `:112-115`.
//   - SCE_ESCRIPT_COMMENTDOC (3) — enum slot defined in
//     `SciLexer.h` but **never entered** by
//     `ColouriseESCRIPTDoc`; the `else if (sc.state ==
//     SCE_ESCRIPT_COMMENTDOC)` branch at `:107-111` handles
//     exit only. No `SetState(SCE_ESCRIPT_COMMENTDOC)` call
//     exists anywhere in the paint loop. Legacy Javadoc-style
//     entry point that was dropped. Mapped for
//     forward-compatibility if a future lexer patch re-enables
//     it, but effectively an orphan today.
//   - SCE_ESCRIPT_NUMBER (4) — decimal / float numeric literal.
//     Entry at `:128-129`, exit at `:77-80`.
//   - SCE_ESCRIPT_WORD (5) — wordlist class 0 hit
//     ("Primary keywords and identifiers"). Emitted at
//     `:92-93` via `ChangeState` after IDENTIFIER settles.
//   - SCE_ESCRIPT_STRING (6) — `"..."` double-quoted string
//     with `\"` and `\\` escape support. Entry at `:137-138`,
//     exit at `:116-123`.
//   - SCE_ESCRIPT_OPERATOR (7) — restricted operator set:
//     `+ - * / = < > & | ! ? :`. Entry at `:140-141`.
//     NOTE: does NOT use `isoperator` — the commented-out
//     `isoperator` at `:139` shows the original intent; the
//     current cascade at `:140` explicitly enumerates the
//     accepted operators, so `. , ; ( ) [ ]` fall through
//     to DEFAULT.
//   - SCE_ESCRIPT_IDENTIFIER (8) — bare identifier fallback
//     when no wordlist match. Emitted transiently at
//     `:130-131` (also accepts `#`-prefixed forms per
//     `IsAWordStart || sc.ch == '#'`) and settles to
//     `SCE_ESCRIPT_DEFAULT` at `:100` after the wordlist
//     probe cascade at `:92-97`. Framework convention:
//     leave unmapped so bare identifiers paint at
//     STYLE_DEFAULT.
//   - SCE_ESCRIPT_BRACE (9) — `{` / `}` structural braces.
//     Distinct from OPERATOR (7). Entry at `:142-143`.
//   - SCE_ESCRIPT_WORD2 (10) — wordlist class 1 hit
//     ("Intrinsic functions"). Emitted at `:94-95`.
//   - SCE_ESCRIPT_WORD3 (11) — wordlist class 2 hit
//     ("Extended and user defined functions"). Emitted at
//     `:96-97`. **Load-bearing for `FoldESCRIPTDoc`** at
//     `:232-243`: only class-2 tokens contribute to
//     `prevWord` and to the fold-classifier's level
//     adjustments, so fold-critical control-flow keywords
//     MUST live in this class.
pub const SCE_ESCRIPT_DEFAULT: usize = 0;
pub const SCE_ESCRIPT_COMMENT: usize = 1;
pub const SCE_ESCRIPT_COMMENTLINE: usize = 2;
pub const SCE_ESCRIPT_COMMENTDOC: usize = 3;
pub const SCE_ESCRIPT_NUMBER: usize = 4;
pub const SCE_ESCRIPT_WORD: usize = 5;
pub const SCE_ESCRIPT_STRING: usize = 6;
pub const SCE_ESCRIPT_OPERATOR: usize = 7;
pub const SCE_ESCRIPT_IDENTIFIER: usize = 8;
pub const SCE_ESCRIPT_BRACE: usize = 9;
pub const SCE_ESCRIPT_WORD2: usize = 10;
pub const SCE_ESCRIPT_WORD3: usize = 11;

// LexForth style indices. 12 contiguous slots (0..=11) for
// Forth — the stack-based concatenative programming language
// (extension `.forth`). Constants mirror `SciLexer.h:702-713`
// verbatim. Dispatches SCLEX_FORTH (= 52, per `SciLexer.h:68`)
// via a **six-class wordlist** declared at
// `vendor\lexilla\lexers\LexForth.cxx:161-169`
// (`forthWordLists[]`):
//
//     forthWordLists[] = {
//         "control keywords",              // class 0 → CONTROL
//         "keywords",                      // class 1 → KEYWORD
//         "definition words",              // class 2 → DEFWORD
//         "prewords with one argument",    // class 3 → PREWORD1
//         "prewords with two arguments",   // class 4 → PREWORD2
//         "string definition keywords",    // class 5 → STRING
//         0,
//     };
//
// **Case-INSENSITIVE.** The identifier-classification path at
// `LexForth.cxx:73` calls `sc.GetCurrentLowered(s, sizeof(s))`
// (lowercased), NOT `GetCurrent`. Forth is traditionally written
// in uppercase but source can be any case — the lexer lowercases
// before probing, so wordlist tokens must be lowercase. Same
// discipline as `PASCAL_KEYWORDS` / `ESCRIPT_KEYWORDS`, inverted
// from `ERLANG_KEYWORDS` / `CSOUND_OPCODES` (both byte-exact).
//
// **First-match-wins cascade across all six classes.** The
// identifier settle path at `:75-88` probes in exact class order
// 0 → 1 → 2 → 3 → 4 → 5. A token duplicated in an earlier class
// silently wins over a later class. Cross-class disjointness is
// required for correct styling; the host wordlists enforce this
// via the invariant test.
//
// **Class 5 (STRING) is behaviorally distinct.** At `:86-87`,
// when a token matches the STRING wordlist, the lexer both
// changes state AND sets `newState = SCE_FORTH_STRING` — so the
// paint loop stays in STRING state on subsequent characters
// until the closing `"`. Class 5 tokens are exclusively
// **string-parsing openers** like `s"` / `."` / `abort"` / `c"`
// / `s\"` — words that syntactically start a string literal in
// Forth's whitespace-delimited token stream.
//
// **Auto-styled word-definition markers.** The paint loop at
// `:138-149` styles `:` and `;` as `SCE_FORTH_DEFWORD` DIRECTLY,
// without wordlist lookup, when they appear in whitespace-
// delimited positions. So the DEFWORD wordlist should NOT
// include `:` / `;` — they would be dead entries. `:` opens a
// definition (with subsequent whitespace also colored as
// DEFWORD to highlight the definition name); `;` closes a
// definition.
//
// **Symbolic word alphabet.** `IsAWordStart` at `:31-35`
// accepts alnum plus `! # ' ( * + , - . / < = > ? @ [ \ ] _`.
// This is unusually permissive because Forth tradition allows
// symbolic word names like `!` (store), `@` (fetch), `>r`
// (to-return-stack), `+!` (add-to-memory), `,` (compile-cell).
// The lexer treats these as full words, not operators —
// wordlist entries must include the exact symbolic form for
// each canonical token.
//
// **Number literals with sigil prefixes.** `:120-129` recognises
// `$`-prefix hex (`$DEADBEEF`) and `%`-prefix binary (`%1010`)
// numbers, in addition to decimal / `.` / `e` / `E` scientific
// forms. This is a Forth tradition — most implementations accept
// both prefixes.
//
// **No fold.** `FoldForthDoc` at `:157-159` is a no-op stub.
// Forth's whitespace-delimited nested-parenthesis grammar
// doesn't admit line-based folding, so the lexer deliberately
// declines to fold. Consequence: no fold-classifier-driven
// wordlist constraints (unlike ESCRIPT class 2, unlike Erlang's
// KEYWORD `case`/`fun`/`if`/etc.). Any class 0 CONTROL token
// can be placed freely.
//
// **Forth-2012 locals syntax.** `SCE_FORTH_LOCALE` (11) is
// entered at `:136-137` on `{` and exits at `:102-105` on `}`.
// This colors the `{ name1 name2 ... }` LOCALS declaration
// syntax from Forth-2012 §13. Contains no wordlist content —
// the state is bounded by literal braces.
//
// Style semantics (paint-loop citations reference LexForth.cxx):
//
//   - SCE_FORTH_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_FORTH_COMMENT (1) — `\ `-to-EOL line comment. Entry
//     at `:114-115` (requires whitespace before `\` and
//     whitespace or line end after — Forth's `\` word is
//     whitespace-delimited).
//   - SCE_FORTH_COMMENT_ML (2) — `( ... )` block comment.
//     Entry at `:116-119` (requires whitespace boundaries
//     around `(` — Forth's `(` is itself a word), exit at
//     `:65-66` on `)`.
//   - SCE_FORTH_IDENTIFIER (3) — bare-identifier transient
//     state entered at `:134-135` for any token starting with
//     an alnum or symbolic-word char. Settles to KEYWORD /
//     CONTROL / DEFWORD / PREWORD1 / PREWORD2 / STRING /
//     DEFAULT at `:72-89` on whitespace, based on wordlist
//     probes. Framework convention: leave unmapped so
//     unmatched bare words paint at STYLE_DEFAULT.
//   - SCE_FORTH_CONTROL (4) — wordlist class 0 hit
//     (control-flow structural words).
//   - SCE_FORTH_KEYWORD (5) — wordlist class 1 hit (general
//     runtime vocabulary).
//   - SCE_FORTH_DEFWORD (6) — wordlist class 2 hit
//     (definition words) OR the auto-styled `:` / `;`
//     markers at `:138-149`.
//   - SCE_FORTH_PREWORD1 (7) — wordlist class 3 hit
//     (prewords with one argument — compile-time next-token
//     consumers).
//   - SCE_FORTH_PREWORD2 (8) — wordlist class 4 hit (prewords
//     with two arguments).
//   - SCE_FORTH_NUMBER (9) — decimal / hex (`$` prefix) /
//     binary (`%` prefix) / float numeric literal. Entry at
//     `:120-133`, exit at `:91-97` (falls back to IDENTIFIER
//     if a wordlist match is found later).
//   - SCE_FORTH_STRING (10) — string literal. Entered from
//     the STRING wordlist match at `:86-87`, exit at `:98-101`
//     on closing `"`.
//   - SCE_FORTH_LOCALE (11) — Forth-2012 `{ name1 name2 ... }`
//     local-variable declaration. Entry at `:136-137` on
//     `{`, exit at `:102-105` on `}`. No wordlist — the state
//     is delimited purely by braces.
pub const SCE_FORTH_DEFAULT: usize = 0;
pub const SCE_FORTH_COMMENT: usize = 1;
pub const SCE_FORTH_COMMENT_ML: usize = 2;
pub const SCE_FORTH_IDENTIFIER: usize = 3;
pub const SCE_FORTH_CONTROL: usize = 4;
pub const SCE_FORTH_KEYWORD: usize = 5;
pub const SCE_FORTH_DEFWORD: usize = 6;
pub const SCE_FORTH_PREWORD1: usize = 7;
pub const SCE_FORTH_PREWORD2: usize = 8;
pub const SCE_FORTH_NUMBER: usize = 9;
pub const SCE_FORTH_STRING: usize = 10;
pub const SCE_FORTH_LOCALE: usize = 11;

// LexMMIXAL style indices. 18 contiguous slots (0..=17) for
// MMIXAL — Donald Knuth's MMIX assembly language from The Art of
// Computer Programming Vol 1 Fascicle 1 (extension `.mms`).
// Constants mirror `SciLexer.h:878-895` verbatim. Dispatches
// SCLEX_MMIXAL (= 44, per `SciLexer.h:60`) via a **three-class
// wordlist** declared at
// `vendor\lexilla\lexers\LexMMIXAL.cxx:178-183`
// (`MMIXALWordListDesc[]`):
//
//     MMIXALWordListDesc[] = {
//         "Operation Codes",   // class 0 → OPCODE_VALID
//         "Special Register",  // class 1 → REGISTER
//         "Predefined Symbols",// class 2 → SYMBOL
//         0,
//     };
//
// **Case-SENSITIVE.** The identifier-classification paths at
// `LexMMIXAL.cxx:104, :123` call `sc.GetCurrent(s, sizeof(s))`
// (NOT `GetCurrentLowered`). MMIXAL by convention writes opcodes
// in uppercase (`ADD`, `TRAP`, `LDO`), registers with lowercase
// `r` prefix (`rA`, `rBB`), and predefined symbols in mixed case
// (`Fputs`, `StdOut`, `ROUND_NEAR`) — the exact spelling must
// match, byte-for-byte.
//
// **Line-based lexer.** Unlike most Scintilla lexers, MMIXAL is
// structurally line-oriented. At `:64-70` every line begins in
// `SCE_MMIXAL_LEADWS` (or `SCE_MMIXAL_INCLUDE` if the line
// starts with `@i`). At `:72-83` the first non-whitespace
// character in a LEADWS line transitions to:
//
//   - `SCE_MMIXAL_COMMENT` if it isn't a word character (comments
//      don't need `%` — anything not-a-label starts a comment);
//   - `SCE_MMIXAL_LABEL` if it IS a word character AND we're still
//     at line start (labels ride column 0);
//   - `SCE_MMIXAL_OPCODE_PRE` → `SCE_MMIXAL_OPCODE` if the token
//     appears after leading whitespace (indented instruction).
//
// After the opcode, at `:154-172` the OPERANDS state dispatches
// on the character class of the first non-whitespace character:
// digit → NUMBER, word/@ → REF, `"` → STRING, `'` → CHAR, `$` →
// REGISTER (numeric $-register), `#` → HEX, symbolic operator →
// OPERATOR, whitespace → COMMENT (rest of line is a comment,
// MMIXAL style).
//
// **Opcode validation.** At `:120-129`, when the OPCODE-collect
// state ends on a non-word char, the collected token is probed
// against the opcodes wordlist (class 0). Match →
// `OPCODE_VALID`, no-match → `OPCODE_UNKNOWN`. Then transitions
// to `OPCODE_POST`.
//
// **REF settle with base-prefix stripping.** At `:101-115`, when
// the REF collect state ends, `sc.GetCurrent(s0, ...)` captures
// the identifier byte-exact, then if it begins with `:` the
// leading `:` is stripped for the wordlist probe (MMIXAL's
// `:Global` base-prefix syntax). Probes special_register
// (class 1) → `REGISTER`, else predef_symbols (class 2) →
// `SYMBOL`, else stays `REF`.
//
// **`@include` directive.** At `:65-66`, a line beginning with
// literal `@i` transitions to `SCE_MMIXAL_INCLUDE` for the
// entire line — MMIXAL's file-inclusion preprocessor directive.
//
// **IsAWordChar at `:35-37`**: alnum (ASCII) + `:` + `_`. `:` is
// accepted inside identifiers so MMIXAL's base-prefix syntax
// (`:GlobalLabel`) parses as one token. Note this means opcode
// mnemonics starting with a digit — `2ADDU`, `4ADDU`, `8ADDU`,
// `16ADDU` — enter OPCODE state via the `!isspace(sc.ch)`
// transition at `:117-119` (any non-space in OPCODE_PRE), collect
// full alnum span, then probe the opcodes wordlist byte-exact.
// These four `NADD` opcodes must be present verbatim as MMIXAL
// source strings.
//
// **No fold.** LexMMIXAL registers `0` as the fold function at
// `:185` (`extern const LexerModule lmMMIXAL(SCLEX_MMIXAL,
// ColouriseMMIXALDoc, "mmixal", 0, MMIXALWordListDesc);`). No
// fold-classifier constraints on wordlist content.
//
// Style semantics (paint-loop citations reference LexMMIXAL.cxx):
//
//   - SCE_MMIXAL_LEADWS (0) — leading whitespace, transient
//     entry state per line. Framework convention: leave unmapped.
//   - SCE_MMIXAL_COMMENT (1) — MMIXAL comment. Any line-leading
//     non-word char starts a comment (`:74-75`); after operands
//     any whitespace-then-anything is a trailing comment
//     (`:156-157`).
//   - SCE_MMIXAL_LABEL (2) — column-0 identifier declaring a
//     label. Entry at `:77-78`.
//   - SCE_MMIXAL_OPCODE (3) — transient collect state for the
//     opcode mnemonic. Framework convention: leave unmapped.
//   - SCE_MMIXAL_OPCODE_PRE (4) — transient whitespace between
//     label and opcode. Framework convention: leave unmapped.
//   - SCE_MMIXAL_OPCODE_VALID (5) — opcode mnemonic that hit
//     wordlist class 0. Entry at `:124-125`.
//   - SCE_MMIXAL_OPCODE_UNKNOWN (6) — opcode mnemonic that
//     missed wordlist class 0. Entry at `:126-127`. Framework
//     convention: leave unmapped so unrecognized opcodes paint
//     at STYLE_DEFAULT (they may be user-defined macros).
//   - SCE_MMIXAL_OPCODE_POST (7) — transient state after opcode
//     validation. Framework convention: leave unmapped.
//   - SCE_MMIXAL_OPERANDS (8) — transient dispatch state
//     between operands. Framework convention: leave unmapped.
//   - SCE_MMIXAL_NUMBER (9) — decimal literal. Entry at
//     `:158-159`; exits to OPERANDS on non-digit or degrades to
//     REF at `:90-92` if a word char follows.
//   - SCE_MMIXAL_REF (10) — bare identifier reference. Entry at
//     `:160-161`; settles to REGISTER / SYMBOL / stays-REF at
//     `:101-115`. Framework convention: leave unmapped so
//     unmatched refs paint at STYLE_DEFAULT.
//   - SCE_MMIXAL_CHAR (11) — `'`-delimited char literal. Entry
//     at `:164-165`, exit at `:138-142`.
//   - SCE_MMIXAL_STRING (12) — `"`-delimited string literal.
//     Entry at `:162-163`, exit at `:132-136`.
//   - SCE_MMIXAL_REGISTER (13) — `$`-prefixed numeric register
//     (`$0`..`$255`) via direct entry at `:166-167`, OR a REF
//     that hit wordlist class 1 (special register like `rA`) via
//     `:109-110`.
//   - SCE_MMIXAL_HEX (14) — `#`-prefixed hex literal
//     (`#DEADBEEF`). Entry at `:168-169`.
//   - SCE_MMIXAL_OPERATOR (15) — MMIXAL operator char from
//     `isMMIXALOperator` at `:39-49` (`+-|^*/%<>&~$,()[]`).
//     Entry at `:170-171`.
//   - SCE_MMIXAL_SYMBOL (16) — predefined symbol via REF hit on
//     wordlist class 2 at `:111-112`.
//   - SCE_MMIXAL_INCLUDE (17) — `@include` directive line.
//     Entry at `:65-66`.
pub const SCE_MMIXAL_LEADWS: usize = 0;
pub const SCE_MMIXAL_COMMENT: usize = 1;
pub const SCE_MMIXAL_LABEL: usize = 2;
pub const SCE_MMIXAL_OPCODE: usize = 3;
pub const SCE_MMIXAL_OPCODE_PRE: usize = 4;
pub const SCE_MMIXAL_OPCODE_VALID: usize = 5;
pub const SCE_MMIXAL_OPCODE_UNKNOWN: usize = 6;
pub const SCE_MMIXAL_OPCODE_POST: usize = 7;
pub const SCE_MMIXAL_OPERANDS: usize = 8;
pub const SCE_MMIXAL_NUMBER: usize = 9;
pub const SCE_MMIXAL_REF: usize = 10;
pub const SCE_MMIXAL_CHAR: usize = 11;
pub const SCE_MMIXAL_STRING: usize = 12;
pub const SCE_MMIXAL_REGISTER: usize = 13;
pub const SCE_MMIXAL_HEX: usize = 14;
pub const SCE_MMIXAL_OPERATOR: usize = 15;
pub const SCE_MMIXAL_SYMBOL: usize = 16;
pub const SCE_MMIXAL_INCLUDE: usize = 17;

// LexNim style indices. 17 contiguous slots (0..=16) for
// Nim — the statically-typed compiled systems programming
// language with Python-like indentation-based syntax
// (extension `.nim`). Constants mirror `SciLexer.h:1933-1949`
// verbatim. Dispatches SCLEX_NIM (= 126, per `SciLexer.h:142`)
// via a **single-class wordlist** declared at
// `vendor\lexilla\lexers\LexNim.cxx:182-185`
// (`nimWordListDesc[]`):
//
//     nimWordListDesc[] = { "Keywords", nullptr };
//
// **Case-SENSITIVE.** The identifier-classification path at
// `LexNim.cxx:446-462` calls `sc.GetCurrent(s, sizeof(s))`
// (NOT `GetCurrentLowered`), then probes `keywords.InList(s)`.
// Nim's identifier-comparison at the language level is
// case-insensitive-except-first-char with underscore collapse
// (`fooBar` == `foo_bar` == `FOOBAR` when the first char
// matches), but the lexer's wordlist probe is a plain
// byte-exact `strcmp`-family lookup via `WordList::InList`.
// Nim source overwhelmingly writes keywords lowercase, so
// wordlist tokens must be lowercase to match.
//
// **`IsAWordChar` at `:65-67`** accepts ASCII alnum + `_` + `.`.
// The `.` inclusion does NOT mean the lexer collects `x.foo`
// as a single identifier span — `SCE_NIM_IDENTIFIER` exits
// immediately on `.` via the explicit disjunct at `:447`
// (`sc.ch == '.' || !IsAWordChar(sc.ch)`), and `.` is in the
// operator strchr set at `:713`, so `x.foo` tokenizes as
// three states: IDENTIFIER (`x`) → OPERATOR (`.`) →
// IDENTIFIER (`foo`). The `.` presence in `IsAWordChar` is
// instead used by (a) the NUMBER state's decimal-continuation
// check at `:387` (recognising `1.5`-style floats without
// re-tokenising the decimal point separately) and (b)
// sub-identifier keyword-suppression checks that need to
// know when a `.` sits between two identifier-shaped spans.
// Wordlist entries are single identifier tokens with no
// dots.
//
// **Auto-styled FUNCNAME after definition keywords.** At
// `:446-465`, when a keyword identifier hits the wordlist
// AND `IsFuncName(s)` returns true (the token is one of
// `proc`/`func`/`macro`/`method`/`template`/`iterator`/
// `converter` per `:85-103`), the lexer sets a
// `funcNameExists` flag; the NEXT identifier or backtick-
// quoted identifier gets emitted as `SCE_NIM_FUNCNAME`
// instead of the usual IDENTIFIER/BACKTICKS style. This is
// entirely paint-loop-driven — no wordlist support required
// for FUNCNAME styling.
//
// **String literal families.** Nim's string syntax is rich —
// six distinct entry paths at `:624-679`:
//   1. Bare double-quote `"..."` → `SCE_NIM_STRING` (`:669`).
//   2. Triple double-quote `"""..."""` → `SCE_NIM_TRIPLEDOUBLE`
//      (`:656`), with special handling for up-to-5 opening
//      quotes at `:660-667`.
//   3. Raw string `r"..."` / `R"..."` → `SCE_NIM_STRING` with
//      `isStylingRawString` flag (`:625-640`).
//   4. Generalized raw string `xyz"..."` (any identifier before
//      the quote) → configurable via
//      `lexer.nim.raw.strings.highlight.ident`; defaults to
//      styling the identifier as IDENTIFIER, the quote as
//      STRING.
//   5. Single-quote character `'x'` → `SCE_NIM_CHARACTER`
//      (`:677`).
//   6. Triple single-quote `'''...'''` → `SCE_NIM_TRIPLE`
//      (`:675`).
//
// **Backtick-quoted identifiers.** Nim allows `` `identifier` ``
// for using keywords as identifiers (e.g. `` `if` ``). At
// `:681-687`, the paint loop enters `SCE_NIM_FUNCNAME` if
// `funcNameExists` (backtick immediately after a def keyword)
// or `SCE_NIM_BACKTICKS` otherwise.
//
// **Comment family.** Four distinct comment states at
// `:693-711`:
//   - `##[` → `SCE_NIM_COMMENTDOC` (nestable block doc comment).
//   - `#[` → `SCE_NIM_COMMENT` (nestable block comment).
//   - `##` → `SCE_NIM_COMMENTLINEDOC` (line doc comment).
//   - `#` → `SCE_NIM_COMMENTLINE` (line comment).
// Block comments are nestable per Nim spec; the lexer tracks
// `commentNestLevel` in `styler.SetLineState` at `:697`.
//
// **STRINGEOL is an error state.** At `:495`/`:555`/`:567`/
// `:575`, an unterminated `FUNCNAME` (backtick-quoted def-name
// span) / `STRING` / `CHARACTER` / `BACKTICKS` (that hits an
// unescaped newline) has its state `ChangeState`'d to
// `SCE_NIM_STRINGEOL`. Four sources total — `:495` is the
// FUNCNAME case (def-position backtick span never closed
// before EOL), not just the three literal-string states.
// Similarly `SCE_NIM_NUMERROR` marks a malformed numeric
// literal per `:52-58` and `:443`.
//
// **Operator set at `:713`.** `"()[]{}:=;-\\/&%$!+<>|^?,.*~@"`
// — a wide set covering Nim's rich operator vocabulary
// including `not`/`and`/`or` word-operators (which are
// keywords, not `SCE_NIM_OPERATOR` tokens).
//
// **Fold** at `:728-812` uses indentation levels via
// `IndentAmount` at `:164-168` (Python-style indent-based
// folding), NOT brace or keyword-based folding.
//
// Style semantics (paint-loop citations reference LexNim.cxx):
//
//   - SCE_NIM_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_NIM_COMMENT (1) — `#[ ... ]#` nestable block comment.
//     Entry at `:704`, exit at `:499-516`.
//   - SCE_NIM_COMMENTDOC (2) — `##[ ... ]##` nestable block
//     doc comment. Entry at `:701-702`, exit at `:518-535`.
//   - SCE_NIM_COMMENTLINE (3) — `# ...` line comment. Entry
//     at `:709`, exit at `:537-541`.
//   - SCE_NIM_COMMENTLINEDOC (4) — `## ...` line doc comment.
//     Entry at `:706-707`, exit at `:537-541`.
//   - SCE_NIM_NUMBER (5) — decimal / hex (`0x`) / binary
//     (`0b`) / octal (`0o`) / exponent numeric literal.
//     Entry at `:605`, exit at `:368-444`.
//   - SCE_NIM_STRING (6) — `"..."` string literal (also raw
//     `r"..."`). Entry at `:633`/`:669`, exit at `:543-556`.
//   - SCE_NIM_CHARACTER (7) — `'x'` single-char literal.
//     Entry at `:677`, exit at `:559-568`.
//   - SCE_NIM_WORD (8) — identifier that hit the keywords
//     wordlist. Entry at `:455-456`.
//   - SCE_NIM_TRIPLE (9) — `'''...'''` triple-quote literal
//     (rare). Entry at `:675`, exit at `:594-598`.
//   - SCE_NIM_TRIPLEDOUBLE (10) — `"""..."""` triple-double-
//     quote literal. Entry at `:631`/`:656`, exit at
//     `:579-591`.
//   - SCE_NIM_BACKTICKS (11) — `` `identifier` `` backtick-
//     quoted identifier. Entry at `:685`, exit at `:571-576`.
//   - SCE_NIM_FUNCNAME (12) — identifier or backtick-span
//     immediately following a `proc`/`func`/`macro`/`method`/
//     `template`/`iterator`/`converter` keyword. Entry via
//     `:459` (identifier path) or `:683` (backtick path).
//     Auto-styled — no wordlist support needed.
//   - SCE_NIM_STRINGEOL (13) — unterminated backtick def-name
//     (`:495`, FUNCNAME source) / string (`:555`) / char
//     (`:567`) / backticks (`:575`) that hit end-of-line.
//     Four distinct paint-loop sources.
//   - SCE_NIM_NUMERROR (14) — malformed numeric literal
//     (invalid digit for base, multiple decimal points, etc.).
//     Entry at `:443` via `GetNumStyle(numType == FormatError)`.
//   - SCE_NIM_OPERATOR (15) — operator char from
//     `"()[]{}:=;-\\/&%$!+<>|^?,.*~@"` at `:713`. Entry at
//     `:714`, exit at `:364-366`.
//   - SCE_NIM_IDENTIFIER (16) — transient identifier-collect
//     state entered at `:690`. Settles to WORD / FUNCNAME /
//     stays-IDENTIFIER at `:446-465` based on the wordlist
//     probe and `funcNameExists`. Framework convention: leave
//     unmapped so unmatched bare identifiers paint at
//     STYLE_DEFAULT.
pub const SCE_NIM_DEFAULT: usize = 0;
pub const SCE_NIM_COMMENT: usize = 1;
pub const SCE_NIM_COMMENTDOC: usize = 2;
pub const SCE_NIM_COMMENTLINE: usize = 3;
pub const SCE_NIM_COMMENTLINEDOC: usize = 4;
pub const SCE_NIM_NUMBER: usize = 5;
pub const SCE_NIM_STRING: usize = 6;
pub const SCE_NIM_CHARACTER: usize = 7;
pub const SCE_NIM_WORD: usize = 8;
pub const SCE_NIM_TRIPLE: usize = 9;
pub const SCE_NIM_TRIPLEDOUBLE: usize = 10;
pub const SCE_NIM_BACKTICKS: usize = 11;
pub const SCE_NIM_FUNCNAME: usize = 12;
pub const SCE_NIM_STRINGEOL: usize = 13;
pub const SCE_NIM_NUMERROR: usize = 14;
pub const SCE_NIM_OPERATOR: usize = 15;
pub const SCE_NIM_IDENTIFIER: usize = 16;

// LexNncrontab style indices. 11 contiguous slots (0..=10) for
// nnCron's extended crontab format — a Windows scheduler / event
// monitor / automation manager by Nick Nemtsev
// (<http://www.nncron.ru/en_index.shtml>) that uses Forth as its
// embedded scripting language on top of cron-style time
// specifications (extension `.tab`). Constants mirror
// `SciLexer.h:691-701` verbatim. Dispatches SCLEX_NNCRONTAB
// (= 26, per `SciLexer.h:44`) via a **three-class wordlist**
// declared at `vendor\lexilla\lexers\LexCrontab.cxx:220-225`
// (`cronWordListDesc[]`):
//
//     cronWordListDesc[] = {
//         "Section keywords and Forth words",   // class 0 → SECTION
//         "nnCrontab keywords",                  // class 1 → KEYWORD
//         "Modifiers",                           // class 2 → MODIFIER
//         0,
//     };
//
// **Case-SENSITIVE.** The identifier-classification path at
// `LexCrontab.cxx:185-196` compares the collected buffer
// byte-exact via `WordList::InList` — no lowering, no folding.
// nnCron source is typically lowercase for keywords / modifiers
// and mixed-case for section markers (`Task`, `Time`, `Rule`,
// `When`, `Action`, `Days`, `Hours`, `Minutes`, …).
//
// **Hand-rolled state machine, no `StyleContext`.** Unlike most
// Lexilla lexers, `LexCrontab.cxx` uses a raw `switch(state)`
// loop with manual `styler.ColourTo` calls (`:63-215`) rather
// than the modern `StyleContext` API. Character transitions are
// hard-coded per state, and the identifier alphabet at
// `:175-177` is unusually wide: `isalnum` + `_` + `-` + `/` +
// `$` + `.` + `<` + `>` + `@`. This wide alphabet supports
// nnCron's directive-argument syntax where identifiers can
// carry inline delimiters (e.g. path-like fragments,
// less-than / greater-than window brackets).
//
// **Line-oriented STATE MACHINE.** Every line begins with the
// default state at `:64`. Nine entry paths dispatch from
// DEFAULT:
//   1. `#(` at `:69-72` → SCE_NNCRONTAB_TASK (task-start marker).
//   2. `)#` at `:83-86` → SCE_NNCRONTAB_TASK (task-end marker).
//   3. `\ ` / `\\t` (backslash + whitespace) at `:74-78` →
//      SCE_NNCRONTAB_COMMENT (extended Forth-style
//      whitespace-required backslash comment).
//   4. `#` (any other position) at `:79-82` → SCE_NNCRONTAB_COMMENT
//      (plain hash-to-EOL comment).
//   5. `"` at `:87-89` → SCE_NNCRONTAB_STRING.
//   6. `%` at `:90-93` or `<%` at `:94-97` → SCE_NNCRONTAB_ENVIRONMENT
//      (environment variable expansion `%VAR%` or `<%VAR%>` bracket
//      form).
//   7. `*` at `:98-101` → SCE_NNCRONTAB_ASTERISK (single-char
//      state, no transition — cron's "every" wildcard).
//   8. Alpha or `<` at `:102-106` → SCE_NNCRONTAB_IDENTIFIER
//      collect state.
//   9. Digit at `:107-111` → SCE_NNCRONTAB_NUMBER collect state.
//
// **Identifier settle at `:185-196`.** When the IDENTIFIER
// state's non-word char terminates the collect, the buffer is
// probed in class order 0 → 1 → 2:
//   - `section.InList(buffer)` → `SCE_NNCRONTAB_SECTION`
//   - else `keyword.InList(buffer)` → `SCE_NNCRONTAB_KEYWORD`
//   - else `modifier.InList(buffer)` → `SCE_NNCRONTAB_MODIFIER`
//   - else stays `SCE_NNCRONTAB_DEFAULT` (no styling).
// **First-match-wins cascade** in class order — a token
// duplicated in an earlier class silently masks its later-class
// sibling.
//
// **String / environment interleaving.** Inside STRING at
// `:141-146`, a `%` transitions to ENVIRONMENT with
// `insideString = true`; from ENVIRONMENT at `:159-163`, a `%`
// with `insideString` true transitions back to STRING. This
// supports `"...text %ENV_VAR% more text..."` syntax where the
// environment expansion is styled distinctly inside a string.
//
// **`<%...%>` environment bracket.** The ENVIRONMENT state
// entered via `<%` at `:94-97` exits on `>` at `:164-165`,
// matching the bracketed form. The plain `%VAR%` form exits on
// the closing `%`.
//
// **Delete-new memory management.** `LexCrontab.cxx:40` allocates
// a `char *buffer = new char[length+1]` and deletes it at
// `:217`. This is legacy-style Lexilla (not the modern
// `StyleContext` GetCurrent path). No security implication
// for the host — it's paint-loop-internal.
//
// **No fold.** Registered with `0` fold-function at `:227`.
//
// Style semantics (paint-loop citations reference LexCrontab.cxx):
//
//   - SCE_NNCRONTAB_DEFAULT (0) — whitespace / unclassified /
//     unmatched identifier. Framework convention: leave
//     unmapped.
//   - SCE_NNCRONTAB_COMMENT (1) — `#`-to-EOL or `\ `-to-EOL
//     line comment. Entry at `:74-82`, exit at `:122-127`
//     (newline).
//   - SCE_NNCRONTAB_TASK (2) — `#(` opening or `)#` closing
//     task-delimiter marker. Entry at `:69-86`, exit at
//     `:133-138` (newline).
//   - SCE_NNCRONTAB_SECTION (3) — identifier that hit wordlist
//     class 0. Entry at `:185-186` via `section.InList` probe.
//   - SCE_NNCRONTAB_KEYWORD (4) — identifier that hit wordlist
//     class 1. Entry at `:187-188` via `keyword.InList` probe
//     (after class-0 miss).
//   - SCE_NNCRONTAB_MODIFIER (5) — identifier that hit wordlist
//     class 2. Entry at `:192-193` via `modifier.InList` probe
//     (after class-0 and class-1 misses).
//   - SCE_NNCRONTAB_ASTERISK (6) — `*` wildcard cron marker.
//     Single-char state, entered and exited on the same char
//     at `:98-101`.
//   - SCE_NNCRONTAB_NUMBER (7) — decimal numeric literal.
//     Entry at `:107-111`, exit at `:202-213` (non-digit).
//   - SCE_NNCRONTAB_STRING (8) — `"..."` string literal.
//     Entry at `:87-89`, exit at `:149-152` (closing `"` or
//     newline).
//   - SCE_NNCRONTAB_ENVIRONMENT (9) — `%VAR%` or `<%VAR%>`
//     environment variable expansion. Entry at `:90-97` from
//     DEFAULT, or at `:141-146` from STRING (with
//     `insideString = true`). Exit at `:159-171` on closing
//     `%` / `>` / newline.
//   - SCE_NNCRONTAB_IDENTIFIER (10) — transient collect state
//     entered at `:102-106` for any alpha-starting token.
//     Settles to SECTION / KEYWORD / MODIFIER / DEFAULT at
//     `:185-196` on completion. Framework convention: leave
//     unmapped so unmatched bare identifiers paint at
//     STYLE_DEFAULT.
pub const SCE_NNCRONTAB_DEFAULT: usize = 0;
pub const SCE_NNCRONTAB_COMMENT: usize = 1;
pub const SCE_NNCRONTAB_TASK: usize = 2;
pub const SCE_NNCRONTAB_SECTION: usize = 3;
pub const SCE_NNCRONTAB_KEYWORD: usize = 4;
pub const SCE_NNCRONTAB_MODIFIER: usize = 5;
pub const SCE_NNCRONTAB_ASTERISK: usize = 6;
pub const SCE_NNCRONTAB_NUMBER: usize = 7;
pub const SCE_NNCRONTAB_STRING: usize = 8;
pub const SCE_NNCRONTAB_ENVIRONMENT: usize = 9;
pub const SCE_NNCRONTAB_IDENTIFIER: usize = 10;

// LexOScript style indices. 19 contiguous slots (0..=18) for
// OScript — the object-oriented programming language for
// OpenText Livelink (now OpenText Content Server), extension
// `.osx`. Constants mirror `SciLexer.h:1720-1738` verbatim.
// Dispatches SCLEX_OSCRIPT (= 106, per `SciLexer.h:122`) via
// a **six-class wordlist** declared at
// `vendor\lexilla\lexers\LexOScript.cxx:539-547`
// (`oscriptWordListDesc[]`) — the widest wordlist descriptor
// of Phase 4.5, ahead of Forth's 6 and Erlang's 6:
//
//     oscriptWordListDesc[] = {
//         "Keywords and reserved words",       // class 0 → KEYWORD
//         "Literal constants",                 // class 1 → CONSTANT
//         "Literal operators",                 // class 2 → OPERATOR
//         "Built-in value and reference types", // class 3 → TYPE
//         "Built-in global functions",         // class 4 → FUNCTION
//         "Built-in static objects",           // class 5 → OBJECT
//         0,
//     };
//
// **Case-INSENSITIVE.** `LexOScript.cxx` calls
// `sc.GetCurrentLowered(s, sizeof(s))` on both classification
// paths — once at `:141` inside `if (sc.Match('('))` at
// `:139-153` (paren path, populates the buffer probed by
// KEYWORD/OPERATOR/FUNCTION at `:144-152`), and again at
// `:156` inside the `else` at `:154-180` (no-paren path,
// populates the buffer probed by OBJECT/KEYWORD/CONSTANT/
// OPERATOR/TYPE/FUNCTION at `:163-176`). Two separate buffer
// scopes — every wordlist token must be lowercase to match
// either. Same discipline as `PASCAL_KEYWORDS` /
// `ESCRIPT_KEYWORDS` / `FORTH_KEYWORD`.
//
// **Context-sensitive classification** (`IdentifierClassifier`
// at `:114-182`). Unlike simple first-match-wins lexers, the
// classifier at `:132-181` performs a **two-phase probe**:
//   - **Parenthesis-suffix path** (`sc.Match('(')`, `:139-153`):
//     probes keywords → operators → functions → METHOD (default
//     if no wordlist matches). Any identifier immediately
//     followed by `(` is treated as a potential function call
//     unless it's a keyword or operator.
//   - **No-parenthesis path** (`:154-180`): if followed by `.`
//     AND matches objects (class 5), styles as OBJECT then
//     enters OPERATOR state for the dot. Otherwise probes
//     keywords → constants → operators → types → functions in
//     order first-match-wins.
//
// **Cross-class disjointness is context-scoped.** Because the
// two probe paths differ, the same token can theoretically hit
// different classes in different syntactic positions — e.g. a
// class-4 function name would style as FUNCTION on both paths
// (KEYWORD/OPERATOR/CONSTANT/TYPE probes all miss); a class-5
// object name styles as OBJECT only when followed by `.` and
// stays IDENTIFIER (styled DEFAULT) otherwise. Still, the
// wordlists should be cross-class disjoint to avoid ambiguous
// intent — the invariant test enforces pairwise disjointness
// across all 15 class-pair combinations.
//
// **Auto-styled LABEL and PROPERTY / METHOD.** Two paint-loop
// mechanics without wordlist support:
//   - **LABEL** (`SCE_OSCRIPT_LABEL`, `:13`): identifier at
//     the start of a line followed by `:` (colon). Entry at
//     `:241-243` after the IDENTIFIER collect state.
//   - **PROPERTY / METHOD** (`:17` / `:18`): `.identifier`
//     enters PROPERTY at `:345-355`; if the property span is
//     followed by `(`, `:262-263` upgrades it to METHOD.
//
// **GLOBAL** (`SCE_OSCRIPT_GLOBAL`, `:10`): `$xxx` or `$$xxx`
// process/thread-global variables. Entry at `:336-339`.
//
// **PREPROCESSOR + DOC_COMMENT.** OScript's `#`-directives
// (`#ifdef`, `#ifndef`, `#endif`, etc.) enter
// `SCE_OSCRIPT_PREPROCESSOR` at `:334-335`. A specific
// `#ifdef DOC` line at `:94-102, :303-305` transitions to
// `SCE_OSCRIPT_DOC_COMMENT` which stays active across lines
// until a `#endif` closes it at `:310-319`.
//
// **String literals.** Both single-quote (`'...'`) and
// double-quote (`"..."`) strings supported at `:271-292`, with
// doubled-quote escaping (`''` inside single, `""` inside
// double). Strings must terminate on the same line —
// unterminated strings roll to DEFAULT at end-of-line.
//
// **Fold** at `:419-534` is keyword-driven, NOT indentation-
// based. `UpdateKeywordFoldLevel` at `:435-450` opens on
// `if`/`for`/`switch`/`function`/`while`/`repeat` and closes
// on `end`/`until` (fires only when style ==
// SCE_OSCRIPT_KEYWORD per the guard at `:501-508`).
// `UpdatePreprocessorFoldLevel` at `:419-433` handles
// `#ifdef`/`#ifndef`/`#endif` block folding. Block-comment
// style transitions at `:478-486` and line-comment
// transitions at `:487-494` also emit fold levels.
//
// **Six-class descriptor is the widest of Phase 4.5** (tied
// with Forth's 6 and Erlang's 6). LexOScript uses class-slot
// granularity to express OScript's rich vocabulary
// categorization: syntactic keywords, literal constants
// (TRUE/FALSE/undefined), word operators (and/or/not), built-in
// types (Integer/String), library functions (Echo/Length), and
// Livelink singletons (DAPI/WAPI).
//
// Style semantics (paint-loop citations reference LexOScript.cxx):
//
//   - SCE_OSCRIPT_DEFAULT (0) — whitespace / unclassified /
//     bare identifier that missed all wordlist probes.
//     Framework convention: leave unmapped.
//   - SCE_OSCRIPT_LINE_COMMENT (1) — `//`-to-EOL line
//     comment. Entry at `:328-330`, exit at `:298-301`.
//   - SCE_OSCRIPT_BLOCK_COMMENT (2) — `/* ... */` block
//     comment. Entry at `:331-333`, exit at `:293-297`.
//   - SCE_OSCRIPT_DOC_COMMENT (3) — `#ifdef DOC ... #endif`
//     conditional-preprocessor documentation block. Entry
//     via `:303-305` after PREPROCESSOR detects
//     `#ifdef DOC`, exit at `:315-319`.
//   - SCE_OSCRIPT_PREPROCESSOR (4) — `#`-directive line
//     (except the DOC-comment starter). Entry at `:334-335`,
//     exit at `:306-308` on line end.
//   - SCE_OSCRIPT_NUMBER (5) — decimal / floating / signed-
//     exponent numeric literal. Entry at `:340-344`, exit
//     at `:267-270`.
//   - SCE_OSCRIPT_SINGLEQUOTE_STRING (6) — `'...'` string.
//     Entry at `:324-325`, exit at `:271-281`.
//   - SCE_OSCRIPT_DOUBLEQUOTE_STRING (7) — `"..."` string.
//     Entry at `:326-327`, exit at `:282-292`.
//   - SCE_OSCRIPT_CONSTANT (8) — identifier that hit
//     wordlist class 1. Entry via `:169-170` in the
//     no-parenthesis path.
//   - SCE_OSCRIPT_IDENTIFIER (9) — transient collect state
//     entered at `:356-357`. Settles to a specific style at
//     `:232-250`. Framework convention: leave unmapped so
//     unmatched bare identifiers paint at STYLE_DEFAULT.
//   - SCE_OSCRIPT_GLOBAL (10) — `$xxx` or `$$xxx`
//     process/thread-global variable. Entry at `:336-339`,
//     exit at `:251-254`.
//   - SCE_OSCRIPT_KEYWORD (11) — identifier that hit
//     wordlist class 0. Entry via `:144-145` (parenthesis
//     path) or `:167-168` (no-paren path).
//   - SCE_OSCRIPT_OPERATOR (12) — symbolic operator from
//     `IsOperator` at `:83-85` (`%^&*()-+={}[]:;<>,/?!.~|\`)
//     OR word operator via wordlist class 2 hit at `:146-147`
//     / `:171-172`. Entry at `:358-359`.
//   - SCE_OSCRIPT_LABEL (13) — column-0 identifier followed
//     by `:`. Auto-styled at `:241-243` in the IDENTIFIER
//     settle path.
//   - SCE_OSCRIPT_TYPE (14) — identifier that hit wordlist
//     class 3. Entry via `:173-174` in the no-paren path.
//   - SCE_OSCRIPT_FUNCTION (15) — identifier that hit
//     wordlist class 4. Entry via `:148-149` (paren path) or
//     `:175-176` (no-paren path).
//   - SCE_OSCRIPT_OBJECT (16) — identifier followed by `.`
//     that hit wordlist class 5. Entry at `:163-164`.
//   - SCE_OSCRIPT_PROPERTY (17) — `.identifier` after an
//     object-access dot. Entry at `:345-355`, exit at
//     `:255-266`.
//   - SCE_OSCRIPT_METHOD (18) — identifier immediately
//     followed by `(` that missed all wordlist probes at
//     `:150-151` (default for un-classified call sites), OR
//     PROPERTY upgraded to METHOD when a property span is
//     followed by `(` at `:262-263`.
pub const SCE_OSCRIPT_DEFAULT: usize = 0;
pub const SCE_OSCRIPT_LINE_COMMENT: usize = 1;
pub const SCE_OSCRIPT_BLOCK_COMMENT: usize = 2;
pub const SCE_OSCRIPT_DOC_COMMENT: usize = 3;
pub const SCE_OSCRIPT_PREPROCESSOR: usize = 4;
pub const SCE_OSCRIPT_NUMBER: usize = 5;
pub const SCE_OSCRIPT_SINGLEQUOTE_STRING: usize = 6;
pub const SCE_OSCRIPT_DOUBLEQUOTE_STRING: usize = 7;
pub const SCE_OSCRIPT_CONSTANT: usize = 8;
pub const SCE_OSCRIPT_IDENTIFIER: usize = 9;
pub const SCE_OSCRIPT_GLOBAL: usize = 10;
pub const SCE_OSCRIPT_KEYWORD: usize = 11;
pub const SCE_OSCRIPT_OPERATOR: usize = 12;
pub const SCE_OSCRIPT_LABEL: usize = 13;
pub const SCE_OSCRIPT_TYPE: usize = 14;
pub const SCE_OSCRIPT_FUNCTION: usize = 15;
pub const SCE_OSCRIPT_OBJECT: usize = 16;
pub const SCE_OSCRIPT_PROPERTY: usize = 17;
pub const SCE_OSCRIPT_METHOD: usize = 18;

// LexRebol style indices. 29 contiguous slots (0..=28) for
// REBOL — Carl Sassenrath's homoiconic message-passing
// dialect language (extensions `.reb` / `.rebol`). Constants
// mirror `SciLexer.h:1195-1223` verbatim. Dispatches
// SCLEX_REBOL (= 71, per `SciLexer.h:87`) via an
// **eight-class wordlist** at
// `vendor\lexilla\lexers\LexRebol.cxx:74-81`. The descriptor
// registration at `:320-323` declares only `{"Keywords", 0}`
// (single-class), but the paint loop `ColouriseRebolDoc`
// accesses ALL EIGHT wordlist slots via
// `keywordlists[0..7]` and emits the corresponding
// `SCE_REBOL_WORD..WORD8` states from `:162-178`. Notepad++
// / SciTE populate the additional slots via
// `SCI_SETKEYWORDS(N, ...)` even though only slot 0's
// descriptor label is exposed by upstream.
//
// **Reverse-first-match-wins cascade.** Unlike every prior
// Phase 4.5 wordlist descriptor (Erlang / Forth /
// ESCRIPT / MMIXAL / Nim / OScript), LexRebol probes classes
// **7 → 6 → 5 → 4 → 3 → 2 → 1 → 0** in REVERSE order at
// `:162-178`. Higher-numbered classes SHADOW lower-numbered
// ones on collision. Cross-class disjointness is still
// checked by the invariant test to prevent inadvertent
// masking.
//
// **Case-INSENSITIVE.** `LexRebol.cxx:160` calls
// `sc.GetCurrentLowered(s, sizeof(s))` before every wordlist
// probe. REBOL source may write words in any case (`If`,
// `IF`, `if` all valid); wordlist tokens must be lowercase.
//
// **Very wide identifier alphabet.** `IsAWordChar` at
// `:37-39` accepts alnum + `? ! . ' + - * & | = _ ~`, and
// `IsAWordStart` at `:41-44` additionally allows `+`/`-`/`.`
// as first byte (when NOT followed by digit — which would
// tokenize as NUMBER). Consequence: REBOL word names can be
// e.g. `+`/`-`/`?`/`!`/`empty?`/`found?`/`type-of`/`+->`/
// symbolic pseudo-operators. Wordlist entries must match
// this alphabet.
//
// **Homoiconic value literals** — REBOL treats many syntactic
// forms as first-class values, each with its own SCE_ state.
// The IDENTIFIER settle path at `:145-153` retroactively
// re-classifies collected identifier spans:
//   - `identifier:` (colon suffix, no following space) →
//     `SCE_REBOL_URL`.
//   - `identifier@` → `SCE_REBOL_EMAIL`.
//   - `identifier$` → `SCE_REBOL_MONEY`.
// Then at `:156-183` if IDENTIFIER survived without such a
// suffix, the wordlist probe fires (settling to
// WORD..WORD8) or falls back to `SCE_REBOL_IDENTIFIER`.
//
// **NUMBER post-settle.** At `:185-197`, a NUMBER-state span
// upgrades to:
//   - `PAIR` on `x` (e.g. `640x480`).
//   - `TIME` on `:` (e.g. `12:30`).
//   - `DATE` on `-` or `/` (e.g. `12-Jun-2024`, `2024/06/12`).
//   - `TUPLE` on multiple `.` (e.g. `1.2.3`).
//
// **Braced string with nesting.** `{...}` strings can nest
// balanced braces per `:206-213` — `stringLevel` tracks
// depth so the outer `}` terminates only when nesting drops
// to zero. Same for the `comment {...}` block comment.
//
// **Binary literal.** `#{...}` / `2#{...}` / `NN#{...}`
// hexadecimal / binary literals recognised by
// `IsBinaryStart` at `:65-69` — a `#{` with optional
// leading base (2 / 16 / 64) enters `SCE_REBOL_BINARY`.
//
// **Preface state.** `SCE_REBOL_PREFACE` (3) covers text
// BEFORE the first `REBOL [...]` header block — REBOL
// convention treats everything before the header as
// documentation prose, not code. Entered at
// `:100` (initial state).
//
// **Fold** at `:275+` uses brace / bracket / paren nesting
// levels, plus block-comment style transitions.
//
// Style semantics (paint-loop citations reference LexRebol.cxx):
//
//   - SCE_REBOL_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_REBOL_COMMENTLINE (1) — `;`-to-EOL line comment.
//   - SCE_REBOL_COMMENTBLOCK (2) — `comment {...}` block
//     comment. Entered when a `{...}` string starts while the
//     `blockComment` flag is set. That flag is updated at
//     `:161` via `blockComment = strcmp(s, "comment") == 0;`
//     — an UNCONDITIONAL byte-exact test on the collected
//     identifier text that runs BEFORE any wordlist probe.
//     Whether `comment` is present in any keyword class has
//     no bearing on the flag flip; wordlist membership only
//     affects how the `comment` word itself is styled.
//   - SCE_REBOL_PREFACE (3) — preamble text before the
//     first `REBOL [...]` header block. Entered at `:100`.
//   - SCE_REBOL_OPERATOR (4) — symbolic operator from
//     `IsAnOperator` at `:46-63` (`+ - * / < > = ?` alone
//     or paired forms `** // <= >= == =? ??` and `<>`).
//   - SCE_REBOL_CHARACTER (5) — `#"..."` character literal.
//   - SCE_REBOL_QUOTEDSTRING (6) — `"..."` string literal.
//     Exit on unescaped `"` at `:203-205`.
//   - SCE_REBOL_BRACEDSTRING (7) — `{...}` multi-line
//     string, nesting-aware.
//   - SCE_REBOL_NUMBER (8) — numeric literal. Post-settles
//     to PAIR / TIME / DATE / TUPLE on suffix.
//   - SCE_REBOL_PAIR (9) — `WxH` pair value.
//   - SCE_REBOL_TUPLE (10) — `x.y.z` tuple value.
//   - SCE_REBOL_BINARY (11) — `#{...}` binary literal.
//   - SCE_REBOL_MONEY (12) — `$xxx` money value.
//   - SCE_REBOL_ISSUE (13) — `#xxx` issue value.
//   - SCE_REBOL_TAG (14) — `<xxx>` tag value.
//   - SCE_REBOL_FILE (15) — `%xxx` file path value.
//   - SCE_REBOL_EMAIL (16) — `foo@bar` email value.
//   - SCE_REBOL_URL (17) — `scheme:xxx` URL value.
//   - SCE_REBOL_DATE (18) — `12-Jun-2024` date value.
//   - SCE_REBOL_TIME (19) — `12:30` time value.
//   - SCE_REBOL_IDENTIFIER (20) — transient collect state.
//     Settles to WORD..WORD8 / URL / EMAIL / MONEY at
//     `:145-183`. Framework convention: leave unmapped.
//   - SCE_REBOL_WORD (21) — class-0 wordlist hit.
//   - SCE_REBOL_WORD2 (22) — class-1 wordlist hit.
//   - SCE_REBOL_WORD3 (23) — class-2 wordlist hit.
//   - SCE_REBOL_WORD4 (24) — class-3 wordlist hit.
//   - SCE_REBOL_WORD5 (25) — class-4 wordlist hit.
//   - SCE_REBOL_WORD6 (26) — class-5 wordlist hit.
//   - SCE_REBOL_WORD7 (27) — class-6 wordlist hit.
//   - SCE_REBOL_WORD8 (28) — class-7 wordlist hit.
pub const SCE_REBOL_DEFAULT: usize = 0;
pub const SCE_REBOL_COMMENTLINE: usize = 1;
pub const SCE_REBOL_COMMENTBLOCK: usize = 2;
pub const SCE_REBOL_PREFACE: usize = 3;
pub const SCE_REBOL_OPERATOR: usize = 4;
pub const SCE_REBOL_CHARACTER: usize = 5;
pub const SCE_REBOL_QUOTEDSTRING: usize = 6;
pub const SCE_REBOL_BRACEDSTRING: usize = 7;
pub const SCE_REBOL_NUMBER: usize = 8;
pub const SCE_REBOL_PAIR: usize = 9;
pub const SCE_REBOL_TUPLE: usize = 10;
pub const SCE_REBOL_BINARY: usize = 11;
pub const SCE_REBOL_MONEY: usize = 12;
pub const SCE_REBOL_ISSUE: usize = 13;
pub const SCE_REBOL_TAG: usize = 14;
pub const SCE_REBOL_FILE: usize = 15;
pub const SCE_REBOL_EMAIL: usize = 16;
pub const SCE_REBOL_URL: usize = 17;
pub const SCE_REBOL_DATE: usize = 18;
pub const SCE_REBOL_TIME: usize = 19;
pub const SCE_REBOL_IDENTIFIER: usize = 20;
pub const SCE_REBOL_WORD: usize = 21;
pub const SCE_REBOL_WORD2: usize = 22;
pub const SCE_REBOL_WORD3: usize = 23;
pub const SCE_REBOL_WORD4: usize = 24;
pub const SCE_REBOL_WORD5: usize = 25;
pub const SCE_REBOL_WORD6: usize = 26;
pub const SCE_REBOL_WORD7: usize = 27;
pub const SCE_REBOL_WORD8: usize = 28;

// LexRegistry style indices. 13 contiguous slots (0..=12) for
// Windows Registry Editor export files (extension `.reg`).
// Constants mirror `SciLexer.h:1843-1855` verbatim. Dispatches
// SCLEX_REGISTRY (= 115, per `SciLexer.h:131`) via
// `vendor/lexilla/lexers/LexRegistry.cxx:415-418`.
//
// **Zero wordlists.** `RegistryWordListDesc[]` at
// `LexRegistry.cxx:38-40` is `{ 0 }` — a bare null terminator.
// `LexerRegistry::WordListSet` at `:191-193` unconditionally
// returns -1, REJECTING any attempt to install keywords via
// `SCI_SETKEYWORDS`. Registry is a pure syntax-driven lexer —
// classification is line-shape based, not identifier-lookup
// based. This is the strongest zero-wordlist contract in Phase
// 4.5: [`SCE_PROPS_KEY`] (INI / Properties) ignores wordlists
// but tolerates the install; Registry rejects it outright.
// Framework consequence: `REGISTRY_THEME.keywords` MUST be
// empty; installing anything is a no-op at best and gets
// silently dropped by Scintilla on the `-1` return.
//
// **State-machine driven.** `LexerRegistry::Lex` at `:213-355`
// runs a StyleContext state machine with lookahead helpers:
//   - `AtValueName` at `:98-119` — string `"..."` is a
//     VALUENAME iff the closing `"` is followed by `=` (only
//     whitespace between). Otherwise it's a plain STRING.
//   - `AtKeyPathEnd` at `:121-135` — `]` closes a keypath only
//     if no further `]` appears before EOL (guards against
//     GUIDs / nested brackets in the middle of a keypath).
//   - `AtValueType` at `:67-79` — a wordStart alpha token
//     (`dword` / `hex` / `hex(b)`) becomes VALUETYPE iff a `:`
//     appears within the next 10 chars.
//   - `AtGUID` at `:137-161` — a `{` opens a GUID span iff
//     exactly `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` follows.
//   - `IsNextNonWhitespace` at `:81-95` — the `[` opens a
//     DELETEDKEY (`[-HKEY...]`) iff `-` is the next
//     non-whitespace character, else ADDEDKEY.
//
// **The `highlight` flag** at `:222-229, :338, :347` gates the
// operator / hexdigit emission on the LHS of an `=` assignment.
// Set true after any `=` or `@` (default-value marker); reset
// per line unless the previous line ended with `\` (continuation).
// Without this, arbitrary hex-looking prose in a comment before
// `=` would paint as HEXDIGIT — the flag ensures HEXDIGIT only
// paints in the value tail of `key=value` lines.
//
// **String → GUID → String nesting.** Inside a VALUENAME /
// STRING at `:245-249`, encountering `{` transitions to
// STRING_GUID if `AtGUID` confirms a well-formed GUID follows.
// The GUID's closing `}` at `:300-303` returns to `beforeGUID`
// (the saved outer VALUENAME/STRING state), and the outer
// string's closing `"` still terminates normally. Same nesting
// for ADDEDKEY / DELETEDKEY → KEYPATH_GUID at `:279-284` and
// return-to-outer at `:300-303` (shared case-label fall-through
// with STRING_GUID at `:298-299`); outer-keypath exit at
// `:307-310` fires only after the return-to-outer has restored
// the state to ADDEDKEY / DELETEDKEY.
//
// **`\`-escape sequences.** At `:241-244`, backslash inside a
// string enters ESCAPED, saving the outer state. `:287-296`
// handles the escape body: `\"` closes the outer string, `\\`
// consumes the second backslash, anything else returns to the
// outer state after one char. Same behaviour for backslashes
// inside GUID strings via `:311-315`.
//
// **Format parameters.** `%0`, `%1`, `%*` inside a STRING (not
// VALUENAME) enter PARAMETER at `:251-255`. Single-char span
// per `:258-263` — one digit / `*` after the `%`, then return
// to STRING, or terminate the whole string if followed by `"`.
//
// **Fold** at `:358-413` is header-driven: any line containing
// a KEYPATH-styled span becomes a fold header (level BASE +
// HEADERFLAG). Following non-header lines fold into the
// previous header's body. Similar to LexOthers' FoldPropsDoc.
//
// Style semantics (paint-loop citations reference LexRegistry.cxx):
//
//   - SCE_REG_DEFAULT (0) — whitespace / unclassified /
//     free text on lines without an `=` or `[` opener.
//     Framework convention: leave unmapped.
//   - SCE_REG_COMMENT (1) — `;`-to-EOL line comment. Entry at
//     `:322-323`, exit at `:231-234`.
//   - SCE_REG_VALUENAME (2) — `"..."` string on LHS of `=`.
//     Distinguished from STRING by `AtValueName` lookahead at
//     `:325-329`.
//   - SCE_REG_STRING (3) — `"..."` string on RHS of `=` or
//     unassigned. Entered at `:327-329` when `AtValueName`
//     returns false.
//   - SCE_REG_HEXDIGIT (4) — hex digit run in the value tail
//     (post-`=` on same line). Entry at `:344-346`. Covers
//     comma-separated `41,42,43` binary data, dword hex
//     `00000001`, etc.
//   - SCE_REG_VALUETYPE (5) — `dword` / `hex` / `hex(b)` /
//     `hex(7)` value-type prefix before the `:` in
//     `Name=dword:...`. Entered at `:340-343`.
//   - SCE_REG_ADDEDKEY (6) — `[HKEY_...\path]` full keypath
//     declaration (primary structural anchor). Entered at
//     `:334`.
//   - SCE_REG_DELETEDKEY (7) — `[-HKEY_...\path]` deletion
//     directive. Entered at `:332` when `IsNextNonWhitespace`
//     detects `-` after `[`.
//   - SCE_REG_ESCAPED (8) — `\"` / `\\` escape sequences
//     inside strings. Transient state that returns to the
//     outer STRING/VALUENAME after consuming the escape body.
//   - SCE_REG_KEYPATH_GUID (9) — `{XXXXXXXX-...-XXXXXXXXXXXX}`
//     GUID span inside an ADDEDKEY / DELETEDKEY. Entered at
//     `:279-283`, exits back to the outer keypath at `:300-303`.
//   - SCE_REG_STRING_GUID (10) — same GUID recognition but
//     inside a VALUENAME / STRING. Entered at `:245-249`.
//   - SCE_REG_PARAMETER (11) — `%0` / `%1` / `%*` format
//     parameter inside a STRING (not VALUENAME). Entered at
//     `:251-255`.
//   - SCE_REG_OPERATOR (12) — one of `-,.=:\\@()` (the
//     `setOperators` set at `:219`) in the value-tail region.
//     Entered at `:348-350`.
pub const SCE_REG_DEFAULT: usize = 0;
pub const SCE_REG_COMMENT: usize = 1;
pub const SCE_REG_VALUENAME: usize = 2;
pub const SCE_REG_STRING: usize = 3;
pub const SCE_REG_HEXDIGIT: usize = 4;
pub const SCE_REG_VALUETYPE: usize = 5;
pub const SCE_REG_ADDEDKEY: usize = 6;
pub const SCE_REG_DELETEDKEY: usize = 7;
pub const SCE_REG_ESCAPED: usize = 8;
pub const SCE_REG_KEYPATH_GUID: usize = 9;
pub const SCE_REG_STRING_GUID: usize = 10;
pub const SCE_REG_PARAMETER: usize = 11;
pub const SCE_REG_OPERATOR: usize = 12;

// LexSpice style indices. 9 contiguous slots (0..=8) for SPICE
// (Simulation Program with Integrated Circuit Emphasis) circuit
// files (extensions `.sp` / `.spice`). Constants mirror
// `SciLexer.h:1335-1343` verbatim. Dispatches SCLEX_SPICE
// (= 78, per `SciLexer.h:94`) via
// `vendor/lexilla/lexers/LexSpice.cxx:49`.
//
// **Three-class wordlist descriptor.** `spiceWordListDesc[]` at
// `LexSpice.cxx:42-46` declares three named slots: `"Keywords"`
// (SPICE simulator directive stems), `"Keywords2"` (expression
// functions), `"Keywords3"` (model types / source waveforms /
// sweep / options). All three extracted from `keywordlists[]`
// at `:143-145` inside `ColouriseDocument`, then passed as
// separate `WordList` refs into the `ColouriseWord` call at
// `:173`.
//
// **First-match-wins cascade** at `:113-130` probes class 0 → 1
// → 2 in FORWARD order (unlike REBOL's reverse cascade). Higher-
// class duplicates silently masked by lower-class siblings —
// cross-class disjointness invariant test enforces strict
// non-overlap.
//
// **Case-INsensitive.** `LexSpice.cxx:110` lowercases every
// collected byte before wordlist lookup (`word +=
// static_cast<char>(tolower(sc.ch));`). SPICE source may write
// `.TRAN`/`.tran`/`.Tran` interchangeably; wordlist entries must
// be lowercase.
//
// **Dot-prefix stripping.** `IsDelimiterCharacter` at `:179-201`
// includes `.` — so `.tran` parses as `SCE_SPICE_DELIMITER` +
// `SCE_SPICE_KEYWORD` (bare stem `tran`), not one keyword token
// with a literal dot. Wordlists hold the dotless stems.
//
// **Comment convention.** Two entry paths at `:160`:
//   - Line-start `*` — traditional Berkeley SPICE line-comment
//     (any line starting with `*` in column 0).
//   - Mid-line `*~` — SciTE / LTspice extended in-line comment
//     starter.
// Both consume to end-of-line at `:64-69`.
//
// **Line-state carry.** `apostropheStartsAttribute` at `:148,
// :155` is persisted in `styler.SetLineState(lineCurrent, ...)`
// so incremental re-lex from mid-file preserves the flag that
// gates whether an apostrophe opens an attribute vs is a bare
// tick delimiter. Wordlist hits toggle this except for the
// literal `all` token (special-cased at `:115-117, :121-123,
// :127-129`).
//
// **SCE_SPICE_VALUE (7) is a DEAD STATE.** Verified by
// exhaustive grep of `LexSpice.cxx`: the paint loop has ZERO
// call sites that emit `SCE_SPICE_VALUE` (neither `SetState`,
// `ChangeState`, nor `ForwardSetState`). The slot is reserved
// in `SciLexer.h` but never entered at runtime. Framework
// convention: leave unmapped since no bytes ever paint into it;
// documented so a future Lexilla update wiring the slot won't
// surprise a maintainer.
//
// **SCE_SPICE_IDENTIFIER (1) is a transient collect state**
// entered at `:107` while `ColouriseWord` gathers bytes until
// hitting a separator / delimiter / line-end. If any of the
// three wordlist probes match at `:113-130`, `ChangeState`
// retroactively repaints the whole span to the matched
// `SCE_SPICE_KEYWORD*` slot. If none match, the span stays
// as `SCE_SPICE_IDENTIFIER` (visible-state for unmatched
// bare identifiers). Framework convention: leave unmapped so
// unmatched user-defined identifiers (net names, component
// designators like `Vin1`, subckt-parameter references) paint
// at `STYLE_DEFAULT`.
//
// Style semantics (paint-loop citations reference LexSpice.cxx):
//
//   - SCE_SPICE_DEFAULT (0) — whitespace / unclassified.
//     Entered on every line-start reset at `:157`, on the
//     unconditional post-cascade fall-through at `:131` (runs
//     after every `ColouriseWord` return whether or not a
//     wordlist matched — `ChangeState` at `:114`/`:120`/`:126`
//     retro-repaints the span but the transition to DEFAULT
//     for the next byte is unconditional), on every
//     `ColouriseWhiteSpace` visit at `:101-102`, and on every
//     `ColouriseDelimiter` / `ColouriseNumber` exit at `:74` /
//     `:97`. Framework convention: leave unmapped.
//   - SCE_SPICE_IDENTIFIER (1) — transient collect state; see
//     above. Framework convention: leave unmapped.
//   - SCE_SPICE_KEYWORD (2) — class-0 wordlist hit. Emitted
//     via `ChangeState` at `:114`.
//   - SCE_SPICE_KEYWORD2 (3) — class-1 wordlist hit. Emitted
//     via `ChangeState` at `:120`.
//   - SCE_SPICE_KEYWORD3 (4) — class-2 wordlist hit. Emitted
//     via `ChangeState` at `:126`.
//   - SCE_SPICE_NUMBER (5) — numeric literal. Entered at `:80`
//     on digit-lead or `#`-lead (`:169`), exits at `:97` after
//     exponent-with-sign handling at `:88-96`.
//   - SCE_SPICE_DELIMITER (6) — single-char span for one of
//     `& ' ( ) * + , - . / : ; < = > |` per
//     `IsDelimiterCharacter` at `:179-201`. Entered at `:73`,
//     exits to DEFAULT at `:74`.
//   - SCE_SPICE_VALUE (7) — DEAD STATE, never emitted. See
//     above.
//   - SCE_SPICE_COMMENTLINE (8) — line-comment span. Entered
//     at `:65`, consumes to EOL at `:66-68`.
pub const SCE_SPICE_DEFAULT: usize = 0;
pub const SCE_SPICE_IDENTIFIER: usize = 1;
pub const SCE_SPICE_KEYWORD: usize = 2;
pub const SCE_SPICE_KEYWORD2: usize = 3;
pub const SCE_SPICE_KEYWORD3: usize = 4;
pub const SCE_SPICE_NUMBER: usize = 5;
pub const SCE_SPICE_DELIMITER: usize = 6;
pub const SCE_SPICE_VALUE: usize = 7;
pub const SCE_SPICE_COMMENTLINE: usize = 8;

// LexTxt2tags style indices. 26 contiguous slots (0..=25) for
// txt2tags — a lightweight markup / plain-text-to-many-formats
// converter language (extension `.t2t`). Constants mirror
// `SciLexer.h:1588-1613` verbatim. Dispatches SCLEX_TXT2TAGS
// (= 99, per `SciLexer.h:115`) via
// `vendor/lexilla/lexers/LexTxt2tags.cxx:479`.
//
// **Zero wordlists.** The `LexerModule` registration at `:479`
// has the 3-argument constructor `lmTxt2tags(SCLEX_TXT2TAGS,
// ColorizeTxt2tagsDoc, "txt2tags")` — no `wordListDesc`
// argument passed at all. The paint function signature at `:108`
// declares `WordList **, ` with an UNNAMED parameter that is
// never referenced in the body. txt2tags is a pure
// structural-markup lexer classifying tokens by line-prefix and
// delimiter-pair rules, not by identifier lookup. Framework
// consequence: `TXT2TAGS_THEME.keywords` MUST be empty; there is
// nothing to install and nowhere for the lexer to consume it.
//
// **State-machine driven.** `ColorizeTxt2tagsDoc` at `:107-476`
// runs a `StyleContext` state machine keyed off the current
// state plus the byte pattern at `sc.ch` / `sc.chNext` /
// `sc.Match(...)`. Two categories of state transitions:
//   - Line-scope entries at `:209-345` (guarded by `sc.state ==
//     SCE_TXT2TAGS_LINE_BEGIN`): headers (`===` / `+++`), lists,
//     blockquotes, code blocks, options / preproc / postproc /
//     comments (all `%!`-prefixed variants).
//   - Inline entries at `:402-470` (guarded by `sc.state ==
//     SCE_TXT2TAGS_DEFAULT`): links (`[text url]`), strong
//     (`**bold**`), emphasis (`//italic//` / `__underline__`),
//     strikeout (`--strike--`), inline code (`` `` ``), tables
//     (`|row|`).
//
// **`\`-escape.** At `:120-123`, `\` inside any state consumes
// the next byte via `sc.Forward()` and `continue` — matches
// Markdown-style escape convention. No dedicated ESCAPED state
// (unlike LexRegistry's `SCE_REG_ESCAPED`).
//
// **Line-prefix transient states.** Bytes on a line's leading
// whitespace pass through `SCE_TXT2TAGS_PRECHAR` (12) or
// `SCE_TXT2TAGS_LINE_BEGIN` (1) during scanning. Both settle to
// a specific role state before end-of-line, or fall through to
// `SCE_TXT2TAGS_DEFAULT` (0). Framework convention: leave both
// unmapped so pre-role whitespace / newlines paint at
// `STYLE_DEFAULT`.
//
// **SCE_TXT2TAGS_STRONG2 (3) is a DEAD STATE.** Verified by
// exhaustive grep: zero call sites emit `SCE_TXT2TAGS_STRONG2`
// (no `SetState`, `ChangeState`, or `ForwardSetState`). The
// slot is reserved in `SciLexer.h` presumably as an
// alternative-strong-syntax escape hatch that upstream never
// finished wiring. Framework convention: leave unmapped since
// no bytes ever paint into it.
//
// Style semantics (paint-loop citations reference LexTxt2tags.cxx):
//
//   - SCE_TXT2TAGS_DEFAULT (0) — plain body text. Entered on
//     every exit from a role state. Framework convention:
//     leave unmapped.
//   - SCE_TXT2TAGS_LINE_BEGIN (1) — transient line-start scan
//     state. Entered on newline at `:339-340, :469`, on every
//     end-of-role return at `:59, :96, :128, :147, :156, :163,
//     :172, :181, :190, :202, :352`, and at initial-state on
//     entry to a new line. Framework convention: leave
//     unmapped.
//   - SCE_TXT2TAGS_STRONG1 (2) — `**bold**` inline strong
//     text. Entered at `:449`, exits at `:164-167` on closing
//     `**`.
//   - SCE_TXT2TAGS_STRONG2 (3) — DEAD STATE. Framework
//     convention: leave unmapped.
//   - SCE_TXT2TAGS_EM1 (4) — `//italic//` inline emphasis.
//     Entered at `:454`, exits at `:173-176` on closing `//`.
//   - SCE_TXT2TAGS_EM2 (5) — `__underline__` inline emphasis.
//     Entered at `:458`, exits at `:182-185` on closing `__`.
//   - SCE_TXT2TAGS_HEADER1 (6) — `= H1 =` or `+ H1 +` single
//     `=`/`+` header. Entered at `:244-246, :295-297`, exits
//     on newline via the shared header handler at `:348-353`.
//   - SCE_TXT2TAGS_HEADER2 (7) — `== H2 ==` or `++ H2 ++`.
//     Entered at `:231-234, :272-275`.
//   - SCE_TXT2TAGS_HEADER3 (8) — `=== H3 ===` or `+++ H3 +++`.
//     Entered at `:225-229, :266-270`.
//   - SCE_TXT2TAGS_HEADER4 (9) — `==== H4 ====` or `++++ H4 ++++`.
//     Entered at `:220-224, :261-265`.
//   - SCE_TXT2TAGS_HEADER5 (10) — `===== H5 =====` or
//     `+++++ H5 +++++`. Entered at `:215-219, :256-260`.
//   - SCE_TXT2TAGS_HEADER6 (11) — `====== H6 ======` or
//     `++++++ H6 ++++++`. Entered at `:210-214, :251-255`.
//   - SCE_TXT2TAGS_PRECHAR (12) — transient leading-whitespace
//     scan state. Entered at `:240, :330, :337, :343`. Settles
//     to `SCE_TXT2TAGS_DEFAULT` at `:395-396`, or to
//     `SCE_TXT2TAGS_ULIST_ITEM` / `SCE_TXT2TAGS_OLIST_ITEM` at
//     `:374, :384-386`, or to `SCE_TXT2TAGS_BLOCKQUOTE` at
//     `:360`. Framework convention: leave unmapped.
//   - SCE_TXT2TAGS_ULIST_ITEM (13) — `- item` unordered list
//     marker. Entered at `:374`. Single-marker span; exits
//     immediately at `:375` via `ForwardSetState(DEFAULT)`.
//   - SCE_TXT2TAGS_OLIST_ITEM (14) — `1. item` numbered list
//     or `+ item` alternate ordered list. Entered at `:287,
//     :335, :384`. Exits at `:289, :337, :386` via
//     `SetState(DEFAULT)` or `SetState(PRECHAR)`.
//   - SCE_TXT2TAGS_BLOCKQUOTE (15) — `""" quote` blockquote
//     marker. Entered at `:360`, exits at `:126-129` via
//     `Forward(2) + SetState(LINE_BEGIN)`.
//   - SCE_TXT2TAGS_STRIKEOUT (16) — `--strike--` inline
//     strikeout. Entered at `:463`, exits at `:203-206` on
//     closing `--`.
//   - SCE_TXT2TAGS_HRULE (17) — 20+ hyphens on a line by
//     themselves (horizontal rule separator). Entered at
//     `:94-97` via `IsValidHrule` at `:81-105`.
//     **Hyphen-only** — the sole call site is `:370`,
//     gated on `sc.ch == '-'`. A line of 20+ `=`
//     characters is consumed by the header-match cascade
//     at `:209-248` and paints as HEADER6 (persisting via
//     the shared exit handler at `:348-353`), NOT HRULE.
//   - SCE_TXT2TAGS_LINK (18) — `[text]` / `[text url]` /
//     `![image]` link and image markup. Entered at `:431`,
//     exits at `:436` via `ForwardSetState(DEFAULT)`.
//   - SCE_TXT2TAGS_CODE (19) — `|table row|` pipe-delimited
//     table row. Entered at `:445`, exits at `:157-158` on
//     closing `|`.
//   - SCE_TXT2TAGS_CODE2 (20) — `` `` inline code`` `` `
//     double-backtick inline code span. Entered at `:441`,
//     exits at `:148-151` on closing `` `` ``.
//   - SCE_TXT2TAGS_CODEBK (21) — `` ``` `` code block. Entered
//     at `:302-307` via `HasPrevLineContent` check, exits at
//     `:191-197` on closing `` ``` `` at line start.
//   - SCE_TXT2TAGS_COMMENT (22) — `%`-to-EOL line comment.
//     Entered at `:324-326`, exits via `FollowToLineEnd` at
//     `:141-143`.
//   - SCE_TXT2TAGS_OPTION (23) — `%!key : value` document
//     option. Entered at `:319-321`, exits via `FollowToLineEnd`
//     at `:131-133`.
//   - SCE_TXT2TAGS_PREPROC (24) — `%!preproc: pattern
//     replacement` preprocessor directive. Entered at
//     `:311-313`, exits via `FollowToLineEnd` at `:137-139`.
//   - SCE_TXT2TAGS_POSTPROC (25) — `%!postproc: pattern
//     replacement` postprocessor directive. Entered at
//     `:315-317`, exits via `FollowToLineEnd` at `:134-136`.
pub const SCE_TXT2TAGS_DEFAULT: usize = 0;
pub const SCE_TXT2TAGS_LINE_BEGIN: usize = 1;
pub const SCE_TXT2TAGS_STRONG1: usize = 2;
pub const SCE_TXT2TAGS_STRONG2: usize = 3;
pub const SCE_TXT2TAGS_EM1: usize = 4;
pub const SCE_TXT2TAGS_EM2: usize = 5;
pub const SCE_TXT2TAGS_HEADER1: usize = 6;
pub const SCE_TXT2TAGS_HEADER2: usize = 7;
pub const SCE_TXT2TAGS_HEADER3: usize = 8;
pub const SCE_TXT2TAGS_HEADER4: usize = 9;
pub const SCE_TXT2TAGS_HEADER5: usize = 10;
pub const SCE_TXT2TAGS_HEADER6: usize = 11;
pub const SCE_TXT2TAGS_PRECHAR: usize = 12;
pub const SCE_TXT2TAGS_ULIST_ITEM: usize = 13;
pub const SCE_TXT2TAGS_OLIST_ITEM: usize = 14;
pub const SCE_TXT2TAGS_BLOCKQUOTE: usize = 15;
pub const SCE_TXT2TAGS_STRIKEOUT: usize = 16;
pub const SCE_TXT2TAGS_HRULE: usize = 17;
pub const SCE_TXT2TAGS_LINK: usize = 18;
pub const SCE_TXT2TAGS_CODE: usize = 19;
pub const SCE_TXT2TAGS_CODE2: usize = 20;
pub const SCE_TXT2TAGS_CODEBK: usize = 21;
pub const SCE_TXT2TAGS_COMMENT: usize = 22;
pub const SCE_TXT2TAGS_OPTION: usize = 23;
pub const SCE_TXT2TAGS_PREPROC: usize = 24;
pub const SCE_TXT2TAGS_POSTPROC: usize = 25;

// LexVisualProlog style indices. 25 contiguous slots (0..=24)
// for Visual Prolog (extension `.vip`) — Prolog Development
// Center's OOP-flavoured Prolog dialect with typed classes,
// interfaces, and clause bodies. Constants mirror
// `SciLexer.h:1739-1763` verbatim. Dispatches
// SCLEX_VISUALPROLOG (= 107, per `SciLexer.h:123`) via
// `vendor/lexilla/lexers/LexVisualProlog.cxx:700`.
//
// **Four-class wordlist descriptor.** `visualPrologWordLists[]`
// at `LexVisualProlog.cxx:60-66` declares four named slots:
//   - Class 0: "Major keywords (class, predicates, ...)" — the
//     class / interface / implement / domains / predicates /
//     clauses / facts / constants / goal / namespace structural
//     declaration keywords.
//   - Class 1: "Minor keywords (if, then, try, ...)" — control
//     flow (if/then/else/foreach/while), exception handling
//     (try/catch/finally), Prolog primitives (succeed/fail/
//     true/false/not), and mode declarations
//     (procedure/determ/nondeterm/multi/erroneous/failure).
//   - Class 2: "Directive keywords without the '#' (include,
//     requires, ...)" — `#include`, `#requires`, `#import` etc.
//     Wordlist entries are DOTLESS / HASHLESS: the lexer at
//     `:429` uses `directiveKeywords.InList(s + 1)` (skipping
//     the leading `#`).
//   - Class 3: "Documentation keywords without the '@' (short,
//     detail, ...)" — `@short`, `@detail`, `@param`, `@return`
//     etc. Wordlist entries are AT-LESS: `docKeywords.InList(s
//     + 1)` at `:461` skips the leading `@`.
//
// **First-match-wins cascade** for the IDENTIFIER path at
// `:411-415`: probes majorKeywords → minorKeywords in forward
// order, then falls through to `SCE_VISUALPROLOG_IDENTIFIER`.
// Directive keywords (class 2) and doc keywords (class 3) live
// in their own scope-guarded probe sites at `:429` and `:461`
// respectively — not part of the identifier cascade.
//
// **Case-SENSITIVE.** `LexVisualProlog.cxx` uses `GetCurrent`
// and `strcmp` throughout — no `MakeLowerCase` /
// `GetCurrentLowered` anywhere. Visual Prolog is a strictly
// case-sensitive language: **lowercase-lead** identifiers are
// atoms/predicates (SCE_VISUALPROLOG_IDENTIFIER at `:580`),
// **UPPERCASE-lead** identifiers are variables
// (SCE_VISUALPROLOG_VARIABLE at `:582`), and `_`-lead
// identifiers are anonymous variables
// (SCE_VISUALPROLOG_ANONYMOUS at `:584`) — this is Prolog
// convention. Wordlist entries must match source casing
// byte-exactly; major/minor keywords are conventionally
// lowercase.
//
// **`end` lookahead.** Special-case at `:408-410`: when the
// IDENTIFIER settle path collects `end`, `endLookAhead` at
// `:240-253` peeks past whitespace to the following keyword,
// then re-uses `s` to lookup THAT keyword's class. So `end`
// itself never changes state via this path — but a following
// `class` / `interface` / `foreach` gives the shared `end`
// its display class colour. Effect: `end class` paints as
// KEY_MAJOR + KEY_MAJOR (both same colour), `end if` paints
// as KEY_MINOR + KEY_MINOR. Wordlist entries must include
// each `end` variant word (`end`, plus every keyword that
// can follow it).
//
// **String verbatim contract.** Strings enter via
// `SCE_VISUALPROLOG_STRING_QUOTE` (16) at the opening
// delimiter: plain `'...'` / `"..."` / (optional) `` `...` ``
// at `:611-620`, or verbatim `@<...>` / `@(...)` / etc. at
// `:573-576` (any Unicode paired-bracket delimiter accepted via
// `isStringVerbatimOpenClose` at `:230-233`). The body then
// settles to `SCE_VISUALPROLOG_STRING` (20) via the shared
// fallthrough at `:489`. Verbatim strings escape their closing
// quote by doubling it (`ls.verbatim && ls.closingQuote ==
// sc.chNext` at `:500`); non-verbatim strings use `\`-escapes
// at `:508-531`. String state stack
// (verbatim flag + closing quote + `kindStack` for nested
// comments / embedded / placeholder) is persisted in the line
// state via `setState`/`getState` at `:313-321` for incremental
// re-lex support.
//
// **Nested constructs.** `[| ... |]` embedded-syntax literal
// strings (`SCE_VISUALPROLOG_EMBEDDED` at `:591-593`) nest
// inside themselves via `ls.enter(embedded)` at `:541`;
// `{| ... |}` syntax placeholders
// (`SCE_VISUALPROLOG_PLACEHOLDER` at `:595-597`) escape to
// `SCE_VISUALPROLOG_EMBEDDED` when nested inside an embedded-
// syntax region (`:554-555`) or to `SCE_VISUALPROLOG_DEFAULT`
// at top-level (`:557`) — the embedded case is the more common
// real use since PLACEHOLDER's `lexicalClasses[]` description
// literally says "in embedded syntax". `/* ... */` block
// comments nest too via `ls.enter(comment)` at `:587`, `:451`. The `kindStack` (2 bits per level, up to
// 16 levels) supports this at `:322-350`.
//
// **UNUSED slots are DEAD STATES.** UNUSED1 (13), UNUSED2 (14),
// UNUSED3 (15), UNUSED4 (19), UNUSED5 (21) are all documented
// as `"unused"` in the `lexicalClasses[]` table at `:92-100`
// with empty descriptions. Verified by exhaustive grep: zero
// call sites emit any UNUSED* state. Framework convention:
// leave unmapped since no bytes ever paint into them. Reserved
// slots that Lexilla may activate in a future revision without
// renumbering; the map is safely additive.
//
// Style semantics (paint-loop citations reference LexVisualProlog.cxx):
//
//   - SCE_VISUALPROLOG_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_VISUALPROLOG_KEY_MAJOR (1) — class-0 wordlist hit.
//     Emitted via `ChangeState` at `:412`.
//   - SCE_VISUALPROLOG_KEY_MINOR (2) — class-1 wordlist hit.
//     Emitted via `ChangeState` at `:414`.
//   - SCE_VISUALPROLOG_KEY_DIRECTIVE (3) — `#`-lead directive.
//     Entered at `:622`. Falls back to IDENTIFIER at `:430`
//     if the identifier following `#` doesn't match
//     directiveKeywords.
//   - SCE_VISUALPROLOG_COMMENT_BLOCK (4) — `/* ... */`
//     multiline comment. Entered at `:586`. Nesting-aware via
//     `ls.enter(comment)` at `:587, :451`.
//   - SCE_VISUALPROLOG_COMMENT_LINE (5) — `%`-to-EOL line
//     comment. Entered at `:590`.
//   - SCE_VISUALPROLOG_COMMENT_KEY (6) — `@keyword` doc tag
//     that matches docKeywords. Emitted via `ChangeState` at
//     `:462`.
//   - SCE_VISUALPROLOG_COMMENT_KEY_ERROR (7) — `@xxx` that
//     doesn't match docKeywords. Entered at `:441` (in line
//     comment) or `:454` (in block comment).
//   - SCE_VISUALPROLOG_IDENTIFIER (8) — lowercase-lead
//     identifier (transient collect state). Entered at
//     `:580`. Settles via first-match-wins at `:411-415` to
//     KEY_MAJOR / KEY_MINOR, or falls through to DEFAULT at
//     `:416`. Framework convention: leave unmapped so
//     unmatched atoms / predicates paint at STYLE_DEFAULT.
//   - SCE_VISUALPROLOG_VARIABLE (9) — uppercase-lead
//     identifier (Prolog variable convention). Entered at
//     `:582`.
//   - SCE_VISUALPROLOG_ANONYMOUS (10) — `_`-lead identifier
//     (Prolog anonymous variable). Entered at `:584`.
//   - SCE_VISUALPROLOG_NUMBER (11) — decimal / hex `0x` /
//     octal `0o` numeric literal. Entered at `:578`.
//   - SCE_VISUALPROLOG_OPERATOR (12) — operator character
//     (`isoperator` in C-lib), `\`, or `@` when
//     verbatim-strings option is disabled. Entered at `:625`.
//   - SCE_VISUALPROLOG_UNUSED1 (13) — DEAD STATE. Framework
//     convention: leave unmapped.
//   - SCE_VISUALPROLOG_UNUSED2 (14) — DEAD STATE.
//   - SCE_VISUALPROLOG_UNUSED3 (15) — DEAD STATE.
//   - SCE_VISUALPROLOG_STRING_QUOTE (16) — literal string
//     opening/closing quote character. Entered at `:575,
//     :505, :612, :616, :620`.
//   - SCE_VISUALPROLOG_STRING_ESCAPE (17) — recognised
//     `\`-escape (`\n`, `\t`, `ሴ`, etc.). Emitted via
//     `ChangeState` at `:515, :525` or `SetState` at `:501`.
//   - SCE_VISUALPROLOG_STRING_ESCAPE_ERROR (18) — invalid or
//     incomplete escape sequence. Entered at `:497`
//     (unterminated non-verbatim string on EOL) and `:509`
//     (backslash-lead but next char is not a recognised
//     escape). Distinct from COMMENT_KEY_ERROR (7), which
//     handles `@xxx` doc-tag failures at `:441` / `:454`.
//   - SCE_VISUALPROLOG_UNUSED4 (19) — DEAD STATE.
//   - SCE_VISUALPROLOG_STRING (20) — string body. Fall-through
//     state after STRING_QUOTE / STRING_ESCAPE / STRING_EOL
//     via the shared switch cases at `:485-490`.
//   - SCE_VISUALPROLOG_UNUSED5 (21) — DEAD STATE.
//   - SCE_VISUALPROLOG_STRING_EOL (22) — verbatim-string
//     end-of-line marker. Entered at `:494`.
//   - SCE_VISUALPROLOG_EMBEDDED (23) — `[| ... |]`
//     embedded-syntax literal string. Entered at `:592`.
//     Nests via `ls.enter(embedded)` at `:541`.
//   - SCE_VISUALPROLOG_PLACEHOLDER (24) — `{| ... |}:ident`
//     placeholder in embedded syntax. Entered at `:596, :600`.
pub const SCE_VISUALPROLOG_DEFAULT: usize = 0;
pub const SCE_VISUALPROLOG_KEY_MAJOR: usize = 1;
pub const SCE_VISUALPROLOG_KEY_MINOR: usize = 2;
pub const SCE_VISUALPROLOG_KEY_DIRECTIVE: usize = 3;
pub const SCE_VISUALPROLOG_COMMENT_BLOCK: usize = 4;
pub const SCE_VISUALPROLOG_COMMENT_LINE: usize = 5;
pub const SCE_VISUALPROLOG_COMMENT_KEY: usize = 6;
pub const SCE_VISUALPROLOG_COMMENT_KEY_ERROR: usize = 7;
pub const SCE_VISUALPROLOG_IDENTIFIER: usize = 8;
pub const SCE_VISUALPROLOG_VARIABLE: usize = 9;
pub const SCE_VISUALPROLOG_ANONYMOUS: usize = 10;
pub const SCE_VISUALPROLOG_NUMBER: usize = 11;
pub const SCE_VISUALPROLOG_OPERATOR: usize = 12;
pub const SCE_VISUALPROLOG_UNUSED1: usize = 13;
pub const SCE_VISUALPROLOG_UNUSED2: usize = 14;
pub const SCE_VISUALPROLOG_UNUSED3: usize = 15;
pub const SCE_VISUALPROLOG_STRING_QUOTE: usize = 16;
pub const SCE_VISUALPROLOG_STRING_ESCAPE: usize = 17;
pub const SCE_VISUALPROLOG_STRING_ESCAPE_ERROR: usize = 18;
pub const SCE_VISUALPROLOG_UNUSED4: usize = 19;
pub const SCE_VISUALPROLOG_STRING: usize = 20;
pub const SCE_VISUALPROLOG_UNUSED5: usize = 21;
pub const SCE_VISUALPROLOG_STRING_EOL: usize = 22;
pub const SCE_VISUALPROLOG_EMBEDDED: usize = 23;
pub const SCE_VISUALPROLOG_PLACEHOLDER: usize = 24;

// LexGDScript style indices. 17 contiguous slots (0..=16) covering
// Godot Engine's GDScript source (extension `.gd`). Constants mirror
// `SciLexer.h:2073-2089` verbatim. Dispatches SCLEX_GDSCRIPT (= 135,
// per `SciLexer.h:151`) via
// `vendor/lexilla/lexers/LexGDScript.cxx:771-772`, which registers
// `LexerModule lmGDScript(SCLEX_GDSCRIPT, LexerFactoryGDScript,
// "gdscript", gdscriptWordListDesc)`.
//
// **Two wordlists.** `gdscriptWordListDesc[]` at
// `LexGDScript.cxx:171-175` declares two named classes:
//   - class 0 = "Keywords" — reserved words like `if` / `func` /
//     `class` / `extends` / `await` / `self` / `null` / `true` /
//     `false`. First-match-wins per the classifier logic below.
//   - class 1 = "Highlighted identifiers" — Godot's built-in
//     global scope: Variant types (`Vector3` / `Color` / `String`),
//     built-in functions (`print` / `range` / `randi`),
//     mathematical constants (`PI` / `TAU` / `INF` / `NAN`).
//
// **Classifier order matters.** The identifier-exit path at
// `LexGDScript.cxx:454-495` probes wordlists in this fixed order:
//   1. If `keywords.InList(s)` → `SCE_GD_WORD`.
//   2. Else if `kwLast == kwClass` (previous WORD was `class`) →
//      `SCE_GD_CLASSNAME`. Applies to `class Foo:` / `class Foo
//      extends Bar:`.
//   3. Else if `kwLast == kwDef` (previous WORD was `func`) →
//      `SCE_GD_FUNCNAME`. Applies to `func my_method():`.
//   4. Else if `keywords2.InList(s)` → `SCE_GD_WORD2`.
//   5. Else → `SCE_GD_IDENTIFIER`.
//
// Because class 0 is probed FIRST, class-0 vs class-1 disjointness
// is load-bearing: a token in both lists renders as `SCE_GD_WORD`,
// making the class-1 entry dead code. Same discipline as JS's
// `LexCPP` classifier at `LexCPP.cxx:995-999`.
//
// **`kwExtends` is a dead branch.** The `enum kwType` at `:41`
// declares `kwExtends` and the classifier at `:490` sets `kwLast =
// kwExtends` after the `extends` keyword — but no downstream code
// tests `kwLast == kwExtends`. The identifier immediately after
// `extends` therefore falls through to `keywords2` or `IDENTIFIER`
// via the standard path. This is an upstream implementation gap
// (Ruby-style parent-class highlighting was likely intended);
// framework consequence: `extends Player` renders `Player` as
// `IDENTIFIER` (STYLE_DEFAULT) unless `Player` happens to also be
// a class-1 built-in like `Node` (renders WORD2).
//
// **`@`-annotations.** GDScript decorators (`@onready`,
// `@export`, `@rpc`, `@tool`, `@icon`, `@warning_ignore`,
// `@export_range`, `@export_group`, …) enter `SCE_GD_ANNOTATION`
// only when the `@` is the first non-whitespace character on its
// line (`LexGDScript.cxx:594-598`). An `@` mid-expression falls
// through to `SCE_GD_OPERATOR` — GDScript has no `@decorator`
// expression form outside statement position. The annotation
// itself is not name-looked-up against a wordlist; the classifier
// paints every alphanumeric run after the leading `@` as
// annotation until whitespace, so `@my_custom_annotation` and
// `@onready` both paint the same. Framework routes ANNOTATION
// through `StyleSlot::Preprocessor` (matches Python's
// `SCE_P_DECORATOR` precedent — same `@name` mechanism, same
// structural role).
//
// **NodePath sigils.** `$Node/Path` and `%SceneName` enter
// `SCE_GD_NODEPATH` at `:586-588`. `$` always enters NODEPATH; `%`
// only enters NODEPATH when the previous non-open-bracket
// character permits it (`percentIsNodePath` flag at `:586-590,
// :590` — inhibited by closing `)` / `]` / `}` because those
// suggest `%` is the modulo operator following an expression).
// Framework routes NODEPATH through `StyleSlot::Lifetime` —
// structural sigil-tagged references matching the Bash SCALAR /
// Lisp SYMBOL precedent for `$`/`&`/`:kw`/`'quoted` prefixed
// tokens.
//
// **Position-derived styles.** CLASSNAME / FUNCNAME are NOT
// keyword-lookup hits — they're pure position-derived. `class`
// keyword bumps `kwLast = kwClass`, the following identifier
// becomes CLASSNAME, and `kwLast` resets on the next OPERATOR or
// non-keyword identifier via `:447` and `:494`. Framework routes
// both through `StyleSlot::Keyword2` — matches Python's
// `SCE_P_CLASSNAME` / `SCE_P_DEFNAME` and Ruby's
// `SCE_RB_CLASSNAME` / `SCE_RB_DEFNAME` precedent.
//
// **String flavours (four).** Single-quoted `'...'` → CHARACTER
// (:601-604 via `GetGDStringState`), double-quoted `"..."` →
// STRING, triple-single `'''..'''` → TRIPLE, triple-double
// `"""..."""` → TRIPLEDOUBLE. All four collapse to
// `StyleSlot::String` — same discipline as Python's SCE_P_STRING /
// SCE_P_CHARACTER / SCE_P_TRIPLE / SCE_P_TRIPLEDOUBLE unification,
// and matches most editors' rendering of GDScript's docstring
// convention (triple-quoted at the top of a class/function).
//
// **STRINGEOL is REACHABLE.** Unterminated string at `:364-365`
// fires `sc.ChangeState(SCE_GD_STRINGEOL); sc.ForwardSetState(
// SCE_GD_DEFAULT)`. Routes to `StyleSlot::String` per the
// established `_STRINGEOL → String` collapse (JS / VHDL / Ada /
// Verilog / Haskell / D precedent — the error state gets the same
// paint as the successful state so partially-typed source
// doesn't flicker into a different colour).
//
// **`##`-doc-comment vs `#`-line-comment.** At `:592-593`, a
// `#` character followed by another `#` enters COMMENTBLOCK;
// otherwise COMMENTLINE. Both route to `StyleSlot::Comment` +
// italic — matches Godot editor convention where `##` marks
// doc-comments (equivalent to Python's `"""docstring"""` but
// line-oriented) but visually indistinguishable from ordinary
// `#`-line-comments.
//
// **Comment / Annotation state exits.** COMMENTLINE / COMMENTBLOCK
// exit on `\r` / `\n` at `:497-500`. ANNOTATION exits on first
// non-word character at `:501-504`. NODEPATH exits on first non-
// nodepath character at `:505-516` (nodepaths may contain nested
// strings — `$"Node With Spaces"` — via `nodePathStringState` at
// `:506-509`, painted as one NODEPATH span).
//
// **Numeric literals.** SCE_GD_NUMBER covers decimal, hex (`0x`),
// binary (`0b`), and octal (`0o`) — the last two gated by
// `options.base2or8Literals` (default true). Float exponent
// `1e5` / `1.5e-3` handled via `:451-452` (`sc.chPrev == 'e' ||
// sc.chPrev == 'E'` allows `+` / `-` continuation).
pub const SCLEX_GDSCRIPT: usize = 135;
pub const SCE_GD_DEFAULT: usize = 0;
pub const SCE_GD_COMMENTLINE: usize = 1;
pub const SCE_GD_NUMBER: usize = 2;
pub const SCE_GD_STRING: usize = 3;
pub const SCE_GD_CHARACTER: usize = 4;
pub const SCE_GD_WORD: usize = 5;
pub const SCE_GD_TRIPLE: usize = 6;
pub const SCE_GD_TRIPLEDOUBLE: usize = 7;
pub const SCE_GD_CLASSNAME: usize = 8;
pub const SCE_GD_FUNCNAME: usize = 9;
pub const SCE_GD_OPERATOR: usize = 10;
pub const SCE_GD_IDENTIFIER: usize = 11;
pub const SCE_GD_COMMENTBLOCK: usize = 12;
pub const SCE_GD_STRINGEOL: usize = 13;
pub const SCE_GD_WORD2: usize = 14;
pub const SCE_GD_ANNOTATION: usize = 15;
pub const SCE_GD_NODEPATH: usize = 16;

// LexHollywood style indices. 15 contiguous slots (0..=14) covering
// Hollywood — a proprietary Lua-inspired multimedia programming
// language by Andreas Falkenhahn (airsoftsoftwair.de). Extension
// `.hws`. Constants mirror `SciLexer.h:1985-1999` verbatim.
// Dispatches SCLEX_HOLLYWOOD (= 130, per `SciLexer.h:146`) via
// `vendor/lexilla/lexers/LexHollywood.cxx:519`, which registers
// `LexerModule lmHollywood(SCLEX_HOLLYWOOD, LexerFactoryHollywood,
// "hollywood", hollywoodWordListDesc)`.
//
// **Four wordlists.** `hollywoodWordListDesc[]` at
// `LexHollywood.cxx:235-241` declares four named classes:
//   - class 0 = "Hollywood keywords" — language reserved words
//     (`If` / `Then` / `Function` / `Local` / `Return` / etc.).
//   - class 1 = "Hollywood standard API functions" — the
//     stdlib API (`Print` / `LoadBrush` / `OpenAnim` / `Rgb` /
//     etc.).
//   - class 2 = "Hollywood plugin API functions" — plugin-
//     provided global functions (plugin-set-specific; empty in
//     Code++'s default install).
//   - class 3 = "Hollywood plugin methods" — plugin-provided
//     object methods (plugin-set-specific; empty in Code++'s
//     default install).
//
// **Case-INSENSITIVE lookup.** The identifier-exit path at
// `LexHollywood.cxx:348-364` calls
// `sc.GetCurrentLowered(s, sizeof(s))` before every
// `keywordlists[i].InList(s)` probe — all wordlist entries MUST
// be stored lowercase. Hollywood's own convention is PascalCase
// (`Print`, `LoadBrush`), but the lexer lowercases the input
// before matching, so the wordlist strings themselves are stored
// lowercase.
//
// **Last-match-wins across classes.** The `for i in 0..4` loop
// at `:358-362` does NOT break — it keeps probing every class
// and each `ChangeState` overwrites the previous one. So a token
// present in both class 0 and class 3 renders as
// `SCE_HOLLYWOOD_PLUGINMETHOD` (class 3), not
// `SCE_HOLLYWOOD_KEYWORD` (class 0). Framework consequence:
// wordlists must be **cross-class disjoint** — any duplicate
// silently promotes to the highest matching class. Different
// discipline from LexCPP (which is first-match-wins) and same as
// LexREBOL (also last-match-wins).
//
// **`;`-line-comment vs `/*..*/` block comment.** At `:410-414`,
// `;` enters COMMENT and `/*` enters COMMENTBLOCK. The block-
// comment exit at `:397-401` matches `*/` but is **string-
// aware**: the `inString` flag toggles on unescaped `"` at
// `:346` and gates the `*/` recognition. This handles the
// pathological case where `*/` appears literally inside a
// string. Both comment states → `StyleSlot::Comment` italic.
//
// **`"..."` string vs `[[..]]` block-string (Lua heredoc).** At
// `:415-419`, `[[` enters STRINGBLOCK and `"` enters STRING.
// The `[[..]]` form is Hollywood's multi-line string literal
// (matches Lua's `[[..]]` syntax). Both collapse to
// `StyleSlot::String` — same discipline as Python's
// SCE_P_STRING / _TRIPLEDOUBLE unification.
//
// **`$xxx` and `0xxxx` hex literals.** At `:420-424`, both
// `$abc` (BASIC-style hex prefix) and `0xabc` (C-style hex
// prefix) enter HEXNUMBER. Framework routes HEXNUMBER +
// NUMBER both to `StyleSlot::Number`.
//
// **`.1234` leading-dot decimal literal.** At `:425-427`, a
// `.` followed by a digit enters NUMBER — Hollywood allows
// leading-dot decimals like BASIC.
//
// **`#`-prefix constants.** At `:430-431`, `#` enters CONSTANT
// state — Hollywood's named-constant convention (`#RED`,
// `#WHITE`, `#TRUE`). Framework routes CONSTANT to
// `StyleSlot::Lifetime` — structural sigil-tagged reference,
// matching the Bash SCALAR / Lisp SYMBOL / Perl SCALAR
// precedent for `$` / `&` / `:kw` / `#`-prefixed identifiers.
//
// **`@`-prefix preprocessor directives.** At `:432-433`, `@`
// enters PREPROCESSOR state — Hollywood uses `@REQUIRE`,
// `@INCLUDE`, `@VERSION`, `@DISPLAY`, `@BGPIC` etc. as
// preprocessor markers. Framework routes PREPROCESSOR to
// `StyleSlot::Preprocessor` + bold — matches Python's
// `SCE_P_DECORATOR` bold-preprocessor precedent.
//
// **Comment-inside-operator gotcha.** The paint loop at
// `:365-372` explicitly resets to DEFAULT after every OPERATOR
// byte (rather than continuing to consume operator chars) — the
// author's comment cites the `+/*` sequence: without this
// reset, `+/*` would tokenise as three-char OPERATOR span and
// the `/*` block-comment start would be missed. Consequence:
// OPERATOR spans are always single-character; no framework
// action needed but noted for future refactoring.
pub const SCLEX_HOLLYWOOD: usize = 130;
pub const SCE_HOLLYWOOD_DEFAULT: usize = 0;
pub const SCE_HOLLYWOOD_COMMENT: usize = 1;
pub const SCE_HOLLYWOOD_COMMENTBLOCK: usize = 2;
pub const SCE_HOLLYWOOD_NUMBER: usize = 3;
pub const SCE_HOLLYWOOD_KEYWORD: usize = 4;
pub const SCE_HOLLYWOOD_STDAPI: usize = 5;
pub const SCE_HOLLYWOOD_PLUGINAPI: usize = 6;
pub const SCE_HOLLYWOOD_PLUGINMETHOD: usize = 7;
pub const SCE_HOLLYWOOD_STRING: usize = 8;
pub const SCE_HOLLYWOOD_STRINGBLOCK: usize = 9;
pub const SCE_HOLLYWOOD_PREPROCESSOR: usize = 10;
pub const SCE_HOLLYWOOD_OPERATOR: usize = 11;
pub const SCE_HOLLYWOOD_IDENTIFIER: usize = 12;
pub const SCE_HOLLYWOOD_CONSTANT: usize = 13;
pub const SCE_HOLLYWOOD_HEXNUMBER: usize = 14;

// LexRaku style indices. 29 contiguous slots (0..=28) covering Raku
// — the current name of what was previously called "Perl 6", a
// gradually-typed, object-oriented / functional / declarative
// language with rich lexical syntax (sigils, twigils, Q language
// / heredocs, grammars, POD blocks, regexes with adverbs, ...).
// Extensions `.raku` / `.rakumod`. Constants mirror
// `SciLexer.h:2000-2028` verbatim. Dispatches SCLEX_RAKU
// (= 131, per `SciLexer.h:147`) via
// `vendor/lexilla/lexers/LexRaku.cxx:1634` — the `LexerModule
// lmRaku(SCLEX_RAKU, LexerFactoryRaku, "raku", rakuWordLists)`
// registration.
//
// **Seven wordlists.** `rakuWordLists[]` at
// `LexRaku.cxx:106-115` declares seven named classes:
//   - class 0 = "Keywords and identifiers" — reserved words
//     like `if` / `for` / `sub` / `class` / phasers `BEGIN` /
//     `END` / etc.
//   - class 1 = "Functions" — Raku built-in-function names
//     (`abs` / `chr` / `say` / `push` / `sort` / `map` / ...).
//   - class 2 = "Types basic" — primitive / core types
//     (`Str` / `Int` / `Num` / `Bool` / `bit` / `int32` /
//     `num64` / ...).
//   - class 3 = "Types composite" — collection / container
//     types (`Array` / `Hash` / `List` / `Map` / `Set` /
//     `Bag` / ...).
//   - class 4 = "Types domain-specific" — I/O / concurrency /
//     grammar / POD types (`IO::Handle` / `Promise` / `Grammar`
//     / `Pod::Block` / ...).
//   - class 5 = "Types exception" — the `X::` exception
//     hierarchy (`X::AdHoc` / `X::TypeCheck` / `X::Syntax::*`
//     / ...).
//   - class 6 = "Adverbs" — regex + `Q`-language + `:sym`
//     adverbs (`sym` / `to` / `qq` / `words` / `heredoc` /
//     ...). Only recognised after `:` sigil.
//
// **Classifier order (identifier).** The identifier-classifier
// at `LexRaku.cxx:1367-1391` probes wordlists in this fixed
// order for a bareword identifier:
//   1. If `keywords.InList(s)` → `SCE_RAKU_WORD` (class 0).
//   2. Else if `functions.InList(s)` → `SCE_RAKU_FUNCTION`
//      (class 1).
//   3. Else if `typesBasic.InList(s)` OR `typesComposite.InList(s)`
//      OR `typesDomainSpecific.InList(s)` OR
//      `typesExceptions.InList(s)` → `SCE_RAKU_TYPEDEF` (classes
//      2 / 3 / 4 / 5 collapse to one style slot). Framework
//      consequence: the four TYPE classes cannot be
//      distinguished by style even though they're kept separate
//      as source data.
//   4. Else if `wordLast == "class"` → `SCE_RAKU_CLASS`
//      (position-derived — identifier following the `class`
//      keyword). Same discipline as Python's `SCE_P_CLASSNAME`
//      / Ruby's `SCE_RB_CLASSNAME` / GDScript's
//      `SCE_GD_CLASSNAME`.
//   5. Else if `wordLast == "grammar"` → `SCE_RAKU_GRAMMAR`
//      (position-derived — identifier following the `grammar`
//      keyword, which is Raku's PEG-parser-declaration form).
//   6. Else → `SCE_RAKU_IDENTIFIER` (bareword).
//
// **First-match-wins semantics** across classes 0-5 (the `else
// if` chain at :1369-1381), so cross-class disjointness is
// load-bearing for the class-0 → class-1 → class-2..5 → position-
// derived hierarchy. Cross-class duplicates within classes 2-5
// are harmless since they all collapse to the same TYPEDEF
// style; still, the upstream fixture keeps them disjoint (verified
// against `crates/scintilla-sys/vendor/lexilla/test/examples/raku/
// SciTE.properties`).
//
// **Class 6 (Adverbs) uses a separate `:` gate.** Adverb
// classification at `LexRaku.cxx:1400-1407` only fires when the
// current char is `:` and the next is a word-start (`:sym`,
// `:qq`, `:to`, `:heredoc`, ...). The wordlist entries
// themselves are STORED WITHOUT the leading `:` — `:sym`
// tokenises as `SCE_RAKU_ADVERB` iff `sym` is in `adverbs`
// wordlist. Same prefix-stripping discipline as REBOL's
// `REBOL_WORD_*` classes.
//
// **Case-SENSITIVE identifier lookup.**
// `LexRaku.cxx:1368` populates the identifier buffer via
// `LengthToNonWordChar(sc, lengthToEnd, s, sizeof(s))`, which
// copies raw source bytes verbatim without any tolower step —
// confirmed by grep (no `tolower` / `MakeLowerCase` /
// `GetCurrentLowered` anywhere in the file). Wordlist entries
// must match source spelling exactly. Raku convention: types
// are `PascalCase` (`Int`, `Str`, `IO::Handle`), keywords +
// functions are lowercase (`if`, `for`, `abs`, `push`),
// phasers + closing-CATCH-like keywords are `SCREAMING`
// (`BEGIN`, `END`, `CATCH`, `LEAVE`).
//
// **Sigil-tagged variable states.** At `LexRaku.cxx:1417-1424`,
// the sigil character maps to a distinct variable state:
//   - `$` → SCE_RAKU_MU (`$foo` — scalar / any, the universal
//     "Mu" type in Raku's type hierarchy — hence the state
//     name).
//   - `@` → SCE_RAKU_POSITIONAL (`@array` — positional
//     collection).
//   - `%` → SCE_RAKU_ASSOCIATIVE (`%hash` — associative
//     collection).
//   - `&` → SCE_RAKU_CALLABLE (`&code` — callable reference).
//
// Framework routes all four sigil states through
// `StyleSlot::Lifetime` (matches Bash SCALAR / PARAM / Lisp
// SYMBOL / Perl SCALAR structural-sigil precedent — the
// established framework convention for `$`/`&`/`%`/`@`
// sigil-prefixed structural anchors).
//
// **Q-language string states.** Raku's Q language exposes
// half a dozen string-quoting forms
// (https://docs.raku.org/language/quoting):
//   - `SCE_RAKU_STRING` (8) — the generic `"..."` state.
//   - `SCE_RAKU_STRING_Q` (9) — `q(...)` / `q{...}` — no
//     interpolation.
//   - `SCE_RAKU_STRING_QQ` (10) — `qq(...)` / `qq{...}` — with
//     interpolation.
//   - `SCE_RAKU_STRING_Q_LANG` (11) — `Q(...)` / `Q{...}` —
//     literal-quoting form (no escapes).
//   - `SCE_RAKU_STRING_VAR` (12) — variable interpolation
//     INSIDE a `qq`/`qq..` state. Ships as a nested state that
//     paints `$var` / `@var` etc. distinctly inside the outer
//     string span.
//   - `SCE_RAKU_HEREDOC_Q` (6) — `q :to/EOF/` heredoc, no
//     interpolation.
//   - `SCE_RAKU_HEREDOC_QQ` (7) — `qq :to/EOF/` heredoc, with
//     interpolation.
//   - `SCE_RAKU_CHARACTER` (5) — **reserved but unemitted** in
//     the vendored Lexilla version. Grep of `LexRaku.cxx`
//     returns zero hits for `SCE_RAKU_CHARACTER` — the
//     `SciLexer.h` constant table declares the slot but the
//     lexer body never enters it. Framework still maps it to
//     `StyleSlot::String` so a future upstream change that
//     starts emitting it gets a defined paint immediately;
//     until then, the mapping is dead code.
//
// The seven ACTIVE string-family states plus the reserved
// CHARACTER slot all collapse to `StyleSlot::String` — same
// collapse discipline as Python's SCE_P_STRING / _CHARACTER
// / _TRIPLE / _TRIPLEDOUBLE unification and TypeScript's
// five-flavour string collapse.
//
// **Regex + adverb inline states.** `SCE_RAKU_REGEX` (13) is
// the body of a `/regex/` or `rx/regex/` construct;
// `SCE_RAKU_REGEX_VAR` (14) is a variable interpolation inside
// it. Both route through `StyleSlot::String` (Perl / Bash
// convention for regex-family paint).
//
// **POD documentation.** `SCE_RAKU_POD` (4) covers Raku's Perl
// Old Documentation blocks (`=begin pod ... =end pod`). Routes
// through `StyleSlot::Comment` — same discipline as Perl's
// SCE_PL_POD.
//
// **Doc-comment `#|` and side-comment `#=` embed comments** —
// `SCE_RAKU_COMMENTEMBED` (3) covers Raku's declarator-doc
// syntax. Distinct from `SCE_RAKU_COMMENTLINE` (2) for `#`-line
// comments. Both route to Comment italic.
//
// **`SCE_RAKU_ERROR` (1)** — an authoritative parse-failure
// state emitted when the classifier can't continue. Left
// unmapped per the established `StyleSlot::Error`-migration
// convention (deferred to a future dedicated error slot).
// Same discipline as Visual Prolog's
// `SCE_VISUALPROLOG_STRING_ESCAPE_ERROR` unmapping.
//
// **`SCE_RAKU_PREPROCESSOR` (17)** — **reserved but
// unemitted** in the vendored Lexilla version. Grep of
// `LexRaku.cxx` returns zero hits for `SCE_RAKU_PREPROCESSOR`.
// `use v6.d` / `use MONKEY-TYPING` / `use nqp` pragma
// directives actually paint as `SCE_RAKU_FUNCTION` (`use` is
// in the class-1 wordlist per the upstream fixture). Framework
// still maps SCE_RAKU_PREPROCESSOR → `StyleSlot::Preprocessor`
// so a future upstream change gets a defined paint; until
// then, the mapping is dead code.
pub const SCLEX_RAKU: usize = 131;
pub const SCE_RAKU_DEFAULT: usize = 0;
pub const SCE_RAKU_ERROR: usize = 1;
pub const SCE_RAKU_COMMENTLINE: usize = 2;
pub const SCE_RAKU_COMMENTEMBED: usize = 3;
pub const SCE_RAKU_POD: usize = 4;
pub const SCE_RAKU_CHARACTER: usize = 5;
pub const SCE_RAKU_HEREDOC_Q: usize = 6;
pub const SCE_RAKU_HEREDOC_QQ: usize = 7;
pub const SCE_RAKU_STRING: usize = 8;
pub const SCE_RAKU_STRING_Q: usize = 9;
pub const SCE_RAKU_STRING_QQ: usize = 10;
pub const SCE_RAKU_STRING_Q_LANG: usize = 11;
pub const SCE_RAKU_STRING_VAR: usize = 12;
pub const SCE_RAKU_REGEX: usize = 13;
pub const SCE_RAKU_REGEX_VAR: usize = 14;
pub const SCE_RAKU_ADVERB: usize = 15;
pub const SCE_RAKU_NUMBER: usize = 16;
pub const SCE_RAKU_PREPROCESSOR: usize = 17;
pub const SCE_RAKU_OPERATOR: usize = 18;
pub const SCE_RAKU_WORD: usize = 19;
pub const SCE_RAKU_FUNCTION: usize = 20;
pub const SCE_RAKU_IDENTIFIER: usize = 21;
pub const SCE_RAKU_TYPEDEF: usize = 22;
pub const SCE_RAKU_MU: usize = 23;
pub const SCE_RAKU_POSITIONAL: usize = 24;
pub const SCE_RAKU_ASSOCIATIVE: usize = 25;
pub const SCE_RAKU_CALLABLE: usize = 26;
pub const SCE_RAKU_GRAMMAR: usize = 27;
pub const SCE_RAKU_CLASS: usize = 28;

// LexHex-family style indices. 19 contiguous slots (0..=18) SHARED
// across Motorola S-Record (`.srec` / `.s19` / `.s28` / `.s37`),
// Intel HEX (`.hex` / `.ihex`), and Tektronix extended HEX
// (`.tehex`) record formats. `vendor/lexilla/lexers/LexHex.cxx`
// registers three separate `LexerModule` instances at
// `LexHex.cxx:1046-1048`:
//   - `lmSrec(SCLEX_SREC = 117, ColouriseSrecDoc, "srec", 0, NULL)`
//   - `lmIHex(SCLEX_IHEX, ColouriseIHexDoc, "ihex", FoldIHexDoc, NULL)`
//   - `lmTEHex(SCLEX_TEHEX, ColouriseTEHexDoc, "tehex", 0, NULL)`
//
// All three use the **same `SCE_HEX_*` style constant table** — no
// separate `SCE_SREC_*` / `SCE_IHEX_*` / `SCE_TEHEX_*` namespaces
// exist. Constants mirror `SciLexer.h:1863-1881` verbatim.
//
// **Zero wordlists.** The `LexerModule` constructors take a 5-arg
// form ending with `NULL` for the `wordListDesc[]` parameter (vs
// the standard 4-arg or 5-arg with a real descriptor). All three
// paint functions have signature `(startPos, length, initStyle,
// WordList *[], Accessor &styler)` and the `WordList *[]`
// parameter is **unnamed and never referenced** in the body —
// installing keywords is a no-op that gets silently ignored by
// Scintilla. Framework consequence: `SREC_THEME.keywords` MUST
// be empty; same discipline as [`SCE_PROPS_KEY`] (INI / Properties
// — ignores wordlists) and [`SCE_REG_DEFAULT`] (Registry —
// rejects them outright with `-1`).
//
// **State-machine driven line classification.** The lexer runs a
// position-based state machine anchored on the record-line
// structure. For S-Record:
//   `S<type><count><address><data...><checksum>`
// where `<type>` is a single digit (0/1/2/3/5/7/8/9), `<count>`
// is the byte count as a 2-digit hex, `<address>` is 4/6/8 hex
// digits (depending on record type), `<data>` is 0..504 hex
// digits, `<checksum>` is 2 hex digits. Each of those fields
// gets its own SCE_HEX_* style at paint time.
//
// **Which SCE_HEX_* states are emitted per lexer** (from the
// per-lexer state matrix at `LexHex.cxx:25-27` for S-Record,
// `:45-49` for Intel HEX, `:82-84` for Tektronix HEX):
//   - **S-Record** (`ColouriseSrecDoc` at `:649-892`): uses
//     RECSTART (`S` leader), RECTYPE / RECTYPE_UNKNOWN, BYTECOUNT
//     / BYTECOUNT_WRONG, NOADDRESS + DATAADDRESS + RECCOUNT +
//     STARTADDRESS + ADDRESSFIELD_UNKNOWN (five address flavours
//     per record type), DATA_ODD / DATA_EVEN / DATA_EMPTY /
//     DATA_UNKNOWN, CHECKSUM / CHECKSUM_WRONG, GARBAGE.
//     `SCE_HEX_EXTENDEDADDRESS` (11) is **IHEX-only** — never
//     emitted by Srec's paint loop; Code++'s SREC_STYLES omits
//     it accordingly.
//
// **Style-routing plan** for S-Record:
//   - `SCE_HEX_RECSTART` (1) → Preprocessor. The `S` leader is
//     a structural per-line marker analogous to Registry's
//     KEYNAME or POD's `=begin`. Bold (matches Python's
//     `SCE_P_DECORATOR` / Hollywood's `PREPROCESSOR` bold
//     precedent for structural anchors).
//   - `SCE_HEX_RECTYPE` (2) → Keyword. The record type digit
//     is the language's flow-control equivalent (S0=header,
//     S1/S2/S3=data at 16/24/32-bit address, S5=count,
//     S7/S8/S9=start-address at 32/24/16-bit). Bold.
//   - `SCE_HEX_BYTECOUNT` (4) → Number.
//   - `SCE_HEX_NOADDRESS` (6) → Number. Used by S0 (header
//     record — address field present but semantically zero).
//   - `SCE_HEX_DATAADDRESS` (7) → Number. Used by S1/S2/S3
//     data records — the load address of the data payload.
//   - `SCE_HEX_RECCOUNT` (8) → Number. Used by S5 (record
//     count — how many S1/S2/S3 records preceded).
//   - `SCE_HEX_STARTADDRESS` (9) → Number. Used by S7/S8/S9
//     terminator records — the entry-point address.
//   - `SCE_HEX_DATA_ODD` (12) + `SCE_HEX_DATA_EVEN` (13) →
//     String. Both collapse to the same paint (alternating
//     stripes rendered by the terminal font at rendering
//     time; not a semantic distinction the framework needs to
//     preserve). Same collapse discipline as Fortran's STRING1
//     + STRING2 + STRINGEOL and TypeScript's five-flavour
//     string collapse.
//   - `SCE_HEX_DATA_EMPTY` (15) → Comment. Padding / trailing
//     empty data — visually deemphasised.
//   - `SCE_HEX_CHECKSUM` (16) → Lifetime. Integrity anchor at
//     end of every record — structural role matching Bash
//     SCALAR / Lisp SYMBOL / GDScript NODEPATH / Perl SCALAR
//     precedent for sigil-like structural markers.
//
// **Unmapped states** (deferred to `StyleSlot::Error` migration):
//   - `SCE_HEX_DEFAULT` (0) — whitespace / unclassified per
//     framework convention.
//   - `SCE_HEX_RECTYPE_UNKNOWN` (3) — undefined record type
//     digit (S4 / S6 don't exist in the S-Record standard).
//   - `SCE_HEX_BYTECOUNT_WRONG` (5) — byte count doesn't
//     match line length.
//   - `SCE_HEX_ADDRESSFIELD_UNKNOWN` (10) — address field
//     malformed.
//   - `SCE_HEX_DATA_UNKNOWN` (14) — data byte incomplete /
//     unexpected non-hex.
//   - `SCE_HEX_CHECKSUM_WRONG` (17) — computed checksum
//     doesn't match trailing byte.
//   - `SCE_HEX_GARBAGE` (18) — arbitrary text outside record
//     structure.
//
// All six authoritative parse-failure states (RECTYPE_UNKNOWN
// / BYTECOUNT_WRONG / ADDRESSFIELD_UNKNOWN / DATA_UNKNOWN /
// CHECKSUM_WRONG / GARBAGE) plus DEFAULT (the idle scan state
// before any `S` leader is seen — NOT a parse failure) are
// left unmapped per the established framework convention (same
// discipline as Visual Prolog's STRING_ESCAPE_ERROR +
// COMMENT_KEY_ERROR unmapping and Registry's malformed-value
// states for the error group; universal DEFAULT-unmapped
// convention for the idle state).
//
// **`SCE_HEX_EXTENDEDADDRESS` (11)** is unmapped in S-Record's
// theme — the state is IHEX-only per `LexHex.cxx:49`. Mapped
// to `StyleSlot::Number` in `IHEX_STYLES` (record types 0x02 /
// 0x04 populate it via `LexHex.cxx:519, :863`). Constant is
// declared here (not in an IHEX-scoped module) because
// `SCE_HEX_*` is a lexer-family-shared namespace covering
// SREC / IHEX / TEHEX. `SREC_STYLES` omits the mapping (dead
// code for Srec); a future TEHEX theme wiring will inherit
// the constant unchanged.
pub const SCLEX_SREC: usize = 117;
pub const SCLEX_IHEX: usize = 118;
pub const SCLEX_TEHEX: usize = 119;
pub const SCE_HEX_DEFAULT: usize = 0;
pub const SCE_HEX_RECSTART: usize = 1;
pub const SCE_HEX_RECTYPE: usize = 2;
pub const SCE_HEX_RECTYPE_UNKNOWN: usize = 3;
pub const SCE_HEX_BYTECOUNT: usize = 4;
pub const SCE_HEX_BYTECOUNT_WRONG: usize = 5;
pub const SCE_HEX_NOADDRESS: usize = 6;
pub const SCE_HEX_DATAADDRESS: usize = 7;
pub const SCE_HEX_RECCOUNT: usize = 8;
pub const SCE_HEX_STARTADDRESS: usize = 9;
pub const SCE_HEX_ADDRESSFIELD_UNKNOWN: usize = 10;
pub const SCE_HEX_EXTENDEDADDRESS: usize = 11;
pub const SCE_HEX_DATA_ODD: usize = 12;
pub const SCE_HEX_DATA_EVEN: usize = 13;
pub const SCE_HEX_DATA_UNKNOWN: usize = 14;
pub const SCE_HEX_DATA_EMPTY: usize = 15;
pub const SCE_HEX_CHECKSUM: usize = 16;
pub const SCE_HEX_CHECKSUM_WRONG: usize = 17;
pub const SCE_HEX_GARBAGE: usize = 18;

// LexBash (SH) style indices. 14 contiguous slots (0..=13) covering
// the Bash / POSIX-shell lexer's full emission set: `#`-to-EOL
// comments (COMMENTLINE), decimal / hex / base-N numeric literals
// (NUMBER), reserved-word + builtin tokens (WORD), `"..."` and
// `'...'` quoted strings (STRING / CHARACTER), the shell operator
// set `^&%()-+=|{}[]:;>,*/<?!.~@` (OPERATOR), `$var` / `$1` / `$@`
// sigil-tagged variables (SCALAR), `${param}` / `${param:-default}`
// parameter expansion (PARAM), `` `cmd` `` and `$(cmd)` command
// substitution (BACKTICKS), and the `<<EOF` / `<<-EOF` heredoc
// machinery split across the opening delimiter line (HERE_DELIM)
// and the body bytes (HERE_Q). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 1094-1107, the
// `lexicalClasses[]` table at `vendor/lexilla/lexers/LexBash.cxx`
// lines 456-472, and the `LexerModule lmBash(SCLEX_BASH, ..., "bash",
// bashWordListDesc)` registration at `LexBash.cxx:1268`.
//
// **LexBash is case-sensitive.** The keyword classification path at
// `LexBash.cxx:689, :691, :699, :727` uses raw `strcmp` and
// `keywords.InList(s)` against the unmodified `sc.GetCurrent(s, ...)`
// buffer — no `MakeLowerCase` / `GetCurrentLowered` anywhere in the
// file. So `if`/`then`/`fi` are keywords, `IF`/`Then`/`FI` fall
// through to `SCE_SH_IDENTIFIER`. The hard-wired `bashStruct` /
// `bashStruct_in` / `cmdDelimiter` / `testOperator` sets at
// `:491-494` are populated lowercase only. Wordlist contents must
// be byte-canonical lowercase to match Bash language semantics.
// Same case-sensitive contract documented for [`SCE_PL_WORD`]
// (Perl), [`SCE_P_WORD`] (Python), and [`SCE_LUA_WORD`] (Lua).
//
// **Single wordlist class.** `bashWordListDesc[]` at
// `LexBash.cxx:205-208` declares one named slot, `"Keywords"`,
// terminated by `nullptr`. `LexerBash::WordListSet` at `:558-572`
// only dispatches `case 0: wordListN = &keywords; break;` and
// no-ops for any other `n`. So unlike Lua (2 classes) / Python
// (2 classes) / SQL (5 classes), Bash exposes exactly ONE keyword
// surface. The lexer ships hard-wired short lists for syntactic
// structure (`bashStruct = "if elif fi while until else then do
// done esac eval"` at `:492`, `bashStruct_in = "for case select"`
// at `:493`) matched independently of the user wordlist at
// `:706, :713` — so a user-supplied class 0 list should populate
// builtins / reserved words NOT already in `bashStruct` (no
// behavioural change from duplicates, but spec noise).
//
// **No `SCE_SH_HERE_QQ` / `SCE_SH_HERE_QX` exist.** Unlike LexPerl
// (which splits heredoc bodies into `SCE_PL_HERE_Q` /
// `SCE_PL_HERE_QQ` / `SCE_PL_HERE_QX` based on the delimiter's
// quoting style), LexBash emits a single `SCE_SH_HERE_Q` (state
// 13) for every heredoc body byte regardless of whether the
// delimiter was `EOF`, `'EOF'`, `"EOF"`, or `\EOF`. The
// quoted-vs-unquoted distinction is tracked INTERNALLY via the
// `HereDocCls::Quoted` flag at `LexBash.cxx:594` (set when the
// delimiter starts with `'` or `"`) and `HereDocCls::Escaped` at
// `:595` (set when the delimiter contains a backslash); both
// flags affect ONLY behaviour inside the body — at `:906-908`
// nested `$var` / `` ` `` expansions are suppressed when the
// body is quoted/escaped. The emitted STYLE stays
// `SCE_SH_HERE_Q`. So Code++ MUST NOT speculatively declare a
// `SCE_SH_HERE_QQ` or `SCE_SH_HERE_QX` — they don't exist in
// the lexer and adding them would mislead future contributors.
// Opening `<<` / `<<-` delimiter line (and the closing-delimiter
// line per `:896`) gets `SCE_SH_HERE_DELIM` (state 12). Here-string
// `<<<` is consumed without a body state per `:828-830`.
//
// **`SCE_SH_SCALAR` (9) vs `SCE_SH_PARAM` (10) distinction.** Both
// represent variable expansion but at different lexical scopes.
// SCALAR is the bare `$var` / `$1` / `$@` form — the lexer enters
// it at `:356` and consumes one identifier-shaped run via the
// `setParam` character class at `:582`; no closing delimiter
// (the comment at `:386-389` is explicit: "scalar has no
// delimiter pair"). PARAM is the braced `${...}` parameter
// expansion form — the lexer upgrades SCALAR → PARAM at
// `:358-360` when the character after `$` is `{`, pushes a
// balanced `{`…`}` region onto the `QuoteStack` at `:397-399`,
// and may nest other expansions inside per `:912`. Both route
// to `StyleSlot::Lifetime` in `BASH_STYLES` (matches the Perl
// SCALAR / ARRAY / HASH / SYMBOLTABLE → Lifetime collapse at
// `crates/ui_win32/src/lib.rs:4211-4214` — sigil-tagged variable
// archetype) but the lexer-level distinction is real and worth
// flagging for future palette tweaks.
//
// **`$(cmd)` styling depends on a property default.** The lexer
// recognises three modes for `$()` command substitution via the
// `lexer.bash.command.substitution` property (`LexBash.cxx:231-234`):
// 0 = `Backtick` (default), 1 = `Inside`, 2 = `InsideTrack`. At
// the default 0, `$(cmd)` is styled as `SCE_SH_BACKTICKS` end-to-
// end — same slot as `` `cmd` ``, matching N++'s out-of-box
// behaviour. Code++'s wiring leaves this property at default,
// keeping emitted styles in the 0..=13 range and avoiding the
// `commandSubstitutionFlag = 0x40` OR-shift at `:92` that would
// produce styles in 64..=127. A future property flip would
// require re-evaluating `BASH_STYLES` — flagged here so the
// next maintainer sees the gotcha.
//
// **`SCE_SH_DEFAULT` (0) and `SCE_SH_IDENTIFIER` (8) intentionally
// unmapped.** Universal-omission pattern: bare-default and post-
// keyword-miss identifier render at STYLE_DEFAULT (the user's
// chosen foreground). Matches `SCE_C_DEFAULT` / `SCE_C_IDENTIFIER`,
// `SCE_PL_DEFAULT` / `SCE_PL_IDENTIFIER`, `SCE_P_DEFAULT` /
// `SCE_P_IDENTIFIER`, `SCE_LUA_DEFAULT` / `SCE_LUA_IDENTIFIER`
// precedent. `SCE_SH_IDENTIFIER` is the dominant fall-through —
// emitted by the lexer at `LexBash.cxx:677, :694, :703, :710,
// :717, :723, :728, :796, :1012, :1028, :1044, :1047, :1050, :1080,
// :1099` (including the reclassification from `SCE_SH_NUMBER` when
// a `.` is encountered at `:793-797`, since bash has no float
// literals).
//
// **`SCE_SH_ERROR` (1) intentionally unmapped.** Joins the deferred-
// Error-slot migration list. The lexer emits it at `:792` for
// out-of-range base-N digits (e.g. `2#3`), at `:862-864` for
// unterminated heredoc bodies, and at `:792` for malformed
// numeric literals. Synthesising an ad-hoc red mapping here
// creates palette drift that the `StyleSlot::Error` migration
// would have to clean up — leave unmapped (falls through to
// STYLE_DEFAULT) and migrate the whole cluster (Perl ERROR +
// Lua STRINGEOL + Python STRINGEOL + ...) together.
pub const SCE_SH_DEFAULT: usize = 0;
pub const SCE_SH_ERROR: usize = 1;
pub const SCE_SH_COMMENTLINE: usize = 2;
pub const SCE_SH_NUMBER: usize = 3;
pub const SCE_SH_WORD: usize = 4;
pub const SCE_SH_STRING: usize = 5;
pub const SCE_SH_CHARACTER: usize = 6;
pub const SCE_SH_OPERATOR: usize = 7;
pub const SCE_SH_IDENTIFIER: usize = 8;
pub const SCE_SH_SCALAR: usize = 9;
pub const SCE_SH_PARAM: usize = 10;
pub const SCE_SH_BACKTICKS: usize = 11;
pub const SCE_SH_HERE_DELIM: usize = 12;
pub const SCE_SH_HERE_Q: usize = 13;

// LexNsis style indices. 19 contiguous slots (0..=18) covering
// the NSIS installer-script lexer's full emission set: `;` and `#`
// line comments (COMMENT) plus `/* ... */` block comments
// (COMMENTBOX), three independent quoted-string flavours
// (STRINGDQ / STRINGLQ / STRINGRQ for `"..."` / `` `...` `` / `'...'`),
// decimal-only numeric literals (NUMBER), wordlist-classified
// instruction / variable / label / user-defined tokens (FUNCTION /
// VARIABLE / LABEL / USERDEFINED), hard-wired structural keyword
// pairs (`Section`/`SectionEnd`, `SubSection`/`SubSectionEnd`,
// `SectionGroup`/`SectionGroupEnd`, `PageEx`/`PageExEnd`,
// `Function`/`FunctionEnd` → SECTIONDEF / SUBSECTIONDEF /
// SECTIONGROUP / PAGEEX / FUNCTIONDEF), the `!`-prefixed
// preprocessor / macro-definition family (`!macro`/`!macroend`
// → MACRODEF; `!if`/`!ifdef`/`!ifndef`/`!else`/`!endif`/
// `!ifmacrodef`/`!ifmacrondef` → IFDEFINEDEF), and the `$var` /
// `${var}` interpolation that fires inside an active string body
// (STRINGVAR). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 859-877, the in-source
// state-comment table at `vendor/lexilla/lexers/LexNsis.cxx`
// lines 36-55, the wordlist-descriptions array `nsisWordLists[]`
// at `LexNsis.cxx:658-663`, and the
// `LexerModule lmNsis(SCLEX_NSIS, ColouriseNsisDoc, "nsis",
// FoldNsisDoc, nsisWordLists)` registration at `LexNsis.cxx:666`.
//
// **Case-sensitivity is property-driven.** The classifier at
// `LexNsis.cxx:178` reads `styler.GetPropertyInt("nsis.ignorecase")`
// and, when set to `1`, both lowercases the buffered token before
// `InList` (`:198-202`) and routes all hard-wired keyword matches
// through `NsisCmp` (`:107-113`) which dispatches to
// `CompareCaseInsensitive`. The default is `0` (strict `strcmp`).
//
// **Code++ runs the lexer at default `nsis.ignorecase=0`.** The
// `LangTheme` struct has no `properties` slot today, so
// `apply_lang` does NOT issue `SCI_SETPROPERTY` for either
// `nsis.ignorecase` or `nsis.uservars`. To still highlight the
// canonical NSIS source (`Section` / `MessageBox` / `$INSTDIR`
// — all mixed-case by convention), the user wordlists ship in
// **canonical mixed-case** matching the on-disk spelling. The
// lexer's byte-exact `strcmp` then matches `MessageBox` in source
// against `MessageBox` in the wordlist. This is the same posture
// Notepad++ uses in practice (its `langs.model.xml` ships
// mixed-case `instre1`/`instre2` lists, not lowercased ones —
// the `nsis.ignorecase=1` claim in N++'s docs is stale relative
// to its shipped wordlist content). Source written in
// non-canonical case (e.g. `MESSAGEBOX`, `messagebox`) will not
// highlight until a future commit adds the
// `LangTheme::properties: &[(&str, &str)]` slot and installs
// `nsis.ignorecase=1`; see the `lexers-coverage.md` follow-up
// tracker. Sourcing the wordlist in mixed-case is the strictly
// better default of the two: it works against the on-disk
// convention without any plumbing changes.
//
// **Four wordlist classes.** `nsisWordLists[]` at
// `LexNsis.cxx:658-663` declares four named slots, terminated by
// `nullptr`:
//   - class 0 `"Functions"` → `SCE_NSIS_FUNCTION` (`:233-234`).
//     Semantically the **instruction set** — NSIS built-in
//     commands like `MessageBox` / `WriteRegStr` / `File` /
//     `SetOutPath`, plus `!`-directives NOT in the hard-wired set
//     (`!define` / `!include` / `!insertmacro` / `!undef` /
//     `!system` / `!warning` / `!error` / `!verbose` / `!pragma`).
//     Naming is misleading — this slot covers far more than
//     traditional "functions".
//   - class 1 `"Variables"` → `SCE_NSIS_VARIABLE` (`:236-237`).
//     Predefined NSIS variables and constants (`$INSTDIR`,
//     `$WINDIR`, `$PROGRAMFILES`, `${NSISDIR}`, the `$0..$9` /
//     `$R0..$R9` numbered registers).
//   - class 2 `"Lables"` (**sic — upstream typo preserved**) →
//     `SCE_NSIS_LABEL` (`:239-240`). User-supplied label / goto-
//     target names. **Do not silently correct to `"Labels"`** —
//     Lexilla dispatches on the exact string at `:191-194` and
//     a corrected name would never match. Notepad++ ships this
//     class empty in `langs.model.xml`; Code++ matches.
//   - class 3 `"UserDefined"` → `SCE_NSIS_USERDEFINED` (`:242-243`).
//     User-supplied `!define`d / `!macro`-defined names the user
//     wants explicitly highlighted. Notepad++ ships empty by
//     default; Code++ matches.
// Unlike Bash (1 class), Lua (2 classes), Python (2 classes), or
// SQL (5 classes), NSIS exposes exactly four — and Code++
// populates classes 0 and 1 only.
//
// **Seven hard-wired keyword groups bypass the wordlist entirely.**
// `classifyWordNsis` at `LexNsis.cxx:206-231` short-circuits on
// these before consulting any user wordlist, dispatching directly
// to their dedicated SCE states:
//   - `!macro` / `!macroend` → `SCE_NSIS_MACRODEF` (`:206-207`)
//   - `!ifdef` / `!ifndef` / `!endif` → `SCE_NSIS_IFDEFINEDEF`
//     (`:209-210`)
//   - `!if` / `!else` → `SCE_NSIS_IFDEFINEDEF` (`:212-213`)
//   - `!ifmacrodef` / `!ifmacrondef` → `SCE_NSIS_IFDEFINEDEF`
//     (`:215-216`)
//   - `SectionGroup` / `SectionGroupEnd` → `SCE_NSIS_SECTIONGROUP`
//     (`:218-219`)
//   - `Section` / `SectionEnd` → `SCE_NSIS_SECTIONDEF` (`:221-222`)
//   - `SubSection` / `SubSectionEnd` → `SCE_NSIS_SUBSECTIONDEF`
//     (`:224-225`)
//   - `PageEx` / `PageExEnd` → `SCE_NSIS_PAGEEX` (`:227-228`)
//   - `Function` / `FunctionEnd` → `SCE_NSIS_FUNCTIONDEF` (`:230-231`)
// These tokens MUST NOT be duplicated in the class-0 `Functions`
// wordlist — they're shadowed by the earlier branch (no behavioural
// change, but spec noise). Conversely, every theme MUST colour
// the seven dedicated `*DEF` / `SECTIONGROUP` / `PAGEEX` /
// `MACRODEF` / `IFDEFINEDEF` slots explicitly — otherwise common
// tokens like `Section` / `!macro` / `Function` render at
// `STYLE_DEFAULT`.
//
// **Three independent string-flavour states.** Unlike Lua's
// LITERALSTRING + CHARACTER + STRING triple that collapses to
// one `String` slot, LexNsis emits three distinct states for
// the three quote characters NSIS accepts:
//   - `SCE_NSIS_STRINGDQ` (state 2) — `"..."` double-quoted, opened
//     at `:322-326` and closed at `:388-393`.
//   - `SCE_NSIS_STRINGLQ` (state 3) — `` `...` `` left-quoted
//     (backtick), opened at `:335-342` and closed at `:395-400`.
//   - `SCE_NSIS_STRINGRQ` (state 4) — `'...'` right-quoted
//     (single), opened at `:327-334` and closed at `:402-407`.
// All three route to `StyleSlot::String` in `NSIS_STYLES` —
// uniform-archetype collapse matching the Lua precedent.
// Strings support `$\` (dollar-backslash) escape at `:385-386`
// so `$\"` does not close a DQ string, and a trailing `\` at
// end-of-line at `:409-443` continues the string onto the next
// line.
//
// **`SCE_NSIS_STRINGVAR` (13) is the `$var` interpolation inside
// an active string body.** Emitted at `:518` (`$\` escape
// sequence inside string), `:527-528` (bare `$var` whose
// identifier matches the class-1 `Variables` wordlist), `:530`
// (bare `$var` user variable when `nsis.uservars=1`), and
// `:536` (`${var}` brace-form interpolation). Direct parallel
// to Bash's `SCALAR` / `PARAM` mid-string handling — same
// archetype, routes to `StyleSlot::Lifetime` matching the bare
// `SCE_NSIS_VARIABLE` routing.
//
// **`nsis.uservars` opt-in.** A second runtime property at
// `LexNsis.cxx:181-185` (read at `:184, :508`). When set to `1`,
// any `$`-prefixed token of valid `isNsisChar` characters
// (`[A-Za-z0-9._]`) is treated as a variable even if not in the
// `Variables` wordlist (`:252-266`) — both at top level (→
// `SCE_NSIS_VARIABLE`) and inside string bodies (→
// `SCE_NSIS_STRINGVAR` at `:529-530`). Default is `0` (off);
// Notepad++ ships `1` (on); Code++'s `apply_lang` MUST set
// `nsis.uservars=1` via `SCI_SETPROPERTY` to match N++ behaviour
// — without it, `$MyCustomVar` lexes as `SCE_NSIS_DEFAULT`
// instead of `SCE_NSIS_VARIABLE`, dropping a meaningful styling
// cue.
//
// **Decimal-only numeric literals.** `isNsisNumber` at
// `LexNsis.cxx:58-61` accepts strictly `[0-9]`. There is NO
// recognition of `0x...` hex, `0...` octal, or `1.5` float —
// `0x1F` would fail the all-digit test at `:272-279` and fall
// through to whichever path the leading-character classifier
// chooses. Detection happens at `:351-352` (single digit + EOL)
// and `:269-283` (multi-digit run inside `classifyWordNsis`).
//
// **No `::` plugin-call recognition.** NSIS source commonly
// writes plugin invocations as `nsExec::Exec` or `StrFunc::*`,
// but `isNsisChar` at `:63-66` excludes `:`, so the `::` breaks
// the identifier. Both halves classify independently against
// the wordlists. To highlight plugin calls, host wordlists must
// contain the bare names (`nsExec`, `Exec`, `StrFunc`, etc.) —
// the qualified `nsExec::Exec` form will never match a single
// wordlist entry.
//
// **No label-trailing-colon detection.** Labels go through
// class 2 `Lables` only — there is no automatic recognition
// of "identifier followed by `:`" as a label definition. User
// must enumerate label names in class 2 for them to highlight.
// The `:` terminator is not in `isNsisChar` so it ends the
// identifier; the bare name (without `:`) is what `InList`
// sees. Notepad++ ships class 2 empty — Code++ matches.
//
// **`SCE_NSIS_DEFAULT` (0) intentionally unmapped.** Universal
// background-fall-through convention matching `SCE_C_DEFAULT`,
// `SCE_SH_DEFAULT`, `SCE_P_DEFAULT`, `SCE_LUA_DEFAULT`,
// `SCE_L_DEFAULT` precedent. No `SCE_NSIS_ERROR` state exists
// in the lexer — `LexNsis.cxx` has no recovery / malformed-token
// branch (the lexer simply walks back to `SCE_NSIS_DEFAULT` on
// any unmatched character), so no deferred-Error-slot entry is
// needed (contrast with `SCE_SH_ERROR` at `:847` which joins
// the deferred-migration cluster).
//
// **Legacy property API.** LexNsis predates the
// `OptionSet` / `DefineProperty` infrastructure used by newer
// lexers (e.g. LexHTML, LexBash). Properties are read directly
// via `styler.GetPropertyInt(...)` at `:144, :178, :184, :508,
// :566-567`; there is no schema, unknown property keys are
// silently ignored. The full property surface is:
// `nsis.ignorecase`, `nsis.uservars`, `fold`, `fold.at.else`,
// `nsis.foldutilcmd`.
pub const SCE_NSIS_DEFAULT: usize = 0;
pub const SCE_NSIS_COMMENT: usize = 1;
pub const SCE_NSIS_STRINGDQ: usize = 2;
pub const SCE_NSIS_STRINGLQ: usize = 3;
pub const SCE_NSIS_STRINGRQ: usize = 4;
pub const SCE_NSIS_FUNCTION: usize = 5;
pub const SCE_NSIS_VARIABLE: usize = 6;
pub const SCE_NSIS_LABEL: usize = 7;
pub const SCE_NSIS_USERDEFINED: usize = 8;
pub const SCE_NSIS_SECTIONDEF: usize = 9;
pub const SCE_NSIS_SUBSECTIONDEF: usize = 10;
pub const SCE_NSIS_IFDEFINEDEF: usize = 11;
pub const SCE_NSIS_MACRODEF: usize = 12;
pub const SCE_NSIS_STRINGVAR: usize = 13;
pub const SCE_NSIS_NUMBER: usize = 14;
pub const SCE_NSIS_SECTIONGROUP: usize = 15;
pub const SCE_NSIS_PAGEEX: usize = 16;
pub const SCE_NSIS_FUNCTIONDEF: usize = 17;
pub const SCE_NSIS_COMMENTBOX: usize = 18;

// LexTCL style indices. 22 contiguous slots (0..=21) covering
// the TCL / Tk lexer's full emission set: `#` line comments with
// two command-position variants (COMMENT at command-start,
// COMMENTLINE elsewhere) plus `#~` block comments (BLOCK_COMMENT)
// and `#-` / `##` line-leading box-comment continuations
// (COMMENT_BOX), `"..."` strings (IN_QUOTE) with WORD_IN_QUOTE
// for keyword hits inside the string body, decimal / hex / `\#NN`
// special-form numeric literals (NUMBER), bare-token operator
// emission for brackets / braces / `;` / `,` / `$` / parentheses
// (OPERATOR), unmatched bare identifiers (IDENTIFIER),
// `$var` / `$arr(idx)` variable substitution (SUBSTITUTION) and
// the `${var}` braced form's interior body (SUB_BRACE), `-flag`
// command-option modifiers (MODIFIER), the special `{keyword}`
// exact-brace expansion-keyword class (EXPAND), and the
// nine-class wordlist surface (WORD plus WORD2..WORD8 for
// the secondary user-customisation slots). Cross-referenced
// against `vendor/lexilla/include/SciLexer.h` lines 245-266 and
// the lexer body `vendor/lexilla/lexers/LexTCL.cxx` with the
// `tclWordListDesc[]` descriptor at `LexTCL.cxx:361-372` and the
// `LexerModule lmTCL(SCLEX_TCL, ColouriseTCLDoc, "tcl", 0,
// tclWordListDesc)` registration at `LexTCL.cxx:375`.
//
// **Case-sensitive lexer.** `LexTCL.cxx` does NO case folding —
// the identifier text is collected raw via
// `sc.GetCurrent(w, sizeof(w))` at `LexTCL.cxx:152` and the
// `keywords.InList(s)` / `keywords2..9.InList(s)` chain at
// `:160-179` runs byte-exact against the source spelling
// (verified: no `MakeLowerCase` / `tolower` / `GetCurrentLowered`
// / `CompareCaseInsensitive` anywhere on the wordlist-match
// path; the only `toupper` call sits in `IsANumberChar` at `:45`
// for the `E` exponent character, unrelated to keywords). TCL
// the language is case-sensitive — `set` and `SET` are distinct
// commands at the interpreter level — so the lexer's byte-exact
// posture matches TCL semantics. Wordlists installed against
// this lexer MUST store source-canonical lowercase spellings
// (`puts` / `set` / `if` / `proc` / `while` / `foreach`, etc.) —
// uppercase entries never match a TCL author's source. Same
// byte-exact contract as `LUA_KEYWORDS` / `PERL_KEYWORDS`.
//
// **The only token normalisation before lookup is stripping
// leading `::` (namespace separators)** at `LexTCL.cxx:156-157`
// — `::set` and `::ns::cmd` have the leading colons skipped so
// the bare `set` / `ns::cmd` is what `InList` sees. The
// trailing-`\r` strip at `:154-155` is a CRLF-safety belt, not
// a semantic transformation. Critically, `IsAWordChar` at
// `LexTCL.cxx:32-35` accepts `:` (the namespace separator), so a
// namespaced identifier like `namespace::cmd` traverses as a
// SINGLE identifier token through the wordlist match — wordlist
// entries for namespaced commands need the full `ns::cmd` form
// (contrast with NSIS's `:`-exclusion at `:1015-1022` which
// breaks `nsExec::Exec` into two halves).
//
// **Nine wordlist classes.** `tclWordListDesc[]` at
// `LexTCL.cxx:361-372` declares nine named slots, terminated by
// `0`:
//   - class 0 `"TCL Keywords"`  → `SCE_TCL_WORD`    (`:160-161`).
//     Primary TCL built-in commands — `puts` / `set` / `if` /
//     `while` / `for` / `foreach` / `proc` / `return` / `expr` /
//     `eval` / `catch` / `switch` / etc. The bulk of the
//     vocabulary; theme paints this `Keyword` bold.
//   - class 1 `"TK Keywords"`   → `SCE_TCL_WORD2`   (`:162-163`).
//     Tk widget-creation commands — `button` / `label` / `entry` /
//     `frame` / `toplevel` / `canvas` / `text` / etc.
//   - class 2 `"iTCL Keywords"` → `SCE_TCL_WORD3`   (`:164-165`).
//     `[incr Tcl]` / TclOO extensions — `class` / `inherit` /
//     `method` / `constructor` / `destructor` / etc. Ships
//     empty in N++'s default.
//   - class 3 `"tkCommands"`    → `SCE_TCL_WORD4`   (`:166-167`).
//     Tk geometry-manager / event / window-info subcommands —
//     `pack` / `grid` / `place` / `bind` / `wm` / `winfo` /
//     `bindtags` / `tk_*` / etc. Distinct from class 1 (widget
//     creation) — semantic split matches N++'s `langs.model.xml`.
//   - class 4 `"expand"`        → `SCE_TCL_EXPAND`  (`:168-170`).
//     **Special-context class** — fires ONLY when the token
//     appears literally inside `{token}` with no surrounding
//     whitespace. The check at `:168-170` reads
//     `sc.GetRelative(-strlen(s)-1) == '{' && keywords5.InList(s)
//     && sc.ch == '}'` — a bare `expand_keyword` in code context
//     never matches this class. This is the TCL `{*}` expansion
//     mechanism's lexer hook. Ships empty in N++'s default.
//   - class 5 `"user1"`         → `SCE_TCL_WORD5`   (`:172-173`).
//   - class 6 `"user2"`         → `SCE_TCL_WORD6`   (`:174-175`).
//   - class 7 `"user3"`         → `SCE_TCL_WORD7`   (`:176-177`).
//   - class 8 `"user4"`         → `SCE_TCL_WORD8`   (`:178-179`).
//     All four `user*` slots ship empty in N++'s default — they're
//     user-customisation slots. Unlike Bash (1 class), NSIS (4
//     classes), or Lua (8 classes), TCL exposes exactly nine —
//     and Code++ populates classes 0-3 only, matching N++.
//
// **Wordlist match precedence is asymmetric.** Classes 0-4 are
// checked in an `if / else if` chain at `:160-171` — first match
// wins. Classes 5-8 are checked in a SEPARATE `if / else if`
// chain at `:172-180` AFTER classes 0-4 — a class-5..8 hit
// OVERRIDES any class-0..3 classification via the unconditional
// `if` at `:172` versus the chained `else if` at `:162-167`. Put
// concretely: if `puts` appears in both class 0 (TCL Keywords)
// and class 5 (user1), the user-class hit replaces the TCL-class
// hit and the token paints as `SCE_TCL_WORD5`. The expand-class
// check (`keywords5` at `:168`) is bracketed inside the 0-4
// chain so it does NOT override; it only fires inside `{token}`
// brace context. Wordlist authors must understand: class 5-8
// entries are "force-style this token regardless of any earlier
// classification". Most use cases (and Code++'s shipped default)
// leave classes 5-8 empty.
//
// **`SCE_TCL_WORD_IN_QUOTE` (4) is the single mid-string
// keyword slot — collapses every class hit.** When the lexer
// catches a keyword while `quote` is true (inside `IN_QUOTE`),
// the ternary at `:158, :161-167` emits `WORD_IN_QUOTE`
// regardless of which class matched — there is no
// `WORD2_IN_QUOTE` / `WORD3_IN_QUOTE` / etc. Code++ routes the
// entire slot to `StyleSlot::String` so the in-quote keyword
// hit blends into the surrounding string body rather than
// punching out of it (mirrors Bash's mid-`"..."` SCALAR not
// pulling the string apart).
//
// **`SCE_TCL_SUBSTITUTION` (8) and `SCE_TCL_SUB_BRACE` (9) are
// the variable-reference pair.** `$var` outside braces lexes
// as `SUBSTITUTION` (entered at `:334` when `sc.chNext != '{'`,
// continues until a non-word char at `:142-144`). `$arr(idx)`
// flips into `OPERATOR` for the `(` then back into
// `SUBSTITUTION` for the index (`:122-139`), with `,` as a
// sub-separator inside the parens. `${var}` enters via `:336-338`
// where the `$` and `{` style as `OPERATOR` and the interior
// styles as `SUB_BRACE` (the `subBrace` flag at `:108-117`
// overrides EVERYTHING including backslash escapes until the
// closing `}`). Both states route to `StyleSlot::Lifetime` —
// sigil-tagged variable archetype, same as Bash SCALAR / PARAM
// and NSIS VARIABLE / STRINGVAR.
//
// **`SCE_TCL_MODIFIER` (10) is the `-flag` command-option
// state.** Entered at `:348` via the ternary
// `IsADigit(sc.chNext) ? SCE_TCL_NUMBER : SCE_TCL_MODIFIER` —
// the lexer disambiguates `-1` (number) from `-flag` (option).
// `string match -nocase -- $foo` produces three `MODIFIER`
// tokens. Routed to `StyleSlot::Keyword2` (secondary keyword
// archetype) — option flags appear densely in any TCL command
// invocation, so the secondary-keyword colour signals "this is
// a modifier" without the visual weight of bold.
//
// **`SCE_TCL_EXPAND` (11) — the brace-context-only class.** See
// the class-4 description above. Routed to `StyleSlot::Keyword`
// + bold matching the primary `WORD` archetype — when the
// brace-context check fires, this is the "TCL `{*}` expansion
// keyword".
//
// **Four comment-state cluster.** TCL's comment surface is the
// richest in the framework: `SCE_TCL_COMMENT` (state 1, `#` at
// command-position at `:279-280`), `SCE_TCL_COMMENTLINE` (state 2,
// `#` elsewhere at `:101, :282`), `SCE_TCL_COMMENT_BOX` (state 20,
// `#-` / `##` at line-start with cross-line continuation through
// the `LS_COMMENT_BOX` lineState at `:105, :220, :226, :286`), and
// `SCE_TCL_BLOCK_COMMENT` (state 21, `#~` at line-start at `:284`).
// All four collapse to `StyleSlot::Comment` in the theme —
// uniform-comment convention matching Lua's COMMENT + COMMENTLINE
// + COMMENTDOC triple-collapse. The `expected` flag tracking
// command-position is set after `{` (`:312`), `}` (`:317`), `[`
// (`:321`), `;` (`:329`), and line start with `IsAWordStart` /
// space (`:251`). A bare `#` at column 0 emits `COMMENTLINE`,
// not `COMMENT` — only command-position `#` gets the (state-1)
// promoted form.
//
// **`SCE_TCL_NUMBER` (3) is approximate, not strict.** The
// in-source comment at `LexTCL.cxx:42-43` is explicit: "Not
// exactly following number definition (several dots are seen
// as OK, etc.) but probably enough in most cases."
// `IsANumberChar` at `:41-47` accepts hex digits (via
// `IsADigit(ch, 0x10)`), `E`/`e` exponent, `.`, `-`, `+`.
// Detection paths: bare-digit start at `:303-304` (when
// `IsADigit(sc.ch) && !IsAWordChar(sc.chPrev)`), `\#NN` form
// at `:239-240`, and a `#`-prefixed hex form when preceded by
// whitespace/operator and followed by a hex digit at `:342-345`.
// There is NO explicit `0x` prefix recognition — the lexer
// relies on `IsADigit(ch, 0x10)` accepting `0`-`9` / `A`-`F` /
// `a`-`f` as the number runs.
//
// **NO dedicated brace-string state.** Brace-grouped `{...}` is
// the TCL deferred-evaluation form, but the lexer treats `{`
// and `}` as `SCE_TCL_OPERATOR` (`:311, :316`) and lexes the
// interior as normal code — fold level increments on `{`
// (`:313`) and decrements on `}` (`:318`). This matches TCL's
// "braces defer evaluation but don't change tokenisation" rule.
// Disambiguating list literals from brace-grouped strings is a
// parser-level concern, not a lexer-level one.
//
// **NO dedicated PROC / proc-definition state.** `proc` is just
// a keyword from class 0 — if the user includes it in TCL
// Keywords (and Code++ does), the `name`, `args`, and `body` of
// `proc name {args} {body}` tokenise as regular identifiers and
// brace-groups. No `SCE_TCL_DEFNAME` analogue exists — contrast
// with Python's `SCE_P_DEFNAME` or Pascal's similar dedicated
// slots.
//
// **NO dedicated `[...]` command-substitution state.** The `[`
// and `]` style as `SCE_TCL_OPERATOR` (`:320-326`); the interior
// recurses through normal lexing with `expected = true` set
// after `[` (`:321`) so the next word is treated as a command.
//
// **NO `if 0 { ... }` dead-code recognition.** The lexer treats
// it as a regular `if` keyword + `0` number + brace block.
// Highlighting "this brace block is dead code" is a parser
// concern.
//
// **`SCE_TCL_DEFAULT` (0) and `SCE_TCL_IDENTIFIER` (7)
// intentionally unmapped.** Universal omission pattern:
// background-text and bare-identifier states render at
// `STYLE_DEFAULT` (the user's chosen foreground) — same
// precedent as `SCE_C_DEFAULT` / `SCE_C_IDENTIFIER`,
// `SCE_PL_DEFAULT` / `SCE_PL_IDENTIFIER`, `SCE_LUA_DEFAULT` /
// `SCE_LUA_IDENTIFIER`. NO `SCE_TCL_ERROR` state exists — the
// lexer has no recovery / malformed-token branch (every
// unmatched character walks back to `SCE_TCL_DEFAULT`), so no
// deferred-Error-slot entry is needed (contrast with
// `SCE_SH_ERROR` / `SCE_LUA_STRINGEOL` which join the deferred-
// migration cluster).
//
// **`SCE_TCL_WORD5..WORD8` (16-19) pre-themed despite empty host
// install.** Code++ ships classes 0-3 today (matching N++
// default); classes 4-8 are left unpopulated. All four `WORD5..8`
// slots still map to `Keyword2` in `TCL_STYLES` for forward-compat
// — costs four table rows, gains zero-effort activation if a
// future commit adds `TCL_USER1` / `_USER2` / etc. Same
// forward-compat pattern as the Lua WORD2..WORD8 pre-theming and
// the Python ATTRIBUTE pre-theming.
//
// **Two runtime properties — `fold.comment` / `fold.compact`.**
// Read at `LexTCL.cxx:51-52` via the legacy `GetPropertyInt`
// API (no `DefineProperty` schema). Both control folding only —
// neither affects token emission. Default `fold.comment=0`
// (off), default `fold.compact=1` (on). `LangTheme` has no
// `properties` slot today, so Code++ runs both at the lexer
// default — same posture as NSIS's `nsis.ignorecase` /
// `nsis.uservars`. The deferred properties-slot follow-up
// referenced in the NSIS banner generalises across this lexer
// too, but folding behaviour is not the gating concern (no
// token-emission impact). Tracked in `docs/lexers-coverage.md`
// for the future folding-host wiring commit.
pub const SCE_TCL_DEFAULT: usize = 0;
pub const SCE_TCL_COMMENT: usize = 1;
pub const SCE_TCL_COMMENTLINE: usize = 2;
pub const SCE_TCL_NUMBER: usize = 3;
pub const SCE_TCL_WORD_IN_QUOTE: usize = 4;
pub const SCE_TCL_IN_QUOTE: usize = 5;
pub const SCE_TCL_OPERATOR: usize = 6;
pub const SCE_TCL_IDENTIFIER: usize = 7;
pub const SCE_TCL_SUBSTITUTION: usize = 8;
pub const SCE_TCL_SUB_BRACE: usize = 9;
pub const SCE_TCL_MODIFIER: usize = 10;
pub const SCE_TCL_EXPAND: usize = 11;
pub const SCE_TCL_WORD: usize = 12;
pub const SCE_TCL_WORD2: usize = 13;
pub const SCE_TCL_WORD3: usize = 14;
pub const SCE_TCL_WORD4: usize = 15;
pub const SCE_TCL_WORD5: usize = 16;
pub const SCE_TCL_WORD6: usize = 17;
pub const SCE_TCL_WORD7: usize = 18;
pub const SCE_TCL_WORD8: usize = 19;
pub const SCE_TCL_COMMENT_BOX: usize = 20;
pub const SCE_TCL_BLOCK_COMMENT: usize = 21;

// LexLaTeX style indices. 13 contiguous slots (0..=12) covering
// the LaTeX lexer's full emission set: `%` line comments, `$...$`
// / `\(...\)` math and `$$...$$` / `\[...\]` display-math regions,
// `\begin{env}` / `\end{env}` tag pairs (TAG / TAG2), the eight
// escaped specials `\#` / `\$` / `\%` / `\&` / `\_` / `\{` / `\}`
// / `\<space>` per `latexIsSpecial`, the verbatim mode (`\verb`,
// `\begin{verbatim}`, `\begin{lstlisting}`), the `[opt]` option
// span on command parameters, and the recovery state for
// malformed escapes. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 492-504 and
// `vendor/lexilla/lexers/LexLaTeX.cxx` lines 195-501.
//
// LexLaTeX is case-sensitive (matches LaTeX semantics:
// `\Begin{equation}` is not the same as `\begin{equation}` —
// the lexer's tag-detection at `LexLaTeX.cxx:158-193` does
// byte-exact `strcmp` against lowercase needles like `"\\begin"`
// / `"{verbatim}"` / `"{math}"`).
//
// **Zero-wordlist surface.** `LexLaTeX.cxx:561` declares
// `emptyWordListDesc = {0}` and the `LexerModule` registration
// at `:565` passes that sentinel; the lexer never calls
// `keywords.InList`. The host must NOT install keyword lists
// against the `"latex"` lexer name — they'd be silently dropped.
// `LATEX_THEME.keywords` is `&[]` by design.
//
// **Math states are doubled (MATH / MATH2).** MATH covers
// `$...$` and `\(...\)` (inline); MATH2 covers `$$...$$` and
// `\[...\]` (display) and the named math environments in
// `mathEnvs[]` at `LexLaTeX.cxx:116-129`. Both route to the
// same `StyleSlot::String` slot — math content is a literal
// region semantically, painted the same way strings are.
//
// **Comment states are doubled (COMMENT / COMMENT2).** COMMENT
// is `%`-to-EOL line comment; COMMENT2 is `\begin{comment}` /
// `\end{comment}` block comment from the `comment` package.
// Both → `StyleSlot::Comment`.
//
// **SCE_L_DEFAULT and SCE_L_ERROR intentionally unmapped.**
// DEFAULT is the plain-prose slot, falls through to
// `STYLE_DEFAULT`. ERROR (state 12) is the recovery slot for
// malformed `\` escapes, EOL-in-`\verb`, EOL-in-`\<command>`
// inside math mode (`LexLaTeX.cxx:246, 326, 338, 364, 406,
// 477`) — joins the deferred-Error-slot migration list.
pub const SCE_L_DEFAULT: usize = 0;
pub const SCE_L_COMMAND: usize = 1;
pub const SCE_L_TAG: usize = 2;
pub const SCE_L_MATH: usize = 3;
pub const SCE_L_COMMENT: usize = 4;
pub const SCE_L_TAG2: usize = 5;
pub const SCE_L_MATH2: usize = 6;
pub const SCE_L_COMMENT2: usize = 7;
pub const SCE_L_VERBATIM: usize = 8;
pub const SCE_L_SHORTCMD: usize = 9;
pub const SCE_L_SPECIAL: usize = 10;
pub const SCE_L_CMDOPT: usize = 11;
pub const SCE_L_ERROR: usize = 12;

// LexLisp style indices. 12 public style slots numbered 0..=12 with a
// deliberate gap at state 7 — the Common Lisp / Scheme S-expression
// lexer covers `;`-line comments and `#| ... |#` block comments,
// decimal and radix-prefixed numeric literals (`#x`, `#o`, `#b`,
// `#NrDDD`), two wordlist classes (`KEYWORD` for functions / special
// operators, `KEYWORD_KW` for `&`-prefixed lambda-list markers),
// `:kw` / `'quoted` sigil-tagged symbols (SYMBOL), `"..."` strings
// (STRING) plus the never-emitted unterminated-string error indicator
// (STRINGEOL), the `(` `)` `[` `]` `{` `}` `'` `` ` `` punctuation
// (OPERATOR), the fall-through identifier state (IDENTIFIER), and
// the earmuff + reader-macro-result state (SPECIAL). Cross-referenced
// against `vendor/lexilla/include/SciLexer.h:670-681` and the lexer
// body `vendor/lexilla/lexers/LexLisp.cxx:50-235`; wordlist descriptor
// at `LexLisp.cxx:280-284`; `LexerModule lmLISP(SCLEX_LISP,
// ColouriseLispDoc, "lisp", FoldLispDoc, lispWordListDesc)` at
// `LexLisp.cxx:286`. Language name string for `SCI_SETILEXER` lookup:
// `"lisp"`.
//
// **Two wordlist classes.** `lispWordListDesc[]` at
// `LexLisp.cxx:280-284` declares exactly two entries: index 0
// "Functions and special operators" → `SCE_LISP_KEYWORD` via
// `classifyWordLisp` (`LexLisp.cxx:64-65`), index 1 "Keywords" →
// `SCE_LISP_KEYWORD_KW` via `:66-67`. First-match-wins chain — class 0
// is checked before class 1, so a token duplicated across classes
// silently demotes the class-1 entry. Contrast Bash (1 class), NSIS
// (4 classes), TCL (9 classes), Lua (8 classes). No `OptionSet` /
// `PropertySet` — this is a legacy classic-Accessor lexer,
// `SCI_SETPROPERTY` calls into `lisp` are no-ops.
//
// **Byte-exact case-sensitive lexer.** `classifyWordLisp` at
// `LexLisp.cxx:50-75` builds its token buffer via raw
// `s[i] = styler[start + i]` (`:56`) — no lowercasing. `WordList::InList`
// does byte-equality; `LexerBase::WordListSet` passes the default
// `lowerCase = false` to `WordList::Set`. Grep of `LexLisp.cxx` for
// `MakeLowerCase|tolower|GetCurrentLowered|CaseInsensitive` returns
// zero matches. Common Lisp's canonical case-insensitivity is a
// reader-level property (`READTABLE-CASE :UPCASE`); the lexer does
// not simulate it. Ship wordlists in the exact byte-case the buffer
// will carry — by CL source convention that is lowercase (`defun`,
// `let`, `lambda`). Same byte-exact contract as LUA_KEYWORDS /
// PERL_KEYWORDS / TCL_KEYWORDS / BASH_KEYWORDS.
//
// **State 7 is a permanent hole in the public numbering.**
// `SciLexer.h:676` declares `SCE_LISP_STRING = 6`; the very next
// line `:677` declares `SCE_LISP_STRINGEOL = 8`. There is no
// `SCE_LISP_*` constant with value 7 anywhere in the Scintilla /
// Lexilla source tree — unlike Bash (`SCE_SH_CHARACTER = 6`), Lua
// (`SCE_LUA_CHARACTER = 7`), Perl (`SCE_PL_CHARACTER = 7`), Python
// (`SCE_P_CHARACTER = 4`), Lisp has no CHARACTER slot in the public
// surface. The `'x` form is the QUOTE reader-macro — an `'` byte
// emitted as OPERATOR (`LexLisp.cxx:120, 140, 202`) followed by a
// SYMBOL run (`:122, 142, 204`), not a character literal. The `#\c`
// character-literal reader-macro emits SPECIAL via an internal-only
// state (see next paragraph). The `pub const` block below reflects
// the gap literally — number 6, then jump to 8, no state-7 stub.
//
// **STRINGEOL (8) is public-declared but never emitted.**
// `SciLexer.h:677` declares the constant, but grep of `LexLisp.cxx`
// for `SCE_LISP_STRINGEOL` returns zero hits — the STRING block at
// `LexLisp.cxx:220-229` closes only on unescaped `"`, has no `atEOL`
// branch, and lets an unterminated string linger in
// `state == SCE_LISP_STRING` until the final `styler.ColourTo` at
// `:234`. Constant is included in the FFI surface for header parity
// but unmapped by `LISP_STYLES` (deferred Error slot per SCE_SH_ERROR
// / SCE_LUA_STRINGEOL / SCE_L_ERROR precedent).
//
// **Internal-only states 29 / 30 / 31 are `#define`d PRIVATELY inside
// `LexLisp.cxx:32-34`:**
//     #define SCE_LISP_CHARACTER 29
//     #define SCE_LISP_MACRO 30
//     #define SCE_LISP_MACRO_DISPATCH 31
// These are NOT in `SciLexer.h` and MUST NOT be exported from
// `scintilla-sys`. They are transient state markers the lexer walks
// through while parsing `#| … |#` (block comment), `#x` / `#o` / `#b`
// (radix macros), and `#\c` (character literals) at
// `LexLisp.cxx:106, 145-176, 179-194`; every transition emits a
// DIFFERENT public style (`SCE_LISP_MULTI_COMMENT`, `SCE_LISP_SPECIAL`,
// or `SCE_LISP_OPERATOR`) via `styler.ColourTo(..., <public>)`.
// Values 29/30/31 fall outside the SciLexer.h public range and would
// resolve to `STYLE_DEFAULT` if they ever escaped — the design intent
// is "never emitted as final style". Do not tempt future contributors
// to add pub consts for them. Contrast TCL where
// `SCE_TCL_WORD_IN_QUOTE` is a public state currently never emitted;
// Lisp's 29/30/31 are stricter — they are `.cxx`-private.
pub const SCE_LISP_DEFAULT: usize = 0;
pub const SCE_LISP_COMMENT: usize = 1;
pub const SCE_LISP_NUMBER: usize = 2;
pub const SCE_LISP_KEYWORD: usize = 3;
pub const SCE_LISP_KEYWORD_KW: usize = 4;
pub const SCE_LISP_SYMBOL: usize = 5;
pub const SCE_LISP_STRING: usize = 6;
// State 7 intentionally absent — SciLexer.h:676-677 jumps 6 → 8. See banner.
pub const SCE_LISP_STRINGEOL: usize = 8;
pub const SCE_LISP_IDENTIFIER: usize = 9;
pub const SCE_LISP_OPERATOR: usize = 10;
pub const SCE_LISP_SPECIAL: usize = 11;
pub const SCE_LISP_MULTI_COMMENT: usize = 12;

// LexAsm style indices. 17 contiguous slots (0..=16) — the
// generic assembler lexer used by MASM / NASM / GAS-syntax
// buffers via SCLEX_ASM (the SCLEX_AS "secondary" lexer at
// `LexAsm.cxx:523` shares the SAME SCE_ASM_* namespace with a
// different set of default properties; both lex identical
// classification). Cross-referenced against
// `vendor/lexilla/include/LexicalStyles.iface:829-847` and the
// `lexicalClassesAsm[]` array at `vendor/lexilla/lexers/LexAsm.cxx:128-147`.
//
// **State model.** LexAsm's paint loop is a classic Scintilla
// stream lexer (`LexerAsm::Lex` at `LexAsm.cxx:274-434`; the
// folder at `:440-518` is separate):
//
//   - DEFAULT (0) is transient — the lexer walks back to it after
//     completing OPERATOR / NUMBER / IDENTIFIER / string bodies.
//   - COMMENT (1) is the `;`-to-EOL line comment (default in
//     MASM/NASM; GAS's `#` variant comes in via the `SCLEX_AS`
//     sibling with commentChar='#'). Termination at
//     `LexAsm.cxx:296-298` (walks back to DEFAULT on line-start
//     reset); entry at `:415-416` (`sc.ch == commentCharacter`
//     inside the DEFAULT state).
//   - NUMBER (2) covers digit-started literals plus the
//     `.<digit>` float-literal head — entry at `:417-418`
//     (`IsADigit(sc.ch) || (sc.ch == '.' && IsADigit(sc.chNext))`),
//     termination at `:323-327` (walks back on `!IsAWordChar`).
//     No explicit hex/binary/octal parser — the lexer just runs
//     as many `IsAWordChar` characters as follow, so
//     `0xFF`, `0b1010`, `1234h`, `77o` all lex as NUMBER by
//     virtue of being digit-started + all `IsAWordChar` bytes.
//   - STRING (3) is the double-quoted string with `\`-escape
//     handling at `:370-380`. CHARACTER (12) is the single-quoted
//     equivalent at `:383-393`. STRINGBACKQUOTE (16) is the
//     back-quoted form (uncommon; some GAS macro dialects).
//     STRINGEOL (13) is the "hit end-of-line inside an open
//     string" error state — three `ChangeState(SCE_ASM_STRINGEOL)`
//     call sites at `:378` (STRING), `:391` (CHARACTER),
//     `:404` (STRINGBACKQUOTE).
//   - OPERATOR (4) covers Assembly's punctuation set — the 18
//     characters `IsAsmOperator` at `:50-55` accepts:
//     `* / - + ( ) = ^ [ ] < & > , | ~ % :`. Notable
//     omissions: `!` and `{` `}` are NOT operators (they fall
//     through to whatever the surrounding state emits); `?` is
//     a WORD character per `IsAWordChar` at `:42`, not an
//     operator; `.` is deliberately kept out of the operator set
//     (see `:53` comment) so it can start identifiers (GAS
//     pseudo-ops) and numbers (floats).
//   - IDENTIFIER (5) is the transient state during word scan;
//     the inline classifier at `:329-358` promotes the completed
//     identifier to CPUINSTRUCTION (6) / MATHINSTRUCTION (7) /
//     REGISTER (8) / DIRECTIVE (9) / DIRECTIVEOPERAND (10) /
//     EXTINSTRUCTION (14) via the first-match `InList` chain
//     rooted at `:335` (`cpuInstruction.InList(s)`); any token
//     not in any wordlist stays IDENTIFIER — that's the archetype
//     for labels, symbols, macros.
//
// **Case handling.** The classifier calls
// `GetCurrentLowered(s, sizeof(s))` at `:332` before every
// `InList` check, so wordlists MUST be lowercase — the source
// token "MOV" / "mov" / "Mov" all match a lowercase "mov"
// wordlist entry. Contrast with LexLisp's byte-exact case-
// sensitive path (`SCE_LISP_KEYWORD` requires an exact-case
// match, so wordlists ship lowercase-only). Both lexers land on
// "lowercase wordlist" as the ergonomic authoring contract.
//
// **The `comment` directive.** MASM's `COMMENT <delim>...<delim>`
// block-comment directive triggers a special path at
// `LexAsm.cxx:350-356`: when the just-classified DIRECTIVE token
// equals literal `"comment"`, the lexer eats whitespace and then
// enters COMMENTDIRECTIVE (15) until it sees the delimiter char
// again (default `~`, configurable via
// `lexer.asm.comment.delimiter`). This means `comment` MUST
// appear in the class-3 (`Directives`) wordlist for the special
// path to fire — omit it and MASM `COMMENT` blocks lex as
// consecutive IDENTIFIERs.
//
// **COMMENTBLOCK (11) is empty state — reserved for a "future
// GNU as colouring" comment at `LexAsm.cxx:6`.** The lexer never
// enters this state today; the constant is retained for API
// stability and forward-compat but unused.
//
// **Wordlist classes.** `asmWordListDesc[]` at
// `LexAsm.cxx:80-90` declares eight classes:
//   - class 0 "CPU instructions"       → SCE_ASM_CPUINSTRUCTION
//   - class 1 "FPU instructions"       → SCE_ASM_MATHINSTRUCTION
//   - class 2 "Registers"              → SCE_ASM_REGISTER
//   - class 3 "Directives"             → SCE_ASM_DIRECTIVE
//   - class 4 "Directive operands"     → SCE_ASM_DIRECTIVEOPERAND
//   - class 5 "Extended instructions"  → SCE_ASM_EXTINSTRUCTION
//   - class 6 "Directives4Foldstart"   → fold-only (empty here)
//   - class 7 "Directives4Foldend"     → fold-only (empty here)
// Classes 6/7 drive syntax-based folding via
// `LexAsm.cxx:490-500`; folding is enabled via the universal
// `fold` property but the empty wordlists mean no
// directive-pair folding fires (indentation-based folding is
// still available via Scintilla's other fold logic if the user
// wants it).
pub const SCE_ASM_DEFAULT: usize = 0;
pub const SCE_ASM_COMMENT: usize = 1;
pub const SCE_ASM_NUMBER: usize = 2;
pub const SCE_ASM_STRING: usize = 3;
pub const SCE_ASM_OPERATOR: usize = 4;
pub const SCE_ASM_IDENTIFIER: usize = 5;
pub const SCE_ASM_CPUINSTRUCTION: usize = 6;
pub const SCE_ASM_MATHINSTRUCTION: usize = 7;
pub const SCE_ASM_REGISTER: usize = 8;
pub const SCE_ASM_DIRECTIVE: usize = 9;
pub const SCE_ASM_DIRECTIVEOPERAND: usize = 10;
pub const SCE_ASM_COMMENTBLOCK: usize = 11;
pub const SCE_ASM_CHARACTER: usize = 12;
pub const SCE_ASM_STRINGEOL: usize = 13;
pub const SCE_ASM_EXTINSTRUCTION: usize = 14;
pub const SCE_ASM_COMMENTDIRECTIVE: usize = 15;
pub const SCE_ASM_STRINGBACKQUOTE: usize = 16;

// LexDiff style indices. 12 contiguous slots (0..=11) — the
// smallest lexer family in Lexilla. LexDiff has no tokeniser in
// the usual sense: `ColouriseDiffLine` at `LexDiff.cxx:38-101`
// inspects the leading character(s) of each line and colours the
// entire line with one style, so every SCE_DIFF_* index below
// corresponds to a **line archetype**, not a token type.
//
// Style semantics (paint-loop citations reference LexDiff.cxx):
//   - DEFAULT (0)                — context / unchanged line
//                                  (` ` prefix). Fall-through at
//                                  `:98-99`.
//   - COMMENT (1)                — free-text preamble ("Only in
//                                  ...", "Binary file ..."). The
//                                  classifier's catch-all at
//                                  `:96-97` for lines that don't
//                                  match a diff-format prefix.
//   - COMMAND (2)                — `diff ...` (GNU diff invocation)
//                                  and `Index: ...` (Subversion
//                                  header). Emitted at `:43-46`.
//   - HEADER (3)                 — file-boundary markers: unified
//                                  `--- ` / `+++ ` file lines
//                                  (`:54, 63`), context-diff
//                                  `*** ` file line (`:75`), p4
//                                  `====` (`:65`), difflib `? `
//                                  (`:77`).
//   - POSITION (4)               — hunk / position markers:
//                                  unified `@@ ... @@` (`:79`),
//                                  normal-diff numeric line
//                                  ranges (`:81`), context-diff
//                                  position variants (`:50-52,
//                                  61, 71-73`).
//   - DELETED (5)                — unified `-` / normal-diff `<`
//                                  removed content (`:90-91`);
//                                  also context-diff `---xxx`
//                                  fall-through at `:56`.
//   - ADDED (6)                  — unified `+` / normal-diff `>`
//                                  added content (`:92-93`).
//   - CHANGED (7)                — context-diff `!` changed
//                                  content (`:94-95`).
//   - PATCH_ADD (8)              — combined-diff `++` (both
//                                  parents added, `:82-83`).
//   - PATCH_DELETE (9)           — combined-diff `+-` (`:84-85`).
//   - REMOVED_PATCH_ADD (10)     — combined-diff `-+` (`:86-87`).
//   - REMOVED_PATCH_DELETE (11)  — combined-diff `--` (`:88-89`).
//
// **No wordlists.** `emptyWordListDesc[]` at `LexDiff.cxx:149-151`
// and the `LexerModule` registration at `:155` — LexDiff is a
// pure line-shape classifier, so `LangTheme.keywords` is empty
// for this row (no `SCI_SETKEYWORDS` calls issue).
//
// **Case handling.** The leading-character discrimination at
// `:43-89` mixes `strncmp` prefix compares (`diff `, `Index: `,
// `--- `, `+++ `, `====`, `***`, `? `, `++`, `+-`, `-+`, `--`)
// with direct byte comparisons (`lineBuffer[0] == '@'` at
// `:78`, digit-range check at `:80`, `'-' | '<' | '+' | '>' |
// '!'` at `:90-95`). Both are byte-exact — no `tolower` /
// `strncasecmp` in the chain — so no case-folding applies.
// Diff output never carries alternative case in these markers,
// so the ADDED/DELETED/HEADER discrimination is pure
// leading-character shape.
//
// Values match `SciLexer.h:596-607`. LexDiff registers
// SCLEX_DIFF at `LexDiff.cxx:155`.
pub const SCE_DIFF_DEFAULT: usize = 0;
pub const SCE_DIFF_COMMENT: usize = 1;
pub const SCE_DIFF_COMMAND: usize = 2;
pub const SCE_DIFF_HEADER: usize = 3;
pub const SCE_DIFF_POSITION: usize = 4;
pub const SCE_DIFF_DELETED: usize = 5;
pub const SCE_DIFF_ADDED: usize = 6;
pub const SCE_DIFF_CHANGED: usize = 7;
pub const SCE_DIFF_PATCH_ADD: usize = 8;
pub const SCE_DIFF_PATCH_DELETE: usize = 9;
pub const SCE_DIFF_REMOVED_PATCH_ADD: usize = 10;
pub const SCE_DIFF_REMOVED_PATCH_DELETE: usize = 11;

// LexPS style indices. 16 contiguous slots (0..=15) covering
// Adobe PostScript's stack-based token grammar as
// implemented by `ColourisePSDoc` at `LexPS.cxx:67-270`.
//
// Style semantics (paint-loop citations reference LexPS.cxx):
//   - DEFAULT (0)             — whitespace / uninteresting
//                               fall-through. **Note:** the
//                               lexer uses `SCE_C_DEFAULT`
//                               (also 0) as its neutral state
//                               throughout the state machine
//                               (`:101, :109, :111, :120, :162,
//                               :166, :169, :197, :224`); the
//                               two constants are numerically
//                               identical so no confusion.
//   - COMMENT (1)             — `%...` line comments. Line
//                               entry at `:229-239` (via the
//                               `%` branch when the next char
//                               isn't `%` at line-start),
//                               terminated at `:99-102` on EOL.
//                               DSC-comment fallback at `:113`
//                               downgrades to COMMENT when a
//                               `%%...`-line-start prefix is
//                               followed by whitespace without
//                               the trailing `:`.
//   - DSC_COMMENT (2)         — Document Structuring
//                               Convention directive line
//                               (`%%directive`). Entry at
//                               `:230-232` when `%%` starts
//                               a line, terminated at `:103-114`
//                               on `:` (which promotes to
//                               `DSC_VALUE`) or EOL.
//   - DSC_VALUE (3)           — Value after `%%directive:`
//                               (e.g. `%%Title: My Document`).
//                               Entry at `:107` (after eating
//                               the colon) or `:233-236` (for
//                               the `%%+` continuation
//                               shorthand), terminated at
//                               `:99-102` on EOL.
//   - NUMBER (4)              — Numeric literals: integers,
//                               reals with exponents, and
//                               radix numbers (`16#FF`,
//                               `2#1010`). Entry at `:240-259`,
//                               with sign / decimal / exponent
//                               state pinned by the flag
//                               triplet `numHasPoint` /
//                               `numHasExponent` / `numHasSign`
//                               (`:89-92`, `:243-246, :250-253,
//                               :256-259`) and radix state via
//                               `numRadix` (`:122-130`).
//                               Terminated at `:116-151` on
//                               self-delimiting / whitespace,
//                               or demoted to `NAME` at
//                               `:119, :123, :129, :133,
//                               :141, :147, :150` when the
//                               token turns out not to parse
//                               as a number.
//   - NAME (5)                — Bare identifier / operator not
//                               matched by any wordlist. Entry
//                               at `:261` (any non-whitespace
//                               non-delimiter char in DEFAULT
//                               state), terminated at
//                               `:152-163`. On termination the
//                               lexer runs the
//                               `keywords[1..5].InList(s)`
//                               chain at `:156-159` — a match
//                               promotes to `KEYWORD` via
//                               `ChangeState` at `:160`;
//                               otherwise the token stays
//                               `NAME`.
//   - KEYWORD (6)             — Wordlist-matched operator.
//                               Set only via the `ChangeState`
//                               promotion at `:160`; never
//                               entered directly.
//   - LITERAL (7)             — `/name` literal-name literal
//                               (pushes the name onto the
//                               stack as a symbol without
//                               executing it). Entry at
//                               `:208` (single `/`), terminated
//                               at `:164-166` on self-
//                               delimiting / whitespace.
//   - IMMEVAL (8)             — `//name`
//                               immediately-evaluated name
//                               (Level-2 feature — evaluates
//                               the name at scan time rather
//                               than execution time). Entry
//                               at `:205-206` (`//`),
//                               terminated at `:164-166`.
//   - PAREN_ARRAY (9)         — Array delimiter `[` / `]`.
//                               Single-char state entered at
//                               `:199-200`, released
//                               immediately at `:167-169`.
//   - PAREN_DICT (10)         — Dictionary delimiter `<<` /
//                               `>>` (Level-2). Entry at
//                               `:210-213, :220-222`,
//                               released at `:167-169`.
//   - PAREN_PROC (11)         — Procedure body delimiter `{`
//                               / `}`. Entry at `:201-202`,
//                               released at `:167-169`. The
//                               folder at `:272-325`
//                               syntax-folds on this style
//                               (`:292` checks
//                               `style == SCE_PS_PAREN_PROC`).
//   - TEXT (12)               — `(...)` string literal with
//                               nested parens and `\`-escape.
//                               Entry at `:226-228`,
//                               terminated at `:170-178` via
//                               the `nestTextCurrent` depth
//                               counter (line state carries
//                               depth across lines via
//                               `SetLineState` at `:265-266`).
//   - HEXSTRING (13)          — `<...>` hex-encoded string.
//                               Entry at `:218` (`<` alone),
//                               terminated at `:179-185`.
//                               A non-hex non-whitespace char
//                               inside triggers an inline
//                               `BADSTRINGCHAR` mark at
//                               `:184` via `styler.ColourTo`.
//   - BASE85STRING (14)       — `<~...~>` base-85 encoded
//                               string (Level-2). Entry at
//                               `:214-217` (`<~`), terminated
//                               at `:186-193` on the closing
//                               `~>`. Bad-char inline mark at
//                               `:192`.
//   - BADSTRINGCHAR (15)      — Error marker for a non-hex /
//                               non-base85 char inside its
//                               respective string state, or
//                               for a stray `>` / `)` at
//                               DEFAULT state at `:223-225`.
//                               Not entered via `SetState` —
//                               applied inline via
//                               `styler.ColourTo` at `:184,
//                               :192, :225`.
//
// **Wordlist classes.** `psWordListDesc[]` at `LexPS.cxx:327-334`
// declares five classes:
//   - class 0 "PS Level 1 operators"     → gated by `ps.level >= 1`
//   - class 1 "PS Level 2 operators"     → gated by `ps.level >= 2`
//   - class 2 "PS Level 3 operators"     → gated by `ps.level >= 3`
//   - class 3 "RIP-specific operators"   → always active
//   - class 4 "User-defined operators"   → always active
// Default `ps.level = 3` (`:84`) enables all three level
// tiers; a lower value disables the higher classes without
// disturbing the always-active RIP + user classes.
//
// **Case handling.** `LexPS` calls `sc.GetCurrent(s, sizeof(s))`
// at `:155`, NOT `GetCurrentLowered` — wordlist matching is
// **case-sensitive**. PostScript is a case-sensitive language;
// `add` / `Add` / `ADD` are distinct names, and canonical
// mixed-case identifiers like `FontDirectory`,
// `StandardEncoding`, `ISOLatin1Encoding`, `HalftoneType` are
// part of the standard operator set.
//
// **DEFAULT-vs-SCE_C_DEFAULT.** The classifier uses
// `SCE_C_DEFAULT` (also value 0, from `SciLexer.h`) as its
// neutral state throughout — a Scintilla-family convention
// where any lexer may fall back on the shared "no style"
// value. Byte-equivalent to `SCE_PS_DEFAULT`.
//
// Values match `SciLexer.h:843-858`. LexPS registers
// SCLEX_PS at `LexPS.cxx:336`.
pub const SCE_PS_DEFAULT: usize = 0;
pub const SCE_PS_COMMENT: usize = 1;
pub const SCE_PS_DSC_COMMENT: usize = 2;
pub const SCE_PS_DSC_VALUE: usize = 3;
pub const SCE_PS_NUMBER: usize = 4;
pub const SCE_PS_NAME: usize = 5;
pub const SCE_PS_KEYWORD: usize = 6;
pub const SCE_PS_LITERAL: usize = 7;
pub const SCE_PS_IMMEVAL: usize = 8;
pub const SCE_PS_PAREN_ARRAY: usize = 9;
pub const SCE_PS_PAREN_DICT: usize = 10;
pub const SCE_PS_PAREN_PROC: usize = 11;
pub const SCE_PS_TEXT: usize = 12;
pub const SCE_PS_HEXSTRING: usize = 13;
pub const SCE_PS_BASE85STRING: usize = 14;
pub const SCE_PS_BADSTRINGCHAR: usize = 15;

// LexRuby style indices. 32 assigned emission slots spanning
// indices 0..=31 and 40..=44 (indices 32..=39 are reserved as
// an IDENTIFIER sub-style range per `SubStyles subStyles`
// declaration at `LexRuby.cxx:211`; `styleSubable[]` at
// `:156` lists only `SCE_RB_IDENTIFIER` as sub-styleable).
// Plus one pseudo-style constant (`SCE_RB_UPPER_BOUND` = 45,
// used as `SCE_RB_IDENTIFIER_PREFERRE` via `#define` at
// `:333` — "prefer regex after identifier" hint that never
// reaches the host as an emitted style).
//
// Style semantics (paint-loop citations reference LexRuby.cxx):
//   - DEFAULT (0)            — whitespace / neutral state.
//   - ERROR (1)              — malformed / unterminated
//                              token. Distinct visual so the
//                              user sees a bad `%<c>...`
//                              string mid-buffer.
//   - COMMENTLINE (2)        — `#`-prefixed line comments.
//   - POD (3)                — `=begin` / `=end` block
//                              comment (Ruby's POD-ish
//                              multi-line comment format).
//   - NUMBER (4)             — numeric literals: integer,
//                              float, rational (`_r`),
//                              complex (`_i`), hex (`0x`),
//                              oct (`0o` / `0`), bin (`0b`),
//                              digit-separators (`1_000`).
//   - WORD (5)               — reserved keywords in their
//                              primary role (leading a
//                              statement / expression).
//                              Emitted via `ChangeState` in
//                              `ClassifyWordRb` at
//                              `:373-374` after the
//                              `keywords.InList(s)` check
//                              at `:358`.
//   - STRING (6)             — `"..."` double-quoted
//                              interpolable string.
//   - CHARACTER (7)          — `'...'` single-quoted
//                              non-interpolable string.
//                              Lexer name is legacy — Ruby
//                              has no C-style char literal.
//   - CLASSNAME (8)          — Identifier following `class`
//                              (the class being defined).
//                              Emitted at `:340-341` via
//                              `prevWord == "class"`.
//   - DEFNAME (9)            — Identifier following `def`
//                              (method being defined).
//                              Emitted at `:344-345`.
//   - OPERATOR (10)          — Punctuation (`+`, `->`, `=>`,
//                              `**`, `<=>`, `&.`, `::`, …).
//   - IDENTIFIER (11)        — Bare identifier that didn't
//                              match the keyword wordlist
//                              and isn't sigil-prefixed.
//                              The one sub-style-able
//                              archetype (per `:156`
//                              `styleSubable[]`).
//   - REGEX (12)             — `/regex/[opts]` literal.
//   - GLOBAL (13)            — `$foo`, `$0`..`$9`, `$_`, and
//                              Ruby's other `$`-prefixed
//                              special globals (`$~`, `$&`,
//                              `$'`, `` $` `` etc.).
//   - SYMBOL (14)            — `:foo` symbol literal, and
//                              trailing-`:` hash-key
//                              shorthand (`foo:`) emitted at
//                              `:1411-1417`.
//   - MODULE_NAME (15)       — Identifier following `module`
//                              (the module being defined).
//                              Emitted at `:342-343`.
//   - INSTANCE_VAR (16)      — `@foo` instance variable.
//   - CLASS_VAR (17)         — `@@foo` class variable.
//   - BACKTICKS (18)         — `` `cmd` `` command
//                              substitution.
//   - DATASECTION (19)       — Everything after a bare
//                              `__END__` marker at
//                              line-start. Entry at
//                              `:1426-1431`.
//   - HERE_DELIM (20)        — `<<HEREDOC` or `<<~HEREDOC`
//                              delimiter word itself.
//   - HERE_Q (21)            — Heredoc body when the
//                              delimiter is single-quoted
//                              (`<<'FOO'` — non-interp).
//   - HERE_QQ (22)           — Heredoc body when the
//                              delimiter is bare or
//                              double-quoted (interp).
//   - HERE_QX (23)           — Heredoc body when the
//                              delimiter is backtick-quoted
//                              (command interp).
//   - STRING_Q (24)          — `%q(...)` — single-quoted
//                              generic-brace string.
//   - STRING_QQ (25)         — `%Q(...)` — double-quoted
//                              generic-brace string.
//   - STRING_QX (26)         — `%x(...)` — command-substituted
//                              generic-brace string.
//   - STRING_QR (27)         — `%r(...)` — regex.
//   - STRING_QW (28)         — `%W(...)` — interpolable
//                              string array. (LexRuby's
//                              lexical-class label is
//                              "qw = array"; matches Perl's
//                              historical `qw` naming.)
//   - WORD_DEMOTED (29)      — Keyword used as trailing
//                              modifier: `stmt if cond`,
//                              `stmt while cond`. Emitted
//                              at `:371` when
//                              `keywordIsAmbiguous(s)` (list
//                              at `:1793-1797`:
//                              `if / do / while / unless /
//                              until / for`) AND
//                              `keywordIsModifier`.
//   - STDIN (30)             — Bare `STDIN` constant.
//   - STDOUT (31)            — Bare `STDOUT` constant.
//   - (32..=39)              — Sub-style range for
//                              `SCE_RB_IDENTIFIER` (host
//                              can allocate up to 8
//                              user-classified identifier
//                              buckets via
//                              `SCI_ALLOCATESUBSTYLES`).
//                              Not statically assigned.
//   - STDERR (40)            — Bare `STDERR` constant.
//   - STRING_W (41)          — `%w(...)` — non-interpolable
//                              string array.
//   - STRING_I (42)          — `%i(...)` — non-interpolable
//                              symbol array.
//   - STRING_QI (43)         — `%I(...)` — interpolable
//                              symbol array.
//   - STRING_QS (44)         — `%s(...)` — bare symbol
//                              generic-brace syntax.
//                              Lexical-class label is
//                              "identifier symbol".
//   - UPPER_BOUND (45)       — Not a real style. Used
//                              internally as
//                              `SCE_RB_IDENTIFIER_PREFERRE`
//                              (`:333` `#define`) — a
//                              "prefer regex after this
//                              identifier" hint that is
//                              intercepted at `:1442` and
//                              never reaches the host.
//                              Declared here for API
//                              stability parity with
//                              `SciLexer.h:462`.
//
// **Wordlist classes.** `rubyWordListDesc[]` at
// `LexRuby.cxx:142-145` declares ONE class: "Keywords"
// (class 0). All identifier promotion to `SCE_RB_WORD` /
// `SCE_RB_WORD_DEMOTED` runs through this single wordlist
// via `keywords.InList(s)` at `:358`. Sigil-prefixed vars
// (`$` / `@` / `@@` / `:`) and definition-context names
// (post-`class` / `module` / `def`) bypass the wordlist —
// they're state-machine-driven.
//
// **Case handling.** `ClassifyWordRb` at `:335-337` calls
// `styler.GetRange(start, end)` — no `GetCurrentLowered`
// wrapper — so wordlist matching is **case-sensitive**.
// Ruby is a case-sensitive language; `BEGIN` / `END`
// (uppercase, top-level blocks) and `__FILE__` / `__LINE__`
// / `__ENCODING__` (double-underscore magic constants) are
// canonical uppercase / mixed-case entries.
//
// **`?` and `!` in identifiers.** LexRuby's `:1418-1425`
// special path admits trailing `?` / `!` on identifiers
// (`empty?`, `nil?`, `strip!`) — the classifier extends the
// segment to include them. So `defined?` in the wordlist
// matches the tokenised `defined?` segment.
//
// Values match `SciLexer.h:425-462`. LexRuby registers
// SCLEX_RUBY at `LexRuby.cxx:2191`.
pub const SCE_RB_DEFAULT: usize = 0;
pub const SCE_RB_ERROR: usize = 1;
pub const SCE_RB_COMMENTLINE: usize = 2;
pub const SCE_RB_POD: usize = 3;
pub const SCE_RB_NUMBER: usize = 4;
pub const SCE_RB_WORD: usize = 5;
pub const SCE_RB_STRING: usize = 6;
pub const SCE_RB_CHARACTER: usize = 7;
pub const SCE_RB_CLASSNAME: usize = 8;
pub const SCE_RB_DEFNAME: usize = 9;
pub const SCE_RB_OPERATOR: usize = 10;
pub const SCE_RB_IDENTIFIER: usize = 11;
pub const SCE_RB_REGEX: usize = 12;
pub const SCE_RB_GLOBAL: usize = 13;
pub const SCE_RB_SYMBOL: usize = 14;
pub const SCE_RB_MODULE_NAME: usize = 15;
pub const SCE_RB_INSTANCE_VAR: usize = 16;
pub const SCE_RB_CLASS_VAR: usize = 17;
pub const SCE_RB_BACKTICKS: usize = 18;
pub const SCE_RB_DATASECTION: usize = 19;
pub const SCE_RB_HERE_DELIM: usize = 20;
pub const SCE_RB_HERE_Q: usize = 21;
pub const SCE_RB_HERE_QQ: usize = 22;
pub const SCE_RB_HERE_QX: usize = 23;
pub const SCE_RB_STRING_Q: usize = 24;
pub const SCE_RB_STRING_QQ: usize = 25;
pub const SCE_RB_STRING_QX: usize = 26;
pub const SCE_RB_STRING_QR: usize = 27;
pub const SCE_RB_STRING_QW: usize = 28;
pub const SCE_RB_WORD_DEMOTED: usize = 29;
pub const SCE_RB_STDIN: usize = 30;
pub const SCE_RB_STDOUT: usize = 31;
pub const SCE_RB_STDERR: usize = 40;
pub const SCE_RB_STRING_W: usize = 41;
pub const SCE_RB_STRING_I: usize = 42;
pub const SCE_RB_STRING_QI: usize = 43;
pub const SCE_RB_STRING_QS: usize = 44;
pub const SCE_RB_UPPER_BOUND: usize = 45;

// LexSmalltalk style indices. 17 contiguous slots (0..=16) —
// a compact lexer (330 lines total) for a syntactically-tiny
// language where "everything is a message send." The
// classifier at `LexSmalltalk.cxx:272-322` runs a
// character-class dispatch (`isSpecial` / `isBinSel` /
// `isDecDigit` / `isLetter` at `:82-86`, driven by the
// auto-generated `ClassificationTable[256]` at `:71-80`) and
// hands off to typed handlers (`handleHash` for `#symbol`,
// `handleSpecial` for `()[]{};.^:` punctuation,
// `handleNumeric` for radix numerics, `handleLetter` for
// identifier + keyword-send + hardcoded-word disambiguation,
// `handleBinSel` for binary selectors).
//
// Style semantics (paint-loop citations reference LexSmalltalk.cxx):
//   - DEFAULT (0)      — whitespace and unclassified local
//                        variables (temp names between `|`
//                        bars) — anything the classifier
//                        leaves unpromoted.
//   - STRING (1)       — `'...'` string literal. `''` is the
//                        escape for a single quote per
//                        `skipString` at `:109-119`.
//   - NUMBER (2)       — Numeric literal. Supports radix
//                        (`16r1F` = decimal 31), decimal
//                        fractions, scaled decimal (`3s2`),
//                        and scientific exponent (`e` / `d` /
//                        `q`). Full grammar at `:166-214`.
//   - COMMENT (3)      — `"..."` block comment (Smalltalk
//                        uses double-quote for comments,
//                        single-quote for strings — the
//                        opposite of every other C-family
//                        convention). No nesting; `skipComment`
//                        at `:103-107`.
//   - SYMBOL (4)       — `#foo` symbol literal or `#'quoted'`
//                        string-form symbol. Also emitted for
//                        keyword-part symbols like
//                        `#at:put:`. Entry at `:301-302`,
//                        classification at `handleHash`
//                        `:121-144`.
//   - BINARY (5)       — Binary-selector message name
//                        composed from
//                        `~@%&*-+=|\/,<>?!` chars (the
//                        `isBinSel` set at `:86`, entered
//                        by `handleBinSel` at `:216-221`).
//                        Note `-` followed by a digit is
//                        promoted to NUMBER instead
//                        (`:313-315`).
//   - BOOL (6)         — `true` / `false`. Hardcoded at
//                        `:263-264`.
//   - SELF (7)         — `self`. Hardcoded at `:257-258`.
//   - SUPER (8)        — `super`. Hardcoded at `:259-260`.
//   - NIL (9)          — `nil`. Hardcoded at `:261-262`.
//   - GLOBAL (10)      — Identifier whose first char is
//                        UpperCase per `isUpper` at `:85`.
//                        Smalltalk convention: class names
//                        and global variables are
//                        `PascalCase` (`Object`, `Array`,
//                        `Smalltalk`); local variables and
//                        method names are lower-case
//                        (`aString`, `aCollection`). Emitted
//                        at `:254-255`.
//   - RETURN (11)      — `^` return operator. Handled
//                        specially by `handleSpecial` at
//                        `:152-157` (any `^` NOT part of
//                        `:=` becomes RETURN; the actual
//                        `^` handler is at `:153-154`).
//   - SPECIAL (12)     — Punctuation from the "special"
//                        char set `()[]{};.^:` at `:44` —
//                        entered by `handleSpecial` at
//                        `:146-158` when NOT a `:=` prefix
//                        and NOT a bare `^`.
//   - KWSEND (13)      — Keyword-send message part. An
//                        identifier ending in a single `:`
//                        (`at:`, `put:`, `do:`, `ifTrue:`
//                        when NOT in the special-selector
//                        wordlist). Classification at
//                        `:252-253`.
//   - ASSIGN (14)      — `:=` assignment operator.
//                        Handled at `:148-151`; the classifier
//                        eats the following `=` at `:150`.
//   - CHARACTER (15)   — `$c` character literal (dollar sign
//                        followed by exactly one character).
//                        Entry at `:303-306`.
//   - SPEC_SEL (16)    — Wordlist-matched control-flow /
//                        boolean-combinator / nil-test
//                        selector (`ifTrue:`, `whileTrue:`,
//                        `isNil`, `and:`, etc.). Promoted
//                        from KWSEND/DEFAULT at `:250-251`
//                        when the ident matches
//                        `wordLists[0]`.
//
// **Wordlist classes.** `smalltalkWordListDesc[]` at
// `LexSmalltalk.cxx:325-328` declares ONE class: "Special
// selectors" (class 0). The lexer ships NO default entries
// — the wordlist is entirely user-populated. SciTE's
// bundled `SciTE.properties` at
// `vendor/lexilla/test/examples/smalltalk/SciTE.properties`
// documents an 11-selector default (`ifTrue: ifFalse:
// whileTrue: whileFalse: ifNil: ifNotNil: whileTrue
// whileFalse repeat isNil notNil`) — Code++ extends this
// with the 4 boolean combinators (`and:` / `or:` / `xor:`
// / `not`).
//
// **Case handling.** The classifier uses byte-exact
// `strcmp` at `:257-266` for the 5 hardcoded reserved words
// and `wordLists[0]->InList` at `:250` for the wordlist —
// both **case-sensitive**. `Self` / `SELF` / `sELF` are
// distinct from the hardcoded `self` and would render as
// `SCE_ST_GLOBAL` / `SCE_ST_DEFAULT` respectively. Wordlist
// entries also match byte-exact.
//
// **Hardcoded language keywords.** LexSmalltalk hardcodes
// its five language constants (`self` / `super` / `nil` /
// `true` / `false`) directly in the `handleLetter`
// classifier at `:257-266` rather than through the
// wordlist. This is deliberate — those constants have
// dedicated styles (`SCE_ST_SELF` / `SUPER` / `NIL` /
// `BOOL`) so the theme can paint them distinctly from
// wordlist-matched selectors. **Do NOT add them to the
// `SMALLTALK_SPECIAL_SELECTORS` wordlist.** The
// `handleLetter` dispatch order at `:250-266` is
// `InList` (first) → `doubleColonPresent` → `isUpper`
// → hardcoded strcmp chain (last, as a fallback for bare
// lowercase idents). Adding these five constants to the
// wordlist would make `InList` fire FIRST and silently
// promote them to `SCE_ST_SPEC_SEL`, OVERRIDING the
// dedicated `SELF` / `SUPER` / `NIL` / `BOOL` styles
// that give them distinct visual identity — the
// opposite of the intended behaviour. The exclusion is
// enforced not because the wordlist path is unreachable,
// but because it would win a dispatch precedence it
// shouldn't win.
//
// Values match `SciLexer.h:1247-1263`. LexSmalltalk registers
// SCLEX_SMALLTALK at `LexSmalltalk.cxx:330`.
pub const SCE_ST_DEFAULT: usize = 0;
pub const SCE_ST_STRING: usize = 1;
pub const SCE_ST_NUMBER: usize = 2;
pub const SCE_ST_COMMENT: usize = 3;
pub const SCE_ST_SYMBOL: usize = 4;
pub const SCE_ST_BINARY: usize = 5;
pub const SCE_ST_BOOL: usize = 6;
pub const SCE_ST_SELF: usize = 7;
pub const SCE_ST_SUPER: usize = 8;
pub const SCE_ST_NIL: usize = 9;
pub const SCE_ST_GLOBAL: usize = 10;
pub const SCE_ST_RETURN: usize = 11;
pub const SCE_ST_SPECIAL: usize = 12;
pub const SCE_ST_KWSEND: usize = 13;
pub const SCE_ST_ASSIGN: usize = 14;
pub const SCE_ST_CHARACTER: usize = 15;
pub const SCE_ST_SPEC_SEL: usize = 16;

// LexVHDL style indices. 16 contiguous slots (0..=15) covering
// IEEE-1076 VHDL as classified by `ColouriseVHDLDoc` at
// `LexVHDL.cxx:60-178`. Seven wordlist classes drive a single
// identifier-recognition state (`SCE_VHDL_IDENTIFIER`) that
// promotes to one of seven distinct styles at classifier exit —
// unlike the C-family lexers, VHDL demands typographic
// discrimination across keyword / word-operator / attribute /
// standard-function / standard-package / standard-type / user-word
// axes because a well-formed VHDL entity references all seven in
// close succession (`entity` / `and` / `'range` / `to_integer` /
// `ieee.numeric_std.all` / `std_logic` / user-signal-name).
//
// Style semantics (paint-loop citations reference LexVHDL.cxx):
//   - DEFAULT (0)          — whitespace and unclassified
//                            fall-through. Entry at `:83-84`,
//                            `:86-87`, `:107-108`, `:116-117`,
//                            `:125-126`, `:130`, `:136`.
//   - COMMENT (1)          — `--...` line comment (VHDL's only
//                            block-comment-free heritage
//                            comment style until VHDL-2008
//                            introduced `/* ... */`). Entry at
//                            `:150`, terminated on `atLineEnd`
//                            at `:115-118`.
//   - COMMENTLINEBANG (2)  — `--!...` line comment. A Doxygen /
//                            documentation-comment convention
//                            adopted from Verilog. Entry at
//                            `:147-148`, terminated on
//                            `atLineEnd` at `:115-118`.
//   - NUMBER (3)           — Numeric literal. Entered at
//                            `:142-143` on a digit or `.digit`
//                            (VHDL literals include decimal
//                            integers, real literals with `E`
//                            exponent, and based-integer form
//                            `2#1010#` / `16#FF#`). Terminated at
//                            `:85-88` when the next char is
//                            neither a wordchar nor `#`.
//   - STRING (4)           — `"..."` string literal. Entry at
//                            `:153-154`; `""` is the doubled-quote
//                            escape per `:119-124`. Also entered
//                            from the char-literal path at
//                            `:155-165` when a single-quoted
//                            three-tick sequence is unambiguously
//                            a character literal (identifier'('x')
//                            disambiguation).
//   - OPERATOR (5)         — Punctuation-class operator. Entered
//                            at `:169-170` when `isoperator(ch)`
//                            matches (Scintilla-shared classifier
//                            covering `+-*/=<>!@%^&|~`, brackets,
//                            comma, semicolon). Terminated
//                            immediately at `:83-84`.
//   - IDENTIFIER (6)       — Intermediate state for a
//                            word-start-to-word-end scan; NEVER
//                            the final emitted style. At scan
//                            exit `:90-114`, `GetCurrentLowered`
//                            case-folds the identifier and the
//                            wordlist chain rewrites the style to
//                            one of KEYWORD / STDOPERATOR /
//                            ATTRIBUTE / STDFUNCTION / STDPACKAGE
//                            / STDTYPE / USERWORD (via
//                            `sc.ChangeState` at `:94-107`) —
//                            or, if no wordlist matches, IDENTIFIER
//                            remains the emitted style at `:108`.
//                            Also the state for extended
//                            identifiers (`\name\`) entered at
//                            `:166-168`, terminated on backslash
//                            or line end at `:109-113`.
//   - STRINGEOL (7)        — Unterminated `"..."` at end of
//                            line. Promoted from STRING at
//                            `:127-131`.
//   - KEYWORD (8)          — Reserved word from
//                            `keywordlists[0]`. Promoted from
//                            IDENTIFIER at `:93-94`.
//   - STDOPERATOR (9)      — Word-form operator (`and`, `or`,
//                            `not`, `xor`, `nand`, `nor`, `xnor`,
//                            `abs`, `mod`, `rem`, `sll`, `srl`,
//                            `sla`, `sra`, `rol`, `ror`) from
//                            `keywordlists[1]`. Promoted from
//                            IDENTIFIER at `:95-96`. Distinct
//                            from OPERATOR (5), which paints the
//                            punctuation-class operators.
//   - ATTRIBUTE (10)       — Predefined attribute (`'range`,
//                            `'length`, `'high`, `'low`, `'left`,
//                            `'right`, `'event`, `'stable`, etc.
//                            — the tick-prefix form is the VHDL
//                            attribute-access syntax) from
//                            `keywordlists[2]`. Promoted from
//                            IDENTIFIER at `:97-98`. Note the
//                            lexer stores attributes without
//                            the leading tick — in the common
//                            multi-char attribute-access case
//                            (`T'range`, `T'event`), the tick's
//                            `else if (sc.ch == '\'')` branch at
//                            `:155-165` calls no `SetState`, so
//                            the tick stays emitted as
//                            `SCE_VHDL_DEFAULT`. The `else if`
//                            chain never falls through to
//                            `isoperator` at `:169-170` (that
//                            branch is a sibling, and
//                            `isoperator` doesn't include `'`
//                            in `CharacterSet.h:165-176`
//                            anyway). `SCE_VHDL_DEFAULT` is
//                            deliberately left unmapped in
//                            `VHDL_STYLES`, so the tick paints
//                            with the default text colour.
//   - STDFUNCTION (11)     — Standard-library function
//                            (`to_integer`, `rising_edge`,
//                            `resize`, etc.) from
//                            `keywordlists[3]`. Promoted from
//                            IDENTIFIER at `:99-100`.
//   - STDPACKAGE (12)      — Standard-library package
//                            (`ieee`, `std_logic_1164`,
//                            `numeric_std`, etc.) from
//                            `keywordlists[4]`. Promoted from
//                            IDENTIFIER at `:101-102`.
//   - STDTYPE (13)         — Standard-library type
//                            (`std_logic`, `std_logic_vector`,
//                            `boolean`, `integer`, etc.) from
//                            `keywordlists[5]`. Promoted from
//                            IDENTIFIER at `:103-104`.
//   - USERWORD (14)        — Project-specific user words from
//                            `keywordlists[6]`. Promoted from
//                            IDENTIFIER at `:105-106`. Code++
//                            ships this class empty — it's the
//                            per-project extension slot (a user
//                            can populate it via a future
//                            per-project override once the
//                            settings surface exists).
//   - BLOCK_COMMENT (15)   — `/* ... */` block comment
//                            (VHDL-2008 addition). Entry at
//                            `:151-152`, terminated on `*/` at
//                            `:132-138`.
//
// **Wordlist classes.** `VHDLWordLists[]` at
// `LexVHDL.cxx:552-561` declares seven classes in this exact
// order: 0=Keywords, 1=Operators, 2=Attributes,
// 3=Standard Functions, 4=Standard Packages, 5=Standard Types,
// 6=User Words. The SCE_VHDL_* style IDs are
// version-agnostic — the same 16 styles cover VHDL-87 through
// VHDL-2008; what differs across revisions is only the
// *contents* of the wordlists (VHDL-2008 adds reserved words
// like `context`, `assume`, `sequence`, etc. that Code++
// currently omits — see `VHDL_KEYWORDS` rationale in
// `codepp_core::lang`). The STD* classes track IEEE-1076
// package annexes.
//
// **Case handling.** VHDL is a **case-insensitive** language.
// The classifier's `GetCurrentLowered` at `:92` case-folds the
// scanned identifier to lowercase before every `InList` probe.
// Wordlist entries must be lowercase — an uppercase entry would
// never match. This is the same convention as LexPS but the
// **opposite** of LexRuby / LexSmalltalk (case-sensitive) and
// LexCPP (case-sensitive with hardcoded folding suppression).
//
// Values match `SciLexer.h:1119-1134`. LexVHDL registers
// SCLEX_VHDL (= 64) at `LexVHDL.cxx:564`.
pub const SCE_VHDL_DEFAULT: usize = 0;
pub const SCE_VHDL_COMMENT: usize = 1;
pub const SCE_VHDL_COMMENTLINEBANG: usize = 2;
pub const SCE_VHDL_NUMBER: usize = 3;
pub const SCE_VHDL_STRING: usize = 4;
pub const SCE_VHDL_OPERATOR: usize = 5;
pub const SCE_VHDL_IDENTIFIER: usize = 6;
pub const SCE_VHDL_STRINGEOL: usize = 7;
pub const SCE_VHDL_KEYWORD: usize = 8;
pub const SCE_VHDL_STDOPERATOR: usize = 9;
pub const SCE_VHDL_ATTRIBUTE: usize = 10;
pub const SCE_VHDL_STDFUNCTION: usize = 11;
pub const SCE_VHDL_STDPACKAGE: usize = 12;
pub const SCE_VHDL_STDTYPE: usize = 13;
pub const SCE_VHDL_USERWORD: usize = 14;
pub const SCE_VHDL_BLOCK_COMMENT: usize = 15;

// LexKix style indices. **Non-contiguous — 11 emission slots
// spanning 0..=10 plus IDENTIFIER at 31**, a 20-index gap that
// reserves 11..=30 for future style additions (Notepad++ convention
// for niche lexers where the author left headroom rather than
// committing to a numeric layout).
//
// LexKix is a compact 130-line lexer (`LexKix.cxx`, contributed by
// Manfred Becker in 2004, extended by Lee Wilmott in 2014 to add
// block-comment support) for KIXtart — a Windows login-script
// language mid-abandoned by its author in 2018 but still in
// production at legacy Windows shops. The language mixes sigil-
// prefixed variables (`$var`) and macros (`@date`, `@time`) with
// C-family strings and dual comment styles (`;` line + `/* */`
// block).
//
// Style semantics (paint-loop citations reference LexKix.cxx):
//   - DEFAULT (0)          — whitespace and any classifier
//                            fall-through. Entry at `:75, :79, :89,
//                            :93, :105, :57, :61, :66, :71`.
//   - COMMENT (1)          — `;...` line comment. Entry at `:112`,
//                            terminated on `atLineEnd` at `:56-57`.
//   - STRING1 (2)          — `"..."` double-quoted string. Entry at
//                            `:115-116`, terminated on the matching
//                            `"` at `:65-66`. **No escape sequences**
//                            — the classifier stops the string at
//                            the first bare `"`. KIXtart doesn't
//                            support C-style backslash escapes in
//                            strings; embedded double-quotes are
//                            impossible in this string form.
//   - STRING2 (3)          — `'...'` single-quoted string. Entry at
//                            `:117-118`, terminated on the matching
//                            `'` at `:70-71`. Same no-escape rule
//                            as STRING1.
//   - NUMBER (4)           — Numeric literal. Entry at `:123-124`
//                            when the char is `IsADigit` OR when
//                            `.digit` / `&digit` prefix appears
//                            (the `&`-prefix is KIXtart's hex-number
//                            marker, per Notepad++ 8.x convention).
//                            Terminated at `:73-75` when the next
//                            char is not a digit.
//   - VAR (5)              — `$var` variable reference. Entry at
//                            `:119-120` on the `$` char; scans
//                            word-chars via `IsAWordChar` at :34
//                            (accented / high-bit chars included by
//                            design — see `IsAWordChar` at `:33-35`),
//                            terminated on non-word-char at `:77-79`.
//                            **The `$` char itself is styled as part
//                            of the VAR run** (the sigil isn't
//                            emitted as OPERATOR — the classifier
//                            enters VAR state before the sigil is
//                            "consumed").
//   - MACRO (6)            — `@macroname` macro reference. Entry at
//                            `:121-122` on the `@` char; on scan
//                            exit at `:81-89`, the identifier
//                            AFTER the `@` (`&s[1]` at `:86`) is
//                            probed against `keywords3` (class 2 —
//                            the "known macros" wordlist). If NOT
//                            in the list, the state DOWNGRADES to
//                            DEFAULT at `:87-88` (so unknown
//                            `@foo` bare tokens paint as default,
//                            not as a false-positive macro). If IN
//                            the list, MACRO stays. **Class 2 is a
//                            positive whitelist**, not a
//                            style-override for typos.
//   - KEYWORD (7)          — Reserved KIXtart command (`if`,
//                            `else`, `while`, `for`, etc.) matched
//                            from `keywords` (class 0) at
//                            `:100-101`. Promoted from IDENTIFIER
//                            at scan exit.
//   - FUNCTIONS (8)        — Built-in KIXtart function (`getobject`,
//                            `readvalue`, `messagebox`, etc.)
//                            matched from `keywords2` (class 1) at
//                            `:102-103`. Promoted from IDENTIFIER
//                            at scan exit. Distinct from KEYWORD
//                            because KIXtart authors read commands
//                            and functions as visually distinct
//                            categories (commands are
//                            statement-only; functions can appear
//                            inside expressions).
//   - OPERATOR (9)         — Punctuation-class operator. Entered at
//                            `:125-126` when `IsOperator(ch)` at
//                            `:37-39` matches. **Note the restricted
//                            operator set**: `+ - * / & | < > =` —
//                            only nine characters. `!`, `~`, `%`,
//                            `^`, `?` are NOT included. Terminated
//                            at `:91-93`.
//   - COMMENTSTREAM (10)   — `/* ... */` block comment.
//                            Contributed by Lee Wilmott's 2014
//                            patch (per the file header). Entry at
//                            `:113-114`, terminated on `*/` at
//                            `:59-62`. **Newline-safe** — spans
//                            multiple lines (no `atLineEnd`
//                            terminator, unlike COMMENT).
//   - IDENTIFIER (31)      — Bare identifier that fails BOTH the
//                            `keywords` and `keywords2` probes at
//                            scan exit `:96-105`. Intermediate scan
//                            state for identifier tokens; only
//                            emitted at paint time when the token
//                            is neither a KEYWORD nor a FUNCTION
//                            (i.e., a user-defined variable name
//                            without the `$` sigil — which is
//                            legal in KIXtart function calls and
//                            UDF definitions).
//
// **Wordlist classes.** LexKix's classifier at `:44-46` names three
// active classes: `keywords` (class 0), `keywords2` (class 1),
// `keywords3` (class 2). A fourth (`keywords4`, class 3) is
// **commented out** at `:47` — declared for future use, never
// probed. The lexer registers NO `WordListDescriptions[]` array
// (unlike VHDL / PostScript / Ruby), meaning the class names above
// aren't self-documented in the classifier — they're inferred from
// the `SCI_SETKEYWORDS(class, ...)` numeric indices at classifier
// entry. Code++ installs three classes; class 3 stays unset.
//
// **Case handling.** The classifier calls `GetCurrentLowered` at
// `:84` (macro path) and `:98` (identifier path) — KIXtart is
// **case-insensitive**. Wordlist entries MUST be lowercase; same
// convention as VHDL / PostScript.
//
// **Sigil handling.** The two sigil-prefixed forms (`$var`,
// `@macro`) are entered by the classifier's `if (sc.ch == '$')` /
// `if (sc.ch == '@')` branches at `:119-122`. The sigil is included
// in the emitted style run (a `$foo` token paints as one continuous
// SCE_KIX_VAR span, not `$` + `foo`). This matches Ruby's
// `SCE_RB_INSTANCE_VAR` / `SCE_RB_CLASS_VAR` (which include the
// sigil) and Perl / Bash `SCE_*_SCALAR` (which also include it) —
// consistent with the family convention.
//
// **The macro whitelist gate.** Unlike KEYWORD / FUNCTIONS (which
// promote IDENTIFIER → styled-token on wordlist hit), MACRO
// DOWNGRADES to DEFAULT on wordlist miss. So a well-known macro
// like `@date` paints as SCE_KIX_MACRO; a typo like `@daat` paints
// as SCE_KIX_DEFAULT (not SCE_KIX_MACRO). This is a deliberate
// visual signal — KIXtart macros are a fixed vocabulary (no user
// extension), so an unrecognised `@name` is almost certainly a
// typo. The classifier catches it at style time.
//
// Values match `SciLexer.h:1027-1038`. LexKix registers SCLEX_KIX
// (= 57) at `LexKix.cxx:136`.
pub const SCE_KIX_DEFAULT: usize = 0;
pub const SCE_KIX_COMMENT: usize = 1;
pub const SCE_KIX_STRING1: usize = 2;
pub const SCE_KIX_STRING2: usize = 3;
pub const SCE_KIX_NUMBER: usize = 4;
pub const SCE_KIX_VAR: usize = 5;
pub const SCE_KIX_MACRO: usize = 6;
pub const SCE_KIX_KEYWORD: usize = 7;
pub const SCE_KIX_FUNCTIONS: usize = 8;
pub const SCE_KIX_OPERATOR: usize = 9;
pub const SCE_KIX_COMMENTSTREAM: usize = 10;
pub const SCE_KIX_IDENTIFIER: usize = 31;

// LexAU3 style indices. 16 contiguous slots (0..=15) covering the
// AutoIt3 Windows automation / scripting language as classified
// by `ColouriseAU3Doc` at `LexAU3.cxx:199-608` (with the 900+-line
// lexer's rich state machine covering variables, macros,
// preprocessor directives, embedded SendKeys tokens inside string
// literals, and the AutoIt3 Standard UDF library).
//
// LexAU3 is the WIDEST wordlist-class lexer we've wired — 8
// classes at `LexAU3.cxx:900-909` (keywords / functions / macros /
// SendKeys / preprocessors / special / expand / UDF). Each drives
// a distinct SCE promotion path from the intermediate
// `SCE_AU3_KEYWORD` scan state at `:314-370` (except SEND, which
// is promoted from the STRING-embedded `SCE_AU3_SENT` state at
// `:464-541` — SendKeys are AutoIt's inline
// `Send("{ENTER}")`-style key names, so the classifier
// recognises them INSIDE a string literal).
//
// Style semantics (paint-loop citations reference LexAU3.cxx):
//   - DEFAULT (0)         — whitespace and unclassified
//                           fall-through. Entry at every
//                           state-exit site (`:262, :304, :328,
//                           :332, :336, :340, :356, :360, :363-364,
//                           :415, :426, :454, :526`).
//   - COMMENT (1)         — `;...` line comment. Entry at `:548`,
//                           terminated on `atLineEnd` at
//                           `:293-295`.
//   - COMMENTBLOCK (2)    — `#cs ... #ce` (or `#comments-start /
//                           #comments-end`) block comment. Entry
//                           at `:322-323` when the scanned
//                           `#`-prefixed identifier is `#cs` or
//                           `#comments-start`; exited at `:262`
//                           when the closing `#ce` / `#comments-end`
//                           is seen. State-machine at `:255-291`
//                           tracks `ci` (0=start-of-line,
//                           1=first-char-seen, 2=skip-rest).
//   - NUMBER (3)          — Numeric literal. Entry at `:561-565`
//                           with `ni` flag tracking the numeric
//                           form (0=integer, 1=has-dot,
//                           2=hex-prefixed, 3=E-notation,
//                           9=malformed). Terminated at
//                           `:409-416`. Hex prefix `0x` or `0X`
//                           at `:377-380`; scientific `e`/`E` at
//                           `:383-386`.
//   - FUNCTION (4)        — Built-in AutoIt3 function. Promoted
//                           from KEYWORD scan state at
//                           `:330-333` on `keywords2.InList(s)`
//                           hit. This is the largest built-in
//                           function surface in Windows scripting
//                           (~1200 built-ins in AutoIt3 core).
//   - KEYWORD (5)         — Reserved word (control flow / decl /
//                           `and` / `or` / `not`). Promoted from
//                           KEYWORD scan state at `:326-329` on
//                           `keywords.InList(s)` hit — the FIRST
//                           wordlist probed. Also the
//                           intermediate scan-in-progress state:
//                           on scan exit at `:314-370` the
//                           classifier probes 8 wordlist classes
//                           in sequence and rewrites the state
//                           via `ChangeState` to KEYWORD /
//                           FUNCTION / MACRO / PREPROCESSOR /
//                           SPECIAL / EXPAND / UDF (or falls
//                           through to OPERATOR at `:359` for
//                           the bare `_` line-continuation, or
//                           DEFAULT at `:363-364` when no
//                           wordlist matches).
//   - MACRO (6)           — `@`-prefixed macro (`@ScriptDir`,
//                           `@Error`, `@CR`, etc.). Entry into
//                           SCE_AU3_KEYWORD scan state on `@`
//                           at `:552`; promoted to MACRO at
//                           `:334-337` on `keywords3.InList(s)`
//                           hit. Wordlist entries include the
//                           leading `@` (differs from KIXtart
//                           where the `@` is stripped before
//                           InList) because the classifier
//                           enters the scan on `@` and includes
//                           it in the identifier run.
//   - STRING (7)          — Double- or single-quoted string
//                           literal. Entry at `:555-560` on `"`
//                           (with `si=1`) or `'` (with `si=2`).
//                           Also entered via `:554` on `<` when
//                           the preceding `#include` set `si=3`
//                           (angle-bracket include-path form).
//                           Terminated on the matching quote at
//                           `:441-445` or line end (with
//                           continuation-line handling) at
//                           `:447-457`.
//   - OPERATOR (8)        — Punctuation-class operator. Entered
//                           at `:551` on `.` (when not
//                           followed by a digit — a `.` before
//                           a digit is a number's decimal
//                           point) OR at `:567` on `IsAOperator`
//                           match (the operator set at
//                           `:90-97` is `+ - * / & ^ = < > ( )
//                           [ ] ,`). Bare `_` at `:358-360` also
//                           promotes to OPERATOR (it's the
//                           line-continuation operator).
//   - VARIABLE (9)        — `$var` variable reference. Entry at
//                           `:550` on `$`; scanned via
//                           `IsAWordChar` (extended to accept
//                           non-ASCII at `:83-86`), terminated on
//                           non-word at `:425-427`. On `.` at
//                           `:422-424` promotes to OPERATOR to
//                           handle the COM-object member-access
//                           chain (`$obj.Method`).
//   - SENT (10)           — SendKeys token inside a string
//                           literal — the AutoIt classifier's
//                           unique feature. `Send("{ENTER}")`
//                           lexes the string as
//                           STRING—SENT—STRING, so `{ENTER}`
//                           paints distinctly from the
//                           surrounding literal. Entry inside
//                           `SCE_AU3_STRING` at `:458-461` on
//                           `{`/`+`/`!`/`^`/`#`; validated by
//                           `keywords4.InList(sk)` at `:483-486`
//                           where `sk` is the brace-wrapped
//                           token produced by `GetSendKey` at
//                           `:106-169`. Wordlist entries include
//                           the braces (e.g., `{ENTER}`,
//                           `{TAB}`, `{F1}`) — see wordlist
//                           class 3 rationale.
//   - PREPROCESSOR (11)   — `#`-prefixed compiler directive
//                           (`#include`, `#Region`, `#EndRegion`,
//                           `#NoTrayIcon`, etc.). Entry into
//                           SCE_AU3_KEYWORD scan state on `#`
//                           at `:549`; promoted to PREPROCESSOR
//                           at `:338-345` on
//                           `keywords5.InList(s)` hit. Special
//                           handling: if the matched directive
//                           is `#include`, sets `si=3` so the
//                           next `<...>` string is styled as
//                           STRING (the include-path form).
//   - SPECIAL (12)        — Rare AutoIt3-specific control tokens
//                           reserved for the SPECIAL wordlist
//                           class. Very small surface — most
//                           installations leave this class
//                           empty. Entry at `:346-348` on
//                           `keywords6.InList(s)` hit; distinctly
//                           uses `sc.SetState(SCE_AU3_SPECIAL)`
//                           (not `SetState(DEFAULT)`) so
//                           subsequent state has to explicitly
//                           re-enter DEFAULT — see the SPECIAL
//                           case at `:308-313`.
//   - EXPAND (13)         — AutoIt3 `_` line-continuation and
//                           related expand keywords. Entry at
//                           `:350-353` on `keywords7.InList(s)`
//                           AND the next char is NOT an operator
//                           (so bare `_` at EOL matches EXPAND,
//                           but `_+5` on a line matches only if
//                           `_` isn't the wordlist).
//   - COMOBJ (14)         — COM-object member-access token —
//                           the identifier AFTER a `.` on a
//                           variable / expression. Entry at
//                           `:299-302` from OPERATOR state when
//                           `sc.chPrev == '.'` and next char is
//                           a word char (`$obj.MyMethod` →
//                           `$obj` VARIABLE, `.` OPERATOR,
//                           `MyMethod` COMOBJ). Terminated on
//                           non-word at `:431-434`.
//   - UDF (15)            — AutoIt3 Standard UDF Library
//                           function. Promoted from KEYWORD scan
//                           state at `:354-357` on
//                           `keywords8.InList(s)` hit. Distinct
//                           style so authors can visually
//                           differentiate first-party built-ins
//                           (FUNCTION) from the UDF-library
//                           helpers (`_ArrayDisplay`,
//                           `_GUICtrlListView_Create`, etc.
//                           — conventionally underscore-prefixed).
//                           Added in April 2006 per the
//                           `LexAU3.cxx:44` change log.
//
// **Wordlist classes.** `AU3WordLists[]` at `LexAU3.cxx:900-909`
// declares 8 named classes:
//   0 = "#autoit keywords"        (KEYWORD  — control flow / decl)
//   1 = "#autoit functions"       (FUNCTION — built-in surface)
//   2 = "#autoit macros"          (MACRO    — `@`-prefixed macros)
//   3 = "#autoit Sent keys"       (SENT     — `{KEYNAME}` tokens in strings)
//   4 = "#autoit Pre-processors"  (PREPROCESSOR — `#`-prefixed directives)
//   5 = "#autoit Special"         (SPECIAL  — rare control tokens)
//   6 = "#autoit Expand"          (EXPAND   — `_` line-continuation)
//   7 = "#autoit UDF"             (UDF      — AutoIt3 Std UDF Library)
//
// **Dispatch precedence at scan exit** (`LexAU3.cxx:314-370`):
// The classifier probes classes in this exact order at scan exit
// (WITH one exception): `#cs`/`#comments-start` COMMENTBLOCK
// literal check FIRST (:320-324), then classes 0 → 1 → 2 → 4 →
// 5 → 6 → 7. **Class 3 (SendKeys) is NEVER probed from the KEYWORD
// scan state** — it's only reached from the SCE_AU3_SENT state
// entered INSIDE a string. Note the OUT-OF-ORDER numbering:
// class 4 (PREPROCESSOR) is probed BEFORE class 5, 6, 7. So
// duplicating a token across two classes always resolves in
// probe-order priority.
//
// **Case handling.** AutoIt3 is case-insensitive. The classifier
// case-folds via `tolower` at `:247` before every wordlist probe.
// Wordlist entries MUST be lowercase — same convention as VHDL /
// KIXtart / PostScript.
//
// **Sigil handling.** Two sigil-prefixed forms:
//   - `$var` → `SCE_AU3_VARIABLE` — the `$` sigil is INCLUDED
//     in the emitted style run (entered at `:550`, span
//     terminates on non-word-char at `:425`). Consistent with
//     KIXtart, Ruby, Perl, Bash convention.
//   - `@macro` → SCE_AU3_KEYWORD scan → promoted to MACRO —
//     the `@` sigil is INCLUDED in the identifier that reaches
//     `keywords3.InList(s)`, so wordlist entries MUST include
//     the leading `@`. This is the OPPOSITE of KIXtart's
//     LexKix, which strips the sigil via `&s[1]` before
//     probing.
//
// Values match `SciLexer.h:1065-1080`. LexAU3 registers SCLEX_AU3
// (= 60) at `LexAU3.cxx:911`.
pub const SCE_AU3_DEFAULT: usize = 0;
pub const SCE_AU3_COMMENT: usize = 1;
pub const SCE_AU3_COMMENTBLOCK: usize = 2;
pub const SCE_AU3_NUMBER: usize = 3;
pub const SCE_AU3_FUNCTION: usize = 4;
pub const SCE_AU3_KEYWORD: usize = 5;
pub const SCE_AU3_MACRO: usize = 6;
pub const SCE_AU3_STRING: usize = 7;
pub const SCE_AU3_OPERATOR: usize = 8;
pub const SCE_AU3_VARIABLE: usize = 9;
pub const SCE_AU3_SENT: usize = 10;
pub const SCE_AU3_PREPROCESSOR: usize = 11;
pub const SCE_AU3_SPECIAL: usize = 12;
pub const SCE_AU3_EXPAND: usize = 13;
pub const SCE_AU3_COMOBJ: usize = 14;
pub const SCE_AU3_UDF: usize = 15;

// LexCaml style indices. 16 contiguous slots (0..=15) covering
// Objective Caml (OCaml) — AND Standard ML '97, which the same
// lexer supports via runtime mode-switching. Contributed by
// Robert Roessler (2005-2009).
//
// **Dual-mode behavior.** LexCaml is unique among the wired
// lexers: the SAME classifier runs in Caml mode OR Standard ML
// mode, gated by a **wordlist sentinel** at `LexCaml.cxx:71` —
// `const bool isSML = keywords.InList("andalso")`. If the
// keywords wordlist contains the literal token `andalso`, every
// mode-dependent branch in the classifier switches to SML rules
// (numeric literal syntax, char literal `#"..."` form, tag
// suppression, extra identifier chars `\`/`\``). Code++ ships
// Caml mode (no `andalso` in `CAML_KEYWORDS`); SML mode is
// deliberately unwired — a future dedicated `L_SML` LangType
// would install its own wordlist with `andalso` included.
//
// Style semantics (paint-loop citations reference LexCaml.cxx):
//   - DEFAULT (0)       — whitespace / unclassified fall-through.
//                         Entry at every state-exit site (`:78,
//                         :148, :155, :169, :190, :222, :235,
//                         :257, :292`).
//   - IDENTIFIER (1)    — Intermediate scan state for a
//                         bare identifier. Entered at `:93-94`
//                         when the char is `iscamlf` (alpha or
//                         `_`). At scan exit `:132-148`, the
//                         token is looked up against 3 wordlist
//                         classes AND the special `_` singleton
//                         → KEYWORD promotion, then falls back
//                         to DEFAULT (leaving IDENTIFIER as
//                         paint style only when no wordlist
//                         matches — the "user identifier" case).
//   - TAGNAME (2)       — `\`Tag` polymorphic-variant tag (Caml
//                         mode only). Entry at `:95-96` on
//                         backtick followed by identifier-start;
//                         scan exits at `:154-155`. Suppressed
//                         in SML mode.
//   - KEYWORD (3)       — Primary Caml reserved word from
//                         `keywords` (class 0). Promoted from
//                         IDENTIFIER at `:141-142`. Also
//                         hardcoded promotion of `_` singleton
//                         at `:141` AND `()` / `[]` empty-tuple
//                         / empty-list tokens at `:183-186`
//                         from the OPERATOR state.
//   - KEYWORD2 (4)      — Optional Pervasives-family functions
//                         from `keywords2` (class 1) — `Stdlib`
//                         since 4.07. Promoted at `:143-144`.
//   - KEYWORD3 (5)      — Optional type-name family from
//                         `keywords3` (class 2). Promoted at
//                         `:145-146`.
//   - LINENUM (6)       — `#123` line-number directive (Caml
//                         mode only — used by `ocamlrun` for
//                         mapping compiled locations back to
//                         source). Entry at `:97-98`; scan exit
//                         on non-digit at `:168-169`.
//                         Suppressed in SML mode.
//   - OPERATOR (7)      — Punctuation-class operator. Two entry
//                         paths: `:122-127` on the sprawling
//                         Caml operator + bracket + punctuation
//                         set (`! ? ~ = < > @ ^ | & + - * / $ %`
//                         plus `( ) [ ] { } ; , : . #`), and
//                         SML additionally accepts `\` / `\``
//                         as "extra identifier chars"
//                         (`:125-127`). Multi-char operators
//                         handled by the OPERATOR-state
//                         continuation at `:172-193`.
//   - NUMBER (8)        — Numeric literal. Entered at `:99-113`
//                         on a digit — base 10 by default,
//                         optionally base 2/8/16 via
//                         `0b`/`0o`/`0x` prefix (Caml) or `0x`
//                         only + `0w` word-prefix (SML). Complex
//                         continuation at `:195-223` handles
//                         underscores, integer suffixes `l`/`L`/
//                         `n`, decimal point, exponent notation.
//   - CHAR (9)          — Character literal. Two forms: Caml
//                         `'c'` at `:114-115` (with backslash
//                         escape handling at `:225-243`); SML
//                         `#"c"` at `:116-117` (falls through
//                         to STRING handling at `:245-247`
//                         via deliberate fallthrough).
//   - WHITE (10)        — SML embedded-whitespace escape inside
//                         string literals — the `\   \` form
//                         where whitespace between two backslashes
//                         is invisible. Entered from
//                         STRING/CHAR at `:250-251`; exited at
//                         `:263-277` by backtracking through the
//                         style buffer to find the pre-white
//                         state. Caml mode never enters this
//                         state.
//   - STRING (11)       — `"..."` string literal. Entry at
//                         `:118-119`; scan exit on unescaped
//                         `"` at `:255-260`. SML mode
//                         additionally terminates at line end
//                         (`:256`), Caml doesn't.
//   - COMMENT (12)      — `(* ... *)` block comment, level 0
//                         (outermost). Entry at `:120-121`.
//   - COMMENT1 (13)     — Nested comment, level 1. Comments in
//                         Caml nest arbitrarily; the state
//                         increments to encode nesting depth
//                         (`sc.state + 1` at `:285`) — a nested
//                         `(*` inside COMMENT enters COMMENT1,
//                         another nest enters COMMENT2, and one
//                         more COMMENT3. Depths beyond 3 are
//                         tracked in the `nesting` counter but
//                         reuse the COMMENT3 style. Closing `*)`
//                         at `:288-293` decrements.
//   - COMMENT2 (14)     — Nested comment, level 2.
//   - COMMENT3 (15)     — Nested comment, level 3+.
//
// **Wordlist classes.** `camlWordListDesc[]` at
// `LexCaml.cxx:322-327` declares three classes: 0 = Keywords
// (primary Caml reserved words), 1 = Keywords2 (Pervasives-family
// functions), 2 = Keywords3 (type names).
//
// **Case handling.** LexCaml is **case-sensitive**. The classifier
// scans byte-exact identifiers into `t[]` at `:136-139` with no
// case-folding, and every `InList(t)` probe is byte-exact.
// Wordlist entries must match the source's exact case. This is
// the OPPOSITE of VHDL / KIXtart / AutoIt3 (all case-insensitive
// with mandatory-lowercase wordlists) and matches Ruby /
// Smalltalk / Rust convention.
//
// **The `_` singleton keyword.** `LexCaml.cxx:141` special-cases
// the single-char underscore — `if ((n == 1 && sc.chPrev == '_') || keywords.InList(t))` —
// so `_` paints as KEYWORD even without appearing in the wordlist.
// Consistent with OCaml semantics (`_` is the wildcard pattern).
//
// **`()` / `[]` are KEYWORDS, not OPERATORS.** The classifier at
// `:183-186` intercepts empty-tuple `()` and empty-list `[]`
// tokens from the OPERATOR state and promotes them to KEYWORD.
// These are literal values in OCaml (the unit value and the
// empty list), not operators — the promotion reflects that.
//
// **Magic comments (`(*@rc ... *)`).** LexCaml supports an
// optional "read-only comment" style via the
// `lexer.caml.magic` property (`:72`, `:294-297`). When set,
// comments beginning with `@rc` after `(*` are marked with the
// `0x10` state bit — a style range beyond 15. Code++ doesn't
// enable this property; the magic-comment feature stays dormant.
//
// Values match `SciLexer.h:1135-1150`. LexCaml registers
// SCLEX_CAML (= 65) at `LexCaml.cxx:329`.
pub const SCE_CAML_DEFAULT: usize = 0;
pub const SCE_CAML_IDENTIFIER: usize = 1;
pub const SCE_CAML_TAGNAME: usize = 2;
pub const SCE_CAML_KEYWORD: usize = 3;
pub const SCE_CAML_KEYWORD2: usize = 4;
pub const SCE_CAML_KEYWORD3: usize = 5;
pub const SCE_CAML_LINENUM: usize = 6;
pub const SCE_CAML_OPERATOR: usize = 7;
pub const SCE_CAML_NUMBER: usize = 8;
pub const SCE_CAML_CHAR: usize = 9;
pub const SCE_CAML_WHITE: usize = 10;
pub const SCE_CAML_STRING: usize = 11;
pub const SCE_CAML_COMMENT: usize = 12;
pub const SCE_CAML_COMMENT1: usize = 13;
pub const SCE_CAML_COMMENT2: usize = 14;
pub const SCE_CAML_COMMENT3: usize = 15;

// LexAda style indices. 12 contiguous slots (0..=11) covering the
// Ada 95 lexer (which also handles Ada 2005/2012 syntax cleanly
// since none of those revisions changed comment/string/numeric
// syntax — only the reserved-word set grew). Contributed by
// Sergey Koshcheyev (2002); dispatches SCLEX_ADA (= 20) via a
// **single wordlist** — `WordList "Keywords"` at
// `vendor/lexilla/lexers/LexAda.cxx:42-45` (`adaWordListDesc[]`).
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 633-644 and `vendor/lexilla/include/LexicalStyles.iface`
// lines 695-707.
//
// **Case-insensitive lexer.** Ada language semantics: identifier
// case does not distinguish tokens (`Package_Body` and
// `PACKAGE_BODY` refer to the same declaration). LexAda enforces
// this at the classifier level, `LexAda.cxx:194-217`
// (`ColouriseWord`): every character of the candidate identifier
// is folded to lowercase with `word += tolower(sc.ch)` BEFORE the
// `keywords.InList(word.c_str())` lookup at `:208`. Consequence:
// wordlist entries MUST be lowercase (an entry like `Begin` would
// be dead code — the lookup key is already `begin` by the time
// InList runs). Code++'s `ADA_KEYWORDS` in
// `crates/core/src/lang.rs` respects this: every token is the
// canonical Ada Reference Manual reserved word in lowercase form.
//
// **Apostrophe disambiguation.** Ada's `'` is overloaded: it
// terminates a character literal (`'a'`) AND opens an attribute
// selector (`X'Range`, `Integer'First`). LexAda tracks this with
// a per-line `apostropheStartsAttribute` bool at
// `LexAda.cxx:234-243` (per-line state stored via
// `styler.SetLineState`). After a keyword hit it clears the flag
// UNLESS the keyword is exactly `all` (which is followed by
// attribute-like syntax in dereference — `Ptr.all'Address` —
// `LexAda.cxx:211-213`). This is transparent to the host — we
// don't need to duplicate the logic, but the wordlist must
// contain `all` for the disambiguation to fire correctly. If
// `all` is missing from `ADA_KEYWORDS`, every apostrophe after
// `all` would be parsed as an attribute open, breaking character
// literals in nearby code. Code++'s wordlist includes `all`.
//
// Style semantics (paint-loop citations reference LexAda.cxx):
//
//   - SCE_ADA_DEFAULT (0) — whitespace and inter-token slack;
//     `ColouriseWhiteSpace :188-192`. Reset target at every
//     line end (`:246`), so mid-line styling never persists
//     across lines.
//   - SCE_ADA_WORD (1) — reserved word from the Keywords
//     wordlist; promoted from IDENTIFIER after case-folded
//     InList hit at `:208-209`. Bold in typical themes.
//   - SCE_ADA_IDENTIFIER (2) — non-reserved word; the initial
//     state for any word run at `:196` before InList resolves.
//     If InList misses, the state stays IDENTIFIER; if the
//     candidate fails `IsValidIdentifier` (`:205-206`), it
//     downgrades to ILLEGAL instead.
//   - SCE_ADA_NUMBER (3) — decimal literal (`42`, `3.14`),
//     scientific notation (`1.0e-3`), based literals
//     (`16#FF#`, `2#1010#`) with the `#`-delimited base
//     syntax handled at `ColouriseNumber :147-178`. The
//     numeric paint state is entered at the SetState call
//     inside that function and validated by `IsValidNumber`
//     before returning to DEFAULT.
//   - SCE_ADA_DELIMITER (4) — single-char operators/punctuation
//     from `IsDelimiterCharacter` at `:286-309`. Includes
//     `&`, `'`, `(`, `)`, `*`, `+`, `,`, `-`, `.`, `/`, `:`,
//     `;`, `<`, `=`, `>`, `|` — everything Ada tokenises as
//     punctuation. Multi-char operators like `:=`, `=>`,
//     `..`, `**`, `<<`, `>>`, `<=`, `>=`, `/=` are painted
//     as consecutive DELIMITER runs (the lexer doesn't fuse
//     them into a single token — visually they still render
//     as one span since they share the DELIMITER style).
//   - SCE_ADA_CHARACTER (5) — well-formed character literal
//     `'x'`; painted from `ColouriseCharacter :73-84`. Entered
//     only when `apostropheStartsAttribute == false` (see above).
//   - SCE_ADA_CHARACTEREOL (6) — unterminated character
//     literal (EOL reached before closing `'`); call site at
//     `LexAda.cxx:83` (`ColouriseContext(sc, '\\'',
//     SCE_ADA_CHARACTEREOL)`) with the classifier body at
//     `ColouriseContext :86-96`. A visible-error state for
//     the "you forgot the closing apostrophe" case.
//   - SCE_ADA_STRING (7) — well-formed double-quoted string
//     literal, `ColouriseString :179-187`.
//   - SCE_ADA_STRINGEOL (8) — unterminated string literal,
//     mirror of CHARACTEREOL; also a visible-error state.
//   - SCE_ADA_LABEL (9) — `<< label_name >>` block label
//     target for `goto`; `ColouriseLabel :114-146`. Distinct
//     paint lets themes emphasise labels since they're
//     unusual in modern Ada style.
//   - SCE_ADA_COMMENTLINE (10) — `--` line comment,
//     `ColouriseComment :98-106`. Ada has no block comments;
//     line comments are the sole comment form.
//   - SCE_ADA_ILLEGAL (11) — malformed identifier or bad
//     numeric literal; used for tokens that failed
//     `IsValidIdentifier` / `IsValidNumber`. A themable
//     visible-error state — good practice to paint it in
//     high-contrast red so syntax errors surface in the
//     editor rather than silently rendering as identifiers.
//
// Wordlist class ordering: 0 = Keywords. There is only one
// class; adaWordListDesc[1] is the NULL sentinel at
// `LexAda.cxx:44`. Consequence: no unused `SCI_SETKEYWORDS` calls
// — Code++ installs class 0 only.
pub const SCE_ADA_DEFAULT: usize = 0;
pub const SCE_ADA_WORD: usize = 1;
pub const SCE_ADA_IDENTIFIER: usize = 2;
pub const SCE_ADA_NUMBER: usize = 3;
pub const SCE_ADA_DELIMITER: usize = 4;
pub const SCE_ADA_CHARACTER: usize = 5;
pub const SCE_ADA_CHARACTEREOL: usize = 6;
pub const SCE_ADA_STRING: usize = 7;
pub const SCE_ADA_STRINGEOL: usize = 8;
pub const SCE_ADA_LABEL: usize = 9;
pub const SCE_ADA_COMMENTLINE: usize = 10;
pub const SCE_ADA_ILLEGAL: usize = 11;

// LexVerilog style indices. Contiguous slots 0..=12 for the
// classic surface, then a **numeric gap** followed by 19..=24
// for the SystemVerilog / port-styling / documentation-word
// extension states. Contributed by Avi Yegudin (based on
// Neil Hodgson's LexCPP frame) and extended by Ted Fried
// with the SystemVerilog states.
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 1008-1026 and
// `vendor/lexilla/include/LexicalStyles.iface`. Dispatches
// SCLEX_VERILOG (= 56) via a **six-class wordlist descriptor**
// at `vendor/lexilla/lexers/LexVerilog.cxx:1076-1084`:
//
//     verilogWordLists[] = {
//         "Primary keywords and identifiers",   // class 0 → SCE_V_WORD
//         "Secondary keywords and identifiers", // class 1 → SCE_V_WORD2
//         "System Tasks",                        // class 2 → SCE_V_WORD3
//         "User defined tasks and identifiers", // class 3 → SCE_V_USER
//         "Documentation comment keywords",     // class 4 → SCE_V_COMMENT_WORD
//         "Preprocessor definitions",           // class 5 → `ppDefinitions`
//     };
//
// **Case-sensitive lexer.** Verilog / SystemVerilog language
// semantics: identifier case DOES distinguish tokens (`module`
// and `Module` are not the same declaration). LexVerilog matches
// wordlist entries byte-exactly at `LexVerilog.cxx:552-559` —
// no `tolower` fold applied — so wordlist entries MUST be the
// canonical lowercase reserved-word form (all IEEE 1364 / 1800
// reserved words are lowercase; user identifiers use whatever
// case the source declares them with, which is why the lexer
// treats them as identifiers rather than keywords).
//
// **Class 5 is NOT a highlighting class.** `ppDefinitions` at
// `LexVerilog.cxx:317` populates the lexer's internal
// `preprocessorDefinitionsStart` table for
// `` `define ``-style macro expansion during lexing — it does
// not drive an SCE_V_* style. Code++ installs classes 0, 1, 2
// only (WORD / WORD2 / WORD3). Classes 3 (USER) and 4
// (COMMENT_WORD) have their styles mapped defensively in the
// theme so a future project-level override that populates them
// takes effect without a theme-side follow-up.
//
// **Wordlist dispatch precedence** at `LexVerilog.cxx:552-561`:
// class 0 (WORD) → class 1 (WORD2) → class 2 (WORD3) → class 3
// (USER). Additionally, if class 4 fires (`keywords5.InList(s)`
// at `:508` inside the SCE_V_COMMENT_WORD scan state), the
// match paints `SCE_V_COMMENT_WORD` — that only fires while
// scanning INSIDE a block comment, so it's not a dispatch-order
// concern for identifiers outside comments.
//
// **Port-styling states (SCE_V_INPUT / OUTPUT / INOUT /
// PORT_CONNECT) are gated by an option** —
// `lexer.verilog.portstyling` at `LexVerilog.cxx:168`, default
// `false` (`:146`). When off, module port directions render as
// `SCE_V_WORD` (matched via the class 0 wordlist entry) and
// `.name` port bindings render as `SCE_V_IDENTIFIER`. When
// on (host sets `SCI_SETPROPERTY "lexer.verilog.portstyling"
// "1"`), the classifier promotes `input`/`output`/`inout`
// after `(` to their dedicated states (`:533-547`) and the
// identifier after `.` inside module instantiation to
// `SCE_V_PORT_CONNECT`. Code++ maps these four styles
// defensively so a user who enables `portstyling` sees a
// coherent theme, but leaves the option OFF by default.
//
// Style semantics (paint-loop citations reference LexVerilog.cxx):
//
//   - SCE_V_DEFAULT (0) — inter-token slack. Reset at every
//     transition back to whitespace.
//   - SCE_V_COMMENT (1) — `/* ... */` block comment; scanned
//     at `case SCE_V_COMMENT` `:571-579`. Terminates on `*/`.
//     Doc-comment keywords (`\author`, `\brief`, …) inside a
//     block comment are promoted to SCE_V_COMMENT_WORD via
//     the `IsAWordStart` branch at `:575-577` when the class
//     4 wordlist matches.
//   - SCE_V_COMMENTLINE (2) — `//` line comment. Doc-comment
//     keywords in line comments are also promoted to
//     SCE_V_COMMENT_WORD via the shared case-fallthrough at
//     `:580-587`.
//   - SCE_V_COMMENTLINEBANG (3) — `//!` line comment, the
//     Verilog-idiomatic "documentation flag" comment variant.
//     Same COMMENT_WORD promotion path as SCE_V_COMMENTLINE
//     (`:580-587`). Distinct paint lets themes emphasise
//     doc-comments over plain `//` comments.
//   - SCE_V_NUMBER (4) — numeric literal. Verilog's rich
//     number syntax includes sized binary/octal/decimal/hex
//     (`4'b1010`, `8'hFF`, `16'd42`, `32'o755`), unsized
//     integers, real literals (`3.14`, `1.0e-3`), and the
//     underscore separator (`64'hDEAD_BEEF`).
//   - SCE_V_WORD (5) — class 0 wordlist match. **Primary
//     reserved words** — module structure (`module` /
//     `endmodule` / `interface`), procedural blocks (`always`
//     / `initial` / `final`), control flow (`if` / `else` /
//     `case` / `for` / `while`), and assertion/property
//     temporal operators.
//   - SCE_V_STRING (6) — `"..."` double-quoted string literal.
//     Verilog supports `\n` / `\t` / `\\` / `\"` / `\ddd`
//     (octal) / `\xHH` (hex) escapes inside strings.
//   - SCE_V_WORD2 (7) — class 1 wordlist match. **Secondary
//     reserved words** — types (`reg`, `wire`, `logic`,
//     `integer`, `real`, `bit`, `int`), net-type variants
//     (`wand`, `wor`, `tri`, `supply0`, `supply1`), gate
//     primitives (`and`, `or`, `nand`, `nor`, `buf`, `not`,
//     `nmos`, `pmos`), drive/charge-strength qualifiers,
//     and type-modifier keywords (`signed`, `unsigned`,
//     `packed`).
//   - SCE_V_WORD3 (8) — class 2 wordlist match. **System
//     tasks** — the `$`-prefixed built-in family
//     (`$display`, `$monitor`, `$time`, `$strobe`, `$random`,
//     `$readmemh`, `$fopen`, `$fclose`, …). The `$` is part
//     of the identifier at `IsAWordStart :362` so wordlist
//     entries MUST include the leading `$`.
//   - SCE_V_PREPROCESSOR (9) — `` ` ``-prefixed directive
//     (`` `include ``, `` `define ``, `` `ifdef ``,
//     `` `timescale ``, …). Entered at `:617-618` when a
//     backtick is encountered at DEFAULT. No wordlist gate;
//     the styling is driven purely by the syntactic
//     backtick-prefix.
//   - SCE_V_OPERATOR (10) — punctuation (`=`, `==`, `===`,
//     `!=`, `!==`, `+`, `-`, `*`, `/`, `%`, `&`, `|`, `^`,
//     `~`, `<<`, `>>`, `<<<`, `>>>`, `<=`, `>=`, `?`, `:`,
//     `@`, `#`, `,`, `;`, `(`, `)`, `[`, `]`, `{`, `}`,
//     `,`, `->`, `->>`, `<->`).
//   - SCE_V_IDENTIFIER (11) — non-reserved word. Initial
//     state for any word-run at `:743` / `:757`. Every
//     variable / signal / instance / module-name declaration
//     terminates here unless a wordlist hit rewrites the
//     state.
//   - SCE_V_STRINGEOL (12) — unterminated `"..."` (newline
//     inside string). Visible-error state.
//   - SCE_V_USER (19) — class 3 wordlist match. **User-defined
//     tasks / identifiers** — a customisation slot so an
//     editor / project can highlight known helper task /
//     function names distinctly from the reserved-word set.
//     Code++ ships this empty; a future per-project override
//     may populate it. Also the target of the
//     `options.allUppercaseDocKeyword` promotion at `:560-561`
//     — any AllUpperCase identifier in the regular
//     SCE_V_IDENTIFIER path (not in a comment) gets promoted
//     to USER when that option is enabled.
//   - SCE_V_COMMENT_WORD (20) — class 4 wordlist match inside
//     ANY comment (block `/* ... */`, line `//`, or
//     doc-line `//!`). LexVerilog transitions into COMMENT_WORD
//     from all three comment states via a shared `IsAWordStart`
//     branch at `:575-577` (block) and `:585-587` (line +
//     line-bang, joint case-fallthrough). The `lineState`
//     capture at `:576` / `:585` preserves the caller state so a
//     `keywords5.InList` MISS restores the correct comment
//     style at `:511`. Scanned at `SCE_V_COMMENT_WORD :503-514`.
//     Typical use: doc-comment keywords like `\author`,
//     `\brief`, `\file` for a Doxygen-style tooling workflow.
//   - SCE_V_INPUT (21) — port direction `input` after a
//     module port `(` when `portStyling == true` at `:533-534`.
//     Off by default; mapped defensively.
//   - SCE_V_OUTPUT (22) — port direction `output`, `:536-538`.
//   - SCE_V_INOUT (23) — port direction `inout`, `:539-541`.
//   - SCE_V_PORT_CONNECT (24) — the identifier after `.` in a
//     module instantiation port-bind (e.g. `.clk (sys_clk)`)
//     when `portStyling == true` at `:548-551`. Off by
//     default; mapped defensively.
//
// **Activity mask (translate_off / translate_on shading).**
// LexVerilog OR's an `activitySet` bit (0x40) into the state
// while inside a `` `translate_off `` region so a fold /
// theme system can render that region dimmed. Code++ does
// NOT map the INACTIVE range today — a future refinement
// could paint activity-masked regions with a dedicated dim
// slot; for now, translate_off regions render at
// `STYLE_DEFAULT` since the (activitySet | SCE_V_*) states
// fall outside our mapping table.
pub const SCE_V_DEFAULT: usize = 0;
pub const SCE_V_COMMENT: usize = 1;
pub const SCE_V_COMMENTLINE: usize = 2;
pub const SCE_V_COMMENTLINEBANG: usize = 3;
pub const SCE_V_NUMBER: usize = 4;
pub const SCE_V_WORD: usize = 5;
pub const SCE_V_STRING: usize = 6;
pub const SCE_V_WORD2: usize = 7;
pub const SCE_V_WORD3: usize = 8;
pub const SCE_V_PREPROCESSOR: usize = 9;
pub const SCE_V_OPERATOR: usize = 10;
pub const SCE_V_IDENTIFIER: usize = 11;
pub const SCE_V_STRINGEOL: usize = 12;
pub const SCE_V_USER: usize = 19;
pub const SCE_V_COMMENT_WORD: usize = 20;
pub const SCE_V_INPUT: usize = 21;
pub const SCE_V_OUTPUT: usize = 22;
pub const SCE_V_INOUT: usize = 23;
pub const SCE_V_PORT_CONNECT: usize = 24;

// LexMatlab style indices. 9 contiguous slots (0..=8) for the
// shared MATLAB / Octave lexer implementation. Contributed by
// José Fonseca; extended by Christoph Dalitz (2003 — Octave
// support + double-quoted strings), John Donoghue (2012-2017 —
// nested block comments, `...` continuation-as-comment,
// updated fold logic), and Andrey Smolyakov (2022 — Matlab
// R2019b+ `arguments` block + classdef `properties` /
// `methods` / `events` contextual keywords).
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 714-722. Dispatches SCLEX_MATLAB (= 32) via a
// **single wordlist** at
// `vendor/lexilla/lexers/LexMatlab.cxx:516-519`
// (`matlabWordListDesc[]`, only class 0 "Keywords" plus the
// NULL sentinel). Same file also registers SCLEX_OCTAVE (= 54)
// as `lmOctave` at `:528`; Code++ does not currently wire
// Octave separately — the Octave lexer differs primarily in
// accepting `#` as a comment-start char AND allowing `\`
// escapes inside double-quoted strings, but MATLAB source
// opened with the Matlab lexer renders correctly on its own.
//
// **Case-sensitive lexer.** MATLAB language semantics:
// identifier case DOES distinguish tokens (`End` and `end`
// are not the same). LexMatlab matches wordlist entries
// byte-exactly at `LexMatlab.cxx:251` (`keywords.InList(s)`)
// with no `tolower` fold. All MATLAB reserved words per
// MathWorks' `iskeyword` are lowercase, so every wordlist
// entry stays lowercase. A `LowerCase` helper is defined at
// `:63-67` but only used in `IsSpaceToEOL` (block-comment
// delimiter detection at end-of-line) — not in the keyword
// lookup path.
//
// **Contextual keywords (NOT in wordlist).** LexMatlab handles
// a family of contextual-keyword tokens INSIDE the classifier
// rather than via wordlist matching, so the host's keyword
// list MUST NOT include them. Each contextual token is
// documented at the wordlist definition; the summary:
//
//   - `arguments` at `:270-274`: promoted to KEYWORD only
//     after a `function` declaration line (via the
//     `expectingArgumentsBlock` flag). The lexer's
//     `:269` comment says outright "arguments is a keyword
//     here, despite not being in the keywords list".
//   - `properties` / `methods` / `events` at `:285-292`:
//     promoted to KEYWORD only inside classdef scope
//     (via the `inClassScope` flag and folding-level
//     check). Otherwise ChangeState to
//     `SCE_MATLAB_IDENTIFIER` so a user-defined variable
//     named `properties` doesn't over-highlight.
//
// Putting any of these four tokens in the wordlist would
// promote them to keyword everywhere, breaking the lexer's
// deliberate contextual behaviour. Code++'s `MATLAB_KEYWORDS`
// respects this.
//
// **Context-sensitive `end`.** Inside indexing (i.e. when
// `allow_end_op > 0` at `:143`, tracked by `(`/`[`/`{`
// counting), the token `end` is ChangeState-ed to
// `SCE_MATLAB_NUMBER` at `:255-257` (matching MATLAB's
// semantics where `x(end)` returns `x`'s last element). This
// is transparent to the host: `end` must be in the wordlist
// so InList fires, then the classifier does the contextual
// promote/demote.
//
// **Initial-state trick.** LexMatlab enters SCE_MATLAB_KEYWORD
// as the INITIAL state for any `isalpha` run at `:399-400`,
// then checks InList when the identifier ends. If InList
// misses, `sc.ChangeState(SCE_MATLAB_IDENTIFIER)` at `:289`
// demotes it. This is the reverse of most lexers (which enter
// IDENTIFIER and promote to KEYWORD on hit) — same visible
// result, different SCE-index history.
//
// Style semantics (paint-loop citations reference LexMatlab.cxx):
//
//   - SCE_MATLAB_DEFAULT (0) — inter-token slack, reset at
//     every state transition back to whitespace.
//   - SCE_MATLAB_COMMENT (1) — MATLAB has THREE comment forms
//     all painting to this style: `%` line comment,
//     `%{ ... %}` block comment (nested, depth tracked via
//     `commentDepth` at `:164` and stored in line state),
//     and `...` line-continuation which is
//     ChangeState-promoted to COMMENT at `:236` when three
//     consecutive dots are seen at the tail of an operator
//     run.
//   - SCE_MATLAB_COMMAND (2) — `!command` shell-escape at
//     line-start (only for MATLAB, not Octave — Octave paints
//     `!` as operator at `:387`). Set at `:385`.
//   - SCE_MATLAB_NUMBER (3) — numeric literal. MATLAB syntax
//     covers integer, decimal (`3.14`), scientific
//     (`1e-3`), hex (`0xFF`), complex-suffix (`1i` / `2j`),
//     and size suffix (`3u32` for integer types) — the
//     numeric-continuation predicate at `:305-311` accepts
//     all of these. Contextual `end` inside indexing also
//     lands here (`:255-257`).
//   - SCE_MATLAB_KEYWORD (4) — reserved word from the
//     wordlist. Initial state for any alphabetic run
//     (`:399-400`); promoted to IDENTIFIER on InList miss
//     (`:289`).
//   - SCE_MATLAB_STRING (5) — single-quoted string
//     (traditional MATLAB char-array literal). Contextual:
//     `'` opens a STRING literal only when NOT following a
//     transpose-eligible token (post-identifier, post-`)`,
//     post-`]`, post-`}`) — at `:389-394` the classifier
//     tests the `transpose` bool and enters
//     `SCE_MATLAB_OPERATOR` for the transpose apostrophe
//     instead of STRING.
//   - SCE_MATLAB_OPERATOR (6) — punctuation and operators
//     (`+`, `-`, `*`, `/`, `\`, `^`, `.*`, `./`, `.\`, `.^`,
//     `.'` transpose, `==`, `~=`, `<`, `>`, `<=`, `>=`, `=`,
//     `&`, `|`, `&&`, `||`, `~`, `@`, `:`, `,`, `;`, `(`,
//     `)`, `[`, `]`, `{`, `}`).
//   - SCE_MATLAB_IDENTIFIER (7) — non-reserved word. The
//     ChangeState target for wordlist misses at `:289`.
//     Framework convention: leave unmapped so ordinary
//     variable / function names paint at STYLE_DEFAULT.
//   - SCE_MATLAB_DOUBLEQUOTESTRING (8) — MATLAB R2017a+
//     double-quoted string literal (the `string` scalar
//     type). Distinct from single-quoted char-array literals
//     — the language has two string forms. Painted at
//     `:395-396`.
pub const SCE_MATLAB_DEFAULT: usize = 0;
pub const SCE_MATLAB_COMMENT: usize = 1;
pub const SCE_MATLAB_COMMAND: usize = 2;
pub const SCE_MATLAB_NUMBER: usize = 3;
pub const SCE_MATLAB_KEYWORD: usize = 4;
pub const SCE_MATLAB_STRING: usize = 5;
pub const SCE_MATLAB_OPERATOR: usize = 6;
pub const SCE_MATLAB_IDENTIFIER: usize = 7;
pub const SCE_MATLAB_DOUBLEQUOTESTRING: usize = 8;

// LexHaskell style indices. 23 contiguous slots (0..=22) for
// Haskell 2010 + common GHC extensions (MagicHash /
// TemplateHaskell / TypeFamilies / SafeHaskell / literate
// `.lhs` files). Dispatches SCLEX_HASKELL (= 68) via a
// **three-class wordlist** at
// `vendor/lexilla/lexers/LexHaskell.cxx:224-229`
// (`haskellWordListDesc[]`):
//
//     haskellWordListDesc[] = {
//         "Keywords",           // class 0 → SCE_HA_KEYWORD
//         "FFI",                // class 1 → SCE_HA_KEYWORD (only inside `foreign` decl)
//         "Reserved operators", // class 2 → SCE_HA_RESERVED_OPERATOR
//     };
//
// The same file registers a second lexer at `:1119` — SCLEX_LITERATEHASKELL
// (= 108) for `.lhs` literate-programming files, which reuses the
// same word list but treats non-`>`-prefixed lines as
// `SCE_HA_LITERATE_COMMENT`. Code++ wires SCLEX_HASKELL only;
// literate `.lhs` support could be added later with a dedicated
// L_LHASKELL langtype.
//
// **Case-sensitive lexer.** Haskell language semantics:
// identifier case DOES distinguish tokens AND carries syntactic
// meaning — a bare identifier that starts with an uppercase
// letter is a data constructor, module name, or type name; one
// that starts with lowercase is a value binding, function, or
// type variable. `LexHaskell.cxx:747` calls `keywords.InList(s)`
// byte-exactly with no `tolower` fold. All Haskell reserved
// words per the Haskell 2010 Report §2.4 are lowercase, so every
// wordlist entry stays lowercase.
//
// **Context-driven state machine.** LexHaskell tracks a
// `KeywordMode` (`HA_MODE_DEFAULT` / `HA_MODE_IMPORT1..3` /
// `HA_MODE_MODULE` / `HA_MODE_TYPE` / `HA_MODE_FFI`) alongside
// the usual scan state. Consequence: several tokens are treated
// as contextual keywords by the classifier and MUST NOT be in
// the wordlist — `qualified` (`:756-759`), `safe` (`:760-764`,
// gated by `highlightSafe` option), `as` and `hiding`
// (`:766-771`), `family` (`:772-774`). Adding any of them to
// `HASKELL_KEYWORDS` would promote them to KEYWORD at every
// site, breaking the contextual promotion the lexer performs.
// Similarly, capitalized identifiers that syntactically must be
// module names (in `import`/`module` context) or data
// constructors (in `data`/`newtype` context) are dispatched to
// SCE_HA_MODULE / SCE_HA_DATA / etc. rather than the wordlist —
// see the mode transitions at `:750-775`.
//
// **Reserved-operator class (class 2).** Class 2 fires from the
// operator-scan path at `:645-654` — an operator run is
// assembled and then `reserved_operators.InList(s)` is checked;
// a hit rewrites the state from `SCE_HA_OPERATOR` (11) to
// `SCE_HA_RESERVED_OPERATOR` (20). Reserved operators per
// Haskell 2010 §2.4 are `..` `:` `::` `=` `\` `|` `<-` `->` `@`
// `~` `=>`. This is DISTINCT from ordinary operators — Haskell
// permits user-defined operators (any run of `!#$%&*+./<=>?@\^|-~:`
// characters), and only the specific reserved set gets the
// distinct paint.
//
// Style semantics (paint-loop citations reference LexHaskell.cxx):
//
//   - SCE_HA_DEFAULT (0) — inter-token slack.
//   - SCE_HA_IDENTIFIER (1) — non-reserved lowercase-initial
//     word. Framework convention: leave unmapped so ordinary
//     value bindings / functions / type variables paint at
//     STYLE_DEFAULT.
//   - SCE_HA_KEYWORD (2) — reserved word from the class 0
//     wordlist. Promoted from IDENTIFIER after `keywords.InList`
//     hit at `:747-748`. Also the target of the contextual
//     promotions for `qualified` / `safe` / `as` / `hiding` /
//     `family` at `:756-774`.
//   - SCE_HA_NUMBER (3) — numeric literal. Haskell syntax
//     covers integer, decimal, scientific, hex (`0xFF`),
//     octal (`0o755`), and with the MagicHash extension the
//     `#`-suffixed unboxed variants (`42#`, `3.14##`).
//   - SCE_HA_STRING (4) — `"..."` string literal. Distinct from
//     the CHARACTER slot so a theme can differentiate.
//   - SCE_HA_CHARACTER (5) — `'x'` character literal.
//   - SCE_HA_CLASS (6) — type-class name inside a `class ...`
//     declaration. Emitted via the HA_MODE_CLASS state
//     transition.
//   - SCE_HA_MODULE (7) — module name in `module M where` or
//     `import [qualified] M [as ...]` context. Emitted via the
//     HA_MODE_MODULE / IMPORT1-3 states at `:750-755`.
//   - SCE_HA_CAPITAL (8) — capitalized identifier not otherwise
//     specialized. The default state for any word starting with
//     an uppercase letter (data constructor, type name, or bare
//     type application) — set at `:710`.
//   - SCE_HA_DATA (9) — the data-declaration payload emitted in
//     HA_MODE_DATA state (data constructor names inside `data
//     T = ...`).
//   - SCE_HA_IMPORT (10) — historical / deprecated state.
//     Modern LexHaskell (since 2013) routes import module names
//     to SCE_HA_MODULE instead and no longer emits this state.
//     Code++ leaves it UNMAPPED — mapping a state the lexer no
//     longer produces would add a dead entry to the theme table.
//   - SCE_HA_OPERATOR (11) — user-defined operator run.
//     Haskell permits any run of `!#$%&*+./<=>?@\^|-~:`
//     characters as an operator name.
//   - SCE_HA_INSTANCE (12) — type-class instance-head classes
//     inside `instance ... where` declarations.
//   - SCE_HA_COMMENTLINE (13) — `--` line comment.
//   - SCE_HA_COMMENTBLOCK (14) — `{- ... -}` block comment at
//     nesting depth 1.
//   - SCE_HA_COMMENTBLOCK2 (15) — `{- {- ... -} -}` at nesting
//     depth 2.
//   - SCE_HA_COMMENTBLOCK3 (16) — nesting depth ≥ 3. Nested
//     block comments per Haskell 2010 §2.3.
//   - SCE_HA_PRAGMA (17) — `{-# ... #-}` compiler pragma
//     (LANGUAGE / OPTIONS_GHC / INLINE / etc.).
//   - SCE_HA_PREPROCESSOR (18) — CPP `#`-prefixed directive
//     (only fires when CPP is being run over the source, e.g.
//     `#ifdef`).
//   - SCE_HA_STRINGEOL (19) — unterminated string (EOL inside
//     `"..."`). Visible-error state.
//   - SCE_HA_RESERVED_OPERATOR (20) — class 2 wordlist match
//     from `:651-652`. The Haskell 2010 §2.4 reserved set.
//   - SCE_HA_LITERATE_COMMENT (21) — literate-programming
//     non-code lines (in `.lhs` files under the LiterateHaskell
//     lexer). Not emitted by the plain Haskell lexer, but
//     mapped defensively.
//   - SCE_HA_LITERATE_CODEDELIM (22) — the `\begin{code}` /
//     `\end{code}` LaTeX-literate delimiter or `>`-prefix
//     marker at column 0. Not emitted by the plain Haskell
//     lexer, but mapped defensively so a future L_LHASKELL
//     wiring inherits the correct visual for these delimiters.
pub const SCE_HA_DEFAULT: usize = 0;
pub const SCE_HA_IDENTIFIER: usize = 1;
pub const SCE_HA_KEYWORD: usize = 2;
pub const SCE_HA_NUMBER: usize = 3;
pub const SCE_HA_STRING: usize = 4;
pub const SCE_HA_CHARACTER: usize = 5;
pub const SCE_HA_CLASS: usize = 6;
pub const SCE_HA_MODULE: usize = 7;
pub const SCE_HA_CAPITAL: usize = 8;
pub const SCE_HA_DATA: usize = 9;
pub const SCE_HA_IMPORT: usize = 10;
pub const SCE_HA_OPERATOR: usize = 11;
pub const SCE_HA_INSTANCE: usize = 12;
pub const SCE_HA_COMMENTLINE: usize = 13;
pub const SCE_HA_COMMENTBLOCK: usize = 14;
pub const SCE_HA_COMMENTBLOCK2: usize = 15;
pub const SCE_HA_COMMENTBLOCK3: usize = 16;
pub const SCE_HA_PRAGMA: usize = 17;
pub const SCE_HA_PREPROCESSOR: usize = 18;
pub const SCE_HA_STRINGEOL: usize = 19;
pub const SCE_HA_RESERVED_OPERATOR: usize = 20;
pub const SCE_HA_LITERATE_COMMENT: usize = 21;
pub const SCE_HA_LITERATE_CODEDELIM: usize = 22;

// LexInno style indices. 13 contiguous slots (0..=12) for the
// Inno Setup script lexer — `.iss` installer script format used
// by the Inno Setup installer authoring tool. Written by
// Friedrich Vedder (2004) as a simple table-driven lexer that
// switches modes based on the current section (identified by
// `[SectionName]` headers). Dispatches SCLEX_INNOSETUP (= 76)
// via a **six-class wordlist** at
// `vendor/lexilla/lexers/LexInno.cxx:329-337`
// (`innoWordListDesc[]`):
//
//     innoWordListDesc[] = {
//         "Sections",                // class 0 → SCE_INNO_SECTION
//         "Keywords",                // class 1 → SCE_INNO_KEYWORD (`= `-suffix Setup directives)
//         "Parameters",              // class 2 → SCE_INNO_PARAMETER (`:`-suffix section-item params)
//         "Preprocessor directives", // class 3 → SCE_INNO_PREPROC
//         "Pascal keywords",         // class 4 → SCE_INNO_KEYWORD_PASCAL (inside [Code])
//         "User defined keywords",   // class 5 → SCE_INNO_KEYWORD_USER
//     };
//
// **Case-insensitive lexer.** Inno Setup language semantics:
// section names, directive names, and parameter names are all
// case-insensitive. `LexInno.cxx:172` / `:191` / `:232` call
// `tolower(ch)` on every identifier / section-name / preproc
// byte BEFORE the `keywords.InList(buffer)` lookup, so every
// wordlist entry MUST be lowercase — an uppercase or mixed-case
// entry would be dead code (the InList probe key is `appname`,
// never `AppName`, even though Inno source conventionally
// spells directives in PascalCase).
//
// **Context-dispatch quirks.** LexInno's classifier uses TWO
// dimensions to decide which wordlist to consult:
//
//   1. **Section context** (`isCode` flag, set true after a
//      `[Code]` section header at `:223`). When `isCode ==
//      true`, only the `pascalKeywords` wordlist is consulted
//      at `:201-202`; when false, `standardKeywords` /
//      `parameterKeywords` / `userKeywords` are all live at
//      `:197-204`.
//   2. **Token-following punctuation** — the `=` / `:`
//      distinction between Setup directives and section-item
//      parameters. Class 1 (`SCE_INNO_KEYWORD`) fires ONLY if
//      the token is followed by `=` (`innoNextNotBlankIs(i,
//      styler, '=')` at `:197`), and class 2
//      (`SCE_INNO_PARAMETER`) fires ONLY if followed by `:`
//      (`:199`). This is language-accurate — Inno Setup
//      distinguishes `AppName=...` (Setup directive assignment)
//      from `Name: ...` (section-item parameter assignment) —
//      but consequence for host wordlist authors: putting the
//      same token in both class 1 and class 2 is fine because
//      the classifier uses the following punctuation to decide,
//      not the wordlist membership order.
//
// **Style semantics (paint-loop citations reference LexInno.cxx):**
//
//   - SCE_INNO_DEFAULT (0) — inter-token slack.
//   - SCE_INNO_COMMENT (1) — `;`-prefixed line comment
//     (script-level Inno comment; the primary comment form
//     outside `[Code]`). Only fires when the `;` is at
//     beginning-of-line or after a run of only whitespace since
//     BOL (`isBOLWS` guard at `:131`) — a mid-line `; note`
//     does NOT start a comment.
//   - SCE_INNO_KEYWORD (2) — Setup-section directive name
//     (`AppName`, `DefaultDirName`, `Compression`, …). Fires
//     via the `standardKeywords.InList(buffer) &&
//     innoNextNotBlankIs(i, styler, '=')` guard at
//     `:197-198`.
//   - SCE_INNO_PARAMETER (3) — section-item parameter name
//     (`Source`, `DestDir`, `Flags`, …). Fires via the
//     `parameterKeywords.InList(buffer) &&
//     innoNextNotBlankIs(i, styler, ':')` guard at
//     `:199-200`.
//   - SCE_INNO_SECTION (4) — `[SectionName]` header at
//     `:215-231`. Matched against `sectionKeywords.InList`; on
//     hit the whole `[...]` span paints SECTION and the
//     classifier sets `isCode` / `isMessages` flags.
//   - SCE_INNO_PREPROC (5) — `#`-prefixed preprocessor directive
//     (`#define`, `#include`, `#if`, …). Fires via
//     `preprocessorKeywords.InList(buffer)` at `:246-247`.
//   - SCE_INNO_INLINE_EXPANSION (6) — `{code:...}` /
//     `{param:...}` inline preprocessor expansion embedded
//     inside string literals and directive values. Entered at
//     `:144` on encountering `{`.
//   - SCE_INNO_COMMENT_PASCAL (7) — Pascal `{...}` / `(*...*)`
//     block comment AND `//` line comment style, only fires
//     inside `[Code]` section (Pascal-style comments; the outer
//     script uses `;` line comments instead). Entered at
//     `:145-149` for `{`, `:150-154` for `(*`, `:155-159` for
//     `//`.
//   - SCE_INNO_KEYWORD_PASCAL (8) — Pascal reserved word inside
//     `[Code]` section (`begin`, `end`, `procedure`,
//     `function`, `if`, `then`, `else`, `for`, `while`, `try`,
//     `except`, `finally`, …). Fires via
//     `pascalKeywords.InList(buffer)` at `:201-202`, gated by
//     `isCode == true`.
//   - SCE_INNO_KEYWORD_USER (9) — user-customization slot.
//     Code++ ships this empty; a future per-project override
//     mechanism may populate it. Fires via
//     `userKeywords.InList(buffer)` at `:203-204`, gated by
//     `isCode == false` — user-defined keywords are NOT
//     recognized inside `[Code]` (matches the
//     two-dimensional-dispatch quirk above).
//   - SCE_INNO_STRING_DOUBLE (10) — `"..."` double-quoted
//     string literal. Entered at `:162-163`.
//   - SCE_INNO_STRING_SINGLE (11) — `'...'` single-quoted
//     string literal. Entered at `:166-167`. Both string
//     forms are valid Inno syntax.
//   - SCE_INNO_IDENTIFIER (12) — non-reserved word. The
//     scanning-state target that becomes SCE_INNO_DEFAULT on
//     wordlist miss at `:206`. Framework convention: leave
//     unmapped so ordinary parameter values and variable
//     names paint at STYLE_DEFAULT.
pub const SCE_INNO_DEFAULT: usize = 0;
pub const SCE_INNO_COMMENT: usize = 1;
pub const SCE_INNO_KEYWORD: usize = 2;
pub const SCE_INNO_PARAMETER: usize = 3;
pub const SCE_INNO_SECTION: usize = 4;
pub const SCE_INNO_PREPROC: usize = 5;
pub const SCE_INNO_INLINE_EXPANSION: usize = 6;
pub const SCE_INNO_COMMENT_PASCAL: usize = 7;
pub const SCE_INNO_KEYWORD_PASCAL: usize = 8;
pub const SCE_INNO_KEYWORD_USER: usize = 9;
pub const SCE_INNO_STRING_DOUBLE: usize = 10;
pub const SCE_INNO_STRING_SINGLE: usize = 11;
pub const SCE_INNO_IDENTIFIER: usize = 12;

// LexCmake style indices. 15 contiguous slots (0..=14) for
// the CMake build-system script lexer. Dispatches SCLEX_CMAKE
// (= 80) via a **three-class wordlist** at
// `vendor/lexilla/lexers/LexCmake.cxx:452-457`
// (`cmakeWordLists[]`):
//
//     cmakeWordLists[] = {
//         "Commands",     // class 0 → SCE_CMAKE_COMMANDS (case-insensitive)
//         "Parameters",   // class 1 → SCE_CMAKE_PARAMETERS (case-sensitive)
//         "UserDefined",  // class 2 → SCE_CMAKE_USERDEFINED (case-sensitive)
//         0, 0,           // NULL sentinels
//     };
//
// **Mixed case sensitivity — critical dispatch quirk.**
// `LexCmake.cxx:105-165` classifies each identifier through
// `classifyWordCmake`, which builds both `word` (preserved
// case) and `lowercaseWord` (lowered) buffers and then:
//
//   - Class 0 (`Commands`) uses `lowercaseWord` (`:135`) —
//     the CMake language treats commands like `add_executable`
//     / `ADD_EXECUTABLE` / `Add_Executable` as equivalent, so
//     the wordlist entry MUST be lowercase and the lexer
//     folds every candidate before probing.
//   - Class 1 (`Parameters`) uses `word` (`:138`) —
//     argument keywords like `PRIVATE` / `PUBLIC` /
//     `INTERFACE` / `REQUIRED` are conventionally uppercase
//     in CMake source and the lexer probes them
//     byte-exactly.
//   - Class 2 (`UserDefined`) uses `word` (`:142`) —
//     case-sensitive same as class 1, a project-override
//     customisation slot.
//
// Host wordlist consequence: `CMAKE_COMMANDS` must be all
// lowercase (uppercase entries are dead code); `CMAKE_PARAMETERS`
// / `CMAKE_USERDEFINED` must be exact-case (typically
// uppercase, matching CMake community convention).
//
// **Hard-coded contextual keywords (NOT in any wordlist).**
// The classifier at `:120-133` special-cases ten flow-control
// keywords with `CompareCaseInsensitive` and dispatches them
// to their own SCE states:
//
//   - `MACRO` / `ENDMACRO` → `SCE_CMAKE_MACRODEF` (`:120-121`)
//   - `IF` / `ENDIF` / `ELSEIF` / `ELSE` →
//     `SCE_CMAKE_IFDEFINEDEF` (`:123-127`)
//   - `WHILE` / `ENDWHILE` → `SCE_CMAKE_WHILEDEF`
//     (`:129-130`)
//   - `FOREACH` / `ENDFOREACH` → `SCE_CMAKE_FOREACHDEF`
//     (`:132-133`)
//
// These MUST NOT appear in `CMAKE_COMMANDS` — adding them
// would be dead code since the classifier short-circuits
// before reaching the wordlist dispatch, but including them
// would also mislead future maintainers about which SCE
// state fires.
//
// **Syntactic (non-wordlist) dispatches.**
//
//   - `SCE_CMAKE_VARIABLE` at `:145-148` — any identifier
//     whose second char is `{` and last char is `}` (i.e.
//     `${...}` / `$ENV{...}` / `$CACHE{...}` reference
//     patterns).
//   - `SCE_CMAKE_NUMBER` at `:150-162` — an identifier that
//     starts with a digit and contains only digits (bare
//     integer literal).
//   - `SCE_CMAKE_STRINGVAR` at `:339-348` — variable
//     interpolation `${var}` INSIDE any string state; a
//     sub-span colour applied over the outer string to
//     highlight the interpolation.
//
// Style semantics (paint-loop citations reference LexCmake.cxx):
//
//   - SCE_CMAKE_DEFAULT (0) — inter-token slack.
//   - SCE_CMAKE_COMMENT (1) — `#`-prefixed line comment.
//     CMake's only comment syntax.
//   - SCE_CMAKE_STRINGDQ (2) — `"..."` double-quoted string.
//     Entered from DEFAULT on `"` at `:318-319`. Terminated
//     on the matching close-quote.
//   - SCE_CMAKE_STRINGLQ (3) — `` `...` `` backtick-quoted
//     string, historical form retained by the lexer.
//     Entered at `:323`.
//   - SCE_CMAKE_STRINGRQ (4) — `'...'` single-quoted string,
//     historical form. Entered at `:328`. Modern CMake uses
//     `"..."` almost exclusively; both LQ and RQ states are
//     defensive.
//   - SCE_CMAKE_COMMANDS (5) — class 0 wordlist match (case-
//     insensitive) — CMake built-in commands.
//   - SCE_CMAKE_PARAMETERS (6) — class 1 wordlist match
//     (case-sensitive) — argument keywords / option names.
//   - SCE_CMAKE_VARIABLE (7) — syntactic `${...}` variable
//     reference at `:145-148`.
//   - SCE_CMAKE_USERDEFINED (8) — class 2 wordlist match
//     (case-sensitive) — user customisation slot.
//   - SCE_CMAKE_WHILEDEF (9) — hard-coded `WHILE` /
//     `ENDWHILE` at `:129-130`.
//   - SCE_CMAKE_FOREACHDEF (10) — hard-coded `FOREACH` /
//     `ENDFOREACH` at `:132-133`.
//   - SCE_CMAKE_IFDEFINEDEF (11) — hard-coded `IF` / `ENDIF`
//     / `ELSEIF` / `ELSE` at `:123-127`.
//   - SCE_CMAKE_MACRODEF (12) — hard-coded `MACRO` /
//     `ENDMACRO` at `:120-121`.
//   - SCE_CMAKE_STRINGVAR (13) — variable interpolation
//     `${var}` inside a string state. Distinct paint lets
//     themes emphasise the interpolation over the outer
//     string colour.
//   - SCE_CMAKE_NUMBER (14) — bare integer literal, syntactic.
pub const SCE_CMAKE_DEFAULT: usize = 0;
pub const SCE_CMAKE_COMMENT: usize = 1;
pub const SCE_CMAKE_STRINGDQ: usize = 2;
pub const SCE_CMAKE_STRINGLQ: usize = 3;
pub const SCE_CMAKE_STRINGRQ: usize = 4;
pub const SCE_CMAKE_COMMANDS: usize = 5;
pub const SCE_CMAKE_PARAMETERS: usize = 6;
pub const SCE_CMAKE_VARIABLE: usize = 7;
pub const SCE_CMAKE_USERDEFINED: usize = 8;
pub const SCE_CMAKE_WHILEDEF: usize = 9;
pub const SCE_CMAKE_FOREACHDEF: usize = 10;
pub const SCE_CMAKE_IFDEFINEDEF: usize = 11;
pub const SCE_CMAKE_MACRODEF: usize = 12;
pub const SCE_CMAKE_STRINGVAR: usize = 13;
pub const SCE_CMAKE_NUMBER: usize = 14;

// LexLua style indices. 21 contiguous slots (0..=20) covering
// the Lua lexer's full emission set: `--` line comments and
// `--[[ ]]` long-bracket block comments, the `---`-initiated
// LDoc-style documentation comments, decimal / hex / hex-float
// number literals, eight wordlist classes (`SCE_LUA_WORD` for
// reserved keywords plus `SCE_LUA_WORD2..WORD8` for the seven
// secondary library / user-customisation classes), double- and
// single-quoted strings, the `[[...]]` / `[=[...]=]` long-bracket
// literal strings, the obsolete Lua-pre-4.0 `$`-prefixed
// preprocessor directive, operators, identifiers, the unterminated
// string error indicator, and the `::name::` goto label anchors.
// Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 505-525 and
// `vendor/lexilla/lexers/LexLua.cxx` lines 51-61
// (`luaWordListDesc`), 65-88 (`lexicalClasses[]`), 191-228
// (`LexerLua::WordListSet` case dispatch), 472-494 (case-sensitive
// `keywords.InList` chain across all 8 wordlist classes), 525-532
// (`LongDelimCheck` long-bracket detection), 534-547 (`---` LDoc
// trigger + cross-line continuation flag), 548-549 (`$` column-0
// preprocessor directive), 320-396 (`::label::` definition AND
// `goto target` label-target paths).
//
// **Case-sensitive lexer.** Lua language semantics: every reserved
// keyword (`if` / `then` / `end` / `function` / `local` / `goto` /
// `return` / ...) is spelled lowercase. LexLua does NO case
// folding — `keywords.InList(identifier)` at `LexLua.cxx:472,479`
// matches the byte-exact source token against the installed
// wordlist (verified: `WordList::InList` at
// `vendor/lexilla/lexlib/WordList.cxx:162-170, 202-204` does
// byte-exact comparison with no `tolower` / `MakeLowerCase` /
// `CompareCaseInsensitive` anywhere on the path). Identifier text
// is captured raw via `sc.GetCurrentString(s, Transform::none)` at
// `LexLua.cxx:391`. Net result: `if` / `IF` / `If` are three
// distinct tokens; only the lowercase form matches a Lua keyword
// list. Wordlists must store source-canonical lowercase casing —
// same byte-exact contract as [`PERL_KEYWORDS`] / [`PYTHON_KEYWORDS`].
//
// **Eight wordlist classes (1 primary + 7 secondary).**
// `luaWordListDesc[]` declares eight slots: `"Keywords"`
// (class 0) → `SCE_LUA_WORD` bold; `"Basic functions"` (class 1)
// → `SCE_LUA_WORD2`; `"String, (table) & math functions"` (class
// 2) → `SCE_LUA_WORD3`; `"(coroutines), I/O & system facilities"`
// (class 3) → `SCE_LUA_WORD4`; `"user1"` / `"user2"` / `"user3"` /
// `"user4"` (classes 4-7) → `SCE_LUA_WORD5..WORD8`. The order is
// LOCKED by `LexLua.cxx:191-228` (`switch (n)` in
// `LexerLua::WordListSet` mapping `n` → `keywords{n+1}`) AND by
// the dispatch chain at `:479-494` consuming them in that exact
// order. So a "basic function" wordlist MUST go to
// `SCI_SETKEYWORDS` index 1, not 0, or it will be styled as a
// reserved keyword. Lexilla checks class 0 first; a cross-class
// duplicate silently demotes the secondary entry.
//
// **`SCE_LUA_LITERALSTRING` (8) trigger.** Long-bracket strings
// `[[...]]` / `[=[...]=]` / `[==[...]==]` … (up to 254 `=`
// characters). At `LexLua.cxx:525-532`: on `sc.ch == '['` from
// `SCE_LUA_DEFAULT`, `LongDelimCheck` at `:41-49` counts `=`
// characters between two brackets — zero → fall through to
// `SCE_LUA_OPERATOR` (subscript); ≥1 → `SetState(LITERALSTRING)`.
// Termination requires `LongDelimCheck` to return the SAME
// `sepCount` recorded on entry (`:437-442`), persisted across
// lines via the line-state low byte (`maskSeparator = 0xFF`).
//
// **`SCE_LUA_COMMENTDOC` (3) triggers.** Three paths at
// `LexLua.cxx:533-547`: explicit `---` triple-dash at `:542-544`
// (sets `lastLineDocComment = 0x200`); cross-line continuation at
// `:534` (the very-next-line `--` inherits doc-comment status via
// the line-state ternary `lastLineDocComment ? COMMENTDOC :
// COMMENTLINE`); plus `SCE_LUA_COMMENT` (the block-comment
// variant, NOT this slot) at `:535-541` via `--[[` / `--[=[`
// long-bracket form. The lexer does NOT parse LDoc `---@param` /
// `---@return` tags — the entire run from `---` to EOL is one
// flat `COMMENTDOC` token. Code++ themes this Comment-italic
// alongside `COMMENT` / `COMMENTLINE`.
//
// **`SCE_LUA_LABEL` (20) triggers.** Two distinct paths. (1)
// `::label::` definition at `LexLua.cxx:320-357` — when
// `OPERATOR` sees `:` with `chPrev == ':'`, a forward scan reads
// the identifier and requires a closing `::`; if the identifier
// is in the primary `keywords` list, the entire construct is
// REJECTED (`!keywords.InList(s)` guard at `:335`). On success
// four segments emit at `:341-353`. (2) `goto target` target
// identifier at `LexLua.cxx:382-396` — when the just-completed
// identifier was the keyword `goto` (tracked at `:515-517`), the
// next identifier types as `LABEL`; if the candidate turned out
// to be a reserved keyword (`goto end`), it downgrades to `WORD`
// at `:393`. Both paths REQUIRE `goto` to actually be in class 0
// (`keywords` list) — see [`LUA_KEYWORDS`] for the placement
// invariant.
//
// **`SCE_LUA_PREPROCESSOR` (9) trigger.** ONLY `$` at column 0
// (`LexLua.cxx:548-549`). The comment at `:549` is explicit:
// "Obsolete since Lua 4.0, but still in old code". This is NOT
// the shebang path — `#!` at top of document is handled separately
// at `:278-281` and types as `COMMENTLINE`, not `PREPROCESSOR`.
// Code++ themes this Preprocessor for visual identification but
// does NOT add it to the bold list — boldening dead syntax
// misleads. Same restraint applied as N++'s defaults.
//
// **`SCE_LUA_STRINGEOL` (12) intentionally unmapped.** Joins the
// deferred-Error-slot migration list — currently 12 entries after
// Python's `SCE_P_STRINGEOL` addition; Lua's `STRINGEOL` makes 13.
// LexLua emits this via `ChangeState` at `:416, 434` when a `"` /
// `'` string hits EOL without a closing quote AND `stringWs == 0`
// (the lexer recognises Lua 5.2+'s `\z` "skip whitespace" escape;
// a string mid-`\z`-suppression does NOT fire STRINGEOL on newline).
// Synthesising an ad-hoc red here creates palette drift that the
// Error-slot migration would have to clean up — leave unmapped
// (falls through to STYLE_DEFAULT) and migrate the whole cluster
// together.
//
// **`SCE_LUA_DEFAULT` (0) and `SCE_LUA_IDENTIFIER` (11) intentionally
// unmapped.** Universal omission pattern: bare-identifier and
// background-text styles render at STYLE_DEFAULT (the user's
// chosen foreground) — same precedent as `SCE_C_DEFAULT` /
// `SCE_C_IDENTIFIER`, `SCE_PAS_DEFAULT` / `SCE_PAS_IDENTIFIER`,
// `SCE_PL_DEFAULT` / `SCE_PL_IDENTIFIER`, `SCE_P_DEFAULT` /
// `SCE_P_IDENTIFIER`.
//
// **`SCE_LUA_WORD2..WORD8` (13-19) pre-themed despite partial
// host install.** Code++ ships [`LUA_KEYWORDS_2`] today (class 1
// = basic functions, drives `SCE_LUA_WORD2`); classes 2-7 are
// left unpopulated pending follow-on commits. All 7 secondary
// WORD slots map to Keyword2 in `LUA_STYLES` for forward-compat
// — costs seven table rows, gains zero-effort activation if a
// future commit adds `LUA_KEYWORDS_3` / `_4` (string-table-math
// / coroutine-io-os library names). Same forward-compat pattern
// as CSS EXTENDED_PSEUDOCLASS pre-theming and Python's ATTRIBUTE
// pre-theming.
pub const SCE_LUA_DEFAULT: usize = 0;
pub const SCE_LUA_COMMENT: usize = 1;
pub const SCE_LUA_COMMENTLINE: usize = 2;
pub const SCE_LUA_COMMENTDOC: usize = 3;
pub const SCE_LUA_NUMBER: usize = 4;
pub const SCE_LUA_WORD: usize = 5;
pub const SCE_LUA_STRING: usize = 6;
pub const SCE_LUA_CHARACTER: usize = 7;
pub const SCE_LUA_LITERALSTRING: usize = 8;
pub const SCE_LUA_PREPROCESSOR: usize = 9;
pub const SCE_LUA_OPERATOR: usize = 10;
pub const SCE_LUA_IDENTIFIER: usize = 11;
pub const SCE_LUA_STRINGEOL: usize = 12;
pub const SCE_LUA_WORD2: usize = 13;
pub const SCE_LUA_WORD3: usize = 14;
pub const SCE_LUA_WORD4: usize = 15;
pub const SCE_LUA_WORD5: usize = 16;
pub const SCE_LUA_WORD6: usize = 17;
pub const SCE_LUA_WORD7: usize = 18;
pub const SCE_LUA_WORD8: usize = 19;
pub const SCE_LUA_LABEL: usize = 20;

// LexTeX style indices. 6 contiguous slots (0..=5) covering the
// plain-TeX lexer's full emission set: comment-marker `%` and
// punctuation symbols (SYMBOL), `\command` keyword runs (COMMAND),
// `{` / `}` / `$` group delimiters (GROUP), the bracket /
// numeric special characters (SPECIAL), the comment body after
// `%` (DEFAULT), and the plain text fall-through (TEXT).
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 930-935 and `vendor/lexilla/lexers/LexTeX.cxx` lines
// 76-280.
//
// LexTeX is case-sensitive — `LexTeX.cxx:236` calls
// `keywords.InList(key)` against the raw `sc.GetCurrent(...)`
// buffer with no case folding; the `isTeXfive` character-class
// predicate at `:107-111` admits both `a..z` and `A..Z` so
// `\Section` and `\section` are distinct tokens (matches
// TeX-the-language semantics).
//
// **Comment body is `SCE_TEX_DEFAULT`, not a dedicated comment
// state.** The lexer's `%`-comment dispatch at `:248-254`:
// (1) styles the leading `%` as `SCE_TEX_SYMBOL` (style 3),
// (2) sets `SCE_TEX_DEFAULT` (style 0) on the next char for the
// rest of the comment body, (3) flips `inComment = true` so
// every subsequent char paints DEFAULT until EOL re-enters
// `SCE_TEX_TEXT` at `:210-215`. So `SCE_TEX_DEFAULT` is the
// comment-body slot — must route to `StyleSlot::Comment` and be
// italic. `SCE_TEX_TEXT` is the StyleContext initial state
// (`:202`) and the plain-prose fall-through — left unmapped,
// it renders as `STYLE_DEFAULT`.
//
// **Wordlist surface (7 classes), shipped empty for parity.**
// `texWordListDesc[]` at `LexTeX.cxx:487-496` declares 7 classes
// ("TeX, eTeX, pdfTeX, Omega" plus 6 ConTeXt language packs).
// Notepad++ defaults ship every class empty — and so does Code++.
// The reason is the lexer's behaviour at `:230-245`: with a
// populated wordlist, any `\command` NOT in the list silently
// downgrades from `SCE_TEX_COMMAND` to `SCE_TEX_TEXT` (plain
// prose). Users opening `.tex` files containing LaTeX content
// (the default `.tex` handler is L_TEX, not L_LATEX) would see
// `\section` / `\textbf` render as plain text while only the
// TeX primitives `\def` / `\let` highlighted — surprising
// visual feedback. Empty wordlist short-circuits the keyword
// check at `:230` and every `\command` paints as
// `SCE_TEX_COMMAND` uniformly. `TEX_THEME.keywords` is `&[]`.
pub const SCE_TEX_DEFAULT: usize = 0;
pub const SCE_TEX_SPECIAL: usize = 1;
pub const SCE_TEX_GROUP: usize = 2;
pub const SCE_TEX_SYMBOL: usize = 3;
pub const SCE_TEX_COMMAND: usize = 4;
pub const SCE_TEX_TEXT: usize = 5;

// LexSQL style indices. LexSQL defines 22 named style indices
// (`SCE_SQL_DEFAULT` 0 through `SCE_SQL_QOPERATOR` 24 with gaps at
// 12 and 14). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 1224-1246.
//
// LexSQL is **case-insensitive** — `LexSQL.cxx:786` lowercases every
// candidate token via `MakeLowerCase(styler[i+j])` before keyword
// comparison, so all wordlists installed against this lexer MUST be
// all-lowercase. Uppercase entries never match.
//
// Wordlist class assignments per `sqlWordListDesc[]`
// (`LexSQL.cxx:266-275`):
//   class 0 "Keywords"          → `SCE_SQL_WORD` (5)
//   class 1 "Database Objects"  → `SCE_SQL_WORD2` (16)
//   class 2 "PLDoc"             → `SCE_SQL_COMMENTDOCKEYWORD` (17)
//   class 3 "SQL*Plus"          → `SCE_SQL_SQLPLUS` (8)
//   classes 4-7 "User Keywords 1-4" → `SCE_SQL_USER1..USER4` (19-22)
//
// `SCE_SQL_DEFAULT` (0) and `SCE_SQL_IDENTIFIER` (11) intentionally
// not declared here — falls through to STYLE_DEFAULT (same omission
// pattern as `SCE_C_DEFAULT` / `SCE_C_IDENTIFIER`). The
// host-unmapped indices `SCE_SQL_COMMENTDOCKEYWORDERROR` (18 — error
// indicator, deferred to `StyleSlot::Error`), `SCE_SQL_QOPERATOR`
// (24 — Oracle `q'[...]'` alternate-quote marker, subordinate to the
// string body), and `SCE_SQL_USER1..USER4` (19-22 — user-customisable,
// deferred until a per-user wordlist UI lands) are likewise not
// declared. `SCE_SQL_QUOTEDIDENTIFIER` (23) IS declared below — it
// was exported as part of an earlier scintilla-sys scaffolding pass
// and is kept for backward compatibility of the FFI surface, but
// `SQL_STYLES` deliberately does not map it (quoted identifiers fall
// through to STYLE_DEFAULT, same omission rationale as the bare
// `SCE_SQL_IDENTIFIER`).
pub const SCE_SQL_COMMENT: usize = 1;
pub const SCE_SQL_COMMENTLINE: usize = 2;
pub const SCE_SQL_COMMENTDOC: usize = 3;
pub const SCE_SQL_NUMBER: usize = 4;
pub const SCE_SQL_WORD: usize = 5;
pub const SCE_SQL_STRING: usize = 6;
pub const SCE_SQL_CHARACTER: usize = 7;
pub const SCE_SQL_SQLPLUS: usize = 8;
pub const SCE_SQL_SQLPLUS_PROMPT: usize = 9;
pub const SCE_SQL_OPERATOR: usize = 10;
pub const SCE_SQL_SQLPLUS_COMMENT: usize = 13;
pub const SCE_SQL_COMMENTLINEDOC: usize = 15;
pub const SCE_SQL_WORD2: usize = 16;
pub const SCE_SQL_COMMENTDOCKEYWORD: usize = 17;
pub const SCE_SQL_QUOTEDIDENTIFIER: usize = 23;

// LexVB style indices. 13 contiguous slots (0..=12) covering the
// Visual Basic family (VB.NET, VBScript, VBA, VB Classic) — `'`
// line comments, decimal / `&H` hex / `&O` octal / `&B` binary
// numbers, four keyword classes (only classes 0 + 1 are populated
// by `VB_THEME`; classes 2 + 3 are wordlist slots Notepad++ leaves
// unset for general `.vb` files), double-quoted strings,
// `#`-prefixed preprocessor directives, operator punctuation,
// identifiers, `#1/1/2024#` date literals, and the
// unterminated-string error state. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 463-475 and
// `vendor/lexilla/lexers/LexVB.cxx` lines 87-101 (lexicalClasses[]).
//
// LexVB is **case-insensitive** — `LexVB.cxx:208` calls
// `sc.GetCurrentLowered(s, ...)` to lowercase candidate tokens
// before consulting any wordlist. Wordlists installed against this
// lexer MUST be all-lowercase.
//
// `SCE_B_DEFAULT` (0), `SCE_B_IDENTIFIER` (7), and `SCE_B_STRINGEOL`
// (9) are intentionally unmapped in `VB_STYLES` — fall through to
// STYLE_DEFAULT (same omission pattern as `SCE_PAS_DEFAULT` /
// `SCE_PAS_IDENTIFIER` / `SCE_PAS_STRINGEOL`). The STRINGEOL
// indicator is also pending the future `StyleSlot::Error` palette
// addition.
//
// Indices 13-22 (`SCE_B_CONSTANT` / `SCE_B_ASM` / `SCE_B_LABEL` /
// `SCE_B_ERROR` / `SCE_B_HEXNUMBER` / `SCE_B_BINNUMBER` /
// `SCE_B_COMMENTBLOCK` / `SCE_B_DOCLINE` / `SCE_B_DOCBLOCK` /
// `SCE_B_DOCKEYWORD`) are `LexBasic`-family emissions (used by
// BlitzBasic / PureBasic / FreeBasic — the three-way shared lexer
// at `vendor/lexilla/lexers/LexBasic.cxx`). `LexVB` itself never
// emits them; the paint-loop analysis of when each is entered lives
// in each family member's `LangTheme` banner in
// `crates/ui_win32/src/lib.rs` (see `BLITZBASIC_THEME`).
//
// **`LexBasic` dispatch registration** at `LexBasic.cxx:569-573`:
//   - `lmBlitzBasic(SCLEX_BLITZBASIC = 66, LexerFactoryBlitzBasic,
//     "blitzbasic", blitzbasicWordListDesc)` — `';'` comment char.
//   - `lmPureBasic(SCLEX_PUREBASIC = 67, ..., "purebasic",
//     purebasicWordListDesc)` — `';'` comment char.
//   - `lmFreeBasic(SCLEX_FREEBASIC = 75, ..., "freebasic",
//     freebasicWordListDesc)` — `'\''` (apostrophe) comment char.
// All three share the same paint function
// (`LexerBasic::Lex` at `LexBasic.cxx:322`) and per-language fold
// point checker (`CheckBlitzFoldPoint` / `CheckPureFoldPoint` /
// `CheckFreeFoldPoint` at `:103-156`). The comment character and
// wordlist descriptor are the only per-family differences at the
// lexer level; the SCE_B_* namespace is fully shared.
//
// **Last-match-wins keyword classification** at
// `LexBasic.cxx:348-352`. Unlike LexAsn1 / LexSpice / LexAVS /
// most other Lexilla lexers (first-match-wins), the LexBasic
// classifier iterates `for (int i = 0; i < 4; i++)` and calls
// `ChangeState(kstates[i])` on EACH match — the loop does not
// break. So a token in both class 0 and class 3 ends up as
// `SCE_B_KEYWORD4` (the last matching class wins). Framework
// consequence: BlitzBasic / PureBasic / FreeBasic themes that
// install multiple wordlist classes must treat higher class
// indices as VISUALLY DOMINANT — the opposite of the usual
// first-match-wins mental model. Code++'s BlitzBasic theme
// deliberately keeps class 0 and class 1 disjoint to avoid the
// interaction entirely; a class-0-vs-class-1 duplicate would
// silently render as `SCE_B_KEYWORD2` instead of `SCE_B_KEYWORD`.
//
// **Case-INSENSITIVE keyword lookup** at `LexBasic.cxx:347` via
// `sc.GetCurrentLowered(s, sizeof(s))` — same discipline as LexVB
// / LexAVS. Wordlist entries must be byte-canonical lowercase.
pub const SCLEX_BLITZBASIC: usize = 66;
pub const SCLEX_PUREBASIC: usize = 67;
pub const SCLEX_FREEBASIC: usize = 75;
pub const SCE_B_DEFAULT: usize = 0;
pub const SCE_B_COMMENT: usize = 1;
pub const SCE_B_NUMBER: usize = 2;
pub const SCE_B_KEYWORD: usize = 3;
pub const SCE_B_STRING: usize = 4;
pub const SCE_B_PREPROCESSOR: usize = 5;
pub const SCE_B_OPERATOR: usize = 6;
pub const SCE_B_IDENTIFIER: usize = 7;
pub const SCE_B_DATE: usize = 8;
pub const SCE_B_STRINGEOL: usize = 9;
pub const SCE_B_KEYWORD2: usize = 10;
pub const SCE_B_KEYWORD3: usize = 11;
pub const SCE_B_KEYWORD4: usize = 12;
pub const SCE_B_CONSTANT: usize = 13;
pub const SCE_B_ASM: usize = 14;
pub const SCE_B_LABEL: usize = 15;
pub const SCE_B_ERROR: usize = 16;
pub const SCE_B_HEXNUMBER: usize = 17;
pub const SCE_B_BINNUMBER: usize = 18;
pub const SCE_B_COMMENTBLOCK: usize = 19;
pub const SCE_B_DOCLINE: usize = 20;
pub const SCE_B_DOCBLOCK: usize = 21;
pub const SCE_B_DOCKEYWORD: usize = 22;

// LexYAML style indices. 10 contiguous slots (0..=9) for the
// YAML line-oriented scalar-value lexer. Dispatches SCLEX_YAML
// (= 48) via a **single-class wordlist** ("Keywords") at
// `vendor\lexilla\lexers\LexYAML.cxx:33-36` (`yamlWordListDesc[]`):
//
//     yamlWordListDesc[] = { "Keywords", nullptr };
//
// **Wordlist semantics — value-position boolean/null tokens.**
// `LexYAML.cxx:188` probes `KeywordAtChar(&lineBuffer[i],
// &lineBuffer[startComment], keywords)` on the value span AFTER
// the `key: ` prefix (i.e. only in the mapping-value position,
// never in the key position and never inside quoted strings).
// The probe is byte-exact via `WordList::InList`, so every
// case variant the theme wants highlighted must appear
// literally. The canonical set is the YAML 1.1 boolean/null
// spelling family (`true`/`True`/`TRUE`, `false`/`False`/`FALSE`,
// `yes`/`no`/`on`/`off` and their case variants, plus `null`/
// `Null`/`NULL` and the tilde `~`). YAML 1.2 restricts these to
// lowercase only but real-world YAML files use every case so
// the full set stays.
//
// **Non-wordlist syntactic dispatches** (line-oriented state
// machine at `LexYAML.cxx:86-216`):
//
//   - `SCE_YAML_DOCUMENT` at `:112-115` — line starts with
//     `---` (document start) or `...` (document end). Whole
//     line coloured.
//   - `SCE_YAML_ERROR` at `:121-124` — indented line with a
//     TAB in the leading whitespace (YAML forbids tab
//     indentation outside block scalars). Whole line
//     coloured; block-header syntax errors also route here.
//   - `SCE_YAML_COMMENT` at `:125-128` — first non-space
//     char is `#`. Whole line coloured. Also mid-line
//     comments at `:133-136` after a space-padded `#`.
//   - `SCE_YAML_IDENTIFIER` at `:138` — token before the
//     first unquoted `:` followed by whitespace or EOL. The
//     mapping-key position.
//   - `SCE_YAML_OPERATOR` at `:139` — the `:` separator
//     itself.
//   - `SCE_YAML_TEXT` at `:106` — content of a folded (`>`)
//     or literal (`|`) block scalar, tracked across lines
//     via the parent-line-state indent comparison at
//     `:99-109`. Block scalar content is verbatim string
//     data.
//   - `SCE_YAML_REFERENCE` at `:183` — value starting with
//     `&` (anchor) or `*` (alias), read to end-of-value.
//   - `SCE_YAML_KEYWORD` at `:189` — wordlist match on the
//     value span (see wordlist semantics above).
//   - `SCE_YAML_NUMBER` at `:206` — value containing only
//     digits / `-` / `.` / `,` / space. Bare numeric scalar.
//   - `SCE_YAML_DEFAULT` at `:134, :156, :168, :198, :215` —
//     fallthrough for values that match no more specific
//     state (unquoted plain-scalar strings, empty block
//     scalar headers, and inter-token slack).
//
// Style semantics (paint-loop citations reference LexYAML.cxx):
//
//   - SCE_YAML_DEFAULT (0) — inter-token slack + plain
//     unquoted scalar values. Framework convention: leave
//     unmapped so plain string values paint at STYLE_DEFAULT.
//   - SCE_YAML_COMMENT (1) — `#`-prefixed comment.
//   - SCE_YAML_IDENTIFIER (2) — key in a mapping-key
//     position (the token before the first `:`). Deliberately
//     mapped (unlike most IDENTIFIER states which follow the
//     framework's "bare identifier → DEFAULT" rule) because
//     YAML keys are structurally distinct — the key IS the
//     structural anchor of a mapping and users read it as
//     "the label" rather than "an ordinary identifier"; this
//     mirrors `SCE_P_CLASSNAME` / `SCE_P_DEFNAME` /
//     `SCE_PL_SUB_PROTOTYPE` / `SCE_PL_FORMAT_IDENT` which
//     also route structural-name identifier states to
//     `Keyword2`.
//   - SCE_YAML_KEYWORD (3) — wordlist match (case-exact) on
//     a value-position boolean/null token.
//   - SCE_YAML_NUMBER (4) — bare numeric scalar in value
//     position.
//   - SCE_YAML_REFERENCE (5) — `&anchor` / `*alias`
//     definition or dereference.
//   - SCE_YAML_DOCUMENT (6) — `---` document-start /
//     `...` document-end marker line.
//   - SCE_YAML_TEXT (7) — content of a folded / literal
//     block scalar spanning multiple lines. String content
//     by semantics.
//   - SCE_YAML_ERROR (8) — TAB-indented line or malformed
//     block scalar header. Framework convention: no Error
//     slot — leave unmapped so the buffer stays legible at
//     STYLE_DEFAULT rather than lighting up in an arbitrary
//     colour.
//   - SCE_YAML_OPERATOR (9) — the mapping-separator `:`.
pub const SCE_YAML_DEFAULT: usize = 0;
pub const SCE_YAML_COMMENT: usize = 1;
pub const SCE_YAML_IDENTIFIER: usize = 2;
pub const SCE_YAML_KEYWORD: usize = 3;
pub const SCE_YAML_NUMBER: usize = 4;
pub const SCE_YAML_REFERENCE: usize = 5;
pub const SCE_YAML_DOCUMENT: usize = 6;
pub const SCE_YAML_TEXT: usize = 7;
pub const SCE_YAML_ERROR: usize = 8;
pub const SCE_YAML_OPERATOR: usize = 9;

// LexCOBOL style indices. 13 slots with a **non-contiguous
// numbering** — `SCE_COBOL_WORD2` occupies slot 16, not 12.
// The gap (12..=15) was reserved for future Scintilla family
// use and never filled; treating WORD2 as 12 would silently
// bind the theme to a state the lexer never emits. Constants
// mirror `SciLexer.h:209-221` verbatim. Dispatches SCLEX_COBOL
// (= 92, per `SciLexer.h:108`) via a **three-class wordlist**
// at `vendor\lexilla\lexers\LexCOBOL.cxx:381-386`
// (`COBOLWordListDesc[]`):
//
//     COBOLWordListDesc[] = {
//         "A Keywords",         // class 0 → SCE_COBOL_WORD
//         "B Keywords",         // class 1 → SCE_COBOL_WORD2
//         "Extended Keywords",  // class 2 → SCE_COBOL_WORD3
//         nullptr,
//     };
//
// **Case-fold classification — CRITICAL.** `LexCOBOL.cxx:76`
// (`getRange`) writes `s[i] = tolower(styler[start+i])` into
// the classification buffer BEFORE the `WordList::InList`
// probe at `:107-121`. COBOL is case-insensitive at the
// language level (`MOVE`, `move`, `Move` are the same verb)
// and the lexer folds every candidate; wordlist entries
// therefore MUST be all-lowercase, same discipline as
// `LexAda` / `LexCmake`. An uppercase entry silently never
// matches — dead code and misleading.
//
// **Sequential-probe A→B→C — dispatch order matters.** The
// classifier at `:112-120` probes list A first, then B, then
// C, first-match-wins. Any token appearing in two lists is
// resolved to the earlier list's SCE state; the later
// entry is dead code. Cross-list duplicates must be
// deliberate (host-side test invariant enforces uniqueness).
//
// **Hyphen is a word character.** `isCOBOLwordchar` at `:47-51`
// treats `-` as part of an identifier, so compound tokens
// like `working-storage`, `high-values`, `packed-decimal`,
// `comp-3`, `end-if`, `date-written` are SINGLE lexemes.
// Wordlist entries for them are written literally with the
// hyphen; splitting them into two tokens breaks the match.
//
// **Column-based intrinsic dispatches** (no wordlist
// involvement — the lexer decides on column position alone):
//
//   - `SCE_COBOL_COMMENTLINE` at column 7 (0-indexed 6) with
//     `*` or `/` → fixed-format comment (`LexCOBOL.cxx:215-218`).
//     Also matches inline `*>` anywhere via `:219-222` for
//     free-format COBOL 2002+ syntax, and col-0 single
//     `*`/`/` at `:223-228`.
//   - `SCE_COBOL_COMMENTDOC` at column 0 with `**` or `/*`
//     → doc comment (`:229-234`).
//   - `SCE_COBOL_PREPROCESSOR` at column 0 with `?` →
//     preprocessor directive (`:241-243`), for the rare
//     COBOL preprocessors that use this convention.
//
// **A-area division/section recognition** (`:122-142`,
// inside `classifyWordCOBOL`) — the lexer tracks `bAarea`
// (whether the current token starts in cols 1-2) and
// hard-codes recognition of `division` / `declaratives` /
// `section` / `end` literals to compute fold levels via
// bitflags
// `IN_DIVISION`/`IN_DECLARATIVES`/`IN_SECTION`/`IN_PARAGRAPH`.
// These four tokens are handled by the lexer intrinsically
// via `strcmp` on the same lowercased buffer that fed the
// InList probes; they DO belong in `COBOL_KEYWORDS_A` (they
// colour as verbs/structural markers) but their fold-level
// effect is separate from wordlist highlighting.
//
// Style semantics (paint-loop citations reference LexCOBOL.cxx):
//
//   - SCE_COBOL_DEFAULT (0) — whitespace / unstyled slack.
//     Framework convention: leave unmapped.
//   - SCE_COBOL_COMMENT (1) — legacy state defined in
//     `SciLexer.h` but not emitted by the current
//     state machine. Mapped defensively so a future Lexilla
//     revision that revives the state doesn't render
//     un-coloured.
//   - SCE_COBOL_COMMENTLINE (2) — fixed-format col-7
//     `*`/`/`, col-0 `*`/`/`, or inline `*>`.
//   - SCE_COBOL_COMMENTDOC (3) — col-0 `**` or `/*`.
//   - SCE_COBOL_NUMBER (4) — digit / `.` / `v` (decimal
//     marker). Also catches COBOL level numbers (`01`, `05`,
//     `77`, `88`) so they don't need wordlist entries.
//   - SCE_COBOL_WORD (5) — wordlist class 0 hit (A Keywords —
//     verbs, divisions, sections, control flow).
//   - SCE_COBOL_STRING (6) — `"..."` double-quoted literal.
//   - SCE_COBOL_CHARACTER (7) — `'...'` single-quoted literal.
//     Semantically the same as STRING at the theme level.
//   - SCE_COBOL_WORD3 (8) — wordlist class 2 hit (Extended
//     Keywords — intrinsic functions like `function`,
//     `length`, `upper-case`).
//   - SCE_COBOL_PREPROCESSOR (9) — `?` at column 0.
//   - SCE_COBOL_OPERATOR (10) — `isoperator()` classification.
//   - SCE_COBOL_IDENTIFIER (11) — bare-identifier fallback.
//     Framework convention: leave unmapped so plain data
//     names (`customer-record`, `total-amount`) paint at
//     STYLE_DEFAULT.
//   - SCE_COBOL_WORD2 (16) — wordlist class 1 hit (B Keywords —
//     PICTURE / VALUE / USAGE clauses, figurative constants,
//     file descriptors). **NOTE the non-sequential 16 —
//     slots 12..=15 are reserved and unused.**
pub const SCE_COBOL_DEFAULT: usize = 0;
pub const SCE_COBOL_COMMENT: usize = 1;
pub const SCE_COBOL_COMMENTLINE: usize = 2;
pub const SCE_COBOL_COMMENTDOC: usize = 3;
pub const SCE_COBOL_NUMBER: usize = 4;
pub const SCE_COBOL_WORD: usize = 5;
pub const SCE_COBOL_STRING: usize = 6;
pub const SCE_COBOL_CHARACTER: usize = 7;
pub const SCE_COBOL_WORD3: usize = 8;
pub const SCE_COBOL_PREPROCESSOR: usize = 9;
pub const SCE_COBOL_OPERATOR: usize = 10;
pub const SCE_COBOL_IDENTIFIER: usize = 11;
pub const SCE_COBOL_WORD2: usize = 16;

// LexGui4Cli style indices. 10 contiguous slots (0..=9) for
// the Gui4Cli GUI-scripting language lexer. **Constant prefix
// is `SCE_GC_`, not `SCE_GUI4CLI_`** — this matches Lexilla's
// own enum in `SciLexer.h:1039-1048` verbatim (the file-header
// comment inside `LexGui4Cli.cxx:13-22` documents the same
// `SCE_GC_*` names). Renaming to `SCE_GUI4CLI_*` on the host
// side would break greppability against the vendor tree.
// Dispatches SCLEX_GUI4CLI via a **five-class wordlist** at
// `vendor\lexilla\lexers\LexGui4Cli.cxx:306-309`
// (`gui4cliWordListDesc[]`):
//
//     gui4cliWordListDesc[] = {
//         "Globals",     // class 0 → SCE_GC_GLOBAL
//         "Events",      // class 1 → SCE_GC_EVENT
//         "Attributes",  // class 2 → SCE_GC_ATTRIBUTE
//         "Control",     // class 3 → SCE_GC_CONTROL
//         "Commands",    // class 4 → SCE_GC_COMMAND
//         0
//     };
//
// **Case-fold classification — CRITICAL.** `LexGui4Cli.cxx:89-93`
// walks the captured token buffer and does `*p = toupper(*p)`
// before `WordList::InList` probes. Gui4Cli is case-insensitive
// (a 90s-era GUI scripting language, keywords typed in any case);
// wordlist entries therefore MUST be all-UPPERCASE. A single
// lowercase entry silently never matches — same discipline as
// `LexCOBOL`'s uppercase policy but inverted from
// `LexAda` / `LexCmake`'s lowercase policy. Test invariant
// enforces this.
//
// **Probe order at `:105-109` — NOT descriptor order.** The
// classifier probes:
//
//     Globals → Attributes → Control → Commands → Events
//                                                   ^
//                                              Events LAST
//
// This is decoupled from `gui4cliWordListDesc[]`'s
// declaration order (which SCI_SETKEYWORDS respects). First
// match wins across the five lists — a token that appears
// in both Globals and Events resolves as Global. Wordlists
// must be mutually disjoint for the intended paint to fire.
//
// **Statement-position matching only.** `colorFirstWord`
// (`:72-120`) is called from the main dispatch at document
// start, after every `\n` / `\r` (`:226-236`), and after
// every `;` statement terminator (`:191-202`). Keyword
// highlighting fires ONLY for the leading token of a
// statement — the same word appearing mid-statement stays
// `SCE_GC_DEFAULT`. This is a lexer behaviour, not a host
// concern; document it in the theme so future readers don't
// file a false bug.
//
// **No number, no identifier, no preprocessor state.** The
// lexer emits exactly the 10 states declared here — plain
// integers and identifiers both fall through to
// `SCE_GC_DEFAULT`. Do not attempt to map `Number` or
// route `Preprocessor` beyond the states listed.
//
// **Word-char alphabet extends beyond alnum.** `IsAWordChar`
// at `:50-52` accepts letters, digits, `.`, `_`, AND `\`
// (backslash) — so `path\to\file` reads as a single
// identifier. The `\` escape dispatch at `:215-224` marks
// the backslash + next character as `SCE_GC_OPERATOR` even
// inside strings, then restores the previous state.
//
// **Fold points at Globals and Events.** `FoldGui4Cli`
// (`:271-273`) sets header points on any line whose lead
// token classifies as `SCE_GC_GLOBAL` or `SCE_GC_EVENT`.
// Fold behaviour is intrinsic — host doesn't cooperate.
//
// Style semantics (paint-loop citations reference LexGui4Cli.cxx):
//
//   - SCE_GC_DEFAULT (0) — whitespace, bare identifiers,
//     `$var` payloads (the `$` sigil itself is OPERATOR at
//     `:204-213`, but the identifier following falls
//     through), numeric literals. Framework convention:
//     leave unmapped.
//   - SCE_GC_COMMENTLINE (1) — `//`-to-EOL comment
//     (`:146-153`).
//   - SCE_GC_COMMENTBLOCK (2) — `/* ... */` block comment
//     (`:154-158`; closed at `:163-173`).
//   - SCE_GC_GLOBAL (3) — wordlist class 0 hit (Globals —
//     top-level control declarations like `G4C`, `WINDOW`,
//     `XBUTTON`).
//   - SCE_GC_EVENT (4) — wordlist class 1 hit (Events —
//     handler declarations like `XONLOAD`, `XONCLICK`).
//     Probes LAST at `:105-109`.
//   - SCE_GC_ATTRIBUTE (5) — wordlist class 2 hit
//     (the attribute-clause declarator `ATTR`; the sole
//     entry per the vendor `SciTE.properties` — see
//     `GUI4CLI_ATTRIBUTES` docstring in `codepp_core::lang`
//     for the statement-position-matching rationale).
//   - SCE_GC_CONTROL (6) — wordlist class 3 hit (Control —
//     flow keywords like `IF`, `ELSE`, `ENDIF`, `GOSUB`).
//   - SCE_GC_COMMAND (7) — wordlist class 4 hit (Commands —
//     built-in verbs like `GUIOPEN`, `MSGBOX`,
//     `SETWINTITLE`).
//   - SCE_GC_STRING (8) — `'...'` or `"..."` literal
//     (`:175-189`). Both quote characters are accepted;
//     lexer records the opening quote at `:187` and matches
//     the same character to close.
//   - SCE_GC_OPERATOR (9) — arithmetic/relational
//     operators + `;` statement terminator (`:191-202`) +
//     `$` variable sigil + `\` escape (`:215-224`).
pub const SCE_GC_DEFAULT: usize = 0;
pub const SCE_GC_COMMENTLINE: usize = 1;
pub const SCE_GC_COMMENTBLOCK: usize = 2;
pub const SCE_GC_GLOBAL: usize = 3;
pub const SCE_GC_EVENT: usize = 4;
pub const SCE_GC_ATTRIBUTE: usize = 5;
pub const SCE_GC_CONTROL: usize = 6;
pub const SCE_GC_COMMAND: usize = 7;
pub const SCE_GC_STRING: usize = 8;
pub const SCE_GC_OPERATOR: usize = 9;

// LexD style indices. 23 contiguous slots (0..=22) for the
// D programming language lexer. Constants mirror
// `SciLexer.h:222-244` verbatim. Dispatches SCLEX_D (= 79,
// per `SciLexer.h:95`) via a **seven-class wordlist** at
// `vendor\lexilla\lexers\LexD.cxx:104-113` (`dWordLists[]`):
//
//     dWordLists[] = {
//         "Primary keywords and identifiers",       // class 0 → SCE_D_WORD
//         "Secondary keywords and identifiers",     // class 1 → SCE_D_WORD2
//         "Documentation comment keywords",         // class 2 → SCE_D_COMMENTDOCKEYWORD
//         "Type definitions and aliases",           // class 3 → SCE_D_TYPEDEF
//         "Keywords 5",                             // class 4 → SCE_D_WORD5
//         "Keywords 6",                             // class 5 → SCE_D_WORD6
//         "Keywords 7",                             // class 6 → SCE_D_WORD7
//         0,
//     };
//
// **Case sensitivity — configurable per instance, default
// case-sensitive.** `LexerD::LexerFactoryD` at
// `LexD.cxx:198-200` constructs the lexer with
// `caseSensitive = true`. The identifier classification
// cascade at `:288-311` calls `sc.GetCurrent(s, sizeof(s))`
// (byte-exact) when `caseSensitive` is true, and
// `sc.GetCurrentLowered(s, sizeof(s))` (lowercased) when
// false. Since D is a case-sensitive language at the spec
// level and the factory default is case-sensitive,
// wordlists MUST be exact-case (lowercase for D keywords).
// Same discipline as `CPP_KEYWORDS` — every entry lives
// under a byte-exact InList probe.
//
// **`SCE_D_WORD3` (value 8) declared but never emitted.**
// SciLexer.h reserves the slot, but LexD.cxx's identifier
// classification cascade at `:296-307` probes wordlists in
// the order 0/1/3/4/5/6 (SKIPPING index 2). Wordlist index
// 2 IS used but routes to `SCE_D_COMMENTDOCKEYWORD`, not
// to any WORDN state — that dispatch lives at `:358` inside
// the `SCE_D_COMMENTDOCKEYWORD` state and only fires within
// a doc comment (`/** */` or `///`) after a `@` or `\`
// sigil. Consequence: mapping `SCE_D_WORD3` in the host
// theme is dead code — the lexer never emits it. Test
// invariant enforces the exclusion.
//
// **Wordlist install map** (per `LexerD::WordListSet` at
// `:210-234`):
//
//     class 0 → keywords  → SCE_D_WORD                (control flow / declarations)
//     class 1 → keywords2 → SCE_D_WORD2               (storage classes / contracts)
//     class 2 → keywords3 → SCE_D_COMMENTDOCKEYWORD   (Ddoc @-tags)
//     class 3 → keywords4 → SCE_D_TYPEDEF             (primitive types / aliases)
//     class 4 → keywords5 → SCE_D_WORD5               (special values / literals)
//     class 5 → keywords6 → SCE_D_WORD6               (traits / meta-programming)
//     class 6 → keywords7 → SCE_D_WORD7               (reserved for user extension)
//
// **String flavors — five distinct states for one visual
// slot.** D's string zoo has:
//
//   - `SCE_D_STRING` (10) — `"..."` double-quoted literal
//     (`LexD.cxx:381-391`, entered at `:459`).
//   - `SCE_D_STRINGB` (18) — `` `...` `` backtick wysiwyg
//     literal (`:409-415`, entered at `:463`).
//   - `SCE_D_STRINGR` (19) — `r"..."` raw string, `x"..."`
//     hex string, `q"..."` delimited string (`:416-422`,
//     entered at `:435`).
//   - `SCE_D_CHARACTER` (12) — `'c'` character literal
//     (`:392-403`, entered at `:461`).
//   - `SCE_D_STRINGEOL` (11) — unterminated string
//     reaching EOL (`:394, :404-408`).
//
// All five collapse to `StyleSlot::String` at the theme
// level for uniform visual identity.
//
// **String suffixes.** `c`/`w`/`d` suffixes are consumed
// after the closing quote for all three string flavors
// via `IsStringSuffix` at `:63-65` (called at :387, :411,
// :418).
//
// **Nested `/+ +/` comments — depth tracked per line.**
// `SCE_D_COMMENTNESTED` (4) is entered at `:443` and
// increments/decrements `curNcLevel` at `:364-380` and
// `:439-444`, persisted per line via
// `styler.SetLineState` at `:263, :369, :377, :442`. Pure
// lexer concern — no host configuration.
//
// **Doc-comment keyword state — bidirectional return.**
// `SCE_D_COMMENTDOCKEYWORD` (16) is entered from either
// `SCE_D_COMMENTDOC` (3) or `SCE_D_COMMENTLINEDOC` (15)
// on the `@`/`\` sigil (`:322-328, :338-344`);
// `styleBeforeDCKeyword` remembers which to return to
// after the tag identifier (`:346-362`). Wordlist index 2
// validates the keyword and routes invalid tags to
// `SCE_D_COMMENTDOCKEYWORDERROR` (17) at `:348, :359`.
//
// **Doc-style detection.**
//   - `/**` or `/*!` → COMMENTDOC (`:446`).
//   - `///` (but NOT `////`) or `//!` → COMMENTLINEDOC
//     (`:453`).
//
// Style semantics (paint-loop citations reference LexD.cxx):
//
//   - SCE_D_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_D_COMMENT (1) — `/* ... */` block comment.
//   - SCE_D_COMMENTLINE (2) — `//`-to-EOL line comment.
//   - SCE_D_COMMENTDOC (3) — `/** ... */` or `/*! ... */`
//     Ddoc block comment.
//   - SCE_D_COMMENTNESTED (4) — `/+ ... +/` nested block
//     comment (D-specific — nests without escaping).
//   - SCE_D_NUMBER (5) — numeric literal. Recognises hex
//     (`0x`), binary (`0b`), underscore digit separators,
//     `e±` / `p±` scientific exponents, and `f`/`F`/`L`/`i`
//     suffixes.
//   - SCE_D_WORD (6) — wordlist class 0 hit (Primary
//     keywords — control flow / declarations).
//   - SCE_D_WORD2 (7) — wordlist class 1 hit (Secondary
//     keywords — storage classes / contracts).
//   - SCE_D_WORD3 (8) — DECLARED BUT NEVER EMITTED.
//     LexD.cxx's cascade at `:296-307` skips this index
//     because wordlist class 2 is repurposed for
//     `SCE_D_COMMENTDOCKEYWORD`. Framework convention:
//     leave unmapped — mapping it would be dead code.
//   - SCE_D_TYPEDEF (9) — wordlist class 3 hit (primitive
//     types / aliases like `int`, `string`, `size_t`).
//   - SCE_D_STRING (10) — `"..."` double-quoted literal.
//   - SCE_D_STRINGEOL (11) — unterminated string reaching
//     EOL.
//   - SCE_D_CHARACTER (12) — `'c'` character literal.
//   - SCE_D_OPERATOR (13) — punctuation classified via
//     `isoperator()` at `:464-467`; the OPERATOR state
//     terminates back to DEFAULT at `:268-270`. Includes
//     `@` sigil — attribute keywords like `@safe` render
//     as OPERATOR + IDENTIFIER, so bare `safe` / `nogc` /
//     etc. would need to go into a wordlist without the
//     `@` (see `D_KEYWORDS` docstring for why they don't).
//   - SCE_D_IDENTIFIER (14) — bare identifier fallback.
//     Framework convention: leave unmapped so plain
//     identifiers paint at STYLE_DEFAULT.
//   - SCE_D_COMMENTLINEDOC (15) — `///` or `//!` Ddoc
//     line comment.
//   - SCE_D_COMMENTDOCKEYWORD (16) — `@param` / `@return`
//     etc. inside a doc comment (COMMENTDOC or
//     COMMENTLINEDOC context).
//   - SCE_D_COMMENTDOCKEYWORDERROR (17) — malformed doc
//     tag (unknown `@name` inside a doc comment).
//   - SCE_D_STRINGB (18) — `` `...` `` backtick wysiwyg
//     string literal.
//   - SCE_D_STRINGR (19) — raw `r"..."`, hex `x"..."`, or
//     delimited `q"..."` string literal.
//   - SCE_D_WORD5 (20) — wordlist class 4 hit (special
//     values / literals — `true`, `false`, `null`,
//     `__FILE__`, etc.).
//   - SCE_D_WORD6 (21) — wordlist class 5 hit (traits /
//     meta-programming — `__traits`, `__ctfe`, etc.).
//   - SCE_D_WORD7 (22) — wordlist class 6 hit (reserved
//     for user extension — Phobos library surface if a
//     future palette wants it).
pub const SCE_D_DEFAULT: usize = 0;
pub const SCE_D_COMMENT: usize = 1;
pub const SCE_D_COMMENTLINE: usize = 2;
pub const SCE_D_COMMENTDOC: usize = 3;
pub const SCE_D_COMMENTNESTED: usize = 4;
pub const SCE_D_NUMBER: usize = 5;
pub const SCE_D_WORD: usize = 6;
pub const SCE_D_WORD2: usize = 7;
pub const SCE_D_WORD3: usize = 8;
pub const SCE_D_TYPEDEF: usize = 9;
pub const SCE_D_STRING: usize = 10;
pub const SCE_D_STRINGEOL: usize = 11;
pub const SCE_D_CHARACTER: usize = 12;
pub const SCE_D_OPERATOR: usize = 13;
pub const SCE_D_IDENTIFIER: usize = 14;
pub const SCE_D_COMMENTLINEDOC: usize = 15;
pub const SCE_D_COMMENTDOCKEYWORD: usize = 16;
pub const SCE_D_COMMENTDOCKEYWORDERROR: usize = 17;
pub const SCE_D_STRINGB: usize = 18;
pub const SCE_D_STRINGR: usize = 19;
pub const SCE_D_WORD5: usize = 20;
pub const SCE_D_WORD6: usize = 21;
pub const SCE_D_WORD7: usize = 22;

// LexPowerShell style indices. 17 contiguous slots (0..=16)
// for the PowerShell scripting language (Windows PowerShell
// 5.1 + PowerShell 6+ / Core; the Lexilla lexer doesn't
// distinguish editions). Constants mirror
// `SciLexer.h:1452-1468` verbatim. Dispatches
// SCLEX_POWERSHELL (= 88, per `SciLexer.h:104`) via a
// **six-class wordlist** declared at
// `vendor\lexilla\lexers\LexPowerShell.cxx:283-291`
// (`powershellWordLists[]`):
//
//     powershellWordLists[] = {
//         "Commands",    // class 0 → SCE_POWERSHELL_KEYWORD
//         "Cmdlets",     // class 1 → SCE_POWERSHELL_CMDLET
//         "Aliases",     // class 2 → SCE_POWERSHELL_ALIAS
//         "Functions",   // class 3 → SCE_POWERSHELL_FUNCTION
//         "User1",       // class 4 → SCE_POWERSHELL_USER1
//         "DocComment",  // class 5 → SCE_POWERSHELL_COMMENTDOCKEYWORD
//         0,
//     };
//
// **Case-insensitive matching.** LexPowerShell has no
// `caseSensitive` factory switch — the identifier
// classification cascade at `LexPowerShell.cxx:154-172` calls
// `sc.GetCurrentLowered(s, sizeof(s))` unconditionally before
// every `WordList::InList` probe. PowerShell is documented as
// a case-insensitive language (Microsoft Learn
// `about_Language_Keywords`, `about_Comparison_Operators`),
// so wordlists MUST be all-lowercase. An uppercase entry
// would never match — inverted from `D_KEYWORDS`, matches
// `COBOL_KEYWORDS_A`'s case-fold discipline.
//
// **CommentDocKeyword — leading-dot sigil stripped.**
// `SCE_POWERSHELL_COMMENTDOCKEYWORD` (16) is entered from
// `SCE_POWERSHELL_COMMENTSTREAM` on `.` + word character at
// `LexPowerShell.cxx:96-98`. Wordlist class 5 (`DocComment`)
// is probed with `keywords6.InList(s + 1)` at `:107` — the
// `+ 1` skips the leading `.`, so wordlist entries must be
// BARE tag names WITHOUT `.` (`SYNOPSIS`, not `.SYNOPSIS`).
// Invalid tags fall back to `SCE_POWERSHELL_COMMENTSTREAM`
// via `ChangeState` at `:108`.
//
// **Wordlist install map** (per the state machine at
// `LexPowerShell.cxx:154-172`):
//
//     class 0 → keywords  → SCE_POWERSHELL_KEYWORD             (language keywords)
//     class 1 → keywords2 → SCE_POWERSHELL_CMDLET              (Verb-Noun cmdlets)
//     class 2 → keywords3 → SCE_POWERSHELL_ALIAS               (built-in aliases)
//     class 3 → keywords4 → SCE_POWERSHELL_FUNCTION            (well-known functions)
//     class 4 → keywords5 → SCE_POWERSHELL_USER1               (user extension)
//     class 5 → keywords6 → SCE_POWERSHELL_COMMENTDOCKEYWORD   (comment-based help tags)
//
// **Two string flavors + two here-string flavors.**
// PowerShell distinguishes double-quoted (`"..."` — expands
// variables and escape sequences) from single-quoted
// (`'...'` — literal) strings, and provides `@"..."@` /
// `@'...'@` here-string variants that span multiple lines.
// LexPowerShell tokenises them as four distinct states:
//   - `SCE_POWERSHELL_STRING` (2) — `"..."` (`:112-118`,
//     entered at `:180`).
//   - `SCE_POWERSHELL_CHARACTER` (3) — `'...'` (`:119-123`,
//     entered at `:182`).
//   - `SCE_POWERSHELL_HERE_STRING` (14) — `@"..."@`
//     (`:124-129`, entered at `:184`).
//   - `SCE_POWERSHELL_HERE_CHARACTER` (15) — `@'...'@`
//     (`:130-135`, entered at `:186`).
//
// All four collapse to `StyleSlot::String` at the theme
// level for uniform visual identity.
//
// **Backtick continuation.** In `SCE_POWERSHELL_STRING`
// (double-quoted), backtick `\`` is the PowerShell escape
// character — `sc.Forward()` at `:117` skips the next
// character so `` `" `` doesn't close the string. Outside
// strings at `:196-198`, a bare backtick at the DEFAULT
// state also consumes the next character (line-continuation
// role).
//
// **Numeric literal recognition — cross-line state.**
// `SCE_POWERSHELL_NUMBER` (4) accepts hex (`0x...`),
// decimals, exponents, sign after `e`, and a curated suffix
// set (`g`/`k`/`l`/`m`/`n`/`p`/`s`/`t`/`u`/`y`) at
// `IsNumericLiteral()` (`:36-69`). Entry at `:190-191`
// includes the leading-dot fractional case
// (`.5` when `chPrev != '.'`).
//
// **`#region`/`#endregion` folding.** `FoldPowerShellDoc`
// at `:247-259` walks `SCE_POWERSHELL_COMMENT` looking for
// `#region` / `#endregion` markers to open/close fold
// levels. Pure lexer concern — no host configuration.
// `<# ... #>` stream comments fold via a separate branch
// at `:241-246`.
//
// Style semantics (paint-loop citations reference
// LexPowerShell.cxx):
//
//   - SCE_POWERSHELL_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_POWERSHELL_COMMENT (1) — `#`-to-EOL line comment.
//   - SCE_POWERSHELL_STRING (2) — `"..."` double-quoted
//     literal.
//   - SCE_POWERSHELL_CHARACTER (3) — `'...'` single-quoted
//     literal.
//   - SCE_POWERSHELL_NUMBER (4) — numeric literal.
//   - SCE_POWERSHELL_VARIABLE (5) — `$name` variable
//     reference. Entered at `:188` on `$`, extends through
//     `IsAWordChar` at `:146-149`.
//   - SCE_POWERSHELL_OPERATOR (6) — punctuation classified
//     via `isoperator()` at `:150-153` and `:192-193`.
//   - SCE_POWERSHELL_IDENTIFIER (7) — bare identifier
//     fallback. Framework convention: leave unmapped so
//     plain identifiers paint at STYLE_DEFAULT.
//   - SCE_POWERSHELL_KEYWORD (8) — wordlist class 0 hit
//     (language keywords).
//   - SCE_POWERSHELL_CMDLET (9) — wordlist class 1 hit
//     (Verb-Noun cmdlets).
//   - SCE_POWERSHELL_ALIAS (10) — wordlist class 2 hit
//     (built-in aliases).
//   - SCE_POWERSHELL_FUNCTION (11) — wordlist class 3 hit
//     (well-known functions).
//   - SCE_POWERSHELL_USER1 (12) — wordlist class 4 hit
//     (reserved user-extension slot).
//   - SCE_POWERSHELL_COMMENTSTREAM (13) — `<# ... #>`
//     stream / block comment.
//   - SCE_POWERSHELL_HERE_STRING (14) — `@"..."@`
//     double-quoted here-string.
//   - SCE_POWERSHELL_HERE_CHARACTER (15) — `@'...'@`
//     single-quoted here-string.
//   - SCE_POWERSHELL_COMMENTDOCKEYWORD (16) — `.SYNOPSIS` /
//     `.DESCRIPTION` etc. inside a `<# ... #>` stream
//     comment. Comment-based help tag.
pub const SCE_POWERSHELL_DEFAULT: usize = 0;
pub const SCE_POWERSHELL_COMMENT: usize = 1;
pub const SCE_POWERSHELL_STRING: usize = 2;
pub const SCE_POWERSHELL_CHARACTER: usize = 3;
pub const SCE_POWERSHELL_NUMBER: usize = 4;
pub const SCE_POWERSHELL_VARIABLE: usize = 5;
pub const SCE_POWERSHELL_OPERATOR: usize = 6;
pub const SCE_POWERSHELL_IDENTIFIER: usize = 7;
pub const SCE_POWERSHELL_KEYWORD: usize = 8;
pub const SCE_POWERSHELL_CMDLET: usize = 9;
pub const SCE_POWERSHELL_ALIAS: usize = 10;
pub const SCE_POWERSHELL_FUNCTION: usize = 11;
pub const SCE_POWERSHELL_USER1: usize = 12;
pub const SCE_POWERSHELL_COMMENTSTREAM: usize = 13;
pub const SCE_POWERSHELL_HERE_STRING: usize = 14;
pub const SCE_POWERSHELL_HERE_CHARACTER: usize = 15;
pub const SCE_POWERSHELL_COMMENTDOCKEYWORD: usize = 16;

// LexR style indices. 16 contiguous slots (0..=15) for the R
// statistical programming language (also handles S, S-PLUS,
// per `LexR.cxx:1-6`). Constants mirror `SciLexer.h:1419-1434`
// verbatim. Dispatches SCLEX_R (= 86, per `SciLexer.h:102`)
// via a **three-class wordlist** declared at
// `vendor\lexilla\lexers\LexR.cxx:339-346` (`RWordLists[]`).
// The descriptor declares five slots, but the last two are
// literally labelled "Unused" (classes 3 and 4) — the paint
// loop at `:146-159` only probes wordlists 0/1/2:
//
//     RWordLists[] = {
//         "Language Keywords",              // class 0 → SCE_R_KWORD
//         "Base / Default package function", // class 1 → SCE_R_BASEKWORD
//         "Other Package Functions",        // class 2 → SCE_R_OTHERKWORD
//         "Unused",                          // class 3 — never probed
//         "Unused",                          // class 4 — never probed
//         nullptr,
//     };
//
// **Case-SENSITIVE matching.** The identifier classification
// cascade at `LexR.cxx:146-158` calls
// `sc.GetCurrent(s, sizeof(s))` (byte-exact), NOT
// `GetCurrentLowered`. R is a case-sensitive language at the
// spec level (`TRUE` != `true`, `NULL` != `null` — actually
// `NULL` and `TRUE` are the canonical spellings; `null` /
// `true` are just user-defined identifiers), so wordlists
// use exact-case spellings. Same discipline as `D_KEYWORDS`,
// inverted from `POWERSHELL_KEYWORDS` / `COBOL_KEYWORDS_A`.
//
// **`.` is a word char but NOT a word start.** `IsAWordChar`
// at `LexR.cxx:30-32` accepts `[0-9A-Za-z._]`, but
// `IsAWordStart` at `:34-36` accepts only `[0-9A-Za-z_]`.
// Consequence: R identifiers like `is.numeric` or
// `data.frame` tokenise as ONE identifier (the `.` extends
// the word), so wordlist entries CAN contain internal `.`
// characters — this is essential for the base-package
// functions where `.`-separated names are the convention
// (`is.na`, `is.null`, `data.frame`, `as.numeric`, etc.).
//
// **Number literals.** `SCE_R_NUMBER` (5) recognises decimal,
// hex (`0x`), scientific (`e±` / `p±`), and R-specific
// suffixes `L` (integer) and `i` (imaginary/complex) per
// `LexR.cxx:134-144` and R Language Reference
// §"Literal constants".
//
// **Raw string literals — R 4.0+ `r"(...)"` syntax.**
// R 4.0.0 (April 2020) added raw strings with three
// delimiter families: `r"(...)"`, `r"[...]"`, `r"{...}"`,
// plus optional dash decorations for nested quotes like
// `r"-(...)-"`. Both `r"..."` and `r'...'` variants are
// supported. Two states:
//   - `SCE_R_RAWSTRING` (13) — `r"..."` double-quoted raw.
//   - `SCE_R_RAWSTRING2` (14) — `r'...'` single-quoted raw.
// Dash count + matching-delimiter state persist per line
// via `styler.SetLineState` at `:271-274`; parsed by
// `CheckRawString` at `:84-103`.
//
// **Backticked identifiers.** `SCE_R_BACKTICKS` (12) covers
// R's non-standard names — anything between `` ` `` marks,
// used for reserved-word-like identifiers (`` `if` `` as a
// variable, column names with spaces, etc.). Same visual
// slot as `SCE_R_STRING` at the theme level.
//
// **Infix operators.** `SCE_R_INFIX` (10) covers R's
// user-definable infix operators like `%%` (modulo), `%in%`
// (membership test), `%*%` (matrix multiplication), `%o%`
// (outer product). Entered at `:260-261` on `%`, exits on
// closing `%` at `:222-223`. `SCE_R_INFIXEOL` (11) is the
// error state when the infix operator hits EOL without
// closing — visual slot: `Operator`.
//
// **`\\uHHHH` / `\\UHHHHHHHH` / `\\xHH` escape sequences.**
// `SCE_R_ESCAPESEQUENCE` (15) is opt-in via the property
// `lexer.r.escape.sequence` (default `0` = off). When
// enabled, `\\x`/`\\u`/`\\U` sequences inside strings render
// distinctly. Entry logic at `:170-182`; `atEscapeEnd`
// counter at `:78-81`. The host does not enable this
// property today; the style constant is defined for future
// use.
//
// Style semantics (paint-loop citations reference LexR.cxx):
//
//   - SCE_R_DEFAULT (0) — whitespace / unclassified.
//     Framework convention: leave unmapped.
//   - SCE_R_COMMENT (1) — `#`-to-EOL line comment.
//   - SCE_R_KWORD (2) — wordlist class 0 hit (R language
//     keywords per spec §"Reserved words").
//   - SCE_R_BASEKWORD (3) — wordlist class 1 hit (base
//     package functions — `c`, `list`, `mean`, `sum`,
//     `length`, etc.).
//   - SCE_R_OTHERKWORD (4) — wordlist class 2 hit (other
//     package functions — reserved for `stats`, `utils`,
//     `graphics`, etc.).
//   - SCE_R_NUMBER (5) — numeric literal.
//   - SCE_R_STRING (6) — `"..."` double-quoted literal.
//   - SCE_R_STRING2 (7) — `'...'` single-quoted literal.
//   - SCE_R_OPERATOR (8) — punctuation per
//     `IsAnOperator()` at `:38-48` (`+`/`-`/`*`/`/`/`^`/
//     `<`/`>`/`=`/`&`/`|`/`$`/`(`/`)`/`{`/`}`/`[`/`]`/`!`/
//     `~`/`?`/`:`). Deliberately EXCLUDES `.` (used in
//     numbers).
//   - SCE_R_IDENTIFIER (9) — bare identifier fallback.
//     Framework convention: leave unmapped.
//   - SCE_R_INFIX (10) — `%...%` user-defined infix
//     operator body.
//   - SCE_R_INFIXEOL (11) — unterminated `%` reached EOL.
//   - SCE_R_BACKTICKS (12) — `` `name` `` backticked
//     non-standard name.
//   - SCE_R_RAWSTRING (13) — R 4.0+ `r"(...)"` /
//     `r"[...]"` / `r"{...}"` raw string, double-quoted.
//   - SCE_R_RAWSTRING2 (14) — R 4.0+ `r'(...)'` /
//     `r'[...]'` / `r'{...}'` raw string, single-quoted.
//   - SCE_R_ESCAPESEQUENCE (15) — `\\x` / `\\u` / `\\U`
//     escape sequence inside a string, only emitted when
//     `lexer.r.escape.sequence` = 1.
pub const SCE_R_DEFAULT: usize = 0;
pub const SCE_R_COMMENT: usize = 1;
pub const SCE_R_KWORD: usize = 2;
pub const SCE_R_BASEKWORD: usize = 3;
pub const SCE_R_OTHERKWORD: usize = 4;
pub const SCE_R_NUMBER: usize = 5;
pub const SCE_R_STRING: usize = 6;
pub const SCE_R_STRING2: usize = 7;
pub const SCE_R_OPERATOR: usize = 8;
pub const SCE_R_IDENTIFIER: usize = 9;
pub const SCE_R_INFIX: usize = 10;
pub const SCE_R_INFIXEOL: usize = 11;
pub const SCE_R_BACKTICKS: usize = 12;
pub const SCE_R_RAWSTRING: usize = 13;
pub const SCE_R_RAWSTRING2: usize = 14;
pub const SCE_R_ESCAPESEQUENCE: usize = 15;

// LexCoffeeScript style indices. 26 contiguous slots (0..=25)
// for CoffeeScript source (`.coffee`, `.litcoffee`), a Ruby-ish
// indentation-scoped language that compiles to JavaScript.
// Constants mirror `SciLexer.h:1651-1676` verbatim. Dispatches
// SCLEX_COFFEESCRIPT (= 102, per `SciLexer.h:118`) via a
// **three-class wordlist** declared at
// `vendor\lexilla\lexers\LexCoffeeScript.cxx:486-492`
// (`csWordLists[]`). The descriptor declares four slots but the
// third (class 2) is literally labelled "Unused" — the paint
// loop at `:117-119` only assigns `keywordlists[0]`,
// `keywordlists[1]`, and `keywordlists[3]`; the identifier
// classification cascade at `:195-203` probes classes 0/1/3
// (skipping 2):
//
//     csWordLists[] = {
//         "Keywords",           // class 0 → SCE_COFFEESCRIPT_WORD
//         "Secondary keywords", // class 1 → SCE_COFFEESCRIPT_WORD2
//         "Unused",             // class 2 — never probed
//         "Global classes",     // class 3 → SCE_COFFEESCRIPT_GLOBALCLASS
//         0,
//     };
//
// **Case-SENSITIVE matching.** The identifier-classification
// cascade at `LexCoffeeScript.cxx:193-203` calls
// `sc.GetCurrent(s, sizeof(s))` (byte-exact), NOT
// `GetCurrentLowered`. CoffeeScript source is case-sensitive at
// the spec level (per the upstream lexer source at
// `github.com/jashkenas/coffeescript/blob/master/src/lexer.coffee`
// lines 1366-1400), so wordlist tokens use exact case. Same
// discipline as `D_KEYWORDS` / `R_RESERVED`, inverted from
// `POWERSHELL_KEYWORDS`.
//
// **Identifier alphabet with special starters.**
// `setWordStart` at `LexCoffeeScript.cxx:124` accepts letters,
// `_`, `$`, and — uniquely — `@`. This is the syntactic
// signature of CoffeeScript: `@foo` is shorthand for
// `this.foo`, and the leading `@` starts an identifier. The
// identifier-classifier at `:200-202` then detects the `@`
// prefix and re-styles the token as `INSTANCEPROPERTY` (25).
// `setWord` at `:125` extends to `.` and `$` beyond
// `[A-Za-z0-9_]`, but the state-exit at `:192` splits on `.`
// so `a.b` tokenises as three tokens — the `.` is a mid-word
// character only for the number-lexer's benefit (hex/decimal
// suffixes), not for identifier extension.
//
// **String interpolation (`"…#{expr}…"`).** Ruby-style
// `#{...}` interpolation inside double-quoted strings —
// implementation borrowed from LexRuby at `:46-73`, driven by
// stack-based tracking of up to 5 levels
// (`INNER_STRINGS_MAX_COUNT = 5` at `:139`). Enter at `:227` on
// `#{`, temporarily paints `#{` as `OPERATOR`, then the
// expression tokenises as normal CoffeeScript until the
// matching `}` at `:329-335` restores the string state. This
// means keywords / identifiers / numbers INSIDE
// interpolation get their normal styles — expected behaviour.
// Single-quoted strings (`CHARACTER`) at `:238-246` do NOT
// interpolate — no `#{` state transition — matching CoffeeScript
// language semantics.
//
// **Two regex flavours.**
//   - `SCE_COFFEESCRIPT_REGEX` (14) — inline `/pattern/flags`
//     regex, entered at `:304-309` after operators or
//     keywords, exited at `:250-254` on the trailing `/`
//     followed by lowercase-only flag gobbling.
//   - `SCE_COFFEESCRIPT_VERBOSE_REGEX` (23) — block regex
//     `///pattern///`, entered at `:300-303`, exited at
//     `:277-280` on `///`. Inside a verbose regex, a `#`
//     starts a `SCE_COFFEESCRIPT_VERBOSE_REGEX_COMMENT` (24)
//     that runs to line end per `:287-291`.
//
// **Two comment flavours.**
//   - `SCE_COFFEESCRIPT_COMMENTLINE` (2) — `#` to end of line
//     at `:314-321`. NOT the block-comment token.
//   - `SCE_COFFEESCRIPT_COMMENTBLOCK` (22) — `###` ... `###`
//     block comment, entered at `:315-318`, exited at
//     `:267-274` on the closing `###`.
//
// **Enum slots that this lexer never emits.** 11 slots
// defined in `SciLexer.h` are never reached by any
// `sc.SetState` / `sc.ChangeState` call in
// `ColouriseCoffeeScriptDoc`:
//
//   - 10 LexCPP-inherited slots that share numbering with the
//     C++ lexer: `COMMENT` (1), `COMMENTDOC` (3), `UUID` (8),
//     `PREPROCESSOR` (9), `VERBATIM` (13), `COMMENTLINEDOC`
//     (15), `COMMENTDOCKEYWORD` (17),
//     `COMMENTDOCKEYWORDERROR` (18), `STRINGRAW` (20),
//     `TRIPLEVERBATIM` (21).
//   - `STRINGEOL` (12) — an **orphan case label** at
//     `LexCoffeeScript.cxx:262-266`. The switch branch
//     handles what to do WHILE in the state (reset to
//     `DEFAULT` on `atLineStart`), but no code path
//     anywhere in the file ever sets the state; grep
//     across the vendored tree confirms zero
//     `SetState(SCE_COFFEESCRIPT_STRINGEOL)` /
//     `ChangeState(SCE_COFFEESCRIPT_STRINGEOL)` calls.
//     Unterminated strings simply fall off the STRING
//     state at line end via the standard state-machine
//     reset path — they don't get a distinctive error
//     style. The theme in `crates/ui_win32/src/lib.rs`
//     deliberately leaves this slot unmapped for that
//     reason.
//
// The switch table at `:181-292` and the state-entry
// cascade at `:295-337` never call `sc.SetState` on any of
// the 11.
//
// Style semantics (paint-loop citations reference
// LexCoffeeScript.cxx):
//
//   - SCE_COFFEESCRIPT_DEFAULT (0) — whitespace /
//     unclassified. Framework convention: leave unmapped so
//     bare punctuation surrounded by whitespace paints at
//     STYLE_DEFAULT.
//   - SCE_COFFEESCRIPT_COMMENT (1) — never entered (LexCPP-
//     inherited slot).
//   - SCE_COFFEESCRIPT_COMMENTLINE (2) — `#` line comment.
//   - SCE_COFFEESCRIPT_COMMENTDOC (3) — never entered.
//   - SCE_COFFEESCRIPT_NUMBER (4) — numeric literal.
//   - SCE_COFFEESCRIPT_WORD (5) — wordlist class 0 hit
//     (primary keywords — control flow, declarations,
//     exception handling, `this`, `debugger`, `await`,
//     `yield`).
//   - SCE_COFFEESCRIPT_STRING (6) — `"..."` double-quoted
//     string. Supports `#{...}` interpolation.
//   - SCE_COFFEESCRIPT_CHARACTER (7) — `'...'` single-quoted
//     string. NO interpolation (spec).
//   - SCE_COFFEESCRIPT_UUID (8) — never entered.
//   - SCE_COFFEESCRIPT_PREPROCESSOR (9) — never entered.
//   - SCE_COFFEESCRIPT_OPERATOR (10) — punctuation per
//     `isoperator()` at `:322-337`; also transient state on
//     `#{` interpolation delimiters at `:234-235`.
//   - SCE_COFFEESCRIPT_IDENTIFIER (11) — bare identifier
//     fallback. Framework convention: leave unmapped so
//     plain identifiers paint at STYLE_DEFAULT.
//   - SCE_COFFEESCRIPT_STRINGEOL (12) — orphan case label
//     at `:262-266` with no `sc.SetState` / `sc.ChangeState`
//     anywhere. Never entered — see the "Enum slots that
//     this lexer never emits" section above.
//   - SCE_COFFEESCRIPT_VERBATIM (13) — never entered.
//   - SCE_COFFEESCRIPT_REGEX (14) — `/pattern/flags` regex.
//   - SCE_COFFEESCRIPT_COMMENTLINEDOC (15) — never entered.
//   - SCE_COFFEESCRIPT_WORD2 (16) — wordlist class 1 hit
//     (secondary keywords — word-form operators, boolean-
//     alias literals, module-syntax words).
//   - SCE_COFFEESCRIPT_COMMENTDOCKEYWORD (17) — never entered.
//   - SCE_COFFEESCRIPT_COMMENTDOCKEYWORDERROR (18) — never
//     entered.
//   - SCE_COFFEESCRIPT_GLOBALCLASS (19) — wordlist class 3
//     hit (JS/Node global classes — `Array`, `Object`,
//     `Math`, etc.).
//   - SCE_COFFEESCRIPT_STRINGRAW (20) — never entered.
//   - SCE_COFFEESCRIPT_TRIPLEVERBATIM (21) — never entered.
//   - SCE_COFFEESCRIPT_COMMENTBLOCK (22) — `###...###` block
//     comment.
//   - SCE_COFFEESCRIPT_VERBOSE_REGEX (23) — `///pattern///`
//     block regex.
//   - SCE_COFFEESCRIPT_VERBOSE_REGEX_COMMENT (24) — `#`
//     comment inside a verbose regex block, runs to line
//     end.
//   - SCE_COFFEESCRIPT_INSTANCEPROPERTY (25) — identifier
//     starting with `@` (CoffeeScript's `this.` shorthand:
//     `@name` == `this.name`). Detected at `:200-202`.
pub const SCE_COFFEESCRIPT_DEFAULT: usize = 0;
pub const SCE_COFFEESCRIPT_COMMENT: usize = 1;
pub const SCE_COFFEESCRIPT_COMMENTLINE: usize = 2;
pub const SCE_COFFEESCRIPT_COMMENTDOC: usize = 3;
pub const SCE_COFFEESCRIPT_NUMBER: usize = 4;
pub const SCE_COFFEESCRIPT_WORD: usize = 5;
pub const SCE_COFFEESCRIPT_STRING: usize = 6;
pub const SCE_COFFEESCRIPT_CHARACTER: usize = 7;
pub const SCE_COFFEESCRIPT_UUID: usize = 8;
pub const SCE_COFFEESCRIPT_PREPROCESSOR: usize = 9;
pub const SCE_COFFEESCRIPT_OPERATOR: usize = 10;
pub const SCE_COFFEESCRIPT_IDENTIFIER: usize = 11;
pub const SCE_COFFEESCRIPT_STRINGEOL: usize = 12;
pub const SCE_COFFEESCRIPT_VERBATIM: usize = 13;
pub const SCE_COFFEESCRIPT_REGEX: usize = 14;
pub const SCE_COFFEESCRIPT_COMMENTLINEDOC: usize = 15;
pub const SCE_COFFEESCRIPT_WORD2: usize = 16;
pub const SCE_COFFEESCRIPT_COMMENTDOCKEYWORD: usize = 17;
pub const SCE_COFFEESCRIPT_COMMENTDOCKEYWORDERROR: usize = 18;
pub const SCE_COFFEESCRIPT_GLOBALCLASS: usize = 19;
pub const SCE_COFFEESCRIPT_STRINGRAW: usize = 20;
pub const SCE_COFFEESCRIPT_TRIPLEVERBATIM: usize = 21;
pub const SCE_COFFEESCRIPT_COMMENTBLOCK: usize = 22;
pub const SCE_COFFEESCRIPT_VERBOSE_REGEX: usize = 23;
pub const SCE_COFFEESCRIPT_VERBOSE_REGEX_COMMENT: usize = 24;
pub const SCE_COFFEESCRIPT_INSTANCEPROPERTY: usize = 25;

// LexCSS style indices. 24 contiguous slots (0..=23) covering CSS
// selectors (tag / class / id / attribute / pseudo-class / pseudo-
// element), CSS1 / CSS2 / CSS3 property names via a four-way
// IDENTIFIER cascade, at-rule directives (`@import` / `@media` /
// `@font-face` / ...), `!important`, single / double-quoted strings,
// `/* ... */` block comments, operators, and SCSS-style `$name` /
// Less-style `@name` variables. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 779-802 and
// `vendor/lexilla/lexers/LexCSS.cxx` lines 558-568 (`cssWordListDesc`
// array) + lines 78-86 (wordlist-pointer extraction) + line 419
// (case-insensitive token matching) + lines 425-438 (four-way
// IDENTIFIER cascade) + lines 440-454 (separate pseudo-class /
// pseudo-element cascade).
//
// **Case-insensitive lexer.** `LexCSS.cxx:419` calls
// `sc.GetCurrentLowered(s, ...)` on every candidate token before
// any `WordList::InList` lookup. Wordlists installed against this
// lexer MUST be all-lowercase — uppercase entries would never
// match. Same shape contract as LexBatch / LexSQL / LexVB /
// LexPascal.
//
// **Eight wordlist classes (0..=7).** Per `cssWordListDesc[]`:
// 0 = CSS1 properties, 1 = standard pseudo-classes, 2 = CSS2
// properties (extension of class 0), 3 = CSS3 properties
// (extension of classes 0 + 2), 4 = standard pseudo-elements,
// 5 = extended/vendor-prefixed properties, 6 = extended
// pseudo-classes, 7 = extended pseudo-elements. Code++ populates
// classes 0 + 1 + 2 + 3 + 4 for v1; classes 5 + 6 + 7 are reserved
// for future vendor-prefix wordlists (current cascade-miss
// behaviour is documented under `SCE_CSS_UNKNOWN_*` below).
//
// **Four-way IDENTIFIER cascade** (`LexCSS.cxx:425-438` —
// property-name arm only; pseudo-class / pseudo-element have a
// separate cascade at lines 440-454). The IDENTIFIER cascade
// consults the property-name wordlists in priority order: class 0
// hit → `SCE_CSS_IDENTIFIER`, class 2 hit → `SCE_CSS_IDENTIFIER2`,
// class 3 hit → `SCE_CSS_IDENTIFIER3`, class 5 hit →
// `SCE_CSS_EXTENDED_IDENTIFIER`, else → `SCE_CSS_UNKNOWN_IDENTIFIER`.
// The host themes 6 / 15 / 17 / 19 identically (Keyword bold) so
// property-name colour is consistent regardless of which spec
// generation a property comes from — distinct lexer-side indices
// exist for plugins that want to differentiate generations, not
// because they should render differently by default.
//
// **`SCE_CSS_UNKNOWN_PSEUDOCLASS` (4) and `SCE_CSS_UNKNOWN_IDENTIFIER`
// (7) are wordlist-miss fallbacks, NOT error states.** Both are
// emitted when a syntactically-valid token doesn't match any
// installed wordlist (e.g. a vendor-prefixed `-webkit-foo` while
// class 5 is empty, or a CSS custom property `--foo` — see VARIABLE
// gap below). Code++ leaves both unmapped so they fall through to
// STYLE_DEFAULT and render at the user's default foreground —
// matches N++ light-theme behaviour and is consistent with how the
// framework treats other "no match" tokens (e.g. `SCE_C_IDENTIFIER`).
// Distinct from STRINGEOL-family error indicators which are pending
// the future `StyleSlot::Error` palette addition.
//
// **`SCE_CSS_GROUP_RULE` (22) is hard-coded for exactly four
// at-rules.** `LexCSS.cxx:460-463` `strcmp`s against `"media"` /
// `"supports"` / `"document"` / `"-moz-document"` and post-hoc
// upgrades from `SCE_CSS_DIRECTIVE` to `SCE_CSS_GROUP_RULE`. Every
// other at-rule (`@import`, `@charset`, `@keyframes`, `@font-face`,
// `@page`, `@namespace`, `@layer`, `@container`, `@property`, ...)
// stays as `SCE_CSS_DIRECTIVE`. The host themes 12 + 22 identically
// (Preprocessor bold) so the visual is uniform N++-parity; no
// wordlist exists for GROUP_RULE and the list cannot be extended
// without patching the lexer.
//
// **`SCE_CSS_VARIABLE` (23) is SCSS `$name` / Less `@name` ONLY —
// NOT CSS custom properties.** CSS custom properties (`--foo: red;`)
// tokenise through the IDENTIFIER cascade, miss every wordlist, and
// land in `SCE_CSS_UNKNOWN_IDENTIFIER` (style 7 → unmapped →
// STYLE_DEFAULT). `SCE_CSS_VARIABLE` only activates when
// `lexer.css.scss.language` / `lexer.css.less.language` /
// `lexer.css.hss.language` is set on the lexer instance. Code++
// doesn't set those for the `L_CSS` row (separate menu entries for
// SCSS / Less would route to dedicated rows). The host still maps
// 23 → Attribute so a future SCSS / Less wiring picks up sensible
// colouring with no theme edit.
//
// **`SCE_CSS_DEFAULT` (0) and `SCE_CSS_VALUE` (8) are intentionally
// unmapped.** `_DEFAULT` is the inherit-from-`STYLE_DEFAULT`
// fallback; `_VALUE` is the right-of-colon literal text
// (`color: RED` — the `RED` is VALUE), which N++ light theme leaves
// at the user's default foreground. Same omission pattern as
// `SCE_C_DEFAULT` / `SCE_PAS_DEFAULT`.
pub const SCE_CSS_DEFAULT: usize = 0;
pub const SCE_CSS_TAG: usize = 1;
pub const SCE_CSS_CLASS: usize = 2;
pub const SCE_CSS_PSEUDOCLASS: usize = 3;
pub const SCE_CSS_UNKNOWN_PSEUDOCLASS: usize = 4;
pub const SCE_CSS_OPERATOR: usize = 5;
pub const SCE_CSS_IDENTIFIER: usize = 6;
pub const SCE_CSS_UNKNOWN_IDENTIFIER: usize = 7;
pub const SCE_CSS_VALUE: usize = 8;
pub const SCE_CSS_COMMENT: usize = 9;
pub const SCE_CSS_ID: usize = 10;
pub const SCE_CSS_IMPORTANT: usize = 11;
pub const SCE_CSS_DIRECTIVE: usize = 12;
pub const SCE_CSS_DOUBLESTRING: usize = 13;
pub const SCE_CSS_SINGLESTRING: usize = 14;
pub const SCE_CSS_IDENTIFIER2: usize = 15;
pub const SCE_CSS_ATTRIBUTE: usize = 16;
pub const SCE_CSS_IDENTIFIER3: usize = 17;
pub const SCE_CSS_PSEUDOELEMENT: usize = 18;
pub const SCE_CSS_EXTENDED_IDENTIFIER: usize = 19;
pub const SCE_CSS_EXTENDED_PSEUDOCLASS: usize = 20;
pub const SCE_CSS_EXTENDED_PSEUDOELEMENT: usize = 21;
pub const SCE_CSS_GROUP_RULE: usize = 22;
pub const SCE_CSS_VARIABLE: usize = 23;

// LexPerl style indices. Sparse range — 0..=31 contiguous, then a
// jump to 40..=44 (sub prototype / format / interpolation base for
// STRING_VAR / XLAT), and a second jump to a 54..=66 interpolation-
// shadow band (variable-interpolation styles for the regex / heredoc
// / q-family states). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 380-424 and
// `vendor/lexilla/lexers/LexPerl.cxx` lines 394-397 (`perlWordListDesc`)
// + lines 96-104 (`isPerlKeyword` byte-exact wordlist matcher) +
// line 94 (`INTERPOLATE_SHIFT` = 37 — defines the _VAR shadow band).
//
// **Case-sensitive lexer.** `LexPerl.cxx:96-104` copies token bytes
// verbatim into a stack buffer and calls `keywords.InList(s)` with
// no case folding. Wordlists installed against this lexer must use
// the exact casing source uses. For Perl this matters specifically
// for two families: the phase-block names (`BEGIN` / `END` / `INIT`
// / `CHECK` / `UNITCHECK` / `AUTOLOAD` / `DESTROY`) and the
// `__TOKEN__` literals (`__FILE__` / `__LINE__` / `__PACKAGE__` /
// `__SUB__` / `__DATA__` / `__END__`) — Perl source writes these
// uppercase by language requirement, so the wordlist MUST store the
// uppercase form. Storing them lowercase silently disables the
// highlight. Same byte-exact contract as LexCPP / LexRust (most
// lexers, in fact — case-folding is the exception, used by LexCSS /
// LexSQL / LexPascal / LexVB / LexBatch).
//
// **Single wordlist class.** `perlWordListDesc[]` declares one slot
// (`"Keywords"`). All Perl built-ins + reserved words + named
// operators (`x` / `cmp` / `lt` / `gt` / `le` / `ge` / `eq` / `ne`
// / `and` / `or` / `not` / `xor`) + quote-like operator names
// (`m` / `s` / `y` / `q` / `qq` / `qx` / `qr` / `qw` / `tr`) install
// to class 0. The quote-like operator names ARE in the wordlist
// even though their bodies tokenise via dedicated states — the
// lexer's state-machine transitions on `m{` / `s/` / `q(` consume
// the body before keyword classification runs, so listing the
// operator name itself is harmless and matches Notepad++'s shipped
// list.
//
// **`SCE_PL_*_VAR` interpolation shadows** (the 43 / 54-66 band).
// `LexPerl.cxx:94` defines `INTERPOLATE_SHIFT = SCE_PL_STRING_VAR -
// SCE_PL_STRING = 43 - 6 = 37`. Every state whose body interpolates
// `$var` / `@var` references gets a `+37` shadow state for the
// variable token: STRING (6) → STRING_VAR (43), REGEX (17) →
// REGEX_VAR (54), REGSUBST (18) → REGSUBST_VAR (55), BACKTICKS (20)
// → BACKTICKS_VAR (57), HERE_QQ (24) → HERE_QQ_VAR (61), HERE_QX
// (25) → HERE_QX_VAR (62), STRING_QQ (27) → STRING_QQ_VAR (64),
// STRING_QX (28) → STRING_QX_VAR (65), STRING_QR (29) →
// STRING_QR_VAR (66). The shift is regular but the band is sparse —
// non-interpolating base states (CHARACTER (7) / PUNCTUATION (8) /
// PREPROCESSOR (9) / OPERATOR (10) / IDENTIFIER (11) / SCALAR (12)
// / ARRAY (13) / HASH (14) / SYMBOLTABLE (15) / VARIABLE_INDEXER
// (16) / LONGQUOTE (19) / DATASECTION (21) / HERE_DELIM (22) /
// HERE_Q (23) / STRING_Q (26) / STRING_QW (30)) leave their +37
// slots unused (45-53, 56, 58-60, 63, 67 — slot 44 is
// `SCE_PL_XLAT` for `tr///` / `y///` transliteration bodies, which
// IS used and is NOT part of the interpolation-shadow band).
// Code++ routes every populated _VAR slot to `StyleSlot::Lifetime`
// — the "purple sigil-tagged identifier" archetype Perl variables
// share with Rust lifetimes.
//
// **Reserved-but-unused style indices** (per LexPerl.cxx:433-444
// `LexicalClass[]` annotations — these are declared in SciLexer.h
// but the lexer never emits them):
//   * 8 PUNCTUATION — "currently not used"; punctuation bytes flow
//     to SCE_PL_OPERATOR (10) instead.
//   * 9 PREPROCESSOR — "preprocessor unused"; Perl has no real
//     preprocessor (the `use` / `no` pragmas tokenise as keywords).
//     Shebang `#!` lines style as COMMENTLINE (2).
//   * 16 VARIABLE_INDEXER — "allocated but unused"; sigil-with-
//     subscript context (`$foo[`, `$foo{`) stays in the SCALAR
//     style.
//   * 19 LONGQUOTE — "obsolete: replaced by qq/qx/qr/qw"; modern
//     lexer emits STRING_QQ/QX/QR/QW (27-30) instead.
// Declared here for completeness (a future Lexilla version may
// activate them) but `PERL_STYLES` leaves all four unmapped.
//
// **`SCE_PL_DEFAULT` (0), `SCE_PL_ERROR` (1), `SCE_PL_IDENTIFIER`
// (11) intentionally unmapped** in `PERL_STYLES` — fall through to
// STYLE_DEFAULT. `_DEFAULT` is the universal omission; `_IDENTIFIER`
// is bare-identifier (post-keyword-miss) text — same precedent as
// `SCE_C_IDENTIFIER` / `SCE_PAS_IDENTIFIER` / `SCE_VB_IDENTIFIER`.
// `_ERROR` is the soft-warning state for unbalanced delimiters etc.
// — pending the future `StyleSlot::Error` palette addition (now at
// 11 entries on the deferred-Error-slot migration list — adds the
// LexPerl ERROR state to the existing 10).
pub const SCE_PL_DEFAULT: usize = 0;
pub const SCE_PL_ERROR: usize = 1;
pub const SCE_PL_COMMENTLINE: usize = 2;
pub const SCE_PL_POD: usize = 3;
pub const SCE_PL_NUMBER: usize = 4;
pub const SCE_PL_WORD: usize = 5;
pub const SCE_PL_STRING: usize = 6;
pub const SCE_PL_CHARACTER: usize = 7;
pub const SCE_PL_PUNCTUATION: usize = 8;
pub const SCE_PL_PREPROCESSOR: usize = 9;
pub const SCE_PL_OPERATOR: usize = 10;
pub const SCE_PL_IDENTIFIER: usize = 11;
pub const SCE_PL_SCALAR: usize = 12;
pub const SCE_PL_ARRAY: usize = 13;
pub const SCE_PL_HASH: usize = 14;
pub const SCE_PL_SYMBOLTABLE: usize = 15;
pub const SCE_PL_VARIABLE_INDEXER: usize = 16;
pub const SCE_PL_REGEX: usize = 17;
pub const SCE_PL_REGSUBST: usize = 18;
pub const SCE_PL_LONGQUOTE: usize = 19;
pub const SCE_PL_BACKTICKS: usize = 20;
pub const SCE_PL_DATASECTION: usize = 21;
pub const SCE_PL_HERE_DELIM: usize = 22;
pub const SCE_PL_HERE_Q: usize = 23;
pub const SCE_PL_HERE_QQ: usize = 24;
pub const SCE_PL_HERE_QX: usize = 25;
pub const SCE_PL_STRING_Q: usize = 26;
pub const SCE_PL_STRING_QQ: usize = 27;
pub const SCE_PL_STRING_QX: usize = 28;
pub const SCE_PL_STRING_QR: usize = 29;
pub const SCE_PL_STRING_QW: usize = 30;
pub const SCE_PL_POD_VERB: usize = 31;
pub const SCE_PL_SUB_PROTOTYPE: usize = 40;
pub const SCE_PL_FORMAT_IDENT: usize = 41;
pub const SCE_PL_FORMAT: usize = 42;
pub const SCE_PL_STRING_VAR: usize = 43;
pub const SCE_PL_XLAT: usize = 44;
pub const SCE_PL_REGEX_VAR: usize = 54;
pub const SCE_PL_REGSUBST_VAR: usize = 55;
pub const SCE_PL_BACKTICKS_VAR: usize = 57;
pub const SCE_PL_HERE_QQ_VAR: usize = 61;
pub const SCE_PL_HERE_QX_VAR: usize = 62;
pub const SCE_PL_STRING_QQ_VAR: usize = 64;
pub const SCE_PL_STRING_QX_VAR: usize = 65;
pub const SCE_PL_STRING_QR_VAR: usize = 66;

// LexHTML (hypertext) style indices — the `H` prefix is upstream's
// for the HTML portion of the multi-mode lexer. The hypertext lexer
// also emits SCE_HJ_* (embedded JavaScript), SCE_HB_* (VBScript),
// SCE_HP_* (Python), and SCE_HPHP_* (PHP) when inside the matching
// `<script>` / `<%...%>` / `<?php ?>` block. Phase 4.5 wires the
// HTML + PHP subsets first; the embedded-script ranges come in with
// later language rows. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 267-298.
// `SCE_H_DEFAULT` and `SCE_H_SCRIPT` are intentionally *not* assigned
// a slot in `ui_win32`'s `HYPERTEXT_STYLES`: `_DEFAULT` is the
// inherit-from-`STYLE_DEFAULT` fallback for unclassified text (the
// user's chosen default fg/bg shows through), and `_SCRIPT` is
// internal lexer transition state that should never reach a rendered
// token. Same omission rationale applies to `SCE_HPHP_DEFAULT` below.
pub const SCE_H_DEFAULT: usize = 0;
pub const SCE_H_TAG: usize = 1;
pub const SCE_H_TAGUNKNOWN: usize = 2;
pub const SCE_H_ATTRIBUTE: usize = 3;
pub const SCE_H_ATTRIBUTEUNKNOWN: usize = 4;
pub const SCE_H_NUMBER: usize = 5;
pub const SCE_H_DOUBLESTRING: usize = 6;
pub const SCE_H_SINGLESTRING: usize = 7;
pub const SCE_H_OTHER: usize = 8;
pub const SCE_H_COMMENT: usize = 9;
pub const SCE_H_ENTITY: usize = 10;
pub const SCE_H_TAGEND: usize = 11;
pub const SCE_H_XMLSTART: usize = 12;
pub const SCE_H_XMLEND: usize = 13;
pub const SCE_H_SCRIPT: usize = 14; // internal transition state — see banner above
pub const SCE_H_ASP: usize = 15;
pub const SCE_H_ASPAT: usize = 16;
pub const SCE_H_CDATA: usize = 17;
pub const SCE_H_QUESTION: usize = 18;
pub const SCE_H_VALUE: usize = 19;
pub const SCE_H_XCCOMMENT: usize = 20;

// LexHTML — SGML / DTD sub-language style indices. Fired inside the
// `<!DOCTYPE ... [ ... ]>` block: markup declarations like
// `<!ELEMENT foo (...)>`, `<!ENTITY % bar "baz">`, attribute lists,
// external identifiers, etc. The `xml` and `hypertext` lexers both
// emit these style numbers when processing a DOCTYPE block, so
// mapping them in `HYPERTEXT_STYLES` benefits HTML / ASP / JSP /
// PHP / XML simultaneously.
//
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 288-298. `BLOCK_DEFAULT` is the per-block fallback; both
// `DEFAULT` (21) and `BLOCK_DEFAULT` (31) are intentionally left
// out of `HYPERTEXT_STYLES` so they fall through to STYLE_DEFAULT
// (matches the existing `SCE_H_DEFAULT` / `SCE_HPHP_DEFAULT`
// omission pattern). `ERROR` (26) is also unmapped pending a
// future `StyleSlot::Error` palette addition.
pub const SCE_H_SGML_DEFAULT: usize = 21;
pub const SCE_H_SGML_COMMAND: usize = 22;
pub const SCE_H_SGML_1ST_PARAM: usize = 23;
pub const SCE_H_SGML_DOUBLESTRING: usize = 24;
pub const SCE_H_SGML_SIMPLESTRING: usize = 25;
pub const SCE_H_SGML_ERROR: usize = 26;
pub const SCE_H_SGML_SPECIAL: usize = 27;
pub const SCE_H_SGML_ENTITY: usize = 28;
pub const SCE_H_SGML_COMMENT: usize = 29;
pub const SCE_H_SGML_1ST_PARAM_COMMENT: usize = 30;
pub const SCE_H_SGML_BLOCK_DEFAULT: usize = 31;

// LexMake (Makefile) style indices. The lexer is small — six emitted
// indices plus an error indicator at 9. Cross-referenced against
// `vendor/lexilla/lexers/LexMake.cxx` lines 54-63. Indices 6 / 7 / 8
// are documented upstream as "unused"; we omit them.
//
// `SCE_MAKE_DEFAULT` (0) is intentionally left unmapped in
// `MAKEFILE_STYLES` so it falls through to STYLE_DEFAULT (same
// pattern as `SCE_H_DEFAULT` / `SCE_HPHP_DEFAULT`).
// `SCE_MAKE_IDEOL` (9) — error indicator for an unclosed `$(`
// variable reference at end-of-line — is also unmapped, pending the
// same future `StyleSlot::Error` palette addition as
// `SCE_H_SGML_ERROR` and `SCE_H_SGML_1ST_PARAM_COMMENT`.
pub const SCE_MAKE_DEFAULT: usize = 0;
pub const SCE_MAKE_COMMENT: usize = 1;
pub const SCE_MAKE_PREPROCESSOR: usize = 2;
pub const SCE_MAKE_IDENTIFIER: usize = 3;
pub const SCE_MAKE_OPERATOR: usize = 4;
pub const SCE_MAKE_TARGET: usize = 5;
pub const SCE_MAKE_IDEOL: usize = 9;

// LexPascal style indices. 16 total emission slots covering all of
// Pascal's lexical surface (three comment forms, two preprocessor
// dialects, decimal+hex numbers, word/operator/string trio, character
// literals, inline assembler, and Delphi 11+ triple-quoted
// multiline strings). Cross-referenced against
// `vendor/lexilla/lexers/LexPascal.cxx` lines 171-186.
//
// `SCE_PAS_DEFAULT` (0) and `SCE_PAS_IDENTIFIER` (1) are intentionally
// left unmapped in `PASCAL_STYLES` so they fall through to
// STYLE_DEFAULT — same omission pattern as `SCE_C_DEFAULT` /
// `SCE_C_IDENTIFIER` in `CPP_STYLES`. `SCE_PAS_STRINGEOL` (11),
// the unterminated-string error indicator, is also unmapped pending
// the future `StyleSlot::Error` palette addition.
pub const SCE_PAS_DEFAULT: usize = 0;
pub const SCE_PAS_IDENTIFIER: usize = 1;
pub const SCE_PAS_COMMENT: usize = 2;
pub const SCE_PAS_COMMENT2: usize = 3;
pub const SCE_PAS_COMMENTLINE: usize = 4;
pub const SCE_PAS_PREPROCESSOR: usize = 5;
pub const SCE_PAS_PREPROCESSOR2: usize = 6;
pub const SCE_PAS_NUMBER: usize = 7;
pub const SCE_PAS_HEXNUMBER: usize = 8;
pub const SCE_PAS_WORD: usize = 9;
pub const SCE_PAS_STRING: usize = 10;
pub const SCE_PAS_STRINGEOL: usize = 11;
pub const SCE_PAS_CHARACTER: usize = 12;
pub const SCE_PAS_OPERATOR: usize = 13;
pub const SCE_PAS_ASM: usize = 14;
pub const SCE_PAS_MULTILINESTRING: usize = 15;

// LexBatch style indices. 9 contiguous slots covering the entire
// Windows batch / cmd.exe lexical surface — line comments (REM /
// `::`), two distinct keyword classes (cmd.exe intrinsics vs.
// PATH-discovered external programs), `:label` markers, the leading
// `@` echo-suppress directive, generic identifiers, operator
// punctuation (`&` / `|` / `<` / `>` / `>>` and the `&&` / `||`
// pairings — parentheses are deliberately styled as DEFAULT by the
// lexer per `LexBatch.cxx:595`, *not* OPERATOR), and "after-label"
// trailing text the cmd interpreter ignores. Cross-referenced
// against `vendor/lexilla/lexers/LexBatch.cxx` lines 44-55.
//
// `SCE_BAT_DEFAULT` (0) and `SCE_BAT_IDENTIFIER` (6) are
// intentionally left unmapped in `BATCH_STYLES` so they fall
// through to STYLE_DEFAULT — same omission pattern as
// `SCE_C_DEFAULT` / `SCE_C_IDENTIFIER` in `CPP_STYLES` (generic
// identifiers, `%VAR%` expansion bodies, and unrecognised bare
// tokens carry no language-specific meaning).
pub const SCE_BAT_DEFAULT: usize = 0;
pub const SCE_BAT_COMMENT: usize = 1;
pub const SCE_BAT_WORD: usize = 2;
pub const SCE_BAT_LABEL: usize = 3;
pub const SCE_BAT_HIDE: usize = 4;
pub const SCE_BAT_COMMAND: usize = 5;
pub const SCE_BAT_IDENTIFIER: usize = 6;
pub const SCE_BAT_OPERATOR: usize = 7;
pub const SCE_BAT_AFTER_LABEL: usize = 8;

// LexProps (INI / `.properties` files) style indices. 6 contiguous
// slots covering the entire INI / Java-properties surface — line
// comments (`#` / `!` / `;`), `[section]` headers, key names,
// the `=` or `:` assignment separator, and Java's `@`-prefixed
// default-value syntax. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 486-491 and
// `vendor/lexilla/lexers/LexProps.cxx` lines 38-80 (the
// `ColourisePropsLine` per-line classifier) plus line 82
// (`ColourisePropsDoc`, the zero-wordlist entry point whose
// unused `WordList *[]` parameter justifies the no-keywords
// claim below).
//
// `SCE_PROPS_DEFAULT` (0) is intentionally left unmapped in
// `PROPS_STYLES` so it falls through to STYLE_DEFAULT — same
// omission pattern as `SCE_C_DEFAULT` / `SCE_BAT_DEFAULT`.
// Value text (the part after `=` / `:`) lands in DEFAULT by design;
// INI values are arbitrary user data with no canonical meaning to
// colour. `LexProps` itself is a **zero-wordlist** lexer — the
// `WordList *[]` parameter in `ColourisePropsDoc` is unused — so
// the host installs no `SCI_SETKEYWORDS` calls for `L_INI` or
// `L_PROPS`. Classification is purely line-prefix-based.
pub const SCE_PROPS_DEFAULT: usize = 0;
pub const SCE_PROPS_COMMENT: usize = 1;
pub const SCE_PROPS_SECTION: usize = 2;
pub const SCE_PROPS_ASSIGNMENT: usize = 3;
pub const SCE_PROPS_DEFVAL: usize = 4;
pub const SCE_PROPS_KEY: usize = 5;

// LexHTML — PHP-mode style indices. Emitted when the lexer is
// inside a `<?php ... ?>` block. `SCE_HPHP_COMPLEX_VARIABLE` lives
// at 104 historically; the rest are a contiguous 118..=127 range.
// Cross-referenced against `vendor/lexilla/include/SciLexer.h`
// lines 356 and 370-379.
pub const SCE_HPHP_COMPLEX_VARIABLE: usize = 104;
pub const SCE_HPHP_DEFAULT: usize = 118;
pub const SCE_HPHP_HSTRING: usize = 119;
pub const SCE_HPHP_SIMPLESTRING: usize = 120;
pub const SCE_HPHP_WORD: usize = 121;
pub const SCE_HPHP_NUMBER: usize = 122;
pub const SCE_HPHP_VARIABLE: usize = 123;
pub const SCE_HPHP_COMMENT: usize = 124;
pub const SCE_HPHP_COMMENTLINE: usize = 125;
pub const SCE_HPHP_HSTRING_VARIABLE: usize = 126;
pub const SCE_HPHP_OPERATOR: usize = 127;

// LexHTML — embedded JavaScript inside client-side `<script>` blocks.
// 14 contiguous indices 40..=53. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 299-312.
//
// `SCE_HJ_START` (40) is the script-region boundary marker and
// `SCE_HJ_DEFAULT` (41) is the per-block fallback; both intentionally
// stay out of `HYPERTEXT_STYLES` so they fall through to STYLE_DEFAULT
// (mirrors `SCE_H_DEFAULT` / `SCE_HPHP_DEFAULT`). `SCE_HJ_STRINGEOL`
// (51) is the unterminated-string error indicator — unmapped pending
// `StyleSlot::Error` (same deferral as `SCE_H_SGML_ERROR` /
// `SCE_PAS_STRINGEOL` / `SCE_MAKE_IDEOL`).
pub const SCE_HJ_START: usize = 40;
pub const SCE_HJ_DEFAULT: usize = 41;
pub const SCE_HJ_COMMENT: usize = 42;
pub const SCE_HJ_COMMENTLINE: usize = 43;
pub const SCE_HJ_COMMENTDOC: usize = 44;
pub const SCE_HJ_NUMBER: usize = 45;
pub const SCE_HJ_WORD: usize = 46;
pub const SCE_HJ_KEYWORD: usize = 47;
pub const SCE_HJ_DOUBLESTRING: usize = 48;
pub const SCE_HJ_SINGLESTRING: usize = 49;
pub const SCE_HJ_SYMBOLS: usize = 50;
pub const SCE_HJ_STRINGEOL: usize = 51;
pub const SCE_HJ_REGEX: usize = 52;
pub const SCE_HJ_TEMPLATELITERAL: usize = 53;

// LexHTML — embedded JavaScript inside ASP server-side `<% %>` blocks
// (the `A` infix is upstream's for "ASP"). Same 14-suffix shape as
// `SCE_HJ_*`, shifted to 55..=68. Same `_START` / `_DEFAULT` /
// `_STRINGEOL` omission rationale as `SCE_HJ_*` above.
// Cross-referenced against `vendor/lexilla/include/SciLexer.h` lines
// 313-326.
pub const SCE_HJA_START: usize = 55;
pub const SCE_HJA_DEFAULT: usize = 56;
pub const SCE_HJA_COMMENT: usize = 57;
pub const SCE_HJA_COMMENTLINE: usize = 58;
pub const SCE_HJA_COMMENTDOC: usize = 59;
pub const SCE_HJA_NUMBER: usize = 60;
pub const SCE_HJA_WORD: usize = 61;
pub const SCE_HJA_KEYWORD: usize = 62;
pub const SCE_HJA_DOUBLESTRING: usize = 63;
pub const SCE_HJA_SINGLESTRING: usize = 64;
pub const SCE_HJA_SYMBOLS: usize = 65;
pub const SCE_HJA_STRINGEOL: usize = 66;
pub const SCE_HJA_REGEX: usize = 67;
pub const SCE_HJA_TEMPLATELITERAL: usize = 68;

// LexHTML — embedded VBScript inside client-side
// `<script language=VBScript>` blocks. 8 contiguous indices 70..=77.
// Cross-referenced against `vendor/lexilla/include/SciLexer.h` lines
// 327-334.
//
// VBScript has fewer lexical categories than JavaScript: only ONE
// comment class (`SCE_HB_COMMENTLINE`, 72) because VBScript has no
// block-comment syntax — both apostrophe-prefixed `' ...` lines and
// `Rem ...` statements end at the line terminator. Only ONE string
// class (`SCE_HB_STRING`, 75) — VBScript has no single-quoted strings
// (single quote starts a comment). No `_KEYWORD` / `_SYMBOLS` /
// `_REGEX` / `_TEMPLATELITERAL` classes (no separate ECMAScript-style
// keyword class, operators tokenise as `_DEFAULT`, no regex
// literals, no template literals). It does have its own
// `_IDENTIFIER` class (76) that JS lacks.
//
// `SCE_HB_START` (70) / `SCE_HB_DEFAULT` (71) intentionally stay out
// of `HYPERTEXT_STYLES` (boundary / fall-through, mirrors
// `SCE_H_DEFAULT`). `SCE_HB_IDENTIFIER` (76) also unmapped (matches
// `SCE_C_IDENTIFIER` / `SCE_PAS_IDENTIFIER` — generic identifiers
// fall through). `SCE_HB_STRINGEOL` (77) unmapped pending
// `StyleSlot::Error`.
pub const SCE_HB_START: usize = 70;
pub const SCE_HB_DEFAULT: usize = 71;
pub const SCE_HB_COMMENTLINE: usize = 72;
pub const SCE_HB_NUMBER: usize = 73;
pub const SCE_HB_WORD: usize = 74;
pub const SCE_HB_STRING: usize = 75;
pub const SCE_HB_IDENTIFIER: usize = 76;
pub const SCE_HB_STRINGEOL: usize = 77;

// LexHTML — embedded VBScript inside ASP server-side `<% %>` blocks.
// The bread-and-butter case for Classic ASP. Same 8-suffix shape as
// `SCE_HB_*`, shifted to 80..=87. Same omissions and same
// `_COMMENTLINE`-only comment class as `SCE_HB_*`. Cross-referenced
// against `vendor/lexilla/include/SciLexer.h` lines 335-342.
pub const SCE_HBA_START: usize = 80;
pub const SCE_HBA_DEFAULT: usize = 81;
pub const SCE_HBA_COMMENTLINE: usize = 82;
pub const SCE_HBA_NUMBER: usize = 83;
pub const SCE_HBA_WORD: usize = 84;
pub const SCE_HBA_STRING: usize = 85;
pub const SCE_HBA_IDENTIFIER: usize = 86;
pub const SCE_HBA_STRINGEOL: usize = 87;

// LexAsn1 style indices. 11 contiguous slots (0..=10) covering the
// ASN.1 lexer's full emission set: `--`-to-EOL comments (COMMENT),
// `"..."` double-quoted strings (STRING), decimal digit-only
// numerals (SCALAR) at generic positions, decimal-only numerals
// **inside** a `{...}` OID definition (OID — distinguished so the
// host can paint object identifier digits differently from bare
// scalars), identifier-shaped tokens (IDENTIFIER) resolving via
// wordlist probe into KEYWORD / ATTRIBUTE / DESCRIPTOR / TYPE
// (LexAsn1's four-class descriptor), and the affectation operator
// `::=` plus punctuation (OPERATOR). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 1108-1118, the
// `SCE_ASN1_*` block in `vendor/lexilla/include/LexicalStyles.iface`
// lines 1241-1252 (line 1240 is the `# Lexical states for SCLEX_ASN1`
// header comment), and the `LexerModule lmAsn1(SCLEX_ASN1,
// ColouriseAsn1Doc, "asn1", FoldAsn1Doc, asn1WordLists)`
// registration at `vendor/lexilla/lexers/LexAsn1.cxx:189`.
//
// Dispatches SCLEX_ASN1 (= 63, per `SciLexer.h:79`) — one of the
// oldest Lexilla lexers (2004, Herr Pfarrer rpfarrer). The lexer's
// design intent is **SNMP MIB / ASN.1 module parsing** — the
// `SCE_ASN1_OID` slot and the `ATTRIBUTE` / `DESCRIPTOR` split
// (unusual for a pure ASN.1 grammar) both trace to SNMP MIB
// conventions (RFC 1155 / 1902 / 2578 SMI). Extensions `.asn1`
// (LANG_TABLE row for `L_ASN1`).
//
// **Four wordlists.** `asn1WordLists[]` at `LexAsn1.cxx:181-186`
// declares four named classes with **first-match-wins semantics**
// via the `if` / `else if` chain at `:95-106` inside the
// `SCE_ASN1_IDENTIFIER` collect-state closer:
//   - class 0 = "Keywords" — X.680 structural reserved words
//     (`DEFINITIONS` / `BEGIN` / `END` / `SEQUENCE` / `CHOICE` /
//     `IMPORTS` / `EXPORTS` / etc.). Highest priority.
//   - class 1 = "Attributes" — SMI macro headers + field names
//     (`OBJECT-TYPE` / `MODULE-IDENTITY` / `SYNTAX` / `ACCESS` /
//     `STATUS` / `DESCRIPTION` / etc.). Second priority.
//   - class 2 = "Descriptors" — SNMP MIB lowercase constants
//     (`current` / `read-only` / `mandatory` / `iso` / `internet`
//     / etc.). Third priority.
//   - class 3 = "Types" — X.680 primitive types + SMI textual
//     conventions (`INTEGER` / `IA5String` / `Counter32` /
//     `DisplayString` / etc.). Lowest priority.
//
// **Classifier order.** `LexAsn1.cxx:95-106` probes wordlists in
// this fixed order after the identifier-collect state closes:
//   1. If `Keywords.InList(s)` → `SCE_ASN1_KEYWORD`.
//   2. Else if `Attributes.InList(s)` → `SCE_ASN1_ATTRIBUTE`.
//   3. Else if `Descriptors.InList(s)` → `SCE_ASN1_DESCRIPTOR`.
//   4. Else if `Types.InList(s)` → `SCE_ASN1_TYPE`.
//   5. Else → `SCE_ASN1_IDENTIFIER` (bareword — the state never
//      transitions away, so the collected bytes retain the
//      `IDENTIFIER` style at paint time). Cross-class duplicates
//      are silently masked by the earlier class. Framework
//      enforces disjointness in the invariant test to keep the
//      wordlist authorship intent visible.
//
// **Case-SENSITIVE identifier lookup.** `LexAsn1.cxx:94` populates
// the identifier buffer via `sc.GetCurrent(s, sizeof(s))`, which
// copies raw source bytes verbatim without any tolower step —
// confirmed by grep (no `tolower` / `MakeLowerCase` /
// `GetCurrentLowered` anywhere in the file). Wordlist entries
// must match source spelling exactly. ASN.1 / SMI convention:
// structural keywords + SMI macro headers are `UPPERCASE` or
// `UPPER-HYPHEN-CASE` (`DEFINITIONS` / `OBJECT-TYPE`), MIB
// descriptors are `lowercase` or `lower-hyphen-case` (`current`
// / `read-only`), primitive types are `MixedCase` (`INTEGER` /
// `IA5String` / `Counter32`).
//
// **`isAsn1Char` identifier grammar.** `LexAsn1.cxx:42-45`
// defines identifier characters as `-` OR ASCII digit OR ASCII
// letter — no `_` (unlike C / Python / most other languages) and
// no Unicode support. Structural consequence: SMI's hyphen-glued
// macro names (`OBJECT-TYPE` / `MAX-ACCESS` / `read-only`) all
// tokenise as SINGLE identifiers. The classifier probe treats
// `MAX-ACCESS` as one wordlist entry, NOT `MAX` + `-` + `ACCESS`.
// Wordlists MUST use hyphen-glued spelling matching source form.
//
// **`SCE_ASN1_OID` (4) vs `SCE_ASN1_SCALAR` (5)** — both paint
// decimal digit runs, but distinct emission sites. SCALAR is
// entered at `:76` from `SCE_ASN1_DEFAULT` when the classifier
// sees a leading digit (ASN.1 identifiers must start with a
// letter, so a bareword digit run is always a plain number). OID
// is entered at `:135` and `:154` from inside the `SCE_ASN1_OPERATOR`
// sub-loop that follows a `::=` — the sub-loop starts on `{`
// (OID definition, e.g. `::= { iso 3 6 1 }`) or a bare digit
// (trap number, e.g. `::= 42 trap`). Inside the `{...}` variant,
// digit runs that BEGIN a new numeric token get OID — the `:133`
// condition requires `!isAsn1Char(sc.chPrev) || isAsn1Number(
// sc.chPrev)`, i.e. either the first digit at a token boundary or
// a continuation of an existing digit run; digits embedded within
// an identifier-shaped token (e.g. `iso3` written without a
// separator) instead fall into the `isAsn1Char` branch at `:136`
// and route to `IDENTIFIER` — an edge case that doesn't arise in
// canonical OID syntax but is worth flagging. Framework routes
// BOTH OID and SCALAR to `StyleSlot::Number` — same paint colour,
// but the lexer-level distinction is real and worth preserving
// for future palette tweaks (e.g., a dedicated "OID" slot with a
// subtle accent).
//
// **`SCE_ASN1_IDENTIFIER` (2)** — the transient collect state at
// `:79` covering identifier-shaped bytes as they accumulate. Two
// exit paths:
//   1. Match found via `Keywords` / `Attributes` / `Descriptors`
//      / `Types` list → `sc.ChangeState(SCE_ASN1_KEYWORD |
//      ATTRIBUTE | DESCRIPTOR | TYPE)` at `:97, :100, :103, :106`
//      retroactively re-styles the collected bytes.
//   2. No match → state stays `IDENTIFIER` when `SetState(
//      SCE_ASN1_DEFAULT)` fires at `:109`; the bytes emitted so
//      far keep the `IDENTIFIER` style at paint. Bareword
//      identifiers (net names / user-defined type references /
//      OID parent names inside `{...}`) paint distinctly. Framework
//      leaves `IDENTIFIER` unmapped so bareword tokens fall
//      through to `STYLE_DEFAULT` — same convention as
//      `SCE_C_IDENTIFIER` / `SCE_REBOL_IDENTIFIER` /
//      `SCE_OSCRIPT_IDENTIFIER` / `SCE_SPICE_IDENTIFIER`.
//
// **`SCE_ASN1_OPERATOR` (10)** — entered at `:82` only on `:`
// (the `::=` affectation operator's first byte). The state
// contains a sub-loop that consumes subsequent `:` / `=` / space
// and then either enters an OID definition on `{` or a trap-
// number scalar on a bare digit. Other operators (`,` / `;` /
// `(` / `)` / `[` / `]`) actually paint as `SCE_ASN1_DEFAULT`
// because the state machine at `:66-83` only recognises `-`
// (comment start), `"` (string start), digit (scalar), letter
// (identifier), and `:` (operator) as state triggers — all
// other punctuation stays in DEFAULT. Framework maps OPERATOR
// to `StyleSlot::Operator` for the `::=` bytes; other
// punctuation gets the fall-through STYLE_DEFAULT.
pub const SCLEX_ASN1: usize = 63;
pub const SCE_ASN1_DEFAULT: usize = 0;
pub const SCE_ASN1_COMMENT: usize = 1;
pub const SCE_ASN1_IDENTIFIER: usize = 2;
pub const SCE_ASN1_STRING: usize = 3;
pub const SCE_ASN1_OID: usize = 4;
pub const SCE_ASN1_SCALAR: usize = 5;
pub const SCE_ASN1_KEYWORD: usize = 6;
pub const SCE_ASN1_ATTRIBUTE: usize = 7;
pub const SCE_ASN1_DESCRIPTOR: usize = 8;
pub const SCE_ASN1_TYPE: usize = 9;
pub const SCE_ASN1_OPERATOR: usize = 10;

// LexAVS style indices. 15 contiguous slots (0..=14) covering the
// AviSynth lexer's full emission set: nested `/*...*/` and `[*...*]`
// block comments (COMMENTBLOCK / COMMENTBLOCKN — depth-tracked via
// per-line state), `#`-to-EOL line comments (COMMENTLINE), decimal /
// dot-fractional / signed-fractional numeric literals (NUMBER), the
// AviSynth operator set (OPERATOR — anything `isoperator(ch)` accepts),
// identifier-shaped tokens (IDENTIFIER) resolving via wordlist probe
// into KEYWORD / FILTER / PLUGIN / FUNCTION / CLIPPROP / USERDFN
// (LexAVS's six-class descriptor), `"..."` double-quoted strings
// (STRING), and `"""..."""` triple-quoted verbatim strings
// (TRIPLESTRING). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 1677-1691, the
// `SCE_AVS_*` block in `vendor/lexilla/include/LexicalStyles.iface`,
// and the `LexerModule lmAVS(SCLEX_AVS, ColouriseAvsDoc, "avs",
// FoldAvsDoc, avsWordLists)` registration at
// `vendor/lexilla/lexers/LexAVS.cxx:294`.
//
// Dispatches SCLEX_AVS (= 104, per `SciLexer.h:120`) — a 2012 lexer
// by Bruno Barbieri "heavily based on LexPOV" (`LexAVS.cxx:6`).
// Extensions `.avs` (script) and `.avsi` (import). `L_AVS`
// (LANG_TABLE id 66) is the only language row using this lexer.
//
// **Six wordlists.** `avsWordLists[]` at `LexAVS.cxx:284-292`
// declares six named classes with **first-match-wins semantics**
// via the `if` / `else if` chain at `:101-113` inside the
// `SCE_AVS_IDENTIFIER` collect-state closer:
//   - class 0 = "Keywords" — structural control-flow words
//     (`function` / `return` / `global` / `if` / `else` /
//     `while` / `for` / `break` / `continue` / `try` / `catch`
//     / `true` / `false`). Highest priority. `import` is a
//     RUNTIME function call (`Import("file.avs")`) in AviSynth,
//     not a language keyword — it lives in class 3 with the
//     other utility functions.
//   - class 1 = "Filters" — built-in AviSynth filter functions
//     (`avisource` / `crop` / `trim` / `overlay` / `convertorgb`
//     / etc.). Second priority.
//   - class 2 = "Plugins" — third-party plugin function names
//     (`ffvideosource` / `qtgmc` / `mvtools2` / etc.). Third
//     priority. Empty in stock AviSynth; populated per user's
//     plugin installation.
//   - class 3 = "Functions" — built-in scalar / math / string /
//     runtime helper functions (`abs` / `min` / `max` / `sin` /
//     `chr` / `string` / `defined` / etc.). Fourth priority.
//   - class 4 = "Clip properties" — clip-info properties accessed
//     via dot-syntax (`width` / `height` / `framecount` /
//     `framerate` / `isyv12` / etc.). Fifth priority.
//   - class 5 = "User defined functions" — script-declared
//     functions the user (or an editor auto-populator) explicitly
//     lists. Lowest priority. Left empty in Code++'s theme by
//     default — the state is dead code by config choice, matching
//     the framework precedent for optional emit-only-if-populated
//     classes.
//
// **Classifier order.** `LexAVS.cxx:101-113` probes wordlists in
// this fixed order after the identifier-collect state closes:
//   1. `keywords.InList(s)` → `SCE_AVS_KEYWORD`.
//   2. `filters.InList(s)` → `SCE_AVS_FILTER`.
//   3. `plugins.InList(s)` → `SCE_AVS_PLUGIN`.
//   4. `functions.InList(s)` → `SCE_AVS_FUNCTION`.
//   5. `clipProperties.InList(s)` → `SCE_AVS_CLIPPROP`.
//   6. `userDefined.InList(s)` → `SCE_AVS_USERDFN`.
//   7. Fall through → identifier bytes retain `SCE_AVS_IDENTIFIER`.
//
// **Case-INSENSITIVE identifier lookup.** `LexAVS.cxx:99, :189`
// populate the identifier buffer via
// `sc.GetCurrentLowered(s, sizeof(s))` — every collected byte is
// lowered *before* the wordlist probe. Wordlist entries **must be
// byte-canonical lowercase** to match at all. AviSynth source
// uses PascalCase / camelCase / lowercase interchangeably (`Trim`
// / `trim` / `TRIM` all match the same wordlist entry `trim`);
// the wordlist author cannot preserve source casing. Same
// case-insensitive contract as [`SCE_HB_WORD`] (VBScript), [`SCE_ADA_WORD`]
// (Ada), [`SCE_F_WORD`] (Fortran), [`SCE_SQL_WORD`] (SQL).
//
// **Word grammar.** `LexAVS.cxx:32-38` defines identifier chars
// as ASCII alphanumeric OR `_` (`IsAWordChar`), and word starts
// as `isalpha(ch)` OR any non-space/newline/`(`/`.`/`,` char
// (`IsAWordStart` — deliberately permissive; the closer at
// `:97-115` will bail on non-`IsAWordChar` if no match). No
// Unicode support.
//
// **Nested block comments** — two distinct nesting families with
// per-line depth tracking:
//   - `SCE_AVS_COMMENTBLOCK` (1) — `/*...*/` style (C-family
//     convention).
//   - `SCE_AVS_COMMENTBLOCKN` (2) — `[*...*]` style (AviSynth's
//     own convention, distinct namespace so `[*` inside `/*`
//     doesn't affect the outer depth). Same "N" suffix
//     convention as OScript's nested comment state.
// Both track depth via `blockCommentLevel` at `:63, :80, :117-136`;
// depth is preserved across lines via `styler.SetLineState`
// (`:81, :84`). Framework routes both to `StyleSlot::Comment` —
// same collapse discipline as C's `SCE_C_COMMENT` /
// `SCE_C_COMMENTDOC` / `SCE_C_COMMENTLINE` unification.
//
// **Triple-quoted strings.** `SCE_AVS_TRIPLESTRING` (8) at
// `:146-152` covers `"""..."""` verbatim strings — AviSynth's
// answer to raw string literals (no escape processing, embedded
// `"` OK). Framework collapses with `SCE_AVS_STRING` (7) to
// `StyleSlot::String` — same discipline as Python's SCE_P_STRING
// / _CHARACTER / _TRIPLE / _TRIPLEDOUBLE quad-collapse.
//
// **`SCE_AVS_IDENTIFIER` (6)** — the transient collect state at
// `:180` covering identifier-shaped bytes as they accumulate.
// Exit paths mirror the LexAsn1 / LexSpice pattern:
//   1. Match found via one of the six wordlists →
//      `sc.ChangeState(...)` at `:102, :104, :106, :108, :110,
//      :112` retroactively re-styles the collected bytes.
//   2. No match → state stays `IDENTIFIER` when
//      `SetState(SCE_AVS_DEFAULT)` fires at `:114`; the bytes
//      keep the `IDENTIFIER` style at paint. Framework leaves
//      `IDENTIFIER` unmapped so bareword tokens (variable
//      references, `LoadPlugin` return-value bindings, iterator
//      names inside `for`) fall through to `STYLE_DEFAULT` —
//      same convention as `SCE_C_IDENTIFIER` /
//      `SCE_ASN1_IDENTIFIER` / `SCE_SPICE_IDENTIFIER`.
//
// **No STRINGEOL / no error state.** LexAVS does not emit a
// `SCE_AVS_STRINGEOL` or `SCE_AVS_STRING_ERROR` state for
// unterminated strings — the string state simply keeps consuming
// bytes until it hits the closing `"` or `"""`. If EOF arrives
// mid-string, the state persists into the file's tail. This is
// a deliberate simplification in the lexer's design; no
// framework mapping needed.
pub const SCLEX_AVS: usize = 104;
pub const SCE_AVS_DEFAULT: usize = 0;
pub const SCE_AVS_COMMENTBLOCK: usize = 1;
pub const SCE_AVS_COMMENTBLOCKN: usize = 2;
pub const SCE_AVS_COMMENTLINE: usize = 3;
pub const SCE_AVS_NUMBER: usize = 4;
pub const SCE_AVS_OPERATOR: usize = 5;
pub const SCE_AVS_IDENTIFIER: usize = 6;
pub const SCE_AVS_STRING: usize = 7;
pub const SCE_AVS_TRIPLESTRING: usize = 8;
pub const SCE_AVS_KEYWORD: usize = 9;
pub const SCE_AVS_FILTER: usize = 10;
pub const SCE_AVS_PLUGIN: usize = 11;
pub const SCE_AVS_FUNCTION: usize = 12;
pub const SCE_AVS_CLIPPROP: usize = 13;
pub const SCE_AVS_USERDFN: usize = 14;

// LexBaan style indices. 25 contiguous slots (0..=24) covering the
// Baan / BaanC lexer's full emission set: `|`-prefixed line comments
// (COMMENT), `dllusage`/`functionusage`-delimited doc blocks
// (COMMENTDOC), decimal + hex + scientific numeric literals (NUMBER),
// nine keyword classes (WORD..WORD9) covering reserved words + std
// functions + sections + predefined variables/attributes/enumerates,
// `"..."` double-quoted strings (STRING), `#`-prefixed preprocessor
// directives (PREPROCESSOR) with `^`-prefixed line-continuation
// support, punctuation operators (OPERATOR), identifier-shaped tokens
// (IDENTIFIER), unterminated-string parse-failure (STRINGEOL), and
// seven **semantically-typed identifier states** emitted by the
// pattern-matcher / position-tracker: `TABLEDEF` / `TABLESQL`
// (schema-name patterns like `^^^^^###` matched at
// `LexBaan.cxx:145-206`), `FUNCTION` (pattern-matched function
// names), `DOMDEF` / `FUNCDEF` (identifier following `domain` /
// `function` keyword), `OBJECTDEF` (identifier after `#pragma` /
// `#include`), and `DEFINEDEF` (identifier after `#define` /
// `#undef` / `#if*`). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h` lines 645-669 and
// `vendor/lexilla/lexers/LexBaan.cxx:995` (the `LexerModule lmBaan(
// SCLEX_BAAN, LexerBaan::LexerFactoryBaan, "baan", baanWordLists)`
// registration).
//
// Dispatches SCLEX_BAAN (= 31, per `SciLexer.h:47`) — a 2001 lexer
// by Vamsi Potluru + Praveen Ambekar, maintained by "oirfeodent" —
// "based heavily on LexCPP.cxx" (`LexBaan.cxx:4`). Baan 4GL is the
// scripting language of the Baan ERP system (later Infor ERP LN).
// Extension `.baan`.
//
// **Nine wordlists** at `LexBaan.cxx:71-81` — the richest wordlist
// descriptor of any Lexilla lexer that Code++ currently wires
// (Raku's 7-class is a distant second):
//   - class 0 = "Baan & BaanSQL Reserved Keywords " (trailing
//     space per source) — the core language grammar. Matches at
//     `:556` → `SCE_BAAN_WORD`.
//   - class 1 = "Baan Standard functions" — the built-in
//     function library. Matches at `:573` → `SCE_BAAN_WORD2`.
//   - class 2 = "Baan Functions Abridged" — user-defined
//     functions expressed with the `~` abridged-list separator
//     (see `WordListAbridged` at `:368-382`). Only matches at
//     `:576-580` if the NEXT char is `(` (function-call context);
//     otherwise the token stays `IDENTIFIER`. Emits `SCE_BAAN_WORD3`.
//   - class 3 = "Baan Main Sections " (trailing space) — section
//     headers like `declaration:` / `code:` / `functions:`.
//     Matches at `:582` → `SCE_BAAN_WORD4`. Section entries
//     typically end with `:` — `LexBaan.cxx:495` sets
//     `kwHasSection = strchr(wl, ':') != NULL` and the classifier
//     at `:556` uses `s1` (with trailing `:`) instead of `s` when
//     the section flag is set.
//   - class 4 = "Baan Sub Sections" — sub-section markers. Matches
//     at `:585` → `SCE_BAAN_WORD5`.
//   - class 5 = "PreDefined Variables" — Baan pre-defined system
//     variables. Matches at `:588` → `SCE_BAAN_WORD6`.
//   - class 6 = "PreDefined Attributes" — Baan attribute names.
//     Matches at `:591` → `SCE_BAAN_WORD7`.
//   - class 7 = "Enumerates" — enumerated values. Matches at
//     `:594` → `SCE_BAAN_WORD8`.
//   - class 8 = **unnamed** — 9th slot handled by the
//     `WordListSet` switch at `:484-486` but absent from the
//     descriptor array at `:71-81` (which has 8 named entries +
//     nullptr terminator). Emits `SCE_BAAN_WORD9` at `:597`.
//     Effectively a reserved user-customization slot.
//
// **Classifier order.** `LexBaan.cxx:556-599` probes wordlists in
// this fixed sequence, with **first-match-wins** across classes 0..=8
// (each is `else if`). Plus two position-tracked overrides at
// `:565-572`:
//   1. `lineHasDomain` (set when `domain` keyword seen earlier on
//      this line) → force `SCE_BAAN_DOMDEF` regardless of wordlist.
//   2. `lineHasFunction` (set when `function` keyword seen) → force
//      `SCE_BAAN_FUNCDEF`.
// These override class-0..=8 classification for the identifier
// immediately following `domain` or `function`.
//
// **Case-INSENSITIVE identifier lookup.** `LexBaan.cxx:550` calls
// `sc.GetCurrentLowered(s, sizeof(s))` — same discipline as LexVB /
// LexAVS / LexBasic. Wordlist entries MUST be byte-canonical
// lowercase. Baan 4GL source convention is lowercase already
// (`if` / `for` / `function` / `endfunction`) so this rarely
// surprises.
//
// **Pattern-matched semantic states.** `IsAnyOtherIdentifier` at
// `LexBaan.cxx:135-209` matches identifier byte patterns against
// Baan naming conventions:
//   - 8 chars, `^^^^^###` (5 letters + 3 digits) → `TABLEDEF`.
//   - 9 chars, `t^^^^^###` → `TABLEDEF`.
//   - 9 chars, `^^^^^###.` → `TABLESQL`.
//   - 13 chars, `^^^^^###.****` → `TABLESQL`.
//   - 13 chars, `rcd.t^^^^^###` → `TABLEDEF`.
//   - 14-15 chars, `^^^^^###.******` (no `:` at position 13) →
//     `TABLESQL`.
//   - 16-17 chars, `^^^^^###._index##` → `TABLEDEF`.
//   - 16-17 chars, `^^^^^###._compnr` → `TABLEDEF`.
//   - `^^^^^.dll####.` (>14 chars) → `FUNCTION`.
//   - `^^int.dll^^^^^.` (>15 chars) → `FUNCTION`.
//   - `i^^^^^####.` (>11 chars) → `FUNCTION`.
// These pattern rules encode Baan ERP's schema/module naming
// conventions (`ttadv100` = table advertising module 100,
// `tcadv001.dll012.` = function ID in transaction module, etc.)
// directly into the lexer.
//
// **`SCE_BAAN_WORD3` gating.** Unlike the other WORD* states,
// WORD3 only emits if the identifier is in class 2 AND the next
// char is `(` — see `:577-580`. This makes WORD3 specifically the
// "abridged function-call" state. If class 2 isn't populated,
// WORD3 never emits.
//
// **`SCE_BAAN_STRINGEOL` (9)** — parse-failure state. Emitted at
// `:653` when a string doesn't terminate before line end (unless
// the line ends with `^` continuation). Left unmapped per the
// framework's deferred `StyleSlot::Error` convention. Same
// discipline as Visual Prolog's error state.
//
// **`SCE_BAAN_IDENTIFIER` (8)** — the transient collect state at
// `:678`. Bareword identifiers that don't match any wordlist and
// don't match any pattern-based semantic slot fall through to
// `IDENTIFIER` and paint at STYLE_DEFAULT. Framework leaves
// unmapped per the [`SCE_C_IDENTIFIER`] / [`SCE_ASN1_IDENTIFIER`]
// / [`SCE_SPICE_IDENTIFIER`] convention.
pub const SCLEX_BAAN: usize = 31;
pub const SCE_BAAN_DEFAULT: usize = 0;
pub const SCE_BAAN_COMMENT: usize = 1;
pub const SCE_BAAN_COMMENTDOC: usize = 2;
pub const SCE_BAAN_NUMBER: usize = 3;
pub const SCE_BAAN_WORD: usize = 4;
pub const SCE_BAAN_STRING: usize = 5;
pub const SCE_BAAN_PREPROCESSOR: usize = 6;
pub const SCE_BAAN_OPERATOR: usize = 7;
pub const SCE_BAAN_IDENTIFIER: usize = 8;
pub const SCE_BAAN_STRINGEOL: usize = 9;
pub const SCE_BAAN_WORD2: usize = 10;
pub const SCE_BAAN_WORD3: usize = 11;
pub const SCE_BAAN_WORD4: usize = 12;
pub const SCE_BAAN_WORD5: usize = 13;
pub const SCE_BAAN_WORD6: usize = 14;
pub const SCE_BAAN_WORD7: usize = 15;
pub const SCE_BAAN_WORD8: usize = 16;
pub const SCE_BAAN_WORD9: usize = 17;
pub const SCE_BAAN_TABLEDEF: usize = 18;
pub const SCE_BAAN_TABLESQL: usize = 19;
pub const SCE_BAAN_FUNCTION: usize = 20;
pub const SCE_BAAN_DOMDEF: usize = 21;
pub const SCE_BAAN_FUNCDEF: usize = 22;
pub const SCE_BAAN_OBJECTDEF: usize = 23;
pub const SCE_BAAN_DEFINEDEF: usize = 24;

// LexTOML style indices. 16 contiguous slots (0..=15) covering the
// TOML (Tom's Obvious, Minimal Language) lexer's full emission set:
// `#`-to-EOL line comments (COMMENT), identifier-collect state
// (IDENTIFIER — transient), bareword literal keywords (KEYWORD —
// `true` / `false` / `inf` / `nan`), integer + float + hex / octal /
// binary + underscore-separated numeric literals (NUMBER),
// `[table]` / `[[array.of.tables]]` headers (TABLE),
// key-part on the LHS of `key = value` including dot-separated
// `foo.bar.baz` and quoted `"quoted"`/`'quoted'` key forms (KEY),
// unrecoverable parse-failure states (ERROR — bad-line-start
// character; STRINGEOL — string not terminated before EOL for
// single-line quote variants), TOML-specific operators (OPERATOR
// — `[`/`]`/`{`/`}`/`,`/`=`/`.`/`+`/`-`), four string flavours
// (STRING_SQ single-quoted literal, STRING_DQ double-quoted basic,
// TRIPLE_STRING_SQ multi-line literal, TRIPLE_STRING_DQ multi-line
// basic), escape sequences inside double-quoted strings only
// (ESCAPECHAR — TOML's `\uXXXX` / `\UXXXXXXXX` / `\xNN` / `\n` /
// etc.), and RFC 3339 date-time literals (DATETIME — full ISO
// `YYYY-MM-DDTHH:MM:SS+ZZ` and local variants). Cross-referenced
// against `vendor/lexilla/include/SciLexer.h` lines 2090-2105 and
// the `LexerModule lmTOML(SCLEX_TOML, ColouriseTOMLDoc, "toml",
// FoldTOMLDoc, tomlWordListDesc)` registration at
// `vendor/lexilla/lexers/LexTOML.cxx:494`.
//
// Dispatches SCLEX_TOML (= 136, per `SciLexer.h:152`) — a 2024
// lexer by Jiri Techet, ported from Zufu Liu's Notepad4 TOML
// lexer (`LexTOML.cxx:5-6`). Extension `.toml`. TOML v1.0.0 syntax.
//
// **One wordlist class** — `tomlWordListDesc[]` at
// `LexTOML.cxx:489-492` declares a single `"Keywords"` slot.
// Content per the upstream fixture at
// `crates/scintilla-sys/vendor/lexilla/test/examples/toml/
// SciTE.properties`: `false inf nan true` (4 tokens — TOML v1.0.0's
// full bareword-literal vocabulary).
//
// **Case-INSENSITIVE identifier lookup.** `LexTOML.cxx:132` calls
// `sc.GetCurrentLowered(s, sizeof(s))` before probing the
// wordlist. Wordlist entries MUST be byte-canonical lowercase.
// Note that TOML per the v1.0.0 spec is CASE-SENSITIVE for its
// literal keywords (`true` / `false` / `inf` / `nan` all lowercase
// per grammar), so the lexer's case-insensitive lookup is
// permissive — source `TRUE` / `True` would ALSO paint as
// SCE_TOML_KEYWORD despite being a syntax error per the spec.
// The paint is unconditionally correct for spec-conformant source;
// non-conformant source gets a "helpful" over-highlight.
//
// **KEY vs IDENTIFIER routing.** `LexTOML.cxx:207-214` sets
// `IDENTIFIER` for a lowercase-leading bareword run at `:372-373`
// (in DEFAULT-state entry), then at end-of-identifier calls
// `IsTOMLKey(sc, braceCount, keywordLists[0])` at `:209` which
// either (a) promotes to `SCE_TOML_KEY` if the next non-whitespace
// char is `=` / `.` / `-` inside an inline table (braceCount > 0
// case at `:122-129`), (b) promotes to `SCE_TOML_KEYWORD` if the
// collected token matches the wordlist (at `:136-138`), or (c)
// falls through to `SCE_TOML_DEFAULT` — the `IDENTIFIER` state
// itself never survives to paint. Framework leaves `IDENTIFIER`
// unmapped per the framework's transient-collect-fallthrough
// convention.
//
// **`SCE_TOML_TABLE` (5) and `SCE_TOML_KEY` (6)** — the two
// structural anchor states unique to TOML. TABLE fires on `[` at
// line start (`:337-344`, always at column 0 per TOML grammar);
// KEY fires either at line start on a bareword / quote-start
// (`:345-350`) or inline via the `IsTOMLKey` promotion
// (`:122-142`). Both consume dot-separated sub-key sequences
// (`foo.bar.baz` at `:244-253`) plus quoted variants (`'literal'`
// and `"basic"` per `TOMLKeyState::Literal` / `Quoted` at
// `:150-155`). Framework routes TABLE to Preprocessor (structural
// header archetype — same slot family as `[section]` markers in
// PROPS / Registry / INI) and KEY to Keyword2 (LHS-of-assignment
// archetype).
//
// **Four string flavours share `StyleSlot::String`.** TOML's
// spec-mandated four quote forms — `'...'` literal (no escapes),
// `"..."` basic (with escapes), `'''...'''` multi-line literal,
// `"""..."""` multi-line basic — all paint as String. The
// distinction matters for ESCAPECHAR handling (only DQ variants
// process `\`-escapes per `:283-287`) but not for framework paint.
// Same collapse discipline as Python's `SCE_P_STRING` /
// `_CHARACTER` / `_TRIPLE` / `_TRIPLEDOUBLE` and Raku's Q-language
// unification.
//
// **`SCE_TOML_ESCAPECHAR` (13)** — the entire escape sequence
// paints as a single ESCAPECHAR span (`:283-287, :308-313`).
// Includes `\b` / `\t` / `\n` / `\f` / `\r` / `\"` / `\\` / `\/`
// single-char forms, plus `\xNN` / `\uNNNN` / `\UNNNNNNNN` Unicode
// hex forms. Framework routes to Preprocessor for a distinct
// accent color — matches the `SCE_RUST_BYTEESCAPE` /
// `SCE_HJA_TEMPLATELITERAL` precedent for "escape sequence
// standing out from surrounding string content".
//
// **`SCE_TOML_DATETIME` (14)** — RFC 3339 date-time literals like
// `1979-05-27T07:32:00-07:00` per TOML spec §Local Date-Time. The
// lexer initially enters NUMBER state on the leading digit at
// `:370-371`, then re-classifies to DATETIME at `:189-190` when
// it detects an ISO-date-time-continuation character (`+`/`-`/`:`
// followed by a digit, per `IsISODateTime` at `:48-51`). Framework
// routes to Number (numeric-literal archetype) — datetimes are
// conceptually numeric constants in TOML.
//
// **`SCE_TOML_ERROR` (7)** — parse-failure state entered at
// `:353` when a line-start character isn't `#`, `[`, `'`, `"`, or
// an unquoted-key char. `SCE_TOML_STRINGEOL` (15) — separate
// parse-failure state entered at `:281-282` when a single-line
// string (SQ or DQ, NOT triple-string) doesn't terminate before
// line end. Both left unmapped per the framework's deferred
// `StyleSlot::Error`-migration convention. Same discipline as
// Visual Prolog / LexBasic ERROR states.
pub const SCLEX_TOML: usize = 136;
pub const SCE_TOML_DEFAULT: usize = 0;
pub const SCE_TOML_COMMENT: usize = 1;
pub const SCE_TOML_IDENTIFIER: usize = 2;
pub const SCE_TOML_KEYWORD: usize = 3;
pub const SCE_TOML_NUMBER: usize = 4;
pub const SCE_TOML_TABLE: usize = 5;
pub const SCE_TOML_KEY: usize = 6;
pub const SCE_TOML_ERROR: usize = 7;
pub const SCE_TOML_OPERATOR: usize = 8;
pub const SCE_TOML_STRING_SQ: usize = 9;
pub const SCE_TOML_STRING_DQ: usize = 10;
pub const SCE_TOML_TRIPLE_STRING_SQ: usize = 11;
pub const SCE_TOML_TRIPLE_STRING_DQ: usize = 12;
pub const SCE_TOML_ESCAPECHAR: usize = 13;
pub const SCE_TOML_DATETIME: usize = 14;
pub const SCE_TOML_STRINGEOL: usize = 15;

// LexSAS style indices. 16 contiguous slots (0..=15) covering the SAS
// (Statistical Analysis System, from SAS Institute Inc.) lexer's full
// state set — but only 10 of the 16 slots survive to paint in
// practice. Cross-referenced against
// `vendor/lexilla/include/SciLexer.h:1917-1932` and the
// `LexerModule lmSAS(SCLEX_SAS, ColouriseSASDoc, "sas", FoldSASDoc,
// SASWordLists)` registration at
// `vendor/lexilla/lexers/LexSAS.cxx:223`.
//
// Dispatches SCLEX_SAS (= 125, per `SciLexer.h:141`) — a lexer by
// Luke Rasmussen at Northwestern University Feinberg School of
// Medicine, developed as part of the StatTag project
// (`LexSAS.cxx:5-13`). Extension `.sas`.
//
// **Four wordlist classes** — `SASWordLists[]` at
// `LexSAS.cxx:216-221` declares three names plus a null terminator
// (`"Language Keywords"`, `"Macro Keywords"`, `"Types"`), but the
// body at `:41-44` binds FOUR wordlists (`keywordlists[0..=3]`):
//
//   class 0 → `keywords` — dispatched to `SCE_SAS_MACRO_KEYWORD`
//     ONLY in macro context (`:73-75`). SAS macro-language
//     directives entered via `%` (e.g. `%macro` / `%mend` / `%let`
//     / `%do` / `%if` / `%then` / `%else` / `%end` / `%while` /
//     `%goto` / `%return` / `%put` / `%global` / `%local` /
//     `%include` / `%abort` / `%input` / `%sysexec` / `%syscall`
//     / `%symdel`).
//   class 1 → `blockKeywords` — dispatched to
//     `SCE_SAS_BLOCK_KEYWORD` in BOTH macro and identifier contexts
//     (`:76-78, :92-94`). DATA-step and PROC-step introducers /
//     terminators (`data` / `proc` / `run` / `quit` / `endsas`).
//   class 2 → `functionKeywords` — dispatched to
//     `SCE_SAS_MACRO_FUNCTION` ONLY in macro context (`:79-81`).
//     Macro built-in intrinsic functions (`%eval` / `%str` /
//     `%sysfunc` / `%index` / `%scan` / `%substr` / `%upcase`
//     etc.).
//   class 3 → `statements` — dispatched to `SCE_SAS_STATEMENT`
//     ONLY in identifier context (`:89-91`). DATA-step and PROC-
//     step statements (`set` / `merge` / `if` / `then` / `else`
//     / `where` / `keep` / `drop` / `retain` / `input` / `put`
//     / `format` / `informat` etc.).
//
// **Macro-wordlist entries include the `%` prefix.** The lexer
// enters `SCE_SAS_MACRO` state AT the `%` character
// (`:142-145` — `SetState` fires before `Forward`), and
// `GetCurrentLowered(s, sizeof(s))` at `:72` returns the full
// span from the `%` to the current non-word position. So wordlist
// classes 0 and 2 must contain byte-canonical `%name` forms
// (leading `%`, lowercase), NOT bareword forms. Class 1 fires in
// both macro and identifier contexts, so containing both `data`
// and `%data` would activate for both — but per SAS grammar
// `%data` is not idiomatic so we seed only bareword forms.
// Class 3 fires only in identifier context — bareword forms.
//
// **Case-INSENSITIVE identifier lookup.** `LexSAS.cxx:72, :88` call
// `sc.GetCurrentLowered(...)` before probing every wordlist.
// Wordlist entries MUST be byte-canonical lowercase — SAS itself
// is case-insensitive per language reference (`DATA` / `data` /
// `Data` all identify the same statement) so the lookup is
// permissive-correct.
//
// **Six unused-in-practice slots.** SciLexer.h declares
// `SCE_SAS_TYPE` (8), `SCE_SAS_WORD` (9), `SCE_SAS_GLOBAL_MACRO`
// (10) — no reference in `LexSAS.cxx` body (verified via
// `grep -n "SCE_SAS_TYPE\|SCE_SAS_WORD\|SCE_SAS_GLOBAL_MACRO"`,
// zero matches). Included for numeric-contiguity correctness at
// the FFI boundary; framework leaves them unmapped per the
// framework's declared-but-unemitted convention.
// `SCE_SAS_IDENTIFIER` (6) and `SCE_SAS_MACRO` (11) are transient
// collect states that can survive to paint when the token doesn't
// match any wordlist — the "unknown bareword" (IDENTIFIER) and
// "unknown %macro-reference" (MACRO) fall-through paths. Framework
// leaves IDENTIFIER unmapped (default paint) and routes MACRO to
// `StyleSlot::Macro` (user macro invocation is semantically a
// macro-call archetype). `SCE_SAS_DEFAULT` (0) also unmapped per
// framework default-slot convention.
//
// **Three comment flavours share `StyleSlot::Comment`.** SAS
// supports three comment forms:
//   `SCE_SAS_COMMENT` (1) — legacy `* text ;` comment at line
//     start (`:128-130` — fires only when `!lineHasNonCommentChar`).
//   `SCE_SAS_COMMENTLINE` (2) — `// text ;` line comment
//     (`:135-137`).
//   `SCE_SAS_COMMENTBLOCK` (3) — `/* text */` block comment
//     (`:131-134`).
// All three collapse to Comment — same discipline as C's line/block
// unification and Rust's `SCE_RUST_COMMENTBLOCK` /
// `_COMMENTBLOCKDOC` / `_COMMENTLINE` / `_COMMENTLINEDOC` collapse.
//
// **Style-slot routing rationale.**
//   `MACRO_KEYWORD` (12) → Preprocessor — SAS macro-language
//     directives (`%do` / `%mend` / `%if`) are semantically
//     equivalent to C's `#define` / `#if` preprocessor control
//     flow; same accent family as PHP's `SCE_HPHP_COMMENT` /
//     `SCE_HPHP_WORD` preprocessor-family collapse.
//   `BLOCK_KEYWORD` (13) → Keyword — DATA / PROC / RUN are the
//     primary structural keywords of the language (equivalent to
//     C's `int` / `if` / `while`); primary-keyword accent.
//   `MACRO_FUNCTION` (14) → Preprocessor — macro-language
//     intrinsics (`%eval` / `%str` / `%sysfunc`) share their host
//     namespace with `MACRO_KEYWORD` and are semantically part of
//     the same macro-preprocessor accent family.
//   `STATEMENT` (15) → Keyword2 — in-step statements (`set` /
//     `if` / `where`) are secondary structural keywords living
//     inside a DATA / PROC container; secondary-keyword accent
//     matches `SCE_C_WORD2` / `SCE_PY_WORD2` precedent.
pub const SCLEX_SAS: usize = 125;
pub const SCE_SAS_DEFAULT: usize = 0;
pub const SCE_SAS_COMMENT: usize = 1;
pub const SCE_SAS_COMMENTLINE: usize = 2;
pub const SCE_SAS_COMMENTBLOCK: usize = 3;
pub const SCE_SAS_NUMBER: usize = 4;
pub const SCE_SAS_OPERATOR: usize = 5;
pub const SCE_SAS_IDENTIFIER: usize = 6;
pub const SCE_SAS_STRING: usize = 7;
pub const SCE_SAS_TYPE: usize = 8;
pub const SCE_SAS_WORD: usize = 9;
pub const SCE_SAS_GLOBAL_MACRO: usize = 10;
pub const SCE_SAS_MACRO: usize = 11;
pub const SCE_SAS_MACRO_KEYWORD: usize = 12;
pub const SCE_SAS_BLOCK_KEYWORD: usize = 13;
pub const SCE_SAS_MACRO_FUNCTION: usize = 14;
pub const SCE_SAS_STATEMENT: usize = 15;

// LexErrorList style indices. 43 defined slots split across two
// numeric ranges (0..=26 + 40..=55), skipping 27..=39 which
// SciLexer.h and the LexicalClass table at
// `vendor/lexilla/lexers/LexErrorList.cxx:92-104` mark as "unused"
// (27..=31) and "predefined" (32..=39, framework-reserved for
// internal use). Cross-referenced against
// `vendor/lexilla/include/SciLexer.h:525-568` and the
// `LexerModule lmErrorList(SCLEX_ERRORLIST, ...)` registration at
// `vendor/lexilla/lexers/LexErrorList.cxx:572`.
//
// Dispatches SCLEX_ERRORLIST (= 10, per `SciLexer.h:28`) — a
// legacy lexer by Neil Hodgson himself (1998-2001, per
// `LexErrorList.cxx:5`), historically used for SciTE's Output
// pane. Notepad++ (and Code++) exposes it as a manually-selectable
// language for viewing compiler / linter / interpreter diagnostic
// output. No default file extension per `LANG_TABLE`.
//
// **ZERO wordlist classes** — `emptyWordListDesc[]` at
// `LexErrorList.cxx:44-46` is `{nullptr}`, and the
// `LexerModule lmErrorList(..., emptyWordListDesc)` registration
// passes it directly. All classification is line-pattern-based
// per `RecogniseErrorListLine` at `:232-452` — no keyword lookup
// whatsoever. Framework consumes zero-wordlist lexers via a
// dedicated theme (same pattern as `TXT2TAGS_THEME` / PROPS /
// LATEX / REGISTRY / TeHex) — theme.keywords = &[].
//
// **Three numeric ranges** — the state space is discontiguous:
//
//   0..=26 (27 states) — the "classic" diagnostic set.
//     DEFAULT (0) is unclassified; states 1..=26 are one-per-tool
//     diagnostic patterns (Python / GCC / Microsoft / Borland /
//     Perl / .NET / Lua / PHP / Fortran family / Java stack /
//     Bash + supporting VALUE / GCC_INCLUDED_FROM / GCC_EXCERPT
//     / CTAG / CMD + four DIFF markers + two ESCSEQ states).
//
//   27..=31 (5 slots) — "unused" per LexicalClass. Framework
//     never emits these; deliberately omitted from `SCE_ERR_*`
//     constants.
//
//   32..=39 (8 slots) — "predefined" per LexicalClass. These
//     are Scintilla framework-reserved indices (STYLE_* range —
//     SciLexer.h uses 32..=39 for DEFAULT / LINENUMBER /
//     BRACELIGHT / BRACEBAD / CONTROLCHAR / INDENTGUIDE /
//     CALLTIP / FOLDDISPLAYTEXT globally). Not emitted by
//     LexErrorList; deliberately omitted from `SCE_ERR_*`
//     constants.
//
//   40..=55 (16 states) — ANSI escape-sequence color states,
//     one per ANSI CSI m color code. When the lexer detects
//     `\033[NNm` sequences and `escape.sequences` property is
//     enabled (`LexErrorList.cxx:39-42, :460-497`), the color
//     numeric maps: ANSI 30..=37 (dim black/red/green/brown/blue/
//     magenta/cyan/gray) → SCE_ERR_ES_BLACK..=GRAY (40..=47);
//     ANSI 90..=97 (bright) or (30..=37 + bold=1) →
//     SCE_ERR_ES_DARK_GRAY..=WHITE (48..=55). Framework leaves
//     these UNMAPPED — the semantic IS "text in specific ANSI
//     color N", and our `StyleSlot` enum has no ANSI-color slot.
//     Mapping any of them to a `StyleSlot` variant would collapse
//     the 16 distinct semantic colors into one visual color,
//     defeating the purpose. Users viewing colored terminal
//     output will see the escape sequences painted (via
//     SCE_ERR_ESCSEQ = Operator) but subsequent text falls back
//     to STYLE_DEFAULT. This is an architectural limitation of
//     the current slot set; extending StyleSlot with ANSI-color
//     slots is deferred future work.
//
// **Diagnostic-family collapse.** 15 tool-specific diagnostic
// states (PYTHON / GCC / MS / BORLAND / PERL / NET / LUA / PHP /
// ELF / IFC / IFORT / ABSF / TIDY / JAVA_STACK / BASH) all
// semantically mean "error/warning line from tool X." Framework
// routes them all to `StyleSlot::Keyword` (primary structural
// attention) — user's eye needs to pick error lines out of
// surrounding output, regardless of which tool produced them.
// The tool-specific distinction (which lexer state fired) is
// preserved at the lexer level but visually collapsed.
//
// **Context / navigation states → `StyleSlot::Keyword2`.** CTAG
// (9 — CTags navigation records), VALUE (21 — matched text /
// message part of GCC error), GCC_INCLUDED_FROM (22 — GCC
// include-path context "In file included from X:5"), and
// GCC_EXCERPT (25 — GCC "73 |   code here" excerpt lines). All
// four are "supporting information adjacent to a diagnostic"
// archetype — Keyword2's secondary-anchor semantics fit.
//
// **CMD (4) → `StyleSlot::Preprocessor`.** SciTE's Output pane
// prefixes command echoes with `>`; the lexer paints this state
// on any `>`-leading line (`LexErrorList.cxx:237-239`).
// Semantically "system directive that produced this output" —
// Preprocessor accent matches the archetype.
//
// **Four DIFF states → distinct semantic slots.**
//   DIFF_ADDITION (11) → `StyleSlot::String` — added-line marker
//     (`+`-prefixed). String's typical green tint matches diff
//     UI convention for additions.
//   DIFF_DELETION (12) → `StyleSlot::Macro` — deleted-line
//     marker (`-`-prefixed or `<`-prefixed). Macro's typical
//     red-orange accent matches diff UI convention for
//     deletions.
//   DIFF_CHANGED (10) → `StyleSlot::Number` — changed-line
//     marker (`!`-prefixed). Number's yellow-ish accent matches
//     diff UI convention for modifications.
//   DIFF_MESSAGE (13) → `StyleSlot::Comment` — diff header
//     (`---`/`+++`-prefixed). Metadata annotation archetype —
//     Comment italic matches the "file-level annotation" role.
//
// **Two ESCSEQ states → `StyleSlot::Operator`.** ESCSEQ (23) is
// a RECOGNISED ANSI escape sequence (e.g. `\033[31m`); ESCSEQ_
// UNKNOWN (24) is an unrecognised or malformed sequence. Both
// are terminal control markers — Operator's accent matches the
// "syntactic delimiter" role. Same slot for both because the
// distinction is "did the color-parse succeed" — the sequences
// themselves paint identically; only the following-text color
// varies (which is where the SCE_ERR_ES_* states then take
// over).
pub const SCLEX_ERRORLIST: usize = 10;
pub const SCE_ERR_DEFAULT: usize = 0;
pub const SCE_ERR_PYTHON: usize = 1;
pub const SCE_ERR_GCC: usize = 2;
pub const SCE_ERR_MS: usize = 3;
pub const SCE_ERR_CMD: usize = 4;
pub const SCE_ERR_BORLAND: usize = 5;
pub const SCE_ERR_PERL: usize = 6;
pub const SCE_ERR_NET: usize = 7;
pub const SCE_ERR_LUA: usize = 8;
pub const SCE_ERR_CTAG: usize = 9;
pub const SCE_ERR_DIFF_CHANGED: usize = 10;
pub const SCE_ERR_DIFF_ADDITION: usize = 11;
pub const SCE_ERR_DIFF_DELETION: usize = 12;
pub const SCE_ERR_DIFF_MESSAGE: usize = 13;
pub const SCE_ERR_PHP: usize = 14;
pub const SCE_ERR_ELF: usize = 15;
pub const SCE_ERR_IFC: usize = 16;
pub const SCE_ERR_IFORT: usize = 17;
pub const SCE_ERR_ABSF: usize = 18;
pub const SCE_ERR_TIDY: usize = 19;
pub const SCE_ERR_JAVA_STACK: usize = 20;
pub const SCE_ERR_VALUE: usize = 21;
pub const SCE_ERR_GCC_INCLUDED_FROM: usize = 22;
pub const SCE_ERR_ESCSEQ: usize = 23;
pub const SCE_ERR_ESCSEQ_UNKNOWN: usize = 24;
pub const SCE_ERR_GCC_EXCERPT: usize = 25;
pub const SCE_ERR_BASH: usize = 26;
// Slots 27..=39 intentionally omitted: 27..=31 are "unused"
// per `LexErrorList.cxx:92-96`, 32..=39 are "predefined"
// framework-reserved indices per `LexErrorList.cxx:97-104`.
pub const SCE_ERR_ES_BLACK: usize = 40;
pub const SCE_ERR_ES_RED: usize = 41;
pub const SCE_ERR_ES_GREEN: usize = 42;
pub const SCE_ERR_ES_BROWN: usize = 43;
pub const SCE_ERR_ES_BLUE: usize = 44;
pub const SCE_ERR_ES_MAGENTA: usize = 45;
pub const SCE_ERR_ES_CYAN: usize = 46;
pub const SCE_ERR_ES_GRAY: usize = 47;
pub const SCE_ERR_ES_DARK_GRAY: usize = 48;
pub const SCE_ERR_ES_BRIGHT_RED: usize = 49;
pub const SCE_ERR_ES_BRIGHT_GREEN: usize = 50;
pub const SCE_ERR_ES_YELLOW: usize = 51;
pub const SCE_ERR_ES_BRIGHT_BLUE: usize = 52;
pub const SCE_ERR_ES_BRIGHT_MAGENTA: usize = 53;
pub const SCE_ERR_ES_BRIGHT_CYAN: usize = 54;
pub const SCE_ERR_ES_WHITE: usize = 55;

// SCN_* notification codes (delivered via WM_NOTIFY's NMHDR.code) are added
// when Phase 2+ first dispatches them. Each constant must be cross-checked
// against `vendor/scintilla/include/Scintilla.h` at the time of addition;
// numeric values must not be guessed.

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Sci_RangeToFormatFull` layout is ABI-critical — Scintilla's
    /// `SCI_FORMATRANGEFULL` dispatch reads fields by fixed byte offset,
    /// so a Rust padding change would silently mis-render or crash. Pin
    /// the field offsets and total size against what the C header
    /// (`vendor/scintilla/include/Scintilla.h:1387-1393`) produces on
    /// this target.
    ///
    /// Values hold on any 64-bit target — verified for
    /// `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, and
    /// `x86_64-unknown-linux-gnu`. The layout depends on pointer width,
    /// not on the OS: every field is either a pointer, an `i32`, or an
    /// `isize` (`Sci_Position`), so the struct is identical across
    /// backends at a given width. Hence the `target_pointer_width`
    /// gate rather than a per-OS one — a future 32-bit build would need
    /// its own expectations, and should get its own arm here rather
    /// than silently failing this one.
    ///
    /// 64-bit `void*`, 64-bit `Sci_Position`, natural alignment:
    ///   `hdc`         at 0   ( 8 bytes)
    ///   `hdc_target`  at 8   ( 8 bytes)
    ///   `rc`          at 16  (16 bytes)
    ///   `rc_page`     at 32  (16 bytes)
    ///   `chrg`        at 48  (16 bytes)
    ///   size          = 64
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn range_to_format_full_layout_matches_c_abi() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<Sci_RangeToFormatFull>(), 64);
        assert_eq!(align_of::<Sci_RangeToFormatFull>(), 8);
        assert_eq!(offset_of!(Sci_RangeToFormatFull, hdc), 0);
        assert_eq!(offset_of!(Sci_RangeToFormatFull, hdc_target), 8);
        assert_eq!(offset_of!(Sci_RangeToFormatFull, rc), 16);
        assert_eq!(offset_of!(Sci_RangeToFormatFull, rc_page), 32);
        assert_eq!(offset_of!(Sci_RangeToFormatFull, chrg), 48);
    }

    #[test]
    fn sci_rectangle_layout_matches_c_abi() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<Sci_Rectangle>(), 16);
        assert_eq!(align_of::<Sci_Rectangle>(), 4);
        assert_eq!(offset_of!(Sci_Rectangle, left), 0);
        assert_eq!(offset_of!(Sci_Rectangle, top), 4);
        assert_eq!(offset_of!(Sci_Rectangle, right), 8);
        assert_eq!(offset_of!(Sci_Rectangle, bottom), 12);
    }

    #[test]
    fn sci_character_range_full_layout_matches_c_abi() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<Sci_CharacterRangeFull>(), 16);
        assert_eq!(align_of::<Sci_CharacterRangeFull>(), 8);
        assert_eq!(offset_of!(Sci_CharacterRangeFull, cp_min), 0);
        assert_eq!(offset_of!(Sci_CharacterRangeFull, cp_max), 8);
    }
}

/// The FFI smoke test DESIGN.md §5.6 requires of this crate: link a real
/// Scintilla, create a real instance, push text in and read it back.
/// Catches build and link regressions that no amount of constant-checking
/// would — a stale object file, a missing source in `build.rs`, or a
/// calling-convention mismatch all surface here and nowhere else.
///
/// Requires a display, because `scintilla_new` builds a `GtkWidget` and
/// GTK refuses to initialise without one. `#[ignore]` rather than a
/// runtime display probe on purpose: a probe that skips silently reads
/// as a pass in CI output, which is exactly the failure mode this test
/// exists to prevent. Run it explicitly:
///
/// ```text
/// cargo test -p codepp-scintilla-sys -- --ignored          # on a desktop
/// xvfb-run cargo test -p codepp-scintilla-sys -- --ignored  # headless
/// ```
#[cfg(all(test, target_os = "linux"))]
mod gtk_ffi_smoke {
    use super::{
        scintilla_new, scintilla_send_message, SCI_GETLENGTH, SCI_GETTEXT, SCI_INSERTTEXT,
    };
    use core::ffi::{c_char, c_int};

    extern "C" {
        /// GTK's fallible initialiser — returns 0 when no display is
        /// reachable, where `gtk_init` would abort the process and take
        /// the whole test binary with it.
        fn gtk_init_check(argc: *mut c_int, argv: *mut *mut *mut c_char) -> c_int;
    }

    /// Payload for the round trip. Non-ASCII on purpose: a UTF-8
    /// multi-byte sequence would be mangled by a byte-vs-character
    /// length confusion anywhere in the FFI chain, so it makes the
    /// length assertions below meaningfully stronger than plain ASCII
    /// would. Trailing NUL because `SCI_INSERTTEXT` takes a C string.
    const ROUND_TRIP_TEXT: &[u8] = b"Code++ \xE2\x86\x92 Scintilla on GTK\0";

    #[test]
    #[ignore = "creates a GTK widget; needs a display (see module docs)"]
    fn scintilla_widget_round_trips_text() {
        // SAFETY: the null/null form is GTK's documented "no command
        // line to parse" call.
        let inited = unsafe { gtk_init_check(core::ptr::null_mut(), core::ptr::null_mut()) };
        assert_ne!(inited, 0, "gtk_init_check failed — no display available?");

        // SAFETY: GTK is initialised, which is `scintilla_new`'s only
        // precondition.
        let sci = unsafe { scintilla_new() };
        assert!(!sci.is_null(), "scintilla_new returned null");

        let text = ROUND_TRIP_TEXT;

        // SAFETY: `sci` is a live Scintilla widget; `text` is a
        // NUL-terminated buffer that outlives the call, which is what
        // SCI_INSERTTEXT's `lparam` requires.
        unsafe {
            scintilla_send_message(sci, SCI_INSERTTEXT, 0, text.as_ptr() as isize);
        }

        // Round-trip through Scintilla's own accessors rather than
        // trusting the insert: `SCI_GETLENGTH` proves the document
        // actually holds the bytes, not just that the call returned.
        // SAFETY: same widget, pure query.
        let len = unsafe { scintilla_send_message(sci, SCI_GETLENGTH, 0, 0) };
        let expected_len = text.len() - 1; // minus the NUL
        assert_eq!(
            usize::try_from(len).expect("SCI_GETLENGTH returned a negative length"),
            expected_len,
            "document length disagrees with the inserted byte count"
        );

        // SCI_GETTEXT writes at most `wparam` bytes including the NUL it
        // appends, so ask for len + 1 and size the buffer to match.
        let mut buf = vec![0u8; expected_len + 1];
        // SAFETY: `buf` has room for `expected_len + 1` bytes, which is
        // exactly the cap handed to Scintilla as `wparam`.
        unsafe {
            scintilla_send_message(sci, SCI_GETTEXT, buf.len(), buf.as_mut_ptr() as isize);
        }
        buf.truncate(expected_len);
        assert_eq!(
            buf.as_slice(),
            &text[..expected_len],
            "text read back from Scintilla differs from what was inserted"
        );
    }
}
