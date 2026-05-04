//! Raw FFI to vendored Scintilla 5.x and Lexilla 5.x.
//!
//! Phase 1 surface: just enough to register the Scintilla Win32 window class,
//! call into Scintilla via `SendMessage`, and capture the direct-call
//! function pointer. The full message constant set lands progressively in
//! later phases.
//!
//! See DESIGN.md §4.1 (vendoring), §4.2 (direct-call API), §6 (plugin ABI).

#![cfg(target_os = "windows")]
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

// Lexilla's public C entry points are declared `__stdcall` on Win32
// (`LEXILLA_CALL` in `Lexilla.h`); on x64 Windows that resolves to the
// single Microsoft x64 calling convention so `extern "system"` ==
// `extern "C"`, but `system` is the convention-agnostic spelling and
// stays correct if/when we add an x86 build.
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

/// The Win32 window class name registered by `Scintilla_RegisterClasses`.
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
pub const SCI_DOCUMENTSTART: u32 = 2316;
pub const SCI_DOCUMENTEND: u32 = 2318;

// View toggles + zoom — driven by the View menu.
pub const SCI_SETWRAPMODE: u32 = 2268;
pub const SCI_GETWRAPMODE: u32 = 2269;
pub const SCI_SETVIEWWS: u32 = 2021;
pub const SCI_GETVIEWWS: u32 = 2020;
pub const SCI_SETVIEWEOL: u32 = 2356;
pub const SCI_GETVIEWEOL: u32 = 2355;
pub const SCI_ZOOMIN: u32 = 2333;
pub const SCI_ZOOMOUT: u32 = 2334;
pub const SCI_SETZOOM: u32 = 2373;
pub const SCI_GETZOOM: u32 = 2374;

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

// SCFIND_* search flag bits, OR'd into the wparam of
// SCI_SETSEARCHFLAGS. The numeric layout is the public ABI plugins
// use too — don't reshuffle.
pub const SCFIND_NONE: u32 = 0x0;
pub const SCFIND_WHOLEWORD: u32 = 0x2;
pub const SCFIND_MATCHCASE: u32 = 0x4;
pub const SCFIND_WORDSTART: u32 = 0x00100000;
pub const SCFIND_REGEXP: u32 = 0x00200000;
pub const SCFIND_CXX11REGEX: u32 = 0x00800000;

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

// `SCNotification.modificationType` flags for SCN_MODIFIED. The
// host filters on the text-changing flags (insert / delete) for
// `scrollWidth` recompute; the rest (style change, fold-level
// change, etc.) don't affect line widths.
pub const SC_MOD_INSERTTEXT: i32 = 0x1;
pub const SC_MOD_DELETETEXT: i32 = 0x2;

// History
pub const SCI_UNDO: u32 = 2176;
pub const SCI_REDO: u32 = 2011;
pub const SCI_CANUNDO: u32 = 2174;
pub const SCI_CANREDO: u32 = 2016;
pub const SCI_EMPTYUNDOBUFFER: u32 = 2175;

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
pub const SCI_STYLESETFORE: u32 = 2051;
pub const SCI_STYLESETBACK: u32 = 2052;
pub const SCI_STYLESETBOLD: u32 = 2053;
pub const SCI_STYLESETITALIC: u32 = 2054;
/// Apply STYLE_DEFAULT to all other styles. Useful as the first call
/// after switching lexers so the previous lexer's per-style colours
/// don't bleed through.
pub const SCI_STYLECLEARALL: u32 = 2050;
/// `STYLE_DEFAULT = 32` — the style index Scintilla uses as the
/// fallback for any text not classified by a lexer. Setting its
/// fore/back/font here is the way to set the editor's "default"
/// appearance.
pub const STYLE_DEFAULT: usize = 32;

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

// SCN_* notification codes (delivered via WM_NOTIFY's NMHDR.code) are added
// when Phase 2+ first dispatches them. Each constant must be cross-checked
// against `vendor/scintilla/include/Scintilla.h` at the time of addition;
// numeric values must not be guessed.
