//! Code++ preinstalled plugin: export the active buffer to HTML
//! preserving the active lexer's styling.
//!
//! Clean-room MIT reimplementation of the canonical Notepad++
//! `NppExport` plugin. Two menu items in this scaffold (Export to
//! HTML…, Copy HTML to Clipboard); the bodies are filled in a
//! follow-up Phase 4 m7 commit. RTF output is deferred — a follow-up
//! commit may add it once HTML output is stable.
//!
//! See DESIGN.md §6.6 for the rationale.
//!
//! # Allowed pedantic lints, with rationale
//!
//! - `clippy::cast_possible_truncation`
//! - `clippy::cast_possible_wrap`
//! - `clippy::cast_sign_loss`
//!
//! This plugin manipulates Win32 / Scintilla / N++-ABI integer
//! shapes (style indices, Scintilla position values, colour
//! channels). Every `as` cast is a deliberate translation; the
//! value-range invariants come from Scintilla's documented ABI,
//! not Rust's type system.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

#[cfg(target_os = "windows")]
mod imp;
