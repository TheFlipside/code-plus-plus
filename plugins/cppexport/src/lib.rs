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

#[cfg(target_os = "windows")]
mod imp;
