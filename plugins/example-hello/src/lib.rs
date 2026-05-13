//! In-tree sample plugin for Code++.
//!
//! Demonstrates the full Notepad++-compatible plugin lifecycle:
//! `setInfo` stash, `getName` / `getFuncsArray` / `isUnicode`
//! identification, and a single menu command that inserts "Hello
//! from plugin" at the editor's current caret. The insertion path
//! exercises both the inbound NPPM dispatcher (the plugin queries
//! `NPPM_GETCURRENTSCINTILLA` to learn which view is active) and
//! direct Scintilla messaging (`SCI_INSERTTEXT` against the
//! returned view's HWND).
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

#[cfg(target_os = "windows")]
mod imp;
