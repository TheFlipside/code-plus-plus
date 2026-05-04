//! Glue layer between `core` and the platform UI crates.
//!
//! The `Shell` owns the application's mutable state (Session, Loader,
//! FileWatcher, the active EditorHandle, the in-memory text buffer
//! shadow) and exposes high-level operations the UI calls in response
//! to user actions: `open_file`, `save_file`, `apply_load_result`,
//! `apply_file_change`. UI crates implement [`UiPlatform`] for the
//! parts that have to live on the UI thread (showing dialogs, posting
//! status-bar text, pushing buffer contents into the active Scintilla
//! control via `EditorHandle::send`).
//!
//! Cross-thread marshaling (DESIGN.md §5.4): worker threads (Loader,
//! FileWatcher) post their typed results into per-source channels and
//! call a wake closure that the UI crate hands the `Shell` at startup.
//! On Win32 the wake closure is `PostMessage(hwnd, WM_APP_WAKE, 0, 0)`.
//! The UI thread's wake handler drains both channels and applies each
//! item to the shell.

#[cfg(target_os = "windows")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use codepp_core::file::{Loader, LoaderShutdown};
use codepp_core::{Encoding, Eol, LoadResult, RequestId, Session};
use codepp_platform::watch::{FileChange, FileWatcher};
#[cfg(target_os = "windows")]
use codepp_plugin_host::{
    dispatch_nppm, notify_all, FuncItem, HostServices, Hwnd, Notification, NppData, PluginCmd,
    PluginHost, NPPMAINMENU, NPPPLUGINMENU,
};

/// Stable nonzero buffer id for the active buffer in the Phase 3
/// single-tab world. Multi-tab assigns per-tab ids in milestone 6.
/// Plugins receive this from `NPPM_GETCURRENTBUFFERID` and pass it
/// back via `NPPM_GETFULLPATHFROMBUFFERID` etc.
///
/// **Multi-tab migration note:** every site that references this
/// constant is a single-tab assumption. Searching for
/// `PRIMARY_BUFFER_ID` is the canonical way to find code that needs
/// rewriting in milestone 6 — keep using the named constant rather
/// than the literal `1` so the search stays useful.
pub const PRIMARY_BUFFER_ID: isize = 1;

/// Side-effecting operations the shell needs from the UI thread. Each
/// platform UI crate (`ui_win32`, `ui_gtk`, `ui_cocoa`) implements this
/// trait.
///
/// **Important:** none of these methods may run a nested message pump
/// (i.e. show a modal dialog). They are called while the shell holds
/// internal state borrows, and a nested pump would let other messages
/// re-enter the wnd_proc, producing aliasing UB. Modal interactions
/// are deferred via [`PendingDialog`] — `Shell::drain` returns a list
/// the UI consumes *after* the drain's borrow ends.
pub trait UiPlatform {
    /// Ensure the editor's currently-displayed document is the one
    /// belonging to tab `idx`. `scintilla_doc` is the tab's stored
    /// document pointer (0 = "no document yet, please create one").
    /// The method returns the document pointer the tab is now
    /// bound to — `Shell` writes this back onto `Tab.scintilla_doc`
    /// so subsequent activations short-circuit.
    ///
    /// On Win32 this routes through `SCI_CREATEDOCUMENT` (when
    /// `scintilla_doc == 0`) plus `SCI_SETDOCPOINTER` to bind the
    /// document to the single Scintilla view. Multi-tab Phase 3
    /// uses this pattern to keep each tab independent without
    /// owning multiple Scintilla controls.
    fn activate_tab(&mut self, idx: usize, scintilla_doc: isize) -> isize;

    /// Push the given decoded text into the *currently-active*
    /// editor document. The caller is responsible for having called
    /// [`Self::activate_tab`] first to ensure the right document is
    /// bound to the view.
    /// On Win32 this routes through `EditorHandle::send` with
    /// `SCI_SETTEXT` plus `SCI_GOTOPOS` for the cursor restore.
    fn set_buffer_text(&mut self, text: &str, cursor: u64);

    /// Pull the current buffer text from the editor control. Called
    /// by `Shell::save_current_to_disk` so that user edits in
    /// Scintilla are written to disk, not the stale shadow held in
    /// `ActiveBuffer::text`. On Win32 this is a `SCI_GETLENGTH` +
    /// `SCI_GETTEXT` round trip via the direct-call API.
    fn get_buffer_text(&mut self) -> String;

    /// Pull the current cursor byte offset from the editor control.
    /// Used by `Shell::save_session` so the next launch can restore
    /// the user's caret position.
    fn get_cursor_pos(&mut self) -> u64;

    /// Update the status bar with encoding, EOL, and any
    /// platform-specific extras.
    fn update_status(&mut self, encoding: &Encoding, eol: Eol, byte_len: u64);

    /// Plugin-driven status-bar override (`NPPM_SETSTATUSBAR`). The
    /// plugin owns `section`'s contents until the next host
    /// `update_status` call repaints the standard fields. Phase 3
    /// platforms route this onto whichever section best matches.
    fn set_plugin_status(&mut self, section: usize, text: &str);
}

/// A modal dialog request the UI must show after `Shell::drain`
/// returns. Holding the dialog *outside* the drain's `&mut Shell`
/// borrow is the only way to safely run a nested Win32 message pump
/// without producing aliasing UB on `WindowState`.
#[derive(Debug, Clone)]
pub enum PendingDialog {
    /// "File changed externally — reload?" prompt for `path`. If the
    /// user accepts, the UI calls `Shell::confirm_reload(path)` to
    /// requeue the load.
    ConfirmReload(PathBuf),
    /// Non-fatal error: title and message strings to display.
    Error { title: String, message: String },
}

/// One open buffer (one tab in the multi-tab UI).
///
/// Phase 3 milestone 6a moves the single-buffer model to a tabbed
/// model. Each `Tab` carries its own path, encoding, EOL, decoded
/// text shadow, pending-load id, and a host-assigned `id` that
/// flows through the plugin ABI as `BufferID` (returned by
/// `NPPM_GETCURRENTBUFFERID`, accepted by `NPPM_GETFULLPATHFROMBUFFERID`,
/// and carried in `NPPN_*.nmhdr.idFrom`).
///
/// `scintilla_doc` is the Scintilla document pointer that backs the
/// tab's editor state. Milestone 6b's UI tab control creates one
/// document per tab via `SCI_CREATEDOCUMENT` and switches the single
/// Scintilla view between them with `SCI_SETDOCPOINTER` on tab
/// click. Milestone 6a leaves it `None` — the existing single-tab
/// UI shares one implicit document.
#[derive(Debug, Default, Clone)]
pub struct Tab {
    /// Stable buffer id assigned at tab-creation time. Zero is
    /// reserved for "no buffer" (matches Notepad++'s convention).
    pub id: i32,
    pub path: Option<PathBuf>,
    pub encoding: Encoding,
    pub eol: Eol,
    pub byte_len: u64,
    /// Most recent decoded text. Held so `save_file` can re-encode
    /// without round-tripping through Scintilla. Phase 3 milestone 6b
    /// pulls the latest text from Scintilla via the direct-call API
    /// (SCI_GETTEXT) at save time, since the user may have edited it.
    pub text: String,
    /// Pending request id from the loader, so we know which load
    /// result actually pertains to this tab (vs. a stale one if
    /// the user dropped a second file before the first finished).
    pub pending_load: Option<RequestId>,
    /// Scintilla document pointer (`sptr_t`). Non-zero once the tab
    /// has been attached to a Scintilla view. Milestone 6b's UI
    /// populates this; milestone 6a leaves it 0.
    pub scintilla_doc: isize,
}

/// Per-call platform handles the UI hands the dispatcher when
/// routing an inbound NPPM_* message. The host crate is platform-
/// agnostic; the UI fills these with whatever opaque pointer types
/// it owns (HWND/HMENU on Win32, GtkWidget* on GTK, NSView*/NSMenu*
/// on Cocoa). All five fields are pointer-sized — `*mut c_void` —
/// so the same struct works on every backend without conditional
/// compilation in `shell`.
///
/// The struct is `Copy` so the wnd_proc can build it on the stack
/// per call without any allocation cost.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
pub struct HostHandles {
    /// Main host window — `nmhdr.hwndFrom` for outbound notifications,
    /// the SendMessage target plugins call into for `NPPM_*`.
    pub npp_hwnd: Hwnd,
    /// Primary Scintilla view's HWND.
    pub scintilla_main: Hwnd,
    /// Secondary Scintilla view's HWND. NULL until split-view lands.
    pub scintilla_secondary: Hwnd,
    /// HMENU for the per-plugin submenu under "Plugins"
    /// (`NPPM_GETMENUHANDLE` with `NPPPLUGINMENU`).
    pub plugin_menu: Hwnd,
    /// HMENU for the entire main menu bar
    /// (`NPPM_GETMENUHANDLE` with `NPPMAINMENU`).
    pub main_menu: Hwnd,
}

#[cfg(target_os = "windows")]
impl HostHandles {
    /// All-NULL handles. **Tests and stub implementations only.**
    /// Production code must supply real handles before any plugin
    /// menu interaction: a plugin querying `NPPM_GETMENUHANDLE` against
    /// a NULL HMENU will likely crash on the receiving side.
    pub fn null() -> Self {
        Self {
            npp_hwnd: core::ptr::null_mut(),
            scintilla_main: core::ptr::null_mut(),
            scintilla_secondary: core::ptr::null_mut(),
            plugin_menu: core::ptr::null_mut(),
            main_menu: core::ptr::null_mut(),
        }
    }
}

/// Application-wide state. Owned by the UI crate's `run()` function;
/// the wnd_proc / event handler reaches into it on every interesting
/// message. On Windows, also owns the `PluginHost` registry — plugins
/// are lazy-loaded, so no DLL is mapped until first menu touch
/// (DESIGN.md §6.4).
pub struct Shell {
    pub session: Session,
    /// Open tabs. Empty at startup; the first `open_file` populates
    /// `tabs[0]` and sets `active_tab = Some(0)`. Subsequent opens
    /// either replace the active tab (if it has no path yet — the
    /// initial-empty case) or push a new tab. The UI drives the
    /// tab strip from `tabs[]` and `active_tab`.
    pub tabs: Vec<Tab>,
    /// Index into [`Self::tabs`] of the currently-active tab.
    /// `None` when no file is open.
    pub active_tab: Option<usize>,
    /// Next buffer id to hand out. Starts at 1 (0 is "no buffer").
    /// Monotonically increasing; never reused so closed-tab ids
    /// don't accidentally resolve a plugin lookup to a different
    /// buffer.
    next_buffer_id: i32,
    /// Plugin registry. Windows-only until Phase 5 wires the same
    /// trait surface against `dlopen`.
    #[cfg(target_os = "windows")]
    plugins: PluginHost,
    /// Outbound NPPN_* notifications queued by shell operations
    /// (load complete, save complete) since the last
    /// [`Self::take_notifications`] drain. The UI fires each one
    /// **after** dropping any `&mut Shell` borrow, since `beNotified`
    /// runs synchronous plugin code that may `SendMessage(NPPM_*)`
    /// back into the wnd_proc.
    #[cfg(target_os = "windows")]
    pending_notifications: Vec<Notification>,
    loader: Loader,
    _loader_shutdown: LoaderShutdown,
    file_watcher: FileWatcher,
    /// Receivers the UI thread drains on every wake. Producer threads
    /// have already called `wake` by the time something appears here.
    load_rx: Receiver<LoadResult>,
    change_rx: Receiver<FileChange>,
}

impl Shell {
    /// Create a `Shell` and wire up the cross-thread plumbing.
    ///
    /// `wake` is invoked by every producer thread after it sends a
    /// result, so the UI thread can drain its channels in the next
    /// message-pump iteration. On Win32 this is
    /// `PostMessage(hwnd, WM_APP_WAKE, 0, 0)` wrapped in an `Arc`.
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, ShellError> {
        // Loader: forward results into a Shell-owned channel so we can
        // wake the UI thread on each result without touching the
        // existing Loader API.
        let (loader, load_rx_inner, loader_shutdown) = Loader::spawn();
        let (load_tx_outer, load_rx_outer) = unbounded::<LoadResult>();
        spawn_forwarder(load_rx_inner, load_tx_outer, wake.clone(), "load-forwarder");

        // FileWatcher: same pattern.
        let (fc_tx_inner, fc_rx_inner) = unbounded::<FileChange>();
        let file_watcher =
            FileWatcher::new(fc_tx_inner).map_err(|e| ShellError::WatcherInit(e.to_string()))?;
        let (fc_tx_outer, fc_rx_outer) = unbounded::<FileChange>();
        spawn_forwarder(fc_rx_inner, fc_tx_outer, wake, "watch-forwarder");

        Ok(Self {
            session: Session::new(),
            tabs: Vec::new(),
            active_tab: None,
            next_buffer_id: 1,
            #[cfg(target_os = "windows")]
            plugins: PluginHost::new(),
            #[cfg(target_os = "windows")]
            pending_notifications: Vec::new(),
            loader,
            _loader_shutdown: loader_shutdown,
            file_watcher,
            load_rx: load_rx_outer,
            change_rx: fc_rx_outer,
        })
    }

    /// Read access to the currently-active tab, or `None` if no
    /// file is open. The UI uses this to populate the title bar
    /// and status fields.
    pub fn active(&self) -> Option<&Tab> {
        self.active_tab.and_then(|i| self.tabs.get(i))
    }

    /// Mutable access to the currently-active tab. Internal Shell
    /// methods use this; the UI should go through high-level
    /// operations like `save_current_to_disk` rather than mutating
    /// directly.
    fn active_mut(&mut self) -> Option<&mut Tab> {
        let idx = self.active_tab?;
        self.tabs.get_mut(idx)
    }

    /// Allocate a fresh buffer id. Caller is responsible for
    /// installing it on a `Tab`. Bumps the `next_buffer_id` counter
    /// without reuse — see the field doc.
    ///
    /// Uses `checked_add` rather than `saturating_add`: saturation
    /// would silently start handing out colliding ids at
    /// `i32::MAX`, breaking the per-tab plugin-ABI BufferID
    /// contract. Two billion tab opens in a single session is
    /// unreachable in practice, but a hostile in-process plugin
    /// could in principle call `NPPM_DOOPEN` in a tight loop —
    /// the panic here turns that DoS path into a clean abort
    /// rather than a silent ABI break. The panic is caught by the
    /// wnd_proc's `catch_unwind` wrappers.
    fn allocate_buffer_id(&mut self) -> i32 {
        let id = self.next_buffer_id;
        self.next_buffer_id = self
            .next_buffer_id
            .checked_add(1)
            .expect("buffer id space exhausted (i32::MAX opens in one session)");
        id
    }

    /// Drain queued plugin notifications. Called by the UI after
    /// [`Self::drain`] (or any operation that may have queued a
    /// notification) — the UI fires each one through
    /// [`Self::notify_plugins`] **after** dropping the `&mut Shell`
    /// borrow, since `beNotified` runs synchronous plugin code that
    /// may re-enter the wnd_proc.
    #[cfg(target_os = "windows")]
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Queue `NPPN_BUFFERACTIVATED` for the currently-active tab.
    /// Call sites: `apply_load_result` after a fresh open (the new
    /// tab becomes active), `HostBridge::switch_to_file` when a
    /// plugin activates an existing tab, and `ui_win32`'s
    /// `handle_tab_selchange` on a user tab click. Each delivery
    /// fires after the `&mut Shell` borrow drops, so plugin
    /// `beNotified` callbacks can `SendMessage(NPPM_*)` back
    /// without aliasing UB.
    ///
    /// Idempotent — a no-op when there's no active tab. Safe to
    /// call from sites that may race with a close-tab path.
    #[cfg(target_os = "windows")]
    pub fn queue_buffer_activated(&mut self) {
        if let Some(tab) = self.active() {
            let buffer_id = tab.id as isize;
            self.pending_notifications
                .push(Notification::BufferActivated { buffer_id });
        }
    }

    /// Enumerate plugin DLLs in `dir`. No DLL is mapped — the loader
    /// only records paths; first-touch load happens when a plugin's
    /// menu is opened (DESIGN.md §6.4). Returns the count discovered.
    /// A non-existent directory is not an error (first-run case).
    #[cfg(target_os = "windows")]
    pub fn discover_plugins(&mut self, dir: &Path) -> std::io::Result<usize> {
        self.plugins.discover(dir)
    }

    /// Total plugins known to the host (any lifecycle state).
    #[cfg(target_os = "windows")]
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// Broadcast `notification` to every loaded plugin's `beNotified`.
    /// `npp_hwnd` is reported in `SCNotification.nmhdr.hwndFrom`.
    /// Synchronous on the UI thread (parity with Notepad++); plugins
    /// that block here block the host.
    #[cfg(target_os = "windows")]
    pub fn notify_plugins(&self, notification: Notification, npp_hwnd: Hwnd) {
        notify_all(&self.plugins, &notification, npp_hwnd);
    }

    /// Load every plugin currently in the `Pending` state. Called by
    /// the UI on first menu-popup open (lazy-load — DESIGN.md §6.4).
    /// Already-loaded plugins are skipped; failed plugins are recorded
    /// on the `PluginInfo` and surface to the UI via [`Self::plugin_load_outcomes`].
    ///
    /// `npp_data` is the `NppData` struct each plugin's `setInfo`
    /// receives. The same struct is passed to every plugin loaded by
    /// this call.
    #[cfg(target_os = "windows")]
    pub fn ensure_plugins_loaded(&mut self, npp_data: NppData) {
        let pending: Vec<usize> = self
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.is_loaded() && p.failed_reason().is_none())
            .map(|(i, _)| i)
            .collect();
        for idx in pending {
            if let Err(e) = self.plugins.load(idx, npp_data) {
                tracing::warn!(idx = idx, error = %e, "plugin load failed");
            }
        }
    }

    /// Iterate the (display name, FuncItem array) pairs of every
    /// loaded plugin. The UI uses this to populate the per-plugin
    /// submenu after [`Self::ensure_plugins_loaded`].
    ///
    /// Plugins with zero FuncItems are skipped — they're loaded but
    /// contribute no menu items (typically `beNotified`-only plugins).
    #[cfg(target_os = "windows")]
    pub fn loaded_plugin_funcs(&self) -> impl Iterator<Item = (String, &[FuncItem])> {
        self.plugins
            .iter()
            .filter(|p| p.is_loaded())
            .filter_map(|p| {
                let funcs = p.func_items()?;
                if funcs.is_empty() {
                    None
                } else {
                    Some((p.display_label(), funcs))
                }
            })
    }

    /// Find the plugin callback registered for menu-command id
    /// `cmd_id`. Returns the bare `PluginCmd` function pointer so
    /// the caller can invoke it after dropping any `&mut Shell`
    /// borrow — invoking the callback while a borrow is alive
    /// would be aliasing UB if the plugin synchronously
    /// `SendMessage`s an `NPPM_*` back into our wnd_proc.
    ///
    /// The returned pointer is valid as long as the plugin's DLL
    /// stays loaded (i.e. for the lifetime of `self`).
    #[cfg(target_os = "windows")]
    pub fn lookup_plugin_command(&self, cmd_id: i32) -> Option<PluginCmd> {
        self.plugins.lookup_cmd(cmd_id)
    }

    /// Route a wnd_proc-received NPPM_* message into the plugin
    /// dispatcher. Returns `Some(lresult)` if the message was handled
    /// (the wnd_proc returns this from `WindowProc`), or `None` if
    /// the message is outside the NPPM_* range and the wnd_proc
    /// should fall through to its default handler.
    ///
    /// # Safety
    ///
    /// Several NPPM_* messages dereference plugin-supplied raw
    /// pointers in `lparam`. The caller must:
    ///
    /// * invoke this only from the UI thread that owns
    ///   `handles.npp_hwnd`,
    /// * pass `(msg, wparam, lparam)` triples received from a real
    ///   wnd_proc dispatch (synthesizing calls outside that flow is
    ///   undefined behaviour on the plugin's behalf),
    /// * supply a `handles` struct whose five fields all belong to
    ///   the same top-level window that received `msg` — mixing
    ///   handles across windows produces wrong results without any
    ///   diagnostic.
    ///
    /// At that point the plugin is the trust boundary and is bound
    /// by the documented NPPM_* ABI in
    /// `plugins/nppcompat-headers/Notepad_plus_msgs.h`.
    #[cfg(target_os = "windows")]
    #[must_use = "the wnd_proc must return the LRESULT this produced for handled messages, or fall through for None"]
    pub unsafe fn dispatch_plugin_message<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        handles: HostHandles,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> Option<isize> {
        let mut bridge = HostBridge {
            shell: self,
            ui,
            handles,
        };
        // SAFETY: forwarded; documented above.
        unsafe { dispatch_nppm(&mut bridge, msg, wparam, lparam) }
    }

    /// Queue a file open. The load runs on a worker thread; the
    /// result lands on the load-results channel and the UI drains
    /// it on the next wake.
    ///
    /// **Tab routing (Phase 3 milestone 6a):** if there is no active
    /// tab, or the active tab has no path yet (the
    /// just-launched-empty case), the load result populates the
    /// active tab in place. Otherwise a new tab is appended and
    /// becomes active.
    pub fn open_file(&mut self, path: PathBuf) {
        let Some(req_id) = self.loader.open(path.clone()) else {
            return;
        };
        // Decide where the load result will land. If the active
        // tab is empty, reuse it; otherwise allocate a fresh tab
        // now so we have a buffer id to associate with `req_id`.
        let target_idx = match self.active_tab {
            Some(i) if self.tabs.get(i).map(|t| t.path.is_none()).unwrap_or(false) => i,
            _ => {
                let id = self.allocate_buffer_id();
                self.tabs.push(Tab {
                    id,
                    pending_load: Some(req_id),
                    ..Tab::default()
                });
                let new_idx = self.tabs.len() - 1;
                self.active_tab = Some(new_idx);
                return;
            }
        };
        // Reusing an empty tab — assign an id if it didn't have one
        // and set the pending-load marker.
        let needs_id = self
            .tabs
            .get(target_idx)
            .map(|t| t.id == 0)
            .unwrap_or(false);
        let new_id = if needs_id {
            Some(self.allocate_buffer_id())
        } else {
            None
        };
        if let Some(tab) = self.tabs.get_mut(target_idx) {
            if let Some(id) = new_id {
                tab.id = id;
            }
            tab.pending_load = Some(req_id);
        } else {
            // active_tab pointed at a missing index — recover by
            // creating a new tab.
            let id = self.allocate_buffer_id();
            self.tabs.push(Tab {
                id,
                pending_load: Some(req_id),
                ..Tab::default()
            });
            self.active_tab = Some(self.tabs.len() - 1);
        }
    }

    /// Drain pending tasks and apply each to the shell + UI. Returns
    /// any dialogs the UI must show *after* this call returns — the
    /// `&mut Shell` borrow ends with the function, so a nested message
    /// pump (e.g. `MessageBoxW`) inside the dialog code can't re-enter
    /// the wnd_proc and produce aliasing UB on the per-window state.
    pub fn drain<U: UiPlatform>(&mut self, ui: &mut U) -> Vec<PendingDialog> {
        let mut pending = Vec::new();
        while let Ok(result) = self.load_rx.try_recv() {
            self.apply_load_result(ui, result, &mut pending);
        }
        while let Ok(change) = self.change_rx.try_recv() {
            self.apply_file_change(change, &mut pending);
        }
        pending
    }

    /// Confirm a deferred reload: requeue the file through the loader.
    /// Called by the UI after the user clicks Yes on the reload prompt
    /// returned in [`PendingDialog::ConfirmReload`].
    pub fn confirm_reload(&mut self, path: PathBuf) {
        self.open_file(path);
    }

    fn apply_load_result<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        result: LoadResult,
        pending: &mut Vec<PendingDialog>,
    ) {
        match result {
            Ok(loaded) => {
                // Find the tab whose pending_load matches this id — that
                // tells us which tab the user requested this load for.
                // Anything else is stale.
                let Some(target_idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.pending_load == Some(loaded.id))
                else {
                    tracing::debug!(
                        stale_id = loaded.id,
                        "discarding stale load result (no matching pending tab)"
                    );
                    return;
                };

                // Begin watching the new file before pushing into
                // the editor — if the watch fails the user still gets
                // the buffer; we just won't catch external changes.
                if let Err(e) = self.file_watcher.watch(&loaded.path) {
                    tracing::warn!(error = %e, path = ?loaded.path, "failed to watch file");
                }

                let cursor = self
                    .session
                    .tabs
                    .iter()
                    .find(|t| t.path == loaded.path)
                    .map(|t| t.cursor)
                    .unwrap_or(0);

                // Write the tab fields first so any UI calls below
                // observe a tab in its post-load state. The borrow
                // ends at the end of the if-let-else.
                let Some(tab) = self.tabs.get_mut(target_idx) else {
                    return;
                };
                tab.pending_load = None;
                tab.path = Some(loaded.path.clone());
                tab.encoding = loaded.encoding.clone();
                tab.eol = loaded.eol;
                tab.byte_len = loaded.byte_len;
                tab.text = loaded.text.clone();
                let stored_doc = tab.scintilla_doc;
                #[cfg(target_os = "windows")]
                let buffer_id = tab.id as isize;

                // Apply UI updates only when this load targets the
                // **active** tab. `activate_tab` rebinds the single
                // Scintilla view to the supplied document — calling
                // it for a non-active tab would leave the view
                // pointed at the wrong document for the rest of
                // the drain (and forever, since nothing else
                // re-binds). Phase 3's `open_file` always makes the
                // newly-opened tab active, so the active branch is
                // the only path actually exercised by the v1 demo.
                //
                // Background-tab loads (session-restore opening
                // multiple files at once, or
                // `NPPM_DOOPEN`-driven loads onto an already-active
                // tab) get their `text`/`encoding` fields populated
                // above, but their Scintilla document stays
                // uncreated until first activation by a tab click
                // — see `handle_tab_selchange`. Milestone 6c will
                // add a `populate_background_tab` flow that
                // creates + fills the document without disturbing
                // the visible view.
                // Queue NPPN_FILEOPENED for the loaded plugins. The UI
                // drains the queue via take_notifications() after
                // dropping its &mut Shell borrow — required because
                // beNotified runs synchronous plugin code that may
                // re-enter the wnd_proc. Pushed BEFORE
                // NPPN_BUFFERACTIVATED below so the delivery order
                // matches Notepad++'s canonical sequence: file-open
                // events fire before buffer-activation events on
                // the same load.
                #[cfg(target_os = "windows")]
                self.pending_notifications
                    .push(Notification::FileOpened { buffer_id });

                let is_active = self.active_tab == Some(target_idx);
                if is_active {
                    let bound_doc = ui.activate_tab(target_idx, stored_doc);
                    if let Some(tab) = self.tabs.get_mut(target_idx) {
                        tab.scintilla_doc = bound_doc;
                    }
                    ui.set_buffer_text(&loaded.text, cursor);
                    ui.update_status(&loaded.encoding, loaded.eol, loaded.byte_len);

                    // The just-loaded tab is now the user-visible
                    // buffer — fire NPPN_BUFFERACTIVATED so plugins
                    // observing buffer changes pick up the new id.
                    // Notification queue is Windows-gated.
                    #[cfg(target_os = "windows")]
                    self.queue_buffer_activated();
                }
            }
            Err(err) => {
                // A failed load on a fresh tab (one that never had a
                // path) leaves an orphan: nonzero buffer id, but
                // `path = None`. Plugins gate on `id != 0 ⇒ path
                // is Some`; preserving the orphan would silently
                // break that invariant. Find the matching tab and
                // either remove it (fresh open) or just clear
                // `pending_load` (reload of a tab with prior
                // contents — keep its previous path/text).
                let target = self
                    .tabs
                    .iter()
                    .position(|t| t.pending_load == Some(err.id));
                if let Some(idx) = target {
                    let is_fresh = self.tabs[idx].path.is_none();
                    if is_fresh {
                        self.tabs.remove(idx);
                        self.active_tab = match self.active_tab {
                            Some(active_idx) if active_idx == idx => {
                                if self.tabs.is_empty() {
                                    None
                                } else if active_idx >= self.tabs.len() {
                                    Some(self.tabs.len() - 1)
                                } else {
                                    Some(active_idx)
                                }
                            }
                            Some(active_idx) if active_idx > idx => Some(active_idx - 1),
                            other => other,
                        };
                    } else {
                        // Reload failed; keep the tab's prior contents,
                        // just drop the pending marker.
                        self.tabs[idx].pending_load = None;
                    }
                }
                pending.push(PendingDialog::Error {
                    title: "Open failed".to_string(),
                    message: format!("{}: {}", err.path.display(), err.error),
                });
            }
        }
    }

    fn apply_file_change(&mut self, change: FileChange, pending: &mut Vec<PendingDialog>) {
        // Path comparison is by exact equality. Windows can spell
        // the same file as a long name, an 8.3 short name, or a
        // junction-routed path; all three would silently miss the
        // reload prompt here. Plugin `NPPM_RELOADFILE` and user
        // junction-traversal both reach this code path, and the
        // canonicalize-both-sides hardening is tracked for milestone 5.
        match change {
            FileChange::Modified(path) => {
                if self
                    .tabs
                    .iter()
                    .any(|t| t.path.as_deref() == Some(path.as_path()))
                {
                    pending.push(PendingDialog::ConfirmReload(path));
                }
            }
            FileChange::Removed(path) => {
                if self
                    .tabs
                    .iter()
                    .any(|t| t.path.as_deref() == Some(path.as_path()))
                {
                    pending.push(PendingDialog::Error {
                        title: "File removed".to_string(),
                        message: format!(
                            "{} was deleted or moved externally. The buffer is still in memory.",
                            path.display()
                        ),
                    });
                }
            }
        }
    }

    /// Write the current buffer to its associated path, atomically.
    ///
    /// Steps:
    ///   1. Pull live text from the editor (covers user edits in
    ///      Scintilla that haven't been mirrored back to the shadow).
    ///   2. Re-encode in the buffer's current encoding.
    ///   3. Write to a sibling tempfile, fsync, persist over the
    ///      destination — same pattern as `Session::save_to_xml`.
    ///      Power-loss safety: the file on disk is always either the
    ///      pre-save bytes or fully the new bytes, never torn.
    ///
    /// The file watcher is briefly unregistered around the write so
    /// our own save doesn't trigger a "file changed externally —
    /// reload?" prompt. **Known limitation:** an *external* write by
    /// another process during this same window is silently missed.
    /// Phase 3+ may switch to inode/serial-number tracking instead of
    /// unwatch/rewatch.
    pub fn save_current_to_disk<U: UiPlatform>(&mut self, ui: &mut U) -> Result<(), ShellError> {
        use std::io::Write;

        // Snapshot what we need from the active tab so we can release
        // its borrow before calling the watcher and the I/O helpers
        // (which take their own &mut self).
        let (path, encoding) = {
            let tab = self.active().ok_or(ShellError::NoActivePath)?;
            (
                tab.path.as_ref().ok_or(ShellError::NoActivePath)?.clone(),
                tab.encoding.clone(),
            )
        };
        let text = ui.get_buffer_text();
        let bytes = codepp_core::encoding::encode(&text, &encoding)
            .map_err(|e| ShellError::Encoding(e.to_string()))?;

        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent_dir = parent.unwrap_or_else(|| std::path::Path::new("."));

        let was_watching = self.file_watcher.unwatch(&path).is_ok();

        let write_result = (|| -> Result<(), ShellError> {
            let mut tmp = tempfile::Builder::new()
                .prefix(".codepp-save-")
                .suffix(".tmp")
                .tempfile_in(parent_dir)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.write_all(&bytes)
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.as_file_mut()
                .sync_all()
                .map_err(|e| ShellError::Io(e.to_string()))?;
            tmp.persist(&path)
                .map_err(|e| ShellError::Io(e.error.to_string()))?;
            Ok(())
        })();

        // Re-watch regardless of save success so we keep tracking the
        // file. A failed save leaves the existing watch handle invalid
        // anyway (the file itself wasn't replaced), so it's harmless.
        if was_watching {
            if let Err(e) = self.file_watcher.watch(&path) {
                tracing::warn!(error = %e, path = ?path, "failed to re-watch after save");
            }
        }
        write_result?;

        // Use the byte count of what we just encoded — re-reading from
        // disk would race with a process that swapped the file between
        // our `persist` and the `metadata` call (TOCTOU), and produce
        // a status-bar size that doesn't match the bytes we just
        // wrote. We already know the size; use it.
        if let Some(tab) = self.active_mut() {
            tab.text = text;
            tab.byte_len = bytes.len() as u64;
        }

        // Queue NPPN_FILESAVED. The UI fires it via take_notifications()
        // after this method returns and after dropping any &mut Shell
        // borrow.
        #[cfg(target_os = "windows")]
        {
            let buffer_id = self.active().map(|t| t.id as isize).unwrap_or(0);
            self.pending_notifications
                .push(Notification::FileSaved { buffer_id });
        }

        Ok(())
    }

    /// Persist the open-tab list to `session.xml` at the configured
    /// path. Called on clean shutdown. The active tab's cursor is
    /// pulled live from the editor so the next launch restores the
    /// caret where the user left it; non-active tabs use cursor 0
    /// for now (milestone 6b's UI tab control records per-tab cursors
    /// at switch time).
    pub fn save_session<U: UiPlatform>(&self, ui: &mut U) -> Result<(), ShellError> {
        let Some(path) = codepp_platform::session_xml_path() else {
            // No config dir resolvable (sandboxed environment); skip
            // session save silently.
            return Ok(());
        };
        let mut session = Session::new();
        for (idx, tab) in self.tabs.iter().enumerate() {
            let Some(tab_path) = &tab.path else {
                continue; // unsaved/empty tab
            };
            // Active tab cursor comes from the editor; others are 0
            // until milestone 6b's tab-switch hook persists per-tab
            // cursors.
            let cursor = if Some(idx) == self.active_tab {
                ui.get_cursor_pos()
            } else {
                0
            };
            session.tabs.push(codepp_core::Tab {
                path: tab_path.clone(),
                cursor,
                encoding: tab.encoding.clone(),
                eol: tab.eol,
            });
        }
        session.active = self.active_tab.and_then(|active_idx| {
            // Map the tabs[] index to the index inside session.tabs[],
            // accounting for any unsaved tabs we skipped above.
            let mut session_idx = 0usize;
            for (i, tab) in self.tabs.iter().enumerate() {
                if tab.path.is_none() {
                    continue;
                }
                if i == active_idx {
                    return Some(session_idx);
                }
                session_idx += 1;
            }
            None
        });
        session
            .save_to_xml(&path)
            .map_err(|e| ShellError::Session(e.to_string()))?;
        Ok(())
    }

    /// Read `session.xml` and return the first tab to restore as the
    /// "initial open" path the UI passes to [`Self::open_file`]. The
    /// remaining tabs in the session file are left in
    /// `self.session.tabs` and milestone 6b's UI iterates them to
    /// open each as a new tab.
    pub fn load_session(&mut self) -> Option<PathBuf> {
        let path = codepp_platform::session_xml_path()?;
        let session = Session::load_from_xml(&path).ok()?;
        let tab = session.tabs.first()?.clone();
        self.session = session;
        Some(tab.path)
    }
}

/// Errors surfaced by `Shell` operations.
#[derive(Debug)]
pub enum ShellError {
    WatcherInit(String),
    NoActivePath,
    Encoding(String),
    Io(String),
    Session(String),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::WatcherInit(s) => write!(f, "watcher init failed: {s}"),
            ShellError::NoActivePath => write!(f, "no active file path"),
            ShellError::Encoding(s) => write!(f, "encoding error: {s}"),
            ShellError::Io(s) => write!(f, "I/O error: {s}"),
            ShellError::Session(s) => write!(f, "session error: {s}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// Adapter that exposes `Shell` + the per-call platform handles to the
/// plugin-host's `HostServices` trait. Lives only for the duration of
/// one `dispatch_plugin_message` call; carries `&mut Shell` and
/// `&mut U` so the trait's mutating methods (open, save, status-bar)
/// reach the right places without `Shell` having to know any HWND.
#[cfg(target_os = "windows")]
struct HostBridge<'a, U: UiPlatform> {
    shell: &'a mut Shell,
    ui: &'a mut U,
    handles: HostHandles,
}

#[cfg(target_os = "windows")]
impl<U: UiPlatform> HostServices for HostBridge<'_, U> {
    fn current_scintilla_hwnd(&self) -> Hwnd {
        self.handles.scintilla_main
    }

    fn scintilla_hwnd_for_view(&self, view: i32) -> Hwnd {
        match view {
            0 => self.handles.scintilla_main,
            1 => self.handles.scintilla_secondary,
            _ => core::ptr::null_mut(),
        }
    }

    fn current_buffer_id(&self) -> isize {
        // 0 means "no buffer" — matches the Notepad++ convention.
        // Active tab's id otherwise. A tab whose load is still
        // pending also reports its id (the buffer exists; only the
        // contents are still arriving), so plugins can address
        // newly-opened tabs without waiting for the load to finish.
        self.shell.active().map(|t| t.id as isize).unwrap_or(0)
    }

    fn buffer_path(&self, id: isize) -> Option<PathBuf> {
        // Linear scan over tabs. Phase 3's tab counts are small
        // (handful at most); a HashMap<id, idx> is overkill until
        // multi-window or session-restore lands hundreds of tabs.
        self.shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .and_then(|t| t.path.clone())
    }

    fn buffer_lang_type(&self, _id: isize) -> i32 {
        // L_TEXT (0). Phase 4 wires this through the lexer registry.
        0
    }

    fn plugins_config_dir(&self) -> PathBuf {
        // Sandboxed runners may not resolve a config dir. Fall back
        // to the OS temp dir rather than the process CWD: a process
        // started from a network share or a directory the user does
        // not own would otherwise hand plugins an attacker-writable
        // location for their config files. `temp_dir` is always
        // user-owned on a healthy system. Plugins that depend on
        // cross-launch persistence still degrade gracefully — the
        // configuration goes to a tempdir for the duration of this
        // launch rather than crashing the host.
        codepp_platform::plugins_config_dir().unwrap_or_else(std::env::temp_dir)
    }

    fn menu_handle(&self, which: i32) -> Hwnd {
        match which {
            NPPPLUGINMENU => self.handles.plugin_menu,
            NPPMAINMENU => self.handles.main_menu,
            _ => core::ptr::null_mut(),
        }
    }

    fn set_status_bar(&mut self, section: usize, text: String) {
        self.ui.set_plugin_status(section, &text);
    }

    fn open_file(&mut self, path: PathBuf) {
        // Path comes verbatim from the plugin via NPPM_DOOPEN. Code++
        // does not confine plugin-driven opens — a plugin can ask
        // the host to open any file the host process can read. This
        // matches Notepad++'s own contract; plugins are in-process
        // and trusted with the host's full address space, so a
        // path-confinement check would be defense in depth against
        // a threat model where the plugin is hostile-but-not-yet-
        // executing-arbitrary-code, which is a narrow window.
        //
        // TODO milestone 5 hardening pass: reject `\\.\` device
        // paths and `\\?\` extended-length paths whose target
        // resolves outside the user's home tree, as a courtesy
        // against accidental plugin bugs. Not security-critical
        // given the threat model; included for sharper diagnostics.
        self.shell.open_file(path);
    }

    fn reload_file(&mut self, path: Option<PathBuf>) {
        let path = path.or_else(|| self.shell.active().and_then(|t| t.path.clone()));
        if let Some(p) = path {
            self.shell.confirm_reload(p);
        }
    }

    fn save_current_file(&mut self) {
        if let Err(e) = self.shell.save_current_to_disk(self.ui) {
            tracing::warn!(error = %e, "plugin-triggered save failed");
        }
    }

    fn switch_to_file(&mut self, path: PathBuf) -> bool {
        // If the path is already open in some tab, activate it.
        // Otherwise route through the regular open path, which
        // either reuses an empty active tab or pushes a new one.
        if let Some(idx) = self
            .shell
            .tabs
            .iter()
            .position(|t| t.path.as_deref() == Some(path.as_path()))
        {
            // Skip the queue entirely if the target is already
            // active — `NPPN_BUFFERACTIVATED` signals "the user's
            // active buffer changed," and a switch to the
            // already-active buffer is not such a change. Plugins
            // that audit-log activations would otherwise log
            // false positives, and plugins that reset buffer-local
            // state on activation would clobber valid state.
            if self.shell.active_tab != Some(idx) {
                self.shell.active_tab = Some(idx);
                self.shell.queue_buffer_activated();
            }
            true
        } else {
            self.shell.open_file(path);
            // open_file's load completion will fire BUFFERACTIVATED
            // for the new tab via apply_load_result.
            true
        }
    }

    fn menu_command(&mut self, cmd_id: i32) {
        // Built-in menu cmdIDs (IDM_FILE_OPEN, IDM_EDIT_COPY, …) get
        // wired in milestone 6 alongside the full menu set. Phase 3
        // milestone 4 logs and ignores so plugins don't crash.
        tracing::trace!(cmd = cmd_id, "NPPM_MENUCOMMAND (no handler wired yet)");
    }

    fn make_current_buffer_dirty(&mut self) {
        // Dirty-state tracking lives entirely in Scintilla
        // (`SCI_GETMODIFY`); plugins calling this expect the title
        // bar to update and the save-on-exit prompt to appear.
        // Title-bar dirty glyph is a milestone 6 task; for now log.
        tracing::trace!("NPPM_MAKECURRENTBUFFERDIRTY (no-op, tracked by Scintilla)");
    }

    fn set_buffer_lang_type(&mut self, _id: isize, _lang: i32) -> bool {
        // Phase 4 wires this through the lexer registry; until
        // then refusing (FALSE) tells the plugin nothing changed.
        false
    }

    fn set_menu_item_check(&mut self, _cmd_id: i32, _checked: bool) {
        // Forwarded to native menu API in milestone 6.
        tracing::trace!("NPPM_SETMENUITEMCHECK (no-op until full menu set lands)");
    }

    fn activate_doc(&mut self, view: i32, pos: i32) -> bool {
        // Phase 3 single-tab: success is the only valid answer (only
        // one document exists, so "activate it" is always true).
        // Multi-tab milestone 6 will route to the per-tab id.
        tracing::trace!(
            view = view,
            pos = pos,
            "NPPM_ACTIVATEDOC (no-op, single-tab Phase 3)"
        );
        true
    }
}

/// Spawn a forwarder thread that pumps items from `src` into `dst`
/// and calls `wake` after each successful send. Used so the shell
/// can wake the UI thread on every producer event without modifying
/// the producer crates' APIs.
fn spawn_forwarder<T: Send + 'static>(
    src: Receiver<T>,
    dst: Sender<T>,
    wake: Arc<dyn Fn() + Send + Sync>,
    name: &'static str,
) {
    thread::Builder::new()
        .name(format!("codepp-{name}"))
        .spawn(move || {
            while let Ok(item) = src.recv() {
                if dst.send(item).is_err() {
                    break;
                }
                wake();
            }
        })
        .expect("forwarder thread spawn");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Test UiPlatform that records calls — lets us assert the shell
    /// reaches the right operations without needing real Win32.
    #[derive(Default)]
    struct FakeUi {
        buffer_text: String,
        cursor: u64,
        set_text_calls: Vec<(String, u64)>,
        status_calls: Vec<(String, String, u64)>,
        plugin_status_calls: Vec<(usize, String)>,
        /// (tab_idx, in_doc, returned_doc) per `activate_tab` call.
        activate_tab_calls: Vec<(usize, isize, isize)>,
        /// Stand-in for SCI_CREATEDOCUMENT — hand out monotonically
        /// increasing fake "doc pointers" so each tab gets a
        /// distinct value.
        next_fake_doc: isize,
    }

    impl UiPlatform for FakeUi {
        fn activate_tab(&mut self, idx: usize, scintilla_doc: isize) -> isize {
            // If the tab already has a doc, keep it; otherwise hand
            // out a fresh fake pointer (the real Win32 impl calls
            // SCI_CREATEDOCUMENT here).
            let bound = if scintilla_doc != 0 {
                scintilla_doc
            } else {
                self.next_fake_doc += 1;
                self.next_fake_doc
            };
            self.activate_tab_calls.push((idx, scintilla_doc, bound));
            bound
        }
        fn set_buffer_text(&mut self, text: &str, cursor: u64) {
            self.buffer_text = text.to_string();
            self.cursor = cursor;
            self.set_text_calls.push((text.to_string(), cursor));
        }
        fn get_buffer_text(&mut self) -> String {
            self.buffer_text.clone()
        }
        fn get_cursor_pos(&mut self) -> u64 {
            self.cursor
        }
        fn update_status(&mut self, encoding: &Encoding, eol: Eol, byte_len: u64) {
            self.status_calls.push((
                encoding.label().to_string(),
                eol.label().to_string(),
                byte_len,
            ));
        }
        fn set_plugin_status(&mut self, section: usize, text: &str) {
            self.plugin_status_calls.push((section, text.to_string()));
        }
    }

    fn drain_until<F: Fn(&FakeUi, &[PendingDialog]) -> bool>(
        shell: &mut Shell,
        ui: &mut FakeUi,
        predicate: F,
        timeout: Duration,
    ) -> Vec<PendingDialog> {
        let deadline = Instant::now() + timeout;
        let mut all_pending = Vec::new();
        while Instant::now() < deadline {
            let p = shell.drain(ui);
            all_pending.extend(p);
            if predicate(ui, &all_pending) {
                return all_pending;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        all_pending
    }

    #[test]
    fn open_file_pushes_text_through_ui() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "Hello, Code++!").unwrap();

        let wake_count = Arc::new(AtomicUsize::new(0));
        let wake_count_clone = wake_count.clone();
        let wake = Arc::new(move || {
            wake_count_clone.fetch_add(1, Ordering::Relaxed);
        }) as Arc<dyn Fn() + Send + Sync>;

        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());

        let pending = drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.set_text_calls[0].0, "Hello, Code++!");
        assert_eq!(ui.set_text_calls[0].1, 0);
        assert_eq!(ui.status_calls.len(), 1);
        assert_eq!(ui.status_calls[0].0, "UTF-8");
        // Successful loads produce no pending dialogs.
        assert!(pending.is_empty());
        assert!(wake_count.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn open_then_save_round_trips_edited_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round.txt");
        std::fs::write(&path, "original\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Simulate user editing in Scintilla: change the buffer text
        // that get_buffer_text will return.
        ui.buffer_text = "edited\n".to_string();
        shell.save_current_to_disk(&mut ui).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "edited\n");
    }

    #[test]
    fn open_missing_file_emits_error_dialog() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(PathBuf::from("definitely-missing-12345.txt"));

        let pending = drain_until(
            &mut shell,
            &mut ui,
            |_, p| !p.is_empty(),
            Duration::from_secs(2),
        );
        assert!(matches!(
            &pending[0],
            PendingDialog::Error { title, .. } if title == "Open failed"
        ));
        // The fresh tab created for the open should be removed when
        // the load fails — leaving it would orphan a buffer id with
        // `path = None`, breaking the `id != 0 ⇒ path is Some`
        // contract that well-behaved Notepad++ plugins assume.
        assert_eq!(
            shell.tabs.len(),
            0,
            "fresh tab should be removed on load failure"
        );
        assert_eq!(shell.active_tab, None);
    }

    // -- Plugin dispatcher entry-point tests ------------------------
    //
    // These assert that `Shell::dispatch_plugin_message` correctly
    // bridges into the plugin-host dispatcher with the right
    // HostServices view of the active buffer — without needing a
    // real plugin DLL.

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_out_of_range_returns_none() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        // WM_USER + 5 is below NPPMSG (= WM_USER + 1000); dispatcher
        // must yield None so the wnd_proc falls through to its
        // default handler.
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), 0x0400 + 5, 0, 0)
        };
        assert!(r.is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_current_buffer_id_reflects_active_path() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_GETCURRENTBUFFERID: u32 = (0x0400 + 1000) + 60;

        // No active buffer yet — should return 0.
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETCURRENTBUFFERID,
                0,
                0,
            )
        };
        assert_eq!(r, Some(0));

        // Open a file; once the buffer settles, we should report the
        // primary id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "hi").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETCURRENTBUFFERID,
                0,
                0,
            )
        };
        let expected_id = shell.active().expect("active tab").id as isize;
        assert_eq!(r, Some(expected_id));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_full_path_returns_active_buffer_path() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "x").unwrap();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        const NPPM_GETFULLPATHFROMBUFFERID: u32 = (0x0400 + 1000) + 58;
        const MAX_PATH_TCHARS: usize = 260;
        let active_id = shell.active().expect("active tab").id as usize;
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETFULLPATHFROMBUFFERID,
                active_id,
                buf.as_mut_ptr() as isize,
            )
        };
        let written = r.unwrap();
        assert!(written > 0);
        let nul = buf.iter().position(|&u| u == 0).unwrap();
        let got = String::from_utf16_lossy(&buf[..nul]);
        assert_eq!(PathBuf::from(got), path);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_set_status_routes_to_ui() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        const NPPM_SETSTATUSBAR: u32 = (0x0400 + 1000) + 24;
        let text: Vec<u16> = "Hello!".encode_utf16().chain(std::iter::once(0)).collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETSTATUSBAR,
                2,
                text.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(ui.plugin_status_calls, vec![(2usize, "Hello!".to_string())]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_doopen_queues_load() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("via-plugin.txt");
        std::fs::write(&path, "from plugin").unwrap();

        // Build a wide-char path the dispatcher will decode and
        // forward into Shell::open_file.
        let path_str = path.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

        const NPPM_DOOPEN: u32 = (0x0400 + 1000) + 77;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_DOOPEN,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));

        // The open is async; drain until the loader delivers.
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.set_text_calls[0].0, "from plugin");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_save_round_trips_via_dispatcher() {
        // Plugin sends NPPM_SAVECURRENTFILE; the bridge must call
        // through to save_current_to_disk and produce on-disk bytes.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save-via-plugin.txt");
        std::fs::write(&path, "before\n").unwrap();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        ui.buffer_text = "after\n".to_string();
        const NPPM_SAVECURRENTFILE: u32 = (0x0400 + 1000) + 38;
        let r = unsafe {
            shell.dispatch_plugin_message(&mut ui, HostHandles::null(), NPPM_SAVECURRENTFILE, 0, 0)
        };
        assert_eq!(r, Some(1));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "after\n");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn discover_plugins_on_missing_dir_is_zero() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let n = shell
            .discover_plugins(Path::new("definitely-not-a-real-plugin-dir-99999"))
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(shell.plugin_count(), 0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn notify_plugins_with_zero_loaded_is_noop() {
        // Sanity: notify_plugins on a Shell with no loaded plugins
        // must not panic. (No plugins loaded means notify_all has
        // nothing to broadcast to; this asserts the wiring doesn't
        // assume any have been loaded.)
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let shell = Shell::new(wake).unwrap();
        shell.notify_plugins(Notification::Ready, core::ptr::null_mut());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn successful_open_queues_file_opened_notification() {
        // A successful load through the loader → drain → apply path
        // should leave a NPPN_FILEOPENED notification waiting for
        // the UI to fire after dropping its &mut Shell borrow.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Bind the active tab's id from the actual data model rather
        // than asserting a literal — the value happens to be
        // PRIMARY_BUFFER_ID today (next_buffer_id starts at 1) but
        // the contract is "the active tab's id," not "always 1."
        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        // A successful open queues NPPN_FILEOPENED followed by
        // NPPN_BUFFERACTIVATED (matches Notepad++'s canonical
        // ordering: file-open before buffer-activation on the
        // same load).
        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0],
            Notification::FileOpened { buffer_id } if buffer_id == expected_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::BufferActivated { buffer_id } if buffer_id == expected_id
        ));

        // Subsequent take_notifications returns an empty Vec (queue
        // drained) — the UI doesn't re-fire on every wake.
        assert!(shell.take_notifications().is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn successful_save_queues_file_saved_notification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save-notify.txt");
        std::fs::write(&path, "before").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );
        // Reset queue state before testing the save path: drain the
        // `FileOpened` queued by the load so the second
        // `take_notifications` later in this test is unambiguously
        // the response to `save_current_to_disk`.
        let _ = shell.take_notifications();

        ui.buffer_text = "after".to_string();
        shell.save_current_to_disk(&mut ui).unwrap();

        // Bind the active tab's id rather than asserting the literal.
        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 1);
        match &notifications[0] {
            Notification::FileSaved { buffer_id } => assert_eq!(*buffer_id, expected_id),
            other => panic!("expected FileSaved, got {other:?}"),
        }
    }

    // -- Multi-tab data model tests (milestone 6a) ------------------

    #[test]
    fn first_open_populates_tab_zero_in_place() {
        // Initial state: no tabs, no active tab. The first open
        // creates tab[0] (using the empty-active-tab branch's
        // freshly-allocated id) and makes it active. The buffer id
        // is 1 (next_buffer_id starts there).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("first.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        assert_eq!(shell.tabs.len(), 0);
        assert_eq!(shell.active_tab, None);

        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(shell.tabs.len(), 1, "first open creates exactly one tab");
        assert_eq!(shell.active_tab, Some(0));
        let tab = shell.active().unwrap();
        assert_eq!(tab.id, PRIMARY_BUFFER_ID as i32);
        assert_eq!(tab.path, Some(path));
    }

    #[test]
    fn second_open_pushes_new_tab_with_distinct_id() {
        // Two opens of distinct paths: the second one should NOT
        // replace tab[0] (it already has a path) — it pushes a new
        // tab with a fresh id.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        let id_a = shell.active().unwrap().id;

        shell.open_file(path_b.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );

        assert_eq!(shell.tabs.len(), 2, "two distinct opens → two tabs");
        assert_eq!(shell.active_tab, Some(1), "second open is now active");
        let id_b = shell.active().unwrap().id;
        assert_ne!(id_a, id_b, "ids must be distinct across tabs");
        assert_eq!(shell.tabs[0].path.as_ref(), Some(&path_a));
        assert_eq!(shell.tabs[1].path.as_ref(), Some(&path_b));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_activates_existing_tab_without_reopen() {
        // Open two files (one tab each), then have a "plugin" call
        // NPPM_SWITCHTOFILE for the first path. The data-model active
        // index should flip to tab[0] WITHOUT a re-load (the file
        // is already in memory). The bridge implements this directly.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );
        assert_eq!(shell.active_tab, Some(1));
        let load_count_before = ui.set_text_calls.len();

        // NPPM_SWITCHTOFILE = NPPMSG + 37
        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_a_str = path_a.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_a_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(shell.active_tab, Some(0), "switch flipped to existing tab");
        assert_eq!(
            ui.set_text_calls.len(),
            load_count_before,
            "no re-load occurred for an in-memory tab"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn open_queues_buffer_activated_for_the_new_tab() {
        // A successful open completes with the new tab as active —
        // apply_load_result should queue NPPN_BUFFERACTIVATED
        // alongside NPPN_FILEOPENED so plugins observing buffer
        // changes see the new id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activated.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let expected_id = shell.active().expect("active tab").id as isize;
        let notifications = shell.take_notifications();
        // Two queued: FileOpened + BufferActivated, both for the
        // active tab's id. Order: FileOpened first (queued before
        // the activate notification in apply_load_result).
        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0],
            Notification::FileOpened { buffer_id } if buffer_id == expected_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::BufferActivated { buffer_id } if buffer_id == expected_id
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_queues_buffer_activated() {
        // NPPM_SWITCHTOFILE on an already-open path activates the
        // existing tab. The dispatcher path should queue
        // NPPN_BUFFERACTIVATED; without it, plugins observing
        // tab changes via switch wouldn't pick up the move.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path_a.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );
        // Discard the FileOpened/BufferActivated notifications
        // queued by the two opens so the next take_notifications
        // is unambiguously the response to the switch below.
        let _ = shell.take_notifications();

        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_a_str = path_a.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_a_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        let id_a = shell.tabs[0].id as isize;
        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(matches!(
            notifications[0],
            Notification::BufferActivated { buffer_id } if buffer_id == id_a
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn switch_to_file_to_already_active_tab_skips_notification() {
        // NPPM_SWITCHTOFILE for the path that's already active is
        // a tautological switch — no buffer change happened. The
        // dispatcher must NOT queue NPPN_BUFFERACTIVATED, otherwise
        // plugins that audit-log activations log a false positive
        // and plugins that reset buffer-local state on activation
        // clobber valid state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        // Drain the open's notifications.
        let _ = shell.take_notifications();

        const NPPM_SWITCHTOFILE: u32 = (0x0400 + 1000) + 37;
        let path_str = path.to_string_lossy().into_owned();
        let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SWITCHTOFILE,
                0,
                wide.as_ptr() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert!(
            shell.take_notifications().is_empty(),
            "switch-to-already-active must not queue any notification"
        );
    }

    #[test]
    fn activate_tab_returned_doc_persists_on_tab() {
        // First open: tab[0].scintilla_doc starts at 0; the FakeUi's
        // activate_tab hands out a fresh fake pointer (1) and Shell
        // records it on the tab. Second open: tab[1] also starts at
        // 0; FakeUi hands out a different pointer (2). Each tab
        // ends up bound to a distinct document.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, "a").unwrap();
        std::fs::write(&path_b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        shell.open_file(path_a);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        shell.open_file(path_b);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 2,
            Duration::from_secs(2),
        );

        assert_eq!(ui.activate_tab_calls.len(), 2);
        // First call: idx=0, in_doc=0 (uninitialised).
        assert_eq!(ui.activate_tab_calls[0].0, 0);
        assert_eq!(ui.activate_tab_calls[0].1, 0);
        // Second call: idx=1, in_doc=0 (uninitialised again — fresh tab).
        assert_eq!(ui.activate_tab_calls[1].0, 1);
        assert_eq!(ui.activate_tab_calls[1].1, 0);
        // The fake doc pointers handed back land on the tabs and
        // are distinct.
        assert_ne!(shell.tabs[0].scintilla_doc, 0);
        assert_ne!(shell.tabs[1].scintilla_doc, 0);
        assert_ne!(shell.tabs[0].scintilla_doc, shell.tabs[1].scintilla_doc);
    }
}
