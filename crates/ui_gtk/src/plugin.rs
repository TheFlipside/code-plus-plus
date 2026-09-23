//! GTK plugin-host wiring.
//!
//! The plugin host, discovery, lifecycle, and NPPM/NPPN dispatcher are
//! all cross-platform (`codepp-plugin-host` + `codepp-shell`). This
//! module supplies the four GTK-specific pieces:
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
//!    goes to `scintilla_send_message`. It also restores the **thread
//!    affinity** the missing OS pump would otherwise have provided — see
//!    [`send_sci_on_main`].
//! 2. **The Plugins menu** — lazy-load on first open, then a submenu per
//!    plugin built from its `FuncItem`s.
//! 3. **Notification delivery** — draining the shell's queued `NPPN_*`
//!    notifications to every loaded plugin's `beNotified`.
//! 4. **Plugin shortcuts** — the always-on accelerator group built from
//!    the `shortcuts.xml` cache at startup
//!    ([`register_startup_shortcuts`]) and rebuilt after every load
//!    ([`rebuild_plugin_accel_group`]), whose chords fire through
//!    [`fire_plugin_chord`] — DESIGN.md §6.4's hotkey lazy-load trigger,
//!    re-resolved against the live cache at press time.
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
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;

use gtk::glib;
use gtk::prelude::*;

use codepp_plugin_host::{HostDispatchFn, NppData};
use codepp_scintilla_sys::{scintilla_send_message, SCI_GETMODIFY};
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

/// The UI thread's id, recorded at startup by [`discover`].
///
/// [`plugin_dispatch`]'s `SCI_*` branch consults this to decide whether
/// it may call Scintilla directly or must marshal — see
/// [`send_sci_on_main`]. Deliberately *not* derived from `with_state`'s
/// thread-local: that branch must not take the borrow (a plugin can send
/// `SCI_*` from inside a `beNotified` that already holds it, where a
/// declined read would read as "not our thread" and needlessly marshal
/// a call that is already on the right thread and inside a live borrow).
/// An unset cell means startup has not reached [`discover`] yet, in
/// which case no plugin can have been handed a handle to send to.
static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Whether the caller is on the thread that owns the GTK main loop.
fn on_main_thread() -> bool {
    MAIN_THREAD.get() == Some(&std::thread::current().id())
}

/// A raw pointer carried onto the main thread by [`send_sci_on_main`].
///
/// GTK's `MainContext::invoke` requires a `Send` closure and a raw
/// pointer is not `Send`, so the crossing has to be made explicit.
struct MainThreadPtr(*mut c_void);

// SAFETY: the only pointer ever wrapped is one that has already passed
// [`is_valid_scintilla`], i.e. the host's own `ScintillaObject*`. That
// widget is created once at startup and never destroyed, removed from
// its container or reassigned (the discipline `GtkUiState::sci_widget`
// documents and a source-scan guard enforces), so the address stays live
// for the whole process. It is *dereferenced only on the main thread*,
// which is the entire point of the marshal — the value crosses threads,
// the dereference does not.
unsafe impl Send for MainThreadPtr {}

/// Run one `SCI_*` message against Scintilla on the UI thread and block
/// until it returns, for a plugin that called from its own thread.
///
/// # Why marshal rather than refuse
///
/// Off Windows the SDK forwards a plugin's `SendMessage` straight to
/// this host callback on whatever thread called it, where Win32 would
/// have had the OS marshal it onto the thread owning the window. Both
/// available answers were considered and this one is deliberate:
///
///   * **Refusing** (returning 0, as the unknown-handle branch does) is
///     three lines and trivially safe, but it is the worse failure. A
///     query — `SCI_GETLENGTH`, `SCI_GETCURRENTPOS` — comes back 0,
///     which is a *plausible* answer rather than an obviously wrong one,
///     and a mutation becomes a silent no-op. The plugin appears to work
///     while its edits vanish.
///   * **Marshaling** reproduces what the plugin was written against,
///     including its blocking semantics: a cross-thread `SendMessage`
///     also blocks until the target thread next pumps its queue, and
///     also deadlocks if that thread is meanwhile waiting on the sender.
///     So this adds no hazard Win32 does not already have — it inherits
///     the same one, which is the point.
///
/// # No timeout, deliberately
///
/// A bounded wait would have to invent a return value on expiry, and the
/// only one available is 0 — i.e. it would convert a visible stall into
/// the silent wrong answer the paragraph above rejects. `SendMessage`
/// has no timeout either (`SendMessageTimeout` is a different call that
/// plugins do not use). The unbounded wait blocks the *plugin's* worker
/// thread only; the UI thread is never a participant.
///
/// # What that costs at shutdown, and the one way it could become a hang
///
/// Once `gtk::main` returns, nothing iterates the default context again,
/// so a worker parked here stays parked until the process exits. That is
/// a leaked thread rather than a visible hang: `exit` does not wait on
/// threads nobody joined, and the UI thread is already on its way out.
///
/// **It becomes a real hang the moment host code joins plugin worker
/// threads, and nothing does today.** A hostile or merely broken plugin
/// can park a thread here deliberately — it need only call `SCI_*` from
/// a thread it never lets finish — so a future teardown path that waits
/// for plugin threads to quiesce would wait forever. If such a path is
/// ever added it must not block on plugin threads; the alternative is a
/// bounded wait here, which means solving the invented-return-value
/// problem above rather than ignoring it. Recorded per DESIGN.md §7.4's
/// practice of writing accepted risk down rather than leaving it to be
/// rediscovered.
///
/// The `recv` error arm is therefore defensive rather than a shutdown
/// path: `MainContext::default()` is a process-global singleton that is
/// never destroyed, so ordinary quit does not drop the sender. An
/// earlier version of this comment claimed it did, and that the arm was
/// the graceful teardown escape hatch — it is not, and the difference
/// matters because it is the whole reason the paragraph above has to
/// talk about leaked threads at all. The one way the arm *is* reached
/// in practice is a panic inside the hop: [`crate::at_callback_boundary`]
/// swallows it (logging at `error`), the sender is dropped unanswered,
/// and the caller gets 0 — the same answer the unknown-handle branch
/// gives, and a logged one.
fn send_sci_on_main(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    let ptr = MainThreadPtr(hwnd);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // `plugin_dispatch`'s boundary runs on the *plugin's* thread and
    // cannot cover this closure, which GLib's main loop enters through
    // its own C frames — so the hop carries its own.
    glib::MainContext::default().invoke(move || {
        crate::at_callback_boundary("plugin:sci_marshal", (), || {
            // Load-bearing, not a leftover: edition-2021 closures capture
            // disjoint fields, so without this rebind the outer `move`
            // closure would capture only `ptr.0` — a bare `*mut c_void`,
            // which is not `Send` — and `invoke`'s `Send` bound fails to
            // compile. Naming the whole `MainThreadPtr` captures the
            // wrapper that carries the `unsafe impl Send`.
            let ptr = ptr;
            // SAFETY: `ptr.0` passed `is_valid_scintilla` on the calling
            // thread and addresses the host's own permanently-live
            // `ScintillaObject*` (see `MainThreadPtr`). This closure runs
            // on the UI thread, which is the affinity GTK requires and
            // the reason the message was marshaled here at all.
            let result = unsafe { scintilla_send_message(ptr.0, msg, wparam, lparam) };
            // The receiver is alive by construction — the calling thread
            // is parked in `recv` — unless it panicked, in which case
            // dropping the result is correct.
            let _ = tx.send(result);
        });
    });
    rx.recv().unwrap_or_else(|_| {
        tracing::warn!(
            msg,
            "cross-thread SCI_* dropped: the main-thread hop never answered \
             (it panicked — see the error above — or the context is gone)"
        );
        0
    })
}

/// The routing callback the SDK forwards a plugin's `SendMessage` to.
///
/// `hwnd == npp_sentinel()` → an `NPPM_*` message for the host
/// dispatcher; anything else → an `SCI_*` message for that Scintilla
/// widget. Runs at a [`crate::at_callback_boundary`]: it is entered from
/// plugin C code, and a Rust panic unwinding across that frame is UB (dev
/// builds default to unwind).
extern "C" fn plugin_dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    // Nothing may reach here before `discover` armed the affinity check:
    // an unset `MAIN_THREAD` makes `on_main_thread` answer `false` for
    // *every* caller, so a message arriving on the UI thread would take
    // the marshal branch and park the UI thread on its own main loop.
    // It survives today only because `MainContext::invoke` dispatches
    // inline when the calling thread can acquire the context — an
    // implementation detail of GLib, not a guarantee this code should be
    // resting on. Unreachable in production (`discover` sets it
    // synchronously during startup, long before a plugin is loaded and
    // handed a handle), so this states the ordering rather than handling
    // it, in the same spirit as the crate's source-scan guards.
    //
    // Deliberately outside the boundary below, which would otherwise
    // swallow the unwind and hand the plugin a plain 0.
    debug_assert!(
        MAIN_THREAD.get().is_some(),
        "plugin_dispatch reached before discover() armed MAIN_THREAD",
    );
    crate::at_callback_boundary("plugin:dispatch", 0, || {
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
            if on_main_thread() {
                // SAFETY: `hwnd` is identity-checked to be the host's own
                // live `ScintillaObject*` and this is the thread that owns
                // it; `scintilla_send_message` is its documented entry
                // point. The message-argument contract is the plugin's
                // responsibility, exactly as on Win32.
                unsafe { scintilla_send_message(hwnd, msg, wparam, lparam) }
            } else {
                // A plugin calling from its own thread. GTK widget calls
                // are main-thread-only, so hop and block. See
                // [`send_sci_on_main`] for why this marshals rather than
                // refusing, and DESIGN.md §7.4.
                send_sci_on_main(hwnd, msg, wparam, lparam)
            }
        } else {
            // Any other pointer: refuse rather than dereference an
            // unvalidated address, matching Win32 `SendMessage` to an
            // unknown/dangling HWND (returns 0, no dereference).
            0
        }
    })
}

/// Route an `NPPM_*` message to the shared dispatcher, building the GTK
/// `HostHandles` from live state. Returns 0 when state is unavailable
/// (re-entrant borrow, or after teardown) — the same "message declined"
/// outcome Win32 produces when a plugin re-enters during a guarded call.
fn dispatch_nppm(msg: u32, wparam: usize, lparam: isize) -> isize {
    let routed = with_state(|st| {
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
        let editor = st.editor;
        let dirty_before = editor.send(SCI_GETMODIFY, 0, 0) != 0;
        let cached_before: Vec<bool> = st.shell.tabs.iter().map(|t| t.dirty).collect();
        let pre_active = st.shell.active_tab;
        let (shell, mut ui) = st.split();
        // SAFETY: called synchronously on the UI thread from plugin code,
        // with `(msg, wparam, lparam)` the plugin passed to `SendMessage`;
        // every `handles` field belongs to this one window.
        let routed =
            unsafe { shell.dispatch_plugin_message(&mut ui, handles, msg, wparam, lparam) }
                .unwrap_or(0);
        let dirty_after = editor.send(SCI_GETMODIFY, 0, 0) != 0;
        let cached_moved = shell
            .tabs
            .iter()
            .enumerate()
            .any(|(i, t)| cached_before.get(i) != Some(&t.dirty));
        let needs_rebind = shell.active_tab != pre_active
            && shell
                .active_tab
                .and_then(|i| shell.tabs.get(i))
                .is_some_and(|t| t.pending_load.is_none());
        (
            routed,
            dirty_before != dirty_after || cached_moved,
            needs_rebind,
        )
    });
    let Some((routed, dirty_edge, needs_rebind)) = routed else {
        return 0;
    };
    // A dispatch can move `active_tab` without a rebind —
    // `NPPM_SWITCHTOFILE`, `NPPM_ACTIVATEDOC`, an `NPPM_DOOPEN` that
    // dedupes onto an open tab — leaving the single view on the previous
    // tab's document while `Shell` believes another is active. That is
    // the split DESIGN.md calls the most damaging this crate can
    // produce: a save takes its path from the active tab and its bytes
    // from the bound document. Win32's NPPM arm has rebound on this
    // edge since Phase 4; this backend did not, so a plugin's switch
    // followed by `NPPM_SAVECURRENTFILE` wrote the wrong buffer to the
    // new tab's path. A tab whose load is still in flight is left to
    // `apply_load_result`, which binds on landing.
    if needs_rebind {
        crate::rebind_active_view();
    }
    // A dispatch can move a document off or onto its save point —
    // `NPPM_SETBUFFERFORMAT`'s `SCI_CONVERTEOLS`,
    // `NPPM_MAKECURRENTBUFFERDIRTY`, `NPPM_SAVECURRENTFILE` — and the
    // `SCN_SAVEPOINT*` / `SCN_MODIFIED` Scintilla emits for it arrives
    // synchronously, inside the borrow above, where the notification
    // handler is declined. So the tab strip's dirty marker would sit
    // stale until an unrelated event repainted it: the same gap the
    // Find/Replace commands close with their own post-borrow refresh
    // (DESIGN.md §7.4). Two readings decide whether to refresh: the
    // bound document's live modify bit, for the active tab, and every
    // tab's cached `Tab.dirty`, which is what the strip paints for the
    // others and what the shell re-reads from the live state when it
    // converts a background document — that one is swapped in and out
    // inside the dispatch, so the bound bit never sees it. Refresh only
    // on a change, so the ~dozen `NPPM_*` queries a plugin command
    // typically makes cost two direct calls and a short `Vec` each
    // rather than a strip rebuild.
    if dirty_edge {
        crate::refresh_tab_chrome();
    }
    // `Some` means the dispatch ran on the main thread with the borrow
    // now dropped, so a prompt it queued — the export Save-As, or
    // `NPPM_RELOADBUFFERID`'s reload confirmation — can be presented
    // before the plugin's `SendMessage` returns, which is when
    // Notepad++ shows the same prompts.
    crate::present_deferred_dialogs();
    routed
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
    // Record the UI thread before anything can route a message here: a
    // plugin only ever reaches `plugin_dispatch` through a handle handed
    // out by `npp_data`, and the first of those is built below this line.
    // Set unconditionally rather than alongside `VALID_SCI` so the
    // affinity check is armed even on a startup where the state read
    // fails — an unset cell would make `on_main_thread` answer `false`
    // here and marshal a call that is already on the main thread, which
    // deadlocks against the very main loop it is waiting for.
    let _ = MAIN_THREAD.set(std::thread::current().id());
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

/// Lazy-load every pending plugin, **holding no `with_state` borrow
/// while plugin code runs**.
///
/// This used to be one `ensure_plugins_loaded` call inside
/// `with_state`, so a plugin's `setInfo` querying the host was
/// declined re-entrantly and read 0 — which real plugins take as a
/// definitive answer. `NppExec` asks for the host version there and
/// refuses to start without one.
///
/// Take what the load needs under a borrow, run the plugin's own
/// entry points with none held, commit under a fresh borrow, and once
/// every pending plugin is loaded deliver the load-time notifications
/// — Notepad++'s order, see `LoadNotifications` — with none held again.
/// A nested pass (a plugin re-entering the loader from `setInfo`) is
/// bounded inside `PluginHost`, which answers "nothing pending" while
/// a load is outstanding.
fn load_pending_plugins() {
    // Holding the borrow across the whole load used to make this
    // unnecessary: a wake landing mid-load found the state borrowed
    // and deferred itself. Dropping the borrow between steps gives
    // that up, so the guard has to be explicit — otherwise a worker
    // result could be applied *between* two plugins' loads, moving
    // the very tabs a `setInfo` is asking about.
    let _freeze = crate::DrainFreeze::new();
    let data = npp_data();
    let dispatch: Option<HostDispatchFn> = Some(plugin_dispatch);
    // Every plugin this pass loads is notified together, after the
    // loop, in Notepad++'s order — see `LoadNotifications`.
    let mut notices = codepp_plugin_host::LoadNotifications::default();
    while let Some(pending) = with_state(|st| st.shell.next_plugin_to_load()).flatten() {
        // No borrow held: `setInfo` runs here and its `NPPM_*` are
        // answered for real. The `catch_unwind` is not about the
        // plugin — `execute_load` already guards each of its entry
        // points — but about our own bookkeeping around it: a panic
        // escaping here would skip the commit below and leave
        // `PluginHost`'s latch set, which silently disables loading
        // for the rest of the session.
        let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            codepp_plugin_host::execute_load(&pending, data, dispatch)
        }))
        .unwrap_or_else(|_| Err("plugin panicked during load".to_string()));
        let Some(ready) = with_state(|st| st.shell.commit_plugin_load(&pending, loaded)) else {
            // The state went away between the two phases — the
            // window torn down under a `setInfo`. Nothing commits,
            // so the latch stays set and no further plugin loads;
            // say so, because the symptom is otherwise silent.
            tracing::error!("lost the UI state mid plugin load; plugin loading is now disabled");
            break;
        };
        if let Some(ready) = ready {
            notices.push(ready);
        }
    }
    with_state(|st| st.shell.after_plugin_loads());
    // No borrow held: a plugin that queries the host from
    // `NPPN_READY` is doing something ordinary. This backend's plugin
    // menu is rebuilt by the caller afterwards, which is not the
    // Win32 order — but a plugin cannot reach this menu at all here
    // (`NPPM_GETMENUHANDLE` answers NULL and `NPPM_SETMENUITEMCHECK`
    // is not implemented), so when it is built is not observable.
    // The active buffer is read per plugin, at delivery — see
    // `LoadNotifications::deliver` — under a borrow that ends before
    // that plugin runs.
    //
    // No panel restore: `NPPM_DMMREGASDCKDLG` carries an `HWND`, so
    // this backend hosts no plugin panel, and the dock restore's
    // `drop_plugin_panels` removes any a Windows-written session
    // names before the layout is applied. Nothing to bring back.
    notices.deliver(
        data.npp_handle,
        || with_state(|st| st.shell.active_buffer_id()).flatten(),
        || {},
    );
}

/// Lazy-load every pending plugin and rebuild the Plugins menu from the
/// loaded set. Called from the Plugins menu's `show` handler.
pub(crate) fn ensure_loaded_and_rebuild(menu: &gtk::Menu) {
    // Load pending plugins, installing the GTK routing callback into each
    // (the SDK handshake) so their `SendMessage` reaches us.
    load_pending_plugins();
    rebuild_menu(menu);
    // The load may have absorbed new plugin-shortcut defaults; rebuild
    // the accel group so their chords become live this session (Win32
    // does the equivalent `refresh_plugin_accels` after its populate).
    rebuild_plugin_accel_group();
    // NPPN_READY fired inside `load_pending_plugins`, outside any
    // borrow; anything a plugin queued back is drained on the next
    // wake.
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
                    item.connect_activate(move |_| {
                        crate::at_callback_boundary("plugin:item:activate", (), || {
                            on_plugin_command(cmd_id);
                        });
                    });
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
    manager.connect_activate(|_| {
        crate::at_callback_boundary("plugin:manager:activate", (), show_plugin_manager);
    });
    menu.append(&manager);
    let folder = gtk::MenuItem::with_mnemonic("_Open Plugin Folder");
    folder.connect_activate(|_| {
        crate::at_callback_boundary("plugin:folder:activate", (), open_plugin_folder);
    });
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
        crate::at_callback_boundary("plugin:toggle:toggled", (), || {
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
/// at a [`crate::at_callback_boundary`] (a panic must not cross the C
/// frame).
fn on_plugin_command(cmd_id: i32) {
    let cmd = with_state(|st| st.shell.lookup_plugin_command(cmd_id)).flatten();
    let Some(cmd) = cmd else {
        return;
    };
    // SAFETY: `cmd` is a plugin `FuncItem.p_func`, invoked on the UI
    // thread with no arguments, per the N++ ABI. The boundary keeps a
    // Rust-plugin panic from unwinding across `extern "C"`.
    crate::at_callback_boundary("plugin:command", (), || unsafe { cmd() });
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
            move |_, _, _, _| {
                crate::at_callback_boundary("plugin:accel:accel_group", false, || {
                    fire_plugin_chord(ctrl, alt, shift, key)
                })
            },
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
    load_pending_plugins();
    // The load may have absorbed new defaults — for *other* commands
    // than the one just pressed (`load_pending_plugins` loads every
    // pending plugin). Rebuild the accel group so those become live
    // this session, matching Win32's `refresh_plugin_accels` after
    // `handle_plugin_shortcut_shim`'s load. Deferred to a glib idle
    // rather than done inline: this runs *inside* the accel group's
    // own closure, and tearing the group down under itself is the
    // kind of reentrancy this backend has been bitten by before.
    glib::idle_add_local_once(|| {
        crate::at_callback_boundary("plugin:accel_rebuild:idle", (), rebuild_plugin_accel_group);
    });
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
/// Called after each drain.
///
/// The queue and the plugins' `beNotified` entry points are taken in
/// one `with_state` borrow and every plugin call happens **after** it
/// has returned. That is what lets a plugin's `beNotified` call back
/// into `NPPM_*`: `plugin_dispatch` routes that through `with_state`,
/// which declines a nested borrow — so a delivery made from *inside*
/// the borrow (as this once did, calling `notify_plugins` through
/// `&Shell`) answered every such callback with 0, silently. The Win32
/// and Cocoa backends deliver from the same snapshot for the same
/// reason. Any dialog a handler queued is presented afterwards.
pub(crate) fn deliver_notifications() {
    let Some((targets, notes)) = with_state(|st| {
        let notes = st.shell.take_notifications();
        (st.shell.notify_targets(), notes)
    }) else {
        return;
    };
    if notes.is_empty() || targets.is_empty() {
        return;
    }
    for note in &notes {
        targets.deliver(note, npp_sentinel());
    }
    crate::present_deferred_dialogs();
}

/// Deliver one [`codepp_shell::SyncNotification`] — the `NPPN_*BEFORE*`
/// family the shell hands out for delivery *ahead* of the operation it
/// announces — with no `with_state` borrow held, then present anything
/// the handlers queued.
pub(crate) fn deliver_sync(announced: &codepp_shell::SyncNotification) {
    announced.deliver(npp_sentinel());
    crate::present_deferred_dialogs();
}

/// The cross-thread `SCI_*` marshal (DESIGN.md §7.4).
///
/// Display-gated for the same reason every other GTK test here is:
/// `scintilla_new` builds a real `GtkWidget`. Driven by
/// `crate::display_tests`, which owns the invocation and explains why
/// these cannot be `#[test]`s of their own.
#[cfg(test)]
pub(crate) mod cross_thread_tests {
    use super::{on_main_thread, plugin_dispatch, MAIN_THREAD, VALID_SCI};
    use codepp_scintilla_sys::{
        scintilla_new, scintilla_send_message, SCI_GETLENGTH, SCI_INSERTTEXT,
    };
    use std::ffi::{c_void, CString};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// A widget pointer carried into the spawned "plugin worker" thread,
    /// standing in for the handle a real plugin caches from `NppData`.
    struct WorkerPtr(*mut c_void);
    // SAFETY: the test's own leaked, permanently-live Scintilla widget.
    // The worker only hands it to `plugin_dispatch`, which is the code
    // under test and is precisely what must not dereference it off the
    // main thread.
    unsafe impl Send for WorkerPtr {}

    pub(crate) fn a_plugins_worker_thread_reaches_scintilla_through_the_main_loop() {
        gtk::init().expect("gtk::init failed — no display?");
        // SAFETY: GTK is initialised, `scintilla_new`'s only precondition.
        let sci = unsafe { scintilla_new() };
        assert!(!sci.is_null(), "scintilla_new returned null");

        // Stand in for `discover`, which arms both of these at startup.
        VALID_SCI.store(sci, Ordering::Release);
        let _ = MAIN_THREAD.set(std::thread::current().id());
        assert!(on_main_thread(), "the test body is the UI thread");

        // Seed five bytes so a correct round trip has a distinctive
        // answer — `0` is what every failure mode returns.
        let text = CString::new("hello").expect("no interior NUL");
        // SAFETY: UI thread, live widget, valid NUL-terminated text.
        unsafe { scintilla_send_message(sci, SCI_INSERTTEXT, 0, text.as_ptr() as isize) };

        // Control: on the UI thread the fast path answers with no
        // main-loop iteration at all. It also proves the affinity check
        // is armed, so the cross-thread assertion below cannot pass
        // vacuously by having classified *everything* as remote.
        //
        // An earlier version of this comment claimed the fast path was
        // required for correctness — that a marshal from the UI thread
        // would queue and then block on the loop that would have run it.
        // **Measured, and false:** calling `send_sci_on_main` directly
        // from here returns Scintilla's real answer immediately.
        // `g_main_context_invoke_full` acquires the context if it can
        // and dispatches inline when it succeeds, which on an idle main
        // thread it does. The branch therefore earns its place by
        // avoiding a channel allocation on the common path and by saying
        // plainly which thread a call is on — not by averting a hang.
        assert_eq!(
            plugin_dispatch(sci, SCI_GETLENGTH, 0, 0),
            5,
            "same-thread SCI_* must answer directly"
        );

        // The real case. A plugin calling from its own thread must be
        // queued rather than dereferencing the widget where it stands...
        let handle = WorkerPtr(sci);
        let worker = std::thread::spawn(move || {
            let handle = handle;
            plugin_dispatch(handle.0, SCI_GETLENGTH, 0, 0)
        });
        // The sleep is a heuristic bound, not a correctness requirement,
        // and it is one-sided: a regressed direct call finishes in
        // microseconds, so a starved runner can only make this wait
        // longer than necessary — never turn a real failure into a pass.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !worker.is_finished(),
            "a cross-thread SCI_* answered without the main loop running — \
             it was executed on the calling thread, which is the bug"
        );

        // ...and answered once the main loop gets to it.
        let mut spins = 0;
        while !worker.is_finished() {
            gtk::main_iteration_do(false);
            spins += 1;
            assert!(spins < 10_000, "marshaled SCI_* never completed");
        }
        assert_eq!(
            worker.join().expect("worker panicked"),
            5,
            "the marshaled call must return Scintilla's real answer"
        );

        // An unrecognised handle is still refused rather than marshaled,
        // from a worker just as from the UI thread — the identity check
        // runs before the affinity check.
        let bogus = WorkerPtr(std::ptr::dangling_mut::<u8>().cast::<c_void>());
        let refused = std::thread::spawn(move || {
            let bogus = bogus;
            plugin_dispatch(bogus.0, SCI_GETLENGTH, 0, 0)
        });
        assert_eq!(
            refused.join().expect("worker panicked"),
            0,
            "an unknown handle must be refused without dereferencing it"
        );
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
