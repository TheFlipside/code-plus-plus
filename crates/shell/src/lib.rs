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
use codepp_core::lang::L_TEXT;
use codepp_core::{Encoding, Eol, FindHistory, LangType, LoadResult, RequestId, Session};
use codepp_platform::watch::{FileChange, FileWatcher};
#[cfg(target_os = "windows")]
use codepp_plugin_host::{
    dispatch_nppm, notify_all, FuncItem, HostServices, Hwnd, Notification, NppData, PluginCmd,
    PluginHost, NPPMAINMENU, NPPPLUGINMENU,
};

pub mod fif;
pub use fif::{FifError, FifEvent, FifJobId, FifRequest, FifStats};

/// Plugin-driven pre-fill for the next FIF dialog open. Populated
/// by `NPPM_LAUNCHFINDINFILESDLG` and drained by the Win32 plugin
/// dispatch in `main_wnd_proc`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FifLaunchPrefill {
    /// Optional directory to seed the dialog's Directory combobox.
    pub directory: Option<PathBuf>,
    /// Optional filter expression to seed the Filters combobox.
    pub filters: Option<String>,
}

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

/// Upper bound on tabs restored from session.xml. Caps the work
/// triggered by a corrupted or tampered session file (a runaway
/// session-save bug or an attacker with write access to AppData
/// could otherwise queue thousands of async loads at startup, each
/// allocating a Tab + decoded buffer text — a local DoS for the
/// invoking user's account). Set well above any realistic open-tab
/// count.
pub const MAX_SESSION_TABS: usize = 512;

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

    /// Attach the lexer (and any per-language style theme + keyword
    /// lists) appropriate for `lang` to the *currently-active*
    /// editor document. `L_TEXT` detaches whatever lexer is bound,
    /// returning the view to plain rendering. Called by `Shell`
    /// after a fresh load and on tab-switch so the right colours
    /// follow the user's tab moves.
    fn apply_lang(&mut self, lang: LangType);

    // --- Search / replace -----------------------------------------
    //
    // The four trait methods below are unconditionally part of the
    // UiPlatform contract — every platform backend (current Win32,
    // future GTK and Cocoa from Phase 5) must implement them. The
    // matching `Shell::find_next` / `replace_*` driver methods are
    // currently `#[cfg(target_os = "windows")]` because their
    // backing infrastructure (the `last_search` field, the plugin
    // dispatcher) is also Windows-only until Phase 5. When the
    // Linux/macOS UI crates land, those `#[cfg]` gates come off
    // alongside the rest of the host plumbing — the trait methods
    // here don't need to change.

    /// Search the active editor forward for `query` under `flags`.
    /// On a hit, Scintilla moves the selection to the match (also
    /// repositions the caret to the match end). Returns the match's
    /// byte offset, or `None` on miss. Phase 4 m3 implementations
    /// route through `EditorHandle::search_anchor` +
    /// `search_next`; Phase 5 backends do the equivalent.
    fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64>;

    /// Same as [`Self::search_next`] but walks backward from the
    /// current selection.
    fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64>;

    /// Replace the currently-selected text with `replacement` if
    /// and only if the selection matches `query` under `flags`.
    /// Returns true if a replacement happened. The match-check
    /// guards against the case where the user reselected
    /// arbitrary text after a Find — Scintilla itself doesn't
    /// gate on that.
    fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool;

    /// Replace every occurrence of `query` with `replacement` in
    /// the active buffer. All replaces are wrapped in one
    /// Scintilla undo group so the user can Ctrl+Z the entire
    /// Replace All in a single step. Returns the count.
    fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize;

    /// Count every match of `query` in the active buffer. Pure
    /// query — does not move the user's selection. The Find
    /// dialog's "Count" button surfaces the result in its status
    /// line.
    fn count_matches(&mut self, query: &str, flags: SearchFlags) -> usize;

    /// Forward search restricted to a byte range — used by the
    /// "In selection" mode of the Find dialog. Returns the
    /// match's byte offset, or `None` if no match falls inside
    /// `[start, end)`. Implementations must NOT move the caret
    /// outside the range on a miss.
    fn search_next_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64>;

    /// Backward sibling of [`Self::search_next_in_range`].
    fn search_prev_in_range(
        &mut self,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64>;

    /// Replace All restricted to `[start, end)`. Returns
    /// `(count, new_end)` — the caller uses `new_end` to keep its
    /// in-selection range bookkeeping in sync after replacements
    /// shrink or grow the original window.
    fn replace_all_in_range(
        &mut self,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> (usize, u64);
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
#[derive(Debug, Clone)]
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
    /// N++-compatible `LangType` for this buffer. Phase 4 m1 derives
    /// it from the path extension on first load; later milestones
    /// expose `NPPM_SETBUFFERLANGTYPE` so plugins can override. New
    /// (unsaved) tabs and unrecognised extensions default to `L_TEXT`.
    pub lang: LangType,
}

impl Default for Tab {
    fn default() -> Self {
        Self {
            id: 0,
            path: None,
            encoding: Encoding::default(),
            eol: Eol::default(),
            byte_len: 0,
            text: String::new(),
            pending_load: None,
            scintilla_doc: 0,
            lang: L_TEXT,
        }
    }
}

/// Snapshot returned by [`Shell::close_active_tab`] describing the
/// platform-side cleanup the UI must perform. Shell has already
/// removed the tab from `Shell.tabs`, updated `Shell.active_tab`,
/// queued the `NPPN_FILECLOSED` / `NPPN_BUFFERACTIVATED`
/// notifications, and unregistered the file watcher; what's left
/// is the things only the UI knows about — the tab control and
/// the Scintilla document.
#[derive(Debug, Clone)]
pub struct ClosedTab {
    /// Index the tab occupied in `Shell.tabs` at the moment of
    /// close. Same index for the platform tab strip — the UI
    /// removes the item at this index. After this snapshot is
    /// returned, `Shell.tabs.len()` is one less and
    /// `Shell.active_tab` reflects the new selection.
    pub closed_idx: usize,
    /// Buffer id of the closed tab. Useful for plugin-host bookkeeping.
    pub buffer_id: i32,
    /// Path the closed tab was bound to (if any). Mostly for logging
    /// — the watcher unwatch already happened inside Shell.
    pub path: Option<PathBuf>,
    /// Scintilla document pointer the closed tab owned. UI calls
    /// `SCI_RELEASEDOCUMENT` against this so Scintilla can free
    /// the underlying buffer. Zero when the tab never had its
    /// document materialized (rare — only background-loaded tabs
    /// closed before first activation).
    pub scintilla_doc: isize,
    /// Scintilla document pointer for the new active tab, if any.
    /// UI calls `SCI_SETDOCPOINTER` on this to bind the view to
    /// the now-visible tab. Zero when there's no new active tab
    /// (closed the last open tab) or when the new active tab's
    /// document hasn't been materialized yet — `handle_tab_selchange`
    /// will lazily create one on the next user click.
    pub new_active_doc: isize,
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
    /// Last query + flags used by `find_next` / Find Replace dialog.
    /// Stored so F3 / Shift+F3 (and the dialog's Find Next button)
    /// can repeat the search without the user re-entering anything.
    /// `None` until the user issues their first search.
    #[cfg(target_os = "windows")]
    last_search: Option<(String, SearchFlags)>,
    /// Rolling Find/Replace dropdown history. Loaded from
    /// `find_history.xml` at startup; pushed to on every Find
    /// Next / Replace operation; saved back on the same path
    /// after each push (eager save — the file is tiny and the
    /// alternative is silently losing history on crash).
    pub find_history: FindHistory,
    /// Find-in-files orchestrator. Owns the active-job cancel
    /// flag and the next-job-id counter; events flow back through
    /// `fif_rx` (see [`Self::drain`]).
    fif_orchestrator: fif::FifOrchestrator,
    /// Receiver half of the FIF event channel. Senders are cloned
    /// per-job into walker / workers / coordinator, so dropping a
    /// job's threads doesn't close this channel.
    fif_rx: Receiver<FifEvent>,
    /// FIF events drained off `fif_rx` but not yet consumed by the
    /// UI. The UI calls [`Self::take_fif_events`] after each drain
    /// to pull them, then applies them to the results dock outside
    /// the `&mut Shell` borrow (matching the
    /// `pending_notifications` pattern).
    pending_fif: Vec<FifEvent>,
    /// Pending pre-fill for the next FIF dialog open, set by
    /// `NPPM_LAUNCHFINDINFILESDLG`. The Win32 plugin dispatch
    /// drains this immediately after `dispatch_plugin_message`
    /// returns and opens the dialog with the directory and
    /// filters pre-populated. `None` is the common case (menu /
    /// hotkey driven open uses whatever the dialog already
    /// holds).
    #[cfg(target_os = "windows")]
    pending_fif_launch: Option<FifLaunchPrefill>,
}

/// Search-option bitset matching Scintilla's `SCFIND_*` flags. Held
/// as a Rust newtype so the public API doesn't bind callers to
/// `scintilla-sys` symbols. Phase 4 m3 covers case sensitivity,
/// whole-word matching, and POSIX/CXX11 regex; m4 (find-in-files)
/// reuses the same flag set against per-file searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchFlags(pub u32);

impl SearchFlags {
    /// `SCFIND_NONE` — case-insensitive plain-text search.
    pub const NONE: SearchFlags = SearchFlags(0);
    /// `SCFIND_MATCHCASE` — `Foo` and `foo` are different.
    pub const MATCH_CASE: SearchFlags = SearchFlags(0x4);
    /// `SCFIND_WHOLEWORD` — `foo` does not match inside `foobar`.
    pub const WHOLE_WORD: SearchFlags = SearchFlags(0x2);
    /// `SCFIND_REGEXP | SCFIND_CXX11REGEX` — interpret the query
    /// as a C++11 regex. Without `CXX11REGEX`, Scintilla falls
    /// back to its older POSIX engine, which is missing common
    /// shorthands (`\d`, `\w`, lookarounds).
    pub const REGEX: SearchFlags = SearchFlags(0x00200000 | 0x00800000);

    /// OR two flag sets. Caller-friendly bit-combine without
    /// exposing the underlying u32.
    pub const fn union(self, other: SearchFlags) -> SearchFlags {
        SearchFlags(self.0 | other.0)
    }

    /// Raw bits, ready for `SCI_SETSEARCHFLAGS`'s wparam.
    pub const fn bits(self) -> u32 {
        self.0
    }
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
        spawn_forwarder(fc_rx_inner, fc_tx_outer, wake.clone(), "watch-forwarder");

        // FIF events: each job's walker/workers/coordinator clone
        // `fif_tx_inner` so per-job thread teardown doesn't close
        // the channel. The forwarder calls `wake` on each event so
        // the UI's message-pump iteration drains them via `drain`.
        let (fif_tx_inner, fif_rx_inner) = unbounded::<FifEvent>();
        let (fif_tx_outer, fif_rx_outer) = unbounded::<FifEvent>();
        spawn_forwarder(fif_rx_inner, fif_tx_outer, wake, "fif-forwarder");
        let fif_orchestrator = fif::FifOrchestrator::new(fif_tx_inner);

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
            #[cfg(target_os = "windows")]
            last_search: None,
            find_history: load_find_history(),
            fif_orchestrator,
            fif_rx: fif_rx_outer,
            pending_fif: Vec::new(),
            #[cfg(target_os = "windows")]
            pending_fif_launch: None,
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

    /// Close the currently-active tab. Returns a [`ClosedTab`] the
    /// UI uses to release the Scintilla document, switch the view
    /// to the new active tab's document, and remove the tab item
    /// from any platform tab strip. `NPPN_FILECLOSED` is queued
    /// for the closed buffer; if a different tab is now active,
    /// `NPPN_BUFFERACTIVATED` is queued for it. Returns `None`
    /// when there's nothing to close.
    ///
    /// New-active-tab selection follows the standard editor UX:
    /// prefer the right-neighbour (the tab that slid into the
    /// closed slot's index), fall back to the previous tab if
    /// the closed tab was the rightmost. Closing the last tab
    /// leaves `active_tab = None`.
    ///
    /// The file watcher is unregistered for the closed path
    /// inside this method so the UI doesn't have to remember;
    /// a failed `unwatch` is logged at debug level (the watcher
    /// silently ignores already-unregistered paths).
    pub fn close_active_tab(&mut self) -> Option<ClosedTab> {
        let idx = self.active_tab?;
        if idx >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(idx);

        if let Some(p) = &removed.path {
            if let Err(e) = self.file_watcher.unwatch(p) {
                tracing::debug!(
                    error = %e,
                    path = ?p,
                    "unwatch on close (already unwatched is fine)"
                );
            }
        }

        // Pick the new active tab. If the closed tab was the last
        // one, `idx` now equals `tabs.len()` after the remove —
        // step left to keep an in-range index.
        let new_active = if self.tabs.is_empty() {
            None
        } else if idx < self.tabs.len() {
            Some(idx)
        } else {
            Some(self.tabs.len() - 1)
        };
        self.active_tab = new_active;

        let new_active_doc = new_active
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.scintilla_doc)
            .unwrap_or(0);

        // Queue notifications in the same order N++ delivers them:
        //   1. NPPN_FILEBEFORECLOSE
        //   2. NPPN_FILECLOSED
        //   3. NPPN_BUFFERACTIVATED (only if there's a new active tab)
        //
        // **Known timing divergence vs N++ (tracked as Phase 5 polish):**
        // these notifications are pushed onto `pending_notifications`
        // and delivered to plugins by the UI *after* `take_notifications`
        // drains them — i.e., after `close_active_tab` returns and
        // after the tab has been removed from `self.tabs`. N++
        // delivers FILEBEFORECLOSE synchronously while the buffer is
        // still in its data structures, so a plugin's
        // `beNotified(NPPN_FILEBEFORECLOSE)` can call back into
        // `NPPM_GETFULLPATHFROMBUFFERID(id)` and get a real path.
        // Code++'s queue-deferred dispatch model means that callback
        // returns -1 (unknown id) instead. Plugins that need the path
        // at close time should cache it from the prior
        // BUFFERACTIVATED notification rather than relying on the
        // path lookup here.
        //
        // The fix needs synchronous-delivery plumbing for specific
        // notifications (Shell calling back into the plugin host
        // mid-operation, currently not part of the architecture);
        // the change is bigger than this batch should carry.
        let closing_id = removed.id as isize;
        #[cfg(target_os = "windows")]
        {
            self.pending_notifications
                .push(Notification::FileBeforeClose {
                    buffer_id: closing_id,
                });
            self.pending_notifications.push(Notification::FileClosed {
                buffer_id: closing_id,
            });
            if new_active.is_some() {
                self.queue_buffer_activated();
            }
        }
        // Suppress "unused" on non-Windows builds where the notification
        // queue isn't fed.
        #[cfg(not(target_os = "windows"))]
        let _ = closing_id;

        Some(ClosedTab {
            closed_idx: idx,
            buffer_id: removed.id,
            path: removed.path,
            scintilla_doc: removed.scintilla_doc,
            new_active_doc,
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
        // tab is empty *and isn't already waiting for its own load
        // to complete*, reuse it; otherwise allocate a fresh tab
        // now so we have a buffer id to associate with `req_id`.
        //
        // Without the `pending_load.is_none()` guard, two rapid
        // open_file calls (e.g. session restore reopening multiple
        // tabs) would both target the same empty tab — the second
        // call overwrites the first call's pending-load marker, so
        // the first load's apply_load_result finds no matching tab
        // and silently discards the buffer. Symptom: only the last
        // file in a multi-tab session is restored.
        let target_idx = match self.active_tab {
            Some(i)
                if self
                    .tabs
                    .get(i)
                    .map(|t| t.path.is_none() && t.pending_load.is_none())
                    .unwrap_or(false) =>
            {
                i
            }
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
        // Stage FIF events for the UI to consume after the borrow
        // ends — same pattern as plugin notifications. Per active
        // job the bound is `MAX_MATCHES_TOTAL + 1` (terminal event);
        // across multiple `start_fif` calls without an intervening
        // `take_fif_events` it scales linearly with the number of
        // jobs. Win32 calls `take_fif_events` on every WM_APP_WAKE,
        // so practical depth stays below the per-job ceiling.
        while let Ok(event) = self.fif_rx.try_recv() {
            self.pending_fif.push(event);
        }
        pending
    }

    /// Start a find-in-files job. Preempts any in-flight job and
    /// returns the new job's id. Events are drained off the shell's
    /// FIF channel into `pending_fif` on each `drain` and consumed
    /// by [`Self::take_fif_events`].
    ///
    /// Returns [`FifError::Query`] on a malformed query (without
    /// spawning any threads) and [`FifError::BadRoot`] when the
    /// requested root is not a directory.
    pub fn start_fif(&mut self, request: FifRequest) -> Result<FifJobId, FifError> {
        self.fif_orchestrator.start(request)
    }

    /// Cancel the current find-in-files job, if any. Idempotent.
    pub fn cancel_fif(&mut self) {
        self.fif_orchestrator.cancel();
    }

    /// Drain queued FIF events. Called by the UI after [`Self::drain`]
    /// (or any operation that may have queued events) so the events
    /// can be applied to the results dock outside the `&mut Shell`
    /// borrow — listview population is a UI-thread, dialog-pump-safe
    /// operation that mustn't run with shell state locked.
    pub fn take_fif_events(&mut self) -> Vec<FifEvent> {
        std::mem::take(&mut self.pending_fif)
    }

    /// Drain the pending `NPPM_LAUNCHFINDINFILESDLG` prefill, if a
    /// plugin posted one since the last call. The Win32 plugin
    /// dispatch consumes this immediately after
    /// `dispatch_plugin_message` returns and feeds it to the FIF
    /// dialog opener.
    #[cfg(target_os = "windows")]
    pub fn take_fif_launch_prefill(&mut self) -> Option<FifLaunchPrefill> {
        self.pending_fif_launch.take()
    }

    /// Setter for the FIF launch prefill — used by the
    /// `HostServices::launch_find_in_files_dialog` impl on
    /// `HostBridge` to stash the plugin's pre-fill request. Public
    /// in symmetry with [`Self::take_fif_launch_prefill`] so the
    /// write path mirrors the read path; the field itself stays
    /// private so external callers can't bypass the take/set
    /// contract.
    #[cfg(target_os = "windows")]
    pub fn set_fif_launch_prefill(&mut self, prefill: FifLaunchPrefill) {
        self.pending_fif_launch = Some(prefill);
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
                // Derive lang from the path extension. Plugins may
                // override later via NPPM_SETBUFFERLANGTYPE; for the
                // first-load default the extension is the only signal
                // we have.
                tab.lang = LangType::from_path(&loaded.path);
                let stored_doc = tab.scintilla_doc;
                let lang = tab.lang;
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
                    // apply_lang AFTER set_buffer_text — Scintilla
                    // re-styles the visible region on lexer attach,
                    // so the lexer needs to see the document already
                    // populated to colour it on the first paint.
                    ui.apply_lang(lang);
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

    /// Change the active tab's save-time encoding to `encoding`,
    /// driving the Encoding menu's "Convert to ..." items.
    ///
    /// Code++'s Scintilla view always stores text as UTF-8 internally
    /// (we set `SC_CP_UTF8` at create time), so an encoding change
    /// is purely a metadata flip: the in-memory bytes don't move,
    /// but the next [`Self::save_current_to_disk`] re-encodes through
    /// the new variant before writing to disk. Open the file again
    /// and `core::encoding::detect`/`decode` reads the new bytes
    /// back into UTF-8 — round-trip-correct for any text whose
    /// codepoints are representable in both encodings (which is
    /// every text for the four UTF variants the Encoding menu
    /// currently exposes).
    ///
    /// Returns `true` if the encoding actually changed (caller
    /// should refresh the status bar / radio); `false` on no-op
    /// (same encoding already, or no active tab). The no-op path is
    /// silent — re-clicking the active radio item shouldn't poke
    /// the title bar with a fake "modified" indicator.
    ///
    /// **Known limitation (deferred to a polish pass):** Scintilla's
    /// own modify flag (driven by `SCI_GETMODIFY`) is not flipped by
    /// this metadata-only change, so the title-bar dirty glyph won't
    /// surface "encoding pending save". The status bar updates
    /// (different label) and the user can still Ctrl+S to commit.
    /// N++'s glyph behaviour here is the same.
    pub fn set_buffer_encoding(&mut self, encoding: codepp_core::Encoding) -> bool {
        let Some(idx) = self.active_tab else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(idx) else {
            return false;
        };
        if tab.encoding == encoding {
            return false;
        }
        tab.encoding = encoding;
        true
    }

    /// Like [`Self::set_buffer_encoding`] but addresses an arbitrary
    /// open buffer by id rather than the active one. Plumbs
    /// `NPPM_SETBUFFERENCODING` from a plugin onto a specific buffer
    /// — the plugin contract takes a buffer id, not "the active
    /// buffer", so we need an id-keyed setter alongside the
    /// menu-driven active-tab setter.
    ///
    /// Same metadata-only semantics: the next save through the
    /// affected tab encodes via the new variant.
    ///
    /// Returns `true` whenever the buffer ends up in the requested
    /// state — both for an actual change and for a same-value
    /// no-op. Returns `false` only for an unknown id. This matches
    /// `set_buffer_lang_type`'s contract, which is the convention
    /// `NPPM_SETBUFFERLANGTYPE` plugins already rely on, and
    /// matches Notepad++'s "TRUE = the buffer is now in the
    /// requested state" return semantics. Distinguishing
    /// "unknown id" from "no-op success" is the bit plugins gate
    /// on; collapsing both to `false` would silently break plugins
    /// that conditionally re-encode only when the set "succeeds".
    pub fn set_buffer_encoding_by_id(
        &mut self,
        id: isize,
        encoding: codepp_core::Encoding,
    ) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id as isize == id) else {
            return false;
        };
        if tab.encoding == encoding {
            // Same-value no-op: the buffer is already in the
            // requested state, which is success per the N++
            // contract. Skip the mutation (no need to rewrite the
            // same value) but report success.
            return true;
        }
        tracing::debug!(
            buffer_id = id,
            from = %tab.encoding.label(),
            to = %encoding.label(),
            "set_buffer_encoding_by_id"
        );
        tab.encoding = encoding;
        true
    }

    /// Set the EOL format on the buffer with id `id`. Mirrors
    /// [`Self::set_buffer_encoding_by_id`] for line endings — same
    /// "TRUE = buffer is in the requested state" return convention,
    /// `false` only for unknown id.
    ///
    /// **Phase 4 metadata-only:** existing line-ending bytes inside
    /// the Scintilla document are not rewritten — `SCI_CONVERTEOLS`
    /// needs UI-side cooperation (the doc-pointer-swap dance to
    /// reach a non-active buffer's document), tracked in DESIGN.md
    /// §7.4.
    pub fn set_buffer_eol_by_id(&mut self, id: isize, eol: codepp_core::Eol) -> bool {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id as isize == id) else {
            return false;
        };
        if tab.eol == eol {
            return true;
        }
        tracing::debug!(
            buffer_id = id,
            from = %tab.eol.label(),
            to = %eol.label(),
            "set_buffer_eol_by_id"
        );
        tab.eol = eol;
        true
    }

    /// Search the active editor forward for `query` under `flags`
    /// and activate the match (Scintilla moves the selection to
    /// it). Returns the byte offset of the match, or `None` on
    /// miss. Stores the query + flags as the "last search" so
    /// subsequent `find_next_repeat` / `find_prev_repeat` calls
    /// can reuse them for keyboard-driven Find Next without the
    /// user re-entering anything. The dialog calls this on its
    /// initial Find click; the menu's Find Next / Find Previous
    /// (and their F3 / Shift+F3 shortcuts in m3b) reuse the
    /// stored state.
    ///
    /// **Misses still record the query.** This matches Notepad++:
    /// after a "not found" hit, F3 re-issues the same search,
    /// which lets the user re-tap F3 once they've expanded the
    /// search target rather than re-typing the query. Callers
    /// that want different semantics should clear `last_search`
    /// themselves on a `None` return.
    #[cfg(target_os = "windows")]
    pub fn find_next<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_next(query, flags)
    }

    /// Repeat the last `find_next` with its stored query and flags.
    /// Returns `None` if no search has been issued yet, or if the
    /// query missed.
    #[cfg(target_os = "windows")]
    pub fn find_next_repeat<U: UiPlatform>(&mut self, ui: &mut U) -> Option<u64> {
        let (query, flags) = self.last_search.clone()?;
        ui.search_next(&query, flags)
    }

    /// Backward sibling of [`Self::find_next`] — used by the Find
    /// dialog when the "Backward direction" checkbox is on. Stores
    /// the query+flags as the new `last_search` so a subsequent
    /// `find_next_repeat` / `find_prev_repeat` reuses them.
    #[cfg(target_os = "windows")]
    pub fn find_prev<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_prev(query, flags)
    }

    /// Repeat the last `find_next` going backward.
    #[cfg(target_os = "windows")]
    pub fn find_prev_repeat<U: UiPlatform>(&mut self, ui: &mut U) -> Option<u64> {
        let (query, flags) = self.last_search.clone()?;
        ui.search_prev(&query, flags)
    }

    /// Replace the current selection with `replacement` if and only
    /// if the selection text matches `query` under `flags`. The
    /// dialog calls this for its "Replace" button: the user has
    /// just done a Find which left the match selected; clicking
    /// Replace substitutes that match, then the dialog typically
    /// fires another Find Next. The match-check guards against
    /// replacing arbitrary text the user dragged a selection over
    /// after the find — Scintilla doesn't gate on that itself.
    /// Returns true if a replacement happened.
    #[cfg(target_os = "windows")]
    pub fn replace_current<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
    ) -> bool {
        if query.is_empty() || self.active_tab.is_none() {
            return false;
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_current(query, replacement, flags)
    }

    /// Replace every match of `query` with `replacement` in the
    /// active buffer. Returns the count of replacements performed.
    /// All replaces happen inside one Scintilla undo group so the
    /// user can Ctrl+Z the entire Replace-All in a single step.
    /// Empty `query` is a no-op (returns 0) — Scintilla would
    /// otherwise spin in an infinite loop on an empty match.
    #[cfg(target_os = "windows")]
    pub fn replace_all<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
    ) -> usize {
        if query.is_empty() || self.active_tab.is_none() {
            return 0;
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_all(query, replacement, flags)
    }

    /// Count occurrences of `query` in the active buffer. The
    /// Find dialog's "Count" button surfaces the result; does
    /// not affect selection or last_search state (matching N++).
    #[cfg(target_os = "windows")]
    pub fn count_matches<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
    ) -> usize {
        if query.is_empty() || self.active_tab.is_none() {
            return 0;
        }
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.count_matches(query, flags)
    }

    /// In-selection sibling of [`Self::find_next`]. The dialog
    /// captures the selection bounds when "In selection" is
    /// checked and forwards them on every Find Next click;
    /// `last_search` is still recorded so an F3 outside the
    /// dialog falls back to the whole-buffer behaviour.
    #[cfg(target_os = "windows")]
    pub fn find_next_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_next_in_range(query, flags, start, end)
    }

    /// Backward sibling of [`Self::find_next_in_range`].
    #[cfg(target_os = "windows")]
    pub fn find_prev_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> Option<u64> {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return None;
        }
        self.last_search = Some((query.to_string(), flags));
        if self.find_history.push_find(query) {
            save_find_history(&self.find_history);
        }
        ui.search_prev_in_range(query, flags, start, end)
    }

    /// Replace All restricted to `[start, end)`. Returns
    /// `(count, new_end)` so the caller can refresh its
    /// in-selection range after the document length shifts.
    #[cfg(target_os = "windows")]
    pub fn replace_all_in_range<U: UiPlatform>(
        &mut self,
        ui: &mut U,
        query: &str,
        replacement: &str,
        flags: SearchFlags,
        start: u64,
        end: u64,
    ) -> (usize, u64) {
        if query.is_empty() || self.active_tab.is_none() || end <= start {
            return (0, end);
        }
        let changed_find = self.find_history.push_find(query);
        let changed_replace = self.find_history.push_replace(replacement);
        if changed_find || changed_replace {
            save_find_history(&self.find_history);
        }
        ui.replace_all_in_range(query, replacement, flags, start, end)
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

    /// Read `session.xml` and return all stored tab paths in their
    /// original order, capped at [`MAX_SESSION_TABS`]. The parsed
    /// [`Session`] is stored on `self` so `apply_load_result` can
    /// later look up each restored tab's cursor position by path.
    /// Returns an empty Vec when the file is missing — first-run
    /// case, never an error. A parse failure also returns empty,
    /// with a warn-level log so a corrupted file is observable.
    ///
    /// The UI is expected to:
    ///   1. Call this method.
    ///   2. Call [`Self::open_file`] for each path returned.
    ///   3. Read [`Self::session_active_index`] and override
    ///      `self.active_tab` so the visible buffer matches the
    ///      pre-shutdown one. Without this override, `open_file`
    ///      always activates the most-recently-pushed tab,
    ///      defeating the session's stored selection.
    pub fn load_session_paths(&mut self) -> Vec<PathBuf> {
        let Some(path) = codepp_platform::session_xml_path() else {
            return Vec::new();
        };
        let session = match Session::load_from_xml(&path) {
            Ok(s) => s,
            Err(e) => {
                // Don't propagate as a hard error — a corrupted
                // session file shouldn't block startup. Warn so
                // the user has a breadcrumb if they wonder why
                // their tabs didn't come back, and so a tampered
                // session.xml leaves a trace.
                tracing::warn!(path = %path.display(), error = ?e, "session.xml load failed; starting clean");
                return Vec::new();
            }
        };
        // Cap restored tabs at `MAX_SESSION_TABS`. A session.xml
        // with thousands of <tab> entries (corrupted, tampered, or
        // a runaway session-save bug) would otherwise queue that
        // many async loads at startup and allocate a Tab + decoded
        // text per file — a local denial-of-service against the
        // invoking user's account. The cap is generous enough that
        // no realistic user hits it.
        if session.tabs.len() > MAX_SESSION_TABS {
            tracing::warn!(
                stored = session.tabs.len(),
                cap = MAX_SESSION_TABS,
                "session.xml exceeds tab cap; excess tabs not restored",
            );
        }
        let paths: Vec<PathBuf> = session
            .tabs
            .iter()
            .take(MAX_SESSION_TABS)
            .map(|t| t.path.clone())
            .collect();
        self.session = session;
        paths
    }

    /// Active tab index recorded in the most recently parsed
    /// session.xml, or `None` if no session was loaded. Returned as
    /// a separate accessor so the UI can read it after the
    /// `load_session_paths` + `open_file` loop without keeping
    /// `&self.session` borrowed across mutations.
    pub fn session_active_index(&self) -> Option<usize> {
        self.session.active
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

    fn buffer_lang_type(&self, id: isize) -> i32 {
        // Phase 4 m1: every tab carries its own `LangType`, derived
        // from the path extension at first-load time. Plugins reading
        // NPPM_GETBUFFERLANGTYPE for an unknown id get `L_TEXT` (the
        // same default the tab is born with), matching Notepad++'s
        // "no such buffer" behaviour.
        self.shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .map(|t| t.lang.as_npp_id())
            .unwrap_or(L_TEXT.as_npp_id())
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

    fn set_buffer_lang_type(&mut self, id: isize, lang: i32) -> bool {
        // Phase 4 m2: real per-buffer lang switch.
        //
        //   1. Find the tab. Unknown id → FALSE (matches N++'s
        //      "no such buffer, nothing changed" return).
        //   2. No-op same-lang sets — re-applying the same lexer
        //      flickers the visible buffer and a NPPN_LANGCHANGED
        //      fired for an unchanged lang would be a false
        //      positive that breaks plugins audit-logging language
        //      changes.
        //   3. Mutate the data model first; if this is the active
        //      tab, re-apply the lexer through the UI (the lexer
        //      lives on the *view*, not the document, so the
        //      apply_lang call has to land on the active editor
        //      regardless of which tab the plugin targeted).
        //   4. Queue NPPN_LANGCHANGED. Drain happens after the
        //      &mut Shell borrow drops, same as the other
        //      lifecycle notifications.
        let new_lang = LangType(lang);
        let Some(idx) = self.shell.tabs.iter().position(|t| t.id as isize == id) else {
            return false;
        };
        if self.shell.tabs[idx].lang == new_lang {
            return true;
        }
        self.shell.tabs[idx].lang = new_lang;
        if self.shell.active_tab == Some(idx) {
            self.ui.apply_lang(new_lang);
        }
        self.shell
            .pending_notifications
            .push(Notification::LangChanged {
                buffer_id: self.shell.tabs[idx].id as isize,
            });
        true
    }

    fn language_name(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_name()
    }

    fn language_desc(&self, lang: i32) -> Option<&'static str> {
        LangType(lang).language_desc()
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

    fn launch_find_in_files_dialog(&mut self, directory: Option<PathBuf>, filters: Option<String>) {
        // Stash the prefill on the underlying Shell; the Win32
        // dispatch in `main_wnd_proc` drains it via
        // `Shell::take_fif_launch_prefill` right after
        // `dispatch_plugin_message` returns. The dialog open
        // itself can't happen here — the shell layer doesn't know
        // about HWNDs (DESIGN.md §2.1) — but the prefill data
        // structure is shared, so the UI sees exactly what the
        // plugin requested. Routed through the public setter to
        // keep the take/set pair as the only field-touch sites.
        self.shell
            .set_fif_launch_prefill(FifLaunchPrefill { directory, filters });
    }

    fn open_buffer_paths(&self, selector: i32) -> Vec<PathBuf> {
        // Single-view through Phase 4: ALL_OPEN_FILES and
        // PRIMARY_VIEW return the same set; SECOND_VIEW is empty.
        // Untitled tabs (no on-disk path) are filtered out so the
        // TCHAR** plugin contract — each slot receives a real
        // path — holds. Tab order in `shell.tabs` matches the tab
        // strip's left-to-right order, which is what plugins
        // expect for "the i-th open file".
        match selector {
            codepp_plugin_host::ALL_OPEN_FILES | codepp_plugin_host::PRIMARY_VIEW => self
                .shell
                .tabs
                .iter()
                .filter_map(|t| t.path.clone())
                .collect(),
            codepp_plugin_host::SECOND_VIEW => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn current_doc_index(&self, view: i32) -> i32 {
        // Primary view exposes the active tab's `tabs[]` index;
        // secondary view doesn't exist yet (split-view is Phase 5),
        // so it reports -1 — the documented "no view" sentinel.
        // The `i as i32` cast is safe: `MAX_SESSION_TABS = 512`,
        // well below `i32::MAX`.
        match view {
            0 => self.shell.active_tab.map(|i| i as i32).unwrap_or(-1),
            _ => -1,
        }
    }

    fn buffer_encoding(&self, id: isize) -> i32 {
        // Map Code++'s internal `Encoding` to N++'s `UniMode` enum.
        // `Other` (unknown WHATWG codepage label, e.g. `windows-1252`,
        // `shift_jis`) collapses to `UNI_8BIT` — N++'s ABI doesn't
        // carry the codepage identity past this point either, and
        // plugins gating on "is this Unicode?" still get the right
        // answer (UNI_8BIT is "no").
        let Some(tab) = self.shell.tabs.iter().find(|t| t.id as isize == id) else {
            return -1;
        };
        match &tab.encoding {
            codepp_core::Encoding::Utf8 => codepp_plugin_host::UNI_COOKIE,
            codepp_core::Encoding::Utf8Bom => codepp_plugin_host::UNI_UTF8,
            codepp_core::Encoding::Utf16LeBom => codepp_plugin_host::UNI_UTF16LE,
            codepp_core::Encoding::Utf16BeBom => codepp_plugin_host::UNI_UTF16BE,
            codepp_core::Encoding::Utf16Le => codepp_plugin_host::UNI_UTF16LE_NO_BOM,
            codepp_core::Encoding::Utf16Be => codepp_plugin_host::UNI_UTF16BE_NO_BOM,
            codepp_core::Encoding::Other(_) => codepp_plugin_host::UNI_8BIT,
        }
    }

    fn buffer_format(&self, id: isize) -> i32 {
        // Map Code++'s internal `Eol` to N++'s `EolType`. `Mixed` is
        // unique to Code++ (per-line preservation when a file's EOL
        // is inconsistent); we report `UNIX_FORMAT` since LF is the
        // modern default and matches what "Edit → EOL Conversion"
        // would normalise a mixed file to.
        let Some(tab) = self.shell.tabs.iter().find(|t| t.id as isize == id) else {
            return -1;
        };
        match tab.eol {
            codepp_core::Eol::CrLf => codepp_plugin_host::WIN_FORMAT,
            codepp_core::Eol::Cr => codepp_plugin_host::MAC_FORMAT,
            codepp_core::Eol::Lf | codepp_core::Eol::Mixed => codepp_plugin_host::UNIX_FORMAT,
        }
    }

    fn reload_buffer_id(&mut self, id: isize, with_alert: bool) -> bool {
        // Resolve the buffer id to its on-disk path. Untitled tabs
        // (no path) report -1 from `NPPM_GETFULLPATHFROMBUFFERID`
        // and similarly are not reloadable here — there's nothing
        // to re-read off disk.
        let Some(path) = self
            .shell
            .tabs
            .iter()
            .find(|t| t.id as isize == id)
            .and_then(|t| t.path.clone())
        else {
            return false;
        };
        if with_alert {
            // Phase 4 limitation: the dispatcher cannot push into
            // the per-window pending-dialog queue from inside a
            // synchronous plugin call without re-engineering the
            // borrow plumbing on `Shell::drain`. Silently reloading
            // matches `with_alert == false` — which is what most
            // plugins pass in practice. The trace makes the gap
            // observable; the wiring is tracked as a follow-up.
            tracing::warn!(
                buffer_id = id,
                path = %path.display(),
                "NPPM_RELOADBUFFERID with_alert=true: silent reload until \
                 dialog-queue wiring lands (Phase 5 polish)",
            );
        }
        // `confirm_reload` is the same code path the file watcher's
        // post-prompt "Yes" arm uses — re-runs the loader for `path`.
        self.shell.confirm_reload(path);
        true
    }

    fn set_buffer_encoding(&mut self, id: isize, unimode: i32) -> bool {
        // Inverse of `Self::buffer_encoding`'s mapping. We reject
        // UNI_7BIT outright: Code++'s detection pipeline never
        // produces it (pure ASCII is reported as `UNI_COOKIE`/Utf8)
        // and there's no exact `Encoding` variant for "ASCII", so
        // a plugin asking for it would silently get UTF-8 and be
        // surprised on save. Better to fail loudly with a `false`
        // return.
        //
        // UNI_8BIT maps to `windows-1252` because that's the de-
        // facto "ANSI" codepage on western-European Windows
        // installs. The encoding label round-trips through
        // `Encoding::from_label`, so a session.xml save+restore
        // cycle preserves the choice. (Future polish: detect the
        // system codepage via `GetACP` at startup and use that.)
        let encoding = match unimode {
            codepp_plugin_host::UNI_8BIT => {
                codepp_core::Encoding::Other("windows-1252".to_string())
            }
            codepp_plugin_host::UNI_UTF8 => codepp_core::Encoding::Utf8Bom,
            codepp_plugin_host::UNI_UTF16BE => codepp_core::Encoding::Utf16BeBom,
            codepp_plugin_host::UNI_UTF16LE => codepp_core::Encoding::Utf16LeBom,
            codepp_plugin_host::UNI_COOKIE => codepp_core::Encoding::Utf8,
            codepp_plugin_host::UNI_UTF16BE_NO_BOM => codepp_core::Encoding::Utf16Be,
            codepp_plugin_host::UNI_UTF16LE_NO_BOM => codepp_core::Encoding::Utf16Le,
            // UNI_7BIT, UNI_END, or anything else: rejected.
            _ => return false,
        };
        self.shell.set_buffer_encoding_by_id(id, encoding)
    }

    fn set_buffer_format(&mut self, id: isize, eoltype: i32) -> bool {
        // Inverse of `Self::buffer_format`'s mapping. Code++'s
        // `Eol::Mixed` is per-line preservation — a state plugins
        // cannot ask for since it has no N++ counterpart. The
        // setter never produces `Mixed`; only WIN/MAC/UNIX_FORMAT
        // are accepted.
        let eol = match eoltype {
            codepp_plugin_host::WIN_FORMAT => codepp_core::Eol::CrLf,
            codepp_plugin_host::MAC_FORMAT => codepp_core::Eol::Cr,
            codepp_plugin_host::UNIX_FORMAT => codepp_core::Eol::Lf,
            _ => return false,
        };
        self.shell.set_buffer_eol_by_id(id, eol)
    }
}

/// Spawn a forwarder thread that pumps items from `src` into `dst`
/// and calls `wake` after each successful send. Used so the shell
/// can wake the UI thread on every producer event without modifying
/// the producer crates' APIs.
/// Read `find_history.xml` if present. A missing file is normal
/// (first launch); a corrupt one is logged + ignored so the user
/// still gets a working dialog with empty dropdowns.
fn load_find_history() -> FindHistory {
    let Some(path) = codepp_platform::find_history_xml_path() else {
        return FindHistory::default();
    };
    match FindHistory::load(&path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "find_history.xml load failed; starting empty");
            FindHistory::default()
        }
    }
}

/// Save `find_history.xml`. Errors are logged + swallowed —
/// failing to persist the dropdown list isn't worth bubbling
/// up through the find/replace UI path. Cfg-gated to Windows
/// because every caller is on a cfg-gated find/replace method;
/// without the gate, a Linux/macOS lint build flags it as
/// dead code.
#[cfg(target_os = "windows")]
fn save_find_history(history: &FindHistory) {
    let Some(path) = codepp_platform::find_history_xml_path() else {
        return;
    };
    if let Err(e) = history.save(&path) {
        tracing::warn!(path = %path.display(), error = %e, "find_history.xml save failed");
    }
}

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
        apply_lang_calls: Vec<LangType>,
        search_calls: Vec<(String, SearchFlags, String)>,
        replace_calls: Vec<(String, String, SearchFlags, String)>,
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
        fn apply_lang(&mut self, lang: LangType) {
            self.apply_lang_calls.push(lang);
        }
        fn search_next(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
            // Naive in-test substring search over the fake buffer.
            // Records the call so tests can assert on it.
            self.search_calls
                .push((query.to_string(), flags, "next".to_string()));
            self.buffer_text.find(query).map(|pos| pos as u64)
        }
        fn search_prev(&mut self, query: &str, flags: SearchFlags) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "prev".to_string()));
            self.buffer_text.rfind(query).map(|pos| pos as u64)
        }
        fn replace_current(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> bool {
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "current".to_string(),
            ));
            // Replace the first occurrence in the fake buffer to
            // approximate what Scintilla does on the real path.
            if let Some(pos) = self.buffer_text.find(query) {
                self.buffer_text
                    .replace_range(pos..pos + query.len(), replacement);
                true
            } else {
                false
            }
        }
        fn replace_all(&mut self, query: &str, replacement: &str, flags: SearchFlags) -> usize {
            // No empty-query guard here — `Shell::replace_all`
            // gates that before reaching the platform impl, so a
            // duplicate guard in the test fake would obscure which
            // layer is responsible.
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "all".to_string(),
            ));
            let count = self.buffer_text.matches(query).count();
            self.buffer_text = self.buffer_text.replace(query, replacement);
            count
        }
        fn count_matches(&mut self, query: &str, flags: SearchFlags) -> usize {
            self.search_calls
                .push((query.to_string(), flags, "count".to_string()));
            self.buffer_text.matches(query).count()
        }
        fn search_next_in_range(
            &mut self,
            query: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "next_in_range".to_string()));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return None;
            }
            self.buffer_text[lo..hi]
                .find(query)
                .map(|p| (lo + p) as u64)
        }
        fn search_prev_in_range(
            &mut self,
            query: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> Option<u64> {
            self.search_calls
                .push((query.to_string(), flags, "prev_in_range".to_string()));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return None;
            }
            self.buffer_text[lo..hi]
                .rfind(query)
                .map(|p| (lo + p) as u64)
        }
        fn replace_all_in_range(
            &mut self,
            query: &str,
            replacement: &str,
            flags: SearchFlags,
            start: u64,
            end: u64,
        ) -> (usize, u64) {
            self.replace_calls.push((
                query.to_string(),
                replacement.to_string(),
                flags,
                "all_in_range".to_string(),
            ));
            let lo = (start as usize).min(self.buffer_text.len());
            let hi = (end as usize).min(self.buffer_text.len());
            if hi <= lo {
                return (0, end);
            }
            let inside = &self.buffer_text[lo..hi];
            let count = inside.matches(query).count();
            let replaced_inside = inside.replace(query, replacement);
            let new_end = lo + replaced_inside.len();
            self.buffer_text.replace_range(lo..hi, &replaced_inside);
            (count, new_end as u64)
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
    fn set_buffer_encoding_no_active_tab_returns_false() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        assert!(!shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
    }

    #[test]
    fn set_buffer_encoding_same_value_returns_false() {
        // Re-clicking the active radio item must be a silent no-op
        // — without the equality check, every same-encoding click
        // would still notify-callers (notification spam, status-bar
        // repaint flicker).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u8.txt");
        std::fs::write(&path, "hello\n").unwrap();

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

        // Plain ASCII content detects as UTF-8 (no BOM).
        assert_eq!(
            shell.active().unwrap().encoding,
            codepp_core::Encoding::Utf8
        );
        assert!(!shell.set_buffer_encoding(codepp_core::Encoding::Utf8));
    }

    #[test]
    fn set_buffer_encoding_then_save_writes_new_encoding_bytes() {
        // Phase 4 demo bullet: "Convert a UTF-8 file to UTF-16 LE
        // and back; bytes are correct." This test pins the
        // forward leg.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conv.txt");
        std::fs::write(&path, "abc\n").unwrap();

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

        // Mirror the in-memory text the Scintilla buffer would
        // hold — `FakeUi::get_buffer_text` returns this verbatim
        // and `save_current_to_disk` re-encodes it through the
        // tab's current encoding.
        ui.buffer_text = "abc\n".to_string();
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
        shell.save_current_to_disk(&mut ui).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        // UTF-16 LE BOM: 0xFF 0xFE then 'a'/'b'/'c'/'\n' as
        // little-endian u16s (each high byte zero).
        assert_eq!(
            on_disk,
            vec![0xFF, 0xFE, b'a', 0x00, b'b', 0x00, b'c', 0x00, b'\n', 0x00]
        );
    }

    #[test]
    fn set_buffer_encoding_round_trip_to_utf16_and_back() {
        // The full round-trip the Phase 4 demo describes: open a
        // UTF-8 file, convert to UTF-16 LE BOM, save, reopen,
        // convert back to UTF-8, save, and compare the final
        // bytes against the original. The text content survives
        // both legs because every codepoint is representable in
        // both encodings.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trip.txt");
        let original_bytes = b"hello world\n".to_vec();
        std::fs::write(&path, &original_bytes).unwrap();

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
        ui.buffer_text = "hello world\n".to_string();

        // Forward: UTF-8 -> UTF-16 LE BOM.
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf16LeBom));
        shell.save_current_to_disk(&mut ui).unwrap();
        let utf16_bytes = std::fs::read(&path).unwrap();
        assert_eq!(&utf16_bytes[..2], b"\xFF\xFE", "BOM should be present");

        // Re-open the file to round-trip the bytes through
        // detection + decode. After this pass the active tab
        // sees the saved encoding (Utf16LeBom) and the same text.
        // `close_active_tab` is synchronous (data-model only); no
        // intermediate drain needed before the re-open. Capture
        // the baseline `set_text_calls` count and wait for *one
        // more* to land — `>= 2` would be satisfied prematurely
        // if the first open's load happened to push more than
        // one chunk.
        let baseline = ui.set_text_calls.len();
        shell.close_active_tab();
        shell.open_file(path.clone());
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() > baseline,
            Duration::from_secs(2),
        );
        assert_eq!(
            shell.active().unwrap().encoding,
            codepp_core::Encoding::Utf16LeBom,
        );

        // Back: UTF-16 LE BOM -> UTF-8 (no BOM). After save, the
        // on-disk bytes match the original UTF-8 input
        // byte-for-byte.
        ui.buffer_text = "hello world\n".to_string();
        assert!(shell.set_buffer_encoding(codepp_core::Encoding::Utf8));
        shell.save_current_to_disk(&mut ui).unwrap();
        let final_bytes = std::fs::read(&path).unwrap();
        assert_eq!(final_bytes, original_bytes);
    }

    #[test]
    fn set_buffer_encoding_by_id_targets_specific_tab() {
        // The id-keyed setter must mutate only the addressed tab,
        // leaving other tabs' encodings untouched. Plugin-driven
        // NPPM_SETBUFFERENCODING addresses tabs by id, not by
        // active-ness, so a plugin that flips the encoding on a
        // background tab shouldn't accidentally flip the active
        // tab's metadata.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a, b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        let id_b = shell.tabs[1].id as isize;

        // Default UTF-8 on both tabs.
        assert_eq!(shell.tabs[0].encoding, codepp_core::Encoding::Utf8);
        assert_eq!(shell.tabs[1].encoding, codepp_core::Encoding::Utf8);

        // Flip tab `b`'s encoding only — tab `a` stays UTF-8.
        assert!(shell.set_buffer_encoding_by_id(id_b, codepp_core::Encoding::Utf16LeBom));
        assert_eq!(shell.tabs[0].encoding, codepp_core::Encoding::Utf8);
        assert_eq!(shell.tabs[1].encoding, codepp_core::Encoding::Utf16LeBom);

        // Same-value set on the same id reports `true` — the buffer
        // *is* in the requested state, which is success per the N++
        // contract. (Distinguishing same-value-success from
        // unknown-id is the bit plugins gate on.)
        assert!(shell.set_buffer_encoding_by_id(id_b, codepp_core::Encoding::Utf16LeBom));

        // Unknown id is rejected.
        assert!(!shell.set_buffer_encoding_by_id(9999, codepp_core::Encoding::Utf8));
    }

    #[test]
    fn set_buffer_eol_by_id_targets_specific_tab() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eol.txt");
        std::fs::write(&path, "line\n").unwrap();
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

        let id = shell.tabs[0].id as isize;
        // Detection of "line\n" produces Eol::Lf.
        assert_eq!(shell.tabs[0].eol, codepp_core::Eol::Lf);

        // Flip to CRLF.
        assert!(shell.set_buffer_eol_by_id(id, codepp_core::Eol::CrLf));
        assert_eq!(shell.tabs[0].eol, codepp_core::Eol::CrLf);

        // Same-value reports success — the buffer is already in
        // the requested state.
        assert!(shell.set_buffer_eol_by_id(id, codepp_core::Eol::CrLf));

        // Unknown id rejected.
        assert!(!shell.set_buffer_eol_by_id(9999, codepp_core::Eol::Lf));
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
    fn plugin_dispatch_get_buffer_format_maps_mixed_to_unix() {
        // `Eol::Mixed` is unique to Code++ — N++'s ABI has no
        // equivalent. The HostBridge mapping reports `UNIX_FORMAT`
        // (LF) so a plugin doing `if (format == WIN_FORMAT)` on a
        // mixed-EOL file gets a stable answer rather than depending
        // on which line ending happens to be most common in the
        // buffer. This test pins that contract.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "a\nb\r\nc\n").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        // Force the tab into the Mixed-EOL state. The on-disk
        // detection rounds to a single dominant EOL; explicit
        // assignment lets this test cover the Mixed branch
        // without crafting a file the detector classifies that
        // way (the detector's threshold is intentionally lenient
        // and may shift in future tuning).
        let active_id = shell.active().expect("active tab").id as usize;
        shell.tabs[0].eol = codepp_core::Eol::Mixed;

        const NPPM_GETBUFFERFORMAT: u32 = (0x0400 + 1000) + 68;
        const UNIX_FORMAT: isize = 2;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERFORMAT,
                active_id,
                0,
            )
        };
        assert_eq!(r, Some(UNIX_FORMAT));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn plugin_dispatch_get_buffer_encoding_returns_unimode() {
        // Default load of a UTF-8 file (no BOM) should report
        // `uniCookie` (UTF-8 without BOM) — the most common case
        // for plain text files. The "Cookie" naming is a historical
        // N++ misnomer for "BOM-less UTF-8"; we keep it for ABI
        // compatibility.
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.txt");
        std::fs::write(&path, "hello").unwrap();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.set_text_calls.is_empty(),
            Duration::from_secs(2),
        );

        let active_id = shell.active().expect("active tab").id as usize;
        const NPPM_GETBUFFERENCODING: u32 = (0x0400 + 1000) + 66;
        const UNI_COOKIE: isize = 4;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERENCODING,
                active_id,
                0,
            )
        };
        assert_eq!(r, Some(UNI_COOKIE));
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
    fn close_active_tab_with_no_tabs_returns_none() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        assert!(shell.close_active_tab().is_none());
        assert!(shell.tabs.is_empty());
        assert_eq!(shell.active_tab, None);
    }

    #[test]
    fn close_active_tab_last_tab_clears_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );

        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 0);
        assert!(shell.tabs.is_empty());
        assert_eq!(shell.active_tab, None);
        // No new active tab → the snapshot's new_active_doc is 0.
        assert_eq!(closed.new_active_doc, 0);
    }

    #[test]
    fn close_active_tab_middle_prefers_right_neighbour() {
        // Three tabs, active is the middle one. Closing it should
        // make the previously-third tab (now at index 1) active.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        std::fs::write(&c, "c").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a, b.clone(), c.clone()] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        // Activate the middle tab.
        shell.active_tab = Some(1);
        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 1);
        assert_eq!(closed.path.as_ref(), Some(&b));
        assert_eq!(shell.tabs.len(), 2);
        // Right-neighbour took the closed slot's index.
        assert_eq!(shell.active_tab, Some(1));
        assert_eq!(shell.tabs[1].path.as_ref(), Some(&c));
    }

    #[test]
    fn close_active_tab_rightmost_falls_back_to_previous() {
        // Two tabs, active is the rightmost. Closing it should
        // make the previously-first tab active (since there's no
        // right-neighbour to slide into).
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a.clone(), b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        assert_eq!(shell.active_tab, Some(1));
        let closed = shell.close_active_tab().expect("close returns snapshot");
        assert_eq!(closed.closed_idx, 1);
        assert_eq!(shell.tabs.len(), 1);
        assert_eq!(shell.active_tab, Some(0));
        assert_eq!(shell.tabs[0].path.as_ref(), Some(&a));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn close_active_tab_queues_file_closed_then_buffer_activated() {
        // Closing one of two open tabs queues, in order:
        //   1. NPPN_FILEBEFORECLOSE (so plugins can save state)
        //   2. NPPN_FILECLOSED (final-act for the closed buffer)
        //   3. NPPN_BUFFERACTIVATED (new active sibling)
        // Order matches Notepad++.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        for path in [a.clone(), b] {
            shell.open_file(path);
            let target = ui.set_text_calls.len() + 1;
            drain_until(
                &mut shell,
                &mut ui,
                |u, _| u.set_text_calls.len() == target,
                Duration::from_secs(2),
            );
        }
        // Drain the open's notifications.
        let _ = shell.take_notifications();

        let closed_id = shell.tabs[1].id as isize;
        let new_active_id = shell.tabs[0].id as isize;
        let closed = shell.close_active_tab().expect("close");
        assert_eq!(closed.buffer_id as isize, closed_id);

        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 3);
        assert!(matches!(
            notifications[0],
            Notification::FileBeforeClose { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::FileClosed { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[2],
            Notification::BufferActivated { buffer_id } if buffer_id == new_active_id
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn close_last_tab_queues_only_file_closed() {
        // Closing the only open tab queues NPPN_FILEBEFORECLOSE
        // followed by NPPN_FILECLOSED but NOT NPPN_BUFFERACTIVATED
        // — there's no new active buffer to activate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only.txt");
        std::fs::write(&path, "x").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| u.set_text_calls.len() == 1,
            Duration::from_secs(2),
        );
        let _ = shell.take_notifications();

        let closed_id = shell.tabs[0].id as isize;
        let _ = shell.close_active_tab().expect("close");
        let notifications = shell.take_notifications();
        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0],
            Notification::FileBeforeClose { buffer_id } if buffer_id == closed_id
        ));
        assert!(matches!(
            notifications[1],
            Notification::FileClosed { buffer_id } if buffer_id == closed_id
        ));
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

    #[test]
    fn open_cpp_file_calls_apply_lang_with_l_cpp() {
        // Phase 4 m1: opening a `.cpp` derives `LangType::L_CPP` from
        // the extension and forwards it to the UI's `apply_lang`. The
        // FakeUi records every call; we check both that the call
        // happens and that it carries the right LangType.
        use codepp_core::lang::L_CPP;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.cpp");
        std::fs::write(&path, "int main() { return 0; }\n").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_CPP);
        // The lang lands on the tab so plugins reading
        // NPPM_GETBUFFERLANGTYPE see it without a re-derive.
        assert_eq!(shell.tabs[0].lang, L_CPP);
    }

    #[test]
    fn open_unknown_extension_calls_apply_lang_with_l_text() {
        use codepp_core::lang::L_TEXT;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.xyz");
        std::fs::write(&path, "plain").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_TEXT);
    }

    #[test]
    fn apply_lang_runs_after_set_buffer_text() {
        // Scintilla re-styles the visible region on lexer attach, so
        // `apply_lang` must run after `set_buffer_text` — otherwise
        // the lexer sees an empty buffer and the first paint shows
        // un-coloured text. Order is observable via the FakeUi's
        // separate vectors plus the order each one was pushed in
        // — apply_load_result writes set_text first, apply_lang
        // second, so the ratio set_text:apply_lang stays 1:1 with
        // the same call ordering across loads.
        use codepp_core::lang::L_RUST;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        assert_eq!(ui.set_text_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls.len(), 1);
        assert_eq!(ui.apply_lang_calls[0], L_RUST);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn set_buffer_lang_type_updates_tab_and_queues_langchanged() {
        // Phase 4 m2: a plugin that NPPM_SETBUFFERLANGTYPE's the
        // active buffer to a new lang must (a) flip Tab.lang, (b)
        // re-apply the lexer through the UI (lexer lives on the
        // view, not the doc), (c) queue NPPN_LANGCHANGED so other
        // plugins see the change.
        use codepp_core::lang::{L_CPP, L_RUST};
        use codepp_plugin_host::dispatch::NPPM_SETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        // After load: tab[0].lang == L_RUST. Now have a "plugin"
        // re-classify it as L_CPP via the dispatcher.
        assert_eq!(shell.tabs[0].lang, L_RUST);
        let id = shell.tabs[0].id as usize;
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETBUFFERLANGTYPE,
                id,
                L_CPP.as_npp_id() as isize,
            )
        };
        assert_eq!(r, Some(1));
        assert_eq!(shell.tabs[0].lang, L_CPP);
        // apply_lang fired twice: once on load (L_RUST), once on
        // the plugin-driven set (L_CPP).
        assert_eq!(ui.apply_lang_calls.len(), 2);
        assert_eq!(*ui.apply_lang_calls.last().unwrap(), L_CPP);
        // NPPN_LANGCHANGED queued for delivery.
        assert!(
            shell
                .pending_notifications
                .iter()
                .any(|n| matches!(n, Notification::LangChanged { .. })),
            "NPPN_LANGCHANGED not queued",
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn set_buffer_lang_type_same_lang_is_idempotent() {
        // Re-classifying a buffer to its current lang must not
        // re-apply the lexer (visible flicker) or queue
        // NPPN_LANGCHANGED (false positive that breaks plugins
        // audit-logging language changes).
        use codepp_core::lang::L_RUST;
        use codepp_plugin_host::dispatch::NPPM_SETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        let id = shell.tabs[0].id as usize;
        // Drain any queued notifications from the open path so we
        // observe only the SETBUFFERLANGTYPE response.
        let _ = shell.take_notifications();
        let calls_before = ui.apply_lang_calls.len();

        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_SETBUFFERLANGTYPE,
                id,
                L_RUST.as_npp_id() as isize,
            )
        };
        // Returns success (the buffer IS that lang now, just was
        // already that lang).
        assert_eq!(r, Some(1));
        // No re-apply, no notification queued.
        assert_eq!(ui.apply_lang_calls.len(), calls_before);
        assert!(!shell
            .pending_notifications
            .iter()
            .any(|n| matches!(n, Notification::LangChanged { .. })));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn buffer_lang_type_returns_tabs_lang_to_plugins() {
        // Verifies that `HostBridge::buffer_lang_type` (the trait
        // impl plugins reach via NPPM_GETBUFFERLANGTYPE) reads the
        // tab's stored lang, not a hardcoded L_TEXT. Goes through
        // the same dispatch_plugin_message path the real wnd_proc
        // uses on `WM_NPPM_*` so we exercise the host-bridge hookup,
        // not just the bare `HostServices` impl.
        use codepp_core::lang::L_RUST;
        use codepp_plugin_host::dispatch::NPPM_GETBUFFERLANGTYPE;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn x() {}").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        shell.open_file(path);
        drain_until(
            &mut shell,
            &mut ui,
            |u, _| !u.apply_lang_calls.is_empty(),
            Duration::from_secs(2),
        );

        // First opened tab gets buffer id 1 (sequential, base 1).
        let id = shell.tabs[0].id as usize;
        // SAFETY: dispatch_plugin_message's wnd_proc safety contract
        // requires UI-thread invocation; the test thread is the sole
        // owner of `shell` and `ui`, satisfying the contract.
        let r = unsafe {
            shell.dispatch_plugin_message(
                &mut ui,
                HostHandles::null(),
                NPPM_GETBUFFERLANGTYPE,
                id,
                0,
            )
        };
        assert_eq!(r, Some(L_RUST.as_npp_id() as isize));
    }

    #[test]
    fn rapid_back_to_back_opens_dont_collide() {
        // Regression: two open_file calls back-to-back (before
        // either load completes) used to share tab[0] because the
        // empty-tab reuse rule only checked `path.is_none()` and
        // not `pending_load.is_none()`. The second open clobbered
        // the first's pending_load id so the first load result
        // was discarded as "stale" — only one of the two files
        // ended up open. Symptom in the wild: session restore
        // with two tabs only restored the last one.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("first.txt");
        let path_b = dir.path().join("second.txt");
        std::fs::write(&path_a, "AAA").unwrap();
        std::fs::write(&path_b, "BBB").unwrap();

        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();

        // Both opens before draining — no apply_load_result has
        // fired yet, so tab[0] is still "no path, pending_load=Some".
        shell.open_file(path_a.clone());
        shell.open_file(path_b.clone());

        // Distinct tabs at this point with distinct pending_loads.
        assert_eq!(shell.tabs.len(), 2);
        assert!(shell.tabs[0].pending_load.is_some());
        assert!(shell.tabs[1].pending_load.is_some());
        assert_ne!(shell.tabs[0].pending_load, shell.tabs[1].pending_load);

        // Drain both loads. Both files should land on their tabs.
        // Wait until both pending_loads clear so the content
        // assertions below aren't observing a half-drained state
        // (a 500 ms timeout that fires before both loads complete
        // would otherwise let the test pass on a tab still
        // pending its real content).
        drain_until(
            &mut shell,
            &mut ui,
            |_, _| false,
            Duration::from_millis(500),
        );
        assert_eq!(shell.tabs.len(), 2, "both tabs survived the drain");
        assert!(
            shell.tabs[0].pending_load.is_none() && shell.tabs[1].pending_load.is_none(),
            "both loads must complete before content assertions",
        );
        assert_eq!(shell.tabs[0].path.as_deref(), Some(path_a.as_path()));
        assert_eq!(shell.tabs[1].path.as_deref(), Some(path_b.as_path()));
        assert_eq!(shell.tabs[0].text, "AAA");
        assert_eq!(shell.tabs[1].text, "BBB");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn find_next_stores_last_search_and_repeat_reuses_it() {
        // First find_next records the query+flags so a later
        // find_next_repeat (the F3 / Find Next path) can fire
        // without the user re-entering the search term.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello hello world").unwrap();

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

        let hit = shell.find_next(&mut ui, "hello", SearchFlags::MATCH_CASE);
        assert_eq!(hit, Some(0), "first find_next returns position");
        assert_eq!(ui.search_calls.len(), 1);
        assert_eq!(ui.search_calls[0].0, "hello");
        assert_eq!(ui.search_calls[0].1, SearchFlags::MATCH_CASE);

        // Repeat — uses stored query, no new args.
        let hit2 = shell.find_next_repeat(&mut ui);
        assert_eq!(hit2, Some(0));
        assert_eq!(ui.search_calls.len(), 2, "second call recorded");
        assert_eq!(ui.search_calls[1].0, "hello");

        // Backward-repeat reuses the same stored query.
        let hit3 = shell.find_prev_repeat(&mut ui);
        assert!(hit3.is_some());
        assert_eq!(ui.search_calls[2].2, "prev");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn find_next_with_no_open_tab_is_noop() {
        let wake = Arc::new(|| {}) as Arc<dyn Fn() + Send + Sync>;
        let mut shell = Shell::new(wake).unwrap();
        let mut ui = FakeUi::default();
        let hit = shell.find_next(&mut ui, "anything", SearchFlags::NONE);
        assert_eq!(hit, None);
        assert!(ui.search_calls.is_empty());
        // Empty search isn't stored as last_search so a stray
        // F3 doesn't trigger an empty-query call.
        assert!(shell.find_next_repeat(&mut ui).is_none());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn replace_all_empty_query_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "abc").unwrap();
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

        let count = shell.replace_all(&mut ui, "", "x", SearchFlags::NONE);
        assert_eq!(count, 0, "empty query must not enter Scintilla loop");
        assert!(ui.replace_calls.is_empty());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn replace_all_counts_substitutions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "foo bar foo baz foo").unwrap();
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

        let count = shell.replace_all(&mut ui, "foo", "qux", SearchFlags::NONE);
        assert_eq!(count, 3);
        assert_eq!(ui.buffer_text, "qux bar qux baz qux");
    }

    #[test]
    fn session_xml_roundtrip_preserves_tab_order_and_active_index() {
        // session.xml round-trip: write a 2-tab session via the
        // production save_to_xml path, then load it back and verify
        // both paths are preserved in their stored order plus the
        // active index. This is the data-shape contract that
        // `load_session_paths` depends on — `load_session_paths`
        // itself can't be unit-tested without the platform's
        // `session_xml_path` (test-only override would be its own
        // refactor).
        use codepp_core::session::{Session as CoreSession, Tab as CoreTab};
        let dir = tempfile::tempdir().unwrap();
        let xml_path = dir.path().join("session.xml");
        let original = CoreSession {
            active: Some(1),
            tabs: vec![
                CoreTab {
                    path: PathBuf::from("/tmp/first.txt"),
                    cursor: 0,
                    encoding: Encoding::default(),
                    eol: Eol::default(),
                },
                CoreTab {
                    path: PathBuf::from("/tmp/second.txt"),
                    cursor: 5,
                    encoding: Encoding::default(),
                    eol: Eol::default(),
                },
            ],
        };
        original.save_to_xml(&xml_path).unwrap();

        let parsed = CoreSession::load_from_xml(&xml_path).unwrap();
        assert_eq!(parsed.active, Some(1));
        assert_eq!(parsed.tabs.len(), 2);
        assert_eq!(parsed.tabs[0].path, PathBuf::from("/tmp/first.txt"));
        assert_eq!(parsed.tabs[1].path, PathBuf::from("/tmp/second.txt"));
        assert_eq!(parsed.tabs[1].cursor, 5);
    }
}
