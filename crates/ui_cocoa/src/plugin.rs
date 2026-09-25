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

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, sel, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSBorderType, NSButton, NSControlTextEditingDelegate, NSFont, NSImage,
    NSLineBreakMode, NSMenu, NSMenuItem, NSScrollView, NSStackView, NSTableColumn, NSTableView,
    NSTableViewColumnAutoresizingStyle, NSTableViewDataSource, NSTableViewDelegate, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};

use codepp_core::dock::DockPanel;
use codepp_plugin_host::{HostDispatchFn, NppData, PluginMenuChecks};
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
    /// `DMN_DOCK` / `DMN_FLOAT` notices waiting to be sent. See
    /// [`deliver_dock_notices`].
    static DOCK_NOTICES: RefCell<VecDeque<crate::dock::DockNotice>> =
        const { RefCell::new(VecDeque::new()) };
    /// Set while [`deliver_dock_notices`] is draining.
    static DOCK_NOTICES_DELIVERING: Cell<bool> = const { Cell::new(false) };
    /// Set while a `DMN_CLOSE` is being delivered. See
    /// [`close_plugin_panel`].
    static DMN_CLOSE_ACTIVE: Cell<bool> = const { Cell::new(false) };
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

/// Send each `DMN_DOCK` / `DMN_FLOAT` notice.
///
/// **Notices raised while one is being delivered are queued, not sent.**
/// The plugin's handler runs with no borrow held, so it may send
/// `NPPM_*` back — including `NPPM_DMMREGASDCKDLG` for a panel nothing
/// has been told about, which reconciles again from inside this loop and
/// raises a notice of its own. Sent there and then, that notice's handler
/// could do the same, nesting a full round trip per link until the
/// registration cap or the stack ran out. So a call made while a delivery
/// is running only appends to the queue and returns, and the outermost
/// call drains it in order: nothing is dropped, and the nesting stays one
/// level deep whatever the plugin does. The same queue Win32's
/// `deliver_container_notices` and GTK's keep.
pub(crate) fn deliver_dock_notices(notices: Vec<crate::dock::DockNotice>) {
    DOCK_NOTICES.with(|q| q.borrow_mut().extend(notices));
    if DOCK_NOTICES_DELIVERING.with(Cell::get) {
        return;
    }
    let _delivering = crate::FlagGuard::set(&DOCK_NOTICES_DELIVERING);
    while let Some(notice) = DOCK_NOTICES.with(|q| q.borrow_mut().pop_front()) {
        // A handler for an earlier notice may have taken this one's view
        // back; the record is already written, so skipping it loses
        // nothing that could still be delivered.
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
            /// Held so the window outlives the scenario, as the app's does.
            _window: Retained<NSWindow>,
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
                    _window: window,
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
/// [`PluginMenuChecks::absorb`]. Run after each load pass and **before**
/// its notifications, so a plugin ticking an item from
/// `NPPN_TBMODIFICATION` or `NPPN_READY` finds its commands known, as
/// Notepad++ has them installed by then.
fn absorb_loaded_commands() {
    // The record is its own thread-local and calls into nothing, so it is
    // filled from under the state borrow rather than from a copy.
    with_state(|st| {
        let funcs = st.shell.loaded_plugin_funcs().flat_map(|(_, funcs)| funcs);
        PLUGIN_CHECKS.with(|c| c.borrow_mut().absorb(funcs));
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
