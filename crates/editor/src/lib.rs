//! Safe Scintilla wrapper for Code++.
//!
//! Phase 1: `EditorHandle` carries the raw HWND plus the direct-call
//! `(fn_ptr, instance_ptr)` pair captured once at construction. Hot
//! operations route through `send` (the direct call); window-managed
//! ones still use `SendMessage` from the UI crate. See DESIGN.md §4.2.

#![cfg(target_os = "windows")]

use core::ffi::c_void;

use codepp_scintilla_sys::{
    sptr_t, uptr_t, CreateLexer, ScintillaDirectFunction, SCI_SETILEXER, SCI_SETKEYWORDS,
    SCI_STYLECLEARALL, SCI_STYLESETBACK, SCI_STYLESETBOLD, SCI_STYLESETFORE, SCI_STYLESETITALIC,
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
}
