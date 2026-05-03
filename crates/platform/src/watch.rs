//! File-change watcher for detecting external modifications.
//!
//! Wraps `notify::RecommendedWatcher` and translates its events into
//! a small Code++-internal [`FileChange`] enum delivered over a
//! `crossbeam_channel::Sender`. The shell registers a watch when a
//! file is opened in a tab and unregisters when the tab closes; on
//! receiving a `FileChange::Modified` the UI shows the "external
//! change — reload?" prompt described in DESIGN.md §5.3.
//!
//! Cross-platform: Windows uses ReadDirectoryChangesW, Linux uses
//! inotify, macOS uses FSEvents — all transparently via `notify`.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// External change detected by the watcher. Phase 2 only exercises
/// `Modified`; `Removed` is delivered too because users will
/// reasonably expect Code++ to flag a tab whose underlying file was
/// deleted, and inotify/FSEvents both surface that cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    /// File contents changed externally — typical case after the
    /// user saves the file in another editor, or after a build step
    /// regenerates it.
    Modified(PathBuf),
    /// File was removed or renamed away from the watched path.
    Removed(PathBuf),
}

/// A file-change watcher. Holds a `notify::RecommendedWatcher` and
/// translates its events to [`FileChange`] over a sender. Drop the
/// watcher to stop all watches; `notify` cleans up its OS-level
/// resources via `Drop`.
pub struct FileWatcher {
    inner: RecommendedWatcher,
}

impl FileWatcher {
    /// Construct a watcher that posts [`FileChange`] events to
    /// `sender`. The `notify` callback runs on a worker thread inside
    /// `notify`; receivers must therefore be on a different thread or
    /// drained from a UI-thread wake handler (DESIGN.md §5.4).
    pub fn new(sender: Sender<FileChange>) -> notify::Result<Self> {
        let inner =
            notify::recommended_watcher(move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    if let Some(change) = translate(&event) {
                        // A closed receiver means the UI has already
                        // shut down. Discard silently — if we can't
                        // deliver, the user can't act on it anyway.
                        let _ = sender.send(change);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "file watcher error");
                }
            })?;
        Ok(Self { inner })
    }

    /// Start watching `path` non-recursively. Subsequent modifications
    /// to the file (or removal) will be reported on the sender given
    /// to [`Self::new`].
    pub fn watch(&mut self, path: &Path) -> notify::Result<()> {
        self.inner.watch(path, RecursiveMode::NonRecursive)
    }

    /// Stop watching `path`. Idempotent within `notify`'s contract.
    pub fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.inner.unwatch(path)
    }
}

/// Translate a `notify::Event` into our domain enum. Returns `None`
/// for events we don't act on (creates, access events, metadata-only
/// changes — anything that doesn't change the bytes a tab would
/// reload).
///
/// Per-backend mapping notes:
///   - **Linux inotify:** content writes arrive as
///     `Modify(Data(Any))`; chmod/touch arrives as
///     `Modify(Metadata(Permissions))` and must be filtered out
///     (otherwise a `chmod 644` produces a spurious "reload?" prompt).
///   - **Windows ReadDirectoryChangesW:** content writes arrive as
///     `Modify(Any)` because the API does not distinguish data from
///     metadata at the granularity inotify does.
///   - **macOS FSEvents:** content writes arrive as
///     `Modify(Data(Content))` typically, sometimes as `Modify(Any)`.
fn translate(event: &Event) -> Option<FileChange> {
    let path = event.paths.first()?.clone();
    match event.kind {
        // Real content modifications. `Any` is needed for Windows
        // (ReadDirectoryChangesW doesn't split data/metadata) and for
        // some FSEvents flavours; `Data(_)` covers Linux inotify and
        // most macOS cases.
        EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
            Some(FileChange::Modified(path))
        }
        // Rename out of (or move from) the watched path — from the
        // tab's perspective, the file at this path is gone.
        EventKind::Modify(ModifyKind::Name(_)) => Some(FileChange::Removed(path)),
        // Outright deletion.
        EventKind::Remove(_) => Some(FileChange::Removed(path)),
        // Metadata-only Modify (chmod, touch), Access events, Create
        // events, and Other are intentionally dropped — they don't
        // change the bytes a tab would re-read.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Drain the channel for up to `deadline_secs` looking for a
    /// `FileChange` whose path matches `expected`. Useful because
    /// notify backends emit varying numbers of events per logical
    /// change (Linux inotify can fire multiple Modify events for a
    /// single save), and we only need one to confirm the wrapper is
    /// alive.
    fn wait_for_change(
        results: &crossbeam_channel::Receiver<FileChange>,
        expected: &Path,
        deadline_secs: u64,
    ) -> Option<FileChange> {
        let deadline = Instant::now() + Duration::from_secs(deadline_secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match results.recv_timeout(remaining) {
                Ok(change) => {
                    let path = match &change {
                        FileChange::Modified(p) | FileChange::Removed(p) => p,
                    };
                    // notify can resolve the path through symlinks /
                    // canonicalize differently per platform, so match
                    // by file_name rather than full equality.
                    if path.file_name() == expected.file_name() {
                        return Some(change);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn detects_external_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watched.txt");
        std::fs::write(&path, b"v1").unwrap();

        let (tx, rx) = unbounded::<FileChange>();
        let mut watcher = FileWatcher::new(tx).unwrap();
        watcher.watch(&path).unwrap();

        // Some backends need a moment after `watch` returns before
        // events for the path actually flow. Without this small sleep
        // the modify below sometimes fires *before* the watch is fully
        // established, producing flake on slower machines.
        std::thread::sleep(Duration::from_millis(200));

        // Modify externally.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(b"v2").unwrap();
        f.sync_all().unwrap();
        drop(f);

        let change = wait_for_change(&rx, &path, 5).expect("no change event within 5s");
        // We accept either Modified or Removed — some macOS FSEvents
        // patterns surface a remove+create instead of a modify on
        // certain editor save patterns. The point is: an event arrived
        // for our path, the wrapper translates and delivers it.
        match change {
            FileChange::Modified(_) | FileChange::Removed(_) => {}
        }
    }

    #[test]
    fn unwatch_stops_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("once.txt");
        std::fs::write(&path, b"v1").unwrap();

        let (tx, rx) = unbounded::<FileChange>();
        let mut watcher = FileWatcher::new(tx).unwrap();
        watcher.watch(&path).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        watcher.unwatch(&path).unwrap();
        // Drain any in-flight events from before the unwatch.
        while rx.try_recv().is_ok() {}

        // Modify after unwatch — should not deliver any new events.
        std::fs::write(&path, b"v2").unwrap();

        // Brief grace period; if events were going to arrive, they
        // would within a second on every supported backend.
        let result = rx.recv_timeout(Duration::from_secs(1));
        assert!(
            result.is_err(),
            "received unexpected event after unwatch: {result:?}"
        );
    }

    #[test]
    fn watcher_drops_cleanly() {
        let (tx, _rx) = unbounded::<FileChange>();
        let watcher = FileWatcher::new(tx).unwrap();
        // No watches registered — drop should not panic or hang.
        drop(watcher);
    }
}
