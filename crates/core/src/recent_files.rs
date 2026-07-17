//! Rolling list of full paths for files the user has recently
//! closed.
//!
//! Powers the File → Recent Files region. The Shell pushes to
//! this list from `Shell::close_active_tab` whenever the closed
//! tab has a real on-disk path and the user's preferences enable
//! the feature. Persisted to `recent_files.xml` next to
//! `session.xml` so the list survives across launches.
//!
//! Schema (one entry per line, most-recent-first):
//!
//! ```xml
//! <?xml version="1.0" encoding="UTF-8"?>
//! <recent-files>
//!   <file>C:\Users\Max\notes.txt</file>
//!   <file>C:\Users\Max\code\main.rs</file>
//! </recent-files>
//! ```
//!
//! `MAX_ENTRIES = 30` is a defense ceiling the core enforces
//! against hand-edited XML — the on-disk file can never exceed
//! this regardless of what the caller passes to
//! [`RecentFiles::push`]. The **user-facing** cap is
//! `preferences.recent_files_history.max_entries` (0..=30);
//! the Shell truncates the retained list to that value on push,
//! on preference change, and on load so "the menu shows exactly
//! what's retained on disk." Matches N++ behaviour: lowering
//! the spinner from 20 to 5 drops entries 6..20 from disk (they
//! aren't just hidden), and raising it back to 20 does not
//! resurrect them.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Hard ceiling on how many paths [`RecentFiles`] retains on
/// disk. Matches the maximum value the Preferences → Recent
/// Files History spinner accepts (30) so a user turning the
/// spinner to its ceiling still sees the full retained history.
pub const MAX_ENTRIES: usize = 30;

/// Most-recent-first list of full paths for files the user has
/// closed. Push new entries with [`Self::push`]; the caller
/// (Shell) is responsible for saving after any successful push.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "recent-files")]
pub struct RecentFiles {
    /// Most-recent-first list of full paths.
    #[serde(rename = "file", default)]
    pub entries: Vec<PathBuf>,
}

/// Errors from reading or writing `recent_files.xml`. Mirrors
/// the shape of [`crate::find_history::FindHistoryError`] so
/// callers can use the same error-handling pattern.
#[derive(Debug)]
pub enum RecentFilesError {
    /// I/O error reading or writing the file.
    Io(std::io::Error),
    /// The file existed but didn't parse — corrupt or wrong
    /// schema. Callers should log + fall back to an empty list.
    Parse(quick_xml::DeError),
    /// Serialization failure. Effectively impossible with the
    /// simple `PathBuf` content we produce.
    Serialize(quick_xml::SeError),
}

impl std::fmt::Display for RecentFilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecentFilesError::Io(e) => write!(f, "recent_files.xml I/O: {e}"),
            RecentFilesError::Parse(e) => write!(f, "recent_files.xml parse: {e}"),
            RecentFilesError::Serialize(e) => write!(f, "recent_files.xml serialize: {e}"),
        }
    }
}

impl std::error::Error for RecentFilesError {}

impl RecentFiles {
    /// Push a new path to the front of the list. Empty paths and
    /// non-local Windows paths (UNC shares, verbatim device
    /// namespaces) are rejected — the same defense
    /// [`Self::load`] enforces at read time, applied at write
    /// time so an in-session UNC file that gets closed while
    /// tracking is on doesn't end up in the retained history
    /// even briefly. If `path` is already at the front, the
    /// list is unchanged and this returns `false` — the Shell
    /// uses that to skip the disk write on the common "close
    /// then reopen then close again" pattern where nothing
    /// needs persisting. Existing entries elsewhere in the list
    /// are moved to the front rather than duplicated. Caps at
    /// [`MAX_ENTRIES`].
    pub fn push(&mut self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return false;
        }
        if crate::npp_session::is_non_local_windows_path(path) {
            return false;
        }
        if self.entries.first().map(PathBuf::as_path) == Some(path) {
            return false;
        }
        self.entries.retain(|p| p.as_path() != path);
        self.entries.insert(0, path.to_path_buf());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.truncate(MAX_ENTRIES);
        }
        true
    }

    /// Remove the most recently added entry and return it. Used
    /// by File → Restore Recent Closed File (Ctrl+Shift+T).
    /// Returns `None` when the list is empty.
    pub fn pop_front(&mut self) -> Option<PathBuf> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.remove(0))
        }
    }

    /// Drain the list, returning every retained path in
    /// most-recent-first order. Used by File → Open All Recent
    /// Files, which reopens every tracked path and then leaves
    /// the list empty (a natural side effect: everything the
    /// user just reopened is now open again, so it doesn't
    /// belong in the "recently closed" list).
    pub fn drain_all(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.entries)
    }

    /// Drop every tracked path. Backs the File → Empty Recent
    /// Files List menu action.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Truncate the retained list to at most `cap` entries.
    /// Returns `true` if the list actually shrank — the Shell
    /// uses that to skip the disk write when the invariant
    /// already held. The user-facing `max_entries` preference
    /// is a hard cap (matches N++): lowering it must drop
    /// entries beyond it from disk, not just hide them.
    /// Called from `Shell::new` on load, from
    /// `Shell::close_active_tab` after each push, and from
    /// `Shell::set_preferences` on any preference mutation, so
    /// the "menu shows exactly what's retained" invariant holds
    /// after every entry point.
    pub fn truncate_to(&mut self, cap: usize) -> bool {
        if self.entries.len() > cap {
            self.entries.truncate(cap);
            true
        } else {
            false
        }
    }

    /// Read the XML file at `path`. Returns an empty list if the
    /// file doesn't exist (first launch); surfaces I/O / parse
    /// errors for any other failure mode. Path entries that
    /// deserialise but resolve to a non-local Windows path
    /// (UNC share `\\server\share\...`) are dropped on load —
    /// the same NTLM-hash-leak defense
    /// [`crate::npp_session::is_non_local_windows_path`] enforces
    /// on session imports. Recent-files XML is user-writable so
    /// the defense applies here too.
    ///
    /// # Errors
    ///
    /// Returns `RecentFilesError::Io` for filesystem errors other
    /// than "not found", and `RecentFilesError::Parse` if the
    /// file is present but doesn't deserialise into the
    /// `RecentFiles` schema.
    pub fn load(path: &Path) -> Result<Self, RecentFilesError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(RecentFilesError::Io(e)),
        };
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut me: Self = quick_xml::de::from_str(&raw).map_err(RecentFilesError::Parse)?;
        me.entries
            .retain(|p| !crate::npp_session::is_non_local_windows_path(p));
        if me.entries.len() > MAX_ENTRIES {
            me.entries.truncate(MAX_ENTRIES);
        }
        Ok(me)
    }

    /// Atomic write to `path` via [`tempfile::NamedTempFile`] —
    /// same shape [`crate::find_history::FindHistory::save`]
    /// uses. Guarantees power-loss safety (`sync_all` before
    /// rename), no stale tmp files (dropped `NamedTempFile`
    /// unlinks itself on error), and no TOCTOU substitution
    /// (randomised owner-only tmp path).
    ///
    /// # Errors
    ///
    /// Returns `RecentFilesError::Serialize` for the (effectively
    /// impossible) quick-xml emit failure, and
    /// `RecentFilesError::Io` for any filesystem failure during
    /// the create-dirs / tempfile / write / sync / rename
    /// pipeline.
    pub fn save(&self, path: &Path) -> Result<(), RecentFilesError> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        quick_xml::se::to_writer(&mut xml, self).map_err(RecentFilesError::Serialize)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(RecentFilesError::Io)?;
        }
        let parent_dir = parent.unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".recent-files-")
            .suffix(".xml.tmp")
            .tempfile_in(parent_dir)
            .map_err(RecentFilesError::Io)?;
        tmp.write_all(xml.as_bytes())
            .map_err(RecentFilesError::Io)?;
        tmp.as_file_mut().sync_all().map_err(RecentFilesError::Io)?;
        tmp.persist(path)
            .map_err(|e| RecentFilesError::Io(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_empty_path_is_rejected() {
        let mut r = RecentFiles::default();
        assert!(!r.push(Path::new("")));
        assert!(r.entries.is_empty());
    }

    #[test]
    fn push_new_entry_prepends() {
        let mut r = RecentFiles::default();
        assert!(r.push(Path::new("a.txt")));
        assert!(r.push(Path::new("b.txt")));
        assert_eq!(r.entries[0], PathBuf::from("b.txt"));
        assert_eq!(r.entries[1], PathBuf::from("a.txt"));
    }

    #[test]
    fn push_existing_moves_to_front() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        r.push(Path::new("c.txt"));
        assert!(r.push(Path::new("a.txt")));
        assert_eq!(
            r.entries,
            vec![PathBuf::from("a.txt"), "c.txt".into(), "b.txt".into()]
        );
    }

    #[test]
    fn push_when_already_front_is_noop_and_returns_false() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        assert!(!r.push(Path::new("a.txt")));
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn push_caps_at_max_entries() {
        let mut r = RecentFiles::default();
        for i in 0..(MAX_ENTRIES + 5) {
            r.push(Path::new(&format!("f{i}.txt")));
        }
        assert_eq!(r.entries.len(), MAX_ENTRIES);
        // Newest push wins the front slot.
        assert_eq!(
            r.entries[0],
            PathBuf::from(format!("f{}.txt", MAX_ENTRIES + 4))
        );
    }

    #[test]
    fn pop_front_returns_and_removes_top() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        assert_eq!(r.pop_front(), Some(PathBuf::from("b.txt")));
        assert_eq!(r.entries, vec![PathBuf::from("a.txt")]);
        assert_eq!(r.pop_front(), Some(PathBuf::from("a.txt")));
        assert_eq!(r.pop_front(), None);
    }

    #[test]
    fn drain_all_returns_everything_and_clears() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        let drained = r.drain_all();
        assert_eq!(drained, vec![PathBuf::from("b.txt"), "a.txt".into()]);
        assert!(r.entries.is_empty());
    }

    #[test]
    fn truncate_to_shrinks_only_when_needed() {
        let mut r = RecentFiles::default();
        for i in 0..10u32 {
            r.push(Path::new(&format!("f{i}.txt")));
        }
        assert_eq!(r.entries.len(), 10);
        // No-op path: cap equals or exceeds current length.
        assert!(!r.truncate_to(10));
        assert!(!r.truncate_to(20));
        assert_eq!(r.entries.len(), 10);
        // Shrink path: signals the caller to persist.
        assert!(r.truncate_to(5));
        assert_eq!(r.entries.len(), 5);
        // Newest entries retained (front = most recent, matches
        // N++: lowering the cap keeps the recent tail visible).
        assert_eq!(r.entries[0], PathBuf::from("f9.txt"));
        assert_eq!(r.entries[4], PathBuf::from("f5.txt"));
    }

    #[test]
    fn truncate_to_zero_empties() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        assert!(r.truncate_to(0));
        assert!(r.entries.is_empty());
    }

    #[test]
    fn clear_empties_the_list() {
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        r.clear();
        assert!(r.entries.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recent_files.xml");
        let mut r = RecentFiles::default();
        r.push(Path::new("a.txt"));
        r.push(Path::new("b.txt"));
        r.save(&path).expect("save");
        let back = RecentFiles::load(&path).expect("load");
        assert_eq!(back, r);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.xml");
        let r = RecentFiles::load(&path).expect("load");
        assert!(r.entries.is_empty());
    }

    #[test]
    fn load_empty_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.xml");
        std::fs::write(&path, "").expect("write empty");
        let r = RecentFiles::load(&path).expect("load empty");
        assert!(r.entries.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn push_rejects_unc_paths() {
        let mut r = RecentFiles::default();
        assert!(!r.push(Path::new("\\\\server\\share\\file.txt")));
        assert!(r.entries.is_empty());
        // Local paths still work.
        assert!(r.push(Path::new("C:\\notes.txt")));
        assert_eq!(r.entries.len(), 1);
    }

    #[test]
    fn load_drops_unc_paths() {
        // A hand-edited (or attacker-planted) file that references
        // a `\\server\share\...` path must not survive the load;
        // File → Recent Files would otherwise trigger an SMB
        // handshake and leak NTLM-hash material on the first
        // menu render.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("recent_files.xml");
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                   <recent-files>\
                     <file>\\\\server\\share\\evil.txt</file>\
                     <file>C:\\notes.txt</file>\
                   </recent-files>";
        std::fs::write(&path, xml).expect("write");
        let r = RecentFiles::load(&path).expect("load");
        // On Windows the UNC entry is stripped; on non-Windows
        // `is_non_local_windows_path` returns false and both
        // entries survive. Either way the local path is retained.
        assert!(r.entries.iter().any(|p| p == Path::new("C:\\notes.txt")));
        #[cfg(target_os = "windows")]
        assert!(!r
            .entries
            .iter()
            .any(|p| p == Path::new("\\\\server\\share\\evil.txt")));
    }
}
