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

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;

use gtk::gdk_pixbuf::Pixbuf;
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
    // A `NPPM_DMM*` handler may have registered, shown, hidden or
    // re-activated a plugin panel. The *model* can change inside the
    // dispatch; the widget tree cannot, because the reconcile syncs the
    // session through `with_state` and would be declined under the live
    // borrow. So the handler marks and the reconcile happens here, with
    // the borrow ended — the same shape as `needs_rebind` above, and as
    // Win32's `dock_dirty`. It also sends the `DMN_DOCK` / `DMN_FLOAT` a
    // registration owes the plugin, before its `SendMessage` returns.
    if crate::dock::take_dirty() {
        crate::dock::apply_layout();
    }
    // `Some` means the dispatch ran on the main thread with the borrow
    // now dropped, so a prompt it queued — the export Save-As, or
    // `NPPM_RELOADBUFFERID`'s reload confirmation — can be presented
    // before the plugin's `SendMessage` returns, which is when
    // Notepad++ shows the same prompts.
    crate::present_deferred_dialogs();
    routed
}

// --- plugin dock panels ------------------------------------------------------------
//
// What `NPPM_DMMREGASDCKDLG` means on this backend. On Windows a plugin
// hands the host its dialog's `HWND`; a recompiled Linux plugin hands it
// the GTK analogue — a `GtkWidget*` it built, unparented — and the host
// adopts that widget as a dock panel's content, exactly as the Win32
// host adopts a window. The panel is then an ordinary dock panel
// (`crate::dock`). The one thing a widget cannot do that a window can is
// receive a message, so the `DMN_*` notifications a Win32 plugin gets as
// `WM_NOTIFY` at its dialog's window procedure arrive here at the
// plugin's own `messageProc` instead, `wParam` naming the panel — see
// `codepp_plugin_host::WM_NOTIFY`.

thread_local! {
    /// `DMN_DOCK` / `DMN_FLOAT` notices waiting to be sent. See
    /// [`deliver_dock_notices`].
    static DOCK_NOTICES: std::cell::RefCell<std::collections::VecDeque<crate::dock::DockNotice>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Set while [`deliver_dock_notices`] is draining.
    static DOCK_NOTICES_DELIVERING: Cell<bool> = const { Cell::new(false) };
    /// Set while a `DMN_CLOSE` is being delivered. See
    /// [`close_plugin_panel`].
    static DMN_CLOSE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// `NPPM_DMMREGASDCKDLG` on this backend: adopt the plugin's widget as a
/// dock panel's content, returning the panel it interns to — for the
/// shell to record, and sign, the command that reopens it.
///
/// `hClient` must be a `GtkWidget*` the plugin made and has not put in a
/// container or made a window of; the host takes its own reference
/// (sinking a floating one, as a container's `add` would) and never
/// destroys it. The checks below refuse what can be told apart without
/// trusting the pointer — the npp handle, the host's own Scintilla view —
/// then what a type check can tell: not a widget, a toplevel, a widget
/// that already has a parent (which covers every host widget, and a
/// widget registered twice). They are bug containment, not a boundary
/// (DESIGN.md §6.5): an arbitrary or dangling pointer cannot be told from
/// a live object — there is no `IsWindow` for memory — and faults in the
/// type check instead of being declined. A plugin runs in this process
/// and needs no help from the host to reparent a widget anyway.
///
/// Registration does not show the panel — `NPPM_DMMSHOW` does, the ABI's
/// own split. The widget goes into a scrolled container of the host's,
/// and that container joins the tree at the reconcile the NPPM dispatch
/// runs once its state borrow has ended.
pub(crate) fn register_dock_dialog(
    params: codepp_plugin_host::DockDialogParams,
) -> Option<codepp_core::dock::DockPanel> {
    use gtk::glib::translate::{from_glib_borrow, from_glib_none, Borrowed};

    let handle = params.h_client;
    if let Err(why) = refuse_host_handle(handle) {
        tracing::warn!(why, "NPPM_DMMREGASDCKDLG: refused");
        return None;
    }
    // SAFETY: `handle` is non-null and not one of the host's own
    // non-widget handles; by the ABI's contract on this backend it
    // points at a live `GtkWidget` — see `is_instance_of` for what
    // happens when it does not.
    if !unsafe { is_instance_of(handle, gtk::Widget::static_type()) } {
        tracing::warn!("NPPM_DMMREGASDCKDLG: refused: hClient is not a GtkWidget");
        return None;
    }
    // SAFETY: a live `GtkWidget`, per the check above. Borrowed, not
    // referenced: nothing about the plugin's widget changes unless every
    // check passes.
    let widget: Borrowed<gtk::Widget> =
        unsafe { from_glib_borrow(handle.cast::<gtk::ffi::GtkWidget>()) };
    if widget.is_toplevel() {
        tracing::warn!("NPPM_DMMREGASDCKDLG: refused: hClient is a toplevel window");
        return None;
    }
    if widget.parent().is_some() {
        tracing::warn!(
            "NPPM_DMMREGASDCKDLG: refused: hClient is already inside a container — \
             register a free-standing widget"
        );
        return None;
    }
    let Some(panel) = codepp_shell::intern_plugin_dock_panel(&params.name, &params.module_name)
    else {
        // Both halves are the plugin's own text, so they are logged as
        // the chrome would show them.
        tracing::warn!(
            module = codepp_shell::sanitize_str_for_display(&params.module_name),
            panel = codepp_shell::plugin_dock_title(&params.name, &params.module_name),
            "NPPM_DMMREGASDCKDLG: unusable panel identity, or the panel table is full"
        );
        return None;
    };
    let gone = Rc::new(Cell::new(false));
    let spec = crate::dock::PluginPanelSpec {
        panel,
        handle,
        tb_data: params.tb_data,
        icon: tab_icon(&params),
        initial_side: codepp_plugin_host::docking::dock_side_from_u_mask(params.u_mask),
        name: params.name,
        module_name: params.module_name,
        caller: params.caller,
        gone: Rc::clone(&gone),
    };
    // SAFETY: the same live widget. `from_glib_none` takes the host's own
    // reference — sinking a floating one, as a container's `add` would —
    // and the dock calls this only once the registration is certain to
    // stand, so a refusal never drops a reference the plugin was relying
    // on.
    let adopted = crate::dock::register_plugin_panel(spec, || unsafe {
        from_glib_none(handle.cast::<gtk::ffi::GtkWidget>())
    });
    match adopted {
        Ok(widget) => {
            watch_for_disposal(&widget, gone);
            Some(panel)
        }
        Err(why) => {
            tracing::warn!(
                why,
                panel = panel.persist_key(),
                "NPPM_DMMREGASDCKDLG: refused"
            );
            None
        }
    }
}

/// Refuse the handles that are the host's own and can be recognised
/// without dereferencing them: the npp handle — a sentinel address, not
/// an object at all — and the main Scintilla view. The Document Map's
/// view is a widget with a parent and is refused by that check instead.
fn refuse_host_handle(handle: *mut c_void) -> Result<(), &'static str> {
    if handle.is_null() {
        return Err("hClient is null");
    }
    if std::ptr::eq(handle, npp_sentinel()) {
        return Err("hClient is the npp handle, not a widget");
    }
    if is_valid_scintilla(handle) {
        return Err("hClient is the host's own Scintilla view");
    }
    Ok(())
}

/// Whether `ptr` is an instance of `gtype` or of a type derived from it.
///
/// # Safety
///
/// `ptr` must be null or point at a live `GTypeInstance`: the check
/// reads the instance's class pointer. That is exactly the check a
/// plugin's mistake cannot pass through safely — a dangling or garbage
/// pointer faults here — and there is no way to test an arbitrary
/// address for being a live object first. Win32 has `IsWindow` for its
/// handles; memory has no equivalent.
unsafe fn is_instance_of(ptr: *mut c_void, gtype: glib::Type) -> bool {
    use gtk::glib::translate::IntoGlib;
    // SAFETY: forwarded from the caller; a null pointer is answered
    // without being read.
    !ptr.is_null()
        && unsafe { glib::gobject_ffi::g_type_check_instance_is_a(ptr.cast(), gtype.into_glib()) }
            != glib::ffi::GFALSE
}

/// The plugin's own tab icon: `tTbData.hIconTab`, which on this backend
/// is a `GdkPixbuf*`, honoured when `uMask` carries `DWS_ICONTAB`. The
/// host takes its own reference. `None` — the generic plugin glyph — for
/// no icon, or for something that is not a pixbuf.
fn tab_icon(params: &codepp_plugin_host::DockDialogParams) -> Option<Pixbuf> {
    use gtk::glib::translate::from_glib_none;

    let icon = params.h_icon_tab;
    if params.u_mask & codepp_plugin_host::DWS_ICONTAB == 0 || icon.is_null() {
        return None;
    }
    // SAFETY: by the ABI's contract on this backend, `hIconTab` with
    // `DWS_ICONTAB` points at a live `GdkPixbuf`; see `is_instance_of`.
    if !unsafe { is_instance_of(icon, Pixbuf::static_type()) } {
        tracing::warn!(
            "NPPM_DMMREGASDCKDLG: hIconTab is not a GdkPixbuf; the tab shows the generic glyph"
        );
        return None;
    }
    // SAFETY: a live `GdkPixbuf`, per the check above; `from_glib_none`
    // takes the host's own reference.
    Some(unsafe { from_glib_none(icon.cast::<gtk::gdk_pixbuf::ffi::GdkPixbuf>()) })
}

/// Drop the registration once the plugin destroys its own widget: mark
/// it gone at once — so nothing puts the disposed widget back in a
/// container — and let an idle close the panel, since `destroy` can fire
/// inside a reconcile that holds the dock borrow.
fn watch_for_disposal(widget: &gtk::Widget, gone: Rc<Cell<bool>>) {
    widget.connect_destroy(move |_| {
        crate::at_callback_boundary("plugin:panel:destroy", (), || {
            gone.set(true);
            glib::idle_add_local_once(|| {
                crate::at_callback_boundary(
                    "plugin:panel:forget",
                    (),
                    crate::dock::forget_destroyed_plugin_panels,
                );
            });
        });
    });
}

/// `NPPM_DMMUPDATEDISPINFO`: re-read the panel's `tTbData` and take its
/// current `pszName` / `pszModuleName` as the panel's lookup keys.
/// `false` for a handle nothing is registered under.
pub(crate) fn update_dock_disp_info(handle: *mut c_void) -> bool {
    let Some(tb_data) = crate::dock::plugin_panel_tb_data(handle) else {
        return false;
    };
    // SAFETY: the `tTbData` the panel was registered with, which its
    // plugin must keep alive for the registration's lifetime — the
    // contract on `DockDialogParams::tb_data`, which the host has no way
    // to verify.
    if let Some(disp) = unsafe { codepp_plugin_host::read_dock_disp_info(tb_data) } {
        crate::dock::rename_plugin_panel(handle, disp.name, disp.module_name);
    }
    true
}

/// Close a plugin's panel from its group's ✕ (or its floating window's
/// close), telling the plugin first.
///
/// The `DMN_CLOSE` is how a plugin keeps a "Show Console" style menu
/// check in step with what the user can see; without it the plugin
/// believes its panel is still open. Sent before the panel hides, as
/// upstream does, with no borrow held, so the plugin may call `NPPM_*`
/// back from its handler.
///
/// A latch bounds the round trip: a plugin's handler may close the panel
/// again — directly or through something that reaches this path — and
/// without the latch that recurses until the stack runs out. A close
/// nested inside any other close skips its notification and just hides,
/// the coarse direction Win32's `DmnCloseGuard` takes for the same reason.
pub(crate) fn close_plugin_panel(panel: codepp_core::dock::DockPanel) {
    if let Some((handle, caller)) = crate::dock::plugin_panel_notify_target(panel) {
        if !DMN_CLOSE_ACTIVE.with(Cell::get) {
            let _closing = crate::FlagGuard::set(&DMN_CLOSE_ACTIVE);
            send_dock_notification(panel, handle, caller, codepp_plugin_host::DMN_CLOSE);
        }
    }
    crate::dock::set_panel_visible(panel, false);
}

/// Send each `DMN_DOCK` / `DMN_FLOAT` notice.
///
/// **Notices raised while one is being delivered are queued, not sent.**
/// The plugin's handler runs with no borrow held, so it may send
/// `NPPM_*` back — including `NPPM_DMMREGASDCKDLG` for a panel nothing
/// has been told about, which reconciles again from inside this loop and
/// raises a notice of its own. Sent there and then, that notice's
/// handler could do the same, nesting a full round trip per link until
/// the registration cap or the stack ran out. So a call made while a
/// delivery is running only appends to the queue and returns, and the
/// outermost call drains it in order: nothing is dropped, and the
/// nesting stays one level deep whatever the plugin does. The same queue
/// Win32's `deliver_container_notices` keeps.
pub(crate) fn deliver_dock_notices(notices: Vec<crate::dock::DockNotice>) {
    DOCK_NOTICES.with(|q| q.borrow_mut().extend(notices));
    if DOCK_NOTICES_DELIVERING.with(Cell::get) {
        return;
    }
    let _delivering = crate::FlagGuard::set(&DOCK_NOTICES_DELIVERING);
    while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) {
        // A handler for an earlier notice may have destroyed this
        // one's widget; the record is already written, so skipping it
        // loses nothing that could still be delivered.
        if !crate::dock::plugin_panel_is_live(notice.panel, notice.handle) {
            continue;
        }
        tracing::debug!(
            panel = notice.panel.persist_key(),
            dmn = if notice.code & 0xFFFF == codepp_plugin_host::DMN_DOCK {
                "DMN_DOCK"
            } else {
                "DMN_FLOAT"
            },
            container = notice.code >> 16,
            "dock container notification"
        );
        send_dock_notification(notice.panel, notice.handle, notice.caller, notice.code);
    }
}

/// Deliver one `DMN_*` about `panel` to the plugin that should hear it —
/// see `Shell::plugin_panel_message_target` — through its `messageProc`:
/// `WM_NOTIFY`, `wParam` the panel's `hClient`, `lParam` an `NMHDR` from
/// the npp handle with `idFrom` 0 and `code` as given. Called with no
/// borrow held.
fn send_dock_notification(
    panel: codepp_core::dock::DockPanel,
    handle: *mut c_void,
    caller: Option<usize>,
    code: u32,
) {
    let Some(target) =
        with_state(|st| st.shell.plugin_panel_message_target(caller, panel)).flatten()
    else {
        tracing::debug!(
            panel = panel.persist_key(),
            code,
            "no loaded plugin to tell about its dock panel"
        );
        return;
    };
    let nmhdr = codepp_plugin_host::SciNotifyHeader {
        hwnd_from: npp_sentinel(),
        id_from: 0,
        code,
    };
    // SAFETY: a loaded plugin's `messageProc`, run as that plugin on the
    // UI thread with no state borrow held. `nmhdr` outlives the call, and
    // `wParam` is the handle the plugin itself registered the panel
    // under.
    let _ = unsafe {
        target.send(
            codepp_plugin_host::WM_NOTIFY,
            handle as usize,
            &raw const nmhdr as isize,
        )
    };
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

/// Which plugins a [`load_plugins_where`] pass may load.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadScope {
    /// Every discovered plugin — DESIGN.md §6.4's lazy triggers: the
    /// Plugins menu, a plugin hotkey.
    All,
    /// Only the plugins owning a dock panel the restored session had
    /// open — the startup pass. Without it a restored plugin panel waits
    /// for a widget that only its plugin can supply, and nothing loads
    /// the plugin until the user opens a menu they have no reason to
    /// connect with the panel they are missing.
    RestoredPanels,
}

/// Lazy-load every pending plugin. See [`load_plugins_where`].
fn load_pending_plugins() {
    load_plugins_where(LoadScope::All);
}

/// Load the plugins whose dock panels the restored session had open,
/// and bring those panels back — the startup counterpart of the lazy
/// triggers, and the one exception to DESIGN.md §8's "no plugin loads at
/// startup" (a restored panel is a recorded interaction with its
/// plugin). With Preferences → Security's guard on, only panels whose
/// record Code++ signed count (`Shell::modules_with_restored_panels`).
///
/// Called once from `run()`, after the dock layout is restored and the
/// plugins are discovered, and before the main loop starts, so the first
/// frame already carries the panels. Parks whatever no loaded plugin can
/// supply even when nothing loads, so no group is left on screen empty.
pub(crate) fn restore_panel_plugins() {
    load_plugins_where(LoadScope::RestoredPanels);
    // The load may have absorbed shortcut defaults; make their chords
    // live, as `ensure_loaded_and_rebuild` does after a lazy load.
    rebuild_plugin_accel_group();
}

/// Load the plugins `scope` admits, **holding no `with_state` borrow
/// while plugin code runs**, and bring back the dock panels they had
/// open.
///
/// This used to be one `ensure_plugins_loaded` call inside
/// `with_state`, so a plugin's `setInfo` querying the host was declined
/// re-entrantly and read 0 — which real plugins take as a definitive
/// answer. `NppExec` asks for the host version there and refuses to
/// start without one. Now each step takes what it needs under a borrow,
/// runs the plugin's entry points with none held, and commits under a
/// fresh one. A nested pass (a plugin re-entering the loader from
/// `setInfo`) is bounded inside `PluginHost`, which answers "nothing
/// pending" while a load is outstanding.
///
/// Then, the same steps in the same order as Win32's `load_plugins_where`
/// — a function for each, so the two read side by side:
///
///   1. the plugins' commands become known, so a tick set from a load-time
///      notification is recorded ([`absorb_loaded_commands`]);
///   2. parked panels these plugins can now supply go back where they
///      were ([`unpark_loaded_plugins_panels`]), and what to restore is
///      noted before any plugin runs ([`PanelRestore`]);
///   3. the load-time notifications — Notepad++'s order, see
///      `LoadNotifications` — with each open panel's own command run
///      between `NPPN_TBMODIFICATION` and `NPPN_BUFFERACTIVATED`
///      ([`restore_plugin_panels`]);
///   4. once `NPPN_READY` is over: a restored panel whose plugin loaded
///      and still supplied nothing is closed, one whose command the guard
///      withheld is parked, and one no loaded plugin can supply is parked.
fn load_plugins_where(scope: LoadScope) {
    // Holding the borrow across the whole load used to make this
    // unnecessary: a wake landing mid-load found the state borrowed and
    // deferred itself. Dropping the borrow between steps gives that up,
    // so the guard has to be explicit — otherwise a worker result could
    // be applied *between* two plugins' loads, moving the very tabs a
    // `setInfo` is asking about.
    let _freeze = crate::DrainFreeze::new();
    let data = npp_data();
    let dispatch: Option<HostDispatchFn> = Some(plugin_dispatch);
    // Every plugin this pass loads is notified together, after the
    // loop, in Notepad++'s order — see `LoadNotifications`.
    let mut notices = codepp_plugin_host::LoadNotifications::default();
    let mut loaded_now: Vec<usize> = Vec::new();
    loop {
        let pending = with_state(|st| match scope {
            LoadScope::All => st.shell.next_plugin_to_load(),
            LoadScope::RestoredPanels => st.shell.next_restored_panel_plugin_to_load(),
        })
        .flatten();
        let Some(pending) = pending else {
            break;
        };
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
            // The state went away between the two phases — the window
            // torn down under a `setInfo`. Nothing commits, so the latch
            // stays set and no further plugin loads; say so, because the
            // symptom is otherwise silent.
            tracing::error!("lost the UI state mid plugin load; plugin loading is now disabled");
            break;
        };
        if let Some(ready) = ready {
            notices.push(ready);
            loaded_now.push(pending.idx);
        }
    }
    with_state(|st| st.shell.after_plugin_loads());
    // The commands these plugins publish are known before any of them is
    // told anything, so a tick set from `NPPN_TBMODIFICATION` or
    // `NPPN_READY` is recorded — Notepad++ has the items installed by
    // then. The menu itself is rebuilt by the caller afterwards, which is
    // not the Win32 order, and not observable either: a mark is kept by
    // command id (`PluginChecks`) and painted on every rebuild, and
    // `NPPM_GETMENUHANDLE` answers NULL here, so no plugin can reach the
    // menu before it exists.
    absorb_loaded_commands();
    // A panel parked because its plugin could not supply it is put back
    // first, so a plugin that has become loadable since — re-enabled in
    // the Plugin Manager — gets its panel restored the way it would have
    // been at startup. Then what to restore is noted, before any of these
    // plugins runs and can change it.
    let unparked = unpark_loaded_plugins_panels(&loaded_now);
    let restore = PanelRestore::capture(&loaded_now);
    if unparked {
        // The groups those panels are back in need building before the
        // plugins that fill them run.
        crate::dock::apply_layout();
    }
    // No borrow held: a plugin that queries the host from `NPPN_READY` is
    // doing something ordinary. The active buffer is read per plugin, at
    // delivery — see `LoadNotifications::deliver` — under a borrow that
    // ends before that plugin runs. Delivered even if a step above lost
    // the state: these plugins are loaded, and a plugin that never hears
    // READY never finishes its own initialisation.
    let mut withheld: Vec<codepp_core::dock::DockPanel> = Vec::new();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        notices.deliver(
            data.npp_handle,
            || with_state(|st| st.shell.active_buffer_id()).flatten(),
            || restore_plugin_panels(&restore, &mut withheld),
        );
    }));
    // Only now, after READY: a plugin may register its panel from there,
    // and one that does must not find it already closed — or parked.
    close_unregistered_restored_panels(&restore, &withheld);
    // A panel whose command was withheld is not broken, so it keeps its
    // place rather than being closed.
    park_withheld_panels(&withheld);
    // And a panel whose plugin could not be loaded at all is parked
    // rather than left on screen with nothing in it.
    park_unsupplied_plugin_panels();
}

/// What a load pass needs to bring back the dock panels its plugins had
/// open. Captured before any of those plugins is notified — see
/// [`load_plugins_where`].
#[derive(Default)]
struct PanelRestore {
    /// The open plugin panels owned by the plugins this pass loaded, in
    /// group then tab order.
    panels: Vec<codepp_core::dock::DockPanel>,
    /// Panels owned by the plugins this pass loaded that stay parked
    /// because Preferences → Security's guard does not trust their record
    /// — see [`unpark_loaded_plugins_panels`]. Restored after all if
    /// their plugin registers them from `NPPN_TBMODIFICATION`, which puts
    /// them back and records a command the plugin itself declared.
    held: Vec<codepp_core::dock::DockPanel>,
    /// Every group's front tab as the pass began.
    fronts: Vec<(u32, codepp_core::dock::DockPanel)>,
}

impl PanelRestore {
    fn capture(loaded: &[usize]) -> Self {
        if loaded.is_empty() {
            return Self::default();
        }
        let Some(layout) = crate::dock::layout_snapshot() else {
            return Self::default();
        };
        let open = layout.open_plugin_panels();
        let parked = layout.parked_panels();
        with_state(|st| Self {
            panels: st.shell.panels_owned_by(&open, loaded),
            held: st.shell.panels_owned_by(&parked, loaded),
            fronts: layout.fronts(),
        })
        .unwrap_or_default()
    }
}

/// Put back where they were the parked panels whose plugins a load pass
/// has just loaded; returns whether any came back.
///
/// A panel is parked when no loaded plugin can supply it — see
/// [`park_unsupplied_plugin_panels`] — and a plugin disabled at startup
/// can be re-enabled in the Plugin Manager and loaded later in the same
/// session. Putting its panels back before the pass notes what to
/// restore is what lets the ordinary restore run their commands, exactly
/// as it would have at startup.
///
/// With Preferences → Security's guard on, only a panel whose record
/// Code++ signed comes back here. An unsigned one's command will not
/// run, so putting it back would only show an empty group until the pass
/// parks it again; it stays parked and is noted as held
/// ([`PanelRestore::held`]), and comes back the moment its plugin
/// registers it.
fn unpark_loaded_plugins_panels(loaded: &[usize]) -> bool {
    if loaded.is_empty() {
        return false;
    }
    let Some(layout) = crate::dock::layout_snapshot() else {
        return false;
    };
    let back: Vec<codepp_core::dock::DockPanel> = with_state(|st| {
        st.shell
            .panels_owned_by(&layout.parked_panels(), loaded)
            .into_iter()
            .filter(|&panel| st.shell.panel_record_is_trusted(&layout, panel))
            .collect()
    })
    .unwrap_or_default();
    !back.is_empty() && crate::dock::update_layout(|l| l.unpark(&back))
}

/// Bring back the dock panels a load pass's plugins had open, the way
/// Notepad++ does: by running each one's own menu command.
///
/// A panel's `tTbData.dlgID` is the index of the plugin's `FuncItem` that
/// shows it, recorded at registration and persisted with the panel.
/// Running that command is the only restore that works for every plugin:
/// many register their panel only from it — `NppExec`'s console among
/// them — and a plugin keeps its own "is my panel open" state and its menu
/// check there, so even a panel whose widget already exists comes back
/// half-restored if the plugin never hears it. Measured against
/// Notepad++ 8.9.6: it runs the command for every panel it recorded open,
/// between `NPPN_TBMODIFICATION` and `NPPN_READY` — which is where
/// `LoadNotifications::deliver` calls this.
///
/// Each command runs exactly as a click on its menu item does
/// ([`on_plugin_item_activated`]). The list is resolved under a borrow
/// that ends before the first one runs: it is the plugin's own code, and
/// it talks back to the host. Each show brings its panel to the front of
/// its group, so the tabs the user had in front are put back in front
/// afterwards, as Notepad++ also does.
///
/// With Preferences → Security's guard on, a command runs only if Code++
/// signed it — which it does only for a command the panel's own plugin
/// registered. Each panel whose command is held back goes into
/// `withheld`, for the caller to park once READY is over. The command is
/// read now rather than when the pass began, so one a plugin has just
/// registered from `NPPN_TBMODIFICATION` is the one judged — and a held
/// panel it registered, back from parking, is restored with the rest.
fn restore_plugin_panels(restore: &PanelRestore, withheld: &mut Vec<codepp_core::dock::DockPanel>) {
    if restore.panels.is_empty() && restore.held.is_empty() {
        return;
    }
    let Some(layout) = crate::dock::layout_snapshot() else {
        return;
    };
    let commands: Vec<i32> = with_state(|st| {
        let mut out: Vec<i32> = Vec::new();
        for &panel in restore.panels.iter().chain(&restore.held) {
            // A held panel its plugin has not registered is still
            // parked, and stays so.
            if layout.is_parked(panel) {
                continue;
            }
            let Some((index, seal)) = layout.open_command(panel) else {
                continue;
            };
            if !st.shell.may_run_panel_command(panel, index, seal) {
                tracing::info!(
                    panel = panel.persist_key(),
                    "not running this panel's startup command: Code++ did not sign it \
                     (Preferences > Security)"
                );
                withheld.push(panel);
                continue;
            }
            // One command can open several panels; a second run of a
            // toggle would close what the first opened.
            if let Some(cmd) = st
                .shell
                .panel_open_command_id(panel, index)
                .filter(|cmd| !out.contains(cmd))
            {
                out.push(cmd);
            }
        }
        out
    })
    .unwrap_or_default();
    for &cmd in &commands {
        tracing::debug!(cmd, "restoring a plugin panel by running its command");
        // A boundary per command, as each click on a menu item has:
        // host bookkeeping that fails for one panel must not cost the
        // rest of the pass their restores.
        crate::at_callback_boundary("plugin:restore:command", (), || {
            on_plugin_item_activated(cmd);
        });
    }
    if !commands.is_empty() && crate::dock::update_layout(|l| l.restore_fronts(&restore.fronts)) {
        crate::dock::apply_layout();
    }
}

/// Close each restored panel whose plugin loaded, was told to restore
/// it, heard `NPPN_READY` — and still never supplied a widget.
///
/// Such a panel is a group with a caption and nothing in it, and nothing
/// is going to fill it this session. Closing it (the layout remembers
/// where it was) is the honest outcome, and it is also what the user
/// would see under Notepad++, which has no container for a panel that was
/// never registered. A panel whose command the guard withheld is not
/// closed: nothing about it is known to be wrong, and
/// [`park_withheld_panels`] keeps its place instead.
fn close_unregistered_restored_panels(
    restore: &PanelRestore,
    withheld: &[codepp_core::dock::DockPanel],
) {
    let unregistered: Vec<codepp_core::dock::DockPanel> = restore
        .panels
        .iter()
        .copied()
        .filter(|p| !withheld.contains(p) && !crate::dock::is_plugin_panel_registered(*p))
        .collect();
    if unregistered.is_empty() {
        return;
    }
    let changed = crate::dock::update_layout(|l| {
        let mut changed = false;
        for &panel in &unregistered {
            if l.is_visible(panel) {
                tracing::warn!(
                    panel = panel.persist_key(),
                    "a restored plugin panel was never registered by its plugin; closing it"
                );
                l.hide(panel);
                changed = true;
            }
        }
        changed
    });
    if changed {
        crate::dock::apply_layout();
    }
}

/// Park each restored panel whose startup command Preferences →
/// Security's guard withheld and whose plugin, READY over, has not
/// registered it anyway.
///
/// Closing it, which is what happens to a panel whose command ran and
/// produced nothing, would record it closed. Nothing is wrong with this
/// one: Code++ declined to run a command it did not sign. Parked, it is
/// saved where it was and comes back there the moment its plugin
/// registers it — when the user opens it from that plugin's menu, which
/// also records the command, signed, for next time.
fn park_withheld_panels(withheld: &[codepp_core::dock::DockPanel]) {
    let waiting: Vec<codepp_core::dock::DockPanel> = withheld
        .iter()
        .copied()
        .filter(|p| !crate::dock::is_plugin_panel_registered(*p))
        .collect();
    if waiting.is_empty() {
        return;
    }
    let changed = crate::dock::update_layout(|l| {
        let visible: Vec<codepp_core::dock::DockPanel> = waiting
            .iter()
            .copied()
            .filter(|p| l.is_visible(*p))
            .collect();
        l.park(&visible)
    });
    if changed {
        crate::dock::apply_layout();
    }
}

/// Park every open plugin panel that has no widget and no loaded plugin
/// to supply one — its plugin is not installed, is disabled, failed to
/// load, or (on this platform) exists only for another one, named by a
/// session written on Windows.
///
/// Left in its group, such a panel is a caption with nothing under it for
/// the whole session. Closed, it would be recorded as closed and not come
/// back once its plugin does. Notepad++ does neither: measured against
/// 8.9.6 with the plugin removed, a panel it had saved open shows
/// nothing, its saved record is written back unchanged, and the panel
/// returns the next time the plugin is installed. A parked panel behaves
/// the same way — nothing presents it, and the layout still saves it
/// where it was (`DockLayout::park`).
///
/// Runs at the end of every load pass, and only ever finds something
/// after the startup one: every other open plugin panel was opened by its
/// plugin registering it. A panel with a widget is never parked, whichever
/// plugin its name belongs to.
fn park_unsupplied_plugin_panels() {
    let Some(layout) = crate::dock::layout_snapshot() else {
        return;
    };
    let widgetless: Vec<codepp_core::dock::DockPanel> = layout
        .open_plugin_panels()
        .into_iter()
        .filter(|p| !crate::dock::is_plugin_panel_registered(*p))
        .collect();
    if widgetless.is_empty() {
        return;
    }
    let unsupplied: Vec<codepp_core::dock::DockPanel> = with_state(|st| {
        let unsupplied = st.shell.panels_without_a_loaded_plugin(&widgetless);
        for &panel in &unsupplied {
            if st.shell.panel_record_is_trusted(&layout, panel) {
                tracing::info!(
                    panel = panel.persist_key(),
                    "no loaded plugin can supply this dock panel; keeping it for when one can"
                );
            } else {
                // Its plugin may well be installed: the guard is what
                // kept it from loading at startup.
                tracing::info!(
                    panel = panel.persist_key(),
                    "keeping this dock panel for when its plugin is loaded: Code++ did not \
                     sign its record, so the plugin was not loaded for it at startup \
                     (Preferences > Security)"
                );
            }
        }
        unsupplied
    })
    .unwrap_or_default();
    if crate::dock::update_layout(|l| l.park(&unsupplied)) {
        crate::dock::apply_layout();
    }
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

/// A menu item's display chord: Ctrl, Alt, Shift, and the Win32 virtual
/// key, as `Shell::plugin_shortcut_chord_for_cmd_id` reports it.
type Chord = (bool, bool, bool, u8);

/// One row of a plugin submenu: label, command id, whether it is a
/// command (vs. a separator), and its display chord if any.
type PluginMenuRow = (String, i32, bool, Option<Chord>);

/// The plugins' check marks on their own menu items, keyed by command id.
///
/// A Notepad++ plugin ticks its items by command id whenever it likes —
/// from `NPPN_TBMODIFICATION` or `NPPN_READY`, from one of its commands,
/// from a `DMN_CLOSE` about its panel — and a click never ticks or
/// unticks an item by itself: the mark changes only when the plugin says
/// so (`NPPM_SETMENUITEMCHECK`), or at load (`_init2Check`). This backend
/// rebuilds the Plugins menu from scratch every time it opens, so a mark
/// cannot live on a widget; it lives here, and every rebuild paints from
/// it. That also makes the order of loading and menu-building
/// unobservable: a tick set before the menu has ever been built is simply
/// waiting here for it.
#[derive(Debug, Default)]
struct PluginChecks {
    /// Every command a loaded plugin's `FuncItem` array publishes — the
    /// only ids a mark is recorded for, which bounds the map by what the
    /// plugins published rather than by what a buggy one sends.
    commands: HashSet<i32>,
    /// The last mark recorded for each command. A command with no entry
    /// has never been ticked or unticked, and its item is drawn as a
    /// plain one — no empty check box beside an action that is not a
    /// toggle, which is what an unchecked item looks like on Win32.
    marks: HashMap<i32, bool>,
}

impl PluginChecks {
    /// Take in the commands a load pass's plugins publish, as
    /// `(command id, is a command rather than a separator, _init2Check)`.
    /// `_init2Check` ticks a command that has no mark yet; a mark already
    /// recorded is the plugin's later word and is kept.
    fn absorb(&mut self, funcs: impl IntoIterator<Item = (i32, bool, bool)>) {
        for (cmd_id, is_command, init_checked) in funcs {
            if !is_command {
                continue;
            }
            self.commands.insert(cmd_id);
            if init_checked {
                self.marks.entry(cmd_id).or_insert(true);
            }
        }
    }

    /// Record a plugin's mark for `cmd_id`. `false` — nothing recorded —
    /// for an id no loaded plugin published as a command: a built-in
    /// `IDM_*` (this backend maps none), a separator, or a stray value.
    fn set(&mut self, cmd_id: i32, checked: bool) -> bool {
        if !self.commands.contains(&cmd_id) {
            return false;
        }
        self.marks.insert(cmd_id, checked);
        true
    }

    /// The mark recorded for `cmd_id`, if the plugin has ever set one.
    fn get(&self, cmd_id: i32) -> Option<bool> {
        self.marks.get(&cmd_id).copied()
    }
}

/// The Plugins-menu item currently built for one command, with what it
/// was built from — enough to build it again as a check item in place,
/// the first time its plugin ticks it while the menu is up.
struct LiveItem {
    item: gtk::MenuItem,
    label: String,
    chord: Option<Chord>,
}

thread_local! {
    /// See [`PluginChecks`].
    static PLUGIN_CHECKS: std::cell::RefCell<PluginChecks> =
        std::cell::RefCell::new(PluginChecks::default());
    /// The items the current Plugins menu holds, by command id. Replaced
    /// wholesale on every rebuild.
    static LIVE_ITEMS: std::cell::RefCell<HashMap<i32, LiveItem>> =
        std::cell::RefCell::new(HashMap::new());
    /// Set while the host itself changes a check item's state. GTK 3's
    /// `gtk_check_menu_item_set_active` re-emits `activate` when the
    /// state really changes, and without this that would run the
    /// plugin's command as if the user had clicked.
    static SYNCING_CHECKS: Cell<bool> = const { Cell::new(false) };
}

/// Record the mark a plugin set on one of its items through
/// `NPPM_SETMENUITEMCHECK`, and show it on the item if the menu is up.
/// `false` when `cmd_id` is none of the loaded plugins' commands.
///
/// Called from inside the dispatch's state borrow, so it touches only
/// this module's own state and the item's widget; the item's `activate`
/// handler returns at once while [`SYNCING_CHECKS`] is set.
pub(crate) fn set_menu_check(cmd_id: i32, checked: bool) -> bool {
    if !PLUGIN_CHECKS.with(|c| c.borrow_mut().set(cmd_id, checked)) {
        tracing::trace!(
            cmd_id,
            "NPPM_SETMENUITEMCHECK: no loaded plugin's command has that id on this backend"
        );
        return false;
    }
    show_recorded_mark(cmd_id);
    true
}

/// Take in the commands every loaded plugin publishes — see
/// [`PluginChecks::absorb`]. Run after each load pass and **before** its
/// notifications, so a plugin ticking an item from
/// `NPPN_TBMODIFICATION` or `NPPN_READY` finds its commands known, as
/// Notepad++ has them installed by then.
fn absorb_loaded_commands() {
    let funcs: Vec<(i32, bool, bool)> = with_state(|st| {
        st.shell
            .loaded_plugin_funcs()
            .flat_map(|(_, funcs)| {
                funcs
                    .iter()
                    .map(|f| (f.cmd_id, f.p_func.is_some(), f.init2_check != 0))
            })
            .collect()
    })
    .unwrap_or_default();
    PLUGIN_CHECKS.with(|c| c.borrow_mut().absorb(funcs));
}

/// Make the live item for `cmd_id`, if the menu holds one, show the mark
/// recorded for it: set a check item's state, or build a plain item
/// again as a check item in the same place — the first time its plugin
/// ticks it while the menu is up.
fn show_recorded_mark(cmd_id: i32) {
    let Some(mark) = PLUGIN_CHECKS.with(|c| c.borrow().get(cmd_id)) else {
        return;
    };
    let live = LIVE_ITEMS.with(|l| {
        l.borrow()
            .get(&cmd_id)
            .map(|live| (live.item.clone(), live.label.clone(), live.chord))
    });
    let Some((item, label, chord)) = live else {
        return;
    };
    if let Some(check) = item.downcast_ref::<gtk::CheckMenuItem>() {
        if check.is_active() != mark {
            let _syncing = crate::FlagGuard::set(&SYNCING_CHECKS);
            check.set_active(mark);
        }
        return;
    }
    let Some(menu) = item.parent().and_then(|p| p.downcast::<gtk::Menu>().ok()) else {
        return;
    };
    let Some(pos) = menu
        .children()
        .iter()
        .position(|c| c == item.upcast_ref::<gtk::Widget>())
    else {
        return;
    };
    let rebuilt = build_command_item(&label, cmd_id, chord);
    menu.remove(&item);
    menu.insert(&rebuilt, i32::try_from(pos).unwrap_or(-1));
    rebuilt.show();
}

/// Build the Plugins-menu item for one plugin command: a check item
/// showing the recorded mark once the plugin has set one, a plain item
/// before that. Registered as the command's live item.
fn build_command_item(label: &str, cmd_id: i32, chord: Option<Chord>) -> gtk::MenuItem {
    let mark = PLUGIN_CHECKS.with(|c| c.borrow().get(cmd_id));
    let item: gtk::MenuItem = match mark {
        Some(active) => {
            let check = gtk::CheckMenuItem::with_label(label);
            // Seeded before `activate` is connected, so it runs nothing.
            check.set_active(active);
            check.upcast()
        }
        None => gtk::MenuItem::with_label(label),
    };
    item.connect_activate(move |_| {
        crate::at_callback_boundary("plugin:item:activate", (), || {
            on_plugin_item_activated(cmd_id);
        });
    });
    // Show the shortcut hint via the display-only accel group (never
    // routes the key — the real binding is `register_startup_shortcuts`).
    // Only a chord that will actually fire is advertised (the shell
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
    LIVE_ITEMS.with(|l| {
        l.borrow_mut().insert(
            cmd_id,
            LiveItem {
                item: item.clone(),
                label: label.to_owned(),
                chord,
            },
        );
    });
    item
}

/// A click on a plugin's menu item: run its command, then put back the
/// plugin's own mark.
///
/// GTK toggles a check item's state in its class handler, before this
/// runs; a Notepad++ item changes its tick only when the plugin says so.
/// So whatever the click did to the state is undone here, after the
/// command — which may itself have set a new mark, and that is the one
/// shown.
fn on_plugin_item_activated(cmd_id: i32) {
    if SYNCING_CHECKS.with(Cell::get) {
        // The host changing the state itself, not a click.
        return;
    }
    on_plugin_command(cmd_id);
    show_recorded_mark(cmd_id);
}

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
    // The items just removed are gone; `build_command_item` registers
    // the ones that replace them.
    LIVE_ITEMS.with(|l| l.borrow_mut().clear());
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
                    submenu.append(&build_command_item(&label, cmd_id, chord));
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
    // thread with no arguments, per the N++ ABI, and marked as its own
    // plugin while it runs (`codepp_plugin_host::caller`). The boundary
    // keeps a Rust-plugin panic from unwinding across `extern "C"`.
    crate::at_callback_boundary("plugin:command", (), || unsafe { cmd.run() });
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

/// Tell every loaded plugin the host is shutting down:
/// `NPPN_BEFORESHUTDOWN` to each of them, then `NPPN_SHUTDOWN` to each —
/// the pair Win32 sends from `WM_CLOSE`, while the plugins' panels still
/// exist.
///
/// Delivered from a snapshot with no borrow held, so a plugin that saves
/// its settings here can ask the host where (`NPPM_GETPLUGINSCONFIGDIR`)
/// and be answered — which Win32, whose teardown cannot drop its borrow
/// first, declines. A plugin cannot veto the shutdown: the notifications
/// are informational, as they are there. Called once, by `crate::quit`.
pub(crate) fn notify_shutdown() {
    let Some(targets) = with_state(|st| st.shell.notify_targets()) else {
        return;
    };
    for note in [
        codepp_plugin_host::Notification::BeforeShutdown,
        codepp_plugin_host::Notification::Shutdown,
    ] {
        targets.deliver(&note, npp_sentinel());
    }
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
mod check_tests {
    use super::PluginChecks;

    /// Only a loaded plugin's own commands take a mark — not a built-in
    /// id, not a separator's slot, not a stray value — so the table is
    /// bounded by what the plugins published.
    #[test]
    fn a_mark_is_recorded_only_for_a_published_command() {
        let mut checks = PluginChecks::default();
        checks.absorb([(50_000, true, false), (50_001, false, false)]);
        assert!(checks.set(50_000, true));
        assert_eq!(checks.get(50_000), Some(true));
        assert!(!checks.set(50_001, true), "a separator is no command");
        assert!(
            !checks.set(42_001, true),
            "a built-in IDM_* id is not mapped"
        );
        assert!(!checks.set(-1, false));
        assert_eq!(checks.get(50_001), None);
        assert_eq!(checks.get(42_001), None);
    }

    /// `_init2Check` ticks a command that has no mark yet and yields to
    /// one the plugin set since — a later load pass re-absorbing the same
    /// commands must not undo the plugin's own untick.
    #[test]
    fn init2check_seeds_a_mark_without_overriding_one() {
        let mut checks = PluginChecks::default();
        checks.absorb([(50_010, true, true), (50_011, true, false)]);
        assert_eq!(checks.get(50_010), Some(true), "_init2Check ticks it");
        assert_eq!(checks.get(50_011), None, "no mark: drawn as a plain item");
        assert!(checks.set(50_010, false));
        checks.absorb([(50_010, true, true)]);
        assert_eq!(
            checks.get(50_010),
            Some(false),
            "the plugin's untick survives"
        );
    }

    /// An untick is a mark too: an item the plugin has unticked is a
    /// toggle the user should see as one, empty, rather than a plain
    /// action.
    #[test]
    fn an_untick_is_recorded_as_a_mark() {
        let mut checks = PluginChecks::default();
        checks.absorb([(50_020, true, false)]);
        assert!(checks.set(50_020, false));
        assert_eq!(checks.get(50_020), Some(false));
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

/// A refused registration logs the plugin's two strings as the chrome
/// would draw them — the twin of `ui_win32`'s
/// `a_refused_registration_is_logged_sanitized`, which says why.
#[cfg(test)]
mod registration_log_tests {
    use crate::source_scan::{code_only, strip_test_modules};

    #[test]
    fn a_refused_registration_is_logged_sanitized() {
        let flat = strip_test_modules(&code_only(include_str!("plugin.rs")))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for (field, sanitized) in [
            (
                "module",
                "codepp_shell::sanitize_str_for_display(&params.module_name)",
            ),
            (
                "panel",
                "codepp_shell::plugin_dock_title(&params.name, &params.module_name)",
            ),
        ] {
            assert!(
                flat.contains(&format!("{field} = {sanitized}")),
                "the refusal log's {field} field is not sanitized"
            );
        }
        assert!(
            !flat.contains("= params.name") && !flat.contains("= params.module_name"),
            "a log field records the plugin's raw text"
        );
    }
}
