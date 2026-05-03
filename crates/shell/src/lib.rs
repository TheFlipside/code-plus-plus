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

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use codepp_core::file::{Loader, LoaderShutdown};
use codepp_core::{Encoding, Eol, LoadResult, RequestId, Session};
use codepp_platform::watch::{FileChange, FileWatcher};

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

/// Application-wide state. Owned by the UI crate's `run()` function;
/// the wnd_proc / event handler reaches into it on every interesting
/// message.
pub struct Shell {
    pub session: Session,
    pub buffer: ActiveBuffer,
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
            loader,
            _loader_shutdown: loader_shutdown,
            file_watcher,
            load_rx: load_rx_outer,
            change_rx: fc_rx_outer,
        })
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
        // Path comparison is by exact equality — symlinks and short
        // names on Windows can refer to the same file under different
        // strings and would silently miss the prompt. Phase 3+ may
        // canonicalize before comparing if multi-tab demands it.
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
        self.buffer.byte_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
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
}
