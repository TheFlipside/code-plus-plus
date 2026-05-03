//! Safe Scintilla wrapper for Code++.
//!
//! Phase 1: `EditorHandle` carries the raw HWND plus the direct-call
//! `(fn_ptr, instance_ptr)` pair captured once at construction. Hot
//! operations route through `send` (the direct call); window-managed
//! ones still use `SendMessage` from the UI crate. See DESIGN.md §4.2.

#![cfg(target_os = "windows")]

use core::ffi::c_void;

use codepp_scintilla_sys::{sptr_t, uptr_t, ScintillaDirectFunction};

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
}
