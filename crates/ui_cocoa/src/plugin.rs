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
//!    plugin built from its `FuncItem`s, plus the two admin entries. The
//!    plugins' own check marks (`NPPM_SETMENUITEMCHECK`, `_init2Check`)
//!    are recorded by command id and painted from `validateMenuItem:`,
//!    since the menu is rebuilt on every open ([`set_menu_check`]).
//! 3. **The Plugin Manager** modal.
//! 4. **Notification delivery** — draining the shell's queued `NPPN_*`
//!    notifications to every loaded plugin's `beNotified`, and the
//!    shutdown pair ([`notify_shutdown`]).
//! 5. **Plugin dock panels** — `NPPM_DMMREGASDCKDLG` adopts the plugin's
//!    `NSView` as a dock panel's content ([`register_dock_dialog`]), the
//!    `DMN_*` notifications about it go to the plugin's `messageProc`,
//!    and the startup pass brings back the panels a session left open by
//!    running each one's own command ([`restore_panel_plugins`]).
//! 6. **What a plugin asks the host to make for it** — a Scintilla view
//!    of its own ([`create_plugin_scintilla`]), a toolbar button for one
//!    of its commands ([`add_toolbar_icon`]) — and the modeless-dialog
//!    registration Win32 needs and this platform does not
//!    ([`register_modeless_dialog`]).
//!
//! # Routing is by handle *identity*, not by message range
//!
//! `SCI_*` and `NPPM_*` message numbers overlap, so the number alone
//! cannot say where a message belongs — only the handle can.
//! [`NPP_SENTINEL`]'s address is this backend's "npp handle" and routes
//! to the host dispatcher; the host's own `ScintillaView*`, and the ones
//! it made for plugins, route to Scintilla; **every other pointer is
//! refused**. That last clause is
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
//! [`VALID_SCI`] and [`PLUGIN_SCIS`] are deliberately atomics rather
//! than a `with_state` read for the same reason in reverse: the
//! identity check must still work when a plugin sends `SCI_*` from
//! inside a `beNotified` that does hold the borrow, where a
//! `with_state` read would be declined — and a declined read here would
//! read as "not our view" and **refuse a legitimate message**.
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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, sel, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSBorderType, NSButton, NSControlTextEditingDelegate, NSFont, NSImage,
    NSLineBreakMode, NSMenu, NSMenuItem, NSScrollView, NSStackView, NSTableColumn, NSTableView,
    NSTableViewColumnAutoresizingStyle, NSTableViewDataSource, NSTableViewDelegate, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};

use codepp_core::dock::DockPanel;
use codepp_editor::EditorHandle;
use codepp_plugin_host::{
    HostDispatchFn, NppData, PluginMenuChecks, PluginMessageProc, SCNotification, WM_NOTIFY,
};
use codepp_scintilla_sys::{
    scintilla_cocoa_new, scintilla_cocoa_send_message, scintilla_cocoa_set_notify_callback,
    COCOA_WM_NOTIFY, SCI_GETMODIFY, SCI_SETCODEPAGE, SCN_PAINTED, SC_CP_UTF8,
};
use codepp_shell::{sanitize_str_for_display, HostHandles};

use crate::menu::Actions;
use crate::state::with_state;

/// The one legitimate `ScintillaView*`, cached so [`plugin_dispatch`]
/// can identity-check the handle a plugin routes an `SCI_*` message to.
/// Set once at startup by [`discover`]; see the module docs for why it
/// is an atomic and not a `with_state` read.
///
/// The Document Map's miniature view is deliberately **not** here, nor
/// in [`PLUGIN_SCIS`]: a plugin is only ever handed
/// `NppData._scintillaMainHandle` and the views it asked the host to
/// make, so a message addressed to the miniature did not come from
/// anywhere legitimate.
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

/// Whether `hwnd` is the host's own Scintilla view.
fn is_valid_scintilla(hwnd: *mut c_void) -> bool {
    let valid = VALID_SCI.load(Ordering::Acquire);
    !valid.is_null() && std::ptr::eq(hwnd, valid)
}

/// The most Scintilla views the host makes for plugins, all told. Each
/// is kept for the rest of the process (see [`create_plugin_scintilla`]),
/// so an unbounded number would be an unbounded leak — and a fixed number
/// is what lets [`PLUGIN_SCIS`] be read from any thread without a lock.
const MAX_PLUGIN_SCINTILLAS: usize = 64;

/// The most the host makes for any one plugin, so a plugin that asks for
/// a view per file it processes runs out of views of its own rather than
/// of everyone's — the same reasoning as the per-plugin quota on dock
/// panels. Views asked for from outside any host call, where the host
/// cannot tell which plugin asked, share one allowance of this size.
const MAX_PLUGIN_SCINTILLAS_PER_PLUGIN: usize = 16;

/// Every Scintilla view made for a plugin, in the order made: the handles
/// besides the host's own that [`plugin_dispatch`] forwards `SCI_*` to.
///
/// Append-only, which is what makes reading it from any thread sound. A
/// slot is written once, on the main thread, before [`PLUGIN_SCI_COUNT`]
/// publishes it with release ordering, and the view it names is never
/// released — so a reader that saw the count sees the pointer, and the
/// pointer stays valid. Atomics rather than a `with_state` read for the
/// reason [`VALID_SCI`] is one.
static PLUGIN_SCIS: [AtomicPtr<c_void>; MAX_PLUGIN_SCINTILLAS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_PLUGIN_SCINTILLAS];

/// How many slots of [`PLUGIN_SCIS`] are filled.
static PLUGIN_SCI_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Whether `hwnd` is a Scintilla view the host made for a plugin.
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

/// Whether `hwnd` is a Scintilla view this host made — its own, or one
/// it made for a plugin. These are the only pointers [`plugin_dispatch`]
/// forwards an `SCI_*` message to.
fn is_known_scintilla(hwnd: *mut c_void) -> bool {
    is_valid_scintilla(hwnd) || is_plugin_scintilla(hwnd)
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
// [`is_known_scintilla`]: the host's own `ScintillaView*`, or one it made
// for a plugin. Neither is ever released. The host's is created once at
// startup and never destroyed, removed from its superview or reassigned
// (the discipline `CocoaUiState::sci_view` documents and a source-scan
// guard enforces); a plugin's keeps the reference `scintilla_cocoa_new`
// returned for the rest of the process, whatever the plugin does with the
// view (see [`create_plugin_scintilla`]). So the address stays live for
// the whole process. It is *dereferenced only on the main thread*, which
// is the entire point of the marshal — the value crosses threads, the
// dereference does not.
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
            // SAFETY: `ptr.0` passed `is_known_scintilla` on the calling
            // thread and addresses a permanently-live `ScintillaView*` of
            // the host's making (see `MainThreadPtr`). This block runs on
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
        } else if is_known_scintilla(hwnd) {
            // SCI_* addressed to a Scintilla view of ours — the host's
            // own, or one made for a plugin. `with_state` is
            // deliberately not taken: this is a direct Scintilla call,
            // and the plugin may well issue it from inside an NPPM
            // dispatch that already holds the borrow. The identity check
            // above is an atomic read for the same reason, and it runs
            // *before* the affinity check so an unknown handle is refused
            // rather than marshaled.
            if on_main_thread() {
                // SAFETY: `hwnd` is identity-checked to be a live
                // `ScintillaView*` of the host's making, which is never
                // released (see `MainThreadPtr`), this is the thread that
                // owns it, and `scintilla_cocoa_send_message` is its
                // documented entry point. The message-argument contract is
                // the plugin's responsibility, exactly as it is on Win32.
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
    // A `NPPM_DMM*` handler may have registered, shown, hidden or
    // re-activated a plugin panel. The *model* can change inside the
    // dispatch; the view tree cannot, because the reconcile re-lays the
    // chrome and syncs the session through `with_state` and would be
    // declined under the live borrow. So the handler marks and the
    // reconcile happens here, with the borrow ended — the same shape as
    // `needs_rebind` above, as Win32's `dock_dirty` and as GTK's. It also
    // sends the `DMN_DOCK` / `DMN_FLOAT` a registration owes the plugin,
    // before its `SendMessageW` returns.
    if crate::dock::take_dirty() {
        crate::dock::apply_layout();
    }
    // `Some` means the dispatch ran on the main thread with the borrow
    // now dropped, so a prompt it queued — the export Save-As, or
    // `NPPM_RELOADBUFFERID`'s reload confirmation — can be presented
    // before the plugin's `SendMessageW` returns, which is when
    // Notepad++ shows the same prompts.
    crate::present_deferred_dialogs();
    routed
}

// --- plugin dock panels ------------------------------------------------------------
//
// What `NPPM_DMMREGASDCKDLG` means on this backend. On Windows a plugin
// hands the host its dialog's `HWND`; a recompiled macOS plugin hands it
// the AppKit analogue — an `NSView*` it built, in no superview — and the
// host adopts that view as a dock panel's content, exactly as the Win32
// host adopts a window and the GTK host a widget. The panel is then an
// ordinary dock panel (`crate::dock`). The one thing a view cannot do
// that a window can is receive a message, so the `DMN_*` notifications a
// Win32 plugin gets as `WM_NOTIFY` at its dialog's window procedure
// arrive here at the plugin's own `messageProc` instead, `wParam` naming
// the panel — the contract `ui_gtk` set, see
// `codepp_plugin_host::WM_NOTIFY`.

thread_local! {
    /// `DMN_*` notices a dock reconcile owes, waiting to be sent. See
    /// [`deliver_dock_notices`].
    static DOCK_NOTICES: RefCell<VecDeque<crate::dock::DockNotice>> =
        const { RefCell::new(VecDeque::new()) };
    /// Set while [`deliver_dock_notices`] is draining.
    static DOCK_NOTICES_DELIVERING: Cell<bool> = const { Cell::new(false) };
    /// Set while a `DMN_CLOSE` is being delivered. See
    /// [`close_plugin_panel`].
    static DMN_CLOSE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(debug_assertions)]
thread_local! {
    /// What [`deliver_dock_notices`] has sent, as `(panel, code)`, while
    /// the smoke binary is recording it — `None` otherwise, which is
    /// always outside that binary. Its rig drives a real dock with no
    /// shell behind it, so there is no plugin to deliver to; this is how
    /// it sees what each change owed. Compiled out of release builds with
    /// the rest of the test surface ([`smoke_support`]).
    static SENT_DOCK_NOTICES: RefCell<Option<Vec<(DockPanel, u32)>>> =
        const { RefCell::new(None) };
}

/// `NPPM_DMMREGASDCKDLG` on this backend: adopt the plugin's view as a
/// dock panel's content, returning the panel it interns to — for the
/// shell to record, and sign, the command that reopens it.
///
/// `hClient` must be an `NSView*` the plugin made and has not put in a
/// superview; the host takes its own strong reference and keeps it for
/// as long as the registration stands. The checks below refuse what can
/// be told apart without trusting the pointer — the npp handle, the
/// host's own Scintilla view — then what the Objective-C runtime can
/// tell: not a view, a view that already has a superview (which covers
/// every view of the host's own, a window's content view, and a view
/// registered twice), and one that belongs to a window, which catches the
/// one window-owned view without a superview, a window's frame view.
/// They are bug containment, not a boundary
/// (DESIGN.md §6.5): an arbitrary or dangling pointer cannot be told from
/// a live object — there is no `IsWindow` for memory — and faults in the
/// class check instead of being declined. A plugin runs in this process
/// and needs no help from the host to move a view anyway.
///
/// Registration does not show the panel — `NPPM_DMMSHOW` does, the ABI's
/// own split. The view goes into a clipping container of the host's
/// (`crate::dock::PluginPanelHost`), and that container joins the tree at
/// the reconcile the NPPM dispatch runs once its state borrow has ended.
pub(crate) fn register_dock_dialog(
    params: codepp_plugin_host::DockDialogParams,
) -> Option<DockPanel> {
    let handle = params.h_client;
    if let Err(why) = refuse_host_handle(handle) {
        tracing::warn!(why, "NPPM_DMMREGASDCKDLG: refused");
        return None;
    }
    // SAFETY: `handle` is non-null and not one of the host's own
    // non-view handles; by the ABI's contract on this backend it points
    // at a live Objective-C object. What happens when it does not is
    // the documented limit above: the class check reads its `isa`.
    let object: &AnyObject = unsafe { &*handle.cast::<AnyObject>() };
    let Some(view) = object.downcast_ref::<NSView>() else {
        tracing::warn!("NPPM_DMMREGASDCKDLG: refused: hClient is not an NSView");
        return None;
    };
    // SAFETY: a plain accessor on a live view, on the main thread — the
    // NPPM dispatch only ever runs there.
    if unsafe { view.superview() }.is_some() {
        tracing::warn!(
            "NPPM_DMMREGASDCKDLG: refused: hClient is already inside a view — \
             register a free-standing view"
        );
        return None;
    }
    // The superview check alone would let one kind of window-owned view
    // through: a window's frame view, the root its content view sits in,
    // has no superview.
    if view.window().is_some() {
        tracing::warn!("NPPM_DMMREGASDCKDLG: refused: hClient belongs to a window");
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
    // The host's own reference, taken only once the dock has ruled out
    // every refusal — so a refused registration leaves the plugin's view
    // as it found it.
    match crate::dock::register_plugin_panel(spec, || view.retain()) {
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
/// view has a superview and is refused by that check instead.
fn refuse_host_handle(handle: *mut c_void) -> Result<(), &'static str> {
    if handle.is_null() {
        return Err("hClient is null");
    }
    if std::ptr::eq(handle, npp_sentinel()) {
        return Err("hClient is the npp handle, not a view");
    }
    if is_valid_scintilla(handle) {
        return Err("hClient is the host's own Scintilla view");
    }
    Ok(())
}

/// The plugin's own tab icon: `tTbData.hIconTab`, which on this backend
/// is an `NSImage*`, honoured when `uMask` carries `DWS_ICONTAB`. The
/// host takes its own reference. `None` — the generic plugin glyph — for
/// no icon, or for something that is not an image.
fn tab_icon(params: &codepp_plugin_host::DockDialogParams) -> Option<Retained<NSImage>> {
    let icon = params.h_icon_tab;
    if params.u_mask & codepp_plugin_host::DWS_ICONTAB == 0 || icon.is_null() {
        return None;
    }
    // SAFETY: by the ABI's contract on this backend, `hIconTab` with
    // `DWS_ICONTAB` points at a live Objective-C object; see
    // `register_dock_dialog` for what the class check can and cannot
    // catch.
    let object: &AnyObject = unsafe { &*icon.cast::<AnyObject>() };
    let Some(image) = object.downcast_ref::<NSImage>() else {
        tracing::warn!(
            "NPPM_DMMREGASDCKDLG: hIconTab is not an NSImage; the tab shows the generic glyph"
        );
        return None;
    };
    Some(image.retain())
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

/// Close a plugin's panel from its group's ✕, telling the plugin first.
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
/// the coarse direction Win32's `DmnCloseGuard` and GTK's latch take for
/// the same reason.
pub(crate) fn close_plugin_panel(panel: DockPanel) {
    if let Some((handle, caller)) = crate::dock::plugin_panel_notify_target(panel) {
        if !DMN_CLOSE_ACTIVE.with(Cell::get) {
            let _closing = crate::FlagGuard::set(&DMN_CLOSE_ACTIVE);
            send_dock_notification(panel, handle, caller, codepp_plugin_host::DMN_CLOSE);
        }
    }
    crate::dock::set_panel_visible(panel, false);
}

/// The most notices one outermost [`deliver_dock_notices`] works through
/// before it gives up on the rest: three a panel — a container, a switch
/// and a relayout, the most one reconcile owes — for every registration
/// the panel table allows, four reconciles deep.
///
/// The queue keeps a plugin re-entering from its handler one level deep;
/// this keeps it from running forever. Plugins whose handlers keep
/// reversing the layout — two of them each bringing its own panel back to
/// the front whenever told it went behind, say — would otherwise hold
/// the loop, and the UI thread with it, for as long as they kept at it,
/// every handler's reconcile queuing more. Past the cap the rest of the
/// queue is dropped with a warning. That loses notices, since their
/// records are already written — but only in a cascade no plugin could
/// have kept up with, and the view tree, which never waits on a notice,
/// is right throughout.
const MAX_DOCK_NOTICES_PER_DELIVERY: usize = 4 * 3 * codepp_core::dock::MAX_PLUGIN_PANELS;

/// Send each notice a dock reconcile owes: `DMN_DOCK` / `DMN_FLOAT`,
/// `DMN_SWITCHIN` / `DMN_SWITCHOFF` and `DMN_FLOATDROPPED`, in the order
/// `crate::dock::panel_notices` queued them.
///
/// **Notices raised while one is being delivered are queued, not sent.**
/// The plugin's handler runs with no borrow held, so it may send
/// `NPPM_*` back — including `NPPM_DMMREGASDCKDLG` for a panel nothing
/// has been told about, which reconciles again from inside this loop and
/// raises a notice of its own. Sent there and then, that notice's handler
/// could do the same, nesting a full round trip per link until the
/// registration cap or the stack ran out. So a call made while a delivery
/// is running only appends to the queue and returns, and the outermost
/// call drains it in order: nothing is dropped that is still true, and
/// the nesting stays one level deep whatever the plugin does. The same
/// queue Win32's `deliver_container_notices` and GTK's keep; the cap
/// below is this backend's alone so far (DESIGN.md §7.4).
///
/// Queued behind a handler that may change the layout, a notice is
/// checked again when its turn comes (`crate::dock::notice_still_applies`)
/// and skipped if it no longer says something true — a view taken back,
/// a panel switched in and closed again before hearing of it. The record
/// is already written, so a skipped notice loses nothing that could
/// still be delivered.
///
/// The queue bounds depth, and [`MAX_DOCK_NOTICES_PER_DELIVERY`] bounds
/// time: past it, the rest of the queue is dropped.
pub(crate) fn deliver_dock_notices(notices: Vec<crate::dock::DockNotice>) {
    DOCK_NOTICES.with(|q| q.borrow_mut().extend(notices));
    if DOCK_NOTICES_DELIVERING.with(Cell::get) {
        return;
    }
    let _delivering = crate::FlagGuard::set(&DOCK_NOTICES_DELIVERING);
    let mut taken = 0usize;
    while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) {
        taken += 1;
        if taken > MAX_DOCK_NOTICES_PER_DELIVERY {
            let dropped = 1 + DOCK_NOTICES.with(|q| {
                let mut q = q.borrow_mut();
                let left = q.len();
                q.clear();
                left
            });
            tracing::warn!(
                dropped,
                "plugins keep changing the dock layout from their DMN_* handlers; \
                 dropping the notifications still queued"
            );
            break;
        }
        if !crate::dock::notice_still_applies(&notice) {
            continue;
        }
        #[cfg(debug_assertions)]
        SENT_DOCK_NOTICES.with(|sent| {
            if let Some(sent) = sent.borrow_mut().as_mut() {
                sent.push((notice.panel, notice.code));
            }
        });
        tracing::debug!(
            panel = notice.panel.persist_key(),
            dmn = codepp_plugin_host::docking::dmn_name(notice.code),
            container = notice.code >> 16,
            "dock panel notification"
        );
        send_dock_notification(notice.panel, notice.handle, notice.caller, notice.code);
    }
}

/// Deliver one `DMN_*` about `panel` to the plugin that should hear it —
/// see `Shell::plugin_panel_message_target` — through its `messageProc`:
/// `WM_NOTIFY`, `wParam` the panel's `hClient`, `lParam` an `NMHDR` from
/// the npp handle with `idFrom` 0 and `code` as given. Called with no
/// borrow held.
fn send_dock_notification(panel: DockPanel, handle: *mut c_void, caller: Option<usize>, code: u32) {
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
    // main thread with no state borrow held. `nmhdr` outlives the call,
    // and `wParam` is the handle the plugin itself registered the panel
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
// `HICON` on Windows; here the same argument is the AppKit object that
// plays the part — an `NSView*`, an `NSWindow*`, an `NSImage*` — as a
// dock panel's `hClient` is an `NSView*`. The checks on those pointers
// are bug containment, not a boundary (DESIGN.md §6.5): the handles the
// host can recognise without trusting the pointer come first, then what
// the Objective-C runtime can tell about the object, and a pointer to
// something that is not an object at all faults in the class check
// rather than being declined, as it does for a dock panel.

/// What the host knows about one Scintilla view it made for a plugin. Its
/// index in [`PLUGIN_SCINTILLAS`] is its slot in [`PLUGIN_SCIS`] and the
/// `windowid` its notifications arrive with.
struct PluginScintilla {
    /// The view, holding the reference `scintilla_cocoa_new` returned.
    /// Nothing ever releases it: a raw pointer rather than a `Retained`,
    /// so no destructor can — see [`create_plugin_scintilla`].
    view: *mut c_void,
    /// The plugin that asked for it — the one whose `messageProc` hears
    /// its notifications — or `None` when it was asked for from outside
    /// any host call, where the host cannot tell which plugin asked.
    owner: Option<usize>,
    /// Who hears its notifications.
    target: NotifyTarget,
}

/// Who hears the notifications of a view made for a plugin.
#[derive(Clone, Copy)]
enum NotifyTarget {
    /// Not looked up yet. It is looked up at a notification rather than
    /// when the view is made, because [`create_plugin_scintilla`] runs
    /// inside the NPPM dispatch's state borrow, where the plugin registry
    /// cannot be read — and it stays unresolved until the lookup finds the
    /// plugin loaded, since a plugin may make a view from its own `setInfo`,
    /// before it is.
    Unresolved,
    /// The `messageProc` of the plugin that asked for the view.
    Plugin(PluginMessageProc),
    /// No one: the view was asked for from outside any host call, where
    /// the host cannot tell which plugin asked.
    Nobody,
}

thread_local! {
    /// The views [`create_plugin_scintilla`] made, in the order made.
    /// Append-only, like [`PLUGIN_SCIS`], and main-thread only.
    static PLUGIN_SCINTILLAS: RefCell<Vec<PluginScintilla>> =
        const { RefCell::new(Vec::new()) };
    /// The label of every command the loaded plugins publish, by command
    /// id — the tooltip of a toolbar button for one of them. Filled with
    /// the menu-check record, and for the same reason: a toolbar button is
    /// added from inside the NPPM dispatch's borrow, where the plugins'
    /// `FuncItem`s cannot be read.
    static COMMAND_LABELS: RefCell<HashMap<i32, String>> = RefCell::new(HashMap::new());
}

/// `NPPM_CREATESCINTILLAHANDLE` on this backend: make a Scintilla view for
/// the plugin that asked, and return its handle, or null.
///
/// `parent` is the `NSView*` to put it in. The view goes in **hidden and
/// zero-sized**, for the plugin to size and show — the Win32 host creates
/// its control at zero size without `WS_VISIBLE`, for the same reason.
/// The npp handle as `parent` makes a view in no window, which a plugin
/// can drive through `SCI_*` alone — a text buffer with Scintilla's search
/// and styling — or put in a view of its own later.
///
/// The view is **kept for the rest of the process**. A plugin that
/// captured its direct-call pair (`SCI_GETDIRECTFUNCTION`) holds pointers
/// into it that nothing could invalidate safely, and Notepad++ keeps every
/// Scintilla it makes for plugins until it exits too. That is what makes
/// the routing check sound from any thread, and it is why the number is
/// capped ([`MAX_PLUGIN_SCINTILLAS`], [`MAX_PLUGIN_SCINTILLAS_PER_PLUGIN`]):
/// a plugin that asks for a view per file it processes would otherwise
/// leak without end.
///
/// Refused, besides null: a Scintilla view as the parent, the host's own
/// or a plugin's — a Scintilla has no room for a subview of anyone
/// else's; anything the runtime says is not a view; the host's container
/// around a plugin's docked panel — the panel's own view is what to pass;
/// and a view of the host's own, which is any view in the main window or a
/// floating dock window that is not inside a plugin's docked panel.
///
/// Its notifications go to the plugin's `messageProc` — see
/// [`on_plugin_sci_notify`]. Its scrollers are permanent, as the host's
/// own view's are and a Win32 plugin Scintilla's are.
///
/// It does not get the host view's scroll-width floor
/// (`clamp_scroll_width_to_viewport`): that keeps state per view, and
/// with width tracking on, the blank area right of a short line is not
/// clickable here (DESIGN.md §7.4).
pub(crate) fn create_plugin_scintilla(parent: *mut c_void, main_window: &NSWindow) -> *mut c_void {
    let owner = codepp_plugin_host::calling_plugin();
    let into = match plugin_scintilla_parent(parent, main_window) {
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
    // SAFETY: the NPPM dispatch runs only on the main thread, after
    // `NSApplication` exists — `scintilla_cocoa_new`'s preconditions.
    let ptr = unsafe { scintilla_cocoa_new() };
    if ptr.is_null() {
        tracing::warn!("NPPM_CREATESCINTILLAHANDLE: scintilla_cocoa_new() returned null");
        return std::ptr::null_mut();
    }
    // SAFETY: a live `ScintillaView`, an `NSView` subclass, whose +1
    // reference is never given up — which is also what makes the borrow
    // outlive this function.
    let view: &NSView = unsafe { &*ptr.cast::<NSView>() };
    // For the reason the host's own view clips: on recent macOS the
    // scroll view's edge effect is sized past the view, and AppKit does
    // not clip a subview to its parent by default.
    view.setClipsToBounds(true);
    view.setHidden(true);
    view.setFrame(NSRect::ZERO);
    // Permanent scrollers, as the host's own view has, repaired after each
    // of this view's paints — see `forward_plugin_sci_notify`.
    crate::force_permanent_scrollers(view);
    // Scintilla 5 is UTF-8 by default, but the host's text is UTF-8
    // throughout and the Win32 host sets it explicitly for the same
    // reason — so a future default cannot change what a plugin gets. The
    // host's own setup goes through `EditorHandle` like every other
    // message the host sends a view it made; the sends in this module are
    // the plugins' traffic.
    //
    // SAFETY: `ptr` is the live view just made, never released.
    if let Some(editor) = unsafe { EditorHandle::from_cocoa_view(ptr) } {
        editor.send(SCI_SETCODEPAGE, SC_CP_UTF8 as usize, 0);
    }
    // Recorded and made routable before it goes anywhere, so a view the
    // host failed to record is never left in a plugin's view. Nothing
    // between the check above and this push can make another view — making
    // one takes a plugin's call, and none runs in between — so neither the
    // borrow nor the routing slot can fail; were either ever to, the plugin
    // is told it got no view.
    let Some(index) = PLUGIN_SCINTILLAS.with(|made| {
        let mut made = made.try_borrow_mut().ok()?;
        made.push(PluginScintilla {
            view: ptr,
            owner,
            target: if owner.is_some() {
                NotifyTarget::Unresolved
            } else {
                NotifyTarget::Nobody
            },
        });
        Some(made.len() - 1)
    }) else {
        tracing::error!("NPPM_CREATESCINTILLAHANDLE: the view could not be recorded");
        return std::ptr::null_mut();
    };
    let Some(slot) = PLUGIN_SCIS.get(index) else {
        tracing::error!(
            index,
            "NPPM_CREATESCINTILLAHANDLE: no routing slot for the view"
        );
        return std::ptr::null_mut();
    };
    slot.store(ptr, Ordering::Relaxed);
    PLUGIN_SCI_COUNT.store(index + 1, Ordering::Release);
    if let Some(parent) = into {
        parent.addSubview(view);
    }
    // SAFETY: `ptr` is the live view just made; `on_plugin_sci_notify`
    // matches `SciNotifyFunc` and cannot unwind out of it.
    unsafe {
        scintilla_cocoa_set_notify_callback(ptr, on_plugin_sci_notify, index as isize);
    }
    tracing::debug!(
        plugin = owner,
        index,
        "NPPM_CREATESCINTILLAHANDLE: made a view"
    );
    ptr
}

/// Where a view made for a plugin goes: `Ok(None)` for the npp handle — no
/// window at all — or the view to add it to, or why `parent` is refused.
/// See [`create_plugin_scintilla`] for what is refused.
fn plugin_scintilla_parent(
    parent: *mut c_void,
    main_window: &NSWindow,
) -> Result<Option<Retained<NSView>>, &'static str> {
    if parent.is_null() {
        return Err("the parent is null");
    }
    if std::ptr::eq(parent, npp_sentinel()) {
        return Ok(None);
    }
    if is_known_scintilla(parent) {
        return Err("the parent is a Scintilla view");
    }
    // SAFETY: not one of the host's non-object handles; by the ABI's
    // contract on this backend a parent is a live `NSView`. What happens
    // when it is not is the limit in the section notes above.
    let object: &AnyObject = unsafe { &*parent.cast::<AnyObject>() };
    let Some(view) = object.downcast_ref::<NSView>() else {
        return Err("the parent is not an NSView");
    };
    if view
        .downcast_ref::<crate::dock::PluginPanelHost>()
        .is_some()
    {
        return Err(
            "the parent is the host's container around a plugin panel — pass the panel's own view",
        );
    }
    if let Some(window) = view.window() {
        if is_host_window(&window, main_window) && !inside_plugin_panel(view) {
            return Err("the parent is one of the host's own views");
        }
    }
    Ok(Some(view.retain()))
}

/// Whether `window` is one of the host's windows a plugin's content sits
/// in: the main window, or a floating dock window. What a plugin can
/// reach of them is its own docked panel's window — `[view window]` — and
/// passing that for a window of its own is the mistake the callers name.
fn is_host_window(window: &NSWindow, main_window: &NSWindow) -> bool {
    std::ptr::eq(window, main_window) || window.downcast_ref::<crate::dock::FloatWindow>().is_some()
}

/// Whether `view` is a plugin's docked panel or inside one: some view
/// above it is the host's container around a panel.
fn inside_plugin_panel(view: &NSView) -> bool {
    // SAFETY (both): a plain accessor on a live view, on the main thread.
    let mut above = unsafe { view.superview() };
    while let Some(ancestor) = above {
        if ancestor
            .downcast_ref::<crate::dock::PluginPanelHost>()
            .is_some()
        {
            return true;
        }
        above = unsafe { ancestor.superview() };
    }
    false
}

/// Whether one more view may be made for `owner`, given the owners of
/// every view made so far. See [`MAX_PLUGIN_SCINTILLAS`] and
/// [`MAX_PLUGIN_SCINTILLAS_PER_PLUGIN`].
fn may_make_plugin_scintilla(
    made: &[Option<usize>],
    owner: Option<usize>,
) -> Result<(), &'static str> {
    if made.len() >= MAX_PLUGIN_SCINTILLAS {
        return Err("the host has made as many Scintilla views for plugins as it makes");
    }
    if made.iter().filter(|&&by| by == owner).count() >= MAX_PLUGIN_SCINTILLAS_PER_PLUGIN {
        return Err(
            "the host has made as many Scintilla views for this plugin as it makes for one",
        );
    }
    Ok(())
}

/// Scintilla's notification callback for the views made for plugins.
///
/// Each `WM_NOTIFY` goes on to the plugin that asked for the view, at its
/// `messageProc`, as `WM_NOTIFY` — where a Win32 plugin's Scintilla child
/// sends it to the plugin's dialog procedure, which a view does not have.
/// The same generalisation the `DMN_*` take. `lParam` is a copy of the
/// `SCNotification` with `nmhdr.hwndFrom` set to the view's handle, which
/// is how a plugin tells its views apart — the Cocoa backend puts its own
/// C++ object there, which means nothing to a plugin. `wParam` is what
/// Scintilla gives: the view's control identifier (`SCI_SETIDENTIFIER`,
/// 0 unless the plugin sets one), as a Win32 `WM_NOTIFY` carries.
///
/// `SCN_PAINTED` also repairs the view's scroller layout first, as the
/// host's own view's is repaired: the permanent scrollers
/// [`create_plugin_scintilla`] gives it are what the vendored `tile`
/// lays out wrong — see `enforce_scroller_layout`.
///
/// # Safety
///
/// Called by Scintilla on the main thread. `lparam` is an
/// `SCNotification*` when `message` is `COCOA_WM_NOTIFY`, live for the
/// call.
unsafe extern "C" fn on_plugin_sci_notify(
    windowid: isize,
    message: u32,
    wparam: usize,
    lparam: usize,
) {
    // Plain `extern "C"`, so an escaping panic is undefined behaviour.
    crate::at_callback_boundary("plugin:sci_notify", (), || {
        // SAFETY: the enclosing signature's contract, unchanged.
        unsafe { forward_plugin_sci_notify(windowid, message, wparam, lparam) }
    });
}

/// The body of [`on_plugin_sci_notify`], so the panic guard wraps it
/// whole.
///
/// # Safety
///
/// As [`on_plugin_sci_notify`].
unsafe fn forward_plugin_sci_notify(windowid: isize, message: u32, wparam: usize, lparam: usize) {
    if message != COCOA_WM_NOTIFY || lparam == 0 {
        return;
    }
    let Some(index) = usize::try_from(windowid).ok() else {
        return;
    };
    let Some(view) = plugin_scintilla_view(index) else {
        return;
    };
    // SAFETY: for `WM_NOTIFY` the Cocoa backend passes its live
    // `NotificationData`, which `SCNotification` mirrors field for field.
    // Copied, so the host never writes into Scintilla's own.
    let mut scn = unsafe { (lparam as *const SCNotification).read() };
    if scn.nmhdr.code == SCN_PAINTED {
        if let Some(mtm) = MainThreadMarker::new() {
            // SAFETY: a view made for a plugin, never released.
            crate::enforce_scroller_layout(unsafe { &*view.cast::<NSView>() }, mtm);
        }
    }
    let Some(target) = plugin_scintilla_target(index) else {
        return;
    };
    scn.nmhdr.hwnd_from = view;
    // SAFETY: a loaded plugin's `messageProc` — plugins are never
    // unloaded — run as that plugin, on the main thread. `scn` outlives
    // the call.
    let _ = unsafe { target.send(WM_NOTIFY, wparam, &raw const scn as isize) };
}

/// The view at `index` in [`PLUGIN_SCINTILLAS`].
fn plugin_scintilla_view(index: usize) -> Option<*mut c_void> {
    PLUGIN_SCINTILLAS.with(|made| made.try_borrow().ok()?.get(index).map(|s| s.view))
}

/// The `messageProc` that hears the notifications of the view at `index`,
/// looked up on first use and kept once found.
///
/// The borrow of [`PLUGIN_SCINTILLAS`] is never held across the lookup or
/// the plugin call, so a plugin that makes another view from inside its
/// handler finds the registry free. A lookup that finds nothing is tried
/// again at the next notification: `with_state` declines one made from
/// inside a host borrow, and a plugin not yet loaded — one that made the
/// view from its `setInfo` — has no `messageProc` to find until it is.
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
/// **Registering changes nothing here, and that is not a gap.** What the
/// Win32 registration buys a dialog is the host's message pump calling
/// `IsDialogMessage` for it — Tab moving between its controls, Enter
/// pressing its default button — and so keeping the host's accelerators
/// out of it. AppKit does the first for every window, and the second has
/// nothing to keep out: a plugin shortcut fires only while the main
/// window is key (`install_plugin_shortcut_monitor`), and on macOS the
/// menu bar's key equivalents reach every window of an application by
/// design.
///
/// A dialog is checked on the way in only. Its removal is answered
/// without looking at what the pointer points at: a plugin removes its
/// dialog on the way to releasing it, when the pointer is least
/// trustworthy, and there is nothing to undo.
pub(crate) fn register_modeless_dialog(
    dlg: *mut c_void,
    register: bool,
    main_window: &NSWindow,
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
    // contract on this backend the dialog is a live `NSWindow`.
    let object: &AnyObject = unsafe { &*dlg.cast::<AnyObject>() };
    let Some(window) = object.downcast_ref::<NSWindow>() else {
        tracing::warn!("NPPM_MODELESSDIALOG: refused: not an NSWindow");
        return false;
    };
    if is_host_window(window, main_window) {
        tracing::warn!("NPPM_MODELESSDIALOG: refused: one of the host's own windows");
        return false;
    }
    tracing::debug!("NPPM_MODELESSDIALOG: registered (nothing to route on this backend)");
    true
}

/// `NPPM_ADDTOOLBARICON` on this backend: a toolbar button that runs the
/// plugin command `cmd_id`, showing `icon` — an `NSImage*`, which the host
/// retains, so the plugin may release its own reference once this
/// returns. The button's tooltip is the command's menu label, and it
/// shows the command's check mark (`NPPM_SETMENUITEMCHECK`) as pressed,
/// as Notepad++'s toolbar does.
///
/// Refused for an id that is not a command a loaded plugin published:
/// the button runs its command through the plugin commands' own path,
/// which knows no other. Asking again for the same command replaces the
/// image rather than adding a second button.
pub(crate) fn add_toolbar_icon(
    toolbar: &crate::toolbar::Toolbar,
    cmd_id: i32,
    icon: *mut c_void,
) -> bool {
    if icon.is_null() || std::ptr::eq(icon, npp_sentinel()) || is_known_scintilla(icon) {
        tracing::warn!(
            cmd_id,
            "NPPM_ADDTOOLBARICON: refused: not an image's handle"
        );
        return false;
    }
    let Some(label) = COMMAND_LABELS.with(|labels| labels.borrow().get(&cmd_id).cloned()) else {
        tracing::warn!(
            cmd_id,
            "NPPM_ADDTOOLBARICON: refused: no loaded plugin publishes that command"
        );
        return false;
    };
    // SAFETY: not one of the host's non-object handles; by the ABI's
    // contract on this backend the icon is a live `NSImage`.
    let object: &AnyObject = unsafe { &*icon.cast::<AnyObject>() };
    let Some(image) = object.downcast_ref::<NSImage>() else {
        tracing::warn!(cmd_id, "NPPM_ADDTOOLBARICON: refused: not an NSImage");
        return false;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    match toolbar.add_plugin_button(cmd_id, image, &label, menu_mark(cmd_id), mtm) {
        Ok(()) => true,
        Err(why) => {
            tracing::warn!(cmd_id, why, "NPPM_ADDTOOLBARICON: refused");
            false
        }
    }
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

    /// Plugin dock panels, driven for real: a dock installed around a
    /// window that is never shown, and real `NSView`s registered through
    /// `NPPM_DMMREGASDCKDLG`'s handler. Panics on the first failed check.
    ///
    /// It needs no `CocoaUiState`: every step that would reach the shell —
    /// the chrome relayout, the session sync, the `DMN_*` delivery — finds
    /// none installed and is skipped, which is what lets the dock be
    /// driven on its own. What it pins is what no source scan can see: the
    /// adopted view really ends up in the dock area's tree (the m4d lesson
    /// — a detached view answers every other question correctly), the
    /// refusals refuse, a hidden panel keeps its registration, and a
    /// plugin taking its view back ends the registration through the
    /// container's `willRemoveSubview:` and the default-mode sweep.
    pub fn plugin_panels_are_hosted_by_the_dock() {
        use objc2::rc::Retained;

        let mtm = objc2_foundation::MainThreadMarker::new()
            .expect("the smoke binary owns the main thread");
        let rig = panel_scenario::Rig::install(mtm);
        let view = panel_scenario::plugin_view(mtm);
        let handle = panel_scenario::handle_of(&view);
        let panel = super::register_dock_dialog(rig.params(handle, "Smoke Panel"))
            .expect("a free-standing view is adopted");
        panel_scenario::refusals_refuse(&rig, handle);

        // Shown: hosted all the way up to the dock area.
        assert!(crate::dock::show_plugin_panel(handle));
        panel_scenario::reconcile();
        let host = panel_scenario::assert_hosted(&view, &rig.area);

        // A second panel asking for the same container becomes a second
        // tab of the same group — which is what makes the hide below
        // mean something: the group outlives the hidden panel.
        let notes = panel_scenario::plugin_view(mtm);
        let notes_handle = panel_scenario::handle_of(&notes);
        assert!(super::register_dock_dialog(rig.params(notes_handle, "Smoke Notes")).is_some());
        assert!(crate::dock::show_plugin_panel(notes_handle));
        panel_scenario::reconcile();
        let notes_host = panel_scenario::assert_hosted(&notes, &rig.area);
        // SAFETY (here and in the helpers): plain accessors on live
        // views, on the main thread.
        let same_slot = unsafe { host.superview() }
            .zip(unsafe { notes_host.superview() })
            .is_some_and(|(a, b)| Retained::as_ptr(&a) == Retained::as_ptr(&b));
        assert!(
            same_slot,
            "two panels asking for one container are not tabs of one group"
        );

        // Hidden: its container goes to parking — not left in the slot it
        // shares, where the group's layout would size it over the tab in
        // front — and the registration stays.
        assert!(crate::dock::hide_plugin_panel(handle));
        panel_scenario::reconcile();
        let parked_in = unsafe { host.superview() }.expect("parked, not dropped");
        assert!(
            parked_in.isHidden(),
            "a hidden panel is not in the parking view"
        );
        assert!(crate::dock::is_plugin_panel_registered(panel));
        assert!(crate::dock::show_plugin_panel(handle));
        panel_scenario::reconcile();

        panel_scenario::taking_the_view_back_ends_the_registration(&view, &host, panel);

        // And the plugin can register the same view again.
        assert!(super::register_dock_dialog(rig.params(handle, "Smoke Panel")).is_some());
        assert!(crate::dock::is_plugin_panel_registered(panel));

        let replacement =
            panel_scenario::a_replacement_before_the_sweep_keeps_the_panel(&rig, &view, panel, mtm);
        // Kept for the process, like every view this binary makes.
        std::mem::forget((rig, view, notes, replacement));
    }

    /// What a plugin is told about how its panels are shown, driven
    /// against real views in a real dock: `DMN_SWITCHIN` /
    /// `DMN_SWITCHOFF` as tabs come and go, `DMN_FLOATDROPPED` as groups
    /// are laid out anew. The rig has no plugin to deliver to, so the
    /// notices are recorded as they are sent. Panics on the first failed
    /// check.
    ///
    /// What it pins is what `codepp_plugin_host::docking`'s unit tests
    /// cannot see: every reconcile records each live registration and
    /// sends in the policy's order; a resize outside the model is never
    /// reported from inside a layout pass, only by the check the run loop
    /// runs in its default mode — once, however many passes the resize
    /// took; a floating window AppKit moves is reported through its
    /// delegate; and one delivery stops at its cap.
    pub fn plugin_panels_hear_how_they_are_shown() {
        use codepp_core::dock::{DockRect, DropTarget};
        use codepp_plugin_host::{
            CONT_BOTTOM, DMN_DOCK, DMN_FLOAT, DMN_FLOATDROPPED as RELAID, DMN_SWITCHIN as IN,
            DMN_SWITCHOFF as OFF, DOCKCONT_MAX,
        };
        use panel_scenario::{expect_nothing_more, expect_sent, expect_sent_now, handle_of};

        let mtm = objc2_foundation::MainThreadMarker::new()
            .expect("the smoke binary owns the main thread");
        let rig = panel_scenario::Rig::install(mtm);
        // Whatever an earlier scenario left queued on the run loop runs
        // now, against the fresh dock, before anything is recorded.
        panel_scenario::run_default_mode();
        panel_scenario::record_notices();
        let (first, second) = (
            panel_scenario::plugin_view(mtm),
            panel_scenario::plugin_view(mtm),
        );
        let a = panel_scenario::register_apart(&rig, &first, "Notice A");
        let b = panel_scenario::register_apart(&rig, &second, "Notice B");

        // Registered: told the container, and nothing else — neither is
        // on screen. The layout pass that follows moves nothing.
        panel_scenario::reconcile();
        let docked_bottom = (CONT_BOTTOM << 16) | DMN_DOCK;
        expect_sent_now(&[(a, docked_bottom), (b, docked_bottom)], "registration");
        expect_nothing_more("a relayout that moved nothing");

        assert!(crate::dock::show_plugin_panel(handle_of(&first)));
        panel_scenario::reconcile();
        expect_sent_now(&[(a, IN), (a, RELAID)], "the first panel shown");

        // The panel coming in before the one going out, then both relaid
        // out: the tab bar the second tab brings takes height from both.
        assert!(crate::dock::show_plugin_panel(handle_of(&second)));
        panel_scenario::reconcile();
        expect_sent_now(
            &[(b, IN), (a, OFF), (a, RELAID), (b, RELAID)],
            "a second tab",
        );

        assert!(crate::dock::show_plugin_panel(handle_of(&first)));
        panel_scenario::reconcile();
        expect_sent_now(&[(a, IN), (b, OFF)], "a tab switch, which lays nothing out");

        // The closed panel hears nothing; the other comes in, relaid out
        // without the tab bar.
        assert!(crate::dock::hide_plugin_panel(handle_of(&first)));
        panel_scenario::reconcile();
        expect_sent_now(&[(b, IN), (b, RELAID)], "the front tab closed");

        // Reopened, it gets a group of its own in the same band, which
        // halves the other's.
        assert!(crate::dock::show_plugin_panel(handle_of(&first)));
        panel_scenario::reconcile();
        expect_sent_now(
            &[(a, IN), (a, RELAID), (b, RELAID)],
            "a second group in the band",
        );
        expect_nothing_more("settled");

        // Resized outside the model, three layout passes in a row: nothing
        // from inside them, then one `DMN_FLOATDROPPED` a panel.
        crate::dock::layout_area(1000.0, 600.0);
        crate::dock::layout_area(1000.0, 550.0);
        crate::dock::layout_area(1000.0, 500.0);
        expect_sent_now(&[], "a layout pass tells the plugins nothing itself");
        expect_sent(
            &[(a, RELAID), (b, RELAID)],
            "a resize, once the run loop is back",
        );
        crate::dock::layout_area(1000.0, 500.0);
        expect_nothing_more("the same size again");
        crate::dock::layout_area(1000.0, 700.0);
        expect_sent(&[(a, RELAID), (b, RELAID)], "the size restored");

        // Floated: a new container and a new placement, and no switch — it
        // never left the screen. The one left in the band is relaid out.
        assert!(crate::dock::update_layout(|l| {
            l.move_panel(b, DropTarget::Floating(DockRect::new(240, 200, 320, 240)));
            true
        }));
        crate::dock::apply_layout();
        let floating = (DOCKCONT_MAX << 16) | DMN_FLOAT;
        expect_sent_now(&[(b, floating), (a, RELAID), (b, RELAID)], "floated");
        // Whatever AppKit made of the rect on screen, settled before the
        // next step measures anything.
        panel_scenario::run_default_mode();
        let _ = panel_scenario::take_notices();

        // Moved by AppKit rather than by the dock: its delegate reports it.
        let window = second.window().expect("a floating panel is in a window");
        let origin = window.frame().origin;
        window.setFrameOrigin(objc2_foundation::NSPoint::new(
            origin.x + 40.0,
            origin.y - 30.0,
        ));
        expect_sent_now(&[], "the delegate tells the plugins nothing itself");
        expect_sent(&[(b, RELAID)], "a floating window moved");

        panel_scenario::a_runaway_delivery_is_cut_off(&first, a);
        panel_scenario::stale_notices_are_not_sent(&first, a, &second, b);
        std::mem::forget((rig, first, second));
    }

    /// What a plugin asks the host to make — `NPPM_CREATESCINTILLAHANDLE`,
    /// `NPPM_MODELESSDIALOG`, `NPPM_ADDTOOLBARICON` — driven against real
    /// AppKit objects, with a dock rig standing in for the main window.
    /// Panics on the first failed check.
    ///
    /// What it pins is what a source scan cannot see: a view made for a
    /// plugin lands where the plugin asked, hidden and zero-sized, and
    /// answers `SCI_*` routed to it from any thread; the parents, windows
    /// and images the host must not take are refused; and a plugin's
    /// toolbar button runs through the toolbar's own action and comes back
    /// showing the plugin's mark rather than AppKit's flip. Delivery of a
    /// plugin view's notifications needs a loaded plugin to deliver to,
    /// so the real app is where that is shown (DESIGN.md §7.4).
    ///
    /// # Safety
    ///
    /// `host_sci` must be a live `ScintillaView*` from
    /// `scintilla_cocoa_new` that stays live for the rest of the process:
    /// it is armed as the host's own view, as [`arm_scintilla`] requires.
    pub unsafe fn what_plugins_ask_the_host_to_make(host_sci: *mut c_void) {
        let mtm = objc2_foundation::MainThreadMarker::new()
            .expect("the smoke binary owns the main thread");
        // SAFETY: forwarded from this function's contract.
        unsafe { arm_scintilla(host_sci) };
        let rig = panel_scenario::Rig::install(mtm);
        let panel = made_for_plugins::scintilla_parents_are_checked(&rig, host_sci, mtm);
        made_for_plugins::a_plugin_panel_is_a_parent_and_its_container_is_not(&rig, panel, mtm);
        made_for_plugins::a_plugin_view_is_routed_from_any_thread(&rig.window, mtm);
        made_for_plugins::modeless_dialogs_are_checked_and_answered(&rig.window, host_sci, mtm);
        made_for_plugins::plugin_toolbar_buttons(mtm);
        // Kept for the process, like every view this binary makes.
        std::mem::forget(rig);
    }

    /// The pieces of [`what_plugins_ask_the_host_to_make`].
    mod made_for_plugins {
        use std::ffi::{c_void, CString};

        use objc2::rc::Retained;
        use objc2::{AnyThread, MainThreadOnly};
        use objc2_app_kit::{
            NSBackingStoreType, NSButton, NSImage, NSView, NSWindow, NSWindowStyleMask,
        };
        use objc2_foundation::{
            MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSObject, NSPoint, NSRect, NSRunLoop,
            NSSize,
        };

        use super::panel_scenario::{handle_of, plugin_view, reconcile, Rig};
        use codepp_scintilla_sys::{SCI_GETCODEPAGE, SCI_GETLENGTH, SCI_SETTEXT, SC_CP_UTF8};

        /// Stand-in for the plugin command a toolbar button runs.
        const SMOKE_CMD: i32 = 22_001;

        /// Ask for a view in `parent`, as the dispatcher would.
        fn make(parent: *mut c_void, main: &NSWindow) -> *mut c_void {
            super::super::create_plugin_scintilla(parent, main)
        }

        /// `text` into the view at `handle`, then its length back — both
        /// through the plugin routing callback, as a plugin would send them.
        fn round_trip(handle: *mut c_void, text: &str) -> isize {
            let text = CString::new(text).expect("no interior NUL");
            super::dispatch(handle, SCI_SETTEXT, 0, text.as_ptr() as isize);
            super::dispatch(handle, SCI_GETLENGTH, 0, 0)
        }

        /// The view at `handle`, which the host made and never releases.
        fn view_at(handle: *mut c_void) -> &'static NSView {
            // SAFETY: a view `create_plugin_scintilla` made, never
            // released, used on the main thread.
            unsafe { &*handle.cast::<NSView>() }
        }

        /// Where a view may go, and where it may not. Returns the plugin
        /// view the last check made a view in, for
        /// [`a_plugin_panel_is_a_parent_and_its_container_is_not`].
        pub(super) fn scintilla_parents_are_checked(
            rig: &Rig,
            host_sci: *mut c_void,
            mtm: MainThreadMarker,
        ) -> Retained<NSView> {
            let main = &rig.window;
            let not_a_view = NSObject::new();
            // SAFETY: a plain accessor on a live view, on the main thread.
            let frame_view = unsafe { rig.content.superview() }.expect("a window's frame view");
            for (what, parent) in [
                ("null", std::ptr::null_mut()),
                ("the host's own Scintilla view", host_sci),
                ("an NSObject", handle_of(&not_a_view)),
                (
                    "a view of the host's, in the main window",
                    handle_of(&rig.editor_cell),
                ),
                ("the main window's frame view", handle_of(&frame_view)),
            ] {
                assert!(
                    make(parent, main).is_null(),
                    "{what} was accepted as a parent"
                );
            }

            // The npp handle: a view in no window at all.
            let detached = make(super::super::npp_sentinel(), main);
            assert!(!detached.is_null(), "the npp handle as parent made no view");
            let view = view_at(detached);
            // SAFETY (here and below): plain accessors on live views, on
            // the main thread.
            assert!(
                unsafe { view.superview() }.is_none(),
                "a detached view was put somewhere"
            );
            assert_eq!(
                round_trip(detached, "abc"),
                3,
                "the detached view is not routed"
            );
            assert!(
                make(detached, main).is_null(),
                "a plugin's own Scintilla view was accepted as a parent"
            );

            // A free-standing view of the plugin's: in it, hidden and
            // zero-sized, and UTF-8.
            let panel = plugin_view(mtm);
            let made = make(handle_of(&panel), main);
            assert!(!made.is_null(), "a plugin's free-standing view was refused");
            let view = view_at(made);
            let parent = unsafe { view.superview() }.expect("put in the parent");
            assert!(std::ptr::eq(
                Retained::as_ptr(&parent),
                Retained::as_ptr(&panel)
            ));
            assert!(
                view.isHidden(),
                "a new view is shown before the plugin sizes it"
            );
            assert_eq!(
                view.frame().size,
                NSSize::new(0.0, 0.0),
                "a new view has a size"
            );
            assert!(view.clipsToBounds(), "a new view does not clip");
            assert_eq!(
                super::dispatch(made, SCI_GETCODEPAGE, 0, 0),
                SC_CP_UTF8 as isize,
                "a new view is not UTF-8"
            );
            panel
        }

        /// A plugin view once it is a docked panel in the main window is
        /// still the plugin's, so still a parent — and so is a view inside
        /// it. The host's container around it is not, whether the panel is
        /// shown or not yet.
        pub(super) fn a_plugin_panel_is_a_parent_and_its_container_is_not(
            rig: &Rig,
            panel: Retained<NSView>,
            mtm: MainThreadMarker,
        ) {
            let main = &rig.window;
            assert!(
                super::super::register_dock_dialog(rig.params(handle_of(&panel), "Smoke Sci Host"))
                    .is_some(),
                "the scenario's panel was not adopted"
            );
            assert!(crate::dock::show_plugin_panel(handle_of(&panel)));
            reconcile();
            let host = unsafe { panel.superview() }.expect("adopted into a container");
            assert!(
                panel
                    .window()
                    .is_some_and(|w| std::ptr::eq(Retained::as_ptr(&w), Retained::as_ptr(main))),
                "the docked panel is not in the main window"
            );
            assert!(
                !make(handle_of(&panel), main).is_null(),
                "a docked plugin panel was refused"
            );
            let inner = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
            panel.addSubview(&inner);
            assert!(
                !make(handle_of(&inner), main).is_null(),
                "a view inside a plugin panel was refused"
            );
            assert!(
                make(handle_of(&host), main).is_null(),
                "the host's container around a plugin panel was accepted as a parent"
            );

            // A panel registered but not yet shown: its container is in no
            // window yet, so the host-window check cannot see it, and only
            // the check for the container itself refuses it.
            let unshown = plugin_view(mtm);
            assert!(
                super::super::register_dock_dialog(
                    rig.params(handle_of(&unshown), "Smoke Sci Unshown")
                )
                .is_some(),
                "the unshown panel was not adopted"
            );
            let unshown_host = unsafe { unshown.superview() }.expect("adopted at registration");
            assert!(
                unshown_host.window().is_none(),
                "an unshown panel's container is already in a window"
            );
            assert!(
                make(handle_of(&unshown_host), main).is_null(),
                "the host's container around an unshown plugin panel was accepted as a parent"
            );
            std::mem::forget((panel, inner, unshown));
        }

        /// A plugin's view answers `SCI_*` from the plugin's own thread
        /// the way the host's does: parked until the main queue drains.
        pub(super) fn a_plugin_view_is_routed_from_any_thread(
            main: &NSWindow,
            mtm: MainThreadMarker,
        ) {
            struct WorkerPtr(*mut c_void);
            // SAFETY: a view the host never releases; the worker hands it
            // only to `plugin_dispatch`, the code under test.
            unsafe impl Send for WorkerPtr {}

            let panel = plugin_view(mtm);
            let made = make(handle_of(&panel), main);
            assert_eq!(round_trip(made, "hello"), 5);
            let handle = WorkerPtr(made);
            let worker = std::thread::spawn(move || {
                let handle = handle;
                super::dispatch(handle.0, SCI_GETLENGTH, 0, 0)
            });
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(
                !worker.is_finished(),
                "a cross-thread SCI_* to a plugin's view ran off the main thread"
            );
            let mut spins = 0;
            while !worker.is_finished() {
                let deadline = NSDate::dateWithTimeIntervalSinceNow(0.01);
                // SAFETY: main thread and a live mode constant.
                let _ = unsafe {
                    NSRunLoop::mainRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &deadline)
                };
                spins += 1;
                assert!(spins < 10_000, "the marshaled SCI_* never completed");
            }
            assert_eq!(worker.join().expect("worker panicked"), 5);
            std::mem::forget(panel);
        }

        /// A plugin's window is registered — to no effect, answered — and
        /// the handles that are no window of a plugin's are not. Removal
        /// refuses only what it can tell without reading the pointer.
        pub(super) fn modeless_dialogs_are_checked_and_answered(
            main: &NSWindow,
            host_sci: *mut c_void,
            mtm: MainThreadMarker,
        ) {
            // SAFETY: `NSWindow`'s designated initialiser on a fresh
            // allocation; never shown, and release-on-close is off.
            let dialog = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 100.0)),
                    NSWindowStyleMask::Titled,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: the safe direction — this test keeps a reference.
            unsafe { dialog.setReleasedWhenClosed(false) };
            let register = super::super::register_modeless_dialog;
            assert!(
                register(handle_of(&dialog), true, main),
                "a plugin's window was refused"
            );
            assert!(
                register(handle_of(&dialog), false, main),
                "its removal was refused"
            );
            let not_a_window = NSObject::new();
            for (what, handle) in [
                ("null", std::ptr::null_mut()),
                ("the npp handle", super::super::npp_sentinel()),
                ("the host's Scintilla view", host_sci),
                ("an NSObject", handle_of(&not_a_window)),
                ("the main window", handle_of(main)),
            ] {
                assert!(!register(handle, true, main), "{what} was registered");
            }
            for (what, handle) in [
                ("null", std::ptr::null_mut()),
                ("the npp handle", super::super::npp_sentinel()),
                ("the host's Scintilla view", host_sci),
            ] {
                assert!(
                    !register(handle, false, main),
                    "{what}'s removal was answered"
                );
            }
            std::mem::forget(dialog);
        }

        /// A plugin's toolbar button: added once per command after a
        /// separator, its image replaced on a second request, refused for
        /// an unknown command or a non-image — and a click that runs the
        /// command leaves the plugin's mark showing, not AppKit's flip.
        pub(super) fn plugin_toolbar_buttons(mtm: MainThreadMarker) {
            let actions = crate::menu::Actions::new(mtm);
            let toolbar = crate::toolbar::Toolbar::new(1000.0, &actions, mtm);
            let before = toolbar.container.subviews().len();
            let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(16.0, 16.0));
            let other = NSImage::initWithSize(NSImage::alloc(), NSSize::new(16.0, 16.0));
            let add = |icon: *mut c_void| super::super::add_toolbar_icon(&toolbar, SMOKE_CMD, icon);
            assert!(
                !add(handle_of(&image)),
                "a button for an unknown command was added"
            );

            // Make the command known, as a load pass does, and give it a
            // mark to show.
            super::super::COMMAND_LABELS.with(|labels| {
                labels
                    .borrow_mut()
                    .insert(SMOKE_CMD, "Smoke Command".to_owned())
            });
            let func = codepp_plugin_host::FuncItem {
                item_name: [0; codepp_plugin_host::MENU_TITLE_LENGTH],
                p_func: Some(smoke_command),
                cmd_id: SMOKE_CMD,
                init2_check: 0,
                p_sh_key: std::ptr::null_mut(),
            };
            super::super::PLUGIN_CHECKS.with(|c| c.borrow_mut().absorb([&func]));
            assert!(super::super::PLUGIN_CHECKS.with(|c| c.borrow_mut().set(SMOKE_CMD, true)));

            let not_an_image = NSObject::new();
            assert!(
                !add(handle_of(&not_an_image)),
                "an NSObject was taken as an image"
            );
            assert!(
                add(handle_of(&image)),
                "a known command's button was refused"
            );
            let subviews = toolbar.container.subviews();
            assert_eq!(
                subviews.len(),
                before + 2,
                "not one separator and one button"
            );
            let button = subviews
                .iter()
                .last()
                .and_then(|v| v.downcast::<NSButton>().ok())
                .expect("the last subview is the button");
            assert_eq!(button.tag(), SMOKE_CMD as isize);
            assert_eq!(
                button.state(),
                1,
                "the button does not show the command's mark"
            );
            assert!(
                button
                    .image()
                    .is_some_and(|i| std::ptr::eq(Retained::as_ptr(&i), Retained::as_ptr(&image))),
                "the button does not show the plugin's image"
            );
            assert_eq!(
                button.toolTip().map(|t| t.to_string()).as_deref(),
                Some("Smoke Command")
            );

            // A second request replaces the image; nothing is added.
            assert!(add(handle_of(&other)));
            assert_eq!(toolbar.container.subviews().len(), before + 2);
            assert!(button
                .image()
                .is_some_and(|i| std::ptr::eq(Retained::as_ptr(&i), Retained::as_ptr(&other))));

            // A click flips a push-on/push-off button; the action runs the
            // command — no plugin is loaded here, so it finds nothing to
            // run — and puts the plugin's mark back.
            // SAFETY: a live button on the main thread; its target is the
            // `actions` this scenario keeps, and the action it sends is
            // the toolbar's own, which takes a button.
            unsafe { button.performClick(None) };
            assert_eq!(button.state(), 1, "the click's flip was left showing");
            // And a mark set through `NPPM_SETMENUITEMCHECK` reaches it.
            toolbar.set_plugin_button_state(SMOKE_CMD, false);
            assert_eq!(button.state(), 0);
            std::mem::forget((actions, toolbar, image, other));
        }

        /// A plugin command that is never run: the scenario loads no
        /// plugin, so `on_plugin_command` resolves nothing.
        extern "C" fn smoke_command() {}
    }

    /// The pieces of [`plugin_panels_are_hosted_by_the_dock`].
    mod panel_scenario {
        use std::ffi::c_void;

        use codepp_core::dock::DockPanel;
        use objc2::rc::Retained;
        use objc2::MainThreadOnly;
        use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
        use objc2_foundation::{
            MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSObject, NSPoint, NSRect, NSRunLoop,
            NSSize,
        };

        fn rect(w: f64, h: f64) -> NSRect {
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h))
        }

        /// The address a plugin would pass as `hClient`.
        pub(super) fn handle_of(object: &NSObject) -> *mut c_void {
            std::ptr::from_ref(object).cast_mut().cast::<c_void>()
        }

        /// The view a plugin would make: free-standing, with content.
        pub(super) fn plugin_view(mtm: MainThreadMarker) -> Retained<NSView> {
            let view = NSView::initWithFrame(NSView::alloc(mtm), rect(240.0, 120.0));
            view.addSubview(&NSView::initWithFrame(
                NSView::alloc(mtm),
                rect(200.0, 20.0),
            ));
            view
        }

        /// A dock installed around a window that is never shown.
        pub(super) struct Rig {
            /// The window the dock sits in, standing in for the main
            /// window. Held so it outlives the scenario, as the app's does.
            pub(super) window: Retained<NSWindow>,
            pub(super) content: Retained<NSView>,
            pub(super) area: Retained<crate::dock::DockArea>,
            pub(super) editor_cell: Retained<NSView>,
            /// Held because the dock's close buttons target it weakly.
            _actions: Retained<crate::menu::Actions>,
            tb_data: &'static codepp_plugin_host::TbData,
        }

        impl Rig {
            pub(super) fn install(mtm: MainThreadMarker) -> Self {
                // SAFETY: `NSWindow`'s designated initialiser on a fresh
                // allocation; the window is never shown and never closed,
                // and release-on-close is turned off before anything could.
                let window = unsafe {
                    NSWindow::initWithContentRect_styleMask_backing_defer(
                        NSWindow::alloc(mtm),
                        rect(1000.0, 700.0),
                        NSWindowStyleMask::Titled,
                        NSBackingStoreType::Buffered,
                        false,
                    )
                };
                // SAFETY: the safe direction — the rig keeps its own reference.
                unsafe { window.setReleasedWhenClosed(false) };
                let content = window.contentView().expect("a content view");
                let area = crate::dock::DockArea::new(content.bounds(), mtm);
                content.addSubview(&area);
                let editor_cell = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
                area.addSubview(&editor_cell);
                let actions = crate::menu::Actions::new(mtm);
                crate::dock::install(
                    &window,
                    &content,
                    crate::dock::DockHosts {
                        area: area.clone(),
                        editor_cell: editor_cell.clone(),
                        workspace: NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO),
                        docmap: NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO),
                    },
                    &actions,
                    mtm,
                );
                // Never read by the scenario (only `NPPM_DMMUPDATEDISPINFO`
                // re-reads it), but the ABI never lets it be null and the
                // registration keeps it: leaked, as a plugin's `static` is.
                let tb_data = Box::leak(Box::new(codepp_plugin_host::TbData {
                    h_client: std::ptr::null_mut(),
                    psz_name: std::ptr::null(),
                    dlg_id: 1,
                    u_mask: codepp_plugin_host::DWS_DF_CONT_BOTTOM,
                    h_icon_tab: std::ptr::null_mut(),
                    psz_add_info: std::ptr::null(),
                    rc_float: codepp_plugin_host::TbRect::default(),
                    i_prev_cont: -1,
                    psz_module_name: std::ptr::null(),
                }));
                Self {
                    window,
                    content,
                    area,
                    editor_cell,
                    _actions: actions,
                    tb_data,
                }
            }

            /// A registration as the dispatcher would decode it.
            pub(super) fn params(
                &self,
                handle: *mut c_void,
                name: &str,
            ) -> codepp_plugin_host::DockDialogParams {
                codepp_plugin_host::DockDialogParams {
                    h_client: handle,
                    name: name.to_owned(),
                    module_name: "smoke_panel.dylib".to_owned(),
                    add_info: None,
                    h_icon_tab: std::ptr::null_mut(),
                    rc_float: codepp_plugin_host::TbRect::default(),
                    u_mask: codepp_plugin_host::DWS_DF_CONT_BOTTOM,
                    dlg_id: 1,
                    i_prev_cont: -1,
                    tb_data: self.tb_data,
                    caller: None,
                }
            }
        }

        /// What the NPPM dispatch does once its borrow has ended.
        pub(super) fn reconcile() {
            assert!(
                crate::dock::take_dirty(),
                "a DMM handler must mark the dock"
            );
            crate::dock::apply_layout();
            crate::dock::layout_area(1000.0, 700.0);
        }

        /// Not a view, a view already in a view (every one of the host's
        /// own, and the same view a second time), a window's frame view —
        /// which has no superview — and null: all refused.
        pub(super) fn refusals_refuse(rig: &Rig, registered: *mut c_void) {
            let not_a_view = NSObject::new();
            // SAFETY: a plain accessor on a live view, on the main thread.
            let frame_view = unsafe { rig.content.superview() }.expect("a window's frame view");
            for (what, refused) in [
                ("an NSObject", handle_of(&not_a_view)),
                ("a view with a superview", handle_of(&rig.editor_cell)),
                ("the same view again", registered),
                ("a window's frame view", handle_of(&frame_view)),
                ("null", std::ptr::null_mut()),
            ] {
                assert!(
                    super::super::register_dock_dialog(rig.params(refused, "Refused")).is_none(),
                    "{what} was adopted"
                );
            }
        }

        /// The view sits in the host's clipping container, filling it,
        /// which sits in a group's slot, in a group frame, in the dock
        /// area. Returns the container.
        pub(super) fn assert_hosted(
            view: &NSView,
            area: &crate::dock::DockArea,
        ) -> Retained<NSView> {
            // SAFETY (all four): plain accessors on live views, on the
            // main thread.
            let host = unsafe { view.superview() }.expect("adopted into a container");
            assert!(
                host.downcast_ref::<crate::dock::PluginPanelHost>()
                    .is_some(),
                "the view's parent is not the host's container"
            );
            assert!(host.clipsToBounds(), "the container must clip");
            assert!(!host.isHidden(), "a shown panel's container is hidden");
            let slot = unsafe { host.superview() }.expect("in a group's slot");
            let group = unsafe { slot.superview() }.expect("in a group frame");
            assert!(
                group.downcast_ref::<crate::dock::GroupFrame>().is_some(),
                "the container is not inside a group frame"
            );
            let docked_in = unsafe { group.superview() }.expect("docked");
            assert!(
                std::ptr::eq(
                    Retained::as_ptr(&docked_in).cast::<c_void>(),
                    std::ptr::from_ref(area).cast::<c_void>()
                ),
                "the group frame is not in the dock area"
            );
            let (outer, inner) = (host.bounds().size, view.frame().size);
            assert!(
                outer.width > 0.0 && outer.height > 0.0,
                "the band has no size"
            );
            assert!(
                (outer.width - inner.width).abs() < 0.5
                    && (outer.height - inner.height).abs() < 0.5,
                "the view does not fill its container: {inner:?} in {outer:?}"
            );
            host
        }

        /// A plugin taking its view back ends the registration at once,
        /// and the sweep, from the run loop's default mode, takes the
        /// emptied container out and closes the panel.
        pub(super) fn taking_the_view_back_ends_the_registration(
            view: &NSView,
            host: &NSView,
            panel: DockPanel,
        ) {
            view.removeFromSuperview();
            assert!(
                !crate::dock::is_plugin_panel_registered(panel),
                "a view taken back still counts as registered"
            );
            // SAFETY (both): a plain accessor on a live view, on the main
            // thread.
            spin_until(|| unsafe { host.superview() }.is_none());
            assert!(
                unsafe { host.superview() }.is_none(),
                "the emptied container was left in its group"
            );
            assert!(
                !crate::dock::layout_snapshot()
                    .expect("the dock is installed")
                    .is_visible(panel),
                "the panel stayed open with nothing in it"
            );
        }

        /// A plugin that takes its view back and registers a replacement
        /// under the same name before the sweep has run — a natural way
        /// to rebuild a panel — keeps the panel open: the sweep drops the
        /// old registration and its emptied container, and the
        /// replacement takes the panel's place. Returns the replacement.
        pub(super) fn a_replacement_before_the_sweep_keeps_the_panel(
            rig: &Rig,
            view: &NSView,
            panel: DockPanel,
            mtm: MainThreadMarker,
        ) -> Retained<NSView> {
            assert!(crate::dock::show_plugin_panel(handle_of(view)));
            reconcile();
            let old_host = assert_hosted(view, &rig.area);
            view.removeFromSuperview();
            let replacement = plugin_view(mtm);
            assert_eq!(
                super::super::register_dock_dialog(
                    rig.params(handle_of(&replacement), "Smoke Panel")
                ),
                Some(panel),
                "the replacement did not take the panel's name"
            );
            // Only now does the run loop reach the sweep the take-back
            // queued.
            // SAFETY: a plain accessor on a live view, on the main thread.
            spin_until(|| unsafe { old_host.superview() }.is_none());
            assert!(
                crate::dock::layout_snapshot()
                    .expect("the dock is installed")
                    .is_visible(panel),
                "the sweep closed a panel its plugin had just rebuilt"
            );
            assert!(crate::dock::is_plugin_panel_registered(panel));
            assert_hosted(&replacement, &rig.area);
            replacement
        }

        /// Register `view` as [`Rig::params`] would, but under a module of
        /// its own, so one scenario's panels take none of another's
        /// per-module allowance of panel identities.
        pub(super) fn register_apart(rig: &Rig, view: &NSView, name: &str) -> DockPanel {
            let mut params = rig.params(handle_of(view), name);
            "smoke_notices.dylib".clone_into(&mut params.module_name);
            super::super::register_dock_dialog(params).expect("a free-standing view is adopted")
        }

        /// Record the dock notices sent from now on, starting from none.
        pub(super) fn record_notices() {
            super::super::SENT_DOCK_NOTICES.with(|sent| *sent.borrow_mut() = Some(Vec::new()));
        }

        /// The notices sent since the last call.
        pub(super) fn take_notices() -> Vec<(DockPanel, u32)> {
            super::super::SENT_DOCK_NOTICES.with(|sent| {
                sent.borrow_mut()
                    .as_mut()
                    .map(std::mem::take)
                    .unwrap_or_default()
            })
        }

        fn sent_count() -> usize {
            super::super::SENT_DOCK_NOTICES.with(|sent| sent.borrow().as_ref().map_or(0, Vec::len))
        }

        /// Notices as a reader would name them, for a failed check.
        fn readable(notices: &[(DockPanel, u32)]) -> Vec<String> {
            notices
                .iter()
                .map(|(panel, code)| {
                    format!(
                        "{} {} ({})",
                        panel.title(),
                        codepp_plugin_host::docking::dmn_name(*code),
                        code >> 16
                    )
                })
                .collect()
        }

        /// Exactly `expected` has been sent — already, without the run
        /// loop having run since.
        pub(super) fn expect_sent_now(expected: &[(DockPanel, u32)], what: &str) {
            let sent = take_notices();
            assert!(
                sent == expected,
                "{what}: sent {:?}, expected {:?}",
                readable(&sent),
                readable(expected)
            );
        }

        /// Exactly `expected` is sent once the run loop has run in its
        /// default mode — where the placement check is queued — and
        /// nothing after it.
        pub(super) fn expect_sent(expected: &[(DockPanel, u32)], what: &str) {
            spin_until(|| sent_count() >= expected.len());
            run_default_mode();
            expect_sent_now(expected, what);
        }

        /// Nothing is sent, even once the run loop has run.
        pub(super) fn expect_nothing_more(what: &str) {
            run_default_mode();
            expect_sent_now(&[], what);
        }

        /// A few turns of the run loop's default mode: enough for anything
        /// queued there to run.
        pub(super) fn run_default_mode() {
            for _ in 0..20 {
                let deadline = NSDate::dateWithTimeIntervalSinceNow(0.01);
                // SAFETY: main thread and a live mode constant.
                let _ = unsafe {
                    NSRunLoop::mainRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &deadline)
                };
            }
        }

        /// One delivery works through at most its cap of notices, however
        /// many are queued, and the rest go with the queue: a second
        /// delivery starts from nothing. `a` must be on screen, so each
        /// notice is still true when its turn comes.
        pub(super) fn a_runaway_delivery_is_cut_off(first: &NSView, a: DockPanel) {
            let cap = super::super::MAX_DOCK_NOTICES_PER_DELIVERY;
            let relaid = crate::dock::DockNotice {
                panel: a,
                handle: handle_of(first),
                caller: None,
                code: codepp_plugin_host::DMN_FLOATDROPPED,
            };
            let _ = take_notices();
            super::super::deliver_dock_notices(vec![relaid; cap + 50]);
            assert_eq!(
                take_notices().len(),
                cap,
                "one delivery sent more, or fewer, than its cap"
            );
            super::super::deliver_dock_notices(Vec::new());
            assert!(
                take_notices().is_empty(),
                "notices past the cap were left queued for the next delivery"
            );
        }

        /// A notice queued behind a handler that changed the layout is
        /// checked again when its turn comes, and one that no longer says
        /// something true is not sent: a view that is not the
        /// registration's, a switch-in or a relayout for a panel closed
        /// since. A switch-off goes out as queued.
        pub(super) fn stale_notices_are_not_sent(
            first: &NSView,
            a: DockPanel,
            second: &NSView,
            b: DockPanel,
        ) {
            use codepp_plugin_host::{DMN_FLOATDROPPED, DMN_SWITCHIN, DMN_SWITCHOFF};
            let applies = |panel, view: &NSView, code| {
                crate::dock::notice_still_applies(&crate::dock::DockNotice {
                    panel,
                    handle: handle_of(view),
                    caller: None,
                    code,
                })
            };
            assert!(
                applies(a, first, DMN_SWITCHIN),
                "a panel in front, as queued"
            );
            assert!(
                !applies(a, second, DMN_SWITCHIN),
                "another registration's view is not this panel's"
            );
            assert!(crate::dock::hide_plugin_panel(handle_of(second)));
            reconcile();
            let _ = take_notices();
            assert!(
                !applies(b, second, DMN_SWITCHIN),
                "switched in, then closed"
            );
            assert!(
                !applies(b, second, DMN_FLOATDROPPED),
                "laid out, then closed"
            );
            assert!(
                applies(b, second, DMN_SWITCHOFF),
                "a switch-off goes out as queued"
            );
        }

        /// Run the main run loop in its default mode — where the sweep is
        /// queued — until `done`, for at most half a second.
        fn spin_until(done: impl Fn() -> bool) {
            for _ in 0..50 {
                if done() {
                    return;
                }
                let deadline = NSDate::dateWithTimeIntervalSinceNow(0.01);
                // SAFETY: main thread and a live mode constant.
                let _ = unsafe {
                    NSRunLoop::mainRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &deadline)
                };
            }
        }
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

/// Which plugins a [`load_plugins_where`] pass may load.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadScope {
    /// Every discovered plugin — DESIGN.md §6.4's lazy triggers: the
    /// Plugins menu, a plugin hotkey.
    All,
    /// Only the plugins owning a dock panel the restored session had
    /// open — the startup pass. Without it a restored plugin panel waits
    /// for a view that only its plugin can supply, and nothing loads the
    /// plugin until the user opens a menu they have no reason to connect
    /// with the panel they are missing.
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
/// Called once, from the application delegate's
/// `applicationDidFinishLaunching:`, after the dock layout is restored,
/// the plugins are discovered and the window is ordered front, and
/// before the first frame paints, so that frame already carries the
/// panels — the moment `ui_gtk` picks with `gtk::main` about to start
/// and Win32 after its window is shown. Parks whatever no loaded plugin
/// can supply even when nothing loads, so no group is left on screen
/// empty.
pub(crate) fn restore_panel_plugins() {
    load_plugins_where(LoadScope::RestoredPanels);
    // The pass froze the drain for its whole length, and a frozen drain
    // does not come back for the wakes it declined — the session's own
    // file loads finish during it — so flush once, now it has lifted, as
    // the other two load triggers do.
    crate::drain_shell();
}

/// Load the plugins `scope` admits, **holding no `with_state` borrow
/// while plugin code runs**, and bring back the dock panels they had
/// open.
///
/// The load used to happen inside one borrow — `dlopen`, `setInfo`,
/// `getFuncsArray` and `NPPN_READY` together — and that borrow is
/// exactly what made a plugin's re-entrant `NPPM_*` decline. Real
/// plugins interrogate the host from `setInfo`: `NppExec` asks for the
/// version there and refuses to start without an answer, so a declined
/// query reads as "older than Notepad++ 5.1". Now each step takes what it
/// needs under a borrow, runs the plugin's entry points with none held,
/// and commits under a fresh one. A nested pass (a plugin re-entering
/// the loader from `setInfo`) is bounded inside `PluginHost`, which
/// answers "nothing pending" while a load is outstanding.
///
/// Then, the same steps in the same order as Win32's `load_plugins_where`
/// and GTK's — a function for each, so the three read side by side:
///
///   1. the plugins' commands become known, so a tick set from a
///      load-time notification is recorded ([`absorb_loaded_commands`]);
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
    // Splitting the load into borrow-free steps costs the property that
    // made a `DrainFreeze` unnecessary here, so the guard is explicit.
    // AppKit's menu-tracking loop services GCD's main-queue source, and
    // without it a worker result could be applied *between* two plugins'
    // loads, moving the very tabs a `setInfo` is asking about.
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
            // The state went away between the two phases — the
            // window torn down under a `setInfo`. Nothing commits,
            // so the latch stays set and no further plugin loads;
            // say so, because the symptom is otherwise silent.
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
    // command id and painted whenever the menu is shown, and
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
    let mut withheld: Vec<DockPanel> = Vec::new();
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
    panels: Vec<DockPanel>,
    /// Panels owned by the plugins this pass loaded that stay parked
    /// because Preferences → Security's guard does not trust their record
    /// — see [`unpark_loaded_plugins_panels`]. Restored after all if
    /// their plugin registers them from `NPPN_TBMODIFICATION`, which puts
    /// them back and records a command the plugin itself declared.
    held: Vec<DockPanel>,
    /// Every group's front tab as the pass began.
    fronts: Vec<(u32, DockPanel)>,
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
    let back: Vec<DockPanel> = with_state(|st| {
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
/// check there, so even a panel whose view already exists comes back
/// half-restored if the plugin never hears it. Measured against
/// Notepad++ 8.9.6: it runs the command for every panel it recorded open,
/// between `NPPN_TBMODIFICATION` and `NPPN_READY` — which is where
/// `LoadNotifications::deliver` calls this.
///
/// Each command runs exactly as a click on its menu item does
/// ([`on_plugin_command`]). The list is resolved under a borrow that ends
/// before the first one runs: it is the plugin's own code, and it talks
/// back to the host. Each show brings its panel to the front of its
/// group, so the tabs the user had in front are put back in front
/// afterwards, as Notepad++ also does.
///
/// With Preferences → Security's guard on, a command runs only if Code++
/// signed it — which it does only for a command the panel's own plugin
/// registered. Each panel whose command is held back goes into
/// `withheld`, for the caller to park once READY is over. The command is
/// read now rather than when the pass began, so one a plugin has just
/// registered from `NPPN_TBMODIFICATION` is the one judged — and a held
/// panel it registered, back from parking, is restored with the rest.
fn restore_plugin_panels(restore: &PanelRestore, withheld: &mut Vec<DockPanel>) {
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
        // A boundary per command, as each click on a menu item has: host
        // bookkeeping that fails for one panel must not cost the rest of
        // the pass their restores.
        crate::at_callback_boundary("plugin:restore:command", (), || {
            on_plugin_command(cmd);
        });
    }
    if !commands.is_empty() && crate::dock::update_layout(|l| l.restore_fronts(&restore.fronts)) {
        crate::dock::apply_layout();
    }
}

/// Close each restored panel whose plugin loaded, was told to restore
/// it, heard `NPPN_READY` — and still never supplied a view.
///
/// Such a panel is a group with a caption and nothing in it, and nothing
/// is going to fill it this session. Closing it (the layout remembers
/// where it was) is the honest outcome, and it is also what the user
/// would see under Notepad++, which has no container for a panel that was
/// never registered. A panel whose command the guard withheld is not
/// closed: nothing about it is known to be wrong, and
/// [`park_withheld_panels`] keeps its place instead.
fn close_unregistered_restored_panels(restore: &PanelRestore, withheld: &[DockPanel]) {
    let unregistered: Vec<DockPanel> = restore
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
fn park_withheld_panels(withheld: &[DockPanel]) {
    let waiting: Vec<DockPanel> = withheld
        .iter()
        .copied()
        .filter(|p| !crate::dock::is_plugin_panel_registered(*p))
        .collect();
    if waiting.is_empty() {
        return;
    }
    let changed = crate::dock::update_layout(|l| {
        let visible: Vec<DockPanel> = waiting
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

/// Park every open plugin panel that has no view and no loaded plugin to
/// supply one — its plugin is not installed, is disabled, failed to
/// load, or (on this platform) exists only for another one, named by a
/// session written on Windows or Linux.
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
/// plugin registering it. A panel with a view is never parked, whichever
/// plugin its name belongs to.
fn park_unsupplied_plugin_panels() {
    let Some(layout) = crate::dock::layout_snapshot() else {
        return;
    };
    let viewless: Vec<DockPanel> = layout
        .open_plugin_panels()
        .into_iter()
        .filter(|p| !crate::dock::is_plugin_panel_registered(*p))
        .collect();
    if viewless.is_empty() {
        return;
    }
    let unsupplied: Vec<DockPanel> = with_state(|st| {
        let unsupplied = st.shell.panels_without_a_loaded_plugin(&viewless);
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

thread_local! {
    /// The plugins' check marks on their own menu items, by command id.
    /// This backend rebuilds the Plugins menu every time it opens, so a
    /// mark cannot live on an item; it lives here, and `validateMenuItem:`
    /// paints it whenever the item is shown — see
    /// [`codepp_plugin_host::PluginMenuChecks`]. A thread-local rather
    /// than state, so the paint never needs `with_state`: AppKit validates
    /// while menus track, and a marking handler runs inside the NPPM
    /// dispatch's borrow.
    static PLUGIN_CHECKS: RefCell<PluginMenuChecks> = RefCell::new(PluginMenuChecks::default());
}

/// Record the mark a plugin set on one of its items through
/// `NPPM_SETMENUITEMCHECK`, and show it on the item at once if the
/// Plugins menu is open. `false` when `cmd_id` is none of the loaded
/// plugins' commands — the built-in `IDM_*` ids are not mapped on this
/// backend, as `NPPM_MENUCOMMAND` is not, and the View menu's own marks
/// come from live state in `validateMenuItem:` anyway.
///
/// Called from inside the dispatch's state borrow, so it touches only
/// this module's own record and the menu item itself: `main_menu` is the
/// menu bar the split UI carries, searched rather than cached because
/// the Plugins menu's items are rebuilt on every open. A click never
/// toggles an `NSMenuItem`'s state by itself, so unlike GTK there is no
/// click to undo — the mark shown is only ever the plugin's.
pub(crate) fn set_menu_check(main_menu: &NSMenu, cmd_id: i32, checked: bool) -> bool {
    if !PLUGIN_CHECKS.with(|c| c.borrow_mut().set(cmd_id, checked)) {
        tracing::trace!(
            cmd_id,
            "NPPM_SETMENUITEMCHECK: no loaded plugin's command has that id on this backend"
        );
        return false;
    }
    if let Some(item) = live_command_item(main_menu, cmd_id) {
        item.setState(isize::from(checked));
    }
    true
}

/// The mark to paint on a plugin command's item: the plugin's last word,
/// or unticked when it has never set one. Read by `validateMenuItem:`,
/// which is what makes a mark set before the menu was ever built, or
/// while it was closed, show the next time it opens.
pub(crate) fn menu_mark(cmd_id: i32) -> bool {
    PLUGIN_CHECKS.with(|c| c.borrow().get(cmd_id)) == Some(true)
}

/// The Plugins menu's item for plugin command `cmd_id`, if the menu holds
/// one now: a plugin's submenu entry whose tag is the command id and
/// whose action is the plugin-command selector — the tag alone would also
/// match any other item carrying the same number.
fn live_command_item(main_menu: &NSMenu, cmd_id: i32) -> Option<Retained<NSMenuItem>> {
    let plugins = crate::menu::plugins_menu(main_menu)?;
    let tag = isize::try_from(cmd_id).ok()?;
    plugins
        .itemArray()
        .iter()
        .filter_map(|top| top.submenu())
        .filter_map(|submenu| submenu.itemWithTag(tag))
        .find(|item| item.action() == Some(sel!(codeppPluginCommand:)))
}

/// Take in the commands every loaded plugin publishes — see
/// [`PluginMenuChecks::absorb`] — and their labels, which a toolbar
/// button for one of them shows ([`add_toolbar_icon`]). Run after each
/// load pass and **before** its notifications, so a plugin ticking an item
/// or adding a toolbar button from `NPPN_TBMODIFICATION` or `NPPN_READY`
/// finds its commands known, as Notepad++ has them installed by then.
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
                if f.p_func.is_some() {
                    labels.insert(f.cmd_id, funcitem_label(f));
                }
            }
        });
    });
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
    // thread with no arguments, per the N++ ABI, and marked as its own
    // plugin while it runs (`codepp_plugin_host::caller`).
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { cmd.run() }));
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

/// Tell every loaded plugin the host is shutting down:
/// `NPPN_BEFORESHUTDOWN` to each of them, then `NPPN_SHUTDOWN` to each —
/// the pair Win32 sends from `WM_CLOSE`, while the plugins' panels still
/// exist.
///
/// Delivered from a snapshot with no borrow held, so a plugin that saves
/// its settings here can ask the host where (`NPPM_GETPLUGINSCONFIGDIR`)
/// and be answered — which Win32, whose teardown cannot drop its borrow
/// first, declines; GTK answers too. A plugin cannot veto the shutdown:
/// the notifications are informational, as they are there. Called once,
/// by `crate::quit`.
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

#[cfg(test)]
mod plugin_scintilla_tests {
    use super::{
        may_make_plugin_scintilla, MAX_PLUGIN_SCINTILLAS, MAX_PLUGIN_SCINTILLAS_PER_PLUGIN,
    };

    /// A plugin that has had its allowance is refused, and that costs
    /// every other plugin nothing — the reason there is a per-plugin cap
    /// at all.
    #[test]
    fn one_plugin_spends_its_own_allowance_and_nobody_elses() {
        let mut made = Vec::new();
        for _ in 0..MAX_PLUGIN_SCINTILLAS_PER_PLUGIN {
            assert!(may_make_plugin_scintilla(&made, Some(3)).is_ok());
            made.push(Some(3));
        }
        assert!(may_make_plugin_scintilla(&made, Some(3)).is_err());
        assert!(may_make_plugin_scintilla(&made, Some(4)).is_ok());
        // Views asked for from outside any host call share an allowance
        // of their own, apart from every plugin's.
        assert!(may_make_plugin_scintilla(&made, None).is_ok());
    }

    /// Views asked for from outside any host call are one allowance
    /// between them.
    #[test]
    fn views_nobody_can_be_charged_for_share_one_allowance() {
        let made = vec![None; MAX_PLUGIN_SCINTILLAS_PER_PLUGIN];
        assert!(may_make_plugin_scintilla(&made, None).is_err());
        assert!(may_make_plugin_scintilla(&made, Some(0)).is_ok());
    }

    /// The table is full once every slot is taken, whoever took them.
    #[test]
    fn the_table_holds_no_more_than_its_slots() {
        let made: Vec<Option<usize>> = (0..MAX_PLUGIN_SCINTILLAS).map(Some).collect();
        assert!(may_make_plugin_scintilla(&made, Some(MAX_PLUGIN_SCINTILLAS)).is_err());
        assert!(may_make_plugin_scintilla(&made[1..], Some(MAX_PLUGIN_SCINTILLAS)).is_ok());
    }
}
