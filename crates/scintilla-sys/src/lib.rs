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
// `SCE_B_DOCKEYWORD`) ARE declared in `SciLexer.h` but are emitted
// by sibling lexers (`LexBasic.cxx` for FreeBASIC / PureBasic /
// BlitzBasic, sharing the SCE_B_ namespace) — `LexVB` itself never
// emits them. Omitted here; add when those lexers are wired.
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

// SCN_* notification codes (delivered via WM_NOTIFY's NMHDR.code) are added
// when Phase 2+ first dispatches them. Each constant must be cross-checked
// against `vendor/scintilla/include/Scintilla.h` at the time of addition;
// numeric values must not be guessed.
