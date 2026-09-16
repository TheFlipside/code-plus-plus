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

use gtk::glib;
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
    // The load may have absorbed new plugin-shortcut defaults; rebuild
    // the accel group so their chords become live this session (Win32
    // does the equivalent `refresh_plugin_accels` after its populate).
    rebuild_plugin_accel_group();
    // NPPN_READY fired synchronously inside `ensure_plugins_loaded`; any
    // notifications a plugin queued back are drained on the next wake.
    crate::drain_shell();
}

/// One row of a plugin submenu: label, command id, whether it is a
/// command (vs. a separator), and its display chord if any.
type PluginMenuRow = (String, i32, bool, Option<(bool, bool, bool, u8)>);

/// Rebuild the Plugins menu: one submenu per loaded plugin (its items
/// taken from the plugin's `FuncItem` array, null `p_func` → separator),
/// or a greyed "No plugins loaded" placeholder when empty. Then, always,
/// a separator and the admin entries "Plugin Manager…" + "Open Plugin
/// Folder" — matching Win32's layout (per-plugin entries, separator,
/// admin items), so the manager is reachable even to re-enable plugins
/// the user previously disabled.
fn rebuild_menu(menu: &gtk::Menu) {
    for child in menu.children() {
        menu.remove(&child);
    }
    let entries = with_state(|st| {
        st.shell
            .loaded_plugin_funcs()
            .map(|(name, funcs)| {
                let items: Vec<PluginMenuRow> = funcs
                    .iter()
                    .map(|f| {
                        (
                            funcitem_label(f),
                            f.cmd_id,
                            f.p_func.is_some(),
                            st.shell.plugin_shortcut_chord_for_cmd_id(f.cmd_id),
                        )
                    })
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
    } else {
        for (name, items) in entries {
            let submenu = gtk::Menu::new();
            for (label, cmd_id, is_command, chord) in items {
                if is_command {
                    let item = gtk::MenuItem::with_label(&label);
                    item.connect_activate(move |_| on_plugin_command(cmd_id));
                    // Show the shortcut hint via the display-only accel
                    // group (never routes the key — the real binding is
                    // `register_startup_shortcuts`). Only a chord that
                    // will actually fire is advertised (the shell
                    // filtered policy-refused / dedupe-losing chords).
                    if let Some((ctrl, alt, shift, key)) = chord {
                        if let Some((gdk_key, mods)) = chord_to_gdk(ctrl, alt, shift, key) {
                            PLUGIN_HINT_ACCEL.with(|h| {
                                if let Some(hint) = h.borrow().as_ref() {
                                    item.add_accelerator(
                                        "activate",
                                        hint,
                                        *gdk_key,
                                        mods,
                                        gtk::AccelFlags::VISIBLE,
                                    );
                                }
                            });
                        }
                    }
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
    }

    menu.append(&gtk::SeparatorMenuItem::new());
    let manager = gtk::MenuItem::with_mnemonic("_Plugin Manager…");
    manager.connect_activate(|_| show_plugin_manager());
    menu.append(&manager);
    let folder = gtk::MenuItem::with_mnemonic("_Open Plugin Folder");
    folder.connect_activate(|_| open_plugin_folder());
    menu.append(&folder);

    menu.show_all();
}

/// Show the modal Plugin Manager: every discovered plugin with an Enabled
/// checkbox and a status column. Toggling a checkbox writes through to
/// `<plugins_config_dir>/disabled.txt` via `Shell::set_plugin_disabled`;
/// the change takes effect on the next launch (Notepad++'s
/// restart-required semantics), mirroring the Win32 Plugin Manager.
fn show_plugin_manager() {
    let Some(window) = with_state(|st| st.window.clone()) else {
        return;
    };
    let dialog = gtk::Dialog::with_buttons(
        Some("Plugin Manager"),
        Some(&window),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[("_Close", gtk::ResponseType::Close)],
    );
    dialog.set_default_size(520, 380);
    let content = dialog.content_area();
    content.set_spacing(6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);

    // Columns: enabled (toggle) | plugin name | status | registry index.
    // The index is the functional value `set_plugin_disabled` takes; it is
    // kept out of the visible columns.
    let store = gtk::ListStore::new(&[
        glib::Type::BOOL,
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::U64,
    ]);
    let tree = gtk::TreeView::with_model(&store);

    let toggle = gtk::CellRendererToggle::new();
    let store_toggle = store.clone();
    toggle.connect_toggled(move |_, path| {
        let Some(iter) = store_toggle.iter(&path) else {
            return;
        };
        // Fail safe rather than fall back to a substitute row: a type
        // mismatch here would otherwise silently toggle plugin index 0 (or
        // the wrong enabled state). The model is first-party and correctly
        // typed, so this never triggers today, but a future column-order
        // change fails closed instead of mutating the wrong plugin.
        let (Ok(was_enabled), Ok(index)) = (
            store_toggle.value(&iter, 0).get::<bool>(),
            store_toggle.value(&iter, 3).get::<u64>(),
        ) else {
            return;
        };
        let now_enabled = !was_enabled;
        // `disabled == !enabled`. Persists to disabled.txt; effective next
        // launch (an already-loaded plugin isn't unloaded mid-session).
        with_state(|st| st.shell.set_plugin_disabled(index as usize, !now_enabled));
        store_toggle.set_value(&iter, 0, &now_enabled.to_value());
    });
    let enabled_col = gtk::TreeViewColumn::new();
    enabled_col.set_title("Enabled");
    gtk::prelude::TreeViewColumnExt::pack_start(&enabled_col, &toggle, false);
    gtk::prelude::TreeViewColumnExt::add_attribute(&enabled_col, &toggle, "active", 0);
    tree.append_column(&enabled_col);
    append_admin_text_column(&tree, "Plugin", 1, 260);
    append_admin_text_column(&tree, "Status", 2, 200);

    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(240)
        .build();
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.add(&tree);
    content.pack_start(&scroll, true, true, 0);

    let hint = gtk::Label::new(Some(
        "Enabling or disabling a plugin takes effect the next time Code++ starts.",
    ));
    hint.set_xalign(0.0);
    hint.set_line_wrap(true);
    content.pack_start(&hint, false, false, 0);

    // Populate from the shell's admin snapshot (index-keyed; sanitized).
    let admin = with_state(|st| st.shell.installed_plugins()).unwrap_or_default();
    for entry in &admin {
        let status = if entry.loaded {
            "Loaded".to_string()
        } else if let Some(reason) = &entry.failed_reason {
            format!("Failed: {reason}")
        } else {
            "Not loaded".to_string()
        };
        store.insert_with_values(
            None,
            &[
                (0, &(!entry.disabled)),
                (
                    1,
                    &codepp_shell::sanitize_str_for_display(&entry.display_label),
                ),
                (2, &codepp_shell::sanitize_str_for_display(&status)),
                (3, &(entry.index as u64)),
            ],
        );
    }

    dialog.show_all();
    {
        // `dialog.run()` spins a nested GTK main loop where the §5.4 worker
        // wake is still dispatched — so without this a completed load could
        // `drain` the shell (moving `active_tab`, rebinding the view, or
        // stacking a dialog) underneath the modal. Same guard the
        // close-confirm modal takes, and the twin of `ui_cocoa`'s. The
        // manager's snapshot is index-keyed against a registry nothing
        // worker-driven mutates, so the realistic worst case without this is
        // a stale row rather than the wrong plugin toggled — but "a user
        // can't reach it" stops holding the moment a plugin or timer can
        // touch the shell, so it is closed rather than argued away. See
        // [`crate::DrainFreeze`]. The freeze lifts on scope exit (a panic in
        // a handler included), so the flush below always runs unfrozen.
        let _freeze = crate::DrainFreeze::new();
        dialog.run();
    }
    // SAFETY: created here, never handed out — same idiom as the Rename /
    // Goto modal dialogs.
    unsafe {
        dialog.destroy();
    }
    // Unfrozen now: flush anything a worker completed while the modal held
    // the main loop, applied against the current state.
    crate::drain_shell();
}

/// Append a resizable left-aligned text column bound to `model_col`.
fn append_admin_text_column(tree: &gtk::TreeView, title: &str, model_col: u32, width: i32) {
    let renderer = gtk::CellRendererText::new();
    let column = gtk::TreeViewColumn::new();
    column.set_title(title);
    column.set_resizable(true);
    column.set_fixed_width(width);
    gtk::prelude::TreeViewColumnExt::pack_start(&column, &renderer, true);
    gtk::prelude::TreeViewColumnExt::add_attribute(&column, &renderer, "text", model_col as i32);
    tree.append_column(&column);
}

/// Open the plugins directory in the desktop file manager — the GTK
/// analogue of Win32's "Open Plugin Folder" (`ShellExecute` open on the
/// folder). `create_dir_all` first so a click before any plugin has been
/// staged still targets a valid path.
fn open_plugin_folder() {
    let Some(window) = with_state(|st| st.window.clone()) else {
        return;
    };
    let Some(dir) = codepp_platform::plugins_dir() else {
        tracing::warn!("no config dir; cannot open the plugins folder");
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(?err, "could not create the plugins folder");
        return;
    }
    match glib::filename_to_uri(&dir, None) {
        Ok(uri) => {
            if let Err(err) =
                gtk::show_uri_on_window(Some(&window), &uri, gtk::current_event_time())
            {
                tracing::warn!(
                    ?err,
                    ?uri,
                    "show_uri_on_window failed for the plugins folder"
                );
            }
        }
        Err(err) => tracing::warn!(?err, "filename_to_uri failed for the plugins folder"),
    }
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

thread_local! {
    /// Display-only accel group for plugin menu-item shortcut hints.
    /// Never added to a window, so `add_accelerator` on it renders
    /// the "Ctrl+H" label without routing the key — the real,
    /// always-on binding is [`rebuild_plugin_accel_group`]'s
    /// `connect_accel_group` on the plugin accel group. Same
    /// display-hint discipline as the File menu's `FILE_HINT_ACCEL`.
    static PLUGIN_HINT_ACCEL: std::cell::RefCell<Option<gtk::AccelGroup>> =
        const { std::cell::RefCell::new(None) };
    /// The live plugin accelerator group attached to the main window.
    /// Rebuilt from the current [`codepp_shell::Shell::startup_plugin_chords`]
    /// whenever the cache changes (startup, and after each plugin
    /// load) so a shortcut absorbed this session becomes live without
    /// a restart, and a removed one stops being registered. The
    /// fire-time [`fire_plugin_chord`] check is the belt to this
    /// suspenders — it covers an `NPPM_REMOVESHORTCUTBYCMDID` between
    /// rebuilds.
    static PLUGIN_ACCEL_GROUP: std::cell::RefCell<Option<gtk::AccelGroup>> =
        const { std::cell::RefCell::new(None) };
}

/// Map a portable key to its GDK keyval. `None` for keys with no
/// faithful GDK identity (`core::shortcuts::portable_key` already
/// declined the layout-dependent ones, so this only fails if a name
/// lookup does).
fn portable_key_to_gdk(pk: codepp_core::shortcuts::PortableKey) -> Option<gtk::gdk::keys::Key> {
    use codepp_core::shortcuts::{NamedKey, PortableKey};
    let key = match pk {
        // ASCII letters and digits: the GDK keyval is the codepoint.
        PortableKey::Letter(c) | PortableKey::Digit(c) => {
            gtk::gdk::keys::Key::from_unicode(c.into())
        }
        PortableKey::Function(n) => gtk::gdk::keys::Key::from_name(&format!("F{n}")),
        PortableKey::Named(n) => gtk::gdk::keys::Key::from_name(match n {
            NamedKey::Space => "space",
            NamedKey::Insert => "Insert",
            NamedKey::Delete => "Delete",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "Page_Up",
            NamedKey::PageDown => "Page_Down",
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
        }),
    };
    // `from_name` yields VoidSymbol (0) for an unknown name; treat
    // that as "no faithful mapping" rather than binding key 0.
    (*key != 0).then_some(key)
}

/// Translate a Code++ chord to a GDK `(keyval, ModifierType)`.
fn chord_to_gdk(
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: u8,
) -> Option<(gtk::gdk::keys::Key, gtk::gdk::ModifierType)> {
    let gdk_key = portable_key_to_gdk(codepp_core::shortcuts::portable_key(key)?)?;
    let mut mods = gtk::gdk::ModifierType::empty();
    if ctrl {
        mods |= gtk::gdk::ModifierType::CONTROL_MASK;
    }
    if alt {
        mods |= gtk::gdk::ModifierType::MOD1_MASK;
    }
    if shift {
        mods |= gtk::gdk::ModifierType::SHIFT_MASK;
    }
    Some((gdk_key, mods))
}

/// Build the plugin accelerator group for the first time and attach
/// it to the main window. Called once at startup **after**
/// `discover()` (the chord set is filtered to discovered plugins);
/// [`rebuild_plugin_accel_group`] does the work and is re-run after
/// each plugin load.
pub(crate) fn register_startup_shortcuts() {
    PLUGIN_HINT_ACCEL.with(|h| *h.borrow_mut() = Some(gtk::AccelGroup::new()));
    rebuild_plugin_accel_group();
}

/// (Re)build the live plugin accelerator group from the current
/// `startup_plugin_chords`. Removes the previous group from the
/// window and installs a fresh one — the GTK analogue of Win32's
/// `refresh_plugin_accels` HACCEL swap — so a chord absorbed this
/// session (a plugin's default, first seen on load) becomes live
/// immediately, and one dropped by `NPPM_REMOVESHORTCUTBYCMDID` is
/// no longer registered after the next rebuild.
///
/// Each closure fires by *chord*, not by a captured identity: it
/// re-resolves through [`fire_plugin_chord`] at press time, so the
/// live cache is the authority even between rebuilds.
pub(crate) fn rebuild_plugin_accel_group() {
    let Some(window) = with_state(|st| st.window.clone()) else {
        return;
    };
    // Detach the previous group before installing the new one.
    if let Some(old) = PLUGIN_ACCEL_GROUP.with(|g| g.borrow_mut().take()) {
        window.remove_accel_group(&old);
    }
    let accel = gtk::AccelGroup::new();
    window.add_accel_group(&accel);

    let chords = with_state(|st| st.shell.startup_plugin_chords()).unwrap_or_default();
    for chord in chords {
        let Some((gdk_key, mods)) = chord_to_gdk(chord.ctrl, chord.alt, chord.shift, chord.key)
        else {
            tracing::debug!(
                module = chord.module_key.as_str(),
                key = chord.key,
                "plugin shortcut has no GDK mapping; skipped on GTK"
            );
            continue;
        };
        let (ctrl, alt, shift, key) = (chord.ctrl, chord.alt, chord.shift, chord.key);
        accel.connect_accel_group(
            *gdk_key,
            mods,
            gtk::AccelFlags::VISIBLE,
            // Return whether the chord actually dispatched: `false`
            // lets GTK propagate the key to the editor, so a chord
            // whose plugin/command no longer resolves (e.g. a bogus
            // hand-edited `internalID`) does not silently swallow the
            // keystroke.
            move |_, _, _, _| fire_plugin_chord(ctrl, alt, shift, key),
        );
    }
    PLUGIN_ACCEL_GROUP.with(|g| *g.borrow_mut() = Some(accel));
}

/// Fire the plugin shortcut bound to a pressed chord. Resolves the
/// chord against the **live** cache ([`codepp_shell::Shell::match_plugin_chord`]),
/// lazy-loads every pending plugin (the hotkey is the §6.4 load
/// trigger), resolves the identity to the loaded command, and
/// dispatches it. Returns `true` iff a command actually ran — the
/// accel-group closure propagates the key to the editor on `false`,
/// and a removed binding (`match` returns `None`) fires nothing.
fn fire_plugin_chord(ctrl: bool, alt: bool, shift: bool, key: u8) -> bool {
    // Live-cache check first: a chord removed via NPPM (or otherwise
    // no longer registrable) must not fire, even though its closure
    // is still installed until the next rebuild.
    let Some((module_key, internal_id)) =
        with_state(|st| st.shell.match_plugin_chord(ctrl, alt, shift, key)).flatten()
    else {
        return false;
    };
    let data = npp_data();
    let dispatch: Option<HostDispatchFn> = Some(plugin_dispatch);
    with_state(|st| st.shell.ensure_plugins_loaded(data, dispatch));
    // The load may have absorbed new defaults — for *other* commands
    // than the one just pressed (`ensure_plugins_loaded` loads every
    // pending plugin). Rebuild the accel group so those become live
    // this session, matching Win32's `refresh_plugin_accels` after
    // `handle_plugin_shortcut_shim`'s load. Deferred to a glib idle
    // rather than done inline: this runs *inside* the accel group's
    // own closure, and tearing the group down under itself is the
    // kind of reentrancy this backend has been bitten by before.
    glib::idle_add_local_once(rebuild_plugin_accel_group);
    let cmd_id = with_state(|st| st.shell.resolve_plugin_command(&module_key, internal_id))
        .flatten()
        .map(|(cmd_id, _)| cmd_id);
    if let Some(cmd_id) = cmd_id {
        on_plugin_command(cmd_id);
        true
    } else {
        // Loaded but the identity didn't resolve to a live command
        // (a stale index into the plugin's FuncItems). Drain any
        // load-time notifications, but let the key through.
        crate::drain_shell();
        false
    }
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

#[cfg(test)]
mod shortcut_tests {
    use super::chord_to_gdk;

    #[test]
    fn chord_to_gdk_maps_keys_and_modifiers() {
        // Ctrl+Alt+H -> keyval 'h' with Control+Mod1.
        let (key, mods) = chord_to_gdk(true, true, false, 0x48).unwrap();
        assert_eq!(*key, u32::from(b'h'));
        assert!(mods.contains(gtk::gdk::ModifierType::CONTROL_MASK));
        assert!(mods.contains(gtk::gdk::ModifierType::MOD1_MASK));
        assert!(!mods.contains(gtk::gdk::ModifierType::SHIFT_MASK));

        // A digit and Shift+F3.
        assert_eq!(
            *chord_to_gdk(true, false, false, 0x31).unwrap().0,
            u32::from(b'1')
        );
        let (key, mods) = chord_to_gdk(false, false, true, 0x72).unwrap();
        assert_eq!(*key, *gtk::gdk::keys::constants::F3);
        assert!(mods.contains(gtk::gdk::ModifierType::SHIFT_MASK));

        // A named key resolves.
        assert_eq!(
            *chord_to_gdk(true, false, false, 0x2E).unwrap().0,
            *gtk::gdk::keys::constants::Delete
        );

        // A layout-dependent OEM key has no portable mapping.
        assert!(chord_to_gdk(true, false, false, 0xBF).is_none());
    }
}
