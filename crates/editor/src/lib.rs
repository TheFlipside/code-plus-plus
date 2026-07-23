//! Safe Scintilla wrapper for Code++.
//!
//! Phase 1: `EditorHandle` carries the raw native control handle plus
//! the direct-call `(fn_ptr, instance_ptr)` pair captured once at
//! construction. Hot operations route through `send` (the direct call);
//! window-managed ones still use `SendMessage` from the UI crate. See
//! DESIGN.md §4.2.
//!
//! # Portability
//!
//! This crate contains no platform code — it holds three opaque
//! pointers and translates Rust types into Scintilla's
//! `wparam`/`lparam` shapes. The handle's first field is a Win32
//! `HWND` on Windows and a `GtkWidget*` on GTK; `editor` never
//! dereferences it, so the same code serves both.
//!
//! Exactly two things carry a `#[cfg]`, both for concrete
//! link-level reasons rather than any behavioural difference:
//!
//! - `EditorHandle::from_gtk_widget` (Linux) — the per-backend
//!   construction path; the Win32 side captures the same pair via
//!   `SendMessage` in `ui_win32` and calls [`EditorHandle::new`].
//! - `EditorHandle::set_lexer_by_name` (Windows + Linux) — the only
//!   method that references a Lexilla symbol, and Lexilla is only
//!   built on targets with a Scintilla backend.
//!
//! # Allowed pedantic lints, with rationale
//!
//! - `clippy::cast_possible_truncation`
//! - `clippy::cast_possible_wrap`
//! - `clippy::cast_sign_loss`
//!
//! This crate's job is to translate between Rust types and
//! Scintilla's `wparam`/`lparam`/`sptr_t` shapes — every one of
//! those is a deliberate `as` cast between integer widths, and
//! the Scintilla ABI semantics (documented in `Scintilla.h`)
//! gate the value range, not Rust's type system. Marking each
//! cast `#[allow(...)]` individually would add ~30 attribute
//! lines to a thin wrapper crate with no real reader-defence
//! value; the inner attributes here document the trade-off once.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

// Per-language lexer themes. Gated to the targets that build Lexilla,
// for the same reason `set_lexer_by_name` is — the module's entry
// point calls it, so on a target with an empty Scintilla archive the
// whole table is unreachable anyway.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod theme;

use core::ffi::c_void;

// Lexilla is only built on the backends that have a Scintilla build,
// so `CreateLexer` exists on exactly those targets — see the gate on
// its declaration in `scintilla-sys`. Imported separately so the
// dependency is visible rather than buried in the bulk list below.
#[cfg(any(target_os = "windows", target_os = "linux"))]
use codepp_scintilla_sys::CreateLexer;
use codepp_scintilla_sys::{
    sptr_t, uptr_t, ScintillaDirectFunction, SCI_BRACEBADLIGHT, SCI_BRACEHIGHLIGHT, SCI_BRACEMATCH,
    SCI_GETCHARAT, SCI_GETCURRENTPOS, SCI_GETENDSTYLED, SCI_GETRANGEPOINTER, SCI_GETTARGETEND,
    SCI_GETTARGETSTART, SCI_LINEFROMPOSITION, SCI_MARKERDEFINE, SCI_MARKERENABLEHIGHLIGHT,
    SCI_MARKERSETBACK, SCI_MARKERSETBACKSELECTED, SCI_MARKERSETFORE, SCI_POSITIONFROMLINE,
    SCI_REPLACETARGET, SCI_REPLACETARGETRE, SCI_SEARCHANCHOR, SCI_SEARCHINTARGET, SCI_SEARCHNEXT,
    SCI_SEARCHPREV, SCI_SETAUTOMATICFOLD, SCI_SETCARETLINEBACK, SCI_SETCARETLINEVISIBLE,
    SCI_SETCHANGEHISTORY, SCI_SETFOLDFLAGS, SCI_SETFOLDMARGINCOLOUR, SCI_SETFOLDMARGINHICOLOUR,
    SCI_SETILEXER, SCI_SETKEYWORDS, SCI_SETMARGINMASKN, SCI_SETMARGINSENSITIVEN,
    SCI_SETMARGINTYPEN, SCI_SETMARGINWIDTHN, SCI_SETPROPERTY, SCI_SETSEARCHFLAGS, SCI_SETSTYLING,
    SCI_SETTARGETRANGE, SCI_STARTSTYLING, SCI_STYLECLEARALL, SCI_STYLESETBACK, SCI_STYLESETBOLD,
    SCI_STYLESETFONT, SCI_STYLESETFORE, SCI_STYLESETITALIC, SCI_STYLESETSIZE,
    SCI_STYLESETUNDERLINE, SCI_TEXTWIDTH, SC_CHANGE_HISTORY_ENABLED, SC_CHANGE_HISTORY_MARKERS,
    SC_MARGIN_NUMBER, SC_MARGIN_SYMBOL, SC_MARKNUM_HISTORY_MODIFIED,
    SC_MARKNUM_HISTORY_REVERTED_TO_MODIFIED, SC_MARKNUM_HISTORY_REVERTED_TO_ORIGIN,
    SC_MARKNUM_HISTORY_SAVED, SC_MARK_EMPTY, SC_MARK_FULLRECT, STYLE_LINENUMBER,
};

/// Opaque handle to a Scintilla editor control.
///
/// Holds three captured-at-construction values:
///   - `hwnd` — the Scintilla child window handle (for `SendMessage`-style
///     calls and window-management ops).
///   - `direct_fn` — the function pointer returned by
///     `SCI_GETDIRECTFUNCTION` for this control.
///   - `direct_ptr` — the matching instance pointer returned by
///     `SCI_GETDIRECTPOINTER` for this control.
///
/// `direct_fn(direct_ptr, msg, wparam, lparam)` is equivalent to
/// `SendMessage(hwnd, msg, wparam, lparam)` but skips the Win32 message
/// queue. Used for every per-keystroke operation; this is the speed path.
#[derive(Clone, Copy)]
pub struct EditorHandle {
    hwnd: *mut c_void,
    direct_fn: ScintillaDirectFunction,
    direct_ptr: *mut c_void,
}

// SAFETY: the handle is logically opaque and the pointers it carries are
// stable for the lifetime of the underlying Scintilla control. Cross-thread
// use must still go through the UI-thread marshaling channel (DESIGN.md
// §5.4); these traits exist so the handle can live in `Send`/`Sync` state
// containers without unsafe wrapping at every site.
unsafe impl Send for EditorHandle {}
unsafe impl Sync for EditorHandle {}

impl EditorHandle {
    /// Construct from raw parts captured by the caller.
    ///
    /// The caller — typically a `ui_*` crate — is responsible for:
    ///   - creating a real Scintilla control (window class
    ///     `SCINTILLA_CLASS_NAME` registered via
    ///     `Scintilla_RegisterClasses`),
    ///   - calling `SendMessage(hwnd, SCI_GETDIRECTFUNCTION, 0, 0)` to
    ///     obtain the function pointer, and
    ///   - calling `SendMessage(hwnd, SCI_GETDIRECTPOINTER, 0, 0)` to
    ///     obtain the instance pointer.
    ///
    /// GTK callers should prefer `Self::from_gtk_widget`, which does
    /// the equivalent capture through `scintilla_send_message`. Plain
    /// code span rather than an intra-doc link on purpose: that method
    /// is `#[cfg(target_os = "linux")]`, so a link here would be
    /// unresolvable — and a `rustdoc::broken_intra_doc_links` warning —
    /// on every `cargo doc` run targeting Windows or macOS.
    ///
    /// # Safety
    ///
    /// `direct_fn` and `direct_ptr` must be the matching pair returned by
    /// the two messages above for this exact `hwnd`. Calling `send` with
    /// mismatched values is undefined behaviour.
    #[inline]
    pub unsafe fn new(
        hwnd: *mut c_void,
        direct_fn: ScintillaDirectFunction,
        direct_ptr: *mut c_void,
    ) -> Self {
        Self {
            hwnd,
            direct_fn,
            direct_ptr,
        }
    }

    /// Capture the direct-call pair from a Scintilla `GtkWidget*` and
    /// build a handle around it.
    ///
    /// The GTK counterpart of the `SendMessage`-based capture documented
    /// on [`Self::new`]: `scintilla_send_message` is the GTK backend's
    /// message entry point, and `SCI_GETDIRECTFUNCTION` /
    /// `SCI_GETDIRECTPOINTER` are handled there exactly as they are on
    /// Win32 (`vendor/scintilla/gtk/ScintillaGTK.cxx`). Once captured,
    /// every hot-path call goes through [`Self::send`] and never touches
    /// GTK again — the §4.2 speed path is identical on both backends.
    ///
    /// Returns `None` if Scintilla hands back a null function or
    /// instance pointer, which would mean `widget` is not a Scintilla
    /// widget. Callers must treat that as a fatal setup error rather
    /// than continuing with a half-built editor.
    ///
    /// # Safety
    ///
    /// `widget` must be a live, non-null pointer returned by
    /// `scintilla_new()` and not yet destroyed. Passing any other
    /// pointer is undefined behaviour — `scintilla_send_message`
    /// casts it to a `ScintillaObject` without validation.
    ///
    /// **The obligation continues after this returns.** `EditorHandle`
    /// is `Copy` and has no `Drop`; it stores raw pointers into the
    /// widget without expressing a lifetime, so nothing stops a copy
    /// from outliving what it points at. The caller must keep the
    /// widget alive for as long as *any* copy of the returned handle
    /// might still be used. Destroying it leaves every copy's
    /// `direct_ptr` dangling, and the next [`Self::send`] calls
    /// through it.
    ///
    /// Both backends discharge that by never destroying the view at
    /// all: one Scintilla widget is created at startup and lives for
    /// the process, with tabs switched underneath it by
    /// `SCI_SETDOCPOINTER` (DESIGN.md §7.2). A backend that instead
    /// gave each tab its own widget would have to tie the handle's
    /// lifetime to the widget — closing a tab would otherwise finalise
    /// the widget while other code still held a copy. That is a real
    /// design constraint on any future backend, not a stylistic
    /// preference; a `ui_cocoa` that reaches for one `NSView` per tab
    /// inherits the problem the single-view model avoids.
    ///
    /// Non-null is necessary but **not sufficient**, and the gap is
    /// not checkable from Rust. `scintilla_init` wraps its
    /// `new ScintillaGTK(...)` in a `catch (...)`, so a throwing
    /// constructor — realistically an allocation failure during widget
    /// setup — leaves a fully-formed `GtkWidget` whose interior
    /// `pscin` is still null, and `scintilla_new` returns it anyway.
    /// `scintilla_send_message` then dereferences that null without a
    /// guard, so the first call below would fault inside vendored C++
    /// *before* the `raw_fn == 0` check further down could reject it.
    /// The `None` return therefore covers "not a Scintilla widget",
    /// not "a Scintilla widget that failed to construct".
    ///
    /// In practice this needs local memory exhaustion at window-
    /// construction time; it is not reachable from file contents or
    /// plugin input, and it faults rather than corrupting memory. It
    /// is called out because the obvious reading of "non-null implies
    /// usable" is wrong here, and because the fix would have to live
    /// in vendored source that DESIGN.md §4.1 keeps unforked.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub unsafe fn from_gtk_widget(widget: *mut c_void) -> Option<Self> {
        use codepp_scintilla_sys::{
            scintilla_send_message, SCI_GETDIRECTFUNCTION, SCI_GETDIRECTPOINTER,
        };

        // SAFETY: the caller guarantees `widget` is a live Scintilla
        // widget; both messages are pure queries with no side effects.
        let (raw_fn, direct_ptr) = unsafe {
            (
                scintilla_send_message(widget, SCI_GETDIRECTFUNCTION, 0, 0),
                scintilla_send_message(widget, SCI_GETDIRECTPOINTER, 0, 0),
            )
        };
        if raw_fn == 0 || direct_ptr == 0 {
            return None;
        }

        // SAFETY: a non-zero `SCI_GETDIRECTFUNCTION` result is, by
        // Scintilla's contract, a pointer to `ScintillaGTK::DirectFunction`,
        // whose C++ signature is exactly `ScintillaDirectFunction`.
        let direct_fn: ScintillaDirectFunction =
            unsafe { core::mem::transmute::<usize, ScintillaDirectFunction>(raw_fn as usize) };

        // SAFETY: the pair was just captured together from this one widget,
        // which is precisely `new`'s requirement.
        Some(unsafe { Self::new(widget, direct_fn, direct_ptr as *mut c_void) })
    }

    /// Direct-call into Scintilla. The hot path — every keystroke, every
    /// selection update, every style query goes through this.
    ///
    /// `send` is intentionally NOT `#[must_use]` even though it
    /// returns `sptr_t`: Scintilla messages are dual-purpose — query
    /// messages (e.g. `SCI_GETLENGTH`) consume the return, but
    /// setter / action messages (`SCI_SETTEXT`, `SCI_INSERTTEXT`,
    /// `SCI_COLOURISE`, …) ignore it. Marking it must-use would
    /// force `let _ = self.send(...)` at every setter call site
    /// across the editor / `ui_win32` hot path — code-churn for no
    /// real reader-defence value.
    #[inline]
    #[allow(clippy::must_use_candidate)]
    pub fn send(&self, msg: u32, wparam: uptr_t, lparam: sptr_t) -> sptr_t {
        // SAFETY: `direct_fn` and `direct_ptr` were captured together from a
        // real Scintilla control via `SCI_GETDIRECTFUNCTION` /
        // `SCI_GETDIRECTPOINTER` (enforced by the `unsafe` constructor).
        unsafe { (self.direct_fn)(self.direct_ptr, msg, wparam, lparam) }
    }

    /// The underlying Scintilla `HWND` (as `*mut c_void` to keep this crate
    /// free of `windows`-crate types).
    #[inline]
    #[must_use]
    pub fn hwnd(&self) -> *mut c_void {
        self.hwnd
    }

    /// Attach the Lexilla lexer registered under `name` (e.g. `"cpp"`,
    /// `"rust"`) to this Scintilla view. Returns `false` if Lexilla
    /// has no lexer with that name (the static build in `build.rs`
    /// only links a small subset; `core::lang::LangType::lexer_name`
    /// is the source of truth for which names resolve).
    ///
    /// Calling with the same name twice is a no-op as far as the
    /// caller can observe — Scintilla releases the prior `ILexer5*`
    /// before adopting the new one. Calling with `name = ""` is
    /// **not** the way to detach a lexer; use [`Self::clear_lexer`]
    /// instead, which sends `SCI_SETILEXER(0, 0)` per the documented
    /// Scintilla contract.
    ///
    /// Available only where Lexilla is actually built — Windows and
    /// Linux today. On a target whose `build.rs` arm produces an empty
    /// archive (macOS, until the Cocoa backend lands) this method does
    /// not exist, so a premature caller fails to compile instead of
    /// failing to link. [`Self::clear_lexer`] has no such gate: it is
    /// a plain `SCI_SETILEXER(0, 0)` and touches no Lexilla symbol.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    #[must_use]
    pub fn set_lexer_by_name(&self, name: &str) -> bool {
        // CreateLexer needs a NUL-terminated `char*`. Build the buffer
        // on the stack for short names (every lexer name in Lexilla
        // 5.x is < 32 chars).
        let mut buf = [0u8; 64];
        let bytes = name.as_bytes();
        if bytes.len() >= buf.len() {
            return false;
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        // SAFETY: `buf` is NUL-terminated (zeroed; we only wrote `bytes.len()`
        // bytes). `CreateLexer` is a pure function that reads the C string
        // and returns either a valid `ILexer5*` we then hand to
        // `SCI_SETILEXER`, or null if the name isn't registered.
        let ilexer = unsafe { CreateLexer(buf.as_ptr().cast::<core::ffi::c_char>()) };
        if ilexer.is_null() {
            return false;
        }
        // wparam = 0 (unused), lparam = ILexer5*. Scintilla takes
        // ownership and will release the pointer when the lexer is
        // replaced or the document is destroyed.
        self.send(SCI_SETILEXER, 0, ilexer as sptr_t);
        true
    }

    /// Detach any current lexer, leaving the view in plain-text
    /// rendering. Equivalent to `SCI_SETILEXER(0, 0)`.
    ///
    /// `SCI_SETILEXER(0, 0)` also enables Scintilla's container-
    /// lexer mode — with no lexer attached, styling is delegated
    /// to the host via `SCN_STYLENEEDED` notifications. Phase
    /// 4.6 m1c-3 uses this path for UDL buffers.
    pub fn clear_lexer(&self) {
        self.send(SCI_SETILEXER, 0, 0);
    }

    /// Position the styler at byte offset `start`. Subsequent
    /// [`Self::set_styling`] calls paint bytes from there
    /// onward, advancing the internal cursor as they go. Called
    /// from Phase 4.6 m1c-3b's `SCN_STYLENEEDED` handler to
    /// begin a container-lexer styling pass.
    pub fn start_styling(&self, start: usize) {
        self.send(SCI_STARTSTYLING, start, 0);
    }

    /// Paint the next `length` bytes with `style`. `style` must
    /// fit in a `u8` (Scintilla style indices are byte-sized);
    /// the host's UDL tokeniser passes `UdlStyleSlot as u8`
    /// values that map 1:1 to N++'s `SCE_USER_STYLE_*`
    /// constants.
    pub fn set_styling(&self, length: usize, style: u8) {
        self.send(SCI_SETSTYLING, length, sptr_t::from(style));
    }

    /// Byte offset up to which Scintilla considers styling
    /// applied. The container-lexer host walks BACKWARDS from
    /// this position to a line boundary before starting a
    /// restart-safe tokenisation pass in the `SCN_STYLENEEDED`
    /// handler.
    #[must_use]
    pub fn get_end_styled(&self) -> usize {
        let ret = self.send(SCI_GETENDSTYLED, 0, 0);
        ret.max(0) as usize
    }

    /// Line number (0-indexed) that contains byte offset
    /// `position`. Used to align the `SCN_STYLENEEDED`
    /// tokenisation range to line boundaries — restarting mid-
    /// line risks splitting a delimiter span the tokeniser
    /// wouldn't otherwise know it was inside.
    #[must_use]
    pub fn line_from_position(&self, position: usize) -> usize {
        let ret = self.send(SCI_LINEFROMPOSITION, position, 0);
        ret.max(0) as usize
    }

    /// First-character byte offset of `line`. Returns `None` when
    /// Scintilla signals "line out of range" — per
    /// `ScintillaDoc.html:796-801`, `SCI_POSITIONFROMLINE`
    /// returns `-1` for a line number strictly greater than the
    /// document's line count (NOT a clamp — an explicit
    /// out-of-range sentinel). Preserving `None` lets the
    /// caller distinguish that from a legitimate "position 0 =
    /// start of document" answer; m1c-3b's line-boundary
    /// alignment for `SCN_STYLENEEDED` uses this discipline so
    /// an off-by-one in the range math fails loudly rather than
    /// producing a plausible-looking "restyle from column 0"
    /// range.
    #[must_use]
    pub fn position_from_line(&self, line: usize) -> Option<usize> {
        let ret = self.send(SCI_POSITIONFROMLINE, line, 0);
        if ret < 0 {
            None
        } else {
            Some(ret as usize)
        }
    }

    /// Read a range of Scintilla's document as an owned
    /// `Vec<u8>`. Returns `None` on zero-length or invalid
    /// ranges.
    ///
    /// **Deliberate copy, not a zero-copy borrow.** Scintilla's
    /// `SCI_GETRANGEPOINTER` returns a raw pointer into the
    /// live document buffer that is invalidated by ANY
    /// subsequent Scintilla call (per
    /// `ScintillaDoc.html:7437-7440`), including read-only ones
    /// like `SCI_GETLENGTH`. A safe scoped-borrow API around
    /// that would require the closure to never call any
    /// `EditorHandle` method — but `EditorHandle` is `Copy`,
    /// so a closure can capture one from its environment and
    /// the type system can't prevent the reentrancy. Copying
    /// eliminates the hazard entirely at the cost of one
    /// bounded allocation per call.
    ///
    /// The typical caller is m1c-3b's `SCN_STYLENEEDED` handler
    /// reading a viewport-sized range (single-digit KB) — the
    /// copy cost is negligible against the tokenise + paint
    /// work that follows.
    ///
    /// Reads bytes via `SCI_GETRANGEPOINTER` under the hood,
    /// then immediately copies before returning. The unsafe
    /// slice construction is bounded to the copy line and
    /// cannot escape.
    #[must_use]
    pub fn get_range_bytes(&self, start: usize, length: usize) -> Option<Vec<u8>> {
        if length == 0 {
            return None;
        }
        let ptr = self.send(SCI_GETRANGEPOINTER, start, length as sptr_t);
        // Scintilla returns 0 (null) for zero-length or invalid
        // ranges. On 64-bit Windows, user-mode addresses never
        // set the sign bit, so `<= 0` also catches any negative
        // return that would indicate an internal error we don't
        // want to interpret as a pointer.
        if ptr <= 0 {
            return None;
        }
        // SAFETY: SCI_GETRANGEPOINTER returned a non-null
        // pointer into Scintilla's own buffer covering exactly
        // `length` bytes starting at `start`. The pointer is
        // valid at this instant (before any subsequent call);
        // we read it immediately into an owned Vec on the very
        // next line, then drop the raw view. No other
        // `EditorHandle::send` call runs between construction
        // of the slice and the copy, and the raw slice does
        // not escape this function.
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, length) };
        Some(slice.to_vec())
    }

    /// Install a space-separated keyword list under `set_index`.
    /// `LexCPP`'s class 0 is "primary keywords"; `LexRust` uses 0 for
    /// the language's reserved words.
    pub fn set_keywords(&self, set_index: u32, words: &str) {
        let mut buf = Vec::with_capacity(words.len() + 1);
        buf.extend_from_slice(words.as_bytes());
        buf.push(0);
        // SAFETY: SCI_SETKEYWORDS is documented to copy the string;
        // `buf`'s pointer must stay valid only for the duration of
        // the call, which is `send`'s duration. The Vec is dropped
        // on return.
        self.send(SCI_SETKEYWORDS, set_index as uptr_t, buf.as_ptr() as sptr_t);
    }

    /// Set a style's foreground colour. `colour` is a Scintilla
    /// `COLORREF` (`0x00BBGGRR`), so `0x0000FF` is red, `0x00FF00`
    /// green, `0xFF0000` blue.
    pub fn style_set_fore(&self, style: usize, colour: u32) {
        self.send(SCI_STYLESETFORE, style as uptr_t, colour as sptr_t);
    }

    /// Set a style's background colour. Same `COLORREF` encoding as
    /// [`Self::style_set_fore`].
    pub fn style_set_back(&self, style: usize, colour: u32) {
        self.send(SCI_STYLESETBACK, style as uptr_t, colour as sptr_t);
    }

    /// Toggle the bold attribute for a style.
    pub fn style_set_bold(&self, style: usize, bold: bool) {
        self.send(SCI_STYLESETBOLD, style as uptr_t, sptr_t::from(bold));
    }

    /// Toggle the italic attribute for a style.
    pub fn style_set_italic(&self, style: usize, italic: bool) {
        self.send(SCI_STYLESETITALIC, style as uptr_t, sptr_t::from(italic));
    }

    /// Toggle the underline attribute for a style.
    pub fn style_set_underline(&self, style: usize, underline: bool) {
        self.send(
            SCI_STYLESETUNDERLINE,
            style as uptr_t,
            sptr_t::from(underline),
        );
    }

    /// Set the font point size for a style. `points` is the integer
    /// point size — Scintilla also supports fractional point sizes
    /// via `SCI_STYLESETSIZEFRACTIONAL`, not exposed here yet.
    pub fn style_set_size(&self, style: usize, points: i32) {
        self.send(SCI_STYLESETSIZE, style as uptr_t, points as sptr_t);
    }

    /// Set the typeface name for a style. Scintilla expects a
    /// UTF-8 C-string and copies the bytes into its own state, so
    /// the caller's `&str` can be dropped immediately after.
    /// An interior NUL byte degrades to "no font name" (Scintilla
    /// falls back to its built-in default) with a trace so the
    /// failure is observable; XML-deserialised values can't carry
    /// NUL but a programmatic caller could.
    pub fn style_set_font(&self, style: usize, name: &str) {
        // SCI_STYLESETFONT requires a NUL-terminated UTF-8 string;
        // build a `CString` so the trailing NUL is guaranteed.
        let cname = if let Ok(c) = std::ffi::CString::new(name) {
            c
        } else {
            tracing::warn!(
                style = style,
                name = name,
                "style_set_font: font name contains interior NUL; using Scintilla default"
            );
            std::ffi::CString::default()
        };
        self.send(SCI_STYLESETFONT, style as uptr_t, cname.as_ptr() as sptr_t);
    }

    // --- Caret-line highlight ---------------------------------------------

    /// Toggle caret-line background highlighting. When enabled,
    /// Scintilla paints the line containing the caret with the
    /// colour set via [`Self::set_caret_line_back`]. The setting is
    /// view state (not a per-style colour), so it survives
    /// `SCI_STYLECLEARALL` and only needs to be applied once at
    /// editor creation.
    pub fn set_caret_line_visible(&self, visible: bool) {
        self.send(SCI_SETCARETLINEVISIBLE, uptr_t::from(visible), 0);
    }

    /// Set the background colour for the caret line. `colour` uses
    /// the same `0x00BBGGRR` `COLORREF` encoding as
    /// [`Self::style_set_back`]. Has no visible effect unless
    /// [`Self::set_caret_line_visible`] is also enabled.
    pub fn set_caret_line_back(&self, colour: u32) {
        self.send(SCI_SETCARETLINEBACK, colour as uptr_t, 0);
    }

    // --- Margins -----------------------------------------------------------

    /// Set margin `n`'s type — e.g. `SC_MARGIN_TEXT` to render
    /// per-line text the host writes via `SCI_MARGINSETTEXT`,
    /// styled per-line via `SCI_MARGINSETSTYLE`. The margin's
    /// width and per-style colours are configured separately via
    /// [`Self::set_margin_width`] and the `style_set_*` helpers.
    pub fn set_margin_type(&self, margin: u32, ty: u32) {
        self.send(SCI_SETMARGINTYPEN, margin as uptr_t, ty as sptr_t);
    }

    /// Set margin `n`'s pixel width. Width `0` hides the margin
    /// without resetting its type or other state — useful for the
    /// future "show line numbers" view toggle, which only needs to
    /// flip between a configured width and `0`.
    pub fn set_margin_width(&self, margin: u32, pixels: i32) {
        self.send(SCI_SETMARGINWIDTHN, margin as uptr_t, pixels as sptr_t);
    }

    /// Set the marker bitmask for margin `n`. Each margin only
    /// renders markers whose id appears in its mask. Used by the
    /// change-history strip to ensure that margin shows the
    /// `SC_MARKNUM_HISTORY_*` markers and *only* those — without
    /// this filter, a future plugin-installed bookmark in margin 1
    /// could leak into the edit-indicator strip.
    pub fn set_margin_mask(&self, margin: u32, mask: u32) {
        self.send(SCI_SETMARGINMASKN, margin as uptr_t, mask as sptr_t);
    }

    /// Configure the symbol drawn for marker number `marker_num`
    /// (one of `SC_MARKNUM_*`). `symbol` is one of the `SC_MARK_*`
    /// shape constants — `SC_MARK_FULLRECT` fills the margin
    /// column, the right shape for the change-history strip.
    pub fn marker_define(&self, marker_num: u32, symbol: u32) {
        self.send(SCI_MARKERDEFINE, marker_num as uptr_t, symbol as sptr_t);
    }

    /// Configure the background colour drawn for marker number
    /// `marker_num`. `colour` uses the same `0x00BBGGRR` encoding
    /// as [`Self::style_set_fore`]. Markers fill their bounding
    /// rectangle with this colour, which is what makes
    /// `SC_MARK_FULLRECT` paint as a solid bar in a narrow margin.
    pub fn marker_set_back(&self, marker_num: u32, colour: u32) {
        self.send(SCI_MARKERSETBACK, marker_num as uptr_t, colour as sptr_t);
    }

    /// Configure the foreground colour drawn for marker number
    /// `marker_num`. For outline-style markers (`SC_MARK_BOXPLUS` /
    /// `SC_MARK_BOXMINUS` / the fold `VLINE`/`LCORNER`/`TCORNER`
    /// family) the foreground is the outline / glyph colour, while
    /// [`Self::marker_set_back`] is the fill. `colour` is the same
    /// `0x00BBGGRR` COLORREF as [`Self::style_set_fore`].
    pub fn marker_set_fore(&self, marker_num: u32, colour: u32) {
        self.send(SCI_MARKERSETFORE, marker_num as uptr_t, colour as sptr_t);
    }

    /// Configure the highlight background colour for marker number
    /// `marker_num` — drawn instead of [`Self::marker_set_back`] when
    /// the marker's fold range contains the caret (only fires while
    /// [`Self::marker_enable_highlight`] is on). Powers the "hover
    /// the caret over a collapsed region and its `+`/`−` glows"
    /// visual feedback that Notepad++ uses.
    pub fn marker_set_back_selected(&self, marker_num: u32, colour: u32) {
        self.send(
            SCI_MARKERSETBACKSELECTED,
            marker_num as uptr_t,
            colour as sptr_t,
        );
    }

    /// Toggle the marker-highlight feature globally. When on,
    /// [`Self::marker_set_back_selected`] takes effect for every
    /// marker whose containing fold range brackets the caret.
    pub fn marker_enable_highlight(&self, on: bool) {
        self.send(SCI_MARKERENABLEHIGHLIGHT, sptr_t::from(on) as uptr_t, 0);
    }

    /// Make margin `n` respond to mouse clicks — required for
    /// click-to-toggle-fold behaviour, whether the host handles the
    /// clicks manually via `SCN_MARGINCLICK` or delegates to
    /// [`Self::set_automatic_fold`].
    pub fn set_margin_sensitive(&self, margin: u32, on: bool) {
        self.send(SCI_SETMARGINSENSITIVEN, margin as uptr_t, sptr_t::from(on));
    }

    /// Paint the fold-margin strip in `colour` (`0x00BBGGRR`
    /// COLORREF). `use_it = false` clears the override and lets
    /// Scintilla fall back to its theme default. Distinct from a
    /// marker background — this fills the strip between markers.
    pub fn set_fold_margin_colour(&self, use_it: bool, colour: u32) {
        self.send(
            SCI_SETFOLDMARGINCOLOUR,
            sptr_t::from(use_it) as uptr_t,
            colour as sptr_t,
        );
    }

    /// Fold-margin strip colour drawn when the mouse hovers over
    /// the margin. Same encoding as
    /// [`Self::set_fold_margin_colour`].
    pub fn set_fold_margin_hi_colour(&self, use_it: bool, colour: u32) {
        self.send(
            SCI_SETFOLDMARGINHICOLOUR,
            sptr_t::from(use_it) as uptr_t,
            colour as sptr_t,
        );
    }

    /// Enable Scintilla's built-in fold-margin behaviour — click
    /// dispatch, marker visibility, auto-expand-on-edit — driven by
    /// `flags` (`SC_AUTOMATICFOLD_SHOW | _CLICK | _CHANGE`). With
    /// all three set, the host does not need an `SCN_MARGINCLICK`
    /// handler for vanilla click-to-toggle behaviour. Shift/Ctrl-
    /// click extensions (fold-all-children) still require a manual
    /// handler.
    pub fn set_automatic_fold(&self, flags: u32) {
        self.send(SCI_SETAUTOMATICFOLD, flags as uptr_t, 0);
    }

    /// Configure visual decorations around contracted/expanded fold
    /// ranges. `flags` is an OR of `SC_FOLDFLAG_*`; N++ ships
    /// `SC_FOLDFLAG_LINEAFTER_CONTRACTED` (0x10) — the "draw a
    /// horizontal rule below a collapsed region" indicator.
    pub fn set_fold_flags(&self, flags: u32) {
        self.send(SCI_SETFOLDFLAGS, flags as uptr_t, 0);
    }

    /// Set a runtime property on the currently-attached Lexilla
    /// lexer — the standard way to toggle features like folding
    /// (`"fold"` → `"1"`), lexer sub-flags
    /// (`"fold.preprocessor"` / `"fold.quotes.python"`), or the
    /// language-specific case-fold / user-word flags that some
    /// lexers respect. Scintilla copies both strings on the call;
    /// caller buffers only need to live for the duration of `send`.
    ///
    /// **Requires a lexer attached** — `LexState::PropSet`
    /// (`ScintillaBase.cxx:687-694`) is `if (instance) { … }` and
    /// silently no-ops with no lexer. Callers must issue
    /// `SCI_SETILEXER` (via [`Self::set_lexer_by_name`]) first, and
    /// re-issue every property after every subsequent
    /// `SCI_SETILEXER` — Scintilla does NOT carry properties over
    /// to the new lexer instance.
    ///
    /// Names and values must be plain ASCII with no interior NUL —
    /// interior NUL falls back to an empty string and emits a
    /// `tracing::warn!` (same pattern as
    /// [`Self::style_set_font`]) so the failure is observable
    /// rather than silent.
    pub fn set_property(&self, name: &str, value: &str) {
        let cname = std::ffi::CString::new(name).unwrap_or_else(|_| {
            tracing::warn!(
                property = name,
                "set_property: name contains interior NUL; using empty string"
            );
            std::ffi::CString::default()
        });
        let cvalue = std::ffi::CString::new(value).unwrap_or_else(|_| {
            tracing::warn!(
                property = name,
                value = value,
                "set_property: value contains interior NUL; using empty string"
            );
            std::ffi::CString::default()
        });
        self.send(
            SCI_SETPROPERTY,
            cname.as_ptr() as uptr_t,
            cvalue.as_ptr() as sptr_t,
        );
    }

    /// Return the current caret position (0-based byte offset into
    /// the buffer). Used by the brace-match dispatch on every
    /// `SCN_UPDATEUI` with `SC_UPDATE_SELECTION`.
    #[must_use]
    pub fn current_pos(&self) -> u64 {
        self.send(SCI_GETCURRENTPOS, 0, 0) as u64
    }

    /// Return the byte at document position `pos`, or `0` if `pos`
    /// is past the end. Used by the brace-match dispatch to detect
    /// whether the caret sits at (or immediately after) a bracket.
    #[must_use]
    pub fn char_at(&self, pos: u64) -> u8 {
        (self.send(SCI_GETCHARAT, pos as uptr_t, 0) & 0xFF) as u8
    }

    /// Return the position of the bracket paired with the one at
    /// `pos`, or `-1` ([`codepp_scintilla_sys::INVALID_POSITION`]) if
    /// there is no match. Scintilla returns `Sci_Position` /
    /// `sptr_t` — signed — because `-1` is the "unmatched" sentinel.
    /// Caller supplies the position of a bracket byte; behaviour is
    /// undefined (returns `-1`) if `pos` is not on a bracket.
    #[must_use]
    pub fn brace_match(&self, pos: u64) -> i64 {
        self.send(SCI_BRACEMATCH, pos as uptr_t, 0) as i64
    }

    /// Highlight the two positions `pos_a` and `pos_b` in
    /// `STYLE_BRACELIGHT` — the matched-brace-pair visual. Passing
    /// `-1` for either clears the highlight; passing `-1` for both
    /// clears any previously-highlighted pair.
    pub fn brace_highlight(&self, pos_a: i64, pos_b: i64) {
        self.send(SCI_BRACEHIGHLIGHT, pos_a as uptr_t, pos_b as sptr_t);
    }

    /// Highlight `pos` in `STYLE_BRACEBAD` — the unmatched-bracket
    /// visual. Passing `-1` clears the highlight. Only one position
    /// at a time; the "bad" highlight is per-caret, not per-pair.
    pub fn brace_bad_light(&self, pos: i64) {
        self.send(SCI_BRACEBADLIGHT, pos as uptr_t, 0);
    }

    /// Enable Scintilla's built-in change-history tracking on the
    /// **currently bound** document. `flags` is a bitmask of
    /// `SC_CHANGE_HISTORY_*` values (`ENABLED | MARKERS` is the
    /// pair Code++'s edit-indicator strip uses). Per-document
    /// setting — must be re-applied after every
    /// `SCI_CREATEDOCUMENT`. Once enabled, Scintilla auto-applies
    /// `SC_MARKNUM_HISTORY_MODIFIED` to lines that diverge from the
    /// last save-point and clears them when `SCI_SETSAVEPOINT`
    /// advances the baseline.
    pub fn set_change_history(&self, flags: u32) {
        self.send(SCI_SETCHANGEHISTORY, flags as uptr_t, 0);
    }

    /// Enable Scintilla's change-history tracking on the **currently
    /// bound** document (`ENABLED | MARKERS`). Per-document setting: every
    /// fresh `SCI_CREATEDOCUMENT` starts with history off, so this must be
    /// called on each new document after binding it. Margin configuration
    /// lives on the view — see [`Self::configure_change_history_margin`].
    pub fn enable_change_history(&self) {
        self.set_change_history(SC_CHANGE_HISTORY_ENABLED | SC_CHANGE_HISTORY_MARKERS);
    }

    /// Configure the change-history "edit indicator" margin — the thin
    /// coloured strip that marks lines changed since the last save. Sets
    /// the margin's type, mask, width, and the `SC_MARKNUM_HISTORY_MODIFIED`
    /// marker's symbol + colour. View-level state, so one call survives
    /// every `SCI_SETDOCPOINTER` cycle the tab strip drives; per-document
    /// *enablement* is [`Self::enable_change_history`].
    ///
    /// Shared by every backend (Win32, GTK, and the coming Cocoa) so the
    /// strip looks and behaves identically. `colour` is a Scintilla
    /// `0x00BBGGRR` value — Code++ passes its Material orange 400, the same
    /// shade the active-tab indicator uses, tying the "you're editing this"
    /// cues into one visual language.
    ///
    /// Critical detail: Scintilla's default mask for margin 1 is
    /// `~SC_MASK_FOLDERS`, which *includes* the history-marker family
    /// (21-24). With `SC_CHANGE_HISTORY_MARKERS` on, every margin whose
    /// mask matches renders it — so without cleanup the strip would also
    /// paint in margin 1 at margin 1's width. Margins 1-3 are therefore
    /// defensively cleared, and the three sibling history markers
    /// (`SAVED` paints a green line background over every just-loaded line,
    /// etc.) are silenced to `SC_MARK_EMPTY` — visually no-ops that keep
    /// Scintilla's internal tracking intact for a future feature.
    pub fn configure_change_history_margin(&self, margin: u32, width_px: i32, colour: u32) {
        for unused in 1..=3u32 {
            self.set_margin_mask(unused, 0);
            self.set_margin_width(unused, 0);
        }
        for silenced in [
            SC_MARKNUM_HISTORY_REVERTED_TO_ORIGIN,
            SC_MARKNUM_HISTORY_SAVED,
            SC_MARKNUM_HISTORY_REVERTED_TO_MODIFIED,
        ] {
            self.marker_define(silenced, SC_MARK_EMPTY);
        }
        self.set_margin_type(margin, SC_MARGIN_SYMBOL);
        self.set_margin_mask(margin, 1 << SC_MARKNUM_HISTORY_MODIFIED);
        self.marker_define(SC_MARKNUM_HISTORY_MODIFIED, SC_MARK_FULLRECT);
        self.marker_set_back(SC_MARKNUM_HISTORY_MODIFIED, colour);
        self.set_margin_width(margin, width_px);
    }

    /// Turn `margin` into Scintilla's built-in line-number margin
    /// (`SC_MARGIN_NUMBER`, which auto-renders `STYLE_LINENUMBER`-styled
    /// numbers with no per-line population) and size it to a fixed,
    /// font-/DPI-correct width. Call once per view; re-call after any
    /// `SCI_STYLECLEARALL` (which resets `STYLE_LINENUMBER`) so the width
    /// re-measures against the restored font.
    ///
    /// The width is **constant** — sized for `LINE_NUMBER_MARGIN_DIGITS`
    /// digits regardless of the document's actual line count — so the left
    /// gutter never jiggles while editing (crossing 9→10 or 99→100 lines
    /// leaves it untouched). This mirrors the Win32 backend's deliberately
    /// roomy fixed bar; both trade clipping on files past the digit budget
    /// for a stable, non-shifting gutter.
    pub fn enable_line_number_margin(&self, margin: u32) {
        self.set_margin_type(margin, SC_MARGIN_NUMBER);
        // Sample = one-space pad + a fixed run of nines + NUL. The pad gives
        // the column a little breathing room from the text, matching N++;
        // the fixed digit count (not the live line count) is what keeps the
        // width constant. `SCI_TEXTWIDTH` makes it font- and DPI-correct.
        let digits = LINE_NUMBER_MARGIN_DIGITS as usize;
        let mut sample = Vec::with_capacity(digits + 2);
        sample.push(b'_');
        sample.resize(sample.len() + digits, b'9');
        sample.push(0);
        let width = self.send(
            SCI_TEXTWIDTH,
            STYLE_LINENUMBER as uptr_t,
            sample.as_ptr() as sptr_t,
        );
        self.set_margin_width(margin, i32::try_from(width).unwrap_or(0));
    }

    /// Reset every style index to the current `STYLE_DEFAULT`. The
    /// idiomatic sequence after switching lexers is:
    ///
    /// 1. configure `STYLE_DEFAULT` (font, size, default colours),
    /// 2. call `style_clear_all` so every other style inherits it,
    /// 3. apply per-style overrides (comment colour, keyword colour, …).
    ///
    /// Without (2), the previous lexer's per-style colours bleed
    /// through into the new lexer's style indices.
    pub fn style_clear_all(&self) {
        self.send(SCI_STYLECLEARALL, 0, 0);
    }

    // --- Search / replace -------------------------------------------------

    /// Install the SCFIND_* flag bitset on the view's sticky
    /// search state. Used by `SCI_SEARCHINTARGET`, which (unlike
    /// `SCI_SEARCHNEXT/PREV`) reads flags from sticky state rather
    /// than from `wparam`. `search_next` / `search_prev` take
    /// flags directly so this is only needed before a Replace All
    /// loop.
    pub fn set_search_flags(&self, flags: u32) {
        self.send(SCI_SETSEARCHFLAGS, flags as uptr_t, 0);
    }

    /// Anchor the search position at the current selection start.
    /// Pair with `search_next`/`search_prev` so the search walks
    /// forward/backward from the user's caret rather than from
    /// document-start. Without an anchor, repeat-search would
    /// re-find the just-found match.
    pub fn search_anchor(&self) {
        self.send(SCI_SEARCHANCHOR, 0, 0);
    }

    /// Search forward from the anchor for `needle` under the
    /// supplied SCFIND_* `flags`. Scintilla reads flags from
    /// `wparam` for `SCI_SEARCHNEXT` (verified against
    /// `Editor.cxx::SearchText`), so passing them here keeps the
    /// helper self-contained — no hidden dependency on a previous
    /// `set_search_flags` call. Returns the byte offset of the
    /// match, or `-1` on miss. On a hit, Scintilla moves the
    /// selection to the match, which doubles as the new anchor for
    /// the next `search_next` so Find Next walks through the
    /// buffer.
    #[must_use]
    pub fn search_next(&self, needle: &str, flags: u32) -> isize {
        let mut buf = Vec::with_capacity(needle.len() + 1);
        buf.extend_from_slice(needle.as_bytes());
        buf.push(0);
        self.send(SCI_SEARCHNEXT, flags as uptr_t, buf.as_ptr() as sptr_t)
    }

    /// Search backward from the anchor for `needle`. Same flags
    /// + return shape as [`Self::search_next`].
    #[must_use]
    pub fn search_prev(&self, needle: &str, flags: u32) -> isize {
        let mut buf = Vec::with_capacity(needle.len() + 1);
        buf.extend_from_slice(needle.as_bytes());
        buf.push(0);
        self.send(SCI_SEARCHPREV, flags as uptr_t, buf.as_ptr() as sptr_t)
    }

    /// Set the byte range (`[start, end)`) Replace All iterates
    /// over and `search_in_target` searches within. Used for
    /// Replace-All-in-Selection (range = current selection) and
    /// for Replace-All-in-Document (range = `[0, length)`).
    pub fn set_target_range(&self, start: u64, end: u64) {
        self.send(SCI_SETTARGETRANGE, start as uptr_t, end as sptr_t);
    }

    /// Search for `needle` (NUL-terminated UTF-8) within the
    /// current target range. On a hit, Scintilla narrows the
    /// target to the match's byte range so a subsequent
    /// `replace_target` substitutes only the match. Returns the
    /// byte offset of the match, or `-1` on miss.
    #[must_use]
    pub fn search_in_target(&self, needle: &str) -> isize {
        // SCI_SEARCHINTARGET takes the length in wparam and a
        // *non-NUL-terminated* pointer in lparam — but a
        // NUL-terminator is harmless (Scintilla reads `wparam`
        // bytes only) and writing it keeps this helper symmetrical
        // with `search_next`. The buffer is dropped after `send`
        // returns, which is fine because Scintilla copies into its
        // internal state synchronously.
        let mut buf = Vec::with_capacity(needle.len() + 1);
        buf.extend_from_slice(needle.as_bytes());
        buf.push(0);
        self.send(
            SCI_SEARCHINTARGET,
            needle.len() as uptr_t,
            buf.as_ptr() as sptr_t,
        )
    }

    /// Replace the current target range with `replacement`, expanding
    /// regex group references (`$1`..`$9` under `SCFIND_CXX11REGEX`)
    /// against the last regex match when `regex` is set.
    ///
    /// `regex` must match the flag the preceding `search_in_target`
    /// used: `SCI_REPLACETARGETRE` only has a meaningful last-match to
    /// reference after a regex search, and running it after a literal
    /// search would expand `$1` against stale or absent group state.
    /// Callers therefore pass the same regex bit they searched with.
    ///
    /// After the replace, the target range is reset to point at the
    /// inserted text so the next `search_in_target` resumes just past
    /// the substitution. Returns the byte length of the replacement.
    #[must_use]
    pub fn replace_target_with(&self, replacement: &str, regex: bool) -> isize {
        debug_assert!(
            replacement.len() != usize::MAX,
            "replacement length must not equal usize::MAX",
        );
        let msg = if regex {
            SCI_REPLACETARGETRE
        } else {
            SCI_REPLACETARGET
        };
        self.send(
            msg,
            replacement.len() as uptr_t,
            replacement.as_ptr() as sptr_t,
        )
    }

    /// Literal replacement — [`Self::replace_target_with`] with
    /// `regex = false`. Kept for the callers that never search by
    /// regex.
    #[must_use]
    pub fn replace_target(&self, replacement: &str) -> isize {
        // Scintilla's `ViewFromParams(lParam, wParam)` treats
        // `wParam == usize::MAX` as a sentinel meaning "use strlen
        // on the buffer". `replacement.as_ptr()` is not
        // NUL-terminated (it's a `&str`), so the strlen branch
        // would read past the slice. A `&str` with `usize::MAX`
        // bytes is unallocatable on any 64-bit Rust process, so
        // this is a theoretical assert rather than a practical
        // guard — debug-only is enough.
        debug_assert!(
            replacement.len() != usize::MAX,
            "replacement length must not equal usize::MAX",
        );
        self.send(
            SCI_REPLACETARGET,
            replacement.len() as uptr_t,
            replacement.as_ptr() as sptr_t,
        )
    }

    /// Read the byte offset of the current target's start. After a
    /// `search_in_target` hit, this equals the match's start.
    #[must_use]
    pub fn target_start(&self) -> u64 {
        self.send(SCI_GETTARGETSTART, 0, 0).max(0) as u64
    }

    /// Read the byte offset of the current target's end. After a
    /// `search_in_target` hit, this equals the match's end.
    #[must_use]
    pub fn target_end(&self) -> u64 {
        self.send(SCI_GETTARGETEND, 0, 0).max(0) as u64
    }
}

/// Fixed digit budget the built-in line-number margin is sized for. The
/// margin width is held constant at this many digits regardless of the
/// document's actual line count, so the left gutter never grows or shrinks
/// while editing — the same deliberately-roomy fixed-bar choice the Win32
/// backend makes. Five digits (99 999 lines) comfortably covers typical
/// files; larger ones clip, exactly as the Win32 fixed-width bar does.
const LINE_NUMBER_MARGIN_DIGITS: u32 = 5;
