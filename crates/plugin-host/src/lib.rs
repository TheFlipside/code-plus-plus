//! Notepad++-compatible plugin host for Code++.
//!
//! Phase 3 milestone 2 landed the C-ABI types (`ffi`) and the
//! discovery / loading / lifecycle layer (`host`). Milestone 3 adds
//! the NPPM/NPPN dispatcher (`dispatch`): synchronous handlers for
//! the v1 NPPM_* set and a notify-all path for NPPN_* delivery.
//! Menu integration and the in-tree `example-hello` plugin land in
//! the next milestone. See DESIGN.md §6.

#[cfg(target_os = "windows")]
pub mod dispatch;
#[cfg(target_os = "windows")]
pub mod ffi;
#[cfg(target_os = "windows")]
pub mod host;

#[cfg(target_os = "windows")]
pub use ffi::{
    BeNotifiedFn, FuncItem, GetFuncsArrayFn, GetNameFn, Hwnd, IsUnicodeFn, MessageProcFn, NppData,
    PluginCmd, SCNotification, SciNotifyHeader, SetInfoFn, ShortcutKey, MENU_TITLE_LENGTH,
};

#[cfg(target_os = "windows")]
pub use dispatch::{
    dispatch_nppm, notify_all, HostServices, Notification, ALL_OPEN_FILES, MAC_FORMAT, NPPMAINMENU,
    NPPMSG, NPPMSG_RANGE, NPPPLUGINMENU, PRIMARY_VIEW, SECOND_VIEW, UNIX_FORMAT, UNI_7BIT,
    UNI_8BIT, UNI_COOKIE, UNI_END, UNI_UTF16BE, UNI_UTF16BE_NO_BOM, UNI_UTF16LE,
    UNI_UTF16LE_NO_BOM, UNI_UTF8, WIN_FORMAT,
};

#[cfg(target_os = "windows")]
pub use host::{PluginAdminEntry, PluginHost, PluginInfo, PLUGIN_CMD_ID_BASE};
