//! In-tree sample plugin for Code++.
//!
//! Demonstrates the full Notepad++-compatible plugin lifecycle:
//! `setInfo` stash, `getName` / `getFuncsArray` / `isUnicode`
//! identification, a menu command that inserts "Hello from plugin"
//! at the editor's current caret, and a real docking panel that
//! exercises the host's `NPPM_DMM*` / `DMN_CLOSE` surface (see
//! [`dock`]). On macOS it also asks the host for a Scintilla view of
//! its own, a toolbar button and a modeless-dialog registration (see
//! [`dock`] and [`dialog`]). The insertion path exercises both the
//! inbound NPPM dispatcher (the plugin queries
//! `NPPM_GETCURRENTSCINTILLA` to learn which view is active) and direct
//! Scintilla messaging (`SCI_INSERTTEXT` against the returned view's
//! HWND).
//!
//! Phase 3 milestone 5: this plugin is the first end-to-end consumer
//! of the host's plugin ABI. A real Notepad++ binary plugin dropped
//! into the same plugins folder is expected to load and run by the
//! same code paths — that's the demo gate per DESIGN.md §7.2.
//!
//! # Allowed pedantic lints
//!
//! Same FFI cast pattern as the other in-tree plugins — see
//! `plugins/cppexport/src/lib.rs` for the shared rationale.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
mod dialog;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
mod dock;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
mod imp;
#[cfg(target_os = "macos")]
mod objc;
