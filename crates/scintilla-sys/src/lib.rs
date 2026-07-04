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
