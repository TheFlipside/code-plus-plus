//! OS-specific utilities for Code++.
//!
//! All Win32/Linux/macOS API calls happen here, gated by
//! `#[cfg(target_os = "...")]`. Higher crates depend on this rather
//! than reaching for the raw OS bindings themselves. See
//! DESIGN.md §2.2.

pub mod config;
pub mod dynlib;
pub mod host_env;
pub mod watch;

pub use config::{
    backups_dir, config_dir, config_xml_path, disabled_plugins_path, find_history_xml_path,
    plugins_config_dir, plugins_dir, session_xml_path,
};
pub use dynlib::{has_plugin_extension, DynLib, PLUGIN_EXTENSION};
pub use host_env::{program_dir, program_path, windows_version_npp};
pub use watch::{FileChange, FileWatcher};
