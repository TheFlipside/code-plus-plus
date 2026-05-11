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
/// ClearType / LCD-optimised rendering — the modern Windows
/// default and Code++'s `NPPM_SETSMOOTHFONT(TRUE)` choice.
pub const SC_EFF_QUALITY_LCD_OPTIMIZED: u32 = 3;
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
/// `SC_MARGIN_SYMBOL = 0` — type constant for a margin that
/// renders only markers (the `SC_MARKNUM_*` family). Code++ uses
/// this for the change-history strip: a 4-px margin whose only
/// content is the `SC_MARKNUM_HISTORY_MODIFIED` marker, painted
/// as a `SC_MARK_FULLRECT` orange bar.
pub const SC_MARGIN_SYMBOL: u32 = 0;

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

// LexPython style indices — the "P" prefix is upstream's choice for
// LexPython's enum. Style numbers verified against
// `vendor/lexilla/include/SciLexer.h` SCE_P_*.
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
pub const SCE_P_COMMENTBLOCK: usize = 12;
pub const SCE_P_STRINGEOL: usize = 13;
pub const SCE_P_WORD2: usize = 14;
pub const SCE_P_DECORATOR: usize = 15;
pub const SCE_P_FSTRING: usize = 16;
pub const SCE_P_FCHARACTER: usize = 17;
pub const SCE_P_FTRIPLE: usize = 18;
pub const SCE_P_FTRIPLEDOUBLE: usize = 19;

// LexJSON style indices.
pub const SCE_JSON_NUMBER: usize = 1;
pub const SCE_JSON_STRING: usize = 2;
pub const SCE_JSON_STRINGEOL: usize = 3;
pub const SCE_JSON_PROPERTYNAME: usize = 4;
pub const SCE_JSON_ESCAPESEQUENCE: usize = 5;
pub const SCE_JSON_LINECOMMENT: usize = 6;
pub const SCE_JSON_BLOCKCOMMENT: usize = 7;
pub const SCE_JSON_OPERATOR: usize = 8;
pub const SCE_JSON_KEYWORD: usize = 11;
pub const SCE_JSON_ERROR: usize = 13;

// LexBash (SH) style indices.
pub const SCE_SH_ERROR: usize = 1;
pub const SCE_SH_COMMENTLINE: usize = 2;
pub const SCE_SH_NUMBER: usize = 3;
pub const SCE_SH_WORD: usize = 4;
pub const SCE_SH_STRING: usize = 5;
pub const SCE_SH_CHARACTER: usize = 6;
pub const SCE_SH_OPERATOR: usize = 7;
pub const SCE_SH_SCALAR: usize = 9;
pub const SCE_SH_PARAM: usize = 10;
pub const SCE_SH_BACKTICKS: usize = 11;
pub const SCE_SH_HERE_DELIM: usize = 12;
pub const SCE_SH_HERE_Q: usize = 13;

// LexLua style indices.
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
pub const SCE_LUA_STRINGEOL: usize = 12;
pub const SCE_LUA_LABEL: usize = 20;

// LexSQL style indices.
pub const SCE_SQL_COMMENT: usize = 1;
pub const SCE_SQL_COMMENTLINE: usize = 2;
pub const SCE_SQL_COMMENTDOC: usize = 3;
pub const SCE_SQL_NUMBER: usize = 4;
pub const SCE_SQL_WORD: usize = 5;
pub const SCE_SQL_STRING: usize = 6;
pub const SCE_SQL_CHARACTER: usize = 7;
pub const SCE_SQL_OPERATOR: usize = 10;
pub const SCE_SQL_QUOTEDIDENTIFIER: usize = 23;

// LexYAML style indices.
pub const SCE_YAML_COMMENT: usize = 1;
pub const SCE_YAML_IDENTIFIER: usize = 2;
pub const SCE_YAML_KEYWORD: usize = 3;
pub const SCE_YAML_NUMBER: usize = 4;
pub const SCE_YAML_REFERENCE: usize = 5;
pub const SCE_YAML_DOCUMENT: usize = 6;
pub const SCE_YAML_TEXT: usize = 7;
pub const SCE_YAML_ERROR: usize = 8;
pub const SCE_YAML_OPERATOR: usize = 9;

// LexTOML style indices. The upstream enum also defines
// `SCE_TOML_ERROR` (7), `SCE_TOML_STRINGEOL` (15), and
// `SCE_TOML_ESCAPECHAR` (13) — those are intentionally omitted
// from the scaffolding because Phase 4.5's TOML theme will not
// colour them differently from the surrounding string / default
// styles. A future contributor wiring a custom error/EOL theme
// can add them back at their numeric values verbatim.
pub const SCE_TOML_COMMENT: usize = 1;
pub const SCE_TOML_IDENTIFIER: usize = 2;
pub const SCE_TOML_KEYWORD: usize = 3;
pub const SCE_TOML_NUMBER: usize = 4;
pub const SCE_TOML_TABLE: usize = 5;
pub const SCE_TOML_KEY: usize = 6;
pub const SCE_TOML_OPERATOR: usize = 8;
pub const SCE_TOML_STRING_SQ: usize = 9;
pub const SCE_TOML_STRING_DQ: usize = 10;
pub const SCE_TOML_TRIPLE_STRING_SQ: usize = 11;
pub const SCE_TOML_TRIPLE_STRING_DQ: usize = 12;
pub const SCE_TOML_DATETIME: usize = 14;

// LexCSS style indices.
pub const SCE_CSS_TAG: usize = 1;
pub const SCE_CSS_CLASS: usize = 2;
pub const SCE_CSS_PSEUDOCLASS: usize = 3;
pub const SCE_CSS_OPERATOR: usize = 5;
pub const SCE_CSS_IDENTIFIER: usize = 6;
pub const SCE_CSS_VALUE: usize = 8;
pub const SCE_CSS_COMMENT: usize = 9;
pub const SCE_CSS_ID: usize = 10;
pub const SCE_CSS_IMPORTANT: usize = 11;
pub const SCE_CSS_DIRECTIVE: usize = 12;
pub const SCE_CSS_DOUBLESTRING: usize = 13;
pub const SCE_CSS_SINGLESTRING: usize = 14;
pub const SCE_CSS_ATTRIBUTE: usize = 16;
pub const SCE_CSS_VARIABLE: usize = 23;

// SCN_* notification codes (delivered via WM_NOTIFY's NMHDR.code) are added
// when Phase 2+ first dispatches them. Each constant must be cross-checked
// against `vendor/scintilla/include/Scintilla.h` at the time of addition;
// numeric values must not be guessed.
