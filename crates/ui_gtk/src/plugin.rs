//! GTK plugin-host wiring.
//!
//! The plugin host, discovery, lifecycle, and NPPM/NPPN dispatcher are
//! all cross-platform (`codepp-plugin-host` + `codepp-shell`). This
//! module supplies the GTK-specific pieces:
//!
//! 1. **The message-routing bridge.** On Windows a plugin's
//!    `SendMessage(scintillaHandle, SCI_*, …)` is routed by the OS
//!    message pump for free — the handle *is* the Scintilla window. A
//!    Linux plugin `.so` has no Scintilla linked and there is no OS
//!    pump, so the SDK forwards every `SendMessage` to a host callback:
//!    a route of the plugin's own (`codepp_plugin_host::plugin_route`,
//!    for the first 128 plugins found), which marks the plugin as the
//!    one whose message it is and hands it
//!    to [`plugin_dispatch`]. [`plugin_dispatch`] routes **by handle
//!    identity** (`SCI` and `NPPM` message numbers overlap, so routing
//!    by range is impossible) — the [`NPP_SENTINEL`] address goes to the
//!    host dispatcher, a Scintilla widget of the host's making (its own,
//!    or one it made for a plugin) goes to `scintilla_send_message`, and
//!    any other pointer is refused. It also restores the **thread
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
//! 5. **What a plugin asks the host to make** — a Scintilla widget of
//!    its own ([`create_plugin_scintilla`]), a toolbar button for one of
//!    its commands ([`add_toolbar_icon`]) — and the modeless-dialog
//!    registration, which on this platform only makes the dialog
//!    transient for the main window ([`register_modeless_dialog`]).
//!
//! # Re-entrancy
//!
//! A plugin menu callback and `beNotified` are invoked with **no**
//! `with_state` borrow held (the caller looks up the function pointer,
//! drops the borrow, then calls) so the plugin's own re-entrant `NPPM_*`
//! calls acquire a fresh borrow and actually work. This is the
//! memory-safe GTK equivalent of Win32's `PLUGIN_CALL_ACTIVE` guard;
//! `with_state`'s `try_borrow_mut` already declines true re-entry.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib;
use gtk::glib::translate::{from_glib_borrow, from_glib_none, Borrowed};
use gtk::prelude::*;

use codepp_editor::EditorHandle;
use codepp_plugin_host::{
    may_make_plugin_scintilla, HostDispatchFn, NppData, PluginMessageProc, SCNotification,
    MAX_PLUGIN_SCINTILLAS, WM_NOTIFY,
};
use codepp_scintilla_sys::{
    scintilla_new, scintilla_send_message, SCI_GETMODIFY, SCI_SETCODEPAGE, SCN_UPDATEUI, SC_CP_UTF8,
};
use codepp_shell::HostHandles;

use crate::state::with_state;

/// The host's own Scintilla widget pointer, cached so [`plugin_dispatch`]
/// can identity-check the handle a plugin routes an `SCI_*` message to
/// and **refuse any pointer that is not a Scintilla of the host's
/// making** — matching Win32's `SendMessage` to an unknown `HWND`, which
/// returns 0 without dereferencing. Without this, a plugin passing a
/// garbage pointer would fault inside `scintilla_send_message` (a raw
/// dereference), where Win32 fails soft. Read as an atomic rather than
/// through `with_state`, so the check still works when a plugin sends
/// `SCI_*` from inside a `beNotified` that holds the borrow. Set once at
/// startup by [`discover`].
///
/// The Document Map's miniature is deliberately **not** here, nor in
/// [`PLUGIN_SCIS`]: a plugin is only ever handed
/// `NppData._scintillaMainHandle` and the widgets it asked the host to
/// make, so a message addressed to the miniature did not come from
/// anywhere legitimate.
static VALID_SCI: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// A dedicated sentinel whose *address* is the GTK backend's "npp
/// handle". A plugin sends `NPPM_*` to this pointer; [`plugin_dispatch`]
/// recognises it by identity and routes to the host dispatcher, while a
/// Scintilla widget of the host's making routes to Scintilla and any
/// other pointer is refused. The **same** address fills
/// `NppData.npp_handle`, `HostHandles.npp_hwnd`, and every outbound
/// `DMN_*` `nmhdr.hwndFrom`, so a plugin that caches the host handle
/// routes back here rather than into `scintilla_send_message`.
static NPP_SENTINEL: u8 = 0;

/// The npp-handle sentinel pointer. Stable for the process lifetime.
fn npp_sentinel() -> *mut c_void {
    std::ptr::addr_of!(NPP_SENTINEL).cast_mut().cast::<c_void>()
}

/// Whether `hwnd` is the host's own Scintilla widget.
fn is_valid_scintilla(hwnd: *mut c_void) -> bool {
    let valid = VALID_SCI.load(Ordering::Acquire);
    !valid.is_null() && std::ptr::eq(hwnd, valid)
}

/// Every Scintilla widget made for a plugin, in the order made: the
/// handles besides the host's own that [`plugin_dispatch`] forwards
/// `SCI_*` to. One slot per widget the host will ever make for plugins
/// ([`MAX_PLUGIN_SCINTILLAS`], from `codepp_plugin_host`).
///
/// Never reused, which is what makes reading it from any thread sound. A
/// slot is written with its widget once, on the main thread, before
/// [`PLUGIN_SCI_COUNT`] publishes it with release ordering, and cleared,
/// also on the main thread, as the widget's dispose begins
/// ([`retire_plugin_scintilla`]) — never given another widget. The widget
/// a slot named is never finalized (the host's reference), so a stale read
/// on another thread still names that widget and no other object; what
/// keeps a destroyed one from being sent anything is the re-check on the
/// main thread before every send ([`send_sci_on_main`]). Atomics rather
/// than a `with_state` read for the reason [`VALID_SCI`] is one.
static PLUGIN_SCIS: [AtomicPtr<c_void>; MAX_PLUGIN_SCINTILLAS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_PLUGIN_SCINTILLAS];

/// How many slots of [`PLUGIN_SCIS`] are filled.
static PLUGIN_SCI_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether `hwnd` is a Scintilla widget the host made for a plugin, and
/// not since destroyed. Exact on the main thread, which writes every slot;
/// another thread can see a destroyed widget a moment longer, which the
/// marshal's re-check on the main thread catches.
fn is_plugin_scintilla(hwnd: *mut c_void) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let made = PLUGIN_SCI_COUNT.load(Ordering::Acquire);
    PLUGIN_SCIS
        .iter()
        .take(made)
        .any(|slot| std::ptr::eq(slot.load(Ordering::Relaxed), hwnd))
}

/// Whether `hwnd` is a Scintilla widget this host made — its own, or one
/// it made for a plugin. These are the only pointers [`plugin_dispatch`]
/// forwards an `SCI_*` message to.
fn is_known_scintilla(hwnd: *mut c_void) -> bool {
    is_valid_scintilla(hwnd) || is_plugin_scintilla(hwnd)
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
// [`is_known_scintilla`]: the host's own `ScintillaObject*`, or one it
// made for a plugin. Neither is ever finalized. The host's is created
// once at startup and never destroyed, removed from its container or
// reassigned (the discipline `GtkUiState::sci_widget` documents and a
// source-scan guard enforces); a plugin's keeps the reference the host
// took when it made it for the rest of the process (see
// [`create_plugin_scintilla`]). So the address names the same object for
// the whole process. A plugin's widget may be *destroyed* meanwhile, which
// this cannot rule out: the hop checks again, on the main thread, before
// it sends anything. It is *dereferenced only on the main thread*, which
// is the entire point of the marshal — the value crosses threads, the
// dereference does not.
unsafe impl Send for MainThreadPtr {}

/// How many messages a worker thread has handed to the main loop — what
/// the display scenario waits on, so a destroy it stages is known to come
/// after the worker's own check.
#[cfg(test)]
static HOPS_QUEUED: AtomicUsize = AtomicUsize::new(0);

/// Run one `SCI_*` message against Scintilla on the UI thread and block
/// until it returns, for a plugin that called from its own thread.
///
/// # Why marshal rather than refuse
///
/// Off Windows the SDK forwards a plugin's `SendMessage`, through the
/// plugin's route, to this host callback on whatever thread called it,
/// where Win32 would have had the OS marshal it onto the thread owning
/// the window. Both available answers were considered and this one is
/// deliberate:
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
            // Checked again here, on the thread that destroys widgets: a
            // plugin's widget can have been destroyed since the calling
            // thread checked it, and a destroyed one is sent nothing — the
            // 0 a destroyed window answers on Windows.
            let result = if is_known_scintilla(ptr.0) {
                // SAFETY: `ptr.0` is a live `ScintillaObject*` of the host's
                // making, checked on this thread, which is the one that
                // marks a destroyed widget (see `MainThreadPtr`), and the
                // affinity GTK requires — the reason the message was
                // marshaled here at all.
                unsafe { scintilla_send_message(ptr.0, msg, wparam, lparam) }
            } else {
                0
            };
            // The receiver is alive by construction — the calling thread
            // is parked in `recv` — unless it panicked, in which case
            // dropping the result is correct.
            let _ = tx.send(result);
        });
    });
    #[cfg(test)]
    HOPS_QUEUED.fetch_add(1, Ordering::SeqCst);
    rx.recv().unwrap_or_else(|_| {
        tracing::warn!(
            msg,
            "cross-thread SCI_* dropped: the main-thread hop never answered \
             (it panicked — see the error above — or the context is gone)"
        );
        0
    })
}

/// The router a plugin's `SendMessage` reaches, through the route of its
/// own the host gave it as it loaded (`codepp_plugin_host::plugin_route`),
/// which marks the plugin first.
///
/// `hwnd == npp_sentinel()` → an `NPPM_*` message for the host
/// dispatcher; a Scintilla widget of the host's making → an `SCI_*`
/// message for it; anything else → refused. Runs at a
/// [`crate::at_callback_boundary`]: it is entered from plugin C code, and
/// a Rust panic unwinding across that frame is UB (dev builds default to
/// unwind).
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
        } else if is_known_scintilla(hwnd) {
            // SCI_* addressed to a Scintilla widget of ours — the host's
            // own, or one made for a plugin: send it straight to
            // Scintilla's GTK message entry point — the analogue of Win32
            // routing SendMessage to the Scintilla HWND. `with_state` is
            // deliberately not taken (this is a direct Scintilla call,
            // and the plugin may issue it from inside an NPPM dispatch that
            // already holds the borrow); the identity check is an atomic
            // read for the same reason, and runs *before* the affinity
            // check so an unknown handle is refused rather than marshaled.
            if on_main_thread() {
                // SAFETY: `hwnd` is identity-checked, on this thread — the
                // one that marks a destroyed widget — to be a live
                // `ScintillaObject*` of the host's making (see
                // `MainThreadPtr`), and this is the thread that owns it;
                // `scintilla_send_message` is its documented entry point.
                // The message-argument contract is the plugin's
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
    // Win32's `dock_dirty`. It also sends the `DMN_*` the change owes —
    // a registration's `DMN_DOCK` / `DMN_FLOAT`, a show's `DMN_SWITCHIN`
    // and `DMN_FLOATDROPPED` — before the plugin's `SendMessage` returns.
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
    /// `DMN_*` notices a dock reconcile owes, waiting to be sent. See
    /// [`deliver_dock_notices`].
    static DOCK_NOTICES: std::cell::RefCell<std::collections::VecDeque<crate::dock::DockNotice>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Set while [`deliver_dock_notices`] is draining.
    static DOCK_NOTICES_DELIVERING: Cell<bool> = const { Cell::new(false) };
    /// Notices dropped since the last warning: refused because the queue
    /// already held as many as one delivery may send, or still queued
    /// when a delivery reached its cap. See [`deliver_dock_notices`].
    static DOCK_NOTICES_DROPPED: Cell<usize> = const { Cell::new(0) };
    /// Set while a `DMN_CLOSE` is being delivered. See
    /// [`close_plugin_panel`].
    static DMN_CLOSE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// A stand-in for a plugin's `DMN_*` handler, as the display scenarios
/// install it. See [`sent_dock_notices::stand_in`].
#[cfg(test)]
type StandInHandler = Box<dyn FnMut(codepp_core::dock::DockPanel, u32)>;

#[cfg(test)]
thread_local! {
    /// What [`send_dock_notification`] has sent, as `(panel, code)`, while
    /// a display scenario is recording it — `None` otherwise. The dock rig
    /// those scenarios drive has no shell behind it, so there is no plugin
    /// to deliver to; this is how a scenario sees what each change owed.
    /// See [`sent_dock_notices`].
    static SENT_DOCK_NOTICES: std::cell::RefCell<Option<Vec<(codepp_core::dock::DockPanel, u32)>>> =
        const { std::cell::RefCell::new(None) };
    /// What runs where a plugin's handler would, at each notice sent. See
    /// [`sent_dock_notices::stand_in`].
    static STAND_IN_HANDLER: std::cell::RefCell<Option<StandInHandler>> =
        const { std::cell::RefCell::new(None) };
}

/// Recording what the dock sends plugins, and standing in for a plugin
/// that answers, for the display scenarios.
#[cfg(test)]
pub(crate) mod sent_dock_notices {
    use codepp_core::dock::DockPanel;

    /// A notice is being sent: record it, then run the stand-in handler if
    /// one is installed, where the plugin's own handler would run. The
    /// handler is taken out while it runs, so a delivery it starts runs
    /// none — which is what a missing latch would show as.
    pub(super) fn sent(panel: DockPanel, code: u32) {
        super::SENT_DOCK_NOTICES.with(|sent| {
            if let Some(sent) = sent.borrow_mut().as_mut() {
                sent.push((panel, code));
            }
        });
        let handler = super::STAND_IN_HANDLER.with(|handler| handler.borrow_mut().take());
        if let Some(mut handler) = handler {
            handler(panel, code);
            super::STAND_IN_HANDLER.with(|slot| {
                slot.borrow_mut().get_or_insert(handler);
            });
        }
    }

    /// Run `handler` at each notice sent from now on, as a plugin's
    /// handler runs — with no borrow held, inside the delivery — until
    /// [`stand_down`].
    pub(crate) fn stand_in(handler: impl FnMut(DockPanel, u32) + 'static) {
        super::STAND_IN_HANDLER.with(|slot| *slot.borrow_mut() = Some(Box::new(handler)));
    }

    /// Remove the stand-in handler.
    pub(crate) fn stand_down() {
        super::STAND_IN_HANDLER.with(|slot| *slot.borrow_mut() = None);
    }

    /// Record the notices sent from now on, starting from none.
    pub(crate) fn record() {
        super::SENT_DOCK_NOTICES.with(|sent| *sent.borrow_mut() = Some(Vec::new()));
    }

    /// Stop recording, and forget what was recorded.
    pub(crate) fn stop() {
        super::SENT_DOCK_NOTICES.with(|sent| *sent.borrow_mut() = None);
    }

    /// The notices sent since the last call.
    pub(crate) fn take() -> Vec<(DockPanel, u32)> {
        super::SENT_DOCK_NOTICES.with(|sent| {
            sent.borrow_mut()
                .as_mut()
                .map(std::mem::take)
                .unwrap_or_default()
        })
    }

    /// Run `f` as a plugin's handler runs: while a delivery is draining,
    /// so a delivery `f` asks for only queues.
    pub(crate) fn inside_a_delivery(f: impl FnOnce()) {
        let _delivering = crate::FlagGuard::set(&super::DOCK_NOTICES_DELIVERING);
        f();
    }

    /// How many notices are waiting to be sent.
    pub(crate) fn queued() -> usize {
        super::DOCK_NOTICES.with(|q| q.borrow().len())
    }
}

/// `NPPM_DMMREGASDCKDLG` on this backend: adopt the plugin's widget as a
/// dock panel's content, returning the panel it interns to — for the
/// shell to record, and sign, the command that reopens it.
///
/// `hClient` must be a `GtkWidget*` the plugin made and has not put in a
/// container or made a window of; the host takes its own reference
/// (sinking a floating one, as a container's `add` would) and never
/// destroys it. The registration stands while the widget stays inside
/// the host's container. Once the plugin takes it out — to move it
/// elsewhere, or by destroying it — the `NPPM_DMM*` messages no longer
/// find it, and if it is still out when control is back in the main
/// loop the host closes the panel and releases its reference
/// (`crate::dock`'s retirement pass). A widget taken out and left
/// free-standing can be registered again; shown, the panel comes back
/// where it was.
///
/// The checks below refuse what can be told apart without
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
    // The raw getter, not `parent()`: gtk-rs wraps a getter's result with
    // `from_glib_none`, which takes over a *floating* reference — and a
    // container the plugin made and has not sunk is floating, so the
    // wrapper dropping at the end of this statement would finalize it,
    // while refusing a mistake the plugin made.
    //
    // SAFETY: a live `GtkWidget`, per the check above; the getter only
    // reads a field.
    if !unsafe { gtk::ffi::gtk_widget_get_parent(handle.cast()) }.is_null() {
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
    let spec = crate::dock::PluginPanelSpec {
        panel,
        handle,
        tb_data: params.tb_data,
        icon: tab_icon(&params),
        initial_side: codepp_plugin_host::docking::dock_side_from_u_mask(params.u_mask),
        name: params.name,
        module_name: params.module_name,
        caller: params.caller,
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
        Ok(()) => Some(panel),
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
/// Refused too, by address: a Scintilla widget the host made for a plugin
/// that the plugin has destroyed, which GTK must not be asked to show
/// again — see [`is_destroyed_plugin_scintilla`].
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
    if is_destroyed_plugin_scintilla(handle) {
        return Err("hClient is a Scintilla widget its plugin has destroyed");
    }
    Ok(())
}

/// Whether `handle` is a Scintilla widget the host made for a plugin that
/// has since been destroyed. Still allocated — the host keeps its
/// reference — but no longer something to show: Scintilla would lay it
/// out from the scrollbars its dispose took away, and trips its own
/// assertions doing so. Compares addresses only. Main thread only, like
/// every caller.
fn is_destroyed_plugin_scintilla(handle: *mut c_void) -> bool {
    PLUGIN_SCINTILLAS.with(|made| {
        made.try_borrow().is_ok_and(|made| {
            made.iter().enumerate().any(|(index, made)| {
                std::ptr::eq(made.view, handle)
                    && PLUGIN_SCIS
                        .get(index)
                        .is_some_and(|slot| slot.load(Ordering::Relaxed).is_null())
            })
        })
    })
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
/// So does a close once the quit has begun — a click while a plugin's
/// shutdown handler spins a main loop, say: the plugin has heard
/// `NPPN_SHUTDOWN`, or is hearing it, and is told nothing more.
pub(crate) fn close_plugin_panel(panel: codepp_core::dock::DockPanel) {
    if let Some((handle, caller)) = crate::dock::plugin_panel_notify_target(panel) {
        if !DMN_CLOSE_ACTIVE.with(Cell::get) && !crate::quitting() {
            let _closing = crate::FlagGuard::set(&DMN_CLOSE_ACTIVE);
            send_dock_notification(panel, handle, caller, codepp_plugin_host::DMN_CLOSE);
        }
    }
    crate::dock::set_panel_visible(panel, false);
}

/// Send each notice a dock reconcile owes: `DMN_DOCK` / `DMN_FLOAT`,
/// `DMN_SWITCHIN` / `DMN_SWITCHOFF` and `DMN_FLOATDROPPED`, in the order
/// `crate::dock::panel_notices` queued them.
///
/// **Notices raised while one is being delivered are queued, not sent.**
/// The plugin's handler runs with no borrow held, so it may send
/// `NPPM_*` back — including `NPPM_DMMREGASDCKDLG` for a panel nothing
/// has been told about, which reconciles again from inside this loop and
/// raises a notice of its own. Sent there and then, that notice's
/// handler could do the same, nesting a full round trip per link until
/// the registration cap or the stack ran out. So a call made while a
/// delivery is running only appends to the queue and returns, and the
/// outermost call drains it in order: nothing is dropped that is still
/// true, and the nesting stays one level deep whatever the plugin does.
/// The same queue Win32's `deliver_container_notices` and Cocoa's keep.
///
/// Queued behind a handler that may change the layout, a notice is
/// checked again when its turn comes (`crate::dock::notice_still_applies`)
/// and skipped if it no longer says something true — a registration
/// gone, a panel switched in and closed again before hearing of it. The
/// record is already written, so a skipped notice loses nothing that
/// could still be delivered.
///
/// The queue bounds depth, and the shared cap,
/// `codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY`, bounds the
/// rest: one delivery works through at most that many notices, sent or
/// found stale, and drops the rest of the queue; and the queue never
/// holds more than that many ([`queue_dock_notices`]), so a handler that
/// keeps raising notices while one is delivered cannot grow it either.
/// What is dropped is reported in one warning when the delivery ends.
///
/// A handler may also begin the quit — by closing the main window — and
/// once it has, the rest of the queue is dropped unsent: every plugin is
/// about to hear `NPPN_SHUTDOWN`, or has, and is told nothing more.
pub(crate) fn deliver_dock_notices(notices: Vec<crate::dock::DockNotice>) {
    queue_dock_notices(notices);
    if DOCK_NOTICES_DELIVERING.with(Cell::get) {
        return;
    }
    let _delivering = crate::FlagGuard::set(&DOCK_NOTICES_DELIVERING);
    let mut taken = 0usize;
    while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) {
        taken += 1;
        if taken > codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY {
            let left = DOCK_NOTICES.with(|q| {
                let mut q = q.borrow_mut();
                let left = q.len();
                q.clear();
                left
            });
            DOCK_NOTICES_DROPPED.with(|dropped| dropped.set(dropped.get() + 1 + left));
            break;
        }
        if crate::quitting() {
            DOCK_NOTICES.with(|q| q.borrow_mut().clear());
            break;
        }
        if !crate::dock::notice_still_applies(&notice) {
            continue;
        }
        tracing::debug!(
            panel = notice.panel.persist_key(),
            dmn = codepp_plugin_host::docking::dmn_name(notice.code),
            container = notice.code >> 16,
            "dock panel notification"
        );
        send_dock_notification(notice.panel, notice.handle, notice.caller, notice.code);
    }
    let dropped = DOCK_NOTICES_DROPPED.with(|dropped| dropped.replace(0));
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "plugins keep changing the dock layout from their DMN_* handlers; \
             dropped the notifications past one delivery's cap"
        );
    }
}

/// Append `notices` to the queue, keeping it no longer than one delivery
/// may send. Anything past that could never go out — a delivery drops
/// whatever is still queued once it reaches the cap — and refusing it
/// here is what bounds the queue while a plugin's handler keeps raising
/// notices from inside a delivery. What is refused is counted for the
/// delivery's warning.
fn queue_dock_notices(notices: Vec<crate::dock::DockNotice>) {
    let refused = DOCK_NOTICES.with(|q| {
        let mut q = q.borrow_mut();
        let room = codepp_plugin_host::docking::MAX_NOTICES_PER_DELIVERY.saturating_sub(q.len());
        let refused = notices.len().saturating_sub(room);
        q.extend(notices.into_iter().take(room));
        refused
    });
    if refused > 0 {
        DOCK_NOTICES_DROPPED.with(|dropped| dropped.set(dropped.get() + refused));
    }
}

/// Deliver one `DMN_*` about `panel` to the plugin that should hear it —
/// see `Shell::plugin_panel_message_target` — through its `messageProc`:
/// `WM_NOTIFY`, `wParam` the panel's `hClient`, `lParam` an `NMHDR` from
/// the npp handle with `idFrom` 0 and `code` as given. Called with no
/// borrow held.
///
/// Sends nothing once the quit has begun. Every `DMN_*` goes out through
/// here, so that holds for any caller, one added later included. The
/// callers stop sooner — a reconcile raises nothing, a delivery drops
/// its queue, a close skips its `DMN_CLOSE` — and this is the backstop
/// behind them.
fn send_dock_notification(
    panel: codepp_core::dock::DockPanel,
    handle: *mut c_void,
    caller: Option<usize>,
    code: u32,
) {
    if crate::quitting() {
        return;
    }
    #[cfg(test)]
    sent_dock_notices::sent(panel, code);
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

// --- what a plugin asks the host to make --------------------------------------------
//
// `NPPM_CREATESCINTILLAHANDLE`, `NPPM_MODELESSDIALOG` and
// `NPPM_ADDTOOLBARICON`, on this backend. Each takes an `HWND` or an
// `HICON` on Windows; here the same argument is the GTK object that
// plays the part — a `GtkWidget*`, a `GtkWindow*`, a `GdkPixbuf*` — as a
// dock panel's `hClient` is a `GtkWidget*`. The checks on those pointers
// are bug containment, not a boundary (DESIGN.md §6.5): the handles the
// host can recognise without trusting the pointer come first, then what
// the GObject type system can tell about the object, and a pointer to
// something that is not an object at all faults in the type check rather
// than being declined, as it does for a dock panel.
//
// Nothing in a plugin's widget tree is ever wrapped as an owned gtk-rs
// object here — only borrowed, compared, or asked `is_ancestor`. gtk-rs
// wraps a getter's result with `from_glib_none`, which takes over a
// floating reference, so a temporary wrapper around a container the
// plugin made and has not sunk would finalize it when dropped. The one
// owned read is a dialog's `transient_for()`, which names a toplevel
// window, and GTK holds every toplevel by a reference of its own from
// creation, so that one is never floating.

/// What the host knows about one Scintilla widget it made for a plugin.
/// Its index in [`PLUGIN_SCINTILLAS`] is its slot in [`PLUGIN_SCIS`].
struct PluginScintilla {
    /// The widget, holding the reference the host took when it made it.
    /// Nothing ever gives that reference up: a raw pointer rather than a
    /// `gtk::Widget`, so no destructor can — see
    /// [`create_plugin_scintilla`].
    view: *mut c_void,
    /// Scintilla's own children — its scrollbars and its text area —
    /// held by the host as well, for as long as the UI thread lives. The
    /// widget's dispose
    /// unparents its scrollbars, which would free them, and the
    /// adjustments Scintilla keeps raw pointers to with them, while the
    /// object itself lives on; see [`create_plugin_scintilla`].
    _internals: Vec<gtk::Widget>,
    /// The host's direct-call handle for it, for what the host does to the
    /// widget itself — `None` if Scintilla would not give one, when the
    /// widget goes without that. Used only while the widget is live; the
    /// object it calls into is never finalized.
    editor: Option<EditorHandle>,
    /// The plugin that asked for it — the one whose `messageProc` hears
    /// its notifications — or `None` when the host cannot name it: a
    /// plugin beyond the routed ones (`codepp_plugin_host::plugin_route`),
    /// asking from outside any call the host made into it.
    owner: Option<usize>,
    /// Who hears its notifications.
    target: NotifyTarget,
}

/// Who hears the notifications of a widget made for a plugin.
#[derive(Clone, Copy)]
enum NotifyTarget {
    /// Not looked up yet. It is looked up at a notification rather than
    /// when the widget is made, because [`create_plugin_scintilla`] runs
    /// inside the NPPM dispatch's state borrow, where the plugin registry
    /// cannot be read — and it stays unresolved until the lookup finds the
    /// plugin loaded, since a plugin may make a widget from its own
    /// `setInfo`, before it is.
    Unresolved,
    /// The `messageProc` of the plugin that asked for the widget.
    Plugin(PluginMessageProc),
    /// No one: the host cannot name the plugin that asked — see
    /// [`PluginScintilla::owner`].
    Nobody,
}

thread_local! {
    /// The widgets [`create_plugin_scintilla`] made, in the order made.
    /// Append-only, like [`PLUGIN_SCIS`], and main-thread only.
    static PLUGIN_SCINTILLAS: RefCell<Vec<PluginScintilla>> =
        const { RefCell::new(Vec::new()) };
    /// The label of every command the loaded plugins publish, by command
    /// id — the tooltip of a toolbar button for one of them, and its
    /// name in the toolbar's overflow menu. Filled with the menu-check
    /// record and for the same reason: a toolbar button is added from
    /// inside the NPPM dispatch's borrow, where the plugins' `FuncItem`s
    /// cannot be read.
    static COMMAND_LABELS: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new());
}

/// `NPPM_CREATESCINTILLAHANDLE` on this backend: make a Scintilla widget
/// for the plugin that asked, and return its handle, or null.
///
/// `parent` is the `GtkContainer*` to put it in; it goes in with the
/// container's plain `add`, so in a `GtkBox` it does not expand unless the
/// plugin sets `hexpand` / `vexpand` on it or repacks it. It goes in
/// **hidden**, for the plugin to show — the Win32 host creates its control
/// without `WS_VISIBLE` for the same reason. (Win32 also makes it zero
/// sized; here the container decides the size, and Scintilla asks for
/// next to nothing.) The npp handle as `parent` makes a widget in no
/// container, which a plugin can drive through `SCI_*` alone — a text
/// buffer with Scintilla's search and styling — or add to a container of
/// its own later.
///
/// The widget is the plugin's, to destroy with its container or on its
/// own, as a Win32 plugin destroys its control. Once it is destroyed the
/// host routes nothing to it: `SCI_*` to its handle answers 0, as
/// `SendMessage` to a destroyed window does, and its notifications stop
/// ([`retire_plugin_scintilla`]). Its direct-call pair
/// (`SCI_GETDIRECTFUNCTION`) is not to be used after that, as on Windows.
///
/// The host keeps a reference to the widget that it never gives up, and
/// to Scintilla's own children, so a destroyed widget frees nothing
/// Scintilla still points at. Its dispose unparents its scrollbars, whose
/// adjustments it keeps raw pointers to, and work it has already queued —
/// the scrollbar update after an edit — runs after the dispose. Without
/// the references that writes into whatever the host's heap has put in
/// the freed memory; with them the object stays whole, if defunct, and
/// its handle never comes to name another object. That bounded leak is
/// why the number is capped ([`may_make_plugin_scintilla`], from
/// `codepp_plugin_host`): a destroyed widget still counts, so a plugin
/// that makes one per dialog it opens runs out, and reusing one is the
/// way.
///
/// Refused, besides null: see [`plugin_scintilla_parent`].
///
/// Its notifications go to the plugin's `messageProc` — see
/// [`forward_plugin_sci_notify`] — after the host's own housekeeping for
/// it: the wheel-overscroll clamp the host's own view gets. The widget is
/// charged to the plugin that asked, wherever it asked from — a GTK
/// signal handler of its own included, since its message comes in by its
/// own route ([`codepp_plugin_host::calling_plugin`]). Only one the host
/// cannot name — beyond the routed plugins, asking from outside any call
/// the host made — is charged to no plugin, and its notifications reach
/// none.
pub(crate) fn create_plugin_scintilla(parent: *mut c_void) -> *mut c_void {
    let owner = codepp_plugin_host::calling_plugin();
    let into = match plugin_scintilla_parent(parent) {
        Ok(into) => into,
        Err(why) => {
            tracing::warn!(why, "NPPM_CREATESCINTILLAHANDLE: refused");
            return std::ptr::null_mut();
        }
    };
    let Some(made) = PLUGIN_SCINTILLAS.with(|made| {
        made.try_borrow()
            .ok()
            .map(|made| made.iter().map(|s| s.owner).collect::<Vec<_>>())
    }) else {
        tracing::warn!("NPPM_CREATESCINTILLAHANDLE: refused: the host is already making one");
        return std::ptr::null_mut();
    };
    if let Err(why) = may_make_plugin_scintilla(&made, owner) {
        tracing::warn!(why, plugin = owner, "NPPM_CREATESCINTILLAHANDLE: refused");
        return std::ptr::null_mut();
    }
    // SAFETY: the NPPM dispatch runs only on the UI thread, after
    // `gtk::init` — `scintilla_new`'s preconditions.
    let ptr = unsafe { scintilla_new() };
    if ptr.is_null() {
        tracing::warn!("NPPM_CREATESCINTILLAHANDLE: scintilla_new() returned null");
        return std::ptr::null_mut();
    }
    // The host's own reference, never given up: `scintilla_new` hands back
    // a floating one, and sinking it makes it the host's, so neither the
    // plugin nor any container can drop the last one.
    //
    // SAFETY: a live `GObject` just made.
    unsafe { glib::gobject_ffi::g_object_ref_sink(ptr.cast()) };
    // Scintilla's own children too — see the doc above. A
    // `ScintillaObject` is a `GtkContainer` whose children are all
    // internal, so `forall` (internals included) is what reaches them.
    //
    // SAFETY: the live widget just made; borrowed.
    let parts: Borrowed<gtk::Container> =
        unsafe { from_glib_borrow(ptr.cast::<gtk::ffi::GtkContainer>()) };
    let mut internals = Vec::new();
    // No callback boundary: `forall` runs the closure inside this call,
    // and a push cannot unwind.
    parts.forall(|child| internals.push(child.clone()));
    // Its scrollbars, watched for the start of its dispose, below.
    let scrollbars: Vec<gtk::Widget> = internals
        .iter()
        .filter(|part| part.is::<gtk::Scrollbar>())
        .cloned()
        .collect();
    // Scintilla 5 is UTF-8 by default, but the host's text is UTF-8
    // throughout and the Win32 host sets it explicitly for the same
    // reason — so a future default cannot change what a plugin gets. The
    // host's own sends go through `EditorHandle` like every other message
    // the host sends a widget it made; the sends in this module are the
    // plugins' traffic.
    //
    // SAFETY: `ptr` is the live widget just made, never finalized — which
    // is also what keeps the handle sound for as long as the table below
    // holds it.
    let editor = unsafe { EditorHandle::from_gtk_widget(ptr) };
    if let Some(editor) = editor {
        editor.send(SCI_SETCODEPAGE, SC_CP_UTF8 as usize, 0);
    }
    // Recorded and made routable before it goes anywhere, so a widget the
    // host failed to record is never left in a plugin's container. Nothing
    // between the check above and this push can make another widget —
    // making one takes a plugin's call, and none runs in between — so
    // neither the borrow nor the routing slot can fail; were either ever
    // to, the plugin is told it got no widget.
    let Some(index) = PLUGIN_SCINTILLAS.with(|made| {
        let mut made = made.try_borrow_mut().ok()?;
        made.push(PluginScintilla {
            view: ptr,
            _internals: internals,
            editor,
            owner,
            target: if owner.is_some() {
                NotifyTarget::Unresolved
            } else {
                NotifyTarget::Nobody
            },
        });
        Some(made.len() - 1)
    }) else {
        tracing::error!("NPPM_CREATESCINTILLAHANDLE: the widget could not be recorded");
        return std::ptr::null_mut();
    };
    let Some(slot) = PLUGIN_SCIS.get(index) else {
        tracing::error!(
            index,
            "NPPM_CREATESCINTILLAHANDLE: no routing slot for the widget"
        );
        return std::ptr::null_mut();
    };
    slot.store(ptr, Ordering::Relaxed);
    PLUGIN_SCI_COUNT.store(index + 1, Ordering::Release);
    // SAFETY: the live widget just made, borrowed: no reference changes.
    let view: Borrowed<gtk::Widget> =
        unsafe { from_glib_borrow(ptr.cast::<gtk::ffi::GtkWidget>()) };
    connect_plugin_scintilla(index, &view, &scrollbars);
    if owner.is_none() {
        tracing::warn!(
            index,
            "NPPM_CREATESCINTILLAHANDLE: asked for by no plugin the host can name; \
             its notifications reach no plugin"
        );
    }
    if let Some(container) = into {
        container.add(&*view);
        if !view.is_ancestor(&*container) {
            // A container that takes children its own way (a `GtkPaned`
            // already full, say) declined it. The widget is made, recorded
            // and routable, so the plugin gets it and can place it itself.
            tracing::warn!(
                index,
                "NPPM_CREATESCINTILLAHANDLE: the parent declined the widget; it is in no container"
            );
        }
    }
    tracing::debug!(
        plugin = owner,
        index,
        "NPPM_CREATESCINTILLAHANDLE: made a widget"
    );
    ptr
}

/// Connect what the widget at `index` in [`PLUGIN_SCINTILLAS`] needs
/// from its signals: its notifications on to its plugin, and out of
/// routing as its dispose begins — see [`create_plugin_scintilla`].
/// `scrollbars` are its own, held by the host.
fn connect_plugin_scintilla(index: usize, view: &gtk::Widget, scrollbars: &[gtk::Widget]) {
    view.connect_local("sci-notify", false, move |values| {
        crate::at_callback_boundary("plugin:sci_notify", None, || {
            forward_plugin_sci_notify(index, values)
        })
    });
    // Out of routing as soon as the widget's dispose begins. Scintilla
    // unparents its scrollbars first, before GTK runs the container's
    // `remove` or emits the widget's `destroy`, and plugin code that runs
    // in between must find the widget gone. The scrollbars are parented
    // once, inside `scintilla_new`, so a scrollbar losing its parent is
    // that dispose; `destroy` is the fallback, should a later Scintilla
    // take itself apart in another order.
    for scrollbar in scrollbars {
        scrollbar.connect_parent_set(move |scrollbar, _| {
            crate::at_callback_boundary("plugin:sci_dispose", (), || {
                if scrollbar.parent().is_none() {
                    retire_plugin_scintilla(index);
                }
            });
        });
    }
    view.connect_destroy(move |_| {
        crate::at_callback_boundary("plugin:sci_destroy", (), || {
            retire_plugin_scintilla(index);
        });
    });
}

/// Where a widget made for a plugin goes: `Ok(None)` for the npp handle —
/// no container at all — or the container to add it to, or why `parent`
/// is refused.
///
/// Refused, besides null: anything `GObject` says is not a
/// `GtkContainer`; any Scintilla widget, the host's or a plugin's, live or
/// destroyed — a Scintilla has no room for anyone else's child; a widget
/// of the host's own,
/// which is anything in the main window or a floating dock window that
/// is not inside a plugin's docked panel; and a `GtkBin` that already
/// holds a child, which would turn the widget away. Between them the last
/// two refuse the host's wrapping around a plugin panel: the panel's own
/// widget is what to pass. When the dock cannot be asked — only from
/// inside its own layout pass — the parent is refused rather than guessed
/// at.
fn plugin_scintilla_parent(
    parent: *mut c_void,
) -> Result<Option<Borrowed<gtk::Container>>, &'static str> {
    if parent.is_null() {
        return Err("the parent is null");
    }
    if std::ptr::eq(parent, npp_sentinel()) {
        return Ok(None);
    }
    // SAFETY: not one of the host's non-object handles; by the ABI's
    // contract on this backend a parent is a live `GtkContainer`. What
    // happens when it is not is the limit in the section notes above.
    if !unsafe { is_instance_of(parent, gtk::Container::static_type()) } {
        return Err("the parent is not a GtkContainer");
    }
    // By type rather than through the routing table, which no longer
    // names a Scintilla widget once it is destroyed.
    //
    // SAFETY: as above.
    if unsafe { is_instance_of(parent, scintilla_type()) } {
        return Err("the parent is a Scintilla widget");
    }
    // SAFETY: a live `GtkContainer`, per the check above; borrowed, so a
    // floating container keeps its floating reference.
    let container: Borrowed<gtk::Container> =
        unsafe { from_glib_borrow(parent.cast::<gtk::ffi::GtkContainer>()) };
    match crate::dock::is_host_widget(container.upcast_ref()) {
        None => return Err("the dock could not be asked whether the parent is the host's"),
        Some(true) => return Err("the parent is one of the host's own widgets"),
        Some(false) => {}
    }
    if let Some(bin) = container.downcast_ref::<gtk::Bin>() {
        // SAFETY: a plain field read on the live container; the child,
        // if any, is not wrapped.
        if !unsafe { gtk::ffi::gtk_bin_get_child(bin.as_ptr()) }.is_null() {
            return Err("the parent is a GtkBin that already holds a widget");
        }
    }
    Ok(Some(container))
}

/// The `GType` of a Scintilla widget, for refusing one as a parent by
/// type.
fn scintilla_type() -> glib::Type {
    // SAFETY: registers the type on first use, and takes nothing.
    unsafe { glib::translate::from_glib(codepp_scintilla_sys::scintilla_object_get_type()) }
}

/// Scintilla's notifications from a widget made for a plugin, on to the
/// plugin that asked for it, at its `messageProc`, as `WM_NOTIFY` — where
/// a Win32 plugin's Scintilla child sends it to the plugin's dialog
/// procedure, which a widget does not have. The same generalisation the
/// `DMN_*` take. `lParam` is a copy of the `SCNotification` with
/// `nmhdr.hwndFrom` the widget's handle, which is how a plugin tells its
/// widgets apart; Scintilla's GTK backend already puts the widget there,
/// and it is set anyway, so the contract does not rest on that detail of
/// the vendored source. `wParam` is the widget's control identifier
/// (`SCI_SETIDENTIFIER`, 0 unless the plugin sets one), as a Win32
/// `WM_NOTIFY` carries.
///
/// `SCN_UPDATEUI` first gets the housekeeping the host's own view gets
/// on it, before the plugin hears of it: the wheel-overscroll clamp —
/// see `crate::clamp_horizontal_overscroll_of`.
fn forward_plugin_sci_notify(index: usize, values: &[glib::Value]) -> Option<glib::Value> {
    let payload = values.last()?;
    // SAFETY: the value belongs to a `sci-notify` emission, whose payload
    // Scintilla declares as a boxed `SCNotification*`; `g_value_get_boxed`
    // returns that pointer or null.
    let notification = unsafe { glib::gobject_ffi::g_value_get_boxed(payload.as_ptr()) }
        .cast::<SCNotification>()
        .cast_const();
    if notification.is_null() {
        return None;
    }
    let (view, editor) = plugin_scintilla_view(index)?;
    // SAFETY: a live `SCNotification` for this emission, which
    // `codepp_plugin_host::SCNotification` mirrors field for field. Copied,
    // so the host never writes into the emission's own.
    let mut scn = unsafe { notification.read() };
    if scn.nmhdr.code == SCN_UPDATEUI {
        if let Some(editor) = editor {
            // SAFETY: a widget made for a plugin, never finalized; borrowed.
            let widget: Borrowed<gtk::Widget> =
                unsafe { from_glib_borrow(view.cast::<gtk::ffi::GtkWidget>()) };
            crate::clamp_horizontal_overscroll_of(editor, &widget);
        }
    }
    let target = plugin_scintilla_target(index)?;
    scn.nmhdr.hwnd_from = view;
    // SAFETY: a loaded plugin's `messageProc` — plugins are never
    // unloaded — run as that plugin, on the UI thread. `scn` outlives the
    // call.
    let _ = unsafe { target.send(WM_NOTIFY, scn.nmhdr.id_from, &raw const scn as isize) };
    None
}

/// A widget made for a plugin is being destroyed: take it out of the
/// routing table, so `SCI_*` to its handle answers 0 from here on and its
/// notifications stop. Runs on the main thread as the widget's dispose
/// begins — when Scintilla unparents its first scrollbar — and again for
/// the second and from its `destroy`, later in the same dispose, which
/// then change nothing.
fn retire_plugin_scintilla(index: usize) {
    if let Some(slot) = PLUGIN_SCIS.get(index) {
        // Relaxed: a reader on another thread that misses this re-checks
        // on this thread, which wrote it, before it sends anything.
        let was = slot.swap(std::ptr::null_mut(), Ordering::Relaxed);
        if !was.is_null() {
            tracing::debug!(
                index,
                "a plugin's Scintilla widget is being destroyed; no longer routed"
            );
        }
    }
}

/// The widget at `index` in [`PLUGIN_SCINTILLAS`], with its handle —
/// `None` once it has been destroyed.
fn plugin_scintilla_view(index: usize) -> Option<(*mut c_void, Option<EditorHandle>)> {
    if PLUGIN_SCIS.get(index)?.load(Ordering::Relaxed).is_null() {
        return None;
    }
    PLUGIN_SCINTILLAS.with(|made| {
        let made = made.try_borrow().ok()?;
        let entry = made.get(index)?;
        Some((entry.view, entry.editor))
    })
}

/// The `messageProc` that hears the notifications of the widget at
/// `index`, looked up on first use and kept once found.
///
/// The borrow of [`PLUGIN_SCINTILLAS`] is never held across the lookup or
/// the plugin call, so a plugin that makes another widget from inside its
/// handler finds the registry free. A lookup that finds nothing is tried
/// again at the next notification: `with_state` declines one made from
/// inside a host borrow, and a plugin not yet loaded — one that made the
/// widget from its `setInfo` — has no `messageProc` to find until it is.
/// Every loaded plugin exports one; the loader refuses a plugin without.
fn plugin_scintilla_target(index: usize) -> Option<PluginMessageProc> {
    let (owner, known) = PLUGIN_SCINTILLAS.with(|made| {
        let made = made.try_borrow().ok()?;
        let entry = made.get(index)?;
        Some((entry.owner, entry.target))
    })?;
    match known {
        NotifyTarget::Plugin(target) => return Some(target),
        NotifyTarget::Nobody => return None,
        NotifyTarget::Unresolved => {}
    }
    let target =
        with_state(|st| owner.and_then(|owner| st.shell.plugin_message_target(owner))).flatten()?;
    PLUGIN_SCINTILLAS.with(|made| {
        if let Ok(mut made) = made.try_borrow_mut() {
            if let Some(entry) = made.get_mut(index) {
                entry.target = NotifyTarget::Plugin(target);
            }
        }
    });
    Some(target)
}

/// `NPPM_MODELESSDIALOG` on this backend: `true` — which the dispatcher
/// answers with the handle, as Notepad++ does — for a window a plugin
/// could have made, `false` for the handles that plainly are not one.
///
/// **What registering does here is make the dialog transient for the main
/// window, if it has no transient parent of its own.** What the Win32
/// registration buys a dialog is the host's message pump calling
/// `IsDialogMessage` for it — Tab moving between its controls, Enter
/// pressing its default button — and so keeping the host's accelerators
/// out of it. GTK does the first in every window, and the second has
/// nothing to keep out: the host's accelerators, its own and the plugins'
/// shortcuts, belong to the main window and fire only there. But a Win32
/// plugin makes its dialog with the npp handle as its owner, so it stays
/// above the editor and goes with it; a GTK plugin cannot do the same,
/// because there the npp handle is not a window. (It could find the main
/// window as the toplevel of its Scintilla handle, but that is GTK code a
/// ported plugin does not have.) Registration is where a Notepad++
/// plugin says it has a dialog, so it is where the host supplies that. A
/// dialog that already has a transient parent keeps it, and one that
/// wants none can unset it after registering.
///
/// A dialog is checked on the way in only. Its removal is answered
/// without looking at what the pointer points at: a plugin removes its
/// dialog on the way to destroying it, when the pointer is least
/// trustworthy, and there is nothing to undo — GTK drops the transient
/// link itself when either window is destroyed.
pub(crate) fn register_modeless_dialog(
    dlg: *mut c_void,
    register: bool,
    main_window: &gtk::Window,
) -> bool {
    if dlg.is_null() || std::ptr::eq(dlg, npp_sentinel()) || is_known_scintilla(dlg) {
        tracing::warn!(
            register,
            "NPPM_MODELESSDIALOG: refused: not a window's handle"
        );
        return false;
    }
    if !register {
        tracing::debug!("NPPM_MODELESSDIALOG: removed (nothing was routed on this backend)");
        return true;
    }
    // SAFETY: not one of the host's non-object handles; by the ABI's
    // contract on this backend the dialog is a live `GtkWindow`.
    if !unsafe { is_instance_of(dlg, gtk::Window::static_type()) } {
        tracing::warn!("NPPM_MODELESSDIALOG: refused: not a GtkWindow");
        return false;
    }
    // SAFETY: a live `GtkWindow`, per the check above; borrowed.
    let window: Borrowed<gtk::Window> =
        unsafe { from_glib_borrow(dlg.cast::<gtk::ffi::GtkWindow>()) };
    match crate::dock::is_host_window(window.upcast_ref()) {
        Some(false) => {}
        Some(true) => {
            tracing::warn!("NPPM_MODELESSDIALOG: refused: one of the host's own windows");
            return false;
        }
        None => {
            tracing::warn!(
                "NPPM_MODELESSDIALOG: refused: the dock could not be asked whether it is the host's"
            );
            return false;
        }
    }
    if window.transient_for().is_none() {
        window.set_transient_for(Some(main_window));
    }
    tracing::debug!("NPPM_MODELESSDIALOG: registered");
    true
}

/// `NPPM_ADDTOOLBARICON` on this backend: a toolbar button that runs the
/// plugin command `cmd_id`, showing `icon` — a `GdkPixbuf*`, of which the
/// host draws a copy, so the plugin may drop its reference once this
/// returns. The button's tooltip is the command's menu label, and it shows
/// the command's check mark (`NPPM_SETMENUITEMCHECK`) as pressed, as
/// Notepad++'s toolbar does.
///
/// Refused for an id that is not a command a loaded plugin published: the
/// button runs its command through the plugin commands' own path, which
/// knows no other. Asking again for the same command replaces the image
/// rather than adding a second button.
pub(crate) fn add_toolbar_icon(toolbar: &gtk::Toolbar, cmd_id: i32, icon: *mut c_void) -> bool {
    if icon.is_null() || std::ptr::eq(icon, npp_sentinel()) || is_known_scintilla(icon) {
        tracing::warn!(
            cmd_id,
            "NPPM_ADDTOOLBARICON: refused: not an image's handle"
        );
        return false;
    }
    let label = COMMAND_LABELS.with(|labels| {
        labels
            .try_borrow()
            .ok()
            .and_then(|labels| labels.get(&cmd_id).cloned())
    });
    let Some(label) = label else {
        tracing::warn!(
            cmd_id,
            "NPPM_ADDTOOLBARICON: refused: no loaded plugin publishes that command"
        );
        return false;
    };
    // SAFETY: not one of the host's non-object handles; by the ABI's
    // contract on this backend the icon is a live `GdkPixbuf`.
    if !unsafe { is_instance_of(icon, Pixbuf::static_type()) } {
        tracing::warn!(cmd_id, "NPPM_ADDTOOLBARICON: refused: not a GdkPixbuf");
        return false;
    }
    // SAFETY: a live `GdkPixbuf`, per the check above, which is never
    // floating; `from_glib_none` takes the host's own reference.
    let pixbuf: Pixbuf = unsafe { from_glib_none(icon.cast::<gtk::gdk_pixbuf::ffi::GdkPixbuf>()) };
    match crate::toolbar::add_plugin_button(toolbar, cmd_id, &pixbuf, &label, menu_mark(cmd_id)) {
        Ok(()) => true,
        Err(why) => {
            tracing::warn!(cmd_id, why, "NPPM_ADDTOOLBARICON: refused");
            false
        }
    }
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
    // command id (`PluginMenuChecks`) and painted on every rebuild, and
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

/// The Plugins-menu item currently built for one command, with what it
/// was built from — enough to build it again as a check item in place,
/// the first time its plugin ticks it while the menu is up.
struct LiveItem {
    item: gtk::MenuItem,
    label: String,
    chord: Option<Chord>,
}

thread_local! {
    /// The plugins' marks, painted on every rebuild — see
    /// [`codepp_plugin_host::PluginMenuChecks`].
    static PLUGIN_CHECKS: std::cell::RefCell<codepp_plugin_host::PluginMenuChecks> =
        std::cell::RefCell::new(codepp_plugin_host::PluginMenuChecks::default());
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
/// `NPPM_SETMENUITEMCHECK`, and show it on the item if the menu is up and
/// on the command's toolbar button if it has one — Notepad++ marks both.
/// `false` when `cmd_id` is none of the loaded plugins' commands.
///
/// Called from inside the dispatch's state borrow, so it touches only
/// this module's own state, the toolbar's, and their widgets; the item's
/// `activate` handler returns at once while [`SYNCING_CHECKS`] is set,
/// and the button's `toggled` while the toolbar's own flag is.
pub(crate) fn set_menu_check(cmd_id: i32, checked: bool) -> bool {
    if !PLUGIN_CHECKS.with(|c| c.borrow_mut().set(cmd_id, checked)) {
        tracing::trace!(
            cmd_id,
            "NPPM_SETMENUITEMCHECK: no loaded plugin's command has that id on this backend"
        );
        return false;
    }
    show_recorded_mark(cmd_id);
    crate::toolbar::set_plugin_button_state(cmd_id, checked);
    true
}

/// Take in the commands every loaded plugin publishes — see
/// [`codepp_plugin_host::PluginMenuChecks::absorb`] — and their labels,
/// which a toolbar button for one of them shows ([`add_toolbar_icon`]).
/// Run after each load pass and **before** its notifications, so a plugin
/// ticking an item or adding a toolbar button from `NPPN_TBMODIFICATION`
/// or `NPPN_READY` finds its commands known, as Notepad++ has them
/// installed by then.
fn absorb_loaded_commands() {
    // Both records are thread-locals of their own and call into nothing,
    // so they are filled from under the state borrow rather than from a
    // copy.
    with_state(|st| {
        let funcs = st.shell.loaded_plugin_funcs().flat_map(|(_, funcs)| funcs);
        PLUGIN_CHECKS.with(|c| c.borrow_mut().absorb(funcs));
        COMMAND_LABELS.with(|labels| {
            let mut labels = labels.borrow_mut();
            for f in st.shell.loaded_plugin_funcs().flat_map(|(_, funcs)| funcs) {
                if f.is_command() {
                    labels.insert(f.cmd_id, funcitem_label(f));
                }
            }
        });
    });
}

/// The mark recorded for plugin command `cmd_id` — what its toolbar
/// button shows as pressed. `false` for a command the plugin has never
/// ticked, and when the record is being written.
pub(crate) fn menu_mark(cmd_id: i32) -> bool {
    PLUGIN_CHECKS
        .with(|c| c.try_borrow().ok().and_then(|c| c.get(cmd_id)))
        .unwrap_or(false)
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
                            f.is_command(),
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

/// Invoke a plugin's menu command — from its menu item, its shortcut or
/// its toolbar button. Looks the function pointer up (a short
/// `with_state` borrow), drops the borrow, then calls the plugin outside
/// it — so the plugin's re-entrant `NPPM_*` calls get a fresh borrow —
/// at a [`crate::at_callback_boundary`] (a panic must not cross the C
/// frame).
pub(crate) fn on_plugin_command(cmd_id: i32) {
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

/// Source guards for what a plugin can make the host make. Each pins a
/// property whose failure has no symptom a runtime test here could see:
/// a plugin's widget routed without the identity check faults inside
/// vendored C++ on a bad pointer rather than failing an assertion, and a
/// cap applied in the wrong place is a leak that grows only as a plugin
/// keeps asking.
#[cfg(test)]
mod made_for_plugins_guards {
    use crate::source_scan::{
        block_after, code_only, fn_body, occurs_at_depth_one, strip_test_modules,
    };

    fn plugin_src() -> String {
        strip_test_modules(&code_only(include_str!("plugin.rs")))
    }

    /// A plugin's `SendMessageW` reaches Scintilla only for a widget the
    /// host made — its own, or one made for a plugin — and only from the
    /// two places that check: `plugin_dispatch` and the marshal it hands a
    /// worker thread's message to. `scintilla_send_message` dereferences
    /// its first argument, so an unchecked send would turn a plugin's bad
    /// pointer into a fault where Win32 answers 0.
    #[test]
    fn scintilla_is_reached_only_through_the_identity_check() {
        let src = plugin_src();
        let dispatch = fn_body(&src, "plugin_dispatch");
        let marshal = fn_body(&src, "send_sci_on_main");
        assert_eq!(
            src.matches("scintilla_send_message(").count(),
            2,
            "plugin.rs sends to Scintilla from somewhere other than `plugin_dispatch` \
             and `send_sci_on_main`; every send must sit behind the identity check"
        );
        assert_eq!(dispatch.matches("scintilla_send_message(").count(), 1);
        assert_eq!(marshal.matches("scintilla_send_message(").count(), 1);
        assert_eq!(
            src.matches("send_sci_on_main(").count(),
            2,
            "`send_sci_on_main` is called from somewhere other than `plugin_dispatch`"
        );
        let check = dispatch
            .find("} else if is_known_scintilla(hwnd) {")
            .expect("`plugin_dispatch` no longer identity-checks the handle it was given");
        let send = dispatch
            .find("scintilla_send_message(")
            .expect("`plugin_dispatch` no longer forwards to Scintilla at all");
        assert!(check < send, "the identity check must come before the send");
        // And again on the main thread, inside the hop: a plugin's widget
        // can be destroyed between the calling thread's check and the
        // send.
        let recheck = marshal
            .find("let result = if is_known_scintilla(ptr.0) {")
            .expect("the marshal no longer re-checks the handle on the main thread");
        let hop_send = marshal
            .find("scintilla_send_message(")
            .expect("the marshal no longer sends to Scintilla");
        assert!(
            recheck < hop_send,
            "the main-thread re-check must come before the send"
        );
        let known = fn_body(&src, "is_known_scintilla");
        assert!(
            known.contains("is_valid_scintilla(hwnd) || is_plugin_scintilla(hwnd)")
                && known.matches("||").count() == 1,
            "`is_known_scintilla` must be exactly the host's widget or a plugin's"
        );
        let plugin_widgets = fn_body(&src, "is_plugin_scintilla");
        assert!(
            plugin_widgets.contains(".take(made)")
                && plugin_widgets.contains("PLUGIN_SCI_COUNT.load(Ordering::Acquire)"),
            "`is_plugin_scintilla` must read only the slots the count has published"
        );
    }

    /// The quota on plugin Scintilla widgets is applied in
    /// `create_plugin_scintilla`, unconditionally, before a widget is
    /// made — the twin of `ui_cocoa`'s guard of the same name, whose
    /// comment gives the reason: the rule lives in `codepp_plugin_host`,
    /// where nothing warns if its use is neutered, and each widget is kept
    /// for the rest of the process.
    #[test]
    fn the_plugin_view_quota_is_applied_before_a_view_is_made() {
        let src = plugin_src();
        let create = fn_body(&src, "create_plugin_scintilla");
        let refusal = "if let Err(why) = may_make_plugin_scintilla(&made, owner) {";
        // Every widget's owner, collected whole: nothing chained after the
        // `collect` may shorten the list. Checked as "no `.` next" rather
        // than by the closing parentheses, so rustfmt's layout of the
        // closure does not matter.
        let counted = "made.iter().map(|s| s.owner).collect::<Vec<_>>()";
        let snapshot = create
            .find(counted)
            .expect("the quota no longer counts every widget made so far");
        assert!(
            !create[snapshot + counted.len()..]
                .trim_start()
                .starts_with('.'),
            "the list of widgets the quota counts is cut short after it is collected"
        );
        let check = create
            .find(refusal)
            .expect("`create_plugin_scintilla` no longer refuses on the quota");
        let make = create
            .find("scintilla_new()")
            .expect("`create_plugin_scintilla` no longer makes a widget");
        assert!(
            snapshot < check && check < make,
            "the quota must count every widget and refuse before a widget is made"
        );
        assert!(
            !create[snapshot..check].contains("let "),
            "something is rebound between counting the widgets and consulting the quota"
        );
        assert!(
            create.matches("let owner").count() == 1
                && create.contains("let owner = codepp_plugin_host::calling_plugin();"),
            "`owner` is no longer the calling plugin, bound once"
        );
        assert!(
            occurs_at_depth_one(&create, refusal),
            "the quota is applied only under a condition, so some widgets go unchecked"
        );
        assert!(
            occurs_at_depth_one(
                &block_after(&create, refusal),
                "return std::ptr::null_mut();"
            ),
            "a refusal by the quota no longer stops the widget being made"
        );
        // Recorded under the plugin that asked: the shorthand `owner`, not
        // an `owner: …` of anything else. Whitespace dropped, so rustfmt's
        // layout of the literal does not matter.
        let recorded = block_after(&create, "made.push(PluginScintilla {")
            .split_whitespace()
            .collect::<String>();
        assert!(
            recorded.contains("owner,") && !recorded.contains("owner:"),
            "a widget is no longer recorded under the plugin that asked for it, so \
             the per-plugin allowance counts nobody"
        );
    }

    /// A plugin's widget gets the host view's wheel-overscroll clamp on
    /// each `SCN_UPDATEUI`, before the plugin hears of it, so the plugin
    /// reads the offset settled — the order the host's own view keeps its
    /// housekeeping in.
    #[test]
    fn a_plugin_widget_is_clamped_before_its_plugin_is_told() {
        let src = plugin_src();
        let forward = fn_body(&src, "forward_plugin_sci_notify");
        let update = forward
            .find("if scn.nmhdr.code == SCN_UPDATEUI {")
            .expect("a plugin widget's SCN_UPDATEUI is no longer recognised");
        let clamp = forward
            .find("crate::clamp_horizontal_overscroll_of(")
            .expect("a plugin widget no longer gets the overscroll clamp");
        let tell = forward
            .find("target.send(WM_NOTIFY,")
            .expect("a plugin widget's notifications no longer reach its plugin");
        assert!(
            update < clamp && clamp < tell,
            "the clamp must run on SCN_UPDATEUI, before the plugin is told"
        );
    }

    /// A destroyed widget frees nothing Scintilla still points at, and is
    /// out of routing as its dispose begins — what keeps a plugin's
    /// destroyed widget from being a use-after-free in the host's heap.
    /// The display scenario shows it; this pins it where CI runs, which
    /// has no display.
    #[test]
    fn a_destroyed_widget_is_held_whole_and_taken_out_of_routing() {
        let src = plugin_src();
        let create = fn_body(&src, "create_plugin_scintilla");
        assert!(
            occurs_at_depth_one(
                &create,
                "parts.forall(|child| internals.push(child.clone()));"
            ),
            "Scintilla's own children are no longer held, or only under a condition"
        );
        let recorded = block_after(&create, "made.push(PluginScintilla {")
            .split_whitespace()
            .collect::<String>();
        assert!(
            recorded.contains("_internals:internals,"),
            "the held children are not kept with the widget"
        );
        assert!(
            occurs_at_depth_one(
                &create,
                "connect_plugin_scintilla(index, &view, &scrollbars);"
            ),
            "a widget's signals are no longer connected, or only under a condition"
        );
        let connect = fn_body(&src, "connect_plugin_scintilla");
        assert!(
            occurs_at_depth_one(&connect, "for scrollbar in scrollbars {"),
            "the scrollbars are no longer watched for the dispose"
        );
        for (what, hook) in [
            (
                "the start of its dispose",
                "scrollbar.connect_parent_set(move |scrollbar, _| {",
            ),
            ("its destroy", "view.connect_destroy(move |_| {"),
        ] {
            assert!(
                block_after(&connect, hook).contains("retire_plugin_scintilla(index)"),
                "the widget is no longer taken out of routing at {what}"
            );
        }
        assert!(
            block_after(
                &connect,
                "scrollbar.connect_parent_set(move |scrollbar, _| {"
            )
            .contains("if scrollbar.parent().is_none() {"),
            "the widget is taken out of routing whenever a scrollbar's parent changes, \
             not when its dispose takes the scrollbar away"
        );
        let retire = fn_body(&src, "retire_plugin_scintilla");
        assert!(
            retire.contains("slot.swap(std::ptr::null_mut(),")
                && retire.matches(".swap(").count() == 1
                && !retire.contains(".store("),
            "`retire_plugin_scintilla` does something besides clear the slot"
        );
        assert!(
            src.matches("slot.store(").count() == 1 && src.matches("slot.swap(").count() == 1,
            "a routing slot is written somewhere besides where a widget is made and where \
             it is cleared"
        );
    }

    /// Nothing that handles a widget a plugin passed in reads it through
    /// a gtk-rs getter that returns an owned wrapper — `parent()`,
    /// `toplevel()`. Those wrap the result with `from_glib_none`, which
    /// takes over a floating reference, so a container the plugin made and
    /// has not sunk would be finalized when the wrapper dropped: a
    /// use-after-free in the plugin, caused by a check meant to protect it.
    #[test]
    fn plugin_widgets_are_never_read_through_owning_getters() {
        let src = plugin_src();
        for name in ["register_dock_dialog", "plugin_scintilla_parent"] {
            let body = fn_body(&src, name);
            for getter in [
                ".parent()",
                ".toplevel()",
                ".transient_for()",
                ".children()",
                ".child()",
                ".ancestor(",
            ] {
                assert!(
                    !body.contains(getter),
                    "`{name}` reads a plugin's widget through `{getter}`, whose owned \
                     result can finalize a floating container"
                );
            }
        }
        let dock = strip_test_modules(&code_only(include_str!("dock.rs")));
        for name in ["is_host_widget", "is_host_window"] {
            let body = fn_body(&dock, name);
            for getter in [
                ".parent()",
                ".toplevel()",
                ".children()",
                ".child()",
                ".ancestor(",
            ] {
                assert!(
                    !body.contains(getter),
                    "`{name}` reads a plugin's widget through `{getter}`"
                );
            }
        }
    }
}

/// What a plugin asks the host to make — `NPPM_CREATESCINTILLAHANDLE`,
/// `NPPM_MODELESSDIALOG`, `NPPM_ADDTOOLBARICON` — driven against real GTK
/// objects, with a bare dock standing in for the main window's.
///
/// What it pins is what a source scan cannot see: a widget made for a
/// plugin lands where the plugin asked, hidden, and answers `SCI_*`
/// routed to it from any thread; once the plugin destroys it, nothing of
/// the host's is written to and it answers 0; the parents, windows and
/// images the host must not take are refused, and a floating container
/// the plugin has not sunk survives being checked; a registered dialog
/// becomes transient for the main window unless it has a transient parent
/// of its own; and a plugin's toolbar button runs its command once a
/// click and comes back showing the plugin's mark rather than GTK's
/// toggle. Delivery of a plugin widget's
/// notifications needs a loaded plugin to deliver to, so the real app is
/// where that is shown (DESIGN.md §7.4).
///
/// Display-gated, driven by `crate::display_tests`, which owns the
/// invocation and explains why these cannot be `#[test]`s of their own.
#[cfg(test)]
pub(crate) mod host_made_tests {
    use std::cell::Cell;
    use std::ffi::{c_void, CString};
    use std::time::Duration;

    use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
    use gtk::glib;
    use gtk::glib::translate::{from_glib_borrow, from_glib_none, Borrowed};
    use gtk::prelude::*;

    use codepp_plugin_host::{
        calling_plugin, plugin_route, CallingPlugin, FuncItem, MENU_TITLE_LENGTH,
    };
    use codepp_scintilla_sys::{
        scintilla_new, SCI_GETCODEPAGE, SCI_GETLENGTH, SCI_SETTEXT, SC_CP_UTF8,
    };

    use std::sync::atomic::Ordering;

    use super::{
        add_toolbar_icon, create_plugin_scintilla, npp_sentinel, plugin_dispatch,
        register_modeless_dialog, set_menu_check, COMMAND_LABELS, HOPS_QUEUED, MAIN_THREAD,
        PLUGIN_CHECKS, PLUGIN_SCINTILLAS, VALID_SCI,
    };
    use crate::dock::departure_tests::{
        dock_panel_back, finish, float_panel, install_bare_dock, open, plugin_widget, pump,
        register, rig_editor_cell, rig_hint_window, rig_window, with_the_dock_busy,
    };
    use crate::toolbar::{plugin_icon_image, COMMANDS_RUN};

    /// Stand-ins for the plugin commands toolbar buttons run.
    const SMOKE_CMD: i32 = 52_001;
    const SMOKE_CMD_2: i32 = 52_002;

    /// A widget's handle carried to a worker thread.
    struct WorkerPtr(*mut c_void);
    // SAFETY: a widget the host never finalizes; the worker hands it only
    // to `plugin_dispatch`, the code under test.
    unsafe impl Send for WorkerPtr {}

    /// `SCI_GETLENGTH` sent to `made` from a thread of its own, the
    /// thread handed back while its message waits for the main loop.
    fn length_from_a_worker(made: *mut c_void) -> std::thread::JoinHandle<isize> {
        let queued = HOPS_QUEUED.load(Ordering::SeqCst);
        let handle = WorkerPtr(made);
        let worker = std::thread::spawn(move || {
            let handle = handle;
            plugin_dispatch(handle.0, SCI_GETLENGTH, 0, 0)
        });
        // Until the worker has passed its own check and handed its message
        // to the main loop, which runs only when the test turns it. A
        // regressed direct call never gets here, and the wait fails.
        let mut waits = 0;
        while HOPS_QUEUED.load(Ordering::SeqCst) == queued {
            std::thread::sleep(Duration::from_millis(1));
            waits += 1;
            assert!(
                waits < 5_000,
                "the worker's SCI_* never reached the main loop: was it sent off the main thread?"
            );
        }
        assert!(
            !worker.is_finished(),
            "a cross-thread SCI_* to a plugin's widget ran off the main thread"
        );
        worker
    }

    /// Run the main loop until `worker` is done.
    fn until_finished(worker: &std::thread::JoinHandle<isize>) {
        let mut spins = 0;
        while !worker.is_finished() {
            if !gtk::main_iteration_do(false) {
                std::thread::sleep(Duration::from_millis(1));
            }
            spins += 1;
            assert!(spins < 10_000, "the marshaled SCI_* never completed");
        }
    }

    /// The router a plugin's route hands to in the scenario: what the
    /// host's dispatch does with `NPPM_CREATESCINTILLAHANDLE` — make a
    /// widget with `lparam` as its parent — and nothing else. Nothing in
    /// here may panic: it is `extern "C"`.
    unsafe extern "C" fn make_through_a_route(
        _hwnd: *mut c_void,
        _msg: u32,
        _wparam: usize,
        lparam: isize,
    ) -> isize {
        create_plugin_scintilla(lparam as *mut c_void) as isize
    }

    /// A plugin's handle for `object`: its address.
    fn handle_of(object: &impl IsA<glib::Object>) -> *mut c_void {
        object.upcast_ref::<glib::Object>().as_ptr().cast()
    }

    /// `text` into the widget at `handle`, then its length back — both
    /// through the router, as a plugin's route would hand them on.
    fn round_trip(handle: *mut c_void, text: &str) -> isize {
        let text = CString::new(text).expect("no interior NUL");
        plugin_dispatch(handle, SCI_SETTEXT, 0, text.as_ptr() as isize);
        plugin_dispatch(handle, SCI_GETLENGTH, 0, 0)
    }

    /// The container a widget the host made sits in, as a raw pointer.
    fn parent_of(handle: *mut c_void) -> *mut c_void {
        // SAFETY: a widget the host made, never finalized; a field read.
        unsafe { gtk::ffi::gtk_widget_get_parent(handle.cast()) }.cast()
    }

    /// A widget the host made, borrowed.
    fn widget_at(handle: *mut c_void) -> Borrowed<gtk::Widget> {
        // SAFETY: a widget the host made, never finalized; borrowed.
        unsafe { from_glib_borrow(handle.cast::<gtk::ffi::GtkWidget>()) }
    }

    pub(crate) fn what_plugins_ask_the_host_to_make() {
        install_bare_dock();
        let main = rig_window();
        // SAFETY: GTK is initialised — `scintilla_new`'s precondition.
        let host_sci = unsafe { scintilla_new() };
        assert!(!host_sci.is_null(), "scintilla_new returned null");
        // The host's stand-in view, kept for the process like the app's.
        //
        // SAFETY: a live, floating `GObject` just made.
        unsafe { glib::gobject_ffi::g_object_ref_sink(host_sci.cast()) };
        // Stand in for `discover`, which arms both of these at startup.
        VALID_SCI.store(host_sci, std::sync::atomic::Ordering::Release);
        let _ = MAIN_THREAD.set(std::thread::current().id());

        scintilla_parents_are_checked(&main, host_sci);
        widgets_are_charged_to_the_plugin_that_asked();
        a_container_that_turns_the_widget_away_leaves_it_routed();
        a_refused_dock_registration_leaves_a_floating_parent_alone();
        a_plugin_panel_is_a_parent_and_its_wrapping_is_not(&main);
        a_plugin_widget_is_routed_from_any_thread();
        a_widget_moved_about_stays_routed();
        a_destroyed_widget_is_routed_nothing();
        modeless_dialogs_are_checked_and_made_transient(&main, host_sci);
        plugin_toolbar_buttons();
        plugin_icons_are_drawn_at_the_screens_pixels();
    }

    /// Where a widget may go, and where it may not.
    fn scintilla_parents_are_checked(main: &gtk::Window, host_sci: *mut c_void) {
        let ask = create_plugin_scintilla;
        let not_a_widget = gtk::Adjustment::new(0.0, 0.0, 1.0, 0.1, 0.1, 0.1);
        let not_a_container = gtk::Label::new(None);
        let full_bin = gtk::Frame::new(None);
        full_bin.add(&gtk::Label::new(None));
        for (what, parent) in [
            ("null", std::ptr::null_mut()),
            ("the host's own Scintilla widget", host_sci),
            ("a GObject that is no widget", handle_of(&not_a_widget)),
            ("a widget that is no container", handle_of(&not_a_container)),
            ("the main window", handle_of(main)),
            (
                "a widget of the host's, in the main window",
                handle_of(&rig_editor_cell()),
            ),
            (
                "another window of the host's",
                handle_of(&rig_hint_window()),
            ),
            ("a GtkBin that holds a widget already", handle_of(&full_bin)),
        ] {
            assert!(ask(parent).is_null(), "{what} was accepted as a parent");
        }

        // The npp handle: a widget in no container at all.
        let detached = ask(npp_sentinel());
        assert!(
            !detached.is_null(),
            "the npp handle as parent made no widget"
        );
        assert!(
            parent_of(detached).is_null(),
            "a detached widget was put somewhere"
        );
        assert_eq!(
            round_trip(detached, "abc"),
            3,
            "the detached widget is not routed"
        );
        assert!(
            ask(detached).is_null(),
            "a plugin's own Scintilla widget was accepted as a parent"
        );

        // A container the plugin made and never sank, as a C plugin makes
        // one: the widget goes in, hidden and UTF-8 — and checking the
        // parent took nothing from the plugin: the container is still
        // floating, and still alive.
        //
        // SAFETY: GTK is initialised; this is the plugin's own box.
        let raw_box = unsafe { gtk::ffi::gtk_box_new(gtk::ffi::GTK_ORIENTATION_VERTICAL, 0) };
        let mut alive: glib::ffi::gpointer = raw_box.cast();
        // SAFETY: a live object; GLib nulls `alive` if it is finalized.
        unsafe { glib::gobject_ffi::g_object_add_weak_pointer(raw_box.cast(), &raw mut alive) };
        let made = ask(raw_box.cast());
        assert!(
            !made.is_null(),
            "a plugin's free-standing container was refused"
        );
        assert!(
            !alive.is_null(),
            "checking the parent finalized the plugin's container"
        );
        assert_ne!(
            // SAFETY: still alive, per the weak pointer.
            unsafe { glib::gobject_ffi::g_object_is_floating(raw_box.cast()) },
            glib::ffi::GFALSE,
            "checking the parent took the plugin's floating reference"
        );
        assert_eq!(
            parent_of(made),
            raw_box.cast::<c_void>(),
            "the widget is not in the parent"
        );
        assert!(
            !widget_at(made).is_visible(),
            "a new widget is shown before the plugin shows it"
        );
        assert_eq!(
            plugin_dispatch(made, SCI_GETCODEPAGE, 0, 0),
            SC_CP_UTF8 as isize,
            "a new widget is not UTF-8"
        );
        // From here the plugin keeps its container, as a plugin would.
        //
        // SAFETY: alive and floating; sinking makes the reference the
        // test's, which it never gives up.
        unsafe {
            glib::gobject_ffi::g_object_remove_weak_pointer(raw_box.cast(), &raw mut alive);
            glib::gobject_ffi::g_object_ref_sink(raw_box.cast());
        }

        // While the dock cannot be asked whether a parent is the host's —
        // from inside its own layout pass — none is taken.
        let plugins_own = gtk::Box::new(gtk::Orientation::Vertical, 0);
        assert!(
            with_the_dock_busy(|| ask(handle_of(&plugins_own))).is_null(),
            "a parent was taken while the dock could not be asked about it"
        );
        assert!(
            !ask(handle_of(&plugins_own)).is_null(),
            "the same parent was refused once the dock was free"
        );
    }

    /// Which plugin a widget is charged to: the one that asked, whether
    /// the host was calling it at the time or it asked through its own
    /// route with no call of the host's under way.
    fn widgets_are_charged_to_the_plugin_that_asked() {
        // Made inside a call to a plugin, a widget is charged to it.
        let owned = {
            let _calling = CallingPlugin::enter(7);
            create_plugin_scintilla(npp_sentinel())
        };
        assert!(!owned.is_null());
        assert_eq!(
            PLUGIN_SCINTILLAS.with(|made| made.borrow().last().map(|s| s.owner)),
            Some(Some(7)),
            "the widget is not charged to the plugin that asked for it"
        );

        // Asked for through a plugin's own route, with no call of the
        // host's under way — as from a signal handler or timer of the
        // plugin's own — a widget is charged to that plugin all the same:
        // the route marks it.
        assert_eq!(
            calling_plugin(),
            None,
            "the scenario must ask with no host call marking a plugin"
        );
        let route = plugin_route(9, make_through_a_route);
        // SAFETY: a route to `make_through_a_route`, which reads no
        // pointer: the npp handle goes on as the parent, by identity.
        let routed = unsafe { route(npp_sentinel(), 0, 0, npp_sentinel() as isize) };
        assert_ne!(routed, 0, "no widget was made through the route");
        assert_eq!(
            PLUGIN_SCINTILLAS.with(|made| made.borrow().last().map(|s| s.owner)),
            Some(Some(9)),
            "a widget asked for through a plugin's route is not charged to that plugin"
        );
    }

    /// A plugin registering as its dock panel a widget already inside a
    /// container it made and never sank is refused — and the refusal takes
    /// nothing from the plugin: the container is still floating, and alive.
    /// Reading the widget's parent through gtk-rs would have finalized it.
    fn a_refused_dock_registration_leaves_a_floating_parent_alone() {
        // SAFETY: GTK is initialised; a box and a label of the plugin's
        // own, the label sunk by the box it goes into.
        let (raw_box, raw_label) = unsafe {
            let raw_box = gtk::ffi::gtk_box_new(gtk::ffi::GTK_ORIENTATION_VERTICAL, 0);
            let raw_label = gtk::ffi::gtk_label_new(std::ptr::null());
            gtk::ffi::gtk_container_add(raw_box.cast(), raw_label);
            (raw_box, raw_label)
        };
        let mut alive: glib::ffi::gpointer = raw_box.cast();
        // SAFETY: a live object; GLib nulls `alive` if it is finalized.
        unsafe { glib::gobject_ffi::g_object_add_weak_pointer(raw_box.cast(), &raw mut alive) };
        // SAFETY: a live widget the box holds, so not floating; a
        // reference of the test's own.
        let client: gtk::Widget = unsafe { from_glib_none(raw_label) };
        assert!(
            register(&client, "made.so", "Made Floating Parent").is_none(),
            "a widget already inside a container was registered"
        );
        assert!(
            !alive.is_null(),
            "refusing the registration finalized the plugin's container"
        );
        assert_ne!(
            // SAFETY: still alive, per the weak pointer.
            unsafe { glib::gobject_ffi::g_object_is_floating(raw_box.cast()) },
            glib::ffi::GFALSE,
            "refusing the registration took the plugin's floating reference"
        );
        // From here the plugin keeps its container.
        //
        // SAFETY: alive and floating; sinking makes the reference the
        // test's, which it never gives up.
        unsafe {
            glib::gobject_ffi::g_object_remove_weak_pointer(raw_box.cast(), &raw mut alive);
            glib::gobject_ffi::g_object_ref_sink(raw_box.cast());
        }
    }

    /// A plugin panel docked in the main window is still the plugin's, so
    /// still a parent — and so is a widget inside it. The host's wrapping
    /// around it is not, shown or not. And a widget made into a realized
    /// parent works.
    fn a_plugin_panel_is_a_parent_and_its_wrapping_is_not(main: &gtk::Window) {
        let ask = create_plugin_scintilla;
        let panel = plugin_widget();
        open(&panel, "made.so", "Made Sci Host");
        assert!(
            panel.is_ancestor(main),
            "the panel is not docked in the main window"
        );
        let made = ask(handle_of(&panel));
        assert!(!made.is_null(), "a docked plugin panel was refused");
        assert_eq!(parent_of(made), handle_of(&panel));
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel
            .downcast_ref::<gtk::Container>()
            .expect("the panel is a box")
            .add(&inner);
        assert!(
            !ask(handle_of(&inner)).is_null(),
            "a widget inside a plugin panel was refused"
        );
        let viewport = panel.parent().expect("the host's viewport");
        let scrolled = viewport.parent().expect("the host's scrolled container");
        for (what, wrapping) in [("viewport", &viewport), ("scrolled container", &scrolled)] {
            assert!(
                ask(handle_of(wrapping)).is_null(),
                "the host's {what} around a plugin panel was accepted as a parent"
            );
        }
        // A panel registered but not shown: its wrapping is parked, and is
        // the host's all the same.
        let unshown = plugin_widget();
        assert!(register(&unshown, "made.so", "Made Sci Unshown").is_some());
        let unshown_wrapping = unshown.parent().expect("adopted at registration");
        assert!(
            ask(handle_of(&unshown_wrapping)).is_null(),
            "the host's wrapping around an unshown plugin panel was accepted as a parent"
        );
        // Its plugin takes the widget back out. The host retires the
        // registration at the next main-loop turn, but it stops counting
        // at once — whether a registration stands is read from GTK — so
        // the viewport it leaves, empty now and no longer refused as a
        // full `GtkBin`, is the host's.
        unshown_wrapping
            .downcast_ref::<gtk::Container>()
            .expect("the host's viewport is a container")
            .remove(&unshown);
        assert!(
            ask(handle_of(&unshown_wrapping)).is_null(),
            "the host's emptied wrapping around a plugin panel was accepted as a parent"
        );

        // Into a parent already on screen: the widget is realized as it
        // goes in, inside the dispatch, and is still the plugin's to show.
        main.show_all();
        pump();
        assert!(panel.is_realized(), "the panel is not on screen");
        let late = ask(handle_of(&panel));
        assert!(!late.is_null());
        let widget = widget_at(late);
        assert!(
            widget.is_realized(),
            "a widget put into a realized parent was not realized as it went in"
        );
        assert!(
            !widget.is_visible(),
            "a new widget is shown before the plugin shows it"
        );
        widget.show();
        pump();
        assert!(
            widget.is_mapped(),
            "a shown widget in a shown panel is not on screen"
        );
        assert_eq!(round_trip(late, "hello"), 5);
        finish(&panel);
    }

    /// A container that turns the widget away — a `GtkPaned` already
    /// holding two, which also prints GTK's own warning — leaves it in no
    /// container, still answered and routed.
    fn a_container_that_turns_the_widget_away_leaves_it_routed() {
        let full_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        full_paned.pack1(&gtk::Label::new(None), true, true);
        full_paned.pack2(&gtk::Label::new(None), true, true);
        let turned_away = create_plugin_scintilla(handle_of(&full_paned));
        assert!(
            !turned_away.is_null(),
            "a container that turned the widget away left the plugin without one"
        );
        assert!(
            parent_of(turned_away).is_null(),
            "the widget a container turned away is in a container"
        );
        assert_eq!(
            round_trip(turned_away, "ab"),
            2,
            "the widget a container turned away is not routed"
        );
    }

    /// A plugin's widget answers `SCI_*` from the plugin's own thread the
    /// way the host's does: parked until the main loop runs.
    fn a_plugin_widget_is_routed_from_any_thread() {
        let made = create_plugin_scintilla(npp_sentinel());
        assert_eq!(round_trip(made, "hello"), 5);
        let worker = length_from_a_worker(made);
        until_finished(&worker);
        assert_eq!(worker.join().expect("worker panicked"), 5);
    }

    /// A live widget the plugin moves about — between windows, hidden,
    /// unrealized, taken out and put back — stays routed: only its
    /// destruction takes it out, and nothing on the way looks like that to
    /// its scrollbars.
    fn a_widget_moved_about_stays_routed() {
        let first = gtk::Window::new(gtk::WindowType::Toplevel);
        let second = gtk::Window::new(gtk::WindowType::Toplevel);
        let made = create_plugin_scintilla(handle_of(&first));
        assert!(!made.is_null());
        let widget = widget_at(made);
        first.show_all();
        pump();
        first.remove(&*widget);
        second.add(&*widget);
        second.show_all();
        pump();
        widget.hide();
        widget.unrealize();
        second.remove(&*widget);
        first.add(&*widget);
        widget.show();
        pump();
        assert_eq!(
            round_trip(made, "still here"),
            10,
            "a widget the plugin moved about was taken out of routing"
        );
        // SAFETY: the test's own windows, which nothing else holds.
        unsafe {
            first.destroy();
            second.destroy();
        }
    }

    /// The plugin destroys its widget, as a Win32 plugin destroys its
    /// control: closing the dialog it sits in, right after editing it.
    /// Scintilla still runs the work the edit queued, after the dispose,
    /// and writes nothing of the host's; and the handle answers 0 from
    /// then on — from a worker thread too, when the widget is destroyed
    /// while the worker's message waits for the main loop.
    fn a_destroyed_widget_is_routed_nothing() {
        let dialog = gtk::Window::new(gtk::WindowType::Toplevel);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dialog.add(&column);
        let made = create_plugin_scintilla(handle_of(&column));
        assert!(!made.is_null());
        widget_at(made).set_vexpand(true);
        dialog.show_all();
        pump();
        // Enough text that Scintilla queues a scrollbar update for it,
        // and the dialog closed before the main loop gets to that.
        let text = "a line long enough to scroll sideways a little way at least\n".repeat(400);
        assert!(round_trip(made, &text) > 0);
        // Scintilla's own children are held by the host too: three of
        // them, each with the host's reference besides Scintilla's. A
        // Scintilla whose dispose frees something else needs re-reading.
        // Collected first and asserted after: a panic inside `forall`
        // would abort in GTK's trampoline rather than fail the test.
        let mut refs = Vec::new();
        widget_at(made)
            .downcast_ref::<gtk::Container>()
            .expect("a Scintilla widget is a container")
            .forall(|part| {
                // SAFETY: a live object, the widget's own; a field read.
                refs.push(unsafe {
                    (*part.as_ptr().cast::<glib::gobject_ffi::GObject>()).ref_count
                });
            });
        assert_eq!(
            refs.len(),
            3,
            "Scintilla's own children changed: re-check what its dispose frees"
        );
        assert!(
            refs.iter().all(|&refs| refs >= 2),
            "a part of a plugin's widget is not held by the host"
        );
        // SAFETY: the test's own window, which nothing else holds.
        unsafe { dialog.destroy() };
        // Objects of the host's, made where the freed memory would be.
        let unrelated: Vec<gtk::Adjustment> = (0..3000)
            .map(|_| gtk::Adjustment::new(0.0, 0.0, 12_345.0, 1.0, 10.0, 7.0))
            .collect();
        pump();
        assert!(
            unrelated
                .iter()
                .all(|a| a.upper().to_bits() == 12_345.0_f64.to_bits()
                    && a.page_size().to_bits() == 7.0_f64.to_bits()),
            "Scintilla wrote into memory the host had reused after the widget was destroyed"
        );
        assert_eq!(
            plugin_dispatch(made, SCI_GETLENGTH, 0, 0),
            0,
            "a destroyed widget was sent a message"
        );
        // A null handle matches no slot, the cleared ones included.
        assert_eq!(
            plugin_dispatch(std::ptr::null_mut(), SCI_GETLENGTH, 0, 0),
            0,
            "a null handle was sent a message"
        );
        assert!(
            create_plugin_scintilla(made).is_null(),
            "a destroyed Scintilla widget was accepted as a parent"
        );
        assert!(
            register(&widget_at(made), "made.so", "Made Destroyed").is_none(),
            "a destroyed Scintilla widget was registered as a dock panel"
        );

        // Plugin code that runs while the widget is being taken apart — a
        // handler on its container's `remove` here — finds it gone: its
        // scrollbars went first, before that handler runs. The code page
        // is asked because the answer differs: a routed message reads
        // UTF-8, a refused one 0.
        let its_dialog = gtk::Window::new(gtk::WindowType::Toplevel);
        let its_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        its_dialog.add(&its_column);
        let doomed = create_plugin_scintilla(handle_of(&its_column));
        assert!(!doomed.is_null());
        let answered = std::rc::Rc::new(Cell::new(-1_isize));
        let seen = answered.clone();
        its_column.connect_remove(move |_, _| {
            seen.set(plugin_dispatch(doomed, SCI_GETCODEPAGE, 0, 0));
        });
        // SAFETY: the test's own window, which nothing else holds.
        unsafe { its_dialog.destroy() };
        assert_eq!(
            answered.get(),
            0,
            "a widget being destroyed was sent a message from its container's `remove`"
        );

        let other = create_plugin_scintilla(npp_sentinel());
        assert_eq!(round_trip(other, "abc"), 3);
        let worker = length_from_a_worker(other);
        // SAFETY: a widget the host made, which the test destroys as its
        // plugin would.
        unsafe { gtk::ffi::gtk_widget_destroy(other.cast()) };
        until_finished(&worker);
        assert_eq!(
            worker.join().expect("worker panicked"),
            0,
            "a widget destroyed while its message waited for the main loop was sent it"
        );
    }

    /// A plugin's window is registered and made transient for the main
    /// window — unless it has a transient parent of its own — and the
    /// handles that are no window of a plugin's are not. Removal refuses
    /// only what it can tell without reading the pointer.
    fn modeless_dialogs_are_checked_and_made_transient(main: &gtk::Window, host_sci: *mut c_void) {
        let register = |handle: *mut c_void, add: bool| register_modeless_dialog(handle, add, main);
        let dialog = gtk::Window::new(gtk::WindowType::Toplevel);
        assert!(
            register(handle_of(&dialog), true),
            "a plugin's window was refused"
        );
        assert_eq!(
            dialog.transient_for().as_ref(),
            Some(main),
            "a registered dialog is not transient for the main window"
        );
        let its_parent = gtk::Window::new(gtk::WindowType::Toplevel);
        let parented = gtk::Window::new(gtk::WindowType::Toplevel);
        parented.set_transient_for(Some(&its_parent));
        assert!(register(handle_of(&parented), true));
        assert_eq!(
            parented.transient_for().as_ref(),
            Some(&its_parent),
            "a dialog's own transient parent was replaced"
        );
        assert!(
            register(handle_of(&dialog), false),
            "its removal was refused"
        );
        // A floating dock window is the host's, and so is one the dock
        // has pooled since.
        let floated = plugin_widget();
        let panel = open(&floated, "made.so", "Made Dialog Float");
        let floating = float_panel(panel);
        assert!(
            !register(handle_of(&floating), true),
            "a floating dock window was registered"
        );
        assert!(
            dock_panel_back(panel).contains(&floating),
            "the dock did not pool the window it no longer floats a group in"
        );
        assert!(
            !register(handle_of(&floating), true),
            "a pooled dock window was registered"
        );
        finish(&floated);
        // Nor is a window registered while the dock cannot say which
        // windows are the host's.
        let busy = gtk::Window::new(gtk::WindowType::Toplevel);
        assert!(
            !with_the_dock_busy(|| register(handle_of(&busy), true)),
            "a dialog was registered while the dock could not be asked about it"
        );
        assert!(busy.transient_for().is_none());
        let not_a_window = gtk::Label::new(None);
        for (what, handle) in [
            ("null", std::ptr::null_mut()),
            ("the npp handle", npp_sentinel()),
            ("the host's Scintilla widget", host_sci),
            ("a widget that is no window", handle_of(&not_a_window)),
            ("the main window", handle_of(main)),
            (
                "another window of the host's",
                handle_of(&rig_hint_window()),
            ),
        ] {
            assert!(!register(handle, true), "{what} was registered");
        }
        for (what, handle) in [
            ("null", std::ptr::null_mut()),
            ("the npp handle", npp_sentinel()),
            ("the host's Scintilla widget", host_sci),
        ] {
            assert!(!register(handle, false), "{what}'s removal was answered");
        }
    }

    /// A plugin's toolbar button: added once per command, the first after
    /// a separator, its icon replaced on a second request, refused for an
    /// unknown command or a non-image — and a click runs the command once
    /// and leaves the plugin's mark showing, not GTK's toggle, where the
    /// host's own changes to the mark run nothing.
    fn plugin_toolbar_buttons() {
        let toolbar = gtk::Toolbar::new();
        toolbar.insert(
            &gtk::ToolButton::new(None::<&gtk::Widget>, Some("built-in")),
            -1,
        );
        let before_items = toolbar.n_items();
        let small = Pixbuf::new(Colorspace::Rgb, true, 8, 16, 16).expect("a 16 px pixbuf");
        let large = Pixbuf::new(Colorspace::Rgb, true, 8, 48, 48).expect("a 48 px pixbuf");
        let add = |icon: *mut c_void| add_toolbar_icon(&toolbar, SMOKE_CMD, icon);
        assert!(
            !add(handle_of(&small)),
            "a button for an unknown command was added"
        );

        // Make the command known, as a load pass does, and give it a mark
        // to show.
        COMMAND_LABELS.with(|labels| {
            labels
                .borrow_mut()
                .insert(SMOKE_CMD, "Smoke Command".to_owned())
        });
        let func = FuncItem {
            item_name: [0; MENU_TITLE_LENGTH],
            p_func: Some(smoke_command),
            cmd_id: SMOKE_CMD,
            init2_check: 0,
            p_sh_key: std::ptr::null_mut(),
        };
        PLUGIN_CHECKS.with(|c| c.borrow_mut().absorb([&func]));
        assert!(set_menu_check(SMOKE_CMD, true));

        let not_an_image = gtk::Label::new(None);
        assert!(
            !add(handle_of(&not_an_image)),
            "a label was taken as an image"
        );
        assert!(
            add(handle_of(&small)),
            "a known command's button was refused"
        );
        assert_eq!(
            toolbar.n_items(),
            before_items + 2,
            "not one separator and one button"
        );
        assert!(toolbar
            .nth_item(before_items)
            .is_some_and(|item| item.is::<gtk::SeparatorToolItem>()));
        let button = toolbar
            .nth_item(before_items + 1)
            .and_then(|item| item.downcast::<gtk::ToggleToolButton>().ok())
            .expect("the last item is the button");
        assert!(
            button.is_active(),
            "the button does not show the command's mark"
        );
        assert_eq!(button.tooltip_text().as_deref(), Some("Smoke Command"));
        assert_eq!(
            ToolButtonExt::label(&button).as_deref(),
            Some("Smoke Command"),
            "the button has no name for the overflow menu"
        );
        let icon_width = |button: &gtk::ToggleToolButton| {
            ToolButtonExt::icon_widget(button)
                .expect("an icon")
                .preferred_width()
                .1
        };
        assert_eq!(
            icon_width(&button),
            16,
            "a small icon is not drawn at its own size"
        );

        // A second request replaces the icon and adds nothing.
        assert!(add(handle_of(&large)));
        assert_eq!(toolbar.n_items(), before_items + 2);
        assert_eq!(
            icon_width(&button),
            24,
            "a large icon is not scaled down to the cell"
        );
        assert!(button.is_active(), "replacing the icon changed the mark");

        clicks_show_the_plugins_mark(&button, &add, &small);

        // A second command's button goes after the first, with no
        // separator of its own.
        COMMAND_LABELS.with(|labels| {
            labels
                .borrow_mut()
                .insert(SMOKE_CMD_2, "Smoke Command 2".to_owned())
        });
        assert!(add_toolbar_icon(&toolbar, SMOKE_CMD_2, handle_of(&small)));
        assert_eq!(toolbar.n_items(), before_items + 3);
        assert!(toolbar
            .nth_item(before_items + 2)
            .is_some_and(|item| item.is::<gtk::ToggleToolButton>()));
    }

    /// A click on `button` runs its command once and leaves the plugin's
    /// mark showing; the host's own changes to the mark run nothing.
    /// `add` asks for the button again, as `NPPM_ADDTOOLBARICON`.
    fn clicks_show_the_plugins_mark(
        button: &gtk::ToggleToolButton,
        add: &dyn Fn(*mut c_void) -> bool,
        small: &Pixbuf,
    ) {
        // A click — on the button inside the tool item, which is what the
        // pointer clicks — flips its state; the handler puts the plugin's
        // mark back and runs the command (no plugin is loaded here, so the
        // command finds nothing to run, and the count is what shows it).
        let click = || {
            button
                .child()
                .and_then(|inner| inner.downcast::<gtk::Button>().ok())
                .expect("a tool button holds a button")
                .clicked();
        };
        let runs = || COMMANDS_RUN.with(Cell::get);
        let before = runs();
        click();
        assert_eq!(runs(), before + 1, "a click did not run the command once");
        assert!(button.is_active(), "the click's toggle was left showing");
        // A mark set through `NPPM_SETMENUITEMCHECK` reaches the button
        // without running anything, and the next click keeps it.
        assert!(set_menu_check(SMOKE_CMD, false));
        assert_eq!(runs(), before + 1, "a mark the plugin set ran its command");
        assert!(
            !button.is_active(),
            "a mark the plugin cleared still shows on its button"
        );
        click();
        assert_eq!(runs(), before + 2, "a click did not run the command once");
        assert!(!button.is_active(), "the click's toggle was left showing");
        // A second request for the button brings the recorded mark to it
        // quietly too.
        PLUGIN_CHECKS.with(|c| c.borrow_mut().set(SMOKE_CMD, true));
        assert!(add(handle_of(small)));
        assert!(button.is_active(), "a second request did not show the mark");
        assert_eq!(runs(), before + 2, "a second request ran the command");
    }

    /// At twice the scale, a plugin's icon is drawn at twice the pixels
    /// in the same 24-pixel cell, so it stays sharp on a high-DPI screen.
    fn plugin_icons_are_drawn_at_the_screens_pixels() {
        let large = Pixbuf::new(Colorspace::Rgb, true, 8, 48, 48).expect("a 48 px pixbuf");
        let sharp = plugin_icon_image(&large, 2).expect("an icon drawn at scale 2");
        let surface = sharp
            .property::<Option<gtk::cairo::Surface>>("surface")
            .expect("drawn as a surface");
        let pixels = gtk::cairo::ImageSurface::try_from(surface).expect("an image surface");
        assert_eq!(
            (pixels.width(), pixels.device_scale()),
            (48, (2.0, 2.0)),
            "a scale-2 icon is not drawn at the screen's pixels"
        );
        // GTK 3 sizes a shown widget only.
        sharp.show();
        assert_eq!(
            sharp.preferred_width().1,
            24,
            "a scale-2 icon left its cell"
        );
    }

    /// A plugin command that is never run: the scenario loads no plugin,
    /// so `on_plugin_command` resolves nothing.
    extern "C" fn smoke_command() {}
}
