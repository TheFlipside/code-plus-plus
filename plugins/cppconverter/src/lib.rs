//! Code++ preinstalled plugin: ASCII↔HEX selection conversion.
//!
//! Clean-room MIT reimplementation of the canonical Notepad++
//! `NppConverter` plugin. Replaces the active selection with its
//! space-separated hex form, or vice versa. Two menu items in this
//! scaffold (ASCII → HEX, HEX → ASCII); the bodies are filled in a
//! follow-up Phase 4 m7 commit.
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

#[cfg(target_os = "windows")]
mod imp;
