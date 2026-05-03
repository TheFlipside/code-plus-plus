//! OS-specific utilities. All Win32/Linux/macOS calls happen here, gated by
//! `#[cfg(target_os = "...")]`. Higher crates depend on this rather than
//! reaching for the raw OS bindings themselves.
//!
//! Phase 0: empty.
