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
//! Every entry point here assumes it is on the main thread, and nothing
//! enforces it. On Windows that assumption is free: a plugin calling
//! `SendMessage` from its own worker thread is marshaled by the OS onto
//! the thread that owns the window. There is no such marshaling here.
//! A plugin that sends `NPPM_*` off-thread degrades safely — the state
//! lives in a `thread_local`, so [`dispatch_nppm`] finds nothing and
//! returns "declined" — but a plugin that sends `SCI_*` off-thread
//! reaches `objc_msgSend` on a `ScintillaView` from the wrong thread,
//! which both AppKit and Scintilla document as undefined. That branch
//! deliberately bypasses `with_state` (see the re-entrancy note above),
//! so it has no equivalent backstop.
//!
//! `ui_gtk` has the identical characteristic and DESIGN.md §6.5 already
//! accepts that an in-process plugin can crash the app, so this is
//! recorded rather than guarded: refusing a cross-thread `SCI_*` would
//! diverge from the Win32 semantics a ported plugin was written
//! against, and marshaling it properly is a decision both non-Windows
//! backends have to make together. Tracked in §7.4.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};

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
use codepp_scintilla_sys::scintilla_cocoa_send_message;
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
            // SAFETY: `hwnd` is identity-checked to be the host's own
            // live `ScintillaView*`, which is created once at startup and
            // never destroyed (see `CocoaUiState::sci_view`), and
            // `scintilla_cocoa_send_message` is its documented entry
            // point. The message-argument contract is the plugin's
            // responsibility, exactly as it is on Win32.
            //
            // `with_state` is deliberately not taken: this is a direct
            // Scintilla call, and the plugin may well issue it from
            // inside an NPPM dispatch that already holds the borrow.
            unsafe { scintilla_cocoa_send_message(hwnd, msg, wparam, lparam) }
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
    with_state(|st| {
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
        let (shell, mut ui) = st.split();
        // SAFETY: called synchronously on the UI thread from plugin
        // code, with `(msg, wparam, lparam)` exactly as the plugin
        // passed them to `SendMessageW`; every `handles` field belongs
        // to this one window.
        unsafe { shell.dispatch_plugin_message(&mut ui, handles, msg, wparam, lparam) }.unwrap_or(0)
    })
    .unwrap_or(0)
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

/// Lazy-load every pending plugin, then rebuild the Plugins menu from
/// the loaded set. Called from the menu's `menuNeedsUpdate:`.
pub(crate) fn ensure_loaded_and_rebuild(menu: &NSMenu, actions: &Actions) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // Load pending plugins, installing this backend's routing callback
    // into each (the SDK handshake) so their `SendMessageW` reaches us.
    let dispatch: Option<HostDispatchFn> = Some(plugin_dispatch);
    let data = npp_data();
    // The whole load — `dlopen`, `setInfo`, `getFuncsArray`, and the
    // synchronous `NPPN_READY` — happens inside this one borrow, which
    // is what makes a `DrainFreeze` unnecessary here despite AppKit's
    // menu-tracking loop servicing GCD's main-queue source. A wake that
    // lands mid-load finds the borrow held and declines, so the work is
    // deferred rather than interleaved — the same property the guard
    // provides, obtained structurally. If this is ever split so the
    // borrow is dropped between steps, it needs the explicit guard.
    with_state(|st| st.shell.ensure_plugins_loaded(data, dispatch));
    rebuild_menu(menu, actions, mtm);
    // `NPPN_READY` fires synchronously inside `ensure_plugins_loaded`;
    // anything a plugin queued back is drained on the next wake.
    crate::drain_shell();
}

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
        crate::menu::add_disabled(menu, mtm, "No plugins loaded");
    } else {
        for (name, items) in entries {
            let submenu = NSMenu::new(mtm);
            for (label, cmd_id, is_command) in items {
                if is_command {
                    let item = crate::menu::add(
                        &submenu,
                        mtm,
                        &label,
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

/// Deliver every queued `NPPN_*` notification to the loaded plugins.
/// Called after each drain.
pub(crate) fn deliver_notifications() {
    let notes = with_state(|st| st.shell.take_notifications()).unwrap_or_default();
    for note in notes {
        with_state(|st| st.shell.notify_plugins(note, npp_sentinel()));
    }
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
