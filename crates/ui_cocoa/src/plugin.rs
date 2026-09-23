//! Cocoa plugin-host wiring.
//!
//! The host, discovery, lifecycle and the ~95-message NPPM/NPPN
//! dispatcher are all cross-platform (`codepp-plugin-host` +
//! `codepp-shell`), so this module supplies only the pieces that have to
//! know about AppKit:
//!
//! 1. **The message-routing bridge.** On Windows a plugin's
//!    `SendMessage(scintillaHandle, SCI_*, …)` is routed by the OS
//!    message pump for free — the handle *is* the Scintilla window. A
//!    macOS plugin `.dylib` has no Scintilla linked and there is no OS
//!    pump, so `codepp-plugin-sdk` forwards every `SendMessageW` to a
//!    host callback instead. [`plugin_dispatch`] is that callback.
//! 2. **The Plugins menu** — lazy-load on first open, then a submenu per
//!    plugin built from its `FuncItem`s, plus the two admin entries.
//! 3. **The Plugin Manager** modal.
//! 4. **Notification delivery** — draining the shell's queued `NPPN_*`
//!    notifications to every loaded plugin's `beNotified`.
//!
//! # Routing is by handle *identity*, not by message range
//!
//! `SCI_*` and `NPPM_*` message numbers overlap, so the number alone
//! cannot say where a message belongs — only the handle can.
//! [`NPP_SENTINEL`]'s address is this backend's "npp handle" and routes
//! to the host dispatcher; the one legitimate `ScintillaView*` routes to
//! Scintilla; **every other pointer is refused**. That last clause is
//! the security-relevant one: `scintilla_cocoa_send_message` casts its
//! argument and messages it, so forwarding an unvalidated pointer would
//! turn a plugin bug into a wild `objc_msgSend`. Win32 fails soft here
//! (`SendMessage` to an unknown `HWND` returns 0 without dereferencing)
//! and so does this.
//!
//! # Re-entrancy
//!
//! A plugin's menu callback and its `beNotified` are invoked with **no**
//! `with_state` borrow held: the caller looks the function pointer up,
//! drops the borrow, then calls. That is what makes a plugin's own
//! re-entrant `NPPM_*` calls work rather than being declined — the
//! memory-safe equivalent of Win32's `PLUGIN_CALL_ACTIVE` guard, since
//! `with_state`'s `try_borrow_mut` already declines true re-entry.
//!
//! [`VALID_SCI`] is deliberately an atomic rather than a `with_state`
//! read for the same reason in reverse: the identity check must still
//! work when a plugin sends `SCI_*` from inside a `beNotified` that does
//! hold the borrow, where a `with_state` read would be declined — and a
//! declined read here would read as "not our view" and **refuse a
//! legitimate message**.
//!
//! # Threading, and where this differs from Windows
//!
//! On Windows a plugin calling `SendMessage` from its own worker thread
//! is marshaled by the OS onto the thread that owns the window, so a
//! plugin written against that semantics is safe by construction. Off
//! Windows the SDK forwards straight to [`plugin_dispatch`] on whatever
//! thread called it, and this module restores the affinity the missing
//! pump would have provided:
//!
//!   * `NPPM_*` off-thread degrades safely on its own — the state lives
//!     in a `thread_local`, so [`dispatch_nppm`] finds nothing and
//!     returns "declined".
//!   * `SCI_*` off-thread would reach `objc_msgSend` on a
//!     `ScintillaView` from the wrong thread, which both AppKit and
//!     Scintilla document as undefined — and that branch deliberately
//!     bypasses `with_state` (see above), so nothing else would catch
//!     it. [`plugin_dispatch`] therefore checks [`on_main_thread`] and
//!     hops through [`send_sci_on_main`] when the caller is not, exactly
//!     as `ui_gtk::plugin` does with `MainContext::invoke`. The same
//!     answer on both non-Windows backends, decided in DESIGN.md §7.4.
//!
//! The same-thread fast path is **load-bearing here, not an
//! optimisation**: `dispatch_sync` onto the main queue *from* the main
//! thread is a libdispatch client bug that aborts the process, where
//! GTK's `invoke` merely dispatches inline. See [`send_sci_on_main`].

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSBorderType, NSButton, NSControlTextEditingDelegate, NSFont, NSLineBreakMode, NSMenu,
    NSMenuItem, NSScrollView, NSStackView, NSTableColumn, NSTableView,
    NSTableViewColumnAutoresizingStyle, NSTableViewDataSource, NSTableViewDelegate, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};

use codepp_plugin_host::{HostDispatchFn, NppData};
use codepp_scintilla_sys::{scintilla_cocoa_send_message, SCI_GETMODIFY};
use codepp_shell::{sanitize_str_for_display, HostHandles};

use crate::menu::Actions;
use crate::state::with_state;

/// The one legitimate `ScintillaView*`, cached so [`plugin_dispatch`]
/// can identity-check the handle a plugin routes an `SCI_*` message to.
/// Set once at startup by [`discover`]; see the module docs for why it
/// is an atomic and not a `with_state` read.
///
/// The Document Map's miniature view is deliberately **not** here: a
/// plugin is only ever handed `NppData._scintillaMainHandle`, so a
/// message addressed to the miniature did not come from anywhere
/// legitimate.
static VALID_SCI: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// A dedicated sentinel whose *address* is this backend's "npp handle".
///
/// The **same** address fills `NppData.npp_handle`, `HostHandles.npp_hwnd`
/// and every outbound `nmhdr.hwndFrom`, so a plugin that caches the host
/// handle from a notification routes back here rather than into
/// `scintilla_cocoa_send_message`.
static NPP_SENTINEL: u8 = 0;

/// Width of the Plugin Manager's table, in points.
const MANAGER_WIDTH: f64 = 520.0;
/// Height of the Plugin Manager's table, in points.
const MANAGER_TABLE_HEIGHT: f64 = 260.0;

/// The npp-handle sentinel pointer. Stable for the process lifetime.
fn npp_sentinel() -> *mut c_void {
    std::ptr::addr_of!(NPP_SENTINEL).cast_mut().cast::<c_void>()
}

/// Whether `hwnd` is the host's own Scintilla view — the only pointer
/// [`plugin_dispatch`] will forward an `SCI_*` message to.
fn is_valid_scintilla(hwnd: *mut c_void) -> bool {
    let valid = VALID_SCI.load(Ordering::Acquire);
    !valid.is_null() && std::ptr::eq(hwnd, valid)
}

/// Whether the caller is on the process main thread — the one AppKit
/// owns, the one every `ScintillaView` message must be sent from, and
/// the one the main dispatch queue drains on.
///
/// `MainThreadMarker::new` is `pthread_main_np`, so this needs no
/// arming step: unlike `ui_gtk`, whose "UI thread" is whichever thread
/// called `gtk::init` and so has to be *recorded*, macOS fixes the main
/// thread at process start. Deliberately not derived from `with_state`'s
/// thread-local either: [`plugin_dispatch`]'s `SCI_*` branch must not
/// take the borrow (a plugin can send `SCI_*` from inside a `beNotified`
/// that already holds it), and a declined read there would read as "not
/// our thread" and marshal a call that is already on the right one —
/// which on this backend is a **process abort**, not a detour. See
/// [`send_sci_on_main`].
fn on_main_thread() -> bool {
    MainThreadMarker::new().is_some()
}

/// A raw pointer carried onto the main thread by [`send_sci_on_main`].
///
/// `DispatchQueue::exec_sync` requires a `Send` closure and a raw
/// pointer is not `Send`, so the crossing has to be made explicit.
struct MainThreadPtr(*mut c_void);

// SAFETY: the only pointer ever wrapped is one that has already passed
// [`is_valid_scintilla`], i.e. the host's own `ScintillaView*`. That view
// is created once at startup and never destroyed, removed from its
// superview or reassigned (the discipline `CocoaUiState::sci_view`
// documents and a source-scan guard enforces), so the address stays live
// for the whole process. It is *dereferenced only on the main thread*,
// which is the entire point of the marshal — the value crosses threads,
// the dereference does not.
unsafe impl Send for MainThreadPtr {}

/// Run one `SCI_*` message against Scintilla on the main thread and
/// block until it returns, for a plugin that called from its own thread.
///
/// # Why marshal rather than refuse
///
/// The reasoning is `ui_gtk::plugin::send_sci_on_main`'s, and is
/// recorded in full in DESIGN.md §7.4; the short form: refusing (return
/// 0) hands a query a *plausible* wrong answer and turns a mutation into
/// a silent no-op, so the plugin appears to work while its edits vanish.
/// Marshaling reproduces what the plugin was written against — a
/// cross-thread `SendMessage` also blocks until the window's thread
/// next pumps, and also deadlocks if that thread is meanwhile waiting
/// on the sender — so it inherits Win32's hazard rather than adding one.
///
/// # `dispatch_sync`, and why the caller must not be the main thread
///
/// `exec_sync` is `dispatch_sync` onto the main queue: it enqueues the
/// block and parks the caller until the main thread drains it, which
/// happens from the main run loop — the ordinary one, and also the
/// nested ones a modal, a menu-tracking loop or a tab drag pump, the
/// same premise `DrainFreeze` rests on. Calling it **from** the main
/// thread would park the only thread that could ever run the block;
/// libdispatch detects that and **aborts the process** rather than
/// letting it deadlock — measured, not assumed: with [`on_main_thread`]
/// hard-wired to `false` the smoke scenario's same-thread control dies
/// with `SIGTRAP` and a crash report reading *"BUG IN CLIENT OF
/// LIBDISPATCH: `dispatch_sync` called on queue already owned by current
/// thread"*. [`plugin_dispatch`] therefore consults [`on_main_thread`]
/// first and only reaches here from another thread. That is a
/// difference from GTK worth knowing: there the fast path avoids a
/// channel allocation, here it avoids a crash.
///
/// # No timeout, deliberately
///
/// A bounded wait would have to invent a return value on expiry, and
/// the only one available is 0 — it would convert a visible stall into
/// the silent wrong answer the first section rejects. `SendMessage` has
/// no timeout either. The unbounded wait blocks the *plugin's* worker
/// thread only; the main thread is never a participant.
///
/// # What that costs at shutdown
///
/// `-[NSApplication terminate:]` calls `exit()` without joining anything,
/// so a worker parked here at quit is a leaked thread, not a hang. It
/// becomes a hang the moment host code waits for plugin threads to
/// quiesce during teardown — nothing does today, and any future path
/// that does must not block on plugin threads. Same accepted risk as
/// GTK's, recorded per DESIGN.md §7.4.
///
/// # A panic in the hop
///
/// The block runs inside libdispatch's own C frames, which
/// [`plugin_dispatch`]'s `catch_unwind` on the *calling* thread cannot
/// cover, so the hop carries its own boundary. A panic there is logged,
/// the slot stays unset, and the caller gets 0 — the same answer the
/// unknown-handle branch gives, and a logged one.
fn send_sci_on_main(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    // Unconditional, not `debug_assert!`: the misuse it guards against
    // ends the process either way, and a panic names the call site where
    // libdispatch's crash report names only the queue. Cheap — one
    // `pthread_main_np` on a path that is already a cross-thread hop.
    assert!(
        !on_main_thread(),
        "send_sci_on_main called from the main thread: dispatch_sync would abort the process",
    );
    let ptr = MainThreadPtr(hwnd);
    let mut answer: Option<isize> = None;
    // `exec_sync` needs `Send` but not `'static`, so the result comes
    // back through a borrowed slot rather than a channel: the caller is
    // parked for the block's whole lifetime by construction.
    let slot = &mut answer;
    DispatchQueue::main().exec_sync(move || {
        crate::at_callback_boundary("plugin:sci_marshal", (), || {
            // Load-bearing, not a leftover: edition-2021 closures capture
            // disjoint fields, so without this rebind the outer `move`
            // closure would capture only `ptr.0` — a bare `*mut c_void`,
            // which is not `Send` — and `exec_sync`'s bound fails to
            // compile. Naming the whole `MainThreadPtr` captures the
            // wrapper that carries the `unsafe impl Send`.
            let ptr = ptr;
            // SAFETY: `ptr.0` passed `is_valid_scintilla` on the calling
            // thread and addresses the host's own permanently-live
            // `ScintillaView*` (see `MainThreadPtr`). This block runs on
            // the main thread, which is the affinity AppKit requires and
            // the reason the message was marshaled here at all.
            *slot = Some(unsafe { scintilla_cocoa_send_message(ptr.0, msg, wparam, lparam) });
        });
    });
    answer.unwrap_or_else(|| {
        tracing::warn!(
            msg,
            "cross-thread SCI_* dropped: the main-thread hop panicked (see the error above)"
        );
        0
    })
}

/// The routing callback the SDK forwards a plugin's `SendMessageW` to.
///
/// Wrapped in `catch_unwind`: it is entered from plugin code across an
/// `extern "C"` frame, where a Rust panic unwinding out is undefined
/// behaviour rather than merely unspecified. `at_callback_boundary` is
/// not used here because this is not an AppKit entry point and the
/// fallback value differs (0 is "message declined", the Win32 answer).
extern "C" fn plugin_dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
    catch_unwind(AssertUnwindSafe(|| {
        if std::ptr::eq(hwnd, npp_sentinel()) {
            dispatch_nppm(msg, wparam, lparam)
        } else if is_valid_scintilla(hwnd) {
            // SCI_* addressed to *our* Scintilla view. `with_state` is
            // deliberately not taken: this is a direct Scintilla call,
            // and the plugin may well issue it from inside an NPPM
            // dispatch that already holds the borrow. The identity check
            // above is an atomic read for the same reason, and it runs
            // *before* the affinity check so an unknown handle is refused
            // rather than marshaled.
            if on_main_thread() {
                // SAFETY: `hwnd` is identity-checked to be the host's own
                // live `ScintillaView*`, which is created once at startup
                // and never destroyed (see `CocoaUiState::sci_view`), this
                // is the thread that owns it, and
                // `scintilla_cocoa_send_message` is its documented entry
                // point. The message-argument contract is the plugin's
                // responsibility, exactly as it is on Win32.
                unsafe { scintilla_cocoa_send_message(hwnd, msg, wparam, lparam) }
            } else {
                // A plugin calling from its own thread: hop to the main
                // queue and block, restoring the affinity the Win32 pump
                // would have provided. See `send_sci_on_main` and
                // DESIGN.md §7.4.
                send_sci_on_main(hwnd, msg, wparam, lparam)
            }
        } else {
            // Any other pointer: refuse rather than message an
            // unvalidated address. See the module docs.
            0
        }
    }))
    .unwrap_or(0)
}

/// Route an `NPPM_*` message to the shared dispatcher, building the
/// Cocoa [`HostHandles`] from live state.
///
/// Returns 0 when state is unavailable — a re-entrant borrow, or after
/// teardown — which is the same "message declined" outcome Win32
/// produces when a plugin re-enters during a guarded call.
fn dispatch_nppm(msg: u32, wparam: usize, lparam: isize) -> isize {
    let routed = with_state(|st| {
        let handles = HostHandles {
            npp_hwnd: npp_sentinel(),
            scintilla_main: st.sci_ptr,
            // Single-view on this backend, like the other two: tabs
            // switch documents under one view via `SCI_SETDOCPOINTER`.
            scintilla_secondary: std::ptr::null_mut(),
            // No host-owned `NSMenu` pointer is exposed, so
            // `NPPM_GETMENUHANDLE` degrades to NULL — matching `ui_gtk`.
            // An `NSMenu*` is not an `HMENU`: a plugin that took one
            // would have to call AppKit on it, which is outside what a
            // source-compatible recompile (DESIGN.md §6.1) promises.
            plugin_menu: std::ptr::null_mut(),
            main_menu: std::ptr::null_mut(),
        };
        let editor = st.editor;
        let dirty_before = editor.send(SCI_GETMODIFY, 0, 0) != 0;
        let cached_before: Vec<bool> = st.shell.tabs.iter().map(|t| t.dirty).collect();
        let pre_active = st.shell.active_tab;
        let (shell, mut ui) = st.split();
        // SAFETY: called synchronously on the UI thread from plugin
        // code, with `(msg, wparam, lparam)` exactly as the plugin
        // passed them to `SendMessageW`; every `handles` field belongs
        // to this one window.
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
    // before the plugin's `SendMessageW` returns, which is when
    // Notepad++ shows the same prompts.
    crate::present_deferred_dialogs();
    routed
}

/// Entry points for `tests/cocoa_smoke.rs`, and nothing else.
///
/// The cross-thread `SCI_*` scenario has to drive the real
/// [`plugin_dispatch`] against a real `ScintillaView` from a real
/// spawned thread, and it has to own the process main thread to do it —
/// which only the `harness = false` smoke binary can (see its module
/// docs). An integration test cannot see private items, so the two it
/// needs are re-exported here. `ui_gtk` keeps the equivalent scenario
/// in-crate because GTK needs only *one* thread, not the first.
///
/// Hidden rather than `pub(crate)` because the consumer is a separate
/// crate; hidden rather than a public API because the store
/// [`arm_scintilla`] performs has exactly one legitimate non-test home,
/// [`discover`] — which does it inline, since this module does not
/// exist in the builds `discover` ships in.
///
/// **Compiled out of release builds.** [`arm_scintilla`] rewrites the
/// one trust anchor `plugin_dispatch` checks before it messages a
/// pointer, so it must not exist in a shipped binary at all — "nothing
/// calls it" is a fact about today's tree, not a guarantee. It is gated
/// on `debug_assertions` rather than a Cargo feature so the documented
/// smoke-test command (`cargo test … --ignored`, a dev-profile build)
/// keeps working unchanged; a `--release` test build reports the
/// scenario as ignored instead (see the smoke binary).
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod smoke_support {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    /// Stand in for [`super::discover`]: make `sci` the one handle
    /// [`super::plugin_dispatch`] will forward `SCI_*` to.
    ///
    /// # Safety
    ///
    /// `sci` must be a live `ScintillaView*` from `scintilla_cocoa_new`
    /// that stays live — never released, removed or reassigned — for
    /// the rest of the process. Every `SCI_*` a caller of [`dispatch`]
    /// addresses to it is then messaged to that object, from the main
    /// thread, exactly as `discover` arranges for the host's own view.
    pub unsafe fn arm_scintilla(sci: *mut c_void) {
        super::VALID_SCI.store(sci, Ordering::Release);
    }

    /// The routing callback itself, exactly as the SDK would call it.
    pub fn dispatch(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize {
        super::plugin_dispatch(hwnd, msg, wparam, lparam)
    }
}

/// The `NppData` handed to each plugin's `setInfo`.
fn npp_data() -> NppData {
    let sci = with_state(|st| st.sci_ptr).unwrap_or(std::ptr::null_mut());
    NppData {
        npp_handle: npp_sentinel(),
        scintilla_main_handle: sci,
        scintilla_second_handle: std::ptr::null_mut(),
    }
}

/// Discover plugins under the config dir's `plugins/` folder.
///
/// Records paths only — loading is deferred to the first Plugins-menu
/// open (DESIGN.md §6.4), so a user with forty installed plugins pays
/// no startup cost for the thirty-nine they do not touch. Called once
/// at startup.
pub(crate) fn discover() {
    // Cache the host's Scintilla view for `plugin_dispatch`'s identity
    // check (see [`VALID_SCI`]). Runs once at startup, by which point
    // the single view exists.
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
/// The load used to happen inside one borrow — `dlopen`, `setInfo`,
/// `getFuncsArray` and `NPPN_READY` together — and that borrow is
/// exactly what made a plugin's re-entrant `NPPM_*` decline. Real
/// plugins interrogate the host from `setInfo`: `NppExec` asks for the
/// version there and refuses to start without an answer, so a
/// declined query reads as "older than Notepad++ 5.1".
///
/// Splitting it costs the property that made a `DrainFreeze`
/// unnecessary here — the comment that used to sit on the call site
/// predicted exactly this — so the guard is now explicit. AppKit's
/// menu-tracking loop services GCD's main-queue source, and without
/// it a worker result could be applied *between* two plugins' loads,
/// moving the very tabs a `setInfo` is asking about.
///
/// A nested pass (a plugin re-entering the loader) is bounded inside
/// `PluginHost`, which answers "nothing pending" while a load is
/// outstanding.
fn load_pending_plugins() {
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
    notices.deliver(data.npp_handle, || {
        with_state(|st| st.shell.active_buffer_id()).flatten()
    });
}

/// Lazy-load every pending plugin, then rebuild the Plugins menu from
/// the loaded set. Called from the menu's `menuNeedsUpdate:`.
pub(crate) fn ensure_loaded_and_rebuild(menu: &NSMenu, actions: &Actions) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // Load pending plugins, installing this backend's routing callback
    // into each (the SDK handshake) so their `SendMessageW` reaches us.
    load_pending_plugins();
    rebuild_menu(menu, actions, mtm);
    // `NPPN_READY` fires inside `load_pending_plugins`, outside any
    // borrow; anything a plugin queued back is drained on the next
    // wake.
    crate::drain_shell();
}

/// One row of a plugin submenu, snapshotted under the `with_state`
/// borrow: label, command id, whether it is a command (vs. a
/// separator), and its display chord `(ctrl, alt, shift, key)` if any.
type PluginMenuRow = (String, i32, bool, Option<(bool, bool, bool, u8)>);

/// Rebuild the Plugins menu: one submenu per loaded plugin (its items
/// taken from the plugin's `FuncItem` array, a null `p_func` rendering
/// as a separator), or a greyed placeholder when none is loaded. Then,
/// always, a separator and the two admin entries — matching Win32's and
/// GTK's layout, so the manager stays reachable even to re-enable a
/// plugin the user previously disabled.
fn rebuild_menu(menu: &NSMenu, actions: &Actions, mtm: MainThreadMarker) {
    menu.removeAllItems();
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
        crate::menu::add_disabled(menu, mtm, "No plugins loaded");
    } else {
        for (name, items) in entries {
            let submenu = NSMenu::new(mtm);
            for (label, cmd_id, is_command, chord) in items {
                if is_command {
                    // Show the shortcut in the item *title* rather than
                    // as an `NSMenuItem` key equivalent. A key equivalent
                    // would let AppKit's own key-equivalent search fire
                    // this command independently of the keyDown monitor —
                    // and, worse, keep firing a chord the plugin has since
                    // dropped via `NPPM_REMOVESHORTCUTBYCMDID` until the
                    // menu is next rebuilt. The monitor is the single
                    // firing authority and consults the live cache, so the
                    // title carries display only. `chord` is already the
                    // winning, registrable chord (the shell filtered the
                    // rest).
                    let title = match chord.map(|(c, a, s, k)| chord_menu_suffix(c, a, s, k)) {
                        Some(suffix) => format!("{label}\t{suffix}"),
                        None => label,
                    };
                    let item = crate::menu::add(
                        &submenu,
                        mtm,
                        &title,
                        sel!(codeppPluginCommand:),
                        "",
                        Some(actions),
                    );
                    item.setTag(cmd_id as isize);
                } else {
                    submenu.addItem(&NSMenuItem::separatorItem(mtm));
                }
            }
            // A plugin is an untrusted source of chrome text, exactly
            // like a filename — `funcitem_label` already sanitizes each
            // item, and `getName()` gets the same treatment here.
            let top = NSMenuItem::new(mtm);
            let title = NSString::from_str(&sanitize_str_for_display(&name));
            // Both titles: a nested submenu item takes its label from
            // the *item*, not the menu. See `crate::menu::submenu`.
            submenu.setTitle(&title);
            top.setTitle(&title);
            top.setSubmenu(Some(&submenu));
            menu.addItem(&top);
        }
    }

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    crate::menu::add(
        menu,
        mtm,
        "Plugin Manager…",
        sel!(codeppPluginManager:),
        "",
        Some(actions),
    );
    crate::menu::add(
        menu,
        mtm,
        "Open Plugin Folder",
        sel!(codeppOpenPluginFolder:),
        "",
        Some(actions),
    );
}

/// Invoke a plugin's menu command.
///
/// Looks the function pointer up under a short `with_state` borrow,
/// drops the borrow, then calls the plugin **outside** it — so the
/// plugin's re-entrant `NPPM_*` calls acquire a fresh borrow and
/// actually work rather than being declined. Under `catch_unwind`,
/// because a panic must not cross the `extern "C"` frame.
pub(crate) fn on_plugin_command(cmd_id: i32) {
    let cmd = with_state(|st| st.shell.lookup_plugin_command(cmd_id)).flatten();
    let Some(cmd) = cmd else {
        return;
    };
    // SAFETY: `cmd` is a plugin `FuncItem.p_func`, invoked on the UI
    // thread with no arguments, per the N++ ABI.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { cmd() }));
    // The command may have edited the buffer, set status text, or queued
    // notifications; flush the wake pipeline and resync the chrome. The
    // chrome half matters for the same reason every other buffer-mutating
    // path here needs it: `SCN_MODIFIED` fired synchronously inside the
    // edit, while the borrow above was held, and was declined.
    crate::drain_shell();
    crate::refresh_tab_chrome();
}

/// Fire a plugin shortcut identified by its `(module_key, internalID)`
/// cache identity: lazy-load every pending plugin (a hotkey is the §6.4
/// load trigger, the second permitted one after the first menu open —
/// see the `startup_discovers_plugins_without_loading_them` guard),
/// resolve the identity to the loaded command, and dispatch it.
///
/// Returns `true` iff a command actually ran. The keyDown monitor
/// swallows the event only on `true`, so a chord that fails to resolve
/// (a bogus hand-edited `internalID`, or a plugin whose load failed)
/// still reaches the editor rather than being silently eaten. The drain
/// runs regardless so a partial load's `NPPN_READY` reaches the plugins.
pub(crate) fn fire_plugin_chord(module_key: &str, internal_id: u32) -> bool {
    load_pending_plugins();
    let cmd_id = with_state(|st| st.shell.resolve_plugin_command(module_key, internal_id))
        .flatten()
        .map(|(cmd_id, _)| cmd_id);
    if let Some(cmd_id) = cmd_id {
        on_plugin_command(cmd_id);
        true
    } else {
        crate::drain_shell();
        false
    }
}

/// Format a chord as the macOS glyph string shown in a plugin menu
/// item's title (`⌘⇧K`, `⌥F5`). `is_ctrl` maps to ⌘ (Command) — the
/// macOS-primary modifier and the convention every built-in Code++
/// shortcut follows — with `is_alt` → ⌥ and `is_shift` → ⇧, in the
/// conventional ⌥⇧⌘ order with ⌘ adjacent to the key.
///
/// This is display only, and deliberately *not* an `NSMenuItem`
/// key equivalent: a key equivalent would let AppKit fire the command
/// independently of the keyDown monitor and would keep firing a chord
/// the plugin later removes until the menu is rebuilt. The monitor is
/// the single firing authority; the title just shows what it will do.
/// The key name comes from `core::shortcuts::vk_display_name`, shared
/// with the Win32/GTK label path.
fn chord_menu_suffix(ctrl: bool, alt: bool, shift: bool, key: u8) -> String {
    let mut out = String::new();
    if alt {
        out.push('\u{2325}'); // ⌥
    }
    if shift {
        out.push('\u{21E7}'); // ⇧
    }
    if ctrl {
        out.push('\u{2318}'); // ⌘
    }
    out.push_str(&codepp_core::shortcuts::vk_display_name(key));
    out
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
/// `&Shell`) answered every such callback with 0, silently, while
/// `drain_shell`'s own comment promised the opposite. The Win32 and
/// GTK backends deliver from the same snapshot for the same reason.
/// Any dialog a handler queued is presented afterwards.
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

/// Decode a `FuncItem`'s NUL-terminated UTF-16 `item_name` to a String,
/// sanitized for display.
fn funcitem_label(f: &codepp_plugin_host::FuncItem) -> String {
    let end = f
        .item_name
        .iter()
        .position(|&u| u == 0)
        .unwrap_or(f.item_name.len());
    sanitize_str_for_display(&String::from_utf16_lossy(&f.item_name[..end]))
}

/// Reveal the plugins directory in Finder.
///
/// `create_dir_all` first, matching the other two backends, so a click
/// before any plugin has been staged still targets a valid path.
pub(crate) fn open_plugin_folder() {
    let Some(dir) = codepp_platform::plugins_dir() else {
        tracing::warn!("no config dir; cannot open the plugins folder");
        return;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(?err, "could not create the plugins folder");
        return;
    }
    let Some(path) = dir.to_str() else {
        tracing::warn!(?dir, "plugins folder path is not valid UTF-8");
        return;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSWorkspace::sharedWorkspace().openURL(&url);
}

/// One row of the Plugin Manager, snapshotted at open.
#[derive(Clone)]
struct ManagerRow {
    /// Registry index — the functional value `set_plugin_disabled`
    /// takes. Deliberately not derived from the label.
    index: usize,
    /// Sanitized display label.
    label: String,
    /// Sanitized status text.
    status: String,
    /// Whether the plugin is currently enabled (`!disabled`).
    enabled: bool,
}

thread_local! {
    /// The Plugin Manager's rows, read directly by its table's data
    /// source.
    ///
    /// A `thread_local` rather than a `with_state` read for the reason
    /// `crate::fif` and `crate::workspace` document: `NSTableView` asks
    /// for its row count and cell views *synchronously* from inside
    /// `reloadData` and from inside layout, and a data source that
    /// reached back through `with_state` would be declined
    /// re-entrantly on any path that already holds the borrow —
    /// answering **zero rows**, i.e. an empty manager.
    static MANAGER_ROWS: std::cell::RefCell<Vec<ManagerRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

const COL_ENABLED: &str = "enabled";
const COL_STATUS: &str = "status";

define_class!(
    /// The Plugin Manager's table data source, delegate, and the target
    /// of its Enabled checkboxes.
    ///
    /// A dedicated class rather than reusing [`Actions`] because
    /// `Actions` is already the Find-in-Files dock's delegate, and one
    /// object answering `numberOfRowsInTableView:` for two tables would
    /// have to disambiguate by receiver — a needless coupling for a
    /// modal that lives for one `runModal`.
    ///
    /// SAFETY: plain `NSObject` subclass with no ivars. Every method is
    /// invoked by AppKit on the main thread, which is where the
    /// `thread_local` row store it reads lives.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CodeppPluginManagerSource"]
    struct ManagerSource;

    unsafe impl NSObjectProtocol for ManagerSource {}

    unsafe impl NSTableViewDataSource for ManagerSource {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            // Zero is the safe fallback: an empty table, never a count
            // AppKit would then ask for views it cannot produce.
            crate::at_callback_boundary("plugin:numberOfRows", 0, || {
                isize::try_from(MANAGER_ROWS.with(|r| r.borrow().len())).unwrap_or(isize::MAX)
            })
        }
    }

    unsafe impl NSControlTextEditingDelegate for ManagerSource {}

    unsafe impl NSTableViewDelegate for ManagerSource {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_column_row(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            crate::at_callback_boundary("plugin:viewForRow", None, || {
                let mtm = MainThreadMarker::new()?;
                let id = column?.identifier().to_string();
                let index = usize::try_from(row).ok()?;
                let entry = MANAGER_ROWS.with(|rows| rows.borrow().get(index).cloned())?;
                if id == COL_ENABLED {
                    // SAFETY: `self` implements
                    // `codeppTogglePluginEnabled:` — declared in this
                    // same `define_class!`, so the selector cannot be
                    // stale. AppKit holds the target weakly;
                    // `show_plugin_manager` keeps `self` alive for the
                    // whole modal.
                    let check = unsafe {
                        NSButton::checkboxWithTitle_target_action(
                            &NSString::from_str(""),
                            Some(self),
                            Some(sel!(codeppTogglePluginEnabled:)),
                            mtm,
                        )
                    };
                    check.setState(isize::from(entry.enabled));
                    // The **registry index**, not the row index, and the
                    // two are *not* equal: `installed_plugins` returns
                    // rows sorted by display label while the index is
                    // discovery order. Measured on the real app —
                    // row 0 is "Converter" at registry index 1, row 2
                    // is "Export" at index 0 — and a row-index key was
                    // driven to confirm the consequence: clicking
                    // Converter's checkbox disabled Export.
                    check.setTag(isize::try_from(entry.index).unwrap_or(isize::MAX));
                    return Some(Retained::into_super(Retained::into_super(check)));
                }
                let text = if id == COL_STATUS {
                    entry.status
                } else {
                    entry.label
                };
                let label = NSTextField::labelWithString(&NSString::from_str(&text), mtm);
                label.setFont(Some(&NSFont::systemFontOfSize(
                    NSFont::smallSystemFontSize(),
                )));
                label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                Some(Retained::into_super(Retained::into_super(label)))
            })
        }
    }

    impl ManagerSource {
        /// A row's Enabled checkbox was clicked.
        ///
        /// AppKit has **already** flipped the button's state by the time
        /// this fires, so the new state is applied rather than the model
        /// inverted — the same rule the toolbar's `PushOnPushOff`
        /// buttons follow, and for the same reason: inverting would
        /// fight the button and land on the wrong value every other
        /// click.
        #[unsafe(method(codeppTogglePluginEnabled:))]
        fn toggle_plugin_enabled(&self, sender: Option<&NSButton>) {
            crate::at_callback_boundary("plugin:toggleEnabled", (), || {
                let Some(sender) = sender else { return };
                let Ok(index) = usize::try_from(sender.tag()) else {
                    return;
                };
                let enabled = sender.state() != 0;
                // `disabled == !enabled`. Persisted to `disabled.txt`;
                // effective on the next launch, since an already-loaded
                // plugin is not unmapped mid-session (Notepad++'s
                // restart-required semantics).
                with_state(|st| st.shell.set_plugin_disabled(index, !enabled));
                MANAGER_ROWS.with(|rows| {
                    if let Some(row) = rows.borrow_mut().iter_mut().find(|r| r.index == index) {
                        row.enabled = enabled;
                    }
                });
            });
        }
    }
);

impl ManagerSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `init` on a freshly allocated instance of our own
        // class, which adds no ivars needing other initialisation.
        unsafe { objc2::msg_send![this, init] }
    }
}

/// Show the modal Plugin Manager: every discovered plugin with an
/// Enabled checkbox and a status column.
///
/// Toggling a checkbox writes through to
/// `<plugins_config_dir>/disabled.txt`; the change takes effect on the
/// next launch, matching Notepad++ and the other two backends.
pub(crate) fn show_plugin_manager() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // The alert spins a nested run loop, where GCD's main-queue source
    // is still serviced — so a worker wake could otherwise drain the
    // shell underneath the modal. See [`crate::DrainFreeze`].
    let _freeze = crate::DrainFreeze::new();

    let admin = with_state(|st| st.shell.installed_plugins()).unwrap_or_default();
    MANAGER_ROWS.with(|rows| {
        *rows.borrow_mut() = admin
            .iter()
            .map(|entry| {
                let status = if entry.loaded {
                    "Loaded".to_owned()
                } else if let Some(reason) = &entry.failed_reason {
                    format!("Failed: {reason}")
                } else {
                    "Not loaded".to_owned()
                };
                ManagerRow {
                    index: entry.index,
                    // Both are plugin-supplied (`getName()`, or a
                    // loader error naming a file), so both are chrome
                    // text from an untrusted source.
                    label: sanitize_str_for_display(&entry.display_label),
                    status: sanitize_str_for_display(&status),
                    enabled: !entry.disabled,
                }
            })
            .collect();
    });

    let table = NSTableView::new(mtm);
    add_column(&table, COL_ENABLED, "Enabled", 60.0, mtm);
    add_column(&table, "plugin", "Plugin", 260.0, mtm);
    add_column(&table, COL_STATUS, "Status", 180.0, mtm);
    table.setColumnAutoresizingStyle(
        NSTableViewColumnAutoresizingStyle::LastColumnOnlyAutoresizingStyle,
    );
    table.setRowHeight(22.0);
    // `source` is held in a local for the whole modal on purpose:
    // `setDataSource:` and `setDelegate:` are **weak**, and the
    // checkboxes' target is weak too, so nothing else keeps it alive.
    // Dropping it before `runModal` returns would leave the table
    // messaging a freed object on its next layout pass.
    let source = ManagerSource::new(mtm);
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
        table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
    }

    let scroll = NSScrollView::new(mtm);
    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(MANAGER_WIDTH, MANAGER_TABLE_HEIGHT),
    ));
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(NSBorderType::BezelBorder);
    scroll.setDocumentView(Some(&table));
    table.reloadData();

    let hint = NSTextField::labelWithString(
        &NSString::from_str(
            "Enabling or disabling a plugin takes effect the next time Code++ starts.",
        ),
        mtm,
    );
    hint.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));

    let stack = NSStackView::new(mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setSpacing(8.0);
    stack.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(MANAGER_WIDTH, MANAGER_TABLE_HEIGHT + 28.0),
    ));
    stack.addArrangedSubview(&scroll);
    stack.addArrangedSubview(&hint);

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Plugin Manager"));
    alert.setAccessoryView(Some(&stack));
    alert.addButtonWithTitle(&NSString::from_str("Close"));
    alert.runModal();
    // `source` is still alive here, which is the point — see above.
    drop(source);

    // Drop the snapshot: it addresses registry entries by index, and
    // leaving it live past the modal would let a stale row answer a
    // later table query. Nothing else reads it, so this is hygiene
    // rather than a fix — but it is the same "an index outlives the
    // list it indexes" shape this project has been bitten by before.
    MANAGER_ROWS.with(|rows| rows.borrow_mut().clear());
}

/// Append a fixed-width column to the manager's table.
fn add_column(
    table: &NSTableView,
    identifier: &str,
    title: &str,
    width: f64,
    mtm: MainThreadMarker,
) {
    let column = NSTableColumn::initWithIdentifier(
        NSTableColumn::alloc(mtm),
        &NSString::from_str(identifier),
    );
    column.setTitle(&NSString::from_str(title));
    column.setWidth(width);
    column.setMinWidth(40.0);
    table.addTableColumn(&column);
}

#[cfg(test)]
mod shortcut_tests {
    use super::chord_menu_suffix;

    #[test]
    fn chord_menu_suffix_formats_macos_glyphs() {
        // Ctrl(⌘)+Alt(⌥)+H, in ⌥⇧⌘ order with ⌘ adjacent to the key.
        assert_eq!(
            chord_menu_suffix(true, true, false, 0x48),
            "\u{2325}\u{2318}H"
        );
        // Shift+F3 (bare of ⌘) → ⇧F3.
        assert_eq!(chord_menu_suffix(false, false, true, 0x72), "\u{21E7}F3");
        // Plain ⌘ and a named key.
        assert_eq!(chord_menu_suffix(true, false, false, 0x31), "\u{2318}1");
        assert_eq!(chord_menu_suffix(true, false, false, 0x2E), "\u{2318}Del");
    }
}
