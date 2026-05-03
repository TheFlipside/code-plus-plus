//! OS-specific utilities for Code++.
//!
//! All Win32/Linux/macOS API calls happen here, gated by
//! `#[cfg(target_os = "...")]`. Higher crates depend on this rather
//! than reaching for the raw OS bindings themselves. See
//! DESIGN.md §2.2.

pub mod config;
pub mod watch;

pub use config::{config_dir, config_xml_path, plugins_dir, session_xml_path};
pub use watch::{FileChange, FileWatcher};
