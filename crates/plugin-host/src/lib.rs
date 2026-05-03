//! Notepad++-compatible plugin host for Code++.
//!
//! Phase 3 milestone 2 lands the C-ABI types (`ffi`) and the
//! discovery / loading / lifecycle layer (`host`). The NPPM/NPPN
//! dispatcher and menu integration land alongside the in-tree
//! `example-hello` plugin in a follow-up commit. See DESIGN.md §6.

#[cfg(target_os = "windows")]
pub mod ffi;
#[cfg(target_os = "windows")]
pub mod host;

#[cfg(target_os = "windows")]
pub use ffi::{
    BeNotifiedFn, FuncItem, GetFuncsArrayFn, GetNameFn, IsUnicodeFn, MessageProcFn, NppData,
    PluginCmd, SetInfoFn, ShortcutKey, MENU_TITLE_LENGTH,
};

#[cfg(target_os = "windows")]
pub use host::{PluginHost, PluginInfo};
