//! GTK plugin-host wiring.
//!
//! The plugin host, discovery, lifecycle, and NPPM/NPPN dispatcher are
//! all cross-platform (`codepp-plugin-host` + `codepp-shell`). This
//! module supplies the three GTK-specific pieces:
//!
//! 1. **The message-routing bridge.** On Windows a plugin's
//!    `SendMessage(scintillaHandle, SCI_*, …)` is routed by the OS
//!    message pump for free — the handle *is* the Scintilla window. A
//!    Linux plugin `.so` has no Scintilla linked and there is no OS
//!    pump, so the SDK forwards every `SendMessage` to a host callback.
//!    [`plugin_dispatch`] is that callback: it routes **by handle
//!    identity** (`SCI` and `NPPM` message numbers overlap, so routing
//!    by range is impossible) — the [`NPP_SENTINEL`] address goes to the
//!    host dispatcher, everything else is a Scintilla `GtkWidget*` and
//!    goes to `scintilla_send_message`.
//! 2. **The Plugins menu** — lazy-load on first open, then a submenu per
//!    plugin built from its `FuncItem`s.
//! 3. **Notification delivery** — draining the shell's queued `NPPN_*`
//!    notifications to every loaded plugin's `beNotified`.
//!
//! # Re-entrancy
//!
//! A plugin menu callback and `beNotified` are invoked with **no**
//! `with_state` borrow held (the caller looks up the function pointer,
//! drops the borrow, then calls) so the plugin's own re-entrant `NPPM_*`
//! calls acquire a fresh borrow and actually work. This is the
//! memory-safe GTK equivalent of Win32's `PLUGIN_CALL_ACTIVE` guard;
//! `with_state`'s `try_borrow_mut` already declines true re-entry.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};

use gtk::prelude::*;

use codepp_plugin_host::{HostDispatchFn, NppData};
use codepp_scintilla_sys::scintilla_send_message;
use codepp_shell::HostHandles;

use crate::state::with_state;

/// The one legitimate Scintilla widget pointer, cached so
/// [`plugin_dispatch`] can identity-check the handle a plugin routes an
/// `SCI_*` message to and **refuse any other pointer** — matching Win32's
/// `SendMessage` to an unknown `HWND`, which returns 0 without
/// dereferencing. Without this, a plugin passing a garbage pointer would
/// fault inside `scintilla_send_message` (a raw dereference), where Win32
/// fails soft. Read as an atomic rather than through `with_state`, so the
/// check still works when a plugin sends `SCI_*` from inside a
/// `beNotified` that holds the borrow. Set once at startup by [`discover`].
static VALID_SCI: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// A dedicated sentinel whose *address* is the GTK backend's "npp
/// handle". A plugin sends `NPPM_*` to this pointer; [`plugin_dispatch`]
/// recognises it by identity and routes to the host dispatcher, while
/// any other pointer is treated as a Scintilla widget. The **same**
/// address fills `NppData.npp_handle`, `HostHandles.npp_hwnd`, and every
/// outbound `nmhdr.hwndFrom`, so a plugin that caches the host handle
/// routes back here rather than into `scintilla_send_message`.
static NPP_SENTINEL: u8 = 0;

/// The npp-handle sentinel pointer. Stable for the process lifetime.
fn npp_sentinel() -> *mut c_void {
    std::ptr::addr_of!(NPP_SENTINEL).cast_mut().cast::<c_void>()
}

/// Whether `hwnd` is the host's own Scintilla widget (the only pointer
/// [`plugin_dispatch`] will forward an `SCI_*` message to).
fn is_valid_scintilla(hwnd: *mut c_void) -> bool {
    let valid = VALID_SCI.load(Ordering::Acquire);
    !valid.is_null() && std::ptr::eq(hwnd, valid)
}

/// The routing callback the SDK forwards a plugin's `SendMessage` to.
///
/// `hwnd == npp_sentinel()` → an `NPPM_*` message for the host
/// dispatcher; anything else → an `SCI_*` message for that Scintilla
/// widget. Wrapped in `catch_unwind`: it is entered from plugin C code,
/// and a Rust panic unwinding across that frame is UB (dev builds
/// default to unwind).
extern "C" fn plugin_dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    catch_unwind(AssertUnwindSafe(|| {
        if std::ptr::eq(hwnd, npp_sentinel()) {
            dispatch_nppm(msg, wparam, lparam)
        } else if is_valid_scintilla(hwnd) {
            // SCI_* addressed to *our* Scintilla widget: send it straight
            // to Scintilla's GTK message entry point — the analogue of
            // Win32 routing SendMessage to the Scintilla HWND. `with_state`
            // is deliberately not taken (this is a direct Scintilla call,
            // and the plugin may issue it from inside an NPPM dispatch that
            // already holds the borrow); the identity check is an atomic
            // read for the same reason.
            //
            // SAFETY: `hwnd` is identity-checked to be the host's own live
            // `ScintillaObject*`; `scintilla_send_message` is its
            // documented entry point. The message-argument contract is the
            // plugin's responsibility, exactly as on Win32.
            unsafe { scintilla_send_message(hwnd, msg, wparam, lparam) }
        } else {
            // Any other pointer: refuse rather than dereference an
            // unvalidated address, matching Win32 `SendMessage` to an
            // unknown/dangling HWND (returns 0, no dereference).
            0
        }
    }))
    .unwrap_or(0)
}

/// Route an `NPPM_*` message to the shared dispatcher, building the GTK
/// `HostHandles` from live state. Returns 0 when state is unavailable
/// (re-entrant borrow, or after teardown) — the same "message declined"
/// outcome Win32 produces when a plugin re-enters during a guarded call.
fn dispatch_nppm(msg: u32, wparam: usize, lparam: isize) -> isize {
    with_state(|st| {
        let handles = HostHandles {
            npp_hwnd: npp_sentinel(),
            scintilla_main: st.sci_ptr,
            // Single-view on GTK, like Win32 today.
            scintilla_secondary: std::ptr::null_mut(),
            // No host-owned GtkMenu handle is exposed yet;
            // `NPPM_GETMENUHANDLE` degrades to NULL. No in-tree plugin
            // needs it, and a menu pointer would be a wider surface to
            // hand a plugin than the demo warrants.
            plugin_menu: std::ptr::null_mut(),
            main_menu: std::ptr::null_mut(),
        };
        let (shell, mut ui) = st.split();
        // SAFETY: called synchronously on the UI thread from plugin code,
        // with `(msg, wparam, lparam)` the plugin passed to `SendMessage`;
        // every `handles` field belongs to this one window.
        unsafe { shell.dispatch_plugin_message(&mut ui, handles, msg, wparam, lparam) }.unwrap_or(0)
    })
    .unwrap_or(0)
}

/// The `NppData` handed to each plugin's `setInfo`: the npp sentinel plus
/// the Scintilla widget pointer.
fn npp_data() -> NppData {
    let sci = with_state(|st| st.sci_ptr).unwrap_or(std::ptr::null_mut());
    NppData {
        npp_handle: npp_sentinel(),
        scintilla_main_handle: sci,
        scintilla_second_handle: std::ptr::null_mut(),
    }
}

/// Discover plugins under the config dir's `plugins/` folder. Records
/// paths only (deferred load — DESIGN.md §6.4); the first Plugins-menu
/// open loads them. Called once at startup.
pub(crate) fn discover() {
    // Cache the host's Scintilla widget pointer for `plugin_dispatch`'s
    // identity check (see [`VALID_SCI`]). Runs once at startup, when the
    // single view already exists.
    if let Some(sci) = with_state(|st| st.sci_ptr) {
        VALID_SCI.store(sci, Ordering::Release);
    }
    let Some(dir) = codepp_platform::plugins_dir() else {
        return;
    };
    let found = with_state(|st| st.shell.discover_plugins(&dir));
    match found {
        Some(Ok(n)) => tracing::info!(count = n, dir = ?dir, "discovered plugins"),
        Some(Err(err)) => tracing::warn!(?err, dir = ?dir, "plugin discovery failed"),
        None => {}
    }
}

/// Lazy-load every pending plugin and rebuild the Plugins menu from the
/// loaded set. Called from the Plugins menu's `show` handler.
pub(crate) fn ensure_loaded_and_rebuild(menu: &gtk::Menu) {
    // Load pending plugins, installing the GTK routing callback into each
    // (the SDK handshake) so their `SendMessage` reaches us.
    let dispatch: Option<HostDispatchFn> = Some(plugin_dispatch);
    let data = npp_data();
    with_state(|st| st.shell.ensure_plugins_loaded(data, dispatch));
    rebuild_menu(menu);
    // NPPN_READY fired synchronously inside `ensure_plugins_loaded`; any
    // notifications a plugin queued back are drained on the next wake.
    crate::drain_shell();
}

/// Rebuild the Plugins menu: one submenu per loaded plugin, its items
/// taken from the plugin's `FuncItem` array (null `p_func` → separator).
/// A greyed "No plugins loaded" placeholder when the set is empty.
fn rebuild_menu(menu: &gtk::Menu) {
    for child in menu.children() {
        menu.remove(&child);
    }
    let entries = with_state(|st| {
        st.shell
            .loaded_plugin_funcs()
            .map(|(name, funcs)| {
                let items: Vec<(String, i32, bool)> = funcs
                    .iter()
                    .map(|f| (funcitem_label(f), f.cmd_id, f.p_func.is_some()))
                    .collect();
                (name, items)
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();

    if entries.is_empty() {
        let placeholder = gtk::MenuItem::with_label("No plugins loaded");
        placeholder.set_sensitive(false);
        menu.append(&placeholder);
        menu.show_all();
        return;
    }

    for (name, items) in entries {
        let submenu = gtk::Menu::new();
        for (label, cmd_id, is_command) in items {
            if is_command {
                let item = gtk::MenuItem::with_label(&label);
                item.connect_activate(move |_| on_plugin_command(cmd_id));
                submenu.append(&item);
            } else {
                submenu.append(&gtk::SeparatorMenuItem::new());
            }
        }
        // Sanitize the plugin-supplied display name — a plugin is an
        // untrusted source of chrome text, same policy as filenames.
        let top = gtk::MenuItem::with_label(&codepp_shell::sanitize_str_for_display(&name));
        top.set_submenu(Some(&submenu));
        menu.append(&top);
    }
    menu.show_all();
}

/// Decode a `FuncItem`'s NUL-terminated UTF-16 `item_name` to a String,
/// sanitized for display (a plugin's menu labels are untrusted chrome).
fn funcitem_label(f: &codepp_plugin_host::FuncItem) -> String {
    let end = f
        .item_name
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(f.item_name.len());
    let raw = String::from_utf16_lossy(&f.item_name[..end]);
    codepp_shell::sanitize_str_for_display(&raw)
}

/// Invoke a plugin's menu command. Looks the function pointer up (a short
/// `with_state` borrow), drops the borrow, then calls the plugin outside
/// it — so the plugin's re-entrant `NPPM_*` calls get a fresh borrow —
/// under `catch_unwind` (a panic must not cross the C frame).
fn on_plugin_command(cmd_id: i32) {
    let cmd = with_state(|st| st.shell.lookup_plugin_command(cmd_id)).flatten();
    let Some(cmd) = cmd else {
        return;
    };
    // SAFETY: `cmd` is a plugin `FuncItem.p_func`, invoked on the UI
    // thread with no arguments, per the N++ ABI. `catch_unwind` keeps a
    // Rust-plugin panic from unwinding across `extern "C"`.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { cmd() }));
    // A command may have edited the buffer, changed status, or queued
    // notifications; flush the wake pipeline.
    crate::drain_shell();
}

/// Deliver every queued `NPPN_*` notification to the loaded plugins.
/// Called after each drain. Each `beNotified` runs with no `with_state`
/// borrow held by us beyond the immutable one `notify_plugins` needs, so
/// the plugin's `beNotified`-time `NPPM_*` calls are declined the same
/// way Win32 declines them during its guarded notify (documented parity).
pub(crate) fn deliver_notifications() {
    let notes = with_state(|st| st.shell.take_notifications()).unwrap_or_default();
    for note in notes {
        with_state(|st| st.shell.notify_plugins(note, npp_sentinel()));
    }
}
