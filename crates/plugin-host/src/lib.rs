//! Notepad++-compatible plugin host for Code++.
//!
//! Phase 3 milestone 2 landed the C-ABI types (`ffi`) and the
//! discovery / loading / lifecycle layer (`host`). Milestone 3 adds
//! the NPPM/NPPN dispatcher (`dispatch`): synchronous handlers for
//! the v1 NPPM_* set and a notify-all path for NPPN_* delivery.
//! Menu integration and the in-tree `example-hello` plugin land in
//! the next milestone. See DESIGN.md §6.
//!
//! # Allowed pedantic lints, with rationale
//!
//! - `clippy::cast_possible_truncation`
//! - `clippy::cast_possible_wrap`
//! - `clippy::cast_sign_loss`
//!
//! Plugin-host is the NPPM/NPPN dispatcher — every public
//! handler in this crate translates between Win32 plugin ABI
//! types (`isize`, `i32`, `usize`, `LRESULT`) and Rust types,
//! and the value-range invariants come from Notepad++'s
//! documented ABI (and from `Notepad_plus_msgs.h` in
//! `plugins/nppcompat-headers/`), not from Rust's type
//! system. Marking each cast `#[allow(...)]` individually
//! would add 75+ attribute lines to the dispatch routes; the
//! inner attributes here document the trade-off once.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

#[cfg(target_os = "windows")]
pub mod dispatch;
#[cfg(target_os = "windows")]
pub mod ffi;
#[cfg(target_os = "windows")]
pub mod host;

#[cfg(target_os = "windows")]
pub use ffi::{
    BeNotifiedFn, CommunicationInfo, FuncItem, GetFuncsArrayFn, GetNameFn, Hwnd, IsUnicodeFn,
    MessageProcFn, NppDarkModeColors, NppData, PluginCmd, SCNotification, SciNotifyHeader,
    SessionInfo, SetInfoFn, ShortcutKey, TbData, TbRect, ToolbarIcons, DMN_CLOSE, DMN_DOCK,
    DMN_FIRST, DMN_FLOAT, DWS_ADDINFO, DWS_DF_CONT_BOTTOM, DWS_DF_CONT_LEFT, DWS_DF_CONT_RIGHT,
    DWS_DF_CONT_TOP, DWS_DF_FLOATING, DWS_ICONBAR, DWS_ICONTAB, DWS_USEOWNDARKMODE,
    MENU_TITLE_LENGTH,
};

#[cfg(target_os = "windows")]
pub use dispatch::{
    dispatch_nppm, notify_all, DockDialogParams, HostServices, Notification, ALL_OPEN_FILES,
    LINENUMWIDTH_CONSTANT, LINENUMWIDTH_DYNAMIC, MAC_FORMAT, MODELESSDIALOGADD,
    MODELESSDIALOGREMOVE, NPPMAINMENU, NPPMSG, NPPMSG_RANGE, NPPPLUGINMENU, PRIMARY_VIEW,
    RUNCOMMAND_RANGE, RUNCOMMAND_USER, SECOND_VIEW, UNIX_FORMAT, UNI_7BIT, UNI_8BIT, UNI_COOKIE,
    UNI_END, UNI_UTF16BE, UNI_UTF16BE_NO_BOM, UNI_UTF16LE, UNI_UTF16LE_NO_BOM, UNI_UTF8,
    WIN_FORMAT,
};

#[cfg(target_os = "windows")]
pub use host::{
    PluginAdminEntry, PluginHost, PluginInfo, PLUGIN_ALLOC_CMD_BASE, PLUGIN_ALLOC_CMD_LIMIT,
    PLUGIN_ALLOC_MARKER_BASE, PLUGIN_ALLOC_MARKER_LIMIT, PLUGIN_CMD_ID_BASE,
};
