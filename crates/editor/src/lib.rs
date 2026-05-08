//! Safe Scintilla wrapper for Code++.
//!
//! Phase 1: `EditorHandle` carries the raw HWND plus the direct-call
//! `(fn_ptr, instance_ptr)` pair captured once at construction. Hot
//! operations route through `send` (the direct call); window-managed
//! ones still use `SendMessage` from the UI crate. See DESIGN.md §4.2.

#![cfg(target_os = "windows")]

use core::ffi::c_void;

use codepp_scintilla_sys::{
    sptr_t, uptr_t, CreateLexer, ScintillaDirectFunction, SCI_GETTARGETEND, SCI_GETTARGETSTART,
    SCI_REPLACETARGET, SCI_SEARCHANCHOR, SCI_SEARCHINTARGET, SCI_SEARCHNEXT, SCI_SEARCHPREV,
    SCI_SETILEXER, SCI_SETKEYWORDS, SCI_SETMARGINTYPEN, SCI_SETMARGINWIDTHN, SCI_SETSEARCHFLAGS,
    SCI_SETTARGETRANGE, SCI_STYLECLEARALL, SCI_STYLESETBACK, SCI_STYLESETBOLD, SCI_STYLESETFORE,
    SCI_STYLESETITALIC,
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

    /// Direct-call into Scintilla. The hot path — every keystroke, every
    /// selection update, every style query goes through this.
    #[inline]
    pub fn send(&self, msg: u32, wparam: uptr_t, lparam: sptr_t) -> sptr_t {
        // SAFETY: `direct_fn` and `direct_ptr` were captured together from a
        // real Scintilla control via `SCI_GETDIRECTFUNCTION` /
        // `SCI_GETDIRECTPOINTER` (enforced by the `unsafe` constructor).
        unsafe { (self.direct_fn)(self.direct_ptr, msg, wparam, lparam) }
    }

    /// The underlying Scintilla `HWND` (as `*mut c_void` to keep this crate
    /// free of `windows`-crate types).
    #[inline]
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
        let ilexer = unsafe { CreateLexer(buf.as_ptr() as *const core::ffi::c_char) };
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
    pub fn clear_lexer(&self) {
        self.send(SCI_SETILEXER, 0, 0);
    }

    /// Install a space-separated keyword list under `set_index`.
    /// LexCPP's class 0 is "primary keywords"; LexRust uses 0 for
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
        self.send(SCI_STYLESETBOLD, style as uptr_t, bold as sptr_t);
    }

    /// Toggle the italic attribute for a style.
    pub fn style_set_italic(&self, style: usize, italic: bool) {
        self.send(SCI_STYLESETITALIC, style as uptr_t, italic as sptr_t);
    }

    // --- Margins -----------------------------------------------------------

    /// Set margin `n`'s type — e.g.
    /// `SC_MARGIN_NUMBER` to render the margin as right-aligned
    /// line numbers in `STYLE_LINENUMBER`. The margin's width and
    /// per-style colours are configured separately via
    /// [`Self::set_margin_width`] and the `style_set_*` helpers
    /// against `STYLE_LINENUMBER`.
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
    pub fn search_next(&self, needle: &str, flags: u32) -> isize {
        let mut buf = Vec::with_capacity(needle.len() + 1);
        buf.extend_from_slice(needle.as_bytes());
        buf.push(0);
        self.send(SCI_SEARCHNEXT, flags as uptr_t, buf.as_ptr() as sptr_t)
    }

    /// Search backward from the anchor for `needle`. Same flags
    /// + return shape as [`Self::search_next`].
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

    /// Replace the current target range with `replacement` (literal
    /// text, NOT regex `\1` substitution — that's
    /// `SCI_REPLACETARGETRE`, not yet wired). After the replace,
    /// the target range is reset to point at the inserted text so
    /// the next `search_in_target` resumes from just past the
    /// substitution. Returns the byte length of the replacement.
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
    pub fn target_start(&self) -> u64 {
        self.send(SCI_GETTARGETSTART, 0, 0).max(0) as u64
    }

    /// Read the byte offset of the current target's end. After a
    /// `search_in_target` hit, this equals the match's end.
    pub fn target_end(&self) -> u64 {
        self.send(SCI_GETTARGETEND, 0, 0).max(0) as u64
    }
}
