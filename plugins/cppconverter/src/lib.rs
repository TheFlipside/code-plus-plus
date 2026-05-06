//! Code++ preinstalled plugin: ASCII↔HEX selection conversion.
//!
//! Clean-room MIT reimplementation of the canonical Notepad++
//! `NppConverter` plugin. Replaces the active selection with its
//! space-separated hex form, or vice versa. Two menu items in this
//! scaffold (ASCII → HEX, HEX → ASCII); the bodies are filled in a
//! follow-up Phase 4 m7 commit.
//!
//! See DESIGN.md §6.6 for the rationale.

#[cfg(target_os = "windows")]
mod imp;
