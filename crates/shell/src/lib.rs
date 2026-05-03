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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use codepp_core::file::{Loader, LoaderShutdown};
use codepp_core::{Encoding, Eol, LoadResult, RequestId, Session};
use codepp_platform::watch::{FileChange, FileWatcher};
#[cfg(target_os = "windows")]
use codepp_plugin_host::{
    dispatch_nppm, notify_all, HostServices, Hwnd, Notification, PluginHost, NPPMAINMENU,
    NPPPLUGINMENU,
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
    /// Push the given decoded text into the active editor control.
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

/// State for the single tab Phase 2 supports. Phase 3 turns this into
/// `Vec<Tab>` keyed by an `EditorHandle` per buffer.
#[derive(Debug, Default)]
pub struct ActiveBuffer {
    pub path: Option<PathBuf>,
    pub encoding: Encoding,
    pub eol: Eol,
    pub byte_len: u64,
    /// Most recent decoded text. Held so `save_file` can re-encode
    /// without round-tripping through Scintilla. Phase 3 will instead
    /// pull the latest text from Scintilla via the direct-call API
    /// (SCI_GETTEXT) at save time, since the user may have edited it.
    pub text: String,
    /// Pending request id from the loader, so we know which load
    /// result actually pertains to this buffer (vs. a stale one if
    /// the user dropped a second file before the first finished).
    pub pending_load: Option<RequestId>,
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
    pub buffer: ActiveBuffer,
    /// Plugin registry. Windows-only until Phase 5 wires the same
    /// trait surface against `dlopen`.
    #[cfg(target_os = "windows")]
    plugins: PluginHost,
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
            buffer: ActiveBuffer::default(),
            #[cfg(target_os = "windows")]
            plugins: PluginHost::new(),
            loader,
            _loader_shutdown: loader_shutdown,
            file_watcher,
            load_rx: load_rx_outer,
            change_rx: fc_rx_outer,
        })
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

    /// Queue a file open. The result will arrive on the load-results
    /// channel; the UI thread drains it on the next wake.
    pub fn open_file(&mut self, path: PathBuf) {
        if let Some(id) = self.loader.open(path.clone()) {
            self.buffer.pending_load = Some(id);
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
                // Discard if a newer load is pending — the user dropped
                // another file before this one finished.
                if let Some(pid) = self.buffer.pending_load {
                    if pid != loaded.id {
                        tracing::debug!(
                            stale_id = loaded.id,
                            pending_id = pid,
                            "discarding stale load result"
                        );
                        return;
                    }
                }
                self.buffer.pending_load = None;

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

                ui.set_buffer_text(&loaded.text, cursor);
                ui.update_status(&loaded.encoding, loaded.eol, loaded.byte_len);

                self.buffer.path = Some(loaded.path.clone());
                self.buffer.encoding = loaded.encoding;
                self.buffer.eol = loaded.eol;
                self.buffer.byte_len = loaded.byte_len;
                self.buffer.text = loaded.text;
            }
            Err(err) => {
                self.buffer.pending_load = None;
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
        // reload prompt here. Two aggravating factors:
        //
        //   1. The plugin host's `NPPM_RELOADFILE` lets a plugin
        //      stash any path it wants in `self.buffer.path`. A
        //      plugin can therefore deliberately load a file under
        //      a non-canonical spelling and the watcher's later
        //      change events for that file (which arrive under a
        //      *different* spelling) will silently not match.
        //   2. The user can experience the same mismatch by hand,
        //      e.g. opening a file via a junction.
        //
        // Tracker: TODO milestone 5 hardening pass — canonicalize
        // both `loaded.path` (at watch-registration time in
        // `apply_load_result`) and the change-event path here
        // before comparing. Until then, the failure mode is
        // user-visible (no reload prompt → user keeps editing
        // stale content) but bounded by the user's filesystem
        // habits.
        match change {
            FileChange::Modified(path) => {
                if self.buffer.path.as_deref() == Some(path.as_path()) {
                    pending.push(PendingDialog::ConfirmReload(path));
                }
            }
            FileChange::Removed(path) => {
                if self.buffer.path.as_deref() == Some(path.as_path()) {
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

        let path = self
            .buffer
            .path
            .as_ref()
            .ok_or(ShellError::NoActivePath)?
            .clone();
        let text = ui.get_buffer_text();
        let bytes = codepp_core::encoding::encode(&text, &self.buffer.encoding)
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

        self.buffer.text = text;
        // Use the byte count of what we just encoded — re-reading from
        // disk would race with a process that swapped the file between
        // our `persist` and the `metadata` call (TOCTOU), and produce
        // a status-bar size that doesn't match the bytes we just
        // wrote. We already know the size; use it.
        self.buffer.byte_len = bytes.len() as u64;
        Ok(())
    }

    /// Persist the open-tab list to `session.xml` at the configured
    /// path. Called on clean shutdown. Pulls the live cursor from the
    /// editor so the next launch restores the caret where the user
    /// left it.
    pub fn save_session<U: UiPlatform>(&self, ui: &mut U) -> Result<(), ShellError> {
        let Some(path) = codepp_platform::session_xml_path() else {
            // No config dir resolvable (sandboxed environment); skip
            // session save silently.
            return Ok(());
        };
        // For Phase 2's single-tab world the session has at most one
        // tab; Phase 3 reflects every open tab.
        let mut session = Session::new();
        if let Some(buffer_path) = &self.buffer.path {
            session.tabs.push(codepp_core::Tab {
                path: buffer_path.clone(),
                cursor: ui.get_cursor_pos(),
                encoding: self.buffer.encoding.clone(),
                eol: self.buffer.eol,
            });
            session.active = Some(0);
        }
        session
            .save_to_xml(&path)
            .map_err(|e| ShellError::Session(e.to_string()))?;
        Ok(())
    }

    /// Read `session.xml` and return the first tab to restore (Phase
    /// 2 single-tab restore). Returns `None` if there's nothing to
    /// restore.
    pub fn load_session(&mut self) -> Option<PathBuf> {
        let path = codepp_platform::session_xml_path()?;
        let session = Session::load_from_xml(&path).ok()?;
        let tab = session.tabs.into_iter().next()?;
        self.session.tabs.push(tab.clone());
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
        // Phase 3 single-tab: 0 if there's no active file (e.g.
        // before the first open), `PRIMARY_BUFFER_ID` otherwise.
        // Plugins gate on nonzero to mean "there is a buffer."
        if self.shell.buffer.path.is_some() {
            PRIMARY_BUFFER_ID
        } else {
            0
        }
    }

    fn buffer_path(&self, id: isize) -> Option<PathBuf> {
        if id == PRIMARY_BUFFER_ID {
            self.shell.buffer.path.clone()
        } else {
            None
        }
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
        let path = path.or_else(|| self.shell.buffer.path.clone());
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
        // Phase 3 single-tab: same path is a no-op success; a
        // different path goes through the regular open path.
        // Multi-tab (milestone 6) routes to an existing tab when
        // present.
        match &self.shell.buffer.path {
            Some(p) if *p == path => true,
            _ => {
                self.shell.open_file(path);
                true
            }
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
    }

    impl UiPlatform for FakeUi {
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
        assert_eq!(r, Some(PRIMARY_BUFFER_ID));
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
        let mut buf = vec![0u16; MAX_PATH_TCHARS];
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETFULLPATHFROMBUFFERID,
                PRIMARY_BUFFER_ID as usize,
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
}
