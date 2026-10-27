//! Code++ preinstalled plugin: ASCII↔HEX selection conversion.
//!
//! Clean-room MIT reimplementation of the canonical Notepad++
//! `NppConverter` plugin. Replaces the active selection with its hex
//! form, in upstream's default format, or vice versa: two menu items,
//! ASCII → HEX and HEX → ASCII.
//!
//! See DESIGN.md §6.6 for the rationale.
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
mod imp;
