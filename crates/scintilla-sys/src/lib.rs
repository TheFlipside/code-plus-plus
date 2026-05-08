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
pub const SCI_LINEFROMPOSITION: u32 = 2166;
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
pub const SCI_SETVIEWWS: u32 = 2021;
pub const SCI_GETVIEWWS: u32 = 2020;
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
/// Scintilla notification fired whenever the caret moves, the
/// selection changes, or any other UI-relevant state shifts. The
/// status bar's cursor / column / pos slots refresh on each
/// SCN_UPDATEUI so they track the live caret without needing a
/// separate timer.
pub const SCN_UPDATEUI: u32 = 2007;

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
pub const SCI_MARGINSETTEXT: u32 = 2530;
pub const SCI_MARGINSETSTYLE: u32 = 2532;
/// `SC_MARGIN_TEXT = 4` — the *type constant* that, when passed as
/// the `lparam` of `SCI_SETMARGINTYPEN`, makes the addressed margin
/// render per-line text supplied via `SCI_MARGINSETTEXT`, styled by
/// the index supplied via `SCI_MARGINSETSTYLE`. Used by Code++ to
/// render line numbers right-aligned within a fixed-width column —
/// the host formats each line's text with leading spaces so the
/// rightmost digit lands in the same column for every line.
pub const SC_MARGIN_TEXT: u32 = 4;

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
